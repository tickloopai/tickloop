use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast, mpsc};
use uuid::Uuid;

use super::{
    AgentAccess, Observation,
    protocol::*,
    receipts::ReceiptStore,
    views::{ViewRegistry, command_origin, verify_host},
};

#[async_trait]
pub trait WorkerFactory: Send + Sync {
    async fn start(
        &self,
        launch: &LaunchSpec,
        owner_epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError>;
    /// Whether the worker behind `worker` has positively exited. An unknown or still running
    /// worker is `false`, because a replacement must never run beside work that can still act.
    async fn exited(&self, worker: &Arc<dyn AgentAccess>) -> bool;
}

#[derive(Clone)]
pub struct OwnerServer {
    inner: Arc<OwnerInner>,
}
struct Worker {
    access: Arc<dyn AgentAccess>,
    launch: LaunchSpec,
    last_directory: Vec<AgentSummary>,
}
struct OwnerInner {
    epoch: Uuid,
    factory: Arc<dyn WorkerFactory>,
    routes: Mutex<HashMap<ExecutionRef, Arc<dyn AgentAccess>>>,
    lost: Mutex<HashSet<ExecutionRef>>,
    workers: Mutex<Vec<Worker>>,
    records: Mutex<HashMap<TranscriptRef, ExecutionRef>>,
    record_gates: Mutex<HashMap<TranscriptRef, Arc<Mutex<()>>>>,
    receipts: ReceiptStore,
    views: ViewRegistry,
    sequence: AtomicU64,
    changes: broadcast::Sender<ObservationUpdate>,
}

impl OwnerServer {
    pub fn new(epoch: Uuid, factory: Arc<dyn WorkerFactory>) -> Self {
        let owner = Self {
            inner: Arc::new(OwnerInner {
                epoch,
                factory,
                routes: Mutex::new(HashMap::new()),
                lost: Mutex::new(HashSet::new()),
                workers: Mutex::new(Vec::new()),
                records: Mutex::new(HashMap::new()),
                record_gates: Mutex::new(HashMap::new()),
                receipts: ReceiptStore::new(epoch),
                views: ViewRegistry::default(),
                sequence: AtomicU64::new(0),
                changes: broadcast::channel(REPLAY_CAPACITY).0,
            }),
        };
        tokio::spawn(monitor_directory(Arc::downgrade(&owner.inner)));
        owner
    }
    pub fn owner_epoch(&self) -> Uuid {
        self.inner.epoch
    }
    async fn workers(&self) -> Vec<Arc<dyn AgentAccess>> {
        self.inner
            .workers
            .lock()
            .await
            .iter()
            .map(|worker| worker.access.clone())
            .collect()
    }
    async fn worker(&self, execution: ExecutionRef) -> Result<Arc<dyn AgentAccess>, AccessError> {
        if execution.owner_epoch != self.inner.epoch {
            return Err(AccessError::new(
                AccessErrorCode::EpochChanged,
                "execution belongs to another owner epoch",
            ));
        }
        if self.inner.lost.lock().await.contains(&execution) {
            return Err(AccessError::new(
                AccessErrorCode::StaleExecution,
                "execution owner was lost; explicitly load its record to continue",
            ));
        }
        if let Some(worker) = self.inner.routes.lock().await.get(&execution).cloned() {
            return Ok(worker);
        }
        self.directory().await?;
        self.inner
            .routes
            .lock()
            .await
            .get(&execution)
            .cloned()
            .ok_or_else(|| {
                AccessError::new(
                    AccessErrorCode::StaleExecution,
                    "execution generation is no longer registered",
                )
            })
    }
    /// Whether `chat` currently runs in any worker of this owner.
    pub async fn runs_chat(&self, chat: Uuid) -> Result<bool, AccessError> {
        Ok(self.directory().await?.iter().any(|entry| {
            entry.chat_id == chat && entry.availability != ExecutionAvailability::Lost
        }))
    }
    /// Verifies that `chat` currently runs in `worker` and is not lost, refreshing the directory
    /// first, and returns its execution. A worker can only act for its own Chats, whatever it
    /// claims.
    pub async fn verify_worker_chat(
        &self,
        chat: Uuid,
        worker: &Arc<dyn AgentAccess>,
    ) -> Result<ExecutionRef, AccessError> {
        let agents = self.directory().await?;
        let execution = agents
            .iter()
            .find(|entry| {
                entry.chat_id == chat && entry.availability != ExecutionAvailability::Lost
            })
            .map(|entry| entry.execution)
            .ok_or_else(|| {
                AccessError::new(
                    AccessErrorCode::AccessDenied,
                    "the Chat is not running in this owner",
                )
            })?;
        let routed = self.inner.routes.lock().await.get(&execution).cloned();
        if !routed.is_some_and(|routed| Arc::ptr_eq(&routed, worker)) {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "the Chat belongs to another worker",
            ));
        }
        Ok(execution)
    }
    fn cursor(&self) -> Cursor {
        Cursor {
            owner_epoch: self.inner.epoch,
            execution: None,
            sequence: self.inner.sequence.load(Ordering::SeqCst),
        }
    }
    async fn directory(&self) -> Result<Vec<AgentSummary>, AccessError> {
        let workers = self
            .inner
            .workers
            .lock()
            .await
            .iter()
            .map(|worker| (worker.access.clone(), worker.last_directory.clone()))
            .collect::<Vec<_>>();
        let views = self.inner.views.displayed().await;
        let mut agents = Vec::new();
        let mut seen = HashSet::new();
        for (worker, previous) in workers {
            let entries = match worker_directory(worker.as_ref()).await {
                Ok(entries) => entries,
                Err(error)
                    if matches!(
                        error.code,
                        AccessErrorCode::Unavailable | AccessErrorCode::NotFound
                    ) =>
                {
                    let mut lost = self.inner.lost.lock().await;
                    for mut entry in previous {
                        lost.insert(entry.execution);
                        entry.availability = ExecutionAvailability::Lost;
                        entry.views.clear();
                        entry.waiting_permissions = 0;
                        entry.waiting_questions = 0;
                        if seen.insert(entry.execution) {
                            agents.push(entry);
                        }
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            {
                if let Some(known) = self
                    .inner
                    .workers
                    .lock()
                    .await
                    .iter_mut()
                    .find(|known| Arc::ptr_eq(&known.access, &worker))
                {
                    known.last_directory = entries.clone();
                }
                let lost = self.inner.lost.lock().await.clone();
                let mut routes = self.inner.routes.lock().await;
                let mut records = self.inner.records.lock().await;
                for mut entry in entries {
                    if !seen.insert(entry.execution) {
                        continue;
                    }
                    routes.insert(entry.execution, worker.clone());
                    if let Some(record) = &entry.record {
                        records.insert(record.clone(), entry.execution);
                    }
                    // Lost is permanent: a worker that answers again cannot lift the fence.
                    if lost.contains(&entry.execution) {
                        entry.availability = ExecutionAvailability::Lost;
                        entry.views.clear();
                        entry.waiting_permissions = 0;
                        entry.waiting_questions = 0;
                    } else {
                        entry.views = views
                            .iter()
                            .filter(|view| view.execution == entry.execution)
                            .cloned()
                            .collect();
                    }
                    agents.push(entry);
                }
            }
        }
        agents.sort_by_key(|entry| entry.execution);
        Ok(agents)
    }
    fn changed(&self) {
        let sequence = self.inner.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.inner.changes.send(ObservationUpdate {
            cursor: Cursor {
                owner_epoch: self.inner.epoch,
                execution: None,
                sequence,
            },
            change: ObservationChange::Changed,
        });
    }
    async fn start(&self, launch: &LaunchSpec) -> Result<Arc<dyn AgentAccess>, AccessError> {
        let worker = self.inner.factory.start(launch, self.inner.epoch).await?;
        self.inner.workers.lock().await.push(Worker {
            access: worker.clone(),
            launch: launch.clone(),
            last_directory: Vec::new(),
        });
        Ok(worker)
    }
    async fn worker_exited(&self, execution: ExecutionRef) -> bool {
        let worker = self.inner.routes.lock().await.get(&execution).cloned();
        match worker {
            Some(worker) => self.inner.factory.exited(&worker).await,
            None => false,
        }
    }
    async fn register_result(&self, result: &CommandResult, worker: Arc<dyn AgentAccess>) {
        let (execution, snapshot) = match result {
            CommandResult::Created {
                execution,
                snapshot,
                ..
            }
            | CommandResult::Loaded {
                execution,
                snapshot,
            }
            | CommandResult::Branch {
                execution,
                snapshot,
                ..
            } => (*execution, snapshot),
            _ => return,
        };
        self.inner
            .routes
            .lock()
            .await
            .insert(execution, worker.clone());
        let root = {
            let mut workers = self.inner.workers.lock().await;
            let known = workers
                .iter_mut()
                .find(|known| Arc::ptr_eq(&known.access, &worker));
            if let Some(known) = known {
                let record = known
                    .launch
                    .config_root
                    .clone()
                    .filter(|_| known.launch.session_persistence)
                    .map(|store_root| TranscriptRef {
                        store_root,
                        chat_id: snapshot.conversation.id,
                    });
                let state = crate::runtime::summary_state(
                    &snapshot.conversation.loops,
                    &snapshot.user_shells,
                );
                let parent = snapshot
                    .conversation
                    .delegation
                    .as_ref()
                    .and_then(|delegation| {
                        known
                            .last_directory
                            .iter()
                            .find(|entry| entry.chat_id == delegation.parent_chat_id)
                            .map(|entry| entry.execution)
                    });
                known
                    .last_directory
                    .retain(|entry| entry.execution != execution);
                let title = crate::runtime::chat_display_title(
                    &snapshot.conversation,
                    &snapshot.user_shells,
                );
                known.last_directory.push(AgentSummary {
                    availability: ExecutionAvailability::Live,
                    execution,
                    chat_id: snapshot.conversation.id,
                    workdir: snapshot.conversation.workdir.clone(),
                    run_id: super::summary_run_id(&snapshot.conversation.loops, &state),
                    state,
                    preview: super::preview(&snapshot.conversation, &title),
                    title,
                    subagent: snapshot.subagent.clone(),
                    has_work: Some(super::has_work(
                        &snapshot.conversation,
                        &snapshot.user_shells,
                        snapshot.pending_inputs.len() + snapshot.pending_commands.len(),
                    )),
                    updated_at: super::updated_at(
                        &snapshot.conversation.loops,
                        &snapshot.user_shells,
                    ),
                    waiting_permissions: snapshot.permissions.len(),
                    waiting_questions: snapshot.questions.len(),
                    parent,
                    record: record.clone(),
                    workspace: known
                        .launch
                        .host_work_context
                        .as_ref()
                        .and_then(|context| context.workspace.clone()),
                    capabilities: Vec::new(),
                    views: Vec::new(),
                });
                record.map(|record| record.store_root)
            } else {
                None
            }
        };
        if let Some(store_root) = root {
            self.inner.records.lock().await.insert(
                TranscriptRef {
                    store_root,
                    chat_id: snapshot.conversation.id,
                },
                execution,
            );
        }
    }
    async fn validate_origin(&self, command: &AgentCommand) -> Result<(), AccessError> {
        if let AgentCommand::SendFamilyInput {
            source_execution,
            origin,
            ..
        } = command
        {
            self.inner
                .views
                .validate_displayed_source(origin, *source_execution)
                .await?;
            verify_host(origin.host.as_ref()).await?;
            return self
                .inner
                .views
                .validate_displayed_source(origin, *source_execution)
                .await;
        }
        if let Some(origin) = command_origin(command) {
            self.inner
                .views
                .validate_source(origin, command.execution())
                .await?;
            verify_host(origin.host.as_ref()).await?;
            self.inner
                .views
                .validate_source(origin, command.execution())
                .await?;
        }
        Ok(())
    }
    async fn execute_worker(
        &self,
        worker: Arc<dyn AgentAccess>,
        envelope: &CommandEnvelope,
    ) -> Result<(CommandResult, PersistenceState), AccessError> {
        let receipt = worker.execute(envelope.clone()).await?;
        let persistence = receipt.persistence.clone();
        self.inner
            .receipts
            .set_persistence(envelope.command_id, persistence.clone())
            .await;
        if let Some(execution) = receipt.target {
            self.inner
                .routes
                .lock()
                .await
                .insert(execution, worker.clone());
            self.inner
                .receipts
                .set_target(envelope.command_id, execution)
                .await;
        }
        let result = receipt.into_result()?;
        self.register_result(&result, worker).await;
        Ok((result, persistence))
    }
    async fn dispatch(
        &self,
        envelope: &CommandEnvelope,
    ) -> Result<(CommandResult, PersistenceState), AccessError> {
        self.validate_origin(&envelope.command).await?;
        match &envelope.command {
            AgentCommand::Create { launch, .. } => {
                self.execute_worker(self.start(launch).await?, envelope)
                    .await
            }
            AgentCommand::LoadRecord {
                record,
                as_new,
                launch,
            } => {
                if launch
                    .as_ref()
                    .and_then(|launch| launch.config_root.as_ref())
                    .is_some_and(|root| root != &record.store_root)
                {
                    return Err(AccessError::invalid(
                        "record store does not match the launch configuration root",
                    ));
                }
                let gate = self
                    .inner
                    .record_gates
                    .lock()
                    .await
                    .entry(record.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                let _guard = gate.lock().await;
                let active = self.inner.records.lock().await.get(record).copied();
                if !as_new && let Some(execution) = active {
                    let observed = match self.worker(execution).await {
                        Ok(worker) => {
                            worker
                                .observe(ObservationRequest::Agent {
                                    execution,
                                    detail: AgentDetailQuery::Snapshot,
                                    after: None,
                                })
                                .await
                        }
                        Err(error) => Err(error),
                    };
                    match observed {
                        Ok(observation) => {
                            let ObservationValue::Agent(snapshot) = observation.initial else {
                                return Err(AccessError::invalid(
                                    "worker returned an invalid loaded snapshot",
                                ));
                            };
                            return Ok((
                                CommandResult::Loaded {
                                    execution,
                                    snapshot,
                                },
                                PersistenceState::NotApplicable,
                            ));
                        }
                        Err(error)
                            if launch.is_some()
                                && matches!(
                                    error.code,
                                    AccessErrorCode::Unavailable
                                        | AccessErrorCode::StaleExecution
                                        | AccessErrorCode::NotFound
                                ) =>
                        {
                            let unreachable = {
                                let mut lost = self.inner.lost.lock().await;
                                let unreachable = error.code == AccessErrorCode::Unavailable
                                    || lost.contains(&execution);
                                lost.insert(execution);
                                unreachable
                            };
                            if unreachable && !self.worker_exited(execution).await {
                                return Err(AccessError::new(
                                    AccessErrorCode::StaleExecution,
                                    "this Agent's execution was lost while its worker may still be running; its outcome is unknown, so a second execution was not started",
                                ));
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
                let launch = launch.as_ref().ok_or_else(|| {
                    AccessError::invalid("loading an inactive record requires its LaunchSpec")
                })?;
                self.execute_worker(self.start(launch).await?, envelope)
                    .await
            }
            AgentCommand::FlushTranscripts => {
                let mut persistence = PersistenceState::FlushObserved;
                for worker in self.workers().await {
                    let (_, observed) = self.execute_worker(worker, envelope).await?;
                    if matches!(observed, PersistenceState::WriteFailed(_)) {
                        persistence = observed;
                    }
                }
                Ok((CommandResult::Done, persistence))
            }
            AgentCommand::TrustWorkingDirectory { workdir } => {
                let worker = self
                    .inner
                    .workers
                    .lock()
                    .await
                    .iter()
                    .rev()
                    .find(|worker| &worker.launch.workdir == workdir)
                    .map(|worker| worker.access.clone())
                    .ok_or_else(|| {
                        AccessError::new(
                            AccessErrorCode::NotFound,
                            "trust requires the launch that requested this directory",
                        )
                    })?;
                self.execute_worker(worker, envelope).await
            }
            command => {
                self.execute_worker(
                    self.worker(command.execution().ok_or_else(|| {
                        AccessError::invalid("command requires an explicit execution")
                    })?)
                    .await?,
                    envelope,
                )
                .await
            }
        }
    }
}

#[async_trait]
impl AgentAccess for OwnerServer {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        match query {
            ObservationRequest::Receipt { epoch, command_id } => Ok(Observation {
                initial: ObservationValue::Receipt(
                    self.inner
                        .receipts
                        .lookup(epoch, command_id)
                        .await
                        .map(Box::new),
                ),
                cursor: self.cursor(),
                reset: None,
                changes: None,
                terminal_error: None,
            }),
            ObservationRequest::Agent { execution, .. } => {
                self.worker(execution).await?.observe(query).await
            }
            ObservationRequest::Overview {
                filter,
                page,
                after,
            } => {
                let mut receiver = self.inner.changes.subscribe();
                let cursor = self.cursor();
                let initial =
                    self.directory()
                        .await?
                        .into_iter()
                        .filter(|entry| {
                            filter
                                .workdir
                                .as_ref()
                                .is_none_or(|path| path == &entry.workdir)
                                && filter.workspace.as_ref().is_none_or(|workspace| {
                                    Some(workspace) == entry.workspace.as_ref()
                                })
                        })
                        .skip(page.offset)
                        .take(page.bounded_limit())
                        .collect();
                let reset = after.and_then(|after| {
                    if after.owner_epoch != cursor.owner_epoch {
                        Some(ResetReason::EpochChanged)
                    } else if after.execution.is_some() || after.sequence > cursor.sequence {
                        Some(ResetReason::ConversationChanged)
                    } else if cursor.sequence.saturating_sub(after.sequence)
                        > REPLAY_CAPACITY as u64
                    {
                        Some(ResetReason::CursorExpired)
                    } else {
                        None
                    }
                });
                let (sender, changes) = mpsc::channel(FEED_CAPACITY);
                let owner = Arc::downgrade(&self.inner);
                tokio::spawn(async move {
                    loop {
                        let update = tokio::select! { _ = sender.closed() => break, update = receiver.recv() => update };
                        let update = match update {
                            Ok(update) if update.cursor.sequence > cursor.sequence => update,
                            Ok(_) => continue,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let Some(owner) = owner.upgrade() else {
                                    break;
                                };
                                ObservationUpdate {
                                    cursor: OwnerServer { inner: owner }.cursor(),
                                    change: ObservationChange::Changed,
                                }
                            }
                            Err(_) => break,
                        };
                        if sender.send(update).await.is_err() {
                            break;
                        }
                    }
                });
                Ok(Observation {
                    initial: ObservationValue::Overview(initial),
                    cursor,
                    reset,
                    changes: Some(changes),
                    terminal_error: None,
                })
            }
            ObservationRequest::SavedRecords {
                store_root,
                workdir,
                page,
            } => {
                let root = match store_root {
                    Some(root) => root,
                    None => self
                        .inner
                        .workers
                        .lock()
                        .await
                        .iter()
                        .rev()
                        .find_map(|worker| worker.launch.config_root.clone())
                        .ok_or_else(|| {
                            AccessError::invalid("record query requires its configuration root")
                        })?,
                };
                for worker in self.workers().await {
                    let _ = worker
                        .execute(CommandEnvelope {
                            owner_epoch: self.inner.epoch,
                            command_id: Uuid::new_v4(),
                            command: AgentCommand::FlushTranscripts,
                        })
                        .await;
                }
                Ok(Observation {
                    initial: super::records::saved_records(root, workdir, page).await?,
                    cursor: self.cursor(),
                    reset: None,
                    changes: None,
                    terminal_error: None,
                })
            }
            ObservationRequest::Transcript { record, page } => Ok(Observation {
                initial: super::records::transcript(record, page).await?,
                cursor: self.cursor(),
                reset: None,
                changes: None,
                terminal_error: None,
            }),
        }
    }
    async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        if let Some(receipt) = self.inner.receipts.begin(&envelope).await? {
            return Ok(receipt);
        }
        let (result, persistence) = match self.dispatch(&envelope).await {
            Ok((result, persistence)) => (Ok(result), persistence),
            Err(error) => (
                Err(error),
                self.inner
                    .receipts
                    .lookup(self.inner.epoch, envelope.command_id)
                    .await
                    .map_or(PersistenceState::NotApplicable, |receipt| {
                        receipt.persistence
                    }),
            ),
        };
        let receipt = self
            .inner
            .receipts
            .finish(envelope.command_id, result, persistence)
            .await;
        self.changed();
        Ok(receipt)
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        if let DisplayUpdate::Displayed { execution, .. } = &update {
            self.worker(*execution).await?;
        }
        if let DisplayUpdate::Displayed { host, .. } | DisplayUpdate::Scene { host, .. } = &update {
            verify_host(host.as_ref()).await?;
        }
        let receipt = self.inner.views.update(update).await?;
        self.changed();
        Ok(receipt)
    }
}

async fn monitor_directory(owner: Weak<OwnerInner>) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    let mut previous = Vec::new();
    loop {
        interval.tick().await;
        let Some(inner) = owner.upgrade() else {
            break;
        };
        let owner = OwnerServer { inner };
        if let Ok(directory) = owner.directory().await
            && let Ok(current) = serde_json::to_vec(&directory)
            && previous != current
        {
            previous = current;
            owner.changed();
        }
    }
}

async fn worker_directory(worker: &dyn AgentAccess) -> Result<Vec<AgentSummary>, AccessError> {
    let mut entries = Vec::new();
    loop {
        let observation = worker
            .observe(ObservationRequest::Overview {
                filter: AgentFilter::default(),
                page: Page {
                    offset: entries.len(),
                    limit: MAX_PAGE_LIMIT,
                },
                after: None,
            })
            .await?;
        let ObservationValue::Overview(page) = observation.initial else {
            return Err(AccessError::unavailable(
                "worker returned an invalid directory",
            ));
        };
        let complete = page.len() < MAX_PAGE_LIMIT;
        entries.extend(page);
        if complete {
            return Ok(entries);
        }
    }
}
