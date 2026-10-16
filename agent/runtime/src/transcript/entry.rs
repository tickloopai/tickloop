use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use tickloop_agent_protocol::{
    AgentMessageSource, DelegationMetadata, DirectedHumanSource, FamilyCompletion,
    FamilyInputRecord, Message, ModelIdentity, MonitorMembership, OriginalUserInput,
    PrivateHistorySource, Role, SubagentRegistration, ToolCallRecovery, Usage,
};

/// Transcript 中的一行。未知类型原样跳过，保证旧版本能读取新版本写入的文件。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum Line {
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
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MetadataKind {
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
    #[serde(other)]
    Other,
}
impl MetadataKind {
    pub(crate) fn is_chain(&self) -> bool {
        matches!(self, Self::User | Self::Assistant | Self::System)
    }
    pub(crate) fn is_retained(&self) -> bool {
        !self.is_chain() && !matches!(self, Self::Other)
    }
}
#[derive(Deserialize)]
pub(crate) struct MetadataHeader {
    #[serde(rename = "type")]
    pub(crate) kind: MetadataKind,
    #[serde(rename = "agentMessageOrigin")]
    pub(crate) agent_origin: Option<serde_json::Value>,
}

impl Line {
    pub(crate) fn chain(role: Role, entry: ChainEntry) -> Self {
        match role {
            Role::User => Self::User(entry),
            Role::Assistant => Self::Assistant(entry),
            Role::System => Self::System(entry),
        }
    }

    pub(crate) fn chain_entry(&self) -> Option<&ChainEntry> {
        match self {
            Self::User(entry) | Self::Assistant(entry) | Self::System(entry) => Some(entry),
            _ => None,
        }
    }

    pub(crate) fn is_conversation_message(&self) -> bool {
        matches!(self, Self::User(_) | Self::Assistant(_))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChainEntry {
    pub uuid: Uuid,
    pub parent_uuid: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_parent_uuid: Option<Uuid>,
    #[serde(default)]
    pub is_sidechain: bool,
    pub chat_id: Uuid,
    pub timestamp: String,
    pub cwd: PathBuf,
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
    pub summarize_metadata: Option<SummarizeMetadata>,
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
    /// Loop 条目（system，subtype 为 `loop_*`）记录的 Loop 完整状态；run id 记在 `runId`。
    /// 这类条目挂在对话链上，但不是对话消息，不进入发给模型的消息。
    #[serde(rename = "loop", default, skip_serializing_if = "Option::is_none")]
    pub loop_record: Option<LoopRecord>,
}

impl ChainEntry {
    pub(crate) fn is_compact_boundary(&self) -> bool {
        self.message.role == Role::System
            && self.subtype.as_deref() == Some(COMPACT_BOUNDARY_SUBTYPE)
    }

    pub(crate) fn is_loop_record(&self) -> bool {
        self.message.role == Role::System && self.loop_record.is_some()
    }
}

pub(crate) const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";
/// Loop 条目的 subtype。恢复时每条都按 Loop id 以完整状态覆盖，subtype 只说明写入原因。
pub(crate) const LOOP_CREATED_SUBTYPE: &str = "loop_created";
pub(crate) const LOOP_STATE_SUBTYPE: &str = "loop_state";
pub(crate) const LOOP_TRIGGER_SUBTYPE: &str = "loop_trigger";
pub(crate) const LOOP_ENDED_SUBTYPE: &str = "loop_ended";

/// Loop 在对话链上的完整状态。状态与 Trigger 按原始 JSON 保存，本模块不解析其结构。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopRecord {
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
    pub(crate) fn without_progress(&self) -> Self {
        Self {
            progress: None,
            ..self.clone()
        }
    }
}

/// 照 Claude Code `compactMetadata`。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactMetadata {
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
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CompactTriggerEntry {
    Manual,
    Auto,
}

/// 照 Claude Code `preservedSegment`：保留的消息不重写，加载时据此重接父指针。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreservedSegmentEntry {
    pub head_uuid: Uuid,
    pub anchor_uuid: Uuid,
    pub tail_uuid: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SummarizeMetadata {
    pub messages_summarized: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
    pub direction: PartialCompactDirection,
}

/// 照 Claude Code `PartialCompactDirection`。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PartialCompactDirection {
    From,
    UpTo,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CustomTitleEntry {
    pub chat_id: Uuid,
    pub custom_title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiTitleEntry {
    pub chat_id: Uuid,
    pub ai_title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubagentsEntry {
    pub chat_id: Uuid,
    pub subagents: Vec<SubagentRegistration>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastPromptEntry {
    pub chat_id: Uuid,
    pub last_prompt: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BranchOriginEntry {
    pub chat_id: Uuid,
    pub forked_from: ForkedFrom,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DelegationEntry {
    pub chat_id: Uuid,
    pub delegation: DelegationMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkedFrom {
    pub chat_id: Uuid,
    #[serde(default)]
    pub message_uuid: Option<Uuid>,
}

/// 照 Claude Code `file-history-snapshot`：`message_id` 是关联的链上消息，更新条目
/// （`is_snapshot_update`）以发起编辑的消息为键，替换 `snapshot.messageId` 对应的快照。
/// 快照本体按原始 JSON 保存，本模块不解析其结构。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileHistorySnapshotEntry {
    pub message_id: Uuid,
    pub snapshot: serde_json::Value,
    pub is_snapshot_update: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContentReplacementEntry {
    pub chat_id: Uuid,
    pub replacements: Vec<ContentReplacementRecord>,
}

/// Claude Code `ContentReplacementRecord`：`replacement` 是模型看到的原文，恢复时直接套用，
/// 不按当前代码重新生成。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum ContentReplacementRecord {
    #[serde(rename_all = "camelCase")]
    ToolResult {
        tool_use_id: String,
        replacement: String,
    },
    #[serde(other)]
    Unknown,
}

impl ContentReplacementRecord {
    pub(crate) fn tool_use_id(&self) -> &str {
        match self {
            Self::ToolResult { tool_use_id, .. } => tool_use_id,
            Self::Unknown => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_line_types_are_accepted_and_ignored() {
        let line: Line = serde_json::from_str(r#"{"type":"future-kind","value":1}"#).unwrap();
        assert!(matches!(line, Line::Unknown));
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactTicksEntry {
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
pub(crate) struct MonitorMembersEntry {
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    pub monitor: MonitorMembership,
}

impl MonitorMembersEntry {
    pub(crate) fn materializes_chat(&self) -> bool {
        self.cwd.is_some() && !self.monitor.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FamilyCompletionsEntry {
    pub chat_id: Uuid,
    pub completions: Vec<FamilyCompletion>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FamilyInputsEntry {
    pub chat_id: Uuid,
    pub inputs: std::collections::BTreeMap<Uuid, FamilyInputRecord>,
}

#[cfg(test)]
mod record_type_tests {
    use super::MetadataHeader;
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SentAgentMessagesEntry {
    pub chat_id: Uuid,
    pub messages: std::collections::BTreeMap<Uuid, AgentMessageSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskBoardsEntry {
    pub chat_id: Uuid,
    pub boards: std::collections::BTreeMap<Uuid, tickloop_agent_protocol::TaskBoard>,
}
