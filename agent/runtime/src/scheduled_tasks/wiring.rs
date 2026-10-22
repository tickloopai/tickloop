//! Scheduled tasks in the Agent: the tools, the timer of a Worker, the wake-up a stateless
//! host gives, and the Loop's turns, Esc, `/stop` and continuing the Chat.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use chrono_tz::Tz as Zone;
use serde_json::{Value, json};
use tickloop_agent_cron::Schedule;
use tickloop_agent_protocol::{
    ChatId, ChatSnapshot, EventKind, OperationRecord, PromptPolicy, Receipt, ScheduledTask,
    ScheduledTaskConfirmation, ScheduledTaskCreator, ScheduledTaskDecision, ScheduledTaskKind,
    ScheduledTaskNotice, ScheduledTaskRevocation, ToolResult, TriggerSource,
};
use tickloop_agent_transcript::{DiscardReason, Line, ScheduledTasksEntry};

use super::tasks::{
    Creation, CronTask, DecisionOutcome, FiredTask, ScheduledTaskError, ScheduledTasks,
    SelfPacedTurnEnd, Wakeup,
};
use super::tools::{self, ScheduledTaskCall};
use crate::wakeup::TriggerDelivery;
use crate::{Agent, AgentError, Chat};
use tickloop_agent_config::loop_skill::{DEFAULT_INSTRUCTIONS_PROMPT, LoopFiles, LoopSchedule};

/// The longest a Worker's timer sleeps before it looks at the clock again, so a machine that
/// slept or a wall clock that moved does not delay a run for long.
const TIMER_RECHECK: Duration = Duration::from_secs(60);
/// What the last run of a recurring or self-paced task tells the Agent.
const FINAL_RUN_NOTE: &str = "This is the final run of this scheduled task: it was created seven days ago and is deleted after this run. Do not schedule it again; tell the user it ended and that they can start a new /loop to continue.";
/// Why a turn a scheduled run started cannot start a self-paced loop.
const NO_NEW_LOOP_FROM_A_RUN: &str = "No self-paced loop is running, and a turn a scheduled task started cannot start one: the loop ended, by its seven-day limit or by being cancelled. Tell the user; they can start a new /loop.";
/// How long a Worker's timer waits before it hands a refused run over again.
const RETRY_DELAY_MS: u64 = 10_000;

/// The time zone scheduled tasks read cron expressions in, and the clock they run by.
pub(crate) struct ScheduleClock {
    zone: Zone,
    #[cfg(any(test, feature = "test-support"))]
    fixed_now: std::sync::Mutex<Option<DateTime<Utc>>>,
    /// Stops a wake-up after the ledger of its runs is stored and before the tasks' next times
    /// are, as a host that goes down there.
    #[cfg(any(test, feature = "test-support"))]
    stop_before_recording: std::sync::atomic::AtomicBool,
}

