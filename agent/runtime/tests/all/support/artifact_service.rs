//! A controllable Tickloop Artifacts service: device login, the `tickloop-artifacts` MCP tools, the Agent
//! request lifecycle under `/api/v1/agent/*` and Artifact image uploads.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tickloop_agent_artifacts::{APP_BASE_PATH, ArtifactClient, artifact_url};
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
/// The service's initialization and `tools/list`, as `artifact` publishes them.
const RECORDED_TOOLS: &str = include_str!("../fixtures/artifact_service_tools.json");

/// One image upload the service received, with the receipt it committed.
#[derive(Clone)]
pub struct Upload {
    pub artifact_id: String,
    pub operation_id: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub receipt: Value,
}

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
    pub compact_ids: AtomicBool,
    pub other_account: AtomicBool,
    pub credential_revision: AtomicUsize,
    pub signed_out: AtomicBool,
    pub hold_device_approval: AtomicBool,
    pub refuse_token_refresh: AtomicBool,
    pub reject_mcp: AtomicBool,
    pub reject_discovery: AtomicBool,
    pub reject_tool_listing: AtomicBool,
    pub refuse_tool_bearer: Mutex<Option<String>>,
    pub directory_unavailable: AtomicBool,
    pub skills_enabled: AtomicBool,
    pub resources_enabled: AtomicBool,
    pub skill_revision: AtomicUsize,
    pub skill_reference_bytes: AtomicUsize,
    pub tamper_skill: AtomicBool,
    pub hold_skill_response: AtomicBool,
    pub skill_response_started: Notify,
    pub release_skill_response: Notify,
    pub session_unavailable: AtomicBool,
    pub hold_status_response: AtomicBool,
    pub status_response_started: Notify,
    pub release_status_response: Notify,
    pub hold_session_response: AtomicBool,
    pub session_response_started: Notify,
    pub release_session_response: Notify,
    pub server_name: Mutex<Option<String>>,
    pub directory_revision: AtomicUsize,
    /// Changes the instructions discovery returns when above zero.
    pub instructions_revision: AtomicUsize,
    pub omit_comment_tool: AtomicBool,
    /// Answers a creation with the catalog's stored format names, as deployed services do.
    pub catalog_format_names: AtomicBool,
    pub read_result: Mutex<Option<Value>>,
    pub drop_call_response: AtomicBool,
    pub fail_link: AtomicBool,
    /// Who the fixture artifact is published to, as its metadata reports it; unknown when unset.
    pub publication: Mutex<Option<Value>>,
    /// The stored format the fixture artifact's metadata reports; `markdown` when unset.
    pub metadata_format: Mutex<Option<String>>,
    pub uploads: Mutex<Vec<Upload>>,
    /// Commits an upload but closes the connection before answering it.
    pub drop_upload_response: AtomicBool,
    /// Answers the operation receipt of a lost upload as if the upload never committed.
    pub forget_upload_operations: AtomicBool,
    /// Stores only an image's eight-byte signature, as if the rest were metadata it strips, so
    /// the receipt describes a smaller file than the one sent.
    pub strip_upload_metadata: AtomicBool,
    /// Refuses every upload with this status, error code and message.
    pub refuse_uploads: Mutex<Option<(u16, String, String)>>,
    /// Commits an upload but never answers it, after signalling `upload_held`.
    pub hold_upload_response: AtomicBool,
    pub upload_held: Notify,
    pub links: Mutex<BTreeMap<String, Link>>,
    pub pending: Mutex<Vec<(String, String)>>,
    pub deliveries: Mutex<BTreeMap<String, (String, String, String, bool)>>,
    pub claims: Mutex<BTreeMap<String, String>>,
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

pub fn instructions(control: &ServiceControl) -> String {
    let recorded: Value = serde_json::from_str(RECORDED_TOOLS).unwrap();
    let instructions = recorded["initialize"]["instructions"].as_str().unwrap();
    match control.instructions_revision.load(Ordering::SeqCst) {
        0 => instructions.to_owned(),
        revision => format!("{instructions} (instructions revision {revision})"),
    }
}

