use tickloop_harness_policy::settings::{SettingsStore, SettingsUpdate};
mod agent_messages;
mod agent_tasks;
mod approval_assessment;
pub(crate) mod approval_context;
mod artifact_reads;
mod auto_approval;
#[cfg(test)]
mod auto_benchmark_tests;
mod auto_health;
mod auto_mode;
#[cfg(test)]
mod auto_replay_tests;
pub(crate) mod auto_rules;
mod auto_settings;
mod auto_setup;
mod branch;
mod family;
mod family_load;
mod family_tree;
pub mod file_history;
mod goal;
mod handback;
mod handover;
pub use handover::HandOver;
mod harness;
pub use citations::{CitationQueryError, VisibleCitations, visible_citations_at};

pub use harness::{
    CitationStyle, DeliveryBinding, HarnessOptions, HarnessSeams, PersonWait, PersonWaitKind,
    Quiescence, SearchDefaults, ToolPolicy, WaitHosting, Waits, durable_chat_host_rules,
};
mod host_context;
mod host_notes;
pub mod host_tools;
mod jev_review;
mod lifecycle;
pub mod terminal_input;
pub use lifecycle::InputLedgerState;
mod loop_lease;
pub mod mcp;
mod mcp_http;
mod mcp_resources;
pub mod mcp_tools;
mod model_request;
mod persistence;
mod remote_skills;
mod scheduled_tasks;
mod timing;
pub use scheduled_tasks::{NewScheduledTask, ScheduledTaskEdit};
pub mod tick_adapter;
pub mod tick_stream;
pub mod tick_tools;
pub mod transcript;
pub use family::summary_state;
mod family_mailbox;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod family_pauses;
pub use family::{FamilyAgent, FamilyAvailability, FamilyRole};
mod artifact_ticks;
mod cache_prefix;
mod chat_reads;
mod chat_settings;
mod chat_title;
mod citations;
mod compaction;
mod context;
mod context_breakdown;
mod delegation;
mod denials;
mod directed_summary;
mod durable_queue;
pub mod environment;
mod environment_info;
mod event_sources;
mod feedback_inbox;
#[cfg(test)]
mod file_read_tests;
mod input_context;
mod instruction_context;
mod interaction;
mod monitor_members;
mod nested_calls;
mod recovery;
#[cfg(test)]
mod request_failure_tests;
#[cfg(test)]
mod request_race_tests;
mod rewind;
mod run_supervision;
mod session_permissions;
mod shell_description;
mod shell_task_center;
mod suspension;
mod system_prompt;
mod tool_batch;
mod tool_execution;
pub mod tool_exposure;
mod tool_results;
pub mod upgrade;
mod web_fetch;
use tool_batch::{BatchExit, RunStart, ToolBatch};
use tool_execution::{ToolCallOutcome, ToolInvocation, ToolRegistry};
mod commands;
mod permission_management;
mod plan;
mod shared_tasks;
mod tick_content;
mod tick_subscriptions;
mod tick_trim;
mod turn;
mod wakeup;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use wakeup::{TestSourceState, trigger_pauses};
mod web_provenance;
pub use event_sources::ExternalDelivery;
pub use turn::{
    Delivery, MessageId, ParentSelection, QuestionReply, SkillInvocation, Turn, TurnOp,
    TurnOutcome, TurnRequest, TurnWait, UserMessage,
};
mod user_shell;
#[cfg(test)]
mod user_shell_tests;
pub use chat_reads::ChatSummary;
pub use permission_management::PermissionChangeError;
pub use shell_task_center::{ShellTaskCenter, ShellTaskSubscription};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::{Mutex, Notify, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::file_history::FileHistory;
use session_permissions::{PermissionOwner, SessionPermissionOperation};
use tickloop_harness_base::output_style::OutputStyle;
use tickloop_harness_model::{
    ModelClient, ModelConfigExt, ModelError, ModelFailureKind, ModelRequest, ModelResponseRejection,
};
use tickloop_harness_protocol::local_messages;
use tickloop_harness_protocol::{
    AgentHostOptions, AgentMessageSource, ApprovalAssessmentDetails, ApprovalFailureKind,
    ApprovalFallbackReason, ApprovalOutcome, ApprovalPath, ApprovalReviewerKind, AutoBlockKind,
    AutoReviewerHealth, ChatId, ChatSnapshot, CompactionMarker, Content, Decision,
    DelegationMetadata, DenialReference, DirectedHumanSource, DirectoryTrustPolicy, Event,
    EventKind, ExecutionMode, HostCallContext, InputContext, LedgerError, LoopId, LoopSnapshot,
    LoopState, Message, ModelConfig, ModelRequestKind, MonitorMembership, OperationRecord,
    OperationState, PendingCommand, PendingInput, PermissionApprovalSource, PermissionExecution,
    PermissionPolicy, PermissionRequest, PermissionRule, PermissionScope, PromptPolicy,
    QueuedInput, QueuedInputKind, ReasoningEffort, Receipt, RequestOutcome, RequestUsageLedger,
    ResponseRejection, Role, RunId, SubagentRegistration, TaskNotification, TriggerSource, Usage,
    UserShellRecord, UserShellState, VerdictEvidence, projection,
};
use tickloop_harness_tools::permissions::SessionPermissionDecision;
use tickloop_harness_tools::{
    file_change::load_file_change_review, permissions::PermissionPolicyExt,
};
use tickloop_harness_transcript::{
    DiscardReason, InputOp, InterruptedBy, ResponseEnding, SettledOutcome,
};

const EVENT_REPLAY_LIMIT: usize = 1024;
const MAX_RESPONSE_CONTINUATIONS: usize = 3;
const OUTPUT_LIMIT_CONTINUATION_PROMPT: &str = "Output token limit hit. Continue directly from the unfinished response without repeating or summarizing it. Break the remaining work into smaller pieces.";
const STREAM_INTERRUPTION_CONTINUATION_PROMPT: &str = "The response stream was interrupted. Continue directly from the unfinished response without repeating or summarizing it.";
const SHELL_STATE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

pub use agent_messages::LIST_SUBAGENTS_TOOL_NAME;
pub use delegation::MAX_SUBAGENT_HANDLE_CHARACTERS;

#[derive(Clone, Debug)]
pub struct Observation {
    pub snapshot: ChatSnapshot,
    pub events: Vec<Event>,
    pub reset_required: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] tickloop_harness_model::ModelError),
    #[error("request usage ledger: {0}")]
    Usage(#[from] LedgerError),
    #[error("storage failure: {0}")]
    Storage(String),
    /// A newer holder owns the Chat's Transcript; this Harness writes nothing more to it.
    #[error("the Chat's Transcript belongs to a newer holder")]
    Fenced,
    #[error("chat does not exist")]
    NotFound,
    /// The Chat has no scheduled task with that id, such as one deleted already.
    #[error("scheduled task does not exist")]
    ScheduledTaskNotFound,
    /// A recurring task's schedule runs more often than the host's least interval allows.
    #[error("{}", scheduled_tasks::interval_limit(*min_interval))]
    ScheduledTaskTooFrequent { min_interval: std::time::Duration },
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("chat is busy")]
    Busy,
    #[error("working directory is not trusted: {path}")]
    UntrustedWorkingDirectory { path: PathBuf },
    #[error(
        "the current model does not support image input; remove the attached images such as @image mentions, or switch to a model that accepts images with /model"
    )]
    ImageInputUnsupported,
}

impl From<transcript::TranscriptError> for AgentError {
    fn from(error: transcript::TranscriptError) -> Self {
        match error {
            transcript::TranscriptError::Fenced => Self::Fenced,
            transcript::TranscriptError::Unsupported => {
                Self::Invalid("Unsupported: the Transcript store does not support this".into())
            }
            transcript::TranscriptError::Conflict(message)
            | transcript::TranscriptError::Storage(message) => Self::Storage(message),
        }
    }
}

impl From<projection::InvalidConversation> for AgentError {
    fn from(error: projection::InvalidConversation) -> Self {
        Self::Invalid(error.0)
    }
}

/// Loop state changes stamped with this process's wall clock.
pub(crate) trait LoopSnapshotExt {
    /// Moves the Loop to `state`, recording the moment only when the state actually changes.
    fn set_state(&mut self, state: LoopState);
}

impl LoopSnapshotExt for LoopSnapshot {
    fn set_state(&mut self, state: LoopState) {
        if self.state != state {
            self.state = state;
            self.state_since = Some(tickloop_harness_base::private_fs::unix_time_milliseconds());
        }
    }
}

/// User Shell state changes stamped with this process's wall clock.
pub(crate) trait UserShellRecordExt {
    /// Moves the Shell to `state`, recording the moment only when the state actually changes.
    fn set_state(&mut self, state: UserShellState);
}

impl UserShellRecordExt for UserShellRecord {
    fn set_state(&mut self, state: UserShellState) {
        if self.state != state {
            self.state = state;
            self.state_since = Some(tickloop_harness_base::private_fs::unix_time_milliseconds());
            self.updated_at_ms = self.state_since;
        }
    }
}

/// What a Chat's model selection means for its next request, read from the model catalogue.
pub trait ChatSnapshotExt {
    fn effective_reasoning_effort(&self) -> Option<ReasoningEffort>;
    fn effective_context_window_tokens(&self) -> Option<usize>;
}

impl ChatSnapshotExt for ChatSnapshot {
    fn effective_reasoning_effort(&self) -> Option<ReasoningEffort> {
        if !self.thinking_enabled
            && tickloop_harness_model::validate_reasoning_effort(
                &self.model,
                Some(ReasoningEffort::Off),
            )
            .is_ok()
        {
            Some(ReasoningEffort::Off)
        } else {
            self.reasoning_effort
        }
    }

    fn effective_context_window_tokens(&self) -> Option<usize> {
        context::context_window(self)
    }
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub(crate) struct WeakAgent {
    inner: std::sync::Weak<Inner>,
}

impl WeakAgent {
    pub(crate) fn upgrade(&self) -> Option<Agent> {
        self.inner.upgrade().map(|inner| Agent { inner })
    }
}

impl Agent {
    pub(crate) fn downgrade(&self) -> WeakAgent {
        WeakAgent {
            inner: Arc::downgrade(&self.inner),
        }
    }
}

struct Inner {
    /// The model currently selected in this process; new and resumed Chats use it, and `set_model`
    /// updates it with the selection.
    selected_model: std::sync::Mutex<chat_settings::SelectedModel>,
    /// Builds the client of each model a Chat selects.
    models: Arc<dyn tickloop_harness_model::ModelClients>,
    /// Host-provided preferences and project permissions; absent when the host keeps none.
    settings: Option<Arc<dyn SettingsStore>>,
    search: SearchDefaults,
    search_provider_factory:
        Option<Arc<dyn tickloop_harness_tools::web_search::SearchProviderFactory>>,
    host_options: AgentHostOptions,
    waits: WaitHosting,
    /// Local credential and tool-output storage, supplied independently of preferences.
    config_root: Option<PathBuf>,
    /// Orders settings reads and writes within this process: writing settings and syncing loaded
    /// Chats, as well as reading settings and adding a new Chat, happen while holding it, keeping
    /// loaded Chats consistent with the settings file. Always acquired before the `chats` lock.
    settings_writes: Mutex<()>,
    task_tools_enabled: AtomicBool,
    /// Problems found the last time user settings were read for each Chat.
    settings_warnings: std::sync::Mutex<HashMap<ChatId, Vec<String>>>,
    provider_failures:
        std::sync::Mutex<HashMap<String, tickloop_harness_protocol::ProviderHttpFailure>>,
    trusted_working_directories: std::sync::Mutex<HashSet<PathBuf>>,
    host_contexts: std::sync::Mutex<host_context::HostContexts>,
    subagent_invocations: Mutex<HashMap<(RunId, String), delegation::SubagentInvocation>>,
    family_mailbox: Mutex<family_mailbox::FamilyMailbox>,
    /// Changes whenever a Chat's input ledger accepts or settles an input.
    ledger_changes: tokio::sync::watch::Sender<()>,
    title_requests: Mutex<HashMap<ChatId, chat_title::TitleJob>>,
    /// Display names generated for background Shells without a description of their own, by
    /// task: `None` while pending or after a failed attempt. Each task gets one attempt in this
    /// process, and model-facing text never uses them.
    shell_descriptions: Mutex<HashMap<Uuid, Option<String>>>,
    /// Announces a generated Shell description by task, for Shell task subscribers only.
    shell_description_events: broadcast::Sender<Uuid>,
    subagent_registry: Mutex<HashMap<ChatId, Vec<SubagentRegistration>>>,
    chats: timing::ChatsMutex<HashMap<ChatId, Chat>>,
    events: broadcast::Sender<Event>,
    user_shell_events: broadcast::Sender<UserShellRecord>,
    user_shell_cancellations: std::sync::Mutex<HashMap<(ChatId, Uuid), CancellationToken>>,
    task_notifications: broadcast::Sender<TaskNotification>,
    /// Every task this instance spawns; `shutdown` waits for all of them.
    tasks: agent_tasks::AgentTasks,
    recorder: Option<persistence::ChatRecorder>,
    /// Trigger leases held by this process; returned when Inner is released.
    chat_leases: loop_lease::ChatLeases,
    /// The Trigger sources' own states, taken over from the store with each newly taken lease.
    trigger_source_states: std::sync::RwLock<Vec<Arc<dyn wakeup::TriggerSourceState>>>,
    approvals: Mutex<HashMap<Uuid, PendingApproval>>,
    question_answers: Mutex<HashMap<Uuid, oneshot::Sender<interaction::PendingReply>>>,
    closed: AtomicBool,
    upgrade_paused: AtomicBool,
    upgrade_adopted: AtomicBool,
    upgrade_gate: Arc<tokio::sync::RwLock<()>>,
    upgrade_guard: Mutex<Option<tokio::sync::OwnedRwLockWriteGuard<()>>>,
    upgrade_restore: Mutex<Option<upgrade::AgentUpgradeCheckpoint>>,
    auto_reviewer: Mutex<Option<Arc<dyn ModelClient>>>,
    judgment: Mutex<Option<Arc<dyn tickloop_harness_model::judgment::JudgmentClient>>>,
    auto_health: Mutex<auto_health::AutoHealth>,
    host_tools: std::sync::RwLock<Option<Arc<dyn crate::host_tools::HostToolProvider>>>,
    /// Where Chats with a Folder run their commands and Ticks; `None` when Chats have no Folder.
    environment: Option<Arc<dyn environment::ExecutionEnvironment>>,
    tool_policy: ToolPolicy,
    citation_style: CitationStyle,
    host_rules: Vec<PermissionRule>,
    /// The host's own section of every conversation's system prompt.
    system_prompt_addition: Option<String>,
    /// The permits each request to a model's vendor takes, in order.
    request_gates: Vec<Arc<tokio::sync::Semaphore>>,
    /// Where every Chat's shell commands run: the Tickhost.
    shells: std::sync::RwLock<Option<Arc<dyn tickloop_harness_tools::ShellRuntime>>>,
    /// Whether a Workspace Tick Update stream is installed; the Tick Subscription tools reach the
    /// model only then.
    tick_stream_installed: AtomicBool,
    tick_deliveries: std::sync::Mutex<tick_subscriptions::TickDeliveries>,
    /// Wakes the delivery task when the subscription set or a frequency changed.
    tick_wake: Notify,
    /// Tests that hand goal continuations over themselves keep the host from starting them.
    #[cfg(any(test, feature = "test-support"))]
    goal_continuations_held: AtomicBool,
    /// The zone scheduled tasks read cron expressions in, and their clock.
    schedule_clock: scheduled_tasks::ScheduleClock,
    scheduled_task_rules: scheduled_tasks::TaskRules,
    /// Whether tasks the Agent creates start without the user's confirmation.
    trust_model_scheduled_tasks: bool,
    /// Wakes a Worker's scheduled task timer when a Chat's tasks changed.
    scheduled_wake: Arc<Notify>,
    scheduled_timer_started: AtomicBool,
    session_permissions: Mutex<session_permissions::SessionPermissionContext>,
    shutdown: CancellationToken,
    /// A voluntary handover began: no Run starts, and each running Run stops at its next safe
    /// point.
    handover: CancellationToken,
}

/// Runtime bookkeeping about a Chat's conversation that never leaves this process. Recovery and
/// Branch copies rebuild it from the conversation chain.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct ChatBookkeeping {
    #[serde(default)]
    additional_directory_sources: tickloop_harness_protocol::AdditionalDirectorySources,
    /// What the Chat's own work vouches for, as its Transcript keeps it.
    provenance: tickloop_harness_transcript::ProvenanceEntry,
    #[serde(default)]
    citation_sources: tickloop_harness_transcript::CitationSourcesEntry,
    #[serde(default)]
    continued_message_ids: HashMap<Uuid, Vec<Uuid>>,
    /// The dependency files' fingerprints when the Chat was created; a restored Chat has none.
    dependency_baseline: Option<BTreeMap<PathBuf, String>>,
    tick_creation_receipts: Vec<approval_context::TickCreationReceipt>,
    denial_references: BTreeMap<usize, DenialReference>,
    /// Project partition (normalized working directory) owning each persistent rule in the rule
    /// set; deletions and modifications are written back to that partition.
    persistent_rule_projects: BTreeMap<Uuid, PathBuf>,
    /// Following Claude Code: the Chat's plan file name prefix, generated when a plan is first set
    /// and recorded in every later chain entry.
    plan_slug: Option<String>,
    /// Highest plan version already used by a Loop whose plan content is unavailable (plan file
    /// missing or unreadable on resume, `--fork-session`/Branch copy failed); that Loop's later
    /// plans continue from a larger version, so version numbers are never reused.
    unavailable_plan_versions: BTreeMap<LoopId, u64>,
    /// The Loop owning each run in this Chat, written as `loopId` on chain entries; rebuilt from
    /// the entries on resume.
    run_loops: BTreeMap<RunId, LoopId>,
    /// Shells whose completion notification waits among the deferred messages.
    deferred_notifications: Vec<tick_protocol::InstanceRef>,
    /// Shells whose completion notification is in the conversation but no turn has read yet.
    unread_notifications: Vec<tick_protocol::InstanceRef>,
    restored_skill_names: HashSet<String>,
    lifecycle: lifecycle::Lifecycle,
    /// The answer each message is part of.
    message_responses: BTreeMap<usize, tickloop_harness_protocol::ResponseId>,
}

/// What a Subagent's Chat takes from its parent's.
struct InheritedChat {
    additional_directory_sources: tickloop_harness_protocol::AdditionalDirectorySources,
    metadata: DelegationMetadata,
    parent: ChatSnapshot,
    client: Arc<dyn ModelClient>,
    output_style: OutputStyle,
    user_context: OnceLock<String>,
    announced_date: Option<String>,
}

#[derive(Clone)]
struct Chat {
    /// How this Chat's Loop writes ids into model text and resolves the ids the model passes
    /// back.
    ids: tickloop_ids::IdContext,
    family_generation: u64,
    family_stopping: bool,
    task_checkpoint_pending: bool,
    family_finishing: HashSet<RunId>,
    snapshot: ChatSnapshot,
    bookkeeping: ChatBookkeeping,
    start_new_tool_result_message: bool,
    /// Claude Code's `needsPlanModeExitAttachment`: the user left Plan mode without approving a
    /// plan, so the next request owes the model the exited-plan-mode notice.
    needs_plan_mode_exit_notice: bool,
    /// Claude Code's `prePlanMode`: the mode this Chat entered Plan from, kept in this process and
    /// across a planned upgrade. A load, or a rewind or branch switch that changes the mode, leaves
    /// it unknown.
    mode_before_plan: Option<ExecutionMode>,
    active: Option<CancellationToken>,
    active_run: Option<RunId>,
    upgrade_wait: Option<Receipt>,
    parked: HashMap<RunId, ParkedRun>,
    /// The release of each Run waiting durably, parked or holding the execution slot. A released
    /// Run leaves this instance without writing anything; it goes on wherever the Chat loads next.
    releases: HashMap<RunId, CancellationToken>,
    ready_runs: VecDeque<RunId>,
    client: Arc<dyn ModelClient>,
    pending_client: Option<Arc<dyn ModelClient>>,
    pending_model_thinking: Option<PendingModelThinking>,
    /// The Chat's tools; its Chat directory is `<chatId>/` next to the Transcript when persisted,
    /// or under the project directory in the system temp directory when not, and externalized tool
    /// results and file change reviews are written there.
    tools: Arc<tickloop_harness_tools::Tools>,
    /// Shared with the tool registry of each model request, whose Skill calls expand against the
    /// listing that request sent even when the Chat's Skills are read again meanwhile.
    skills: Arc<tickloop_harness_policy::skills::SkillRegistry>,
    /// Following Claude Code, replacement decisions are kept only at runtime and rebuilt from
    /// content replacement entries in the Transcript on resume.
    content_replacements: tool_results::ContentReplacementState,
    /// `None` for a Chat without a Folder, which edits no local file.
    file_history: Option<FileHistory>,
    watching_commands: bool,
    watching_user_shells: bool,
    user_shell_queue: Arc<Notify>,
    /// Polls the Loop's artifact links for @tickloop requests while it receives.
    feedback_watcher: Option<CancellationToken>,
    feedback_inputs: HashMap<RunId, feedback_inbox::FeedbackIdentity>,
    feedback_cleanup_started: bool,
    /// What the user's `/loop` of each turn schedules, as the host read it: the one task the
    /// Agent may create for the user in that turn.
    user_loops: HashMap<RunId, tickloop_harness_policy::loop_skill::LoopSchedule>,
    /// Local messages waiting to be written to the conversation; held while the Chat is executing,
    /// waiting or running a foreground `!` command, and written when idle or at a tool boundary.
    deferred_messages: Vec<Message>,
    /// Runs cancelled by a user interrupt or stop; only these Runs keep partial replies and write
    /// an interrupt marker.
    user_cancelled_runs: HashSet<RunId>,
    /// Loaded when the Chat opens in this process and unaffected by later settings changes.
    output_style: OutputStyle,
    /// Claude Code's user context: the instruction files and the date, read at the first
    /// conversation request and kept until compaction or until the Chat loads again.
    user_context: OnceLock<String>,
    /// Claude Code's environment section, read once at the Chat's first conversation request in
    /// this process and kept, unlike the user context, through compaction.
    environment: OnceLock<environment_info::ChatEnvironment>,
    /// The date the model was last told, by the user context or a date change note; `None` until
    /// the user context is read.
    announced_date: Option<String>,
    /// Claude Code-style Auto classifier denial counts for this in-process Chat.
    auto_denials: auto_mode::DenialTracking,
    denial_window: denials::DenialWindow,
    /// Another stored branch of the Chat is being restored to replace its conversation: until the
    /// reservation's own input installs it, or the reservation ends, the Chat takes no input.
    branch_reservation: Option<Uuid>,
    /// The goal's usage its last record holds that the Transcript took.
    goal_recorded_tokens: u64,
    /// Restored from history by this process and not yet taken over: the history-load rule
    /// applies to its goal once it takes the Trigger lease.
    loaded_from_history: bool,
    /// When the goal's running time was last settled.
    goal_clock_ms: u64,
    /// What keeps this Chat's prompt cache prefix stable, kept only in this process.
    cache_prefix: cache_prefix::CachePrefixState,
}

enum RunAdmission {
    Idle,
    Ready(Receipt, Box<ModelRequest>, CancellationToken),
    /// The first takeable input is a Trigger's whose source proves it again outside the lock.
    ValidateTrigger(Box<PendingInput>),
}

#[derive(Clone, Copy)]
struct PendingModelThinking {
    effort: Option<ReasoningEffort>,
    enabled: bool,
}

#[derive(Clone)]
struct ParkedRun {
    receipt: Receipt,
    cancel: CancellationToken,
    resume: Arc<Notify>,
    sequence: u64,
}

