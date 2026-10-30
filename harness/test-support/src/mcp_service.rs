//! A programmable MCP peer, also reachable over stdio using the bridge below.
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct McpService {
    url: String,
    state: Arc<Mutex<State>>,
    cancel: CancellationToken,
    discover_started: Arc<tokio::sync::Notify>,
    discover_disconnected: Arc<tokio::sync::Notify>,
}
struct State {
    instructions: String,
    tools: Value,
    result: Value,
    calls: Vec<Value>,
    delay: std::time::Duration,
    directory_delay: std::time::Duration,
    directory_error: Option<Value>,
    received: Vec<(String, tokio::time::Instant)>,
    status: u16,
    error: Option<Value>,
    changed: bool,
    challenge: Option<String>,
    bearer: Option<String>,
    requests: usize,
    notification: Option<String>,
    resume_directory: bool,
    directory_response: Option<Value>,
    directory_resumes: usize,
    directory_resume_id: Option<String>,
    empty_sse_priming: bool,
    resources_capability: Option<Value>,
    resources: Value,
    resource_text: String,
    resources_changed: bool,
    discover_error_over_sse: bool,
    discover_replies: std::collections::VecDeque<(u16, Value)>,
    discover_delay: std::time::Duration,
    close_discover: bool,
    truncate_discover: bool,
    stall_discover_stream: bool,
    http_responses: std::collections::HashMap<String, (u16, String)>,
    protocol_version: Option<String>,
    initialize_error: Option<Value>,
    messages: Vec<(Value, Option<String>)>,
}
impl McpService {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Self {
            url: format!("http://{}/mcp", listener.local_addr().unwrap()),
            state: Arc::new(Mutex::new(State {
                instructions: "Fixture service instructions".into(),
                tools: json!([]),
                result: json!({"content":[]}),
                calls: Vec::new(),
                delay: std::time::Duration::ZERO,
                directory_delay: std::time::Duration::ZERO,
                directory_error: None,
                received: Vec::new(),
                status: 200,
                error: None,
                changed: false,
                challenge: None,
                bearer: None,
                requests: 0,
                notification: None,
                resume_directory: false,
                directory_response: None,
                directory_resumes: 0,
                directory_resume_id: None,
                empty_sse_priming: false,
                resources_capability: Some(json!({})),
                resources: json!([{"uri":"fixture://text","name":"text"}]),
                resource_text: "resource text".into(),
                resources_changed: false,
                discover_error_over_sse: false,
                discover_replies: Default::default(),
                discover_delay: std::time::Duration::ZERO,
                close_discover: false,
                truncate_discover: false,
                stall_discover_stream: false,
                http_responses: Default::default(),
                protocol_version: None,
                initialize_error: None,
                messages: Vec::new(),
            })),
            cancel: CancellationToken::new(),
            discover_started: Arc::default(),
            discover_disconnected: Arc::default(),
        };
        let state = server.state.clone();
        let cancel = server.cancel.clone();
        let discover_started = server.discover_started.clone();
        let discover_disconnected = server.discover_disconnected.clone();
        tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break };
                        let state = state.clone();
                        let discover_started = discover_started.clone();
                        let discover_disconnected = discover_disconnected.clone();
                        tasks.spawn(async move {
                            let mut bytes = Vec::new();
                            let header_end = loop {
                                let mut chunk = [0; 4096];
                                let Ok(size) = stream.read(&mut chunk).await else { return };
                                if size == 0 { return; }
                                bytes.extend_from_slice(&chunk[..size]);
                                if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") { break index + 4; }
                                if bytes.len() > 65536 { return; }
                            };
                            let headers = String::from_utf8_lossy(&bytes[..header_end]);
                            if headers.starts_with("GET ") && headers.lines().any(|line| line.to_ascii_lowercase().starts_with("last-event-id:")) {
                                let response = {
                                    let mut state = state.lock().unwrap();
                                    let response = state.directory_response.take();
                                    if response.is_some() {
                                        state.directory_resumes += 1;
                                        state.directory_resume_id = headers.lines().find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("last-event-id")).map(|(_, value)| value.trim().to_owned()));
                                    }
                                    response
                                };
                                if let Some(response) = response {
                                    let body = format!("data: {response}\n\n");
                                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                                    let _ = stream.write_all(reply.as_bytes()).await;
                                    return;
                                }
                            }
                            if !headers.starts_with("POST ") {
                                let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                                return;
                            }
                            let protocol_version = headers.lines().find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("mcp-protocol-version")).map(|(_, value)| value.trim().to_owned()));
                            let authorization = headers.lines().find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("authorization")).map(|(_, value)| value.trim().to_owned()));
                            let length = headers.lines().find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("content-length")).and_then(|(_, n)| n.trim().parse::<usize>().ok())).unwrap_or(0);
                            if length > 8 * 1024 * 1024 { return; }
                            while bytes.len() < header_end + length {
                                let mut chunk = [0; 4096];
                                let Ok(size) = stream.read(&mut chunk).await else { return };
                                if size == 0 { return; }
                                bytes.extend_from_slice(&chunk[..size]);
                            }
                            let Ok(request) = serde_json::from_slice::<Value>(&bytes[header_end..header_end + length]) else { return };
                            let (status, delay, result, error, notifications, challenge) = {
                                let mut state = state.lock().unwrap();
                                state.requests += 1;
                                let method = request["method"].as_str().unwrap_or("");
                                state.messages.push((request.clone(), protocol_version));
                                if method == "server/discover" { discover_started.notify_one(); }
                                state.received.push((method.to_owned(), tokio::time::Instant::now()));
                                if method == "server/discover" && state.close_discover { return; }
                                let result = match method {
                                    "initialize" => {
                                        let mut capabilities = json!({"tools":{"listChanged":true}});
                                        if let Some(resources) = &state.resources_capability { capabilities["resources"] = resources.clone(); }
                                        json!({"protocolVersion":state.protocol_version.as_ref().map_or_else(|| request["params"]["protocolVersion"].clone(), |version| json!(version)), "capabilities":capabilities,"serverInfo":{"name":"fixture","version":"1"},"instructions":state.instructions})
                                    }
                                    "tools/list" => json!({"tools":state.tools}),
                                    "resources/list" => json!({"resources":state.resources}),
                                    "resources/read" => json!({"contents":[{"uri":request["params"]["uri"],"text":state.resource_text}]}),
                                    "tools/call" => { state.calls.push(request.clone()); state.result.clone() },
                                    _ => json!({}),
                                };
                                let call = method == "tools/call";
                                let mut notifications = Vec::new();
                                if call && std::mem::take(&mut state.changed) { notifications.push(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}).to_string()); }
                                if (call || method == "resources/read") && std::mem::take(&mut state.resources_changed) { notifications.push(json!({"jsonrpc":"2.0","method":"notifications/resources/list_changed"}).to_string()); }
                                if call && let Some(notification) = state.notification.take() { notifications.push(notification); }
                                (if state.bearer.as_ref().is_some_and(|token| authorization.as_deref() != Some(format!("Bearer {token}").as_str())) {401} else {state.status}, if method == "server/discover" {state.discover_delay} else if call {state.delay} else if method == "tools/list" {state.directory_delay} else {std::time::Duration::ZERO}, result, if call {state.error.clone()} else if method == "initialize" {state.initialize_error.clone()} else if method == "tools/list" {state.directory_error.clone()} else {None}, notifications, state.challenge.clone())
                            };
                            if request["method"] == "server/discover" && state.lock().unwrap().stall_discover_stream {
                                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: waiting\n\n").await;
                                let mut byte = [0];
                                if peer_has_disconnected(stream.read(&mut byte).await) { discover_disconnected.notify_one(); }
                                return;
                            }
                            if request["method"] == "server/discover" && !delay.is_zero() {
                                let mut byte = [0];
                                tokio::select! {
                                    _ = tokio::time::sleep(delay) => {}
                                    read = stream.read(&mut byte) => {
                                        if peer_has_disconnected(read) { discover_disconnected.notify_one(); }
                                        return;
                                    }
                                }
                            } else { tokio::time::sleep(delay).await; }
                            let error = if request["method"] == "server/discover" {Some(json!({"code":-32601,"message":"Method not found"}))} else {error};
                            let response = if let Some(error) = error {json!({"jsonrpc":"2.0","id":request["id"],"error":error})} else {json!({"jsonrpc":"2.0","id":request["id"],"result":result})};
                            let (status, response) = if request["method"] == "server/discover"
                                && let Some((status, mut response)) = state.lock().unwrap().discover_replies.pop_front()
                            {
                                response["jsonrpc"] = json!("2.0");
                                if response.get("id").is_none() { response["id"] = request["id"].clone(); }
                                (status, response)
                            } else { (status, response) };
                            let resume_directory = {
                                let mut state = state.lock().unwrap();
                                if request["method"] == "tools/list" && std::mem::take(&mut state.resume_directory) {
                                    state.directory_response = Some(response.clone());
                                    true
                                } else { false }
                            };
                            let empty_sse_priming = state.lock().unwrap().empty_sse_priming;
                            let (status, content_type, body) = if resume_directory {
                                let data = if empty_sse_priming { "data: \n" } else { "" };
                                (200, "text/event-stream", format!("{data}id: directory-cut\nretry: 0\n\n"))
                            } else if request.get("id").is_none() {
                                (202, "application/json", String::new())
                            } else if !notifications.is_empty() || (request["method"] == "server/discover" && state.lock().unwrap().discover_error_over_sse) || (empty_sse_priming && request["method"] != "server/discover") {
                                let priming = if empty_sse_priming { "data: \nid: 0\nretry: 3000\n\n" } else { "" };
                                let events: String = notifications.iter().map(|notification| format!("data: {notification}\n\n")).collect();
                                (status, "text/event-stream", format!("{priming}{events}data: {response}\n\n"))
                            } else { (status, "application/json", response.to_string()) };
                            let (status, body) = state.lock().unwrap().http_responses.get(request["method"].as_str().unwrap_or("")).cloned().unwrap_or((status, body));
                            let challenge = challenge.map(|value| format!("WWW-Authenticate: {value}\r\n")).unwrap_or_default();
                            let body_length = body.len() + if request["method"] == "server/discover" && state.lock().unwrap().truncate_discover { 100 } else { 0 };
                            let reply = format!("HTTP/1.1 {status} Fixture\r\n{challenge}Content-Type: {content_type}\r\nContent-Length: {body_length}\r\nConnection: close\r\n\r\n{body}");
                            let _ = stream.write_all(reply.as_bytes()).await;
                        });
                    }
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });
        server
    }
    pub fn url(&self) -> String {
        self.url.clone()
    }
    pub fn set_tools(&self, tools: Value) {
        self.state.lock().unwrap().tools = tools;
    }
    pub fn set_instructions(&self, instructions: impl Into<String>) {
        self.state.lock().unwrap().instructions = instructions.into();
    }
    pub fn set_result(&self, result: Value) {
        self.state.lock().unwrap().result = result;
    }
    pub fn set_delay(&self, delay: std::time::Duration) {
        self.state.lock().unwrap().delay = delay;
    }
    pub fn set_directory_delay(&self, delay: std::time::Duration) {
        self.state.lock().unwrap().directory_delay = delay;
    }
    /// Answers every later `tools/list` with this JSON-RPC error, or normally again with `None`.
    pub fn set_directory_error(&self, error: Option<Value>) {
        self.state.lock().unwrap().directory_error = error;
    }
    /// When each request with `method` arrived, in arrival order; every request with `None`.
    pub fn arrivals<'a>(&self, method: impl Into<Option<&'a str>>) -> Vec<tokio::time::Instant> {
        let method = method.into();
        self.state
            .lock()
            .unwrap()
            .received
            .iter()
            .filter(|(received, _)| method.is_none_or(|method| received == method))
            .map(|(_, at)| *at)
            .collect()
    }
    pub fn set_status(&self, status: u16) {
        self.state.lock().unwrap().status = status;
    }
    pub fn reject_discover_over_sse(&self) {
        self.state.lock().unwrap().discover_error_over_sse = true;
    }
    pub fn set_http_response(&self, method: &str, status: u16, body: impl Into<String>) {
        self.state
            .lock()
            .unwrap()
            .http_responses
            .insert(method.into(), (status, body.into()));
    }
    pub fn set_initialize_error(&self, error: Value) {
        self.state.lock().unwrap().initialize_error = Some(error);
    }
    pub fn set_protocol_version(&self, version: &str) {
        self.state.lock().unwrap().protocol_version = Some(version.into());
    }
    pub fn methods(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .messages
            .iter()
            .map(|(message, _)| message["method"].as_str().unwrap_or("").to_owned())
            .collect()
    }
    pub fn messages(&self) -> Vec<(Value, Option<String>)> {
        self.state.lock().unwrap().messages.clone()
    }
    pub fn set_discover_replies(&self, replies: Vec<(u16, Value)>) {
        self.state.lock().unwrap().discover_replies = replies.into();
    }
    pub fn delay_discover(&self, delay: std::time::Duration) {
        self.state.lock().unwrap().discover_delay = delay;
    }
    pub fn close_discover(&self) {
        self.state.lock().unwrap().close_discover = true;
    }
    pub fn truncate_discover(&self) {
        self.state.lock().unwrap().truncate_discover = true;
    }
    pub fn stall_discover_stream(&self) {
        self.state.lock().unwrap().stall_discover_stream = true;
    }
    pub async fn discover_started(&self) {
        self.discover_started.notified().await;
    }
    pub async fn discover_disconnected(&self) {
        self.discover_disconnected.notified().await;
    }
    pub fn prime_sse_with_empty_data(&self) {
        self.state.lock().unwrap().empty_sse_priming = true;
    }
    pub fn resume_tool_directory_once(&self) {
        self.state.lock().unwrap().resume_directory = true;
    }
    pub fn directory_resumes(&self) -> usize {
        self.state.lock().unwrap().directory_resumes
    }
    pub fn directory_resume_id(&self) -> Option<String> {
        self.state.lock().unwrap().directory_resume_id.clone()
    }
    pub fn notify_log(&self, text: &str) {
        self.state.lock().unwrap().notification = Some(
            json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":text}}).to_string(),
        );
    }
    pub fn notify_custom(&self, method: &str) {
        self.state.lock().unwrap().notification =
            Some(json!({"jsonrpc":"2.0", "method":method,"params":{}}).to_string());
    }
    pub fn notify_log_with_unicode_escapes(&self, text: &str) {
        self.notify_log(text);
        let mut state = self.state.lock().unwrap();
        let wire = state.notification.take().unwrap();
        state.notification = Some(
            wire.chars()
                .map(|character| {
                    if character.is_ascii_alphabetic() {
                        format!("\\u{:04x}", character as u32)
                    } else {
                        character.to_string()
                    }
                })
                .collect(),
        );
    }
    pub fn require_bearer(&self, token: &str) {
        self.state.lock().unwrap().bearer = Some(token.into());
    }
    pub fn set_challenge(&self, challenge: &str) {
        self.state.lock().unwrap().challenge = Some(challenge.into());
    }
    pub fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests
    }
    pub fn set_error(&self, error: Option<Value>) {
        self.state.lock().unwrap().error = error;
    }
    pub fn notify_tools_changed(&self) {
        self.state.lock().unwrap().changed = true;
    }
    /// `None` initializes without the resources capability.
    pub fn set_resources_capability(&self, capability: Option<Value>) {
        self.state.lock().unwrap().resources_capability = capability;
    }
    pub fn set_resources(&self, resources: Value) {
        self.state.lock().unwrap().resources = resources;
    }
    /// The text every resource read returns.
    pub fn set_resource_text(&self, text: impl Into<String>) {
        self.state.lock().unwrap().resource_text = text.into();
    }
    /// The next tool call or resource read also delivers a resource list change.
    pub fn notify_resources_changed(&self) {
        self.state.lock().unwrap().resources_changed = true;
    }
    pub fn calls(&self) -> Vec<Value> {
        self.state.lock().unwrap().calls.clone()
    }
    pub fn stdio_args(&self) -> Vec<String> {
        vec!["-u".into(), "-c".into(), STDIO_BRIDGE.into(), self.url()]
    }
}
impl Drop for McpService {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn peer_has_disconnected(read: std::io::Result<usize>) -> bool {
    match read {
        Ok(0) => true,
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
        ),
        _ => false,
    }
}

const STDIO_BRIDGE: &str = r#"
import json, sys, urllib.request
for line in sys.stdin:
    req = urllib.request.Request(sys.argv[1], data=line.encode(), headers={'Content-Type':'application/json'})
    try:
        with urllib.request.urlopen(req) as response:
            body = response.read().decode()
            if response.headers.get('Content-Type') == 'text/event-stream':
                for event in body.splitlines():
                    if event.startswith('data: '): print(event[6:], flush=True)
            elif body: print(body, flush=True)
    except Exception:
        sys.exit(1)
"#;
