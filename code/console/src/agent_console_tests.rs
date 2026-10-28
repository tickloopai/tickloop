use std::sync::Arc;

use tokio::net::UnixListener;
use uuid::Uuid;

use super::{AgentConsole, AgentState};
use tickloop_code_access::{
    AgentSummary, ClientMessage, ExecutionAvailability, ExecutionRef, HANDSHAKE_FRAME_BYTES,
    PROTOCOL_MAJOR, PROTOCOL_MINOR, ServerHello, ServerMessage,
};
use tickloop_code_client::{
    LocalAgentClient,
    frame::{read_frame, write_frame},
};
use tickloop_harness::protocol::LoopState;

use tickloop_code_access::test_support::TRANSPORT_LIMITS;

const FRAME_BYTES: usize = TRANSPORT_LIMITS.frame_bytes();

struct UnknownCreation {
    epoch: Uuid,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl tickloop_code_access::AgentAccess for UnknownCreation {
    async fn observe(
        &self,
        query: tickloop_code_access::ObservationRequest,
    ) -> Result<tickloop_code_access::Observation, tickloop_code_access::AccessError> {
        assert!(matches!(
            query,
            tickloop_code_access::ObservationRequest::Receipt { .. }
        ));
        Ok(tickloop_code_access::Observation::single(
            tickloop_code_access::ObservationValue::Receipt(None),
            tickloop_code_access::Cursor {
                daemon_epoch: self.epoch,
                execution: None,
                sequence: 0,
            },
            None,
        ))
    }

    async fn execute(
        &self,
        _: tickloop_code_access::CommandEnvelope,
    ) -> Result<tickloop_code_access::CommandReceipt, tickloop_code_access::AccessError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(tickloop_code_access::AccessError::new(
            tickloop_code_access::AccessErrorCode::OutcomeUnknown,
            "creation reply was lost",
        ))
    }

    async fn show_view(
        &self,
        _: tickloop_code_access::DisplayUpdate,
    ) -> Result<tickloop_code_access::DisplayReceipt, tickloop_code_access::AccessError> {
        unreachable!()
    }
}

