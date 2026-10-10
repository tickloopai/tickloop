//! Where shell commands run. The Registry decides what runs and follows it; a [`ShellHost`]
//! starts what the Registry checked last and reports how it ended. The Agent's host is the
//! Tickhost.

use std::path::PathBuf;

use tickloop_agent_protocol::CommandState;
use uuid::Uuid;

use crate::ToolError;

/// What a command starts with, fixed after the final check: the approved shell and command line,
/// the working directory opened then, and the output file its stdout and stderr append to.
pub struct ShellLaunch {
    pub operation_id: Uuid,
    pub shell: PathBuf,
    pub command: String,
    pub description: Option<String>,
    /// The approved directory's path, for describing the command only; it runs in `directory`.
    pub cwd: PathBuf,
    pub directory: std::fs::File,
    pub output: std::fs::File,
}

/// How a command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellExit {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    /// [`CommandState::Completed`], [`CommandState::Failed`] or [`CommandState::Unknown`].
    pub state: CommandState,
}

#[async_trait::async_trait]
pub trait ShellHost: Send + Sync {
    async fn start(&self, launch: ShellLaunch) -> Result<Box<dyn ShellProcess>, ToolError>;
}

/// A started command. Dropping it stops the command if it still runs.
#[async_trait::async_trait]
pub trait ShellProcess: Send {
    fn pid(&self) -> u32;

    /// How the command ended, once it did. Cancel-safe: waiting again picks up where it left.
    async fn exited(&mut self) -> ShellExit;

    /// Asks the command to stop: SIGTERM to its process group, SIGKILL 200 ms later.
    async fn stop(&mut self, reason: &str);
}

/// The host of a Tools made without one: it runs nothing.
pub(super) struct NoShellHost;

#[async_trait::async_trait]
impl ShellHost for NoShellHost {
    async fn start(&self, _: ShellLaunch) -> Result<Box<dyn ShellProcess>, ToolError> {
        Err(ToolError::Rejected(
            "shell commands run in the Tickhost, which this Agent does not have".into(),
        ))
    }
}
