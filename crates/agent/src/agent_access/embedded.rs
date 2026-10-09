use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{AgentAccess, Observation, protocol::*, receipts::ReceiptStore, views::ViewRegistry};
use crate::{
    host_tools::{HerdrInteraction, HostCallContext},
    model::model_client,
    protocol::FEED_CAPACITY,
    runtime::{Agent, BranchRequest, ShellTaskAction, ShellTaskCenter},
};

#[derive(Clone)]
pub struct EmbeddedAgentAccess {
    inner: Arc<Inner>,
}

type QueuedAction = (ExecutionRef, Option<Uuid>, AfterTurnAction);

struct Inner {
    agent: Agent,
    epoch: Uuid,
    executions: Mutex<HashMap<ExecutionRef, Uuid>>,
    references: Mutex<HashMap<Uuid, ExecutionRef>>,
    launches: Mutex<HashMap<ExecutionRef, LaunchSpec>>,
    account_models: crate::model::availability::AccountModelDiscovery,
    receipts: ReceiptStore,
    answers: Mutex<HashMap<(ExecutionRef, Uuid, RequestKind), AnswerState>>,
    queue: Mutex<HashMap<Uuid, QueuedAction>>,
    queue_failures: Mutex<VecDeque<QueuedCommandFailure>>,
    views: ViewRegistry,
    verified_sources: AtomicBool,
    builtin_catalog_only: AtomicBool,
    sequence: AtomicU64,
    changes: broadcast::Sender<ObservationUpdate>,
    publishing: Mutex<()>,
    cancel: CancellationToken,
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
        let events = agent.subscribe();
        let shells = agent.subscribe_user_shells();
        let tasks = agent.subscribe_task_notifications();
        let inner = Arc::new(Inner {
            agent,
            epoch,
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
        let access = Self::with_epoch(agent, epoch);
        access.inner.verified_sources.store(true, Ordering::SeqCst);
        access
            .inner
            .builtin_catalog_only
            .store(true, Ordering::SeqCst);
        access
    }

    pub fn owner_epoch(&self) -> Uuid {
        self.inner.epoch
    }
    pub fn runtime(&self) -> &Agent {
        &self.inner.agent
    }
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.agent.shutdown().await;
    }

