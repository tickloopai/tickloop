mod composite;
pub(crate) use composite::CompositeHostTools;
pub use composite::IntoHostToolSources;
use std::{any::Any, collections::BTreeSet, path::PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use tickloop_harness_protocol::{
    ChatId, HostCallContext, HostEndpointIdentity, HostToolAuthority, ToolDefinition, ToolResult,
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum HostToolPermission {
    ReadOnly,
    Automatic,
    /// Agent actions reviewed in Auto and Plan. Claude's delegation review also
    /// runs after an ordinary Allow; an allowed SendMessage skips review.
    Classified {
        allow_rule_bypasses_review: bool,
    },
    /// A private document write from the fixed Artifact service, without runtime capabilities.
    PrivateArtifactWrite,
    /// The first write to a publicly shared Artifact in this conversation.
    PublicArtifactWrite,
    /// A subsequent write to an Artifact with the same admitted audience in this conversation.
    ArtifactContinuation,
    /// A mutation governed by the Chat execution mode and ordinary approval policy.
    Reviewable,
    /// A host requirement for human confirmation, independent of execution mode.
    Confirmation,
    /// Third-party tools ask by default in Ask and Auto, regardless of server annotations.
    ThirdParty,
}

struct HostPolicyIdentity {
    name: String,
    input: Value,
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
    artifact_account: Option<ArtifactAccountContext>,
    policy_identity: Option<HostPolicyIdentity>,
    approval_facts: tickloop_harness_protocol::PreparedApprovalFacts,
    local_read: Option<(tickloop_harness_tools::PreparedTool, bool)>,
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
            approval_facts: tickloop_harness_protocol::PreparedApprovalFacts {
                normalized_input: input.clone(),
                ..Default::default()
            },
            name,
            source_input,
            permission_input: input.clone(),
            input,
            permission,
            approval_summary,
            payload: None,
            artifact_account: None,
            policy_identity: None,
            local_read: None,
        }
    }

    pub fn with_payload<T: Any + Send + Sync>(mut self, payload: T) -> Self {
        self.payload = Some(Box::new(payload));
        self
    }

    pub fn with_approval_facts(
        mut self,
        facts: tickloop_harness_protocol::PreparedApprovalFacts,
    ) -> Self {
        self.approval_facts = facts;
        self
    }

    pub fn approval_facts(&self) -> &tickloop_harness_protocol::PreparedApprovalFacts {
        &self.approval_facts
    }

    /// The call sends a local file the Chat admitted a `Read` of; `read_asks` reports an explicit
    /// Ask on that `Read`, which the call's one approval must satisfy. The runtime checks the
    /// `Read` again before dispatch.
    pub fn with_local_read(
        mut self,
        read: tickloop_harness_tools::PreparedTool,
        read_asks: bool,
    ) -> Self {
        self.local_read = Some((read, read_asks));
        self
    }

    pub fn local_read(&self) -> Option<&tickloop_harness_tools::PreparedTool> {
        self.local_read.as_ref().map(|(read, _)| read)
    }

    pub fn local_read_asks(&self) -> bool {
        self.local_read.as_ref().is_some_and(|(_, asks)| *asks)
    }

    pub(crate) fn with_artifact_account(mut self, context: ArtifactAccountContext) -> Self {
        self.artifact_account = Some(context);
        self
    }

    pub(crate) fn artifact_account(&self) -> Option<&ArtifactAccountContext> {
        self.artifact_account.as_ref()
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
    pub fn policy_name(&self) -> &str {
        self.policy_identity
            .as_ref()
            .map_or(&self.name, |identity| &identity.name)
    }
    pub fn policy_input(&self) -> &Value {
        self.policy_identity
            .as_ref()
            .map_or(&self.input, |identity| &identity.input)
    }
    /// Subscription admission uses the native query's resolved policy identity and arguments.
    pub fn with_policy_identity(mut self, name: String, input: Value) -> Self {
        self.policy_identity = Some(HostPolicyIdentity { name, input });
        self
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
    CompletedWithImages(ToolResult, Vec<tickloop_harness_protocol::Image>),
    CompletedWithEffect(ToolResult, Box<HostEffect>),
    NotDispatched(String),
    OutcomeUnknown(String),
}

#[derive(Clone, Debug)]
pub enum HostEffect {
    /// A confirmed `ArtifactEdit` create; the new artifact joins Monitor and is linked to the
    /// calling Loop.
    CreatedArtifact(tickloop_harness_protocol::ArtifactTick),
    /// A verified body read joins Monitor without linking the artifact to a Loop.
    ReadArtifact(tickloop_harness_protocol::ArtifactTick),
    /// A confirmed `ArtifactEdit` rename; Monitor shows the artifact's new title without waiting
    /// for its metadata to be read again.
    RenamedArtifact {
        instance: tick_protocol::InstanceRef,
        title: String,
    },
    /// A verified open created this Tick Instance for the calling Chat. It joins that Chat's
    /// Monitor without subscribing; a failed or unknown open never produces this effect.
    OpenedTick {
        is_terminal: bool,
        instance: tick_protocol::InstanceRef,
        title: Option<String>,
    },
}

/// The Artifact account that supplied an input; switching credentials cannot redirect its writes.
#[derive(Clone)]
pub(crate) struct ArtifactAccountContext {
    pub origin: String,
    pub account_id: String,
}

/// What the calling Chat lends one host call: read-only access to its local files by the `Read`
/// path rules in its workspace, and the host context of the View or Work Context it serves.
pub struct HostCallScope<'a> {
    /// The calling Chat's local files; `None` for a Chat without a Folder.
    pub local_files: Option<&'a tickloop_harness_tools::LocalFileReader>,
    pub context: Option<&'a HostCallContext>,
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
    /// Whether this Agent can show its Ticks in the container's Panes.
    pub integration: WorkspaceIntegration,
    /// Whether this Agent can run Ticks in the Tickhost, in any container.
    pub tickhost: WorkspaceIntegration,
    /// The installed plugins, including disabled plugins.
    pub plugins: Vec<WorkspacePlugin>,
}

/// An installed plugin, as the Agent sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspacePlugin {
    pub id: String,
    pub enabled: bool,
    pub has_tick: bool,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub executable: Option<PathBuf>,
    /// Its login state, from the credentials Tickloop saved for it.
    pub login: WorkspacePluginLogin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePluginLogin {
    LoggedIn,
    LoggedOut,
    Expired,
    NotRequired,
    Unknown,
}

impl WorkspaceHostContext {
    /// The container this process runs in, as its environment says, with no integration.
    pub fn from_environment() -> Self {
        Self::detected(
            std::env::var_os("HERDR_ENV").is_some_and(|value| value == "1"),
            std::env::var_os("TMUX").is_some_and(|value| !value.is_empty()),
            std::env::var_os("ZELLIJ").is_some(),
        )
    }

    /// The same container, where this Agent can also run Ticks in the Tickhost.
    pub fn with_tickhost(mut self) -> Self {
        self.tickhost = WorkspaceIntegration::Available;
        self
    }

    /// Herdr, where this Agent can also show its Ticks in Panes.
    pub fn with_herdr_views(mut self) -> Self {
        self.name = WorkspaceHostName::Herdr;
        self.integration = WorkspaceIntegration::Available;
        self
    }

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
            tickhost: WorkspaceIntegration::Unavailable,
            plugins: Vec::new(),
        }
    }

    /// Whether this Agent can open or operate any Tick outside Shell and Artifact.
    pub fn runs_ticks(&self) -> bool {
        self.tickhost == WorkspaceIntegration::Available
    }
}

