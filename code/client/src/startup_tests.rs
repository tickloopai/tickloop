use std::os::unix::fs::PermissionsExt;

use tickloop_code_access::{
    AccessError, AccessLimits, CLIENT_HANDOFF_VERSION, ClientHandoffHello, ClientMessage,
    HANDSHAKE_FRAME_BYTES, ServerHandoffHello, ServerHello, ServerMessage,
};

use tokio::{net::UnixListener, sync::oneshot};
use uuid::Uuid;

use super::*;

#[tokio::test]
async fn startup_waits_for_a_handoff_handshake_without_starting_another_daemon() {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .unwrap();
    let endpoint = directory.path().join(SOCKET_NAME);
    let listener = UnixListener::bind(&endpoint).unwrap();
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
    let started = directory.path().join("started");
    let executable = directory.path().join("must-not-start");
    std::fs::write(
        &executable,
        format!("#!/bin/sh\nprintf started > '{}'\n", started.display()),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let epoch = Uuid::new_v4();
    let session = Uuid::new_v4();
    let (finish, finished) = oneshot::channel();
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello = frame::read_frame::<_, ClientMessage>(&mut stream, HANDSHAKE_FRAME_BYTES)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                hello,
                ClientMessage::Hello {
                    handoff: Some(ClientHandoffHello {
                        version: CLIENT_HANDOFF_VERSION,
                        resume: None
                    }),
                    ..
                }
            ));
            let response = if attempt == 0 {
                ServerMessage::Error {
                    request_id: None,
                    error: AccessError::new(
                        AccessErrorCode::HandoffNotAccepted,
                        "the daemon is transferring its connections",
                    ),
                }
            } else {
                ServerMessage::Hello(ServerHello {
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR,
                    daemon_epoch: epoch,
                    process_id: std::process::id(),
                    session_id: session,
                    capabilities: Vec::new(),
                    limits: AccessLimits::DEFAULT.into(),
                    build: None,
                    executable: None,
                    upgrade_protocol: None,
                    handoff: Some(ServerHandoffHello {
                        version: CLIENT_HANDOFF_VERSION,
                        resume_token: "successor-session-proof".into(),
                    }),
                })
            };
            frame::write_frame(&mut stream, &response, HANDSHAKE_FRAME_BYTES)
                .await
                .unwrap();
            if attempt == 1 {
                let _ = finished.await;
                return;
            }
        }
    });
    let client = tokio::time::timeout(
        STARTUP_DEADLINE * 2,
        connect_or_start_at(directory.path().into(), executable),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(client.daemon_epoch(), epoch);
    assert_eq!(client.session_id(), session);
    assert!(
        !started.exists(),
        "a handoff must not launch another daemon"
    );
    assert!(!directory.path().join(LOCK_NAME).exists());
    let _ = finish.send(());
    server.await.unwrap();
}
