//! The AgentAccess contract: the commands, observations, receipts and errors a View, the daemon
//! and its workers exchange, and the messages that carry them over one connection.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tick_protocol::InstanceRef;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    AccessLimits, AccountModelDirectory, AgentHostOptions, ApprovalHistoryView, ArtifactTick,
    BranchSessionPermissionRegistration, ChatSnapshot, ChatStatus, CodeRewind, CommandSnapshot,
    DirectedSummaryDraft, DirectedSummaryRequest, Event, ExecutionMode, FEED_CAPACITY,
    FEED_CHANGE_BYTES, FEED_MAX_BYTES, HostedMember, HostedViewGrant, InputContext, LoopState,
    LoopTriggerNotice, Message, ModelChange, ModelConfig, MonitorMembership, OperationRecord,
    PendingCommand, PendingInput, PermissionChange, PermissionPolicy, PermissionRequest,
    PermissionRule, PermissionScope, PermissionsRevision, PermissionsView, ProviderPatch,
    QuestionAnswer, QuestionRequest, QueuedInput, ReasoningEffort, Receipt, RequestUsageLedger,
    RewindDraft, RewindMode, RewindPoint, RunId, SavedChat, SearchConfig, SearchTestResult,
    ShellTask, ShellTaskOutputChunk, SubagentRegistration, SummaryDirection, TakenBackPrompt,
    TaskNotification, TickSubscriptionChange, TickSubscriptionStatus, ToolCallRecovery, Trigger,
    TriggerSource, UserShellReceipt, UserShellRecord, chat_status::ProcessState,
};

/// Business 18.0 replaces a Tick Subscription's cadence in seconds with one of four frequencies,
/// in the subscription, its status and the change a View requests. 19.0: a hosted Monitor member
/// can be an exited terminal Tick, with how its program ended. 20.0: the hosted Tick snapshot
/// command is gone, and `AttachHostedTick` also grants views of the Chat's Shell tasks. 20.1 names
/// the Bash call that started each Agent Shell task, and adds reading a Chat's status without its
/// history, one recorded tool operation, and the events after a sequence. 20.2 adds each Tick
/// Subscription status's next check and period, so Monitor counts down to the next check, the
/// user's own Tick Subscription from Monitor, and reopening a hosted Tick with its subscriptions.
/// 20.3 records where a Trigger's prompt came from, so a prompt copied from the Loop goal reaches
/// deliveries as background. 20.4 adds reading a running Tickhost of another build than the
/// daemon's, with what replacing it ends, and replacing it on the user's word. 20.5 says whether a
/// Terminal Tick's screen changed since a Tick Subscription last got it, so Monitor shows that
/// change waiting for the next check. 20.6 records when the Chat's Agent last read or operated
/// each Monitor instance with a tool, so Monitor flashes the row it uses. 21.0 adds source-bound
/// remote Skill manifests and their Run and reference activation records. 21.1 adds the first
/// pending change time, terminal program exit, and terminal Tick identity in Monitor membership
/// and listings.
/// 22.0 uses TickDescribe, TickRead, TickCall and one TickManage with per-action admission;
/// TickRead returns state without repeating the capability catalog.
/// 23.0 adds manual TickUpdate delivery and subscription-independent Monitor observations.
/// 24.0 distinguishes internal execution failures from model service failures in the request ledger.
/// 25.0 selects historical feedback when adding an independent Artifact feedback receiver.
/// 25.1 adds cancellable search connection checks in the selected Agent credential scope.
/// 26.0 uses the service model ID as the sole ModelCard ID on the wire.
/// 26.1 adds opt-in planned runtime upgrades and runtime version observations.
/// 26.2 attributes each Worker's runtime version and upgrade error to its executions.
/// Older business majors
/// are refused before dispatch; the frozen daemon management protocol remains independent.
pub const PROTOCOL_MAJOR: u16 = 26;
pub const PROTOCOL_MINOR: u16 = 2;

static BUILD_IDENTITY: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Records which build this process is, once, at its entry; the daemon announces it and a View
/// compares it with the daemon's.
pub fn set_build_identity(identity: &'static str) {
    let _ = BUILD_IDENTITY.set(identity);
}

/// `None` in processes that never named their build, such as in-process tests, which then skip
/// the comparison.
pub fn build_identity() -> Option<&'static str> {
    BUILD_IDENTITY.get().copied()
}

