//! `tickloop monitor`: the attach client a Monitor popup runs, and the explicit commands that
//! install or remove that popup's plugin in one Herdr session.

mod popup;

use std::{ffi::OsString, io::IsTerminal, path::PathBuf};

use crate::terminal::attach::{ATTACH_ENVIRONMENT, AttachEnd, run_view};
use tickloop_agent_protocol::HostedViewGrant;

const MONITOR_USAGE: &str = "Usage: tickloop monitor attach
       tickloop monitor popup install --herdr-socket SOCKET
       tickloop monitor popup remove --herdr-socket SOCKET";

#[derive(Debug, PartialEq, Eq)]
enum MonitorCommand {
    Attach,
    Popup {
        change: popup::PopupChange,
        herdr_socket: PathBuf,
    },
}

pub async fn run(arguments: &[OsString]) -> Result<(), String> {
    match parse(arguments)? {
        MonitorCommand::Attach => attach().await,
        MonitorCommand::Popup {
            change,
            herdr_socket,
        } => popup::run(change, &herdr_socket).await,
    }
}

fn parse(arguments: &[OsString]) -> Result<MonitorCommand, String> {
    match arguments {
        [command] if command == "attach" => Ok(MonitorCommand::Attach),
        [command, change, option, herdr_socket]
            if command == "popup" && option == "--herdr-socket" && !herdr_socket.is_empty() =>
        {
            let change = match change.to_str() {
                Some("install") => popup::PopupChange::Install,
                Some("remove") => popup::PopupChange::Remove,
                _ => return Err(MONITOR_USAGE.into()),
            };
            Ok(MonitorCommand::Popup {
                change,
                herdr_socket: herdr_socket.into(),
            })
        }
        _ => Err(MONITOR_USAGE.into()),
    }
}

/// Shows the hosted Tick its popup was granted in this terminal until the user detaches.
async fn attach() -> Result<(), String> {
    let grant: HostedViewGrant = std::env::var(ATTACH_ENVIRONMENT)
        .map_err(|_| format!("{ATTACH_ENVIRONMENT} names no view to attach"))
        .and_then(|grant| {
            serde_json::from_str(&grant)
                .map_err(|error| format!("{ATTACH_ENVIRONMENT} is not a view grant: {error}"))
        })?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("tickloop monitor attach needs an interactive terminal".into());
    }
    crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
    let ended = match run_view(&grant).await {
        Ok(AttachEnd::Detached) => Ok(()),
        Ok(AttachEnd::Gone) => Err("the Tick has ended".to_owned()),
        Ok(AttachEnd::Lost(reason)) => Err(reason),
        Err(error) => Err(error.to_string()),
    };
    match (ended, crossterm::terminal::disable_raw_mode()) {
        (ended, Ok(())) => ended,
        (Ok(()), Err(restore)) => Err(format!("cannot restore the terminal: {restore}")),
        (Err(error), Err(restore)) => {
            Err(format!("{error}; cannot restore the terminal: {restore}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(arguments: &[&str]) -> Result<MonitorCommand, String> {
        parse(&arguments.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn monitor_commands_take_exactly_their_documented_arguments() {
        assert_eq!(parsed(&["attach"]), Ok(MonitorCommand::Attach));
        assert_eq!(
            parsed(&["popup", "install", "--herdr-socket", "/run/herdr.sock"]),
            Ok(MonitorCommand::Popup {
                change: popup::PopupChange::Install,
                herdr_socket: "/run/herdr.sock".into(),
            })
        );
        assert_eq!(
            parsed(&["popup", "remove", "--herdr-socket", "/run/herdr.sock"]),
            Ok(MonitorCommand::Popup {
                change: popup::PopupChange::Remove,
                herdr_socket: "/run/herdr.sock".into(),
            })
        );
        for rejected in [
            &[][..],
            &["attach", "extra"],
            &["popup", "install"],
            &["popup", "install", "--herdr-socket"],
            &["popup", "install", "--herdr-socket", ""],
            &["popup", "install", "--socket", "/run/herdr.sock"],
            &["popup", "enable", "--herdr-socket", "/run/herdr.sock"],
            &[
                "popup",
                "install",
                "--herdr-socket",
                "/run/herdr.sock",
                "--yes",
            ],
            &["popup", "--herdr-socket", "/run/herdr.sock", "install"],
        ] {
            assert_eq!(parsed(rejected), Err(MONITOR_USAGE.into()), "{rejected:?}");
        }
    }

    #[test]
    fn the_usage_names_every_monitor_command() {
        for command in [
            "tickloop monitor attach",
            "tickloop monitor popup install --herdr-socket SOCKET",
            "tickloop monitor popup remove --herdr-socket SOCKET",
        ] {
            assert!(MONITOR_USAGE.contains(command), "{command}");
        }
    }
}
