//! The Anthropic prompt cache across a Run's tool loop, on the wire the runtime really sends: the
//! second request repeats the first byte for byte up to the first's tail breakpoint, and its own
//! tail breakpoint moves forward.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use tickloop_harness_model::{LocalModelClients, ModelClients, cards::ModelCatalog};
use tickloop_harness_protocol::{EventKind, ExecutionMode};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::model_http_support::accept_script_request;
use tokio::{io::AsyncWriteExt, net::TcpListener};

const RUN_TIMEOUT: Duration = Duration::from_secs(8);

fn frame(kind: &str, mut data: Value) -> String {
    data["type"] = json!(kind);
    format!("event: {kind}\ndata: {data}\n\n")
}

fn reply(block: Value, stop: &str) -> String {
    [
        frame(
            "message_start",
            json!({"message":{"id":"msg","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}),
        ),
        frame("content_block_start", json!({"index":0,"content_block":block})),
        frame("content_block_stop", json!({"index":0})),
        frame(
            "message_delta",
            json!({"delta":{"stop_reason":stop},"usage":{"output_tokens":2}}),
        ),
        frame("message_stop", json!({})),
    ]
    .concat()
}

fn has_tool_result(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .any(|block| block["type"] == "tool_result")
}

/// The path of every block carrying a `cache_control`.
fn breakpoints(body: &Value) -> Vec<String> {
    let mut found = Vec::new();
    for (index, block) in body["system"].as_array().into_iter().flatten().enumerate() {
        if block.get("cache_control").is_some() {
            found.push(format!("system/{index}"));
        }
    }
    for (message, entry) in body["messages"].as_array().unwrap().iter().enumerate() {
        for (index, block) in entry["content"].as_array().unwrap().iter().enumerate() {
            if block.get("cache_control").is_some() {
                found.push(format!("messages/{message}/{index}"));
            }
        }
    }
    found
}

fn last_block(body: &Value, message: usize) -> String {
    let blocks = body["messages"][message]["content"]
        .as_array()
        .unwrap()
        .len();
    format!("messages/{message}/{}", blocks - 1)
}

fn without_breakpoints(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            object.remove("cache_control");
            for item in object.values_mut() {
                *item = without_breakpoints(item.take());
            }
        }
        Value::Array(items) => {
            for item in items {
                *item = without_breakpoints(item.take());
            }
        }
        _ => {}
    }
    value
}

#[tokio::test]
async fn a_tool_loop_reads_the_previous_request_and_moves_its_tail_breakpoint() {
    let workdir = tempfile::tempdir().unwrap();
    std::fs::write(workdir.path().join("notes.txt"), "Delivery is Friday.").unwrap();
    let notes = workdir.path().join("notes.txt");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let conversation = Arc::new(Mutex::new(Vec::new()));
    let captured = conversation.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, (_, body)) = accept_script_request(&listener).await;
            let response = if body["tools"].is_null() {
                reply(json!({"type":"text","text":"Notes"}), "end_turn")
            } else {
                let response = if has_tool_result(&body) {
                    reply(json!({"type":"text","text":"Friday."}), "end_turn")
                } else {
                    reply(
                        json!({"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":notes}}),
                        "tool_use",
                    )
                };
                captured.lock().unwrap().push(body);
                response
            };
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });

    let mut config = ModelCatalog::defaults()
        .get("anthropic-api", "claude-opus-5-5")
        .unwrap();
    config.base_url = base_url;
    // A loopback service needs no key.
    config.api_key_env.clear();
    let client = LocalModelClients::new(None).client(&config).unwrap();
    let agent = Agent::new_embedded(config, client);
    let chat = agent
        .create_chat(workdir.path().to_path_buf())
        .await
        .unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "When is the delivery?".into())
        .await
        .unwrap();
    tokio::time::timeout(RUN_TIMEOUT, async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id != chat {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the Run finishes");
    agent.shutdown().await;
    server.abort();

    let requests = conversation.lock().unwrap().clone();
    let [first, second] = &requests[..] else {
        panic!("two conversation requests, got {}", requests.len());
    };
    let first_system_end = format!("system/{}", first["system"].as_array().unwrap().len() - 1);
    let first_tail = last_block(first, first["messages"].as_array().unwrap().len() - 1);
    assert_eq!(
        breakpoints(first),
        [
            first_system_end.clone(),
            last_block(first, 0),
            first_tail.clone()
        ],
        "the system prompt, the user context and the tail: {first}"
    );
    assert_eq!(
        breakpoints(second),
        [
            first_system_end,
            last_block(second, 0),
            first_tail,
            last_block(second, second["messages"].as_array().unwrap().len() - 1),
        ],
        "the previous tail stays marked and the tail moves to the tool result: {second}"
    );
    for part in ["model", "system", "tools", "thinking", "output_config"] {
        assert_eq!(
            without_breakpoints(first[part].clone()),
            without_breakpoints(second[part].clone()),
            "{part}"
        );
    }
    let first_messages = without_breakpoints(first["messages"].clone());
    let second_messages = without_breakpoints(second["messages"].clone());
    let first_messages = first_messages.as_array().unwrap();
    assert_eq!(
        first_messages[..],
        second_messages.as_array().unwrap()[..first_messages.len()],
        "the first request's messages lead the second's byte for byte"
    );
}