fn tools(control: &ServiceControl) -> Value {
    let mut recorded: Value = serde_json::from_str(RECORDED_TOOLS).unwrap();
    let mut tools = json!({"tools": recorded["tools"].take()});
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

/// A doc text anchor as the service stores and returns it.
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
    let mut structured = match (
        params["name"].as_str().unwrap_or(""),
        arguments["action"].as_str().unwrap_or(""),
    ) {
        ("ArtifactRead", "read") => {
            if let Some(result) = control.read_result.lock().unwrap().clone() {
                return result;
            }
            json!({"artifact_id":artifact_id,"title":"Fixture","format":"doc","url":artifact_url(origin, artifact_id),"access_role":"owner","revision":3,"blocks":[{"id":"b1","hash":"h1","text":"# Fixture body"}],"next_cursor":null})
        }
        ("ArtifactRead", "threads") => {
            json!({"threads":[{"thread_id":"thread-1","status":"open","anchor":thread_anchor(),"anchor_description":ANCHOR_DESCRIPTION,"messages":[{"message_id":"m1","author":{"name":"One","kind":"human"},"text":"@tickloop tighten this","created_at":REQUEST_CREATED_AT}],"feedback_id":"request-1"}],"next_cursor":null})
        }
        ("ArtifactRead", _) => json!({"items":[],"next_cursor":null}),
        ("ArtifactEdit", "create") => {
            let format = match arguments["format"].as_str() {
                Some("doc") if control.catalog_format_names.load(Ordering::SeqCst) => {
                    json!("markdown")
                }
                Some("page") if control.catalog_format_names.load(Ordering::SeqCst) => {
                    json!("html")
                }
                _ => arguments["format"].clone(),
            };
            let mut created = json!({"artifact_id":CREATED_ARTIFACT,"title":arguments["title"],"format":format,"url":artifact_url(origin, CREATED_ARTIFACT),"revision":0});
            if format == "slides" {
                created["blocks"] = json!([{"id":"cover","hash":"c1"}]);
            }
            created
        }
        ("ArtifactEdit", "patch") => {
            json!({"artifact_id":artifact_id,"revision":4,"changed":[{"id":"b1","hash":"h2"}]})
        }
        ("ArtifactEdit", "rename") if arguments["expected_title"] != "Fixture" => {
            return tool_error("content_conflict", "The title changed since it was read");
        }
        ("ArtifactEdit", "rename") => {
            json!({"artifact_id":artifact_id,"title":arguments["title"].as_str().unwrap_or_default().trim(),"revision":4})
        }
        ("ArtifactEdit", _) => json!({"artifact_id":artifact_id,"revision":4}),
        ("ArtifactComment", "reply") => {
            json!({"thread_id":arguments["thread_id"],"message_id":"m2"})
        }
        _ => json!({}),
    };
    if let Some(id) = structured.get("artifact_id").and_then(Value::as_str) {
        structured["artifact_id"] = json!(wire_service_id(control, id));
    }
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
    if (body["method"] == "server/discover" && control.reject_discovery.load(Ordering::SeqCst))
        || (body["method"] == "tools/list" && control.reject_tool_listing.load(Ordering::SeqCst))
    {
        return json!({"jsonrpc":"2.0","id":body["id"],"error":{"code":-32601,"message":"Method not found: test-service-token-0","data":{"token":"test-service-token-0"}}});
    }
    let name = control
        .server_name
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "tickloop-artifacts".into());
    if body["method"] == "resources/read" {
        let uri = body["params"]["uri"].as_str().unwrap_or("");
        let is_fixture = matches!(
            uri,
            "fixture://tickloop-artifacts/info" | "fixture://tickloop-artifacts/details"
        );
        let is_skill = skill_entries(
            control.skill_revision.load(Ordering::SeqCst),
            control.skill_reference_bytes.load(Ordering::SeqCst),
        )
        .iter()
        .any(|entry| {
            entry["resources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|resource| resource["uri"] == uri)
        });
        if !is_fixture && !is_skill {
            return json!({"jsonrpc":"2.0","id":body["id"],"error":{"code":-32002,"message":"Resource not found"}});
        }
    }
    let mut result = match body["method"].as_str().unwrap_or("") {
        "server/discover" => {
            let mut capabilities = json!({"tools":{"listChanged":true}});
            if control.resources_enabled.load(Ordering::SeqCst)
                || control.skills_enabled.load(Ordering::SeqCst)
            {
                capabilities["resources"] = json!({});
            }
            if control.skills_enabled.load(Ordering::SeqCst) {
                capabilities["extensions"] = json!({"io.modelcontextprotocol/skills":{}});
            }
            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":capabilities,"instructions":instructions(control),"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":name,"version":"1"}}})
        }
        "skills/list" => {
            json!({"resultType":"complete","skills":skill_entries(control.skill_revision.load(Ordering::SeqCst), control.skill_reference_bytes.load(Ordering::SeqCst)),"ttlMs":0,"cacheScope":"private"})
        }
        "skills/get" => {
            json!({"resultType":"complete","skill":skill_entries(control.skill_revision.load(Ordering::SeqCst), control.skill_reference_bytes.load(Ordering::SeqCst)).into_iter().find(|entry| entry["uri"] == body["params"]["uri"]).unwrap(),"ttlMs":0,"cacheScope":"private"})
        }
        "resources/list" => {
            if body["params"]["cursor"] == "next-page" {
                json!({"resources":[{"uri":"fixture://tickloop-artifacts/details","name":"details","mimeType":"text/plain"}]})
            } else {
                json!({"resources":[{"uri":"fixture://tickloop-artifacts/info","name":"info","mimeType":"text/plain"}],"nextCursor":"next-page"})
            }
        }
        "resources/read"
            if body["params"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.starts_with("fixture://tickloop-artifacts/")) =>
        {
            json!({"contents":[{"uri":body["params"]["uri"],"mimeType":"text/plain","text":"Ordinary MCP resource fixture"}]})
        }
        "resources/read" => {
            let uri = body["params"]["uri"].as_str().unwrap();
            let name = uri.split('/').nth(3).unwrap();
            let text = if control.tamper_skill.load(Ordering::SeqCst) {
                "tampered content".into()
            } else if uri.ends_with("SKILL.md") {
                skill_body(name, control.skill_revision.load(Ordering::SeqCst))
            } else {
                skill_reference(control.skill_reference_bytes.load(Ordering::SeqCst))
            };
            json!({"resultType":"complete","contents":[{"uri":uri,"mimeType":"text/markdown","text":text}],"ttlMs":0,"cacheScope":"private"})
        }
        "tools/list" => tools(control),
        "tools/call" => call_tool(control, origin, &body["params"]),
        method => panic!("unexpected MCP method: {method}"),
    };
    if matches!(body["method"].as_str(), Some("skills/list" | "skills/get")) {
        result["_meta"] = json!({"io.modelcontextprotocol/serverInfo":{"name":name,"version":"1"}});
    }
    json!({"jsonrpc":"2.0","id":body["id"],"result":result})
}

