//! ACP through the real CLI, daemon, worker and local HTTP model transport.
use super::support;
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tickloop_code_access::{
    AgentAccess, AgentFilter, AgentSummary, ObservationRequest, ObservationValue, Page,
};
use tickloop_code_client::LocalAgentClient;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdin, ChildStdout},
};

const TIMEOUT: Duration = Duration::from_secs(45);

struct Fixture {
    directory: tempfile::TempDir,
    runtime: PathBuf,
    model: PathBuf,
}
impl Fixture {
    fn new(address: std::net::SocketAddr, rules: Value) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().canonicalize().unwrap();
        let root = directory.path().join("config");
        std::fs::create_dir(&root).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(directory.path().join("home")).unwrap();
        let mut settings = json!({"promptSuggestionEnabled":false, "defaultModel":{"provider":"deepseek-api","model":"deepseek-flash"}});
        settings["projects"][folder.to_str().unwrap()] = json!({"permissions":{"rules":rules}});
        std::fs::write(root.join("settings.json"), settings.to_string()).unwrap();
        let model = directory.path().join("model.json");
        std::fs::write(&model, json!({"name":"ACP fixture", "protocol":"messages", "base_url":format!("http://{address}"), "model":"fixture", "api_key_env":"", "max_output_tokens":256}).to_string()).unwrap();
        Self {
            directory,
            runtime: support::cli_runtime::adopt(&root),
            model,
        }
    }
    fn folder(&self) -> PathBuf {
        self.directory.path().canonicalize().unwrap()
    }
    fn spawn(&self) -> Wire {
        let mut command = support::fixture_cli::tokio_command();
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.directory.path().join("home"))
            .arg("acp")
            .arg("--working-directory-is-trusted")
            .arg("--transport-fixture")
            .arg(&self.model)
            .current_dir(self.folder())
            .env("TICKLOOP_CONFIG_DIR", self.folder().join("config"))
            .env_remove("HERDR_SOCKET")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_PANE_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut stderr = child.stderr.take().unwrap();
        let errors = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.unwrap();
            String::from_utf8_lossy(&bytes).into_owned()
        });
        Wire {
            child,
            input: Some(input),
            output,
            errors,
            frames: Vec::new(),
        }
    }
    fn transcript_jsonl(&self) -> String {
        let projects = self.folder().join("config/projects");
        let mut text = String::new();
        for project in std::fs::read_dir(projects).unwrap() {
            let project = project.unwrap().path();
            if !project.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(project).unwrap() {
                let path = entry.unwrap().path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                {
                    text.push_str(&std::fs::read_to_string(path).unwrap());
                }
            }
        }
        assert!(!text.is_empty(), "expected persisted fixture Transcript");
        text
    }
    async fn failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        let client = LocalAgentClient::connect(self.runtime.join("daemon.sock"))
            .await
            .unwrap();
        for row in self.catalog().await {
            let observation = client
                .observe(ObservationRequest::Agent {
                    execution: row.execution,
                    detail: tickloop_code_access::AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap();
            if let ObservationValue::Agent(snapshot) = observation.initial {
                for event in snapshot.events {
                    if let tickloop_harness::protocol::EventKind::Failed(message) = event.kind {
                        failures.push(message);
                    }
                }
            }
        }
        failures
    }
    async fn catalog(&self) -> Vec<AgentSummary> {
        let client = LocalAgentClient::connect(self.runtime.join("daemon.sock"))
            .await
            .unwrap();
        let observation = client
            .observe(ObservationRequest::Overview {
                filter: AgentFilter::default(),
                page: Page {
                    offset: 0,
                    limit: 64,
                },
                after: None,
            })
            .await
            .unwrap();
        let ObservationValue::Overview(rows) = observation.initial else {
            panic!("expected overview")
        };
        rows
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            for path in [
                self.model.with_extension("panic.log"),
                self.folder().join("config/fixture-process.log"),
            ] {
                if let Ok(log) = std::fs::read_to_string(path) {
                    eprintln!("Fixture child diagnostic: {log}");
                }
            }
        }
    }
}
struct Wire {
    child: Child,
    input: Option<ChildStdin>,
    output: Lines<BufReader<ChildStdout>>,
    errors: tokio::task::JoinHandle<String>,
    frames: Vec<Value>,
}
impl Wire {
    async fn send(&mut self, value: Value) {
        let bytes = format!("{value}\n");
        tokio::time::timeout(
            TIMEOUT,
            self.input.as_mut().unwrap().write_all(bytes.as_bytes()),
        )
        .await
        .expect("ACP input write timed out")
        .unwrap();
    }
    async fn request(&mut self, id: u64, method: &str, params: Value) {
        self.send(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
            .await;
    }
    fn record(&mut self, value: Value) {
        let params = &value["params"];
        let call = if params["update"]["sessionUpdate"] == "tool_call_update" {
            Some(&params["update"]["toolCallId"])
        } else if value["method"] == "session/request_permission" {
            Some(&params["toolCall"]["toolCallId"])
        } else {
            None
        };
        if let Some(call) = call {
            assert!(
                self.frames.iter().any(|previous| {
                    previous["method"] == "session/update"
                        && previous["params"]["sessionId"] == params["sessionId"]
                        && previous["params"]["update"]["sessionUpdate"] == "tool_call"
                        && previous["params"]["update"]["toolCallId"] == *call
                }),
                "tool update/permission preceded initial tool_call: {value}"
            );
        }
        self.frames.push(value);
    }
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(TIMEOUT, self.output.next_line())
            .await
            .expect("ACP frame timed out")
            .unwrap()
            .expect("ACP stdout closed unexpectedly");
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("non-ACP stdout {line:?}: {error}"));
        assert_eq!(value["jsonrpc"], "2.0", "{value}");
        self.record(value.clone());
        value
    }
    async fn wait_for_pid(&mut self, marker: &std::path::Path) -> u32 {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Ok(text) = std::fs::read_to_string(marker)
                    && let Ok(pid) = text.parse()
                {
                    return pid;
                }
                tokio::select! {
                    line = self.output.next_line() => {
                        let line = line.unwrap().expect("ACP closed before Shell started");
                        let frame: Value = serde_json::from_str(&line).unwrap();
                        assert!(frame.get("error").is_none(), "Shell startup failed: {frame}");
                        if frame["method"] == "session/request_permission" {
                            self.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}})).await;
                        }
                        self.record(frame);
                    }
                    () = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        }).await.expect("controlled Shell did not start")
    }
    async fn response(&mut self, id: u64) -> Value {
        loop {
            let value = self.next().await;
            if value["id"] == id && value.get("method").is_none() {
                if value.get("error").is_some() {
                    eprintln!("ACP error response: {value}");
                }
                return value;
            }
        }
    }
    async fn initialize(&mut self) {
        self.request(1,"initialize",json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"fullchain-test","version":"1"}})).await;
        let value = self.response(1).await;
        assert_eq!(value["result"]["protocolVersion"], 1, "{value}");
        assert!(
            value["result"]["agentCapabilities"]["loadSession"].is_boolean(),
            "{value}"
        );
    }
    async fn session(&mut self, id: u64, fixture: &Fixture) -> String {
        self.request(
            id,
            "session/new",
            json!({"cwd":fixture.folder(),"mcpServers":[]}),
        )
        .await;
        let response = self.response(id).await;
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{response}"))
            .to_owned()
    }
    async fn load(&mut self, id: u64, session: &str, cwd: &std::path::Path) -> Value {
        self.request(
            id,
            "session/load",
            json!({"sessionId":session,"cwd":cwd,"mcpServers":[]}),
        )
        .await;
        self.response(id).await
    }
    async fn mcp_session(&mut self, fixture: &Fixture, server: Value) -> String {
        self.request(
            2,
            "session/new",
            json!({"cwd":fixture.folder(),"mcpServers":[server]}),
        )
        .await;
        let response = self.response(2).await;
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{response}"))
            .to_owned()
    }
    async fn permission(&mut self) -> Value {
        loop {
            let frame = self.next().await;
            assert!(frame.get("error").is_none(), "expected permission: {frame}");
            if frame["method"] == "session/request_permission" {
                return frame;
            }
            assert!(
                frame.get("method").is_some(),
                "prompt ended before permission: {frame}"
            );
        }
    }
    async fn decide(&mut self, permission: &Value, option: &str) {
        self.send(json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":option}}})).await;
    }
    async fn prompt(&mut self, id: u64, session: &str, text: &str) {
        self.request(
            id,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}),
        )
        .await;
    }
    fn text(&self, session: &str) -> String {
        self.frames
            .iter()
            .filter(|frame| {
                frame["method"] == "session/update"
                    && frame["params"]["sessionId"] == session
                    && frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
            })
            .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
            .collect()
    }
    async fn finish(mut self) {
        drop(self.input.take());
        tokio::time::timeout(TIMEOUT, async {
            while let Some(line) = self.output.next_line().await.unwrap() {
                let frame: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("non-ACP stdout at EOF {line:?}: {error}"));
                assert_eq!(frame["jsonrpc"], "2.0");
            }
        })
        .await
        .expect("ACP did not close stdout after EOF");
        let status = tokio::time::timeout(TIMEOUT, self.child.wait())
            .await
            .expect("ACP did not exit after EOF")
            .unwrap();
        let errors = tokio::time::timeout(TIMEOUT, self.errors)
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "ACP exited {status}: {errors}");
    }
}
async fn read_request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0, "HTTP body truncated");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::to_owned)
                })
                .unwrap()
                .parse()
                .unwrap();
            if bytes.len() >= end + 4 + length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
            }
        }
    }
}
async fn respond(socket: &mut TcpStream, body: &str) {
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
}
fn stream(content: Value, delta: Value, stop: &str) -> String {
    [json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
    json!({"type":"content_block_start","index":0,"content_block":content}),
    json!({"type":"content_block_delta","index":0,"delta":delta}),
    json!({"type":"content_block_stop","index":0}),
    json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":4}}),json!({"type":"message_stop"})].iter().map(|event|format!("data: {event}\n\n")).collect()
}
fn text_stream(text: &str) -> String {
    stream(
        json!({"type":"text","text":""}),
        json!({"type":"text_delta","text":text}),
        "end_turn",
    )
}
fn tool_stream(name: &str, input: Value) -> String {
    stream(
        json!({"type":"tool_use","id":tickloop_harness::ids::generate().to_string(),"name":name,"input":{}}),
        json!({"type":"input_json_delta","partial_json":input.to_string()}),
        "tool_use",
    )
}
fn ask_rule(tool: &str) -> Value {
    json!([{"id":tickloop_harness::ids::generate(),"decision":"ask","tool":tool}])
}

#[tokio::test]
async fn acp_initialize_creates_no_execution_and_rejects_unsupported_inputs() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(listener.local_addr().unwrap(), json!([]));
    let mut wire = fixture.spawn();
    wire.initialize().await;
    assert!(fixture.catalog().await.is_empty());
    wire.request(2,"session/new",json!({"cwd":fixture.folder(),"mcpServers":[{"name":"unsupported","command":"false","args":[],"env":[]}]})).await;
    assert!(wire.response(2).await.get("error").is_some());
    assert!(
        fixture.catalog().await.is_empty(),
        "MCP rejection must precede execution creation"
    );
    let session = wire.session(3, &fixture).await;
    wire.request(4,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"image","data":"AA==","mimeType":"image/png"}]})).await;
    assert!(wire.response(4).await.get("error").is_some());
    assert_eq!(fixture.catalog().await.len(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), listener.accept())
            .await
            .is_err(),
        "initialize/new/invalid content must not call the model"
    );
    wire.finish().await;
}

