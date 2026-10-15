//! A list row in the Subagents panel's look: an indent the selection fill covers, a glyph whose
//! colour is the row's state, the name, and right-aligned muted details. A row whose whole name
//! does not fit beside its details continues the name on a second line, under its first, and keeps
//! the details at the right edge there, so details line up whatever a row's height.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    conversation_picker::ellipsize,
    display::{DisplayProfile, Glyph},
    style::{Role, RoleLine},
};
use crate::monitor_rows::{StatusTone, SubscriptionMark, Unread};
use tickloop_agent_protocol::TickSubscriptionFrequency;

/// The columns before a list row's glyph. The selection fill alone marks the selected row.
pub(super) const ROW_INDENT: &str = "  ";
const STATUS_GAP_COLUMNS: usize = 2;
const SEGMENT_SEPARATOR: &str = " · ";
const MARK_SEPARATOR: &str = " ";
const MILLISECONDS_PER_SECOND: u64 = 1000;

pub(super) struct ListRow<'a> {
    pub(super) selected: bool,
    pub(super) glyph: &'a str,
    /// The glyph's colour, which is the row's state; see [`state_mark`].
    pub(super) glyph_role: Role,
    pub(super) name: &'a str,
    pub(super) status: Vec<(String, Role)>,
    /// The row's flash fill while it flashes; see [`flash_fill`]. It covers the countdown, and
    /// the selection fill wins over it.
    pub(super) fill: Option<Role>,
    /// How many columns from the row's left a countdown's elapsed part fills.
    pub(super) countdown_columns: Option<usize>,
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
        let mut line = RoleLine::role(ROW_INDENT, role);
        line.push(self.glyph, Some(self.glyph_role));
        line.push(" ", Some(role));
        line
    }

    /// Columns a row's details take at its right, with the gap before them.
    fn status_reserve(&self) -> usize {
        match status_columns(&self.status) {
            0 => 0,
            columns => STATUS_GAP_COLUMNS + columns,
        }
    }

    /// Columns a one-line row needs beside its name: the indent, glyph and details.
    fn reserved_columns(&self) -> usize {
        self.prefix().text().width() + self.status_reserve()
    }

    /// The row's lines: one when the whole name fits beside the details, else two, with the name
    /// running on into the second, so a name is never cut for its details while two lines hold it.
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
        let mut first = self.prefix();
        let name_column = first.text().width();
        let (head, tail) = split_at_columns(self.name, width.saturating_sub(name_column));
        first.push(head, Some(self.role()));
        let mut second = RoleLine::role(" ".repeat(name_column), self.role());
        let tail_columns = width.saturating_sub(name_column + self.status_reserve());
        second.push(ellipsize(tail, tail_columns), Some(self.role()));
        second.push(
            " ".repeat(width.saturating_sub(second.text().width() + status_columns(&self.status))),
            None,
        );
        for (text, role) in &self.status {
            second.push(text.as_str(), Some(*role));
        }
        [self.finish(first, width), self.finish(second, width)]
    }

    /// Fills the line: the selection with the countdown in its deeper shade, else a flash over
    /// everything, else the countdown alone.
    fn finish(&self, mut line: RoleLine, width: usize) -> RoleLine {
        line.truncate(width);
        if self.selected {
            line = line.filled(Role::Selection);
            if let Some(columns) = self.countdown_columns {
                line = line.lead_filled(columns, Role::SelectionProgress);
            }
            return line;
        }
        match (self.fill, self.countdown_columns) {
            (Some(fill), _) => line.filled(fill),
            (None, Some(columns)) => line.lead_filled(columns, Role::CheckProgress),
            (None, None) => line,
        }
    }
}