pub fn current_workspace_host(provider: Option<&dyn HostToolProvider>) -> WorkspaceHostContext {
    provider
        .and_then(HostToolProvider::workspace_host_context)
        .unwrap_or_else(WorkspaceHostContext::from_environment)
}

impl HostCallResult {
    pub fn status(&self) -> HostCallStatus {
        match self {
            Self::Completed(_)
            | Self::CompletedWithImages(_, _)
            | Self::CompletedWithEffect(_, _) => HostCallStatus::Completed,
            Self::NotDispatched(_) => HostCallStatus::NotDispatched,
            Self::OutcomeUnknown(_) => HostCallStatus::OutcomeUnknown,
        }
    }

    pub fn into_tool_result(self) -> ToolResult {
        match self {
            Self::Completed(result) | Self::CompletedWithImages(result, _) => result,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpResourceServer {
    pub name: String,
    pub authority: HostToolAuthority,
    /// A server the host connects to for the user, as opposed to Tickloop Artifacts; its
    /// resource results follow the same size rule as its `mcp__` tool results.
    pub third_party: bool,
}

/// The instructions an MCP server gave when the host connected to it for an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerInstructions {
    pub server: McpResourceServer,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct HostToolUpgradeCheckpoint {
    pub state: Value,
}

#[async_trait]
pub trait HostToolProvider: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Private process-transfer state. `None` defers replacement while the provider owns an
    /// interaction that cannot move, such as a pending device sign-in.
    fn upgrade_checkpoint(&self) -> Result<Option<Value>, String> {
        Ok(Some(Value::Null))
    }

    fn restore_upgrade(&self, state: Value) -> Result<(), String> {
        if state.is_null() {
            Ok(())
        } else {
            Err("this host tool provider cannot restore the supplied upgrade state".into())
        }
    }

    /// Scheduling capability only; preparation and permission checks remain authoritative.
    fn is_concurrency_safe(&self, _name: &str, _input: &Value) -> bool {
        false
    }

    /// The Tickloop Artifacts client whose account the MCP tools act for; it also carries the
    /// feedback request lifecycle.
    fn artifact_client(
        &self,
    ) -> Option<std::sync::Arc<tickloop_harness_artifacts::ArtifactClient>> {
        None
    }

    /// Refreshes optional host capabilities. An error reports a failed refresh; callers may
    /// continue chatting with the remaining definitions. MCP directory failures withdraw those
    /// tools and return a diagnostic containing only the origin and a fixed error class.
    async fn refresh(&self, _cancel: CancellationToken) -> Result<(), String> {
        Ok(())
    }

    fn mcp_resource_server(&self) -> Option<McpResourceServer> {
        None
    }

    fn mcp_resource_servers(&self) -> Vec<McpResourceServer> {
        self.mcp_resource_server().into_iter().collect()
    }

    async fn list_mcp_resources_for(
        &self,
        server: &str,
        cursor: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        if !self
            .mcp_resource_servers()
            .iter()
            .any(|source| source.name == server)
        {
            return Err("MCP resource server unavailable".into());
        }
        self.list_mcp_resources(cursor, cancel).await
    }

    async fn read_mcp_resource_for(
        &self,
        server: &str,
        uri: &str,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        if !self
            .mcp_resource_servers()
            .iter()
            .any(|source| source.name == server)
        {
            return Err("MCP resource server unavailable".into());
        }
        self.read_mcp_resource(uri, cancel).await
    }

    /// The instructions of the MCP servers whose tools this provider currently offers.
    fn mcp_instructions(&self) -> Vec<McpServerInstructions> {
        Vec::new()
    }

    async fn list_mcp_resources(
        &self,
        _cursor: Option<&str>,
        _cancel: CancellationToken,
    ) -> Result<Value, String> {
        Err("MCP resources are unavailable".into())
    }

    async fn read_mcp_resource(
        &self,
        _uri: &str,
        _cancel: CancellationToken,
    ) -> Result<Value, String> {
        Err("MCP resources are unavailable".into())
    }

    fn remote_skills(&self) -> Vec<tickloop_harness_protocol::RemoteSkillDefinition> {
        Vec::new()
    }

    fn remote_skill_is_current(&self, _source: &tickloop_harness_protocol::SkillSource) -> bool {
        false
    }

    async fn read_remote_skill(
        &self,
        _definition: &tickloop_harness_protocol::RemoteSkillDefinition,
        _resource: Option<&str>,
        _cancel: CancellationToken,
    ) -> Result<String, String> {
        Err("remote Skills are unavailable".into())
    }

    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        crate::validate_host_schema_input(name, schema, input)
    }

    /// The endpoint of a provider that one host serves; a provider routing to several hosts
    /// reports each call's own through [`Self::call_authority`].
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        Ok(None)
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        None
    }

