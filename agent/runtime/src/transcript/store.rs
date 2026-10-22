//! Where a Harness keeps its Chats' Transcripts: the `TranscriptStore` seam.
//!
//! The Harness appends Lines in batches and loads them back; it never derives a store position
//! itself. Each batch has an identity, so retrying it after an unclear failure commits it once.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tickloop_agent_protocol::ChatId;
use tickloop_agent_transcript::display::{DisplayNode, DisplayProjector};
use tickloop_agent_transcript::{ChainEntry, Line};
use uuid::Uuid;

#[async_trait]
pub trait TranscriptStore: std::any::Any + Send + Sync {
    /// Makes room for a new Chat and returns the position its first batch expects.
    /// `Unsupported` leaves the Chat without a Transcript.
    async fn register(
        &self,
        chat: ChatId,
        placement: &ChatPlacement,
    ) -> Result<StorePosition, TranscriptError>;

    /// The Chat's Lines in store order, or `None` when the store keeps nothing of it. A store may
    /// return more history than `range` asks for.
    async fn load(
        &self,
        chat: ChatId,
        range: LoadRange,
    ) -> Result<Option<StoredTranscript>, TranscriptError>;

    /// Commits the batch as a whole. Retrying a batch with the same id commits it once.
    async fn append(&self, chat: ChatId, batch: &AppendBatch)
    -> Result<AppendAck, TranscriptError>;

    /// `append` from a thread that cannot wait asynchronously, such as when a Harness is dropped.
    /// A store that cannot block returns `Unsupported`.
    fn append_blocking(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError>;
}

/// Where a new Chat's Transcript goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatPlacement {
    /// With the other Chats of the Folder.
    Folder(PathBuf),
    /// Beside the Chat a Branch was made from.
    BesideChat(ChatId),
    /// A Chat without a Folder.
    Folderless,
}

/// An opaque, monotonic commit position the store defines. The Harness only passes it back.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct StorePosition(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BatchId(pub Uuid);

impl BatchId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BatchId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadRange {
    /// From the latest compact boundary that starts the conversation chain, with the retained
    /// Lines written before it.
    FromLatestBoundary,
    Full,
}

#[derive(Clone, Debug, Default)]
pub struct StoredTranscript {
    /// The retained Lines (`Line::is_retained`) before `lines`, in store order.
    pub retained: Vec<Line>,
    pub lines: Vec<Line>,
    /// The commit position of the same snapshot `retained` and `lines` were read from.
    pub position: StorePosition,
    pub warnings: Vec<String>,
}

impl StoredTranscript {
    /// The conversation chain from its root to `leaf`, or to the newest leaf. `None` when these
    /// Lines hold no such leaf or its chain does not reach the root, as when a load from the
    /// latest boundary leaves the leaf's ancestors out.
    pub fn chain_to(&self, leaf: Option<Uuid>) -> Option<Vec<ChainEntry>> {
        match super::reader::assemble(self, leaf) {
            (Some(loaded), true) => Some(loaded.chain),
            _ => None,
        }
    }

    /// The public display ancestry of a selected branch, including history before compaction.
    /// Callers load `LoadRange::Full` once and keep this immutable snapshot for every page.
    /// System messages and other internal records never become history items.
    pub fn display_history(&self, leaf: Option<Uuid>) -> Option<DisplayHistorySnapshot> {
        let chain = self.chain_to(leaf)?;
        let mut projector = DisplayProjector::new();
        for line in self.retained.iter().chain(&self.lines) {
            projector.apply(line);
        }
        let mut cursor = chain
            .iter()
            .rev()
            .find_map(|entry| projector.canonical_id(entry.uuid));
        let mut visited = std::collections::HashSet::new();
        let mut nodes = Vec::new();
        while let Some(id) = cursor {
            if !visited.insert(id) {
                return None;
            }
            let node = projector.node(id)?;
            nodes.push(node.clone());
            cursor = node.parent;
        }
        nodes.reverse();
        Some(DisplayHistorySnapshot {
            position: self.position,
            nodes,
        })
    }
}

/// A fixed public history read. Later appends cannot alter its items or page boundaries.
#[derive(Clone, Debug)]
pub struct DisplayHistorySnapshot {
    pub position: StorePosition,
    pub nodes: Vec<DisplayNode>,
}

#[derive(Clone, Debug)]
pub struct DisplayHistoryPage {
    pub position: StorePosition,
    pub items: Vec<DisplayNode>,
    pub next_offset: Option<usize>,
}

