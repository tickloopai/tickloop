use super::*;

/// The screen a terminal would show for everything the View wrote, including what an attached
/// Tick drew without the Console's synchronized frames.
fn terminal_screen(view: &PtyView) -> String {
    let mut parser = vt100::Parser::new(TERMINAL_HEIGHT as u16, TERMINAL_WIDTH as u16, 0);
    parser.process(&view.output.lock().unwrap());
    parser.screen().contents()
}

async fn terminal_shows(view: &PtyView, text: &str) {
    tokio::time::timeout(UI_TIMEOUT, async {
        while !terminal_screen(view).contains(text) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{text:?} never appeared:\n{}", terminal_screen(view)));
}

/// Enter on a hosted Tick outside Herdr shows its screen as a snapshot and says why it is not
/// floated. `a` attaches the same process full screen in this terminal: the Console stops
/// reading and drawing, Esc and other keys reach the Tick, and Ctrl+B q gives the terminal back
/// to the Console with a fresh snapshot and nothing typed during the attach left in its input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_monitor_shows_a_hosted_tick_snapshot_and_attaches_it_full_screen() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let script = fixture.path("work/keys.py");
        std::fs::write(&script, include_str!("../support/keys_tick.py")).unwrap();
        let (base_url, _requests) = model_http_support::serve_responses(vec![
            (
                200,
                chat_reply(
                    Some((
                        "ModifyTick",
                        "open-keys",
                        json!({"action":"open","command":[support::fixture_cli::executable(), "--tick-launch", "--", "/usr/bin/python3", "-u", script],"placement":{"type":"hidden"}}),
                    )),
                    "",
                ),
            ),
            (200, chat_reply(None, "Keys Tick opened.")),
        ])
        .await;
        fixture.set_chat_model(&base_url);
        let mut view = PtyView::start_with_flags(&fixture, &["--dangerously-skip-permissions"]);
        view.wait_for("input ready", new_input_is_visible).await;
        view.send(b"Open the keys Tick.\r");
        view.wait_for("the hosted Tick joins Monitor", |frame| {
            frame.contains("Monitor 1") && frame.contains("Keys Tick opened.")
        })
        .await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("Enter show"))
            .await;
        view.send(b"\r");
        view.wait_for("snapshot detail", |frame| {
            frame.contains("size 120x30")
                && frame.contains("No Herdr session")
                && frame.contains("a attach full screen")
        })
        .await;

        view.send(b"a");
        terminal_shows(&view, "size 100x32").await;
        view.send(b"z");
        terminal_shows(&view, "keys 7a").await;
        view.send(b"\x1b");
        terminal_shows(&view, "keys 7a 1b").await;
        view.send(b"\x02");
        tokio::time::sleep(Duration::from_millis(200)).await;
        view.send(b"q");
        view.wait_for("the Console takes the terminal back", |frame| {
            frame.contains("keys 7a 1b") && frame.contains("a attach full screen")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("back in the Monitor list", |frame| {
            frame.contains("Enter show") && !frame.contains("a attach full screen")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("input focused again", new_input_is_visible)
            .await;
        assert!(
            !view.frame().lines().any(|line| line.starts_with('❯') && line.contains('z')),
            "nothing typed during the attach reached the Console: {}",
            view.frame()
        );
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
