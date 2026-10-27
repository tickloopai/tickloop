//! The values Codemode scripts keep with `store()`: each successful script's changes belong to
//! its Codemode call, so a conversation chain sees only the changes made on it.
use serde_json::Value;
use std::collections::BTreeMap;
use tickloop_harness_protocol::{ChatId, ChatSnapshot};
use tickloop_harness_transcript::CodemodeStoreEntry;

/// Whether `entry` belongs to the chain `snapshot` holds; the values a Chat started with always
/// do.
pub(crate) fn is_on_chain(snapshot: &ChatSnapshot, entry: &CodemodeStoreEntry) -> bool {
    match (entry.run_id, &entry.call_id) {
        (Some(run), Some(call_id)) => {
            crate::nested_calls::tool_use_on_chain(snapshot, run.into(), call_id)
        }
        _ => true,
    }
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

/// The values a Chat copied from the chain `snapshot` holds starts with, recorded under its own
/// id.
pub(crate) fn starting_entries(
    chat_id: ChatId,
    snapshot: &ChatSnapshot,
    entries: &[CodemodeStoreEntry],
) -> Vec<CodemodeStoreEntry> {
    let set = current(snapshot, entries);
    if set.is_empty() {
        return Vec::new();
    }
    vec![CodemodeStoreEntry {
        chat_id: chat_id.into(),
        run_id: None,
        call_id: None,
        set,
        delete: Vec::new(),
    }]
}