fn skill_body(name: &str, revision: usize) -> String {
    format!(
        "---\nname: {name}\ndescription: Fixture guidance for {name}\n---\nPage guidance fixture for {name}: verify the facts and preserve existing content.\nManifest revision: {revision}\n"
    )
}

fn skill_reference(extra_bytes: usize) -> String {
    let mut content = "Reference fixture: preserve data units".to_owned();
    if extra_bytes > 0 {
        content.push_str(&"x".repeat(extra_bytes));
        content.push_str("REFERENCE_END");
    }
    content
}

fn skill_entries(revision: usize, reference_bytes: usize) -> Vec<Value> {
    ["artifact-design", "artifact-diagramming", "artifact-dataviz", "artifact-draft", "artifact-slides"].into_iter().map(|name| {
        let uri = format!("skill://tickloop-artifacts/{name}/SKILL.md");
        let resources = [(uri.clone(), skill_body(name, revision)), (format!("skill://tickloop-artifacts/{name}/references/runtime.md"), skill_reference(reference_bytes))].into_iter().map(|(uri, text)| json!({"uri":uri,"size":text.len(),"digest":format!("sha256:{:x}", Sha256::digest(text.as_bytes()))})).collect::<Vec<_>>();
        json!({"uri":uri,"frontmatter":{"name":name,"description":format!("Fixture guidance for {name}")},"resources":resources})
    }).collect()
}

