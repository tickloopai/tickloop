use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

use agent_client_protocol::{
    ByteStreams, Client,
    schema::{
        ProtocolVersion,
        v1::{
            CancelNotification, InitializeRequest, NewSessionRequest, PromptRequest,
            RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
        },
    },
};
use serde_json::{Value, json};
use tickloop_code_access::LaunchSpec;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::fake::Fake;

fn decoded<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}
fn launch() -> LaunchSpec {
    LaunchSpec::embedded(std::env::current_dir().unwrap())
}

/// Official stable SDK client, not a hand-written substitute for protocol compatibility.
#[tokio::test]
async fn official_sdk_handshake_prompt_and_permission() {
    let fake = Fake::new();
    let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(agent_io);
    let backend = fake.client();
    let server = tokio::spawn(tickloop_code_acp::serve(
        backend,
        launch(),
        true,
        input,
        output,
    ));
    let (input, output) = tokio::io::split(client_io);
    let updates = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = updates.clone();
    let client = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                observed
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(notification).unwrap());
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let wire = serde_json::to_value(&request).unwrap();
                assert_eq!(wire["options"].as_array().unwrap().len(), 2);
                assert_eq!(wire["options"][0]["kind"], "allow_once");
                assert_eq!(wire["options"][1]["kind"], "reject_once");
                responder.respond(decoded::<RequestPermissionResponse>(
                    json!({"outcome":{"outcome":"selected", "optionId":"allow_once"}}),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect_with(
            ByteStreams::new(output.compat_write(), input.compat()),
            async move |cx| {
                let initialization = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(initialization.protocol_version, ProtocolVersion::V1);
                assert!(initialization.agent_capabilities.load_session);
                let capabilities =
                    serde_json::to_value(&initialization.agent_capabilities).unwrap();
                assert_eq!(capabilities["promptCapabilities"]["image"], true);
                assert_eq!(capabilities["promptCapabilities"]["embeddedContext"], true);
                assert!(
                    !capabilities["mcpCapabilities"]["http"]
                        .as_bool()
                        .unwrap_or(false)
                );
                assert!(
                    !capabilities["mcpCapabilities"]["sse"]
                        .as_bool()
                        .unwrap_or(false)
                );
                assert!(initialization.auth_methods.is_empty());
                assert!(capabilities["sessionCapabilities"]["close"].is_object());
                let session = cx
                    .send_request(decoded::<NewSessionRequest>(json!({
                        "cwd":std::env::current_dir().unwrap(), "mcpServers":[]
                    })))
                    .block_task()
                    .await?;
                for prompt in ["hello", "permission"] {
                    let response = cx.send_request(decoded::<PromptRequest>(json!({
                "sessionId":session.session_id, "prompt":[{"type":"text", "text":prompt}]
            }))).block_task().await?;
                    assert_eq!(
                        serde_json::to_value(response).unwrap()["stopReason"],
                        "end_turn"
                    );
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 2);
    assert_eq!(fake.releases.load(Ordering::SeqCst), 1);
    let decisions = fake.decisions.lock().unwrap();
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].1);
    assert_eq!(decisions[0].0.version, 7);
    let text: String = updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|update| update["update"]["content"]["text"].as_str())
        .collect();
    assert_eq!(text, "helloallowed");
}

