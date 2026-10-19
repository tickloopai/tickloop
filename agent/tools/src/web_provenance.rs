//! Where a URL comes from, for a host that lets `WebFetch` fetch only URLs with provenance: ones
//! the user wrote, or ones an earlier search result or fetched page of the same Chat contained.
//! A URL the model composed itself, such as one carrying data in its query, has none, so injected
//! instructions cannot use `WebFetch` to send data out.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use reqwest::Url;

use super::WEB_FETCH_TOOL_NAME;

const URL_SCHEMES: [&str; 2] = ["https://", "http://"];
/// Characters that end a URL written in running text or Markdown.
const URL_TERMINATORS: &[char] = &['<', '>', '"', '\'', '`', '(', ')', '[', ']', '{', '}', '|'];
/// Punctuation that ends a sentence rather than the URL before it.
const TRAILING_PUNCTUATION: &[char] = &['.', ',', ';', ':', '!', '?', '*', '_', '~'];
/// The most links one fetched page contributes.
const MAX_LINKS_PER_PAGE: usize = 2_000;
/// The most URLs a Chat's fetched pages and redirects contribute.
const MAX_OBSERVED_URLS: usize = 20_000;

/// The form two URLs are compared in: an HTTP(S) URL without its fragment.
pub fn provenance_key(url: &str) -> Option<String> {
    let mut url = Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    Some(url.into())
}

/// The HTTP(S) URLs written in `text`, in their compared form.
pub fn urls_in(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(start) = URL_SCHEMES
        .iter()
        .filter_map(|scheme| rest.find(scheme))
        .min()
    {
        let candidate = &rest[start..];
        let end = candidate
            .find(|character: char| {
                character.is_whitespace() || URL_TERMINATORS.contains(&character)
            })
            .unwrap_or(candidate.len());
        let written = candidate[..end].trim_end_matches(TRAILING_PUNCTUATION);
        urls.extend(provenance_key(written));
        rest = &candidate[end.max(1)..];
    }
    urls
}

/// The links of a page fetched from `base`: its absolute URLs and its Markdown link targets,
/// relative ones resolved against the page.
pub(super) fn page_links(markdown: &str, base: &Url) -> Vec<String> {
    let mut links = urls_in(markdown);
    let mut rest = markdown;
    while let Some(start) = rest.find("](") {
        let target = &rest[start + 2..];
        let end = target
            .find(|character: char| character == ')' || character.is_whitespace())
            .unwrap_or(target.len());
        if let Ok(resolved) = base.join(&target[..end]) {
            links.extend(provenance_key(resolved.as_str()));
        }
        rest = &target[end..];
    }
    links.truncate(MAX_LINKS_PER_PAGE);
    links
}

/// The URLs a Chat's fetched pages linked to and its redirects led to.
#[derive(Clone, Default)]
pub(super) struct ObservedUrls(Arc<Mutex<HashSet<String>>>);

impl ObservedUrls {
    pub(super) fn record(&self, urls: impl IntoIterator<Item = String>) {
        let mut observed = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for url in urls {
            if observed.len() >= MAX_OBSERVED_URLS {
                break;
            }
            observed.insert(url);
        }
    }

    pub(super) fn contains(&self, key: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(key)
    }
}

/// What the model is told when it asks for a URL without provenance.
pub fn provenance_refusal(url: &str) -> String {
    format!(
        "{WEB_FETCH_TOOL_NAME} only fetches a URL that appears exactly in the user's messages or \
         in an earlier search result or fetched page of this conversation; {url} does not. Ask \
         the user for the URL, or find it with a web search if one is available."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_read_from_running_text_and_markdown_without_their_punctuation() {
        assert_eq!(
            urls_in(
                "See https://example.com/a?b=1#top, then (https://example.org/x). \
                 Also <http://example.net/y> and [doc](https://example.com/doc)."
            ),
            [
                "https://example.com/a?b=1",
                "https://example.org/x",
                "http://example.net/y",
                "https://example.com/doc",
            ]
        );
        assert!(urls_in("ftp://example.com and example.com").is_empty());
        assert_eq!(
            provenance_key("https://Example.com"),
            Some("https://example.com/".into())
        );
    }

    #[test]
    fn a_pages_relative_links_resolve_against_the_page() {
        let base = Url::parse("https://example.com/news/today").unwrap();
        assert_eq!(
            page_links("[next](/news/tomorrow) and [up](../)", &base),
            ["https://example.com/news/tomorrow", "https://example.com/"]
        );
    }
}
