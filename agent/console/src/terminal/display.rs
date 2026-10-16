use tickloop_agent_config::DisplayPreferences;
use tickloop_ui::icons::{TICK_PROGRESS_FRAMES, TICK_SPINNER_FRAMES, TICK_SPINNER_INTERVAL_MS};

use super::style::{Theme, ThemeName};

/// How long a blinking glyph stays in each phase. Every blinking glyph takes its phase from the
/// same clock, so they blink together.
const BLINK_PHASE_MS: u64 = 500;
/// A one-off flash runs a little quicker than the blink, so it reads as an event beside it.
const FLASH_PHASE_MS: u64 = 400;
/// Strong, soft, strong: the flash ends after its third phase.
const FLASH_PHASES: u64 = 3;
const MILLISECONDS_PER_SECOND: u64 = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Glyph {
    ShellTick,
    ArtifactTick,
    TerminalTick,
    ProtocolTick,
    /// A Monitor row's periodic Tick Subscription; its colour tells the frequency.
    Subscription,
    Prompt,
    Assistant,
    Subagents,
    ToolCall,
    ToolResult,
    TickDelivery,
    /// Claude Code's `›` before an Agent message from another family member.
    AgentMessage,
    Success,
    TodoPending,
    TodoInProgress,
    TodoCompleted,
    Failure,
    Warning,
    /// The mark of a finished turn and of a compaction: the reply dot, muted with its line.
    TurnSummary,
    ScrollUp,
    ScrollDown,
    /// Agent states in the overview list; each pairs with its status word.
    AgentNeedsInput,
    AgentRunning,
    AgentWaiting,
    AgentIdle,
    AgentCompleted,
    AgentFailed,
    /// Stopped, interrupted or expired: ended without completing, not by failure.
    AgentStopped,
    AgentUnknown,
    AgentEmpty,
    /// The Subagents panel's member dot, filled for the member this View shows; its colour is the
    /// member's state.
    AgentViewed,
    AgentUnviewed,
    /// Group headings that fold their rows.
    Collapsed,
    Expanded,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DisplayProfile {
    theme: Theme,
    preferences: DisplayPreferences,
}

impl DisplayProfile {
    pub(super) const fn new(preferences: DisplayPreferences) -> Self {
        Self {
            theme: Theme::new(ThemeName::ClaudeCodeDark),
            preferences,
        }
    }

    pub(super) const fn theme(self) -> Theme {
        self.theme
    }

    pub(super) fn set_theme(&mut self, name: ThemeName) {
        self.theme = Theme::new(name);
    }

    pub(super) const fn preferences(self) -> DisplayPreferences {
        self.preferences
    }

    pub(super) fn set_preferences(&mut self, preferences: DisplayPreferences) {
        self.preferences = preferences;
    }

    pub(super) const fn glyph(self, glyph: Glyph) -> &'static str {
        match glyph {
            Glyph::ShellTick => "$",
            Glyph::ArtifactTick => "↗",
            Glyph::TerminalTick => ">",
            Glyph::ProtocolTick | Glyph::TickDelivery => "◉",
            Glyph::Subscription => "↻",
            Glyph::Prompt => "❯",
            Glyph::Assistant | Glyph::Subagents | Glyph::ToolCall | Glyph::TurnSummary => "⏺",
            Glyph::ToolResult => "⎿",
            Glyph::AgentMessage => "›",
            Glyph::TodoPending => "◻",
            Glyph::TodoInProgress => "◼",
            Glyph::TodoCompleted | Glyph::AgentCompleted => "✔",
            Glyph::Success => "✓",
            Glyph::Failure => "✗",
            Glyph::Warning => "!",
            Glyph::ScrollUp => "↑",
            Glyph::ScrollDown => "↓",
            Glyph::AgentNeedsInput => "◆",
            Glyph::AgentRunning => TICK_SPINNER_FRAMES[0],
            Glyph::AgentWaiting => "◷",
            Glyph::AgentIdle | Glyph::AgentUnviewed => "○",
            Glyph::AgentFailed | Glyph::AgentStopped => "✖",
            Glyph::AgentUnknown => "?",
            Glyph::AgentEmpty => "◦",
            Glyph::AgentViewed => "●",
            Glyph::Collapsed => "▸",
            Glyph::Expanded => "▾",
        }
    }

    /// The working spinner `elapsed_ms` into its run: the lit tick steps clockwise every
    /// [`TICK_SPINNER_INTERVAL_MS`]. Under Reduce motion it rests at 12 o'clock.
    pub(super) fn spinner_at(self, elapsed_ms: u64) -> &'static str {
        if self.preferences.prefers_reduced_motion {
            return self.glyph(Glyph::AgentRunning);
        }
        let frame = elapsed_ms / TICK_SPINNER_INTERVAL_MS % TICK_SPINNER_FRAMES.len() as u64;
        TICK_SPINNER_FRAMES[frame as usize]
    }

    /// The mark of a delivered Tick Update: each seq lights the next tick, so consecutive
    /// updates step around the ring.
    pub(super) fn tick_update_glyph(self, seq: u64) -> &'static str {
        TICK_SPINNER_FRAMES[(seq % TICK_SPINNER_FRAMES.len() as u64) as usize]
    }

    /// A periodic subscription's progress toward its next check: one lit tick per eighth of the
    /// period elapsed, and the first from the start so the ring never goes blank.
    pub(super) fn subscription_progress(self, elapsed_eighths: usize) -> &'static str {
        TICK_PROGRESS_FRAMES[elapsed_eighths.clamp(1, TICK_PROGRESS_FRAMES.len()) - 1]
    }

    /// Whether a blinking glyph is in its dimmed phase at `now_ms`; under Reduce motion it never
    /// dims.
    pub(super) fn blink_dimmed(self, now_ms: u64) -> bool {
        !self.preferences.prefers_reduced_motion && now_ms / BLINK_PHASE_MS % 2 == 1
    }

    /// When a running row next changes by time alone: its glyph's next blink phase, or under
    /// Reduce motion the next second of its elapsed time.
    pub(super) fn next_running_change_ms(self, now_ms: u64) -> u64 {
        let period = if self.preferences.prefers_reduced_motion {
            MILLISECONDS_PER_SECOND
        } else {
            BLINK_PHASE_MS
        };
        (now_ms / period + 1) * period
    }

    /// Whether a one-off flash that began at `since_ms` shows its strong phase at `now_ms`:
    /// `Some(true)` in its first and third phase, `Some(false)` in the second, and `None` once it
    /// ended and always under Reduce motion. A start a little ahead of this clock counts as its
    /// first phase.
    pub(super) fn flash_strong(self, since_ms: u64, now_ms: u64) -> Option<bool> {
        self.flash_phase(since_ms, now_ms)
            .map(|phase| phase % 2 == 0)
    }

    /// When a flash that began at `since_ms` next changes; `None` once it ended or under Reduce
    /// motion, so a finished flash schedules no frame.
    pub(super) fn next_flash_change_ms(self, since_ms: u64, now_ms: u64) -> Option<u64> {
        self.flash_phase(since_ms, now_ms)
            .map(|phase| since_ms + (phase + 1) * FLASH_PHASE_MS)
    }

    fn flash_phase(self, since_ms: u64, now_ms: u64) -> Option<u64> {
        let phase = now_ms.saturating_sub(since_ms) / FLASH_PHASE_MS;
        (!self.preferences.prefers_reduced_motion && phase < FLASH_PHASES).then_some(phase)
    }

    pub(super) fn navigation_hint(self, hint: &str) -> String {
        hint.replace(
            "↑↓",
            &format!(
                "{}{}",
                self.glyph(Glyph::ScrollUp),
                self.glyph(Glyph::ScrollDown)
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_profile_maps_semantic_glyphs_and_reduced_motion() {
        let animated = DisplayProfile::default();
        assert_eq!(animated.glyph(Glyph::Prompt), "❯");
        assert_eq!(animated.glyph(Glyph::Assistant), "⏺");
        assert_eq!(animated.glyph(Glyph::ToolResult), "⎿");
        assert_eq!(animated.glyph(Glyph::AgentNeedsInput), "◆");
        assert_eq!(animated.glyph(Glyph::AgentWaiting), "◷");
        assert_eq!(animated.spinner_at(0), "⠁");
        assert_eq!(animated.spinner_at(109), "⠁");
        assert_eq!(animated.spinner_at(110), "⠈");
        assert_eq!(animated.spinner_at(7 * 110), "⠂");
        assert_eq!(animated.spinner_at(8 * 110), "⠁", "it loops");

        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
        });
        assert_eq!(still.spinner_at(770), "⠁", "the still running glyph");
    }

    #[test]
    fn tick_updates_step_the_ring_and_progress_fills_it() {
        let display = DisplayProfile::default();
        let steps = (41..=49)
            .map(|seq| display.tick_update_glyph(seq))
            .collect::<String>();
        assert_eq!(steps, "⠈⠐⠠⢀⡀⠄⠂⠁⠈");
        let progress = (0..=9)
            .map(|eighths| display.subscription_progress(eighths))
            .collect::<String>();
        assert_eq!(progress, "⠁⠁⠉⠙⠹⢹⣹⣽⣿⣿");
    }

    #[test]
    fn monitor_type_glyphs_keep_their_shapes_in_one_column() {
        let unicode = DisplayProfile::default();
        for (glyph, expected) in [
            (Glyph::ShellTick, "$"),
            (Glyph::ProtocolTick, "◉"),
            (Glyph::TerminalTick, ">"),
            (Glyph::ArtifactTick, "↗"),
        ] {
            assert_eq!(unicode.glyph(glyph), expected);
            assert_eq!(unicode_width::UnicodeWidthStr::width(expected), 1);
        }
    }

    #[test]
    fn running_glyphs_blink_together_every_half_second_unless_motion_is_reduced() {
        let animated = DisplayProfile::default();
        assert!(!animated.blink_dimmed(1_000));
        assert!(!animated.blink_dimmed(1_499));
        assert!(animated.blink_dimmed(1_500));
        assert!(!animated.blink_dimmed(2_000));
        assert_eq!(animated.next_running_change_ms(1_200), 1_500);
        assert_eq!(animated.next_running_change_ms(1_500), 2_000);
        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
        });
        assert!(!still.blink_dimmed(1_500));
        assert_eq!(
            still.next_running_change_ms(1_200),
            2_000,
            "only the elapsed time still ticks"
        );
    }

    #[test]
    fn a_flash_runs_strong_soft_strong_for_1200_ms_unless_motion_is_reduced() {
        let animated = DisplayProfile::default();
        assert_eq!(
            [999, 1_000, 1_399, 1_400, 1_799, 1_800, 2_199, 2_200]
                .map(|now| animated.flash_strong(1_000, now)),
            [
                Some(true),
                Some(true),
                Some(true),
                Some(false),
                Some(false),
                Some(true),
                Some(true),
                None
            ],
            "a start just ahead of this clock already shows the first phase"
        );
        assert_eq!(animated.next_flash_change_ms(1_000, 1_100), Some(1_400));
        assert_eq!(
            animated.next_flash_change_ms(1_000, 1_900),
            Some(2_200),
            "its end is its last frame"
        );
        assert_eq!(animated.next_flash_change_ms(1_000, 2_200), None);
        let still = DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
        });
        assert_eq!(still.flash_strong(1_000, 1_000), None);
        assert_eq!(still.next_flash_change_ms(1_000, 1_000), None);
    }

    #[test]
    fn every_glyph_takes_one_column() {
        let profile = DisplayProfile::default();
        for glyph in [
            Glyph::ShellTick,
            Glyph::ArtifactTick,
            Glyph::TerminalTick,
            Glyph::ProtocolTick,
            Glyph::Subscription,
            Glyph::Prompt,
            Glyph::Assistant,
            Glyph::Subagents,
            Glyph::ToolCall,
            Glyph::ToolResult,
            Glyph::TickDelivery,
            Glyph::AgentMessage,
            Glyph::Success,
            Glyph::TodoPending,
            Glyph::TodoInProgress,
            Glyph::TodoCompleted,
            Glyph::Failure,
            Glyph::Warning,
            Glyph::TurnSummary,
            Glyph::ScrollUp,
            Glyph::ScrollDown,
            Glyph::AgentNeedsInput,
            Glyph::AgentRunning,
            Glyph::AgentWaiting,
            Glyph::AgentIdle,
            Glyph::AgentCompleted,
            Glyph::AgentFailed,
            Glyph::AgentStopped,
            Glyph::AgentUnknown,
            Glyph::AgentEmpty,
            Glyph::AgentViewed,
            Glyph::AgentUnviewed,
            Glyph::Collapsed,
            Glyph::Expanded,
        ] {
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(profile.glyph(glyph)),
                1,
                "{glyph:?}: layout calculation counts one column (unicode-width); terminal font geometry is not verified here"
            );
        }
    }
}