#[derive(Default)]
struct SubmissionOptions {
    task_delegation: Option<Receipt>,
    attachment_admission: Option<family::AttachmentAdmission>,
    family_generation: Option<u64>,
    contexts: Vec<InputContext>,
    attached_file_reads: Vec<(PathBuf, tickloop_harness_protocol::FileReadRecord)>,
    denial_reference: Option<DenialReference>,
    /// Set only by [`Agent::deliver_trigger`], the wake-up base's one entry.
    trigger_source: Option<TriggerSource>,
    /// The lease holding the delivery was let in under, which must still stand when it is
    /// accepted, and the Chat's Trigger boundary it is accepted within, left once it is.
    trigger_lease: Option<(loop_lease::LeaseHolding, loop_lease::TriggerBoundary)>,
    /// The source's own change for the delivery, made as it is accepted.
    trigger_commit: Option<wakeup::SourceCommit>,
    invoked_skill: Option<tickloop_harness_protocol::InvokedSkill>,
    /// What the user's `/loop` schedules, when the input is one.
    user_loop: Option<tickloop_harness_policy::loop_skill::LoopSchedule>,
    /// The Run the input is answered by, when its caller chose it ahead.
    run_id: Option<RunId>,
    host_context: Option<HostCallContext>,
    agent_message: Option<AgentMessageSource>,
    directed_human: Option<DirectedHumanSource>,
    ledger: Option<lifecycle::LedgerInput>,
    turn: TurnAdmission,
}

/// How a turn's input is admitted beyond queuing it.
#[derive(Default)]
pub(crate) struct TurnAdmission {
    /// Interrupt the work ahead of it in the same change that queues it.
    pub send_now: bool,
    /// The explicitly selected place it goes: the Chat must be idle, and the conversation is
    /// cut there in the same change that accepts it.
    pub placement: Option<turn::Placement>,
    /// The stored branch that place is on, staged under a reservation of the Chat and installed
    /// in the same change that accepts the input.
    pub branch: Option<turn::BranchSwitch>,
}

/// How long a started Trigger input whose start cannot be recorded waits before it tries again,
/// doubling with each failure up to the longest.
const TRIGGER_START_RETRY_FIRST: std::time::Duration = std::time::Duration::from_millis(200);
const TRIGGER_START_RETRY_LONGEST: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a Trigger input waits for its start to be recorded before receiving pauses.
const TRIGGER_START_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(any(test, feature = "test-support"))]
static TRIGGER_START_BUDGET_FOR_TESTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn trigger_start_budget() -> std::time::Duration {
    #[cfg(any(test, feature = "test-support"))]
    {
        let millis = TRIGGER_START_BUDGET_FOR_TESTS.load(Ordering::Relaxed);
        if millis > 0 {
            return std::time::Duration::from_millis(millis);
        }
    }
    TRIGGER_START_BUDGET
}

/// What taking queued Trigger input came to.
enum TriggerTake {
    /// It started, and its run runs.
    Started,
    /// It waits, for the lease, its source or the Loop.
    Waits,
    /// The queue changed meanwhile: look at it again.
    LookAgain,
}

/// Why a take of `pending` stops before it starts the input, as another take of it may have
/// started it meanwhile; `None` while it waits in the Chat's queue as it was found there and
/// the ledger has not started it.
fn taken_elsewhere(chat: &Chat, pending: &PendingInput) -> Option<TriggerTake> {
    let queued = chat.snapshot.pending_inputs.iter().any(|queued| {
        queued.receipt.run_id == pending.receipt.run_id
            && queued.input == pending.input
            && queued.trigger_source == pending.trigger_source
    });
    if !queued {
        return Some(TriggerTake::LookAgain);
    }
    // Still queued while the ledger has it started: looking again would find it again.
    chat.bookkeeping
        .lifecycle
        .is_started(pending.receipt.run_id)
        .then_some(TriggerTake::Waits)
}

/// Queues a Run that continues the conversation without a message of its own.
fn queue_continuation(chat: &mut Chat, receipt: Receipt) {
    chat.snapshot.last_queue_sequence += 1;
    let queued_sequence = chat.snapshot.last_queue_sequence;
    chat.snapshot.pending_inputs.push(PendingInput {
        receipt,
        input: String::new(),
        contexts: Vec::new(),
        agent_message: None,
        directed_human: None,
        denial_reference: None,
        trigger_source: None,
        armed: true,
        queued_sequence,
        task_continuation: true,
        is_skill_invocation: false,
        attached_file_reads: Vec::new(),
    });
}

pub(crate) fn chat_is_busy() -> AgentError {
    AgentError::Invalid("chat is busy".into())
}

#[derive(Clone, Copy)]
enum MessageBoundary {
    /// The Chat has no executing or waiting Run and is not reorganizing its context.
    Idle,
    /// The current Run just finished writing a round of tool results, and the next model request
    /// has not started.
    ToolResults,
}

struct PermissionAnswer {
    allow: bool,
    revision: u64,
    comment: Option<String>,
}

struct HostAdmission<'a> {
    mcp_server: Option<&'a str>,
    tool_name: &'a str,
    expected_task_binding: Option<Option<tickloop_harness_protocol::TaskExecutionBinding>>,
    session_evaluation: &'a session_permissions::SessionPermissionEvaluation,
    policy_revision: u64,
    ancestor_revisions: &'a [(ChatId, u64)],
    host_permission: Option<crate::host_tools::HostToolPermission>,
    local_read: Option<&'a tickloop_harness_tools::PreparedTool>,
    approval_path: ApprovalPath,
    direct_basis: denials::DirectApprovalBasis,
    approval_source: Option<PermissionApprovalSource>,
    assessment: Option<&'a ApprovalAssessmentDetails>,
}

struct PermissionPrompt {
    source: PermissionApprovalSource,
    reason: Option<String>,
    mcp_server: Option<String>,
}

struct PendingApproval {
    sender: oneshot::Sender<PermissionAnswer>,
    owner: PermissionOwner,
    operation: SessionPermissionOperation,
}

#[derive(Clone, Debug)]
enum AutoReview {
    Allow,
    Ask {
        diagnostics: Vec<tickloop_harness_protocol::ApprovalDiagnostic>,
        reason: String,
    },
    Block {
        reason: String,
        kind: AutoBlockKind,
        rule_id: Option<String>,
        evidence: Option<VerdictEvidence>,
    },
    ContextLimit,
    Unavailable {
        message: String,
    },
}

fn permission_denial_message(comment: Option<&str>) -> String {
    match comment {
        Some(comment) => {
            format!("The user denied permission for this operation and gave this reason: {comment}")
        }
        None => "permission denied; the requested operation was not executed".into(),
    }
}

fn host_tool_decision(
    configured: Option<tickloop_harness_tools::permissions::ExplicitRuleMatch>,
    host_permission: Option<crate::host_tools::HostToolPermission>,
    policy: &tickloop_harness_protocol::PermissionPolicy,
    mode_before_plan: Option<ExecutionMode>,
    has_human_input: bool,
) -> Decision {
    let execution_mode = policy.mode;
    if host_permission == Some(crate::host_tools::HostToolPermission::ThirdParty) {
        // As in Claude Code, Plan entered from Full Access keeps allowing third-party calls, and
        // otherwise uses Auto's review unless the user turned that off; then Plan decides them as
        // the mode it was entered from, an unknown one as Ask.
        let mode = match (execution_mode, mode_before_plan) {
            (ExecutionMode::Plan, Some(ExecutionMode::FullAccess)) => ExecutionMode::FullAccess,
            (ExecutionMode::Plan, _) if policy.uses_auto_admission() => ExecutionMode::Auto,
            (ExecutionMode::Plan, _) => mode_before_plan.unwrap_or(ExecutionMode::AcceptEdits),
            (mode, _) => mode,
        };
        return match (configured, mode) {
            (Some(rule), _) => Decision::from(rule.decision),
            (None, ExecutionMode::FullAccess) => Decision::Allow,
            (None, ExecutionMode::Auto) => Decision::Check,
            (None, _) => Decision::Ask,
        };
    }
    let is_allowed_by_host = configured.is_some_and(|matched| {
        matched.decision == tickloop_harness_tools::permissions::ExplicitPermissionDecision::Allow
            && matched.source == tickloop_harness_tools::permissions::RuleSource::Host
    });
    let configured = configured.map(|matched| Decision::from(matched.decision));
    if let Some(crate::host_tools::HostToolPermission::Classified {
        allow_rule_bypasses_review,
    }) = host_permission
    {
        // The host's Allow is the host's own decision: no reviewer or person is there to ask.
        return match configured {
            Some(Decision::Deny) => Decision::Deny,
            Some(Decision::Ask) => Decision::Ask,
            Some(Decision::Allow) if allow_rule_bypasses_review || is_allowed_by_host => {
                Decision::Allow
            }
            _ if matches!(execution_mode, ExecutionMode::Auto | ExecutionMode::Plan) => {
                Decision::Check
            }
            _ => Decision::Allow,
        };
    }
    if matches!(
        host_permission,
        Some(
            crate::host_tools::HostToolPermission::PublicArtifactWrite
                | crate::host_tools::HostToolPermission::ArtifactContinuation
        )
    ) {
        return match configured {
            Some(Decision::Deny) => Decision::Deny,
            Some(Decision::Ask) => Decision::Ask,
            _ if host_permission
                == Some(crate::host_tools::HostToolPermission::ArtifactContinuation) =>
            {
                Decision::Allow
            }
            _ if execution_mode == ExecutionMode::Plan => Decision::Ask,
            Some(Decision::Allow) => Decision::Allow,
            _ if execution_mode == ExecutionMode::FullAccess => Decision::Allow,
            _ if execution_mode == ExecutionMode::Auto => Decision::Check,
            _ => Decision::Ask,
        };
    }
    if host_permission == Some(crate::host_tools::HostToolPermission::PrivateArtifactWrite) {
        return match configured {
            Some(Decision::Deny) => Decision::Deny,
            Some(Decision::Ask) => Decision::Ask,
            _ if execution_mode == ExecutionMode::Plan => Decision::Ask,
            Some(Decision::Allow) => Decision::Allow,
            _ if has_human_input || execution_mode == ExecutionMode::FullAccess => Decision::Allow,
            _ if execution_mode == ExecutionMode::Auto => Decision::Check,
            _ => Decision::Ask,
        };
    }
    if execution_mode == ExecutionMode::Plan
        && matches!(
            host_permission,
            Some(
                crate::host_tools::HostToolPermission::Automatic
                    | crate::host_tools::HostToolPermission::Reviewable
                    | crate::host_tools::HostToolPermission::Confirmation
            )
        )
    {
        return Decision::Deny;
    }
    if host_permission == Some(crate::host_tools::HostToolPermission::Confirmation) {
        return match configured {
            Some(Decision::Deny) => Decision::Deny,
            _ => Decision::Ask,
        };
    }
    configured.unwrap_or_else(|| {
        if host_permission == Some(crate::host_tools::HostToolPermission::Reviewable) {
            match execution_mode {
                ExecutionMode::FullAccess => Decision::Allow,
                ExecutionMode::Auto => Decision::Check,
                ExecutionMode::AcceptEdits => Decision::Ask,
                ExecutionMode::Plan => Decision::Deny,
            }
        } else {
            Decision::Allow
        }
    })
}

/// The mode Plan was entered from once the mode changes from `from` to `to`: known only while in
/// Plan, and kept while Plan is entered again.
fn mode_before_plan_after(
    remembered: Option<ExecutionMode>,
    from: ExecutionMode,
    to: ExecutionMode,
) -> Option<ExecutionMode> {
    match (from, to) {
        (_, ExecutionMode::AcceptEdits | ExecutionMode::Auto | ExecutionMode::FullAccess) => None,
        (ExecutionMode::Plan, ExecutionMode::Plan) => remembered,
        (from, ExecutionMode::Plan) => Some(from),
    }
}

impl Chat {
    /// Changes the execution mode, remembering the mode Plan is entered from.
    fn change_execution_mode(&mut self, mode: ExecutionMode) {
        self.mode_before_plan =
            mode_before_plan_after(self.mode_before_plan, self.snapshot.policy.mode, mode);
        self.snapshot.policy.mode = mode;
    }

    /// Replaces the snapshot, forgetting the mode Plan was entered from if the mode changes.
    fn replace_snapshot(&mut self, snapshot: ChatSnapshot) {
        if snapshot.policy.mode != self.snapshot.policy.mode {
            self.mode_before_plan = None;
        }
        self.snapshot = snapshot;
    }

    fn has_human_input_for_run(&self, run: RunId) -> bool {
        approval_context::originals_for_active_chain(&self.snapshot, &self.bookkeeping)
            .is_ok_and(|inputs| inputs.iter().any(|input| input.run_id == run))
    }
    /// While a foreground `!` command is running or stopping, the Chat accepts no compaction and
    /// writes no other local messages.
    fn has_foreground_user_shell(&self) -> bool {
        self.snapshot.user_shells.iter().any(|record| {
            matches!(
                record.state,
                UserShellState::WaitingPermission
                    | UserShellState::Running
                    | UserShellState::Cancelling
            )
        })
    }

    fn run_cancel(&self, run_id: RunId) -> Option<&CancellationToken> {
        if self.active_run == Some(run_id) {
            self.active.as_ref()
        } else {
            self.parked.get(&run_id).map(|run| &run.cancel)
        }
    }

    /// Cancels the running run, or else the latest waiting one, as the user's interruption.
    /// Queued user and Agent messages stay armed and start right after it. Returns the
    /// operations whose outcome the interruption may leave admitted or unknown.
    /// Esc: interrupts the running turn, or else the latest one waiting for a person.
    fn interrupt(&mut self) -> Vec<OperationRecord> {
        if let Some(cancel) = &self.active {
            cancel.cancel();
            if let Some(run_id) = self.active_run {
                self.user_cancelled_runs.insert(run_id);
            }
        } else if let Some(waiting) = self.parked.values().max_by_key(|run| run.sequence) {
            waiting.cancel.cancel();
            self.user_cancelled_runs.insert(waiting.receipt.run_id);
        }
        self.uncertain_operations()
    }

    /// `/stop`: interrupts the running turn and every turn waiting for a person.
    fn interrupt_every_run(&mut self) -> Vec<OperationRecord> {
        if let Some(cancel) = &self.active {
            cancel.cancel();
            if let Some(run_id) = self.active_run {
                self.user_cancelled_runs.insert(run_id);
            }
        }
        for waiting in self.parked.values() {
            waiting.cancel.cancel();
            self.user_cancelled_runs.insert(waiting.receipt.run_id);
        }
        // Root continuation may proceed after this stop's memory transition. Descendants
        // remain closed to new execution until their subtree stop finishes.
        if self.snapshot.delegation.is_none() {
            self.family_stopping = false;
        }
        self.uncertain_operations()
    }

    fn uncertain_operations(&self) -> Vec<OperationRecord> {
        self.snapshot
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.state,
                    OperationState::Admitted | OperationState::Unknown
                )
            })
            .cloned()
            .collect()
    }
}

fn host_provider_is_confirmed(
    host_tools: &std::sync::RwLock<Option<Arc<dyn crate::host_tools::HostToolProvider>>>,
) -> bool {
    host_tools
        .read()
        .unwrap()
        .as_ref()
        .is_none_or(|provider| provider.tool_set_is_confirmed())
}

/// The options of an Agent on this computer: every capability and no host rule.
#[cfg(test)]
fn local_options(
    model: ModelConfig,
    settings: Option<Arc<dyn SettingsStore>>,
    host: AgentHostOptions,
) -> HarnessOptions {
    HarnessOptions {
        settings,
        host,
        ..HarnessOptions::local(model)
    }
}

fn configured_workspace_for_chat(chat: &Chat) -> tickloop_harness_tools::Workspace {
    tickloop_harness_tools::Workspace {
        folder: chat.snapshot.folder.clone(),
        additional_directories: chat.snapshot.additional_directories.clone(),
    }
}

/// Why a Chat without a Folder refuses what needs local files or commands.
const FOLDERLESS_CHAT: &str = "Unsupported: this Chat has no Folder";

/// Why an Agent without an Execution Environment refuses what concerns a local Folder.
const NO_ENVIRONMENT: &str = "Unsupported: this Agent has no Execution Environment for a Folder";

/// The Folder of a Chat that needs one for local files or commands.
fn require_folder(snapshot: &ChatSnapshot) -> Result<&Path, AgentError> {
    snapshot
        .folder
        .as_deref()
        .ok_or_else(|| AgentError::Invalid(FOLDERLESS_CHAT.into()))
}

/// A permission request can still be answered: the request exists, its Run is not cancelled and the
/// Loop is waiting for permission; when approving a file change, the review matches the request.
fn answerable_permission_request(
    chat: &Chat,
    request_id: Uuid,
    allow: bool,
) -> Result<PermissionRequest, AgentError> {
    let request = chat
        .snapshot
        .permissions
        .iter()
        .find(|request| request.id == request_id)
        .cloned()
        .ok_or_else(|| AgentError::Invalid("permission request expired".into()))?;
    let answerable = match request.execution {
        PermissionExecution::ToolCall { run_id, .. } => {
            chat.run_cancel(run_id)
                .is_some_and(|cancel| !cancel.is_cancelled())
                && chat.snapshot.chat_loop.run_id == Some(run_id)
                && chat.snapshot.chat_loop.state == LoopState::WaitingPermission
        }
        PermissionExecution::UserShell { command_id } => {
            chat.snapshot.user_shells.iter().any(|shell| {
                shell.id == command_id
                    && shell.loop_id == Some(request.loop_id)
                    && shell.state == UserShellState::WaitingPermission
            })
        }
    };
    if !answerable {
        return Err(AgentError::Invalid("permission request cancelled".into()));
    }
    if allow && let Some(prepared) = request.file_change.as_deref() {
        let review = load_file_change_review(&prepared.artifact).map_err(|error| {
            AgentError::Invalid(format!(
                "file change review is unavailable; deny this request or restore the review artifact: {error}"
            ))
        })?;
        if review.summary() != prepared.summary {
            return Err(AgentError::Invalid(
                "file change review does not match the permission request; deny this request"
                    .into(),
            ));
        }
    }
    Ok(request)
}

fn empty_snapshot(id: ChatId, folder: Option<PathBuf>, model: ModelConfig) -> ChatSnapshot {
    ChatSnapshot {
        approval_records: Vec::new(),
        search_config: tickloop_harness_protocol::SearchConfig::enabled_default(),
        task_tools_enabled: false,
        subagent_reports: BTreeMap::new(),
        failure_events: Vec::new(),
        provider_failures: HashMap::new(),
        tool_call_recoveries: Vec::new(),
        artifact_ticks: Vec::new(),
        removed_artifact_ticks: Default::default(),
        monitor: MonitorMembership::default(),
        request_usage: RequestUsageLedger::new(),
        id,
        branch_origin: None,
        delegation: None,
        family_completions: Vec::new(),
        task_boards: BTreeMap::new(),
        scheduled_tasks: Vec::new(),
        scheduled_task_revocations: Vec::new(),
        task_board: None,
        goal: None,
        goal_history: Vec::new(),
        family_inputs: BTreeMap::new(),
        sent_agent_messages: BTreeMap::new(),
        generated_title: None,
        folder,
        model,
        reasoning_effort: None,
        thinking_enabled: true,
        messages: Vec::new(),
        invoked_skills: Vec::new(),
        user_skill_invocations: BTreeMap::new(),
        message_runs: BTreeMap::new(),
        trigger_origins: BTreeMap::new(),
        message_contexts: BTreeMap::new(),
        message_ids: BTreeMap::new(),
        agent_message_origins: BTreeMap::new(),
        human_family_origins: BTreeMap::new(),
        original_message_ids: BTreeMap::new(),
        prompt_execution_modes: BTreeMap::new(),
        inherited_prompts: HashSet::new(),
        host_notes: BTreeMap::new(),
        announced_host_tools: None,
        message_compactions: BTreeMap::new(),
        message_usage: BTreeMap::new(),
        message_sources: BTreeMap::new(),
        message_models: BTreeMap::new(),
        chat_loop: LoopSnapshot::new(tickloop_ids::generate().into()),
        last_sequence: 0,
        permissions: Vec::new(),
        questions: Vec::new(),
        pending_inputs: Vec::new(),
        pending_commands: Vec::new(),
        last_queue_sequence: 0,
        operations: Vec::new(),
        policy: PermissionPolicy::default(),
        additional_directories: Vec::new(),
        events: Vec::new(),
        pending_thinking: BTreeMap::new(),
        context_window_tokens: None,
        auto_compact_window: None,
        pending_model: None,
        plans: Vec::new(),
        progress: Vec::new(),
        commands: Vec::new(),
        user_shells: Vec::new(),
        last_model_run: None,
    }
}

impl Drop for Inner {
    /// When the last Agent handle is released, synchronously writes out Transcript entries not yet
    /// flushed.
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(recorder) = &self.recorder {
            recorder.flush_on_drop(self.chats.get_mut());
        }
    }
}

impl Agent {
    /// The Skills of a Chat with `folder`; a Chat without a Folder reads no Skill directory.
    fn discover_skills(
        &self,
        folder: Option<&Path>,
    ) -> Arc<tickloop_harness_policy::skills::SkillRegistry> {
        Arc::new(self.inner.settings.as_ref().map_or_else(
            || {
                if folder.is_some() {
                    tickloop_harness_policy::skills::SkillRegistry::builtin()
                } else {
                    tickloop_harness_policy::skills::SkillRegistry::builtin_without_ticks()
                }
            },
            |settings| settings.skills(folder),
        ))
    }

    /// Reads a Chat's Skills again, so a Tick package installed, updated or uninstalled by
    /// `/ticks` or `tickloop tick` in another process, or an edited user or project Skill, reaches
    /// the Chat at its next request or Skill invocation. Unchanged Skills give a byte-identical
    /// listing, so the model's prompt cache still holds.
    async fn rediscover_skills(&self, chat_id: ChatId) {
        let Some(Some(folder)) = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .map(|chat| chat.snapshot.folder.clone())
        else {
            return;
        };
        let skills = self.discover_skills(Some(&folder));
        let mut chats = self.inner.chats.lock().await;
        if let Some(chat) = chats.get_mut(&chat_id)
            && chat.snapshot.folder.as_ref() == Some(&folder)
        {
            chat.skills = skills;
        }
    }

    fn tool_definitions(&self, chat: &Chat) -> Vec<tickloop_harness_protocol::ToolDefinition> {
        self.tool_registry(chat).definitions()
    }

    /// The tools a request of `chat` offers, without refreshing what they depend on.
    fn tool_registry(&self, chat: &Chat) -> ToolRegistry {
        ToolRegistry::for_chat(
            chat,
            self.inner.host_tools.read().unwrap().clone(),
            self.inner.tick_stream_installed.load(Ordering::SeqCst),
            self.inner.host_options.prompt_policy,
            if chat.snapshot.task_tools_enabled {
                shared_tasks::TaskToolAccess::Enabled
            } else {
                shared_tasks::TaskToolAccess::Disabled
            },
            &self.inner.tool_policy,
            self.scheduled_task_terms(),
        )
    }

    /// Installs execution-owner services for Shell handoff. An environment is required.
    pub fn install_shell_runtime(&self, runtime: Arc<dyn tickloop_harness_tools::ShellRuntime>) {
        if self.inner.environment.is_some() {
            runtime.attach(Arc::new(self.downgrade()));
            *self.inner.shells.write().unwrap() = Some(runtime);
        }
    }

    pub(crate) fn shell_runtime(&self) -> Option<Arc<dyn tickloop_harness_tools::ShellRuntime>> {
        self.inner.shells.read().unwrap().clone()
    }

    #[cfg(feature = "test-support")]
    pub fn mcp_performance_counts(&self) -> (u64, u64, u64) {
        self.inner
            .host_tools
            .read()
            .unwrap()
            .as_ref()
            .map_or((0, 0, 0), |provider| provider.mcp_performance_counts())
    }

    #[cfg(feature = "test-support")]
    pub async fn benchmark_host_tool_preparation(
        &self,
        chat_id: ChatId,
    ) -> Result<usize, AgentError> {
        let provider = self.inner.host_tools.read().unwrap().clone();
        if let Some(provider) = &provider {
            provider
                .refresh(CancellationToken::new())
                .await
                .map_err(AgentError::Invalid)?;
        }
        let count = {
            let chats = self.inner.chats.lock().await;
            self.tool_definitions(chats.get(&chat_id).ok_or(AgentError::NotFound)?)
                .len()
        };
        if let Some(provider) = provider {
            provider.mcp_instructions();
        }
        Ok(count)
    }

