//! Transcript: an append-only sequence of Lines per Chat, following Claude Code's session format.
//!
//! User, assistant and system entries form a `uuid`/`parentUuid` chain. Metadata stays outside
//! the chain, with the latest entry winning for each kind. Concurrent continuations can branch;
//! loading follows the branch with the newest leaf. A `TranscriptStore` keeps the Lines; the
//! local one writes a JSONL file per Chat.

pub mod display;
mod entry;
mod jsonl;
mod listing;
mod outbox;
mod path;
mod reader;
pub mod records;
mod store;

pub use entry::{
    AiTitleEntry, ArtifactTicksEntry, BranchOriginEntry, COMPACT_BOUNDARY_SUBTYPE, ChainEntry,
    CompactMetadata, CompactTriggerEntry, ContentReplacementEntry, ContentReplacementRecord,
    CustomTitleEntry, DelegationEntry, DiscardReason, FamilyCompletionsEntry, FamilyInputsEntry,
    FileHistorySnapshotEntry, ForkedFrom, InputEntry, InputOp, InputState, InterruptedBy,
    LOOP_CREATED_SUBTYPE, LOOP_ENDED_SUBTYPE, LOOP_STATE_SUBTYPE, LOOP_TRIGGER_SUBTYPE,
    LastPromptEntry, Line, LoopRecord, MonitorMembersEntry, PartialCompactDirection,
    PreservedSegmentEntry, ResponseEnding, ResponseEntry, ResponseState, SentAgentMessagesEntry,
    SettledOutcome, SubagentsEntry, SummarizeMetadata as SummarizeMetadataEntry, TaskBoardsEntry,
};
pub use jsonl::JsonlTranscriptStore;
pub use listing::TranscriptSummary;
pub use store::{
    AppendAck, AppendBatch, BatchId, ChatPlacement, LoadRange, StorePosition, StoredTranscript,
    TranscriptError, TranscriptStore,
};

pub(crate) use outbox::{CachedMetadata, FLUSH_INTERVAL, Settled, TranscriptOutbox};
pub(crate) use path::{
    TICKHOST_RUNTIME_SUBDIRECTORY, private_family_temp_directory, private_project_temp_directory,
    private_tickhost_directory,
};
pub(crate) use reader::{
    LoadedTranscript, assemble, load_compaction_ancestors, load_original_user_inputs,
};