#[tokio::test]
async fn acp_real_model_multiple_turns_and_sessions_keep_context_isolated() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(listener.local_addr().unwrap(), json!([]));
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for answer in [
            "Remembered violet.",
            "The word is violet.",
            "Independent answer.",
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            respond(&mut socket, &text_stream(answer)).await;
        }
        requests
    });
    let mut wire = fixture.spawn();
    wire.initialize().await;
    let first = wire.session(2, &fixture).await;
    let second = wire.session(3, &fixture).await;
    assert_ne!(first, second);
    wire.prompt(4, &first, "Remember the secret word violet.")
        .await;
    let response = wire.response(4).await;
    assert_eq!(
        response["result"]["stopReason"],
        "end_turn",
        "{response}; runtime failures: {:?}",
        fixture.failures().await
    );
    wire.prompt(5, &first, "Recall my secret word.").await;
    assert_eq!(wire.response(5).await["result"]["stopReason"], "end_turn");
    wire.prompt(6, &second, "Independent session request.")
        .await;
    assert_eq!(wire.response(6).await["result"]["stopReason"], "end_turn");
    assert_eq!(wire.text(&first), "Remembered violet.The word is violet.");
    assert_eq!(wire.text(&second), "Independent answer.");
    let requests = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Remember the secret word violet.")
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Remembered violet.")
    );
    assert!(!requests[2]["messages"].to_string().contains("violet"));
    assert_eq!(fixture.catalog().await.len(), 2);
    wire.finish().await;
    assert!(
        fixture.runtime.join("daemon.sock").exists(),
        "EOF must leave the shared daemon running"
    );
}

