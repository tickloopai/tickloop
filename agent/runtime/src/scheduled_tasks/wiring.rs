//! Scheduled tasks in the Agent: the tools, the timer of a Worker, the wake-up a stateless
//! host gives, and the Loop's turns, Esc, `/stop` and continuing the Chat.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use chrono_tz::Tz as Zone;
use serde_json::{Value, json};
use tickloop_agent_cron::Schedule;
use tickloop_agent_protocol::{
    ChatId, EventKind, OperationRecord, PromptPolicy, Receipt, ScheduledTask,
    ScheduledTaskConfirmation, ScheduledTaskCreator, ScheduledTaskDecision, ScheduledTaskKind,
    ScheduledTaskNotice, ToolResult, TriggerSource,
};
use tickloop_agent_transcript::{DiscardReason, Line, ScheduledTasksEntry};

use super::LOOP_SKILL_NAME;
use super::tasks::{
    Creation, CronTask, DecisionOutcome, ScheduledTaskError, ScheduledTasks, SelfPacedTurnEnd,
    Wakeup,
};
use super::tools::{self, ScheduledTaskCall};
use crate::wakeup::TriggerDelivery;
use crate::{Agent, AgentError, Chat};

/// The longest a Worker's timer sleeps before it looks at the clock again, so a machine that
/// slept or a wall clock that moved does not delay a run for long.
const TIMER_RECHECK: Duration = Duration::from_secs(60);

/// The time zone scheduled tasks read cron expressions in, and the clock they run by.
pub(crate) struct ScheduleClock {
    zone: Zone,
    #[cfg(test)]
    fixed_now: std::sync::Mutex<Option<DateTime<Utc>>>,
}

impl ScheduleClock {
    pub(crate) fn new(zone: Zone) -> Self {
        Self {
            zone,
            #[cfg(test)]
            fixed_now: std::sync::Mutex::new(None),
        }
    }

    pub(crate) fn zone(&self) -> Zone {
        self.zone
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        #[cfg(test)]
        if let Some(now) = *self.fixed_now.lock().unwrap() {
            return now;
        }
        DateTime::from(SystemTime::now())
    }

    #[cfg(test)]
    pub(crate) fn set_now(&self, now: DateTime<Utc>) {
        *self.fixed_now.lock().unwrap() = Some(now);
    }
}

/// The zone a host names, or the machine's own: a stateless host passes the user's, a Worker
/// reads its local one, and a machine whose zone cannot be read uses UTC.
pub(crate) fn resolve_zone(name: Option<&str>) -> Result<Zone, String> {
    match name {
        Some(name) => name
            .parse()
            .map_err(|_| format!("`{name}` is not an IANA time zone")),
        None => Ok(iana_time_zone::get_timezone()
            .ok()
            .and_then(|name| name.parse().ok())
            .unwrap_or(Zone::UTC)),
    }
}

/// The scheduled run's input: the task's instruction, a Skill the Agent may invoke on its own
/// when it names one with `/`, and anything else as plain text.
fn run_input(chat: &Chat, task: &ScheduledTask) -> String {
    let prompt = task.prompt.trim();
    if let Some(command) = prompt.strip_prefix('/') {
        let (name, arguments) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, arguments)| (name, arguments.trim()));
        if chat.skills.contains(name) && !chat.skills.is_user_only(name) {
            let arguments = if arguments.is_empty() {
                String::new()
            } else {
                format!(" with the arguments `{arguments}`")
            };
            return format!(
                "Scheduled task {}: invoke the `{name}` Skill with the Skill tool{arguments}.",
                task.id
            );
        }
    }
    task.prompt.clone()
}

/// A time as the user reads it in the task's zone.
fn local_time(millis: Option<u64>, zone: &str) -> Value {
    let time = millis
        .and_then(|millis| i64::try_from(millis).ok())
        .and_then(DateTime::from_timestamp_millis)
        .zip(zone.parse::<Zone>().ok())
        .map(|(time, zone)| {
            time.with_timezone(&zone)
                .format("%Y-%m-%d %H:%M:%S %Z")
                .to_string()
        });
    json!(time)
}

