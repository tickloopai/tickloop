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
use serde::Serialize;
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
pub use tickloop_harness_protocol::mcp::{McpServerState, McpServerStatus, McpToolSummary};
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
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SERVER_TEXT_CHARACTERS: usize = 2048;
const RESOURCES_UNAVAILABLE: &str = "MCP resources unavailable";
const CONNECTION_CLOSED: &str = "MCP connection closed";
const CONNECTION_TIMED_OUT: &str = "MCP connection timed out";
struct Handler {
    revision: Arc<AtomicU64>,
    changed: Arc<tokio::sync::Notify>,
    resource_revision: Arc<AtomicU64>,
    resources_changed: Arc<tokio::sync::Notify>,
}
impl ClientHandler for Handler {
    async fn on_tool_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_one();
    }
    async fn on_resource_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.resource_revision.fetch_add(1, Ordering::SeqCst);
        self.resources_changed.notify_one();
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
    /// `None` when the server does not provide resources.
    resources: Option<ResourceDirectory>,
    warnings: RwLock<Vec<String>>,
}
/// The resources a server listed. A server that announces list changes is listed from here while
/// the directory is current; any other server is asked each time.
struct ResourceDirectory {
    follows_notifications: bool,
    changed: Arc<tokio::sync::Notify>,
    revision: Arc<AtomicU64>,
    loaded_revision: AtomicU64,
    resources: RwLock<Vec<Value>>,
}
impl ResourceDirectory {
    fn current(&self) -> Option<Vec<Value>> {
        (self.follows_notifications
            && self.loaded_revision.load(Ordering::SeqCst) == self.revision.load(Ordering::SeqCst))
        .then(|| self.resources.read().unwrap().clone())
    }
}
struct Entry {
    config: McpServerConfig,
    token: RwLock<Option<AccessToken>>,
    credential: RwLock<Option<ConnectorCredential>>,
    credentials: Option<Arc<dyn ConnectorCredentials>>,
    connector: Option<Connector>,
    environment_secrets: Vec<String>,
    credential_secrets: RwLock<Vec<String>>,
    tool_schemas: RwLock<BTreeMap<String, Value>>,
    state: RwLock<McpServerState>,
    connection: RwLock<Option<Arc<Connection>>>,
    /// Why the last connection attempt failed or ended, in our own fixed words.
    failure: RwLock<Option<String>>,
    cancel: CancellationToken,
}
impl Entry {
    fn offers_tools(&self) -> bool {
        use tickloop_harness_protocol::mcp::ToolExposure;
        self.config.exposure != ToolExposure::Hidden
            || self
                .config
                .tool_exposure
                .iter()
                .any(|(_, exposure)| *exposure != ToolExposure::Hidden)
    }

    fn restore_tool_schemas(
        &self,
        tools: &mut BTreeMap<String, catalog::Tool>,
        warnings: &mut Vec<String>,
    ) {
        let mut originals = self.tool_schemas.write().unwrap();
        tools.retain(|name, tool| {
            if !originals.contains_key(&tool.original) {
                warnings.push(format!(
                    "{name}: original input schema unavailable; tool omitted"
                ));
                return false;
            }
            if let Some(schema) = originals.get(&tool.original) {
                if jsonschema::validator_for(schema).is_err() {
                    warnings.push(format!("{name}: invalid input schema; tool omitted"));
                    return false;
                }
                tool.schema = schema.clone();
            }
            true
        });
        originals.retain(|name, _| tools.values().any(|tool| tool.original == *name));
    }

