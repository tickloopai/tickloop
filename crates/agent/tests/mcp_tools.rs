use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent::{
    agent_access::{
        AgentAccess, AgentCommand, CommandEnvelope, CommandResult, EmbeddedAgentAccess, LaunchSpec,
        LocalAgentClient, LoopAction, serve_listener,
    },
    artifacts::ArtifactClient,
    host_tools::{
        EventLeaseOutcome, EventLeaseRequest, EventSourceLocator, HostCallContext,
        HostEndpointIdentity, HostToolPermission, HostToolProvider, PreparedHostCall,
        WorkspaceHostContext, WorkspaceHostName, WorkspaceIntegration,
    },
    mcp_tools::McpHostToolProvider,
    model::{
        Content, ModelClient, ModelDelta, ModelError, ModelIdentity, ModelOutput, ModelRequest,
    },
    permissions::ExecutionMode,
    protocol::ToolDefinition,
    runtime::{Agent, EventKind, LoopState, OperationState},
    tools::ToolResult,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixListener},
};
use tokio_util::sync::CancellationToken;

mod support;
use support::{ScriptedModel, call, config, text};

#[derive(Clone, Copy)]
enum PartialAssociationFailure {
    Rejected,
    Unknown,
}

#[derive(Default)]
struct ServiceControl {
    hold_session_response: AtomicBool,
    session_response_started: tokio::sync::Notify,
    release_session_response: tokio::sync::Notify,
    active: AtomicBool,
    reject_mcp: AtomicBool,
    directory_unavailable: AtomicBool,
    session_unavailable: AtomicBool,
    other_account: AtomicBool,
    drop_call_response: AtomicBool,
    drop_association_response: AtomicBool,
    hold_association_response: AtomicBool,
    association_response_started: tokio::sync::Notify,
    release_association_response: tokio::sync::Notify,
    hold_pending_response: AtomicBool,
    pending_response_started: tokio::sync::Notify,
    release_pending_response: tokio::sync::Notify,
    fail_association_after_publish: AtomicBool,
    stale_next_directory: AtomicBool,
    bridge_enabled: AtomicBool,
    creation_enabled: AtomicBool,
    resource_directory_enabled: AtomicBool,
    attachment_revision: AtomicUsize,
    reject_creation: AtomicBool,
    unconfirmed_creation: AtomicBool,
    wrong_creation_operation: AtomicBool,
    metadata_enabled: AtomicBool,
    feedback_pending: AtomicBool,
    feedback_claimed: AtomicBool,
    feedback_completed: AtomicBool,
    recovered_feedback_event_id: Mutex<Option<uuid::Uuid>>,
    next_feedback_event: AtomicBool,
    second_feedback_same_epoch: AtomicBool,
    paged_feedback: AtomicBool,
    fail_renew: AtomicBool,
    renew_error: Mutex<Option<&'static str>>,
    terminal_renew_override: Mutex<Option<(String, Value)>>,
    delay_renew_response: AtomicBool,
    renew_response_started: tokio::sync::Notify,
    finish_renew_response: tokio::sync::Notify,
    publication_revision: AtomicUsize,
    second_publication_new_source: AtomicBool,
    directory_revision: AtomicUsize,
    connection_enabled: AtomicBool,
    association: Mutex<Option<Value>>,
    association_receipts: Mutex<std::collections::BTreeMap<String, Value>>,
    partial_association_failure: Mutex<Option<PartialAssociationFailure>>,
    unknown_association_operation_id: Mutex<Option<String>>,
    hide_association_receipts: AtomicBool,
    bridge_revision: AtomicUsize,
    credential_revision: AtomicUsize,
    transcript_root: Mutex<Option<std::path::PathBuf>>,
    association_disk_snapshot: Mutex<Option<String>>,
    hold_connection_check_response: AtomicBool,
    connection_check_response_started: tokio::sync::Notify,
    release_connection_check_response: tokio::sync::Notify,
}

async fn service() -> (String, Arc<Mutex<Vec<Value>>>, Arc<ServiceControl>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let service_origin = origin.clone();
    let control = Arc::new(ServiceControl::default());
    control.active.store(true, Ordering::SeqCst);
    let server_control = control.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let observed = observed.clone();
            let origin = service_origin.clone();
            let control = server_control.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let size = stream.read(&mut buffer).await.unwrap();
                    if size == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..size]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .and_then(|length| length.parse::<usize>().ok())
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let end = bytes
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .unwrap();
                let head = String::from_utf8_lossy(&bytes[..end]).to_string();
                let path = head.lines().next().unwrap_or("");
                let body: Value = serde_json::from_slice(&bytes[end + 4..]).unwrap_or(Value::Null);
                observed
                    .lock()
                    .unwrap()
                    .push(json!({"path":path,"headers":head,"body":body}));
                let account_id = if control.other_account.load(Ordering::SeqCst) {
                    "user-two"
                } else {
                    "user-one"
                };
                let response = if path.starts_with("POST /api/v1/auth/device ") {
                    json!({"device_code":"test-device","user_code":"TEST","verification_uri":format!("{origin}/device?code=TEST"),"expires_in":30,"interval":1})
                } else if path.starts_with("POST /api/v1/auth/device/token ") {
                    let revision = control.credential_revision.load(Ordering::SeqCst);
                    let token = if revision == 0 {
                        "test-service-token".into()
                    } else {
                        format!("test-service-token-{revision}")
                    };
                    json!({"access_token":token,"token_type":"Bearer","expires_in":60,"user":{"id":account_id,"name":"One","email":"one@example.test"}})
                } else if path.starts_with("GET /api/v1/auth/session ") {
                    json!({"user":{"id":account_id,"name":"One","email":"one@example.test"},"expires_at":"2099-01-01T00:00:00Z"})
                } else if path.starts_with("POST /api/v1/auth/logout ") {
                    control.active.store(false, Ordering::SeqCst);
                    json!({"revoked":true})
                } else if path.starts_with("GET /api/v1/artifacts/")
                    && control.metadata_enabled.load(Ordering::SeqCst)
                {
                    json!({"id":"source-one","title":"REST title","format":"markdown","access_role":"owner","url":format!("{origin}/a/source-one")})
                } else if path.starts_with("POST /mcp ") {
                    let id = body["id"].clone();
                    let result = match body["method"].as_str().unwrap_or("") {
                        "server/discover" => {
                            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":if control.creation_enabled.load(Ordering::SeqCst) || control.resource_directory_enabled.load(Ordering::SeqCst) {json!({"tools":{},"resources":{}})}else{json!({"tools":{}})},"ttlMs":0,"cacheScope":"private"})
                        }
                        "tools/list" => {
                            let mut tools = vec![
                                json!({"name":"artifact_publish","description":format!("Publish source revision {}", control.directory_revision.load(Ordering::SeqCst)),"inputSchema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"operation_id":{"type":"string","minLength":1},"source":{"type":"string","maxLength":1000000}},"required":["operation_id","source"],"additionalProperties":false}}),
                            ];
                            if control.bridge_enabled.load(Ordering::SeqCst) {
                                let stale =
                                    control.stale_next_directory.swap(false, Ordering::SeqCst);
                                tools.extend(["associate", "pending", "claim", "renew", "release", "operation"].map(|role| json!({"name":format!("event_{role}"),"inputSchema":if role == "associate" && stale { json!({"type":"object","additionalProperties":false}) } else { json!({"type":"object"}) }})));
                                tools.push(json!({"name":"artifact_feedback_reply","inputSchema":{"type":"object","properties":{"source_key":{"type":"string"},"operation_id":{"type":"string"},"text":{"type":"string"}},"required":["source_key","operation_id","text"],"additionalProperties":false}}));
                                tools.push(json!({"name":"artifact_feedback_complete","inputSchema":{"type":"object","properties":{"source_key":{"type":"string"},"operation_id":{"type":"string"},"reply_operation_id":{"type":"string"}},"required":["source_key","operation_id","reply_operation_id"],"additionalProperties":false}}));
                                if control.connection_enabled.load(Ordering::SeqCst) {
                                    tools.push(json!({"name":"artifact_edit_publish","inputSchema":{"type":"object","properties":{"artifact_id":{"type":"string"},"operation_id":{"type":"string"}},"required":["artifact_id","operation_id"],"additionalProperties":false}}));
                                    tools.push(json!({"name":"artifact_agent_connect","inputSchema":{"type":"object","properties":{"artifact_id":{"type":"string"},"operation_id":{"type":"string"},"action":{"type":"string","enum":["connect","check"]},"replace_existing":{"type":"boolean"}},"required":["artifact_id","operation_id"],"additionalProperties":false}}));
                                }
                            }
                            tools.push(json!({"name":"create_custom_document","inputSchema":{"type":"object","properties":{"operation_id":{"type":"string"},"title":{"type":"string"},"format":{"type":"string"}}}}));
                            json!({"tools":tools})
                        }
                        "resources/list" => {
                            json!({"resources":if control.creation_enabled.load(Ordering::SeqCst){json!([{"uri":"tickloop://artifact-tick-attachments/v1","name":"attachments"}])}else{json!([])}})
                        }
                        "resources/read"
                            if body["params"]["uri"]
                                == "tickloop://artifact-tick-attachments/v1" =>
                        {
                            let mut descriptor = json!({"version":1,"creation_tools":[{"tool":"create_custom_document","operation_id_argument":"operation_id","artifact_id_result":"artifact_id","title_argument":"title","format_argument":"format","url_result":"url"}]});
                            if control.attachment_revision.load(Ordering::SeqCst) != 0 {
                                descriptor["creation_tools"][0]["url_result"] = json!("public_url");
                            }
                            json!({"contents":[{"uri":"tickloop://artifact-tick-attachments/v1","mimeType":"application/json","text":descriptor.to_string()}]})
                        }
                        "resources/read" => {
                            let mut descriptor = json!({"version":1,"publication_tools":[{"tool":"artifact_publish","operation_id_argument":"operation_id","source_key_result":"artifact_id","version_key_result":"version_id"}],"tools":{"associate":"event_associate","pending":"event_pending","claim":"event_claim","renew":"event_renew","release":"event_release","operation":"event_operation"}});
                            let uri = if control.connection_enabled.load(Ordering::SeqCst) {
                                descriptor["version"] = json!(2);
                                descriptor["publication_tools"].as_array_mut().unwrap().push(json!({"tool":"artifact_edit_publish","operation_id_argument":"operation_id","source_key_result":"artifact_id","version_key_result":"version_id"}));
                                descriptor["connection_tool"] = json!({"tool":"artifact_agent_connect","source_key_argument":"artifact_id","operation_id_argument":"operation_id","action_argument":"action"});
                                if control.bridge_revision.load(Ordering::SeqCst) != 0 {
                                    descriptor["publication_tools"][0]["version_key_result"] =
                                        json!("release_id");
                                }
                                "tickloop://event-bridge/v2"
                            } else {
                                "tickloop://event-bridge/v1"
                            };
                            json!({"contents":[{"uri":uri,"mimeType":"application/json","text":descriptor.to_string()}]})
                        }
                        "tools/call" => {
                            if body["params"]["name"] == "create_custom_document" {
                                json!({"content":[],"isError":control.reject_creation.load(Ordering::SeqCst),"structuredContent":{"status":if control.unconfirmed_creation.load(Ordering::SeqCst){"unknown"}else{"committed"},"operation_id":if control.wrong_creation_operation.load(Ordering::SeqCst){json!("wrong-operation")}else{body["params"]["arguments"]["operation_id"].clone()},"artifact_id":"source-one","url":format!("{origin}/a/source-one")}})
                            } else if body["params"]["name"] == "artifact_agent_connect" {
                                let arguments = &body["params"]["arguments"];
                                let action = arguments["action"].as_str().unwrap_or("connect");
                                let operation = if control
                                    .hide_association_receipts
                                    .load(Ordering::SeqCst)
                                {
                                    None
                                } else {
                                    control.association_receipts.lock().unwrap().get(arguments["operation_id"].as_str().unwrap()).map(|receipt| json!({"status":receipt["result"]["status"],"source_key":receipt["result"]["source_key"],"association_epoch":receipt["result"]["association_epoch"]}))
                                };
                                json!({"content":[],"structuredContent":{"artifact_id":arguments["artifact_id"],"operation_id":arguments["operation_id"],"connection_status":if action == "check" {"checked"} else {"prepared"}},"_meta":{"tickloop.ai/source-connect/v1":{"version":1,"action":action,"source_key":arguments["artifact_id"],"operation_id":arguments["operation_id"],"head_version_id":null,"current_association":control.association.lock().unwrap().clone(),"association_operation":operation}}})
                            } else if body["params"]["name"] == "event_associate"
                                && control.connection_enabled.load(Ordering::SeqCst)
                            {
                                if let Some(root) = control.transcript_root.lock().unwrap().as_ref()
                                {
                                    fn read_transcripts(
                                        path: &std::path::Path,
                                        output: &mut String,
                                    ) {
                                        if path
                                            .extension()
                                            .is_some_and(|extension| extension == "jsonl")
                                        {
                                            output.push_str(
                                                &std::fs::read_to_string(path).unwrap_or_default(),
                                            );
                                        } else if let Ok(entries) = std::fs::read_dir(path) {
                                            for entry in entries.flatten() {
                                                read_transcripts(&entry.path(), output);
                                            }
                                        }
                                    }
                                    let mut saved = String::new();
                                    read_transcripts(root, &mut saved);
                                    *control.association_disk_snapshot.lock().unwrap() =
                                        Some(saved);
                                }
                                let arguments = &body["params"]["arguments"];
                                let operation_id = arguments["operation_id"].as_str().unwrap();
                                let mut receipts = control.association_receipts.lock().unwrap();
                                let mut current = control.association.lock().unwrap();
                                let data = if let Some(previous) = receipts.get(operation_id) {
                                    if previous["arguments"] == *arguments {
                                        previous["result"].clone()
                                    } else {
                                        json!({"error":{"code":"operation_conflict"}})
                                    }
                                } else if arguments["expected_epoch"].as_u64()
                                    != current.as_ref().and_then(|association| {
                                        association["association_epoch"].as_u64()
                                    })
                                {
                                    json!({"error":{"code":"association_conflict"}})
                                } else {
                                    let epoch = current
                                        .as_ref()
                                        .and_then(|association| {
                                            association["association_epoch"].as_u64()
                                        })
                                        .unwrap_or(0)
                                        + 1;
                                    *current = Some(
                                        json!({"binding_id":arguments["binding_id"],"client_instance_id":arguments["client_instance_id"],"chat_id":arguments["chat_id"],"loop_id":arguments["loop_id"],"association_epoch":epoch}),
                                    );
                                    let result = json!({"status":"committed","operation_id":operation_id,"source_key":arguments["source_key"],"association_epoch":epoch});
                                    receipts.insert(
                                        operation_id.into(),
                                        json!({"arguments":arguments,"result":result}),
                                    );
                                    result
                                };
                                json!({"content":[],"isError":data.get("error").is_some(),"structuredContent":data})
                            } else if body["params"]["name"] == "event_associate" {
                                let arguments = &body["params"]["arguments"];
                                let failure = if arguments["source_key"] == "source-two"
                                    && arguments["expected_epoch"] == 1
                                {
                                    control.partial_association_failure.lock().unwrap().take()
                                } else {
                                    None
                                };
                                if let Some(failure) = failure {
                                    let code = match failure {
                                        PartialAssociationFailure::Rejected => {
                                            "association_conflict"
                                        }
                                        PartialAssociationFailure::Unknown => {
                                            *control
                                                .unknown_association_operation_id
                                                .lock()
                                                .unwrap() = arguments["operation_id"]
                                                .as_str()
                                                .map(str::to_owned);
                                            "temporary_failure"
                                        }
                                    };
                                    json!({"content":[],"isError":true,"structuredContent":{"error":{"code":code}}})
                                } else {
                                    let epoch = arguments["expected_epoch"]
                                        .as_u64()
                                        .map_or(1, |epoch| epoch + 1);
                                    json!({"content":[],"structuredContent":{"status":"committed","operation_id":arguments["operation_id"],"source_key":arguments["source_key"],"association_epoch":epoch}})
                                }
                            } else if body["params"]["name"] == "event_operation" {
                                if control
                                    .unknown_association_operation_id
                                    .lock()
                                    .unwrap()
                                    .as_deref()
                                    .is_some_and(|operation_id| {
                                        body["params"]["arguments"]["operation_id"].as_str()
                                            == Some(operation_id)
                                    })
                                {
                                    json!({"content":[],"structuredContent":{"status":"unknown","operation_id":body["params"]["arguments"]["operation_id"]}})
                                } else if control.connection_enabled.load(Ordering::SeqCst) {
                                    let data =
                                        if control.hide_association_receipts.load(Ordering::SeqCst)
                                        {
                                            None
                                        } else {
                                            control
                                                .association_receipts
                                                .lock()
                                                .unwrap()
                                                .get(
                                                    body["params"]["arguments"]["operation_id"]
                                                        .as_str()
                                                        .unwrap(),
                                                )
                                                .map(|receipt| receipt["result"].clone())
                                        };
                                    json!({"content":[],"structuredContent":{"status":if data.is_some() {"committed"} else {"unknown"},"operation_id":body["params"]["arguments"]["operation_id"],"result":data}})
                                } else {
                                    json!({"content":[],"structuredContent":{"status":"committed","operation_id":body["params"]["arguments"]["operation_id"],"result":{"source_key":body["params"]["arguments"]["source_key"],"association_epoch":1}}})
                                }
                            } else if body["params"]["name"] == "event_pending" {
                                let paged = control.paged_feedback.load(Ordering::SeqCst);
                                let cursor = body["params"]["arguments"]["cursor"].as_str();
                                let events = if paged && cursor.is_none() {
                                    (0..30).map(|index| json!({"source_key":"source-one","association_epoch":1,"event_id":format!("blocked-{index}"),"kind":"pending"})).collect()
                                } else if paged && cursor == Some("page-one") {
                                    vec![
                                        json!({"source_key":"source-one","association_epoch":1,"event_id":"event-after-page","kind":"pending"}),
                                    ]
                                } else if control.feedback_pending.load(Ordering::SeqCst)
                                    && !control.feedback_claimed.load(Ordering::SeqCst)
                                {
                                    let recovered_event_id =
                                        *control.recovered_feedback_event_id.lock().unwrap();
                                    let event_id = if let Some(event_id) = recovered_event_id {
                                        event_id.to_string()
                                    } else if control.next_feedback_event.load(Ordering::SeqCst) {
                                        "event-two".to_owned()
                                    } else {
                                        "event-one".to_owned()
                                    };
                                    let epoch = if recovered_event_id.is_some()
                                        || (control.next_feedback_event.load(Ordering::SeqCst)
                                            && !control
                                                .second_feedback_same_epoch
                                                .load(Ordering::SeqCst))
                                    {
                                        2
                                    } else {
                                        1
                                    };
                                    vec![
                                        json!({"source_key":"source-one","association_epoch":epoch,"event_id":event_id,"kind":"pending"}),
                                    ]
                                } else {
                                    Vec::new()
                                };
                                json!({"content":[],"structuredContent":{"events":events,"next_cursor":if paged && cursor.is_none() { Some("page-one") } else { None }}})
                            } else if body["params"]["name"] == "event_claim" {
                                if control.paged_feedback.load(Ordering::SeqCst)
                                    && body["params"]["arguments"]["event_id"]
                                        .as_str()
                                        .is_some_and(|id| id.starts_with("blocked-"))
                                {
                                    json!({"content":[],"structuredContent":{"status":"committed","operation_id":body["params"]["arguments"]["operation_id"]}})
                                } else {
                                    control.feedback_claimed.store(true, Ordering::SeqCst);
                                    control.feedback_completed.store(false, Ordering::SeqCst);
                                    let is_recovery = control
                                        .recovered_feedback_event_id
                                        .lock()
                                        .unwrap()
                                        .is_some_and(|event_id| {
                                            body["params"]["arguments"]["event_id"]
                                                == event_id.to_string()
                                        });
                                    let mut input = json!({"text":"Please update the source","author":"Owner","comment_id":"comment-one","version_id":"version-one","quote":"# Source","source_key":"source-one"});
                                    if is_recovery {
                                        input["prior_progress"] = json!({"publish_operation_id":"op-partial","version_id":"version-two"});
                                    }
                                    json!({"content":[],"structuredContent":{"status":"committed","operation_id":body["params"]["arguments"]["operation_id"],"claim":{"event_id":body["params"]["arguments"]["event_id"],"claim_id":if is_recovery {"claim-recovered"} else {"claim-one"},"fencing_token":if is_recovery {"secret-fence-recovered"} else {"secret-fence"},"lease_expires_at":"2099-01-01T00:00:00Z","input":input}}})
                                }
                            } else if body["params"]["name"] == "event_release" {
                                if control.feedback_completed.load(Ordering::SeqCst) {
                                    json!({"content":[],"structuredContent":{"status":"committed","operation_id":body["params"]["arguments"]["operation_id"],"claim_status":"completed","source_key":body["params"]["arguments"]["source_key"],"binding_id":body["params"]["arguments"]["binding_id"],"association_epoch":body["params"]["arguments"]["association_epoch"],"event_id":body["params"]["arguments"]["event_id"],"claim_id":body["params"]["arguments"]["claim_id"]}})
                                } else {
                                    json!({"content":[],"structuredContent":{"status":"committed","operation_id":body["params"]["arguments"]["operation_id"],"event_id":body["params"]["arguments"]["event_id"]}})
                                }
                            } else if body["params"]["name"] == "event_renew" {
                                if control.feedback_completed.load(Ordering::SeqCst) {
                                    let mut data = json!({"status":"committed","operation_id":body["params"]["arguments"]["operation_id"],"claim_status":"completed","source_key":body["params"]["arguments"]["source_key"],"binding_id":body["params"]["arguments"]["binding_id"],"association_epoch":body["params"]["arguments"]["association_epoch"],"event_id":body["params"]["arguments"]["event_id"],"claim_id":body["params"]["arguments"]["claim_id"]});
                                    if let Some((key, value)) =
                                        &*control.terminal_renew_override.lock().unwrap()
                                    {
                                        data[key] = value.clone();
                                    }
                                    json!({"content":[],"structuredContent":data})
                                } else if let Some(code) = *control.renew_error.lock().unwrap() {
                                    json!({"content":[],"isError":true,"structuredContent":{"error":{"code":code}}})
                                } else if control.fail_renew.load(Ordering::SeqCst) {
                                    json!({"content":[],"structuredContent":{"status":"unknown","operation_id":body["params"]["arguments"]["operation_id"]}})
                                } else {
                                    json!({"content":[],"structuredContent":{"status":"committed","operation_id":body["params"]["arguments"]["operation_id"],"lease_expires_at":"2100-01-01T00:00:00Z"}})
                                }
                            } else if body["params"]["name"] == "artifact_feedback_reply" {
                                json!({"content":[{"type":"text","text":"Reply recorded"}],"structuredContent":{"status":"committed","operation_id":body["params"]["arguments"]["operation_id"],"reply_id":"reply-one"}})
                            } else if body["params"]["name"] == "artifact_feedback_complete" {
                                control.feedback_completed.store(true, Ordering::SeqCst);
                                json!({"content":[],"structuredContent":{"status":"committed","operation_id":body["params"]["arguments"]["operation_id"],"feedback_status":"replied"}})
                            } else if body["params"]["name"] == "artifact_publish"
                                && control.feedback_completed.load(Ordering::SeqCst)
                                && !body["params"]["_meta"]["tickloop.ai/event-bridge"].is_null()
                            {
                                json!({"content":[{"type":"text","text":"Completed claim cannot mutate"}],"isError":true})
                            } else if control.bridge_enabled.load(Ordering::SeqCst) {
                                let revision =
                                    control.publication_revision.fetch_add(1, Ordering::SeqCst) + 1;
                                if control
                                    .fail_association_after_publish
                                    .swap(false, Ordering::SeqCst)
                                {
                                    control.stale_next_directory.store(true, Ordering::SeqCst);
                                }
                                let version_id = match revision {
                                    1 => "version-one".to_owned(),
                                    2 => "version-two".to_owned(),
                                    _ => format!("version-{revision}"),
                                };
                                let source_key = if revision == 2
                                    && control.second_publication_new_source.load(Ordering::SeqCst)
                                {
                                    "source-two"
                                } else {
                                    "source-one"
                                };
                                json!({"content":[{"type":"text","text":"Published https://docs.tickloop.ai/a/test"}],"structuredContent":{"status":"committed","url":"https://docs.tickloop.ai/a/test","operation_id":body["params"]["arguments"]["operation_id"],"artifact_id":source_key,"version_id":version_id}})
                            } else {
                                json!({"content":[{"type":"text","text":"Published https://docs.tickloop.ai/a/test"}],"structuredContent":{"url":"https://docs.tickloop.ai/a/test","operation_id":"op-one"}})
                            }
                        }
                        other => panic!("unexpected MCP method: {other}"),
                    };
                    json!({"jsonrpc":"2.0","id":id,"result":result})
                } else {
                    json!({"error":"not_found"})
                };
                if body["method"] == "tools/call"
                    && (control.drop_call_response.load(Ordering::SeqCst)
                        || (body["params"]["name"] == "event_associate"
                            && control.drop_association_response.load(Ordering::SeqCst)))
                {
                    return;
                }
                if body["params"]["name"] == "event_renew"
                    && control.delay_renew_response.load(Ordering::SeqCst)
                {
                    control.renew_response_started.notify_one();
                    control.finish_renew_response.notified().await;
                }
                if body["method"] == "tools/call"
                    && body["params"]["name"] == "event_associate"
                    && control
                        .hold_association_response
                        .swap(false, Ordering::SeqCst)
                {
                    control.association_response_started.notify_one();
                    control.release_association_response.notified().await;
                }
                if body["method"] == "tools/call"
                    && body["params"]["name"] == "event_pending"
                    && control.hold_pending_response.swap(false, Ordering::SeqCst)
                {
                    control.pending_response_started.notify_one();
                    control.release_pending_response.notified().await;
                }
                if body["method"] == "tools/call"
                    && body["params"]["name"] == "artifact_agent_connect"
                    && body["params"]["arguments"]["action"] == "check"
                    && control
                        .hold_connection_check_response
                        .swap(false, Ordering::SeqCst)
                {
                    control.connection_check_response_started.notify_one();
                    control.release_connection_check_response.notified().await;
                }
                let status = if (control.reject_mcp.load(Ordering::SeqCst)
                    && path.starts_with("POST /mcp "))
                    || (!control.active.load(Ordering::SeqCst)
                        && (path.starts_with("POST /mcp ")
                            || path.starts_with("GET /api/v1/auth/session ")))
                {
                    401
                } else if control.directory_unavailable.load(Ordering::SeqCst)
                    && path.starts_with("POST /mcp ")
                    || control.session_unavailable.load(Ordering::SeqCst)
                        && path.starts_with("GET /api/v1/auth/session ")
                {
                    503
                } else {
                    200
                };
                if path.starts_with("GET /api/v1/auth/session ")
                    && control.hold_session_response.swap(false, Ordering::SeqCst)
                {
                    control.session_response_started.notify_one();
                    control.release_session_response.notified().await;
                }
                let body = response.to_string();
                let message = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(message.as_bytes()).await.unwrap();
            });
        }
    });
    (origin, requests, control)
}