struct Peer {
    input: BufReader<ReadHalf<DuplexStream>>,
    output: WriteHalf<DuplexStream>,
}
impl Peer {
    async fn send(&mut self, value: Value) {
        self.output
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        self.output.flush().await.unwrap();
    }
    async fn next(&mut self) -> Value {
        let mut line = String::new();
        let bytes = tokio::time::timeout(Duration::from_secs(5), self.input.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(bytes, 0, "unexpected ACP EOF");
        serde_json::from_str(&line).unwrap()
    }
    async fn request(&mut self, id: usize, method: &str, params: Value) -> (Value, Vec<Value>) {
        self.send(json!({"jsonrpc":"2.0", "id":id,"method":method,"params":params}))
            .await;
        self.response(id).await
    }
    async fn response(&mut self, id: usize) -> (Value, Vec<Value>) {
        let mut updates = Vec::new();
        loop {
            let value = self.next().await;
            if value["id"] == id && value.get("method").is_none() {
                return (value, updates);
            }
            updates.push(value);
        }
    }
    async fn new_session(&mut self, id: usize) -> String {
        let (value, _) = self
            .request(
                id,
                "session/new",
                json!({"cwd":std::env::current_dir().unwrap(),"mcpServers":[]}),
            )
            .await;
        let session = value["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("new session: {value}"))
            .to_owned();
        // Editors register the session from the response, so its commands must follow it.
        let commands = self.next().await;
        assert_eq!(commands["params"]["sessionId"], session.as_str());
        assert_eq!(
            commands["params"]["update"]["sessionUpdate"],
            "available_commands_update"
        );
        session
    }
    async fn prompt(&mut self, id: usize, session: &str, text: &str) -> (Value, Vec<Value>) {
        self.request(
            id,
            "session/prompt",
            json!({"sessionId":session, "prompt":[{"type":"text","text":text}]}),
        )
        .await
    }
}
async fn setup() -> (Arc<Fake>, Peer, tokio::task::JoinHandle<Result<(), String>>) {
    let fake = Fake::new();
    let (agent, client) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(agent);
    let server = tokio::spawn(tickloop_code_acp::serve(
        fake.client(),
        launch(),
        true,
        input,
        output,
    ));
    let (input, output) = tokio::io::split(client);
    let mut peer = Peer {
        input: BufReader::new(input),
        output,
    };
    let (initialize, _) = peer
        .request(
            1,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
    assert_eq!(initialize["result"]["protocolVersion"], 1);
    (fake, peer, server)
}
async fn finish(peer: Peer, server: tokio::task::JoinHandle<Result<(), String>>) {
    drop(peer);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
fn text(updates: &[Value]) -> String {
    updates
        .iter()
        .map(|update| &update["params"]["update"])
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|update| update["content"]["text"].as_str())
        .collect()
}

#[tokio::test]
async fn fast_completion_reset_dedup_and_multiple_assistant_messages() {
    let (fake, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    for (id, prompt, expected) in [
        (3, "hello", "hello"),
        (4, "reset", "recovered"),
        (5, "duplicate", "once"),
        (6, "multi", "workingdone"),
        (7, "chunked", "Hello world"),
    ] {
        let (result, updates) = peer.prompt(id, &session, prompt).await;
        assert_eq!(result["result"]["stopReason"], "end_turn", "{result}");
        assert_eq!(text(&updates), expected);
    }
    finish(peer, server).await;
    assert_eq!(fake.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn policy_denial_announces_pending_tool_before_terminal_update_without_execution() {
    let (_, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    let (response, updates) = peer.prompt(3, &session, "policy-denied").await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let calls: Vec<_> = updates
        .iter()
        .filter_map(|value| {
            let update = &value["params"]["update"];
            (update["toolCallId"] == "denied-tool").then_some(update)
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["sessionUpdate"], "tool_call");
    assert_eq!(calls[0]["title"], "Write");
    // The official schema omits the default pending status on initial tool calls.
    assert_eq!(calls[0]["status"].as_str().unwrap_or("pending"), "pending");
    assert_eq!(calls[1]["sessionUpdate"], "tool_call_update");
    assert_eq!(calls[1]["status"], "failed");
    finish(peer, server).await;
}

#[tokio::test]
async fn cancel_is_concurrent_with_prompt_and_other_sessions_are_independent() {
    let (fake, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    let other = peer.new_session(3).await;
    peer.send(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"wait"}]}})).await;
    let partial = peer.next().await;
    assert_eq!(partial["params"]["update"]["content"]["text"], "partial");
    let (parallel, _) = peer.prompt(5, &session, "hello").await;
    assert_eq!(parallel["error"]["code"], -32602);
    let (independent, _) = peer.prompt(6, &other, "hello").await;
    assert_eq!(independent["result"]["stopReason"], "end_turn");
    peer.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    let (cancelled, _) = peer.response(4).await;
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    let (again, _) = peer.prompt(7, &session, "hello").await;
    assert_eq!(again["result"]["stopReason"], "end_turn");
    finish(peer, server).await;
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 3);
    assert_eq!(fake.releases.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn permission_reject_and_cancel_never_grant_persistent_access() {
    for selected in ["reject_once", "cancelled", "allow_always"] {
        let (fake, mut peer, server) = setup().await;
        let session = peer.new_session(2).await;
        peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"permission"}]}})).await;
        let pending = peer.next().await;
        assert_eq!(pending["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(
            pending["params"]["update"]["status"]
                .as_str()
                .unwrap_or("pending"),
            "pending"
        );
        let request = peer.next().await;
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(
            request["params"]["toolCall"]["toolCallId"],
            pending["params"]["update"]["toolCallId"]
        );
        let outcome = if selected == "cancelled" {
            json!({"outcome":"cancelled"})
        } else {
            json!({"outcome":"selected","optionId":selected})
        };
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"outcome":outcome}}))
            .await;
        let (result, _) = peer.response(3).await;
        if selected == "allow_always" {
            assert!(result.get("error").is_some(), "{result}");
            assert!(fake.decisions.lock().unwrap().is_empty());
        } else {
            assert_eq!(result["result"]["stopReason"], "end_turn");
            assert!(!fake.decisions.lock().unwrap()[0].1);
        }
        finish(peer, server).await;
    }
}

