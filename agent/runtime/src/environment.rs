//! Where a Chat's Folder, commands and Ticks live: its Execution Environment.

use std::sync::Arc;

use crate::host_tools::{HostCallResult, HostEffect, PreparedHostCall};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::hosted::{shells::ShellGrants, tools::HostedTickTools};

mod dispatch;
pub(crate) mod local_shells;
mod observe;
mod port;
pub(crate) use dispatch::ExecutionDispatch;
pub(crate) mod shell_host;
pub(crate) mod tools;
pub use port::*;
pub use tools::ExecutionTools;

/// Where the Agent runs commands and Ticks for Chats with a Folder. A Harness without one has
/// Chats without a Folder: they touch no local file and read no local process state.
pub trait ExecutionEnvironment: Send + Sync {
    fn bind(
        &self,
        scope: ExecutionScope,
        authority: ExecutionAuthority,
    ) -> Result<Arc<dyn ExecutionPort>, ExecutionError>;
}

/// This computer: the file tools and project Skills work on the Chat's local Folder, and the
/// daemon's Tickhost, when it granted one, runs commands and Ticks.
pub struct LocalEnvironment {
    hosted_ticks: Option<Arc<HostedTickTools>>,
    shells: Option<Arc<local_shells::LocalShells>>,
}

impl LocalEnvironment {
    pub fn with_ticks(tools: Arc<HostedTickTools>) -> Self {
        Self {
            hosted_ticks: Some(tools),
            shells: None,
        }
    }

    /// Local files without a Tickhost: no command runs and no Tick opens.
    pub fn bare() -> Self {
        Self {
            hosted_ticks: None,
            shells: None,
        }
    }

    pub fn with_shells(grants: Arc<dyn ShellGrants>) -> Self {
        Self {
            hosted_ticks: None,
            shells: Some(local_shells::LocalShells::new(grants)),
        }
    }

    /// Local files with the commands and Ticks of the Tickhost a daemon granted.
    pub fn with(shell_grants: Arc<dyn ShellGrants>, hosted_ticks: Arc<HostedTickTools>) -> Self {
        let shells = local_shells::LocalShells::new(shell_grants);
        hosted_ticks.bind_shell_environment(shells.clone());
        Self {
            shells: Some(shells),
            hosted_ticks: Some(hosted_ticks),
        }
    }
}

impl ExecutionEnvironment for LocalEnvironment {
    fn bind(
        &self,
        scope: ExecutionScope,
        authority: ExecutionAuthority,
    ) -> Result<Arc<dyn ExecutionPort>, ExecutionError> {
        authority.check(scope.chat)?;
        Ok(Arc::new(LocalPort {
            identity: tickloop_ids::generate(),
            scope,
            authority,
            tools: self.hosted_ticks.clone(),
            shells: self.shells.clone(),
            prepared: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            context: None,
            source_input: None,
            staged_shell: std::sync::Mutex::new(None),
            samples: Default::default(),
            capabilities: ExecutionCapabilities {
                ticks: self.hosted_ticks.is_some(),
                shells: self.shells.is_some(),
                shell_output: ShellOutputDelivery::CallerFile,
            },
        }))
    }
}

struct LocalPort {
    identity: uuid::Uuid,
    scope: ExecutionScope,
    authority: ExecutionAuthority,
    capabilities: ExecutionCapabilities,
    tools: Option<Arc<HostedTickTools>>,
    shells: Option<Arc<local_shells::LocalShells>>,
    prepared: tokio::sync::Mutex<
        std::collections::HashMap<PreparationId, (std::sync::Weak<()>, PreparedHostCall)>,
    >,
    context: Option<tickloop_agent_protocol::HostCallContext>,
    source_input: Option<serde_json::Value>,
    staged_shell: std::sync::Mutex<Option<tickloop_agent_tools::ShellLaunch>>,
    samples: tokio::sync::Mutex<std::collections::HashMap<uuid::Uuid, observe::LocalObservation>>,
}

#[async_trait::async_trait]
impl ExecutionPort for LocalPort {
    async fn observe(
        &self,
        request: ObserveRequest,
    ) -> Result<ExecutionObservation, ExecutionError> {
        self.authority.check(self.scope.chat)?;
        if request.instance.access() == tick_protocol::SHELL_ACCESS {
            return self
                .shells
                .as_ref()
                .ok_or_else(|| {
                    ExecutionError::new("Unsupported", "this environment cannot run Shells")
                })?
                .observe(self.scope.chat, self.authority.clone(), request)
                .await;
        }
        observe::observe(self, request).await
    }

