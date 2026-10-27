use tickloop_code_local_environment::AgentLocalExt;
// Agent runs against numeric-loopback Codex subscription servers.

mod error_tests;
mod history_tests;
mod tool_locality_tests;

use std::{collections::HashSet, fs, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::oneshot};
use tokio_tungstenite::{accept_async, tungstenite::Message as WsMessage};

use tickloop_harness::model::{cards::ModelCatalog, codex_test_support::*};
use tickloop_harness::protocol::{
    AgentHostOptions, Content, Decision, Event, EventKind, ExecutionMode, ModelProvider,
    ModelRequestKind, PromptPolicy, Protocol,
};
use tickloop_harness::runtime::Agent;

async fn serve_codex_response() -> (Url, oneshot::Receiver<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!(
        "http://{}/codex/responses",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, request) = read_request(&mut socket).await;
        sender.send((headers, request)).unwrap();
        let stream = text_stream();
        send_stream(&mut socket, 200, &stream).await;
    });
    (endpoint, receiver)
}

async fn completed_agent_turn(events: &mut tokio::sync::broadcast::Receiver<Event>) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(reason) => panic!("Codex WS Agent request failed: {reason}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn agent_websocket_reuses_chat_connection_for_tools_and_continuation() {
    let (_directory, state_dir) = state_with_credentials();
    fs::write(state_dir.join("first.txt"), "FIRST_WS_MARKER").unwrap();
    fs::write(state_dir.join("second.txt"), "SECOND_WS_MARKER").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let (captured_tx, captured_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let mut frames = Vec::new();
        for reply in [
            tool_stream(),
            reply_stream("ws-tools", "Files read", 30),
            reply_stream("ws-followup", "Both markers remain", 12),
        ] {
            let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
            frames.push(serde_json::from_str::<Value>(&frame).unwrap());
            send_ws_stream(&mut socket, &reply).await;
        }
        captured_tx.send(frames).unwrap();
    });
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, &endpoint).with_websocket();
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Read both files".into()).await.unwrap();
    completed_agent_turn(&mut events).await;
    agent
        .submit(chat, "Repeat their markers".into())
        .await
        .unwrap();
    completed_agent_turn(&mut events).await;
    let frames = tokio::time::timeout(Duration::from_secs(5), captured_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0]["type"], "response.create");
    assert_eq!(frames[0]["prompt_cache_key"], chat.uuid().to_string());
    assert_eq!(frames[0]["store"], false);
    assert_eq!(frames[1]["previous_response_id"], "resp-tools");
    assert!(frames[2]["previous_response_id"].is_null());
    assert_ne!(frames[1]["instructions"], frames[2]["instructions"]);
    assert!(
        frames[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output")
    );
    assert!(frames[1]["input"].as_array().unwrap().iter().any(|item| {
        item["output"]
            .as_str()
            .is_some_and(|text| text.contains("FIRST_WS_MARKER"))
    }));
    agent.shutdown().await;
}