#[tokio::test]
async fn permission_wait_can_be_cancelled_and_eof_releases_owned_session() {
    let (fake, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"permission"}]}})).await;
    let pending = peer.next().await;
    assert_eq!(pending["params"]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(
        pending["params"]["update"]["status"]
            .as_str()
            .unwrap_or("pending"),
        "pending"
    );
    assert_eq!(peer.next().await["method"], "session/request_permission");
    peer.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    let (response, updates) = peer.response(3).await;
    assert_eq!(response["result"]["stopReason"], "cancelled");
    assert!(
        updates
            .iter()
            .any(
                |value| value["params"]["update"]["sessionUpdate"] == "tool_call_update"
                    && value["params"]["update"]["status"] == "failed"
            )
    );
    assert!(fake.decisions.lock().unwrap().is_empty());
    peer.send(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"wait"}]}})).await;
    assert_eq!(
        peer.next().await["params"]["update"]["content"]["text"],
        "partial"
    );
    finish(peer, server).await;
    assert_eq!(fake.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unsupported_http_mcp_bad_image_live_load_unknown_session_and_auth_are_explicit() {
    let (fake, mut peer, server) = setup().await;
    let (mcp,_) = peer.request(2,"session/new",json!({"cwd":std::env::current_dir().unwrap(),"mcpServers":[{"type":"http","name":"server","url":"https://example.invalid/mcp","headers":[]}]})).await;
    assert_eq!(mcp["error"]["code"], -32602);
    assert_eq!(fake.launches.lock().unwrap().len(), 0);
    let session = peer.new_session(3).await;
    let (image,_) = peer.request(4,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"image","data":"aGVsbG8=","mimeType":"image/png"}]})).await;
    assert_eq!(image["error"]["code"], -32602);
    let (load, _) = peer
        .request(
            5,
            "session/load",
            json!({"sessionId":session,"cwd":std::env::current_dir().unwrap(),"mcpServers":[]}),
        )
        .await;
    assert_eq!(load["error"]["code"], -32602);
    let (missing, _) = peer.prompt(6, "unknown", "hello").await;
    assert_eq!(missing["error"]["code"], -32602);
    let (auth, _) = peer.prompt(7, &session, "auth").await;
    assert_eq!(auth["error"]["code"], -32000);
    assert!(
        auth["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/provider")
    );
    finish(peer, server).await;
}

#[tokio::test]
async fn unknown_outcomes_waiting_and_missing_terminal_never_report_success() {
    for mode in ["unknown", "waiting", "missing-terminal", "failed", "large"] {
        let (_, mut peer, server) = setup().await;
        let session = peer.new_session(2).await;
        let (response, _) = peer.prompt(3, &session, mode).await;
        assert!(response.get("error").is_some(), "{mode}: {response}");
        finish(peer, server).await;
    }
}

#[tokio::test]
async fn explicit_shutdown_releases_with_stdin_open_and_unread_output() {
    let fake = Fake::new();
    let (agent, client) = tokio::io::duplex(256);
    let (input, output) = tokio::io::split(agent);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(tickloop_code_acp::serve_until(
        fake.client(),
        launch(),
        true,
        input,
        output,
        shutdown.clone().cancelled_owned(),
    ));
    let (input, output) = tokio::io::split(client);
    let mut peer = Peer {
        input: BufReader::new(input),
        output,
    };
    peer.request(
        1,
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    )
    .await;
    let session = peer.new_session(2).await;
    peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"flood"}]}})).await;
    // Leave stdout unread: 2 MiB of updates cannot fit in this 256-byte pipe.
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.submissions.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !server.is_finished(),
        "unread output should apply backpressure"
    );
    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(result.unwrap_err().contains("signal"));
    assert_eq!(fake.releases.load(Ordering::SeqCst), 1);
    drop(peer);
}

#[tokio::test]
async fn frame_and_session_limits_are_enforced_and_cleanup_owned_work() {
    let (fake, mut peer, server) = setup().await;
    for id in 2..18 {
        peer.new_session(id).await;
    }
    let (overflow, _) = peer
        .request(
            18,
            "session/new",
            json!({"cwd":std::env::current_dir().unwrap(),"mcpServers":[]}),
        )
        .await;
    assert_eq!(overflow["error"]["code"], -32602);
    let _ = peer
        .output
        .write_all(&vec![b'x'; 24 * 1024 * 1024 + 1])
        .await;
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert_eq!(fake.launches.lock().unwrap().len(), 16);
    assert_eq!(fake.releases.load(Ordering::SeqCst), 16);
}

#[tokio::test]
async fn explicit_startup_trust_does_not_trust_other_client_directories() {
    let (fake, mut peer, server) = setup().await;
    peer.new_session(2).await;
    let other = std::env::current_dir()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    peer.request(3, "session/new", json!({"cwd":other,"mcpServers":[]}))
        .await;
    let launches = fake.launches.lock().unwrap().clone();
    assert_eq!(
        launches[0].host_options.directory_trust,
        tickloop_harness::protocol::DirectoryTrustPolicy::CallerManaged
    );
    assert_eq!(
        launches[1].host_options.directory_trust,
        tickloop_harness::protocol::DirectoryTrustPolicy::RequireUserTrust
    );
    finish(peer, server).await;
}

