use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use tickloop_agent_protocol::{
    AgentMessageSource, DelegationMetadata, DirectedHumanSource, FamilyCompletion,
    FamilyInputRecord, Message, ModelIdentity, MonitorMembership, OriginalUserInput,
    PrivateHistorySource, QuestionRequest, Role, SubagentRegistration, ToolCallRecovery,
    ToolResult, ToolResultContent, Usage,
};

/// Transcript 中的一行。未知类型原样保留并原样写回，保证旧版本能读取、也不丢掉新版本写入的记录。
/// A store keeps each Line as its JSON; the local store writes one per line of a JSONL file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Line {
    User(ChainEntry),
    Assistant(ChainEntry),
    System(ChainEntry),
    ArtifactTicks(ArtifactTicksEntry),
    MonitorMembers(MonitorMembersEntry),
    CustomTitle(CustomTitleEntry),
    AiTitle(AiTitleEntry),
    Subagents(SubagentsEntry),
    TaskBoards(TaskBoardsEntry),
    FamilyCompletions(FamilyCompletionsEntry),
    FamilyInputs(FamilyInputsEntry),
    SentAgentMessages(SentAgentMessagesEntry),
    LastPrompt(LastPromptEntry),
    BranchOrigin(BranchOriginEntry),
    Delegation(DelegationEntry),
    /// 照 Claude Code：文件回滚快照，按消息 id 关联到链上消息。
    FileHistorySnapshot(FileHistorySnapshotEntry),
    /// 照 Claude Code：大工具结果的替换决定，恢复时重建，同一 Chat 的条目逐条累积。
    ContentReplacement(ContentReplacementEntry),
    /// An answer's lifecycle; the latest record of a response id wins.
    Response(ResponseEntry),
    /// The input ledger; the latest record of an input id wins.
    Input(InputEntry),
    /// A Run's durable wait; the latest record of a wait id wins.
    Wait(WaitEntry),
    /// The retained state of the Chat as of this Line: loading it stands for every retained Line
    /// before it.
    RetainedSnapshot(RetainedSnapshotEntry),
    /// A Line of a kind this version does not know, kept as it was written.
    #[serde(untagged)]
    Unknown(UnknownLine),
}

/// A Line of a kind this version does not know: its `type` and its whole JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct UnknownLine {
    pub kind: String,
    pub value: serde_json::Value,
}

impl Serialize for UnknownLine {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UnknownLine {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let kind = value["type"]
            .as_str()
            .ok_or_else(|| serde::de::Error::custom("a Line has a type"))?
            .to_owned();
        // A known kind that does not parse is a broken Line, not an unknown one.
        if serde_json::from_value::<MetadataKind>(serde_json::Value::String(kind.clone()))
            .is_ok_and(|known| known != MetadataKind::Other)
        {
            return Err(serde::de::Error::custom(format!("invalid {kind} Line")));
        }
        Ok(Self { kind, value })
    }
}

/// The retained Lines that stand for the retained state of a Chat: the latest of each kind
/// whose latest Line wins, the records by id still needed, the live accumulating Lines, and the
/// newest Lines of each unknown kind.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetainedSnapshotEntry {
    pub chat_id: Uuid,
    pub lines: Vec<Line>,
}

/// How a load keeps a kind of Line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retention {
    /// The conversation chain, read from the latest compact boundary.
    Chain,
    /// The latest Line of the kind stands for the Chat.
    ChatLatest,
    /// The latest Line of each id wins.
    LatestById,
    /// Every Line still adds to the Chat's state.
    Accumulating,
    /// Kept as written, the newest few of each kind.
    Unknown,
    /// Stands for every retained Line before it.
    Snapshot,
}