#[tokio::test]
async fn confirmed_publication_records_effect_on_current_loop_receipt() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.metadata_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let transcript_root = directory.path().join("transcripts");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-confirmed","source":"# Source"}),
        ),
        text("Published"),
    ]));
    let agent = Agent::open_embedded(transcript_root.clone(), config(), model).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        permission
            .approval_reason
            .as_deref()
            .unwrap_or_default()
            .contains("associate a newly introduced source with the current Loop")
    );
    assert!(
        permission
            .approval_reason
            .as_deref()
            .unwrap_or_default()
            .contains("preserve any existing explicit connection and its listening state")
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks.len(), 1);
    assert_eq!(snapshot.artifact_ticks[0].title, "REST title");
    assert_eq!(snapshot.artifact_ticks[0].origin, origin);
    let goal = snapshot.loops.last().unwrap();
    assert_eq!(goal.event_sources.len(), 1);
    let source = &goal.event_sources[0];
    assert_eq!(source.publication.as_ref().unwrap().receipt.chat_id, chat);
    assert_eq!(
        source.publication.as_ref().unwrap().receipt.loop_id,
        goal.id
    );
    assert_eq!(
        source.publication.as_ref().unwrap().receipt.run_id,
        goal.run_id.unwrap()
    );
    assert_eq!(source.source_key, "source-one");
    assert_eq!(
        source.publication.as_ref().unwrap().version_id,
        "version-one"
    );
    assert_eq!(
        source.publication.as_ref().unwrap().operation_id,
        "op-confirmed"
    );
    assert!(!source.association_operation_id.is_empty());
    assert_eq!(source.association_epoch, Some(1));
    assert!(!source.resume_required);
    let calls: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["body"]["method"] == "tools/call")
        .cloned()
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["body"]["params"]["name"], "artifact_publish");
    assert_eq!(calls[1]["body"]["params"]["name"], "event_associate");
    assert_eq!(
        calls[1]["body"]["params"]["arguments"]["operation_id"],
        source.association_operation_id
    );
    assert_eq!(
        calls[1]["body"]["params"]["arguments"]["binding_id"],
        source.binding_id.to_string()
    );
    assert!(
        calls[1]["body"]["params"]["arguments"]
            .get("expected_epoch")
            .is_none()
    );
    agent.shutdown().await;
    drop(agent);
    let restored_agent = Agent::open_embedded(
        transcript_root,
        config(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .unwrap();
    let restored = restored_agent.resume_chat(chat).await.unwrap();
    let restored_goal = restored.loops.last().unwrap();
    assert_eq!(restored_goal.event_sources.len(), 1);
    assert_eq!(
        restored_goal.event_sources[0].association_operation_id,
        source.association_operation_id
    );
    assert_eq!(restored_goal.event_sources[0].association_epoch, Some(1));
    assert!(restored_goal.event_sources[0].resume_required);
    assert_eq!(restored_goal.state, LoopState::Interrupted);
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_associate")
            .count(),
        1
    );
    let restored_account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    restored_agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(restored_account, None)
                .with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    restored_agent
        .resume_event_sources(chat, restored_goal.id)
        .await
        .unwrap();
    let resumed = restored_agent.snapshot(chat).await.unwrap();
    assert_eq!(
        resumed.loops.last().unwrap().event_sources[0].association_epoch,
        Some(2)
    );
    assert!(!resumed.loops.last().unwrap().event_sources[0].resume_required);
    assert_eq!(
        resumed.loops.last().unwrap().state,
        LoopState::WaitingTrigger
    );
    ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin)
        .unwrap()
        .logout()
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = restored_agent.snapshot(chat).await.unwrap();
            let goal = snapshot.loops.last().unwrap();
            if goal.event_sources[0].resume_required && goal.state == LoopState::Interrupted {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn lost_association_response_keeps_one_persisted_operation_without_replaying_write() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control
        .drop_association_response
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let transcript_root = directory.path().join("transcripts");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-loss","source":"# Source"}),
        ),
        text("Published"),
    ]));
    let agent = Agent::open_embedded(transcript_root.clone(), config(), model).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let source = agent
        .snapshot(chat)
        .await
        .unwrap()
        .loops
        .last()
        .unwrap()
        .event_sources[0]
        .clone();
    assert_eq!(source.association_epoch, Some(1));
    assert!(!source.resume_required);
    let calls: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["body"]["method"] == "tools/call")
        .cloned()
        .collect();
    let associations: Vec<_> = calls
        .iter()
        .filter(|request| request["body"]["params"]["name"] == "event_associate")
        .collect();
    assert_eq!(associations.len(), 1);
    assert_eq!(
        associations[0]["body"]["params"]["arguments"]["operation_id"],
        source.association_operation_id
    );
    assert_eq!(
        calls
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_operation")
            .count(),
        1
    );
    agent.shutdown().await;
    drop(agent);
    let restored = Agent::open_embedded(
        transcript_root,
        config(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .unwrap()
    .resume_chat(chat)
    .await
    .unwrap();
    assert_eq!(
        restored.loops.last().unwrap().event_sources[0].association_operation_id,
        source.association_operation_id
    );
}

#[tokio::test]
async fn durable_feedback_is_claimed_once_after_ordinary_chat_finishes() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-feedback","source":"# Source"}),
        ),
        text("Published"),
        text("I received the owner feedback"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    let waiting = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        waiting.is_ok(),
        "snapshot: {:?}; calls: {:?}",
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .map(|goal| (&goal.state, &goal.event_sources)),
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .collect::<Vec<_>>()
    );
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    control.feedback_pending.store(true, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "event_claim")
    );
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if model.requests.lock().unwrap().len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let calls: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            request["body"]["method"] == "tools/call"
                && request["body"]["params"]["name"] == "event_claim"
        })
        .cloned()
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["body"]["params"]["arguments"]["event_id"],
        "event-one"
    );
    assert!(
        model.requests.lock().unwrap()[2]
            .messages
            .iter()
            .any(|message| format!("{message:?}").contains("Please update the source"))
    );
    assert!(
        model.requests.lock().unwrap()[2]
            .messages
            .iter()
            .any(|message| format!("{message:?}").contains("comment-one")
                && format!("{message:?}").contains("version-one")
                && format!("{message:?}").contains("# Source"))
    );
    assert!(!format!("{:?}", model.requests.lock().unwrap()).contains("secret-fence"));
}

