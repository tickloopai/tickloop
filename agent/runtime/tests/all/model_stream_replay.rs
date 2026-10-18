use tickloop_agent_test_support::model_http_support;

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use tickloop_agent_model::{LocalModelClients};
use tickloop_agent_protocol::{EventKind, ModelConfig, Protocol};
use tickloop_agent_runtime::Agent;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy)]
enum IncompleteOutput {
    Thinking,
    ToolArguments,
}

struct ReplayServer {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl ReplayServer {
    async fn start(protocol: Protocol, first_response: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, (_, body)) =
                    model_http_support::accept_script_request(&listener).await;
                let response = if captured.lock().unwrap().is_empty() {
                    first_response.clone()
                } else {
                    complete_response(protocol)
                };
                captured.lock().unwrap().push(body);
                answer_request(socket, response).await;
            }
        });
        Self {
            base_url,
            requests,
            task,
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn request_bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for ReplayServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn answer_request(mut socket: TcpStream, response: String) {
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.shutdown().await.unwrap();
}

fn frame(kind: &str, mut data: Value) -> String {
    data["type"] = json!(kind);
    format!("event: {kind}\ndata: {data}\n\n")
}

fn chat_chunk(delta: Value, finish_reason: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id":"chat-1", "object":"chat.completion.chunk",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}]
        })
    )
}

