//! The process entries the `tickloop` executable dispatches before it starts the Console.

use std::ffi::OsString;

use tickloop_agent_client::{DAEMON_COMMAND, RUN_SUBCOMMAND};
use tickloop_agent_daemon::{WORKER_ARGUMENT, daemon_control, run_daemon, run_worker};

const MONITOR_COMMAND: &str = "monitor";

/// Names this build, from its sources, for the comparison between a View and the running daemon.
pub fn name_this_build() {
    tickloop_agent_protocol::set_build_identity(env!("TICKLOOP_BUILD_IDENTITY"));
}

/// Runs the process entry `arguments` name, or returns `None` when they name none and the
/// Console should start.
pub async fn run_internal(arguments: &[OsString]) -> Option<Result<(), String>> {
    let command = arguments.first()?.to_str()?;
    let subcommand = arguments.get(1).and_then(|value| value.to_str());
    match (command, subcommand) {
        (DAEMON_COMMAND, Some(RUN_SUBCOMMAND)) => Some(run_daemon(&arguments[2..]).await),
        (DAEMON_COMMAND, _) => Some(daemon_control::run(&arguments[1..]).await),
        (WORKER_ARGUMENT, _) => Some(run_worker(&arguments[1..]).await),
        (MONITOR_COMMAND, _) => {
            Some(tickloop_agent_console::terminal::monitor_command::run(&arguments[1..]).await)
        }
        _ => None,
    }
}
