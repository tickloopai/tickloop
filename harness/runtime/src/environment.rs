//! Where a Chat's Folder, commands and Ticks live: its Execution Environment.

use std::sync::Arc;

use crate::host_tools::{HostCallResult, HostEffect, PreparedHostCall};
use tokio_util::sync::CancellationToken;

#[cfg(any(test, feature = "test-support"))]
pub mod memory;
mod port;
pub mod shell_host;
pub mod tools;
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

pub(crate) struct EnvironmentPreparedCall {
    port: Arc<dyn ExecutionPort>,
    prepared: PreparedExecution,
    authority: ExecutionAuthority,
    chat: tickloop_harness_protocol::ChatId,
}

impl EnvironmentPreparedCall {
    pub(crate) async fn revalidate(
        &self,
        chat: tickloop_harness_protocol::ChatId,
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
        chat: tickloop_harness_protocol::ChatId,
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
                    min_rows: opened.min_rows,
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

/// A Folder without Tick or Shell execution capabilities.
pub struct FolderEnvironment;
impl FolderEnvironment {
    pub fn bare() -> Self {
        Self
    }
}
impl ExecutionEnvironment for FolderEnvironment {
    fn bind(
        &self,
        scope: ExecutionScope,
        authority: ExecutionAuthority,
    ) -> Result<Arc<dyn ExecutionPort>, ExecutionError> {
        authority.check(scope.chat)?;
        Ok(Arc::new(FolderPort { scope, authority }))
    }
}
struct FolderPort {
    scope: ExecutionScope,
    authority: ExecutionAuthority,
}
#[async_trait::async_trait]
impl ExecutionPort for FolderPort {
    async fn inspect(&self, query: ExecutionQuery) -> Result<ExecutionInspection, ExecutionError> {
        self.authority.check(self.scope.chat)?;
        match query {
            ExecutionQuery::Capabilities => {
                Ok(ExecutionInspection::Capabilities(ExecutionCapabilities {
                    ticks: false,
                    shells: false,
                    shell_output: ShellOutputDelivery::CallerFile,
                }))
            }
            ExecutionQuery::Instances => Ok(ExecutionInspection::Instances(Vec::new())),
            _ => Err(ExecutionError::new(
                "Unsupported",
                "this environment has no execution host",
            )),
        }
    }
    async fn prepare(&self, _: ExecutionIntent) -> Result<PreparedExecution, ExecutionError> {
        self.authority.check(self.scope.chat)?;
        Err(ExecutionError::new(
            "Unsupported",
            "this environment has no execution host",
        ))
    }
    async fn execute(
        &self,
        _: PreparedExecution,
        _: ExecutionAdmission,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        ExecutionOutcome::NotDispatched(ExecutionError::new(
            "Unsupported",
            "this environment has no execution host",
        ))
    }
    async fn observe(&self, _: ObserveRequest) -> Result<ExecutionObservation, ExecutionError> {
        self.authority.check(self.scope.chat)?;
        Err(ExecutionError::new(
            "Unsupported",
            "this environment has no execution host",
        ))
    }
}
