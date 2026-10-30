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
    guidance: crate::tool_exposure::McpGuidance,
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
            guidance: crate::tool_exposure::McpGuidance::default(),
            refresh_lock: tokio::sync::Mutex::new(()),
            #[cfg(feature = "test-support")]
            configuration_parses: AtomicU64::new(0),
            current: RwLock::new(Vec::new()),
            warnings: RwLock::new(Vec::new()),
        }
    }

    /// Replaces what the model is told to ask of the user when a server needs a login; the
    /// guidance gets the server's name. By default the user logs in from /mcp or with
    /// `tickloop mcp login`.
    pub fn with_login_guidance(
        mut self,
        guidance: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Self {
        self.guidance.login = Arc::new(guidance);
        self
    }

    /// Replaces everything the model is told of where the user manages MCP servers: the login
    /// guidance, the place named for failed, unsupported and unconfigured servers and for a
    /// server's exposure, and the system prompt's sentence on managing servers.
    pub fn with_guidance(mut self, guidance: crate::tool_exposure::McpGuidance) -> Self {
        self.guidance = guidance;
        self
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
        } else if matches!(restart, Restart::Server(_)) {
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
        hash.update(serde_json::to_vec(&server_identity(&entry.config)).unwrap_or_default());
        // A host's login counts by its account, so that renewing its token changes nothing.
        if let Some(bearer) = &entry.bearer {
            hash.update([0]);
            hash.update(bearer.account.as_bytes());
        } else if let Some(token) = entry.token.read().unwrap().as_ref() {
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
            entry.snapshot.read().unwrap().generation.hash(&mut hash);
            entry.lists_tools().hash(&mut hash);
        }
        Some(hash.finish())
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.connections
            .catalog()
            .into_iter()
            .filter(|(_, tool)| {
                tool.exposure != tickloop_harness_protocol::mcp::ToolExposure::Hidden
            })
            .map(|(_, tool)| (*tool.definition).clone())
            .collect()
    }

    fn has_tool_search(&self) -> bool {
        true
    }
    fn mcp_guidance(&self) -> Option<crate::tool_exposure::McpGuidance> {
        Some(self.guidance.clone())
    }
    fn discovery_tools(&self) -> Vec<crate::tool_exposure::ExposureTool> {
        self.connections
            .catalog()
            .into_iter()
            .map(|(entry, tool)| crate::tool_exposure::ExposureTool {
                exposure: tool.exposure,
                server: Some(entry.config.name.clone()),
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

    fn exposure_revisions(&self) -> BTreeMap<String, u64> {
        self.connections.exposure_revisions()
    }

    fn awaited_mcp_servers(&self) -> Option<tokio::sync::watch::Receiver<Vec<String>>> {
        Some(self.connections.awaited_servers())
    }

    fn validate_input(&self, name: &str, _: &Value, input: &Value) -> Result<(), String> {
        let (_, tool) = self
            .connections
            .catalog()
            .into_iter()
            .find(|(_, tool)| tool.definition.name == name)
            .ok_or("MCP tool unavailable")?;
        tool.display_validator
            .validate(input)
            .map_err(|error| crate::tool_input::schema_refusal(name, &error))
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
            .find(|(_, tool)| tool.definition.name == name)
            .map(|(entry, _)| Self::authority(&entry)))
    }
    async fn refresh(&self, cancel: CancellationToken) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("MCP refresh cancelled".into());
        }
        self.update_configuration(Restart::Nothing).await?;
        self.connections.renew_expiring_tokens().await;
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
            .entries()
            .iter()
            .filter_map(|entry| {
                let state = entry.state.read().unwrap().clone();
                if state == McpServerState::Disabled {
                    return None;
                }
                let instructions = entry.current_catalog().instructions.clone();
                Some(McpServerInstructions {
                    status: crate::tool_exposure::server_status(
                        &entry.config.name,
                        &entry.config.source,
                        &state,
                        entry.exposure().server,
                        &entry.description.read().unwrap(),
                        !instructions.is_empty(),
                        &self.guidance,
                    ),
                    text: instructions,
                    server: McpResourceServer {
                        name: entry.config.name.clone(),
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
        let auto_review_unruled = *call.entry.unruled.read().unwrap()
            == tickloop_harness_protocol::mcp::McpUnruled::AutoReview;
        // The reviewer learns which server and tool the call reaches, never what the server
        // writes of its tool: a description or annotation could argue for its own approval.
        let facts = tickloop_harness_protocol::PreparedApprovalFacts {
            normalized_input: input.clone(),
            external_declaration: Some(serde_json::json!({
                "mcp_server": call.entry.config.name,
                "tool": call.tool.definition.name,
            })),
            ..Default::default()
        };
        let is_remote = matches!(call.entry.config.transport, McpTransport::Http { .. });
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            input.clone(),
            HostToolPermission::ThirdParty,
            format!("Call third-party MCP tool {name}"),
        )
        .with_mcp_server(call.entry.config.name.clone())
        .with_target(call.target.clone())
        .with_auto_review_unruled(auto_review_unruled)
        .with_remote(is_remote)
        .with_approval_facts(facts)
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
        self.connections
            .revalidate(
                call.payload::<PreparedMcpCall>()
                    .ok_or("MCP prepared call missing")?,
            )
            .map(|_| ())
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
