//! Compaction boundaries and compaction summaries, represented following Claude Code's
//! `compact_boundary` and `isCompactSummary`.
//!
//! Full compaction appends a boundary system message and a summary user message at the end of the
//! conversation; messages before the boundary stay in this process's memory, and the model sees
//! only messages after the last boundary. A directed summary (partial compact) replaces the
//! conversation with the boundary, the summary and the kept messages; kept messages get new message
//! ids and are rewritten linearly after the new boundary in the Transcript (diverging from Claude
//! Code's preserved-segment relinking; see `CompactionLayout::apply`).

use std::collections::{BTreeMap, HashMap};

use uuid::Uuid;

use super::ChatBookkeeping;
use tickloop_harness_protocol::{
    COMPACT_BOUNDARY_TEXT, ChatSnapshot, CompactBoundary, CompactionMarker, Content, Message, Role,
    SummarizeMetadata, Usage,
};

/// Source of one message in the conversation after compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MessageSource {
    Existing(usize),
    Boundary,
    Summary,
}

/// Compaction result: the new conversation consists of existing messages, the new boundary and the
/// summary, in order.
pub(super) struct CompactionLayout {
    pub sources: Vec<MessageSource>,
    /// Index of the boundary's logical parent message in the conversation before compaction.
    pub logical_parent: Option<usize>,
    pub boundary: CompactBoundary,
    pub summary: Message,
    pub summarize_metadata: Option<SummarizeMetadata>,
}

impl CompactionLayout {
    pub(super) fn position(&self, source: MessageSource) -> Option<usize> {
        self.sources
            .iter()
            .position(|candidate| *candidate == source)
    }

    /// All messages before compaction stay at the start unchanged (full compaction); otherwise this
    /// is a directed summary, and kept messages are rewritten after the new boundary.
    pub(super) fn keeps_existing_prefix(&self, message_count: usize) -> bool {
        let existing = self
            .sources
            .iter()
            .filter(|source| matches!(source, MessageSource::Existing(_)))
            .count();
        existing == message_count
            && self.sources[..existing]
                .iter()
                .enumerate()
                .all(|(index, source)| *source == MessageSource::Existing(index))
    }

