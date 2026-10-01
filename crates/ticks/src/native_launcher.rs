use std::{ffi::OsString, io::Write, os::unix::fs::PermissionsExt};

use serde_json::json;
use tick_connect::{
    access::TickAccess,
    herdr::{HerdrContext, HerdrPort, SocketHerdr},
    session::{SessionLaunch, TickSession},
};
use tokio_util::sync::CancellationToken;

use crate::{
    cli::{Cli, Command},
    error::{Error, Result},
};

const METADATA_TTL_MS: u64 = 30_000;
const METADATA_RENEWAL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub fn preflight(cli: &Cli) -> Result<()> {
    cli.global
        .settings()
        .map_err(|error| Error::usage(error.to_string()))?;
    if cli.command.is_once() {
        return Err(Error::usage(
            "opening a visible Tick does not accept --once",
        ));
    }
    match &cli.command {
        Command::Demo(_) => Ok(()),
        Command::Reader(args) => {
            let cwd = std::env::current_dir().map_err(|error| Error::usage(error.to_string()))?;
            for document in &args.documents {
                crate::reader::validate_location(document, &cwd)?;
            }
            Ok(())
        }
        Command::Options(args) => {
            crate::options::OptionsRequest::new(
                &args.symbol,
                args.expiry.as_deref(),
                args.strikes,
            )?;
            crate::futu::FutuOptions::lazy(&args.host, args.port)?;
            Ok(())
        }
        Command::Run { command } => check_command(command),
        Command::Describe | Command::Auth { .. } | Command::ProbeFundamentals(_) => {
            Err(Error::usage("select a live Tick command"))
        }
        _ => crate::auth::check_noninteractive(&crate::auth::default_storage()?),
    }
}

fn check_command(command: &[String]) -> Result<()> {
    let program = command
        .first()
        .ok_or_else(|| Error::usage("tick run requires a command after --"))?;
    let candidates = if program.contains('/') {
        vec![std::path::PathBuf::from(program)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|path| path.join(program))
            .collect()
    };
    if candidates.iter().any(|path| {
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }) {
        Ok(())
    } else {
        Err(Error::usage(format!("executable not found: {program}")))
    }
}

pub async fn maybe_launch(cli: &Cli, arguments: &[OsString]) -> Option<Result<()>> {
    let external = matches!(cli.command, Command::Run { .. });
    if std::env::var_os(tick_connect::protocol::INSTANCE_ID_ENV).is_some() {
        return None;
    }
    if !external
        && (cli.command.is_once()
            || matches!(
                cli.command,
                Command::Describe | Command::Auth { .. } | Command::ProbeFundamentals(_)
            ))
    {
        return None;
    }
    let context = match HerdrContext::from_environment() {
        Ok(Some(context)) => context,
        Ok(None) if !external => return None,
        Ok(None) => {
            return Some(Err(Error::usage(
                "tick run requires a Herdr Pane; standalone programs can use their own stdio entry",
            )));
        }
        Err(error) => return Some(Err(Error::usage(error))),
    };
    let host = SocketHerdr::new(context.clone());
    let current = match host
        .request(
            "pane.current",
            json!({"caller_pane_id":context.caller_pane}),
            CancellationToken::new(),
        )
        .await
    {
        Ok(current) => current,
        Err(error) => return Some(Err(Error::usage(error.to_string()))),
    };
    let process = match host
        .request(
            "pane.process_info",
            json!({"pane_id":current["pane"]["pane_id"]}),
            CancellationToken::new(),
        )
        .await
    {
        Ok(process) => process,
        Err(error) => return Some(Err(Error::usage(error.to_string()))),
    };
    // Inherited environment in a test PTY or nested terminal is not a Herdr Pane association.
    let process_group = unsafe { libc::getpgrp() } as u64;
    if process["process_info"]["foreground_process_group_id"].as_u64() != Some(process_group) {
        return if external {
            Some(Err(Error::usage(
                "tick run must run in the foreground of its Herdr Pane",
            )))
        } else {
            None
        };
    }
    Some(launch(cli, arguments, context).await)
}

