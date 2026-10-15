use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use tick_protocol::{InstanceRef, TickDescriptor, TickProtocol, TickState};

use crate::hosted::HostedTerminal;

/// Where Tick Sessions put their sockets, and beside each its `<run>.log`, when the launch names
/// no directory.
pub const RUNTIME_DIR_ENV: &str = "TICKLOOP_TICK_RUNTIME_DIR";
const MAX_CONNECTIONS: usize = 8;
const MAX_IN_FLIGHT: usize = 8;
const HOST_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const RESPONSE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
/// A frame between a Session and its host: one Tick response, read or update within its own
/// limit, plus the frame's envelope.
const SESSION_FRAME_BYTES: usize = tick_protocol::MAX_TOOL_RESPONSE_BYTES + 64 * 1024;
/// How many unknown notification methods a session logs; later ones are only counted.
const MAX_LOGGED_NOTIFICATION_METHODS: usize = 16;
/// How much of an unknown notification method a log line names.
const MAX_LOGGED_METHOD_CHARS: usize = 128;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("{0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Tick session is gone")]
    Gone,
    #[error("Tick session is busy")]
    Busy,
    #[error("Tick call was cancelled before dispatch")]
    CancelledBeforeSend,
    #[error("Tick call outcome is unknown after dispatch")]
    OutcomeUnknown,
    /// The Tick answered that it did not run the request: it has no such method, or the
    /// arguments or the tool are not its own.
    #[error("the Tick refused the request: {0}")]
    Refused(String),
    /// The Tick ran the call and answered that it failed: the outcome is known, and a corrected
    /// call may be made again.
    #[error("the Tick Tool failed: {0}")]
    ToolFailed(String),
    /// The Tick refused the call before running it: its catalog is no longer the one the call
    /// was prepared and approved against.
    #[error(
        "the Tick Tool catalog changed since the call was prepared; read the Tick and prepare the call again"
    )]
    RevisionMismatch,
}

