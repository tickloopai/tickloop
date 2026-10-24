use std::time::Duration;

use chrono::{DateTime, Offset, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz as Zone;
use tickloop_agent_cron::{Run, Schedule, TaskKind, next_run};
use tickloop_agent_protocol::{
    MAX_SCHEDULED_TASK_TITLE_CHARS, MAX_SCHEDULED_TASKS_PER_CHAT, ScheduledTask,
    ScheduledTaskConfirmation, ScheduledTaskCreator, ScheduledTaskDecision, ScheduledTaskKind,
    ScheduledTaskRemovalReason,
};
use tickloop_agent_protocol::{ScheduledTaskId, ScheduledTaskIdKind};
use tickloop_ids::IdContext;
use uuid::Uuid;

/// Recurring and self-paced tasks run one last time this long after creation, unless the host
/// sets another lifetime.
pub(crate) const TASK_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_WEEK: u64 = 7 * SECONDS_PER_DAY;
/// The shortest gap between two runs of a monthly schedule.
const SECONDS_PER_SHORTEST_MONTH: u64 = 28 * SECONDS_PER_DAY;
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
    #[error("`{0}` names no time before the task ends")]
    EndsBeforeNextRun(String),
    #[error("`{cron}` runs too often: {}", interval_limit(*min_interval))]
    TooFrequent {
        cron: String,
        min_interval: Duration,
    },
    #[error("a title must contain 1 to {MAX_SCHEDULED_TASK_TITLE_CHARS} characters")]
    BadTitle,
    #[error("no scheduled task `{}`: it does not exist or was deleted", ScheduledTaskId::from(*.0).public())]
    NotFound(Uuid),
    #[error("scheduled task `{}` {NOT_PENDING}", ScheduledTaskId::from(*.0).public())]
    NotPending(Uuid),
    #[error("scheduled task `{}` {PENDING}", ScheduledTaskId::from(*.0).public())]
    Pending(Uuid),
    #[error("scheduled task `{}` {SELF_PACED}", ScheduledTaskId::from(*.0).public())]
    SelfPaced(Uuid),
}

const NOT_PENDING: &str = "is not waiting for confirmation";
const PENDING: &str = "is waiting for confirmation and cannot be changed";
const SELF_PACED: &str = "is the self-paced loop, which has no schedule to change";

impl ScheduledTaskError {
    /// The error as the model reads it, a task's id in its Loop's model form; an id the model
    /// passed that names no task reads as [`tickloop_ids::not_found`].
    pub(crate) fn model_message(&self, ids: &IdContext, reference: Option<&str>) -> String {
        match self {
            Self::NotFound(id) => tickloop_ids::not_found::<ScheduledTaskIdKind>(
                reference
                    .map_or_else(|| ids.model_text(ScheduledTaskId::from(*id)), str::to_owned)
                    .as_str(),
            ),
            Self::NotPending(id) => ids.state_error(ScheduledTaskId::from(*id), NOT_PENDING),
            Self::Pending(id) => ids.state_error(ScheduledTaskId::from(*id), PENDING),
            Self::SelfPaced(id) => ids.state_error(ScheduledTaskId::from(*id), SELF_PACED),
            other => other.to_string(),
        }
    }
}

/// The host's rules for its scheduled tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskRules {
    /// How long a recurring or self-paced task lives; `None` lets it run until deleted or
    /// ended by the host.
    pub lifetime: Option<Duration>,
    /// The least time between two runs of a recurring task its schedule may name.
    pub min_interval: Option<Duration>,
}

impl TaskRules {
    /// Claude Code's: seven days, and no limit on how often.
    #[cfg(test)]
    pub(crate) const DEFAULT: Self = Self {
        lifetime: Some(TASK_LIFETIME),
        min_interval: None,
    };

    /// The delays ScheduleWakeup is clamped to: one minute through one hour, and never less
    /// than [`Self::min_interval`], so a self-paced loop runs no more often than a recurring
    /// task may.
    pub(crate) fn wakeup_delays(&self) -> (Duration, Duration) {
        let least = self.min_interval.map_or(MIN_WAKEUP_DELAY, |min_interval| {
            min_interval.max(MIN_WAKEUP_DELAY)
        });
        (least, MAX_WAKEUP_DELAY.max(least))
    }