fn incomplete_response(protocol: Protocol, output: IncompleteOutput) -> String {
    match (protocol, output) {
        (Protocol::Google, IncompleteOutput::Thinking) => format!("data: {}\n\n", json!({"responseId":"google-1","candidates":[{"index":0,"content":{"role":"model","parts":[{"thought":true,"text":"Consider a file write"}]}}]})),
        (Protocol::Google, IncompleteOutput::ToolArguments) => format!("data: {}\n\n", json!({"responseId":"google-1","candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{"name":"Write","args":{"file_path":"side-effect.txt"}}}]}}]})),
        (Protocol::SystemOne, _) => unreachable!("Jev is not a conversation protocol"),
        (Protocol::Messages, IncompleteOutput::Thinking) => [
            frame("message_start", json!({"message":{"role":"assistant","content":[]}})),
            frame("content_block_start", json!({"index":0,"content_block":{"type":"thinking","thinking":""}})),
            frame("content_block_delta", json!({"index":0,"delta":{"type":"thinking_delta","thinking":"Consider a file write"}})),
        ].concat(),
        (Protocol::Messages, IncompleteOutput::ToolArguments) => [
            frame("message_start", json!({"message":{"role":"assistant","content":[]}})),
            frame("content_block_start", json!({"index":0,"content_block":{"type":"tool_use","id":"call-1","name":"Write","input":{}}})),
            frame("content_block_delta", json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"side-effect.txt\","}})),
        ].concat(),
        (Protocol::Responses, IncompleteOutput::Thinking) => [
            frame("response.created", json!({"response":{"id":"resp-1","status":"in_progress"}})),
            frame("response.output_item.added", json!({"output_index":0,"item":{"type":"reasoning","id":"reason-1","content":[],"summary":[]}})),
            frame("response.reasoning_text.delta", json!({"output_index":0,"item_id":"reason-1","content_index":0,"delta":"Consider a file write"})),
        ].concat(),
        (Protocol::Responses, IncompleteOutput::ToolArguments) => [
            frame("response.created", json!({"response":{"id":"resp-1","status":"in_progress"}})),
            frame("response.output_item.added", json!({"output_index":0,"item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"Write","arguments":""}})),
            frame("response.function_call_arguments.delta", json!({"output_index":0,"item_id":"fc-1","delta":"{\"file_path\":\"side-effect.txt\","})),
        ].concat(),
        (Protocol::ChatCompletions, IncompleteOutput::Thinking) =>
            chat_chunk(json!({"role":"assistant","reasoning_content":"Consider a file write"}), Value::Null),
        (Protocol::ChatCompletions, IncompleteOutput::ToolArguments) =>
            chat_chunk(json!({"role":"assistant","tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"Write","arguments":"{\"file_path\":\"side-effect.txt\","}}]}), Value::Null),
    }
}

fn complete_response(protocol: Protocol) -> String {
    match protocol {
        Protocol::Google => format!(
            "data: {}\n\n",
            json!({"responseId":"google-2","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"Quick completion"}]},"finishReason":"STOP"}]})
        ),
        Protocol::SystemOne => unreachable!("Jev is not a conversation protocol"),
        Protocol::Messages => [
            frame(
                "message_start",
                json!({"message":{"role":"assistant","content":[]}}),
            ),
            frame(
                "content_block_start",
                json!({"index":0,"content_block":{"type":"text","text":"Quick completion"}}),
            ),
            frame("content_block_stop", json!({"index":0})),
            frame("message_delta", json!({"delta":{"stop_reason":"end_turn"}})),
            frame("message_stop", json!({})),
        ]
        .concat(),
        Protocol::Responses => {
            let item = json!({"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Quick completion"}]});
            [
                frame("response.created", json!({"response":{"id":"resp-2","status":"in_progress"}})),
                frame("response.output_item.added", json!({"output_index":0,"item":{"type":"message","id":"msg-1","role":"assistant","content":[]}})),
                frame("response.output_text.delta", json!({"output_index":0,"item_id":"msg-1","content_index":0,"delta":"Quick completion"})),
                frame("response.output_item.done", json!({"output_index":0,"item":item})),
                frame("response.completed", json!({"response":{"id":"resp-2","status":"completed","output":[item]}})),
            ].concat()
        }
        Protocol::ChatCompletions => [
            chat_chunk(
                json!({"role":"assistant","content":"Quick completion"}),
                Value::Null,
            ),
            chat_chunk(json!({}), json!("stop")),
            "data: [DONE]\n\n".into(),
        ]
        .concat(),
    }
}

fn config(base_url: String, protocol: Protocol) -> ModelConfig {
    let mut config = tickloop_agent_test_support::config();
    config.base_url = base_url;
    config.protocol = protocol;
    config
}

async fn run_agent_until_terminal(
    protocol: Protocol,
    server: &ReplayServer,
    workdir: &Path,
) -> bool {
    let config = config(server.base_url.clone(), protocol);
    let client = LocalModelClients::new(None).client(&config).unwrap();
    let agent = Agent::new_embedded(config, client);
    let chat = agent.create_chat(workdir.to_path_buf()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            chat,
            None,
            "Write side-effect.txt if a tool call is complete".into(),
        )
        .await
        .unwrap();
    let outcome = tokio::time::timeout(RUN_TIMEOUT, async {
        let mut permission_requests = 0;
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id != chat {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break (true, permission_requests),
                EventKind::Failed(_) => break (false, permission_requests),
                EventKind::PermissionRequested { request } => {
                    permission_requests += 1;
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }
    })
    .await
    .expect("model run did not finish");
    agent.shutdown().await;
    assert_eq!(
        outcome.1, 0,
        "{protocol:?} requested permission for an incomplete tool call"
    );
    outcome.0
}

fn assert_real_request_encoding(protocol: Protocol, request: &Value) {
    if protocol != Protocol::Google {
        assert_eq!(request["stream"], true, "{protocol:?}");
    }
    match protocol {
        Protocol::Google => assert!(request["contents"].is_array(), "{request}"),
        Protocol::SystemOne => unreachable!("Jev is not a conversation protocol"),
        Protocol::Responses => assert!(request["input"].is_array(), "{request}"),
        Protocol::Messages | Protocol::ChatCompletions => {
            assert!(request["messages"].is_array(), "{request}")
        }
    }
}

#[tokio::test]
async fn reasoning_only_disconnect_is_not_replayed_by_any_http_adapter() {
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
        Protocol::Google,
    ] {
        let workdir = tempfile::tempdir().unwrap();
        let server = ReplayServer::start(
            protocol,
            incomplete_response(protocol, IncompleteOutput::Thinking),
        )
        .await;
        let completed = run_agent_until_terminal(protocol, &server, workdir.path()).await;
        assert!(
            !completed,
            "{protocol:?} replayed a reasoning-only response"
        );
        assert_eq!(
            server.request_count(),
            1,
            "{protocol:?} sent a second HTTP request"
        );
        assert_real_request_encoding(protocol, &server.request_bodies()[0]);
        assert!(!workdir.path().join("side-effect.txt").exists());
    }
}

#[tokio::test]
async fn partial_tool_arguments_disconnect_without_replay_or_tool_execution() {
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
        Protocol::Google,
    ] {
        let workdir = tempfile::tempdir().unwrap();
        let server = ReplayServer::start(
            protocol,
            incomplete_response(protocol, IncompleteOutput::ToolArguments),
        )
        .await;
        let completed = run_agent_until_terminal(protocol, &server, workdir.path()).await;
        assert!(!completed, "{protocol:?} replayed an incomplete tool call");
        assert_eq!(
            server.request_count(),
            1,
            "{protocol:?} sent a second HTTP request"
        );
        assert_real_request_encoding(protocol, &server.request_bodies()[0]);
        assert!(
            !workdir.path().join("side-effect.txt").exists(),
            "{protocol:?} executed incomplete tool input"
        );
    }
}

#[tokio::test]
async fn disconnect_before_meaningful_output_still_retries_and_completes() {
    let protocol = Protocol::Responses;
    let workdir = tempfile::tempdir().unwrap();
    let server = ReplayServer::start(
        protocol,
        frame(
            "response.created",
            json!({"response":{"id":"resp-1","status":"in_progress"}}),
        ),
    )
    .await;
    let completed = run_agent_until_terminal(protocol, &server, workdir.path()).await;
    assert!(completed, "a stream with no output did not retry");
    assert_eq!(server.request_count(), 2);
    assert_real_request_encoding(protocol, &server.request_bodies()[0]);
    assert!(!workdir.path().join("side-effect.txt").exists());
}
