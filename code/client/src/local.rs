use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::{
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{Mutex, mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::frame::{read_frame, verify_peer, write_frame};
use super::incoming::IncomingTransfer;

use tickloop_code_access::{
    AccessCapability, AccessError, AccessErrorCode, AccessLimits, AgentAccess,
    CLIENT_HANDOFF_VERSION, ClientHandoffHello, ClientMessage, CommandEnvelope, CommandReceipt,
    Cursor, DisplayReceipt, DisplayUpdate, FEED_CAPACITY, HANDSHAKE_FRAME_BYTES, Observation,
    ObservationRequest, ObservationUpdate, ObservationValue, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    ResetReason, ServerHello, ServerMessage, bounded_update,
};

mod handoff;
#[cfg(test)]
mod handoff_tests;
use handoff::{HandoffState, WritePhase};

#[derive(Clone)]
pub struct LocalAgentClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    hello: RwLock<ServerHello>,
    endpoint: PathBuf,
    handoff: watch::Sender<HandoffState>,
    write_queue: Arc<WriteQueue>,
    limits: AccessLimits,
    pending: Mutex<HashMap<Uuid, PendingRequest>>,
    subscriptions: Mutex<HashMap<Uuid, Subscription>>,
    /// Cancelled once the connection takes no more requests: a write failed, the daemon closed
    /// it, or the client was dropped.
    cancel: CancellationToken,
    /// Stops the response reader, and with it `cancel`. Only dropping the client stops it, so
    /// replies the daemon wrote before closing, such as `DaemonStopped`, are still delivered
    /// after a write to the closed connection failed.
    reader_stop: CancellationToken,
}

struct PendingRequest {
    sender: oneshot::Sender<Result<Reply, AccessError>>,
    written: bool,
    attempt: Uuid,
    order: u64,
    notification: Option<watch::Sender<Option<u64>>>,
}

struct WriteQueue {
    next: AtomicU64,
    waiting: std::sync::Mutex<BTreeSet<u64>>,
    changed: watch::Sender<()>,
}

impl WriteQueue {
    fn remove(&self, order: u64) {
        self.waiting.lock().unwrap().remove(&order);
        self.changed.send_replace(());
    }

    fn is_first(&self, order: u64) -> bool {
        self.waiting.lock().unwrap().first() == Some(&order)
    }
}

struct WriteTicket {
    queue: Arc<WriteQueue>,
    order: u64,
}

impl WriteTicket {
    fn new(queue: Arc<WriteQueue>) -> Result<Self, AccessError> {
        let mut waiting = queue.waiting.lock().unwrap();
        let order = queue
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| AccessError::invalid("client request sequence exhausted"))?;
        waiting.insert(order);
        drop(waiting);
        Ok(Self { queue, order })
    }
}

impl Drop for WriteTicket {
    fn drop(&mut self) {
        self.queue.remove(self.order);
    }
}

struct PendingObservationCancel {
    writer: Weak<Mutex<OwnedWriteHalf>>,
    connection: CancellationToken,
    limits: AccessLimits,
    request_id: Uuid,
    handoff: watch::Receiver<HandoffState>,
    written: Option<watch::Receiver<Option<u64>>>,
}

async fn write_observation_cancel(
    writer: Arc<Mutex<OwnedWriteHalf>>,
    connection: CancellationToken,
    limits: AccessLimits,
    observation_id: Uuid,
    mut handoff: watch::Receiver<HandoffState>,
    generation: u64,
) {
    let result = tokio::select! {
        biased;
        () = connection.cancelled() => return,
        result = async {
            loop {
                let mut writer = writer.lock().await;
                let state = *handoff.borrow_and_update();
                if state.generation != generation { return Ok(()); }
                if state.phase == WritePhase::Serving {
                    return write_frame(&mut *writer, &ClientMessage::CancelObservation { observation_id }, limits.frame_bytes()).await;
                }
                drop(writer);
                handoff.changed().await.map_err(|_| AccessError::unavailable("handoff state closed"))?;
            }
        } => result,
    };
    if result.is_err() {
        connection.cancel();
    }
}

