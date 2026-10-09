//! A controllable Artifact Service: device login, the `tickloop-docs` MCP tools and the Agent
//! request lifecycle under `/api/v1/agent/*`.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use serde_json::{Value, json};
use tickloop_agent_artifacts::ArtifactClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};
use tokio_util::sync::CancellationToken;

pub const REQUEST_CREATED_AT: &str = "2026-10-08T10:00:00.000Z";
pub const REQUEST_CREATED_AT_MS: u64 = 1_791_453_600_000;
pub const CREATED_ARTIFACT: &str = "created-doc";
const PROTOCOL_VERSION: &str = "2026-07-28";
/// The service's `tools/list`, recorded from `artifact`.
const RECORDED_TOOLS: &str = include_str!("../fixtures/artifact_service_tools.json");

#[derive(Clone)]
pub struct Link {
    pub binding_id: String,
    pub client_instance_id: String,
    pub chat_id: String,
    pub loop_id: String,
    pub epoch: u64,
}

#[derive(Default)]
pub struct ServiceControl {
    pub requests: Mutex<Vec<Value>>,
    pub other_account: AtomicBool,
    pub credential_revision: AtomicUsize,
    pub signed_out: AtomicBool,
    pub reject_mcp: AtomicBool,
    pub directory_unavailable: AtomicBool,
    pub session_unavailable: AtomicBool,
    pub hold_session_response: AtomicBool,
    pub session_response_started: Notify,
    pub release_session_response: Notify,
    pub server_name: Mutex<Option<String>>,
    pub directory_revision: AtomicUsize,
    pub omit_comment_tool: AtomicBool,
    pub drop_call_response: AtomicBool,
    pub fail_link: AtomicBool,
    pub links: Mutex<BTreeMap<String, Link>>,
    pub pending: Mutex<Vec<(String, String)>>,
    pub running: Mutex<BTreeMap<String, String>>,
    pub releases: Mutex<Vec<(String, String)>>,
    pub finished: Mutex<Vec<(String, String)>>,
}

pub struct ArtifactService {
    pub origin: String,
    pub control: Arc<ServiceControl>,
}

fn account(control: &ServiceControl) -> &'static str {
    if control.other_account.load(Ordering::SeqCst) {
        "user-two"
    } else {
        "user-one"
    }
}

