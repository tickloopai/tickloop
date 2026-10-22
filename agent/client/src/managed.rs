//! Scoped clients for a newly created managed execution. A control proof never grants access to
//! another execution. Mutations are dispatched once; uncertain replies are resolved by receipt,
//! never by repeating the mutation.

use std::{
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tickloop_agent_protocol::{
    AccessCapability, AccessError, AccessErrorCode, AgentAccess, AgentCommand, AgentDetailQuery,
    CommandEnvelope, CommandResult, ExecutionProfile, ExecutionRef, LaunchSpec, LiveChatSnapshot,
    ManagedConfiguration, ManagedConfigurationChange, ManagedHistoryPage, ManagedSessionPage,
    ManagedToken, Observation, ObservationRequest, ObservationValue, PermissionScope, Receipt,
    RequestRef, UserInput,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{ReconnectFactory, RecoveringAgentClient};

#[cfg(test)]
#[path = "managed_tests.rs"]
mod tests;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ManagedClient {
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    session_id: Arc<dyn Fn() -> Uuid + Send + Sync>,
}

/// Connects without Console startup, View registration, interactive upgrade or daemon shutdown.
pub async fn connect(executable: PathBuf) -> Result<ManagedClient, AccessError> {
    let local = crate::connect_or_start(executable.clone())
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    if !local
        .protocol()
        .capabilities
        .contains(&AccessCapability::ManagedExecutions)
    {
        return Err(AccessError::new(
            AccessErrorCode::Unsupported,
            "This Agent daemon does not support managed executions; no work was created",
        ));
    }
    let epoch = local.daemon_epoch();
    let reconnect: ReconnectFactory = Arc::new(move || {
        let executable = executable.clone();
        Box::pin(async move {
            let client = crate::connect_to_running_daemon(executable)
                .await
                .map_err(AccessError::unavailable)?;
            if client.daemon_epoch() != epoch {
                return Err(AccessError::new(
                    AccessErrorCode::EpochChanged,
                    "Agent daemon changed; the managed execution was not restarted",
                ));
            }
            if !client
                .protocol()
                .capabilities
                .contains(&AccessCapability::ManagedExecutions)
            {
                return Err(AccessError::new(
                    AccessErrorCode::Unsupported,
                    "Agent daemon no longer supports managed executions",
                ));
            }
            Ok(client)
        })
    });
    let recovery = RecoveringAgentClient::new(local, reconnect);
    let identity = recovery.clone();
    Ok(ManagedClient {
        access: Arc::new(recovery),
        epoch,
        session_id: Arc::new(move || identity.session_id()),
    })
}

impl ManagedClient {
    /// An injectable transport. Production callers should use `connect` to negotiate support.
    pub fn new(access: Arc<dyn AgentAccess>, epoch: Uuid, session_id: Uuid) -> Self {
        Self {
            access,
            epoch,
            session_id: Arc::new(move || session_id),
        }
    }

    pub async fn create(&self, launch: LaunchSpec) -> Result<ManagedSession, AccessError> {
        if launch.profile == ExecutionProfile::Local || launch.exact_chat.is_some() {
            return Err(AccessError::invalid(
                "managed execution requires a new OneShot or Acp Chat",
            ));
        }
        let token = ManagedToken::new();
        let result = dispatch(
            &self.access,
            self.epoch,
            AgentCommand::CreateManaged {
                launch,
                initial: None,
                session_id: (self.session_id)(),
                token: token.clone(),
            },
        )
        .await?;
        let CommandResult::Created { execution, .. } = result else {
            return Err(AccessError::invalid("invalid managed creation reply"));
        };
        self.session(execution, token)
    }

    pub async fn load(
        &self,
        session_id: Uuid,
        launch: LaunchSpec,
    ) -> Result<ManagedSession, AccessError> {
        if launch.profile != ExecutionProfile::Acp
            || !launch.session_persistence
            || launch.exact_chat.is_some()
        {
            return Err(AccessError::invalid(
                "ACP load requires a persistent ACP launch",
            ));
        }
        let token = ManagedToken::new();
        let result = dispatch(
            &self.access,
            self.epoch,
            AgentCommand::ManagedLoad {
                session_uuid: session_id,
                launch,
                session_id: (self.session_id)(),
                token: token.clone(),
            },
        )
        .await?;
        let CommandResult::Loaded { execution, .. } = result else {
            return Err(AccessError::invalid("invalid ACP load reply"));
        };
        self.session(execution, token)
    }

    pub async fn list(
        &self,
        launch: LaunchSpec,
        cursor: Option<String>,
    ) -> Result<ManagedSessionPage, AccessError> {
        match self
            .access
            .observe(ObservationRequest::ManagedSessions {
                launch: Box::new(launch),
                cursor,
            })
            .await?
            .initial
        {
            ObservationValue::ManagedSessions(page) => Ok(page),
            _ => Err(AccessError::invalid("invalid ACP session listing reply")),
        }
    }

    fn session(
        &self,
        execution: ExecutionRef,
        token: ManagedToken,
    ) -> Result<ManagedSession, AccessError> {
        let inner = Arc::new(SessionInner {
            access: self.access.clone(),
            execution,
            token,
            released: AtomicBool::new(false),
            stop: CancellationToken::new(),
            lease_error: std::sync::Mutex::new(None),
            release_result: tokio::sync::Mutex::new(None),
        });
        tokio::spawn(heartbeat(Arc::downgrade(&inner), inner.stop.clone()));
        Ok(ManagedSession { inner })
    }
}

#[derive(Clone)]
pub struct ManagedSession {
    inner: Arc<SessionInner>,
}
struct SessionInner {
    access: Arc<dyn AgentAccess>,
    execution: ExecutionRef,
    token: ManagedToken,
    released: AtomicBool,
    stop: CancellationToken,
    lease_error: std::sync::Mutex<Option<AccessError>>,
    release_result: tokio::sync::Mutex<Option<Result<(), AccessError>>>,
}

impl ManagedSession {
    pub fn execution(&self) -> ExecutionRef {
        self.inner.execution
    }
    pub fn access(&self) -> Arc<dyn AgentAccess> {
        self.inner.access.clone()
    }

    fn active(&self) -> Result<(), AccessError> {
        if self.inner.released.load(Ordering::Acquire) {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "managed execution was released",
            ));
        }
        if let Some(error) = self.inner.lease_error.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(())
    }

    pub async fn skills(
        &self,
    ) -> Result<Vec<tickloop_agent_protocol::ManagedSkillInfo>, AccessError> {
        self.active()?;
        match self
            .inner
            .access
            .observe(ObservationRequest::Agent {
                execution: self.execution(),
                detail: AgentDetailQuery::Skills { name: None },
                after: None,
            })
            .await?
            .initial
        {
            ObservationValue::Skills { commands, .. } => Ok(commands),
            _ => Err(AccessError::invalid("invalid Skill catalog reply")),
        }
    }
    pub async fn configure(
        &self,
        change: ManagedConfigurationChange,
    ) -> Result<ManagedConfiguration, AccessError> {
        self.active()?;
        match self
            .command(AgentCommand::ManagedConfigure {
                execution: self.execution(),
                token: self.inner.token.clone(),
                change,
            })
            .await?
        {
            CommandResult::ManagedConfigured(config) => Ok(config),
            _ => Err(AccessError::invalid("invalid managed configuration reply")),
        }
    }
    pub async fn configuration(&self) -> Result<ManagedConfiguration, AccessError> {
        self.active()?;
        match self
            .inner
            .access
            .observe(ObservationRequest::Agent {
                execution: self.execution(),
                detail: AgentDetailQuery::ManagedConfiguration {
                    token: self.inner.token.clone(),
                },
                after: None,
            })
            .await?
            .initial
        {
            ObservationValue::ManagedConfiguration(config) => Ok(config),
            _ => Err(AccessError::invalid(
                "invalid managed configuration query reply",
            )),
        }
    }
    pub async fn history(&self, cursor: Option<String>) -> Result<ManagedHistoryPage, AccessError> {
        self.active()?;
        match self
            .inner
            .access
            .observe(ObservationRequest::Agent {
                execution: self.execution(),
                detail: AgentDetailQuery::ManagedHistory {
                    token: self.inner.token.clone(),
                    cursor,
                },
                after: None,
            })
            .await?
            .initial
        {
            ObservationValue::ManagedHistory(page) => Ok(page),
            _ => Err(AccessError::invalid("invalid managed history query reply")),
        }
    }
    /// Establish this observation before submitting, including when the model can finish immediately.
    pub async fn observe(&self) -> Result<Observation, AccessError> {
        self.active()?;
        self.inner
            .access
            .observe(ObservationRequest::Agent {
                execution: self.execution(),
                detail: AgentDetailQuery::Snapshot,
                after: None,
            })
            .await
    }
    pub async fn snapshot(&self) -> Result<LiveChatSnapshot, AccessError> {
        let observation = self.observe().await?;
        match observation.initial {
            ObservationValue::Agent(snapshot) => Ok(*snapshot),
            _ => Err(AccessError::invalid("invalid managed snapshot reply")),
        }
    }
    pub async fn submit(&self, input: UserInput) -> Result<Receipt, AccessError> {
        self.active()?;
        match self
            .command(AgentCommand::ManagedSubmit {
                execution: self.execution(),
                token: self.inner.token.clone(),
                input,
            })
            .await?
        {
            CommandResult::Submitted(receipt) => Ok(receipt),
            _ => Err(AccessError::invalid("invalid managed submission reply")),
        }
    }
    pub async fn decide_permission(
        &self,
        request: RequestRef,
        allow: bool,
    ) -> Result<(), AccessError> {
        self.active()?;
        if request.execution != self.execution() {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "permission belongs to another execution",
            ));
        }
        match self
            .command(AgentCommand::ManagedDecidePermission {
                token: self.inner.token.clone(),
                request,
                allow,
                scope: PermissionScope::Once,
                comment: None,
                rule: None,
            })
            .await?
        {
            CommandResult::Done => Ok(()),
            _ => Err(AccessError::invalid("invalid managed permission reply")),
        }
    }
    /// Captures an authoritative final snapshot only after runtime and owned resource cleanup.
    /// `cancel` interrupts the current turn without authorizing the next one.
    pub async fn settle(&self, cancel: bool) -> Result<LiveChatSnapshot, AccessError> {
        self.active()?;
        match self
            .command(AgentCommand::ManagedSettle {
                execution: self.execution(),
                token: self.inner.token.clone(),
                cancel,
            })
            .await?
        {
            CommandResult::ManagedSettled { snapshot } => Ok(*snapshot),
            _ => Err(AccessError::invalid("invalid managed settlement reply")),
        }
    }
    /// Explicit release is awaited. Drop is only a best-effort fallback; the daemon lease handles
    /// process death and failures where an acknowledgement cannot be obtained.
    pub async fn release(&self) -> Result<(), AccessError> {
        let mut saved = self.inner.release_result.lock().await;
        if let Some(result) = &*saved {
            return result.clone();
        }
        self.inner.released.store(true, Ordering::Release);
        // A caller may cancel this future after dispatch. Keep that uncertainty instead of
        // silently issuing another release, and stop renewal even on future cancellation.
        *saved = Some(Err(AccessError::new(
            AccessErrorCode::OutcomeUnknown,
            "managed release was started but its acknowledgement was not observed",
        )));
        let _stop_on_drop = StopHeartbeatOnDrop(self.inner.stop.clone());
        let result = self
            .command(AgentCommand::ManagedRelease {
                execution: self.execution(),
                token: self.inner.token.clone(),
            })
            .await
            .and_then(|result| match result {
                CommandResult::Done => Ok(()),
                _ => Err(AccessError::invalid("invalid managed release reply")),
            });
        self.inner.stop.cancel();
        *saved = Some(result.clone());
        result
    }
    async fn command(&self, command: AgentCommand) -> Result<CommandResult, AccessError> {
        dispatch(&self.inner.access, self.execution().daemon_epoch, command).await
    }
}

