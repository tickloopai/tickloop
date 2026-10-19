use std::{
    collections::{HashMap, VecDeque},
    path::Path,
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
use tickloop_agent_model::{
    ModelClient, ModelError, cards::model_for_persistent_selection, model_client,
};
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AfterTurnAction, AgentAccess, AgentCommand, AgentDetailQuery,
    AgentSummary, BranchRequest, CommandEnvelope, CommandReceipt, CommandResult,
    ConfigurationChange, Cursor, DirectedHumanSource, DisplayReceipt, DisplayUpdate, Event,
    ExecutionAvailability, ExecutionRef, FEED_CAPACITY, HerdrInteraction, HistoryAction,
    HostCallContext, InteractionSource, LaunchSpec, LiveChatSnapshot, LiveChatStatus, LoopAction,
    LoopTriggerNotice, ModelConfig, Observation, ObservationChange, ObservationRequest,
    ObservationUpdate, ObservationValue, PendingRequestKind, PersistenceState,
    QueuedCommandFailure, REPLAY_CAPACITY, RequestRef, ResetReason, ShellTaskAction,
    ShellTaskOrigin, SubagentRegistration, TaskAction, TaskNotification, TaskOrigin, TaskRef,
    TranscriptRef, UserInput, UserShellRecord, bounded_update,
};
use tickloop_agent_runtime::{Agent, ShellTaskCenter};

/// Builds the transport an Agent switches to from the chosen model and the Agent's configuration
/// root.
pub type ModelClientFactory = Arc<
    dyn Fn(ModelConfig, Option<&Path>) -> Result<Arc<dyn ModelClient>, ModelError> + Send + Sync,
>;

#[derive(Clone)]
pub struct EmbeddedAgentAccess {
    inner: Arc<Inner>,
}

type QueuedAction = (ExecutionRef, Option<Uuid>, AfterTurnAction);

