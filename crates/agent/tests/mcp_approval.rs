use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use serde_json::{Value, json};
use tickloop_agent::{
    artifacts::ArtifactClient,
    host_tools::{HostCallResult, HostToolProvider},
    mcp_tools::McpHostToolProvider,
    permissions::{Decision, PermissionRule},
    runtime::{Agent, ChatId, DenialRuleSource, EventKind, LoopState},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod support;
use support::{ScriptedModel, call, config, text};

#[derive(Default)]
struct ServiceControl {
    account: AtomicUsize,
    block_discovery: AtomicBool,
    discovery_entered: Notify,
    release_discovery: Notify,
    block_call_response: AtomicBool,
    call_entered: Notify,
    release_call_response: Notify,
    requests: Mutex<Vec<Value>>,
}

async fn service() -> (String, Arc<ServiceControl>) {
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
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
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
                let head = String::from_utf8_lossy(&bytes[..header_end]);
                let path = head.lines().next().unwrap_or("").to_owned();
                let body: Value =
                    serde_json::from_slice(&bytes[header_end + 4..]).unwrap_or(Value::Null);
                control
                    .requests
                    .lock()
                    .unwrap()
                    .push(json!({"path": path, "body": body}));
                if body["method"] == "server/discover"
                    && control.block_discovery.load(Ordering::SeqCst)
                {
                    control.discovery_entered.notify_one();
                    control.release_discovery.notified().await;
                }
                if body["method"] == "tools/call"
                    && control.block_call_response.load(Ordering::SeqCst)
                {
                    control.call_entered.notify_one();
                    control.release_call_response.notified().await;
                }
                let account = control.account.load(Ordering::SeqCst);
                let response = if path.starts_with("POST /api/v1/auth/device ") {
                    json!({"device_code":"test-device","user_code":"TEST","verification_uri":format!("{origin}/device?code=TEST"),"expires_in":30,"interval":1})
                } else if path.starts_with("POST /api/v1/auth/device/token ") {
                    json!({"access_token":format!("token-{account}"),"token_type":"Bearer","expires_in":60,"user":{"id":format!("user-{account}"),"name":"Test","email":"test@example.test"}})
                } else if path.starts_with("GET /api/v1/auth/session ") {
                    json!({"user":{"id":format!("user-{account}"),"name":"Test","email":"test@example.test"},"expires_at":"2099-01-01T00:00:00Z"})
                } else if path.starts_with("POST /mcp ") {
                    let result = match body["method"].as_str().unwrap_or("") {
                        "server/discover" => {
                            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
                        }
                        "tools/list" => {
                            json!({"tools":[{"name":"mutate","inputSchema":{"type":"object","additionalProperties":true}}]})
                        }
                        "tools/call" => {
                            json!({"content":[],"structuredContent":{"status":"committed"}})
                        }
                        method => panic!("unexpected MCP method: {method}"),
                    };
                    json!({"jsonrpc":"2.0","id":body["id"],"result":result})
                } else {
                    json!({"error":"not_found"})
                };
                let response = response.to_string();
                let message = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                stream.write_all(message.as_bytes()).await.unwrap();
            });
        }
    });
    (origin, control)
}

async fn login(client: &ArtifactClient) {
    let device = client.start_device_login().await.unwrap();
    client
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();
}

