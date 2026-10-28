use std::{fs::File, io::Write, sync::Arc};

use async_trait::async_trait;
use tick_protocol::InstanceRef;
use tickloop_harness_protocol::CommandState;
use tickloop_harness_tools::{
    PreparedShellStart, PreparedShellStop, ShellExit, ShellHost, ShellLaunch, ShellPreparation,
    ShellPreparationRequest, ShellProcess, ShellStartError, ShellStopError, ShellStopPreparation,
    ToolError,
};
use tokio_util::sync::CancellationToken;

use super::*;

pub(crate) struct EnvironmentShellHost {
    port: Result<Arc<dyn ExecutionPort>, ExecutionError>,
    authority: ExecutionAuthority,
    local: Arc<dyn ShellHost>,
}

impl EnvironmentShellHost {
    pub(crate) fn new(
        port: Result<Arc<dyn ExecutionPort>, ExecutionError>,
        authority: ExecutionAuthority,
        local: Arc<dyn ShellHost>,
    ) -> Self {
        Self {
            port,
            authority,
            local,
        }
    }
}

#[async_trait]
impl ShellHost for EnvironmentShellHost {
    async fn prepare_stop(
        &self,
        task: uuid::Uuid,
        reason: &str,
    ) -> Result<Option<ShellStopPreparation>, ToolError> {
        let port = self
            .port
            .as_ref()
            .map_err(|error| ToolError::Rejected(error.to_string()))?;
        if matches!(
            port.inspect(ExecutionQuery::Capabilities)
                .await
                .map_err(|error| ToolError::Rejected(error.to_string()))?,
            ExecutionInspection::Capabilities(ExecutionCapabilities { shells: false, .. })
        ) {
            return self.local.prepare_stop(task, reason).await;
        }
        prepare_stop(port.clone(), self.authority.clone(), task, reason)
            .await
            .map(Some)
    }
    async fn prepare(
        &self,
        request: ShellPreparationRequest,
    ) -> Result<Option<ShellPreparation>, ToolError> {
        let port = self
            .port
            .as_ref()
            .map_err(|error| ToolError::Rejected(error.to_string()))?;
        let ExecutionInspection::Capabilities(capabilities) = port
            .inspect(ExecutionQuery::Capabilities)
            .await
            .map_err(|error| ToolError::Rejected(error.to_string()))?
        else {
            return Err(ToolError::Rejected(
                "execution environment returned no capabilities".into(),
            ));
        };
        if !capabilities.shells {
            return self.local.prepare(request).await;
        }
        prepare_shell(
            port.clone(),
            self.authority.clone(),
            request,
            capabilities.shell_output,
        )
        .await
        .map(Some)
    }
    async fn start(&self, launch: ShellLaunch) -> Result<Box<dyn ShellProcess>, ShellStartError> {
        let port = self
            .port
            .as_ref()
            .map_err(|error| ShellStartError::NotStarted(ToolError::Rejected(error.to_string())))?;
        let capabilities = port
            .inspect(ExecutionQuery::Capabilities)
            .await
            .map_err(|error| ShellStartError::NotStarted(ToolError::Rejected(error.to_string())))?;
        if matches!(
            capabilities,
            ExecutionInspection::Capabilities(ExecutionCapabilities { shells: false, .. })
        ) {
            return self.local.start(launch).await;
        }
        let spec = shell_spec(&launch);
        let prepared = port
            .prepare(ExecutionIntent::SpawnShell(spec))
            .await
            .map_err(|error| ShellStartError::NotStarted(ToolError::Rejected(error.to_string())))?;
        let admission = self
            .authority
            .admit(&prepared)
            .map_err(|error| ShellStartError::NotStarted(ToolError::Rejected(error.to_string())))?;
        match port.execute(prepared, admission, launch.cancel).await {
            ExecutionOutcome::Completed(result) => {
                let status = result.shell.ok_or_else(|| {
                    ShellStartError::OutcomeUnknown("Shell start returned no instance".into())
                })?;
                let output = (status.output_delivery == ShellOutputDelivery::Environment)
                    .then_some(launch.output);
                Ok(Box::new(PortShellProcess::new(
                    port.clone(),
                    self.authority.clone(),
                    *status,
                    output,
                    result.handoff.map(ExecutionHandoff::into_value),
                )))
            }
            ExecutionOutcome::NotDispatched(error) => Err(start_refusal(error)),
            ExecutionOutcome::OutcomeUnknown { reason, .. } => {
                Err(ShellStartError::OutcomeUnknown(reason))
            }
        }
    }

