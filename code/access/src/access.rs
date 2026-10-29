//! The AgentAccess contract: the commands, observations, receipts and errors a View, the daemon
//! and its workers exchange, and the messages that carry them over one connection.

use tickloop_harness::protocol::OtherTickhost;

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

use crate::{AccessLimits, FEED_CAPACITY, FEED_CHANGE_BYTES, FEED_MAX_BYTES};
use tickloop_harness::protocol::{
    AccountModelDirectory, AgentHostOptions, ApprovalHistoryView, ArtifactTick,
    BranchSessionPermissionRegistration, ChatSnapshot, ChatStatus, ChatStatusInput, CodeRewind,
    CommandSnapshot, ContextBreakdown, ContextDraft, DirectedSummaryDraft, DirectedSummaryRequest,
    Event, ExecutionMode, HostedMember, HostedViewGrant, InputContext, LoopState,
    LoopTriggerNotice, Message, ModelChange, ModelConfig, MonitorMembership, OperationRecord,
    PendingCommand, PendingInput, PermissionChange, PermissionPolicy, PermissionRequest,
    PermissionRule, PermissionScope, PermissionsRevision, PermissionsView, ProviderCredentialScope,
    ProviderPatch, QuestionAnswer, QuestionRequest, QueuedInput, ReasoningEffort, Receipt,
    RequestUsageLedger, RewindDraft, RewindMode, RewindPoint, RunId, SavedChat, SearchConfig,
    SearchTestResult, ShellTask, ShellTaskOutputChunk, SubagentRegistration, SummaryDirection,
    TakenBackPrompt, TaskNotification, TickSubscriptionChange, TickSubscriptionStatus,
    ToolCallRecovery, UserShellReceipt, UserShellRecord,
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
/// 27.0 gives each Chat exactly one Loop: commands no longer select a Loop, `Stop` pauses
/// receiving instead of ending a Loop, `ContinueLoop` resumes it, and the ended Loop states are
/// gone. Older business majors are refused before dispatch; the frozen daemon management
/// protocol remains independent.
/// 28.0 puts every Trigger on one wake-up base: the Loop carries its receiving switch, with why
/// it paused, and its Tick Subscriptions; a delivery names its source by type; the explicit
/// Trigger commands are gone.
/// 28.1 adds the Chat's goal and scheduled tasks: the Chat snapshot carries both, with
/// `ManageGoal`, goal updates and the completion check's requests for the goal, and Loop
/// actions that delete a scheduled task or answer a pending one, with an event announcing
/// their changes.
/// 28.2 adds Goal display history, continuation times, and the Goal and scheduled tasks on the
/// lightweight Chat status.
/// 28.3 adds user-controlled pauses of individual scheduled tasks and periodic subscriptions.
/// 28.4 adds the plugin source of an invoked Skill.
/// 28.5 adds the status of the Agent's MCP servers and reconnecting one of them.
/// 29.0 adds the model card's native tool search, which older readers reject as unknown.
/// 29.1 adds reading the Chat's context breakdown, as `/context` shows it.
/// 29.2 adds the context breakdown of a New Agent before its first message.
/// 30.0 adds the display-only event naming the MCP servers a request waits for, which older
/// readers reject as unknown.
/// 30.1 carries the context breakdowns of 29.1 and 29.2 onto 30.0.
/// 31.0 adds imported Auto Environment provenance, which older readers reject as unknown,
/// and appending custom Auto rules and Environment entries in one permission change.
/// 32.0 adds launch-argument permission rule sources and optional launch rules and additional
/// directories on Create.
/// 32.1 adds the automatic compaction line to the Chat status and the `autoCompactWindow` cap
/// to the live Chat snapshot.
/// 33.0 adds the context breakdown's Tool search index category, which older readers reject as
/// unknown, and its per-server items.
/// 34.0 adds the host notes that announce the execution mode and the Loop's state, and the MCP
/// configuration warnings guidance note, which older readers reject as unknown, in the Chat
/// snapshot's host notes; the host state of the system prompt no longer carries what changes
/// during a Chat.
/// 34.1 adds the prompt cache break a request caused to its request usage record, and to the
/// Loop how many Runs it started and how many times the Chat was compacted.
/// 34.2 adds the reasoning tokens a response reported to its usage.
/// 34.3 adds the total tokens of a Subagent run's last request to its family completion.
/// 35.0 removes `MarkMonitorHandled`, which older daemons expect from a View opening a row (read
/// state belongs to the Agent), and adds the most rows a Tick can use to its Monitor member.
/// 35.1 adds the ended root Agents the overview keeps after their execution ended, stopping a
/// root Agent, and forgetting an ended one.
/// 36.0 adds an ended root Agent whose last turn had finished (`EndedOutcome::Completed`,
/// `EndReason::Finished`), which older readers reject as unknown, and whether an Agent summary
/// still has work pending while its Loop is idle.
/// 37.0 adds managed ACP and Print executions (profiles, control leases, settlement and
/// session loading), resource and image input contexts and recorded-edit replay, which older
/// readers reject as unknown.
pub const PROTOCOL_MAJOR: u16 = 37;
pub const PROTOCOL_MINOR: u16 = 0;
pub const MANAGED_LEASE_SECONDS: u64 = 45;

/// An execution contract, enforced by the Worker rather than by a View.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProfile {
    #[default]
    Local,
    OneShot,
    Acp,
}