async fn completed_without_permission_request(
    agent: &Agent,
    chat: ChatId,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|loop_| loop_.state == LoopState::Completed)
            {
                return;
            }
            if let Ok(event) = events.try_recv() {
                assert!(
                    !matches!(event.kind, EventKind::PermissionRequested { .. }),
                    "exact permission rule was ignored"
                );
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn prepared_approval_shows_every_short_argument_and_bounds_private_content() {
    let (origin, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let source = "private body ".repeat(400);
    let input = json!({
        "mode":"fixed",
        "version_id":"version-7",
        "comment_text":"Approve this short correction",
        "share_token":"capability-private-value",
        "limits":{"enabled":true,"max_comments":3},
        "source":source,
    });
    let prepared = provider
        .prepare_call(
            ChatId::new_v4(),
            "mcp__artifacts__mutate",
            &input,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let review = prepared.input();
    assert_eq!(review["mode"], "fixed");
    assert_eq!(review["version_id"], "version-7");
    assert_eq!(review["comment_text"], "Approve this short correction");
    assert_eq!(review["limits"], json!({"enabled":true,"max_comments":3}));
    assert_eq!(review["source"]["bytes"], 5200);
    assert_eq!(review["source"]["truncated"], true);
    assert!(review["source"]["preview"].as_str().unwrap().len() <= 200);
    assert_eq!(review["source"]["sha256"].as_str().unwrap().len(), 64);
    assert!(!review.to_string().contains("capability-private-value"));
    assert!(review.to_string().len() < 2_000);
    assert_eq!(prepared.source_input(), &input);
}

#[tokio::test]
async fn account_change_during_mcp_handshake_prevents_old_account_mutation() {
    let (origin, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let provider = Arc::new(McpHostToolProvider::new(account, None));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let prepared = provider
        .prepare_call(
            ChatId::new_v4(),
            "mcp__artifacts__mutate",
            &json!({"mode":"fixed"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    control.block_discovery.store(true, Ordering::SeqCst);
    let dispatch_provider = provider.clone();
    let dispatch = tokio::spawn(async move {
        dispatch_provider
            .execute_prepared(ChatId::new_v4(), prepared, CancellationToken::new())
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        control.discovery_entered.notified(),
    )
    .await
    .unwrap();
    control.account.store(1, Ordering::SeqCst);
    let new_account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&new_account).await;
    control.release_discovery.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), dispatch)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, HostCallResult::NotDispatched(_)));
    assert_eq!(
        control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .count(),
        0
    );
}

#[tokio::test]
async fn agent_approval_and_operation_record_keep_bounded_review_arguments() {
    let (origin, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let source = "private body ".repeat(400);
    let model = Arc::new(ScriptedModel::new(vec![call(
        "mutation",
        "mcp__artifacts__mutate",
        json!({"mode":"fixed","version_id":"version-7","comment_text":"Short reply","share_token":"capability-private-value","source":source}),
    )]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Update the artifact".into())
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
    assert_eq!(permission.input["mode"], "fixed");
    assert_eq!(permission.input["version_id"], "version-7");
    assert_eq!(permission.input["comment_text"], "Short reply");
    let snapshot = agent.snapshot(chat).await.unwrap();
    let operation = snapshot.operations.last().unwrap();
    assert_eq!(operation.input, permission.input);
    let persisted = serde_json::to_string(operation).unwrap();
    assert!(!persisted.contains("capability-private-value"));
    assert!(!persisted.contains(&"private body ".repeat(400)));
    assert!(
        control
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request["body"]["method"] != "tools/call")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn account_change_after_tool_reaches_server_is_outcome_unknown() {
    let (origin, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let provider = Arc::new(McpHostToolProvider::new(account, None));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let prepared = provider
        .prepare_call(
            ChatId::new_v4(),
            "mcp__artifacts__mutate",
            &json!({"mode":"fixed"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    control.block_call_response.store(true, Ordering::SeqCst);
    let dispatch_provider = provider.clone();
    let dispatch = tokio::spawn(async move {
        dispatch_provider
            .execute_prepared(ChatId::new_v4(), prepared, CancellationToken::new())
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        control.call_entered.notified(),
    )
    .await
    .unwrap();
    control.account.store(1, Ordering::SeqCst);
    let new_account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&new_account).await;
    control.release_call_response.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), dispatch)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, HostCallResult::OutcomeUnknown(_)));
    assert_eq!(
        control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .count(),
        1
    );
}

#[tokio::test]
async fn exact_deny_rule_matches_frozen_arguments_even_when_review_redacts_them() {
    let (origin, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let input = json!({"mode":"fixed","share_token":"secret-capability","source":"private body ".repeat(400)});
    let model = Arc::new(ScriptedModel::new(vec![
        call("mutation", "mcp__artifacts__mutate", input.clone()),
        text("Denied"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    let rule_id = Uuid::new_v4();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: rule_id,
                decision: Decision::Deny,
                tool: "mcp__artifacts__mutate".into(),
                specifier: None,
                input: Some(input),
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Do the mutation".into())
        .await
        .unwrap();
    completed_without_permission_request(&agent, chat, &mut events).await;
    let denial = agent
        .recent_denials(chat)
        .await
        .unwrap()
        .records
        .pop()
        .unwrap();
    assert!(
        denial
            .matching_rules
            .iter()
            .any(|rule| { rule.id == rule_id && rule.source == DenialRuleSource::ChatSession })
    );
    assert!(
        !serde_json::to_string(&denial)
            .unwrap()
            .contains("secret-capability")
    );
    assert_eq!(
        control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .count(),
        0
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn exact_allow_rule_survives_final_admission_with_bounded_operation_record() {
    let (origin, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let input = json!({"mode":"fixed","share_token":"secret-capability","source":"private body ".repeat(400)});
    let model = Arc::new(ScriptedModel::new(vec![
        call("mutation", "mcp__artifacts__mutate", input.clone()),
        text("Done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let chat = agent.create_chat(root).await.unwrap();
    let rule_id = Uuid::new_v4();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: rule_id,
                decision: Decision::Allow,
                tool: "mcp__artifacts__mutate".into(),
                specifier: None,
                input: Some(input),
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Do the mutation".into())
        .await
        .unwrap();
    completed_without_permission_request(&agent, chat, &mut events).await;
    let approval = agent
        .approval_history_view(chat)
        .await
        .unwrap()
        .records
        .pop()
        .unwrap();
    assert!(
        approval
            .matching_rules
            .iter()
            .any(|rule| { rule.id == rule_id && rule.source == DenialRuleSource::ChatSession })
    );
    assert!(
        !serde_json::to_string(&approval)
            .unwrap()
            .contains("secret-capability")
    );
    assert_eq!(
        control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .count(),
        1
    );
    let operation = agent
        .snapshot(chat)
        .await
        .unwrap()
        .operations
        .pop()
        .unwrap();
    assert!(
        !serde_json::to_string(&operation)
            .unwrap()
            .contains("secret-capability")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn deeply_structured_private_input_cannot_fill_the_operation_record() {
    let (origin, control) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let account = ArtifactClient::for_origin(&root, &origin).unwrap();
    login(&account).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let fields = (0..64)
        .map(|index| {
            (
                format!("field_{index}"),
                json!(vec!["private".repeat(16); 64]),
            )
        })
        .collect();
    let input = Value::Object(fields);
    assert!(
        provider
            .prepare_call(
                ChatId::new_v4(),
                "mcp__artifacts__mutate",
                &input,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(
        control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .count(),
        0
    );
}
