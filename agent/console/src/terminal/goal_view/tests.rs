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
fn status_uses_codex_wording_for_every_goal_state() {
    for (state, expected, role) in [
        (
            GoalState::Active,
            "Pursuing goal (12.5K / 50K)",
            Role::Accent,
        ),
        (
            GoalState::Paused,
            "Goal paused (/goal resume)",
            Role::MutedText,
        ),
        (
            GoalState::Blocked,
            "Goal stalled (/goal resume)",
            Role::Warning,
        ),
        (
            GoalState::BudgetLimited,
            "Goal unmet (12.5K / 50K tokens)",
            Role::Warning,
        ),
        (
            GoalState::Complete,
            "Goal achieved (12.5K tokens)",
            Role::Success,
        ),
    ] {
        let goal = GoalView {
            state,
            ..example_goal()
        };
        let line = status_line(Some(&goal), false, 100).unwrap();
        assert_eq!(line.text(), expected);
        assert_eq!(line.spans[0].role, Some(role));
        assert!(
            goal.monitor_lines(100, DisplayProfile::default(), 0)
                .iter()
                .any(|line| line.text().starts_with(state.label()))
        );
    }
}

#[test]
fn unbudgeted_goals_show_runtime_and_trigger_pause_takes_priority() {
    let mut goal = GoalView {
        token_budget: None,
        ..example_goal()
    };
    assert_eq!(
        status_line(Some(&goal), false, 100).unwrap().text(),
        "Pursuing goal (1m)"
    );
    goal.state = GoalState::Complete;
    assert_eq!(
        status_line(Some(&goal), false, 100).unwrap().text(),
        "Goal achieved (1m)"
    );
    goal.state = GoalState::BudgetLimited;
    assert_eq!(
        status_line(Some(&goal), false, 100).unwrap().text(),
        "Goal unmet (12.5K tokens)"
    );
    for goal in [Some(&goal), None] {
        assert_eq!(
            status_line(goal, true, 100).unwrap().text(),
            TRIGGERS_PAUSED
        );
    }
    assert!(status_line(None, false, 100).is_none());
}

#[test]
fn monitor_caps_condition_at_three_wrapped_lines_and_reason_at_one() {
    let goal = GoalView {
        condition: "第一行条件很长而且需要折行\n第二行\n第三行\n第四行",
        check_reason: Some("reason\nmore reason\u{1b}[31m"),
        ..example_goal()
    };
    let lines = goal.monitor_lines(12, DisplayProfile::default(), 0);
    assert_eq!(lines.len(), 6);
    assert_eq!(lines[0].text(), "Goal");
    assert!(lines[3].text().ends_with('…'));
    assert!(lines[4].text().starts_with("active"));
    assert!(lines[5].text().starts_with("Check: "));
    assert!(lines.iter().all(|line| line.text().width() <= 12));
    assert!(
        lines
            .iter()
            .all(|line| !line.text().chars().any(char::is_control))
    );
    let short = example_goal().monitor_lines(100, DisplayProfile::default(), 0);
    assert_eq!(
        text(&short),
        [
            "Goal",
            "Ship the verified release",
            "active · 12.5K / 50K",
            "Check: The acceptance check still needs evidence"
        ]
    );
}

#[test]
fn monitor_three_line_boundary_preserves_whole_graphemes() {
    for grapheme in ["👩‍💻", "👍🏽", "🇨🇳"] {
        for (condition, width, expected) in [
            (
                format!("{grapheme}abc"),
                2,
                vec![grapheme.to_owned(), "ab".into(), "c".into()],
            ),
            (
                format!("{grapheme}abcde"),
                2,
                vec![grapheme.to_owned(), "ab".into(), "c…".into()],
            ),
            (
                format!("abc\ndef\n{grapheme}xy"),
                3,
                vec!["abc".into(), "def".into(), format!("{grapheme}…")],
            ),
        ] {
            let goal = GoalView {
                condition: &condition,
                check_reason: None,
                ..example_goal()
            };
            let lines = goal.monitor_lines(width, DisplayProfile::default(), 0);
            assert_eq!(text(&lines[1..4]), expected, "{condition:?}");
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
        assert_eq!(
            status_line(Some(&goal), false, 100).unwrap().text(),
            format!("Pursuing goal ({expected})")
        );
        assert_eq!(
            goal.monitor_lines(100, DisplayProfile::default(), 0)[2].text(),
            format!("active · {expected}")
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
            goal.monitor_lines(40, display, now)
                .iter()
                .all(|line| line.fill == expected)
        );
    }
    let still = DisplayProfile::new(DisplayPreferences {
        prefers_reduced_motion: true,
    });
    assert!(
        goal.monitor_lines(40, still, 1_000)
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
            let mut lines = goal.monitor_lines(width, display, 0);
            lines.extend(goal.detail_lines(width, 0, 100));
            lines.push(status_line(Some(&goal), false, width).unwrap());
            lines.push(status_line(Some(&goal), true, width).unwrap());
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