    /// Whether a login for the server's current address is saved.
    fn has_saved_token(&self) -> bool {
        let McpTransport::Http { url, .. } = &self.config.transport else {
            return false;
        };
        self.credential
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|saved| {
                saved.token.is_some()
                    && reqwest::Url::parse(url).is_ok_and(|url| saved.server_url == url.as_str())
            })
    }

    fn remember_credential(&self, credential: Option<ConnectorCredential>) {
        if let Some(token) = credential
            .as_ref()
            .and_then(|credential| credential.token.as_ref())
        {
            let mut secrets = self.credential_secrets.write().unwrap();
            for secret in std::iter::once(&token.access_token).chain(token.refresh_token.as_ref()) {
                if !secrets.contains(secret) {
                    secrets.push(secret.clone());
                }
            }
        }
        *self.credential.write().unwrap() = credential;
    }
}
struct PreparedMcpCall {
    entry: Arc<Entry>,
    connection: Arc<Connection>,
    tool: catalog::Tool,
    input: Value,
}
/// Which unchanged servers a reconcile connects again.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Restart<'a> {
    Failed,
    Nothing,
    Server(&'a str),
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
    startup_waited: AtomicBool,
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
                startup_waited: AtomicBool::new(false),
                state_changed: Arc::new(tokio::sync::Notify::new()),
                local_slots: Arc::new(tokio::sync::Semaphore::new(4)),
                remote_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            }),
        }
    }
    /// The first request waits, at most once, for servers whose tools reach the model directly
    /// or through discovery, so that it does not miss a server that is still starting.
    async fn wait_for_startup(&self, cancel: CancellationToken) {
        if self.inner.startup_waited.swap(true, Ordering::SeqCst) {
            return;
        }
        let wait = async {
            loop {
                let changed = self.inner.state_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let pending = self.inner.entries.read().unwrap().values().any(|entry| {
                    *entry.state.read().unwrap() == McpServerState::Connecting
                        && entry.offers_tools()
                });
                if !pending {
                    break;
                }
                changed.await;
            }
        };
        tokio::select! { _ = cancel.cancelled() => {}, _ = tokio::time::timeout(Duration::from_secs(10), wait) => {} }
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

    /// Takes up `configs`, connecting changed servers and again those that failed.
    pub async fn reconcile(&self, configs: Vec<McpServerConfig>) {
        self.reconcile_restarting(configs, Restart::Failed).await;
    }

    /// Takes up `configs` as [`Self::reconcile`] does, but leaves failed servers failed, so that
    /// reading their state retries nothing.
    pub async fn reconcile_keeping_failures(&self, configs: Vec<McpServerConfig>) {
        self.reconcile_restarting(configs, Restart::Nothing).await;
    }

    /// Reconciles `configs` and connects the server `name` again even when nothing about it
    /// changed; false when no server has that name.
    pub async fn reconnect(&self, configs: Vec<McpServerConfig>, name: &str) -> bool {
        if !configs.iter().any(|config| config.name == name) {
            return false;
        }
        self.reconcile_restarting(configs, Restart::Server(name))
            .await;
        true
    }

    async fn reconcile_restarting(&self, configs: Vec<McpServerConfig>, restart: Restart<'_>) {
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
        for mut config in configs.into_values() {
            if !tickloop_harness_protocol::mcp::is_valid_mcp_server_name(&config.name) {
                config.transport = McpTransport::Failed {
                    reason: "invalid MCP server name: use letters, digits, _ and - without __"
                        .into(),
                };
            }
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
                restart != Restart::Server(config.name.as_str())
                    && entry.config == config
                    && *entry.credential.read().unwrap() == credential
                    && (restart == Restart::Nothing
                        || *entry.state.read().unwrap() != McpServerState::Failed)
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
                let state = if config.disabled {
                    McpServerState::Disabled
                } else {
                    credential_failure.unwrap_or_else(|| match &config.transport {
                        McpTransport::Unsupported { .. } => McpServerState::Unsupported,
                        McpTransport::Failed { .. } => McpServerState::Failed,
                        McpTransport::NeedsConfiguration => McpServerState::NeedsConfiguration,
                        McpTransport::Stdio { .. } if !self.inner.capabilities.local_processes => {
                            McpServerState::Unsupported
                        }
                        _ => McpServerState::Connecting,
                    })
                };
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
                    credential: RwLock::new(None),
                    credential_secrets: RwLock::new(Vec::new()),
                    tool_schemas: RwLock::new(BTreeMap::new()),
                    credentials,
                    connector,
                    environment_secrets,
                    state: RwLock::new(state),
                    connection: RwLock::new(None),
                    failure: RwLock::new(None),
                    cancel: CancellationToken::new(),
                });
                entry.remember_credential(credential);
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
            let authenticating = AtomicBool::new(false);
            let connect = async {
                let _permit = slots.acquire().await.map_err(|_| "MCP connection cancelled")?;
                let has_token = entry.has_saved_token();
                if has_token && let Some(connector) = &entry.connector {
                    authenticating.store(true, Ordering::SeqCst);
                    let token = connector.access_token(&entry.cancel).await;
                    if let Some(credentials) = &entry.credentials {
                        entry.remember_credential(credentials.load().await.map_err(|_| "MCP credential unavailable")?);
                    }
                    *entry.token.write().unwrap() = Some(token.map_err(|error| match error {
                        TokenError::NeedsLogin | TokenError::RenewalFailed => "MCP login required",
                        _ => "MCP credential unavailable",
                    })?);
                }
                authenticating.store(false, Ordering::SeqCst);
                let token = entry.token.read().unwrap().clone();
                let opened = open(&entry, token.as_ref().map(|token| token.secret().to_owned()), &environment, stderr_directory.as_ref()).await;
                if matches!(&opened, Err(error) if error == "MCP login required")
                    && let (Some(connector), Some(token)) = (&entry.connector, token)
                {
                    authenticating.store(true, Ordering::SeqCst);
                    let renewed = connector.renew_rejected(&token, &entry.cancel).await;
                    if let Some(credentials) = &entry.credentials {
                        entry.remember_credential(credentials.load().await.map_err(|_| "MCP credential unavailable")?);
                    }
                    let renewed = renewed.map_err(|_| "MCP login required")?;
                    *entry.token.write().unwrap() = Some(renewed.clone());
                    authenticating.store(false, Ordering::SeqCst);
                    return open(&entry, Some(renewed.secret().to_owned()), &environment, stderr_directory.as_ref()).await;
                }
                opened
            };
            let result = tokio::select! {
                _ = entry.cancel.cancelled() => return,
                result = tokio::time::timeout(CONNECT_TIMEOUT, connect) => result,
            };
            match result {
                Ok(Ok((connection, service))) => {
                    let connection = Arc::new(connection);
                    *entry.connection.write().unwrap() = Some(connection.clone());
                    *entry.state.write().unwrap() = McpServerState::Connected;
                    state_changed.notify_waiters();
                    let stop = service.cancellation_token();
                    let mut waiting = Box::pin(service.waiting());
                    let resources_changed = connection.resources.as_ref().map(|directory| directory.changed.clone());
                    loop {
                        tokio::select! {
                            _ = entry.cancel.cancelled() => { stop.cancel(); let _ = waiting.await; break; }
                            _ = &mut waiting => { if *entry.state.read().unwrap() != McpServerState::NeedsLogin { *entry.state.write().unwrap() = McpServerState::Failed; *entry.failure.write().unwrap() = Some(CONNECTION_CLOSED.into()); } state_changed.notify_waiters(); break; }
                            _ = connection.changed.notified() => {
                                if *entry.state.read().unwrap() != McpServerState::Connected { continue; }
                                let revision = connection.revision.load(Ordering::SeqCst);
                                entry.tool_schemas.write().unwrap().clear();
                                let tools = tokio::select! {
                                    _ = entry.cancel.cancelled() => continue,
                                    tools = tokio::time::timeout(REQUEST_TIMEOUT, connection.peer.list_all_tools()) => tools,
                                };
                                if let Some(http) = connection.http.as_ref().filter(|http| http.unauthorized()) {
                                    authentication_rejected(&entry, http, &entry.cancel).await;
                                    state_changed.notify_waiters();
                                }
                                if let Ok(Ok(tools)) = tools {
                                    let (mut tools, mut warnings) = catalog::directory(&entry.config, tools);
                                    entry.restore_tool_schemas(&mut tools, &mut warnings);
                                    *connection.tools.write().unwrap() = tools;
                                    *connection.warnings.write().unwrap() = warnings;
                                    connection.catalog_revision.store(revision, Ordering::SeqCst);
                                } else {
                                    connection.tools.write().unwrap().clear();
                                    *connection.warnings.write().unwrap() = vec!["MCP tools unavailable".into()];
                                }
                            }
                            _ = async { resources_changed.as_ref().unwrap().notified().await }, if resources_changed.is_some() => {
                                if *entry.state.read().unwrap() != McpServerState::Connected { continue; }
                                let directory = connection.resources.as_ref().unwrap();
                                let revision = directory.revision.load(Ordering::SeqCst);
                                let resources = tokio::select! {
                                    _ = entry.cancel.cancelled() => continue,
                                    resources = tokio::time::timeout(REQUEST_TIMEOUT, resource_directory(&connection.peer)) => resources,
                                };
                                if let Some(http) = connection.http.as_ref().filter(|http| http.unauthorized()) {
                                    authentication_rejected(&entry, http, &entry.cancel).await;
                                    state_changed.notify_waiters();
                                }
                                let mut warnings = connection.warnings.write().unwrap();
                                warnings.retain(|warning| warning != RESOURCES_UNAVAILABLE);
                                if let Ok(Ok(resources)) = resources {
                                    *directory.resources.write().unwrap() = resources;
                                    directory.loaded_revision.store(revision, Ordering::SeqCst);
                                } else {
                                    warnings.push(RESOURCES_UNAVAILABLE.into());
                                }
                            }
                        }
                    }

                }
                error => { *entry.failure.write().unwrap() = Some(match &error { Ok(Err(reason)) => reason.clone(), _ => CONNECTION_TIMED_OUT.into() }); *entry.state.write().unwrap() = if (error.is_err() && authenticating.load(Ordering::SeqCst)) || matches!(error, Ok(Err(ref error)) if (error == "MCP login required" || error == "MCP scope required") && entry.connector.is_some()) {McpServerState::NeedsLogin} else {McpServerState::Failed}; state_changed.notify_waiters(); }
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
                    tool.definition.description =
                        redact_server_text(entry, tool.definition.description);
                    redact_schema(entry, &mut tool.definition.input_schema);
                    if tool.definition.input_schema["type"] != "object"
                        || jsonschema::validator_for(&tool.definition.input_schema).is_err()
                    {
                        continue;
                    }
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
                tool.definition.name = catalog::hashed_name(&entry.config.name, &tool.original);
            }
        }
        tools
    }
    /// Servers that would offer tools but are starting, failed or waiting for login.
    pub fn unavailable_tool_servers(&self) -> Vec<crate::tool_exposure::UnavailableServer> {
        self.inner
            .entries
            .read()
            .unwrap()
            .values()
            .filter(|entry| entry.offers_tools())
            .filter_map(|entry| {
                let state = entry.state.read().unwrap().clone();
                matches!(
                    state,
                    McpServerState::Connecting
                        | McpServerState::Failed
                        | McpServerState::NeedsLogin
                )
                .then(|| crate::tool_exposure::UnavailableServer {
                    name: entry.config.name.clone(),
                    state,
                })
            })
            .collect()
    }

    /// What a View shows of each server, in name order.
    pub fn statuses(&self) -> Vec<McpServerStatus> {
        let catalog = self.catalog();
        self.inner
            .entries
            .read()
            .unwrap()
            .values()
            .map(|entry| {
                let state = entry.state.read().unwrap().clone();
                let (transport, oauth) = match &entry.config.transport {
                    McpTransport::Stdio { .. } => ("stdio".to_owned(), false),
                    McpTransport::Http { .. } => ("http".to_owned(), entry.connector.is_some()),
                    McpTransport::Unsupported { kind } => (kind.clone(), false),
                    McpTransport::NeedsConfiguration | McpTransport::Failed { .. } => {
                        (String::new(), false)
                    }
                };
                let failure = match &entry.config.transport {
                    McpTransport::Failed { reason } => Some(reason.clone()),
                    _ => entry.failure.read().unwrap().clone(),
                }
                .filter(|_| state == McpServerState::Failed)
                .map(|reason| redact(entry, reason));
                McpServerStatus {
                    name: entry.config.name.clone(),
                    source: entry.config.source.clone(),
                    transport,
                    exposure: entry
                        .config
                        .exposure
                        .for_host(self.inner.capabilities.scripts)
                        .as_str()
                        .into(),
                    tools: catalog
                        .iter()
                        .filter(|(owner, _, _)| Arc::ptr_eq(owner, entry))
                        .map(|(_, _, tool)| McpToolSummary {
                            name: tool.definition.name.clone(),
                            description: tool.definition.description.clone(),
                        })
                        .collect(),
                    failure,
                    oauth,
                    logged_in: oauth && entry.has_saved_token(),
                    state,
                }
            })
            .collect()
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
                        .and_then(|c| c.resources.as_ref())
                        .map(|directory| {
                            redacted_resources(entry, directory.resources.read().unwrap().clone())
                        })
                        .unwrap_or_default(),
                    instructions: connection
                        .as_ref()
                        .map(|c| redact_server_text(entry, c.instructions.clone()))
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
        if !jsonschema::validator_for(&tool.schema).is_ok_and(|schema| schema.is_valid(&input)) {
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
                && redact_server_text(&call.entry, tool.definition.description.clone())
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
                redact_value(&entry, &mut value);
                if value["isError"] != true && crate::nested_calls::is_script() {
                    crate::nested_calls::capture_script_value(value.clone());
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
        if self
            .inner
            .entries
            .read()
            .unwrap()
            .get(server)
            .is_some_and(|entry| *entry.state.read().unwrap() != McpServerState::Connected)
        {
            return Err("MCP connection unavailable".into());
        }
        if cursor.is_none()
            && let Some(entry) = self.inner.entries.read().unwrap().get(server)
            && let Some(resources) = entry
                .connection
                .read()
                .unwrap()
                .as_ref()
                .and_then(|connection| connection.resources.as_ref()?.current())
        {
            return Ok(json!({"resources": redacted_resources(entry, resources)}));
        }
        let params = cursor.map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
        self.resource_request(server, "resources/list", params, cancel)
            .await
    }
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let result = self
            .resource_request(server, "resources/read", json!({"uri":uri}), cancel)
            .await?;
        if !result["contents"].is_array() {
            return Err("invalid MCP resource content".into());
        }
        Ok(result)
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
        if *entry.state.read().unwrap() != McpServerState::Connected {
            return Err("MCP connection unavailable".into());
        }
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
            result = tokio::time::timeout(REQUEST_TIMEOUT, connection.peer.send_request(request)) => result,
        };
        if let Some(http) = connection.http.as_ref().filter(|http| http.unauthorized()) {
            authentication_rejected(&entry, http, &cancel).await;
            self.inner.state_changed.notify_waiters();
        }
        let response = response
            .map_err(|_| "MCP resource request timed out")?
            .map_err(|_| "MCP resource request failed")?;
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
            entry.remember_credential(credential);
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
    entry: &Arc<Entry>,
    token: Option<String>,
    environment: &BTreeMap<String, String>,
    stderr_directory: Option<&tickloop_harness_base::private_fs::PrivateDirectory>,
) -> Result<(Connection, RunningService<RoleClient, Handler>), String> {
    let config = &entry.config;
    let changed = Arc::new(tokio::sync::Notify::new());
    let resources_changed = Arc::new(tokio::sync::Notify::new());
    let revision = Arc::new(AtomicU64::new(0));
    let resource_revision = Arc::new(AtomicU64::new(0));
    let handler = Handler {
        changed: changed.clone(),
        revision: revision.clone(),
        resource_revision: resource_revision.clone(),
        resources_changed: resources_changed.clone(),
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
            let redact_entry = Arc::downgrade(entry);
            let http = http.with_redactor(Arc::new(move |value, part| {
                if let Some(entry) = redact_entry.upgrade() {
                    redact_wire_value(&entry, value, part);
                } else {
                    *value = Value::Null;
                }
            }));
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
            // Arbitrary stderr bytes cannot be safely redacted across chunk boundaries.
            let stderr = if !config.sensitive_values.is_empty() {
                std::process::Stdio::null()
            } else if let Some(directory) = stderr_directory {
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
            let redact_entry = Arc::downgrade(entry);
            let transport = process::ProcessTransport::spawn(
                process,
                stderr,
                Arc::new(move |value, part| {
                    if let Some(entry) = redact_entry.upgrade() {
                        redact_wire_value(&entry, value, part);
                    } else {
                        *value = Value::Null;
                    }
                }),
            )?;
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
        entry.tool_schemas.write().unwrap().clear();
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
    let resources = match &capabilities.resources {
        Some(capability) => {
            let loaded_revision = resource_revision.load(Ordering::SeqCst);
            let resources = resource_directory(service.peer()).await.map_err(|_| {
                if http.as_ref().is_some_and(McpHttpClient::insufficient_scope) {
                    "MCP scope required"
                } else if http.as_ref().is_some_and(McpHttpClient::unauthorized) {
                    "MCP login required"
                } else {
                    RESOURCES_UNAVAILABLE
                }
            })?;
            Some(ResourceDirectory {
                follows_notifications: capability.list_changed == Some(true),
                changed: resources_changed,
                revision: resource_revision,
                loaded_revision: AtomicU64::new(loaded_revision),
                resources: RwLock::new(resources),
            })
        }
        None => None,
    };
    let (mut tools, mut warnings) = catalog::directory(config, tools);
    entry.restore_tool_schemas(&mut tools, &mut warnings);
    let instructions = service
        .peer()
        .peer_info()
        .and_then(|info| info.instructions.clone())
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
async fn resource_directory(
    peer: &rmcp::service::Peer<RoleClient>,
) -> Result<Vec<Value>, rmcp::ServiceError> {
    Ok(peer
        .list_all_resources()
        .await?
        .into_iter()
        .filter_map(|resource| serde_json::to_value(resource).ok())
        .collect())
}
fn redacted_resources(entry: &Entry, mut resources: Vec<Value>) -> Vec<Value> {
    for resource in &mut resources {
        redact_value(entry, resource);
    }
    resources
}
fn redact_wire_value(entry: &Entry, value: &mut Value, part: crate::mcp_http::ResponsePart) {
    if matches!(part, crate::mcp_http::ResponsePart::ToolDirectory)
        && let Some(tools) = value.get_mut("tools").and_then(Value::as_array_mut)
    {
        let mut originals = entry.tool_schemas.write().unwrap();
        for tool in tools {
            if let (Some(name), Some(schema)) = (
                tool.get("name").and_then(Value::as_str),
                tool.get("inputSchema"),
            ) {
                originals.insert(name.to_owned(), schema.clone());
            }
            redact_schema(entry, tool);
        }
    }
    let capabilities = value
        .get("protocolVersion")
        .and_then(Value::as_str)
        .and_then(|_| value.get("capabilities"))
        .and_then(|value| {
            serde_json::from_value::<rmcp::model::ServerCapabilities>(value.clone()).ok()
        })
        .and_then(|value| serde_json::to_value(value).ok())
        .map(|mut capabilities| {
            for field in ["experimental", "extensions", "logging", "completions"] {
                if let Some(value) = capabilities.get_mut(field) {
                    redact_value(entry, value);
                }
            }
            capabilities
        });
    redact_value(entry, value);
    if let Some(capabilities) = capabilities
        && let Some(fields) = value.as_object_mut()
    {
        fields.insert("capabilities".into(), capabilities);
    }
}

fn redact_schema(entry: &Entry, schema: &mut Value) {
    fn remove_sensitive_constraints(entry: &Entry, value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.retain(|_, value| {
                    !entry.config.sensitive_values.contains(value)
                        && !value.as_array().is_some_and(|items| {
                            items
                                .iter()
                                .any(|item| entry.config.sensitive_values.contains(item))
                        })
                });
                for value in fields.values_mut() {
                    remove_sensitive_constraints(entry, value);
                }
            }
            Value::Array(items) => {
                for value in items {
                    remove_sensitive_constraints(entry, value);
                }
            }
            _ => {}
        }
    }
    remove_sensitive_constraints(entry, schema);
    redact_value(entry, schema);
}

fn redact_value(entry: &Entry, value: &mut Value) {
    if matches!(value, Value::Number(_) | Value::Bool(_))
        && entry.config.sensitive_values.contains(value)
    {
        *value = Value::String("[redacted]".into());
        return;
    }
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
fn redact_server_text(entry: &Entry, content: String) -> String {
    redact(entry, content)
        .chars()
        .take(MAX_SERVER_TEXT_CHARACTERS)
        .collect()
}

fn redact(entry: &Entry, content: String) -> String {
    let normalizer = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    let mut content = normalizer.normalize(&content).into_owned();
    let mut secrets = entry.environment_secrets.clone();
    secrets.extend(entry.credential_secrets.read().unwrap().iter().cloned());
    secrets.extend(entry.config.sensitive_values.iter().map(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    }));
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
