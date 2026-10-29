//! Removes known secret values from text, in the forms they commonly take there: as written
//! (NFC-normalized), percent-encoded and escaped in a JSON string.

use std::collections::{BTreeMap, BTreeSet};

/// What stands in for a secret value.
pub(crate) const REDACTED: &str = "[redacted]";

pub(crate) struct SecretRedactor {
    matcher: aho_corasick::AhoCorasick,
}

impl SecretRedactor {
    /// A redactor for `secrets`; empty ones are ignored.
    pub(crate) fn new<'a>(secrets: impl IntoIterator<Item = &'a String>) -> Self {
        let normalizer = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
        let mut variants = BTreeSet::new();
        for secret in secrets.into_iter().filter(|secret| !secret.is_empty()) {
            let secret = normalizer.normalize(secret).into_owned();
            variants.insert(secret.clone());
            let encoded =
                percent_encoding::utf8_percent_encode(&secret, percent_encoding::NON_ALPHANUMERIC)
                    .to_string();
            variants.insert(encoded.clone());
            variants.insert(encoded.replace("%2F", "%2f").replace("%2B", "%2b"));
            variants.insert(secret.bytes().map(|b| format!("%{b:02X}")).collect());
            variants.insert(secret.bytes().map(|b| format!("%{b:02x}")).collect());
            variants.insert(
                serde_json::to_string(&secret)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_owned(),
            );
        }
        // OAuth tokens are long; avoid determinizing their encoded variants on rotation.
        let matcher = aho_corasick::AhoCorasick::builder()
            .kind(Some(aho_corasick::AhoCorasickKind::ContiguousNFA))
            .build(variants)
            .expect("valid secret patterns");
        Self { matcher }
    }

    pub(crate) fn redact(&self, content: String) -> String {
        let normalizer = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
        let content = normalizer.normalize(&content);
        let mut matches: Vec<_> = self
            .matcher
            .find_overlapping_iter(content.as_bytes())
            .collect();
        if matches.is_empty() {
            return content.into_owned();
        }
        // Preserve longest-pattern priority even when a shorter match starts earlier.
        matches.sort_unstable_by_key(|m| (std::cmp::Reverse(m.len()), m.pattern(), m.start()));
        let mut selected = BTreeMap::new();
        for matched in matches {
            if selected
                .range(..matched.end())
                .next_back()
                .is_some_and(|(_, end)| *end > matched.start())
            {
                continue;
            }
            selected.insert(matched.start(), matched.end());
        }
        let mut result = String::with_capacity(content.len());
        let mut end = 0;
        for (start, next) in selected {
            result.push_str(&content[end..start]);
            result.push_str(REDACTED);
            end = next;
        }
        result.push_str(&content[end..]);
        result
    }
}
