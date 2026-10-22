//! What subscriptions keep between deliveries, within bounds: the newest Tick Update data of
//! every subscription of one Agent shares a byte budget, and each subscription's delivered
//! sequence is recorded durably so a repeated update is never delivered twice.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use tickloop_agent_base::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically,
};

/// The newest update data of all of one Agent's subscriptions together.
pub(crate) const CONTENT_CACHE_BYTES: usize = 64 * 1024;
pub(crate) const WATERMARK_DIRECTORY: &str = "tick-watermarks";
const WATERMARK_EXTENSION: &str = ".json";

/// Newest update data per subscription, least recently stored evicted first. Data larger than
/// the whole budget is never kept; an evicted value is read again from its Tick on delivery.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct TickContentCache {
    budget: usize,
    used: usize,
    clock: u64,
    entries: BTreeMap<Uuid, CachedContent>,
    /// The data of a delivery item put back to wait, by subscription, beside its newest data
    /// and within the same budget.
    #[serde(default)]
    put_back: BTreeMap<Uuid, PutBack>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PutBack {
    sequence: u64,
    content: CachedContent,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedContent {
    stored_at: u64,
    bytes: usize,
    data: Value,
}

impl Default for TickContentCache {
    fn default() -> Self {
        Self::with_budget(CONTENT_CACHE_BYTES)
    }
}

impl TickContentCache {
    pub(super) fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            clock: 0,
            entries: BTreeMap::new(),
            put_back: BTreeMap::new(),
        }
    }

    /// Keeps `data` as the subscription's newest; false when it is larger than the whole budget.
    pub(super) fn store(&mut self, subscription: Uuid, data: Value) -> bool {
        self.remove(subscription);
        let Some(content) = self.make_room(data) else {
            return false;
        };
        self.entries.insert(subscription, content);
        true
    }

    /// Keeps the data of the update `sequence` of a delivery item put back to wait, for when its
    /// Tick cannot be read again; false when it is larger than the whole budget.
    pub(super) fn put_back(&mut self, subscription: Uuid, sequence: u64, data: Value) -> bool {
        // What was put back for a newer update of the subscription stays.
        if self
            .put_back
            .get(&subscription)
            .is_some_and(|kept| kept.sequence > sequence)
        {
            return false;
        }
        self.take_put_back(subscription, sequence);
        let Some(content) = self.make_room(data) else {
            return false;
        };
        self.put_back
            .insert(subscription, PutBack { sequence, content });
        true
    }

    /// The data put back for the subscription's update `sequence`; what was put back for another
    /// update of it goes.
    pub(super) fn take_put_back(&mut self, subscription: Uuid, sequence: u64) -> Option<Value> {
        let put_back = self.put_back.remove(&subscription)?;
        self.used -= put_back.content.bytes;
        (put_back.sequence == sequence).then_some(put_back.content.data)
    }

    /// `data` as content to keep, room made for it by evicting the least recently stored;
    /// `None` when it is larger than the whole budget.
    fn make_room(&mut self, data: Value) -> Option<CachedContent> {
        let bytes = serde_json::to_vec(&data).map_or(usize::MAX, |encoded| encoded.len());
        if bytes > self.budget {
            return None;
        }
        while self.used + bytes > self.budget {
            let newest = self
                .entries
                .iter()
                .map(|(id, entry)| (entry.stored_at, *id, false));
            let put_back = self
                .put_back
                .iter()
                .map(|(id, put_back)| (put_back.content.stored_at, *id, true));
            let Some((_, oldest, is_put_back)) = newest.chain(put_back).min() else {
                break;
            };
            if is_put_back {
                let evicted = self.put_back.remove(&oldest).expect("it was just found");
                self.used -= evicted.content.bytes;
            } else {
                self.remove(oldest);
            }
        }
        self.clock += 1;
        self.used += bytes;
        Some(CachedContent {
            stored_at: self.clock,
            bytes,
            data,
        })
    }

    pub(super) fn take(&mut self, subscription: Uuid) -> Option<Value> {
        let entry = self.entries.remove(&subscription)?;
        self.used -= entry.bytes;
        Some(entry.data)
    }

    pub(super) fn remove(&mut self, subscription: Uuid) {
        self.take(subscription);
    }

    pub(super) fn used_bytes(&self) -> usize {
        self.used
    }
}

