use std::{sync::OnceLock, time::Duration};

use regex::Regex;
use serde_json::Value;

use tickloop_agent_protocol::{ModelConfig, ProviderFailureClass, ProviderHttpFailure};

use super::ModelFailureKind;

const MAX_MESSAGE_CHARS: usize = 1024;
const TRUNCATION_SUFFIX: &str = "...";
const MAX_RETRY_AFTER_SECONDS: u64 = 86400;
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(15);
const REDACTED: &str = "***";

/// Building a [`ProviderHttpFailure`] from a provider's HTTP rejection; the record itself lives
/// in the protocol crate.
pub(super) trait ProviderHttpFailureExt: Sized {
    fn from_response(
        config: &ModelConfig,
        status: u16,
        bytes: &[u8],
        retry_after: Option<Duration>,
        server_retry: Option<bool>,
        secrets: &[&str],
    ) -> Self;
    fn set_class(&mut self, class: ProviderFailureClass, server_retry: Option<bool>);
    fn require_complete_body(&mut self, complete: bool, secrets: &[&str]);
}

impl ProviderHttpFailureExt for ProviderHttpFailure {
    fn from_response(
        config: &ModelConfig,
        status: u16,
        bytes: &[u8],
        retry_after: Option<Duration>,
        server_retry: Option<bool>,
        secrets: &[&str],
    ) -> Self {
        let body = serde_json::from_slice::<Value>(bytes).ok();
        let error = body.as_ref().map(|body| {
            body.get("error")
                .filter(|error| error.is_object())
                .unwrap_or(body)
        });
        let raw = String::from_utf8_lossy(bytes);
        let message = error
            .and_then(|error| error["message"].as_str())
            .or_else(|| body.as_ref().and_then(|body| body["error"].as_str()))
            .unwrap_or_else(|| {
                if body.is_none() {
                    &raw
                } else {
                    "The provider did not supply an error message."
                }
            });
        let codes = error
            .map(|error| {
                ["code", "type", "status"]
                    .into_iter()
                    .filter_map(|field| error.get(field))
                    .map(Value::to_string)
                    .chain(error["details"]["error_code"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let signal = format!("{codes} {message}").to_ascii_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|needle| signal.contains(needle));
        let has_account_limit = has(&[
            "insufficient_quota",
            "usage_limit",
            "usage limit",
            "enforced_spend_limit_reached",
            "monthly limit",
            "credits_exhausted",
            "gousagelimiterror",
            "freeusagelimiterror",
        ]);
        let has_rate_window = has(&[
            "rate_limit",
            "rate limit",
            "per minute",
            "per second",
            "requests per",
            "tokens per",
        ]);
        let class = if config.provider_id == "kimi-code-oauth"
            && status == 403
            && has(&["access_terminated_error"])
            && has_account_limit
        {
            ProviderFailureClass::UsageOrPlan
        } else if has_account_limit
            || has(&[
                "quota exceeded",
                "quota exhausted",
                "quota_exceeded",
                "quota_exhausted",
                "resource_exhausted",
                "exceeded your quota",
            ]) && !has_rate_window
        {
            ProviderFailureClass::UsageLimit
        } else if status == 402
            || has(&[
                "billing",
                "insufficient_balance",
                "insufficient balance",
                "credit balance",
                "payment_required",
            ])
        {
            ProviderFailureClass::Billing
        } else if status == 401
            || has(&[
                "authentication_error",
                "unauthenticated",
                "invalid_api_key",
                "invalid_token",
                "expired_token",
                "token expired",
                "invalid api key",
            ])
        {
            ProviderFailureClass::Authentication
        } else if has(&[
            "usage_not_included",
            "model_not_found",
            "model_not_enabled",
            "model_not_available",
            "model not available",
            "not included",
            "not entitled",
            "plan does not",
            "subscription_required",
            "unsupported_model",
            "model_not_supported",
            "permission_denied",
        ]) || status == 403
            || status == 404
        {
            ProviderFailureClass::ModelUnavailable
        } else if status == 429 || has_rate_window || has(&["too many requests"]) {
            ProviderFailureClass::RateLimit
        } else if status >= 500 {
            ProviderFailureClass::ServerError
        } else {
            ProviderFailureClass::Refused
        };
        let mut failure = Self {
            provider_id: config.provider_id.clone(),
            provider_name: if config.provider_name.is_empty() {
                config.name.clone()
            } else {
                config.provider_name.clone()
            },
            status,
            class,
            message: message.into(),
            hint: String::new(),
            retry_after,
            retryable: false,
        };
        failure.set_class(class, server_retry);
        sanitize_fields(&mut failure, secrets);
        failure
    }

    fn set_class(&mut self, class: ProviderFailureClass, server_retry: Option<bool>) {
        self.class = class;
        self.retryable = (matches!(
            class,
            ProviderFailureClass::RateLimit | ProviderFailureClass::ServerError
        ) || class == ProviderFailureClass::Refused
            && matches!(self.status, 408 | 409))
            && server_retry != Some(false)
            && self
                .retry_after
                .is_none_or(|delay| delay <= Duration::from_secs(MAX_RETRY_AFTER_SECONDS));
        self.hint = match class {
            ProviderFailureClass::UsageLimit => "Usage limit reached. Check the provider's allowance/reset or upgrade options; switch Provider via /provider to continue.".into(),
            ProviderFailureClass::UsageOrPlan => "Usage limit OR the current plan does not include this model. Check K3 plan access and usage limits via the provider's subscription link, or switch Provider via /provider. K3 may require a higher Kimi Code plan.".into(),
            ProviderFailureClass::Billing => "Billing / insufficient balance. Review the provider's billing or add balance; switch Provider via /provider to continue.".into(),
            ProviderFailureClass::Authentication => "Credentials expired or invalid. Open this connection in /provider to sign in again or replace its API key.".into(),
            ProviderFailureClass::RateLimit => match self.retry_after {
                Some(delay) => format!("Rate limit. Retry after {} seconds, or switch Provider via /provider.", delay.as_secs()),
                None => "Rate limit. Wait before retrying, or switch Provider via /provider.".into(),
            },
            ProviderFailureClass::ModelUnavailable => "Model / plan access denied. Check this model's availability and account permissions; choose another model via /model or switch Provider via /provider.".into(),
            ProviderFailureClass::ServerError => "Provider server error. Retry later, or switch Provider via /provider.".into(),
            ProviderFailureClass::Refused => "Request refused. Review the provider's message; choose another model via /model or switch Provider via /provider.".into(),
        };
        if self
            .retry_after
            .is_some_and(|delay| delay > Duration::from_secs(MAX_RETRY_AFTER_SECONDS))
        {
            self.hint
                .push_str(" Automatic retry is paused because the wait exceeds one day.");
        }
    }

    fn require_complete_body(&mut self, complete: bool, secrets: &[&str]) {
        if !complete {
            self.retryable = false;
            self.hint
                .push_str(" Rejection body incomplete; automatic retry is paused.");
        }
        sanitize_fields(self, secrets);
    }
}

pub(super) fn failure_kind(failure: &ProviderHttpFailure) -> ModelFailureKind {
    match failure.class {
        ProviderFailureClass::UsageLimit | ProviderFailureClass::Billing => {
            ModelFailureKind::QuotaExhausted
        }
        ProviderFailureClass::UsageOrPlan | ProviderFailureClass::ModelUnavailable => {
            ModelFailureKind::EntitlementDenied
        }
        ProviderFailureClass::Authentication => ModelFailureKind::AuthenticationFailed,
        ProviderFailureClass::RateLimit => ModelFailureKind::RateLimited,
        ProviderFailureClass::ServerError | ProviderFailureClass::Refused => {
            ModelFailureKind::ProviderFailure
        }
    }
}

const MIN_SECRET_FRAGMENT_CHARS: usize = 8;
const MAX_DECODING_PASSES: usize = 16;
const SAFE_MESSAGE: &str = "Provider response hidden for safety.";

fn normalize_text(text: &str) -> String {
    use icu_properties::{
        CodePointMapData, CodePointSetData,
        props::{DefaultIgnorableCodePoint, GeneralCategory},
    };
    let text = terminal_escapes().replace_all(text, "");
    let normalized = if text.is_ascii() {
        std::borrow::Cow::Borrowed(text.as_ref())
    } else {
        icu_normalizer::ComposingNormalizer::new_nfkc().normalize(&text)
    };
    let categories = CodePointMapData::<GeneralCategory>::new();
    let ignorables = CodePointSetData::new::<DefaultIgnorableCodePoint>();
    let visible: String = normalized
        .chars()
        .filter(|character| {
            if character.is_ascii() {
                return !character.is_ascii_control();
            }
            !matches!(
                categories.get(*character),
                GeneralCategory::Control
                    | GeneralCategory::Format
                    | GeneralCategory::LineSeparator
                    | GeneralCategory::ParagraphSeparator
            ) && !ignorables.contains(*character)
        })
        .collect();
    visible.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_text(text: &str) -> Option<String> {
    static ESCAPES: OnceLock<Regex> = OnceLock::new();
    static ENTITIES: OnceLock<Regex> = OnceLock::new();
    static PERCENT: OnceLock<Regex> = OnceLock::new();
    let entities_matcher = ENTITIES.get_or_init(|| {
        Regex::new(r"&(?:#[xX][0-9a-fA-F]{1,6}|#[0-9]{1,7}|[A-Za-z][A-Za-z0-9]{1,32});?")
            .expect("valid character reference regex")
    });
    let escapes_matcher = ESCAPES.get_or_init(|| {
        Regex::new(r"(?i)\\(?:u\{([0-9a-f]{1,6})\}|u([0-9a-f]{4})|x([0-9a-f]{2}))")
            .expect("valid encoded character regex")
    });
    let percent_matcher = PERCENT.get_or_init(|| {
        Regex::new(r"(?i)(?:%[0-9a-f]{2})+").expect("valid percent encoding regex")
    });
    let mut current = text.to_owned();
    let mut has_changed_framing = false;
    for _ in 0..MAX_DECODING_PASSES {
        let entities = decode_transform(
            &current,
            entities_matcher,
            &mut has_changed_framing,
            |captures| {
                let entity = &captures[0][1..];
                let value = entity.trim_end_matches(';');
                if let Some(hex) = value
                    .strip_prefix("#x")
                    .or_else(|| value.strip_prefix("#X"))
                {
                    return u32::from_str_radix(hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map_or_else(|| captures[0].to_owned(), |c| c.to_string());
                }
                if let Some(decimal) = value.strip_prefix('#') {
                    return decimal
                        .parse::<u32>()
                        .ok()
                        .and_then(char::from_u32)
                        .map_or_else(|| captures[0].to_owned(), |c| c.to_string());
                }
                web_atoms::NAMED_ENTITIES.get(entity).map_or_else(
                    || captures[0].to_owned(),
                    |&(first, second)| {
                        [first, second]
                            .into_iter()
                            .filter(|value| *value != 0)
                            .filter_map(char::from_u32)
                            .collect()
                    },
                )
            },
        );
        let percent = decode_transform(
            &entities,
            percent_matcher,
            &mut has_changed_framing,
            |captures| {
                percent_encoding::percent_decode_str(&captures[0])
                    .decode_utf8_lossy()
                    .into_owned()
            },
        );
        let escapes = decode_transform(
            &percent,
            escapes_matcher,
            &mut has_changed_framing,
            |captures| {
                let digits = captures.iter().skip(1).flatten().next().unwrap().as_str();
                u32::from_str_radix(digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map_or_else(|| captures[0].to_owned(), |character| character.to_string())
            },
        );
        has_changed_framing |= normalization_changes_framing(&escapes);
        let next = normalize_source(&escapes);
        if has_changed_framing && credential_labels().is_match(&html_tags().replace_all(&next, ""))
        {
            return None;
        }
        if next == current {
            return Some(next);
        }
        current = next;
    }
    None
}

fn contains_authentication_boundary(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            character,
            '"' | '\'' | '\\' | '\r' | '\n' | '\u{2028}' | '\u{2029}'
        )
    })
}

fn decode_transform(
    source: &str,
    matcher: &Regex,
    has_changed_framing: &mut bool,
    decode: impl Fn(&regex::Captures<'_>) -> String,
) -> String {
    matcher
        .replace_all(source, |captures: &regex::Captures<'_>| {
            let replacement = decode(captures);
            *has_changed_framing |= replacement != captures[0]
                && (contains_authentication_boundary(&captures[0])
                    || contains_authentication_boundary(&replacement));
            replacement
        })
        .into_owned()
}

fn normalization_changes_framing(source: &str) -> bool {
    let normalizer = icu_normalizer::ComposingNormalizer::new_nfkc();
    terminal_escapes()
        .find_iter(source)
        .any(|escape| contains_authentication_boundary(escape.as_str()))
        || source
            .chars()
            .filter(|character| !character.is_ascii())
            .any(|character| {
                let original = character.to_string();
                let normalized = normalizer.normalize(&original);
                normalized != original
                    && (contains_authentication_boundary(&original)
                        || contains_authentication_boundary(&normalized))
            })
}

fn terminal_escapes() -> &'static Regex {
    static ESCAPES: OnceLock<Regex> = OnceLock::new();
    ESCAPES.get_or_init(|| {
        Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))")
            .expect("valid terminal escape regex")
    })
}