#[tokio::test]
async fn initialization_is_required_and_stable_version_is_negotiated() {
    let fake = Fake::new();
    let (agent, client) = tokio::io::duplex(4096);
    let (input, output) = tokio::io::split(agent);
    let server = tokio::spawn(tickloop_code_acp::serve(
        fake.client(),
        launch(),
        false,
        input,
        output,
    ));
    let (input, output) = tokio::io::split(client);
    let mut peer = Peer {
        input: BufReader::new(input),
        output,
    };
    let (early, _) = peer
        .request(
            1,
            "session/new",
            json!({"cwd":std::env::current_dir().unwrap(),"mcpServers":[]}),
        )
        .await;
    assert_eq!(early["error"]["code"], -32602);
    let (future, _) = peer
        .request(
            2,
            "initialize",
            json!({"protocolVersion":999,"capabilities":{},"info":{"name":"future-client","version":"1"}}),
        )
        .await;
    assert_eq!(future["result"]["protocolVersion"], 1, "{future}");
    let (duplicate, _) = peer
        .request(
            3,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
    assert_eq!(duplicate["error"]["code"], -32602);
    finish(peer, server).await;
}

#[tokio::test]
async fn cancellation_notification_uses_stable_sdk_type() {
    let cancel: CancelNotification = decoded(json!({"sessionId":"session"}));
    assert_eq!(cancel.session_id.to_string(), "session");
}

#[tokio::test]
async fn stdio_mcp_descriptor_is_lazy_and_private_except_transient_argv_approval() {
    let (fake, mut peer, server) = setup().await;
    let cwd = std::env::current_dir().unwrap();
    let (created, initial) = peer.request(2, "session/new", json!({
        "cwd":cwd,"mcpServers":[{"name":"private-fixture","command":"/fixture/private-command",
        "args":["PRIVATE_ARG_SENTINEL"],"env":[{"name":"PRIVATE_ENV","value":"PRIVATE_ENV_SECRET"}]}]
    })).await;
    let id = created["result"]["sessionId"].as_str().unwrap().to_owned();
    assert_eq!(uuid::Uuid::parse_str(&id).unwrap(), fake.chat_ids()[0]);
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 0);
    let launches = fake.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 1);
    assert_eq!(
        launches[0].session_mcp_servers[0].args,
        ["PRIVATE_ARG_SENTINEL"]
    );
    assert_eq!(
        launches[0].session_mcp_servers[0].env["PRIVATE_ENV"],
        "PRIVATE_ENV_SECRET"
    );
    for wire in initial.iter().chain(std::iter::once(&created)) {
        assert!(!wire.to_string().contains("PRIVATE_ARG_SENTINEL"));
        assert!(!wire.to_string().contains("PRIVATE_ENV_SECRET"));
    }
    peer.send(
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{
            "sessionId":id,"prompt":[{"type":"text","text":"mcp-permission"}]
        }}),
    )
    .await;
    let mut ordinary = Vec::new();
    let approval = loop {
        let wire = peer.next().await;
        if wire["method"] == "session/request_permission" {
            break wire;
        }
        ordinary.push(wire);
    };
    assert!(approval.to_string().contains("PRIVATE_ARG_SENTINEL"));
    assert!(!approval.to_string().contains("PRIVATE_ENV_SECRET"));
    peer.send(json!({"jsonrpc":"2.0","id":approval["id"],"result":{"outcome":{"outcome":"selected","optionId":"reject_once"}}})).await;
    let (result, updates) = peer.response(3).await;
    ordinary.extend(updates);
    ordinary.push(result.clone());
    assert_eq!(result["result"]["stopReason"], "end_turn", "{result}");
    for wire in ordinary {
        assert!(!wire.to_string().contains("PRIVATE_ARG_SENTINEL"));
        assert!(!wire.to_string().contains("PRIVATE_ENV_SECRET"));
    }
    finish(peer, server).await;
}

