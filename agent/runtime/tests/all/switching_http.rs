use tickloop_agent_test_support::model_http_support;

use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tickloop_agent_model::{HttpModelClient, cards::model_for_persistent_selection};
use tickloop_agent_protocol::{
    BranchRequest, CommandState, Content, EventKind, ModelChange, ModelConfig, Protocol,
};
use tickloop_agent_runtime::Agent;
use tokio::{io::AsyncWriteExt, net::TcpListener};
use uuid::Uuid;

fn frame(kind: &str, mut data: Value) -> String {
    data["type"] = json!(kind);
    format!("event: {kind}\ndata: {data}\n\n")
}

#[tokio::test]
async fn builtin_selection_with_a_mock_transport_completes_tools_and_resumes_history() {
    let directory = tempfile::tempdir().unwrap();
    let tool = json!({"command":"printf gateway-tool","timeout":10000});
    let (endpoint, requests) = old_endpoint(vec![
        response(Protocol::Responses, Some(&tool), ""),
        response(Protocol::Responses, None, "Tool handled"),
        response(Protocol::Responses, None, "Resumed answer"),
    ])
    .await;
    let root = directory.path().join("agent");
    let config = profile(endpoint, Protocol::Responses, "assistant");
    assert!(!root.join("providers").exists());
    let agent = Agent::open_embedded(
        root.clone(),
        config.clone(),
        Arc::new(HttpModelClient::new(config.clone()).unwrap()),
    )
    .unwrap();
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Use the shell tool".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::Completed { .. } if event.run_id == first.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
    let reopened = Agent::open_embedded(
        root,
        config.clone(),
        Arc::new(HttpModelClient::new(config).unwrap()),
    )
    .unwrap();
    reopened.install_own_tickhost();
    reopened.resume_chat(chat).await.unwrap();
    let mut events = reopened.subscribe();
    let second = reopened
        .submit(chat, None, "Continue".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == second.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let requests = tokio::time::timeout(Duration::from_secs(2), requests)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].1["model"], "gpt-6-luna");
    assert_tool_history_with_output(Protocol::Responses, &requests[1].1, "gateway-tool");
    assert!(requests[2].1.to_string().contains("Tool handled"));
    reopened.shutdown().await;
}

#[tokio::test]
async fn api_key_branch_uses_public_history_while_source_keeps_proven_private_history() {
    let directory = tempfile::tempdir().unwrap();
    let private_reasoning = json!({"type":"reasoning","id":"reasoning-1","summary":[],"encrypted_content":"opaque-branch-source"});
    let public_message = json!({"type":"message","id":"message-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Source answer"}]});
    let first_reply = [
        frame("response.created", json!({"response":{"id":"response-1","status":"in_progress"}})),
        frame("response.output_item.added", json!({"output_index":0,"item":{"type":"reasoning","id":"reasoning-1","summary":[]}})),
        frame("response.output_item.done", json!({"output_index":0,"item":private_reasoning.clone()})),
        frame("response.output_item.added", json!({"output_index":1,"item":{"type":"message","id":"message-1","role":"assistant","content":[]}})),
        frame("response.output_text.delta", json!({"output_index":1,"item_id":"message-1","content_index":0,"delta":"Source answer"})),
        frame("response.output_item.done", json!({"output_index":1,"item":public_message.clone()})),
        frame("response.completed", json!({"response":{"id":"response-1","status":"completed","output":[private_reasoning,public_message]}})),
    ].concat();
    let (endpoint, requests) = old_endpoint(vec![
        first_reply,
        response(Protocol::Responses, None, "Child answer"),
        response(Protocol::Responses, None, "Source continuation"),
    ])
    .await;
    let mut config = profile(endpoint, Protocol::Responses, "branch-api-key");
    config.api_key_env = "PATH".into();
    let agent = Agent::new_embedded(
        config.clone(),
        Arc::new(HttpModelClient::new(config).unwrap()),
    );
    let source = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(source, None, "Start".into()).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == first.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(
        agent
            .snapshot(source)
            .await
            .unwrap()
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::ProtocolData { .. }))
    );
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            source_loop_id: Some(first.loop_id),
            name: Some("Public child".into()),
        })
        .await
        .unwrap();
    assert!(
        branch
            .snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .all(|content| !matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            ))
    );
    let child_turn = agent
        .submit(branch.child_chat_id, None, "Continue child".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == child_turn.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let source_turn = agent
        .submit(source, None, "Continue source".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == source_turn.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let requests = requests.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|(headers, _)| {
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer ")
    }));
    assert!(!requests[1].1.to_string().contains("opaque-branch-source"));
    assert!(requests[2].1.to_string().contains("opaque-branch-source"));
    agent.shutdown().await;
}