    async fn adopt(&self, handoff: serde_json::Value) -> Result<Box<dyn ShellProcess>, ToolError> {
        self.local.adopt(handoff).await
    }
    async fn retained_handoff(
        &self,
        operation: uuid::Uuid,
    ) -> Result<Option<serde_json::Value>, ToolError> {
        self.local.retained_handoff(operation).await
    }
}

pub struct PortShellProcess {
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    instance: InstanceRef,
    pid: u32,
    observation: Option<ExecutionObservation>,
    output: Option<File>,
    output_offset: u64,
    handoff: Option<serde_json::Value>,
    ended: bool,
    stop_sent: bool,
}

impl PortShellProcess {
    pub fn new(
        port: Arc<dyn ExecutionPort>,
        authority: ExecutionAuthority,
        status: ShellStatus,
        output: Option<File>,
        handoff: Option<serde_json::Value>,
    ) -> Self {
        Self {
            port,
            authority,
            instance: status.instance,
            pid: status.pid.unwrap_or(0),
            observation: None,
            output,
            output_offset: 0,
            handoff,
            ended: false,
            stop_sent: false,
        }
    }

    async fn copy_output(&mut self) -> Result<(), ExecutionError> {
        while let Some(output) = &mut self.output {
            let ExecutionInspection::Output(page) = self
                .port
                .inspect(ExecutionQuery::ShellOutput {
                    instance: self.instance.clone(),
                    offset: self.output_offset,
                    limit: SHELL_OUTPUT_BYTES,
                })
                .await?
            else {
                return Err(ExecutionError::new(
                    "ProtocolViolation",
                    "Shell output query returned no data",
                ));
            };
            output
                .write_all(&page.data)
                .map_err(|error| ExecutionError::new("OutputUnavailable", error.to_string()))?;
            self.output_offset = page.next_offset;
            if page.eof {
                self.output = None;
            } else if page.data.is_empty() {
                return Err(ExecutionError::new(
                    "ProtocolViolation",
                    "Shell output cursor did not advance",
                ));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ShellProcess for PortShellProcess {
    fn pid(&self) -> u32 {
        self.pid
    }
    fn handoff(&self) -> Option<serde_json::Value> {
        self.handoff.clone()
    }

    async fn exited(&mut self) -> ShellExit {
        loop {
            let status = match self
                .port
                .inspect(ExecutionQuery::ShellStatus(self.instance.clone()))
                .await
            {
                Ok(ExecutionInspection::Shell(status)) => status,
                _ => {
                    self.ended = true;
                    return unknown_exit("the command's exit was never reported");
                }
            };
            if matches!(
                status.state,
                CommandState::Completed | CommandState::Failed | CommandState::Unknown
            ) {
                let mut reason = status.reason;
                if let Err(error) = self.copy_output().await {
                    reason.get_or_insert_with(|| error.to_string());
                }
                self.ended = true;
                return ShellExit {
                    exit_code: status.exit_code,
                    signal: status.signal,
                    state: status.state,
                    reason,
                };
            }
            if self.observation.is_none() {
                match self
                    .port
                    .observe(ObserveRequest {
                        instance: self.instance.clone(),
                        after: None,
                    })
                    .await
                {
                    Ok(observation) => self.observation = Some(observation),
                    Err(_) => {
                        self.ended = true;
                        return unknown_exit("the command's exit was never reported");
                    }
                }
                continue;
            }
            if self
                .observation
                .as_mut()
                .expect("registered observation")
                .updates
                .changed()
                .await
                .is_err()
            {
                self.ended = true;
                return unknown_exit("the command's exit was never reported");
            }
        }
    }

    async fn stop(&mut self, reason: &str) {
        let _ = self.stop_checked(reason, None).await;
    }
    async fn stop_checked(
        &mut self,
        reason: &str,
        before_dispatch: Option<&tickloop_harness_tools::ShellStopAdmission>,
    ) -> Result<(), ShellStopError> {
        let prepared = self
            .port
            .prepare(ExecutionIntent::StopShell {
                instance: self.instance.clone(),
                reason: reason.into(),
            })
            .await
            .map_err(|error| ShellStopError::NotDispatched(error.to_string()))?;
        let admission = self
            .authority
            .admit(&prepared)
            .map_err(|error| ShellStopError::NotDispatched(error.to_string()))?;
        if let Some(admit) = before_dispatch {
            admit()?;
        }
        self.stop_sent = true;
        match self
            .port
            .execute(prepared, admission, CancellationToken::new())
            .await
        {
            ExecutionOutcome::Completed(_) => Ok(()),
            ExecutionOutcome::NotDispatched(error) => {
                Err(ShellStopError::NotDispatched(error.to_string()))
            }
            ExecutionOutcome::OutcomeUnknown { reason, .. } => {
                Err(ShellStopError::OutcomeUnknown(reason))
            }
        }
    }
}

impl Drop for PortShellProcess {
    fn drop(&mut self) {
        if self.ended || self.stop_sent {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (port, authority, instance) = (
            self.port.clone(),
            self.authority.clone(),
            self.instance.clone(),
        );
        runtime.spawn(async move {
            if let Ok(prepared) = port
                .prepare(ExecutionIntent::StopShell {
                    instance,
                    reason: "Shell owner ended".into(),
                })
                .await
                && let Ok(admission) = authority.admit(&prepared)
            {
                let _ = port
                    .execute(prepared, admission, CancellationToken::new())
                    .await;
            }
        });
    }
}

fn unknown_exit(reason: &str) -> ShellExit {
    ShellExit {
        exit_code: None,
        signal: None,
        reason: Some(reason.into()),
        state: CommandState::Unknown,
    }
}

pub fn shell_spec(launch: &ShellLaunch) -> ShellSpec {
    ShellSpec {
        dangerously_disable_sandbox: false,
        additional_directories: Vec::new(),
        family_temp: None,
        operation: launch.operation_id,
        shell: launch.shell.to_string_lossy().into_owned(),
        command: launch.command.clone(),
        cwd: launch.cwd.to_string_lossy().into_owned(),
        output: launch.output_path.to_string_lossy().into_owned(),
        description: launch.description.clone(),
    }
}

pub async fn prepare_shell(
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    request: ShellPreparationRequest,
    delivery: ShellOutputDelivery,
) -> Result<ShellPreparation, ToolError> {
    let spec = ShellSpec {
        dangerously_disable_sandbox: request.dangerously_disable_sandbox,
        additional_directories: request
            .additional_directories
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        family_temp: request
            .family_temp
            .map(|path| path.to_string_lossy().into_owned()),
        operation: request.operation,
        shell: request.shell.to_string_lossy().into_owned(),
        command: request.command,
        cwd: request.cwd.to_string_lossy().into_owned(),
        output: request.output.to_string_lossy().into_owned(),
        description: request.description,
    };
    let prepared = port
        .prepare(ExecutionIntent::SpawnShell(spec))
        .await
        .map_err(|error| ToolError::Rejected(error.to_string()))?;
    let isolation = prepared.isolation();
    let requires_approval = prepared.requires_approval();
    let skips_approval_when_isolated = prepared.skips_approval_when_isolated();
    Ok(ShellPreparation::new(
        request.operation,
        delivery == ShellOutputDelivery::CallerFile,
        Box::new(PreparedEnvironmentShell {
            port,
            authority,
            prepared,
            delivery,
        }),
    )
    .with_isolation(isolation)
    .with_required_approval(requires_approval)
    .with_approval_skipped_when_isolated(skips_approval_when_isolated))
}

struct PreparedEnvironmentShell {
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    prepared: PreparedExecution,
    delivery: ShellOutputDelivery,
}

#[async_trait]
impl PreparedShellStart for PreparedEnvironmentShell {
    async fn start(
        self: Box<Self>,
        output: Option<File>,
        cancel: CancellationToken,
    ) -> Result<Box<dyn ShellProcess>, ShellStartError> {
        let admission = self
            .authority
            .admit(&self.prepared)
            .map_err(|error| ShellStartError::NotStarted(ToolError::Rejected(error.to_string())))?;
        match self.port.execute(self.prepared, admission, cancel).await {
            ExecutionOutcome::Completed(result) => {
                let status = result.shell.ok_or_else(|| {
                    ShellStartError::OutcomeUnknown("Shell start returned no instance".into())
                })?;
                if status.output_delivery != self.delivery {
                    return Err(ShellStartError::OutcomeUnknown(
                        "Shell output delivery changed after preparation".into(),
                    ));
                }
                Ok(Box::new(PortShellProcess::new(
                    self.port,
                    self.authority,
                    *status,
                    output,
                    result.handoff.map(ExecutionHandoff::into_value),
                )))
            }
            ExecutionOutcome::NotDispatched(error) => Err(start_refusal(error)),
            ExecutionOutcome::OutcomeUnknown { reason, .. } => {
                Err(ShellStartError::OutcomeUnknown(reason))
            }
        }
    }
}

pub async fn prepare_stop(
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    task: uuid::Uuid,
    reason: &str,
) -> Result<ShellStopPreparation, ToolError> {
    let prepared = port
        .prepare(ExecutionIntent::StopShell {
            instance: InstanceRef::shell(task),
            reason: reason.into(),
        })
        .await
        .map_err(|error| ToolError::Rejected(error.to_string()))?;
    Ok(ShellStopPreparation::new(
        task,
        Box::new(PreparedEnvironmentStop {
            port,
            authority,
            prepared,
        }),
    ))
}

struct PreparedEnvironmentStop {
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    prepared: PreparedExecution,
}
#[async_trait]
impl PreparedShellStop for PreparedEnvironmentStop {
    async fn stop(self: Box<Self>, cancel: CancellationToken) -> Result<(), ShellStopError> {
        let admission = self
            .authority
            .admit(&self.prepared)
            .map_err(|error| ShellStopError::NotDispatched(error.to_string()))?;
        match self.port.execute(self.prepared, admission, cancel).await {
            ExecutionOutcome::Completed(_) => Ok(()),
            ExecutionOutcome::NotDispatched(error) => {
                Err(ShellStopError::NotDispatched(error.to_string()))
            }
            ExecutionOutcome::OutcomeUnknown { reason, .. } => {
                Err(ShellStopError::OutcomeUnknown(reason))
            }
        }
    }
}

pub fn start_refusal(error: ExecutionError) -> ShellStartError {
    ShellStartError::NotStarted(if error.code == "Cancelled" {
        ToolError::Cancelled
    } else {
        ToolError::Rejected(error.message)
    })
}

pub(crate) struct CurrentLocalShellHost {
    agent: crate::WeakAgent,
    chat: uuid::Uuid,
}

impl CurrentLocalShellHost {
    pub(crate) fn new(agent: crate::WeakAgent, chat: uuid::Uuid) -> Self {
        Self { agent, chat }
    }
    fn current(&self) -> Result<Arc<dyn ShellHost>, ToolError> {
        let agent = self
            .agent
            .upgrade()
            .ok_or_else(|| ToolError::Rejected("Shell owner is unavailable".into()))?;
        Ok(agent
            .shell_runtime()
            .map(|runtime| runtime.for_chat(self.chat))
            .unwrap_or_else(|| Arc::new(UnavailableShellHost)))
    }
}

#[async_trait]
impl ShellHost for CurrentLocalShellHost {
    fn is_available(&self) -> bool {
        self.current().is_ok_and(|host| host.is_available())
    }
    async fn prepare(
        &self,
        request: ShellPreparationRequest,
    ) -> Result<Option<ShellPreparation>, ToolError> {
        self.current()?.prepare(request).await
    }
    async fn prepare_stop(
        &self,
        task: uuid::Uuid,
        reason: &str,
    ) -> Result<Option<ShellStopPreparation>, ToolError> {
        self.current()?.prepare_stop(task, reason).await
    }
    async fn start(&self, launch: ShellLaunch) -> Result<Box<dyn ShellProcess>, ShellStartError> {
        self.current()
            .map_err(ShellStartError::NotStarted)?
            .start(launch)
            .await
    }
    async fn adopt(&self, handoff: serde_json::Value) -> Result<Box<dyn ShellProcess>, ToolError> {
        self.current()?.adopt(handoff).await
    }
    async fn retained_handoff(
        &self,
        operation: uuid::Uuid,
    ) -> Result<Option<serde_json::Value>, ToolError> {
        self.current()?.retained_handoff(operation).await
    }
}

struct UnavailableShellHost;
#[async_trait]
impl ShellHost for UnavailableShellHost {
    fn is_available(&self) -> bool {
        false
    }
    async fn start(&self, _: ShellLaunch) -> Result<Box<dyn ShellProcess>, ShellStartError> {
        Err(ShellStartError::NotStarted(ToolError::Rejected(
            "this Agent has no Shell execution host".into(),
        )))
    }
}
