use std::{
    collections::HashMap,
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

use crate::{
    instance::InstanceRef,
    protocol::{self, TickDescriptor, TickProtocol, TickRead, TickUpdate},
};

const MAX_CONNECTIONS: usize = 8;
const MAX_IN_FLIGHT: usize = 8;
const HOST_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const RESPONSE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

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
    #[error("Tick rejected the dispatched call: {0}")]
    ChildError(String),
}

#[derive(Clone, Debug)]
pub struct SessionLaunch {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub host_socket: PathBuf,
    pub terminal_id: String,
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
    pub update: Option<TickUpdate>,
    pub received: u64,
    pub state: SessionLinkState,
}

struct Content {
    protocol: TickProtocol,
    read: TickRead,
    observation: SessionObservation,
}

struct PendingCall {
    reply: oneshot::Sender<Result<Value, SessionError>>,
    _permit: OwnedSemaphorePermit,
}

struct Core {
    instance: InstanceRef,
    host_socket: PathBuf,
    terminal_id: String,
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, PendingCall>>,
    next_id: AtomicU64,
    in_flight: Arc<Semaphore>,
    response_budget: Arc<Semaphore>,
    content: Mutex<Content>,
    observations: watch::Sender<SessionObservation>,
    shutdown: CancellationToken,
}

pub struct TickSession {
    instance: InstanceRef,
    socket: PathBuf,
    _socket_lease: SocketLease,
    core: Arc<Core>,
    child: Mutex<Child>,
    reader: JoinHandle<()>,
    listener: JoinHandle<()>,
}

struct SocketLease {
    path: PathBuf,
    identity: (u64, u64),
}

impl Drop for SocketLease {
    fn drop(&mut self) {
        remove_owned_socket(&self.path, self.identity);
    }
}

#[derive(Clone, Debug)]
pub struct SessionClient {
    instance: InstanceRef,
    socket: PathBuf,
    host_socket: PathBuf,
    terminal_id: String,
}

