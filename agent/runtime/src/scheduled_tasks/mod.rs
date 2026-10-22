//! A Chat's scheduled tasks, following Claude Code's `/loop` and cron tools: the task set and
//! its timing rules, the tools, and how the Agent runs them.

pub(crate) mod tasks;
#[cfg(test)]
mod tasks_tests;
pub(crate) mod tools;
mod wiring;

pub(crate) use wiring::{ScheduleClock, after_history_load, resolve_zone};

/// The built-in Skill a user starts a `/loop` with; tasks created in its turn are the user's.
pub(crate) const LOOP_SKILL_NAME: &str = "loop";
/// The prompt of a `/loop` without an instruction: each run reads the Chat's loop.md, or the
/// built-in maintenance instructions when there is none.
pub(crate) const LOOP_INSTRUCTIONS_PLACEHOLDER: &str = "<<loop-instructions>>";
