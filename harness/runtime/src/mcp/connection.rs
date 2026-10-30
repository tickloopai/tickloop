//! How a server's connection is opened, served and lost: one connection attempt, following its
//! tool and resource list changes, and what a rejected login does to it.

use super::*;
use rmcp::{
    ClientHandler,
    model::{ClientConfig, ProtocolVersion},
    service::{NotificationContext, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

/// The wait before the first reconnect after a failure.
const FIRST_RECONNECT_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(super) struct Handler {
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
        ClientConfig::default().with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
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
/// A host's login was already asked for a new token by the request itself.
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
            connector.renew_saved_rejected(&token, cancel).await.ok()
        }
        _ => None,
    };
    // A token of another login is not taken up here: that login waits for an entry of its own.
    if renewed.is_some_and(|saved| entry.take_up_token(saved, Some(connection))) {
        return;
    }
    remember_invalidation(entry).await;
    if entry.logs_in() && !cancel.is_cancelled() {
        entry.needs_login_on(connection);
    } else {
        connection.lost.cancel();
    }
}

/// Remembers the entry's saved login as it stands after its token could not be used, such as
/// with the token deleted, so that the next reconcile does not take it for a new login. Another
/// login is left for an entry of its own.
pub(super) async fn remember_invalidation(entry: &Entry) {
    if let Some(credentials) = &entry.credentials
        && let Ok(Some(credential)) = credentials.load().await
        && entry.takes_up(&credential)
    {
        #[cfg(feature = "test-support")]
        super::test_pauses::wait(
            super::test_pauses::PausePoint::Invalidation,
            &entry.config.name,
        );
        entry.take_up_credential(credential);
    }
}

/// How one connection attempt ended.
pub(super) enum Attempt {
    Opened(Box<Opened>),
    Failed(String),
    /// The server needs a login, for the reason given.
    NeedsLogin(String),
    Cancelled,
}
/// Why a connection stopped serving.
pub(super) enum Served {
    /// Cancelled, or waiting for a login: nothing reconnects it.
    Stopped,
    Closed,
}

/// Which remembered tool list a server may start from: its configuration without secret values
/// or exposure, and the account its login belongs to.
pub(super) fn tool_list_key(
    config: &McpServerConfig,
    credential: Option<&ConnectorCredential>,
    bearer: Option<&Bearer>,
) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut identity = super::server_identity(config);
    identity.sensitive_values.clear();
    identity.disabled = false;
    identity.exposure = Default::default();
    identity.tool_exposure.clear();
    match &mut identity.transport {
        McpTransport::Http { headers, .. } => headers.values_mut().for_each(String::clear),
        McpTransport::Stdio { env, .. } => env.values_mut().for_each(String::clear),
        _ => {}
    }
    let account = credential
        .map(|saved| {
            (
                saved.server_url.as_str(),
                saved.registration.client_id.as_str(),
            )
        })
        .or_else(|| bearer.map(|bearer| ("host", bearer.account.as_str())));
    Sha256::digest(serde_json::to_vec(&(identity, account)).expect("configuration serializes"))
        .into()
}

/// How long the `attempt`th reconnect after a failure waits: one second, doubling with each
/// reconnect up to the last of [`MAX_RECONNECTS`].
pub(super) fn reconnect_delay(attempt: u32) -> Duration {
    FIRST_RECONNECT_DELAY * 2u32.pow(attempt.clamp(1, MAX_RECONNECTS) - 1)
}

const LOGIN_REQUIRED: &str = "MCP login required";
const SCOPE_REQUIRED: &str = "MCP scope required";
const CONNECTION_FAILED: &str = "MCP connection failed";

/// Why opening a connection failed. The variants the connection's next step depends on are
/// named, with the redacted cause the server's answer gave when there is one; every other
/// failure keeps our own fixed words, followed by such a cause.
pub(super) enum OpenFailure {
    LoginRequired(Option<String>),
    ScopeRequired(Option<String>),
    CredentialUnavailable,
    Other(String),
}

impl OpenFailure {
    /// Why the server rejected a request of `http`, or `otherwise` when it did not reject it.
    fn rejection(http: Option<&McpHttpClient>, otherwise: &str) -> Self {
        if http.is_some_and(McpHttpClient::insufficient_scope) {
            Self::ScopeRequired(None)
        } else if http.is_some_and(McpHttpClient::unauthorized) {
            Self::LoginRequired(None)
        } else {
            Self::Other(otherwise.to_owned())
        }
    }

    /// Why connecting through `http` failed with the redacted `cause`.
    fn connection(http: Option<&McpHttpClient>, cause: String) -> Self {
        if http.is_some_and(McpHttpClient::insufficient_scope) {
            Self::ScopeRequired(Some(cause))
        } else if http.is_some_and(McpHttpClient::unauthorized) {
            Self::LoginRequired(Some(cause))
        } else {
            Self::Other(format!("{CONNECTION_FAILED}: {cause}"))
        }
    }

    fn needs_login(&self) -> bool {
        matches!(self, Self::LoginRequired(_) | Self::ScopeRequired(_))
    }