/// A second signed-in provider's models are offered beside the current provider's, grouped by
/// provider, and choosing one switches the session to that provider.
#[tokio::test]
async fn acp_offers_every_signed_in_providers_models_and_switches_between_them() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(listener.local_addr().unwrap(), json!([]));
    tickloop_harness::model::credentials::save_api_key(
        &fixture.folder().join("config"),
        "ZAI_API_KEY",
        "fixture-key",
    )
    .unwrap();
    let mut wire = fixture.spawn();
    wire.initialize().await;
    wire.request(
        2,
        "session/new",
        json!({"cwd":fixture.folder(),"mcpServers":[]}),
    )
    .await;
    let created = wire.response(2).await;
    let model = &created["result"]["configOptions"][0];
    assert_eq!(model["currentValue"], "deepseek-flash", "{created}");
    let groups: Vec<_> = model["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["name"].as_str().unwrap())
        .collect();
    assert_eq!(groups, ["DeepSeek API", "Z.AI Global API"], "{created}");
    let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
    wire.request(
        3,
        "session/set_config_option",
        json!({"sessionId":session,"configId":"model","type":"select","value":"glm-5.3"}),
    )
    .await;
    let switched = wire.response(3).await;
    assert_eq!(
        switched["result"]["configOptions"][0]["currentValue"], "glm-5.3",
        "{switched}"
    );
    wire.finish().await;
}

