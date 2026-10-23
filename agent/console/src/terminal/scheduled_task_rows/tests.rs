use super::*;
use crate::terminal::style::{ColourDepth, ThemeName};
use tickloop_agent_config::DisplayPreferences;
use time::UtcOffset;
use unicode_width::UnicodeWidthStr;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap()
        .to_offset(UtcOffset::from_hms(8, 0, 0).unwrap())
}

fn task() -> ScheduledTaskView<'static> {
    ScheduledTaskView {
        schedule: ScheduleView::Interval {
            minutes: 5,
            cron: "*/5 * * * *",
        },
        instruction: "Check the build",
        created_at: at(0),
        expires_at: Some(at(604_800)),
        last_trigger_at: Some(at(3_600)),
        next_trigger_at: Some(at(3_900)),
        scheduled_at: Some(at(3_900)),
        source: "user",
        skipped: 2,
        pending_confirmation: false,
        triggers_paused: false,
        triggered_at_ms: None,
    }
}

fn render(seconds: i64) -> ScheduledTaskRender {
    ScheduledTaskRender {
        display: DisplayProfile::default(),
        ascii: false,
        now: at(seconds),
        now_ms: 1_000,
    }
}

fn text(lines: &[RoleLine]) -> String {
    lines
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn rows_show_cadence_instruction_local_time_and_confirmation_without_selection_cursor() {
    for (schedule, label) in [
        (task().schedule, "5m"),
        (ScheduleView::SelfPaced, "auto"),
        (ScheduleView::Cron("0 9 * * 1"), "0 9 * * 1"),
        (ScheduleView::Once("0 9 * * *"), "once"),
    ] {
        let task = ScheduledTaskView {
            schedule,
            pending_confirmation: true,
            ..task()
        };
        let lines = task.row_lines(100, true, render(3_750));
        let shown = text(&lines);
        assert!(shown.contains(label), "{shown}");
        assert!(shown.contains("Check the build"));
        assert!(lines[0].text().contains("pending"));
        assert!(lines[0].text().ends_with("09:05"));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].text(), "  Check the build");
        assert!(shown.starts_with("◷ "));
        assert!(!shown.contains('❯'));
        assert!(lines.iter().all(|line| line.fill == Some(Role::Selection)));
        assert!(lines.iter().all(|line| line.band.is_none()));
    }
}

#[test]
fn rows_preserve_whole_graphemes_in_instruction_summaries() {
    for grapheme in ["👩‍💻", "👍🏽", "🇨🇳"] {
        let instruction = format!("{grapheme}abcdefghijk");
        let task = ScheduledTaskView {
            schedule: ScheduleView::Cron("x"),
            instruction: &instruction,
            ..task()
        };
        assert_eq!(
            text(&task.row_lines(13, false, render(3_750))),
            format!("◷ x · 09:05\n  {grapheme}abcdefgh…")
        );
    }
}

#[test]
fn each_display_time_keeps_its_own_local_offset_across_clock_changes() {
    let next = at(3_900).to_offset(UtcOffset::from_hms(-5, 0, 0).unwrap());
    let task = ScheduledTaskView {
        next_trigger_at: Some(next),
        scheduled_at: Some(next),
        expires_at: Some(next),
        ..task()
    };
    let render = ScheduledTaskRender {
        now: at(3_750).to_offset(UtcOffset::from_hms(-4, 0, 0).unwrap()),
        ..render(3_750)
    };
    assert!(text(&task.row_lines(80, false, render)).contains("20:05"));
    let details = text(&task.detail_lines(80, 0, 100));
    assert!(details.contains("Created: 1970-01-01 08:00"));
    assert!(details.contains("Expires: 1969-12-31 20:05"));
}

#[test]
fn progress_runs_from_last_trigger_and_stays_full_while_due() {
    let task = task();
    for (seconds, expected) in [
        (3_500, 0),
        (3_600, 0),
        (3_750, 20),
        (3_900, 40),
        (5_000, 40),
    ] {
        assert_eq!(task.progress(40, at(seconds)), Some(expected));
    }
    let lines = task.row_lines(44, false, render(3_750));
    assert_eq!(lines[0].band.as_ref().unwrap().columns, 2..23);
    assert_eq!(lines[0].band.as_ref().unwrap().role, Role::UpdateProgress);
    assert_eq!(
        task.row_lines(44, false, render(4_000))[0]
            .band
            .as_ref()
            .unwrap()
            .columns,
        2..44
    );
    let created = ScheduledTaskView {
        last_trigger_at: None,
        ..task.clone()
    };
    assert_eq!(created.progress(40, at(1_950)), Some(0));
    assert_eq!(created.progress(40, at(3_000)), Some(20));
    assert!(
        created.row_lines(44, false, render(1_950))[0]
            .band
            .is_none()
    );
    let unscheduled = ScheduledTaskView {
        next_trigger_at: None,
        scheduled_at: None,
        ..task.clone()
    };
    assert_eq!(unscheduled.progress(40, at(4_000)), None);
    assert!(text(&unscheduled.row_lines(80, false, render(4_000))).contains("not scheduled"));
    let invalid = ScheduledTaskView {
        last_trigger_at: Some(at(4_500)),
        ..task
    };
    assert_eq!(invalid.progress(40, at(3_000)), None);
}

