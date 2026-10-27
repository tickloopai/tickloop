//! How a server's connection is opened, served and lost: one connection attempt, following its
//! tool and resource list changes, and what a rejected login does to it.

use super::*;
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt,
    model::{ClientConfig, ProtocolVersion},
    service::{NotificationContext, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

/// The wait before the first reconnect after a failure.
const FIRST_RECONNECT_DELAY: Duration = Duration::from_secs(1);

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

/// A connection that has listed its tools.
pub(super) struct Opened {
    connection: Connection,
    service: RunningService<RoleClient, Handler>,
    tools: BTreeMap<String, catalog::Tool>,
    warnings: Vec<String>,
}

/// After the server rejected the connection's token: renews it on the same connection when the
/// login allows, waits for a new login when it does not, and otherwise opens the connection again.
pub(super) async fn authentication_rejected(
    entry: &Entry,
    connection: &Connection,
    cancel: &CancellationToken,
) {
    let Some(http) = &connection.http else {
        return;
    };
    let token = entry.token.read().unwrap().clone();
    let renewed = match (&entry.connector, token) {
        (Some(connector), Some(token)) if !http.insufficient_scope() => {
            connector.renew_rejected(&token, cancel).await.ok()
        }
        _ => None,
    };
    // Remember invalidation too: a deleted token is not a new login on the next request.
    if let Some(credentials) = &entry.credentials
        && let Ok(credential) = credentials.load().await
    {
        entry.remember_credential(credential);
    }
    if let Some(token) = renewed {
        http.set_token(token.secret());
        entry.remember_token(token);
    } else if entry.connector.is_some() && !cancel.is_cancelled() {
        *entry.state.write().unwrap() = McpServerState::NeedsLogin;
    } else {
        connection.lost.cancel();
    }
}

/// How one connection attempt ended.
pub(super) enum Attempt {
    Opened(Box<Opened>),
    Failed(String),
    NeedsLogin,
    Cancelled,
}
/// Why a connection stopped serving.
pub(super) enum Served {
    /// Cancelled, or waiting for a login: nothing reconnects it.
    Stopped,
    Closed,
}

/// Which remembered tool list a server may start from: its configuration without secret values,
/// and the account its login belongs to.
pub(super) fn tool_list_key(
    config: &McpServerConfig,
    credential: Option<&ConnectorCredential>,
) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut identity = config.clone();
    identity.sensitive_values.clear();
    identity.disabled = false;
    match &mut identity.transport {
        McpTransport::Http { headers, .. } => headers.values_mut().for_each(String::clear),
        McpTransport::Stdio { env, .. } => env.values_mut().for_each(String::clear),
        _ => {}
    }
    let account = credential.map(|saved| (&saved.server_url, &saved.registration.client_id));
    Sha256::digest(serde_json::to_vec(&(identity, account)).expect("configuration serializes"))
        .into()
}

/// How long the `attempt`th reconnect after a failure waits: one second, doubling with each
/// reconnect up to the last of [`MAX_RECONNECTS`].
pub(super) fn reconnect_delay(attempt: u32) -> Duration {
    FIRST_RECONNECT_DELAY * 2u32.pow(attempt.clamp(1, MAX_RECONNECTS) - 1)
}

/// Why opening a connection failed. The variants the connection's next step depends on are
/// named; every other failure keeps our own fixed words.
pub(super) enum OpenFailure {
    LoginRequired,
    ScopeRequired,
    CredentialUnavailable,
    Other(String),
}

impl OpenFailure {
    /// Why the server rejected a request of `http`, or `otherwise` when it did not reject it.
    fn rejection(http: Option<&McpHttpClient>, otherwise: &str) -> Self {
        if http.is_some_and(McpHttpClient::insufficient_scope) {
            Self::ScopeRequired
        } else if http.is_some_and(McpHttpClient::unauthorized) {
            Self::LoginRequired
        } else {
            Self::Other(otherwise.to_owned())
        }
    }

    fn needs_login(&self) -> bool {
        matches!(self, Self::LoginRequired | Self::ScopeRequired)
    }