/// The last update a subscription delivered, recorded before the delivery is handed on.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct DeliveredWatermark {
    pub(super) sequence: u64,
    pub(super) delivery: Uuid,
}

fn watermark_path(directory: &Path, subscription: Uuid) -> PathBuf {
    directory.join(format!("{subscription}{WATERMARK_EXTENSION}"))
}

pub(super) fn read_watermark(
    directory: &Path,
    subscription: Uuid,
) -> io::Result<Option<DeliveredWatermark>> {
    match std::fs::read(watermark_path(directory, subscription)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn write_watermark(
    directory: &Path,
    subscription: Uuid,
    watermark: DeliveredWatermark,
) -> io::Result<()> {
    create_private_directory(directory)?;
    let contents = serde_json::to_vec(&watermark).map_err(io::Error::other)?;
    replace_file_atomically(
        &watermark_path(directory, subscription),
        &contents,
        PRIVATE_FILE_MODE,
    )
}

/// Removes a cancelled subscription's watermark, and the directory once it holds none.
pub(super) fn remove_watermark(directory: &Path, subscription: Uuid) -> io::Result<()> {
    match std::fs::remove_file(watermark_path(directory, subscription)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let _ = std::fs::remove_dir(directory);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn put_back_data_shares_the_budget_and_answers_only_its_own_update() {
        let mut cache = TickContentCache::with_budget(100);
        let (waiting, newest) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(cache.put_back(waiting, 4, json!("x".repeat(40))));
        assert!(cache.store(newest, json!("y".repeat(40))));
        assert_eq!(cache.take_put_back(waiting, 3), None);
        assert!(cache.put_back(waiting, 4, json!("x".repeat(40))));
        assert_eq!(cache.take_put_back(waiting, 4), Some(json!("x".repeat(40))));
        assert!(cache.put_back(waiting, 5, json!("x".repeat(40))));
        // The oldest of both goes first.
        assert!(cache.store(Uuid::new_v4(), json!("z".repeat(40))));
        assert!(cache.take(newest).is_none());
        assert_eq!(cache.take_put_back(waiting, 5), Some(json!("x".repeat(40))));
        assert!(cache.used_bytes() <= 100);
    }

    #[test]
    fn an_older_update_put_back_later_leaves_the_newer_data() {
        let mut cache = TickContentCache::with_budget(1024);
        let subscription = Uuid::new_v4();
        assert!(cache.put_back(subscription, 2, json!("newer")));
        assert!(!cache.put_back(subscription, 1, json!("older")));
        assert_eq!(cache.take_put_back(subscription, 2), Some(json!("newer")));
    }

    #[test]
    fn the_cache_stays_within_its_budget_and_evicts_the_oldest_first() {
        let mut cache = TickContentCache::with_budget(100);
        let (first, second, third) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        cache.store(first, json!("a".repeat(40)));
        cache.store(second, json!("b".repeat(40)));
        assert!(cache.used_bytes() <= 100);
        cache.store(third, json!("c".repeat(40)));
        assert!(cache.used_bytes() <= 100, "{}", cache.used_bytes());
        assert_eq!(cache.take(first), None, "the oldest was evicted");
        assert_eq!(cache.take(second), Some(json!("b".repeat(40))));
        assert!(!cache.store(first, json!("x".repeat(200))));
        assert_eq!(cache.take(first), None, "larger than the whole budget");
        cache.remove(third);
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    fn a_watermark_is_durable_and_cancelling_removes_it_and_its_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(WATERMARK_DIRECTORY);
        let subscription = Uuid::new_v4();
        assert_eq!(read_watermark(&directory, subscription).unwrap(), None);
        let watermark = DeliveredWatermark {
            sequence: 7,
            delivery: Uuid::new_v4(),
        };
        write_watermark(&directory, subscription, watermark).unwrap();
        assert_eq!(
            read_watermark(&directory, subscription).unwrap(),
            Some(watermark)
        );
        remove_watermark(&directory, subscription).unwrap();
        assert!(!directory.exists());
    }
}