/// The kind of a Line read from its top-level `type` alone, without parsing the rest of it.
/// [`Line::kind`] maps every Line variant to the kind of the same name.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum MetadataKind {
    User,
    Assistant,
    System,
    CustomTitle,
    AiTitle,
    LastPrompt,
    BranchOrigin,
    Delegation,
    ArtifactTicks,
    MonitorMembers,
    Subagents,
    FamilyInputs,
    SentAgentMessages,
    TaskBoards,
    FamilyCompletions,
    FileHistorySnapshot,
    ContentReplacement,
    Response,
    Input,
    Wait,
    RetainedSnapshot,
    #[serde(other)]
    Other,
}
impl MetadataKind {
    /// Every kind decides here how a load keeps it; a new kind must be classified.
    pub fn retention(&self) -> Retention {
        match self {
            Self::User | Self::Assistant | Self::System => Retention::Chain,
            Self::CustomTitle
            | Self::AiTitle
            | Self::LastPrompt
            | Self::BranchOrigin
            | Self::Delegation
            | Self::ArtifactTicks
            | Self::MonitorMembers
            | Self::Subagents
            | Self::FamilyInputs
            | Self::SentAgentMessages
            | Self::TaskBoards
            | Self::FamilyCompletions => Retention::ChatLatest,
            Self::Response | Self::Input | Self::Wait => Retention::LatestById,
            Self::FileHistorySnapshot | Self::ContentReplacement => Retention::Accumulating,
            Self::RetainedSnapshot => Retention::Snapshot,
            Self::Other => Retention::Unknown,
        }
    }
    pub fn is_chain(&self) -> bool {
        self.retention() == Retention::Chain
    }
    pub fn is_retained(&self) -> bool {
        !self.is_chain()
    }
}
/// The fields of a Line a store reads before, or instead of, parsing the whole Line.
#[derive(Debug, Deserialize)]
pub struct MetadataHeader {
    #[serde(rename = "type")]
    pub kind: MetadataKind,
    #[serde(rename = "agentMessageOrigin")]
    pub agent_origin: Option<serde_json::Value>,
}

impl Line {
    pub fn chain(role: Role, entry: ChainEntry) -> Self {
        match role {
            Role::User => Self::User(entry),
            Role::Assistant => Self::Assistant(entry),
            Role::System => Self::System(entry),
        }
    }

    pub fn chain_entry(&self) -> Option<&ChainEntry> {
        match self {
            Self::User(entry) | Self::Assistant(entry) | Self::System(entry) => Some(entry),
            _ => None,
        }
    }

    pub fn is_conversation_message(&self) -> bool {
        matches!(self, Self::User(_) | Self::Assistant(_))
    }

    /// Chat-level records outside the conversation chain. A load from the latest compact
    /// boundary keeps the ones written before it.
    pub fn is_retained(&self) -> bool {
        self.kind().is_retained()
    }

