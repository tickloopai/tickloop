use std::sync::atomic::Ordering;

use serde_json::json;
use tokio::{io::AsyncWriteExt, net::TcpListener};

use super::*;
use crate::model::{ModelFailureKind, cards::ModelCatalog, codex_test_support::*};

async fn response_with_content_type(
    content_type: Option<&str>,
    body: String,
) -> Result<ModelOutput, ModelError> {
    let (_directory, state) = state_with_credentials();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let content_type = content_type
        .map(|mime| format!("Content-Type: {mime}\r\n"))
        .unwrap_or_default();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_raw_request(&mut socket).await;
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
        let result = response_with_content_type(mime, text_stream()).await;
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

fn client(state_dir: &Path, endpoint: &str) -> CodexModelClient {
    let config = ModelCatalog::defaults()
        .profiles()
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
        history_sources: Vec::new(),
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
    let (_directory, state_dir) = state_with_credentials();
    for (status, body, expected, retryable) in [
        (
            401,
            json!({"error":{"message":"local-test-access-token"}}),
            "expired or invalid",
            false,
        ),
        (
            403,
            json!({"error":{"type":"usage_not_included","message":"local-test-access-token"}}),
            "plan access",
            false,
        ),
        (
            429,
            json!({"error":{"type":"usage_limit_reached","resets_at":1790000000,"message":"local-test-access-token"}}),
            "resets at 2026-09-21T14:13:20Z",
            false,
        ),
        (
            503,
            json!({"error":{"message":"local-test-access-token"}}),
            "server error",
            true,
        ),
    ] {
        let (endpoint, count, server) = serve_http_replies(vec![HttpReply {
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
        assert!(!error.to_string().contains("local-test-access-token"));
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
async fn websocket_http_refusals_preserve_details_without_blind_http_fallback() {
    let (_directory, state) = state_with_credentials();
    for (status, code, class) in [
        (
            429,
            "insufficient_quota",
            crate::model::ProviderFailureClass::UsageLimit,
        ),
        (
            402,
            "billing_error",
            crate::model::ProviderFailureClass::Billing,
        ),
        (
            403,
            "usage_not_included",
            crate::model::ProviderFailureClass::ModelUnavailable,
        ),
        (
            401,
            "authentication_error",
            crate::model::ProviderFailureClass::Authentication,
        ),
        (
            429,
            "rate_limit_error",
            crate::model::ProviderFailureClass::RateLimit,
        ),
        (
            503,
            "api_error",
            crate::model::ProviderFailureClass::ServerError,
        ),
    ] {
        let (endpoint, count, server) = serve_http_replies(vec![HttpReply {status, retry_after: Some(7), should_retry: Some(false),
            body: json!({"error":{"code":code,"message":"Provider explanation local-test-access-token"}}).to_string()}]).await;
        let mut client = client(&state, &endpoint);
        client.prefer_websocket = true;
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        server.await.unwrap();
        let detail = error.provider_failure().unwrap();
        assert_eq!(detail.class, class, "{error}");
        assert_eq!(detail.status, status);
        assert!(
            !error.retry_hint().0,
            "explicit server retry policy must be honored"
        );
        assert!(error.to_string().contains("Provider explanation"));
        assert!(!format!("{error:?}").contains("local-test-access-token"));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn split_http_rejections_are_classified_after_the_entire_bounded_body_arrives() {
    let (_directory, state) = state_with_credentials();
    for (status, code, class) in [
        (
            429,
            "insufficient_quota",
            crate::model::ProviderFailureClass::UsageLimit,
        ),
        (
            402,
            "billing_error",
            crate::model::ProviderFailureClass::Billing,
        ),
        (
            400,
            "insufficient_quota",
            crate::model::ProviderFailureClass::UsageLimit,
        ),
        (
            404,
            "model_not_found",
            crate::model::ProviderFailureClass::ModelUnavailable,
        ),
        (
            501,
            "insufficient_quota",
            crate::model::ProviderFailureClass::UsageLimit,
        ),
    ] {
        for chunked in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
            let body=json!({"error":{"code":code,"message":"Delayed provider explanation local-test-access-token"}}).to_string();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (headers, _) = read_raw_request(&mut socket).await;
                assert!(headers.to_ascii_lowercase().contains("upgrade: websocket"));
                let framing = if chunked {
                    "Transfer-Encoding: chunked\r\n".into()
                } else {
                    format!("Content-Length: {}\r\n", body.len())
                };
                socket.write_all(format!("HTTP/1.1 {status} Test\r\n{framing}Content-Type: application/json\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
                for chunk in body.as_bytes().chunks(11) {
                    if chunked {
                        socket
                            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                            .await
                            .unwrap();
                    }
                    if socket.write_all(chunk).await.is_err() {
                        break;
                    }
                    if chunked {
                        let _ = socket.write_all(b"\r\n").await;
                    }
                    tokio::task::yield_now().await;
                }
                if chunked {
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                }
                drop(socket);
                tokio::time::timeout(Duration::from_millis(250), listener.accept())
                    .await
                    .is_ok()
            });
            let mut client = client(&state, &endpoint);
            client.prefer_websocket = true;
            let (events, _) = tokio::sync::mpsc::unbounded_channel();
            let error = client
                .complete(request(), events, CancellationToken::new())
                .await
                .unwrap_err();
            let fallback = server.await.unwrap();
            assert!(
                !fallback,
                "a declared rejection body must not be mistaken for an empty unsupported upgrade"
            );
            let detail = error
                .provider_failure()
                .expect("HTTP rejection must remain an HTTP error");
            assert_eq!(detail.class, class, "{error}");
            assert!(
                !error.retry_hint().0,
                "quota/billing/model access must not retry"
            );
            assert!(
                error.to_string().contains("Delayed provider explanation"),
                "{error}"
            );
            assert!(!format!("{error:?}").contains("local-test-access-token"));
        }
    }
}

#[tokio::test]
async fn confirmation_incomplete_rejection_bodies_fail_closed() {
    let (_directory, state) = state_with_credentials();
    let mut failures = Vec::new();
    for websocket in [true, false] {
        for status in [429, 500, 501, 402] {
            for mode in ["empty", "truncated", "chunked-truncated", "over-bound"] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
                let body = if mode == "over-bound" {
                    json!({"error":{"message":"x".repeat(16400),"code":"insufficient_quota"}})
                        .to_string()
                } else {
                    json!({"error":{"message":"account limit local-test-access-token","code":"insufficient_quota"}}).to_string()
                };
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let _ = read_raw_request(&mut socket).await;
                    let framing = if mode == "chunked-truncated" {
                        "Transfer-Encoding: chunked\r\n".into()
                    } else {
                        format!("Content-Length: {}\r\n", body.len())
                    };
                    socket
                        .write_all(
                            format!("HTTP/1.1 {status} Test\r\n{framing}Connection: close\r\n\r\n")
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    match mode {
                        "empty" => {}
                        "truncated" => {
                            let _ = socket.write_all(b"{").await;
                        }
                        "chunked-truncated" => {
                            let _ = socket.write_all(b"1\r\n{\r\n").await;
                        }
                        _ => {
                            let _ = socket.write_all(body.as_bytes()).await;
                        }
                    }
                    drop(socket);
                    if let Ok(Ok((mut fallback, _))) =
                        tokio::time::timeout(Duration::from_millis(250), listener.accept()).await
                    {
                        let _ = read_raw_request(&mut fallback).await;
                        let _=fallback.write_all(b"HTTP/1.1 429 Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await;
                        true
                    } else {
                        false
                    }
                });
                let mut c = client(&state, &endpoint);
                c.prefer_websocket = websocket;
                let (events, _) = tokio::sync::mpsc::unbounded_channel();
                let error = c
                    .complete(request(), events, CancellationToken::new())
                    .await
                    .unwrap_err();
                let fallback = server.await.unwrap();
                eprintln!(
                    "INCOMPLETE ws={websocket} status={status} mode={mode} class={:?} retry={} fallback={fallback}",
                    error.provider_failure().map(|f| f.class),
                    error.retry_hint().0
                );
                assert!(!format!("{error:?}").contains("local-test-access-token"));
                if error.retry_hint().0 || fallback {
                    failures.push((websocket, status, mode));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "incomplete/over-bound errors must fail closed: {failures:?}"
    );
}
#[tokio::test]
async fn confirmation_ws_rejection_body_timeout_fails_closed() {
    let (_directory, state) = state_with_credentials();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_raw_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 429 Test\r\nContent-Length: 100\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(16)).await;
        drop(socket);
    });
    let mut c = client(&state, &endpoint);
    c.prefer_websocket = true;
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let error = c
        .complete(request(), events, CancellationToken::new())
        .await
        .unwrap_err();
    eprintln!("TIMEOUT retry={}, error={error}", error.retry_hint().0);
    assert!(!error.retry_hint().0);
    assert!(error.to_string().contains("incomplete"));
    server.await.unwrap();
}
