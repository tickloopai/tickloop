use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ChatId, ChatSnapshot, ExecutionMode, InputContext, LoopId, Message, ModelConfig,
    OriginalUserInput, RunId,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchRequest {
    pub request_id: Uuid,
    pub source_chat_id: ChatId,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchSessionPermissionRegistration {
    pub source_chat_id: ChatId,
    pub source_loop_id: LoopId,
    pub child_chat_id: ChatId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BranchPublicSource {
    pub chat_id: ChatId,
    pub run_id: Option<RunId>,
    pub message_id: Option<Uuid>,
    #[serde(default)]
    pub is_agent_message: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchPublicItem {
    Message {
        source: BranchPublicSource,
        message: Message,
        contexts: Vec<InputContext>,
        #[serde(default)]
        has_thinking: bool,
    },
    Summary {
        source: BranchPublicSource,
        direction: Option<SummaryDirection>,
        summary: String,
    },
}

/// Branch 的来源。来源 Chat 与名称随 Transcript 保存（`branch-origin` 与 `custom-title` 元数据）；
/// 创建请求与复制种子只在创建 Branch 的进程内存中。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchOrigin {
    pub source_chat_id: ChatId,
    pub name: String,
    /// 从 Transcript 恢复的 Branch 没有这部分，公开对话按子 Chat 自身的消息投影。
    #[serde(default)]
    pub creation: Option<BranchCreation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchCreation {
    pub source_loop_id: LoopId,
    pub request_id: Uuid,
    pub requested_name: Option<String>,
    pub source_raw_boundary: usize,
    pub source_public_boundary: usize,
    pub source_fingerprint: String,
    pub created_at_ms: u64,
    pub model_seed_message_count: usize,
    /// 复制对话在子 Chat 中的起始位置。向前保留的定向摘要在保留消息之前插入压缩边界，
    /// 复制对话随之后移。
    #[serde(default)]
    pub model_seed_start: usize,
    pub public_seed: Vec<BranchPublicItem>,
    pub session_permissions: BranchSessionPermissionRegistration,
}

impl BranchCreation {
    /// 截断或压缩拆散了复制种子：之后公开对话按子 Chat 自身的消息投影。
    pub fn clear_seed(&mut self) {
        self.model_seed_message_count = 0;
        self.model_seed_start = 0;
        self.source_public_boundary = 0;
        self.public_seed.clear();
    }
}

#[derive(Clone, Debug)]
pub struct BranchOutcome {
    pub source_chat_id: ChatId,
    pub child_chat_id: ChatId,
    pub name: String,
    pub snapshot: ChatSnapshot,
    pub session_permissions: BranchSessionPermissionRegistration,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RewindMode {
    Code,
    Conversation,
    CodeAndConversation,
}

impl RewindMode {
    pub fn restores_code(self) -> bool {
        self != Self::Conversation
    }

    pub fn restores_conversation(self) -> bool {
        self != Self::Code
    }
}

/// rewind 与定向摘要可以选择的位置：当前对话中的一条用户提示。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RewindPoint {
    /// 提示在 Transcript 中的 uuid，也是文件回滚快照关联的消息 id。
    pub message_id: Uuid,
    pub message_index: usize,
    pub prompt: String,
    pub contexts: Vec<InputContext>,
    /// The execution mode captured when this prompt was submitted.
    pub execution_mode: ExecutionMode,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RewindDraft {
    pub prompt: String,
    pub contexts: Vec<InputContext>,
}

/// A prompt an interrupt took back from the conversation, to edit and send again.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TakenBackPrompt {
    pub prompt: String,
    pub contexts: Vec<InputContext>,
}

/// 恢复代码的结果。照 Claude Code，单个文件失败时跳过该文件继续；找不到目标提示的快照时
/// 不改动任何文件，`error` 说明原因。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CodeRewind {
    pub changed_files: Vec<PathBuf>,
    pub failed_files: Vec<(PathBuf, String)>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RewindOutcome {
    pub snapshot: ChatSnapshot,
    pub mode: RewindMode,
    pub code: Option<CodeRewind>,
    pub conversation_restored: bool,
    pub draft: Option<RewindDraft>,
}

/// 照 Claude Code `createCompactBoundaryMessage` 的 `content`。
pub const COMPACT_BOUNDARY_TEXT: &str = "Conversation compacted";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Manual,
    Auto,
}

/// 照 Claude Code `compactMetadata`。Tickloop 不写 `preservedSegment`。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompactBoundary {
    pub trigger: CompactTrigger,
    pub pre_tokens: usize,
    pub user_context: Option<String>,
    pub messages_summarized: Option<usize>,
    /// 边界被之后的 Summarize from here 作为保留消息带到新边界之后。这样的边界在 Transcript 中
    /// 接在前一条消息后面，不再是对话链的起点。
    #[serde(default)]
    pub preserved_by_later_summary: bool,
    /// Host-copied original prompts from the active chain, never model summaries.
    #[serde(default)]
    pub original_user_inputs: Option<Vec<OriginalUserInput>>,
    /// 压缩时的模型，只在本进程内有效：rewind 越过边界重新暴露私有推理时用来判断兼容性。
    /// 恢复后边界之前的消息不在内存中，无需保存。
    #[serde(skip)]
    pub model: Option<Box<ModelConfig>>,
}

/// 照 Claude Code `summarizeMetadata`。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SummarizeMetadata {
    pub messages_summarized: usize,
    pub user_context: Option<String>,
    pub direction: SummaryDirection,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum CompactionMarker {
    Boundary(CompactBoundary),
    /// 摘要保留了消息时带 `summarize_metadata`，否则照 Claude Code 只在完整记录中显示。
    Summary {
        summarize_metadata: Option<SummarizeMetadata>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryDirection {
    FromHere,
    UpToHere,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DirectedSummaryRequest {
    /// 选中的用户提示，见 [`RewindPoint::message_id`]。
    pub message_id: Uuid,
    pub direction: SummaryDirection,
    pub guidance: Option<String>,
}

/// 模型可见对话中的一项：普通消息或压缩摘要。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ActiveConversationItem {
    RawMessage {
        message_index: usize,
    },
    Summary {
        message_index: usize,
        direction: Option<SummaryDirection>,
        summary: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DirectedSummaryDraft {
    pub prompt: String,
    pub contexts: Vec<InputContext>,
}

#[derive(Clone, Debug)]
pub struct DirectedSummaryOutcome {
    pub snapshot: ChatSnapshot,
    pub direction: SummaryDirection,
    /// 模型给出的摘要文字。
    pub summary: String,
    pub guidance: Option<String>,
    pub messages_summarized: usize,
    pub draft: Option<DirectedSummaryDraft>,
    pub continuation_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum CompactionStatus {
    #[default]
    Idle,
    Running,
    Completed,
    Failed,
}

/// 全量压缩的结果。
#[derive(Clone, Debug)]
pub struct CompactionOutcome {
    /// 模型给出的摘要文字。
    pub summary: String,
    pub snapshot: ChatSnapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelChange {
    Applied,
    Pending,
}
