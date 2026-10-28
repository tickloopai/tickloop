//! Codex subscription fixtures shared by model and runtime tests: local credentials, Responses
//! stream builders and numeric-loopback servers that stand in for the official endpoint.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::SinkExt;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::{ModelClient, codex::CodexModelClient, fixture_client};
use tickloop_harness_protocol::ModelConfig;

/// A Codex subscription client aimed at a numeric-loopback test server.
pub struct LoopbackCodexClient(CodexModelClient);

impl LoopbackCodexClient {
    pub fn new(config: ModelConfig, state_dir: &Path, endpoint: &str) -> Self {
        Self(
            CodexModelClient::new(config, state_dir)
                .unwrap()
                .with_loopback_test_endpoint(endpoint)
                .unwrap(),
        )
    }

    pub fn with_websocket(self) -> Self {
        Self(self.0.with_loopback_test_websocket())
    }

    /// Like every loopback login helper (`CodexAuth::new_for_loopback_test` and the others), this
    /// exists only in debug builds, so a release build never accepts a loopback login origin. The
    /// test suites that call these helpers run in debug builds; a release build with `test-support`
    /// (the MCP benchmark) does not call them.
    #[cfg(debug_assertions)]
    pub fn with_oauth_origin(self, state_dir: &Path, origin: &str) -> Self {
        Self(self.0.with_loopback_test_auth(state_dir, origin).unwrap())
    }

    pub fn into_fixture_client(self) -> Arc<dyn ModelClient> {
        fixture_client(Arc::new(self.0))
    }
}

pub fn event(kind: &str, sequence: u64, value: Value) -> String {
    let mut value = value;
    value["type"] = json!(kind);
    value["sequence_number"] = json!(sequence);
    format!("event: {kind}\ndata: {value}\n\n")
}

pub fn state_with_credentials() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let auth_dir = state_dir.join("auth");
    fs::create_dir(&auth_dir).unwrap();
    fs::set_permissions(&auth_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let expires_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 3_600_000;
    let credential_path = auth_dir.join("openai-codex-credentials.json");
    fs::write(
        &credential_path,
        json!({
            "access_token":"local-test-access-token",
            "refresh_token":"local-test-refresh-token",
            "account_id":"local-test-account",
            "expires_at_ms":expires_at_ms,
        })
        .to_string(),
    )
    .unwrap();
    fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600)).unwrap();
    (directory, state_dir)
}

pub fn text_stream() -> String {
    let reasoning = json!({"type":"reasoning","id":"rs-1","summary":[{"type":"summary_text","text":"Considering"}],"encrypted_content":"opaque"});
    let text = json!({"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello from Codex","annotations":[]}]});
    [
        event("response.created", 0, json!({"response":{"id":"resp-1","status":"in_progress"}})),
        event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"reasoning","id":"rs-1","summary":[]}})),
        event("response.reasoning_summary_text.delta", 2, json!({"output_index":0,"item_id":"rs-1","summary_index":0,"delta":"Considering"})),
        event("response.output_item.done", 3, json!({"output_index":0,"item":reasoning})),
        event("response.output_item.added", 4, json!({"output_index":1,"item":{"type":"message","id":"msg-1","role":"assistant","status":"in_progress","content":[]}})),
        event("response.output_text.delta", 5, json!({"output_index":1,"item_id":"msg-1","content_index":0,"delta":"Hello from Codex"})),
        event("response.output_item.done", 6, json!({"output_index":1,"item":text})),
        event("response.done", 7, json!({"response":{"id":"resp-1","status":"completed","output":[reasoning,text],"usage":{"input_tokens":25,"output_tokens":4}}})),
    ].concat()
}

pub async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let (headers, body) = read_raw_request(socket).await;
    (headers, serde_json::from_slice(&body).unwrap())
}

pub async fn read_raw_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut received = Vec::new();
    let mut buffer = [0; 8192];
    let (headers, body_start, body_length) = loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        received.extend_from_slice(&buffer[..count]);
        if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8(received[..end].to_vec()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (headers, end + 4, length);
        }
    };
    while received.len() < body_start + body_length {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        received.extend_from_slice(&buffer[..count]);
    }
    (
        headers,
        received[body_start..body_start + body_length].to_vec(),
    )
}

pub async fn send_stream(socket: &mut TcpStream, status: u16, body: &str) {
    socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
}

