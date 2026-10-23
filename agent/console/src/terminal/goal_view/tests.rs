use super::*;
use crate::terminal::style::{ColourDepth, ThemeName};
use tickloop_agent_config::DisplayPreferences;
use unicode_width::UnicodeWidthStr;

fn example_goal() -> GoalView<'static> {
    GoalView {
        condition: "Ship the verified release",
        state: GoalState::Active,
        tokens_used: 12_500,
        token_budget: Some(50_000),
        elapsed_ms: 90_000,
        check_reason: Some("The acceptance check still needs evidence"),
        continued_at_ms: None,
    }
}

fn text(lines: &[RoleLine]) -> Vec<String> {
    lines.iter().map(RoleLine::text).collect()
}

#[test]
fn monitor_uses_state_titles_and_colours_for_every_goal_state() {
    for state in [
        GoalState::Active,
        GoalState::Paused,
        GoalState::Blocked,
        GoalState::BudgetLimited,
        GoalState::Complete,
    ] {
        let goal = GoalView {
            state,
            ..example_goal()
        };
        let monitor = goal.monitor_lines(100, false, DisplayProfile::default(), false, 0);
        assert!(
            monitor[0]
                .text()
                .starts_with(&format!("◎ {}", state.title()))
        );
        assert!(
            monitor[0]
                .spans
                .iter()
                .any(|span| span.text.contains(state.title()) && span.role == Some(state.role()))
        );
        assert!(monitor[1].text().contains(goal.condition));
    }
}

#[test]
fn receiving_status_only_shows_paused_triggers() {
    assert_eq!(
        receiving_status_line(true, 100).unwrap().text(),
        TRIGGERS_PAUSED
    );
    assert!(receiving_status_line(false, 100).is_none());
}

#[test]
fn monitor_has_state_title_right_aligned_time_and_one_condition_line() {
    let goal = GoalView {
        condition: "第一行条件很长而且需要折行\n第二行\n第三行\n第四行",
        check_reason: Some("reason\nmore reason\u{1b}[31m"),
        ..example_goal()
    };
    let lines = goal.monitor_lines(12, false, DisplayProfile::default(), false, 0);
    assert_eq!(text(&lines), ["◎ Pursui… 1m", "  第一行条…"]);
    assert!(lines.iter().all(|line| line.text().width() <= 12));
    assert!(
        lines
            .iter()
            .all(|line| !line.text().chars().any(char::is_control))
    );
    let short = example_goal().monitor_lines(60, false, DisplayProfile::default(), false, 0);
    assert_eq!(short.len(), 2);
    assert_eq!(short[0].text().width(), 60);
    assert!(short[0].text().ends_with("1m"));
    assert_eq!(short[1].text(), "  Ship the verified release");
    assert!(
        short
            .iter()
            .all(|line| !line.text().contains("Check:") && !line.text().contains("tokens"))
    );
}

#[test]
fn monitor_summary_truncation_preserves_whole_graphemes() {
    for grapheme in ["👩‍💻", "👍🏽", "🇨🇳"] {
        for (condition, width, expected) in [
            (format!("{grapheme}abc"), 5, format!("  {grapheme}…")),
            (format!("{grapheme}abcde"), 4, "  …".into()),
            (grapheme.to_owned(), 4, format!("  {grapheme}")),
            (format!("abc\ndef\n{grapheme}xy"), 6, "  abc…".into()),
        ] {
            let goal = GoalView {
                condition: &condition,
                ..example_goal()
            };
            let lines = goal.monitor_lines(width, false, DisplayProfile::default(), false, 0);
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[1].text().trim_end(), expected, "{condition:?}");
        }
    }
}

#[test]
fn monitor_selection_wins_over_flash_and_keeps_text_indent() {
    let goal = GoalView {
        continued_at_ms: Some(1000),
        ..example_goal()
    };
    for width in [1, 2, 3, 4, 8, 20, 80] {
        for selected in [false, true] {
            let lines = goal.monitor_lines(width, selected, DisplayProfile::default(), false, 1000);
            assert_eq!(lines.len(), 2);
            assert!(lines.iter().all(|line| line.fill
                == Some(if selected {
                    Role::Selection
                } else {
                    Role::TakeInFlashStrong
                })));
            assert!(lines.iter().all(|line| line.text().width() <= width));
            assert!(lines[0].text().starts_with('◎'));
            assert!(lines[1].text().starts_with(&" ".repeat(width.min(2))));
        }
    }
}

#[test]
fn unbudgeted_goal_runtime_uses_compact_minute_hour_and_day_boundaries() {
    for (elapsed_ms, expected) in [
        (0, "0s"),
        (59_999, "59s"),
        (60_000, "1m"),
        (90_000, "1m"),
        (3_599_999, "59m"),
        (3_600_000, "1h"),
        (3_659_999, "1h"),
        (3_660_000, "1h 1m"),
        (5_400_000, "1h 30m"),
        (7_199_999, "1h 59m"),
        (7_200_000, "2h"),
        (86_399_999, "23h 59m"),
        (86_400_000, "1d 0h 0m"),
        (258_120_000, "2d 23h 42m"),
    ] {
        let goal = GoalView {
            token_budget: None,
            elapsed_ms,
            ..example_goal()
        };
        assert_eq!(goal.usage(), expected, "{elapsed_ms}ms");
        assert!(
            goal.monitor_lines(100, false, DisplayProfile::default(), false, 0)[0]
                .text()
                .ends_with(expected)
        );
        assert_eq!(
            goal.detail_lines(100, 0, 100)[3].text(),
            format!("Usage: {expected}")
        );
    }
}