pub const REPLAY_CAPACITY: usize = 4096;
pub const RECEIPT_CAPACITY: usize = 4096;
pub const RECEIPT_RETENTION_SECONDS: u64 = 600;
pub const VIEW_LEASE_SECONDS: u64 = 45;
pub const DEFAULT_PAGE_LIMIT: usize = 100;
pub const MAX_PAGE_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExecutionRef {
    pub daemon_epoch: Uuid,
    pub agent_id: Uuid,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct LoopRef {
    pub execution: ExecutionRef,
    pub loop_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum PendingRequestKind {
    Permission,
    Question,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct RequestRef {
    pub execution: ExecutionRef,
    pub request_id: Uuid,
    pub kind: PendingRequestKind,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum TaskOrigin {
    Agent,
    User,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct TaskRef {
    pub execution: ExecutionRef,
    pub task_id: Uuid,
    pub origin: TaskOrigin,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct TranscriptRef {
    pub store_root: PathBuf,
    pub chat_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecordedMessage {
    pub id: Uuid,
    pub message: Message,
    pub loop_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    pub timestamp: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecordedTranscript {
    pub record: TranscriptRef,
    pub title: Option<String>,
    pub workdir: PathBuf,
    pub messages: Vec<RecordedMessage>,
    pub total_messages: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ViewRef {
    pub session_id: Uuid,
    pub view_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostLocation {
    pub host_instance: String,
    pub socket: PathBuf,
    pub workspace: Option<String>,
    pub tab: Option<String>,
    pub pane: Option<String>,
    pub terminal: Option<String>,
    pub client: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InteractionSource {
    pub view: ViewRef,
    pub display_version: u64,
    pub host: Option<HostLocation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostWorkContext {
    pub socket: PathBuf,
    pub workspace: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct EnvironmentValue {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl std::fmt::Debug for EnvironmentValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnvironmentValue")
            .field("name", &String::from_utf8_lossy(&self.name))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderCredentialScope {
    pub config_root: Option<PathBuf>,
    pub environment_keys: Vec<String>,
    pub service_account_path: Result<Option<String>, String>,
    pub service_account_configured: Result<bool, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchSpec {
    pub workdir: PathBuf,
    pub config_root: Option<PathBuf>,
    pub model: Option<ModelConfig>,
    pub host_options: AgentHostOptions,
    pub session_persistence: bool,
    pub exact_chat: Option<Uuid>,
    pub environment: Vec<EnvironmentValue>,
    pub host_work_context: Option<HostWorkContext>,
}

impl LaunchSpec {
    pub fn embedded(workdir: PathBuf) -> Self {
        Self {
            workdir,
            config_root: None,
            model: None,
            host_options: AgentHostOptions::embedded_defaults(),
            session_persistence: true,
            exact_chat: None,
            environment: Vec::new(),
            host_work_context: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserInput {
    Text {
        text: String,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
    },
    Skill {
        name: String,
        arguments: String,
    },
    Shell {
        command: String,
        background: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommandEnvelope {
    pub daemon_epoch: Uuid,
    pub command_id: Uuid,
    pub command: AgentCommand,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum AgentCommand {
    AddArtifactTick {
        execution: ExecutionRef,
        tick: ArtifactTick,
    },
    /// Unlinks the artifact from the Chat: it leaves Monitor and its association with the
    /// Chat's Loops ends.
    RemoveArtifactTick {
        execution: ExecutionRef,
        instance_id: String,
    },
    /// Links the artifact to the Chat for the signed-in account's @tickloop requests.
    LinkArtifact {
        execution: ExecutionRef,
        tick: ArtifactTick,
        history: crate::ArtifactFeedbackHistory,
    },
    ListArtifactTicks {
        execution: ExecutionRef,
    },
    /// Removes an instance from the Chat's Monitor view only.
    RemoveMonitorMember {
        execution: ExecutionRef,
        instance: InstanceRef,
    },
    /// The user opened the instance's Monitor row: its updates so far count as handled.
    MarkMonitorHandled {
        execution: ExecutionRef,
        instance: InstanceRef,
    },
    /// The Chat's running hosted Ticks with a resource sample of each, for its Monitor.
    ListHostedTicks {
        execution: ExecutionRef,
    },
    /// Ends one of the Chat's hosted Ticks at the user's request.
    CloseHostedTick {
        execution: ExecutionRef,
        instance: InstanceRef,
    },
    /// Starts a new instance in place of a hosted Tick that no longer runs, at the user's
    /// request. It takes the old row's place in Monitor; the old instance's Tick Subscriptions in
    /// this Chat end, and with `restore_subscriptions` their Loops subscribe to the new instance
    /// at the frequencies they had.
    ReopenHostedTick {
        execution: ExecutionRef,
        instance: InstanceRef,
        #[serde(default)]
        restore_subscriptions: bool,
    },
    /// Lets the asking View attach once to one of the Chat's local Ticks the Tickhost runs: a
    /// hosted Tick's terminal, or the screen of one of its Shell tasks.
    AttachHostedTick {
        execution: ExecutionRef,
        instance: InstanceRef,
    },
    Create {
        launch: LaunchSpec,
        initial: Option<UserInput>,
        origin: Option<InteractionSource>,
    },
    LoadRecord {
        record: TranscriptRef,
        as_new: bool,
        launch: Option<LaunchSpec>,
    },
    SendFamilyInput {
        source_execution: ExecutionRef,
        target_execution: ExecutionRef,
        target_loop: Option<Uuid>,
        origin: InteractionSource,
        input: UserInput,
    },
    Submit {
        execution: ExecutionRef,
        target_loop: Option<Uuid>,
        input: UserInput,
        origin: Option<InteractionSource>,
    },
    ContinueLoop {
        execution: ExecutionRef,
        loop_id: Uuid,
        input: UserInput,
        origin: Option<InteractionSource>,
    },
    DecidePermission {
        request: RequestRef,
        allow: bool,
        scope: PermissionScope,
        comment: Option<String>,
        rule: Option<String>,
    },
    AnswerQuestion {
        request: RequestRef,
        answers: Vec<QuestionAnswer>,
    },
    /// Declines a pending question, as Esc in its dialog: the call reports the rejection and the
    /// run that asked it ends there.
    DeclineQuestion {
        request: RequestRef,
    },
    Configure {
        execution: ExecutionRef,
        change: ConfigurationChange,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Legacy wire field; built-in-only workers reject supplied definitions.
        provider: Option<Box<ProviderPatch>>,
    },
    EditHistory {
        execution: ExecutionRef,
        action: HistoryAction,
    },
    ManageLoop {
        execution: ExecutionRef,
        action: LoopAction,
    },
    ManageTask {
        execution: ExecutionRef,
        action: TaskAction,
    },
    ClearCompletedTasks {
        execution: ExecutionRef,
        root_loop_id: Uuid,
        expected_revision: u64,
    },
    EnqueueAfterTurn {
        execution: ExecutionRef,
        target_loop: Option<Uuid>,
        action: AfterTurnAction,
    },
    RefreshConfiguration {
        execution: ExecutionRef,
    },
    TrustWorkingDirectory {
        workdir: PathBuf,
    },
    FlushTranscripts,
    UpgradeRuntime {
        executable: PathBuf,
    },
    /// Stops the running Tickhost of another build than the daemon's, ending what it runs; the
    /// next open or Shell starts one of the daemon's build.
    ReplaceTickhost,
}

impl AgentCommand {
    pub fn execution(&self) -> Option<ExecutionRef> {
        match self {
            Self::SendFamilyInput {
                source_execution, ..
            } => Some(*source_execution),
            Self::Submit { execution, .. }
            | Self::ContinueLoop { execution, .. }
            | Self::Configure { execution, .. }
            | Self::EditHistory { execution, .. }
            | Self::ManageLoop { execution, .. }
            | Self::ManageTask { execution, .. }
            | Self::AddArtifactTick { execution, .. }
            | Self::RemoveArtifactTick { execution, .. }
            | Self::LinkArtifact { execution, .. }
            | Self::ListArtifactTicks { execution }
            | Self::RemoveMonitorMember { execution, .. }
            | Self::MarkMonitorHandled { execution, .. }
            | Self::ListHostedTicks { execution }
            | Self::CloseHostedTick { execution, .. }
            | Self::ReopenHostedTick { execution, .. }
            | Self::AttachHostedTick { execution, .. }
            | Self::EnqueueAfterTurn { execution, .. }
            | Self::RefreshConfiguration { execution }
            | Self::ClearCompletedTasks { execution, .. } => Some(*execution),
            Self::DecidePermission { request, .. }
            | Self::AnswerQuestion { request, .. }
            | Self::DeclineQuestion { request } => Some(request.execution),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum ConfigurationChange {
    TaskToolsEnabled {
        enabled: bool,
    },
    GenerateAutoEnvironment {
        options: crate::AutoSetupOptions,
    },
    Model {
        config: ModelConfig,
        thinking: Option<Option<ReasoningEffort>>,
    },
    ReasoningEffort {
        effort: Option<ReasoningEffort>,
    },
    ThinkingEnabled {
        enabled: bool,
    },
    ExecutionMode {
        mode: ExecutionMode,
    },
    ContextWindow {
        tokens: usize,
    },
    Search {
        config: Option<SearchConfig>,
    },
    Directories {
        workdir: PathBuf,
        additional: Vec<PathBuf>,
    },
    PermissionChange {
        change: PermissionChange,
        expected: PermissionsRevision,
    },
    ReplaceRules {
        rules: Vec<PermissionRule>,
    },
    ProjectConstraints {
        rules: Vec<PermissionRule>,
    },
    ManagedConstraints {
        rules: Vec<PermissionRule>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryAction {
    Branch {
        target_loop: Option<Uuid>,
        name: Option<String>,
    },
    Compact {
        instructions: Option<String>,
    },
    DirectedSummary {
        request: DirectedSummaryRequest,
    },
    Rewind {
        message_id: Uuid,
        mode: RewindMode,
    },
    /// Takes back the prompt of the user-interrupted run `run_id` for editing when the run has no
    /// assistant text or tool call yet; answers `PromptTakenBack` when it did, `Done` when not.
    TakeBackInterruptedPrompt {
        run_id: Uuid,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoopAction {
    Stop {
        loop_id: Uuid,
    },
    Interrupt,
    /// Interrupts the current work so the queued user message `run_id` starts now; does nothing
    /// once the message has joined a run or when no work runs ahead of it.
    SendNow {
        run_id: Uuid,
    },
    DisableEventSources {
        loop_id: Uuid,
    },
    ConfigureTrigger {
        loop_id: Uuid,
        trigger: Option<Trigger>,
    },
    EmitTrigger {
        loop_id: Uuid,
        source: TriggerSource,
    },
    ApprovePlan {
        loop_id: Uuid,
        version: u64,
        mode: ExecutionMode,
    },
    DeclinePlan {
        loop_id: Uuid,
        version: u64,
        feedback: Option<String>,
    },
    UpdateSubscription {
        subscription_id: Uuid,
        change: TickSubscriptionChange,
    },
    /// Sends one snapshot of an unsubscribed Tick to an eligible Loop without subscribing.
    TickUpdate {
        instance: tick_protocol::InstanceRef,
    },
    /// Subscribes the Chat's newest Loop that has not ended to `instance` at the default
    /// frequency, at the user's request from Monitor.
    TickManage {
        instance: InstanceRef,
    },
    RetryDeniedOperation {
        record_id: Uuid,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskAction {
    SubmitShell {
        target_loop: Option<Uuid>,
        command: String,
        background: bool,
    },
    Background {
        task: TaskRef,
    },
    Stop {
        task: TaskRef,
    },
    TakeBackQueued {
        draft_empty: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AfterTurnAction {
    Submit {
        input: UserInput,
        origin: Option<InteractionSource>,
    },
    Configure {
        change: ConfigurationChange,
    },
    History {
        action: HistoryAction,
    },
    Loop {
        action: LoopAction,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct Page {
    pub offset: usize,
    pub limit: usize,
}

impl Page {
    pub fn bounded_limit(self) -> usize {
        if self.limit == 0 {
            DEFAULT_PAGE_LIMIT
        } else {
            self.limit.min(MAX_PAGE_LIMIT)
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AgentFilter {
    pub workdir: Option<PathBuf>,
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentDetailQuery {
    /// The Chat with its whole history.
    Snapshot,
    /// What the Chat is doing now, without its history.
    Status,
    /// Refreshes the Task tools preference from this Agent's configuration scope.
    TaskToolsSetting,
    /// The tool operation a run recorded for one call.
    Operation {
        run_id: Uuid,
        call_id: String,
    },
    /// The events still in the Chat's replay window after sequence `after`, such as one a feed
    /// could not carry.
    Events {
        after: u64,
    },
    ProviderCredentials,
    TestSearch {
        config: SearchConfig,
    },
    AccountModels {
        config: Box<ModelConfig>,
        refresh: bool,
    },
    Permissions,
    ApprovalHistory,
    Tasks,
    TaskOutput {
        task: TaskRef,
        offset: u64,
        limit: usize,
    },
    Subscriptions,
    RewindPoints,
    RewindFiles {
        message_id: Uuid,
    },
    Skills {
        name: Option<String>,
    },
    Warnings,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationRequest {
    Overview {
        filter: AgentFilter,
        page: Page,
        after: Option<Cursor>,
    },
    Agent {
        execution: ExecutionRef,
        detail: AgentDetailQuery,
        after: Option<Cursor>,
    },
    Transcript {
        record: TranscriptRef,
        page: Page,
    },
    SavedRecords {
        store_root: Option<PathBuf>,
        workdir: PathBuf,
        page: Page,
    },
    Receipt {
        epoch: Uuid,
        command_id: Uuid,
    },
    /// The running Tickhost when it is another build than the daemon's and replacing it would end
    /// something; the daemon replaces one that runs nothing at once.
    OtherTickhost,
    RuntimeVersion,
}

/// A running Tickhost of another build than the daemon's, which the daemon keeps until the user
/// replaces it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OtherTickhost {
    /// Which build runs and which the daemon is.
    pub description: String,
    /// What replacing it ends; `None` from a Tickhost too old to say.
    pub occupancy: Option<TickhostOccupancy>,
}

/// What stopping a Tickhost ends.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct TickhostOccupancy {
    /// Each Tick it holds, by its title, or its instance when it has none.
    pub ticks: Vec<String>,
    pub shells: usize,
    /// Ticks and Shells it is starting, which stopping it ends before they run.
    pub pending: usize,
    /// The views attached to its Ticks and Shells, which stopping it closes; `None` from a
    /// Tickhost too old to say.
    pub views: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Cursor {
    pub daemon_epoch: Uuid,
    pub execution: Option<ExecutionRef>,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ResetReason {
    EpochChanged,
    Lagged,
    CursorExpired,
    ConversationChanged,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ExecutionAvailability {
    #[default]
    Live,
    Lost,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentSummary {
    #[serde(default)]
    pub availability: ExecutionAvailability,
    pub execution: ExecutionRef,
    pub chat_id: Uuid,
    pub workdir: PathBuf,
    pub state: LoopState,
    pub run_id: Option<RunId>,
    /// Unix milliseconds when the current summarized run began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<u64>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentRegistration>,
    /// Whether this execution ever received work: a Loop, a message, a user Shell or queued input.
    /// Configuration alone does not count. `None` means the producer did not report the fact (an
    /// older daemon or worker); it is neither worked nor empty and stays absent when re-sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_work: Option<bool>,
    /// Unix milliseconds when a Loop or user Shell of this execution last entered its current
    /// state; for finished work, when it finished. `None` when no producer recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    /// The latest reply or typed prompt as one bounded line, when it says more than the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub waiting_permissions: usize,
    pub waiting_questions: usize,
    pub parent: Option<ExecutionRef>,
    pub record: Option<TranscriptRef>,
    pub workspace: Option<String>,
    pub capabilities: Vec<String>,
    pub views: Vec<DisplayedView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QueuedCommandFailure {
    pub command_id: Uuid,
    pub execution: ExecutionRef,
    pub label: String,
    pub error: AccessError,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LiveChatSnapshot {
    pub execution: ExecutionRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentRegistration>,
    pub conversation: ChatSnapshot,
    pub model: ModelConfig,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub thinking_enabled: bool,
    pub policy: PermissionPolicy,
    pub additional_directories: Vec<PathBuf>,
    pub last_sequence: u64,
    pub permissions: Vec<PermissionRequest>,
    pub questions: Vec<QuestionRequest>,
    pub pending_inputs: Vec<PendingInput>,
    pub pending_commands: Vec<PendingCommand>,
    pub operations: Vec<OperationRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_recoveries: Vec<ToolCallRecovery>,
    pub events: Vec<Event>,
    pub pending_thinking: BTreeMap<Uuid, String>,
    pub pending_model: Option<ModelConfig>,
    pub commands: Vec<CommandSnapshot>,
    pub user_shells: Vec<UserShellRecord>,
    pub request_usage: RequestUsageLedger,
    pub context_window_tokens: Option<usize>,
    pub last_model_run: Option<Uuid>,
    pub warnings: Vec<String>,
    pub request_refs: Vec<RequestRef>,
    pub resume_triggers: Vec<Uuid>,
    pub last_queue_sequence: u64,
    pub transcript_write_error: Option<String>,
    pub loop_trigger_notices: Vec<LoopTriggerNotice>,
    pub queue_failures: Vec<QueuedCommandFailure>,
}

impl LiveChatSnapshot {
    pub fn from_snapshot(
        execution: ExecutionRef,
        snapshot: ChatSnapshot,
        warnings: Vec<String>,
    ) -> Self {
        Self {
            execution,
            subagent: None,
            resume_triggers: snapshot.resume_triggers.clone(),
            last_queue_sequence: snapshot.last_queue_sequence,
            transcript_write_error: None,
            loop_trigger_notices: Vec::new(),
            queue_failures: Vec::new(),
            model: snapshot.model.clone(),
            reasoning_effort: snapshot.reasoning_effort,
            thinking_enabled: snapshot.thinking_enabled,
            policy: snapshot.policy.clone(),
            additional_directories: snapshot.additional_directories.clone(),
            last_sequence: snapshot.last_sequence,
            permissions: snapshot.permissions.clone(),
            questions: snapshot.questions.clone(),
            pending_inputs: snapshot.pending_inputs.clone(),
            pending_commands: snapshot.pending_commands.clone(),
            operations: snapshot.operations.clone(),
            tool_call_recoveries: snapshot.tool_call_recoveries.clone(),
            events: snapshot.events.clone(),
            pending_thinking: snapshot.pending_thinking.clone(),
            pending_model: snapshot.pending_model.clone(),
            commands: snapshot.commands.clone(),
            user_shells: snapshot.user_shells.clone(),
            request_usage: snapshot.request_usage.clone(),
            context_window_tokens: snapshot.context_window_tokens,
            last_model_run: snapshot.last_model_run,
            request_refs: RequestRef::pending(
                execution,
                &snapshot.permissions,
                &snapshot.questions,
            ),
            conversation: snapshot,
            warnings,
        }
    }

    /// The status this snapshot holds, as a status query would have answered at the same point.
    pub fn status(&self) -> LiveChatStatus {
        LiveChatStatus {
            execution: self.execution,
            subagent: self.subagent.clone(),
            chat: ChatStatus::assemble(
                &self.conversation,
                ProcessState {
                    model: &self.model,
                    pending_model: &self.pending_model,
                    reasoning_effort: self.reasoning_effort,
                    thinking_enabled: self.thinking_enabled,
                    policy: &self.policy,
                    additional_directories: &self.additional_directories,
                    permissions: &self.permissions,
                    questions: &self.questions,
                    pending_inputs: &self.pending_inputs,
                    pending_commands: &self.pending_commands,
                    user_shells: &self.user_shells,
                    commands: &self.commands,
                    request_usage: &self.request_usage,
                    context_window_tokens: self.context_window_tokens,
                    events: &self.events,
                    last_sequence: self.last_sequence,
                },
            ),
            request_refs: self.request_refs.clone(),
            warnings: self.warnings.clone(),
            transcript_write_error: self.transcript_write_error.clone(),
            loop_trigger_notices: self.loop_trigger_notices.clone(),
        }
    }

    pub fn into_snapshot(self) -> ChatSnapshot {
        let mut snapshot = self.conversation;
        snapshot.resume_triggers = self.resume_triggers;
        snapshot.last_queue_sequence = self.last_queue_sequence;
        snapshot.model = self.model;
        snapshot.reasoning_effort = self.reasoning_effort;
        snapshot.thinking_enabled = self.thinking_enabled;
        snapshot.policy = self.policy;
        snapshot.additional_directories = self.additional_directories;
        snapshot.last_sequence = self.last_sequence;
        snapshot.permissions = self.permissions;
        snapshot.questions = self.questions;
        snapshot.pending_inputs = self.pending_inputs;
        snapshot.pending_commands = self.pending_commands;
        snapshot.operations = self.operations;
        snapshot.tool_call_recoveries = self.tool_call_recoveries;
        snapshot.events = self.events;
        snapshot.pending_thinking = self.pending_thinking;
        snapshot.pending_model = self.pending_model;
        snapshot.commands = self.commands;
        snapshot.user_shells = self.user_shells;
        snapshot.request_usage = self.request_usage;
        snapshot.context_window_tokens = self.context_window_tokens;
        snapshot.last_model_run = self.last_model_run;
        snapshot
    }
}

/// A live Chat's status: its `ChatStatus` with what the serving process adds, the pending
/// requests a View answers and the notices it shows. A `LiveChatSnapshot` of the same moment
/// derives the same value.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LiveChatStatus {
    pub execution: ExecutionRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentRegistration>,
    pub chat: ChatStatus,
    pub request_refs: Vec<RequestRef>,
    pub warnings: Vec<String>,
    pub transcript_write_error: Option<String>,
    pub loop_trigger_notices: Vec<LoopTriggerNotice>,
}

impl RequestRef {
    /// The first version of each request still waiting for an answer.
    pub fn pending(
        execution: ExecutionRef,
        permissions: &[PermissionRequest],
        questions: &[QuestionRequest],
    ) -> Vec<Self> {
        permissions
            .iter()
            .map(|request| Self {
                execution,
                request_id: request.id,
                kind: PendingRequestKind::Permission,
                version: 1,
            })
            .chain(questions.iter().map(|request| Self {
                execution,
                request_id: request.id,
                kind: PendingRequestKind::Question,
                version: 1,
            }))
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ObservationValue {
    SearchTestPending,
    AccountModelsPending,
    ProviderCredentials(Box<ProviderCredentialScope>),
    AccountModels(Result<AccountModelDirectory, String>),
    Overview(Vec<AgentSummary>),
    Agent(Box<LiveChatSnapshot>),
    Status(Box<LiveChatStatus>),
    TaskToolsSetting(bool),
    Operation(Option<Box<OperationRecord>>),
    Events(Vec<Event>),
    Permissions(Box<PermissionsView>),
    ApprovalHistory(ApprovalHistoryView),
    Tasks(Vec<ShellTask>),
    TaskOutput(ShellTaskOutputChunk),
    Subscriptions(Vec<TickSubscriptionStatus>),
    RewindPoints(Vec<RewindPoint>),
    RewindFiles(Option<Vec<PathBuf>>),
    Skills {
        available: Option<bool>,
        warnings: Vec<String>,
    },
    Warnings(Vec<String>),
    Records(Vec<SavedChat>),
    Transcript(Box<RecordedTranscript>),
    Receipt(Option<Box<CommandReceipt>>),
    OtherTickhost(Option<OtherTickhost>),
    RuntimeVersion(Box<crate::RuntimeVersion>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ObservationUpdate {
    pub cursor: Cursor,
    pub change: ObservationChange,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ObservationChange {
    SearchTest(SearchTestResult),
    AccountModels(Result<AccountModelDirectory, String>),
    Event(Event),
    UserShell(UserShellRecord),
    TaskNotification(TaskNotification),
    Changed,
    Resync {
        reason: ResetReason,
    },
    Reset {
        reason: ResetReason,
        snapshot: Box<LiveChatSnapshot>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ReceiptState {
    Accepted,
    Running,
    Completed,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PersistenceState {
    NotApplicable,
    BestEffortPending,
    WriteFailed(String),
    FlushObserved,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommandReceipt {
    pub daemon_epoch: Uuid,
    pub command_id: Uuid,
    pub target: Option<ExecutionRef>,
    pub state: ReceiptState,
    pub result: Option<CommandResult>,
    pub error: Option<AccessError>,
    pub persistence: PersistenceState,
}

impl CommandReceipt {
    pub fn into_result(self) -> Result<CommandResult, AccessError> {
        self.error.map_or_else(
            || {
                self.result
                    .ok_or_else(|| AccessError::unavailable("command is still being processed"))
            },
            Err,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum CommandResult {
    AutoEnvironmentDraft(crate::AutoEnvironmentDraft),
    ArtifactTicks(Vec<ArtifactTick>),
    MonitorMembership(MonitorMembership),
    HostedTicks(Vec<HostedMember>),
    HostedTickReopened {
        instance: InstanceRef,
        membership: MonitorMembership,
        /// Why some Tick Subscriptions the user asked to restore did not follow.
        #[serde(default)]
        restore_error: Option<String>,
    },
    HostedTickView(HostedViewGrant),
    Created {
        execution: ExecutionRef,
        snapshot: Box<LiveChatSnapshot>,
        submitted: Option<Receipt>,
        shell: Option<UserShellReceipt>,
    },
    Loaded {
        execution: ExecutionRef,
        snapshot: Box<LiveChatSnapshot>,
    },
    FamilyInputSent {
        source_execution: ExecutionRef,
        target_execution: ExecutionRef,
        receipt: Receipt,
        message_id: Uuid,
        target_handle: String,
    },
    Submitted(Receipt),
    ShellSubmitted(UserShellReceipt),
    Done,
    ModelChanged(ModelChange),
    Permissions(Box<PermissionsView>),
    Operations(Vec<OperationRecord>),
    Text(String),
    Task(ShellTask),
    QueueTaken(Vec<QueuedInput>),
    Queued {
        command_id: Uuid,
    },
    Branch {
        execution: ExecutionRef,
        source_chat_id: Uuid,
        child_chat_id: Uuid,
        name: String,
        snapshot: Box<LiveChatSnapshot>,
        session_permissions: BranchSessionPermissionRegistration,
    },
    Compact {
        summary: String,
        snapshot: Box<LiveChatSnapshot>,
    },
    DirectedSummary {
        snapshot: Box<LiveChatSnapshot>,
        direction: SummaryDirection,
        summary: String,
        guidance: Option<String>,
        messages_summarized: usize,
        draft: Option<DirectedSummaryDraft>,
        continuation_error: Option<String>,
    },
    Rewind {
        snapshot: Box<LiveChatSnapshot>,
        mode: RewindMode,
        code: Option<CodeRewind>,
        conversation_restored: bool,
        draft: Option<RewindDraft>,
    },
    PromptTakenBack {
        snapshot: Box<LiveChatSnapshot>,
        draft: TakenBackPrompt,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum AccessErrorCode {
    Invalid,
    NotFound,
    StaleExecution,
    EpochChanged,
    Unsupported,
    UntrustedWorkingDirectory,
    Busy,
    CommandIdConflict,
    AlreadyResolved,
    Responding,
    VersionConflict,
    Unavailable,
    HandoffNotAccepted,
    PayloadTooLarge,
    AccessDenied,
    VersionMismatch,
    OutcomeUnknown,
    CapacityExceeded,
    Storage,
}

#[derive(Clone, Debug, Deserialize, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct AccessError {
    pub code: AccessErrorCode,
    pub message: String,
    pub latest_permissions: Option<Box<PermissionsView>>,
    pub path: Option<PathBuf>,
}

impl AccessError {
    pub fn new(code: AccessErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            latest_permissions: None,
            path: None,
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(AccessErrorCode::Invalid, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(AccessErrorCode::Unavailable, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(AccessErrorCode::NotFound, message)
    }
    pub fn storage(message: impl Into<String>) -> Self {
        Self::new(AccessErrorCode::Storage, message)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DisplayedView {
    pub view: ViewRef,
    pub version: u64,
    pub execution: ExecutionRef,
    pub host: Option<HostLocation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DisplayUpdate {
    Displayed {
        view: ViewRef,
        expected_version: u64,
        execution: ExecutionRef,
        host: Option<HostLocation>,
    },
    Scene {
        view: ViewRef,
        expected_version: u64,
        host: Option<HostLocation>,
    },
    Renew {
        view: ViewRef,
        version: u64,
    },
    Left {
        view: ViewRef,
        version: u64,
    },
    SessionClosed {
        session_id: Uuid,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DisplayReceipt {
    pub view: ViewRef,
    pub version: u64,
    pub displayed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum AccessCapability {
    Overview,
    AgentDetails,
    Records,
    Receipts,
    Create,
    LoadRecord,
    Submit,
    ContinueLoop,
    DecidePermission,
    AnswerQuestion,
    Configure,
    EditHistory,
    ManageLoop,
    ManageTask,
    ClearCompletedTasks,
    EnqueueAfterTurn,
    RefreshConfiguration,
    TrustWorkingDirectory,
    FlushTranscripts,
    DisplayRegistration,
    ChunkedResponses,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProtocolLimits {
    pub frame_bytes: usize,
    pub source_items: usize,
    pub feed_items: usize,
    pub feed_bytes: usize,
    pub change_bytes: usize,
    pub receipt_items: usize,
    pub receipt_seconds: u64,
    pub view_lease_seconds: u64,
    pub page_items: usize,
    pub default_page_items: usize,
    pub chunk_bytes: usize,
}

impl From<AccessLimits> for ProtocolLimits {
    fn from(transport: AccessLimits) -> Self {
        Self {
            frame_bytes: transport.frame_bytes(),
            source_items: REPLAY_CAPACITY,
            feed_items: FEED_CAPACITY,
            feed_bytes: FEED_MAX_BYTES,
            change_bytes: FEED_CHANGE_BYTES,
            receipt_items: RECEIPT_CAPACITY,
            receipt_seconds: RECEIPT_RETENTION_SECONDS,
            view_lease_seconds: VIEW_LEASE_SECONDS,
            page_items: MAX_PAGE_LIMIT,
            default_page_items: DEFAULT_PAGE_LIMIT,
            chunk_bytes: transport.chunk_bytes(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServerHello {
    #[serde(default)]
    pub process_id: u32,
    pub major: u16,
    pub minor: u16,
    pub daemon_epoch: Uuid,
    pub session_id: Uuid,
    #[serde(default)]
    pub capabilities: Vec<AccessCapability>,
    pub limits: ProtocolLimits,
    /// Absent from daemons older than 16.1 and from those that never named their build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// The executable the daemon runs, which can open a View of the same build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_protocol: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<ServerHandoffHello>,
}

pub const CLIENT_HANDOFF_VERSION: u16 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientHandoffHello {
    pub version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<SessionResume>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResume {
    pub daemon_epoch: Uuid,
    pub session_id: Uuid,
    pub token: String,
    pub transfer_id: Uuid,
}

impl std::fmt::Debug for SessionResume {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionResume")
            .field("daemon_epoch", &self.daemon_epoch)
            .field("session_id", &self.session_id)
            .field("transfer_id", &self.transfer_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerHandoffHello {
    pub version: u16,
    pub resume_token: String,
}

impl std::fmt::Debug for ServerHandoffHello {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerHandoffHello")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[allow(clippy::large_enum_variant)]
pub enum ClientMessage {
    ShutdownDaemon {
        request_id: Uuid,
        daemon_epoch: Uuid,
    },
    Hello {
        major: u16,
        minor: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handoff: Option<ClientHandoffHello>,
    },
    HandoffReady {
        transfer_id: Uuid,
    },
    Observe {
        request_id: Uuid,
        query: ObservationRequest,
    },
    Execute {
        request_id: Uuid,
        command: CommandEnvelope,
    },
    ShowView {
        request_id: Uuid,
        update: DisplayUpdate,
    },
    CancelObservation {
        observation_id: Uuid,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ChunkedResponseKind {
    Opened,
    Receipt,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ServerMessage {
    HandoffPrepare {
        transfer_id: Uuid,
    },
    HandoffAbort {
        transfer_id: Uuid,
    },
    HandoffCommit {
        transfer_id: Uuid,
        successor_process_id: u32,
    },
    /// The request never entered the command receipt store and was never dispatched.
    NotAccepted {
        request_id: Uuid,
        transfer_id: Uuid,
    },
    ResponseStart {
        request_id: Uuid,
        transfer_id: Uuid,
        kind: ChunkedResponseKind,
        total_bytes: u64,
        chunk_bytes: usize,
    },
    ResponseChunk {
        transfer_id: Uuid,
        offset: u64,
        base64: String,
    },
    ResponseEnd {
        transfer_id: Uuid,
        total_bytes: u64,
        sha256: String,
    },
    DaemonStopped {
        request_id: Uuid,
    },
    Hello(ServerHello),
    Opened {
        request_id: Uuid,
        observation_id: Uuid,
        initial: ObservationValue,
        cursor: Cursor,
        reset: Option<ResetReason>,
        feed: bool,
    },
    Changed {
        observation_id: Uuid,
        update: ObservationUpdate,
    },
    ObservationClosed {
        observation_id: Uuid,
        error: AccessError,
    },
    Receipt {
        request_id: Uuid,
        receipt: CommandReceipt,
    },
    Display {
        request_id: Uuid,
        receipt: DisplayReceipt,
    },
    Error {
        request_id: Option<Uuid>,
        error: AccessError,
    },
}

/// Why an observation's change feed ended, recorded by its producer before the feed closes.
pub type ObservationFailure = Arc<Mutex<Option<AccessError>>>;

/// The answer to an observation request: the value at `cursor`, followed by its changes when the
/// request opened a feed.
#[derive(Debug)]
pub struct Observation {
    pub initial: ObservationValue,
    pub cursor: Cursor,
    pub reset: Option<ResetReason>,
    changes: Option<mpsc::Receiver<ObservationUpdate>>,
    failure: Option<ObservationFailure>,
}

impl Observation {
    /// A one-time answer without a change feed.
    pub fn single(initial: ObservationValue, cursor: Cursor, reset: Option<ResetReason>) -> Self {
        Self::new(initial, cursor, reset, None, None)
    }

    /// An answer whose changes arrive on `changes` when present; `failure` holds why they ended.
    pub fn new(
        initial: ObservationValue,
        cursor: Cursor,
        reset: Option<ResetReason>,
        changes: Option<mpsc::Receiver<ObservationUpdate>>,
        failure: Option<ObservationFailure>,
    ) -> Self {
        Self {
            initial,
            cursor,
            reset,
            changes,
            failure,
        }
    }

    pub fn has_feed(&self) -> bool {
        self.changes.is_some()
    }

    /// The initial answer and the feed that follows it, taken apart for forwarding.
    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        ObservationValue,
        Cursor,
        Option<ResetReason>,
        Option<mpsc::Receiver<ObservationUpdate>>,
        Option<ObservationFailure>,
    ) {
        (
            self.initial,
            self.cursor,
            self.reset,
            self.changes,
            self.failure,
        )
    }

    pub async fn recv(&mut self) -> Result<ObservationUpdate, AccessError> {
        match self.changes.as_mut() {
            Some(changes) => changes.recv().await.ok_or_else(|| {
                self.failure
                    .as_ref()
                    .and_then(|failure| failure.lock().unwrap().clone())
                    .unwrap_or_else(|| AccessError::unavailable("observation disconnected"))
            }),
            None => Err(AccessError::invalid("this observation is a single query")),
        }
    }
}

#[async_trait]
pub trait AgentAccess: Send + Sync {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError>;
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError>;
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError>;
}

/// `update` as sent on a feed: a change too large for one feed item becomes a resync request.
pub fn bounded_update(mut update: ObservationUpdate) -> ObservationUpdate {
    if serde_json::to_vec(&update).map_or(true, |bytes| bytes.len() > FEED_CHANGE_BYTES) {
        update.change = ObservationChange::Resync {
            reason: ResetReason::Lagged,
        };
    }
    update
}

impl ChunkedResponseKind {
    /// The chunked kind a response travels as: Agent histories, the events and tool operations
    /// they record, and the receipts that carry one may outgrow a frame; every other response
    /// never does.
    pub fn of(response: &ServerMessage) -> Option<Self> {
        match response {
            ServerMessage::Opened { initial, .. } if carries_history(initial) => Some(Self::Opened),
            ServerMessage::Receipt { receipt, .. } if receipt_carries_history(receipt) => {
                Some(Self::Receipt)
            }
            _ => None,
        }
    }
}

fn carries_history(value: &ObservationValue) -> bool {
    match value {
        ObservationValue::Agent(_)
        | ObservationValue::Operation(_)
        | ObservationValue::Events(_)
        | ObservationValue::Transcript(_)
        | ObservationValue::RewindPoints(_) => true,
        ObservationValue::Receipt(Some(receipt)) => receipt_carries_history(receipt),
        _ => false,
    }
}

fn receipt_carries_history(receipt: &CommandReceipt) -> bool {
    matches!(
        receipt.result,
        Some(
            CommandResult::Created { .. }
                | CommandResult::Loaded { .. }
                | CommandResult::Branch { .. }
                | CommandResult::Compact { .. }
                | CommandResult::DirectedSummary { .. }
                | CommandResult::Rewind { .. }
                | CommandResult::PromptTakenBack { .. }
                | CommandResult::QueueTaken(_)
        )
    )
}
