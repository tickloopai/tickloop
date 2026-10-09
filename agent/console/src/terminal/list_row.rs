//! A list row in the Subagents panel's look: a `›` marker on the selected row, a glyph whose
//! colour is the row's state, the name, and right-aligned muted details. A row whose whole name
//! does not fit beside its details moves them to a second, indented line.

use unicode_width::UnicodeWidthStr;

use super::{
    conversation_picker::ellipsize,
    display::DisplayProfile,
    style::{Role, RoleLine},
};
use crate::monitor_rows::{StatusTone, Unread};

const SELECTED_MARKER: &str = "› ";
const UNSELECTED_MARKER: &str = "  ";
const STATUS_GAP_COLUMNS: usize = 2;
const SECOND_LINE_INDENT: &str = "    ";
const SEGMENT_SEPARATOR: &str = " · ";
const MILLISECONDS_PER_SECOND: u64 = 1000;

pub(super) struct ListRow<'a> {
    pub(super) selected: bool,
    pub(super) glyph: &'a str,
    /// The glyph's colour, which is the row's state; see [`state_mark`].
    pub(super) glyph_role: Role,
    pub(super) name: &'a str,
    pub(super) status: Vec<(String, Role)>,
}

impl ListRow<'_> {
    fn role(&self) -> Role {
        if self.selected {
            Role::Text
        } else {
            Role::MutedText
        }
    }

    fn prefix(&self) -> RoleLine {
        let role = self.role();
        let mut line = RoleLine::role(
            if self.selected {
                SELECTED_MARKER
            } else {
                UNSELECTED_MARKER
            },
            role,
        );
        line.push(self.glyph, Some(self.glyph_role));
        line.push(" ", Some(role));
        line
    }

    /// Columns a one-line row needs beside its name: the marker, glyph, gap and status.
    fn reserved_columns(&self) -> usize {
        self.prefix().text().width() + STATUS_GAP_COLUMNS + status_columns(&self.status)
    }

    /// The row's lines: one when the whole name fits beside the status, else the name and then the
    /// status, so a name is never cut for its status.
    pub(super) fn lines(&self, width: usize) -> Vec<RoleLine> {
        if self.reserved_columns() + self.name.width() <= width {
            vec![self.one_line(width)]
        } else {
            self.two_lines(width).into()
        }
    }

    fn one_line(&self, width: usize) -> RoleLine {
        let mut line = self.prefix();
        let name_columns = width.saturating_sub(self.reserved_columns());
        line.push(ellipsize(self.name, name_columns), Some(self.role()));
        line.push(
            " ".repeat(width.saturating_sub(line.text().width() + status_columns(&self.status))),
            None,
        );
        for (text, role) in &self.status {
            line.push(text.as_str(), Some(*role));
        }
        self.finish(line, width)
    }

    fn two_lines(&self, width: usize) -> [RoleLine; 2] {
        let mut name = self.prefix();
        let name_columns = width.saturating_sub(name.text().width());
        name.push(ellipsize(self.name, name_columns), Some(self.role()));
        let mut status = RoleLine::role(SECOND_LINE_INDENT, self.role());
        for (text, role) in &self.status {
            status.push(text.as_str(), Some(*role));
        }
        [self.finish(name, width), self.finish(status, width)]
    }

    fn finish(&self, mut line: RoleLine, width: usize) -> RoleLine {
        line.truncate(width);
        if self.selected {
            line.filled(Role::Selection)
        } else {
            line
        }
    }
}

fn status_columns(status: &[(String, Role)]) -> usize {
    status.iter().map(|(text, _)| text.width()).sum()
}

