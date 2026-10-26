//! The process entries the `tickloop` executable dispatches before it starts the Console.

mod mcp_command;
mod mcp_login;
mod plugin_command;
mod plugin_config;
mod plugin_marketplace;
mod plugin_mcp;
mod plugin_tick;

use std::ffi::OsString;

use tickloop_code_client::{DAEMON_COMMAND, RUN_SUBCOMMAND};
use tickloop_code_daemon::{WORKER_ARGUMENT, daemon_control, run_daemon, run_worker};
use tickloop_tick_installer::PluginKind;

const MONITOR_COMMAND: &str = "monitor";
const TICK_COMMAND: &str = "tick";
const PLUGIN_COMMAND: &str = "plugin";

/// Names this build, from its sources, for the comparison between a View and the running daemon.
pub fn name_this_build() {
    tickloop_code_access::set_build_identity(env!("TICKLOOP_BUILD_IDENTITY"));
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
            Some(tickloop_code_console::terminal::monitor_command::run(&arguments[1..]).await)
        }
        (TICK_COMMAND, _) => Some(plugin_command::run(PluginKind::Tick, &arguments[1..]).await),
        ("mcp", _) => Some(mcp_command::run(&arguments[1..]).await),
        (PLUGIN_COMMAND, _) => Some(plugin_command::run(PluginKind::Plugin, &arguments[1..]).await),
        _ => None,
    }
}