#[tokio::test]
async fn view_upgrade_keeps_dispatch_directory_and_defers_an_unknown_creation() {
    use super::Intent;

    use tickloop_code_access::{AgentFilter, LaunchSpec, UserInput};

    let epoch = Uuid::new_v4();
    let access = Arc::new(UnknownCreation {
        epoch,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut console = AgentConsole::new(access.clone(), epoch, "/original".into(), None);
    console
        .interpret(Intent::DispatchDirectory("/chosen/project".into()))
        .unwrap();
    console
        .interpret(Intent::Filter(AgentFilter {
            folder: Some("/chosen/project".into()),
            workspace: Some("work".into()),
        }))
        .unwrap();
    let saved = console.prepare_upgrade_navigation().unwrap();
    let mut restored = AgentConsole::new(access.clone(), epoch, "/original".into(), None);
    restored.restore_upgrade_navigation(
        serde_json::from_value(serde_json::to_value(saved).unwrap()).unwrap(),
    );
    assert_eq!(
        restored.projection().dispatch_directory,
        std::path::PathBuf::from("/chosen/project")
    );
    assert_eq!(
        restored
            .prepare_upgrade_navigation()
            .unwrap()
            .filter
            .workspace
            .as_deref(),
        Some("work")
    );
    let input = || UserInput::Text {
        text: "do this once".into(),
        contexts: Vec::new(),
        paths: Vec::new(),
    };
    let launch = LaunchSpec::embedded("/original".into());
    assert!(
        restored
            .dispatch(launch.clone(), input(), None)
            .await
            .is_err()
    );
    assert!(restored.prepare_upgrade_navigation().is_none());
    assert!(restored.dispatch(launch, input(), None).await.is_err());
    assert_eq!(access.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

async fn serve_overview_stub(listener: UnixListener, epoch: Uuid, rows: serde_json::Value) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let _: Option<serde_json::Value> = read_frame(&mut stream, HANDSHAKE_FRAME_BYTES)
        .await
        .unwrap();
    write_frame(
        &mut stream,
        &ServerMessage::Hello(ServerHello {
            process_id: 0,
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            daemon_epoch: epoch,
            session_id: Uuid::new_v4(),
            capabilities: Vec::new(),
            limits: TRANSPORT_LIMITS.into(),
            build: None,
            executable: None,
            upgrade_protocol: None,
            handoff: None,
        }),
        HANDSHAKE_FRAME_BYTES,
    )
    .await
    .unwrap();
    while let Ok(Some(message)) = read_frame::<_, ClientMessage>(&mut stream, FRAME_BYTES).await {
        if let ClientMessage::Observe {
            request_id, query, ..
        } = message
        {
            let initial = match query {
                tickloop_code_access::ObservationRequest::EndedAgents { .. } => {
                    serde_json::json!({ "EndedAgents": [] })
                }
                _ => serde_json::json!({ "Overview": rows.clone() }),
            };
            let opened = serde_json::json!({
                "Opened": {
                    "request_id": request_id,
                    "observation_id": Uuid::new_v4(),
                    "initial": initial,
                    "cursor": { "daemon_epoch": epoch, "execution": null, "sequence": 0 },
                    "reset": null,
                    "feed": false,
                }
            });
            write_frame(&mut stream, &opened, FRAME_BYTES)
                .await
                .unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicitly_unknown_work_fact_stays_unknown() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("unknown.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = Uuid::new_v4();
    let summary = |has_work: Option<bool>| AgentSummary {
        subagent: None,
        availability: ExecutionAvailability::Live,
        execution: ExecutionRef {
            daemon_epoch: epoch,
            agent_id: Uuid::new_v4(),
            generation: 1,
        },
        chat_id: Uuid::new_v4(),
        folder: Some(temporary.path().to_path_buf()),
        state: LoopState::Idle,
        run_id: None,
        title: "New Agent".into(),
        has_work,
        updated_at: None,
        run_started_at: None,
        preview: None,
        waiting_permissions: 0,
        waiting_questions: 0,
        parent: None,
        record: None,
        workspace: None,
        capabilities: Vec::new(),
        views: Vec::new(),
    };
    // Unknown, empty and worked are distinct observations with the same display label.
    let (unknown, empty, worked) = (summary(None), summary(Some(false)), summary(Some(true)));
    let unknown_json = serde_json::to_value(&unknown).unwrap();
    let server = tokio::spawn(serve_overview_stub(
        listener,
        epoch,
        serde_json::json!([
            unknown_json,
            serde_json::to_value(&empty).unwrap(),
            serde_json::to_value(&worked).unwrap(),
        ]),
    ));
    let client = Arc::new(LocalAgentClient::connect(&socket).await.unwrap());
    let mut console = AgentConsole::new(client, epoch, temporary.path().to_path_buf(), None);
    let projection = console.refresh().await.unwrap();
    let row = |execution: ExecutionRef| {
        projection
            .rows
            .iter()
            .find(|row| row.execution == execution)
            .unwrap()
    };
    let forwarded = serde_json::to_value(row(unknown.execution)).unwrap();
    assert!(
        forwarded.get("has_work").is_none(),
        "a explicitly unknown summary must not be re-sent with an invented work fact: {forwarded}"
    );
    assert_eq!(row(unknown.execution).has_work, None);
    // Claude Code's agent view has no Unknown state: a summary without the fact reads as Idle.
    assert_eq!(AgentState::of(row(unknown.execution)), AgentState::Idle);
    assert!(super::is_listed(row(unknown.execution)));
    assert_eq!(row(empty.execution).has_work, Some(false));
    assert!(
        !super::is_listed(row(empty.execution)),
        "an Agent that holds nothing is not listed in the overview"
    );
    assert_eq!(
        serde_json::to_value(row(worked.execution)).unwrap()["has_work"],
        true
    );
    assert_eq!(
        AgentState::of(row(worked.execution)),
        AgentState::Idle,
        "the `New Agent` title alone never makes a worked Agent empty"
    );
    assert!(super::is_listed(row(worked.execution)));
    server.abort();
}

/// A daemon listing one running root and the ended roots it keeps, recording forgotten Chats.
struct EndedAgentsDaemon {
    epoch: Uuid,
    running: AgentSummary,
    ended: std::sync::Mutex<Vec<tickloop_code_access::EndedAgent>>,
    forgotten: std::sync::Mutex<Vec<Uuid>>,
}

#[async_trait::async_trait]
impl tickloop_code_access::AgentAccess for EndedAgentsDaemon {
    async fn observe(
        &self,
        query: tickloop_code_access::ObservationRequest,
    ) -> Result<tickloop_code_access::Observation, tickloop_code_access::AccessError> {
        use tickloop_code_access::{ObservationRequest, ObservationValue};
        let value = match query {
            ObservationRequest::Overview { .. } => {
                ObservationValue::Overview(vec![self.running.clone()])
            }
            ObservationRequest::EndedAgents { store_root } => {
                assert_eq!(store_root.as_deref(), Some(std::path::Path::new("/config")));
                ObservationValue::EndedAgents(self.ended.lock().unwrap().clone())
            }
            other => panic!("unexpected observation {other:?}"),
        };
        Ok(tickloop_code_access::Observation::single(
            value,
            tickloop_code_access::Cursor {
                daemon_epoch: self.epoch,
                execution: None,
                sequence: 0,
            },
            None,
        ))
    }

    async fn execute(
        &self,
        envelope: tickloop_code_access::CommandEnvelope,
    ) -> Result<tickloop_code_access::CommandReceipt, tickloop_code_access::AccessError> {
        let tickloop_code_access::AgentCommand::ForgetEndedAgent { record } = envelope.command
        else {
            panic!("unexpected command");
        };
        let chat_id = record.chat_id;
        self.forgotten.lock().unwrap().push(chat_id);
        self.ended
            .lock()
            .unwrap()
            .retain(|agent| agent.record.chat_id != chat_id);
        Ok(tickloop_code_access::CommandReceipt {
            command_id: envelope.command_id,
            daemon_epoch: self.epoch,
            state: tickloop_code_access::ReceiptState::Completed,
            target: None,
            result: Some(tickloop_code_access::CommandResult::Done),
            error: None,
            persistence: tickloop_code_access::PersistenceState::NotApplicable,
        })
    }

    async fn show_view(
        &self,
        _: tickloop_code_access::DisplayUpdate,
    ) -> Result<tickloop_code_access::DisplayReceipt, tickloop_code_access::AccessError> {
        unreachable!()
    }
}

fn ended_agent(chat: u128, folder: &str) -> tickloop_code_access::EndedAgent {
    tickloop_code_access::EndedAgent {
        record: tickloop_code_access::TranscriptRef {
            store_root: "/config".into(),
            chat_id: Uuid::from_u128(chat),
        },
        agent_id: Uuid::from_u128(chat + 1000),
        folder: Some(folder.into()),
        workspace: None,
        title: format!("Ended {chat}"),
        preview: None,
        created_at: 1,
        updated_at: 2,
        ended_at: 3,
        last_state: LoopState::Idle,
        outcome: tickloop_code_access::EndedOutcome::Failed,
        reason: tickloop_code_access::EndReason::DaemonStopped,
        resumable: true,
    }
}

#[tokio::test]
async fn the_projection_lists_ended_roots_once_and_forgets_them_on_request() {
    use super::Intent;
    use tickloop_code_access::AgentFilter;

    let epoch = Uuid::new_v4();
    let running = AgentSummary {
        subagent: None,
        availability: ExecutionAvailability::Live,
        execution: ExecutionRef {
            daemon_epoch: epoch,
            agent_id: Uuid::new_v4(),
            generation: 1,
        },
        chat_id: Uuid::from_u128(1),
        folder: Some("/work".into()),
        state: LoopState::Idle,
        run_id: None,
        title: "Running".into(),
        has_work: Some(true),
        updated_at: None,
        run_started_at: None,
        preview: None,
        waiting_permissions: 0,
        waiting_questions: 0,
        parent: None,
        record: None,
        workspace: None,
        capabilities: Vec::new(),
        views: Vec::new(),
    };
    let access = Arc::new(EndedAgentsDaemon {
        epoch,
        running,
        // The running Chat's stale ended record and two ended roots in other folders.
        ended: std::sync::Mutex::new(vec![
            ended_agent(1, "/work"),
            ended_agent(2, "/work"),
            ended_agent(3, "/elsewhere"),
        ]),
        forgotten: std::sync::Mutex::new(Vec::new()),
    });
    let mut console = AgentConsole::new(access.clone(), epoch, "/work".into(), None)
        .with_store_root(Some("/config".into()));

    let projection = console.refresh().await.unwrap();
    let ended = |projection: &super::Projection| {
        projection
            .ended
            .iter()
            .map(|agent| agent.record.chat_id.as_u128())
            .collect::<Vec<_>>()
    };
    assert_eq!(ended(&projection), [2, 3]);

    console
        .interpret(Intent::Filter(AgentFilter {
            folder: Some("/work".into()),
            workspace: None,
        }))
        .unwrap();
    assert_eq!(ended(&console.projection()), [2]);

    console.forget_ended(Uuid::from_u128(2)).await.unwrap();
    assert_eq!(*access.forgotten.lock().unwrap(), [Uuid::from_u128(2)]);
    assert!(console.projection().ended.is_empty());
    assert!(ended(&console.refresh().await.unwrap()).is_empty());
}
