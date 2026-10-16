use std::sync::Arc;

use tokio::net::UnixListener;
use uuid::Uuid;

use super::{AgentConsole, AttentionGroup};
use tickloop_agent_client::{
    LocalAgentClient,
    frame::{read_frame, write_frame},
};
use tickloop_agent_protocol::{
    AgentSummary, ClientMessage, ExecutionAvailability, ExecutionRef, HANDSHAKE_FRAME_BYTES,
    LoopState, PROTOCOL_MAJOR, PROTOCOL_MINOR, ServerHello, ServerMessage,
};
use tickloop_agent_test_support::TRANSPORT_LIMITS;

const FRAME_BYTES: usize = TRANSPORT_LIMITS.frame_bytes();

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
        }),
        HANDSHAKE_FRAME_BYTES,
    )
    .await
    .unwrap();
    while let Ok(Some(message)) = read_frame::<_, ClientMessage>(&mut stream, FRAME_BYTES).await {
        if let ClientMessage::Observe { request_id, .. } = message {
            let opened = serde_json::json!({
                "Opened": {
                    "request_id": request_id,
                    "observation_id": Uuid::new_v4(),
                    "initial": { "Overview": rows.clone() },
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
        workdir: temporary.path().to_path_buf(),
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
    assert_eq!(
        AttentionGroup::of(row(unknown.execution)),
        AttentionGroup::Unknown,
        "a summary without the fact is neither known worked Idle nor known Empty"
    );
    assert_eq!(row(empty.execution).has_work, Some(false));
    assert_eq!(
        AttentionGroup::of(row(empty.execution)),
        AttentionGroup::Empty
    );
    assert_eq!(
        serde_json::to_value(row(worked.execution)).unwrap()["has_work"],
        true
    );
    assert_eq!(
        AttentionGroup::of(row(worked.execution)),
        AttentionGroup::Idle,
        "the `New Agent` title alone never makes a worked Agent empty"
    );
    server.abort();
}
