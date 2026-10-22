use std::{collections::HashMap, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
    sync::{Mutex, oneshot},
    task::AbortHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::ingress::{DaemonIngress, SessionTicket};
use crate::lifecycle::DaemonLifecycle;
use crate::{chunked::EncodedResponse, views::command_origin};
use tickloop_agent_client::frame::{read_frame_body, verify_peer, write_frame};
use tickloop_agent_protocol::{
    AccessCapability, AccessError, AccessErrorCode, AccessLimits, AgentAccess, ChunkedResponseKind,
    ClientMessage, DisplayUpdate, HANDSHAKE_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    ServerHello, ServerMessage, bounded_update, decode_frame,
};

const OBSERVATION_CANCELLED_BEFORE_OPENING: &str = "observation cancelled before opening";
const UNKNOWN_REQUEST_MESSAGE: &str = "this request is unknown to the running daemon, which is another Tickloop build; relaunch tickloop to choose a build";

/// An observation feed a connection is writing. Cancelling an observation on a live connection
/// stops its task between frames; aborting is reserved for connections that are closing, because
/// an aborted write would leave part of a frame on the shared stream.
struct FeedTask {
    stop: CancellationToken,
    task: AbortHandle,
}

#[derive(Clone)]
pub(super) struct DaemonConnection {
    lifecycle: Arc<DaemonLifecycle>,
    ingress: Arc<DaemonIngress>,
}

struct ConnectionCleanup {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    observations: Arc<Mutex<HashMap<Uuid, FeedTask>>>,
    access: Arc<dyn AgentAccess>,
    cancel: CancellationToken,
    session_id: Uuid,
    session: Option<SessionTicket>,
}

/// The id of a request whose kind this build does not know: the single externally tagged
/// variant's `request_id`, when the body is otherwise a well-formed request.
fn unknown_request_id(body: &[u8]) -> Option<Uuid> {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let mut variants = value.as_object()?.values();
    let fields = variants.next()?.as_object()?;
    if variants.next().is_some() {
        return None;
    }
    fields.get("request_id")?.as_str()?.parse().ok()
}

#[cfg(test)]
pub(super) struct ObservationTestHooks {
    pub(super) next: std::sync::atomic::AtomicBool,
    pub(super) opened: tokio::sync::Notify,
    pub(super) canceled: tokio::sync::Notify,
    pub(super) resume: tokio::sync::Notify,
}
impl Drop for ConnectionCleanup {
    fn drop(&mut self) {
        self.cancel.cancel();
        let writer = self.writer.clone();
        let observations = self.observations.clone();
        let access = self.access.clone();
        let session_id = self.session_id;
        let close_session = self
            .session
            .as_ref()
            .is_none_or(|session| !session.preserves_lease());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for (_, feed) in observations.lock().await.drain() {
                    feed.task.abort();
                }
                let _ = writer.lock().await.shutdown().await;
                if close_session {
                    let _ = access
                        .show_view(DisplayUpdate::SessionClosed { session_id })
                        .await;
                }
            });
        }
    }
}

pub async fn serve_listener(
    listener: UnixListener,
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    limits: AccessLimits,
) -> Result<(), AccessError> {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        let access = access.clone();
        tokio::spawn(async move {
            let _ = serve_connection(stream, access, epoch, limits).await;
        });
    }
}

/// Serves the daemon endpoint until `shutdown` is cancelled, by a `ShutdownDaemon` request on
/// any connection or by the daemon itself.
pub async fn serve_daemon_listener(
    listener: UnixListener,
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    limits: AccessLimits,
    shutdown: CancellationToken,
) -> Result<(), AccessError> {
    serve_daemon_listener_with_lifecycle(
        listener,
        access,
        epoch,
        limits,
        Arc::new(DaemonLifecycle::new(shutdown)),
        Arc::new(DaemonIngress::new(epoch, limits)),
    )
    .await
}

