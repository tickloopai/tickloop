use std::{future::Future, time::Duration};

use tickloop_agent_protocol::{
    AgentCommand, ExecutionRef, InteractionSource, PersistenceState, ReceiptState,
    ServerHandoffHello, UserInput, ViewRef,
};
use tokio::{net::UnixListener, task::JoinHandle};

use super::*;

async fn deadline<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("handoff test made no progress")
}

async fn receive(stream: &mut UnixStream) -> ClientMessage {
    deadline(read_frame(stream, AccessLimits::DEFAULT.frame_bytes()))
        .await
        .unwrap()
        .unwrap()
}

async fn publish(stream: &mut UnixStream, message: ServerMessage) {
    write_frame(stream, &message, AccessLimits::DEFAULT.frame_bytes())
        .await
        .unwrap();
}

async fn pair() -> (
    tempfile::TempDir,
    UnixListener,
    LocalAgentClient,
    UnixStream,
    ServerHello,
) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let endpoint = directory.path().join("access.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    let connect = tokio::spawn(async move { LocalAgentClient::connect(endpoint).await.unwrap() });
    let (mut stream, _) = deadline(listener.accept()).await.unwrap();
    assert!(matches!(
        receive(&mut stream).await,
        ClientMessage::Hello {
            handoff: Some(ClientHandoffHello {
                version: CLIENT_HANDOFF_VERSION,
                resume: None
            }),
            ..
        }
    ));
    let hello = ServerHello {
        process_id: 1234,
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
        daemon_epoch: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        capabilities: vec![],
        limits: AccessLimits::DEFAULT.into(),
        build: None,
        executable: None,
        upgrade_protocol: None,
        handoff: Some(ServerHandoffHello {
            version: CLIENT_HANDOFF_VERSION,
            resume_token: "private-session-proof".into(),
        }),
    };
    publish(&mut stream, ServerMessage::Hello(hello.clone())).await;
    (
        directory,
        listener,
        deadline(connect).await.unwrap(),
        stream,
        hello,
    )
}

fn command(hello: &ServerHello, text: &str) -> CommandEnvelope {
    CommandEnvelope {
        daemon_epoch: hello.daemon_epoch,
        command_id: Uuid::new_v4(),
        command: AgentCommand::Submit {
            execution: ExecutionRef {
                daemon_epoch: hello.daemon_epoch,
                agent_id: Uuid::new_v4(),
                generation: 3,
            },
            input: UserInput::Text {
                text: text.into(),
                contexts: vec![],
                paths: vec![],
            },
            origin: Some(InteractionSource {
                view: ViewRef {
                    session_id: hello.session_id,
                    view_id: Uuid::new_v4(),
                },
                display_version: 7,
                host: None,
            }),
        },
    }
}

fn execute(
    client: &LocalAgentClient,
    command: CommandEnvelope,
) -> JoinHandle<Result<CommandReceipt, AccessError>> {
    let client = client.clone();
    tokio::spawn(async move { client.execute(command).await })
}

async fn prepare(stream: &mut UnixStream) -> Uuid {
    let transfer_id = Uuid::new_v4();
    publish(stream, ServerMessage::HandoffPrepare { transfer_id }).await;
    assert!(
        matches!(receive(stream).await, ClientMessage::HandoffReady { transfer_id: actual } if actual == transfer_id)
    );
    transfer_id
}

async fn resume(
    listener: &UnixListener,
    stream: &mut UnixStream,
    hello: &ServerHello,
    transfer_id: Uuid,
) -> UnixStream {
    publish(
        stream,
        ServerMessage::HandoffCommit {
            transfer_id,
            successor_process_id: 5678,
        },
    )
    .await;
    let (mut successor, _) = deadline(listener.accept()).await.unwrap();
    let ClientMessage::Hello {
        handoff:
            Some(ClientHandoffHello {
                resume: Some(proof),
                ..
            }),
        ..
    } = receive(&mut successor).await
    else {
        panic!("successor was not given the authenticated resume proof");
    };
    assert_eq!(proof.daemon_epoch, hello.daemon_epoch);
    assert_eq!(proof.session_id, hello.session_id);
    assert_eq!(proof.transfer_id, transfer_id);
    assert_eq!(proof.token, hello.handoff.as_ref().unwrap().resume_token);
    let mut resumed = hello.clone();
    resumed.process_id = 5678;
    publish(&mut successor, ServerMessage::Hello(resumed)).await;
    successor
}

async fn complete(stream: &mut UnixStream, message: &ClientMessage) {
    let ClientMessage::Execute {
        request_id,
        command,
    } = message
    else {
        panic!("expected one command");
    };
    publish(
        stream,
        ServerMessage::Receipt {
            request_id: *request_id,
            receipt: CommandReceipt {
                daemon_epoch: command.daemon_epoch,
                command_id: command.command_id,
                target: None,
                state: ReceiptState::Accepted,
                result: None,
                error: None,
                persistence: PersistenceState::NotApplicable,
            },
        },
    )
    .await;
}