fn describe(task: &ScheduledTask) -> Value {
    let (schedule, recurring) = match &task.kind {
        ScheduledTaskKind::Recurring { cron } => (cron.as_str(), true),
        ScheduledTaskKind::OneShot { cron } => (cron.as_str(), false),
        ScheduledTaskKind::SelfPaced { .. } => ("self-paced", true),
    };
    json!({
        "id": task.id,
        "schedule": schedule,
        "recurring": recurring,
        "time_zone": task.time_zone,
        "next_run": local_time(task.next_fire_at_ms, &task.time_zone),
        "expires": local_time(task.expires_at_ms, &task.time_zone),
        "awaiting_confirmation": task.confirmation == Some(ScheduledTaskConfirmation::Pending),
        "skipped": task.skipped,
        "prompt": task.prompt,
    })
}

fn tool_result(value: Value) -> ToolResult {
    ToolResult {
        content: value.to_string(),
        is_error: false,
        output_ref: None,
    }
}

fn tool_error(message: String) -> ToolResult {
    ToolResult {
        content: message,
        is_error: true,
        output_ref: None,
    }
}

/// The scheduled task that started the run `run`: the latest run of the self-paced task is
/// named by its delivery's ledger input.
fn started_run(chat: &Chat, task: &ScheduledTask, run: tickloop_agent_protocol::RunId) -> bool {
    task.last_scheduled_for_ms.is_some_and(|scheduled_for_ms| {
        let input = tickloop_agent_transcript::input_ids::trigger(
            chat.snapshot.id,
            &TriggerSource::ScheduledTask {
                task_id: task.id.clone(),
                scheduled_for_ms,
            },
        );
        chat.bookkeeping
            .lifecycle
            .input(input)
            .is_some_and(|input| input.run_id == run)
    })
}

impl Agent {
    pub(crate) fn scheduled_now(&self) -> DateTime<Utc> {
        self.inner.schedule_clock.now()
    }

    #[cfg(test)]
    pub(crate) fn set_scheduled_now(&self, now: DateTime<Utc>) {
        self.inner.schedule_clock.set_now(now);
    }

    /// Records the Chat's scheduled tasks after a change, announces it, and has the timer look
    /// at them again.
    pub(crate) fn persist_scheduled_tasks(
        &self,
        chat: &mut Chat,
        notices: Vec<ScheduledTaskNotice>,
    ) {
        self.record_chat_metadata(
            chat.snapshot.id,
            Line::ScheduledTasks(ScheduledTasksEntry {
                chat_id: chat.snapshot.id,
                tasks: chat.snapshot.scheduled_tasks.clone(),
            }),
        );
        self.save(&chat.snapshot);
        let receipt = Receipt {
            chat_id: chat.snapshot.id,
            loop_id: chat.snapshot.chat_loop.id,
            run_id: chat.snapshot.chat_loop.run_id.unwrap_or_default(),
        };
        if notices.is_empty() {
            self.publish_event(
                chat,
                &receipt,
                EventKind::ScheduledTasksChanged { notice: None },
            );
        }
        for notice in notices {
            self.publish_event(
                chat,
                &receipt,
                EventKind::ScheduledTasksChanged {
                    notice: Some(notice),
                },
            );
        }
        self.rearm_scheduled_timer();
    }

