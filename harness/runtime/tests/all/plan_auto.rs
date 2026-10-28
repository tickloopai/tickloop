use serde_json::json;
use std::sync::Arc;
use tickloop_harness_protocol::{EventKind, ExecutionMode, PermissionScope};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::{
    ScriptedModel, auto_classification, call, config, next_event, text,
};

/// An Agent like [`Agent::new_embedded`] whose user settings set `useAutoModeDuringPlan`.
pub(crate) fn agent_with_plan_setting(
    model: Arc<ScriptedModel>,
    use_auto_mode_during_plan: bool,
) -> Agent {
    use tickloop_harness_policy::settings::{MemorySettingsStore, SettingsSnapshot};
    use tickloop_harness_runtime::{HarnessOptions, HarnessSeams, environment::FolderEnvironment};
    use tickloop_harness_test_support::harness::ChosenModels;
    let settings = MemorySettingsStore::new(SettingsSnapshot {
        use_auto_mode_during_plan,
        ..SettingsSnapshot::default()
    });
    Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(FolderEnvironment::bare())),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            settings: Some(Arc::new(settings)),
            host: tickloop_harness_protocol::AgentHostOptions::embedded_defaults(),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

/// What Plan entered from Auto would ask about goes through Auto's admission: an outside `Read` is
/// admitted and a `WebFetch` is reviewed, whose rejection returns to the model. With the setting
/// off, or entered from Ask, both ask and the reviewer never runs.
#[tokio::test]
async fn plan_admits_and_reviews_as_auto_only_after_auto_unless_the_user_turned_it_off() {
    for (mode_before_plan, use_auto_mode_during_plan, use_auto) in [
        (ExecutionMode::Auto, true, true),
        (ExecutionMode::Auto, false, false),
        (ExecutionMode::AcceptEdits, true, false),
    ] {
        let external = tempfile::tempdir().unwrap();
        let notes = external.path().join("notes.txt");
        std::fs::write(&notes, "PLAN_NOTES").unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call("read", "Read", json!({"file_path": notes})),
            call(
                "fetch",
                "WebFetch",
                json!({"url":"https://docs.example.com/guide","prompt":"Summarize"}),
            ),
            text("done"),
        ]));
        let agent = agent_with_plan_setting(model.clone(), use_auto_mode_during_plan);
        let reviewer = Arc::new(ScriptedModel::new(vec![
            text(
                &auto_classification(true).to_string()
            );
            2
        ]));
        agent.set_auto_reviewer(reviewer.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        for mode in [mode_before_plan, ExecutionMode::Plan] {
            agent.set_execution_mode(chat, mode).await.unwrap();
        }
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Plan from the notes".into())
            .await
            .unwrap();
        let mut prompts = Vec::new();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    let allow = request.name == "Read";
                    prompts.push(request.name.clone());
                    agent
                        .answer_permission_with_comment(
                            chat,
                            request.id,
                            allow,
                            PermissionScope::Once,
                            (!allow).then(|| "not now".into()),
                        )
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let reviews = reviewer.requests.lock().unwrap().len();
        if use_auto {
            assert!(prompts.is_empty(), "{prompts:?}");
            assert!(reviews > 0);
        } else {
            assert_eq!(prompts, ["Read", "WebFetch"]);
            assert_eq!(reviews, 0);
        }
        let last = serde_json::to_string(&model.requests.lock().unwrap().last().unwrap().messages)
            .unwrap();
        assert!(last.contains("PLAN_NOTES"), "{mode_before_plan:?}");
        assert_eq!(
            last.contains("external-code restriction"),
            use_auto,
            "the review's rejection reaches the model"
        );
        agent.shutdown().await;
    }
}

