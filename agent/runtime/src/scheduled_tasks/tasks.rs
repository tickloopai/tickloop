use std::time::Duration;

use chrono::{DateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz as Zone;
use tickloop_agent_cron::{Run, Schedule, TaskKind, next_run};
use tickloop_agent_protocol::{
    MAX_SCHEDULED_TASKS_PER_CHAT, ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskCreator,
    ScheduledTaskDecision, ScheduledTaskKind,
};

/// Recurring and self-paced tasks run one last time this long after creation.
pub(crate) const TASK_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(crate) const MIN_WAKEUP_DELAY: Duration = Duration::from_secs(60);
pub(crate) const MAX_WAKEUP_DELAY: Duration = Duration::from_secs(60 * 60);
/// How long after a self-paced turn that neither reschedules nor stops the host wakes it once.
pub(crate) const FALLBACK_WAKEUP_DELAY: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ScheduledTaskError {
    #[error(
        "a Chat holds at most {MAX_SCHEDULED_TASKS_PER_CHAT} scheduled tasks; delete one first"
    )]
    TooMany,
    #[error("`{0}` names no time from now on")]
    NeverRuns(String),
    #[error("no scheduled task `{0}`")]
    NotFound(String),
    #[error("scheduled task `{0}` is not waiting for confirmation")]
    NotPending(String),
}

/// A cron-driven task to create.
#[derive(Debug, Clone)]
pub(crate) enum CronTask {
    Recurring(Schedule),
    OneShot(Schedule),
}

/// Who creates a task and whether the host must ask the user before it starts timing: the
/// Agent creating one on a host without permission requests. The caller decides both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Creation {
    pub creator: ScheduledTaskCreator,
    pub needs_confirmation: bool,
}

/// A ScheduleWakeup call that reschedules the self-paced task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Wakeup {
    pub delay: Duration,
    pub reason: String,
    /// Becomes the instruction only of a self-paced task this call creates; an existing task
    /// keeps the instruction the user started or confirmed.
    pub prompt: String,
}

/// The self-paced task after a ScheduleWakeup call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScheduledWakeup<'a> {
    pub task: &'a ScheduledTask,
    /// The call named an instruction other than the task's, which was kept; the tool result
    /// says so.
    pub ignored_prompt: bool,
}

/// The result of the user's answer to a pending task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecisionOutcome<'a> {
    Confirmed(&'a ScheduledTask),
    Rejected(ScheduledTask),
    /// Confirmed too late: its one-shot time or its seven days had passed, so it was deleted
    /// without running.
    ExpiredUnrun(ScheduledTask),
}

/// What became of the self-paced task when one of its turns ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelfPacedTurnEnd {
    /// No self-paced task, or the turn already rescheduled it.
    Unchanged,
    /// The turn neither rescheduled nor stopped, so the host wakes it once more later.
    FallbackScheduled,
    /// The fallback turn did not reschedule either, so the task ended.
    Ended(Box<ScheduledTask>),
}

/// One task run that came due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FiredTask {
    /// The task as it stands after this run.
    pub task: ScheduledTask,
    /// The latest scheduled time this run covers; with the task id it identifies the run.
    pub scheduled_for_ms: u64,
    /// Earlier scheduled times this run covers instead of running them.
    pub skipped: u64,
    /// Whether the task is deleted after this run.
    pub is_last: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DueTasks {
    pub fired: Vec<FiredTask>,
    /// Tasks deleted by this pass, including those that just ran for the last time.
    pub removed: Vec<ScheduledTask>,
}

/// A Chat's scheduled tasks, in creation order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScheduledTasks {
    tasks: Vec<ScheduledTask>,
}

impl ScheduledTasks {
    pub(crate) fn new(tasks: Vec<ScheduledTask>) -> Self {
        Self { tasks }
    }

    pub(crate) fn tasks(&self) -> &[ScheduledTask] {
        &self.tasks
    }

    pub(crate) fn into_tasks(self) -> Vec<ScheduledTask> {
        self.tasks
    }

    #[cfg(test)]
    pub(crate) fn get(&self, id: &str) -> Option<&ScheduledTask> {
        self.tasks.iter().find(|task| task.id == id)
    }

