//! OSC 8 hyperlinks, after Claude Code `utils/hyperlink.ts`; which terminals show them is the
//! shared rule in `tickloop_ui::hyperlink`.
//!
//! A link is a span attribute like its style: the text stays free of escape
//! sequences, and the writer wraps the span in OSC 8 only when it reaches the
//! terminal, so width and cursor arithmetic never see the URL.

use std::fmt::Write as _;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

pub(super) use tickloop_ui::hyperlink::supported;
use tickloop_ui::hyperlink::{OSC8_END, OSC8_START};

const FILE_URL_SCHEME: &str = "file://";

/// A link target. Only `file://` URLs are built, with every byte outside the
/// unreserved set percent-encoded, so the target can never carry a control
/// character into the escape sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Hyperlink(Arc<str>);

impl Hyperlink {
    pub(super) fn web(url: &str) -> Option<Self> {
        if url.chars().any(char::is_control) {
            return None;
        }
        let parsed = reqwest::Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return None;
        }
        Some(Self(url.into()))
    }
    /// Node's `pathToFileURL` for an absolute path; a relative path has no file URL.
    pub(super) fn file(path: &Path) -> Option<Self> {
        if !path.is_absolute() {
            return None;
        }
        let mut url = String::from(FILE_URL_SCHEME);
        for byte in path.as_os_str().as_encoded_bytes() {
            if byte.is_ascii_alphanumeric() || b"/-._~".contains(byte) {
                url.push(char::from(*byte));
            } else {
                let _ = write!(url, "%{byte:02X}");
            }
        }
        Some(Self(url.into()))
    }

    pub(super) fn url(&self) -> &str {
        &self.0
    }

    pub(super) fn open(&self) -> String {
        format!("{OSC8_START}{}{OSC8_END}", self.url())
    }

    pub(super) fn close() -> String {
        format!("{OSC8_START}{OSC8_END}")
    }
}

/// Splits `text` into non-empty runs, each carrying the link of its byte range or none.
/// Ranges are sorted, disjoint and on character boundaries.
pub(super) fn runs<'a>(
    text: &'a str,
    links: &'a [(Range<usize>, Hyperlink)],
) -> Vec<(&'a str, Option<&'a Hyperlink>)> {
    let mut runs = Vec::with_capacity(links.len() * 2 + 1);
    let mut written = 0;
    for (range, link) in links {
        runs.push((&text[written..range.start], None));
        runs.push((&text[range.clone()], Some(link)));
        written = range.end;
    }
    runs.push((&text[written..], None));
    runs.retain(|(run, _)| !run.is_empty());
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_percent_encode_everything_outside_the_unreserved_set() {
        let link = Hyperlink::file(Path::new("/tmp/a b/\x1b]x\x07/图.png")).unwrap();
        assert_eq!(
            link.url(),
            "file:///tmp/a%20b/%1B%5Dx%07/%E5%9B%BE.png",
            "a control character in the path must never reach the escape sequence"
        );
        assert!(Hyperlink::file(Path::new("relative/1.png")).is_none());
    }

    #[test]
    fn linked_ranges_become_their_own_runs_and_the_rest_is_untouched() {
        let link = Hyperlink::file(Path::new("/cache/1.png")).unwrap();
        assert_eq!(
            runs("see [Image #1] now", &[(4..14, link.clone())]),
            [("see ", None), ("[Image #1]", Some(&link)), (" now", None)]
        );
        assert_eq!(
            format!("{}[Image #1]{}", link.open(), Hyperlink::close()),
            "\x1b]8;;file:///cache/1.png\x07[Image #1]\x1b]8;;\x07"
        );
    }
}
