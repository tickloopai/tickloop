//! The retained state of a Chat's Transcript, folded as its Lines go by, and the
//! [`RetainedSnapshot`](Line::RetainedSnapshot) that stands for it.
//!
//! A load reads the conversation chain from the latest compact boundary and the retained Lines
//! from the latest snapshot, so the retained Lines a load reads stay bounded however long the
//! Chat lives. Loading from a snapshot gives the Chat a load of everything would give it.

use std::collections::{HashMap, HashSet, VecDeque};

use tickloop_agent_protocol::Content;
use tickloop_agent_transcript::{
    InputState, InterruptedBy, Line, MetadataKind, ResponseEnding, ResponseState,
    RetainedSnapshotEntry, Retention, WaitState,
};
use uuid::Uuid;

/// A snapshot is written once this many retained Lines followed the last one…
const SNAPSHOT_AFTER_LINES: usize = 512;
/// …or once they take this many bytes.
const SNAPSHOT_AFTER_BYTES: usize = 256 * 1024;
/// The newest Lines of each unknown kind a snapshot carries; hosts of different versions take
/// turns holding a Chat, so this keeps what a newer one wrote without letting it pile up.
const UNKNOWN_LINES_PER_KIND: usize = 16;
/// The settled inputs a host or the user sends under their own id that a snapshot keeps, so a
/// repeated delivery still finds its outcome.
const SETTLED_EXTERNAL_INPUTS: usize = 1000;

/// What the conversation chain from the latest compact boundary refers to: the records a load
/// needs to restore it. What compaction left behind is remembered too, so a record that came
/// before its message is only dropped once compaction left that message behind.
#[derive(Clone, Debug, Default)]
struct LoadableChain {
    entries: HashSet<Uuid>,
    responses: HashSet<Uuid>,
    inputs: HashSet<Uuid>,
    tool_results: HashSet<String>,
    compacted_entries: HashSet<Uuid>,
    compacted_tool_results: HashSet<String>,
}

impl LoadableChain {
    fn add(&mut self, line: &Line) {
        let Some(entry) = line.chain_entry() else {
            return;
        };
        if entry.restarts_chain() {
            let entries = std::mem::take(&mut self.entries);
            let tool_results = std::mem::take(&mut self.tool_results);
            self.compacted_entries.extend(entries);
            self.compacted_tool_results.extend(tool_results);
            self.responses.clear();
            self.inputs.clear();
        }
        self.entries.insert(entry.uuid);
        self.entries.extend(entry.original_message_id);
        self.responses.extend(entry.response_id);
        self.inputs.extend(entry.input_id);
        self.tool_results
            .extend(
                entry
                    .message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    }),
            );
    }
}

impl LoadableChain {
    fn keeps_entry(&self, entry: Uuid) -> bool {
        self.entries.contains(&entry) || !self.compacted_entries.contains(&entry)
    }

    fn keeps_tool_result(&self, call_id: &str) -> bool {
        self.tool_results.contains(call_id) || !self.compacted_tool_results.contains(call_id)
    }
}

/// A retained Line and when it last changed the state, so a snapshot keeps their order.
#[derive(Clone, Debug)]
struct Kept {
    order: u64,
    line: Line,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RetainedFold {
    next_order: u64,
    /// The latest Line of each kind whose latest Line stands for the Chat.
    latest: HashMap<MetadataKind, Kept>,
    /// The latest record of each answer, input and wait.
    by_id: HashMap<(MetadataKind, Uuid), Kept>,
    content_replacements: Vec<Kept>,
    /// Every goal event, which the conversation shows as its goal lines.
    goal_events: Vec<Kept>,
    /// The latest file history snapshot of each message.
    file_history: HashMap<Uuid, Kept>,
    unknown: HashMap<String, VecDeque<Kept>>,
    chain: LoadableChain,
    lines_since_snapshot: usize,
    bytes_since_snapshot: usize,
}

impl RetainedFold {
    /// Folds in a Line as a load reads it.
    pub(crate) fn apply(&mut self, line: &Line) {
        let kind = line.kind();
        match kind.retention() {
            Retention::Chain => self.chain.add(line),
            Retention::SourceState => {}
            Retention::Snapshot => {
                let Line::RetainedSnapshot(snapshot) = line else {
                    unreachable!("a snapshot kind is a snapshot");
                };
                // What compaction left behind is gone from the snapshot, and nothing refers to it
                // later.
                let mut chain = std::mem::take(&mut self.chain);
                chain.compacted_entries.clear();
                chain.compacted_tool_results.clear();
                *self = Self {
                    next_order: self.next_order,
                    chain,
                    ..Self::default()
                };
                for line in &snapshot.lines {
                    self.keep(line);
                }
                self.lines_since_snapshot = 0;
                self.bytes_since_snapshot = 0;
            }
            _ => {
                self.keep(line);
                self.lines_since_snapshot += 1;
                self.bytes_since_snapshot += serde_json::to_vec(line).map_or(0, |raw| raw.len());
            }
        }
    }

    fn keep(&mut self, line: &Line) {
        self.next_order += 1;
        let kept = Kept {
            order: self.next_order,
            line: line.clone(),
        };
        let kind = line.kind();
        match (kind.retention(), line) {
            (Retention::ChatLatest, _) => {
                self.latest.insert(kind, kept);
            }
            (Retention::LatestById, Line::Response(entry)) => {
                self.by_id.insert((kind, entry.response_id), kept);
            }
            (Retention::LatestById, Line::Input(entry)) => {
                self.by_id.insert((kind, entry.input_id), kept);
            }
            (Retention::LatestById, Line::Wait(entry)) => {
                self.by_id.insert((kind, entry.wait_id), kept);
            }
            (Retention::Accumulating, Line::ContentReplacement(_)) => {
                self.content_replacements.push(kept);
            }
            (Retention::Accumulating, Line::GoalEvent(_)) => {
                self.goal_events.push(kept);
            }
            (Retention::Accumulating, Line::FileHistorySnapshot(entry)) => {
                self.file_history.insert(entry.message_id, kept);
            }
            (Retention::Unknown, Line::Unknown(unknown)) => {
                let lines = self.unknown.entry(unknown.kind.clone()).or_default();
                lines.push_back(kept);
                if lines.len() > UNKNOWN_LINES_PER_KIND {
                    lines.pop_front();
                }
            }
            _ => {}
        }
    }