    /// Every authority through which this provider can currently serve `name`, one per
    /// backend. Delegation freezes this set; a backend that appears later is never added, apart
    /// from the first Artifact account a child delegated before any sign-in takes up.
    fn tool_authorities(&self, _name: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        Ok(self
            .host_endpoint()?
            .map(|endpoint| HostToolAuthority::Native { endpoint })
            .into_iter()
            .collect())
    }

    /// The authority of the backend that serves this invocation, chosen from its input alone
    /// before any host resolves its target; `None` when no backend can serve it.
    fn call_authority(
        &self,
        _name: &str,
        _input: &Value,
    ) -> Result<Option<HostToolAuthority>, String> {
        Ok(self
            .host_endpoint()?
            .map(|endpoint| HostToolAuthority::Native { endpoint }))
    }

    fn service_connections(&self) -> Value {
        serde_json::json!({})
    }

    /// Whether [`Self::definitions`] reflects confirmed service state. While a transient failure
    /// leaves it unconfirmed, the host keeps the tool set it last announced to the model.
    fn tool_set_is_confirmed(&self) -> bool {
        true
    }

    /// Ends the work the provider keeps running between tool calls, and waits for it, when its
    /// Agent shuts down.
    async fn shutdown(&self) {}

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

    async fn prepare_call_in_scope(
        &self,
        chat_id: ChatId,
        _scope: &HostCallScope<'_>,
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