    async fn inspect(&self, query: ExecutionQuery) -> Result<ExecutionInspection, ExecutionError> {
        self.authority.check(self.scope.chat)?;
        let inspection = async {
            match query {
                query @ (ExecutionQuery::ShellStatus(_) | ExecutionQuery::ShellOutput { .. }) => {
                    let result = self
                        .shells
                        .as_ref()
                        .ok_or_else(|| {
                            ExecutionError::new("Unsupported", "this environment cannot run Shells")
                        })?
                        .inspect(self.scope.chat, query)
                        .await?;
                    self.authority.check(self.scope.chat)?;
                    Ok(result)
                }
                ExecutionQuery::Sample(cursor) => {
                    let samples = self
                        .samples
                        .lock()
                        .await
                        .get(&cursor.watch)
                        .filter(|observation| observation.instance == cursor.instance)
                        .map(|observation| observation.samples.clone())
                        .ok_or_else(|| {
                            ExecutionError::new(
                                "TargetGone",
                                "terminal observation is no longer available",
                            )
                        })?;
                    let (reply, answer) = tokio::sync::oneshot::channel();
                    samples.send(observe::Sample { reply }).await.map_err(|_| {
                        ExecutionError::new("Disconnected", "terminal observation ended")
                    })?;
                    let read = answer.await.map_err(|_| {
                        ExecutionError::new("Disconnected", "terminal observation ended")
                    })??;
                    self.authority.check(self.scope.chat)?;
                    Ok(ExecutionInspection::Read(read))
                }
                ExecutionQuery::Kind(instance) => {
                    let tools = self.tools.as_ref().ok_or_else(|| {
                        ExecutionError::new("Unsupported", "this environment has no Tickhost")
                    })?;
                    tools
                        .data(self.scope.chat, &instance)
                        .await
                        .map_err(|error| ExecutionError::new(error.code, error.message))?;
                    let adapter = tools
                        .adapter(self.scope.chat, &instance)
                        .await
                        .map_err(|error| ExecutionError::new(error.code, error.message))?;
                    self.authority.check(self.scope.chat)?;
                    Ok(ExecutionInspection::Kind(
                        if adapter == tickhost::host::TickAdapter::Protocol {
                            ExecutionKind::Protocol
                        } else {
                            ExecutionKind::Terminal
                        },
                    ))
                }
                ExecutionQuery::Describe(instance) => {
                    let tools = self.tools.as_ref().ok_or_else(|| {
                        ExecutionError::new("Unsupported", "this environment has no Tickhost")
                    })?;
                    let mut connection = tools
                        .data(self.scope.chat, &instance)
                        .await
                        .map_err(|error| ExecutionError::new(error.code, error.message))?;
                    let description = connection
                        .request(
                            "describe",
                            json!({}),
                            tickhost_connect::hosted::CONTROL_TIMEOUT,
                        )
                        .await
                        .map_err(|error| ExecutionError::new(error.code, error.message))?;
                    self.authority.check(self.scope.chat)?;
                    Ok(ExecutionInspection::Description(description))
                }
                ExecutionQuery::Capabilities => {
                    Ok(ExecutionInspection::Capabilities(self.capabilities))
                }
                ExecutionQuery::Instances => {
                    let members = match &self.tools {
                        Some(tools) => tools
                            .members(self.scope.chat)
                            .await
                            .map_err(|error| ExecutionError::new(error.code, error.message))?,
                        None => Vec::new(),
                    };
                    self.authority.check(self.scope.chat)?;
                    Ok(ExecutionInspection::Instances(
                        members.into_iter().map(|member| member.instance).collect(),
                    ))
                }
                ExecutionQuery::Receipt(operation) => {
                    let receipt = match &self.tools {
                        Some(tools) => tools.execution_receipt(self.scope.chat, operation).await,
                        None => ExecutionReceipt::Unknown,
                    };
                    self.authority.check(self.scope.chat)?;
                    Ok(ExecutionInspection::Receipt(receipt))
                }
                ExecutionQuery::Preparation(operation) => {
                    let plans = self.prepared.lock().await;
                    let (_, call) = plans.get(&operation).ok_or_else(|| {
                        ExecutionError::new(
                            "UnknownPreparation",
                            "preparation was consumed or expired",
                        )
                    })?;
                    self.tools
                        .as_ref()
                        .ok_or_else(|| {
                            ExecutionError::new("Unsupported", "this environment has no Tickhost")
                        })?
                        .revalidate_native(call)
                        .await
                        .map_err(|message| ExecutionError::new("TargetChanged", message))?;
                    self.authority.check(self.scope.chat)?;
                    Ok(ExecutionInspection::Validated)
                }
                ExecutionQuery::Read(instance) => {
                    let tools = self.tools.as_ref().ok_or_else(|| {
                        ExecutionError::new("Unsupported", "this environment has no Tickhost")
                    })?;
                    let value = tools
                        .read(self.scope.chat, &instance)
                        .await
                        .map_err(|message| ExecutionError::new("ReadFailed", message))?;
                    self.authority.check(self.scope.chat)?;
                    let state = serde_json::from_value(value.clone()).map_err(|error| {
                        ExecutionError::new("ProtocolViolation", error.to_string())
                    })?;
                    Ok(ExecutionInspection::Read(ExecutionRead {
                        kind: if value["source"] == "protocol" {
                            ExecutionKind::Protocol
                        } else {
                            ExecutionKind::Terminal
                        },
                        state,
                        truncated: value["truncated"].as_bool().unwrap_or(false),
                    }))
                }
            }
        };
        tokio::select! { biased; _ = self.authority.revoked() => Err(ExecutionError::new("AuthorityRevoked", "execution authority is no longer valid")), result = inspection => result }
    }