/// The colour of a row's leading glyph, which carries the row's state: accent while running,
/// success, error or warning, and muted when idle, never updated or neutral.
pub(super) fn state_mark(tone: StatusTone, display: DisplayProfile, now_ms: u64) -> Role {
    match tone {
        StatusTone::Running { .. } => running_mark(display, now_ms),
        StatusTone::Succeeded | StatusTone::Live => Role::SuccessMark,
        StatusTone::Failed | StatusTone::Gone => Role::ErrorMark,
        StatusTone::Stopped | StatusTone::Degraded => Role::WarningMark,
        StatusTone::Neutral => Role::MutedText,
    }
}

/// A running row's glyph: the accent colour, dimmed every other blink phase, as Claude Code's
/// tool dot blinks while the tool runs.
pub(super) fn running_mark(display: DisplayProfile, now_ms: u64) -> Role {
    if display.blink_dimmed(now_ms) {
        Role::RunningMarkDim
    } else {
        Role::RunningMark
    }
}

/// Marks an update the Chat has not handled yet.
const UNREAD_MARKER: &str = "●";

/// A row's latest update: how long ago it came, and whether the Chat still has to handle it.
pub(super) struct UpdateMark {
    pub(super) age: String,
    pub(super) unread: Option<Unread>,
}

/// The details at a row's right. The glyph's colour already tells the state, so no status word
/// or result glyph repeats it: a running row's elapsed time and the row's notes, muted, then its
/// latest update, its muted age once handled or `●` with the waiting count, else the age, in the
/// accent colour while unread.
pub(super) fn status_spans(
    tone: StatusTone,
    notes: &[String],
    update: Option<UpdateMark>,
    now_ms: u64,
) -> Vec<(String, Role)> {
    let elapsed = match tone {
        StatusTone::Running {
            since_ms: Some(since),
        } => Some(format_elapsed(now_ms.saturating_sub(since))),
        _ => None,
    };
    let mut segments = elapsed
        .into_iter()
        .chain(notes.iter().cloned())
        .map(|text| (text, Role::MutedText))
        .collect::<Vec<_>>();
    match update {
        Some(UpdateMark { age, unread: None }) => segments.push((age, Role::MutedText)),
        Some(UpdateMark {
            age,
            unread: Some(unread),
        }) => segments.push((
            format!(
                "{UNREAD_MARKER} {}",
                unread.count.map_or(age, |count| count.to_string())
            ),
            Role::Accent,
        )),
        None => {}
    }
    let mut spans = Vec::new();
    for (text, role) in segments {
        if !spans.is_empty() {
            spans.push((SEGMENT_SEPARATOR.to_owned(), Role::MutedText));
        }
        spans.push((text, role));
    }
    spans
}