    /// Withdraws the queued run of each deleted task: a task's input waits only while the task
    /// does.
    fn withdraw_deleted_task_runs(&self, chat: &mut Chat, deleted: &[ScheduledTask]) {
        let waiting: Vec<Receipt> = chat
            .snapshot
            .pending_inputs
            .iter()
            .filter(|pending| {
                matches!(
                    &pending.trigger_source,
                    Some(TriggerSource::ScheduledTask { task_id, .. })
                        if deleted.iter().any(|task| &task.id == task_id)
                )
            })
            .map(|pending| pending.receipt.clone())
            .collect();
        for receipt in waiting {
            self.withdraw_trigger_input(chat, &receipt, DiscardReason::SourceEnded);
        }
    }

    /// Deletes tasks from the Chat, with their queued runs.
    fn delete_scheduled(
        &self,
        chat: &mut Chat,
        deleted: Vec<ScheduledTask>,
        notices: Vec<ScheduledTaskNotice>,
    ) {
        if deleted.is_empty() {
            return;
        }
        chat.snapshot
            .scheduled_tasks
            .retain(|task| !deleted.iter().any(|gone| gone.id == task.id));
        self.withdraw_deleted_task_runs(chat, &deleted);
        self.persist_scheduled_tasks(chat, notices);
    }

    /// Who creates a task in the turn `receipt`: the user in the turn of their own `/loop`, the
    /// Agent otherwise; only the Agent's need confirming, and only where nobody is asked.
    fn creation(&self, chat: &Chat, receipt: &Receipt) -> Creation {
        let is_users_loop = chat
            .snapshot
            .invoked_skills
            .iter()
            .any(|skill| skill.name == LOOP_SKILL_NAME && skill.run_id == Some(receipt.run_id));
        let creator = if is_users_loop {
            ScheduledTaskCreator::User
        } else {
            ScheduledTaskCreator::Agent
        };
        Creation {
            creator,
            needs_confirmation: creator == ScheduledTaskCreator::Agent
                && self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt,
        }
    }

    /// One call of the scheduled task tools.
    pub(crate) async fn scheduled_task_tool(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
    ) -> Result<ToolResult, String> {
        let call = match tools::parse(&operation.name, &operation.input) {
            Ok(call) => call,
            Err(message) => return Ok(tool_error(message)),
        };
        let now = self.scheduled_now();
        let zone = self.inner.schedule_clock.zone();
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or_else(|| "the Chat is gone".to_owned())?;
        if chat.snapshot.delegation.is_some() {
            return Ok(tool_error(
                "Only the root Agent schedules tasks; a Subagent has none.".into(),
            ));
        }
        let creation = self.creation(chat, receipt);
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let id = tasks.unused_id();
        let outcome = match call {
            ScheduledTaskCall::Create {
                cron,
                prompt,
                recurring,
            } => match Schedule::parse(&cron) {
                Err(error) => Err(error.to_string()),
                Ok(schedule) => {
                    let cron_task = if recurring {
                        CronTask::Recurring(schedule)
                    } else {
                        CronTask::OneShot(schedule)
                    };
                    tasks
                        .create_cron(id, cron_task, prompt, creation, zone, &now)
                        .map(|task| (describe(task), Vec::new(), Vec::new()))
                        .map_err(|error| error.to_string())
                }
            },
            ScheduledTaskCall::List => {
                let listed: Vec<Value> = tasks.tasks().iter().map(describe).collect();
                return Ok(tool_result(json!({ "tasks": listed })));
            }
            ScheduledTaskCall::Delete { id } => match tasks.delete(&id) {
                Some(task) => Ok((json!({ "deleted": task.id }), vec![task], Vec::new())),
                None => Err(ScheduledTaskError::NotFound(id).to_string()),
            },
            ScheduledTaskCall::Wakeup {
                delay,
                reason,
                prompt,
            } => tasks
                .schedule_wakeup(
                    id,
                    Wakeup {
                        delay,
                        reason,
                        prompt,
                    },
                    creation,
                    zone,
                    &now,
                )
                .map(|scheduled| {
                    let mut value = describe(scheduled.task);
                    if scheduled.ignored_prompt {
                        value["note"] = json!(
                            "The loop keeps the instruction it was started with; the prompt passed here was not used."
                        );
                    }
                    (value, Vec::new(), Vec::new())
                })
                .map_err(|error| error.to_string()),
            ScheduledTaskCall::StopWakeup => match tasks.stop_self_paced() {
                Some(task) => Ok((json!({ "stopped": task.id }), vec![task], Vec::new())),
                None => Ok((
                    json!({ "stopped": null, "note": "No self-paced loop is running." }),
                    Vec::new(),
                    Vec::new(),
                )),
            },
        };
        let (value, deleted, notices) = match outcome {
            Ok(outcome) => outcome,
            Err(message) => return Ok(tool_error(message)),
        };
        chat.snapshot.scheduled_tasks = tasks.into_tasks();
        self.withdraw_deleted_task_runs(chat, &deleted);
        self.persist_scheduled_tasks(chat, notices);
        Ok(tool_result(value))
    }

