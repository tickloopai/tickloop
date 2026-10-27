use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::{Value, json};
use tickloop_code_access::{
    AccessError, AccessErrorCode, AgentAccess, AgentCommand, AgentDetailQuery, AgentFilter,
    CommandEnvelope, CommandResult, ExecutionAvailability, LaunchSpec, ObservationRequest,
    ObservationValue, Page, UserInput,
};
use tickloop_code_client::{LocalAgentClient, worker_link};
use tickloop_code_daemon::{
    DaemonServer, EmbeddedAgentAccess, WorkerFactory, serve_listener, serve_worker_listener,
};
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::Content;

use tickloop_harness::runtime::Agent;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Notify, mpsc::UnboundedSender},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
struct MessageBurstModel {
    parent_ready: Notify,
    send_messages: Notify,
    children_ready: AtomicUsize,
    emit_deltas: Notify,
}

async fn read_json_frame(stream: &mut UnixStream) -> Option<Value> {
    let length = match stream.read_u32().await {
        Ok(length) => length as usize,
        // Linux resets the connection when the peer closes with our unread frames still queued.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return None;
        }
        Err(error) => panic!("{error}"),
    };
    assert!(length <= tickloop_code_access::AccessLimits::DEFAULT.frame_bytes());
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await.unwrap();
    Some(serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn legacy_wire_schema_is_rejected_before_pipelined_create_can_execute() {
    let directory = tempfile::tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT).unwrap();
    let socket = directory.path().join("worker.sock");
    let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
        tickloop_harness::test_support::text("fixture reply"),
    ]));
    let runtime = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
    let epoch = Uuid::new_v4();
    let access = Arc::new(EmbeddedAgentAccess::for_worker(runtime.clone(), epoch));
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve_worker_listener(
        listener,
        access,
        epoch,
        tickloop_code_access::test_support::TRANSPORT_LIMITS,
    ));
    let verification = async {
        let mut client = UnixStream::connect(socket).await.unwrap();
        let command = CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch_rules: Vec::new(),
                launch: LaunchSpec::embedded(directory.path().into()),
                initial: Some(UserInput::Text {
                    text: "Produce the fixture reply.".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: None,
            },
        };
        let mut pipelined = Vec::new();
        for value in [
            json!({"Hello":{"major":1,"minor":1}}),
            json!({"Execute":{"request_id":Uuid::new_v4(),"command":command}}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            pipelined.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            pipelined.extend_from_slice(&bytes);
        }
        client.write_all(&pipelined).await.unwrap();
        let response = read_json_frame(&mut client).await.unwrap();
        assert_eq!(
            response["Error"]["error"]["code"],
            serde_json::to_value(AccessErrorCode::VersionMismatch).unwrap()
        );
        assert!(read_json_frame(&mut client).await.is_none());
        assert!(runtime.list_chats().await.is_empty());
        assert!(model.requests.lock().unwrap().is_empty());
    };
    let result = std::panic::AssertUnwindSafe(verification)
        .catch_unwind()
        .await;
    runtime.shutdown().await;
    server.abort();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn tool_result(request: &ModelRequest, id: &str) -> Option<Value> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| {
            if let Content::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
                && tool_use_id == id
            {
                serde_json::from_str(&content.text()).ok()
            } else {
                None
            }
        })
}

#[async_trait]
impl ModelClient for MessageBurstModel {
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
        tickloop_harness::test_support::ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let host: Value = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                Content::Text { text } => text
                    .rsplit_once("Current host state: ")
                    .and_then(|(_, value)| serde_json::from_str(value).ok()),
                _ => None,
            })
            .unwrap();
        let content = if let Some(task) = host["delegation"]["task"].as_str() {
            let delivered = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, Content::Text { text } if text.contains("GO-BURST")));
            if delivered {
                let emit = self.emit_deltas.notified();
                tokio::pin!(emit);
                emit.as_mut().enable();
                self.children_ready.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    _ = &mut emit => {},
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                }
                let mut body = String::new();
                for index in 0..1100 {
                    let delta = format!("{task}: delta {index:04}\n");
                    events.send(ModelDelta::Text(delta.clone())).unwrap();
                    body.push_str(&delta);
                }
                tickloop_harness::test_support::text(&body)
            } else if tool_result(&request, "ready").is_none() {
                tickloop_harness::test_support::call(
                    "ready",
                    "SendMessage",
                    json!({"to":"parent","message":format!("READY {task}")}),
                )
            } else {
                tickloop_harness::test_support::text("Ready for the next pushed context.")
            }
        } else if tool_result(&request, "delegate-A").is_none() {
            ["A", "B", "C"]
                .into_iter()
                .flat_map(|label| {
                    tickloop_harness::test_support::call(
                        &format!("delegate-{label}"),
                        "Subagent",
                        json!({"task":format!("Child {label}")}),
                    )
                })
                .collect()
        } else if tool_result(&request, "send-A").is_none() {
            self.parent_ready.notify_one();
            tokio::select! {
                _ = self.send_messages.notified() => {},
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
            ["A", "B", "C"].into_iter().flat_map(|label| {
                tickloop_harness::test_support::call(&format!("send-{label}"), "SendMessage", json!({"to":tool_result(&request, &format!("delegate-{label}")).unwrap()["child_chat_id"],"message":"GO-BURST"}))
            }).collect()
        } else {
            tickloop_harness::test_support::text(
                "Parent received the accepted messages and child results.",
            )
        };
        Ok(ModelOutput {
            content,
            usage: Default::default(),
            stop_reason: "end_turn".into(),
            private_source: None,
        })
    }
}