#[tokio::test]
async fn acp_permission_roundtrip_executes_once_and_cancel_does_not_block_on_permission() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(listener.local_addr().unwrap(), ask_rule("Write"));
    let path = fixture.folder().join("approved.txt");
    let denied_path = fixture.folder().join("cancelled.txt");
    let tool_path = path.clone();
    let cancel_path = denied_path.clone();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in [
            tool_stream(
                "Write",
                json!({"file_path":tool_path,"content":"approved once"}),
            ),
            text_stream("Written with approval."),
            tool_stream(
                "Write",
                json!({"file_path":cancel_path,"content":"must not write"}),
            ),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            respond(&mut socket, &body).await;
        }
        requests
    });
    let mut wire = fixture.spawn();
    wire.initialize().await;
    let session = wire.session(2, &fixture).await;
    wire.prompt(3, &session, "Write the approved note.").await;
    let permission = loop {
        let value = wire.next().await;
        if value["method"] == "session/request_permission" {
            break value;
        }
    };
    assert!(!path.exists(), "tool must wait for ACP permission response");
    assert_eq!(permission["params"]["options"][0]["optionId"], "allow_once");
    wire.send(json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}})).await;
    assert_eq!(wire.response(3).await["result"]["stopReason"], "end_turn");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "approved once");
    assert_eq!(
        wire.frames
            .iter()
            .filter(|frame| frame["method"] == "session/request_permission")
            .count(),
        1,
        "replay must not request permission twice"
    );
    wire.prompt(4, &session, "Write another note that will be cancelled.")
        .await;
    loop {
        if wire.next().await["method"] == "session/request_permission" {
            break;
        }
    }
    wire.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    let cancelled = tokio::time::timeout(Duration::from_secs(15), wire.response(4))
        .await
        .expect("cancel blocked behind permission request");
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );
    assert!(!denied_path.exists());
    let requests = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert!(requests[1]["messages"].to_string().contains("tool_result"));
    assert!(
        serde_json::to_string(&wire.frames)
            .unwrap()
            .contains("+approved once")
    );
    wire.finish().await;
    // Restoring must use the recorded hunks, never read the now-unrelated live file.
    std::fs::write(&path, "unrelated current file").unwrap();
    let mut restored = fixture.spawn();
    restored.initialize().await;
    restored
        .request(
            2,
            "session/load",
            json!({"sessionId":session,"cwd":fixture.folder(),"mcpServers":[]}),
        )
        .await;
    let response = restored.response(2).await;
    assert!(response.get("result").is_some(), "{response}");
    let replay = serde_json::to_string(&restored.frames).unwrap();
    assert!(
        replay.contains("+approved once"),
        "recorded edit missing from history: {replay}"
    );
    assert!(!replay.contains("unrelated current file"));
    restored.finish().await;
}

