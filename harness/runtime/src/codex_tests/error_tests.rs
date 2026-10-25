use std::{path::Path, sync::atomic::Ordering, time::Duration};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpListener};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message as WebSocketMessage,
        handshake::server::{Request as WebSocketRequest, Response as WebSocketResponse},
    },
};

use crate::Agent;
use tickloop_harness_model::{cards::ModelCatalog, codex_test_support::*};
use tickloop_harness_protocol::{
    AgentHostOptions, Decision, EventKind, ExecutionMode, ModelProvider, ModelRequestKind,
    PromptPolicy,
};

fn agent_with_codex(state_dir: &Path, endpoint: &str) -> Agent {
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    Agent::new_embedded(
        config.clone(),
        LoopbackCodexClient::new(config, state_dir, endpoint).into_fixture_client(),
    )
}

#[tokio::test]
async fn quota_failure_is_terminal_in_agent_with_one_http_request() {
    let (_directory, state_dir) = state_with_credentials();
    let (endpoint, count, server) = serve_http_replies(vec![HttpReply {
        status: 429,
        retry_after: Some(30),
        should_retry: None,
        body: json!({"error":{"type":"usage_limit_reached","resets_at":1790000000,"message":"local-test-access-token"}}).to_string(),
    }])
    .await;
    let agent = agent_with_codex(&state_dir, &endpoint);
    let chat = agent.create_chat(state_dir).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Hello".into()).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Failed(reason) => {
                assert!(reason.contains("resets at 2026-09-21T14:13:20Z"));
                assert!(!reason.contains("local-test-access-token"));
                break;
            }
            EventKind::ModelRetry { .. } => panic!("quota must not retry"),
            _ => {}
        }
    }
    server.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn service_retry_false_stops_agent_after_one_request() {
    let (_directory, state_dir) = state_with_credentials();
    let (endpoint, count, server) = serve_http_replies(vec![HttpReply {
        status: 503,
        retry_after: Some(30),
        should_retry: Some(false),
        body: json!({"error":{"message":"local-test-access-token"}}).to_string(),
    }])
    .await;
    let agent = agent_with_codex(&state_dir, &endpoint);
    let chat = agent.create_chat(state_dir).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Hello".into()).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Failed(reason) => {
                assert!(reason.contains("HTTP 503"));
                assert!(!reason.contains("local-test-access-token"));
                break;
            }
            EventKind::ModelRetry { .. } => panic!("server explicitly disabled retry"),
            _ => {}
        }
    }
    server.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn transient_429_retries_once_using_retry_after() {
    let (_directory, state_dir) = state_with_credentials();
    let (endpoint, count, server) = serve_http_replies(vec![
        HttpReply {
            status: 429,
            retry_after: Some(1),
            should_retry: None,
            body: json!({"error":{"type":"rate_limit_error"}}).to_string(),
        },
        HttpReply {
            status: 200,
            retry_after: None,
            should_retry: None,
            body: text_stream(),
        },
    ])
    .await;
    let agent = agent_with_codex(&state_dir, &endpoint);
    let chat = agent.create_chat(state_dir).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Hello".into()).await.unwrap();
    let mut retries = 0;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::ModelRetry { delay_ms, .. } => {
                assert_eq!(delay_ms, 1_000);
                retries += 1;
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(reason) => panic!("transient request failed: {reason}"),
            _ => {}
        }
    }
    server.await.unwrap();
    assert_eq!(retries, 1);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn retry_after_wait_is_cancelled_before_another_http_request() {
    let (_directory, state_dir) = state_with_credentials();
    let (endpoint, count, server) = serve_http_replies(vec![HttpReply {
        status: 429,
        retry_after: Some(30),
        should_retry: None,
        body: json!({"error":{"type":"rate_limit_error"}}).to_string(),
    }])
    .await;
    let agent = agent_with_codex(&state_dir, &endpoint);
    let chat = agent.create_chat(state_dir).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Hello".into()).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        if let EventKind::ModelRetry { delay_ms, .. } = event.kind {
            assert_eq!(delay_ms, 30_000);
            break;
        }
    }
    agent.interrupt(chat).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Interrupted => break,
            EventKind::Failed(reason) => panic!("cancellation failed: {reason}"),
            _ => {}
        }
    }
    server.await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn quota_reset_is_shown_when_auto_review_denies_protected_tool() {
    let (requests, kinds, reason) = super::blocked_auto_review_case(vec![(
        429,
        json!({"error":{"type":"usage_limit_reached","resets_at":1790000000,"message":"local-test-access-token"}}).to_string(),
    )])
    .await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == ModelRequestKind::PermissionCheck)
            .count(),
        1
    );
    assert!(reason.contains("resets at 2026-09-21T14:13:20Z"));
    assert!(!reason.contains("local-test-access-token"));
}

