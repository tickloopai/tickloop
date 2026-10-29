use std::{collections::BTreeMap, sync::Arc, time::Duration};
pub use tickloop_harness_protocol::{ExecutionIsolation, NetworkIsolation, WriteIsolation};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tick_protocol::InstanceRef;
use tickloop_harness_protocol::{ChatId, PreparedApprovalFacts, ToolResult};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::files::{
    FileCapabilities, FileInspection, FileMetadata, FilePath, FileQuery, FileVersion, error_code,
};
use crate::host_tools::HostToolPermission;

pub const TERMINAL_INPUT_BYTES: usize = 4 * 1024;
pub const SHELL_OUTPUT_BYTES: usize = 64 * 1024;
/// The most bytes a one-shot command keeps of each output stream; the rest is only counted.
pub const COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionScope {
    pub chat: ChatId,
    /// A path in the execution environment, never resolved by the Harness.
    pub folder: Option<String>,
    /// Paths in the execution environment, never resolved by the Harness. A change binds a new
    /// Port.
    pub additional_directories: Vec<String>,
}

/// Issued by the embedding host. Revocation also invalidates already bound Ports.
#[derive(Clone)]
pub struct ExecutionAuthority {
    chat: ChatId,
    identity: Uuid,
    revoked: CancellationToken,
}

impl ExecutionAuthority {
    pub fn new(chat: ChatId) -> Self {
        Self {
            chat,
            identity: tickloop_ids::generate(),
            revoked: CancellationToken::new(),
        }
    }

    pub fn revoke(&self) {
        self.revoked.cancel();
    }

    pub async fn revoked(&self) {
        self.revoked.cancelled().await;
    }

    pub fn with_cancellation(chat: ChatId, cancelled: CancellationToken) -> Self {
        Self {
            chat,
            identity: tickloop_ids::generate(),
            revoked: cancelled,
        }
    }

    /// Records the Harness's admission of these immutable facts after its policy check.
    pub fn admit(
        &self,
        prepared: &PreparedExecution,
    ) -> Result<ExecutionAdmission, ExecutionError> {
        self.check(prepared.chat)?;
        if self.identity != prepared.authority {
            return Err(ExecutionError::new(
                "Unauthorized",
                "preparation belongs to another authority",
            ));
        }
        Ok(ExecutionAdmission {
            preparation: prepared.preparation,
            operation: prepared.operation,
            authority: self.identity,
        })
    }

