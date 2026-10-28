use super::input_dialog_pty::{FIXTURE_DIRECTORY, Fixture, TIMEOUT};
use super::*;
#[cfg(test)]
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_harness::test_support::screen;

const SEND_NOW_FIXTURE_STACK_BYTES: usize = 16 * 1024 * 1024;
/// What a terminal speaking the kitty keyboard protocol sends for ctrl+enter.
const CTRL_ENTER: &[u8] = b"\x1b[13;5u";
const TYPED_NOW: &str = "how is it going";
const QUEUED_THEN_SENT: &str = "promote the queued one";
const TYPED_NOW_REPLY: &str = "Answered the typed message.";
const QUEUED_REPLY: &str = "Answered the queued message.";

/// A Bash call that only ends when the fixture directory gets a `finish` file, which the
/// driver never writes: send now moves it to the background, where it runs until the Agent
/// shuts down.
fn waiting_tool(id: &str) -> Vec<tickloop_harness::protocol::Content> {
    tickloop_harness::test_support::call(
        id,
        "Bash",
        serde_json::json!({
            "command": "while [ ! -f finish ]; do sleep 0.05; done",
            "description": "Wait for the fixture",
        }),
    )
}

/// How often `text` shows above the composer of `frame`: sent in the conversation, and queued.
fn placements(frame: &str, text: &str) -> (usize, usize) {
    let rows = frame.lines().collect::<Vec<_>>();
    let rules = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.starts_with('─'))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let above = &rows[..rules[rules.len() - 2]];
    let total = above.iter().filter(|row| row.contains(text)).count();
    let queued = screen::queued_inputs(frame)
        .iter()
        .filter(|row| row.contains(text))
        .count();
    (total.saturating_sub(queued), queued)
}

/// Follows `text` through the frames drawn after `offset`: it shows at most once, queued or
/// sent, not as sent before `taken`, and once sent it stays sent, as Claude Code moves a queued
/// message into the conversation only when the turn takes it.
fn assert_taken_once(fixture: &Fixture, offset: usize, taken: usize, text: &str) {
    let mut sent = false;
    for (end, frame) in fixture.frames_after(offset) {
        let (echoes, entries) = placements(&frame, text);
        assert!(echoes + entries <= 1, "{text} is shown once: {frame}");
        assert!(
            end > taken || echoes == 0,
            "{text} is not sent yet: {frame}"
        );
        assert!(!sent || echoes == 1, "{text} stays sent: {frame}");
        sent |= echoes == 1;
    }
    assert!(sent, "{text} is shown as sent in the end");
}