    /// The user schedules a task through the host's interface, as with `/loop`: it needs no
    /// confirmation and starts timing at once.
    pub async fn schedule_task(
        &self,
        chat_id: ChatId,
        cron: &str,
        prompt: String,
        recurring: bool,
    ) -> Result<ScheduledTask, AgentError> {
        let schedule =
            Schedule::parse(cron).map_err(|error| AgentError::Invalid(error.to_string()))?;
        let cron_task = if recurring {
            CronTask::Recurring(schedule)
        } else {
            CronTask::OneShot(schedule)
        };
        let now = self.scheduled_now();
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if chat.snapshot.delegation.is_some() {
            return Err(AgentError::Invalid(
                "only the root Agent has scheduled tasks".into(),
            ));
        }
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let id = tasks.unused_id();
        let task = tasks
            .create_cron(
                id,
                cron_task,
                prompt,
                Creation {
                    creator: ScheduledTaskCreator::User,
                    needs_confirmation: false,
                },
                self.inner.schedule_clock.zone(),
                &now,
            )
            .map_err(|error| AgentError::Invalid(error.to_string()))?
            .clone();
        chat.snapshot.scheduled_tasks = tasks.into_tasks();
        self.persist_scheduled_tasks(chat, Vec::new());
        Ok(task)
    }

