use super::*;

pub(super) struct Redactor {
    secrets: std::collections::BTreeSet<String>,
    matcher: aho_corasick::AhoCorasick,
}
impl Redactor {
    pub(super) fn new(entry: &Entry) -> Option<Self> {
        let mut secrets = entry.environment_secrets.clone();
        secrets.extend(entry.credential_secrets.read().unwrap().iter().cloned());
        secrets.extend(entry.config.sensitive_values.iter().map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        }));
        if let Some(token) = entry.token.read().unwrap().as_ref() {
            secrets.push(token.secret().to_owned());
        }
        if let Some(token) = entry
            .credential
            .read()
            .unwrap()
            .as_ref()
            .and_then(|saved| saved.token.as_ref())
        {
            secrets.push(token.access_token.clone());
            secrets.extend(token.refresh_token.clone());
        }
        match &entry.config.transport {
            McpTransport::Http { url, headers } => {
                for value in headers.values() {
                    secrets.push(value.clone());
                    let mut parts = value.split_whitespace();
                    if parts.next().is_some_and(|scheme| {
                        scheme.eq_ignore_ascii_case("bearer")
                            || scheme.eq_ignore_ascii_case("basic")
                    }) && let Some(token) = parts.next()
                    {
                        secrets.push(token.into());
                    }
                }
                if let Ok(url) = reqwest::Url::parse(url) {
                    secrets.extend(url.query_pairs().map(|(_, value)| value.into_owned()));
                }
            }
            McpTransport::Stdio { env, args, .. } => {
                secrets.extend(env.values().cloned());
                for argument in args {
                    if let Some((_, value)) = argument.split_once('=') {
                        secrets.push(value.into());
                    } else if !argument.starts_with('-') {
                        secrets.push(argument.clone());
                    }
                }
            }
            _ => {}
        }
        let secrets: std::collections::BTreeSet<_> = secrets
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .collect();
        if entry
            .redactor
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|old| old.secrets == secrets)
        {
            return None;
        }
        let normalizer = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
        let mut variants = std::collections::BTreeSet::new();
        for secret in &secrets {
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
        #[cfg(feature = "test-support")]
        super::test_pauses::wait(
            super::test_pauses::PausePoint::RedactorBuild,
            &entry.config.name,
        );
        // OAuth tokens are long; avoid determinizing their encoded variants on rotation.
        let matcher = aho_corasick::AhoCorasick::builder()
            .kind(Some(aho_corasick::AhoCorasickKind::ContiguousNFA))
            .build(variants)
            .expect("valid secret patterns");
        Some(Self { secrets, matcher })
    }
    /// Whether the redactor redacts `secret`.
    pub(super) fn covers(&self, secret: &str) -> bool {
        self.secrets.contains(secret)
    }
    pub(super) fn redact(&self, content: String) -> String {
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
            result.push_str("[redacted]");
            end = next;
        }
        result.push_str(&content[end..]);
        result
    }
}