fn normalize_source(text: &str) -> String {
    // Retain value boundaries until authentication constructs have been removed.
    text.split(['\r', '\n', '\u{2028}', '\u{2029}'])
        .map(normalize_text)
        .collect::<Vec<_>>()
        .join("\n")
}

struct CredentialSkeleton {
    text: String,
    source_spans: Vec<std::ops::Range<usize>>,
}

impl CredentialSkeleton {
    fn new(source: &str) -> Self {
        let mut text = String::new();
        let mut source_spans = Vec::new();
        let mut tags = html_tags().find_iter(source).peekable();
        for (start, character) in source.char_indices() {
            while tags.peek().is_some_and(|tag| tag.end() <= start) {
                tags.next();
            }
            if tags.peek().is_some_and(|tag| tag.start() <= start) {
                continue;
            }
            let end = start + character.len_utf8();
            for character in character.to_lowercase().filter(|c| c.is_alphanumeric()) {
                text.push(character);
                source_spans.extend(std::iter::repeat_n(start..end, character.len_utf8()));
            }
        }
        Self { text, source_spans }
    }
}

struct FailureSanitizer {
    secret_matcher: Option<Regex>,
    literal_matcher: Option<Regex>,
    has_ambiguous_secret: bool,
}

impl FailureSanitizer {
    fn new(secrets: &[&str]) -> Self {
        let mut patterns = std::collections::BTreeSet::new();
        let mut literals = std::collections::BTreeSet::new();
        let mut has_ambiguous_secret = false;
        for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
            let Some(normalized) = decode_text(secret) else {
                has_ambiguous_secret = true;
                continue;
            };
            let skeleton = CredentialSkeleton::new(&normalized).text;
            literals.insert(regex::escape(&normalized));
            for fragment in normalized
                .chars()
                .collect::<Vec<_>>()
                .windows(MIN_SECRET_FRAGMENT_CHARS)
            {
                literals.insert(regex::escape(&fragment.iter().collect::<String>()));
            }
            let chars: Vec<_> = skeleton.chars().collect();
            if chars.is_empty() {
                has_ambiguous_secret = true;
                continue;
            }
            patterns.insert(regex::escape(&skeleton));
            for fragment in chars.windows(MIN_SECRET_FRAGMENT_CHARS) {
                patterns.insert(regex::escape(&fragment.iter().collect::<String>()));
            }
        }
        let mut patterns: Vec<_> = patterns.into_iter().collect();
        patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.len()));
        let secret_matcher = (!patterns.is_empty())
            .then(|| Regex::new(&patterns.join("|")).expect("valid literal credential skeletons"));
        let literal_matcher = (!literals.is_empty()).then(|| {
            regex::RegexBuilder::new(&literals.into_iter().collect::<Vec<_>>().join("|"))
                .case_insensitive(true)
                .build()
                .expect("valid literal credential patterns")
        });
        Self {
            secret_matcher,
            literal_matcher,
            has_ambiguous_secret,
        }
    }

    fn redact_known(&self, source: &str) -> Option<String> {
        if self.has_ambiguous_secret {
            return None;
        }
        let Some(matcher) = &self.secret_matcher else {
            return Some(source.into());
        };
        let skeleton = CredentialSkeleton::new(source);
        let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
        for found in matcher.find_iter(&skeleton.text) {
            let start = skeleton.source_spans.get(found.start())?.start;
            let end = skeleton.source_spans.get(found.end().checked_sub(1)?)?.end;
            if !source.is_char_boundary(start) || !source.is_char_boundary(end) || start >= end {
                return None;
            }
            spans.push(start..end);
        }
        if let Some(matcher) = &self.literal_matcher {
            spans.extend(matcher.find_iter(source).map(|found| found.range()));
        }
        redact_spans(source, spans)
    }

    fn redact_auth_constructs(&self, source: &str) -> String {
        let mut view = source.to_owned();
        for token in url_tokens().find_iter(source) {
            let scheme = token.as_str().split_once(':').unwrap().0;
            if !is_credential_label(scheme) {
                view.replace_range(token.range(), &"x".repeat(token.len()));
            }
        }
        let spans = credential_labels()
            .find_iter(&view)
            .map(|found| found.range())
            .collect();
        redact_spans(source, spans).unwrap_or_else(|| REDACTED.into())
    }

    fn redact(&self, text: &str) -> String {
        let Some(text) = decode_text(text).and_then(|text| self.redact_known(&text)) else {
            return REDACTED.into();
        };
        let text = credential_labels().replace_all(&text, REDACTED);
        static FORMATS: OnceLock<Regex> = OnceLock::new();
        let text = FORMATS.get_or_init(|| Regex::new(r"(?i)\b(?:sk-|ghp_|github_pat_)[a-z0-9_-]+|\beyJ[a-z0-9_-]+\.[a-z0-9_-]+\.[a-z0-9_-]+").expect("valid credential format regex"))
            .replace_all(&text, REDACTED);
        self.redact_known(&text).unwrap_or_else(|| REDACTED.into())
    }

    fn url(&self, token: &str) -> String {
        let address = token.trim_end_matches(['.', ',', ';', ')']);
        let punctuation = &token[address.len()..];
        let Ok(mut url) = reqwest::Url::parse(address) else {
            return REDACTED.into();
        };
        let _ = url.set_username("");
        let _ = url.set_password(None);
        if let Some(segments) = url.path_segments().map(|segments| {
            segments
                .map(|segment| self.redact(segment))
                .collect::<Vec<_>>()
        }) {
            if let Ok(mut path) = url.path_segments_mut() {
                path.clear().extend(segments);
            }
        } else {
            let path = self.redact(url.path());
            url.set_path(&path);
        }
        let pairs = url
            .query_pairs()
            .filter_map(|(key, value)| {
                let normalized_key = decode_text(&key)?;
                if is_credential_label(&normalized_key) {
                    return None;
                }
                Some((self.redact(&normalized_key), self.redact(&value)))
            })
            .collect::<Vec<_>>();
        if url.query().is_some() {
            url.query_pairs_mut().clear().extend_pairs(pairs);
        }
        if let Some(fragment) = url.fragment() {
            let fragment = self.redact(fragment);
            url.set_fragment(Some(&fragment));
        }
        let serialized = format!("{url}{punctuation}");
        if self.leaks(&serialized) {
            "***".into()
        } else {
            serialized
        }
    }

    fn content(&self, text: &str) -> String {
        let Some(decoded) = decode_text(text) else {
            return self.safe_message();
        };
        let text = normalize_source(text);
        let Some(redacted) = self.redact_known(&decoded) else {
            return REDACTED.into();
        };
        // Keep URI escaping when no credential span requires canonical-source replacement.
        let redacted = self.redact_auth_constructs(&redacted);
        let text = if redacted == decoded { text } else { redacted };
        let matcher = url_tokens();
        let mut output = String::new();
        let mut start = 0;
        for token in matcher.find_iter(&text) {
            let scheme = token.as_str().split_once(':').unwrap().0;
            if is_credential_label(scheme) {
                continue;
            }
            output.push_str(&self.redact(&text[start..token.start()]));
            output.push(' ');
            output.push_str(&self.url(token.as_str()));
            output.push(' ');
            start = token.end();
        }
        output.push_str(&self.redact(&text[start..]));
        normalize_text(
            &self
                .redact_known(&output)
                .unwrap_or_else(|| REDACTED.into()),
        )
    }

    fn leaks(&self, text: &str) -> bool {
        if self.has_ambiguous_secret {
            return true;
        }
        let Some(text) = decode_text(text) else {
            return true;
        };
        let skeleton = CredentialSkeleton::new(&text).text;
        self.literal_matcher
            .as_ref()
            .is_some_and(|matcher| matcher.is_match(&text))
            || self
                .secret_matcher
                .as_ref()
                .is_some_and(|matcher| matcher.is_match(&skeleton))
    }

    fn known_matches(&self, text: &str) -> Option<std::collections::BTreeSet<String>> {
        if self.has_ambiguous_secret {
            return None;
        }
        let decoded = decode_text(text)?;
        let skeleton = CredentialSkeleton::new(&decoded).text;
        let mut matches = std::collections::BTreeSet::new();
        if let Some(matcher) = &self.literal_matcher {
            for (offset, _) in decoded.char_indices() {
                if let Some(found) = matcher.find_at(&decoded, offset)
                    && found.start() == offset
                {
                    matches.insert(CredentialSkeleton::new(found.as_str()).text);
                }
            }
        }
        if let Some(matcher) = &self.secret_matcher {
            for (offset, _) in skeleton.char_indices() {
                if let Some(found) = matcher.find_at(&skeleton, offset)
                    && found.start() == offset
                {
                    matches.insert(found.as_str().to_owned());
                }
            }
        }
        Some(matches)
    }

    fn sanitize(&self, text: &str) -> String {
        let mut text = self.content(text);
        let markup = text.to_ascii_lowercase();
        if (markup.starts_with("<html")
            || markup.starts_with("<!doctype")
            || markup.starts_with("<body")
            || markup.contains("<a "))
            && let Ok(readable) = htmd::convert(&text)
        {
            static MARKDOWN_ESCAPES: OnceLock<Regex> = OnceLock::new();
            let readable = MARKDOWN_ESCAPES
                .get_or_init(|| {
                    Regex::new(r"\\([\\`*_{}\[\]()#+\-.!>])").expect("valid markdown escape regex")
                })
                .replace_all(&readable, "$1");
            text = self.content(&readable);
        }
        static AUTO_LINKS: OnceLock<Regex> = OnceLock::new();
        text = self.content(
            &AUTO_LINKS
                .get_or_init(|| {
                    Regex::new(r"(?i)<\s*([a-z][a-z0-9+.-]*://[^>\s]+)\s*>")
                        .expect("valid autolink regex")
                })
                .replace_all(&text, "$1"),
        );
        text = self.content(&html_tags().replace_all(&text, ""));
        if self.leaks(&text) {
            return self.safe_message();
        }
        let mut bounded = text.clone();
        if text.chars().count() > MAX_MESSAGE_CHARS {
            bounded = text
                .chars()
                .take(MAX_MESSAGE_CHARS - TRUNCATION_SUFFIX.len())
                .collect();
            if let Some(url) = url_tokens()
                .find_iter(&text)
                .find(|url| url.start() < bounded.len() && url.end() > bounded.len())
            {
                bounded.truncate(url.start());
            }
            bounded.push_str(TRUNCATION_SUFFIX);
        }
        let bounded = self.content(&bounded);
        let published = if bounded.trim().is_empty() {
            "The provider returned an empty error body.".into()
        } else {
            bounded
                .chars()
                .take(MAX_MESSAGE_CHARS + 1)
                .collect::<String>()
        };
        if self.leaks(&published) {
            self.safe_message()
        } else {
            published
        }
    }

    fn safe_message(&self) -> String {
        if self.leaks(SAFE_MESSAGE) {
            REDACTED.into()
        } else {
            SAFE_MESSAGE.into()
        }
    }
}