#[tokio::test]
async fn concurrent_agent_chats_keep_websocket_tools_and_results_separate() {
    let (_directory, state_dir) = state_with_credentials();
    fs::write(state_dir.join("parallel-a.txt"), "PARALLEL_A_MARKER").unwrap();
    fs::write(state_dir.join("parallel-b.txt"), "PARALLEL_B_MARKER").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, &endpoint).with_websocket();
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    let first_chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let second_chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (captured_tx, captured_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut connections = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let request: Value = serde_json::from_str(&frame).unwrap();
            let chat =
                uuid::Uuid::parse_str(request["prompt_cache_key"].as_str().unwrap()).unwrap();
            assert!(chat == first_chat || chat == second_chat);
            connections.push((chat, socket, request));
        }
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        for (chat, socket, _) in &mut connections {
            let path = if *chat == first_chat {
                "parallel-a.txt"
            } else {
                "parallel-b.txt"
            };
            send_ws_stream(
                socket,
                &function_call_stream("parallel", "Read", json!({"file_path":path}), 8),
            )
            .await;
        }
        let mut captured = Vec::new();
        for (chat, mut socket, first) in connections {
            let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let continuation: Value = serde_json::from_str(&frame).unwrap();
            let answer = if chat == first_chat {
                "A_DONE"
            } else {
                "B_DONE"
            };
            send_ws_stream(&mut socket, &reply_stream("parallel-reply", answer, 12)).await;
            captured.push((chat, first, continuation));
        }
        captured_tx.send(captured).unwrap();
    });
    let mut events = agent.subscribe();
    agent
        .submit(first_chat, "Read parallel-a.txt".into())
        .await
        .unwrap();
    agent
        .submit(second_chat, "Read parallel-b.txt".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .unwrap()
        .unwrap();
    release_tx.send(()).unwrap();
    let mut completed = HashSet::new();
    let mut tool_completed = HashSet::new();
    while completed.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::ToolCompleted { .. } => {
                tool_completed.insert(event.chat_id);
            }
            EventKind::Completed { .. } => {
                completed.insert(event.chat_id);
            }
            EventKind::Failed(reason) => panic!("concurrent Codex Chat failed: {reason}"),
            _ => {}
        }
    }
    assert_eq!(tool_completed, HashSet::from([first_chat, second_chat]));
    let captured = captured_rx.await.unwrap();
    assert_eq!(captured.len(), 2);
    for (chat, first, continuation) in captured {
        let (own, foreign) = if chat == first_chat {
            ("PARALLEL_A_MARKER", "PARALLEL_B_MARKER")
        } else {
            ("PARALLEL_B_MARKER", "PARALLEL_A_MARKER")
        };
        assert_eq!(first["prompt_cache_key"], chat.to_string());
        assert!(first["previous_response_id"].is_null());
        assert_eq!(continuation["prompt_cache_key"], chat.to_string());
        let results = continuation["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 1);
        let output = results[0]["output"].as_str().unwrap();
        assert!(output.contains(own));
        assert!(!output.contains(foreign));
        let snapshot = agent.snapshot(chat.into()).await.unwrap();
        let text = snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(if chat == first_chat {
            "A_DONE"
        } else {
            "B_DONE"
        }));
        assert!(!text.contains(if chat == first_chat {
            "B_DONE"
        } else {
            "A_DONE"
        }));
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_falls_back_to_sse_only_before_websocket_output() {
    let (_directory, state_dir) = state_with_credentials();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let (captured_tx, captured_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = accept_async(stream).await.unwrap();
        let request = websocket
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        send_ws_stream(
            &mut websocket,
            &event(
                "response.created",
                0,
                json!({"response":{"id":"before-output","status":"in_progress"}}),
            ),
        )
        .await;
        websocket.close(None).await.unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, body) = read_request(&mut socket).await;
        send_stream(
            &mut socket,
            200,
            &reply_stream("sse-fallback", "Recovered through SSE", 8),
        )
        .await;
        captured_tx
            .send((
                serde_json::from_str::<Value>(&request).unwrap(),
                headers,
                body,
            ))
            .unwrap();
    });
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, &endpoint).with_websocket();
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Hello".into()).await.unwrap();
    completed_agent_turn(&mut events).await;
    let (ws, headers, sse) = captured_rx.await.unwrap();
    assert_eq!(ws["type"], "response.create");
    assert_eq!(sse["input"][1]["content"], "Hello");
    assert!(headers.to_ascii_lowercase().contains("session-id: "));
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_continues_from_shown_text_after_websocket_output_breaks() {
    let (_directory, state_dir) = state_with_credentials();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let (continuation_tx, continuation_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = accept_async(stream).await.unwrap();
        let _ = websocket.next().await.unwrap().unwrap();
        let partial = [
            event("response.created", 0, json!({"response":{"id":"partial","status":"in_progress"}})),
            event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"message","id":"msg-partial","role":"assistant","status":"in_progress","content":[]}})),
            event("response.output_text.delta", 2, json!({"output_index":0,"item_id":"msg-partial","content_index":0,"delta":"Partial"})),
        ].concat();
        send_ws_stream(&mut websocket, &partial).await;
        websocket.close(None).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = accept_async(stream).await.unwrap();
        let frame = websocket
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        continuation_tx
            .send(serde_json::from_str::<Value>(&frame).unwrap())
            .unwrap();
        send_ws_stream(&mut websocket, &text_stream()).await;
    });
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, &endpoint).with_websocket();
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Hello".into()).await.unwrap();
    let mut saw_partial = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Text(text) if text == "Partial" => saw_partial = true,
            EventKind::Failed(reason) => panic!("interrupted WebSocket reply failed: {reason}"),
            EventKind::Completed { .. } => break,
            _ => {}
        }
    }
    assert!(saw_partial);
    let continuation = continuation_rx.await.unwrap().to_string();
    assert!(continuation.contains("Partial"), "{continuation}");
    assert!(
        continuation.contains("stream was interrupted"),
        "{continuation}"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.messages.last().unwrap().content,
        vec![Content::Text {
            text: "PartialHello from Codex".into()
        }]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_conversation_uses_codex_credentials_and_streams_text_and_reasoning() {
    let (_directory, state_dir) = state_with_credentials();

    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let (endpoint, captured) = serve_codex_response().await;
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, endpoint.as_str());
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Say hello".into()).await.unwrap();
    let mut text = String::new();
    let mut thinking = String::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Text(delta) => text.push_str(&delta),
            EventKind::Thinking(delta) => thinking.push_str(&delta),
            EventKind::Completed { usage } => {
                assert_eq!(usage.input_tokens, Some(25));
                break;
            }
            EventKind::Failed(reason) => panic!("Agent request failed: {reason}"),
            _ => {}
        }
    }
    assert_eq!(text, "Hello from Codex");
    assert_eq!(thinking, "Considering");
    let (headers, body) = captured.await.unwrap();
    let headers = headers.to_ascii_lowercase();
    assert!(headers.starts_with("post /codex/responses http/1.1"));
    assert!(headers.contains("authorization: bearer local-test-access-token"));
    assert!(headers.contains("chatgpt-account-id: local-test-account"));
    assert!(headers.contains("originator: tickloop"));
    assert!(headers.contains("openai-beta: responses=experimental"));
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["input"][1]["content"], "Say hello");
    assert!(
        body["instructions"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(body["max_output_tokens"].is_null());
    assert!(
        body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(
                content,
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    ..
                }
            ))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_uses_replaced_account_on_its_next_real_model_request() {
    let (_directory, state_dir) = state_with_credentials();
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let (endpoint, captured) = serve_requests_with_headers(vec![
        text_stream(),
        reply_stream("after-account-switch", "New account", 30),
    ])
    .await;
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, endpoint.as_str());
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Hello".into()).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(reason) => panic!("first request failed: {reason}"),
            _ => {}
        }
    }
    rewrite_credentials(
        &state_dir,
        "local-test-access-token-2",
        "local-test-account-2",
        3_600_000,
    );
    agent.submit(chat, "Continue".into()).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(reason) => panic!("account replacement failed: {reason}"),
            _ => {}
        }
    }
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer local-test-access-token\r\n")
    );
    assert!(
        requests[0]
            .0
            .to_ascii_lowercase()
            .contains("chatgpt-account-id: local-test-account\r\n")
    );
    assert!(
        requests[1]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer local-test-access-token-2\r\n")
    );
    assert!(
        requests[1]
            .0
            .to_ascii_lowercase()
            .contains("chatgpt-account-id: local-test-account-2\r\n")
    );
    assert!(
        requests[1].1["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "reasoning")
    );
    assert!(
        requests[1].1["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["content"] == "Hello from Codex")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_sends_refreshed_token_and_account_to_the_model_service() {
    const REFRESHED_TOKEN: &str = "ey.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1uZXcifX0.sig";
    let (_directory, state_dir) = state_with_credentials();
    rewrite_credentials(&state_dir, "expired-token", "old-account", 1_000);
    let oauth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oauth_origin = format!("http://{}", oauth_listener.local_addr().unwrap());
    let (oauth_request_sender, oauth_request_receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = oauth_listener.accept().await.unwrap();
        let (headers, form) = read_raw_request(&mut socket).await;
        oauth_request_sender
            .send((headers, String::from_utf8(form).unwrap()))
            .unwrap();
        let response = json!({
            "access_token":REFRESHED_TOKEN,
            "refresh_token":"new-refresh-token",
            "expires_in":3600,
        })
        .to_string();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
    });
    let (endpoint, captured) =
        serve_requests_with_headers(vec![reply_stream("after-refresh", "Fresh credentials", 9)])
            .await;
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, endpoint.as_str())
        .with_oauth_origin(&state_dir, &oauth_origin);
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Hello".into()).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(reason) => panic!("refreshed Agent request failed: {reason}"),
            _ => {}
        }
    }
    let (oauth_headers, oauth_form) = oauth_request_receiver.await.unwrap();
    assert!(oauth_headers.starts_with("POST /oauth/token HTTP/1.1"));
    assert!(oauth_form.contains("grant_type=refresh_token"));
    assert!(oauth_form.contains("refresh_token=local-test-refresh-token"));
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 1);
    let headers = requests[0].0.to_ascii_lowercase();
    assert!(
        headers
            .contains(&format!("authorization: bearer {REFRESHED_TOKEN}\r\n").to_ascii_lowercase())
    );
    assert!(headers.contains("chatgpt-account-id: acct-new\r\n"));
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_reads_two_real_files_and_continues_with_paired_codex_tool_history() {
    let (_directory, state_dir) = state_with_credentials();
    fs::write(state_dir.join("first.txt"), "FRIDAY_MARKER_123").unwrap();
    fs::write(state_dir.join("second.txt"), "MONDAY_MARKER_456").unwrap();
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let (endpoint, captured) = serve_scripted_responses(vec![
        (200, tool_stream()),
        (200, reply_stream("reply-one", "Friday and Monday", 30)),
        (
            200,
            reply_stream("reply-two", "The same dates are Friday and Monday", 44),
        ),
    ])
    .await;
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, endpoint.as_str());
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    let chat = agent.create_chat(state_dir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read both files and tell me their dates".into())
        .await
        .unwrap();
    let mut tool_completions = 0;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::ToolCompleted { .. } => tool_completions += 1,
            EventKind::Completed { usage } => {
                assert_eq!(usage.input_tokens, Some(42));
                break;
            }
            EventKind::Failed(reason) => panic!("first Codex request failed: {reason}"),
            _ => {}
        }
    }
    assert_eq!(tool_completions, 2);
    agent
        .submit(chat, "What were the dates?".into())
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(reason) => panic!("followup Codex request failed: {reason}"),
            _ => {}
        }
    }
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 3);
    let tool_history = requests[1]["input"].as_array().unwrap();
    let calls = tool_history
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect::<Vec<_>>();
    let results = tool_history
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!(results.len(), 2);
    for index in 0..2 {
        assert_eq!(calls[index]["call_id"], results[index]["call_id"]);
        assert_eq!(calls[index]["id"], format!("fc_{}", index + 1));
    }
    assert!(
        results[0]["output"]
            .as_str()
            .unwrap()
            .contains("FRIDAY_MARKER_123")
    );
    assert!(
        results[1]["output"]
            .as_str()
            .unwrap()
            .contains("MONDAY_MARKER_456")
    );
    assert!(requests[2]["input"].as_array().unwrap().iter().any(|item| {
        item["type"] == "function_call_output"
            && item["output"]
                .as_str()
                .unwrap()
                .contains("FRIDAY_MARKER_123")
    }));
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text } if text == "The same dates are Friday and Monday")));
    agent.shutdown().await;
}

