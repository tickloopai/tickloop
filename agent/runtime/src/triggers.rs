use super::loop_lease::{LEASE_RETRY_INTERVAL, RunStateChange, TriggerPermit};
use super::*;
use tickloop_agent_protocol::local_messages;
use tickloop_agent_protocol::{
    ChatSnapshot, EventKind, LoopId, LoopSnapshot, LoopState, OperationRecord, OperationState,
    Receipt, TICK_SUBSCRIPTION_SOURCE, Trigger, TriggerSource, UserShellState,
};
use tickloop_agent_transcript::{DiscardReason, SettledOutcome};

pub(super) fn can_trigger(goal: &LoopSnapshot) -> bool {
    (goal.trigger.as_ref().is_some_and(|trigger| {
        trigger.armed
            && !trigger.awaits_confirmation()
            && trigger.expires_at_ms.is_none_or(|deadline| {
                deadline > tickloop_agent_base::private_fs::unix_time_milliseconds()
            })
    }) || goal
        .event_sources
        .iter()
        .any(|source| !source.resume_required))
        && matches!(
            goal.state,
            LoopState::Running
                | LoopState::Queued
                | LoopState::WaitingTrigger
                | LoopState::Completed
        )
}

/// The longest a Trigger's timer sleeps before it looks at the clock again, so that a computer
/// waking from sleep fires what came due meanwhile without waiting out the rest of a long sleep.
const SCHEDULE_RECHECK: std::time::Duration = std::time::Duration::from_secs(60);
/// The maximum drift from wall time before the schedule clock is rebased.
const SCHEDULE_CLOCK_CATCH_UP_MS: u64 = 60_000;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
/// The source a Trigger's timer fires as.
const TIMER_SOURCE: &str = "timer";
/// The most schedules a Chat keeps active at once, confirmed or awaiting confirmation.
pub(crate) const MAX_ACTIVE_SCHEDULES: usize = 20;
/// How long a schedule the model created runs before it expires, unless the user changes it.
pub(crate) const MODEL_SCHEDULE_LIFETIME_MS: u64 = MODEL_SCHEDULE_LIFETIME_DAYS * MS_PER_DAY;
pub(crate) const MODEL_SCHEDULE_LIFETIME_DAYS: u64 = 30;
const MS_PER_DAY: u64 = SECONDS_PER_DAY * 1_000;

/// How many of the Chat's Loops that have not ended keep a schedule armed or awaiting the user's
/// confirmation.
/// A schedule disarmed by a restart until the user continues its Loop counts as well.
pub(crate) fn active_schedules(snapshot: &ChatSnapshot) -> usize {
    snapshot
        .loops
        .iter()
        .filter(|goal| has_active_schedule(snapshot, goal))
        .count()
}

fn has_active_schedule(snapshot: &ChatSnapshot, goal: &LoopSnapshot) -> bool {
    !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
        && goal.trigger.as_ref().is_some_and(|trigger| {
            trigger.schedule.is_some()
                && (trigger.armed
                    || trigger.awaits_confirmation()
                    || snapshot.resume_triggers.contains(&goal.id))
        })
}

pub(crate) fn too_many_schedules() -> String {
    format!(
        "this Chat already has {MAX_ACTIVE_SCHEDULES} active schedules; stop one before adding \
         another"
    )
}

/// The user's answer to a schedule the model created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleDecision {
    /// Arms the schedule: it runs from now on.
    Confirm,
    /// Stops the schedule's Loop before it ever runs.
    Reject,
}

/// The wall clock Trigger schedules run on: the system's time when the Agent was built, carried
/// on by the runtime's monotonic clock, so that a test's paused clock moves it too. It catches
/// up after sleep and corrects an anchor ahead of wall time. Scheduled-time ordering is kept
/// separately by `Trigger::note_scheduled`, even when wall time moves backwards.
pub(crate) struct ScheduleClock {
    anchor: std::sync::Mutex<(u64, tokio::time::Instant)>,
}

impl ScheduleClock {
    pub(crate) fn new() -> Self {
        Self {
            anchor: std::sync::Mutex::new((
                tickloop_agent_base::private_fs::unix_time_milliseconds(),
                tokio::time::Instant::now(),
            )),
        }
    }

