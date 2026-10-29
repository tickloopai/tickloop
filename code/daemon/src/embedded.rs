use crate::access_error::agent_access_error;

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{receipts::ReceiptStore, views::ViewRegistry};
use tickloop_code_access::{
    AccessError, AccessErrorCode, AfterTurnAction, AgentAccess, AgentCommand, AgentDetailQuery,
    AgentSummary, CommandEnvelope, CommandReceipt, CommandResult, ConfigurationChange, Cursor,
    DisplayReceipt, DisplayUpdate, ExecutionAvailability, ExecutionRef, FEED_CAPACITY,
    HistoryAction, InteractionSource, LaunchSpec, LiveChatSnapshot, LiveChatStatus, LoopAction,
    Observation, ObservationChange, ObservationRequest, ObservationUpdate, ObservationValue,
    PendingRequestKind, PersistenceState, QueuedCommandFailure, REPLAY_CAPACITY, RequestRef,
    ResetReason, TaskAction, TaskOrigin, TaskRef, TranscriptRef, UserInput, bounded_update,
};
use tickloop_harness::model::cards::model_for_persistent_selection;
use tickloop_harness::protocol::{
    BranchRequest, DirectedHumanSource, Event, HerdrInteraction, HostCallContext,
    LoopTriggerNotice, ShellTaskAction, ShellTaskOrigin, SubagentRegistration, TaskNotification,
    UserShellRecord,
};

use tickloop_harness::runtime::{
    Agent, Delivery, QuestionReply, ShellTaskCenter, TurnOp, TurnRequest,
};

#[derive(Clone)]
pub struct EmbeddedAgentAccess {
    inner: Arc<Inner>,
}

type QueuedAction = (ExecutionRef, AfterTurnAction);
type CommandFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CommandResult, AccessError>> + Send + 'a>>;

struct Inner {
    agent: Agent,
    environment: Option<tickloop_code_local_environment::environment::LocalEnvironment>,
    epoch: Uuid,
    executions: Mutex<HashMap<ExecutionRef, Uuid>>,
    references: Mutex<HashMap<Uuid, ExecutionRef>>,
    launches: Mutex<HashMap<ExecutionRef, LaunchSpec>>,
    account_models: tickloop_harness::model::availability::AccountModelDiscovery,
    receipts: ReceiptStore,
    managed: super::managed::ManagedRegistry,
    answers: Mutex<HashMap<(ExecutionRef, Uuid, PendingRequestKind), AnswerState>>,
    queue: Mutex<HashMap<Uuid, QueuedAction>>,
    queue_failures: Mutex<VecDeque<QueuedCommandFailure>>,
    views: ViewRegistry,
    verified_sources: AtomicBool,
    builtin_catalog_only: AtomicBool,
    sequence: AtomicU64,
    changes: broadcast::Sender<ObservationUpdate>,
    publishing: Mutex<()>,
    cancel: CancellationToken,
    admission: Arc<RwLock<()>>,
    upgrade_paused: AtomicBool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct EmbeddedCheckpoint {
    pub(crate) runtime: tickloop_harness::runtime::upgrade::AgentUpgradeCheckpoint,
    executions: Vec<(ExecutionRef, Uuid)>,
    launches: Vec<(ExecutionRef, LaunchSpec)>,
    receipts: super::receipts::ReceiptCheckpoint,
    #[serde(default)]
    managed: Vec<super::managed::SavedManaged>,
    queue: Vec<(Uuid, QueuedAction)>,
    queue_failures: VecDeque<QueuedCommandFailure>,
    sequence: u64,
}

/// What the serving process knows about a Chat beyond the Chat's own state.
struct LiveFacts {
    subagent: Option<SubagentRegistration>,
    loop_trigger_notices: Vec<LoopTriggerNotice>,
    transcript_write_error: Option<String>,
    warnings: Vec<String>,
    queue_failures: Vec<QueuedCommandFailure>,
}

#[derive(Default)]
struct AnswerState {
    version: u64,
    claimed: Option<Uuid>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl EmbeddedAgentAccess {
    pub fn new(agent: Agent) -> Self {
        Self::with_epoch(agent, tickloop_ids::generate())
    }

    pub fn with_epoch(agent: Agent, epoch: Uuid) -> Self {
        Self::build(agent, epoch, None)
    }

    fn build(
        agent: Agent,
        epoch: Uuid,
        environment: Option<tickloop_code_local_environment::environment::LocalEnvironment>,
    ) -> Self {
        let events = agent.subscribe();
        let shells = agent.subscribe_user_shells();
        let tasks = agent.subscribe_task_notifications();
        let inner = Arc::new(Inner {
            agent,
            environment,
            epoch,
            executions: Mutex::new(HashMap::new()),
            references: Mutex::new(HashMap::new()),
            launches: Mutex::new(HashMap::new()),
            account_models: Default::default(),
            receipts: ReceiptStore::new(epoch),
            managed: Default::default(),
            answers: Mutex::new(HashMap::new()),
            queue: Mutex::new(HashMap::new()),
            queue_failures: Mutex::new(VecDeque::new()),
            views: ViewRegistry::default(),
            verified_sources: AtomicBool::new(false),
            builtin_catalog_only: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            changes: broadcast::channel(REPLAY_CAPACITY).0,
            publishing: Mutex::new(()),
            cancel: CancellationToken::new(),
            admission: Arc::new(RwLock::new(())),
            upgrade_paused: AtomicBool::new(false),
        });
        tokio::spawn(watch_runtime(
            Arc::downgrade(&inner),
            events,
            shells,
            tasks,
            inner.cancel.clone(),
        ));
        tokio::spawn(drive_queue(Arc::downgrade(&inner), inner.cancel.clone()));
        Self { inner }
    }

    pub fn for_worker(agent: Agent, epoch: Uuid) -> Self {
        Self::for_worker_with_environment(
            agent,
            epoch,
            tickloop_code_local_environment::environment::LocalEnvironment::bare(),
        )
    }

    pub fn for_worker_with_environment(
        agent: Agent,
        epoch: Uuid,
        environment: tickloop_code_local_environment::environment::LocalEnvironment,
    ) -> Self {
        let access = Self::build(agent, epoch, Some(environment));
        access.inner.verified_sources.store(true, Ordering::SeqCst);
        access
            .inner
            .builtin_catalog_only
            .store(true, Ordering::SeqCst);
        access
    }

    pub fn daemon_epoch(&self) -> Uuid {
        self.inner.epoch
    }
    pub fn runtime(&self) -> &Agent {
        &self.inner.agent
    }
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.agent.shutdown().await;
    }