struct Inner {
    agent: Agent,
    epoch: Uuid,
    model_clients: ModelClientFactory,
    executions: Mutex<HashMap<ExecutionRef, Uuid>>,
    references: Mutex<HashMap<Uuid, ExecutionRef>>,
    launches: Mutex<HashMap<ExecutionRef, LaunchSpec>>,
    account_models: tickloop_agent_model::availability::AccountModelDiscovery,
    receipts: ReceiptStore,
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
    pub(crate) runtime: tickloop_agent_runtime::upgrade::AgentUpgradeCheckpoint,
    executions: Vec<(ExecutionRef, Uuid)>,
    launches: Vec<(ExecutionRef, LaunchSpec)>,
    receipts: super::receipts::ReceiptCheckpoint,
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
        Self::with_epoch(agent, Uuid::new_v4())
    }

    pub fn with_epoch(agent: Agent, epoch: Uuid) -> Self {
        Self::build(agent, epoch, Arc::new(model_client))
    }

    fn build(agent: Agent, epoch: Uuid, model_clients: ModelClientFactory) -> Self {
        let events = agent.subscribe();
        let shells = agent.subscribe_user_shells();
        let tasks = agent.subscribe_task_notifications();
        let inner = Arc::new(Inner {
            agent,
            epoch,
            model_clients,
            executions: Mutex::new(HashMap::new()),
            references: Mutex::new(HashMap::new()),
            launches: Mutex::new(HashMap::new()),
            account_models: Default::default(),
            receipts: ReceiptStore::new(epoch),
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
        Self::for_worker_with_model_clients(agent, epoch, Arc::new(model_client))
    }

    pub fn for_worker_with_model_clients(
        agent: Agent,
        epoch: Uuid,
        model_clients: ModelClientFactory,
    ) -> Self {
        let access = Self::build(agent, epoch, model_clients);
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
            if self.inner.references.lock().await.contains_key(&chat) {
                continue;
            }
            if let Some(root) = root {
                let parent = self.inner.references.lock().await.get(&root).copied();
                if let Some(parent) = parent {
                    let launch = self.inner.launches.lock().await.get(&parent).cloned();
                    if let Some(launch) = launch {
                        self.register(chat, launch).await;
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
            agent_id: Uuid::new_v4(),
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
    ) -> Result<Arc<tickloop_agent_runtime::hosted::tools::HostedTickTools>, AccessError> {
        self.inner.agent.hosted_ticks().ok_or_else(|| {
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
        let snapshot = self.inner.agent.snapshot(chat).await?;
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
        let status = self.inner.agent.status(chat).await?;
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
        let mut warnings = self.inner.agent.user_settings_warnings(chat);
        warnings.extend(self.inner.agent.transcript_load_warnings(chat));
        warnings.extend(transcript_write_error.clone());
        warnings.extend(
            queue_failures.iter().map(|failure| {
                format!("Queued command {} failed: {}", failure.label, failure.error)
            }),
        );
        Ok(LiveFacts {
            subagent: self.inner.agent.subagent_registration(chat).await?,
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
        let snapshot = self.inner.agent.snapshot(chat).await?;
        let mut result = if let Some(delegation) = &snapshot.delegation {
            delegation
                .ceiling
                .host_context
                .clone()
                .map(|mut inherited| {
                    inherited.workdir = snapshot.workdir.clone();
                    inherited.interaction = None;
                    inherited
                })
        } else {
            context.map(|context| HostCallContext {
                workdir: snapshot.workdir.clone(),
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
                let pane = tickloop_agent_protocol::HerdrPane {
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
                    workdir: snapshot.workdir,
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
        target_loop: Option<Uuid>,
        input: UserInput,
        origin: Option<InteractionSource>,
    ) -> Result<CommandResult, AccessError> {
        let chat = self.chat(execution).await?;
        let context = self.host_context(execution, chat, origin, true).await?;
        self.submit_in_context(execution, target_loop, input, context)
            .await
    }

    async fn submit_in_context(
        &self,
        execution: ExecutionRef,
        target_loop: Option<Uuid>,
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
                        chat,
                        target_loop,
                        text,
                        contexts,
                        paths,
                        context,
                    )
                    .await?,
            )),
            UserInput::Skill { name, arguments } => Ok(CommandResult::Submitted(
                self.inner
                    .agent
                    .invoke_skill_in_host_context(chat, target_loop, &name, &arguments, context)
                    .await?,
            )),
            UserInput::Shell {
                command,
                background,
            } => Ok(CommandResult::ShellSubmitted(
                self.inner
                    .agent
                    .submit_user_shell_for_goal(chat, target_loop, command, background)
                    .await?,
            )),
        }
    }

    async fn dispatch(&self, envelope: &CommandEnvelope) -> Result<CommandResult, AccessError> {
        match envelope.command.clone() {
            AgentCommand::AddArtifactTick { execution, tick } => {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::ArtifactTicks(
                    self.inner.agent.add_artifact_tick(chat, tick).await?,
                ))
            }
            AgentCommand::RemoveArtifactTick {
                execution,
                instance_id,
            } => {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::ArtifactTicks(
                    self.inner
                        .agent
                        .remove_artifact_tick(chat, &instance_id)
                        .await?,
                ))
            }
            AgentCommand::LinkArtifact {
                execution,
                tick,
                history,
            } => {
                let chat = self.chat(execution).await?;
                self.inner.agent.link_artifact(chat, &tick, history).await?;
                Ok(CommandResult::Done)
            }
            AgentCommand::ListArtifactTicks { execution } => {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::ArtifactTicks(
                    self.inner.agent.artifact_ticks(chat).await?,
                ))
            }
            AgentCommand::ListHostedTicks { execution } => {
                let chat = self.chat(execution).await?;
                let members = self
                    .hosted_ticks()?
                    .members(chat)
                    .await
                    .map_err(|error| AccessError::unavailable(error.to_string()))?;
                Ok(CommandResult::HostedTicks(members))
            }
            AgentCommand::CloseHostedTick {
                execution,
                instance,
            } => {
                let chat = self.chat(execution).await?;
                self.hosted_ticks()?
                    .close_for_user(chat, &instance)
                    .await
                    .map_err(hosted_access_error)?;
                Ok(CommandResult::Done)
            }
            AgentCommand::ReopenHostedTick {
                execution,
                instance,
                restore_subscriptions,
            } => {
                let chat = self.chat(execution).await?;
                let reopened = self
                    .hosted_ticks()?
                    .reopen_for_user(chat, &instance)
                    .await
                    .map_err(hosted_access_error)?;
                let membership = self
                    .inner
                    .agent
                    .record_reopened_tick(chat, &instance, reopened.clone())
                    .await?;
                let restore_error = self
                    .inner
                    .agent
                    .replace_tick_subscriptions(chat, &instance, &reopened, restore_subscriptions)
                    .await
                    .err()
                    .map(|error| error.to_string());
                Ok(CommandResult::HostedTickReopened {
                    instance: reopened,
                    membership,
                    restore_error,
                })
            }
            AgentCommand::AttachHostedTick {
                execution,
                instance,
            } => {
                let chat = self.chat(execution).await?;
                let grant = if instance.access() == tick_protocol::SHELL_ACCESS {
                    self.inner
                        .agent
                        .shell_tasks()
                        .allow_view(chat, &instance)
                        .await
                } else {
                    self.hosted_ticks()?.allow_view(chat, &instance).await
                };
                Ok(CommandResult::HostedTickView(
                    grant.map_err(hosted_access_error)?,
                ))
            }
            AgentCommand::RemoveMonitorMember {
                execution,
                instance,
            } => {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::MonitorMembership(
                    self.inner
                        .agent
                        .remove_monitor_member(chat, instance)
                        .await?,
                ))
            }
            AgentCommand::MarkMonitorHandled {
                execution,
                instance,
            } => {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::MonitorMembership(
                    self.inner
                        .agent
                        .mark_monitor_handled(chat, instance)
                        .await?,
                ))
            }
            AgentCommand::Create {
                launch,
                initial,
                origin,
            } => {
                if let Some(UserInput::Skill { name, arguments }) = &initial {
                    self.inner
                        .agent
                        .check_user_skill(&launch.workdir, name, arguments)?;
                }
                let chat = if let Some(chat) = launch.exact_chat {
                    self.inner
                        .agent
                        .restore_empty_chat(chat, launch.workdir.clone())
                        .await?;
                    chat
                } else {
                    self.inner.agent.create_chat(launch.workdir.clone()).await?
                };
                let execution = self.register(chat, launch.clone()).await;
                self.inner
                    .receipts
                    .set_target(envelope.command_id, execution)
                    .await;
                self.inner
                    .agent
                    .set_host_work_context(
                        chat,
                        HostCallContext {
                            workdir: launch.workdir,
                            workspace_id: launch
                                .host_work_context
                                .and_then(|context| context.workspace),
                            interaction: None,
                        },
                    )
                    .await?;
                // Every Agent starts on its launch model, as the worker started for that launch
                // does; one in-process runtime otherwise gives each Agent the runtime's own model.
                if let Some(model) = launch
                    .model
                    .and_then(|model| self.builtin_config(model).ok())
                    && model_for_persistent_selection(model.clone())
                        != self.inner.agent.snapshot(chat).await?.model
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
                    Some(input) => Some(
                        self.submit(execution, None, input, origin)
                            .await
                            .map_err(|mut error| {
                                error.message = format!(
                                    "Agent {} was created, but its first input was not submitted: {}",
                                    execution.agent_id, error.message
                                );
                                error
                            })?,
                    ),
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
            }
            AgentCommand::LoadRecord {
                record,
                as_new,
                launch,
            } => {
                let snapshot = if as_new {
                    self.inner.agent.resume_as_new_chat(record.chat_id).await?
                } else {
                    self.inner.agent.resume_chat(record.chat_id).await?
                };
                let launch =
                    launch.unwrap_or_else(|| LaunchSpec::embedded(snapshot.workdir.clone()));
                if snapshot.delegation.is_none() {
                    self.inner
                        .agent
                        .set_host_work_context(
                            snapshot.id,
                            HostCallContext {
                                workdir: snapshot.workdir.clone(),
                                workspace_id: launch
                                    .host_work_context
                                    .as_ref()
                                    .and_then(|context| context.workspace.clone()),
                                interaction: None,
                            },
                        )
                        .await?;
                }
                if let Some(delegation) = &snapshot.delegation {
                    for ancestor in delegation.lineage() {
                        self.register(ancestor.chat_id, launch.clone()).await;
                    }
                }
                let execution = self.register(snapshot.id, launch).await;
                Ok(CommandResult::Loaded {
                    execution,
                    snapshot: Box::new(self.snapshot(execution).await?),
                })
            }
            AgentCommand::SendFamilyInput {
                source_execution,
                target_execution,
                target_loop,
                origin,
                input,
            } => {
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
                let message_id = Uuid::new_v4();
                let source_handle = self
                    .inner
                    .agent
                    .subagent_registration(source_chat)
                    .await?
                    .map(|record| record.handle)
                    .unwrap_or_else(|| "main".into());
                let target_handle = self
                    .inner
                    .agent
                    .subagent_registration(target_chat)
                    .await?
                    .map(|record| record.handle)
                    .unwrap_or_else(|| "main".into());
                let directed = DirectedHumanSource {
                    message_id,
                    source_chat_id: source_chat,
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
                    .submit_family_input(
                        target_chat,
                        target_loop,
                        directed,
                        contexts,
                        paths,
                        context,
                    )
                    .await?;
                Ok(CommandResult::FamilyInputSent {
                    source_execution,
                    target_execution,
                    receipt,
                    message_id,
                    target_handle,
                })
            }
            AgentCommand::Submit {
                execution,
                target_loop,
                input,
                origin,
            } => Box::pin(self.submit(execution, target_loop, input, origin)).await,
            AgentCommand::ContinueLoop {
                execution,
                loop_id,
                input,
                origin,
            } => Box::pin(self.submit(execution, Some(loop_id), input, origin)).await,
            AgentCommand::DecidePermission {
                request,
                allow,
                scope,
                comment,
                rule,
            } => {
                self.claim_answer(request, PendingRequestKind::Permission, envelope.command_id)
                    .await?;
                let chat = self.chat(request.execution).await?;
                let result = if let Some(rule) = rule {
                    self.inner
                        .agent
                        .answer_permission_with_rule(chat, request.request_id, rule)
                        .await
                } else {
                    self.inner
                        .agent
                        .answer_permission_with_comment(
                            chat,
                            request.request_id,
                            allow,
                            scope,
                            comment,
                        )
                        .await
                };
                if let Err(error) = result {
                    self.release_answer(request).await;
                    return Err(error.into());
                }
                Ok(CommandResult::Done)
            }
            AgentCommand::AnswerQuestion { request, answers } => {
                self.claim_answer(request, PendingRequestKind::Question, envelope.command_id)
                    .await?;
                let chat = self.chat(request.execution).await?;
                if let Err(error) = self
                    .inner
                    .agent
                    .answer_question(chat, request.request_id, answers)
                    .await
                {
                    self.release_answer(request).await;
                    return Err(error.into());
                }
                Ok(CommandResult::Done)
            }
            AgentCommand::DeclineQuestion { request } => {
                self.claim_answer(request, PendingRequestKind::Question, envelope.command_id)
                    .await?;
                let chat = self.chat(request.execution).await?;
                if let Err(error) = self
                    .inner
                    .agent
                    .decline_question(chat, request.request_id)
                    .await
                {
                    self.release_answer(request).await;
                    return Err(error.into());
                }
                Ok(CommandResult::Done)
            }
            AgentCommand::Configure {
                execution,
                change,
                provider,
            } => {
                if provider.is_some() {
                    return Err(AccessError::invalid(
                        "External Provider definitions are not supported; choose a built-in model",
                    ));
                }
                Box::pin(self.configure(execution, change)).await
            }
            AgentCommand::EditHistory { execution, action } => {
                self.history(execution, action, envelope.command_id).await
            }
            AgentCommand::ManageLoop { execution, action } => {
                self.manage_loop(execution, action).await
            }
            AgentCommand::ManageTask { execution, action } => {
                self.manage_task(execution, action).await
            }
            AgentCommand::ClearCompletedTasks {
                execution,
                root_loop_id,
                expected_revision,
            } => {
                let chat = self.chat(execution).await?;
                self.inner
                    .agent
                    .clear_completed_tasks(chat, root_loop_id, expected_revision)
                    .await?;
                Ok(CommandResult::Done)
            }
            AgentCommand::EnqueueAfterTurn {
                execution,
                target_loop,
                action,
            } => {
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
                    .queue_command(chat, target_loop, label)
                    .await?;
                self.inner
                    .queue
                    .lock()
                    .await
                    .insert(command_id, (execution, target_loop, action));
                Ok(CommandResult::Queued { command_id })
            }
            AgentCommand::RefreshConfiguration { execution } => {
                let snapshot = self.snapshot(execution).await?;
                self.configure(
                    execution,
                    ConfigurationChange::Model {
                        config: snapshot.model,
                        thinking: None,
                    },
                )
                .await
            }
            AgentCommand::TrustWorkingDirectory { workdir } => {
                self.inner.agent.trust_working_directory(workdir).await?;
                Ok(CommandResult::Done)
            }
            AgentCommand::FlushTranscripts => {
                self.inner.agent.flush_transcripts().await;
                Ok(CommandResult::Done)
            }
            AgentCommand::UpgradeRuntime { .. } => Err(AccessError::new(
                AccessErrorCode::Unsupported,
                "runtime upgrades are coordinated by the daemon",
            )),
            AgentCommand::ReplaceTickhost => Err(AccessError::invalid(
                "only the Agent daemon replaces its Tickhost",
            )),
        }
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
            ConfigurationChange::TaskToolsEnabled { enabled } => {
                agent.set_task_tools_enabled(chat, enabled).await?
            }
            ConfigurationChange::Model { config, thinking } => {
                let config = self.builtin_config(config)?;
                let root = self
                    .inner
                    .launches
                    .lock()
                    .await
                    .get(&execution)
                    .and_then(|launch| launch.config_root.clone());
                let client = (self.inner.model_clients)(config.clone(), root.as_deref())
                    .map_err(|error| AccessError::invalid(error.to_string()))?;
                let change = match thinking {
                    Some(effort) => {
                        agent
                            .set_model_with_thinking(chat, config, client, effort)
                            .await?
                    }
                    None => agent.set_model(chat, config, client).await?,
                };
                return Ok(CommandResult::ModelChanged(change));
            }
            ConfigurationChange::ReasoningEffort { effort } => {
                agent.set_reasoning_effort(chat, effort).await?
            }
            ConfigurationChange::ThinkingEnabled { enabled } => {
                agent.set_thinking_enabled(chat, enabled).await?
            }
            ConfigurationChange::ExecutionMode { mode } => {
                agent.set_execution_mode(chat, mode).await?
            }
            ConfigurationChange::ContextWindow { tokens } => {
                if self.inner.builtin_catalog_only.load(Ordering::SeqCst) {
                    return Err(AccessError::invalid(
                        "Context window is version-owned by the built-in catalog and cannot be overridden",
                    ));
                }
                agent.set_context_window(chat, tokens).await?
            }
            ConfigurationChange::Search { config } => agent.set_search(chat, config).await?,
            ConfigurationChange::Directories {
                workdir,
                additional,
            } => agent.set_directories(chat, workdir, additional).await?,
            ConfigurationChange::ReplaceRules { rules } => {
                agent.set_permission_rules(chat, rules).await?
            }
            ConfigurationChange::ProjectConstraints { rules } => {
                agent
                    .set_project_permission_constraints(chat, rules)
                    .await?
            }
            ConfigurationChange::ManagedConstraints { rules } => {
                agent
                    .set_managed_permission_constraints(chat, rules)
                    .await?
            }
            ConfigurationChange::GenerateAutoEnvironment { options } => {
                // Keep setup and its model retries out of every command-dispatch stack frame.
                return Ok(CommandResult::AutoEnvironmentDraft(
                    Box::pin(agent.generate_auto_environment(chat, options)).await?,
                ));
            }
            ConfigurationChange::PermissionChange { change, expected } => {
                return match agent.apply_permission_change(chat, change, expected).await {
                    Ok(view) => Ok(CommandResult::Permissions(Box::new(view))),
                    Err(tickloop_agent_runtime::PermissionChangeError::Conflict { latest }) => {
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
        let agent = &self.inner.agent;
        match action {
            HistoryAction::Branch { target_loop, name } => {
                let result = agent
                    .branch_chat(BranchRequest {
                        request_id: command_id,
                        source_chat_id: chat,
                        source_loop_id: target_loop,
                        name,
                    })
                    .await?;
                let launch = self
                    .inner
                    .launches
                    .lock()
                    .await
                    .get(&execution)
                    .cloned()
                    .unwrap_or_else(|| LaunchSpec::embedded(result.snapshot.workdir.clone()));
                let execution = self.register(result.child_chat_id, launch).await;
                Ok(CommandResult::Branch {
                    execution,
                    source_chat_id: result.source_chat_id,
                    child_chat_id: result.child_chat_id,
                    name: result.name,
                    snapshot: Box::new(self.snapshot(execution).await?),
                    session_permissions: result.session_permissions,
                })
            }
            HistoryAction::Compact { instructions } => {
                let result = agent.compact_with_instructions(chat, instructions).await?;
                Ok(CommandResult::Compact {
                    summary: result.summary,
                    snapshot: Box::new(self.snapshot(execution).await?),
                })
            }
            HistoryAction::DirectedSummary { request } => {
                let result = agent.summarize_checkpoint(chat, request).await?;
                Ok(CommandResult::DirectedSummary {
                    snapshot: Box::new(self.snapshot(execution).await?),
                    direction: result.direction,
                    summary: result.summary,
                    guidance: result.guidance,
                    messages_summarized: result.messages_summarized,
                    draft: result.draft,
                    continuation_error: result.continuation_error,
                })
            }
            HistoryAction::Rewind { message_id, mode } => {
                let result = agent.rewind(chat, message_id, mode).await?;
                Ok(CommandResult::Rewind {
                    snapshot: Box::new(self.snapshot(execution).await?),
                    mode: result.mode,
                    code: result.code,
                    conversation_restored: result.conversation_restored,
                    draft: result.draft,
                })
            }
            HistoryAction::TakeBackInterruptedPrompt { run_id } => {
                match agent.take_back_interrupted_prompt(chat, run_id).await? {
                    Some(draft) => Ok(CommandResult::PromptTakenBack {
                        snapshot: Box::new(self.snapshot(execution).await?),
                        draft,
                    }),
                    None => Ok(CommandResult::Done),
                }
            }
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
            LoopAction::Stop { loop_id } => {
                Ok(CommandResult::Operations(agent.stop(chat, loop_id).await?))
            }
            LoopAction::Interrupt => Ok(CommandResult::Operations(agent.interrupt(chat).await?)),
            LoopAction::SendNow { run_id } => Ok(CommandResult::Operations(
                agent.send_now(chat, run_id).await?,
            )),
            LoopAction::DisableEventSources { loop_id } => {
                agent.disable_event_sources_for_loop(chat, loop_id).await;
                Ok(CommandResult::Done)
            }
            LoopAction::ConfigureTrigger { loop_id, trigger } => {
                agent.configure_trigger(chat, loop_id, trigger).await?;
                Ok(CommandResult::Done)
            }
            LoopAction::EmitTrigger { loop_id, source } => Ok(CommandResult::Submitted(
                agent.trigger(chat, loop_id, source).await?,
            )),
            LoopAction::ApprovePlan {
                loop_id,
                version,
                mode,
            } => match agent.approve_plan(chat, loop_id, version, mode).await? {
                Some(continuation) => Ok(CommandResult::Text(continuation)),
                None => Ok(CommandResult::Done),
            },
            LoopAction::DeclinePlan {
                loop_id,
                version,
                feedback,
            } => match agent.decline_plan(chat, loop_id, version, feedback).await? {
                Some(receipt) => Ok(CommandResult::Submitted(receipt)),
                None => Ok(CommandResult::Done),
            },
            LoopAction::UpdateSubscription {
                subscription_id,
                change,
            } => {
                agent
                    .update_tick_subscription(chat, subscription_id, change)
                    .await?;
                Ok(CommandResult::Done)
            }
            LoopAction::TickUpdate { instance } => Ok(CommandResult::Submitted(
                agent.tick_send_update_for_user(chat, instance).await?,
            )),
            LoopAction::TickManage { instance } => {
                agent.tick_subscribe_for_user(chat, instance).await?;
                Ok(CommandResult::Done)
            }
            LoopAction::RetryDeniedOperation { record_id } => Ok(CommandResult::Submitted(
                agent.request_denial_retry(chat, record_id).await?,
            )),
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
                target_loop,
                command,
                background,
            } => Ok(CommandResult::ShellSubmitted(
                self.inner
                    .agent
                    .submit_user_shell_for_goal(chat, target_loop, command, background)
                    .await?,
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
                    .dispatch(chat, task.task_id, action)
                    .await
                    .map(CommandResult::Task)
                    .map_err(|error| AccessError::invalid(error.to_string()))
            }
            TaskAction::TakeBackQueued { draft_empty } => {
                let taken = self
                    .inner
                    .agent
                    .take_queued_inputs(chat, draft_empty)
                    .await?;
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
            .snapshot(chat)
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

    fn builtin_config(
        &self,
        config: tickloop_agent_protocol::ModelConfig,
    ) -> Result<tickloop_agent_protocol::ModelConfig, AccessError> {
        if !self.inner.builtin_catalog_only.load(Ordering::SeqCst) {
            return Ok(config);
        }
        tickloop_agent_model::cards::ModelCatalog::defaults()
            .get(&config.provider_id, &config.model_id)
            .ok_or_else(|| {
                AccessError::invalid("Only built-in Provider/model selections are supported")
            })
    }

    async fn observe_account_models(
        &self,
        execution: ExecutionRef,
        config: tickloop_agent_protocol::ModelConfig,
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
        config: tickloop_agent_protocol::SearchConfig,
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
                result = tickloop_agent_tools::web_search::test_connection(&config, root.as_deref(), cancel.clone()) => result,
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
            AgentDetailQuery::ProviderCredentials => {
                let launches = self.inner.launches.lock().await;
                let launch = launches.get(&execution);
                ObservationValue::ProviderCredentials(Box::new(
                    tickloop_agent_model::auth::current_provider_credential_scope(
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
            AgentDetailQuery::TaskToolsSetting => {
                ObservationValue::TaskToolsSetting(self.inner.agent.task_tools_setting(chat).await?)
            }
            AgentDetailQuery::Operation { run_id, call_id } => ObservationValue::Operation(
                self.inner
                    .agent
                    .operation(chat, run_id, &call_id)
                    .await?
                    .map(Box::new),
            ),
            AgentDetailQuery::Events { after } => {
                ObservationValue::Events(self.inner.agent.events_after(chat, after).await?)
            }
            AgentDetailQuery::Permissions => ObservationValue::Permissions(Box::new(
                self.inner.agent.permissions_view(chat).await?,
            )),
            AgentDetailQuery::ApprovalHistory => ObservationValue::ApprovalHistory(
                self.inner.agent.approval_history_view(chat).await?,
            ),
            AgentDetailQuery::Tasks => ObservationValue::Tasks(
                ShellTaskCenter::new(self.inner.agent.clone())
                    .snapshot(chat)
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
                        .read_output(chat, task.task_id, offset, limit)
                        .await
                        .map_err(|error| AccessError::invalid(error.to_string()))?,
                )
            }
            AgentDetailQuery::Subscriptions => {
                ObservationValue::Subscriptions(self.inner.agent.tick_subscriptions(chat).await?)
            }
            AgentDetailQuery::RewindPoints => ObservationValue::RewindPoints(
                self.inner.agent.snapshot(chat).await?.rewind_points(),
            ),
            AgentDetailQuery::RewindFiles { message_id } => ObservationValue::RewindFiles(
                self.inner
                    .agent
                    .rewind_file_changes(chat, message_id)
                    .await?,
            ),
            AgentDetailQuery::Skills { name } => ObservationValue::Skills {
                available: match name {
                    Some(name) => Some(self.inner.agent.has_skill(chat, &name).await?),
                    None => None,
                },
                warnings: self.inner.agent.skill_warnings(chat).await?,
            },
            AgentDetailQuery::Warnings => {
                ObservationValue::Warnings(self.live_facts(execution, chat).await?.warnings)
            }
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
                self.inner.agent.last_sequence(chat).await?,
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
                ObservationValue::RuntimeVersion(Box::new(
                    tickloop_agent_protocol::RuntimeVersion {
                        executable: std::env::current_exe().unwrap_or_default(),
                        build_id: tickloop_agent_protocol::build_identity()
                            .unwrap_or_default()
                            .into(),
                        requested_executable: None,
                        upgrading: false,
                        error: None,
                        workers: Vec::new(),
                    },
                )),
                Cursor {
                    daemon_epoch: self.inner.epoch,
                    execution: None,
                    sequence: self.inner.sequence.load(Ordering::SeqCst),
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
                    let summary = self.inner.agent.summary(chat).await?;
                    if filter
                        .workdir
                        .as_ref()
                        .is_some_and(|path| path != &summary.workdir)
                    {
                        continue;
                    }
                    let state =
                        tickloop_agent_runtime::summary_state(&summary.loops, &summary.user_shells);
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
                    let registration = self.inner.agent.subagent_registration(chat).await?;
                    entries.push(AgentSummary {
                        availability: ExecutionAvailability::Live,
                        execution,
                        chat_id: chat,
                        preview: summary.preview,
                        updated_at: super::summary::updated_at(
                            &summary.loops,
                            &summary.user_shells,
                        ),
                        workdir: summary.workdir,
                        run_id: super::summary::summary_run_id(&summary.loops, &state),
                        run_started_at: super::summary::run_started_at(&summary.loops, &state),
                        state,
                        title: summary.title,
                        subagent: registration,
                        has_work: Some(summary.has_work),
                        waiting_permissions: summary.waiting_permissions,
                        waiting_questions: summary.waiting_questions,
                        parent: match summary.parent_chat {
                            Some(parent) => {
                                self.inner.references.lock().await.get(&parent).copied()
                            }
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
            ObservationRequest::SavedRecords {
                store_root,
                workdir,
                page,
            } => {
                if let Some(root) = store_root {
                    self.inner.agent.flush_transcripts().await;
                    return Ok(Observation::single(
                        tickloop_agent_runtime::transcript::records::saved_records(
                            root, workdir, page,
                        )
                        .await?,
                        Cursor {
                            daemon_epoch: self.inner.epoch,
                            execution: None,
                            sequence: 0,
                        },
                        None,
                    ));
                }
                let records = self.inner.agent.saved_chats(&workdir).await?;
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
                tickloop_agent_runtime::transcript::records::transcript(record, page).await?,
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
        if let Some(receipt) = self.inner.receipts.begin(&envelope).await? {
            return Ok(receipt);
        }
        let result = Box::pin(self.dispatch(&envelope)).await;
        let persistence = if let Some(error) = self.inner.agent.transcript_write_error() {
            PersistenceState::WriteFailed(error)
        } else if matches!(envelope.command, AgentCommand::FlushTranscripts) {
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
        let mut execution = access.inner.references.lock().await.get(&chat).copied();
        if execution.is_none()
            && let Ok(snapshot) = access.inner.agent.snapshot(chat).await
            && let Some(delegation) = snapshot.delegation
        {
            let parent = access
                .inner
                .references
                .lock()
                .await
                .get(&delegation.root_chat_id())
                .copied();
            if let Some(parent) = parent {
                let launch = access.inner.launches.lock().await.get(&parent).cloned();
                if let Some(launch) = launch {
                    for ancestor in delegation.lineage() {
                        access.register(ancestor.chat_id, launch.clone()).await;
                    }
                    execution = Some(access.register(chat, launch).await);
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
            let Ok(Some(command)) = access.inner.agent.take_next_queued_command(chat).await else {
                continue;
            };
            let queued = access.inner.queue.lock().await.remove(&command.id);
            if let Some((_, target_loop, action)) = queued {
                let access = access.clone();
                tokio::spawn(async move {
                    let _admission = admission;
                    let label = action_label(&action);
                    let result = match action {
                        AfterTurnAction::Submit { input, origin } => {
                            match access.host_context(execution, chat, origin, false).await {
                                Ok(context) => {
                                    access
                                        .submit_in_context(execution, target_loop, input, context)
                                        .await
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
                        .finish_queued_command(chat, command.id)
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
            change: ConfigurationChange::Directories { workdir, .. },
        } => format!("/cwd {}", workdir.display()),
        AfterTurnAction::Configure { .. } => "/configure".into(),
        AfterTurnAction::History { .. } => "/history".into(),
        AfterTurnAction::Loop { .. } => "/loop".into(),
        AfterTurnAction::Submit { .. } => "/submit".into(),
    }
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
