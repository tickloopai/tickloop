//! End-to-end check of the host's Artifact Service integration against a real `tickloop-docs`
//! deployment (the local realapp harness). Ignored by default; run with
//!
//! ```sh
//! TICKLOOP_REALAPP_PROXY=http://127.0.0.1:18180 TICKLOOP_REALAPP_CA=<realapp>/tls/ca.pem \
//!   cargo test --locked -p tickloop-agent-runtime --test all artifact_real_service -- --ignored
//! ```
//!
//! The host only trusts public roots, so a loopback relay forwards its plain HTTP to the
//! service's HTTPS origin and maps that origin back to the relay's in every response. People
//! (sign-in approval, comments, invitations) act through the web API with real session cookies.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent_artifacts::{
    AgentLink, AgentLinkRequest, ArtifactClient, ArtifactError, ReleaseOutcome,
};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    ArtifactTick, ChatId, Content, ExecutionMode, LoopState, ModelIdentity, ToolResultContent,
};
use tickloop_agent_runtime::{Agent, host_tools::HostToolProvider, mcp_tools::McpHostToolProvider};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_agent_test_support::{ScriptedModel, call, config, text};

const DEFAULT_SERVICE_ORIGIN: &str = "https://docs.example.test";
const SESSION_COOKIE: &str = "__Host-artifact_session";
const REQUEST_MARKER: &str = "Request input JSON:\n";
const WAIT: Duration = Duration::from_secs(60);
const HOP_BY_HOP_HEADERS: [&str; 5] = [
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "accept-encoding",
];

struct RealService {
    origin: String,
    http: reqwest::Client,
}

impl RealService {
    fn from_env() -> Self {
        let proxy = std::env::var("TICKLOOP_REALAPP_PROXY").expect("TICKLOOP_REALAPP_PROXY");
        let ca = std::fs::read(std::env::var("TICKLOOP_REALAPP_CA").expect("TICKLOOP_REALAPP_CA"))
            .unwrap();
        let origin = std::env::var("TICKLOOP_REALAPP_ORIGIN")
            .unwrap_or_else(|_| DEFAULT_SERVICE_ORIGIN.into());
        let http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(proxy).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self { origin, http }
    }

    /// A loopback HTTP origin whose requests reach the service unchanged and whose responses
    /// name the relay wherever the service names its own origin.
    async fn start_relay(self: &Arc<Self>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_origin = format!("http://{}", listener.local_addr().unwrap());
        let service = self.clone();
        let local = relay_origin.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(service.clone().relay(stream, local.clone()));
            }
        });
        relay_origin
    }

    async fn relay(self: Arc<Self>, mut stream: tokio::net::TcpStream, relay_origin: String) {
        let mut bytes = Vec::new();
        let mut buffer = [0; 16 * 1024];
        let (header_end, length) = loop {
            let Ok(count) = stream.read(&mut buffer).await else {
                return;
            };
            if count == 0 {
                return;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let length = String::from_utf8_lossy(&bytes[..end])
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    break (end, length);
                }
            }
        };
        let head = String::from_utf8_lossy(&bytes[..header_end]).to_string();
        let mut lines = head.lines();
        let mut request_line = lines.next().unwrap_or_default().split_whitespace();
        let method =
            reqwest::Method::from_bytes(request_line.next().unwrap_or("GET").as_bytes()).unwrap();
        let target = request_line.next().unwrap_or("/").to_owned();
        let mut request = self
            .http
            .request(method, format!("{}{target}", self.origin))
            .body(bytes[header_end + 4..header_end + 4 + length].to_vec());
        for line in lines {
            if let Some((name, value)) = line.split_once(':')
                && !HOP_BY_HOP_HEADERS.contains(&name.trim().to_ascii_lowercase().as_str())
            {
                request = request.header(name.trim(), value.trim());
            }
        }
        let (status, headers, body) = match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let headers = response
                    .headers()
                    .iter()
                    .filter(|(name, _)| !HOP_BY_HOP_HEADERS.contains(&name.as_str()))
                    .filter_map(|(name, value)| {
                        Some(format!(
                            "{name}: {}\r\n",
                            value.to_str().ok()?.replace(&self.origin, &relay_origin)
                        ))
                    })
                    .collect::<String>();
                let body = response.bytes().await.unwrap_or_default();
                let body = String::from_utf8_lossy(&body).replace(&self.origin, &relay_origin);
                (status, headers, body)
            }
            Err(_) => (502, String::new(), String::new()),
        };
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 {status} Relay\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
    }
}