#[test]
fn details_preserve_full_condition_and_reason_and_scroll_by_display_row() {
    let goal = GoalView {
        condition: "one\ntwo\nthree\nfour\nfive",
        check_reason: Some("first reason\nsecond reason"),
        ..example_goal()
    };
    let lines = goal.detail_lines(80, 0, usize::MAX);
    assert_eq!(
        text(&lines),
        [
            "Goal",
            "one",
            "two",
            "three",
            "four",
            "five",
            "Status: active",
            "Usage: 12.5K / 50K",
            "Check: first reason",
            "second reason"
        ]
    );
    assert_eq!(goal.detail_lines(80, 4, 3), lines[4..7]);
    assert!(goal.detail_lines(80, usize::MAX, 3).is_empty());
    assert!(goal.detail_lines(80, 0, 0).is_empty());
}

#[test]
fn continuation_flash_reuses_take_in_phases_and_reduced_motion() {
    let goal = GoalView {
        continued_at_ms: Some(1_000),
        ..example_goal()
    };
    let display = DisplayProfile::default();
    for (now, expected) in [
        (1_000, Some(Role::TakeInFlashStrong)),
        (1_400, Some(Role::TakeInFlashSoft)),
        (1_800, Some(Role::TakeInFlashStrong)),
        (2_200, None),
    ] {
        assert!(
            goal.monitor_lines(40, false, display, false, now)
                .iter()
                .all(|line| line.fill == expected)
        );
    }
    let still = DisplayProfile::new(DisplayPreferences {
        prefers_reduced_motion: true,
    });
    assert!(
        goal.monitor_lines(40, false, still, false, 1_000)
            .iter()
            .all(|line| line.fill.is_none())
    );
}

#[test]
fn narrow_and_wide_unicode_views_fit_and_resolve_in_both_themes() {
    let goal = GoalView {
        condition: "目标👩‍💻检查\nlong-condition-without-spaces",
        ..example_goal()
    };
    for width in [0, 1, 2, 3, 8, 20, 80] {
        for theme in ThemeName::ALL {
            let mut display = DisplayProfile::default();
            display.set_theme(theme);
            let mut lines = goal.monitor_lines(width, false, display, false, 0);
            lines.extend(goal.detail_lines(width, 0, 100));
            lines.extend(receiving_status_line(false, width));
            lines.extend(receiving_status_line(true, width));
            for line in lines {
                assert!(
                    line.text().width() <= width,
                    "width {width}: {:?}",
                    line.text()
                );
                let rendered = line
                    .resolve(display.theme(), width)
                    .render(ColourDepth::TrueColour);
                assert!(!rendered.contains("\n"));
            }
        }
    }
}

#[test]
fn goal_notices_use_semantic_roles_and_keep_untrusted_reasons_on_one_line() {
    use tickloop_agent_protocol::{GoalBlock, GoalNotice, GoalPause};
    for (notice, role) in [
        (GoalNotice::Set, Role::Accent),
        (GoalNotice::Completed, Role::Success),
        (
            GoalNotice::Blocked {
                by: GoalBlock::Reported {
                    reason: "blocked\nreason\x1b[3J".into(),
                },
            },
            Role::Warning,
        ),
        (GoalNotice::BudgetLimited, Role::Warning),
        (
            GoalNotice::Paused {
                by: GoalPause::User,
            },
            Role::MutedText,
        ),
        (GoalNotice::Cleared, Role::MutedText),
    ] {
        let pieces = notice_pieces(&notice, "first\nsecond\tthird\x1b");
        let super::super::console::ConversationPiece::Notice {
            text,
            role: actual_role,
        } = &pieces[1]
        else {
            panic!("Goal notices are single-row events")
        };
        assert_eq!(*actual_role, role);
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\t'));
        assert_eq!(
            text.contains("first second third"),
            !matches!(notice, GoalNotice::Paused { .. } | GoalNotice::Cleared)
        );
    }
}

#[test]
fn selected_monitor_title_keeps_state_colour_on_the_selection_background() {
    for theme in ThemeName::ALL {
        let mut display = DisplayProfile::default();
        display.set_theme(theme);
        for state in [
            GoalState::Active,
            GoalState::Paused,
            GoalState::Blocked,
            GoalState::BudgetLimited,
            GoalState::Complete,
        ] {
            let goal = GoalView {
                state,
                ..example_goal()
            };
            let lines = goal.monitor_lines(48, true, display, false, 0);
            let resolved = lines[0].resolve(display.theme(), 48);
            let title = resolved
                .spans
                .iter()
                .find(|span| span.text.contains(state.title()))
                .unwrap();
            assert_eq!(
                title.style.foreground,
                display.theme().style(state.role()).foreground
            );
            assert_eq!(
                title.style.background,
                display.theme().style(Role::Selection).background
            );
        }
    }
}

#[test]
fn goal_glyph_has_an_ascii_fallback_with_matching_indentation_and_state_colour() {
    for ascii in [false, true] {
        let lines = example_goal().monitor_lines(48, false, DisplayProfile::default(), ascii, 0);
        assert!(lines[0].text().starts_with(if ascii {
            "o Pursuing goal"
        } else {
            "◎ Pursuing goal"
        }));
        assert_eq!(lines[0].spans[0].role, Some(Role::Accent));
        assert!(lines[1].text().starts_with("  Ship"));
        assert_eq!(lines[0].text().width(), 48);
    }
}
