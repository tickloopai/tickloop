use chrono::{DateTime, TimeDelta, TimeZone, Timelike};
use uuid::Uuid;

use crate::Schedule;

const TASK_ID_LENGTH: usize = 8;
const RECURRING_MAX_DELAY: TimeDelta = TimeDelta::minutes(30);
const EVERY_FIVE_MINUTES_LEAD: TimeDelta = TimeDelta::seconds(15);
const ONE_SHOT_MAX_LEAD: TimeDelta = TimeDelta::seconds(90);
/// One-shot tasks only fire early on the marks people round to: `:00` and `:30`.
const ONE_SHOT_ROUND_MINUTE: u32 = 30;
const FRACTION_DENOMINATOR: i128 = 1 << 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Recurring,
    OneShot,
}

/// One run of a task: the time its schedule names and the time it actually fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run<Tz: TimeZone> {
    pub scheduled_for: DateTime<Tz>,
    pub fire_at: DateTime<Tz>,
}

impl<Tz: TimeZone> Run<Tz> {
    /// How much later (positive) or earlier (negative) than scheduled the run fires.
    pub fn jitter(&self) -> TimeDelta {
        self.fire_at.clone() - self.scheduled_for.clone()
    }
}

/// An 8-character lowercase hexadecimal id, like Claude Code's: the start of a random UUID.
pub fn generate_task_id() -> String {
    Uuid::new_v4().simple().to_string()[..TASK_ID_LENGTH].to_string()
}

/// The first run scheduled strictly after `after`, fired as [`run_for`] describes and never
/// before `after`. To advance a recurring task, pass the previous run's `scheduled_for`, not
/// its `fire_at`, which may precede it.
pub fn next_run<Tz: TimeZone>(
    schedule: &Schedule,
    kind: TaskKind,
    task_id: &str,
    after: &DateTime<Tz>,
) -> Option<Run<Tz>> {
    let scheduled_for = schedule.next_after(after)?;
    Some(run_for(schedule, kind, task_id, scheduled_for, after))
}

/// The run of a known scheduled time, such as one restored from storage, with jitter derived
/// from `task_id` so that many tasks on the same expression do not fire at the same moment.
///
/// - Recurring tasks fire late by a fixed share of the gap to the following run: under half
///   of it and at most 30 minutes, so evenly spaced runs are always delayed by the same
///   amount. A `*/5 * * * *` task fires 15 seconds early instead.
/// - One-shot tasks scheduled on `:00` or `:30` local time fire up to 90 seconds early;
///   other minutes fire on time.
///
/// The run never fires before `not_before`, typically when the task was created.
pub fn run_for<Tz: TimeZone>(
    schedule: &Schedule,
    kind: TaskKind,
    task_id: &str,
    scheduled_for: DateTime<Tz>,
    not_before: &DateTime<Tz>,
) -> Run<Tz> {
    let jitter = match kind {
        TaskKind::Recurring if schedule.is_every_five_minutes() => -EVERY_FIVE_MINUTES_LEAD,
        TaskKind::Recurring => schedule
            .gap_after(&scheduled_for)
            .map(|gap| share(gap / 2, task_id).min(RECURRING_MAX_DELAY))
            .unwrap_or(TimeDelta::zero()),
        TaskKind::OneShot if scheduled_for.minute().is_multiple_of(ONE_SHOT_ROUND_MINUTE) => {
            -share(ONE_SHOT_MAX_LEAD, task_id)
        }
        TaskKind::OneShot => TimeDelta::zero(),
    };
    let fire_at = (scheduled_for.clone() + jitter).max(not_before.clone());
    Run {
        scheduled_for,
        fire_at,
    }
}

/// `span` scaled by the task id read as a fraction in `[0, 1)`. Ids that are not 8 hex
/// digits get no jitter.
fn share(span: TimeDelta, task_id: &str) -> TimeDelta {
    let fraction = if task_id.len() == TASK_ID_LENGTH {
        u32::from_str_radix(task_id, 16).unwrap_or(0)
    } else {
        0
    };
    let milliseconds =
        i128::from(span.num_milliseconds()) * i128::from(fraction) / FRACTION_DENOMINATOR;
    TimeDelta::milliseconds(milliseconds as i64)
}
