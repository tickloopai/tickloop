//! Transcript: one append-only JSONL file per Chat, following Claude Code's session format.
//!
//! User, assistant and system entries form a `uuid`/`parentUuid` chain. Metadata stays outside
//! the chain, with the latest entry winning for each kind. Concurrent continuations can branch
//! without file locking; loading follows the branch with the newest leaf. This module owns
//! the format and storage; execution scheduling belongs to the runtime.

mod entry;
mod listing;
mod path;
mod reader;
mod writer;

pub(crate) use entry::{
    AiTitleEntry, ArtifactTicksEntry, BranchOriginEntry, COMPACT_BOUNDARY_SUBTYPE, ChainEntry,
    CompactMetadata, CompactTriggerEntry, ContentReplacementEntry, ContentReplacementRecord,
    CustomTitleEntry, DelegationEntry, FamilyCompletionsEntry, FamilyInputsEntry,
    FileHistorySnapshotEntry, ForkedFrom, LOOP_CREATED_SUBTYPE, LOOP_ENDED_SUBTYPE,
    LOOP_STATE_SUBTYPE, LOOP_TRIGGER_SUBTYPE, LastPromptEntry, Line, LoopRecord,
    MonitorMembersEntry, PartialCompactDirection, SentAgentMessagesEntry, SubagentsEntry,
    SummarizeMetadata as SummarizeMetadataEntry, format_timestamp, now_timestamp,
    parse_timestamp_ms,
};
pub(crate) use listing::list_project;
pub(crate) use path::{
    locate, private_family_temp_directory, private_project_temp_directory, project_directory,
    transcript_path,
};
pub(crate) use reader::{LoadedTranscript, load, load_original_user_inputs};
pub(crate) use writer::{CachedMetadata, FLUSH_INTERVAL, TranscriptWriter};