/// The model's EnterPlanMode from Ask keeps asking for what Ask asks about, an outside `Read` and
/// a `WebFetch`, and the reviewer never runs; entered from Auto, the same calls go through Auto's
/// admission. `Bash` never reaches the reviewer in Plan: its reads in the workspace are admitted
/// and everything else is refused as a side effect.
#[tokio::test]
async fn plan_entered_through_the_tool_uses_auto_admission_only_after_auto() {
    for (before_plan, use_auto) in [
        (ExecutionMode::AcceptEdits, false),
        (ExecutionMode::Auto, true),
    ] {
        let external = tempfile::tempdir().unwrap();
        let notes = external.path().join("notes.txt");
        std::fs::write(&notes, "PLAN_NOTES").unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call("plan", "EnterPlanMode", json!({})),
            call("read", "Read", json!({"file_path": notes})),
            call(
                "fetch",
                "WebFetch",
                json!({"url":"https://docs.example.com/guide","prompt":"Summarize"}),
            ),
            text("done"),
        ]));
        let agent = agent_with_plan_setting(model.clone(), true);
        let reviewer = Arc::new(ScriptedModel::new(vec![
            text(
                &auto_classification(true).to_string()
            );
            2
        ]));
        agent.set_auto_reviewer(reviewer.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        agent.set_execution_mode(chat, before_plan).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Plan from the notes".into())
            .await
            .unwrap();
        let mut prompts = Vec::new();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    let allow = request.name == "Read";
                    prompts.push(request.name.clone());
                    agent
                        .answer_permission_with_comment(
                            chat,
                            request.id,
                            allow,
                            PermissionScope::Once,
                            (!allow).then(|| "not now".into()),
                        )
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let label = format!("{before_plan:?}");
        let reviews = reviewer.requests.lock().unwrap().len();
        if use_auto {
            assert!(prompts.is_empty(), "{label} {prompts:?}");
            assert!(reviews > 0, "{label}");
        } else {
            assert_eq!(prompts, ["Read", "WebFetch"], "{label}");
            assert_eq!(reviews, 0, "{label}");
        }
        assert_eq!(
            agent.snapshot(chat).await.unwrap().policy.mode,
            ExecutionMode::Plan,
            "{label}"
        );
        let last = serde_json::to_string(&model.requests.lock().unwrap().last().unwrap().messages)
            .unwrap();
        assert!(last.contains("PLAN_NOTES"), "{label}");
        agent.shutdown().await;
    }
}

/// Serves the same HTML page to every request until dropped.
struct PageServer {
    url: String,
    requests: Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PageServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PageServer {
    async fn start() -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/guide", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(count) => request.extend_from_slice(&buffer[..count]),
                    }
                }
                served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = "<h1>Guide</h1><p>GUIDE_PAGE</p>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}

/// As Claude Code's Plan with bypass available does, Plan entered from Full Access, here through
/// the model's EnterPlanMode, allows what it would otherwise ask about: an outside `Read` and a
/// `WebFetch` run without a prompt or review. Plan still refuses an edit, and an explicit Ask rule
/// still asks.
#[tokio::test]
async fn plan_entered_from_full_access_allows_what_it_does_not_refuse() {
    for ask_for_reads in [false, true] {
        let external = tempfile::tempdir().unwrap();
        let notes = external.path().join("notes.txt");
        std::fs::write(&notes, "PLAN_NOTES").unwrap();
        let page = PageServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let edited = dir.path().join("edited.txt");
        let model = Arc::new(ScriptedModel::new(vec![
            call("plan", "EnterPlanMode", json!({})),
            call("read", "Read", json!({"file_path": notes})),
            call(
                "fetch",
                "WebFetch",
                json!({"url": page.url, "prompt": "Summarize"}),
            ),
            text("The guide says GUIDE_PAGE."),
            call(
                "edit",
                "Write",
                json!({"file_path": edited, "content": "changed"}),
            ),
            text("done"),
        ]));
        let agent = agent_with_plan_setting(model.clone(), true);
        let reviewer = Arc::new(ScriptedModel::new(vec![]));
        agent.set_auto_reviewer(reviewer.clone()).await;
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        if ask_for_reads {
            agent
                .set_permission_rules(
                    chat,
                    vec![tickloop_harness_protocol::PermissionRule {
                        id: uuid::Uuid::new_v4(),
                        decision: tickloop_harness_protocol::Decision::Ask,
                        tool: "Read".into(),
                        specifier: None,
                        input: None,
                        loop_id: None,
                        persistent: false,
                    }],
                )
                .await
                .unwrap();
        }
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Plan from the notes".into())
            .await
            .unwrap();
        let mut prompts = Vec::new();
        let mut results = std::collections::HashMap::new();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    prompts.push(request.name.clone());
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted {
                    call_id, result, ..
                } => {
                    results.insert(call_id, result);
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let label = format!("ask_for_reads={ask_for_reads}");
        let expected_prompts: &[&str] = if ask_for_reads { &["Read"] } else { &[] };
        assert_eq!(prompts, expected_prompts, "{label}");
        assert!(reviewer.requests.lock().unwrap().is_empty(), "{label}");
        assert!(results["read"].content.contains("PLAN_NOTES"), "{label}");
        assert!(
            !results["fetch"].is_error,
            "{label} {}",
            results["fetch"].content
        );
        assert_eq!(
            page.requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{label}"
        );
        assert!(results["edit"].is_error, "{label}");
        assert!(
            results["edit"].content.starts_with("Plan mode is active."),
            "{label} {}",
            results["edit"].content
        );
        assert!(!edited.exists(), "{label}");
        assert_eq!(
            agent.snapshot(chat).await.unwrap().policy.mode,
            ExecutionMode::Plan,
            "{label}"
        );
        agent.shutdown().await;
    }
}