fn redact_spans(source: &str, mut spans: Vec<std::ops::Range<usize>>) -> Option<String> {
    spans.sort_by_key(|span| span.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for span in spans {
        if span.start >= span.end
            || !source.is_char_boundary(span.start)
            || !source.is_char_boundary(span.end)
        {
            return None;
        }
        if let Some(previous) = merged.last_mut()
            && span.start <= previous.end
        {
            previous.end = previous.end.max(span.end);
        } else {
            merged.push(span);
        }
    }
    let mut output = String::new();
    let mut start = 0;
    for span in merged {
        output.push_str(source.get(start..span.start)?);
        output.push_str(REDACTED);
        start = span.end;
    }
    output.push_str(source.get(start..)?);
    Some(output)
}

fn is_credential_label(label: &str) -> bool {
    let label: String = label
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        label.as_str(),
        "authorization"
            | "proxyauthorization"
            | "apikey"
            | "xapikey"
            | "accesstoken"
            | "refreshtoken"
            | "oauthtoken"
            | "authtoken"
            | "token"
            | "bearer"
            | "basic"
            | "digest"
            | "negotiate"
            | "password"
            | "secret"
            | "key"
            | "signature"
            | "clientsecret"
    )
}

fn url_tokens() -> &'static Regex {
    static URLS: OnceLock<Regex> = OnceLock::new();
    URLS.get_or_init(|| {
        Regex::new(r#"(?i)\b[a-z][a-z0-9+.-]*:(?://)?[^\s<>"']+"#).expect("valid URI token regex")
    })
}

