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
    const THINKING_DELTAS: usize = 8192;
    const ATTACHED_REPLY: &str = "Reply tail received while the Tick owned the terminal.";
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let script = fixture.path("work/keys.py");
        std::fs::write(&script, include_str!("../support/keys_tick.py")).unwrap();
        let thinking_delta = format!(
            "data: {}\n\n",
            json!({"id":"cli-fixture","choices":[{"index":0,"delta":{"reasoning_content":"思"},"finish_reason":null}]})
        );
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
            (
                200,
                thinking_delta.repeat(THINKING_DELTAS) + &chat_reply(None, ATTACHED_REPLY),
            ),
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
        let client = fixture.client().await;
        let execution = catalog(&client).await[0].execution;
        wait_snapshot(&client, execution, "the opening turn finished", |snapshot| {
            snapshot.conversation.loops.iter().all(|goal| {
                goal.state == tickloop_agent::runtime::LoopState::Completed
            })
        })
        .await;
        let attached_output_start = view.output.lock().unwrap().len();
        client
            .execute(CommandEnvelope {
                owner_epoch: client.owner_epoch(),
                command_id: uuid::Uuid::new_v4(),
                command: AgentCommand::Submit {
                    execution,
                    target_loop: None,
                    input: UserInput::Text {
                        text: "Think while the Tick owns the terminal.".into(),
                        contexts: Vec::new(),
                        paths: Vec::new(),
                    },
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        wait_snapshot(&client, execution, "the attached stream completed", |snapshot| {
            snapshot.events.iter().any(|event| {
                matches!(&event.kind, tickloop_agent::runtime::EventKind::Text(text) if text.contains(ATTACHED_REPLY))
            }) && snapshot.conversation.loops.iter().all(|goal| {
                goal.state == tickloop_agent::runtime::LoopState::Completed
            })
        })
        .await;
        let attached_output = view.output.lock().unwrap()[attached_output_start..].to_vec();
        assert!(
            !String::from_utf8_lossy(&attached_output).contains(COMPLETED_FRAME_START),
            "Console frames must stay suppressed while the Tick owns the terminal"
        );
        assert!(
            !String::from_utf8_lossy(&attached_output).contains(ATTACHED_REPLY),
            "the Agent reply must not overwrite the attached Tick"
        );
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
        view.wait_for("the attached stream tail survived reclaim", |frame| {
            frame.contains(ATTACHED_REPLY)
        })
        .await;
        assert!(
            !String::from_utf8_lossy(&view.output.lock().unwrap()).contains("The view fell behind"),
            "internal recovery must remain invisible across attach and reclaim"
        );
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
