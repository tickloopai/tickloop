use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message as WebSocketMessage,
        handshake::server::{Request as WebSocketRequest, Response as WebSocketResponse},
    },
};

use super::*;
use crate::model::ModelFailureKind;
use crate::{
    model::cards::built_in_model_profiles,
    permissions::{Decision, ExecutionMode},
    runtime::{Agent, EventKind},
};

struct Reply {
    status: u16,
    retry_after: Option<u64>,
    should_retry: Option<bool>,
    body: String,
}

async fn response_with_content_type(
    content_type: Option<&str>,
    body: String,
) -> Result<ModelOutput, ModelError> {
    let (_directory, state) = super::tests::state_with_credentials();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let content_type = content_type
        .map(|mime| format!("Content-Type: {mime}\r\n"))
        .unwrap_or_default();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let result = client(&state, &endpoint)
        .complete(request(), events, CancellationToken::new())
        .await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn codex_accepts_valid_sse_with_missing_or_inaccurate_content_type() {
    for mime in [None, Some("application/octet-stream"), Some("text/plain")] {
        let result = response_with_content_type(mime, super::tests::text_stream()).await;
        assert!(result.is_ok(), "{mime:?}: {result:?}");
    }
}

#[tokio::test]
async fn codex_rejects_non_stream_bodies_with_safe_response_diagnostics() {
    for (mime, body) in [
        ("text/html", "<html>private-response-data</html>"),
        ("application/json", "{\"secret\":\"private-response-data\"}"),
    ] {
        let error = response_with_content_type(Some(mime), body.into())
            .await
            .unwrap_err();
        assert!(matches!(error, ModelError::InvalidResponse(_)));
        let message = error.to_string();
        assert!(message.contains("HTTP 200"), "{message}");
        assert!(message.contains(mime), "{message}");
        assert!(!message.contains("private-response-data"), "{message}");
    }
}

#[tokio::test]
async fn codex_empty_terminal_output_uses_only_completed_text_and_tool_items() {
    for is_tool in [false, true] {
        for complete_item in [true, false] {
            let item = if is_tool {
                json!({"id":"fc-1","type":"function_call","call_id":"call-1","name":"Read","arguments":"{}"})
            } else {
                json!({"id":"msg-1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"OK","annotations":[]}]})
            };
            let mut initial = item.clone();
            if is_tool {
                initial["arguments"] = json!("");
            } else {
                initial["content"] = json!([]);
                initial["status"] = json!("in_progress");
            }
            let mut events = vec![
                json!({"type":"response.created","response":{"id":"resp-1","status":"in_progress"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":initial}),
                if is_tool {
                    json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":item["id"],"delta":"{}"})
                } else {
                    json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":item["id"],"delta":"OK"})
                },
            ];
            if complete_item {
                events
                    .push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
            }
            events.push(json!({"type":"response.completed","response":{"id":"resp-1","status":"completed","output":[]}}));
            let stream = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect();
            let result = response_with_content_type(Some("text/plain"), stream).await;
            if complete_item {
                let output = result.unwrap();
                assert_eq!(output.content.len(), 1);
                if is_tool {
                    assert!(
                        matches!(&output.content[0], Content::ToolUse { name, input, .. } if name == "Read" && input == &json!({}))
                    );
                } else {
                    assert!(matches!(&output.content[0], Content::Text { text } if text == "OK"));
                }
            } else {
                assert!(result.is_err(), "unfinished items cannot complete");
            }
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, body_len) = loop {
        let read = socket.read(&mut buffer).await.unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let body_len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (header_end, body_len);
        }
    };
    let body_start = header_end + 4;
    let body_end = body_start + body_len;
    while bytes.len() < body_end {
        let read = socket.read(&mut buffer).await.unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&buffer[..read]);
    }
    (
        String::from_utf8(bytes[..header_end].to_vec()).unwrap(),
        bytes[body_start..body_end].to_vec(),
    )
}

async fn serve(replies: Vec<Reply>) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let server = tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let _ = read_request(&mut socket).await;
            observed.fetch_add(1, Ordering::SeqCst);
            let retry_header = reply
                .retry_after
                .map(|seconds| format!("Retry-After: {seconds}\r\n"))
                .unwrap_or_default();
            let retry_policy_header = reply
                .should_retry
                .map(|retry| format!("x-should-retry: {retry}\r\n"))
                .unwrap_or_default();
            let content_type = if reply.status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            };
            let headers = format!(
                "HTTP/1.1 {} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{retry_header}{retry_policy_header}Connection: close\r\n\r\n",
                reply.status,
                reply.body.len(),
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(reply.body.as_bytes()).await.unwrap();
        }
    });
    (endpoint, count, server)
}

fn client(state_dir: &Path, endpoint: &str) -> CodexModelClient {
    let config = built_in_model_profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    CodexModelClient::new(config, state_dir)
        .unwrap()
        .with_loopback_test_endpoint(endpoint)
        .unwrap()
}

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![crate::model::Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Hello".into(),
            }],
        }],
        tools: Vec::new(),
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn real_http_errors_classify_auth_entitlement_quota_and_service() {
    let (_directory, state_dir) = super::tests::state_with_credentials();
    for (status, body, expected, retryable) in [
        (
            401,
            json!({"error":{"message":"bearer-secret"}}),
            "authentication failed",
            false,
        ),
        (
            403,
            json!({"error":{"type":"usage_not_included","message":"bearer-secret"}}),
            "not included",
            false,
        ),
        (
            429,
            json!({"error":{"type":"usage_limit_reached","resets_at":1790000000,"message":"bearer-secret"}}),
            "resets at 2026-09-21T14:13:20Z",
            false,
        ),
        (
            503,
            json!({"error":{"message":"bearer-secret"}}),
            "temporarily unavailable",
            true,
        ),
    ] {
        let (endpoint, count, server) = serve(vec![Reply {
            status,
            retry_after: None,
            should_retry: None,
            body: body.to_string(),
        }])
        .await;
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let error = client(&state_dir, &endpoint)
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        server.await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("bearer-secret"));
        assert_eq!(error.stage(), ModelRequestStage::ResponseHeaders);
        if status != 401 {
            assert_eq!(error.status(), Some(status));
        }
        assert_eq!(error.retry_hint().0, retryable);
        assert_eq!(
            error.kind(),
            match status {
                401 => ModelFailureKind::AuthenticationFailed,
                403 => ModelFailureKind::EntitlementDenied,
                429 => ModelFailureKind::QuotaExhausted,
                _ => ModelFailureKind::ProviderFailure,
            }
        );
    }
}