async fn launch(cli: &Cli, arguments: &[OsString], context: HerdrContext) -> Result<()> {
    preflight(cli)?;
    let _terminal_restore = TerminalRestore::capture();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| Error::usage(error.to_string()))?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|error| Error::usage(error.to_string()))?;
    let command = if let Command::Run { command } = &cli.command {
        command.clone()
    } else {
        let executable =
            std::env::current_exe().map_err(|error| Error::usage(error.to_string()))?;
        std::iter::once(executable.into_os_string())
            .chain(arguments.iter().cloned())
            .map(|arg| {
                arg.into_string()
                    .map_err(|_| Error::usage("Native Tick arguments must be valid UTF-8"))
            })
            .collect::<Result<Vec<_>>>()?
    };
    let pane = TickAccess::new(context.clone())
        .caller()
        .await
        .map_err(|error| Error::usage(error.to_string()))?;
    let session = TickSession::spawn(SessionLaunch {
        command,
        cwd: std::env::current_dir().map_err(|error| Error::usage(error.to_string()))?,
        host_socket: context.socket.clone(),
        terminal_id: pane.terminal_id.clone(),
        runtime_dir: None,
    })
    .await
    .map_err(|error| Error::usage(error.to_string()))?;
    let instance = session.instance().to_string();
    let caller_pane = context.caller_pane.clone();
    let host = SocketHerdr::new(context);
    host.request("pane.report_metadata", json!({"pane_id":pane.pane_id,"source":"tickloop","tokens":{"tickloop_instance":instance},"ttl_ms":METADATA_TTL_MS}), CancellationToken::new()).await
        .map_err(|error| Error::usage(error.to_string()))?;
    let renew = async {
        loop {
            tokio::time::sleep(METADATA_RENEWAL_INTERVAL).await;
            if let Ok(current) = host
                .request(
                    "pane.current",
                    json!({"caller_pane_id":caller_pane}),
                    CancellationToken::new(),
                )
                .await
                && current["pane"]["terminal_id"] == pane.terminal_id
                && current["pane"]["tokens"]["tickloop_instance"]
                    .as_str()
                    .is_none_or(|owner| owner == instance)
            {
                let _ = host.request("pane.report_metadata", json!({"pane_id":current["pane"]["pane_id"],"source":"tickloop","tokens":{"tickloop_instance":instance},"ttl_ms":METADATA_TTL_MS}), CancellationToken::new()).await;
            }
        }
    };
    let result = tokio::select! {
        result = session.wait() => result.map_err(|error| Error::usage(error.to_string())).and_then(|status| if status.success() {Ok(())} else {Err(Error::usage(format!("Tick exited with {status}")))}),
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
        _ = renew => unreachable!(),
    };
    if let Ok(current) = host
        .request(
            "pane.current",
            json!({"caller_pane_id":caller_pane}),
            CancellationToken::new(),
        )
        .await
        && current["pane"]["terminal_id"] == pane.terminal_id
        && current["pane"]["tokens"]["tickloop_instance"] == instance
    {
        let _ = host.request("pane.report_metadata", json!({"pane_id":current["pane"]["pane_id"],"source":"tickloop","tokens":{"tickloop_instance":null}}), CancellationToken::new()).await;
    }
    result
}

struct TerminalRestore(Option<libc::termios>);

impl TerminalRestore {
    fn capture() -> Self {
        let mut modes = std::mem::MaybeUninit::uninit();
        // stderr retains the controlling terminal while the child's stdin/stdout carry JSON-RPC.
        let modes = unsafe {
            if libc::tcgetattr(libc::STDERR_FILENO, modes.as_mut_ptr()) == 0 {
                Some(modes.assume_init())
            } else {
                None
            }
        };
        Self(modes)
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        if let Some(modes) = &self.0 {
            // The wrapper restores its terminal even when its child was killed before Rust cleanup.
            unsafe {
                libc::tcsetattr(libc::STDERR_FILENO, libc::TCSANOW, modes);
            }
            let _ = std::io::stderr().write_all(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m");
        }
    }
}
