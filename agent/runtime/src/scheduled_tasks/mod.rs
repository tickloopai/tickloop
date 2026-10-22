//! A Chat's scheduled tasks, following Claude Code's `/loop` and cron tools: the task set and
//! its timing rules, the tools, and how the Agent runs them.

pub(crate) mod tasks;
#[cfg(test)]
mod tasks_tests;
pub(crate) mod tools;
mod wiring;

pub(crate) use wiring::{ScheduleClock, after_history_load, is_current_run, resolve_zone};