#[tokio::test]
async fn idle_feedback_watcher_bounds_mcp_requests_and_recovers_a_later_comment() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-idle-rate","source":"# Source"}),
        ),
        text("Published"),
        text("I received the later comment"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting = agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger);
            let polled = requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["body"]["params"]["name"] == "event_pending");
            if waiting && polled {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    requests.lock().unwrap().clear();
    for _ in 0..60 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
    }
    let idle_requests = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["path"] == "POST /mcp HTTP/1.1")
        .cloned()
        .collect::<Vec<_>>();
    let count_method = |method: &str| {
        idle_requests
            .iter()
            .filter(|request| request["body"]["method"] == method)
            .count()
    };
    let pending_polls = idle_requests
        .iter()
        .filter(|request| request["body"]["params"]["name"] == "event_pending")
        .count();
    assert!(
        (3..=4).contains(&pending_polls),
        "60 seconds of idle feedback listening sent {pending_polls} pending polls"
    );
    assert_eq!(count_method("server/discover"), pending_polls);
    assert_eq!(count_method("tools/list"), pending_polls);
    assert_eq!(count_method("resources/read"), pending_polls);
    assert_eq!(count_method("tools/call"), pending_polls);
    assert_eq!(idle_requests.len(), pending_polls * 4);
    control.feedback_pending.store(true, Ordering::SeqCst);
    for _ in 0..20 {
        if model.requests.lock().unwrap().len() >= 3 {
            break;
        }
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
    }
    let pending_calls = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["body"]["params"]["name"] == "event_pending")
        .count();
    let claim_calls = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["body"]["params"]["name"] == "event_claim")
        .count();
    assert!(
        model.requests.lock().unwrap().len() >= 3,
        "feedback was not recovered in 20 seconds: pending={pending_calls}, claim={claim_calls}"
    );
    assert_eq!(claim_calls, 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn recovered_feedback_reaches_failed_loop_without_replaying_saved_publication() {
    assert_recovered_feedback_reaches_failed_loop(true).await;
}

#[tokio::test]
async fn live_failed_feedback_needs_explicit_resume_before_recovered_event_runs() {
    assert_recovered_feedback_reaches_failed_loop(false).await;
}

async fn assert_recovered_feedback_reaches_failed_loop(restart_agent: bool) {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let transcript_root = root.join("transcripts");
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "initial-publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-initial","source":"# Source"}),
        ),
        text("Published"),
        call(
            "partial-publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-partial","source":"# Saved partial work"}),
        ),
        call("invalid-tool", "missing_feedback_tool", json!({})),
    ]));
    let agent = Agent::open_embedded(transcript_root.clone(), config(), model.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let initial_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, initial_permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent.snapshot(chat).await.unwrap().loops[0].state == LoopState::WaitingTrigger {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    control.feedback_pending.store(true, Ordering::SeqCst);
    let partial_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind
                && request.name == "mcp__artifacts__artifact_publish"
            {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, partial_permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent.snapshot(chat).await.unwrap().loops[0].state == LoopState::Failed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(control.publication_revision.load(Ordering::SeqCst), 2);
    let failed = agent.snapshot(chat).await.unwrap();
    let failure = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::Failed(error) = events.recv().await.unwrap().kind {
                break error;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        failure.contains("invalid or repeated tool call"),
        "{failure}"
    );
    assert_eq!(failed.loops[0].event_sources[0].claim_operations.len(), 1);
    assert_eq!(
        failed.loops[0].event_sources[0].claim_operations[0].event_id,
        "event-one"
    );
    let old_loop_id = failed.loops[0].id;
    let (restored, recovered_model) = if restart_agent {
        agent.shutdown().await;
        drop(agent);
        let recovered_model = Arc::new(ScriptedModel::new(Vec::new()));
        let restored =
            Agent::open_embedded(transcript_root, config(), recovered_model.clone()).unwrap();
        let restored_snapshot = restored.resume_chat(chat).await.unwrap();
        assert_eq!(restored_snapshot.loops[0].state, LoopState::Failed);
        assert_eq!(
            restored_snapshot.loops[0].event_sources[0].claim_operations[0].status,
            tickloop_agent::runtime::EventClaimStatus::NeedsAttention,
        );
        let restored_account = ArtifactClient::for_origin(&root, &origin).unwrap();
        restored
            .install_host_tools(Arc::new(
                McpHostToolProvider::new(restored_account, None)
                    .with_event_bridge("tickloop://event-bridge/v1"),
            ))
            .unwrap();
        (restored, recovered_model)
    } else {
        assert!(!failed.loops[0].event_sources[0].resume_required);
        (agent, model)
    };
    recovered_model.enqueue(call(
        "recovered-reply",
        "mcp__artifacts__artifact_feedback_reply",
        json!({"source_key":"source-one","operation_id":"op-recovered-reply","text":"Completed the saved work"}),
    ));
    recovered_model.enqueue(call(
        "recovered-complete",
        "mcp__artifacts__artifact_feedback_complete",
        json!({"source_key":"source-one","operation_id":"op-recovered-complete","reply_operation_id":"op-recovered-reply"}),
    ));
    recovered_model.enqueue(text("Feedback completed from the saved publication"));
    restored
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let recovered_event_id = uuid::Uuid::new_v4();
    *control.recovered_feedback_event_id.lock().unwrap() = Some(recovered_event_id);
    control.feedback_claimed.store(false, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_claim")
            .count(),
        1,
        "historical feedback alone must not restart a failed Loop"
    );
    assert_eq!(
        restored
            .resume_event_sources(chat, old_loop_id)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        restored.snapshot(chat).await.unwrap().loops[0].state,
        LoopState::WaitingTrigger
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if control.feedback_completed.load(Ordering::SeqCst)
                && restored.snapshot(chat).await.unwrap().loops[0].state
                    == LoopState::WaitingTrigger
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    {
        let model_requests = recovered_model.requests.lock().unwrap();
        let recovery_request = model_requests
            .iter()
            .find(|request| {
                format!("{:?}", request.messages).contains(&recovered_event_id.to_string())
            })
            .expect("model did not receive the recovered feedback event");
        assert!(format!("{:?}", recovery_request.messages).contains("comment-one"));
        assert!(format!("{:?}", recovery_request.messages).contains("op-partial"));
        assert!(format!("{:?}", recovery_request.messages).contains("version-two"));
        assert!(!format!("{model_requests:?}").contains("secret-fence"));
    }
    {
        let calls = requests.lock().unwrap();
        let claims: Vec<_> = calls
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_claim")
            .collect();
        assert_eq!(claims.len(), 2);
        assert_eq!(
            claims[0]["body"]["params"]["arguments"]["event_id"],
            "event-one"
        );
        assert_eq!(
            claims[1]["body"]["params"]["arguments"]["event_id"],
            recovered_event_id.to_string()
        );
        assert_eq!(
            calls
                .iter()
                .filter(|request| request["body"]["params"]["name"] == "artifact_publish")
                .count(),
            2,
        );
        for tool in ["artifact_feedback_reply", "artifact_feedback_complete"] {
            let call = calls
                .iter()
                .find(|request| request["body"]["params"]["name"] == tool)
                .unwrap();
            let metadata = &call["body"]["params"]["_meta"]["tickloop.ai/event-bridge"];
            assert_eq!(metadata["event_id"], recovered_event_id.to_string());
            assert_eq!(metadata["claim_id"], "claim-recovered");
            assert_eq!(metadata["fencing_token"], "secret-fence-recovered");
        }
    }
    let finished = restored.snapshot(chat).await.unwrap();
    assert_eq!(
        finished.loops[0].event_sources[0].claim_operations[0].status,
        tickloop_agent::runtime::EventClaimStatus::NeedsAttention
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn explicit_resume_rearms_every_source_of_a_failed_loop() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control
        .second_publication_new_source
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "first-source",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-first-source","source":"# First source"}),
        ),
        call(
            "second-source",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-second-source","source":"# Second source"}),
        ),
        text("Both sources are published"),
        call("invalid-tool", "missing_feedback_tool", json!({})),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let published = agent
        .submit(chat, None, "Publish two sources".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.loops[0].state == LoopState::WaitingTrigger
                && snapshot.loops[0].event_sources.len() == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before = agent.snapshot(chat).await.unwrap();
    let source_keys = before.loops[0]
        .event_sources
        .iter()
        .map(|source| source.source_key.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        source_keys,
        std::collections::HashSet::from(["source-one", "source-two"])
    );
    agent
        .submit(
            chat,
            Some(published.loop_id),
            "This model output is invalid".into(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent.snapshot(chat).await.unwrap().loops[0].state == LoopState::Failed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let failed = agent.snapshot(chat).await.unwrap();
    assert!(
        failed.loops[0]
            .event_sources
            .iter()
            .all(|source| !source.resume_required)
    );
    assert_eq!(
        agent
            .resume_event_sources(chat, published.loop_id)
            .await
            .unwrap(),
        2
    );
    let resumed = agent.snapshot(chat).await.unwrap();
    assert_eq!(resumed.loops[0].state, LoopState::WaitingTrigger);
    assert!(resumed.loops[0].event_sources.iter().all(|source| {
        source.association_epoch == Some(2)
            && !source.resume_required
            && !source.association_unknown
    }));
    {
        let calls = requests.lock().unwrap();
        for source_key in ["source-one", "source-two"] {
            assert_eq!(
                calls
                    .iter()
                    .filter(
                        |request| request["body"]["params"]["name"] == "event_associate"
                            && request["body"]["params"]["arguments"]["source_key"] == source_key
                    )
                    .count(),
                2,
            );
        }
        assert!(
            !calls
                .iter()
                .any(|request| request["body"]["params"]["name"] == "event_claim")
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn partial_failed_resume_keeps_committed_epochs_for_explicit_retry() {
    assert_partial_failed_resume(PartialAssociationFailure::Rejected).await;
}

#[tokio::test]
async fn unknown_second_association_blocks_later_explicit_resume() {
    assert_partial_failed_resume(PartialAssociationFailure::Unknown).await;
}

async fn assert_partial_failed_resume(failure: PartialAssociationFailure) {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control
        .second_publication_new_source
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let transcripts = root.join("transcripts");
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "first-source",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-partial-first","source":"# First source"}),
        ),
        call(
            "second-source",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-partial-second","source":"# Second source"}),
        ),
        text("Both sources are published"),
        call("invalid-tool", "missing_feedback_tool", json!({})),
    ]));
    let agent = Agent::open_embedded(transcripts.clone(), config(), model).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let published = agent
        .submit(chat, None, "Publish two sources".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.loops[0].state == LoopState::WaitingTrigger
                && snapshot.loops[0].event_sources.len() == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    agent
        .submit(
            chat,
            Some(published.loop_id),
            "Return an invalid tool".into(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent.snapshot(chat).await.unwrap().loops[0].state == LoopState::Failed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    *control.partial_association_failure.lock().unwrap() = Some(failure);
    let error = agent
        .resume_event_sources(chat, published.loop_id)
        .await
        .unwrap_err();
    let partial = agent.snapshot(chat).await.unwrap();
    assert_eq!(partial.loops[0].state, LoopState::Failed);
    let first = partial.loops[0]
        .event_sources
        .iter()
        .find(|source| source.source_key == "source-one")
        .unwrap();
    let second = partial.loops[0]
        .event_sources
        .iter()
        .find(|source| source.source_key == "source-two")
        .unwrap();
    assert_eq!(first.association_epoch, Some(2));
    assert!(!first.association_unknown);
    assert_eq!(second.association_epoch, Some(1));
    assert_eq!(
        second.association_unknown,
        matches!(failure, PartialAssociationFailure::Unknown)
    );
    match failure {
        PartialAssociationFailure::Rejected => {
            assert!(
                error
                    .to_string()
                    .contains("Association rejected: association_conflict")
            );
            assert_eq!(
                second.association_rejected.as_deref(),
                Some("association_conflict")
            );
        }
        PartialAssociationFailure::Unknown => {
            assert!(error.to_string().contains("Association outcome unknown"));
            assert!(!second.association_operation_id.is_empty());
        }
    }
    control.feedback_pending.store(true, Ordering::SeqCst);
    control.next_feedback_event.store(true, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "event_claim")
    );
    control.feedback_pending.store(false, Ordering::SeqCst);
    agent.shutdown().await;
    drop(agent);

    let restored = Agent::open_embedded(
        transcripts,
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let resumed = restored.resume_chat(chat).await.unwrap();
    assert_eq!(resumed.loops[0].state, LoopState::Failed);
    let first = resumed.loops[0]
        .event_sources
        .iter()
        .find(|source| source.source_key == "source-one")
        .unwrap();
    let second = resumed.loops[0]
        .event_sources
        .iter()
        .find(|source| source.source_key == "source-two")
        .unwrap();
    assert_eq!(first.association_epoch, Some(2));
    assert_eq!(second.association_epoch, Some(1));
    let restored_account = ArtifactClient::for_origin(&root, &origin).unwrap();
    restored
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(restored_account, None)
                .with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    match failure {
        PartialAssociationFailure::Rejected => {
            assert_eq!(
                restored
                    .resume_event_sources(chat, published.loop_id)
                    .await
                    .unwrap(),
                2
            );
            let completed = restored.snapshot(chat).await.unwrap();
            assert_eq!(completed.loops[0].state, LoopState::WaitingTrigger);
            let first = completed.loops[0]
                .event_sources
                .iter()
                .find(|source| source.source_key == "source-one")
                .unwrap();
            let second = completed.loops[0]
                .event_sources
                .iter()
                .find(|source| source.source_key == "source-two")
                .unwrap();
            assert_eq!(first.association_epoch, Some(3));
            assert_eq!(second.association_epoch, Some(2));
            let calls = requests.lock().unwrap();
            for (source_key, expected_epochs) in [
                ("source-one", [json!(null), json!(1), json!(2)]),
                ("source-two", [json!(null), json!(1), json!(1)]),
            ] {
                let actual = calls
                    .iter()
                    .filter(|request| {
                        request["body"]["params"]["name"] == "event_associate"
                            && request["body"]["params"]["arguments"]["source_key"] == source_key
                    })
                    .map(|request| request["body"]["params"]["arguments"]["expected_epoch"].clone())
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected_epochs, "{source_key}");
            }
        }
        PartialAssociationFailure::Unknown => {
            let blocked = restored
                .resume_event_sources(chat, published.loop_id)
                .await
                .unwrap_err();
            assert!(
                blocked
                    .to_string()
                    .contains("previous association outcome is unknown")
            );
            let calls = requests.lock().unwrap();
            for source_key in ["source-one", "source-two"] {
                assert_eq!(
                    calls
                        .iter()
                        .filter(
                            |request| request["body"]["params"]["name"] == "event_associate"
                                && request["body"]["params"]["arguments"]["source_key"]
                                    == source_key
                        )
                        .count(),
                    2
                );
            }
            assert!(calls.iter().any(|request| request["body"]["params"]["name"]
                == "event_operation"
                && request["body"]["params"]["arguments"]["operation_id"]
                    == second.association_operation_id));
        }
    }
    restored.shutdown().await;
}

#[tokio::test]
async fn failed_loop_resume_does_not_rearm_when_plan_enters_during_association() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-plan-gate","source":"# Source"}),
        ),
        text("Published"),
        call("invalid-tool", "missing_feedback_tool", json!({})),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let published = agent.submit(chat, None, "Publish".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent.snapshot(chat).await.unwrap().loops[0].state == LoopState::WaitingTrigger {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    agent
        .submit(
            chat,
            Some(published.loop_id),
            "Return an invalid tool".into(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent.snapshot(chat).await.unwrap().loops[0].state == LoopState::Failed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    control
        .hold_association_response
        .store(true, Ordering::SeqCst);
    let resuming_agent = agent.clone();
    let resume = tokio::spawn(async move {
        resuming_agent
            .resume_event_sources(chat, published.loop_id)
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        control.association_response_started.notified(),
    )
    .await
    .unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    control.release_association_response.notify_one();
    let error = resume.await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Loop changed during feedback resume")
    );
    let blocked = agent.snapshot(chat).await.unwrap();
    assert_eq!(blocked.loops[0].state, LoopState::Failed);
    assert!(blocked.loops[0].event_sources[0].resume_required);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "event_claim")
    );

    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    assert_eq!(
        agent
            .resume_event_sources(chat, published.loop_id)
            .await
            .unwrap(),
        1
    );
    let recovered = agent.snapshot(chat).await.unwrap();
    assert_eq!(recovered.loops[0].state, LoopState::WaitingTrigger);
    assert_eq!(
        recovered.loops[0].event_sources[0].association_epoch,
        Some(3)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn claimed_feedback_reply_uses_host_metadata_without_exposing_fence_to_model() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-publish","source":"# Source"}),
        ),
        text("Published"),
        call(
            "reply",
            "mcp__artifacts__artifact_feedback_reply",
            json!({"source_key":"source-one","operation_id":"op-reply","text":"I can update it"}),
        ),
        text("Replied"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.feedback_pending.store(true, Ordering::SeqCst);
    let reply_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind
                && request.name == "mcp__artifacts__artifact_feedback_reply"
            {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "artifact_feedback_reply")
    );
    agent
        .answer_permission(chat, reply_permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["body"]["params"]["name"] == "artifact_feedback_reply")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    {
        let calls = requests.lock().unwrap();
        let reply = calls
            .iter()
            .find(|request| request["body"]["params"]["name"] == "artifact_feedback_reply")
            .unwrap();
        assert_eq!(
            reply["body"]["params"]["_meta"]["tickloop.ai/event-bridge"]["fencing_token"],
            "secret-fence"
        );
        assert!(
            reply["body"]["params"]["arguments"]
                .get("fencing_token")
                .is_none()
        );
    }
    assert!(!format!("{:?}", model.requests.lock().unwrap()).contains("secret-fence"));
    assert!(
        !serde_json::to_string(&agent.snapshot(chat).await.unwrap())
            .unwrap()
            .contains("secret-fence")
    );
}

#[tokio::test]
async fn stopping_a_waiting_feedback_run_releases_its_claim_once() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-publish","source":"# Source"}),
        ),
        text("Published"),
        call(
            "reply",
            "mcp__artifacts__artifact_feedback_reply",
            json!({"source_key":"source-one","operation_id":"op-reply","text":"Reply"}),
        ),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let first_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, first_permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.feedback_pending.store(true, Ordering::SeqCst);
    let reply_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind
                && request.name == "mcp__artifacts__artifact_feedback_reply"
            {
                break request;
            }
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    for _ in 0..1000 {
        if requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "event_renew")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let renew_count = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["body"]["params"]["name"] == "event_renew")
        .count();
    let snapshot = agent.snapshot(chat).await.unwrap();
    let claim_operations = &snapshot.loops.last().unwrap().event_sources[0].claim_operations;
    assert_eq!(renew_count, 1, "claim: {claim_operations:?}");
    for _ in 0..1000 {
        if agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .event_sources[0]
            .claim_operations[0]
            .lease_expires_at
            .as_deref()
            == Some("2100-01-01T00:00:00Z")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .event_sources[0]
            .claim_operations[0]
            .lease_expires_at
            .as_deref(),
        Some("2100-01-01T00:00:00Z")
    );
    tokio::time::resume();
    agent.stop(chat, reply_permission.loop_id).await.unwrap();
    let release = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["body"]["params"]["name"] == "event_release")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        release.len(),
        1,
        "snapshot: {:?}",
        agent.snapshot(chat).await.unwrap().loops.last()
    );
    assert_eq!(
        release[0]["body"]["params"]["arguments"]["outcome"],
        "needs-attention"
    );
    assert_eq!(
        release[0]["body"]["params"]["arguments"]["fencing_token"],
        "secret-fence"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.last().unwrap().state, LoopState::Stopped);
    assert!(
        snapshot.loops.last().unwrap().event_sources[0].claim_operations[0]
            .release_operation_id
            .is_some()
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("secret-fence")
    );
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "artifact_feedback_reply")
    );
}

