//! Where shell commands run. The Registry decides what runs and follows it; a [`ShellHost`]
//! starts what the Registry checked last and reports how it ended. The Agent's host is the
//! Tickhost.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tickloop_harness_protocol::CommandState;
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
    pub output_path: PathBuf,
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

#[derive(Debug)]
pub enum ShellStopError {
    NotDispatched(String),
    OutcomeUnknown(String),
}

pub type ShellStopAdmission = dyn Fn() -> Result<(), ShellStopError> + Send + Sync;

pub struct ShellPreparationRequest {
    pub additional_directories: Vec<PathBuf>,
    pub family_temp: Option<PathBuf>,
    pub operation: Uuid,
    pub shell: PathBuf,
    pub command: String,
    pub cwd: PathBuf,
    pub output: PathBuf,
    pub description: Option<String>,
}

#[async_trait::async_trait]
pub trait PreparedShellStart: Send + Sync {
    async fn start(
        self: Box<Self>,
        output: Option<std::fs::File>,
        cancel: CancellationToken,
    ) -> Result<Box<dyn ShellProcess>, ShellStartError>;
}

#[derive(Clone)]
pub struct ShellPreparation {
    operation: Uuid,
    creates_output: bool,
    isolation: tickloop_harness_protocol::ExecutionIsolation,
    action: Arc<Mutex<Option<Box<dyn PreparedShellStart>>>>,
}

impl std::fmt::Debug for ShellPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellPreparation")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

impl ShellPreparation {
    pub fn new(operation: Uuid, creates_output: bool, action: Box<dyn PreparedShellStart>) -> Self {
        Self {
            operation,
            creates_output,
            isolation: Default::default(),
            action: Arc::new(Mutex::new(Some(action))),
        }
    }
    pub fn with_isolation(
        mut self,
        isolation: tickloop_harness_protocol::ExecutionIsolation,
    ) -> Self {
        self.isolation = isolation;
        self
    }
    pub fn isolation(&self) -> tickloop_harness_protocol::ExecutionIsolation {
        self.isolation
    }
    pub fn operation(&self) -> Uuid {
        self.operation
    }
    pub fn creates_output(&self) -> bool {
        self.creates_output
    }
    pub async fn start(
        &self,
        output: Option<std::fs::File>,
        cancel: CancellationToken,
    ) -> Result<Box<dyn ShellProcess>, ShellStartError> {
        let action = self.action.lock().unwrap().take().ok_or_else(|| {
            ShellStartError::NotStarted(ToolError::Rejected(
                "Shell preparation was already consumed".into(),
            ))
        })?;
        action.start(output, cancel).await
    }
}

#[async_trait::async_trait]
pub trait PreparedShellStop: Send + Sync {
    async fn stop(self: Box<Self>, cancel: CancellationToken) -> Result<(), ShellStopError>;
}

#[derive(Clone)]
pub struct ShellStopPreparation {
    target: Uuid,
    action: Arc<Mutex<Option<Box<dyn PreparedShellStop>>>>,
}

impl std::fmt::Debug for ShellStopPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellStopPreparation")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl ShellStopPreparation {
    pub fn new(target: Uuid, action: Box<dyn PreparedShellStop>) -> Self {
        Self {
            target,
            action: Arc::new(Mutex::new(Some(action))),
        }
    }
    pub fn target(&self) -> Uuid {
        self.target
    }
    pub async fn stop(&self, cancel: CancellationToken) -> Result<(), ShellStopError> {
        let action = self.action.lock().unwrap().take().ok_or_else(|| {
            ShellStopError::NotDispatched("Shell stop preparation was already consumed".into())
        })?;
        action.stop(cancel).await
    }
}

#[async_trait::async_trait]
pub trait ShellHost: Send + Sync {
    async fn prepare_stop(
        &self,
        _task: Uuid,
        _reason: &str,
    ) -> Result<Option<ShellStopPreparation>, ToolError> {
        Ok(None)
    }
    async fn prepare(
        &self,
        _request: ShellPreparationRequest,
    ) -> Result<Option<ShellPreparation>, ToolError> {
        Ok(None)
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn start(&self, launch: ShellLaunch) -> Result<Box<dyn ShellProcess>, ShellStartError>;

    async fn adopt(&self, _handoff: serde_json::Value) -> Result<Box<dyn ShellProcess>, ToolError> {
        Err(ToolError::Rejected(
            "this Shell host cannot transfer running commands".into(),
        ))
    }

    async fn retained_handoff(
        &self,
        _operation: Uuid,
    ) -> Result<Option<serde_json::Value>, ToolError> {
        Ok(None)
    }
}

/// A started command. Dropping it stops the command if it still runs.
#[async_trait::async_trait]
pub trait ShellProcess: Send {
    fn pid(&self) -> u32;

    fn handoff(&self) -> Option<serde_json::Value> {
        None
    }

    /// How the command ended, once it did. Cancel-safe: waiting again picks up where it left.
    async fn exited(&mut self) -> ShellExit;

    /// Asks the command to stop: SIGTERM to its process group, SIGKILL 200 ms later.
    async fn stop(&mut self, reason: &str);

    async fn stop_checked(
        &mut self,
        reason: &str,
        before_dispatch: Option<&ShellStopAdmission>,
    ) -> Result<(), ShellStopError> {
        if let Some(admit) = before_dispatch {
            admit()?;
        }
        self.stop(reason).await;
        Ok(())
    }
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

/// The execution owner's observer of whether a completed Shell is still retained.
#[async_trait::async_trait]
pub trait ShellRetention: Send + Sync {
    fn spawn(&self, task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>);
    async fn is_retained(&self, chat: uuid::Uuid, task: uuid::Uuid) -> bool;
}

/// Live Shell ownership services used during a planned worker handoff.
#[async_trait::async_trait]
pub trait ShellRuntime: Send + Sync {
    fn attach(&self, retention: std::sync::Arc<dyn ShellRetention>);
    fn for_chat(self: std::sync::Arc<Self>, chat: uuid::Uuid) -> std::sync::Arc<dyn ShellHost>;
    async fn commit_handoffs(&self) -> Result<(), String> {
        Ok(())
    }
}
