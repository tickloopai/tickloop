//! Fits a trusted approval snapshot into one reviewer's estimated input capacity.
//!
//! The operation, policy, directories, permission revision, prepared facts and the current and
//! latest user inputs are never omitted. When the snapshot is too large, oversized single
//! entries first lose their middle, oldest first; then the oldest Agent messages, reply turns,
//! tool calls and earlier user inputs are omitted in that order. Every cut is marked in place and
//! counted in `context_truncation`, so a reviewer never mistakes the remainder for complete
//! evidence.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use tickloop_agent_protocol::{Content, OriginalUserInput};

use super::reply_history::ReplyTurn;
use super::{AgentMessageEvidence, ApprovalSnapshot, HistoricalToolCall};

/// Kept user inputs together use at most this share of the space beside the fixed evidence.
const KEPT_INPUTS_SHARE_DIVISOR: usize = 2;
/// Any other single entry uses at most this share of the space left for optional evidence.
const OPTIONAL_ENTRY_SHARE_DIVISOR: usize = 4;
const MIN_KEPT_INPUT_BYTES: usize = 8 * 1024;
const MIN_OPTIONAL_ENTRY_BYTES: usize = 2 * 1024;
/// Strings this short are never cut, and a cut string keeps at least this much text.
const MIN_CUT_STRING_BYTES: usize = 512;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct ContextTruncation {
    pub agent_messages: SectionTruncation,
    pub reply_history: SectionTruncation,
    pub tool_history: SectionTruncation,
    pub original_user_inputs: SectionTruncation,
}

/// Oldest entries omitted, entries whose middle was cut, and the bytes both removed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct SectionTruncation {
    pub omitted_oldest: usize,
    pub shortened: usize,
    pub omitted_bytes: usize,
}

impl SectionTruncation {
    fn is_untouched(&self) -> bool {
        *self == Self::default()
    }

    fn record_cut(&mut self, removed: usize) {
        self.shortened += 1;
        self.omitted_bytes += removed;
    }

    fn record_omission(&mut self, bytes: usize) {
        self.omitted_oldest += 1;
        self.omitted_bytes += bytes;
    }
}

/// The evidence one reviewer receives: the complete snapshot fitted to that reviewer's estimated
/// input capacity, which one context-limit rejection may shrink further.
pub(crate) struct ReviewEvidence<'a> {
    complete: &'a ApprovalSnapshot,
    fitted: ApprovalSnapshot,
    shrunk: bool,
}

impl<'a> ReviewEvidence<'a> {
    pub(crate) fn new(complete: &'a ApprovalSnapshot, budget_bytes: usize) -> Self {
        Self {
            complete,
            fitted: fit(complete, budget_bytes),
            shrunk: false,
        }
    }

    pub(crate) fn snapshot(&self) -> &ApprovalSnapshot {
        &self.fitted
    }

    /// After the reviewer rejects the evidence as exceeding its context, refits it once to half
    /// of the bytes last sent. Returns whether a smaller snapshot is now available.
    pub(crate) fn shrink(&mut self) -> bool {
        if std::mem::replace(&mut self.shrunk, true) {
            return false;
        }
        let sent = json_len(&self.fitted);
        let smaller = fit(self.complete, sent / 2);
        if json_len(&smaller) >= sent {
            return false;
        }
        self.fitted = smaller;
        true
    }
}

