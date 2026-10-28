//! `tickloop monitor attach`: the attach client a Herdr view Pane runs.

use std::{
    ffi::OsString,
    io::{IsTerminal, Read, Write},
};

use crossterm::style::Stylize;
use tickhost_connect::herdr::{
    HerdrContext, SocketHerdr, VIEW_TOKEN_RENEWAL, current_pane, report_view,
};

use crate::terminal::attach::{AttachEnd, run_view};
use tickloop_harness::protocol::{ATTACH_ENVIRONMENT, HostedViewGrant, VIEW_PANE_ENVIRONMENT};

const MONITOR_USAGE: &str = "Usage: tickloop monitor attach";
const CLOSE_HINT: &str = "Press any key to close";

pub async fn run(arguments: &[OsString]) -> Result<(), String> {
    match arguments {
        [command] if command == "attach" => attach().await,
        _ => Err(MONITOR_USAGE.into()),
    }
}

/// Shows the Tick its view Pane was granted in this terminal until the user detaches.
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
