use super::*;
use tickloop_agent_protocol::AgentAccess;

/// A View whose Agent opened `count` keys Ticks hidden in the Tickhost, now in its Monitor.
async fn view_with_keys_ticks(
    fixture: &PrivateFixture,
    environment: &[(&str, &std::path::Path)],
    count: usize,
    extra_reply: Option<String>,
) -> PtyView {
    let script = fixture.path("work/keys.py");
    std::fs::write(&script, include_str!("../../support/keys_tick.py")).unwrap();
    let mut replies: Vec<_> = (0..count)
        .map(|index| {
            (
                200,
                chat_reply(
                    Some((
                        "ModifyTick",
                        &format!("open-keys-{index}"),
                        json!({"action":"open","command":[support::fixture_cli::tickhost(), "open", "--", "/usr/bin/python3", "-u", script]}),
                    )),
                    "",
                ),
            )
        })
        .collect();
    replies.push((200, chat_reply(None, "Keys Ticks opened.")));
    replies.extend(extra_reply.map(|reply| (200, reply)));
    let (base_url, _requests) = model_http_support::serve_responses(replies).await;
    fixture.set_chat_model(&base_url);
    let mut view =
        PtyView::start_with_environment(fixture, &["--dangerously-skip-permissions"], environment);
    view.wait_for("input ready", new_input_is_visible).await;
    view.send(b"Open the keys Ticks.\r");
    let members = format!("Monitor {count}");
    view.wait_for("the hosted Ticks join Monitor", |frame| {
        frame.contains(&members) && frame.contains("Keys Ticks opened.")
    })
    .await;
    view
}

/// The screen line of the selected hosted row.
fn selected_row(frame: &str) -> Option<usize> {
    frame.lines().position(|line| line.contains("› ◉"))
}

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

/// Notices the Console used to show after a hosted Tick opened or closed; it now returns to the
/// Monitor list quietly.
const RETIRED_NOTICES: [&str; 4] = [
    "Not inside Herdr",
    "keeps running",
    "Herdr popup",
    "Herdr plugin",
];

/// The Monitor list is back without a notice on how the Tick was opened or closed.
fn quietly_back_in_monitor(frame: &str) -> bool {
    frame.contains("enter open") && !RETIRED_NOTICES.iter().any(|notice| frame.contains(notice))
}

/// Waits until the Console has taken the terminal back from a full-screen Tick, whose screen
/// shows its size, and returns the Monitor list, which must carry no such notice.
async fn back_in_monitor_quietly(view: &PtyView) -> String {
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let screen = terminal_screen(view);
            if screen.contains("enter open") && !screen.contains("size 100x32") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the Console never took the terminal back:\n{}",
            terminal_screen(view)
        )
    });
    let frame = view.frame();
    assert!(quietly_back_in_monitor(&frame), "{frame}");
    frame
}

/// A plugin of someone else's, linked before Tick Loop registers its own.
const OTHER_PLUGIN_ID: &str = "test.other";

/// Closes a Herdr popup or full-screen view with a lone Esc.
const CLOSE: &[u8] = b"\x1b";
/// Ctrl+B Esc sends Esc to the Tick.
const SEND_ESCAPE: &[u8] = b"\x02\x1b";

