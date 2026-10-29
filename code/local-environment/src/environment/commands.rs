//! One-shot commands the Worker runs itself, outside Tickhost and any sandbox. Each runs in a
//! process group of its own, which a timeout, cancellation or revocation ends as a whole. A
//! command is done once its shell exits and its output pipes close; a process it left running
//! with its own output is not stopped.

use std::{os::unix::process::ExitStatusExt, process::Stdio, time::Duration};

use tickloop_harness::protocol::ChatId;
use tickloop_harness::runtime::environment::{
    CapturedOutput, CommandOutput, CommandSpec, ExecutionAuthority, ExecutionError,
    ExecutionOutcome, ProgramExit,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SHELL: &str = "/bin/sh";
const TERMINATE_GRACE: Duration = Duration::from_millis(200);
const KILL_CONFIRMATION: Duration = Duration::from_secs(1);
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_millis(10);
const READ_CHUNK_BYTES: usize = 16 * 1024;

pub(super) fn check(spec: &CommandSpec) -> Result<(), ExecutionError> {
    if spec.command.trim().is_empty() {
        return Err(ExecutionError::new("InvalidInput", "command is empty"));
    }
    let cwd = std::path::Path::new(&spec.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(ExecutionError::new(
            "InvalidInput",
            format!("{} is not an existing absolute directory", spec.cwd),
        ));
    }
    Ok(())
}

enum Ending {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Interrupted(&'static str),
}

pub(super) async fn run(
    spec: &CommandSpec,
    operation: Uuid,
    authority: &ExecutionAuthority,
    chat: ChatId,
    cancel: &CancellationToken,
) -> ExecutionOutcome {
    if let Err(error) = authority.check(chat) {
        return ExecutionOutcome::NotDispatched(error);
    }
    if cancel.is_cancelled() {
        return ExecutionOutcome::NotDispatched(ExecutionError::new(
            "Cancelled",
            "execution cancelled before dispatch",
        ));
    }
    let mut child = match Command::new(SHELL)
        .arg("-c")
        .arg(&spec.command)
        .current_dir(&spec.cwd)
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ExecutionOutcome::NotDispatched(ExecutionError::new(
                "SpawnFailed",
                error.to_string(),
            ));
        }
    };
    let group = ProcessGroup::new(&child);
    let input = child.stdin.take().map(|mut stdin| {
        let input = spec.stdin.clone();
        // A command that never reads its input must not wait for it to be written.
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        })
    });
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let mut stdout = CapturedOutput::default();
    let mut stderr = CapturedOutput::default();
    let ending = {
        let completion = async {
            let (status, (), ()) = tokio::join!(
                child.wait(),
                capture(stdout_pipe, &mut stdout),
                capture(stderr_pipe, &mut stderr)
            );
            status
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Ending::Interrupted("command cancelled"),
            _ = authority.revoked() => Ending::Interrupted("authority revoked"),
            _ = tokio::time::sleep(spec.timeout) => Ending::TimedOut,
            status = completion => Ending::Exited(status),
        }
    };
    if let Some(input) = input {
        input.abort();
    }
    match ending {
        Ending::Exited(Ok(status)) => {
            group.release();
            ExecutionOutcome::Completed(
                CommandOutput {
                    exit: program_exit(status),
                    timed_out: false,
                    stdout,
                    stderr,
                }
                .into_result(),
            )
        }
        Ending::Exited(Err(error)) => {
            group.terminate(&mut child).await;
            ExecutionOutcome::OutcomeUnknown {
                operation,
                reason: format!("cannot wait for the command: {error}; it was stopped"),
            }
        }
        Ending::TimedOut => {
            let status = group.terminate(&mut child).await;
            ExecutionOutcome::Completed(
                CommandOutput {
                    exit: status.map_or(
                        ProgramExit {
                            code: None,
                            signal: None,
                        },
                        program_exit,
                    ),
                    timed_out: true,
                    stdout,
                    stderr,
                }
                .into_result(),
            )
        }
        Ending::Interrupted(reason) => {
            group.terminate(&mut child).await;
            ExecutionOutcome::OutcomeUnknown {
                operation,
                reason: format!("{reason} after dispatch; the command was stopped"),
            }
        }
    }
}

/// Reads a pipe to its end, keeping what the captured output keeps.
async fn capture(pipe: Option<impl AsyncRead + Unpin>, captured: &mut CapturedOutput) {
    let Some(mut pipe) = pipe else {
        return;
    };
    let mut buffer = vec![0; READ_CHUNK_BYTES];
    while let Ok(read @ 1..) = pipe.read(&mut buffer).await {
        captured.append(&buffer[..read]);
    }
}

fn program_exit(status: std::process::ExitStatus) -> ProgramExit {
    ProgramExit {
        code: status.code(),
        signal: status.signal(),
    }
}

/// The command's process group, killed when dropped unless released after a normal exit, so an
/// abandoned execution leaves nothing running.
struct ProcessGroup {
    id: Option<libc::pid_t>,
}

impl ProcessGroup {
    fn new(child: &Child) -> Self {
        Self {
            id: child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()),
        }
    }

    fn release(mut self) {
        self.id = None;
    }

    /// Asks the group to end, kills what is left after a grace period and reaps the shell.
    async fn terminate(mut self, child: &mut Child) -> Option<std::process::ExitStatus> {
        let group = self.id.take()?;
        signal_group(group, libc::SIGTERM);
        let deadline = tokio::time::Instant::now() + TERMINATE_GRACE;
        let status = tokio::time::timeout_at(deadline, child.wait()).await;
        while group_exists(group) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
        }
        if group_exists(group) {
            signal_group(group, libc::SIGKILL);
        }
        match status {
            Ok(status) => status.ok(),
            Err(_) => tokio::time::timeout(KILL_CONFIRMATION, child.wait())
                .await
                .ok()
                .and_then(Result::ok),
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(group) = self.id {
            signal_group(group, libc::SIGKILL);
        }
    }
}

fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    unsafe { libc::killpg(group, signal) };
}

fn group_exists(group: libc::pid_t) -> bool {
    (unsafe { libc::killpg(group, 0) } == 0)
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
