use super::input_dialog_pty::{FIXTURE_DIRECTORY, Fixture, TIMEOUT};
use super::*;

const SUGGESTION_FIXTURE_STACK_BYTES: usize = 16 * 1024 * 1024;

use crate::terminal::model_http_support as suggestion_http;

#[test]
#[ignore]
fn prompt_suggestion_pty_fixture() {
    let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
    thread::Builder::new()
        .stack_size(SUGGESTION_FIXTURE_STACK_BYTES)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut model = crate::terminal::test_support::config();
                    model.base_url = std::fs::read_to_string(directory.join("model-url")).unwrap();
                    let replies = if directory.join("fail-turn").exists() {
                        vec![]
                    } else {
                        vec![
                            crate::terminal::test_support::text("Completed fixture turn."),
                            crate::terminal::test_support::text("Accepted fixture prompt."),
                        ]
                    };
                    let scripted =
                        Arc::new(crate::terminal::test_support::ScriptedModel::new(replies));
                    let core = tickloop_agent::runtime::Agent::new_embedded(
                        model.clone(),
                        scripted.clone(),
                    );
                    let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
                        core.clone(),
                        model.clone(),
                    );
                    let chat = agent.create_chat(directory.clone()).await.unwrap();
                    if directory.join("plan-mode").exists() {
                        agent
                            .set_execution_mode(
                                chat,
                                tickloop_agent::permissions::ExecutionMode::Plan,
                            )
                            .await
                            .unwrap();
                    }
                    if directory.join("running-shell").exists() {
                        agent
                            .submit_user_shell_for_goal(chat, None, "sleep 30".into(), true)
                            .await
                            .unwrap();
                    }
                    let snapshot = agent.snapshot(chat).await.unwrap();
                    let (console, input) = Console::start(true).unwrap();
                    crate::terminal::initialize_startup_console(
                        &console,
                        &model,
                        &directory,
                        tickloop_agent::permissions::ExecutionMode::Auto,
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
                            chat: crate::terminal::StartupChat::Resumed(Box::new(snapshot), false),
                            user_settings: Some(tickloop_agent::settings::UserSettings::in_root(
                                &directory,
                            )),
                        },
                        Some(directory.clone()),
                    )
                    .await
                    .unwrap();
                    let prompts = scripted
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .flat_map(|request| &request.messages)
                        .filter(|message| message.role == tickloop_agent::model::Role::User)
                        .flat_map(|message| &message.content)
                        .filter_map(|content| match content {
                            tickloop_agent::model::Content::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    std::fs::write(
                        directory.join("submitted-prompts"),
                        serde_json::to_vec(&prompts).unwrap(),
                    )
                    .unwrap();
                    core.shutdown().await;
                })
        })
        .unwrap()
        .join()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_suggestion_pty_generates_ghost_without_submitting_until_accepted() {
    let event = |name: &str, mut value: serde_json::Value| {
        value["type"] = name.into();
        format!("event: {name}\ndata: {value}\n\n")
    };
    let body = [
        event(
            "message_start",
            serde_json::json!({"message":{"role":"assistant","content":[]}}),
        ),
        event(
            "content_block_start",
            serde_json::json!({"index":0,"content_block":{"type":"text","text":"进度怎么样了"}}),
        ),
        event("content_block_stop", serde_json::json!({"index":0})),
        event(
            "message_delta",
            serde_json::json!({"delta":{"stop_reason":"end_turn"}}),
        ),
        event("message_stop", serde_json::json!({})),
    ]
    .concat();
    let (url, request) = suggestion_http::serve_response(200, body).await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model-url"), url).unwrap();
    std::fs::write(directory.path().join("running-shell"), "").unwrap();
    let mut fixture = Fixture::start(
        directory.path(),
        80,
        24,
        "terminal::console::prompt_suggestion_pty::prompt_suggestion_pty_fixture",
    );
    fixture.wait(|frame| frame.contains("Chat:") && frame.contains("fixture"));
    fixture.send("修好了检查一下\r".as_bytes());
    let frame = fixture.wait(|frame| frame.contains("进度怎么样了"));
    assert!(frame.contains("Done"), "{frame}");
    assert!(frame.contains("1 shell still running"), "{frame}");
    assert!(
        regex::Regex::new(r"Done · \d+s · done \d{2}:\d{2}")
            .unwrap()
            .is_match(&frame),
        "{frame}"
    );
    assert!(!frame.contains("Accepted fixture prompt."), "{frame}");
    let (_, request) = tokio::time::timeout(TIMEOUT, request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request["model"], "fixture");
    assert!(
        request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    );
    fixture.send("\t 再检查\r".as_bytes());
    fixture.wait(|frame| frame.contains("Accepted fixture prompt."));
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
    let prompts: Vec<String> =
        serde_json::from_slice(&std::fs::read(directory.path().join("submitted-prompts")).unwrap())
            .unwrap();
    assert!(
        prompts.iter().any(|prompt| prompt == "进度怎么样了 再检查"),
        "{prompts:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_suggestion_pty_off_plan_and_failed_turns_make_no_model_request() {
    for suppression in ["off", "plan", "failed"] {
        let (url, request) = suggestion_http::serve_response(200, String::new()).await;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("model-url"), url).unwrap();
        match suppression {
            "off" => std::fs::write(
                directory.path().join("settings.json"),
                r#"{"promptSuggestionEnabled":false}"#,
            )
            .unwrap(),
            "plan" => std::fs::write(directory.path().join("plan-mode"), "").unwrap(),
            "failed" => std::fs::write(directory.path().join("fail-turn"), "").unwrap(),
            _ => unreachable!(),
        }
        let mut fixture = Fixture::start(
            directory.path(),
            80,
            24,
            "terminal::console::prompt_suggestion_pty::prompt_suggestion_pty_fixture",
        );
        fixture.wait(|frame| frame.contains("Chat:") && frame.contains("fixture"));
        fixture.send(b"check the fixture\r");
        fixture.wait(|frame| {
            frame.contains(if suppression == "failed" {
                "Failed:"
            } else {
                "Done"
            })
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(300), request)
                .await
                .is_err(),
            "{suppression} issued a suggestion request"
        );
        fixture.send(b"/exit\r");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = fixture.child.try_wait().unwrap() {
                assert!(status.success(), "{suppression}: {}", fixture.frame());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{suppression}: {}",
                fixture.frame()
            );
            thread::sleep(Duration::from_millis(15));
        }
    }
}
