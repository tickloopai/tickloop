//! Stopping the running Tickhost of a runtime directory from outside it: `tickhost stop` does it
//! on the user's word, and the Agent daemon does it to a host no daemon can control any more.

use std::time::Duration;

use crate::{
    cli::RUN_COMMAND,
    paths::{self, HostPaths},
    process::{self, ProcessIdentity},
};

/// How long a stop waits for the host to end its Ticks: each gets the host's terminate grace,
/// side by side.
pub(crate) const STOP_WAIT: Duration = Duration::from_secs(10);
const KILL_WAIT: Duration = Duration::from_secs(2);
const STOP_POLL: Duration = Duration::from_millis(50);
/// The longest a stop waits for the host to exit.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(STOP_WAIT.as_secs() + KILL_WAIT.as_secs());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStop {
    NotRunning,
    Stopped,
    /// The host did not end its Ticks within the stop wait and exited only on SIGKILL, so their
    /// cleanup is unconfirmed.
    Killed {
        pid: u32,
    },
}

/// Ends the verified host with SIGTERM and waits for that process, not just its lock; one that
/// outlasts the stop wait gets SIGKILL. The socket is removed only while it is still the one the
/// stopped host served.
pub async fn stop_host(paths: &HostPaths) -> Result<HostStop, String> {
    let Some((pid, identity)) = verified_host_process(paths)? else {
        return Ok(HostStop::NotRunning);
    };
    let socket_identity = paths::file_identity(&paths.socket()).ok();
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
        paths::remove_if_identity(&paths.socket(), identity);
    }
    Ok(if forced {
        HostStop::Killed { pid }
    } else {
        HostStop::Stopped
    })
}

async fn wait_for_exit(
    pid: u32,
    identity: ProcessIdentity,
    timeout: Duration,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let current = process::running_identity(pid)
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
fn verified_host_process(paths: &HostPaths) -> Result<Option<(u32, ProcessIdentity)>, String> {
    let read = || {
        paths
            .running_host_pid()
            .map_err(|error| format!("cannot read the Tickhost state: {error}"))
    };
    let Some(pid) = read()? else {
        return Ok(None);
    };
    let process_identity = || {
        process::running_identity(pid)
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
    let arguments = process::arguments(pid).ok_or_else(|| refusal("cannot be inspected"))?;
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
        return Err("the Tickhost changed while it was being stopped".into());
    }
    Ok(Some((pid, identity)))
}