fn response(protocol: Protocol, tool: Option<&Value>, text: &str) -> String {
    match protocol {
        Protocol::Google => unreachable!("Google switching is covered by google_models tests"),
        Protocol::SystemOne => unreachable!("Jev is not a conversation protocol"),
        Protocol::Messages => {
            let block = tool.map_or_else(
                || json!({"type":"text","text":text}),
                |input| json!({"type":"tool_use","id":"shell-1","name":"Bash","input":input}),
            );
            [
                frame(
                    "message_start",
                    json!({"message":{"role":"assistant","content":[]}}),
                ),
                frame(
                    "content_block_start",
                    json!({"index":0,"content_block":block}),
                ),
                frame("content_block_stop", json!({"index":0})),
                frame(
                    "message_delta",
                    json!({"delta":{"stop_reason":if tool.is_some(){"tool_use"}else{"end_turn"}}}),
                ),
                frame("message_stop", json!({})),
            ]
            .concat()
        }
        Protocol::Responses => {
            let item = tool.map_or_else(|| json!({"type":"message","id":"item-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}), |input| json!({"type":"function_call","id":"item-1","call_id":"shell-1","name":"Bash","arguments":input.to_string(),"status":"completed"}));
            let initial = if tool.is_some() {
                let mut initial = item.clone();
                initial["arguments"] = json!("");
                initial
            } else {
                json!({"type":"message","id":"item-1","role":"assistant","content":[]})
            };
            let delta = tool.map_or_else(
                || {
                    frame(
                        "response.output_text.delta",
                        json!({"output_index":0,"item_id":"item-1","content_index":0,"delta":text}),
                    )
                },
                |input| {
                    frame(
                        "response.function_call_arguments.delta",
                        json!({"output_index":0,"item_id":"item-1","delta":input.to_string()}),
                    )
                },
            );
            [
                frame(
                    "response.created",
                    json!({"response":{"id":"response-1","status":"in_progress"}}),
                ),
                frame(
                    "response.output_item.added",
                    json!({"output_index":0,"item":initial}),
                ),
                delta,
                frame(
                    "response.output_item.done",
                    json!({"output_index":0,"item":item}),
                ),
                frame(
                    "response.completed",
                    json!({"response":{"id":"response-1","status":"completed","output":[item]}}),
                ),
            ]
            .concat()
        }
        Protocol::ChatCompletions => {
            let delta = tool.map_or_else(|| json!({"role":"assistant","content":text}), |input| json!({"role":"assistant","tool_calls":[{"index":0,"id":"shell-1","type":"function","function":{"name":"Bash","arguments":input.to_string()}}]}));
            let chunk = |delta, reason| {
                format!(
                    "data: {}\n\n",
                    json!({"id":"chat-1","choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
                )
            };
            [
                chunk(delta, Value::Null),
                chunk(
                    json!({}),
                    json!(if tool.is_some() { "tool_calls" } else { "stop" }),
                ),
                "data: [DONE]\n\n".into(),
            ]
            .concat()
        }
    }
}

async fn old_endpoint(
    replies: Vec<String>,
) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut socket, request) = model_http_support::accept_script_request(&listener).await;
            requests.push(request);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",reply.len()).as_bytes()).await.unwrap();
            for fragment in reply.as_bytes().chunks(11) {
                socket.write_all(fragment).await.unwrap();
                tokio::task::yield_now().await;
            }
        }
        requests
    });
    (url, task)
}

fn profile(base_url: String, protocol: Protocol, name: &str) -> ModelConfig {
    let (provider, id) = match protocol {
        Protocol::Messages => ("anthropic-api", "claude-haiku-5-5"),
        Protocol::Responses => ("openai-api", "gpt-6-luna"),
        Protocol::ChatCompletions => ("deepseek-api", "deepseek-flash"),
        _ => unreachable!(),
    };
    let mut config = tickloop_agent_model::cards::ModelCatalog::defaults()
        .get(provider, id)
        .unwrap();
    config.base_url = base_url.trim_end_matches("/anthropic").into();
    config.name = name.into();
    config.card.as_mut().unwrap().display_name = name.into();
    config.api_key_env.clear();
    config
}

fn assert_tool_history(protocol: Protocol, body: &Value) {
    assert_tool_history_with_output(protocol, body, "shell-finished");
}