#[tokio::test]
async fn shutdown_cancels_inflight_feedback_pending_poll() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.hold_pending_response.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-shutdown-poll","source":"# Source"}),
        ),
        text("Published"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        control.pending_response_started.notified(),
    )
    .await
    .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().loops[0].state,
        LoopState::WaitingTrigger
    );
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| { request["body"]["params"]["name"] == "event_claim" })
    );

    let shutdown_result =
        tokio::time::timeout(std::time::Duration::from_secs(2), agent.shutdown()).await;
    control.release_pending_response.notify_one();
    assert!(
        shutdown_result.is_ok(),
        "shutdown waited for a held feedback pending response"
    );
}

#[tokio::test]
async fn pending_pagination_reaches_later_events_after_unknown_first_page() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-page","source":"# Source"}),
        ),
        text("Published"),
        text("Later feedback received"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.paged_feedback.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if model.requests.lock().unwrap().len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    {
        let calls = requests.lock().unwrap();
        assert!(calls.iter().any(
            |request| request["body"]["params"]["name"] == "event_pending"
                && request["body"]["params"]["arguments"]["cursor"] == "page-one"
        ));
        assert!(
            calls
                .iter()
                .any(|request| request["body"]["params"]["name"] == "event_claim"
                    && request["body"]["params"]["arguments"]["event_id"] == "event-after-page")
        );
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let binding = &snapshot.loops.last().unwrap().event_sources[0];
    assert_eq!(
        binding
            .claim_operations
            .iter()
            .filter(
                |record| record.status == tickloop_agent::runtime::EventClaimStatus::NeedsAttention
            )
            .count(),
        30
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn publishing_a_second_version_reuses_the_confirmed_loop_binding() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish-first",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-first","source":"# First"}),
        ),
        text("First version published"),
        call(
            "publish-second",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-second","source":"# Second"}),
        ),
        text("Second version published"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Publish first version".into())
        .await
        .unwrap();
    let first_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, first_permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let original = agent
        .snapshot(chat)
        .await
        .unwrap()
        .loops
        .last()
        .unwrap()
        .event_sources[0]
        .clone();
    agent
        .submit(
            chat,
            Some(first.loop_id),
            "Publish an updated version".into(),
        )
        .await
        .unwrap();
    let second_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, second_permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.loops.last().is_some_and(|goal| {
                goal.state == LoopState::WaitingTrigger
                    && goal.event_sources[0]
                        .publication
                        .as_ref()
                        .unwrap()
                        .version_id
                        == "version-two"
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    let binding = &snapshot.loops.last().unwrap().event_sources;
    assert_eq!(binding.len(), 1);
    assert_eq!(binding[0].binding_id, original.binding_id);
    assert_eq!(binding[0].association_epoch, Some(1));
    assert_eq!(
        binding[0].publication.as_ref().unwrap().operation_id,
        "op-second"
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_associate")
            .count(),
        1
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_second_publication_retries_only_the_known_unsubmitted_association() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control
        .fail_association_after_publish
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "first",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-first","source":"# First"}),
        ),
        text("First publication complete"),
        call(
            "second",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-second","source":"# Second"}),
        ),
        text("Second publication complete"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Publish first".into())
        .await
        .unwrap();
    let first_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, first_permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let initial = agent
        .snapshot(chat)
        .await
        .unwrap()
        .loops
        .last()
        .unwrap()
        .event_sources[0]
        .clone();
    assert_eq!(initial.association_epoch, None);
    assert!(initial.resume_required);
    assert!(!initial.association_unknown);
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "event_associate")
    );
    agent
        .submit(chat, Some(first.loop_id), "Publish second".into())
        .await
        .unwrap();
    let second_permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, second_permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    let binding = &snapshot.loops.last().unwrap().event_sources;
    assert_eq!(binding.len(), 1);
    assert_eq!(binding[0].binding_id, initial.binding_id);
    assert_eq!(
        binding[0].association_operation_id,
        initial.association_operation_id
    );
    assert_eq!(binding[0].association_epoch, Some(1));
    assert_eq!(
        binding[0].publication.as_ref().unwrap().version_id,
        "version-two"
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_associate")
            .count(),
        1
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn explicit_resume_retries_an_association_confirmed_not_dispatched() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control
        .fail_association_after_publish
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-unsubmitted","source":"# Source"}),
        ),
        text("Published"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let access = Arc::new(EmbeddedAgentAccess::new(agent.clone()));
    let epoch = access.daemon_epoch();
    let socket = directory.path().join("access.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve_listener(
        listener,
        access,
        epoch,
        support::TRANSPORT_LIMITS,
    ));
    let client = LocalAgentClient::connect(&socket).await.unwrap();
    let created = client
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(directory.path().into()),
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created {
        execution,
        snapshot,
        ..
    } = created
    else {
        panic!("expected Create");
    };
    let chat = snapshot.conversation.id;
    let mut events = agent.subscribe();
    let first = agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let initial = agent
        .snapshot(chat)
        .await
        .unwrap()
        .loops
        .last()
        .unwrap()
        .event_sources[0]
        .clone();
    assert_eq!(initial.association_epoch, None);
    assert!(!initial.association_unknown);
    let resumed = client
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::ManageLoop {
                execution,
                action: LoopAction::ResumeEventSources {
                    loop_id: first.loop_id,
                },
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    assert!(matches!(
        resumed,
        CommandResult::EventSourcesResumed { count: 1 }
    ));
    let snapshot = agent.snapshot(chat).await.unwrap();
    let resumed = &snapshot.loops.last().unwrap().event_sources[0];
    assert_eq!(resumed.binding_id, initial.binding_id);
    assert_eq!(
        resumed.association_operation_id,
        initial.association_operation_id
    );
    assert_eq!(resumed.association_epoch, Some(1));
    assert!(!resumed.resume_required);
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_associate")
            .count(),
        1
    );
    client
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::ManageLoop {
                execution,
                action: LoopAction::DisableEventSources {
                    loop_id: first.loop_id,
                },
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let disabled = agent.snapshot(chat).await.unwrap();
    let goal = disabled.loops.last().unwrap();
    assert_eq!(goal.state, LoopState::Interrupted);
    assert!(goal.state_since.is_some());
    assert!(goal.event_sources[0].resume_required);
    agent.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn renew_failure_then_explicit_resume_starts_a_new_watcher() {
    assert_renew_failure_then_resume(None).await;
}

#[tokio::test]
async fn expired_or_replaced_claim_errors_cancel_the_run_without_replaying_work() {
    for code in ["claim_expired", "association_conflict"] {
        assert_renew_failure_then_resume(Some(code)).await;
    }
}

async fn assert_renew_failure_then_resume(error_code: Option<&'static str>) {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-renew","source":"# Source"}),
        ),
        text("Published"),
        call(
            "reply",
            "mcp__artifacts__artifact_feedback_reply",
            json!({"source_key":"source-one","operation_id":"op-reply","text":"Waiting for approval"}),
        ),
        text("New feedback received"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    let loop_id = permission.loop_id;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.feedback_pending.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind
                && request.name == "mcp__artifacts__artifact_feedback_reply"
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    control
        .fail_renew
        .store(error_code.is_none(), Ordering::SeqCst);
    *control.renew_error.lock().unwrap() = error_code;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    for _ in 0..1000 {
        if agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .event_sources[0]
            .resume_required
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.loops.last().unwrap().event_sources[0].resume_required
                && snapshot.loops.last().unwrap().state == LoopState::Interrupted
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_renew")
            .count(),
        1
    );
    control.next_feedback_event.store(true, Ordering::SeqCst);
    control.feedback_claimed.store(false, Ordering::SeqCst);
    let resumed = agent.resume_event_sources(chat, loop_id).await.unwrap();
    assert_eq!(resumed, 1);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if model.requests.lock().unwrap().len() >= 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    {
        let calls = requests.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|request| request["body"]["params"]["name"] == "event_claim"
                    && request["body"]["params"]["arguments"]["event_id"] == "event-two")
        );
    }
    assert!(!format!("{:?}", model.requests.lock().unwrap()).contains("secret-fence"));
    agent.shutdown().await;
}

#[tokio::test]
async fn terminal_lease_confirmation_requires_the_exact_operation_and_claim_identity() {
    let (origin, _, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let provider = Arc::new(
        McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-terminal-validation","source":"# Source"}),
        ),
        text("Published"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .state
            != LoopState::WaitingTrigger
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = snapshot.loops.last().unwrap();
    let binding = &goal.event_sources[0];
    let request = EventLeaseRequest {
        source: EventSourceLocator {
            server_id: binding.server_id.clone(),
            owner_account_id: binding.owner_account_id.clone(),
            source_key: binding.source_key.clone(),
            binding_id: binding.binding_id,
            client_instance_id: binding.client_instance_id,
            chat_id: chat,
            loop_id: goal.id,
            association_epoch: binding.association_epoch.unwrap(),
            credential_generation: binding.credential_generation,
            directory_fingerprint: binding.directory_fingerprint,
            bridge_fingerprint: binding.bridge_fingerprint,
        },
        event_id: "event-one".into(),
        claim_id: "claim-one".into(),
        fencing_token: "secret-fence".into(),
        operation_id: "terminal-validation".into(),
    };
    control.feedback_completed.store(true, Ordering::SeqCst);
    assert!(matches!(
        provider
            .renew_event_claim(request.clone(), CancellationToken::new())
            .await,
        EventLeaseOutcome::Completed
    ));
    for (key, value) in [
        ("operation_id", json!("another-operation")),
        ("claim_status", json!("running")),
        ("source_key", json!("another-source")),
        ("binding_id", json!(uuid::Uuid::new_v4())),
        ("association_epoch", json!(2)),
        ("event_id", json!("another-event")),
        ("claim_id", json!("another-claim")),
        ("lease_expires_at", json!("2100-01-01T00:00:00Z")),
    ] {
        *control.terminal_renew_override.lock().unwrap() = Some((key.into(), value));
        assert!(
            matches!(
                provider
                    .renew_event_claim(request.clone(), CancellationToken::new())
                    .await,
                EventLeaseOutcome::OutcomeUnknown(_)
            ),
            "accepted invalid terminal {key}"
        );
    }
    agent.shutdown().await;
}

struct DelayedFinalModel {
    scripted: ScriptedModel,
    final_started: tokio::sync::Notify,
    finish_final: tokio::sync::Notify,
}

#[async_trait]
impl ModelClient for DelayedFinalModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.scripted.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let output = self
            .scripted
            .complete(request, events, cancel.clone())
            .await?;
        if output.content.iter().any(
            |content| matches!(content, Content::Text { text } if text == "Feedback finalized"),
        ) {
            self.final_started.notify_one();
            tokio::select! {
                () = self.finish_final.notified() => {},
                () = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
        }
        Ok(output)
    }
}

#[tokio::test]
async fn completed_claim_keeps_streaming_run_and_watcher_alive_with_fenced_followup_tools() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(DelayedFinalModel {
        scripted: ScriptedModel::new(vec![
            call(
                "publish",
                "mcp__artifacts__artifact_publish",
                json!({"operation_id":"op-completion","source":"# Source"}),
            ),
            text("Published"),
            call(
                "reply",
                "mcp__artifacts__artifact_feedback_reply",
                json!({"source_key":"source-one","operation_id":"op-reply","text":"Done"}),
            ),
            call(
                "complete",
                "mcp__artifacts__artifact_feedback_complete",
                json!({"source_key":"source-one","operation_id":"op-complete","reply_operation_id":"op-reply"}),
            ),
            [
                text("Feedback finalized"),
                call(
                    "after-completion",
                    "mcp__artifacts__artifact_publish",
                    json!({"operation_id":"op-after-completion","source":"# Must stay fenced"}),
                ),
            ]
            .concat(),
            text("Completed request remains closed"),
            text("Second feedback received"),
        ]),
        final_started: tokio::sync::Notify::new(),
        finish_final: tokio::sync::Notify::new(),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .state
            != LoopState::WaitingTrigger
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.feedback_pending.store(true, Ordering::SeqCst);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        model.final_started.notified(),
    )
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    for _ in 0..1000 {
        if requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "event_renew")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    for _ in 0..1000 {
        let snapshot = agent.snapshot(chat).await.unwrap();
        let source = &snapshot.loops.last().unwrap().event_sources[0];
        if source.resume_required
            || serde_json::to_value(source.claim_operations[0].status).unwrap() == "Completed"
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = snapshot.loops.last().unwrap();
    assert!(
        !goal.event_sources[0].resume_required,
        "completed feedback canceled its streaming run: {goal:?}"
    );
    assert_eq!(goal.state, LoopState::Running);
    assert_eq!(
        serde_json::to_value(goal.event_sources[0].claim_operations[0].status).unwrap(),
        "Completed"
    );
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_renew")
            .count(),
        1
    );
    tokio::time::resume();
    model.finish_final.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .state
            != LoopState::WaitingTrigger
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    {
        let calls = requests.lock().unwrap();
        let followup = calls
            .iter()
            .find(|request| {
                request["body"]["params"]["arguments"]["operation_id"] == "op-after-completion"
            })
            .unwrap();
        assert_eq!(
            followup["body"]["params"]["_meta"]["tickloop.ai/event-bridge"]["fencing_token"],
            "secret-fence"
        );
        assert_eq!(
            control.publication_revision.load(Ordering::SeqCst),
            1,
            "post-completion publication must stay fenced"
        );
        for name in [
            "event_claim",
            "artifact_feedback_reply",
            "artifact_feedback_complete",
        ] {
            assert_eq!(
                calls
                    .iter()
                    .filter(|request| request["body"]["params"]["name"] == name)
                    .count(),
                1,
                "{name} was replayed"
            );
        }
    }
    control.next_feedback_event.store(true, Ordering::SeqCst);
    control
        .second_feedback_same_epoch
        .store(true, Ordering::SeqCst);
    control.feedback_claimed.store(false, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while model.scripted.requests.lock().unwrap().len() < 7 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "event_claim"
                && request["body"]["params"]["arguments"]["event_id"] == "event-two")
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(!snapshot.loops.last().unwrap().event_sources[0].resume_required);
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("secret-fence")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn stop_racing_remote_completion_records_a_terminal_claim_without_reopening_feedback() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(DelayedFinalModel {
        scripted: ScriptedModel::new(vec![
            call(
                "publish",
                "mcp__artifacts__artifact_publish",
                json!({"operation_id":"op-stop-completion","source":"# Source"}),
            ),
            text("Published"),
            call(
                "reply",
                "mcp__artifacts__artifact_feedback_reply",
                json!({"source_key":"source-one","operation_id":"op-reply","text":"Done"}),
            ),
            call(
                "complete",
                "mcp__artifacts__artifact_feedback_complete",
                json!({"source_key":"source-one","operation_id":"op-complete","reply_operation_id":"op-reply"}),
            ),
            text("Feedback finalized"),
        ]),
        final_started: tokio::sync::Notify::new(),
        finish_final: tokio::sync::Notify::new(),
    });
    let transcript_root = directory.path().join("transcripts");
    let agent = Agent::open_embedded(transcript_root.clone(), config(), model.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let receipt = agent.submit(chat, None, "Publish".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .state
            != LoopState::WaitingTrigger
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.feedback_pending.store(true, Ordering::SeqCst);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        model.final_started.notified(),
    )
    .await
    .unwrap();
    agent.stop(chat, receipt.loop_id).await.unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = snapshot.loops.last().unwrap();
    assert_eq!(goal.state, LoopState::Stopped);
    assert_eq!(
        goal.event_sources[0].claim_operations[0].status,
        tickloop_agent::runtime::EventClaimStatus::Completed
    );
    assert!(control.feedback_completed.load(Ordering::SeqCst));
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_release")
            .count(),
        1
    );
    agent.shutdown().await;
    let restored = Agent::open_embedded(
        transcript_root,
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert_eq!(
        snapshot.loops.last().unwrap().event_sources[0].claim_operations[0].status,
        tickloop_agent::runtime::EventClaimStatus::Completed
    );
    assert!(snapshot.loops.last().unwrap().event_sources[0].resume_required);
    restored.shutdown().await;
}

#[tokio::test]
async fn late_failed_renewal_cannot_interrupt_a_later_feedback_run_or_its_watcher() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(DelayedFinalModel {
        scripted: ScriptedModel::new(vec![
            call(
                "publish",
                "mcp__artifacts__artifact_publish",
                json!({"operation_id":"op-late-completion","source":"# Source"}),
            ),
            text("Published"),
            call(
                "reply",
                "mcp__artifacts__artifact_feedback_reply",
                json!({"source_key":"source-one","operation_id":"op-reply","text":"Done"}),
            ),
            call(
                "complete",
                "mcp__artifacts__artifact_feedback_complete",
                json!({"source_key":"source-one","operation_id":"op-complete","reply_operation_id":"op-reply"}),
            ),
            text("Feedback finalized"),
            text("Second feedback received"),
        ]),
        final_started: tokio::sync::Notify::new(),
        finish_final: tokio::sync::Notify::new(),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .state
            != LoopState::WaitingTrigger
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.feedback_pending.store(true, Ordering::SeqCst);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        model.final_started.notified(),
    )
    .await
    .unwrap();
    control.delay_renew_response.store(true, Ordering::SeqCst);
    *control.terminal_renew_override.lock().unwrap() = Some(("status".into(), json!("unknown")));
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    for _ in 0..1000 {
        if requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["params"]["name"] == "event_renew")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    tokio::time::resume();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        control.renew_response_started.notified(),
    )
    .await
    .unwrap();
    model.finish_final.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .unwrap()
            .state
            != LoopState::WaitingTrigger
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.next_feedback_event.store(true, Ordering::SeqCst);
    control
        .second_feedback_same_epoch
        .store(true, Ordering::SeqCst);
    control.feedback_claimed.store(false, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while model.scripted.requests.lock().unwrap().len() < 6 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    control.finish_renew_response.notify_one();
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(!snapshot.loops.last().unwrap().event_sources[0].resume_required);
    assert_ne!(snapshot.loops.last().unwrap().state, LoopState::Interrupted);
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_claim")
            .count(),
        2
    );
    agent.shutdown().await;
}

fn current_host_state(request: &ModelRequest) -> Value {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            Content::Text { text } => text
                .rsplit_once("Current host state: ")
                .map(|(_, state)| serde_json::from_str(state).unwrap()),
            _ => None,
        })
        .unwrap()
}