fn approval_findings(block: bool) -> Value {
    tickloop_harness::test_support::auto_classification(block)
}

#[tokio::test]
async fn codex_auto_review_allows_or_blocks_a_real_protected_operation() {
    for should_block in [false, true] {
        let (_directory, state_dir) = state_with_credentials();
        let config = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|profile| {
                profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
            })
            .unwrap();
        let mut responses = vec![
            (
                200,
                function_call_stream(
                    "propose",
                    "Bash",
                    json!({"command":"ln -s missing-target checked"}),
                    12,
                ),
            ),
            (
                200,
                reply_stream("review", &approval_findings(should_block).to_string(), 7),
            ),
            (200, reply_stream("after-review", "reviewed", 30)),
        ];
        if should_block {
            responses.insert(
                2,
                (
                    200,
                    reply_stream(
                        "authorization-review",
                        &approval_findings(true).to_string(),
                        7,
                    ),
                ),
            );
        }
        let (endpoint, captured) = serve_scripted_responses(responses).await;
        let client = LoopbackCodexClient::new(config.clone(), &state_dir, endpoint.as_str());
        let client = client.into_fixture_client();
        let agent = Agent::new_embedded(config, client.clone());
        agent.install_own_tickhost();
        agent.set_auto_reviewer(client).await;
        let chat = agent.create_chat(state_dir.clone()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(
                chat,
                "Create the checked link in this temporary directory".into(),
            )
            .await
            .unwrap();
        let mut decision = None;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::PermissionChecked {
                    decision: checked, ..
                } => decision = Some(checked),
                EventKind::Completed { usage } => {
                    assert_eq!(usage.input_tokens, Some(42));
                    break;
                }
                EventKind::PermissionRequested { .. } => {
                    panic!("Codex Auto reviewer asked for user input")
                }
                EventKind::Failed(reason) => panic!("Codex Auto run failed: {reason}"),
                _ => {}
            }
        }
        assert_eq!(
            decision,
            Some(if should_block {
                Decision::Deny
            } else {
                Decision::Allow
            })
        );
        assert_eq!(
            state_dir.join("checked").symlink_metadata().is_ok(),
            !should_block
        );
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), if should_block { 4 } else { 3 });
        assert_eq!(requests[1]["tool_choice"], "auto");
        assert_eq!(requests[1]["parallel_tool_calls"], false);
        assert!(requests[1].get("tools").is_none());
        assert!(
            requests[1]["instructions"]
                .as_str()
                .unwrap()
                .contains("JSON Schema")
        );
        let snapshot = agent.snapshot(chat).await.unwrap();
        let usage = snapshot
            .request_usage
            .entries()
            .iter()
            .filter(|entry| entry.kind != ModelRequestKind::Title)
            .collect::<Vec<_>>();
        assert_eq!(usage.len(), if should_block { 4 } else { 3 });
        assert_eq!(usage[1].kind, ModelRequestKind::PermissionCheck);
        assert_eq!(usage[1].usage.input_tokens, Some(7));
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn explicit_codex_reviewer_uses_its_own_model_and_usage_identity() {
    let (_directory, state_dir) = state_with_credentials();
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
    let (main_endpoint, main_requests) = serve_scripted_responses(vec![
        (
            200,
            function_call_stream(
                "propose",
                "Bash",
                json!({"command":"ln -s missing-target approved"}),
                12,
            ),
        ),
        (200, reply_stream("after-review", "approved", 18)),
    ])
    .await;
    let (reviewer_endpoint, reviewer_requests) = serve_scripted_responses(vec![(
        200,
        reply_stream("review", &approval_findings(false).to_string(), 7),
    )])
    .await;
    let main_client =
        LoopbackCodexClient::new(main_config.clone(), &state_dir, main_endpoint.as_str());
    let reviewer_client = LoopbackCodexClient::new(
        reviewer_config.clone(),
        &state_dir,
        reviewer_endpoint.as_str(),
    );
    let agent = Agent::new_embedded(main_config.clone(), main_client.into_fixture_client());
    agent.install_own_tickhost();
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
        .submit(
            chat,
            "Create the approved link in this temporary directory".into(),
        )
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(reason) => panic!("explicit Codex reviewer failed: {reason}"),
            _ => {}
        }
    }
    assert!(state_dir.join("approved").symlink_metadata().is_ok());
    let main_requests = main_requests.await.unwrap();
    let reviewer_requests = reviewer_requests.await.unwrap();
    assert_eq!(main_requests.len(), 2);
    assert_eq!(reviewer_requests.len(), 1);
    assert_eq!(main_requests[0]["model"], "gpt-5.5");
    assert_eq!(reviewer_requests[0]["model"], "gpt-5.6-sol");
    assert_eq!(reviewer_requests[0]["tool_choice"], "auto");
    assert!(reviewer_requests[0].get("tools").is_none());
    let snapshot = agent.snapshot(chat).await.unwrap();
    let usage = snapshot
        .request_usage
        .entries()
        .iter()
        .filter(|entry| entry.kind != ModelRequestKind::Title)
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 3);
    assert_eq!(usage[1].kind, ModelRequestKind::PermissionCheck);
    assert_eq!(usage[1].model, reviewer_config.identity());
    assert_eq!(usage[1].usage.input_tokens, Some(7));
    agent.shutdown().await;
}

