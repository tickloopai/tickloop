//! The `tick` command: parses `tick <provider> <tick>`, checks that the provider implements the
//! interfaces the Tick reads, and runs it standalone or connected to its host.

pub mod cli;
pub mod entry;
mod locale;
pub mod login;
mod native_launcher;