    pub(crate) async fn prepare_upgrade(&self) -> Result<Option<EmbeddedCheckpoint>, AccessError> {
        let Ok(_admission) = self.inner.admission.try_write() else {
            return Ok(None);
        };
        if self.inner.upgrade_paused.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let Some(runtime) = self
            .inner
            .agent
            .prepare_upgrade()
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))?
        else {
            return Ok(None);
        };
        for (chat, root) in runtime.chat_roots() {
            if self
                .inner
                .references
                .lock()
                .await
                .contains_key(&chat.uuid())
            {
                continue;
            }
            if let Some(root) = root {
                let parent = self
                    .inner
                    .references
                    .lock()
                    .await
                    .get(&root.uuid())
                    .copied();
                if let Some(parent) = parent {
                    let launch = self.inner.launches.lock().await.get(&parent).cloned();
                    if let Some(launch) = launch {
                        self.register(chat.into(), launch).await;
                    }
                }
            }
        }
        let receipts = match self.inner.receipts.checkpoint().await {
            Ok(receipts) => receipts,
            Err(error) => {
                self.inner.agent.cancel_upgrade().await;
                return Err(error);
            }
        };
        self.inner.upgrade_paused.store(true, Ordering::SeqCst);
        Ok(Some(EmbeddedCheckpoint {
            runtime,
            executions: self
                .inner
                .executions
                .lock()
                .await
                .iter()
                .map(|(execution, chat)| (*execution, *chat))
                .collect(),
            launches: self
                .inner
                .launches
                .lock()
                .await
                .iter()
                .map(|(execution, launch)| (*execution, launch.clone()))
                .collect(),
            receipts,
            managed: self.inner.managed.checkpoint().await,
            queue: self
                .inner
                .queue
                .lock()
                .await
                .iter()
                .map(|(id, action)| (*id, action.clone()))
                .collect(),
            queue_failures: self.inner.queue_failures.lock().await.clone(),
            sequence: self.inner.sequence.load(Ordering::SeqCst),
        }))
    }

    pub(crate) async fn cancel_upgrade(&self) {
        let _admission = self.inner.admission.write().await;
        self.inner.agent.cancel_upgrade().await;
        self.inner.upgrade_paused.store(false, Ordering::SeqCst);
    }

    pub(crate) async fn restore_upgrade(
        &self,
        checkpoint: EmbeddedCheckpoint,
    ) -> Result<(), String> {
        self.inner.upgrade_paused.store(true, Ordering::SeqCst);
        self.inner
            .agent
            .restore_upgrade(checkpoint.runtime)
            .await
            .map_err(|error| error.to_string())?;
        *self.inner.references.lock().await = checkpoint
            .executions
            .iter()
            .map(|(execution, chat)| (*chat, *execution))
            .collect();
        *self.inner.executions.lock().await = checkpoint.executions.into_iter().collect();
        *self.inner.launches.lock().await = checkpoint.launches.into_iter().collect();
        self.inner.managed.restore(checkpoint.managed).await;
        self.inner
            .receipts
            .restore(checkpoint.receipts)
            .await
            .map_err(|error| error.to_string())?;
        *self.inner.queue.lock().await = checkpoint.queue.into_iter().collect();
        *self.inner.queue_failures.lock().await = checkpoint.queue_failures;
        self.inner
            .sequence
            .store(checkpoint.sequence, Ordering::SeqCst);
        Ok(())
    }

    pub(crate) async fn activate_upgrade(&self) -> Result<(), String> {
        self.inner
            .agent
            .activate_upgrade()
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn release_upgrade(&self) -> Result<(), String> {
        self.inner
            .agent
            .release_upgrade()
            .await
            .map_err(|error| error.to_string())?;
        self.inner.upgrade_paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub(crate) async fn shutdown_for_upgrade(&self) {
        self.inner.cancel.cancel();
        self.inner.agent.shutdown_for_upgrade().await;
    }

    async fn register(&self, chat_id: Uuid, launch: LaunchSpec) -> ExecutionRef {
        let mut references = self.inner.references.lock().await;
        if let Some(execution) = references.get(&chat_id) {
            return *execution;
        }
        let execution = ExecutionRef {
            daemon_epoch: self.inner.epoch,
            agent_id: tickloop_ids::generate(),
            generation: 1,
        };
        references.insert(chat_id, execution);
        self.inner
            .executions
            .lock()
            .await
            .insert(execution, chat_id);
        self.inner.launches.lock().await.insert(execution, launch);
        execution
    }

    fn hosted_ticks(
        &self,
    ) -> Result<Arc<tickloop_code_local_environment::hosted::tools::HostedTickTools>, AccessError>
    {
        self.inner
            .environment
            .as_ref()
            .and_then(|environment| environment.hosted_ticks())
            .ok_or_else(|| {
                AccessError::new(
                    AccessErrorCode::Unsupported,
                    "this Agent runs no hosted Ticks",
                )
            })
    }

    async fn chat(&self, execution: ExecutionRef) -> Result<Uuid, AccessError> {
        if execution.daemon_epoch != self.inner.epoch {
            return Err(AccessError::new(
                AccessErrorCode::EpochChanged,
                "execution belongs to an earlier daemon",
            ));
        }
        let executions = self.inner.executions.lock().await;
        executions.get(&execution).copied().ok_or_else(|| {
            let code = if executions
                .keys()
                .any(|candidate| candidate.agent_id == execution.agent_id)
            {
                AccessErrorCode::StaleExecution
            } else {
                AccessErrorCode::NotFound
            };
            AccessError::new(code, "execution generation is no longer available")
        })
    }

    async fn snapshot(&self, execution: ExecutionRef) -> Result<LiveChatSnapshot, AccessError> {
        let chat = self.chat(execution).await?;
        let snapshot = self
            .inner
            .agent
            .snapshot(chat.into())
            .await
            .map_err(agent_access_error)?;
        let mut snapshot = LiveChatSnapshot::from_snapshot(execution, snapshot, Vec::new());
        let live = self.live_facts(execution, chat).await?;
        snapshot.subagent = live.subagent;
        snapshot.transcript_write_error = live.transcript_write_error;
        snapshot.loop_trigger_notices = live.loop_trigger_notices;
        snapshot.warnings = live.warnings;
        snapshot.queue_failures = live.queue_failures;
        self.version_requests(execution, &mut snapshot.request_refs)
            .await;
        Ok(snapshot)
    }

    /// What a status query answers; the same facts a snapshot of this moment carries.
    async fn status(&self, execution: ExecutionRef) -> Result<LiveChatStatus, AccessError> {
        let chat = self.chat(execution).await?;
        let status = self
            .inner
            .agent
            .status(chat.into())
            .await
            .map_err(agent_access_error)?;
        let live = self.live_facts(execution, chat).await?;
        let mut request_refs =
            RequestRef::pending(execution, &status.permissions, &status.questions);
        self.version_requests(execution, &mut request_refs).await;
        Ok(LiveChatStatus {
            execution,
            subagent: live.subagent,
            chat: status,
            request_refs,
            warnings: live.warnings,
            transcript_write_error: live.transcript_write_error,
            loop_trigger_notices: live.loop_trigger_notices,
        })
    }

    /// What this process adds to a Chat's own state for its Views.
    async fn live_facts(
        &self,
        execution: ExecutionRef,
        chat: Uuid,
    ) -> Result<LiveFacts, AccessError> {
        let transcript_write_error = self.inner.agent.transcript_write_error();
        let queue_failures = self
            .inner
            .queue_failures
            .lock()
            .await
            .iter()
            .filter(|failure| failure.execution == execution)
            .cloned()
            .collect::<Vec<_>>();
        let mut warnings = self.inner.agent.user_settings_warnings(chat.into());
        warnings.extend(self.inner.agent.transcript_load_warnings(chat.into()));
        warnings.extend(transcript_write_error.clone());
        warnings.extend(
            queue_failures.iter().map(|failure| {
                format!("Queued command {} failed: {}", failure.label, failure.error)
            }),
        );
        Ok(LiveFacts {
            subagent: self
                .inner
                .agent
                .subagent_registration(chat.into())
                .await
                .map_err(agent_access_error)?,
            loop_trigger_notices: self
                .inner
                .agent
                .loop_trigger_notices()
                .into_iter()
                .filter(|notice| notice.chat_id == chat)
                .collect(),
            transcript_write_error,
            warnings,
            queue_failures,
        })
    }

    /// Gives each pending request the version its answers reached, and forgets the answers of
    /// requests no longer pending.
    async fn version_requests(&self, execution: ExecutionRef, requests: &mut [RequestRef]) {
        let mut answers = self.inner.answers.lock().await;
        let pending = requests
            .iter()
            .map(|request| (request.execution, request.request_id, request.kind))
            .collect::<std::collections::HashSet<_>>();
        answers.retain(|key, _| key.0 != execution || pending.contains(key));
        for request in requests {
            request.version = answers
                .get(&(request.execution, request.request_id, request.kind))
                .map_or(1, |state| state.version);
        }
    }

    async fn host_context(
        &self,
        execution: ExecutionRef,
        chat: Uuid,
        origin: Option<InteractionSource>,
        validate_view: bool,
    ) -> Result<Option<HostCallContext>, AccessError> {
        let launch = self.inner.launches.lock().await.get(&execution).cloned();
        let context = launch.and_then(|launch| launch.host_work_context);
        let snapshot = self
            .inner
            .agent
            .snapshot(chat.into())
            .await
            .map_err(agent_access_error)?;
        // A Chat without a Folder has no host Work Context.
        let Some(folder) = snapshot.folder.clone() else {
            return Ok(None);
        };
        let mut result = if let Some(delegation) = &snapshot.delegation {
            delegation
                .ceiling
                .host_context
                .clone()
                .map(|mut inherited| {
                    inherited.folder = folder.clone();
                    inherited.interaction = None;
                    inherited
                })
        } else {
            context.map(|context| HostCallContext {
                folder: folder.clone(),
                workspace_id: context.workspace,
                interaction: None,
            })
        };
        if let Some(origin) = origin {
            if validate_view && !self.inner.verified_sources.load(Ordering::SeqCst) {
                self.inner
                    .views
                    .validate_source(&origin, Some(execution))
                    .await?;
            }
            if let Some(host) = origin.host {
                let pane = tickloop_harness::protocol::HerdrPane {
                    pane_id: host
                        .pane
                        .ok_or_else(|| AccessError::invalid("interaction source has no Pane"))?,
                    terminal_id: host.terminal.ok_or_else(|| {
                        AccessError::invalid("interaction source has no terminal")
                    })?,
                    workspace_id: host.workspace.ok_or_else(|| {
                        AccessError::invalid("interaction source has no Workspace")
                    })?,
                    tab_id: host
                        .tab
                        .ok_or_else(|| AccessError::invalid("interaction source has no Tab"))?,
                    cwd: None,
                    foreground_cwd: None,
                };
                result = Some(HostCallContext {
                    folder,
                    workspace_id: result.and_then(|context| context.workspace_id),
                    interaction: Some(HerdrInteraction {
                        socket: host.socket,
                        pane,
                    }),
                });
            }
        }
        Ok(result)
    }

    async fn submit(
        &self,
        execution: ExecutionRef,
        input: UserInput,
        origin: Option<InteractionSource>,
    ) -> Result<CommandResult, AccessError> {
        let chat = self.chat(execution).await?;
        let context = self.host_context(execution, chat, origin, true).await?;
        self.submit_in_context(execution, input, context).await
    }

    async fn submit_in_context(
        &self,
        execution: ExecutionRef,
        input: UserInput,
        context: Option<HostCallContext>,
    ) -> Result<CommandResult, AccessError> {
        let chat = self.chat(execution).await?;
        match input {
            UserInput::Text {
                text,
                contexts,
                paths,
            } => Ok(CommandResult::Submitted(
                self.inner
                    .agent
                    .submit_with_input_contexts_in_host_context(
                        chat.into(),
                        text,
                        contexts,
                        paths,
                        context,
                    )
                    .await
                    .map_err(agent_access_error)?,
            )),
            UserInput::Skill { name, arguments } => Ok(CommandResult::Submitted(
                self.inner
                    .agent
                    .invoke_skill_in_host_context(chat.into(), &name, &arguments, context)
                    .await
                    .map_err(agent_access_error)?,
            )),
            UserInput::Shell {
                command,
                background,
            } => Ok(CommandResult::ShellSubmitted(
                self.inner
                    .agent
                    .submit_user_shell(chat.into(), command, background)
                    .await
                    .map_err(agent_access_error)?,
            )),
        }
    }

    // Keep managed lifecycle futures out of ordinary Console dispatch's stack frames.
    async fn dispatch_managed(
        &self,
        envelope: &CommandEnvelope,
        command: AgentCommand,
    ) -> Result<CommandResult, AccessError> {
        match command {
            AgentCommand::CreateManaged {
                launch,
                initial,
                token,
                ..
            } => {
                Box::pin(async move {
                    super::managed::validate_launch(&launch)?;
                    if initial.is_some() {
                        return Err(AccessError::invalid(
                            "managed creation must not submit initial input",
                        ));
                    }
                    let launch = super::managed::normalized_launch(launch);
                    self.inner
                        .agent
                        .validate_tool_policy(
                            crate::execution_tool_policy(launch.profile),
                            launch.host_options,
                        )
                        .map_err(agent_access_error)?;
                    // Do not publish an execution reference until ACP identity is durable. A failed
                    // model/origin/commit check must not leave a generic, unguarded execution behind.
                    if launch.profile == tickloop_code_access::ExecutionProfile::Acp {
                        let root = if launch.session_persistence && self.inner.agent.saves_chats() {
                            Some(launch.config_root.as_deref().ok_or_else(|| {
                                AccessError::invalid("persistent ACP requires a configuration root")
                            })?)
                        } else {
                            None
                        };
                        let chat = self
                            .inner
                            .agent
                            .create_chat(launch.folder.clone())
                            .await
                            .map_err(agent_access_error)?;
                        let prepared: Result<(), AccessError> = async {
                            self.inner
                                .agent
                                .set_host_work_context(
                                    chat,
                                    HostCallContext {
                                        folder: launch.folder.clone(),
                                        workspace_id: launch
                                            .host_work_context
                                            .as_ref()
                                            .and_then(|context| context.workspace.clone()),
                                        interaction: None,
                                    },
                                )
                                .await
                                .map_err(agent_access_error)?;
                            if let Some(model) = launch.model.clone() {
                                let model = self.builtin_config(model)?;
                                self.inner
                                    .agent
                                    .configure_managed_session(chat, Some(model), None, None)
                                    .await
                                    .map_err(agent_access_error)?;
                            }
                            if let Some(root) = root {
                                self.inner
                                    .agent
                                    .record_new_acp_session(chat, &launch.folder, root)
                                    .await
                                    .map_err(agent_access_error)?;
                            }
                            Ok(())
                        }
                        .await;
                        if let Err(mut error) = prepared {
                            if let Err(cleanup) = self
                                .inner
                                .agent
                                .discard_unpublished_managed_chat(chat)
                                .await
                            {
                                error.message = format!(
                                    "{}; provisional Chat cleanup failed: {cleanup}",
                                    error.message
                                );
                            }
                            return Err(error);
                        }
                        let execution = self.register(chat.into(), launch.clone()).await;
                        self.inner
                            .managed
                            .install(execution, chat.into(), launch, token)
                            .await;
                        self.inner
                            .receipts
                            .set_target(envelope.command_id, execution)
                            .await;
                        return Ok(CommandResult::Created {
                            execution,
                            snapshot: Box::new(self.snapshot(execution).await?),
                            submitted: None,
                            shell: None,
                        });
                    }
                    let result = Box::pin(self.dispatch(&CommandEnvelope {
                        command: AgentCommand::Create {
                            launch_rules: Vec::new(),
                            launch: launch.clone(),
                            initial: None,
                            origin: None,
                        },
                        ..envelope.clone()
                    }))
                    .await?;
                    if let CommandResult::Created {
                        execution,
                        snapshot,
                        ..
                    } = &result
                    {
                        self.inner
                            .managed
                            .install(*execution, snapshot.conversation.id.into(), launch, token)
                            .await;
                    }
                    Ok(result)
                })
                .await
            }
            AgentCommand::ManagedLoad {
                session_uuid,
                launch,
                token,
                ..
            } => {
                Box::pin(async move {
                    // Restore admission is large; do not add its stack frame to unrelated Console
                    // commands that reenter submission through this shared dispatcher.
                    if launch.profile != tickloop_code_access::ExecutionProfile::Acp
                        || !launch.session_persistence
                        || launch.exact_chat.is_some()
                    {
                        return Err(AccessError::invalid("ACP restore requires persistence"));
                    }
                    let launch = super::managed::normalized_launch(launch);
                    let root = launch.config_root.as_deref().ok_or_else(|| {
                        AccessError::invalid("ACP restore requires a configuration root")
                    })?;
                    let old_reference = self
                        .inner
                        .references
                        .lock()
                        .await
                        .get(&session_uuid)
                        .copied();
                    if let Some(old) = old_reference {
                        if !launch.session_mcp_servers.is_empty() {
                            return Err(AccessError::invalid(
                                "restoring session MCP descriptors requires a fresh Worker",
                            ));
                        }
                        if !self.inner.managed.reacquirable(old).await {
                            return Err(AccessError::new(
                                AccessErrorCode::Busy,
                                "ACP session is already active or cleanup is incomplete",
                            ));
                        }
                    }
                    let snapshot = self
                        .inner
                        .agent
                        .resume_acp_session(session_uuid.into(), &launch.folder, root)
                        .await
                        .map_err(agent_access_error)?;
                    // Keep the revoked prior reference intact until all restore validation succeeds.
                    if let Some(old) = old_reference {
                        self.inner.references.lock().await.remove(&session_uuid);
                        self.inner.executions.lock().await.remove(&old);
                        self.inner.launches.lock().await.remove(&old);
                        self.inner
                            .answers
                            .lock()
                            .await
                            .retain(|(execution, _, _), _| *execution != old);
                    }
                    let execution = self.register(snapshot.id.into(), launch.clone()).await;
                    self.inner
                        .managed
                        .install(execution, snapshot.id.into(), launch, token)
                        .await;
                    Ok(CommandResult::Loaded {
                        execution,
                        snapshot: Box::new(self.snapshot(execution).await?),
                    })
                })
                .await
            }
            AgentCommand::ManagedConfigure {
                execution, change, ..
            } => {
                Box::pin(async move {
                    let entry = self
                        .inner
                        .managed
                        .authorize_worker(&envelope.command)
                        .await?
                        .ok_or_else(|| AccessError::invalid("managed execution missing"))?;
                    let chat = self.chat(execution).await?;
                    let config = self.managed_configuration(execution, &entry).await?;
                    if let Some(mode) = change.mode
                        && !config.modes.contains(&mode)
                    {
                        return Err(AccessError::invalid(
                            "mode is outside the session startup ceiling",
                        ));
                    }
                    let model = if let Some(id) = change.model_id {
                        let current = self
                            .inner
                            .agent
                            .snapshot(chat.into())
                            .await
                            .map_err(agent_access_error)?
                            .model;
                        Some(
                            self.managed_models(&current, entry.launch.config_root.clone())
                                .await
                                .into_iter()
                                .find_map(|(offered, config)| (offered == id).then_some(config))
                                .ok_or_else(|| {
                                    AccessError::invalid(
                                        "model is not in the session model directory",
                                    )
                                })?,
                        )
                    } else {
                        None
                    };
                    self.inner
                        .agent
                        .configure_managed_session(
                            chat.into(),
                            model,
                            change.mode,
                            change.reasoning_effort,
                        )
                        .await
                        .map_err(agent_access_error)?;
                    Ok(CommandResult::ManagedConfigured(
                        self.managed_configuration(execution, &entry).await?,
                    ))
                })
                .await
            }
            AgentCommand::ManagedSubmit {
                execution, input, ..
            } => {
                if !matches!(&input, UserInput::Text { .. } | UserInput::Skill { .. }) {
                    return Err(AccessError::invalid(
                        "managed execution does not accept user Shell input",
                    ));
                }
                let entry = self
                    .inner
                    .managed
                    .authorize_worker(&envelope.command)
                    .await?
                    .ok_or_else(|| AccessError::invalid("managed execution missing"))?;
                entry.begin_submit().await?;
                self.submit(execution, input, None).await
            }
            AgentCommand::ManagedHeartbeat { .. } => Ok(CommandResult::Done),
            AgentCommand::ManagedSettle {
                execution, cancel, ..
            } => {
                let chat = self.chat(execution).await?;
                self.inner
                    .agent
                    .settle_managed(chat.into(), cancel)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::ManagedSettled {
                    snapshot: Box::new(self.snapshot(execution).await?),
                })
            }
            AgentCommand::ManagedRelease { execution, .. } => {
                let chat = self.chat(execution).await?;
                let entry = self
                    .inner
                    .managed
                    .authorize_worker(&envelope.command)
                    .await?
                    .ok_or_else(|| AccessError::invalid("managed execution missing"))?;
                entry.revoke().await;
                let result = self.inner.agent.settle_managed(chat.into(), true).await;
                let closed = self.inner.agent.close_session_mcp(chat.into()).await;
                let failure = result
                    .as_ref()
                    .err()
                    .or(closed.as_ref().err())
                    .map(ToString::to_string);
                entry
                    .cleanup_finished(result.is_ok() && closed.is_ok(), failure)
                    .await;
                entry.settle().await;
                result.map_err(agent_access_error)?;
                closed.map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }
            AgentCommand::ManagedFinalizeRelease { execution, .. } => {
                let chat = self.chat(execution).await?;
                if !self.inner.managed.reacquirable(execution).await {
                    return Err(AccessError::new(
                        AccessErrorCode::Busy,
                        "ACP release cleanup has not completed",
                    ));
                }
                self.inner
                    .agent
                    .flush_acp_session(chat.into())
                    .await
                    .map_err(agent_access_error)?;
                self.inner
                    .agent
                    .release_acp_session_claim(chat.into())
                    .await;
                Ok(CommandResult::Done)
            }
            _ => unreachable!("managed dispatch requires a managed lifecycle command"),
        }
    }

    /// Each command is its own boxed future: in unoptimized builds, one future matching every
    /// command polls with a stack frame as large as all the commands' awaits together.
    fn dispatch<'a>(&'a self, envelope: &'a CommandEnvelope) -> CommandFuture<'a> {
        match envelope.command.clone() {
            command @ (AgentCommand::CreateManaged { .. }
            | AgentCommand::ManagedLoad { .. }
            | AgentCommand::ManagedConfigure { .. }
            | AgentCommand::ManagedSubmit { .. }
            | AgentCommand::ManagedHeartbeat { .. }
            | AgentCommand::ManagedSettle { .. }
            | AgentCommand::ManagedRelease { .. }
            | AgentCommand::ManagedFinalizeRelease { .. }) => {
                Box::pin(self.dispatch_managed(envelope, command))
            }
            AgentCommand::AddArtifactTick { execution, tick } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::ArtifactTicks(
                    self.inner
                        .agent
                        .add_artifact_tick(chat.into(), tick)
                        .await
                        .map_err(agent_access_error)?,
                ))
            }),
            AgentCommand::RemoveArtifactTick {
                execution,
                instance_id,
            } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::ArtifactTicks(
                    self.inner
                        .agent
                        .remove_artifact_tick(chat.into(), &instance_id)
                        .await
                        .map_err(agent_access_error)?,
                ))
            }),
            AgentCommand::LinkArtifact {
                execution,
                tick,
                history,
            } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                self.inner
                    .agent
                    .link_artifact(chat.into(), &tick, history)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }),
            AgentCommand::ListArtifactTicks { execution } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::ArtifactTicks(
                    self.inner
                        .agent
                        .artifact_ticks(chat.into())
                        .await
                        .map_err(agent_access_error)?,
                ))
            }),
            AgentCommand::ListHostedTicks { execution } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                let members = self
                    .hosted_ticks()?
                    .members(chat.into())
                    .await
                    .map_err(|error| AccessError::unavailable(error.to_string()))?;
                Ok(CommandResult::HostedTicks(members))
            }),
            AgentCommand::CloseHostedTick {
                execution,
                instance,
            } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                self.hosted_ticks()?
                    .close_for_user(chat.into(), &instance)
                    .await
                    .map_err(hosted_access_error)?;
                Ok(CommandResult::Done)
            }),
            AgentCommand::ReopenHostedTick {
                execution,
                instance,
                restore_subscriptions,
            } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                let reopened = self
                    .hosted_ticks()?
                    .reopen_for_user(chat.into(), &instance)
                    .await
                    .map_err(hosted_access_error)?;
                let membership = self
                    .inner
                    .agent
                    .record_reopened_tick(chat.into(), &instance, reopened.clone())
                    .await
                    .map_err(agent_access_error)?;
                let restore_error = self
                    .inner
                    .agent
                    .replace_tick_subscriptions(
                        chat.into(),
                        &instance,
                        &reopened,
                        restore_subscriptions,
                    )
                    .await
                    .err()
                    .map(|error| error.to_string());
                Ok(CommandResult::HostedTickReopened {
                    instance: reopened,
                    membership,
                    restore_error,
                })
            }),
            AgentCommand::AttachHostedTick {
                execution,
                instance,
            } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                let grant = if instance.access() == tick_protocol::SHELL_ACCESS {
                    self.inner
                        .environment
                        .as_ref()
                        .and_then(|environment| environment.shell_tasks())
                        .ok_or_else(|| {
                            AccessError::new(
                                AccessErrorCode::Unsupported,
                                "this Agent runs no Shells",
                            )
                        })?
                        .allow_view(chat, &instance)
                        .await
                } else {
                    self.hosted_ticks()?
                        .allow_view(chat.into(), &instance)
                        .await
                };
                Ok(CommandResult::HostedTickView(
                    grant.map_err(hosted_access_error)?,
                ))
            }),
            AgentCommand::RemoveMonitorMember {
                execution,
                instance,
            } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::MonitorMembership(
                    self.inner
                        .agent
                        .remove_monitor_member(chat.into(), instance)
                        .await
                        .map_err(agent_access_error)?,
                ))
            }),
            AgentCommand::Create {
                launch_rules,
                launch,
                initial,
                origin,
            } => Box::pin(async move {
                if let Some(UserInput::Skill { name, arguments }) = &initial {
                    self.inner
                        .agent
                        .check_user_skill(&launch.folder, name, arguments)
                        .map_err(agent_access_error)?;
                }
                let chat = if let Some(chat) = launch.exact_chat {
                    if !launch_rules.is_empty() {
                        return Err(AccessError::invalid(
                            "launch tool rules require a new Agent",
                        ));
                    }
                    self.inner
                        .agent
                        .restore_empty_chat(chat.into(), launch.folder.clone())
                        .await
                        .map_err(agent_access_error)?;
                    chat
                } else {
                    self.inner
                        .agent
                        .create_chat_with_launch_rules(launch.folder.clone(), launch_rules)
                        .await
                        .map_err(agent_access_error)?
                        .uuid()
                };
                if !launch.additional_directories.is_empty() {
                    self.inner
                        .agent
                        .add_launch_directories(chat.into(), launch.additional_directories.clone())
                        .await
                        .map_err(agent_access_error)?;
                }
                let execution = self.register(chat, launch.clone()).await;
                self.inner
                    .receipts
                    .set_target(envelope.command_id, execution)
                    .await;
                self.inner
                    .agent
                    .set_host_work_context(
                        chat.into(),
                        HostCallContext {
                            folder: launch.folder,
                            workspace_id: launch
                                .host_work_context
                                .and_then(|context| context.workspace),
                            interaction: None,
                        },
                    )
                    .await
                    .map_err(agent_access_error)?;
                // Every Agent starts on its launch model, as the worker started for that launch
                // does; one in-process runtime otherwise gives each Agent the runtime's own model.
                if let Some(model) = launch
                    .model
                    .and_then(|model| self.builtin_config(model).ok())
                    && model_for_persistent_selection(model.clone())
                        != self
                            .inner
                            .agent
                            .snapshot(chat.into())
                            .await
                            .map_err(agent_access_error)?
                            .model
                {
                    let change = ConfigurationChange::Model {
                        config: model,
                        thinking: None,
                    };
                    self.configure(execution, change)
                        .await
                        .map_err(|mut error| {
                            error.message = format!(
                                "Agent {} was created, but its launch model was not applied: {}",
                                execution.agent_id, error.message
                            );
                            error
                        })?;
                }
                let initial = match initial {
                    Some(input) => Some(self.submit(execution, input, origin).await.map_err(
                        |mut error| {
                            error.message = format!(
                                "Agent {} was created, but its first input was not submitted: {}",
                                execution.agent_id, error.message
                            );
                            error
                        },
                    )?),
                    None => None,
                };
                let (submitted, shell) = match initial {
                    Some(CommandResult::Submitted(receipt)) => (Some(receipt), None),
                    Some(CommandResult::ShellSubmitted(receipt)) => (None, Some(receipt)),
                    _ => (None, None),
                };
                Ok(CommandResult::Created {
                    execution,
                    snapshot: Box::new(self.snapshot(execution).await?),
                    submitted,
                    shell,
                })
            }),
            AgentCommand::LoadRecord {
                record,
                as_new,
                launch,
            } => Box::pin(async move {
                if crate::records::acp_session(&record).await?.is_some() {
                    return Err(AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "ACP records require scoped ManagedLoad",
                    ));
                }
                let snapshot = if as_new {
                    self.inner
                        .agent
                        .resume_as_new_chat(record.chat_id.into())
                        .await
                        .map_err(agent_access_error)?
                } else {
                    self.inner
                        .agent
                        .resume_chat(record.chat_id.into())
                        .await
                        .map_err(agent_access_error)?
                };
                let folder = chat_folder(&snapshot)?;
                let launch = launch.unwrap_or_else(|| LaunchSpec::embedded(folder.clone()));
                if snapshot.delegation.is_none() {
                    self.inner
                        .agent
                        .set_host_work_context(
                            snapshot.id,
                            HostCallContext {
                                folder,
                                workspace_id: launch
                                    .host_work_context
                                    .as_ref()
                                    .and_then(|context| context.workspace.clone()),
                                interaction: None,
                            },
                        )
                        .await
                        .map_err(agent_access_error)?;
                }
                if let Some(delegation) = &snapshot.delegation {
                    for ancestor in delegation.lineage() {
                        self.register(ancestor.chat_id.into(), launch.clone()).await;
                    }
                }
                let execution = self.register(snapshot.id.into(), launch).await;
                Ok(CommandResult::Loaded {
                    execution,
                    snapshot: Box::new(self.snapshot(execution).await?),
                })
            }),
            AgentCommand::SendFamilyInput {
                source_execution,
                target_execution,
                origin,
                input,
            } => Box::pin(async move {
                if !self.inner.verified_sources.load(Ordering::SeqCst) {
                    self.inner
                        .views
                        .validate_displayed_source(&origin, source_execution)
                        .await?;
                }
                let source_chat = self.chat(source_execution).await?;
                let target_chat = self.chat(target_execution).await?;
                let UserInput::Text {
                    text,
                    contexts,
                    paths,
                } = input
                else {
                    return Err(AccessError::invalid("directed family input is text only"));
                };
                let message_id = tickloop_ids::generate();
                let source_handle = self
                    .inner
                    .agent
                    .subagent_registration(source_chat.into())
                    .await
                    .map_err(agent_access_error)?
                    .map(|record| record.handle)
                    .unwrap_or_else(|| "main".into());
                let target_handle = self
                    .inner
                    .agent
                    .subagent_registration(target_chat.into())
                    .await
                    .map_err(agent_access_error)?
                    .map(|record| record.handle)
                    .unwrap_or_else(|| "main".into());
                let directed = DirectedHumanSource {
                    message_id,
                    source_chat_id: source_chat.into(),
                    source_handle,
                    original_submission: text,
                    original_contexts: contexts.clone(),
                    original_paths: paths.clone(),
                    source_session_id: origin.view.session_id,
                    source_view_id: origin.view.view_id,
                    source_display_version: origin.display_version,
                };
                let context = self
                    .host_context(target_execution, target_chat, None, false)
                    .await?;
                let receipt = self
                    .inner
                    .agent
                    .submit_family_input(target_chat.into(), directed, contexts, paths, context)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::FamilyInputSent {
                    source_execution,
                    target_execution,
                    receipt,
                    message_id,
                    target_handle,
                })
            }),
            AgentCommand::Submit {
                execution,
                input,
                origin,
            } => Box::pin(self.submit(execution, input, origin)),
            AgentCommand::ContinueLoop { execution, .. } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                self.inner
                    .agent
                    .continue_loop(chat.into())
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }),
            AgentCommand::DecidePermission {
                request,
                allow,
                scope,
                comment,
                rule,
            }
            | AgentCommand::ManagedDecidePermission {
                request,
                allow,
                scope,
                comment,
                rule,
                ..
            } => Box::pin(async move {
                self.claim_answer(request, PendingRequestKind::Permission, envelope.command_id)
                    .await?;
                let chat = self.chat(request.execution).await?;
                let result = if let Some(rule) = rule {
                    self.inner
                        .agent
                        .answer_permission_with_rule(chat.into(), request.request_id, rule)
                        .await
                } else {
                    self.inner
                        .agent
                        .answer_permission_with_comment(
                            chat.into(),
                            request.request_id,
                            allow,
                            scope,
                            comment,
                        )
                        .await
                };
                if let Err(error) = result {
                    self.release_answer(request).await;
                    return Err(agent_access_error(error));
                }
                Ok(CommandResult::Done)
            }),
            AgentCommand::AnswerQuestion { request, answers } => Box::pin(async move {
                self.claim_answer(request, PendingRequestKind::Question, envelope.command_id)
                    .await?;
                let chat = self.chat(request.execution).await?;
                if let Err(error) = self
                    .reply_to_question(
                        chat,
                        envelope.command_id,
                        request.request_id,
                        QuestionReply::Answered(answers),
                    )
                    .await
                {
                    self.release_answer(request).await;
                    return Err(agent_access_error(error));
                }
                Ok(CommandResult::Done)
            }),
            AgentCommand::DeclineQuestion { request } => Box::pin(async move {
                self.claim_answer(request, PendingRequestKind::Question, envelope.command_id)
                    .await?;
                let chat = self.chat(request.execution).await?;
                if let Err(error) = self
                    .reply_to_question(
                        chat,
                        envelope.command_id,
                        request.request_id,
                        QuestionReply::Declined,
                    )
                    .await
                {
                    self.release_answer(request).await;
                    return Err(agent_access_error(error));
                }
                Ok(CommandResult::Done)
            }),
            AgentCommand::Configure {
                execution,
                change,
                provider,
            } => Box::pin(async move {
                if provider.is_some() {
                    return Err(AccessError::invalid(
                        "External Provider definitions are not supported; choose a built-in model",
                    ));
                }
                Box::pin(self.configure(execution, change)).await
            }),
            AgentCommand::EditHistory { execution, action } => {
                Box::pin(self.history(execution, action, envelope.command_id))
            }
            AgentCommand::ManageLoop { execution, action } => {
                Box::pin(self.manage_loop(execution, action))
            }
            AgentCommand::ManageTask { execution, action } => {
                Box::pin(self.manage_task(execution, action))
            }
            AgentCommand::ManageGoal { execution, action } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                self.inner
                    .agent
                    .manage_goal(chat.into(), action)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }),
            AgentCommand::ClearCompletedTasks {
                execution,
                root_loop_id,
                expected_revision,
            } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                self.inner
                    .agent
                    .clear_completed_tasks(chat.into(), root_loop_id.into(), expected_revision)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }),
            AgentCommand::EnqueueAfterTurn { execution, action } => Box::pin(async move {
                let chat = self.chat(execution).await?;
                if let AfterTurnAction::Submit {
                    origin: Some(origin),
                    ..
                } = &action
                {
                    self.host_context(execution, chat, Some(origin.clone()), true)
                        .await?;
                }
                let label = action_label(&action);
                let command_id = self
                    .inner
                    .agent
                    .queue_command(chat.into(), label)
                    .await
                    .map_err(agent_access_error)?;
                self.inner
                    .queue
                    .lock()
                    .await
                    .insert(command_id, (execution, action));
                Ok(CommandResult::Queued { command_id })
            }),
            AgentCommand::RefreshConfiguration { execution } => Box::pin(async move {
                let snapshot = self.snapshot(execution).await?;
                self.configure(
                    execution,
                    ConfigurationChange::Model {
                        config: snapshot.model,
                        thinking: None,
                    },
                )
                .await
            }),
            AgentCommand::ReconnectMcpServer { execution, name } => Box::pin(async move {
                self.chat(execution).await?;
                self.inner
                    .agent
                    .reconnect_mcp_server(&name)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }),
            AgentCommand::TrustWorkingDirectory { folder } => Box::pin(async move {
                self.inner
                    .agent
                    .trust_working_directory(folder)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }),
            AgentCommand::FlushTranscripts => Box::pin(async move {
                self.inner.agent.flush_transcripts().await;
                Ok(CommandResult::Done)
            }),
            AgentCommand::UpgradeRuntime { .. } => {
                Box::pin(std::future::ready(Err(AccessError::new(
                    AccessErrorCode::Unsupported,
                    "runtime upgrades are coordinated by the daemon",
                ))))
            }
            AgentCommand::ReplaceTickhost => Box::pin(std::future::ready(Err(
                AccessError::invalid("only the Agent daemon replaces its Tickhost"),
            ))),
            AgentCommand::StopAgent { .. } | AgentCommand::ForgetEndedAgent { .. } => {
                Box::pin(std::future::ready(Err(AccessError::new(
                    AccessErrorCode::Unsupported,
                    "only the Agent daemon keeps ended Agents",
                ))))
            }
        }
    }

    /// A reply to a question is a ledger input under the command's id, so a repeated command
    /// replies once.
    async fn reply_to_question(
        &self,
        chat: Uuid,
        command_id: Uuid,
        request: Uuid,
        reply: QuestionReply,
    ) -> Result<(), tickloop_harness::runtime::AgentError> {
        self.inner
            .agent
            .advance(
                chat.into(),
                TurnRequest {
                    input_id: command_id.into(),
                    op: TurnOp::Answer { request, reply },
                    delivery: Delivery::Queue,
                    host_context: None,
                },
            )
            .await
            .map(|_| ())
    }

    async fn claim_answer(
        &self,
        request: RequestRef,
        kind: PendingRequestKind,
        command_id: Uuid,
    ) -> Result<(), AccessError> {
        if request.kind != kind {
            return Err(AccessError::invalid(
                "answer kind does not match the original request",
            ));
        }
        let snapshot = self.snapshot(request.execution).await?;
        let pending = match kind {
            PendingRequestKind::Permission => snapshot
                .permissions
                .iter()
                .any(|pending| pending.id == request.request_id),
            PendingRequestKind::Question => snapshot
                .questions
                .iter()
                .any(|pending| pending.id == request.request_id),
        };
        if !pending {
            return Err(AccessError::new(
                AccessErrorCode::AlreadyResolved,
                "original request has already been resolved or cancelled",
            ));
        }
        let mut answers = self.inner.answers.lock().await;
        let state = answers
            .entry((request.execution, request.request_id, kind))
            .or_insert(AnswerState {
                version: 1,
                claimed: None,
            });
        if state.version != request.version {
            return Err(AccessError::new(
                AccessErrorCode::VersionConflict,
                "request version is no longer current",
            ));
        }
        if state.claimed.is_some() {
            return Err(AccessError::new(
                AccessErrorCode::Responding,
                "another View already claimed this request",
            ));
        }
        state.claimed = Some(command_id);
        Ok(())
    }

    async fn release_answer(&self, request: RequestRef) {
        if let Some(state) = self.inner.answers.lock().await.get_mut(&(
            request.execution,
            request.request_id,
            request.kind,
        )) {
            state.claimed = None;
            state.version += 1;
        }
    }

    async fn configure(
        &self,
        execution: ExecutionRef,
        change: ConfigurationChange,
    ) -> Result<CommandResult, AccessError> {
        let chat = self.chat(execution).await?;
        let agent = &self.inner.agent;
        match change {
            ConfigurationChange::TaskToolsEnabled { enabled } => agent
                .set_task_tools_enabled(chat.into(), enabled)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::Model { config, thinking } => {
                let config = self.builtin_config(config)?;
                let change = match thinking {
                    Some(effort) => agent
                        .set_model_with_thinking(chat.into(), config, effort)
                        .await
                        .map_err(agent_access_error)?,
                    None => agent
                        .set_model(chat.into(), config)
                        .await
                        .map_err(agent_access_error)?,
                };
                return Ok(CommandResult::ModelChanged(change));
            }
            ConfigurationChange::ReasoningEffort { effort } => agent
                .set_reasoning_effort(chat.into(), effort)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::ThinkingEnabled { enabled } => agent
                .set_thinking_enabled(chat.into(), enabled)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::ExecutionMode { mode } => agent
                .set_execution_mode(chat.into(), mode)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::ContextWindow { tokens } => {
                if self.inner.builtin_catalog_only.load(Ordering::SeqCst) {
                    return Err(AccessError::invalid(
                        "Context window is version-owned by the built-in catalog and cannot be overridden",
                    ));
                }
                agent
                    .set_context_window(chat.into(), tokens)
                    .await
                    .map_err(agent_access_error)?
            }
            ConfigurationChange::Search { config } => agent
                .set_search(chat.into(), config)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::Directories { folder, additional } => agent
                .set_directories(chat.into(), folder, additional)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::ReplaceRules { rules } => agent
                .set_permission_rules(chat.into(), rules)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::ProjectConstraints { rules } => agent
                .set_project_permission_constraints(chat.into(), rules)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::ManagedConstraints { rules } => agent
                .set_managed_permission_constraints(chat.into(), rules)
                .await
                .map_err(agent_access_error)?,
            ConfigurationChange::GenerateAutoEnvironment { options } => {
                // Keep setup and its model retries out of every command-dispatch stack frame.
                return Ok(CommandResult::AutoEnvironmentDraft(
                    Box::pin(agent.generate_auto_environment(chat.into(), options))
                        .await
                        .map_err(agent_access_error)?,
                ));
            }
            ConfigurationChange::PermissionChange { change, expected } => {
                return match agent
                    .apply_permission_change(chat.into(), change, expected)
                    .await
                {
                    Ok(view) => Ok(CommandResult::Permissions(Box::new(view))),
                    Err(tickloop_harness::runtime::PermissionChangeError::Conflict { latest }) => {
                        Err(AccessError {
                            code: AccessErrorCode::VersionConflict,
                            message: "permissions changed while editing".into(),
                            latest_permissions: Some(latest),
                            path: None,
                        })
                    }
                    Err(error) => Err(AccessError::invalid(error.to_string())),
                };
            }
        }
        Ok(CommandResult::Done)
    }

    async fn history(
        &self,
        execution: ExecutionRef,
        action: HistoryAction,
        command_id: Uuid,
    ) -> Result<CommandResult, AccessError> {
        let chat = self.chat(execution).await?;
        self.history_action(execution, chat, action, command_id)
            .await
    }

    /// Each history action is its own boxed future, as in `dispatch`.
    fn history_action(
        &self,
        execution: ExecutionRef,
        chat: Uuid,
        action: HistoryAction,
        command_id: Uuid,
    ) -> CommandFuture<'_> {
        let agent = &self.inner.agent;
        match action {
            HistoryAction::Branch { name } => Box::pin(async move {
                let result = agent
                    .branch_chat(BranchRequest {
                        request_id: command_id,
                        source_chat_id: chat.into(),
                        name,
                    })
                    .await
                    .map_err(agent_access_error)?;
                let launch = self
                    .inner
                    .launches
                    .lock()
                    .await
                    .get(&execution)
                    .cloned()
                    .map_or_else(
                        || chat_folder(&result.snapshot).map(LaunchSpec::embedded),
                        Ok,
                    )?;
                let execution = self.register(result.child_chat_id.into(), launch).await;
                Ok(CommandResult::Branch {
                    execution,
                    source_chat_id: result.source_chat_id.into(),
                    child_chat_id: result.child_chat_id.into(),
                    name: result.name,
                    snapshot: Box::new(self.snapshot(execution).await?),
                    session_permissions: result.session_permissions,
                })
            }),
            HistoryAction::Compact { instructions } => Box::pin(async move {
                let result = agent
                    .compact_with_instructions(chat.into(), instructions)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Compact {
                    summary: result.summary,
                    snapshot: Box::new(self.snapshot(execution).await?),
                })
            }),
            HistoryAction::DirectedSummary { request } => Box::pin(async move {
                let result = agent
                    .summarize_checkpoint(chat.into(), request)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::DirectedSummary {
                    snapshot: Box::new(self.snapshot(execution).await?),
                    direction: result.direction,
                    summary: result.summary,
                    guidance: result.guidance,
                    messages_summarized: result.messages_summarized,
                    draft: result.draft,
                    continuation_error: result.continuation_error,
                })
            }),
            HistoryAction::Rewind { message_id, mode } => Box::pin(async move {
                let result = agent
                    .rewind(chat.into(), message_id, mode)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Rewind {
                    snapshot: Box::new(self.snapshot(execution).await?),
                    mode: result.mode,
                    code: result.code,
                    conversation_restored: result.conversation_restored,
                    draft: result.draft,
                })
            }),
            HistoryAction::TakeBackInterruptedPrompt { run_id } => Box::pin(async move {
                match agent
                    .take_back_interrupted_prompt(chat.into(), run_id.into())
                    .await
                    .map_err(agent_access_error)?
                {
                    Some(draft) => Ok(CommandResult::PromptTakenBack {
                        snapshot: Box::new(self.snapshot(execution).await?),
                        draft,
                    }),
                    None => Ok(CommandResult::Done),
                }
            }),
        }
    }

    async fn manage_loop(
        &self,
        execution: ExecutionRef,
        action: LoopAction,
    ) -> Result<CommandResult, AccessError> {
        let chat = self.chat(execution).await?;
        let agent = &self.inner.agent;
        match action {
            LoopAction::Stop => Ok(CommandResult::Operations(
                agent.stop(chat.into()).await.map_err(agent_access_error)?,
            )),
            LoopAction::Interrupt => Ok(CommandResult::Operations(
                agent
                    .interrupt(chat.into())
                    .await
                    .map_err(agent_access_error)?,
            )),
            LoopAction::SendNow { run_id } => Ok(CommandResult::Operations(
                agent
                    .send_now(chat.into(), run_id.into())
                    .await
                    .map_err(agent_access_error)?,
            )),
            LoopAction::DisableEventSources => {
                agent.disable_event_sources_for_chat(chat.into()).await;
                Ok(CommandResult::Done)
            }
            LoopAction::ApprovePlan {
                version,
                mode,
                feedback,
            } => {
                match agent
                    .approve_plan_with_feedback(chat.into(), version, mode, feedback)
                    .await
                    .map_err(agent_access_error)?
                {
                    Some(continuation) => Ok(CommandResult::Text(continuation)),
                    None => Ok(CommandResult::Done),
                }
            }
            LoopAction::EditPlan { version, markdown } => {
                agent
                    .edit_plan(chat.into(), version, markdown)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }
            LoopAction::DeclinePlan { version, feedback } => {
                match agent
                    .decline_plan(chat.into(), version, feedback)
                    .await
                    .map_err(agent_access_error)?
                {
                    Some(receipt) => Ok(CommandResult::Submitted(receipt)),
                    None => Ok(CommandResult::Done),
                }
            }
            LoopAction::UpdateSubscription {
                subscription_id,
                change,
            } => {
                agent
                    .update_tick_subscription(chat.into(), subscription_id, change)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }
            // Held updates go through their subscription's own delivery, with no receipt.
            LoopAction::TickUpdate { instance } => Ok(
                match agent
                    .tick_send_update_for_user(chat.into(), instance)
                    .await
                    .map_err(agent_access_error)?
                {
                    Some(receipt) => CommandResult::Submitted(receipt),
                    None => CommandResult::Done,
                },
            ),
            LoopAction::TickManage { instance } => {
                agent
                    .tick_subscribe_for_user(chat.into(), instance)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }
            LoopAction::RetryDeniedOperation { record_id } => Ok(CommandResult::Submitted(
                agent
                    .request_denial_retry(chat.into(), record_id)
                    .await
                    .map_err(agent_access_error)?,
            )),
            LoopAction::SetScheduledTaskPaused { task_id, paused } => {
                agent
                    .set_scheduled_task_paused(chat.into(), task_id, paused)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }
            LoopAction::DeleteScheduledTask { task_id } => {
                agent
                    .delete_scheduled_task(chat.into(), task_id)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }
            LoopAction::DecideScheduledTask { task_id, decision } => {
                agent
                    .decide_scheduled_task(chat.into(), task_id, decision)
                    .await
                    .map_err(agent_access_error)?;
                Ok(CommandResult::Done)
            }
        }
    }

    async fn manage_task(
        &self,
        execution: ExecutionRef,
        action: TaskAction,
    ) -> Result<CommandResult, AccessError> {
        let chat = self.chat(execution).await?;
        match action {
            TaskAction::SubmitShell {
                command,
                background,
            } => Ok(CommandResult::ShellSubmitted(
                self.inner
                    .agent
                    .submit_user_shell(chat.into(), command, background)
                    .await
                    .map_err(agent_access_error)?,
            )),
            TaskAction::Background { task } | TaskAction::Stop { task } => {
                if task.execution != execution {
                    return Err(AccessError::invalid(
                        "task owner does not match the captured execution",
                    ));
                }
                self.validate_task(chat, task).await?;
                let action = if matches!(action, TaskAction::Background { .. }) {
                    ShellTaskAction::Background
                } else {
                    ShellTaskAction::Stop
                };
                ShellTaskCenter::new(self.inner.agent.clone())
                    .dispatch(chat.into(), task.task_id, action)
                    .await
                    .map(CommandResult::Task)
                    .map_err(|error| AccessError::invalid(error.to_string()))
            }
            TaskAction::TakeBackQueued { draft_empty } => {
                let taken = self
                    .inner
                    .agent
                    .take_queued_inputs(chat.into(), draft_empty)
                    .await
                    .map_err(agent_access_error)?;
                let mut queue = self.inner.queue.lock().await;
                for input in &taken {
                    queue.remove(&input.id);
                }
                Ok(CommandResult::QueueTaken(taken))
            }
        }
    }

    async fn validate_task(&self, chat: Uuid, reference: TaskRef) -> Result<(), AccessError> {
        let tasks = ShellTaskCenter::new(self.inner.agent.clone())
            .snapshot(chat.into())
            .await
            .map_err(|error| AccessError::invalid(error.to_string()))?
            .tasks;
        let origin = match reference.origin {
            TaskOrigin::Agent => ShellTaskOrigin::Agent,
            TaskOrigin::User => ShellTaskOrigin::User,
        };
        if !tasks.iter().any(|task| {
            task.id == reference.task_id && task.chat_id == chat && task.origin == origin
        }) {
            return Err(AccessError::new(
                AccessErrorCode::NotFound,
                "task owner or origin does not match the captured reference",
            ));
        }
        Ok(())
    }

    async fn publish(&self, execution: Option<ExecutionRef>, change: ObservationChange) {
        let _publishing = self.inner.publishing.lock().await;
        let sequence = self.inner.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let update = bounded_update(ObservationUpdate {
            cursor: Cursor {
                daemon_epoch: self.inner.epoch,
                execution,
                sequence,
            },
            change,
        });
        let _ = self.inner.changes.send(update);
    }

    async fn managed_configuration(
        &self,
        execution: ExecutionRef,
        entry: &super::managed::ManagedEntry,
    ) -> Result<tickloop_code_access::ManagedConfiguration, AccessError> {
        use tickloop_code_access::{ManagedConfiguration, ManagedModelInfo};
        use tickloop_harness::model::ReasoningEffortExt;
        use tickloop_harness::protocol::{ExecutionMode, ReasoningEffort};
        if entry.launch.profile != tickloop_code_access::ExecutionProfile::Acp {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "configuration requires ACP",
            ));
        }
        let snapshot = self
            .inner
            .agent
            .snapshot(entry.chat.into())
            .await
            .map_err(agent_access_error)?;
        let current = &snapshot.model;
        let models = self
            .managed_models(current, entry.launch.config_root.clone())
            .await
            .into_iter()
            .map(|(id, config)| ManagedModelInfo {
                id,
                name: config.name,
                provider: config.provider_name,
            })
            .collect();
        let ceiling = if let Ok(Some(metadata)) =
            self.inner.agent.read_acp_session(entry.chat.into()).await
        {
            metadata.startup_permission_ceiling.mode
        } else {
            entry
                .launch
                .host_options
                .execution_mode_override
                .unwrap_or(ExecutionMode::Auto)
        };
        let modes = match ceiling {
            ExecutionMode::AcceptEdits => vec![ExecutionMode::AcceptEdits],
            ExecutionMode::FullAccess => vec![
                ExecutionMode::Auto,
                ExecutionMode::AcceptEdits,
                ExecutionMode::FullAccess,
            ],
            _ => vec![ExecutionMode::Auto, ExecutionMode::AcceptEdits],
        };
        let _ = execution;
        Ok(ManagedConfiguration {
            model_id: current.model_id.clone(),
            mode: snapshot.policy.mode,
            models,
            modes,
            reasoning_effort: snapshot.reasoning_effort,
            efforts: ReasoningEffort::choices(current).to_vec(),
        })
    }

    /// Every model of each signed-in built-in provider, as the Console offers them, with the
    /// option id a managed client selects it by: the bare model id for the provider that owns
    /// it (the current provider first, else the first listed), `provider/model` for the others.
    /// Built-in model ids contain no `/`, so the two forms never collide.
    async fn managed_models(
        &self,
        current: &tickloop_harness::protocol::ModelConfig,
        config_root: Option<std::path::PathBuf>,
    ) -> Vec<(String, tickloop_harness::protocol::ModelConfig)> {
        let scope =
            tickloop_harness::model::auth::current_provider_credential_scope(config_root.clone());
        let mut providers: Vec<Vec<tickloop_harness::protocol::ModelConfig>> = Vec::new();
        for config in tickloop_harness::model::cards::ModelCatalog::defaults().profiles() {
            match providers.last_mut() {
                Some(provider) if provider[0].provider_id == config.provider_id => {
                    provider.push(config)
                }
                _ => providers.push(vec![config]),
            }
        }
        providers.retain(|provider| {
            provider[0].provider_id == current.provider_id
                || tickloop_harness::model::auth::provider_signed_in(&provider[0], &scope)
                    == Ok(true)
        });
        let cancel = CancellationToken::new();
        let directories = futures_util::future::join_all(providers.iter().map(|provider| {
            tokio::time::timeout(
                Duration::from_secs(15),
                self.inner.account_models.discover(
                    &provider[0],
                    config_root.as_deref(),
                    false,
                    &cancel,
                ),
            )
        }))
        .await;
        cancel.cancel();
        // A provider whose directory cannot be read keeps its catalog models, as in the Console.
        let listed: Vec<_> = providers
            .into_iter()
            .zip(directories)
            .flat_map(|(provider, directory)| {
                let directory = directory.ok().and_then(Result::ok);
                provider.into_iter().filter(move |config| {
                    (config.provider_id == current.provider_id
                        && config.model_id == current.model_id)
                        || directory.as_ref().is_none_or(|directory| {
                            directory.availability(&config.model).selectable()
                        })
                })
            })
            .collect();
        let mut owners = HashMap::new();
        for config in listed
            .iter()
            .filter(|config| config.provider_id == current.provider_id)
            .chain(&listed)
        {
            owners
                .entry(config.model_id.clone())
                .or_insert_with(|| config.provider_id.clone());
        }
        listed
            .into_iter()
            .map(|config| {
                let id = if owners[&config.model_id] == config.provider_id {
                    config.model_id.clone()
                } else {
                    format!("{}/{}", config.provider_id, config.model_id)
                };
                (id, config)
            })
            .collect()
    }

    fn builtin_config(
        &self,
        config: tickloop_harness::protocol::ModelConfig,
    ) -> Result<tickloop_harness::protocol::ModelConfig, AccessError> {
        if !self.inner.builtin_catalog_only.load(Ordering::SeqCst) {
            return Ok(config);
        }
        tickloop_harness::model::cards::ModelCatalog::defaults()
            .get(&config.provider_id, &config.model_id)
            .ok_or_else(|| {
                AccessError::invalid("Only built-in Provider/model selections are supported")
            })
    }

    async fn observe_account_models(
        &self,
        execution: ExecutionRef,
        config: tickloop_harness::protocol::ModelConfig,
        refresh: bool,
    ) -> Result<Observation, AccessError> {
        self.chat(execution).await?;
        let config = self.builtin_config(config)?;
        let root = self
            .inner
            .launches
            .lock()
            .await
            .get(&execution)
            .and_then(|launch| launch.config_root.clone());
        let cursor = Cursor {
            daemon_epoch: self.inner.epoch,
            execution: Some(execution),
            sequence: self.inner.sequence.load(Ordering::SeqCst),
        };
        let cache = self.inner.account_models.clone();
        let cancel = CancellationToken::new();
        let _cancel_on_drop = cancel.clone().drop_guard();
        if !refresh
            && let Some(directory) = cache
                .cached_directory(&config, root.as_deref(), &cancel)
                .await
        {
            return Ok(Observation::single(
                ObservationValue::AccountModels(directory),
                cursor,
                None,
            ));
        }
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let result = tokio::select! {
                biased;
                () = sender.closed() => { cancel.cancel(); return; }
                result = cache.discover(&config, root.as_deref(), refresh, &cancel) => result,
            };
            let change = match result {
                Ok(_) => ObservationChange::Changed,
                Err(error) => ObservationChange::AccountModels(Err(error)),
            };
            let _ = sender.send(ObservationUpdate { cursor, change }).await;
        });
        Ok(Observation::new(
            ObservationValue::AccountModelsPending,
            cursor,
            None,
            Some(receiver),
            None,
        ))
    }

    async fn observe_search_test(
        &self,
        execution: ExecutionRef,
        config: tickloop_harness::protocol::SearchConfig,
    ) -> Result<Observation, AccessError> {
        self.chat(execution).await?;
        let root = self
            .inner
            .launches
            .lock()
            .await
            .get(&execution)
            .and_then(|launch| launch.config_root.clone());
        let cursor = Cursor {
            daemon_epoch: self.inner.epoch,
            execution: Some(execution),
            sequence: self.inner.sequence.load(Ordering::SeqCst),
        };
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let _guard = cancel.clone().drop_guard();
            let result = tokio::select! {
                biased;
                () = sender.closed() => return,
                result = tickloop_harness::tools::web_search::test_connection(&config, root.as_deref(), Some(tickloop_harness::tools::web_search::DEFAULT_SEARCH_SETTINGS), cancel.clone()) => result,
            };
            let _ = sender
                .send(ObservationUpdate {
                    cursor,
                    change: ObservationChange::SearchTest(result),
                })
                .await;
        });
        Ok(Observation::new(
            ObservationValue::SearchTestPending,
            cursor,
            None,
            Some(receiver),
            None,
        ))
    }

    /// The answer to an Agent query other than the Chat itself.
    async fn detail_value(
        &self,
        execution: ExecutionRef,
        chat: Uuid,
        detail: AgentDetailQuery,
    ) -> Result<ObservationValue, AccessError> {
        Ok(match detail {
            AgentDetailQuery::OperationReview { run_id, call_id } => {
                ObservationValue::OperationReview(
                    self.inner
                        .agent
                        .operation_review(chat.into(), run_id.into(), &call_id)
                        .await
                        .map_err(agent_access_error)?,
                )
            }
            AgentDetailQuery::ManagedConfiguration { token } => {
                let entry = self
                    .inner
                    .managed
                    .authorize_worker(&AgentCommand::ManagedHeartbeat { execution, token })
                    .await?
                    .ok_or_else(|| AccessError::invalid("managed execution missing"))?;
                ObservationValue::ManagedConfiguration(
                    self.managed_configuration(execution, &entry).await?,
                )
            }
            AgentDetailQuery::ManagedHistory { token, cursor } => {
                let entry = self
                    .inner
                    .managed
                    .authorize_worker(&AgentCommand::ManagedHeartbeat { execution, token })
                    .await?
                    .ok_or_else(|| AccessError::invalid("managed execution missing"))?;
                if entry.launch.profile != tickloop_code_access::ExecutionProfile::Acp
                    || !entry.launch.session_persistence
                {
                    return Err(AccessError::invalid(
                        "history replay requires a persistent ACP session",
                    ));
                }
                if cursor.is_none() {
                    self.inner
                        .agent
                        .flush_acp_session(chat.into())
                        .await
                        .map_err(agent_access_error)?;
                }
                ObservationValue::ManagedHistory(entry.history(cursor).await?)
            }
            AgentDetailQuery::ProviderCredentials => {
                let launches = self.inner.launches.lock().await;
                let launch = launches.get(&execution);
                ObservationValue::ProviderCredentials(Box::new(
                    tickloop_harness::model::auth::current_provider_credential_scope(
                        launch.and_then(|launch| launch.config_root.clone()),
                    ),
                ))
            }
            AgentDetailQuery::AccountModels { .. } | AgentDetailQuery::TestSearch { .. } => {
                unreachable!("provider checks return their own observation")
            }
            AgentDetailQuery::Snapshot | AgentDetailQuery::Status => {
                unreachable!("Chat answers are read with their feed watermark")
            }
            AgentDetailQuery::TaskToolsSetting => ObservationValue::TaskToolsSetting(
                self.inner
                    .agent
                    .task_tools_setting(chat.into())
                    .await
                    .map_err(agent_access_error)?,
            ),
            AgentDetailQuery::Operation { run_id, call_id } => ObservationValue::Operation(
                self.inner
                    .agent
                    .operation(chat.into(), run_id.into(), &call_id)
                    .await
                    .map_err(agent_access_error)?
                    .map(Box::new),
            ),
            AgentDetailQuery::Events { after } => ObservationValue::Events(
                self.inner
                    .agent
                    .events_after(chat.into(), after)
                    .await
                    .map_err(agent_access_error)?,
            ),
            AgentDetailQuery::Permissions => ObservationValue::Permissions(Box::new(
                self.inner
                    .agent
                    .permissions_view(chat.into())
                    .await
                    .map_err(agent_access_error)?,
            )),
            AgentDetailQuery::ApprovalHistory => ObservationValue::ApprovalHistory(
                self.inner
                    .agent
                    .approval_history_view(chat.into())
                    .await
                    .map_err(agent_access_error)?,
            ),
            AgentDetailQuery::ContextBreakdown => ObservationValue::ContextBreakdown(Box::new(
                self.inner
                    .agent
                    .context_breakdown(chat.into())
                    .await
                    .map_err(agent_access_error)?,
            )),
            AgentDetailQuery::Tasks => ObservationValue::Tasks(
                ShellTaskCenter::new(self.inner.agent.clone())
                    .snapshot(chat.into())
                    .await
                    .map_err(|error| AccessError::invalid(error.to_string()))?
                    .tasks,
            ),
            AgentDetailQuery::TaskOutput {
                task,
                offset,
                limit,
            } => {
                if task.execution != execution {
                    return Err(AccessError::invalid(
                        "output task belongs to another execution",
                    ));
                }
                self.validate_task(chat, task).await?;
                ObservationValue::TaskOutput(
                    ShellTaskCenter::new(self.inner.agent.clone())
                        .read_output(chat.into(), task.task_id, offset, limit)
                        .await
                        .map_err(|error| AccessError::invalid(error.to_string()))?,
                )
            }
            AgentDetailQuery::Subscriptions => ObservationValue::Subscriptions(
                self.inner
                    .agent
                    .tick_subscriptions(chat.into())
                    .await
                    .map_err(agent_access_error)?,
            ),
            AgentDetailQuery::RewindPoints => ObservationValue::RewindPoints(
                self.inner
                    .agent
                    .snapshot(chat.into())
                    .await
                    .map_err(agent_access_error)?
                    .rewind_points(),
            ),
            AgentDetailQuery::RewindFiles { message_id } => ObservationValue::RewindFiles(
                self.inner
                    .agent
                    .rewind_file_changes(chat.into(), message_id)
                    .await
                    .map_err(agent_access_error)?,
            ),
            AgentDetailQuery::Skills { name } => ObservationValue::Skills {
                available: match name {
                    Some(name) => Some(
                        self.inner
                            .agent
                            .has_skill(chat.into(), &name)
                            .await
                            .map_err(agent_access_error)?,
                    ),
                    None => None,
                },
                warnings: self
                    .inner
                    .agent
                    .skill_warnings(chat.into())
                    .await
                    .map_err(agent_access_error)?,
                commands: self
                    .inner
                    .agent
                    .user_skill_catalog(chat.into())
                    .await
                    .map_err(agent_access_error)?
                    .into_iter()
                    .map(
                        |(name, description)| tickloop_code_access::ManagedSkillInfo {
                            name,
                            description,
                        },
                    )
                    .collect(),
            },
            AgentDetailQuery::Warnings => {
                ObservationValue::Warnings(self.live_facts(execution, chat).await?.warnings)
            }
            AgentDetailQuery::McpServers => {
                ObservationValue::McpServers(self.inner.agent.mcp_servers().await)
            }
            AgentDetailQuery::PlanFile => ObservationValue::PlanFile(
                self.inner
                    .agent
                    .plan_file(chat.into())
                    .await
                    .map_err(agent_access_error)?,
            ),
        })
    }

    async fn observe_agent(
        &self,
        execution: ExecutionRef,
        detail: AgentDetailQuery,
        after: Option<Cursor>,
    ) -> Result<Observation, AccessError> {
        if let AgentDetailQuery::AccountModels { config, refresh } = &detail {
            return self
                .observe_account_models(execution, *config.clone(), *refresh)
                .await;
        }
        if let AgentDetailQuery::TestSearch { config } = &detail {
            return self.observe_search_test(execution, config.clone()).await;
        }
        let mut receiver = self.inner.changes.subscribe();
        let sequence = self.inner.sequence.load(Ordering::SeqCst);
        let chat = self.chat(execution).await?;
        // The feed carries the events after the ones the answer reflects. Only the two Chat
        // answers read the Chat's state; every other answer goes by the Chat's sequence now.
        let (initial, watermark) = match detail {
            AgentDetailQuery::Snapshot => {
                let snapshot = self.snapshot(execution).await?;
                let watermark = snapshot.last_sequence;
                (ObservationValue::Agent(Box::new(snapshot)), watermark)
            }
            AgentDetailQuery::Status => {
                let status = self.status(execution).await?;
                let watermark = status.chat.last_sequence;
                (ObservationValue::Status(Box::new(status)), watermark)
            }
            detail => (
                self.detail_value(execution, chat, detail).await?,
                self.inner
                    .agent
                    .last_sequence(chat.into())
                    .await
                    .map_err(agent_access_error)?,
            ),
        };
        let cursor = Cursor {
            daemon_epoch: self.inner.epoch,
            execution: Some(execution),
            sequence,
        };
        let reset = after.and_then(|old| {
            if old.daemon_epoch != cursor.daemon_epoch {
                Some(ResetReason::EpochChanged)
            } else if old.execution != cursor.execution || old.sequence > cursor.sequence {
                Some(ResetReason::ConversationChanged)
            } else if cursor.sequence.saturating_sub(old.sequence) > REPLAY_CAPACITY as u64 {
                Some(ResetReason::CursorExpired)
            } else {
                None
            }
        });
        let (sender, changes) = mpsc::channel(FEED_CAPACITY);
        let access = self.clone();
        tokio::spawn(async move {
            loop {
                let received = tokio::select! { _ = sender.closed() => break, _ = access.inner.cancel.cancelled() => break, received = receiver.recv() => received };
                let update = match received {
                    Ok(update) if update.cursor.execution == Some(execution) => {
                        if update.cursor.sequence <= sequence {
                            continue;
                        }
                        if let ObservationChange::Event(event) = &update.change
                            && event.sequence <= watermark
                        {
                            continue;
                        }
                        update
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        match access.snapshot(execution).await {
                            Ok(snapshot) => ObservationUpdate {
                                cursor: Cursor {
                                    daemon_epoch: access.inner.epoch,
                                    execution: Some(execution),
                                    sequence: access.inner.sequence.load(Ordering::SeqCst),
                                },
                                change: ObservationChange::Reset {
                                    reason: ResetReason::Lagged,
                                    snapshot: Box::new(snapshot),
                                },
                            },
                            Err(_) => break,
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if sender.send(bounded_update(update)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Observation::new(
            initial,
            cursor,
            reset,
            Some(changes),
            None,
        ))
    }
}

#[async_trait]
impl AgentAccess for EmbeddedAgentAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        match query {
            ObservationRequest::Agent {
                execution,
                detail,
                after,
            } => self.observe_agent(execution, detail, after).await,
            // Only the Agent daemon controls a Tickhost whose build can differ from its own.
            ObservationRequest::RuntimeVersion => Ok(Observation::single(
                ObservationValue::RuntimeVersion(Box::new(tickloop_code_access::RuntimeVersion {
                    executable: std::env::current_exe().unwrap_or_default(),
                    build_id: tickloop_code_access::build_identity()
                        .unwrap_or_default()
                        .into(),
                    requested_executable: None,
                    upgrading: false,
                    error: None,
                    workers: Vec::new(),
                })),
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: self.inner.sequence.load(Ordering::SeqCst),
                },
                None,
            )),
            ObservationRequest::DraftContextBreakdown { launch, draft } => Ok(Observation::single(
                ObservationValue::ContextBreakdown(Box::new(
                    self.inner
                        .agent
                        .draft_context_breakdown(Some(launch.folder), *draft)
                        .await
                        .map_err(agent_access_error)?,
                )),
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: 0,
                },
                None,
            )),
            ObservationRequest::DraftMcpServer { launch, name } => Ok(Observation::single(
                ObservationValue::McpServers(vec![
                    crate::draft_mcp::connect(&launch, &name).await?,
                ]),
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: 0,
                },
                None,
            )),
            ObservationRequest::EndedAgents { .. } => Ok(Observation::single(
                ObservationValue::EndedAgents(Vec::new()),
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: 0,
                },
                None,
            )),
            ObservationRequest::OtherTickhost => Ok(Observation::single(
                ObservationValue::OtherTickhost(None),
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: 0,
                },
                None,
            )),
            ObservationRequest::Overview {
                filter,
                page,
                after,
            } => {
                let mut receiver = self.inner.changes.subscribe();
                let cursor = Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: self.inner.sequence.load(Ordering::SeqCst),
                };
                let references = self.inner.references.lock().await.clone();
                let views = self.inner.views.displayed().await;
                let mut entries = Vec::new();
                for (chat, execution) in references {
                    let summary = self
                        .inner
                        .agent
                        .summary(chat.into())
                        .await
                        .map_err(agent_access_error)?;
                    if filter
                        .folder
                        .as_ref()
                        .is_some_and(|path| Some(path) != summary.folder.as_ref())
                    {
                        continue;
                    }
                    let state = tickloop_harness::runtime::summary_state(
                        &summary.chat_loop,
                        &summary.user_shells,
                    );
                    let launch = self.inner.launches.lock().await.get(&execution).cloned();
                    let workspace = launch
                        .as_ref()
                        .and_then(|launch| launch.host_work_context.as_ref())
                        .and_then(|context| context.workspace.clone());
                    if filter
                        .workspace
                        .as_ref()
                        .is_some_and(|selected| Some(selected) != workspace.as_ref())
                    {
                        continue;
                    }
                    let record = launch
                        .as_ref()
                        .and_then(|launch| launch.config_root.clone())
                        .filter(|_| self.inner.agent.saves_chats())
                        .map(|store_root| TranscriptRef {
                            store_root,
                            chat_id: chat,
                        });
                    let registration = self
                        .inner
                        .agent
                        .subagent_registration(chat.into())
                        .await
                        .map_err(agent_access_error)?;
                    entries.push(AgentSummary {
                        availability: ExecutionAvailability::Live,
                        execution,
                        chat_id: chat,
                        preview: summary.preview,
                        updated_at: super::summary::updated_at(
                            &summary.chat_loop,
                            &summary.user_shells,
                        ),
                        folder: summary.folder,
                        run_id: super::summary::summary_run_id(&summary.chat_loop, &state),
                        run_started_at: super::summary::run_started_at(&summary.chat_loop, &state),
                        state,
                        title: summary.title,
                        subagent: registration,
                        has_work: Some(summary.has_work),
                        pending_work: Some(summary.pending_work),
                        waiting_permissions: summary.waiting_permissions,
                        waiting_questions: summary.waiting_questions,
                        parent: match summary.parent_chat {
                            Some(parent) => self
                                .inner
                                .references
                                .lock()
                                .await
                                .get(&parent.uuid())
                                .copied(),
                            None => None,
                        },
                        record,
                        workspace,
                        capabilities: if summary.parent_chat.is_some() {
                            vec![
                                "submit".into(),
                                "scoped_shell".into(),
                                "model_config".into(),
                                "loop".into(),
                                "observe".into(),
                                "respond".into(),
                            ]
                        } else {
                            vec![
                                "submit".into(),
                                "shell".into(),
                                "configure".into(),
                                "history".into(),
                                "loop".into(),
                                "delegate".into(),
                            ]
                        },
                        views: views
                            .iter()
                            .filter(|view| view.execution == execution)
                            .cloned()
                            .collect(),
                    });
                }
                entries.sort_by_key(|entry| entry.execution);
                let limit = page.bounded_limit();
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
                let cancel = self.inner.cancel.clone();
                let inner = Arc::downgrade(&self.inner);
                tokio::spawn(async move {
                    loop {
                        let update = tokio::select! { _ = sender.closed() => break, _ = cancel.cancelled() => break, update = receiver.recv() => update };
                        let sequence = match update {
                            Ok(update) if update.cursor.sequence > cursor.sequence => {
                                update.cursor.sequence
                            }
                            Ok(_) => continue,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let Some(inner) = inner.upgrade() else {
                                    break;
                                };
                                inner.sequence.load(Ordering::SeqCst)
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        };
                        if sender
                            .send(ObservationUpdate {
                                cursor: Cursor { sequence, ..cursor },
                                change: ObservationChange::Changed,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                Ok(Observation::new(
                    ObservationValue::Overview(
                        entries.into_iter().skip(page.offset).take(limit).collect(),
                    ),
                    cursor,
                    reset,
                    Some(changes),
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
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: self.inner.sequence.load(Ordering::SeqCst),
                },
                None,
            )),
            ObservationRequest::ManagedSessions { launch, cursor } => Ok(Observation::single(
                ObservationValue::ManagedSessions(
                    crate::records::managed_sessions(*launch, cursor).await?,
                ),
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: 0,
                },
                None,
            )),
            ObservationRequest::SavedRecords {
                store_root,
                folder,
                page,
            } => {
                if let Some(root) = store_root {
                    self.inner.agent.flush_transcripts().await;
                    return Ok(Observation::single(
                        crate::records::saved_records(root, folder, page).await?,
                        Cursor {
                            daemon_epoch: self.inner.epoch,
                            execution: None,
                            sequence: 0,
                        },
                        None,
                    ));
                }
                let records = self
                    .inner
                    .agent
                    .saved_chats(&folder)
                    .await
                    .map_err(agent_access_error)?;
                let limit = page.bounded_limit();
                Ok(Observation::single(
                    ObservationValue::Records(
                        records.into_iter().skip(page.offset).take(limit).collect(),
                    ),
                    Cursor {
                        daemon_epoch: self.inner.epoch,
                        execution: None,
                        sequence: 0,
                    },
                    None,
                ))
            }
            ObservationRequest::Transcript { record, page } => Ok(Observation::single(
                crate::records::transcript(record, page).await?,
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: 0,
                },
                None,
            )),
        }
    }

    async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let _admission = self.inner.admission.read().await;
        if self.inner.upgrade_paused.load(Ordering::SeqCst) {
            return Err(AccessError::unavailable("Worker is transferring execution"));
        }
        if matches!(&envelope.command, AgentCommand::Create { launch, .. } if launch.profile != tickloop_code_access::ExecutionProfile::Local)
            || matches!(&envelope.command, AgentCommand::LoadRecord { launch: Some(launch), .. } if launch.profile != tickloop_code_access::ExecutionProfile::Local)
        {
            return Err(AccessError::invalid(
                "managed profiles require CreateManaged, never history loading",
            ));
        }
        match &envelope.command {
            AgentCommand::Create { launch, .. }
            | AgentCommand::LoadRecord {
                launch: Some(launch),
                ..
            } => {
                self.inner
                    .agent
                    .validate_tool_policy(
                        crate::execution_tool_policy(launch.profile),
                        launch.host_options,
                    )
                    .map_err(agent_access_error)?;
            }
            AgentCommand::LoadRecord { launch: None, .. } => {
                self.inner
                    .agent
                    .validate_tool_policy(
                        tickloop_harness::runtime::ToolPolicy::local(),
                        tickloop_harness::protocol::AgentHostOptions::embedded_defaults(),
                    )
                    .map_err(agent_access_error)?;
            }
            _ => {}
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
        let managed = self
            .inner
            .managed
            .authorize_worker(&envelope.command)
            .await?;
        let _gate = if matches!(&envelope.command, AgentCommand::ManagedHeartbeat { .. }) {
            None
        } else if let Some(entry) = &managed {
            Some(entry.gate.clone().lock_owned().await)
        } else {
            None
        };
        if managed.is_some() {
            self.inner
                .managed
                .authorize_worker(&envelope.command)
                .await?;
        }
        if let Some(receipt) = self.inner.receipts.begin(&envelope).await? {
            return Ok(receipt);
        }
        let result = self.dispatch(&envelope).await;
        let persistence = if let Some(error) = self.inner.agent.transcript_write_error() {
            PersistenceState::WriteFailed(error)
        } else if matches!(
            &envelope.command,
            AgentCommand::FlushTranscripts
                | AgentCommand::ManagedConfigure { .. }
                | AgentCommand::ManagedLoad { .. }
        ) || matches!(&envelope.command, AgentCommand::CreateManaged { launch, .. } if launch.profile == tickloop_code_access::ExecutionProfile::Acp && launch.session_persistence)
        {
            PersistenceState::FlushObserved
        } else if self.inner.agent.saves_chats() {
            PersistenceState::BestEffortPending
        } else {
            PersistenceState::NotApplicable
        };
        let execution = match &result {
            Ok(
                CommandResult::Created { execution, .. }
                | CommandResult::Loaded { execution, .. }
                | CommandResult::Branch { execution, .. },
            ) => Some(*execution),
            _ => envelope.command.execution(),
        };
        let receipt = self
            .inner
            .receipts
            .finish(envelope.command_id, result, persistence)
            .await;
        self.publish(execution, ObservationChange::Changed).await;
        Ok(receipt)
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        if let DisplayUpdate::Displayed { execution, .. } = &update {
            if self.inner.managed.contains(*execution).await {
                return Err(AccessError::new(
                    AccessErrorCode::AccessDenied,
                    "managed executions cannot be attached to a Console",
                ));
            }
            self.chat(*execution).await?;
        }
        let receipt = self.inner.views.update(update).await?;
        self.publish(None, ObservationChange::Changed).await;
        Ok(receipt)
    }
}