pub(crate) fn fit(complete: &ApprovalSnapshot, budget: usize) -> ApprovalSnapshot {
    let mut fitted = complete.clone();
    let full_size = json_len(&fitted);
    if full_size <= budget {
        return fitted;
    }
    // Removed bytes are counted without their separators, so `size` never underestimates.
    let size = full_size + truncation_reserve();
    let mut truncation = ContextTruncation::default();
    let kept = kept_user_inputs(&fitted);
    let kept_bytes = user_inputs_bytes(&fitted.original_user_inputs, &kept, true);
    let optional_bytes = fitted.agent_messages.iter().map(json_len).sum::<usize>()
        + fitted
            .reply_history
            .contexts
            .iter()
            .flat_map(|context| &context.preceding)
            .map(json_len)
            .sum::<usize>()
        + fitted
            .tool_history
            .calls
            .iter()
            .map(json_len)
            .sum::<usize>()
        + user_inputs_bytes(&fitted.original_user_inputs, &kept, false);
    let room = budget.saturating_sub(size.saturating_sub(kept_bytes + optional_bytes));
    let kept_count = kept.iter().filter(|keep| **keep).count().max(1);
    let kept_cap = (room / KEPT_INPUTS_SHARE_DIVISOR / kept_count).max(MIN_KEPT_INPUT_BYTES);
    let capped_kept_bytes = fitted
        .original_user_inputs
        .iter()
        .zip(&kept)
        .filter(|(_, keep)| **keep)
        .map(|(input, _)| json_len(input).min(kept_cap))
        .sum::<usize>();
    let entry_cap = (room.saturating_sub(capped_kept_bytes) / OPTIONAL_ENTRY_SHARE_DIVISOR)
        .max(MIN_OPTIONAL_ENTRY_BYTES);

    let mut fitter = Fitter { budget, size };
    fitter.shorten_entries(
        &mut fitted.agent_messages,
        entry_cap,
        &mut truncation.agent_messages,
    );
    fitter.shorten_entries(
        fitted
            .reply_history
            .contexts
            .iter_mut()
            .flat_map(|context| &mut context.preceding),
        entry_cap,
        &mut truncation.reply_history,
    );
    fitter.shorten_entries(
        &mut fitted.tool_history.calls,
        entry_cap,
        &mut truncation.tool_history,
    );
    for keep_inputs in [false, true] {
        fitter.shorten_entries(
            fitted
                .original_user_inputs
                .iter_mut()
                .zip(&kept)
                .filter(|(_, keep)| **keep == keep_inputs)
                .map(|(input, _)| input),
            if keep_inputs { kept_cap } else { entry_cap },
            &mut truncation.original_user_inputs,
        );
    }

    fitter.omit_oldest(&mut fitted.agent_messages, &mut truncation.agent_messages);
    for context in &mut fitted.reply_history.contexts {
        fitter.omit_oldest(&mut context.preceding, &mut truncation.reply_history);
    }
    fitter.omit_oldest(&mut fitted.tool_history.calls, &mut truncation.tool_history);
    fitter.omit_oldest_user_inputs(&mut fitted, &kept, &mut truncation);

    if truncation != ContextTruncation::default() {
        fitted.tool_history.complete &= truncation.tool_history.is_untouched();
        fitted.reply_history.complete &= truncation.reply_history.is_untouched();
        fitted.context_truncation = Some(truncation);
    }
    fitted
}

/// The inputs of the operation's own turn and the latest input are always kept.
fn kept_user_inputs(snapshot: &ApprovalSnapshot) -> Vec<bool> {
    let latest = snapshot.original_user_inputs.len().saturating_sub(1);
    snapshot
        .original_user_inputs
        .iter()
        .enumerate()
        .map(|(index, input)| index == latest || input.run_id == snapshot.operation.run_id)
        .collect()
}

fn user_inputs_bytes(inputs: &[OriginalUserInput], kept: &[bool], keep_inputs: bool) -> usize {
    inputs
        .iter()
        .zip(kept)
        .filter(|(_, keep)| **keep == keep_inputs)
        .map(|(input, _)| json_len(input))
        .sum()
}

/// Bytes the `context_truncation` record and two completeness flags turning false can add.
fn truncation_reserve() -> usize {
    let widest_section = SectionTruncation {
        omitted_oldest: usize::MAX,
        shortened: usize::MAX,
        omitted_bytes: usize::MAX,
    };
    let widest = ContextTruncation {
        agent_messages: widest_section,
        reply_history: widest_section,
        tool_history: widest_section,
        original_user_inputs: widest_section,
    };
    json_len(&widest) + r#","context_truncation":"#.len() + 2
}

struct Fitter {
    budget: usize,
    size: usize,
}

impl Fitter {
    fn fits(&self) -> bool {
        self.size <= self.budget
    }

    fn shorten_entries<'e, T: Shortenable + 'e>(
        &mut self,
        entries: impl IntoIterator<Item = &'e mut T>,
        cap: usize,
        section: &mut SectionTruncation,
    ) {
        for entry in entries {
            if self.fits() {
                return;
            }
            let before = json_len(entry);
            if before <= cap {
                continue;
            }
            let removed = shorten(entry, cap);
            if removed > 0 {
                self.size = self
                    .size
                    .saturating_sub(before.saturating_sub(json_len(entry)));
                section.record_cut(removed);
            }
        }
    }

    fn omit_oldest<T: Serialize>(&mut self, entries: &mut Vec<T>, section: &mut SectionTruncation) {
        let mut omitted = 0;
        for entry in entries.iter() {
            if self.fits() {
                break;
            }
            let bytes = json_len(entry);
            self.size = self.size.saturating_sub(bytes);
            section.record_omission(bytes);
            omitted += 1;
        }
        entries.drain(..omitted);
    }

