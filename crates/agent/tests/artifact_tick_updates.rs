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
            let user = json!({"id":if changes.load(Ordering::SeqCst) >= 20 {"owner-two"} else {"owner"},"name":"Owner","email":"owner@example.test"});
            let body = if request.starts_with("POST /api/v1/auth/device ") {
                json!({"device_code":"device","user_code":"CODE","verification_uri":format!("{endpoint}/device?code=CODE"),"expires_in":60,"interval":1})
            } else if request.starts_with("POST /api/v1/auth/device/token ") {
                json!({"access_token":if changes.load(Ordering::SeqCst) >= 20 {"test-token-two"} else {"test-token"},"token_type":"Bearer","expires_in":600,"user":user})
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
                        json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"tickloop-docs","version":"1"}}})
                    }
                    "tools/list" => json!({"tools":[
                        {"name":"ArtifactRead","description":"Read artifact","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["list","read","threads"]},"artifact_id":{"type":"string"}},"required":["action"],"additionalProperties":false},"annotations":{"readOnlyHint":true}},
                        {"name":"ArtifactEdit","description":"Edit artifact","inputSchema":{"type":"object","properties":{"action":{"type":"string"},"artifact_id":{"type":"string"}},"required":["action"]},"annotations":{"readOnlyHint":false}},
                        {"name":"ArtifactComment","description":"Comment on artifact","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":false}}
                    ]}),
                    "tools/call" => {
                        assert_eq!(rpc["params"]["name"], "ArtifactRead");
                        assert_eq!(rpc["params"]["arguments"]["action"], "read");
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
                json!({"id":"doc","title":"Artifact updates","format":"markdown","access_role":"owner","human_sequence":sequence,"human_updated_at":format!("2026-10-06T04:00:{sequence:02}.000Z"),"human_change":if change == 1 {"feedback"} else {"edit"},"human_body_sequence":body_sequence,"human_body_updated_at":format!("2026-10-06T04:00:{body_sequence:02}.000Z"),"human_body_change":"edit"})
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
    stream.set_watch_authorities(BTreeMap::from([(
        watch,
        tickloop_agent::host_tools::HostToolAuthority::Mcp {
            server_id: origin.clone(),
            account_id: "owner".into(),
        },
    )]));
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
    feedback.store(0, Ordering::SeqCst);
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
    assert_eq!(value.data["change"], "edit");
}
#[tokio::test]
async fn subscribed_artifact_baselines_each_watch_polls_at_cadence_and_preserves_feedback_path() {
    let (origin, sequence, feedback, reads) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let stream = ArtifactTickStream::new(directory.path().canonicalize().unwrap());
    let watch = Uuid::new_v4();
    stream.set_watch_authorities(BTreeMap::from([(
        watch,
        tickloop_agent::host_tools::HostToolAuthority::Mcp {
            server_id: origin.clone(),
            account_id: "owner".into(),
        },
    )]));
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
    stream.set_watch_authorities(BTreeMap::from([
        (
            watch,
            tickloop_agent::host_tools::HostToolAuthority::Mcp {
                server_id: origin.clone(),
                account_id: "owner".into(),
            },
        ),
        (
            late,
            tickloop_agent::host_tools::HostToolAuthority::Mcp {
                server_id: origin.clone(),
                account_id: "owner".into(),
            },
        ),
    ]));
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
async fn artifact_subscription_preserves_native_mcp_read_policy() {
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
                "ArtifactRead",
                json!({"action":"read","artifact_id":"doc"}),
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
            None,
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
                    tool: "ArtifactRead".into(),
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
                    assert_eq!(request.name, "ArtifactRead");
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
        assert_eq!(completed.len(), 2);
        assert_eq!(asked, if decision == Decision::Ask { 2 } else { 0 });
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
async fn artifact_subscription_preserves_plan_and_final_admission_as_the_native_query() {
    use support::tick_admission_contract::{AdmissionCase, verify_admission_policy};
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
        None,
    ));
    provider.refresh(CancellationToken::new()).await.unwrap();
    agent.install_host_tools(provider).unwrap();
    agent.install_tick_stream(adapters).await;
    let chat = agent.create_chat(root).await.unwrap();
    let queries = [
        ("ArtifactRead", json!({"action":"read","artifact_id":"doc"})),
        (
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
    ];
    let cases = queries
        .into_iter()
        .map(|(name, input)| AdmissionCase {
            plan_denied: false,
            name,
            input,
            policy_name: "ArtifactRead",
            policy_input: json!({"action":"read","artifact_id":"doc"}),
        })
        .collect::<Vec<_>>();
    verify_admission_policy(&agent, &model, chat, &cases).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn delegated_artifact_watch_stays_on_its_admitted_account() {
    verify_delegated_watch_account(false).await;
}

#[tokio::test]
async fn restored_delegated_artifact_watch_stays_on_its_admitted_account() {
    verify_delegated_watch_account(true).await;
}

async fn verify_delegated_watch_account(restore: bool) {
    use tickloop_agent::{
        mcp_tools::McpHostToolProvider, permissions::ExecutionMode, runtime::EventKind,
    };
    let (origin, sequence, _, reads) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let root = directory.path().canonicalize().unwrap();
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call(
            "parent-subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        support::call(
            "delegate",
            "Subagent",
            json!({"task":"Watch the same authorized artifact"}),
        ),
        support::call(
            "child-subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        support::text("Child watching."),
        support::text("Parent watching."),
        support::text("Changed artifact seen."),
        support::text("Changed artifact seen."),
    ]));
    let store = root.join("state");
    let mut agent = Agent::open_embedded(store.clone(), support::config(), model.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            ArtifactClient::for_origin(&root, &origin).unwrap(),
            None,
        )))
        .unwrap();
    agent
        .install_tick_stream(Arc::new(
            TickAdapters::new([
                Arc::new(ArtifactAdapter::new(Some(root.clone()))) as Arc<dyn TickAdapter>
            ])
            .unwrap(),
        ))
        .await;
    let parent = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(parent, None, "Subscribe then delegate watching".into())
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if let EventKind::ToolCompleted { call_id, result } = &event.kind
            && call_id.ends_with("subscribe")
        {
            assert!(!result.is_error, "{}", result.content);
        }
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = agent
        .list_chats()
        .await
        .into_iter()
        .find(|chat| chat.id != parent)
        .unwrap()
        .id;
    assert_eq!(
        agent
            .snapshot(child)
            .await
            .unwrap()
            .delegation
            .unwrap()
            .ceiling
            .host_tools["ArtifactRead"],
        std::collections::BTreeSet::from([tickloop_agent::host_tools::HostToolAuthority::Mcp {
            server_id: origin.clone(),
            account_id: "owner".into()
        }])
    );
    assert_eq!(agent.tick_subscriptions(child).await.unwrap().len(), 1);
    let saved = agent.snapshot(child).await.unwrap().loops[0].clone();
    let authority = saved.trigger.as_ref().unwrap().subscriptions[0]
        .authority
        .clone();
    assert_eq!(
        authority,
        Some(tickloop_agent::host_tools::HostToolAuthority::Mcp {
            server_id: origin.clone(),
            account_id: "owner".into()
        })
    );
    if restore {
        agent.shutdown().await;
    }
    sequence.store(25, Ordering::SeqCst);
    login(&root, &origin).await;
    if restore {
        agent = Agent::open_embedded(store, support::config(), model.clone()).unwrap();
        agent
            .install_host_tools(Arc::new(McpHostToolProvider::new(
                ArtifactClient::for_origin(&root, &origin).unwrap(),
                None,
            )))
            .unwrap();
        agent
            .install_tick_stream(Arc::new(
                TickAdapters::new([
                    Arc::new(ArtifactAdapter::new(Some(root.clone()))) as Arc<dyn TickAdapter>
                ])
                .unwrap(),
            ))
            .await;
        let snapshot = agent.resume_chat(child).await.unwrap();
        assert_eq!(
            snapshot.loops[0].trigger.as_ref().unwrap().subscriptions[0].authority,
            authority
        );
        events = agent.subscribe();
        agent
            .submit(
                child,
                Some(saved.id),
                "Continue the authorized watch".into(),
            )
            .await
            .unwrap();
        loop {
            let event = support::next_event(&mut events).await;
            if event.chat_id == child && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    }
    let reads_before = reads.load(Ordering::SeqCst);
    let child_update = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == child && matches!(event.kind, EventKind::Completed { .. }) {
                return agent
                    .tick_subscriptions(child)
                    .await
                    .unwrap()
                    .remove(0)
                    .last_delivery;
            }
        }
    })
    .await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        reads_before,
        "an active/restored watch issued an Artifact read as the new account"
    );
    agent.shutdown().await;
    assert!(
        child_update.is_err(),
        "Account-two data reached a child frozen to owner: {:?}",
        child_update
    );
}