pub(crate) async fn serve_daemon_listener_with_lifecycle(
    listener: UnixListener,
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    limits: AccessLimits,
    lifecycle: Arc<DaemonLifecycle>,
    ingress: Arc<DaemonIngress>,
) -> Result<(), AccessError> {
    loop {
        let incoming = tokio::select! { _ = lifecycle.shutdown().cancelled() => return Ok(()), incoming = listener.accept() => incoming };
        let (stream, _) = incoming.map_err(|error| AccessError::unavailable(error.to_string()))?;
        let access = access.clone();
        let connection = DaemonConnection {
            lifecycle: lifecycle.clone(),
            ingress: ingress.clone(),
        };
        tokio::spawn(async move {
            let _ = serve_connection_mode(
                stream,
                access,
                epoch,
                limits,
                false,
                Some(connection),
                #[cfg(test)]
                None,
            )
            .await;
        });
    }
}

pub async fn serve_worker_listener(
    listener: UnixListener,
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    limits: AccessLimits,
) -> Result<(), AccessError> {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        let access = access.clone();
        tokio::spawn(async move {
            let _ = serve_connection_mode(
                stream,
                access,
                epoch,
                limits,
                true,
                None,
                #[cfg(test)]
                None,
            )
            .await;
        });
    }
}

pub async fn serve_connection(
    stream: UnixStream,
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    limits: AccessLimits,
) -> Result<(), AccessError> {
    serve_connection_mode(
        stream,
        access,
        epoch,
        limits,
        false,
        None,
        #[cfg(test)]
        None,
    )
    .await
}