/// A Tick its launcher runs on `terminal`, the Tickhost terminal it proved it runs on. Every
/// hello and request names that terminal, so a Session is never mistaken for one on another host
/// or terminal.
#[derive(Clone, Debug)]
pub struct SessionLaunch {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub terminal: HostedTerminal,
    pub runtime_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum SessionLinkState {
    Live,
    Gone,
    Disconnected,
    ProtocolError(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionObservation {
    pub update: Option<TickState>,
    pub received: u64,
    pub state: SessionLinkState,
}

/// When the session last received a Tick Update, without the update itself: what a Monitor
/// shows of an instance nobody subscribes to.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStatus {
    /// Unix milliseconds of the latest Tick Update, by the session's clock.
    pub updated_at_ms: Option<u64>,
    /// How many Tick Updates the session has received.
    pub received: u64,
    /// How many notifications the session ignored because it does not know their method.
    pub ignored_notifications: u64,
}

/// The method that answers a [`SessionStatus`].
const STATUS_METHOD: &str = "session/status";

struct Content {
    protocol: TickProtocol,
    read: TickState,
    observation: SessionObservation,
    updated_at_ms: Option<u64>,
    ignored_notifications: u64,
    logged_notification_methods: HashSet<String>,
}

struct PendingCall {
    reply: oneshot::Sender<Result<Value, SessionError>>,
    method: &'static str,
    /// The [`TickProtocol::read_floor`] of when the request was sent.
    read_floor: Option<u64>,
    _permit: OwnedSemaphorePermit,
}

struct Core {
    instance: InstanceRef,
    terminal: HostedTerminal,
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, PendingCall>>,
    next_id: AtomicU64,
    in_flight: Arc<Semaphore>,
    response_budget: Arc<Semaphore>,
    content: Mutex<Content>,
    observations: watch::Sender<SessionObservation>,
    shutdown: CancellationToken,
    log: PathBuf,
}

pub struct TickSession {
    instance: InstanceRef,
    process_id: Option<u32>,
    socket: PathBuf,
    _files: SessionFiles,
    core: Arc<Core>,
    child: Mutex<Child>,
    reader: JoinHandle<()>,
    listener: JoinHandle<()>,
}

/// The socket and the log a session keeps in its runtime directory, removed with the session.
struct SessionFiles {
    socket: PathBuf,
    socket_identity: (u64, u64),
    log: PathBuf,
}

impl Drop for SessionFiles {
    fn drop(&mut self) {
        remove_owned_socket(&self.socket, self.socket_identity);
        let _ = std::fs::remove_file(&self.log);
    }
}

#[derive(Clone, Debug)]
pub struct SessionClient {
    instance: InstanceRef,
    socket: PathBuf,
    terminal: HostedTerminal,
}

impl TickSession {
    pub async fn spawn(launch: SessionLaunch) -> Result<Self, SessionError> {
        let command = launch
            .command
            .first()
            .ok_or_else(|| SessionError::Protocol("Tick command is empty".into()))?;
        let run = uuid::Uuid::new_v4();
        let instance = InstanceRef::hosted(run);
        let identity = instance.to_string();
        let runtime_dir = resolve_runtime_dir(launch.runtime_dir.as_deref())?;
        std::fs::create_dir_all(&runtime_dir)?;
        private_directory(&runtime_dir)?;
        let socket = runtime_dir.join(format!("{run}.sock"));
        if socket.as_os_str().len() >= 104 {
            return Err(SessionError::Protocol(
                "Tick session socket path is too long".into(),
            ));
        }
        let listener = UnixListener::bind(&socket)?;
        let files = SessionFiles {
            socket: socket.clone(),
            socket_identity: file_identity(&socket)?,
            log: runtime_dir.join(format!("{run}.log")),
        };
        private_socket(&socket)?;
        let mut child = Command::new(command);
        child
            .args(&launch.command[1..])
            .current_dir(&launch.cwd)
            .env(tick_protocol::INSTANCE_ID_ENV, &identity)
            // The launch token belongs to the launcher alone; the business Tick never sees it.
            .env_remove(crate::hosted::LAUNCH_ENV)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        let mut child = crate::hosted::spawn_child(|| child.spawn())?;
        let process_id = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SessionError::Protocol("Tick stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SessionError::Protocol("Tick stdout unavailable".into()))?;
        let observation = SessionObservation {
            update: None,
            received: 0,
            state: SessionLinkState::Live,
        };
        let (observations, _) = watch::channel(observation.clone());
        let core = Arc::new(Core {
            instance: instance.clone(),
            terminal: launch.terminal,
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            response_budget: Arc::new(Semaphore::new(RESPONSE_BUDGET_BYTES)),
            content: Mutex::new(Content {
                protocol: TickProtocol::default(),
                read: TickState::empty(),
                observation,
                updated_at_ms: None,
                ignored_notifications: 0,
                logged_notification_methods: HashSet::new(),
            }),
            observations,
            shutdown: CancellationToken::new(),
            log: files.log.clone(),
        });
        let reader = tokio::spawn(read_child(stdout, Arc::clone(&core)));
        let server = Arc::clone(&core);
        let listener = tokio::spawn(async move { serve(listener, server).await });
        let session = Self {
            instance,
            process_id,
            socket,
            _files: files,
            core,
            child: Mutex::new(child),
            reader,
            listener,
        };
        let descriptor = session
            .core
            .request(
                tick_protocol::DESCRIBE_METHOD,
                json!({}),
                HOST_REQUEST_TIMEOUT,
            )
            .await?;
        TickDescriptor::parse(descriptor).map_err(SessionError::Protocol)?;
        session.core.read().await?;
        Ok(session)
    }

    pub fn instance(&self) -> &InstanceRef {
        &self.instance
    }
    pub fn client(&self) -> SessionClient {
        SessionClient {
            instance: self.instance.clone(),
            socket: self.socket.clone(),
            terminal: self.core.terminal.clone(),
        }
    }
    /// The business process the Session spawned, while the operating system reported one.
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }
    pub fn endpoint(&self) -> &Path {
        &self.socket
    }
    pub async fn wait(mut self) -> Result<ExitStatus, SessionError> {
        let mut child = self.child.lock().await;
        let result = tokio::select! {
            result = child.wait() => result?,
            _ = self.core.shutdown.cancelled() => {
                child.start_kill()?;
                child.wait().await?
            }
        };
        drop(child);
        if tokio::time::timeout(HOST_REQUEST_TIMEOUT, &mut self.reader)
            .await
            .is_err()
        {
            self.reader.abort();
            link_state(&self.core, SessionLinkState::Gone).await;
        }
        self.listener.abort();
        self.core.shutdown.cancel();
        Ok(result)
    }
    pub async fn shutdown(mut self) -> Result<(), SessionError> {
        {
            let mut child = self.child.lock().await;
            if child.try_wait()?.is_none() {
                child.kill().await?;
            }
        }
        if tokio::time::timeout(HOST_REQUEST_TIMEOUT, &mut self.reader)
            .await
            .is_err()
        {
            self.reader.abort();
            link_state(&self.core, SessionLinkState::Gone).await;
        }
        self.listener.abort();
        self.core.shutdown.cancel();
        Ok(())
    }
}

impl Drop for TickSession {
    fn drop(&mut self) {
        self.listener.abort();
        self.reader.abort();
        self.core.shutdown.cancel();
    }
}

impl Core {
    async fn request(
        &self,
        method: &'static str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value, SessionError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let permit = Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|_| SessionError::Busy)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({"jsonrpc":tick_protocol::JSON_RPC_VERSION,"id":id,"method":method,"params":params});
        let mut bytes = serde_json::to_vec(&frame)
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > tick_protocol::MAX_REQUEST_BYTES {
            return Err(SessionError::Protocol("Tick request too large".into()));
        }
        let (sender, receiver) = oneshot::channel();
        let read_floor = self.content.lock().await.protocol.read_floor();
        self.pending.lock().await.insert(
            id,
            PendingCall {
                reply: sender,
                method,
                read_floor,
                _permit: permit,
            },
        );
        let mut stdin = match tokio::time::timeout_at(deadline, self.stdin.lock()).await {
            Ok(stdin) => stdin,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(SessionError::CancelledBeforeSend);
            }
        };
        let write = tokio::time::timeout_at(deadline, stdin.write_all(&bytes)).await;
        drop(stdin);
        if write.is_err() || write.is_ok_and(|result| result.is_err()) {
            link_state(
                self,
                SessionLinkState::ProtocolError("Tick stdin write failed or timed out".into()),
            )
            .await;
            self.pending.lock().await.remove(&id);
            return Err(SessionError::OutcomeUnknown);
        }
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SessionError::Gone),
            Err(_) => Err(SessionError::OutcomeUnknown),
        }
    }

    /// The Tick's current state, as [`read_child`] checked it on arrival.
    async fn read(&self) -> Result<Value, SessionError> {
        self.request(tick_protocol::READ_METHOD, json!({}), HOST_REQUEST_TIMEOUT)
            .await
    }

    /// Dispatches a prepared call as it is: the Tick runs it only while its catalog still has
    /// the revision the call names.
    async fn call(&self, params: Value) -> Result<Value, SessionError> {
        let (Some(tool), Some(revision)) = (params["tool"].as_str(), params["revision"].as_str())
        else {
            return Err(SessionError::Protocol(
                "a Tick call names its tool and catalog revision".into(),
            ));
        };
        let args = params.get("args").cloned().unwrap_or(json!({}));
        self.request(
            tick_protocol::CALL_METHOD,
            json!({"tool":tool,"args":args,"revision":revision}),
            TOOL_TIMEOUT,
        )
        .await
        .map_err(|error| match error {
            SessionError::Gone => SessionError::OutcomeUnknown,
            other => other,
        })
    }

    async fn watch(
        &self,
    ) -> (
        TickState,
        SessionObservation,
        watch::Receiver<SessionObservation>,
    ) {
        let content = self.content.lock().await;
        (
            content.read.clone(),
            content.observation.clone(),
            self.observations.subscribe(),
        )
    }
}