    pub fn kind(&self) -> MetadataKind {
        match self {
            Self::User(_) => MetadataKind::User,
            Self::Assistant(_) => MetadataKind::Assistant,
            Self::System(_) => MetadataKind::System,
            Self::ArtifactTicks(_) => MetadataKind::ArtifactTicks,
            Self::MonitorMembers(_) => MetadataKind::MonitorMembers,
            Self::CustomTitle(_) => MetadataKind::CustomTitle,
            Self::AiTitle(_) => MetadataKind::AiTitle,
            Self::Subagents(_) => MetadataKind::Subagents,
            Self::TaskBoards(_) => MetadataKind::TaskBoards,
            Self::FamilyCompletions(_) => MetadataKind::FamilyCompletions,
            Self::FamilyInputs(_) => MetadataKind::FamilyInputs,
            Self::SentAgentMessages(_) => MetadataKind::SentAgentMessages,
            Self::LastPrompt(_) => MetadataKind::LastPrompt,
            Self::BranchOrigin(_) => MetadataKind::BranchOrigin,
            Self::Delegation(_) => MetadataKind::Delegation,
            Self::FileHistorySnapshot(_) => MetadataKind::FileHistorySnapshot,
            Self::ContentReplacement(_) => MetadataKind::ContentReplacement,
            Self::Response(_) => MetadataKind::Response,
            Self::Input(_) => MetadataKind::Input,
            Self::Wait(_) => MetadataKind::Wait,
            Self::RetainedSnapshot(_) => MetadataKind::RetainedSnapshot,
            Self::Unknown(_) => MetadataKind::Other,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainEntry {
    pub uuid: Uuid,
    pub parent_uuid: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_parent_uuid: Option<Uuid>,
    #[serde(default)]
    pub is_sidechain: bool,
    pub chat_id: Uuid,
    pub timestamp: String,
    /// The Chat's Folder. A Chat without a Folder writes none, and a chain whose last entry has
    /// none restores as a Chat without a Folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invoked_skills: Vec<tickloop_agent_protocol::InvokedSkill>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_meta: bool,
    /// Tickloop extension: marks a host note, the host tool names announced as available once
    /// it was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_tools: Option<std::collections::BTreeSet<String>>,
    /// Tickloop extension: marks the host note telling the model that Plan mode ended, Claude
    /// Code's `plan_mode_exit` attachment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_mode_exit: bool,
    /// The execution mode at a user checkpoint stays raw so an unknown legacy value cannot
    /// invalidate the entire Transcript entry.
    #[serde(
        default,
        alias = "permissionMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub execution_mode: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_inherited_prompt: bool,
    #[serde(
        default,
        alias = "originalUserMessageId",
        skip_serializing_if = "Option::is_none"
    )]
    pub original_message_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message_origin: Option<serde_json::Value>,
    pub human_family_origin: Option<DirectedHumanSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_recoveries: Vec<ToolCallRecovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<serde_json::Value>,
    /// Tickloop 扩展：同一条内存消息拆成多个条目时（向结果消息追加的并行工具结果），后续条目
    /// 标记为接续父条目所在的消息，加载时合并回同一条消息。Claude Code 的每个工具结果本来就是
    /// 独立的 user 消息，Tickloop 在内存中把并行结果合在一条消息里，所以保留这个字段。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub continues_message: bool,
    /// 照 Claude Code system 条目的 `subtype`；压缩边界为 [`COMPACT_BOUNDARY_SUBTYPE`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_metadata: Option<CompactMetadata>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_compact_summary: bool,
    /// 照 Claude Code：定向摘要保留了消息时，摘要消息记录被摘要的消息数、用户补充说明和方向。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarize_metadata: Option<SummarizeMetadataEntry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_visible_in_transcript_only: bool,
    /// assistant 条目的用量（Claude Code 记在 `message.usage`，Tickloop 的消息结构没有该字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// assistant 条目生成该消息的模型（Claude Code 记在 `message.model`，Tickloop 的消息结构
    /// 没有该字段）。恢复时据此判断历史中的私有推理是否来自当前模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_source: Option<PrivateHistorySource>,
    /// 条目所属 run 的 Loop（有 `runId` 时记录）。恢复时据此把计划与进度工具调用归到各自的 Loop。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_id: Option<Uuid>,
    /// 照 Claude Code 消息的 `slug`：Chat 的计划文件名前缀，生成之后写在每个条目上；
    /// 恢复时按链上第一个 slug 关联计划文件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// The answer this entry is part of: its assistant messages, tool results, interruption
    /// marker and the prompts merged into it at a tool boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<Uuid>,
    /// The ledger input this user prompt entered with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<Uuid>,
    /// Loop 条目（system，subtype 为 `loop_*`）记录的 Loop 完整状态；run id 记在 `runId`。
    /// 这类条目挂在对话链上，但不是对话消息，不进入发给模型的消息。
    #[serde(rename = "loop", default, skip_serializing_if = "Option::is_none")]
    pub loop_record: Option<LoopRecord>,
}

impl ChainEntry {
    pub fn is_compact_boundary(&self) -> bool {
        self.message.role == Role::System
            && self.subtype.as_deref() == Some(COMPACT_BOUNDARY_SUBTYPE)
    }

    /// A compact boundary the conversation chain starts again at, which a load from the latest
    /// boundary may begin with: no parent, and no preserved segment before it.
    pub fn restarts_chain(&self) -> bool {
        self.is_compact_boundary()
            && self.parent_uuid.is_none()
            && self
                .compact_metadata
                .as_ref()
                .is_none_or(|metadata| metadata.preserved_segment.is_none())
    }