impl Drop for PendingObservationCancel {
    fn drop(&mut self) {
        let Some(mut written) = self.written.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let writer = self.writer.clone();
        let connection = self.connection.clone();
        let limits = self.limits;
        let observation_id = self.request_id;
        let handoff = self.handoff.clone();
        runtime.spawn(async move {
            let generation = loop {
                if let Some(generation) = *written.borrow_and_update() {
                    break generation;
                }
                tokio::select! {
                    biased;
                    () = connection.cancelled() => return,
                    result = written.changed() => if result.is_err() { return; },
                }
            };
            let Some(writer) = writer.upgrade() else {
                return;
            };
            write_observation_cancel(
                writer,
                connection,
                limits,
                observation_id,
                handoff,
                generation,
            )
            .await;
        });
    }
}

struct Subscription {
    sender: mpsc::Sender<ObservationUpdate>,
    cancel: CancellationToken,
    terminal_error: Arc<std::sync::Mutex<Option<AccessError>>>,
}

enum Reply {
    NotAccepted,
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
        let endpoint = path.as_ref().to_path_buf();
        let mut stream = UnixStream::connect(&endpoint)
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        verify_peer(&stream)?;
        let (hello, limits) = negotiate_handoff(
            &mut stream,
            Some(ClientHandoffHello {
                version: CLIENT_HANDOFF_VERSION,
                resume: None,
            }),
        )
        .await?;
        let (reader, writer) = stream.into_split();
        let reader_stop = CancellationToken::new();
        let inner = Arc::new(ClientInner {
            writer: Arc::new(Mutex::new(writer)),
            hello: RwLock::new(hello),
            endpoint,
            handoff: watch::channel(HandoffState::default()).0,
            write_queue: Arc::new(WriteQueue {
                next: AtomicU64::new(1),
                waiting: std::sync::Mutex::new(BTreeSet::new()),
                changed: watch::channel(()).0,
            }),
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

    pub fn is_handing_off(&self) -> bool {
        self.inner.handoff.borrow().phase != WritePhase::Serving
    }

    pub fn daemon_epoch(&self) -> Uuid {
        self.inner.hello.read().unwrap().daemon_epoch
    }
    pub fn protocol(&self) -> ServerHello {
        self.inner.hello.read().unwrap().clone()
    }
    pub async fn shutdown_daemon(&self) -> Result<(), AccessError> {
        let request_id = tickloop_ids::generate();
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
        self.inner.hello.read().unwrap().process_id
    }

    pub fn session_id(&self) -> Uuid {
        self.inner.hello.read().unwrap().session_id
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
        written: Option<watch::Sender<Option<u64>>>,
    ) -> Result<Reply, AccessError> {
        let ticket = WriteTicket::new(self.inner.write_queue.clone())?;
        loop {
            let reply = self
                .request_once(request_id, message.clone(), written.clone(), ticket.order)
                .await?;
            if !matches!(reply, Reply::NotAccepted) {
                return Ok(reply);
            }
        }
    }

    async fn request_once(
        &self,
        request_id: Uuid,
        message: ClientMessage,
        notification: Option<watch::Sender<Option<u64>>>,
        order: u64,
    ) -> Result<Reply, AccessError> {
        if self.inner.cancel.is_cancelled() {
            return Err(AccessError::unavailable("daemon connection is closed"));
        }
        let (sender, receiver) = oneshot::channel();
        let attempt = tickloop_ids::generate();
        self.inner.pending.lock().await.insert(
            request_id,
            PendingRequest {
                sender,
                written: false,
                attempt,
                order,
                notification,
            },
        );
        let inner = Arc::downgrade(&self.inner);
        let writer = self.inner.writer.clone();
        let connection = self.inner.cancel.clone();
        let frame_bytes = self.inner.limits.frame_bytes();
        let mut handoff = self.inner.handoff.subscribe();
        let queue = self.inner.write_queue.clone();
        let mut queue_changed = queue.changed.subscribe();
        // A canceled caller must never truncate a frame already being written.
        let send = tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = connection.cancelled() => Err(AccessError::unavailable("daemon connection is closed")),
                result = async {
                    loop {
                        queue_changed.borrow_and_update();
                        let Some(client) = inner.upgrade() else { return Ok(()); };
                        {
                            let mut requests = client.pending.lock().await;
                            if requests.get(&request_id).is_none_or(|request| request.sender.is_closed()) {
                                requests.remove(&request_id);
                                queue.remove(order);
                                return Ok(());
                            }
                        }
                        drop(client);
                        let state = *handoff.borrow_and_update();
                        if state.phase == WritePhase::Serving && queue.is_first(order) {
                            let mut writer = writer.lock().await;
                            let state = *handoff.borrow_and_update();
                            if state.phase != WritePhase::Serving || !queue.is_first(order) { continue; }
                            let Some(client) = inner.upgrade() else { return Ok(()); };
                            {
                                let mut requests = client.pending.lock().await;
                                let Some(request) = requests.get_mut(&request_id) else { return Ok(()); };
                                if request.sender.is_closed() {
                                    requests.remove(&request_id);
                                    queue.remove(order);
                                    return Ok(());
                                }
                                request.written = true;
                            }
                            drop(client);
                            write_frame(&mut *writer, &message, frame_bytes).await?;
                            if let Some(client) = inner.upgrade() {
                                let requests = client.pending.lock().await;
                                if let Some(request) = requests.get(&request_id)
                                    && request.attempt == attempt {
                                    if let Some(notification) = &request.notification {
                                        notification.send_replace(Some(state.generation));
                                    }
                                    queue.remove(order);
                                }
                            }
                            return Ok(());
                        }
                        tokio::select! {
                            result = handoff.changed() => result.map_err(|_| AccessError::unavailable("handoff state closed"))?,
                            result = queue_changed.changed() => result.map_err(|_| AccessError::unavailable("write queue closed"))?,
                        }
                    }
                } => result,
            };
            if let Err(error) = &result {
                if let Some(client) = inner.upgrade() {
                    let mut requests = client.pending.lock().await;
                    if requests
                        .get(&request_id)
                        .is_some_and(|request| request.attempt == attempt)
                    {
                        requests.remove(&request_id);
                    }
                }
                queue.remove(order);
                if error.code == AccessErrorCode::Unavailable {
                    connection.cancel();
                }
            }
            result
        });
        send.await
            .map_err(|_| AccessError::unavailable("request writer stopped"))??;
        receiver.await.map_err(|_| {
            AccessError::unavailable("daemon disconnected before returning a result")
        })?
    }
}

