use std::{path::Path, sync::Arc, time::Duration};

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{broadcast, oneshot},
};
use tokio_tungstenite::accept_async;
use uuid::Uuid;

use super::{
    CodexModelClient,
    tests::{
        function_call_stream, reply_stream, send_ws_stream, serve_scripted_responses,
        state_with_credentials, text_stream,
    },
};
use crate::{
    model::{Content, HttpModelClient, ModelClient, ModelConfig, cards::built_in_model_profiles},
    permissions::ExecutionMode,
    runtime::{
        Agent, BranchRequest, Event, EventKind, LoopState, RequestKind, RewindMode, Trigger,
        TriggerSource,
    },
};

fn config() -> ModelConfig {
    built_in_model_profiles()
        .into_iter()
        .find(|config| config.model == "gpt-5.5")
        .unwrap()
}

fn client(state: &Path, endpoint: &reqwest::Url) -> Arc<dyn ModelClient> {
    Arc::new(
        CodexModelClient::new(config(), state)
            .unwrap()
            .with_loopback_test_endpoint(endpoint.as_str())
            .unwrap(),
    )
}

async fn completed(events: &mut broadcast::Receiver<Event>) {
    loop {
        match tokio::time::timeout(Duration::from_secs(8), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("Codex history request failed: {error}"),
            EventKind::PermissionRequested { .. } => panic!("unexpected protected operation"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn event_loop_after_logout_fails_and_does_not_resume_automatically() {
    triggered_request_after_logout(None).await;
}

#[tokio::test]
async fn timed_loop_after_logout_fails_and_does_not_resume_automatically() {
    triggered_request_after_logout(Some(1)).await;
}

async fn triggered_request_after_logout(interval_seconds: Option<u64>) {
    let (_directory, state) = state_with_credentials();
    let (endpoint, observed) = serve_scripted_responses(vec![(200, text_stream())]).await;
    let agent = Agent::open_embedded(state.clone(), config(), client(&state, &endpoint)).unwrap();
    let chat = agent.create_chat(state.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Observe when new evidence arrives".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let saved = std::fs::read(state.join("auth/openai-codex-credentials.json")).unwrap();
    crate::auth::CodexAuth::new(&state)
        .logout(&tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "Check fresh evidence".into(),
                interval_seconds,
                expires_at_ms: None,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    let source = || TriggerSource {
        source: "test-tick".into(),
        payload: json!({"sequence": 1}),
    };
    if interval_seconds.is_none() {
        agent
            .trigger(chat, receipt.loop_id, source())
            .await
            .unwrap();
    }
    loop {
        if let EventKind::Failed(error) =
            tokio::time::timeout(Duration::from_secs(8), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
        {
            assert!(error.contains("/model → Provider"), "{error}");
            break;
        }
    }
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .find(|goal| goal.id == receipt.loop_id)
            .unwrap()
            .state,
        LoopState::Failed
    );
    std::fs::write(state.join("auth/openai-codex-credentials.json"), saved).unwrap();
    assert!(
        agent
            .trigger(chat, receipt.loop_id, source())
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .find(|goal| goal.id == receipt.loop_id)
            .unwrap()
            .state,
        LoopState::Failed
    );
    assert_eq!(observed.await.unwrap().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn source_switch_and_rewind_preserve_original_private_checkpoint() {
    let (_directory, state) = state_with_credentials();
    let (endpoint, observed) = serve_scripted_responses(vec![
        (200, text_stream()),
        (200, reply_stream("second", "Public second response", 30)),
        (
            200,
            reply_stream("summary", "Public summary: preserve source files", 40),
        ),
        (200, reply_stream("continue", "Continuing after rewind", 40)),
    ])
    .await;
    let agent = Agent::open_embedded(state.clone(), config(), client(&state, &endpoint)).unwrap();
    let chat = agent.create_chat(state.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Preserve source files".into())
        .await
        .unwrap();
    completed(&mut events).await;
    agent
        .submit(chat, None, "Second checkpoint".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before
        .rewind_points()
        .into_iter()
        .find(|point| point.prompt == "Second checkpoint")
        .unwrap();
    let mut api = crate::model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash-responses")
        .unwrap();
    api.api_key_env = "CODEX_HISTORY_TEST_MISSING_API_KEY".into();
    let api_client = Arc::new(HttpModelClient::deferred(api.clone()).unwrap());
    agent
        .set_model(chat, api.clone(), api_client.clone())
        .await
        .unwrap();
    agent
        .set_model(chat, config(), client(&state, &endpoint))
        .await
        .unwrap();
    let compacted = agent.compact(chat).await.unwrap();
    assert!(compacted.summary.contains("preserve source files"));
    agent.set_model(chat, api, api_client).await.unwrap();
    agent
        .rewind(chat, checkpoint.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let rewound = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        rewound.messages,
        before.messages[..checkpoint.message_index]
    );
    assert!(!rewound.message_sources.is_empty());
    agent
        .set_model(chat, config(), client(&state, &endpoint))
        .await
        .unwrap();
    agent
        .submit(chat, None, "Continue after rewind".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let requests = observed.await.unwrap();
    assert!(requests[2]["tools"].is_null());
    assert!(
        requests[2]["input"]
            .to_string()
            .contains("Preserve source files")
    );
    assert!(
        requests[3]["input"]
            .to_string()
            .contains("Hello from Codex")
    );
    assert!(
        !requests[3]["input"]
            .to_string()
            .contains("Public second response")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn branch_copies_public_context_and_keeps_source_private_history_unchanged() {
    let (_directory, state) = state_with_credentials();
    let (endpoint, observed) = serve_scripted_responses(vec![
        (200, text_stream()),
        (200, reply_stream("branch", "Branch response", 20)),
    ])
    .await;
    let agent = Agent::open_embedded(state.clone(), config(), client(&state, &endpoint)).unwrap();
    let source = agent.create_chat(state.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(source, None, "Research source".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let original = agent.snapshot(source).await.unwrap();
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            source_loop_id: Some(receipt.loop_id),
            name: Some("Independent research".into()),
        })
        .await
        .unwrap();
    assert_ne!(source, branch.child_chat_id);
    agent
        .submit(branch.child_chat_id, None, "Continue separately".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let requests = observed.await.unwrap();
    let copied = requests[1]["input"].to_string();
    assert!(copied.contains("Hello from Codex"));
    assert!(!copied.contains("opaque"));
    assert_eq!(
        agent.snapshot(source).await.unwrap().messages,
        original.messages
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn automatic_compaction_uses_the_subscription_and_can_continue_a_real_file_tool() {
    let (_directory, state) = state_with_credentials();
    std::fs::write(state.join("evidence.txt"), "actual-file-marker").unwrap();
    let (endpoint, observed) = serve_scripted_responses(vec![
        (
            200,
            reply_stream("first", &"Older observation ".repeat(40_000), 20_000),
        ),
        (
            200,
            reply_stream("summary", "Summary: read evidence.txt", 40),
        ),
        (
            200,
            function_call_stream("read", "Read", json!({"file_path":"evidence.txt"}), 45),
        ),
        (200, reply_stream("done", "File verified", 55)),
    ])
    .await;
    let agent = Agent::open_embedded(state.clone(), config(), client(&state, &endpoint)).unwrap();
    let chat = agent.create_chat(state.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Initial observation".into())
        .await
        .unwrap();
    completed(&mut events).await;
    agent
        .submit(chat, None, "Read evidence.txt".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .latest_compact_summary()
            .is_some_and(|summary| summary.contains("evidence.txt"))
    );
    let requests = observed.await.unwrap();
    assert!(requests[1]["tools"].is_null());
    assert!(
        requests[3]["input"]
            .to_string()
            .contains("actual-file-marker")
    );
    assert!(
        snapshot
            .request_usage
            .entries()
            .iter()
            .any(|entry| entry.kind == RequestKind::CompactionAutomatic)
    );
    agent.shutdown().await;
}

#[test]
fn recovery_child() {
    let Some(state) = std::env::var_os("TICKLOOP_TEST_CODEX_HISTORY_ROOT") else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let state = std::path::PathBuf::from(state);
        let chat = std::env::var("TICKLOOP_TEST_CODEX_HISTORY_CHAT").unwrap().parse().unwrap();
        let endpoint = std::env::var("TICKLOOP_TEST_CODEX_RESPONSES").unwrap().parse().unwrap();
        let agent = Agent::open_embedded(state.clone(), config(), client(&state, &endpoint)).unwrap();
        let restored = agent.resume_chat(chat).await.unwrap();
        assert!(restored.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text } if text.contains("Hello from Codex"))));
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Read restart-marker.txt after process restart".into()).await.unwrap();
        completed(&mut events).await;
        agent.shutdown().await;
    });
}

#[tokio::test]
async fn new_process_restores_transcript_with_full_history_and_no_previous_connection() {
    let (_directory, state) = state_with_credentials();
    std::fs::write(state.join("restart-marker.txt"), "restart-file-proof").unwrap();
    let (endpoint, observed) = serve_scripted_responses(vec![
        (200, text_stream()),
        (
            200,
            function_call_stream(
                "restart",
                "Read",
                json!({"file_path":"restart-marker.txt"}),
                25,
            ),
        ),
        (200, reply_stream("done", "Recovered and read the file", 35)),
    ])
    .await;
    let agent = Agent::open_embedded(state.clone(), config(), client(&state, &endpoint)).unwrap();
    let chat = agent.create_chat(state.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Remember the initial observation".into())
        .await
        .unwrap();
    completed(&mut events).await;
    agent.shutdown().await;
    drop(agent);
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "model::codex::history_tests::recovery_child",
            "--nocapture",
        ])
        .env("TICKLOOP_TEST_CODEX_HISTORY_ROOT", &state)
        .env("TICKLOOP_TEST_CODEX_HISTORY_CHAT", chat.to_string())
        .env("TICKLOOP_TEST_CODEX_RESPONSES", endpoint.as_str())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests: Vec<Value> = tokio::time::timeout(Duration::from_secs(8), observed)
        .await
        .unwrap()
        .unwrap();
    assert!(requests[1]["previous_response_id"].is_null());
    assert!(
        requests[1]["input"]
            .to_string()
            .contains("Hello from Codex")
    );
    assert!(
        requests[2]["input"]
            .to_string()
            .contains("restart-file-proof")
    );
}

#[tokio::test]
async fn failed_subscription_compaction_preserves_the_last_valid_history() {
    let (_directory, state) = state_with_credentials();
    let (endpoint, observed) = serve_scripted_responses(vec![
        (200, text_stream()),
        (401, json!({"error":{"code":"token_expired"}}).to_string()),
    ])
    .await;
    let agent = Agent::open_embedded(state.clone(), config(), client(&state, &endpoint)).unwrap();
    let chat = agent.create_chat(state.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Keep this progress".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let before = agent.snapshot(chat).await.unwrap();
    assert!(agent.compact(chat).await.is_err());
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.message_compactions, before.message_compactions);
    assert_eq!(observed.await.unwrap().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn websocket_history_survives_compaction_branch_and_compatible_rewind() {
    let (_directory, state) = state_with_credentials();
    std::fs::write(state.join("ws-history.txt"), "WS_HISTORY_MARKER").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = reqwest::Url::parse(&format!(
        "http://{}/codex/responses",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (captured_tx, captured_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut source_socket = accept_async(stream).await.unwrap();
        let mut source_requests = Vec::new();
        for reply in [
            text_stream(),
            function_call_stream(
                "ws-history-tool",
                "Read",
                json!({"file_path":"ws-history.txt"}),
                20,
            ),
            reply_stream("ws-history-tool-answer", "Tool answer", 25),
            reply_stream("ws-history-checkpoint", "Checkpoint answer", 30),
            reply_stream(
                "ws-history-summary",
                "Summary: remember WS_HISTORY_MARKER",
                35,
            ),
            reply_stream("ws-history-after-compact", "After compact answer", 20),
        ] {
            let request = source_socket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            source_requests.push(serde_json::from_str::<Value>(&request).unwrap());
            send_ws_stream(&mut source_socket, &reply).await;
        }
        let (stream, _) = listener.accept().await.unwrap();
        let mut branch_socket = accept_async(stream).await.unwrap();
        let branch_request = branch_socket
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        send_ws_stream(
            &mut branch_socket,
            &reply_stream("ws-history-branch", "Branch answer", 18),
        )
        .await;
        let rewind_request = source_socket
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        source_requests.push(serde_json::from_str(&rewind_request).unwrap());
        send_ws_stream(
            &mut source_socket,
            &reply_stream("ws-history-rewind", "Rewind answer", 24),
        )
        .await;
        captured_tx
            .send((
                source_requests,
                serde_json::from_str::<Value>(&branch_request).unwrap(),
            ))
            .unwrap();
    });
    let client = Arc::new(
        CodexModelClient::new(config(), &state)
            .unwrap()
            .with_loopback_test_endpoint(endpoint.as_str())
            .unwrap()
            .with_loopback_test_websocket(),
    );
    let agent = Agent::open_embedded(state.clone(), config(), client).unwrap();
    let source = agent.create_chat(state.clone()).await.unwrap();
    agent
        .set_execution_mode(source, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let mut completed_tools = 0;
    let first = agent
        .submit(source, None, "Remember this observation".into())
        .await
        .unwrap();
    completed(&mut events).await;
    agent
        .submit(source, Some(first.loop_id), "Read ws-history.txt".into())
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(8), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::ToolCompleted { .. } => completed_tools += 1,
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("WS tool request failed: {error}"),
            _ => {}
        }
    }
    agent
        .submit(source, None, "Checkpoint before compact".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let checkpoint = agent
        .snapshot(source)
        .await
        .unwrap()
        .rewind_points()
        .into_iter()
        .find(|point| point.prompt == "Checkpoint before compact")
        .unwrap();
    let compacted = agent.compact(source).await.unwrap();
    assert!(compacted.summary.contains("WS_HISTORY_MARKER"));
    let continued = agent
        .submit(source, None, "Continue after compact".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            source_loop_id: Some(continued.loop_id),
            name: Some("WS branch".into()),
        })
        .await
        .unwrap();
    assert_ne!(branch.child_chat_id, source);
    agent
        .submit(branch.child_chat_id, None, "Continue branch".into())
        .await
        .unwrap();
    completed(&mut events).await;
    agent
        .rewind(source, checkpoint.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    agent
        .submit(source, None, "Continue after rewind".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let (source_requests, branch_request) =
        tokio::time::timeout(Duration::from_secs(8), captured_rx)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(source_requests.len(), 7);
    assert_eq!(source_requests[0]["prompt_cache_key"], source.to_string());
    assert!(source_requests[1]["input"].to_string().contains("opaque"));
    assert!(
        source_requests[2]["input"]
            .to_string()
            .contains("WS_HISTORY_MARKER")
    );
    assert!(source_requests[4]["tools"].is_null());
    assert!(
        source_requests[5]["input"]
            .to_string()
            .contains("Summary: remember WS_HISTORY_MARKER")
    );
    assert!(source_requests[5]["previous_response_id"].is_null());
    assert_eq!(
        branch_request["prompt_cache_key"],
        branch.child_chat_id.to_string()
    );
    assert!(branch_request["previous_response_id"].is_null());
    assert!(
        branch_request["input"]
            .to_string()
            .contains("Summary: remember WS_HISTORY_MARKER")
    );
    let rewind = &source_requests[6];
    assert_eq!(rewind["prompt_cache_key"], source.to_string());
    assert!(rewind["previous_response_id"].is_null());
    assert!(rewind["input"].to_string().contains("WS_HISTORY_MARKER"));
    assert!(!rewind["input"].to_string().contains("After compact answer"));
    assert_eq!(completed_tools, 1);
    let branch_snapshot = agent.snapshot(branch.child_chat_id).await.unwrap();
    assert!(
        branch_snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::Text { text } if text == "Branch answer"))
    );
    agent.shutdown().await;
}