/// Elapsed time as the Subagents panel shows it: `45s`, `2m 3s`, `1h 2m 3s`, `1d 2h 3m`.
pub(super) fn format_elapsed(milliseconds: u64) -> String {
    let seconds = if milliseconds < 60_000 {
        milliseconds / MILLISECONDS_PER_SECOND
    } else {
        milliseconds.saturating_add(500) / MILLISECONDS_PER_SECOND
    };
    let days = seconds / 86400;
    let hours = seconds % 86400 / 3600;
    let minutes = seconds % 3600 / 60;
    let remainder = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {remainder}s")
    } else if minutes > 0 {
        format!("{minutes}m {remainder}s")
    } else {
        format!("{remainder}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_config::DisplayPreferences;

    fn text(spans: &[(String, Role)]) -> String {
        spans.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[test]
    fn the_glyph_colour_carries_the_state() {
        let display = DisplayProfile::default();
        for (tone, role) in [
            (StatusTone::Succeeded, Role::SuccessMark),
            (StatusTone::Live, Role::SuccessMark),
            (StatusTone::Failed, Role::ErrorMark),
            (StatusTone::Gone, Role::ErrorMark),
            (StatusTone::Stopped, Role::WarningMark),
            (StatusTone::Degraded, Role::WarningMark),
            (StatusTone::Neutral, Role::MutedText),
        ] {
            assert_eq!(state_mark(tone, display, 1_500), role, "{tone:?}");
        }
    }

    #[test]
    fn a_running_glyph_blinks_unless_motion_is_reduced() {
        let running = StatusTone::Running { since_ms: None };
        let animated = DisplayProfile::default();
        assert_eq!(state_mark(running, animated, 1_000), Role::RunningMark);
        assert_eq!(state_mark(running, animated, 1_500), Role::RunningMarkDim);
        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
            ..Default::default()
        });
        assert_eq!(state_mark(running, still, 1_500), Role::RunningMark);
    }

    #[test]
    fn running_rows_show_only_their_elapsed_time() {
        let spans = status_spans(
            StatusTone::Running {
                since_ms: Some(1_000),
            },
            &[],
            None,
            124_000,
        );
        assert_eq!(spans, [("2m 3s".into(), Role::MutedText)]);
        assert!(
            status_spans(StatusTone::Running { since_ms: None }, &[], None, 0).is_empty(),
            "an unknown start shows no word in its place"
        );
    }

    #[test]
    fn notes_and_a_handled_update_follow_muted() {
        let spans = status_spans(
            StatusTone::Live,
            &["⟳ 60s".into()],
            Some(UpdateMark {
                age: "2m".into(),
                unread: None,
            }),
            0,
        );
        assert_eq!(text(&spans), "⟳ 60s · 2m");
        assert!(spans.iter().all(|(_, role)| *role == Role::MutedText));
        let failed = status_spans(
            StatusTone::Failed,
            &["101".into()],
            Some(UpdateMark {
                age: "5s".into(),
                unread: None,
            }),
            0,
        );
        assert_eq!(text(&failed), "101 · 5s");
    }

    #[test]
    fn an_unread_update_ends_the_details_in_the_accent_colour() {
        let unread = |count| UpdateMark {
            age: "2s".into(),
            unread: Some(Unread { count }),
        };
        let tick = status_spans(
            StatusTone::Live,
            &["⟳ 60s".into()],
            Some(unread(Some(3))),
            0,
        );
        assert_eq!(text(&tick), "⟳ 60s · ● 3");
        assert_eq!(tick.last().unwrap(), &("● 3".into(), Role::Accent));
        let shell = status_spans(StatusTone::Succeeded, &[], Some(unread(None)), 0);
        assert_eq!(shell, [("● 2s".into(), Role::Accent)]);
        let stopped = status_spans(
            StatusTone::Stopped,
            &["SIGTERM".into()],
            Some(unread(None)),
            0,
        );
        assert_eq!(text(&stopped), "SIGTERM · ● 2s");
        assert!(status_spans(StatusTone::Neutral, &[], None, 0).is_empty());
    }

    #[test]
    fn one_line_rows_right_align_the_status_and_two_line_rows_indent_it() {
        let row = ListRow {
            selected: true,
            glyph: "$",
            glyph_role: Role::SuccessMark,
            name: "Run the console tests in the worktree",
            status: vec![("12s".into(), Role::MutedText)],
        };
        let line = row.one_line(40);
        assert_eq!(line.text().width(), 40);
        assert!(line.text().starts_with("› $ Run the console"), "{line:?}");
        assert!(line.text().ends_with("w…  12s"), "{line:?}");
        assert_eq!(line.spans[1].role, Some(Role::SuccessMark));
        assert_eq!(line.fill, Some(Role::Selection));
        let [name, status] = row.two_lines(30);
        assert_eq!(name.text(), "› $ Run the console tests in…");
        assert_eq!(status.text(), "    12s");
    }

    #[test]
    fn elapsed_time_matches_the_subagents_panel() {
        for (milliseconds, expected) in [
            (45_000, "45s"),
            (123_000, "2m 3s"),
            (3_723_000, "1h 2m 3s"),
            (93_780_000, "1d 2h 3m"),
        ] {
            assert_eq!(format_elapsed(milliseconds), expected);
        }
    }
}