    /// Unix milliseconds now.
    pub(crate) fn now_ms(&self) -> u64 {
        let mut anchor = self.anchor.lock().unwrap();
        let (anchor_ms, anchor_instant) = *anchor;
        let elapsed = tokio::time::Instant::now().saturating_duration_since(anchor_instant);
        let carried =
            anchor_ms.saturating_add(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        let system = tickloop_agent_base::private_fs::unix_time_milliseconds();
        if system.abs_diff(carried) > SCHEDULE_CLOCK_CATCH_UP_MS {
            *anchor = (system, tokio::time::Instant::now());
            return system;
        }
        carried
    }

    /// When the timer that waits for `milliseconds` looks at the clock next.
    fn next_check(&self, milliseconds: u64) -> tokio::time::Instant {
        let remaining =
            std::time::Duration::from_millis(milliseconds.saturating_sub(self.now_ms()));
        tokio::time::Instant::now() + remaining.min(SCHEDULE_RECHECK)
    }
}

/// When a quiet Chat next needs its host, in Unix milliseconds: the earliest scheduled time of a
/// Trigger whose Loop can take a fire, or deadline of a Loop that has not ended, whatever its
/// state. It may be past already.
pub(crate) fn wake_at(snapshot: &ChatSnapshot) -> Option<u64> {
    snapshot
        .loops
        .iter()
        .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
        .filter_map(|goal| {
            let trigger = goal.trigger.as_ref()?;
            let scheduled = trigger
                .next_scheduled_ms()
                .filter(|_| trigger.armed && can_trigger(goal));
            scheduled.into_iter().chain(trigger.expires_at_ms).min()
        })
        .min()
}

/// Loop 进入一个新进程（重启恢复、`--fork-session` 或 Branch 复制）时的恢复规则：运行或等待中的 Loop 转为
/// 已中断；过期的 Trigger 标为已过期；武装中的 Trigger 解除武装，记入手动恢复时重新武装。
pub(super) fn recover_loops(snapshot: &mut ChatSnapshot) {
    recover_loops_keeping(snapshot, &HashSet::new(), WaitHosting::Process);
}

/// [`recover_loops`], except that the Loops in `kept` keep their state: their Runs are rebuilt.
/// A durable host keeps every armed Trigger armed and its Loop waiting for it: the schedule
/// goes on from the times the Loop already dealt with. It keeps the Loop's artifact links
/// receiving too, since the host delivers their requests rather than a watcher of this process.
pub(super) fn recover_loops_keeping(
    snapshot: &mut ChatSnapshot,
    kept: &HashSet<LoopId>,
    waits: WaitHosting,
) {
    recover_triggers(snapshot, waits);
    for goal in snapshot
        .loops
        .iter_mut()
        .filter(|goal| !kept.contains(&goal.id))
    {
        let keeps_its_trigger = waits == WaitHosting::Durable
            && goal.trigger.as_ref().is_some_and(|trigger| trigger.armed);
        if !goal.event_sources.is_empty() && waits == WaitHosting::Process {
            for source in &mut goal.event_sources {
                source.resume_required = true;
            }
            if goal.state == LoopState::WaitingTrigger && !keeps_its_trigger {
                goal.recover_state(LoopState::Interrupted);
            }
        }
        if matches!(
            goal.state,
            LoopState::Running
                | LoopState::Queued
                | LoopState::WaitingPermission
                | LoopState::WaitingQuestion
        ) {
            goal.recover_state(LoopState::Interrupted);
        }
    }
}

fn recover_triggers(snapshot: &mut ChatSnapshot, waits: WaitHosting) {
    for goal in &mut snapshot.loops {
        let Some(trigger) = &goal.trigger else {
            continue;
        };
        let armed = trigger.armed;
        let awaits_confirmation = trigger.awaits_confirmation();
        if !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
            && trigger.expires_at_ms.is_some_and(|deadline| {
                deadline <= tickloop_agent_base::private_fs::unix_time_milliseconds()
            })
        {
            goal.recover_state(LoopState::Expired);
        }
        let has_ended = matches!(goal.state, LoopState::Stopped | LoopState::Expired);
        // A schedule awaiting the user's confirmation never ran; it waits on, wherever it loads.
        if !has_ended && awaits_confirmation {
            continue;
        }
        if has_ended {
            snapshot.resume_triggers.retain(|id| *id != goal.id);
        } else if waits == WaitHosting::Durable && armed {
            continue;
        } else if armed && !snapshot.resume_triggers.contains(&goal.id) {
            snapshot.resume_triggers.push(goal.id);
        }
        if let Some(trigger) = &mut goal.trigger {
            trigger.armed = false;
        }
        if goal.state == LoopState::WaitingTrigger {
            goal.recover_state(LoopState::Interrupted);
        }
    }
    snapshot.pending_inputs.retain(|pending| {
        pending.trigger_source.is_none()
            && snapshot.loops.iter().any(|goal| {
                goal.id == pending.receipt.loop_id
                    && !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
            })
    });
}

/// Why a Trigger armed at `now_ms` is refused, or `None` when it may be configured under the
/// host's floor for schedules.
pub(crate) fn invalid_trigger(
    trigger: &Trigger,
    floor: std::time::Duration,
    now_ms: u64,
) -> Option<String> {
    if trigger.prompt.trim().is_empty()
        || trigger.expires_at_ms.is_some_and(|deadline| {
            deadline <= tickloop_agent_base::private_fs::unix_time_milliseconds()
        })
        || (trigger.schedule.is_none() && !trigger.accepts_events)
    {
        return Some(
            "trigger requires a prompt and valid future schedule or explicit events".into(),
        );
    }
    let schedule = trigger.schedule?;
    if schedule.shortest_gap().is_some_and(|gap| gap < floor) {
        return Some(format!(
            "schedule `{schedule}` runs more often than this host allows: runs must be at least \
             {} apart",
            describe_interval(floor)
        ));
    }
    let mut armed = trigger.clone();
    armed.arm_at(now_ms);
    if armed.next_scheduled_ms().is_none() {
        return Some(format!("schedule `{schedule}` has no future run"));
    }
    None
}

/// An interval as a person reads it, such as `1 minute` or `90 seconds`.
pub(crate) fn describe_interval(interval: std::time::Duration) -> String {
    const UNITS: [(u64, &str); 3] = [
        (SECONDS_PER_DAY, "day"),
        (SECONDS_PER_HOUR, "hour"),
        (SECONDS_PER_MINUTE, "minute"),
    ];
    let seconds = interval.as_secs();
    let (count, unit) = UNITS
        .iter()
        .find(|(unit_seconds, _)| seconds > 0 && seconds.is_multiple_of(*unit_seconds))
        .map_or((seconds, "second"), |(unit_seconds, unit)| {
            (seconds / unit_seconds, *unit)
        });
    match count {
        1 => format!("1 {unit}"),
        _ => format!("{count} {unit}s"),
    }
}

impl Agent {
    pub async fn configure_trigger(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        trigger: Option<Trigger>,
    ) -> Result<(), AgentError> {
        if let Some(trigger) = &trigger {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if chat.snapshot.delegation.as_ref().is_some_and(|delegation| {
                trigger.subscriptions.iter().any(|subscription| {
                    !delegation
                        .ceiling
                        .tick_instances
                        .contains(&subscription.instance)
                })
            }) {
                return Err(AgentError::Invalid("Unsupported: Tick Subscription is outside the inherited delegated instance scope".into()));
            }
        }
        if let Some(trigger) = &trigger {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if let Some(delegation) = &chat.snapshot.delegation
                && trigger
                    .subscriptions
                    .iter()
                    .filter_map(|subscription| subscription.authority.as_ref())
                    .any(|authority| {
                        !delegation
                            .ceiling
                            .host_tools
                            .values()
                            .any(|allowed| allowed.contains(authority))
                    })
            {
                return Err(AgentError::Invalid(
                    "Tick watch authority is outside the frozen delegated account/endpoint scope"
                        .into(),
                ));
            }
        }
        let now_ms = self.inner.schedule_clock.now_ms();
        if let Some(reason) = trigger
            .as_ref()
            .and_then(|trigger| invalid_trigger(trigger, self.inner.min_schedule_interval, now_ms))
        {
            return Err(AgentError::Invalid(reason));
        }
        let mut trigger = trigger;
        if let Some(trigger) = trigger.as_mut().filter(|trigger| trigger.armed) {
            trigger.arm_at(now_ms);
        }
        if let Some(trigger) = &trigger {
            tick_subscriptions::validate(trigger).map_err(AgentError::Invalid)?;
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let mut snapshot = chat.snapshot.clone();
        let adds_a_schedule = trigger
            .as_ref()
            .is_some_and(|trigger| trigger.schedule.is_some() && trigger.armed);
        let other_schedules = snapshot
            .loops
            .iter()
            .filter(|goal| goal.id != loop_id && has_active_schedule(&snapshot, goal))
            .count();
        let goal = snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == loop_id)
            .ok_or(AgentError::NotFound)?;
        if matches!(goal.state, LoopState::Stopped | LoopState::Expired) {
            return Err(AgentError::Invalid(
                "ended goals cannot be rescheduled".into(),
            ));
        }
        let previous_confirmation = goal
            .trigger
            .as_ref()
            .and_then(|trigger| trigger.confirmation);
        if previous_confirmation == Some(tickloop_agent_protocol::ScheduleConfirmation::Pending) {
            return Err(AgentError::Invalid(
                "the goal's schedule awaits the user's confirmation; confirm or reject it first"
                    .into(),
            ));
        }
        if adds_a_schedule && other_schedules >= MAX_ACTIVE_SCHEDULES {
            return Err(AgentError::Invalid(too_many_schedules()));
        }
        // Only the user's own decision confirms a schedule: a configuration carries the one the
        // Loop's schedule has.
        if let Some(trigger) = trigger.as_mut() {
            trigger.confirmation = previous_confirmation;
        }
        goal.trigger = trigger.clone();
        if goal.state == LoopState::Completed
            && trigger.as_ref().is_some_and(|trigger| trigger.armed)
        {
            goal.set_state(LoopState::WaitingTrigger);
        } else if goal.state == LoopState::WaitingTrigger
            && trigger.as_ref().is_none_or(|trigger| !trigger.armed)
            && !goal
                .event_sources
                .iter()
                .any(|source| !source.resume_required)
        {
            goal.set_state(LoopState::Completed);
        }
        snapshot.resume_triggers.retain(|id| *id != loop_id);
        chat.snapshot = snapshot;
        self.discard_replaced_trigger_inputs(chat, loop_id);
        self.save(&chat.snapshot);
        self.inner
            .trigger_tried_through
            .lock()
            .unwrap()
            .remove(&(chat_id, loop_id));
        if let Some(previous) = chat.trigger_cancellations.remove(&loop_id) {
            previous.cancel();
        }
        let watcher = trigger.filter(|trigger| trigger.armed).map(|trigger| {
            let cancel = CancellationToken::new();
            chat.trigger_cancellations.insert(loop_id, cancel.clone());
            (trigger, cancel)
        });
        drop(chats);
        self.sync_tick_subscriptions().await;
        let Some((trigger, cancel)) = watcher else {
            let recorded = self
                .record_loop_run_state(chat_id, loop_id, RunStateChange::Disarmed)
                .await;
            self.inner.loop_leases.release(chat_id, loop_id);
            self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
            return Ok(());
        };
        self.start_armed_trigger(chat_id, loop_id, trigger, cancel)
            .await;
        Ok(())
    }

