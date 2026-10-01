use std::{any::Any, path::PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{model::ToolDefinition, runtime::ChatId, tools::ToolResult};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostCallContext {
    pub workdir: PathBuf,
    pub workspace_id: Option<String>,
    pub interaction: Option<HerdrInteraction>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HerdrInteraction {
    pub socket: PathBuf,
    pub pane: tick_connect::herdr::Pane,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostEndpointIdentity {
    HerdrSocket {
        path: PathBuf,
        owner: u32,
        device: u64,
        inode: u64,
    },
    TrustedAdapter {
        scope: uuid::Uuid,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum HostToolPermission {
    ReadOnly,
    Automatic,
    /// A mutation governed by the Chat execution mode and ordinary approval policy.
    Reviewable,
    /// A host requirement for human confirmation, independent of execution mode.
    Confirmation,
}

/// One invocation's resolved operation. Ownership is transferred to dispatch exactly once.
pub struct PreparedHostCall {
    name: String,
    source_input: Value,
    input: Value,
    permission: HostToolPermission,
    approval_summary: String,
    payload: Option<Box<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for PreparedHostCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedHostCall")
            .field("name", &self.name)
            .field("input", &self.input)
            .field("permission", &self.permission)
            .finish_non_exhaustive()
    }
}

impl PreparedHostCall {
    pub fn new(
        name: String,
        source_input: Value,
        input: Value,
        permission: HostToolPermission,
        approval_summary: String,
    ) -> Self {
        Self {
            name,
            source_input,
            input,
            permission,
            approval_summary,
            payload: None,
        }
    }

    pub fn with_payload<T: Any + Send + Sync>(mut self, payload: T) -> Self {
        self.payload = Some(Box::new(payload));
        self
    }

    pub fn payload<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.payload.as_ref()?.downcast_ref::<T>()
    }

    pub fn into_payload<T: Any + Send + Sync>(self) -> Result<T, Box<Self>> {
        let Self { payload, .. } = &self;
        if payload.as_ref().is_some_and(|payload| payload.is::<T>()) {
            Ok(*self
                .payload
                .expect("checked payload type")
                .downcast::<T>()
                .expect("checked payload type"))
        } else {
            Err(Box::new(self))
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn source_input(&self) -> &Value {
        &self.source_input
    }
    pub fn input(&self) -> &Value {
        &self.input
    }
    pub fn permission(&self) -> HostToolPermission {
        self.permission
    }
    pub fn approval_summary(&self) -> &str {
        &self.approval_summary
    }
}

pub enum HostCallResult {
    Completed(ToolResult),
    NotDispatched(String),
    OutcomeUnknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostCallStatus {
    Completed,
    NotDispatched,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceHostName {
    Herdr,
    Tmux,
    Zellij,
    Standalone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceIntegration {
    Available,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceHostContext {
    pub name: WorkspaceHostName,
    pub integration: WorkspaceIntegration,
    pub tick_executable: Option<PathBuf>,
}

impl WorkspaceHostContext {
    fn detected(herdr: bool, tmux: bool, zellij: bool) -> Self {
        let name = if herdr {
            WorkspaceHostName::Herdr
        } else if tmux {
            WorkspaceHostName::Tmux
        } else if zellij {
            WorkspaceHostName::Zellij
        } else {
            WorkspaceHostName::Standalone
        };
        Self {
            name,
            integration: WorkspaceIntegration::Unavailable,
            tick_executable: None,
        }
    }

    pub fn attached_herdr(tick_executable: Option<PathBuf>) -> Self {
        Self {
            name: WorkspaceHostName::Herdr,
            integration: WorkspaceIntegration::Available,
            tick_executable,
        }
    }
}

pub fn current_workspace_host(provider: Option<&dyn HostToolProvider>) -> WorkspaceHostContext {
    if let Some(context) = provider.and_then(HostToolProvider::workspace_host_context) {
        return context;
    }
    WorkspaceHostContext::detected(
        std::env::var_os("HERDR_ENV").is_some_and(|value| value == "1"),
        std::env::var_os("TMUX").is_some_and(|value| !value.is_empty()),
        std::env::var_os("ZELLIJ").is_some(),
    )
}

impl HostCallResult {
    pub fn status(&self) -> HostCallStatus {
        match self {
            Self::Completed(_) => HostCallStatus::Completed,
            Self::NotDispatched(_) => HostCallStatus::NotDispatched,
            Self::OutcomeUnknown(_) => HostCallStatus::OutcomeUnknown,
        }
    }

    pub fn into_tool_result(self) -> ToolResult {
        match self {
            Self::Completed(result) => result,
            Self::NotDispatched(message) => host_error_result(message),
            Self::OutcomeUnknown(message) => host_error_result(format!(
                "host operation outcome unknown after dispatch: {message}"
            )),
        }
    }
}

#[async_trait]
pub trait HostToolProvider: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        Ok(None)
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        None
    }

    fn permission(&self, chat_id: ChatId, name: &str, input: &Value) -> HostToolPermission;

    async fn prepare(
        &self,
        chat_id: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<HostToolPermission, String> {
        if cancel.is_cancelled() {
            return Err("host operation was cancelled".into());
        }
        Ok(self.permission(chat_id, name, input))
    }

    async fn prepare_call(
        &self,
        chat_id: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let permission = self.prepare(chat_id, name, input, cancel).await?;
        Ok(PreparedHostCall::new(
            name.to_owned(),
            input.clone(),
            input.clone(),
            permission,
            format!("{name}: {input}"),
        ))
    }

    async fn prepare_call_in_context(
        &self,
        chat_id: ChatId,
        _context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.prepare_call(chat_id, name, input, cancel).await
    }

    async fn revalidate_call(
        &self,
        chat_id: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        self.revalidate(
            chat_id,
            call.name(),
            call.input(),
            call.permission(),
            cancel,
        )
        .await
    }

    async fn execute_prepared(
        &self,
        chat_id: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched("host operation was cancelled".into());
        }
        if let Err(error) = self.revalidate_call(chat_id, &call, cancel.clone()).await {
            return HostCallResult::NotDispatched(error);
        }
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched("host operation was cancelled".into());
        }
        HostCallResult::Completed(
            self.execute(chat_id, call.name(), call.input().clone(), cancel)
                .await,
        )
    }

    async fn revalidate(
        &self,
        chat_id: ChatId,
        name: &str,
        input: &Value,
        expected_permission: HostToolPermission,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("host operation was cancelled".into());
        }
        if self.permission(chat_id, name, input) != expected_permission {
            return Err("host operation permission changed before execution".into());
        }
        Ok(())
    }

    async fn execute(
        &self,
        chat_id: ChatId,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult;
}

fn host_error_result(content: String) -> ToolResult {
    ToolResult {
        content,
        is_error: true,
        output_ref: None,
    }
}

#[cfg(test)]
mod workspace_host_tests {
    use super::*;

    #[test]
    fn nested_workspace_hosts_use_herdr_then_tmux_then_zellij() {
        assert_eq!(
            WorkspaceHostContext::detected(true, true, true).name,
            WorkspaceHostName::Herdr
        );
        assert_eq!(
            WorkspaceHostContext::detected(false, true, true).name,
            WorkspaceHostName::Tmux
        );
        assert_eq!(
            WorkspaceHostContext::detected(false, false, true).name,
            WorkspaceHostName::Zellij
        );
        assert_eq!(
            WorkspaceHostContext::detected(false, false, false).name,
            WorkspaceHostName::Standalone
        );
    }
}
