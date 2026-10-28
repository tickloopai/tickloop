use super::*;
use tickloop_code_access::AgentAccess;
use tickloop_harness::test_support::kitty_keyboard::{DISAMBIGUATE_ESCAPE_CODES, KittyKeyboard};
use tickloop_harness::test_support::screen::SELECTED_MARK;

/// A View whose Agent opened `count` keys Ticks hidden in the Tickhost, now in its Monitor.
async fn view_with_keys_ticks(
    fixture: &PrivateFixture,
    environment: &[(&str, &std::path::Path)],
    count: usize,
    extra_reply: Option<String>,
) -> PtyView {
    view_with_keys_ticks_progress(fixture, environment, count, extra_reply, None).await
}

async fn view_with_keys_ticks_progress(
    fixture: &PrivateFixture,
    environment: &[(&str, &std::path::Path)],
    count: usize,
    extra_reply: Option<String>,
    progress: Option<model_http_support::ResponseProgress>,
) -> PtyView {
    let script = fixture.path("work/keys.py");
    std::fs::write(&script, include_str!("../../support/keys_tick.py")).unwrap();
    let mut replies: Vec<_> = (0..count)
        .map(|index| {
            (
                200,
                chat_reply(
                    Some((
                        "TickManage",
                        &format!("open-keys-{index}"),
                        json!({"action":"open","command":["/usr/bin/python3", "-u", script],"protocol":true}),
                    )),
                    "",
                ),
            )
        })
        .collect();
    replies.push((200, chat_reply(None, "Keys Ticks opened.")));
    replies.extend(extra_reply.map(|reply| (200, reply)));
    let (base_url, _requests) = match progress {
        Some(progress) => {
            model_http_support::serve_responses_with_progress(replies, progress).await
        }
        None => model_http_support::serve_responses(replies).await,
    };
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

/// Leaves an entered card, or the Monitor list, with a lone Esc.
const CLOSE: &[u8] = b"\x1b";

/// How many Monitor cards show a keys Tick's screen; each card is 40 columns wide.
fn keys_cards(frame: &str) -> usize {
    frame
        .lines()
        .filter(|line| monitor_part(line).is_some_and(|part| part.contains("size 40x")))
        .count()
}

/// The Monitor lines of the card that shows `keys`, the bytes its Tick received.
fn card_shows(frame: &str, keys: &str) -> bool {
    frame
        .lines()
        .any(|line| monitor_part(line).is_some_and(|part| part.trim_end().ends_with(keys)))
}

/// One SGR left click at 1-based `column` and `row`, as the terminal reports it.
fn click(column: usize, row: usize) -> Vec<u8> {
    format!("\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m").into_bytes()
}

/// The 1-based screen position of the Monitor text `needle`.
fn monitor_position(frame: &str, needle: &str) -> (usize, usize) {
    frame
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            let (left, part) = line.split_once('│')?;
            let column = part.find(needle)?;
            Some((
                left.chars().count() + 1 + part[..column].chars().count() + 1,
                row + 1,
            ))
        })
        .unwrap_or_else(|| panic!("no Monitor text {needle:?}: {frame}"))
}