    pub fn is_loop_record(&self) -> bool {
        self.message.role == Role::System && self.loop_record.is_some()
    }
}

pub const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";
/// Loop 条目的 subtype。恢复时每条都按 Loop id 以完整状态覆盖，subtype 只说明写入原因。
pub const LOOP_CREATED_SUBTYPE: &str = "loop_created";
pub const LOOP_STATE_SUBTYPE: &str = "loop_state";
pub const LOOP_TRIGGER_SUBTYPE: &str = "loop_trigger";
pub const LOOP_ENDED_SUBTYPE: &str = "loop_ended";

/// Loop 在对话链上的完整状态。状态与 Trigger 按原始 JSON 保存，本模块不解析其结构。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LoopRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_report: Option<tickloop_agent_protocol::SubagentReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_events: Vec<serde_json::Value>,
    pub loop_id: Uuid,
    pub goal: String,
    pub state: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<serde_json::Value>,
    /// Trigger 已解除武装、等待用户向该 Loop 提交输入时重新武装（重启恢复或 Branch 复制后）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trigger_awaits_resume: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_sources: Vec<serde_json::Value>,
    /// The submitted version is durable before the approval tool receives its result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_version: Option<u64>,
    #[serde(default)]
    pub plan_approval_pending: bool,
    /// 与 `plan_version` 同时写出的进度步骤（原始 JSON）；链上之后的进度工具调用覆盖它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<serde_json::Value>,
    /// Unix milliseconds when the runtime moved the Loop into `state`, kept as raw JSON so an
    /// unreadable value leaves the time unknown instead of dropping the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<serde_json::Value>,
    pub run_started_at: Option<u64>,
}

impl LoopRecord {
    /// Progress remains a separate checkpoint; plan changes are part of the Loop state.
    pub fn without_progress(&self) -> Self {
        Self {
            progress: None,
            ..self.clone()
        }
    }
}

/// 照 Claude Code `compactMetadata`。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompactMetadata {
    pub trigger: CompactTriggerEntry,
    pub pre_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_summarized: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_segment: Option<PreservedSegmentEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_user_inputs: Option<Vec<OriginalUserInput>>,
    /// The display node the Chat showed as current when it was compacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_leaf: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CompactTriggerEntry {
    Manual,
    Auto,
}

/// 照 Claude Code `preservedSegment`：保留的消息不重写，加载时据此重接父指针。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreservedSegmentEntry {
    pub head_uuid: Uuid,
    pub anchor_uuid: Uuid,
    pub tail_uuid: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SummarizeMetadataEntry {
    pub messages_summarized: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
    pub direction: PartialCompactDirection,
}