    /// Whether `cron_task` keeps to [`Self::min_interval`]: every two consecutive times of a
    /// recurring schedule are at least that far apart on the wall clock. One-shot tasks run
    /// once.
    pub(crate) fn check_interval(&self, cron_task: &CronTask) -> Result<(), ScheduledTaskError> {
        let (CronTask::Recurring(schedule), Some(min_interval)) = (cron_task, self.min_interval)
        else {
            return Ok(());
        };
        let least = TimeDelta::from_std(min_interval).unwrap_or(TimeDelta::MAX);
        if schedule.shortest_gap().is_some_and(|gap| gap < least) {
            return Err(ScheduledTaskError::TooFrequent {
                cron: schedule.expression().to_owned(),
                min_interval,
            });
        }
        Ok(())
    }
}

/// How often a recurring task may run, with a schedule that keeps to it, as the model and the
/// host read it.
pub(crate) fn interval_limit(min_interval: Duration) -> String {
    let example = match min_interval.as_secs() {
        seconds if seconds <= SECONDS_PER_HOUR => "0 * * * *",
        seconds if seconds <= SECONDS_PER_DAY => "0 9 * * *",
        seconds if seconds <= SECONDS_PER_WEEK => "0 9 * * 1",
        seconds if seconds <= SECONDS_PER_SHORTEST_MONTH => "0 9 1 * *",
        _ => "0 9 1 1 *",
    };
    format!(
        "recurring tasks may run at most {}; use a schedule such as `{example}`",
        rate(min_interval)
    )
}

/// "once per hour" or "once every 30 minutes".
pub(crate) fn rate(interval: Duration) -> String {
    match count_of_units(interval) {
        (1, unit) => format!("once per {unit}"),
        (count, unit) => format!("once every {count} {unit}s"),
    }
}

/// "7 days" or "1 hour".
pub(crate) fn duration_words(duration: Duration) -> String {
    match count_of_units(duration) {
        (1, unit) => format!("1 {unit}"),
        (count, unit) => format!("{count} {unit}s"),
    }
}

/// `duration` as a whole number of the largest unit that measures it exactly.
fn count_of_units(duration: Duration) -> (u64, &'static str) {
    let seconds = duration.as_secs();
    [
        (SECONDS_PER_DAY, "day"),
        (SECONDS_PER_HOUR, "hour"),
        (SECONDS_PER_MINUTE, "minute"),
    ]
    .into_iter()
    .find(|(unit_seconds, _)| seconds >= *unit_seconds && seconds.is_multiple_of(*unit_seconds))
    .map_or((seconds, "second"), |(unit_seconds, unit)| {
        (seconds / unit_seconds, unit)
    })
}

/// A title as given, trimmed; refused when blank or longer than
/// [`MAX_SCHEDULED_TASK_TITLE_CHARS`].
pub(crate) fn title(text: &str) -> Result<String, ScheduledTaskError> {
    let title = text.trim();
    if title.is_empty() || title.chars().count() > MAX_SCHEDULED_TASK_TITLE_CHARS {
        return Err(ScheduledTaskError::BadTitle);
    }
    Ok(title.to_owned())
}

/// A cron-driven task to create.
#[derive(Debug, Clone)]
pub(crate) enum CronTask {
    Recurring(Schedule),
    OneShot(Schedule),
}

impl CronTask {
    pub(crate) fn new(schedule: Schedule, recurring: bool) -> Self {
        if recurring {
            Self::Recurring(schedule)
        } else {
            Self::OneShot(schedule)
        }
    }

    fn kind(&self) -> ScheduledTaskKind {
        match self {
            Self::Recurring(schedule) => ScheduledTaskKind::Recurring {
                cron: schedule.expression().to_string(),
            },
            Self::OneShot(schedule) => ScheduledTaskKind::OneShot {
                cron: schedule.expression().to_string(),
            },
        }
    }
}

