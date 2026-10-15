//! A list row in the Subagents panel's look: an indent the selection fill covers, a glyph whose
//! colour is the row's state, the name, and right-aligned muted details and mark. A row whose
//! whole name does not fit beside its details continues the name on a second line, under its
//! first, and keeps the details at the right edge there, so details line up whatever a row's
//! height. A Monitor row's update shows as a square in the indent, and while a periodic
//! subscription holds it, as a bar that runs from the square across the row.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    conversation_picker::ellipsize,
    display::{DisplayProfile, Glyph},
    style::{Role, RoleLine},
};
use crate::monitor_rows::{StatusTone, SubscriptionMark};
use tickloop_agent_protocol::TickSubscriptionFrequency;

/// The columns before a list row's glyph. The selection fill alone marks the selected row.
pub(super) const ROW_INDENT: &str = "  ";
/// The indent column a Monitor row's update square takes, next to the glyph. The column before it
/// stays the row's own, so the selection fill always starts a selected row.
const SQUARE_COLUMN: usize = 1;
const STATUS_GAP_COLUMNS: usize = 2;
const SEGMENT_SEPARATOR: &str = " · ";
/// The blank column after a row's details. A mark some terminals draw wider than its one column,
/// such as `⟳` from a fallback font, still shows whole instead of meeting the column's edge.
const RIGHT_MARGIN: &str = " ";
const MILLISECONDS_PER_SECOND: u64 = 1000;

pub(super) struct ListRow<'a> {
    pub(super) selected: bool,
    pub(super) glyph: &'a str,
    /// The glyph's colour, which is the row's state; see [`state_mark`].
    pub(super) glyph_role: Role,
    pub(super) name: &'a str,
    pub(super) status: RowStatus,
    /// The row's flash fill while it flashes; see [`flash_fill`]. It covers the update bar, and
    /// the selection fill wins over it.
    pub(super) fill: Option<Role>,
    pub(super) update: Option<UpdateIndicator>,
}

