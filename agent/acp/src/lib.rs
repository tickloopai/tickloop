//! Stable ACP v1 over stdio. Execution and permissions remain in AgentAccess.
//!
//! The official SDK is pinned to 3.4.0, without any unstable features. Its request callbacks
//! block dispatch: long operations are spawned so cancellation and permission replies can pass.
mod configuration;
mod content;
mod history;
mod stdio;
mod transport;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, ConnectionTo, Error,
    schema::{
        ProtocolVersion,
        v1::{
            CancelNotification, ClientRequest, LoadSessionRequest, NewSessionRequest,
            RequestPermissionRequest,
        },
    },
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tickloop_agent_client::managed::{ManagedClient, ManagedSession};
use tickloop_agent_model::ModelConfigExt;
use tickloop_agent_protocol::{
    AccessError, ChatStatus, DirectoryTrustPolicy, Event, EventKind, ExecutionMode,
    ExecutionProfile, LaunchSpec, LiveChatSnapshot, ObservationChange, PermissionRequest, Receipt,
    SessionMcpServer, UserInput, validate_session_mcp_servers,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_SESSIONS: usize = 16;
const MAX_PROMPT: usize = 256 * 1024;
const MAX_TURN_OUTPUT: usize = 2 * 1024 * 1024;
const MAX_TURN_THOUGHT: usize = 1024 * 1024;
const MAX_TOOL_CONTENT: usize = 32 * 1024;
const TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// CLI entry. stdout is exclusively ACP; errors are returned to the executable for stderr.
/// The explicit trust flag applies only to the startup directory, never an arbitrary client cwd.
pub async fn run(arguments: Vec<String>) -> Result<(), String> {
    let options = Options::parse(arguments)?;
    if options.help {
        eprintln!(
            "Usage: tickloop acp [--working-directory-is-trusted] [--mode auto|ask|fullAccess]\n\nUses existing provider settings. Configure/sign in with /provider in the Console.\n--mode sets each session's starting mode and the highest mode it may select; default auto."
        );
        return Ok(());
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let client = tickloop_agent_client::managed::connect(executable)
        .await
        .map_err(|error| error.to_string())?;
    let mut launch = LaunchSpec::embedded(options.startup_folder.clone());
    launch.profile = ExecutionProfile::Acp;
    launch.host_options = tickloop_agent_protocol::AgentHostOptions::product_defaults();
    launch.host_options.execution_mode_override = Some(options.mode);
    launch.config_root = tickloop_agent_config::config_root_directory();
    if let Some(root) = &launch.config_root {
        launch.model = tickloop_agent_config::UserSettings::in_root(root)
            .load()
            .configured_model()?;
    }
    launch.host_options.directory_trust = DirectoryTrustPolicy::RequireUserTrust;
    use std::os::unix::ffi::OsStrExt;
    launch.environment = std::env::vars_os()
        .map(|(name, value)| tickloop_agent_protocol::EnvironmentValue {
            name: name.as_os_str().as_bytes().to_vec(),
            value: value.as_os_str().as_bytes().to_vec(),
        })
        .collect();
    let (input, output) =
        stdio::open().map_err(|error| format!("cannot open ACP stdio: {error}"))?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|error| error.to_string())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| error.to_string())?;
    let shutdown = CancellationToken::new();
    let signal_shutdown = shutdown.clone();
    let signals = tokio::spawn(async move {
        tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        signal_shutdown.cancel();
    });
    let result = serve_until(
        client,
        launch,
        options.trust_startup,
        input,
        output,
        shutdown.cancelled_owned(),
    )
    .await;
    signals.abort();
    result
}

struct Options {
    startup_folder: PathBuf,
    trust_startup: bool,
    mode: ExecutionMode,
    help: bool,
}
impl Options {
    fn parse(arguments: Vec<String>) -> Result<Self, String> {
        let mut options = Self {
            startup_folder: std::env::current_dir()
                .and_then(|path| path.canonicalize())
                .map_err(|error| error.to_string())?,
            trust_startup: false,
            mode: ExecutionMode::Auto,
            help: false,
        };
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--working-directory-is-trusted" => options.trust_startup = true,
                "--mode" => {
                    options.mode = match arguments.next().as_deref() {
                        Some("auto") => ExecutionMode::Auto,
                        Some("ask" | "acceptEdits" | "accept-edits") => ExecutionMode::AcceptEdits,
                        Some("fullAccess" | "full-access") => ExecutionMode::FullAccess,
                        _ => {
                            return Err("--mode requires auto, ask or fullAccess (not plan)".into());
                        }
                    };
                }
                "--help" | "-h" => options.help = true,
                _ => return Err(format!("unknown ACP argument: {argument}")),
            }
        }
        Ok(options)
    }
}

struct Session {
    managed: ManagedSession,
    active: Mutex<Option<CancellationToken>>,
    mcp_servers: Vec<SessionMcpServer>,
    commands: Mutex<Vec<Value>>,
    published: Mutex<Published>,
    output_budget: transport::OutputBudget,
    _slot: OwnedSemaphorePermit,
}
/// The session title and context usage last sent to the editor.
#[derive(Default)]
struct Published {
    title: Option<String>,
    usage: Option<(u64, u64)>,
}
impl Session {
    /// Title and context usage outlive any one turn; each is sent only when it changed.
    fn publish_status(
        &self,
        connection: &ConnectionTo<Client>,
        id: &str,
        snapshot: &LiveChatSnapshot,
    ) -> Result<(), Error> {
        let status = snapshot.status().chat;
        let usage = context_usage(&status);
        let mut published = self.published.lock().expect("ACP status");
        if status.generated_title.is_some() && status.generated_title != published.title {
            self.output_budget.send(
                connection,
                id,
                json!({"sessionUpdate":"session_info_update", "title":status.generated_title}),
            )?;
            published.title = status.generated_title;
        }
        if let Some((used, size)) = usage
            && usage != published.usage
        {
            self.output_budget.send(
                connection,
                id,
                json!({"sessionUpdate":"usage_update", "used":used, "size":size}),
            )?;
            published.usage = usage;
        }
        Ok(())
    }
}
/// Console's context meter: estimated active-context tokens against the model's window.
fn context_usage(status: &ChatStatus) -> Option<(u64, u64)> {
    let size = status
        .model
        .maximum_context_window_tokens()
        .or(status.context_window_tokens)
        .filter(|size| *size > 0)?;
    Some((status.context_tokens?, size as u64))
}
struct State {
    client: ManagedClient,
    launch: LaunchSpec,
    trust_startup: bool,
    startup_identity: (u64, u64),
    initialized: AtomicBool,
    shutting_down: AtomicBool,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    slots: Arc<Semaphore>,
    output_budget: transport::OutputBudget,
}