    async fn register(&self, chat_id: Uuid, launch: LaunchSpec) -> ExecutionRef {
        let mut references = self.inner.references.lock().await;
        if let Some(execution) = references.get(&chat_id) {
            return *execution;
        }
        let execution = ExecutionRef {
            owner_epoch: self.inner.epoch,
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

    fn hosted_ticks(&self) -> Result<Arc<crate::hosted::tools::HostedTickTools>, AccessError> {
        self.inner.agent.hosted_ticks().ok_or_else(|| {
            AccessError::new(
                AccessErrorCode::Unsupported,
                "this Agent runs no hosted Ticks",
            )
        })
    }

    async fn chat(&self, execution: ExecutionRef) -> Result<Uuid, AccessError> {
        if execution.owner_epoch != self.inner.epoch {
            return Err(AccessError::new(
                AccessErrorCode::EpochChanged,
                "execution belongs to an earlier owner",
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
        let mut warnings = self.inner.agent.user_settings_warnings(chat);
        warnings.extend(self.inner.agent.transcript_load_warnings(chat));
        if let Some(error) = self.inner.agent.transcript_write_error() {
            warnings.push(error);
        }
        let mut snapshot = LiveChatSnapshot::from_snapshot(execution, snapshot, warnings);
        snapshot.subagent = self.inner.agent.subagent_registration(chat).await?;
        snapshot.transcript_write_error = self.inner.agent.transcript_write_error();
        snapshot.loop_trigger_notices = self
            .inner
            .agent
            .loop_trigger_notices()
            .into_iter()
            .filter(|notice| notice.chat_id == chat)
            .collect();
        snapshot.queue_failures = self
            .inner
            .queue_failures
            .lock()
            .await
            .iter()
            .filter(|failure| failure.execution == execution)
            .cloned()
            .collect();
        snapshot.warnings.extend(
            snapshot.queue_failures.iter().map(|failure| {
                format!("Queued command {} failed: {}", failure.label, failure.error)
            }),
        );
        let mut answers = self.inner.answers.lock().await;
        let pending = snapshot
            .request_refs
            .iter()
            .map(|request| (request.execution, request.request_id, request.kind))
            .collect::<std::collections::HashSet<_>>();
        answers.retain(|key, _| key.0 != execution || pending.contains(key));
        for request in &mut snapshot.request_refs {
            request.version = answers
                .get(&(request.execution, request.request_id, request.kind))
                .map_or(1, |state| state.version);
        }
        Ok(snapshot)
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
                let pane = tick_connect::herdr::Pane {
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
                    title: None,
                    tokens: Default::default(),
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
                Ok(CommandResult::HostedTickReopened {
                    instance: reopened,
                    membership,
                })
            }
            AgentCommand::HostedTickSnapshot {
                execution,
                instance,
            } => {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::HostedTickSnapshot(
                    self.hosted_ticks()?
                        .snapshot(chat, &instance)
                        .await
                        .map_err(hosted_access_error)?,
                ))
            }
            AgentCommand::AttachHostedTick {
                execution,
                instance,
            } => {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::HostedTickView(
                    self.hosted_ticks()?
                        .allow_view(chat, &instance)
                        .await
                        .map_err(hosted_access_error)?,
                ))
            }
            AgentCommand::AddMonitorMember {
                execution,
                instance,
                title,
            } => {
                let chat = self.chat(execution).await?;
                Ok(CommandResult::MonitorMembership(
                    self.inner
                        .agent
                        .add_monitor_member(chat, instance, title)
                        .await?,
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
                let directed = crate::runtime::DirectedHumanSource {
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
            } => self.submit(execution, target_loop, input, origin).await,
            AgentCommand::ContinueLoop {
                execution,
                loop_id,
                input,
                origin,
            } => self.submit(execution, Some(loop_id), input, origin).await,
            AgentCommand::DecidePermission {
                request,
                allow,
                scope,
                comment,
                rule,
            } => {
                self.claim_answer(request, RequestKind::Permission, envelope.command_id)
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
                self.claim_answer(request, RequestKind::Question, envelope.command_id)
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
                self.configure(execution, change).await
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
        }
    }

    async fn claim_answer(
        &self,
        request: RequestRef,
        kind: RequestKind,
        command_id: Uuid,
    ) -> Result<(), AccessError> {
        if request.kind != kind {
            return Err(AccessError::invalid(
                "answer kind does not match the original request",
            ));
        }
        let snapshot = self.snapshot(request.execution).await?;
        let pending = match kind {
            RequestKind::Permission => snapshot
                .permissions
                .iter()
                .any(|pending| pending.id == request.request_id),
            RequestKind::Question => snapshot
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
            ConfigurationChange::Model { config, thinking } => {
                let config = self.builtin_config(config)?;
                let root = self
                    .inner
                    .launches
                    .lock()
                    .await
                    .get(&execution)
                    .and_then(|launch| launch.config_root.clone());
                let client = model_client(config.clone(), root.as_deref())
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
            ConfigurationChange::PermissionChange { change, expected } => {
                return match agent.apply_permission_change(chat, change, expected).await {
                    Ok(view) => Ok(CommandResult::Permissions(Box::new(view))),
                    Err(crate::runtime::PermissionChangeError::Conflict { latest }) => {
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
            LoopAction::DisableEventSources { loop_id } => {
                agent.disable_event_sources_for_loop(chat, loop_id).await;
                Ok(CommandResult::Done)
            }
            LoopAction::ResumeEventSources { loop_id } => {
                let count = agent.resume_event_sources(chat, loop_id).await?;
                Ok(CommandResult::EventSourcesResumed { count })
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
            TaskOrigin::Agent => crate::runtime::ShellTaskOrigin::Agent,
            TaskOrigin::User => crate::runtime::ShellTaskOrigin::User,
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
        let update = super::bounded_update(ObservationUpdate {
            cursor: Cursor {
                owner_epoch: self.inner.epoch,
                execution,
                sequence,
            },
            change,
        });
        let _ = self.inner.changes.send(update);
    }

    fn builtin_config(
        &self,
        config: crate::model::ModelConfig,
    ) -> Result<crate::model::ModelConfig, AccessError> {
        if !self.inner.builtin_catalog_only.load(Ordering::SeqCst) {
            return Ok(config);
        }
        crate::model::cards::ModelCatalog::defaults()
            .get(&config.provider_id, &config.model_id)
            .ok_or_else(|| {
                AccessError::invalid("Only built-in Provider/model selections are supported")
            })
    }

    async fn observe_account_models(
        &self,
        execution: ExecutionRef,
        config: crate::model::ModelConfig,
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
            owner_epoch: self.inner.epoch,
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
            return Ok(Observation {
                initial: ObservationValue::AccountModels(directory),
                cursor,
                reset: None,
                changes: None,
                terminal_error: None,
            });
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
        Ok(Observation {
            initial: ObservationValue::AccountModelsPending,
            cursor,
            reset: None,
            changes: Some(receiver),
            terminal_error: None,
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
        let mut receiver = self.inner.changes.subscribe();
        let sequence = self.inner.sequence.load(Ordering::SeqCst);
        let chat = self.chat(execution).await?;
        let snapshot = self.snapshot(execution).await?;
        let watermark = snapshot.last_sequence;
        let initial = match detail {
            AgentDetailQuery::ProviderCredentials => {
                let launches = self.inner.launches.lock().await;
                let launch = launches.get(&execution);
                ObservationValue::ProviderCredentials(Box::new(ProviderCredentialScope::current(
                    launch.and_then(|launch| launch.config_root.clone()),
                )))
            }
            AgentDetailQuery::AccountModels { .. } => {
                unreachable!("account queries return their own observation")
            }
            AgentDetailQuery::Snapshot => ObservationValue::Agent(Box::new(snapshot)),
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
            AgentDetailQuery::RewindPoints => {
                ObservationValue::RewindPoints(snapshot.conversation.rewind_points())
            }
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
            AgentDetailQuery::Warnings => ObservationValue::Warnings(snapshot.warnings),
        };
        let cursor = Cursor {
            owner_epoch: self.inner.epoch,
            execution: Some(execution),
            sequence,
        };
        let reset = after.and_then(|old| {
            if old.owner_epoch != cursor.owner_epoch {
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
                                    owner_epoch: access.inner.epoch,
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
                if sender.send(super::bounded_update(update)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Observation {
            initial,
            cursor,
            reset,
            changes: Some(changes),
            terminal_error: None,
        })
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
            ObservationRequest::Overview {
                filter,
                page,
                after,
            } => {
                let mut receiver = self.inner.changes.subscribe();
                let cursor = Cursor {
                    owner_epoch: self.inner.epoch,
                    execution: None,
                    sequence: self.inner.sequence.load(Ordering::SeqCst),
                };
                let references = self.inner.references.lock().await.clone();
                let views = self.inner.views.displayed().await;
                let mut entries = Vec::new();
                for (chat, execution) in references {
                    let snapshot = self.inner.agent.snapshot(chat).await?;
                    if filter
                        .workdir
                        .as_ref()
                        .is_some_and(|path| path != &snapshot.workdir)
                    {
                        continue;
                    }
                    let state =
                        crate::runtime::summary_state(&snapshot.loops, &snapshot.user_shells);
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
                    let has_work = super::has_work(
                        &snapshot,
                        &snapshot.user_shells,
                        snapshot.pending_inputs.len() + snapshot.pending_commands.len(),
                    );
                    let title =
                        crate::runtime::chat_display_title(&snapshot, &snapshot.user_shells);
                    let registration = self.inner.agent.subagent_registration(chat).await?;
                    entries.push(AgentSummary {
                        availability: ExecutionAvailability::Live,
                        execution,
                        chat_id: chat,
                        preview: super::preview(&snapshot, &title),
                        updated_at: super::updated_at(&snapshot.loops, &snapshot.user_shells),
                        workdir: snapshot.workdir,
                        run_id: super::summary_run_id(&snapshot.loops, &state),
                        state,
                        title,
                        subagent: registration,
                        has_work: Some(has_work),
                        waiting_permissions: snapshot.permissions.len(),
                        waiting_questions: snapshot.questions.len(),
                        parent: match &snapshot.delegation {
                            Some(delegation) => self
                                .inner
                                .references
                                .lock()
                                .await
                                .get(&delegation.parent_chat_id)
                                .copied(),
                            None => None,
                        },
                        record,
                        workspace,
                        capabilities: if snapshot.delegation.is_some() {
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
                Ok(Observation {
                    initial: ObservationValue::Overview(
                        entries.into_iter().skip(page.offset).take(limit).collect(),
                    ),
                    cursor,
                    reset,
                    changes: Some(changes),
                    terminal_error: None,
                })
            }
            ObservationRequest::Receipt { epoch, command_id } => Ok(Observation {
                initial: ObservationValue::Receipt(
                    self.inner
                        .receipts
                        .lookup(epoch, command_id)
                        .await
                        .map(Box::new),
                ),
                cursor: Cursor {
                    owner_epoch: self.inner.epoch,
                    execution: None,
                    sequence: self.inner.sequence.load(Ordering::SeqCst),
                },
                reset: None,
                changes: None,
                terminal_error: None,
            }),
            ObservationRequest::SavedRecords {
                store_root,
                workdir,
                page,
            } => {
                if let Some(root) = store_root {
                    self.inner.agent.flush_transcripts().await;
                    return Ok(Observation {
                        initial: super::records::saved_records(root, workdir, page).await?,
                        cursor: Cursor {
                            owner_epoch: self.inner.epoch,
                            execution: None,
                            sequence: 0,
                        },
                        reset: None,
                        changes: None,
                        terminal_error: None,
                    });
                }
                let records = self.inner.agent.saved_chats(&workdir).await?;
                let limit = page.bounded_limit();
                Ok(Observation {
                    initial: ObservationValue::Records(
                        records.into_iter().skip(page.offset).take(limit).collect(),
                    ),
                    cursor: Cursor {
                        owner_epoch: self.inner.epoch,
                        execution: None,
                        sequence: 0,
                    },
                    reset: None,
                    changes: None,
                    terminal_error: None,
                })
            }
            ObservationRequest::Transcript { record, page } => Ok(Observation {
                initial: super::records::transcript(record, page).await?,
                cursor: Cursor {
                    owner_epoch: self.inner.epoch,
                    execution: None,
                    sequence: 0,
                },
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
        let result = self.dispatch(&envelope).await;
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
    mut events: broadcast::Receiver<crate::runtime::Event>,
    mut shells: broadcast::Receiver<crate::runtime::UserShellRecord>,
    mut tasks: broadcast::Receiver<crate::runtime::TaskNotification>,
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
                .get(&delegation.parent_chat_id)
                .copied();
            if let Some(parent) = parent {
                let launch = access.inner.launches.lock().await.get(&parent).cloned();
                if let Some(launch) = launch {
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
            let Ok(Some(command)) = access.inner.agent.take_next_queued_command(chat).await else {
                continue;
            };
            let queued = access.inner.queue.lock().await.remove(&command.id);
            if let Some((_, target_loop, action)) = queued {
                let access = access.clone();
                tokio::spawn(async move {
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
fn hosted_access_error(error: tick_connect::host_error::HostError) -> AccessError {
    let code = if error.dispatched {
        AccessErrorCode::OutcomeUnknown
    } else {
        AccessErrorCode::Unavailable
    };
    AccessError::new(code, error.to_string())
}
