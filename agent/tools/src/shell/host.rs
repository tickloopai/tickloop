//! Where shell commands run. The Registry decides what runs and follows it; a [`ShellHost`]
//! starts what the Registry checked last and reports how it ended. The Agent's host is the
//! Tickhost.

use std::path::PathBuf;

use tickloop_agent_protocol::CommandState;
use tokio_util::sync::CancellationToken;
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
    /// Cancelled once the command is stopped or its operation cancelled. A host that sees it
    /// before dispatching the start dispatches nothing.
    pub cancel: CancellationToken,
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

/// Why a command was not started as asked.
#[derive(Debug)]
pub enum ShellStartError {
    /// Nothing was dispatched, so nothing runs; [`ToolError::Cancelled`] when the launch was
    /// cancelled first.
    NotStarted(ToolError),
    /// The start was dispatched and its answer lost: the command may have run, and running it
    /// again may repeat what it did.
    OutcomeUnknown(String),
}

#[async_trait::async_trait]
pub trait ShellHost: Send + Sync {
    async fn start(&self, launch: ShellLaunch) -> Result<Box<dyn ShellProcess>, ShellStartError>;
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
    async fn start(&self, _: ShellLaunch) -> Result<Box<dyn ShellProcess>, ShellStartError> {
        Err(ShellStartError::NotStarted(ToolError::Rejected(
            "shell commands run in the Tickhost, which this Agent does not have".into(),
        )))
    }
}