async fn read_child(stdout: tokio::process::ChildStdout, core: Arc<Core>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let line =
            match read_line_bounded(&mut reader, tick_protocol::MAX_TOOL_RESPONSE_BYTES).await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    link_state(&core, SessionLinkState::ProtocolError(error.to_string())).await;
                    return;
                }
            };
        let frame: Value = match serde_json::from_slice(&line) {
            Ok(frame) => frame,
            Err(error) => {
                link_state(&core, SessionLinkState::ProtocolError(error.to_string())).await;
                return;
            }
        };
        if frame.get("jsonrpc").and_then(Value::as_str) != Some(tick_protocol::JSON_RPC_VERSION) {
            link_state(
                &core,
                SessionLinkState::ProtocolError("JSON-RPC version mismatch".into()),
            )
            .await;
            return;
        }
        if let Some(id) = frame.get("id").and_then(Value::as_u64) {
            let Some(pending) = core.pending.lock().await.remove(&id) else {
                continue;
            };
            let result = match (frame.get("result"), frame.get("error")) {
                // The Tick has already acted on the request, so an answer too large to forward
                // leaves its outcome unknown rather than undone.
                (Some(result), None) => match tick_protocol::check_encoded_size(
                    result,
                    tick_protocol::MAX_TOOL_RESPONSE_BYTES,
                    "Tick response",
                ) {
                    Err(_) => Err(SessionError::OutcomeUnknown),
                    Ok(()) if pending.method == tick_protocol::READ_METHOD => {
                        read_answer(&core, result.clone(), pending.read_floor).await
                    }
                    Ok(()) => Ok(result.clone()),
                },
                (None, Some(error)) => Err(tick_error(error)),
                // An answer with neither a result nor an error leaves what a call did unknown.
                _ if pending.method == tick_protocol::CALL_METHOD => {
                    Err(SessionError::OutcomeUnknown)
                }
                _ => Err(SessionError::Protocol("invalid Tick response".into())),
            };
            let _ = pending.reply.send(result);
            continue;
        }
        match frame.get("method").and_then(Value::as_str) {
            Some(tick_protocol::UPDATE_METHOD) => {}
            // A notification this host does not know yet is skipped, so a Tick speaking a later
            // addition to the protocol keeps running. It is counted in the session status, and
            // the first one of each method is logged.
            Some(method) => {
                let method: String = method.chars().take(MAX_LOGGED_METHOD_CHARS).collect();
                let is_first_of_its_method = {
                    let mut content = core.content.lock().await;
                    content.ignored_notifications += 1;
                    content.logged_notification_methods.len() < MAX_LOGGED_NOTIFICATION_METHODS
                        && content.logged_notification_methods.insert(method.clone())
                };
                if is_first_of_its_method {
                    let line =
                        format!("ignored unknown notification {method:?}; counting the rest");
                    log(&core, &line).await;
                }
                continue;
            }
            None => {
                link_state(
                    &core,
                    SessionLinkState::ProtocolError("invalid Tick frame".into()),
                )
                .await;
                return;
            }
        }
        if line.len() > tick_protocol::MAX_UPDATE_BYTES {
            link_state(
                &core,
                SessionLinkState::ProtocolError("Tick update too large".into()),
            )
            .await;
            return;
        }
        let mut content = core.content.lock().await;
        let update = match content
            .protocol
            .update(frame.get("params").cloned().unwrap_or(Value::Null))
        {
            Ok(update) => update,
            Err(error) => {
                drop(content);
                link_state(&core, SessionLinkState::ProtocolError(error)).await;
                return;
            }
        };
        if update.seq >= content.read.seq {
            content.read = update.clone();
        }
        content.observation.update = Some(update);
        content.observation.received += 1;
        content.updated_at_ms = Some(now_ms());
        let _ = core.observations.send_replace(content.observation.clone());
    }
    link_state(&core, SessionLinkState::Gone).await;
    for (_, pending) in core.pending.lock().await.drain() {
        let _ = pending.reply.send(Err(SessionError::Gone));
    }
}