pub fn tool_stream() -> String {
    let mut events = vec![event(
        "response.created",
        0,
        json!({"response":{"id":"resp-tools","status":"in_progress"}}),
    )];
    let mut output = Vec::new();
    let mut sequence = 1;
    for (index, file) in ["first.txt", "second.txt"].iter().enumerate() {
        let id = format!("fc_{}", index + 1);
        let call_id = format!("call_{}", index + 1);
        let arguments = format!("{{\"file_path\":\"{file}\"}}");
        events.push(event("response.output_item.added", sequence, json!({"output_index":index,"item":{"type":"function_call","id":id,"call_id":call_id,"name":"Read","arguments":""}})));
        sequence += 1;
        for fragment in ["{\"file_path\":\"", *file, "\"}"] {
            events.push(event(
                "response.function_call_arguments.delta",
                sequence,
                json!({"output_index":index,"item_id":id,"delta":fragment}),
            ));
            sequence += 1;
        }
        events.push(event(
            "response.function_call_arguments.done",
            sequence,
            json!({"output_index":index,"item_id":id,"arguments":arguments}),
        ));
        sequence += 1;
        let item = json!({"type":"function_call","id":id,"call_id":call_id,"name":"Read","arguments":arguments,"status":"completed"});
        events.push(event(
            "response.output_item.done",
            sequence,
            json!({"output_index":index,"item":item}),
        ));
        sequence += 1;
        output.push(item);
    }
    events.push(event("response.done", sequence, json!({"response":{"id":"resp-tools","status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":8}}})));
    events.concat()
}

pub fn reply_stream(response_id: &str, text: &str, input_tokens: u64) -> String {
    reply_stream_with_output(response_id, text, input_tokens, 6)
}

/// [`reply_stream`] reporting `output_tokens`, as a long reply's usage does.
pub fn reply_stream_with_output(
    response_id: &str,
    text: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> String {
    let item = json!({"type":"message","id":format!("msg-{response_id}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
    [
        event("response.created", 0, json!({"response":{"id":response_id,"status":"in_progress"}})),
        event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"message","id":format!("msg-{response_id}"),"role":"assistant","status":"in_progress","content":[]}})),
        event("response.output_text.delta", 2, json!({"output_index":0,"item_id":format!("msg-{response_id}"),"content_index":0,"delta":text})),
        event("response.output_item.done", 3, json!({"output_index":0,"item":item})),
        event("response.done", 4, json!({"response":{"id":response_id,"status":"completed","output":[item],"usage":{"input_tokens":input_tokens,"output_tokens":output_tokens}}})),
    ].concat()
}

pub fn function_call_stream(
    response_id: &str,
    name: &str,
    arguments: Value,
    input_tokens: u64,
) -> String {
    let arguments = arguments.to_string();
    let item = json!({"type":"function_call","id":format!("fc_{response_id}"),"call_id":format!("call_{response_id}"),"name":name,"arguments":arguments,"status":"completed"});
    [
        event("response.created", 0, json!({"response":{"id":response_id,"status":"in_progress"}})),
        event("response.output_item.added", 1, json!({"output_index":0,"item":{"type":"function_call","id":format!("fc_{response_id}"),"call_id":format!("call_{response_id}"),"name":name,"arguments":""}})),
        event("response.function_call_arguments.delta", 2, json!({"output_index":0,"item_id":format!("fc_{response_id}"),"delta":arguments})),
        event("response.function_call_arguments.done", 3, json!({"output_index":0,"item_id":format!("fc_{response_id}"),"arguments":arguments})),
        event("response.output_item.done", 4, json!({"output_index":0,"item":item})),
        event("response.done", 5, json!({"response":{"id":response_id,"status":"completed","output":[item],"usage":{"input_tokens":input_tokens,"output_tokens":3}}})),
    ].concat()
}

pub async fn serve_scripted_responses(
    streams: Vec<(u16, String)>,
) -> (Url, oneshot::Receiver<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!(
        "http://{}/codex/responses",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, stream) in streams {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (_, body) = read_request(&mut socket).await;
            requests.push(body);
            send_stream(&mut socket, status, &stream).await;
        }
        sender.send(requests).unwrap();
    });
    (endpoint, receiver)
}

pub async fn serve_requests_with_headers(
    streams: Vec<String>,
) -> (Url, oneshot::Receiver<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!(
        "http://{}/codex/responses",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for stream in streams {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            send_stream(&mut socket, 200, &stream).await;
        }
        sender.send(requests).unwrap();
    });
    (endpoint, receiver)
}

pub fn rewrite_credentials(state_dir: &Path, access_token: &str, account_id: &str, ttl_ms: u64) {
    let credential_path = state_dir.join("auth/openai-codex-credentials.json");
    let expires_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + ttl_ms;
    fs::write(
        &credential_path,
        json!({
            "access_token":access_token,
            "refresh_token":"local-test-refresh-token",
            "account_id":account_id,
            "expires_at_ms":expires_at_ms,
        })
        .to_string(),
    )
    .unwrap();
    fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600)).unwrap();
}

pub async fn send_ws_stream(socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>, sse: &str) {
    for event in sse.split("\n\n") {
        if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) {
            socket.send(WsMessage::Text(data.into())).await.unwrap();
        }
    }
}

pub struct HttpReply {
    pub status: u16,
    pub retry_after: Option<u64>,
    pub should_retry: Option<bool>,
    pub body: String,
}

pub async fn serve_http_replies(
    replies: Vec<HttpReply>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
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
            let _ = read_raw_request(&mut socket).await;
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