async fn login(client: &ArtifactClient) {
    let device = client.start_device_login().await.unwrap();
    client
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn delegated_chat_inherits_native_artifact_mcp_tools() {
    let (origin, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Inspect available Artifact tools"}),
        ),
        text("Child inspected tools."),
        text("Done."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Delegate tool inspection".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests[..2] {
        assert!(
            request
                .tools
                .iter()
                .any(|tool| tool.name == "mcp__artifacts__artifact_publish"),
            "Artifact MCP tools missing from parent or delegated child"
        );
        let host = current_host_state(request);
        assert_eq!(
            host["service_connections"]["artifacts"]["authentication"],
            "signed_in"
        );
        assert_eq!(
            host["service_connections"]["artifacts"]["email"],
            "one@example.test"
        );
        assert!(
            !serde_json::to_string(&request.messages)
                .unwrap()
                .contains("test-service-token")
        );
    }
}

#[tokio::test]
async fn delegated_artifact_calls_obey_frozen_allow_ask_and_deny() {
    use tickloop_agent::permissions::{Decision, PermissionRule};
    for decision in [Decision::Allow, Decision::Ask, Decision::Deny] {
        let (origin, observed, _) = service().await;
        let directory = tempfile::tempdir().unwrap();
        let account =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
        login(&account).await;
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "delegate",
                "Subagent",
                json!({"task":"Publish the authorized fixture document"}),
            ),
            call(
                "child-publish",
                "mcp__artifacts__artifact_publish",
                json!({"operation_id":"child-publish","source":"# Child"}),
            ),
            text("Child done."),
            text("Parent done."),
        ]));
        let agent = Agent::new_embedded(config(), model.clone());
        agent
            .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
            .unwrap();
        let parent = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(parent, ExecutionMode::FullAccess)
            .await
            .unwrap();
        agent
            .set_permission_rules(
                parent,
                vec![PermissionRule {
                    id: uuid::Uuid::new_v4(),
                    decision,
                    tool: "mcp__artifacts__artifact_publish".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(parent, None, "Delegate fixture publication".into())
            .await
            .unwrap();
        let mut asked = 0;
        let mut child_result = None;
        loop {
            let event = support::next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    assert_ne!(event.chat_id, parent);
                    assert_eq!(request.name, "mcp__artifacts__artifact_publish");
                    asked += 1;
                    agent
                        .answer_permission(event.chat_id, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted { call_id, result } if call_id == "child-publish" => {
                    child_result = Some(result)
                }
                EventKind::Completed { .. } if event.chat_id == parent => break,
                _ => {}
            }
        }
        assert_eq!(asked, usize::from(decision == Decision::Ask));
        assert_eq!(child_result.unwrap().is_error, decision == Decision::Deny);
        assert_eq!(
            observed
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["body"]["method"] == "tools/call")
                .count(),
            usize::from(decision != Decision::Deny)
        );
        {
            let requests = model.requests.lock().unwrap();
            assert!(
                requests[1]
                    .tools
                    .iter()
                    .any(|tool| tool.name == "mcp__artifacts__artifact_publish")
            );
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn artifact_login_state_is_independent_of_directory_and_connection_failures() {
    let (origin, _, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out"
    );
    login(&ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap())
        .await;
    control.directory_unavailable.store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "unavailable"
    );
    control.session_unavailable.store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    control.session_unavailable.store(false, Ordering::SeqCst);
    control.directory_unavailable.store(false, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "available"
    );
}

struct GatedDelegatedMcpModel {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    child_turns: AtomicUsize,
    parent_turns: AtomicUsize,
}
impl GatedDelegatedMcpModel {
    fn new() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            child_turns: AtomicUsize::new(0),
            parent_turns: AtomicUsize::new(0),
        }
    }
}
#[async_trait]
impl ModelClient for GatedDelegatedMcpModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ScriptedModel::new(vec![]).identity()
    }
    async fn complete(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let host = current_host_state(&request);
        let content = if host["delegation"].is_object() {
            if self.child_turns.fetch_add(1, Ordering::SeqCst) == 0 {
                assert!(
                    request
                        .tools
                        .iter()
                        .any(|tool| tool.name == "mcp__artifacts__artifact_publish")
                );
                self.entered.notify_one();
                self.release.notified().await;
                call(
                    "frozen-write",
                    "mcp__artifacts__artifact_publish",
                    json!({"operation_id":"frozen-write","source":"# Child"}),
                )
            } else {
                text("Child done.")
            }
        } else if self.parent_turns.fetch_add(1, Ordering::SeqCst) == 0 {
            call(
                "delegate",
                "Subagent",
                json!({"task":"Publish fixture under inherited authority"}),
            )
        } else {
            text("Parent done.")
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn delegated_artifact_authority_cannot_expand_after_parent_policy_or_account_changes() {
    use tickloop_agent::permissions::{Decision, PermissionRule};
    for change_account in [false, true] {
        let (origin, observed, control) = service().await;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let account = ArtifactClient::for_origin(&root, &origin).unwrap();
        login(&account).await;
        let model = Arc::new(GatedDelegatedMcpModel::new());
        let agent = Agent::new_embedded(config(), model.clone());
        agent
            .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
            .unwrap();
        let parent = agent.create_chat(root.clone()).await.unwrap();
        agent
            .set_execution_mode(parent, ExecutionMode::FullAccess)
            .await
            .unwrap();
        let rules = |decision| {
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision,
                tool: "mcp__artifacts__artifact_publish".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }]
        };
        if !change_account {
            agent
                .set_permission_rules(parent, rules(Decision::Deny))
                .await
                .unwrap();
        }
        let mut events = agent.subscribe();
        agent
            .submit(parent, None, "Delegate fixture write".into())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), model.entered.notified())
            .await
            .unwrap();
        if change_account {
            control.other_account.store(true, Ordering::SeqCst);
            login(&ArtifactClient::for_origin(&root, &origin).unwrap()).await;
        } else {
            agent
                .set_permission_rules(parent, rules(Decision::Allow))
                .await
                .unwrap();
        }
        model.release.notify_one();
        let result = loop {
            let event = support::next_event(&mut events).await;
            if let EventKind::ToolCompleted { call_id, result } = event.kind
                && call_id == "frozen-write"
            {
                break result;
            }
        };
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains(if change_account {
                "account is outside"
            } else {
                "ceiling denies"
            }),
            "{}",
            result.content
        );
        wait_for_completion(&agent, parent).await;
        assert!(
            !observed
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["body"]["method"] == "tools/call")
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn model_tool_inventory_keeps_native_artifact_shell_and_herdr_names() {
    let (origin, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let native = tickloop_agent::native_host::NativeHostToolProvider::new(Arc::new(
        tick_connect::access::TickAccess::new(tick_connect::herdr::HerdrContext::without_view(
            directory.path().join("unused.sock"),
        )),
    ));
    let model = Arc::new(ScriptedModel::new(vec![text("Ready.")]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            account,
            Some(Arc::new(native)),
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Inspect native tool inventory".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    for name in [
        "mcp__artifacts__artifact_publish",
        "Bash",
        "Read",
        "TaskStop",
        "CallTickTool",
        "ReadTick",
    ] {
        assert!(
            requests[0].tools.iter().any(|tool| tool.name == name),
            "{name}"
        );
    }
    let prompt = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(prompt.contains("Tool absence is not evidence of being signed out"));
    assert!(prompt.contains("Shared Tick behavior means updates reach the same Loop"));
    for name in ["CallTickTool", "ReadTick"] {
        let definition = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap();
        assert!(definition.description.contains("Herdr"));
        assert!(definition.description.contains("Artifact MCP"));
    }
}

async fn wait_for_completion(agent: &Agent, chat: tickloop_agent::protocol::ChatId) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .is_some_and(|loop_| loop_.state == LoopState::Completed)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn configured_event_bridge_reads_descriptor_and_hides_lifecycle_tools() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![text("Ready")]));
    let agent = Agent::new_embedded(config(), model.clone());
    let provider = Arc::new(
        McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v1"),
    );
    agent.install_host_tools(provider).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Inspect tools".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    let model_requests = model.requests.lock().unwrap();
    let names: Vec<_> = model_requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(names.contains(&"mcp__artifacts__artifact_publish"));
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("mcp__artifacts__event_"))
    );
    let observed = requests.lock().unwrap();
    assert!(observed.iter().any(|request| {
        request["body"]["method"] == "resources/read"
            && request["body"]["params"]["uri"] == "tickloop://event-bridge/v1"
            && request["headers"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("authorization: bearer test-service-token")
    }));
}