async fn watch_runtime(
    inner: Weak<Inner>,
    mut events: broadcast::Receiver<Event>,
    mut shells: broadcast::Receiver<UserShellRecord>,
    mut tasks: broadcast::Receiver<TaskNotification>,
    cancel: CancellationToken,
) {
    loop {
        let (chat, change) = tokio::select! {
            _ = cancel.cancelled() => break,
            event = events.recv() => match event { Ok(event) => (event.chat_id, ObservationChange::Event(event)), Err(broadcast::error::RecvError::Lagged(_)) => { notify_all(&inner).await; continue; }, Err(_) => break },
            shell = shells.recv() => match shell { Ok(shell) => (shell.chat_id, ObservationChange::UserShell(shell)), Err(broadcast::error::RecvError::Lagged(_)) => { notify_all(&inner).await; continue; }, Err(_) => break },
            task = tasks.recv() => match task { Ok(task) => (task.chat_id, ObservationChange::TaskNotification(task)), Err(broadcast::error::RecvError::Lagged(_)) => { notify_all(&inner).await; continue; }, Err(_) => break },
        };
        let Some(inner) = inner.upgrade() else {
            break;
        };
        let access = EmbeddedAgentAccess { inner };
        let _admission = access.inner.admission.read().await;
        if access.inner.upgrade_paused.load(Ordering::SeqCst) {
            continue;
        }
        let mut execution = access
            .inner
            .references
            .lock()
            .await
            .get(&chat.uuid())
            .copied();
        if execution.is_none()
            && let Ok(snapshot) = access.inner.agent.snapshot(chat).await
            && let Some(delegation) = snapshot.delegation
        {
            let parent = access
                .inner
                .references
                .lock()
                .await
                .get(&(delegation.root_chat_id()).uuid())
                .copied();
            if let Some(parent) = parent {
                let launch = access.inner.launches.lock().await.get(&parent).cloned();
                if let Some(launch) = launch {
                    for ancestor in delegation.lineage() {
                        access
                            .register(ancestor.chat_id.into(), launch.clone())
                            .await;
                    }
                    execution = Some(access.register(chat.into(), launch).await);
                }
            }
        }
        if let Some(execution) = execution {
            access.publish(Some(execution), change).await;
        }
    }
}

