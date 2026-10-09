mod approval_assessment;
pub(crate) mod approval_context;
mod auto_approval;
mod auto_health;
mod auto_mode;
pub(crate) mod auto_rules;
pub use auto_rules::{
    AutoEnvironment, AutoEnvironmentSource, AutoPolicy, AutoRule, AutoRuleChange,
    AutoRuleGroupSettings, AutoRuleKind, AutoRuleSettings, AutoRuleSource,
};
mod auto_settings;
mod branch;
mod jev_review;
mod local_messages;
mod loop_lease;
pub use approval_assessment::{
    ApprovalAssessmentDetails, ApprovalAttemptDetails, ApprovalAttemptOutcome, ApprovalFailureKind,
    ApprovalFallbackReason, ApprovalOutcome, ApprovalReviewer, ApprovalReviewerKind,
};
pub use auto_health::AutoReviewerHealth;
pub use auto_rules::AutoBlockKind;
pub use auto_rules::{RuleApplicability, SoftDenyClearance, TaskScopeFinding};
pub use loop_lease::{LoopTriggerNotice, LoopTriggerNoticeKind};
mod host_context;
mod model_request;
mod persistence;
pub use persistence::SavedChat;
mod usage;
pub use usage::{
    CounterSummary, CounterTotal, LedgerError, ModelUsageSummary, ObservationEffect,
    ProtocolFailure, RequestFailure, RequestKind, RequestOutcome, RequestOwner, RequestSequence,
    RequestUsageLedger, RequestUsageRecord, ResponseRejection, UsageCounter, UsageCoverage,
    UsageSummary,
};
mod agent_messages;
mod family;
pub use family::SubagentNotificationDelivery;
pub(crate) use family::summary_state;
mod family_mailbox;
pub use family::{
    DirectedHumanSource, FamilyAgent, FamilyAvailability, FamilyCompletion, FamilyInputRecord,
    FamilyRole,
};
pub use family_mailbox::MessageCause;
mod artifact_ticks;
mod chat_title;
mod monitor_members;
pub use monitor_members::{MonitorMember, MonitorMemberSource, MonitorMembership};
mod compaction;
mod context;
mod delegation;
pub(crate) use chat_title::chat_display_title;
mod denials;
mod event_sources;
pub use denials::{
    ApprovalHistoryView, ApprovalPath, ApprovalRecord, ApprovalResult, DenialAutoFinding,
    DenialAutoRuleSummary, DenialCategory, DenialPersistence, DenialRecord, DenialReviewFacts,
    DenialRuleSource, DenialRuleSummary, DenialsView, DisplaySummary, MAX_RECENT_DENIALS,
};
mod directed_summary;
mod input_context;
mod interaction;
#[cfg(test)]
pub(crate) use local_messages::tick_delivery as tick_delivery_message;
pub use local_messages::{
    FOLDED_TICK_REPLY_CHARACTERS, INTERRUPT_MESSAGE, INTERRUPT_MESSAGE_FOR_TOOL_USE, LocalMessage,
    TaskNotification, TaskNotificationStatus, TickDeliveryLine,
};
mod chat_settings;
pub(crate) use chat_settings::new_chat_settings;
#[cfg(test)]
mod file_read_tests;
mod recovery;
#[cfg(test)]
mod request_failure_tests;
#[cfg(test)]
mod request_race_tests;
mod rewind;
mod session_permissions;
mod shell_task_center;
mod system_prompt;
mod tool_execution;
mod tool_results;
mod web_fetch;
pub(crate) use tick_content::WATERMARK_DIRECTORY;
use tool_execution::{ToolCallOutcome, ToolInvocation, ToolRegistry};
pub(crate) use tool_results::TOOL_RESULTS_SUBDIRECTORY;
mod user_shell;
#[cfg(test)]
mod user_shell_tests;
pub use interaction::{
    ASK_USER_QUESTION_TOOL_NAME, Question, QuestionAnswer, QuestionOption, QuestionRequest,
    START_GOAL_TOOL_NAME,
};
mod commands;
mod tick_content;
mod tick_subscriptions;
pub use tick_subscriptions::{
    DEFAULT_CADENCE_SECONDS, MAX_CADENCE_SECONDS, MAX_DELIVERY_PAYLOAD_BYTES, MIN_CADENCE_SECONDS,
    TICK_SUBSCRIPTION_SOURCE, TickLinkState, TickSubscription, TickSubscriptionChange,
    TickSubscriptionStatus,
};
mod triggers;
pub use triggers::{Trigger, TriggerSource};
mod permission_management;
mod plan;
mod todo_lifecycle;
pub use crate::settings::{AutoReviewerPreference, AutoSettingsRevision};
pub(crate) use auto_settings::jev_key_status_in;
pub use branch::{
    BranchCreation, BranchOrigin, BranchOutcome, BranchPublicItem, BranchPublicSource,
    BranchRequest, BranchSessionPermissionRegistration,
};
pub use compaction::{
    COMPACT_BOUNDARY_TEXT, CompactBoundary, CompactTrigger, CompactionMarker, SummarizeMetadata,
};
pub use context::{CompactionOutcome, CompactionStatus, ModelChange};
pub use directed_summary::{
    ActiveConversationItem, DirectedSummaryDraft, DirectedSummaryOutcome, DirectedSummaryRequest,
    SummaryDirection,
};
pub use input_context::{ImageContext, ImageOrigin, InputContext, TextContext};
pub use permission_management::{
    PermissionChange, PermissionChangeError, PermissionRuleScope, PermissionRuleSource,
    PermissionRuleView, PermissionsRevision, PermissionsView,
};
pub(crate) use permission_management::{canonical_directory, rule_views, validate_rule};
pub use plan::{
    ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME, Plan, Progress, TODO_WRITE_TOOL_NAME,
    TodoItem, TodoStatus,
};
pub(crate) use plan::{
    MAX_PLAN_SLUG_BYTES, PLAN_FILE_EXTENSION, PLANS_DIRECTORY, is_valid_plan_slug, plan_file_slug,
};
pub use rewind::{CodeRewind, RewindDraft, RewindMode, RewindOutcome, RewindPoint};
pub(crate) use shell_task_center::recover_shell_update_time;
pub use shell_task_center::{
    SHELL_TASK_OUTPUT_MAX_BYTES, SHELL_TASK_PROMPT_QUIET_PERIOD, ShellTask, ShellTaskAction,
    ShellTaskCapabilities, ShellTaskCenter, ShellTaskCenterError, ShellTaskDelta, ShellTaskNotice,
    ShellTaskOrigin, ShellTaskOutputChunk, ShellTaskOwner, ShellTaskPromptWatchdog,
    ShellTaskSnapshot, ShellTaskState, ShellTaskSubscription, ShellTaskSubscriptionEvent,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::{Mutex, Notify, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::file_change::load_file_change_review;
use crate::file_history::FileHistory;
use crate::model::{
    Content, Message, ModelClient, ModelConfig, ModelError, ModelRequest, ModelResponseRejection,
    ReasoningEffort, Role, ToolResultBlock, ToolResultContent, Usage,
};
use crate::output_style::OutputStyle;
use crate::permissions::{
    Decision, ExecutionMode, PermissionPolicy, PermissionRule, PermissionScope,
    SessionPermissionDecision,
};
pub use session_permissions::SessionPermissionDenial;
use session_permissions::{PermissionOwner, SessionPermissionOperation, WorkspaceScope};

const EVENT_REPLAY_LIMIT: usize = 1024;
const MAX_OUTPUT_LIMIT_CONTINUATIONS: usize = 3;
const OUTPUT_LIMIT_CONTINUATION_PROMPT: &str = "Output token limit hit. Continue directly from the unfinished response without repeating or summarizing it. Break the remaining work into smaller pieces.";
const SHELL_STATE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

pub type ChatId = Uuid;
pub type LoopId = Uuid;
pub type RunId = Uuid;
pub use agent_messages::{
    AgentMessageSource, FamilyMessageRejection, FamilyMessageRejectionCode,
    LIST_SUBAGENTS_TOOL_NAME, SEND_AGENT_MESSAGE_TOOL_NAME, StoppedChildHook, VerifiedUserRelay,
};
pub use delegation::{
    DelegationCeiling, DelegationMetadata, MAX_SUBAGENT_HANDLE_CHARACTERS, SUBAGENT_TOOL_NAME,
    SubagentRegistration,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum LoopState {
    Idle,
    Queued,
    WaitingPermission,
    WaitingQuestion,
    WaitingPlan,
    WaitingTrigger,
    Expired,
    Running,
    Completed,
    Failed,
    Interrupted,
    Stopped,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoopSnapshot {
    pub id: LoopId,
    pub goal: String,
    pub state: LoopState,
    pub run_id: Option<RunId>,
    pub run_started_at: Option<u64>,
    #[serde(default)]
    pub trigger: Option<Trigger>,
    /// Unix milliseconds when the runtime moved this Loop into its current state. A state set by
    /// restoring, and records written before this field, leave it unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_sources: Vec<EventSourceBinding>,
}

/// An artifact linked to a Loop for one account's @tickloop feedback requests. Records saved in
/// earlier formats do not parse and are dropped when the Chat loads.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventSourceBinding {
    pub origin: String,
    pub account_id: String,
    pub artifact_id: String,
    pub binding_id: Uuid,
    pub client_instance_id: Uuid,
    pub epoch: u64,
    /// Listening waits for an explicit Continue, which links the artifact again.
    pub resume_required: bool,
    /// Unix milliseconds of the newest feedback request delivered through this link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_at_ms: Option<u64>,
}

impl LoopSnapshot {
    /// Moves the Loop to `state`, recording the moment only when the state actually changes.
    pub(crate) fn set_state(&mut self, state: LoopState) {
        if self.state != state {
            self.state = state;
            self.state_since = Some(crate::private_fs::unix_time_milliseconds());
        }
    }

    /// A state the Loop is found in when it is restored, not one it entered now: when the state
    /// changes, nobody knows when that happened, so its time becomes unknown.
    pub(crate) fn recover_state(&mut self, state: LoopState) {
        if self.state != state {
            self.state = state;
            self.state_since = None;
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatSnapshot {
    #[serde(default)]
    pub failure_events: Vec<Event>,
    #[serde(default)]
    pub provider_failures: HashMap<String, crate::model::ProviderHttpFailure>,
    #[serde(skip)]
    pub tool_call_recoveries: Vec<ToolCallRecovery>,
    #[serde(default)]
    pub artifact_ticks: Vec<crate::artifacts::ArtifactTick>,
    #[serde(skip)]
    pub removed_artifact_ticks: std::collections::BTreeSet<String>,
    #[serde(default, skip_serializing_if = "MonitorMembership::is_empty")]
    pub monitor: MonitorMembership,
    /// 本进程内的请求用量账本（不持久化，照 Claude Code 用量只记在 assistant 消息上）。
    #[serde(skip)]
    pub request_usage: RequestUsageLedger,
    pub id: ChatId,
    #[serde(default)]
    pub branch_origin: Option<BranchOrigin>,
    #[serde(default)]
    pub delegation: Option<DelegationMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub family_completions: Vec<FamilyCompletion>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub family_inputs: BTreeMap<Uuid, FamilyInputRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sent_agent_messages: BTreeMap<Uuid, AgentMessageSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_title: Option<String>,
    pub workdir: PathBuf,
    /// Chat 当前使用的模型，只在本进程内有效；恢复时使用宿主当前选择的模型。
    #[serde(skip, default = "unselected_model")]
    pub model: ModelConfig,
    #[serde(skip)]
    pub reasoning_effort: Option<crate::model::ReasoningEffort>,
    #[serde(skip, default = "thinking_enabled_by_default")]
    pub thinking_enabled: bool,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invoked_skills: Vec<crate::skills::InvokedSkill>,
    #[serde(default)]
    pub message_runs: BTreeMap<usize, RunId>,
    #[serde(default)]
    pub message_contexts: BTreeMap<usize, Vec<InputContext>>,
    #[serde(skip)]
    denial_references: BTreeMap<usize, denials::DenialReference>,
    /// 消息在 Transcript 中的 uuid，文件回滚快照以此关联消息。用户提示与模型回复写入对话时
    /// 确定，恢复时只重建用户提示的；其余消息的 uuid 在写入 Transcript 时生成。
    #[serde(default)]
    pub message_ids: BTreeMap<usize, Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_message_origins: BTreeMap<Uuid, serde_json::Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub human_family_origins: BTreeMap<Uuid, DirectedHumanSource>,
    /// Stable source IDs for genuine prompts rewritten across directed summaries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub original_user_message_ids: BTreeMap<usize, Uuid>,
    /// Execution modes captured for user prompts that can serve as rewind points.
    #[serde(default)]
    pub prompt_execution_modes: BTreeMap<usize, ExecutionMode>,
    /// Copied prompts in a Branch remain rewind points, but are not fresh child-Chat authorization.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub inherited_prompts: HashSet<usize>,
    /// 压缩边界与压缩摘要消息（链上字段）。
    #[serde(default)]
    pub message_compactions: BTreeMap<usize, CompactionMarker>,
    /// assistant 消息的用量（链上字段）。定向摘要保留的消息在恢复时照 Claude Code 清零。
    #[serde(default)]
    pub message_usage: BTreeMap<usize, Usage>,
    #[serde(default)]
    pub message_sources: BTreeMap<usize, crate::model::PrivateHistorySource>,
    #[serde(default)]
    pub message_models: BTreeMap<usize, crate::model::ModelIdentity>,
    /// 链上字段：由对话链上的 Loop 条目重放得到。
    #[serde(default)]
    pub loops: Vec<LoopSnapshot>,
    /// 各 Loop 创建时的对话消息数（链上字段）。rewind 截断到这个位置或更早时 Loop 不再存在。
    #[serde(skip)]
    pub(crate) loop_starts: BTreeMap<LoopId, usize>,
    /// Trigger 已解除武装、等待用户向该 Loop 提交输入时重新武装的 Loop（重启恢复或 Branch
    /// 复制后）。随 Loop 条目记录在对话链上。
    #[serde(skip)]
    pub resume_triggers: Vec<LoopId>,
    /// The fields through `pending_model` are process-local state and are not persisted with the
    /// Chat. Recovery starts them from defaults; persistent rules and context limits come from
    /// user settings.
    #[serde(skip)]
    pub last_sequence: u64,
    #[serde(skip)]
    pub permissions: Vec<PermissionRequest>,
    #[serde(skip)]
    pub questions: Vec<QuestionRequest>,
    #[serde(skip)]
    pub pending_inputs: Vec<PendingInput>,
    #[serde(skip)]
    pub pending_commands: Vec<PendingCommand>,
    #[serde(skip)]
    pub last_queue_sequence: u64,
    #[serde(skip)]
    pub operations: Vec<OperationRecord>,
    #[serde(skip)]
    pub policy: PermissionPolicy,
    /// 规则集中每条持久规则所属的项目分区（规范化的工作目录），删除或修改时写回该分区。
    #[serde(skip)]
    pub(crate) persistent_rule_projects: BTreeMap<Uuid, PathBuf>,
    #[serde(skip)]
    pub additional_directories: Vec<PathBuf>,
    #[serde(skip)]
    pub events: Vec<Event>,
    #[serde(skip)]
    pub pending_thinking: BTreeMap<RunId, String>,
    #[serde(skip)]
    pub context_window_tokens: Option<usize>,
    #[serde(skip)]
    pub pending_model: Option<ModelConfig>,
    /// 各 Loop 的计划与进度。计划内容写在 `<root>/plans/<slug>-loop-<loopId>.md`，版本与进度
    /// 恢复时由对话链重建；批准时选择的模式只在本进程内有效。
    #[serde(default)]
    pub plans: Vec<Plan>,
    #[serde(default)]
    pub progress: Vec<Progress>,
    /// 照 Claude Code：Chat 的计划文件名前缀，第一次设置计划时生成，记录在之后的每个链上条目中。
    #[serde(skip)]
    pub(crate) plan_slug: Option<String>,
    /// 计划内容不可用（恢复时计划文件缺失或不可读、`--fork-session`／Branch 复制失败）的 Loop 已使用过的
    /// 最高计划版本；该 Loop 之后的计划从更大的版本继续，版本号不会被复用。
    #[serde(skip)]
    pub(crate) unavailable_plan_versions: BTreeMap<LoopId, u64>,
    /// 本 Chat 中各 run 所属的 Loop，写入链上条目的 `loopId`；恢复时由条目重建。
    #[serde(skip)]
    pub(crate) run_loops: BTreeMap<RunId, LoopId>,
    /// 本进程内的 Agent 命令状态，不随 Chat 保存，重启后不可见。
    #[serde(skip)]
    pub commands: Vec<crate::tools::CommandSnapshot>,
    /// 本进程内的 `!` 命令状态，不随 Chat 保存，重启后不可见。
    #[serde(skip)]
    pub user_shells: Vec<UserShellRecord>,
    /// 本进程内最后一次模型回复所在的 run，只用于显示流式回复；不随 Chat 保存。
    #[serde(skip)]
    pub last_model_run: Option<RunId>,
}

fn thinking_enabled_by_default() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum UserShellState {
    Queued,
    WaitingPermission,
    Running,
    Background,
    Cancelling,
    Completed,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserShellRecord {
    pub id: Uuid,
    pub chat_id: ChatId,
    #[serde(default)]
    pub loop_id: Option<LoopId>,
    pub command: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub starts_in_background: bool,
    #[serde(default)]
    pub queued_sequence: u64,
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub preceding_model_runs: Vec<RunId>,
    pub state: UserShellState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    pub output_ref: Option<PathBuf>,
    pub output_bytes: u64,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    /// 提交时的对话消息数，只用于 Task Center 排序。
    pub submitted_at_message: usize,
    /// Unix milliseconds when the Shell entered its current state; output growth does not move it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<u64>,
}

impl UserShellRecord {
    /// Moves the Shell to `state`, recording the moment only when the state actually changes.
    pub(crate) fn set_state(&mut self, state: UserShellState) {
        if self.state != state {
            self.state = state;
            self.state_since = Some(crate::private_fs::unix_time_milliseconds());
            self.updated_at_ms = self.state_since;
        }
    }

    /// Whether the Shell still waits for admission: for approval, or accepted but without a
    /// started process.
    pub(crate) fn awaits_admission(&self) -> bool {
        self.state == UserShellState::WaitingPermission
            || self.state == UserShellState::Running && self.pid.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserShellReceipt {
    pub chat_id: ChatId,
    pub command_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum PermissionExecution {
    ToolCall { run_id: RunId, call_id: String },
    UserShell { command_id: Uuid },
}

impl PermissionExecution {
    pub fn run_id(&self) -> Option<RunId> {
        match self {
            Self::ToolCall { run_id, .. } => Some(*run_id),
            Self::UserShell { .. } => None,
        }
    }
    pub fn call_key(&self) -> String {
        match self {
            Self::ToolCall { call_id, .. } => call_id.clone(),
            Self::UserShell { command_id } => format!("user-shell:{command_id}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PermissionRequest {
    pub id: Uuid,
    pub loop_id: LoopId,
    pub execution: PermissionExecution,
    pub name: String,
    pub input: serde_json::Value,
    #[serde(default)]
    pub file_change: Option<Box<crate::file_change::PreparedFileChange>>,
    #[serde(default)]
    pub approval_reason: Option<String>,
    #[serde(default)]
    pub approval_source: PermissionApprovalSource,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionApprovalSource {
    #[default]
    PermissionPolicy,
    ExplicitAsk,
    SafetyCheck,
    AutoReviewer,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum OperationState {
    Proposed,
    Admitted,
    Completed,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OperationRecord {
    pub id: Uuid,
    pub loop_id: LoopId,
    pub run_id: RunId,
    pub call_id: String,
    pub name: String,
    pub input: serde_json::Value,
    #[serde(default)]
    pub file_change: Option<crate::file_change::PreparedFileChange>,
    pub state: OperationState,
    pub result: Option<crate::tools::ToolResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCallRecovery {
    pub run_id: Option<RunId>,
    pub call_id: String,
    pub was_admitted: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub sequence: u64,
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub run_id: RunId,
    pub kind: EventKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum AutoReviewerNotice {
    Fallback { failure: ApprovalFailureKind },
    Recovered,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProposedToolCall {
    pub call_id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum EventKind {
    FamilyInputUpdated {
        record: FamilyInputRecord,
    },
    FamilyRunCompleted {
        completion: FamilyCompletion,
    },
    Started,
    Text(String),
    Thinking(String),
    AssistantMessageCommitted {
        message_id: Uuid,
        tool_calls: Vec<ProposedToolCall>,
    },
    Completed {
        usage: Usage,
    },
    Failed(String),
    ModelRetry {
        kind: RequestKind,
        error: String,
        attempt: usize,
        max_retries: usize,
        delay_ms: u64,
    },
    Interrupted,
    InputMerged {
        into_run_id: RunId,
    },
    InputTakenBack,
    AgentMessageQueued {
        source: AgentMessageSource,
    },
    ConversationRewound {
        message_id: Uuid,
    },
    PermissionRequested {
        request: PermissionRequest,
    },
    QuestionAsked {
        request: QuestionRequest,
    },
    GoalEnded {
        reason: String,
        operations: Vec<OperationRecord>,
    },
    ToolStarted {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolCompleted {
        call_id: String,
        result: crate::tools::ToolResult,
    },
    PermissionChecked {
        decision: Decision,
        reason: String,
    },
    ApprovalAssessed {
        assessment: Box<ApprovalAssessmentDetails>,
    },
    AutoReviewerNotice {
        notice: AutoReviewerNotice,
    },
    ContextCompaction {
        status: CompactionStatus,
        error: Option<String>,
    },
    PlanUpdated {
        plan: Plan,
    },
    ProgressUpdated {
        progress: Progress,
    },
    CommandUpdated {
        command: crate::tools::CommandSnapshot,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub run_id: RunId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingInput {
    pub receipt: Receipt,
    pub input: String,
    #[serde(default)]
    pub contexts: Vec<InputContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message: Option<AgentMessageSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directed_human: Option<DirectedHumanSource>,
    #[serde(default)]
    denial_reference: Option<denials::DenialReference>,
    #[serde(default)]
    pub trigger_source: Option<TriggerSource>,
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub queued_sequence: u64,
    /// A queued continuation for a completed background task; its notification is already in the conversation.
    #[serde(default)]
    pub task_continuation: bool,
    #[serde(skip)]
    is_skill_invocation: bool,
    /// Files attached whole with the prompt; they count as read once the prompt enters the
    /// conversation. Not persisted, so a restored prompt grants no reads.
    #[serde(skip)]
    attached_file_reads: Vec<(PathBuf, crate::tools::FileReadRecord)>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueuedInputKind {
    Message,
    Command,
    Shell,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingCommand {
    pub id: Uuid,
    pub chat_id: ChatId,
    pub loop_id: Option<LoopId>,
    pub command: String,
    pub queued_sequence: u64,
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub dispatched: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct QueuedInput {
    pub id: Uuid,
    pub chat_id: ChatId,
    pub loop_id: Option<LoopId>,
    pub kind: QueuedInputKind,
    pub text: String,
    #[serde(default)]
    pub contexts: Vec<InputContext>,
    pub queued_sequence: u64,
    #[serde(default)]
    pub armed: bool,
}

impl ChatSnapshot {
    pub(super) fn run_response_text(&self, run: RunId) -> Option<String> {
        self.messages
            .iter()
            .enumerate()
            .rev()
            .find(|(index, message)| {
                message.role == Role::Assistant && self.message_runs.get(index) == Some(&run)
            })
            .map(|(_, message)| {
                message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
    }

    pub fn default_input_loop(&self) -> Option<LoopId> {
        (self.loops.len() == 1).then(|| self.loops[0].id)
    }

    pub(crate) fn effective_reasoning_effort(&self) -> Option<crate::model::ReasoningEffort> {
        if !self.thinking_enabled
            && crate::model::validate_reasoning_effort(
                &self.model,
                Some(crate::model::ReasoningEffort::Off),
            )
            .is_ok()
        {
            Some(crate::model::ReasoningEffort::Off)
        } else {
            self.reasoning_effort
        }
    }

    pub fn effective_context_window_tokens(&self) -> Option<usize> {
        context::context_window(self)
    }

    /// Whether the model that answers the next user input accepts images; a pending model
    /// switch takes effect before that input runs.
    pub fn accepts_images(&self) -> bool {
        self.pending_model
            .as_ref()
            .unwrap_or(&self.model)
            .accepts_images()
    }

    /// The highest `[Image #N]` pasted into this conversation, sent or queued, so a resumed
    /// editor numbers new pastes after it.
    pub fn highest_pasted_image_id(&self) -> Option<u32> {
        self.message_contexts
            .values()
            .chain(self.pending_inputs.iter().map(|pending| &pending.contexts))
            .flatten()
            .filter_map(|context| match context {
                InputContext::Image(ImageContext {
                    origin: ImageOrigin::Pasted { id },
                    ..
                }) => Some(*id),
                _ => None,
            })
            .max()
    }

    /// 新建 Loop，记下创建时的对话位置。
    pub(crate) fn add_loop(&mut self, goal: LoopSnapshot) {
        self.loop_starts.insert(goal.id, self.messages.len());
        self.loops.push(goal);
    }

    pub fn queued_inputs(&self) -> Vec<QueuedInput> {
        let mut entries = self
            .pending_inputs
            .iter()
            .filter(|pending| {
                pending.trigger_source.is_none()
                    && !pending.task_continuation
                    && pending.agent_message.is_none()
            })
            .map(|pending| QueuedInput {
                id: pending.receipt.run_id,
                chat_id: pending.receipt.chat_id,
                loop_id: Some(pending.receipt.loop_id),
                kind: QueuedInputKind::Message,
                text: pending.input.clone(),
                contexts: pending.contexts.clone(),
                queued_sequence: pending.queued_sequence,
                armed: pending.armed,
            })
            .chain(self.pending_commands.iter().map(|command| QueuedInput {
                id: command.id,
                chat_id: command.chat_id,
                loop_id: command.loop_id,
                kind: QueuedInputKind::Command,
                text: command.command.clone(),
                contexts: Vec::new(),
                queued_sequence: command.queued_sequence,
                armed: command.armed,
            }))
            .chain(
                self.user_shells
                    .iter()
                    .filter(|record| record.state == UserShellState::Queued)
                    .map(|record| QueuedInput {
                        id: record.id,
                        chat_id: record.chat_id,
                        loop_id: record.loop_id,
                        kind: QueuedInputKind::Shell,
                        text: record.command.clone(),
                        contexts: Vec::new(),
                        queued_sequence: record.queued_sequence,
                        armed: record.armed,
                    }),
            )
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.queued_sequence);
        entries
    }
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub snapshot: ChatSnapshot,
    pub events: Vec<Event>,
    pub reset_required: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] crate::model::ModelError),
    #[error("request usage ledger: {0}")]
    Usage(#[from] LedgerError),
    #[error("storage failure: {0}")]
    Storage(String),
    #[error("chat or goal does not exist")]
    NotFound,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DirectoryTrustPolicy {
    #[default]
    CallerManaged,
    RequireUserTrust,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PromptPolicy {
    #[default]
    Interactive,
    DenyOnPrompt,
    Unrestricted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentHostOptions {
    pub execution_mode_override: Option<ExecutionMode>,
    pub directory_trust: DirectoryTrustPolicy,
    pub prompt_policy: PromptPolicy,
}

impl Default for AgentHostOptions {
    fn default() -> Self {
        Self::product_defaults()
    }
}

impl AgentHostOptions {
    pub const fn product_defaults() -> Self {
        Self {
            execution_mode_override: None,
            directory_trust: DirectoryTrustPolicy::RequireUserTrust,
            prompt_policy: PromptPolicy::Interactive,
        }
    }

    pub const fn embedded_defaults() -> Self {
        Self {
            execution_mode_override: Some(ExecutionMode::AcceptEdits),
            directory_trust: DirectoryTrustPolicy::CallerManaged,
            prompt_policy: PromptPolicy::Interactive,
        }
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

    pub(crate) fn shutdown_token(&self) -> CancellationToken {
        self.inner.shutdown.clone()
    }
}

struct Inner {
    /// 本进程当前选择的模型；新建与恢复的 Chat 使用它，`set_model` 随选择更新。
    selected_model: std::sync::Mutex<chat_settings::SelectedModel>,
    /// 用户设置文件；不写设置的 Agent（测试等）为 `None`。
    settings: Option<crate::settings::UserSettings>,
    host_options: AgentHostOptions,
    user_skills_directory: Option<PathBuf>,
    /// 本进程内设置读写的次序：写入设置并同步已加载的 Chat、以及读取设置并加入新 Chat 都在
    /// 持有它时完成，保证已加载的 Chat 与设置文件一致。总在 `chats` 锁之前获取。
    settings_writes: Mutex<()>,
    /// 最近一次为各 Chat 读取用户设置时发现的问题。
    settings_warnings: std::sync::Mutex<HashMap<ChatId, Vec<String>>>,
    provider_failures: std::sync::Mutex<HashMap<String, crate::model::ProviderHttpFailure>>,
    trusted_working_directories: std::sync::Mutex<HashSet<PathBuf>>,
    host_contexts: std::sync::Mutex<host_context::HostContexts>,
    subagent_invocations: Mutex<HashMap<(RunId, String), delegation::SubagentInvocation>>,
    family_mailbox: Mutex<family_mailbox::FamilyMailbox>,
    input_wakeup: tokio::sync::watch::Sender<()>,
    title_requests: Mutex<HashSet<ChatId>>,
    subagent_registry: Mutex<HashMap<ChatId, Vec<SubagentRegistration>>>,
    chats: Mutex<HashMap<ChatId, Chat>>,
    events: broadcast::Sender<Event>,
    user_shell_events: broadcast::Sender<UserShellRecord>,
    user_shell_cancellations: std::sync::Mutex<HashMap<(ChatId, Uuid), CancellationToken>>,
    task_notifications: broadcast::Sender<TaskNotification>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    recorder: Option<persistence::ChatRecorder>,
    /// 本进程持有的 Trigger 租约；Inner 释放时随之归还。
    loop_leases: loop_lease::LoopLeases,
    approvals: Mutex<HashMap<Uuid, PendingApproval>>,
    question_answers: Mutex<HashMap<Uuid, oneshot::Sender<String>>>,
    closed: AtomicBool,
    auto_reviewer: Mutex<Option<Arc<dyn ModelClient>>>,
    judgment: Mutex<Option<Arc<dyn crate::judgment::JudgmentClient>>>,
    auto_health: Mutex<auto_health::AutoHealth>,
    host_tools: std::sync::RwLock<Option<Arc<dyn crate::host_tools::HostToolProvider>>>,
    hosted_ticks: std::sync::RwLock<Option<Arc<crate::hosted::tools::HostedTickTools>>>,
    stopped_child_hook: std::sync::RwLock<Option<Arc<dyn StoppedChildHook>>>,
    /// Whether a Workspace Tick Update stream is installed; the Tick Subscription tools reach the
    /// model only then.
    tick_stream_installed: AtomicBool,
    tick_deliveries: std::sync::Mutex<tick_subscriptions::TickDeliveries>,
    /// Wakes the delivery task when the subscription set or a cadence changed.
    tick_wake: Notify,
    session_permissions: Mutex<session_permissions::SessionPermissionContext>,
    shutdown: CancellationToken,
}

#[derive(Clone)]
struct Chat {
    family_generation: u64,
    snapshot: ChatSnapshot,
    todo_completion_graces: HashMap<LoopId, todo_lifecycle::CompletionGrace>,
    start_new_tool_result_message: bool,
    retention: Option<Arc<crate::transcript::retention::RetentionGuard>>,
    active: Option<CancellationToken>,
    active_run: Option<RunId>,
    parked: HashMap<RunId, ParkedRun>,
    ready_runs: VecDeque<RunId>,
    client: Arc<dyn ModelClient>,
    pending_client: Option<Arc<dyn ModelClient>>,
    pending_model_thinking: Option<PendingModelThinking>,
    /// Chat 的工具；其 Chat 目录持久化时为 Transcript 同级的 `<chatId>/`，不持久化时位于
    /// 系统临时目录的项目目录下，外存的工具结果与文件改动 review 写在这里。
    tools: Arc<crate::tools::Tools>,
    skills: crate::skills::SkillRegistry,
    /// 照 Claude Code，替换决定只在运行期保存，恢复时由 Transcript 中的内容替换条目重建。
    content_replacements: tool_results::ContentReplacementState,
    file_history: FileHistory,
    watching_commands: bool,
    watching_user_shells: bool,
    user_shell_queue: Arc<Notify>,
    trigger_cancellations: HashMap<LoopId, CancellationToken>,
    event_cancellations: HashMap<LoopId, CancellationToken>,
    active_event_claims: HashMap<RunId, ActiveEventClaim>,
    /// 等待写入对话的本地消息；Chat 正在执行、等待或运行前台 `!` 命令时暂存，空闲或到达工具边界时写入。
    deferred_messages: Vec<Message>,
    /// 由用户中断或停止而取消的 Run；只有这些 Run 保留部分回复并写中断标记。
    user_cancelled_runs: HashSet<RunId>,
    /// Loaded when the Chat opens in this process and unaffected by later settings changes.
    output_style: OutputStyle,
    /// Claude Code-style Auto classifier denial counts for this in-process Chat.
    auto_denials: auto_mode::DenialTracking,
    denial_window: denials::DenialWindow,
}

enum RunAdmission {
    Idle,
    Ready(Receipt, ModelRequest, CancellationToken),
    ValidateTick(Box<PendingInput>),
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

#[derive(Clone)]
struct ActiveEventClaim {
    loop_id: LoopId,
    source: EventSourceBinding,
    claim: crate::artifacts::ClaimedRequest,
    /// The service finished the request or already took the claim back; nothing to release.
    settled: bool,
}

#[derive(Default)]
struct SubmissionOptions {
    attachment_admission: Option<family::AttachmentAdmission>,
    family_generation: Option<u64>,
    contexts: Vec<InputContext>,
    attached_file_reads: Vec<(PathBuf, crate::tools::FileReadRecord)>,
    denial_reference: Option<denials::DenialReference>,
    trigger_source: Option<TriggerSource>,
    trigger_cancel: Option<CancellationToken>,
    event_claim: Option<ActiveEventClaim>,
    invoked_skill: Option<crate::skills::InvokedSkill>,
    host_context: Option<crate::host_tools::HostCallContext>,
    agent_message: Option<AgentMessageSource>,
    directed_human: Option<DirectedHumanSource>,
    goal_title: Option<String>,
}

#[derive(Clone, Copy)]
enum MessageBoundary {
    /// Chat 没有执行中或等待中的 Run，也没有整理上下文。
    Idle,
    /// 当前 Run 刚写完一轮工具结果，下一次模型请求尚未开始。
    ToolResults,
}

struct PermissionAnswer {
    allow: bool,
    revision: u64,
    comment: Option<String>,
}

struct HostAdmission<'a> {
    session_evaluation: &'a session_permissions::SessionPermissionEvaluation,
    policy_revision: u64,
    host_permission: Option<crate::host_tools::HostToolPermission>,
    approval_path: ApprovalPath,
    direct_basis: denials::DirectApprovalBasis,
    approval_source: Option<PermissionApprovalSource>,
    assessment: Option<&'a ApprovalAssessmentDetails>,
}

struct PendingApproval {
    sender: oneshot::Sender<PermissionAnswer>,
    owner: PermissionOwner,
    operation: SessionPermissionOperation,
}

#[derive(Debug)]
enum AutoReview {
    Allow,
    Ask {
        reason: String,
    },
    Block {
        reason: String,
        kind: AutoBlockKind,
        rule_id: Option<String>,
        evidence: auto_rules::VerdictEvidence,
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
    configured: Option<Decision>,
    host_permission: Option<crate::host_tools::HostToolPermission>,
    execution_mode: ExecutionMode,
) -> Decision {
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

impl Chat {
    /// 前台 `!` 命令运行或停止中时，Chat 不接受压缩，也不写入其他本地消息。
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
}

fn configured_workspace_for_chat(chat: &Chat) -> crate::tools::Workspace {
    crate::tools::Workspace {
        workdir: chat.snapshot.workdir.clone(),
        additional_directories: chat.snapshot.additional_directories.clone(),
    }
}

/// 从 Transcript 重建或反序列化快照时的占位模型；恢复流程随后换成宿主当前选择的模型。
fn unselected_model() -> ModelConfig {
    ModelConfig {
        provider: crate::model::ModelProvider::Api,
        auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
        name: String::new(),
        protocol: crate::model::Protocol::Messages,
        base_url: String::new(),
        model: String::new(),
        api_key_env: String::new(),
        max_output_tokens: 0,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![crate::model::InputModality::Text],
    }
}

/// 新 Chat 的初始快照。
/// 权限请求仍可回答：请求存在、其 Run 未取消且 Loop 正在等待权限；批准文件改动时 review 与请求一致。
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
                && chat.snapshot.loops.iter().any(|goal| {
                    goal.id == request.loop_id
                        && goal.run_id == Some(run_id)
                        && goal.state == LoopState::WaitingPermission
                })
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

fn empty_snapshot(id: ChatId, workdir: PathBuf, model: ModelConfig) -> ChatSnapshot {
    ChatSnapshot {
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
        family_inputs: BTreeMap::new(),
        sent_agent_messages: BTreeMap::new(),
        generated_title: None,
        workdir,
        model,
        reasoning_effort: None,
        thinking_enabled: true,
        messages: Vec::new(),
        invoked_skills: Vec::new(),
        message_runs: BTreeMap::new(),
        message_contexts: BTreeMap::new(),
        denial_references: BTreeMap::new(),
        message_ids: BTreeMap::new(),
        agent_message_origins: BTreeMap::new(),
        human_family_origins: BTreeMap::new(),
        original_user_message_ids: BTreeMap::new(),
        prompt_execution_modes: BTreeMap::new(),
        inherited_prompts: HashSet::new(),
        message_compactions: BTreeMap::new(),
        message_usage: BTreeMap::new(),
        message_sources: BTreeMap::new(),
        message_models: BTreeMap::new(),
        loops: Vec::new(),
        loop_starts: BTreeMap::new(),
        resume_triggers: Vec::new(),
        last_sequence: 0,
        permissions: Vec::new(),
        questions: Vec::new(),
        pending_inputs: Vec::new(),
        pending_commands: Vec::new(),
        last_queue_sequence: 0,
        operations: Vec::new(),
        policy: PermissionPolicy::default(),
        persistent_rule_projects: BTreeMap::new(),
        additional_directories: Vec::new(),
        events: Vec::new(),
        pending_thinking: BTreeMap::new(),
        context_window_tokens: None,
        pending_model: None,
        plans: Vec::new(),
        progress: Vec::new(),
        plan_slug: None,
        unavailable_plan_versions: BTreeMap::new(),
        run_loops: BTreeMap::new(),
        commands: Vec::new(),
        user_shells: Vec::new(),
        last_model_run: None,
    }
}

impl Drop for Inner {
    /// 最后一个 Agent 句柄释放时，同步写出尚未冲刷的 Transcript 条目。
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(recorder) = &self.recorder {
            recorder.flush_on_drop(self.chats.get_mut());
        }
    }
}

impl Agent {
    fn tool_definitions(&self, chat: &Chat) -> Vec<crate::model::ToolDefinition> {
        ToolRegistry::for_chat(
            chat,
            self.inner.host_tools.read().unwrap().clone(),
            self.inner.tick_stream_installed.load(Ordering::SeqCst),
        )
        .definitions()
    }

    /// Installs the worker's hosted Ticks, which its Chats' Monitors list and close directly.
    pub fn install_hosted_ticks(&self, tools: Arc<crate::hosted::tools::HostedTickTools>) {
        *self.inner.hosted_ticks.write().unwrap() = Some(tools);
    }

    pub fn hosted_ticks(&self) -> Option<Arc<crate::hosted::tools::HostedTickTools>> {
        self.inner.hosted_ticks.read().unwrap().clone()
    }

    pub fn install_host_tools(
        &self,
        provider: Arc<dyn crate::host_tools::HostToolProvider>,
    ) -> Result<(), AgentError> {
        let reserved = reserved_host_tool_names();
        let serves_artifacts = provider.artifact_client().is_some();
        let mut names = HashSet::new();
        for definition in provider.definitions() {
            let is_artifact_tool =
                crate::mcp_tools::ARTIFACT_TOOL_NAMES.contains(&definition.name.as_str());
            if definition.name.trim().is_empty()
                || (reserved.contains(&definition.name) && !(serves_artifacts && is_artifact_tool))
                || !names.insert(definition.name.clone())
            {
                return Err(AgentError::Invalid(format!(
                    "host tool name '{}' is empty, duplicated, or reserved",
                    definition.name
                )));
            }
        }
        *self.inner.host_tools.write().unwrap() = Some(provider);
        Ok(())
    }

    async fn skill_tool(
        &self,
        receipt: &Receipt,
        input: &serde_json::Value,
    ) -> Result<crate::tools::ToolResult, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SkillInput {
            skill: String,
            #[serde(default)]
            args: String,
        }

        let input = serde_json::from_value::<SkillInput>(input.clone()).map_err(|error| {
            format!("invalid {} input: {error}", crate::skills::SKILL_TOOL_NAME)
        })?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or_else(|| "chat is unavailable".to_string())?;
        let invoked = chat
            .skills
            .expand(&input.skill, &input.args, crate::skills::SkillCaller::Model)
            .and_then(crate::skills::InvokedSkill::try_from)
            .map_err(|error| error.to_string())?;
        let name = invoked.name.clone();
        record_invoked_skill(&mut chat.snapshot, invoked);
        self.save(&chat.snapshot);
        Ok(crate::tools::ToolResult {
            content: format!("Launching skill: {name}"),
            is_error: false,
            output_ref: None,
        })
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
        cancel: CancellationToken,
    ) -> Result<approval_assessment::ApprovalAssessment, AgentError> {
        let (snapshot, chat_client, boundary_id, legacy_path) = {
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
            let legacy_path = boundary_id
                .and_then(|_| self.inner.recorder.as_ref()?.path_for_chat(receipt.chat_id));
            (
                chat.snapshot.clone(),
                chat.client.clone(),
                boundary_id,
                legacy_path,
            )
        };
        let chat_id = receipt.chat_id;
        let legacy = match (boundary_id, legacy_path) {
            (Some(boundary_id), Some(path)) => tokio::task::spawn_blocking(move || {
                crate::transcript::load_original_user_inputs(&path, boundary_id, chat_id)
                    .ok()
                    .flatten()
            })
            .await
            .ok()
            .flatten(),
            _ => None,
        };
        let (auto_settings, auto_revision) = match self.auto_settings_snapshot().await {
            Ok(settings) => settings,
            Err(error) => {
                let reason = format!(
                    "Auto reviewer settings are unavailable ({error}); review this action manually."
                );
                self.emit_permission_check(
                    receipt,
                    self.apply_prompt_policy(Decision::Ask),
                    reason.clone(),
                )
                .await;
                return Ok(approval_assessment::ApprovalAssessment::missing_context(
                    operation,
                    snapshot.policy.revision,
                    reason,
                ));
            }
        };
        let context = approval_context::capture(
            &snapshot,
            operation,
            legacy,
            auto_rules::AutoPolicy::from_settings(&auto_settings),
        );
        let context = match context {
            Ok(context) => context,
            Err(approval_context::ApprovalContextError::MissingOriginalUserInput) => {
                let reason = "Original user authorization for this Loop is unavailable; review this action manually.".to_owned();
                self.emit_permission_check(
                    receipt,
                    self.apply_prompt_policy(Decision::Ask),
                    reason.clone(),
                )
                .await;
                return Ok(approval_assessment::ApprovalAssessment::missing_context(
                    operation,
                    snapshot.policy.revision,
                    reason,
                ));
            }
        };
        let preferred = auto_settings.reviewer;
        let configured_reviewer = self.inner.auto_reviewer.lock().await.clone();
        let has_isolated_reviewer = configured_reviewer.is_some();
        let has_judgment_adapter = self.inner.judgment.lock().await.is_some();
        let llm_client = configured_reviewer.unwrap_or(chat_client);
        let use_jev = preferred == AutoReviewerPreference::Jev
            || (has_judgment_adapter && !has_isolated_reviewer);
        let assess = |run: auto_approval::ReviewerRun,
                      fallback_from: Option<ApprovalReviewerKind>,
                      fallback_failure: Option<ApprovalFailureKind>,
                      fallback_reason: Option<ApprovalFallbackReason>| {
            approval_assessment::ApprovalAssessment::new(&context, run.review, run.reviewer)
                .with_auto_settings(preferred, &auto_revision)
                .with_attempts(run.attempts, fallback_from, fallback_failure)
                .with_fallback_reason(fallback_reason)
        };
        if !use_jev {
            let run = self
                .review_with_llm(receipt, &context, llm_client, cancel)
                .await?;
            self.emit_auto_review_check(receipt, &run.review).await;
            return Ok(assess(run, None, None, None));
        }

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
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
                self.review_with_jev(receipt, &context, client, deadline, cancel.clone())
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
                let first_fallback = self
                    .inner
                    .auto_health
                    .lock()
                    .await
                    .failure(permit, tokio::time::Instant::now());
                if first_fallback {
                    self.emit_auto_reviewer_notice(
                        receipt,
                        AutoReviewerNotice::Fallback { failure },
                    )
                    .await;
                }
                jev_attempts = run.attempts;
            } else {
                let recovered = self.inner.auto_health.lock().await.success(permit);
                if recovered {
                    self.emit_auto_reviewer_notice(receipt, AutoReviewerNotice::Recovered)
                        .await;
                }
                self.emit_auto_review_check(receipt, &run.review).await;
                return Ok(assess(run, None, None, None));
            }
        }
        if cancel.is_cancelled() {
            return Err(ModelError::Cancelled.into());
        }
        let mut run = self
            .review_with_llm(receipt, &context, llm_client, cancel)
            .await?;
        jev_attempts.append(&mut run.attempts);
        run.attempts = jev_attempts;
        self.emit_auto_review_check(receipt, &run.review).await;
        Ok(assess(
            run,
            Some(ApprovalReviewerKind::Jev),
            fallback_failure,
            fallback_reason,
        ))
    }

    async fn emit_auto_review_check(&self, receipt: &Receipt, review: &AutoReview) {
        let (decision, reason) = match review {
            AutoReview::Allow => (Decision::Allow, "Auto policy allowed the operation".into()),
            AutoReview::Block { reason, .. } => (Decision::Deny, reason.clone()),
            AutoReview::Ask { reason } => (self.apply_prompt_policy(Decision::Ask), reason.clone()),
            AutoReview::ContextLimit => (
                self.apply_prompt_policy(Decision::Ask),
                auto_mode::REVIEW_CONTEXT_LIMIT_MESSAGE.into(),
            ),
            AutoReview::Unavailable { message } => {
                (self.apply_prompt_policy(Decision::Ask), message.clone())
            }
        };
        self.emit_permission_check(receipt, decision, reason).await;
    }

    async fn emit_auto_reviewer_notice(&self, receipt: &Receipt, notice: AutoReviewerNotice) {
        let mut chats = self.inner.chats.lock().await;
        if let Some(chat) = chats.get_mut(&receipt.chat_id) {
            self.emit(chat, receipt, EventKind::AutoReviewerNotice { notice });
        }
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

    pub async fn stop(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        self.end_goal(chat_id, loop_id, LoopState::Stopped, None)
            .await
    }

    pub async fn interrupt(&self, chat_id: ChatId) -> Result<Vec<OperationRecord>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let mut interrupted_loop = None;
        if let Some(cancel) = &chat.active {
            cancel.cancel();
            if let Some(run_id) = chat.active_run {
                chat.user_cancelled_runs.insert(run_id);
                interrupted_loop = chat.snapshot.run_loops.get(&run_id).copied();
            }
        } else if let Some(waiting) = chat.parked.values().max_by_key(|run| run.sequence) {
            waiting.cancel.cancel();
            chat.user_cancelled_runs.insert(waiting.receipt.run_id);
            interrupted_loop = Some(waiting.receipt.loop_id);
        }
        for pending in &mut chat.snapshot.pending_inputs {
            if pending.agent_message.is_some() && Some(pending.receipt.loop_id) == interrupted_loop
            {
                pending.armed = false;
            }
        }
        Ok(chat
            .snapshot
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.state,
                    OperationState::Admitted | OperationState::Unknown
                )
            })
            .cloned()
            .collect())
    }

    /// 打开以 `root` 为根目录的持久化 Agent。打开时不加载任何 Chat；
    /// 用 [`Agent::resume_chat`] 按 id 恢复，用 [`Agent::saved_chats`] 列出已保存的 Chat。
    ///
    /// 照 Claude Code `shouldSkipPersistence`：用户设置的清理天数为 0 时不保存 Chat，与
    /// [`Agent::new_with_user_settings`] 相同，仍读写用户设置。设置只在打开时读取一次。
    pub fn open(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
    ) -> Result<Self, AgentError> {
        Self::open_with_options(root, model, client, AgentHostOptions::default())
    }

    pub fn open_embedded(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
    ) -> Result<Self, AgentError> {
        Self::open_with_options(root, model, client, AgentHostOptions::embedded_defaults())
    }

    pub fn open_with_options(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        host_options: AgentHostOptions,
    ) -> Result<Self, AgentError> {
        Self::open_with_settings_options(root, model, client, None, host_options)
    }

    pub fn open_with_user_settings_and_options(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        settings: crate::settings::UserSettings,
        host_options: AgentHostOptions,
    ) -> Result<Self, AgentError> {
        Self::open_with_settings_options(root, model, client, Some(settings), host_options)
    }

    fn open_with_settings_options(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        settings: Option<crate::settings::UserSettings>,
        host_options: AgentHostOptions,
    ) -> Result<Self, AgentError> {
        crate::private_fs::create_private_directory(&root)
            .map_err(|error| AgentError::Storage(error.to_string()))?;
        let root = root
            .canonicalize()
            .map_err(|error| AgentError::Storage(error.to_string()))?;
        if !root.is_dir() {
            return Err(AgentError::Storage(
                "session root is not a directory".into(),
            ));
        }
        let settings = settings.unwrap_or_else(|| crate::settings::UserSettings::in_root(&root));
        let recorder =
            (!settings.skips_transcripts()).then(|| persistence::ChatRecorder::new(root));
        Ok(Self::build(
            model,
            client,
            recorder,
            Some(settings),
            host_options,
        ))
    }

    /// 是否把 Chat 保存为 Transcript：不持久化的 Agent、以及清理天数为 0 时为 `false`。
    pub fn saves_chats(&self) -> bool {
        self.inner.recorder.is_some()
    }

    /// 本进程已加载（创建或恢复）的 Chat。
    pub async fn list_chats(&self) -> Vec<ChatSnapshot> {
        self.inner
            .chats
            .lock()
            .await
            .values()
            .map(|chat| chat.snapshot.clone())
            .collect()
    }

    /// 不持久化、也不读写用户设置的 Agent：Chat 只存在于本进程内存中。
    pub fn new(model: ModelConfig, client: Arc<dyn ModelClient>) -> Self {
        Self::build(model, client, None, None, AgentHostOptions::default())
    }

    pub fn new_embedded(model: ModelConfig, client: Arc<dyn ModelClient>) -> Self {
        Self::build(
            model,
            client,
            None,
            None,
            AgentHostOptions::embedded_defaults(),
        )
    }

    pub fn new_with_options(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        host_options: AgentHostOptions,
    ) -> Self {
        Self::build(model, client, None, None, host_options)
    }

    /// 不保存 Chat、但照常读写用户设置的 Agent（`--no-session-persistence`）。
    pub fn new_with_user_settings(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        settings: crate::settings::UserSettings,
    ) -> Self {
        Self::build(
            model,
            client,
            None,
            Some(settings),
            AgentHostOptions::default(),
        )
    }

    pub fn new_with_user_settings_embedded(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        settings: crate::settings::UserSettings,
    ) -> Self {
        Self::build(
            model,
            client,
            None,
            Some(settings),
            AgentHostOptions::embedded_defaults(),
        )
    }

    pub fn new_with_user_settings_and_options(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        settings: crate::settings::UserSettings,
        host_options: AgentHostOptions,
    ) -> Self {
        Self::build(model, client, None, Some(settings), host_options)
    }

    fn build(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        recorder: Option<persistence::ChatRecorder>,
        settings: Option<crate::settings::UserSettings>,
        host_options: AgentHostOptions,
    ) -> Self {
        let model = crate::model::cards::model_for_persistent_selection(model);
        let user_skills_directory = settings
            .as_ref()
            .and_then(crate::settings::UserSettings::root)
            .map(|root| root.join("skills"));
        Self {
            inner: Arc::new(Inner {
                selected_model: std::sync::Mutex::new(chat_settings::SelectedModel {
                    config: model,
                    client,
                }),
                settings,
                host_options,
                user_skills_directory,
                settings_writes: Mutex::new(()),
                settings_warnings: std::sync::Mutex::new(HashMap::new()),
                provider_failures: std::sync::Mutex::new(HashMap::new()),
                trusted_working_directories: std::sync::Mutex::new(HashSet::new()),
                host_contexts: std::sync::Mutex::new(host_context::HostContexts::default()),
                subagent_invocations: Mutex::new(HashMap::new()),
                family_mailbox: Mutex::new(family_mailbox::FamilyMailbox::default()),
                input_wakeup: tokio::sync::watch::channel(()).0,
                title_requests: Mutex::new(HashSet::new()),
                subagent_registry: Mutex::new(HashMap::new()),
                chats: Mutex::new(HashMap::new()),
                events: broadcast::channel(256).0,
                user_shell_events: broadcast::channel(256).0,
                user_shell_cancellations: std::sync::Mutex::new(HashMap::new()),
                task_notifications: broadcast::channel(256).0,
                tasks: Mutex::new(Vec::new()),
                recorder,
                loop_leases: loop_lease::LoopLeases::default(),
                approvals: Mutex::new(HashMap::new()),
                question_answers: Mutex::new(HashMap::new()),
                closed: AtomicBool::new(false),
                auto_reviewer: Mutex::new(None),
                judgment: Mutex::new(None),
                auto_health: Mutex::new(auto_health::AutoHealth::default()),
                host_tools: std::sync::RwLock::new(None),
                hosted_ticks: std::sync::RwLock::new(None),
                stopped_child_hook: std::sync::RwLock::new(None),
                tick_stream_installed: AtomicBool::new(false),
                tick_deliveries: std::sync::Mutex::new(
                    tick_subscriptions::TickDeliveries::default(),
                ),
                tick_wake: Notify::new(),
                session_permissions: Mutex::new(
                    session_permissions::SessionPermissionContext::default(),
                ),
                shutdown: CancellationToken::new(),
            }),
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
            .is_some_and(|settings| settings.is_working_directory_trusted(workdir))
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

    /// `apply_prompt_policy`, except that an Ask the user explicitly required still asks under
    /// an Unrestricted host.
    fn apply_prompt_policy_with_explicit_ask(
        &self,
        decision: Decision,
        is_explicit_ask: bool,
    ) -> Decision {
        if is_explicit_ask
            && decision == Decision::Ask
            && self.inner.host_options.prompt_policy == PromptPolicy::Unrestricted
        {
            Decision::Ask
        } else {
            self.apply_prompt_policy(decision)
        }
    }

    pub async fn trust_working_directory(&self, workdir: PathBuf) -> Result<(), AgentError> {
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
        tokio::task::spawn_blocking(move || settings.trust_working_directory(&workdir))
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    pub async fn create_chat(&self, workdir: PathBuf) -> Result<ChatId, AgentError> {
        self.create_chat_with_id(Uuid::new_v4(), workdir).await
    }

    pub async fn restore_empty_chat(
        &self,
        chat_id: ChatId,
        workdir: PathBuf,
    ) -> Result<(), AgentError> {
        if self.inner.chats.lock().await.contains_key(&chat_id) {
            return Ok(());
        }
        self.create_chat_with_id(chat_id, workdir).await?;
        Ok(())
    }

    async fn create_chat_with_id(
        &self,
        id: ChatId,
        workdir: PathBuf,
    ) -> Result<ChatId, AgentError> {
        self.create_chat_with_delegation(id, workdir, None).await
    }

    async fn create_chat_with_delegation(
        &self,
        id: ChatId,
        workdir: PathBuf,
        inherited: Option<(
            DelegationMetadata,
            ChatSnapshot,
            Arc<dyn ModelClient>,
            OutputStyle,
        )>,
    ) -> Result<ChatId, AgentError> {
        let workdir = workdir
            .canonicalize()
            .map_err(|_| AgentError::Invalid("working directory does not exist".into()))?;
        if !workdir.is_dir() {
            return Err(AgentError::Invalid(
                "working directory must be a directory".into(),
            ));
        }
        self.require_working_directory_trust(&workdir)?;
        let retention = self.retain_chat(id)?;
        if let Some(recorder) = &self.inner.recorder {
            recorder.register_chat(id, &workdir);
        }
        let mut snapshot = empty_snapshot(id, workdir, unselected_model());
        let _settings_gate = self.inner.settings_writes.lock().await;
        let (client, output_style) =
            if let Some((metadata, parent, client, output_style)) = inherited {
                snapshot.model = parent.model;
                snapshot.reasoning_effort = parent.reasoning_effort;
                snapshot.thinking_enabled = parent.thinking_enabled;
                snapshot.additional_directories = metadata.ceiling.additional_directories.clone();
                snapshot.policy = metadata.ceiling.policy.clone();
                snapshot.delegation = Some(metadata);
                (client, output_style)
            } else {
                self.apply_chat_defaults(&mut snapshot)
            };
        self.inner
            .subagent_registry
            .lock()
            .await
            .entry(id)
            .or_default();
        let tools = self.create_chat_tools(&snapshot);
        let skills = crate::skills::SkillRegistry::discover(
            &snapshot.workdir,
            self.inner.user_skills_directory.as_deref(),
        );
        let file_history = FileHistory::new(self.file_history_directory(id, &snapshot.workdir));
        let mut chats = self.inner.chats.lock().await;
        if let Some(metadata) = &mut snapshot.delegation {
            let sessions = self.inner.session_permissions.lock().await;
            let parent = PermissionOwner::new(metadata.parent_chat_id, metadata.parent_loop_id)
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
            if let Some(denials) = &mut metadata.ceiling.session_denials {
                for denial in sessions.freeze_denials(parent) {
                    if !denials.contains(&denial) {
                        denials.push(denial);
                    }
                }
            }
            self.record_delegation(id, metadata.clone());
        }
        self.save(&snapshot);
        chats.insert(
            id,
            Chat {
                family_generation: 0,
                snapshot,
                todo_completion_graces: HashMap::new(),
                start_new_tool_result_message: false,
                retention,
                active: None,
                active_run: None,
                parked: HashMap::new(),
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
                trigger_cancellations: HashMap::new(),
                event_cancellations: HashMap::new(),
                active_event_claims: HashMap::new(),
                deferred_messages: Vec::new(),
                user_cancelled_runs: HashSet::new(),
                output_style,
                auto_denials: auto_mode::DenialTracking::default(),
                denial_window: denials::DenialWindow::default(),
            },
        );
        Ok(id)
    }

    pub async fn submit(
        &self,
        chat_id: ChatId,
        goal: Option<LoopId>,
        input: String,
    ) -> Result<Receipt, AgentError> {
        self.submit_internal(chat_id, goal, input, SubmissionOptions::default())
            .await
    }

    pub async fn invoke_skill(
        &self,
        chat_id: ChatId,
        goal: Option<LoopId>,
        name: &str,
        arguments: &str,
    ) -> Result<Receipt, AgentError> {
        self.invoke_skill_in_host_context(chat_id, goal, name, arguments, None)
            .await
    }

    pub async fn invoke_skill_in_host_context(
        &self,
        chat_id: ChatId,
        goal: Option<LoopId>,
        name: &str,
        arguments: &str,
        host_context: Option<crate::host_tools::HostCallContext>,
    ) -> Result<Receipt, AgentError> {
        let invoked = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat.skills
                .expand(name, arguments, crate::skills::SkillCaller::User)
                .and_then(crate::skills::InvokedSkill::try_from)
                .map_err(|error| AgentError::Invalid(error.to_string()))?
        };
        let input = if arguments.trim().is_empty() {
            format!("/{}", invoked.name)
        } else {
            format!("/{} {}", invoked.name, arguments.trim())
        };
        self.submit_internal(
            chat_id,
            goal,
            input,
            SubmissionOptions {
                invoked_skill: Some(invoked),
                host_context,
                ..Default::default()
            },
        )
        .await
    }

    pub async fn skill_warnings(&self, chat_id: ChatId) -> Result<Vec<String>, AgentError> {
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

    /// Checks that the user may invoke `name` in a new Chat at `workdir`, before any Chat exists.
    pub fn check_user_skill(
        &self,
        workdir: &Path,
        name: &str,
        arguments: &str,
    ) -> Result<(), AgentError> {
        crate::skills::SkillRegistry::discover(workdir, self.inner.user_skills_directory.as_deref())
            .expand(name, arguments, crate::skills::SkillCaller::User)
            .and_then(crate::skills::InvokedSkill::try_from)
            .map(|_| ())
            .map_err(|error| AgentError::Invalid(error.to_string()))
    }

    pub async fn has_skill(&self, chat_id: ChatId, name: &str) -> Result<bool, AgentError> {
        Ok(self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .skills
            .contains(name))
    }

    pub async fn submit_with_file_references(
        &self,
        chat_id: ChatId,
        goal: Option<LoopId>,
        input: String,
        paths: Vec<PathBuf>,
    ) -> Result<Receipt, AgentError> {
        self.submit_with_input_contexts(chat_id, goal, input, Vec::new(), paths)
            .await
    }

    /// Submits a prompt with attachments already in hand, such as pasted images or those
    /// restored from a saved prompt, followed by the `@` references in `paths`.
    pub async fn submit_with_input_contexts(
        &self,
        chat_id: ChatId,
        goal: Option<LoopId>,
        input: String,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
    ) -> Result<Receipt, AgentError> {
        self.submit_with_input_contexts_in_host_context(chat_id, goal, input, contexts, paths, None)
            .await
    }

    pub async fn submit_with_input_contexts_in_host_context(
        &self,
        chat_id: ChatId,
        goal: Option<LoopId>,
        input: String,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
        host_context: Option<crate::host_tools::HostCallContext>,
    ) -> Result<Receipt, AgentError> {
        self.submit_contexts(family::ContextSubmission {
            chat_id,
            goal,
            input,
            contexts,
            paths,
            host_context,
            directed_source: None,
            target_generation: None,
        })
        .await
    }

    async fn submit_contexts(
        &self,
        submission: family::ContextSubmission,
    ) -> Result<Receipt, AgentError> {
        let family::ContextSubmission {
            chat_id,
            goal,
            input,
            mut contexts,
            paths,
            host_context,
            directed_source,
            target_generation,
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
            let snapshot = &chats.get(&chat_id).ok_or(AgentError::NotFound)?.snapshot;
            if goal.is_some_and(|id| !snapshot.loops.iter().any(|existing| existing.id == id)) {
                return Err(AgentError::NotFound);
            }
            let workspace = crate::tools::Workspace {
                workdir: snapshot.workdir.clone(),
                additional_directories: snapshot.additional_directories.clone(),
            };
            let reads = paths
                .iter()
                .map(PathBuf::as_path)
                .chain(contexts.iter().filter_map(InputContext::path))
                .map(|path| {
                    crate::tools::prepare(
                        &workspace,
                        crate::tools::READ_TOOL_NAME,
                        &serde_json::json!({"file_path":path}),
                    )
                    .map_err(|error| AgentError::Invalid(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let admission = family::AttachmentAdmission { workspace, reads };
            let sessions = self.inner.session_permissions.lock().await;
            Self::validate_attachment_admission(&chats, &sessions, chat_id, goal, &admission)?;
            admission
        };
        let workspace = admission.workspace.clone();
        let paths = admission
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
        let loaded = tokio::task::spawn_blocking(move || input_context::load(&workspace, paths))
            .await
            .map_err(|_| AgentError::Invalid("referenced paths could not be loaded".into()))?
            .map_err(AgentError::Invalid)?;
        for context in loaded.contexts {
            contexts.retain(|existing| existing.path() != context.path());
            contexts.push(context);
        }
        self.submit_internal(
            chat_id,
            goal,
            input,
            SubmissionOptions {
                contexts,
                attached_file_reads: loaded.file_reads,
                directed_human: directed_source,
                family_generation: target_generation,
                attachment_admission: Some(admission),
                host_context,
                ..Default::default()
            },
        )
        .await
    }

    async fn submit_internal(
        &self,
        chat_id: ChatId,
        goal: Option<LoopId>,
        input: String,
        options: SubmissionOptions,
    ) -> Result<Receipt, AgentError> {
        let SubmissionOptions {
            attachment_admission,
            family_generation,
            contexts,
            attached_file_reads,
            denial_reference,
            trigger_source,
            trigger_cancel,
            event_claim,
            invoked_skill,
            host_context,
            agent_message,
            directed_human,
            goal_title,
        } = options;
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(AgentError::Invalid("agent host has shut down".into()));
        }
        if input.trim().is_empty() {
            return Err(AgentError::Invalid("input cannot be empty".into()));
        }
        let mut chats = self.inner.chats.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(AgentError::Invalid("agent host has shut down".into()));
        }
        if let Some(admission) = &attachment_admission {
            let sessions = self.inner.session_permissions.lock().await;
            Self::validate_attachment_admission(&chats, &sessions, chat_id, goal, admission)?;
        }
        let original = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if family_generation.is_some_and(|expected| {
            expected != original.family_generation
                || original
                    .snapshot
                    .loops
                    .iter()
                    .find(|existing| Some(existing.id) == goal)
                    .is_some_and(|goal| {
                        matches!(
                            goal.state,
                            LoopState::Stopped
                                | LoopState::Interrupted
                                | LoopState::Failed
                                | LoopState::Expired
                        )
                    })
        }) {
            return Err(AgentError::Invalid(
                "directed target changed while preparing the input".into(),
            ));
        }
        let inherit_branch_session =
            goal.is_none() && trigger_source.is_none() && original.snapshot.branch_origin.is_some();
        let mut candidate = original.clone();
        let chat = &mut candidate;
        let is_skill_invocation = invoked_skill.is_some();
        if let Some(invoked_skill) = invoked_skill {
            record_invoked_skill(&mut chat.snapshot, invoked_skill);
        }

        let loop_id = if let Some(id) = goal {
            if !chat.snapshot.loops.iter().any(|goal| goal.id == id) {
                return Err(AgentError::NotFound);
            }
            id
        } else {
            let id = Uuid::new_v4();
            chat.snapshot.add_loop(LoopSnapshot {
                id,
                goal: goal_title.unwrap_or_else(|| input.clone()),
                state: LoopState::Idle,
                run_id: None,
                run_started_at: None,
                trigger: None,
                event_sources: Vec::new(),
                state_since: None,
            });
            id
        };
        let run_id = Uuid::new_v4();
        let receipt = Receipt {
            chat_id,
            loop_id,
            run_id,
        };
        let requested_receipt = receipt.clone();
        if trigger_source.is_none() {
            self.apply_pending_model(chat)?;
            recovery::before_resume(
                &mut chat.snapshot,
                loop_id,
                self.inner
                    .settings
                    .as_ref()
                    .and_then(crate::settings::UserSettings::root_directory),
            )?;
        }
        if !chat.snapshot.accepts_images()
            && contexts
                .iter()
                .any(|context| matches!(context, InputContext::Image(_)))
        {
            return Err(AgentError::ImageInputUnsupported);
        }
        let goal = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == loop_id)
            .unwrap();
        if matches!(goal.state, LoopState::Stopped | LoopState::Expired) {
            return Err(AgentError::Invalid(
                "goal has ended; create a new goal".into(),
            ));
        }
        let resume_watcher =
            if trigger_source.is_none() && chat.snapshot.resume_triggers.contains(&loop_id) {
                let trigger = goal
                    .trigger
                    .as_mut()
                    .ok_or_else(|| AgentError::Invalid("restored trigger is unavailable".into()))?;
                if trigger
                    .expires_at_ms
                    .is_some_and(|deadline| deadline <= crate::private_fs::unix_time_milliseconds())
                {
                    return Err(AgentError::Invalid(
                        "goal deadline has passed; create a new goal".into(),
                    ));
                }
                trigger.armed = true;
                let trigger = trigger.clone();
                chat.snapshot.resume_triggers.retain(|id| *id != loop_id);
                let cancel = CancellationToken::new();
                chat.trigger_cancellations.insert(loop_id, cancel.clone());
                Some((trigger, cancel))
            } else {
                None
            };
        if trigger_source.is_some() {
            if trigger_cancel
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                return Err(AgentError::Invalid(
                    "trigger configuration was replaced".into(),
                ));
            }
            if !triggers::can_trigger(goal) {
                return Err(AgentError::Invalid(
                    "goal is not eligible for a trigger".into(),
                ));
            }
            let pending_receipt = chat
                .snapshot
                .pending_inputs
                .iter()
                .find(|pending| {
                    pending.receipt.loop_id == loop_id && pending.trigger_source.is_some()
                })
                .map(|pending| pending.receipt.clone());
            if let Some(pending_receipt) = pending_receipt {
                // A claimed feedback request is never merged into another queued Trigger input.
                if event_claim.is_some() {
                    return Err(AgentError::Busy);
                }
                *original = candidate;
                drop(chats);
                let next = self.next_run(chat_id).await?;
                if let Some(next) = next {
                    self.inner.tasks.lock().await.push(self.run_task(next));
                }
                return Ok(pending_receipt);
            }
        }
        if !matches!(
            goal.state,
            LoopState::Running | LoopState::WaitingPermission | LoopState::WaitingQuestion
        ) {
            goal.set_state(LoopState::Queued);
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
        chat.snapshot.pending_inputs.insert(
            index,
            PendingInput {
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
            },
        );
        let carries_event_claim = event_claim.is_some();
        if let Some(event_claim) = event_claim {
            chat.active_event_claims.insert(receipt.run_id, event_claim);
        }
        let watch = (!chat.watching_commands).then(|| chat.tools.clone());
        chat.watching_commands = true;
        let mut session_context = None;
        let mut session_candidate = None;
        if inherit_branch_session {
            let context = self.inner.session_permissions.lock().await;
            let mut candidate = context.clone();
            candidate
                .inherit_loop(chat_id, loop_id)
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
            session_context = Some(context);
            session_candidate = Some(candidate);
        }
        *original = candidate;
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
        let next = match self.next_run(chat_id).await {
            Ok(next) => next,
            Err(error) => {
                if carries_event_claim {
                    self.withdraw_feedback_input(&receipt).await;
                }
                return Err(error);
            }
        };
        if let Some((trigger, cancel)) = resume_watcher {
            // 先写出重新武装再启动 Trigger 任务：任务第一次读取停止状态时已知道这次武装。
            let recorded = self
                .record_loop_run_state(chat_id, loop_id, loop_lease::RunStateChange::Armed)
                .await;
            self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
            self.inner
                .tasks
                .lock()
                .await
                .push(self.trigger_task(chat_id, loop_id, trigger, cancel));
            self.sync_tick_subscriptions().await;
        }
        if let Some(tools) = watch {
            self.watch_commands(chat_id, tools).await;
        }
        if let Some(next) = next {
            self.inner.tasks.lock().await.push(self.run_task(next));
        }

        self.inner.input_wakeup.send_replace(());
        self.start_title_generation(chat_id).await;
        Ok(requested_receipt)
    }

    fn emit(&self, chat: &mut Chat, receipt: &Receipt, kind: EventKind) {
        chat.snapshot
            .run_loops
            .insert(receipt.run_id, receipt.loop_id);
        self.publish_event(chat, receipt, kind);
    }

    fn publish_event(&self, chat: &mut Chat, receipt: &Receipt, kind: EventKind) {
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
        if !matches!(event.kind, EventKind::Text(_)) {
            self.save(&chat.snapshot);
        }
        let _ = self.inner.events.send(event);
    }

    async fn model_request(
        &self,
        receipt: &Receipt,
        mut request: ModelRequest,
        cancel: CancellationToken,
        used_calls: &mut HashSet<String>,
    ) -> Result<crate::model::ModelOutput, AgentError> {
        {
            let chats = self.inner.chats.lock().await;
            if cancel.is_cancelled()
                || chats
                    .get(&receipt.chat_id)
                    .is_none_or(|chat| chat.active_run != Some(receipt.run_id))
            {
                return Err(crate::model::ModelError::Cancelled.into());
            }
        }
        self.maybe_compact(receipt, cancel.clone()).await?;
        let workspace_host = crate::host_tools::current_workspace_host(
            self.inner.host_tools.read().unwrap().as_deref(),
        );
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).unwrap();
            if workspace_host.runs_ticks()
                && !chat
                    .snapshot
                    .invoked_skills
                    .iter()
                    .any(|skill| skill.name == "tickloop")
            {
                let invoked = chat
                    .skills
                    .expand("tickloop", "", crate::skills::SkillCaller::Model)
                    .and_then(crate::skills::InvokedSkill::try_from)
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                record_invoked_skill(&mut chat.snapshot, invoked);
                self.save(&chat.snapshot);
            }
            (request.messages, request.history_sources) = context::model_history(chat)?;
            let replacements = tool_results::enforce_tool_result_budget(
                &mut request.messages,
                &mut chat.content_replacements,
                chat.tools.chat_directory(),
            );
            if !replacements.is_empty() {
                self.record_content_replacements(&chat.snapshot, replacements);
            }
            let state = serde_json::json!({
                "workdir": chat.snapshot.workdir,
                "family_temp_directory": chat.tools.family_temp_directory(),
                "scratchpad": chat.tools.family_temp_directory().join(crate::tools::SCRATCHPAD_SUBDIRECTORY),
                "delegation": chat.snapshot.delegation,
                "workspace_host": workspace_host,
                "service_connections": self.inner.host_tools.read().unwrap().as_ref().map(|provider| provider.service_connections()).unwrap_or_else(|| serde_json::json!({})),
                "artifact_ticks": chat.snapshot.artifact_ticks,
                "goal": chat.snapshot.loops.iter().find(|goal| goal.id == receipt.loop_id),
                "chat_goals": chat.snapshot.loops,
                "execution_mode": chat.snapshot.policy.mode,
                "plan": chat.snapshot.plans.iter().find(|plan| plan.loop_id == receipt.loop_id),
                "progress": chat.snapshot.progress.iter().find(|progress| progress.loop_id == receipt.loop_id),
                "uncertain_operations": chat.snapshot.current_operation_uncertainties(),
            });
            let plan_mode = self.plan_mode_section(chat, receipt.loop_id);
            request.history_sources.insert(0, None);
            request.messages.insert(
                0,
                Message {
                    role: Role::System,
                    content: vec![Content::Text {
                        text: system_prompt::conversation_system_prompt(
                            &state,
                            chat.output_style,
                            plan_mode.as_deref(),
                        ),
                    }],
                },
            );
        }
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
            .map(|tool| tool.name.clone())
            .collect::<HashSet<_>>();
        let mut recovered_text = String::new();
        let mut continuations = 0;
        let response = loop {
            let mut attempt_text = String::new();
            let attempt = self
                .tracked_model_request_capturing(
                    model_request::RequestContext::for_run(receipt, RequestKind::Conversation),
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
                Err(AgentError::Model(ModelError::ResponseRejected(
                    ModelResponseRejection::OutputLimit,
                ))) if continuations < MAX_OUTPUT_LIMIT_CONTINUATIONS
                    && !attempt_text.trim().is_empty() =>
                {
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
                            text: OUTPUT_LIMIT_CONTINUATION_PROMPT.into(),
                        }],
                    });
                    continuations += 1;
                }
                Err(AgentError::Model(ModelError::Cancelled)) => {
                    recovered_text.push_str(&attempt_text);
                    self.commit_interrupted_reply(receipt, recovered_text)
                        .await?;
                    return Err(ModelError::Cancelled.into());
                }
                Err(error) => return Err(error),
            }
        };
        let invalid_call = response.output.content.iter().any(|content| {
            if let Content::ToolUse { id, name, input } = content {
                id.is_empty()
                    || !available_tools.contains(name)
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
            return Err(crate::model::ModelError::InvalidResponse(
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
            if original.active_run != Some(active.run_id) {
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
                self.append_input_message(&mut candidate, pending);
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
            .filter(|assessment| assessment.outcome == approval_assessment::ApprovalOutcome::Allow)
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
        let chat = chats.get_mut(&receipt.chat_id).unwrap();
        let current_session_operation = SessionPermissionOperation::from_normalized(
            operation.name.clone(),
            permission_input.clone(),
            WorkspaceScope::capture(&configured_workspace_for_chat(chat))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let current_decision = host_tool_decision(
            chat.snapshot.policy.explicit_decision_with_session(
                &operation.name,
                permission_input,
                receipt.loop_id,
                admission.session_evaluation.decision,
            ),
            admission.host_permission,
            chat.snapshot.policy.mode,
        );
        let permission_is_current = session_context.revalidate(
            &admission.session_evaluation.token,
            PermissionOwner::new(receipt.chat_id, receipt.loop_id)
                .map_err(|error| error.to_string())?,
            &current_session_operation,
        );
        if cancel.is_cancelled()
            || chat.active_run != Some(receipt.run_id)
            || !permission_is_current
            || current_decision == Decision::Deny
            || (chat.snapshot.policy.revision != admission.policy_revision
                && current_decision != Decision::Allow)
        {
            return Err("host operation permission changed before admission".into());
        }
        drop(session_context);
        chat.snapshot
            .operations
            .iter_mut()
            .find(|record| record.id == operation.id)
            .expect("proposed host operation remains present")
            .state = OperationState::Admitted;
        let approval = ApprovalRecord::allowed(
            receipt.chat_id,
            operation,
            permission_input,
            &chat.snapshot.policy,
            denials::AllowedDecision {
                path: admission.approval_path,
                direct_basis: admission.direct_basis,
                approval_source: admission.approval_source,
                assessment: admission.assessment,
            },
        );
        chat.denial_window.add_approval(approval);
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

    async fn run(
        &self,
        receipt: Receipt,
        mut request: ModelRequest,
        cancel: CancellationToken,
    ) -> Option<(Receipt, ModelRequest, CancellationToken)> {
        self.complete_file_history_snapshots(receipt.chat_id).await;
        let mut used_calls = HashSet::new();
        let mut is_awaiting_model = true;
        let mut result:Result<Usage,String> = async {
            loop {
                is_awaiting_model = true;
                let host_provider = self.inner.host_tools.read().unwrap().clone();
                if let Some(provider) = host_provider {
                    let _ = provider.refresh(cancel.clone()).await;
                }
                let registry = {
                    let chats = self.inner.chats.lock().await;
                    let chat = chats.get(&receipt.chat_id).unwrap();
                    ToolRegistry::for_chat(
                        chat,
                        self.inner.host_tools.read().unwrap().clone(),
                        self.inner.tick_stream_installed.load(Ordering::SeqCst),
                    )
                };
                request.tools = registry.definitions();
                let output = self.model_request(&receipt,request,cancel.clone(),&mut used_calls).await.map_err(|error|error.to_string())?;
                is_awaiting_model = false;
                let assistant_message_id = Uuid::new_v4();
                let calls = output.content.iter().filter_map(|content| {
                    if let Content::ToolUse {id,name,input}=content {Some(ProposedToolCall {call_id:id.clone(),name:name.clone(),input:input.clone()})} else {None}
                }).collect::<Vec<_>>();
                {
                    let mut chats = self.inner.chats.lock().await;
                    let chat=chats.get_mut(&receipt.chat_id).unwrap();
                    chat.snapshot.pending_thinking.remove(&receipt.run_id);
                    chat.snapshot.message_runs.insert(chat.snapshot.messages.len(),receipt.run_id);
                    chat.snapshot.message_ids.insert(chat.snapshot.messages.len(),assistant_message_id);
                    chat.snapshot.message_usage.insert(chat.snapshot.messages.len(),output.usage);
                    let index = chat.snapshot.messages.len();
                    chat.snapshot.message_models.insert(index, chat.snapshot.model.identity());
                    if let Some(source) = output.private_source.or_else(|| chat.snapshot.model.private_history_source()) {
                        chat.snapshot.message_sources.insert(index, source);
                    }
                    chat.snapshot.messages.push(Message {role:Role::Assistant,content:output.content});
                    chat.snapshot.last_model_run=Some(receipt.run_id);
                    self.emit(chat, &receipt, EventKind::AssistantMessageCommitted { message_id: assistant_message_id, tool_calls: calls.clone() });
                }
                if calls.is_empty() {
                    return self.inner.chats.lock().await.get(&receipt.chat_id).unwrap()
                        .snapshot.request_usage.run_conversation_usage(receipt.run_id)
                        .map_err(|error| error.to_string());
                }
                for ProposedToolCall {call_id:id,name,input} in calls {
                    let invocation = ToolInvocation::new(
                        id.clone(), name.clone(), input, registry.resolve(&name),
                    );
                    let outcome = self.execute_registered_call(
                        &receipt, assistant_message_id, invocation, cancel.clone(),
                    ).await;
                    let outcome = match outcome {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            if let Some(record) = error.denial {
                                self.record_final_denial(record).await;
                            }
                            return Err(error.message);
                        }
                    };
                    let ToolCallOutcome { result, host_call_status, host_effect, images: result_images, permission_comment, continue_after_denial, is_rejected_by_user, denial } = outcome;
                    if let Some(record) = denial {
                        self.record_final_denial(record).await;
                    }
                    let command_state=if name==crate::tools::BASH_TOOL_NAME {self.command_state_for_call(&receipt,&id).await} else {None};
                    let mut result = result;
                    if let Some(crate::host_tools::HostEffect::CreatedArtifact(tick)) = &host_effect
                        && !result.is_error
                        && host_call_status == Some(crate::host_tools::HostCallStatus::Completed)
                        && let Err(error) = self.link_created_artifact(&receipt, tick).await
                    {
                        result.content.push_str(&format!("\n\nThe artifact was created, but linking it for @tickloop requests failed: {error}. The user can press f on it in Monitor to link it."));
                    }
                    let result = self.persist_large_tool_result(&receipt,&name,&id,result).await.map_err(|error|error.to_string())?;
                    let mut chats=self.inner.chats.lock().await;
                    let chat=chats.get_mut(&receipt.chat_id).unwrap();
                    let model_result=if !continue_after_denial {
                        permission_comment.as_deref().map_or_else(||result.content.clone(),|comment|format!("{}\n\nThe user approved this action with this comment: {comment}",result.content))
                    } else {result.content.clone()};
                    let content=if result_images.is_empty() {model_result.into()} else {
                        ToolResultContent::Blocks(std::iter::once(ToolResultBlock::Text {text:model_result}).chain(result_images.into_iter().map(ToolResultBlock::from)).collect())
                    };
                    let block=Content::ToolResult {tool_use_id:id.clone(),content,is_error:result.is_error};
                    let last_index=chat.snapshot.messages.len().checked_sub(1);
                    let same_run=!chat.start_new_tool_result_message && last_index.is_some_and(|index|chat.snapshot.message_runs.get(&index)==Some(&receipt.run_id));
                    if let Some(message)=chat.snapshot.messages.last_mut().filter(|message|same_run && message.role==Role::User && message.content.iter().all(|content|matches!(content,Content::ToolResult{..}))) {message.content.push(block);} else {chat.snapshot.message_runs.insert(chat.snapshot.messages.len(),receipt.run_id);chat.snapshot.messages.push(Message {role:Role::User,content:vec![block]});}
                    chat.start_new_tool_result_message = false;
                    if let Some(operation) = chat.snapshot.operations.iter_mut().find(|operation|operation.run_id==receipt.run_id && operation.call_id==id) {
                        let state=match (host_call_status, command_state) {
                            (Some(crate::host_tools::HostCallStatus::OutcomeUnknown), _)=>OperationState::Unknown,
                            (Some(crate::host_tools::HostCallStatus::NotDispatched), _)=>OperationState::Failed,
                            (_, Some(crate::tools::CommandState::Running|crate::tools::CommandState::Background|crate::tools::CommandState::Cancelling))=>OperationState::Admitted,
                            (_, Some(crate::tools::CommandState::Unknown))=>OperationState::Unknown,
                            (_, Some(crate::tools::CommandState::Completed))=>OperationState::Completed,
                            (_, Some(crate::tools::CommandState::Failed))=>OperationState::Failed,
                            _ if result.is_error=>OperationState::Failed,
                            _=>OperationState::Completed,
                        };
                        if !matches!(operation.state,OperationState::Completed|OperationState::Failed) {operation.state=state;}
                        operation.result=Some(result.clone());
                    }
                    if !result.is_error && host_call_status == Some(crate::host_tools::HostCallStatus::Completed) {
                        match &host_effect {
                            Some(crate::host_tools::HostEffect::CreatedArtifact(tick)) => self.auto_attach_artifact_tick(chat, tick.clone()),
                            Some(crate::host_tools::HostEffect::OpenedTick { instance, title }) => self.record_opened_tick(chat, instance.clone(), title.clone()),
                            Some(crate::host_tools::HostEffect::FinishedFeedbackRequest { request_id }) => {
                                if let Some(active) = chat.active_event_claims.get_mut(&receipt.run_id)
                                    && active.claim.request_id == *request_id
                                {
                                    active.settled = true;
                                }
                            }
                            None => {}
                        }
                    }
                    self.save(&chat.snapshot);
                    self.emit(chat,&receipt,EventKind::ToolCompleted {call_id:id.clone(),result:result.clone()});
                    // Tool errors go back to the model; a bare user denial or stopped foreground command ends the run.
                    if is_rejected_by_user && !continue_after_denial {
                        chat.user_cancelled_runs.insert(receipt.run_id);
                    }
                    if chat.user_cancelled_runs.contains(&receipt.run_id) {
                        cancel.cancel();
                        return Err(result.content);
                    }
                }
                {
                    let mut chats=self.inner.chats.lock().await;
                    let chat=chats.get_mut(&receipt.chat_id).unwrap();
                    self.flush_deferred_messages(chat,MessageBoundary::ToolResults).map_err(|error|error.to_string())?;
                }
                self.merge_queued_messages(&receipt)
                    .await
                    .map_err(|error| error.to_string())?;
                self.complete_file_history_snapshots(receipt.chat_id).await;
                let chats=self.inner.chats.lock().await;
                let chat=chats.get(&receipt.chat_id).unwrap();
                let (messages, history_sources) = context::model_history(chat).map_err(|error|error.to_string())?;
                request=ModelRequest { history_sources,messages,tools:self.tool_definitions(chat),structured_output:None,temperature:None,reasoning_effort:chat.snapshot.effective_reasoning_effort()};
            }
        }.await;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id)?;
        let finished_claim = chat.active_event_claims.remove(&receipt.run_id);
        chat.snapshot.pending_thinking.remove(&receipt.run_id);
        if chat.active_run == Some(receipt.run_id) {
            chat.active = None;
            chat.active_run = None;
        }
        chat.parked.remove(&receipt.run_id);
        chat.ready_runs.retain(|id| *id != receipt.run_id);
        if let Err(error) =
            recovery::close_dangling_tool_calls(&mut chat.snapshot, Some(receipt.run_id))
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
        let goal = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == receipt.loop_id)
            .unwrap();
        let event =
            match result {
                _ if matches!(goal.state, LoopState::Stopped | LoopState::Expired) => {
                    EventKind::Interrupted
                }
                Ok(usage) if !cancel.is_cancelled() => {
                    goal.set_state(
                        if chat.snapshot.policy.mode == ExecutionMode::Plan
                            && chat.snapshot.plans.iter().any(|plan| {
                                plan.loop_id == receipt.loop_id && plan.approval_pending
                            })
                        {
                            LoopState::WaitingPlan
                        } else if goal.trigger.as_ref().is_some_and(|trigger| trigger.armed)
                            || goal
                                .event_sources
                                .iter()
                                .any(|source| !source.resume_required)
                        {
                            LoopState::WaitingTrigger
                        } else {
                            LoopState::Completed
                        },
                    );
                    EventKind::Completed { usage }
                }
                _ if cancel.is_cancelled() => {
                    goal.set_state(LoopState::Interrupted);
                    EventKind::Interrupted
                }
                Err(error) => {
                    goal.set_state(LoopState::Failed);
                    EventKind::Failed(error)
                }
                Ok(_) => unreachable!(),
            };
        // An interrupted or failed Loop stops listening until an explicit Continue links it again.
        let pauses_feedback = matches!(goal.state, LoopState::Interrupted | LoopState::Failed)
            && goal
                .event_sources
                .iter()
                .any(|source| !source.resume_required);
        if pauses_feedback {
            for source in &mut goal.event_sources {
                source.resume_required = true;
            }
        }
        let disarms_feedback =
            pauses_feedback && Self::settle_without_listening(goal, LoopState::Interrupted);
        // A run that ended without `done` hands its request back: an interrupted run for another
        // attempt, a finished or failed one as failed.
        if let Some(claim) = finished_claim.filter(|claim| !claim.settled) {
            let outcome = if cancel.is_cancelled() {
                crate::artifacts::ReleaseOutcome::Retry
            } else {
                crate::artifacts::ReleaseOutcome::Failed
            };
            let agent = self.clone();
            tokio::spawn(async move { agent.release_event_claim(&claim, outcome).await });
        }
        let is_deliverable = chat
            .snapshot
            .loops
            .iter()
            .find(|goal| goal.id == receipt.loop_id)
            .is_some_and(|goal| {
                matches!(goal.state, LoopState::WaitingTrigger | LoopState::Completed)
            });
        self.save(&chat.snapshot);
        self.emit(chat, &receipt, event);
        self.inner
            .host_contexts
            .lock()
            .unwrap()
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
        if disarms_feedback {
            self.disarm_feedback_lease(receipt.chat_id, receipt.loop_id)
                .await;
        }
        let next = match self.next_run(receipt.chat_id).await {
            Ok(next) => next,
            Err(error) => {
                let mut chats = self.inner.chats.lock().await;
                if let Some(chat) = chats.get_mut(&receipt.chat_id) {
                    self.emit(chat, &receipt, EventKind::Failed(error.to_string()));
                }
                None
            }
        };
        self.flush_subagent_notifications(receipt.chat_id).await;
        self.record_directed_replies(&receipt, &finished_snapshot)
            .await;
        if finished_snapshot.delegation.is_some() {
            self.record_family_completion(&receipt, &finished_snapshot)
                .await;
        }
        next
    }

    fn run_task(
        &self,
        first: (Receipt, ModelRequest, CancellationToken),
    ) -> tokio::task::JoinHandle<()> {
        let agent = self.clone();
        tokio::spawn(async move {
            let mut next = Some(first);
            while let Some((receipt, request, cancel)) = next {
                next = agent.run(receipt, request, cancel).await;
            }
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
        if !chat.ready_runs.contains(&receipt.run_id) {
            chat.ready_runs.push_back(receipt.run_id);
        }
        drop(chats);
        let next = self
            .next_run(receipt.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(next) = next {
            self.inner.tasks.lock().await.push(self.run_task(next));
        }
        tokio::select! {biased;_ = cancel.cancelled()=>Err("waiting run was cancelled".into()),_ = resume.notified()=>Ok(())}
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
        operation.state = OperationState::Admitted;
        Ok(())
    }

    /// Writes an accepted input to the conversation. User prompts capture the current execution
    /// mode and create a pending file snapshot as a rewind point. The run completes snapshots
    /// outside the lock through `complete_file_history_snapshots`.
    fn append_input_message(&self, chat: &mut Chat, pending: PendingInput) {
        if pending.task_continuation {
            chat.snapshot
                .run_loops
                .insert(pending.receipt.run_id, pending.receipt.loop_id);
            return;
        }
        let message_index = chat.snapshot.messages.len();
        chat.snapshot
            .message_runs
            .insert(message_index, pending.receipt.run_id);
        chat.snapshot
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
            chat.snapshot
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
        let message_id = pending
            .directed_human
            .as_ref()
            .map_or_else(Uuid::new_v4, |source| source.message_id);
        if let Some(source) = pending.directed_human {
            chat.snapshot
                .human_family_origins
                .insert(message_id, source);
        }
        chat.snapshot.message_ids.insert(message_index, message_id);
        chat.snapshot
            .prompt_execution_modes
            .insert(message_index, chat.snapshot.policy.mode);
        chat.file_history.begin_snapshot(message_id);
    }

    async fn next_run(
        &self,
        chat_id: ChatId,
    ) -> Result<Option<(Receipt, ModelRequest, CancellationToken)>, AgentError> {
        loop {
            let admission = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                self.select_next_run(chat, None)?
            };
            match admission {
                RunAdmission::Idle => return Ok(None),
                RunAdmission::Ready(receipt, request, cancel) => {
                    return Ok(Some((receipt, request, cancel)));
                }
                RunAdmission::ValidateTick(pending) => {
                    let source = pending.trigger_source.as_ref().unwrap();
                    if self
                        .revalidate_tick_source(chat_id, pending.receipt.loop_id, &source.payload)
                        .await
                        .is_ok()
                    {
                        if let Ok(mut chats) = self.inner.chats.try_lock() {
                            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                            match self.select_next_run(chat, Some(&pending))? {
                                RunAdmission::Idle => return Ok(None),
                                RunAdmission::Ready(receipt, request, cancel) => {
                                    return Ok(Some((receipt, request, cancel)));
                                }
                                RunAdmission::ValidateTick(_) => {}
                            }
                        }
                    } else {
                        let mut chats = self.inner.chats.lock().await;
                        if let Some(chat) = chats.get_mut(&chat_id) {
                            chat.snapshot
                                .pending_inputs
                                .retain(|queued| queued.receipt.run_id != pending.receipt.run_id);
                            self.finish_discarded_trigger_input(chat, &pending.receipt);
                            self.discard_invalid_tick_inputs(chat);
                            self.save(&chat.snapshot);
                        }
                    }
                }
            }
        }
    }

    fn select_next_run(
        &self,
        chat: &mut Chat,
        validated_tick: Option<&PendingInput>,
    ) -> Result<RunAdmission, AgentError> {
        self.flush_deferred_messages(chat, MessageBoundary::Idle)?;
        if self.inner.closed.load(Ordering::SeqCst)
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
            let goal = candidate
                .snapshot
                .loops
                .iter_mut()
                .find(|goal| goal.id == parked.receipt.loop_id)
                .ok_or(AgentError::NotFound)?;
            if matches!(goal.state, LoopState::Stopped | LoopState::Expired) {
                continue;
            }
            goal.set_state(LoopState::Running);
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
        while let Some(index) = candidate
            .snapshot
            .pending_inputs
            .iter()
            .position(|pending| {
                pending.armed
                    && command_barrier.is_none_or(|sequence| pending.queued_sequence < sequence)
                    && preceding_model_runs
                        .as_ref()
                        .is_none_or(|runs| runs.contains(&pending.receipt.run_id))
                    && candidate.snapshot.loops.iter().any(|goal| {
                        goal.id == pending.receipt.loop_id
                            && !matches!(
                                goal.state,
                                LoopState::WaitingPermission | LoopState::WaitingQuestion
                            )
                    })
            })
        {
            let pending = candidate.snapshot.pending_inputs[index].clone();
            let goal = candidate
                .snapshot
                .loops
                .iter()
                .find(|goal| goal.id == pending.receipt.loop_id)
                .ok_or(AgentError::NotFound)?;
            if matches!(goal.state, LoopState::Stopped | LoopState::Expired)
                || (pending.trigger_source.is_some() && !triggers::can_trigger(goal))
            {
                candidate.snapshot.pending_inputs.remove(index);
                continue;
            }
            if let Some(source) = &pending.trigger_source
                && source.source == tick_subscriptions::TICK_SUBSCRIPTION_SOURCE
            {
                let validated = validated_tick.is_some_and(|validated| {
                    validated.receipt.run_id == pending.receipt.run_id
                        && validated.input == pending.input
                        && validated.trigger_source.as_ref().is_some_and(|previous| {
                            previous.source == source.source && previous.payload == source.payload
                        })
                });
                if !validated {
                    self.save(&candidate.snapshot);
                    *chat = candidate;
                    return Ok(RunAdmission::ValidateTick(Box::new(pending)));
                }
                if self
                    .validate_tick_source_bindings(
                        pending.receipt.chat_id,
                        pending.receipt.loop_id,
                        &source.payload,
                        &candidate.snapshot.loops,
                    )
                    .is_err()
                {
                    candidate.snapshot.pending_inputs.remove(index);
                    self.finish_discarded_trigger_input(&mut candidate, &pending.receipt);
                    self.discard_invalid_tick_inputs(&mut candidate);
                    continue;
                }
            }
            recovery::before_resume(
                &mut candidate.snapshot,
                pending.receipt.loop_id,
                self.inner
                    .settings
                    .as_ref()
                    .and_then(crate::settings::UserSettings::root_directory),
            )?;
            candidate.snapshot.pending_inputs.remove(index);
            let mut merged_inputs = Vec::new();
            let mut agent_message_receipts = Vec::new();
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
                        && !queued.armed
                        && queued.receipt.loop_id == pending.receipt.loop_id
                    {
                        let receipt = queued.receipt.clone();
                        self.append_input_message(&mut candidate, queued);
                        agent_message_receipts.push(receipt);
                    } else {
                        retained.push(queued);
                    }
                }
                candidate.snapshot.pending_inputs = retained;
            }
            let goal = candidate
                .snapshot
                .loops
                .iter_mut()
                .find(|goal| goal.id == pending.receipt.loop_id)
                .unwrap();
            goal.set_state(LoopState::Running);
            goal.run_id = Some(pending.receipt.run_id);
            goal.run_started_at = Some(crate::private_fs::unix_time_milliseconds());
            let cancel = CancellationToken::new();
            candidate.active = Some(cancel.clone());
            candidate.active_run = Some(pending.receipt.run_id);
            let receipt = pending.receipt.clone();
            self.append_input_message(&mut candidate, pending);
            let mut merged_receipts = Vec::with_capacity(merged_inputs.len());
            for queued in merged_inputs {
                let consumed = queued.receipt.clone();
                self.merge_host_context(&receipt, &consumed);
                self.append_input_message(&mut candidate, queued);
                merged_receipts.push(consumed);
            }
            self.save(&candidate.snapshot);
            for consumed in agent_message_receipts {
                self.emit(
                    &mut candidate,
                    &consumed,
                    EventKind::InputMerged {
                        into_run_id: receipt.run_id,
                    },
                );
            }
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
                history_sources,
                messages,
                tools: self.tool_definitions(&candidate),
                structured_output: None,
                temperature: None,
                reasoning_effort: candidate.snapshot.effective_reasoning_effort(),
            };
            *chat = candidate;
            return Ok(RunAdmission::Ready(receipt, request, cancel));
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
        approval_source: PermissionApprovalSource,
        approval_reason: Option<String>,
        cancel: CancellationToken,
    ) -> Result<PermissionAnswer, String> {
        let request = PermissionRequest {
            id: Uuid::new_v4(),
            loop_id: receipt.loop_id,
            execution: PermissionExecution::ToolCall {
                run_id: receipt.run_id,
                call_id: operation.call_id.clone(),
            },
            name: operation.name.clone(),
            input: operation.input.clone(),
            file_change: operation.file_change.clone().map(Box::new),
            approval_reason,
            approval_source,
        };
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
        let model_receipt = request.execution.run_id().map(|run_id| Receipt {
            chat_id,
            loop_id: request.loop_id,
            run_id,
        });
        let (sender, receiver) = oneshot::channel();
        let owner =
            PermissionOwner::new(chat_id, request.loop_id).map_err(|error| error.to_string())?;
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
                    .loops
                    .iter_mut()
                    .find(|goal| goal.id == receipt.loop_id)
                    .ok_or("permission Loop is unavailable")?
                    .set_state(LoopState::WaitingPermission);
                self.park_run(chat, receipt, cancel.clone())
                    .map_err(|error| error.to_string())?;
            } else if let PermissionExecution::UserShell { command_id } = request.execution
                && !chat.snapshot.user_shells.iter().any(|shell| {
                    shell.id == command_id && shell.state == UserShellState::WaitingPermission
                })
            {
                return Err("user Shell permission expired".into());
            }
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
                run_id: Uuid::nil(),
            });
            self.publish_event(
                chat,
                &receipt,
                EventKind::PermissionRequested {
                    request: request.clone(),
                },
            );
        }
        if model_receipt.is_some()
            && let Some(next) = self
                .next_run(chat_id)
                .await
                .map_err(|error| error.to_string())?
        {
            self.inner.tasks.lock().await.push(self.run_task(next));
        }
        let answer = tokio::select! {biased; _ = cancel.cancelled() => Err("permission request cancelled".into()), answer = receiver => answer.map_err(|_| "permission request expired".to_owned())};
        let resumed = if answer.is_ok() {
            if let Some(receipt) = &model_receipt {
                self.resume_parked_run(receipt, cancel.clone()).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        let mut chats = self.inner.chats.lock().await;
        if let Some(chat) = chats.get_mut(&chat_id) {
            chat.snapshot
                .permissions
                .retain(|permission| permission.id != request.id);
            self.save(&chat.snapshot);
        }
        self.inner.approvals.lock().await.remove(&request.id);
        resumed?;
        answer
    }

    pub async fn set_search(
        &self,
        chat_id: ChatId,
        config: Option<crate::tools::SearchConfig>,
    ) -> Result<(), AgentError> {
        let tools = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .tools
            .clone();
        tools
            .set_search(config)
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))
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
        chat.snapshot.policy.mode = mode;
        chat.snapshot.policy.revision += 1;
        self.save(&chat.snapshot);
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
        let _settings_gate = self.inner.settings_writes.lock().await;
        let changes = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat_settings::persistent_rule_changes(&chat.snapshot, &rules)?
        };
        self.save_persistent_rule_changes(&changes).await?;
        let mut chats = self.inner.chats.lock().await;
        self.sync_persistent_rules(&mut chats, &changes, Some(chat_id));
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot.policy.rules = rules;
        chat_settings::record_rule_projects(&mut chat.snapshot, &changes);
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
        workdir: PathBuf,
        additional: Vec<PathBuf>,
    ) -> Result<(), AgentError> {
        self.require_root_chat(chat_id, "change working directories")
            .await?;
        let canonical = |path: PathBuf| -> Result<PathBuf, AgentError> {
            let path = path
                .canonicalize()
                .map_err(|_| AgentError::Invalid("directory is unavailable".into()))?;
            if !path.is_dir() {
                return Err(AgentError::Invalid("path is not a directory".into()));
            }
            Ok(path)
        };
        let workdir = canonical(workdir)?;
        let additional = additional
            .into_iter()
            .map(canonical)
            .collect::<Result<Vec<_>, _>>()?;
        self.require_working_directory_trust(&workdir)?;
        let _settings_gate = self.inner.settings_writes.lock().await;
        let saved = self.load_user_settings().await?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let directory_changed =
            chat.snapshot.workdir != workdir || chat.snapshot.additional_directories != additional;
        if chat.snapshot.workdir != workdir {
            // 持久规则属于工作目录的分区：换目录时换成新目录的持久规则，会话规则保留。
            chat.snapshot.workdir = workdir;
            chat.skills = crate::skills::SkillRegistry::discover(
                &chat.snapshot.workdir,
                self.inner.user_skills_directory.as_deref(),
            );
            if let Some(saved) = saved {
                self.replace_persistent_rules(&mut chat.snapshot, saved);
            }
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
                    request.id == request_id && request.name == EXIT_PLAN_MODE_TOOL_NAME
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
                    let parent =
                        PermissionOwner::new(metadata.parent_chat_id, metadata.parent_loop_id)
                            .map_err(|error| AgentError::Invalid(error.to_string()))?;
                    let mut changed = false;
                    for denial in candidate.freeze_denials(parent) {
                        if !denials.contains(&denial) {
                            denials.push(denial);
                            changed = true;
                        }
                    }
                    if changed {
                        self.record_delegation(child.snapshot.id, metadata.clone());
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

    pub async fn snapshot(&self, chat: ChatId) -> Result<ChatSnapshot, AgentError> {
        self.inner
            .chats
            .lock()
            .await
            .get(&chat)
            .map(|chat| {
                let mut snapshot = chat.snapshot.clone();
                snapshot.provider_failures = self.inner.provider_failures.lock().unwrap().clone();
                snapshot
            })
            .ok_or(AgentError::NotFound)
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
                pending.agent_message.is_some() || !ids.contains(&pending.receipt.run_id)
            });
            candidate
                .snapshot
                .pending_commands
                .retain(|command| !ids.contains(&command.id));
            candidate.snapshot.user_shells.retain(|record| {
                record.state != UserShellState::Queued || !ids.contains(&record.id)
            });
            for entry in &taken {
                if let Some(loop_id) = entry.loop_id
                    && candidate.active_run.is_none()
                    && !candidate
                        .snapshot
                        .pending_inputs
                        .iter()
                        .any(|pending| pending.receipt.loop_id == loop_id)
                    && let Some(goal) = candidate
                        .snapshot
                        .loops
                        .iter_mut()
                        .find(|goal| goal.id == loop_id)
                    && goal.state == LoopState::Queued
                {
                    goal.set_state(LoopState::Interrupted);
                }
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
                    run_id: entry.id,
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
        loop_id: Option<LoopId>,
        command: String,
    ) -> Result<Uuid, AgentError> {
        if command.trim().is_empty() || !command.trim_start().starts_with('/') {
            return Err(AgentError::Invalid(
                "queued command must start with /".into(),
            ));
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if loop_id.is_some_and(|loop_id| !chat.snapshot.loops.iter().any(|goal| goal.id == loop_id))
        {
            return Err(AgentError::NotFound);
        }
        chat.snapshot.last_queue_sequence += 1;
        let id = Uuid::new_v4();
        chat.snapshot.pending_commands.push(PendingCommand {
            id,
            chat_id,
            loop_id,
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
        self.append_local_messages(chat, [local_messages::task_notification(&notification)])?;
        let _ = self.inner.task_notifications.send(notification);
        Ok(self.queue_task_continuation(chat, loop_id))
    }

    /// Continues the owning Loop without creating one for an unowned Shell result.
    /// An already queued input absorbs further task notifications into the same run.
    fn queue_task_continuation(&self, chat: &mut Chat, loop_id: Option<LoopId>) -> bool {
        let Some(loop_id) = loop_id else {
            return false;
        };
        if chat
            .snapshot
            .pending_inputs
            .iter()
            .any(|pending| pending.armed && pending.receipt.loop_id == loop_id)
        {
            return false;
        }
        let Some(goal) = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == loop_id)
        else {
            return false;
        };
        if matches!(
            goal.state,
            LoopState::Stopped | LoopState::Expired | LoopState::Interrupted | LoopState::Failed
        ) {
            return false;
        }
        if !matches!(
            goal.state,
            LoopState::Running | LoopState::WaitingPermission | LoopState::WaitingQuestion
        ) {
            goal.set_state(LoopState::Queued);
        }
        chat.snapshot.last_queue_sequence += 1;
        let queued_sequence = chat.snapshot.last_queue_sequence;
        chat.snapshot.pending_inputs.push(PendingInput {
            receipt: Receipt {
                chat_id: chat.snapshot.id,
                loop_id,
                run_id: Uuid::new_v4(),
            },
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
        self.save(&chat.snapshot);
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.shutdown.cancel();
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
                if let Ok(command) = tools.stop_user_shell(command.operation_id, chat_id).await {
                    let _ = self.record_user_shell(chat_id, command).await;
                }
            }
        }
        for chat in self.inner.chats.lock().await.values() {
            if let Some(cancel) = &chat.active {
                cancel.cancel();
            }
            for waiting in chat.parked.values() {
                waiting.cancel.cancel();
            }
            for cancel in chat.event_cancellations.values() {
                cancel.cancel();
            }
        }
        let tasks = std::mem::take(&mut *self.inner.tasks.lock().await);
        for task in tasks {
            let _ = task.await;
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
        self.inner.loop_leases.release_all();
        self.close_transcripts().await;
        for chat in self.inner.chats.lock().await.values_mut() {
            chat.retention = None;
        }
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

pub(crate) fn reserved_host_tool_names() -> HashSet<String> {
    crate::tools::Tools::definitions()
        .into_iter()
        .chain(plan::definitions())
        .chain(interaction::definitions())
        .chain(tick_subscriptions::definitions())
        .chain(delegation::definitions())
        .chain(agent_messages::definitions(false))
        .map(|definition| definition.name)
        .chain(std::iter::once(crate::skills::SKILL_TOOL_NAME.into()))
        .chain(crate::mcp_tools::ARTIFACT_TOOL_NAMES.map(String::from))
        .collect()
}

fn record_invoked_skill(snapshot: &mut ChatSnapshot, invoked: crate::skills::InvokedSkill) {
    snapshot
        .invoked_skills
        .retain(|existing| existing.name != invoked.name);
    snapshot.invoked_skills.push(invoked);
}

#[cfg(test)]
mod thinking_settings_tests {
    use super::*;

    #[test]
    fn disabled_preference_applies_only_when_model_card_allows_it() {
        let model = crate::model::cards::built_in_model_profiles().remove(0);
        let mut snapshot = empty_snapshot(Uuid::new_v4(), PathBuf::new(), model.clone());
        snapshot.thinking_enabled = false;
        assert_eq!(
            snapshot.effective_reasoning_effort(),
            Some(crate::model::ReasoningEffort::Off)
        );

        snapshot.model = crate::model::cards::ModelCatalog::defaults()
            .get("anthropic-api", "claude-fable-5-messages")
            .unwrap();
        assert_eq!(snapshot.effective_reasoning_effort(), None);
        assert!(!snapshot.thinking_enabled);

        snapshot.model = model;
        assert_eq!(
            snapshot.effective_reasoning_effort(),
            Some(crate::model::ReasoningEffort::Off)
        );
    }
}