/// Appends a line to the session log beside its socket. Nothing goes to stderr: the launcher's
/// stderr is the Tick's visible terminal. A log that cannot be written is skipped.
async fn log(core: &Core, line: &str) {
    let mut options = tokio::fs::OpenOptions::new();
    options
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    if let Ok(mut file) = options.open(&core.log).await {
        let entry = format!(
            "{} {} {line}\n",
            tick_protocol::now_rfc3339(),
            core.instance
        );
        let _ = file.write_all(entry.as_bytes()).await;
    }
}

/// What an explicit JSON-RPC error answer says about the request: a request-level error means
/// the Tick did not run it, any other error that it ran and failed.
fn tick_error(error: &Value) -> SessionError {
    let message = match (error["message"].as_str(), error.get("data")) {
        (Some(message), Some(data)) if !data.is_null() => format!("{message} ({data})"),
        (Some(message), _) => message.to_owned(),
        (None, _) => error.to_string(),
    };
    match error["code"].as_i64() {
        Some(tick_protocol::REVISION_MISMATCH) => SessionError::RevisionMismatch,
        Some(
            tick_protocol::PARSE_ERROR
            | tick_protocol::INVALID_REQUEST
            | tick_protocol::METHOD_NOT_FOUND
            | tick_protocol::INVALID_PARAMS
            | tick_protocol::VERSION_MISMATCH
            | tick_protocol::FRAME_TOO_LARGE,
        ) => SessionError::Refused(message),
        _ => SessionError::ToolFailed(message),
    }
}

/// Checks a `tick/read` answer in the order it arrived among the updates, and keeps it as the
/// session's state unless an update that arrived first is newer.
async fn read_answer(
    core: &Core,
    answer: Value,
    floor: Option<u64>,
) -> Result<Value, SessionError> {
    let mut content = core.content.lock().await;
    let read = content
        .protocol
        .read(answer, floor)
        .map_err(SessionError::Protocol)?;
    if read.seq >= content.read.seq {
        content.read = read.clone();
    }
    serde_json::to_value(read).map_err(|error| SessionError::Protocol(error.to_string()))
}

