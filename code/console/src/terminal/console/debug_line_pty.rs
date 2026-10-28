use super::input_dialog_pty::{FIXTURE_DIRECTORY, Fixture, TIMEOUT};
use super::*;
use tickloop_code_local_environment::AgentLocalExt;

const DEBUG_LINE_FIXTURE_STACK_BYTES: usize = 16 * 1024 * 1024;
const REPLY: &str = "Checked the fixture.";

#[test]
#[ignore]
fn debug_line_pty_fixture() {
    let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
    thread::Builder::new()
        .stack_size(DEBUG_LINE_FIXTURE_STACK_BYTES)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let model = tickloop_harness::test_support::config();
                    let scripted =
                        Arc::new(tickloop_harness::test_support::ScriptedModel::with_usages(
                            vec![tickloop_harness::test_support::text(REPLY)],
                            vec![tickloop_harness::protocol::Usage {
                                input_tokens: Some(50),
                                output_tokens: Some(40),
                                cache_read_input_tokens: Some(1_000),
                                cache_write_input_tokens: Some(200),
                                reasoning_output_tokens: None,
                            }],
                        ));
                    let core =
                        tickloop_harness::runtime::Agent::new_embedded(model.clone(), scripted);
                    core.install_own_tickhost();
                    let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
                        core.clone(),
                        model.clone(),
                    );
                    let chat = agent.create_chat(directory.clone()).await.unwrap();
                    let snapshot = agent.snapshot(chat).await.unwrap();
                    let (console, input) = Console::start(true).unwrap();
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
                            user_settings: None,
                        },
                        None,
                    )
                    .await
                    .unwrap();
                    if let Some(details) = console.test_debug_details() {
                        std::fs::write(directory.join("debug-details"), details).unwrap();
                    }
                    core.shutdown().await;
                })
        })
        .unwrap()
        .join()
        .unwrap();
}

/// The conversation column's rows, trimmed, with Monitor's column beside it cut off.
fn conversation_rows(frame: &str) -> Vec<String> {
    frame
        .lines()
        .map(|line| {
            line.split('│')
                .next()
                .unwrap()
                .replace('\u{a0}', " ")
                .trim()
                .to_owned()
        })
        .collect()
}

/// Runs one turn in a Console whose environment sets `TICKLOOP_DEBUG` to `debug`, and returns its
/// conversation rows once the turn's finished line shows, with the debug details it kept.
fn finished_turn(debug: &str) -> (Vec<String>, Option<String>) {
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::start_with_environment(
        directory.path(),
        200,
        30,
        "terminal::console::debug_line_pty::debug_line_pty_fixture",
        &[("TICKLOOP_DEBUG", debug)],
    );
    fixture.wait(|frame| frame.contains("continue this Chat"));
    fixture.send(b"Check the fixture\r");
    let frame =
        fixture.wait(|frame| frame.contains(REPLY) && super::super::shows_turn_summary(frame));
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
    (
        conversation_rows(&frame),
        std::fs::read_to_string(directory.path().join("debug-details")).ok(),
    )
}

#[test]
fn debug_line_pty_ends_the_finished_line_with_the_cache_summary_only_when_switched_on() {
    let (rows, details) = finished_turn("1");
    let finished = rows
        .iter()
        .find(|row| row.starts_with("⏺ ") && row.contains(" for "))
        .unwrap();
    assert!(
        regex::Regex::new(r"^⏺ \p{L}+ for \d+s · cache 80% · avg 80%  \[debug info\]$")
            .unwrap()
            .is_match(finished),
        "{rows:#?}"
    );
    let details = details.unwrap();
    for line in [
        "turn: 1",
        "model: fixture",
        "requests: 1 (chat total ",
        "cache this turn: read 1000 / input 1250 (80%), write 200",
        "cache state: normal",
        "output: 40",
        "tickloop: ",
    ] {
        assert!(details.contains(line), "{line}: {details}");
    }

    let (rows, details) = finished_turn("0");
    let finished = rows
        .iter()
        .find(|row| row.starts_with("⏺ ") && row.contains(" for "))
        .unwrap();
    assert!(
        regex::Regex::new(r"^⏺ \p{L}+ for \d+s$")
            .unwrap()
            .is_match(finished),
        "{rows:#?}"
    );
    assert_eq!(details, None);
}
