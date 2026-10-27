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
    configuration: Arc<dyn Fn() -> McpConfiguration + Send + Sync>,
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
        Self {
            connections,
            configuration,
            warnings: RwLock::new(Vec::new()),
        }
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
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.discovery_tools()
            .into_iter()
            .filter(|tool| tool.exposure != tickloop_harness_protocol::mcp::ToolExposure::Hidden)
            .map(|tool| tool.definition)
            .collect()
    }
    fn has_tool_search(&self) -> bool {
        true
    }
    fn discovery_tools(&self) -> Vec<crate::tool_exposure::ExposureTool> {
        self.connections
            .catalog()
            .into_iter()
            .map(|(entry, _, tool)| {
                let mut original_schema = tool.schema;
                redact_schema(&entry, &mut original_schema);
                let mut annotations = tool.annotations;
                redact_schema(&entry, &mut annotations);
                crate::tool_exposure::ExposureTool {
                    exposure: crate::tool_exposure::effective_exposure(
                        entry.config.exposure,
                        &entry.config.tool_exposure,
                        &tool.original,
                    )
                    .for_host(self.connections.inner.capabilities.scripts),
                    server: entry.config.name.clone(),
                    original_name: tool.original,
                    definition: tool.definition,
                    original_schema,
                    annotations,
                }
            })
            .collect()
    }

    fn unavailable_tool_servers(&self) -> Vec<crate::tool_exposure::UnavailableServer> {
        self.connections.unavailable_tool_servers()
    }

    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        let validator =
            jsonschema::validator_for(schema).map_err(|_| format!("invalid {name} schema"))?;
        validator
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
        let configuration = (self.configuration)();
        self.connections.reconcile(configuration.servers).await;
        *self.warnings.write().unwrap() = configuration.warnings;
        self.connections.wait_for_startup(cancel).await;
        Ok(())
    }
    async fn mcp_server_statuses(&self, cancel: CancellationToken) -> Vec<McpServerStatus> {
        if !cancel.is_cancelled() {
            let configuration = (self.configuration)();
            self.connections
                .reconcile_keeping_failures(configuration.servers)
                .await;
            *self.warnings.write().unwrap() = configuration.warnings;
        }
        self.connections.statuses()
    }
    async fn reconnect_mcp_server(&self, name: &str) -> bool {
        let configuration = (self.configuration)();
        let reconnected = self
            .connections
            .reconnect(configuration.servers, name)
            .await;
        *self.warnings.write().unwrap() = configuration.warnings;
        reconnected
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
                        entry
                            .config
                            .exposure
                            .for_host(self.connections.inner.capabilities.scripts),
                        &redact(entry, entry.config.description.clone()),
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
        if !self.definitions().iter().any(|tool| tool.name == name) {
            return Err("MCP tool is unavailable or hidden".into());
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