/// A client-generated control secret. Never include it in transcripts, diagnostics or results.
#[derive(Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ManagedToken(String);

impl ManagedToken {
    pub fn new() -> Self {
        // A random control secret, not a time-ordered Tickloop identity.
        Self(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ))
    }

    pub fn matches(&self, other: &Self) -> bool {
        let left = self.0.as_bytes();
        let right = other.0.as_bytes();
        left.len() == 64
            && right.len() == 64
            && left
                .iter()
                .zip(right)
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0
    }
}

impl Default for ManagedToken {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ManagedToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedToken([redacted])")
    }
}

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goal_history: Vec<tickloop_harness::protocol::GoalHistoryEntry>,
    pub record: TranscriptRef,
    pub title: Option<String>,
    pub folder: PathBuf,
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
pub struct LaunchSpec {
    #[serde(default)]
    pub profile: ExecutionProfile,
    pub folder: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<PathBuf>,
    pub config_root: Option<PathBuf>,
    pub model: Option<ModelConfig>,
    pub host_options: AgentHostOptions,
    pub session_persistence: bool,
    pub exact_chat: Option<Uuid>,
    pub environment: Vec<EnvironmentValue>,
    pub host_work_context: Option<HostWorkContext>,
    /// Invocation-private descriptors. Never persisted in transcript metadata.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_mcp_servers: Vec<tickloop_harness::protocol::SessionMcpServer>,
}

