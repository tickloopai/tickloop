use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::{
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{Mutex, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::frame::{read_frame, verify_peer, write_frame};
use crate::agent_access::{
    AgentAccess, Observation, bounded_update, chunked::IncomingTransfer, protocol::*,
};
use tickloop_agent_protocol::{AccessLimits, FEED_CAPACITY, HANDSHAKE_FRAME_BYTES};

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
    /// Cancelled once the connection takes no more requests: a write failed, the daemon closed
    /// it, or the client was dropped.
    cancel: CancellationToken,
    /// Stops the response reader, and with it `cancel`. Only dropping the client stops it, so
    /// replies the daemon wrote before closing, such as `DaemonStopped`, are still delivered
    /// after a write to the closed connection failed.
    reader_stop: CancellationToken,
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
        self.reader_stop.cancel();
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
        let reader_stop = CancellationToken::new();
        let inner = Arc::new(ClientInner {
            writer: Arc::new(Mutex::new(writer)),
            hello,
            limits,
            pending: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            cancel: reader_stop.child_token(),
            reader_stop: reader_stop.clone(),
        });
        tokio::spawn(read_responses(
            reader,
            Arc::downgrade(&inner),
            inner.cancel.clone(),
            reader_stop,
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

pub(super) async fn negotiate(
    stream: &mut UnixStream,
) -> Result<(ServerHello, AccessLimits), AccessError> {
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
    stop: CancellationToken,
    limits: AccessLimits,
) {
    let mut transfer: Option<IncomingTransfer> = None;
    loop {
        let response = tokio::select! { _ = stop.cancelled() => break, response = read_frame::<_, ServerMessage>(&mut reader, limits.frame_bytes()) => response };
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
                    && sender.try_send(bounded_update(update)).is_err()
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
