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
    sources: Arc<[Arc<dyn HostToolProvider>]>,
    index: Arc<std::sync::RwLock<Arc<SourceIndex>>>,
}
#[derive(Default)]
struct SourceIndex {
    generations: Vec<Option<u64>>,
    names: std::collections::BTreeMap<String, usize>,
    definitions: Vec<ToolDefinition>,
    discovery: Vec<crate::tool_exposure::ExposureTool>,
}
impl SourceIndex {
    /// The index of `sources` published in `published`, captured again when a source's
    /// generation changed or, after a refresh, when a source is unversioned.
    fn update(
        sources: &[Arc<dyn HostToolProvider>],
        published: &std::sync::RwLock<Arc<SourceIndex>>,
        refreshed: bool,
    ) -> Arc<SourceIndex> {
        let generations: Vec<_> = sources
            .iter()
            .map(|source| source.catalog_generation())
            .collect();
        let current = published.read().unwrap().clone();
        if current.generations == generations
            && !(refreshed && generations.iter().any(Option::is_none))
        {
            return current;
        }
        let mut next = SourceIndex {
            generations: generations.clone(),
            ..Default::default()
        };
        // A name two sources offer keeps the tool of the source installed earlier, which alone
        // answers its calls, permission and authority. The rule depends on the fixed source order
        // only, never on which source began offering the name first.
        for (index, source) in sources.iter().enumerate() {
            for definition in source.definitions() {
                if let Some(owner) = next.names.get(&definition.name) {
                    tracing::warn!(
                        tool = %definition.name,
                        kept_source = owner,
                        ignored_source = index,
                        "two host tool sources offer the same tool name; keeping the earlier source's tool"
                    );
                    continue;
                }
                next.names.insert(definition.name.clone(), index);
                next.definitions.push(definition);
            }
            next.discovery
                .extend(source.discovery_tools().into_iter().filter(|tool| {
                    next.names
                        .get(&tool.definition.name)
                        .is_none_or(|owner| *owner == index)
                }));
        }
        let next = Arc::new(next);
        let mut index = published.write().unwrap();
        // A concurrent refresh may have published while this index was being assembled.
        if Arc::ptr_eq(&index, &current)
            && sources
                .iter()
                .map(|source| source.catalog_generation())
                .eq(generations)
        {
            *index = next.clone();
            next
        } else {
            index.clone()
        }
    }
}
impl CompositeHostTools {
    pub(crate) fn combine(sources: Vec<Arc<dyn HostToolProvider>>) -> Arc<dyn HostToolProvider> {
        let combined = Self {
            sources: sources.into(),
            index: Arc::new(std::sync::RwLock::new(Arc::new(SourceIndex::default()))),
        };
        combined.update_index(true);
        Arc::new(combined)
    }
    fn update_index(&self, refreshed: bool) -> Arc<SourceIndex> {
        SourceIndex::update(&self.sources, &self.index, refreshed)
    }
    /// Captures unversioned sources again on a blocking thread: their definitions may take long
    /// to assemble, and a runtime worker blocked meanwhile would strand the task it last woke,
    /// such as the next holder of the Chats lock.
    async fn capture_refreshed_index(&self) {
        let sources = self.sources.clone();
        let published = self.index.clone();
        let captured = tokio::task::spawn_blocking(move || {
            SourceIndex::update(&sources, &published, true);
        })
        .await;
        if let Err(error) = captured
            && let Ok(panic) = error.try_into_panic()
        {
            std::panic::resume_unwind(panic);
        }
    }
    fn source(&self, name: &str) -> Result<&Arc<dyn HostToolProvider>, String> {
        let index = self.update_index(false);
        index
            .names
            .get(name)
            .map(|owner| &self.sources[*owner])
            .ok_or_else(|| "host tool unavailable".into())
    }
    fn authority_source(&self, name: &str) -> Result<Option<&Arc<dyn HostToolProvider>>, String> {
        let index = self.update_index(false);
        let mut owner = index.names.get(name).copied();
        if crate::mcp_tools::ARTIFACT_ACCOUNT_TOOL_NAMES.contains(&name) {
            for (candidate, source) in self
                .sources
                .iter()
                .enumerate()
                .filter(|(_, source)| source.artifact_client().is_some())
            {
                let _ = source;
                if owner.is_some_and(|owner| owner != candidate) {
                    return Err("host tool has conflicting authorities".into());
                }
                owner = Some(candidate);
            }
        }
        Ok(owner.map(|owner| &self.sources[owner]))
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
    #[cfg(feature = "test-support")]
    fn mcp_performance_counts(&self) -> (u64, u64, u64) {
        self.sources
            .iter()
            .map(|source| source.mcp_performance_counts())
            .fold((0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.update_index(false).definitions.clone()
    }
    fn discovery_tools(&self) -> Vec<crate::tool_exposure::ExposureTool> {
        self.update_index(false).discovery.clone()
    }
    fn unavailable_tool_servers(&self) -> Vec<crate::tool_exposure::UnavailableServer> {
        self.sources
            .iter()
            .flat_map(|source| source.unavailable_tool_servers())
            .collect()
    }
    fn has_tool_search(&self) -> bool {
        self.sources.iter().any(|source| source.has_tool_search())
    }
    fn upgrade_checkpoint(&self) -> Result<Option<Value>, String> {
        let mut states = Vec::new();
        for source in self.sources.iter() {
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
        self.capture_refreshed_index().await;
        results.into_iter().collect()
    }
    fn awaited_mcp_servers(&self) -> Option<tokio::sync::watch::Receiver<Vec<String>>> {
        self.sources
            .iter()
            .find_map(|source| source.awaited_mcp_servers())
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
    async fn mcp_server_statuses(
        &self,
        cancel: CancellationToken,
    ) -> Vec<tickloop_harness_protocol::mcp::McpServerStatus> {
        futures_util::future::join_all(
            self.sources
                .iter()
                .map(|s| s.mcp_server_statuses(cancel.clone())),
        )
        .await
        .into_iter()
        .flatten()
        .collect()
    }
    async fn reconnect_mcp_server(&self, name: &str) -> bool {
        for source in self.sources.iter() {
            if source.reconnect_mcp_server(name).await {
                return true;
            }
        }
        false
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
        for source in self.sources.iter() {
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
        for source in self.sources.iter() {
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
