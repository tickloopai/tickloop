use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use super::LocalAgentClient;
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AgentAccess, CommandEnvelope, CommandReceipt, Cursor,
    DisplayReceipt, DisplayUpdate, FEED_CAPACITY, Observation, ObservationChange,
    ObservationRequest, ObservationUpdate, ObservationValue, ResetReason, bounded_update,
};

pub type ReconnectFactory = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<LocalAgentClient, AccessError>> + Send>>
        + Send
        + Sync,
>;

const WORKER_RECONNECT_DEADLINE: Duration = Duration::from_secs(5);

/// The daemon's link to one worker. A closed link is rebuilt only to the same endpoint, only
/// while `is_running` reports that worker process alive, and only when it still answers for the
/// same daemon epoch; otherwise the link stays Unavailable and the daemon marks its executions Lost.
pub fn worker_link(
    client: LocalAgentClient,
    endpoint: PathBuf,
    is_running: impl Fn() -> bool + Send + Sync + 'static,
) -> RecoveringAgentClient {
    let daemon_epoch = client.daemon_epoch();
    let is_running = Arc::new(is_running);
    let reconnect: ReconnectFactory = Arc::new(move || {
        let endpoint = endpoint.clone();
        let is_running = is_running.clone();
        Box::pin(async move {
            if !is_running() {
                return Err(AccessError::unavailable(
                    "Agent worker is no longer running",
                ));
            }
            let client = tokio::time::timeout(
                WORKER_RECONNECT_DEADLINE,
                LocalAgentClient::connect(&endpoint),
            )
            .await
            .map_err(|_| AccessError::unavailable("Agent worker did not answer a reconnect"))??;
            if client.daemon_epoch() != daemon_epoch {
                return Err(AccessError::unavailable(
                    "worker belongs to another daemon epoch",
                ));
            }
            Ok(client)
        })
    });
    RecoveringAgentClient::new(client, reconnect)
}

#[derive(Clone)]
pub struct RecoveringAgentClient {
    inner: Arc<RecoveryInner>,
}
struct RecoveryInner {
    client: RwLock<LocalAgentClient>,
    reconnect: ReconnectFactory,
    reconnect_gate: Mutex<()>,
    revision: AtomicU64,
}

