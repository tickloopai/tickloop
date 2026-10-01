//! OSC 8 hyperlinks, after Claude Code `utils/hyperlink.ts` and `ink/supports-hyperlinks.ts`.
//!
//! A link is a span attribute like its style: the text stays free of escape
//! sequences, and the writer wraps the span in OSC 8 only when it reaches the
//! terminal, so width and cursor arithmetic never see the URL.

use std::fmt::Write as _;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

const OSC8_START: &str = "\x1b]8;;";
/// BEL rather than ST, as Claude Code does: more terminals accept it.
const OSC8_END: &str = "\x07";
const FILE_URL_SCHEME: &str = "file://";

/// Terminals that `supports-hyperlinks` misses; Claude Code checks them against both
/// `TERM_PROGRAM` and `LC_TERMINAL`, which survives inside tmux.
const ADDITIONAL_HYPERLINK_TERMINALS: [&str; 6] = [
    "ghostty",
    "Hyper",
    "kitty",
    "alacritty",
    "iTerm.app",
    "iTerm2",
];

/// A link target. Only `file://` URLs are built, with every byte outside the
/// unreserved set percent-encoded, so the target can never carry a control
/// character into the escape sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Hyperlink(Arc<str>);

impl Hyperlink {
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

/// Writes `text` with each linked byte range wrapped in OSC 8.
pub(super) fn apply(text: &str, links: &[(Range<usize>, Hyperlink)]) -> String {
    let mut output = String::with_capacity(text.len());
    for (run, link) in runs(text, links) {
        match link {
            Some(link) => {
                output.push_str(&link.open());
                output.push_str(run);
                output.push_str(&Hyperlink::close());
            }
            None => output.push_str(run),
        }
    }
    output
}

/// Whether the terminal described by `variable` shows OSC 8 links: Claude Code
/// `supportsHyperlinks`, the `supports-hyperlinks` rules for an interactive stdout
/// followed by the terminals that library misses.
pub(super) fn supported(variable: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| variable(name).filter(|value| !value.is_empty());
    library_supports_links(&set)
        || ["TERM_PROGRAM", "LC_TERMINAL"].into_iter().any(|name| {
            set(name)
                .is_some_and(|terminal| ADDITIONAL_HYPERLINK_TERMINALS.contains(&terminal.as_str()))
        })
        || set("TERM").is_some_and(|term| term.contains("kitty"))
}

/// `supports-hyperlinks` 3.2 for a terminal stdout on a Unix platform; `set` only
/// returns non-empty values, matching the library's truthiness checks.
fn library_supports_links(set: &impl Fn(&str) -> Option<String>) -> bool {
    if let Some(forced) = set("FORCE_HYPERLINK") {
        return leading_integer(&forced) != Some(0);
    }
    if set("NETLIFY").is_some() {
        return true;
    }
    if colour_is_disabled(set) {
        return false;
    }
    if set("WT_SESSION").is_some() {
        return true;
    }
    if set("CI").is_some() || set("TEAMCITY_VERSION").is_some() {
        return false;
    }
    if let Some(program) = set("TERM_PROGRAM") {
        let (major, minor) = parse_version(set("TERM_PROGRAM_VERSION").as_deref().unwrap_or(""));
        match program.as_str() {
            "iTerm.app" => return major > 3 || (major == 3 && minor >= 1),
            "WezTerm" => return major >= 20200620,
            "vscode" => return major > 1 || (major == 1 && minor >= 72),
            "ghostty" => return true,
            _ => {}
        }
    }
    if let Some(version) = set("VTE_VERSION") {
        // 0.50.0 was meant to support links but crashes on them.
        if version == "0.50.0" {
            return false;
        }
        let (major, minor) = parse_version(&version);
        return major > 0 || minor >= 50;
    }
    set("TERM").as_deref() == Some("alacritty")
}

/// `supports-color`'s refusals that apply to a terminal: `FORCE_COLOR=0` or `false`,
/// and `TERM=dumb` unless colour is forced on.
fn colour_is_disabled(set: &impl Fn(&str) -> Option<String>) -> bool {
    match set("FORCE_COLOR").as_deref() {
        Some("false") => true,
        Some("true") => false,
        Some(forced) => leading_integer(forced) == Some(0),
        None => set("TERM").as_deref() == Some("dumb"),
    }
}

/// JavaScript `parseInt(value, 10)` for the non-negative values these variables hold.
fn leading_integer(value: &str) -> Option<u64> {
    value
        .trim_start()
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok())
}

/// `supports-hyperlinks` `parseVersion`: `"3.1.2"` is 3.1, and VTE's dotless `"5002"` is 0.50.
fn parse_version(version: &str) -> (u64, u64) {
    if (3..=4).contains(&version.len()) && version.bytes().all(|byte| byte.is_ascii_digit()) {
        return (0, leading_integer(version).unwrap_or(0) / 100);
    }
    let mut parts = version
        .split('.')
        .map(|part| leading_integer(part).unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<Vec<_>>();
        move |name| {
            pairs
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.clone())
        }
    }

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
    fn linked_ranges_are_wrapped_in_osc_8_and_the_rest_is_untouched() {
        let link = Hyperlink::file(Path::new("/cache/1.png")).unwrap();
        assert_eq!(
            apply("see [Image #1] now", &[(4..14, link)]),
            "see \x1b]8;;file:///cache/1.png\x07[Image #1]\x1b]8;;\x07 now"
        );
    }

    #[test]
    fn terminals_that_show_links_are_detected_like_claude_code() {
        for supported_terminal in [
            &[
                ("TERM_PROGRAM", "iTerm.app"),
                ("TERM_PROGRAM_VERSION", "3.5.0"),
            ][..],
            &[
                ("TERM_PROGRAM", "WezTerm"),
                ("TERM_PROGRAM_VERSION", "20240203"),
            ],
            &[
                ("TERM_PROGRAM", "vscode"),
                ("TERM_PROGRAM_VERSION", "1.90.0"),
            ],
            &[("TERM_PROGRAM", "ghostty")],
            &[("VTE_VERSION", "7600")],
            &[("TERM", "alacritty")],
            &[("TERM", "xterm-kitty")],
            &[("TERM_PROGRAM", "tmux"), ("LC_TERMINAL", "iTerm2")],
            &[("WT_SESSION", "1")],
            &[("CI", "1"), ("TERM_PROGRAM", "kitty")],
            &[("FORCE_HYPERLINK", "1")],
            &[("FORCE_HYPERLINK", ""), ("TERM_PROGRAM", "ghostty")],
        ] {
            assert!(
                supported(environment(supported_terminal)),
                "{supported_terminal:?}"
            );
        }
        for plain_terminal in [
            &[][..],
            &[("TERM_PROGRAM", "Apple_Terminal")],
            &[
                ("TERM_PROGRAM", "vscode"),
                ("TERM_PROGRAM_VERSION", "1.71.0"),
            ],
            &[("VTE_VERSION", "0.50.0")],
            &[("VTE_VERSION", "4901")],
            &[("CI", "1"), ("TERM_PROGRAM", "WezTerm")],
            &[("FORCE_HYPERLINK", "0"), ("TERM_PROGRAM", "WezTerm")],
            &[
                ("FORCE_COLOR", "0"),
                ("TERM_PROGRAM", "vscode"),
                ("TERM_PROGRAM_VERSION", "1.90.0"),
            ],
            &[("TERM", "dumb"), ("WT_SESSION", "1")],
        ] {
            assert!(
                !supported(environment(plain_terminal)),
                "{plain_terminal:?}"
            );
        }
    }
}