#[tokio::test]
#[allow(clippy::result_large_err)]
async fn auto_review_refreshes_login_and_denies_after_websocket_output_breaks() {
    const REFRESHED_ACCESS_TOKEN: &str = "ey.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1uZXcifX0.sig";
    let (_directory, state_dir) = state_with_credentials();
    rewrite_credentials(&state_dir, "expired-test-token", "old-account", 0);

    let oauth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oauth_origin = format!("http://{}", oauth_listener.local_addr().unwrap());
    let oauth_server = tokio::spawn(async move {
        let (mut socket, _) = oauth_listener.accept().await.unwrap();
        let (headers, body) = read_raw_request(&mut socket).await;
        assert!(headers.starts_with("POST /oauth/token HTTP/1.1"));
        let form = String::from_utf8(body).unwrap();
        assert!(form.contains("grant_type=refresh_token"));
        assert!(form.contains("refresh_token=local-test-refresh-token"));
        let body = json!({
            "access_token": REFRESHED_ACCESS_TOKEN,
            "refresh_token": "new-test-refresh-token",
            "expires_in": 3600,
        })
        .to_string();
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });

    let (main_endpoint, main_requests) = serve_scripted_responses(vec![
        (
            200,
            function_call_stream(
                "propose",
                "Bash",
                json!({"command":"ln -s missing-target blocked"}),
                12,
            ),
        ),
        (
            200,
            reply_stream("after-denial", "I will continue without the link", 18),
        ),
    ])
    .await;

    let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_endpoint = format!(
        "http://{}/codex/responses",
        ws_listener.local_addr().unwrap()
    );
    let (handshake_sender, handshake_receiver) = tokio::sync::oneshot::channel();
    let ws_server = tokio::spawn(async move {
        let (stream, _) = ws_listener.accept().await.unwrap();
        let mut authorization = String::new();
        let mut account_id = String::new();
        let mut socket = accept_hdr_async(
            stream,
            |request: &WebSocketRequest, response: WebSocketResponse| {
                authorization = request.headers()["authorization"].to_str().unwrap().into();
                account_id = request.headers()["chatgpt-account-id"]
                    .to_str()
                    .unwrap()
                    .into();
                Ok(response)
            },
        )
        .await
        .unwrap();
        let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let request: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(request["model"], "gpt-5.6-sol");
        handshake_sender.send((authorization, account_id)).unwrap();
        socket
            .send(WebSocketMessage::Text(
                json!({"type":"response.created","sequence_number":0,"response":{"id":"review-broken","status":"in_progress"}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        socket
            .send(WebSocketMessage::Text(
                json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_review","role":"assistant","status":"in_progress","content":[]}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        drop(socket);
        tokio::time::timeout(Duration::from_millis(200), ws_listener.accept())
            .await
            .is_ok()
    });

    let profiles = ModelCatalog::defaults().profiles();
    let main_config = profiles
        .iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap()
        .clone();
    let reviewer_config = profiles
        .iter()
        .find(|profile| {
            profile.model == "gpt-5.6-sol" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap()
        .clone();
    let main_client =
        LoopbackCodexClient::new(main_config.clone(), &state_dir, main_endpoint.as_str())
            .with_oauth_origin(&state_dir, &oauth_origin);
    let reviewer_client = LoopbackCodexClient::new(reviewer_config, &state_dir, &ws_endpoint)
        .with_websocket()
        .with_oauth_origin(&state_dir, &oauth_origin);
    let agent = Agent::new_with_options(
        main_config,
        main_client.into_fixture_client(),
        AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::embedded_defaults()
        },
    );
    agent
        .set_auto_reviewer(reviewer_client.into_fixture_client())
        .await;
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Inspect the directory without making changes".into())
        .await
        .unwrap();
    let mut denial = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::PermissionChecked { decision, reason } => {
                assert_eq!(decision, Decision::Deny);
                denial = Some(reason);
            }
            EventKind::Completed { .. } => break,
            EventKind::ToolStarted { .. } => panic!("protected operation was admitted"),
            EventKind::ModelRetry { .. } => panic!("output-started review failure was retried"),
            EventKind::PermissionRequested { .. } => panic!("review requested user input"),
            EventKind::Failed(reason) => panic!("Agent failed instead of denying tool: {reason}"),
            _ => {}
        }
    }
    let (authorization, account_id) = handshake_receiver.await.unwrap();
    assert_eq!(authorization, format!("Bearer {REFRESHED_ACCESS_TOKEN}"));
    assert_eq!(account_id, "acct-new");
    assert!(denial.unwrap().contains("action was not run"));
    assert!(state_dir.join("blocked").symlink_metadata().is_err());
    assert_eq!(main_requests.await.unwrap().len(), 2);
    assert!(
        !ws_server.await.unwrap(),
        "review retried or fell back to SSE"
    );
    oauth_server.await.unwrap();
    agent.shutdown().await;
}