async fn link_state(core: &Core, state: SessionLinkState) {
    let mut content = core.content.lock().await;
    content.observation.state = state.clone();
    let _ = core.observations.send_replace(content.observation.clone());
    drop(content);
    if state != SessionLinkState::Live {
        for (_, pending) in core.pending.lock().await.drain() {
            let error = match &state {
                SessionLinkState::ProtocolError(_) => SessionError::OutcomeUnknown,
                _ => SessionError::Gone,
            };
            let _ = pending.reply.send(Err(error));
        }
        core.shutdown.cancel();
    }
}

async fn serve(listener: UnixListener, core: Arc<Core>) {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let busy_response = Arc::new(Semaphore::new(1));
    loop {
        let accept = tokio::select! { _ = core.shutdown.cancelled() => return, accept = listener.accept() => accept };
        let Ok((stream, _)) = accept else {
            return;
        };
        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
            if let Ok(permit) = Arc::clone(&busy_response).try_acquire_owned() {
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = reject_busy(stream).await;
                });
            }
            continue;
        };
        let core = Arc::clone(&core);
        tokio::spawn(async move {
            let _permit = permit;
            let _ = serve_connection(stream, core).await;
        });
    }
}

async fn reject_busy(mut stream: UnixStream) -> Result<(), SessionError> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        async {
            let Some(line) = read_line_bounded(&mut reader, tick_protocol::MAX_REQUEST_BYTES).await? else {
                return Ok(());
            };
            let frame = tick_protocol::parse_request(&line).map_err(SessionError::Protocol)?;
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            write_frame(&mut writer, json!({"jsonrpc":tick_protocol::JSON_RPC_VERSION,"id":id,"error":{"code":-32010,"message":"Tick session is busy","data":{"dispatched":false}}})).await
        },
    )
    .await
    .map_err(|_| SessionError::Busy)?
}

async fn serve_connection(mut stream: UnixStream, core: Arc<Core>) -> Result<(), SessionError> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    loop {
        let line = tokio::select! { _ = core.shutdown.cancelled() => return Ok(()), line = tokio::time::timeout(HOST_REQUEST_TIMEOUT, read_line_bounded(&mut reader, tick_protocol::MAX_REQUEST_BYTES)) => line.map_err(|_| SessionError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "Session request timed out")))?? };
        let Some(line) = line else {
            return Ok(());
        };
        let frame = tick_protocol::parse_request(&line).map_err(SessionError::Protocol)?;
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let binding = frame.get("binding").cloned().unwrap_or(Value::Null);
        if binding["instance_id"] != core.instance.to_string()
            || binding["terminal"] != json!(core.terminal)
        {
            write_frame(&mut writer, json!({"jsonrpc":tick_protocol::JSON_RPC_VERSION,"id":id,"error":{"code":-32014,"message":"Tick Session host, terminal, or identity mismatch"}})).await?;
            continue;
        }
        let method = frame["method"].as_str().unwrap_or_default();
        let params = frame.get("params").cloned().unwrap_or(json!({}));
        if method == "session/watch" {
            let (baseline, registration, mut observations) = core.watch().await;
            write_frame(&mut writer, json!({"jsonrpc":tick_protocol::JSON_RPC_VERSION,"id":id,"result":{"read":baseline,"received":registration.received,"state":registration.state}})).await?;
            loop {
                tokio::select! {
                    biased;
                    changed = observations.changed() => if changed.is_err() { return Ok(()); },
                    _ = reader.read_u8() => return Ok(()),
                    _ = core.shutdown.cancelled() => return Ok(()),
                }
                let observation = observations.borrow_and_update().clone();
                write_frame(
                    &mut writer,
                    json!({"jsonrpc":tick_protocol::JSON_RPC_VERSION,"method":"session/observation","params":observation}),
                )
                .await?;
                if observation.state != SessionLinkState::Live {
                    return Ok(());
                }
            }
        }
        let response_reservation = if method == tick_protocol::CALL_METHOD {
            Some(
                Arc::clone(&core.response_budget)
                    .try_acquire_many_owned(tick_protocol::MAX_TOOL_RESPONSE_BYTES as u32),
            )
        } else {
            None
        };
        let result = if response_reservation.as_ref().is_some_and(Result::is_err) {
            Err(SessionError::Busy)
        } else {
            match method {
                STATUS_METHOD => {
                    let content = core.content.lock().await;
                    Ok(json!(SessionStatus {
                        updated_at_ms: content.updated_at_ms,
                        received: content.observation.received,
                        ignored_notifications: content.ignored_notifications,
                    }))
                }
                "session/hello" => Ok(
                    json!({"instance_id":core.instance.to_string(),"protocol":tick_protocol::NATIVE_PROTOCOL_VERSION,"terminal":core.terminal}),
                ),
                tick_protocol::DESCRIBE_METHOD => {
                    core.request(tick_protocol::DESCRIBE_METHOD, params, HOST_REQUEST_TIMEOUT)
                        .await
                }
                tick_protocol::READ_METHOD => core.read().await,
                tick_protocol::CALL_METHOD => core.call(params).await,
                _ => Err(SessionError::Protocol(format!(
                    "unknown Session method {method:?}"
                ))),
            }
        };
        let response = match result {
            Ok(result) => {
                json!({"jsonrpc":tick_protocol::JSON_RPC_VERSION,"id":id,"result":result})
            }
            Err(error) => {
                let (code, dispatched) = match error {
                    SessionError::Busy => (-32010, false),
                    SessionError::CancelledBeforeSend => (-32011, false),
                    SessionError::OutcomeUnknown => (-32012, true),
                    SessionError::Gone => (-32013, true),
                    SessionError::ToolFailed(_) => (-32015, true),
                    SessionError::RevisionMismatch => (-32016, false),
                    SessionError::Refused(_) => (-32017, false),
                    _ => (tick_protocol::TOOL_FAILED, false),
                };
                let message = match error {
                    SessionError::Refused(message) | SessionError::ToolFailed(message) => message,
                    other => other.to_string(),
                };
                json!({"jsonrpc":tick_protocol::JSON_RPC_VERSION,"id":id,"error":{"code":code,"message":message,"data":{"dispatched":dispatched}}})
            }
        };
        write_frame(&mut writer, response).await?;
    }
}