pub(super) async fn serve_connection_mode(
    mut stream: UnixStream,
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    limits: AccessLimits,
    forwarded_sessions: bool,
    daemon_shutdown: Option<DaemonConnection>,
    #[cfg(test)] observation_hooks: Option<Arc<ObservationTestHooks>>,
) -> Result<(), AccessError> {
    verify_peer(&stream)?;
    let hello = tickloop_agent_client::daemon_management::read::<_, ClientMessage>(&mut stream)
        .await
        .map_err(AccessError::invalid)?;
    let (minor, handoff) = match hello {
        Some(ClientMessage::Hello {
            major: PROTOCOL_MAJOR,
            handoff,
            ..
        }) => (PROTOCOL_MINOR, handoff),
        Some(ClientMessage::Hello {
            major: tickloop_agent_protocol::daemon_management::CONTROL_MAJOR,
            minor: 0,
            ..
        }) if daemon_shutdown.is_some() => {
            return crate::daemon_management::serve_shutdown(
                stream,
                epoch,
                (*daemon_shutdown
                    .expect("daemon management session")
                    .lifecycle)
                    .clone(),
            )
            .await
            .map_err(AccessError::invalid);
        }
        Some(ClientMessage::Hello { major, minor, .. }) => {
            write_frame(
                &mut stream,
                &ServerMessage::Error {
                    request_id: None,
                    error: AccessError::new(
                        AccessErrorCode::VersionMismatch,
                        format!(
                            "AgentAccess client protocol {major}.{minor} is incompatible with server {PROTOCOL_MAJOR}.{PROTOCOL_MINOR}; use the same Tickloop version",
                        ),
                    ),
                },
                HANDSHAKE_FRAME_BYTES,
            )
            .await?;
            return Ok(());
        }
        _ => {
            write_frame(
                &mut stream,
                &ServerMessage::Error {
                    request_id: None,
                    error: AccessError::new(
                        AccessErrorCode::VersionMismatch,
                        "AgentAccess connection must begin with protocol negotiation",
                    ),
                },
                HANDSHAKE_FRAME_BYTES,
            )
            .await?;
            return Ok(());
        }
    };
    let mut session = if let Some(daemon) = &daemon_shutdown {
        match daemon.ingress.register(handoff) {
            Ok(session) => Some(session),
            Err(error) => {
                write_frame(
                    &mut stream,
                    &ServerMessage::Error {
                        request_id: None,
                        error,
                    },
                    HANDSHAKE_FRAME_BYTES,
                )
                .await?;
                return Ok(());
            }
        }
    } else {
        None
    };
    let session_id = session
        .as_ref()
        .map_or_else(Uuid::new_v4, |session| session.session_id);
    let capabilities = vec![
        AccessCapability::Overview,
        AccessCapability::AgentDetails,
        AccessCapability::Records,
        AccessCapability::Receipts,
        AccessCapability::Create,
        AccessCapability::LoadRecord,
        AccessCapability::Submit,
        AccessCapability::ContinueLoop,
        AccessCapability::DecidePermission,
        AccessCapability::AnswerQuestion,
        AccessCapability::Configure,
        AccessCapability::EditHistory,
        AccessCapability::ManageLoop,
        AccessCapability::ManageTask,
        AccessCapability::ManageGoal,
        AccessCapability::ClearCompletedTasks,
        AccessCapability::EnqueueAfterTurn,
        AccessCapability::RefreshConfiguration,
        AccessCapability::FlushTranscripts,
        AccessCapability::DisplayRegistration,
        AccessCapability::TrustWorkingDirectory,
        AccessCapability::ChunkedResponses,
    ];
    write_frame(
        &mut stream,
        &ServerMessage::Hello(ServerHello {
            major: PROTOCOL_MAJOR,
            minor,
            daemon_epoch: epoch,
            process_id: std::process::id(),
            session_id,
            capabilities,
            limits: limits.into(),
            build: tickloop_agent_protocol::build_identity().map(str::to_owned),
            executable: std::env::current_exe().ok(),
            upgrade_protocol: daemon_shutdown
                .as_ref()
                .map(|_| tickloop_agent_protocol::RUNTIME_HANDOFF_VERSION),
            handoff: session.as_ref().and_then(|session| session.handoff.clone()),
        }),
        HANDSHAKE_FRAME_BYTES,
    )
    .await?;
    let frame_bytes = limits.frame_bytes();
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let observations: Arc<Mutex<HashMap<Uuid, FeedTask>>> = Arc::new(Mutex::new(HashMap::new()));
    let pending_observations: Arc<Mutex<HashMap<Uuid, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let cancel = daemon_shutdown
        .as_ref()
        .map_or_else(CancellationToken::new, |lifecycle| {
            lifecycle.lifecycle.shutdown().child_token()
        });
    let peer = session
        .as_mut()
        .map(|session| session.attach(writer.clone(), cancel.clone(), frame_bytes));
    let preserves_lease = session.as_ref().is_some_and(SessionTicket::preserves_lease);
    let _cleanup = ConnectionCleanup {
        writer: writer.clone(),
        observations: observations.clone(),
        access: access.clone(),
        cancel: cancel.clone(),
        session_id,
        session,
    };
    let result = loop {
        let incoming = tokio::select! { _ = cancel.cancelled() => break Ok(()), incoming = read_frame_body(&mut reader, frame_bytes) => incoming };
        let body = match incoming {
            Ok(Some(body)) => body,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        let message = match decode_frame::<ClientMessage>(&body) {
            Ok(message) => message,
            // A request of another build is refused alone, so its View keeps the connection and
            // shows why instead of reconnecting into the same refusal.
            Err(error) => match unknown_request_id(&body) {
                Some(request_id) => {
                    write_frame(
                        &mut *writer.lock().await,
                        &ServerMessage::Error {
                            request_id: Some(request_id),
                            error: AccessError::new(
                                AccessErrorCode::Unsupported,
                                UNKNOWN_REQUEST_MESSAGE,
                            ),
                        },
                        frame_bytes,
                    )
                    .await?;
                    continue;
                }
                None => break Err(error),
            },
        };
        if let ClientMessage::ShutdownDaemon {
            request_id,
            daemon_epoch,
        } = message
        {
            if daemon_epoch != epoch {
                write_frame(
                    &mut *writer.lock().await,
                    &ServerMessage::Error {
                        request_id: Some(request_id),
                        error: AccessError::new(
                            AccessErrorCode::EpochChanged,
                            "daemon shutdown belongs to another epoch",
                        ),
                    },
                    frame_bytes,
                )
                .await?;
            } else if let Some(shutdown) = &daemon_shutdown {
                let stop = match shutdown.lifecycle.reserve_stop() {
                    Ok(stop) => stop,
                    Err(error) => {
                        write_frame(
                            &mut *writer.lock().await,
                            &ServerMessage::Error {
                                request_id: Some(request_id),
                                error,
                            },
                            frame_bytes,
                        )
                        .await?;
                        continue;
                    }
                };
                write_frame(
                    &mut *writer.lock().await,
                    &ServerMessage::DaemonStopped { request_id },
                    frame_bytes,
                )
                .await?;
                drop(stop);
                break Ok(());
            } else {
                write_frame(
                    &mut *writer.lock().await,
                    &ServerMessage::Error {
                        request_id: Some(request_id),
                        error: AccessError::new(
                            AccessErrorCode::Unsupported,
                            "this endpoint does not expose daemon administration",
                        ),
                    },
                    frame_bytes,
                )
                .await?;
            }
            continue;
        }
        if let ClientMessage::HandoffReady { transfer_id } = message {
            if let Some(peer) = &peer {
                peer.acknowledge(transfer_id);
            }
            continue;
        }
        if let ClientMessage::CancelObservation { observation_id } = message {
            if let Some(pending) = pending_observations.lock().await.remove(&observation_id) {
                pending.cancel();
            }
            if let Some(feed) = observations.lock().await.remove(&observation_id) {
                feed.stop.cancel();
            }
            #[cfg(test)]
            if let Some(hooks) = &observation_hooks {
                hooks.canceled.notify_one();
            }
            continue;
        }
        let request_id = match &message {
            ClientMessage::Observe { request_id, .. }
            | ClientMessage::Execute { request_id, .. }
            | ClientMessage::ShowView { request_id, .. } => *request_id,
            _ => continue,
        };
        let observation_cancel = if matches!(&message, ClientMessage::Observe { .. }) {
            let pending = cancel.child_token();
            pending_observations
                .lock()
                .await
                .insert(request_id, pending.clone());
            Some(pending)
        } else {
            None
        };
        let request_activity = peer.as_ref().map(|peer| peer.request());
        let ingress = daemon_shutdown
            .as_ref()
            .map(|daemon| daemon.ingress.clone());
        let pending_observations = pending_observations.clone();
        let access = access.clone();
        let writer = writer.clone();
        let observations = observations.clone();
        let cancel = cancel.clone();
        #[cfg(test)]
        let observation_hooks = observation_hooks.clone();
        tokio::spawn(async move {
            let _request_activity = request_activity;
            let command_response = matches!(&message, ClientMessage::Execute { .. });
            let response = match message {
                ClientMessage::Observe { request_id, query } => {
                    let observation_cancel =
                        observation_cancel.expect("Observe has a cancellation scope");
                    let result = async {
                        tokio::select! {
                            biased;
                            () = observation_cancel.cancelled() => Err(AccessError::unavailable(OBSERVATION_CANCELLED_BEFORE_OPENING)),
                            result = access.observe(query) => result,
                        }
                    }.await;
                    let result = result.and_then(|observation| {
                        if observation_cancel.is_cancelled() {
                            Err(AccessError::unavailable(
                                OBSERVATION_CANCELLED_BEFORE_OPENING,
                            ))
                        } else {
                            Ok(observation)
                        }
                    });
                    match result {
                        Ok(observation) => {
                            let observation_id = request_id;
                            let feed = observation.has_feed();
                            let (initial, cursor, reset, changes, terminal_error) =
                                observation.into_parts();
                            let response = ServerMessage::Opened {
                                request_id,
                                observation_id,
                                initial,
                                cursor,
                                reset,
                                feed,
                            };
                            let start = if let Some(mut changes) = changes {
                                let (start, started) = oneshot::channel();
                                let writer = writer.clone();
                                let cancel = cancel.clone();
                                let stop = observation_cancel.clone();
                                let task_stop = stop.clone();
                                let active = observations.clone();
                                let task = tokio::spawn(async move {
                                    let ready = tokio::select! {
                                        _ = cancel.cancelled() => return,
                                        _ = task_stop.cancelled() => return,
                                        ready = started => ready,
                                    };
                                    if ready.is_err() {
                                        return;
                                    }
                                    loop {
                                        // Stopping is checked only here, never while a frame is being
                                        // written.
                                        let update = tokio::select! {
                                            biased;
                                            _ = cancel.cancelled() => break,
                                            _ = task_stop.cancelled() => break,
                                            update = changes.recv() => update,
                                        };
                                        let Some(update) = update else {
                                            let error = terminal_error
                                                .as_ref()
                                                .and_then(|error| error.lock().unwrap().clone())
                                                .unwrap_or_else(|| {
                                                    AccessError::unavailable(
                                                        "execution observation ended",
                                                    )
                                                });
                                            if write_frame(
                                                &mut *writer.lock().await,
                                                &ServerMessage::ObservationClosed {
                                                    observation_id,
                                                    error,
                                                },
                                                frame_bytes,
                                            )
                                            .await
                                            .is_err()
                                            {
                                                cancel.cancel();
                                            }
                                            break;
                                        };
                                        if write_frame(
                                            &mut *writer.lock().await,
                                            &ServerMessage::Changed {
                                                observation_id,
                                                update: bounded_update(update),
                                            },
                                            frame_bytes,
                                        )
                                        .await
                                        .is_err()
                                        {
                                            cancel.cancel();
                                            break;
                                        }
                                    }
                                    active.lock().await.remove(&observation_id);
                                });
                                observations.lock().await.insert(
                                    observation_id,
                                    FeedTask {
                                        stop,
                                        task: task.abort_handle(),
                                    },
                                );
                                Some(start)
                            } else {
                                None
                            };
                            pending_observations.lock().await.remove(&request_id);
                            if !write_response(
                                &mut *writer.lock().await,
                                response,
                                request_id,
                                false,
                                &cancel,
                                limits,
                            )
                            .await
                            {
                                if let Some(feed) =
                                    observations.lock().await.remove(&observation_id)
                                {
                                    feed.task.abort();
                                }
                                return;
                            }
                            #[cfg(test)]
                            if let Some(hooks) = observation_hooks
                                && hooks.next.swap(false, std::sync::atomic::Ordering::SeqCst)
                            {
                                hooks.opened.notify_one();
                                hooks.resume.notified().await;
                            }
                            if let Some(start) = start {
                                let _ = start.send(());
                            }
                            return;
                        }
                        Err(error) => {
                            pending_observations.lock().await.remove(&request_id);
                            ServerMessage::Error {
                                request_id: Some(request_id),
                                error,
                            }
                        }
                    }
                }
                ClientMessage::Execute {
                    request_id,
                    command,
                } => match if !forwarded_sessions
                    && command_origin(&command.command)
                        .is_some_and(|source| source.view.session_id != session_id)
                {
                    Err(AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "input source does not belong to this connection session",
                    ))
                } else {
                    access.execute(command).await
                } {
                    Ok(receipt) => ServerMessage::Receipt {
                        request_id,
                        receipt,
                    },
                    Err(error) => ServerMessage::Error {
                        request_id: Some(request_id),
                        error,
                    },
                },
                ClientMessage::ShowView { request_id, update } => {
                    let view_session = match &update {
                        DisplayUpdate::Displayed { view, .. }
                        | DisplayUpdate::Scene { view, .. }
                        | DisplayUpdate::Renew { view, .. }
                        | DisplayUpdate::Left { view, .. } => Some(view.session_id),
                        DisplayUpdate::SessionClosed { .. } => None,
                    };
                    if (!forwarded_sessions && view_session != Some(session_id))
                        || cancel.is_cancelled()
                    {
                        ServerMessage::Error {
                            request_id: Some(request_id),
                            error: AccessError::new(
                                AccessErrorCode::AccessDenied,
                                "View does not belong to this connection session",
                            ),
                        }
                    } else {
                        match access.show_view(update).await {
                            Ok(receipt) => ServerMessage::Display {
                                request_id,
                                receipt,
                            },
                            Err(error) => ServerMessage::Error {
                                request_id: Some(request_id),
                                error,
                            },
                        }
                    }
                }
                _ => return,
            };
            let response = match response {
                ServerMessage::Error {
                    request_id: Some(request_id),
                    error,
                } if error.code == AccessErrorCode::HandoffNotAccepted
                    && ingress
                        .as_ref()
                        .and_then(|ingress| ingress.committed_transfer())
                        .is_some() =>
                {
                    ServerMessage::NotAccepted {
                        request_id,
                        transfer_id: ingress.as_ref().unwrap().committed_transfer().unwrap(),
                    }
                }
                response => response,
            };
            write_response(
                &mut *writer.lock().await,
                response,
                request_id,
                command_response,
                &cancel,
                limits,
            )
            .await;
        });
    };
    cancel.cancel();
    for (_, feed) in observations.lock().await.drain() {
        feed.task.abort();
    }
    if !preserves_lease {
        let _ = access
            .show_view(DisplayUpdate::SessionClosed { session_id })
            .await;
    }
    result
}

