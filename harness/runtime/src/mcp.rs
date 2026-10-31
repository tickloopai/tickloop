//! Agent-owned MCP connections. Hosts supply resolved configuration, credentials and capabilities.
mod catalog;
mod connection;
pub(crate) mod negotiation;
mod process;
mod provider;
mod redaction;
mod snapshot;
use crate::{host_tools::HostCallResult, mcp_http::McpHttpClient};
use base64::Engine;
use connection::{
    Attempt, Served, authentication_rejected, connect, reconnect_delay, serve, tool_list_key,
};
pub use provider::{McpConfiguration, McpToolProvider};
use rmcp::{RoleClient, model::CallToolRequestParams};
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
/// How long a Worker's first request waits for the servers still connecting: one deadline,
/// computed once and shared by every server, including those whose tools are listed directly.
const STARTUP_GRACE: Duration = Duration::from_secs(2);
/// How many times a remote server reconnects after one failure before it stays failed.
const MAX_RECONNECTS: u32 = 5;
/// How long a connection must stay up before its reconnects count from zero again, so that a
/// server that connects and drops at once still stops after the capped reconnects.
const RECONNECT_STABLE_AFTER: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How soon before it expires a login is renewed on its open connection.
const TOKEN_RENEWAL_WINDOW_SECONDS: u64 = 30;
const MAX_SERVER_TEXT_CHARACTERS: usize = 2048;
const RESOURCES_UNAVAILABLE: &str = "MCP resources unavailable";
const CONNECTION_CLOSED: &str = "MCP connection closed";
const CONNECTION_TIMED_OUT: &str = "MCP connection timed out";
const CONNECTION_UNAVAILABLE: &str = "MCP connection is unavailable";
const TOOLS_RELIST_FAILED: &str =
    "MCP tools could not be listed again; the last listed tools remain";
struct Connection {
    peer: rmcp::service::Peer<RoleClient>,
    revision: Arc<AtomicU64>,
    catalog_revision: AtomicU64,
    http: Option<McpHttpClient>,
    changed: Arc<tokio::sync::Notify>,
    instructions: String,
    /// `None` when the server does not provide resources.
    resources: Option<ResourceDirectory>,
    warnings: RwLock<Vec<String>>,
    /// Cancelled when a call finds the connection unusable, so that it is opened again.
    lost: CancellationToken,
}
/// The tools a server listed last, kept in memory by server name while it reconnects, renews its
/// login or fails to list them again, so that its tools do not come and go.
#[derive(PartialEq)]
struct ToolList {
    /// The server's configuration without secret values, and the account it lists tools for.
    key: [u8; 32],
    tools: BTreeMap<String, catalog::Tool>,
    warnings: Vec<String>,
}
type ToolLists = Arc<std::sync::Mutex<BTreeMap<String, Arc<ToolList>>>>;
/// When each server began a connection attempt and when each attempt failed.
#[cfg(feature = "test-support")]
type ConnectionLog = Arc<std::sync::Mutex<Vec<(String, ConnectionEvent, tokio::time::Instant)>>>;
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    Attempt,
    Failure,
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
    snapshot: RwLock<Arc<snapshot::Catalog>>,
    tools: RwLock<Option<Arc<ToolList>>>,
    tool_lists: ToolLists,
    tool_list_key: [u8; 32],
    /// A failed remote server that will connect again after its backoff.
    reconnecting: AtomicBool,
    renewing: AtomicBool,
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
    /// How the tools reach the model: the configuration's at first, then the user's latest
    /// choice, which takes effect without reconnecting.
    exposure: RwLock<snapshot::Exposure>,
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
    legacy: Arc<AtomicBool>,
}
impl Entry {
    /// Whether its last listed tools reach the model: while it is connected, connecting or
    /// waiting to reconnect.
    fn lists_tools(&self) -> bool {
        match *self.state.read().unwrap() {
            McpServerState::Connected | McpServerState::Connecting => true,
            McpServerState::Failed => self.reconnecting.load(Ordering::SeqCst),
            _ => false,
        }
    }

    /// Takes up a fresh listing. An unchanged one keeps the published catalog and its generation.
    fn adopt_tools(&self, tools: BTreeMap<String, catalog::Tool>, warnings: Vec<String>) {
        let list = ToolList {
            key: self.tool_list_key,
            tools,
            warnings,
        };
        let unchanged = self.tools.read().unwrap().as_deref() == Some(&list)
            && self.snapshot.read().unwrap().secret_generation
                == self.secret_generation.load(Ordering::SeqCst);
        if unchanged {
            self.publish_metadata();
            return;
        }
        let list = Arc::new(list);
        *self.tools.write().unwrap() = Some(list.clone());
        self.tool_lists
            .lock()
            .unwrap()
            .insert(self.config.name.clone(), list);
        self.publish_catalog();
    }