async fn write_frame(
    writer: &mut tokio::net::unix::WriteHalf<'_>,
    frame: Value,
) -> Result<(), SessionError> {
    let mut bytes =
        serde_json::to_vec(&frame).map_err(|error| SessionError::Protocol(error.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() > SESSION_FRAME_BYTES {
        return Err(SessionError::Protocol("Session response too large".into()));
    }
    tokio::time::timeout(HOST_REQUEST_TIMEOUT, writer.write_all(&bytes))
        .await
        .map_err(|_| SessionError::OutcomeUnknown)??;
    Ok(())
}

impl SessionClient {
    pub async fn connect(
        instance: InstanceRef,
        terminal: HostedTerminal,
        runtime_dir: Option<PathBuf>,
    ) -> Result<Self, SessionError> {
        let run = instance.hosted_run().ok_or_else(|| {
            SessionError::Protocol("a Tick Session serves a hosted: instance".into())
        })?;
        let directory = resolve_runtime_dir(runtime_dir.as_deref())?;
        verify_owner(&directory)?;
        let socket = directory.join(format!("{run}.sock"));
        verify_owner(&socket)?;
        let client = Self {
            instance,
            socket,
            terminal,
        };
        let hello = client.request("session/hello", json!({}), None).await?;
        if hello["instance_id"] != client.instance.to_string()
            || hello["protocol"] != tick_protocol::NATIVE_PROTOCOL_VERSION
            || hello["terminal"] != json!(client.terminal)
        {
            return Err(SessionError::Protocol(
                "Tick Session host, terminal, or identity mismatch".into(),
            ));
        }
        Ok(client)
    }

    pub async fn describe(&self) -> Result<TickDescriptor, SessionError> {
        let value = self
            .request(tick_protocol::DESCRIBE_METHOD, json!({}), None)
            .await?;
        TickDescriptor::parse(value).map_err(SessionError::Protocol)
    }
    /// When the Tick last updated, without reading its content.
    pub async fn status(&self) -> Result<SessionStatus, SessionError> {
        let value = self.request(STATUS_METHOD, json!({}), None).await?;
        serde_json::from_value(value).map_err(|error| SessionError::Protocol(error.to_string()))
    }
    pub async fn read(&self) -> Result<TickState, SessionError> {
        let value = self
            .request(tick_protocol::READ_METHOD, json!({}), None)
            .await?;
        TickProtocol::default()
            .read(value, None)
            .map_err(SessionError::Protocol)
    }
    /// Calls `tool` against the catalog `revision` it was prepared with; a Tick whose catalog
    /// changed since refuses it unrun with [`SessionError::RevisionMismatch`].
    pub async fn call(
        &self,
        tool: &str,
        args: Value,
        revision: &str,
        cancellation: CancellationToken,
    ) -> Result<Value, SessionError> {
        if cancellation.is_cancelled() {
            return Err(SessionError::CancelledBeforeSend);
        }
        self.request(
            tick_protocol::CALL_METHOD,
            json!({"tool":tool,"args":args,"revision":revision}),
            Some(cancellation),
        )
        .await
    }
    pub async fn watch(
        &self,
    ) -> Result<(TickState, u64, watch::Receiver<SessionObservation>), SessionError> {
        let (mut reader, registration) = tokio::time::timeout(HOST_REQUEST_TIMEOUT, async {
            let mut stream = UnixStream::connect(&self.socket).await?;
            write_client_request(&mut stream, self, "session/watch", json!({})).await?;
            let mut reader = BufReader::new(stream);
            let registration = response_result(read_client_frame(&mut reader).await?)?;
            Ok::<_, SessionError>((reader, registration))
        })
        .await
        .map_err(|_| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Session watch timed out",
            ))
        })??;
        let baseline = TickProtocol::default()
            .read(registration["read"].clone(), None)
            .map_err(SessionError::Protocol)?;
        let received = registration["received"].as_u64().ok_or_else(|| {
            SessionError::Protocol("Session watch registration has no received count".into())
        })?;
        let state: SessionLinkState = serde_json::from_value(registration["state"].clone())
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let (sender, receiver) = watch::channel(SessionObservation {
            update: None,
            received,
            state,
        });
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! { _ = sender.closed() => return, frame = read_client_frame(&mut reader) => frame };
                let Ok(frame) = frame else {
                    break;
                };
                if frame["method"] != "session/observation" {
                    break;
                }
                let Ok(observation) = serde_json::from_value(frame["params"].clone()) else {
                    break;
                };
                sender.send_replace(observation);
            }
            sender.send_modify(|observation| {
                if observation.state == SessionLinkState::Live {
                    observation.state = SessionLinkState::Disconnected;
                }
            });
        });
        Ok((baseline, received, receiver))
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancellation: Option<CancellationToken>,
    ) -> Result<Value, SessionError> {
        let timeout = if method == tick_protocol::CALL_METHOD {
            TOOL_TIMEOUT
        } else {
            HOST_REQUEST_TIMEOUT
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let connect_deadline =
            std::cmp::min(deadline, tokio::time::Instant::now() + HOST_REQUEST_TIMEOUT);
        let mut stream = tokio::time::timeout_at(connect_deadline, async {
            if let Some(cancel) = &cancellation {
                tokio::select! { _ = cancel.cancelled() => Err(SessionError::CancelledBeforeSend), stream = UnixStream::connect(&self.socket) => stream.map_err(SessionError::Io) }
            } else { UnixStream::connect(&self.socket).await.map_err(SessionError::Io) }
        }).await.map_err(|_| SessionError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "Session connect timed out")))??;
        if let Some(cancel) = &cancellation
            && cancel.is_cancelled()
        {
            return Err(SessionError::CancelledBeforeSend);
        }
        let bytes = client_request_bytes(self, method, params)?;
        if let Some(cancel) = &cancellation {
            tokio::time::timeout_at(deadline, async { tokio::select! {
                _ = cancel.cancelled() => Err(SessionError::OutcomeUnknown),
                result = stream.write_all(&bytes) => result.map_err(|_| SessionError::OutcomeUnknown),
            }}).await.map_err(|_| SessionError::OutcomeUnknown)??;
        } else {
            tokio::time::timeout_at(deadline, stream.write_all(&bytes))
                .await
                .map_err(|_| {
                    SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Session write timed out",
                    ))
                })??;
        }
        let mut reader = BufReader::new(stream);
        let response = tokio::time::timeout_at(deadline, async {
            if let Some(cancel) = &cancellation {
                tokio::select! { _ = cancel.cancelled() => Err(SessionError::OutcomeUnknown), frame = read_client_frame(&mut reader) => frame.map_err(|error| match error { SessionError::Io(_) | SessionError::Gone => SessionError::OutcomeUnknown, other => other }) }
            } else { read_client_frame(&mut reader).await }
        }).await.map_err(|_| if method == tick_protocol::CALL_METHOD { SessionError::OutcomeUnknown } else { SessionError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "Session response timed out")) })??;
        response_result(response)
    }
}

