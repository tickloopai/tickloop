//! The launch contract of an extension Tick in the Tickhost. The host starts its launcher on the
//! terminal it holds; the launcher runs the business Tick under a TickSession only after proving
//! it is the foreground process group of that terminal, and reports the running instance back.

use std::{io::Write, os::fd::AsRawFd, path::PathBuf, process::ExitStatus};

use serde_json::json;

use crate::{
    host_error::HostError,
    hosted::{self, CAPABILITY_LAUNCH, HostConnection, LaunchBinding, Role, TerminalDevice},
    session::{SessionLaunch, TickSession},
};

/// A launch the Tickhost started this process for, proven to run in the foreground of the
/// host's terminal.
pub struct Launch {
    binding: LaunchBinding,
    connection: HostConnection,
}

impl Launch {
    /// The launch the environment names, once this process proved to the host that it is the
    /// foreground of the host's terminal. A process the host did not start has no launch.
    pub async fn from_environment() -> Result<Self, HostError> {
        let binding = LaunchBinding::from_environment()
            .map_err(|error| HostError::before("InvalidLaunch", error))?
            .ok_or_else(|| {
                HostError::before(
                    "NotLaunched",
                    "only the Tickhost starts a launcher; an Agent opens a protocol program with ModifyTick and protocol: true",
                )
            })?;
        let terminal = std::io::stderr();
        let descriptor = terminal.as_raw_fd();
        let group = unsafe { libc::getpgrp() };
        if unsafe { libc::isatty(descriptor) } != 1
            || unsafe { libc::tcgetpgrp(descriptor) } != group
        {
            return Err(HostError::before(
                "NotForeground",
                "the hosted launcher must be the foreground of its terminal",
            ));
        }
        let device = TerminalDevice::of(descriptor)
            .map_err(|error| HostError::before("NotForeground", error.to_string()))?;
        let mut connection = HostConnection::connect(
            &binding.terminal.endpoint,
            Role::Launcher {
                operation: binding.operation,
                launch_token: binding.launch_token.clone(),
            },
            &[CAPABILITY_LAUNCH],
        )
        .await?;
        if connection.epoch() != binding.terminal.epoch {
            return Err(HostError::before(
                "EpochChanged",
                "the Tickhost restarted after this launch was prepared",
            ));
        }
        connection
            .request(
                "verify",
                json!({"device":device,"process_group":group}),
                hosted::CONTROL_TIMEOUT,
            )
            .await?;
        Ok(Self {
            binding,
            connection,
        })
    }

    /// Registers the running instance with the business process the TickSession spawned.
    async fn report(&mut self, session: &TickSession) -> Result<(), HostError> {
        let business_pid = session.process_id().ok_or_else(|| {
            HostError::before("LaunchFailed", "the Tick exited before registration")
        })?;
        self.connection
            .request(
                "register",
                json!({"instance":session.instance().to_string(),"business_pid":business_pid}),
                hosted::CONTROL_TIMEOUT,
            )
            .await
            .map(|_| ())
    }
}

/// Runs `command` as the business Tick behind a TickSession on the verified terminal, registers
/// the instance with the host and waits until the Tick exits or the terminal goes away. The
/// launcher's terminal modes are restored even when the Tick was killed.
pub async fn run(
    mut launch: Launch,
    command: Vec<String>,
    cwd: PathBuf,
) -> Result<ExitStatus, HostError> {
    let _terminal = TerminalRestore::capture();
    let signal_error = |error: std::io::Error| HostError::before("LaunchFailed", error.to_string());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(signal_error)?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(signal_error)?;
    let session = TickSession::spawn(SessionLaunch {
        command,
        cwd,
        terminal: launch.binding.terminal.clone(),
        runtime_dir: None,
    })
    .await
    .map_err(|error| HostError::before("LaunchFailed", error.to_string()))?;
    launch.report(&session).await?;
    tokio::select! {
        status = session.wait() => status.map_err(|error| HostError::after("TickFailed", error.to_string())),
        _ = tokio::signal::ctrl_c() => Ok(ExitStatus::default()),
        _ = terminate.recv() => Ok(ExitStatus::default()),
        _ = hangup.recv() => Ok(ExitStatus::default()),
    }
}

/// What the launcher writes to put its terminal back the way the business Tick may have left it:
/// mouse, paste and alternate screen modes off, cursor shown, attributes reset. The leading DECSC
/// saves the cursor of the screen in use, so leaving an alternate screen the Tick never entered
/// keeps the cursor below its output instead of moving it to the top left, where the launcher's
/// own failure would overwrite the Tick's last words. A Tick stopped inside the alternate screen
/// still returns to where it entered it: that screen keeps its own saved cursor.
const TERMINAL_RESTORE: &[u8] =
    b"\x1b7\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m";

struct TerminalRestore(Option<libc::termios>);

impl TerminalRestore {
    fn capture() -> Self {
        let mut modes = std::mem::MaybeUninit::uninit();
        // stderr keeps the controlling terminal while the child's stdin/stdout carry JSON-RPC.
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
            // The launcher restores its terminal even when its child was killed before cleanup.
            unsafe {
                libc::tcsetattr(libc::STDERR_FILENO, libc::TCSANOW, modes);
            }
            let _ = std::io::stderr().write_all(TERMINAL_RESTORE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TERMINAL_RESTORE;

    const LAUNCHER_FAILURE: &str = "tickhost: LaunchFailed: Tick session is gone\r\n";

    fn rows(output: &[&[u8]]) -> Vec<String> {
        let mut terminal = vt100::Parser::new(24, 80, 0);
        for bytes in output {
            terminal.process(bytes);
        }
        terminal
            .screen()
            .rows(0, 80)
            .take_while(|row| !row.is_empty())
            .collect()
    }

    #[test]
    fn the_launchers_failure_follows_the_words_of_a_tick_that_never_took_the_alternate_screen() {
        let reason = "tick-sample: not authorized; run `tick-sample auth`";
        assert_eq!(
            rows(&[
                format!("{reason}\r\n").as_bytes(),
                TERMINAL_RESTORE,
                LAUNCHER_FAILURE.as_bytes(),
            ]),
            [reason, LAUNCHER_FAILURE.trim_end()]
        );
    }

    #[test]
    fn a_tick_stopped_inside_the_alternate_screen_returns_to_where_it_entered_it() {
        assert_eq!(
            rows(&[
                b"before\r\n\x1b[?1049h\x1b[5;10Hfull screen view",
                TERMINAL_RESTORE,
                LAUNCHER_FAILURE.as_bytes(),
            ]),
            ["before", LAUNCHER_FAILURE.trim_end()]
        );
    }
}
