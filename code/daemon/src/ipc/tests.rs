use std::sync::Arc;

use async_trait::async_trait;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Mutex, mpsc, oneshot},
};
use uuid::Uuid;

use super::server::{
    ObservationTestHooks, serve_connection, serve_connection_mode, serve_listener,
};
use tickloop_code_access::{
    AccessError, AccessErrorCode, AccessLimits, AgentAccess, AgentCommand, AgentDetailQuery,
    AgentFilter, ChunkedResponseKind, ClientMessage, CommandEnvelope, CommandReceipt, Cursor,
    DisplayReceipt, DisplayUpdate, ExecutionRef, FEED_CAPACITY, FEED_CHANGE_BYTES,
    HANDSHAKE_FRAME_BYTES, Observation, ObservationChange, ObservationRequest, ObservationUpdate,
    ObservationValue, PROTOCOL_MAJOR, PROTOCOL_MINOR, Page, ProtocolLimits, RecordedMessage,
    RecordedTranscript, ServerHello, ServerMessage, TranscriptRef,
};
use tickloop_code_client::{
    LocalAgentClient,
    frame::{read_frame, write_frame},
    negotiate,
};
use tickloop_harness_protocol::{Event, EventKind};

use tickloop_code_access::test_support::TRANSPORT_LIMITS;

const FRAME_BYTES: usize = TRANSPORT_LIMITS.frame_bytes();
/// Backpressure needs a request larger than any kernel socket buffer, so these peers keep the
/// production frame limit.
const BACKPRESSURE_FRAME_BYTES: usize = AccessLimits::DEFAULT.frame_bytes();

struct ControlledFeed {
    sender: Mutex<Option<mpsc::Sender<ObservationUpdate>>>,
    epoch: Uuid,
    initial: Option<ObservationValue>,
}

fn transcript_initial(text: String) -> ObservationValue {
    ObservationValue::Transcript(Box::new(RecordedTranscript {
        goal_history: Vec::new(),
        record: TranscriptRef {
            store_root: "/tmp/transcripts".into(),
            chat_id: Uuid::new_v4(),
        },
        title: None,
        folder: "/tmp/workdir".into(),
        messages: vec![RecordedMessage {
            id: Uuid::new_v4(),
            message: tickloop_harness_protocol::Message {
                role: tickloop_harness_protocol::Role::User,
                content: vec![tickloop_harness_protocol::Content::Text { text }],
            },
            loop_id: None,
            run_id: None,
            timestamp: String::new(),
        }],
        total_messages: 1,
        warnings: Vec::new(),
    }))
}

#[async_trait]
impl AgentAccess for ControlledFeed {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let (initial, changes) = if matches!(query, ObservationRequest::Receipt { .. }) {
            (ObservationValue::Receipt(None), None)
        } else {
            let (sender, changes) = mpsc::channel(FEED_CAPACITY);
            *self.sender.lock().await = Some(sender);
            (
                self.initial
                    .clone()
                    .unwrap_or_else(|| ObservationValue::Overview(Vec::new())),
                Some(changes),
            )
        };
        Ok(Observation::new(
            initial,
            Cursor {
                daemon_epoch: self.epoch,
                execution: None,
                sequence: 0,
            },
            None,
            changes,
            None,
        ))
    }
    async fn execute(&self, _: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        Err(AccessError::invalid("fixture does not execute commands"))
    }
    async fn show_view(&self, _: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        Err(AccessError::invalid("fixture has no display"))
    }
}

#[tokio::test]
async fn a_peer_closing_during_hello_is_unavailable_without_a_version_claim() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let path = temporary.path().join("daemon.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(matches!(
            read_frame::<_, ClientMessage>(&mut stream, HANDSHAKE_FRAME_BYTES)
                .await
                .unwrap(),
            Some(ClientMessage::Hello { .. })
        ));
    });
    let error = match LocalAgentClient::connect(&path).await {
        Ok(_) => panic!("closed peer cannot negotiate a connection"),
        Err(error) => error,
    };
    server.await.unwrap();
    assert_eq!(error.code, AccessErrorCode::Unavailable);
}

