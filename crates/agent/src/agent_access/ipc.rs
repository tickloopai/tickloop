use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{Mutex, mpsc, oneshot},
    task::AbortHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    AgentAccess, Observation,
    chunked::{EncodedResponse, IncomingTransfer, response_kind},
    protocol::*,
};
use crate::protocol::{AccessLimits, FEED_CAPACITY, HANDSHAKE_FRAME_BYTES};

const OBSERVATION_CANCELLED_BEFORE_OPENING: &str = "observation cancelled before opening";

#[derive(Clone)]
pub struct LocalAgentClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    hello: ServerHello,
    limits: AccessLimits,
    pending: Mutex<HashMap<Uuid, oneshot::Sender<Result<Reply, AccessError>>>>,
    subscriptions: Mutex<HashMap<Uuid, Subscription>>,
    cancel: CancellationToken,
}

struct PendingObservationCancel {
    writer: Weak<Mutex<OwnedWriteHalf>>,
    connection: CancellationToken,
    limits: AccessLimits,
    request_id: Uuid,
    written: Option<oneshot::Receiver<()>>,
}

async fn write_observation_cancel(
    writer: Arc<Mutex<OwnedWriteHalf>>,
    connection: CancellationToken,
    limits: AccessLimits,
    observation_id: Uuid,
) {
    let result = tokio::select! {
        biased;
        () = connection.cancelled() => return,
        result = async {
            write_frame(&mut *writer.lock().await, &ClientMessage::CancelObservation { observation_id }, limits.frame_bytes()).await
        } => result,
    };
    if result.is_err() {
        connection.cancel();
    }
}

impl Drop for PendingObservationCancel {
    fn drop(&mut self) {
        let Some(written) = self.written.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let writer = self.writer.clone();
        let connection = self.connection.clone();
        let limits = self.limits;
        let observation_id = self.request_id;
        runtime.spawn(async move {
            tokio::select! {
                biased;
                () = connection.cancelled() => return,
                result = written => if result.is_err() { return; },
            }
            let Some(writer) = writer.upgrade() else {
                return;
            };
            write_observation_cancel(writer, connection, limits, observation_id).await;
        });
    }
}

struct Subscription {
    sender: mpsc::Sender<ObservationUpdate>,
    cancel: CancellationToken,
    terminal_error: Arc<std::sync::Mutex<Option<AccessError>>>,
}

/// An observation feed a connection is writing. Cancelling an observation on a live connection
/// stops its task between frames; aborting is reserved for connections that are closing, because
/// an aborted write would leave part of a frame on the shared stream.
struct FeedTask {
    stop: CancellationToken,
    task: AbortHandle,
}

struct ConnectionCleanup {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    observations: Arc<Mutex<HashMap<Uuid, FeedTask>>>,
    access: Arc<dyn AgentAccess>,
    cancel: CancellationToken,
    session_id: Uuid,
}

#[cfg(test)]
struct ObservationTestHooks {
    next: std::sync::atomic::AtomicBool,
    opened: tokio::sync::Notify,
    canceled: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
impl Drop for ConnectionCleanup {
    fn drop(&mut self) {
        self.cancel.cancel();
        let writer = self.writer.clone();
        let observations = self.observations.clone();
        let access = self.access.clone();
        let session_id = self.session_id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for (_, feed) in observations.lock().await.drain() {
                    feed.task.abort();
                }
                let _ = writer.lock().await.shutdown().await;
                let _ = access
                    .show_view(DisplayUpdate::SessionClosed { session_id })
                    .await;
            });
        }
    }
}

