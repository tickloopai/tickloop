//! A Chat's scheduled tasks, following Claude Code's `/loop` and cron tools: the task set and
//! its timing rules. The caller passes the current time in the host's local time zone and
//! owns timers, delivery and the Transcript.

pub(crate) mod tasks;
#[cfg(test)]
mod tasks_tests;
