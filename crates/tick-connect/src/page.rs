//! Stable keyset pages over a complete listing.
//!
//! A page never stands for the whole set: callers follow `next` until it is absent. The cursor
//! names the listing generation and the last key already returned, so a changed set expires the
//! cursor explicitly instead of skipping or repeating entries the way a bare offset would.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_PAGE_SIZE: usize = 128;
pub const MAX_PAGE_SIZE: usize = 128;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PageCursor {
    pub generation: String,
    pub after: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Page<T> {
    pub generation: String,
    pub total: usize,
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<PageCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PageError {
    #[error(
        "CursorExpired: the listing changed after this cursor was issued; list again from the first page"
    )]
    CursorExpired,
    #[error("InvalidPageSize: page size must be between 1 and {MAX_PAGE_SIZE}")]
    InvalidPageSize,
    #[error("DuplicateKey: a listing key appears more than once")]
    DuplicateKey,
}

impl PageError {
    /// The public error code: an expired cursor or a bad size is the caller's to fix, while a
    /// duplicate key is a broken listing invariant on the serving side.
    pub fn code(&self) -> &'static str {
        match self {
            Self::CursorExpired => "CursorExpired",
            Self::InvalidPageSize => "InvalidArguments",
            Self::DuplicateKey => "ProtocolViolation",
        }
    }
}

/// Pages `items` in ascending `key` order. Keys must be unique; the generation covers every key,
/// so adding or removing any entry invalidates older cursors.
pub fn paginate<T>(
    items: Vec<T>,
    key: impl Fn(&T) -> String,
    cursor: Option<&PageCursor>,
    limit: Option<usize>,
) -> Result<Page<T>, PageError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(PageError::InvalidPageSize);
    }
    let mut keyed = items
        .into_iter()
        .map(|item| (key(&item), item))
        .collect::<Vec<_>>();
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    if keyed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(PageError::DuplicateKey);
    }
    let mut digest = Sha256::new();
    for (key, _) in &keyed {
        digest.update(key.as_bytes());
        digest.update([0]);
    }
    let generation = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let start = match cursor {
        None => 0,
        Some(cursor) if cursor.generation != generation => return Err(PageError::CursorExpired),
        Some(cursor) => keyed.partition_point(|(key, _)| key.as_str() <= cursor.after.as_str()),
    };
    let total = keyed.len();
    let items = keyed
        .into_iter()
        .skip(start)
        .take(limit)
        .collect::<Vec<_>>();
    let next = (start + items.len() < total).then(|| PageCursor {
        generation: generation.clone(),
        after: items
            .last()
            .expect("nonempty page before the end")
            .0
            .clone(),
    });
    Ok(Page {
        generation,
        total,
        items: items.into_iter().map(|(_, item)| item).collect(),
        next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("hosted:{index:04}"))
            .collect()
    }

    #[test]
    fn walking_every_page_returns_each_entry_exactly_once() {
        let items = names(300);
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = paginate(items.clone(), Clone::clone, cursor.as_ref(), None).unwrap();
            assert_eq!(page.total, 300);
            assert!(page.items.len() <= DEFAULT_PAGE_SIZE);
            seen.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let mut expected = items;
        expected.sort();
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_changed_listing_expires_the_old_cursor() {
        let first = paginate(names(200), Clone::clone, None, None).unwrap();
        let cursor = first.next.unwrap();
        let mut grown = names(200);
        grown.push("hosted:0000a".into());
        assert_eq!(
            paginate(grown, Clone::clone, Some(&cursor), None),
            Err(PageError::CursorExpired)
        );
        let mut shrunk = names(200);
        shrunk.remove(150);
        assert_eq!(
            paginate(shrunk, Clone::clone, Some(&cursor), None),
            Err(PageError::CursorExpired)
        );
    }

    #[test]
    fn page_size_is_bounded_and_duplicates_are_rejected() {
        for limit in [0, MAX_PAGE_SIZE + 1] {
            assert_eq!(
                paginate(names(1), Clone::clone, None, Some(limit)),
                Err(PageError::InvalidPageSize)
            );
        }
        let page = paginate(names(5), Clone::clone, None, Some(2)).unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 5);
        let duplicated = vec!["hosted:a".to_owned(), "hosted:a".to_owned()];
        let error = paginate(duplicated, Clone::clone, None, None).unwrap_err();
        assert_eq!(error.code(), "ProtocolViolation");
    }
}