enum Reply {
    Message(Box<ServerMessage>),
    Opened {
        initial: ObservationValue,
        cursor: Cursor,
        reset: Option<ResetReason>,
        changes: Option<mpsc::Receiver<ObservationUpdate>>,
        terminal_error: Option<Arc<std::sync::Mutex<Option<AccessError>>>>,
    },
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl LocalAgentClient {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, AccessError> {
        let mut stream = UnixStream::connect(path)
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        verify_peer(&stream)?;
        let (hello, limits) = negotiate(&mut stream).await?;
        let (reader, writer) = stream.into_split();
        let inner = Arc::new(ClientInner {
            writer: Arc::new(Mutex::new(writer)),
            hello,
            limits,
            pending: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            cancel: CancellationToken::new(),
        });
        tokio::spawn(read_responses(
            reader,
            Arc::downgrade(&inner),
            inner.cancel.clone(),
            limits,
        ));
        Ok(Self { inner })
    }
    pub fn is_connected(&self) -> bool {
        !self.inner.cancel.is_cancelled()
    }

    pub fn daemon_epoch(&self) -> Uuid {
        self.inner.hello.daemon_epoch
    }
    pub fn protocol(&self) -> &ServerHello {
        &self.inner.hello
    }
    pub async fn shutdown_daemon(&self) -> Result<(), AccessError> {
        let request_id = Uuid::new_v4();
        match self
            .request(
                request_id,
                ClientMessage::ShutdownDaemon {
                    request_id,
                    daemon_epoch: self.daemon_epoch(),
                },
            )
            .await?
        {
            Reply::Message(message) if matches!(*message, ServerMessage::DaemonStopped { .. }) => {
                Ok(())
            }
            _ => Err(AccessError::invalid(
                "server returned an invalid daemon shutdown reply",
            )),
        }
    }

    pub fn server_process_id(&self) -> u32 {
        self.inner.hello.process_id
    }

    pub fn session_id(&self) -> Uuid {
        self.inner.hello.session_id
    }

    async fn request(
        &self,
        request_id: Uuid,
        message: ClientMessage,
    ) -> Result<Reply, AccessError> {
        self.request_with_notification(request_id, message, None)
            .await
    }

    async fn request_with_notification(
        &self,
        request_id: Uuid,
        message: ClientMessage,
        written: Option<oneshot::Sender<()>>,
    ) -> Result<Reply, AccessError> {
        if self.inner.cancel.is_cancelled() {
            return Err(AccessError::unavailable("daemon connection is closed"));
        }
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.lock().await.insert(request_id, sender);
        let detached = written.is_some();
        let inner = Arc::downgrade(&self.inner);
        let writer = self.inner.writer.clone();
        let connection = self.inner.cancel.clone();
        let frame_bytes = self.inner.limits.frame_bytes();
        let send = async move {
            let result = tokio::select! {
                biased;
                () = connection.cancelled() => Err(AccessError::unavailable("daemon connection is closed")),
                result = async { write_frame(&mut *writer.lock().await, &message, frame_bytes).await } => result,
            };
            if let Err(error) = result {
                if let Some(inner) = inner.upgrade() {
                    inner.pending.lock().await.remove(&request_id);
                }
                if error.code == AccessErrorCode::Unavailable {
                    connection.cancel();
                }
                return Err(error);
            }
            if let Some(written) = written {
                let _ = written.send(());
            }
            Ok(())
        };
        if detached {
            // Finish the request frame before cancellation is written on the shared stream.
            tokio::spawn(send)
                .await
                .map_err(|_| AccessError::unavailable("observation request writer stopped"))??;
        } else {
            send.await?;
        }
        receiver.await.map_err(|_| {
            AccessError::unavailable("daemon disconnected before returning a result")
        })?
    }
}

#[async_trait]
impl AgentAccess for LocalAgentClient {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let request_id = Uuid::new_v4();
        let (written, waiting) = oneshot::channel();
        let mut cancel = PendingObservationCancel {
            writer: Arc::downgrade(&self.inner.writer),
            connection: self.inner.cancel.clone(),
            limits: self.inner.limits,
            request_id,
            written: Some(waiting),
        };
        let result = self
            .request_with_notification(
                request_id,
                ClientMessage::Observe { request_id, query },
                Some(written),
            )
            .await;
        cancel.written = None;
        match result? {
            Reply::Opened {
                initial,
                cursor,
                reset,
                changes,
                terminal_error,
            } => Ok(Observation {
                initial,
                cursor,
                reset,
                changes,
                terminal_error,
            }),
            _ => Err(AccessError::invalid(
                "daemon returned the wrong observation reply",
            )),
        }
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let request_id = Uuid::new_v4();
        match self
            .request(
                request_id,
                ClientMessage::Execute {
                    request_id,
                    command,
                },
            )
            .await
        {
            Ok(Reply::Message(message)) => match *message {
                ServerMessage::Receipt { receipt, .. } => Ok(receipt),
                _ => Err(AccessError::invalid(
                    "daemon returned the wrong command reply",
                )),
            },
            Err(error) if error.code == AccessErrorCode::Unavailable => Err(AccessError::new(
                AccessErrorCode::OutcomeUnknown,
                "daemon disconnected; query the original CommandId before retrying",
            )),
            Err(error) => Err(error),
            _ => Err(AccessError::invalid(
                "daemon returned the wrong command reply",
            )),
        }
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        let request_id = Uuid::new_v4();
        match self
            .request(request_id, ClientMessage::ShowView { request_id, update })
            .await?
        {
            Reply::Message(message) => match *message {
                ServerMessage::Display { receipt, .. } => Ok(receipt),
                _ => Err(AccessError::invalid(
                    "daemon returned the wrong display reply",
                )),
            },
            _ => Err(AccessError::invalid(
                "daemon returned the wrong display reply",
            )),
        }
    }
}