    /// Whether a changed saved login can replace the token on the open connection instead of
    /// connecting again: the same account at the same address, still logged in.
    fn renews_in_place(&self, credential: Option<&ConnectorCredential>) -> bool {
        let current = self.credential.read().unwrap();
        *self.state.read().unwrap() == McpServerState::Connected
            && self.connector.is_some()
            && matches!((current.as_ref(), credential), (Some(current), Some(next))
                if current.server_url == next.server_url
                    && current.registration == next.registration
                    && next.token.is_some())
    }

    fn token_expires_soon(&self) -> bool {
        self.credential
            .read()
            .unwrap()
            .as_ref()
            .and_then(|saved| saved.token.as_ref())
            .is_some_and(|token| {
                token.expires_at
                    <= tickloop_harness_base::private_fs::unix_time_milliseconds() / 1000
                        + TOKEN_RENEWAL_WINDOW_SECONDS
            })
    }

    fn offers_tools(&self) -> bool {
        self.exposure().offers_tools()
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
        if changed {
            self.publish_catalog();
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
    startup_deadline: std::sync::OnceLock<tokio::time::Instant>,
    /// The servers a first request waits for right now; empty when none is waiting.
    awaited: tokio::sync::watch::Sender<Vec<String>>,
    tool_lists: ToolLists,
    /// How many times the user changed each server's exposure while it stayed configured.
    exposure_revisions: RwLock<BTreeMap<String, u64>>,
    #[cfg(feature = "test-support")]
    connection_log: ConnectionLog,
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
                startup_deadline: std::sync::OnceLock::new(),
                awaited: tokio::sync::watch::Sender::new(Vec::new()),
                tool_lists: Arc::default(),
                exposure_revisions: RwLock::default(),
                #[cfg(feature = "test-support")]
                connection_log: Arc::default(),
                state_changed: Arc::new(tokio::sync::Notify::new()),
                local_slots: Arc::new(tokio::sync::Semaphore::new(4)),
                remote_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            }),
        }
    }
    /// Waits until no server that offers tools is still connecting, at most until the Worker's
    /// startup deadline: two seconds after its first request began waiting, shared by every
    /// server and every Chat. Once the deadline has passed, requests do not wait.
    async fn wait_for_startup(&self, cancel: CancellationToken) {
        let deadline = *self
            .inner
            .startup_deadline
            .get_or_init(|| tokio::time::Instant::now() + STARTUP_GRACE);
        loop {
            let changed = self.inner.state_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let mut pending = self.connecting_tool_servers();
            if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                pending.clear();
            }
            let waiting = !pending.is_empty();
            self.inner.awaited.send_if_modified(|awaited| {
                let modified = *awaited != pending;
                *awaited = pending;
                modified
            });
            if !waiting {
                return;
            }
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tokio::time::sleep_until(deadline) => {}
                _ = changed => {}
            }
        }
    }

    fn connecting_tool_servers(&self) -> Vec<String> {
        self.inner
            .entries
            .read()
            .unwrap()
            .values()
            .filter(|entry| {
                *entry.state.read().unwrap() == McpServerState::Connecting && entry.offers_tools()
            })
            .map(|entry| entry.config.name.clone())
            .collect()
    }

    /// Every configured server's entry, in name order.
    fn entries(&self) -> Vec<Arc<Entry>> {
        self.inner
            .entries
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    /// The servers a request is waiting for, as they change.
    pub fn awaited_servers(&self) -> tokio::sync::watch::Receiver<Vec<String>> {
        self.inner.awaited.subscribe()
    }

    /// Renews, on their open connections, the logins that expire within
    /// [`TOKEN_RENEWAL_WINDOW_SECONDS`].
    async fn renew_expiring_tokens(&self) {
        let expiring: Vec<_> = self
            .inner
            .entries
            .read()
            .unwrap()
            .values()
            .filter(|entry| {
                *entry.state.read().unwrap() == McpServerState::Connected
                    && entry.connector.is_some()
                    && entry.token_expires_soon()
            })
            .cloned()
            .collect();
        for entry in expiring {
            self.renew_in_place(entry).await;
        }
    }

    /// Replaces the token of an open connection with the saved login, renewed first when it
    /// expires soon; the server's tools stay listed throughout.
    async fn renew_in_place(&self, entry: Arc<Entry>) {
        if entry.renewing.swap(true, Ordering::SeqCst) {
            return;
        }
        let state_changed = self.inner.state_changed.clone();
        self.inner.tasks.lock().await.spawn(async move {
            let Some(connector) = &entry.connector else {
                return;
            };
            let token = tokio::select! {
                _ = entry.cancel.cancelled() => return,
                token = connector.access_token(&entry.cancel) => token,
            };
            if let Some(credentials) = &entry.credentials
                && let Ok(credential) = credentials.load().await
            {
                entry.remember_credential(credential);
            }
            match token {
                Ok(token) => {
                    if let Some(http) = entry
                        .connection
                        .read()
                        .unwrap()
                        .as_ref()
                        .and_then(|connection| connection.http.as_ref())
                    {
                        http.set_token(token.secret());
                    }
                    entry.remember_token(token);
                }
                Err(TokenError::NeedsLogin) => {
                    *entry.state.write().unwrap() = McpServerState::NeedsLogin;
                    state_changed.notify_waiters();
                }
                // A later request tries again; the server rejects an expired token meanwhile.
                Err(_) => {}
            }
            entry.renewing.store(false, Ordering::SeqCst);
        });
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

    /// Takes up `configs`, connecting the servers that changed. Failed servers stay failed until
    /// their configuration or login changes or [`Self::reconnect`] names them.
    pub async fn reconcile(&self, configs: Vec<McpServerConfig>) {
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
            let legacy = existing
                .as_ref()
                .filter(|entry| connects_alike(&entry.config, &config))
                .map(|entry| entry.legacy.clone())
                .unwrap_or_default();
            let entry = if let Some(entry) = existing.filter(|entry| {
                restart != Restart::Server(config.name.as_str())
                    && connects_alike(&entry.config, &config)
                    && (*entry.credential.read().unwrap() == credential
                        || entry.renews_in_place(credential.as_ref()))
            }) {
                if *entry.credential.read().unwrap() != credential {
                    entry.remember_credential(credential);
                    self.renew_in_place(entry.clone()).await;
                }
                if entry.change_exposure(snapshot::Exposure::of(&config)) {
                    *self
                        .inner
                        .exposure_revisions
                        .write()
                        .unwrap()
                        .entry(config.name.clone())
                        .or_default() += 1;
                }
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
                let tool_list_key = tool_list_key(&config, credential.as_ref());
                let cached = self
                    .inner
                    .tool_lists
                    .lock()
                    .unwrap()
                    .get(&config.name)
                    .filter(|list| list.key == tool_list_key)
                    .cloned();
                let entry = Arc::new(Entry {
                    legacy,
                    snapshot: RwLock::new(Arc::new(snapshot::Catalog::default())),
                    tools: RwLock::new(cached),
                    tool_lists: self.inner.tool_lists.clone(),
                    tool_list_key,
                    reconnecting: AtomicBool::new(false),
                    renewing: AtomicBool::new(false),
                    redactor: RwLock::new(previous_redactor),
                    publication: std::sync::Mutex::new(()),
                    generation: AtomicU64::new(previous_generation),
                    secret_generation: AtomicU64::new(previous_secret_generation),
                    description: RwLock::new(String::new()),
                    config_failure: RwLock::new(None),
                    catalog_builds: self.inner.catalog_builds.clone(),
                    catalog_micros: self.inner.catalog_micros.clone(),
                    redactor_builds: self.inner.redactor_builds.clone(),
                    exposure: RwLock::new(snapshot::Exposure::of(&config)),
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
                    // A remembered tool list is offered while the server connects, unless the new
                    // secrets already published it.
                    if entry.snapshot.read().unwrap().secret_generation
                        != entry.secret_generation.load(Ordering::SeqCst)
                    {
                        entry.publish_catalog();
                    }
                    self.start(entry.clone()).await;
                }
                entry
            };
            next.insert(entry.config.name.clone(), entry);
        }
        self.inner
            .tool_lists
            .lock()
            .unwrap()
            .retain(|name, _| next.contains_key(name));
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
    /// Connects `entry` in the background and keeps it connected: a remote server that
    /// disconnects or fails to connect tries again after 1, 2, 4, 8 and 16 seconds and then stays
    /// failed, counting from zero again only after a connection stayed up for
    /// [`RECONNECT_STABLE_AFTER`]; a local program that fails or exits is not started again.
    async fn start(&self, entry: Arc<Entry>) {
        let environment = self.inner.environment.clone();
        let stderr_directory = self.inner.stderr_directory.clone();
        let state_changed = self.inner.state_changed.clone();
        let slots = if matches!(entry.config.transport, McpTransport::Stdio { .. }) {
            self.inner.local_slots.clone()
        } else {
            self.inner.remote_slots.clone()
        };
        #[cfg(feature = "test-support")]
        let record = {
            let log = self.inner.connection_log.clone();
            let name = entry.config.name.clone();
            move |event| {
                log.lock()
                    .unwrap()
                    .push((name.clone(), event, tokio::time::Instant::now()));
            }
        };
        self.inner.tasks.lock().await.spawn(async move {
            let mut reconnects = 0;
            loop {
                #[cfg(feature = "test-support")]
                record(ConnectionEvent::Attempt);
                let failure =
                    match connect(&entry, &slots, &environment, stderr_directory.as_ref()).await {
                        Attempt::Cancelled => return,
                        Attempt::NeedsLogin(reason) => {
                            *entry.failure.write().unwrap() = Some(reason);
                            *entry.state.write().unwrap() = McpServerState::NeedsLogin;
                            state_changed.notify_waiters();
                            return;
                        }
                        Attempt::Failed(reason) => reason,
                        Attempt::Opened(opened) => {
                            let opened_at = tokio::time::Instant::now();
                            let served = serve(&entry, opened, &state_changed).await;
                            if opened_at.elapsed() >= RECONNECT_STABLE_AFTER {
                                reconnects = 0;
                            }
                            match served {
                                Served::Stopped => return,
                                Served::Closed => CONNECTION_CLOSED.to_owned(),
                            }
                        }
                    };
                *entry.connection.write().unwrap() = None;
                *entry.failure.write().unwrap() = Some(failure);
                let retries = matches!(entry.config.transport, McpTransport::Http { .. })
                    && reconnects < MAX_RECONNECTS;
                entry.reconnecting.store(retries, Ordering::SeqCst);
                *entry.state.write().unwrap() = McpServerState::Failed;
                state_changed.notify_waiters();
                #[cfg(feature = "test-support")]
                record(ConnectionEvent::Failure);
                if !retries {
                    return;
                }
                reconnects += 1;
                tokio::select! {
                    _ = entry.cancel.cancelled() => return,
                    _ = tokio::time::sleep(reconnect_delay(reconnects)) => {}
                }
                *entry.state.write().unwrap() = McpServerState::Connecting;
                state_changed.notify_waiters();
            }
        });
    }
    /// When the server `name` began each connection attempt and when each one failed.
    #[cfg(feature = "test-support")]
    pub fn connection_events(&self, name: &str) -> Vec<(ConnectionEvent, tokio::time::Instant)> {
        self.inner
            .connection_log
            .lock()
            .unwrap()
            .iter()
            .filter(|(server, _, _)| server == name)
            .map(|(_, event, at)| (*event, *at))
            .collect()
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
    /// The tools of every server whose last listed tools reach the model.
    fn catalog(&self) -> Vec<(Arc<Entry>, Arc<snapshot::Tool>)> {
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
            if !entry.lists_tools() {
                continue;
            }
            let snapshot = entry.snapshot.read().unwrap().clone();
            if entry.secret_generation.load(Ordering::SeqCst) != snapshot.secret_generation {
                continue;
            }
            tools.extend(
                snapshot
                    .tools
                    .values()
                    .map(|tool| (entry.clone(), tool.clone())),
            );
        }
        tools
    }
    /// How many times the user changed each server's exposure, by server name; a change shows in
    /// the next request even during a Run.
    pub fn exposure_revisions(&self) -> BTreeMap<String, u64> {
        self.inner.exposure_revisions.read().unwrap().clone()
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

    /// The status of the server `name` once it has stopped connecting, with the tools it listed;
    /// `None` when no server has that name. It waits at most as long as a connection may take, so
    /// a server still connecting after that is reported as connecting.
    pub async fn settled_status(&self, name: &str) -> Option<McpServerStatus> {
        let settled = async {
            loop {
                let changed = self.inner.state_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let connecting =
                    self.inner
                        .entries
                        .read()
                        .unwrap()
                        .get(name)
                        .is_some_and(|entry| {
                            *entry.state.read().unwrap() == McpServerState::Connecting
                        });
                if !connecting {
                    break;
                }
                changed.await;
            }
        };
        let _ = tokio::time::timeout(CONNECT_TIMEOUT + REQUEST_TIMEOUT, settled).await;
        self.statuses()
            .into_iter()
            .find(|status| status.name == name)
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
                    exposure: entry.exposure().server.as_str().into(),
                    tools: catalog
                        .iter()
                        .filter(|(owner, _)| Arc::ptr_eq(owner, entry))
                        .map(|(_, tool)| McpToolSummary {
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
                let published = entry.current_catalog();
                McpServerSnapshot {
                    name: entry.config.name.clone(),
                    source: entry.config.source.clone(),
                    state: entry.state.read().unwrap().clone(),
                    tools: catalog
                        .iter()
                        .filter(|(owner, _)| Arc::ptr_eq(owner, entry))
                        .map(|(_, tool)| (*tool.definition).clone())
                        .collect(),
                    resources: published.resources.clone(),
                    instructions: published.instructions.clone(),
                    warnings: if let Some(reason) = entry.config_failure.read().unwrap().clone() {
                        vec![reason]
                    } else {
                        published.warnings.clone()
                    },
                }
            })
            .collect()
    }
    /// Resolves a call against the current tools. While the tool's server connects or connects
    /// again, the call waits for it, at most for the connection timeout and until cancelled.
    async fn prepare_ready(
        &self,
        name: &str,
        input: Value,
        cancel: &CancellationToken,
    ) -> Result<PreparedMcpCall, String> {
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            let changed = self.inner.state_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (entry, tool) = self
                .catalog()
                .into_iter()
                .find(|(_, tool)| tool.definition.name == name)
                .ok_or("MCP tool is unavailable")?;
            if tool.exposure == tickloop_harness_protocol::mcp::ToolExposure::Hidden {
                return Err("MCP tool is unavailable or hidden".into());
            }
            if !tool.validator.is_valid(&input) {
                return Err("invalid MCP tool arguments".into());
            }
            let state = entry.state.read().unwrap().clone();
            let connection = entry
                .connection
                .read()
                .unwrap()
                .clone()
                .filter(|connection| !connection.lost.is_cancelled());
            match (state, connection) {
                (McpServerState::Connected, Some(connection)) => {
                    return Ok(PreparedMcpCall {
                        generation: tool.generation,
                        entry,
                        connection,
                        tool,
                        input,
                    });
                }
                (McpServerState::Connecting | McpServerState::Connected, _) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err("MCP connection cancelled".into()),
                        _ = tokio::time::sleep_until(deadline) => return Err(CONNECTION_TIMED_OUT.into()),
                        _ = changed => {}
                    }
                }
                _ => return Err(CONNECTION_UNAVAILABLE.into()),
            }
        }
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
        if call.connection.lost.is_cancelled()
            || !call
                .entry
                .connection
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|connection| Arc::ptr_eq(connection, &call.connection))
        {
            return Err("MCP connection changed before dispatch".into());
        }
        if call.connection.revision.load(Ordering::SeqCst)
            != call.connection.catalog_revision.load(Ordering::SeqCst)
        {
            return Err("MCP tool directory changed before dispatch".into());
        }
        let current = call.entry.snapshot.read().unwrap().clone();
        if current.generation != call.generation
            || current.secret_generation != call.entry.secret_generation.load(Ordering::SeqCst)
            || current
                .tools
                .get(&call.tool.definition.name)
                .is_none_or(|tool| tool.fingerprint != call.tool.fingerprint)
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
            authentication_rejected(&entry, &connection, &cancel).await;
            self.inner.state_changed.notify_waiters();
        } else if connection.http.is_some() && matches!(response, Ok(Err(_))) {
            // A local program's broken pipe ends its connection by itself.
            entry.reconnecting.store(true, Ordering::SeqCst);
            *entry.state.write().unwrap() = McpServerState::Failed;
            connection.lost.cancel();
            self.inner.state_changed.notify_waiters();
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
        if connection
            .http
            .as_ref()
            .is_some_and(McpHttpClient::unauthorized)
        {
            authentication_rejected(&entry, &connection, &cancel).await;
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
/// Whether `next` connects as `current` does: the two differ at most in how the tools reach the
/// model, which changes without reconnecting.
fn connects_alike(current: &McpServerConfig, next: &McpServerConfig) -> bool {
    let mut next = next.clone();
    next.exposure = current.exposure;
    next.tool_exposure.clone_from(&current.tool_exposure);
    *current == next
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