#[tokio::test]
async fn login_reveals_discovered_tool_and_approved_call_uploads_once() {
    let (origin, requests, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let artifact =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("Sign in first."),
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-one","source":"# Private"}),
        ),
        text("Published."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let provider = Arc::new(McpHostToolProvider::new(artifact, None));
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Check publishing".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(
        !model.requests.lock().unwrap()[0]
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("mcp__"))
    );

    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Publish the document".into())
        .await
        .unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        model.requests.lock().unwrap()[1]
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__artifacts__artifact_publish")
    );
    assert_eq!(provider.negotiated_version().as_deref(), Some("2026-07-28"));
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["method"] == "tools/call")
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let calls: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["body"]["method"] == "tools/call")
        .cloned()
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["body"]["params"]["name"], "artifact_publish");
    assert_eq!(
        calls[0]["body"]["params"]["arguments"]["source"],
        "# Private"
    );
    assert!(
        calls[0]["headers"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer test-service-token")
    );
    assert_eq!(
        calls[0]["body"]["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    let model_result = serde_json::to_string(&model.requests.lock().unwrap()[2].messages).unwrap();
    assert!(model_result.contains("https://docs.tickloop.ai/a/test"));
    assert!(!model_result.contains("test-service-token"));
    agent.shutdown().await;
}

#[tokio::test]
async fn logout_after_approval_request_prevents_dispatch_and_hides_directory() {
    let (origin, requests, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let artifact = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&artifact).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-two","source":"private"}),
        ),
        text("Stopped."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(artifact, None)))
        .unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert!(!permission.input.to_string().contains("private"));
    ArtifactClient::for_origin(&root, &origin)
        .unwrap()
        .logout()
        .await
        .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .count(),
        0
    );
    assert!(
        !model.requests.lock().unwrap()[1]
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("mcp__"))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn changed_complete_directory_rejects_prepared_write() {
    let (origin, requests, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let artifact = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&artifact).await;
    let provider = McpHostToolProvider::new(artifact, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let name = "mcp__artifacts__artifact_publish";
    let chat_id = tickloop_agent::protocol::ChatId::new_v4();
    let prepared = provider
        .prepare_call(
            chat_id,
            name,
            &json!({"operation_id":"op-three","source":"initial"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    control.directory_revision.store(1, Ordering::SeqCst);
    assert!(
        provider
            .revalidate_call(chat_id, &prepared, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .count(),
        0
    );
}

#[tokio::test]
async fn lost_call_response_is_unknown_and_is_never_replayed() {
    let (origin, requests, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let artifact = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&artifact).await;
    control.drop_call_response.store(true, Ordering::SeqCst);
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"op-lost","source":"do not replay"}),
        ),
        text("The outcome needs checking."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(artifact, None)))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let operation = &agent.snapshot(chat).await.unwrap().operations[0];
    assert_eq!(operation.state, OperationState::Unknown);
    assert!(
        operation
            .result
            .as_ref()
            .unwrap()
            .content
            .contains("outcome unknown")
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .count(),
        1
    );
    agent.shutdown().await;
}

struct NativeFixture;

#[async_trait]
impl HostToolProvider for NativeFixture {
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        Ok(Some(HostEndpointIdentity::TrustedAdapter {
            scope: uuid::Uuid::nil(),
        }))
    }

    async fn prepare_call_in_context(
        &self,
        chat_id: tickloop_agent::protocol::ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        Ok(self
            .prepare_call(chat_id, name, input, cancel)
            .await?
            .with_payload(context.clone()))
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "native_fixture".into(),
            description: "Native tool".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }]
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        Some(WorkspaceHostContext::attached_herdr(None))
    }

    fn permission(
        &self,
        _: tickloop_agent::protocol::ChatId,
        _: &str,
        _: &Value,
    ) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(
        &self,
        _: tickloop_agent::protocol::ChatId,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> ToolResult {
        ToolResult {
            content: "native".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn native_tools_and_workspace_metadata_survive_mcp_composition() {
    let (origin, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let artifact =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    let provider = McpHostToolProvider::new(artifact, Some(Arc::new(NativeFixture)));
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider
            .definitions()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["native_fixture"]
    );
    let context = provider.workspace_host_context().unwrap();
    assert_eq!(context.name, WorkspaceHostName::Herdr);
    assert_eq!(context.integration, WorkspaceIntegration::Available);
    assert_eq!(
        provider.host_endpoint().unwrap(),
        NativeFixture.host_endpoint().unwrap()
    );
    let context = HostCallContext {
        workdir: directory.path().into(),
        workspace_id: Some("work-context".into()),
        interaction: None,
    };
    let prepared = provider
        .prepare_call_in_context(
            uuid::Uuid::new_v4(),
            &context,
            "native_fixture",
            &json!({}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let captured = prepared
        .payload::<HostCallContext>()
        .expect("Native preparation receives its context");
    assert_eq!(captured.workdir, context.workdir);
    assert_eq!(captured.workspace_id, context.workspace_id);
}

#[tokio::test]
async fn remote_unauthorized_hides_tools_and_invalidates_local_login() {
    let (origin, _, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let artifact = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&artifact).await;
    let provider = McpHostToolProvider::new(artifact, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(
        provider
            .definitions()
            .iter()
            .any(|tool| tool.name == "mcp__artifacts__artifact_publish")
    );
    control.reject_mcp.store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert!(
        ArtifactClient::for_origin(root, &origin)
            .unwrap()
            .status()
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires the test-only local Artifact Worker bridge"]
async fn real_worker_bridge_publishes_and_reads_via_agent() {
    let origin = std::env::var("TICKLOOP_MCP_BRIDGE_ORIGIN")
        .unwrap_or_else(|_| "http://127.0.0.1:8788".into());
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let artifact = ArtifactClient::for_origin(&root, &origin).unwrap();
    let device = artifact.start_device_login().await.unwrap();
    let approved = reqwest::Client::new()
        .post(format!("{origin}/fixture/approve"))
        .json(&json!({"user_code": device.user_code}))
        .send()
        .await
        .unwrap();
    assert!(approved.status().is_success());
    artifact
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();

    let source = "# Rust MCP interop\n\nPrivate source";
    let operation_id = uuid::Uuid::new_v4().to_string();
    let publish_model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({
                "operation_id": operation_id, "title": "Rust MCP interop", "format": "markdown", "source": source,
            }),
        ),
        text("Published."),
    ]));
    let publish_agent = Agent::new_embedded(config(), publish_model.clone());
    let publish_provider = Arc::new(McpHostToolProvider::new(artifact, None));
    publish_agent
        .install_host_tools(publish_provider.clone())
        .unwrap();
    let chat = publish_agent.create_chat(root.clone()).await.unwrap();
    let mut events = publish_agent.subscribe();
    publish_agent
        .submit(chat, None, "Publish through MCP".into())
        .await
        .unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    publish_agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&publish_agent, chat).await;
    assert_eq!(
        publish_provider.negotiated_version().as_deref(),
        Some("2026-07-28")
    );
    let result = publish_agent.snapshot(chat).await.unwrap().operations[0]
        .result
        .as_ref()
        .unwrap()
        .content
        .clone();
    let published: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(published["structuredContent"]["operation_id"], operation_id);
    let artifact_id = published["structuredContent"]["artifact_id"]
        .as_str()
        .unwrap();
    assert!(
        published["structuredContent"]["url"]
            .as_str()
            .unwrap()
            .contains(artifact_id)
    );
    assert!(
        publish_model.requests.lock().unwrap()[0]
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__artifacts__artifact_read")
    );
    publish_agent.shutdown().await;

    let read_model = Arc::new(ScriptedModel::new(vec![
        call(
            "read",
            "mcp__artifacts__artifact_read",
            json!({"artifact_id":artifact_id}),
        ),
        text("Read."),
    ]));
    let read_agent = Agent::new_embedded(config(), read_model);
    let artifact = ArtifactClient::for_origin(&root, &origin).unwrap();
    read_agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(artifact, None)))
        .unwrap();
    let chat = read_agent.create_chat(root).await.unwrap();
    read_agent
        .submit(chat, None, "Read through MCP".into())
        .await
        .unwrap();
    wait_for_completion(&read_agent, chat).await;
    let read = read_agent.snapshot(chat).await.unwrap().operations[0]
        .result
        .as_ref()
        .unwrap()
        .content
        .clone();
    let read: Value = serde_json::from_str(&read).unwrap();
    assert_eq!(read["structuredContent"]["source"], source);
    read_agent.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the test-only local Artifact Worker bridge"]
async fn real_worker_event_bridge_associates_a_published_source() {
    let origin = std::env::var("TICKLOOP_MCP_BRIDGE_ORIGIN")
        .unwrap_or_else(|_| "http://127.0.0.1:8788".into());
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let artifact = ArtifactClient::for_origin(&root, &origin).unwrap();
    let device = artifact.start_device_login().await.unwrap();
    let approved = reqwest::Client::new()
        .post(format!("{origin}/fixture/approve"))
        .json(&json!({"user_code": device.user_code}))
        .send()
        .await
        .unwrap();
    assert!(approved.status().is_success());
    artifact
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();

    let operation_id = uuid::Uuid::new_v4().to_string();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({
                "operation_id": operation_id, "title": "Rust feedback bridge", "format": "markdown", "source": "# Feedback source",
            }),
        ),
        text("Published and listening"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let provider = Arc::new(
        McpHostToolProvider::new(artifact, None).with_event_bridge("tickloop://event-bridge/v1"),
    );
    agent.install_host_tools(provider).unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Publish and listen".into())
        .await
        .unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        permission
            .approval_reason
            .as_deref()
            .unwrap_or_default()
            .contains("associate a newly introduced source with the current Loop")
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    let binding = &snapshot.loops.last().unwrap().event_sources[0];
    assert_eq!(
        binding.publication.as_ref().unwrap().operation_id,
        operation_id
    );
    assert_eq!(binding.association_epoch, Some(1));
    assert!(!binding.resume_required);
    assert!(
        model.requests.lock().unwrap()[0]
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__artifacts__artifact_publish")
    );
    assert!(
        !model.requests.lock().unwrap()[0]
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__artifacts__artifact_event_claim")
    );
    let source_key = binding.source_key.clone();
    let version_id = binding.publication.as_ref().unwrap().version_id.clone();
    let reply_operation_id = uuid::Uuid::new_v4().to_string();
    let completion_operation_id = uuid::Uuid::new_v4().to_string();
    model.enqueue(call("feedback-reply", "mcp__artifacts__artifact_feedback_reply", json!({
        "source_key": source_key, "operation_id": reply_operation_id, "text": "I received your feedback",
    })));
    model.enqueue(call("feedback-complete", "mcp__artifacts__artifact_feedback_complete", json!({
        "source_key": source_key, "operation_id": completion_operation_id, "reply_operation_id": reply_operation_id,
    })));
    model.enqueue(text("Feedback handled"));

    let comment_model = Arc::new(ScriptedModel::new(vec![
        call(
            "comment",
            "mcp__artifacts__artifact_comment_add",
            json!({
                "artifact_id": source_key, "operation_id": uuid::Uuid::new_v4().to_string(),
                "text": "Please check this source", "version_id": version_id, "send_to_agent": true,
            }),
        ),
        text("Comment sent"),
    ]));
    let comment_agent = Agent::new_embedded(config(), comment_model);
    let comment_artifact =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    comment_agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(comment_artifact, None)
                .with_event_bridge("tickloop://event-bridge/v1"),
        ))
        .unwrap();
    let comment_chat = comment_agent
        .create_chat(directory.path().into())
        .await
        .unwrap();
    let mut comment_events = comment_agent.subscribe();
    comment_agent
        .submit(comment_chat, None, "Send owner feedback".into())
        .await
        .unwrap();
    let comment_permission = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let EventKind::PermissionRequested { request } =
                comment_events.recv().await.unwrap().kind
            {
                break request;
            }
        }
    })
    .await
    .unwrap();
    comment_agent
        .answer_permission(comment_chat, comment_permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&comment_agent, comment_chat).await;
    assert!(
        !comment_agent
            .snapshot(comment_chat)
            .await
            .unwrap()
            .operations[0]
            .result
            .as_ref()
            .unwrap()
            .is_error
    );
    comment_agent.shutdown().await;

    for expected in [
        "mcp__artifacts__artifact_feedback_reply",
        "mcp__artifacts__artifact_feedback_complete",
    ] {
        let permission = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let EventKind::PermissionRequested { request } =
                    events.recv().await.unwrap().kind
                    && request.name == expected
                {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        agent
            .answer_permission(chat, permission.id, true)
            .await
            .unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.loops.last().is_some_and(|goal| {
                goal.event_sources
                    .iter()
                    .map(|source| source.claim_operations.len())
                    .sum::<usize>()
                    == 1
                    && goal.state == LoopState::WaitingTrigger
            }) && snapshot.operations.iter().any(|operation| {
                operation.name == "mcp__artifacts__artifact_feedback_complete"
                    && operation.result.is_some()
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let handled = agent.snapshot(chat).await.unwrap();
    assert!(handled.operations.iter().any(|operation| {
        operation.name == "mcp__artifacts__artifact_feedback_reply"
            && operation
                .result
                .as_ref()
                .is_some_and(|result| !result.is_error)
    }));
    assert!(handled.operations.iter().any(|operation| {
        operation.name == "mcp__artifacts__artifact_feedback_complete"
            && operation
                .result
                .as_ref()
                .is_some_and(|result| !result.is_error)
    }));
    assert!(!format!("{:?}", model.requests.lock().unwrap()).contains("fencing_token"));
    agent.shutdown().await;
}

#[tokio::test]
async fn connection36_approved_connect_check_persists_real_unpublished_origin_without_private_meta()
{
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.connection_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "connect",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"connect-36"}),
        ),
        call(
            "check",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"connect-36","action":"check"}),
        ),
        text("The association is confirmed, not proof of online activity."),
    ]));
    let transcripts = root.join("transcripts");
    *control.transcript_root.lock().unwrap() = Some(transcripts.clone());
    let agent = Agent::open_embedded(transcripts.clone(), config(), model.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v2"),
        ))
        .unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            chat,
            None,
            "Connect this existing private document without publishing".into(),
        )
        .await
        .unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        permission
            .approval_reason
            .as_deref()
            .unwrap_or_default()
            .contains("associate")
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["body"]["params"]["name"] == "artifact_agent_connect")
            .count(),
        1
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["body"]["params"]["name"] == "event_associate")
            .count(),
        0
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let state = agent.snapshot(chat).await.unwrap();
            if state.loops.last().is_some_and(|goal| {
                matches!(
                    goal.state,
                    LoopState::WaitingTrigger | LoopState::Completed | LoopState::Stopped
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = snapshot.loops.last().unwrap();
    assert_eq!(
        goal.state,
        LoopState::WaitingTrigger,
        "{:?}",
        snapshot.operations
    );
    let binding = &goal.event_sources[0];
    let connection = binding.connection.as_ref().unwrap();
    assert!(binding.publication.is_none());
    assert_eq!(binding.association_epoch, Some(1));
    assert!(!binding.resume_required);
    assert_eq!(connection.operation_id, "connect-36");
    assert_eq!(connection.receipt.chat_id, chat);
    assert_eq!(connection.receipt.loop_id, goal.id);
    assert_eq!(connection.receipt.run_id, goal.run_id.unwrap());
    assert_eq!(connection.request.binding_id, binding.binding_id);
    assert_eq!(
        connection.request.client_instance_id,
        binding.client_instance_id
    );
    let sent: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["body"]["params"]["name"] == "event_associate")
        .cloned()
        .collect();
    assert_eq!(sent.len(), 1);
    let saved_at_dispatch = control
        .association_disk_snapshot
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    assert!(
        saved_at_dispatch.contains("association_request"),
        "complete connection request must be on disk before dispatch: {saved_at_dispatch}"
    );
    assert!(saved_at_dispatch.contains(&connection.request.binding_id.to_string()));
    assert!(saved_at_dispatch.contains(&connection.request.client_instance_id.to_string()));
    assert!(saved_at_dispatch.contains("prepared"));
    assert_eq!(
        sent[0]["body"]["params"]["arguments"]["operation_id"],
        "connect-36"
    );
    assert_eq!(
        sent[0]["body"]["params"]["arguments"]["binding_id"],
        connection.request.binding_id.to_string()
    );
    let result: Value =
        serde_json::from_str(&snapshot.operations[1].result.as_ref().unwrap().content).unwrap();
    assert_eq!(
        result["structuredContent"]["connection_status"],
        "associated_current"
    );
    assert_eq!(result["structuredContent"]["resume_required"], false);
    let saved = serde_json::to_value(binding).unwrap();
    assert!(saved.get("version_id").is_none());
    assert_eq!(saved["origin"]["kind"], "connection");
    assert_eq!(saved["association_request"]["operation_id"], "connect-36");
    for operation in &snapshot.operations {
        let content = &operation.result.as_ref().unwrap().content;
        assert!(!content.contains("tickloop.ai/source-connect/v1"));
        assert!(!content.contains("current_association"));
        assert!(!content.contains("\"connection_status\":\"checked\""));
    }
    {
        let model_requests = model.requests.lock().unwrap();
        assert!(
            !model_requests[0]
                .tools
                .iter()
                .any(|tool| tool.name.starts_with("mcp__artifacts__event_"))
        );
        assert!(
            model_requests
                .iter()
                .skip(1)
                .flat_map(|r| &r.messages)
                .flat_map(|message| &message.content)
                .all(|block| match block {
                    Content::ToolResult { content, .. } =>
                        !format!("{content:?}").contains("tickloop.ai/source-connect/v1"),
                    _ => true,
                })
        );
    }
    agent.shutdown().await;
    drop(agent);
    let restored =
        Agent::open_embedded(transcripts, config(), Arc::new(ScriptedModel::new(vec![]))).unwrap();
    let restored_snapshot = restored.resume_chat(chat).await.unwrap();
    let restored_binding = &restored_snapshot.loops.last().unwrap().event_sources[0];
    assert!(restored_binding.resume_required);
    assert_eq!(
        restored_binding.connection.as_ref().unwrap().request,
        connection.request
    );
    assert!(restored_binding.publication.is_none());
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["body"]["params"]["name"] == "event_associate")
            .count(),
        1
    );
    restored.shutdown().await;
}

async fn real_worker_fixture_tool(
    provider: &McpHostToolProvider,
    name: &str,
    input: Value,
) -> Value {
    let chat = uuid::Uuid::new_v4();
    let cancel = CancellationToken::new();
    let prepared = provider
        .prepare_call(chat, name, &input, cancel.clone())
        .await
        .unwrap();
    provider
        .revalidate_call(chat, &prepared, cancel.clone())
        .await
        .unwrap();
    let result = match provider.execute_prepared(chat, prepared, cancel).await {
        tickloop_agent::host_tools::HostCallResult::Completed(result) => result,
        tickloop_agent::host_tools::HostCallResult::NotDispatched(error)
        | tickloop_agent::host_tools::HostCallResult::OutcomeUnknown(error) => {
            panic!("{name}: {error}")
        }
        tickloop_agent::host_tools::HostCallResult::CompletedWithEffect(_, _) => {
            panic!("fixture must not create a host effect")
        }
    };
    assert!(!result.is_error, "{name}: {}", result.content);
    let response: Value = serde_json::from_str(&result.content).unwrap();
    response["structuredContent"].clone()
}