/// The version every AgentAccess major's Hello leads with. It is read before the rest of the
/// Hello, so a peer whose Hello has another shape is still reported as an incompatible version.
#[derive(Deserialize)]
struct PeerVersion {
    major: u16,
    minor: u16,
}

async fn negotiate(stream: &mut UnixStream) -> Result<(ServerHello, AccessLimits), AccessError> {
    write_frame(
        stream,
        &ClientMessage::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        },
        HANDSHAKE_FRAME_BYTES,
    )
    .await?;
    let reply = read_frame::<_, serde_json::Value>(stream, HANDSHAKE_FRAME_BYTES)
        .await?
        .ok_or_else(|| {
            AccessError::unavailable("daemon disconnected during protocol negotiation")
        })?;
    if let Some(Ok(peer)) = reply.get("Hello").map(PeerVersion::deserialize)
        && peer.major != PROTOCOL_MAJOR
    {
        return Err(AccessError::new(
            AccessErrorCode::VersionMismatch,
            format!(
                "AgentAccess peer protocol {}.{} is incompatible with client {}.{}; use the same Tick Loop version",
                peer.major, peer.minor, PROTOCOL_MAJOR, PROTOCOL_MINOR,
            ),
        ));
    }
    let hello = match serde_json::from_value::<ServerMessage>(reply)
        .map_err(|error| AccessError::invalid(error.to_string()))?
    {
        ServerMessage::Hello(hello) => hello,
        ServerMessage::Error { error, .. } => return Err(error),
        _ => {
            return Err(AccessError::new(
                AccessErrorCode::VersionMismatch,
                "daemon did not complete protocol negotiation",
            ));
        }
    };
    let limits = AccessLimits::new(hello.limits.frame_bytes, hello.limits.chunk_bytes).map_err(
        |error| {
            AccessError::new(
                AccessErrorCode::VersionMismatch,
                format!(
                    "AgentAccess peer announced unusable transport limits ({error}); use the same Tick Loop version"
                ),
            )
        },
    )?;
    Ok((hello, limits))
}