impl RecoveringAgentClient {
    pub fn new(client: LocalAgentClient, reconnect: ReconnectFactory) -> Self {
        Self {
            inner: Arc::new(RecoveryInner {
                client: RwLock::new(client),
                reconnect,
                reconnect_gate: Mutex::new(()),
                revision: AtomicU64::new(1),
            }),
        }
    }
    pub fn daemon_epoch(&self) -> Uuid {
        self.inner.client.read().unwrap().daemon_epoch()
    }
    pub fn session_id(&self) -> Uuid {
        self.inner.client.read().unwrap().session_id()
    }
    pub fn connection_revision(&self) -> u64 {
        self.inner.revision.load(Ordering::SeqCst)
    }
    pub fn is_connected(&self) -> bool {
        self.inner.client.read().unwrap().is_connected()
    }
    fn client(&self) -> LocalAgentClient {
        self.inner.client.read().unwrap().clone()
    }
    pub async fn reconnect_now(&self) -> Result<(), AccessError> {
        let _guard = self.inner.reconnect_gate.lock().await;
        if self.is_connected() {
            return Ok(());
        }
        let replacement = (self.inner.reconnect)().await?;
        *self.inner.client.write().unwrap() = replacement;
        self.inner.revision.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn open(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let client = self.client();
        match client.observe(query.clone()).await {
            Ok(observation) => Ok(observation),
            Err(error) if error.code == AccessErrorCode::Unavailable => {
                self.reconnect_now().await?;
                self.client().observe(query).await
            }
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl AgentAccess for RecoveringAgentClient {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let observation = self.open(query.clone()).await?;
        if !observation.has_feed() {
            return Ok(observation);
        }
        let initial = observation.initial.clone();
        let cursor = observation.cursor;
        let reset = observation.reset;
        let terminal_error = Arc::new(std::sync::Mutex::new(None));
        let (sender, changes) = mpsc::channel(FEED_CAPACITY);
        tokio::spawn(recover_feed(
            Arc::downgrade(&self.inner),
            query,
            observation,
            sender,
            terminal_error.clone(),
        ));
        Ok(Observation::new(
            initial,
            cursor,
            reset,
            Some(changes),
            Some(terminal_error),
        ))
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        if command.daemon_epoch != self.daemon_epoch() {
            return Err(AccessError::new(
                AccessErrorCode::EpochChanged,
                "command belongs to the previous daemon; old execution was not restarted",
            ));
        }
        let command_id = command.command_id;
        let epoch = command.daemon_epoch;
        match self.client().execute(command).await {
            Ok(receipt) => Ok(receipt),
            Err(error) if error.code == AccessErrorCode::OutcomeUnknown => {
                if let Ok(observation) = self
                    .open(ObservationRequest::Receipt { epoch, command_id })
                    .await
                    && let ObservationValue::Receipt(Some(receipt)) = observation.initial
                {
                    return Ok(*receipt);
                }
                Err(AccessError::new(
                    AccessErrorCode::OutcomeUnknown,
                    format!(
                        "Command {command_id} has an unknown result in daemon {epoch}; it was not replayed"
                    ),
                ))
            }
            Err(error) => Err(error),
        }
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        let session = match &update {
            DisplayUpdate::Displayed { view, .. }
            | DisplayUpdate::Scene { view, .. }
            | DisplayUpdate::Renew { view, .. }
            | DisplayUpdate::Left { view, .. } => Some(view.session_id),
            DisplayUpdate::SessionClosed { .. } => None,
        };
        if session != Some(self.session_id()) {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "UiReady belongs to an earlier connection session; register a fresh View",
            ));
        }
        self.client().show_view(update).await
    }
}

fn resumed_query(query: &ObservationRequest, cursor: Cursor) -> ObservationRequest {
    match query {
        ObservationRequest::Overview { filter, page, .. } => ObservationRequest::Overview {
            filter: filter.clone(),
            page: *page,
            after: Some(cursor),
        },
        ObservationRequest::Agent {
            execution, detail, ..
        } => ObservationRequest::Agent {
            execution: *execution,
            detail: detail.clone(),
            after: Some(cursor),
        },
        query => query.clone(),
    }
}

async fn recover_feed(
    inner: Weak<RecoveryInner>,
    query: ObservationRequest,
    mut observation: Observation,
    sender: mpsc::Sender<ObservationUpdate>,
    terminal_error: Arc<std::sync::Mutex<Option<AccessError>>>,
) {
    let mut cursor = observation.cursor;
    loop {
        let update =
            tokio::select! { _ = sender.closed() => break, update = observation.recv() => update };
        match update {
            Ok(update) => {
                cursor = update.cursor;
                if sender.send(bounded_update(update)).await.is_err() {
                    break;
                }
            }
            Err(error) if error.code == AccessErrorCode::Unavailable => {
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let access = RecoveringAgentClient { inner };
                let reopened = tokio::select! { _ = sender.closed() => break, reopened = access.open(resumed_query(&query, cursor)) => reopened };
                match reopened {
                    Ok(next) => {
                        let change = match &next.initial {
                            ObservationValue::Agent(snapshot) => ObservationChange::Reset {
                                reason: next.reset.unwrap_or(ResetReason::Lagged),
                                snapshot: snapshot.clone(),
                            },
                            _ => ObservationChange::Resync {
                                reason: next.reset.unwrap_or(ResetReason::Lagged),
                            },
                        };
                        cursor = next.cursor;
                        if sender
                            .send(bounded_update(ObservationUpdate { cursor, change }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        observation = next;
                    }
                    Err(error) if error.code == AccessErrorCode::Unavailable => {
                        tokio::select! { _ = sender.closed() => break, _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
                    }
                    Err(error) => {
                        *terminal_error.lock().unwrap() = Some(error);
                        break;
                    }
                }
            }
            Err(error) => {
                *terminal_error.lock().unwrap() = Some(error);
                break;
            }
        }
    }
}