#[tokio::test]
async fn older_business_peers_are_fenced_before_a_query_can_dispatch() {
    for (major, minor) in [(3, 2), (5, 0), (6, 0), (8, 0), (9, 0)] {
        let epoch = Uuid::new_v4();
        let source = Arc::new(CompletingLookup {
            epoch,
            started: tokio::sync::Notify::new(),
            release: std::sync::atomic::AtomicBool::new(true),
        });
        let (server_stream, mut client) = UnixStream::pair().unwrap();
        let server = tokio::spawn(serve_connection(
            server_stream,
            source.clone(),
            epoch,
            TRANSPORT_LIMITS,
        ));
        write_frame(
            &mut client,
            &ClientMessage::Hello {
                major,
                minor,
                handoff: None,
            },
            HANDSHAKE_FRAME_BYTES,
        )
        .await
        .unwrap();
        let response = read_frame::<_, ServerMessage>(&mut client, HANDSHAKE_FRAME_BYTES)
            .await
            .unwrap();
        assert!(matches!(
            response,
            Some(ServerMessage::Error {
                error: AccessError {
                    code: AccessErrorCode::VersionMismatch,
                    ..
                },
                ..
            })
        ));
        assert!(
            read_frame::<_, ServerMessage>(&mut client, HANDSHAKE_FRAME_BYTES)
                .await
                .unwrap()
                .is_none()
        );
        server.await.unwrap().unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                source.started.notified()
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn a_hello_announcing_unusable_limits_is_an_incompatible_peer() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("limits.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_frame::<_, ClientMessage>(&mut stream, HANDSHAKE_FRAME_BYTES)
            .await
            .unwrap();
        let mut limits = ProtocolLimits::from(TRANSPORT_LIMITS);
        limits.frame_bytes = HANDSHAKE_FRAME_BYTES - 1;
        write_frame(
            &mut stream,
            &ServerMessage::Hello(ServerHello {
                process_id: 0,
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
                daemon_epoch: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                capabilities: Vec::new(),
                limits,
                build: None,
                executable: None,
                upgrade_protocol: None,
                handoff: None,
            }),
            HANDSHAKE_FRAME_BYTES,
        )
        .await
        .unwrap();
    });
    let error = match LocalAgentClient::connect(&socket).await {
        Ok(_) => panic!("a client cannot adopt limits below the handshake frame"),
        Err(error) => error,
    };
    server.await.unwrap();
    assert_eq!(error.code, AccessErrorCode::VersionMismatch);
    assert!(error.message.contains("transport limits"), "{error:?}");
}

#[tokio::test]
async fn frames_beyond_the_negotiated_limit_are_refused_on_both_sides() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("frame.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = Uuid::new_v4();
    let source = Arc::new(CompletingLookup {
        epoch,
        started: tokio::sync::Notify::new(),
        release: std::sync::atomic::AtomicBool::new(true),
    });
    let server = tokio::spawn(serve_listener(listener, source, epoch, TRANSPORT_LIMITS));
    let client = LocalAgentClient::connect(&socket).await.unwrap();
    assert_eq!(client.protocol().limits.frame_bytes, FRAME_BYTES);
    let error = client
        .observe(ObservationRequest::Overview {
            filter: AgentFilter {
                folder: None,
                workspace: Some("x".repeat(FRAME_BYTES)),
            },
            page: Page::default(),
            after: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, AccessErrorCode::PayloadTooLarge);
    assert!(client.is_connected());
    let receipt = client
        .observe(ObservationRequest::Receipt {
            epoch,
            command_id: Uuid::new_v4(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt.initial, ObservationValue::Receipt(None)));

    let mut stream = UnixStream::connect(&socket).await.unwrap();
    negotiate(&mut stream).await.unwrap();
    stream.write_u32(FRAME_BYTES as u32 + 1).await.unwrap();
    let mut remaining = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut remaining),
    )
    .await
    .expect("the server closes a connection that announces an oversized frame")
    .unwrap();
    assert!(remaining.is_empty());
    server.abort();
}

#[tokio::test]
async fn a_request_of_another_build_is_refused_alone_and_the_connection_stays() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("unknown.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = Uuid::new_v4();
    let source = Arc::new(CompletingLookup {
        epoch,
        started: tokio::sync::Notify::new(),
        release: std::sync::atomic::AtomicBool::new(true),
    });
    let server = tokio::spawn(serve_listener(listener, source, epoch, TRANSPORT_LIMITS));
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    negotiate(&mut stream).await.unwrap();
    let unknown = Uuid::new_v4();
    write_frame(
        &mut stream,
        &serde_json::json!({"RequestFromANewerBuild": {"request_id": unknown, "detail": 1}}),
        FRAME_BYTES,
    )
    .await
    .unwrap();
    let Some(ServerMessage::Error { request_id, error }) =
        read_frame::<_, ServerMessage>(&mut stream, FRAME_BYTES)
            .await
            .unwrap()
    else {
        panic!("the unknown request gets its own error");
    };
    assert_eq!(request_id, Some(unknown));
    assert_eq!(error.code, AccessErrorCode::Unsupported);
    assert!(
        error.message.contains("another Tickloop build"),
        "{error:?}"
    );
    let known = Uuid::new_v4();
    write_frame(
        &mut stream,
        &ClientMessage::ShutdownDaemon {
            request_id: known,
            daemon_epoch: Uuid::new_v4(),
        },
        FRAME_BYTES,
    )
    .await
    .unwrap();
    let Some(ServerMessage::Error { request_id, .. }) =
        read_frame::<_, ServerMessage>(&mut stream, FRAME_BYTES)
            .await
            .unwrap()
    else {
        panic!("the same connection still answers known requests");
    };
    assert_eq!(request_id, Some(known));
    server.abort();
}

