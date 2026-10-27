//! A single-line list row: state glyph, clipped name, right-aligned details and subscription
//! mark. Selection, update progress and row flashes cover the whole row.

use unicode_width::UnicodeWidthStr;

use super::{
    display::{DisplayProfile, Glyph},
    loop_view_text::ellipsize,
    style::{Role, RoleLine},
};
use crate::monitor_rows::{StatusTone, SubscriptionMark};
use tickloop_harness::protocol::TickSubscriptionFrequency;

/// The indentation of panel rows outside Monitor.
pub(super) const ROW_INDENT: &str = "  ";
const NAME_GAP: &str = " ";
const SUBSCRIPTION_GAP: &str = " ";

pub(super) fn compact_name_column(glyph: &str) -> usize {
    glyph.width() + NAME_GAP.width()
}
/// Columns reserved for a row's glyph, right-aligned details and subscription mark.
pub(super) fn reserved_columns(
    glyph: &str,
    details: &str,
    subscription: Option<(&str, Role)>,
) -> usize {
    let mark_columns = subscription.map_or(0, |(mark, _)| {
        mark.width()
            + if details.is_empty() {
                0
            } else {
                SUBSCRIPTION_GAP.width()
            }
    });
    let trailing_columns = details.width() + mark_columns;
    compact_name_column(glyph)
        + if trailing_columns == 0 {
            0
        } else {
            STATUS_GAP_COLUMNS + trailing_columns
        }
}