    /// Registers the provider's current tools without refreshing remote directories. Call
    /// [`crate::host_tools::HostToolProvider::refresh`] to observe discovery failures.
    pub fn install_host_tools(
        &self,
        sources: impl crate::host_tools::IntoHostToolSources,
    ) -> Result<(), AgentError> {
        let sources = sources.into_sources();
        let reserved = reserved_host_tool_names();
        let serves_artifacts = sources
            .iter()
            .any(|source| source.artifact_client().is_some());
        let mut names = HashSet::new();
        for definition in sources.iter().flat_map(|source| source.definitions()) {
            let is_artifact_tool =
                crate::mcp_tools::ARTIFACT_ACCOUNT_TOOL_NAMES.contains(&definition.name.as_str());
            if definition.name.trim().is_empty()
                || (reserved.contains(&definition.name)
                    && definition.name != crate::tick_tools::TICK_MANAGE
                    && !(serves_artifacts && is_artifact_tool))
                || !names.insert(definition.name.clone())
            {
                return Err(AgentError::Invalid(format!(
                    "host tool name '{}' is empty, duplicated, or reserved",
                    definition.name
                )));
            }
        }
        *self.inner.host_tools.write().unwrap() =
            Some(crate::host_tools::CompositeHostTools::combine(sources));
        Ok(())
    }

    async fn skill_tool(
        &self,
        receipt: &Receipt,
        skills: &tickloop_harness_policy::skills::SkillRegistry,
        input: &serde_json::Value,
    ) -> Result<tickloop_harness_protocol::ToolResult, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SkillInput {
            skill: String,
            args: Option<String>,
        }

        let input = serde_json::from_value::<SkillInput>(input.clone()).map_err(|error| {
            format!(
                "invalid {} input: {error}",
                tickloop_harness_policy::skills::SKILL_TOOL_NAME
            )
        })?;
        if input.skill.starts_with("tickloop-artifacts:") {
            return self
                .remote_skill_tool(receipt, &input.skill, input.args.as_deref().unwrap_or(""))
                .await;
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or_else(|| "chat is unavailable".to_string())?;
        let invoked = skills
            .expand(
                &input.skill,
                input.args.as_deref().unwrap_or(""),
                tickloop_harness_policy::skills::SkillCaller::Model,
            )
            .and_then(tickloop_harness_protocol::InvokedSkill::try_from)
            .map_err(|error| error.to_string())?;
        let name = invoked.name.clone();
        let origin = invoked.source.display_label();
        let changed = record_invoked_skill(chat, invoked);
        self.save(&chat.snapshot);
        Ok(tickloop_harness_protocol::ToolResult {
            content: if changed {
                format!("Launching skill: {name} [{origin}]")
            } else {
                format!("Already loaded {name} [{origin}]. {ALREADY_IN_CONVERSATION}")
            },
            is_error: false,
            output_ref: None,
        })
    }