fn query(target: &str) -> BTreeMap<String, String> {
    target
        .split_once('?')
        .map(|(_, query)| {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn tools(control: &ServiceControl) -> Value {
    let mut tools: Value = serde_json::from_str(RECORDED_TOOLS).unwrap();
    let listed = tools["tools"].as_array_mut().unwrap();
    for tool in listed.iter_mut() {
        if tool["name"] == "ArtifactEdit" {
            let description = format!(
                "{} (directory revision {})",
                tool["description"].as_str().unwrap(),
                control.directory_revision.load(Ordering::SeqCst)
            );
            tool["description"] = description.into();
        }
    }
    listed.push(json!({"name":"unrelated_remote_tool","description":"Not one of the host's Artifact tools.","inputSchema":{"type":"object"}}));
    if control.omit_comment_tool.load(Ordering::SeqCst) {
        listed.retain(|tool| tool["name"] != "ArtifactComment");
    }
    tools
}

/// A Markdown text anchor as the service stores and returns it.
fn thread_anchor() -> Value {
    json!({"schema":1,"kind":"text","working_id":"working-1","generation":"generation-1","representation":"rich","quote":"Fixture body","start":{"relative":"AQ==","block_id":"b1"},"end":{"relative":"Ag==","block_id":"b1"}})
}

const ANCHOR_DESCRIPTION: &str = "Selected text: Fixture body";

fn tool_error(code: &str, message: &str) -> Value {
    json!({"content":[{"type":"text","text":format!("{code}: {message}")}],"isError":true,"structuredContent":{"error":{"code":code,"message":message}}})
}

fn call_tool(control: &ServiceControl, origin: &str, params: &Value) -> Value {
    let arguments = &params["arguments"];
    let artifact_id = arguments["artifact_id"].as_str().unwrap_or("doc");
    let structured = match (
        params["name"].as_str().unwrap_or(""),
        arguments["action"].as_str().unwrap_or(""),
    ) {
        ("ArtifactRead", "read") => {
            json!({"artifact_id":artifact_id,"title":"Fixture","format":"markdown","url":format!("{origin}/a/{artifact_id}"),"access_role":"owner","revision":3,"blocks":[{"id":"b1","hash":"h1","markdown":"# Fixture body"}],"next_cursor":null})
        }
        ("ArtifactRead", "threads") => {
            json!({"threads":[{"thread_id":"thread-1","status":"open","anchor":thread_anchor(),"anchor_description":ANCHOR_DESCRIPTION,"messages":[{"message_id":"m1","author":{"name":"One","kind":"human"},"text":"@tickloop tighten this","created_at":REQUEST_CREATED_AT}],"request":{"request_id":"request-1","status":"pending","requested_by":"One"}}],"next_cursor":null})
        }
        ("ArtifactRead", _) => json!({"items":[],"next_cursor":null}),
        ("ArtifactEdit", "create") => {
            json!({"artifact_id":CREATED_ARTIFACT,"title":arguments["title"],"format":arguments["format"],"url":format!("{origin}/a/{CREATED_ARTIFACT}"),"revision":0})
        }
        ("ArtifactEdit", "patch") => {
            json!({"artifact_id":artifact_id,"revision":4,"changed":[{"id":"b1","hash":"h2"}]})
        }
        ("ArtifactEdit", _) => json!({"artifact_id":artifact_id,"revision":4}),
        ("ArtifactComment", "reply") => {
            json!({"thread_id":arguments["thread_id"],"message_id":"m2"})
        }
        ("ArtifactComment", "done") => {
            let context = &params["_meta"]["tickloop.ai/event-bridge"];
            let Some(request_id) = context["event_id"].as_str() else {
                return tool_error(
                    "invalid_request",
                    "done needs the context of the claimed @tickloop request",
                );
            };
            let outcome = arguments["outcome"].as_str().unwrap_or("done").to_owned();
            control.running.lock().unwrap().remove(request_id);
            control
                .finished
                .lock()
                .unwrap()
                .push((request_id.to_owned(), outcome.clone()));
            json!({"request_id":request_id,"status":outcome})
        }
        _ => json!({}),
    };
    json!({"content":[{"type":"text","text":structured.to_string()}],"structuredContent":structured,"isError":false})
}

/// The standard request headers a 2026-07-28 server requires to mirror the JSON-RPC body; the
/// service answers a missing or disagreeing header with HTTP 400 and error -32020.
fn header_mismatch(head: &str, body: &Value) -> Option<Value> {
    let header = |name: &str| {
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let method = body["method"].as_str().unwrap_or_default();
    let mismatch = if header("mcp-protocol-version").as_deref() != Some(PROTOCOL_VERSION) {
        Some("MCP-Protocol-Version")
    } else if header("mcp-method").as_deref() != Some(method) {
        Some("Mcp-Method")
    } else if method == "tools/call"
        && header("mcp-name").as_deref() != body["params"]["name"].as_str()
    {
        Some("Mcp-Name")
    } else {
        None
    }?;
    Some(
        json!({"jsonrpc":"2.0","id":body["id"],"error":{"code":-32020,"message":format!("Bad Request: the {mismatch} header is missing or disagrees with the body")}}),
    )
}

fn mcp(control: &ServiceControl, origin: &str, body: &Value) -> Value {
    let name = control
        .server_name
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "tickloop-docs".into());
    let result = match body["method"].as_str().unwrap_or("") {
        "server/discover" => {
            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":true}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":name,"version":"1"}}})
        }
        "tools/list" => tools(control),
        "tools/call" => call_tool(control, origin, &body["params"]),
        method => panic!("unexpected MCP method: {method}"),
    };
    json!({"jsonrpc":"2.0","id":body["id"],"result":result})
}