pub(super) const STATUS_GAP_COLUMNS: usize = 2;
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
        let mut line = RoleLine::role(self.glyph, self.glyph_role);
        line.push(NAME_GAP, Some(role));
        line
    }

    pub(super) fn lines(&self, width: usize) -> Vec<RoleLine> {
        self.lines_matching(width, "")
    }

    /// The row with the matches of the list's `filter` emphasized in its name and in the
    /// leading segment of its details; see [`push_details`].
    pub(super) fn lines_matching(&self, width: usize, filter: &str) -> Vec<RoleLine> {
        vec![self.one_line(width, filter)]
    }

    fn one_line(&self, width: usize, filter: &str) -> RoleLine {
        let mut tail = RoleLine::default();
        let mark_columns = self.subscription.map_or(0, |(mark, _)| mark.width());
        let gap = if mark_columns > 0 && !self.details.is_empty() {
            SUBSCRIPTION_GAP.width()
        } else {
            0
        };
        let details = ellipsize(
            &self.details,
            width.saturating_sub(compact_name_column(self.glyph) + mark_columns + gap),
        );
        push_details(&mut tail, &details, Role::MutedText, filter);
        if let Some((mark, role)) = self.subscription {
            if !details.is_empty() {
                tail.push(SUBSCRIPTION_GAP, None);
            }
            tail.push(mark, Some(role));
        }
        let tail_columns = tail.text().width();
        let mut line = self.prefix();
        line.truncate(width.saturating_sub(tail_columns));
        let name_columns = width.saturating_sub(reserved_columns(
            self.glyph,
            &self.details,
            self.subscription,
        ));
        line.push_matching(
            &ellipsize(self.name, name_columns),
            Some(self.role()),
            filter,
        );
        line.push(
            " ".repeat(width.saturating_sub(line.text().width() + tail_columns)),
            None,
        );
        line.spans.extend(tail.spans);
        self.finish(line, width)
    }

    /// Selection takes priority over a flash, with the progress bar over the selection.
    /// All fills start at the first column, including the glyph and subscription mark.
    fn finish(&self, mut line: RoleLine, width: usize) -> RoleLine {
        if line.text().width() > width {
            line.truncate(width);
        }
        let bar = self
            .bar_columns
            .filter(|_| width > 0)
            .map(|columns| 0..columns.max(1).min(width));
        line.preserve_selection_colours = self
            .subscription
            .is_some_and(|(_, role)| role == Role::MutedText);
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

/// Pushes a row's `details` in `role`, emphasizing the matches of `filter` only in their leading
/// segment, the row's source, which a filter reads; the later segments, such as a version or a
/// state, are not filtered on.
pub(super) fn push_details(line: &mut RoleLine, details: &str, role: Role, filter: &str) {
    let source_end = details.find(SEGMENT_SEPARATOR).unwrap_or(details.len());
    line.push_matching(&details[..source_end], Some(role), filter);
    line.push(&details[source_end..], Some(role));
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
    /// Content arrived, or an Agent turn took the row's update in: warm.
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

/// The `↻` at the right of a row with periodic subscriptions, in its frequency's colour.
pub(super) fn subscription_mark(
    subscription: Option<SubscriptionMark>,
    display: DisplayProfile,
) -> Option<(&'static str, Role)> {
    subscription.map(|mark| {
        (
            display.glyph(Glyph::Subscription),
            if mark.paused {
                Role::MutedText
            } else {
                subscription_role(mark.frequency)
            },
        )
    })
}

/// The details at a row's right. The glyph's colour already tells the state and the mark the
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
    use tickloop_code_settings::DisplayPreferences;

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
            paused: false,
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
    fn long_selected_and_mixed_width_names_stay_on_one_line() {
        for name in [
            "Rerun full test suite in background to verify",
            "中英文 mixed 名称 very long 👩‍💻👍🏽🇨🇳",
        ] {
            for selected in [false, true] {
                for width in 0..=60 {
                    let lines = ListRow {
                        selected,
                        subscription: Some(("↻", Role::SubscriptionHigh)),
                        ..row(name, "14s")
                    }
                    .lines(width);
                    assert_eq!(lines.len(), 1);
                    let text = lines[0].text();
                    assert!(text.width() <= width, "{width}: {text:?}");
                    if width >= 10 {
                        assert!(text.starts_with("$ "), "{text}");
                        assert!(text.ends_with("14s ↻"), "{text}");
                        if width < 25 {
                            assert!(text.contains('…'), "{text}");
                        }
                    }
                    assert_eq!(lines[0].fill == Some(Role::Selection), selected);
                }
            }
        }
        assert_eq!(
            row("Rerun full test suite in background to verify", "14s").lines(30)[0].text(),
            "$ Rerun full test suite…   14s"
        );
    }

    #[test]
    fn clipping_names_preserves_whole_graphemes() {
        for grapheme in ["👩‍💻", "👍🏽", "🇨🇳"] {
            let name = format!("{grapheme}abc");
            assert_eq!(row(&name, "").lines(4)[0].text(), "$ … ");
            assert_eq!(row(&name, "").lines(5)[0].text(), format!("$ {grapheme}…"));
        }
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
    fn periodic_subscriptions_trail_the_details_in_their_frequency_colour() {
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
            .one_line(30, "");
            assert!(line.text().starts_with("$ Quotes"), "{line:?}");
            assert!(line.text().ends_with("exit 101 ↻"), "{line:?}");
            let last = line.spans.last().unwrap();
            assert_eq!((last.text.as_str(), last.role), ("↻", Some(role)));
        }
    }

    #[test]
    fn all_tick_types_start_at_the_first_column() {
        for glyph in ["◉", ">", "$", "↗"] {
            for subscription in [None, Some(("↻", Role::SubscriptionHigh))] {
                let line = ListRow {
                    glyph,
                    subscription,
                    ..row("Example", "")
                }
                .one_line(20, "");
                assert!(
                    line.text().starts_with(&format!("{glyph} Example")),
                    "{line:?}"
                );
            }
        }
    }

    #[test]
    fn subscribed_and_plain_rows_start_at_the_same_column_and_keep_their_tails() {
        for subscription in [None, Some(("↻", Role::SubscriptionLow))] {
            let line = ListRow {
                subscription,
                ..row("Quotes", "")
            }
            .one_line(20, "");
            assert!(line.text().starts_with("$ Quotes"), "{line:?}");
            for width in 7..=60 {
                let line = ListRow {
                    subscription,
                    ..row("python3 -q with a long name", "14s")
                }
                .one_line(width, "");
                assert_eq!(line.text().width(), width, "{line:?}");
                assert!(
                    line.text().ends_with(if subscription.is_some() {
                        "14s ↻"
                    } else {
                        "14s"
                    }),
                    "{line:?}"
                );
            }
        }
    }

    #[test]
    fn oversized_details_keep_the_type_glyph_and_trailing_subscription_mark() {
        for width in 3..=20 {
            let line = ListRow {
                subscription: Some(("↻", Role::SubscriptionLow)),
                ..row("python3 -q", "exit 101 · SIGTERM")
            }
            .one_line(width, "");
            assert_eq!(line.text().width(), width, "{line:?}");
            assert!(
                line.text().starts_with("$ ") && line.text().ends_with('↻'),
                "{line:?}"
            );
        }
    }

    #[test]
    fn an_unread_row_colours_its_first_column_and_progress_covers_the_whole_row() {
        let unread = ListRow {
            glyph_role: Role::UpdateMark,
            ..row("Quotes", "")
        }
        .one_line(20, "");
        assert_eq!(lead(&unread, 1), [("$".to_owned(), Some(Role::UpdateMark))]);
        assert_eq!(unread.band, None);
        for columns in [0, 10, 20, 30] {
            let held = ListRow {
                glyph_role: Role::UpdateMark,
                bar_columns: Some(columns),
                subscription: Some(("↻", Role::SubscriptionMedium)),
                ..row("Quotes", "")
            }
            .one_line(20, "");
            assert_eq!(
                held.band.clone().map(|band| (band.columns, band.role)),
                Some((0..columns.clamp(1, 20), Role::UpdateProgress))
            );
            let resolved = held.resolve(
                super::super::style::Theme::new(super::super::style::ThemeName::ClaudeCodeDark),
                20,
            );
            let mark = resolved.spans.last().unwrap();
            assert_eq!(mark.text, "↻");
            if columns >= 20 {
                let theme =
                    super::super::style::Theme::new(super::super::style::ThemeName::ClaudeCodeDark);
                assert_eq!(
                    mark.style.background,
                    theme.style(Role::UpdateProgress).background
                );
                assert_eq!(
                    mark.style.foreground,
                    theme.style(Role::SubscriptionMedium).foreground
                );
            }
        }
    }

    #[test]
    fn selection_and_flashes_fill_the_whole_row_with_or_without_a_subscription() {
        let theme = super::super::style::Theme::new(super::super::style::ThemeName::ClaudeCodeDark);
        for subscription in [None, Some(("↻", Role::SubscriptionHigh))] {
            for selected in [false, true] {
                let fill = if selected {
                    Role::Selection
                } else {
                    Role::UseFlashStrong
                };
                let line = ListRow {
                    selected,
                    glyph_role: Role::UpdateMark,
                    subscription,
                    fill: Some(Role::UseFlashStrong),
                    ..row("Run the console tests in the worktree", "12s")
                }
                .one_line(30, "");
                assert_eq!((line.fill, line.fill_start), (Some(fill), 0));
                assert!(line.text().starts_with("$ Run the console"), "{line:?}");
                assert!(line.text().contains('…'), "{line:?}");
                let resolved = line.resolve(theme, 30);
                assert!(
                    resolved
                        .spans
                        .iter()
                        .all(|span| span.style.background == theme.style(fill).background),
                    "{resolved:?}"
                );
                assert_eq!(
                    resolved.spans[0].style.foreground,
                    theme.style(Role::UpdateMark).foreground
                );
                if subscription.is_some() {
                    let mark = resolved.spans.last().unwrap();
                    assert_eq!(mark.text, "↻");
                    assert_eq!(
                        mark.style.foreground,
                        theme.style(Role::SubscriptionHigh).foreground
                    );
                }
            }
        }
    }

    #[test]
    fn a_bar_fills_the_row_and_selection_wins_over_a_flash() {
        let check = NextCheck {
            at_ms: 160_000,
            wait_ms: 60_000,
        };
        let waiting = ListRow {
            bar_columns: Some(check.elapsed_columns(30, 130_000)),
            subscription: Some(("↻", Role::SubscriptionHigh)),
            ..row("Quote board for the whole watchlist", "")
        };
        let lines = waiting.lines(30);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].band.clone().map(|band| (band.columns, band.role)),
            Some((0..15, Role::UpdateProgress))
        );
        let flashing = ListRow {
            fill: Some(Role::UseFlashStrong),
            ..waiting
        };
        let line = flashing.one_line(30, "");
        assert_eq!(
            (line.fill, line.fill_start, line.band),
            (Some(Role::UseFlashStrong), 0, None)
        );
        let selected = ListRow {
            selected: true,
            ..flashing
        }
        .one_line(30, "");
        assert_eq!(selected.fill, Some(Role::Selection));
        assert_eq!(
            selected.band.map(|band| (band.columns, band.role)),
            Some((0..15, Role::UpdateProgress))
        );
    }

    #[test]
    fn paused_subscriptions_keep_a_muted_mark_at_the_right_in_narrow_rows() {
        let mut subscription = periodic(TickSubscriptionFrequency::High).unwrap();
        subscription.paused = true;
        for width in 0..=12 {
            let line = ListRow {
                selected: true,
                subscription: subscription_mark(Some(subscription), DisplayProfile::default()),
                ..row("中文 long name 👩‍💻", "22s")
            }
            .one_line(width, "");
            assert!(line.text().width() <= width, "{line:?}");
            assert_eq!((line.fill, line.fill_start), (Some(Role::Selection), 0));
            if width >= 3 {
                assert!(line.text().starts_with("$ "), "{line:?}");
            }
            if width > 0 {
                assert!(line.text().ends_with('↻'), "{line:?}");
                assert_eq!(line.spans.last().unwrap().role, Some(Role::MutedText));
                let theme =
                    super::super::style::Theme::new(super::super::style::ThemeName::ClaudeCodeDark);
                let resolved = line.resolve(theme, width);
                let mark = resolved.spans.last().unwrap();
                assert_eq!(
                    mark.style.foreground,
                    theme.style(Role::MutedText).foreground
                );
                assert_eq!(
                    mark.style.background,
                    theme.style(Role::Selection).background
                );
            }
            if width >= 7 {
                assert!(line.text().ends_with("22s ↻"), "{line:?}");
            }
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
