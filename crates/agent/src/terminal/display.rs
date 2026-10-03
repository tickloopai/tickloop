use tickloop_agent::settings::{DisplayPreferences, GlyphSet};

use super::style::{Theme, ThemeName};

const UNICODE_SPINNER: [&str; 12] = ["·", "✢", "✳", "✶", "✻", "✽", "✽", "✻", "✶", "✳", "✢", "·"];
const NERD_FONT_SPINNER: [&str; 12] = [
    "\u{f0a9e}",
    "\u{f0a9f}",
    "\u{f0aa0}",
    "\u{f0aa1}",
    "\u{f0aa2}",
    "\u{f0aa3}",
    "\u{f0aa4}",
    "\u{f0aa3}",
    "\u{f0aa2}",
    "\u{f0aa1}",
    "\u{f0aa0}",
    "\u{f0a9f}",
];

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
    Success,
    TodoBranch,
    TodoPending,
    TodoInProgress,
    TodoCompleted,
    Failure,
    Warning,
    ScrollUp,
    ScrollDown,
    /// Agent states in the overview and family lists; each pairs with its status word.
    AgentNeedsInput,
    AgentRunning,
    AgentWaiting,
    AgentIdle,
    AgentStopped,
    AgentUnknown,
    AgentEmpty,
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
            (GlyphSet::Unicode, Glyph::ArtifactTick) => "▤",
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
            (GlyphSet::Unicode, Glyph::ScrollUp) => "↑",
            (GlyphSet::Unicode, Glyph::ScrollDown) => "↓",
            (GlyphSet::Unicode, Glyph::AgentNeedsInput) => "●",
            (GlyphSet::Unicode, Glyph::AgentRunning) => "▶",
            (GlyphSet::Unicode, Glyph::AgentWaiting) => "◌",
            (GlyphSet::Unicode, Glyph::AgentIdle) => "○",
            (GlyphSet::Unicode, Glyph::AgentStopped) => "■",
            (GlyphSet::Unicode, Glyph::AgentUnknown) => "?",
            (GlyphSet::Unicode, Glyph::AgentEmpty) => "◦",
            (GlyphSet::Unicode, Glyph::Collapsed) => "▸",
            (GlyphSet::Unicode, Glyph::Expanded) => "▾",
            (GlyphSet::NerdFont, Glyph::Prompt) => "\u{f0142}",
            (GlyphSet::NerdFont, Glyph::Assistant | Glyph::Subagents) => "\u{f06a9}",
            (GlyphSet::NerdFont, Glyph::ToolCall) => "\u{f1064}",
            (GlyphSet::NerdFont, Glyph::ToolResult) => "\u{f060c}",
            (GlyphSet::NerdFont, Glyph::TickDelivery) => "\u{f0318}",
            (GlyphSet::NerdFont, Glyph::Success) => "\u{f05e0}",
            (GlyphSet::NerdFont, Glyph::Failure) => "\u{f0159}",
            (GlyphSet::NerdFont, Glyph::Warning) => "\u{f0026}",
            (GlyphSet::NerdFont, Glyph::ScrollUp) => "\u{f0143}",
            (GlyphSet::NerdFont, Glyph::ScrollDown) => "\u{f0140}",
            (GlyphSet::NerdFont, Glyph::AgentNeedsInput) => "\u{f0f3}",
            (GlyphSet::NerdFont, Glyph::AgentRunning) => "\u{f04b}",
            (GlyphSet::NerdFont, Glyph::AgentWaiting) => "\u{f0150}",
            (GlyphSet::NerdFont, Glyph::AgentIdle) => "\u{f0766}",
            (GlyphSet::NerdFont, Glyph::AgentStopped) => "\u{f04d}",
            (GlyphSet::NerdFont, Glyph::AgentUnknown) => "\u{f0625}",
            (GlyphSet::NerdFont, Glyph::AgentEmpty) => "\u{f09df}",
            (GlyphSet::NerdFont, Glyph::Collapsed) => "\u{f0142}",
            (GlyphSet::NerdFont, Glyph::Expanded) => "\u{f0140}",
        }
    }

    pub(super) fn spinner_frame(self, frame: usize) -> &'static str {
        let frame = if self.preferences.prefers_reduced_motion {
            0
        } else {
            frame % UNICODE_SPINNER.len()
        };
        match self.preferences.glyph_set {
            GlyphSet::Unicode => UNICODE_SPINNER[frame],
            GlyphSet::NerdFont => NERD_FONT_SPINNER[frame],
        }
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
        glyphs.extend((0..UNICODE_SPINNER.len()).map(|frame| self.spinner_frame(frame)));
        glyphs.join("  ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn agent_state_glyphs_differ_per_state_in_both_glyph_sets() {
        let states = [
            Glyph::AgentNeedsInput,
            Glyph::Failure,
            Glyph::AgentRunning,
            Glyph::AgentWaiting,
            Glyph::AgentIdle,
            Glyph::Success,
            Glyph::AgentStopped,
            Glyph::AgentUnknown,
            Glyph::AgentEmpty,
        ];
        for glyph_set in [GlyphSet::Unicode, GlyphSet::NerdFont] {
            let profile = DisplayProfile::new(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            let shapes = states
                .iter()
                .map(|glyph| profile.glyph(*glyph))
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(shapes.len(), states.len(), "{glyph_set:?}");
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