#[async_trait]
impl AgentAccess for LocalAgentClient {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let request_id = tickloop_ids::generate();
        let (written, waiting) = watch::channel(None);
        let mut cancel = PendingObservationCancel {
            writer: Arc::downgrade(&self.inner.writer),
            connection: self.inner.cancel.clone(),
            limits: self.inner.limits,
            request_id,
            handoff: self.inner.handoff.subscribe(),
            written: Some(waiting),
        };
        let result = self
            .request_with_notification(
                request_id,
                ClientMessage::Observe {
                    request_id,
                    query: query.clone(),
                },
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
            } => Ok(Observation::new(
                initial,
                cursor,
                reset,
                changes,
                terminal_error,
            )),
            _ => Err(AccessError::invalid(
                "daemon returned the wrong observation reply",
            )),
        }
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let request_id = tickloop_ids::generate();
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
        let request_id = tickloop_ids::generate();
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

/// The client half of the AgentAccess handshake on a freshly connected `stream`.
pub async fn negotiate(
    stream: &mut UnixStream,
) -> Result<(ServerHello, AccessLimits), AccessError> {
    negotiate_handoff(stream, None).await
}

async fn negotiate_handoff(
    stream: &mut UnixStream,
    handoff: Option<ClientHandoffHello>,
) -> Result<(ServerHello, AccessLimits), AccessError> {
    write_frame(
        stream,
        &ClientMessage::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            handoff,
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
                "AgentAccess peer protocol {}.{} is incompatible with client {}.{}; use the same Tickloop version",
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
                    "AgentAccess peer announced unusable transport limits ({error}); use the same Tickloop version"
                ),
            )
        },
    )?;
    if hello.handoff.as_ref().is_some_and(|handoff| {
        handoff.version != CLIENT_HANDOFF_VERSION || handoff.resume_token.is_empty()
    }) {
        return Err(AccessError::new(
            AccessErrorCode::VersionMismatch,
            "unsupported or invalid client handoff protocol",
        ));
    }
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
                let chunks_supported = inner
                    .hello
                    .read()
                    .unwrap()
                    .capabilities
                    .contains(&AccessCapability::ChunkedResponses);
                if !chunks_supported
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
            ServerMessage::HandoffPrepare { transfer_id } => {
                match handoff::prepare(&inner, transfer_id).await {
                    Ok(()) => {}
                    Err(_) => break,
                }
            }
            ServerMessage::HandoffAbort { transfer_id } => {
                match handoff::abort(&inner, transfer_id) {
                    Ok(()) => {}
                    Err(_) => break,
                }
            }
            ServerMessage::HandoffCommit {
                transfer_id,
                successor_process_id,
            } => match handoff::commit(&inner, transfer_id, successor_process_id).await {
                Ok(replacement) => reader = replacement,
                Err(_) => break,
            },
            ServerMessage::NotAccepted {
                request_id,
                transfer_id,
            } => {
                if !handoff::confirms_not_accepted(&inner, transfer_id) {
                    break;
                }
                if let Some(pending) = inner.pending.lock().await.remove(&request_id) {
                    if let Some(notification) = &pending.notification {
                        notification.send_replace(None);
                    }
                    {
                        let mut waiting = inner.write_queue.waiting.lock().unwrap();
                        if !pending.sender.is_closed() {
                            waiting.insert(pending.order);
                        }
                    }
                    let _ = pending.sender.send(Ok(Reply::NotAccepted));
                    inner.write_queue.changed.send_replace(());
                }
            }
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
                    let handoff = inner.handoff.subscribe();
                    let generation = handoff.borrow().generation;
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
                        write_observation_cancel(
                            writer,
                            connection,
                            limits,
                            observation_id,
                            handoff,
                            generation,
                        )
                        .await;
                    });
                    Some(receiver)
                } else {
                    None
                };
                if let Some(pending) = inner.pending.lock().await.remove(&request_id) {
                    if let Some(notification) = &pending.notification {
                        notification.send_replace(Some(inner.handoff.borrow().generation));
                    }
                    inner.write_queue.remove(pending.order);
                    let _ = pending.sender.send(Ok(Reply::Opened {
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
                        inner.handoff.subscribe(),
                        inner.handoff.borrow().generation,
                    ));
                }
            }
            ServerMessage::Error {
                request_id: Some(request_id),
                error,
            } => {
                if let Some(pending) = inner.pending.lock().await.remove(&request_id) {
                    inner.write_queue.remove(pending.order);
                    let _ = pending.sender.send(Err(error));
                }
            }
            response @ (ServerMessage::Receipt { request_id, .. }
            | ServerMessage::DaemonStopped { request_id }
            | ServerMessage::Display { request_id, .. }) => {
                if let Some(pending) = inner.pending.lock().await.remove(&request_id) {
                    inner.write_queue.remove(pending.order);
                    let _ = pending.sender.send(Ok(Reply::Message(Box::new(response))));
                }
            }
            _ => {}
        }
    }
    if let Some(inner) = inner.upgrade() {
        inner.cancel.cancel();
        for (_, pending) in inner.pending.lock().await.drain() {
            let _ = pending
                .sender
                .send(Err(AccessError::unavailable("daemon connection closed")));
        }
        for (_, subscription) in inner.subscriptions.lock().await.drain() {
            subscription.cancel.cancel();
        }
    }
}