struct CompletingLookup {
    epoch: Uuid,
    started: tokio::sync::Notify,
    release: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl AgentAccess for CompletingLookup {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let initial = if matches!(
            query,
            ObservationRequest::Agent {
                detail: AgentDetailQuery::AccountModels { .. },
                ..
            }
        ) {
            self.started.notify_one();
            tokio::task::block_in_place(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while !self.release.load(std::sync::atomic::Ordering::SeqCst) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "Controlled lookup was not released"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            ObservationValue::AccountModels(Err("Controlled completed lookup".into()))
        } else {
            ObservationValue::Receipt(None)
        };
        Ok(Observation::single(
            initial,
            Cursor {
                daemon_epoch: self.epoch,
                execution: None,
                sequence: 0,
            },
            None,
        ))
    }
    async fn execute(&self, _: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        Err(AccessError::invalid("Fixture has no commands"))
    }
    async fn show_view(&self, _: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        Err(AccessError::invalid("Fixture has no display"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completing_a_cancelled_lookup_returns_exactly_one_terminal_reply() {
    use std::time::Duration;
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let socket = directory.path().join("completion.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = Uuid::new_v4();
    let source = Arc::new(CompletingLookup {
        epoch,
        started: tokio::sync::Notify::new(),
        release: std::sync::atomic::AtomicBool::new(false),
    });
    let server = tokio::spawn(serve_listener(
        listener,
        source.clone(),
        epoch,
        TRANSPORT_LIMITS,
    ));
    let mut client = UnixStream::connect(&socket).await.unwrap();
    write_frame(
        &mut client,
        &ClientMessage::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            handoff: None,
        },
        HANDSHAKE_FRAME_BYTES,
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame::<_, ServerMessage>(&mut client, HANDSHAKE_FRAME_BYTES)
            .await
            .unwrap(),
        Some(ServerMessage::Hello(_))
    ));
    for _ in 0..3 {
        source
            .release
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let cancelled_request = Uuid::new_v4();
        write_frame(
            &mut client,
            &ClientMessage::Observe {
                request_id: cancelled_request,
                query: ObservationRequest::Agent {
                    execution: ExecutionRef {
                        daemon_epoch: epoch,
                        agent_id: Uuid::new_v4(),
                        generation: 1,
                    },
                    detail: AgentDetailQuery::AccountModels {
                        config: Box::new(
                            tickloop_harness_model::cards::ModelCatalog::defaults()
                                .get("openai-api", "gpt-6.1-sol")
                                .unwrap(),
                        ),
                        refresh: true,
                    },
                    after: None,
                },
            },
            FRAME_BYTES,
        )
        .await
        .unwrap();
        source.started.notified().await;
        write_frame(
            &mut client,
            &ClientMessage::CancelObservation {
                observation_id: cancelled_request,
            },
            FRAME_BYTES,
        )
        .await
        .unwrap();
        let compatible_request = Uuid::new_v4();
        write_frame(
            &mut client,
            &ClientMessage::Observe {
                request_id: compatible_request,
                query: ObservationRequest::Receipt {
                    epoch,
                    command_id: Uuid::new_v4(),
                },
            },
            FRAME_BYTES,
        )
        .await
        .unwrap();
        let compatible = tokio::time::timeout(
            Duration::from_secs(2),
            read_frame::<_, ServerMessage>(&mut client, FRAME_BYTES),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(compatible,Some(ServerMessage::Opened { request_id,initial:ObservationValue::Receipt(None),.. }) if request_id==compatible_request)
        );
        source
            .release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let terminal = tokio::time::timeout(Duration::from_secs(2),read_frame::<_,ServerMessage>(&mut client, FRAME_BYTES)).await.expect("A completing cancelled lookup must retire its pending request with a terminal response").unwrap();
        assert!(
            matches!(terminal,Some(ServerMessage::Error { request_id:Some(request_id),error:AccessError { code:AccessErrorCode::Unavailable,.. } }) if request_id==cancelled_request)
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                read_frame::<_, ServerMessage>(&mut client, FRAME_BYTES)
            )
            .await
            .is_err(),
            "Cancellation and completion must not send duplicate terminal replies"
        );
    }
    drop(client);
    server.abort();
}

/// Shrinks a server socket's send buffer so one feed frame cannot fit in the kernel, and
/// returns the size the kernel applied.
fn shrink_send_buffer(stream: &UnixStream) -> usize {
    use std::os::fd::AsRawFd;
    let requested: libc::c_int = 2048;
    let option_size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let set = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&requested as *const libc::c_int).cast(),
            option_size,
        )
    };
    assert_eq!(set, 0, "cannot shrink the server send buffer");
    let mut applied: libc::c_int = 0;
    let mut applied_size = option_size;
    let read = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&mut applied as *mut libc::c_int).cast(),
            &mut applied_size,
        )
    };
    assert_eq!(read, 0, "cannot read the server send buffer");
    applied as usize
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_backpressured_lookup_keeps_the_live_request_frame_whole() {
    use std::time::Duration;
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let socket = directory.path().join("ordering.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = Uuid::new_v4();
    let (started, waiting) = oneshot::channel();
    let (resume, draining) = oneshot::channel();
    let (cancelled, cancellation) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(matches!(
            read_frame::<_, ClientMessage>(&mut stream, HANDSHAKE_FRAME_BYTES)
                .await
                .unwrap(),
            Some(ClientMessage::Hello { .. })
        ));
        write_frame(
            &mut stream,
            &ServerMessage::Hello(ServerHello {
                process_id: 0,
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
                daemon_epoch: epoch,
                session_id: Uuid::new_v4(),
                capabilities: Vec::new(),
                limits: AccessLimits::DEFAULT.into(),
                build: None,
                executable: None,
                upgrade_protocol: None,
                handoff: None,
            }),
            HANDSHAKE_FRAME_BYTES,
        )
        .await
        .unwrap();
        let length = stream.read_u32().await.unwrap() as usize;
        assert!(length > 3 * 1024 * 1024);
        started.send(()).unwrap();
        draining.await.unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        let ClientMessage::Observe { request_id, .. } =
            serde_json::from_slice::<ClientMessage>(&body)
                .expect("Observation cancellation must not truncate a live request frame")
        else {
            panic!("Expected Observe")
        };
        assert!(
            matches!(read_frame::<_,ClientMessage>(&mut stream, BACKPRESSURE_FRAME_BYTES).await.unwrap(),Some(ClientMessage::CancelObservation { observation_id }) if observation_id==request_id)
        );
        write_frame(
            &mut stream,
            &ServerMessage::Error {
                request_id: Some(request_id),
                error: AccessError::unavailable("Controlled observation cancelled"),
            },
            BACKPRESSURE_FRAME_BYTES,
        )
        .await
        .unwrap();
        cancelled.send(()).unwrap();
        let Some(ClientMessage::Observe { request_id, .. }) =
            read_frame::<_, ClientMessage>(&mut stream, BACKPRESSURE_FRAME_BYTES)
                .await
                .unwrap()
        else {
            panic!("Expected compatible next request")
        };
        write_frame(
            &mut stream,
            &ServerMessage::Opened {
                request_id,
                observation_id: request_id,
                initial: ObservationValue::Overview(Vec::new()),
                cursor: Cursor {
                    daemon_epoch: epoch,
                    execution: None,
                    sequence: 0,
                },
                reset: None,
                feed: false,
            },
            BACKPRESSURE_FRAME_BYTES,
        )
        .await
        .unwrap();
    });
    let client = LocalAgentClient::connect(&socket).await.unwrap();
    let observing = client.clone();
    let query = tokio::spawn(async move {
        observing
            .observe(ObservationRequest::Overview {
                filter: AgentFilter {
                    folder: None,
                    workspace: Some("x".repeat(3 * 1024 * 1024)),
                },
                page: Page::default(),
                after: None,
            })
            .await
    });
    waiting.await.unwrap();
    query.abort();
    let _ = query.await;
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), cancellation)
        .await
        .unwrap()
        .unwrap();
    assert!(client.is_connected());
    let next = tokio::time::timeout(
        Duration::from_secs(2),
        client.observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page::default(),
            after: None,
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(next.initial,ObservationValue::Overview(rows) if rows.is_empty()));
    peer.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_opened_feed_cannot_keep_a_backpressured_client_alive() {
    use std::time::Duration;
    for opened_feed in [true, false] {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let socket = directory.path().join("feed-pressure.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let epoch = Uuid::new_v4();
        let (started, waiting) = oneshot::channel();
        let (resume, draining) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(matches!(
                read_frame::<_, ClientMessage>(&mut stream, HANDSHAKE_FRAME_BYTES)
                    .await
                    .unwrap(),
                Some(ClientMessage::Hello { .. })
            ));
            write_frame(
                &mut stream,
                &ServerMessage::Hello(ServerHello {
                    process_id: 0,
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR,
                    daemon_epoch: epoch,
                    session_id: Uuid::new_v4(),
                    capabilities: Vec::new(),
                    limits: AccessLimits::DEFAULT.into(),
                    build: None,
                    executable: None,
                    upgrade_protocol: None,
                    handoff: None,
                }),
                HANDSHAKE_FRAME_BYTES,
            )
            .await
            .unwrap();
            if opened_feed {
                let Some(ClientMessage::Observe { request_id, .. }) =
                    read_frame::<_, ClientMessage>(&mut stream, BACKPRESSURE_FRAME_BYTES)
                        .await
                        .unwrap()
                else {
                    panic!("Expected feed request")
                };
                write_frame(
                    &mut stream,
                    &ServerMessage::Opened {
                        request_id,
                        observation_id: request_id,
                        initial: ObservationValue::Overview(Vec::new()),
                        cursor: Cursor {
                            daemon_epoch: epoch,
                            execution: None,
                            sequence: 0,
                        },
                        reset: None,
                        feed: true,
                    },
                    BACKPRESSURE_FRAME_BYTES,
                )
                .await
                .unwrap();
            }
            let length = stream.read_u32().await.unwrap() as usize;
            assert!(length > 8 * 1024 * 1024);
            started.send(()).unwrap();
            draining.await.unwrap();
            let mut remaining = Vec::new();
            tokio::time::timeout(Duration::from_secs(2),stream.read_to_end(&mut remaining)).await.expect("An opened-feed cancellation must release the last client without waiting for full peer drain").unwrap();
            assert!(
                remaining.len() < length,
                "opened_feed={opened_feed}: feed cancellation retained the client and completed its abandoned frame ({} bytes)",
                remaining.len()
            );
        });
        let client = LocalAgentClient::connect(&socket).await.unwrap();
        let observation = if opened_feed {
            Some(
                client
                    .observe(ObservationRequest::Overview {
                        filter: AgentFilter::default(),
                        page: Page::default(),
                        after: None,
                    })
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let observing = client.clone();
        let query = tokio::spawn(async move {
            observing
                .observe(ObservationRequest::Overview {
                    filter: AgentFilter {
                        folder: None,
                        workspace: Some("x".repeat(8 * 1024 * 1024)),
                    },
                    page: Page::default(),
                    after: None,
                })
                .await
        });
        waiting.await.unwrap();
        drop(observation);
        tokio::time::sleep(Duration::from_millis(50)).await;
        query.abort();
        let _ = query.await;
        drop(client);
        tokio::time::sleep(Duration::from_millis(50)).await;
        resume.send(()).unwrap();
        peer.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backpressured_observation_write_stops_on_eof_or_last_client_drop() {
    use std::time::Duration;
    for peer_eof in [true, false] {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let socket = directory.path().join("pressure.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (started, waiting) = oneshot::channel();
        let (resume, draining) = oneshot::channel();
        let epoch = Uuid::new_v4();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(matches!(
                read_frame::<_, ClientMessage>(&mut stream, HANDSHAKE_FRAME_BYTES)
                    .await
                    .unwrap(),
                Some(ClientMessage::Hello { .. })
            ));
            write_frame(
                &mut stream,
                &ServerMessage::Hello(ServerHello {
                    process_id: 0,
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR,
                    daemon_epoch: epoch,
                    session_id: Uuid::new_v4(),
                    capabilities: Vec::new(),
                    limits: AccessLimits::DEFAULT.into(),
                    build: None,
                    executable: None,
                    upgrade_protocol: None,
                    handoff: None,
                }),
                HANDSHAKE_FRAME_BYTES,
            )
            .await
            .unwrap();
            let length = stream.read_u32().await.unwrap() as usize;
            assert!(length > 8 * 1024 * 1024);
            let (mut reader, mut writer) = stream.into_split();
            if peer_eof {
                writer.shutdown().await.unwrap();
            }
            started.send(length).unwrap();
            draining.await.unwrap();
            let mut remaining = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), reader.read_to_end(&mut remaining)).await.expect("Dropping the last client must close a backpressured writer without completing its abandoned frame").unwrap();
            assert!(
                remaining.len() < length,
                "peer_eof={peer_eof}: an abandoned request was still written in full ({} bytes)",
                remaining.len()
            );
        });
        let client = LocalAgentClient::connect(&socket).await.unwrap();
        let observing = client.clone();
        let query = tokio::spawn(async move {
            observing
                .observe(ObservationRequest::Overview {
                    filter: AgentFilter {
                        folder: None,
                        workspace: Some("x".repeat(8 * 1024 * 1024)),
                    },
                    page: Page::default(),
                    after: None,
                })
                .await
        });
        waiting.await.unwrap();
        if peer_eof {
            let result = tokio::time::timeout(Duration::from_secs(1), query)
                .await
                .expect("Response EOF must stop the blocked request writer and unblock the caller")
                .unwrap();
            assert!(matches!(
                result,
                Err(AccessError {
                    code: AccessErrorCode::Unavailable,
                    ..
                })
            ));
            assert!(!client.is_connected());
        } else {
            query.abort();
            let _ = query.await;
        }
        drop(client);
        tokio::time::sleep(Duration::from_millis(50)).await;
        resume.send(()).unwrap();
        peer.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceling_an_observation_mid_frame_never_corrupts_the_shared_connection() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("cancel-mid-frame.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = Uuid::new_v4();
    let source = Arc::new(ControlledFeed {
        sender: Mutex::new(None),
        epoch,
        initial: None,
    });
    let hooks = Arc::new(ObservationTestHooks {
        next: std::sync::atomic::AtomicBool::new(false),
        opened: tokio::sync::Notify::new(),
        canceled: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let (buffer_sender, buffer_size) = oneshot::channel();
    let server_source = source.clone();
    let server_hooks = hooks.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = buffer_sender.send(shrink_send_buffer(&stream));
        let _ = serve_connection_mode(
            stream,
            server_source,
            epoch,
            TRANSPORT_LIMITS,
            false,
            None,
            Some(server_hooks),
        )
        .await;
    });
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    write_frame(
        &mut stream,
        &ClientMessage::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            handoff: None,
        },
        HANDSHAKE_FRAME_BYTES,
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame::<_, ServerMessage>(&mut stream, HANDSHAKE_FRAME_BYTES)
            .await
            .unwrap(),
        Some(ServerMessage::Hello(_))
    ));
    let send_buffer = buffer_size.await.unwrap();
    let feed_request = Uuid::new_v4();
    write_frame(
        &mut stream,
        &ClientMessage::Observe {
            request_id: feed_request,
            query: ObservationRequest::Overview {
                filter: AgentFilter::default(),
                page: Page::default(),
                after: None,
            },
        },
        FRAME_BYTES,
    )
    .await
    .unwrap();
    let observation_id = loop {
        if let Some(ServerMessage::Opened {
            request_id,
            observation_id,
            ..
        }) = read_frame::<_, ServerMessage>(&mut stream, FRAME_BYTES)
            .await
            .unwrap()
            && request_id == feed_request
        {
            break observation_id;
        }
    };
    let update = |sequence, text: &str| ObservationUpdate {
        cursor: Cursor {
            daemon_epoch: epoch,
            execution: None,
            sequence,
        },
        change: ObservationChange::Event(Event {
            sequence,
            chat_id: Uuid::nil().into(),
            loop_id: Uuid::nil().into(),
            run_id: Uuid::nil().into(),
            kind: EventKind::Text(text.into()),
        }),
    };
    // The largest update the feed forwards unchanged, so one frame is as long as possible.
    let overhead = serde_json::to_vec(&update(1, "")).unwrap().len();
    let text = "x".repeat(FEED_CHANGE_BYTES - overhead - 64);
    let sender = source.sender.lock().await.take().unwrap();
    sender.send(update(1, &text)).await.unwrap();
    sender.send(update(2, &text)).await.unwrap();
    // Reading the length prefix proves the server is inside the first frame's write. Linux
    // can buffer up to one and a half send buffers of data, so a longer body cannot fit: the
    // write stays in progress until this client reads again.
    let length = tokio::time::timeout(std::time::Duration::from_secs(10), stream.read_u32())
        .await
        .expect("the server starts writing the first update")
        .unwrap() as usize;
    assert!(
        send_buffer * 3 / 2 + 4 < length,
        "a {length}-byte frame must not fit a {send_buffer}-byte send buffer"
    );
    let (mut reader, mut writer) = stream.into_split();
    write_frame(
        &mut writer,
        &ClientMessage::CancelObservation { observation_id },
        FRAME_BYTES,
    )
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        hooks.canceled.notified(),
    )
    .await
    .expect("the server handles the cancel while the frame is still being written");
    let later_request = Uuid::new_v4();
    write_frame(
        &mut writer,
        &ClientMessage::Observe {
            request_id: later_request,
            query: ObservationRequest::Receipt {
                epoch,
                command_id: Uuid::new_v4(),
            },
        },
        FRAME_BYTES,
    )
    .await
    .unwrap();
    let (in_progress, next) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await.unwrap();
        let in_progress = serde_json::from_slice::<ServerMessage>(&body)
            .expect("the frame being written when the cancel arrived is whole");
        (
            in_progress,
            read_frame::<_, ServerMessage>(&mut reader, FRAME_BYTES).await,
        )
    })
    .await
    .expect("canceling an observation left a partial frame on the connection");
    assert!(
        matches!(
            &in_progress,
            ServerMessage::Changed { observation_id: changed, update }
                if *changed == observation_id
                    && update.cursor.sequence == 1
                    && matches!(
                        &update.change,
                        ObservationChange::Event(Event {
                            kind: EventKind::Text(sent),
                            ..
                        }) if *sent == text
                    )
        ),
        "{in_progress:?}"
    );
    assert!(
        matches!(
            &next,
            Ok(Some(ServerMessage::Opened { request_id, .. })) if *request_id == later_request
        ),
        "the canceled feed writes nothing more, and the later request is answered on the same connection: {next:?}"
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), sender.closed())
        .await
        .expect("a canceled feed closes its source");
    server.abort();
}

