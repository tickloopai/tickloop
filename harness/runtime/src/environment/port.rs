use std::sync::Arc;
pub use tickloop_harness_protocol::{ExecutionIsolation, NetworkIsolation, WriteIsolation};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tick_protocol::InstanceRef;
use tickloop_harness_protocol::{ChatId, PreparedApprovalFacts, ToolResult};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::host_tools::HostToolPermission;

pub const TERMINAL_INPUT_BYTES: usize = 4 * 1024;
pub const WORKSPACE_PAGE_SIZE: usize = 128;
pub const SHELL_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionScope {
    pub chat: ChatId,
    /// A path in the execution environment, never resolved by the Harness.
    pub folder: Option<String>,
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
                "AuthorityRevoked",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionCapabilities {
    pub ticks: bool,
    pub shells: bool,
    pub shell_output: ShellOutputDelivery,
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
    pub view: Option<ViewPlacement>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSpec {
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewPlacement {
    pub workspace_id: Option<String>,
    pub placement: ViewPosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewPosition {
    NewTab,
    CurrentTab {
        #[serde(default = "default_view_direction")]
        direction: String,
    },
    Pane {
        pane_id: String,
        #[serde(default = "default_view_direction")]
        direction: String,
    },
}

fn default_view_direction() -> String {
    "right".into()
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
}

#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub result: ToolResult,
    pub opened: Option<OpenedExecution>,
    pub shell: Option<Box<ShellStatus>>,
    pub handoff: Option<ExecutionHandoff>,
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
}

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
