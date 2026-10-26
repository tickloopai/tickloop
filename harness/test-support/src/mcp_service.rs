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
}
struct State {
    tools: Value,
    result: Value,
    calls: Vec<Value>,
    delay: std::time::Duration,
    directory_delay: std::time::Duration,
    status: u16,
    error: Option<Value>,
    changed: bool,
    challenge: Option<String>,
    bearer: Option<String>,
    requests: usize,
    notification: Option<String>,
    resources_capability: Option<Value>,
    resources: Value,
    resource_text: String,
    resources_changed: bool,
}
impl McpService {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Self {
            url: format!("http://{}/mcp", listener.local_addr().unwrap()),
            state: Arc::new(Mutex::new(State {
                tools: json!([]),
                result: json!({"content":[]}),
                calls: Vec::new(),
                delay: std::time::Duration::ZERO,
                directory_delay: std::time::Duration::ZERO,
                status: 200,
                error: None,
                changed: false,
                challenge: None,
                bearer: None,
                requests: 0,
                notification: None,
                resources_capability: Some(json!({})),
                resources: json!([{"uri":"fixture://text","name":"text"}]),
                resource_text: "resource text".into(),
                resources_changed: false,
            })),
            cancel: CancellationToken::new(),
        };
        let state = server.state.clone();
        let cancel = server.cancel.clone();
        tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break };
                        let state = state.clone();
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
                            if !headers.starts_with("POST ") {
                                let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                                return;
                            }
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
                                let result = match method {
                                    "initialize" => {
                                        let mut capabilities = json!({"tools":{"listChanged":true}});
                                        if let Some(resources) = &state.resources_capability { capabilities["resources"] = resources.clone(); }
                                        json!({"protocolVersion":request["params"]["protocolVersion"], "capabilities":capabilities,"serverInfo":{"name":"fixture","version":"1"},"instructions":"Fixture service instructions"})
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
                                (if state.bearer.as_ref().is_some_and(|token| authorization.as_deref() != Some(format!("Bearer {token}").as_str())) {401} else {state.status}, if call {state.delay} else if method == "tools/list" {state.directory_delay} else {std::time::Duration::ZERO}, result, if call {state.error.clone()} else {None}, notifications, state.challenge.clone())
                            };
                            tokio::time::sleep(delay).await;
                            let error = if request["method"] == "server/discover" {Some(json!({"code":-32601,"message":"Method not found"}))} else {error};
                            let response = if let Some(error) = error {json!({"jsonrpc":"2.0","id":request["id"],"error":error})} else {json!({"jsonrpc":"2.0","id":request["id"],"result":result})};
                            let (status, content_type, body) = if request.get("id").is_none() {
                                (202, "application/json", String::new())
                            } else if !notifications.is_empty() {
                                let events: String = notifications.iter().map(|notification| format!("data: {notification}\n\n")).collect();
                                (status, "text/event-stream", format!("{events}data: {response}\n\n"))
                            } else { (status, "application/json", response.to_string()) };
                            let challenge = challenge.map(|value| format!("WWW-Authenticate: {value}\r\n")).unwrap_or_default();
                            let reply = format!("HTTP/1.1 {status} Fixture\r\n{challenge}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
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
    pub fn set_result(&self, result: Value) {
        self.state.lock().unwrap().result = result;
    }
    pub fn set_delay(&self, delay: std::time::Duration) {
        self.state.lock().unwrap().delay = delay;
    }
    pub fn set_directory_delay(&self, delay: std::time::Duration) {
        self.state.lock().unwrap().directory_delay = delay;
    }
    pub fn set_status(&self, status: u16) {
        self.state.lock().unwrap().status = status;
    }
    pub fn notify_log(&self, text: &str) {
        self.state.lock().unwrap().notification = Some(
            json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":text}}).to_string(),
        );
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
