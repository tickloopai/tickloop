mod support;

use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tickloop_agent::{
    artifacts::ArtifactClient,
    runtime::Agent,
    tick_adapter::{ArtifactAdapter, TickAdapter, TickAdapters},
    tick_stream::{ArtifactTickStream, InstanceRef, TickStreamEvent, TickStreamPort},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn service() -> (String, Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let sequence = Arc::new(AtomicU64::new(12));
    let feedback = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicU64::new(0));
    let (changes, kind, read_count, endpoint) = (
        sequence.clone(),
        feedback.clone(),
        reads.clone(),
        origin.clone(),
    );
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    return;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&bytes);
            let user = json!({"id":"owner","name":"Owner","email":"owner@example.test"});
            let body = if request.starts_with("POST /api/v1/auth/device ") {
                json!({"device_code":"device","user_code":"CODE","verification_uri":format!("{endpoint}/device?code=CODE"),"expires_in":60,"interval":1})
            } else if request.starts_with("POST /api/v1/auth/device/token ") {
                json!({"access_token":"test-token","token_type":"Bearer","expires_in":600,"user":user})
            } else if request.starts_with("GET /api/v1/auth/session ") {
                json!({"user":user,"expires_at":"2099-01-01T00:00:00Z"})
            } else if request.starts_with("POST /api/v1/auth/logout ") {
                json!({"revoked":true})
            } else if request.starts_with("POST /mcp ") {
                let rpc: serde_json::Value =
                    serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                if rpc.get("id").is_none() {
                    socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    continue;
                }
                let result = match rpc["method"].as_str().unwrap_or_default() {
                    "server/discover" => {
                        json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
                    }
                    "initialize" => {
                        json!({"protocolVersion":"2026-07-28","capabilities":{"tools":{}},"serverInfo":{"name":"mock-artifacts","version":"1"}})
                    }
                    "tools/list" => json!({"tools":[
                        {"name":"artifact_read","description":"Read artifact","inputSchema":{"type":"object","properties":{"artifact_id":{"type":"string"},"version_id":{"type":"string"}},"required":["artifact_id"],"additionalProperties":false},"annotations":{"readOnlyHint":true}},
                        {"name":"artifact_publish","description":"Publish artifact","inputSchema":{"type":"object","properties":{"artifact_id":{"type":"string"}},"required":["artifact_id"]},"annotations":{"readOnlyHint":false}}
                    ]}),
                    "tools/call" => {
                        assert_eq!(rpc["params"]["name"], "artifact_read");
                        assert_eq!(rpc["params"]["arguments"]["artifact_id"], "doc");
                        json!({"content":[{"type":"text","text":"# Recovered artifact body"}],"isError":false})
                    }
                    _ => json!({}),
                };
                json!({"jsonrpc":"2.0","id":rpc["id"],"result":result})
            } else {
                read_count.fetch_add(1, Ordering::SeqCst);
                let sequence = changes.load(Ordering::SeqCst);
                let change = kind.load(Ordering::SeqCst);
                let body_sequence = sequence - u64::from(change == 1);
                json!({"id":"doc","title":"Artifact updates","format":"markdown","access_role":"owner","human_sequence":sequence,"human_updated_at":format!("2026-10-06T04:00:{sequence:02}.000Z"),"human_change":if change == 1 {"feedback"} else if change == 2 {"publish"} else {"edit"},"human_version_id":null,"human_body_sequence":body_sequence,"human_body_updated_at":format!("2026-10-06T04:00:{body_sequence:02}.000Z"),"human_body_change":if change == 2 {"publish"} else {"edit"},"human_body_version_id":if change == 2 {Some("version-15")} else {None}})
            };
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (origin, sequence, feedback, reads)
}
async fn login(root: &std::path::Path, origin: &str) {
    let client = ArtifactClient::for_origin(root.canonicalize().unwrap(), origin).unwrap();
    let device = client.start_device_login().await.unwrap();
    client
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();
}
async fn ready(events: &mut tokio::sync::mpsc::Receiver<TickStreamEvent>, watch: Uuid) {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            TickStreamEvent::Ready { watch: received } if received == watch => return,
            TickStreamEvent::Observation { .. } => {
                panic!("existing content was delivered instead of baselined")
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn feedback_between_polls_does_not_swallow_the_latest_undelivered_body_change() {
    let (origin, sequence, feedback, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let stream = ArtifactTickStream::new(directory.path().canonicalize().unwrap());
    let watch = Uuid::new_v4();
    stream.set_cadences(BTreeMap::from([(watch, 5)]));
    let mut handle = stream.open();
    handle.desired.send_replace(BTreeMap::from([(
        watch,
        InstanceRef::artifact(&origin, "doc").unwrap(),
    )]));
    ready(&mut handle.events, watch).await;
    feedback.store(1, Ordering::SeqCst);
    sequence.store(14, Ordering::SeqCst);
    let value = tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            if let TickStreamEvent::Observation { value, .. } = handle.events.recv().await.unwrap()
            {
                break value;
            }
        }
    })
    .await
    .expect("edit 13 was swallowed by feedback 14");
    assert_eq!(value.sequence, 13);
    assert_eq!(value.data["change"], "edit");
    assert_eq!(value.observed_at, "2026-10-06T04:00:13.000Z");
    feedback.store(2, Ordering::SeqCst);
    sequence.store(15, Ordering::SeqCst);
    let value = tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            if let TickStreamEvent::Observation { value, .. } = handle.events.recv().await.unwrap()
            {
                break value;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(value.sequence, 15);
    assert_eq!(value.data["change"], "publish");
    assert_eq!(value.data["version_id"], "version-15");
}
#[tokio::test]
async fn subscribed_artifact_baselines_each_watch_polls_at_cadence_and_preserves_feedback_path() {
    let (origin, sequence, feedback, reads) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let stream = ArtifactTickStream::new(directory.path().canonicalize().unwrap());
    let watch = Uuid::new_v4();
    stream.set_cadences(BTreeMap::from([(watch, 5)]));
    let mut handle = stream.open();
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    handle
        .desired
        .send_replace(BTreeMap::from([(watch, instance.clone())]));
    ready(&mut handle.events, watch).await;
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    sequence.store(13, Ordering::SeqCst);
    let value = loop {
        if let TickStreamEvent::Observation { value, audience } =
            tokio::time::timeout(Duration::from_secs(7), handle.events.recv())
                .await
                .unwrap()
                .unwrap()
        {
            assert_eq!(audience, vec![watch]);
            break value;
        }
    };
    assert_eq!(value.sequence, 13);
    assert_eq!(value.data["change"], "edit");
    assert!(value.data.get("source").is_none());
    let late = Uuid::new_v4();
    handle.desired.send_replace(BTreeMap::from([
        (watch, instance.clone()),
        (late, instance),
    ]));
    ready(&mut handle.events, late).await;
    feedback.store(1, Ordering::SeqCst);
    sequence.store(14, Ordering::SeqCst);
    let metadata = loop {
        match tokio::time::timeout(Duration::from_secs(7), handle.events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            TickStreamEvent::Metadata { metadata, .. }
                if metadata.updated_at_ms == Some(1791259214000) =>
            {
                break metadata;
            }
            TickStreamEvent::Observation { .. } => {
                panic!("feedback must stay on its association path")
            }
            _ => {}
        }
    };
    assert_eq!(metadata.updated_at_ms, Some(1791259214000));
}

#[tokio::test]
async fn artifact_subscription_reaches_loop_and_quiet_polling_does_not_call_the_model() {
    let (origin, sequence, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        support::text("watching"),
        support::text("human edit observed"),
    ]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    agent
        .install_tick_stream(Arc::new(
            TickAdapters::new([Arc::new(ArtifactAdapter::new(Some(
                directory.path().canonicalize().unwrap(),
            ))) as Arc<dyn TickAdapter>])
            .unwrap(),
        ))
        .await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Watch this artifact".into())
        .await
        .unwrap();
    loop {
        if matches!(
            support::next_event(&mut events).await.kind,
            tickloop_agent::runtime::EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    tokio::time::sleep(Duration::from_millis(5200)).await;
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    sequence.store(13, Ordering::SeqCst);
    loop {
        let event = tokio::time::timeout(Duration::from_secs(12), events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            event.kind,
            tickloop_agent::runtime::EventKind::Completed { .. }
        ) {
            break;
        }
    }
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            serde_json::to_string(&requests[2].messages)
                .unwrap()
                .contains("human_sequence")
        );
    }
    let subscription = agent.tick_subscriptions(chat).await.unwrap().remove(0);
    agent.stop(chat, subscription.loop_id).await.unwrap();
    assert!(agent.tick_subscriptions(chat).await.unwrap().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn artifact_adapter_catalog_uses_mcp_queries_and_keeps_identity_and_credentials_bound() {
    let (origin, _, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    let adapters = TickAdapters::new([Arc::new(ArtifactAdapter::new(Some(
        directory.path().canonicalize().unwrap(),
    ))) as Arc<dyn TickAdapter>])
    .unwrap();
    let tools = adapters.host_tools(None);
    let chat = Uuid::new_v4();
    let prepared = tools
        .prepare_call(
            chat,
            "ReadTick",
            &json!({"tick_instance_id":instance}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let read = tools
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await
        .into_tool_result();
    assert!(!read.is_error, "{}", read.content);
    let data: serde_json::Value = serde_json::from_str(&read.content).unwrap();
    assert!(
        data["content"]
            .as_str()
            .unwrap()
            .contains("Recovered artifact body")
    );
    assert_eq!(data["tools"].as_array().unwrap().len(), 1);
    assert_eq!(data["tools"][0]["name"], "artifact_read");
    assert!(
        data["tools"][0]["input_schema"]["properties"]
            .get("artifact_id")
            .is_none()
    );
    assert!(tools.prepare_call(chat,"CallTickTool",&json!({"tick_instance_id":instance,"tool":"artifact_read","args":{"artifact_id":"another"}}),CancellationToken::new()).await.is_err());
    assert!(
        tools
            .prepare_call(
                chat,
                "CallTickTool",
                &json!({"tick_instance_id":instance,"tool":"artifact_publish","args":{}}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    let query = tools.prepare_call(chat,"CallTickTool",&json!({"tick_instance_id":instance,"tool":"artifact_read","args":{"version_id":"version-1"}}),CancellationToken::new()).await.unwrap();
    assert_eq!(
        query.permission(),
        tickloop_agent::host_tools::HostToolPermission::ReadOnly
    );
    let result = tools
        .execute_prepared(chat, query, CancellationToken::new())
        .await
        .into_tool_result();
    assert!(!result.is_error, "{}", result.content);
    let stale = tools
        .prepare_call(
            chat,
            "CallTickTool",
            &json!({"tick_instance_id":instance,"tool":"artifact_read","args":{}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin)
        .unwrap()
        .logout()
        .await
        .unwrap();
    assert!(matches!(
        tools
            .execute_prepared(chat, stale, CancellationToken::new())
            .await,
        tickloop_agent::host_tools::HostCallResult::NotDispatched(_)
    ));
}

#[tokio::test]
async fn artifact_aliases_preserve_underlying_read_policy_for_queries_and_subscription() {
    use tickloop_agent::{
        host_tools::HostToolProvider,
        permissions::{Decision, ExecutionMode, PermissionRule},
        runtime::EventKind,
    };
    for decision in [Decision::Deny, Decision::Ask] {
        let (origin, _, _, _) = service().await;
        let directory = tempfile::tempdir().unwrap();
        login(directory.path(), &origin).await;
        let root = directory.path().canonicalize().unwrap();
        let instance = InstanceRef::artifact(&origin, "doc").unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::call(
                "direct",
                "mcp__artifacts__artifact_read",
                json!({"artifact_id":"doc"}),
            ),
            support::call("read", "ReadTick", json!({"tick_instance_id":instance})),
            support::call(
                "query",
                "CallTickTool",
                json!({"tick_instance_id":instance,"tool":"artifact_read","args":{}}),
            ),
            support::call(
                "subscribe",
                "SubscribeTick",
                json!({"tick_instance_id":instance,"every_seconds":5}),
            ),
            support::text("done"),
        ]));
        let agent = Agent::new_embedded(support::config(), model.clone());
        let adapters = Arc::new(
            TickAdapters::new([
                Arc::new(ArtifactAdapter::new(Some(root.clone()))) as Arc<dyn TickAdapter>
            ])
            .unwrap(),
        );
        let provider = Arc::new(tickloop_agent::mcp_tools::McpHostToolProvider::new(
            ArtifactClient::for_origin(&root, &origin).unwrap(),
            Some(adapters.host_tools(None)),
        ));
        provider.refresh(CancellationToken::new()).await.unwrap();
        agent.install_host_tools(provider).unwrap();
        agent.install_tick_stream(adapters).await;
        let chat = agent.create_chat(root).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision,
                    tool: "mcp__artifacts__artifact_read".into(),
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
            .submit(chat, None, "read and watch this artifact".into())
            .await
            .unwrap();
        let mut asked = 0;
        let mut completed = Vec::new();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    assert_eq!(request.name, "mcp__artifacts__artifact_read");
                    asked += 1;
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted { call_id, result } => completed.push((call_id, result)),
                EventKind::Completed { .. } => break,
                _ => {}
            }
        }
        assert_eq!(completed.len(), 4);
        assert_eq!(asked, if decision == Decision::Ask { 4 } else { 0 });
        for (id, result) in completed {
            assert_eq!(
                result.is_error,
                decision == Decision::Deny,
                "{id}: {}",
                result.content
            );
        }
        assert_eq!(
            agent.tick_subscriptions(chat).await.unwrap().len(),
            usize::from(decision == Decision::Ask)
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn artifact_aliases_preserve_plan_and_final_admission_as_the_direct_query() {
    use support::tick_alias_contract::{AliasCase, verify_alias_policy};
    use tickloop_agent::host_tools::HostToolProvider;
    let (origin, _, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let root = directory.path().canonicalize().unwrap();
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    let model = Arc::new(support::ScriptedModel::new(vec![]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    let adapters = Arc::new(
        TickAdapters::new([
            Arc::new(ArtifactAdapter::new(Some(root.clone()))) as Arc<dyn TickAdapter>
        ])
        .unwrap(),
    );
    let provider = Arc::new(tickloop_agent::mcp_tools::McpHostToolProvider::new(
        ArtifactClient::for_origin(&root, &origin).unwrap(),
        Some(adapters.host_tools(None)),
    ));
    provider.refresh(CancellationToken::new()).await.unwrap();
    agent.install_host_tools(provider).unwrap();
    agent.install_tick_stream(adapters).await;
    let chat = agent.create_chat(root).await.unwrap();
    let queries = [
        (
            "mcp__artifacts__artifact_read",
            json!({"artifact_id":"doc"}),
        ),
        ("ReadTick", json!({"tick_instance_id":instance})),
        (
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"artifact_read","args":{}}),
        ),
        (
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
    ];
    let cases = queries
        .into_iter()
        .map(|(name, input)| AliasCase {
            plan_denied: false,
            name,
            input,
            policy_name: "mcp__artifacts__artifact_read",
            policy_input: json!({"artifact_id":"doc"}),
        })
        .collect::<Vec<_>>();
    verify_alias_policy(&agent, &model, chat, &cases).await;
    agent.shutdown().await;
}