fn lifecycle(control: &ServiceControl, method: &str, target: &str, body: &Value) -> (u16, Value) {
    let path = target.split('?').next().unwrap_or("");
    let segments = path.split('/').collect::<Vec<_>>();
    match (method, segments.as_slice()) {
        ("PUT", ["", "api", "v1", "agent", "links", artifact_id]) => {
            if control.fail_link.load(Ordering::SeqCst) {
                return (
                    503,
                    json!({"error":{"code":"unavailable","message":"try later"}}),
                );
            }
            let mut links = control.links.lock().unwrap();
            let mut link = Link {
                binding_id: body["binding_id"].as_str().unwrap().to_owned(),
                client_instance_id: body["client_instance_id"].as_str().unwrap().to_owned(),
                chat_id: body["chat_id"].as_str().unwrap().to_owned(),
                loop_id: body["loop_id"].as_str().unwrap().to_owned(),
                epoch: 1,
            };
            if let Some(current) = links.get(*artifact_id) {
                let unchanged = current.binding_id == link.binding_id
                    && current.client_instance_id == link.client_instance_id
                    && current.chat_id == link.chat_id
                    && current.loop_id == link.loop_id;
                link.epoch = current.epoch + u64::from(!unchanged);
            }
            let epoch = link.epoch;
            links.insert((*artifact_id).to_owned(), link.clone());
            (
                200,
                json!({"artifact_id":artifact_id,"binding_id":link.binding_id,"epoch":epoch}),
            )
        }
        ("GET", ["", "api", "v1", "agent", "requests"]) => {
            let query = query(target);
            let artifact_id = query.get("artifact_id").cloned().unwrap_or_default();
            let current = control.links.lock().unwrap().get(&artifact_id).cloned();
            if current.is_none_or(|link| {
                Some(&link.binding_id) != query.get("binding_id")
                    || Some(&link.epoch.to_string()) != query.get("epoch")
            }) {
                return (
                    409,
                    json!({"error":{"code":"agent_link_missing","message":"link replaced"}}),
                );
            }
            let items = control
                .pending
                .lock()
                .unwrap()
                .iter()
                .filter(|(artifact, _)| *artifact == artifact_id)
                .map(|(_, request_id)| json!({"request_id":request_id,"thread_id":"thread-1","created_at":REQUEST_CREATED_AT}))
                .collect::<Vec<_>>();
            (200, json!({"items":items,"next_cursor":null}))
        }
        ("POST", ["", "api", "v1", "agent", "requests", request_id, "claim"]) => {
            let artifact_id = body["artifact_id"].as_str().unwrap_or_default().to_owned();
            let current = control.links.lock().unwrap().get(&artifact_id).cloned();
            if current.is_none_or(|link| {
                body["binding_id"] != link.binding_id.as_str()
                    || body["epoch"] != link.epoch
                    || body["client_instance_id"] != link.client_instance_id.as_str()
            }) {
                return (
                    409,
                    json!({"error":{"code":"agent_link_missing","message":"link replaced"}}),
                );
            }
            let mut pending = control.pending.lock().unwrap();
            let Some(index) = pending
                .iter()
                .position(|(artifact, request)| *artifact == artifact_id && request == request_id)
            else {
                return (
                    409,
                    json!({"error":{"code":"claim_conflict","message":"not pending"}}),
                );
            };
            pending.remove(index);
            control
                .running
                .lock()
                .unwrap()
                .insert((*request_id).to_owned(), artifact_id.clone());
            (
                200,
                json!({"request_id":request_id,"claim_id":format!("claim-{request_id}"),"fencing_token":format!("fence-{request_id}"),"lease_expires_at":"2099-01-01T00:00:00Z","input":{"artifact_id":artifact_id,"thread_id":"thread-1","text":"@tickloop tighten the introduction","anchor":thread_anchor(),"anchor_description":ANCHOR_DESCRIPTION,"requested_by":"One","access_role":"owner","messages":[{"message_id":"m1","author":{"name":"One","kind":"human"},"text":"@tickloop tighten the introduction","created_at":REQUEST_CREATED_AT}]}}),
            )
        }
        (
            "POST",
            [
                "",
                "api",
                "v1",
                "agent",
                "requests",
                request_id,
                "renew" | "release",
            ],
        ) if body["claim_id"] != format!("claim-{request_id}")
            || body["fencing_token"] != format!("fence-{request_id}")
            || !control.running.lock().unwrap().contains_key(*request_id) =>
        {
            (
                409,
                json!({"error":{"code":"claim_expired","message":"Feedback claim expired or was replaced"}}),
            )
        }
        ("POST", ["", "api", "v1", "agent", "requests", _, "renew"]) => {
            (200, json!({"lease_expires_at":"2099-01-01T00:00:00Z"}))
        }
        ("POST", ["", "api", "v1", "agent", "requests", request_id, "release"]) => {
            let outcome = body["outcome"].as_str().unwrap_or_default().to_owned();
            let artifact_id = control.running.lock().unwrap().remove(*request_id);
            if outcome == "retry"
                && let Some(artifact_id) = artifact_id
            {
                control
                    .pending
                    .lock()
                    .unwrap()
                    .push((artifact_id, (*request_id).to_owned()));
            }
            control
                .releases
                .lock()
                .unwrap()
                .push(((*request_id).to_owned(), outcome.clone()));
            (
                200,
                json!({"request_id":request_id,"status":if outcome == "retry" {"pending"} else {"failed"}}),
            )
        }
        ("GET", ["", "api", "v1", "artifacts", artifact_id]) => {
            let link = control.links.lock().unwrap().get(*artifact_id).cloned();
            (
                200,
                json!({"id":artifact_id,"title":"Fixture","format":"markdown","access_role":"owner","agent_link":link.map(|link| json!({"binding_id":link.binding_id,"chat_id":link.chat_id,"loop_id":link.loop_id,"epoch":link.epoch}))}),
            )
        }
        _ => (
            404,
            json!({"error":{"code":"not_found","message":"not found"}}),
        ),
    }
}

