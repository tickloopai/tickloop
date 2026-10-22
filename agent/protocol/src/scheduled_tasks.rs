use serde::{Deserialize, Serialize};

/// The most scheduled tasks one Chat holds, self-paced one included.
pub const MAX_SCHEDULED_TASKS_PER_CHAT: usize = 50;

/// A Chat's scheduled task: an instruction its Loop runs at times a schedule names, like
/// Claude Code's `/loop` and cron tools. Times are Unix milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTask {
    /// Eight lowercase hexadecimal characters; jitter is derived from it.
    pub id: String,
    pub kind: ScheduledTaskKind,
    pub prompt: String,
    pub prompt_source: ScheduledTaskPromptSource,
    /// `None` when the task never needed confirming.
    pub confirmation: Option<ScheduledTaskConfirmation>,
    pub created_at_ms: u64,
    /// When a recurring or self-paced task runs for the last time; one-shot tasks have none.
    pub expires_at_ms: Option<u64>,
    /// The time the schedule names for the next run; `None` while the task is not timing,
    /// such as a pending task or a self-paced task waiting for its turn to reschedule it.
    pub scheduled_for_ms: Option<u64>,
    /// When the next run fires: `scheduled_for_ms` shifted by `jitter_ms`.
    pub next_fire_at_ms: Option<u64>,
    pub jitter_ms: i64,
    pub last_fired_at_ms: Option<u64>,
    /// Scheduled times passed over without running because the Loop was busy or the host woke
    /// late, summed over the task's life.
    pub skipped: u64,
}

impl ScheduledTask {
    /// Whether the task is counting down to its next run.
    pub fn is_timing(&self) -> bool {
        self.confirmation != Some(ScheduledTaskConfirmation::Pending)
            && self.next_fire_at_ms.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ScheduledTaskKind {
    /// Runs at every time a 5-field cron expression names, in the host's local time zone.
    Recurring { cron: String },
    /// Runs once, at the first time the cron expression names after creation.
    OneShot { cron: String },
    /// `/loop` without an interval: each turn picks the next wake-up with ScheduleWakeup.
    /// `delay_seconds` and `reason` are the latest pick; `fallback` marks the one wake-up the
    /// host adds when a turn neither reschedules nor stops.
    SelfPaced {
        delay_seconds: Option<u64>,
        reason: Option<String>,
        fallback: bool,
    },
}

/// Who wrote the instruction the task runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskPromptSource {
    /// The user, through `/loop`.
    User,
    /// The Agent, through CronCreate or ScheduleWakeup.
    Agent,
}

/// Confirmation of a task the Agent created on a host without permission requests: it starts
/// timing only once the user confirms it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskConfirmation {
    Pending,
    Confirmed,
}

/// The user's answer to a pending task; rejecting deletes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskDecision {
    Confirm,
    Reject,
}
