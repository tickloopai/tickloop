//! The process entries the `tickloop` executable dispatches before it starts the Console.

mod package_command;

use std::ffi::OsString;

use tickloop_agent_client::{DAEMON_COMMAND, RUN_SUBCOMMAND};
use tickloop_agent_daemon::{WORKER_ARGUMENT, daemon_control, run_daemon, run_worker};

const MONITOR_COMMAND: &str = "monitor";
const TICK_COMMAND: &str = "tick";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientEntry {
    Console,
    Print,
    Acp,
    Internal,
}

/// Classifies only option positions, never an option's value or text after `--`.
/// Full validation remains with the selected adapter.
pub fn client_entry(arguments: &[OsString]) -> ClientEntry {
    match arguments.first().and_then(|arg| arg.to_str()) {
        Some("acp") => return ClientEntry::Acp,
        Some("update" | "daemon" | "worker" | "monitor" | "tick") => return ClientEntry::Internal,
        _ => {}
    }
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("-p" | "--print" | "--p") => return ClientEntry::Print,
            Some("--") => break,
            Some(
                "--cwd"
                | "--mode"
                | "--view-handoff"
                | "--chat-id"
                | "--config"
                | "--auto-reviewer-config"
                | "--permission-checker-config"
                | "--output-format"
                | "--timeout",
            ) => {
                arguments.next();
            }
            Some("-r" | "--resume") => {
                // Resume has an optional non-option Chat ID. It cannot contain a print flag.
            }
            Some(value) if value.starts_with('-') => {}
            _ => break,
        }
    }
    ClientEntry::Console
}

pub async fn run_noninteractive(entry: ClientEntry, arguments: &[OsString]) -> Option<i32> {
    if !matches!(entry, ClientEntry::Print | ClientEntry::Acp) {
        return None;
    }
    let values = arguments
        .iter()
        .cloned()
        .map(|argument| argument.into_string())
        .collect::<Result<Vec<_>, _>>();
    let Ok(mut values) = values else {
        eprintln!("Error: command arguments must be valid UTF-8");
        return Some(2);
    };
    Some(match entry {
        ClientEntry::Print => tickloop_agent_print::run(values).await,
        ClientEntry::Acp => {
            values.remove(0);
            match tickloop_agent_acp::run(values).await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!(
                        "Error: {}",
                        tickloop_agent_console::terminal::safe_text(&error)
                    );
                    1
                }
            }
        }
        _ => unreachable!(),
    })
}

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
        (TICK_COMMAND, _) => Some(package_command::run(&arguments[1..]).await),
        _ => None,
    }
}
