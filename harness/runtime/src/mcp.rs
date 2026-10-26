//! Agent-owned MCP connections. Hosts supply resolved configuration, credentials and capabilities.
mod catalog;
mod process;
mod provider;
use crate::{host_tools::HostCallResult, mcp_http::McpHttpClient};
use base64::Engine;
pub use provider::{McpConfiguration, McpToolProvider};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::{CallToolRequestParams, ClientConfig, ProtocolVersion},
    service::{NotificationContext, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tickloop_harness_protocol::{
    ToolDefinition, ToolResult,
    mcp::{McpServerConfig, McpTransport},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default)]
pub struct HostCapabilities {
    pub local_processes: bool,
    pub scripts: bool,
}

use tickloop_harness_connector::{AccessToken, Connector, OAuthSettings, TokenError};
pub use tickloop_harness_connector::{
    ConnectorCredential, ConnectorCredentials, InMemoryCredentials,
};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Connecting,
    Connected,
    Failed,
    Unsupported,
    NeedsLogin,
    NeedsConfiguration,
}
#[derive(Clone, Debug, Serialize)]
pub struct McpServerSnapshot {
    pub name: String,
    pub source: String,
    pub state: McpServerState,
    pub tools: Vec<ToolDefinition>,
    pub resources: Vec<Value>,
    pub instructions: String,
    pub warnings: Vec<String>,
}
struct Handler {
    revision: Arc<AtomicU64>,
    changed: Arc<tokio::sync::Notify>,
}
impl ClientHandler for Handler {
    async fn on_tool_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_one();
    }
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}
struct Connection {
    peer: rmcp::service::Peer<RoleClient>,
    revision: Arc<AtomicU64>,
    catalog_revision: AtomicU64,
    http: Option<McpHttpClient>,
    changed: Arc<tokio::sync::Notify>,
    tools: RwLock<BTreeMap<String, catalog::Tool>>,
    instructions: String,
    resources: Vec<Value>,
    warnings: RwLock<Vec<String>>,
}
struct Entry {
    config: McpServerConfig,
    token: RwLock<Option<AccessToken>>,
    credential: RwLock<Option<ConnectorCredential>>,
    credentials: Option<Arc<dyn ConnectorCredentials>>,
    connector: Option<Connector>,
    environment_secrets: Vec<String>,
    state: RwLock<McpServerState>,
    connection: RwLock<Option<Arc<Connection>>>,
    cancel: CancellationToken,
}
struct PreparedMcpCall {
    entry: Arc<Entry>,
    connection: Arc<Connection>,
    tool: catalog::Tool,
    input: Value,
}
#[derive(Clone)]
pub struct McpConnections {
    inner: Arc<Inner>,
}
struct Inner {
    capabilities: HostCapabilities,
    credentials: RwLock<BTreeMap<String, Arc<dyn ConnectorCredentials>>>,
    environment: BTreeMap<String, String>,
    stderr_directory: Option<tickloop_harness_base::private_fs::PrivateDirectory>,
    entries: RwLock<BTreeMap<String, Arc<Entry>>>,
    tasks: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
    reconcile_lock: tokio::sync::Mutex<()>,
    closed: AtomicBool,
    state_changed: Arc<tokio::sync::Notify>,
    local_slots: Arc<tokio::sync::Semaphore>,
    remote_slots: Arc<tokio::sync::Semaphore>,
}
impl McpConnections {
    pub fn new(
        capabilities: HostCapabilities,
        credentials: BTreeMap<String, Arc<dyn ConnectorCredentials>>,
        environment: BTreeMap<String, String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                capabilities,
                credentials: RwLock::new(credentials),
                environment,
                stderr_directory: None,
                entries: RwLock::new(BTreeMap::new()),
                tasks: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
                reconcile_lock: tokio::sync::Mutex::new(()),
                closed: AtomicBool::new(false),
                state_changed: Arc::new(tokio::sync::Notify::new()),
                local_slots: Arc::new(tokio::sync::Semaphore::new(4)),
                remote_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            }),
        }
    }
    pub fn with_stderr_directory(
        mut self,
        directory: tickloop_harness_base::private_fs::PrivateDirectory,
    ) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("configure MCP logging before sharing connections")
            .stderr_directory = Some(directory);
        self
    }
    /// Replaces the host's credential adapters before reconciling a fresh configuration.
    pub fn set_credentials(&self, credentials: BTreeMap<String, Arc<dyn ConnectorCredentials>>) {
        *self.inner.credentials.write().unwrap() = credentials;
    }

    pub async fn reconcile(&self, configs: Vec<McpServerConfig>) {
        let _guard = self.inner.reconcile_lock.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return;
        }
        while self.inner.tasks.lock().await.try_join_next().is_some() {}
        let mut next = BTreeMap::new();
        let configs: BTreeMap<_, _> = configs
            .into_iter()
            .map(|config| (config.name.clone(), config))
            .collect();
        for config in configs.into_values() {
            let (connector, credentials, credential, credential_failure) = match &config.transport {
                McpTransport::Http { url, headers }
                    if !headers
                        .keys()
                        .any(|key| key.eq_ignore_ascii_case("authorization")) =>
                {
                    let credentials = self
                        .inner
                        .credentials
                        .write()
                        .unwrap()
                        .entry(config.name.clone())
                        .or_insert_with(|| Arc::new(InMemoryCredentials::default()))
                        .clone();
                    let connector = Connector::new(
                        url,
                        OAuthSettings {
                            client_id: config.oauth.client_id.clone(),
                            callback_port: config.oauth.callback_port,
                            authorization_server_metadata_url: config
                                .oauth
                                .authorization_server_metadata_url
                                .clone(),
                        },
                        credentials.clone(),
                    )
                    .ok();
                    let (credential, failure) = match credentials.load().await {
                        Ok(credential) => (credential, None),
                        Err(_) => (None, Some(McpServerState::Failed)),
                    };
                    (connector, Some(credentials), credential, failure)
                }
                _ => (None, None, None, None),
            };
            let existing = self
                .inner
                .entries
                .read()
                .unwrap()
                .get(&config.name)
                .cloned();
            let entry = if let Some(entry) = existing.filter(|entry| {
                entry.config == config
                    && *entry.credential.read().unwrap() == credential
                    && *entry.state.read().unwrap() != McpServerState::Failed
                    && !(*entry.state.read().unwrap() == McpServerState::Connected
                        && credential
                            .as_ref()
                            .and_then(|saved| saved.token.as_ref())
                            .is_some_and(|token| {
                                token.expires_at
                                    <= tickloop_harness_base::private_fs::unix_time_milliseconds()
                                        / 1000
                                        + 30
                            }))
            }) {
                entry
            } else {
                let state = credential_failure.unwrap_or_else(|| match &config.transport {
                    McpTransport::Unsupported { .. } => McpServerState::Unsupported,
                    McpTransport::Failed { .. } => McpServerState::Failed,
                    McpTransport::NeedsConfiguration => McpServerState::NeedsConfiguration,
                    McpTransport::Stdio { .. } if !self.inner.capabilities.local_processes => {
                        McpServerState::Unsupported
                    }
                    _ => McpServerState::Connecting,
                });
                let environment_secrets = self
                    .inner
                    .environment
                    .iter()
                    .filter(|(name, _)| {
                        let name = name.to_ascii_lowercase();
                        ["token", "secret", "password", "api_key", "credential"]
                            .iter()
                            .any(|key| name.contains(key))
                    })
                    .map(|(_, value)| value.clone())
                    .collect();
                let entry = Arc::new(Entry {
                    config,
                    token: RwLock::new(None),
                    credential: RwLock::new(credential),
                    credentials,
                    connector,
                    environment_secrets,
                    state: RwLock::new(state),
                    connection: RwLock::new(None),
                    cancel: CancellationToken::new(),
                });
                if *entry.state.read().unwrap() == McpServerState::Connecting {
                    self.start(entry.clone()).await;
                }
                entry
            };
            next.insert(entry.config.name.clone(), entry);
        }
        let old = std::mem::replace(&mut *self.inner.entries.write().unwrap(), next);
        for (name, entry) in old {
            if !self
                .inner
                .entries
                .read()
                .unwrap()
                .get(&name)
                .is_some_and(|current| Arc::ptr_eq(current, &entry))
            {
                entry.cancel.cancel();
            }
        }
    }
    async fn start(&self, entry: Arc<Entry>) {
        let environment = self.inner.environment.clone();
        let stderr_directory = self.inner.stderr_directory.clone();
        let state_changed = self.inner.state_changed.clone();
        let slots = if matches!(entry.config.transport, McpTransport::Stdio { .. }) {
            self.inner.local_slots.clone()
        } else {
            self.inner.remote_slots.clone()
        };
        self.inner.tasks.lock().await.spawn(async move {
            let connect = async {
                let _permit = slots.acquire().await.map_err(|_| "MCP connection cancelled")?;
                let has_token = entry.credential.read().unwrap().as_ref().is_some_and(|saved| matches!(&entry.config.transport, McpTransport::Http {url, ..} if reqwest::Url::parse(url).is_ok_and(|url| saved.server_url == url.as_str())) && saved.token.is_some());
                if has_token && let Some(connector) = &entry.connector {
                    let token = connector.access_token(&entry.cancel).await;
                    if let Some(credentials) = &entry.credentials {
                        *entry.credential.write().unwrap() = credentials.load().await.map_err(|_| "MCP credential unavailable")?;
                    }
                    *entry.token.write().unwrap() = Some(token.map_err(|error| match error {
                        TokenError::NeedsLogin | TokenError::RenewalFailed => "MCP login required",
                        _ => "MCP credential unavailable",
                    })?);
                }
                let token = entry.token.read().unwrap().clone();
                let opened = open(&entry.config, token.as_ref().map(|token| token.secret().to_owned()), &environment, stderr_directory.as_ref()).await;
                if matches!(&opened, Err(error) if error == "MCP login required")
                    && let (Some(connector), Some(token)) = (&entry.connector, token)
                {
                    let renewed = connector.renew_rejected(&token, &entry.cancel).await;
                    if let Some(credentials) = &entry.credentials {
                        *entry.credential.write().unwrap() = credentials.load().await.map_err(|_| "MCP credential unavailable")?;
                    }
                    let renewed = renewed.map_err(|_| "MCP login required")?;
                    *entry.token.write().unwrap() = Some(renewed.clone());
                    return open(&entry.config, Some(renewed.secret().to_owned()), &environment, stderr_directory.as_ref()).await;
                }
                opened
            };
            let result = tokio::select! {
                _ = entry.cancel.cancelled() => return,
                result = tokio::time::timeout(Duration::from_secs(30), connect) => result,
            };
            match result {
                Ok(Ok((connection, service))) => {
                    let connection = Arc::new(connection);
                    *entry.connection.write().unwrap() = Some(connection.clone());
                    *entry.state.write().unwrap() = McpServerState::Connected;
                    state_changed.notify_waiters();
                    let stop = service.cancellation_token();
                    let mut waiting = Box::pin(service.waiting());
                    loop {
                        tokio::select! {
                            _ = entry.cancel.cancelled() => { stop.cancel(); let _ = waiting.await; break; }
                            _ = &mut waiting => { if *entry.state.read().unwrap() != McpServerState::NeedsLogin { *entry.state.write().unwrap() = McpServerState::Failed; } state_changed.notify_waiters(); break; }
                            _ = connection.changed.notified() => {
                                let revision = connection.revision.load(Ordering::SeqCst);
                                let tools = tokio::select! {
                                    _ = entry.cancel.cancelled() => continue,
                                    tools = tokio::time::timeout(Duration::from_secs(30), connection.peer.list_all_tools()) => tools,
                                };
                                if let Ok(Ok(tools)) = tools {
                                    let (tools, warnings) = catalog::directory(&entry.config.name, tools);
                                    *connection.tools.write().unwrap() = tools;
                                    *connection.warnings.write().unwrap() = warnings;
                                    connection.catalog_revision.store(revision, Ordering::SeqCst);
                                } else {
                                    connection.tools.write().unwrap().clear();
                                    *connection.warnings.write().unwrap() = vec!["MCP tools unavailable".into()];
                                }
                            }
                        }
                    }

                }
                error => { *entry.state.write().unwrap() = if matches!(error, Ok(Err(ref error)) if (error == "MCP login required" || error == "MCP scope required") && entry.connector.is_some()) {McpServerState::NeedsLogin} else {McpServerState::Failed}; state_changed.notify_waiters(); }
            }
        });
    }
    fn catalog(&self) -> Vec<(Arc<Entry>, Arc<Connection>, catalog::Tool)> {
        let mut tools = Vec::new();
        for entry in self.inner.entries.read().unwrap().values() {
            if *entry.state.read().unwrap() == McpServerState::NeedsLogin {
                continue;
            }
            if let Some(connection) = entry.connection.read().unwrap().clone() {
                if connection.revision.load(Ordering::SeqCst)
                    != connection.catalog_revision.load(Ordering::SeqCst)
                {
                    continue;
                }
                for tool in connection.tools.read().unwrap().values() {
                    let mut tool = tool.clone();
                    if redact(entry, tool.definition.name.clone()) != tool.definition.name {
                        continue;
                    }
                    tool.definition.description = redact(entry, tool.definition.description);
                    redact_value(entry, &mut tool.definition.input_schema);
                    tools.push((entry.clone(), connection.clone(), tool));
                }
            }
        }
        let mut counts = BTreeMap::<String, usize>::new();
        for (_, _, tool) in &tools {
            *counts.entry(tool.definition.name.clone()).or_default() += 1;
        }
        for (entry, _, tool) in &mut tools {
            if counts[&tool.definition.name] > 1 {
                tool.definition.name = catalog::hashed_name(
                    &tool.definition.name,
                    &format!("{}\0{}", entry.config.name, tool.original),
                );
            }
        }
        tools
    }
    pub fn snapshot(&self) -> Vec<McpServerSnapshot> {
        let catalog = self.catalog();
        self.inner
            .entries
            .read()
            .unwrap()
            .values()
            .map(|entry| {
                let connection = entry.connection.read().unwrap();
                McpServerSnapshot {
                    name: entry.config.name.clone(),
                    source: entry.config.source.clone(),
                    state: entry.state.read().unwrap().clone(),
                    tools: catalog
                        .iter()
                        .filter(|(owner, _, _)| Arc::ptr_eq(owner, entry))
                        .map(|(_, _, tool)| tool.definition.clone())
                        .collect(),
                    resources: connection
                        .as_ref()
                        .map(|c| {
                            c.resources
                                .iter()
                                .cloned()
                                .map(|mut value| {
                                    redact_value(entry, &mut value);
                                    value
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    instructions: connection
                        .as_ref()
                        .map(|c| redact(entry, c.instructions.clone()))
                        .unwrap_or_default(),
                    warnings: match &entry.config.transport {
                        McpTransport::Failed { reason } => vec![reason.clone()],
                        _ => connection
                            .as_ref()
                            .map(|c| {
                                c.warnings
                                    .read()
                                    .unwrap()
                                    .iter()
                                    .map(|warning| redact(entry, warning.clone()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    },
                }
            })
            .collect()
    }
    async fn prepare_ready(
        &self,
        name: &str,
        input: Value,
        cancel: &CancellationToken,
    ) -> Result<PreparedMcpCall, String> {
        let reconnect = self
            .catalog()
            .iter()
            .find(|(entry, _, tool)| {
                *entry.state.read().unwrap() == McpServerState::Failed
                    && tool.definition.name == name
            })
            .map(|(entry, _, _)| entry.config.name.clone());
        if let Some(server) = reconnect {
            let configs = self
                .inner
                .entries
                .read()
                .unwrap()
                .values()
                .map(|entry| entry.config.clone())
                .collect();
            self.reconcile(configs).await;
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let notified = self.inner.state_changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if !self.inner.entries.read().unwrap().get(&server).is_some_and(|entry| *entry.state.read().unwrap() == McpServerState::Connecting) {break;}
                    tokio::select! { _ = cancel.cancelled() => return Err("MCP connection cancelled"), _ = notified => {} }
                }
                Ok(())
            }).await.map_err(|_| "MCP connection timed out")??;
        }
        self.prepare(name, input)
    }
    fn prepare(&self, name: &str, input: Value) -> Result<PreparedMcpCall, String> {
        let target = self
            .catalog()
            .into_iter()
            .find(|(_, _, tool)| tool.definition.name == name);
        let (entry, connection, tool) = target.ok_or("MCP tool is unavailable")?;
        if crate::validate_host_schema_input(name, &tool.schema, &input).is_err() {
            return Err("invalid MCP tool arguments".into());
        }
        if *entry.state.read().unwrap() != McpServerState::Connected {
            return Err("MCP connection is unavailable".into());
        }
        Ok(PreparedMcpCall {
            entry,
            connection,
            tool,
            input,
        })
    }
    fn revalidate(&self, call: &PreparedMcpCall) -> Result<(), String> {
        if call.entry.cancel.is_cancelled()
            || !self
                .inner
                .entries
                .read()
                .unwrap()
                .get(&call.entry.config.name)
                .is_some_and(|entry| Arc::ptr_eq(entry, &call.entry))
        {
            return Err("MCP configuration changed before dispatch".into());
        }
        if call.connection.revision.load(Ordering::SeqCst)
            != call.connection.catalog_revision.load(Ordering::SeqCst)
        {
            return Err("MCP tool directory changed before dispatch".into());
        }
        let current = call.connection.tools.read().unwrap();
        if !current.values().any(|tool| {
            tool.original == call.tool.original
                && tool.schema == call.tool.schema
                && redact(&call.entry, tool.definition.description.clone())
                    == call.tool.definition.description
        }) {
            return Err("MCP tool changed before dispatch".into());
        }
        Ok(())
    }
    pub async fn call(
        &self,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> HostCallResult {
        match self.prepare_ready(name, input, &cancel).await {
            Ok(call) => self.dispatch(call, cancel).await,
            Err(error) => HostCallResult::NotDispatched(error),
        }
    }
    async fn dispatch(&self, call: PreparedMcpCall, cancel: CancellationToken) -> HostCallResult {
        if let Err(error) = self.revalidate(&call) {
            return HostCallResult::NotDispatched(error);
        }
        let PreparedMcpCall {
            entry,
            connection,
            tool,
            input,
        } = call;
        if cancel.is_cancelled() || entry.cancel.is_cancelled() {
            return HostCallResult::NotDispatched("MCP call cancelled before dispatch".into());
        }
        let Some(arguments) = input.as_object() else {
            return HostCallResult::NotDispatched("MCP arguments must be an object".into());
        };
        let request = CallToolRequestParams::new(tool.original).with_arguments(arguments.clone());
        let response = tokio::select! {
            _ = cancel.cancelled() => return HostCallResult::OutcomeUnknown("MCP call cancelled after dispatch; do not automatically retry".into()),
            _ = entry.cancel.cancelled() => return HostCallResult::OutcomeUnknown("MCP configuration changed after dispatch".into()),
            response = tokio::time::timeout(Duration::from_millis(entry.config.timeout_ms), connection.peer.call_tool_once(request)) => response,
        };
        if connection
            .http
            .as_ref()
            .is_some_and(McpHttpClient::unauthorized)
        {
            authentication_rejected(&entry, connection.http.as_ref().unwrap(), &cancel).await;
            self.inner.state_changed.notify_waiters();
        } else if !matches!(response, Ok(Ok(rmcp::model::CallToolResponse::Complete(_)))) {
            *entry.state.write().unwrap() = McpServerState::Failed;
        }
        match response {
            Ok(Ok(rmcp::model::CallToolResponse::Complete(result))) => {
                let mut value = serde_json::to_value(result).unwrap_or(Value::Null);
                if let Some(structured) = value.get_mut("structuredContent") {
                    redact_value(&entry, structured);
                }
                let mut images = Vec::new();
                let content = if value["isError"] == true {
                    "MCP tool reported an error".into()
                } else if let Some(blocks) = value["content"]
                    .as_array()
                    .filter(|blocks| !blocks.is_empty())
                {
                    blocks
                        .iter()
                        .filter_map(|block| match block["type"].as_str() {
                            Some("text") => block["text"].as_str().map(str::to_owned),
                            Some("resource") => {
                                block["resource"]["text"].as_str().map(str::to_owned)
                            }
                            Some("image") => {
                                let prepared = block["data"]
                                    .as_str()
                                    .filter(|data| {
                                        data.len() <= tickloop_harness_base::image::MAX_BASE64_BYTES
                                    })
                                    .and_then(|data| {
                                        base64::engine::general_purpose::STANDARD.decode(data).ok()
                                    })
                                    .and_then(|bytes| {
                                        tickloop_harness_base::image::prepare(&bytes).ok()
                                    });
                                if let Some(image) = prepared {
                                    images.push(image);
                                    None
                                } else {
                                    Some("[MCP image omitted: unsupported or invalid image]".into())
                                }
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    value
                        .get("structuredContent")
                        .map(Value::to_string)
                        .unwrap_or_default()
                };
                let result = ToolResult {
                    content: redact(&entry, content),
                    is_error: value["isError"] == true,
                    output_ref: None,
                };
                if images.is_empty() {
                    HostCallResult::Completed(result)
                } else {
                    HostCallResult::CompletedWithImages(result, images)
                }
            }
            Ok(Err(ref error)) if crate::mcp_http::session_expired(error) => {
                HostCallResult::NotDispatched(
                    "MCP session expired; reconnect before retrying".into(),
                )
            }
            _ => HostCallResult::OutcomeUnknown(
                "MCP response unavailable after dispatch; do not automatically retry".into(),
            ),
        }
    }
    pub async fn list_resources(
        &self,
        server: &str,
        cursor: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.resource_request(server, "resources/list", json!({"cursor":cursor}), cancel)
            .await
    }
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.resource_request(server, "resources/read", json!({"uri":uri}), cancel)
            .await
    }
    async fn resource_request(
        &self,
        server: &str,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let entry = self
            .inner
            .entries
            .read()
            .unwrap()
            .get(server)
            .cloned()
            .ok_or("MCP resource server unavailable")?;
        let connection = entry
            .connection
            .read()
            .unwrap()
            .clone()
            .ok_or("MCP connection unavailable")?;
        let request = rmcp::model::ClientRequest::CustomRequest(rmcp::model::CustomRequest::new(
            method,
            Some(params),
        ));
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err("MCP resource request cancelled".into()),
            _ = entry.cancel.cancelled() => return Err("MCP resource server changed".into()),
            result = tokio::time::timeout(Duration::from_secs(30), connection.peer.send_request(request)) => result,
        }.map_err(|_| "MCP resource request timed out")?.map_err(|_| "MCP resource request failed")?;
        let mut value =
            serde_json::to_value(&response).map_err(|_| "invalid MCP resource result")?;
        redact_value(&entry, &mut value);
        Ok(value)
    }
    pub async fn shutdown(&self) {
        let _guard = self.inner.reconcile_lock.lock().await;
        self.inner.closed.store(true, Ordering::SeqCst);
        for entry in self.inner.entries.read().unwrap().values() {
            entry.cancel.cancel();
        }
        let mut tasks = self.inner.tasks.lock().await;
        while tasks.join_next().await.is_some() {}
        self.inner.entries.write().unwrap().clear();
    }
}
async fn authentication_rejected(entry: &Entry, http: &McpHttpClient, cancel: &CancellationToken) {
    let token = entry.token.read().unwrap().clone();
    let renewed = match (&entry.connector, token) {
        (Some(connector), Some(token)) if !http.insufficient_scope() => {
            connector.renew_rejected(&token, cancel).await.is_ok()
        }
        _ => false,
    };
    if !renewed && let Some(credentials) = &entry.credentials {
        // Remember invalidation too: a deleted token is not a new login on the next request.
        if let Ok(credential) = credentials.load().await {
            *entry.credential.write().unwrap() = credential;
        }
    }
    *entry.state.write().unwrap() = if renewed || entry.connector.is_none() || cancel.is_cancelled()
    {
        McpServerState::Failed
    } else {
        McpServerState::NeedsLogin
    };
}

async fn open(
    config: &McpServerConfig,
    token: Option<String>,
    environment: &BTreeMap<String, String>,
    stderr_directory: Option<&tickloop_harness_base::private_fs::PrivateDirectory>,
) -> Result<(Connection, RunningService<RoleClient, Handler>), String> {
    let changed = Arc::new(tokio::sync::Notify::new());
    let revision = Arc::new(AtomicU64::new(0));
    let handler = Handler {
        changed: changed.clone(),
        revision: revision.clone(),
    };
    let lifecycle = ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_11_25],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    };
    let (service, http) = match &config.transport {
        McpTransport::Http { url, headers } => {
            let endpoint = reqwest::Url::parse(url).map_err(|_| "invalid MCP URL")?;
            let http = McpHttpClient::for_server(
                endpoint,
                headers,
                token.clone(),
                Duration::from_millis(config.timeout_ms),
            )?;
            let mut options = StreamableHttpClientTransportConfig::with_uri(url.clone())
                .reinit_on_expired_session(false);
            if let Some(token) = token {
                options = options.auth_header(token);
            }
            let transport = StreamableHttpClientTransport::with_client(http.clone(), options);
            let service = handler
                .serve_with_lifecycle(transport, lifecycle)
                .await
                .map_err(|_| {
                    if http.insufficient_scope() {
                        "MCP scope required"
                    } else if http.unauthorized() {
                        "MCP login required"
                    } else {
                        "MCP connection failed"
                    }
                })?;
            (service, Some(http))
        }
        McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let mut process = tokio::process::Command::new(command);
            process
                .args(args)
                .env_clear()
                .envs(environment)
                .envs(env)
                .current_dir(cwd);
            let stderr = if let Some(directory) = stderr_directory {
                use sha2::{Digest, Sha256};
                use std::os::unix::fs::OpenOptionsExt;
                directory
                    .create()
                    .map_err(|_| "MCP log directory unavailable")?;
                let file = directory
                    .path()
                    .join(format!("{:x}.log", Sha256::digest(config.name.as_bytes())));
                let log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(file)
                    .map_err(|_| "MCP log file unavailable")?;
                if !log
                    .metadata()
                    .map_err(|_| "MCP log file unavailable")?
                    .is_file()
                {
                    return Err("MCP log must be a regular file".into());
                }
                std::process::Stdio::from(log)
            } else {
                std::process::Stdio::inherit()
            };
            let transport = process::ProcessTransport::spawn(process, stderr)?;
            let service = handler
                .serve_with_lifecycle(transport, lifecycle)
                .await
                .map_err(|_| "MCP connection failed")?;
            (service, None)
        }
        _ => return Err("MCP transport unavailable".into()),
    };
    let capabilities = service
        .peer()
        .peer_info()
        .map(|info| info.capabilities.clone())
        .unwrap_or_default();
    let tools = if capabilities.tools.is_some() {
        service.peer().list_all_tools().await.map_err(|_| {
            if http.as_ref().is_some_and(McpHttpClient::insufficient_scope) {
                "MCP scope required"
            } else if http.as_ref().is_some_and(McpHttpClient::unauthorized) {
                "MCP login required"
            } else {
                "MCP tools unavailable"
            }
        })?
    } else {
        Vec::new()
    };
    let resources = if capabilities.resources.is_some() {
        service
            .peer()
            .list_all_resources()
            .await
            .map_err(|_| {
                if http.as_ref().is_some_and(McpHttpClient::insufficient_scope) {
                    "MCP scope required"
                } else if http.as_ref().is_some_and(McpHttpClient::unauthorized) {
                    "MCP login required"
                } else {
                    "MCP resources unavailable"
                }
            })?
            .into_iter()
            .filter_map(|resource| serde_json::to_value(resource).ok())
            .collect()
    } else {
        Vec::new()
    };
    let (tools, warnings) = catalog::directory(&config.name, tools);
    let instructions = service
        .peer()
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .map(|s| s.chars().take(2048).collect())
        .unwrap_or_default();
    Ok((
        Connection {
            peer: service.peer().clone(),
            revision,
            catalog_revision: AtomicU64::new(0),
            http,
            changed,
            tools: RwLock::new(tools),
            instructions,
            resources,
            warnings: RwLock::new(warnings),
        },
        service,
    ))
}
fn redact_value(entry: &Entry, value: &mut Value) {
    match value {
        Value::String(text) => *text = redact(entry, std::mem::take(text)),
        Value::Array(values) => {
            for value in values {
                redact_value(entry, value);
            }
        }
        Value::Object(values) => {
            let old = std::mem::take(values);
            for (key, mut value) in old {
                redact_value(entry, &mut value);
                values.insert(redact(entry, key), value);
            }
        }
        _ => {}
    }
}
fn redact(entry: &Entry, content: String) -> String {
    let normalizer = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    let mut content = normalizer.normalize(&content).into_owned();
    let mut secrets = entry.environment_secrets.clone();
    if let Some(token) = entry.token.read().unwrap().as_ref() {
        secrets.push(token.secret().to_owned());
    }
    if let Some(token) = entry
        .credential
        .read()
        .unwrap()
        .as_ref()
        .and_then(|saved| saved.token.as_ref())
    {
        secrets.push(token.access_token.clone());
        secrets.extend(token.refresh_token.clone());
    }
    match &entry.config.transport {
        McpTransport::Http { url, headers } => {
            for value in headers.values() {
                secrets.push(value.clone());
                let mut parts = value.split_whitespace();
                if parts.next().is_some_and(|scheme| {
                    scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic")
                }) && let Some(token) = parts.next()
                {
                    secrets.push(token.into());
                }
            }
            if let Ok(url) = reqwest::Url::parse(url) {
                secrets.extend(url.query_pairs().map(|(_, value)| value.into_owned()));
            }
        }
        McpTransport::Stdio { env, args, .. } => {
            secrets.extend(env.values().cloned());
            for argument in args {
                if let Some((_, value)) = argument.split_once('=') {
                    secrets.push(value.into());
                } else if !argument.starts_with('-') {
                    secrets.push(argument.clone());
                }
            }
        }
        _ => {}
    }
    let mut variants = std::collections::BTreeSet::new();
    for secret in secrets.into_iter().filter(|secret| !secret.is_empty()) {
        let secret = normalizer.normalize(&secret).into_owned();
        variants.insert(secret.clone());
        let encoded =
            percent_encoding::utf8_percent_encode(&secret, percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        variants.insert(encoded.clone());
        variants.insert(encoded.replace("%2F", "%2f").replace("%2B", "%2b"));
        variants.insert(secret.bytes().map(|b| format!("%{b:02X}")).collect());
        variants.insert(secret.bytes().map(|b| format!("%{b:02x}")).collect());
        variants.insert(
            serde_json::to_string(&secret)
                .unwrap_or_default()
                .trim_matches('"')
                .to_owned(),
        );
    }
    let mut variants: Vec<_> = variants.into_iter().collect();
    variants.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for secret in variants {
        if !secret.is_empty() {
            content = content.replace(&secret, "[redacted]");
        }
    }
    content
}
