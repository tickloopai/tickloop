use std::{ffi::OsString, os::unix::fs::PermissionsExt};

use tick_connect::launch::{LaunchContext, LaunchHost};

use crate::{
    cli::{Cli, Command},
    error::{Error, Result},
};

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
    let context = match LaunchContext::from_environment() {
        Ok(Some(context)) => context,
        Ok(None) if !external => return None,
        Ok(None) => {
            return Some(Err(Error::usage(
                "tick run requires a Herdr Pane or a Tickhost launch; standalone programs can use their own stdio entry",
            )));
        }
        Err(error) => return Some(Err(Error::usage(error))),
    };
    let inherited_herdr = matches!(context, LaunchContext::Herdr(_));
    let host = match context.verify_foreground().await {
        Ok(host) => host,
        // Inherited Herdr variables in a test PTY or nested terminal are not a Pane association;
        // a hosted launch always reports, so it never falls back to running unannounced.
        Err(error) if error.code == "NotForeground" && inherited_herdr && !external => {
            return None;
        }
        Err(error) => return Some(Err(Error::usage(error.to_string()))),
    };
    Some(launch(cli, arguments, host).await)
}

async fn launch(cli: &Cli, arguments: &[OsString], host: LaunchHost) -> Result<()> {
    preflight(cli)?;
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
    let cwd = std::env::current_dir().map_err(|error| Error::usage(error.to_string()))?;
    let status = tick_connect::launch::run(host, command, cwd)
        .await
        .map_err(|error| Error::usage(error.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::usage(format!("Tick exited with {status}")))
    }
}
