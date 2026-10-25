//! Scheduled tasks in the Agent: the tools, the timer of a Worker, the wake-up a stateless
//! host gives, and the Loop's turns, Esc, `/stop` and continuing the Chat.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};
use tickloop_harness_protocol::{ScheduledTaskId, ScheduledTaskIdKind};
use tickloop_ids::{IdContext, ModelRef};

use chrono::{DateTime, Utc};
use chrono_tz::Tz as Zone;
use serde_json::{Value, json};
use tickloop_harness_cron::Schedule;
use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, EventKind, OperationRecord, OperationState, PromptPolicy, Receipt,
    ScheduledInputWrapper, ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskCreator,
    ScheduledTaskDecision, ScheduledTaskKind, ScheduledTaskNotice, ScheduledTaskRemovalReason,
    ScheduledTaskRevocation, ToolResult, TriggerSource, ending_scheduled_run_prompt,
    final_scheduled_run_prompt, loop_instructions_error_prompt, scheduled_skill_prompt,
};
use tickloop_harness_transcript::{DiscardReason, Line, ScheduledTasksEntry};
use uuid::Uuid;

use super::tasks::{
    Creation, CronTask, DecisionOutcome, FiredTask, NewCronTask, RemovedTask, ScheduledTaskError,
    ScheduledTasks, SelfPacedTurnEnd, TASK_LIFETIME, TaskEdit, TaskRules, TimingEdit, Wakeup,
    title,
};
use super::tools::{self, ScheduledTaskCall, ToolTerms};
use crate::wakeup::{SourceChange, SourceChangeError, TriggerDelivery};
use crate::{Agent, AgentError, Chat};
use tickloop_harness_policy::loop_skill::{
    DEFAULT_INSTRUCTIONS_PROMPT, LoopInstructionSource, LoopSchedule, MaintenanceInstructions,
};

/// The longest a Worker's timer sleeps before it looks at the clock again, so a machine that
/// slept or a wall clock that moved does not delay a run for long.
const TIMER_RECHECK: Duration = Duration::from_secs(60);
/// Why a turn a scheduled run started cannot start a self-paced loop.
const NO_NEW_LOOP_FROM_A_RUN: &str = "No self-paced loop is running, and a turn a scheduled task started cannot start one: the loop ended. Tell the user; they can start a new /loop.";
/// How long a Worker's timer waits before it hands a refused run over again.
const RETRY_DELAY_MS: u64 = 10_000;

/// The time zone new scheduled tasks read cron expressions in, and the clock tasks run by.
pub(crate) struct ScheduleClock {
    zone: std::sync::RwLock<Zone>,
    #[cfg(any(test, feature = "test-support"))]
    fixed_now: std::sync::Mutex<Option<DateTime<Utc>>>,
}

impl ScheduleClock {
    pub(crate) fn new(zone: Zone) -> Self {
        Self {
            zone: std::sync::RwLock::new(zone),
            #[cfg(any(test, feature = "test-support"))]
            fixed_now: std::sync::Mutex::new(None),
        }
    }

    pub(crate) fn zone(&self) -> Zone {
        *self.zone.read().unwrap()
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(now) = *self.fixed_now.lock().unwrap() {
            return now;
        }
        DateTime::from(SystemTime::now())
    }
}

/// A task the host creates for the user, such as from a scheduling dialog: it needs no
/// confirmation and starts timing at once.
#[derive(Clone, Debug)]
pub struct NewScheduledTask {
    /// A 5-field cron expression read in `time_zone`.
    pub cron: String,
    pub prompt: String,
    /// `false` runs once, at the first time `cron` names.
    pub recurring: bool,
    /// The task's id. Creating a task with the id of one the Chat has returns that task
    /// unchanged, so a retried request creates no second task; the id of a task the Chat
    /// deleted is refused. A task that ended by itself, such as a one-shot task that ran, is no
    /// longer known, so its id creates a new task. `None` generates a UUIDv7.
    pub uuid: Option<Uuid>,
    pub title: Option<String>,
    /// An IANA zone; `None` uses the Harness's.
    pub time_zone: Option<String>,
    /// When the task ends: no run fires at or after it, and the task is deleted then.
    pub ends_at_ms: Option<u64>,
}

impl NewScheduledTask {
    /// A task on `cron` in the Harness's zone, with a generated id and neither title nor end.
    pub fn new(cron: impl Into<String>, prompt: impl Into<String>, recurring: bool) -> Self {
        Self {
            cron: cron.into(),
            prompt: prompt.into(),
            recurring,
            uuid: None,
            title: None,
            time_zone: None,
            ends_at_ms: None,
        }
    }
}

/// What a CronUpdate call changes; `None` leaves that part as it is.
struct TaskUpdate {
    cron: Option<String>,
    prompt: Option<String>,
    recurring: Option<bool>,
    title: Option<String>,
}

/// A change the host makes to a task; `None` leaves that part as it is. A new schedule,
/// recurrence, zone or end times the next run from now.
#[derive(Clone, Debug, Default)]
pub struct ScheduledTaskEdit {
    pub prompt: Option<String>,
    pub cron: Option<String>,
    pub recurring: Option<bool>,
    /// `Some(None)` removes the title.
    pub title: Option<Option<String>>,
    pub time_zone: Option<String>,
    /// `Some(None)` lets the task run without an end.
    pub ends_at_ms: Option<Option<u64>>,
}

impl From<ScheduledTaskError> for AgentError {
    fn from(error: ScheduledTaskError) -> Self {
        match error {
            ScheduledTaskError::NotFound(_) => Self::ScheduledTaskNotFound,
            ScheduledTaskError::TooFrequent { min_interval, .. } => {
                Self::ScheduledTaskTooFrequent { min_interval }
            }
            error => Self::Invalid(error.to_string()),
        }
    }
}

/// The notice of each removed task, saying why it left.
fn removal_notices(removed: &[RemovedTask]) -> Vec<ScheduledTaskNotice> {
    removed
        .iter()
        .map(|removed| ScheduledTaskNotice::Removed {
            task_id: removed.task.id,
            reason: removed.reason,
        })
        .collect()
}

fn removed(task: ScheduledTask, reason: ScheduledTaskRemovalReason) -> RemovedTask {
    RemovedTask { task, reason }
}