#[tokio::test]
async fn quota_failure_is_terminal_in_agent_with_one_http_request() {
    let (_directory, state_dir) = super::tests::state_with_credentials();
    let (endpoint, count, server) = serve(vec![Reply {
        status: 429,
        retry_after: Some(30),
        should_retry: None,
        body: json!({"error":{"type":"usage_limit_reached","resets_at":1790000000,"message":"bearer-secret"}}).to_string(),
    }])
    .await;
    let model = client(&state_dir, &endpoint);
    let config = model.config.clone();
    let agent = Agent::new_embedded(config, Arc::new(model));
    let chat = agent.create_chat(state_dir).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Hello".into()).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Failed(reason) => {
                assert!(reason.contains("resets at 2026-09-21T14:13:20Z"));
                assert!(!reason.contains("bearer-secret"));
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
    let (_directory, state_dir) = super::tests::state_with_credentials();
    let (endpoint, count, server) = serve(vec![Reply {
        status: 503,
        retry_after: Some(30),
        should_retry: Some(false),
        body: json!({"error":{"message":"bearer-secret"}}).to_string(),
    }])
    .await;
    let model = client(&state_dir, &endpoint);
    let config = model.config.clone();
    let agent = Agent::new_embedded(config, Arc::new(model));
    let chat = agent.create_chat(state_dir).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Hello".into()).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Failed(reason) => {
                assert!(reason.contains("HTTP 503"));
                assert!(!reason.contains("bearer-secret"));
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
    let (_directory, state_dir) = super::tests::state_with_credentials();
    let (endpoint, count, server) = serve(vec![
        Reply {
            status: 429,
            retry_after: Some(1),
            should_retry: None,
            body: json!({"error":{"type":"rate_limit_error"}}).to_string(),
        },
        Reply {
            status: 200,
            retry_after: None,
            should_retry: None,
            body: super::tests::text_stream(),
        },
    ])
    .await;
    let model = client(&state_dir, &endpoint);
    let config = model.config.clone();
    let agent = Agent::new_embedded(config, Arc::new(model));
    let chat = agent.create_chat(state_dir).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Hello".into()).await.unwrap();
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
    let (_directory, state_dir) = super::tests::state_with_credentials();
    let (endpoint, count, server) = serve(vec![Reply {
        status: 429,
        retry_after: Some(30),
        should_retry: None,
        body: json!({"error":{"type":"rate_limit_error"}}).to_string(),
    }])
    .await;
    let model = client(&state_dir, &endpoint);
    let config = model.config.clone();
    let agent = Agent::new_embedded(config, Arc::new(model));
    let chat = agent.create_chat(state_dir).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Hello".into()).await.unwrap();
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
    let (requests, kinds, reason) = super::tests::blocked_auto_review_case(vec![(
        429,
        json!({"error":{"type":"usage_limit_reached","resets_at":1790000000,"message":"bearer-secret"}}).to_string(),
    )])
    .await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == crate::runtime::RequestKind::PermissionCheck)
            .count(),
        1
    );
    assert!(reason.contains("resets at 2026-09-21T14:13:20Z"));
    assert!(!reason.contains("bearer-secret"));
}

