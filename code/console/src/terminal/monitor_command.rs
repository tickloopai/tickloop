//! `tickloop monitor`: the attach client a Monitor popup or a Herdr view Pane runs, and the
//! explicit commands that install or remove that popup's plugin in one Herdr session.

mod popup;

use std::{
    ffi::OsString,
    io::{IsTerminal, Read, Write},
    path::PathBuf,
};

use crossterm::style::Stylize;
use tickhost_connect::herdr::{
    HerdrContext, SocketHerdr, VIEW_TOKEN_RENEWAL, current_pane, report_view,
};

use crate::terminal::attach::{AttachEnd, run_view};
use tickloop_harness_protocol::{ATTACH_ENVIRONMENT, HostedViewGrant, VIEW_PANE_ENVIRONMENT};

const MONITOR_USAGE: &str = "Usage: tickloop monitor attach
       tickloop monitor popup install --herdr-socket SOCKET
       tickloop monitor popup remove --herdr-socket SOCKET";
const CLOSE_HINT: &str = "Press any key to close";

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

/// Shows the Tick its popup or view Pane was granted in this terminal until the user detaches.
/// When it cannot, the reason stays on screen until a key is pressed, so the view never just
/// vanishes.
async fn attach() -> Result<(), String> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("tickloop monitor attach needs an interactive terminal".into());
    }
    crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
    let ended = show_granted_view().await;
    if let Err(reason) = &ended {
        hold_until_a_key(reason);
    }
    match (ended, crossterm::terminal::disable_raw_mode()) {
        (ended, Ok(())) => ended,
        (Ok(()), Err(restore)) => Err(format!("cannot restore the terminal: {restore}")),
        (Err(error), Err(restore)) => {
            Err(format!("{error}; cannot restore the terminal: {restore}"))
        }
    }
}

async fn show_granted_view() -> Result<(), String> {
    let grant: HostedViewGrant = std::env::var(ATTACH_ENVIRONMENT)
        .map_err(|_| format!("{ATTACH_ENVIRONMENT} names no view to attach"))
        .and_then(|grant| {
            serde_json::from_str(&grant)
                .map_err(|error| format!("{ATTACH_ENVIRONMENT} is not a view grant: {error}"))
        })?;
    let view_pane = ViewPane::from_environment()?;
    let ended = match &view_pane {
        Some(view_pane) => tokio::select! {
            ended = run_view(&grant) => ended,
            never = view_pane.report(grant.instance.as_str()) => match never {},
        },
        None => run_view(&grant).await,
    };
    if let Some(view_pane) = &view_pane {
        view_pane.clear().await;
    }
    match ended {
        Ok(AttachEnd::Detached) => Ok(()),
        Ok(AttachEnd::Gone) => Err("the Tick has ended".to_owned()),
        Ok(AttachEnd::Lost(reason)) => Err(reason),
        Err(error) => Err(error.to_string()),
    }
}

/// The Herdr Pane a Worker placed this attach client in as a view. While the view is attached, it
/// names the Tick it shows in the Pane's metadata and renews that before it lapses, so the Worker
/// recognises the Pane, also after the Worker restarted.
struct ViewPane {
    context: HerdrContext,
    host: SocketHerdr,
}

impl ViewPane {
    fn from_environment() -> Result<Option<Self>, String> {
        if std::env::var_os(VIEW_PANE_ENVIRONMENT).is_none() {
            return Ok(None);
        }
        Ok(HerdrContext::from_environment()?.map(|context| Self {
            host: SocketHerdr::new(context.clone()),
            context,
        }))
    }

    /// Reports `instance` on this Pane now and at each renewal. A report Herdr refuses is only
    /// missed; the view goes on.
    async fn report(&self, instance: &str) -> std::convert::Infallible {
        loop {
            if let Ok(pane) = current_pane(&self.host, &self.context.caller_pane).await {
                let _ = report_view(&self.host, &pane.pane_id, Some(instance)).await;
            }
            tokio::time::sleep(VIEW_TOKEN_RENEWAL).await;
        }
    }

    async fn clear(&self) {
        if let Ok(pane) = current_pane(&self.host, &self.context.caller_pane).await {
            let _ = report_view(&self.host, &pane.pane_id, None).await;
        }
    }
}

/// Shows `reason` in this raw-mode terminal and waits for one key.
fn hold_until_a_key(reason: &str) {
    let reason = crate::terminal::safe_text(reason).replace('\n', "\r\n");
    let mut output = std::io::stdout().lock();
    let _ = write!(
        output,
        "\r\n{}\r\n\r\n{CLOSE_HINT}",
        format!("Error: {reason}").red().bold()
    )
    .and_then(|()| output.flush());
    drop(output);
    let _ = std::io::stdin().read(&mut [0; 1]);
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
