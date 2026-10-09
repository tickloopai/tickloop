use super::input_dialog_pty::{FIXTURE_DIRECTORY, Fixture, TIMEOUT};
use super::*;

const SUGGESTION_FIXTURE_STACK_BYTES: usize = 16 * 1024 * 1024;

use tickloop_agent_test_support::model_http_support as suggestion_http;

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
                    let user_case = directory.join("user-case").exists();
                    let mut model = if user_case {
                        tickloop_agent_model::cards::ModelCatalog::defaults()
                            .get("kimi-cn-api", "kimi-k3")
                            .unwrap()
                    } else {
                        tickloop_agent_test_support::config()
                    };
                    model.base_url = std::fs::read_to_string(directory.join("model-url")).unwrap();
                    model.api_key_env.clear();
                    let replies = if user_case {
                        vec![
                            tickloop_agent_test_support::call("delegate", "Subagent", serde_json::json!({"task":"Complete the fixture child"})),
                            tickloop_agent_test_support::text("Child completed."),
                            tickloop_agent_test_support::text("Parent completed."),
                            tickloop_agent_test_support::call("ls", "Bash", serde_json::json!({"command":"ls", "description":"List files in current directory"})),
                            tickloop_agent_test_support::text("当前目录包含 settings.json 和 model-url。"),
                            tickloop_agent_test_support::text("Accepted fixture prompt."),
                        ]
                    } else if directory.join("fail-turn").exists() {
                        vec![]
                    } else {
                        vec![
                            tickloop_agent_test_support::text("Completed fixture turn."),
                            tickloop_agent_test_support::text("Accepted fixture prompt."),
                        ]
                    };
                    let scripted =
                        Arc::new(tickloop_agent_test_support::ScriptedModel::new(replies));
                    let core = tickloop_agent_runtime::Agent::new_embedded(
                        model.clone(),
                        scripted.clone(),
                    );
                    let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
                        core.clone(),
                        model.clone(),
                    );
                    let mut chat = agent.create_chat(directory.clone()).await.unwrap();
                    if user_case {
                        core.set_execution_mode(chat, tickloop_agent_protocol::ExecutionMode::Auto).await.unwrap();
                        let mut events = core.subscribe();
                        let receipt = core.submit(chat, None, "Complete the fixture family".into()).await.unwrap();
                        loop {
                            let event = tickloop_agent_test_support::next_event(&mut events).await;
                            assert!(!matches!(event.kind, tickloop_agent_protocol::EventKind::Failed(_)), "{event:?}");
                            if event.run_id == receipt.run_id && matches!(event.kind, tickloop_agent_protocol::EventKind::Completed { .. }) { break; }
                        }
                        let child = agent.catalog().await.unwrap().into_iter().find(|row| row.parent.is_some()).unwrap();
                        assert_eq!(child.state, tickloop_agent_protocol::LoopState::Completed);
                        if directory.join("delegated-view").exists() { chat = child.chat_id; }
                    }
                    if directory.join("plan-mode").exists() {
                        agent
                            .set_execution_mode(
                                chat,
                                tickloop_agent_protocol::ExecutionMode::Plan,
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
                        tickloop_agent_protocol::ExecutionMode::Auto,
                        vec![],
                    )
                    .unwrap();
                    console.set_prompt_enabled(true).unwrap();
                    let interaction = crate::terminal::interact(
                        &agent,
                        directory.clone(),
                        &console,
                        input,
                        crate::terminal::StartupState {
                            chat: crate::terminal::StartupChat::Resumed(Box::new(snapshot), false),
                            user_settings: Some(tickloop_agent_config::UserSettings::in_root(
                                &directory,
                            )),
                        },
                        Some(directory.clone()),
                    );
                    tokio::pin!(interaction);
                    let mut inspection = tokio::time::interval(Duration::from_millis(10));
                    loop {
                        tokio::select! {
                            result = &mut interaction => { result.unwrap(); break; },
                            _ = inspection.tick() => {
                                let state = console.state.lock().unwrap();
                                std::fs::write(directory.join("editor-state.tmp"), serde_json::to_vec(&serde_json::json!({
                                    "draft": state.editor.draft(),
                                    "suggestion": state.visible_prompt_suggestion(),
                                    "pending": state.prompt_suggestion_request.is_some(),
                                })).unwrap()).unwrap();
                                std::fs::rename(directory.join("editor-state.tmp"), directory.join("editor-state")).unwrap();
                            }
                        }
                    }
                    let prompts = scripted
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .flat_map(|request| &request.messages)
                        .filter(|message| message.role == tickloop_agent_protocol::Role::User)
                        .flat_map(|message| &message.content)
                        .filter_map(|content| match content {
                            tickloop_agent_protocol::Content::Text { text } => Some(text.clone()),
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
    // At 80 columns Monitor sits beside the conversation, so the status line wraps within the
    // conversation column; rejoining its rows reads it whole.
    let conversation: String = frame
        .lines()
        .map(|line| line.split('│').next().unwrap())
        .collect();
    assert!(conversation.contains("1 shell still running"), "{frame}");
    assert!(
        regex::Regex::new(r"Done · \d+s · done \d{2}:\d{2}")
            .unwrap()
            .is_match(&conversation),
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
async fn prompt_suggestion_pty_chinese_bash_kimi_with_completed_child_shows_and_tab_accepts() {
    let body = concat!(
        "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"用户在查看目录，接下来可能查看说明。\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"查看 README.md\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (url, request) = suggestion_http::serve_response(200, body.into()).await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model-url"), url).unwrap();
    std::fs::write(directory.path().join("user-case"), "").unwrap();
    let mut fixture = Fixture::start(
        directory.path(),
        110,
        32,
        "terminal::console::prompt_suggestion_pty::prompt_suggestion_pty_fixture",
    );
    fixture.wait(|frame| frame.contains("Chat:") && frame.contains("kimi-k3"));
    fixture.send("列出当前目录下的文件\r".as_bytes());
    let frame = fixture.wait(|frame| frame.contains("查看 README.md"));
    assert!(
        frame.contains("Done") && frame.contains("Bash") && frame.contains("Subagents 1"),
        "{frame}"
    );
    let editor = || {
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(directory.path().join("editor-state")).unwrap(),
        )
        .unwrap()
    };
    fixture.wait(|_| editor()["suggestion"] == "查看 README.md");
    assert_eq!(editor()["draft"], "");
    let (_, body) = tokio::time::timeout(TIMEOUT, request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body["model"], "kimi-k3");
    assert_eq!(body["max_completion_tokens"], 1_024);
    assert!(body.get("tools").is_none());
    assert!(body.to_string().contains("列出当前目录下的文件"));
    fixture.send(b"\t");
    fixture.wait(|_| editor()["draft"] == "查看 README.md" && editor()["suggestion"].is_null());
    fixture.send(" 再检查\r".as_bytes());
    fixture.wait(|frame| frame.contains("Accepted fixture prompt."));
    fixture.send(b"/exit\r");
    let deadline = Instant::now() + TIMEOUT;
    while fixture.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "{}", fixture.frame());
        thread::sleep(Duration::from_millis(15));
    }
    let prompts: Vec<String> =
        serde_json::from_slice(&std::fs::read(directory.path().join("submitted-prompts")).unwrap())
            .unwrap();
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt == "查看 README.md 再检查"),
        "{prompts:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_suggestion_pty_delegated_chinese_bash_explains_suppression_without_request() {
    let (url, request) = suggestion_http::serve_response(200, String::new()).await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model-url"), url).unwrap();
    std::fs::write(directory.path().join("user-case"), "").unwrap();
    std::fs::write(directory.path().join("delegated-view"), "").unwrap();
    let mut fixture = Fixture::start(
        directory.path(),
        110,
        32,
        "terminal::console::prompt_suggestion_pty::prompt_suggestion_pty_fixture",
    );
    fixture.wait(|frame| frame.contains("Chat:") && frame.contains("kimi-k3"));
    fixture.send("列出当前目录下的文件\r".as_bytes());
    let frame = fixture.wait(|frame| frame.contains("Done") && frame.contains("当前目录包含"));
    assert!(frame.contains("Bash"), "{frame}");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), request)
            .await
            .is_err()
    );
    fixture.send(b"\t");
    let editor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("editor-state")).unwrap())
            .unwrap();
    assert_eq!(editor["draft"], "");
    assert!(editor["suggestion"].is_null());
    fixture.send(b"/status\r");
    fixture.wait(|frame| frame.contains("Prompt suggestions: On · suppressed: delegated Agent"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_suggestion_pty_provider_failure_is_quiet_and_status_has_no_private_payload() {
    let (url, mut requests, release) = suggestion_http::serve_responses_with_gated_response(
        vec![(500, "private provider error payload".into())],
        0,
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model-url"), url).unwrap();
    std::fs::write(directory.path().join("user-case"), "").unwrap();
    let mut fixture = Fixture::start(
        directory.path(),
        110,
        32,
        "terminal::console::prompt_suggestion_pty::prompt_suggestion_pty_fixture",
    );
    fixture.wait(|frame| frame.contains("Chat:") && frame.contains("kimi-k3"));
    fixture.send("列出当前目录下的文件\r".as_bytes());
    requests.recv().await.unwrap();
    let editor = || {
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(directory.path().join("editor-state")).unwrap(),
        )
        .unwrap()
    };
    fixture.wait(|_| editor()["pending"] == true);
    release.send(()).unwrap();
    let frame = fixture.wait(|frame| frame.contains("Done") && editor()["pending"] == false);
    assert!(editor()["suggestion"].is_null());
    assert!(
        !frame.contains("failed: Provider") && !frame.contains("private provider"),
        "{frame}"
    );
    fixture.send(b"/status\r");
    let frame = fixture
        .wait(|frame| frame.contains("Prompt suggestions: On · last: failed: Provider request"));
    assert!(!frame.contains("private provider"), "{frame}");
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