    fn into_reason(self) -> String {
        match self {
            Self::LoginRequired => "MCP login required".into(),
            Self::ScopeRequired => "MCP scope required".into(),
            Self::CredentialUnavailable => "MCP credential unavailable".into(),
            Self::Other(reason) => reason,
        }
    }
}

impl From<&str> for OpenFailure {
    fn from(reason: &str) -> Self {
        Self::Other(reason.to_owned())
    }
}

impl From<String> for OpenFailure {
    fn from(reason: String) -> Self {
        Self::Other(reason)
    }
}

pub(super) async fn connect(
    entry: &Arc<Entry>,
    slots: &tokio::sync::Semaphore,
    environment: &BTreeMap<String, String>,
    stderr_directory: Option<&tickloop_harness_base::private_fs::PrivateDirectory>,
) -> Attempt {
    let authenticating = AtomicBool::new(false);
    let connect = async {
        let _permit = slots
            .acquire()
            .await
            .map_err(|_| "MCP connection cancelled")?;
        if entry.has_saved_token()
            && let Some(connector) = &entry.connector
        {
            authenticating.store(true, Ordering::SeqCst);
            let token = connector.access_token(&entry.cancel).await;
            if let Some(credentials) = &entry.credentials {
                entry.remember_credential(
                    credentials
                        .load()
                        .await
                        .map_err(|_| OpenFailure::CredentialUnavailable)?,
                );
            }
            entry.remember_token(token.map_err(|error| match error {
                TokenError::NeedsLogin | TokenError::RenewalFailed => OpenFailure::LoginRequired,
                _ => OpenFailure::CredentialUnavailable,
            })?);
        }
        authenticating.store(false, Ordering::SeqCst);
        let token = entry.token.read().unwrap().clone();
        let opened = open(
            entry,
            token.as_ref().map(|token| token.secret().to_owned()),
            environment,
            stderr_directory,
        )
        .await;
        if matches!(opened, Err(OpenFailure::LoginRequired))
            && let (Some(connector), Some(token)) = (&entry.connector, token)
        {
            authenticating.store(true, Ordering::SeqCst);
            let renewed = connector.renew_rejected(&token, &entry.cancel).await;
            if let Some(credentials) = &entry.credentials {
                entry.remember_credential(
                    credentials
                        .load()
                        .await
                        .map_err(|_| OpenFailure::CredentialUnavailable)?,
                );
            }
            let renewed = renewed.map_err(|_| OpenFailure::LoginRequired)?;
            entry.remember_token(renewed.clone());
            authenticating.store(false, Ordering::SeqCst);
            return open(
                entry,
                Some(renewed.secret().to_owned()),
                environment,
                stderr_directory,
            )
            .await;
        }
        opened
    };
    let result = tokio::select! {
        _ = entry.cancel.cancelled() => return Attempt::Cancelled,
        result = tokio::time::timeout(CONNECT_TIMEOUT, connect) => result,
    };
    match result {
        Ok(Ok(opened)) => Attempt::Opened(Box::new(opened)),
        Ok(Err(failure)) if failure.needs_login() && entry.connector.is_some() => {
            Attempt::NeedsLogin
        }
        Ok(Err(failure)) => Attempt::Failed(failure.into_reason()),
        Err(_) if authenticating.load(Ordering::SeqCst) => Attempt::NeedsLogin,
        Err(_) => Attempt::Failed(CONNECTION_TIMED_OUT.into()),
    }
}