fn html_tags() -> &'static Regex {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    TAGS.get_or_init(|| Regex::new(r"(?is)<[^>]*>").expect("valid HTML tag regex"))
}

pub(super) fn retry_headers(
    headers: &reqwest::header::HeaderMap,
) -> (Option<Duration>, Option<bool>) {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    let server_retry = headers
        .get("x-should-retry")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| match value.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        });
    (retry_after, server_retry)
}

pub(super) fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline =
        time::OffsetDateTime::parse(value.trim(), &time::format_description::well_known::Rfc2822)
            .ok()?;
    let seconds = (deadline - time::OffsetDateTime::now_utc())
        .whole_seconds()
        .max(0) as u64;
    Some(Duration::from_secs(seconds))
}

pub(super) struct ErrorBody {
    pub bytes: Vec<u8>,
    pub complete: bool,
}

pub(super) async fn read_error_body(
    response: &mut reqwest::Response,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ErrorBody, super::ModelError> {
    let mut body = ErrorBody {
        bytes: Vec::new(),
        complete: false,
    };
    let deadline = tokio::time::sleep(ERROR_BODY_TIMEOUT);
    tokio::pin!(deadline);
    while body.bytes.len() < super::MAX_ERROR_BODY_BYTES {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(super::ModelError::Cancelled),
            _ = &mut deadline => break,
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(chunk)) => {
                let remaining = super::MAX_ERROR_BODY_BYTES - body.bytes.len();
                body.bytes
                    .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(None) => {
                body.complete = true;
                break;
            }
            Err(_) => break,
        }
    }
    Ok(body)
}

