use super::*;
use std::sync::Arc;

pub trait IntoHostToolSources {
    fn into_sources(self) -> Vec<Arc<dyn HostToolProvider>>;
}
impl<T: HostToolProvider + 'static> IntoHostToolSources for Arc<T> {
    fn into_sources(self) -> Vec<Arc<dyn HostToolProvider>> {
        vec![self]
    }
}
impl IntoHostToolSources for Arc<dyn HostToolProvider> {
    fn into_sources(self) -> Vec<Arc<dyn HostToolProvider>> {
        vec![self]
    }
}
impl IntoHostToolSources for Vec<Arc<dyn HostToolProvider>> {
    fn into_sources(self) -> Vec<Arc<dyn HostToolProvider>> {
        self
    }
}

pub(crate) struct CompositeHostTools {
    sources: Vec<Arc<dyn HostToolProvider>>,
}
impl CompositeHostTools {
    pub(crate) fn combine(
        mut sources: Vec<Arc<dyn HostToolProvider>>,
    ) -> Arc<dyn HostToolProvider> {
        if sources.len() == 1 {
            return sources.pop().unwrap();
        }
        Arc::new(Self { sources })
    }
    fn source(&self, name: &str) -> Result<&Arc<dyn HostToolProvider>, String> {
        let mut matches = self
            .sources
            .iter()
            .filter(|source| source.definitions().iter().any(|tool| tool.name == name));
        let source = matches.next().ok_or("host tool unavailable")?;
        if matches.next().is_some() {
            return Err("host tool has conflicting sources".into());
        }
        Ok(source)
    }
    fn authority_source(&self, name: &str) -> Result<Option<&Arc<dyn HostToolProvider>>, String> {
        let artifact = crate::mcp_tools::ARTIFACT_ACCOUNT_TOOL_NAMES.contains(&name);
        let mut matches = self.sources.iter().filter(|source| {
            source.definitions().iter().any(|tool| tool.name == name)
                || artifact && source.artifact_client().is_some()
        });
        let source = matches.next();
        if matches.next().is_some() {
            return Err("host tool has conflicting authorities".into());
        }
        Ok(source)
    }
    fn resource_source(&self, server: &str) -> Result<&Arc<dyn HostToolProvider>, String> {
        let mut matches = self.sources.iter().filter(|source| {
            source
                .mcp_resource_servers()
                .iter()
                .any(|s| s.name == server)
        });
        let source = matches.next().ok_or("MCP resource server unavailable")?;
        if matches.next().is_some() {
            return Err("MCP resource server has conflicting sources".into());
        }
        Ok(source)
    }
    fn skill_source(
        &self,
        skill: &tickloop_harness_protocol::SkillSource,
    ) -> Option<&Arc<dyn HostToolProvider>> {
        self.sources
            .iter()
            .find(|source| source.remote_skill_is_current(skill))
    }
}
#[async_trait]
impl HostToolProvider for CompositeHostTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let all: Vec<_> = self
            .sources
            .iter()
            .flat_map(|source| source.definitions())
            .collect();
        all.iter()
            .filter(|tool| all.iter().filter(|other| other.name == tool.name).count() == 1)
            .cloned()
            .collect()
    }
    fn upgrade_checkpoint(&self) -> Result<Option<Value>, String> {
        let mut states = Vec::new();
        for source in &self.sources {
            let Some(state) = source.upgrade_checkpoint()? else {
                return Ok(None);
            };
            states.push(state);
        }
        Ok(Some(Value::Array(states)))
    }
    fn restore_upgrade(&self, state: Value) -> Result<(), String> {
        let states = state
            .as_array()
            .filter(|states| states.len() == self.sources.len())
            .ok_or("host source upgrade state changed")?;
        for (source, state) in self.sources.iter().zip(states) {
            source.restore_upgrade(state.clone())?;
        }
        Ok(())
    }
    fn is_concurrency_safe(&self, name: &str, input: &Value) -> bool {
        self.source(name)
            .is_ok_and(|s| s.is_concurrency_safe(name, input))
    }
    fn artifact_client(&self) -> Option<Arc<tickloop_harness_artifacts::ArtifactClient>> {
        self.sources.iter().find_map(|s| s.artifact_client())
    }
    async fn refresh(&self, cancel: CancellationToken) -> Result<(), String> {
        let results =
            futures_util::future::join_all(self.sources.iter().map(|s| s.refresh(cancel.clone())))
                .await;
        results.into_iter().collect()
    }
    fn mcp_resource_servers(&self) -> Vec<McpResourceServer> {
        self.sources
            .iter()
            .flat_map(|s| s.mcp_resource_servers())
            .collect()
    }
    fn mcp_instructions(&self) -> Vec<McpServerInstructions> {
        self.sources
            .iter()
            .flat_map(|s| s.mcp_instructions())
            .collect()
    }
    async fn list_mcp_resources_for(
        &self,
        server: &str,
        cursor: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.resource_source(server)?
            .list_mcp_resources_for(server, cursor, cancel)
            .await
    }
    async fn read_mcp_resource_for(
        &self,
        server: &str,
        uri: &str,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.resource_source(server)?
            .read_mcp_resource_for(server, uri, cancel)
            .await
    }
    fn remote_skills(&self) -> Vec<tickloop_harness_protocol::RemoteSkillDefinition> {
        self.sources
            .iter()
            .flat_map(|s| s.remote_skills())
            .collect()
    }
    fn remote_skill_is_current(&self, source: &tickloop_harness_protocol::SkillSource) -> bool {
        self.skill_source(source).is_some()
    }
    async fn read_remote_skill(
        &self,
        definition: &tickloop_harness_protocol::RemoteSkillDefinition,
        resource: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        self.skill_source(&definition.source)
            .ok_or("remote Skill unavailable")?
            .read_remote_skill(definition, resource, cancel)
            .await
    }
    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        self.source(name)?.validate_input(name, schema, input)
    }
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        for source in &self.sources {
            if let Some(endpoint) = source.host_endpoint()? {
                return Ok(Some(endpoint));
            }
        }
        Ok(None)
    }
    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        self.sources.iter().find_map(|s| s.workspace_host_context())
    }
    fn tool_authorities(&self, name: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        self.authority_source(name)?
            .map(|source| source.tool_authorities(name))
            .transpose()
            .map(Option::unwrap_or_default)
    }
    fn call_authority(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<Option<HostToolAuthority>, String> {
        self.authority_source(name)?
            .map(|source| source.call_authority(name, input))
            .transpose()
            .map(Option::flatten)
    }
    fn service_connections(&self) -> Value {
        let mut combined = serde_json::Map::new();
        for source in &self.sources {
            if let Value::Object(values) = source.service_connections() {
                combined.extend(values);
            }
        }
        Value::Object(combined)
    }
    fn tool_set_is_confirmed(&self) -> bool {
        self.sources.iter().all(|s| s.tool_set_is_confirmed())
    }
    async fn shutdown(&self) {
        futures_util::future::join_all(self.sources.iter().map(|s| s.shutdown())).await;
    }
    fn permission(&self, chat: ChatId, name: &str, input: &Value) -> HostToolPermission {
        self.source(name)
            .map_or(HostToolPermission::Confirmation, |s| {
                s.permission(chat, name, input)
            })
    }
    async fn prepare(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<HostToolPermission, String> {
        self.source(name)?.prepare(chat, name, input, cancel).await
    }
    async fn prepare_call(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.source(name)?
            .prepare_call(chat, name, input, cancel)
            .await
    }
    async fn prepare_call_in_scope(
        &self,
        chat: ChatId,
        scope: &HostCallScope<'_>,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.source(name)?
            .prepare_call_in_scope(chat, scope, name, input, cancel)
            .await
    }
    async fn revalidate_call(
        &self,
        chat: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        self.source(call.name())?
            .revalidate_call(chat, call, cancel)
            .await
    }
    async fn revalidate(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        permission: HostToolPermission,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        self.source(name)?
            .revalidate(chat, name, input, permission, cancel)
            .await
    }
    async fn execute_prepared(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        match self.source(call.name()) {
            Ok(source) => source.execute_prepared(chat, call, cancel).await,
            Err(error) => HostCallResult::NotDispatched(error),
        }
    }
    async fn execute(
        &self,
        chat: ChatId,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        match self.source(name) {
            Ok(source) => source.execute(chat, name, input, cancel).await,
            Err(error) => host_error_result(error),
        }
    }
}