impl DisplayHistorySnapshot {
    pub const MAX_PAGE_SIZE: usize = 100;

    pub fn page(&self, offset: usize, limit: usize) -> Result<DisplayHistoryPage, TranscriptError> {
        if limit == 0 || limit > Self::MAX_PAGE_SIZE || offset > self.nodes.len() {
            return Err(TranscriptError::Storage(
                "invalid display history page bounds".into(),
            ));
        }
        let end = offset.saturating_add(limit).min(self.nodes.len());
        Ok(DisplayHistoryPage {
            position: self.position,
            items: self.nodes[offset..end].to_vec(),
            next_offset: (end < self.nodes.len()).then_some(end),
        })
    }
}

#[derive(Clone, Debug)]
pub struct AppendBatch {
    pub id: BatchId,
    /// The position the previous batch or the load acknowledged. A store that enforces it
    /// answers `Conflict` when the Transcript moved on.
    pub expected: Option<StorePosition>,
    pub lines: Vec<Line>,
    /// The last event sequence produced before these Lines.
    pub events_through: u64,
    /// The display node the Chat shows as current after this batch.
    pub display_leaf: Option<Uuid>,
    /// Make this batch, and every one before it, durable before acknowledging. An empty durable
    /// batch only does that.
    pub durable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendAck {
    pub position: StorePosition,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptError {
    /// A newer holder owns the Chat. Nothing more may be written.
    #[error("the Transcript belongs to a newer holder")]
    Fenced,
    /// The Transcript is no longer the one this holder writes to. Nothing more may be written.
    #[error("{0}")]
    Conflict(String),
    #[error("the Transcript store does not support this")]
    Unsupported,
    /// Possibly temporary; the same batch may be retried.
    #[error("{0}")]
    Storage(String),
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use serde_json::json;

    fn entry(id: Uuid, parent: Option<Uuid>, kind: &str, text: &str) -> Line {
        let mut value = json!({
            "type": kind, "uuid": id, "parentUuid": parent,
            "chatId": Uuid::nil(), "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "test", "humanFamilyOrigin": null,
            "message": {"role": kind, "content": [{"type": "text", "text": text}]}
        });
        if kind == "user" {
            value["executionMode"] = json!("auto");
        } else if kind == "assistant" {
            value["responseId"] = json!(id);
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn full_display_history_follows_selected_branch_across_compaction_without_system_notes() {
        let [prompt, answer, sibling, boundary, summary, next] =
            std::array::from_fn(|_| Uuid::new_v4());
        let mut compact = entry(boundary, None, "system", "INTERNAL COMPACTION");
        if let Line::System(entry) = &mut compact {
            entry.subtype = Some(tickloop_agent_transcript::COMPACT_BOUNDARY_SUBTYPE.into());
            entry.logical_parent_uuid = Some(answer);
        }
        let mut compact_summary = entry(summary, Some(boundary), "user", "INTERNAL SUMMARY");
        if let Line::User(entry) = &mut compact_summary {
            entry.is_compact_summary = true;
        }
        let mut stored = StoredTranscript {
            position: StorePosition(7),
            lines: vec![
                entry(prompt, None, "user", "first"),
                entry(answer, Some(prompt), "assistant", "answer"),
                entry(sibling, Some(prompt), "assistant", "other branch"),
                compact,
                compact_summary,
                entry(next, Some(summary), "user", "next"),
            ],
            ..Default::default()
        };
        let snapshot = stored.display_history(Some(next)).unwrap();
        assert_eq!(
            snapshot
                .nodes
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            vec![prompt, answer, next]
        );
        assert_eq!(
            stored.display_history(Some(sibling)).unwrap().nodes.len(),
            2
        );
        assert!(snapshot.page(0, 0).is_err());
        assert!(snapshot.page(0, 101).is_err());
        assert!(snapshot.page(4, 1).is_err());
        let first = snapshot.page(0, 2).unwrap();
        assert_eq!(first.position, StorePosition(7));
        assert_eq!(first.next_offset, Some(2));
        stored
            .lines
            .push(entry(Uuid::new_v4(), Some(next), "user", "later append"));
        let last = snapshot.page(2, 2).unwrap();
        assert_eq!(last.items.len(), 1);
        assert_eq!(last.items[0].id, next);
        assert_eq!(last.next_offset, None);
    }
}
