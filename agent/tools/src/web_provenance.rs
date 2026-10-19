//! Where a URL comes from, for a host that lets `WebFetch` fetch only URLs with provenance: ones
//! the user wrote or an earlier search result of the same Chat contained, and the links and
//! redirects those pages led to without leaving their origin or site.
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
/// The most page links a Chat's fetches keep until its host takes them.
const MAX_PENDING_LINKS: usize = 4_000;

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

/// What a Chat's fetches saw since its host last took them: the links of each page on the page's
/// own origin, and each redirect to another host of the same site. A link to another origin or a
/// redirect to another site is not kept, so a page cannot offer the model a menu of hosts to
/// choose from.
#[derive(Clone, Default)]
pub(super) struct Observations(Arc<Mutex<FetchObservations>>);

/// URLs in their compared form, as [`provenance_key`] gives them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchObservations {
    pub page_links: Vec<String>,
    pub redirect_targets: Vec<String>,
}

impl Observations {
    pub(super) fn record_page(&self, page: &Url, links: Vec<String>) {
        let page_origin = page.origin();
        let mut observed = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let room = MAX_PENDING_LINKS.saturating_sub(observed.page_links.len());
        observed.page_links.extend(
            links
                .into_iter()
                .filter(|link| Url::parse(link).is_ok_and(|link| link.origin() == page_origin))
                .take(room),
        );
    }

    pub(super) fn record_redirect(&self, from: &Url, to: &Url) {
        if site(from).is_some()
            && site(from) == site(to)
            && let Some(key) = provenance_key(to.as_str())
        {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .redirect_targets
                .push(key);
        }
    }

    pub(super) fn take(&self) -> FetchObservations {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The site of a URL's host: its registrable domain, taken as the last two labels, or the last
/// three under a two-letter country code whose second level is a generic one such as `co.uk`. An
/// IP address or a single-label host is a site of its own.
pub fn site(url: &Url) -> Option<String> {
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if host.starts_with('[') || host.parse::<std::net::IpAddr>().is_ok() {
        return Some(host);
    }
    let labels = host.split('.').collect::<Vec<_>>();
    let kept = match labels.as_slice() {
        [_, .., second, top] if top.len() == 2 && GENERIC_SECOND_LEVELS.contains(second) => 3,
        _ => 2,
    };
    Some(labels[labels.len().saturating_sub(kept)..].join("."))
}

/// Second-level labels under which country codes register names, as `co.uk` or `com.au`.
const GENERIC_SECOND_LEVELS: [&str; 7] = ["co", "com", "net", "org", "ac", "gov", "edu"];

/// The origins of `urls`: scheme, host and port.
pub fn origins<'a>(urls: impl IntoIterator<Item = &'a String>) -> HashSet<String> {
    urls.into_iter()
        .filter_map(|url| Url::parse(url).ok())
        .map(|url| url.origin().ascii_serialization())
        .collect()
}

/// The origin of one URL in the form [`origins`] gives.
pub fn origin_of(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

/// What the model is told when it asks for a URL without provenance.
pub fn provenance_refusal(url: &str) -> String {
    format!(
        "{WEB_FETCH_TOOL_NAME} only fetches a URL that appears exactly in the user's messages or \
         in an earlier search result of this conversation, or that a page fetched from such a \
         site linked to on its own site; {url} does not. Ask the user for the URL, or find it \
         with a web search if one is available."
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

    #[test]
    fn pages_keep_only_their_own_origins_links_and_redirects_their_own_site() {
        let observations = Observations::default();
        let page = Url::parse("https://example.com/news").unwrap();
        observations.record_page(
            &page,
            vec![
                "https://example.com/next".into(),
                "https://other.example/a".into(),
                "http://example.com/plain".into(),
            ],
        );
        observations.record_redirect(&page, &Url::parse("https://www.example.com/x").unwrap());
        observations.record_redirect(&page, &Url::parse("https://example.org/x").unwrap());
        assert_eq!(
            observations.take(),
            FetchObservations {
                page_links: vec!["https://example.com/next".into()],
                redirect_targets: vec!["https://www.example.com/x".into()],
            }
        );
        assert_eq!(observations.take(), FetchObservations::default());
        let site_of = |url: &str| site(&Url::parse(url).unwrap()).unwrap();
        assert_eq!(site_of("https://a.b.example.co.uk/"), "example.co.uk");
        assert_eq!(site_of("http://localhost:8080/"), "localhost");
        assert_eq!(site_of("http://127.0.0.1/"), "127.0.0.1");
    }
}
