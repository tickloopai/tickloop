use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use uuid::Uuid;

use super::{
    receipts::ReceiptStore,
    views::{ViewRegistry, command_origin, verify_host},
};
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AgentAccess, AgentCommand, AgentDetailQuery, AgentFilter,
    AgentSummary, CommandEnvelope, CommandReceipt, CommandResult, Cursor, DisplayReceipt,
    DisplayUpdate, ExecutionAvailability, ExecutionRef, FEED_CAPACITY, LaunchSpec, MAX_PAGE_LIMIT,
    Observation, ObservationChange, ObservationRequest, ObservationUpdate, ObservationValue,
    OtherTickhost, Page, PersistenceState, REPLAY_CAPACITY, ResetReason, RuntimeVersion,
    TranscriptRef, chat_display_title, chat_has_work, chat_preview,
};

#[async_trait]
pub trait WorkerFactory: Send + Sync {
    async fn request_upgrade(&self, _executable: &std::path::Path) -> Result<(), AccessError> {
        Err(AccessError::new(
            AccessErrorCode::Unsupported,
            "this runtime does not support process upgrades",
        ))
    }
    async fn runtime_version(
        &self,
        _routes: &[(ExecutionRef, Arc<dyn AgentAccess>)],
    ) -> RuntimeVersion {
        RuntimeVersion {
            executable: std::env::current_exe().unwrap_or_default(),
            build_id: tickloop_agent_protocol::build_identity()
                .unwrap_or_default()
                .into(),
            requested_executable: None,
            upgrading: false,
            error: None,
            workers: Vec::new(),
        }
    }
    /// Ends only this managed invocation's hosted ownership; never stops the daemon.
    async fn end_managed(
        &self,
        _launch: &LaunchSpec,
        _chat: Uuid,
        _operation: Uuid,
    ) -> Result<(), AccessError> {
        Ok(())
    }
    async fn start(
        &self,
        launch: &LaunchSpec,
        daemon_epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError>;
    /// Whether the worker behind `worker` has positively exited. An unknown or still running
    /// worker is `false`, because a replacement must never run beside work that can still act.
    async fn exited(&self, worker: &Arc<dyn AgentAccess>) -> bool;
    /// Lets a worker that runs nothing anymore exit and forgets it. The daemon already dropped
    /// every route to it.
    async fn retire(&self, _worker: &Arc<dyn AgentAccess>) {}
    /// The running Tickhost when it is another build than this daemon's and replacing it would
    /// end something. A daemon without a Tickhost of its own has none.
    async fn other_tickhost(&self) -> Result<Option<OtherTickhost>, AccessError> {
        Ok(None)
    }
    /// Stops the running Tickhost of another build on the user's word.
    async fn replace_tickhost(&self) -> Result<(), AccessError> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct DaemonServer {
    inner: Arc<DaemonInner>,
}
struct Worker {
    access: Arc<dyn AgentAccess>,
    launch: LaunchSpec,
    last_directory: Vec<AgentSummary>,
}
struct DaemonInner {
    epoch: Uuid,
    factory: Arc<dyn WorkerFactory>,
    routes: Mutex<HashMap<ExecutionRef, Arc<dyn AgentAccess>>>,
    lost: Mutex<HashSet<ExecutionRef>>,
    workers: Mutex<Vec<Worker>>,
    records: Mutex<HashMap<TranscriptRef, ExecutionRef>>,
    record_gates: Mutex<HashMap<TranscriptRef, Arc<Mutex<()>>>>,
    receipts: ReceiptStore,
    managed: super::managed::ManagedRegistry,
    views: ViewRegistry,
    sequence: AtomicU64,
    changes: broadcast::Sender<ObservationUpdate>,
    admission: Arc<RwLock<()>>,
    retired: AtomicBool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct DaemonCheckpoint {
    workers: Vec<WorkerCheckpoint>,
    routes: Vec<(ExecutionRef, usize)>,
    lost: HashSet<ExecutionRef>,
    records: Vec<(TranscriptRef, ExecutionRef)>,
    receipts: super::receipts::ReceiptCheckpoint,
    #[serde(default)]
    managed: Vec<super::managed::SavedManaged>,
    views: super::views::ViewCheckpoint,
    sequence: u64,
}

#[derive(Serialize, Deserialize)]
struct WorkerCheckpoint {
    launch: LaunchSpec,
    last_directory: Vec<AgentSummary>,
}

impl DaemonServer {
    pub fn new(epoch: Uuid, factory: Arc<dyn WorkerFactory>) -> Self {
        let daemon = Self {
            inner: Arc::new(DaemonInner {
                epoch,
                factory,
                routes: Mutex::new(HashMap::new()),
                lost: Mutex::new(HashSet::new()),
                workers: Mutex::new(Vec::new()),
                records: Mutex::new(HashMap::new()),
                record_gates: Mutex::new(HashMap::new()),
                receipts: ReceiptStore::new(epoch),
                managed: Default::default(),
                views: ViewRegistry::default(),
                sequence: AtomicU64::new(0),
                changes: broadcast::channel(REPLAY_CAPACITY).0,
                admission: Arc::new(RwLock::new(())),
                retired: AtomicBool::new(false),
            }),
        };
        tokio::spawn(monitor_directory(Arc::downgrade(&daemon.inner)));
        tokio::spawn(monitor_managed(Arc::downgrade(&daemon.inner)));
        daemon
    }
    pub fn daemon_epoch(&self) -> Uuid {
        self.inner.epoch
    }
    #[cfg(test)]
    pub(crate) async fn pause(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.inner.admission.clone().write_owned().await
    }
    pub(crate) fn try_pause(&self) -> Option<tokio::sync::OwnedRwLockWriteGuard<()>> {
        self.inner.admission.clone().try_write_owned().ok()
    }
    pub(crate) fn retire(&self) {
        self.inner.retired.store(true, Ordering::SeqCst);
    }

    fn ensure_active(&self) -> Result<(), AccessError> {
        if self.inner.retired.load(Ordering::SeqCst) {
            Err(AccessError::new(
                AccessErrorCode::HandoffNotAccepted,
                "Agent daemon transferred ownership before accepting the request",
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) async fn checkpoint(
        &self,
        access: &[Arc<dyn AgentAccess>],
    ) -> Result<DaemonCheckpoint, AccessError> {
        let workers = self.inner.workers.lock().await;
        let index = |worker: &Arc<dyn AgentAccess>| {
            access
                .iter()
                .position(|entry| Arc::ptr_eq(entry, worker))
                .ok_or_else(|| AccessError::invalid("unknown worker during handoff"))
        };
        let mut saved_workers = Vec::with_capacity(access.len());
        for entry in access {
            let worker = workers
                .iter()
                .find(|worker| Arc::ptr_eq(&worker.access, entry))
                .ok_or_else(|| AccessError::invalid("missing worker launch during handoff"))?;
            saved_workers.push(WorkerCheckpoint {
                launch: worker.launch.clone(),
                last_directory: worker.last_directory.clone(),
            });
        }
        let mut routes = Vec::new();
        for (execution, worker) in self.inner.routes.lock().await.iter() {
            routes.push((*execution, index(worker)?));
        }
        Ok(DaemonCheckpoint {
            workers: saved_workers,
            routes,
            lost: self.inner.lost.lock().await.clone(),
            records: self
                .inner
                .records
                .lock()
                .await
                .iter()
                .map(|(record, execution)| (record.clone(), *execution))
                .collect(),
            receipts: self.inner.receipts.checkpoint().await?,
            managed: self.inner.managed.checkpoint().await,
            views: self.inner.views.checkpoint().await,
            sequence: self.inner.sequence.load(Ordering::SeqCst),
        })
    }

    pub(crate) async fn restore(
        &self,
        checkpoint: DaemonCheckpoint,
        access: &[Arc<dyn AgentAccess>],
    ) -> Result<(), AccessError> {
        if checkpoint.workers.len() != access.len() {
            return Err(AccessError::invalid("worker handoff count mismatch"));
        }
        let mut routes = HashMap::new();
        for (execution, index) in checkpoint.routes {
            let worker = access
                .get(index)
                .ok_or_else(|| AccessError::invalid("invalid handoff worker index"))?;
            if execution.daemon_epoch != self.inner.epoch {
                return Err(AccessError::invalid("invalid handoff execution epoch"));
            }
            routes.insert(execution, worker.clone());
        }
        *self.inner.workers.lock().await = checkpoint
            .workers
            .into_iter()
            .zip(access)
            .map(|(worker, access)| Worker {
                access: access.clone(),
                launch: worker.launch,
                last_directory: worker.last_directory,
            })
            .collect();
        *self.inner.routes.lock().await = routes;
        *self.inner.records.lock().await = checkpoint.records.into_iter().collect();
        *self.inner.lost.lock().await = checkpoint.lost;
        self.inner.receipts.restore(checkpoint.receipts).await?;
        self.inner.managed.restore(checkpoint.managed).await;
        self.inner.views.restore(checkpoint.views).await;
        self.inner
            .sequence
            .store(checkpoint.sequence, Ordering::SeqCst);
        Ok(())
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
        if execution.daemon_epoch != self.inner.epoch {
            return Err(AccessError::new(
                AccessErrorCode::EpochChanged,
                "execution belongs to another daemon epoch",
            ));
        }
        if self.inner.lost.lock().await.contains(&execution) {
            return Err(AccessError::new(
                AccessErrorCode::StaleExecution,
                "execution worker was lost; explicitly load its record to continue",
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
    /// Verifies that `chat` currently runs in `worker` and is not lost, refreshing that Worker's
    /// directory first, and returns its execution. A worker can only act for its own Chats, whatever it
    /// claims.
    pub async fn verify_worker_chat(
        &self,
        chat: Uuid,
        worker: &Arc<dyn AgentAccess>,
    ) -> Result<ExecutionRef, AccessError> {
        let agents = worker_directory(worker.as_ref()).await?;
        let lost = self.inner.lost.lock().await;
        let execution = agents
            .iter()
            .find(|entry| {
                entry.chat_id == chat
                    && entry.execution.daemon_epoch == self.inner.epoch
                    && entry.availability != ExecutionAvailability::Lost
                    && !lost.contains(&entry.execution)
            })
            .map(|entry| entry.execution)
            .ok_or_else(|| {
                AccessError::new(
                    AccessErrorCode::AccessDenied,
                    "the Chat is not running in this Worker",
                )
            })?;
        let mut workers = self.inner.workers.lock().await;
        let known = workers
            .iter_mut()
            .find(|known| Arc::ptr_eq(&known.access, worker))
            .ok_or_else(|| {
                AccessError::new(
                    AccessErrorCode::AccessDenied,
                    "the Worker is not registered in this daemon",
                )
            })?;
        let mut routes = self.inner.routes.lock().await;
        if routes
            .get(&execution)
            .is_some_and(|routed| !Arc::ptr_eq(routed, worker))
        {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "the Chat belongs to another Worker",
            ));
        }
        let mut records = self.inner.records.lock().await;
        for entry in &agents {
            if entry.execution.daemon_epoch != self.inner.epoch
                || routes
                    .get(&entry.execution)
                    .is_some_and(|routed| !Arc::ptr_eq(routed, worker))
            {
                continue;
            }
            routes.insert(entry.execution, worker.clone());
            if let Some(record) = &entry.record {
                records.insert(record.clone(), entry.execution);
            }
        }
        known.last_directory = agents;
        Ok(execution)
    }
    fn cursor(&self) -> Cursor {
        Cursor {
            daemon_epoch: self.inner.epoch,
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
                    // A Worker retired meanwhile was stopped on purpose; nothing of it is lost.
                    if !self
                        .inner
                        .workers
                        .lock()
                        .await
                        .iter()
                        .any(|known| Arc::ptr_eq(&known.access, &worker))
                    {
                        continue;
                    }
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
                let lost = self.inner.lost.lock().await.clone();
                let mut workers = self.inner.workers.lock().await;
                // A Worker retired meanwhile must not get its routes or records back.
                let Some(known) = workers
                    .iter_mut()
                    .find(|known| Arc::ptr_eq(&known.access, &worker))
                else {
                    continue;
                };
                known.last_directory = entries.clone();
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
                daemon_epoch: self.inner.epoch,
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
    /// Stops a Worker started for one managed invocation and forgets every route, record and
    /// fence naming it. `execution` is the only execution it may run; `None` accepts any, for a
    /// Worker whose creation failed. A Worker started more than once or running anything else
    /// is kept.
    async fn retire_worker(
        &self,
        worker: &Arc<dyn AgentAccess>,
        execution: Option<ExecutionRef>,
    ) -> bool {
        {
            let mut lost = self.inner.lost.lock().await;
            let mut workers = self.inner.workers.lock().await;
            let mut routes = self.inner.routes.lock().await;
            let dedicated = workers
                .iter()
                .filter(|known| Arc::ptr_eq(&known.access, worker))
                .count()
                == 1
                && routes.iter().all(|(routed, access)| {
                    !Arc::ptr_eq(access, worker) || execution.is_none_or(|only| *routed == only)
                });
            if !dedicated {
                return false;
            }
            workers.retain(|known| !Arc::ptr_eq(&known.access, worker));
            let mut forgotten = execution.into_iter().collect::<HashSet<_>>();
            routes.retain(|routed, access| {
                let retired = Arc::ptr_eq(access, worker);
                if retired {
                    forgotten.insert(*routed);
                }
                !retired
            });
            self.inner
                .records
                .lock()
                .await
                .retain(|_, routed| !forgotten.contains(routed));
            lost.retain(|routed| !forgotten.contains(routed));
        }
        self.inner.factory.retire(worker).await;
        true
    }
    /// A managed Worker runs exactly its invocation. Once that invocation's release fully
    /// completed, stop the Worker and forget the execution, so neither the process nor its
    /// proof, launch and session MCP descriptors outlive the invocation. Failed cleanup keeps
    /// both for its bounded retries.
    async fn retire_released(
        &self,
        execution: ExecutionRef,
        entry: &Arc<super::managed::ManagedEntry>,
    ) {
        if !entry.released_cleanly().await || !self.inner.managed.is_current(execution, entry).await
        {
            return;
        }
        let worker = self.inner.routes.lock().await.get(&execution).cloned();
        if let Some(worker) = worker
            && self.retire_worker(&worker, Some(execution)).await
        {
            self.inner.managed.remove(execution, entry).await;
        }
    }
    /// Ends the control and hosted ownership of an invocation whose Worker positively exited,
    /// before another Worker loads the same Chat. Its late release or expiry then finds no
    /// proof, so it can never end the Ticks of the invocation that replaced it.
    async fn supersede_exited(&self, execution: ExecutionRef) -> Result<(), AccessError> {
        if let Some(entry) = self.inner.managed.entry(execution).await {
            let _gate =
                tokio::time::timeout(Duration::from_secs(10), entry.gate.clone().lock_owned())
                    .await
                    .map_err(|_| {
                        AccessError::new(
                            AccessErrorCode::Busy,
                            "the exited ACP invocation is still settling",
                        )
                    })?;
            if self.inner.managed.is_current(execution, &entry).await {
                entry.revoke().await;
                let cleanup = self.cleanup_managed(execution, &entry).await;
                // An exited Worker ended at least everything its release would have.
                entry
                    .cleanup_finished(true, cleanup.as_ref().err().map(|e| e.message.clone()))
                    .await;
                entry.settle().await;
                cleanup?;
                self.inner.managed.remove(execution, &entry).await;
            }
        }
        let worker = self.inner.routes.lock().await.get(&execution).cloned();
        if let Some(worker) = worker {
            self.retire_worker(&worker, Some(execution)).await;
        }
        Ok(())
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
        let restored_family = if snapshot.conversation.delegation.is_some() {
            worker_directory(worker.as_ref()).await.ok()
        } else {
            None
        };
        if let Some(family) = &restored_family {
            let mut routes = self.inner.routes.lock().await;
            for member in family {
                routes.insert(member.execution, worker.clone());
            }
        }
        let root = {
            let mut workers = self.inner.workers.lock().await;
            let known = workers
                .iter_mut()
                .find(|known| Arc::ptr_eq(&known.access, &worker));
            if let Some(known) = known {
                if let Some(family) = restored_family {
                    known.last_directory = family;
                }
                let record = known
                    .launch
                    .config_root
                    .clone()
                    .filter(|_| known.launch.session_persistence)
                    .map(|store_root| TranscriptRef {
                        store_root,
                        chat_id: snapshot.conversation.id,
                    });
                let state = tickloop_agent_runtime::summary_state(
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
                let title = chat_display_title(&snapshot.conversation, &snapshot.user_shells);
                known.last_directory.push(AgentSummary {
                    availability: ExecutionAvailability::Live,
                    execution,
                    chat_id: snapshot.conversation.id,
                    folder: snapshot.conversation.folder.clone(),
                    run_id: super::summary::summary_run_id(&snapshot.conversation.loops, &state),
                    run_started_at: super::summary::run_started_at(
                        &snapshot.conversation.loops,
                        &state,
                    ),
                    state,
                    preview: chat_preview(&snapshot.conversation, &title),
                    title,
                    subagent: snapshot.subagent.clone(),
                    has_work: Some(chat_has_work(
                        &snapshot.conversation,
                        &snapshot.user_shells,
                        snapshot.pending_inputs.len() + snapshot.pending_commands.len(),
                    )),
                    updated_at: super::summary::updated_at(
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
    async fn cleanup_managed(
        &self,
        _execution: ExecutionRef,
        entry: &super::managed::ManagedEntry,
    ) -> Result<(), AccessError> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.inner
                .factory
                .end_managed(&entry.launch, entry.chat, entry.operation().await),
        )
        .await
        .map_err(|_| {
            AccessError::new(
                AccessErrorCode::OutcomeUnknown,
                "managed hosted cleanup timed out; its outcome is unknown",
            )
        })?
    }
    async fn finalize_managed_release(
        &self,
        execution: ExecutionRef,
        entry: &super::managed::ManagedEntry,
    ) -> Result<(), AccessError> {
        if entry.launch.profile != tickloop_agent_protocol::ExecutionProfile::Acp {
            return Ok(());
        }
        // This runs under upgrade admission: a wedged Worker must not hold it indefinitely.
        tokio::time::timeout(Duration::from_secs(65), async {
            let worker = self.worker(execution).await?;
            let receipt = worker
                .execute(CommandEnvelope {
                    daemon_epoch: self.inner.epoch,
                    command_id: Uuid::new_v4(),
                    command: AgentCommand::ManagedFinalizeRelease {
                        execution,
                        token: entry.token.clone(),
                    },
                })
                .await?;
            receipt.into_result().map(|_| ())
        })
        .await
        .map_err(|_| {
            AccessError::new(
                AccessErrorCode::OutcomeUnknown,
                "managed release finalization timed out; its outcome is unknown",
            )
        })?
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
            AgentCommand::CreateManaged {
                launch,
                initial,
                token,
                ..
            } => {
                super::managed::validate_launch(launch)?;
                if initial.is_some() {
                    return Err(AccessError::invalid(
                        "managed creation must not submit initial input",
                    ));
                }
                let launch = super::managed::normalized_launch(launch.clone());
                let mut command = envelope.clone();
                if let AgentCommand::CreateManaged {
                    launch: supplied, ..
                } = &mut command.command
                {
                    *supplied = launch.clone();
                }
                let worker = self.start(&launch).await?;
                let result = self.execute_worker(worker.clone(), &command).await;
                if let Ok((
                    CommandResult::Created {
                        execution,
                        snapshot,
                        ..
                    },
                    _,
                )) = &result
                {
                    self.inner
                        .managed
                        .install(*execution, snapshot.conversation.id, launch, token.clone())
                        .await;
                } else {
                    // The Worker was started for this invocation alone and has no control proof.
                    self.retire_worker(&worker, None).await;
                }
                result
            }
            AgentCommand::ManagedFinalizeRelease { .. } => Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "release finalization is Worker-internal",
            )),
            AgentCommand::ManagedLoad {
                session_uuid,
                launch,
                token,
                ..
            } => {
                super::managed::validate_launch(launch)?;
                if launch.profile != tickloop_agent_protocol::ExecutionProfile::Acp
                    || !launch.session_persistence
                {
                    return Err(AccessError::invalid("ACP load requires persistent scope"));
                }
                let mut launch = super::managed::normalized_launch(launch.clone());
                let root = launch
                    .config_root
                    .as_ref()
                    .ok_or_else(|| AccessError::invalid("ACP load requires a configuration root"))?
                    .canonicalize()
                    .map_err(|e| AccessError::invalid(e.to_string()))?;
                launch.folder = launch
                    .folder
                    .canonicalize()
                    .map_err(|e| AccessError::invalid(e.to_string()))?;
                launch.config_root = Some(root.clone());
                let record = TranscriptRef {
                    store_root: root,
                    chat_id: *session_uuid,
                };
                let gate = self
                    .inner
                    .record_gates
                    .lock()
                    .await
                    .entry(record.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                let _guard = gate.lock().await;
                let metadata = tickloop_agent_runtime::transcript::records::acp_session(&record)
                    .await?
                    .ok_or_else(|| {
                        AccessError::new(AccessErrorCode::AccessDenied, "record has no ACP origin")
                    })?;
                if !metadata
                    .origin
                    .matches_current(&launch.folder, &record.store_root)
                    .map_err(|e| AccessError::invalid(e.to_string()))?
                {
                    return Err(AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "ACP working directory identity changed",
                    ));
                }
                launch.model = Some(
                    tickloop_agent_model::cards::ModelCatalog::defaults()
                        .profiles()
                        .into_iter()
                        .find(|config| config.identity() == metadata.model)
                        .ok_or_else(|| {
                            AccessError::invalid("saved ACP model is not in current catalog")
                        })?,
                );
                let active = self.inner.records.lock().await.get(&record).copied();
                if let Some(active) = active
                    && !self.inner.managed.reacquirable(active).await
                {
                    if !self.worker_exited(active).await {
                        return Err(AccessError::new(
                            AccessErrorCode::Busy,
                            "ACP session has a live or unresolved execution",
                        ));
                    }
                    self.supersede_exited(active).await?;
                }
                // Any old invocation is revoked and fully cleaned. A fresh Worker gets only this
                // caller's descriptors; no closed provider or environment leaks.
                let worker = self.start(&launch).await?;
                let mut command = envelope.clone();
                if let AgentCommand::ManagedLoad {
                    launch: supplied, ..
                } = &mut command.command
                {
                    *supplied = launch.clone();
                }
                let result = self.execute_worker(worker.clone(), &command).await;
                if let Ok((
                    CommandResult::Loaded {
                        execution,
                        snapshot,
                    },
                    _,
                )) = &result
                {
                    self.inner
                        .managed
                        .install(*execution, snapshot.conversation.id, launch, token.clone())
                        .await;
                } else {
                    // The Worker was started for this invocation alone and has no control proof.
                    self.retire_worker(&worker, None).await;
                }
                result
            }
            AgentCommand::ManagedSettle { execution, .. }
            | AgentCommand::ManagedRelease { execution, .. } => {
                let entry = self
                    .inner
                    .managed
                    .authorize(&envelope.command)
                    .await?
                    .ok_or_else(|| AccessError::invalid("managed execution missing"))?;
                let release = matches!(&envelope.command, AgentCommand::ManagedRelease { .. });
                if release {
                    entry.revoke().await;
                }
                let result = match self.worker(*execution).await {
                    Ok(worker) => tokio::time::timeout(
                        Duration::from_secs(65),
                        self.execute_worker(worker, envelope),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(AccessError::new(
                            AccessErrorCode::OutcomeUnknown,
                            "managed Worker settlement timed out",
                        ))
                    }),
                    Err(error) => Err(error),
                };
                let mut cleanup = self.cleanup_managed(*execution, &entry).await;
                if release && result.is_ok() && cleanup.is_ok() {
                    cleanup = self.finalize_managed_release(*execution, &entry).await;
                }
                let failure = match (&result, &cleanup) {
                    (Err(error), _) => Some(error.message.clone()),
                    (_, Err(error)) => Some(error.message.clone()),
                    _ => None,
                };
                if release {
                    entry.cleanup_finished(result.is_ok(), failure).await;
                } else {
                    entry.round_finished(failure).await;
                }
                entry.settle().await;
                if release {
                    self.retire_released(*execution, &entry).await;
                }
                match (result, cleanup) {
                    (Ok(result), Ok(())) => Ok(result),
                    (Err(error), Ok(())) => Err(error),
                    (Ok(_), Err(error)) => Err(error),
                    (Err(mut error), Err(cleanup)) => {
                        // A cleanup that may still have acted makes the whole outcome unknown.
                        if cleanup.code == AccessErrorCode::OutcomeUnknown {
                            error.code = AccessErrorCode::OutcomeUnknown;
                        }
                        error.message = format!(
                            "{}; managed hosted cleanup failed: {}",
                            error.message, cleanup.message
                        );
                        Err(error)
                    }
                }
            }
            AgentCommand::ManagedSubmit { .. } => {
                let entry = self
                    .inner
                    .managed
                    .authorize(&envelope.command)
                    .await?
                    .ok_or_else(|| AccessError::invalid("managed execution missing"))?;
                entry.begin_submit().await?;
                self.execute_worker(
                    self.worker(envelope.command.execution().unwrap()).await?,
                    envelope,
                )
                .await
            }
            AgentCommand::UpgradeRuntime { executable } => {
                self.inner.factory.request_upgrade(executable).await?;
                Ok((CommandResult::Done, PersistenceState::NotApplicable))
            }
            AgentCommand::Create { launch, .. } => {
                self.execute_worker(self.start(launch).await?, envelope)
                    .await
            }
            AgentCommand::LoadRecord {
                record,
                as_new,
                launch,
            } => {
                if tickloop_agent_runtime::transcript::records::acp_session(record)
                    .await?
                    .is_some()
                {
                    return Err(AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "ACP records require scoped ManagedLoad",
                    ));
                }
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
                if let Some(execution) = active
                    && self.inner.managed.contains(execution).await
                {
                    return Err(AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "managed executions cannot be attached or loaded",
                    ));
                }
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
            AgentCommand::ReplaceTickhost => {
                self.inner.factory.replace_tickhost().await?;
                Ok((CommandResult::Done, PersistenceState::NotApplicable))
            }
            AgentCommand::FlushTranscripts => {
                let mut persistence = PersistenceState::FlushObserved;
                for worker in self.workers().await {
                    let observed = match self.execute_worker(worker.clone(), envelope).await {
                        Ok((_, observed)) => observed,
                        // A Worker retired meanwhile flushes its own records as it exits.
                        Err(_)
                            if !self
                                .inner
                                .workers
                                .lock()
                                .await
                                .iter()
                                .any(|known| Arc::ptr_eq(&known.access, &worker)) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if matches!(observed, PersistenceState::WriteFailed(_)) {
                        persistence = observed;
                    }
                }
                Ok((CommandResult::Done, persistence))
            }
            AgentCommand::TrustWorkingDirectory { folder } => {
                let worker = self
                    .inner
                    .workers
                    .lock()
                    .await
                    .iter()
                    .rev()
                    .find(|worker| &worker.launch.folder == folder)
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
impl AgentAccess for DaemonServer {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let _admission = self.inner.admission.read().await;
        self.ensure_active()?;
        match query {
            ObservationRequest::RuntimeVersion => {
                let routes = self
                    .inner
                    .routes
                    .lock()
                    .await
                    .iter()
                    .map(|(execution, worker)| (*execution, worker.clone()))
                    .collect::<Vec<_>>();
                Ok(Observation::single(
                    ObservationValue::RuntimeVersion(Box::new(
                        self.inner.factory.runtime_version(&routes).await,
                    )),
                    self.cursor(),
                    None,
                ))
            }
            ObservationRequest::Receipt { epoch, command_id } => Ok(Observation::single(
                ObservationValue::Receipt(
                    self.inner
                        .receipts
                        .lookup(epoch, command_id)
                        .await
                        .map(Box::new),
                ),
                self.cursor(),
                None,
            )),
            ObservationRequest::Agent { execution, .. } => {
                if let ObservationRequest::Agent {
                    detail:
                        AgentDetailQuery::ManagedConfiguration { token }
                        | AgentDetailQuery::ManagedHistory { token, .. },
                    ..
                } = &query
                {
                    self.inner
                        .managed
                        .authorize(&AgentCommand::ManagedHeartbeat {
                            execution,
                            token: token.clone(),
                        })
                        .await?
                        .ok_or_else(|| AccessError::invalid("managed execution missing"))?;
                }
                let mut observation = self.worker(execution).await?.observe(query).await?;
                if let Some(warning) = self.inner.managed.warning(execution).await {
                    match &mut observation.initial {
                        ObservationValue::Agent(snapshot) => snapshot.warnings.push(warning),
                        ObservationValue::Status(status) => status.warnings.push(warning),
                        ObservationValue::Warnings(warnings) => warnings.push(warning),
                        _ => {}
                    }
                }
                Ok(observation)
            }
            ObservationRequest::OtherTickhost => Ok(Observation::single(
                ObservationValue::OtherTickhost(self.inner.factory.other_tickhost().await?),
                self.cursor(),
                None,
            )),
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
                                .folder
                                .as_ref()
                                .is_none_or(|path| Some(path) == entry.folder.as_ref())
                                && filter.workspace.as_ref().is_none_or(|workspace| {
                                    Some(workspace) == entry.workspace.as_ref()
                                })
                        })
                        .skip(page.offset)
                        .take(page.bounded_limit())
                        .collect();
                let reset = after.and_then(|after| {
                    if after.daemon_epoch != cursor.daemon_epoch {
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
                let daemon = Arc::downgrade(&self.inner);
                tokio::spawn(async move {
                    loop {
                        let update = tokio::select! { _ = sender.closed() => break, update = receiver.recv() => update };
                        let update = match update {
                            Ok(update) if update.cursor.sequence > cursor.sequence => update,
                            Ok(_) => continue,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let Some(daemon) = daemon.upgrade() else {
                                    break;
                                };
                                ObservationUpdate {
                                    cursor: DaemonServer { inner: daemon }.cursor(),
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
                Ok(Observation::new(
                    ObservationValue::Overview(initial),
                    cursor,
                    reset,
                    Some(changes),
                    None,
                ))
            }
            ObservationRequest::ManagedSessions { launch, cursor } => Ok(Observation::single(
                ObservationValue::ManagedSessions(
                    tickloop_agent_runtime::transcript::records::managed_sessions(*launch, cursor)
                        .await?,
                ),
                self.cursor(),
                None,
            )),
            ObservationRequest::SavedRecords {
                store_root,
                folder,
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
                            daemon_epoch: self.inner.epoch,
                            command_id: Uuid::new_v4(),
                            command: AgentCommand::FlushTranscripts,
                        })
                        .await;
                }
                Ok(Observation::single(
                    tickloop_agent_runtime::transcript::records::saved_records(root, folder, page)
                        .await?,
                    self.cursor(),
                    None,
                ))
            }
            ObservationRequest::Transcript { record, page } => Ok(Observation::single(
                tickloop_agent_runtime::transcript::records::transcript(record, page).await?,
                self.cursor(),
                None,
            )),
        }
    }
    async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let _admission = self.inner.admission.read().await;
        self.ensure_active()?;
        if matches!(&envelope.command, AgentCommand::Create { launch, .. } if launch.profile != tickloop_agent_protocol::ExecutionProfile::Local)
            || matches!(&envelope.command, AgentCommand::LoadRecord { launch: Some(launch), .. } if launch.profile != tickloop_agent_protocol::ExecutionProfile::Local)
        {
            return Err(AccessError::invalid(
                "managed profiles require CreateManaged, never history loading",
            ));
        }
        if let AgentCommand::SendFamilyInput {
            target_execution, ..
        } = &envelope.command
            && self.inner.managed.contains(*target_execution).await
        {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "managed executions do not accept family input",
            ));
        }
        let managed = self.inner.managed.authorize(&envelope.command).await?;
        if matches!(&envelope.command, AgentCommand::ManagedHeartbeat { .. }) {
            // Renewal is idempotent and changes nothing observable. It neither waits for the
            // settlement gate nor retains a receipt: one receipt per beat would let enough live
            // leases fill receipt capacity and refuse every other command.
            if envelope.daemon_epoch != self.inner.epoch {
                return Err(AccessError::new(
                    AccessErrorCode::EpochChanged,
                    "command belongs to an earlier daemon",
                ));
            }
            managed
                .ok_or_else(|| AccessError::invalid("managed execution missing"))?
                .renew()
                .await?;
            return Ok(CommandReceipt {
                daemon_epoch: self.inner.epoch,
                command_id: envelope.command_id,
                target: envelope.command.execution(),
                state: tickloop_agent_protocol::ReceiptState::Completed,
                result: Some(CommandResult::Done),
                error: None,
                persistence: PersistenceState::NotApplicable,
            });
        }
        let _gate = if let Some(entry) = &managed {
            Some(entry.gate.clone().lock_owned().await)
        } else {
            None
        };
        if managed.is_some() {
            self.inner.managed.authorize(&envelope.command).await?;
        }
        if let Some(receipt) = self.inner.receipts.begin(&envelope).await? {
            return Ok(receipt);
        }
        // Dispatch contains the large create/load paths; keep their future off the stack of
        // every AgentAccess caller, including embedded hosts with the default thread stack.
        let (result, persistence) = match Box::pin(self.dispatch(&envelope)).await {
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
        let _admission = self.inner.admission.read().await;
        self.ensure_active()?;
        if let DisplayUpdate::Displayed { execution, .. } = &update {
            if self.inner.managed.contains(*execution).await {
                return Err(AccessError::new(
                    AccessErrorCode::AccessDenied,
                    "managed executions cannot be attached to a Console",
                ));
            }
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

// Lease revocation must not depend on querying any Worker's directory: a live but
// unresponsive Worker must not retain resources owned by an unrelated invocation.
async fn monitor_managed(daemon: Weak<DaemonInner>) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        interval.tick().await;
        let Some(inner) = daemon.upgrade() else {
            break;
        };
        let daemon = DaemonServer { inner };
        let _admission = daemon.inner.admission.read().await;
        if daemon.ensure_active().is_err() {
            continue;
        }
        for (execution, entry) in daemon.inner.managed.expired().await {
            let daemon = daemon.clone();
            let admission = daemon.inner.admission.clone().read_owned().await;
            tokio::spawn(async move {
                let _admission = admission;
                // A lost mutation reply can hold this gate indefinitely. Revoke first,
                // bound the wait, and still attempt hosted ownership cleanup on failure.
                let gate =
                    tokio::time::timeout(Duration::from_secs(10), entry.gate.clone().lock_owned())
                        .await;
                // A release that completed meanwhile, or a load that replaced an exited
                // invocation, already owns this cleanup. Ending the Chat's hosted Ticks again
                // could end those of the invocation running it now.
                if !daemon.inner.managed.is_current(execution, &entry).await {
                    return;
                }
                let worker_result = if gate.is_err() {
                    Err(AccessError::new(
                        AccessErrorCode::OutcomeUnknown,
                        "managed expiry admission timed out; prior mutation remains unresolved",
                    ))
                } else if entry.worker_settled().await {
                    Ok(())
                } else {
                    match daemon.worker(execution).await {
                        Ok(worker) => tokio::time::timeout(
                            Duration::from_secs(65),
                            daemon.execute_worker(
                                worker,
                                &CommandEnvelope {
                                    daemon_epoch: daemon.inner.epoch,
                                    command_id: Uuid::new_v4(),
                                    command: AgentCommand::ManagedRelease {
                                        execution,
                                        token: entry.token.clone(),
                                    },
                                },
                            ),
                        )
                        .await
                        .map_err(|_| {
                            AccessError::new(
                                AccessErrorCode::OutcomeUnknown,
                                "managed expiry cancellation timed out",
                            )
                        })
                        .and_then(|result| result.map(|_| ())),
                        Err(error) => Err(error),
                    }
                };
                // Hosted ownership cleanup still runs if the Worker is lost or settlement fails.
                let mut hosted = daemon.cleanup_managed(execution, &entry).await;
                if worker_result.is_ok() && hosted.is_ok() {
                    hosted = daemon.finalize_managed_release(execution, &entry).await;
                }
                let error = worker_result
                    .as_ref()
                    .err()
                    .or(hosted.as_ref().err())
                    .map(|error| error.message.clone());
                entry.cleanup_finished(worker_result.is_ok(), error).await;
                entry.settle().await;
                daemon.retire_released(execution, &entry).await;
                daemon.changed();
            });
        }
    }
}

async fn monitor_directory(daemon: Weak<DaemonInner>) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    let mut previous = Vec::new();
    loop {
        interval.tick().await;
        let Some(inner) = daemon.upgrade() else { break };
        let daemon = DaemonServer { inner };
        let _admission = daemon.inner.admission.read().await;
        if daemon.ensure_active().is_err() {
            continue;
        }
        // Do not hold upgrade admission forever on a stalled directory observation.
        if let Ok(Ok(directory)) =
            tokio::time::timeout(Duration::from_secs(1), daemon.directory()).await
            && let Ok(current) = serde_json::to_vec(&directory)
            && previous != current
        {
            previous = current;
            daemon.changed();
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
