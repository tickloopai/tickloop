use std::ffi::OsString;

use tick_provider::error::{Error, Result};
use tickhost_connect::launch::{LaunchContext, LaunchHost};

use crate::cli::{Cli, Command, LongbridgeCommand, TickCommand};

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
        Command::Builtin { .. } => Ok(()),
        Command::Reader(args) => {
            let cwd = std::env::current_dir().map_err(|error| Error::usage(error.to_string()))?;
            for document in &args.documents {
                tick_market::reader::validate_location(document, &cwd)?;
            }
            Ok(())
        }
        Command::Futu { connection, tick } => {
            if let TickCommand::Options(args) = tick {
                tick_market::options::OptionsRequest::new(
                    &args.symbol,
                    args.expiry.as_deref(),
                    args.strikes,
                )?;
            }
            tick_futu::FutuOptions::lazy(&connection.host, connection.port)?;
            Ok(())
        }
        Command::Longbridge {
            command: LongbridgeCommand::Tick(_),
        } => {
            tick_longbridge::auth::check_noninteractive(&tick_longbridge::auth::default_storage()?)
        }
        Command::Describe | Command::Longbridge { .. } => {
            Err(Error::usage("select a live Tick command"))
        }
    }
}

pub async fn maybe_launch(cli: &Cli, arguments: &[OsString]) -> Option<Result<()>> {
    if std::env::var_os(tick_protocol::INSTANCE_ID_ENV).is_some()
        || cli.command.is_once()
        || cli.command.is_separate_entry_point()
    {
        return None;
    }
    let context = match LaunchContext::from_environment() {
        Ok(Some(context)) => context,
        Ok(None) => return None,
        Err(error) => return Some(Err(Error::usage(error))),
    };
    let inherited_herdr = matches!(context, LaunchContext::Herdr(_));
    let host = match context.verify_foreground().await {
        Ok(host) => host,
        // Inherited Herdr variables in a test PTY or nested terminal are not a Pane association;
        // a hosted launch always reports, so it never falls back to running unannounced.
        Err(error) if error.code == "NotForeground" && inherited_herdr => {
            return None;
        }
        Err(error) => return Some(Err(Error::usage(error.to_string()))),
    };
    Some(launch(cli, arguments, host).await)
}

async fn launch(cli: &Cli, arguments: &[OsString], host: LaunchHost) -> Result<()> {
    preflight(cli)?;
    let executable = std::env::current_exe().map_err(|error| Error::usage(error.to_string()))?;
    let command = std::iter::once(executable.into_os_string())
        .chain(arguments.iter().cloned())
        .map(|arg| {
            arg.into_string()
                .map_err(|_| Error::usage("Native Tick arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    let cwd = std::env::current_dir().map_err(|error| Error::usage(error.to_string()))?;
    let status = tickhost_connect::launch::run(host, command, cwd)
        .await
        .map_err(|error| Error::usage(error.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::usage(format!("Tick exited with {status}")))
    }
}