    /// Cancels one of the Chat's scheduled tasks at the user's request.
    pub async fn delete_scheduled_task(
        &self,
        chat_id: ChatId,
        task_id: &str,
    ) -> Result<(), AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let deleted = tasks
            .delete(task_id)
            .ok_or_else(|| AgentError::Invalid(format!("no scheduled task `{task_id}`")))?;
        self.delete_scheduled(chat, vec![deleted], Vec::new());
        Ok(())
    }

    /// The user's answer to a task the Agent created on a host without permission requests:
    /// confirming starts its timing now, unless its time already passed, when it is deleted
    /// without running; rejecting deletes it.
    pub async fn decide_scheduled_task(
        &self,
        chat_id: ChatId,
        task_id: &str,
        decision: ScheduledTaskDecision,
    ) -> Result<(), AgentError> {
        let now = self.scheduled_now();
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let notices = match tasks
            .decide(task_id, decision, &now)
            .map_err(|error| AgentError::Invalid(error.to_string()))?
        {
            DecisionOutcome::Confirmed(_) | DecisionOutcome::Rejected(_) => Vec::new(),
            DecisionOutcome::ExpiredUnrun(task) => {
                vec![ScheduledTaskNotice::ExpiredUnrun { task_id: task.id }]
            }
        };
        chat.snapshot.scheduled_tasks = tasks.into_tasks();
        self.persist_scheduled_tasks(chat, notices);
        Ok(())
    }

    /// A stateless host's wake-up at `due_at_ms`, the time the Harness reported last: every
    /// task due by then, or by now when the host's clock is ahead, runs once.
    pub async fn wake_scheduled_tasks(
        &self,
        chat_id: ChatId,
        due_at_ms: u64,
    ) -> Result<(), AgentError> {
        if !self.inner.chats.lock().await.contains_key(&chat_id) {
            return Err(AgentError::NotFound);
        }
        let due_at = i64::try_from(due_at_ms)
            .ok()
            .and_then(DateTime::from_timestamp_millis)
            .ok_or_else(|| AgentError::Invalid("the wake-up time is out of range".into()))?;
        self.run_due_scheduled_tasks(chat_id, due_at.max(self.scheduled_now()))
            .await;
        Ok(())
    }

    /// Hands the Loop each task due by `now`, at most once each, while it receives.
    pub(crate) async fn run_due_scheduled_tasks(&self, chat_id: ChatId, now: DateTime<Utc>) {
        let deliveries = {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return;
            };
            if !chat.snapshot.chat_loop.receiving.is_receiving() {
                return;
            }
            let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
            let due = tasks.take_due(&now);
            if due.fired.is_empty() && due.removed.is_empty() {
                return;
            }
            let mut deliveries = Vec::new();
            let mut notices = Vec::new();
            for fired in &due.fired {
                // A run of the task already waits: this one merges into it, so its time is
                // passed over like one the Loop was too busy for.
                let waits = chat.snapshot.pending_inputs.iter().any(|pending| {
                    matches!(
                        &pending.trigger_source,
                        Some(TriggerSource::ScheduledTask { task_id, .. }) if *task_id == fired.task.id
                    )
                });
                if waits {
                    tasks.add_skipped(&fired.task.id);
                }
                notices.push(ScheduledTaskNotice::Fired {
                    task_id: fired.task.id.clone(),
                });
                deliveries.push(TriggerDelivery {
                    source: TriggerSource::ScheduledTask {
                        task_id: fired.task.id.clone(),
                        scheduled_for_ms: fired.scheduled_for_ms,
                    },
                    input: run_input(chat, &fired.task),
                    ledger: None,
                });
            }
            chat.snapshot.scheduled_tasks = tasks.into_tasks();
            self.persist_scheduled_tasks(chat, notices);
            deliveries
        };
        for delivery in deliveries {
            if let Err(error) = self.deliver_trigger(chat_id, delivery).await {
                tracing::debug!(%chat_id, %error, "a scheduled run was not delivered");
            }
        }
    }

    /// A turn ended: when the self-paced task started it and it neither rescheduled nor
    /// stopped the loop, the host wakes it once more later, and after a fallback turn that did
    /// not reschedule either the loop ends.
    pub(crate) fn end_self_paced_turn(&self, chat: &mut Chat, run: tickloop_agent_protocol::RunId) {
        let Some(task) = chat
            .snapshot
            .scheduled_tasks
            .iter()
            .find(|task| matches!(task.kind, ScheduledTaskKind::SelfPaced { .. }))
        else {
            return;
        };
        if task.scheduled_for_ms.is_some() || !started_run(chat, task, run) {
            return;
        }
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        match tasks.self_paced_turn_ended(&self.scheduled_now()) {
            SelfPacedTurnEnd::Unchanged => {}
            SelfPacedTurnEnd::FallbackScheduled => {
                chat.snapshot.scheduled_tasks = tasks.into_tasks();
                self.persist_scheduled_tasks(chat, Vec::new());
            }
            SelfPacedTurnEnd::Ended(task) => {
                chat.snapshot.scheduled_tasks = tasks.into_tasks();
                self.persist_scheduled_tasks(
                    chat,
                    vec![ScheduledTaskNotice::SelfPacedEnded { task_id: task.id }],
                );
            }
        }
    }

    /// Esc or `/stop`: the self-paced loop's pending wake-up, its fallback and its queued run
    /// are cancelled; fixed-interval and one-shot tasks go on.
    pub(crate) fn cancel_self_paced_wakeup(&self, chat: &mut Chat) {
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let Some(task) = tasks.stop_self_paced() else {
            return;
        };
        let notice = ScheduledTaskNotice::WakeupCancelled {
            task_id: task.id.clone(),
        };
        self.delete_scheduled(chat, vec![task], vec![notice]);
    }

    /// Receiving resumed: each task waits for its next time after now, and nothing missed
    /// while paused runs late.
    pub(crate) fn resume_scheduled_tasks(&self, chat: &mut Chat) {
        if chat.snapshot.scheduled_tasks.is_empty() {
            return;
        }
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let removed = tasks.resume(&self.scheduled_now());
        chat.snapshot.scheduled_tasks = tasks.into_tasks();
        self.withdraw_deleted_task_runs(chat, &removed);
        self.persist_scheduled_tasks(chat, Vec::new());
    }

    /// When a Loop that receives next needs its host: the earliest fire time of its confirmed
    /// tasks.
    pub(crate) fn next_scheduled_wake(chat: &Chat) -> Option<u64> {
        ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone())
            .next_wake_at_ms(chat.snapshot.chat_loop.receiving.is_receiving())
    }

    /// A Worker keeps time for its Chats: it wakes for the earliest task, and at least once a
    /// minute. A stateless host keeps time itself from the wake time the Harness reports.
    pub(crate) fn rearm_scheduled_timer(&self) {
        if self.inner.waits != crate::WaitHosting::Process {
            return;
        }
        if !self
            .inner
            .scheduled_timer_started
            .swap(true, Ordering::SeqCst)
        {
            match tokio::runtime::Handle::try_current() {
                Ok(_) => self.scheduled_timer(),
                Err(_) => {
                    self.inner
                        .scheduled_timer_started
                        .store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        self.inner.scheduled_wake.notify_one();
    }

    fn scheduled_timer(&self) {
        let agent = self.downgrade();
        let (shutdown, wake) = (
            self.inner.shutdown.clone(),
            self.inner.scheduled_wake.clone(),
        );
        self.inner.tasks.spawn(async move {
            loop {
                let Some(agent) = agent.upgrade() else {
                    return;
                };
                let now = agent.scheduled_now();
                let (next, due): (Option<u64>, Vec<ChatId>) = {
                    let chats = agent.inner.chats.lock().await;
                    let now_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
                    let wakes: Vec<(ChatId, u64)> = chats
                        .iter()
                        .filter_map(|(id, chat)| {
                            Self::next_scheduled_wake(chat).map(|at| (*id, at))
                        })
                        .collect();
                    (
                        wakes
                            .iter()
                            .map(|(_, at)| *at)
                            .filter(|at| *at > now_ms)
                            .min(),
                        wakes
                            .iter()
                            .filter(|(_, at)| *at <= now_ms)
                            .map(|(id, _)| *id)
                            .collect(),
                    )
                };
                if !due.is_empty() {
                    let _upgrade = agent.inner.upgrade_gate.read().await;
                    for chat_id in due {
                        agent.run_due_scheduled_tasks(chat_id, now).await;
                    }
                    continue;
                }
                let sleep = next
                    .map(|at| {
                        let now_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
                        Duration::from_millis(at.saturating_sub(now_ms))
                    })
                    .unwrap_or(TIMER_RECHECK)
                    .min(TIMER_RECHECK);
                drop(agent);
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = wake.notified() => {}
                    _ = tokio::time::sleep(sleep) => {}
                }
            }
        });
    }
}

/// The task kinds a Chat's loaded history keeps, as an ordinary history load rules: the
/// self-paced loop, one-shot tasks whose time passed and expired recurring tasks go.
pub(crate) fn after_history_load(tasks: &mut Vec<ScheduledTask>, now: DateTime<Utc>) {
    let mut kept = ScheduledTasks::new(std::mem::take(tasks));
    kept.after_history_load(&now);
    *tasks = kept.into_tasks();
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