    async fn prepare(&self, intent: ExecutionIntent) -> Result<PreparedExecution, ExecutionError> {
        self.authority.check(self.scope.chat)?;
        let preparation = async {
            if let ExecutionIntent::StopShell { instance, reason } = intent {
                let prepared = self
                    .shells
                    .as_ref()
                    .ok_or_else(|| {
                        ExecutionError::new("Unsupported", "this environment cannot run Shells")
                    })?
                    .prepare_stop(
                        self.identity,
                        self.scope.chat,
                        &self.authority,
                        instance,
                        reason,
                    )
                    .await?;
                self.authority.check(self.scope.chat)?;
                return Ok(prepared);
            }
            if let ExecutionIntent::SpawnShell(spec) = intent {
                let staged = self.staged_shell.lock().unwrap().take();
                let prepared = self
                    .shells
                    .as_ref()
                    .ok_or_else(|| {
                        ExecutionError::new("Unsupported", "this environment cannot run Shells")
                    })?
                    .prepare(self.identity, &self.authority, spec, staged)
                    .await?;
                self.authority.check(self.scope.chat)?;
                return Ok(prepared);
            }
            let tools = self.tools.as_ref().ok_or_else(|| {
                ExecutionError::new("Unsupported", "this environment has no Tickhost")
            })?;
            let (name, input) = match &intent {
                ExecutionIntent::SpawnShell(_) | ExecutionIntent::StopShell { .. } => {
                    return Err(ExecutionError::new(
                        "Unsupported",
                        "Shell execution is unavailable",
                    ));
                }
                ExecutionIntent::Open(open) => {
                    let mut input =
                        json!({"action":"open", "command":open.command, "protocol":open.protocol});
                    if let Some(cwd) = &open.cwd {
                        input["cwd"] = json!(cwd);
                    }
                    if let Some(view) = &open.view {
                        input["placement"] = json!(view.placement);
                        if let Some(workspace) = &view.workspace_id {
                            input["workspace_id"] = json!(workspace);
                        }
                    }
                    (crate::tick_tools::TICK_MANAGE, input)
                }
                ExecutionIntent::SendInput { instance, data } => (
                    crate::tick_tools::TICK_MANAGE,
                    json!({"action":"send_input", "tick_instance_id":instance, "data":data}),
                ),
                ExecutionIntent::Reopen { instance } => (
                    crate::tick_tools::TICK_MANAGE,
                    json!({"action":"reopen", "tick_instance_id":instance}),
                ),
                ExecutionIntent::Close { instance } => (
                    crate::tick_tools::TICK_MANAGE,
                    json!({"action":"close", "tick_instance_id":instance}),
                ),
                ExecutionIntent::Call {
                    instance,
                    tool,
                    args,
                    ..
                } => (
                    crate::tick_tools::TICK_CALL,
                    json!({"tick_instance_id":instance, "tool":tool, "args":args}),
                ),
            };
            let context =
                self.scope
                    .folder
                    .as_ref()
                    .map(|folder| tickloop_agent_protocol::HostCallContext {
                        folder: folder.into(),
                        workspace_id: None,
                        interaction: None,
                    });
            let call = match &intent {
                ExecutionIntent::SendInput { instance, data } if self.source_input.is_none() => {
                    tools
                        .prepare_input_bytes(self.scope.chat, instance, data)
                        .await
                }
                _ => {
                    tools
                        .prepare(
                            self.scope.chat,
                            name,
                            self.source_input.as_ref().unwrap_or(&input),
                            self.context.as_ref().or(context.as_ref()),
                        )
                        .await
                }
            }
            .map_err(|message| ExecutionError::new("PreparationFailed", message))?;
            if let ExecutionIntent::Call { revision, .. } = &intent
                && call
                    .approval_facts()
                    .external_declaration
                    .as_ref()
                    .and_then(|catalog| catalog["revision"].as_str())
                    != Some(revision)
            {
                return Err(ExecutionError::new(
                    "RevisionMismatch",
                    "the Tick catalog changed; prepare and approve again",
                ));
            }
            self.authority.check(self.scope.chat)?;
            let prepared = PreparedExecution::for_operation(
                HostedTickTools::prepared_operation(&call).unwrap_or_else(tickloop_ids::generate),
                self.identity,
                &self.authority,
                ExecutionFacts {
                    intent: HostedTickTools::resolved_intent(&call, intent),
                    normalized_input: call.input().clone(),
                    permission: call.permission(),
                    summary: call.approval_summary().into(),
                    approval: call.approval_facts().clone(),
                },
            );
            let mut plans = self.prepared.lock().await;
            plans.retain(|_, (lifetime, _)| lifetime.strong_count() > 0);
            plans.insert(prepared.preparation(), (prepared.lifetime(), call));
            Ok(prepared)
        };
        tokio::select! { biased; _ = self.authority.revoked() => Err(ExecutionError::new("AuthorityRevoked", "execution authority is no longer valid")), result = preparation => result }
    }

