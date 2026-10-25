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

/// Where a Branch came from. The source Chat and the name are saved with the Transcript
/// (`branch-origin` and `custom-title` metadata); the creation request and the copy seed exist only
/// in the memory of the process that created the Branch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchOrigin {
    pub source_chat_id: ChatId,
    pub name: String,
    /// A Branch restored from the Transcript has no such part; its public conversation is projected
    /// from the child Chat's own messages.
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
    /// Where the copied conversation starts in the child Chat. A backward-keeping directional
    /// summary inserts a compact boundary before the kept messages, shifting the copied
    /// conversation later accordingly.
    #[serde(default)]
    pub model_seed_start: usize,
    pub public_seed: Vec<BranchPublicItem>,
    pub session_permissions: BranchSessionPermissionRegistration,
}

impl BranchCreation {
    /// Truncation or compaction broke up the copy seed: from then on the public conversation is
    /// projected from the child Chat's own messages.
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

/// A position that rewind and directional summaries can pick: a user prompt in the current
/// conversation.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RewindPoint {
    /// The prompt's uuid in the Transcript, which is also the message id file-rewind snapshots are
    /// associated with.
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

/// Result of restoring code. Following Claude Code, a failure on one file skips that file and
/// continues; when no snapshot is found for the target prompt, no file is changed and `error`
/// explains why.
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

/// Follows the `content` of Claude Code's `createCompactBoundaryMessage`.
pub const COMPACT_BOUNDARY_TEXT: &str = "Conversation compacted";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Manual,
    Auto,
}

/// Follows Claude Code's `compactMetadata`. Tickloop does not write `preservedSegment`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompactBoundary {
    pub trigger: CompactTrigger,
    pub pre_tokens: usize,
    pub user_context: Option<String>,
    pub messages_summarized: Option<usize>,
    /// The boundary was carried past a newer boundary as a kept message by a later Summarize from
    /// here. Such a boundary follows the previous message in the Transcript and is no longer the
    /// start of the conversation chain.
    #[serde(default)]
    pub preserved_by_later_summary: bool,
    /// Host-copied original prompts from the active chain, never model summaries.
    #[serde(default)]
    pub original_user_inputs: Option<Vec<OriginalUserInput>>,
    /// The model at compaction time, valid only within this process: used to check compatibility
    /// when a rewind past the boundary re-exposes private reasoning. After resume the messages
    /// before the boundary are not in memory, so it need not be saved.
    #[serde(skip)]
    pub model: Option<Box<ModelConfig>>,
}

/// Follows Claude Code's `summarizeMetadata`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SummarizeMetadata {
    pub messages_summarized: usize,
    pub user_context: Option<String>,
    pub direction: SummaryDirection,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum CompactionMarker {
    Boundary(CompactBoundary),
    /// Carries `summarize_metadata` when the summary kept messages; otherwise, following Claude
    /// Code, it is shown only in the full transcript.
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
    /// The selected user prompt; see [`RewindPoint::message_id`].
    pub message_id: Uuid,
    pub direction: SummaryDirection,
    pub guidance: Option<String>,
}

/// One item of the model-visible conversation: an ordinary message or a compact summary.
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
    /// The summary text produced by the model.
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

/// Result of a full compaction.
#[derive(Clone, Debug)]
pub struct CompactionOutcome {
    /// The summary text produced by the model.
    pub summary: String,
    pub snapshot: ChatSnapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelChange {
    Applied,
    Pending,
}