/// Injectable IO and AgentAccess connection for protocol tests and non-Console hosts.
/// `trust_startup` trusts only `launch.folder` after canonicalization; the client's cwd is not proof.
pub async fn serve<I, O>(
    client: ManagedClient,
    launch: LaunchSpec,
    trust_startup: bool,
    input: I,
    output: O,
) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    serve_until(
        client,
        launch,
        trust_startup,
        input,
        output,
        std::future::pending(),
    )
    .await
}

/// As `serve`, with an explicit host shutdown signal. Owned sessions are released before return.
pub async fn serve_until<I, O, F>(
    client: ManagedClient,
    mut launch: LaunchSpec,
    trust_startup: bool,
    input: I,
    output: O,
    shutdown: F,
) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
    F: std::future::Future<Output = ()> + Send,
{
    launch.folder = launch
        .folder
        .canonicalize()
        .map_err(|error| error.to_string())?;
    launch.profile = ExecutionProfile::Acp;
    launch.exact_chat = None;
    launch.host_work_context = None;
    launch.host_options.directory_trust = DirectoryTrustPolicy::RequireUserTrust;
    use std::os::unix::fs::MetadataExt;
    let startup_metadata = std::fs::metadata(&launch.folder).map_err(|error| error.to_string())?;
    let startup_identity = (startup_metadata.dev(), startup_metadata.ino());
    let output_budget = transport::OutputBudget::default();
    let state = Arc::new(State {
        client,
        launch,
        trust_startup,
        startup_identity,
        initialized: AtomicBool::new(false),
        shutting_down: AtomicBool::new(false),
        sessions: Mutex::new(HashMap::new()),
        slots: Arc::new(Semaphore::new(MAX_SESSIONS)),
        output_budget: output_budget.clone(),
    });
    let request_state = state.clone();
    let cancel_state = state.clone();
    let admission = Arc::new(Mutex::new(transport::Admission::default()));
    let cancel_admission = admission.clone();
    let builder = Agent.builder().name("tickloop-acp")
        .on_receive_request(async move |request: ClientRequest, responder, connection| {
            let state = request_state.clone();
            match request {
                ClientRequest::InitializeRequest(request) => {
                    if state.initialized.swap(true, Ordering::AcqRel) {
                        return responder.respond_with_error(invalid("already initialized"));
                    }
                    let _requested_version = request.protocol_version;
                    // ACP negotiation selects our supported stable version, even for newer clients.
                    responder.respond(json!({
                        "protocolVersion": ProtocolVersion::V1,
                        "agentInfo": {"name":"tickloop", "version":env!("CARGO_PKG_VERSION")},
                        "agentCapabilities": {"loadSession":true, "promptCapabilities":{"image":true,"embeddedContext":true}, "mcpCapabilities":{}, "sessionCapabilities":{"list":{}}},
                        "authMethods":[]
                    }))
                }
                ClientRequest::NewSessionRequest(request) => {
                    if !state.initialized.load(Ordering::Acquire) {
                        return responder.respond_with_error(invalid("initialize first"));
                    }
                    let slot = match state.slots.clone().try_acquire_owned() {
                        Ok(slot) => slot,
                        Err(_) => return responder.respond_with_error(invalid("ACP session limit reached")),
                    };
                    let output_connection = connection.clone();
                    connection.spawn(async move {
                        let result = async {
                            let value = state.new_session(request, slot).await?;
                            let id = value["sessionId"].as_str().ok_or_else(|| invalid("invalid session identity"))?.to_owned();
                            let session = state.session(&id)?;
                            let prepared = async {
                                let commands = configuration::available_commands(&session.managed).await?;
                                let snapshot = session.managed.snapshot().await.map_err(access_error)?;
                                Ok::<_, Error>((commands, snapshot))
                            }.await;
                            session.active.lock().expect("ACP active").take();
                            match prepared {
                                Ok((commands, snapshot)) => {
                                    *session.commands.lock().expect("ACP commands") = commands.clone();
                                    Ok((value, id, session, commands, snapshot))
                                }
                                Err(error) => {
                                    state.sessions.lock().expect("ACP sessions").remove(&id);
                                    session.managed.release().await.map_err(access_error)?;
                                    Err(error)
                                }
                            }
                        }.await;
                        let (value, id, session, commands, snapshot) = match result {
                            Ok(created) => created,
                            Err(error) => return responder.respond_with_error(error),
                        };
                        responder.respond(value)?;
                        // Editors register a session from this response and drop earlier updates.
                        state.output_budget.send(
                            &output_connection,
                            &id,
                            json!({"sessionUpdate":"available_commands_update","availableCommands":commands}),
                        )?;
                        session.publish_status(&output_connection, &id, &snapshot)
                    })
                }
                ClientRequest::LoadSessionRequest(request) => {
                    if !state.initialized.load(Ordering::Acquire) { return responder.respond_with_error(invalid("initialize first")); }
                    let slot = match state.slots.clone().try_acquire_owned() {
                        Ok(slot) => slot,
                        Err(_) => return responder.respond_with_error(invalid("ACP session limit reached")),
                    };
                    let output_connection = connection.clone();
                    connection.spawn(async move {
                        match state.load_session(request, slot, &output_connection).await {
                            Ok(value) => responder.respond(value),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                }
                ClientRequest::ListSessionsRequest(request) => {
                    if !state.initialized.load(Ordering::Acquire) { return responder.respond_with_error(invalid("initialize first")); }
                    connection.spawn(async move {
                        let result = async {
                            let cwd = request.cwd.as_deref().unwrap_or(&state.launch.folder);
                            let launch = state.session_launch(cwd)?;
                            let page = state.client.list(launch, request.cursor).await.map_err(access_error)?;
                            let sessions: Vec<_> = page.sessions.into_iter().map(|session| json!({"sessionId":session.session_id.to_string(),"cwd":session.cwd,"title":session.title,"updatedAt":session.updated_at})).collect();
                            Ok::<_, Error>(json!({"sessions":sessions,"nextCursor":page.next_cursor}))
                        }.await;
                        match result { Ok(value) => responder.respond(value), Err(error) => responder.respond_with_error(error) }
                    })
                }
                ClientRequest::SetSessionConfigOptionRequest(request) => {
                    let session = match state.session(&request.session_id.to_string()) { Ok(session) => session, Err(error) => return responder.respond_with_error(error) };
                    {
                        let mut active = session.active.lock().expect("ACP active");
                        if active.is_some() { return responder.respond_with_error(invalid("session is busy")); }
                        *active = Some(CancellationToken::new());
                    }
                    let output_connection = connection.clone();
                    connection.spawn(async move {
                        let result = async {
                            let value = serde_json::to_value(&request).map_err(Error::into_internal_error)?;
                            let id = request.session_id.to_string();
                            let result = configuration::configure(&session.managed, value["configId"].as_str().ok_or_else(|| invalid("invalid configuration ID"))?, value["value"].clone()).await?;
                            session.output_budget.send(&output_connection, &id, json!({"sessionUpdate":"config_option_update","configOptions":result["configOptions"]}))?;
                            Ok::<_,Error>(result)
                        }.await;
                        session.active.lock().expect("ACP active").take();
                        match result { Ok(value) => responder.respond(value), Err(error) => responder.respond_with_error(error) }
                    })
                }
                ClientRequest::SetSessionModeRequest(request) => {
                    let session = match state.session(&request.session_id.to_string()) { Ok(session) => session, Err(error) => return responder.respond_with_error(error) };
                    {
                        let mut active = session.active.lock().expect("ACP active");
                        if active.is_some() { return responder.respond_with_error(invalid("session is busy")); }
                        *active = Some(CancellationToken::new());
                    }
                    let output_connection = connection.clone();
                    connection.spawn(async move {
                        let result = async {
                            let id = request.session_id.to_string();
                            let result = configuration::set_mode(&session.managed, &request.mode_id.to_string()).await?;
                            session.output_budget.send(&output_connection, &id, json!({"sessionUpdate":"config_option_update","configOptions":result["configOptions"]}))?;
                            session.output_budget.send(&output_connection, &id, json!({"sessionUpdate":"current_mode_update","currentModeId":request.mode_id}))?;
                            Ok::<_,Error>(json!({}))
                        }.await;
                        session.active.lock().expect("ACP active").take();
                        match result { Ok(value) => responder.respond(value), Err(error) => responder.respond_with_error(error) }
                    })
                }
                ClientRequest::PromptRequest(request) => {
                    let session = match state.session(&request.session_id.to_string()) {
                        Ok(session) => session,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let cancel = CancellationToken::new();
                    {
                        let mut active = session.active.lock().expect("ACP active prompt lock");
                        if active.is_some() {
                            return responder.respond_with_error(invalid("a prompt is already active in this session"));
                        }
                        *active = Some(cancel.clone());
                    }
                    let output_connection = connection.clone();
                    connection.spawn(async move {
                        let prepared = tokio::select! {
                            biased;
                            () = cancel.cancelled() => Ok(None),
                            prepared = content::prompt_input(&request) => prepared.map(Some),
                        };
                        let result = match prepared {
                            Ok(Some(input)) => prompt(session.clone(), request.session_id.to_string(), input, cancel, output_connection).await,
                            Ok(None) => Ok(json!({"stopReason":"cancelled"})),
                            Err(error) => Err(error),
                        };
                        session.active.lock().expect("ACP active prompt lock").take();
                        match result {
                            Ok(value) => responder.respond(value),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                }
                ClientRequest::AuthenticateRequest(_) => responder.respond_with_error(
                    Error::new(-32000, "Configure/sign in to an existing provider with /provider in the Tickloop Console")),
                _ => responder.respond_with_error(Error::method_not_found()),
            }
        }, agent_client_protocol::on_receive_request!())
        .on_receive_notification(async move |notification: CancelNotification, _connection| {
            cancel_admission.lock().expect("ACP admission lock").acknowledge_cancel();
            if let Ok(session) = cancel_state.session(&notification.session_id.to_string())
                && let Some(cancel) = session.active.lock().expect("ACP active prompt lock").as_ref() {
                cancel.cancel();
            }
            Ok(())
        }, agent_client_protocol::on_receive_notification!());
    let protocol = tokio::select! {
        biased;
        () = shutdown => Err("ACP connection terminated by host signal".to_owned()),
        result = transport::connect(input, output, admission, output_budget, builder) => result.map_err(|error| error.to_string()),
    };
    let sessions: Vec<_> = {
        let mut sessions = state.sessions.lock().expect("ACP session lock");
        state.shutting_down.store(true, Ordering::Release);
        sessions.drain().map(|(_, session)| session).collect()
    };
    // Never stop the daemon. Release only the executions created by this connection.
    let cleanup = futures::future::join_all(sessions.into_iter().map(|session| async move {
        if let Some(cancel) = session
            .active
            .lock()
            .expect("ACP active prompt lock")
            .as_ref()
        {
            cancel.cancel();
        }
        session
            .managed
            .release()
            .await
            .map_err(|error| error.to_string())
    }))
    .await;
    let cleanup_error = cleanup.into_iter().find_map(Result::err);
    match (protocol, cleanup_error) {
        (Err(error), Some(cleanup)) => {
            Err(format!("{error}; owned session release failed: {cleanup}"))
        }
        (Err(error), None) => Err(error),
        (Ok(()), Some(error)) => Err(format!("owned session release failed: {error}")),
        (Ok(()), None) => Ok(()),
    }
}

impl State {
    fn session(&self, id: &str) -> Result<Arc<Session>, Error> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(invalid("initialize first"));
        }
        self.sessions
            .lock()
            .expect("ACP session lock")
            .get(id)
            .cloned()
            .ok_or_else(|| invalid("unknown ACP session"))
    }

    fn session_launch(&self, cwd: &std::path::Path) -> Result<LaunchSpec, Error> {
        if !cwd.is_absolute() {
            return Err(invalid("session cwd must be absolute"));
        }
        let folder = cwd
            .canonicalize()
            .map_err(|_| invalid("invalid session cwd"))?;
        if !folder.is_dir() {
            return Err(invalid("session cwd is not a directory"));
        }
        use std::os::unix::fs::MetadataExt;
        let metadata =
            std::fs::metadata(&folder).map_err(|_| invalid("cannot inspect session cwd"))?;
        let mut launch = self.launch.clone();
        launch.host_options.directory_trust = if self.trust_startup
            && folder == self.launch.folder
            && (metadata.dev(), metadata.ino()) == self.startup_identity
        {
            DirectoryTrustPolicy::CallerManaged
        } else {
            DirectoryTrustPolicy::RequireUserTrust
        };
        launch.folder = folder;
        Ok(launch)
    }

    async fn new_session(
        &self,
        request: NewSessionRequest,
        slot: OwnedSemaphorePermit,
    ) -> Result<Value, Error> {
        let mut launch = self.session_launch(&request.cwd)?;
        let mcp_servers = session_mcp_servers(&request.mcp_servers, &launch.folder)?;
        launch.session_mcp_servers = mcp_servers.clone();
        let managed = self.client.create(launch).await.map_err(access_error)?;
        self.insert_session(managed, mcp_servers, slot).await
    }

    async fn load_session(
        &self,
        request: LoadSessionRequest,
        slot: OwnedSemaphorePermit,
        connection: &ConnectionTo<Client>,
    ) -> Result<Value, Error> {
        let id = request.session_id.to_string();
        let uuid = Uuid::parse_str(&id).map_err(|_| invalid("invalid ACP session ID"))?;
        if self
            .sessions
            .lock()
            .expect("ACP sessions")
            .contains_key(&id)
        {
            return Err(invalid("session is already attached"));
        }
        let mut launch = self.session_launch(&request.cwd)?;
        let mcp_servers = session_mcp_servers(&request.mcp_servers, &launch.folder)?;
        launch.session_mcp_servers = mcp_servers.clone();
        let managed = self.client.load(uuid, launch).await.map_err(access_error)?;
        let mut response = self.insert_session(managed, mcp_servers, slot).await?;
        let session = self.session(&id)?;
        let cancel = session
            .active
            .lock()
            .expect("ACP active")
            .as_ref()
            .expect("loading session guard")
            .clone();
        let result = tokio::time::timeout(TURN_TIMEOUT, async {
            history::replay(
                &session.managed,
                &id,
                &cancel,
                connection,
                &session.output_budget,
            )
            .await?;
            self.commands(&session, &id, connection).await?;
            let snapshot = session.managed.snapshot().await.map_err(access_error)?;
            session.publish_status(connection, &id, &snapshot)?;
            if cancel.is_cancelled() {
                return Err(invalid("session history replay cancelled"));
            }
            Ok::<_, Error>(())
        })
        .await
        .unwrap_or_else(|_| Err(invalid("session history replay timed out")));
        session.active.lock().expect("ACP active").take();
        if let Err(error) = result {
            self.sessions.lock().expect("ACP sessions").remove(&id);
            session.managed.release().await.map_err(access_error)?;
            return Err(error);
        }
        response
            .as_object_mut()
            .expect("ACP response")
            .remove("sessionId");
        Ok(response)
    }

    async fn commands(
        &self,
        session: &Session,
        id: &str,
        connection: &ConnectionTo<Client>,
    ) -> Result<Vec<Value>, Error> {
        let commands = configuration::available_commands(&session.managed).await?;
        self.output_budget.send(
            connection,
            id,
            json!({"sessionUpdate":"available_commands_update","availableCommands":commands}),
        )?;
        *session.commands.lock().expect("ACP commands") = commands.clone();
        Ok(commands)
    }

    async fn insert_session(
        &self,
        managed: ManagedSession,
        mcp_servers: Vec<SessionMcpServer>,
        slot: OwnedSemaphorePermit,
    ) -> Result<Value, Error> {
        let snapshot = match managed.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                managed.release().await.map_err(access_error)?;
                return Err(access_error(error));
            }
        };
        let id = snapshot.conversation.id.to_string();
        let mut response = match configuration::response(&managed).await {
            Ok(value) => value,
            Err(error) => {
                managed.release().await.map_err(access_error)?;
                return Err(error);
            }
        };
        let session = Arc::new(Session {
            managed,
            // Publish busy atomically with the session; initialization/history is not a prompt window.
            active: Mutex::new(Some(CancellationToken::new())),
            mcp_servers,
            commands: Mutex::new(Vec::new()),
            published: Mutex::default(),
            output_budget: self.output_budget.clone(),
            _slot: slot,
        });
        let late = {
            let mut sessions = self.sessions.lock().expect("ACP session lock");
            if self.shutting_down.load(Ordering::Acquire) || sessions.contains_key(&id) {
                true
            } else {
                sessions.insert(id.clone(), session.clone());
                false
            }
        };
        if late {
            session.managed.release().await.map_err(access_error)?;
            return Err(invalid(
                "ACP connection is closing or session is already attached",
            ));
        }
        response["sessionId"] = json!(id);
        Ok(response)
    }
}

fn session_mcp_servers(
    servers: &[agent_client_protocol::schema::v1::McpServer],
    cwd: &std::path::Path,
) -> Result<Vec<SessionMcpServer>, Error> {
    let mut result = Vec::new();
    for server in servers {
        let agent_client_protocol::schema::v1::McpServer::Stdio(server) = server else {
            return Err(invalid("only stdio MCP servers are supported"));
        };
        let mut env = std::collections::BTreeMap::new();
        for variable in &server.env {
            if env
                .insert(variable.name.clone(), variable.value.clone())
                .is_some()
            {
                return Err(invalid("duplicate MCP environment name"));
            }
        }
        result.push(SessionMcpServer {
            name: server.name.clone(),
            program: server.command.clone(),
            args: server.args.clone(),
            env,
            cwd: cwd.to_path_buf(),
        });
    }
    validate_session_mcp_servers(&result).map_err(invalid)?;
    Ok(result)
}

struct Projection {
    session_id: String,
    receipt: Receipt,
    sequence: u64,
    emitted: String,
    committed_text: String,
    output_bytes: usize,
    thought_bytes: usize,
    tools: HashMap<String, bool>,
    terminal: Option<Result<bool, String>>,
    output_budget: transport::OutputBudget,
}

async fn prompt(
    session: Arc<Session>,
    session_id: String,
    input: UserInput,
    cancel: CancellationToken,
    connection: ConnectionTo<Client>,
) -> Result<Value, Error> {
    let result = tokio::time::timeout(
        TURN_TIMEOUT,
        prompt_inner(&session, session_id, input, &cancel, &connection),
    )
    .await;
    match result {
        Ok(Ok(response)) => Ok(response),
        other => {
            // Uncertain submission is never retried. A settlement is a separate scoped control.
            let cleanup = session.managed.settle(true).await.map_err(access_error);
            cleanup?;
            match other {
                Ok(Err(error)) => Err(error),
                Err(_) => Err(Error::new(-32603, "ACP prompt timed out and was cancelled")),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

async fn prompt_inner(
    session: &Session,
    session_id: String,
    input: UserInput,
    cancel: &CancellationToken,
    connection: &ConnectionTo<Client>,
) -> Result<Value, Error> {
    let commands = configuration::available_commands(&session.managed).await?;
    {
        let mut published = session.commands.lock().expect("ACP commands");
        if *published != commands {
            session.output_budget.send(
                connection,
                &session_id,
                json!({"sessionUpdate":"available_commands_update","availableCommands":commands}),
            )?;
            *published = commands.clone();
        }
    }
    let input = configuration::route_skill(input, &commands)?;
    // Observe-before-submit closes the fast-model race. The SDK still dispatches cancel/replies.
    let mut observation = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(json!({"stopReason":"cancelled"})),
        observation = session.managed.observe() => observation.map_err(access_error)?,
    };
    if cancel.is_cancelled() {
        return Ok(json!({"stopReason":"cancelled"}));
    }
    // The observation cursor orders connection updates, not this Chat's events. They diverge
    // after permissions/configuration changes and must never be used for event deduplication.
    let sequence = match &observation.initial {
        tickloop_agent_protocol::ObservationValue::Agent(snapshot)
            if snapshot.execution == session.managed.execution() =>
        {
            // A title generated after the previous turn ended arrives here.
            session.publish_status(connection, &session_id, snapshot)?;
            snapshot.last_sequence
        }
        _ => return Err(invalid("invalid managed observation snapshot")),
    };
    let receipt = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            session.managed.settle(true).await.map_err(access_error)?;
            return Ok(json!({"stopReason":"cancelled"}));
        }
        receipt = session.managed.submit(input) => receipt.map_err(access_error)?,
    };
    let mut projection = Projection {
        session_id,
        receipt,
        sequence,
        emitted: String::new(),
        committed_text: String::new(),
        output_bytes: 0,
        thought_bytes: 0,
        tools: HashMap::new(),
        terminal: None,
        output_budget: session.output_budget.clone(),
    };
    let mut recovery = tokio::time::interval(Duration::from_secs(5));
    recovery.reset();
    loop {
        if cancel.is_cancelled() {
            let snapshot = session.managed.settle(true).await.map_err(access_error)?;
            projection
                .replay(&snapshot, session, cancel, connection)
                .await?;
            projection.cancel_tools(connection)?;
            session.publish_status(connection, &projection.session_id, &snapshot)?;
            return Ok(json!({"stopReason":"cancelled"}));
        }
        if projection.terminal.is_some() {
            let snapshot = tokio::select! {
                biased;
                () = cancel.cancelled() => continue,
                snapshot = session.managed.settle(false) => snapshot.map_err(access_error)?,
            };
            projection
                .replay(&snapshot, session, cancel, connection)
                .await?;
            if cancel.is_cancelled() {
                continue;
            }
            projection.validate_settled(&snapshot)?;
            projection.final_text(&snapshot, connection)?;
            if matches!(projection.terminal, Some(Ok(true))) {
                projection.cancel_tools(connection)?;
            }
            session.publish_status(connection, &projection.session_id, &snapshot)?;
            return match projection.terminal.take().expect("terminal state") {
                Ok(true) => Ok(json!({"stopReason":"cancelled"})),
                Ok(false) => Ok(json!({"stopReason":"end_turn"})),
                Err(message) => Err(run_error(message)),
            };
        }
        let update = tokio::select! {
            biased;
            () = cancel.cancelled() => continue,
            update = observation.recv() => update.map_err(access_error)?,
            _ = recovery.tick() => {
                let snapshot = tokio::select! {
                    biased;
                    () = cancel.cancelled() => continue,
                    snapshot = session.managed.snapshot() => snapshot.map_err(access_error)?,
                };
                projection.replay(&snapshot, session, cancel, connection).await?;
                continue;
            }
        };
        match update.change {
            ObservationChange::Event(event) => {
                projection.event(event, session, cancel, connection).await?
            }
            ObservationChange::Reset { snapshot, .. } => {
                if snapshot.execution != session.managed.execution() {
                    return Err(invalid("execution changed during prompt"));
                }
                projection
                    .replay(&snapshot, session, cancel, connection)
                    .await?;
            }
            ObservationChange::Resync { .. } => {
                let snapshot = session.managed.snapshot().await.map_err(access_error)?;
                projection
                    .replay(&snapshot, session, cancel, connection)
                    .await?;
            }
            ObservationChange::Changed => {
                let snapshot = session.managed.snapshot().await.map_err(access_error)?;
                projection
                    .replay(&snapshot, session, cancel, connection)
                    .await?;
            }
            _ => {}
        }
    }
}

impl Projection {
    fn notify(&mut self, connection: &ConnectionTo<Client>, update: Value) -> Result<(), Error> {
        let value = json!({"sessionId":self.session_id, "update":update});
        self.output_bytes = self.output_bytes.saturating_add(value.to_string().len());
        if self.output_bytes > MAX_TURN_OUTPUT {
            return Err(invalid("ACP turn output exceeds 2 MiB"));
        }
        self.output_budget
            .send(connection, &self.session_id, update)
    }

    // Permissions and policy refusals precede ToolStarted. Every update/request must
    // refer to a call already announced to the editor, including tools never executed.
    fn announce_tool(
        &mut self,
        connection: &ConnectionTo<Client>,
        call_id: &str,
        name: &str,
        input: Value,
        status: &str,
    ) -> Result<bool, Error> {
        if self.tools.contains_key(call_id) {
            return Ok(false);
        }
        self.notify(
            connection,
            json!({
                "sessionUpdate":"tool_call", "toolCallId":call_id, "title":name,
                "kind":tool_kind(name), "locations":tool_locations(name, &input), "status":status, "rawInput":bounded_value(input),
            }),
        )?;
        self.tools.insert(call_id.to_owned(), false);
        Ok(true)
    }

    fn cancel_tools(&mut self, connection: &ConnectionTo<Client>) -> Result<(), Error> {
        let mut active: Vec<_> = self
            .tools
            .iter()
            .filter(|(_, done)| !**done)
            .map(|(id, _)| id.clone())
            .collect();
        active.sort();
        for call_id in active {
            self.notify(connection, json!({
                "sessionUpdate":"tool_call_update", "toolCallId":call_id, "status":"failed",
                "content":[{"type":"content", "content":{"type":"text", "text":"Tool cancelled"}}],
            }))?;
            self.tools.insert(call_id, true);
        }
        Ok(())
    }

    async fn replay(
        &mut self,
        snapshot: &LiveChatSnapshot,
        session: &Session,
        cancel: &CancellationToken,
        connection: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        if snapshot.execution != session.managed.execution() {
            return Err(invalid("managed execution identity changed"));
        }
        for event in &snapshot.events {
            self.event(event.clone(), session, cancel, connection)
                .await?;
        }
        if snapshot.last_sequence > self.sequence {
            return Err(invalid(
                "ACP event replay window expired; terminal outcome is not inferred",
            ));
        }
        if self.terminal.is_none()
            && snapshot.conversation.loops.iter().any(|goal| {
                goal.id == self.receipt.loop_id
                    && matches!(
                        goal.state,
                        tickloop_agent_protocol::LoopState::Completed
                            | tickloop_agent_protocol::LoopState::Failed
                            | tickloop_agent_protocol::LoopState::Interrupted
                            | tickloop_agent_protocol::LoopState::Stopped
                            | tickloop_agent_protocol::LoopState::Expired
                    )
            })
        {
            return Err(invalid(
                "ACP terminal event is unavailable; success was not inferred",
            ));
        }
        Ok(())
    }

    fn validate_settled(&self, snapshot: &LiveChatSnapshot) -> Result<(), Error> {
        use tickloop_agent_protocol::{LoopState, OperationState, UserShellState};
        if snapshot.operations.iter().any(|operation| {
            operation.run_id == self.receipt.run_id
                && !matches!(
                    operation.state,
                    OperationState::Completed | OperationState::Failed
                )
        }) {
            return Err(invalid(
                "a tool operation has an unresolved or unknown outcome",
            ));
        }
        if !snapshot.permissions.is_empty()
            || !snapshot.questions.is_empty()
            || !snapshot.pending_inputs.is_empty()
            || !snapshot.pending_commands.is_empty()
            || snapshot.conversation.loops.iter().any(|goal| {
                matches!(
                    goal.state,
                    LoopState::Running
                        | LoopState::Queued
                        | LoopState::WaitingPermission
                        | LoopState::WaitingQuestion
                        | LoopState::WaitingPlan
                        | LoopState::WaitingTrigger
                ) || goal.trigger.as_ref().is_some_and(|trigger| trigger.armed)
                    || !goal.event_sources.is_empty()
            })
            || snapshot.user_shells.iter().any(|shell| {
                !matches!(
                    shell.state,
                    UserShellState::Completed | UserShellState::Failed
                )
            })
        {
            return Err(invalid(
                "ACP prompt did not settle; background work or human waiting remains",
            ));
        }
        Ok(())
    }

    async fn event(
        &mut self,
        event: Event,
        session: &Session,
        cancel: &CancellationToken,
        connection: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        if event.chat_id != self.receipt.chat_id
            || event.run_id != self.receipt.run_id
            || event.loop_id != self.receipt.loop_id
            || event.sequence <= self.sequence
        {
            return Ok(());
        }
        if event.sequence != self.sequence + 1 {
            return Err(invalid(
                "ACP event replay window expired; output was not silently skipped",
            ));
        }
        self.sequence = event.sequence;
        match event.kind {
            EventKind::AssistantMessageCommitted { tool_calls, .. } => {
                self.committed_text = std::mem::take(&mut self.emitted);
                for call in tool_calls {
                    self.announce_tool(
                        connection,
                        &call.call_id,
                        &call.name,
                        call.input,
                        "pending",
                    )?;
                }
            }
            EventKind::Text(text) => {
                if self.emitted.len().saturating_add(text.len()) > MAX_TURN_OUTPUT {
                    return Err(invalid("ACP text exceeds limit"));
                }
                self.emitted.push_str(&text);
                self.notify(connection, json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":text}}))?;
            }
            // Reasoning is supplementary: past its own bound it ends with a marker instead of
            // failing the answer, so it is not counted against the turn output limit.
            EventKind::Thinking(text) if self.thought_bytes <= MAX_TURN_THOUGHT => {
                self.thought_bytes = self.thought_bytes.saturating_add(text.len());
                let text = if self.thought_bytes > MAX_TURN_THOUGHT {
                    "\n[Further thinking omitted]".to_owned()
                } else {
                    text
                };
                self.output_budget.send(
                    connection,
                    &self.session_id,
                    json!({"sessionUpdate":"agent_thought_chunk", "content":{"type":"text", "text":text}}),
                )?;
            }
            EventKind::ToolStarted {
                call_id,
                name,
                input,
            } => {
                let announced =
                    self.announce_tool(connection, &call_id, &name, input.clone(), "in_progress")?;
                if !announced {
                    self.notify(
                        connection,
                        json!({
                            "sessionUpdate":"tool_call_update", "toolCallId":call_id,
                            "title":name, "status":"in_progress", "rawInput":bounded_value(input),
                        }),
                    )?;
                }
            }
            EventKind::ToolCompleted { call_id, result } => {
                self.announce_tool(connection, &call_id, "Tool", Value::Null, "pending")?;
                let mut content = vec![
                    json!({"type":"content", "content":{"type":"text", "text":bounded_text(&result.content)}}),
                ];
                let review = session
                    .managed
                    .access()
                    .observe(tickloop_agent_protocol::ObservationRequest::Agent {
                        execution: session.managed.execution(),
                        detail: tickloop_agent_protocol::AgentDetailQuery::OperationReview {
                            run_id: self.receipt.run_id,
                            call_id: call_id.clone(),
                        },
                        after: None,
                    })
                    .await;
                match review {
                    Ok(observation) => match observation.initial {
                        tickloop_agent_protocol::ObservationValue::OperationReview(Some(review)) => {
                            content.push(json!({"type":"content","content":{"type":"text","text":"Recorded unified-diff hunks (not a complete before/after snapshot):"}}));
                            for text in history::text_chunks(&review.unified_diff) {
                                content.push(json!({"type":"content","content":{"type":"text","text":text}}));
                            }
                        }
                        tickloop_agent_protocol::ObservationValue::OperationReview(None) => {},
                        _ => return Err(invalid("invalid recorded review response")),
                    },
                    Err(_) => content.push(json!({"type":"content","content":{"type":"text","text":"Recorded review unavailable; no diff was inferred from the current file."}})),
                }
                self.notify(
                    connection,
                    json!({
                        "sessionUpdate":"tool_call_update", "toolCallId":call_id,
                        "status":if result.is_error {"failed"} else {"completed"},
                        "content":content,
                    }),
                )?;
                self.tools.insert(call_id, true);
            }
            EventKind::ProgressUpdated { progress } => {
                let entries: Vec<_> = progress.todos.into_iter().take(128).map(|todo| json!({
                    "content":bounded_text(&todo.content), "priority":"medium", "status":todo.status,
                })).collect();
                self.notify(
                    connection,
                    json!({"sessionUpdate":"plan", "entries":entries}),
                )?;
            }
            EventKind::PermissionRequested { request } if !cancel.is_cancelled() => {
                if request.loop_id != self.receipt.loop_id
                    || request.execution.run_id() != Some(self.receipt.run_id)
                {
                    return Err(invalid("permission belongs to another prompt"));
                }
                self.announce_tool(
                    connection,
                    &request.execution.call_key(),
                    &request.name,
                    request.input.clone(),
                    "pending",
                )?;
                permission(session, &self.session_id, request, cancel, connection).await?;
            }
            EventKind::QuestionAsked { .. } => {
                return Err(invalid("unexpected unsupported human question"));
            }
            EventKind::Completed { .. } => self.terminal = Some(Ok(false)),
            EventKind::Interrupted => self.terminal = Some(Ok(true)),
            EventKind::Failed(message) => self.terminal = Some(Err(message)),
            _ => {}
        }
        Ok(())
    }

    fn final_text(
        &mut self,
        snapshot: &LiveChatSnapshot,
        connection: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let text = snapshot
            .conversation
            .run_response_text(self.receipt.run_id)
            .unwrap_or_default();
        // Recover a dropped tail without replaying chunks the editor has already displayed.
        let displayed = if self.emitted.is_empty() {
            &self.committed_text
        } else {
            &self.emitted
        };
        if let Some(tail) = text.strip_prefix(displayed) {
            if !tail.is_empty() {
                self.notify(connection, json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":tail}}))?;
            }
        } else if !text.is_empty() {
            return Err(invalid(
                "stream reset cannot be reconciled without duplicating output",
            ));
        }
        Ok(())
    }
}

async fn permission(
    session: &Session,
    session_id: &str,
    request: PermissionRequest,
    cancel: &CancellationToken,
    connection: &ConnectionTo<Client>,
) -> Result<(), Error> {
    let snapshot = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(()),
        snapshot = session.managed.snapshot() => snapshot.map_err(access_error)?,
    };
    let Some(reference) = snapshot
        .request_refs
        .iter()
        .find(|reference| {
            reference.request_id == request.id
                && reference.execution == session.managed.execution()
                && reference.kind == tickloop_agent_protocol::PendingRequestKind::Permission
        })
        .copied()
    else {
        return Ok(()); // A replayed permission already resolved or cancelled.
    };
    if !snapshot
        .permissions
        .iter()
        .any(|pending| pending.id == request.id && pending.execution == request.execution)
    {
        return Err(invalid("permission identity changed"));
    }
    if request.input.to_string().len() > MAX_PROMPT {
        return Err(invalid(
            "permission input exceeds limit; truncated operations are not offered for approval",
        ));
    }
    let mut approval_input = request.input.clone();
    if matches!(
        request.execution,
        tickloop_agent_protocol::PermissionExecution::HostOperation { .. }
    ) {
        if request.name != "MCPServerStart" {
            return Err(invalid("unknown host operation"));
        }
        let server = session
            .mcp_servers
            .iter()
            .find(|server| request.input["server"].as_str() == Some(&server.name))
            .ok_or_else(|| invalid("MCP launch does not belong to this client session"))?;
        // Only this client supplied argv. Keep it out of events/history and never send env values.
        approval_input["arguments"] = json!(server.args);
        approval_input["notice"] = json!(
            "Starts an unsandboxed process. Clients may record approval interactions; put secrets in env, not argv."
        );
    }
    let value = json!({"sessionId":session_id, "toolCall":{
        "toolCallId":request.execution.call_key(), "title":request.name, "kind":tool_kind(&request.name), "locations":tool_locations(&request.name, &request.input), "status":"pending", "rawInput":approval_input,
    }, "options":[
        {"optionId":"allow_once", "name":"Allow once", "kind":"allow_once"},
        {"optionId":"reject_once", "name":"Reject once", "kind":"reject_once"}
    ]});
    let response = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(()),
        response = connection.send_request(decode::<RequestPermissionRequest>(value)?).block_task() => response?,
    };
    let response = serde_json::to_value(response).map_err(Error::into_internal_error)?;
    let allow = match response["outcome"]["outcome"].as_str() {
        Some("selected") => match response["outcome"]["optionId"].as_str() {
            Some("allow_once") => true,
            Some("reject_once") => false,
            _ => return Err(invalid("permission option was not offered")),
        },
        Some("cancelled") => false,
        _ => return Err(invalid("invalid permission outcome")),
    };
    if !cancel.is_cancelled() {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {},
            decision = session.managed.decide_permission(reference, allow) => decision.map_err(access_error)?,
        }
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(Error::into_internal_error)
}
fn invalid(message: impl Into<String>) -> Error {
    Error::new(-32602, message)
}
fn access_error(error: AccessError) -> Error {
    use tickloop_agent_protocol::AccessErrorCode;
    let code = match error.code {
        AccessErrorCode::Invalid | AccessErrorCode::UntrustedWorkingDirectory => -32602,
        AccessErrorCode::Unsupported => -32601,
        _ => -32603,
    };
    // Control/lease credentials are not provider authentication. Keep the typed failure visible.
    Error::new(code, format!("{:?}: {}", error.code, error.message))
}
fn run_error(message: String) -> Error {
    let lower = message.to_lowercase();
    if lower.contains("model authentication failed")
        || lower.contains("api key missing")
        || lower.contains("subscription is not logged in")
        || (lower.contains("/provider")
            && (lower.contains("not signed in") || lower.contains("renewed authorization")))
    {
        Error::new(
            -32000,
            "Provider authentication required; configure/sign in with /provider in the Tickloop Console",
        )
    } else {
        Error::new(-32603, message)
    }
}
fn tool_kind(name: &str) -> &'static str {
    match name {
        "Read" => "read",
        "Write" | "Edit" | "NotebookEdit" => "edit",
        "Glob" | "Grep" => "search",
        "Bash" | "TaskStop" => "execute",
        "WebFetch" | "WebSearch" => "fetch",
        _ => "other",
    }
}
fn tool_locations(name: &str, input: &Value) -> Vec<Value> {
    let keys: &[&str] = match name {
        "Read" | "Write" | "Edit" => &["file_path"],
        "Glob" | "Grep" => &["path"],
        "Bash" => &["cwd"],
        _ => &[],
    };
    keys.iter()
        .filter_map(|key| input[*key].as_str())
        .filter(|path| std::path::Path::new(path).is_absolute())
        .map(|path| json!({"path":path}))
        .collect()
}

fn bounded_text(text: &str) -> String {
    if text.len() <= MAX_TOOL_CONTENT {
        return text.to_owned();
    }
    let mut end = MAX_TOOL_CONTENT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &text[..end])
}
fn bounded_value(value: Value) -> Value {
    let serialized = value.to_string();
    if serialized.len() <= MAX_TOOL_CONTENT {
        value
    } else {
        json!({"truncated":true, "preview":bounded_text(&serialized)})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_to_auto_and_full_access_is_explicit() {
        let parse = |arguments: &[&str]| {
            Options::parse(
                arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
            )
        };
        assert_eq!(parse(&[]).unwrap().mode, ExecutionMode::Auto);
        assert_eq!(
            parse(&["--mode", "fullAccess"]).unwrap().mode,
            ExecutionMode::FullAccess
        );
        assert_eq!(
            parse(&["--mode", "ask"]).unwrap().mode,
            ExecutionMode::AcceptEdits
        );
        for invalid in [&["--mode", "plan"][..], &["--mode"]] {
            assert!(parse(invalid).is_err());
        }
    }
}