    /// A recurring or one-shot task whose cron expression reads times in `zone`.
    pub(crate) fn create_cron(
        &mut self,
        id: String,
        cron_task: CronTask,
        prompt: String,
        creation: Creation,
        zone: Zone,
        now: &DateTime<Utc>,
    ) -> Result<&ScheduledTask, ScheduledTaskError> {
        self.ensure_room(false)?;
        let (kind, expires_at_ms) = match &cron_task {
            CronTask::Recurring(schedule) => (
                ScheduledTaskKind::Recurring {
                    cron: schedule.expression().to_string(),
                },
                Some(expiry(now)),
            ),
            CronTask::OneShot(schedule) => (
                ScheduledTaskKind::OneShot {
                    cron: schedule.expression().to_string(),
                },
                None,
            ),
        };
        let mut task = new_task(id, kind, prompt, creation, zone, now, expires_at_ms);
        let (schedule, task_kind) = match &cron_task {
            CronTask::Recurring(schedule) => (schedule, TaskKind::Recurring),
            CronTask::OneShot(schedule) => (schedule, TaskKind::OneShot),
        };
        let run = in_zone(now, &zone)
            .and_then(|now| next_run(schedule, task_kind, &task.id, &now))
            .ok_or_else(|| ScheduledTaskError::NeverRuns(schedule.expression().to_string()))?;
        if !creation.needs_confirmation {
            set_timing(&mut task, &run);
        }
        Ok(self.push(task))
    }

    /// `/loop` without an interval: replaces any self-paced task with one whose first turn is
    /// the one running now, so it waits for that turn to reschedule it.
    pub(crate) fn start_self_paced(
        &mut self,
        id: String,
        prompt: String,
        creation: Creation,
        zone: Zone,
        now: &DateTime<Utc>,
    ) -> Result<&ScheduledTask, ScheduledTaskError> {
        self.ensure_room(true)?;
        self.tasks.retain(|task| !is_self_paced(task));
        let kind = ScheduledTaskKind::SelfPaced {
            delay_seconds: None,
            reason: None,
            fallback: false,
        };
        let task = new_task(id, kind, prompt, creation, zone, now, Some(expiry(now)));
        Ok(self.push(task))
    }