impl LaunchSpec {
    pub fn embedded(folder: PathBuf) -> Self {
        Self {
            profile: ExecutionProfile::Local,
            folder,
            additional_directories: Vec::new(),
            config_root: None,
            model: None,
            host_options: AgentHostOptions::embedded_defaults(),
            session_persistence: true,
            exact_chat: None,
            environment: Vec::new(),
            host_work_context: None,
            session_mcp_servers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedConfigurationChange {
    pub model_id: Option<String>,
    pub mode: Option<ExecutionMode>,
    /// Omitted keeps the effort; null selects the model's automatic effort.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable"
    )]
    pub reasoning_effort: Option<Option<ReasoningEffort>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedSkillInfo {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedModelInfo {
    pub id: String,
    pub name: String,
    /// The provider's display name, which a client may group models under.
    pub provider: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedConfiguration {
    pub model_id: String,
    pub mode: ExecutionMode,
    pub models: Vec<ManagedModelInfo>,
    pub modes: Vec<ExecutionMode>,
    /// `None` is the model's automatic effort.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The effort levels the current model's card offers.
    #[serde(default)]
    pub efforts: Vec<ReasoningEffort>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedSessionInfo {
    pub session_id: Uuid,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedSessionPage {
    pub sessions: Vec<ManagedSessionInfo>,
    pub next_cursor: Option<String>,
}

/// Public Transcript DisplayNode values, not model/system messages.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedHistoryPage {
    pub session_id: Uuid,
    pub items: Vec<serde_json::Value>,
    pub next_cursor: Option<String>,
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
    /// Must be bound to the actual IPC session before dispatch; initial input must be absent.
    CreateManaged {
        launch: LaunchSpec,
        initial: Option<UserInput>,
        session_id: Uuid,
        token: ManagedToken,
    },
    /// ACP-only restore. Bound to the real IPC session, never accepts initial input.
    ManagedLoad {
        session_uuid: Uuid,
        launch: LaunchSpec,
        session_id: Uuid,
        token: ManagedToken,
    },
    ManagedConfigure {
        execution: ExecutionRef,
        token: ManagedToken,
        change: ManagedConfigurationChange,
    },
    ManagedSubmit {
        execution: ExecutionRef,
        token: ManagedToken,
        input: UserInput,
    },
    ManagedDecidePermission {
        request: RequestRef,
        token: ManagedToken,
        allow: bool,
        scope: PermissionScope,
        comment: Option<String>,
        rule: Option<String>,
    },
    ManagedHeartbeat {
        execution: ExecutionRef,
        token: ManagedToken,
    },
    ManagedSettle {
        execution: ExecutionRef,
        token: ManagedToken,
        cancel: bool,
    },
    /// Internal daemon-to-Worker acknowledgement after *all* ownership cleanup succeeded.
    ManagedFinalizeRelease {
        execution: ExecutionRef,
        token: ManagedToken,
    },
    ManagedRelease {
        execution: ExecutionRef,
        token: ManagedToken,
    },
    AddArtifactTick {
        execution: ExecutionRef,
        tick: ArtifactTick,
    },
    /// Unlinks the artifact from the Chat: it leaves Monitor and its association with the
    /// Chat's Loop ends.
    RemoveArtifactTick {
        execution: ExecutionRef,
        instance_id: String,
    },
    /// Links the artifact to the Chat for the signed-in account's @tickloop requests.
    LinkArtifact {
        execution: ExecutionRef,
        tick: ArtifactTick,
        history: tickloop_harness::protocol::ArtifactFeedbackHistory,
    },
    ListArtifactTicks {
        execution: ExecutionRef,
    },
    /// Removes an instance from the Chat's Monitor view only.
    RemoveMonitorMember {
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
    /// this Chat end, and with `restore_subscriptions` the Loop subscribes to the new instance at
    /// the frequencies they had.
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        launch_rules: Vec<PermissionRule>,
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
        origin: InteractionSource,
        input: UserInput,
    },
    Submit {
        execution: ExecutionRef,
        input: UserInput,
        origin: Option<InteractionSource>,
    },
    /// The user continues the Chat without a new input: its Loop receives Triggers again.
    ContinueLoop {
        execution: ExecutionRef,
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
    /// A `/goal` command for the Chat's goal.
    ManageGoal {
        execution: ExecutionRef,
        action: tickloop_harness::protocol::GoalAction,
    },
    ClearCompletedTasks {
        execution: ExecutionRef,
        root_loop_id: Uuid,
        expected_revision: u64,
    },
    EnqueueAfterTurn {
        execution: ExecutionRef,
        action: AfterTurnAction,
    },
    RefreshConfiguration {
        execution: ExecutionRef,
    },
    /// Connects one of the Agent's MCP servers again at the user's request.
    ReconnectMcpServer {
        execution: ExecutionRef,
        name: String,
    },
    TrustWorkingDirectory {
        folder: PathBuf,
    },
    FlushTranscripts,
    UpgradeRuntime {
        executable: PathBuf,
    },
    /// Stops the running Tickhost of another build than the daemon's, ending what it runs; the
    /// next open or Shell starts one of the daemon's build.
    ReplaceTickhost,
    /// Stops the root Agent of `execution` and its family: their Loops and Worker end, and the
    /// overview keeps the root as Stopped until the user continues or removes it.
    StopAgent {
        execution: ExecutionRef,
    },
    /// Removes the ended root Agent of `record` from the overview; its saved history stays.
    ForgetEndedAgent {
        record: TranscriptRef,
    },
}

impl AgentCommand {
    pub fn is_managed(&self) -> bool {
        matches!(
            self,
            Self::CreateManaged { .. }
                | Self::ManagedLoad { .. }
                | Self::ManagedConfigure { .. }
                | Self::ManagedSubmit { .. }
                | Self::ManagedDecidePermission { .. }
                | Self::ManagedHeartbeat { .. }
                | Self::ManagedSettle { .. }
                | Self::ManagedRelease { .. }
                | Self::ManagedFinalizeRelease { .. }
        )
    }

    pub fn execution(&self) -> Option<ExecutionRef> {
        match self {
            Self::SendFamilyInput {
                source_execution, ..
            } => Some(*source_execution),
            Self::Submit { execution, .. }
            | Self::ManagedSubmit { execution, .. }
            | Self::ManagedConfigure { execution, .. }
            | Self::ManagedHeartbeat { execution, .. }
            | Self::ManagedSettle { execution, .. }
            | Self::ManagedRelease { execution, .. }
            | Self::ManagedFinalizeRelease { execution, .. }
            | Self::ContinueLoop { execution, .. }
            | Self::Configure { execution, .. }
            | Self::EditHistory { execution, .. }
            | Self::ManageLoop { execution, .. }
            | Self::ManageTask { execution, .. }
            | Self::ManageGoal { execution, .. }
            | Self::AddArtifactTick { execution, .. }
            | Self::RemoveArtifactTick { execution, .. }
            | Self::LinkArtifact { execution, .. }
            | Self::ListArtifactTicks { execution }
            | Self::RemoveMonitorMember { execution, .. }
            | Self::ListHostedTicks { execution }
            | Self::CloseHostedTick { execution, .. }
            | Self::ReopenHostedTick { execution, .. }
            | Self::AttachHostedTick { execution, .. }
            | Self::EnqueueAfterTurn { execution, .. }
            | Self::RefreshConfiguration { execution }
            | Self::ReconnectMcpServer { execution, .. }
            | Self::ClearCompletedTasks { execution, .. } => Some(*execution),
            Self::DecidePermission { request, .. }
            | Self::ManagedDecidePermission { request, .. }
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
        options: tickloop_harness::protocol::AutoSetupOptions,
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
        folder: PathBuf,
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
    /// Interrupts the current turn and pauses receiving until the Chat is continued.
    Stop,
    Interrupt,
    /// Interrupts the current work so the queued user message `run_id` starts now; does nothing
    /// once the message has joined a run or when no work runs ahead of it.
    SendNow {
        run_id: Uuid,
    },
    DisableEventSources,
    ApprovePlan {
        version: u64,
        mode: ExecutionMode,
        /// The user's feedback on the plan, sent along with the approval.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    /// Replaces the reviewed plan version with the user's edit; the edit awaits approval.
    EditPlan {
        version: u64,
        markdown: String,
    },
    DeclinePlan {
        version: u64,
        feedback: Option<String>,
    },
    UpdateSubscription {
        subscription_id: Uuid,
        change: TickSubscriptionChange,
    },
    /// Sends one snapshot of an unsubscribed Tick to the Loop without subscribing.
    TickUpdate {
        instance: tick_protocol::InstanceRef,
    },
    /// Subscribes the Chat's Loop to `instance` at the default frequency, at the user's request
    /// from Monitor.
    TickManage {
        instance: InstanceRef,
    },
    RetryDeniedOperation {
        record_id: Uuid,
    },
    /// Pauses or resumes one scheduled task at the user's request from Monitor.
    SetScheduledTaskPaused {
        task_id: Uuid,
        paused: bool,
    },
    /// Cancels one of the Chat's scheduled tasks, as from its Monitor row.
    DeleteScheduledTask {
        task_id: Uuid,
    },
    /// The user's answer to a scheduled task the Agent created on a host without permission
    /// requests: confirming starts its timing, rejecting deletes it.
    DecideScheduledTask {
        task_id: Uuid,
        decision: tickloop_harness::protocol::ScheduledTaskDecision,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskAction {
    SubmitShell { command: String, background: bool },
    Background { task: TaskRef },
    Stop { task: TaskRef },
    TakeBackQueued { draft_empty: bool },
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
    pub folder: Option<PathBuf>,
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
    OperationReview {
        run_id: Uuid,
        call_id: String,
    },
    ManagedConfiguration {
        token: ManagedToken,
    },
    ManagedHistory {
        token: ManagedToken,
        cursor: Option<String>,
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
    /// The Chat's current context by category, as `/context` shows it.
    ContextBreakdown,
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
    /// The Agent's MCP servers, once it has taken up its current configuration.
    McpServers,
    /// The current Loop's plan file, when one has been written.
    PlanFile,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationRequest {
    ManagedSessions {
        launch: Box<LaunchSpec>,
        cursor: Option<String>,
    },
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
        folder: PathBuf,
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
    /// What the first request of a New Agent `launch` would start would hold, as `/context`
    /// shows it before that Agent exists; nothing is created or connected.
    DraftContextBreakdown {
        launch: Box<LaunchSpec>,
        draft: Box<ContextDraft>,
    },
    /// Connects the MCP server `name` a New Agent `launch` would start, as that Agent would, and
    /// answers with its status and the tools it lists as the one entry of `McpServers`. The
    /// connection closes once listed; no tool runs.
    DraftMcpServer {
        launch: Box<LaunchSpec>,
        name: String,
    },
    /// The root Agents of the configuration root `store_root` whose execution ended, such as when
    /// the daemon stopped, that the overview still lists until the user continues or forgets
    /// them. Running Agents are never included. Without a root, the daemon uses its latest
    /// Worker's.
    EndedAgents {
        store_root: Option<PathBuf>,
    },
}

/// A running Tickhost of another build than the daemon's, which the daemon keeps until the user
/// replaces it.
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
    pub folder: Option<PathBuf>,
    pub state: LoopState,
    pub run_id: Option<RunId>,
    /// Unix milliseconds when the current summarized run began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<u64>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentRegistration>,
    /// Whether this execution ever received work: a message, a user Shell or queued input.
    /// Configuration alone does not count. `None` means the producer did not report the fact (an
    /// older daemon or worker); it is neither worked nor empty and stays absent when re-sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_work: Option<bool>,
    /// Whether work is still coming while its Loop is idle: queued input, an active Goal, a
    /// scheduled task, a received Tick Subscription or a running Shell. `None` when the producer
    /// did not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_work: Option<bool>,
    /// Unix milliseconds when the Loop or a user Shell of this execution last entered its current
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

/// How an ended root Agent reads in the overview, following Claude Code's agent view: one that
/// ended within the last 48 hours of its activity failed, an older one stopped.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EndedOutcome {
    /// Its last turn had finished with nothing pending: Claude Code's done.
    Completed,
    Failed,
    Stopped,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The daemon stopped while the Agent was running in it.
    DaemonStopped,
    /// The Agent had been inactive for over 48 hours when its daemon stopped.
    DaemonOff,
    /// Its worker was lost while the daemon kept running.
    WorkerLost,
    /// The user stopped it.
    UserStopped,
    /// Its last turn had already ended, with nothing pending, when its execution ended.
    Finished,
}

/// A root Agent whose execution ended, as the overview lists it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EndedAgent {
    pub record: TranscriptRef,
    /// The Agent of its last execution.
    pub agent_id: Uuid,
    pub folder: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Unix milliseconds.
    pub created_at: u64,
    pub updated_at: u64,
    pub ended_at: u64,
    pub last_state: LoopState,
    pub outcome: EndedOutcome,
    pub reason: EndReason,
    /// Whether its saved history is still on disk to load.
    pub resumable: bool,
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
    pub pending_thinking: BTreeMap<RunId, String>,
    pub pending_model: Option<ModelConfig>,
    pub commands: Vec<CommandSnapshot>,
    pub user_shells: Vec<UserShellRecord>,
    pub request_usage: RequestUsageLedger,
    pub context_window_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_window: Option<usize>,
    pub last_model_run: Option<RunId>,
    pub warnings: Vec<String>,
    pub request_refs: Vec<RequestRef>,
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
            auto_compact_window: snapshot.auto_compact_window,
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
                ChatStatusInput {
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
                    auto_compact_window: self.auto_compact_window,
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
        snapshot.auto_compact_window = self.auto_compact_window;
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
    ManagedConfiguration(ManagedConfiguration),
    ManagedSessions(ManagedSessionPage),
    ManagedHistory(ManagedHistoryPage),
    OperationReview(Option<tickloop_harness::protocol::RecordedFileChangeReview>),
    SearchTestPending,
    AccountModelsPending,
    ProviderCredentials(Box<ProviderCredentialScope>),
    AccountModels(Result<AccountModelDirectory, String>),
    Overview(Vec<AgentSummary>),
    EndedAgents(Vec<EndedAgent>),
    Agent(Box<LiveChatSnapshot>),
    Status(Box<LiveChatStatus>),
    TaskToolsSetting(bool),
    Operation(Option<Box<OperationRecord>>),
    Events(Vec<Event>),
    Permissions(Box<PermissionsView>),
    ApprovalHistory(ApprovalHistoryView),
    ContextBreakdown(Box<ContextBreakdown>),
    Tasks(Vec<ShellTask>),
    TaskOutput(ShellTaskOutputChunk),
    Subscriptions(Vec<TickSubscriptionStatus>),
    RewindPoints(Vec<RewindPoint>),
    RewindFiles(Option<Vec<PathBuf>>),
    Skills {
        available: Option<bool>,
        warnings: Vec<String>,
        #[serde(default)]
        commands: Vec<ManagedSkillInfo>,
    },
    Warnings(Vec<String>),
    McpServers(Vec<tickloop_harness::protocol::mcp::McpServerStatus>),
    Records(Vec<SavedChat>),
    Transcript(Box<RecordedTranscript>),
    Receipt(Option<Box<CommandReceipt>>),
    OtherTickhost(Option<OtherTickhost>),
    RuntimeVersion(Box<crate::RuntimeVersion>),
    PlanFile(Option<PathBuf>),
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
    ManagedConfigured(ManagedConfiguration),
    ManagedSettled {
        snapshot: Box<LiveChatSnapshot>,
    },
    AutoEnvironmentDraft(tickloop_harness::protocol::AutoEnvironmentDraft),
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
    ManagedExecutions,
    ManagedConfiguration,
    ManagedHistory,
    ManagedSessionLoad,
    ManagedSessionList,
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
    ManageGoal,
    ClearCompletedTasks,
    EnqueueAfterTurn,
    RefreshConfiguration,
    ReconnectMcpServer,
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
        | ObservationValue::ManagedHistory(_)
        | ObservationValue::OperationReview(_)
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
                | CommandResult::ManagedSettled { .. }
                | CommandResult::Branch { .. }
                | CommandResult::Compact { .. }
                | CommandResult::DirectedSummary { .. }
                | CommandResult::Rewind { .. }
                | CommandResult::PromptTakenBack { .. }
                | CommandResult::QueueTaken(_)
        )
    )
}

fn deserialize_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
