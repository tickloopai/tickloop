use std::{any::Any, path::PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{model::ToolDefinition, runtime::ChatId, tools::ToolResult};

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
    permission_input: Value,
    permission: HostToolPermission,
    approval_summary: String,
    payload: Option<Box<dyn Any + Send + Sync>>,
    event_context: Option<EventToolContext>,
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
            permission_input: input.clone(),
            input,
            permission,
            approval_summary,
            payload: None,
            event_context: None,
        }
    }

    pub fn with_payload<T: Any + Send + Sync>(mut self, payload: T) -> Self {
        self.payload = Some(Box::new(payload));
        self
    }

    pub(crate) fn with_event_context(mut self, context: EventToolContext) -> Self {
        self.event_context = Some(context);
        self
    }

    pub(crate) fn event_context(&self) -> Option<&EventToolContext> {
        self.event_context.as_ref()
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
    pub fn permission_input(&self) -> &Value {
        &self.permission_input
    }
    pub fn input(&self) -> &Value {
        &self.input
    }
    pub fn with_permission_input(mut self, permission_input: Value) -> Self {
        self.permission_input = permission_input;
        self
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
    CompletedWithEffect(ToolResult, Box<HostEffect>),
    NotDispatched(String),
    OutcomeUnknown(String),
}

#[derive(Clone, Debug)]
pub enum HostEffect {
    PublishedSource(PublishedSourceEffect),
    /// A strictly validated connection read. It carries the private service envelope, so it is
    /// never serialized; Runtime turns it into the model-visible result.
    ConnectSource(ConnectSourceEffect),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishedSourceEffect {
    pub server_id: String,
    pub owner_account_id: String,
    pub source_key: String,
    pub operation_id: String,
    pub version_id: String,
    pub credential_generation: [u8; 32],
    pub directory_fingerprint: [u8; 32],
    pub bridge_fingerprint: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectAction {
    Connect { replace_existing: bool },
    Check,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteAssociation {
    pub binding_id: String,
    pub client_instance_id: String,
    pub chat_id: String,
    pub loop_id: String,
    pub association_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteAssociationOperation {
    Pending,
    Committed { association_epoch: u64 },
    Failed { error_code: Option<String> },
    Unknown,
}

#[derive(Clone, Debug)]
pub struct ConnectSourceEffect {
    /// The model-visible identity fields under the service's mapped argument names.
    pub result_identity: serde_json::Map<String, Value>,
    pub server_id: String,
    pub owner_account_id: String,
    pub source_key: String,
    pub operation_id: String,
    pub action: ConnectAction,
    pub current_association: Option<RemoteAssociation>,
    pub association_operation: Option<RemoteAssociationOperation>,
    pub credential_generation: [u8; 32],
    pub directory_fingerprint: [u8; 32],
    pub bridge_fingerprint: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct EventAssociationRequest {
    pub server_id: String,
    pub owner_account_id: String,
    pub source_key: String,
    pub binding_id: uuid::Uuid,
    pub client_instance_id: uuid::Uuid,
    pub chat_id: ChatId,
    pub loop_id: uuid::Uuid,
    pub operation_id: String,
    pub expected_epoch: Option<u64>,
    pub credential_generation: [u8; 32],
    pub directory_fingerprint: [u8; 32],
    pub bridge_fingerprint: [u8; 32],
}

pub enum EventAssociationOutcome {
    Committed(u64),
    NotDispatched(String),
    OutcomeUnknown(String),
    /// A complete, authenticated, definite rejection carrying its public error code.
    Rejected(String),
}

#[derive(Clone, Debug)]
pub struct EventSourceLocator {
    pub server_id: String,
    pub owner_account_id: String,
    pub source_key: String,
    pub binding_id: uuid::Uuid,
    pub client_instance_id: uuid::Uuid,
    pub chat_id: ChatId,
    pub loop_id: uuid::Uuid,
    pub association_epoch: u64,
    pub credential_generation: [u8; 32],
    pub directory_fingerprint: [u8; 32],
    pub bridge_fingerprint: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct PendingEvent {
    pub event_id: String,
}

pub enum EventPendingOutcome {
    Events(Vec<PendingEvent>, Option<String>),
    Unavailable(String),
}

#[derive(Clone, Debug)]
pub struct EventClaimRequest {
    pub source: EventSourceLocator,
    pub event_id: String,
    pub operation_id: String,
}

#[derive(Clone, Debug)]
pub struct ClaimedEvent {
    pub event_id: String,
    pub claim_id: String,
    pub fencing_token: String,
    pub lease_expires_at: String,
    pub input: Value,
}

#[derive(Clone)]
pub(crate) struct EventToolContext {
    pub source: EventSourceLocator,
    pub event_id: String,
    pub claim_id: String,
    pub fencing_token: String,
}

pub enum EventClaimOutcome {
    Claimed(ClaimedEvent),
    Busy,
    NeedsAttention,
    NotDispatched(String),
    OutcomeUnknown(String),
}

#[derive(Clone)]
pub struct EventLeaseRequest {
    pub source: EventSourceLocator,
    pub event_id: String,
    pub claim_id: String,
    pub fencing_token: String,
    pub operation_id: String,
}

pub enum EventLeaseOutcome {
    Committed(Option<String>),
    Completed,
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
            Self::Completed(_) | Self::CompletedWithEffect(_, _) => HostCallStatus::Completed,
            Self::NotDispatched(_) => HostCallStatus::NotDispatched,
            Self::OutcomeUnknown(_) => HostCallStatus::OutcomeUnknown,
        }
    }

    pub fn into_tool_result(self) -> ToolResult {
        match self {
            Self::Completed(result) => result,
            Self::CompletedWithEffect(result, _) => result,
            Self::NotDispatched(message) => host_error_result(message),
            Self::OutcomeUnknown(message) => host_error_result(format!(
                "host operation outcome unknown after dispatch: {message}"
            )),
        }
    }

    pub fn into_parts(self) -> (ToolResult, Option<HostEffect>) {
        match self {
            Self::CompletedWithEffect(result, effect) => (result, Some(*effect)),
            other => (other.into_tool_result(), None),
        }
    }
}

#[async_trait]
pub trait HostToolProvider: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    async fn associate_event_source(
        &self,
        _request: EventAssociationRequest,
        _cancel: CancellationToken,
    ) -> EventAssociationOutcome {
        EventAssociationOutcome::NotDispatched("MCP event bridge is unavailable".into())
    }

    async fn refresh_event_source(
        &self,
        _source: EventSourceLocator,
    ) -> Result<EventSourceLocator, String> {
        Err("MCP event bridge is unavailable".into())
    }

    fn event_source_credentials_match(&self, _source: &EventSourceLocator) -> bool {
        false
    }

    async fn pending_event_source(
        &self,
        _source: EventSourceLocator,
        _cursor: Option<String>,
        _cancel: CancellationToken,
    ) -> EventPendingOutcome {
        EventPendingOutcome::Unavailable("MCP event bridge is unavailable".into())
    }

    async fn claim_event_source(
        &self,
        _request: EventClaimRequest,
        _cancel: CancellationToken,
    ) -> EventClaimOutcome {
        EventClaimOutcome::NotDispatched("MCP event bridge is unavailable".into())
    }

    async fn renew_event_claim(
        &self,
        _request: EventLeaseRequest,
        _cancel: CancellationToken,
    ) -> EventLeaseOutcome {
        EventLeaseOutcome::NotDispatched("MCP event bridge is unavailable".into())
    }

    async fn release_event_claim(
        &self,
        _request: EventLeaseRequest,
        _cancel: CancellationToken,
    ) -> EventLeaseOutcome {
        EventLeaseOutcome::NotDispatched("MCP event bridge is unavailable".into())
    }

    async fn refresh(&self, _cancel: CancellationToken) -> Result<(), String> {
        Ok(())
    }

    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        crate::runtime::validate_host_schema_input(name, schema, input)
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

/// A model-visible connection outcome. Only the documented status and its public fields are
/// included; private envelope data never reaches this result.
pub fn connection_result(
    identity: &serde_json::Map<String, Value>,
    status: &str,
    fields: Value,
    is_error: bool,
    summary: &str,
) -> ToolResult {
    let mut structured = identity.clone();
    structured.insert("connection_status".into(), Value::String(status.into()));
    if let Value::Object(fields) = fields {
        structured.extend(fields);
    }
    ToolResult {
        content: serde_json::json!({
            "content": [{"type": "text", "text": summary}],
            "structuredContent": structured,
            "isError": is_error,
        })
        .to_string(),
        is_error,
        output_ref: None,
    }
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