#[tokio::test]
async fn rich_context_is_preserved_without_path_reads_and_binary_is_rejected() {
    use tickloop_code_access::UserInput;
    use tickloop_harness::protocol::{ImageOrigin, InputContext};
    // Generated as a 1x1 RGBA PNG with valid IHDR/IDAT/IEND CRCs.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    let (fake, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    let (response, _) = peer.request(3, "session/prompt", json!({"sessionId":session,"prompt":[
        {"type":"text","text":"review"},
        {"type":"resource_link","uri":"file:///missing/do-not-read","name":"reference","mimeType":"text/plain"},
        {"type":"resource","resource":{"uri":"client://context","mimeType":"text/plain","text":"untrusted client material"}},
        {"type":"image","data":PNG,"mimeType":"image/png","uri":"client://pixel"}
    ]})).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    {
        let inputs = fake.inputs.lock().unwrap();
        let UserInput::Text {
            text,
            contexts,
            paths,
        } = &inputs[0]
        else {
            panic!("text input")
        };
        assert_eq!(text, "review");
        assert!(paths.is_empty());
        assert_eq!(contexts.len(), 3);
        assert!(contexts.iter().all(|context| context.path().is_none()));
        let InputContext::Resource(link) = &contexts[0] else {
            panic!("resource link")
        };
        assert_eq!(link.uri, "file:///missing/do-not-read");
        assert!(link.content.is_none());
        let InputContext::Resource(resource) = &contexts[1] else {
            panic!("embedded resource")
        };
        assert_eq!(
            resource.content.as_deref(),
            Some("untrusted client material")
        );
        let InputContext::Image(image) = &contexts[2] else {
            panic!("image")
        };
        assert!(
            matches!(&image.origin, ImageOrigin::Attached { id: 1, uri: Some(uri) } if uri == "client://pixel")
        );
        assert!(!image.image.data.is_empty());
    }
    let (pure, _) = peer
        .request(
            4,
            "session/prompt",
            json!({"sessionId":session,"prompt":[
                {"type":"resource","resource":{"uri":"client://only","text":"attachment only"}}
            ]}),
        )
        .await;
    assert_eq!(pure["result"]["stopReason"], "end_turn", "{pure}");
    let (binary, _) = peer.request(5,"session/prompt",json!({"sessionId":session,"prompt":[
        {"type":"resource","resource":{"uri":"client://binary","blob":"aGVsbG8=","mimeType":"application/octet-stream"}}
    ]})).await;
    assert_eq!(binary["error"]["code"], -32602);
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 2);
    finish(peer, server).await;
}

#[tokio::test]
async fn config_select_roundtrip_notifications_and_busy_rejection() {
    use agent_client_protocol::schema::v1::{
        NewSessionResponse, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    };
    let (fake, mut peer, server) = setup().await;
    let (created, _) = peer
        .request(
            2,
            "session/new",
            json!({"cwd":std::env::current_dir().unwrap(),"mcpServers":[]}),
        )
        .await;
    let _: NewSessionResponse = decoded(created["result"].clone());
    let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
    assert!(
        created["result"]["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|option| option["type"] == "select")
    );
    let request: SetSessionConfigOptionRequest = decoded(
        json!({"sessionId":session,"configId":"model","type":"select","value":"fixture/second"}),
    );
    let (updated, notifications) = peer
        .request(
            3,
            "session/set_config_option",
            serde_json::to_value(request).unwrap(),
        )
        .await;
    let _: SetSessionConfigOptionResponse = decoded(updated["result"].clone());
    assert_eq!(
        updated["result"]["configOptions"][0]["currentValue"],
        "fixture/second"
    );
    assert!(
        notifications
            .iter()
            .any(|wire| wire["params"]["update"]["sessionUpdate"] == "config_option_update")
    );
    let (mode, notifications) = peer
        .request(
            4,
            "session/set_mode",
            json!({"sessionId":session,"modeId":"ask"}),
        )
        .await;
    assert_eq!(mode["result"], json!({}));
    assert!(
        notifications
            .iter()
            .any(|wire| wire["params"]["update"]["currentModeId"] == "ask")
    );
    // A mode chosen through the config option keeps mode-based clients in step too.
    let (mode, notifications) = peer
        .request(
            9,
            "session/set_config_option",
            json!({"sessionId":session,"configId":"mode","type":"select","value":"auto"}),
        )
        .await;
    assert_eq!(mode["result"]["configOptions"][1]["currentValue"], "auto");
    assert!(notifications.iter().any(|wire| {
        wire["params"]["update"]["sessionUpdate"] == "current_mode_update"
            && wire["params"]["update"]["currentModeId"] == "auto"
    }));
    let (invalid, _) = peer
        .request(
            5,
            "session/set_config_option",
            json!({"sessionId":session,"configId":"mode","type":"select","value":"plan"}),
        )
        .await;
    assert_eq!(invalid["error"]["code"], -32602);
    peer.send(json!({"jsonrpc":"2.0","id":6,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"wait"}]}})).await;
    assert_eq!(
        peer.next().await["params"]["update"]["content"]["text"],
        "partial"
    );
    let (busy, _) = peer
        .request(
            7,
            "session/set_config_option",
            json!({"sessionId":session,"configId":"model","type":"select","value":"fixture/model"}),
        )
        .await;
    assert_eq!(busy["error"]["code"], -32602);
    let (busy_mode, _) = peer
        .request(
            8,
            "session/set_mode",
            json!({"sessionId":session,"modeId":"auto"}),
        )
        .await;
    assert_eq!(busy_mode["error"]["code"], -32602);
    peer.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    assert_eq!(
        peer.response(6).await.0["result"]["stopReason"],
        "cancelled"
    );
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 1);
    finish(peer, server).await;
}