async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: ServerMessage,
    request_id: Uuid,
    command_response: bool,
    cancel: &CancellationToken,
    limits: AccessLimits,
) -> bool {
    let kind = ChunkedResponseKind::of(&response);
    let mut encoding_failed = false;
    let result = if let Some(kind) = kind {
        let encoding_cancel = cancel.clone();
        let encoded =
            tokio::task::spawn_blocking(move || EncodedResponse::new(response, encoding_cancel))
                .await;
        encoding_failed = !matches!(&encoded, Ok(Ok(_)));
        match encoded {
            Ok(Ok(encoded)) if encoded.total_bytes <= limits.frame_bytes() as u64 => {
                write_encoded_frame(writer, encoded).await
            }
            Ok(Ok(encoded)) => {
                write_chunked_response(writer, encoded, request_id, kind, cancel, limits).await
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(AccessError::invalid(format!(
                "AgentAccess response could not be encoded: {error}"
            ))),
        }
    } else {
        write_frame(writer, &response, limits.frame_bytes()).await
    };
    match result {
        Ok(()) => true,
        Err(error)
            if encoding_failed
                || matches!(
                    error.code,
                    AccessErrorCode::PayloadTooLarge | AccessErrorCode::Invalid
                ) =>
        {
            let message = if error.code == AccessErrorCode::PayloadTooLarge {
                "AgentAccess response exceeds the transport limit; query the original CommandId before repeating any command"
            } else {
                "AgentAccess response could not be encoded; query the original CommandId before repeating any command"
            };
            if write_frame(
                writer,
                &ServerMessage::Error {
                    request_id: Some(request_id),
                    error: AccessError::new(
                        if command_response {
                            AccessErrorCode::OutcomeUnknown
                        } else {
                            error.code
                        },
                        message,
                    ),
                },
                limits.frame_bytes(),
            )
            .await
            .is_err()
            {
                cancel.cancel();
            }
            false
        }
        Err(_) => {
            cancel.cancel();
            false
        }
    }
}

async fn write_encoded_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    encoded: EncodedResponse,
) -> Result<(), AccessError> {
    writer
        .write_u32(encoded.total_bytes as u32)
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    let mut reader = encoded.into_reader();
    tokio::io::copy(&mut reader, writer)
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))
}

