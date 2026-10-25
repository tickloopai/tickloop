use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness_protocol::{
    ChatId, Content, Decision, EventKind, ExecutionMode, OperationState, PermissionRule, Role,
    SearchConfig, SearchProviderKind, ToolDefinition, ToolResult,
};
use tickloop_harness_runtime::{
    Agent,
    host_tools::{HostToolPermission, HostToolProvider},
};
use tickloop_harness_test_support::{ScriptedModel, call, config, text};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Barrier, Notify},
};
use tokio_util::sync::CancellationToken;

struct ParallelChildren(Barrier);

#[async_trait::async_trait]
impl ModelClient for ParallelChildren {
    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        config().identity()
    }

    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_harness_test_support::title_output(request))
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let child = request.messages.iter().filter(|message| message.role == Role::User).flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text } if text.contains("child-one-work") || text.contains("child-two-work")));
        let content = if child {
            tokio::select! {
                _ = self.0.wait() => {},
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
            text("child completed")
        } else if request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::ToolResult { .. }))
        {
            text("both children completed")
        } else {
            let mut calls = call(
                "child-a",
                "Subagent",
                json!({"name":"one","task":"child-one-work"}),
            );
            calls.extend(call(
                "child-b",
                "Subagent",
                json!({"name":"two","task":"child-two-work"}),
            ));
            calls
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
async fn foreground_subagents_in_the_same_batch_can_work_at_the_same_time() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ParallelChildren(Barrier::new(2))));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Run two independent children".into())
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut reports = std::collections::HashSet::new();
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id != chat {
                continue;
            }
            match event.kind {
                EventKind::ToolCompleted {
                    call_id, result, ..
                } => {
                    assert!(!result.is_error, "{}", result.content);
                    reports.insert(call_id);
                }
                EventKind::Completed { .. } => break reports,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await;
    if result.is_err() {
        agent.interrupt(chat).await.unwrap();
    }
    agent.shutdown().await;
    assert_eq!(
        result.unwrap(),
        std::collections::HashSet::from(["child-a".into(), "child-b".into()])
    );
}

struct HeldReads {
    started: AtomicUsize,
    settled: AtomicUsize,
    changed: Notify,
    barrier: Barrier,
}

#[tokio::test]
async fn denying_a_parallel_read_interrupts_the_run_and_dispatches_no_peers() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("must-not-be-written");
    let reads = Arc::new(HeldReads {
        started: AtomicUsize::new(0),
        settled: AtomicUsize::new(0),
        changed: Notify::new(),
        barrier: Barrier::new(2),
    });
    let mut batch = call("read-a", "ReadFixture", json!({"panic":false}));
    batch.extend(call("read-b", "ReadFixture", json!({"panic":false})));
    batch.extend(call(
        "write",
        "Write",
        json!({"file_path":target,"content":"must not run"}),
    ));
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![batch, text("done")])),
    );
    agent.install_host_tools(reads.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "ReadFixture".into(),
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
        .submit(chat, "Read both before writing".into())
        .await
        .unwrap();
    let mut denied = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } if !denied => {
                    denied = true;
                    agent
                        .answer_permission(chat, request.id, false)
                        .await
                        .unwrap();
                }
                EventKind::Interrupted => break,
                EventKind::Failed(error) => panic!("human denial must interrupt the Run: {error}"),
                EventKind::Completed { .. } => panic!("the denied Run must not complete"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(denied);
    assert_eq!(reads.started.load(Ordering::SeqCst), 0);
    assert!(!target.exists());
    agent.shutdown().await;
}

#[async_trait::async_trait]
impl HostToolProvider for HeldReads {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "ReadFixture".into(),
            description: "A cancellable read".into(),
            input_schema: json!({"type":"object"}),
        }]
    }

    fn is_concurrency_safe(&self, _: &str, _: &Value) -> bool {
        true
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(
        &self,
        _: ChatId,
        _: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        self.barrier.wait().await;
        assert!(input["panic"] != true, "fixture-private-panic");
        cancel.cancelled().await;
        self.settled.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            content: "read cancelled".into(),
            is_error: true,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn cancellation_and_panics_settle_in_flight_reads_without_starting_the_next_write() {
    for panics in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("must-not-be-written");
        let reads = Arc::new(HeldReads {
            started: AtomicUsize::new(0),
            settled: AtomicUsize::new(0),
            changed: Notify::new(),
            barrier: Barrier::new(2),
        });
        let mut batch = call("read-a", "ReadFixture", json!({"panic":panics}));
        batch.extend(call("read-b", "ReadFixture", json!({"panic":false})));
        batch.extend(call(
            "write",
            "Write",
            json!({"file_path":target,"content":"must not run"}),
        ));
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![batch, text("done")])),
        );
        agent.install_host_tools(reads.clone()).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Read both before writing".into())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let changed = reads.changed.notified();
                if reads.started.load(Ordering::SeqCst) == 2 {
                    break;
                }
                changed.await;
            }
        })
        .await
        .unwrap();
        if !panics {
            agent.interrupt(chat).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.unwrap().kind {
                    EventKind::Failed(error) if panics => {
                        assert!(!error.contains("fixture-private-panic"));
                        break;
                    }
                    EventKind::Interrupted if !panics => break,
                    EventKind::Completed { .. } => panic!("the run must stop before the write"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            reads.settled.load(Ordering::SeqCst),
            if panics { 1 } else { 2 }
        );
        assert!(!target.exists());
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(
            snapshot
                .operations
                .iter()
                .all(|operation| operation.state != OperationState::Proposed)
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn consecutive_searches_overlap_and_writes_separate_the_batches() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("between-batches.txt");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/search", listener.local_addr().unwrap());
    let observations = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(2));
    let server = {
        let observations = observations.clone();
        let target = target.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let observations = observations.clone();
                let target = target.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 1024];
                    loop {
                        let count = stream.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                            break;
                        }
                    }
                    observations.lock().unwrap().push(target.exists());
                    barrier.wait().await;
                    let body = r#"{"results":[]}"#;
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                });
            }
        })
    };
    let mut batch = Vec::new();
    for id in ["before-a", "before-b"] {
        batch.extend(call(id, "WebSearch", json!({"query":id})));
    }
    batch.extend(call(
        "write",
        "Write",
        json!({"file_path":target,"content":"written between read batches"}),
    ));
    for id in ["after-a", "after-b"] {
        batch.extend(call(id, "WebSearch", json!({"query":id})));
    }
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![batch, text("done")])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .set_search(
            chat,
            Some(SearchConfig {
                provider: SearchProviderKind::Searxng,
                endpoint,
                api_key_env: None,
            }),
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Search, write the marker, then search again".into())
        .await
        .unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                EventKind::PermissionRequested { request } => {
                    panic!("unexpected permission: {request:?}")
                }
                _ => {}
            }
        }
    })
    .await;
    if completed.is_err() {
        agent.interrupt(chat).await.unwrap();
    }
    agent.shutdown().await;
    server.abort();
    assert!(
        completed.is_ok(),
        "both searches must enter the server before either can finish"
    );
    assert_eq!(*observations.lock().unwrap(), [false, false, true, true]);
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "written between read batches"
    );
}