    /// ScheduleWakeup: reschedules the self-paced task, creating one with `id` when there is
    /// none. The delay is clamped to one minute through one hour. Rescheduling changes only
    /// the delay and the reason: replacing the instruction would let the Agent swap what the
    /// user started or confirmed without asking again.
    pub(crate) fn schedule_wakeup(
        &mut self,
        id: String,
        wakeup: Wakeup,
        creation: Creation,
        zone: Zone,
        now: &DateTime<Utc>,
    ) -> Result<ScheduledWakeup<'_>, ScheduledTaskError> {
        let delay = wakeup.delay.clamp(MIN_WAKEUP_DELAY, MAX_WAKEUP_DELAY);
        let kind = ScheduledTaskKind::SelfPaced {
            delay_seconds: Some(delay.as_secs()),
            reason: Some(wakeup.reason),
            fallback: false,
        };
        let (index, ignored_prompt) = match self.tasks.iter().position(is_self_paced) {
            Some(index) => {
                let task = &mut self.tasks[index];
                task.kind = kind;
                (index, task.prompt != wakeup.prompt)
            }
            None => {
                self.ensure_room(false)?;
                let task = new_task(
                    id,
                    kind,
                    wakeup.prompt,
                    creation,
                    zone,
                    now,
                    Some(expiry(now)),
                );
                self.tasks.push(task);
                (self.tasks.len() - 1, false)
            }
        };
        let task = &mut self.tasks[index];
        if task.confirmation != Some(ScheduledTaskConfirmation::Pending) {
            set_wake(task, now, delay);
        }
        Ok(ScheduledWakeup {
            task: &self.tasks[index],
            ignored_prompt,
        })
    }

    /// ScheduleWakeup `stop`, or Esc: the self-paced task ends with its pending wake-up.
    pub(crate) fn stop_self_paced(&mut self) -> Option<ScheduledTask> {
        let index = self.tasks.iter().position(is_self_paced)?;
        Some(self.tasks.remove(index))
    }

    /// A turn of the self-paced task ended. Without a reschedule the host wakes it once more
    /// after [`FALLBACK_WAKEUP_DELAY`]; when that fallback turn does not reschedule either, the
    /// task ends.
    pub(crate) fn self_paced_turn_ended(&mut self, now: &DateTime<Utc>) -> SelfPacedTurnEnd {
        let Some(index) = self.tasks.iter().position(is_self_paced) else {
            return SelfPacedTurnEnd::Unchanged;
        };
        let task = &mut self.tasks[index];
        if task.scheduled_for_ms.is_some()
            || task.confirmation == Some(ScheduledTaskConfirmation::Pending)
        {
            return SelfPacedTurnEnd::Unchanged;
        }
        let ScheduledTaskKind::SelfPaced { fallback, .. } = &mut task.kind else {
            return SelfPacedTurnEnd::Unchanged;
        };
        if *fallback {
            return SelfPacedTurnEnd::Ended(Box::new(self.tasks.remove(index)));
        }
        *fallback = true;
        set_wake(task, now, FALLBACK_WAKEUP_DELAY);
        SelfPacedTurnEnd::FallbackScheduled
    }

    pub(crate) fn delete(&mut self, id: &str) -> Option<ScheduledTask> {
        let index = self.tasks.iter().position(|task| task.id == id)?;
        Some(self.tasks.remove(index))
    }

    /// The user's answer to a pending task: confirming starts its timing from `now`, unless
    /// the task already expired, and rejecting deletes it.
    pub(crate) fn decide(
        &mut self,
        id: &str,
        decision: ScheduledTaskDecision,
        now: &DateTime<Utc>,
    ) -> Result<DecisionOutcome<'_>, ScheduledTaskError> {
        let index = self
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or_else(|| ScheduledTaskError::NotFound(id.to_string()))?;
        if self.tasks[index].confirmation != Some(ScheduledTaskConfirmation::Pending) {
            return Err(ScheduledTaskError::NotPending(id.to_string()));
        }
        match decision {
            ScheduledTaskDecision::Reject => {
                Ok(DecisionOutcome::Rejected(self.tasks.remove(index)))
            }
            ScheduledTaskDecision::Confirm => {
                if has_expired(&self.tasks[index], now) {
                    return Ok(DecisionOutcome::ExpiredUnrun(self.tasks.remove(index)));
                }
                let task = &mut self.tasks[index];
                task.confirmation = Some(ScheduledTaskConfirmation::Confirmed);
                match timing_after(task, now) {
                    Timing::Run(run) => set_timing(task, &run),
                    Timing::Wake(delay) => set_wake(task, now, delay),
                    Timing::AwaitingTurn => {}
                    Timing::Never => {
                        self.tasks.remove(index);
                        return Err(ScheduledTaskError::NeverRuns(id.to_string()));
                    }
                }
                Ok(DecisionOutcome::Confirmed(&self.tasks[index]))
            }
        }
    }

    /// Runs every timing task whose fire time has come, each at most once: scheduled times
    /// passed over while the Loop was busy or the host woke late count as skipped, never as
    /// extra runs. One-shot tasks, and recurring or self-paced ones past their expiry, are
    /// deleted after running.
    pub(crate) fn take_due(&mut self, now: &DateTime<Utc>) -> DueTasks {
        let now_ms = millis(now);
        let mut due = DueTasks::default();
        let mut kept = Vec::with_capacity(self.tasks.len());
        for mut task in std::mem::take(&mut self.tasks) {
            let is_due = task.is_timing()
                && task
                    .next_fire_at_ms
                    .is_some_and(|fire_at| fire_at <= now_ms);
            if !is_due {
                kept.push(task);
                continue;
            }
            let Some(fired) = fire(&mut task, now) else {
                due.removed.push(task);
                continue;
            };
            let is_last = fired.is_last;
            due.fired.push(fired);
            if is_last {
                due.removed.push(task);
            } else {
                kept.push(task);
            }
        }
        self.tasks = kept;
        due
    }

    /// Receiving resumed: every task waits for its next fire time strictly after `now`, so
    /// nothing missed while paused runs late. One-shot and self-paced tasks whose fire time
    /// passed, jitter included, are deleted. Returns the deleted tasks.
    pub(crate) fn resume(&mut self, now: &DateTime<Utc>) -> Vec<ScheduledTask> {
        let now_ms = millis(now);
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.tasks.len());
        for mut task in std::mem::take(&mut self.tasks) {
            let keep = match (&task.kind, task.is_timing()) {
                (_, false) => true,
                (ScheduledTaskKind::Recurring { .. }, true) => match timing_after(&task, now) {
                    Timing::Run(run) => {
                        set_timing(&mut task, &run);
                        true
                    }
                    Timing::Wake(_) | Timing::AwaitingTurn | Timing::Never => false,
                },
                (ScheduledTaskKind::OneShot { .. } | ScheduledTaskKind::SelfPaced { .. }, true) => {
                    task.next_fire_at_ms.is_some_and(|fire_at| fire_at > now_ms)
                }
            };
            if keep {
                kept.push(task);
            } else {
                removed.push(task);
            }
        }
        self.tasks = kept;
        removed
    }

    /// An ordinary history load: the self-paced task is not restored, and one-shot tasks
    /// whose time passed and expired recurring tasks are deleted. Receiving starts paused, so
    /// [`Self::resume`] sets the timing of what remains. Returns the deleted tasks.
    pub(crate) fn after_history_load(&mut self, now: &DateTime<Utc>) -> Vec<ScheduledTask> {
        let (kept, removed) = std::mem::take(&mut self.tasks)
            .into_iter()
            .partition(|task| !is_self_paced(task) && !has_expired(task, now));
        self.tasks = kept;
        removed
    }

    /// When the host must next wake the Loop: the earliest fire time of a confirmed, timing
    /// task, and none while receiving is paused.
    pub(crate) fn next_wake_at_ms(&self, receiving: bool) -> Option<u64> {
        if !receiving {
            return None;
        }
        self.tasks
            .iter()
            .filter(|task| task.is_timing())
            .filter_map(|task| task.next_fire_at_ms)
            .min()
    }

    fn ensure_room(&self, replaces_self_paced: bool) -> Result<(), ScheduledTaskError> {
        let replaced = usize::from(replaces_self_paced && self.tasks.iter().any(is_self_paced));
        if self.tasks.len() - replaced >= MAX_SCHEDULED_TASKS_PER_CHAT {
            Err(ScheduledTaskError::TooMany)
        } else {
            Ok(())
        }
    }

    fn push(&mut self, task: ScheduledTask) -> &ScheduledTask {
        self.tasks.push(task);
        &self.tasks[self.tasks.len() - 1]
    }
}

