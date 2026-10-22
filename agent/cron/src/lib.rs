//! Timing rules of scheduled tasks, following Claude Code's `/loop` and cron tools.
//!
//! - [`Schedule`]: a 5-field cron expression (`minute hour day-of-month month day-of-week`)
//!   evaluated in whatever time zone the caller's times carry, so the host passes local time.
//! - [`Interval`] and [`parse_loop_arguments`]: `/loop` intervals and their conversion to cron.
//! - [`next_run`]: the next scheduled time of a task and when it actually fires after jitter.
//! - [`generate_task_id`]: 8-character task ids.
//!
//! Every function is pure apart from id generation: callers pass the current time.

mod interval;
mod jitter;
mod schedule;

pub use interval::{CronInterval, Interval, IntervalError, LoopArguments, parse_loop_arguments};
pub use jitter::{Run, TaskKind, generate_task_id, next_run};
pub use schedule::{Schedule, ScheduleError};
