use tickloop_agent_config::{DisplayPreferences, GlyphSet};

use super::style::{Theme, ThemeName};

const UNICODE_SPINNER: [&str; 12] = ["·", "✢", "✳", "✶", "✻", "✽", "✽", "✻", "✶", "✳", "✢", "·"];
const NERD_FONT_CIRCLE_EMPTY: &str = "\u{f0766}";
const NERD_FONT_CIRCLE_SLICE_1: &str = "\u{f0a9e}";
const NERD_FONT_CIRCLE_SLICE_5: &str = "\u{f0aa2}";
/// Material Design circle slices filling from empty to full and draining back, holding each end
/// for a frame as the Unicode spinner does.
const NERD_FONT_SPINNER: [&str; 18] = [
    NERD_FONT_CIRCLE_EMPTY,
    NERD_FONT_CIRCLE_SLICE_1,
    "\u{f0a9f}",
    "\u{f0aa0}",
    "\u{f0aa1}",
    NERD_FONT_CIRCLE_SLICE_5,
    "\u{f0aa3}",
    "\u{f0aa4}",
    "\u{f0aa5}",
    "\u{f0aa5}",
    "\u{f0aa4}",
    "\u{f0aa3}",
    NERD_FONT_CIRCLE_SLICE_5,
    "\u{f0aa1}",
    "\u{f0aa0}",
    "\u{f0a9f}",
    NERD_FONT_CIRCLE_SLICE_1,
    NERD_FONT_CIRCLE_EMPTY,
];