    /// Omits earlier user inputs, oldest first, with the reply contexts anchored to them; by now
    /// every reply turn and tool call is already omitted.
    fn omit_oldest_user_inputs(
        &mut self,
        snapshot: &mut ApprovalSnapshot,
        kept: &[bool],
        truncation: &mut ContextTruncation,
    ) {
        if self.fits() {
            return;
        }
        let anchor_bytes = snapshot
            .reply_history
            .contexts
            .iter()
            .map(|context| {
                (
                    (context.chat_id, context.user_message_id),
                    json_len(context),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut omitted_anchors = Vec::new();
        let mut keep = kept.iter();
        snapshot.original_user_inputs.retain(|input| {
            if *keep.next().unwrap_or(&true) || self.fits() {
                return true;
            }
            let bytes = json_len(input);
            self.size = self.size.saturating_sub(bytes);
            truncation.original_user_inputs.record_omission(bytes);
            let anchor = (input.chat_id, input.message_id);
            if let Some(bytes) = anchor_bytes.get(&anchor) {
                self.size = self.size.saturating_sub(*bytes);
                truncation.reply_history.omitted_bytes += bytes;
                omitted_anchors.push(anchor);
            }
            false
        });
        snapshot.reply_history.contexts.retain(|context| {
            !omitted_anchors.contains(&(context.chat_id, context.user_message_id))
        });
    }
}

/// An evidence entry whose strings may lose their middle to fit a byte cap.
trait Shortenable: Serialize {
    fn strings(&mut self) -> Vec<&mut String>;
}

impl Shortenable for OriginalUserInput {
    fn strings(&mut self) -> Vec<&mut String> {
        vec![&mut self.text]
    }
}

impl Shortenable for ReplyTurn {
    fn strings(&mut self) -> Vec<&mut String> {
        vec![&mut self.text]
    }
}

impl Shortenable for HistoricalToolCall {
    fn strings(&mut self) -> Vec<&mut String> {
        let mut strings = Vec::new();
        value_strings(&mut self.input, &mut strings);
        strings
    }
}

impl Shortenable for AgentMessageEvidence {
    fn strings(&mut self) -> Vec<&mut String> {
        let mut strings = Vec::new();
        for content in &mut self.content {
            match content {
                Content::Text { text } => strings.push(text),
                Content::ToolUse { input, .. } => value_strings(input, &mut strings),
                _ => {}
            }
        }
        strings
    }
}

fn value_strings<'a>(value: &'a mut Value, strings: &mut Vec<&'a mut String>) {
    match value {
        Value::String(text) => strings.push(text),
        Value::Array(items) => {
            for item in items {
                value_strings(item, strings);
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                value_strings(item, strings);
            }
        }
        _ => {}
    }
}

/// Cuts the middle of the entry's longest strings, each at most once, until it serializes within
/// `cap` bytes or no string is long enough to cut. Returns the bytes removed.
fn shorten(entry: &mut impl Shortenable, cap: usize) -> usize {
    let mut excess = json_len(entry).saturating_sub(cap);
    let marker_bytes = omission_marker(usize::MAX).len();
    let mut strings = entry.strings();
    strings.sort_by_key(|text| std::cmp::Reverse(text.len()));
    let mut removed = 0;
    for text in strings {
        if excess == 0 || text.len() <= MIN_CUT_STRING_BYTES {
            break;
        }
        let keep = text
            .len()
            .saturating_sub(excess + marker_bytes)
            .max(MIN_CUT_STRING_BYTES);
        let cut = cut_middle(text, keep);
        removed += cut;
        // Escaping only makes the removed text longer once serialized.
        excess = excess.saturating_sub(cut.saturating_sub(marker_bytes));
    }
    removed
}

/// Replaces the middle of `text` with an omission marker, keeping about `keep` bytes split
/// between its head and tail. Returns the bytes removed.
fn cut_middle(text: &mut String, keep: usize) -> usize {
    let mut head = keep / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len().saturating_sub(keep - keep / 2);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let removed = tail.saturating_sub(head);
    let marker = omission_marker(removed);
    if removed <= marker.len() {
        return 0;
    }
    text.replace_range(head..tail, &marker);
    removed
}

/// The in-place mark of a cut. It needs no JSON escaping, so its serialized size is its length.
fn omission_marker(bytes: usize) -> String {
    format!(" [... {bytes} bytes omitted by the host to fit the review context ...] ")
}

pub(crate) fn json_len(value: &impl Serialize) -> usize {
    struct ByteCount(usize);
    impl std::io::Write for ByteCount {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, value).expect("approval context serializes");
    count.0
}

#[cfg(test)]
#[path = "budget_tests.rs"]
mod tests;
