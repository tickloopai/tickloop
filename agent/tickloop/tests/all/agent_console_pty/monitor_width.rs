use super::*;
use unicode_width::UnicodeWidthStr;

const WORD_COUNT: usize = 40;
const LAPTOP_WIDTH: usize = 100;
const MINIMUM_WIDTH: usize = 80;
const OVERLAY_WIDTH: usize = 70;

/// The conversation text of each content row with Monitor on its right; empty when Monitor is not
/// shown.
fn conversation_beside_monitor(frame: &str) -> Vec<&str> {
    frame
        .lines()
        .take_while(|line| !line.starts_with('─'))
        .filter_map(|line| line.split_once('│').map(|(left, _)| left))
        .collect()
}

fn shows_every_word(rows: &[&str]) -> bool {
    let shown = rows.concat();
    (0..WORD_COUNT).all(|index| shown.contains(&format!("word{index:02}")))
}

/// Whether Monitor takes `monitor_columns` of a `terminal_columns` terminal, the console's
/// reserved last column aside, and leaves the conversation the rest.
fn monitor_takes(frame: &str, terminal_columns: usize, monitor_columns: usize) -> bool {
    let rows = conversation_beside_monitor(frame);
    !rows.is_empty()
        && rows
            .iter()
            .all(|row| row.width() == terminal_columns - 1 - monitor_columns)
}

async fn run_monitor_width(fixture: &PrivateFixture) {
    let mut view = PtyView::start(fixture);
    view.wait_for_editor("View ready", Some("")).await;
    // The command line names no word itself, so the last word on screen means the Shell printed.
    // Eight words a line keep every word within a result's preview.
    view.send(
        format!(
            "!for index in $(seq 0 {}); do printf 'word%02d ' $index; [ $((index % 8)) -eq 7 ] && echo; done\r",
            WORD_COUNT - 1
        )
        .as_bytes(),
    );
    view.wait_for("printed words", |frame| {
        frame.replace('\n', "").contains("word39")
    })
    .await;
    view.wait_for_editor("echo finished", Some("")).await;
    view.send(b"!printf monitor-width; while [ ! -f finish ]; do sleep 0.02; done\r");
    view.wait_for("running Shell", |frame| frame.contains("ctrl+b background"))
        .await;
    view.send(b"\x02");
    for (terminal_columns, monitor_columns) in [(LAPTOP_WIDTH, 42), (MINIMUM_WIDTH, 32)] {
        view.resize(terminal_columns, TERMINAL_HEIGHT).await;
        let beside = view
            .wait_for("Monitor beside the conversation", |frame| {
                frame.contains("Shells (1)")
                    && monitor_takes(frame, terminal_columns, monitor_columns)
            })
            .await;
        eprintln!("[monitor-width {terminal_columns}]\n{beside}\n[/monitor-width]");
        assert!(
            shows_every_word(&conversation_beside_monitor(&beside)),
            "the conversation reflows beside Monitor: {beside}"
        );
    }

    view.resize(OVERLAY_WIDTH, TERMINAL_HEIGHT).await;
    view.wait_for("Monitor hidden below the minimum width", |frame| {
        !frame.contains("Shells (1)")
    })
    .await;
    view.send(b"/monitor\r");
    let overlay = view
        .wait_for("Monitor overlay", |frame| {
            frame.contains("Shells (1)") && monitor_takes(frame, OVERLAY_WIDTH, 42)
        })
        .await;
    eprintln!("[monitor-width {OVERLAY_WIDTH}]\n{overlay}\n[/monitor-width]");
    assert!(
        !shows_every_word(&conversation_beside_monitor(&overlay)),
        "below the minimum width Monitor covers the conversation: {overlay}"
    );
    view.send(b"\x1b");
    std::fs::write(fixture.path("work").join("finish"), "").unwrap();
    view.close().await;
}

#[tokio::test]
async fn actual_cli_monitor_sits_beside_the_reflowed_conversation_down_to_80_columns() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_monitor_width(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