async fn notify_all(inner: &Weak<Inner>) {
    if let Some(inner) = inner.upgrade() {
        let executions = inner
            .executions
            .lock()
            .await
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let access = EmbeddedAgentAccess { inner };
        for execution in executions {
            if let Ok(snapshot) = access.snapshot(execution).await {
                access
                    .publish(
                        Some(execution),
                        ObservationChange::Reset {
                            reason: ResetReason::Lagged,
                            snapshot: Box::new(snapshot),
                        },
                    )
                    .await;
            }
        }
    }
}

async fn drive_queue(inner: Weak<Inner>, cancel: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! { _ = cancel.cancelled() => break, _ = interval.tick() => {} }
        let Some(inner) = inner.upgrade() else {
            break;
        };
        let access = EmbeddedAgentAccess { inner };
        let executions = access.inner.executions.lock().await.clone();
        for (execution, chat) in executions {
            let admission = access.inner.admission.clone().read_owned().await;
            if access.inner.upgrade_paused.load(Ordering::SeqCst) {
                continue;
            }
            let Ok(Some(command)) = access
                .inner
                .agent
                .take_next_queued_command(chat.into())
                .await
            else {
                continue;
            };
            let queued = access.inner.queue.lock().await.remove(&command.id);
            if let Some((_, action)) = queued {
                let access = access.clone();
                tokio::spawn(async move {
                    let _admission = admission;
                    let label = action_label(&action);
                    let result = match action {
                        AfterTurnAction::Submit { input, origin } => {
                            match access.host_context(execution, chat, origin, false).await {
                                Ok(context) => {
                                    access.submit_in_context(execution, input, context).await
                                }
                                Err(error) => Err(error),
                            }
                        }
                        AfterTurnAction::Configure { change } => {
                            access.configure(execution, change).await
                        }
                        AfterTurnAction::History { action } => {
                            access.history(execution, action, command.id).await
                        }
                        AfterTurnAction::Loop { action } => {
                            access.manage_loop(execution, action).await
                        }
                    };
                    if let Err(error) = result {
                        access
                            .publish(Some(execution), ObservationChange::Changed)
                            .await;
                        let mut failures = access.inner.queue_failures.lock().await;
                        failures.push_back(QueuedCommandFailure {
                            command_id: command.id,
                            execution,
                            label,
                            error,
                        });
                        while failures.len() > FEED_CAPACITY {
                            failures.pop_front();
                        }
                    }
                    let _ = access
                        .inner
                        .agent
                        .finish_queued_command(chat.into(), command.id)
                        .await;
                    access
                        .publish(Some(execution), ObservationChange::Changed)
                        .await;
                });
            }
        }
    }
}