    async fn execute(
        &self,
        prepared: PreparedExecution,
        admission: ExecutionAdmission,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        if let Err(error) = prepared.validate(self.identity, &self.authority, &admission) {
            return ExecutionOutcome::NotDispatched(error);
        }
        let operation = prepared.operation();
        let is_shell_start = matches!(prepared.facts().intent, ExecutionIntent::SpawnShell(_));
        let dispatch =
            ExecutionDispatch::new(self.authority.clone(), self.scope.chat, cancel.clone());
        let execution = async {
            if matches!(
                prepared.facts().intent,
                ExecutionIntent::SpawnShell(_) | ExecutionIntent::StopShell { .. }
            ) {
                return match &self.shells {
                    Some(shells) => {
                        shells
                            .execute(
                                self.identity,
                                self.scope.chat,
                                prepared,
                                cancel,
                                dispatch.clone(),
                            )
                            .await
                    }
                    None => ExecutionOutcome::NotDispatched(ExecutionError::new(
                        "Unsupported",
                        "this environment cannot run Shells",
                    )),
                };
            }
            let Some((_, call)) = self.prepared.lock().await.remove(&prepared.preparation()) else {
                return ExecutionOutcome::NotDispatched(ExecutionError::new(
                    "UnknownPreparation",
                    "preparation was consumed or expired",
                ));
            };
            let Some(tools) = &self.tools else {
                return ExecutionOutcome::NotDispatched(ExecutionError::new(
                    "Unsupported",
                    "this environment has no Tickhost",
                ));
            };
            match tools
                .execute_native(self.scope.chat, call, cancel, Some(dispatch.clone()))
                .await
            {
                HostCallResult::Completed(result) => ExecutionOutcome::Completed(ExecutionResult {
                    result,
                    opened: None,
                    shell: None,
                    handoff: None,
                }),
                HostCallResult::CompletedWithEffect(result, effect) => {
                    let opened = match *effect {
                        HostEffect::OpenedTick {
                            instance,
                            title,
                            is_terminal,
                        } => Some(OpenedExecution {
                            instance,
                            title,
                            is_terminal,
                        }),
                        _ => None,
                    };
                    ExecutionOutcome::Completed(ExecutionResult {
                        result,
                        opened,
                        shell: None,
                        handoff: None,
                    })
                }
                HostCallResult::NotDispatched(message) => {
                    ExecutionOutcome::NotDispatched(ExecutionError::new("NotDispatched", message))
                }
                HostCallResult::OutcomeUnknown(reason) => ExecutionOutcome::OutcomeUnknown {
                    operation: prepared.operation(),
                    reason,
                },
            }
        };
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = dispatch.interrupted() => {
                if is_shell_start && !dispatch.was_sent() && self.authority.check(self.scope.chat).is_ok() {
                    // Settle an already-issued spawn grant before completing pre-spawn cancellation.
                    tokio::select! {
                        biased;
                        _ = self.authority.revoked() => dispatch.interruption(operation),
                        outcome = &mut execution => outcome,
                    }
                } else { dispatch.interruption(operation) }
            },
            outcome = &mut execution => outcome,
        }
    }
}