#[tokio::test]
#[ignore = "requires the test-only local Artifact Worker bridge"]
async fn real_worker_connection_preserves_unpublished_markdown_and_html_working_content() {
    let origin = std::env::var("TICKLOOP_MCP_BRIDGE_ORIGIN")
        .unwrap_or_else(|_| "http://127.0.0.1:8788".into());
    for (format, source) in [
        (
            "markdown",
            "# Unpublished research\n\nHuman notes must remain unchanged.",
        ),
        (
            "html",
            "<!doctype html><h1>Unpublished dashboard</h1><button onclick=\"this.textContent='Clicked'\">Counter</button>",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let account = ArtifactClient::for_origin(&root, &origin).unwrap();
        let device = account.start_device_login().await.unwrap();
        let approved = reqwest::Client::new()
            .post(format!("{origin}/fixture/approve"))
            .json(&json!({"user_code": device.user_code}))
            .send()
            .await
            .unwrap();
        assert!(approved.status().is_success());
        account
            .finish_device_login(device, &CancellationToken::new())
            .await
            .unwrap();
        let fixtures =
            McpHostToolProvider::new(ArtifactClient::for_origin(&root, &origin).unwrap(), None);
        fixtures.refresh(CancellationToken::new()).await.unwrap();
        let created = real_worker_fixture_tool(&fixtures, "mcp__artifacts__artifact_draft_create", json!({
            "operation_id": uuid::Uuid::new_v4().to_string(), "title": "Unpublished connection fixture", "format": format,
        })).await;
        let artifact_id = created["artifact_id"].as_str().unwrap();
        let opened = real_worker_fixture_tool(
            &fixtures,
            "mcp__artifacts__artifact_edit_open",
            json!({
                "artifact_id": artifact_id, "operation_id": uuid::Uuid::new_v4().to_string(),
            }),
        )
        .await;
        let target = &opened["targets"][0];
        real_worker_fixture_tool(&fixtures, "mcp__artifacts__artifact_edit_patch", json!({
            "artifact_id": artifact_id, "operation_id": uuid::Uuid::new_v4().to_string(),
            "working_id": opened["working_id"], "generation": opened["generation"],
            "patches": [{"type":"replace", "target":target["id"], "expectedHash":target["hash"], "text":source}],
        })).await;
        let before = real_worker_fixture_tool(
            &fixtures,
            "mcp__artifacts__artifact_edit_read",
            json!({"artifact_id":artifact_id}),
        )
        .await;
        assert!(before["base_version_id"].is_null());
        assert!(before["source"].as_str().unwrap().contains("Unpublished"));
        let versions_before = real_worker_fixture_tool(
            &fixtures,
            "mcp__artifacts__artifact_versions",
            json!({"artifact_id":artifact_id}),
        )
        .await;
        let operation_id = uuid::Uuid::new_v4().to_string();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "connect",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":artifact_id,"operation_id":operation_id}),
            ),
            call(
                "check",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":artifact_id,"operation_id":operation_id,"action":"check"}),
            ),
            text("Connected without publishing or editing."),
        ]));
        let agent = Agent::open_embedded(root.join("transcripts"), config(), model).unwrap();
        agent
            .install_host_tools(Arc::new(
                McpHostToolProvider::new(account, None)
                    .with_event_bridge("tickloop://event-bridge/v2"),
            ))
            .unwrap();
        let chat = agent.create_chat(root.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(
                chat,
                None,
                "Connect this unpublished document without changing its content".into(),
            )
            .await
            .unwrap();
        connection36_permission(&agent, chat, &mut events).await;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let state = agent.snapshot(chat).await.unwrap();
                if state.loops.last().is_some_and(|goal| {
                    matches!(goal.state, LoopState::WaitingTrigger | LoopState::Completed)
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            snapshot.loops.last().unwrap().state,
            LoopState::WaitingTrigger,
            "{:?}",
            snapshot.operations
        );
        assert_eq!(snapshot.operations.len(), 2);
        let prepared: Value =
            serde_json::from_str(&snapshot.operations[0].result.as_ref().unwrap().content).unwrap();
        let checked: Value =
            serde_json::from_str(&snapshot.operations[1].result.as_ref().unwrap().content).unwrap();
        assert_eq!(
            prepared["structuredContent"]["connection_status"],
            "prepared"
        );
        assert_eq!(
            checked["structuredContent"]["connection_status"],
            "associated_current"
        );
        let binding = &snapshot.loops.last().unwrap().event_sources[0];
        assert!(binding.publication.is_none());
        assert_eq!(
            binding.connection.as_ref().unwrap().operation_id,
            operation_id
        );
        assert_eq!(binding.association_operation_id, operation_id);
        assert_eq!(binding.source_key, artifact_id);
        assert_eq!(
            real_worker_fixture_tool(
                &fixtures,
                "mcp__artifacts__artifact_edit_read",
                json!({"artifact_id":artifact_id})
            )
            .await,
            before
        );
        assert_eq!(
            real_worker_fixture_tool(
                &fixtures,
                "mcp__artifacts__artifact_versions",
                json!({"artifact_id":artifact_id})
            )
            .await,
            versions_before
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn connection36_same_run_cannot_replay_its_unknown_association() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.connection_enabled.store(true, Ordering::SeqCst);
    control
        .drop_association_response
        .store(true, Ordering::SeqCst);
    control
        .hide_association_receipts
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "connect",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"same-run-unknown"}),
        ),
        call(
            "check",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"same-run-unknown","action":"check"}),
        ),
        call(
            "automatic-retry",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"same-run-unknown"}),
        ),
        call(
            "final-check",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"same-run-unknown","action":"check"}),
        ),
        text("The outcome remains unknown; only a new explicit owner request may retry."),
    ]));
    let agent = Agent::open_embedded(root.join("transcripts"), config(), model.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v2"),
        ))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .submit(
            chat,
            None,
            "Connect once; do not retry an unknown outcome".into(),
        )
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_associate")
            .count(),
        1
    );
    assert_eq!(snapshot.loops[0].event_sources.len(), 1);
    assert!(snapshot.loops[0].event_sources[0].association_unknown);
    let checked: Value = serde_json::from_str(
        &snapshot
            .operations
            .last()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .content,
    )
    .unwrap();
    assert_eq!(checked["structuredContent"]["connection_status"], "unknown");
    let original_loop = snapshot.loops[0].id;
    agent
        .configure_trigger(
            chat,
            original_loop,
            Some(tickloop_agent::runtime::Trigger {
                id: uuid::Uuid::new_v4(),
                prompt: "Check the pending connection without repeating it".into(),
                interval_seconds: None,
                expires_at_ms: None,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    model.enqueue(call(
        "triggered-retry",
        "mcp__artifacts__artifact_agent_connect",
        json!({"artifact_id":"source-one","operation_id":"same-run-unknown"}),
    ));
    model.enqueue(text("The timer must not authorize another attempt."));
    let mut events = agent.subscribe();
    agent
        .trigger(
            chat,
            original_loop,
            tickloop_agent::runtime::TriggerSource {
                source: "timer".into(),
                payload: json!({"connection":"still unknown"}),
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind,
                EventKind::Completed { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_associate")
            .count(),
        1,
        "a new automatic run is not a new owner request"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn connection36_working_publication_rejects_effect_for_a_different_artifact() {
    let (origin, _, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.connection_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let provider =
        McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v2");
    provider.refresh(CancellationToken::new()).await.unwrap();
    let chat = uuid::Uuid::new_v4();
    let cancel = CancellationToken::new();
    let prepared = provider
        .prepare_call(
            chat,
            "mcp__artifacts__artifact_edit_publish",
            &json!({"artifact_id":"source-two","operation_id":"wrong-source-publication"}),
            cancel.clone(),
        )
        .await
        .unwrap();
    provider
        .revalidate_call(chat, &prepared, cancel.clone())
        .await
        .unwrap();
    let outcome = provider.execute_prepared(chat, prepared, cancel).await;
    assert!(
        matches!(
            outcome,
            tickloop_agent::host_tools::HostCallResult::Completed(_)
        ),
        "a committed result for source-one must not produce a source effect for frozen source-two"
    );
}

#[tokio::test]
async fn connection36_check_does_not_adopt_a_publication_only_association() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.connection_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "publish",
            "mcp__artifacts__artifact_publish",
            json!({"operation_id":"publication-only","source":"# Existing publication"}),
        ),
        text("Published"),
    ]));
    let agent = Agent::open_embedded(root.join("transcripts"), config(), model.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v2"),
        ))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Publish".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind,
                EventKind::Completed { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    agent.disable_event_sources().await;
    let before = agent.snapshot(chat).await.unwrap();
    let publication_source = &before.loops[0].event_sources[0];
    assert!(publication_source.connection.is_none());
    assert_eq!(publication_source.association_epoch, Some(1));
    model.enqueue(call(
        "check",
        "mcp__artifacts__artifact_agent_connect",
        json!({"artifact_id":"source-one","operation_id":publication_source.association_operation_id,"action":"check"}),
    ));
    model.enqueue(text(
        "There is no saved explicit connection request to verify.",
    ));
    agent.submit(chat, None, "Check only".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    let after = agent.snapshot(chat).await.unwrap();
    let checked: Value = serde_json::from_str(
        &after
            .operations
            .last()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .content,
    )
    .unwrap();
    assert_eq!(checked["structuredContent"]["connection_status"], "unknown");
    assert_eq!(
        serde_json::to_value(&after.loops[0].event_sources[0]).unwrap(),
        serde_json::to_value(publication_source).unwrap(),
    );
    assert!(after.loops.last().unwrap().event_sources.is_empty());
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_associate")
            .count(),
        1,
    );
    agent.shutdown().await;
}

async fn connection36_permission(
    agent: &Agent,
    chat: tickloop_agent::protocol::ChatId,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
) {
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn connection36_approval_wait_does_not_refresh_takeover_epoch_and_check_reports_definite_rejection()
 {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.connection_enabled.store(true, Ordering::SeqCst);
    *control.association.lock().unwrap() = Some(
        json!({"binding_id":"other-binding","client_instance_id":"other-client","chat_id":"other-chat","loop_id":"other-loop","association_epoch":4}),
    );
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "connect",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"takeover-36","replace_existing":true}),
        ),
        call(
            "check",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"takeover-36","action":"check"}),
        ),
        text("The frozen takeover was refused."),
    ]));
    let agent =
        Agent::open_embedded(directory.path().join("transcripts"), config(), model).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v2"),
        ))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Explicitly replace the old association".into())
        .await
        .unwrap();
    let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["body"]["params"]["name"] == "artifact_agent_connect")
            .count(),
        1
    );
    control.association.lock().unwrap().as_mut().unwrap()["association_epoch"] = json!(5);
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let sent: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["body"]["params"]["name"] == "event_associate")
        .cloned()
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["body"]["params"]["arguments"]["expected_epoch"], 4);
    assert_eq!(
        control.association.lock().unwrap().as_ref().unwrap()["association_epoch"],
        5
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    let source = &snapshot.loops.last().unwrap().event_sources[0];
    assert_eq!(
        source.connection.as_ref().unwrap().request.expected_epoch,
        Some(4)
    );
    assert_eq!(source.association_epoch, None);
    assert_eq!(
        source.association_rejected.as_deref(),
        Some("association_conflict")
    );
    assert!(source.resume_required);
    let checked: Value =
        serde_json::from_str(&snapshot.operations[1].result.as_ref().unwrap().content).unwrap();
    assert_eq!(checked["structuredContent"]["connection_status"], "failed");
    assert_eq!(
        checked["structuredContent"]["error"]["code"],
        "association_conflict"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn connection36_exact_retry_keeps_frozen_account_directory_and_bridge_guards() {
    for changed_guard in ["credential", "directory", "bridge"] {
        let (origin, requests, control) = service().await;
        control.bridge_enabled.store(true, Ordering::SeqCst);
        control.connection_enabled.store(true, Ordering::SeqCst);
        control
            .drop_association_response
            .store(true, Ordering::SeqCst);
        control
            .hide_association_receipts
            .store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let account = ArtifactClient::for_origin(&root, &origin).unwrap();
        login(&account).await;
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "first",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"retry-36"}),
            ),
            text("Unknown; retain the exact request."),
        ]));
        let agent =
            Agent::open_embedded(root.join("transcripts"), config(), model.clone()).unwrap();
        agent
            .install_host_tools(Arc::new(
                McpHostToolProvider::new(account, None)
                    .with_event_bridge("tickloop://event-bridge/v2"),
            ))
            .unwrap();
        let chat = agent.create_chat(root.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, None, "Connect the document".into())
            .await
            .unwrap();
        connection36_permission(&agent, chat, &mut events).await;
        wait_for_completion(&agent, chat).await;
        let before = agent.snapshot(chat).await.unwrap();
        let original = &before.loops[0].event_sources[0];
        assert!(original.association_unknown);
        match changed_guard {
            "directory" => control.directory_revision.store(1, Ordering::SeqCst),
            "bridge" => control.bridge_revision.store(1, Ordering::SeqCst),
            "credential" => {
                control.credential_revision.store(1, Ordering::SeqCst);
                login(&ArtifactClient::for_origin(&root, &origin).unwrap()).await;
            }
            _ => unreachable!(),
        }
        control
            .drop_association_response
            .store(false, Ordering::SeqCst);
        model.enqueue(call(
            "retry",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"retry-36"}),
        ));
        model.enqueue(text("The original unknown request remains frozen."));
        agent
            .submit(
                chat,
                None,
                "Explicitly retry the exact same operation ID".into(),
            )
            .await
            .unwrap();
        connection36_permission(&agent, chat, &mut events).await;
        wait_for_completion(&agent, chat).await;
        let after = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["body"]["params"]["name"] == "event_associate")
                .count(),
            1,
            "{changed_guard} changed"
        );
        assert_eq!(
            serde_json::to_value(&after.loops[0].event_sources[0]).unwrap(),
            serde_json::to_value(original).unwrap(),
            "{changed_guard} changed"
        );
        assert!(after.loops.last().unwrap().event_sources.is_empty());
        let result: Value = serde_json::from_str(
            &after
                .operations
                .last()
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .content,
        )
        .unwrap();
        assert_eq!(
            result["structuredContent"]["connection_status"],
            "not_dispatched"
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn connection36_checkpoint_failure_and_memory_only_agent_dispatch_nothing() {
    for persisted in [true, false] {
        let (origin, requests, control) = service().await;
        control.bridge_enabled.store(true, Ordering::SeqCst);
        control.connection_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let account = ArtifactClient::for_origin(&root, &origin).unwrap();
        login(&account).await;
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "connect",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"checkpoint-failure"}),
            ),
            call(
                "check",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"checkpoint-failure","action":"check"}),
            ),
            text("Nothing was dispatched by this attempt."),
        ]));
        let transcripts = root.join("transcripts");
        let agent = if persisted {
            Agent::open_embedded(transcripts.clone(), config(), model).unwrap()
        } else {
            Agent::new_embedded(config(), model)
        };
        agent
            .install_host_tools(Arc::new(
                McpHostToolProvider::new(account, None)
                    .with_event_bridge("tickloop://event-bridge/v2"),
            ))
            .unwrap();
        let chat = agent.create_chat(root).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Connect".into()).await.unwrap();
        let permission = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let EventKind::PermissionRequested { request } =
                    events.recv().await.unwrap().kind
                {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        let mut blocked_path = None;
        if persisted {
            agent.flush_transcripts().await;
            fn find(path: &std::path::Path) -> Option<std::path::PathBuf> {
                if path.extension().is_some_and(|ext| ext == "jsonl") {
                    return Some(path.into());
                }
                std::fs::read_dir(path)
                    .ok()?
                    .flatten()
                    .find_map(|entry| find(&entry.path()))
            }
            let path = find(&transcripts).unwrap();
            std::fs::rename(&path, path.with_extension("saved")).unwrap();
            std::fs::create_dir(&path).unwrap();
            blocked_path = Some(path);
        }
        agent
            .answer_permission(chat, permission.id, true)
            .await
            .unwrap();
        wait_for_completion(&agent, chat).await;
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["body"]["params"]["name"] == "event_associate")
                .count(),
            0
        );
        let snapshot = agent.snapshot(chat).await.unwrap();
        let source = &snapshot.loops[0].event_sources[0];
        assert!(!source.association_unknown);
        assert!(source.connection.is_some());
        for operation in &snapshot.operations {
            let result: Value =
                serde_json::from_str(&operation.result.as_ref().unwrap().content).unwrap();
            assert_eq!(
                result["structuredContent"]["connection_status"],
                "not_dispatched"
            );
        }
        if let Some(path) = blocked_path {
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(path.with_extension("saved"), path).unwrap();
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn connection36_new_loop_publication_preserves_current_connection_origin() {
    for (verify_first, stop_original, clear_remote) in [
        (true, false, false),
        (false, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let (origin, requests, control) = service().await;
        control.bridge_enabled.store(true, Ordering::SeqCst);
        control.connection_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let account = ArtifactClient::for_origin(&root, &origin).unwrap();
        login(&account).await;
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "connect",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"original-connect"}),
            ),
            call(
                "check",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"original-connect","action":"check"}),
            ),
            text("Connected."),
        ]));
        let agent =
            Agent::open_embedded(root.join("transcripts"), config(), model.clone()).unwrap();
        agent
            .install_host_tools(Arc::new(
                McpHostToolProvider::new(account, None)
                    .with_event_bridge("tickloop://event-bridge/v2"),
            ))
            .unwrap();
        let chat = agent.create_chat(root.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Connect".into()).await.unwrap();
        connection36_permission(&agent, chat, &mut events).await;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if agent
                    .snapshot(chat)
                    .await
                    .unwrap()
                    .loops
                    .last()
                    .is_some_and(|goal| {
                        matches!(goal.state, LoopState::WaitingTrigger | LoopState::Completed)
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let first = agent.snapshot(chat).await.unwrap();
        let original = first.loops[0].event_sources[0].clone();
        if stop_original {
            agent.stop(chat, first.loops[0].id).await.unwrap();
        }
        if clear_remote {
            agent.disable_event_sources().await;
            *control.association.lock().unwrap() = None;
        }
        let before_publication = agent.snapshot(chat).await.unwrap();
        if verify_first {
            model.enqueue(call(
                "again",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"new-connect-id"}),
            ));
        }
        model.enqueue(call(
            "publish",
            "mcp__artifacts__artifact_edit_publish",
            json!({"artifact_id":"source-one","operation_id":"real-new-publication"}),
        ));
        model.enqueue(text("Published without moving the original connection."));
        agent
            .submit(
                chat,
                None,
                "Publish this document's actual working content".into(),
            )
            .await
            .unwrap();
        connection36_permission(&agent, chat, &mut events).await;
        if verify_first {
            connection36_permission(&agent, chat, &mut events).await;
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if agent
                    .snapshot(chat)
                    .await
                    .unwrap()
                    .loops
                    .last()
                    .is_some_and(|goal| {
                        matches!(goal.state, LoopState::WaitingTrigger | LoopState::Completed)
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let after = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["body"]["params"]["name"] == "event_associate")
                .count(),
            1
        );
        assert_eq!(
            after
                .loops
                .iter()
                .map(|goal| goal.event_sources.len())
                .sum::<usize>(),
            1
        );
        let source = &after.loops[0].event_sources[0];
        let before_source = &before_publication.loops[0].event_sources[0];
        assert_eq!(source.resume_required, before_source.resume_required);
        assert_eq!(source.association_epoch, before_source.association_epoch);
        assert_eq!(
            source.credential_generation,
            before_source.credential_generation
        );
        assert_eq!(
            source.directory_fingerprint,
            before_source.directory_fingerprint
        );
        assert_eq!(source.bridge_fingerprint, before_source.bridge_fingerprint);
        if stop_original {
            assert_eq!(after.loops[0].state, LoopState::Stopped);
        }
        if clear_remote {
            assert!(control.association.lock().unwrap().is_none());
        }
        assert_eq!(
            source.connection.as_ref().unwrap().request,
            original.connection.as_ref().unwrap().request
        );
        assert_eq!(
            serde_json::to_value(&source.connection.as_ref().unwrap().receipt).unwrap(),
            serde_json::to_value(&original.connection.as_ref().unwrap().receipt).unwrap()
        );
        assert_eq!(source.binding_id, original.binding_id);
        assert_eq!(
            source.association_operation_id,
            original.association_operation_id
        );
        let publication = source.publication.as_ref().unwrap();
        assert_eq!(publication.operation_id, "real-new-publication");
        assert_eq!(publication.version_id, "version-one");
        assert_eq!(publication.receipt.loop_id, after.loops[1].id);
        assert_ne!(
            publication.receipt.run_id,
            original.connection.unwrap().receipt.run_id
        );
        if verify_first {
            let already: Value =
                serde_json::from_str(&after.operations[2].result.as_ref().unwrap().content)
                    .unwrap();
            assert_eq!(
                already["structuredContent"]["connection_status"],
                "already_associated"
            );
        }
        assert!(after.loops[1].event_sources.is_empty());
        for replay_version in ["version-one", "version-two"] {
            control.publication_revision.store(
                usize::from(replay_version == "version-two"),
                Ordering::SeqCst,
            );
            model.enqueue(call(
                "publish-replay",
                "mcp__artifacts__artifact_edit_publish",
                json!({"artifact_id":"source-one","operation_id":"real-new-publication"}),
            ));
            model.enqueue(text(
                "The original publication receipt remains authoritative.",
            ));
            agent
                .submit(chat, None, "Retry the same publication".into())
                .await
                .unwrap();
            connection36_permission(&agent, chat, &mut events).await;
            wait_for_completion(&agent, chat).await;
            let replayed = agent.snapshot(chat).await.unwrap();
            assert_eq!(
                serde_json::to_value(&replayed.loops[0].event_sources[0]).unwrap(),
                serde_json::to_value(source).unwrap(),
                "an operation replay must preserve the original version and Receipt"
            );
            assert!(replayed.loops.last().unwrap().event_sources.is_empty());
        }
        if verify_first {
            control.publication_revision.store(1, Ordering::SeqCst);
            model.enqueue(call(
                "publish-second",
                "mcp__artifacts__artifact_edit_publish",
                json!({"artifact_id":"source-one","operation_id":"second-publication"}),
            ));
            model.enqueue(text("Second publication recorded."));
            agent
                .submit(chat, None, "Publish another version".into())
                .await
                .unwrap();
            connection36_permission(&agent, chat, &mut events).await;
            wait_for_completion(&agent, chat).await;
            let second = agent.snapshot(chat).await.unwrap();
            let second_source = &second.loops[0].event_sources[0];
            assert_eq!(
                second_source.publication.as_ref().unwrap().operation_id,
                "second-publication"
            );
            assert_eq!(
                second_source.publication.as_ref().unwrap().version_id,
                "version-two"
            );
            assert_eq!(second_source.publication_history.len(), 1);
            assert_eq!(
                second_source.publication_history[0].operation_id,
                "real-new-publication"
            );
            assert_eq!(
                second_source.publication_history[0].receipt.run_id,
                publication.receipt.run_id
            );
            let legacy_record = serde_json::to_value(source).unwrap();
            assert!(legacy_record.get("publication_history").is_none());
            let legacy_restored: tickloop_agent::runtime::EventSourceBinding =
                serde_json::from_value(legacy_record).unwrap();
            assert!(legacy_restored.publication_history.is_empty());
            let mut duplicate_operation = serde_json::to_value(second_source).unwrap();
            duplicate_operation["publication_history"][0]["operation_id"] =
                json!("second-publication");
            assert!(
                serde_json::from_value::<tickloop_agent::runtime::EventSourceBinding>(
                    duplicate_operation
                )
                .is_err()
            );
            control.publication_revision.store(2, Ordering::SeqCst);
            model.enqueue(call(
                "replay-first",
                "mcp__artifacts__artifact_edit_publish",
                json!({"artifact_id":"source-one","operation_id":"real-new-publication"}),
            ));
            model.enqueue(text("Original publication replayed."));
            agent
                .submit(chat, None, "Replay first publication".into())
                .await
                .unwrap();
            connection36_permission(&agent, chat, &mut events).await;
            wait_for_completion(&agent, chat).await;
            let replayed = agent.snapshot(chat).await.unwrap();
            assert_eq!(
                serde_json::to_value(&replayed.loops[0].event_sources[0]).unwrap(),
                serde_json::to_value(second_source).unwrap(),
                "P1 -> P2 -> P1 must keep P2 and its Receipt"
            );
            assert!(replayed.loops.last().unwrap().event_sources.is_empty());
        }
        if clear_remote {
            model.enqueue(call(
                "connect-another-loop",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"second-connect"}),
            ));
            model.enqueue(call(
                "check-another-loop",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"second-connect","action":"check"}),
            ));
            model.enqueue(text("Connected in the new Loop."));
            agent
                .submit(chat, None, "Connect in a new Loop".into())
                .await
                .unwrap();
            connection36_permission(&agent, chat, &mut events).await;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if agent
                        .snapshot(chat)
                        .await
                        .unwrap()
                        .loops
                        .last()
                        .is_some_and(|goal| {
                            matches!(goal.state, LoopState::WaitingTrigger | LoopState::Completed)
                        })
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let with_second_connection = agent.snapshot(chat).await.unwrap();
            let later_binding = with_second_connection
                .loops
                .last()
                .unwrap()
                .event_sources
                .first()
                .unwrap();
            assert!(later_binding.connection.is_some());
            assert!(later_binding.publication.is_none());
            model.enqueue(call(
                "replay-before-restart",
                "mcp__artifacts__artifact_edit_publish",
                json!({"artifact_id":"source-one","operation_id":"real-new-publication"}),
            ));
            model.enqueue(text("Older publication replayed."));
            agent
                .submit(chat, None, "Replay an older publication".into())
                .await
                .unwrap();
            connection36_permission(&agent, chat, &mut events).await;
            wait_for_completion(&agent, chat).await;
            let replayed = agent.snapshot(chat).await.unwrap();
            assert!(
                replayed
                    .loops
                    .iter()
                    .flat_map(|goal| &goal.event_sources)
                    .find(|source| source
                        .connection
                        .as_ref()
                        .is_some_and(|connection| connection.operation_id == "second-connect"))
                    .unwrap()
                    .publication
                    .is_none()
            );
        }
        agent.shutdown().await;
        if verify_first || clear_remote {
            drop(agent);
            let restored_model = Arc::new(ScriptedModel::new(vec![
                call(
                    "replay-after-restart",
                    "mcp__artifacts__artifact_edit_publish",
                    json!({"artifact_id":"source-one","operation_id":"real-new-publication"}),
                ),
                text("Durable replay checked."),
            ]));
            let restored =
                Agent::open_embedded(root.join("transcripts"), config(), restored_model).unwrap();
            let restored_account = ArtifactClient::for_origin(&root, &origin).unwrap();
            restored
                .install_host_tools(Arc::new(
                    McpHostToolProvider::new(restored_account, None)
                        .with_event_bridge("tickloop://event-bridge/v2"),
                ))
                .unwrap();
            let before = restored.resume_chat(chat).await.unwrap();
            let before_sources: Vec<_> = before
                .loops
                .iter()
                .flat_map(|goal| &goal.event_sources)
                .map(|source| serde_json::to_value(source).unwrap())
                .collect();
            let mut restored_events = restored.subscribe();
            restored
                .submit(chat, None, "Replay after restart".into())
                .await
                .unwrap();
            connection36_permission(&restored, chat, &mut restored_events).await;
            wait_for_completion(&restored, chat).await;
            let after = restored.snapshot(chat).await.unwrap();
            let after_sources: Vec<_> = after
                .loops
                .iter()
                .flat_map(|goal| &goal.event_sources)
                .map(|source| serde_json::to_value(source).unwrap())
                .collect();
            assert_eq!(
                after_sources, before_sources,
                "restart replay changed durable source provenance"
            );
            restored.shutdown().await;
        }
    }
}