/// Why a recurring or self-paced task's final run is its last, for the note the run carries.
/// A task living the standard seven days keeps the note it always had.
fn final_run_input(
    input: &str,
    task: &ScheduledTask,
    reason: ScheduledTaskRemovalReason,
) -> String {
    let lifetime = task
        .expires_at_ms
        .map(|expires_at| Duration::from_millis(expires_at.saturating_sub(task.created_at_ms)));
    match (reason, lifetime) {
        (ScheduledTaskRemovalReason::Expired, Some(TASK_LIFETIME)) => {
            final_scheduled_run_prompt(input)
        }
        (ScheduledTaskRemovalReason::Expired, Some(_)) => {
            ending_scheduled_run_prompt(input, "it reached the end of its lifetime")
        }
        (ScheduledTaskRemovalReason::EndsAtReached, _) => {
            ending_scheduled_run_prompt(input, "it reached the end set for it")
        }
        _ => ending_scheduled_run_prompt(input, "its schedule names no later time"),
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
fn run_input(
    chat: &Chat,
    task: &ScheduledTask,
    settings: Option<&dyn tickloop_harness_policy::settings::SettingsStore>,
) -> (String, Vec<ScheduledInputWrapper>) {
    let prompt = task.prompt.trim();
    if prompt == DEFAULT_INSTRUCTIONS_PROMPT {
        return match settings.map_or_else(
            || MaintenanceInstructions.instructions(),
            |settings| settings.loop_instructions(chat.snapshot.folder.as_deref()),
        ) {
            Ok(instructions) => (
                instructions.prompt(),
                vec![ScheduledInputWrapper::LoopInstructions {
                    origin: instructions.origin(),
                    instructions: instructions.text,
                }],
            ),
            Err(error) => {
                let task_text = chat.ids.model_text(ScheduledTaskId::from(task.id));
                (
                    loop_instructions_error_prompt(&task_text, &error.to_string()),
                    vec![ScheduledInputWrapper::LoopInstructionsError {
                        task_text: Some(task_text),
                    }],
                )
            }
        };
    }
    if let Some(command) = prompt.strip_prefix('/') {
        let (name, arguments) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, arguments)| (name, arguments.trim()));
        if chat.skills.contains(name) && !chat.skills.is_user_only(name) {
            return (
                scheduled_skill_prompt(
                    &chat.ids.model_text(ScheduledTaskId::from(task.id)),
                    name,
                    arguments,
                ),
                vec![ScheduledInputWrapper::Skill {
                    instruction: task.prompt.clone(),
                }],
            );
        }
    }
    (task.prompt.clone(), Vec::new())
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

fn describe(ids: &IdContext, task: &ScheduledTask) -> Value {
    let (schedule, recurring) = match &task.kind {
        ScheduledTaskKind::Recurring { cron } => (cron.as_str(), true),
        ScheduledTaskKind::OneShot { cron } => (cron.as_str(), false),
        ScheduledTaskKind::SelfPaced { .. } => ("self-paced", true),
    };
    json!({
        "id": ids.model_text(ScheduledTaskId::from(task.id)),
        "title": task.title,
        "schedule": schedule,
        "recurring": recurring,
        "time_zone": task.time_zone,
        "next_run": local_time(task.next_fire_at_ms, &task.time_zone),
        "expires": local_time(task.expires_at_ms, &task.time_zone),
        "ends": local_time(task.ends_at_ms, &task.time_zone),
        "awaiting_confirmation": task.confirmation == Some(ScheduledTaskConfirmation::Pending),
        "paused": task.paused_at_ms.is_some(),
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
fn started_by_scheduled_run(chat: &Chat, run: tickloop_harness_protocol::RunId) -> bool {
    chat.bookkeeping
        .lifecycle
        .open_input(run)
        .and_then(|input| chat.bookkeeping.lifecycle.input(input))
        .and_then(|input| input.trigger.as_ref())
        .is_some_and(|trigger| {
            matches!(
                trigger.identity,
                tickloop_harness_protocol::TriggerIdentity::ScheduledTask { .. }
            )
        })
}

/// The scheduled tasks and their revocations as the Chat's Transcript keeps them. A process
/// taking the Chat over goes on from what the previous holder recorded, so a task deleted there
/// is neither run nor written back here.
pub(crate) struct ScheduledTasksState;

impl crate::wakeup::TriggerSourceState for ScheduledTasksState {
    fn take_over(&self, change: &SourceChange, chat: &mut Chat, stored: &[Line]) {
        let Some(recorded) = stored.iter().rev().find_map(|line| match line {
            Line::ScheduledTasks(entry) if entry.chat_id == chat.snapshot.id => Some(entry),
            _ => None,
        }) else {
            return;
        };
        if chat.snapshot.scheduled_tasks == recorded.tasks
            && chat.snapshot.scheduled_task_revocations == recorded.revoked
        {
            return;
        }
        chat.snapshot.scheduled_tasks = recorded.tasks.clone();
        chat.snapshot.scheduled_task_revocations = recorded.revoked.clone();
        change.agent().announce_scheduled_tasks(chat, Vec::new());
    }

    fn records(&self, chat: &Chat) -> Vec<Line> {
        let snapshot = &chat.snapshot;
        if snapshot.scheduled_tasks.is_empty() && snapshot.scheduled_task_revocations.is_empty() {
            return Vec::new();
        }
        vec![scheduled_tasks_record(chat)]
    }

    /// `/stop` cancels the self-paced loop's pending wake-up with the pause.
    fn on_stop(&self, change: &SourceChange, chat: &mut Chat) {
        change.agent().cancel_self_paced_wakeup(change, chat);
    }

    /// Continuing re-times the tasks from now, so nothing missed while paused runs late.
    fn on_resume(&self, change: &SourceChange, chat: &mut Chat) {
        change.agent().resume_scheduled_tasks(change, chat);
    }
}

fn scheduled_tasks_record(chat: &Chat) -> Line {
    Line::ScheduledTasks(ScheduledTasksEntry {
        chat_id: chat.snapshot.id.into(),
        cwd: chat.snapshot.folder.clone(),
        tasks: chat.snapshot.scheduled_tasks.clone(),
        revoked: chat.snapshot.scheduled_task_revocations.clone(),
    })
}

impl From<SourceChangeError> for AgentError {
    fn from(error: SourceChangeError) -> Self {
        match error {
            SourceChangeError::NotLoaded => Self::NotFound,
            SourceChangeError::Busy => Self::Busy,
            SourceChangeError::Unrecorded(message) => Self::Storage(message),
            error @ (SourceChangeError::HeldElsewhere
            | SourceChangeError::ForeignHost(_)
            | SourceChangeError::Closing) => Self::Invalid(error.to_string()),
        }
    }
}

/// A due run of a task, prepared for the base: the task before it ran and as it is after.
struct DueRun {
    before: ScheduledTask,
    fired: FiredTask,
}

/// The scheduled task that started the run `run`: the latest run of the self-paced task is
/// named by its delivery's ledger input.
fn started_run(chat: &Chat, task: &ScheduledTask, run: tickloop_harness_protocol::RunId) -> bool {
    task.last_scheduled_for_ms.is_some_and(|scheduled_for_ms| {
        let input = tickloop_harness_transcript::input_ids::trigger(
            chat.snapshot.id.into(),
            &tickloop_harness_protocol::TriggerIdentity::ScheduledTask {
                task_id: task.id,
                scheduled_for_ms,
                wrappers: Vec::new(),
            },
        );
        chat.bookkeeping
            .lifecycle
            .input(input.into())
            .is_some_and(|input| input.run_id == run)
    })
}

impl Agent {
    /// Whether a run of `task_id` for `scheduled_for_ms` still stands: its task was not
    /// cancelled, and it is not older than [`MAX_RUN_AGE_MS`]. A one-shot or final run outlives
    /// its task.
    pub(crate) fn scheduled_run_eligibility(
        &self,
        snapshot: &ChatSnapshot,
        task_id: &Uuid,
        scheduled_for_ms: u64,
    ) -> crate::wakeup::Eligibility {
        let is_revoked = snapshot
            .scheduled_task_revocations
            .iter()
            .any(|revocation| revocation.task_id == *task_id);
        let now_ms = u64::try_from(self.scheduled_now().timestamp_millis()).unwrap_or(0);
        let is_void = scheduled_for_ms.saturating_add(MAX_RUN_AGE_MS) < now_ms;
        let is_paused = snapshot
            .scheduled_tasks
            .iter()
            .any(|task| task.id == *task_id && task.paused_at_ms.is_some());
        if is_revoked || is_void || is_paused {
            crate::wakeup::Eligibility::Ended(DiscardReason::SourceEnded)
        } else {
            crate::wakeup::Eligibility::Eligible
        }
    }

    pub(crate) fn scheduled_now(&self) -> DateTime<Utc> {
        self.inner.schedule_clock.now()
    }

    /// Fixes the clock scheduled tasks run by.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn set_scheduled_now_for_tests(&self, now: DateTime<Utc>) {
        *self.inner.schedule_clock.fixed_now.lock().unwrap() = Some(now);
    }

    /// Stands in a task for a scheduled run a test hands over by itself: unless the Chat has
    /// the task or cancelled it, a recurring task with that id is due at the run's time and
    /// does not time on its own, and the clock is pinned there unless a test pinned it.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn stand_in_scheduled_task(&self, chat_id: ChatId, source: &TriggerSource) {
        let TriggerSource::ScheduledTask {
            task_id,
            scheduled_for_ms,
            ..
        } = source
        else {
            return;
        };
        let scheduled_for_ms = *scheduled_for_ms;
        {
            let mut fixed = self.inner.schedule_clock.fixed_now.lock().unwrap();
            if fixed.is_none() {
                *fixed = i64::try_from(scheduled_for_ms)
                    .ok()
                    .and_then(DateTime::from_timestamp_millis);
            }
        }
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        let snapshot = &mut chat.snapshot;
        let known = snapshot
            .scheduled_tasks
            .iter()
            .any(|task| task.id == *task_id)
            || snapshot
                .scheduled_task_revocations
                .iter()
                .any(|revocation| revocation.task_id == *task_id);
        if !known {
            snapshot.scheduled_tasks.push(ScheduledTask {
                paused_at_ms: None,
                id: *task_id,
                kind: ScheduledTaskKind::Recurring {
                    cron: "* * * * *".into(),
                },
                title: None,
                prompt: String::new(),
                creator: ScheduledTaskCreator::User,
                time_zone: "UTC".into(),
                confirmation: None,
                created_at_ms: scheduled_for_ms,
                expires_at_ms: None,
                ends_at_ms: None,
                scheduled_for_ms: Some(scheduled_for_ms),
                next_fire_at_ms: None,
                jitter_ms: 0,
                last_fired_at_ms: None,
                last_scheduled_for_ms: None,
                started_by_run: None,
                skipped: 0,
            });
            // Recorded, so a process taking the Chat over finds the task its run belongs to;
            // the timer is left alone, as the task does not time on its own.
            self.queue_with_source_states(chat, Vec::new(), vec![scheduled_tasks_record(chat)]);
        }
    }

    /// Records the Chat's scheduled tasks after a change made under its lease, announces it,
    /// and has the timer look at them again.
    pub(crate) fn persist_scheduled_tasks(
        &self,
        change: &SourceChange,
        chat: &mut Chat,
        notices: Vec<ScheduledTaskNotice>,
    ) {
        change.record_source_state(scheduled_tasks_record(chat));
        self.announce_scheduled_tasks(chat, notices);
    }

    /// Announces the Chat's scheduled tasks as they are now, and has the timer look at them
    /// again.
    fn announce_scheduled_tasks(&self, chat: &mut Chat, notices: Vec<ScheduledTaskNotice>) {
        self.save(&chat.snapshot);
        let receipt = Receipt {
            chat_id: chat.snapshot.id,
            loop_id: chat.snapshot.chat_loop.id,
            run_id: chat
                .snapshot
                .chat_loop
                .run_id
                .unwrap_or(tickloop_harness_protocol::RunId::nil()),
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
                        if deleted.iter().any(|task| task.id == *task_id)
                )
            })
            .map(|pending| pending.receipt.clone())
            .collect();
        for receipt in waiting {
            self.withdraw_trigger_input(chat, &receipt, DiscardReason::SourceEnded);
        }
    }

    /// Deletes tasks from the Chat, with their queued runs, announcing why each left after
    /// `notices`.
    fn delete_scheduled(
        &self,
        change: &SourceChange,
        chat: &mut Chat,
        deleted: Vec<RemovedTask>,
        mut notices: Vec<ScheduledTaskNotice>,
    ) {
        if deleted.is_empty() {
            return;
        }
        notices.extend(removal_notices(&deleted));
        let deleted: Vec<ScheduledTask> = deleted.into_iter().map(|removed| removed.task).collect();
        chat.snapshot
            .scheduled_tasks
            .retain(|task| !deleted.iter().any(|gone| gone.id == task.id));
        self.revoke_scheduled(chat, &deleted);
        self.persist_scheduled_tasks(change, chat, notices);
    }

    /// The host's rules for its scheduled tasks.
    pub(crate) fn scheduled_task_rules(&self) -> TaskRules {
        self.inner.scheduled_task_rules
    }

    /// What the scheduled task tools offer and promise the model on this host.
    pub(crate) fn scheduled_task_terms(&self) -> ToolTerms {
        ToolTerms {
            rules: self.inner.scheduled_task_rules,
            confirms_agent_tasks: !self.inner.trust_model_scheduled_tasks,
            zone: self.inner.schedule_clock.zone(),
        }
    }

    /// The IANA time zone the scheduled tasks created from now on read their cron expressions
    /// in, such as after the user changed theirs: CronCreate, ScheduleWakeup and a host-created
    /// task without its own zone use it, even later in a turn already running. Tasks that exist
    /// keep their zone and next run. The scheduled task tools name the zone to the model in each
    /// request, so the host needs no other change to tell it. An unknown name is refused and
    /// the zone stays.
    pub fn set_time_zone(&self, zone: &str) -> Result<(), AgentError> {
        let zone = resolve_zone(Some(zone)).map_err(AgentError::Invalid)?;
        *self.inner.schedule_clock.zone.write().unwrap() = zone;
        Ok(())
    }

    /// Who creates a task in the turn `receipt`: the user only for the one task their own
    /// `/loop` of that turn schedules, exactly as the host read it; the Agent for every other.
    /// Only the Agent's need confirming, and only where nobody is asked and the host does not
    /// trust the Agent's tasks.
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
                && self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt
                && !self.inner.trust_model_scheduled_tasks,
        }
    }

    /// One call of the scheduled task tools. A change is made under the Chat's lease and
    /// recorded before the call returns; another process holding the lease refuses it.
    pub(crate) async fn scheduled_task_tool(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
    ) -> Result<ToolResult, String> {
        let call = match tools::parse(&operation.name, &operation.input) {
            Ok(call) => call,
            Err(message) => return Ok(tool_error(message)),
        };
        if self
            .inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .ok_or_else(|| "the Chat is gone".to_owned())?
            .snapshot
            .delegation
            .is_some()
        {
            return Ok(tool_error(
                "Only the root Agent schedules tasks; a Subagent has none.".into(),
            ));
        }
        let now = self.scheduled_now();
        let zone = self.inner.schedule_clock.zone();
        let call_receipt = receipt.clone();
        self.run_source_change(receipt.chat_id, move |change, chat| {
            change
                .agent()
                .apply_scheduled_task_call(change, chat, &call_receipt, call, zone, now)
        })
        .await
        .or_else(|error| {
            Ok(tool_error(format!(
                "The scheduled tasks were not changed: {error}."
            )))
        })
    }

    fn apply_scheduled_task_call(
        &self,
        change: &SourceChange,
        chat: &mut Chat,
        receipt: &Receipt,
        call: ScheduledTaskCall,
        zone: Zone,
        now: DateTime<Utc>,
    ) -> ToolResult {
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let rules = self.scheduled_task_rules();
        let id = unused_id(&chat.snapshot);
        let outcome = match call {
            ScheduledTaskCall::Create {
                cron,
                prompt,
                recurring,
                title,
            } => match Schedule::parse(&cron) {
                Err(error) => Err(error.to_string()),
                Ok(schedule) => {
                    let users_loop = recurring.then(|| LoopSchedule::Cron {
                        cron: schedule.expression().to_owned(),
                        prompt: prompt.clone(),
                    });
                    let creation = self.creation(chat, receipt, users_loop.as_ref());
                    let new = NewCronTask {
                        id,
                        cron_task: CronTask::new(schedule, recurring),
                        prompt,
                        title,
                        creation,
                        zone,
                        ends_at_ms: None,
                    };
                    tasks
                        .create_cron(new, rules, &now)
                        .map(|task| (describe(&chat.ids, task), Vec::new(), Vec::new()))
                        .map_err(|error| error.to_string())
                }
            },
            ScheduledTaskCall::List => {
                let listed: Vec<Value> = tasks
                    .tasks()
                    .iter()
                    .map(|task| describe(&chat.ids, task))
                    .collect();
                return tool_result(json!({ "tasks": listed }));
            }
            ScheduledTaskCall::Update {
                id,
                cron,
                prompt,
                recurring,
                title,
            } => self
                .update_from_tool(
                    chat,
                    receipt,
                    &mut tasks,
                    id,
                    TaskUpdate {
                        cron,
                        prompt,
                        recurring,
                        title,
                    },
                    zone,
                    now,
                )
                .map(|task| (describe(&chat.ids, &task), Vec::new(), Vec::new())),
            ScheduledTaskCall::Delete { id: reference } => match chat
                .ids
                .resolve(&reference)
                .ok()
                .and_then(|id| tasks.delete(&id.uuid()))
            {
                Some(task) => Ok((
                    json!({ "deleted": chat.ids.model_text(ScheduledTaskId::from(task.id)) }),
                    vec![removed(task, ScheduledTaskRemovalReason::AgentDeleted)],
                    Vec::new(),
                )),
                None => Err(tickloop_ids::not_found::<ScheduledTaskIdKind>(
                    reference.as_str(),
                )),
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
                    rules,
                    &now,
                )
                .map(|scheduled| {
                    let mut value = describe(&chat.ids, scheduled.task);
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
                    let notice = ScheduledTaskNotice::SelfPacedEnded { task_id: task.id };
                    Ok((
                        json!({ "stopped": chat.ids.model_text(ScheduledTaskId::from(task.id)) }),
                        vec![removed(task, ScheduledTaskRemovalReason::SelfPacedEnded)],
                        vec![notice],
                    ))
                }
                None => Ok((
                    json!({ "stopped": null, "note": "No self-paced loop is running." }),
                    Vec::new(),
                    Vec::new(),
                )),
            },
        };
        let (value, deleted, mut notices): (Value, Vec<RemovedTask>, _) = match outcome {
            Ok(outcome) => outcome,
            Err(message) => return tool_error(message),
        };
        chat.snapshot.scheduled_tasks = tasks.into_tasks();
        notices.extend(removal_notices(&deleted));
        let deleted: Vec<ScheduledTask> = deleted.into_iter().map(|removed| removed.task).collect();
        self.revoke_scheduled(chat, &deleted);
        self.persist_scheduled_tasks(change, chat, notices);
        tool_result(value)
    }

    /// CronUpdate: changes the Chat's task `id` in place, as the host's edit does, with a new
    /// schedule read in the Harness's current zone. Where nobody is asked and the host does not
    /// trust the Agent's tasks, the changed task waits for the user to confirm it again. A new
    /// schedule, or waiting for confirmation, withdraws the run the task had queued.
    #[allow(clippy::too_many_arguments)]
    fn update_from_tool(
        &self,
        chat: &mut Chat,
        receipt: &Receipt,
        tasks: &mut ScheduledTasks,
        reference: ModelRef<ScheduledTaskIdKind>,
        update: TaskUpdate,
        zone: Zone,
        now: DateTime<Utc>,
    ) -> Result<ScheduledTask, String> {
        let not_found = || tickloop_ids::not_found::<ScheduledTaskIdKind>(reference.as_str());
        let id = chat
            .ids
            .resolve(&reference)
            .map_err(|_| not_found())?
            .uuid();
        let before = tasks.get(&id).cloned().ok_or_else(not_found)?;
        let current_cron = match &before.kind {
            ScheduledTaskKind::Recurring { cron } | ScheduledTaskKind::OneShot { cron } => cron,
            ScheduledTaskKind::SelfPaced { .. } => {
                return Err(chat.ids.state_error(
                    ScheduledTaskId::from(id),
                    &format!(
                        "is the self-paced loop; reschedule it with {}",
                        tools::SCHEDULE_WAKEUP_TOOL_NAME
                    ),
                ));
            }
        };
        let timing = (update.cron.is_some() || update.recurring.is_some())
            .then(|| update.cron.clone().unwrap_or_else(|| current_cron.clone()))
            .map(|cron| {
                let schedule = Schedule::parse(&cron).map_err(|error| error.to_string())?;
                let recurring = update
                    .recurring
                    .unwrap_or(matches!(before.kind, ScheduledTaskKind::Recurring { .. }));
                Ok::<_, String>(TimingEdit {
                    cron_task: CronTask::new(schedule, recurring),
                    zone,
                    ends_at_ms: before.ends_at_ms,
                })
            })
            .transpose()?;
        let changes_timing = timing.is_some();
        let reconfirm = self.creation(chat, receipt, None).needs_confirmation;
        let edited = tasks
            .edit(
                &id,
                TaskEdit {
                    prompt: update.prompt,
                    title: update.title.map(Some),
                    timing,
                    reconfirm,
                },
                self.scheduled_task_rules(),
                &now,
            )
            .map_err(|error| error.model_message(&chat.ids, Some(reference.as_str())))?
            .clone();
        if changes_timing || reconfirm {
            self.withdraw_deleted_task_runs(chat, std::slice::from_ref(&before));
        }
        Ok(edited)
    }

    /// The user schedules a task through the host's interface, as with `/loop`: it needs no
    /// confirmation and starts timing at once, in a Chat with or without messages. A task
    /// with the Chat's id of one it has already is returned as it is, so retrying a request
    /// creates no second task; the id of a task the Chat deleted is refused. A recurring task
    /// must keep to the host's least interval, refused with
    /// [`AgentError::ScheduledTaskTooFrequent`].
    pub async fn schedule_task(
        &self,
        chat_id: ChatId,
        task: NewScheduledTask,
    ) -> Result<ScheduledTask, AgentError> {
        if task.prompt.trim().is_empty() {
            return Err(AgentError::Invalid(
                "a scheduled task needs a prompt to run".into(),
            ));
        }
        let schedule =
            Schedule::parse(&task.cron).map_err(|error| AgentError::Invalid(error.to_string()))?;
        let zone = match &task.time_zone {
            Some(name) => resolve_zone(Some(name)).map_err(AgentError::Invalid)?,
            None => self.inner.schedule_clock.zone(),
        };
        let task_title = task.title.as_deref().map(title).transpose()?;
        let now = self.scheduled_now();
        let rules = self.scheduled_task_rules();
        self.run_source_change(chat_id, move |change, chat| {
            if chat.snapshot.delegation.is_some() {
                return Err(AgentError::Invalid(
                    "only the root Agent has scheduled tasks".into(),
                ));
            }
            if let Some(id) = task.uuid {
                if let Some(existing) = chat
                    .snapshot
                    .scheduled_tasks
                    .iter()
                    .find(|existing| existing.id == id)
                {
                    return Ok(existing.clone());
                }
                if chat
                    .snapshot
                    .scheduled_task_revocations
                    .iter()
                    .any(|revocation| revocation.task_id == id)
                {
                    return Err(AgentError::Invalid(format!(
                        "scheduled task `{id}` was deleted; create it with another id"
                    )));
                }
            }
            let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
            let new = NewCronTask {
                id: task.uuid.unwrap_or_else(|| unused_id(&chat.snapshot)),
                cron_task: CronTask::new(schedule, task.recurring),
                prompt: task.prompt,
                title: task_title,
                creation: Creation {
                    creator: ScheduledTaskCreator::User,
                    needs_confirmation: false,
                },
                zone,
                ends_at_ms: task.ends_at_ms,
            };
            let created = tasks.create_cron(new, rules, &now)?.clone();
            chat.snapshot.scheduled_tasks = tasks.into_tasks();
            change
                .agent()
                .persist_scheduled_tasks(change, chat, Vec::new());
            Ok(created)
        })
        .await?
    }

    /// The user changes a task through the host's interface. Its id stays; a new schedule,
    /// recurrence, zone or end times its next run from now and withdraws the run it had
    /// queued for the old time. A recurring schedule must keep to the host's least interval,
    /// and a task waiting for confirmation is confirmed or rejected instead.
    pub async fn edit_scheduled_task(
        &self,
        chat_id: ChatId,
        task_id: Uuid,
        edit: ScheduledTaskEdit,
    ) -> Result<ScheduledTask, AgentError> {
        if edit
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.trim().is_empty())
        {
            return Err(AgentError::Invalid(
                "a scheduled task needs a prompt to run".into(),
            ));
        }
        let schedule = edit
            .cron
            .as_deref()
            .map(Schedule::parse)
            .transpose()
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let zone = edit
            .time_zone
            .as_deref()
            .map(|name| resolve_zone(Some(name)))
            .transpose()
            .map_err(AgentError::Invalid)?;
        let new_title = edit
            .title
            .map(|text| text.as_deref().map(title).transpose())
            .transpose()?;
        let changes_timing = schedule.is_some()
            || edit.recurring.is_some()
            || zone.is_some()
            || edit.ends_at_ms.is_some();
        let now = self.scheduled_now();
        let rules = self.scheduled_task_rules();
        self.run_source_change(chat_id, move |change, chat| {
            let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
            let before = tasks
                .get(&task_id)
                .cloned()
                .ok_or(AgentError::ScheduledTaskNotFound)?;
            let timing = match (changes_timing, &before.kind) {
                (false, _) => None,
                (true, ScheduledTaskKind::SelfPaced { .. }) => {
                    return Err(ScheduledTaskError::SelfPaced(task_id).into());
                }
                (
                    true,
                    ScheduledTaskKind::Recurring { cron } | ScheduledTaskKind::OneShot { cron },
                ) => {
                    let schedule = match schedule {
                        Some(schedule) => schedule,
                        None => Schedule::parse(cron)
                            .map_err(|error| AgentError::Invalid(error.to_string()))?,
                    };
                    let recurring = edit
                        .recurring
                        .unwrap_or(matches!(before.kind, ScheduledTaskKind::Recurring { .. }));
                    let zone = match zone {
                        Some(zone) => zone,
                        None => {
                            resolve_zone(Some(&before.time_zone)).map_err(AgentError::Invalid)?
                        }
                    };
                    Some(TimingEdit {
                        cron_task: CronTask::new(schedule, recurring),
                        zone,
                        ends_at_ms: edit.ends_at_ms.unwrap_or(before.ends_at_ms),
                    })
                }
            };
            let edited = tasks
                .edit(
                    &task_id,
                    TaskEdit {
                        prompt: edit.prompt,
                        title: new_title,
                        timing,
                        reconfirm: false,
                    },
                    rules,
                    &now,
                )?
                .clone();
            chat.snapshot.scheduled_tasks = tasks.into_tasks();
            let agent = change.agent();
            if changes_timing {
                agent.withdraw_deleted_task_runs(chat, std::slice::from_ref(&before));
            }
            agent.persist_scheduled_tasks(change, chat, Vec::new());
            Ok(edited)
        })
        .await?
    }

    /// The user's `/loop` without an interval starts the self-paced loop with its turn `run`,
    /// so that turn gets the fallback wake-up too; a self-paced loop running already is
    /// replaced. The task is recorded under the Chat's lease, naming `run`, before the turn is
    /// submitted.
    pub(crate) async fn start_users_self_paced_loop(
        &self,
        chat_id: ChatId,
        prompt: String,
        run: tickloop_harness_protocol::RunId,
    ) -> Result<Option<Uuid>, AgentError> {
        if !self.inner.tool_policy.scheduled_tasks || !self.inner.tool_policy.schedule_wakeup {
            return Ok(None);
        }
        let now = self.scheduled_now();
        let zone = self.inner.schedule_clock.zone();
        let rules = self.scheduled_task_rules();
        self.run_source_change(chat_id, move |change, chat| {
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
            tasks.start_self_paced(
                id,
                prompt,
                Creation {
                    creator: ScheduledTaskCreator::User,
                    needs_confirmation: false,
                },
                zone,
                rules,
                &now,
            )?;
            chat.snapshot.scheduled_tasks = tasks.into_tasks();
            if let Some(task) = chat
                .snapshot
                .scheduled_tasks
                .iter_mut()
                .find(|task| task.id == id)
            {
                task.started_by_run = Some(run.into());
            }
            let agent = change.agent();
            let notices = replaced
                .iter()
                .map(|task| ScheduledTaskNotice::Removed {
                    task_id: task.id,
                    reason: ScheduledTaskRemovalReason::SelfPacedEnded,
                })
                .collect();
            agent.revoke_scheduled(chat, replaced.as_slice());
            agent.persist_scheduled_tasks(change, chat, notices);
            Ok(Some(id))
        })
        .await?
    }

    /// Pauses or resumes a task at the user's request: a paused task neither runs nor counts
    /// down, and resuming times its next run from now. A task waiting for confirmation is
    /// confirmed or rejected instead.
    pub async fn set_scheduled_task_paused(
        &self,
        chat_id: ChatId,
        task_id: Uuid,
        paused: bool,
    ) -> Result<(), AgentError> {
        let now = self.scheduled_now();
        self.run_source_change(chat_id, move |change, chat| {
            let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
            tasks.set_paused(&task_id, paused, &now)?;
            chat.snapshot.scheduled_tasks = tasks.into_tasks();
            change
                .agent()
                .persist_scheduled_tasks(change, chat, Vec::new());
            Ok::<(), AgentError>(())
        })
        .await??;
        self.withdraw_every_ended_input().await;
        Ok(())
    }

    /// Cancels one of the Chat's scheduled tasks at the user's request.
    pub async fn delete_scheduled_task(
        &self,
        chat_id: ChatId,
        task_id: Uuid,
    ) -> Result<(), AgentError> {
        self.run_source_change(chat_id, move |change, chat| {
            let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
            let deleted = tasks
                .delete(&task_id)
                .ok_or(AgentError::ScheduledTaskNotFound)?;
            change.agent().delete_scheduled(
                change,
                chat,
                vec![removed(deleted, ScheduledTaskRemovalReason::UserDeleted)],
                Vec::new(),
            );
            Ok(())
        })
        .await?
    }

    /// The user's answer to a task the Agent created on a host without permission requests:
    /// confirming starts its timing now, unless its time already passed, when it is deleted
    /// without running; rejecting deletes it.
    pub async fn decide_scheduled_task(
        &self,
        chat_id: ChatId,
        task_id: Uuid,
        decision: ScheduledTaskDecision,
    ) -> Result<(), AgentError> {
        let now = self.scheduled_now();
        self.run_source_change(chat_id, move |change, chat| {
            let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
            let notices = match tasks.decide(&task_id, decision, &now)? {
                DecisionOutcome::Confirmed(_) => Vec::new(),
                DecisionOutcome::Rejected(task) => vec![ScheduledTaskNotice::Removed {
                    task_id: task.id,
                    reason: ScheduledTaskRemovalReason::Rejected,
                }],
                DecisionOutcome::ExpiredUnrun(removed) => vec![
                    ScheduledTaskNotice::ExpiredUnrun {
                        task_id: removed.task.id,
                    },
                    ScheduledTaskNotice::Removed {
                        task_id: removed.task.id,
                        reason: removed.reason,
                    },
                ],
            };
            chat.snapshot.scheduled_tasks = tasks.into_tasks();
            change
                .agent()
                .persist_scheduled_tasks(change, chat, notices);
            Ok(())
        })
        .await?
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
    /// on to its next time as the base accepts its run, before the run's turn can start: a run
    /// the base refuses leaves the task as it was, due again.
    pub(crate) async fn run_due_scheduled_tasks(
        &self,
        chat_id: ChatId,
        now: DateTime<Utc>,
    ) -> Result<Vec<Receipt>, AgentError> {
        let (unreadable, runs) = {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return Err(AgentError::NotFound);
            };
            if !chat.snapshot.chat_loop.receiving.is_receiving() {
                return Ok(Vec::new());
            }
            let mut prepared = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
            let due = prepared.take_due(&now);
            // A task whose stored schedule no longer reads, or whose end came, is dropped
            // without a run.
            let unrun: Vec<RemovedTask> = due
                .removed
                .into_iter()
                .filter(|removed| {
                    !due.fired
                        .iter()
                        .any(|fired| fired.task.id == removed.task.id)
                })
                .collect();
            let runs = due
                .fired
                .into_iter()
                .filter_map(|fired| {
                    let before = chat
                        .snapshot
                        .scheduled_tasks
                        .iter()
                        .find(|task| task.id == fired.task.id)?
                        .clone();
                    let (mut input, mut wrappers) =
                        run_input(chat, &fired.task, self.inner.settings.as_deref());
                    if let Some(reason) = fired.last_run
                        && !matches!(fired.task.kind, ScheduledTaskKind::OneShot { .. })
                    {
                        input = final_run_input(&input, &fired.task, reason);
                        wrappers.push(ScheduledInputWrapper::FinalRun);
                    }
                    let source = TriggerSource::ScheduledTask {
                        task_id: before.id,
                        scheduled_for_ms: fired.scheduled_for_ms,
                        wrappers,
                    };
                    let run = DueRun { before, fired };
                    Some(TriggerDelivery {
                        source,
                        input,
                        ledger: None,
                        on_accept: Some(Box::new(move |change, chat, receipt| {
                            change
                                .agent()
                                .commit_scheduled_run(change, chat, &run, receipt);
                        })),
                    })
                })
                .collect::<Vec<_>>();
            (unrun, runs)
        };
        if !unreadable.is_empty() {
            self.run_source_change(chat_id, move |change, chat| {
                let unreadable = unreadable
                    .into_iter()
                    .filter(|removed| chat.snapshot.scheduled_tasks.contains(&removed.task))
                    .collect();
                change
                    .agent()
                    .delete_scheduled(change, chat, unreadable, Vec::new());
            })
            .await?;
        }
        let mut receipts = Vec::new();
        let mut refusal = None;
        for delivery in runs {
            match self.deliver_trigger(chat_id, delivery).await {
                Ok(receipt) => receipts.push(receipt),
                Err(error) => {
                    tracing::debug!(chat_id = %chat_id.uuid(), %error, "a scheduled run was not taken");
                    refusal.get_or_insert(error);
                }
            }
        }
        match refusal {
            Some(error) => Err(error),
            None => Ok(receipts),
        }
    }

    /// The base accepted a task's run under `receipt`: the task moves on to its next time, or
    /// ends with its last run, unless its timing changed since the run was prepared, such as by
    /// a reschedule; a new prompt or title it was given meanwhile stays. A run merged into one
    /// of the task's that already waits is passed over like one the Loop was too busy for.
    fn commit_scheduled_run(
        &self,
        change: &SourceChange,
        chat: &mut Chat,
        run: &DueRun,
        receipt: &Receipt,
    ) {
        let Some(index) = chat.snapshot.scheduled_tasks.iter().position(|task| {
            let with_prepared_wording = ScheduledTask {
                prompt: run.before.prompt.clone(),
                title: run.before.title.clone(),
                ..task.clone()
            };
            with_prepared_wording == run.before
        }) else {
            return;
        };
        let mut notices = vec![ScheduledTaskNotice::Fired {
            task_id: run.before.id,
        }];
        if let Some(reason) = run.fired.last_run {
            chat.snapshot.scheduled_tasks.remove(index);
            notices.push(ScheduledTaskNotice::Removed {
                task_id: run.before.id,
                reason,
            });
        } else {
            let queued_itself = chat.snapshot.pending_inputs.iter().any(|pending| {
                pending.receipt.run_id == receipt.run_id
                    && matches!(
                        &pending.trigger_source,
                        Some(TriggerSource::ScheduledTask { scheduled_for_ms, .. })
                            if *scheduled_for_ms == run.fired.scheduled_for_ms
                    )
            });
            let current = &chat.snapshot.scheduled_tasks[index];
            let mut after = ScheduledTask {
                prompt: current.prompt.clone(),
                title: current.title.clone(),
                ..run.fired.task.clone()
            };
            after.skipped += u64::from(!queued_itself);
            chat.snapshot.scheduled_tasks[index] = after;
        }
        self.persist_scheduled_tasks(change, chat, notices);
    }

    /// A turn ended: forgets the user's `/loop` it read, and tells whether a self-paced task is
    /// there for [`Agent::fall_back_after_turn`] to look at once the Chats lock is released.
    pub(crate) fn end_self_paced_turn(
        &self,
        chat: &mut Chat,
        run: tickloop_harness_protocol::RunId,
    ) -> bool {
        chat.user_loops.remove(&run);
        chat.snapshot
            .scheduled_tasks
            .iter()
            .any(|task| matches!(task.kind, ScheduledTaskKind::SelfPaced { .. }))
    }

    /// When the user's self-paced `/loop` or the self-paced task started the turn `run` and it
    /// neither rescheduled nor stopped the loop, the host wakes it once more later, and after a
    /// fallback turn that did not reschedule either the loop ends. Creating a cron task without
    /// calling ScheduleWakeup converts the loop to that task instead. Made under the Chat's lease
    /// and recorded before the turn finishes, so a host that quiesces or hands over after the
    /// turn keeps the fallback.
    pub(crate) async fn fall_back_after_turn(
        &self,
        chat_id: ChatId,
        run: tickloop_harness_protocol::RunId,
    ) {
        #[cfg(any(test, feature = "test-support"))]
        self.pause_point(
            chat_id,
            crate::wakeup::trigger_pauses::PausePoint::SelfPacedTurnEnded,
        )
        .await;
        let changed = self
            .run_source_change(chat_id, move |change, chat| {
                change
                    .agent()
                    .fall_back_after_self_paced_turn(change, chat, run);
            })
            .await;
        if let Err(error) = changed {
            tracing::warn!(chat_id = %chat_id.uuid(), %error, "the self-paced loop's fallback was not scheduled");
        }
    }

    fn fall_back_after_self_paced_turn(
        &self,
        change: &SourceChange,
        chat: &mut Chat,
        run: tickloop_harness_protocol::RunId,
    ) {
        let Some(task) = chat
            .snapshot
            .scheduled_tasks
            .iter()
            .find(|task| matches!(task.kind, ScheduledTaskKind::SelfPaced { .. }))
        else {
            return;
        };
        let started_by_users_loop = task.started_by_run == Some(run.into());
        if task.scheduled_for_ms.is_some()
            || !(started_by_users_loop || started_run(chat, task, run))
        {
            return;
        }
        let mut operations = chat
            .snapshot
            .operations
            .iter()
            .filter(|operation| operation.run_id == run);
        let created_cron = operations.clone().any(|operation| {
            operation.name == tools::CRON_CREATE_TOOL_NAME
                && operation.state == OperationState::Completed
                && operation
                    .result
                    .as_ref()
                    .is_some_and(|result| !result.is_error)
        });
        let called_wakeup =
            operations.any(|operation| operation.name == tools::SCHEDULE_WAKEUP_TOOL_NAME);
        if created_cron && !called_wakeup {
            let notice = ScheduledTaskNotice::SelfPacedEnded { task_id: task.id };
            let ended = removed(task.clone(), ScheduledTaskRemovalReason::SelfPacedEnded);
            self.delete_scheduled(change, chat, vec![ended], vec![notice]);
            return;
        }
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        match tasks.self_paced_turn_ended(self.scheduled_task_rules(), &self.scheduled_now()) {
            SelfPacedTurnEnd::Unchanged => {}
            SelfPacedTurnEnd::FallbackScheduled => {
                chat.snapshot.scheduled_tasks = tasks.into_tasks();
                self.persist_scheduled_tasks(change, chat, Vec::new());
            }
            SelfPacedTurnEnd::Ended(task) => {
                chat.snapshot.scheduled_tasks = tasks.into_tasks();
                self.persist_scheduled_tasks(
                    change,
                    chat,
                    vec![
                        ScheduledTaskNotice::SelfPacedEnded { task_id: task.id },
                        ScheduledTaskNotice::Removed {
                            task_id: task.id,
                            reason: ScheduledTaskRemovalReason::SelfPacedEnded,
                        },
                    ],
                );
            }
        }
    }

    /// Esc or `/stop`: the self-paced loop's pending wake-up, its fallback and its queued run
    /// are cancelled; fixed-interval and one-shot tasks go on.
    pub(crate) fn cancel_self_paced_wakeup(&self, change: &SourceChange, chat: &mut Chat) {
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let Some(task) = tasks.stop_self_paced() else {
            return;
        };
        let notice = ScheduledTaskNotice::WakeupCancelled { task_id: task.id };
        let cancelled = removed(task, ScheduledTaskRemovalReason::UserDeleted);
        self.delete_scheduled(change, chat, vec![cancelled], vec![notice]);
    }

    /// Esc cancels the self-paced loop's pending wake-up under the Chat's lease; where it cannot,
    /// such as while another process holds the Chat, a notice tells why and Esc still stops
    /// the turn.
    pub(crate) async fn cancel_self_paced_wakeup_on_esc(&self, chat_id: ChatId) {
        let has_self_paced = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .is_some_and(|chat| {
                chat.snapshot
                    .scheduled_tasks
                    .iter()
                    .any(|task| matches!(task.kind, ScheduledTaskKind::SelfPaced { .. }))
            });
        if !has_self_paced {
            return;
        }
        let cancelled = self
            .run_source_change(chat_id, |change, chat| {
                change.agent().cancel_self_paced_wakeup(change, chat);
            })
            .await;
        if let Err(error) = cancelled {
            tracing::warn!(chat = %chat_id.uuid(), %error, "could not cancel the self-paced wake-up");
            self.inner.chat_leases.set_notice(
                chat_id,
                tickloop_harness_protocol::LoopTriggerNoticeKind::Warning,
                format!("the self-paced loop's next wake-up was not cancelled: {error}"),
            );
        }
    }

    /// Receiving resumed: each task waits for its next time after now, and nothing missed
    /// while paused runs late.
    pub(crate) fn resume_scheduled_tasks(&self, change: &SourceChange, chat: &mut Chat) {
        if chat.snapshot.scheduled_tasks.is_empty() {
            return;
        }
        let mut tasks = ScheduledTasks::new(chat.snapshot.scheduled_tasks.clone());
        let removed = tasks.resume(&self.scheduled_now());
        chat.snapshot.scheduled_tasks = tasks.into_tasks();
        let notices = removal_notices(&removed);
        let removed: Vec<ScheduledTask> = removed.into_iter().map(|removed| removed.task).collect();
        self.withdraw_deleted_task_runs(chat, &removed);
        self.persist_scheduled_tasks(change, chat, notices);
    }

    /// A Worker's history load dropped what does not outlive it, the self-paced loop among it:
    /// that is recorded under the Chat's lease, so a takeover later does not bring the loop
    /// back from the record before the load. Another live process holding the lease still runs
    /// the Chat and its tasks, so nothing is recorded then.
    pub(crate) async fn record_scheduled_tasks_after_history_load(&self, chat_id: ChatId) {
        let has_tasks = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .is_some_and(|chat| {
                !chat.snapshot.scheduled_tasks.is_empty()
                    || !chat.snapshot.scheduled_task_revocations.is_empty()
            });
        if !has_tasks {
            return;
        }
        let now = self.scheduled_now();
        let recorded = self
            .run_source_change(chat_id, move |change, chat| {
                let before = (
                    chat.snapshot.scheduled_tasks.clone(),
                    chat.snapshot.scheduled_task_revocations.clone(),
                );
                after_history_load(&mut chat.snapshot, now);
                let after = (
                    &chat.snapshot.scheduled_tasks,
                    &chat.snapshot.scheduled_task_revocations,
                );
                if after != (&before.0, &before.1) {
                    change
                        .agent()
                        .persist_scheduled_tasks(change, chat, Vec::new());
                }
            })
            .await;
        if let Err(error) = recorded {
            tracing::debug!(chat_id = %chat_id.uuid(), %error, "the history load's scheduled tasks were not recorded");
        }
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
fn unused_id(snapshot: &ChatSnapshot) -> Uuid {
    loop {
        let id = tickloop_harness_cron::generate_task_id();
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

/// The oldest a scheduled run may be, from the time it was scheduled for, to be handed over or
/// taken: an older one is void, so nothing missed long ago runs late.
const MAX_RUN_AGE_MS: u64 = 24 * 60 * 60 * 1000;
/// How long a cancellation is remembered: well past the age a run of the task may still have.
const REVOCATION_MEMORY_MS: u64 = 8 * 24 * 60 * 60 * 1000;

/// Whether a new run of `task_id` for `scheduled_for_ms` is the one its task comes due for:
/// not older than the task's next scheduled time, which a Continue moves past the times missed
/// while paused, and not of a task that is gone. A run already handed over is answered by its
/// ledger record before this is asked.
pub(crate) fn is_current_run(
    snapshot: &ChatSnapshot,
    task_id: &Uuid,
    scheduled_for_ms: u64,
) -> bool {
    snapshot.scheduled_tasks.iter().any(|task| {
        task.id == *task_id
            && task
                .scheduled_for_ms
                .is_some_and(|next| scheduled_for_ms >= next)
    })
}

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
        task_id: task.id,
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