    pub fn check(&self, chat: ChatId) -> Result<(), ExecutionError> {
        if self.chat != chat {
            return Err(ExecutionError::new(
                "Unauthorized",
                "authority belongs to another Chat",
            ));
        }
        if self.revoked.is_cancelled() {
            return Err(ExecutionError::new(
                error_code::AUTHORITY_REVOKED,
                "execution authority is no longer valid",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ExecutionError {
    pub code: String,
    pub message: String,
}

impl ExecutionError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionCapabilities {
    pub ticks: bool,
    pub shells: bool,
    pub shell_output: ShellOutputDelivery,
    pub files: FileCapabilities,
}

pub enum ExecutionQuery {
    Capabilities,
    Instances,
    Receipt(Uuid),
    Preparation(PreparationId),
    Read(InstanceRef),
    Kind(InstanceRef),
    Describe(InstanceRef),
    Sample(ObservationCursor),
    ShellStatus(InstanceRef),
    ShellOutput {
        instance: InstanceRef,
        offset: u64,
        limit: usize,
    },
    File(FileQuery),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionIntent {
    Open(OpenSpec),
    Close {
        instance: InstanceRef,
    },
    Reopen {
        instance: InstanceRef,
    },
    SendInput {
        instance: InstanceRef,
        data: Vec<u8>,
    },
    SpawnShell(ShellSpec),
    StopShell {
        instance: InstanceRef,
        reason: String,
    },
    Call {
        instance: InstanceRef,
        tool: String,
        args: Value,
        revision: String,
    },
    RunCommand(CommandSpec),
    /// Replaces the file, creating its parent directories as needed. `expected` is the version
    /// it must still have, or `None` when it must not exist.
    WriteFile {
        path: FilePath,
        content: Vec<u8>,
        expected: Option<FileVersion>,
    },
    DeleteFile {
        path: FilePath,
        expected: FileVersion,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionKind {
    Protocol,
    Terminal,
    Shell,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionRead {
    pub kind: ExecutionKind,
    pub state: tick_protocol::TickState,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationCursor {
    pub instance: InstanceRef,
    pub epoch: Uuid,
    pub watch: Uuid,
    pub position: u64,
}

pub struct ObserveRequest {
    pub instance: InstanceRef,
    pub after: Option<ObservationCursor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExecutionLinkState {
    Live,
    Gone,
    Disconnected,
    ProtocolError(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutionUpdate {
    pub update: Option<tick_protocol::TickState>,
    pub received: u64,
    pub state: ExecutionLinkState,
    #[serde(default)]
    pub terminal: Option<TerminalChange>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalChange {
    pub dirty: bool,
    pub exited: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TerminalDescription {
    pub title: Option<String>,
    pub command: Vec<String>,
    pub cwd: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TerminalRead {
    pub seq: u64,
    pub at: String,
    pub text: String,
    pub truncated: bool,
    pub exit: Option<ProgramExit>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProgramExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl TerminalRead {
    pub fn data(&self) -> Value {
        serde_json::json!({"text":self.text, "exit":self.exit})
    }

    pub fn from_read(read: ExecutionRead) -> Result<Self, ExecutionError> {
        let text = read.state.data["text"]
            .as_str()
            .ok_or_else(|| {
                ExecutionError::new("ProtocolViolation", "terminal state has no screen text")
            })?
            .to_owned();
        let exit = serde_json::from_value(read.state.data["exit"].clone())
            .map_err(|error| ExecutionError::new("ProtocolViolation", error.to_string()))?;
        Ok(Self {
            seq: read.state.seq,
            at: read.state.at.unwrap_or_default(),
            text,
            truncated: read.truncated,
            exit,
        })
    }

    pub fn into_read(self) -> ExecutionRead {
        ExecutionRead {
            kind: ExecutionKind::Terminal,
            state: tick_protocol::TickState {
                seq: self.seq,
                data: self.data(),
                at: Some(self.at),
            },
            truncated: self.truncated,
        }
    }
}

pub struct ExecutionObservation {
    pub baseline: ExecutionRead,
    pub cursor: ObservationCursor,
    pub gap: bool,
    pub updates: tokio::sync::watch::Receiver<ExecutionUpdate>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenSpec {
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub protocol: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSpec {
    /// The trusted Chat Folder; cwd never grants write access.
    pub folder: Option<String>,
    pub dangerously_disable_sandbox: bool,
    pub additional_directories: Vec<String>,
    pub family_temp: Option<String>,
    pub operation: Uuid,
    pub shell: String,
    pub command: String,
    pub cwd: String,
    pub output: String,
    pub description: Option<String>,
}

/// A command run once to completion by `/bin/sh -c`. It is not a Shell task: it has no Shell
/// record, no Monitor row and no sandbox, and the environment ends its whole process group when
/// the timeout passes or the execution is cancelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: String,
    pub stdin: Vec<u8>,
    /// Added to the environment's own variables, replacing those with the same name.
    pub env: BTreeMap<String, String>,
    /// A path in the execution environment, never resolved by the Harness.
    pub cwd: String,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit: ProgramExit,
    /// The timeout passed and the environment ended the command's process group.
    pub timed_out: bool,
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
}

impl CommandOutput {
    pub fn into_result(self) -> ExecutionResult {
        let status = if self.timed_out {
            "timed_out"
        } else {
            "exited"
        };
        ExecutionResult {
            result: ToolResult {
                content: serde_json::json!({
                    "status": status,
                    "exit_code": self.exit.code,
                    "signal": self.exit.signal,
                })
                .to_string(),
                is_error: self.timed_out || self.exit.code != Some(0),
                output_ref: None,
            },
            opened: None,
            shell: None,
            command: Some(Box::new(self)),
            handoff: None,
            file: None,
        }
    }
}

/// The first [`COMMAND_OUTPUT_BYTES`] of an output stream and how many bytes it had in all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapturedOutput {
    data: Vec<u8>,
    total_bytes: u64,
}

impl CapturedOutput {
    pub fn new(bytes: &[u8]) -> Self {
        let mut captured = Self::default();
        captured.append(bytes);
        captured
    }

    pub fn append(&mut self, chunk: &[u8]) {
        let room = COMMAND_OUTPUT_BYTES.saturating_sub(self.data.len());
        self.data.extend_from_slice(&chunk[..room.min(chunk.len())]);
        self.total_bytes += chunk.len() as u64;
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn is_truncated(&self) -> bool {
        self.total_bytes > self.data.len() as u64
    }

    /// The kept bytes as text, followed by a notice when the stream was cut.
    pub fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.data).into_owned();
        if self.is_truncated() {
            text.push_str(&format!(
                "\n[output truncated: kept the first {} of {} bytes]",
                self.data.len(),
                self.total_bytes
            ));
        }
        text
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShellStatus {
    pub instance: InstanceRef,
    pub pid: Option<u32>,
    pub state: tickloop_harness_protocol::CommandState,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    pub output: String,
    pub output_bytes: u64,
    pub output_delivery: ShellOutputDelivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellOutputDelivery {
    CallerFile,
    Environment,
}

#[derive(Clone, PartialEq)]
pub struct ExecutionHandoff(Value);

impl ExecutionHandoff {
    pub fn new(value: Value) -> Self {
        Self(value)
    }
    pub fn into_value(self) -> Value {
        self.0
    }
}

impl std::fmt::Debug for ExecutionHandoff {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExecutionHandoff { .. }")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputPage {
    pub data: Vec<u8>,
    pub next_offset: u64,
    pub eof: bool,
}

pub enum PreparationKind {}
impl tickloop_ids::Kind for PreparationKind {
    const NOUN: &'static str = "execution preparation";
}
pub type PreparationId = tickloop_ids::Id<PreparationKind>;

pub struct ExecutionAdmission {
    preparation: PreparationId,
    operation: Uuid,
    authority: Uuid,
}

/// Immutable approval facts; only the issuing Adapter keeps the execution proof.
pub struct PreparedExecution {
    preparation: PreparationId,
    operation: Uuid,
    port: Uuid,
    authority: Uuid,
    chat: ChatId,
    facts: ExecutionFacts,
    isolation: ExecutionIsolation,
    skips_approval_when_isolated: bool,
    lifetime: Arc<()>,
    cleanup: Option<Box<dyn FnOnce() + Send + Sync>>,
}

#[derive(Clone, Debug)]
pub struct ExecutionFacts {
    pub intent: ExecutionIntent,
    pub normalized_input: Value,
    pub permission: HostToolPermission,
    pub summary: String,
    pub approval: PreparedApprovalFacts,
}

impl ExecutionFacts {
    /// The facts of a one-shot command. They name the variables it adds and the size of its
    /// input, never their values.
    pub fn for_command(spec: CommandSpec) -> Self {
        let normalized_input = serde_json::json!({
            "command": spec.command,
            "cwd": spec.cwd,
            "env": spec.env.keys().collect::<Vec<_>>(),
            "stdin_bytes": spec.stdin.len(),
            "timeout_ms": u64::try_from(spec.timeout.as_millis()).unwrap_or(u64::MAX),
        });
        Self {
            summary: format!("Run {}", spec.command),
            intent: ExecutionIntent::RunCommand(spec),
            permission: HostToolPermission::Reviewable,
            approval: PreparedApprovalFacts {
                normalized_input: normalized_input.clone(),
                ..Default::default()
            },
            normalized_input,
        }
    }
}

impl PreparedExecution {
    pub fn new(port: Uuid, authority: &ExecutionAuthority, facts: ExecutionFacts) -> Self {
        Self::for_operation(tickloop_ids::generate(), port, authority, facts)
    }

    pub fn for_operation(
        operation: Uuid,
        port: Uuid,
        authority: &ExecutionAuthority,
        facts: ExecutionFacts,
    ) -> Self {
        Self {
            preparation: PreparationId::new(),
            operation,
            port,
            authority: authority.identity,
            chat: authority.chat,
            facts,
            isolation: ExecutionIsolation::default(),
            skips_approval_when_isolated: false,
            lifetime: Arc::new(()),
            cleanup: None,
        }
    }

    pub fn with_required_approval(mut self, required: bool) -> Self {
        self.facts.approval.requires_approval = required.then_some(true);
        self
    }

    pub fn requires_approval(&self) -> bool {
        self.facts.approval.requires_approval.unwrap_or(false)
    }

    pub fn with_isolation(mut self, isolation: ExecutionIsolation) -> Self {
        self.isolation = isolation;
        self.facts.approval.isolation = Some(isolation);
        self
    }

    pub fn isolation(&self) -> ExecutionIsolation {
        self.isolation
    }

    /// Whether the environment's owner lets a sufficiently isolated execution skip ordinary
    /// approval; the Harness decides whether the reported isolation qualifies.
    pub fn with_approval_skipped_when_isolated(mut self, allowed: bool) -> Self {
        self.skips_approval_when_isolated = allowed;
        self
    }

    pub fn skips_approval_when_isolated(&self) -> bool {
        self.skips_approval_when_isolated
    }

    pub fn preparation(&self) -> PreparationId {
        self.preparation
    }

    pub fn operation(&self) -> Uuid {
        self.operation
    }
    pub fn with_cleanup(mut self, cleanup: impl FnOnce() + Send + Sync + 'static) -> Self {
        self.cleanup = Some(Box::new(cleanup));
        self
    }
    pub fn facts(&self) -> &ExecutionFacts {
        &self.facts
    }
    pub fn lifetime(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.lifetime)
    }

    pub fn validate(
        &self,
        port: Uuid,
        authority: &ExecutionAuthority,
        admission: &ExecutionAdmission,
    ) -> Result<(), ExecutionError> {
        authority.check(self.chat)?;
        if self.port != port
            || self.authority != authority.identity
            || admission.authority != self.authority
            || admission.operation != self.operation
            || admission.preparation != self.preparation
        {
            return Err(ExecutionError::new(
                "Unauthorized",
                "admission does not match the prepared operation",
            ));
        }
        Ok(())
    }
}

impl Drop for PreparedExecution {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpenedExecution {
    pub instance: InstanceRef,
    pub title: Option<String>,
    pub is_terminal: bool,
    /// The fewest terminal rows the Tick declared it stays usable at, when it declared any.
    pub min_rows: Option<u16>,
    /// The most terminal rows the Tick declared it can use, when it declared any.
    pub max_rows: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub result: ToolResult,
    pub opened: Option<OpenedExecution>,
    pub shell: Option<Box<ShellStatus>>,
    pub command: Option<Box<CommandOutput>>,
    pub handoff: Option<ExecutionHandoff>,
    /// The file a `WriteFile` left.
    pub file: Option<Box<FileMetadata>>,
}

#[derive(Debug)]
pub enum ExecutionOutcome {
    Completed(ExecutionResult),
    NotDispatched(ExecutionError),
    OutcomeUnknown { operation: Uuid, reason: String },
}

#[derive(Clone, Debug)]
pub enum ExecutionReceipt {
    Completed(ExecutionResult),
    NotDispatched(ExecutionError),
    Unknown,
}

#[derive(Debug)]
pub enum ExecutionInspection {
    Capabilities(ExecutionCapabilities),
    Instances(Vec<InstanceRef>),
    Receipt(ExecutionReceipt),
    Validated,
    Read(ExecutionRead),
    Kind(ExecutionKind),
    Description(Value),
    Shell(ShellStatus),
    Output(OutputPage),
    File(FileInspection),
}

/// One Chat's bound access to its Execution Environment.
///
/// File operations are designed for files that live elsewhere: every operation may be a network
/// call, and the Harness assumes nothing about inodes, symbolic links or atomic renames.
///
/// - Reads ([`FileQuery`]) go through `inspect` and are idempotent. A failure or timeout is
///   returned as an error; cancelling one drops its future, which the environment must tolerate.
/// - Writes (`WriteFile`, `DeleteFile`) go through `prepare` and `execute` with the approval facts
///   and admission of any other intent; a root whose capability does not allow writing refuses
///   them at `prepare` with [`error_code::ACCESS_DENIED`]. A failure before dispatch is
///   `NotDispatched`; a timeout or cancellation after dispatch is `OutcomeUnknown`, and the
///   Harness never sends the write again. A later read sees a write either whole or not at all.
/// - A [`FileVersion`] is compared only for equality; a write whose expected version does not
///   match fails with [`error_code::CONFLICT`] and changes nothing.
/// - The Harness maps a path to a root and refuses `..` that leaves it, and the environment
///   enforces its roots again (locally including symbolic links) with
///   [`error_code::OUTSIDE_ROOT`]. A root's capability is fixed when the Port is bound; whether a
///   directory is reachable is not a capability, so an unreachable one fails each operation.
#[async_trait::async_trait]
pub trait ExecutionPort: Send + Sync {
    async fn inspect(&self, query: ExecutionQuery) -> Result<ExecutionInspection, ExecutionError>;
    async fn prepare(&self, intent: ExecutionIntent) -> Result<PreparedExecution, ExecutionError>;
    async fn execute(
        &self,
        prepared: PreparedExecution,
        admission: ExecutionAdmission,
        cancel: CancellationToken,
    ) -> ExecutionOutcome;
    async fn observe(
        &self,
        request: ObserveRequest,
    ) -> Result<ExecutionObservation, ExecutionError>;
}