fn history_node(kind: &str, blocks: Value) -> Value {
    let node = json!({"id":tickloop_harness::ids::generate(),"parent":null,
        "kind":if kind == "prompt" { json!({"kind":"prompt","input_id":null}) } else { json!({"kind":"answer","input_id":null,"attempt":1}) },
        "blocks":blocks,"status":{"kind":"complete"},"timestamp":"2026-10-22T00:00:00Z"});
    let _: tickloop_harness::transcript::display::DisplayNode = decoded(node.clone());
    node
}

#[tokio::test]
async fn load_replays_all_public_pages_without_submission_and_rejects_live_ownership() {
    use agent_client_protocol::schema::v1::LoadSessionResponse;
    let (fake, mut peer, server) = setup().await;
    let cwd = std::env::current_dir().unwrap();
    let long_answer = "history-中文".repeat(2500);
    let id = fake.record(cwd.clone(), vec![
        vec![history_node("prompt", json!([{"kind":"text","text":"public question"}]))],
        vec![history_node("answer", json!([
            {"kind":"text","text":long_answer},
            {"kind":"toolCall","call_id":"recorded-read","name":"Read","input":{"file_path":"/fixture/history.rs"},"result":{"text":"recorded output","is_error":false,"truncated":true}}
        ]))],
        vec![history_node("answer", json!([
            {"kind":"text","text":"last history page"},
            {"kind":"toolCall","call_id":"incomplete-write","name":"Write","input":{"file_path":"/fixture/pending.rs"},"result":null}
        ]))],
    ]);
    let params = json!({"sessionId":id,"cwd":cwd,"mcpServers":[]});
    let (loaded, updates) = peer.request(2, "session/load", params.clone()).await;
    let _: LoadSessionResponse = decoded(loaded["result"].clone());
    assert_eq!(fake.history_requests.load(Ordering::SeqCst), 3);
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(fake.chat_ids(), vec![id]);
    let chunks: String = updates
        .iter()
        .filter(|wire| wire["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|wire| wire["params"]["update"]["content"]["text"].as_str())
        .collect();
    assert_eq!(chunks, format!("{long_answer}last history page"));
    assert!(
        updates
            .iter()
            .all(|wire| wire["params"]["sessionId"] == id.to_string())
    );
    let wire = serde_json::to_string(&updates).unwrap();
    assert!(wire.contains("public question"));
    assert!(wire.contains("Recorded display result is truncated"));
    assert!(wire.contains("No terminal result was recorded"));
    assert!(!wire.contains("MODEL_ONLY_SYSTEM_SECRET"));
    let (duplicate, _) = peer.request(3, "session/load", params.clone()).await;
    assert_eq!(duplicate["error"]["code"], -32602);
    // A distinct ACP connection must not take over a still-live managed execution either.
    let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(agent_io);
    let other_server = tokio::spawn(tickloop_code_acp::serve(
        fake.client(),
        launch(),
        true,
        input,
        output,
    ));
    let (input, output) = tokio::io::split(client_io);
    let mut other = Peer {
        input: BufReader::new(input),
        output,
    };
    other
        .request(
            1,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
    let (busy, _) = other.request(2, "session/load", params.clone()).await;
    assert_eq!(busy["error"]["code"], -32602);
    finish(peer, server).await;
    let (restored, _) = other.request(3, "session/load", params).await;
    assert!(restored.get("error").is_none(), "{restored}");
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 0);
    let (continued, _) = other.prompt(4, &id.to_string(), "hello").await;
    assert_eq!(continued["result"]["stopReason"], "end_turn", "{continued}");
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 1);
    finish(other, other_server).await;
}

#[tokio::test]
async fn session_list_preserves_pagination_and_cwd_scope_without_creation() {
    let (fake, mut peer, server) = setup().await;
    let cwd = std::env::current_dir().unwrap();
    let first = fake.record(cwd.clone(), vec![]);
    let second = fake.record(cwd.clone(), vec![]);
    let excluded = fake.record(cwd.parent().unwrap().to_owned(), vec![]);
    let (page, _) = peer.request(2, "session/list", json!({"cwd":cwd})).await;
    assert_eq!(page["result"]["sessions"].as_array().unwrap().len(), 1);
    let (next, _) = peer
        .request(
            3,
            "session/list",
            json!({"cwd":cwd,"cursor":page["result"]["nextCursor"]}),
        )
        .await;
    assert!(next["result"]["nextCursor"].is_null());
    let mut listed = vec![
        page["result"]["sessions"][0]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned(),
        next["result"]["sessions"][0]["sessionId"]
            .as_str()
            .unwrap()
            .to_owned(),
    ];
    listed.sort();
    let mut expected = vec![first.to_string(), second.to_string()];
    expected.sort();
    assert_eq!(listed, expected);
    assert!(!listed.contains(&excluded.to_string()));
    assert!(fake.launches.lock().unwrap().is_empty());
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 0);
    finish(peer, server).await;
}