/// How a task's next run is set from a given time.
enum Timing {
    Run(Run<Zone>),
    Wake(Duration),
    /// A self-paced task whose turn has not picked a wake-up yet.
    AwaitingTurn,
    Never,
}

fn timing_after(task: &ScheduledTask, after: &DateTime<Utc>) -> Timing {
    let (cron, task_kind) = match &task.kind {
        ScheduledTaskKind::Recurring { cron } => (cron, TaskKind::Recurring),
        ScheduledTaskKind::OneShot { cron } => (cron, TaskKind::OneShot),
        ScheduledTaskKind::SelfPaced {
            delay_seconds: Some(delay_seconds),
            ..
        } => return Timing::Wake(Duration::from_secs(*delay_seconds)),
        ScheduledTaskKind::SelfPaced {
            delay_seconds: None,
            ..
        } => return Timing::AwaitingTurn,
    };
    let (Ok(schedule), Some(zone)) = (Schedule::parse(cron), zone_of(task)) else {
        return Timing::Never;
    };
    let Some(after) = in_zone(after, &zone) else {
        return Timing::Never;
    };
    let mut run = next_run(&schedule, task_kind, &task.id, &after);
    // A recurring run jittered early can fire no later than `after` (it is then clamped to
    // it); starting the task then would run a time that passed, so it waits for the next.
    if task_kind == TaskKind::Recurring {
        while let Some(early) = run.take_if(|run| run.fire_at <= after) {
            run = next_run(&schedule, task_kind, &task.id, &early.scheduled_for);
        }
    }
    run.map_or(Timing::Never, Timing::Run)
}

/// Runs a due task: records the run and sets its next one. `None` when the stored schedule
/// no longer parses, so the task is dropped.
fn fire(task: &mut ScheduledTask, now: &DateTime<Utc>) -> Option<FiredTask> {
    let now_ms = millis(now);
    let mut scheduled_for_ms = task.scheduled_for_ms?;
    let mut skipped = 0;
    match &task.kind {
        ScheduledTaskKind::Recurring { cron } => {
            let schedule = Schedule::parse(cron).ok()?;
            let zone = zone_of(task)?;
            let next = loop {
                let scheduled_for = at(&zone, scheduled_for_ms)?;
                match next_run(&schedule, TaskKind::Recurring, &task.id, &scheduled_for) {
                    Some(run) if millis(&run.fire_at) <= now_ms => {
                        skipped += 1;
                        scheduled_for_ms = millis(&run.scheduled_for);
                    }
                    following => break following,
                }
            };
            match next {
                Some(run) => set_timing(task, &run),
                None => clear_timing(task),
            }
        }
        ScheduledTaskKind::OneShot { .. } | ScheduledTaskKind::SelfPaced { .. } => {
            clear_timing(task)
        }
    }
    let is_expired = task
        .expires_at_ms
        .is_some_and(|expires_at| expires_at <= now_ms);
    let is_last = is_expired
        || match task.kind {
            ScheduledTaskKind::OneShot { .. } => true,
            ScheduledTaskKind::Recurring { .. } => task.next_fire_at_ms.is_none(),
            ScheduledTaskKind::SelfPaced { .. } => false,
        };
    if is_last {
        clear_timing(task);
    }
    task.last_fired_at_ms = Some(now_ms);
    task.last_scheduled_for_ms = Some(scheduled_for_ms);
    task.skipped += skipped;
    Some(FiredTask {
        task: task.clone(),
        scheduled_for_ms,
        skipped,
        is_last,
    })
}