/// Enter on a hosted Tick outside Herdr opens the same process full screen in this terminal at
/// once: the Console stops reading and drawing, keys reach the Tick, Ctrl+B Esc sends Esc, and a
/// lone Esc gives the terminal back to the Monitor list quietly with the same row selected and
/// nothing typed left in the input. `s` opens the text snapshot detail instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_enter_opens_a_hosted_tick_full_screen_outside_herdr_and_esc_closes_it() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let mut view = view_with_keys_ticks(&fixture, &[], 2, None).await;
        view.send(b"\x1b[C");
        let first = view
            .wait_for("Monitor focused", |frame| {
                frame.contains("enter open") && frame.contains("s snapshot")
            })
            .await;
        let first_row = first
            .lines()
            .position(|line| line.contains(" ◉"))
            .expect("a hosted row");
        view.send(if selected_row(&first) == Some(first_row) {
            b"j"
        } else {
            b"k"
        });
        let second = view
            .wait_for("the other row selected", |frame| {
                selected_row(frame).is_some_and(|row| Some(row) != selected_row(&first))
            })
            .await;

        view.send(b"\r");
        terminal_shows(&view, "size 100x32").await;
        view.send(b"z");
        terminal_shows(&view, "keys 7a").await;
        view.send(SEND_ESCAPE);
        terminal_shows(&view, "keys 7a 1b").await;
        view.send(CLOSE);
        let back = back_in_monitor_quietly(&view).await;
        assert_eq!(
            selected_row(&back),
            selected_row(&second),
            "the same row is selected: {back}"
        );

        view.send(b"s");
        view.wait_for("the snapshot detail", |frame| {
            frame.contains("keys 7a 1b") && frame.contains("r refresh")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("back in the Monitor list", |frame| {
            frame.contains("enter open") && !frame.contains("r refresh")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("input focused again", new_input_is_visible)
            .await;
        assert!(
            !view
                .frame()
                .lines()
                .any(|line| line.starts_with('❯') && line.contains('z')),
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

/// A long Agent stream remains live while Enter lends the terminal to the Tick. Closing the
/// attached view restores its reply tail and snapshot without an internal recovery notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_monitor_shows_a_hosted_tick_snapshot_and_attaches_it_full_screen() {
    const THINKING_DELTAS: usize = 8192;
    const ATTACHED_REPLY: &str = "Reply tail received while the Tick owned the terminal.";
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let thinking_delta = format!(
            "data: {}\n\n",
            json!({"id":"cli-fixture","choices":[{"index":0,"delta":{"reasoning_content":"思"},"finish_reason":null}]})
        );
        let stream = thinking_delta.repeat(THINKING_DELTAS) + &chat_reply(None, ATTACHED_REPLY);
        let mut view = view_with_keys_ticks(&fixture, &[], 1, Some(stream)).await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;
        view.send(b"\r");
        terminal_shows(&view, "size 100x32").await;
        view.send(b"z");
        terminal_shows(&view, "keys 7a").await;
        view.send(SEND_ESCAPE);
        terminal_shows(&view, "keys 7a 1b").await;
        let client = fixture.client().await;
        let execution = catalog(&client).await[0].execution;
        wait_snapshot(&client, execution, "the opening turn finished", |snapshot| {
            snapshot.conversation.loops.iter().all(|goal| {
                goal.state == tickloop_agent_protocol::LoopState::Completed
            })
        })
        .await;
        let attached_output_start = view.output.lock().unwrap().len();
        client
            .execute(CommandEnvelope {
                daemon_epoch: client.daemon_epoch(),
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
                matches!(&event.kind, tickloop_agent_protocol::EventKind::Text(text) if text.contains(ATTACHED_REPLY))
            }) && snapshot.conversation.loops.iter().all(|goal| {
                goal.state == tickloop_agent_protocol::LoopState::Completed
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
        view.send(CLOSE);
        back_in_monitor_quietly(&view).await;
        view.send(b"s");
        view.wait_for("the snapshot retains attached input", |frame| {
            frame.contains("keys 7a 1b") && frame.contains("r refresh")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("back in the Monitor list", |frame| {
            frame.contains("enter open") && !frame.contains("r refresh")
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

/// Inside Herdr, Enter on a hosted Tick opens it in Tick Loop's Herdr popup, attached to the same
/// process. The first time, the plugin is offered once for registration (Enter confirms; another
/// plugin already linked is left alone) and the popup opens at once; a lone Esc in the popup
/// closes it while the Tick keeps running. A click on the row is Enter, and while Herdr already
/// shows a popup the View only says so instead of opening the Tick a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_enter_opens_a_hosted_tick_in_a_herdr_popup_after_a_one_key_registration() {
    use support::herdr::{HerdrClient, PrivateHerdr, wait_until};
    let herdr = PrivateHerdr::start();
    let pane = herdr.create_workspace();
    let mut client = HerdrClient::attach(&herdr);
    let fixture = PrivateFixture::new();
    let socket = herdr.socket();
    let result = AssertUnwindSafe(async {
        let other = herdr.link_other_plugin(OTHER_PLUGIN_ID);
        let mut view = view_with_keys_ticks(
            &fixture,
            &[
                ("HERDR_ENV", std::path::Path::new("1")),
                ("HERDR_SOCKET_PATH", &socket),
                ("HERDR_PANE_ID", std::path::Path::new(&pane)),
            ],
            1,
            None,
        )
        .await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;
        view.send(b"\r");
        view.wait_for("the one-key registration offer", |frame| {
            frame.contains("Register Tick Loop") && frame.contains("enter confirm")
        })
        .await;
        assert!(
            herdr.plugins().is_empty(),
            "nothing is registered before Enter"
        );

        view.send(b"\r");
        wait_until("the popup shows the Tick", || {
            Some(client.screen())
                .filter(|screen| screen.contains("┌Tick Loop") && screen.contains("keys"))
        });
        assert_eq!(
            herdr.plugins().len(),
            1,
            "only Tick Loop's plugin is linked"
        );
        let others = herdr.plugins_with_id(OTHER_PLUGIN_ID);
        assert_eq!(
            (
                others.len(),
                others[0]["plugin_root"].as_str(),
                others[0]["enabled"].as_bool()
            ),
            (1, other.to_str(), Some(true)),
            "the other plugin is left as it was: {others:?}"
        );
        let settings = std::fs::read_to_string(fixture.path("config/settings.json")).unwrap();
        assert!(settings.contains("\"monitorPopup\""), "{settings}");
        assert!(settings.contains("\"enabled\""), "{settings}");
        client.send(b"z");
        client.shows("keys 7a");
        client.send(SEND_ESCAPE);
        client.shows("keys 7a 1b");
        assert!(
            quietly_back_in_monitor(&view.frame()),
            "the View says nothing about the popup: {}",
            view.frame()
        );
        client.send(CLOSE);
        wait_until("the popup to close", || {
            Some(()).filter(|()| !client.screen().contains("┌Tick Loop"))
        });

        view.send(b"\r");
        client.shows("┌Tick Loop");
        view.send(b"\r");
        view.wait_for("a busy Herdr is explained", |frame| {
            frame.contains("Herdr is showing another view")
        })
        .await;
        assert!(
            !terminal_screen(&view).contains("size 100x32"),
            "nothing opens full screen while Herdr is busy"
        );
        client.send(CLOSE);
        wait_until("the popup to close", || {
            Some(()).filter(|()| !client.screen().contains("┌Tick Loop"))
        });

        let frame = view.frame();
        let (row, column) = frame
            .lines()
            .enumerate()
            .find_map(|(row, line)| {
                line.contains("› ◉")
                    .then(|| line.chars().position(|character| character == '◉'))
                    .flatten()
                    .map(|column| (row + 1, column + 1))
            })
            .unwrap_or_else(|| panic!("the hosted row is on screen: {frame}"));
        view.send(format!("\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m").as_bytes());
        client.shows("┌Tick Loop");
        client.send(CLOSE);
        wait_until("the popup to close", || {
            Some(()).filter(|()| !client.screen().contains("┌Tick Loop"))
        });
        view.send(b"\x1b");
        view.wait_for("input focused again", new_input_is_visible)
            .await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    drop(client);
    drop(herdr);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Tick Loop's own popup plugin linked by another tickloop build runs that build's attach client,
/// which may not speak this Tickhost's protocol and would close the popup at once. Enter offers to
/// register the plugin for this tickloop instead of floating the Tick into the other one, and once
/// confirmed the popup shows the Tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_enter_re_registers_a_popup_plugin_another_tickloop_linked() {
    use std::os::unix::fs::PermissionsExt;
    use support::herdr::{HerdrClient, PLUGIN_ID, PrivateHerdr, wait_until};
    let herdr = PrivateHerdr::start();
    let pane = herdr.create_workspace();
    let mut client = HerdrClient::attach(&herdr);
    let fixture = PrivateFixture::new();
    let socket = herdr.socket();
    let result = AssertUnwindSafe(async {
        let other_tickloop = fixture.path("other-build/tickloop");
        std::fs::create_dir_all(other_tickloop.parent().unwrap()).unwrap();
        std::fs::write(
            &other_tickloop,
            "#!/bin/sh\necho 'Error: IncompatibleHost' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&other_tickloop, std::fs::Permissions::from_mode(0o700)).unwrap();
        let other_tickloop = other_tickloop.canonicalize().unwrap();
        let plugin = fixture
            .path("config")
            .canonicalize()
            .unwrap()
            .join("herdr-popup-plugin");
        std::fs::create_dir(&plugin).unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            plugin.join("herdr-plugin.toml"),
            format!(
                "id = \"{PLUGIN_ID}\"\nname = \"Tick Loop Monitor\"\nversion = \"0.1.0\"\nmin_herdr_version = \"0.9.0\"\nplatforms = [\"macos\", \"linux\"]\n\n[[panes]]\nid = \"attach\"\ntitle = \"Tick Loop\"\nplacement = \"popup\"\nwidth = \"85%\"\nheight = \"85%\"\ncommand = [{:?}, \"monitor\", \"attach\"]\n",
                other_tickloop.to_str().unwrap()
            ),
        )
        .unwrap();
        herdr
            .request("plugin.link", json!({"path": plugin, "enabled": true}))
            .unwrap();
        let settings = fixture.path("config/settings.json");
        let mut enabled = std::fs::read(&settings)
            .map(|saved| serde_json::from_slice::<serde_json::Value>(&saved).unwrap())
            .unwrap_or_else(|_| json!({}));
        enabled["monitorPopup"] = json!("enabled");
        std::fs::write(&settings, enabled.to_string()).unwrap();
        let mut view = view_with_keys_ticks(
            &fixture,
            &[
                ("HERDR_ENV", std::path::Path::new("1")),
                ("HERDR_SOCKET_PATH", &socket),
                ("HERDR_PANE_ID", std::path::Path::new(&pane)),
            ],
            1,
            None,
        )
        .await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;
        view.send(b"\r");
        view.wait_for("the offer to register this tickloop", |frame| {
            frame.contains("runs another tickloop") && frame.contains("enter confirm")
        })
        .await;
        assert_eq!(
            herdr.plugins()[0]["panes"][0]["command"][0],
            json!(other_tickloop),
            "nothing is re-registered before Enter"
        );

        view.send(b"\r");
        wait_until("the popup shows the Tick", || {
            Some(client.screen())
                .filter(|screen| screen.contains("┌Tick Loop") && screen.contains("keys"))
        });
        let plugins = herdr.plugins();
        assert_eq!(plugins.len(), 1, "{plugins:?}");
        assert_eq!(plugins[0]["plugin_root"], json!(plugin));
        assert_ne!(
            plugins[0]["panes"][0]["command"][0],
            json!(other_tickloop),
            "the popup runs this tickloop now: {plugins:?}"
        );
        client.send(CLOSE);
        wait_until("the popup to close", || {
            Some(()).filter(|()| !client.screen().contains("┌Tick Loop"))
        });
        view.send(b"\x1b");
        view.wait_for("input focused again", new_input_is_visible)
            .await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    drop(client);
    drop(herdr);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Declining the one-time registration offer with Esc links nothing and is not asked again: the
/// next Enter opens the Tick full screen and returns quietly. With another plugin holding Tick
/// Loop's popup id, Enter opens full screen too and leaves that plugin linked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_enter_without_a_usable_popup_opens_full_screen() {
    use support::herdr::PrivateHerdr;
    let herdr = PrivateHerdr::start();
    let pane = herdr.create_workspace();
    let fixture = PrivateFixture::new();
    let socket = herdr.socket();
    let result = AssertUnwindSafe(async {
        let mut view = view_with_keys_ticks(
            &fixture,
            &[
                ("HERDR_ENV", std::path::Path::new("1")),
                ("HERDR_SOCKET_PATH", &socket),
                ("HERDR_PANE_ID", std::path::Path::new(&pane)),
            ],
            1,
            None,
        )
        .await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;
        view.send(b"\r");
        view.wait_for("the one-key registration offer", |frame| {
            frame.contains("Register Tick Loop")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("the offer declined", |frame| {
            !frame.contains("Register Tick Loop") && frame.contains("enter open")
        })
        .await;
        assert!(herdr.plugins().is_empty(), "declining links nothing");

        view.send(b"\r");
        terminal_shows(&view, "size 100x32").await;
        view.send(CLOSE);
        let back = back_in_monitor_quietly(&view).await;
        assert!(
            !back.contains("Register Tick Loop"),
            "the offer is made once: {back}"
        );
        assert!(
            herdr.plugins().is_empty(),
            "nothing is linked without consent"
        );

        let foreign = herdr.link_foreign_plugin();
        view.send(b"\r");
        terminal_shows(&view, "size 100x32").await;
        view.send(CLOSE);
        back_in_monitor_quietly(&view).await;
        let plugins = herdr.plugins();
        assert_eq!(
            (plugins.len(), plugins[0]["plugin_root"].as_str()),
            (1, foreign.to_str()),
            "the foreign plugin is left linked: {plugins:?}"
        );
        view.send(b"\x1b");
        view.wait_for("input focused again", new_input_is_visible)
            .await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    drop(herdr);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
