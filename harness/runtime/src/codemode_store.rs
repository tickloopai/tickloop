//! The values Codemode scripts keep with `store()`: each successful script's changes belong to
//! its Codemode call, so a conversation chain sees only the changes made on it.
use serde_json::Value;
use std::collections::BTreeMap;
use tickloop_harness_protocol::{ChatId, ChatSnapshot};
use tickloop_harness_transcript::CodemodeStoreEntry;

/// Whether `entry` belongs to the chain `snapshot` holds; a whole store always does.
pub(crate) fn is_on_chain(snapshot: &ChatSnapshot, entry: &CodemodeStoreEntry) -> bool {
    entry.parent_call().is_none_or(|(run, call_id)| {
        crate::nested_calls::tool_use_on_chain(snapshot, run.into(), call_id)
    })
}

/// The values a script on the chain `snapshot` holds loads.
pub(crate) fn current(
    snapshot: &ChatSnapshot,
    entries: &[CodemodeStoreEntry],
) -> BTreeMap<String, Value> {
    let mut store = BTreeMap::new();
    for entry in entries.iter().filter(|entry| is_on_chain(snapshot, entry)) {
        entry.apply_to(&mut store);
    }
    store
}

/// The whole store of the chain `snapshot` holds, recorded under `chat_id`, for a Chat that
/// goes on without the calls that stored it: a Branch, a copy under a new Chat id, or the Chat
/// after a compaction. A Chat that never stored anything needs none.
pub(crate) fn whole_store(
    chat_id: ChatId,
    snapshot: &ChatSnapshot,
    entries: &[CodemodeStoreEntry],
) -> Option<CodemodeStoreEntry> {
    (!entries.is_empty()).then(|| CodemodeStoreEntry {
        chat_id: chat_id.into(),
        run_id: None,
        call_id: None,
        set: current(snapshot, entries),
        delete: Vec::new(),
    })
}