/// A recurring or one-shot task to create.
#[derive(Debug, Clone)]
pub(crate) struct NewCronTask {
    pub id: Uuid,
    pub cron_task: CronTask,
    pub prompt: String,
    pub title: Option<String>,
    pub creation: Creation,
    /// The zone its cron expression reads times in.
    pub zone: Zone,
    pub ends_at_ms: Option<u64>,
}

/// A change to a task; `None` leaves that part as it is.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskEdit {
    pub prompt: Option<String>,
    /// `Some(None)` removes the title.
    pub title: Option<Option<String>>,
    /// The task's schedule, zone and end, all given when any of them changes.
    pub timing: Option<TimingEdit>,
    /// The Agent made the change where nobody is asked: the task waits for the user to
    /// confirm it again, as one it created would.
    pub reconfirm: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TimingEdit {
    pub cron_task: CronTask,
    pub zone: Zone,
    pub ends_at_ms: Option<u64>,
}

/// A task a change removed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemovedTask {
    pub task: ScheduledTask,
    pub reason: ScheduledTaskRemovalReason,
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
    /// Confirmed too late: its one-shot time, its lifetime or its end had passed, or its
    /// schedule names no time before its end, so it was deleted without running.
    ExpiredUnrun(RemovedTask),
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
    /// Why the task is deleted after this run; `None` when it goes on.
    pub last_run: Option<ScheduledTaskRemovalReason>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DueTasks {
    pub fired: Vec<FiredTask>,
    /// Tasks deleted by this pass, including those that just ran for the last time.
    pub removed: Vec<RemovedTask>,
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

    pub(crate) fn get(&self, id: &Uuid) -> Option<&ScheduledTask> {
        self.tasks.iter().find(|task| task.id == *id)
    }

    /// A recurring or one-shot task. It must keep to the host's least interval and name a
    /// time before it ends.
    pub(crate) fn create_cron(
        &mut self,
        new: NewCronTask,
        rules: TaskRules,
        now: &DateTime<Utc>,
    ) -> Result<&ScheduledTask, ScheduledTaskError> {
        self.ensure_room(false)?;
        rules.check_interval(&new.cron_task)?;
        let expires_at_ms = match &new.cron_task {
            CronTask::Recurring(_) => expiry(now, rules),
            CronTask::OneShot(_) => None,
        };
        let mut task = new_task(
            new.id,
            new.cron_task.kind(),
            new.prompt,
            new.creation,
            new.zone,
            now,
            expires_at_ms,
        );
        task.title = new.title;
        task.ends_at_ms = new.ends_at_ms;
        let (schedule, task_kind) = match &new.cron_task {
            CronTask::Recurring(schedule) => (schedule, TaskKind::Recurring),
            CronTask::OneShot(schedule) => (schedule, TaskKind::OneShot),
        };
        let run = in_zone(now, &new.zone)
            .and_then(|now| next_run(schedule, task_kind, &task.id, &now))
            .ok_or_else(|| ScheduledTaskError::NeverRuns(schedule.expression().to_string()))?;
        let run = before_end(&task, Some(run)).ok_or_else(|| {
            ScheduledTaskError::EndsBeforeNextRun(schedule.expression().to_string())
        })?;
        if !new.creation.needs_confirmation {
            set_timing(&mut task, &run);
        }
        Ok(self.push(task))
    }

    /// Changes a task in place: its id, creation and record of past runs stay. A new schedule,
    /// zone or end times the next run from `now`, and must keep to the host's least interval
    /// and name a time before the task ends. A change to be confirmed again leaves the task
    /// waiting for confirmation, not timing. A task already waiting for confirmation is
    /// confirmed or rejected, never changed. The self-paced loop's prompt and title change, but
    /// it has no schedule.
    pub(crate) fn edit(
        &mut self,
        id: &Uuid,
        edit: TaskEdit,
        rules: TaskRules,
        now: &DateTime<Utc>,
    ) -> Result<&ScheduledTask, ScheduledTaskError> {
        let index = self
            .tasks
            .iter()
            .position(|task| task.id == *id)
            .ok_or(ScheduledTaskError::NotFound(*id))?;
        let mut task = self.tasks[index].clone();
        // What the user confirms must be what they saw: a task waiting for confirmation does
        // not change under them.
        if task.confirmation == Some(ScheduledTaskConfirmation::Pending) {
            return Err(ScheduledTaskError::Pending(*id));
        }
        if let Some(prompt) = edit.prompt {
            task.prompt = prompt;
        }
        if let Some(title) = edit.title {
            task.title = title;
        }
        if let Some(timing) = edit.timing {
            if is_self_paced(&task) {
                return Err(ScheduledTaskError::SelfPaced(*id));
            }
            rules.check_interval(&timing.cron_task)?;
            let was_recurring = matches!(task.kind, ScheduledTaskKind::Recurring { .. });
            task.expires_at_ms = match (&timing.cron_task, was_recurring) {
                (CronTask::Recurring(_), true) => task.expires_at_ms,
                (CronTask::Recurring(_), false) => expiry(now, rules),
                (CronTask::OneShot(_), _) => None,
            };
            task.kind = timing.cron_task.kind();
            task.time_zone = timing.zone.name().to_string();
            task.ends_at_ms = timing.ends_at_ms;
            let cron = match &timing.cron_task {
                CronTask::Recurring(schedule) | CronTask::OneShot(schedule) => {
                    schedule.expression().to_string()
                }
            };
            match timing_after(&task, now) {
                Timing::Run(run) => set_timing(&mut task, &run),
                Timing::Ended => return Err(ScheduledTaskError::EndsBeforeNextRun(cron)),
                Timing::Wake(_) | Timing::AwaitingTurn | Timing::Never => {
                    return Err(ScheduledTaskError::NeverRuns(cron));
                }
            }
        }
        if edit.reconfirm {
            task.confirmation = Some(ScheduledTaskConfirmation::Pending);
            clear_timing(&mut task);
        }
        self.tasks[index] = task;
        Ok(&self.tasks[index])
    }

    /// `/loop` without an interval: replaces any self-paced task with one whose first turn is
    /// the one running now, so it waits for that turn to reschedule it.
    pub(crate) fn start_self_paced(
        &mut self,
        id: Uuid,
        prompt: String,
        creation: Creation,
        zone: Zone,
        rules: TaskRules,
        now: &DateTime<Utc>,
    ) -> Result<&ScheduledTask, ScheduledTaskError> {
        self.ensure_room(true)?;
        self.tasks.retain(|task| !is_self_paced(task));
        let kind = ScheduledTaskKind::SelfPaced {
            delay_seconds: None,
            reason: None,
            fallback: false,
        };
        let task = new_task(id, kind, prompt, creation, zone, now, expiry(now, rules));
        Ok(self.push(task))
    }

    /// ScheduleWakeup: reschedules the self-paced task, creating one with `id` when there is
    /// none. The delay is clamped to one minute through one hour. Rescheduling changes only
    /// the delay and the reason: replacing the instruction would let the Agent swap what the
    /// user started or confirmed without asking again.
    pub(crate) fn schedule_wakeup(
        &mut self,
        id: Uuid,
        wakeup: Wakeup,
        creation: Creation,
        zone: Zone,
        rules: TaskRules,
        now: &DateTime<Utc>,
    ) -> Result<ScheduledWakeup<'_>, ScheduledTaskError> {
        let (least, most) = rules.wakeup_delays();
        let delay = wakeup.delay.clamp(least, most);
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
                    expiry(now, rules),
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
    /// after [`FALLBACK_WAKEUP_DELAY`], or the host's least interval when longer; when that
    /// fallback turn does not reschedule either, the task ends.
    pub(crate) fn self_paced_turn_ended(
        &mut self,
        rules: TaskRules,
        now: &DateTime<Utc>,
    ) -> SelfPacedTurnEnd {
        let Some(index) = self.tasks.iter().position(is_self_paced) else {
            return SelfPacedTurnEnd::Unchanged;
        };
        let task = &mut self.tasks[index];
        if task.paused_at_ms.is_some()
            || task.scheduled_for_ms.is_some()
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
        set_wake(
            task,
            now,
            FALLBACK_WAKEUP_DELAY.max(rules.wakeup_delays().0),
        );
        SelfPacedTurnEnd::FallbackScheduled
    }

    pub(crate) fn set_paused(
        &mut self,
        id: &Uuid,
        paused: bool,
        now: &DateTime<Utc>,
    ) -> Result<(), ScheduledTaskError> {
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.id == *id)
            .ok_or(ScheduledTaskError::NotFound(*id))?;
        if task.confirmation == Some(ScheduledTaskConfirmation::Pending) {
            return Err(ScheduledTaskError::Pending(*id));
        }
        if paused == task.paused_at_ms.is_some() {
            return Ok(());
        }
        if paused {
            task.paused_at_ms = Some(millis(now));
        } else {
            task.paused_at_ms = None;
            match task.kind {
                ScheduledTaskKind::SelfPaced { .. } => {
                    if task.next_fire_at_ms.is_none_or(|fire| fire <= millis(now)) {
                        set_wake(task, now, MIN_WAKEUP_DELAY);
                    }
                }
                ScheduledTaskKind::Recurring { .. } | ScheduledTaskKind::OneShot { .. } => {
                    match timing_after(task, now) {
                        Timing::Run(run) => set_timing(task, &run),
                        Timing::Wake(_) | Timing::AwaitingTurn | Timing::Ended | Timing::Never => {
                            clear_timing(task)
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn delete(&mut self, id: &Uuid) -> Option<ScheduledTask> {
        let index = self.tasks.iter().position(|task| task.id == *id)?;
        Some(self.tasks.remove(index))
    }

    /// The user's answer to a pending task: confirming starts its timing from `now`, unless
    /// the task already expired, and rejecting deletes it.
    pub(crate) fn decide(
        &mut self,
        id: &Uuid,
        decision: ScheduledTaskDecision,
        now: &DateTime<Utc>,
    ) -> Result<DecisionOutcome<'_>, ScheduledTaskError> {
        let index = self
            .tasks
            .iter()
            .position(|task| task.id == *id)
            .ok_or(ScheduledTaskError::NotFound(*id))?;
        if self.tasks[index].confirmation != Some(ScheduledTaskConfirmation::Pending) {
            return Err(ScheduledTaskError::NotPending(*id));
        }
        match decision {
            ScheduledTaskDecision::Reject => {
                Ok(DecisionOutcome::Rejected(self.tasks.remove(index)))
            }
            ScheduledTaskDecision::Confirm => {
                if let Some(reason) = ended(&self.tasks[index], now) {
                    return Ok(DecisionOutcome::ExpiredUnrun(RemovedTask {
                        task: self.tasks.remove(index),
                        reason,
                    }));
                }
                let task = &mut self.tasks[index];
                task.confirmation = Some(ScheduledTaskConfirmation::Confirmed);
                match timing_after(task, now) {
                    Timing::Run(run) => set_timing(task, &run),
                    Timing::Wake(delay) => set_wake(task, now, delay),
                    Timing::AwaitingTurn => {}
                    Timing::Ended => {
                        return Ok(DecisionOutcome::ExpiredUnrun(RemovedTask {
                            task: self.tasks.remove(index),
                            reason: ScheduledTaskRemovalReason::EndsAtReached,
                        }));
                    }
                    Timing::Never => {
                        return Ok(DecisionOutcome::ExpiredUnrun(RemovedTask {
                            task: self.tasks.remove(index),
                            reason: ScheduledTaskRemovalReason::Expired,
                        }));
                    }
                }
                Ok(DecisionOutcome::Confirmed(&self.tasks[index]))
            }
        }
    }

    /// Runs every timing task whose fire time has come, each at most once: scheduled times
    /// passed over while the Loop was busy or the host woke late count as skipped, never as
    /// extra runs. One-shot tasks, recurring or self-paced ones past their expiry, and those
    /// with no run left before their end are deleted after running; any other task whose end
    /// came is deleted without one.
    pub(crate) fn take_due(&mut self, now: &DateTime<Utc>) -> DueTasks {
        let now_ms = millis(now);
        let mut due = DueTasks::default();
        let mut kept = Vec::with_capacity(self.tasks.len());
        for mut task in std::mem::take(&mut self.tasks) {
            // A host waking after the end runs nothing of the task, even a run due before it.
            if task.ends_at_ms.is_some_and(|end| end <= now_ms) {
                due.removed.push(RemovedTask {
                    task,
                    reason: ScheduledTaskRemovalReason::EndsAtReached,
                });
                continue;
            }
            let is_due = task.is_timing()
                && task
                    .next_fire_at_ms
                    .is_some_and(|fire_at| fire_at <= now_ms);
            if !is_due {
                kept.push(task);
                continue;
            }
            let Some(fired) = fire(&mut task, now) else {
                // Its stored schedule or zone no longer reads.
                due.removed.push(RemovedTask {
                    task,
                    reason: ScheduledTaskRemovalReason::Expired,
                });
                continue;
            };
            let last_run = fired.last_run;
            due.fired.push(fired);
            match last_run {
                Some(reason) => due.removed.push(RemovedTask { task, reason }),
                None => kept.push(task),
            }
        }
        self.tasks = kept;
        due
    }

    /// Receiving resumed: every task waits for its next fire time strictly after `now`, so
    /// nothing missed while paused runs late. One-shot and self-paced tasks whose fire time
    /// passed, jitter included, are deleted, as are recurring ones with no run left before
    /// their end. Returns the deleted tasks.
    pub(crate) fn resume(&mut self, now: &DateTime<Utc>) -> Vec<RemovedTask> {
        let now_ms = millis(now);
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.tasks.len());
        for mut task in std::mem::take(&mut self.tasks) {
            let removal = match (&task.kind, task.is_timing()) {
                (_, false) => None,
                (ScheduledTaskKind::Recurring { .. }, true) => match timing_after(&task, now) {
                    Timing::Run(run) => {
                        set_timing(&mut task, &run);
                        None
                    }
                    Timing::Ended => Some(ScheduledTaskRemovalReason::EndsAtReached),
                    Timing::Wake(_) | Timing::AwaitingTurn | Timing::Never => {
                        Some(ScheduledTaskRemovalReason::Expired)
                    }
                },
                (ScheduledTaskKind::OneShot { .. }, true) => task
                    .next_fire_at_ms
                    .is_none_or(|fire_at| fire_at <= now_ms)
                    .then_some(ScheduledTaskRemovalReason::Expired),
                (ScheduledTaskKind::SelfPaced { .. }, true) => task
                    .next_fire_at_ms
                    .is_none_or(|fire_at| fire_at <= now_ms)
                    .then_some(ScheduledTaskRemovalReason::SelfPacedEnded),
            };
            match removal {
                Some(reason) => removed.push(RemovedTask { task, reason }),
                None => kept.push(task),
            }
        }
        self.tasks = kept;
        removed
    }

    /// An ordinary history load drops ended tasks and unpaused self-paced tasks. A user's
    /// individual pause survives while the task's lifetime and end permit. Receiving starts
    /// paused, so [`Self::resume`] sets the timing of what remains. Returns the deleted tasks.
    pub(crate) fn after_history_load(&mut self, now: &DateTime<Utc>) -> Vec<ScheduledTask> {
        let now_ms = millis(now);
        let (kept, removed) = std::mem::take(&mut self.tasks)
            .into_iter()
            .partition(|task| {
                let before_end = task.ends_at_ms.is_none_or(|end| end > now_ms);
                before_end
                    && ((task.paused_at_ms.is_some()
                        && task.expires_at_ms.is_none_or(|expires| expires > now_ms))
                        || (!is_self_paced(task) && ended(task, now).is_none()))
            });
        self.tasks = kept;
        removed
    }

    /// When the host must next wake the Loop: the earliest fire time of a confirmed, timing
    /// task or end of any task, and none while receiving is paused.
    pub(crate) fn next_wake_at_ms(&self, receiving: bool) -> Option<u64> {
        if !receiving {
            return None;
        }
        self.tasks
            .iter()
            .filter(|task| task.is_timing())
            .filter_map(|task| task.next_fire_at_ms)
            .chain(self.tasks.iter().filter_map(|task| task.ends_at_ms))
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
    /// The schedule's next run would fire at or after the task's end.
    Ended,
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
    match (run.is_some(), before_end(task, run)) {
        (_, Some(run)) => Timing::Run(run),
        (true, None) => Timing::Ended,
        (false, None) => Timing::Never,
    }
}

/// `run` when it fires before the task's end.
fn before_end<Tz: TimeZone>(task: &ScheduledTask, run: Option<Run<Tz>>) -> Option<Run<Tz>> {
    run.filter(|run| task.ends_at_ms.is_none_or(|end| millis(&run.fire_at) < end))
}

/// Runs a due task: records the run and sets its next one. `None` when the stored schedule
/// no longer parses, so the task is dropped.
fn fire(task: &mut ScheduledTask, now: &DateTime<Utc>) -> Option<FiredTask> {
    let now_ms = millis(now);
    let mut scheduled_for_ms = task.scheduled_for_ms?;
    let mut skipped = 0;
    let mut ends_with = None;
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
            let has_next = next.is_some();
            match before_end(task, next) {
                Some(run) => set_timing(task, &run),
                None => {
                    clear_timing(task);
                    if has_next {
                        ends_with = Some(ScheduledTaskRemovalReason::EndsAtReached);
                    }
                }
            }
        }
        ScheduledTaskKind::OneShot { .. } | ScheduledTaskKind::SelfPaced { .. } => {
            clear_timing(task)
        }
    }
    let is_expired = task
        .expires_at_ms
        .is_some_and(|expires_at| expires_at <= now_ms);
    let last_run = if is_expired {
        Some(ScheduledTaskRemovalReason::Expired)
    } else {
        match task.kind {
            ScheduledTaskKind::OneShot { .. } => Some(ScheduledTaskRemovalReason::OneShotCompleted),
            ScheduledTaskKind::Recurring { .. } if task.next_fire_at_ms.is_none() => {
                Some(ends_with.unwrap_or(ScheduledTaskRemovalReason::Expired))
            }
            ScheduledTaskKind::Recurring { .. } | ScheduledTaskKind::SelfPaced { .. } => None,
        }
    };
    if last_run.is_some() {
        clear_timing(task);
    }
    task.last_fired_at_ms = Some(now_ms);
    task.last_scheduled_for_ms = Some(scheduled_for_ms);
    task.skipped += skipped;
    Some(FiredTask {
        task: task.clone(),
        scheduled_for_ms,
        skipped,
        last_run,
    })
}

/// Why a task can no longer run, when it cannot: a one-shot task's time, or a recurring or
/// self-paced task's lifetime, has passed, or its end came.
fn ended(task: &ScheduledTask, now: &DateTime<Utc>) -> Option<ScheduledTaskRemovalReason> {
    let now_ms = millis(now);
    if task.ends_at_ms.is_some_and(|end| end <= now_ms) {
        return Some(ScheduledTaskRemovalReason::EndsAtReached);
    }
    let has_expired = match &task.kind {
        ScheduledTaskKind::Recurring { .. } | ScheduledTaskKind::SelfPaced { .. } => task
            .expires_at_ms
            .is_some_and(|expires_at| expires_at <= now_ms),
        ScheduledTaskKind::OneShot { cron } => {
            one_shot_time(task, cron).is_none_or(|scheduled_for| scheduled_for <= now_ms)
        }
    };
    has_expired.then_some(ScheduledTaskRemovalReason::Expired)
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
    id: Uuid,
    kind: ScheduledTaskKind,
    prompt: String,
    creation: Creation,
    zone: Zone,
    now: &DateTime<Utc>,
    expires_at_ms: Option<u64>,
) -> ScheduledTask {
    ScheduledTask {
        paused_at_ms: None,
        id,
        kind,
        title: None,
        prompt,
        creator: creation.creator,
        time_zone: zone.name().to_string(),
        confirmation: creation
            .needs_confirmation
            .then_some(ScheduledTaskConfirmation::Pending),
        created_at_ms: millis(now),
        expires_at_ms,
        ends_at_ms: None,
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

fn expiry(now: &DateTime<Utc>, rules: TaskRules) -> Option<u64> {
    rules.lifetime.map(|lifetime| {
        millis(now).saturating_add(u64::try_from(lifetime.as_millis()).unwrap_or(u64::MAX))
    })
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