/// Splits `text` where its first `columns` end, at the last space within them when there is one,
/// so a name continues on the next line at a word.
fn split_at_columns(text: &str, columns: usize) -> (&str, &str) {
    let mut used = 0;
    let mut last_space = None;
    for (index, character) in text.char_indices() {
        let character_columns = character.width().unwrap_or(0);
        if used + character_columns > columns {
            let cut = last_space.filter(|space| *space > 0).unwrap_or(index);
            return (text[..cut].trim_end(), text[cut..].trim_start());
        }
        if character == ' ' {
            last_space = Some(index);
        }
        used += character_columns;
    }
    (text, "")
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

/// A row's glyph colour. After a new update it flashes once since `update_flash_since` in the
/// running glyph's bright and dim phases, bright, dim, bright, then shows the state again; under
/// Reduce motion it keeps the state colour.
pub(super) fn glyph_mark(
    tone: StatusTone,
    update_flash_since: Option<u64>,
    display: DisplayProfile,
    now_ms: u64,
) -> Role {
    match update_flash_since.and_then(|since| display.flash_strong(since, now_ms)) {
        Some(true) => Role::RunningMark,
        Some(false) => Role::RunningMarkDim,
        None => state_mark(tone, display, now_ms),
    }
}

/// The fill of a row flashing since `since`: strong, soft, strong, then none. Under Reduce motion
/// it never flashes.
pub(super) fn flash_fill(since: Option<u64>, display: DisplayProfile, now_ms: u64) -> Option<Role> {
    let strong = display.flash_strong(since?, now_ms)?;
    Some(if strong {
        Role::FlashStrong
    } else {
        Role::FlashSoft
    })
}

/// Marks an update the Chat has not handled yet.
const UNREAD_MARKER: &str = "●";

/// The marks at a row's right, after its details.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RowMarks {
    pub(super) subscription: Option<SubscriptionMark>,
    /// The latest update while the Chat has not handled it.
    pub(super) unread: Option<Unread>,
}

/// The colour of a subscription mark: one hue, brightest for the most frequent subscription.
fn subscription_role(frequency: TickSubscriptionFrequency) -> Role {
    match frequency {
        TickSubscriptionFrequency::Medium => Role::SubscriptionMedium,
        TickSubscriptionFrequency::Low => Role::SubscriptionLow,
        // Only periodic subscriptions are marked, and high is the most frequent of them.
        TickSubscriptionFrequency::High | TickSubscriptionFrequency::Immediate => {
            Role::SubscriptionHigh
        }
    }
}