async fn write_client_request(
    stream: &mut UnixStream,
    client: &SessionClient,
    method: &str,
    params: Value,
) -> Result<(), SessionError> {
    let bytes = client_request_bytes(client, method, params)?;
    stream.write_all(&bytes).await?;
    Ok(())
}

fn client_request_bytes(
    client: &SessionClient,
    method: &str,
    params: Value,
) -> Result<Vec<u8>, SessionError> {
    let mut bytes = serde_json::to_vec(
        &json!({"jsonrpc":tick_protocol::JSON_RPC_VERSION,"id":1,"method":method,"params":params,
            "binding":{"instance_id":client.instance.to_string(),"terminal":client.terminal}
        }),
    )
    .map_err(|error| SessionError::Protocol(error.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() > tick_protocol::MAX_REQUEST_BYTES {
        return Err(SessionError::Protocol("Session request too large".into()));
    }
    Ok(bytes)
}

async fn read_client_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value, SessionError> {
    let line = read_line_bounded(reader, SESSION_FRAME_BYTES)
        .await?
        .ok_or(SessionError::Gone)?;
    serde_json::from_slice(&line).map_err(|error| SessionError::Protocol(error.to_string()))
}

async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    maximum: usize,
) -> Result<Option<Vec<u8>>, SessionError> {
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(SessionError::Protocol("unterminated JSON-RPC frame".into()))
            };
        }
        let take = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |index| index + 1);
        if line.len().saturating_add(take) > maximum {
            return Err(SessionError::Protocol(format!(
                "JSON-RPC frame exceeds {maximum} bytes"
            )));
        }
        let complete = chunk[take - 1] == b'\n';
        line.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if complete {
            line.pop();
            return Ok(Some(line));
        }
    }
}