fn sanitize_fields(failure: &mut ProviderHttpFailure, secrets: &[&str]) {
    let sanitizer = FailureSanitizer::new(secrets);
    for text in [
        &mut failure.provider_id,
        &mut failure.provider_name,
        &mut failure.message,
        &mut failure.hint,
    ] {
        *text = sanitizer.sanitize(text);
    }
    if has_unsafe_rendering(failure, &sanitizer) {
        // A terminal mask contributes no letters to a later field join.
        failure.provider_id = REDACTED.into();
        failure.provider_name = REDACTED.into();
        failure.message = REDACTED.into();
        failure.hint = REDACTED.into();
    }
}

fn has_unsafe_rendering(failure: &ProviderHttpFailure, sanitizer: &FailureSanitizer) -> bool {
    // Client identity and protocol metadata are independent of the provider's error text.
    let mut metadata = failure.clone();
    for text in [&mut metadata.message, &mut metadata.hint] {
        text.clear();
    }
    let (Ok(serialized), Ok(metadata_serialized)) = (
        serde_json::to_string(failure),
        serde_json::to_string(&metadata),
    ) else {
        return true;
    };
    [
        (failure.to_string(), metadata.to_string()),
        (format!("{failure:?}"), format!("{metadata:?}")),
        (failure.summary(), metadata.summary()),
        (serialized, metadata_serialized),
    ]
    .iter()
    .any(|(rendered, metadata)| {
        let (Some(matches), Some(public_matches)) = (
            sanitizer.known_matches(rendered),
            sanitizer.known_matches(metadata),
        ) else {
            return true;
        };
        !matches.is_subset(&public_matches)
    })
}