async fn blocked_auto_review_case(
    review_responses: Vec<(u16, String)>,
) -> (Vec<Value>, Vec<ModelRequestKind>, String) {
    let (_directory, state_dir) = state_with_credentials();
    let config = ModelCatalog::defaults()
        .profiles()
        .into_iter()
        .find(|profile| {
            profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
        })
        .unwrap();
    let mut responses = vec![(
        200,
        function_call_stream(
            "propose",
            "Bash",
            json!({"command":"ln -s missing-target blocked"}),
            12,
        ),
    )];
    responses.extend(review_responses);
    responses.push((
        200,
        reply_stream("after-block", "I will continue without the link", 18),
    ));
    let (endpoint, captured) = serve_scripted_responses(responses).await;
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, endpoint.as_str());
    let client = client.into_fixture_client();
    let agent = Agent::new_with_options(
        config,
        client.clone(),
        AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::embedded_defaults()
        },
    );
    agent.set_auto_reviewer(client).await;
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
    let mut reason = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionChecked {
                decision,
                reason: checked_reason,
            } => {
                assert_eq!(decision, Decision::Deny);
                reason = Some(checked_reason);
            }
            EventKind::Completed { .. } => break,
            EventKind::PermissionRequested { .. } => panic!("Auto review asked for user input"),
            EventKind::ToolStarted { .. } => panic!("Auto review admitted blocked operation"),
            EventKind::Failed(error) => panic!("Auto review failed the run: {error}"),
            _ => {}
        }
    }
    assert!(state_dir.join("blocked").symlink_metadata().is_err());
    let kinds = agent
        .snapshot(chat)
        .await
        .unwrap()
        .request_usage
        .entries()
        .iter()
        .map(|entry| entry.kind)
        .collect();
    let requests = captured.await.unwrap();
    agent.shutdown().await;
    (requests, kinds, reason.unwrap())
}