/// 照 Claude Code `PartialCompactDirection`。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PartialCompactDirection {
    From,
    UpTo,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomTitleEntry {
    pub chat_id: Uuid,
    pub custom_title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTitleEntry {
    pub chat_id: Uuid,
    pub ai_title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentsEntry {
    pub chat_id: Uuid,
    pub subagents: Vec<SubagentRegistration>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LastPromptEntry {
    pub chat_id: Uuid,
    pub last_prompt: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BranchOriginEntry {
    pub chat_id: Uuid,
    pub forked_from: ForkedFrom,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationEntry {
    pub chat_id: Uuid,
    pub delegation: DelegationMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ForkedFrom {
    pub chat_id: Uuid,
    #[serde(default)]
    pub message_uuid: Option<Uuid>,
}

/// 照 Claude Code `file-history-snapshot`：`message_id` 是关联的链上消息，更新条目
/// （`is_snapshot_update`）以发起编辑的消息为键，替换 `snapshot.messageId` 对应的快照。
/// 快照本体按原始 JSON 保存，本模块不解析其结构。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistorySnapshotEntry {
    pub message_id: Uuid,
    pub snapshot: serde_json::Value,
    pub is_snapshot_update: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContentReplacementEntry {
    pub chat_id: Uuid,
    pub replacements: Vec<ContentReplacementRecord>,
}

/// Claude Code `ContentReplacementRecord`：`replacement` 是模型看到的原文，恢复时直接套用，
/// 不按当前代码重新生成。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ContentReplacementRecord {
    #[serde(rename_all = "camelCase")]
    ToolResult {
        tool_use_id: String,
        replacement: String,
    },
    #[serde(other)]
    Unknown,
}

impl ContentReplacementRecord {
    pub fn tool_use_id(&self) -> &str {
        match self {
            Self::ToolResult { tool_use_id, .. } => tool_use_id,
            Self::Unknown => "",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactTicksEntry {
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    pub artifact_ticks: Vec<tickloop_agent_protocol::ArtifactTick>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub removed_artifact_ticks: std::collections::BTreeSet<String>,
}

/// The latest Monitor membership of one Chat. An empty Chat with a working directory and members
/// materializes its Transcript so the membership survives a restart.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorMembersEntry {
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    pub monitor: MonitorMembership,
}

impl MonitorMembersEntry {
    pub fn materializes_chat(&self) -> bool {
        self.cwd.is_some() && !self.monitor.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyCompletionsEntry {
    pub chat_id: Uuid,
    pub completions: Vec<FamilyCompletion>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyInputsEntry {
    pub chat_id: Uuid,
    pub inputs: std::collections::BTreeMap<Uuid, FamilyInputRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SentAgentMessagesEntry {
    pub chat_id: Uuid,
    pub messages: std::collections::BTreeMap<Uuid, AgentMessageSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskBoardsEntry {
    pub chat_id: Uuid,
    pub boards: std::collections::BTreeMap<Uuid, tickloop_agent_protocol::TaskBoard>,
}

/// One answer: one attempt of one Run. Written when it starts and again when it ends.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResponseEntry {
    pub response_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<Uuid>,
    pub attempt: u32,
    pub run_id: Uuid,
    pub loop_id: Uuid,
    /// The display id of the prompt the answer belongs under: the latest prompt when it started.
    pub display_parent: Option<Uuid>,
    pub timestamp: String,
    pub state: ResponseState,
    /// It ended without a message of its own, so no chain entry stands for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub without_messages: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResponseState {
    Started,
    /// Its Run waits durably: a [`WaitEntry`] of the answer says on what and where it continues.
    Suspended,
    /// Its Run continued after a wait, in the change that put the wait's result on the chain.
    Resumed,
    Ended {
        ending: ResponseEnding,
    },
}

impl ResponseState {
    pub fn is_ended(&self) -> bool {
        matches!(self, Self::Ended { .. })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResponseEnding {
    Completed,
    Interrupted { by: InterruptedBy },
    Failed { error: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InterruptedBy {
    /// The user interrupted or stopped it.
    User,
    /// The host went away, as on shutdown or a crash; the input may be continued.
    Host,
    /// A voluntary handover's host ended it at its ceiling; the input may be continued.
    Handover,
}

/// One ledger record of an input. Every record repeats what was accepted, so the latest record
/// of an input id is enough on its own.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InputEntry {
    pub input_id: Uuid,
    pub op: InputOp,
    /// SHA-256 of the input's payload, hex encoded; reusing the id with another payload is
    /// refused.
    pub digest: String,
    /// The display node the input was accepted under; `None` when it joins the conversation
    /// wherever it ends when the input starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_parent: Option<Uuid>,
    pub loop_id: Uuid,
    pub run_id: Uuid,
    pub timestamp: String,
    pub state: InputState,
    /// While `Accepted`: the queued input as it waits, enough to queue it again after a load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InputOp {
    /// A prompt continuing the conversation.
    Continue,
    /// A user Skill invocation.
    Skill,
    /// A prompt in place of an earlier one, as its sibling.
    ReplacePrompt,
    /// Another answer to the prompt of an earlier answer, as its sibling.
    RegenerateAnswer,
    /// The user's reply to a question a Run waits on; it joins that Run.
    Answer,
    /// A Trigger, Tick update or other source that continues a Loop.
    Trigger,
    /// An @tickloop request on an Artifact.
    Feedback,
    /// A message from another Agent of the family, such as a completion notification.
    AgentMessage,
    /// A goal the model registered with StartGoal.
    Goal,
}

impl InputOp {
    /// The host or the user sends it under its own id, which repeating it must find again; the
    /// others come from the Agent itself.
    pub fn is_external(&self) -> bool {
        matches!(
            self,
            Self::Continue
                | Self::Skill
                | Self::ReplacePrompt
                | Self::RegenerateAnswer
                | Self::Answer
                | Self::Feedback
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InputState {
    Accepted,
    /// Its Run started. Only an input whose prompt does not carry its id records this.
    Started,
    Settled {
        outcome: SettledOutcome,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SettledOutcome {
    Completed {
        usage: Usage,
    },
    Interrupted {
        by: InterruptedBy,
    },
    Failed {
        error: String,
    },
    /// Answered by the Run `into`, which it joined before or while that Run went on.
    Merged {
        into: Uuid,
    },
    /// Taken back from the queue before it started.
    Withdrawn,
    Discarded {
        reason: DiscardReason,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiscardReason {
    /// Its Loop ended before the input started.
    LoopEnded,
    /// Its Loop was stopped while it waited.
    Stopped,
}

/// A Run's durable wait: a point in the Run that only an outside event ends, and where the Run
/// continues once it ends.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitEntry {
    /// A question's wait id is its request id.
    pub wait_id: Uuid,
    pub chat_id: Uuid,
    pub loop_id: Uuid,
    pub run_id: Uuid,
    pub response_id: Uuid,
    pub kind: WaitKind,
    pub resume: ResumePoint,
    pub timestamp: String,
    pub state: WaitState,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WaitKind {
    /// An `AskUserQuestion` call waits for the user's reply.
    #[serde(rename_all = "camelCase")]
    Question {
        request: QuestionRequest,
        /// The call's operation, which a rebuilt Run restores so a cancellation reads as it would
        /// have in the process that asked.
        operation_id: Uuid,
    },
}

/// Where a Run continues.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResumePoint {
    /// In the tool batch of the assistant message whose canonical (earliest source) id is
    /// `assistant_message`, at its call `call_id`; the calls after it run next, then the end of
    /// the batch. Re-entry fails when the loaded chain does not hold that message.
    #[serde(rename_all = "camelCase")]
    InBatch {
        assistant_message: Uuid,
        call_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WaitState {
    Open,
    /// The wait ended with `result`, which is not on the chain yet; `by_input` is the ledger input
    /// that ended it.
    #[serde(rename_all = "camelCase")]
    Resolved {
        result: ModelVisibleResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by_input: Option<Uuid>,
    },
    /// The result is on the chain, written in the same change as the answer's `Resumed`.
    Appended,
    Abandoned {
        reason: AbandonReason,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AbandonReason {
    /// The Run was interrupted, stopped or failed while it waited.
    Interrupted,
    /// The host that held the Run went away and its successor does not rebuild waits.
    HostLost,
}

/// A call's result as the conversation records it, decided when the wait on the call ends.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelVisibleResult {
    /// The tool's own result, which its operation record and `ToolCompleted` carry.
    pub tool: ToolResult,
    /// What the model sees: the tool's text with any approval comment, then its images.
    pub content: ToolResultContent,
    /// The Run ends after this result, as it does after a bare user denial, a stopped foreground
    /// command or a declined question.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ends_run: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_line_types_are_kept_as_written() {
        let raw = r#"{"type":"future-kind","value":1}"#;
        let line: Line = serde_json::from_str(raw).unwrap();
        assert!(matches!(&line, Line::Unknown(unknown) if unknown.kind == "future-kind"));
        assert_eq!(line.kind(), MetadataKind::Other);
        assert!(line.is_retained());
        assert_eq!(
            serde_json::to_value(&line).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }

    #[test]
    fn a_broken_line_of_a_known_kind_is_not_unknown() {
        assert!(serde_json::from_str::<Line>(r#"{"type":"user","uuid":7}"#).is_err());
    }

    /// A store classifying a written Line by its header alone reads the kind the Line has.
    #[test]
    fn the_header_of_a_written_line_reads_as_its_kind() {
        let chat_id = Uuid::nil();
        let user: ChainEntry = serde_json::from_value(serde_json::json!({
            "uuid": Uuid::nil(),
            "parentUuid": null,
            "chatId": chat_id,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "test",
            "message": {"role": "user", "content": []},
            "humanFamilyOrigin": null,
        }))
        .unwrap();
        let lines = [
            Line::User(user.clone()),
            Line::Assistant(user.clone()),
            Line::System(user),
            Line::ArtifactTicks(ArtifactTicksEntry {
                chat_id,
                cwd: None,
                artifact_ticks: Vec::new(),
                removed_artifact_ticks: Default::default(),
            }),
            Line::MonitorMembers(MonitorMembersEntry {
                chat_id,
                cwd: None,
                monitor: Default::default(),
            }),
            Line::CustomTitle(CustomTitleEntry {
                chat_id,
                custom_title: "title".into(),
            }),
            Line::AiTitle(AiTitleEntry {
                chat_id,
                ai_title: "title".into(),
            }),
            Line::Subagents(SubagentsEntry {
                chat_id,
                subagents: Vec::new(),
            }),
            Line::TaskBoards(TaskBoardsEntry {
                chat_id,
                boards: Default::default(),
            }),
            Line::FamilyCompletions(FamilyCompletionsEntry {
                chat_id,
                completions: Vec::new(),
            }),
            Line::FamilyInputs(FamilyInputsEntry {
                chat_id,
                inputs: Default::default(),
            }),
            Line::SentAgentMessages(SentAgentMessagesEntry {
                chat_id,
                messages: Default::default(),
            }),
            Line::LastPrompt(LastPromptEntry {
                chat_id,
                last_prompt: "prompt".into(),
            }),
            Line::BranchOrigin(BranchOriginEntry {
                chat_id,
                forked_from: ForkedFrom {
                    chat_id,
                    message_uuid: None,
                },
            }),
            Line::FileHistorySnapshot(FileHistorySnapshotEntry {
                message_id: Uuid::nil(),
                snapshot: serde_json::json!({}),
                is_snapshot_update: false,
            }),
            Line::ContentReplacement(ContentReplacementEntry {
                chat_id,
                replacements: Vec::new(),
            }),
            Line::Response(ResponseEntry {
                response_id: Uuid::nil(),
                input_id: None,
                attempt: 1,
                run_id: Uuid::nil(),
                loop_id: Uuid::nil(),
                display_parent: None,
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                state: ResponseState::Started,
                without_messages: false,
            }),
            Line::Input(InputEntry {
                input_id: Uuid::nil(),
                op: InputOp::Continue,
                digest: String::new(),
                display_parent: None,
                loop_id: Uuid::nil(),
                run_id: Uuid::nil(),
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                state: InputState::Accepted,
                queued: None,
            }),
            Line::Wait(WaitEntry {
                wait_id: Uuid::nil(),
                chat_id,
                loop_id: Uuid::nil(),
                run_id: Uuid::nil(),
                response_id: Uuid::nil(),
                kind: WaitKind::Question {
                    request: QuestionRequest {
                        id: Uuid::nil(),
                        chat_id,
                        loop_id: Uuid::nil(),
                        run_id: Uuid::nil(),
                        call_id: "call".into(),
                        questions: Vec::new(),
                    },
                    operation_id: Uuid::nil(),
                },
                resume: ResumePoint::InBatch {
                    assistant_message: Uuid::nil(),
                    call_id: "call".into(),
                },
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                state: WaitState::Open,
            }),
        ];
        for line in lines {
            let raw = serde_json::to_vec(&line).unwrap();
            let header = serde_json::from_slice::<MetadataHeader>(&raw).unwrap();
            assert_eq!(header.kind, line.kind());
        }
    }

    fn is_chain(raw: &[u8]) -> bool {
        serde_json::from_slice::<MetadataHeader>(raw)
            .unwrap()
            .kind
            .is_chain()
    }

    #[test]
    fn chain_classification_uses_the_top_level_type_independently_of_field_order() {
        assert!(is_chain(br#"{"message":{},"type":"user"}"#));
        assert!(is_chain(br#"{"type":"assistant","message":{}}"#));
        assert!(is_chain(br#"{"message":{},"type":"\u0075ser"}"#));
        assert!(!is_chain(
            br#"{"inputs":{"example":{"type":"user"}},"type":"family-inputs"}"#
        ));
    }
}