/// Live Tick screens open as cards under their Monitor rows. Space shows and hides a card with
/// the focus on the list, and several stay open; Enter enters one, so typed keys, Ctrl+C included,
/// reach that Tick alone; a lone Esc leaves with the card still shown; Esc Esc sends one Esc. A
/// click on a row toggles its card, a click in a card enters it and a click outside leaves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_monitor_cards_show_live_ticks_and_take_keys_while_entered() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let mut view = view_with_keys_ticks(&fixture, &[], 2, None).await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("space show"))
            .await;
        view.send(b" ");
        view.wait_for("one card shows its Tick", |frame| keys_cards(frame) == 1)
            .await;
        view.send(b"j");
        view.wait_for("the other row selected", |frame| {
            frame.contains("space show")
        })
        .await;
        view.send(b" ");
        let both = view
            .wait_for("both cards show", |frame| keys_cards(frame) == 2)
            .await;
        assert!(
            !card_focused(&both),
            "Space leaves the focus on the list: {both}"
        );

        view.send(b"\r");
        view.wait_for("the card has the focus", card_focused).await;
        view.send(b"z\x03\x02");
        view.wait_for("the keys reach the entered Tick alone", |frame| {
            card_shows(frame, "keys 7a 03 02") && keys_cards(frame) == 2
        })
        .await;
        view.send(b"\x1b");
        let left = view
            .wait_for("a lone Esc leaves the card", |frame| {
                !card_focused(frame) && frame.contains("space hide")
            })
            .await;
        assert_eq!(keys_cards(&left), 2, "the card stays shown: {left}");
        assert!(
            !left
                .lines()
                .any(|line| line.starts_with('❯') && line.contains('z')),
            "nothing typed reached the Console: {left}"
        );

        view.send(b"\r");
        view.wait_for("entered again", card_focused).await;
        view.send(b"\x1b\x1b");
        view.wait_for("Esc Esc sends one Esc", |frame| {
            card_shows(frame, "keys 7a 03 02 1b") && card_focused(frame)
        })
        .await;
        let frame = view.frame();
        view.send(&click(2, 2));
        view.wait_for("a click outside leaves the card", |frame| {
            !card_focused(frame)
        })
        .await;

        let (column, row) = monitor_position(&frame, "keys 7a 03 02 1b");
        view.send(&click(column, row));
        view.wait_for("a click in a card enters it", card_focused)
            .await;
        view.send(b"y");
        view.wait_for("the clicked card takes the keys", |frame| {
            card_shows(frame, "keys 7a 03 02 1b 79")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("left again", |frame| !card_focused(frame))
            .await;

        view.send(b" ");
        view.wait_for("Space hides the selected card", |frame| {
            keys_cards(frame) == 1
        })
        .await;
        let frame = view.frame();
        let (column, row) = monitor_position(&frame, "size 40x");
        view.send(&click(column, row));
        view.wait_for("a click in the shown card enters it", card_focused)
            .await;
        let frame = view.frame();
        let hidden_row = frame
            .lines()
            .position(|line| {
                monitor_part(line)
                    .is_some_and(|part| part.contains("Keys") && !part.contains(SELECTED_MARK))
            })
            .unwrap_or_else(|| panic!("the hidden card's row: {frame}"));
        let column = frame
            .lines()
            .nth(hidden_row)
            .unwrap()
            .split_once('│')
            .unwrap()
            .0
            .chars()
            .count()
            + 4;
        view.send(&click(column, hidden_row + 1));
        view.wait_for(
            "a click on another row leaves the focused card and shows that row's card",
            |frame| !card_focused(frame) && keys_cards(frame) == 2,
        )
        .await;
        let frame = view.frame();
        let (column, row) = monitor_position(&frame, "Keys");
        view.send(&click(column, row));
        view.wait_for("a click on the row toggles its card", |frame| {
            keys_cards(frame) == 1
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("input focused again", input_focused).await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// The hosted Ticks of `execution`'s Chat, as its Monitor probes them.
async fn hosted_members(
    client: &LocalAgentClient,
    execution: ExecutionRef,
) -> Vec<tickloop_harness::protocol::HostedMember> {
    let mut last_error = None;
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let result = client
                .execute(CommandEnvelope {
                    daemon_epoch: client.daemon_epoch(),
                    command_id: uuid::Uuid::new_v4(),
                    command: AgentCommand::ListHostedTicks { execution },
                })
                .await
                .unwrap()
                .into_result();
            match result {
                Ok(CommandResult::HostedTicks(members)) => break members,
                // The last Tick ending can close the host while a read-only listing is in flight.
                Err(error) if error.code == tickloop_code_access::AccessErrorCode::Unavailable => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                result => panic!("hosted Ticks expected: {result:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("hosted Ticks stayed unavailable: {last_error:?}"))
}

/// How many hosted Tick launch records the fixture's Chats keep.
fn launch_record_count(fixture: &PrivateFixture) -> usize {
    let Ok(chats) = std::fs::read_dir(fixture.path("config/tickhost/chats")) else {
        return 0;
    };
    chats
        .filter_map(Result::ok)
        .filter_map(|chat| std::fs::read_dir(chat.path()).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("launch-"))
        .count()
}

/// The selected Monitor row offers to reopen a hosted Tick that no longer runs.
fn selected_row_is_gone(frame: &str) -> bool {
    frame
        .lines()
        .any(|line| line.starts_with("enter reopen · x remove"))
}

/// The screen lines of the hosted Tick rows in Monitor.
fn hosted_rows(frame: &str) -> Vec<usize> {
    frame
        .lines()
        .enumerate()
        .filter(|(_, line)| monitor_part(line).is_some_and(|part| part.contains('◉')))
        .map(|(row, _)| row)
        .collect()
}

/// The last fully committed Monitor screen, excluding an in-flight repaint.
fn completed_monitor_frame(view: &PtyView) -> Option<String> {
    let output = view.output.lock().unwrap();
    let ansi = String::from_utf8_lossy(&output);
    let end = ansi.rfind(COMPLETED_FRAME_END)? + COMPLETED_FRAME_END.len();
    Some(screen::visible_screen_with_selection(
        &ansi[..end],
        view.width,
        view.height,
    ))
}

/// Selects a hosted row by subscription identity and confirms its action in the same frame.
/// A reopen can replace and reorder rows after its process is already running.
async fn select_committed_hosted_state(view: &mut PtyView, subscribed: bool, gone: bool) -> String {
    let mut latest = String::new();
    let mut pending_from = None;
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let Some(frame) = completed_monitor_frame(view) {
                let is_gone = selected_row_is_gone(&frame);
                let is_running = frame
                    .lines()
                    .any(|line| line.starts_with("enter open · x close"));
                if is_gone || is_running {
                    let target = hosted_rows(&frame).into_iter().find(|row| {
                        frame
                            .lines()
                            .nth(*row)
                            .is_some_and(|line| line.contains('↻') == subscribed)
                    });
                    if let (Some(selected), Some(target)) = (selected_line(&frame), target) {
                        if selected == target && if gone { is_gone } else { is_running } {
                            return frame;
                        }
                        if pending_from.is_some_and(|previous| previous != selected) {
                            pending_from = None;
                        }
                        if selected != target && pending_from.is_none() {
                            view.send(if target > selected { b"j" } else { b"k" });
                            pending_from = Some(selected);
                        }
                    }
                }
                latest = frame;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("Monitor did not select subscribed={subscribed}, gone={gone}: {latest}")
    })
}

/// The Tick Subscriptions of `execution`'s Chat.
async fn tick_subscriptions(
    client: &LocalAgentClient,
    execution: ExecutionRef,
) -> Vec<tickloop_harness::protocol::TickSubscriptionStatus> {
    let observation = client
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Subscriptions,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Subscriptions(rows) = observation.initial else {
        panic!("expected Tick Subscriptions");
    };
    rows
}

/// Polls `probe` until it yields a value, for at most `UI_TIMEOUT`.
async fn eventually<T, F: std::future::Future<Output = Option<T>>>(
    what: &str,
    mut probe: impl FnMut() -> F,
) -> T {
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let Some(value) = probe().await {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never happened"))
}

/// How often the Console probes its hosted Ticks. That an ended Tick's row stays however long
/// ago it ended is `monitor_rows::builtin`'s `ended_ticks_stay_listed_until_the_user_removes_them`;
/// here it outlasts the Console's next probe.
const MEMBER_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// An extension Tick whose process ended by itself keeps its Monitor row, and its launch record,
/// until its user removes it, across the Console's later probes and refreshes.
/// Enter reopens it in its row's place from that record directly, restoring its
/// subscription by default; `x` takes a gone row out with its record at once, and closes a
/// reopened one, with the record it was reopened from and its subscription, after confirmation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_an_ended_protocol_tick_stays_until_removed_and_enter_reopens_it() {
    use tickloop_harness::protocol::{HostedMemberState, TickSubscriptionFrequency};
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let mut view = view_with_keys_ticks(&fixture, &[], 2, None).await;
        let client = fixture.client().await;
        let execution = catalog(&client).await[0].execution;
        let members = hosted_members(&client, execution).await;
        assert_eq!(members.len(), 2, "{members:?}");
        assert_eq!(launch_record_count(&fixture), 2);

        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;
        view.send(b"s");
        let subscribed = eventually("the subscription", || async {
            tick_subscriptions(&client, execution).await.pop()
        })
        .await;
        assert_eq!(subscribed.frequency, TickSubscriptionFrequency::High);

        let first_pids = members
            .iter()
            .map(|member| {
                member
                    .resources
                    .as_ref()
                    .expect("a process sample")
                    .business_pid
            })
            .collect::<Vec<_>>();
        for pid in &first_pids {
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
        }
        eventually("both Ticks ending", || async {
            hosted_members(&client, execution)
                .await
                .is_empty()
                .then_some(())
        })
        .await;
        view.wait_for("the selected row turns gone", selected_row_is_gone)
            .await;
        tokio::time::sleep(MEMBER_PROBE_INTERVAL + Duration::from_secs(1)).await;
        let later = view.frame();
        assert!(
            later.contains("Monitor 2") && selected_row_is_gone(&later),
            "both gone rows stay: {later}"
        );
        let other = hosted_rows(&later)
            .into_iter()
            .find(|row| Some(*row) != selected_line(&later))
            .expect("the other hosted row");
        select_line(&mut view, other).await;
        view.wait_for("the other row is gone too", selected_row_is_gone)
            .await;
        assert_eq!(
            launch_record_count(&fixture),
            2,
            "a Tick that ended by itself keeps its launch record"
        );

        select_row(&mut view, "↻").await;
        view.send(b"\r");
        let reopened = eventually("the reopened Tick running", || async {
            hosted_members(&client, execution)
                .await
                .into_iter()
                .find(|member| member.state == HostedMemberState::Running)
        })
        .await;
        let reopened_pid = reopened.resources.as_ref().unwrap().business_pid;
        assert!(
            !view.frame().contains("Reopened hosted:"),
            "{}",
            view.frame()
        );
        assert!(!first_pids.contains(&reopened_pid), "a new process");
        assert_ne!(reopened.instance, subscribed.instance, "a new instance");
        let restored = eventually("the restored subscription", || async {
            tick_subscriptions(&client, execution)
                .await
                .pop()
                .filter(|row| row.instance == reopened.instance)
        })
        .await;
        assert_eq!(restored.frequency, TickSubscriptionFrequency::High);
        assert_eq!(
            launch_record_count(&fixture),
            3,
            "the reopened instance's record joins the one it came from and the other gone Tick's"
        );

        select_committed_hosted_state(&mut view, true, false).await;
        let selected_gone = select_committed_hosted_state(&mut view, false, true).await;
        assert!(selected_row_is_gone(&selected_gone), "{selected_gone}");
        view.send(b"x");
        view.wait_for("the gone row needs confirmation", |frame| {
            frame.contains("Press x again to remove")
        })
        .await;
        assert!(view.frame().contains("Monitor 2"));
        view.send(b"x");
        view.wait_for("the confirmed gone row leaves", |frame| {
            frame.contains("Monitor 1") && frame.contains("enter open · x close")
        })
        .await;
        assert_eq!(
            launch_record_count(&fixture),
            2,
            "its launch record goes with the row"
        );

        view.send(b"x");
        view.wait_for("closing needs confirmation", |frame| {
            frame.contains("Press x again to close")
        })
        .await;
        assert_eq!(hosted_members(&client, execution).await.len(), 1);
        view.send(b"x");
        view.wait_for("the reopened row leaves", |frame| {
            !frame.contains("Monitor 1") && !frame.contains("◉ Keys")
        })
        .await;
        assert_eq!(
            launch_record_count(&fixture),
            0,
            "closing the reopened Tick forgets the record it was reopened from too"
        );
        assert!(!view.frame().contains("Tick closed"), "{}", view.frame());
        assert!(hosted_members(&client, execution).await.is_empty());
        assert!(tick_subscriptions(&client, execution).await.is_empty());
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// A long Agent stream keeps streaming into the conversation while a card has the focus; the
/// keys typed meanwhile reach the Tick, never the Console, and no internal recovery shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_keeps_a_long_reply_streaming_while_a_card_has_the_focus() {
    const THINKING_DELTAS: usize = 8192;
    const ATTACHED_REPLY: &str = "Reply tail received while a card had the focus.";
    let fixture = PrivateFixture::new();
    let progress = model_http_support::ResponseProgress::default();
    let result = AssertUnwindSafe(async {
        let thinking_delta = format!(
            "data: {}\n\n",
            json!({"id":"cli-fixture","choices":[{"index":0,"delta":{"reasoning_content":"思"},"finish_reason":null}]})
        );
        let stream = thinking_delta.repeat(THINKING_DELTAS) + &chat_reply(None, ATTACHED_REPLY);
        let mut view = view_with_keys_ticks_progress(&fixture, &[], 1, Some(stream), Some(progress.clone())).await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;
        view.send(b"\r");
        view.wait_for("the card has the focus", card_focused).await;
        view.send(b"z");
        view.wait_for("the key reaches the Tick", |frame| {
            card_shows(frame, "keys 7a")
        })
        .await;
        let client = fixture.client().await;
        let execution = catalog(&client).await[0].execution;
        wait_snapshot(&client, execution, "the opening turn finished", |snapshot| {
            snapshot.conversation.chat_loop.state == tickloop_harness::protocol::LoopState::Idle
        })
        .await;
        client
            .execute(CommandEnvelope {
                daemon_epoch: client.daemon_epoch(),
                command_id: uuid::Uuid::new_v4(),
                command: AgentCommand::Submit {
                    execution,
                    input: UserInput::Text {
                        text: "Think while a card has the focus.".into(),
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
                matches!(&event.kind, tickloop_harness::protocol::EventKind::Text(text) if text.contains(ATTACHED_REPLY))
            }) && snapshot.conversation.chat_loop.state == tickloop_harness::protocol::LoopState::Idle
        })
        .await;
        view.wait_for("the reply streams beside the focused card", |frame| {
            frame.contains(ATTACHED_REPLY) && card_focused(frame) && card_shows(frame, "keys 7a")
        })
        .await;
        view.send(CLOSE);
        view.wait_for("a lone Esc leaves the card", |frame| !card_focused(frame))
            .await;
        view.send(b"\x1b");
        view.wait_for("input focused again", input_focused)
            .await;
        assert!(
            !String::from_utf8_lossy(&view.output.lock().unwrap()).contains("The view fell behind"),
            "internal recovery must remain invisible while a card has the focus"
        );
        assert!(
            !view.frame().lines().any(|line| line.starts_with('❯') && line.contains('z')),
            "nothing typed into the card reached the Console: {}",
            view.frame()
        );
        view.close().await;
    })
    .catch_unwind()
    .await;
    if result.is_err() {
        eprintln!("Attached stream response progress: {}", progress.summary());
    }
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// A background Shell's script: one line, then another once `gate` exists in its directory.
const SHELL_VIEW_SCRIPT: &str =
    "printf 'out%s\\n' one\nwhile [ ! -e gate ]; do sleep 0.05; done\nprintf 'out%s\\n' two\n";
const SHELL_VIEW_COMMAND: &str = "sh shellview.sh";
const SHELL_VIEW_DESCRIPTION: &str = "Watch the shell view";
/// A background Shell that runs until it is stopped.
const LONG_SHELL_COMMAND: &str = "sh sleeper.sh";
const LOCAL_TICKS_OPENED: &str = "Local Ticks opened.";

/// A View whose Agent opened an extension Tick, a terminal Tick and a background Shell, and with
/// `long_shell` a second Shell that runs until it is stopped, all in its Monitor.
async fn view_with_local_ticks(
    fixture: &PrivateFixture,
    environment: &[(&str, &std::path::Path)],
    long_shell: bool,
) -> PtyView {
    let keys = fixture.path("work/keys.py");
    std::fs::write(&keys, include_str!("../../support/keys_tick.py")).unwrap();
    std::fs::write(fixture.path("work/shellview.sh"), SHELL_VIEW_SCRIPT).unwrap();
    std::fs::write(fixture.path("work/sleeper.sh"), "sleep 600\n").unwrap();
    let mut calls = vec![
        (
            "TickManage",
            "open-keys",
            json!({"action":"open","command":["/usr/bin/python3", "-u", keys],"protocol":true}),
        ),
        (
            "TickManage",
            "open-cat",
            json!({"action":"open","command":["/bin/cat"]}),
        ),
        (
            "Bash",
            "shell-view",
            json!({"command":SHELL_VIEW_COMMAND,"description":SHELL_VIEW_DESCRIPTION,"run_in_background":true}),
        ),
    ];
    if long_shell {
        calls.push((
            "Bash",
            "long-shell",
            json!({"command":LONG_SHELL_COMMAND,"description":"Sleep until stopped","run_in_background":true}),
        ));
    }
    let rows = calls.len();
    let mut replies = calls
        .into_iter()
        .map(|(tool, id, input)| (200, chat_reply(Some((tool, id, input)), "")))
        .collect::<Vec<_>>();
    replies.push((200, chat_reply(None, LOCAL_TICKS_OPENED)));
    // The turns the Shells' ends start.
    replies.extend((0..2).map(|_| (200, chat_reply(None, "Noted."))));
    let progress = model_http_support::ResponseProgress::default();
    let (base_url, _requests) =
        model_http_support::serve_responses_with_progress(replies, progress.clone()).await;
    fixture.set_chat_model(&base_url);
    let mut view =
        PtyView::start_with_environment(fixture, &["--dangerously-skip-permissions"], environment);
    view.wait_for("input ready", new_input_is_visible).await;
    view.send(b"Open the local Ticks.\r");
    let members = format!("Monitor {rows}");
    let ready = AssertUnwindSafe(view.wait_for("the local Ticks join Monitor", |frame| {
        frame.contains(&members) && frame.contains(LOCAL_TICKS_OPENED)
    }))
    .catch_unwind()
    .await;
    if let Err(panic) = ready {
        eprintln!("Local Tick setup response progress: {}", progress.summary());
        let observed = tokio::time::timeout(
            Duration::from_secs(1),
            AssertUnwindSafe(async {
                let client = fixture.client().await;
                let rows = catalog(&client).await;
                if let Some(row) = rows.first() {
                    let state = snapshot(&client, row.execution).await;
                    snapshot_diagnostic(&state)
                } else {
                    "no Agent execution was registered".into()
                }
            })
            .catch_unwind(),
        )
        .await;
        match observed {
            Ok(Ok(state)) => eprintln!("Local Tick setup Agent state: {state}"),
            Ok(Err(_)) => eprintln!("Local Tick setup Agent state could not be read"),
            Err(_) => eprintln!("Local Tick setup Agent state query exceeded 1s"),
        }
        std::panic::resume_unwind(panic);
    }
    view
}

/// The input has the focus again: its prompt and the status bar show and no Monitor row offers
/// its keys. Unlike the welcome header, none of these scroll away as the conversation grows.
fn input_focused(frame: &str) -> bool {
    frame.contains("⏵⏵ ")
        && frame.lines().any(|line| line.starts_with('❯'))
        && !frame.lines().any(|line| line.starts_with("enter open"))
}

/// The key hints of the selected Monitor row.
fn monitor_hint(frame: &str) -> &str {
    frame
        .lines()
        .rfind(|line| line.starts_with("enter open"))
        .unwrap_or_default()
}

/// The part of a screen line in the Monitor pane, right of its border.
fn monitor_part(line: &str) -> Option<&str> {
    line.split_once('│').map(|(_, part)| part)
}

/// The screen line of the selected Monitor row.
fn selected_line(frame: &str) -> Option<usize> {
    frame
        .lines()
        .position(|line| monitor_part(line).is_some_and(|part| part.contains(SELECTED_MARK)))
}

/// Moves the Monitor selection to the row on screen line `target` one key at a time, sending each
/// only once the last one moved the selection, so no key is still on its way when it returns.
async fn select_line(view: &mut PtyView, target: usize) {
    loop {
        let frame = view.frame();
        let selected =
            selected_line(&frame).unwrap_or_else(|| panic!("no Monitor row is selected: {frame}"));
        if selected == target {
            return;
        }
        view.send(if target > selected { b"j" } else { b"k" });
        view.wait_for("the Monitor selection to move", |frame| {
            selected_line(frame) != Some(selected)
        })
        .await;
    }
}

/// Moves the Monitor selection to the row that names `needle`.
async fn select_row(view: &mut PtyView, needle: &str) {
    let frame = view.frame();
    let target = frame
        .lines()
        .position(|line| monitor_part(line).is_some_and(|part| part.contains(needle)))
        .unwrap_or_else(|| panic!("no Monitor row names {needle:?}: {frame}"));
    select_line(view, target).await;
}

/// Whether a Monitor line shows `text`, such as a card's screen line.
fn monitor_shows(frame: &str, text: &str) -> bool {
    frame
        .lines()
        .any(|line| monitor_part(line).is_some_and(|part| part.contains(text)))
}

/// A terminal Tick and a Shell show in cards too: the terminal Tick takes the keys typed into
/// its entered card; the Shell's card shows its status over its live output, takes no input and
/// shows how it ended. A lone Esc leaves each card while it keeps running. A running Shell needs
/// two presses to stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_cards_show_terminal_ticks_and_shells_and_a_row_stops_a_shell() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let mut view = view_with_local_ticks(&fixture, &[], true).await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;

        select_row(&mut view, "cat").await;
        view.send(b"\r");
        view.wait_for("the cat card has the focus", card_focused)
            .await;
        view.send(b"hello cat\r");
        view.wait_for("cat echoes in its card", |frame| {
            monitor_shows(frame, "hello cat")
        })
        .await;
        view.send(CLOSE);
        view.wait_for("a lone Esc leaves the cat card", |frame| {
            !card_focused(frame) && monitor_shows(frame, "hello cat")
        })
        .await;

        select_row(&mut view, SHELL_VIEW_COMMAND).await;
        view.send(b"\r");
        view.wait_for("the Shell card shows its output", |frame| {
            card_focused(frame) && monitor_shows(frame, "outone") && monitor_shows(frame, "running")
        })
        .await;
        view.send(b"typed into the view");
        std::fs::write(fixture.path("work/gate"), "").unwrap();
        let ended = view
            .wait_for("the Shell card shows how it ended", |frame| {
                monitor_shows(frame, "outtwo") && monitor_shows(frame, "exit 0")
            })
            .await;
        assert!(!ended.contains("typed into the view"), "{ended}");
        view.send(CLOSE);
        view.wait_for("the ended Shell keeps its row and card", |frame| {
            !card_focused(frame) && frame.contains(SHELL_VIEW_COMMAND) && frame.contains("cat")
        })
        .await;

        select_row(&mut view, LONG_SHELL_COMMAND).await;
        view.wait_for("the running Shell's row offers stop", |frame| {
            monitor_hint(frame).contains("x stop")
        })
        .await;
        view.send(b"x");
        view.wait_for("stopping needs confirmation", |frame| {
            frame.contains("Press x again to stop this Shell")
        })
        .await;
        assert!(
            view.frame()
                .lines()
                .any(|line| line.starts_with("Press x again to stop this Shell"))
        );
        view.send(b"x");
        view.wait_for("the stopped Shell shows how it ended", |frame| {
            frame.lines().any(|line| {
                line.split_once(SELECTED_MARK).is_some_and(|(_, row)| {
                    row.contains(LONG_SHELL_COMMAND) && row.contains("SIGTERM")
                })
            }) && !monitor_hint(frame).contains("x stop")
        })
        .await;
        assert!(!view.frame().contains("Shell stopped"), "{}", view.frame());
        assert!(!input_focused(&view.frame()), "{}", view.frame());
        view.send(b"\x1b");
        view.wait_for("input focused again", input_focused).await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// A card whose Tick ends keeps its last screen, says how the Tick ended on its rule and gives
/// the focus back to the list: a terminal Tick whose program exited tells its exit status, an
/// extension Tick whose process died says it ended. Space still hides the ended Tick's card.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_a_card_whose_tick_ends_keeps_its_last_screen_and_says_how() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let mut view = view_with_local_ticks(&fixture, &[], false).await;
        let client = fixture.client().await;
        let execution = catalog(&client).await[0].execution;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;

        select_row(&mut view, "cat").await;
        view.send(b"\r");
        view.wait_for("the cat card has the focus", card_focused)
            .await;
        view.send(b"last words\r");
        view.wait_for("cat echoes in its card", |frame| {
            monitor_shows(frame, "last words")
        })
        .await;
        view.send(b"\x04");
        let exited = view
            .wait_for("the cat card says how its program ended", |frame| {
                monitor_shows(frame, " exited 0 ")
            })
            .await;
        assert!(
            monitor_shows(&exited, "last words"),
            "the last screen stays: {exited}"
        );
        assert!(
            !card_focused(&exited),
            "the focus leaves the ended card: {exited}"
        );

        let keys = hosted_members(&client, execution)
            .await
            .into_iter()
            .find(|member| !member.is_terminal)
            .expect("the keys Tick");
        select_row(&mut view, "Keys").await;
        view.send(b"\r");
        view.wait_for("the keys card has the focus", |frame| {
            card_focused(frame) && monitor_shows(frame, "size ")
        })
        .await;
        let pid = keys
            .resources
            .as_ref()
            .expect("a process sample")
            .business_pid;
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let ended = view
            .wait_for("the keys card says its Tick ended", |frame| {
                monitor_shows(frame, " ended ")
                    && frame
                        .lines()
                        .any(|line| line.starts_with("enter reopen") && line.contains("space hide"))
            })
            .await;
        assert!(
            monitor_shows(&ended, "size "),
            "the last screen stays: {ended}"
        );
        assert!(!card_focused(&ended), "{ended}");
        view.send(b" ");
        view.wait_for("Space hides the ended Tick's card", |frame| {
            !monitor_shows(frame, " ended ")
        })
        .await;
        view.send(CLOSE);
        view.wait_for("input focused again", input_focused).await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// The kitty keyboard flags of a terminal that showed everything the View wrote, kept for each
/// screen across screen switches as kitty and Ghostty keep them.
fn kitty_keyboard(view: &PtyView) -> KittyKeyboard {
    KittyKeyboard::after(&view.output.lock().unwrap())
}

/// In a terminal speaking the kitty keyboard protocol, every entered card, of a protocol Tick, a
/// terminal Tick and a Shell alike, reads legacy keys and leaves on Esc each time it is entered,
/// and Esc Esc sends Esc. The Console gets its disambiguated keys back on its own screen, pushed
/// once however often a card was entered, with the main screen left untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_entered_cards_read_legacy_keys_in_a_terminal_speaking_the_kitty_keyboard_protocol()
 {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let mut view = view_with_local_ticks(&fixture, &[], false).await;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open"))
            .await;
        for (row, typed, shown) in [
            ("Keys", None, "size 40x"),
            ("cat", Some(b"meow\r".as_slice()), "meow"),
            (SHELL_VIEW_COMMAND, None, "outone"),
        ] {
            for _ in 0..2 {
                select_row(&mut view, row).await;
                view.send(b"\r");
                view.wait_for("the card has the focus", card_focused).await;
                if let Some(typed) = typed {
                    view.send(typed);
                }
                view.wait_for("the card shows its Tick", |frame| {
                    monitor_shows(frame, shown)
                })
                .await;
                let in_card = kitty_keyboard(&view);
                assert_eq!(in_card.flags(), 0, "the {row} card reads legacy keys");
                if row == "Keys" {
                    view.send(&[in_card.escape(), in_card.escape()].concat());
                    view.wait_for("Esc Esc reaches the Tick as one Esc", |frame| {
                        monitor_shows(frame, "keys 1b") && card_focused(frame)
                    })
                    .await;
                }
                view.send(in_card.escape());
                view.wait_for("a lone Esc leaves the card", |frame| !card_focused(frame))
                    .await;
                let in_console = kitty_keyboard(&view);
                assert_eq!(
                    in_console.alternate(),
                    [DISAMBIGUATE_ESCAPE_CODES],
                    "the Console's flags are back on its screen, pushed once"
                );
                assert!(in_console.main().is_empty(), "{in_console:?}");
            }
        }
        let in_console = kitty_keyboard(&view);
        assert_eq!(in_console.escape(), b"\x1b[27u");
        view.send(in_console.escape());
        view.wait_for("input focused again", input_focused).await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_s_keeps_a_terminal_update_pending_while_changing_frequency() {
    use tickloop_harness::protocol::TickSubscriptionFrequency;
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let script = fixture.path("work/frequency.py");
        let gate = fixture.path("work/frequency-change");
        std::fs::write(&script, format!("import pathlib,time\nprint('baseline',flush=True)\nwhile not pathlib.Path({:?}).exists(): time.sleep(.02)\nprint('pending content',flush=True)\ntime.sleep(600)\n", gate.to_string_lossy())).unwrap();
        let (base_url, mut requests) = model_http_support::serve_responses(vec![
            (200, chat_reply(Some(("TickManage", "open", json!({"action":"open","command":["/usr/bin/python3","-u",script]}))), "")),
            (200, chat_reply(None, "Frequency fixture ready.")),
            (200, chat_reply(None, "Manual update received.")),
        ]).await;
        fixture.set_chat_model(&base_url);
        let mut view = PtyView::start_with_environment(&fixture, &["--dangerously-skip-permissions"], &[("COLORTERM", std::path::Path::new("truecolor"))]);
        view.wait_for("input ready", new_input_is_visible).await;
        view.send(b"Open the test terminal.\r");
        view.wait_for("terminal opened", |frame| frame.contains("Monitor 1") && frame.contains("Frequency fixture ready.")).await;
        let client = fixture.client().await;
        let execution = catalog(&client).await[0].execution;
        view.send(b"\x1b[C");
        view.wait_for("Monitor focused", |frame| frame.contains("enter open")).await;
        view.send(b"s");
        let original = eventually("high subscription", || async {
            tick_subscriptions(&client, execution).await.into_iter().find(|row| row.frequency == TickSubscriptionFrequency::High)
        }).await;
        std::fs::write(&gate, "change").unwrap();
        eventually("pending screen", || async {
            tick_subscriptions(&client, execution).await.into_iter().find(|row| row.has_pending_change)
        }).await;
        let signals = |view: &PtyView| {
            let mut parser = vt100::Parser::new(view.height as u16, view.width as u16, 0);
            parser.process(&view.output.lock().unwrap());
            let (mut unread, mut progress) = (false, false);
            for row in 0..view.height as u16 {
                let mut in_monitor = false;
                for col in 0..view.width as u16 {
                    if let Some(cell) = parser.screen().cell(row, col) {
                        if cell.contents() == "│" { in_monitor = true; }
                        if !in_monitor { continue; }
                        unread |= cell.contents() == ">" && cell.fgcolor() == vt100::Color::Rgb(255, 160, 120);
                        progress |= cell.bgcolor() == vt100::Color::Rgb(110, 63, 48);
                    }
                }
            }
            (unread, progress)
        };
        eventually("visible unread and progress", || async { (signals(&view) == (true, true)).then_some(()) }).await;
        for (frequency, label) in [(TickSubscriptionFrequency::Medium, "10 min"), (TickSubscriptionFrequency::Low, "30 min")] {
            view.send(b"s");
            view.wait_for("new frequency footer", |frame| frame.contains(label)).await;
            eventually("frequency applied by the worker", || async {
                tick_subscriptions(&client, execution).await.into_iter().find(|row| row.frequency == frequency)
            }).await;
            for _ in 0..10 {
                let rows = tick_subscriptions(&client, execution).await;
                let row = rows.iter().find(|row| row.id == original.id).expect("frequency does not replace the subscription");
                assert_eq!(row.frequency, frequency);
                assert!(row.has_pending_change, "s cleared pending at {frequency:?}: {row:?}\n{}", view.frame());
                assert!(row.pending_since_ms.is_some());
                assert_eq!(signals(&view), (true, true), "s cleared the visible update signals at {frequency:?}: {}", view.frame());
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }
        view.send(b"s");
        eventually("subscription cancelled", || async {
            tick_subscriptions(&client, execution).await.is_empty().then_some(())
        }).await;
        eventually("cancellation keeps unread but removes periodic progress", || async {
            (signals(&view) == (true, false)).then_some(())
        }).await;
        view.send(b"s");
        eventually("resubscribed at high", || async {
            tick_subscriptions(&client, execution).await.into_iter().find(|row| row.frequency == TickSubscriptionFrequency::High)
        }).await;
        eventually("resubscribing preserves known unread", || async { (signals(&view).0).then_some(()) }).await;
        view.send(b"sss");
        eventually("rapid cycle cancels again", || async { tick_subscriptions(&client, execution).await.is_empty().then_some(()) }).await;
        view.wait_for("manual send available globally", |frame| frame.contains("u send update") && frame.contains("s subscriptions")).await;
        assert!(!view.frame().contains("Not subscribed"));
        view.send(b"u");
        view.wait_for("manual update delivered", |frame| frame.contains("Manual update received.") && frame.contains("TickUpdate:")).await;
        eventually("manual consumption clears unread", || async { (!signals(&view).0).then_some(()) }).await;
        assert!(tick_subscriptions(&client, execution).await.is_empty());
        let mut captured = Vec::new();
        while let Ok(request) = requests.try_recv() { captured.push(request); }
        assert!(captured.iter().any(|(_, body)| body.to_string().contains("pending content")), "manual update carries the current terminal text");
        view.send(CLOSE);
        view.wait_for("input focused after frequency test", input_focused).await;
        view.close().await;
    }).catch_unwind().await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