/// The details at a row's right. The glyph's colour already tells the state, so no status word
/// or result glyph repeats it: a running row's elapsed time and the row's notes, muted, then its
/// marks: `⟳` in its frequency's colour and, while an update is unread, `●` with the waiting count
/// in the accent colour. No time tells when the row last updated; its detail does. The marks keep
/// their colours on a selected row.
pub(super) fn status_spans(
    tone: StatusTone,
    notes: &[String],
    marks: RowMarks,
    display: DisplayProfile,
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
        .map(|text| vec![(text, Role::MutedText)])
        .collect::<Vec<_>>();
    let mut icons = Vec::new();
    if let Some(mark) = marks.subscription {
        icons.push((
            display.glyph(Glyph::Subscription).to_owned(),
            subscription_role(mark.frequency),
        ));
    }
    if let Some(unread) = marks.unread {
        if !icons.is_empty() {
            icons.push((MARK_SEPARATOR.to_owned(), Role::MutedText));
        }
        icons.push((UNREAD_MARKER.to_owned(), Role::UnreadMark));
        if let Some(count) = unread.count {
            icons.push((format!(" {count}"), Role::Accent));
        }
    }
    if !icons.is_empty() {
        segments.push(icons);
    }
    let mut spans = Vec::new();
    for segment in segments {
        if !spans.is_empty() {
            spans.push((SEGMENT_SEPARATOR.to_owned(), Role::MutedText));
        }
        spans.extend(segment);
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
    use crate::monitor_rows::NextCheck;
    use tickloop_agent_config::{DisplayPreferences, GlyphSet};

    fn text(spans: &[(String, Role)]) -> String {
        spans.iter().map(|(text, _)| text.as_str()).collect()
    }

    fn row<'a>(name: &'a str, status: Vec<(String, Role)>) -> ListRow<'a> {
        ListRow {
            selected: false,
            glyph: "$",
            glyph_role: Role::RunningMark,
            name,
            status,
            fill: None,
            countdown_columns: None,
        }
    }

    fn periodic(frequency: TickSubscriptionFrequency) -> RowMarks {
        RowMarks {
            subscription: Some(SubscriptionMark {
                frequency,
                next_check: None,
            }),
            ..RowMarks::default()
        }
    }

    #[test]
    fn a_long_name_continues_on_its_second_line_with_the_details_still_at_the_right() {
        let long = row(
            "Rerun full test suite in background to verify",
            vec![("14s".into(), Role::MutedText)],
        );
        let lines = long
            .lines(30)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                "  $ Rerun full test suite in",
                "    background to verify   14s"
            ]
        );
        let one_line = ListRow {
            name: "cargo test",
            ..long
        };
        let line = one_line.lines(30)[0].text();
        assert_eq!(line, "  $ cargo test             14s");
        assert_eq!(
            line.width(),
            lines[1].width(),
            "details share one right edge"
        );
    }

    #[test]
    fn a_name_too_long_for_two_lines_is_cut_before_the_details() {
        let lines = row(
            "a b c d e f g h i j k l m n o p q r s t u v w x y z",
            vec![("2m 3s".into(), Role::MutedText)],
        )
        .lines(20);
        assert_eq!(lines[0].text(), "  $ a b c d e f g h");
        let second = lines[1].text();
        assert!(
            second.contains('…') && second.ends_with("  2m 3s"),
            "{second}"
        );
        assert_eq!(lines[1].text().width(), 20);
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
        let display = DisplayProfile::default();
        let spans = status_spans(
            StatusTone::Running {
                since_ms: Some(1_000),
            },
            &[],
            RowMarks::default(),
            display,
            124_000,
        );
        assert_eq!(spans, [("2m 3s".into(), Role::MutedText)]);
        assert!(
            status_spans(
                StatusTone::Running { since_ms: None },
                &[],
                RowMarks::default(),
                display,
                0
            )
            .is_empty(),
            "an unknown start shows no word in its place"
        );
    }

    #[test]
    fn a_subscription_shows_one_mark_coloured_by_frequency_and_never_a_word() {
        for glyph_set in [GlyphSet::Unicode, GlyphSet::NerdFont] {
            let display = DisplayProfile::new(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            for (frequency, role) in [
                (TickSubscriptionFrequency::High, Role::SubscriptionHigh),
                (TickSubscriptionFrequency::Medium, Role::SubscriptionMedium),
                (TickSubscriptionFrequency::Low, Role::SubscriptionLow),
            ] {
                assert_eq!(
                    status_spans(StatusTone::Live, &[], periodic(frequency), display, 0),
                    [("⟳".into(), role)],
                    "{glyph_set:?} {frequency:?}"
                );
            }
        }
    }

    #[test]
    fn an_unread_update_shows_only_its_mark_and_waiting_count_without_a_time() {
        let display = DisplayProfile::default();
        let unread = |count| RowMarks {
            unread: Some(Unread { count }),
            ..periodic(TickSubscriptionFrequency::High)
        };
        let tick = status_spans(StatusTone::Live, &[], unread(Some(3)), display, 0);
        assert_eq!(text(&tick), "⟳ ● 3");
        assert_eq!(
            tick[tick.len() - 2..],
            [("●".into(), Role::UnreadMark), (" 3".into(), Role::Accent)]
        );
        let shell = status_spans(
            StatusTone::Stopped,
            &["SIGTERM".into()],
            RowMarks {
                unread: Some(Unread::default()),
                ..RowMarks::default()
            },
            display,
            0,
        );
        assert_eq!(text(&shell), "SIGTERM · ●");
        assert!(status_spans(StatusTone::Neutral, &[], RowMarks::default(), display, 0).is_empty());
    }

    #[test]
    fn a_new_update_flashes_the_glyph_bright_dim_bright_then_shows_the_state_again() {
        let animated = DisplayProfile::default();
        assert_eq!(
            [5_000, 5_400, 5_800, 6_200].map(|now| glyph_mark(
                StatusTone::Live,
                Some(5_000),
                animated,
                now
            )),
            [
                Role::RunningMark,
                Role::RunningMarkDim,
                Role::RunningMark,
                Role::SuccessMark
            ]
        );
        assert_eq!(
            glyph_mark(StatusTone::Failed, None, animated, 5_000),
            Role::ErrorMark
        );
        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
            ..Default::default()
        });
        assert_eq!(
            glyph_mark(StatusTone::Live, Some(5_000), still, 5_000),
            Role::SuccessMark,
            "under Reduce motion the glyph keeps its state"
        );
    }

    #[test]
    fn one_line_and_two_line_rows_both_right_align_the_status_and_mark_the_selection_by_fill() {
        let selected = ListRow {
            selected: true,
            glyph_role: Role::SuccessMark,
            ..row(
                "Run the console tests in the worktree",
                vec![("12s".into(), Role::MutedText)],
            )
        };
        let line = selected.one_line(40);
        assert_eq!(line.text().width(), 40);
        assert!(line.text().starts_with("  $ Run the console"), "{line:?}");
        assert!(line.text().ends_with("w…  12s"), "{line:?}");
        assert_eq!(line.spans[1].role, Some(Role::SuccessMark));
        assert_eq!(line.fill, Some(Role::Selection));
        let [name, status] = selected.two_lines(30);
        assert_eq!(name.text(), "  $ Run the console tests in");
        assert_eq!(status.text(), "    the worktree           12s");
        assert_eq!(name.fill, Some(Role::Selection));
        assert_eq!(status.fill, Some(Role::Selection));
        assert!(!line.text().contains('›'));
    }

    #[test]
    fn a_countdown_fills_the_first_columns_of_every_line_and_a_flash_covers_it() {
        let check = NextCheck {
            at_ms: 160_000,
            period_ms: 60_000,
        };
        let counting = ListRow {
            countdown_columns: Some(check.elapsed_columns(30, 130_000)),
            ..row(
                "Quote board for the whole watchlist",
                vec![("⟳".into(), Role::SubscriptionHigh)],
            )
        };
        let lines = counting.lines(30);
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert_eq!(line.fill, None);
            assert_eq!(
                line.lead_fill.map(|lead| (lead.columns, lead.role)),
                Some((15, Role::CheckProgress)),
                "half the period fills half the row: {line:?}"
            );
        }
        let flashing = ListRow {
            fill: Some(Role::FlashStrong),
            ..counting
        };
        for line in flashing.lines(30) {
            assert_eq!(line.fill, Some(Role::FlashStrong));
            assert_eq!(line.lead_fill, None, "the flash covers the countdown");
        }
        let selected = ListRow {
            selected: true,
            ..flashing
        };
        for line in selected.lines(30) {
            assert_eq!(line.fill, Some(Role::Selection), "the selection wins");
            assert_eq!(
                line.lead_fill.map(|lead| (lead.columns, lead.role)),
                Some((15, Role::SelectionProgress)),
                "the elapsed part is the selection's deeper shade"
            );
        }
    }

    #[test]
    fn a_row_flashes_strong_soft_strong_for_1200_ms_unless_motion_is_reduced() {
        let animated = DisplayProfile::default();
        assert_eq!(
            [5_000, 5_400, 5_800, 6_200].map(|now| flash_fill(Some(5_000), animated, now)),
            [
                Some(Role::FlashStrong),
                Some(Role::FlashSoft),
                Some(Role::FlashStrong),
                None
            ]
        );
        assert_eq!(flash_fill(None, animated, 5_000), None);
        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
            ..Default::default()
        });
        assert_eq!(flash_fill(Some(5_000), still, 5_000), None);
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