#[tokio::test]
#[allow(clippy::result_large_err)]
async fn auto_review_refreshes_login_and_denies_after_websocket_output_breaks() {
    const REFRESHED_ACCESS_TOKEN: &str = "ey.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1uZXcifX0.sig";
    let (_directory, state_dir) = super::tests::state_with_credentials();
    super::tests::rewrite_credentials(&state_dir, "expired-test-token", "old-account", 0);

    let oauth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oauth_origin = format!("http://{}", oauth_listener.local_addr().unwrap());
    let oauth_server = tokio::spawn(async move {
        let (mut socket, _) = oauth_listener.accept().await.unwrap();
        let (headers, body) = read_request(&mut socket).await;
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

    let (main_endpoint, main_requests) = super::tests::serve_scripted_responses(vec![
        (
            200,
            super::tests::function_call_stream(
                "propose",
                "Bash",
                json!({"command":"ln -s missing-target blocked"}),
                12,
            ),
        ),
        (
            200,
            super::tests::reply_stream("after-denial", "I will continue without the link", 18),
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
                json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_review","call_id":"call_review","name":"classify_result","arguments":""}})
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

    let profiles = built_in_model_profiles();
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
    let main_client = CodexModelClient::new(main_config.clone(), &state_dir)
        .unwrap()
        .with_loopback_test_endpoint(main_endpoint.as_str())
        .unwrap()
        .with_loopback_test_auth(&state_dir, &oauth_origin)
        .unwrap();
    let reviewer_client = CodexModelClient::new(reviewer_config, &state_dir)
        .unwrap()
        .with_loopback_test_endpoint(&ws_endpoint)
        .unwrap()
        .with_loopback_test_websocket()
        .with_loopback_test_auth(&state_dir, &oauth_origin)
        .unwrap();
    let agent = Agent::new_with_options(
        main_config,
        Arc::new(main_client),
        crate::runtime::AgentHostOptions {
            prompt_policy: crate::runtime::PromptPolicy::DenyOnPrompt,
            ..crate::runtime::AgentHostOptions::embedded_defaults()
        },
    );
    agent.set_auto_reviewer(Arc::new(reviewer_client)).await;
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            chat,
            None,
            "Inspect the directory without making changes".into(),
        )
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