fn response_result(response: Value) -> Result<Value, SessionError> {
    match (response.get("result"), response.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => {
            let message = error["message"]
                .as_str()
                .unwrap_or("Tick Session error")
                .to_owned();
            match error["code"].as_i64() {
                Some(-32010) => Err(SessionError::Busy),
                Some(-32011) => Err(SessionError::CancelledBeforeSend),
                Some(-32012) => Err(SessionError::OutcomeUnknown),
                Some(-32013) => Err(SessionError::Gone),
                Some(-32015) => Err(SessionError::ToolFailed(message)),
                Some(-32016) => Err(SessionError::RevisionMismatch),
                Some(-32017) => Err(SessionError::Refused(message)),
                _ => Err(SessionError::Protocol(message)),
            }
        }
        _ => Err(SessionError::Protocol("invalid Session response".into())),
    }
}

fn resolve_runtime_dir(configured: Option<&Path>) -> Result<PathBuf, SessionError> {
    let directory = configured
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(RUNTIME_DIR_ENV).map(PathBuf::from))
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/tickloop-{}/ticks", unsafe {
                libc::geteuid()
            }))
        });
    if directory.as_os_str().len() <= 55 {
        return Ok(directory);
    }
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(directory.as_os_str().as_encoded_bytes());
    Ok(PathBuf::from(format!(
        "/tmp/tickloop-{}/d{:02x}{:02x}{:02x}{:02x}",
        unsafe { libc::geteuid() },
        hash[0],
        hash[1],
        hash[2],
        hash[3]
    )))
}

fn private_directory(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::PermissionsExt;
    verify_owner(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn private_socket(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::PermissionsExt;
    verify_owner(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn verify_owner(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(SessionError::Protocol(format!(
            "Tick Session resource {} is not owned by this user",
            path.display()
        )));
    }
    Ok(())
}

/// The current Unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn file_identity(path: &Path) -> Result<(u64, u64), SessionError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

fn remove_owned_socket(path: &Path, expected: (u64, u64)) {
    use std::os::unix::fs::FileTypeExt;
    if std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_socket())
        && file_identity(path).ok() == Some(expected)
    {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers already in flight that leave less than one largest answer of the 64 MiB response
    /// budget make the next call Busy before it reaches the Tick, though connections and
    /// in-flight slots are free.
    #[tokio::test]
    async fn a_call_beyond_the_response_budget_is_busy_before_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let counter = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tick/examples/counter.py");
        let session = TickSession::spawn(SessionLaunch {
            command: vec![
                "python3".into(),
                "-u".into(),
                counter.to_string_lossy().into_owned(),
            ],
            cwd: directory.path().into(),
            terminal: HostedTerminal {
                endpoint: PathBuf::from("/tmp/example-tickhost.sock"),
                epoch: uuid::Uuid::new_v4(),
                terminal: uuid::Uuid::new_v4(),
            },
            runtime_dir: Some(directory.path().join("sessions")),
        })
        .await
        .unwrap();
        let in_flight = RESPONSE_BUDGET_BYTES - tick_protocol::MAX_TOOL_RESPONSE_BYTES + 1;
        let answers = Arc::clone(&session.core.response_budget)
            .try_acquire_many_owned(in_flight as u32)
            .unwrap();
        let refused = session
            .client()
            .call(
                "increment",
                json!({"amount": 1}),
                "1",
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(refused, Err(SessionError::Busy)), "{refused:?}");
        assert_eq!(
            session.client().read().await.unwrap().data,
            json!({"value":0}),
            "the Tick never received the call"
        );
        drop(answers);
        let called = session
            .client()
            .call(
                "increment",
                json!({"amount": 1}),
                "1",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(called["value"], 1);
        session.shutdown().await.unwrap();
    }
}