    fn into_reason(self) -> String {
        let with_cause = |category: &str, cause: Option<String>| match cause {
            Some(cause) => format!("{category}: {cause}"),
            None => category.to_owned(),
        };
        match self {
            Self::LoginRequired(cause) => with_cause(LOGIN_REQUIRED, cause),
            Self::ScopeRequired(cause) => with_cause(SCOPE_REQUIRED, cause),
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
        if let Some(bearer) = &entry.bearer {
            match bearer.source.bearer().await {
                Ok(Some(token)) if !token.is_empty() && !bearer.was_rejected(&token) => {}
                Ok(_) => return Err(OpenFailure::LoginRequired(None)),
                Err(_) => return Err(OpenFailure::CredentialUnavailable),
            }
        }
        if entry.has_saved_token()
            && let Some(connector) = &entry.connector
        {
            authenticating.store(true, Ordering::SeqCst);
            let saved = match connector.saved_access_token(&entry.cancel).await {
                Ok(saved) => saved,
                Err(error) => {
                    remember_invalidation(entry).await;
                    return Err(match error {
                        TokenError::NeedsLogin | TokenError::RenewalFailed => {
                            OpenFailure::LoginRequired(None)
                        }
                        _ => OpenFailure::CredentialUnavailable,
                    });
                }
            };
            // A new connection may carry another login: what its calls reach changes with it.
            entry.adopt_token(saved);
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
        let rejected = match opened {
            Err(failure @ OpenFailure::LoginRequired(_)) => failure,
            opened => return opened,
        };
        let (Some(connector), Some(token)) = (&entry.connector, token) else {
            return Err(rejected);
        };
        authenticating.store(true, Ordering::SeqCst);
        let renewed = match connector.renew_saved_rejected(&token, &entry.cancel).await {
            Ok(renewed) => renewed,
            Err(_) => {
                remember_invalidation(entry).await;
                return Err(rejected);
            }
        };
        let secret = renewed.token.secret().to_owned();
        entry.adopt_token(renewed);
        authenticating.store(false, Ordering::SeqCst);
        open(entry, Some(secret), environment, stderr_directory).await
    };
    let result = tokio::select! {
        _ = entry.cancel.cancelled() => return Attempt::Cancelled,
        result = tokio::time::timeout(CONNECT_TIMEOUT, connect) => result,
    };
    match result {
        Ok(Ok(opened)) => Attempt::Opened(Box::new(opened)),
        Ok(Err(failure)) if failure.needs_login() && entry.logs_in() => {
            Attempt::NeedsLogin(failure.into_reason())
        }
        Ok(Err(failure)) => Attempt::Failed(failure.into_reason()),
        Err(_) if authenticating.load(Ordering::SeqCst) => {
            Attempt::NeedsLogin(CONNECTION_TIMED_OUT.into())
        }
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
    {
        // Under the login switch, so a stale failure of the old connection cannot follow it.
        let _switch = entry.login_switch.lock().unwrap();
        *entry.connection.write().unwrap() = Some(connection.clone());
        entry.adopt_tools(tools, warnings);
        *entry.failure.write().unwrap() = None;
        entry.reconnecting.store(false, Ordering::SeqCst);
        *entry.state.write().unwrap() = McpServerState::Connected;
    }
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
                    let (mut tools, mut warnings) = catalog::directory(&entry.config.name, tools);
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

fn stdio_transport(
    entry: &Arc<Entry>,
    environment: &BTreeMap<String, String>,
    stderr_directory: Option<&tickloop_harness_base::private_fs::PrivateDirectory>,
    probe: &negotiation::ProbeState,
) -> Result<process::ProcessTransport, String> {
    let McpTransport::Stdio {
        command,
        args,
        env,
        cwd,
    } = &entry.config.transport
    else {
        return Err("MCP transport unavailable".into());
    };
    let config = &entry.config;
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
    process::ProcessTransport::spawn(
        process,
        stderr,
        Arc::new(move |value, part| {
            if let Some(entry) = redact_entry.upgrade() {
                redact_wire_value(&entry, value, part);
            } else {
                *value = Value::Null;
            }
        }),
        probe.clone(),
    )
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
    let probe = negotiation::ProbeState::default();
    let (service, http) = match &config.transport {
        McpTransport::Http { url, headers } => {
            let endpoint = reqwest::Url::parse(url).map_err(|_| "invalid MCP URL")?;
            let mut http = McpHttpClient::for_server(
                endpoint,
                headers,
                token.clone(),
                Duration::from_millis(config.timeout_ms),
                entry.reach,
            )?;
            if let Some(bearer) = &entry.bearer {
                let remember_entry = Arc::downgrade(entry);
                http = http.with_host_bearer(crate::mcp_http::ConnectionBearer {
                    source: bearer.source.clone(),
                    sending: Arc::new(move |token| {
                        if let Some(entry) = remember_entry.upgrade() {
                            entry.remember_host_token(token);
                        }
                    }),
                    rejected: bearer.rejected.clone(),
                });
            }
            let redact_entry = Arc::downgrade(entry);
            let http =
                http.with_probe(probe.clone())
                    .with_redactor(Arc::new(move |value, part| {
                        if let Some(entry) = redact_entry.upgrade() {
                            redact_wire_value(&entry, value, part);
                        } else {
                            *value = Value::Null;
                        }
                    }));
            // The client sends the token itself, so that a renewed one reaches this connection.
            let options = StreamableHttpClientTransportConfig::with_uri(url.clone())
                .reinit_on_expired_session(false);
            let service = negotiation::connect(entry, handler, &probe, true, || {
                Ok(StreamableHttpClientTransport::with_client(
                    http.clone(),
                    options.clone(),
                ))
            })
            .await
            .map_err(|cause| OpenFailure::connection(Some(&http), redact(entry, cause)))?;
            (service, Some(http))
        }
        McpTransport::Stdio { .. } => {
            let service = negotiation::connect(entry, handler, &probe, false, || {
                stdio_transport(entry, environment, stderr_directory, &probe)
            })
            .await
            .map_err(|cause| OpenFailure::connection(None, redact(entry, cause)))?;
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
    let (mut tools, mut warnings) = catalog::directory(&config.name, tools);
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