struct SocketWorkerFactory {
    directory: PathBuf,
    model: Arc<MessageBurstModel>,
    runtime: Mutex<Option<Agent>>,
    servers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[async_trait]
impl WorkerFactory for SocketWorkerFactory {
    async fn start(
        &self,
        _: &LaunchSpec,
        epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        let runtime =
            Agent::new_embedded(tickloop_harness::test_support::config(), self.model.clone());
        *self.runtime.lock().await = Some(runtime.clone());
        let served = Arc::new(EmbeddedAgentAccess::for_worker(runtime, epoch));
        let socket = self.directory.join("worker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        self.servers.lock().await.push(tokio::spawn(async move {
            serve_worker_listener(
                listener,
                served,
                epoch,
                tickloop_code_access::test_support::TRANSPORT_LIMITS,
            )
            .await
            .unwrap();
        }));
        let client = LocalAgentClient::connect(&socket).await?;
        Ok(Arc::new(worker_link(client, socket, || true)))
    }

    async fn exited(&self, _: &Arc<dyn AgentAccess>) -> bool {
        false
    }
}

fn overview_query() -> ObservationRequest {
    ObservationRequest::Overview {
        filter: AgentFilter::default(),
        page: Page {
            offset: 0,
            limit: 256,
        },
        after: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_parent_child_messages_and_burst_feed_cancellation_keep_the_family_live() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let model = Arc::new(MessageBurstModel::default());
    let factory = Arc::new(SocketWorkerFactory {
        directory: directory.path().into(),
        model: model.clone(),
        runtime: Mutex::new(None),
        servers: Mutex::new(Vec::new()),
    });
    let epoch = Uuid::new_v4();
    let daemon = Arc::new(DaemonServer::new(epoch, factory.clone()));
    let listener = UnixListener::bind(directory.path().join("daemon.sock")).unwrap();
    let server = tokio::spawn(serve_listener(
        listener,
        daemon,
        epoch,
        tickloop_code_access::test_support::TRANSPORT_LIMITS,
    ));
    let client = LocalAgentClient::connect(directory.path().join("daemon.sock"))
        .await
        .unwrap();
    let created = client
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch_rules: Vec::new(),
                launch: LaunchSpec::embedded(directory.path().into()),
                initial: Some(UserInput::Text {
                    text: "Delegate three scoped children and send follow-up context.".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created {
        execution: parent, ..
    } = created
    else {
        panic!("create result");
    };
    tokio::time::timeout(Duration::from_secs(5), model.parent_ready.notified())
        .await
        .unwrap();
    let observed = client.observe(overview_query()).await.unwrap();
    let ObservationValue::Overview(rows) = observed.initial else {
        panic!("directory");
    };
    assert_eq!(rows.len(), 4);
    let targets = rows.iter().map(|row| row.execution).collect::<Vec<_>>();
    let mut feeds = Vec::new();
    for execution in &targets {
        feeds.push(
            client
                .observe(ObservationRequest::Agent {
                    execution: *execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap(),
        );
    }
    model.send_messages.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.children_ready.load(Ordering::SeqCst) != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before = client
        .observe(ObservationRequest::Agent {
            execution: parent,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(snapshot) = before.initial else {
        panic!("parent snapshot");
    };
    let accepted = snapshot
        .events
        .iter()
        .filter(|event| {
            matches!(&event.kind,
        tickloop_harness::protocol::EventKind::ToolCompleted { call_id, result, .. }
        if call_id.starts_with("send-") && !result.is_error && result.content.contains("accepted"))
        })
        .count();
    assert_eq!(
        accepted, 3,
        "actual SendMessage tools must be accepted before the stress"
    );
    model.emit_deltas.notify_waiters();
    let mut completed_rounds = 0;
    let mut stage = "starting feed churn".to_owned();
    // Forty rounds of feed churn; on a machine busy with parallel tests they took over 15 s.
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        for round in 0..40 {
            for execution in &targets {
                stage = format!("round {}/40: Snapshot for {execution:?}", round + 1);
                drop(
                    client
                        .observe(ObservationRequest::Agent {
                            execution: *execution,
                            detail: AgentDetailQuery::Snapshot,
                            after: None,
                        })
                        .await
                        .unwrap(),
                );
            }
            stage = format!("round {}/40: Overview", round + 1);
            let ObservationValue::Overview(rows) =
                client.observe(overview_query()).await.unwrap().initial
            else {
                panic!("directory");
            };
            assert_eq!(rows.len(), 4);
            assert!(
                rows.iter()
                    .all(|row| row.availability == ExecutionAvailability::Live),
                "accepted messages caused family Lost: {rows:?}"
            );
            completed_rounds = round + 1;
        }
        for execution in &targets {
            stage = format!("final Snapshot for {execution:?}");
            let ObservationValue::Agent(_) = client
                .observe(ObservationRequest::Agent {
                    execution: *execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap()
                .initial
            else {
                panic!("snapshot");
            };
        }
    })
    .await;
    drop(feeds.pop());
    drop(feeds);
    let runtime = factory.runtime.lock().await.take().unwrap();
    runtime.shutdown().await;
    for task in factory.servers.lock().await.drain(..) {
        task.abort();
    }
    server.abort();
    result.unwrap_or_else(|_| {
        panic!(
            "feed churn timed out after {completed_rounds}/40 completed rounds; awaiting {stage}"
        )
    });
}