#[tokio::test]
async fn malformed_codex_review_is_retried_then_blocks_without_running_the_tool() {
    let malformed = || {
        reply_stream(
            "bad-review",
            &json!({"thinking":"fixture","shouldBlock":"false","reason":"wrong type"}).to_string(),
            7,
        )
    };
    let (requests, kinds, reason) = blocked_auto_review_case(vec![
        (200, malformed()),
        (200, malformed()),
        (200, malformed()),
    ])
    .await;
    assert_eq!(requests.len(), 5);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == ModelRequestKind::PermissionCheck)
            .count(),
        3
    );
    assert!(reason.to_ascii_lowercase().contains("invalid"), "{reason}");
}

#[tokio::test]
async fn codex_review_rejects_extra_business_tools_before_execution() {
    let (requests, kinds, reason) = blocked_auto_review_case(vec![(200, tool_stream())]).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == ModelRequestKind::PermissionCheck)
            .count(),
        1
    );
    assert!(reason.contains("unexpected tool operations"), "{reason}");
}

async fn orphan_tool_event_requires_manual_review(malformed_result: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for websocket in [false, true] {
        for event_type in [
            "response.custom_tool_call_input.delta",
            "response.custom_tool_call_input.done",
            "response.function_call_output.delta",
            "response.mcp_call_arguments.delta",
            "response.file_search_call.searching",
            "response.code_interpreter_call_code.delta",
        ] {
            let (_directory, state_dir) = state_with_credentials();
            let config = ModelCatalog::defaults()
                .profiles()
                .into_iter()
                .find(|profile| {
                    profile.model == "gpt-5.5" && profile.provider == ModelProvider::OpenAiCodex
                })
                .unwrap();
            let main_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let main_endpoint = format!(
                "http://{}/codex/responses",
                main_listener.local_addr().unwrap()
            );
            let main_server = tokio::spawn(async move {
                for reply in [
                    function_call_stream(
                        "proposal",
                        "Bash",
                        json!({"command":"ln -s missing-target blocked"}),
                        12,
                    ),
                    reply_stream("done", "Done without the operation", 12),
                ] {
                    let (mut socket, _) = main_listener.accept().await.unwrap();
                    read_request(&mut socket).await;
                    send_stream(&mut socket, 200, &reply).await;
                }
            });
            let input = if malformed_result {
                json!(true)
            } else {
                approval_findings(false)
            };
            let original = function_call_stream("review", "classify_result", input, 7);
            let mut response = String::new();
            for (index, chunk) in original
                .split("\n\n")
                .filter(|chunk| !chunk.is_empty())
                .enumerate()
            {
                let mut data: Value = serde_json::from_str(
                    chunk
                        .lines()
                        .find_map(|line| line.strip_prefix("data: "))
                        .unwrap(),
                )
                .unwrap();
                data["sequence_number"] = json!(if index > 1 { index + 1 } else { index });
                response += &format!(
                    "event: {}\ndata: {data}\n\n",
                    data["type"].as_str().unwrap()
                );
                if index == 1 {
                    response += &event(
                        event_type,
                        2,
                        json!({"output_index":42,"item_id":"orphan","delta":"unexpected command","input":"unexpected command"}),
                    );
                }
            }
            let repair = reply_stream("repair", &approval_findings(false).to_string(), 7);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
            let calls = Arc::new(AtomicUsize::new(0));
            let captured = calls.clone();
            let server = tokio::spawn(async move {
                if websocket {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut socket = accept_async(stream).await.unwrap();
                    for reply in [response, repair] {
                        let Some(Ok(WsMessage::Text(_))) = socket.next().await else {
                            break;
                        };
                        captured.fetch_add(1, Ordering::SeqCst);
                        for line in reply.lines().filter_map(|line| line.strip_prefix("data: ")) {
                            if socket
                                .send(WsMessage::Text(line.to_owned().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                } else {
                    for reply in [response, repair] {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        read_request(&mut socket).await;
                        captured.fetch_add(1, Ordering::SeqCst);
                        send_stream(&mut socket, 200, &reply).await;
                    }
                }
            });
            let main = LoopbackCodexClient::new(config.clone(), &state_dir, main_endpoint.as_str());
            let mut reviewer = LoopbackCodexClient::new(config.clone(), &state_dir, &endpoint);
            if websocket {
                reviewer = reviewer.with_websocket();
            }
            let agent = Agent::new_embedded(config, main.into_fixture_client());
            agent
                .set_auto_reviewer(reviewer.into_fixture_client())
                .await;
            let chat = agent.create_chat(state_dir.clone()).await.unwrap();
            agent
                .set_execution_mode(chat, ExecutionMode::Auto)
                .await
                .unwrap();
            let mut events = agent.subscribe();
            agent
                .submit(chat, "Create the blocked link".into())
                .await
                .unwrap();
            let mut manual = false;
            loop {
                match tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .kind
                {
                    EventKind::PermissionChecked { decision, reason } => {
                        assert_eq!(
                            decision,
                            Decision::Ask,
                            "{event_type}, websocket={websocket}, malformed={malformed_result}: {reason}"
                        );
                        assert!(reason.contains("unexpected tool operations"), "{reason}");
                    }
                    EventKind::PermissionRequested { request } => {
                        manual = true;
                        agent
                            .answer_permission(chat, request.id, false)
                            .await
                            .unwrap();
                    }
                    EventKind::ToolStarted { .. } => {
                        panic!("orphan tool event admitted the original operation")
                    }
                    EventKind::Completed { .. } | EventKind::Interrupted => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            assert!(manual);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the queued valid Allow must not be requested"
            );
            assert!(state_dir.join("blocked").symlink_metadata().is_err());
            assert_eq!(
                agent
                    .snapshot(chat)
                    .await
                    .unwrap()
                    .request_usage
                    .entries()
                    .iter()
                    .filter(|entry| entry.kind == ModelRequestKind::PermissionCheck)
                    .count(),
                1
            );
            agent.shutdown().await;
            main_server.abort();
            server.abort();
        }
    }
}

#[tokio::test]
async fn allowing_result_with_orphan_custom_tool_event_requires_manual_review() {
    orphan_tool_event_requires_manual_review(false).await;
}

#[tokio::test]
async fn malformed_result_with_orphan_custom_tool_event_never_gets_a_correction_retry() {
    orphan_tool_event_requires_manual_review(true).await;
}

#[tokio::test]
async fn codex_authentication_and_quota_errors_never_admit_protected_tools() {
    for (status, body) in [
        (401, json!({"error":{"code":"invalid_token"}}).to_string()),
        (
            429,
            json!({"error":{"code":"usage_limit_reached","message":"limit reached"}}).to_string(),
        ),
    ] {
        let (requests, kinds, reason) = blocked_auto_review_case(vec![(status, body)]).await;
        assert_eq!(requests.len(), 3);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == ModelRequestKind::PermissionCheck)
                .count(),
            1
        );
        assert!(!reason.is_empty());
    }
}
