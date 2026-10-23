//! The `tickhost` command: `stop` ends every Tick the host runs; the hidden `run` is the service
//! the Agent daemon starts, and the hidden `launch` the launcher the host puts in front of each
//! protocol program it opens.

use std::{ffi::OsString, os::unix::fs::PermissionsExt, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use tickhost_connect::{hosted::BUILD_VERSION, launch::Launch};

use crate::{host, paths::HostPaths};

/// The hidden entry the Agent daemon starts the service with.
pub const RUN_COMMAND: &str = "run";
/// The hidden entry that runs a protocol program for a launch the host started; a person running
/// it gets a refusal.
pub const LAUNCH_COMMAND: &str = "launch";
const USAGE: u8 = 2;
/// How long `stop` waits for the host to end its Ticks: each gets the host's terminate grace,
/// side by side.
const STOP_WAIT: Duration = Duration::from_secs(10);
const KILL_WAIT: Duration = Duration::from_secs(2);
const STOP_POLL: Duration = Duration::from_millis(50);

#[derive(Parser, Debug)]
#[command(
    name = "tickhost",
    version = BUILD_VERSION,
    about = "Tickloop's Tick host: runs the Ticks Agents open; Herdr Panes, the Monitor and terminals only show them"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// End every Tick by stopping the Tickhost. The next Tick an Agent opens starts a new
    /// Tickhost; the stopped Ticks show as lost and are reopened only on request
    Stop,
    /// Serve Ticks with the configuration the Agent daemon writes on standard input
    #[command(hide = true)]
    Run,
    /// Run PROGRAM as the extension Tick of a launch the Tickhost started
    #[command(name = LAUNCH_COMMAND, hide = true)]
    Launch {
        #[arg(last = true, required = true, num_args = 1.., value_name = "PROGRAM")]
        command: Vec<String>,
    },
}

pub fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    let cli =
        match Cli::try_parse_from(std::iter::once(OsString::from("tickhost")).chain(arguments)) {
            Ok(cli) => cli,
            Err(error) => {
                let code = error.exit_code();
                let _ = error.print();
                return ExitCode::from(code as u8);
            }
        };
    let result = runtime().and_then(|runtime| {
        runtime.block_on(async {
            match cli.command {
                Command::Stop => stop().await,
                Command::Run => serve().await,
                Command::Launch { command } => launch(command).await,
            }
        })
    });
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

/// Refuses a program that is not an executable file, before it starts.
fn check_program(command: &[String]) -> Result<(), String> {
    let program = command
        .first()
        .ok_or("tickhost launch requires a command after --")?;
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

/// Runs `command` for the launch the host started this process for, once it proved to be the
/// foreground of the host's terminal.
async fn launch(command: Vec<String>) -> Result<(), String> {
    let launch = Launch::from_environment()
        .await
        .map_err(|error| error.to_string())?;
    check_program(&command)?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let status = tickhost_connect::launch::run(launch, command, cwd)
        .await
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Tick exited with {status}"))
    }
}

/// Ends the verified host with SIGTERM and waits for that process, not just its lock.
async fn stop() -> Result<(), String> {
    let paths = HostPaths::from_environment();
    let Some((pid, identity)) = verified_host_process(&paths)? else {
        println!("No Tickhost is running.");
        return Ok(());
    };
    let socket_identity = crate::paths::file_identity(&paths.socket()).ok();
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("cannot stop Tickhost process {pid}: {error}"));
        }
    }
    let forced = !wait_for_exit(pid, identity, STOP_WAIT).await?;
    if forced {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("cannot force-stop Tickhost process {pid}: {error}"));
            }
        }
        if !wait_for_exit(pid, identity, KILL_WAIT).await? {
            return Err(format!("Tickhost process {pid} did not exit after SIGKILL"));
        }
    }
    if !paths.host_running().map_err(|error| error.to_string())?
        && let Some(identity) = socket_identity
    {
        crate::paths::remove_if_identity(&paths.socket(), identity);
    }
    if forced {
        return Err(format!(
            "Tickhost process {pid} did not stop within {}s and required SIGKILL; the host has exited, but Tick cleanup could not be confirmed",
            STOP_WAIT.as_secs()
        ));
    }
    println!("Stopped the Tickhost; its Ticks have ended.");
    Ok(())
}

async fn wait_for_exit(
    pid: u32,
    identity: crate::process::ProcessIdentity,
    timeout: Duration,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let current = crate::process::running_identity(pid)
            .map_err(|error| format!("cannot inspect Tickhost process {pid}: {error}"))?;
        if current != Some(identity) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}

/// The pid of the live host, proven before anything signals it: the lock is held, the process it
/// names was started as `tickhost run`, and the lock still names it afterwards. A host lock that
/// names any other process, or one whose arguments cannot be read, is refused.
fn verified_host_process(
    paths: &HostPaths,
) -> Result<Option<(u32, crate::process::ProcessIdentity)>, String> {
    let read = || {
        paths
            .running_host_pid()
            .map_err(|error| format!("cannot read the Tickhost state: {error}"))
    };
    let Some(pid) = read()? else {
        return Ok(None);
    };
    let process_identity = || {
        crate::process::running_identity(pid)
            .map_err(|error| format!("cannot inspect Tickhost process {pid}: {error}"))
    };
    let Some(identity) = process_identity()? else {
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
    if read()? != Some(pid) || process_identity()? != Some(identity) {
        return Err(
            "the Tickhost changed while it was being stopped; run `tickhost stop` again".into(),
        );
    }
    Ok(Some((pid, identity)))
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