    /// Produces the post-compaction snapshot according to the layout: records attached to kept
    /// messages move with them, and the boundary and summary get new message ids.
    ///
    /// Directed summaries diverge from Claude Code: Claude Code keeps the original entries for kept
    /// messages and relinks them on load via the preserved segment on the boundary, but when
    /// directed summaries stack, or the conversation continues after a rewind past the segment's
    /// head, relinking drops or revives messages. Tickloop rewrites kept messages linearly as new
    /// entries after the new boundary, so they get new message ids (file rollback snapshots tied to
    /// prompt ids are re-recorded by the caller per [`Self::renamed_message_ids`]); usage of kept
    /// assistant messages is zeroed as Claude Code does when relinking, and old boundaries carried
    /// after the new boundary are marked `preserved_by_later_summary`.
    pub(super) fn apply(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
    ) -> (ChatSnapshot, ChatBookkeeping) {
        let rewrites_existing = !self.keeps_existing_prefix(snapshot.messages.len());
        let mut compacted = snapshot.clone();
        if let Some(compactions) = &mut compacted.chat_loop.compactions {
            *compactions += 1;
        }
        let mut compacted_bookkeeping = bookkeeping.clone();
        compacted_bookkeeping.lifecycle.remember_compacted_leaf(
            bookkeeping
                .lifecycle
                .display_leaf(snapshot, &bookkeeping.message_responses),
        );
        compacted.messages = Vec::with_capacity(self.sources.len());
        compacted.message_runs = BTreeMap::new();
        compacted.message_contexts = BTreeMap::new();
        compacted_bookkeeping.denial_references = BTreeMap::new();
        compacted_bookkeeping.loop_state_views = BTreeMap::new();
        compacted_bookkeeping.message_responses = BTreeMap::new();
        compacted_bookkeeping
            .lifecycle
            .restart_claims_at(self.sources.len());
        compacted.message_ids = BTreeMap::new();
        compacted.original_message_ids = BTreeMap::new();
        compacted.prompt_execution_modes = BTreeMap::new();
        compacted.inherited_prompts.clear();
        compacted.host_notes = BTreeMap::new();
        compacted.message_compactions = BTreeMap::new();
        compacted.message_usage = BTreeMap::new();
        compacted.message_sources = BTreeMap::new();
        compacted.message_models = BTreeMap::new();
        for (index, source) in self.sources.iter().enumerate() {
            match *source {
                MessageSource::Existing(original) => {
                    let message = snapshot.messages[original].clone();
                    if let Some(run) = snapshot.message_runs.get(&original) {
                        compacted.message_runs.insert(index, *run);
                    }
                    if let Some(contexts) = snapshot.message_contexts.get(&original) {
                        compacted.message_contexts.insert(index, contexts.clone());
                    }
                    if let Some(response) = bookkeeping.message_responses.get(&original) {
                        compacted_bookkeeping
                            .message_responses
                            .insert(index, *response);
                    }
                    if let Some(reference) = bookkeeping.denial_references.get(&original) {
                        compacted_bookkeeping
                            .denial_references
                            .insert(index, reference.clone());
                    }
                    if let Some(view) = bookkeeping.loop_state_views.get(&original) {
                        compacted_bookkeeping
                            .loop_state_views
                            .insert(index, view.clone());
                    }
                    if let Some(original_id) = snapshot.message_ids.get(&original) {
                        let id = if rewrites_existing {
                            tickloop_ids::generate()
                        } else {
                            *original_id
                        };
                        compacted.message_ids.insert(index, id);
                        if let Some(origin) = snapshot.agent_message_origins.get(original_id) {
                            compacted.agent_message_origins.insert(id, origin.clone());
                        }
                        if let Some(origin) = snapshot.human_family_origins.get(original_id) {
                            compacted.human_family_origins.insert(id, origin.clone());
                        }
                    }
                    if let Some(source_id) = snapshot
                        .original_message_ids
                        .get(&original)
                        .or_else(|| snapshot.message_ids.get(&original))
                    {
                        compacted.original_message_ids.insert(index, *source_id);
                    }
                    if let Some(mode) = snapshot.prompt_execution_modes.get(&original) {
                        compacted.prompt_execution_modes.insert(index, *mode);
                    }
                    if snapshot.inherited_prompts.contains(&original) {
                        compacted.inherited_prompts.insert(index);
                    }
                    if let Some(announced) = snapshot.host_notes.get(&original) {
                        compacted.host_notes.insert(index, announced.clone());
                    }
                    if let Some(source) = snapshot.message_sources.get(&original) {
                        compacted.message_sources.insert(index, source.clone());
                    }
                    if let Some(model) = snapshot.message_models.get(&original) {
                        compacted.message_models.insert(index, model.clone());
                    }
                    let usage = snapshot.message_usage.get(&original).cloned();
                    if rewrites_existing && message.role == Role::Assistant {
                        compacted
                            .message_usage
                            .insert(index, zeroed(usage.unwrap_or_default()));
                    } else if let Some(usage) = usage {
                        compacted.message_usage.insert(index, usage);
                    }
                    if let Some(marker) = snapshot.message_compactions.get(&original) {
                        let mut marker = marker.clone();
                        if let CompactionMarker::Boundary(boundary) = &mut marker
                            && rewrites_existing
                        {
                            boundary.preserved_by_later_summary = true;
                        }
                        compacted.message_compactions.insert(index, marker);
                    }
                    compacted.messages.push(message);
                }
                MessageSource::Boundary => {
                    compacted.messages.push(Message {
                        role: Role::System,
                        content: vec![Content::Text {
                            text: COMPACT_BOUNDARY_TEXT.into(),
                        }],
                    });
                    compacted
                        .message_ids
                        .insert(index, tickloop_ids::generate());
                    compacted
                        .message_compactions
                        .insert(index, CompactionMarker::Boundary(self.boundary.clone()));
                }
                MessageSource::Summary => {
                    compacted.messages.push(self.summary.clone());
                    compacted
                        .message_ids
                        .insert(index, tickloop_ids::generate());
                    compacted.message_compactions.insert(
                        index,
                        CompactionMarker::Summary {
                            summarize_metadata: self.summarize_metadata.clone(),
                        },
                    );
                }
            }
        }
        if let Some(origin) = compacted
            .branch_origin
            .as_mut()
            .and_then(|origin| origin.creation.as_mut())
        {
            let seed_start = (origin.model_seed_message_count > 0)
                .then(|| self.moved(origin.model_seed_start))
                .flatten();
            let seed_moves_together = seed_start.is_some_and(|start| {
                (0..origin.model_seed_message_count).all(|offset| {
                    self.moved(origin.model_seed_start + offset) == Some(start + offset)
                })
            });
            match seed_start.filter(|_| seed_moves_together) {
                Some(start) => origin.model_seed_start = start,
                None if !rewrites_existing => {}
                None => origin.clear_seed(),
            }
        }
        if rewrites_existing {
            let messages = crate::citations::snapshot_messages(
                snapshot,
                Some(&bookkeeping.continued_message_ids),
            );
            for (index, source) in self.sources.iter().enumerate() {
                let MessageSource::Existing(original) = source else {
                    continue;
                };
                for call in &mut compacted_bookkeeping.citation_sources.calls {
                    if crate::citations::call_is_visible_in(
                        call,
                        std::slice::from_ref(&messages[*original]),
                    ) {
                        call.aliases.push(compacted.message_ids[&index]);
                    }
                }
            }
        }
        if let Some(summary_index) = self.position(MessageSource::Summary) {
            let summary_id = compacted.message_ids[&summary_index];
            let mut summarized = snapshot.clone();
            let is_full = self.keeps_existing_prefix(snapshot.messages.len());
            for (index, message) in summarized.messages.iter_mut().enumerate() {
                if !is_full && self.sources.contains(&MessageSource::Existing(index)) {
                    message.content.clear();
                    summarized.message_ids.remove(&index);
                    summarized.original_message_ids.remove(&index);
                }
            }
            for call in &mut compacted_bookkeeping.citation_sources.calls {
                if crate::citations::call_is_visible(
                    call,
                    &summarized,
                    &bookkeeping.continued_message_ids,
                ) {
                    call.aliases.push(summary_id);
                }
            }
        }
        compacted.announced_host_tools = compacted.visible_announced_host_tools();
        (compacted, compacted_bookkeeping)
    }

    /// Id changes of the same kept message before and after `apply` (old id → new id); empty for
    /// full compaction.
    pub(super) fn renamed_message_ids(
        &self,
        before: &ChatSnapshot,
        after: &ChatSnapshot,
    ) -> HashMap<Uuid, Uuid> {
        self.sources
            .iter()
            .enumerate()
            .filter_map(|(index, source)| match source {
                MessageSource::Existing(original) => Some((
                    *before.message_ids.get(original)?,
                    *after.message_ids.get(&index)?,
                )),
                MessageSource::Boundary | MessageSource::Summary => None,
            })
            .filter(|(old, new)| old != new)
            .collect()
    }

    fn moved(&self, original: usize) -> Option<usize> {
        self.position(MessageSource::Existing(original))
    }
}

fn zeroed(mut usage: Usage) -> Usage {
    usage.input_tokens = Some(0);
    usage.output_tokens = Some(0);
    usage.cache_read_input_tokens = Some(0);
    usage.cache_write_input_tokens = Some(0);
    usage
}
