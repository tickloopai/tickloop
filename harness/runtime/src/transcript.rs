//! Transcript storage and assembly. The record format, `Line` and its entries, and the display
//! projection live in `tickloop_harness_transcript`. A `TranscriptStore` keeps a Chat's Lines; the
//! local one writes a JSONL file per Chat. Concurrent continuations can branch; loading follows
//! the branch with the newest leaf.

mod jsonl;
mod listing;
mod outbox;
mod path;
mod reader;
pub mod records;
mod retained;
mod store;

pub use jsonl::JsonlTranscriptStore;
#[cfg(test)]
pub(crate) use jsonl::load_file;
pub use listing::TranscriptSummary;
pub use retained::retained_before;
pub use store::{
    AppendAck, AppendBatch, BatchId, ChatPlacement, LoadRange, StorePosition, StoredTranscript,
    TranscriptError, TranscriptStore,
};

pub(crate) use outbox::{CachedMetadata, FLUSH_INTERVAL, Settled, TranscriptOutbox};
pub(crate) use path::{private_family_temp_directory, private_project_temp_directory};
pub(crate) use reader::{
    LoadedTranscript, assemble, load_compaction_ancestors, load_original_user_inputs,
};
pub(crate) use retained::RetainedFold;