#[tokio::test]
async fn announced_tool_kinds_and_only_absolute_locations_match_official_schema() {
    let (_, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    let (response, updates) = peer.prompt(3, &session, "tool-kinds").await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    for (index, kind, path) in [
        (0, "read", Some("/fixture/read.rs")),
        (1, "edit", Some("/fixture/write.rs")),
        (2, "search", Some("/fixture/src")),
        (3, "execute", Some("/fixture")),
        (4, "fetch", None),
        (5, "read", None),
    ] {
        let wire = updates
            .iter()
            .find(|wire| {
                wire["params"]["update"]["sessionUpdate"] == "tool_call"
                    && wire["params"]["update"]["toolCallId"] == format!("kind-{index}")
            })
            .unwrap();
        let _: SessionNotification = decoded(wire["params"].clone());
        assert_eq!(wire["params"]["update"]["kind"], kind);
        let locations = wire["params"]["update"]["locations"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(path) = path {
            assert_eq!(locations, vec![json!({"path":path})]);
        } else {
            assert!(locations.is_empty());
        }
    }
    finish(peer, server).await;
}

#[tokio::test]
async fn thinking_title_and_context_usage_reach_the_editor() {
    let (_, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    let (response, updates) = peer.prompt(3, &session, "status").await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    for wire in &updates {
        let _: SessionNotification = decoded(wire["params"].clone());
    }
    let of = |updates: &[Value], kind: &str| {
        updates
            .iter()
            .map(|wire| wire["params"]["update"].clone())
            .filter(|update| update["sessionUpdate"] == kind)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        of(&updates, "agent_thought_chunk")[0]["content"]["text"],
        "weighing options"
    );
    assert_eq!(
        of(&updates, "session_info_update"),
        vec![json!({"sessionUpdate":"session_info_update", "title":"Fixture title"})]
    );
    let usage = of(&updates, "usage_update");
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["size"], 100_000);
    assert!(usage[0]["used"].as_u64().unwrap() > 0);
    // An unchanged title is not sent again.
    let (_, updates) = peer.prompt(4, &session, "again").await;
    assert!(of(&updates, "session_info_update").is_empty());
    finish(peer, server).await;
}

#[tokio::test]
async fn excess_thinking_is_cut_off_without_failing_the_answer() {
    let (_, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    let (response, updates) = peer.prompt(3, &session, "thought-flood").await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let thoughts: Vec<_> = updates
        .iter()
        .filter(|wire| wire["params"]["update"]["sessionUpdate"] == "agent_thought_chunk")
        .map(|wire| {
            wire["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(thoughts.len(), 65);
    assert_eq!(thoughts.last().unwrap(), "\n[Further thinking omitted]");
    assert_eq!(text(&updates), "answered");
    finish(peer, server).await;
}

#[tokio::test]
async fn cancel_request_ends_its_prompt_and_unknown_notifications_are_ignored() {
    let (fake, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    for method in ["_vendor/ping", "$/progress", "session/unknown"] {
        peer.send(json!({"jsonrpc":"2.0","method":method,"params":{}}))
            .await;
    }
    // The official SDK sends this when a caller drops a pending request.
    peer.send(json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":99}}))
        .await;
    peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"wait"}]}})).await;
    assert_eq!(
        peer.next().await["params"]["update"]["content"]["text"],
        "partial"
    );
    peer.send(json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":3}}))
        .await;
    let (cancelled, _) = peer.response(3).await;
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );
    let (again, _) = peer.prompt(4, &session, "hello").await;
    assert_eq!(again["result"]["stopReason"], "end_turn");
    finish(peer, server).await;
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_run_after_a_committed_message_keeps_its_error_and_closes_tools() {
    let (_, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    let (response, updates) = peer.prompt(3, &session, "fail-after-commit").await;
    // Provider authentication keeps its own code instead of a stream reconciliation error.
    assert_eq!(response["error"]["code"], -32000, "{response}");
    let closed: Vec<_> = updates
        .iter()
        .map(|wire| &wire["params"]["update"])
        .filter(|update| update["toolCallId"] == "never-run")
        .collect();
    assert_eq!(closed.len(), 2, "{closed:?}");
    assert_eq!(closed[1]["sessionUpdate"], "tool_call_update");
    assert_eq!(closed[1]["status"], "failed");
    finish(peer, server).await;
}

#[tokio::test]
async fn load_accepts_only_the_canonical_session_id_and_never_strands_a_lease() {
    let (fake, mut peer, server) = setup().await;
    let cwd = std::env::current_dir().unwrap();
    let id = fake.record(
        cwd.clone(),
        vec![vec![history_node(
            "prompt",
            json!([{"kind":"text","text":"question"}]),
        )]],
    );
    let aliases = [
        id.to_string().to_uppercase(),
        id.simple().to_string(),
        id.braced().to_string(),
        id.urn().to_string(),
    ];
    for (request, alias) in (2..).zip(aliases) {
        let (rejected, _) = peer
            .request(
                request,
                "session/load",
                json!({"sessionId":alias,"cwd":cwd,"mcpServers":[]}),
            )
            .await;
        assert_eq!(rejected["error"]["code"], -32602, "{alias}: {rejected}");
    }
    assert!(fake.chat_ids().is_empty());
    let (loaded, _) = peer
        .request(
            6,
            "session/load",
            json!({"sessionId":id,"cwd":cwd,"mcpServers":[]}),
        )
        .await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let (continued, _) = peer.prompt(7, &id.to_string(), "hello").await;
    assert_eq!(continued["result"]["stopReason"], "end_turn", "{continued}");
    finish(peer, server).await;
    assert_eq!(fake.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn close_cancels_the_turn_releases_the_lease_and_frees_the_slot() {
    let (fake, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"wait"}]}})).await;
    assert_eq!(
        peer.next().await["params"]["update"]["content"]["text"],
        "partial"
    );
    peer.send(
        json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":session}}),
    )
    .await;
    let (mut prompt, mut close) = (None, None);
    while prompt.is_none() || close.is_none() {
        let wire = peer.next().await;
        match wire["id"].as_u64() {
            Some(3) if wire.get("method").is_none() => prompt = Some(wire),
            Some(4) if wire.get("method").is_none() => close = Some(wire),
            _ => {}
        }
    }
    let (prompt, close) = (prompt.unwrap(), close.unwrap());
    assert_eq!(prompt["result"]["stopReason"], "cancelled", "{prompt}");
    assert_eq!(close["result"], json!({}), "{close}");
    assert_eq!(fake.releases.load(Ordering::SeqCst), 1);
    let (gone, _) = peer.prompt(5, &session, "hello").await;
    assert_eq!(gone["error"]["code"], -32602);
    let (again, _) = peer
        .request(6, "session/close", json!({"sessionId":session}))
        .await;
    assert_eq!(again["error"]["code"], -32602);
    // The closed session's slot is free: a full set of sessions fits again.
    for id in 7..23 {
        peer.new_session(id).await;
    }
    let (overflow, _) = peer
        .request(
            23,
            "session/new",
            json!({"cwd":std::env::current_dir().unwrap(),"mcpServers":[]}),
        )
        .await;
    assert_eq!(overflow["error"]["code"], -32602);
    finish(peer, server).await;
    assert_eq!(fake.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(fake.releases.load(Ordering::SeqCst), 17);
}

#[tokio::test]
async fn supplementary_tool_content_degrades_instead_of_failing_the_turn() {
    let (_, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    let (response, updates) = peer.prompt(3, &session, "tool-flood").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(text(&updates), "survived");
    let call = |id: &str, kind: &str| {
        updates
            .iter()
            .map(|wire| &wire["params"]["update"])
            .find(|update| update["toolCallId"] == id && update["sessionUpdate"] == kind)
            .unwrap()
            .clone()
    };
    let first = call("flood-0", "tool_call");
    assert_eq!(
        first["rawInput"]["command"].as_str().unwrap().len(),
        31 * 1024
    );
    let last = call("flood-39", "tool_call");
    assert_eq!(last["rawInput"]["truncated"], true, "{last}");
    let result = call("flood-39", "tool_call_update");
    assert_eq!(result["status"], "completed");
    let output = result["content"][0]["content"]["text"].as_str().unwrap();
    assert!(output.ends_with("[output truncated]"), "{output}");
    assert!(output.len() < 1024);
    let sent: usize = updates.iter().map(|wire| wire.to_string().len()).sum();
    assert!(sent < 2 * 1024 * 1024 + 128 * 1024, "{sent}");
    finish(peer, server).await;
}

#[tokio::test(start_paused = true)]
async fn lost_lease_ends_the_turn_and_leaves_cleanup_to_the_daemon() {
    let (fake, mut peer, server) = setup().await;
    let session = peer.new_session(2).await;
    fake.lease_lost.store(true, Ordering::SeqCst);
    // Start between feed recovery ticks: the first renewal (10s) is rejected before the next.
    tokio::time::sleep(Duration::from_secs(7)).await;
    let started = tokio::time::Instant::now();
    peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"wait"}]}})).await;
    assert_eq!(
        peer.next().await["params"]["update"]["content"]["text"],
        "partial"
    );
    let response = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut line = String::new();
            peer.input.read_line(&mut line).await.unwrap();
            let wire: Value = serde_json::from_str(&line).unwrap();
            if wire["id"] == 3 && wire.get("method").is_none() {
                return wire;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("fake lease rejected"),
        "{response}"
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    // The daemon's lease expiry owns cleanup: EOF does not wait on a release it cannot answer.
    finish(peer, server).await;
}