#[tokio::test]
async fn cancel_immediately_after_opened_closes_the_source_before_feed_start() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("cancel.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = Uuid::new_v4();
    let source = Arc::new(ControlledFeed {
        sender: Mutex::new(None),
        epoch,
        initial: None,
    });
    let hooks = Arc::new(ObservationTestHooks {
        next: std::sync::atomic::AtomicBool::new(true),
        opened: tokio::sync::Notify::new(),
        canceled: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let server_source = source.clone();
    let server_hooks = hooks.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_connection_mode(
            stream,
            server_source,
            epoch,
            TRANSPORT_LIMITS,
            false,
            None,
            Some(server_hooks),
        )
        .await
        .unwrap();
    });
    let client = LocalAgentClient::connect(socket).await.unwrap();
    let observation = client
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 256,
            },
            after: None,
        })
        .await
        .unwrap();
    hooks.opened.notified().await;
    let sender = source.sender.lock().await.take().unwrap();
    drop(observation);
    tokio::time::timeout(std::time::Duration::from_secs(2), hooks.canceled.notified())
        .await
        .unwrap();
    let canceled = tokio::time::timeout(std::time::Duration::from_secs(2), sender.closed()).await;
    hooks.resume.notify_one();
    assert!(
        canceled.is_ok(),
        "a canceled source must close before its feed starts"
    );
    assert!(client.is_connected());
    let receipt = client
        .observe(ObservationRequest::Receipt {
            epoch,
            command_id: Uuid::new_v4(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt.initial, ObservationValue::Receipt(None)));
    drop(client);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spilled_history_is_complete_in_single_frame_and_chunked_transfers() {
    let text = "x".repeat(crate::chunked::RESPONSE_MEMORY_BYTES + 1);
    for limits in [AccessLimits::DEFAULT, TRANSPORT_LIMITS] {
        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temporary.path().join("spilled.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let epoch = Uuid::new_v4();
        let initial = transcript_initial(text.clone());
        let expected = serde_json::to_vec(&initial).unwrap();
        let source = Arc::new(ControlledFeed {
            sender: Mutex::new(None),
            epoch,
            initial: Some(initial),
        });
        let server_source = source.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream, server_source, epoch, limits)
                .await
                .unwrap();
        });
        let client = LocalAgentClient::connect(socket).await.unwrap();
        let observation = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.observe(ObservationRequest::Overview {
                filter: AgentFilter::default(),
                page: Page::default(),
                after: None,
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(serde_json::to_vec(&observation.initial).unwrap(), expected);
        drop(observation);
        let sender = source.sender.lock().await.take().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), sender.closed())
            .await
            .expect("dropping a received snapshot must cancel its source feed");
        drop(client);
        server.abort();
        if let Err(error) = server.await {
            assert!(error.is_cancelled());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_during_chunked_open_cancels_the_source_before_any_opened_is_published() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("chunk-cancel.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = Uuid::new_v4();
    let source = Arc::new(ControlledFeed {
        sender: Mutex::new(None),
        epoch,
        initial: Some(transcript_initial("x".repeat(FRAME_BYTES))),
    });
    let server_source = source.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_connection(stream, server_source, epoch, TRANSPORT_LIMITS)
            .await
            .unwrap();
    });
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    write_frame(
        &mut stream,
        &ClientMessage::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            handoff: None,
        },
        HANDSHAKE_FRAME_BYTES,
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame::<_, ServerMessage>(&mut stream, HANDSHAKE_FRAME_BYTES)
            .await
            .unwrap(),
        Some(ServerMessage::Hello(_))
    ));
    write_frame(
        &mut stream,
        &ClientMessage::Observe {
            request_id: Uuid::new_v4(),
            query: ObservationRequest::Overview {
                filter: AgentFilter::default(),
                page: Page::default(),
                after: None,
            },
        },
        FRAME_BYTES,
    )
    .await
    .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame::<_, ServerMessage>(&mut stream, FRAME_BYTES),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(
            response,
            Some(ServerMessage::ResponseStart {
                kind: ChunkedResponseKind::Opened,
                chunk_bytes,
                ..
            }) if chunk_bytes == TRANSPORT_LIMITS.chunk_bytes()
        ),
        "an Opened beyond the negotiated frame is chunked at the negotiated size: {response:?}"
    );
    let sender = source.sender.lock().await.take().unwrap();
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(5), sender.closed())
        .await
        .expect("a canceled partial snapshot must close its source feed");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