#[test]
fn paused_receiving_greys_the_row_and_suppresses_progress_and_trigger_flash() {
    let task = ScheduledTaskView {
        triggers_paused: true,
        triggered_at_ms: Some(1_000),
        ..task()
    };
    for selected in [false, true] {
        let lines = task.row_lines(44, selected, render(3_750));
        assert!(lines[0].band.is_none());
        assert!(lines[1].band.is_none());
        for line in lines {
            assert_eq!(line.fill, selected.then_some(Role::Selection));
            assert!(
                line.spans
                    .iter()
                    .all(|span| span.role == Some(Role::MutedText))
            );
        }
    }
    for theme in ThemeName::ALL {
        let mut render = render(3_750);
        render.display.set_theme(theme);
        let line = &task.row_lines(44, false, render)[0];
        let resolved = line.resolve(render.display.theme(), 44);
        let backgrounds = resolved
            .spans
            .iter()
            .filter_map(|span| span.style.background)
            .collect::<Vec<_>>();
        assert!(backgrounds.is_empty());
    }
}

#[test]
fn trigger_flash_uses_shared_phases_selection_and_reduced_motion() {
    let task = ScheduledTaskView {
        triggered_at_ms: Some(1_000),
        ..task()
    };
    for (now_ms, fill) in [
        (1_000, Some(Role::TakeInFlashStrong)),
        (1_400, Some(Role::TakeInFlashSoft)),
        (1_800, Some(Role::TakeInFlashStrong)),
        (2_200, None),
    ] {
        let lines = task.row_lines(
            44,
            false,
            ScheduledTaskRender {
                now_ms,
                ..render(3_750)
            },
        );
        assert!(lines.iter().all(|line| line.fill == fill));
        assert_eq!(lines[0].band.is_none(), fill.is_some());
    }
    let lines = task.row_lines(44, true, render(3_750));
    assert!(lines.iter().all(|line| line.fill == Some(Role::Selection)));
    let still = DisplayProfile::new(DisplayPreferences {
        prefers_reduced_motion: true,
    });
    let lines = task.row_lines(
        44,
        false,
        ScheduledTaskRender {
            display: still,
            ..render(3_750)
        },
    );
    assert!(lines.iter().all(|line| line.fill.is_none()));
    assert!(lines[0].band.is_some());
}

#[test]
fn details_include_full_instruction_cron_dates_and_skip_count_and_scroll() {
    let task = ScheduledTaskView {
        instruction: "first\nsecond\nthird\nfourth",
        ..task()
    };
    let lines = task.detail_lines(80, 0, usize::MAX);
    assert_eq!(
        text(&lines),
        [
            "Scheduled task",
            "Cron: */5 * * * *",
            "Instruction: first",
            "second",
            "third",
            "fourth",
            "Created: 1970-01-01 08:00",
            "Expires: 1970-01-08 08:00",
            "Next run: 1970-01-01T09:05:00+08:00",
            "Source: user",
            "Skipped: 2"
        ]
        .join("\n")
    );
    assert_eq!(task.detail_lines(80, 3, 3), lines[3..6]);
    assert!(task.detail_lines(80, 0, 0).is_empty());
    assert!(task.detail_lines(80, usize::MAX, 10).is_empty());
    let self_paced = ScheduledTaskView {
        schedule: ScheduleView::SelfPaced,
        expires_at: None,
        ..task
    };
    let shown = text(&self_paced.detail_lines(80, 0, 100));
    assert!(shown.contains("Cron: self-paced"));
    assert!(shown.contains("Expires: none"));
}

#[test]
fn narrow_rows_long_text_ascii_icon_and_both_themes_render_safely() {
    let task = ScheduledTaskView {
        instruction: "检查编译结果👩‍💻\n\u{1b}[31mfollow up with a very long instruction",
        ..task()
    };
    for width in [0, 1, 2, 3, 4, 8, 20, 80] {
        for theme in ThemeName::ALL {
            for ascii in [false, true] {
                let mut render = ScheduledTaskRender {
                    ascii,
                    ..render(3_750)
                };
                render.display.set_theme(theme);
                let mut lines = task.row_lines(width, false, render);
                if width >= 4 {
                    assert!(lines[0].text().starts_with(if ascii { "@ " } else { "◷ " }));
                }
                lines.extend(task.detail_lines(width, 0, 100));
                for line in lines {
                    assert!(
                        line.text().width() <= width,
                        "width {width}: {:?}",
                        line.text()
                    );
                    assert!(!line.text().chars().any(char::is_control));
                    let shown = line
                        .resolve(render.display.theme(), width)
                        .render(ColourDepth::TrueColour);
                    assert!(!shown.contains('\n'));
                }
            }
        }
    }
}