async fn pending_count(client: &LocalAgentClient, count: usize) {
    deadline(async {
        while client.inner.pending.lock().await.len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test]
async fn committed_handoff_replays_only_not_accepted_before_queued_input_in_the_same_session() {
    let (_directory, listener, client, mut original, hello) = pair().await;
    let first = execute(&client, command(&hello, "first"));
    let original_message = receive(&mut original).await;
    let ClientMessage::Execute { request_id, .. } = &original_message else {
        panic!("expected command");
    };
    let transfer_id = prepare(&mut original).await;
    assert!(client.is_handing_off());
    let second_command = command(&hello, "second");
    let second_id = second_command.command_id;
    let second = execute(&client, second_command);
    pending_count(&client, 2).await;
    publish(
        &mut original,
        ServerMessage::NotAccepted {
            request_id: *request_id,
            transfer_id,
        },
    )
    .await;
    let mut successor = resume(&listener, &mut original, &hello, transfer_id).await;
    let replayed = receive(&mut successor).await;
    assert_eq!(
        serde_json::to_value(&replayed).unwrap(),
        serde_json::to_value(original_message).unwrap()
    );
    complete(&mut successor, &replayed).await;
    let queued = receive(&mut successor).await;
    assert!(
        matches!(&queued, ClientMessage::Execute { command, .. } if command.command_id == second_id)
    );
    complete(&mut successor, &queued).await;
    deadline(first).await.unwrap().unwrap();
    deadline(second).await.unwrap().unwrap();
    assert_eq!(client.session_id(), hello.session_id);
    assert_eq!(client.daemon_epoch(), hello.daemon_epoch);
    assert_eq!(client.server_process_id(), 5678);
    assert!(!client.is_handing_off());
}

#[tokio::test]
async fn aborted_handoff_releases_locally_queued_input_once() {
    let (_directory, _listener, client, mut original, hello) = pair().await;
    let transfer_id = prepare(&mut original).await;
    let task = execute(&client, command(&hello, "queued"));
    pending_count(&client, 1).await;
    assert!(
        !client
            .inner
            .pending
            .lock()
            .await
            .values()
            .next()
            .unwrap()
            .written
    );
    publish(&mut original, ServerMessage::HandoffAbort { transfer_id }).await;
    let message = receive(&mut original).await;
    complete(&mut original, &message).await;
    deadline(task).await.unwrap().unwrap();
    assert_eq!(client.server_process_id(), hello.process_id);
    assert!(!client.is_handing_off());
}

#[tokio::test]
async fn canceled_not_accepted_observation_is_not_replayed_or_canceled_on_the_successor() {
    let (_directory, listener, client, mut original, hello) = pair().await;
    let observing = {
        let client = client.clone();
        tokio::spawn(async move { client.observe(ObservationRequest::RuntimeVersion).await })
    };
    let ClientMessage::Observe { request_id, .. } = receive(&mut original).await else {
        panic!("expected observation");
    };
    let transfer_id = prepare(&mut original).await;
    publish(
        &mut original,
        ServerMessage::NotAccepted {
            request_id,
            transfer_id,
        },
    )
    .await;
    deadline(async {
        loop {
            let pending = client.inner.pending.lock().await;
            if pending
                .get(&request_id)
                .is_some_and(|request| !request.written)
            {
                break;
            }
            drop(pending);
            tokio::task::yield_now().await;
        }
    })
    .await;
    observing.abort();
    assert!(observing.await.unwrap_err().is_cancelled());
    let task = execute(&client, command(&hello, "after canceled observation"));
    let mut successor = resume(&listener, &mut original, &hello, transfer_id).await;
    let message = receive(&mut successor).await;
    assert!(matches!(message, ClientMessage::Execute { .. }));
    complete(&mut successor, &message).await;
    deadline(task).await.unwrap().unwrap();
}

#[tokio::test]
async fn ordinary_eof_after_dispatch_is_unknown_and_never_replayed() {
    let (_directory, listener, client, mut original, hello) = pair().await;
    let task = execute(&client, command(&hello, "only once"));
    assert!(matches!(
        receive(&mut original).await,
        ClientMessage::Execute { .. }
    ));
    drop(original);
    let error = deadline(task).await.unwrap().unwrap_err();
    assert_eq!(error.code, AccessErrorCode::OutcomeUnknown);
    assert!(!client.is_connected());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unnegotiated_handoff_frames_close_the_connection_without_replay() {
    let (_directory, _listener, client, mut original, _) = pair().await;
    client.inner.hello.write().unwrap().handoff = None;
    publish(
        &mut original,
        ServerMessage::HandoffPrepare {
            transfer_id: Uuid::new_v4(),
        },
    )
    .await;
    deadline(client.inner.cancel.cancelled()).await;
    drop(client);
    assert!(
        deadline(read_frame::<_, ClientMessage>(
            &mut original,
            AccessLimits::DEFAULT.frame_bytes()
        ))
        .await
        .unwrap()
        .is_none()
    );
}