#[test]
#[ignore]
fn send_now_pty_fixture() {
    let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
    thread::Builder::new()
        .stack_size(SEND_NOW_FIXTURE_STACK_BYTES)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let model = tickloop_harness::test_support::config();
                    let scripted =
                        Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
                            waiting_tool("first-wait"),
                            tickloop_harness::test_support::text(TYPED_NOW_REPLY),
                            waiting_tool("second-wait"),
                            tickloop_harness::test_support::text(QUEUED_REPLY),
                        ]));
                    let core = tickloop_harness::runtime::Agent::new_embedded(
                        model.clone(),
                        scripted.clone(),
                    );
                    core.install_own_tickhost();
                    let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
                        core.clone(),
                        model.clone(),
                    );
                    let chat = agent.create_chat(directory.clone()).await.unwrap();
                    core.set_execution_mode(
                        chat.into(),
                        tickloop_harness::protocol::ExecutionMode::AcceptEdits,
                    )
                    .await
                    .unwrap();
                    core.set_permission_rules(
                        chat.into(),
                        vec![tickloop_harness::protocol::PermissionRule {
                            id: Uuid::new_v4(),
                            decision: tickloop_harness::protocol::Decision::Allow,
                            tool: "Bash".into(),
                            specifier: None,
                            input: None,
                            loop_id: None,
                            persistent: false,
                        }],
                    )
                    .await
                    .unwrap();
                    let snapshot = agent.snapshot(chat).await.unwrap();
                    let (console, input) = Console::start(true).unwrap();
                    crate::terminal::initialize_startup_console(
                        &console,
                        Some(&model),
                        &directory,
                        tickloop_harness::protocol::ExecutionMode::AcceptEdits,
                        vec![],
                    )
                    .unwrap();
                    console.set_prompt_enabled(true).unwrap();
                    crate::terminal::interact(
                        &agent,
                        directory.clone(),
                        &console,
                        input,
                        crate::terminal::StartupState {
                            model_selected: true,
                            launch_arguments: Vec::new(),
                            chat: crate::terminal::StartupChat::Resumed(Box::new(snapshot), false),
                            user_settings: Some(tickloop_code_settings::UserSettings::in_root(
                                &directory,
                            )),
                        },
                        Some(directory.clone()),
                    )
                    .await
                    .unwrap();
                    // The newest user text of every model request, in request order.
                    let newest_inputs = scripted
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|request| {
                            request
                                .messages
                                .iter()
                                .filter(|message| {
                                    message.role == tickloop_harness::protocol::Role::User
                                })
                                .flat_map(|message| &message.content)
                                .filter_map(|content| match content {
                                    tickloop_harness::protocol::Content::Text { text } => {
                                        Some(text.clone())
                                    }
                                    _ => None,
                                })
                                .next_back()
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>();
                    std::fs::write(
                        directory.join("newest-inputs"),
                        serde_json::to_vec(&newest_inputs).unwrap(),
                    )
                    .unwrap();
                    core.shutdown().await;
                })
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn send_now_pty_ctrl_enter_reaches_the_model_without_waiting_for_the_running_tool() {
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::start(
        directory.path(),
        120,
        30,
        "terminal::console::send_now_pty::send_now_pty_fixture",
    );
    fixture.wait(|frame| frame.contains("continue this Chat") && frame.contains("fixture"));
    assert!(
        fixture.raw_output().contains("\x1b[>1u"),
        "the View asks the terminal to tell ctrl+enter from enter"
    );

    fixture.send(b"start the first long work\r");
    fixture
        .wait(|frame| frame.contains("esc to interrupt") && frame.contains("Wait for the fixture"));
    fixture.send(TYPED_NOW.as_bytes());
    fixture.wait(|frame| frame.contains("enter queue · ctrl+enter send now"));
    let typed_at = fixture.raw_output().len();
    fixture.send(CTRL_ENTER);
    let answered = fixture.wait(|frame| frame.contains(TYPED_NOW_REPLY));
    assert!(
        answered.contains("Command was moved to the background")
            && answered.contains("was not interrupted")
            && !answered.contains("Interrupted"),
        "the running command moved to the background instead of being interrupted: {answered}"
    );
    assert_taken_once(&fixture, typed_at, typed_at, TYPED_NOW);

    fixture.send(b"start the second long work\r");
    fixture.wait(|frame| {
        frame.contains("esc to interrupt") && frame.matches("Wait for the fixture").count() == 2
    });
    let queued_at = fixture.raw_output().len();
    fixture.send(format!("{QUEUED_THEN_SENT}\r").as_bytes());
    let queued = fixture.wait(|frame| {
        placements(frame, QUEUED_THEN_SENT) == (0, 1) && frame.contains(QUEUED_SEND_NOW_HINT)
    });
    assert!(!queued.contains(QUEUED_REPLY), "{queued}");
    let promoted_at = fixture.raw_output().len();
    fixture.send(CTRL_ENTER);
    fixture.wait(|frame| frame.contains(QUEUED_REPLY));
    assert_taken_once(&fixture, queued_at, promoted_at, QUEUED_THEN_SENT);

    fixture.send(b"/exit\r");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(status.success(), "{}", fixture.frame());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not exit: {}",
            fixture.frame()
        );
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        fixture.raw_output().contains("\x1b[<1u"),
        "the keyboard flags are popped on exit"
    );
    assert!(!directory.path().join("finish").exists());
    let newest_inputs: Vec<String> =
        serde_json::from_slice(&std::fs::read(directory.path().join("newest-inputs")).unwrap())
            .unwrap();
    assert_eq!(newest_inputs.len(), 4, "{newest_inputs:?}");
    assert_eq!(newest_inputs[1], TYPED_NOW, "{newest_inputs:?}");
    assert_eq!(newest_inputs[3], QUEUED_THEN_SENT, "{newest_inputs:?}");
}