struct AssociationRefusal {
    inner: McpHostToolProvider,
    refuse: AtomicBool,
}

#[async_trait]
impl HostToolProvider for AssociationRefusal {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }
    fn permission(
        &self,
        chat: tickloop_agent::protocol::ChatId,
        name: &str,
        input: &Value,
    ) -> HostToolPermission {
        self.inner.permission(chat, name, input)
    }
    async fn refresh(&self, cancel: CancellationToken) -> Result<(), String> {
        self.inner.refresh(cancel).await
    }
    async fn prepare_call(
        &self,
        chat: tickloop_agent::protocol::ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<tickloop_agent::host_tools::PreparedHostCall, String> {
        self.inner.prepare_call(chat, name, input, cancel).await
    }
    async fn revalidate_call(
        &self,
        chat: tickloop_agent::protocol::ChatId,
        call: &tickloop_agent::host_tools::PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        self.inner.revalidate_call(chat, call, cancel).await
    }
    async fn execute_prepared(
        &self,
        chat: tickloop_agent::protocol::ChatId,
        call: tickloop_agent::host_tools::PreparedHostCall,
        cancel: CancellationToken,
    ) -> tickloop_agent::host_tools::HostCallResult {
        self.inner.execute_prepared(chat, call, cancel).await
    }
    async fn execute(
        &self,
        chat: tickloop_agent::protocol::ChatId,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        self.inner.execute(chat, name, input, cancel).await
    }
    async fn associate_event_source(
        &self,
        request: tickloop_agent::host_tools::EventAssociationRequest,
        cancel: CancellationToken,
    ) -> tickloop_agent::host_tools::EventAssociationOutcome {
        if self.refuse.load(Ordering::SeqCst) {
            tickloop_agent::host_tools::EventAssociationOutcome::NotDispatched(
                "account revalidation refused this attempt".into(),
            )
        } else {
            self.inner.associate_event_source(request, cancel).await
        }
    }
}

#[tokio::test]
async fn connection36_later_not_dispatched_preserves_unknown_and_exact_retry_can_commit() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.connection_enabled.store(true, Ordering::SeqCst);
    control
        .drop_association_response
        .store(true, Ordering::SeqCst);
    control
        .hide_association_receipts
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "first",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"unknown-connect"}),
        ),
        text("Unknown."),
    ]));
    let provider = Arc::new(AssociationRefusal {
        inner: McpHostToolProvider::new(account, None)
            .with_event_bridge("tickloop://event-bridge/v2"),
        refuse: AtomicBool::new(false),
    });
    let agent = Agent::open_embedded(root.join("transcripts"), config(), model.clone()).unwrap();
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Connect".into()).await.unwrap();
    connection36_permission(&agent, chat, &mut events).await;
    wait_for_completion(&agent, chat).await;
    let original = agent.snapshot(chat).await.unwrap().loops[0].event_sources[0].clone();
    assert!(original.association_unknown);
    provider.refuse.store(true, Ordering::SeqCst);
    model.enqueue(call(
        "retry",
        "mcp__artifacts__artifact_agent_connect",
        json!({"artifact_id":"source-one","operation_id":"unknown-connect"}),
    ));
    model.enqueue(call(
        "check",
        "mcp__artifacts__artifact_agent_connect",
        json!({"artifact_id":"source-one","operation_id":"unknown-connect","action":"check"}),
    ));
    model.enqueue(text("The earlier request remains unknown."));
    agent
        .submit(chat, None, "Explicit exact retry".into())
        .await
        .unwrap();
    connection36_permission(&agent, chat, &mut events).await;
    wait_for_completion(&agent, chat).await;
    let after = agent.snapshot(chat).await.unwrap();
    assert!(after.loops[0].event_sources[0].association_unknown);
    assert_eq!(
        after.loops[0].event_sources[0]
            .connection
            .as_ref()
            .unwrap()
            .request,
        original.connection.as_ref().unwrap().request
    );
    let checked: Value = serde_json::from_str(
        &after
            .operations
            .last()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .content,
    )
    .unwrap();
    assert_eq!(checked["structuredContent"]["connection_status"], "unknown");
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["body"]["params"]["name"] == "event_associate")
            .count(),
        1
    );
    provider.refuse.store(false, Ordering::SeqCst);
    control
        .drop_association_response
        .store(false, Ordering::SeqCst);
    control
        .hide_association_receipts
        .store(false, Ordering::SeqCst);
    model.enqueue(call(
        "exact",
        "mcp__artifacts__artifact_agent_connect",
        json!({"artifact_id":"source-one","operation_id":"unknown-connect"}),
    ));
    model.enqueue(call(
        "final",
        "mcp__artifacts__artifact_agent_connect",
        json!({"artifact_id":"source-one","operation_id":"unknown-connect","action":"check"}),
    ));
    model.enqueue(text(
        "Confirmed historical association; listening still requires explicit resume.",
    ));
    agent
        .submit(chat, None, "Explicit exact retry again".into())
        .await
        .unwrap();
    connection36_permission(&agent, chat, &mut events).await;
    wait_for_completion(&agent, chat).await;
    let final_state = agent.snapshot(chat).await.unwrap();
    let sent: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["body"]["params"]["name"] == "event_associate")
        .cloned()
        .collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0]["body"]["params"]["arguments"],
        sent[1]["body"]["params"]["arguments"]
    );
    assert!(!final_state.loops[0].event_sources[0].association_unknown);
    assert!(final_state.loops[0].event_sources[0].resume_required);
    assert_eq!(
        final_state.loops[0].event_sources[0]
            .connection
            .as_ref()
            .unwrap()
            .request,
        original.connection.unwrap().request
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn connection36_stopped_source_check_and_new_id_report_resume_required_without_listening() {
    for check_only in [true, false] {
        let (origin, requests, control) = service().await;
        control.bridge_enabled.store(true, Ordering::SeqCst);
        control.connection_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let account = ArtifactClient::for_origin(&root, &origin).unwrap();
        login(&account).await;
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "connect",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"stopped-original"}),
            ),
            call(
                "first-check",
                "mcp__artifacts__artifact_agent_connect",
                json!({"artifact_id":"source-one","operation_id":"stopped-original","action":"check"}),
            ),
            text("The association is current."),
        ]));
        let agent =
            Agent::open_embedded(root.join("transcripts"), config(), model.clone()).unwrap();
        agent
            .install_host_tools(Arc::new(
                McpHostToolProvider::new(account, None)
                    .with_event_bridge("tickloop://event-bridge/v2"),
            ))
            .unwrap();
        let chat = agent.create_chat(root).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, None, "Connect the document".into())
            .await
            .unwrap();
        connection36_permission(&agent, chat, &mut events).await;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if agent.snapshot(chat).await.unwrap().loops[0].state == LoopState::WaitingTrigger {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let connected = agent.snapshot(chat).await.unwrap();
        let original_loop = connected.loops[0].id;
        let original_request = connected.loops[0].event_sources[0]
            .connection
            .as_ref()
            .unwrap()
            .request
            .clone();
        assert!(!connected.loops[0].event_sources[0].resume_required);
        agent.stop(chat, original_loop).await.unwrap();
        // Allow the cancelled watcher to finish; compare activity across two poll intervals below.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let pending_before = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_pending")
            .count();
        let arguments = if check_only {
            json!({"artifact_id":"source-one","operation_id":"stopped-original","action":"check"})
        } else {
            json!({"artifact_id":"source-one","operation_id":"new-id-after-stop"})
        };
        model.enqueue(call(
            "after-stop",
            "mcp__artifacts__artifact_agent_connect",
            arguments,
        ));
        model.enqueue(text(
            "Listening requires explicit resume of the original Loop.",
        ));
        agent
            .submit(
                chat,
                None,
                "Check the existing association after Stop".into(),
            )
            .await
            .unwrap();
        if !check_only {
            connection36_permission(&agent, chat, &mut events).await;
        }
        wait_for_completion(&agent, chat).await;
        let checked = agent.snapshot(chat).await.unwrap();
        let result: Value = serde_json::from_str(
            &checked
                .operations
                .last()
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .content,
        )
        .unwrap();
        assert_eq!(
            result["structuredContent"]["connection_status"],
            if check_only {
                "associated_current"
            } else {
                "already_associated"
            }
        );
        assert_eq!(
            result["structuredContent"]["resume_required"], true,
            "check_only={check_only}"
        );
        assert_eq!(checked.loops[0].state, LoopState::Stopped);
        assert!(checked.loops[0].event_sources[0].resume_required);
        assert_eq!(
            checked.loops[0].event_sources[0]
                .connection
                .as_ref()
                .unwrap()
                .request,
            original_request
        );
        assert!(checked.loops.last().unwrap().event_sources.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        {
            let calls = requests.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|request| request["body"]["params"]["name"] == "event_associate")
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|request| request["body"]["params"]["name"] == "event_pending")
                    .count(),
                pending_before
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|request| request["body"]["params"]["name"] == "event_claim")
                    .count(),
                0
            );
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn connection36_delayed_initiating_check_does_not_rearm_explicitly_disabled_listening() {
    let (origin, requests, control) = service().await;
    control.bridge_enabled.store(true, Ordering::SeqCst);
    control.connection_enabled.store(true, Ordering::SeqCst);
    control
        .hold_connection_check_response
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "connect",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"delayed-check-connect"}),
        ),
        call(
            "check",
            "mcp__artifacts__artifact_agent_connect",
            json!({"artifact_id":"source-one","operation_id":"delayed-check-connect","action":"check"}),
        ),
        text("The association is current; listening remains explicitly disabled."),
    ]));
    let agent = Agent::open_embedded(root.join("transcripts"), config(), model).unwrap();
    agent
        .install_host_tools(Arc::new(
            McpHostToolProvider::new(account, None).with_event_bridge("tickloop://event-bridge/v2"),
        ))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Connect the existing document".into())
        .await
        .unwrap();
    connection36_permission(&agent, chat, &mut events).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        control.connection_check_response_started.notified(),
    )
    .await
    .unwrap();

    let held = agent.snapshot(chat).await.unwrap();
    let original_request = held.loops[0].event_sources[0]
        .connection
        .as_ref()
        .unwrap()
        .request
        .clone();
    assert_eq!(held.loops[0].state, LoopState::Running);
    assert_eq!(held.loops[0].event_sources[0].association_epoch, Some(1));
    assert!(!held.loops[0].event_sources[0].association_unknown);
    assert!(
        held.operations
            .iter()
            .any(|operation| operation.call_id == "check" && operation.result.is_none())
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["params"]["name"] == "event_associate")
            .count(),
        1
    );

    agent.disable_event_sources().await;
    let disabled = agent.snapshot(chat).await.unwrap();
    assert_eq!(disabled.loops[0].state, LoopState::Running);
    assert!(disabled.loops[0].event_sources[0].resume_required);
    control.release_connection_check_response.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot
                .operations
                .iter()
                .any(|operation| operation.call_id == "check" && operation.result.is_some())
                && matches!(
                    snapshot.loops[0].state,
                    LoopState::Completed | LoopState::WaitingTrigger | LoopState::Interrupted
                )
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let after = agent.snapshot(chat).await.unwrap();
    let checked: Value = serde_json::from_str(
        &after
            .operations
            .iter()
            .find(|operation| operation.call_id == "check")
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .content,
    )
    .unwrap();
    assert_eq!(
        checked["structuredContent"]["connection_status"],
        "associated_current"
    );
    assert_eq!(checked["structuredContent"]["resume_required"], true);
    assert!(after.loops[0].event_sources[0].resume_required);
    assert_eq!(
        after.loops[0].event_sources[0]
            .connection
            .as_ref()
            .unwrap()
            .request,
        original_request
    );
    assert_eq!(after.loops[0].event_sources[0].association_epoch, Some(1));
    assert!(after.loops[0].event_sources[0].publication.is_none());
    assert_ne!(after.loops[0].state, LoopState::WaitingTrigger);
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    {
        let calls = requests.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|request| request["body"]["params"]["name"] == "event_associate")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|request| request["body"]["params"]["name"] == "event_pending")
                .count(),
            0
        );
        assert_eq!(
            calls
                .iter()
                .filter(|request| request["body"]["params"]["name"] == "event_claim")
                .count(),
            0
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn optional_attachment_descriptor_creates_a_display_only_tick() {
    for (advertised, failed, unconfirmed, wrong_operation, expected) in [
        (true, false, false, false, 1),
        (false, false, false, false, 0),
        (true, true, false, false, 0),
        (true, false, true, false, 0),
        (true, false, false, true, 0),
    ] {
        let (origin, requests, control) = service().await;
        control.creation_enabled.store(advertised, Ordering::SeqCst);
        control
            .resource_directory_enabled
            .store(true, Ordering::SeqCst);
        control.reject_creation.store(failed, Ordering::SeqCst);
        control
            .unconfirmed_creation
            .store(unconfirmed, Ordering::SeqCst);
        control
            .wrong_creation_operation
            .store(wrong_operation, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let account =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
        login(&account).await;
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call(
                    "create",
                    "mcp__artifacts__create_custom_document",
                    json!({"operation_id":"creation-one","title":"Created document","format":"markdown"}),
                ),
                text("done"),
            ])),
        );
        agent
            .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
            .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Create".into()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match events.recv().await.unwrap().kind {
                    EventKind::PermissionRequested { request } => agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap(),
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let ticks = agent.artifact_ticks(chat).await.unwrap();
        assert_eq!(
            ticks.len(),
            expected,
            "advertised={advertised} failed={failed}"
        );
        if let Some(tick) = ticks.first() {
            assert_eq!(tick.title, "Created document");
            assert_eq!(tick.origin, origin);
        }
        assert!(agent.tick_subscriptions(chat).await.unwrap().is_empty());
        assert!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .iter()
                .all(|goal| goal.event_sources.is_empty())
        );
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r["body"]["params"]["name"] == "event_associate")
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn changed_attachment_mapping_is_rejected_before_dispatch() {
    let (origin, requests, control) = service().await;
    control.creation_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account =
        ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
    login(&account).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let chat = tickloop_agent::protocol::ChatId::new_v4();
    let prepared = provider
        .prepare_call(
            chat,
            "mcp__artifacts__create_custom_document",
            &json!({"operation_id":"create-once","title":"Document","format":"markdown"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    control.attachment_revision.store(1, Ordering::SeqCst);
    assert!(
        provider
            .revalidate_call(chat, &prepared, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["body"]["method"] == "tools/call")
    );
}

#[tokio::test]
async fn stale_session_401_never_reports_a_replaced_login_as_signed_out() {
    let (origin, _, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let provider = Arc::new(McpHostToolProvider::new(account, None));
    control.active.store(false, Ordering::SeqCst);
    control.hold_session_response.store(true, Ordering::SeqCst);
    let running = provider.clone();
    let refresh =
        tokio::spawn(async move { running.refresh(CancellationToken::new()).await.unwrap() });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        control.session_response_started.notified(),
    )
    .await
    .unwrap();
    control.active.store(true, Ordering::SeqCst);
    control.credential_revision.store(1, Ordering::SeqCst);
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    control.release_session_response.notify_one();
    refresh.await.unwrap();
    assert!(
        account.status().await.unwrap().is_some(),
        "the new login is valid and retained"
    );
    assert_ne!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out",
        "a stale 401 response labelled the valid newer login signed_out"
    );
}

#[tokio::test]
async fn account_status_cache_requires_the_current_credential_generation() {
    let (origin, _, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out"
    );
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    control.credential_revision.store(1, Ordering::SeqCst);
    login(&account).await;
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
}