struct StopHeartbeatOnDrop(CancellationToken);
impl Drop for StopHeartbeatOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        self.stop.cancel();
        if self.released.load(Ordering::Acquire) {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let access = self.access.clone();
            let execution = self.execution;
            let token = self.token.clone();
            runtime.spawn(async move {
                let _ = dispatch(
                    &access,
                    execution.daemon_epoch,
                    AgentCommand::ManagedRelease { execution, token },
                )
                .await;
            });
        }
    }
}

async fn heartbeat(inner: Weak<SessionInner>, stop: CancellationToken) {
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(HEARTBEAT_INTERVAL) => {}
        }
        let Some(session) = inner.upgrade() else {
            return;
        };
        let access = session.access.clone();
        let execution = session.execution;
        let token = session.token.clone();
        drop(session);
        let result = dispatch(
            &access,
            execution.daemon_epoch,
            AgentCommand::ManagedHeartbeat { execution, token },
        )
        .await;
        if let Err(error) = result {
            // Transport failures can recover before lease expiry. An explicit rejection is final.
            if !matches!(
                error.code,
                AccessErrorCode::Unavailable | AccessErrorCode::OutcomeUnknown
            ) && let Some(session) = inner.upgrade()
            {
                *session.lease_error.lock().unwrap() = Some(error);
                return;
            }
        }
    }
}

async fn dispatch(
    access: &Arc<dyn AgentAccess>,
    epoch: Uuid,
    command: AgentCommand,
) -> Result<CommandResult, AccessError> {
    let command_id = Uuid::new_v4();
    let result = tokio::time::timeout(
        CONTROL_TIMEOUT,
        access.execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id,
            command,
        }),
    )
    .await;
    let receipt = match result {
        Ok(Ok(receipt)) => receipt,
        Ok(Err(error)) if error.code != AccessErrorCode::OutcomeUnknown => return Err(error),
        _ => {
            let observation = tokio::time::timeout(
                Duration::from_secs(5),
                access.observe(ObservationRequest::Receipt { epoch, command_id }),
            )
            .await;
            match observation {
                Ok(Ok(Observation {
                    initial: ObservationValue::Receipt(Some(receipt)),
                    ..
                })) => *receipt,
                _ => {
                    return Err(AccessError::new(
                        AccessErrorCode::OutcomeUnknown,
                        format!("Command {command_id} outcome is unknown; it was not replayed"),
                    ));
                }
            }
        }
    };
    receipt.into_result()
}
