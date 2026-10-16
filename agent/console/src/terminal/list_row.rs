//! A list row in the Subagents panel's look: an indent the selection fill covers, a glyph whose
//! colour is the row's state, the name, and right-aligned muted details. A row whose whole name
//! does not fit beside its details continues the name on a second line, under its first, and
//! keeps the details at the right edge there, so details line up whatever a row's height. A
//! Monitor row leads with its periodic subscriptions' `↻` in the first column, and its glyph
//! takes the update colour over its state colour while the row holds an unhandled update; while a
//! periodic subscription holds the update, a bar runs from the first column across the row.

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
/// What follows a Monitor row's first column before its glyph: a blank column, so a `↻` some
/// fallback font draws wider than its column never meets the glyph.
const LEAD_GAP: &str = " ";
const NAME_GAP: &str = " ";

pub(super) fn name_column(glyph: &str) -> usize {
    ROW_INDENT.width() + glyph.width() + NAME_GAP.width()
}
const STATUS_GAP_COLUMNS: usize = 2;
const SEGMENT_SEPARATOR: &str = " · ";
const MILLISECONDS_PER_SECOND: u64 = 1000;

pub(super) struct ListRow<'a> {
    pub(super) selected: bool,
    pub(super) glyph: &'a str,
    /// The glyph's colour: the row's state, see [`state_mark`], or an update the Chat has not
    /// handled, see [`update_mark`].
    pub(super) glyph_role: Role,
    pub(super) name: &'a str,
    /// A running row's elapsed time and the row's notes; see [`row_details`].
    pub(super) details: String,
    /// The row's flash fill while it flashes; see [`flash_fill`]. It covers the update bar, and
    /// the selection fill wins over it.
    pub(super) fill: Option<Role>,
    /// The `↻` of the row's periodic subscriptions in its frequency's colour, on the update bar
    /// too; see [`subscription_mark`].
    pub(super) subscription: Option<(&'a str, Role)>,
    /// While a periodic subscription holds an update, how many columns from the row's left the
    /// update bar reaches: the part of the period elapsed before the check that hands it to the
    /// Agent.
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
        let mut line = match self.subscription {
            Some((mark, mark_role)) => RoleLine::role(mark, mark_role),
            None => RoleLine::role(" ", role),
        };
        line.push(LEAD_GAP, Some(role));
        line.push(self.glyph, Some(self.glyph_role));
        line.push(NAME_GAP, Some(role));
        line
    }

    /// Columns a row's details take at its right, with the gap before them.
    fn details_reserve(&self) -> usize {
        match self.details.width() {
            0 => 0,
            columns => STATUS_GAP_COLUMNS + columns,
        }
    }

    /// Columns a one-line row needs beside its name: the lead, glyph and details.
    fn reserved_columns(&self) -> usize {
        self.prefix().text().width() + self.details_reserve()
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
        self.push_details(&mut line, width);
        self.finish(line, width)
    }

    fn two_lines(&self, width: usize) -> [RoleLine; 2] {
        let mut first = self.prefix();
        let name_column = first.text().width();
        let (head, tail) = split_at_columns(self.name, width.saturating_sub(name_column));
        first.push(head, Some(self.role()));
        let mut second = RoleLine::role(" ".repeat(name_column), self.role());
        let tail_columns = width.saturating_sub(name_column + self.details_reserve());
        second.push(ellipsize(tail, tail_columns), Some(self.role()));
        self.push_details(&mut second, width);
        [self.finish(first, width), self.finish(second, width)]
    }

    /// Right-aligns the details in the columns the line leaves, shortened there when they do not
    /// fit.
    fn push_details(&self, line: &mut RoleLine, width: usize) {
        let details = ellipsize(&self.details, width.saturating_sub(line.text().width()));
        line.push(
            " ".repeat(width.saturating_sub(line.text().width() + details.width())),
            None,
        );
        line.push(details, Some(Role::MutedText));
    }

    /// Selection takes priority over a flash, with the progress bar over the selection.
    /// Progress and flashes start at the name; the subscription mark keeps its own background.
    fn finish(&self, mut line: RoleLine, width: usize) -> RoleLine {
        line.truncate(width);
        let name_start = name_column(self.glyph).min(width);
        let bar = self
            .bar_columns
            .filter(|_| name_start < width)
            .map(|columns| name_start..(name_start + columns.max(1)).min(width));
        let after_mark = usize::from(self.subscription.is_some());
        if self.selected {
            line = line.filled_from(Role::Selection, after_mark);
        } else if let Some(fill) = self.fill {
            return line.filled_from(fill, name_start);
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

/// The colour of a row's glyph while the row holds an update the Chat has not handled, over its
/// state colour. A new update flashes it once since `flash_since`, bright, dim, bright, then it
/// stays bright; under Reduce motion it never flashes.
pub(super) fn update_mark(flash_since: Option<u64>, display: DisplayProfile, now_ms: u64) -> Role {
    let is_dim = flash_since.and_then(|since| display.flash_strong(since, now_ms)) == Some(false);
    if is_dim {
        Role::UpdateMarkDim
    } else {
        Role::UpdateMark
    }
}

/// What a whole Monitor row flashes for, each in its own colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowFlash {
    /// An Agent turn took the row's update in: warm.
    TakeIn,
    /// The Agent read or operated the row's instance with a tool: cold.
    Use,
}

/// The fill of a row flashing for `flash` since the time it holds: strong, soft, strong, then
/// none. Under Reduce motion it never flashes.
pub(super) fn flash_fill(
    flash: Option<(RowFlash, u64)>,
    display: DisplayProfile,
    now_ms: u64,
) -> Option<Role> {
    let (flash, since) = flash?;
    let strong = display.flash_strong(since, now_ms)?;
    Some(match (flash, strong) {
        (RowFlash::TakeIn, true) => Role::TakeInFlashStrong,
        (RowFlash::TakeIn, false) => Role::TakeInFlashSoft,
        (RowFlash::Use, true) => Role::UseFlashStrong,
        (RowFlash::Use, false) => Role::UseFlashSoft,
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

/// The `↻` that leads a row with periodic subscriptions, in its frequency's colour.
pub(super) fn subscription_mark(
    subscription: Option<SubscriptionMark>,
    display: DisplayProfile,
) -> Option<(&'static str, Role)> {
    subscription.map(|mark| {
        (
            display.glyph(Glyph::Subscription),
            subscription_role(mark.frequency),
        )
    })
}

/// The details at a row's right. The glyph's colour already tells the state and the lead the
/// subscriptions, so no status word or result glyph repeats them: a running row's elapsed time
/// and the row's notes. Nothing on the row keeps changing with each update, neither a time nor a
/// count of waiting updates; its detail and the subscription detail tell them.
pub(super) fn row_details(tone: StatusTone, notes: &[String], now_ms: u64) -> String {
    let elapsed = match tone {
        StatusTone::Running {
            since_ms: Some(since),
        } => Some(format_elapsed(now_ms.saturating_sub(since))),
        _ => None,
    };
    elapsed
        .into_iter()
        .chain(notes.iter().cloned())
        .collect::<Vec<_>>()
        .join(SEGMENT_SEPARATOR)
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

    fn row<'a>(name: &'a str, details: &str) -> ListRow<'a> {
        ListRow {
            selected: false,
            glyph: "$",
            glyph_role: Role::RunningMark,
            name,
            details: details.into(),
            fill: None,
            subscription: None,
            bar_columns: None,
        }
    }

    fn periodic(frequency: TickSubscriptionFrequency) -> Option<SubscriptionMark> {
        Some(SubscriptionMark {
            frequency,
            is_waiting: false,
            next_check: None,
        })
    }

    /// Each column's text and role as far as `columns`.
    fn lead(line: &RoleLine, columns: usize) -> Vec<(String, Option<Role>)> {
        let mut cells = Vec::new();
        for span in &line.spans {
            for character in span.text.chars() {
                if cells.len() < columns {
                    cells.push((character.to_string(), span.role));
                }
            }
        }
        cells
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
            "2m 3s",
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
        });
        assert_eq!(state_mark(running, still, 1_500), Role::RunningMark);
    }

    #[test]
    fn running_rows_show_only_their_elapsed_time() {
        let details = row_details(
            StatusTone::Running {
                since_ms: Some(1_000),
            },
            &["SIGTERM".into()],
            120_000,
        );
        assert_eq!(details, "1m 59s · SIGTERM");
        assert!(
            row_details(StatusTone::Running { since_ms: None }, &[], 0).is_empty(),
            "an unknown start shows no word in its place"
        );
    }

    #[test]
    fn a_quiet_subscription_leads_the_row_with_one_mark_coloured_by_frequency() {
        for (frequency, role) in [
            (TickSubscriptionFrequency::High, Role::SubscriptionHigh),
            (TickSubscriptionFrequency::Medium, Role::SubscriptionMedium),
            (TickSubscriptionFrequency::Low, Role::SubscriptionLow),
        ] {
            let mark = subscription_mark(periodic(frequency), DisplayProfile::default());
            assert_eq!(mark, Some(("↻", role)), "{frequency:?}");
            let line = ListRow {
                subscription: mark,
                ..row("Quotes", "exit 101")
            }
            .lines(30)[0]
                .clone();
            assert_eq!(
                lead(&line, 3),
                [
                    ("↻".to_owned(), Some(role)),
                    (" ".to_owned(), Some(Role::MutedText)),
                    ("$".to_owned(), Some(Role::RunningMark))
                ],
                "{frequency:?}: the mark, a blank column, then the glyph"
            );
            assert!(
                line.text().ends_with("exit 101"),
                "nothing trails the details: {line:?}"
            );
        }
    }

    #[test]
    fn the_name_starts_in_one_column_with_or_without_a_mark() {
        let names = [
            row("Quotes", "").lines(20)[0].clone(),
            ListRow {
                subscription: Some(("↻", Role::SubscriptionLow)),
                ..row("Quotes", "")
            }
            .lines(20)[0]
                .clone(),
        ]
        .map(|line| line.text().find("Quotes").unwrap());
        assert_eq!(
            names,
            [4, 6],
            "byte offsets: `↻` takes three bytes in one column"
        );
        let narrow = ListRow {
            subscription: Some(("↻", Role::SubscriptionLow)),
            ..row("python3 -q", "exit 101 · SIGTERM")
        };
        for width in [12, 20, 60] {
            let last = narrow.lines(width).last().unwrap().text();
            assert_eq!(last.width(), width, "{last:?}");
            assert!(
                !last.ends_with('↻') && !last.ends_with(' '),
                "{width}: no mark or blank column at the right: {last:?}"
            );
        }
    }

    #[test]
    fn an_unread_row_leaves_its_first_column_bare_and_the_bar_turns_the_mark_clear_on_it() {
        let theme = super::super::style::Theme::new(super::super::style::ThemeName::ClaudeCodeDark);
        let unread = ListRow {
            glyph_role: Role::UpdateMark,
            ..row("Quotes", "")
        }
        .lines(20)[0]
            .clone();
        assert_eq!(unread.text(), row("Quotes", "").lines(20)[0].text());
        assert_eq!(
            lead(&unread, 3),
            [
                (" ".to_owned(), Some(Role::MutedText)),
                (" ".to_owned(), Some(Role::MutedText)),
                ("$".to_owned(), Some(Role::UpdateMark))
            ],
            "the glyph's colour alone tells the update"
        );
        assert_eq!(
            unread.resolve(theme, 20).spans[0].style.background,
            None,
            "the first column has no background"
        );
        assert_eq!(
            unread.band, None,
            "no bar without a subscription holding the update"
        );
        let held = ListRow {
            glyph_role: Role::UpdateMark,
            bar_columns: Some(10),
            subscription: Some(("↻", Role::SubscriptionMedium)),
            ..row("Quotes", "")
        }
        .lines(20)[0]
            .clone();
        assert_eq!(
            lead(&held, 2),
            [
                ("↻".to_owned(), Some(Role::SubscriptionMedium)),
                (" ".to_owned(), Some(Role::MutedText))
            ],
            "on the bar the `↻` keeps its frequency's colour"
        );
        assert_eq!(
            held.band.map(|band| (band.columns, band.role)),
            Some((4..14, Role::UpdateProgress)),
            "the bar starts after the icons"
        );
    }

    #[test]
    fn a_selected_row_fills_from_its_second_column_when_its_first_holds_a_mark() {
        let selected_row = || ListRow {
            selected: true,
            glyph_role: Role::SuccessMark,
            ..row("Run the console tests in the worktree", "12s")
        };
        let selected = selected_row();
        let line = selected.one_line(40);
        assert_eq!(line.text().width(), 40);
        assert!(line.text().starts_with("  $ Run the console"), "{line:?}");
        assert!(line.text().ends_with("the w…  12s"), "{line:?}");
        assert_eq!(line.spans[1].role, Some(Role::SuccessMark));
        assert_eq!(
            (line.fill, line.fill_start),
            (Some(Role::Selection), 0),
            "without a mark the selection fills the whole row"
        );
        let [name, status] = selected.two_lines(30);
        assert_eq!(name.text(), "  $ Run the console tests in");
        assert_eq!(status.text(), "    the worktree           12s");
        assert_eq!(name.fill, Some(Role::Selection));
        assert_eq!(status.fill, Some(Role::Selection));
        assert!(!line.text().contains('›'));
        let marked = ListRow {
            subscription: Some(("↻", Role::SubscriptionHigh)),
            ..selected_row()
        };
        for line in marked.lines(30) {
            assert_eq!(
                (line.fill, line.fill_start),
                (Some(Role::Selection), 1),
                "the first column keeps its own colours: {line:?}"
            );
        }
        let theme = super::super::style::Theme::new(super::super::style::ThemeName::ClaudeCodeDark);
        let unread = ListRow {
            glyph_role: Role::UpdateMark,
            ..selected_row()
        }
        .one_line(40);
        assert_eq!(
            (unread.fill, unread.fill_start),
            (Some(Role::Selection), 0),
            "an unread row without `↻` fills from its first column"
        );
        let glyph = unread
            .resolve(theme, 40)
            .spans
            .into_iter()
            .find(|span| span.text == "$")
            .unwrap();
        assert_eq!(
            (glyph.style.foreground, glyph.style.background),
            (
                theme.style(Role::UpdateMark).foreground,
                theme.style(Role::Selection).background
            ),
            "the glyph keeps the update colour on the selection"
        );
        let resolved = ListRow {
            subscription: Some(("↻", Role::SubscriptionHigh)),
            ..selected_row()
        }
        .one_line(40)
        .resolve(theme, 40);
        assert_eq!(resolved.spans[0].style.background, None, "{resolved:?}");
        assert_eq!(
            resolved.spans[1].style.background,
            theme.style(Role::Selection).background,
            "the selection starts at the second column"
        );
    }

    #[test]
    fn a_bar_and_flash_start_at_the_name_on_every_line() {
        let check = NextCheck {
            at_ms: 160_000,
            wait_ms: 60_000,
        };
        let waiting = ListRow {
            bar_columns: Some(check.elapsed_columns(26, 130_000)),
            subscription: Some(("↻", Role::SubscriptionHigh)),
            ..row("Quote board for the whole watchlist", "")
        };
        let lines = waiting.lines(30);
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert_eq!(line.fill, None);
            assert_eq!(
                line.band.clone().map(|band| (band.columns, band.role)),
                Some((4..17, Role::UpdateProgress)),
                "half the wait fills half the name area: {line:?}"
            );
        }
        let just_started = ListRow {
            bar_columns: Some(0),
            subscription: Some(("↻", Role::SubscriptionHigh)),
            ..row("Quotes", "")
        };
        assert_eq!(
            just_started.lines(30)[0]
                .band
                .clone()
                .map(|band| band.columns),
            Some(4..5),
            "the bar starts with one name cell and excludes the icons"
        );
        let flashing = ListRow {
            fill: Some(Role::UseFlashStrong),
            ..waiting
        };
        for line in flashing.lines(30) {
            assert_eq!(
                (line.fill, line.fill_start),
                (Some(Role::UseFlashStrong), 4),
                "the flash starts at the name"
            );
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
                Some((4..17, Role::UpdateProgress)),
                "a selected row's bar keeps its colour over the selection"
            );
        }
    }

    #[test]
    fn a_new_update_flashes_the_glyph_bright_dim_bright_and_it_then_stays_bright() {
        let animated = DisplayProfile::default();
        assert_eq!(
            [5_000, 5_400, 5_800, 6_200].map(|now| update_mark(Some(5_000), animated, now)),
            [
                Role::UpdateMark,
                Role::UpdateMarkDim,
                Role::UpdateMark,
                Role::UpdateMark
            ]
        );
        assert_eq!(
            update_mark(None, animated, 5_400),
            Role::UpdateMark,
            "an update without a flash stays bright"
        );
        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
        });
        assert_eq!(
            update_mark(Some(5_000), still, 5_400),
            Role::UpdateMark,
            "under Reduce motion the glyph does not flash"
        );
    }

    #[test]
    fn a_row_flashes_strong_soft_strong_for_1200_ms_in_its_colour_unless_motion_is_reduced() {
        let animated = DisplayProfile::default();
        let phases = |flash| {
            [5_000, 5_400, 5_800, 6_200].map(|now| flash_fill(Some((flash, 5_000)), animated, now))
        };
        assert_eq!(
            phases(RowFlash::TakeIn),
            [
                Some(Role::TakeInFlashStrong),
                Some(Role::TakeInFlashSoft),
                Some(Role::TakeInFlashStrong),
                None
            ]
        );
        assert_eq!(
            phases(RowFlash::Use),
            [
                Some(Role::UseFlashStrong),
                Some(Role::UseFlashSoft),
                Some(Role::UseFlashStrong),
                None
            ]
        );
        assert_eq!(flash_fill(None, animated, 5_000), None);
        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
        });
        for flash in [RowFlash::TakeIn, RowFlash::Use] {
            assert_eq!(flash_fill(Some((flash, 5_000)), still, 5_000), None);
        }
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