    async fn remote_skill_tool(
        &self,
        receipt: &Receipt,
        name: &str,
        arguments: &str,
    ) -> Result<tickloop_harness_protocol::ToolResult, String> {
        let provider = self
            .inner
            .host_tools
            .read()
            .unwrap()
            .clone()
            .ok_or("Artifact Skill source unavailable")?;
        let definition = provider
            .remote_skills()
            .into_iter()
            .find(|definition| definition.name == name)
            .ok_or("unknown or unavailable remote Skill")?;
        let (cancel, policy_revision) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or("chat unavailable")?;
            if chat.active_run != Some(receipt.run_id)
                || !remote_skill_allowed(chat, &definition.source)
            {
                return Err("remote Skill is outside this Run's service authority".into());
            }
            (
                chat.active.clone().ok_or("Run ended")?,
                chat.snapshot.policy.revision,
            )
        };
        let content = provider
            .read_remote_skill(&definition, None, cancel.clone())
            .await?;
        let mut invoked =
            tickloop_harness_policy::skills::expand_remote(&content, &definition, arguments)
                .map_err(|error| error.to_string())?;
        invoked.run_id = Some(receipt.run_id);
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id).ok_or("chat unavailable")?;
        if chat.snapshot.policy.revision != policy_revision {
            return Err("Skill permission policy changed during loading".into());
        }
        if cancel.is_cancelled()
            || chat.active_run != Some(receipt.run_id)
            || !remote_skill_allowed(chat, &definition.source)
            || !provider.remote_skill_is_current(&definition.source)
        {
            return Err("Skill source or Run changed during loading".into());
        }
        let changed = record_invoked_skill(chat, invoked);
        self.save(&chat.snapshot);
        Ok(tickloop_harness_protocol::ToolResult {
            content: if changed {
                format!("Loaded {name} · Tickloop Artifacts")
            } else {
                format!("Already loaded {name} · Tickloop Artifacts. {ALREADY_IN_CONVERSATION}")
            },
            is_error: false,
            output_ref: None,
        })
    }

    async fn synchronize_remote_skills(&self, receipt: &Receipt) {
        let provider = self.inner.host_tools.read().unwrap().clone();
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&receipt.chat_id) else {
            return;
        };
        let definitions: Vec<_> = provider
            .as_ref()
            .map(|provider| provider.remote_skills())
            .unwrap_or_default()
            .into_iter()
            .filter(|definition| {
                remote_skill_allowed(chat, &definition.source)
                    && chat.snapshot.policy.explicit_decision_with_session(
                        tickloop_harness_policy::skills::SKILL_TOOL_NAME,
                        &serde_json::json!({"skill":definition.name}),
                        receipt.loop_id.into(),
                        None,
                    ) != Some(Decision::Deny)
            })
            .collect();
        chat.snapshot
            .invoked_skills
            .retain(|skill| match &skill.source {
                tickloop_harness_protocol::SkillSource::Remote { .. } => {
                    definitions.iter().any(|definition| {
                        definition.name == skill.name && definition.source == skill.source
                    }) && provider
                        .as_ref()
                        .is_some_and(|provider| provider.remote_skill_is_current(&skill.source))
                }
                tickloop_harness_protocol::SkillSource::BuiltIn
                    if skill.name == "artifact-draft" =>
                {
                    false
                }
                _ => true,
            });
        for skill in &mut chat.snapshot.invoked_skills {
            if matches!(
                skill.source,
                tickloop_harness_protocol::SkillSource::Remote { .. }
            ) {
                skill.run_id = Some(receipt.run_id);
            }
        }
        Arc::make_mut(&mut chat.skills).replace_remote(definitions);
    }
    pub async fn observe(&self, chat: ChatId, after: u64) -> Result<Observation, AgentError> {
        let snapshot = self.snapshot(chat).await?;
        let first = snapshot
            .events
            .first()
            .map(|event| event.sequence)
            .unwrap_or(snapshot.last_sequence.saturating_add(1));
        let conversation_rewound = snapshot.events.iter().any(|event| {
            event.sequence > after && matches!(event.kind, EventKind::ConversationRewound { .. })
        });
        let reset_required = after > snapshot.last_sequence
            || after.saturating_add(1) < first
            || conversation_rewound;
        let events = snapshot
            .conversation_events()
            .into_iter()
            .filter(|event| event.sequence > after)
            .collect();
        Ok(Observation {
            snapshot,
            events,
            reset_required,
        })
    }
    pub async fn set_auto_reviewer(&self, client: Arc<dyn ModelClient>) {
        *self.inner.auto_reviewer.lock().await = Some(client);
    }

    /// Captures one immutable trusted approval context before selecting a reviewer.
    async fn review_auto_operation(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        prepared_facts: Option<tickloop_harness_protocol::PreparedApprovalFacts>,
        cancel: CancellationToken,
    ) -> Result<approval_assessment::ApprovalAssessment, AgentError> {
        self.review_operation(receipt, operation, prepared_facts, cancel, true)
            .await
    }

    async fn review_operation(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        prepared_facts: Option<tickloop_harness_protocol::PreparedApprovalFacts>,
        cancel: CancellationToken,
        emit_permission: bool,
    ) -> Result<approval_assessment::ApprovalAssessment, AgentError> {
        let (snapshot, bookkeeping, chat_client, boundary_id, root_goal) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or(AgentError::NotFound)?;
            let boundary_id =
                chat.snapshot
                    .message_compactions
                    .iter()
                    .rev()
                    .find_map(|(index, marker)| match marker {
                        CompactionMarker::Boundary(boundary)
                            if boundary.original_user_inputs.is_none() =>
                        {
                            chat.snapshot.message_ids.get(index).copied()
                        }
                        _ => None,
                    });
            let mut snapshot = chat.snapshot.clone();
            snapshot.policy =
                delegation::effective_policy(&chats, receipt).map_err(AgentError::Invalid)?;
            let root_goal = match &chat.snapshot.delegation {
                None => chat.snapshot.goal.clone(),
                Some(delegation) => chats
                    .get(&delegation.root_chat_id())
                    .and_then(|root| root.snapshot.goal.clone()),
            };
            (
                snapshot,
                chat.bookkeeping.clone(),
                chat.client.clone(),
                boundary_id,
                root_goal,
            )
        };
        // One load serves both the original prompts and the history before the boundary.
        let has_boundary = snapshot
            .message_compactions
            .values()
            .any(|marker| matches!(marker, CompactionMarker::Boundary(_)));
        let lines = if has_boundary {
            self.stored_lines(receipt.chat_id).await
        } else {
            None
        };
        let legacy = boundary_id
            .zip(lines.as_deref())
            .and_then(|(boundary_id, lines)| {
                crate::transcript::load_original_user_inputs(
                    lines,
                    boundary_id,
                    receipt.chat_id.into(),
                )
            });
        let ancestors = match &lines {
            Some(lines) => {
                self.approval_history_ancestors(&snapshot, Some(lines))
                    .await
            }
            None => None,
        };
        let (auto_settings, auto_revision) = match self.auto_settings_snapshot().await {
            Ok(settings) => settings,
            Err(error) => {
                let reason = format!(
                    "Auto reviewer settings are unavailable ({error}); review this action manually."
                );
                if emit_permission {
                    self.emit_permission_check(receipt, self.review_escalation(), reason.clone())
                        .await;
                }
                return Ok(approval_assessment::ApprovalAssessment::missing_context(
                    operation,
                    snapshot.policy.revision,
                    reason,
                    tickloop_harness_protocol::ApprovalDiagnostic::ReviewerFailure {
                        failure: ApprovalFailureKind::Configuration,
                    },
                ));
            }
        };
        let Some(folder) = snapshot.folder.clone() else {
            let reason =
                "This Chat has no Folder to review this action in; review it manually.".to_owned();
            if emit_permission {
                self.emit_permission_check(receipt, self.review_escalation(), reason.clone())
                    .await;
            }
            return Ok(approval_assessment::ApprovalAssessment::missing_context(
                operation,
                snapshot.policy.revision,
                reason,
                tickloop_harness_protocol::ApprovalDiagnostic::ReviewerFailure {
                    failure: ApprovalFailureKind::Configuration,
                },
            ));
        };
        let ids = self.chat_ids(snapshot.id).await;
        let context = approval_context::capture(
            &ids,
            &snapshot,
            &bookkeeping,
            operation,
            root_goal.as_ref(),
            legacy,
            &folder,
            auto_settings.policy_for_directory(&folder),
        );
        let mut context = match context {
            Ok(context) => context,
            Err(approval_context::ApprovalContextError::MissingOriginalUserInput) => {
                let reason = "Original user authorization for this Loop is unavailable; review this action manually.".to_owned();
                if emit_permission {
                    self.emit_permission_check(receipt, self.review_escalation(), reason.clone())
                        .await;
                }
                return Ok(approval_assessment::ApprovalAssessment::missing_context(
                    operation,
                    snapshot.policy.revision,
                    reason,
                    tickloop_harness_protocol::ApprovalDiagnostic::MissingAuthorization,
                ));
            }
        };
        if let Some(ancestors) = ancestors.as_deref() {
            context.reply_history = approval_context::capture_reply_history(
                &snapshot,
                &bookkeeping,
                Some((operation.run_id.into(), &operation.call_id)),
                &context.original_user_inputs,
                Some(ancestors),
            );
            context.tool_history =
                approval_context::recovered_tool_history(&snapshot, operation, Some(ancestors));
        }
        if let Some(evidence) = nested_calls::review_evidence().await {
            approval_context::append_nested_evidence(&mut context, evidence);
        }
        if operation.name == "ListSubagents"
            && operation
                .input
                .as_object()
                .is_some_and(|input| input.is_empty())
        {
            return Ok(approval_assessment::ApprovalAssessment::new(
                &context,
                AutoReview::Allow,
                None,
            )
            .with_auto_settings(auto_settings.reviewer, &auto_revision));
        }
        context.creation_receipt = prepared_facts
            .as_ref()
            .filter(|facts| facts.closes_tick)
            .and_then(|facts| {
                bookkeeping
                    .tick_creation_receipts
                    .iter()
                    .find(|proof| {
                        proof.loop_id == operation.loop_id
                            && facts.tick_instance.as_ref() == Some(&proof.instance)
                            && snapshot.operations.iter().any(|created| {
                                created.id == proof.operation_id
                                    && created.state == OperationState::Completed
                            })
                    })
                    .cloned()
            });
        context.prepared_facts = prepared_facts.map(|mut facts| {
            facts.normalized_input = approval_context::input_projection::project_tool_input(
                &operation.name,
                &facts.normalized_input,
                approval_context::input_projection::InputPurpose::Proposed,
            )
            .unwrap_or(serde_json::Value::Null);
            facts
        });
        let preferred = auto_settings.reviewer;
        let configured_reviewer = self.inner.auto_reviewer.lock().await.clone();
        let has_isolated_reviewer = configured_reviewer.is_some();
        let has_judgment_adapter = self.inner.judgment.lock().await.is_some();
        let llm_client = if let Some(client) = configured_reviewer {
            client
        } else {
            chat_client
                .auxiliary_client(
                    tickloop_harness_model::auxiliary::AuxiliaryTask::PermissionCheck,
                )?
                .unwrap_or(chat_client)
        };
        let auxiliary_model = (!has_isolated_reviewer)
            .then(|| {
                tickloop_harness_model::auxiliary::auxiliary_model_config(
                    &snapshot.model,
                    tickloop_harness_model::auxiliary::AuxiliaryTask::PermissionCheck,
                )
            })
            .flatten();
        let llm_budget = if let Some(model) = auxiliary_model.as_ref() {
            auto_mode::review_budget_bytes(model, None)
        } else {
            auto_mode::review_budget_bytes(
                &snapshot.model,
                crate::context::context_window(&snapshot),
            )
        };
        let use_jev = preferred == tickloop_harness_protocol::AutoReviewerPreference::Jev
            || (has_judgment_adapter && !has_isolated_reviewer);
        let assess = |run: auto_approval::ReviewerRun,
                      fallback_from: Option<ApprovalReviewerKind>,
                      fallback_failure: Option<ApprovalFailureKind>,
                      fallback_reason: Option<ApprovalFallbackReason>| {
            let review = if matches!(run.review, AutoReview::Allow)
                && (approval_context::input_projection::is_incomplete(&context.operation.input)
                    || context.prepared_facts.as_ref().is_some_and(|facts| {
                        approval_context::input_projection::is_incomplete(&facts.normalized_input)
                    })) {
                AutoReview::Ask {
                    diagnostics: vec![tickloop_harness_protocol::ApprovalDiagnostic::TaskScopeUncertain],
                    reason: "The reviewer received a bounded excerpt of this third-party tool input. Omitted details require manual review before execution.".into(),
                }
            } else {
                run.review
            };
            approval_assessment::ApprovalAssessment::new(&context, review, run.reviewer)
                .with_auto_settings(preferred, &auto_revision)
                .with_attempts(run.attempts, fallback_from, fallback_failure)
                .with_fallback_reason(fallback_reason)
        };
        if !use_jev {
            if emit_permission {
                self.emit_approval_review_started(receipt, operation, ApprovalReviewerKind::Llm)
                    .await;
            }
            let run = self
                .review_with_llm(receipt, &context, llm_budget, llm_client, cancel)
                .await?;
            if emit_permission {
                self.emit_auto_review_check(receipt, &run.review).await;
            }
            return Ok(assess(run, None, None, None));
        }

        let deadline = tokio::time::Instant::now() + auto_approval::JEV_REVIEW_BUDGET;
        let generation_before_binding = self.inner.auto_health.lock().await.generation();
        let bound = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Cancelled.into()),
            result = tokio::time::timeout_at(deadline, self.judgment_client()) => {
                result.unwrap_or(Err(ModelError::TimedOut))
            }
        };
        if cancel.is_cancelled() {
            return Err(ModelError::Cancelled.into());
        }
        let (client, connection_version, bind_failure) = match bound {
            Ok((client, version)) => (Some(client), version, None),
            Err(error) => (None, None, Some(auto_approval::failure_kind(&error))),
        };
        let (permit, skipped_reason) = {
            let mut health = self.inner.auto_health.lock().await;
            let now = tokio::time::Instant::now();
            let permit = health.begin_bound(connection_version, generation_before_binding, now);
            let reason = if permit.is_none() {
                Some(match health.status(now) {
                    AutoReviewerHealth::CoolingDown { .. } => ApprovalFallbackReason::CoolingDown,
                    AutoReviewerHealth::Probing => ApprovalFallbackReason::ProbeInProgress,
                    AutoReviewerHealth::Ready => ApprovalFallbackReason::ConnectionChanged,
                })
            } else {
                None
            };
            (permit, reason)
        };
        let mut jev_attempts = Vec::new();
        let mut fallback_failure = None;
        let mut fallback_reason = skipped_reason;
        if let Some(permit) = permit {
            let run = if let Some(client) = client {
                if emit_permission {
                    self.emit_approval_review_started(
                        receipt,
                        operation,
                        ApprovalReviewerKind::Jev,
                    )
                    .await;
                }
                self.review_with_jev(
                    receipt,
                    &context,
                    jev_review::review_budget_bytes(),
                    client,
                    deadline,
                    cancel.clone(),
                )
                .await
            } else {
                let failure = bind_failure.expect("failed binding has a failure kind");
                Ok(auto_approval::ReviewerRun::binding_failure(failure))
            };
            let run = match run {
                Ok(run) => run,
                Err(error) => {
                    self.inner.auto_health.lock().await.cancel(permit);
                    return Err(error);
                }
            };
            if cancel.is_cancelled() {
                self.inner.auto_health.lock().await.cancel(permit);
                return Err(ModelError::Cancelled.into());
            }
            if let Some(failure) = run.failure {
                fallback_failure = Some(failure);
                fallback_reason = Some(ApprovalFallbackReason::TechnicalFailure(failure));
                self.inner
                    .auto_health
                    .lock()
                    .await
                    .failure(permit, tokio::time::Instant::now());
                jev_attempts = run.attempts;
            } else {
                self.inner.auto_health.lock().await.success(permit);
                if emit_permission {
                    self.emit_auto_review_check(receipt, &run.review).await;
                }
                return Ok(assess(run, None, None, None));
            }
        }
        if cancel.is_cancelled() {
            return Err(ModelError::Cancelled.into());
        }
        if emit_permission {
            self.emit_approval_review_started(receipt, operation, ApprovalReviewerKind::Llm)
                .await;
        }
        let mut run = self
            .review_with_llm(receipt, &context, llm_budget, llm_client, cancel)
            .await?;
        jev_attempts.append(&mut run.attempts);
        run.attempts = jev_attempts;
        if emit_permission {
            self.emit_auto_review_check(receipt, &run.review).await;
        }
        Ok(assess(
            run,
            Some(ApprovalReviewerKind::Jev),
            fallback_failure,
            fallback_reason,
        ))
    }

    async fn emit_approval_review_started(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        reviewer: ApprovalReviewerKind,
    ) {
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id) {
            self.emit(
                chat,
                receipt,
                EventKind::ApprovalReviewStarted {
                    call_id: operation.call_id.clone(),
                    reviewer,
                },
            );
        }
    }

    async fn emit_auto_review_check(&self, receipt: &Receipt, review: &AutoReview) {
        let (decision, reason) = match review {
            AutoReview::Allow => (Decision::Allow, "Auto policy allowed the operation".into()),
            AutoReview::Block { reason, .. } => (Decision::Deny, reason.clone()),
            AutoReview::Ask { reason, .. } => (self.review_escalation(), reason.clone()),
            AutoReview::ContextLimit => (
                self.review_escalation(),
                auto_mode::REVIEW_CONTEXT_LIMIT_MESSAGE.into(),
            ),
            AutoReview::Unavailable { message } => (self.review_escalation(), message.clone()),
        };
        self.emit_permission_check(receipt, decision, reason).await;
    }

    async fn emit_permission_check(&self, receipt: &Receipt, decision: Decision, reason: String) {
        let mut chats = self.inner.chats.lock().await;
        if let Some(chat) = chats.get_mut(&receipt.chat_id) {
            self.emit(
                chat,
                receipt,
                EventKind::PermissionChecked { decision, reason },
            );
        }
    }

    /// `/stop`: interrupts the current turn and pauses receiving until the Chat is continued.
    /// Stops this Agent and all of its descendants.
    pub async fn stop(&self, chat_id: ChatId) -> Result<Vec<OperationRecord>, AgentError> {
        let stopped = self.stop_subagent_tree(chat_id, None).await?;
        if !stopped.failures.is_empty() {
            return Err(AgentError::Invalid(format!(
                "Agent subtree stop was incomplete: {}",
                serde_json::json!({"failures":stopped.failures,"unresolved_operations":stopped.operations})
            )));
        }
        Ok(stopped.operations)
    }

    /// Esc: interrupts the current turn, pauses an active goal, and cancels the self-paced
    /// loop's pending wake-up under the Chat's lease.
    pub async fn interrupt(&self, chat_id: ChatId) -> Result<Vec<OperationRecord>, AgentError> {
        let operations = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            chat.interrupt()
        };
        self.pause_goal(chat_id, goal::state::GoalEvent::Interrupted)
            .await;
        self.cancel_self_paced_wakeup_on_esc(chat_id).await;
        Ok(operations)
    }

    /// Sends the queued user message `run_id` now: interrupts the work ahead of it, after which
    /// it starts at once instead of at the next tool boundary. The check and the interruption
    /// share one lock, so the run answering a message that has already joined or started it is
    /// never interrupted. Such a message, an unknown one, or one with no work ahead is left as is.
    pub async fn send_now(
        &self,
        chat_id: ChatId,
        run_id: RunId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let is_queued_message = chat.snapshot.queued_inputs().iter().any(|queued| {
            queued.id == run_id && queued.armed && queued.kind == QueuedInputKind::Message
        });
        let has_work_ahead = chat.active.is_some() || !chat.parked.is_empty();
        if !is_queued_message || !has_work_ahead {
            return Ok(Vec::new());
        }
        Ok(chat.interrupt())
    }

    /// Opens a persistent Agent rooted at `root`. No Chat is loaded on open; use
    /// [`Agent::resume_chat`] to resume one by id and [`Agent::saved_chats`] to list saved Chats.
    ///
    /// Uses an in-memory Settings Store; file-backed composition is tested by Code.
    #[cfg(test)]
    pub fn open(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
    ) -> Result<Self, AgentError> {
        Self::open_with_options(root, model, client, AgentHostOptions::default())
    }

    #[cfg(test)]
    pub fn open_embedded(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
    ) -> Result<Self, AgentError> {
        Self::open_with_options(root, model, client, AgentHostOptions::embedded_defaults())
    }

    #[cfg(test)]
    pub fn open_with_options(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        host_options: AgentHostOptions,
    ) -> Result<Self, AgentError> {
        let store = transcript::JsonlTranscriptStore::open(root)
            .map_err(|error| AgentError::Storage(error.to_string()))?;
        let mut options = local_options(
            model,
            Some(Arc::new(
                tickloop_harness_policy::settings::MemorySettingsStore::default(),
            )),
            host_options,
        );
        options.local_state_root = Some(store.root().to_owned());
        let recorder = Some(persistence::ChatRecorder::new(Arc::new(store)));
        Ok(Self::build(
            options,
            Arc::new(harness::OneClient(client.clone())),
            client,
            recorder,
            Some(Arc::new(environment::FolderEnvironment::bare())),
            None,
        ))
    }

    /// Whether Chats are saved as Transcripts: `false` for Agents without persistence and when
    /// `cleanupPeriodDays` is 0.
    pub fn saves_chats(&self) -> bool {
        self.inner.recorder.is_some()
    }

    /// Chats loaded (created or resumed) by this process.
    pub async fn list_chats(&self) -> Vec<ChatSnapshot> {
        self.inner
            .chats
            .lock()
            .await
            .values()
            .map(|chat| chat.snapshot.clone())
            .collect()
    }

    /// An Agent that neither persists nor reads or writes user settings: Chats exist only in this
    /// process's memory.
    pub fn new(model: ModelConfig, client: Arc<dyn ModelClient>) -> Self {
        Self::new_local(model, client, None, AgentHostOptions::default())
    }

    pub fn new_embedded(model: ModelConfig, client: Arc<dyn ModelClient>) -> Self {
        Self::new_local(model, client, None, AgentHostOptions::embedded_defaults())
    }

    pub fn new_with_options(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        host_options: AgentHostOptions,
    ) -> Self {
        Self::new_local(model, client, None, host_options)
    }

    /// An Agent on this computer's files that keeps no Transcript.
    fn new_local(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        settings: Option<Arc<dyn SettingsStore>>,
        host_options: AgentHostOptions,
    ) -> Self {
        Self::build(
            HarnessOptions {
                settings,
                host: host_options,
                ..HarnessOptions::local(model)
            },
            Arc::new(harness::OneClient(client.clone())),
            client,
            None,
            Some(Arc::new(environment::FolderEnvironment::bare())),
            None,
        )
    }

    /// `client` is the one `models` built for `options.model`.
    fn build(
        options: HarnessOptions,
        models: Arc<dyn tickloop_harness_model::ModelClients>,
        client: Arc<dyn ModelClient>,
        recorder: Option<persistence::ChatRecorder>,
        environment: Option<Arc<dyn environment::ExecutionEnvironment>>,
        search_provider_factory: Option<
            Arc<dyn tickloop_harness_tools::web_search::SearchProviderFactory>,
        >,
    ) -> Self {
        let HarnessOptions {
            model,
            settings,
            local_state_root,
            search,
            host: host_options,
            tools: tool_policy,
            host_rules,
            task_tools,
            waits,
            system_prompt_addition,
            citation_style,
            request_gates,
            time_zone,
            scheduled_task_expiry,
            scheduled_task_min_interval,
            trust_model_scheduled_tasks,
        } = options;
        let model = tickloop_harness_model::cards::model_for_persistent_selection(model);
        let config_root = local_state_root;
        let task_tools_enabled = settings
            .as_ref()
            .map_or(task_tools.unwrap_or(false), |settings| {
                settings.read(None).task_tools_enabled
            });
        let agent = Self {
            inner: Arc::new(Inner {
                selected_model: std::sync::Mutex::new(chat_settings::SelectedModel {
                    config: model,
                    client,
                }),
                models,
                settings,
                search,
                search_provider_factory,
                host_options,
                waits,
                config_root,
                settings_writes: Mutex::new(()),
                task_tools_enabled: AtomicBool::new(task_tools_enabled),
                settings_warnings: std::sync::Mutex::new(HashMap::new()),
                provider_failures: std::sync::Mutex::new(HashMap::new()),
                trusted_working_directories: std::sync::Mutex::new(HashSet::new()),
                host_contexts: std::sync::Mutex::new(host_context::HostContexts::default()),
                subagent_invocations: Mutex::new(HashMap::new()),
                family_mailbox: Mutex::new(family_mailbox::FamilyMailbox::default()),
                ledger_changes: tokio::sync::watch::channel(()).0,
                title_requests: Mutex::new(HashMap::new()),
                shell_descriptions: Mutex::new(HashMap::new()),
                shell_description_events: broadcast::channel(256).0,
                subagent_registry: Mutex::new(HashMap::new()),
                chats: timing::ChatsMutex::new(HashMap::new()),
                events: broadcast::channel(256).0,
                user_shell_events: broadcast::channel(256).0,
                user_shell_cancellations: std::sync::Mutex::new(HashMap::new()),
                task_notifications: broadcast::channel(256).0,
                tasks: agent_tasks::AgentTasks::default(),
                recorder,
                chat_leases: loop_lease::ChatLeases::default(),
                trigger_source_states: std::sync::RwLock::new(Vec::new()),
                approvals: Mutex::new(HashMap::new()),
                question_answers: Mutex::new(HashMap::new()),
                closed: AtomicBool::new(false),
                upgrade_paused: AtomicBool::new(false),
                upgrade_adopted: AtomicBool::new(false),
                upgrade_gate: Arc::new(tokio::sync::RwLock::new(())),
                upgrade_guard: Mutex::new(None),
                upgrade_restore: Mutex::new(None),
                auto_reviewer: Mutex::new(None),
                judgment: Mutex::new(None),
                auto_health: Mutex::new(auto_health::AutoHealth::default()),
                host_tools: std::sync::RwLock::new(None),
                // Until a daemon grants its spawns and views, an Agent runs and shows no shell
                // command.
                shells: std::sync::RwLock::new(None),
                environment,
                tool_policy,
                citation_style,
                host_rules,
                system_prompt_addition,
                request_gates,
                tick_stream_installed: AtomicBool::new(false),
                tick_deliveries: std::sync::Mutex::new(
                    tick_subscriptions::TickDeliveries::default(),
                ),
                tick_wake: Notify::new(),
                #[cfg(any(test, feature = "test-support"))]
                goal_continuations_held: AtomicBool::new(false),
                schedule_clock: scheduled_tasks::ScheduleClock::new(
                    scheduled_tasks::resolve_zone(time_zone.as_deref())
                        .unwrap_or(chrono_tz::Tz::UTC),
                ),
                scheduled_task_rules: scheduled_tasks::TaskRules {
                    lifetime: scheduled_task_expiry,
                    min_interval: scheduled_task_min_interval,
                },
                trust_model_scheduled_tasks,
                scheduled_wake: Arc::new(Notify::new()),
                scheduled_timer_started: AtomicBool::new(false),
                session_permissions: Mutex::new(
                    session_permissions::SessionPermissionContext::default(),
                ),
                shutdown: CancellationToken::new(),
                handover: CancellationToken::new(),
            }),
        };
        agent.register_trigger_source_state(Arc::new(goal::GoalSourceState));
        agent.register_trigger_source_state(Arc::new(scheduled_tasks::ScheduledTasksState));
        agent
    }

    /// Refuses a local-Folder operation before it reads anything local, in an Agent whose Chats
    /// have no Folder.
    fn require_environment(&self) -> Result<(), AgentError> {
        match self.inner.environment {
            Some(_) => Ok(()),
            None => Err(AgentError::Invalid(NO_ENVIRONMENT.into())),
        }
    }

    fn require_working_directory_trust(&self, workdir: &Path) -> Result<(), AgentError> {
        if self.inner.host_options.directory_trust == DirectoryTrustPolicy::CallerManaged {
            return Ok(());
        }
        if self
            .inner
            .settings
            .as_ref()
            .is_some_and(|settings| settings.read(Some(workdir)).project.trusted)
            || self
                .inner
                .trusted_working_directories
                .lock()
                .unwrap()
                .contains(workdir)
        {
            return Ok(());
        }
        Err(AgentError::UntrustedWorkingDirectory {
            path: workdir.to_path_buf(),
        })
    }

    fn apply_prompt_policy(&self, decision: Decision) -> Decision {
        match (self.inner.host_options.prompt_policy, decision) {
            (PromptPolicy::DenyOnPrompt, Decision::Ask) => Decision::Deny,
            (PromptPolicy::Unrestricted, Decision::Ask | Decision::Check) => Decision::Allow,
            _ => decision,
        }
    }

    /// `apply_prompt_policy`, except that an Unrestricted host cannot waive a person or a reviewer
    /// that an explicit Ask rule or the tool's own permission class requires.
    fn apply_prompt_policy_keeping_required_approval(
        &self,
        decision: Decision,
        is_approval_required: bool,
    ) -> Decision {
        if is_approval_required
            && matches!(decision, Decision::Ask | Decision::Check)
            && self.inner.host_options.prompt_policy == PromptPolicy::Unrestricted
        {
            decision
        } else {
            self.apply_prompt_policy(decision)
        }
    }

    /// Where an Auto review that does not admit a call sends it: to a person, or to a denial on a
    /// host that cannot ask. Review runs under an Unrestricted host only for calls that host
    /// cannot waive, so their escalation still reaches a person.
    fn review_escalation(&self) -> Decision {
        if self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt {
            Decision::Deny
        } else {
            Decision::Ask
        }
    }

    pub async fn trust_working_directory(&self, workdir: PathBuf) -> Result<(), AgentError> {
        self.require_environment()?;
        let workdir = workdir
            .canonicalize()
            .map_err(|_| AgentError::Invalid("working directory does not exist".into()))?;
        let Some(settings) = self.inner.settings.clone() else {
            self.inner
                .trusted_working_directories
                .lock()
                .unwrap()
                .insert(workdir);
            return Ok(());
        };
        tokio::task::spawn_blocking(move || settings.write(SettingsUpdate::TrustDirectory(workdir)))
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    pub async fn create_chat(&self, folder: PathBuf) -> Result<ChatId, AgentError> {
        self.create_chat_with_id(tickloop_ids::generate().into(), Some(folder))
            .await
    }

    /// Creates a root Chat with rules that last only for this launch and its descendants.
    pub async fn create_chat_with_launch_rules(
        &self,
        folder: PathBuf,
        rules: Vec<PermissionRule>,
    ) -> Result<ChatId, AgentError> {
        for rule in &rules {
            tickloop_harness_tools::permissions::validate_rule(rule)
                .map_err(|reason| AgentError::Invalid(reason.into()))?;
            if rule.persistent
                || rule.loop_id.is_some()
                || !matches!(rule.decision, Decision::Allow | Decision::Deny)
            {
                return Err(AgentError::Invalid(
                    "launch rules must be nonpersistent allow or deny rules for the whole Chat"
                        .into(),
                ));
            }
        }
        self.create_chat_with_delegation(tickloop_ids::generate().into(), Some(folder), None, rules)
            .await
    }

    /// A new Chat without a Folder, in an Agent without an Execution Environment.
    pub async fn create_chat_without_folder(&self) -> Result<ChatId, AgentError> {
        self.create_chat_with_id(tickloop_ids::generate().into(), None)
            .await
    }

    pub async fn restore_empty_chat(
        &self,
        chat_id: ChatId,
        folder: PathBuf,
    ) -> Result<(), AgentError> {
        if self.inner.chats.lock().await.contains_key(&chat_id) {
            return Ok(());
        }
        self.create_chat_with_id(chat_id, Some(folder)).await?;
        Ok(())
    }

    async fn create_chat_with_id(
        &self,
        id: ChatId,
        folder: Option<PathBuf>,
    ) -> Result<ChatId, AgentError> {
        self.create_chat_with_delegation(id, folder, None, Vec::new())
            .await
    }

    /// The canonical Folder of a new Chat. Chats have a Folder exactly when the Agent has an
    /// Execution Environment.
    fn admit_folder(&self, folder: Option<PathBuf>) -> Result<Option<PathBuf>, AgentError> {
        let folder = match (folder, &self.inner.environment) {
            (None, None) => return Ok(None),
            (Some(folder), Some(_)) => folder,
            (Some(_), None) => {
                return Err(AgentError::Invalid(NO_ENVIRONMENT.into()));
            }
            (None, Some(_)) => {
                return Err(AgentError::Invalid("a new Chat needs a Folder".into()));
            }
        };
        let folder = folder
            .canonicalize()
            .map_err(|_| AgentError::Invalid("working directory does not exist".into()))?;
        if !folder.is_dir() {
            return Err(AgentError::Invalid(
                "working directory must be a directory".into(),
            ));
        }
        self.require_working_directory_trust(&folder)?;
        Ok(Some(folder))
    }

    async fn create_chat_with_delegation(
        &self,
        id: ChatId,
        folder: Option<PathBuf>,
        inherited: Option<InheritedChat>,
        launch_rules: Vec<PermissionRule>,
    ) -> Result<ChatId, AgentError> {
        let _upgrade = self.inner.upgrade_gate.read().await;
        let folder = self.admit_folder(folder)?;
        if let Some(recorder) = &self.inner.recorder {
            recorder.register_chat(id, folder.as_deref()).await?;
        }
        let mut snapshot = empty_snapshot(id, folder, ModelConfig::unselected());
        let mut bookkeeping = ChatBookkeeping {
            dependency_baseline: Some(
                snapshot
                    .folder
                    .as_deref()
                    .map(approval_context::dependency_fingerprints)
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        let _settings_gate = self.inner.settings_writes.lock().await;
        let (client, output_style, user_context, announced_date) = if let Some(InheritedChat {
            additional_directory_sources,
            metadata,
            parent,
            client,
            output_style,
            user_context,
            announced_date,
        }) = inherited
        {
            bookkeeping.additional_directory_sources = additional_directory_sources;
            snapshot.model = parent.model;
            snapshot.reasoning_effort = parent.reasoning_effort;
            snapshot.thinking_enabled = parent.thinking_enabled;
            snapshot.search_config = parent.search_config;
            snapshot.task_tools_enabled = parent.task_tools_enabled;
            snapshot.auto_compact_window = parent.auto_compact_window;
            snapshot.additional_directories = metadata.ceiling.additional_directories.clone();
            let mut metadata = metadata;
            self.apply_host_rules(&mut metadata.ceiling.policy);
            snapshot.policy = metadata.ceiling.policy.clone();
            snapshot.delegation = Some(metadata);
            (client, output_style, user_context, announced_date)
        } else {
            let (client, output_style) = self.apply_chat_defaults(&mut snapshot, &mut bookkeeping);
            snapshot.policy.launch_rules = launch_rules;
            (client, output_style, OnceLock::new(), None)
        };
        if let SearchDefaults::Fixed(config) = &self.inner.search {
            snapshot.search_config = config.clone();
        }
        self.inner
            .subagent_registry
            .lock()
            .await
            .entry(id)
            .or_default();
        let tools = self.create_chat_tools(&snapshot);
        let skills = self.discover_skills(snapshot.folder.as_deref());
        let file_history = snapshot
            .folder
            .as_deref()
            .map(|folder| FileHistory::new(self.file_history_directory(id, folder)));
        let mut chats = self.inner.chats.lock().await;
        if let Some(metadata) = &mut snapshot.delegation {
            let sessions = self.inner.session_permissions.lock().await;
            let parent = PermissionOwner::new(
                metadata.parent_chat_id.into(),
                metadata.parent_loop_id.into(),
            )
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
            if let Some(denials) = &mut metadata.ceiling.session_denials {
                for denial in sessions.freeze_denials(parent) {
                    if !denials.contains(&denial) {
                        denials.push(denial);
                    }
                }
            }
            self.record_delegation(id, metadata.clone(), snapshot.search_config.clone());
        }
        self.save(&snapshot);
        chats.insert(
            id,
            NewChat {
                snapshot,
                bookkeeping,
                client,
                output_style,
                user_context,
                announced_date,
                tools,
                skills,
                file_history,
            }
            .into_chat(),
        );
        Ok(id)
    }

    pub async fn submit(&self, chat_id: ChatId, input: String) -> Result<Receipt, AgentError> {
        let request = TurnRequest::after_current_leaf(input, Vec::new(), Vec::new(), None);
        Ok(self.advance(chat_id, request).await?.receipt().clone())
    }

    pub async fn invoke_skill(
        &self,
        chat_id: ChatId,
        name: &str,
        arguments: &str,
    ) -> Result<Receipt, AgentError> {
        self.invoke_skill_in_host_context(chat_id, name, arguments, None)
            .await
    }

    /// Queues a user Skill invocation after the current leaf as a new input, as the Worker's
    /// Skill does.
    pub async fn invoke_skill_in_host_context(
        &self,
        chat_id: ChatId,
        name: &str,
        arguments: &str,
        host_context: Option<HostCallContext>,
    ) -> Result<Receipt, AgentError> {
        let turn = self
            .advance(
                chat_id,
                TurnRequest {
                    input_id: tickloop_ids::generate().into(),
                    op: TurnOp::Skill {
                        after: ParentSelection::CurrentLeaf,
                        skill: SkillInvocation {
                            name: name.into(),
                            arguments: arguments.into(),
                        },
                    },
                    delivery: Delivery::Queue,
                    host_context,
                },
            )
            .await?;
        Ok(turn.receipt().clone())
    }

    /// Expands a user Skill and submits its invocation.
    async fn submit_skill(
        &self,
        chat_id: ChatId,
        skill: SkillInvocation,
        host_context: Option<HostCallContext>,
        ledger: lifecycle::LedgerInput,
        turn: TurnAdmission,
    ) -> Result<Receipt, AgentError> {
        if !self.inner.tool_policy.user_skills {
            return Err(AgentError::Invalid(
                "user Skill invocation is not available in this Chat".into(),
            ));
        }
        self.rediscover_skills(chat_id).await;
        let invoked = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat.skills
                .expand(
                    &skill.name,
                    &skill.arguments,
                    tickloop_harness_policy::skills::SkillCaller::User,
                )
                .and_then(tickloop_harness_protocol::InvokedSkill::try_from)
                .map_err(|error| AgentError::Invalid(error.to_string()))?
        };
        let input = if skill.arguments.trim().is_empty() {
            format!("/{}", invoked.name)
        } else {
            format!("/{} {}", invoked.name, skill.arguments.trim())
        };
        // The host's reading of the user's /loop: the one task the turn creates for the user.
        let user_loop = (invoked.source == tickloop_harness_protocol::SkillSource::BuiltIn
            && invoked.name == tickloop_harness_policy::skills::LOOP_SKILL_NAME)
            .then(|| tickloop_harness_policy::loop_skill::LoopSchedule::of(&skill.arguments).ok())
            .flatten();
        // The self-paced task names the turn it starts before the turn is submitted.
        let run_id = tickloop_ids::generate();
        let started_loop = match &user_loop {
            Some(tickloop_harness_policy::loop_skill::LoopSchedule::SelfPaced { prompt }) => {
                self.start_users_self_paced_loop(chat_id, prompt.clone(), run_id.into())
                    .await?
            }
            Some(tickloop_harness_policy::loop_skill::LoopSchedule::Cron { .. }) | None => None,
        };
        let submitted = self
            .submit_internal(
                chat_id,
                input,
                SubmissionOptions {
                    invoked_skill: Some(invoked),
                    user_loop,
                    run_id: started_loop.is_some().then_some(run_id.into()),
                    host_context,
                    ledger: Some(ledger),
                    turn,
                    ..Default::default()
                },
            )
            .await;
        if let Some(task_id) = started_loop
            && submitted.is_err()
        {
            let _ = self.delete_scheduled_task(chat_id, task_id).await;
        }
        submitted
    }

    /// The Agent's MCP servers as they stand once its current configuration is taken up.
    pub async fn mcp_servers(&self) -> Vec<tickloop_harness_protocol::mcp::McpServerStatus> {
        let provider = self.inner.host_tools.read().unwrap().clone();
        match provider {
            Some(provider) => provider.mcp_server_statuses(CancellationToken::new()).await,
            None => Vec::new(),
        }
    }

    /// Connects the Agent's MCP server `name` again, at the user's request.
    pub async fn reconnect_mcp_server(&self, name: &str) -> Result<(), AgentError> {
        let provider = self.inner.host_tools.read().unwrap().clone();
        match provider {
            Some(provider) if provider.reconnect_mcp_server(name).await => Ok(()),
            _ => Err(AgentError::Invalid(format!(
                "no MCP server is named {name}"
            ))),
        }
    }

    pub async fn skill_warnings(&self, chat_id: ChatId) -> Result<Vec<String>, AgentError> {
        self.rediscover_skills(chat_id).await;
        Ok(self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .skills
            .warnings()
            .to_vec())
    }

    /// Checks that the user may invoke `name` in a new Chat with `folder`, before any Chat exists.
    pub fn check_user_skill(
        &self,
        folder: &Path,
        name: &str,
        arguments: &str,
    ) -> Result<(), AgentError> {
        self.require_environment()?;
        if !self.inner.tool_policy.user_skills {
            return Err(AgentError::Invalid(
                "user Skill invocation is not available in this Chat".into(),
            ));
        }
        self.discover_skills(Some(folder))
            .expand(
                name,
                arguments,
                tickloop_harness_policy::skills::SkillCaller::User,
            )
            .and_then(tickloop_harness_protocol::InvokedSkill::try_from)
            .map(|_| ())
            .map_err(|error| AgentError::Invalid(error.to_string()))
    }

    pub async fn has_skill(&self, chat_id: ChatId, name: &str) -> Result<bool, AgentError> {
        self.rediscover_skills(chat_id).await;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        match chat.skills.resolve_user_name(name) {
            Ok(_) => Ok(true),
            Err(tickloop_harness_policy::skills::SkillError::Unknown(_)) => Ok(false),
            Err(error) => Err(AgentError::Invalid(error.to_string())),
        }
    }

    pub async fn submit_with_file_references(
        &self,
        chat_id: ChatId,
        input: String,
        paths: Vec<PathBuf>,
    ) -> Result<Receipt, AgentError> {
        let request = TurnRequest::after_current_leaf(input, Vec::new(), paths, None);
        Ok(self.advance(chat_id, request).await?.receipt().clone())
    }

    /// Submits a prompt with attachments already in hand, such as pasted images or those
    /// restored from a saved prompt, followed by the `@` references in `paths`.
    pub async fn submit_with_input_contexts(
        &self,
        chat_id: ChatId,
        input: String,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
    ) -> Result<Receipt, AgentError> {
        let request = TurnRequest::after_current_leaf(input, contexts, paths, None);
        Ok(self.advance(chat_id, request).await?.receipt().clone())
    }

    /// Queues a prompt after the current leaf as a new input, as the Worker's Submit does.
    pub async fn submit_with_input_contexts_in_host_context(
        &self,
        chat_id: ChatId,
        input: String,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
        host_context: Option<HostCallContext>,
    ) -> Result<Receipt, AgentError> {
        let request = TurnRequest::after_current_leaf(input, contexts, paths, host_context);
        Ok(self.advance(chat_id, request).await?.receipt().clone())
    }

    async fn submit_contexts(
        &self,
        submission: family::ContextSubmission,
    ) -> Result<Receipt, AgentError> {
        let family::ContextSubmission {
            chat_id,
            input,
            mut contexts,
            paths,
            host_context,
            directed_source,
            target_generation,
            ledger,
            turn,
        } = submission;
        if contexts.len().saturating_add(paths.len()) > input_context::MAX_REFERENCES {
            return Err(AgentError::Invalid(format!(
                "a prompt can attach at most {} files or directories",
                input_context::MAX_REFERENCES
            )));
        }
        if !input_context::restored_contexts_are_valid(&contexts) {
            return Err(AgentError::Invalid(
                "restored prompt context is invalid, duplicated, or too large".into(),
            ));
        }
        let admission = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if let Some(ledger) = &ledger
                && let Some(replay) = lifecycle::replay(chat, ledger)
            {
                return replay;
            }
            let snapshot = &chat.snapshot;
            if snapshot.folder.is_none() {
                if !paths.is_empty() || contexts.iter().any(|context| context.path().is_some()) {
                    return Err(AgentError::Invalid(format!(
                        "{FOLDERLESS_CHAT}, so a prompt cannot attach local files"
                    )));
                }
                drop(chats);
                return self
                    .submit_internal(
                        chat_id,
                        input,
                        SubmissionOptions {
                            contexts,
                            directed_human: directed_source,
                            family_generation: target_generation,
                            host_context,
                            ledger,
                            turn,
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let workspace = tickloop_harness_tools::Workspace {
                folder: snapshot.folder.clone(),
                additional_directories: snapshot.additional_directories.clone(),
            };
            let reads = paths
                .iter()
                .map(PathBuf::as_path)
                .chain(contexts.iter().filter_map(InputContext::path))
                .map(|path| {
                    tickloop_harness_tools::prepare(
                        &workspace,
                        tickloop_harness_tools::READ_TOOL_NAME,
                        &serde_json::json!({"file_path":path}),
                    )
                    .map_err(|error| AgentError::Invalid(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let admission = family::AttachmentAdmission { workspace, reads };
            let sessions = self.inner.session_permissions.lock().await;
            Self::validate_attachment_admission(&chats, &sessions, chat_id, &admission)?;
            admission
        };
        let workspace = admission.workspace.clone();
        let paths: Vec<PathBuf> = admission
            .reads
            .iter()
            .take(paths.len())
            .map(|prepared| {
                PathBuf::from(
                    prepared.input["file_path"]
                        .as_str()
                        .expect("prepared Read has a canonical path"),
                )
            })
            .collect();
        let mut attached_file_reads = Vec::new();
        if !paths.is_empty() {
            let loaded =
                tokio::task::spawn_blocking(move || input_context::load(&workspace, paths))
                    .await
                    .map_err(|_| {
                        AgentError::Invalid("referenced paths could not be loaded".into())
                    })?
                    .map_err(AgentError::Invalid)?;
            for context in loaded.contexts {
                contexts.retain(|existing| existing.path() != context.path());
                contexts.push(context);
            }
            attached_file_reads = loaded.file_reads;
        }
        self.submit_internal(
            chat_id,
            input,
            SubmissionOptions {
                contexts,
                attached_file_reads,
                directed_human: directed_source,
                family_generation: target_generation,
                attachment_admission: Some(admission),
                host_context,
                ledger,
                turn,
                ..Default::default()
            },
        )
        .await
    }

    async fn submit_internal(
        &self,
        chat_id: ChatId,
        input: String,
        options: SubmissionOptions,
    ) -> Result<Receipt, AgentError> {
        let SubmissionOptions {
            task_delegation,
            attachment_admission,
            family_generation,
            contexts,
            attached_file_reads,
            denial_reference,
            trigger_source,
            trigger_lease,
            trigger_commit,
            invoked_skill,
            user_loop,
            run_id: chosen_run_id,
            host_context,
            agent_message,
            directed_human,
            ledger,
            mut turn,
        } = options;
        // A user input continues the Chat: its Loop receives Triggers again.
        let continues_chat = trigger_source.is_none() && agent_message.is_none();
        // Every queued input has its ledger record; one the Agent queues for itself is named by
        // its source where the source names it.
        let ledger = ledger.unwrap_or_else(|| {
            if let Some(source) = &trigger_source {
                return wakeup::trigger_ledger(chat_id, source, &input);
            }
            let (id, op) = match (&agent_message, &directed_human) {
                (Some(source), _) => (source.message_id, InputOp::AgentMessage),
                (_, Some(source)) => (source.message_id, InputOp::AgentMessage),
                (None, None) => (tickloop_ids::generate(), InputOp::Continue),
            };
            lifecycle::LedgerInput::new(id.into(), op, &input)
        });
        // Held through every task this submission starts: `shutdown` either refuses it or waits
        // for those tasks.
        let _admission = self.inner.tasks.token();
        if self.inner.closed.load(Ordering::SeqCst)
            || self.inner.upgrade_paused.load(Ordering::SeqCst)
        {
            return Err(AgentError::Invalid(
                "agent is unavailable during shutdown or planned upgrade".into(),
            ));
        }
        if input.trim().is_empty() {
            return Err(AgentError::Invalid("input cannot be empty".into()));
        }
        let mut family_mailbox = if continues_chat {
            Some(self.inner.family_mailbox.lock().await)
        } else {
            None
        };
        let mut chats = self.inner.chats.lock().await;
        if self.inner.closed.load(Ordering::SeqCst)
            || self.inner.upgrade_paused.load(Ordering::SeqCst)
        {
            return Err(AgentError::Invalid(
                "agent is unavailable during shutdown or planned upgrade".into(),
            ));
        }
        // Checked again under this lock: a concurrent first submission of the same input may
        // have been accepted while this one was being prepared.
        if let Some(replay) =
            lifecycle::replay(chats.get(&chat_id).ok_or(AgentError::NotFound)?, &ledger)
        {
            return replay;
        }
        if let Some(admission) = &attachment_admission {
            let sessions = self.inner.session_permissions.lock().await;
            Self::validate_attachment_admission(&chats, &sessions, chat_id, admission)?;
        }
        if matches!(trigger_source, Some(TriggerSource::GoalContinuation { .. }))
            && goal::background_work_runs(&chats, chat_id)
        {
            return Err(AgentError::Invalid(
                "the goal's continuation waits for the Chat's background work".into(),
            ));
        }
        let original = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if original.family_stopping {
            return Err(AgentError::Invalid("Subagent subtree is stopping".into()));
        }
        let staged = match (turn.branch.take(), original.branch_reservation) {
            (Some(switch), Some(reserved)) if switch.reservation() == reserved => {
                Some(switch.into_chat(original))
            }
            (None, None) => None,
            _ => return Err(chat_is_busy()),
        };
        let base = staged.as_ref().map_or(&*original, |staged| &staged.chat);
        let cut = match turn.placement {
            Some(placement) => {
                rewind::ensure_idle(base).map_err(|_| chat_is_busy())?;
                turn::placement_cut(base, placement)?
            }
            None => None,
        };
        if family_generation.is_some_and(|expected| expected != original.family_generation) {
            return Err(AgentError::Invalid(
                "directed target changed while preparing the input".into(),
            ));
        }
        let base = staged.as_ref().map_or(&*original, |staged| &staged.chat);
        let inherit_branch_session = continues_chat && base.snapshot.branch_origin.is_some();
        let mut candidate = base.clone();
        Self::prepare_cut(base, &mut candidate, cut)?;
        let chat = &mut candidate;
        let is_skill_invocation = invoked_skill.is_some();
        let loop_id = chat.snapshot.chat_loop.id;
        let feedback_input = trigger_source
            .as_ref()
            .and_then(|source| event_sources::feedback_identity(chat_id, loop_id, source));
        let run_id = feedback_input
            .as_ref()
            .map(|feedback| feedback.delivery_id)
            .or(chosen_run_id.map(Into::into))
            .unwrap_or_else(tickloop_ids::generate);
        let receipt = Receipt {
            chat_id,
            loop_id,
            run_id: run_id.into(),
        };
        if let Some(user_loop) = user_loop {
            chat.user_loops.insert(run_id.into(), user_loop);
        }
        if let Some(mut invoked_skill) = invoked_skill {
            chat.snapshot.user_skill_invocations.insert(
                run_id.into(),
                tickloop_harness_protocol::UserSkillInvocation {
                    name: invoked_skill.name.clone(),
                    source: invoked_skill.source.clone(),
                },
            );
            if invoked_skill.source == tickloop_harness_protocol::SkillSource::BuiltIn
                && chat.skills.is_user_only(&invoked_skill.name)
            {
                invoked_skill.run_id = Some(run_id.into());
            }
            record_invoked_skill(chat, invoked_skill);
        } else if trigger_source.is_none() && agent_message.is_none() {
            carry_user_skills_past_sign_in(chat, loop_id, run_id.into());
        }
        let requested_receipt = receipt.clone();
        if trigger_source.is_none() {
            self.apply_pending_model(chat)?;
            recovery::before_resume(
                &mut chat.snapshot,
                loop_id,
                self.inner.config_root.as_deref(),
            )?;
        }
        if !chat.snapshot.accepts_images()
            && contexts
                .iter()
                .any(|context| matches!(context, InputContext::Image(_)))
        {
            return Err(AgentError::ImageInputUnsupported);
        }
        if trigger_lease
            .as_ref()
            .is_some_and(|(holding, _)| !self.inner.chat_leases.stands(chat_id, *holding))
        {
            return Err(AgentError::Invalid(
                "this Chat's Trigger lease changed hands; the delivery waits with its source"
                    .into(),
            ));
        }
        if let Some(source) = &trigger_source
            && let Some(waiting) = self.admit_trigger(chat, source)?
        {
            self.record_merged_trigger(chat, &ledger, source, &waiting);
            *original = candidate;
            let commits = trigger_commit.is_some();
            if let Some(commit) = trigger_commit {
                self.commit_trigger_source(original, commit, &waiting);
            }
            self.save(&original.snapshot);
            drop(chats);
            if commits {
                self.checkpoint_trigger_acceptance(chat_id).await;
            }
            self.inner.ledger_changes.send_replace(());
            return Ok(waiting);
        }
        let chat_loop = &mut chat.snapshot.chat_loop;
        if !matches!(
            chat_loop.state,
            LoopState::Running | LoopState::WaitingPermission | LoopState::WaitingQuestion
        ) {
            chat_loop.set_state(LoopState::Queued);
        }
        let index = if trigger_source.is_none() {
            chat.snapshot
                .pending_inputs
                .iter()
                .position(|pending| {
                    pending.trigger_source.is_some()
                        || (agent_message.is_none() && pending.agent_message.is_some())
                })
                .unwrap_or(chat.snapshot.pending_inputs.len())
        } else {
            chat.snapshot.pending_inputs.len()
        };
        chat.snapshot.last_queue_sequence += 1;
        let queued_sequence = chat.snapshot.last_queue_sequence;
        let directed_origin = directed_human.clone();
        let queued = PendingInput {
            receipt: receipt.clone(),
            input,
            contexts,
            agent_message,
            directed_human,
            denial_reference,
            trigger_source,
            armed: true,
            queued_sequence,
            task_continuation: false,
            is_skill_invocation,
            attached_file_reads,
        };
        chat.bookkeeping.lifecycle.accept(&ledger, &queued);
        chat.snapshot.pending_inputs.insert(index, queued);
        if let Some(feedback_input) = feedback_input {
            chat.feedback_inputs.insert(receipt.run_id, feedback_input);
        }
        // A Chat without a Folder runs no command, so nothing watches its commands.
        let watch =
            (chat.snapshot.folder.is_some() && !chat.watching_commands).then(|| chat.tools.clone());
        if watch.is_some() {
            chat.watching_commands = true;
        }
        let mut session_context = None;
        let mut session_candidate = None;
        if inherit_branch_session {
            let context = self.inner.session_permissions.lock().await;
            let mut candidate = context.clone();
            candidate
                .inherit_loop(chat_id.into(), loop_id.into())
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
            session_context = Some(context);
            session_candidate = Some(candidate);
        }
        let selected_leaf = staged.map(|staged| self.install_branch(original, staged));
        if let Some((index, _)) = cut {
            self.commit_cut(original, &mut candidate, index);
        }
        if let Some(mailbox) = family_mailbox.as_mut() {
            mailbox.stopped_runs.remove(&chat_id);
            family::resume_messages(&mut candidate);
            self.persist_family_message_hold(&candidate, mailbox);
        }
        *original = candidate;
        if cut.is_none()
            && let Some(leaf) = selected_leaf
        {
            self.emit_conversation_rewound(original, leaf);
        }
        if turn.send_now && (original.active.is_some() || !original.parked.is_empty()) {
            original.interrupt();
        }
        let commits = trigger_commit.is_some();
        if let Some(commit) = trigger_commit {
            self.commit_trigger_source(original, commit, &receipt);
        }
        if let Some(source) = directed_origin {
            let source_chat = chats
                .get_mut(&source.source_chat_id)
                .ok_or(AgentError::NotFound)?;
            self.record_family_delivery(source_chat, &source, &receipt);
        }
        self.remember_host_context(&receipt, host_context);
        if let (Some(mut context), Some(candidate)) = (session_context, session_candidate) {
            *context = candidate;
        }
        drop(chats);
        drop(family_mailbox);
        if commits {
            self.checkpoint_trigger_acceptance(chat_id).await;
        }
        // Accepted: the run it may start enters the boundary again to take it.
        drop(trigger_lease);
        if let Some(parent) = task_delegation {
            self.bind_task_delegate(&parent, &receipt)
                .await
                .map_err(AgentError::Storage)?;
        }
        let next = match self.next_run(chat_id).await {
            Ok(next) => next,
            Err(error) => {
                return Err(error);
            }
        };
        if let Some(tools) = watch {
            self.watch_commands(chat_id, tools).await;
        }
        #[cfg(test)]
        turn::admission_pause::wait_if_paused(chat_id).await;
        if let Some(next) = next {
            self.run_task(next);
        }
        self.inner.ledger_changes.send_replace(());
        self.start_title_generation(chat_id).await;
        if continues_chat {
            Box::pin(self.continue_receiving(chat_id)).await;
        }
        Ok(requested_receipt)
    }

    fn emit(&self, chat: &mut Chat, receipt: &Receipt, kind: EventKind) {
        chat.bookkeeping
            .run_loops
            .insert(receipt.run_id, receipt.loop_id);
        self.publish_event(chat, receipt, kind);
    }

    fn publish_event(&self, chat: &mut Chat, receipt: &Receipt, kind: EventKind) {
        lifecycle::claim_response_messages(chat);
        chat.snapshot.last_sequence += 1;
        let event = Event {
            sequence: chat.snapshot.last_sequence,
            chat_id: receipt.chat_id,
            loop_id: receipt.loop_id,
            run_id: receipt.run_id,
            kind,
        };
        if matches!(event.kind, EventKind::Failed(_)) {
            chat.snapshot.failure_events.push(event.clone());
        }
        chat.snapshot.events.push(event.clone());
        if chat.snapshot.events.len() > EVENT_REPLAY_LIMIT {
            chat.snapshot.events.remove(0);
        }
        // Streamed text and the servers a request waits for only show progress: no record changes.
        if !matches!(
            event.kind,
            EventKind::Text(_) | EventKind::WaitingForMcpServers { .. }
        ) {
            self.save(&chat.snapshot);
        }
        let _ = self.inner.events.send(event);
    }

    #[tracing::instrument(target = "tickloop::request_timing", skip_all, fields(chat = ?receipt.chat_id, run = ?receipt.run_id))]
    async fn model_request(
        &self,
        receipt: &Receipt,
        mut request: ModelRequest,
        registry: &mut ToolRegistry,
        cancel: CancellationToken,
        used_calls: &mut HashSet<String>,
    ) -> Result<tickloop_harness_model::ModelOutput, AgentError> {
        {
            let chats = self.inner.chats.lock().await;
            if cancel.is_cancelled()
                || chats
                    .get(&receipt.chat_id)
                    .is_none_or(|chat| chat.active_run != Some(receipt.run_id))
            {
                return Err(tickloop_harness_model::ModelError::Cancelled.into());
            }
        }
        self.acknowledge_task_assignments(receipt)
            .await
            .map_err(AgentError::Invalid)?;
        if self
            .maybe_compact(receipt, &registry.definitions(), cancel.clone())
            .await?
        {
            let provider = self.inner.host_tools.read().unwrap().clone();
            if let Some(provider) = provider {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(ModelError::Cancelled.into()),
                    _ = provider.refresh(cancel.clone()) => {},
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(ModelError::Cancelled.into());
        }
        self.synchronize_remote_skills(receipt).await;
        let host_tools =
            host_provider_is_confirmed(&self.inner.host_tools).then(|| registry.host_tool_names());
        // After compaction, which may drop earlier notes; boxed to keep the run future small.
        let timing = timing::Phase::new("announce_host_tool_changes");
        Box::pin(self.announce_host_tool_changes(receipt, host_tools)).await?;
        drop(timing);
        let timing = timing::Phase::new("announce_plan_mode_exit");
        Box::pin(self.announce_plan_mode_exit(receipt)).await?;
        drop(timing);
        // Only a Chat with a Folder probes the host it runs in.
        let workspace_host = self
            .inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .is_some_and(|chat| chat.snapshot.folder.is_some())
            .then(|| {
                crate::host_tools::current_workspace_host(
                    self.inner.host_tools.read().unwrap().as_deref(),
                )
            });
        let timing = timing::Phase::new("announce_guidance");
        Box::pin(self.announce_guidance(receipt)).await?;
        drop(timing);
        let family_roster = self.family_agents(receipt.chat_id).await?;
        let user_context = self.user_context(receipt.chat_id).await?;
        let timing = timing::Phase::new("announce_date_change");
        Box::pin(self.announce_date_change(receipt)).await?;
        drop(timing);
        let prefix = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).unwrap();
            registry.refresh(chat);
            request.tools = registry.definitions();
            request.deferred_tools = registry.deferred_definitions();
            (request.messages, request.history_sources) = context::model_history(chat)?;
            tool_exposure::fold_history_as_text(&mut request.messages, &request.tools);
            match chat.tools.chat_directory() {
                Some(chat_directory) => {
                    let replacements = tool_results::enforce_tool_result_budget(
                        &mut request.messages,
                        &mut chat.content_replacements,
                        chat_directory,
                    );
                    if !replacements.is_empty() {
                        self.record_content_replacements(&chat.snapshot, replacements);
                    }
                }
                None => tool_results::cut_tool_results_over_budget(&mut request.messages),
            }
            let plan_mode = self.plan_mode_section(chat, receipt.loop_id);
            request.history_sources.insert(0, None);
            request.messages.insert(
                0,
                self.conversation_system_message(
                    chat,
                    receipt.loop_id,
                    &family_roster,
                    workspace_host.as_ref(),
                    plan_mode.as_deref(),
                ),
            );
            request.history_sources.insert(1, None);
            request.messages.insert(1, user_context);
            cache_prefix::CachePrefix::of(
                &chat.snapshot.model.identity(),
                &request.messages[..2],
                &request.tools,
            )
        };
        let client = self
            .inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .ok_or(AgentError::NotFound)?
            .client
            .clone();
        let available_tools = request
            .tools
            .iter()
            .chain(&request.deferred_tools)
            .map(|tool| tool.name.clone())
            .collect::<HashSet<_>>();
        let mut recovered_text = String::new();
        let mut continuations = 0;
        let response = loop {
            let mut attempt_text = String::new();
            let attempt = self
                .tracked_model_request_capturing(
                    model_request::RequestContext::for_run(receipt, ModelRequestKind::Conversation),
                    client.clone(),
                    request.clone(),
                    cancel.clone(),
                    None,
                    Some(&mut attempt_text),
                )
                .await;
            match attempt {
                Ok(mut response) => {
                    if !recovered_text.is_empty() {
                        // The merged reply was never signed by the provider.
                        response.output.content.retain(|content| {
                            !matches!(
                                content,
                                Content::Thinking { .. } | Content::ProtocolData { .. }
                            )
                        });
                        let first_tool = response
                            .output
                            .content
                            .iter()
                            .position(|content| matches!(content, Content::ToolUse { .. }))
                            .unwrap_or(response.output.content.len());
                        if let Some(Content::Text { text }) = response.output.content[..first_tool]
                            .iter_mut()
                            .find(|content| matches!(content, Content::Text { .. }))
                        {
                            text.insert_str(0, &recovered_text);
                        } else {
                            response.output.content.insert(
                                first_tool,
                                Content::Text {
                                    text: recovered_text,
                                },
                            );
                        }
                    }
                    break response;
                }
                Err(AgentError::Model(ModelError::Cancelled)) => {
                    recovered_text.push_str(&attempt_text);
                    self.commit_interrupted_reply(receipt, recovered_text)
                        .await?;
                    return Err(ModelError::Cancelled.into());
                }
                Err(error) => {
                    let Some(continuation_prompt) =
                        response_continuation_prompt(&error).filter(|_| {
                            continuations < MAX_RESPONSE_CONTINUATIONS
                                && !attempt_text.trim().is_empty()
                        })
                    else {
                        return Err(error);
                    };
                    let mut chats = self.inner.chats.lock().await;
                    let chat = chats
                        .get_mut(&receipt.chat_id)
                        .ok_or(AgentError::NotFound)?;
                    chat.snapshot.pending_thinking.remove(&receipt.run_id);
                    drop(chats);
                    recovered_text.push_str(&attempt_text);
                    request.history_sources.push(None);
                    request.messages.push(Message {
                        role: Role::Assistant,
                        content: vec![Content::Text { text: attempt_text }],
                    });
                    request.history_sources.push(None);
                    request.messages.push(Message {
                        role: Role::User,
                        content: vec![Content::Text {
                            text: continuation_prompt.into(),
                        }],
                    });
                    continuations += 1;
                }
            }
        };
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id)
            && let Some(cache_break) = chat
                .cache_prefix
                .observe(prefix, response.output.usage.cache_read_input_tokens)
        {
            cache_break.log(receipt.chat_id, receipt.run_id);
        }
        let invalid_call = response.output.content.iter().any(|content| {
            if let Content::ToolUse { id, name, input } = content {
                id.is_empty()
                    || (!available_tools.contains(name)
                        && !shared_tasks::is_task_tool(name)
                        && !scheduled_tasks::tools::is_scheduled_task_tool(name)
                        && name != tickloop_harness_tools::WEB_SEARCH_TOOL_NAME)
                    || !input.is_object()
                    || !used_calls.insert(id.clone())
            } else {
                false
            }
        });
        let outcome = if invalid_call {
            RequestOutcome::HostRejected(ResponseRejection::InvalidToolCall)
        } else {
            RequestOutcome::Accepted
        };
        self.finish_model_request(receipt.chat_id, response.sequence, outcome)
            .await?;
        if invalid_call {
            return Err(tickloop_harness_model::ModelError::InvalidResponse(
                "invalid or repeated tool call; no incomplete operation was executed".into(),
            )
            .into());
        }
        Ok(response.output)
    }

    async fn merge_queued_messages(&self, active: &Receipt) -> Result<(), AgentError> {
        let events = {
            let mut chats = self.inner.chats.lock().await;
            let original = chats.get_mut(&active.chat_id).ok_or(AgentError::NotFound)?;
            // Input queued before an interruption is answered by the run that follows it.
            if original.active_run != Some(active.run_id)
                || original.user_cancelled_runs.contains(&active.run_id)
            {
                return Ok(());
            }
            let mut candidate = original.clone();
            let queue_barrier = candidate
                .snapshot
                .user_shells
                .iter()
                .filter(|record| record.state == UserShellState::Queued && record.armed)
                .map(|record| record.queued_sequence)
                .chain(
                    candidate
                        .snapshot
                        .pending_commands
                        .iter()
                        .map(|command| command.queued_sequence),
                )
                .min();
            let merge_indexes = candidate
                .snapshot
                .pending_inputs
                .iter()
                .enumerate()
                .filter(|(_, pending)| {
                    // Trigger input waits for the turn to end.
                    pending.armed
                        && pending.trigger_source.is_none()
                        && pending.receipt.loop_id == active.loop_id
                        && (pending.agent_message.is_some()
                            || queue_barrier
                                .is_none_or(|sequence| pending.queued_sequence < sequence))
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if merge_indexes.is_empty() {
                return Ok(());
            }
            let merged = merge_indexes
                .into_iter()
                .rev()
                .map(|index| candidate.snapshot.pending_inputs.remove(index))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>();
            let mut events = Vec::with_capacity(merged.len());
            for pending in merged {
                let receipt = pending.receipt.clone();
                if pending.agent_message.is_none() {
                    self.merge_host_context(active, &receipt);
                }
                for skill in &mut candidate.snapshot.invoked_skills {
                    if skill.source == tickloop_harness_protocol::SkillSource::BuiltIn
                        && skill.run_id == Some(receipt.run_id)
                    {
                        skill.run_id = Some(active.run_id);
                    }
                }
                let index = candidate.snapshot.messages.len();
                self.append_input_message(&mut candidate, pending);
                if let Some(response) = candidate
                    .bookkeeping
                    .lifecycle
                    .active_response(active.run_id)
                    && index < candidate.snapshot.messages.len()
                {
                    candidate
                        .bookkeeping
                        .message_responses
                        .insert(index, response);
                }
                self.settle_input(
                    &mut candidate,
                    receipt.run_id,
                    SettledOutcome::Merged {
                        into: active.run_id.into(),
                    },
                );
                candidate.snapshot.last_sequence += 1;
                let event = Event {
                    sequence: candidate.snapshot.last_sequence,
                    chat_id: receipt.chat_id,
                    loop_id: receipt.loop_id,
                    run_id: receipt.run_id,
                    kind: EventKind::InputMerged {
                        into_run_id: active.run_id,
                    },
                };
                candidate.snapshot.events.push(event.clone());
                events.push(event);
            }
            while candidate.snapshot.events.len() > 1024 {
                candidate.snapshot.events.remove(0);
            }
            self.save(&candidate.snapshot);
            *original = candidate;
            events
        };
        for event in events {
            let _ = self.inner.events.send(event);
        }
        self.flush_subagent_notifications(active.chat_id).await;
        Ok(())
    }

    async fn admit_host_operation(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        permission_input: &serde_json::Value,
        admission: HostAdmission<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        if let Some(assessment) = admission
            .assessment
            .filter(|assessment| assessment.outcome == ApprovalOutcome::Allow)
        {
            let (_, revision) = self
                .auto_settings_snapshot()
                .await
                .map_err(|error| error.to_string())?;
            if assessment.auto_settings_revision.as_deref() != Some(revision.as_str()) {
                return Err(
                    "Auto approval settings changed before host admission; retry the operation"
                        .into(),
                );
            }
        }
        let mut chats = self.inner.chats.lock().await;
        if let Some(expected) = admission.expected_task_binding
            && shared_tasks::binding_for_tool(&chats, receipt, admission.tool_name)? != expected
        {
            return Err("task execution binding changed before host admission".into());
        }
        let session_context = self.inner.session_permissions.lock().await;
        delegation::check_call(
            &chats,
            &session_context,
            receipt,
            &operation.name,
            &operation.input,
            None,
            None,
        )?;
        let current_session_operation = SessionPermissionOperation::from_normalized(
            operation.name.clone(),
            permission_input.clone(),
            session_permissions::capture_workspace_scope(&configured_workspace_for_chat(
                &chats[&receipt.chat_id],
            ))
            .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let (authority, _) = delegation::host_authority(
            &chats,
            &session_context,
            receipt,
            operation,
            permission_input,
            delegation::HostCallPermission {
                permission: admission.host_permission,
                mcp_server: admission.mcp_server,
                local_read: admission.local_read,
            },
            admission.session_evaluation.decision,
        )?;
        let chat = chats.get_mut(&receipt.chat_id).unwrap();
        let permission_is_current = session_context.revalidate(
            &admission.session_evaluation.token,
            PermissionOwner::new(receipt.chat_id.into(), receipt.loop_id.into())
                .map_err(|error| error.to_string())?,
            &current_session_operation,
        );
        if cancel.is_cancelled()
            || chat.active_run != Some(receipt.run_id)
            || !permission_is_current
            || !authority.still_admits(admission.policy_revision, admission.ancestor_revisions)
        {
            return Err("host operation permission changed before admission".into());
        }
        drop(session_context);
        nested_calls::check_dispatch()?;
        chat.snapshot
            .operations
            .iter_mut()
            .find(|record| record.id == operation.id)
            .expect("proposed host operation remains present")
            .state = OperationState::Admitted;
        if let Some(operation) = chat
            .snapshot
            .operations
            .iter()
            .find(|record| record.id == operation.id)
        {
            self.record_nested_operation(receipt.chat_id, operation);
        }
        let approval = denials::allowed_approval_record(
            receipt.chat_id,
            operation,
            permission_input,
            &chat.snapshot.policy,
            denials::AllowedDecision {
                mcp_server: admission.mcp_server,
                path: admission.approval_path,
                direct_basis: admission.direct_basis,
                approval_source: admission.approval_source,
                assessment: admission.assessment,
            },
        );
        self.record_approval(chat, receipt, approval);
        self.save(&chat.snapshot);
        self.emit(
            chat,
            receipt,
            EventKind::ToolStarted {
                call_id: operation.call_id.clone(),
                name: operation.name.clone(),
                input: operation.input.clone(),
            },
        );
        Ok(())
    }

    async fn execute_run(
        &self,
        receipt: Receipt,
        start: RunStart,
        cancel: CancellationToken,
        is_awaiting_model: &mut bool,
    ) -> Result<Usage, String> {
        self.complete_file_history_snapshots(receipt.chat_id).await;
        let mut used_calls = self.calls_on_chain(&receipt).await;
        let mut request = match start {
            RunStart::Request(request) => request,
            RunStart::Resume { at, result } => {
                *is_awaiting_model = false;
                match self.reenter_batch(&receipt, at, result, &cancel).await? {
                    BatchExit::Continue(request) => request,
                    BatchExit::Ended(usage) => return Ok(usage),
                }
            }
        };
        loop {
            Box::pin(self.stop_at_safe_point(&receipt, None)).await?;
            self.checkpoint_feedback_inputs(receipt.chat_id).await?;
            self.feedback_account_context(&receipt).await?;
            *is_awaiting_model = true;
            let prepared = Box::pin(timing::request(&receipt, async {
                let mut registry = self
                    .prepare_tool_registry(&receipt, &cancel)
                    .await
                    .map_err(AgentError::Invalid)?;
                request.tools = registry.definitions();
                request.deferred_tools = registry.deferred_definitions();
                let output = self
                    .model_request(
                        &receipt,
                        request,
                        &mut registry,
                        cancel.clone(),
                        &mut used_calls,
                    )
                    .await?;
                Ok::<_, AgentError>((registry, output))
            }))
            .await;
            let (registry, output) = match prepared {
                Ok(prepared) => prepared,
                // A handover stopped the request before it reached the vendor, or in a retry
                // backoff: the Run stops before its model request.
                Err(AgentError::Model(ModelError::Cancelled))
                    if self.stopped_for_handover(&cancel) =>
                {
                    return Err(Box::pin(self.leave_before_model_request(&receipt)).await);
                }
                Err(error) => return Err(error.to_string()),
            };
            *is_awaiting_model = false;
            let assistant_message_id = tickloop_ids::generate();
            let calls = tool_batch::batch_calls(&output.content);
            {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&receipt.chat_id).unwrap();
                // A Run ended at a handover's ceiling leaves without its reply.
                if Agent::release_of(chat, receipt.run_id).is_cancelled() {
                    return Err(suspension::RUN_RELEASED.into());
                }
                chat.snapshot.pending_thinking.remove(&receipt.run_id);
                chat.snapshot
                    .message_runs
                    .insert(chat.snapshot.messages.len(), receipt.run_id);
                chat.snapshot
                    .message_ids
                    .insert(chat.snapshot.messages.len(), assistant_message_id);
                chat.snapshot
                    .message_usage
                    .insert(chat.snapshot.messages.len(), output.usage);
                let index = chat.snapshot.messages.len();
                chat.snapshot
                    .message_models
                    .insert(index, chat.snapshot.model.identity());
                if let Some(source) = output
                    .private_source
                    .or_else(|| chat.snapshot.model.private_history_source())
                {
                    chat.snapshot.message_sources.insert(index, source);
                }
                chat.snapshot.messages.push(Message {
                    role: Role::Assistant,
                    content: output.content,
                });
                chat.snapshot.last_model_run = Some(receipt.run_id);
                self.emit(
                    chat,
                    &receipt,
                    EventKind::AssistantMessageCommitted {
                        message_id: assistant_message_id,
                        tool_calls: calls.clone(),
                    },
                );
            }
            if calls.is_empty() {
                if let Some(continuation) = self.after_complete_response(&receipt, &cancel).await? {
                    request = continuation;
                    continue;
                }
                let chats = self.inner.chats.lock().await;
                return chats[&receipt.chat_id]
                    .snapshot
                    .request_usage
                    .run_conversation_usage(receipt.run_id)
                    .map_err(|error| error.to_string());
            }
            match self
                .run_batch_from(
                    &receipt,
                    &registry,
                    ToolBatch {
                        message_id: assistant_message_id,
                        calls,
                        is_reentered: false,
                    },
                    0,
                    None,
                    &cancel,
                )
                .await?
            {
                BatchExit::Continue(next) => request = next,
                BatchExit::Ended(usage) => return Ok(usage),
            }
        }
    }

    async fn finish_run(
        &self,
        receipt: Receipt,
        cancel: CancellationToken,
        mut result: Result<Usage, String>,
        is_awaiting_model: bool,
        internal_failure: bool,
    ) -> Option<(Receipt, ModelRequest, CancellationToken)> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id)?;
        debug_assert_eq!(
            receipt.loop_id, chat.snapshot.chat_loop.id,
            "every run belongs to its Chat's one Loop"
        );
        chat.family_finishing.insert(receipt.run_id);
        chat.feedback_inputs.remove(&receipt.run_id);
        run_supervision::clear_run_state(chat, receipt.run_id);
        if internal_failure {
            chat.snapshot.request_usage.fail_in_flight_requests(
                receipt.run_id,
                tickloop_harness_protocol::RequestFailure::Internal,
            );
            self.clear_run_interactions(chat, receipt.run_id).await;
        }
        if let Err(error) =
            recovery::close_dangling_tool_calls(&chat.ids, &mut chat.snapshot, Some(receipt.run_id))
        {
            result = Err(error.to_string());
        }
        let is_user_cancelled = chat.user_cancelled_runs.remove(&receipt.run_id);
        if cancel.is_cancelled() && is_user_cancelled && !self.inner.closed.load(Ordering::SeqCst) {
            chat.snapshot
                .message_runs
                .insert(chat.snapshot.messages.len(), receipt.run_id);
            chat.snapshot
                .messages
                .push(local_messages::interruption(!is_awaiting_model));
        }
        let awaits_plan_review = chat.snapshot.policy.mode == ExecutionMode::Plan
            && chat.snapshot.plans.iter().any(|plan| plan.approval_pending);
        let chat_loop = &mut chat.snapshot.chat_loop;
        let event = match result {
            _ if cancel.is_cancelled() && !internal_failure => {
                chat_loop.set_state(LoopState::Interrupted);
                EventKind::Interrupted
            }
            Ok(usage) => {
                chat_loop.set_state(if awaits_plan_review {
                    LoopState::WaitingPlan
                } else {
                    LoopState::Idle
                });
                EventKind::Completed { usage }
            }
            Err(error) => {
                chat_loop.set_state(LoopState::Failed);
                EventKind::Failed(error)
            }
        };
        let has_self_paced_task = self.end_self_paced_turn(chat, receipt.run_id);
        let is_closed = self.inner.closed.load(Ordering::SeqCst);
        let (ending, usage) = match &event {
            EventKind::Completed { usage } => (ResponseEnding::Completed, usage.clone()),
            EventKind::Failed(error) if !is_closed => (
                ResponseEnding::Failed {
                    error: error.clone(),
                },
                Usage::default(),
            ),
            // A Run that fails or stops while the host shuts down lost its host.
            _ => (
                ResponseEnding::Interrupted {
                    by: if is_closed {
                        InterruptedBy::Host
                    } else {
                        InterruptedBy::User
                    },
                },
                Usage::default(),
            ),
        };
        let goal_turn = match &event {
            EventKind::Completed { .. } => goal::GoalTurnEnd::Completed,
            EventKind::Failed(error) if !is_closed => goal::GoalTurnEnd::Failed(error.clone()),
            _ => goal::GoalTurnEnd::Interrupted,
        };
        let goal_turn = self.goal_turn_events(chat, &receipt, goal_turn);
        // A durable host goes on with an answer whose host it lost: its input has not settled,
        // so its child run has no completion yet.
        let is_settled = !(self.inner.waits == WaitHosting::Durable
            && ending
                == ResponseEnding::Interrupted {
                    by: InterruptedBy::Host,
                });
        self.end_response(chat, receipt.run_id, ending, usage);
        // An interrupted or failed turn does not hold back a Tick delivery that came due.
        let is_deliverable = matches!(
            chat.snapshot.chat_loop.state,
            LoopState::Idle | LoopState::Interrupted | LoopState::Failed
        );
        self.save(&chat.snapshot);
        self.emit(chat, &receipt, event);
        self.inner
            .host_contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .runs
            .remove(&receipt.run_id);
        if is_deliverable {
            self.flush_due_tick_delivery(receipt.chat_id, receipt.loop_id);
        }
        if let Err(error) = self.apply_pending_model(chat) {
            self.emit(chat, &receipt, EventKind::Failed(error.to_string()));
            return None;
        }
        let finished_snapshot = chat.snapshot.clone();
        drop(chats);
        if let Some(events) = goal_turn {
            self.end_goal_turn(receipt.chat_id, events).await;
        }
        if has_self_paced_task {
            self.fall_back_after_turn(receipt.chat_id, receipt.run_id)
                .await;
        }
        if is_settled {
            self.settle_task_claims(&receipt).await;
        }
        self.flush_subagent_notifications(receipt.chat_id).await;
        self.record_directed_replies(&receipt, &finished_snapshot)
            .await;
        if finished_snapshot.delegation.is_some() && is_settled {
            self.record_family_completion(&receipt, &finished_snapshot)
                .await;
        }
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id) {
            chat.family_finishing.remove(&receipt.run_id);
        }
        if internal_failure {
            return None;
        }
        match self.next_run(receipt.chat_id).await {
            Ok(None) => {
                self.continue_goal(receipt.chat_id, receipt.run_id).await;
                None
            }
            Ok(next) => next,
            Err(error) => {
                let mut chats = self.inner.chats.lock().await;
                if let Some(chat) = chats.get_mut(&receipt.chat_id) {
                    self.emit(chat, &receipt, EventKind::Failed(error.to_string()));
                }
                None
            }
        }
    }

    fn continuation_request(&self, chat: &Chat) -> Result<ModelRequest, String> {
        let (messages, history_sources) =
            context::model_history(chat).map_err(|error| error.to_string())?;
        Ok(ModelRequest {
            deferred_tools: Vec::new(),
            history_sources,
            messages,
            tools: self.tool_definitions(chat),
            structured_output: None,
            temperature: None,
            reasoning_effort: chat.snapshot.effective_reasoning_effort(),
        })
    }

    fn park_run(
        &self,
        chat: &mut Chat,
        receipt: &Receipt,
        cancel: CancellationToken,
    ) -> Result<(), AgentError> {
        if chat.active_run != Some(receipt.run_id) || cancel.is_cancelled() {
            return Err(AgentError::Invalid(
                "run no longer owns the execution slot".into(),
            ));
        }
        chat.parked.insert(
            receipt.run_id,
            ParkedRun {
                receipt: receipt.clone(),
                cancel,
                resume: Arc::new(Notify::new()),
                sequence: chat.snapshot.last_sequence,
            },
        );
        chat.active = None;
        chat.active_run = None;
        self.save(&chat.snapshot);
        Ok(())
    }

    async fn resume_parked_run(
        &self,
        receipt: &Receipt,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("waiting run was cancelled".into());
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or_else(|| "chat is unavailable".to_string())?;
        let resume = chat
            .parked
            .get(&receipt.run_id)
            .ok_or_else(|| "waiting run expired".to_string())?
            .resume
            .clone();
        let release = Self::release_of(chat, receipt.run_id);
        if !chat.ready_runs.contains(&receipt.run_id) {
            chat.ready_runs.push_back(receipt.run_id);
        }
        drop(chats);
        let next = self
            .next_run(receipt.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(next) = next {
            self.run_task(next);
        }
        tokio::select! {
            biased;
            _ = release.cancelled() => Err(suspension::RUN_RELEASED.into()),
            _ = cancel.cancelled() => Err("waiting run was cancelled".into()),
            _ = resume.notified() => Ok(()),
        }
    }

    fn admit_operation(&self, chat: &mut Chat, operation_id: Uuid) -> Result<(), AgentError> {
        let operation = chat
            .snapshot
            .operations
            .iter_mut()
            .find(|operation| operation.id == operation_id)
            .ok_or(AgentError::NotFound)?;
        if operation.state != OperationState::Proposed {
            return Err(AgentError::Invalid(
                "operation has already been admitted".into(),
            ));
        }
        nested_calls::check_dispatch().map_err(AgentError::Invalid)?;
        operation.state = OperationState::Admitted;
        self.record_nested_operation(chat.snapshot.id, operation);
        Ok(())
    }

    /// Writes an accepted input to the conversation. User prompts capture the current execution
    /// mode and create a pending file snapshot as a rewind point. The run completes snapshots
    /// outside the lock through `complete_file_history_snapshots`.
    fn append_input_message(&self, chat: &mut Chat, mut pending: PendingInput) {
        if let Some(TriggerSource::GoalContinuation {
            goal_id,
            objective_revision,
            ..
        }) = &pending.trigger_source
            && let Some(input) = chat
                .snapshot
                .goal
                .as_ref()
                .filter(|goal| goal.id == *goal_id)
                .and_then(|goal| goal::current_continuation(goal, *objective_revision))
        {
            pending.input = input;
        }
        if let Some(identity) = chat.feedback_inputs.remove(&pending.receipt.run_id) {
            let active = chat.active_run.unwrap_or(pending.receipt.run_id);
            chat.feedback_inputs.entry(active).or_insert(identity);
        }

        if pending.task_continuation {
            chat.bookkeeping
                .run_loops
                .insert(pending.receipt.run_id, pending.receipt.loop_id);
            return;
        }
        let message_index = chat.snapshot.messages.len();
        chat.snapshot
            .message_runs
            .insert(message_index, pending.receipt.run_id);
        chat.bookkeeping
            .run_loops
            .insert(pending.receipt.run_id, pending.receipt.loop_id);
        chat.snapshot.messages.push(Message {
            role: Role::User,
            content: vec![Content::Text {
                text: pending.input,
            }],
        });
        if !pending.contexts.is_empty() {
            chat.snapshot
                .message_contexts
                .insert(message_index, pending.contexts);
        }
        for (path, record) in pending.attached_file_reads {
            chat.tools.file_reads().record(path, record);
        }
        if let Some(reference) = pending.denial_reference {
            chat.bookkeeping
                .denial_references
                .insert(message_index, reference);
        }
        if pending.trigger_source.is_some() {
            return;
        }
        if let Some(source) = pending.agent_message {
            chat.snapshot
                .message_ids
                .insert(message_index, source.message_id);
            chat.snapshot.agent_message_origins.insert(
                source.message_id,
                serde_json::to_value(source).expect("Agent message source is serializable"),
            );
            return;
        }
        let message_id = pending.directed_human.as_ref().map_or_else(
            || {
                chat.bookkeeping
                    .lifecycle
                    .open_input(pending.receipt.run_id)
                    .unwrap_or_else(tickloop_harness_protocol::InputId::new)
            },
            |source| source.message_id.into(),
        );
        if let Some(source) = pending.directed_human {
            chat.snapshot
                .human_family_origins
                .insert(message_id.into(), source);
        }
        chat.snapshot
            .message_ids
            .insert(message_index, message_id.into());
        chat.snapshot
            .prompt_execution_modes
            .insert(message_index, chat.snapshot.policy.mode);
        if let Some(file_history) = &mut chat.file_history {
            file_history.begin_snapshot(message_id.into());
        }
    }

    async fn next_run(
        &self,
        chat_id: ChatId,
    ) -> Result<Option<(Receipt, ModelRequest, CancellationToken)>, AgentError> {
        loop {
            let admission = {
                let mut chats = self.inner.chats.lock().await;
                let background_work = goal::background_work_runs(&chats, chat_id);
                let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                if chat.family_stopping {
                    return Ok(None);
                }
                self.select_next_run(chat, None, background_work)?
            };
            match admission {
                RunAdmission::Idle => return Ok(None),
                RunAdmission::Ready(receipt, request, cancel) => {
                    return Ok(Some((receipt, *request, cancel)));
                }
                RunAdmission::ValidateTrigger(pending) => {
                    match self.take_trigger_input(chat_id, *pending).await? {
                        TriggerTake::LookAgain => {}
                        // A run it started runs on a task of its own.
                        TriggerTake::Started | TriggerTake::Waits => return Ok(None),
                    }
                }
            }
        }
    }

    /// Takes the queued Trigger input `pending` and starts its run on a task of the Agent's own,
    /// which a caller that stops waiting does not stop: no input is left started and not
    /// running.
    async fn take_trigger_input(
        &self,
        chat_id: ChatId,
        pending: PendingInput,
    ) -> Result<TriggerTake, AgentError> {
        self.inner
            .tasks
            .spawn(self.clone().take_and_start_trigger_input(chat_id, pending))
            .await
            .map_err(|error| AgentError::Invalid(format!("taking Trigger input failed: {error}")))?
    }

    /// The whole take, within the Chat's Trigger boundary: under the Chat's lease, while no
    /// other process paused receiving, once its source proved it again, the input starts, its
    /// start is recorded durably before the boundary is left, and its run is started. Its future
    /// is named, not inferred: the run asks for the next input, which may be taken this way.
    fn take_and_start_trigger_input(
        self,
        chat_id: ChatId,
        pending: PendingInput,
    ) -> futures_util::future::BoxFuture<'static, Result<TriggerTake, AgentError>> {
        Box::pin(async move { self.take_and_start(chat_id, &pending).await })
    }

    async fn take_and_start(
        &self,
        chat_id: ChatId,
        pending: &PendingInput,
    ) -> Result<TriggerTake, AgentError> {
        let source = pending
            .trigger_source
            .as_ref()
            .expect("only Trigger input is validated");
        let boundary = self.enter_trigger_boundary(chat_id).await;
        let holding = match self.acquire_trigger_permit(chat_id, &boundary).await {
            loop_lease::TriggerPermit::Fire(holding) => holding,
            loop_lease::TriggerPermit::Wait {
                recoverable: true, ..
            }
            | loop_lease::TriggerPermit::Failed(_) => {
                self.retry_waiting_trigger_input(chat_id);
                return Ok(TriggerTake::Waits);
            }
            loop_lease::TriggerPermit::Wait {
                recoverable: false, ..
            }
            | loop_lease::TriggerPermit::Withdrawn => return Ok(TriggerTake::Waits),
        };
        // Another take of the same input may have started it while this one waited for the
        // boundary: only a take that finds it still waiting here goes on, so no take starts or
        // takes back what another one started.
        if let Some(take) = taken_elsewhere(
            self.inner
                .chats
                .lock()
                .await
                .get(&chat_id)
                .ok_or(AgentError::NotFound)?,
            pending,
        ) {
            return Ok(take);
        }
        // Boxed: proving a source must not enlarge every run's shared async stack.
        let validation = Box::pin(self.revalidate_trigger_source(chat_id, source)).await;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if let Some(take) = taken_elsewhere(chat, pending) {
            return Ok(take);
        }
        if validation.is_err() {
            self.withdraw_trigger_input(chat, &pending.receipt, DiscardReason::SourceEnded);
            self.withdraw_ended_inputs(chat);
            self.save(&chat.snapshot);
            return Ok(TriggerTake::LookAgain);
        }
        // Within the boundary nothing releases the lease, but the input waits rather than
        // start under a holding that is gone.
        if !self.inner.chat_leases.stands(chat_id, holding) {
            drop(chats);
            self.retry_waiting_trigger_input(chat_id);
            return Ok(TriggerTake::Waits);
        }
        #[cfg(any(test, feature = "test-support"))]
        let chat = {
            drop(chats);
            self.pause_point(chat_id, wakeup::trigger_pauses::PausePoint::TriggerChecked)
                .await;
            chats = self.inner.chats.lock().await;
            chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?
        };
        // The start alone is recorded first, before the input enters the conversation or its
        // run begins, so another process taking the lease over finds it started; until it is
        // recorded the start can be taken back by a record after it.
        chat.bookkeeping
            .lifecycle
            .start_ahead(pending.receipt.run_id);
        self.save(&chat.snapshot);
        drop(chats);
        let Some(boundary) = self.record_trigger_start(chat_id, pending, boundary).await else {
            return Ok(TriggerTake::Waits);
        };
        #[cfg(any(test, feature = "test-support"))]
        self.pause_point(
            chat_id,
            wakeup::trigger_pauses::PausePoint::TriggerStartRecorded,
        )
        .await;
        let mut chats = self.inner.chats.lock().await;
        let background_work = goal::background_work_runs(&chats, chat_id);
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let admission = match self.select_next_run(chat, Some(pending), background_work) {
            Ok(admission) => admission,
            Err(error) => {
                // A selection that fails changes nothing of the Chat: the input never started,
                // and waits again.
                chat.bookkeeping.lifecycle.take_start_back(pending);
                self.save(&chat.snapshot);
                return Err(error);
            }
        };
        if !matches!(&admission, RunAdmission::Ready(receipt, ..) if receipt.run_id == pending.receipt.run_id)
        {
            // Something else came first: the start is taken back, and the input waits again.
            chat.bookkeeping.lifecycle.take_start_back(pending);
            self.save(&chat.snapshot);
        }
        match admission {
            RunAdmission::Idle => Ok(TriggerTake::Waits),
            RunAdmission::ValidateTrigger(_) => Ok(TriggerTake::LookAgain),
            RunAdmission::Ready(receipt, request, cancel) => {
                drop(chats);
                #[cfg(any(test, feature = "test-support"))]
                self.pause_point(chat_id, wakeup::trigger_pauses::PausePoint::TriggerStarted)
                    .await;
                // Its prompt is recorded before its run begins: a load that finds the start
                // without the prompt after it knows the run never began.
                if !self.record_trigger_prompt(chat_id, &cancel).await {
                    return Ok(TriggerTake::Waits);
                }
                drop(boundary);
                self.run_task((receipt, *request, cancel));
                Ok(TriggerTake::Started)
            }
        }
    }

    /// Records durably the start of the Trigger input `pending`, within the Chat's boundary,
    /// trying again after a delay that doubles while the Transcript cannot be written, and gives
    /// the boundary back once it is recorded. Otherwise the start is taken back by a record
    /// after it, which the same Transcript writes in order, so whatever lands of the two, the
    /// latest has the input accepted and not started: as receiving pauses here, as the Agent
    /// shuts down, and past the budget, when receiving pauses here too, the lease kept, until
    /// the Chat is continued.
    async fn record_trigger_start(
        &self,
        chat_id: ChatId,
        pending: &PendingInput,
        boundary: loop_lease::TriggerBoundary,
    ) -> Option<loop_lease::TriggerBoundary> {
        // Nothing of the Chat is stored yet, so no other process can take it over.
        if !self.transcript_materialized(chat_id) {
            return Some(boundary);
        }
        let began = tokio::time::Instant::now();
        let mut delay = TRIGGER_START_RETRY_FIRST;
        loop {
            let recorded = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&chat_id)?;
                if !chat
                    .bookkeeping
                    .lifecycle
                    .is_started(pending.receipt.run_id)
                {
                    // Settled meanwhile, as withdrawn: nothing to take back.
                    return None;
                }
                if !chat.snapshot.chat_loop.receiving.is_receiving() {
                    chat.bookkeeping.lifecycle.take_start_back(pending);
                    self.save(&chat.snapshot);
                    return None;
                }
                self.checkpoint_transcript(&chat.snapshot, &chat.bookkeeping)
                    .await
            };
            let Err(error) = recorded else {
                return Some(boundary);
            };
            if began.elapsed() >= trigger_start_budget() {
                self.pause_for_unwritable_transcript(chat_id, pending, &error)
                    .await;
                return None;
            }
            self.inner.chat_leases.set_notice(
                chat_id,
                tickloop_harness_protocol::LoopTriggerNoticeKind::Warning,
                format!(
                    "a Trigger input waits to start until its start is recorded in the \
                     Transcript: {error}"
                ),
            );
            #[cfg(any(test, feature = "test-support"))]
            self.pause_point(
                chat_id,
                wakeup::trigger_pauses::PausePoint::TriggerStartRetrying,
            )
            .await;
            tokio::select! {
                _ = self.inner.shutdown.cancelled() => {
                    #[cfg(any(test, feature = "test-support"))]
                    self.pause_point(
                        chat_id,
                        wakeup::trigger_pauses::PausePoint::TriggerStartAbandoned,
                    )
                    .await;
                    if let Some(chat) = self.inner.chats.lock().await.get_mut(&chat_id) {
                        chat.bookkeeping.lifecycle.take_start_back(pending);
                        self.save(&chat.snapshot);
                    }
                    return None;
                }
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(TRIGGER_START_RETRY_LONGEST);
        }
    }

    /// Records the started input's prompt, trying again while the Transcript cannot be written;
    /// whether its run may begin: once recorded, or once interrupted, when the run only ends.
    /// A shutdown leaves the run unbegun: a prompt that never lands has the next load start the
    /// input again, and one that lands late leaves it as one whose run may have begun.
    async fn record_trigger_prompt(&self, chat_id: ChatId, cancel: &CancellationToken) -> bool {
        let mut delay = TRIGGER_START_RETRY_FIRST;
        loop {
            let recorded = match self.inner.chats.lock().await.get(&chat_id) {
                Some(chat) => {
                    self.checkpoint_transcript(&chat.snapshot, &chat.bookkeeping)
                        .await
                }
                None => return false,
            };
            let Err(error) = recorded else {
                return true;
            };
            self.inner.chat_leases.set_notice(
                chat_id,
                tickloop_harness_protocol::LoopTriggerNoticeKind::Warning,
                format!(
                    "a Trigger input waits to begin until its prompt is recorded in the \
                     Transcript: {error}"
                ),
            );
            tokio::select! {
                _ = cancel.cancelled() => return !self.inner.closed.load(Ordering::SeqCst),
                _ = self.inner.shutdown.cancelled() => return false,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(TRIGGER_START_RETRY_LONGEST);
        }
    }

    /// The Transcript took no start for longer than the budget: the start is taken back,
    /// receiving pauses here, and the user is told; the lease stays, so no other process runs
    /// what may yet land as started here.
    async fn pause_for_unwritable_transcript(
        &self,
        chat_id: ChatId,
        pending: &PendingInput,
        error: &str,
    ) {
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&chat_id) {
            chat.bookkeeping.lifecycle.take_start_back(pending);
            chat.snapshot.chat_loop.receiving = tickloop_harness_protocol::Receiving::Paused {
                reason: tickloop_harness_protocol::PauseReason::TranscriptUnavailable,
            };
            self.save(&chat.snapshot);
        }
        self.inner.chat_leases.set_notice(
            chat_id,
            tickloop_harness_protocol::LoopTriggerNoticeKind::Warning,
            format!(
                "this Chat's Transcript cannot be written ({error}); receiving paused, and the \
                 Trigger input waits. Continue the Chat to receive again"
            ),
        );
    }

    /// `background_work` holds back a goal continuation while the Chat's background children
    /// or Shells still run.
    fn select_next_run(
        &self,
        chat: &mut Chat,
        validated_trigger: Option<&PendingInput>,
        background_work: bool,
    ) -> Result<RunAdmission, AgentError> {
        self.flush_deferred_messages(chat, MessageBoundary::Idle)?;
        if self.inner.closed.load(Ordering::SeqCst)
            || self.inner.handover.is_cancelled()
            || self.inner.upgrade_paused.load(Ordering::SeqCst)
            || chat.active.is_some()
            || chat.has_foreground_user_shell()
        {
            self.save(&chat.snapshot);
            return Ok(RunAdmission::Idle);
        }
        let mut candidate = chat.clone();
        while let Some(run_id) = candidate.ready_runs.pop_front() {
            let Some(parked) = candidate.parked.get(&run_id).cloned() else {
                continue;
            };
            if parked.cancel.is_cancelled() {
                continue;
            }
            candidate.snapshot.chat_loop.set_state(LoopState::Running);
            candidate.parked.remove(&run_id);
            candidate.active = Some(parked.cancel.clone());
            candidate.active_run = Some(run_id);
            self.save(&candidate.snapshot);
            *chat = candidate;
            parked.resume.notify_one();
            return Ok(RunAdmission::Idle);
        }
        let preceding_model_runs = candidate
            .snapshot
            .user_shells
            .iter()
            .find(|record| record.state == UserShellState::Queued)
            .map(|record| record.preceding_model_runs.clone());
        let command_barrier = candidate
            .snapshot
            .pending_commands
            .iter()
            .map(|command| command.queued_sequence)
            .min();
        self.apply_pending_model(&mut candidate)?;
        self.withdraw_ended_inputs(&mut candidate);
        if let Some(index) = candidate
            .snapshot
            .pending_inputs
            .iter()
            .position(|pending| {
                pending.armed
                    && command_barrier.is_none_or(|sequence| pending.queued_sequence < sequence)
                    && preceding_model_runs
                        .as_ref()
                        .is_none_or(|runs| runs.contains(&pending.receipt.run_id))
                    && !matches!(
                        candidate.snapshot.chat_loop.state,
                        LoopState::WaitingPermission | LoopState::WaitingQuestion
                    )
                    // Trigger input waits while its Loop pauses receiving or its source holds it.
                    && pending.trigger_source.as_ref().is_none_or(|source| {
                        self.takes_trigger_input(&candidate, source) == wakeup::Eligibility::Eligible
                            && !(background_work
                                && matches!(source, TriggerSource::GoalContinuation { .. }))
                    })
            })
        {
            let pending = candidate.snapshot.pending_inputs[index].clone();
            if let Some(source) = &pending.trigger_source {
                let validated = validated_trigger.is_some_and(|validated| {
                    validated.receipt.run_id == pending.receipt.run_id
                        && validated.input == pending.input
                        && validated.trigger_source.as_ref() == Some(source)
                });
                if !validated {
                    self.save(&candidate.snapshot);
                    *chat = candidate;
                    return Ok(RunAdmission::ValidateTrigger(Box::new(pending)));
                }
            }
            recovery::before_resume(
                &mut candidate.snapshot,
                pending.receipt.loop_id,
                self.inner.config_root.as_deref(),
            )?;
            candidate.snapshot.pending_inputs.remove(index);
            let mut merged_inputs = Vec::new();
            if pending.agent_message.is_none()
                && pending.trigger_source.is_none()
                && !pending.task_continuation
            {
                while !pending.is_skill_invocation
                    && candidate
                        .snapshot
                        .pending_inputs
                        .get(index)
                        .is_some_and(|queued| {
                            queued.armed
                                && queued.agent_message.is_none()
                                && queued.trigger_source.is_none()
                                && !queued.task_continuation
                                && !queued.is_skill_invocation
                                && queued.receipt.loop_id == pending.receipt.loop_id
                                && command_barrier
                                    .is_none_or(|sequence| queued.queued_sequence < sequence)
                                && preceding_model_runs
                                    .as_ref()
                                    .is_none_or(|runs| runs.contains(&queued.receipt.run_id))
                        })
                {
                    merged_inputs.push(candidate.snapshot.pending_inputs.remove(index));
                }
                let mut retained = Vec::new();
                for queued in std::mem::take(&mut candidate.snapshot.pending_inputs) {
                    if queued.agent_message.is_some()
                        && queued.armed
                        && queued.receipt.loop_id == pending.receipt.loop_id
                    {
                        merged_inputs.push(queued);
                    } else {
                        retained.push(queued);
                    }
                }
                candidate.snapshot.pending_inputs = retained;
            }
            let chat_loop = &mut candidate.snapshot.chat_loop;
            chat_loop.set_state(LoopState::Running);
            chat_loop.run_id = Some(pending.receipt.run_id);
            chat_loop.run_started_at =
                Some(tickloop_harness_base::private_fs::unix_time_milliseconds());
            let cancel = CancellationToken::new();
            candidate.active = Some(cancel.clone());
            candidate.active_run = Some(pending.receipt.run_id);
            let receipt = pending.receipt.clone();
            self.mark_run_inputs_handled(&mut candidate, &pending);
            candidate.bookkeeping.lifecycle.start_input(receipt.run_id);
            self.append_input_message(&mut candidate, pending);
            let mut merged_receipts = Vec::with_capacity(merged_inputs.len());
            for queued in merged_inputs {
                let consumed = queued.receipt.clone();
                if queued.agent_message.is_none() {
                    self.merge_host_context(&receipt, &consumed);
                }
                self.append_input_message(&mut candidate, queued);
                self.settle_input(
                    &mut candidate,
                    consumed.run_id,
                    SettledOutcome::Merged {
                        into: receipt.run_id.into(),
                    },
                );
                merged_receipts.push(consumed);
            }
            self.save(&candidate.snapshot);
            self.start_response(&mut candidate, &receipt);
            self.emit(&mut candidate, &receipt, EventKind::Started);
            for consumed in merged_receipts {
                self.emit(
                    &mut candidate,
                    &consumed,
                    EventKind::InputMerged {
                        into_run_id: receipt.run_id,
                    },
                );
            }
            let (messages, history_sources) = context::model_history(&candidate)?;
            let request = ModelRequest {
                deferred_tools: Vec::new(),
                history_sources,
                messages,
                tools: self.tool_definitions(&candidate),
                structured_output: None,
                temperature: None,
                reasoning_effort: candidate.snapshot.effective_reasoning_effort(),
            };
            *chat = candidate;
            return Ok(RunAdmission::Ready(receipt, Box::new(request), cancel));
        }
        if preceding_model_runs.is_some()
            && command_barrier.is_none_or(|command_sequence| {
                candidate
                    .snapshot
                    .user_shells
                    .iter()
                    .find(|record| record.state == UserShellState::Queued && record.armed)
                    .is_some_and(|record| record.queued_sequence < command_sequence)
            })
        {
            self.save(&candidate.snapshot);
            *chat = candidate;
            chat.user_shell_queue.notify_one();
            return Ok(RunAdmission::Idle);
        }
        self.save(&candidate.snapshot);
        *chat = candidate;
        Ok(RunAdmission::Idle)
    }

    async fn request_permission(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        session_operation: SessionPermissionOperation,
        prompt: PermissionPrompt,
        cancel: CancellationToken,
    ) -> Result<PermissionAnswer, String> {
        let request = PermissionRequest {
            parent_call: nested_calls::parent_call().await,
            mcp_server: prompt.mcp_server,
            id: tickloop_ids::generate(),
            loop_id: receipt.loop_id,
            execution: PermissionExecution::ToolCall {
                run_id: receipt.run_id,
                call_id: operation.call_id.clone(),
            },
            name: operation.name.clone(),
            input: operation.input.clone(),
            file_change: operation.file_change.clone().map(Box::new),
            approval_reason: prompt.reason,
            approval_source: prompt.source,
        };
        nested_calls::register_permission(request.id);
        self.wait_for_permission(receipt.chat_id, request, session_operation, cancel)
            .await
    }

    async fn wait_for_permission(
        &self,
        chat_id: ChatId,
        request: PermissionRequest,
        session_operation: SessionPermissionOperation,
        cancel: CancellationToken,
    ) -> Result<PermissionAnswer, String> {
        let nested = nested_calls::is_nested();
        if !nested {
            tool_batch::serialize_permission_prompt(&cancel).await?;
        }
        let model_receipt = request.execution.run_id().map(|run_id| Receipt {
            chat_id,
            loop_id: request.loop_id,
            run_id,
        });
        let (sender, receiver) = oneshot::channel();
        let owner = PermissionOwner::new(chat_id.into(), request.loop_id.into())
            .map_err(|error| error.to_string())?;
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&chat_id)
                .ok_or("permission requester is unavailable")?;
            if cancel.is_cancelled() {
                return Err("permission request cancelled before publication".into());
            }
            if let Some(receipt) = &model_receipt {
                chat.snapshot
                    .chat_loop
                    .set_state(LoopState::WaitingPermission);
                let run_cancel = chat
                    .run_cancel(receipt.run_id)
                    .cloned()
                    .ok_or("permission Run is unavailable")?;
                if !nested {
                    self.park_run(chat, receipt, run_cancel)
                        .map_err(|error| error.to_string())?;
                }
            } else if let PermissionExecution::UserShell { command_id } = request.execution
                && !chat.snapshot.user_shells.iter().any(|shell| {
                    shell.id == command_id && shell.state == UserShellState::WaitingPermission
                })
            {
                return Err("user Shell permission expired".into());
            }
            chat.denial_window.remember_human_prompt(&request);
            chat.snapshot.permissions.push(request.clone());
            self.inner.approvals.lock().await.insert(
                request.id,
                PendingApproval {
                    sender,
                    owner,
                    operation: session_operation,
                },
            );
            let receipt = model_receipt.clone().unwrap_or(Receipt {
                chat_id,
                loop_id: request.loop_id,
                run_id: Uuid::nil().into(),
            });
            self.publish_event(
                chat,
                &receipt,
                EventKind::PermissionRequested {
                    request: request.clone(),
                },
            );
        }
        if !nested
            && model_receipt.is_some()
            && let Some(next) = self
                .next_run(chat_id)
                .await
                .map_err(|error| error.to_string())?
        {
            self.run_task(next);
        }
        let answer = tokio::select! {biased; _ = nested_calls::stopped() => Err("Nested execution stopped after Auto rejection; call not dispatched".into()), _ = cancel.cancelled() => Err("permission request cancelled".into()), answer = receiver => answer.map_err(|_| "permission request expired".to_owned())};
        let resumed = if answer.is_ok() {
            if let Some(receipt) = &model_receipt
                && !nested
            {
                self.resume_parked_run(receipt, cancel.clone()).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        let mut chats = self.inner.chats.lock().await;
        if let Some(chat) = chats.get_mut(&chat_id) {
            if answer.is_err() || resumed.is_err() {
                chat.denial_window.human_prompts.retain(|(run, call, _)| {
                    Some(*run) != request.execution.run_id()
                        || *call != request.execution.call_key()
                });
            }
            chat.snapshot
                .permissions
                .retain(|permission| permission.id != request.id);
            if nested
                && chat.snapshot.permissions.is_empty()
                && chat.active_run == model_receipt.as_ref().map(|receipt| receipt.run_id)
            {
                chat.snapshot.chat_loop.set_state(LoopState::Running);
            }
            self.save(&chat.snapshot);
        }
        self.inner.approvals.lock().await.remove(&request.id);
        resumed?;
        answer
    }

    pub async fn set_search(
        &self,
        chat_id: ChatId,
        config: Option<tickloop_harness_protocol::SearchConfig>,
    ) -> Result<(), AgentError> {
        let _gate = self.inner.settings_writes.lock().await;
        if let Some(config) = &config {
            tickloop_harness_tools::web_search::validate_config(config)
                .map_err(AgentError::Invalid)?;
        }
        let tools = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .tools
            .clone();
        let saved = config.clone();
        self.write_user_settings(move |settings| settings.write(SettingsUpdate::Search(saved)))
            .await?;
        tools
            .set_search(config.clone())
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot.search_config = config;
        if let Some(delegation) = &chat.snapshot.delegation {
            self.record_delegation(
                chat_id,
                delegation.clone(),
                chat.snapshot.search_config.clone(),
            );
            self.save(&chat.snapshot);
        }
        Ok(())
    }

    pub async fn set_execution_mode(
        &self,
        chat_id: ChatId,
        mode: ExecutionMode,
    ) -> Result<(), AgentError> {
        self.require_root_chat(chat_id, "change the execution authorization mode")
            .await?;
        let _settings_gate = self.inner.settings_writes.lock().await;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        // Claude Code's `handlePlanModeTransition`: leaving Plan owes the model an exit notice, and
        // returning to Plan before it is sent withdraws it.
        let leaves_plan =
            chat.snapshot.policy.mode == ExecutionMode::Plan && mode != ExecutionMode::Plan;
        match (chat.snapshot.policy.mode, mode) {
            (_, ExecutionMode::Plan) => chat.needs_plan_mode_exit_notice = false,
            (ExecutionMode::Plan, _) => chat.needs_plan_mode_exit_notice = true,
            _ => {}
        }
        chat.change_execution_mode(mode);
        chat.snapshot.policy.revision += 1;
        self.save(&chat.snapshot);
        drop(chats);
        // Plan mode held the goal's continuation back.
        if leaves_plan {
            self.reconsider_goal(chat_id).await;
        }
        Ok(())
    }

    pub async fn set_permission_rules(
        &self,
        chat_id: ChatId,
        rules: Vec<PermissionRule>,
    ) -> Result<(), AgentError> {
        self.require_root_chat(chat_id, "replace permission rules")
            .await?;
        if rules
            .iter()
            .any(|rule| rule.tool.is_empty() || rule.decision == Decision::Check)
        {
            return Err(AgentError::Invalid(
                "rules require a tool and allow, ask or deny decision".into(),
            ));
        }
        for rule in rules.iter().filter(|rule| rule.tool.starts_with("mcp__")) {
            tickloop_harness_tools::permissions::validate_rule(rule)
                .map_err(|error| AgentError::Invalid(error.into()))?;
        }
        let _settings_gate = self.inner.settings_writes.lock().await;
        let changes = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat_settings::persistent_rule_changes(&chat.snapshot, &chat.bookkeeping, &rules)?
        };
        self.save_persistent_rule_changes(&changes).await?;
        let mut chats = self.inner.chats.lock().await;
        self.sync_persistent_rules(&mut chats, &changes, Some(chat_id));
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot.policy.rules = rules;
        chat_settings::record_rule_projects(&mut chat.bookkeeping, &changes);
        chat.snapshot.policy.revision += 1;
        self.save(&chat.snapshot);
        Ok(())
    }

    pub async fn set_project_permission_constraints(
        &self,
        chat_id: ChatId,
        constraints: Vec<PermissionRule>,
    ) -> Result<(), AgentError> {
        self.require_root_chat(chat_id, "replace project constraints")
            .await?;
        if constraints.iter().any(|rule| {
            rule.tool.is_empty() || !matches!(rule.decision, Decision::Ask | Decision::Deny)
        }) {
            return Err(AgentError::Invalid(
                "project constraints may contain only ask or deny rules with a tool".into(),
            ));
        }
        let _settings_gate = self.inner.settings_writes.lock().await;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot.policy.project_constraints = constraints;
        chat.snapshot.policy.revision += 1;
        self.save(&chat.snapshot);
        Ok(())
    }

    pub async fn set_managed_permission_constraints(
        &self,
        chat_id: ChatId,
        constraints: Vec<PermissionRule>,
    ) -> Result<(), AgentError> {
        self.require_root_chat(chat_id, "replace managed constraints")
            .await?;
        if constraints.iter().any(|rule| {
            rule.tool.is_empty() || !matches!(rule.decision, Decision::Ask | Decision::Deny)
        }) {
            return Err(AgentError::Invalid(
                "managed constraints may contain only ask or deny rules with a tool".into(),
            ));
        }
        let _settings_gate = self.inner.settings_writes.lock().await;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot.policy.managed_constraints = constraints;
        chat.snapshot.policy.revision += 1;
        self.save(&chat.snapshot);
        Ok(())
    }

    pub async fn set_directories(
        &self,
        chat_id: ChatId,
        folder: PathBuf,
        additional: Vec<PathBuf>,
    ) -> Result<(), AgentError> {
        self.require_root_chat(chat_id, "change working directories")
            .await?;
        require_folder(
            &self
                .inner
                .chats
                .lock()
                .await
                .get(&chat_id)
                .ok_or(AgentError::NotFound)?
                .snapshot,
        )?;
        let canonical = |path: PathBuf| -> Result<PathBuf, AgentError> {
            let path = path
                .canonicalize()
                .map_err(|_| AgentError::Invalid("directory is unavailable".into()))?;
            if !path.is_dir() {
                return Err(AgentError::Invalid("path is not a directory".into()));
            }
            Ok(path)
        };
        let folder = canonical(folder)?;
        let mut additional = additional
            .into_iter()
            .map(canonical)
            .collect::<Result<Vec<_>, _>>()?;
        additional.sort();
        additional.dedup();
        self.require_working_directory_trust(&folder)?;
        let _settings_gate = self.inner.settings_writes.lock().await;
        let saved = self.load_user_settings(Some(folder.clone())).await?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let folder_changed = chat.snapshot.folder.as_ref() != Some(&folder);
        let directory_changed =
            folder_changed || chat.snapshot.additional_directories != additional;
        if folder_changed {
            // Persistent rules belong to the working directory's partition: changing directories
            // swaps in the new directory's persistent rules, while session rules are kept.
            chat.skills = self.discover_skills(Some(&folder));
            chat.snapshot.folder = Some(folder);
            if let Some(saved) = saved {
                self.replace_persistent_rules(&mut chat.snapshot, &mut chat.bookkeeping, saved);
            }
        }
        chat.bookkeeping
            .additional_directory_sources
            .retain(|path, _| additional.contains(path));
        for path in &additional {
            chat.bookkeeping
                .additional_directory_sources
                .entry(path.clone())
                .or_insert_with(|| {
                    [tickloop_harness_protocol::AdditionalDirectorySource::Session].into()
                });
        }
        chat.snapshot.additional_directories = additional;
        if directory_changed {
            chat.snapshot.policy.revision += 1;
        }
        self.save(&chat.snapshot);
        Ok(())
    }

    pub async fn answer_permission_with_scope(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
        allow: bool,
        scope: PermissionScope,
    ) -> Result<(), AgentError> {
        self.answer_permission_with_comment(chat_id, request_id, allow, scope, None)
            .await
    }

    pub async fn answer_permission_with_rule(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
        specifier: String,
    ) -> Result<(), AgentError> {
        self.answer_permission_scoped(
            chat_id,
            request_id,
            true,
            PermissionScope::Persistent,
            None,
            Some(specifier),
        )
        .await
    }

    pub async fn answer_permission_with_comment(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
        allow: bool,
        scope: PermissionScope,
        comment: Option<String>,
    ) -> Result<(), AgentError> {
        self.answer_permission_scoped(chat_id, request_id, allow, scope, comment, None)
            .await
    }

    async fn answer_permission_scoped(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
        allow: bool,
        scope: PermissionScope,
        comment: Option<String>,
        persistent_specifier: Option<String>,
    ) -> Result<(), AgentError> {
        if allow {
            let chats = self.inner.chats.lock().await;
            if chats.get(&chat_id).is_some_and(|chat| {
                chat.snapshot.permissions.iter().any(|request| {
                    request.id == request_id
                        && request.name == tickloop_harness_tools::EXIT_PLAN_MODE_TOOL_NAME
                })
            }) {
                return Err(AgentError::Invalid(
                    "choose an execution mode in the plan approval dialog".into(),
                ));
            }
        }
        if scope != PermissionScope::Once {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            let request = chat
                .snapshot
                .permissions
                .iter()
                .find(|request| request.id == request_id)
                .ok_or_else(|| AgentError::Invalid("permission request expired".into()))?;
            if request.approval_source == PermissionApprovalSource::ExplicitAsk {
                return Err(AgentError::Invalid(
                    "explicit Ask requires approval for this action only".into(),
                ));
            }
        }
        let comment = comment
            .map(|comment| comment.trim().to_owned())
            .filter(|comment| !comment.is_empty());
        if comment.is_some() && scope != PermissionScope::Once {
            return Err(AgentError::Invalid(
                "permission comments apply only to a single action".into(),
            ));
        }
        let _settings_gate = if scope == PermissionScope::Persistent {
            self.require_root_chat(chat_id, "persist permission grants")
                .await?;
            let gate = self.inner.settings_writes.lock().await;
            self.add_persistent_rule_for_request(chat_id, request_id, allow, persistent_specifier)
                .await?;
            Some(gate)
        } else {
            if persistent_specifier.is_some() {
                return Err(AgentError::Invalid(
                    "a permission rule specifier requires persistent scope".into(),
                ));
            }
            None
        };
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        answerable_permission_request(chat, request_id, allow)?;
        let mut approvals = self.inner.approvals.lock().await;
        let (permission_owner, session_operation) = approvals
            .get(&request_id)
            .map(|pending| (pending.owner, pending.operation.clone()))
            .ok_or_else(|| AgentError::Invalid("permission was already answered".into()))?;
        let mut session_context = None;
        let mut session_candidate = None;
        if scope == PermissionScope::Session {
            let context = self.inner.session_permissions.lock().await;
            let mut candidate = context.clone();
            candidate
                .record(
                    permission_owner,
                    session_operation,
                    if allow {
                        SessionPermissionDecision::Allow
                    } else {
                        SessionPermissionDecision::Deny
                    },
                )
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
            session_context = Some(context);
            session_candidate = Some(candidate);
        }
        let mut snapshot = chat.snapshot.clone();
        snapshot
            .permissions
            .retain(|request| request.id != request_id);
        self.save(&snapshot);
        chat.snapshot = snapshot;
        let answer_revision = chat.snapshot.policy.revision;
        if let (Some(mut context), Some(candidate)) = (session_context, session_candidate) {
            if !allow {
                for child in chats.values_mut() {
                    let Some(metadata) = &mut child.snapshot.delegation else {
                        continue;
                    };
                    let Some(denials) = &mut metadata.ceiling.session_denials else {
                        continue;
                    };
                    let parent = PermissionOwner::new(
                        metadata.parent_chat_id.into(),
                        metadata.parent_loop_id.into(),
                    )
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                    let mut changed = false;
                    for denial in candidate.freeze_denials(parent) {
                        if !denials.contains(&denial) {
                            denials.push(denial);
                            changed = true;
                        }
                    }
                    if changed {
                        self.record_delegation(
                            child.snapshot.id,
                            metadata.clone(),
                            child.snapshot.search_config.clone(),
                        );
                        self.save(&child.snapshot);
                    }
                }
            }
            *context = candidate;
        }
        let pending = approvals.remove(&request_id).unwrap();
        pending
            .sender
            .send(PermissionAnswer {
                allow,
                revision: answer_revision,
                comment,
            })
            .map_err(|_| AgentError::Invalid("permission request expired".into()))
    }

    pub async fn answer_permission(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
        allow: bool,
    ) -> Result<(), AgentError> {
        self.answer_permission_with_scope(chat_id, request_id, allow, PermissionScope::Once)
            .await
    }

    /// Lets the terminal and tools ask about image input without copying the whole snapshot.
    pub async fn accepts_images(&self, chat: ChatId) -> Result<bool, AgentError> {
        self.inner
            .chats
            .lock()
            .await
            .get(&chat)
            .map(|chat| chat.snapshot.accepts_images())
            .ok_or(AgentError::NotFound)
    }

    /// The id context of `chat`'s Loop, for model text written outside the Chat lock. A Chat
    /// this Agent does not hold writes ids in the default form.
    pub(crate) async fn chat_ids(&self, chat: ChatId) -> tickloop_ids::IdContext {
        self.inner
            .chats
            .lock()
            .await
            .get(&chat)
            .map_or_else(tickloop_ids::IdContext::new, |chat| chat.ids)
    }

    pub async fn snapshot(&self, chat_id: ChatId) -> Result<ChatSnapshot, AgentError> {
        let chats = self.inner.chats.lock().await;
        let mut snapshot = chats
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .clone();
        snapshot.approval_records = chats[&chat_id]
            .denial_window
            .records
            .iter()
            .cloned()
            .collect();
        snapshot.task_board = shared_tasks::view(&chats, chat_id);
        snapshot.trigger_origins = chats[&chat_id].bookkeeping.lifecycle.trigger_origins();
        snapshot.provider_failures = self.inner.provider_failures.lock().unwrap().clone();
        Ok(snapshot)
    }

    pub async fn take_queued_inputs(
        &self,
        chat_id: ChatId,
        draft_empty: bool,
    ) -> Result<Vec<QueuedInput>, AgentError> {
        let (taken, events) = {
            let mut chats = self.inner.chats.lock().await;
            let original = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let entries = original.snapshot.queued_inputs();
            let take_shell =
                draft_empty && entries.len() == 1 && entries[0].kind == QueuedInputKind::Shell;
            let taken = entries
                .into_iter()
                .filter(|entry| {
                    matches!(
                        entry.kind,
                        QueuedInputKind::Message | QueuedInputKind::Command
                    ) || take_shell
                })
                .collect::<Vec<_>>();
            if taken.is_empty() {
                return Ok(Vec::new());
            }
            let mut candidate = original.clone();
            let ids = taken.iter().map(|entry| entry.id).collect::<HashSet<_>>();
            candidate.snapshot.pending_inputs.retain(|pending| {
                pending.agent_message.is_some() || !ids.contains(&pending.receipt.run_id.uuid())
            });
            for entry in &taken {
                if entry.kind == QueuedInputKind::Message {
                    self.settle_input(&mut candidate, entry.id.into(), SettledOutcome::Withdrawn);
                }
            }
            candidate
                .snapshot
                .pending_commands
                .retain(|command| !ids.contains(&command.id));
            candidate.snapshot.user_shells.retain(|record| {
                record.state != UserShellState::Queued || !ids.contains(&record.id)
            });
            if !taken.is_empty()
                && candidate.active_run.is_none()
                && candidate.snapshot.pending_inputs.is_empty()
                && candidate.snapshot.chat_loop.state == LoopState::Queued
            {
                candidate
                    .snapshot
                    .chat_loop
                    .set_state(LoopState::Interrupted);
            }
            let mut events = Vec::new();
            for entry in &taken {
                let Some(loop_id) = entry.loop_id else {
                    continue;
                };
                if entry.kind != QueuedInputKind::Message {
                    continue;
                }
                candidate.snapshot.last_sequence += 1;
                let event = Event {
                    sequence: candidate.snapshot.last_sequence,
                    chat_id,
                    loop_id,
                    run_id: entry.id.into(),
                    kind: EventKind::InputTakenBack,
                };
                candidate.snapshot.events.push(event.clone());
                events.push(event);
            }
            while candidate.snapshot.events.len() > 1024 {
                candidate.snapshot.events.remove(0);
            }
            self.save(&candidate.snapshot);
            *original = candidate;
            (taken, events)
        };
        for event in events {
            let _ = self.inner.events.send(event);
        }
        Ok(taken)
    }

    pub async fn queue_command(
        &self,
        chat_id: ChatId,
        command: String,
    ) -> Result<Uuid, AgentError> {
        if command.trim().is_empty() || !command.trim_start().starts_with('/') {
            return Err(AgentError::Invalid(
                "queued command must start with /".into(),
            ));
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot.last_queue_sequence += 1;
        let id = tickloop_ids::generate();
        chat.snapshot.pending_commands.push(PendingCommand {
            id,
            chat_id,
            loop_id: Some(chat.snapshot.chat_loop.id),
            command,
            queued_sequence: chat.snapshot.last_queue_sequence,
            armed: true,
            dispatched: false,
        });
        Ok(id)
    }

    pub async fn take_next_queued_command(
        &self,
        chat_id: ChatId,
    ) -> Result<Option<QueuedInput>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if chat.active.is_some() || !chat.parked.is_empty() || chat.has_foreground_user_shell() {
            return Ok(None);
        }
        let Some(first) = chat.snapshot.queued_inputs().into_iter().next() else {
            return Ok(None);
        };
        if !first.armed || first.kind != QueuedInputKind::Command {
            return Ok(None);
        }
        let command = chat
            .snapshot
            .pending_commands
            .iter_mut()
            .find(|command| command.id == first.id)
            .ok_or(AgentError::NotFound)?;
        command.armed = false;
        command.dispatched = true;
        Ok(Some(first))
    }

    pub async fn finish_queued_command(
        &self,
        chat_id: ChatId,
        command_id: Uuid,
    ) -> Result<(), AgentError> {
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let Some(command) = chat
                .snapshot
                .pending_commands
                .iter()
                .find(|command| command.id == command_id)
            else {
                return Err(AgentError::NotFound);
            };
            if !command.dispatched {
                return Err(AgentError::Invalid(
                    "queued command was not dispatched".into(),
                ));
            }
            chat.snapshot
                .pending_commands
                .retain(|command| command.id != command_id);
        }
        self.start_user_shell_queue(chat_id).await
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    pub fn subscribe_task_notifications(&self) -> broadcast::Receiver<TaskNotification> {
        self.inner.task_notifications.subscribe()
    }

    /// Writes a foreground `!` command and its result together when it finishes or backgrounds,
    /// before notifications deferred while the command was running.
    fn append_foreground_shell_messages(
        &self,
        chat: &mut Chat,
        messages: impl IntoIterator<Item = Message>,
    ) -> Result<(), AgentError> {
        chat.deferred_messages.splice(0..0, messages);
        self.flush_deferred_messages(chat, MessageBoundary::Idle)
    }

    /// Defers local messages while the Chat is busy so tool calls and results stay adjacent.
    fn append_local_messages(
        &self,
        chat: &mut Chat,
        messages: impl IntoIterator<Item = Message>,
    ) -> Result<(), AgentError> {
        chat.deferred_messages.extend(messages);
        self.flush_deferred_messages(chat, MessageBoundary::Idle)
    }

    /// A background task notification may queue one continuation run. The caller starts it
    /// after releasing the Chat lock.
    fn notify_task_completion(
        &self,
        chat: &mut Chat,
        loop_id: Option<LoopId>,
        notification: TaskNotification,
    ) -> Result<bool, AgentError> {
        chat.bookkeeping
            .deferred_notifications
            .push(tick_protocol::InstanceRef::shell(notification.task_id));
        self.append_local_messages(
            chat,
            [local_messages::task_notification(&chat.ids, &notification)],
        )?;
        let _ = self.inner.task_notifications.send(notification);
        Ok(self.queue_task_continuation(chat, loop_id))
    }

    /// Continues the owning Loop, but not for an unowned Shell result, after an interrupted or
    /// failed turn, or while the Chat is stopped. An already queued input absorbs further task
    /// notifications into the same run.
    fn queue_task_continuation(&self, chat: &mut Chat, loop_id: Option<LoopId>) -> bool {
        let is_being_interrupted = chat
            .active_run
            .is_some_and(|run| chat.user_cancelled_runs.contains(&run));
        if loop_id != Some(chat.snapshot.chat_loop.id)
            || !chat.snapshot.chat_loop.receiving.is_receiving()
            || is_being_interrupted
            || chat
                .snapshot
                .pending_inputs
                .iter()
                .any(|pending| pending.armed)
        {
            return false;
        }
        let chat_loop = &mut chat.snapshot.chat_loop;
        let loop_id = chat_loop.id;
        if matches!(chat_loop.state, LoopState::Interrupted | LoopState::Failed) {
            return false;
        }
        if !matches!(
            chat_loop.state,
            LoopState::Running | LoopState::WaitingPermission | LoopState::WaitingQuestion
        ) {
            chat_loop.set_state(LoopState::Queued);
        }
        let receipt = Receipt {
            chat_id: chat.snapshot.id,
            loop_id,
            run_id: tickloop_ids::generate().into(),
        };
        queue_continuation(chat, receipt);
        true
    }

    fn flush_deferred_messages(
        &self,
        chat: &mut Chat,
        boundary: MessageBoundary,
    ) -> Result<(), AgentError> {
        let is_writable = match boundary {
            MessageBoundary::Idle => {
                chat.active.is_none() && chat.parked.is_empty() && !chat.has_foreground_user_shell()
            }
            MessageBoundary::ToolResults => chat.parked.is_empty(),
        };
        if chat.deferred_messages.is_empty() || !is_writable {
            return Ok(());
        }
        chat.snapshot
            .messages
            .extend(std::mem::take(&mut chat.deferred_messages));
        let notified = std::mem::take(&mut chat.bookkeeping.deferred_notifications);
        if chat.active.is_some() {
            // Written between tool results, so the running turn reads them next.
            self.mark_taken_in(chat, notified);
        } else {
            chat.bookkeeping.unread_notifications.extend(notified);
        }
        self.save(&chat.snapshot);
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.chat_leases.close();
        for chat in self.inner.chats.lock().await.values() {
            self.release_runs_at_shutdown(chat);
        }
        self.cancel_upgrade().await;
        self.inner.shutdown.cancel();
        // Inputs a lost answer leaves open stay open; waiters on them learn of the shutdown.
        self.inner.ledger_changes.send_replace(());
        let chat_tools = self
            .inner
            .chats
            .lock()
            .await
            .iter()
            .map(|(id, chat)| (*id, chat.tools.clone()))
            .collect::<Vec<_>>();
        for (chat_id, tools) in chat_tools {
            for command in tools.commands().await {
                if let Ok(command) = tools.stop_command(command.operation_id).await {
                    let _ = self.record_command(chat_id, command).await;
                }
            }
            for command in tools.user_commands().await {
                if let Ok(command) = tools
                    .stop_user_shell(command.operation_id, chat_id.into())
                    .await
                {
                    let _ = self.record_user_shell(chat_id, command).await;
                }
            }
        }
        for chat in self.inner.chats.lock().await.values() {
            self.stop_runs_at_shutdown(chat);
            if let Some(cancel) = &chat.feedback_watcher {
                cancel.cancel();
            }
        }
        self.stop_transcript_flushing();
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
        let host_tools = self.inner.host_tools.read().unwrap().clone();
        if let Some(host_tools) = host_tools {
            host_tools.shutdown().await;
        }
        let chat_clients = self
            .inner
            .chats
            .lock()
            .await
            .iter()
            .map(|(chat_id, chat)| (*chat_id, chat.client.clone(), chat.pending_client.clone()))
            .collect::<Vec<_>>();
        let reviewer = self.inner.auto_reviewer.lock().await.clone();
        for (chat_id, client, pending_client) in chat_clients {
            client.close_chat(chat_id).await;
            if let Some(pending_client) = pending_client {
                pending_client.close_chat(chat_id).await;
            }
            if let Some(reviewer) = &reviewer {
                reviewer.close_chat(chat_id).await;
            }
        }
        // The last writes land before the leases go back: none lands after another process took
        // a Chat over.
        self.close_transcripts().await;
        self.inner.chat_leases.release_all();
    }
}

pub(crate) fn validate_host_schema_input(
    name: &str,
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<(), String> {
    tool_execution::validate_schema(schema, input, "input")
        .map_err(|reason| format!("invalid {name} input: {reason}"))
}

/// The prompt that resumes a reply cut short after its text was shown; replaying it would show
/// that text twice.
fn response_continuation_prompt(error: &AgentError) -> Option<&'static str> {
    match error {
        AgentError::Model(ModelError::ResponseRejected(ModelResponseRejection::OutputLimit)) => {
            Some(OUTPUT_LIMIT_CONTINUATION_PROMPT)
        }
        AgentError::Model(error) if error.kind() == ModelFailureKind::StreamInterrupted => {
            Some(STREAM_INTERRUPTION_CONTINUATION_PROMPT)
        }
        _ => None,
    }
}

pub(crate) fn reserved_host_tool_names() -> HashSet<String> {
    tickloop_harness_tools::Tools::definitions()
        .into_iter()
        .chain(tool_exposure::fold_definitions())
        .chain(plan::definitions(false))
        .chain([interaction::definition(false)])
        .chain(tick_subscriptions::definitions())
        .chain(delegation::definitions())
        .chain(agent_messages::definitions())
        .map(|definition| definition.name)
        .chain(std::iter::once(
            tickloop_harness_policy::skills::SKILL_TOOL_NAME.into(),
        ))
        .chain(
            mcp_resources::definitions()
                .into_iter()
                .map(|tool| tool.name),
        )
        .chain(crate::mcp_tools::ARTIFACT_ACCOUNT_TOOL_NAMES.map(String::from))
        .collect()
}

/// A built-in Skill only the user invokes, such as /slides, guides the run its invocation started.
/// When that run stopped at Artifact sign-in before creating an artifact, the user's next input
/// to the same Loop continues the work, so the Skill guides that run instead.
fn carry_user_skills_past_sign_in(chat: &mut Chat, loop_id: LoopId, run_id: RunId) {
    let operations = &chat.snapshot.operations;
    let stopped_at_sign_in = |run: RunId| {
        let mut in_run = operations
            .iter()
            .filter(|operation| operation.run_id == run);
        let started_sign_in = in_run.clone().any(|operation| {
            operation.loop_id == loop_id && operation.name == crate::mcp_tools::ARTIFACT_LOGIN_TOOL
        });
        started_sign_in
            && !in_run.any(|operation| {
                operation.name == crate::mcp_tools::ARTIFACT_EDIT_TOOL
                    && operation.input["action"] == "create"
                    && matches!(
                        operation.state,
                        OperationState::Completed | OperationState::Unknown
                    )
            })
    };
    for skill in &mut chat.snapshot.invoked_skills {
        if skill.source == tickloop_harness_protocol::SkillSource::BuiltIn
            && let Some(bound) = skill.run_id
            && chat.active_run != Some(bound)
            && stopped_at_sign_in(bound)
        {
            skill.run_id = Some(run_id);
        }
    }
}

/// What a repeated identical Skill call tells the model: the guidance it wrote into history stays.
const ALREADY_IN_CONVERSATION: &str =
    "Its guidance is already in this conversation; follow it without loading it again.";

fn record_invoked_skill(chat: &mut Chat, invoked: tickloop_harness_protocol::InvokedSkill) -> bool {
    let identical = chat.snapshot.invoked_skills.iter().any(|existing| {
        existing.name == invoked.name
            && existing.source == invoked.source
            && existing.content == invoked.content
    });
    let restored = chat.bookkeeping.restored_skill_names.remove(&invoked.name);
    chat.snapshot
        .invoked_skills
        .retain(|existing| existing.name != invoked.name);
    chat.snapshot.invoked_skills.push(invoked);
    !identical || restored
}

fn remote_skill_allowed(chat: &Chat, source: &tickloop_harness_protocol::SkillSource) -> bool {
    let tickloop_harness_protocol::SkillSource::Remote {
        server_id,
        account_id,
        ..
    } = source
    else {
        return false;
    };
    mcp_resource_authority_allowed(
        chat,
        &tickloop_harness_protocol::HostToolAuthority::Mcp {
            server_id: server_id.clone(),
            account_id: account_id.clone(),
        },
    )
}

fn mcp_resource_authority_allowed(
    chat: &Chat,
    authority: &tickloop_harness_protocol::HostToolAuthority,
) -> bool {
    chat.snapshot.delegation.as_ref().is_none_or(|delegation| {
        delegation
            .ceiling
            .host_tools
            .iter()
            .any(|(name, authorities)| {
                (name == crate::mcp_tools::ARTIFACT_READ_TOOL || name.starts_with("mcp__"))
                    && authorities.contains(authority)
            })
    })
}

#[async_trait::async_trait]
impl tickloop_harness_tools::ShellRetention for WeakAgent {
    fn spawn(&self, task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>) {
        if let Some(agent) = self.upgrade() {
            let shutdown = agent.inner.shutdown.clone();
            agent.inner.tasks.spawn(async move {
                tokio::select! { _ = shutdown.cancelled() => {}, _ = task => {} }
            });
        }
    }
    async fn is_retained(&self, chat: uuid::Uuid, task: uuid::Uuid) -> bool {
        match self.upgrade() {
            Some(agent) => agent.monitor_may_list_shell(chat.into(), task).await,
            None => false,
        }
    }
}

/// What a new Chat starts from; everything else in it starts empty.
pub(crate) struct NewChat {
    snapshot: ChatSnapshot,
    bookkeeping: ChatBookkeeping,
    client: Arc<dyn ModelClient>,
    output_style: tickloop_harness_base::output_style::OutputStyle,
    user_context: OnceLock<String>,
    announced_date: Option<String>,
    tools: Arc<tickloop_harness_tools::Tools>,
    skills: Arc<tickloop_harness_policy::skills::SkillRegistry>,
    file_history: Option<FileHistory>,
}

impl NewChat {
    fn into_chat(self) -> Chat {
        let Self {
            snapshot,
            bookkeeping,
            client,
            output_style,
            user_context,
            announced_date,
            tools,
            skills,
            file_history,
        } = self;
        Chat {
            ids: tickloop_ids::IdContext::new(),
            family_generation: 0,
            family_stopping: false,
            task_checkpoint_pending: false,
            family_finishing: HashSet::new(),
            snapshot,
            bookkeeping,
            start_new_tool_result_message: false,
            needs_plan_mode_exit_notice: false,
            mode_before_plan: None,
            active: None,
            active_run: None,
            upgrade_wait: None,
            parked: HashMap::new(),
            releases: HashMap::new(),
            ready_runs: VecDeque::new(),
            client,
            pending_client: None,
            pending_model_thinking: None,
            tools,
            skills,
            content_replacements: Default::default(),
            file_history,
            watching_commands: false,
            watching_user_shells: false,
            user_shell_queue: Arc::new(Notify::new()),
            feedback_watcher: None,
            feedback_inputs: HashMap::new(),
            feedback_cleanup_started: false,
            user_loops: HashMap::new(),
            deferred_messages: Vec::new(),
            user_cancelled_runs: HashSet::new(),
            output_style,
            user_context,
            environment: OnceLock::new(),
            announced_date,
            auto_denials: auto_mode::DenialTracking::default(),
            denial_window: denials::DenialWindow::default(),
            branch_reservation: None,
            goal_recorded_tokens: 0,
            loaded_from_history: false,
            goal_clock_ms: 0,
            cache_prefix: Default::default(),
        }
    }
}

#[cfg(test)]
mod thinking_settings_tests {
    use super::*;

    #[test]
    fn disabled_preference_applies_only_when_model_card_allows_it() {
        let model = tickloop_harness_model::cards::built_in_model_profiles().remove(0);
        let mut snapshot =
            empty_snapshot(Uuid::new_v4().into(), Some(PathBuf::new()), model.clone());
        snapshot.thinking_enabled = false;
        assert_eq!(
            snapshot.effective_reasoning_effort(),
            Some(tickloop_harness_protocol::ReasoningEffort::Off)
        );

        snapshot.model = tickloop_harness_model::cards::ModelCatalog::defaults()
            .get("anthropic-api", "claude-fable-5")
            .unwrap();
        assert_eq!(snapshot.effective_reasoning_effort(), None);
        assert!(!snapshot.thinking_enabled);

        snapshot.model = model;
        assert_eq!(
            snapshot.effective_reasoning_effort(),
            Some(tickloop_harness_protocol::ReasoningEffort::Off)
        );
    }
}
