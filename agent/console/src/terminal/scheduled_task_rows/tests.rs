use super::*;
use crate::terminal::style::{Colour, ColourDepth, ThemeName};
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
        (task().schedule, "every 5m"),
        (ScheduleView::SelfPaced, "self-paced"),
        (ScheduleView::Cron("0 9 * * 1"), "0 9 * * 1"),
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
        assert!(shown.contains("09:05 · pending confirmation"));
        assert!(shown.starts_with("  ◷ "));
        assert!(!shown.contains('❯'));
        assert!(lines.iter().all(|line| line.fill == Some(Role::Selection)));
        assert!(lines.iter().all(|line| line.band.is_none()));
    }
}

#[test]
fn each_display_time_keeps_its_own_local_offset_across_clock_changes() {
    let next = at(3_900).to_offset(UtcOffset::from_hms(-5, 0, 0).unwrap());
    let task = ScheduledTaskView {
        next_trigger_at: Some(next),
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
    assert_eq!(lines[0].band.as_ref().unwrap().columns, 4..24);
    assert_eq!(lines[0].band.as_ref().unwrap().role, Role::UpdateProgress);
    assert_eq!(
        task.row_lines(44, false, render(4_000))[0]
            .band
            .as_ref()
            .unwrap()
            .columns,
        4..44
    );
    let created = ScheduledTaskView {
        last_trigger_at: None,
        ..task.clone()
    };
    assert_eq!(created.progress(40, at(1_950)), Some(20));
    let unscheduled = ScheduledTaskView {
        next_trigger_at: None,
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
fn paused_receiving_greys_the_progress_and_suppresses_trigger_flash() {
    let task = ScheduledTaskView {
        triggers_paused: true,
        triggered_at_ms: Some(1_000),
        ..task()
    };
    for selected in [false, true] {
        let lines = task.row_lines(44, selected, render(3_750));
        for line in lines {
            assert_eq!(line.band.as_ref().unwrap().role, Role::UserMessageBg);
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
        assert!(!backgrounds.is_empty());
        for background in backgrounds {
            let Colour::Rgb(rgb) = background else {
                panic!("paused progress uses the theme's grey surface")
            };
            assert_eq!(rgb.red, rgb.green);
            assert_eq!(rgb.green, rgb.blue);
        }
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
                    assert!(
                        lines[0]
                            .text()
                            .starts_with(if ascii { "  @ " } else { "  ◷ " })
                    );
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