/// A person signed in to the web app with a real session cookie.
struct Person {
    service: Arc<RealService>,
    id: String,
    email: String,
    cookie: String,
}

impl Person {
    async fn sign_in(service: &Arc<RealService>, sub: &str) -> Self {
        let login: Value = service
            .http
            .get(format!(
                "{}/__fixture/login?sub={sub}&format=json",
                service.origin
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Self {
            service: service.clone(),
            id: login["user"]["id"].as_str().unwrap().into(),
            email: login["user"]["email"].as_str().unwrap().into(),
            cookie: login["cookie"]["value"].as_str().unwrap().into(),
        }
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut request = self
            .service
            .http
            .request(method.clone(), format!("{}{path}", self.service.origin))
            .header("cookie", format!("{SESSION_COOKIE}={}", self.cookie))
            .header("origin", &self.service.origin)
            .header("x-tickloop-expected-user", &self.id);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "{method} {path} -> {status}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    /// Follows a 202 operation until it commits, as the web app does.
    async fn settled(&self, result: Value, artifact_id: &str) -> Value {
        if !matches!(result["status"].as_str(), Some("pending" | "unknown")) {
            return result;
        }
        let path = format!(
            "/api/v1/artifacts/{artifact_id}/operations/{}",
            result["operation_id"].as_str().unwrap()
        );
        tokio::time::timeout(WAIT, async {
            loop {
                let outcome = self.request(reqwest::Method::GET, &path, None).await;
                if outcome["status"] == "committed" {
                    return outcome.get("result").cloned().unwrap_or(outcome);
                }
                assert_ne!(outcome["status"], "failed", "{outcome}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .unwrap()
    }

    /// Signs the host in through device authorization, approving the code as this person.
    async fn sign_in_host(&self, root: &std::path::Path, relay_origin: &str) -> ArtifactClient {
        let client =
            ArtifactClient::for_origin(root.canonicalize().unwrap(), relay_origin).unwrap();
        let device = client.start_device_login().await.unwrap();
        self.request(
            reqwest::Method::POST,
            "/api/v1/auth/device/approve",
            Some(json!({"user_code": device.user_code, "approve": true})),
        )
        .await;
        let user = client
            .finish_device_login(device, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(user.id, self.id);
        client
    }

    async fn comment(&self, artifact_id: &str, text: &str, quote: Option<&str>) -> Value {
        let mut body = json!({"operation_id": Uuid::new_v4(), "text": text});
        if let Some(quote) = quote {
            let anchor: Value = self
                .service
                .http
                .post(format!(
                    "{}/__fixture/anchors/markdown",
                    self.service.origin
                ))
                .json(&json!({"artifact_id": artifact_id, "quote": quote, "kind": "text", "occurrence": 1}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            body["anchor"] = anchor["anchor"].clone();
        }
        let created = self
            .request(
                reqwest::Method::POST,
                &format!("/api/v1/artifacts/{artifact_id}/threads"),
                Some(body),
            )
            .await;
        assert_eq!(created["request"]["status"], "pending", "{created}");
        created
    }

    async fn thread(&self, artifact_id: &str, thread_id: &str) -> Value {
        self.request(
            reqwest::Method::GET,
            &format!("/api/v1/artifacts/{artifact_id}/threads/{thread_id}"),
            None,
        )
        .await
    }

    async fn wait_for_request_status(&self, artifact_id: &str, thread_id: &str, status: &str) {
        tokio::time::timeout(WAIT, async {
            while self.thread(artifact_id, thread_id).await["thread"]["request"]["status"] != status
            {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("request in thread {thread_id} never became {status}"));
    }

    async fn add_viewer(&self, artifact_id: &str, viewer: &Person) {
        let invited = self
            .request(
                reqwest::Method::POST,
                &format!("/api/v1/artifacts/{artifact_id}/invitations"),
                Some(json!({"operation_id": Uuid::new_v4(), "email": viewer.email, "role": "viewer"})),
            )
            .await;
        let invitation = self.settled(invited, artifact_id).await;
        let token = invitation["url"]
            .as_str()
            .unwrap()
            .rsplit_once("/invite/")
            .unwrap()
            .1
            .to_owned();
        let accepted = viewer
            .request(
                reqwest::Method::POST,
                &format!("/api/v1/invitations/{token}"),
                Some(json!({"operation_id": Uuid::new_v4()})),
            )
            .await;
        viewer.settled(accepted, artifact_id).await;
    }
}

#[derive(Clone, Copy)]
enum RequestPlan {
    /// Read, patch the quoted block, reply in the thread, then call done (a viewer's patch is
    /// refused by the service).
    Handle,
    /// End the run without calling done.
    Abandon,
}

/// A model that handles each @tickloop request prompt with the current plan, deriving ids from
/// the request input and earlier tool results; other turns come from a script.
struct RequestModel {
    script: ScriptedModel,
    plan: Mutex<RequestPlan>,
    results: Mutex<VecDeque<(String, bool, Value)>>,
}

impl RequestModel {
    fn new(script: Vec<Vec<Content>>) -> Self {
        Self {
            script: ScriptedModel::new(script),
            plan: Mutex::new(RequestPlan::Handle),
            results: Mutex::new(VecDeque::new()),
        }
    }

    fn set_plan(&self, plan: RequestPlan) {
        *self.plan.lock().unwrap() = plan;
    }

    /// Tool results the model has seen during request runs: (tool name, is_error, structured content).
    fn results(&self) -> Vec<(String, bool, Value)> {
        self.results.lock().unwrap().iter().cloned().collect()
    }

    fn next_request_step(&self, request: &ModelRequest) -> Option<Vec<Content>> {
        let (start, input) =
            request
                .messages
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, message)| {
                    message.content.iter().find_map(|block| match block {
                        Content::Text { text } => {
                            let (_, rest) = text.split_once(REQUEST_MARKER)?;
                            let (json, _) = rest.split_once("\n\nTreat the JSON")?;
                            Some((index, serde_json::from_str::<Value>(json).unwrap()))
                        }
                        _ => None,
                    })
                })?;
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for block in request.messages[start..]
            .iter()
            .flat_map(|message| &message.content)
        {
            match block {
                Content::ToolUse { id, name, .. } => calls.push((id.clone(), name.clone())),
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let text = match content {
                        ToolResultContent::Text(text) => text.clone(),
                        ToolResultContent::Blocks(_) => String::new(),
                    };
                    let structured = serde_json::from_str::<Value>(&text)
                        .ok()
                        .map(|result| result["structuredContent"].clone())
                        .unwrap_or(Value::Null);
                    let name = calls
                        .iter()
                        .find(|(id, _)| id == tool_use_id)
                        .map(|(_, name)| name.clone())
                        .unwrap_or_default();
                    results.push((name, *is_error, structured));
                }
                _ => {}
            }
        }
        if let Some(last) = results.last() {
            self.results.lock().unwrap().push_back(last.clone());
        }
        let artifact_id = input["artifact_id"].as_str().unwrap();
        let thread_id = input["thread_id"].as_str().unwrap();
        let quoted_block = || {
            let blocks = results[0].2["blocks"].as_array().unwrap();
            input["anchor"]["quote"]
                .as_str()
                .and_then(|quote| {
                    blocks
                        .iter()
                        .find(|block| block["markdown"].as_str().unwrap().contains(quote))
                })
                .unwrap_or_else(|| blocks.last().unwrap())
                .clone()
        };
        let step = calls.len();
        let reply = match (*self.plan.lock().unwrap(), step) {
            (RequestPlan::Abandon, _) => text("Nothing to change."),
            (RequestPlan::Handle, 0) => call(
                "read",
                "ArtifactRead",
                json!({"action": "read", "artifact_id": artifact_id}),
            ),
            (RequestPlan::Handle, 1) => {
                let block = quoted_block();
                call(
                    "patch",
                    "ArtifactEdit",
                    json!({"action": "patch", "artifact_id": artifact_id, "patches": [
                        {"type": "replace", "target": block["id"], "expected_hash": block["hash"], "markdown": "A tighter first paragraph."}
                    ]}),
                )
            }
            (RequestPlan::Handle, 2) => call(
                "reply",
                "ArtifactComment",
                json!({"action": "reply", "artifact_id": artifact_id, "thread_id": thread_id, "text": "Looking at this now."}),
            ),
            (RequestPlan::Handle, 3) => call(
                "done",
                "ArtifactComment",
                json!({"action": "done", "artifact_id": artifact_id, "outcome": "done", "summary": "Handled the request."}),
            ),
            _ => text("Handled."),
        };
        Some(reply)
    }
}

#[async_trait]
impl ModelClient for RequestModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.script.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        match self.next_request_step(&request) {
            Some(content) => Ok(ModelOutput {
                private_source: None,
                content,
                usage: Default::default(),
                stop_reason: "end_turn".into(),
            }),
            None => self.script.complete(request, events, cancel).await,
        }
    }
}

async fn host_agent(client: ArtifactClient, model: Arc<RequestModel>) -> Agent {
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(client, None)))
        .unwrap();
    agent
}

async fn full_access_chat(agent: &Agent, root: &std::path::Path) -> ChatId {
    let chat = agent.create_chat(root.into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    chat
}

async fn wait_for_last_loop(agent: &Agent, chat: ChatId, state: LoopState) {
    tokio::time::timeout(WAIT, async {
        while agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .is_none_or(|goal| goal.state != state)
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the last Loop never reached {state:?}"));
}

fn agent_messages(thread: &Value) -> Vec<String> {
    thread["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["author"]["type"] == "agent")
        .map(|message| message["text"].as_str().unwrap().to_owned())
        .collect()
}

fn service_code(error: ArtifactError) -> String {
    match error {
        ArtifactError::Service { status, code } => format!("{status} {code}"),
        other => panic!("expected a service error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the realapp harness; see the module docs"]
async fn host_runs_the_artifact_v2_contract_against_the_real_service() {
    let service = Arc::new(RealService::from_env());
    let relay = service.start_relay().await;
    let owner = Person::sign_in(&service, "owner").await;
    let viewer = Person::sign_in(&service, "viewer").await;
    let owner_root = tempfile::tempdir().unwrap();
    let viewer_root = tempfile::tempdir().unwrap();

    // MCP discovery: the host keeps exactly the three Artifact tools, unprefixed.
    let owner_client = owner.sign_in_host(owner_root.path(), &relay).await;
    let directory = McpHostToolProvider::new(owner_client.clone(), None);
    directory.refresh(CancellationToken::new()).await.unwrap();
    let mut names = directory
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["ArtifactComment", "ArtifactEdit", "ArtifactRead"]);
    assert_eq!(
        directory.negotiated_version().as_deref(),
        Some("2026-07-28")
    );

    // ArtifactEdit create: the host links the new artifact to the creating Loop.
    let owner_model = Arc::new(RequestModel::new(vec![
        call(
            "create",
            "ArtifactEdit",
            json!({"action": "create", "title": "Real brief", "format": "markdown", "content": "# Real brief\n\nFirst paragraph that is far too long.\n"}),
        ),
        text("Created."),
    ]));
    let owner_agent = host_agent(owner_client.clone(), owner_model.clone()).await;
    let owner_chat = full_access_chat(&owner_agent, owner_root.path()).await;
    owner_agent
        .submit(owner_chat, None, "Write a brief".into())
        .await
        .unwrap();
    wait_for_last_loop(&owner_agent, owner_chat, LoopState::WaitingTrigger).await;
    let snapshot = owner_agent.snapshot(owner_chat).await.unwrap();
    let created = snapshot.artifact_ticks[0].clone();
    assert_eq!(created.title, "Real brief");
    assert!(created.url.starts_with(&relay), "{}", created.url);
    let goal = snapshot.loops.last().unwrap();
    assert_eq!(goal.event_sources[0].artifact_id, created.artifact_id);
    let metadata = owner
        .request(
            reqwest::Method::GET,
            &format!("/api/v1/artifacts/{}", created.artifact_id),
            None,
        )
        .await;
    assert_eq!(metadata["agent_link"]["chat_id"], owner_chat.to_string());
    assert_eq!(metadata["agent_link"]["loop_id"], goal.id.to_string());
    assert_eq!(metadata["agent_link"]["epoch"], 1);
    let artifact_id = created.artifact_id.clone();

    // An @tickloop comment is claimed, edited, answered and finished inside the claimed request.
    let first = owner
        .comment(
            &artifact_id,
            "@tickloop tighten this paragraph",
            Some("First paragraph"),
        )
        .await;
    let first_thread = first["thread_id"].as_str().unwrap();
    owner
        .wait_for_request_status(&artifact_id, first_thread, "done")
        .await;
    let thread = owner.thread(&artifact_id, first_thread).await;
    assert_eq!(
        agent_messages(&thread),
        ["Looking at this now.", "Handled the request."]
    );
    let results = owner_model.results();
    assert!(
        results.iter().all(|(name, is_error, structured)| !is_error
            && !name.is_empty()
            && structured.is_object()),
        "{results:?}"
    );
    assert_eq!(results[0].2["access_role"], "owner");
    assert_eq!(results[1].2["changed"].as_array().unwrap().len(), 1);
    assert_eq!(results[3].2["status"], "done");
    let document = owner
        .request(
            reqwest::Method::GET,
            &format!("/api/v1/artifacts/{artifact_id}/edit"),
            None,
        )
        .await;
    assert!(
        document.to_string().contains("A tighter first paragraph."),
        "{document}"
    );

    // A run that ends without done fails its request.
    owner_model.set_plan(RequestPlan::Abandon);
    let second = owner
        .comment(&artifact_id, "@tickloop anything else?", None)
        .await;
    let second_thread = second["thread_id"].as_str().unwrap();
    owner
        .wait_for_request_status(&artifact_id, second_thread, "failed")
        .await;

    // A viewer's own host: linking from the picker, replies allowed, edits refused.
    owner.add_viewer(&artifact_id, &viewer).await;
    let viewer_client = viewer.sign_in_host(viewer_root.path(), &relay).await;
    let viewer_model = Arc::new(RequestModel::new(Vec::new()));
    let viewer_agent = host_agent(viewer_client, viewer_model.clone()).await;
    let viewer_chat = full_access_chat(&viewer_agent, viewer_root.path()).await;
    viewer_agent
        .link_artifact(
            viewer_chat,
            &ArtifactTick {
                access_role: "view".into(),
                origin: relay.clone(),
                ..created.clone()
            },
        )
        .await
        .unwrap();
    let viewer_thread = viewer
        .comment(&artifact_id, "@tickloop please summarize", None)
        .await;
    let viewer_thread = viewer_thread["thread_id"].as_str().unwrap();
    viewer
        .wait_for_request_status(&artifact_id, viewer_thread, "done")
        .await;
    let viewer_results = viewer_model.results();
    assert_eq!(viewer_results[0].2["access_role"], "view");
    assert!(viewer_results[1].1, "{viewer_results:?}");
    assert_eq!(viewer_results[1].2["error"]["code"], "forbidden");
    assert!(!viewer_results[2].1, "{viewer_results:?}");
    let thread = owner.thread(&artifact_id, viewer_thread).await;
    assert_eq!(
        agent_messages(&thread),
        ["Looking at this now.", "Handled the request."]
    );
    let metadata = owner
        .request(
            reqwest::Method::GET,
            &format!("/api/v1/artifacts/{artifact_id}"),
            None,
        )
        .await;
    assert_eq!(
        metadata["agent_link"]["chat_id"],
        owner_chat.to_string(),
        "each account has its own link"
    );
    viewer_agent.shutdown().await;
    owner_agent.shutdown().await;

    // The lifecycle client directly: a replacing link, claim, renew and release semantics.
    let account = owner.id.as_str();
    let old_link = AgentLink {
        artifact_id: artifact_id.clone(),
        binding_id: Uuid::parse_str(metadata["agent_link"]["binding_id"].as_str().unwrap())
            .unwrap(),
        epoch: 1,
    };
    let client_instance_id = Uuid::new_v4();
    let link = owner_client
        .link_agent(
            account,
            &artifact_id,
            &AgentLinkRequest {
                binding_id: Uuid::new_v4(),
                client_instance_id,
                chat_id: Uuid::new_v4(),
                loop_id: Uuid::new_v4(),
            },
        )
        .await
        .unwrap();
    assert_eq!(link.epoch, 2);
    assert_eq!(
        service_code(
            owner_client
                .pending_requests(account, &old_link, None)
                .await
                .unwrap_err()
        ),
        "409 agent_link_missing"
    );
    let third = owner
        .comment(&artifact_id, "@tickloop one more", None)
        .await;
    let request_id = third["request"]["request_id"].as_str().unwrap().to_owned();
    let pending = owner_client
        .pending_requests(account, &link, None)
        .await
        .unwrap();
    assert_eq!(pending.items.len(), 1);
    assert_eq!(pending.items[0].request_id, request_id);
    let claim = owner_client
        .claim_request(account, &link, client_instance_id, &request_id)
        .await
        .unwrap();
    assert_eq!(claim.input["access_role"], "owner");
    assert_eq!(claim.input["text"], "@tickloop one more");
    assert!(claim.input["anchor"].is_null());
    assert_eq!(
        service_code(
            owner_client
                .claim_request(account, &link, client_instance_id, &request_id)
                .await
                .unwrap_err()
        ),
        "409 claim_conflict"
    );
    let renewed = owner_client
        .renew_request(account, &artifact_id, &claim)
        .await
        .unwrap();
    assert!(renewed.as_str() >= claim.lease_expires_at.as_str());
    owner_client
        .release_request(account, &artifact_id, &claim, ReleaseOutcome::Retry)
        .await
        .unwrap();
    assert_eq!(
        service_code(
            owner_client
                .renew_request(account, &artifact_id, &claim)
                .await
                .unwrap_err()
        ),
        "409 claim_expired"
    );
    let pending = owner_client
        .pending_requests(account, &link, None)
        .await
        .unwrap();
    assert_eq!(pending.items[0].request_id, request_id);
    let claim = owner_client
        .claim_request(account, &link, client_instance_id, &request_id)
        .await
        .unwrap();
    owner_client
        .release_request(account, &artifact_id, &claim, ReleaseOutcome::Failed)
        .await
        .unwrap();
    owner
        .wait_for_request_status(&artifact_id, third["thread_id"].as_str().unwrap(), "failed")
        .await;
    assert!(
        owner_client
            .pending_requests(account, &link, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