#[test]
fn scheduled_task_render_evidence() {
    let task = ScheduledTaskView {
        instruction: "每小时帮我看看特斯拉股价并记录变化",
        ..task()
    };
    let lines = task.row_lines(48, false, render(3_750));
    println!("{}", text(&lines));
    assert_eq!(
        text(&lines),
        "◷ 5m · 09:05\n  每小时帮我看看特斯拉股价并记录变化"
    );
}

#[test]
fn two_line_rows_prioritize_time_and_use_nominal_dates_and_compact_cadences() {
    for (schedule, expected) in [
        (
            ScheduleView::Interval {
                minutes: 60,
                cron: "0 * * * *",
            },
            "1h",
        ),
        (
            ScheduleView::Interval {
                minutes: 1440,
                cron: "0 0 * * *",
            },
            "1d",
        ),
        (ScheduleView::Cron("0 12 * * 2"), "0 12 * * 2"),
        (ScheduleView::Once("0 12 * * *"), "once"),
        (ScheduleView::SelfPaced, "auto"),
    ] {
        let task = ScheduledTaskView {
            schedule,
            scheduled_at: Some(at(86400 + 3600)),
            ..task()
        };
        let lines = task.row_lines(48, false, render(3750));
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].text(),
            format!(
                "◷ {expected} · {}01-02 09:00",
                if schedule == ScheduleView::SelfPaced {
                    "~"
                } else {
                    ""
                }
            )
        );
        let narrow = task.row_lines(20, false, render(3750));
        assert!(narrow[0].text().ends_with("01-02 09:00"), "{narrow:?}");
        assert!(narrow.iter().all(|line| line.text().width() <= 20));
    }
    let auto = ScheduledTaskView {
        schedule: ScheduleView::SelfPaced,
        scheduled_at: None,
        next_trigger_at: None,
        ..task()
    };
    assert_eq!(
        auto.row_lines(40, false, render(3750))[0].text(),
        "◷ auto · ~"
    );
}

#[test]
fn progress_uses_only_last_thirty_minutes_on_the_first_line_and_rounds_wide_cells_down() {
    let task = ScheduledTaskView {
        schedule: ScheduleView::Cron("中0文"),
        instruction: "每小时中文与 ASCII mixed",
        created_at: at(0),
        last_trigger_at: Some(at(0)),
        next_trigger_at: Some(at(7200)),
        scheduled_at: Some(at(7200)),
        ..task()
    };
    for (now, expected) in [
        (5000, 0),
        (5400, 0),
        (5850, 5),
        (6300, 10),
        (7200, 20),
        (7400, 20),
    ] {
        assert_eq!(task.progress(20, at(now)), Some(expected));
        let lines = task.row_lines(24, false, render(now));
        assert!(lines[1].band.is_none());
        if expected == 0 {
            assert!(lines[0].band.is_none());
        }
    }
    // One display column into a wide glyph must leave the whole glyph unfilled.
    let lines = task.row_lines(24, false, render(5490));
    assert!(lines[0].band.is_none());
    let lines = task.row_lines(24, false, render(5670));
    assert_eq!(lines[0].band.as_ref().unwrap().columns, 2..5);
    assert_eq!(floor_display_column("中0文", 1), 0);
    assert_eq!(floor_display_column("中0文", 2), 2);
    assert_eq!(floor_display_column("中0文", 4), 3);
    for theme in ThemeName::ALL {
        let mut display = DisplayProfile::default();
        display.set_theme(theme);
        let resolved = lines[0].resolve(display.theme(), 24);
        assert!(resolved.spans.iter().any(|span| span.text.contains("中0")));
        assert!(
            lines[1]
                .resolve(display.theme(), 24)
                .spans
                .iter()
                .all(|span| span.style.background.is_none())
        );
    }
}

#[test]
fn paused_receiving_replaces_every_kind_of_next_time_with_a_muted_paused_label() {
    for schedule in [
        task().schedule,
        ScheduleView::Once("0 9 * * *"),
        ScheduleView::Cron("0 12 * * 2"),
        ScheduleView::SelfPaced,
    ] {
        let task = ScheduledTaskView {
            schedule,
            triggers_paused: true,
            ..task()
        };
        let lines = task.row_lines(48, false, render(5000));
        assert!(lines[0].text().ends_with("paused"), "{lines:?}");
        assert!(!lines[0].text().contains("09:05"));
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.role == Some(Role::MutedText))
        );
        assert!(lines[0].text().starts_with("◷ "));
        assert!(lines[1].text().starts_with("  Check"));
    }
}