/// Publishes an opened connection and follows its tool and resource list changes until it ends.
pub(super) async fn serve(
    entry: &Arc<Entry>,
    opened: Box<Opened>,
    state_changed: &tokio::sync::Notify,
) -> Served {
    let Opened {
        connection,
        service,
        tools,
        warnings,
    } = *opened;
    let connection = Arc::new(connection);
    *entry.connection.write().unwrap() = Some(connection.clone());
    entry.adopt_tools(tools, warnings);
    *entry.failure.write().unwrap() = None;
    entry.reconnecting.store(false, Ordering::SeqCst);
    *entry.state.write().unwrap() = McpServerState::Connected;
    state_changed.notify_waiters();
    let stop = service.cancellation_token();
    let mut waiting = Box::pin(service.waiting());
    let resources_changed = connection
        .resources
        .as_ref()
        .map(|directory| directory.changed.clone());
    loop {
        tokio::select! {
            _ = entry.cancel.cancelled() => {
                stop.cancel();
                let _ = waiting.await;
                return Served::Stopped;
            }
            _ = connection.lost.cancelled() => {
                stop.cancel();
                let _ = waiting.await;
                return Served::Closed;
            }
            _ = &mut waiting => {
                return if *entry.state.read().unwrap() == McpServerState::NeedsLogin {
                    Served::Stopped
                } else {
                    Served::Closed
                };
            }
            _ = connection.changed.notified() => {
                if *entry.state.read().unwrap() != McpServerState::Connected {
                    continue;
                }
                let revision = connection.revision.load(Ordering::SeqCst);
                entry.tool_schemas.write().unwrap().clear();
                let tools = tokio::select! {
                    _ = entry.cancel.cancelled() => continue,
                    tools = tokio::time::timeout(REQUEST_TIMEOUT, connection.peer.list_all_tools()) => tools,
                };
                if connection.http.as_ref().is_some_and(McpHttpClient::unauthorized) {
                    authentication_rejected(entry, &connection, &entry.cancel).await;
                    state_changed.notify_waiters();
                }
                let listed = matches!(tools, Ok(Ok(_)));
                {
                    let mut warnings = connection.warnings.write().unwrap();
                    warnings.retain(|warning| warning != TOOLS_RELIST_FAILED);
                    if !listed {
                        warnings.push(TOOLS_RELIST_FAILED.into());
                    }
                }
                if let Ok(Ok(tools)) = tools {
                    let (mut tools, mut warnings) = catalog::directory(&entry.config, tools);
                    entry.restore_tool_schemas(&mut tools, &mut warnings);
                    entry.adopt_tools(tools, warnings);
                } else {
                    entry.publish_metadata();
                }
                connection.catalog_revision.store(revision, Ordering::SeqCst);
            }
            _ = async { resources_changed.as_ref().unwrap().notified().await }, if resources_changed.is_some() => {
                if *entry.state.read().unwrap() != McpServerState::Connected {
                    continue;
                }
                let directory = connection.resources.as_ref().unwrap();
                let revision = directory.revision.load(Ordering::SeqCst);
                let resources = tokio::select! {
                    _ = entry.cancel.cancelled() => continue,
                    resources = tokio::time::timeout(REQUEST_TIMEOUT, resource_directory(&connection.peer)) => resources,
                };
                if connection.http.as_ref().is_some_and(McpHttpClient::unauthorized) {
                    authentication_rejected(entry, &connection, &entry.cancel).await;
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
                entry.publish_metadata();
            }
        }
    }
}

async fn open(
    entry: &Arc<Entry>,
    token: Option<String>,
    environment: &BTreeMap<String, String>,
    stderr_directory: Option<&tickloop_harness_base::private_fs::PrivateDirectory>,
) -> Result<Opened, OpenFailure> {
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
            // The client sends the token itself, so that a renewed one reaches this connection.
            let options = StreamableHttpClientTransportConfig::with_uri(url.clone())
                .reinit_on_expired_session(false);
            let transport = StreamableHttpClientTransport::with_client(http.clone(), options);
            let service = handler
                .serve_with_lifecycle(transport, lifecycle)
                .await
                .map_err(|_| OpenFailure::rejection(Some(&http), "MCP connection failed"))?;
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
        service
            .peer()
            .list_all_tools()
            .await
            .map_err(|_| OpenFailure::rejection(http.as_ref(), "MCP tools unavailable"))?
    } else {
        Vec::new()
    };
    let resources = match &capabilities.resources {
        Some(capability) => {
            let loaded_revision = resource_revision.load(Ordering::SeqCst);
            let resources = resource_directory(service.peer())
                .await
                .map_err(|_| OpenFailure::rejection(http.as_ref(), RESOURCES_UNAVAILABLE))?;
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
    Ok(Opened {
        connection: Connection {
            peer: service.peer().clone(),
            revision,
            catalog_revision: AtomicU64::new(0),
            http,
            changed,
            instructions,
            resources,
            warnings: RwLock::new(Vec::new()),
            lost: CancellationToken::new(),
        },
        service,
        tools,
        warnings,
    })
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