fn action_label(action: &AfterTurnAction) -> String {
    match action {
        AfterTurnAction::Submit {
            input: UserInput::Skill { name, arguments },
            ..
        } => format!("/{name} {arguments}"),
        AfterTurnAction::History {
            action: HistoryAction::Compact { instructions },
        } => format!("/compact {}", instructions.as_deref().unwrap_or_default()),
        AfterTurnAction::Configure {
            change: ConfigurationChange::Directories { folder, .. },
        } => format!("/cwd {}", folder.display()),
        AfterTurnAction::Configure { .. } => "/configure".into(),
        AfterTurnAction::History { .. } => "/history".into(),
        AfterTurnAction::Loop { .. } => "/loop".into(),
        AfterTurnAction::Submit { .. } => "/submit".into(),
    }
}

/// The Folder of a Chat a local launch shows; this daemon runs only Chats with one.
fn chat_folder(
    snapshot: &tickloop_harness::protocol::ChatSnapshot,
) -> Result<std::path::PathBuf, AccessError> {
    snapshot.folder.clone().ok_or_else(|| {
        AccessError::new(
            AccessErrorCode::Unsupported,
            "this Chat has no Folder to launch in",
        )
    })
}

/// A hosted Tick lifecycle that may have reached the host is unknown, not refused.
fn hosted_access_error(error: tickhost_connect::host_error::HostError) -> AccessError {
    let code = if error.dispatched {
        AccessErrorCode::OutcomeUnknown
    } else {
        AccessErrorCode::Unavailable
    };
    AccessError::new(code, error.to_string())
}
