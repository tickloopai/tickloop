//! OSC 8 terminal hyperlinks: which terminals show them, after Claude Code
//! `ink/supports-hyperlinks.ts`, and how a ratatui frame carries one. In a frame, a link's text is
//! drawn as usual and each of its rows then becomes one cell holding the escape sequence around
//! the row's text, with the width the text takes, while the cells it covers are skipped when
//! ratatui writes the frame. Every row links to the whole target, so a terminal opens all of a
//! link that wraps from any of its rows.

use std::num::NonZeroU16;
use std::ops::Range;

use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Starts an OSC 8 hyperlink; the target and [`OSC8_END`] follow.
pub const OSC8_START: &str = "\x1b]8;;";
/// BEL rather than ST, as Claude Code does: more terminals accept it.
pub const OSC8_END: &str = "\x07";

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

/// Herdr parses OSC 8 in its panes and opens a link's whole target on a modified click, so the
/// rows of a wrapped link need it there even when the outer terminal is not recognised.
const HERDR_ENVIRONMENT: (&str, &str) = ("HERDR_ENV", "1");

const WEB_URL_SCHEMES: [&str; 2] = ["https://", "http://"];
/// Punctuation that ends a sentence rather than a URL when it is a URL's last character.
const URL_TRAILING_PUNCTUATION: &str = ".,;:!?'\"`*";
const URL_BRACKETS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// Byte ranges of the http(s) URLs written as plain text in `text`, after Herdr's pane URL
/// detection. A URL runs to whitespace, a quote or angle bracket, or a wide character such as
/// CJK text or full-width punctuation, then loses trailing sentence punctuation and any closing
/// bracket it did not open.
pub fn web_url_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(start) = next_web_url_start(text, from) {
        let scheme_end = start
            + WEB_URL_SCHEMES
                .iter()
                .find(|scheme| text[start..].starts_with(*scheme))
                .map_or(0, |scheme| scheme.len());
        let run_end = text[scheme_end..]
            .char_indices()
            .find(|(_, character)| ends_web_url(*character))
            .map_or(text.len(), |(offset, _)| scheme_end + offset);
        let end = start + trimmed_url_len(&text[start..run_end]);
        if end > scheme_end {
            ranges.push(start..end);
        }
        from = end.max(scheme_end);
    }
    ranges
}

fn next_web_url_start(text: &str, from: usize) -> Option<usize> {
    let mut search = from;
    while let Some(offset) = text.get(search..)?.find("http") {
        let start = search + offset;
        if WEB_URL_SCHEMES
            .iter()
            .any(|scheme| text[start..].starts_with(scheme))
        {
            return Some(start);
        }
        search = start + "http".len();
    }
    None
}

fn ends_web_url(character: char) -> bool {
    character.is_whitespace()
        || character.is_control()
        || matches!(character, '"' | '<' | '>' | '`')
        || character.width().unwrap_or(0) > 1
}

fn trimmed_url_len(url: &str) -> usize {
    let mut end = url.len();
    while let Some(last) = url[..end].chars().next_back() {
        let unopened_closer = URL_BRACKETS.iter().any(|(open, close)| {
            last == *close && {
                let body = &url[..end - last.len_utf8()];
                body.matches(*open).count() <= body.matches(*close).count()
            }
        });
        if !URL_TRAILING_PUNCTUATION.contains(last) && !unopened_closer {
            break;
        }
        end -= last.len_utf8();
    }
    end
}

/// `text` as an OSC 8 hyperlink to `target`.
pub fn linked(target: &str, text: &str) -> String {
    format!("{OSC8_START}{target}{OSC8_END}{text}{OSC8_START}{OSC8_END}")
}

/// `text` split into rows of at most `columns` columns each, as a link is drawn across rows.
pub fn split_rows(text: &str, columns: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    let mut used = 0;
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        if used + width > columns.max(1) && used > 0 {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut()
            .expect("there is always a row")
            .push(character);
        used += width;
    }
    rows
}

/// Links `rows`, a link's text as [`split_rows`] split it and as drawn on consecutive rows of
/// `area` from its left edge, to `target`. Returns whether it found them there; when it does not,
/// or `target` or the text holds a control character, the buffer stays as it was.
pub fn link_rows(buffer: &mut Buffer, area: Rect, rows: &[String], target: &str) -> bool {
    let has_control = |text: &str| text.chars().any(char::is_control);
    if rows.is_empty() || has_control(target) || rows.iter().any(|row| has_control(row)) {
        return false;
    }
    let row_text = |buffer: &Buffer, y: u16| {
        (area.left()..area.right())
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    };
    let shows_rows_from = |y: u16| {
        rows.iter()
            .zip(y..area.bottom())
            .filter(|(row, y)| row_text(buffer, *y).starts_with(row.as_str()))
            .count()
            == rows.len()
    };
    let Some(top) = (area.top()..area.bottom()).find(|y| shows_rows_from(*y)) else {
        return false;
    };
    for (row, y) in rows.iter().zip(top..) {
        let Some(columns) = u16::try_from(row.width()).ok().and_then(NonZeroU16::new) else {
            continue;
        };
        let cell = &mut buffer[(area.x, y)];
        cell.set_symbol(&linked(target, row));
        cell.diff_option = CellDiffOption::ForcedWidth(columns);
        for x in area.x + 1..area.x + columns.get() {
            buffer[(x, y)].diff_option = CellDiffOption::Skip;
        }
    }
    true
}

/// Whether the terminal described by `variable` shows OSC 8 links: Claude Code
/// `supportsHyperlinks`, the `supports-hyperlinks` rules for an interactive stdout
/// followed by the terminals that library misses.
pub fn supported(variable: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| variable(name).filter(|value| !value.is_empty());
    library_supports_links(&set)
        || ["TERM_PROGRAM", "LC_TERMINAL"].into_iter().any(|name| {
            set(name)
                .is_some_and(|terminal| ADDITIONAL_HYPERLINK_TERMINALS.contains(&terminal.as_str()))
        })
        || set("TERM").is_some_and(|term| term.contains("kitty"))
        || set(HERDR_ENVIRONMENT.0).as_deref() == Some(HERDR_ENVIRONMENT.1)
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