fn credential_labels() -> &'static Regex {
    static LABELS: OnceLock<Regex> = OnceLock::new();
    LABELS.get_or_init(|| Regex::new(r#"(?ix)\b(?:(?:proxy[\s_-]*)?authorization|(?:x[\s_-]*)?api[\s_-]*key|(?:access|refresh|oauth|auth)?[\s_-]*token|bearer|basic|digest|negotiate|password|secret|signature|client[\s_-]*secret)\b[\s`*_\[\]()-]*(?:["']?\s*[:=][\s`*_]*)?(?:"(?:\\[^\r\n]|[^"\\\r\n])*"?|'(?:\\[^\r\n]|[^'\\\r\n])*'?|[^\s"'][^\r\n]*)"#).expect("valid credential grammar"))
}

#[cfg(test)]
mod security_properties {
    use super::*;

    #[test]
    fn separated_fragments_and_complete_authentication_constructs_are_removed() {
        let sanitizer = FailureSanitizer::new(&["opaque-fixture-credential"]);
        let skeleton = "opaquefixturecredential";
        for width in 1..=8 {
            for separator in [
                " ", "/", ":", "--", "%20", "&#32;", "<b></b>", "<i> </i>", "\u{034f}",
            ] {
                for end in 8..=skeleton.len() {
                    let split = skeleton.as_bytes()[..end]
                        .chunks(width)
                        .map(|chunk| std::str::from_utf8(chunk).unwrap())
                        .collect::<Vec<_>>()
                        .join(separator);
                    for value in [
                        format!("Rejected {split}"),
                        format!("<html>Rejected {split}</html>"),
                        format!("https://support.example/{split}"),
                    ] {
                        let output = sanitizer.sanitize(&value);
                        let compact: String = output
                            .chars()
                            .filter(|c| c.is_alphanumeric())
                            .flat_map(char::to_lowercase)
                            .collect();
                        assert!(
                            !compact.contains(&skeleton[..end]),
                            "separated credential fragment survived"
                        );
                    }
                }
            }
        }
        let private_value = "v9N2t6D5p8Q1secret";
        for label in [
            "Authorization",
            "Proxy-Authorization",
            "api-key",
            "x-api-key",
            "token",
            "api key",
            "refresh-token",
        ] {
            for scheme in [
                "Bearer",
                "Basic",
                "Token",
                "Digest",
                "Bearer Basic Token",
                "Token Bearer Bearer",
            ] {
                for delimiter in [": ", "=", "%3A%20", "&#58;&#32;"] {
                    for value in [
                        format!("{scheme} {private_value}\nHelp https://support.example/quota"),
                        format!(
                            "{scheme} https://private.example/{private_value}\nHelp https://support.example/quota"
                        ),
                        format!("\"{scheme} {private_value}\" Help https://support.example/quota"),
                        format!(
                            "{scheme} username=\"{private_value}\", nonce=\"{private_value}\"\nHelp https://support.example/quota"
                        ),
                    ] {
                        let output = sanitizer.sanitize(&format!("{label}{delimiter}{value}"));
                        assert!(
                            !output.contains(private_value),
                            "authentication value survived"
                        );
                        assert!(
                            output.contains("https://support.example/quota"),
                            "following help link was removed: {output}"
                        );
                    }
                }
            }
        }
        for quote in ['"', '\''] {
            for encoded in [
                format!("%{:02x}", quote as u32),
                format!("&#{};", quote as u32),
                format!("&#x{:x};", quote as u32),
                format!("\\u{:04x}", quote as u32),
                char::from_u32(quote as u32 + 0xfee0).unwrap().to_string(),
            ] {
                for depth in [0, 1, 2, 4, 8, 15, 16, 17, 24] {
                    let mut encoded = encoded.clone();
                    for _ in 0..depth {
                        encoded = encoded.replace('%', "%25");
                    }
                    for label in ["Authorization", "Proxy-Authorization", "api-key", "token"] {
                        let output = sanitizer.sanitize(&format!(
                            "{label}: {quote}Bearer prefix{encoded}{private_value}{quote}"
                        ));
                        assert!(
                            !output.contains(private_value),
                            "restored authentication framing leaked"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn generated_encodings_invisibles_case_and_truncation_never_expose_secret_fragments() {
        let secret = "opaque-fixture-credential";
        let sanitizer = FailureSanitizer::new(&[secret]);
        let invisible = [
            '\u{00ad}',
            '\u{034f}',
            '\u{061c}',
            '\u{180e}',
            '\u{200b}',
            '\u{2060}',
            '\u{206f}',
            '\u{feff}',
            '\u{fe0f}',
            '\u{e0100}',
            '\u{2028}',
            '\u{2029}',
        ];
        let forms = [
            secret.to_owned(),
            secret.to_uppercase(),
            secret
                .chars()
                .map(|c| {
                    if c.is_ascii_graphic() {
                        char::from_u32(c as u32 + 0xfee0).unwrap()
                    } else {
                        c
                    }
                })
                .collect(),
        ];
        let mut count = 0;
        for form in forms {
            let chars: Vec<_> = form.chars().collect();
            for offset in 1..chars.len() {
                for character in invisible {
                    let value = format!(
                        "{}{}{}",
                        chars[..offset].iter().collect::<String>(),
                        character,
                        chars[offset..].iter().collect::<String>()
                    );
                    for wrapped in [
                        value.clone(),
                        format!(
                            "HTTPS://user:password@support.example/{value}?{value}={value}#{value}"
                        ),
                        format!("<html>Rejected {value}</html>"),
                    ] {
                        let out = sanitizer.sanitize(&wrapped);
                        let decoded = decode_text(&out).unwrap().to_lowercase();
                        for fragment in secret.as_bytes().windows(MIN_SECRET_FRAGMENT_CHARS) {
                            assert!(
                                !decoded.contains(std::str::from_utf8(fragment).unwrap()),
                                "credential fragment survived"
                            );
                        }
                        count += 1;
                    }
                }
            }
        }
        for end in MIN_SECRET_FRAGMENT_CHARS..=secret.len() {
            let prefix = &secret[..end];
            for length in [0, 1000, 1020, 16360, 16384] {
                let out = sanitizer.sanitize(&format!("{} {prefix}", "x".repeat(length)));
                assert!(!out.to_lowercase().contains(&prefix.to_lowercase()));
                count += 1;
            }
        }
        for label in [
            "Bearer",
            "api key",
            "TOKEN",
            "api_key",
            "access-token",
            "authorization",
        ] {
            for delimiter in [" ", ": ", ":", "=", "&#32;", "%20", "\\u0020"] {
                let out = sanitizer.sanitize(&format!("{label}{delimiter}synthetic-private-value"));
                assert!(
                    !out.contains("synthetic-private-value"),
                    "encoded credential label failed: {label} {delimiter:?} -> {out}"
                );
                count += 1;
            }
        }
        assert!(count > 2500);
    }
}
