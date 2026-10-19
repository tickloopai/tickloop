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
        }
    }

    /// Keeps `data` as the subscription's newest; false when it is larger than the whole budget.
    pub(super) fn store(&mut self, subscription: Uuid, data: Value) -> bool {
        self.remove(subscription);
        let bytes = serde_json::to_vec(&data).map_or(usize::MAX, |encoded| encoded.len());
        if bytes > self.budget {
            return false;
        }
        while self.used + bytes > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.remove(oldest);
        }
        self.clock += 1;
        self.used += bytes;
        self.entries.insert(
            subscription,
            CachedContent {
                stored_at: self.clock,
                bytes,
                data,
            },
        );
        true
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