/// How a Monitor row shows an update it has not handed over yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UpdateIndicator {
    /// The square's role; see [`update_square`].
    pub(super) square: Role,
    /// While a periodic subscription holds the update, how many columns from the row's left the
    /// bar reaches: the part of the period elapsed before the check that hands it to the Agent.
    pub(super) bar_columns: Option<usize>,
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
        let (before_square, square) = ROW_INDENT.split_at(SQUARE_COLUMN);
        let mut line = RoleLine::role(before_square, role);
        line.push(
            square,
            Some(self.update.map_or(role, |update| update.square)),
        );
        line.push(self.glyph, Some(self.glyph_role));
        line.push(" ", Some(role));
        line
    }

    /// Columns a row's details take at its right, with the gap before them.
    fn status_reserve(&self) -> usize {
        match self.status.columns() {
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
        self.push_status(&mut line, width);
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
        self.push_status(&mut second, width);
        [self.finish(first, width), self.finish(second, width)]
    }

    /// Right-aligns the details in the columns the line leaves, shortened there when they do not
    /// fit.
    fn push_status(&self, line: &mut RoleLine, width: usize) {
        let spans = self.status.spans(width.saturating_sub(line.text().width()));
        line.push(
            " ".repeat(width.saturating_sub(line.text().width() + spans_columns(&spans))),
            None,
        );
        for (text, role) in spans {
            line.push(text, Some(role));
        }
    }

    /// Fills the line: the selection with the update bar over it, else a flash over everything,
    /// else the update bar alone. The bar starts at the square, which it never leaves behind.
    fn finish(&self, mut line: RoleLine, width: usize) -> RoleLine {
        line.truncate(width);
        let bar = self
            .update
            .and_then(|update| update.bar_columns)
            .map(|columns| SQUARE_COLUMN..columns.max(SQUARE_COLUMN + 1));
        if self.selected {
            line = line.filled(Role::Selection);
        } else if let Some(fill) = self.fill {
            return line.filled(fill);
        }
        match bar {
            Some(columns) => line.banded(columns, Role::UpdateProgress),
            None => line,
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

fn spans_columns(spans: &[(String, Role)]) -> usize {
    spans.iter().map(|(text, _)| text.width()).sum()
}

/// What a row shows at its right: muted details, then its mark in its own colour.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RowStatus {
    /// A running row's elapsed time and the row's notes.
    pub(super) details: String,
    pub(super) mark: Option<(String, Role)>,
}

impl RowStatus {
    fn is_empty(&self) -> bool {
        self.details.is_empty() && self.mark.is_none()
    }

    /// Columns the whole status takes, its right margin included.
    fn columns(&self) -> usize {
        spans_columns(&self.spans(usize::MAX))
    }

    /// The status within `columns`, its right margin included: the mark always whole, the
    /// details shortened before it when the row is too narrow for both.
    fn spans(&self, columns: usize) -> Vec<(String, Role)> {
        if self.is_empty() {
            return Vec::new();
        }
        let mark_columns = self
            .mark
            .as_ref()
            .map_or(0, |(mark, _)| mark.width() + SEGMENT_SEPARATOR.width());
        let details = ellipsize(
            &self.details,
            columns.saturating_sub(mark_columns + RIGHT_MARGIN.width()),
        );
        let mut spans = Vec::new();
        if !details.is_empty() {
            spans.push((details, Role::MutedText));
        }
        if let Some(mark) = &self.mark {
            if !spans.is_empty() {
                spans.push((SEGMENT_SEPARATOR.to_owned(), Role::MutedText));
            }
            spans.push(mark.clone());
        }
        spans.push((RIGHT_MARGIN.to_owned(), Role::MutedText));
        spans
    }
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

/// A row's update square. A new update flashes it once since `flash_since`, full, dim, full,
/// then it stays full; under Reduce motion it never flashes.
pub(super) fn update_square(
    flash_since: Option<u64>,
    display: DisplayProfile,
    now_ms: u64,
) -> Role {
    match flash_since.and_then(|since| display.flash_strong(since, now_ms)) {
        Some(false) => Role::UpdateProgress,
        Some(true) | None => Role::UpdateSquare,
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
/// or result glyph repeats it: a running row's elapsed time and the row's notes, muted, then the
/// `⟳` of its periodic subscriptions in their frequency's colour, which keeps its colour on a
/// selected row. Nothing on the row keeps changing with each update, neither a time nor a count of
/// waiting updates; its detail and the subscription detail tell them.
pub(super) fn row_status(
    tone: StatusTone,
    notes: &[String],
    subscription: Option<SubscriptionMark>,
    display: DisplayProfile,
    now_ms: u64,
) -> RowStatus {
    let elapsed = match tone {
        StatusTone::Running {
            since_ms: Some(since),
        } => Some(format_elapsed(now_ms.saturating_sub(since))),
        _ => None,
    };
    RowStatus {
        details: elapsed
            .into_iter()
            .chain(notes.iter().cloned())
            .collect::<Vec<_>>()
            .join(SEGMENT_SEPARATOR),
        mark: subscription.map(|mark| {
            (
                display.glyph(Glyph::Subscription).to_owned(),
                subscription_role(mark.frequency),
            )
        }),
    }
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
    use tickloop_agent_config::DisplayPreferences;

    fn text(status: &RowStatus) -> String {
        status
            .spans(usize::MAX)
            .iter()
            .map(|(text, _)| text.as_str())
            .collect()
    }

    fn row<'a>(name: &'a str, details: &str) -> ListRow<'a> {
        ListRow {
            selected: false,
            glyph: "$",
            glyph_role: Role::RunningMark,
            name,
            status: RowStatus {
                details: details.into(),
                mark: None,
            },
            fill: None,
            update: None,
        }
    }

    fn marked(details: &str, mark: Role) -> RowStatus {
        RowStatus {
            details: details.into(),
            mark: Some(("⟳".into(), mark)),
        }
    }

    fn periodic(frequency: TickSubscriptionFrequency) -> Option<SubscriptionMark> {
        Some(SubscriptionMark {
            frequency,
            is_waiting: false,
            next_check: None,
        })
    }

    #[test]
    fn a_long_name_continues_on_its_second_line_with_the_details_still_at_the_right() {
        let long = row("Rerun full test suite in background to verify", "14s");
        let lines = long
            .lines(30)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                "  $ Rerun full test suite in",
                "    background to verify  14s "
            ]
        );
        let one_line = ListRow {
            name: "cargo test",
            ..long
        };
        let line = one_line.lines(30)[0].text();
        assert_eq!(line, "  $ cargo test            14s ");
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
            "2m 3s",
        )
        .lines(20);
        assert_eq!(lines[0].text(), "  $ a b c d e f g h");
        let second = lines[1].text();
        assert!(
            second.contains('…') && second.ends_with("  2m 3s "),
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
        });
        assert_eq!(state_mark(running, still, 1_500), Role::RunningMark);
    }

    #[test]
    fn running_rows_show_only_their_elapsed_time() {
        let display = DisplayProfile::default();
        let status = row_status(
            StatusTone::Running {
                since_ms: Some(1_000),
            },
            &["SIGTERM".into()],
            None,
            display,
            120_000,
        );
        assert_eq!(text(&status), "1m 59s · SIGTERM ");
        assert!(
            row_status(
                StatusTone::Running { since_ms: None },
                &[],
                None,
                display,
                0
            )
            .is_empty(),
            "an unknown start shows no word in its place"
        );
    }

    #[test]
    fn a_subscription_shows_one_mark_coloured_by_frequency_and_never_a_word_or_count() {
        for (frequency, role) in [
            (TickSubscriptionFrequency::High, Role::SubscriptionHigh),
            (TickSubscriptionFrequency::Medium, Role::SubscriptionMedium),
            (TickSubscriptionFrequency::Low, Role::SubscriptionLow),
        ] {
            assert_eq!(
                row_status(
                    StatusTone::Live,
                    &[],
                    periodic(frequency),
                    DisplayProfile::default(),
                    0
                ),
                marked("", role),
                "{frequency:?}"
            );
        }
    }

    #[test]
    fn the_mark_stays_whole_with_a_free_column_after_it_however_narrow_the_row() {
        for (width, details) in [(12, "ex…"), (20, "exit 101 ·…"), (60, "exit 101 · SIGTERM")]
        {
            let lines = ListRow {
                status: marked("exit 101 · SIGTERM", Role::SubscriptionLow),
                ..row("python3 -q", "")
            }
            .lines(width);
            let last = lines.last().unwrap().text();
            assert_eq!(last.width(), width, "{last:?}");
            assert!(
                last.ends_with(&format!("{details} · ⟳ ")),
                "{width}: the mark keeps its column and a blank one after it: {last:?}"
            );
        }
    }

    #[test]
    fn a_square_takes_the_indent_column_next_to_the_glyph_and_never_moves_the_name() {
        let plain = row("Quotes", "").lines(20)[0].clone();
        let updated = ListRow {
            update: Some(UpdateIndicator {
                square: Role::UpdateSquare,
                bar_columns: None,
            }),
            ..row("Quotes", "")
        }
        .lines(20)[0]
            .clone();
        assert_eq!(updated.text(), plain.text(), "the name stays where it was");
        assert_eq!(updated.spans[0].text, " ");
        assert_eq!(
            (updated.spans[1].text.as_str(), updated.spans[1].role),
            (" ", Some(Role::UpdateSquare))
        );
        assert_eq!(
            updated.band, None,
            "no bar without a subscription holding it"
        );
    }

    #[test]
    fn one_line_and_two_line_rows_both_right_align_the_status_and_mark_the_selection_by_fill() {
        let selected = ListRow {
            selected: true,
            glyph_role: Role::SuccessMark,
            ..row("Run the console tests in the worktree", "12s")
        };
        let line = selected.one_line(40);
        assert_eq!(line.text().width(), 40);
        assert!(line.text().starts_with("  $ Run the console"), "{line:?}");
        assert!(line.text().ends_with("the…   12s "), "{line:?}");
        assert_eq!(line.spans[1].role, Some(Role::SuccessMark));
        assert_eq!(line.fill, Some(Role::Selection));
        let [name, status] = selected.two_lines(30);
        assert_eq!(name.text(), "  $ Run the console tests in");
        assert_eq!(status.text(), "    the worktree          12s ");
        assert_eq!(name.fill, Some(Role::Selection));
        assert_eq!(status.fill, Some(Role::Selection));
        assert!(!line.text().contains('›'));
    }

    #[test]
    fn a_bar_runs_from_the_square_on_every_line_and_a_flash_covers_it() {
        let check = NextCheck {
            at_ms: 160_000,
            period_ms: 60_000,
        };
        let waiting = ListRow {
            update: Some(UpdateIndicator {
                square: Role::UpdateSquare,
                bar_columns: Some(check.elapsed_columns(30, 130_000)),
            }),
            status: marked("", Role::SubscriptionHigh),
            ..row("Quote board for the whole watchlist", "")
        };
        let lines = waiting.lines(30);
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert_eq!(line.fill, None);
            assert_eq!(
                line.band.clone().map(|band| (band.columns, band.role)),
                Some((1..15, Role::UpdateProgress)),
                "half the period fills half the row, from the square on: {line:?}"
            );
        }
        let just_started = ListRow {
            update: Some(UpdateIndicator {
                square: Role::UpdateSquare,
                bar_columns: Some(0),
            }),
            ..row("Quotes", "")
        };
        assert_eq!(
            just_started.lines(30)[0]
                .band
                .clone()
                .map(|band| band.columns),
            Some(1..2),
            "the bar never leaves its square behind"
        );
        let flashing = ListRow {
            fill: Some(Role::FlashStrong),
            ..waiting
        };
        for line in flashing.lines(30) {
            assert_eq!(line.fill, Some(Role::FlashStrong));
            assert_eq!(line.band, None, "the flash covers the bar");
        }
        let selected = ListRow {
            selected: true,
            ..flashing
        };
        for line in selected.lines(30) {
            assert_eq!(line.fill, Some(Role::Selection), "the selection wins");
            assert_eq!(
                line.band.map(|band| (band.columns, band.role)),
                Some((1..15, Role::UpdateProgress)),
                "a selected row's bar keeps its colour over the selection"
            );
        }
    }

    #[test]
    fn a_new_update_flashes_the_square_full_dim_full_and_it_then_stays_full() {
        let animated = DisplayProfile::default();
        assert_eq!(
            [5_000, 5_400, 5_800, 6_200].map(|now| update_square(Some(5_000), animated, now)),
            [
                Role::UpdateSquare,
                Role::UpdateProgress,
                Role::UpdateSquare,
                Role::UpdateSquare
            ]
        );
        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
        });
        assert_eq!(
            update_square(Some(5_000), still, 5_400),
            Role::UpdateSquare,
            "under Reduce motion the square does not flash"
        );
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
