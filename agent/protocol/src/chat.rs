use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

use crate::{
    AgentMessageSource, ApprovalAssessmentDetails, ArtifactTick, BranchOrigin, ChatId,
    CommandSnapshot, CompactionMarker, CompactionStatus, Content, Decision, DelegationMetadata,
    DenialReference, DirectedHumanSource, ExecutionMode, FamilyCompletion, FamilyInputRecord,
    FileReadRecord, ImageContext, ImageOrigin, InputContext, InvokedSkill, LoopSnapshot, Message,
    ModelConfig, ModelIdentity, ModelRequestKind, MonitorMembership, PermissionPolicy, Plan,
    PreparedFileChange, PrivateHistorySource, Progress, ProviderHttpFailure, QuestionRequest,
    ReasoningEffort, RequestUsageLedger, Role, ToolResult, TriggerSource, Usage,
};

pub use crate::ids::{InputId, LoopId, ResponseId, RunId};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatSnapshot {
    #[serde(
        default = "crate::SearchConfig::enabled_default",
        skip_serializing_if = "crate::SearchConfig::is_enabled_default"
    )]
    pub search_config: Option<crate::SearchConfig>,
    #[serde(default)]
    pub task_tools_enabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub task_boards: BTreeMap<LoopId, crate::TaskBoard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_board: Option<crate::TaskBoard>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub subagent_reports: BTreeMap<RunId, crate::SubagentReport>,
    #[serde(default)]
    pub failure_events: Vec<Event>,
    #[serde(default)]
    pub provider_failures: HashMap<String, ProviderHttpFailure>,
    #[serde(skip)]
    pub tool_call_recoveries: Vec<ToolCallRecovery>,
    #[serde(default)]
    pub artifact_ticks: Vec<ArtifactTick>,
    /// The root Chat's scheduled tasks, at most [`crate::MAX_SCHEDULED_TASKS_PER_CHAT`], one of
    /// them self-paced at most.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduled_tasks: Vec<crate::ScheduledTask>,
    /// The tasks cancelled lately, whose runs handed over or restored are no longer taken.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduled_task_revocations: Vec<crate::ScheduledTaskRevocation>,
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
    /// The Chat's Folder; `None` for a Chat without one, which reaches no local files.
    pub folder: Option<PathBuf>,
    /// Chat 当前使用的模型，只在本进程内有效；恢复时使用宿主当前选择的模型。
    #[serde(skip, default = "ModelConfig::unselected")]
    pub model: ModelConfig,
    #[serde(skip)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip, default = "thinking_enabled_by_default")]
    pub thinking_enabled: bool,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invoked_skills: Vec<InvokedSkill>,
    #[serde(default)]
    pub message_runs: BTreeMap<usize, RunId>,
    /// Recorded Trigger sources for display only; raw messages remain the model's input.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub trigger_origins: BTreeMap<RunId, crate::TriggerIdentity>,
    #[serde(default)]
    pub message_contexts: BTreeMap<usize, Vec<InputContext>>,
    /// Stable Transcript identities for prompts, replies and host messages, including after
    /// restoration. Identity alone does not establish user authority; source metadata does.
    #[serde(default)]
    pub message_ids: BTreeMap<usize, Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_message_origins: BTreeMap<Uuid, serde_json::Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub human_family_origins: BTreeMap<Uuid, DirectedHumanSource>,
    /// Source identities across compaction rewrites; these do not establish user authority.
    #[serde(
        default,
        alias = "original_user_message_ids",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub original_message_ids: BTreeMap<usize, Uuid>,
    /// Execution modes captured for user prompts that can serve as rewind points.
    #[serde(default)]
    pub prompt_execution_modes: BTreeMap<usize, ExecutionMode>,
    /// Copied prompts in a Branch remain rewind points, but are not fresh child-Chat authorization.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub inherited_prompts: HashSet<usize>,
    /// User-role messages the host wrote for the model. History views skip them; they are never
    /// human input.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host_notes: BTreeMap<usize, HostNote>,
    /// Host tool names the model was last told about; `None` when this history has never been
    /// told, as in an older Transcript or after compaction dropped the notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announced_host_tools: Option<BTreeSet<String>>,
    /// 压缩边界与压缩摘要消息（链上字段）。
    #[serde(default)]
    pub message_compactions: BTreeMap<usize, CompactionMarker>,
    /// assistant 消息的用量（链上字段）。定向摘要保留的消息在恢复时照 Claude Code 清零。
    #[serde(default)]
    pub message_usage: BTreeMap<usize, Usage>,
    #[serde(default)]
    pub message_sources: BTreeMap<usize, PrivateHistorySource>,
    #[serde(default)]
    pub message_models: BTreeMap<usize, ModelIdentity>,
    /// The Chat's goal; only a root Agent's Chat has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<crate::GoalSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goal_history: Vec<crate::GoalHistoryEntry>,
    /// The Chat's one Loop, there from the Chat's creation. 链上字段：由对话链上的 Loop 条目重放得到。
    pub chat_loop: LoopSnapshot,
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
    /// 本进程内的 Agent 命令状态，不随 Chat 保存，重启后不可见。
    #[serde(skip)]
    pub commands: Vec<CommandSnapshot>,
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

