use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

/// The most scheduled tasks one Chat holds, self-paced one included.
pub const MAX_SCHEDULED_TASKS_PER_CHAT: usize = 50;
/// The most characters a scheduled task's title holds.
pub const MAX_SCHEDULED_TASK_TITLE_CHARS: usize = 120;
/// The namespace of the UUIDv5 ids that tasks recorded with an eight-character id read as.
const LEGACY_TASK_NAMESPACE: Uuid = Uuid::from_u128(0x6f1c_5a0e_93d4_4b8a_a0f2_57c1_e93b_0d24);
/// The length of the ids tasks were recorded with before they had UUIDs.
const LEGACY_TASK_ID_LENGTH: usize = 8;

/// A Chat's scheduled task: an instruction its Loop runs at times a schedule names, like
/// Claude Code's `/loop` and cron tools. Times are Unix milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTask {
    /// Unique across every Chat and kept for the task's life: a UUIDv7 for a new task, or the
    /// one the host chose. Jitter is derived from it.
    pub id: Uuid,
    pub kind: ScheduledTaskKind,
    /// A short name the user or the Agent gave the task, such as "Daily news digest".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The instruction to run, or a placeholder the host replaces with `loop.md` or the
    /// built-in maintenance instructions each time the task runs.
    pub prompt: String,
    pub creator: ScheduledTaskCreator,
    /// The IANA time zone its cron expression is read in: the host's local zone, or the user's
    /// zone a stateless host supplies.
    pub time_zone: String,
    /// `None` when the task never needed confirming.
    pub confirmation: Option<ScheduledTaskConfirmation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at_ms: Option<u64>,
    pub created_at_ms: u64,
    /// When a recurring or self-paced task runs for the last time; one-shot tasks, and every
    /// task of a host without a lifetime limit, have none.
    pub expires_at_ms: Option<u64>,
    /// When the host ends the task: no run fires at or after it, and the task is deleted then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at_ms: Option<u64>,
    /// The time the schedule names for the next run; `None` while the task is not timing,
    /// such as a pending task or a self-paced task waiting for its turn to reschedule it.
    pub scheduled_for_ms: Option<u64>,
    /// When the next run fires: `scheduled_for_ms` shifted by `jitter_ms`.
    pub next_fire_at_ms: Option<u64>,
    pub jitter_ms: i64,
    pub last_fired_at_ms: Option<u64>,
    /// The scheduled time the latest run covered; with the task id it names that run's
    /// delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scheduled_for_ms: Option<u64>,
    /// The turn of the user's `/loop` that started a self-paced task; that turn's end
    /// reschedules it or falls back like the turns its wake-ups start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by_run: Option<uuid::Uuid>,
    /// Scheduled times passed over without running because the Loop was busy or the host woke
    /// late, summed over the task's life.
    pub skipped: u64,
}

impl ScheduledTask {
    /// A task as the Chat `chat_id`'s record holds it. A task recorded before tasks had UUIDs
    /// names itself by an eight-character id; it reads as [`legacy_scheduled_task_id`], the same
    /// on every read.
    pub fn from_record(chat_id: Uuid, mut record: serde_json::Value) -> serde_json::Result<Self> {
        let legacy = record
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| id.len() == LEGACY_TASK_ID_LENGTH)
            .map(str::to_owned);
        if let Some(short_id) = legacy {
            let created_at_ms = record
                .get("created_at_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            record["id"] = serde_json::Value::String(
                legacy_scheduled_task_id(chat_id, &short_id, created_at_ms).to_string(),
            );
        }
        serde_json::from_value(record)
    }

    /// Whether the task is counting down to its next run.
    pub fn is_timing(&self) -> bool {
        self.paused_at_ms.is_none()
            && self.confirmation != Some(ScheduledTaskConfirmation::Pending)
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

/// Who created the task, which decides whether a host without permission requests asks the
/// user to confirm it first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskCreator {
    /// The user: CronCreate or ScheduleWakeup in the turn the user's `/loop` started.
    User,
    /// The Agent on its own: every other CronCreate or ScheduleWakeup.
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

/// What happened to a scheduled task that the Chat shows once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduledTaskNotice {
    /// A run came due and was handed to the Loop.
    Fired { task_id: Uuid },
    /// Esc or `/stop` cancelled the self-paced task's pending wake-up.
    WakeupCancelled { task_id: Uuid },
    /// A self-paced task ended because neither its turn nor the fallback turn rescheduled it.
    SelfPacedEnded { task_id: Uuid },
    /// The user confirmed a pending task after its time or its lifetime had passed, so it was
    /// deleted without running.
    ExpiredUnrun { task_id: Uuid },
    /// The task left the Chat; every way a task ends announces this once, beside the notices
    /// above.
    Removed {
        task_id: Uuid,
        reason: ScheduledTaskRemovalReason,
    },
}

/// Why a scheduled task left its Chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskRemovalReason {
    /// The user deleted it through the host, or cancelled the self-paced loop with Esc or
    /// `/stop`.
    UserDeleted,
    /// The Agent deleted it with CronDelete.
    AgentDeleted,
    /// Its lifetime or its one-shot time passed, or its stored schedule no longer reads.
    Expired,
    /// The end the host set for it came.
    EndsAtReached,
    /// The user rejected it while it waited for confirmation.
    Rejected,
    /// A one-shot task ran.
    OneShotCompleted,
    /// The self-paced loop stopped, was not rescheduled, or gave way to a cron task or a new
    /// `/loop`.
    SelfPacedEnded,
}

/// A task the user or the host cancelled: a run of it handed over before, or restored from the
/// ledger, is not taken. A one-shot or final run deleting its task is no cancellation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTaskRevocation {
    #[serde(deserialize_with = "scheduled_task_reference")]
    pub task_id: Uuid,
    pub revoked_at_ms: u64,
}

/// The UUIDv5 a task recorded with the eight-character id `short_id` reads as, from its Chat,
/// that id and its creation time.
pub fn legacy_scheduled_task_id(chat_id: Uuid, short_id: &str, created_at_ms: u64) -> Uuid {
    let mut name = Vec::with_capacity(16 + short_id.len() + 8);
    name.extend_from_slice(chat_id.as_bytes());
    name.extend_from_slice(short_id.as_bytes());
    name.extend_from_slice(&created_at_ms.to_be_bytes());
    Uuid::new_v5(&LEGACY_TASK_NAMESPACE, &name)
}

/// A task id a run or a cancellation refers to. One recorded as an eight-character id reads as
/// a UUIDv5 of that id alone: the record has no creation time to name the task by, so it
/// matches other such references and no task.
pub(crate) fn scheduled_task_reference<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Uuid, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text.len() == LEGACY_TASK_ID_LENGTH {
        return Ok(Uuid::new_v5(&LEGACY_TASK_NAMESPACE, text.as_bytes()));
    }
    text.parse().map_err(serde::de::Error::custom)
}