/// Whether a one-shot task's time or a recurring or self-paced task's seven days have passed.
fn has_expired(task: &ScheduledTask, now: &DateTime<Utc>) -> bool {
    let now_ms = millis(now);
    match &task.kind {
        ScheduledTaskKind::Recurring { .. } | ScheduledTaskKind::SelfPaced { .. } => task
            .expires_at_ms
            .is_some_and(|expires_at| expires_at <= now_ms),
        ScheduledTaskKind::OneShot { cron } => {
            one_shot_time(task, cron).is_none_or(|scheduled_for| scheduled_for <= now_ms)
        }
    }
}

/// The scheduled time of a one-shot task: the stored one, or for a pending task the first
/// time its cron names after creation.
fn one_shot_time(task: &ScheduledTask, cron: &str) -> Option<u64> {
    if let Some(scheduled_for) = task.scheduled_for_ms {
        return Some(scheduled_for);
    }
    let created_at = at(&zone_of(task)?, task.created_at_ms)?;
    let schedule = Schedule::parse(cron).ok()?;
    schedule.next_after(&created_at).map(|time| millis(&time))
}

fn new_task(
    id: String,
    kind: ScheduledTaskKind,
    prompt: String,
    creation: Creation,
    zone: Zone,
    now: &DateTime<Utc>,
    expires_at_ms: Option<u64>,
) -> ScheduledTask {
    ScheduledTask {
        id,
        kind,
        prompt,
        creator: creation.creator,
        time_zone: zone.name().to_string(),
        confirmation: creation
            .needs_confirmation
            .then_some(ScheduledTaskConfirmation::Pending),
        created_at_ms: millis(now),
        expires_at_ms,
        scheduled_for_ms: None,
        next_fire_at_ms: None,
        jitter_ms: 0,
        last_fired_at_ms: None,
        last_scheduled_for_ms: None,
        started_by_run: None,
        skipped: 0,
    }
}

fn set_timing<Tz: TimeZone>(task: &mut ScheduledTask, run: &Run<Tz>) {
    task.scheduled_for_ms = Some(millis(&run.scheduled_for));
    task.next_fire_at_ms = Some(millis(&run.fire_at));
    task.jitter_ms = run.jitter().num_milliseconds();
}

fn set_wake(task: &mut ScheduledTask, now: &DateTime<Utc>, delay: Duration) {
    let wake_at_ms =
        millis(now).saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
    task.scheduled_for_ms = Some(wake_at_ms);
    task.next_fire_at_ms = Some(wake_at_ms);
    task.jitter_ms = 0;
}

fn clear_timing(task: &mut ScheduledTask) {
    task.scheduled_for_ms = None;
    task.next_fire_at_ms = None;
    task.jitter_ms = 0;
}

/// The zone a task's cron expression reads times in; `None` for a name this build does not
/// know, which drops the task like an expression that no longer parses.
fn zone_of(task: &ScheduledTask) -> Option<Zone> {
    task.time_zone.parse().ok()
}

fn is_self_paced(task: &ScheduledTask) -> bool {
    matches!(task.kind, ScheduledTaskKind::SelfPaced { .. })
}

fn expiry(now: &DateTime<Utc>) -> u64 {
    millis(now).saturating_add(u64::try_from(TASK_LIFETIME.as_millis()).unwrap_or(u64::MAX))
}

fn millis<Tz: TimeZone>(time: &DateTime<Tz>) -> u64 {
    u64::try_from(time.timestamp_millis()).unwrap_or(0)
}

fn at(zone: &Zone, millis: u64) -> Option<DateTime<Zone>> {
    let millis = i64::try_from(millis).ok()?;
    in_zone(&DateTime::from_timestamp_millis(millis)?, zone)
}

/// `time` in `zone`, or `None` when its local reading falls outside what chrono represents.
fn in_zone(time: &DateTime<Utc>, zone: &Zone) -> Option<DateTime<Zone>> {
    let local = time.with_timezone(zone);
    local
        .naive_utc()
        .checked_add_offset(local.offset().fix())
        .map(|_| local)
}
