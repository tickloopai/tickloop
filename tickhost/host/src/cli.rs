//! The `tickhost` command: `open` connects a protocol program to the host it runs in, `stop` ends
//! every hosted Tick, and the hidden `run` is the service the Agent daemon starts.

use std::{ffi::OsString, os::unix::fs::PermissionsExt, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use tickhost_connect::{
    hosted::BUILD_VERSION,
    launch::{LaunchContext, LaunchHost, PREFLIGHT_ARGUMENT},
};

use crate::{host, paths::HostPaths};

/// The hidden entry the Agent daemon starts the service with.
pub const RUN_COMMAND: &str = "run";
const USAGE: u8 = 2;
/// How long `stop` waits for the host to end its Ticks: each gets the host's terminate grace,
/// side by side.
const STOP_WAIT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(50);

#[derive(Parser, Debug)]
#[command(
    name = "tickhost",
    version = BUILD_VERSION,
    about = "Tickloop's Tick host: connects protocol programs to Agents and runs the Ticks Agents open hidden"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run PROGRAM as a Tick: its stdin/stdout speak the Tick protocol to Agents, from a Herdr
    /// Pane or from the Tickhost that started it
    Open {
        #[arg(last = true, required = true, num_args = 1.., value_name = "PROGRAM")]
        command: Vec<String>,
    },
    /// End every hosted Tick by stopping the Tickhost. The next Tick an Agent opens starts a new
    /// Tickhost; the stopped Ticks show as lost and are reopened only on request. Ticks in Herdr
    /// Panes keep running
    Stop,
    /// Serve hosted Ticks with the configuration the Agent daemon writes on standard input
    #[command(hide = true)]
    Run,
}

pub fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    let mut arguments: Vec<_> = arguments.into_iter().collect();
    let preflight = arguments
        .first()
        .is_some_and(|argument| argument == PREFLIGHT_ARGUMENT);
    if preflight {
        arguments.remove(0);
    }
    let cli =
        match Cli::try_parse_from(std::iter::once(OsString::from("tickhost")).chain(arguments)) {
            Ok(cli) => cli,
            Err(error) => {
                let code = error.exit_code();
                let _ = error.print();
                return ExitCode::from(code as u8);
            }
        };
    let result = match (preflight, cli.command) {
        (true, Command::Open { command }) => check_program(&command),
        (true, _) => Err("only open can be checked before it starts".into()),
        (false, Command::Open { command }) => {
            runtime().and_then(|runtime| runtime.block_on(open(command)))
        }
        (false, Command::Stop) => runtime().and_then(|runtime| runtime.block_on(stop())),
        (false, Command::Run) => runtime().and_then(|runtime| runtime.block_on(serve())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tickhost: {error}");
            ExitCode::from(USAGE)
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start async runtime: {error}"))
}

/// Refuses a program that is not an executable file, before any host starts it.
fn check_program(command: &[String]) -> Result<(), String> {
    let program = command
        .first()
        .ok_or("tickhost open requires a command after --")?;
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
        Err(format!("executable not found: {program}"))
    }
}

async fn open(command: Vec<String>) -> Result<(), String> {
    check_program(&command)?;
    let host: LaunchHost = LaunchContext::from_environment()?
        .ok_or(
            "tickhost open requires a Herdr Pane or a Tickhost launch; a program run alone can use its own stdio",
        )?
        .verify_foreground()
        .await
        .map_err(|error| error.to_string())?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let status = tickhost_connect::launch::run(host, command, cwd)
        .await
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Tick exited with {status}"))
    }
}

/// Ends the running host with SIGTERM, which ends each of its Ticks, and waits until its lock is
/// free. Without the empty-host mark, the daemon then counts those Ticks as lost.
async fn stop() -> Result<(), String> {
    let paths = HostPaths::from_environment();
    let Some(pid) = verified_host_pid(&paths)? else {
        println!("No Tickhost is running.");
        return Ok(());
    };
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("cannot stop Tickhost process {pid}: {error}"));
        }
    }
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    while paths.host_running().map_err(|error| error.to_string())? {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Tickhost process {pid} did not stop within {}s",
                STOP_WAIT.as_secs()
            ));
        }
        tokio::time::sleep(STOP_POLL).await;
    }
    println!("Stopped the Tickhost; its Ticks have ended.");
    Ok(())
}

/// The pid of the live host, proven before anything signals it: the lock is held, the process it
/// names was started as `tickhost run`, and the lock still names it afterwards. A host lock that
/// names any other process, or one whose arguments cannot be read, is refused.
fn verified_host_pid(paths: &HostPaths) -> Result<Option<u32>, String> {
    let read = || {
        paths
            .running_host_pid()
            .map_err(|error| format!("cannot read the Tickhost state: {error}"))
    };
    let Some(pid) = read()? else {
        return Ok(None);
    };
    let refusal = |reason: &str| {
        format!(
            "{} names process {pid}, which {reason}; refusing to signal it",
            paths.lock().display()
        )
    };
    let arguments = crate::process::arguments(pid).ok_or_else(|| refusal("cannot be inspected"))?;
    let is_service = matches!(
        arguments.as_slice(),
        [executable, command]
            if std::path::Path::new(executable).file_name() == Some("tickhost".as_ref())
                && command == RUN_COMMAND
    );
    if !is_service {
        return Err(refusal("is not `tickhost run`"));
    }
    if read()? != Some(pid) {
        return Err(
            "the Tickhost changed while it was being stopped; run `tickhost stop` again".into(),
        );
    }
    Ok(Some(pid))
}

async fn serve() -> Result<(), String> {
    let config = host::read_config(std::io::stdin().lock()).map_err(|error| error.to_string())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| format!("Tickhost: {error}"))?;
    host::serve_until(HostPaths::from_environment(), config, async move {
        terminate.recv().await;
    })
    .await
    .map_err(|error| format!("Tickhost: {error}"))
}