    /// Settles a schedule the model created and the user has not confirmed yet: `Confirm` arms
    /// it from now on, `Reject` stops its Loop. The decision is recorded with the Loop, and
    /// repeating it changes nothing; a schedule whose deadline passed expires instead.
    pub async fn decide_schedule(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        decision: ScheduleDecision,
    ) -> Result<(), AgentError> {
        use tickloop_agent_protocol::ScheduleConfirmation;
        let _admission = self.inner.tasks.token();
        self.refuse_when_closed()?;
        self.make_resident(chat_id).await?;
        let now = tickloop_agent_base::private_fs::unix_time_milliseconds();
        let armed = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let goal = chat
                .snapshot
                .loops
                .iter_mut()
                .find(|goal| goal.id == loop_id)
                .ok_or(AgentError::NotFound)?;
            let has_ended = matches!(goal.state, LoopState::Stopped | LoopState::Expired);
            let trigger = goal
                .trigger
                .as_mut()
                .filter(|trigger| trigger.confirmation.is_some())
                .ok_or_else(|| {
                    AgentError::Invalid("the goal has no schedule the Agent created".into())
                })?;
            match (trigger.confirmation, decision) {
                (Some(ScheduleConfirmation::Confirmed { .. }), ScheduleDecision::Confirm) => {
                    return Ok(());
                }
                // A rejection whose Loop has not ended yet stops it now.
                (Some(ScheduleConfirmation::Rejected { .. }), ScheduleDecision::Reject)
                    if has_ended =>
                {
                    return Ok(());
                }
                (Some(ScheduleConfirmation::Rejected { .. }), ScheduleDecision::Confirm) => {
                    return Err(AgentError::Invalid("the schedule was rejected".into()));
                }
                (Some(ScheduleConfirmation::Confirmed { .. }), ScheduleDecision::Reject) => {
                    return Err(AgentError::Invalid(
                        "the schedule was confirmed; stop its goal instead".into(),
                    ));
                }
                _ => {}
            }
            if has_ended {
                return Err(AgentError::Invalid("the schedule's goal has ended".into()));
            }
            if trigger
                .expires_at_ms
                .is_some_and(|deadline| deadline <= now)
            {
                drop(chats);
                let _ = self
                    .end_goal(chat_id, loop_id, LoopState::Expired, None)
                    .await;
                return Err(AgentError::Invalid(
                    "the schedule expired before it was confirmed".into(),
                ));
            }
            let now_on_schedule = self.inner.schedule_clock.now_ms();
            if decision == ScheduleDecision::Confirm
                && let Some(reason) =
                    invalid_trigger(trigger, self.inner.min_schedule_interval, now_on_schedule)
            {
                return Err(AgentError::Invalid(reason));
            }
            match decision {
                ScheduleDecision::Confirm => {
                    trigger.confirmation = Some(ScheduleConfirmation::Confirmed { at_ms: now });
                    trigger.arm_at(now_on_schedule);
                    let trigger = trigger.clone();
                    self.save(&chat.snapshot);
                    let cancel = CancellationToken::new();
                    if let Some(previous) =
                        chat.trigger_cancellations.insert(loop_id, cancel.clone())
                    {
                        previous.cancel();
                    }
                    Some((trigger, cancel))
                }
                // Stopping the Loop records the rejection with its end.
                ScheduleDecision::Reject => {
                    trigger.confirmation = Some(ScheduleConfirmation::Rejected { at_ms: now });
                    None
                }
            }
        };
        match armed {
            Some((trigger, cancel)) => {
                self.start_armed_trigger(chat_id, loop_id, trigger, cancel)
                    .await;
            }
            None => {
                self.end_goal(chat_id, loop_id, LoopState::Stopped, None)
                    .await?;
            }
        }
        Ok(())
    }