pub(crate) struct EnvironmentPreparedCall {
    port: Arc<dyn ExecutionPort>,
    prepared: PreparedExecution,
    authority: ExecutionAuthority,
    chat: tickloop_agent_protocol::ChatId,
}

impl EnvironmentPreparedCall {
    pub(crate) async fn revalidate(
        &self,
        chat: tickloop_agent_protocol::ChatId,
    ) -> Result<(), String> {
        if chat != self.chat {
            return Err("preparation belongs to another Chat".into());
        }
        self.port
            .inspect(ExecutionQuery::Preparation(self.prepared.preparation()))
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn execute(
        self,
        chat: tickloop_agent_protocol::ChatId,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if chat != self.chat {
            return HostCallResult::NotDispatched("preparation belongs to another Chat".into());
        }
        let admission = match self.authority.admit(&self.prepared) {
            Ok(admission) => admission,
            Err(error) => return HostCallResult::NotDispatched(error.to_string()),
        };
        match self.port.execute(self.prepared, admission, cancel).await {
            ExecutionOutcome::Completed(ExecutionResult {
                result,
                opened: Some(opened),
                ..
            }) => HostCallResult::CompletedWithEffect(
                result,
                Box::new(HostEffect::OpenedTick {
                    instance: opened.instance,
                    title: opened.title,
                    is_terminal: opened.is_terminal,
                }),
            ),
            ExecutionOutcome::Completed(ExecutionResult { result, .. }) => {
                HostCallResult::Completed(result)
            }
            ExecutionOutcome::NotDispatched(error) => HostCallResult::NotDispatched(error.message),
            ExecutionOutcome::OutcomeUnknown { reason, .. } => {
                HostCallResult::OutcomeUnknown(reason)
            }
        }
    }
}

pub(crate) async fn prepare_hosted_call(
    tools: Arc<HostedTickTools>,
    chat: tickloop_agent_protocol::ChatId,
    name: &str,
    input: &serde_json::Value,
    context: Option<&tickloop_agent_protocol::HostCallContext>,
    cancel: CancellationToken,
    views: bool,
) -> Result<PreparedHostCall, String> {
    let authority = ExecutionAuthority::with_cancellation(chat, cancel);
    let port: Arc<dyn ExecutionPort> = Arc::new(LocalPort {
        identity: tickloop_ids::generate(),
        scope: ExecutionScope {
            chat,
            folder: context.map(|context| context.folder.to_string_lossy().into_owned()),
        },
        authority: authority.clone(),
        capabilities: ExecutionCapabilities {
            ticks: true,
            shells: false,
            shell_output: ShellOutputDelivery::CallerFile,
        },
        tools: Some(tools),
        shells: None,
        prepared: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        context: context.cloned(),
        source_input: Some(input.clone()),
        staged_shell: std::sync::Mutex::new(None),
        samples: Default::default(),
    });
    tools::prepare_on_port(port, authority, chat, name, input, views).await
}

pub(crate) fn local_shell_host(
    tasks: Arc<crate::hosted::shells::ShellTasks>,
    chat: uuid::Uuid,
) -> Arc<dyn tickloop_agent_tools::ShellHost> {
    local_shells::host(tasks, chat.into())
}

fn shell_port(
    shells: Arc<local_shells::LocalShells>,
    chat: tickloop_agent_protocol::ChatId,
    authority: ExecutionAuthority,
    staged: Option<tickloop_agent_tools::ShellLaunch>,
) -> Arc<dyn ExecutionPort> {
    Arc::new(LocalPort {
        identity: tickloop_ids::generate(),
        scope: ExecutionScope {
            chat,
            folder: staged
                .as_ref()
                .map(|launch| launch.cwd.to_string_lossy().into_owned()),
        },
        authority,
        capabilities: ExecutionCapabilities {
            ticks: false,
            shells: true,
            shell_output: ShellOutputDelivery::CallerFile,
        },
        tools: None,
        shells: Some(shells),
        prepared: Default::default(),
        context: None,
        source_input: None,
        staged_shell: std::sync::Mutex::new(staged),
        samples: Default::default(),
    })
}
