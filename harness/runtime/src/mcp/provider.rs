use super::*;
use crate::host_tools::{
    HostToolPermission, HostToolProvider, McpResourceServer, McpServerInstructions,
    PreparedHostCall,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use tickloop_harness_protocol::{ChatId, HostToolAuthority};

pub struct McpConfiguration {
    pub servers: Vec<McpServerConfig>,
    pub warnings: Vec<String>,
}

pub struct McpToolProvider {
    connections: McpConnections,
    configuration: Arc<dyn Fn() -> Option<McpConfiguration> + Send + Sync>,
    refresh_lock: tokio::sync::Mutex<()>,
    #[cfg(feature = "test-support")]
    configuration_parses: AtomicU64,
    current: RwLock<Vec<McpServerConfig>>,
    warnings: RwLock<Vec<String>>,
}
impl McpToolProvider {
    pub fn new(
        connections: McpConnections,
        configuration: Arc<dyn Fn() -> Vec<McpServerConfig> + Send + Sync>,
    ) -> Self {
        Self::with_configuration(
            connections,
            Arc::new(move || McpConfiguration {
                servers: configuration(),
                warnings: Vec::new(),
            }),
        )
    }

    pub fn with_configuration(
        connections: McpConnections,
        configuration: Arc<dyn Fn() -> McpConfiguration + Send + Sync>,
    ) -> Self {
        Self::with_configuration_updates(connections, Arc::new(move || Some(configuration())))
    }

    /// The host returns `None` when its configuration and credential files are unchanged.
    pub fn with_configuration_updates(
        connections: McpConnections,
        configuration: Arc<dyn Fn() -> Option<McpConfiguration> + Send + Sync>,
    ) -> Self {
        Self {
            connections,
            configuration,
            refresh_lock: tokio::sync::Mutex::new(()),
            #[cfg(feature = "test-support")]
            configuration_parses: AtomicU64::new(0),
            current: RwLock::new(Vec::new()),
            warnings: RwLock::new(Vec::new()),
        }
    }

    async fn update_configuration(&self, restart: Restart<'_>) -> Result<(), String> {
        let _guard = self.refresh_lock.lock().await;
        let configuration = self.configuration.clone();
        let timing = crate::timing::Phase::new("configuration");
        let configuration = tokio::task::spawn_blocking(move || configuration())
            .await
            .map_err(|_| "MCP configuration unavailable")?;
        drop(timing);
        let timing = crate::timing::Phase::new("reconcile");
        if let Some(configuration) = configuration {
            #[cfg(feature = "test-support")]
            self.configuration_parses.fetch_add(1, Ordering::Relaxed);
            *self.current.write().unwrap() = configuration.servers.clone();
            self.connections
                .reconcile_restarting(configuration.servers, restart)
                .await;
            *self.warnings.write().unwrap() = configuration.warnings;
        } else if matches!(restart, Restart::Server(_))
            || (restart == Restart::Failed && self.connections.inner.entries.read().unwrap().values().any(|entry| {
                *entry.state.read().unwrap() == McpServerState::Connected
                    && entry.credential.read().unwrap().as_ref().and_then(|saved| saved.token.as_ref()).is_some_and(|token| token.expires_at <= tickloop_harness_base::private_fs::unix_time_milliseconds() / 1000 + 30)
            }))
        {
            let configs = self.current.read().unwrap().clone();
            self.connections
                .reconcile_restarting(configs, restart)
                .await;
        }
        drop(timing);
        Ok(())
    }
    fn authority(entry: &Entry) -> HostToolAuthority {
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(&entry.config).unwrap_or_default());
        if let Some(token) = entry.token.read().unwrap().as_ref() {
            hash.update(token.secret());
        }
        HostToolAuthority::Mcp {
            server_id: entry.config.name.clone(),
            account_id: format!("{:x}", hash.finalize()),
        }
    }
}
#[async_trait]
impl HostToolProvider for McpToolProvider {
    #[cfg(feature = "test-support")]
    fn mcp_performance_counts(&self) -> (u64, u64, u64) {
        let (catalog, redactor) = self.connections.performance_counts();
        (
            catalog,
            redactor,
            self.configuration_parses.load(Ordering::Relaxed),
        )
    }
    fn catalog_generation(&self) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for (name, entry) in self.connections.inner.entries.read().unwrap().iter() {
            name.hash(&mut hash);
            (Arc::as_ptr(entry) as usize).hash(&mut hash);
            entry
                .secret_generation
                .load(Ordering::SeqCst)
                .hash(&mut hash);
            if let Some(connection) = entry.connection.read().unwrap().as_ref() {
                connection.revision.load(Ordering::SeqCst).hash(&mut hash);
                connection
                    .snapshot
                    .read()
                    .unwrap()
                    .generation
                    .hash(&mut hash);
            }
            (*entry.state.read().unwrap() == McpServerState::NeedsLogin).hash(&mut hash);
        }
        Some(hash.finish())
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.connections
            .catalog()
            .into_iter()
            .filter(|(_, _, tool)| {
                tool.exposure != tickloop_harness_protocol::mcp::ToolExposure::Hidden
            })
            .map(|(_, _, tool)| tool.definition.clone())
            .collect()
    }

    fn has_tool_search(&self) -> bool {
        true
    }
    fn discovery_tools(&self) -> Vec<crate::tool_exposure::ExposureTool> {
        self.connections
            .catalog()
            .into_iter()
            .map(|(entry, _, tool)| crate::tool_exposure::ExposureTool {
                exposure: tool.exposure,
                server: entry.config.name.clone(),
                original_name: tool.original.clone(),
                definition: tool.definition.clone(),
                original_schema: tool.original_schema.clone(),
                annotations: tool.annotations.clone(),
            })
            .collect()
    }

    fn unavailable_tool_servers(&self) -> Vec<crate::tool_exposure::UnavailableServer> {
        self.connections.unavailable_tool_servers()
    }

    fn validate_input(&self, name: &str, _: &Value, input: &Value) -> Result<(), String> {
        let (_, _, tool) = self
            .connections
            .catalog()
            .into_iter()
            .find(|(_, _, tool)| tool.definition.name == name)
            .ok_or("MCP tool unavailable")?;
        tool.display_validator
            .validate(input)
            .map_err(|_| format!("invalid {name} input"))
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ThirdParty
    }
    fn tool_authorities(&self, name: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        Ok(self
            .call_authority(name, &Value::Null)?
            .into_iter()
            .collect())
    }
    fn call_authority(&self, name: &str, _: &Value) -> Result<Option<HostToolAuthority>, String> {
        Ok(self
            .connections
            .catalog()
            .into_iter()
            .find(|(_, _, tool)| tool.definition.name == name)
            .map(|(entry, _, _)| Self::authority(&entry)))
    }
    async fn refresh(&self, cancel: CancellationToken) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("MCP refresh cancelled".into());
        }
        self.update_configuration(Restart::Failed).await?;
        let _timing = crate::timing::Phase::new("wait_for_startup");
        self.connections.wait_for_startup(cancel).await;
        Ok(())
    }
    async fn mcp_server_statuses(&self, cancel: CancellationToken) -> Vec<McpServerStatus> {
        if !cancel.is_cancelled() {
            let _ = self.update_configuration(Restart::Nothing).await;
        }
        self.connections.statuses()
    }
    async fn reconnect_mcp_server(&self, name: &str) -> bool {
        self.update_configuration(Restart::Server(name))
            .await
            .is_ok()
            && self
                .current
                .read()
                .unwrap()
                .iter()
                .any(|config| config.name == name)
    }
    fn service_connections(&self) -> Value {
        serde_json::json!({"mcp": {"warnings": *self.warnings.read().unwrap()}})
    }
    fn mcp_instructions(&self) -> Vec<McpServerInstructions> {
        self.connections
            .snapshot()
            .into_iter()
            .filter(|snapshot| snapshot.state != McpServerState::Disabled)
            .filter_map(|snapshot| {
                let entries = self.connections.inner.entries.read().unwrap();
                let entry = entries.get(&snapshot.name)?;
                Some(McpServerInstructions {
                    text: crate::tool_exposure::server_message(
                        &snapshot.name,
                        &snapshot.source,
                        &snapshot.state,
                        entry.config.exposure,
                        &entry.description.read().unwrap(),
                        &snapshot.instructions,
                    ),
                    server: McpResourceServer {
                        name: snapshot.name,
                        authority: Self::authority(entry),
                        third_party: true,
                    },
                })
            })
            .collect()
    }

    fn mcp_resource_servers(&self) -> Vec<McpResourceServer> {
        self.connections
            .inner
            .entries
            .read()
            .unwrap()
            .values()
            .filter(|entry| {
                *entry.state.read().unwrap() == McpServerState::Connected
                    && entry
                        .connection
                        .read()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|connection| connection.resources.is_some())
            })
            .map(|entry| McpResourceServer {
                name: entry.config.name.clone(),
                authority: Self::authority(entry),
                third_party: true,
            })
            .collect()
    }
    async fn list_mcp_resources_for(
        &self,
        server: &str,
        cursor: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.connections
            .list_resources(server, cursor, cancel)
            .await
    }
    async fn read_mcp_resource_for(
        &self,
        server: &str,
        uri: &str,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.connections.read_resource(server, uri, cancel).await
    }
    async fn prepare_call(
        &self,
        _: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if cancel.is_cancelled() {
            return Err("MCP call cancelled before preparation".into());
        }
        let call = self
            .connections
            .prepare_ready(name, input.clone(), &cancel)
            .await?;
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            input.clone(),
            HostToolPermission::ThirdParty,
            format!("Call third-party MCP tool {name}"),
        )
        .with_mcp_server(call.entry.config.name.clone())
        .with_payload(call))
    }
    async fn revalidate_call(
        &self,
        _: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("MCP call cancelled before dispatch".into());
        }
        self.connections.revalidate(
            call.payload::<PreparedMcpCall>()
                .ok_or("MCP prepared call missing")?,
        )
    }
    async fn execute_prepared(
        &self,
        _: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        match call.into_payload::<PreparedMcpCall>() {
            Ok(call) => self.connections.dispatch(call, cancel).await,
            Err(_) => HostCallResult::NotDispatched("MCP prepared call missing".into()),
        }
    }
    async fn execute(
        &self,
        _: ChatId,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        self.connections
            .call(name, input, cancel)
            .await
            .into_tool_result()
    }
    async fn shutdown(&self) {
        self.connections.shutdown().await;
    }
}