    /// Enough retained Lines followed the last snapshot to fold them into a new one.
    pub(crate) fn is_due(&self) -> bool {
        self.lines_since_snapshot >= SNAPSHOT_AFTER_LINES
            || self.bytes_since_snapshot >= SNAPSHOT_AFTER_BYTES
    }

    /// The snapshot of the retained state: what a load of the conversation from the latest
    /// compact boundary needs of it, in the order it last changed.
    pub(crate) fn snapshot(&self, chat_id: Uuid) -> Line {
        let kept_responses = self
            .by_id
            .values()
            .filter_map(|kept| match &kept.line {
                Line::Response(entry) if self.keeps_response(entry) => Some(entry),
                _ => None,
            })
            .collect::<Vec<_>>();
        let response_inputs = kept_responses
            .iter()
            .filter_map(|response| response.input_id)
            .collect::<HashSet<_>>();
        let kept_responses = kept_responses
            .iter()
            .map(|response| response.response_id)
            .collect::<HashSet<_>>();
        let recent_external_inputs = {
            let mut settled = self
                .by_id
                .values()
                .filter_map(|kept| match &kept.line {
                    Line::Input(entry)
                        if entry.op.is_external()
                            && matches!(entry.state, InputState::Settled { .. }) =>
                    {
                        Some((kept.order, entry.input_id))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            settled.sort_unstable_by_key(|(order, _)| std::cmp::Reverse(*order));
            settled
                .into_iter()
                .take(SETTLED_EXTERNAL_INPUTS)
                .map(|(_, id)| id)
                .collect::<HashSet<_>>()
        };
        let mut lines = self
            .latest
            .values()
            .chain(self.by_id.values().filter(|kept| match &kept.line {
                Line::Response(entry) => kept_responses.contains(&entry.response_id),
                Line::Input(entry) => {
                    !matches!(entry.state, InputState::Settled { .. })
                        || self.chain.inputs.contains(&entry.input_id)
                        || response_inputs.contains(&entry.input_id)
                        || recent_external_inputs.contains(&entry.input_id)
                }
                Line::Wait(entry) => {
                    matches!(entry.state, WaitState::Open | WaitState::Resolved { .. })
                        || (matches!(entry.kind, tickloop_agent_transcript::WaitKind::Question { .. })
                            && self.chain.responses.contains(&entry.response_id))
                }
                _ => false,
            }))
            .chain(self.content_replacements.iter().filter(|kept| {
                matches!(&kept.line, Line::ContentReplacement(entry) if entry.replacements.iter().any(|replacement| self.chain.keeps_tool_result(replacement.tool_use_id())))
            }))
            .chain(&self.goal_events)
            .chain(
                self.file_history
                    .iter()
                    .filter(|(message, _)| self.chain.keeps_entry(**message))
                    .map(|(_, kept)| kept),
            )
            .chain(self.unknown.values().flatten())
            .collect::<Vec<_>>();
        lines.sort_unstable_by_key(|kept| kept.order);
        Line::RetainedSnapshot(RetainedSnapshotEntry {
            chat_id,
            lines: lines.into_iter().map(|kept| kept.line.clone()).collect(),
        })
    }

    /// An answer the Chat may still go on with, one whose open input another attempt may
    /// continue, or one the loadable chain shows.
    fn keeps_response(&self, entry: &tickloop_agent_transcript::ResponseEntry) -> bool {
        let lost_host = matches!(
            entry.state,
            ResponseState::Ended {
                ending: ResponseEnding::Interrupted {
                    by: InterruptedBy::Host | InterruptedBy::Handover,
                },
            }
        );
        let input_is_open = entry.input_id.is_some_and(|input| {
            self.by_id
                .get(&(MetadataKind::Input, input))
                .is_some_and(|kept| matches!(&kept.line, Line::Input(input) if !matches!(input.state, InputState::Settled { .. })))
        });
        // An answer that ended without messages of its own shows under the node it answered.
        let shows_without_messages = entry.without_messages
            && entry.display_parent.is_some_and(|parent| {
                self.chain.entries.contains(&parent)
                    || self.chain.inputs.contains(&parent)
                    || self.chain.responses.contains(&parent)
            });
        !entry.state.is_ended()
            || (lost_host && input_is_open)
            || shows_without_messages
            || self.chain.responses.contains(&entry.response_id)
    }
}

/// The index of the latest snapshot in `lines`.
fn latest_snapshot(lines: &[Line]) -> Option<usize> {
    lines
        .iter()
        .rposition(|line| matches!(line, Line::RetainedSnapshot(_)))
}

/// The retained Lines a load from the latest compact boundary reads of the Lines before it,
/// `lines[..boundary]`: those from the latest snapshot on. A snapshot at or after the boundary
/// stands for all of them.
pub fn retained_before(lines: &[Line], boundary: usize) -> Vec<Line> {
    let from = match latest_snapshot(lines) {
        Some(snapshot) if snapshot >= boundary => return Vec::new(),
        Some(snapshot) => snapshot,
        None => 0,
    };
    lines[from..boundary]
        .iter()
        .filter(|line| line.is_retained())
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "retained_tests.rs"]
mod tests;