/// `POST /api/v1/artifacts/:id/assets?operation_id=…` with the image as the raw body; the same
/// operation returns the receipt it committed.
fn upload(control: &ServiceControl, target: &str, head: &str, body: &[u8]) -> (u16, Value) {
    if let Some((status, code, message)) = control.refuse_uploads.lock().unwrap().clone() {
        return (status, json!({"error":{"code":code,"message":message}}));
    }
    let artifact_id = target.split('/').nth(4).unwrap_or_default().to_owned();
    let operation_id = query(target)
        .get("operation_id")
        .cloned()
        .unwrap_or_default();
    let content_type = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("content-type")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    let mut uploads = control.uploads.lock().unwrap();
    if let Some(existing) = uploads
        .iter()
        .find(|upload| upload.operation_id == operation_id)
    {
        return (201, existing.receipt.clone());
    }
    let asset_id = format!("asset-{}", uploads.len() + 1);
    let stored = if control.strip_upload_metadata.load(Ordering::SeqCst) {
        &body[..body.len().min(8)]
    } else {
        body
    };
    let receipt = json!({"asset_id":asset_id,"ref":format!("asset:{asset_id}"),"content_type":content_type,"bytes":stored.len(),"sha256":format!("{:x}", Sha256::digest(stored)),"width":640,"height":360});
    uploads.push(Upload {
        artifact_id,
        operation_id,
        content_type,
        bytes: body.to_vec(),
        receipt: receipt.clone(),
    });
    (201, receipt)
}

fn wire_service_id(control: &ServiceControl, id: &str) -> String {
    if control.compact_ids.load(Ordering::SeqCst) {
        tickloop_agent_artifacts::short_artifact_id(id)
    } else {
        id.to_owned()
    }
}