fn assert_tool_history_with_output(protocol: Protocol, body: &Value, expected_output: &str) {
    let messages = body[if protocol == Protocol::Responses {
        "input"
    } else {
        "messages"
    }]
    .as_array()
    .unwrap();
    let (call, result, content) = match protocol {
        Protocol::Google => unreachable!("Google switching is covered by google_models tests"),
        Protocol::SystemOne => unreachable!("Jev is not a conversation protocol"),
        Protocol::Messages => {
            let index = messages
                .iter()
                .position(|message| message["content"][0]["type"] == "tool_use")
                .unwrap();
            (
                &messages[index]["content"][0]["id"],
                &messages[index + 1]["content"][0]["tool_use_id"],
                &messages[index + 1]["content"][0]["content"],
            )
        }
        Protocol::Responses => {
            let index = messages
                .iter()
                .position(|message| message["type"] == "function_call")
                .unwrap();
            assert_eq!(messages[index + 1]["type"], "function_call_output");
            (
                &messages[index]["call_id"],
                &messages[index + 1]["call_id"],
                &messages[index + 1]["output"],
            )
        }
        Protocol::ChatCompletions => {
            let index = messages
                .iter()
                .position(|message| message["tool_calls"].is_array())
                .unwrap();
            assert_eq!(messages[index + 1]["role"], "tool");
            (
                &messages[index]["tool_calls"][0]["id"],
                &messages[index + 1]["tool_call_id"],
                &messages[index + 1]["content"],
            )
        }
    };
    assert_eq!(call, "shell-1");
    assert_eq!(call, result);
    assert_eq!(content.as_str().unwrap(), expected_output);
}