impl ChatSnapshot {
    pub fn run_response_text(&self, run: RunId) -> Option<String> {
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
        highest_pasted_image_id(&self.message_contexts, &self.pending_inputs)
    }

    pub fn queued_inputs(&self) -> Vec<QueuedInput> {
        queued_inputs(
            &self.pending_inputs,
            &self.pending_commands,
            &self.user_shells,
        )
    }

    /// 本进程内结果未知的操作，作为宿主状态告知模型。
    pub fn current_operation_uncertainties(&self) -> Vec<serde_json::Value> {
        self.operations
            .iter()
            .filter(|operation| operation.state == OperationState::Unknown)
            .map(|operation| {
                serde_json::json!({
                    "id": operation.id,
                    "loop_id": operation.loop_id,
                    "tool": operation.name,
                })
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub run_id: RunId,
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
pub enum EventKind {
    FamilyInputUpdated {
        record: FamilyInputRecord,
    },
    FamilyRunCompleted {
        completion: FamilyCompletion,
    },
    /// A new answer attempt began; its lifecycle record is written with this event.
    ResponseStarted {
        response_id: ResponseId,
        attempt: u32,
        /// The display id of the prompt the answer belongs under.
        display_parent: Option<Uuid>,
    },
    Started,
    Text(String),
    Thinking(String),
    /// The model began writing a tool call whose input still streams. Display only: the call
    /// itself comes with `AssistantMessageCommitted`, and no Transcript record reflects this.
    ToolUseStarted {
        call_id: String,
        name: String,
    },
    AssistantMessageCommitted {
        message_id: Uuid,
        tool_calls: Vec<ProposedToolCall>,
    },
    Completed {
        usage: Usage,
    },
    Failed(String),
    ModelRetry {
        kind: ModelRequestKind,
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
    ToolStarted {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolCompleted {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sources: Vec<crate::CitationSource>,
        call_id: String,
        result: ToolResult,
    },
    PermissionChecked {
        decision: Decision,
        reason: String,
    },
    ApprovalAssessed {
        assessment: Box<ApprovalAssessmentDetails>,
    },
    ContextCompaction {
        status: CompactionStatus,
        error: Option<String>,
    },
    PlanUpdated {
        plan: Plan,
    },
    TaskBoardChanged {
        root_chat_id: ChatId,
        root_loop_id: LoopId,
        revision: u64,
    },
    /// The Chat's scheduled tasks changed; the snapshot holds them. `notice` is what the Chat
    /// shows once about it.
    ScheduledTasksChanged {
        notice: Option<crate::ScheduledTaskNotice>,
    },
    ProgressUpdated {
        progress: Progress,
    },
    CommandUpdated {
        command: CommandSnapshot,
    },
    /// The Chat's goal changed; `notice` is the line the conversation shows for it, if any.
    GoalUpdated {
        goal: Option<crate::GoalSnapshot>,
        notice: Option<crate::GoalNotice>,
    },
}

impl EventKind {
    /// Streaming progress that no Transcript record reflects; a publisher may release it without
    /// waiting for a commit.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Text(_)
                | Self::Thinking(_)
                | Self::ToolUseStarted { .. }
                | Self::ModelRetry { .. }
        )
    }
}

/// What a host note tells the model.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostNote {
    /// The host tool set changed; the names announced as available once the note was written.
    HostTools(BTreeSet<String>),
    /// Claude Code's `plan_mode_exit` attachment: the user left Plan mode without approving a
    /// plan.
    PlanModeExit,
    /// Guidance the model follows for the rest of the Chat, written into history once where it
    /// starts to apply so every later request keeps the earlier conversation as its prefix.
    Guidance(GuidanceNote),
}

impl HostNote {
    pub fn announced_host_tools(&self) -> Option<&BTreeSet<String>> {
        match self {
            Self::HostTools(announced) => Some(announced),
            Self::PlanModeExit | Self::Guidance(_) => None,
        }
    }
}

/// What a guidance note announces, like Claude Code's Skill messages and
/// `mcp_instructions_delta` attachment.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuidanceNote {
    /// An invoked Skill's guidance; `digest` identifies its full content, and `truncated` marks
    /// the shortened copy a compaction restores.
    Skill {
        name: String,
        digest: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// The instructions an MCP server gave when the host connected to it.
    McpInstructions { server: String, digest: String },
    /// Guidance announced earlier that no longer applies.
    Withdrawn {
        #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
        skills: BTreeSet<String>,
        #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
        mcp_servers: BTreeSet<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProposedToolCall {
    pub call_id: String,
    pub name: String,
    pub input: serde_json::Value,
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
    pub file_change: Option<Box<PreparedFileChange>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_binding: Option<crate::TaskExecutionBinding>,
    pub id: Uuid,
    pub loop_id: LoopId,
    pub run_id: RunId,
    pub call_id: String,
    pub name: String,
    pub input: serde_json::Value,
    #[serde(default)]
    pub file_change: Option<PreparedFileChange>,
    pub state: OperationState,
    pub result: Option<ToolResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCallRecovery {
    pub run_id: Option<RunId>,
    pub call_id: String,
    pub was_admitted: Option<bool>,
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
    /// Whether the Shell still waits for admission: for approval, or accepted but without a
    /// started process.
    pub fn awaits_admission(&self) -> bool {
        self.state == UserShellState::WaitingPermission
            || self.state == UserShellState::Running && self.pid.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserShellReceipt {
    pub chat_id: ChatId,
    pub command_id: Uuid,
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
    pub denial_reference: Option<DenialReference>,
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
    pub is_skill_invocation: bool,
    /// Files attached whole with the prompt; they count as read once the prompt enters the
    /// conversation. Not persisted, so a restored prompt grants no reads.
    #[serde(skip)]
    pub attached_file_reads: Vec<(PathBuf, FileReadRecord)>,
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

/// 一个 Transcript 在项目目录中的摘要，供 Chat 选择器使用。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SavedChat {
    pub id: ChatId,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub modified: SystemTime,
    pub cwd: Option<PathBuf>,
    pub branch_source: Option<ChatId>,
    pub git_branch: Option<String>,
}

const MAX_TITLE_COLUMNS: usize = 64;

/// `text` on one line within the columns a Chat title may take, ending in `…` when cut.
pub fn short_chat_title(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = String::new();
    let mut columns = 0;
    for character in text.chars().filter(|character| !character.is_control()) {
        let width = character.width().unwrap_or(0);
        if columns + width > MAX_TITLE_COLUMNS - 1 {
            title.push('…');
            return title;
        }
        title.push(character);
        columns += width;
    }
    title
}

const NEW_AGENT_TITLE: &str = "New Agent";

/// Provides a bounded label from the first user prompt or Shell until a generated or explicit
/// Chat title exists.
pub(crate) fn fallback_title(
    first_prompt: Option<&str>,
    user_shells: &[UserShellRecord],
) -> String {
    first_prompt
        .map(short_chat_title)
        .or_else(|| {
            user_shells
                .first()
                .map(|shell| short_chat_title(&format!("!{}", shell.command)))
        })
        .unwrap_or_else(|| NEW_AGENT_TITLE.into())
}

pub fn chat_display_title(snapshot: &ChatSnapshot, user_shells: &[UserShellRecord]) -> String {
    snapshot
        .branch_origin
        .as_ref()
        .map(|origin| origin.name.clone())
        .or_else(|| snapshot.generated_title.clone())
        .unwrap_or_else(|| {
            fallback_title(
                first_prompt(snapshot, &snapshot.pending_inputs).as_deref(),
                user_shells,
            )
        })
}

/// What the conversation was first asked: a delegated Chat's task, or else the first prompt the
/// user gave it, or the first one still queued.
pub(crate) fn first_prompt(
    conversation: &ChatSnapshot,
    pending_inputs: &[PendingInput],
) -> Option<String> {
    if let Some(delegation) = &conversation.delegation {
        return Some(delegation.task.clone());
    }
    conversation
        .prompt_execution_modes
        .keys()
        .find_map(|index| conversation.rewind_point_at(*index))
        .map(|point| point.prompt)
        .or_else(|| {
            pending_inputs
                .iter()
                .find(|pending| is_queued_user_message(pending))
                .map(|pending| pending.input.clone())
        })
}

fn is_queued_user_message(pending: &PendingInput) -> bool {
    pending.trigger_source.is_none()
        && !pending.task_continuation
        && pending.agent_message.is_none()
}

/// Whether an execution ever received work. Configuration changes alone leave it empty.
pub fn chat_has_work(
    conversation: &ChatSnapshot,
    user_shells: &[UserShellRecord],
    queued: usize,
) -> bool {
    !conversation.messages.is_empty() || !user_shells.is_empty() || queued > 0
}

const PREVIEW_MAX_CHARACTERS: usize = 160;

/// The latest conversation content as one bounded line: the newest reply, or the newest typed
/// prompt with its text (an image-only prompt reads as an image cue). Tool traffic, thinking and
/// local notices are skipped; text that only repeats `title` adds nothing.
pub fn chat_preview(conversation: &ChatSnapshot, title: &str) -> Option<String> {
    let text = conversation
        .messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            if conversation.host_notes.contains_key(&index)
                || conversation
                    .message_ids
                    .get(&index)
                    .is_some_and(|id| conversation.agent_message_origins.contains_key(id))
            {
                return None;
            }
            let texts = message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let text = match message.role {
                Role::Assistant => texts.last().copied()?.to_owned(),
                Role::User
                    if message
                        .content
                        .iter()
                        .any(|content| matches!(content, Content::ToolResult { .. })) =>
                {
                    return None;
                }
                Role::User if texts.iter().all(|text| text.trim().is_empty()) => {
                    let images = message
                        .content
                        .iter()
                        .filter(|content| matches!(content, Content::Image { .. }))
                        .count();
                    match images {
                        0 => return None,
                        1 => "[image]".to_owned(),
                        images => format!("[{images} images]"),
                    }
                }
                Role::User => texts.join("\n"),
                _ => return None,
            };
            (!text.trim().is_empty()
                && matches!(
                    crate::LocalMessage::parse(&text),
                    None | Some(crate::LocalMessage::AgentCommunication { .. })
                ))
            .then_some(text)
        })?;
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line == title.split_whitespace().collect::<Vec<_>>().join(" ") {
        return None;
    }
    Some(line.chars().take(PREVIEW_MAX_CHARACTERS).collect())
}

/// The highest `[Image #N]` pasted into a conversation, sent or queued, so a resumed editor
/// numbers new pastes after it.
pub(crate) fn highest_pasted_image_id(
    message_contexts: &BTreeMap<usize, Vec<InputContext>>,
    pending_inputs: &[PendingInput],
) -> Option<u32> {
    message_contexts
        .values()
        .chain(pending_inputs.iter().map(|pending| &pending.contexts))
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

/// The inputs waiting for a turn, in the order they were queued: typed messages, Agent commands
/// and user Shells.
pub(crate) fn queued_inputs(
    pending_inputs: &[PendingInput],
    pending_commands: &[PendingCommand],
    user_shells: &[UserShellRecord],
) -> Vec<QueuedInput> {
    let mut entries = pending_inputs
        .iter()
        .filter(|pending| is_queued_user_message(pending))
        .map(|pending| QueuedInput {
            id: pending.receipt.run_id.uuid(),
            chat_id: pending.receipt.chat_id,
            loop_id: Some(pending.receipt.loop_id),
            kind: QueuedInputKind::Message,
            text: pending.input.clone(),
            contexts: pending.contexts.clone(),
            queued_sequence: pending.queued_sequence,
            armed: pending.armed,
        })
        .chain(pending_commands.iter().map(|command| QueuedInput {
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
            user_shells
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