impl TickSession {
    pub async fn spawn(launch: SessionLaunch) -> Result<Self, SessionError> {
        let command = launch
            .command
            .first()
            .ok_or_else(|| SessionError::Protocol("Tick command is empty".into()))?;
        let identity = format!("herdr:{}", uuid::Uuid::new_v4());
        let instance =
            InstanceRef::native(&identity).map_err(|error| SessionError::Protocol(error.into()))?;
        let runtime_dir = resolve_runtime_dir(launch.runtime_dir.as_deref())?;
        std::fs::create_dir_all(&runtime_dir)?;
        private_directory(&runtime_dir)?;
        let socket = runtime_dir.join(format!("{}.sock", &identity[6..]));
        if socket.as_os_str().len() >= 104 {
            return Err(SessionError::Protocol(
                "Tick session socket path is too long".into(),
            ));
        }
        let listener = UnixListener::bind(&socket)?;
        let socket_identity = file_identity(&socket)?;
        let socket_lease = SocketLease {
            path: socket.clone(),
            identity: socket_identity,
        };
        private_socket(&socket)?;
        let mut child = Command::new(command);
        child
            .args(&launch.command[1..])
            .current_dir(&launch.cwd)
            .env(protocol::INSTANCE_ID_ENV, &identity)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        let mut child = child.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SessionError::Protocol("Tick stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SessionError::Protocol("Tick stdout unavailable".into()))?;
        let initial = TickRead {
            version: protocol::NATIVE_UPDATE_VERSION,
            tick_instance_id: identity,
            initialized: false,
            sequence: 0,
            observed_at: String::new(),
            data: None,
        };
        let observation = SessionObservation {
            update: None,
            received: 0,
            state: SessionLinkState::Live,
        };
        let (observations, _) = watch::channel(observation.clone());
        let core = Arc::new(Core {
            instance: instance.clone(),
            host_socket: launch.host_socket,
            terminal_id: launch.terminal_id,
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            response_budget: Arc::new(Semaphore::new(RESPONSE_BUDGET_BYTES)),
            content: Mutex::new(Content {
                protocol: TickProtocol::new(instance.to_string()),
                read: initial,
                observation,
            }),
            observations,
            shutdown: CancellationToken::new(),
        });
        let reader = tokio::spawn(read_child(stdout, Arc::clone(&core)));
        let server = Arc::clone(&core);
        let listener = tokio::spawn(async move { serve(listener, server).await });
        let session = Self {
            instance,
            socket,
            _socket_lease: socket_lease,
            core,
            child: Mutex::new(child),
            reader,
            listener,
        };
        let descriptor = session
            .core
            .request("tick/describe", json!({}), HOST_REQUEST_TIMEOUT)
            .await?;
        session
            .core
            .content
            .lock()
            .await
            .protocol
            .descriptor(descriptor)
            .map_err(SessionError::Protocol)?;
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
            host_socket: self.core.host_socket.clone(),
            terminal_id: self.core.terminal_id.clone(),
        }
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
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value, SessionError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let permit = Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|_| SessionError::Busy)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let mut bytes = serde_json::to_vec(&frame)
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > protocol::MAX_REQUEST_BYTES {
            return Err(SessionError::Protocol("Tick request too large".into()));
        }
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(
            id,
            PendingCall {
                reply: sender,
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

    async fn read(&self) -> Result<TickRead, SessionError> {
        let result = self
            .request(protocol::READ_METHOD, json!({}), HOST_REQUEST_TIMEOUT)
            .await?;
        let mut content = self.content.lock().await;
        let read = content
            .protocol
            .read(result)
            .map_err(SessionError::Protocol)?;
        content.read = read.clone();
        Ok(read)
    }

    async fn call(&self, params: Value) -> Result<Value, SessionError> {
        let deadline = tokio::time::Instant::now() + TOOL_TIMEOUT;
        let tool_name = params["tool"]
            .as_str()
            .ok_or_else(|| SessionError::Protocol("Tick Tool name missing".into()))?;
        let args = params.get("args").cloned().unwrap_or(json!({}));
        let descriptor_result = self
            .request("tick/describe", json!({}), HOST_REQUEST_TIMEOUT)
            .await?;
        let child_descriptor_digest = protocol::descriptor_digest_value(&descriptor_result);
        let descriptor = self
            .content
            .lock()
            .await
            .protocol
            .descriptor(descriptor_result)
            .map_err(SessionError::Protocol)?;
        if let Some(expected) = params
            .get("expected_descriptor_digest")
            .and_then(Value::as_str)
            && expected != protocol::descriptor_digest(&descriptor)
        {
            return Err(SessionError::Protocol(
                "Tick Tool catalog changed before dispatch".into(),
            ));
        }
        let tool = descriptor
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .ok_or_else(|| SessionError::Protocol(format!("unknown Tick Tool {tool_name:?}")))?;
        let validator = jsonschema::validator_for(&tool.params).map_err(|error| {
            SessionError::Protocol(format!("invalid Tick Tool schema: {error}"))
        })?;
        if !validator.is_valid(&args) {
            return Err(SessionError::Protocol(
                "Tick Tool arguments do not match its schema".into(),
            ));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(SessionError::OutcomeUnknown);
        }
        self.request("tool/call", json!({"tool":tool_name,"args":args,"expected_descriptor_digest":child_descriptor_digest}), remaining).await.map_err(|error| match error {
            SessionError::Gone => SessionError::OutcomeUnknown,
            other => other,
        })
    }

    async fn watch(
        &self,
    ) -> (
        TickRead,
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
        let line = match read_line_bounded(&mut reader, protocol::MAX_TOOL_RESPONSE_BYTES).await {
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
        if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            link_state(
                &core,
                SessionLinkState::ProtocolError("JSON-RPC version mismatch".into()),
            )
            .await;
            return;
        }
        if let Some(id) = frame.get("id").and_then(Value::as_u64) {
            let result = match (frame.get("result"), frame.get("error")) {
                (Some(result), None) => Ok(result.clone()),
                (None, Some(error)) => Err(SessionError::ChildError(error.to_string())),
                _ => Err(SessionError::Protocol("invalid Tick response".into())),
            };
            if let Some(pending) = core.pending.lock().await.remove(&id) {
                let _ = pending.reply.send(result);
            }
            continue;
        }
        if frame.get("method").and_then(Value::as_str) != Some("tick/update") {
            link_state(
                &core,
                SessionLinkState::ProtocolError("unknown Tick notification".into()),
            )
            .await;
            return;
        }
        if line.len() > protocol::MAX_UPDATE_BYTES {
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
        content.read = TickRead {
            version: update.version,
            tick_instance_id: update.tick_instance_id.clone(),
            initialized: true,
            sequence: update.sequence,
            observed_at: update.observed_at.clone(),
            data: Some(update.data.clone()),
        };
        content.observation.update = Some(update);
        content.observation.received += 1;
        let _ = core.observations.send_replace(content.observation.clone());
    }
    link_state(&core, SessionLinkState::Gone).await;
    for (_, pending) in core.pending.lock().await.drain() {
        let _ = pending.reply.send(Err(SessionError::Gone));
    }
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
            let Some(line) = read_line_bounded(&mut reader, protocol::MAX_REQUEST_BYTES).await? else {
                return Ok(());
            };
            let frame = protocol::parse_request(&line).map_err(SessionError::Protocol)?;
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            write_frame(&mut writer, json!({"jsonrpc":"2.0","id":id,"error":{"code":-32010,"message":"Tick session is busy","data":{"dispatched":false}}})).await
        },
    )
    .await
    .map_err(|_| SessionError::Busy)?
}

async fn serve_connection(mut stream: UnixStream, core: Arc<Core>) -> Result<(), SessionError> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    loop {
        let line = tokio::select! { _ = core.shutdown.cancelled() => return Ok(()), line = tokio::time::timeout(HOST_REQUEST_TIMEOUT, read_line_bounded(&mut reader, protocol::MAX_REQUEST_BYTES)) => line.map_err(|_| SessionError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "Session request timed out")))?? };
        let Some(line) = line else {
            return Ok(());
        };
        let frame = protocol::parse_request(&line).map_err(SessionError::Protocol)?;
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let binding = frame.get("binding").cloned().unwrap_or(Value::Null);
        if binding["instance_id"] != core.instance.to_string()
            || binding["host_socket"] != core.host_socket.to_string_lossy().as_ref()
            || binding["terminal_id"] != core.terminal_id
        {
            write_frame(&mut writer, json!({"jsonrpc":"2.0","id":id,"error":{"code":-32014,"message":"Tick Session host, terminal, or identity mismatch"}})).await?;
            continue;
        }
        let method = frame["method"].as_str().unwrap_or_default();
        let params = frame.get("params").cloned().unwrap_or(json!({}));
        if method == "session/watch" {
            let (baseline, registration, mut observations) = core.watch().await;
            write_frame(&mut writer, json!({"jsonrpc":"2.0","id":id,"result":{"read":baseline,"received":registration.received,"state":registration.state}})).await?;
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
                    json!({"jsonrpc":"2.0","method":"session/observation","params":observation}),
                )
                .await?;
                if observation.state != SessionLinkState::Live {
                    return Ok(());
                }
            }
        }
        let response_reservation = if method == "tool/call" {
            Some(
                Arc::clone(&core.response_budget)
                    .try_acquire_many_owned(protocol::MAX_TOOL_RESPONSE_BYTES as u32),
            )
        } else {
            None
        };
        let result = if response_reservation.as_ref().is_some_and(Result::is_err) {
            Err(SessionError::Busy)
        } else {
            match method {
                "session/hello" => Ok(
                    json!({"instance_id":core.instance.to_string(),"protocol":protocol::NATIVE_PROTOCOL_VERSION,"host_socket":core.host_socket,"terminal_id":core.terminal_id}),
                ),
                "tick/describe" => core.request(method, params, HOST_REQUEST_TIMEOUT).await,
                "tick/read" => core.read().await.and_then(|read| {
                    serde_json::to_value(read)
                        .map_err(|error| SessionError::Protocol(error.to_string()))
                }),
                "tool/call" => core.call(params).await,
                _ => Err(SessionError::Protocol(format!(
                    "unknown Session method {method:?}"
                ))),
            }
        };
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => {
                let (code, dispatched) = match error {
                    SessionError::Busy => (-32010, false),
                    SessionError::CancelledBeforeSend => (-32011, false),
                    SessionError::OutcomeUnknown => (-32012, true),
                    SessionError::Gone => (-32013, true),
                    SessionError::ChildError(_) => (-32015, true),
                    _ => (-32000, false),
                };
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":error.to_string(),"data":{"dispatched":dispatched}}})
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
    if bytes.len() > protocol::MAX_TOOL_RESPONSE_BYTES {
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
        host_socket: PathBuf,
        terminal_id: String,
        runtime_dir: Option<PathBuf>,
    ) -> Result<Self, SessionError> {
        let uuid = instance
            .as_str()
            .strip_prefix("herdr:")
            .ok_or_else(|| SessionError::Protocol("invalid Native Tick identity".into()))?;
        uuid::Uuid::parse_str(uuid).map_err(|error| SessionError::Protocol(error.to_string()))?;
        let directory = resolve_runtime_dir(runtime_dir.as_deref())?;
        verify_owner(&directory)?;
        let socket = directory.join(format!("{uuid}.sock"));
        verify_owner(&socket)?;
        let client = Self {
            instance,
            socket,
            host_socket,
            terminal_id,
        };
        let hello = client.request("session/hello", json!({}), None).await?;
        if hello["instance_id"] != client.instance.to_string()
            || hello["protocol"] != protocol::NATIVE_PROTOCOL_VERSION
            || hello["host_socket"] != client.host_socket.to_string_lossy().as_ref()
            || hello["terminal_id"] != client.terminal_id
        {
            return Err(SessionError::Protocol(
                "Tick Session host, terminal, or identity mismatch".into(),
            ));
        }
        Ok(client)
    }

    pub async fn describe(&self) -> Result<TickDescriptor, SessionError> {
        let value = self.request("tick/describe", json!({}), None).await?;
        TickProtocol::new(self.instance.to_string())
            .descriptor(value)
            .map_err(SessionError::Protocol)
    }
    pub async fn read(&self) -> Result<TickRead, SessionError> {
        let value = self.request("tick/read", json!({}), None).await?;
        TickProtocol::new(self.instance.to_string())
            .read(value)
            .map_err(SessionError::Protocol)
    }
    pub async fn call(
        &self,
        tool: &str,
        args: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, SessionError> {
        if cancellation.is_cancelled() {
            return Err(SessionError::CancelledBeforeSend);
        }
        self.request(
            "tool/call",
            json!({"tool":tool,"args":args}),
            Some(cancellation),
        )
        .await
    }
    pub async fn call_prepared(
        &self,
        tool: &str,
        args: Value,
        expected_descriptor_digest: &str,
        cancellation: CancellationToken,
    ) -> Result<Value, SessionError> {
        if cancellation.is_cancelled() {
            return Err(SessionError::CancelledBeforeSend);
        }
        self.request("tool/call", json!({"tool":tool,"args":args,"expected_descriptor_digest":expected_descriptor_digest}), Some(cancellation)).await
    }
    pub async fn watch(
        &self,
    ) -> Result<(TickRead, u64, watch::Receiver<SessionObservation>), SessionError> {
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
        let baseline = TickProtocol::new(self.instance.to_string())
            .read(registration["read"].clone())
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
        let timeout = if method == "tool/call" {
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
        }).await.map_err(|_| if method == "tool/call" { SessionError::OutcomeUnknown } else { SessionError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "Session response timed out")) })??;
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
    let mut bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params,
        "binding":{"instance_id":client.instance.to_string(),"host_socket":client.host_socket,"terminal_id":client.terminal_id}
    })).map_err(|error| SessionError::Protocol(error.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() > protocol::MAX_REQUEST_BYTES {
        return Err(SessionError::Protocol("Session request too large".into()));
    }
    Ok(bytes)
}

async fn read_client_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value, SessionError> {
    let line = read_line_bounded(reader, protocol::MAX_TOOL_RESPONSE_BYTES)
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
                Some(-32015) => Err(SessionError::ChildError(message)),
                _ => Err(SessionError::Protocol(message)),
            }
        }
        _ => Err(SessionError::Protocol("invalid Session response".into())),
    }
}

fn resolve_runtime_dir(configured: Option<&Path>) -> Result<PathBuf, SessionError> {
    let directory = configured
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("TICKLOOP_TICK_RUNTIME_DIR").map(PathBuf::from))
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