impl ArtifactService {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let control = Arc::new(ServiceControl::default());
        let server_control = control.clone();
        let server_origin = origin.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let control = server_control.clone();
                let origin = server_origin.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 4096];
                    let header_end = loop {
                        let Ok(count) = stream.read(&mut buffer).await else {
                            return;
                        };
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            let headers =
                                String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                            let length = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length: "))
                                .and_then(|length| length.parse::<usize>().ok())
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                break end;
                            }
                        }
                    };
                    let head = String::from_utf8_lossy(&bytes[..header_end]).to_string();
                    let line = head.lines().next().unwrap_or("").to_owned();
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_owned();
                    let target = parts.next().unwrap_or("").to_owned();
                    let body: Value =
                        serde_json::from_slice(&bytes[header_end + 4..]).unwrap_or(Value::Null);
                    control
                        .requests
                        .lock()
                        .unwrap()
                        .push(json!({"path":line,"headers":head,"body":body}));
                    let user =
                        json!({"id":account(&control),"name":"One","email":"one@example.test"});
                    let is_mcp = method == "POST" && target == "/mcp";
                    if is_mcp && body.get("id").is_none() {
                        let _ = stream
                            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let (mut status, response) = if target == "/api/v1/auth/device" {
                        (
                            200,
                            json!({"device_code":"test-device","user_code":"TEST","verification_uri":format!("{origin}/device?code=TEST"),"expires_in":30,"interval":1}),
                        )
                    } else if target == "/api/v1/auth/device/token" {
                        let revision = control.credential_revision.load(Ordering::SeqCst);
                        (
                            200,
                            json!({"access_token":format!("test-service-token-{revision}"),"token_type":"Bearer","expires_in":600,"user":user}),
                        )
                    } else if target == "/api/v1/auth/session" {
                        (
                            200,
                            json!({"user":user,"expires_at":"2099-01-01T00:00:00Z"}),
                        )
                    } else if target == "/api/v1/auth/logout" {
                        control.signed_out.store(true, Ordering::SeqCst);
                        (200, json!({"revoked":true}))
                    } else if let Some(error) =
                        is_mcp.then(|| header_mismatch(&head, &body)).flatten()
                    {
                        (400, error)
                    } else if is_mcp {
                        if body["method"] == "tools/call"
                            && control.drop_call_response.load(Ordering::SeqCst)
                        {
                            return;
                        }
                        (200, mcp(&control, &origin, &body))
                    } else {
                        lifecycle(&control, &method, &target, &body)
                    };
                    if (is_mcp && control.reject_mcp.load(Ordering::SeqCst))
                        || (control.signed_out.load(Ordering::SeqCst)
                            && (is_mcp || target == "/api/v1/auth/session"))
                    {
                        status = 401;
                    } else if (is_mcp && control.directory_unavailable.load(Ordering::SeqCst))
                        || (target == "/api/v1/auth/session"
                            && control.session_unavailable.load(Ordering::SeqCst))
                    {
                        status = 503;
                    }
                    if target == "/api/v1/auth/session"
                        && control.hold_session_response.swap(false, Ordering::SeqCst)
                    {
                        control.session_response_started.notify_one();
                        control.release_session_response.notified().await;
                    }
                    let response = response.to_string();
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                                response.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        Self { origin, control }
    }

    pub fn client(&self, root: &Path) -> ArtifactClient {
        ArtifactClient::for_origin(root.canonicalize().unwrap(), &self.origin).unwrap()
    }

    /// Signs in through device authorization and returns the signed-in client.
    pub async fn login(&self, root: &Path) -> ArtifactClient {
        self.control.signed_out.store(false, Ordering::SeqCst);
        let client = self.client(root);
        let device = client.start_device_login().await.unwrap();
        client
            .finish_device_login(device, &CancellationToken::new())
            .await
            .unwrap();
        client
    }

    pub fn request_feedback(&self, artifact_id: &str, request_id: &str) {
        self.control
            .pending
            .lock()
            .unwrap()
            .push((artifact_id.into(), request_id.into()));
    }

    pub fn link(&self, artifact_id: &str) -> Option<Link> {
        self.control.links.lock().unwrap().get(artifact_id).cloned()
    }

    /// Another Chat of the same account takes over the artifact's link.
    pub fn replace_link(&self, artifact_id: &str) {
        let mut links = self.control.links.lock().unwrap();
        let link = links.get_mut(artifact_id).unwrap();
        link.binding_id = uuid::Uuid::new_v4().to_string();
        link.epoch += 1;
    }

    pub fn is_pending(&self, request_id: &str) -> bool {
        self.control
            .pending
            .lock()
            .unwrap()
            .iter()
            .any(|(_, pending)| pending == request_id)
    }

    pub fn releases(&self) -> Vec<(String, String)> {
        self.control.releases.lock().unwrap().clone()
    }

    pub fn finished(&self) -> Vec<(String, String)> {
        self.control.finished.lock().unwrap().clone()
    }

    pub fn requests(&self) -> Vec<Value> {
        self.control.requests.lock().unwrap().clone()
    }

    pub fn tool_calls(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .map(|request| request["body"]["params"].clone())
            .collect()
    }

    pub fn lifecycle_requests(&self, prefix: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request["path"].as_str().unwrap().starts_with(prefix))
            .collect()
    }
}