impl ScheduleClock {
    pub(crate) fn new(zone: Zone) -> Self {
        Self {
            zone,
            #[cfg(any(test, feature = "test-support"))]
            fixed_now: std::sync::Mutex::new(None),
            #[cfg(any(test, feature = "test-support"))]
            stop_before_recording: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn zone(&self) -> Zone {
        self.zone
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(now) = *self.fixed_now.lock().unwrap() {
            return now;
        }
        DateTime::from(SystemTime::now())
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

/// The scheduled run's input: the Chat's loop instructions, read again now, for a `/loop`
/// without a prompt; a Skill the Agent may invoke on its own when the instruction names one with
/// `/`; and anything else as plain text.
fn run_input(chat: &Chat, task: &ScheduledTask, config_root: Option<&std::path::Path>) -> String {
    let prompt = task.prompt.trim();
    if prompt == DEFAULT_INSTRUCTIONS_PROMPT {
        return match LoopFiles::new(chat.snapshot.folder.as_deref(), config_root).instructions() {
            Ok(instructions) => instructions.prompt(),
            Err(error) => format!(
                "Scheduled task {}: the loop instructions cannot be read ({error}). Tell the user, and do not run other work in their place.",
                task.id
            ),
        };
    }
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

fn has_self_paced(tasks: &ScheduledTasks) -> bool {
    tasks
        .tasks()
        .iter()
        .any(|task| matches!(task.kind, ScheduledTaskKind::SelfPaced { .. }))
}

/// Whether a scheduled run started the turn `run`: its open ledger input is a scheduled run.
fn started_by_scheduled_run(chat: &Chat, run: tickloop_agent_protocol::RunId) -> bool {
    chat.bookkeeping
        .lifecycle
        .open_input(run)
        .and_then(|input| chat.bookkeeping.lifecycle.input(input))
        .and_then(|input| input.trigger.as_ref())
        .is_some_and(|trigger| {
            matches!(
                trigger.identity,
                tickloop_agent_protocol::TriggerIdentity::ScheduledTask { .. }
            )
        })
}

/// A due run of a task, prepared for the base: the task before it ran and as it is after.
struct DueRun {
    before: ScheduledTask,
    fired: FiredTask,
    /// A run of the task already waits, so this one merges into it.
    merges: bool,
}

/// The scheduled task that started the run `run`: the latest run of the self-paced task is
/// named by its delivery's ledger input.
fn started_run(chat: &Chat, task: &ScheduledTask, run: tickloop_agent_protocol::RunId) -> bool {
    task.last_scheduled_for_ms.is_some_and(|scheduled_for_ms| {
        let input = tickloop_agent_transcript::input_ids::trigger(
            chat.snapshot.id,
            &tickloop_agent_protocol::TriggerIdentity::ScheduledTask {
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

    /// Fixes the clock scheduled tasks run by.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn set_scheduled_now_for_tests(&self, now: DateTime<Utc>) {
        *self.inner.schedule_clock.fixed_now.lock().unwrap() = Some(now);
    }

    /// Stops each later wake-up after the ledger of its runs is stored and before the tasks'
    /// next times are, as a host that goes down there.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn stop_scheduled_wakeups_before_recording_for_tests(&self) {
        self.inner
            .schedule_clock
            .stop_before_recording
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
                revoked: chat.snapshot.scheduled_task_revocations.clone(),
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

    /// Cancels tasks: none of their runs, waiting or handed over later, is taken any more.
    fn revoke_scheduled(&self, chat: &mut Chat, cancelled: &[ScheduledTask]) {
        revoke(
            &mut chat.snapshot.scheduled_task_revocations,
            cancelled,
            self.scheduled_now(),
        );
        self.withdraw_deleted_task_runs(chat, cancelled);
    }

    /// Records the Chat's scheduled tasks as they are now.
    pub(crate) async fn record_scheduled_tasks(&self, chat_id: ChatId) {
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&chat_id) {
            self.persist_scheduled_tasks(chat, Vec::new());
        }
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
        self.revoke_scheduled(chat, &deleted);
        self.persist_scheduled_tasks(chat, notices);
    }

    /// Who creates a task in the turn `receipt`: the user only for the one task their own
    /// `/loop` of that turn schedules, exactly as the host read it; the Agent for every other.
    /// Only the Agent's need confirming, and only where nobody is asked.
    fn creation(
        &self,
        chat: &mut Chat,
        receipt: &Receipt,
        created: Option<&LoopSchedule>,
    ) -> Creation {
        let is_users_loop =
            created.is_some_and(|created| chat.user_loops.get(&receipt.run_id) == Some(created));
        if is_users_loop {
            chat.user_loops.remove(&receipt.run_id);
        }
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
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let id = unused_id(&chat.snapshot);
        let outcome = match call {
            ScheduledTaskCall::Create {
                cron,
                prompt,
                recurring,
            } => match Schedule::parse(&cron) {
                Err(error) => Err(error.to_string()),
                Ok(schedule) => {
                    let users_loop = recurring.then(|| LoopSchedule::Cron {
                        cron: schedule.expression().to_owned(),
                        prompt: prompt.clone(),
                    });
                    let creation = self.creation(chat, receipt, users_loop.as_ref());
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
            ScheduledTaskCall::Wakeup { .. }
                if !has_self_paced(&tasks) && started_by_scheduled_run(chat, receipt.run_id) =>
            {
                Err(NO_NEW_LOOP_FROM_A_RUN.to_owned())
            }
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
                    // The user's self-paced loop was started with their /loop; a new one is
                    // the Agent's.
                    self.creation(chat, receipt, None),
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
                Some(task) => {
                    let notice = ScheduledTaskNotice::SelfPacedEnded {
                        task_id: task.id.clone(),
                    };
                    Ok((json!({ "stopped": task.id }), vec![task], vec![notice]))
                }
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
        self.revoke_scheduled(chat, &deleted);
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
        let id = unused_id(&chat.snapshot);
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

    /// The user's `/loop` without an interval starts the self-paced loop with its turn, so that
    /// turn gets the fallback wake-up too; a self-paced loop running already is replaced.
    pub(crate) async fn start_users_self_paced_loop(
        &self,
        chat_id: ChatId,
        prompt: String,
    ) -> Result<Option<String>, AgentError> {
        if !self.inner.tool_policy.scheduled_tasks {
            return Ok(None);
        }
        let now = self.scheduled_now();
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if chat.snapshot.delegation.is_some() {
            return Ok(None);
        }
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let replaced = tasks
            .tasks()
            .iter()
            .find(|task| matches!(task.kind, ScheduledTaskKind::SelfPaced { .. }))
            .cloned();
        let id = unused_id(&chat.snapshot);
        tasks
            .start_self_paced(
                id.clone(),
                prompt,
                Creation {
                    creator: ScheduledTaskCreator::User,
                    needs_confirmation: false,
                },
                self.inner.schedule_clock.zone(),
                &now,
            )
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        chat.snapshot.scheduled_tasks = tasks.into_tasks();
        self.revoke_scheduled(chat, replaced.as_slice());
        self.persist_scheduled_tasks(chat, Vec::new());
        Ok(Some(id))
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
    /// task due by then, or by now when the host's clock is ahead, runs once. Returns the
    /// receipts the runs were handed over under, or the error of a run the base did not take,
    /// whose task keeps its time so that the next wake-up hands it over again.
    pub async fn wake_scheduled_tasks(
        &self,
        chat_id: ChatId,
        due_at_ms: u64,
    ) -> Result<Vec<Receipt>, AgentError> {
        if !self.inner.chats.lock().await.contains_key(&chat_id) {
            return Err(AgentError::NotFound);
        }
        let due_at = i64::try_from(due_at_ms)
            .ok()
            .and_then(DateTime::from_timestamp_millis)
            .ok_or_else(|| AgentError::Invalid("the wake-up time is out of range".into()))?;
        self.run_due_scheduled_tasks(chat_id, due_at.max(self.scheduled_now()))
            .await
    }

    /// Hands the Loop each task due by `now`, at most once each, while it receives. A task moves
    /// on to its next time only once the base took its run: a run the base refuses leaves the
    /// task as it was, due again.
    pub(crate) async fn run_due_scheduled_tasks(
        &self,
        chat_id: ChatId,
        now: DateTime<Utc>,
    ) -> Result<Vec<Receipt>, AgentError> {
        let runs = {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return Err(AgentError::NotFound);
            };
            if !chat.snapshot.chat_loop.receiving.is_receiving() {
                return Ok(Vec::new());
            }
            let mut prepared = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
            let due = prepared.take_due(&now);
            // A task whose stored schedule no longer reads is dropped without a run.
            let unreadable: Vec<ScheduledTask> = due
                .removed
                .into_iter()
                .filter(|task| !due.fired.iter().any(|fired| fired.task.id == task.id))
                .collect();
            if !unreadable.is_empty() {
                self.delete_scheduled(chat, unreadable, Vec::new());
            }
            due.fired
                .into_iter()
                .filter_map(|fired| {
                    let before = chat
                        .snapshot
                        .scheduled_tasks
                        .iter()
                        .find(|task| task.id == fired.task.id)?
                        .clone();
                    // A run of the task already waits: this one merges into it, so its time is
                    // passed over like one the Loop was too busy for.
                    let merges = chat.snapshot.pending_inputs.iter().any(|pending| {
                        matches!(
                            &pending.trigger_source,
                            Some(TriggerSource::ScheduledTask { task_id, .. }) if *task_id == before.id
                        )
                    });
                    let mut input =
                        run_input(chat, &fired.task, self.inner.config_root.as_deref());
                    if fired.is_last && !matches!(fired.task.kind, ScheduledTaskKind::OneShot { .. })
                    {
                        input.push_str(&format!("\n\n{FINAL_RUN_NOTE}"));
                    }
                    let delivery = TriggerDelivery {
                        source: TriggerSource::ScheduledTask {
                            task_id: before.id.clone(),
                            scheduled_for_ms: fired.scheduled_for_ms,
                        },
                        input,
                        ledger: None,
                    };
                    Some((
                        DueRun {
                            before,
                            fired,
                            merges,
                        },
                        delivery,
                    ))
                })
                .collect::<Vec<_>>()
        };
        let mut taken = Vec::new();
        let mut refusal = None;
        for (run, delivery) in runs {
            match self.deliver_trigger(chat_id, delivery).await {
                Ok(receipt) => taken.push((run, receipt)),
                Err(error) => {
                    tracing::debug!(%chat_id, %error, "a scheduled run was not taken");
                    refusal.get_or_insert(error);
                }
            }
        }
        if !taken.is_empty() {
            let mut chats = self.inner.chats.lock().await;
            if let Some(chat) = chats.get_mut(&chat_id) {
                // The runs' ledger is stored before the tasks' next times: a host that goes down
                // in between finds the same runs due again, and their records answer them.
                self.checkpoint_ledger(&chat.snapshot, &chat.bookkeeping)
                    .await
                    .map_err(AgentError::Storage)?;
                #[cfg(any(test, feature = "test-support"))]
                if self
                    .inner
                    .schedule_clock
                    .stop_before_recording
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Ok(taken.into_iter().map(|(_, receipt)| receipt).collect());
                }
                self.commit_taken_runs(chat, &taken);
            }
        }
        match refusal {
            Some(error) => Err(error),
            None => Ok(taken.into_iter().map(|(_, receipt)| receipt).collect()),
        }
    }

    /// Moves each task whose run the base took on to its next time, unless the task changed
    /// meanwhile, such as by a reschedule or a deletion that withdrew the run.
    fn commit_taken_runs(&self, chat: &mut Chat, taken: &[(DueRun, Receipt)]) {
        let mut notices = Vec::new();
        for (run, _) in taken {
            let Some(index) = chat
                .snapshot
                .scheduled_tasks
                .iter()
                .position(|task| *task == run.before)
            else {
                continue;
            };
            if run.fired.is_last {
                chat.snapshot.scheduled_tasks.remove(index);
            } else {
                let mut after = run.fired.task.clone();
                after.skipped += u64::from(run.merges);
                chat.snapshot.scheduled_tasks[index] = after;
            }
            notices.push(ScheduledTaskNotice::Fired {
                task_id: run.before.id.clone(),
            });
        }
        if !notices.is_empty() {
            self.persist_scheduled_tasks(chat, notices);
        }
    }

    /// A turn ended: when the user's self-paced `/loop` or the self-paced task started it and it
    /// neither rescheduled nor stopped the loop, the host wakes it once more later, and after a
    /// fallback turn that did not reschedule either the loop ends.
    pub(crate) fn end_self_paced_turn(&self, chat: &mut Chat, run: tickloop_agent_protocol::RunId) {
        chat.user_loops.remove(&run);
        let Some(task) = chat
            .snapshot
            .scheduled_tasks
            .iter()
            .find(|task| matches!(task.kind, ScheduledTaskKind::SelfPaced { .. }))
        else {
            return;
        };
        let started_by_users_loop = task.started_by_run == Some(run);
        if task.scheduled_for_ms.is_some()
            || !(started_by_users_loop || started_run(chat, task, run))
        {
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
            let mut retries: std::collections::HashMap<ChatId, u64> = Default::default();
            loop {
                let Some(agent) = agent.upgrade() else {
                    return;
                };
                let now = agent.scheduled_now();
                let now_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
                let (next, due): (Option<u64>, Vec<ChatId>) = {
                    let chats = agent.inner.chats.lock().await;
                    retries.retain(|id, _| chats.contains_key(id));
                    let wakes: Vec<(ChatId, u64)> = chats
                        .iter()
                        .filter_map(|(id, chat)| {
                            let at = Self::next_scheduled_wake(chat)?;
                            Some((*id, at.max(retries.get(id).copied().unwrap_or(0))))
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
                        // A run the base refused, as while another process holds the Chat,
                        // stays due; the timer tries it again a little later.
                        match agent.run_due_scheduled_tasks(chat_id, now).await {
                            Ok(_) => retries.remove(&chat_id),
                            Err(_) => {
                                retries.insert(chat_id, now_ms.saturating_add(RETRY_DELAY_MS))
                            }
                        };
                    }
                    continue;
                }
                let sleep = next
                    .map(|at| Duration::from_millis(at.saturating_sub(now_ms)))
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

/// A fresh task id that neither a task of the Chat nor a cancellation it remembers uses.
fn unused_id(snapshot: &ChatSnapshot) -> String {
    loop {
        let id = tickloop_agent_cron::generate_task_id();
        let taken = snapshot.scheduled_tasks.iter().any(|task| task.id == id)
            || snapshot
                .scheduled_task_revocations
                .iter()
                .any(|revocation| revocation.task_id == id);
        if !taken {
            return id;
        }
    }
}

/// How long a cancellation is remembered: past a recurring task's seven days and the day a run
/// may wait, no run of the task can still come.
const REVOCATION_MEMORY_MS: u64 = 8 * 24 * 60 * 60 * 1000;

/// Records that `cancelled` tasks were cancelled at `now`, and forgets cancellations too old to
/// matter.
fn revoke(
    revocations: &mut Vec<ScheduledTaskRevocation>,
    cancelled: &[ScheduledTask],
    now: DateTime<Utc>,
) {
    let now_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
    revocations.retain(|revocation| {
        revocation
            .revoked_at_ms
            .saturating_add(REVOCATION_MEMORY_MS)
            > now_ms
    });
    revocations.extend(cancelled.iter().map(|task| ScheduledTaskRevocation {
        task_id: task.id.clone(),
        revoked_at_ms: now_ms,
    }));
}

/// An ordinary history load: the self-paced loop, one-shot tasks whose time passed and expired
/// recurring tasks go. The self-paced loop is not restored, so a run of it the ledger restores
/// is cancelled with it; the others' last runs stand.
pub(crate) fn after_history_load(snapshot: &mut ChatSnapshot, now: DateTime<Utc>) {
    let mut kept = ScheduledTasks::new(std::mem::take(&mut snapshot.scheduled_tasks));
    let removed = kept.after_history_load(&now);
    snapshot.scheduled_tasks = kept.into_tasks();
    let self_paced: Vec<ScheduledTask> = removed
        .into_iter()
        .filter(|task| matches!(task.kind, ScheduledTaskKind::SelfPaced { .. }))
        .collect();
    revoke(&mut snapshot.scheduled_task_revocations, &self_paced, now);
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