fn lifecycle(control: &ServiceControl, method: &str, target: &str, body: &Value) -> (u16, Value) {
    let path = target.split('?').next().unwrap_or("");
    let path = path
        .strip_prefix("/api/v1/agent/feedback/")
        .and_then(|suffix| suffix.split_once('/'))
        .and_then(|(id, action)| {
            tickloop_agent_artifacts::parse_service_id(id)
                .map(|id| format!("/api/v1/agent/feedback/{id}/{action}"))
        })
        .unwrap_or_else(|| path.to_owned());
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
            let previous = links
                .iter()
                .find(|(key, current)| {
                    key.starts_with(&format!("{artifact_id}:"))
                        && current.chat_id == body["chat_id"]
                        && current.loop_id == body["loop_id"]
                })
                .map(|(key, link)| (key.clone(), link.clone()));
            let link = Link {
                binding_id: body["binding_id"].as_str().unwrap().to_owned(),
                client_instance_id: body["client_instance_id"].as_str().unwrap().to_owned(),
                chat_id: body["chat_id"].as_str().unwrap().to_owned(),
                loop_id: body["loop_id"].as_str().unwrap().to_owned(),
                epoch: previous.as_ref().map_or(1, |(_, old)| {
                    old.epoch + u64::from(old.binding_id != body["binding_id"])
                }),
            };
            if let Some((key, _)) = previous {
                links.remove(&key);
            }
            let key = format!("{artifact_id}:{}", link.binding_id);
            if !links.contains_key(&key) && body["history"] == "all" {
                for (artifact, feedback) in control
                    .pending
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(id, _)| id == artifact_id)
                {
                    control.deliveries.lock().unwrap().insert(
                        uuid::Uuid::new_v4().to_string(),
                        (
                            artifact.clone(),
                            feedback.clone(),
                            link.binding_id.clone(),
                            false,
                        ),
                    );
                }
            }
            links.insert(key, link.clone());
            (
                200,
                json!({"artifact_id":artifact_id,"binding_id":link.binding_id,"epoch":link.epoch}),
            )
        }
        ("DELETE", ["", "api", "v1", "agent", "links", artifact_id]) => {
            let key = format!("{artifact_id}:{}", body["binding_id"].as_str().unwrap());
            control.links.lock().unwrap().remove(&key);
            (200, json!({"removed":true}))
        }
        ("GET", ["", "api", "v1", "agent", "feedback"]) => {
            let query = query(target);
            let artifact = query.get("artifact_id").cloned().unwrap_or_default();
            let binding = query.get("binding_id").cloned().unwrap_or_default();
            let current = control
                .links
                .lock()
                .unwrap()
                .get(&format!("{artifact}:{binding}"))
                .cloned();
            if current.is_none_or(|link| Some(&link.epoch.to_string()) != query.get("epoch")) {
                return (
                    409,
                    json!({"error":{"code":"agent_link_missing","message":"link replaced"}}),
                );
            }
            let items = control.deliveries.lock().unwrap().iter().filter(|(_, (id, _, receiver, ack))| *id == artifact && *receiver == binding && !ack)
                .map(|(delivery, (_, feedback, _, _))| json!({"delivery_id":wire_service_id(control, delivery),"feedback_id":wire_service_id(control, feedback),"thread_id":wire_service_id(control, "e1b85a39-a57b-4c28-a8a5-ae9233aead3e"),"created_at":REQUEST_CREATED_AT})).collect::<Vec<_>>();
            (200, json!({"items":items,"next_cursor":null}))
        }
        ("POST", ["", "api", "v1", "agent", "feedback", delivery, "claim"]) => {
            let artifact = body["artifact_id"].as_str().unwrap_or_default();
            let binding = body["binding_id"].as_str().unwrap_or_default();
            let current = control
                .links
                .lock()
                .unwrap()
                .get(&format!("{artifact}:{binding}"))
                .cloned();
            if current.is_none_or(|link| {
                body["epoch"] != link.epoch || body["client_instance_id"] != link.client_instance_id
            }) {
                return (
                    409,
                    json!({"error":{"code":"agent_link_missing","message":"link replaced"}}),
                );
            }
            let deliveries = control.deliveries.lock().unwrap();
            let Some((id, feedback, receiver, ack)) = deliveries.get(*delivery) else {
                return (404, json!({"error":{"code":"not_found"}}));
            };
            if id != artifact || receiver != binding || *ack {
                return (
                    409,
                    json!({"error":{"code":"claim_conflict","reason":"delivered"}}),
                );
            }
            let mut claims = control.claims.lock().unwrap();
            if claims.contains_key(*delivery) {
                return (
                    409,
                    json!({"error":{"code":"claim_conflict","reason":"leased"}}),
                );
            }
            let claim = uuid::Uuid::new_v4().to_string();
            claims.insert((*delivery).to_owned(), claim.clone());
            (
                200,
                json!({"delivery_id":wire_service_id(control, delivery),"feedback_id":wire_service_id(control, feedback),"claim_id":claim,"fencing_token":format!("fence-{delivery}"),"lease_expires_at":"2099-01-01T00:00:00Z","input":{"artifact_id":artifact,"thread_id":"thread-1","text":"@tickloop tighten the introduction","anchor":thread_anchor(),"anchor_description":ANCHOR_DESCRIPTION,"requested_by":"One","access_role":"owner","messages":[]}}),
            )
        }
        (
            "POST",
            [
                "",
                "api",
                "v1",
                "agent",
                "feedback",
                delivery,
                action @ ("ack" | "release"),
            ],
        ) => {
            if control
                .claims
                .lock()
                .unwrap()
                .get(*delivery)
                .is_none_or(|claim| body["claim_id"] != *claim)
                || body["fencing_token"] != format!("fence-{delivery}")
            {
                return (409, json!({"error":{"code":"claim_expired"}}));
            }
            if *action == "release" {
                control.claims.lock().unwrap().remove(*delivery);
                control
                    .releases
                    .lock()
                    .unwrap()
                    .push(((*delivery).to_owned(), "released".into()));
                return (200, json!({"delivery_id":delivery,"released":true}));
            }
            if let Some((_, feedback, _, ack)) =
                control.deliveries.lock().unwrap().get_mut(*delivery)
            {
                if !*ack {
                    control
                        .finished
                        .lock()
                        .unwrap()
                        .push((feedback.clone(), "received".into()));
                }
                *ack = true;
            }
            (
                200,
                json!({"delivery_id":wire_service_id(control, delivery),"accepted":true}),
            )
        }
        ("GET", ["", "api", "v1", "artifacts", artifact_id]) => {
            let links = control.links.lock().unwrap().iter().filter(|(key, _)| key.starts_with(&format!("{artifact_id}:"))).map(|(_, link)| json!({"binding_id":link.binding_id,"chat_id":link.chat_id,"loop_id":link.loop_id,"epoch":link.epoch})).collect::<Vec<_>>();
            let canonical_id = control
                .read_result
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|result| result["structuredContent"]["artifact_id"].as_str())
                .filter(|id| tickloop_agent_artifacts::artifact_ids_match(artifact_id, id))
                .unwrap_or(artifact_id)
                .to_owned();
            (
                200,
                json!({"id":canonical_id,"title":"Fixture","format":control.metadata_format.lock().unwrap().as_deref().unwrap_or("markdown"),"access_role":"owner","publication":control.publication.lock().unwrap().clone(),"agent_links":links}),
            )
        }
        (
            "GET",
            [
                "",
                "api",
                "v1",
                "artifacts",
                artifact_id,
                "operations",
                operation_id,
            ],
        ) => {
            let upload = control
                .uploads
                .lock()
                .unwrap()
                .iter()
                .find(|upload| {
                    upload.artifact_id == *artifact_id && upload.operation_id == *operation_id
                })
                .cloned();
            match upload {
                Some(upload) if !control.forget_upload_operations.load(Ordering::SeqCst) => (
                    200,
                    json!({"operation_id":operation_id,"status":"committed","result":upload.receipt}),
                ),
                _ => (
                    404,
                    json!({"error":{"code":"not_found","message":"Operation not found"}}),
                ),
            }
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
                    let raw_target = parts.next().unwrap_or("");
                    let target = raw_target
                        .strip_prefix(APP_BASE_PATH)
                        .unwrap_or(raw_target)
                        .to_owned();
                    let body: Value =
                        serde_json::from_slice(&bytes[header_end + 4..]).unwrap_or(Value::Null);
                    control
                        .requests
                        .lock()
                        .unwrap()
                        .push(json!({"path":line,"headers":head,"body":body}));
                    let is_mcp = method == "POST" && target == "/mcp";
                    let reject_tool = is_mcp
                        && body["method"] == "tools/call"
                        && control
                            .refuse_tool_bearer
                            .lock()
                            .unwrap()
                            .as_ref()
                            .is_some_and(|token| {
                                head.to_ascii_lowercase().contains(&format!(
                                    "authorization: bearer {}",
                                    token.to_ascii_lowercase()
                                ))
                            });
                    if reject_tool {
                        let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    }
                    if is_mcp && body.get("id").is_none() {
                        let _ = stream
                            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let is_upload = method == "POST"
                        && target.starts_with("/api/v1/artifacts/")
                        && target
                            .split('?')
                            .next()
                            .unwrap_or_default()
                            .ends_with("/assets");
                    let (mut status, response) = if is_upload {
                        let (status, receipt) =
                            upload(&control, &target, &head, &bytes[header_end + 4..]);
                        if status == 201 && control.drop_upload_response.load(Ordering::SeqCst) {
                            return;
                        }
                        if status == 201 && control.hold_upload_response.load(Ordering::SeqCst) {
                            control.upload_held.notify_one();
                            std::future::pending::<()>().await;
                        }
                        (status, receipt)
                    } else if target == "/.well-known/oauth-authorization-server/account" {
                        (
                            200,
                            json!({"issuer":format!("{origin}/account"),"token_endpoint":format!("{origin}/account/token"),"device_authorization_endpoint":format!("{origin}/account/device"),"userinfo_endpoint":format!("{origin}/account/userinfo"),"revocation_endpoint":format!("{origin}/account/revoke"),"grant_types_supported":["urn:ietf:params:oauth:grant-type:device_code"]}),
                        )
                    } else if target == "/account/device" {
                        (
                            200,
                            json!({"device_code":"test-device","user_code":"TEST","verification_uri":format!("{origin}/account/device?code=TEST"),"expires_in":30,"interval":1}),
                        )
                    } else if target == "/account/token"
                        && control.hold_device_approval.load(Ordering::SeqCst)
                    {
                        (400, json!({"error":"authorization_pending"}))
                    } else if target == "/account/token"
                        && control.refuse_token_refresh.load(Ordering::SeqCst)
                    {
                        (400, json!({"error":"invalid_grant"}))
                    } else if target == "/account/token" {
                        control.signed_out.store(false, Ordering::SeqCst);
                        let revision = control.credential_revision.load(Ordering::SeqCst);
                        (
                            200,
                            json!({"access_token":format!("test-service-token-{revision}"),"token_type":"Bearer","expires_in":600,"refresh_token":format!("refresh-{revision}")}),
                        )
                    } else if target == "/account/userinfo" {
                        if control.hold_status_response.load(Ordering::SeqCst) {
                            control.status_response_started.notify_one();
                            control.release_status_response.notified().await;
                        }
                        (
                            200,
                            json!({"sub":account(&control),"name":"One","email":"one@example.test"}),
                        )
                    } else if target == "/account/revoke" {
                        control.signed_out.store(true, Ordering::SeqCst);
                        (200, json!({"revoked":true}))
                    } else if let Some(error) =
                        is_mcp.then(|| header_mismatch(&head, &body)).flatten()
                    {
                        (400, error)
                    } else if is_mcp {
                        if matches!(
                            body["method"].as_str(),
                            Some("resources/read" | "resources/list")
                        ) && control.hold_skill_response.load(Ordering::SeqCst)
                        {
                            control.skill_response_started.notify_one();
                            control.release_skill_response.notified().await;
                        }
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
                            && (is_mcp || target == "/account/userinfo"))
                    {
                        status = 401;
                    } else if (is_mcp && control.directory_unavailable.load(Ordering::SeqCst))
                        || (target == "/account/userinfo"
                            && control.session_unavailable.load(Ordering::SeqCst))
                    {
                        status = 503;
                    }
                    if target == "/account/userinfo"
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

    /// Device authorizations the service has started.
    pub fn device_logins_started(&self) -> usize {
        self.lifecycle_requests("POST /account/device ").len()
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
        for (key, link) in self
            .control
            .links
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{artifact_id}:")))
        {
            let _ = key;
            self.control.deliveries.lock().unwrap().insert(
                uuid::Uuid::new_v4().to_string(),
                (
                    artifact_id.into(),
                    request_id.into(),
                    link.binding_id.clone(),
                    false,
                ),
            );
        }
    }

    pub fn link(&self, artifact_id: &str) -> Option<Link> {
        self.control
            .links
            .lock()
            .unwrap()
            .iter()
            .find(|(key, _)| key.starts_with(&format!("{artifact_id}:")))
            .map(|(_, link)| link.clone())
    }

    /// Another Chat of the same account takes over the artifact's link.
    pub fn replace_link(&self, artifact_id: &str) {
        let mut links = self.control.links.lock().unwrap();
        let link = links
            .iter_mut()
            .find(|(key, _)| key.starts_with(&format!("{artifact_id}:")))
            .unwrap()
            .1;
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
            && !self
                .control
                .finished
                .lock()
                .unwrap()
                .iter()
                .any(|(id, _)| id == request_id)
    }

    pub fn releases(&self) -> Vec<(String, String)> {
        self.control.releases.lock().unwrap().clone()
    }

    pub fn finished(&self) -> Vec<(String, String)> {
        self.control.finished.lock().unwrap().clone()
    }

    pub fn uploads(&self) -> Vec<Upload> {
        self.control.uploads.lock().unwrap().clone()
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