async fn write_chunked_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    encoded: EncodedResponse,
    request_id: Uuid,
    kind: ChunkedResponseKind,
    cancel: &CancellationToken,
    limits: AccessLimits,
) -> Result<(), AccessError> {
    let transfer_id = Uuid::new_v4();
    let total_bytes = encoded.total_bytes;
    let frame_bytes = limits.frame_bytes();
    let chunk_bytes = limits.chunk_bytes();
    let mut reader = encoded.into_reader();
    let transfer = async {
        write_frame(
            writer,
            &ServerMessage::ResponseStart {
                request_id,
                transfer_id,
                kind,
                total_bytes,
                chunk_bytes,
            },
            frame_bytes,
        )
        .await?;
        let mut offset = 0;
        let mut bytes = vec![0; chunk_bytes];
        let mut digest = Sha256::new();
        while offset < total_bytes {
            let length = (total_bytes - offset).min(chunk_bytes as u64) as usize;
            reader
                .read_exact(&mut bytes[..length])
                .await
                .map_err(|error| AccessError::unavailable(error.to_string()))?;
            digest.update(&bytes[..length]);
            write_frame(
                writer,
                &ServerMessage::ResponseChunk {
                    transfer_id,
                    offset,
                    base64: BASE64.encode(&bytes[..length]),
                },
                frame_bytes,
            )
            .await?;
            offset += length as u64;
        }
        write_frame(
            writer,
            &ServerMessage::ResponseEnd {
                transfer_id,
                total_bytes,
                sha256: format!("{:x}", digest.finalize()),
            },
            frame_bytes,
        )
        .await
    };
    tokio::select! { _ = cancel.cancelled() => Err(AccessError::unavailable("response transfer canceled")), result = transfer => result }
}
