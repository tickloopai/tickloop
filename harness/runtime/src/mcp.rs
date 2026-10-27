//! Agent-owned MCP connections. Hosts supply resolved configuration, credentials and capabilities.
mod catalog;
mod process;
mod provider;
mod redaction;
mod snapshot;
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
/// How long the first request waits, once, for servers that are still starting.
const STARTUP_WAIT: Duration = Duration::from_secs(10);
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
    snapshot: RwLock<Arc<snapshot::Catalog>>,
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
    redactor: RwLock<Option<Arc<redaction::Redactor>>>,
    publication: std::sync::Mutex<()>,
    generation: AtomicU64,
    secret_generation: AtomicU64,
    description: RwLock<String>,
    config_failure: RwLock<Option<String>>,
    catalog_builds: Arc<AtomicU64>,
    catalog_micros: Arc<AtomicU64>,
    redactor_builds: Arc<AtomicU64>,
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
        self.refresh_redactor();
    }

    fn remember_token(&self, token: AccessToken) {
        let secret = token.secret().to_owned();
        let mut secrets = self.credential_secrets.write().unwrap();
        if !secrets.contains(&secret) {
            secrets.push(secret);
        }
        drop(secrets);
        *self.token.write().unwrap() = Some(token);
        self.refresh_redactor();
    }

    fn refresh_redactor(&self) {
        let changed = {
            let _publication = self.publication.lock().unwrap();
            if let Some(redactor) = redaction::Redactor::new(self) {
                self.secret_generation.fetch_add(1, Ordering::SeqCst);
                *self.redactor.write().unwrap() = Some(Arc::new(redactor));
                self.redactor_builds.fetch_add(1, Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        *self.description.write().unwrap() = redact(self, self.config.description.clone());
        *self.config_failure.write().unwrap() = match &self.config.transport {
            McpTransport::Failed { reason } => Some(redact(self, reason.clone())),
            _ => None,
        };
        if changed && let Some(connection) = self.connection.read().unwrap().clone() {
            self.publish_catalog(&connection);
        }
    }
}
struct PreparedMcpCall {
    entry: Arc<Entry>,
    connection: Arc<Connection>,
    tool: Arc<snapshot::Tool>,
    generation: u64,
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
    catalog_builds: Arc<AtomicU64>,
    catalog_micros: Arc<AtomicU64>,
    redactor_builds: Arc<AtomicU64>,
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
                catalog_builds: Arc::new(AtomicU64::new(0)),
                catalog_micros: Arc::new(AtomicU64::new(0)),
                redactor_builds: Arc::new(AtomicU64::new(0)),
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
        tokio::select! { _ = cancel.cancelled() => {}, _ = tokio::time::timeout(STARTUP_WAIT, wait) => {} }
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
        let waiting = crate::timing::Phase::new("reconcile_lock_wait");
        let _guard = self.inner.reconcile_lock.lock().await;
        drop(waiting);
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
            let previous_redactor = existing
                .as_ref()
                .and_then(|entry| entry.redactor.read().unwrap().clone());
            let previous_secret_generation = existing
                .as_ref()
                .map_or(0, |entry| entry.secret_generation.load(Ordering::SeqCst));
            let previous_generation = existing
                .as_ref()
                .map_or(0, |entry| entry.generation.load(Ordering::SeqCst));
            let previous_secrets = existing
                .as_ref()
                .map(|entry| entry.credential_secrets.read().unwrap().clone())
                .unwrap_or_default();
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
                    redactor: RwLock::new(previous_redactor),
                    publication: std::sync::Mutex::new(()),
                    generation: AtomicU64::new(previous_generation),
                    secret_generation: AtomicU64::new(previous_secret_generation),
                    description: RwLock::new(String::new()),
                    config_failure: RwLock::new(None),
                    catalog_builds: self.inner.catalog_builds.clone(),
                    catalog_micros: self.inner.catalog_micros.clone(),
                    redactor_builds: self.inner.redactor_builds.clone(),
                    config,
                    token: RwLock::new(None),
                    credential: RwLock::new(None),
                    credential_secrets: RwLock::new(previous_secrets),
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
                    entry.remember_token(token.map_err(|error| match error {
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
                    entry.remember_token(renewed.clone());
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
                    entry.publish_catalog(&connection);
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
                                    entry.publish_catalog(&connection);
                                } else {
                                    connection.tools.write().unwrap().clear();
                                    entry.publish_catalog(&connection);
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
                                drop(warnings);
                                entry.publish_metadata(&connection);
                            }
                        }
                    }

                }
                error => { *entry.failure.write().unwrap() = Some(match &error { Ok(Err(reason)) => reason.clone(), _ => CONNECTION_TIMED_OUT.into() }); *entry.state.write().unwrap() = if (error.is_err() && authenticating.load(Ordering::SeqCst)) || matches!(error, Ok(Err(ref error)) if (error == "MCP login required" || error == "MCP scope required") && entry.connector.is_some()) {McpServerState::NeedsLogin} else {McpServerState::Failed}; state_changed.notify_waiters(); }
            }
        });
    }
    #[cfg(feature = "test-support")]
    pub fn performance_counts(&self) -> (u64, u64) {
        (
            self.inner.catalog_builds.load(Ordering::Relaxed),
            self.inner.redactor_builds.load(Ordering::Relaxed),
        )
    }
    #[cfg(feature = "test-support")]
    pub fn last_catalog_build_micros(&self) -> u64 {
        self.inner.catalog_micros.load(Ordering::Relaxed)
    }

    #[cfg(feature = "test-support")]
    pub fn benchmark_remember_credential(&self, server: &str, credential: ConnectorCredential) {
        let entry = self
            .inner
            .entries
            .read()
            .unwrap()
            .get(server)
            .expect("benchmark server")
            .clone();
        entry.remember_credential(Some(credential));
    }

    #[cfg(feature = "test-support")]
    pub fn benchmark_redact_result(&self, server: &str, value: &mut Value) {
        let entry = self
            .inner
            .entries
            .read()
            .unwrap()
            .get(server)
            .expect("benchmark server")
            .clone();
        redact_value(&entry, value);
    }
    fn catalog(&self) -> Vec<(Arc<Entry>, Arc<Connection>, Arc<snapshot::Tool>)> {
        let _timing = crate::timing::Phase::new("mcp_catalog_read");
        let entries: Vec<_> = self
            .inner
            .entries
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let mut tools = Vec::new();
        for entry in entries {
            if *entry.state.read().unwrap() == McpServerState::NeedsLogin {
                continue;
            }
            let connection = entry.connection.read().unwrap().clone();
            if let Some(connection) = connection {
                let snapshot = connection.snapshot.read().unwrap().clone();
                if connection.revision.load(Ordering::SeqCst) != snapshot.revision
                    || entry.secret_generation.load(Ordering::SeqCst) != snapshot.secret_generation
                {
                    continue;
                }
                tools.extend(
                    snapshot
                        .tools
                        .values()
                        .map(|tool| (entry.clone(), connection.clone(), tool.clone())),
                );
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
                    McpTransport::Failed { .. } => entry.config_failure.read().unwrap().clone(),
                    _ => entry.failure.read().unwrap().clone(),
                }
                .filter(|_| state == McpServerState::Failed);
                McpServerStatus {
                    name: entry.config.name.clone(),
                    source: entry.config.source.clone(),
                    transport,
                    exposure: entry.config.exposure.as_str().into(),
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
                        .map(|(_, _, tool)| (*tool.definition).clone())
                        .collect(),
                    resources: connection
                        .as_ref()
                        .map(|connection| entry.current_catalog(connection).resources.clone())
                        .unwrap_or_default(),
                    instructions: connection
                        .as_ref()
                        .map(|connection| entry.current_catalog(connection).instructions.clone())
                        .unwrap_or_default(),
                    warnings: if let Some(reason) = entry.config_failure.read().unwrap().clone() {
                        vec![reason]
                    } else {
                        connection
                            .as_ref()
                            .map(|connection| entry.current_catalog(connection).warnings.clone())
                            .unwrap_or_default()
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
        let mut target = self
            .catalog()
            .into_iter()
            .find(|(_, _, tool)| tool.definition.name == name);
        let reconnect = target
            .as_ref()
            .filter(|(entry, _, _)| *entry.state.read().unwrap() == McpServerState::Failed)
            .map(|(entry, _, _)| entry.config.name.clone());
        if let Some(server) = &reconnect {
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
                    if !self.inner.entries.read().unwrap().get(server).is_some_and(|entry| *entry.state.read().unwrap() == McpServerState::Connecting) {break;}
                    tokio::select! { _ = cancel.cancelled() => return Err("MCP connection cancelled"), _ = notified => {} }
                }
                Ok(())
            }).await.map_err(|_| "MCP connection timed out")??;
        }
        if reconnect.is_some() {
            target = self
                .catalog()
                .into_iter()
                .find(|(_, _, tool)| tool.definition.name == name);
        }
        let (entry, connection, tool) = target.ok_or("MCP tool is unavailable")?;
        if tool.exposure == tickloop_harness_protocol::mcp::ToolExposure::Hidden {
            return Err("MCP tool is unavailable or hidden".into());
        }
        if !tool.validator.is_valid(&input) {
            return Err("invalid MCP tool arguments".into());
        }
        if *entry.state.read().unwrap() != McpServerState::Connected {
            return Err("MCP connection is unavailable".into());
        }
        let generation = tool.generation;
        Ok(PreparedMcpCall {
            generation,
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
        let current = call.connection.snapshot.read().unwrap().clone();
        if current.generation != call.generation
            || current.secret_generation != call.entry.secret_generation.load(Ordering::SeqCst)
            || !current
                .tools
                .get(&call.tool.definition.name)
                .is_some_and(|tool| tool.fingerprint == call.tool.fingerprint)
        {
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
            generation: _,
        } = call;
        if cancel.is_cancelled() || entry.cancel.is_cancelled() {
            return HostCallResult::NotDispatched("MCP call cancelled before dispatch".into());
        }
        let Some(arguments) = input.as_object() else {
            return HostCallResult::NotDispatched("MCP arguments must be an object".into());
        };
        let request =
            CallToolRequestParams::new(tool.original.clone()).with_arguments(arguments.clone());
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
        let waiting = crate::timing::Phase::new("reconcile_lock_wait");
        let _guard = self.inner.reconcile_lock.lock().await;
        drop(waiting);
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
            snapshot: RwLock::new(Arc::new(snapshot::Catalog::default())),
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
    let redactor = entry
        .redactor
        .read()
        .unwrap()
        .clone()
        .expect("redactor initialized before use");
    redactor.redact(content)
}