#[tokio::test]
async fn three_http_protocols_switch_only_after_the_running_shell_and_old_tool_round_finish() {
    for (old, new) in [
        (Protocol::Messages, Protocol::Responses),
        (Protocol::Responses, Protocol::ChatCompletions),
        (Protocol::ChatCompletions, Protocol::Messages),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let tool = json!({"command":"printf started > started; while [ ! -f release ]; do sleep 0.02; done; printf shell-finished","timeout":10000});
        let (old_url, old_requests) = old_endpoint(vec![
            response(old, Some(&tool), ""),
            response(old, None, "Old round completed"),
        ])
        .await;
        let (new_url, mut new_request) =
            model_http_support::serve_response(200, response(new, None, "New profile answered"))
                .await;
        let original = profile(old_url, old, "old-profile");
        let selected = profile(new_url, new, "new-profile");
        let agent = Agent::new_embedded(
            original.clone(),
            Arc::new(HttpModelClient::new(original.clone()).unwrap()),
        );
        agent.install_own_tickhost();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let initial_model = agent.snapshot(chat).await.unwrap().model;
        let mut events = agent.subscribe();
        let first = agent
            .submit(chat, None, "Run the supervised shell probe".into())
            .await
            .unwrap();
        loop {
            match tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::PermissionRequested { request } => {
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                    break;
                }
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while !directory.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let command = agent.commands(chat).await.unwrap().pop().unwrap();
        assert_eq!(command.state, CommandState::Running);
        assert_eq!(
            agent
                .set_model(
                    chat,
                    selected.clone(),
                    Arc::new(HttpModelClient::new(selected.clone()).unwrap())
                )
                .await
                .unwrap(),
            ModelChange::Pending
        );
        let pending = agent.snapshot(chat).await.unwrap();
        assert_eq!(pending.model, initial_model);
        assert_eq!(
            pending.pending_model,
            Some(model_for_persistent_selection(selected.clone()))
        );
        assert!(matches!(
            new_request.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            agent.commands(chat).await.unwrap()[0].state,
            CommandState::Running
        );
        std::fs::write(directory.path().join("release"), "").unwrap();
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            match event.kind {
                EventKind::Completed { .. } if event.run_id == first.run_id => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let old_requests = tokio::time::timeout(Duration::from_secs(2), old_requests)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old_requests.len(), 2);
        assert_tool_history(old, &old_requests[1].1);
        assert!(
            old_requests
                .iter()
                .all(|(_, body)| body["model"] == original.model)
        );
        let next = agent
            .submit(
                chat,
                Some(first.loop_id),
                "Continue with selected protocol".into(),
            )
            .await
            .unwrap();
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            match event.kind {
                EventKind::Completed { .. } if event.run_id == next.run_id => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let (headers, body) = tokio::time::timeout(Duration::from_secs(2), new_request)
            .await
            .unwrap()
            .unwrap();
        assert_tool_history(new, &body);
        let path = match new {
            Protocol::Google => unreachable!("Google switching is covered by google_models tests"),
            Protocol::SystemOne => unreachable!("Jev is not a conversation protocol"),
            Protocol::Messages => "/v1/messages",
            Protocol::Responses => "/responses",
            Protocol::ChatCompletions => "/chat/completions",
        };
        assert!(headers.starts_with(&format!("POST {path} HTTP/1.1")));
        assert_eq!(body["model"], selected.model);
        assert!(body.to_string().contains("Old round completed"));
        let completed = agent.commands(chat).await.unwrap().pop().unwrap();
        assert_eq!(completed.operation_id, command.operation_id);
        assert_eq!(completed.state, CommandState::Completed);
        assert_eq!(completed.exit_code, Some(0));
        assert_eq!(agent.snapshot(chat).await.unwrap().loops.len(), 1);
        agent.shutdown().await;
    }
}

fn signed_messages_reply() -> String {
    [
        frame("message_start", json!({"message":{"role":"assistant","content":[]}})),
        frame("content_block_start", json!({"index":0,"content_block":{"type":"thinking","thinking":"Readable reasoning from A","signature":"original-signed-block"}})),
        frame("content_block_stop", json!({"index":0})),
        frame("content_block_start", json!({"index":1,"content_block":{"type":"text","text":"Original answer from A"}})),
        frame("content_block_stop", json!({"index":1})),
        frame("message_delta", json!({"delta":{"stop_reason":"end_turn"}})),
        frame("message_stop", json!({})),
    ].concat()
}

async fn finish_turn(agent: &Agent, chat: Uuid, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => return,
            EventKind::Failed(error) => panic!("{prompt}: {error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn messages_responses_roundtrip_and_restart_preserve_signed_cache_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("state");
    tickloop_agent_model::credentials::save_api_key(
        &root,
        "SIGNED_HISTORY_TEST_KEY",
        "test-session-key",
    )
    .unwrap();
    let (a_url, a_requests) = old_endpoint(vec![
        signed_messages_reply(),
        response(Protocol::Messages, None, "Second A answer"),
        response(Protocol::Messages, None, "Returned to A"),
    ])
    .await;
    let (b_url, b_requests) = old_endpoint(vec![
        response(Protocol::Responses, None, "B answer"),
        response(Protocol::Responses, None, "B after restart"),
    ])
    .await;
    let mut a = profile(a_url, Protocol::Messages, "model-a");
    a.api_key_env = "SIGNED_HISTORY_TEST_KEY".into();
    let mut b = profile(b_url, Protocol::Responses, "model-b");
    b.api_key_env = "SIGNED_HISTORY_TEST_KEY".into();
    let agent = Agent::open_embedded(
        root.clone(),
        a.clone(),
        tickloop_agent_model::model_client(a.clone(), Some(&root)).unwrap(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    finish_turn(&agent, chat, "First prompt").await;
    finish_turn(&agent, chat, "Second prompt").await;
    let before = agent.snapshot(chat).await.unwrap();
    agent
        .set_model(
            chat,
            b.clone(),
            tickloop_agent_model::model_client(b.clone(), Some(&root)).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().messages,
        before.messages
    );
    finish_turn(&agent, chat, "Use B").await;
    let mixed = agent.snapshot(chat).await.unwrap();
    assert_ne!(mixed.message_sources.get(&1), mixed.message_sources.get(&5));
    agent.shutdown().await;
    let resumed = Agent::open_embedded(
        root.clone(),
        b.clone(),
        tickloop_agent_model::model_client(b.clone(), Some(&root)).unwrap(),
    )
    .unwrap();
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert_eq!(restored.message_sources, mixed.message_sources);
    assert_eq!(restored.message_models, mixed.message_models);
    assert!(restored.messages.iter().flat_map(|message| &message.content).any(|block| matches!(block, Content::Thinking { signature: Some(value), .. } if value == "original-signed-block")));
    finish_turn(&resumed, chat, "Continue after restart").await;
    resumed
        .set_model(
            chat,
            a.clone(),
            tickloop_agent_model::model_client(a, Some(&root)).unwrap(),
        )
        .await
        .unwrap();
    finish_turn(&resumed, chat, "Return to A").await;
    let a_requests = a_requests.await.unwrap();
    let b_requests = b_requests.await.unwrap();
    let stable_prefix = &a_requests[1].1["messages"].as_array().unwrap()[..2];
    assert_eq!(
        stable_prefix,
        &a_requests[2].1["messages"].as_array().unwrap()[..2]
    );
    assert_eq!(
        stable_prefix[1]["content"][0]["signature"],
        "original-signed-block"
    );
    for (_, body) in &b_requests {
        assert!(!body.to_string().contains("original-signed-block"));
        assert!(body.to_string().contains("Readable reasoning from A"));
        assert!(body.to_string().contains("Original answer from A"));
    }
    assert_eq!(a_requests[1].1["model"], a_requests[2].1["model"]);
    resumed.shutdown().await;
}
