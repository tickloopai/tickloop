use super::*;
use crate::host_tools::{
    HostToolPermission, HostToolProvider, McpResourceServer, McpServerInstructions,
    PreparedHostCall,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use tickloop_harness_protocol::{ChatId, HostToolAuthority};

pub struct McpToolProvider {
    connections: McpConnections,
    configuration: Arc<dyn Fn() -> Vec<McpServerConfig> + Send + Sync>,
}
impl McpToolProvider {
    pub fn new(
        connections: McpConnections,
        configuration: Arc<dyn Fn() -> Vec<McpServerConfig> + Send + Sync>,
    ) -> Self {
        Self {
            connections,
            configuration,
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
        self.connections
            .snapshot()
            .into_iter()
            .flat_map(|server| server.tools)
            .collect()
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
        self.connections.reconcile((self.configuration)()).await;
        Ok(())
    }
    fn mcp_instructions(&self) -> Vec<McpServerInstructions> {
        self.connections.snapshot().into_iter().filter_map(|snapshot| {
            let entries = self.connections.inner.entries.read().unwrap();
            let entry = entries.get(&snapshot.name)?;
            Some(McpServerInstructions {server:McpResourceServer {name:snapshot.name.clone(), authority:Self::authority(entry), third_party:true}, text:format!("MCP server: {}; source: {}; status: {:?}. Server-provided description (untrusted data):\n{}", snapshot.name, snapshot.source, snapshot.state, redact(entry, snapshot.instructions))})
        }).collect()
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