struct BufferedArtifactStream {
    updates: tickloop_agent::tick_stream::InMemoryTickStream,
    admission: ArtifactTickStream,
}
#[async_trait::async_trait]
impl TickStreamPort for BufferedArtifactStream {
    fn set_watch_authorities(
        &self,
        authorities: BTreeMap<Uuid, tickloop_agent::host_tools::HostToolAuthority>,
    ) {
        self.admission.set_watch_authorities(authorities);
    }
    fn open(&self) -> tickloop_agent::tick_stream::TickStreamHandle {
        self.updates.open()
    }
    async fn prepare_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        input: &serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<Option<tickloop_agent::tick_stream::PreparedTickWatch>, String> {
        self.admission
            .prepare_watch(chat, instance, input, cancel)
            .await
    }
    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&tickloop_agent::host_tools::HostToolAuthority>,
    ) -> Result<(), String> {
        self.admission
            .revalidate_watch(chat, instance, authority)
            .await
    }
}

#[tokio::test]
async fn a_buffered_artifact_update_is_revalidated_before_waking_the_loop() {
    let (origin, sequence, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    login(&root, &origin).await;
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    let updates = tickloop_agent::tick_stream::InMemoryTickStream::new();
    let stream = BufferedArtifactStream {
        updates: updates.clone(),
        admission: ArtifactTickStream::new(root.clone()),
    };
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call(
            "watch",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        support::text("Watching."),
        support::text("Unexpected update."),
    ]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    agent.install_tick_stream(Arc::new(stream)).await;
    let chat = agent.create_chat(root.clone()).await.unwrap();
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
    updates.push(tickloop_agent::tick_stream::TickObservation {
        instance,
        sequence: 13,
        observed_at: "2026-10-07T00:00:00Z".into(),
        dropped: 0,
        data: json!({"change":"edit"}),
        kind: tickloop_agent::tick_stream::ObservationKind::Protocol,
        recover_with: None,
        gap: false,
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while agent.tick_subscriptions(chat).await.unwrap()[0].accumulated == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sequence.store(25, Ordering::SeqCst);
    login(&root, &origin).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "a cached old-account update resumed a Loop after switching accounts"
    );
    assert_eq!(
        agent.tick_subscriptions(chat).await.unwrap()[0].link,
        tickloop_agent::runtime::TickLinkState::IdentityUncertain
    );
    agent.shutdown().await;
}

async fn queued_artifact_update(change_account: bool, invalidate_watch: bool) {
    use tickloop_agent::model::*;
    use tokio::sync::{Notify, mpsc::UnboundedSender};
    struct GatedSiblingModel {
        gate: Arc<Notify>,
        entered: Arc<Notify>,
        calls: std::sync::atomic::AtomicUsize,
        model: Arc<support::ScriptedModel>,
    }
    #[async_trait::async_trait]
    impl ModelClient for GatedSiblingModel {
        async fn generate_title(
            &self,
            request: tickloop_agent::model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
            Ok(support::title_output(request))
        }

        fn identity(&self) -> ModelIdentity {
            self.model.identity()
        }
        async fn complete(
            &self,
            request: ModelRequest,
            events: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 2 {
                self.entered.notify_one();
                self.gate.notified().await;
            }
            self.model.complete(request, events, cancel).await
        }
    }
    let (origin, sequence, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    login(&root, &origin).await;
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    let updates = tickloop_agent::tick_stream::InMemoryTickStream::new();
    let stream = BufferedArtifactStream {
        updates: updates.clone(),
        admission: ArtifactTickStream::new(root.clone()),
    };
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call(
            "watch",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        support::text("Watching."),
        support::text("Busy sibling done."),
        support::text("Unexpected queued update."),
    ]));
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let agent = Agent::new_embedded(
        support::config(),
        Arc::new(GatedSiblingModel {
            gate: gate.clone(),
            entered: entered.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            model: model.clone(),
        }),
    );
    agent.install_tick_stream(Arc::new(stream)).await;
    let chat = agent.create_chat(root.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let watched = agent
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
    let busy = agent
        .submit(chat, None, "Busy sibling Loop".into())
        .await
        .unwrap();
    assert_ne!(busy.loop_id, watched.loop_id);
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    updates.push(tickloop_agent::tick_stream::TickObservation {
        instance: instance.clone(),
        sequence: 13,
        observed_at: "2026-10-07T00:00:00Z".into(),
        dropped: 0,
        data: json!({"change":"QUEUED_OLD_ACCOUNT_MARKER"}),
        kind: tickloop_agent::tick_stream::ObservationKind::Protocol,
        recover_with: None,
        gap: false,
    });
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.pending_inputs.iter().any(|input| {
                input.receipt.loop_id == watched.loop_id
                    && input
                        .trigger_source
                        .as_ref()
                        .is_some_and(|source| source.source == "tick_subscription")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let saved_authority = agent
        .snapshot(chat)
        .await
        .unwrap()
        .loops
        .into_iter()
        .find(|goal| goal.id == watched.loop_id)
        .unwrap()
        .trigger
        .unwrap()
        .subscriptions[0]
        .authority
        .clone();
    if change_account {
        sequence.store(25, Ordering::SeqCst);
        login(&root, &origin).await;
        let result = ArtifactTickStream::new(root.clone())
            .revalidate_watch(chat, &instance, saved_authority.as_ref())
            .await;
        assert!(result.is_err());
        if invalidate_watch {
            let watch = *updates.watches().keys().next().unwrap();
            updates.emit(TickStreamEvent::State {
                instance: instance.clone(),
                state: tickloop_agent::tick_stream::TickConnectionState::IdentityUncertain,
                audience: vec![watch],
            });
            tokio::time::timeout(Duration::from_secs(3), async {
                while agent.tick_subscriptions(chat).await.unwrap()[0].link
                    != tickloop_agent::runtime::TickLinkState::IdentityUncertain
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    gate.notify_one();
    loop {
        let event = support::next_event(&mut events).await;
        if event.run_id == busy.run_id
            && matches!(
                event.kind,
                tickloop_agent::runtime::EventKind::Completed { .. }
            )
        {
            break;
        }
    }
    if !change_account {
        loop {
            let event = support::next_event(&mut events).await;
            if event.loop_id == watched.loop_id
                && matches!(
                    event.kind,
                    tickloop_agent::runtime::EventKind::Completed { .. }
                )
            {
                break;
            }
        }
    }
    let finished = agent.snapshot(chat).await.unwrap();
    let (received_tick, request_count) = {
        let requests = model.requests.lock().unwrap();
        let received = requests.iter().any(|request| {
            request.messages.iter().any(|message| {
                message.content.iter().any(|content| {
                    matches!(content, Content::Text { text } if text.contains("QUEUED_OLD_ACCOUNT_MARKER"))
                })
            })
        });
        (received, requests.len())
    };
    agent.shutdown().await;
    if invalidate_watch {
        assert!(
            !snapshot
                .pending_inputs
                .iter()
                .any(|input| input.trigger_source.is_some()),
            "invalidated Watch retained a queued Tick input"
        );
    }
    assert_eq!(request_count, if change_account { 3 } else { 4 });
    assert!(finished.pending_inputs.is_empty());
    assert_eq!(
        finished
            .loops
            .iter()
            .find(|goal| goal.id == watched.loop_id)
            .unwrap()
            .state,
        tickloop_agent::runtime::LoopState::WaitingTrigger,
    );
    assert_eq!(
        received_tick, !change_account,
        "queued Tick data resumed the watched Loop after its account authority became invalid"
    );
}

#[tokio::test]
async fn invalidated_artifact_watch_discards_queued_tick_input() {
    queued_artifact_update(true, true).await;
}

#[tokio::test]
async fn queued_artifact_tick_revalidates_authority_at_run_admission() {
    queued_artifact_update(true, false).await;
}

#[tokio::test]
async fn queued_artifact_tick_runs_when_authority_is_unchanged() {
    queued_artifact_update(false, false).await;
}