#[tokio::test]
async fn acp_cancel_pending_model_and_eof_release_only_owned_work() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(listener.local_addr().unwrap(), json!([]));
    let (seen, mut requests) = tokio::sync::mpsc::channel(4);
    let server = tokio::spawn(async move {
        let mut sockets = Vec::new();
        for index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            seen.send(index).await.unwrap();
            if index == 1 {
                respond(&mut socket, &text_stream("Still usable after cancel.")).await;
            } else {
                sockets.push(socket);
            }
        }
        // Retain pending model connections until the test has verified cancellation and EOF.
        std::future::pending::<()>().await;
    });
    let mut wire = fixture.spawn();
    wire.initialize().await;
    let session = wire.session(2, &fixture).await;
    wire.prompt(3, &session, "Wait for model cancellation.")
        .await;
    tokio::time::timeout(TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    wire.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    let cancelled = tokio::time::timeout(Duration::from_secs(15), wire.response(3))
        .await
        .expect("cancel blocked behind model request");
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );
    wire.prompt(4, &session, "New prompt after cancellation.")
        .await;
    assert_eq!(wire.response(4).await["result"]["stopReason"], "end_turn");
    tokio::time::timeout(TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    wire.prompt(5, &session, "Pending model when stdin closes.")
        .await;
    tokio::time::timeout(TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    wire.finish().await;
    assert!(fixture.runtime.join("daemon.sock").exists());
    for row in fixture.catalog().await {
        assert_eq!(row.waiting_permissions, 0);
        assert_eq!(row.waiting_questions, 0);
        assert!(
            !matches!(row.state, tickloop_harness::protocol::LoopState::Running),
            "EOF left a model running: {row:?}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn acp_cancel_running_shell_stops_the_process_and_returns_prompt_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(
        listener.local_addr().unwrap(),
        json!([{
            "id":tickloop_harness::ids::generate(), "decision":"ask", "tool":"Bash"
        }]),
    );
    let marker = fixture.folder().join("foreground.pid");
    let command = format!("printf '%s' $$ > '{}'; exec sleep 300", marker.display());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        respond(
            &mut socket,
            &tool_stream("Bash", json!({"command":command})),
        )
        .await;
    });
    let mut wire = fixture.spawn();
    wire.initialize().await;
    let session = wire.session(2, &fixture).await;
    wire.prompt(
        3,
        &session,
        "Run the controlled long shell until cancelled.",
    )
    .await;
    let pid = wire.wait_for_pid(&marker).await;
    wire.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    let response = tokio::time::timeout(Duration::from_secs(15), wire.response(3))
        .await
        .expect("cancel blocked behind Shell tool");
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    tokio::time::timeout(Duration::from_secs(15), async {
        while !support::has_exited(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("ACP cancellation left its Shell alive");
    wire.finish().await;
    tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
}

/// Settling one prompt ends its hosted Ticks without fencing the Chat: the next prompt of the
/// same session opens Ticks again.
#[tokio::test]
async fn acp_each_prompt_opens_hosted_ticks_and_settlement_ends_only_its_own() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(
        listener.local_addr().unwrap(),
        json!([{"id":tickloop_harness::ids::generate(), "decision":"allow", "tool":"TickManage"}]),
    );
    let markers = ["first.pid", "second.pid"].map(|name| fixture.folder().join(name));
    let opens = markers.clone().map(|marker| {
        let command = format!("printf '%s' $$ > '{}'; exec sleep 300", marker.display());
        tool_stream(
            "TickManage",
            json!({"action":"open","command":["/bin/sh","-c",command]}),
        )
    });
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in [
            opens[0].clone(),
            text_stream("first opened"),
            opens[1].clone(),
            text_stream("second opened"),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            respond(&mut socket, &body).await;
        }
        requests
    });
    let mut wire = fixture.spawn();
    wire.initialize().await;
    let session = wire.session(2, &fixture).await;
    let mut pids = Vec::new();
    for (id, marker) in [(3, &markers[0]), (4, &markers[1])] {
        wire.prompt(id, &session, "Open the controlled Tick.").await;
        let response = wire.response(id).await;
        assert_eq!(
            response["result"]["stopReason"],
            "end_turn",
            "{response}; runtime failures: {:?}",
            fixture.failures().await
        );
        let pid: u32 = std::fs::read_to_string(marker)
            .unwrap_or_else(|error| panic!("prompt {id} opened no Tick: {error}"))
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while !support::has_exited(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("settlement left the prompt's hosted Tick running");
        pids.push(pid);
    }
    assert_ne!(pids[0], pids[1]);
    let requests = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    let second_result = requests[3]["messages"].to_string();
    assert!(second_result.contains("hosted:"), "{second_result}");
    assert!(!second_result.contains("OwnerEnded"), "{second_result}");
    wire.finish().await;
}

#[tokio::test]
async fn acp_process_crash_expires_lease_and_stops_its_shell() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(
        listener.local_addr().unwrap(),
        json!([{
            "id":tickloop_harness::ids::generate(), "decision":"ask", "tool":"Bash"
        }]),
    );
    let marker = fixture.folder().join("crash.pid");
    let command = format!("printf '%s' $$ > '{}'; exec sleep 300", marker.display());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        respond(
            &mut socket,
            &tool_stream("Bash", json!({"command":command})),
        )
        .await;
    });
    let mut wire = fixture.spawn();
    wire.initialize().await;
    let session = wire.session(2, &fixture).await;
    wire.prompt(
        3,
        &session,
        "Run a controlled shell to test client crash recovery.",
    )
    .await;
    let pid = wire.wait_for_pid(&marker).await;
    assert!(!support::has_exited(pid));
    wire.child.kill().await.unwrap(); // SIGKILL cannot run the adapter's explicit release path.
    wire.input.take();
    tokio::time::timeout(Duration::from_secs(70), async {
        while !support::has_exited(pid) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("expired managed lease left its Shell alive");
    assert!(fixture.runtime.join("daemon.sock").exists());
    wire.errors.await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn acp_eof_stops_owned_background_shell_but_another_connection_keeps_working() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(
        listener.local_addr().unwrap(),
        json!([{
            "id":tickloop_harness::ids::generate(), "decision":"ask", "tool":"Bash"
        }]),
    );
    let marker = fixture.folder().join("background.pid");
    let command = format!("printf '%s' $$ > '{}'; exec sleep 300", marker.display());
    let (pending, pending_seen) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        respond(
            &mut socket,
            &tool_stream("Bash", json!({"command":command,"run_in_background":true})),
        )
        .await;
        let (mut held, _) = listener.accept().await.unwrap();
        read_request(&mut held).await;
        pending.send(()).unwrap();
        for text in [
            "Other connection still works.",
            "Other connection survived EOF.",
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            respond(&mut socket, &text_stream(text)).await;
        }
        drop(held);
    });
    let mut owner = fixture.spawn();
    owner.initialize().await;
    let owned_session = owner.session(2, &fixture).await;
    owner
        .prompt(
            3,
            &owned_session,
            "Start a background shell until this connection closes.",
        )
        .await;
    let pid = owner.wait_for_pid(&marker).await;
    tokio::time::timeout(TIMEOUT, pending_seen)
        .await
        .unwrap()
        .unwrap();
    assert!(!support::has_exited(pid));
    let mut other = fixture.spawn();
    other.initialize().await;
    let other_session = other.session(2, &fixture).await;
    other
        .prompt(3, &other_session, "Check unrelated connection before EOF.")
        .await;
    assert_eq!(other.response(3).await["result"]["stopReason"], "end_turn");
    owner.finish().await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while !support::has_exited(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("ACP EOF left its background Shell alive");
    other
        .prompt(4, &other_session, "Check unrelated connection after EOF.")
        .await;
    assert_eq!(other.response(4).await["result"]["stopReason"], "end_turn");
    assert_eq!(
        other.text(&other_session),
        "Other connection still works.Other connection survived EOF."
    );
    other.finish().await;
    tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn acp_history_eof_load_replays_public_messages_and_continues_same_uuid() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(listener.local_addr().unwrap(), json!([]));
    let mut first = fixture.spawn();
    first.initialize().await;
    let session = first.session(2, &fixture).await;
    uuid::Uuid::parse_str(&session).expect("ACP session must be its persistent Chat UUID");

    // A second real ACP client cannot acquire the still-live managed execution.
    let mut next = fixture.spawn();
    next.initialize().await;
    let busy = next.load(2, &session, &fixture.folder()).await;
    assert!(
        busy.get("error").is_some(),
        "live load unexpectedly succeeded: {busy}"
    );
    assert!(next.text(&session).is_empty());
    next.finish().await;

    first
        .prompt(3, &session, "Remember the history word marigold.")
        .await;
    let (mut socket, _) = tokio::time::timeout(TIMEOUT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let initial_request = read_request(&mut socket).await;
    respond(&mut socket, &text_stream("Recorded marigold.")).await;
    assert_eq!(first.response(3).await["result"]["stopReason"], "end_turn");
    first.finish().await;

    let mut next = fixture.spawn();
    next.initialize().await;
    // Changing cwd cannot silently reinterpret an existing Transcript's authorization scope.
    let wrong_cwd = fixture.folder().join("different-project");
    std::fs::create_dir(&wrong_cwd).unwrap();
    let wrong = next.load(3, &session, &wrong_cwd).await;
    assert!(
        wrong.get("error").is_some(),
        "wrong-cwd load unexpectedly succeeded: {wrong}"
    );
    assert!(next.text(&session).is_empty());

    let loaded = next.load(4, &session, &fixture.folder()).await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert!(loaded.get("error").is_none(), "{loaded}");
    assert_eq!(next.text(&session), "Recorded marigold.");
    let public_prompt: String = next
        .frames
        .iter()
        .filter(|frame| {
            frame["method"] == "session/update"
                && frame["params"]["sessionId"] == session
                && frame["params"]["update"]["sessionUpdate"] == "user_message_chunk"
        })
        .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
        .collect();
    assert_eq!(public_prompt, "Remember the history word marigold.");
    assert!(
        next.frames
            .iter()
            .all(|frame| frame["method"] != "session/request_permission"),
        "history must not re-execute operations"
    );
    assert_no_model(&listener).await;

    next.prompt(
        5,
        &session,
        "Recall the word from before this client connected.",
    )
    .await;
    let (mut socket, _) = tokio::time::timeout(TIMEOUT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let continued = read_request(&mut socket).await;
    respond(&mut socket, &text_stream("It was marigold.")).await;
    assert_eq!(next.response(5).await["result"]["stopReason"], "end_turn");
    let context = continued["messages"].to_string();
    assert!(
        context.contains("Remember the history word marigold."),
        "{continued}"
    );
    assert!(context.contains("Recorded marigold."), "{continued}");
    assert!(context.contains("Recall the word from before this client connected."));
    assert!(initial_request["messages"].to_string().contains("marigold"));
    assert_eq!(next.text(&session), "Recorded marigold.It was marigold.");
    next.finish().await;
    let transcript = fixture.transcript_jsonl();
    assert!(transcript.contains("Recorded marigold."));
    assert!(transcript.contains("It was marigold."));
    assert_no_model(&listener).await;
}

#[tokio::test]
async fn acp_empty_session_survives_eof_and_is_listed_and_loaded_without_a_model() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(listener.local_addr().unwrap(), json!([]));
    let mut first = fixture.spawn();
    first.initialize().await;
    let session = first.session(2, &fixture).await;
    first.finish().await;

    let mut next = fixture.spawn();
    next.initialize().await;
    next.request(2, "session/list", json!({"cwd":fixture.folder()}))
        .await;
    let listed = next.response(2).await;
    assert!(listed.get("error").is_none(), "{listed}");
    assert!(
        listed["result"]["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
            .iter()
            .any(|row| row["sessionId"] == session),
        "empty session disappeared: {listed}"
    );
    let loaded = next.load(3, &session, &fixture.folder()).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert!(next.text(&session).is_empty());
    assert!(
        !next.frames.iter().any(|frame| matches!(
            frame["params"]["update"]["sessionUpdate"].as_str(),
            Some("user_message_chunk" | "agent_message_chunk" | "tool_call")
        )),
        "empty history manufactured public content"
    );
    next.finish().await;
    assert_no_model(&listener).await;
    assert!(
        !fixture
            .transcript_jsonl()
            .contains("\"role\":\"assistant\"")
    );
}

async fn assert_no_model(listener: &TcpListener) {
    assert!(
        tokio::time::timeout(Duration::from_millis(150), listener.accept())
            .await
            .is_err(),
        "operation unexpectedly invoked the HTTP model"
    );
}

const MCP_ARG_SECRET: &str = "acp-argv-secret-do-not-persist-8041";
const MCP_ENV_SECRET: &str = "acp-env-secret-do-not-persist-5286";

struct StdioMcp {
    program: PathBuf,
    pid: PathBuf,
    log: PathBuf,
}
impl StdioMcp {
    fn new(fixture: &Fixture) -> Self {
        use std::os::unix::fs::PermissionsExt;
        // An absolute interpreter and executable script avoid inherited PATH and user packages.
        assert!(
            std::path::Path::new("/usr/bin/python3").is_file(),
            "stdio MCP fixture requires /usr/bin/python3"
        );
        let program = fixture.folder().join("fake-mcp.py");
        let pid = fixture.folder().join("mcp.pid");
        let log = fixture.folder().join("mcp-requests.jsonl");
        std::fs::write(&program, r#"#!/usr/bin/python3
import json, os, subprocess, sys
pid_file, log_file, arg_secret = sys.argv[1:4]
env_secret = os.environ['ACP_FIXTURE_SECRET']
with open(pid_file, 'w') as marker:
    marker.write(str(os.getpid()))
def record(method, **extra):
    with open(log_file, 'a') as log:
        log.write(json.dumps(dict(method=method, pid=os.getpid(), **extra)) + '\n')
descendant = subprocess.Popen(['/bin/sleep', '600'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
supervisor = os.getppid()
worker = int(subprocess.check_output(['/bin/ps', '-o', 'ppid=', '-p', str(supervisor)]).strip())
record('spawn', supervisor=supervisor, worker=worker, descendant=descendant.pid, secrets_received=(arg_secret == 'acp-argv-secret-do-not-persist-8041' and env_secret == 'acp-env-secret-do-not-persist-5286'))
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    record(method)
    if 'id' not in request:
        continue
    if method == 'initialize':
        result = dict(protocolVersion=request['params']['protocolVersion'], capabilities=dict(tools={}), serverInfo=dict(name='isolated-fixture', version='1'))
    elif method == 'tools/list':
        result = dict(tools=[dict(name='echo', description='Return controlled test data', inputSchema=dict(type='object', properties=dict(message=dict(type='string')), required=['message']), annotations=dict(readOnlyHint=True))])
    elif method == 'tools/call':
        result = dict(content=[dict(type='text', text='stdio-pipe-result pid=' + str(os.getpid()) + ' message=' + request['params']['arguments']['message'] + ' ' + arg_secret + ' ' + env_secret)])
    else:
        print(json.dumps(dict(jsonrpc='2.0', id=request['id'], error=dict(code=-32601, message='Unsupported fixture method'))), flush=True)
        continue
    print(json.dumps(dict(jsonrpc='2.0', id=request['id'], result=result)), flush=True)
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { program, pid, log }
    }
    fn descriptor(&self) -> Value {
        json!({"name":"fixture","command":self.program,"args":[self.pid,self.log,MCP_ARG_SECRET],"env":[{"name":"ACP_FIXTURE_SECRET","value":MCP_ENV_SECRET}]})
    }
    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn assert_not_spawned(&self) {
        assert!(!self.pid.exists(), "MCP executable ran before permission");
        assert!(
            !self.log.exists(),
            "MCP executable produced startup side effects"
        );
    }
    fn assert_approval(&self, permission: &Value) {
        assert_eq!(
            permission["params"]["toolCall"]["title"], "MCPServerStart",
            "{permission}"
        );
        assert!(
            permission["params"]["toolCall"]["rawInput"]["arguments"]
                .to_string()
                .contains(MCP_ARG_SECRET),
            "transient argv must be shown for approval"
        );
        assert!(
            !permission.to_string().contains(MCP_ENV_SECRET),
            "environment values leaked into approval"
        );
        assert!(
            permission["params"]["toolCall"]["rawInput"]["notice"]
                .as_str()
                .unwrap()
                .contains("unsandboxed")
        );
        self.assert_not_spawned();
    }
}

#[tokio::test]
async fn acp_stdio_mcp_real_permission_pipe_reuse_redaction_and_eof_cleanup() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(
        listener.local_addr().unwrap(),
        ask_rule("mcp__fixture__echo"),
    );
    let mcp = StdioMcp::new(&fixture);
    let mut wire = fixture.spawn();
    wire.initialize().await;
    let session = wire.mcp_session(&fixture, mcp.descriptor()).await;
    mcp.assert_not_spawned();
    assert_no_model(&listener).await;

    wire.prompt(
        3,
        &session,
        "Use the approved stdio fixture for the first turn.",
    )
    .await;
    let startup = wire.permission().await;
    mcp.assert_approval(&startup);
    assert_no_model(&listener).await;
    wire.decide(&startup, "allow_once").await;

    let mut requests = Vec::new();
    for (id, message) in [(3, "first"), (4, "second")] {
        if id == 4 {
            wire.prompt(id, &session, "Use the same stdio fixture again.")
                .await;
        }
        let (mut socket, _) = tokio::time::timeout(TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let request = read_request(&mut socket).await;
        assert!(
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "mcp__fixture__echo"),
            "MCP discovery did not reach the real model: {request}"
        );
        respond(
            &mut socket,
            &tool_stream("mcp__fixture__echo", json!({"message":message})),
        )
        .await;
        requests.push(request);
        let call_permission = wire.permission().await;
        assert_eq!(
            call_permission["params"]["toolCall"]["title"], "mcp__fixture__echo",
            "MCP startup must not recur between prompts: {call_permission}"
        );
        wire.decide(&call_permission, "allow_once").await;
        let (mut socket, _) = tokio::time::timeout(TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let result = read_request(&mut socket).await;
        let context = result["messages"].to_string();
        assert!(
            context.contains("stdio-pipe-result"),
            "MCP result missing from context: {result}"
        );
        assert!(context.contains(&format!("message={message}")), "{result}");
        assert!(!context.contains(MCP_ARG_SECRET));
        assert!(!context.contains(MCP_ENV_SECRET));
        respond(
            &mut socket,
            &text_stream(&format!("Completed {message} MCP turn.")),
        )
        .await;
        requests.push(result);
        let response = wire.response(id).await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        let pid: u32 = std::fs::read_to_string(&mcp.pid).unwrap().parse().unwrap();
        assert!(
            !support::has_exited(pid),
            "per-prompt settle terminated the session MCP process"
        );
    }
    let pid: u32 = std::fs::read_to_string(&mcp.pid).unwrap().parse().unwrap();
    let events = mcp.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["method"] == "spawn")
            .count(),
        1
    );
    assert_eq!(
        events[0]["secrets_received"], true,
        "argv/env did not reach real subprocess"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["method"] == "initialize")
            .count(),
        1
    );
    assert!(events.iter().any(|event| event["method"] == "tools/list"));
    assert_eq!(
        events
            .iter()
            .filter(|event| event["method"] == "tools/call")
            .count(),
        2
    );
    assert!(events.iter().all(|event| event["pid"] == pid));
    assert_eq!(
        wire.frames
            .iter()
            .filter(|frame| frame["method"] == "session/request_permission"
                && frame["params"]["toolCall"]["title"] == "MCPServerStart")
            .count(),
        1
    );
    // Only the originating client's transient approval may contain argv, never normal updates.
    for frame in &wire.frames {
        assert!(
            !frame.to_string().contains(MCP_ENV_SECRET),
            "env leak: {frame}"
        );
        if frame["method"] != "session/request_permission" {
            assert!(
                !frame.to_string().contains(MCP_ARG_SECRET),
                "argv leak: {frame}"
            );
        }
    }
    for request in requests {
        assert!(!request.to_string().contains(MCP_ARG_SECRET));
        assert!(!request.to_string().contains(MCP_ENV_SECRET));
    }
    wire.finish().await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while !support::has_exited(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("ACP EOF left its stdio MCP process alive");
    let descendant = events[0]["descendant"].as_u64().unwrap() as u32;
    assert!(
        support::has_exited(descendant),
        "ACP EOF left an MCP descendant alive"
    );
    let transcript = fixture.transcript_jsonl();
    assert!(
        !transcript.contains(MCP_ARG_SECRET),
        "argv persisted in Transcript JSONL"
    );
    assert!(
        !transcript.contains(MCP_ENV_SECRET),
        "env persisted in Transcript JSONL"
    );
    assert!(
        transcript.contains("stdio-pipe-result"),
        "test must inspect the actual tool Transcript"
    );
    assert_no_model(&listener).await;
}

#[tokio::test]
async fn acp_stdio_mcp_lifeline_lease_and_supervisor_term_stop_owned_descendants() {
    for failure in ["worker", "client", "supervisor"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(listener.local_addr().unwrap(), json!([]));
        let mcp = StdioMcp::new(&fixture);
        let mut wire = fixture.spawn();
        wire.initialize().await;
        let session = wire.mcp_session(&fixture, mcp.descriptor()).await;
        wire.prompt(3, &session, "Start only the approved MCP service.")
            .await;
        let permission = wire.permission().await;
        mcp.assert_approval(&permission);
        wire.decide(&permission, "allow_once").await;
        let (mut socket, _) = tokio::time::timeout(TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        read_request(&mut socket).await;
        respond(&mut socket, &text_stream("Ready.")).await;
        assert_eq!(wire.response(3).await["result"]["stopReason"], "end_turn");
        let started = mcp.events().remove(0);
        let server = started["pid"].as_u64().unwrap() as u32;
        let descendant = started["descendant"].as_u64().unwrap() as u32;
        assert!(!support::has_exited(server));
        assert!(!support::has_exited(descendant));
        // These PIDs come from this test's own child, not process-name discovery.
        let (target, signal) = match failure {
            "worker" => (started["worker"].as_u64().unwrap() as i32, libc::SIGKILL),
            "supervisor" => (
                started["supervisor"].as_u64().unwrap() as i32,
                libc::SIGTERM,
            ),
            _ => (wire.child.id().unwrap() as i32, libc::SIGKILL),
        };
        assert_eq!(unsafe { libc::kill(target, signal) }, 0);
        tokio::time::timeout(Duration::from_secs(65), async {
            while !support::has_exited(server) || !support::has_exited(descendant) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{failure} left MCP processes alive"));
        if failure != "client" {
            drop(wire.input.take());
        }
        tokio::time::timeout(TIMEOUT, wire.child.wait())
            .await
            .unwrap()
            .unwrap();
        // Cleanup failure can legitimately make the ACP exit nonzero after Worker loss.
        let errors = wire.errors.await.unwrap();
        assert!(!errors.contains(MCP_ARG_SECRET));
        assert!(!errors.contains(MCP_ENV_SECRET));
        assert_no_model(&listener).await;
    }
}

#[tokio::test]
async fn acp_stdio_mcp_deny_and_cancel_never_spawn_or_call_model() {
    for decision in ["reject_once", "cancelled", "session/cancel"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(listener.local_addr().unwrap(), json!([]));
        let mcp = StdioMcp::new(&fixture);
        let mut wire = fixture.spawn();
        wire.initialize().await;
        let session = wire.mcp_session(&fixture, mcp.descriptor()).await;
        mcp.assert_not_spawned();
        assert_no_model(&listener).await;
        wire.prompt(3, &session, "Do not start this MCP until I approve.")
            .await;
        let permission = wire.permission().await;
        mcp.assert_approval(&permission);
        assert_no_model(&listener).await;
        match decision {
            "reject_once" => wire.decide(&permission, "reject_once").await,
            "cancelled" => wire.send(json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"cancelled"}}})).await,
            _ => wire.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}})).await,
        }
        let response = tokio::time::timeout(Duration::from_secs(15), wire.response(3))
            .await
            .expect("MCP cancellation blocked on approval");
        if decision == "session/cancel" {
            assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
        } else {
            assert!(
                response.get("error").is_some(),
                "denied MCP startup must fail before model: {response}"
            );
        }
        wire.finish().await;
        mcp.assert_not_spawned();
        assert_no_model(&listener).await;
        let transcript = fixture.transcript_jsonl();
        assert!(!transcript.contains(MCP_ARG_SECRET));
        assert!(!transcript.contains(MCP_ENV_SECRET));
    }
}