    /// Moves the deadline of a goal's schedule to `expires_at`, or clears it with `None`, as the
    /// user extends or keeps a schedule; the schedule goes on from the times it already dealt
    /// with.
    pub async fn set_schedule_expiry(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        expires_at: Option<std::time::SystemTime>,
    ) -> Result<(), AgentError> {
        let _admission = self.inner.tasks.token();
        self.refuse_when_closed()?;
        self.make_resident(chat_id).await?;
        let expires_at_ms = expires_at
            .map(|expires_at| {
                expires_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|since| u64::try_from(since.as_millis()).ok())
                    .filter(|deadline| {
                        *deadline > tickloop_agent_base::private_fs::unix_time_milliseconds()
                    })
                    .ok_or_else(|| AgentError::Invalid("the new deadline has passed".into()))
            })
            .transpose()?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let goal = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == loop_id)
            .ok_or(AgentError::NotFound)?;
        if matches!(goal.state, LoopState::Stopped | LoopState::Expired) {
            return Err(AgentError::Invalid("the goal has ended".into()));
        }
        let trigger = goal
            .trigger
            .as_mut()
            .filter(|trigger| trigger.schedule.is_some())
            .ok_or_else(|| AgentError::Invalid("the goal has no schedule".into()))?;
        trigger.expires_at_ms = expires_at_ms;
        let timer = (trigger.armed || trigger.awaits_confirmation()).then(|| trigger.clone());
        self.save(&chat.snapshot);
        if let Some(previous) = chat.trigger_cancellations.remove(&loop_id) {
            previous.cancel();
        }
        let Some(trigger) = timer else {
            return Ok(());
        };
        let cancel = CancellationToken::new();
        chat.trigger_cancellations.insert(loop_id, cancel.clone());
        drop(chats);
        self.trigger_task(chat_id, loop_id, trigger, cancel);
        Ok(())
    }

    fn discard_replaced_trigger_inputs(&self, chat: &mut Chat, loop_id: LoopId) {
        let mut discarded = Vec::new();
        chat.snapshot.pending_inputs.retain(|pending| {
            let keep = pending.receipt.loop_id != loop_id
                || pending.trigger_source.as_ref().is_none_or(|source| {
                    matches!(
                        source.source.as_str(),
                        super::event_sources::ARTIFACT_FEEDBACK_SOURCE
                            | TICK_SUBSCRIPTION_SOURCE
                            | TICK_MANUAL_SOURCE
                    )
                });
            if !keep {
                discarded.push(pending.receipt.clone());
            }
            keep
        });
        for receipt in discarded {
            self.finish_discarded_trigger_input(chat, &receipt);
        }
        self.discard_invalid_tick_inputs(chat);
    }

    /// 停止状态显示另一个进程解除了该 Trigger 的武装：本地同样解除武装、取消 Trigger 任务并
    /// 归还租约，不再写停止状态。
    pub(super) async fn disarm_trigger_disarmed_elsewhere(&self, chat_id: ChatId, loop_id: LoopId) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        let mut snapshot = chat.snapshot.clone();
        if let Some(goal) = snapshot.loops.iter_mut().find(|goal| goal.id == loop_id) {
            if let Some(trigger) = &mut goal.trigger {
                trigger.armed = false;
            }
            if goal.state == LoopState::WaitingTrigger
                && !goal
                    .event_sources
                    .iter()
                    .any(|source| !source.resume_required)
            {
                goal.set_state(LoopState::Completed);
            }
        }
        snapshot.resume_triggers.retain(|id| *id != loop_id);
        chat.snapshot = snapshot;
        self.discard_replaced_trigger_inputs(chat, loop_id);
        self.save(&chat.snapshot);
        if let Some(cancel) = chat.trigger_cancellations.remove(&loop_id) {
            cancel.cancel();
        }
        drop(chats);
        self.inner.loop_leases.release(chat_id, loop_id);
        self.sync_tick_subscriptions().await;
    }

    /// Starts the Trigger just armed: the run state records the arming first, so the timer's
    /// first look at it already sees this arming.
    pub(super) async fn start_armed_trigger(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        trigger: Trigger,
        cancel: CancellationToken,
    ) {
        let recorded = self
            .record_loop_run_state(chat_id, loop_id, RunStateChange::Armed)
            .await;
        self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
        self.trigger_task(chat_id, loop_id, trigger, cancel);
    }

    /// Runs an armed Trigger in this instance: its timer fires each scheduled time that comes
    /// due, and its deadline ends the Loop. A time that comes due while another process holds
    /// the Loop, or while the Loop cannot take it, passes without a fire.
    pub(super) fn trigger_task(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        trigger: Trigger,
        cancel: CancellationToken,
    ) {
        let agent = self.clone();
        let deadline = trigger.expires_at_ms.map(|deadline| {
            tokio::time::Instant::now()
                + std::time::Duration::from_millis(
                    deadline
                        .saturating_sub(tickloop_agent_base::private_fs::unix_time_milliseconds()),
                )
        });
        self.inner.tasks.spawn(async move {
            // 拿不到租约时下一次重试获取的时间；期间到点的触发直接跳过。
            let mut lease_retry_at: Option<tokio::time::Instant> = None;
            // The scheduled times this timer has tried so far, fired or passed. A Worker upgrade
            // hands them to its successor.
            let mut tried_through: Option<u64> = agent
                .inner
                .trigger_tried_through
                .lock()
                .unwrap()
                .get(&(chat_id, loop_id))
                .copied();
            let expires = async {
                if let Some(deadline) = deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(expires);
            loop {
                let next = match trigger.schedule {
                    Some(_) => agent.next_scheduled_time(chat_id, loop_id, trigger.id, tried_through).await,
                    None => None,
                };
                tokio::select! {
                    biased;
                    _ = agent.inner.shutdown.cancelled() => break,
                    _ = cancel.cancelled() => break,
                    _ = &mut expires => {
                        let _upgrade = agent.inner.upgrade_gate.read().await;
                        let _ = agent.end_goal(chat_id, loop_id, LoopState::Expired, Some(&cancel)).await;
                        break;
                    },
                    _ = async {
                        match lease_retry_at {
                            Some(retry_at) => tokio::time::sleep_until(retry_at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        lease_retry_at = match agent.acquire_trigger_permit(chat_id, loop_id).await {
                            TriggerPermit::Fire => None,
                            TriggerPermit::Withdrawn => break,
                            TriggerPermit::Wait(_) | TriggerPermit::Failed(_) => {
                                Some(tokio::time::Instant::now() + LEASE_RETRY_INTERVAL)
                            }
                        };
                    },
                    _ = async {
                        match next {
                            Some(next) => tokio::time::sleep_until(agent.inner.schedule_clock.next_check(next)).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        let _upgrade = agent.inner.upgrade_gate.read().await;
                        let now = agent.inner.schedule_clock.now_ms();
                        if next.is_none_or(|next| now < next) {
                            continue;
                        }
                        tried_through = Some(now);
                        agent.inner.trigger_tried_through.lock().unwrap().insert((chat_id, loop_id), now);
                        if lease_retry_at.is_some() {
                            continue;
                        }
                        match agent.acquire_trigger_permit(chat_id, loop_id).await {
                            TriggerPermit::Fire => {}
                            TriggerPermit::Withdrawn => break,
                            TriggerPermit::Wait(_) | TriggerPermit::Failed(_) => {
                                lease_retry_at = Some(tokio::time::Instant::now() + LEASE_RETRY_INTERVAL);
                                continue;
                            }
                        }
                        if let Err(error) = agent.fire_scheduled(chat_id, loop_id, now, Some(cancel.clone())).await {
                            agent.inner.loop_leases.set_notice(chat_id, loop_id,
                                tickloop_agent_protocol::LoopTriggerNoticeKind::Warning,
                                format!("Scheduled fire: {error}"));
                        }
                    }
                }
            }
        });
    }

    /// The next scheduled time of the armed Trigger `trigger_id` after the ones its Loop dealt
    /// with and after `tried_through`; `None` when the Trigger was replaced or has no further
    /// time.
    async fn next_scheduled_time(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        trigger_id: Uuid,
        tried_through: Option<u64>,
    ) -> Option<u64> {
        let chats = self.inner.chats.lock().await;
        let trigger = chats
            .get(&chat_id)?
            .snapshot
            .loops
            .iter()
            .find(|goal| goal.id == loop_id)?
            .trigger
            .as_ref()
            .filter(|trigger| trigger.armed && trigger.id == trigger_id)?;
        match tried_through {
            Some(after) => trigger.next_scheduled_after_ms(after),
            None => trigger.next_scheduled_ms(),
        }
    }

    /// Fires the Loop's armed Trigger for the latest scheduled time due by `until_ms` that the
    /// Loop has not dealt with; the ones missed before it never fire. The fire is named by its
    /// scheduled time, so a fire another instance made already is the same input.
    pub(super) async fn fire_scheduled(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        until_ms: u64,
        cancel: Option<CancellationToken>,
    ) -> Result<(), AgentError> {
        let (prompt, trigger_id, scheduled, cancel) = {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return Ok(());
            };
            let Some(goal) = chat
                .snapshot
                .loops
                .iter_mut()
                .find(|goal| goal.id == loop_id)
            else {
                return Ok(());
            };
            let is_eligible = can_trigger(goal);
            let Some(trigger) = goal.trigger.as_mut().filter(|trigger| trigger.armed) else {
                return Ok(());
            };
            let Some(scheduled) = trigger.due_scheduled_ms(until_ms) else {
                return Ok(());
            };
            if !is_eligible {
                if self.inner.waits == WaitHosting::Durable {
                    trigger.note_scheduled(scheduled);
                    self.save(&chat.snapshot);
                }
                return Ok(());
            }
            let cancel = cancel.or_else(|| chat.trigger_cancellations.get(&loop_id).cloned());
            (trigger.prompt.clone(), trigger.id, scheduled, cancel)
        };
        let source = TriggerSource {
            source: TIMER_SOURCE.into(),
            payload: serde_json::json!({"trigger_id":trigger_id}),
        };
        let result = self
            .submit_internal(
                chat_id,
                Some(loop_id),
                prompt,
                SubmissionOptions {
                    trigger_source: Some(source),
                    trigger_cancel: cancel.clone(),
                    scheduled_for: Some(scheduled),
                    ..Default::default()
                },
            )
            .await;
        if result.is_err()
            && self.inner.waits == WaitHosting::Durable
            && !cancel.as_ref().is_some_and(CancellationToken::is_cancelled)
        {
            let mut chats = self.inner.chats.lock().await;
            if let Some(chat) = chats.get_mut(&chat_id)
                && let Some(trigger) = chat
                    .snapshot
                    .loops
                    .iter_mut()
                    .find(|goal| goal.id == loop_id)
                    .and_then(|goal| goal.trigger.as_mut())
                    .filter(|trigger| trigger.id == trigger_id && trigger.armed)
                && trigger.last_fired_at_ms.is_none_or(|last| last < scheduled)
            {
                trigger.note_scheduled(scheduled);
                self.save(&chat.snapshot);
            }
        }
        result.map(|_| ())
    }

    /// With durable wait hosting, the loaded Chat's armed Triggers go on in this instance: each
    /// gets its timer and deadline again, and what came due while no host held the Chat fires
    /// once, its earlier missed times skipped. A schedule awaiting confirmation gets its deadline
    /// alone.
    pub(crate) async fn resume_triggers_after_load(&self, chat_id: ChatId) {
        let armed = {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return;
            };
            let armed = chat
                .snapshot
                .loops
                .iter()
                .filter(|goal| can_trigger(goal))
                .filter_map(|goal| {
                    Some((
                        goal.id,
                        goal.trigger
                            .clone()
                            .filter(|trigger| trigger.armed || trigger.awaits_confirmation())?,
                    ))
                })
                .collect::<Vec<_>>();
            armed
                .into_iter()
                .map(|(loop_id, trigger)| {
                    let cancel = CancellationToken::new();
                    if let Some(previous) =
                        chat.trigger_cancellations.insert(loop_id, cancel.clone())
                    {
                        previous.cancel();
                    }
                    (loop_id, trigger, cancel)
                })
                .collect::<Vec<_>>()
        };
        let now = self.inner.schedule_clock.now_ms();
        for (loop_id, trigger, cancel) in armed {
            if let Err(error) = self
                .fire_scheduled(chat_id, loop_id, now, Some(cancel.clone()))
                .await
            {
                self.inner.loop_leases.set_notice(
                    chat_id,
                    loop_id,
                    tickloop_agent_protocol::LoopTriggerNoticeKind::Warning,
                    format!("Scheduled fire: {error}"),
                );
            }
            self.trigger_task(chat_id, loop_id, trigger, cancel);
        }
    }

    /// What is due by `until`, in Unix milliseconds, happens across the family of `chat_id`:
    /// each Trigger fires for its latest time due by then, and each Loop whose deadline came
    /// ends.
    pub(crate) async fn fire_due_triggers(
        &self,
        chat_id: ChatId,
        until: u64,
    ) -> Result<(), AgentError> {
        let due = {
            let chats = self.inner.chats.lock().await;
            let members = crate::family_tree::root(&chats, chat_id)
                .map(|root| {
                    std::iter::once(root)
                        .chain(crate::family_tree::descendants(&chats, root).unwrap_or_default())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|_| vec![chat_id]);
            members
                .into_iter()
                .filter_map(|member| Some((member, chats.get(&member)?)))
                .flat_map(|(member, chat)| {
                    chat.snapshot
                        .loops
                        .iter()
                        .filter(|goal| {
                            !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
                        })
                        .filter_map(move |goal| {
                            let trigger = goal.trigger.as_ref()?;
                            let expired = trigger
                                .expires_at_ms
                                .is_some_and(|deadline| deadline <= until);
                            Some((member, goal.id, expired))
                        })
                })
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for (member, loop_id, expired) in due {
            if expired {
                let _ = self
                    .end_goal(member, loop_id, LoopState::Expired, None)
                    .await;
            } else if let Err(error) = self.fire_scheduled(member, loop_id, until, None).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(super) async fn end_goal(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        state: LoopState,
        expected: Option<&CancellationToken>,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        self.unlink_feedback_receivers(chat_id, Some(loop_id), None)
            .await?;
        let reason = if state == LoopState::Expired {
            "deadline reached"
        } else {
            "stopped by user"
        };
        self.finish_goal(chat_id, loop_id, state, expected, reason, true)
            .await
    }

    /// 停止状态显示 Loop 已在另一个进程结束：本地同样停止，不再写停止状态。
    pub(super) async fn end_goal_ended_elsewhere(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        self.finish_goal(
            chat_id,
            loop_id,
            LoopState::Stopped,
            None,
            "stopped in another Tickloop process",
            false,
        )
        .await
    }

    async fn finish_goal(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        state: LoopState,
        expected: Option<&CancellationToken>,
        reason: &str,
        records_run_state: bool,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let original = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if expected.is_some_and(CancellationToken::is_cancelled) {
            return Err(AgentError::Invalid(
                "trigger configuration was replaced".into(),
            ));
        }
        let mut chat = original.clone();
        let goal = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == loop_id)
            .ok_or(AgentError::NotFound)?;
        let operations: Vec<_> = chat
            .snapshot
            .operations
            .iter()
            .filter(|operation| {
                operation.loop_id == loop_id
                    && matches!(
                        operation.state,
                        OperationState::Admitted | OperationState::Unknown
                    )
            })
            .cloned()
            .collect();
        if matches!(goal.state, LoopState::Stopped | LoopState::Expired) {
            return Ok(operations);
        }
        let receipt = Receipt {
            chat_id,
            loop_id,
            run_id: goal.run_id.unwrap_or_else(Uuid::new_v4),
        };
        let is_active = chat.active_run == Some(receipt.run_id);
        goal.set_state(state.clone());
        for source in &mut goal.event_sources {
            source.resume_required = true;
        }
        if let Some(trigger) = &mut goal.trigger {
            trigger.armed = false;
        }
        let mut cancelled_shells = HashSet::new();
        for shell in &mut chat.snapshot.user_shells {
            if shell.loop_id == Some(loop_id)
                && (shell.state == UserShellState::Queued || shell.awaits_admission())
            {
                cancelled_shells.insert(shell.id);
                shell.armed = false;
                shell.set_state(UserShellState::Failed);
                shell.reason = Some("the owning Loop ended before Shell admission".into());
                if let Some(cancel) = self.take_user_shell_cancellation(chat_id, shell.id) {
                    cancel.cancel();
                }
                let _ = self.inner.user_shell_events.send(shell.clone());
            }
        }
        self.withdraw_user_shell_requests(&mut chat.snapshot, |command_id| {
            cancelled_shells.contains(&command_id)
        })
        .await;
        let (discarded, kept) = std::mem::take(&mut chat.snapshot.pending_inputs)
            .into_iter()
            .partition::<Vec<_>, _>(|pending| pending.receipt.loop_id == loop_id);
        chat.snapshot.pending_inputs = kept;
        self.settle_removed_inputs(
            &mut chat,
            &discarded,
            SettledOutcome::Discarded {
                reason: if state == LoopState::Stopped {
                    DiscardReason::Stopped
                } else {
                    DiscardReason::LoopEnded
                },
            },
        );
        chat.snapshot
            .permissions
            .retain(|permission| permission.loop_id != loop_id);
        chat.snapshot
            .questions
            .retain(|question| question.loop_id != loop_id);
        chat.snapshot.resume_triggers.retain(|id| *id != loop_id);
        self.save(&chat.snapshot);
        let cancelled_runs = chat
            .parked
            .values()
            .filter(|run| run.receipt.loop_id == loop_id)
            .map(|waiting| {
                waiting.cancel.cancel();
                waiting.receipt.run_id
            })
            .chain(is_active.then_some(receipt.run_id))
            .collect::<Vec<_>>();
        if is_active && let Some(cancel) = &chat.active {
            cancel.cancel();
        }
        if state == LoopState::Stopped {
            chat.user_cancelled_runs.extend(cancelled_runs);
        }
        if let Some(cancel) = chat.trigger_cancellations.remove(&loop_id) {
            cancel.cancel();
        }
        if let Some(cancel) = chat.event_cancellations.remove(&loop_id) {
            cancel.cancel();
        }
        let tools = chat.tools.clone();
        self.emit(
            &mut chat,
            &receipt,
            EventKind::GoalEnded {
                reason: reason.to_owned(),
                operations: operations.clone(),
            },
        );
        *original = chat;
        drop(chats);
        // 先写结束状态再归还租约：接手的进程读取停止状态时已看到结束。
        let recorded = if records_run_state {
            self.record_loop_run_state(chat_id, loop_id, RunStateChange::Ended)
                .await
        } else {
            Ok(())
        };
        self.inner.loop_leases.release(chat_id, loop_id);
        self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
        self.sync_tick_subscriptions().await;
        for command in tools.stop_loop(loop_id).await {
            self.record_command(chat_id, command).await?;
        }
        // Child completions held for this Loop while it had failed or was interrupted are refused
        // now that it has ended.
        self.flush_subagent_notifications(chat_id).await;
        Ok(operations)
    }

    pub async fn trigger(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        source: TriggerSource,
    ) -> Result<Receipt, AgentError> {
        if source.source.trim().is_empty() {
            return Err(AgentError::Invalid("event source is required".into()));
        }
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        let goal = chat
            .snapshot
            .loops
            .iter()
            .find(|goal| goal.id == loop_id)
            .ok_or(AgentError::NotFound)?;
        let trigger = goal
            .trigger
            .as_ref()
            .filter(|trigger| trigger.accepts_events)
            .ok_or_else(|| AgentError::Invalid("goal does not accept explicit events".into()))?;
        let input = if source.source == TICK_SUBSCRIPTION_SOURCE {
            local_messages::tick_delivery(&source.payload, trigger)
        } else {
            local_messages::external_trigger(&source, trigger)
        };
        let is_eligible = can_trigger(goal);
        let cancel = chat.trigger_cancellations.get(&loop_id).cloned();
        drop(chats);
        if is_eligible {
            match self.acquire_trigger_permit(chat_id, loop_id).await {
                TriggerPermit::Fire => {}
                TriggerPermit::Wait(reason) => return Err(AgentError::Invalid(reason)),
                TriggerPermit::Withdrawn => {
                    return Err(AgentError::Invalid(
                        "this goal was stopped or its trigger disarmed in another Tickloop \
                         process; the event was not delivered"
                            .into(),
                    ));
                }
                TriggerPermit::Failed(error) => return Err(AgentError::Storage(error)),
            }
        }
        if source.source == TICK_SUBSCRIPTION_SOURCE
            && let Err(error) = self
                .revalidate_tick_source(chat_id, loop_id, &source.payload)
                .await
        {
            self.discard_invalid_pending_tick_inputs().await;
            return Err(error);
        }
        self.submit_internal(
            chat_id,
            Some(loop_id),
            input,
            SubmissionOptions {
                trigger_source: Some(source),
                trigger_cancel: cancel,
                ..Default::default()
            },
        )
        .await
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use tickloop_agent_test_support::{ScriptedModel, config, text};

    #[tokio::test]
    async fn schedule_clock_corrects_an_anchor_ahead_of_wall_time() {
        let clock = ScheduleClock::new();
        let system = tickloop_agent_base::private_fs::unix_time_milliseconds();
        *clock.anchor.lock().unwrap() = (system + 3_600_000, tokio::time::Instant::now());
        let corrected = clock.now_ms();
        assert!(corrected < system + SCHEDULE_CLOCK_CATCH_UP_MS);
        let minutes = (system / 60_000 + 10) % (24 * 60);
        let mut trigger = Trigger::for_loop_goal("daily check");
        trigger.schedule = Some(
            format!("daily {:02}:{:02}", minutes / 60, minutes % 60)
                .parse()
                .unwrap(),
        );
        trigger.arm_at(system);
        assert!(
            trigger.due_scheduled_ms(corrected).is_none(),
            "a clock ahead must not fire the daily check early"
        );
        let due = trigger.next_scheduled_ms().unwrap();
        trigger.note_scheduled(due);
        *clock.anchor.lock().unwrap() = (system - 3_600_000, tokio::time::Instant::now());
        let corrected = clock.now_ms();
        assert!(corrected >= system);
        trigger.note_scheduled(corrected);
        assert_eq!(trigger.last_fired_at_ms, Some(due));
        assert!(
            trigger.due_scheduled_ms(due).is_none(),
            "rebasing never repeats a processed fire"
        );
    }

    async fn scheduled_agent() -> (Agent, Receipt) {
        let agent = Agent::with_seams(
            HarnessSeams {
                transcripts: None,
                environment: None,
                models: tickloop_agent_test_support::harness::ChosenModels::new(Arc::new(
                    ScriptedModel::new(vec![text("ready")]),
                )),
            },
            HarnessOptions {
                search: Default::default(),
                model: config(),
                settings: None,
                host: AgentHostOptions {
                    prompt_policy: tickloop_agent_protocol::PromptPolicy::DenyOnPrompt,
                    ..AgentHostOptions::product_defaults()
                },
                tools: ToolPolicy::web_fetch_only(),
                host_rules: Vec::new(),
                task_tools: None,
                min_schedule_interval: LOCAL_MIN_SCHEDULE_INTERVAL,
                waits: WaitHosting::Durable,
                citation_style: Default::default(),
                system_prompt_addition: None,
                request_gates: Vec::new(),
            },
        )
        .unwrap();
        let chat = agent.create_chat_without_folder().await.unwrap();
        let receipt = agent.submit(chat, None, "watch".into()).await.unwrap();
        loop {
            if agent.snapshot(chat).await.unwrap().loops[0].state == LoopState::Completed {
                break;
            }
            tokio::task::yield_now().await;
        }
        let mut chats = agent.inner.chats.lock().await;
        let chat = chats.get_mut(&chat).unwrap();
        let mut trigger = Trigger::for_loop_goal("watch");
        trigger.schedule = Some("every 1s".parse().unwrap());
        trigger.arm_at(1_000);
        chat.snapshot.loops[0].trigger = Some(trigger);
        drop(chats);
        (agent, receipt)
    }

    #[tokio::test]
    async fn refused_scheduled_fire_advances_and_surfaces_its_error() {
        for stopping in [true, false] {
            let (agent, receipt) = scheduled_agent().await;
            {
                let mut chats = agent.inner.chats.lock().await;
                let chat = chats.get_mut(&receipt.chat_id).unwrap();
                chat.family_stopping = stopping;
                if !stopping {
                    chat.branch_reservation = Some(Uuid::new_v4());
                }
            }
            assert!(
                agent
                    .wake(
                        receipt.chat_id,
                        Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(2_000))
                    )
                    .await
                    .is_err()
            );
            let snapshot = agent.snapshot(receipt.chat_id).await.unwrap();
            assert_eq!(
                snapshot.loops[0].trigger.as_ref().unwrap().last_fired_at_ms,
                Some(2_000)
            );
            assert_eq!(wake_at(&snapshot), Some(3_000));
            agent.shutdown().await;
        }
    }

    #[tokio::test]
    async fn durable_load_does_not_start_timers_for_failed_or_interrupted_loops() {
        for state in [LoopState::Failed, LoopState::Interrupted] {
            let (agent, receipt) = scheduled_agent().await;
            {
                let mut chats = agent.inner.chats.lock().await;
                let chat = chats.get_mut(&receipt.chat_id).unwrap();
                chat.snapshot.loops[0].state = state;
            }
            agent.resume_triggers_after_load(receipt.chat_id).await;
            let chats = agent.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).unwrap();
            assert!(!chat.trigger_cancellations.contains_key(&receipt.loop_id));
            assert_eq!(
                chat.snapshot.loops[0]
                    .trigger
                    .as_ref()
                    .unwrap()
                    .last_fired_at_ms,
                None
            );
            drop(chats);
            agent.shutdown().await;
        }
    }
}