/// A daemon answers `ShutdownDaemon` and closes the connection at once, so a concurrent write,
/// such as an observation cancel, fails while the reply is still unread.
#[tokio::test]
async fn a_reply_written_before_the_daemon_closed_survives_a_failed_write() {
    use std::io::{Read, Write};
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("closing.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (received, request_arrived) = oneshot::channel();
    let (reply, reply_allowed) = std::sync::mpsc::channel::<()>();
    let (closed, daemon_closed) = std::sync::mpsc::channel::<()>();
    let daemon = std::thread::spawn(move || {
        let read = |stream: &mut std::os::unix::net::UnixStream| {
            let mut length = [0; 4];
            stream.read_exact(&mut length).unwrap();
            let mut frame = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut frame).unwrap();
            serde_json::from_slice::<ClientMessage>(&frame).unwrap()
        };
        let write = |stream: &mut std::os::unix::net::UnixStream, message: &ServerMessage| {
            let frame = serde_json::to_vec(message).unwrap();
            stream
                .write_all(&(frame.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&frame).unwrap();
        };
        let (mut stream, _) = listener.accept().unwrap();
        read(&mut stream);
        write(
            &mut stream,
            &ServerMessage::Hello(ServerHello {
                process_id: 0,
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
                daemon_epoch: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                capabilities: Vec::new(),
                limits: TRANSPORT_LIMITS.into(),
                build: None,
                executable: None,
                upgrade_protocol: None,
                handoff: None,
            }),
        );
        let ClientMessage::ShutdownDaemon { request_id, .. } = read(&mut stream) else {
            panic!("expected a shutdown request");
        };
        received.send(()).unwrap();
        reply_allowed.recv().unwrap();
        write(&mut stream, &ServerMessage::DaemonStopped { request_id });
        drop(stream);
        closed.send(()).unwrap();
    });
    let client = LocalAgentClient::connect(&socket).await.unwrap();
    let stopping = client.clone();
    let shutdown = tokio::spawn(async move { stopping.shutdown_daemon().await });
    request_arrived.await.unwrap();
    reply.send(()).unwrap();
    // Blocking this runtime thread keeps the response reader from seeing the reply before the
    // next write fails on the closed connection.
    daemon_closed.recv().unwrap();
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::FlushTranscripts,
        })
        .await
        .expect_err("the daemon closed the connection");
    assert!(!client.is_connected());
    shutdown
        .await
        .unwrap()
        .expect("the reply sent before the daemon closed is delivered");
    daemon.join().unwrap();
}