/// How long a blinking glyph stays in each phase. Every blinking glyph takes its phase from the
/// same clock, so they blink together.
const BLINK_PHASE_MS: u64 = 500;
const MILLISECONDS_PER_SECOND: u64 = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Glyph {
    ShellTick,
    ArtifactTick,
    TerminalTick,
    ProtocolTick,
    Prompt,
    Assistant,
    Subagents,
    ToolCall,
    ToolResult,
    TickDelivery,
    /// An Agent message from another family member.
    AgentMessage,
    Success,
    TodoBranch,
    TodoPending,
    TodoInProgress,
    TodoCompleted,
    Failure,
    Warning,
    /// Claude Code's `✻` on a finished turn and on a compaction.
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
        match (self.preferences.glyph_set, glyph) {
            (GlyphSet::Unicode, Glyph::ShellTick) => "$",
            (GlyphSet::Unicode, Glyph::ArtifactTick) => "⧉",
            (GlyphSet::Unicode, Glyph::TerminalTick) => ">",
            (GlyphSet::Unicode, Glyph::ProtocolTick) => "◉",
            (GlyphSet::NerdFont, Glyph::ShellTick) => "\u{f489}",
            (GlyphSet::NerdFont, Glyph::ArtifactTick) => "\u{f15c}",
            (GlyphSet::NerdFont, Glyph::TerminalTick) => "\u{f120}",
            (GlyphSet::NerdFont, Glyph::ProtocolTick) => "\u{f0318}",
            (GlyphSet::Unicode, Glyph::Prompt) => "❯",
            (GlyphSet::Unicode, Glyph::Assistant | Glyph::Subagents | Glyph::ToolCall) => "⏺",
            (GlyphSet::Unicode, Glyph::ToolResult) => "⎿",
            (GlyphSet::Unicode, Glyph::TickDelivery) => "◉",
            (GlyphSet::Unicode, Glyph::AgentMessage) => "✉",
            (GlyphSet::Unicode, Glyph::TodoBranch) => "⎿",
            (GlyphSet::Unicode, Glyph::TodoPending) => "◻",
            (GlyphSet::Unicode, Glyph::TodoInProgress) => "◼",
            (GlyphSet::Unicode, Glyph::TodoCompleted) => "✔",
            (GlyphSet::NerdFont, Glyph::TodoBranch) => "\u{f060d}",
            (GlyphSet::NerdFont, Glyph::TodoPending) => "\u{f0131}",
            (GlyphSet::NerdFont, Glyph::TodoInProgress) => "\u{f012e}",
            (GlyphSet::NerdFont, Glyph::TodoCompleted) => "\u{f00c}",
            (GlyphSet::Unicode, Glyph::Success) => "✓",
            (GlyphSet::Unicode, Glyph::Failure) => "✗",
            (GlyphSet::Unicode, Glyph::Warning) => "!",
            (GlyphSet::Unicode, Glyph::TurnSummary) => "✻",
            (GlyphSet::Unicode, Glyph::ScrollUp) => "↑",
            (GlyphSet::Unicode, Glyph::ScrollDown) => "↓",
            (GlyphSet::Unicode, Glyph::AgentNeedsInput) => "◆",
            (GlyphSet::Unicode, Glyph::AgentRunning) => UNICODE_SPINNER[0],
            (GlyphSet::Unicode, Glyph::AgentWaiting) => "◷",
            (GlyphSet::Unicode, Glyph::AgentIdle) => "○",
            (GlyphSet::Unicode, Glyph::AgentCompleted) => "✔",
            (GlyphSet::Unicode, Glyph::AgentFailed | Glyph::AgentStopped) => "✖",
            (GlyphSet::Unicode, Glyph::AgentUnknown) => "?",
            (GlyphSet::Unicode, Glyph::AgentEmpty) => "◦",
            (GlyphSet::Unicode, Glyph::AgentViewed) => "●",
            (GlyphSet::Unicode, Glyph::AgentUnviewed) => "○",
            (GlyphSet::Unicode, Glyph::Collapsed) => "▸",
            (GlyphSet::Unicode, Glyph::Expanded) => "▾",
            (GlyphSet::NerdFont, Glyph::Prompt) => "\u{f0142}",
            (GlyphSet::NerdFont, Glyph::Assistant | Glyph::Subagents) => "\u{f06a9}",
            (GlyphSet::NerdFont, Glyph::ToolCall) => "\u{f1064}",
            (GlyphSet::NerdFont, Glyph::ToolResult) => "\u{f060c}",
            (GlyphSet::NerdFont, Glyph::TickDelivery) => "\u{f0318}",
            (GlyphSet::NerdFont, Glyph::AgentMessage) => "\u{f01ee}",
            (GlyphSet::NerdFont, Glyph::Success) => "\u{f05e1}",
            (GlyphSet::NerdFont, Glyph::Failure) => "\u{f015a}",
            (GlyphSet::NerdFont, Glyph::Warning) => "\u{f0026}",
            (GlyphSet::NerdFont, Glyph::TurnSummary) => NERD_FONT_CIRCLE_SLICE_5,
            (GlyphSet::NerdFont, Glyph::ScrollUp) => "\u{f0143}",
            (GlyphSet::NerdFont, Glyph::ScrollDown) => "\u{f0140}",
            (GlyphSet::NerdFont, Glyph::AgentNeedsInput) => "\u{f173b}",
            (GlyphSet::NerdFont, Glyph::AgentRunning) => NERD_FONT_CIRCLE_SLICE_1,
            (GlyphSet::NerdFont, Glyph::AgentWaiting) => "\u{f051f}",
            (GlyphSet::NerdFont, Glyph::AgentIdle) => "\u{f03e6}",
            (GlyphSet::NerdFont, Glyph::AgentCompleted) => "\u{f05e1}",
            (GlyphSet::NerdFont, Glyph::AgentFailed | Glyph::AgentStopped) => "\u{f015a}",
            (GlyphSet::NerdFont, Glyph::AgentUnknown) => "\u{f0625}",
            (GlyphSet::NerdFont, Glyph::AgentEmpty) => "\u{f09df}",
            (GlyphSet::NerdFont, Glyph::AgentViewed) => "\u{f111}",
            (GlyphSet::NerdFont, Glyph::AgentUnviewed) => "\u{f10c}",
            (GlyphSet::NerdFont, Glyph::Collapsed) => "\u{f0142}",
            (GlyphSet::NerdFont, Glyph::Expanded) => "\u{f0140}",
        }
    }

    /// Under Reduce motion every frame is the still running glyph.
    pub(super) fn spinner_frame(self, frame: usize) -> &'static str {
        if self.preferences.prefers_reduced_motion {
            return self.glyph(Glyph::AgentRunning);
        }
        let frames = self.spinner_frames();
        frames[frame % frames.len()]
    }

    fn spinner_frames(self) -> &'static [&'static str] {
        match self.preferences.glyph_set {
            GlyphSet::Unicode => &UNICODE_SPINNER,
            GlyphSet::NerdFont => &NERD_FONT_SPINNER,
        }
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

    pub(super) fn preview(self) -> String {
        let mut glyphs = vec![
            self.glyph(Glyph::Prompt),
            self.glyph(Glyph::Assistant),
            self.glyph(Glyph::ToolCall),
            self.glyph(Glyph::ToolResult),
            self.glyph(Glyph::TickDelivery),
            self.glyph(Glyph::Success),
            self.glyph(Glyph::TodoBranch),
            self.glyph(Glyph::TodoPending),
            self.glyph(Glyph::TodoInProgress),
            self.glyph(Glyph::TodoCompleted),
            self.glyph(Glyph::Failure),
            self.glyph(Glyph::Warning),
            self.glyph(Glyph::ScrollUp),
            self.glyph(Glyph::ScrollDown),
        ];
        glyphs.extend((0..self.spinner_frames().len()).map(|frame| self.spinner_frame(frame)));
        glyphs.join("  ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_state_icons_use_the_verified_nerd_font_names_and_unicode_fallbacks() {
        let expected = [
            (Glyph::AgentNeedsInput, "◆", "\u{f173b}"),
            (Glyph::AgentCompleted, "✔", "\u{f05e1}"),
            (Glyph::AgentFailed, "✖", "\u{f015a}"),
            (Glyph::AgentStopped, "✖", "\u{f015a}"),
            (Glyph::AgentIdle, "○", "\u{f03e6}"),
            (Glyph::AgentWaiting, "◷", "\u{f051f}"),
        ];
        for glyph_set in [GlyphSet::Unicode, GlyphSet::NerdFont] {
            let profile = DisplayProfile::new(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            for (glyph, unicode, nerd) in expected {
                assert_eq!(
                    profile.glyph(glyph),
                    if glyph_set == GlyphSet::Unicode {
                        unicode
                    } else {
                        nerd
                    }
                );
                assert_eq!(
                    unicode_width::UnicodeWidthStr::width(profile.glyph(glyph)),
                    1
                );
            }
        }
    }

    #[test]
    fn display_profile_maps_semantic_glyphs_and_reduced_motion() {
        let unicode = DisplayProfile::new(DisplayPreferences::default());
        assert_eq!(unicode.glyph(Glyph::Prompt), "❯");
        assert_eq!(unicode.glyph(Glyph::Assistant), "⏺");
        assert_eq!(unicode.glyph(Glyph::ToolResult), "⎿");
        assert_eq!(unicode.spinner_frame(0), "·");
        assert_eq!(unicode.spinner_frame(1), "✢");

        let nerd_font = DisplayProfile::new(DisplayPreferences {
            glyph_set: GlyphSet::NerdFont,
            prefers_reduced_motion: true,
        });
        assert_eq!(nerd_font.glyph(Glyph::Prompt), "\u{f0142}");
        assert_eq!(nerd_font.glyph(Glyph::Assistant), "\u{f06a9}");
        assert_eq!(nerd_font.glyph(Glyph::ToolResult), "\u{f060c}");
        assert_eq!(nerd_font.spinner_frame(7), "\u{f0a9e}");

        let moving_nerd_font = DisplayProfile::new(DisplayPreferences {
            glyph_set: GlyphSet::NerdFont,
            prefers_reduced_motion: false,
        });
        let frames = (0..NERD_FONT_SPINNER.len())
            .map(|frame| moving_nerd_font.spinner_frame(frame))
            .collect::<Vec<_>>();
        assert_eq!(frames.first(), Some(&"\u{f0766}"), "it starts empty");
        assert_eq!(frames.last(), Some(&"\u{f0766}"), "it drains back to empty");
        assert_eq!(
            frames.iter().filter(|frame| **frame == "\u{f0aa5}").count(),
            2,
            "it holds the full circle for a frame"
        );
        assert_eq!(
            moving_nerd_font.spinner_frame(NERD_FONT_SPINNER.len()),
            "\u{f0766}",
            "it loops"
        );
    }

    #[test]
    fn monitor_type_glyphs_keep_their_shapes_in_one_column() {
        let unicode = DisplayProfile::default();
        for (glyph, expected) in [
            (Glyph::ShellTick, "$"),
            (Glyph::ProtocolTick, "◉"),
            (Glyph::TerminalTick, ">"),
            (Glyph::ArtifactTick, "⧉"),
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
            ..Default::default()
        });
        assert!(!still.blink_dimmed(1_500));
        assert_eq!(
            still.next_running_change_ms(1_200),
            2_000,
            "only the elapsed time still ticks"
        );
    }

    #[test]
    fn subagents_glyph_uses_the_robot_with_a_single_column_unicode_fallback() {
        for (glyph_set, expected, expanded) in [
            (GlyphSet::Unicode, "⏺", "▾"),
            (GlyphSet::NerdFont, "\u{f06a9}", "\u{f0140}"),
        ] {
            let profile = DisplayProfile::new(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            assert_eq!(profile.glyph(Glyph::Subagents), expected);
            assert_eq!(unicode_width::UnicodeWidthStr::width(expected), 1);
            assert_eq!(profile.glyph(Glyph::Expanded), expanded);
        }
    }

    #[test]
    fn agent_state_glyphs_have_single_cell_geometry_in_both_glyph_sets() {
        let states = [
            Glyph::AgentNeedsInput,
            Glyph::AgentFailed,
            Glyph::AgentRunning,
            Glyph::AgentWaiting,
            Glyph::AgentIdle,
            Glyph::AgentCompleted,
            Glyph::AgentStopped,
            Glyph::AgentUnknown,
            Glyph::AgentEmpty,
        ];
        for glyph_set in [GlyphSet::Unicode, GlyphSet::NerdFont] {
            let profile = DisplayProfile::new(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            for glyph in states.into_iter().chain([
                Glyph::Collapsed,
                Glyph::Expanded,
                Glyph::ShellTick,
                Glyph::ArtifactTick,
                Glyph::TerminalTick,
                Glyph::ProtocolTick,
                Glyph::TodoBranch,
                Glyph::TodoPending,
                Glyph::TodoInProgress,
                Glyph::TodoCompleted,
            ]) {
                assert_eq!(
                    unicode_width::UnicodeWidthStr::width(profile.glyph(glyph)),
                    1,
                    "{glyph_set:?} {glyph:?}: layout calculation counts one column (unicode-width); terminal font geometry is not verified here"
                );
            }
        }
    }
}