async fn read_responses(
    mut reader: OwnedReadHalf,
    inner: Weak<ClientInner>,
    cancel: CancellationToken,
    limits: AccessLimits,
) {
    let mut transfer: Option<IncomingTransfer> = None;
    loop {
        let response = tokio::select! { _ = cancel.cancelled() => break, response = read_frame::<_, ServerMessage>(&mut reader, limits.frame_bytes()) => response };
        let Ok(Some(response)) = response else {
            break;
        };
        let Some(inner) = inner.upgrade() else {
            break;
        };
        let response = match response {
            ServerMessage::ResponseStart {
                request_id,
                transfer_id,
                kind,
                total_bytes,
                chunk_bytes,
            } => {
                if !inner
                    .hello
                    .capabilities
                    .contains(&AccessCapability::ChunkedResponses)
                    || transfer.is_some()
                    || !inner.pending.lock().await.contains_key(&request_id)
                {
                    break;
                }
                match IncomingTransfer::new(
                    transfer_id,
                    request_id,
                    kind,
                    total_bytes,
                    chunk_bytes,
                    limits,
                ) {
                    Ok(started) => transfer = Some(started),
                    Err(_) => break,
                }
                continue;
            }
            ServerMessage::ResponseChunk {
                transfer_id,
                offset,
                base64,
            } => {
                if transfer
                    .as_mut()
                    .is_none_or(|transfer| transfer.append(transfer_id, offset, base64).is_err())
                {
                    break;
                }
                continue;
            }
            ServerMessage::ResponseEnd {
                transfer_id,
                total_bytes,
                sha256,
            } => {
                let Some(transfer) = transfer.take() else {
                    break;
                };
                match tokio::task::spawn_blocking(move || {
                    transfer.finish(transfer_id, total_bytes, sha256)
                })
                .await
                {
                    Ok(Ok(response)) => response,
                    _ => break,
                }
            }
            _ if transfer.is_some() => break,
            response => response,
        };
        match response {
            ServerMessage::Opened {
                request_id,
                observation_id,
                initial,
                cursor,
                reset,
                feed,
            } => {
                let terminal_error = Arc::new(std::sync::Mutex::new(None));
                let changes = if feed {
                    let (sender, receiver) = mpsc::channel(FEED_CAPACITY);
                    let subscription_cancel = CancellationToken::new();
                    inner.subscriptions.lock().await.insert(
                        observation_id,
                        Subscription {
                            sender: sender.clone(),
                            cancel: subscription_cancel.clone(),
                            terminal_error: terminal_error.clone(),
                        },
                    );
                    let client = Arc::downgrade(&inner);
                    let writer = inner.writer.clone();
                    let connection = cancel.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            biased;
                            () = connection.cancelled() => return,
                            () = subscription_cancel.cancelled() => return,
                            () = sender.closed() => {},
                        }
                        if let Some(client) = client.upgrade() {
                            client.subscriptions.lock().await.remove(&observation_id);
                        }
                        write_observation_cancel(writer, connection, limits, observation_id).await;
                    });
                    Some(receiver)
                } else {
                    None
                };
                if let Some(sender) = inner.pending.lock().await.remove(&request_id) {
                    let _ = sender.send(Ok(Reply::Opened {
                        initial,
                        cursor,
                        reset,
                        changes,
                        terminal_error: feed.then_some(terminal_error),
                    }));
                }
            }
            ServerMessage::ObservationClosed {
                observation_id,
                error,
            } => {
                if let Some(subscription) = inner.subscriptions.lock().await.remove(&observation_id)
                {
                    *subscription.terminal_error.lock().unwrap() = Some(error);
                    subscription.cancel.cancel();
                }
            }
            ServerMessage::Changed {
                observation_id,
                update,
            } => {
                let sender = inner
                    .subscriptions
                    .lock()
                    .await
                    .get(&observation_id)
                    .map(|subscription| subscription.sender.clone());
                if let Some(sender) = sender
                    && sender.try_send(super::bounded_update(update)).is_err()
                {
                    if let Some(subscription) =
                        inner.subscriptions.lock().await.remove(&observation_id)
                    {
                        subscription.cancel.cancel();
                    }
                    tokio::spawn(write_observation_cancel(
                        inner.writer.clone(),
                        cancel.clone(),
                        limits,
                        observation_id,
                    ));
                }
            }
            ServerMessage::Error {
                request_id: Some(request_id),
                error,
            } => {
                if let Some(sender) = inner.pending.lock().await.remove(&request_id) {
                    let _ = sender.send(Err(error));
                }
            }
            response @ (ServerMessage::Receipt { request_id, .. }
            | ServerMessage::DaemonStopped { request_id }
            | ServerMessage::Display { request_id, .. }) => {
                if let Some(sender) = inner.pending.lock().await.remove(&request_id) {
                    let _ = sender.send(Ok(Reply::Message(Box::new(response))));
                }
            }
            _ => {}
        }
    }
    if let Some(inner) = inner.upgrade() {
        inner.cancel.cancel();
        for (_, sender) in inner.pending.lock().await.drain() {
            let _ = sender.send(Err(AccessError::unavailable("daemon connection closed")));
        }
        for (_, subscription) in inner.subscriptions.lock().await.drain() {
            subscription.cancel.cancel();
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

pub async fn serve_daemon_listener(
    listener: UnixListener,
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    limits: AccessLimits,
) -> Result<(), AccessError> {
    let shutdown = CancellationToken::new();
    loop {
        let incoming = tokio::select! { _ = shutdown.cancelled() => return Ok(()), incoming = listener.accept() => incoming };
        let (stream, _) = incoming.map_err(|error| AccessError::unavailable(error.to_string()))?;
        let access = access.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _ = serve_connection_mode(
                stream,
                access,
                epoch,
                limits,
                false,
                Some(shutdown),
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

async fn serve_connection_mode(
    mut stream: UnixStream,
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    limits: AccessLimits,
    forwarded_sessions: bool,
    daemon_shutdown: Option<CancellationToken>,
    #[cfg(test)] observation_hooks: Option<Arc<ObservationTestHooks>>,
) -> Result<(), AccessError> {
    verify_peer(&stream)?;
    let hello = crate::daemon_management::read::<_, crate::daemon_management::Request>(&mut stream)
        .await
        .map_err(AccessError::invalid)?;
    let minor = match hello {
        Some(crate::daemon_management::Request::Hello {
            major: PROTOCOL_MAJOR,
            ..
        }) => PROTOCOL_MINOR,
        Some(crate::daemon_management::Request::Hello {
            major: crate::daemon_management::CONTROL_MAJOR,
            minor: 0,
        }) if daemon_shutdown.is_some() => {
            return crate::daemon_management::serve_shutdown(
                stream,
                epoch,
                daemon_shutdown.expect("daemon management session"),
            )
            .await
            .map_err(AccessError::invalid);
        }
        Some(crate::daemon_management::Request::Hello { major, minor }) => {
            write_frame(
                &mut stream,
                &ServerMessage::Error {
                    request_id: None,
                    error: AccessError::new(
                        AccessErrorCode::VersionMismatch,
                        format!(
                            "AgentAccess client protocol {major}.{minor} is incompatible with server {PROTOCOL_MAJOR}.{PROTOCOL_MINOR}; use the same Tick Loop version",
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
    let session_id = Uuid::new_v4();
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
        .map_or_else(CancellationToken::new, CancellationToken::child_token);
    let _cleanup = ConnectionCleanup {
        writer: writer.clone(),
        observations: observations.clone(),
        access: access.clone(),
        cancel: cancel.clone(),
        session_id,
    };
    let result = loop {
        let incoming = tokio::select! { _ = cancel.cancelled() => break Ok(()), incoming = read_frame::<_, ClientMessage>(&mut reader, frame_bytes) => incoming };
        let message = match incoming {
            Ok(Some(message)) => message,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
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
                write_frame(
                    &mut *writer.lock().await,
                    &ServerMessage::DaemonStopped { request_id },
                    frame_bytes,
                )
                .await?;
                shutdown.cancel();
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
        let pending_observations = pending_observations.clone();
        let access = access.clone();
        let writer = writer.clone();
        let observations = observations.clone();
        let cancel = cancel.clone();
        #[cfg(test)]
        let observation_hooks = observation_hooks.clone();
        tokio::spawn(async move {
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
                            let feed = observation.changes.is_some();
                            let terminal_error = observation.terminal_error.clone();
                            let response = ServerMessage::Opened {
                                request_id,
                                observation_id,
                                initial: observation.initial,
                                cursor: observation.cursor,
                                reset: observation.reset,
                                feed,
                            };
                            let start = if let Some(mut changes) = observation.changes {
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
                                                update: super::bounded_update(update),
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
                    && super::views::command_origin(&command.command)
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
    let _ = access
        .show_view(DisplayUpdate::SessionClosed { session_id })
        .await;
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
    let kind = response_kind(&response);
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
    let mut file = tokio::fs::File::from_std(encoded.file);
    tokio::io::copy(&mut file, writer)
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
    let sha256 = encoded.sha256;
    let frame_bytes = limits.frame_bytes();
    let chunk_bytes = limits.chunk_bytes();
    let mut file = tokio::fs::File::from_std(encoded.file);
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
        while offset < total_bytes {
            let length = (total_bytes - offset).min(chunk_bytes as u64) as usize;
            file.read_exact(&mut bytes[..length])
                .await
                .map_err(|error| AccessError::unavailable(error.to_string()))?;
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
                sha256,
            },
            frame_bytes,
        )
        .await
    };
    tokio::select! { _ = cancel.cancelled() => Err(AccessError::unavailable("response transfer canceled")), result = transfer => result }
}

async fn write_frame<W: AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    value: &T,
    frame_bytes: usize,
) -> Result<(), AccessError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| AccessError::invalid(error.to_string()))?;
    if bytes.len() > frame_bytes {
        return Err(AccessError::new(
            AccessErrorCode::PayloadTooLarge,
            "AgentAccess frame exceeds the transport limit",
        ));
    }
    writer
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))
}

async fn read_frame<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    reader: &mut R,
    frame_bytes: usize,
) -> Result<Option<T>, AccessError> {
    let length = match reader.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(AccessError::unavailable(error.to_string())),
    };
    if length > frame_bytes {
        return Err(AccessError::new(
            AccessErrorCode::PayloadTooLarge,
            "AgentAccess frame exceeds the transport limit",
        ));
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| AccessError::invalid(error.to_string()))
}

fn verify_peer(stream: &UnixStream) -> Result<(), AccessError> {
    // A peer that disconnected before its credentials could be read is gone, not foreign: the
    // caller treats the connection as unavailable, as a lost worker.
    let credentials = stream
        .peer_cred()
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    if credentials.uid() != unsafe { libc::geteuid() } {
        return Err(AccessError::new(
            AccessErrorCode::AccessDenied,
            "AgentAccess permits only the current user",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FEED_CHANGE_BYTES, TRANSPORT_LIMITS};

    const FRAME_BYTES: usize = TRANSPORT_LIMITS.frame_bytes();
    /// Backpressure needs a request larger than any kernel socket buffer, so these peers keep the
    /// production frame limit.
    const BACKPRESSURE_FRAME_BYTES: usize = AccessLimits::DEFAULT.frame_bytes();

    struct ControlledFeed {
        sender: Mutex<Option<mpsc::Sender<ObservationUpdate>>>,
        epoch: Uuid,
        initial: Option<ObservationValue>,
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
            Ok(Observation {
                initial,
                cursor: Cursor {
                    daemon_epoch: self.epoch,
                    execution: None,
                    sequence: 0,
                },
                reset: None,
                changes,
                terminal_error: None,
            })
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
                &ClientMessage::Hello { major, minor },
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
                    workdir: None,
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
            Ok(Observation {
                initial,
                cursor: Cursor {
                    daemon_epoch: self.epoch,
                    execution: None,
                    sequence: 0,
                },
                reset: None,
                changes: None,
                terminal_error: None,
            })
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
                                crate::model::cards::ModelCatalog::defaults()
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
                        workdir: None,
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
                            workdir: None,
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
                            workdir: None,
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
                    .expect(
                        "Response EOF must stop the blocked request writer and unblock the caller",
                    )
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
            change: ObservationChange::Event(crate::runtime::Event {
                sequence,
                chat_id: Uuid::nil(),
                loop_id: Uuid::nil(),
                run_id: Uuid::nil(),
                kind: crate::runtime::EventKind::Text(text.into()),
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
                            ObservationChange::Event(crate::runtime::Event {
                                kind: crate::runtime::EventKind::Text(sent),
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
        let canceled =
            tokio::time::timeout(std::time::Duration::from_secs(2), sender.closed()).await;
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
    async fn disconnect_during_chunked_open_cancels_the_source_before_any_opened_is_published() {
        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temporary.path().join("chunk-cancel.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let epoch = Uuid::new_v4();
        let source = Arc::new(ControlledFeed {
            sender: Mutex::new(None),
            epoch,
            initial: Some(ObservationValue::Transcript(Box::new(RecordedTranscript {
                record: TranscriptRef {
                    store_root: temporary.path().to_path_buf(),
                    chat_id: Uuid::new_v4(),
                },
                title: None,
                workdir: temporary.path().to_path_buf(),
                messages: vec![RecordedMessage {
                    id: Uuid::new_v4(),
                    message: crate::model::Message {
                        role: crate::model::Role::User,
                        content: vec![crate::model::Content::Text {
                            text: "x".repeat(FRAME_BYTES),
                        }],
                    },
                    loop_id: None,
                    run_id: None,
                    timestamp: String::new(),
                }],
                total_messages: 1,
                warnings: Vec::new(),
            }))),
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
            }),
            HANDSHAKE_FRAME_BYTES,
        )
        .await
        .unwrap();
        while let Ok(Some(message)) = read_frame::<_, ClientMessage>(&mut stream, FRAME_BYTES).await
        {
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
        use crate::agent_console::AttentionGroup;
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
            state: crate::runtime::LoopState::Idle,
            run_id: None,
            title: "New Agent".into(),
            has_work,
            updated_at: None,
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
        let mut console = crate::agent_console::AgentConsole::new(
            client,
            epoch,
            temporary.path().to_path_buf(),
            None,
        );
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
}
