//! Session-scoped, explicitly admitted stdio MCP servers. This is not an OS sandbox.

impl crate::Agent {
    /// Explicit session teardown, never used for per-prompt settlement.
    pub async fn close_session_mcp(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
    ) -> Result<(), crate::AgentError> {
        {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(crate::AgentError::NotFound)?;
            if chat.snapshot.delegation.is_some() {
                return Err(crate::AgentError::Invalid(
                    "only the session root can close its MCP provider".into(),
                ));
            }
        }
        let provider = self.inner.host_tools.read().unwrap().clone();
        if let Some(provider) = provider {
            provider
                .close_session()
                .await
                .map_err(crate::AgentError::Invalid)?;
        }
        Ok(())
    }

    pub(super) async fn bootstrap_session_mcp(
        &self,
        receipt: &tickloop_agent_protocol::Receipt,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        use crate::session_permissions::{PermissionOwner, SessionPermissionOperation};
        use tickloop_agent_protocol::{
            Decision, EventKind, ExecutionMode, OperationRecord, OperationState,
            PermissionApprovalSource, PermissionExecution, PermissionRequest, PromptPolicy,
        };
        use tickloop_agent_tools::permissions::PermissionPolicyExt;
        let provider = self.inner.host_tools.read().unwrap().clone();
        let Some(provider) = provider else {
            return Ok(());
        };
        let starts = provider.session_bootstrap()?;
        if starts.is_empty() {
            return Ok(());
        }
        for call in starts {
            let operation_id = uuid::Uuid::new_v4();
            let (session_operation, evaluation, revision, ancestors) = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats
                    .get(&receipt.chat_id)
                    .ok_or("MCP session unavailable")?;
                if chat.snapshot.delegation.is_some()
                    || chat.active_run != Some(receipt.run_id)
                    || cancel.is_cancelled()
                {
                    return Err("MCP bootstrap requires the active root Run".into());
                }
                let session_operation = SessionPermissionOperation::from_normalized(
                    call.name().into(),
                    call.input().clone(),
                    crate::session_permissions::capture_workspace_scope(
                        &crate::configured_workspace_for_chat(chat),
                    )
                    .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                let sessions = self.inner.session_permissions.lock().await;
                let owner = PermissionOwner::new(receipt.chat_id, receipt.loop_id)
                    .map_err(|e| e.to_string())?;
                let evaluation = sessions.evaluate(owner, &session_operation);
                let policy = crate::delegation::effective_policy(&chats, receipt)?;
                if policy.mode == ExecutionMode::Plan
                    || policy.explicit_decision_with_session(
                        call.name(),
                        call.input(),
                        receipt.loop_id,
                        evaluation.decision,
                    ) == Some(Decision::Deny)
                {
                    return Err("MCP server startup denied by current permission policy".into());
                }
                if self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt {
                    return Err("MCP server startup requires one-time human approval".into());
                }
                let revision = policy.revision;
                let ancestors = crate::delegation::ancestor_revisions(&chats, receipt.chat_id)?;
                chats
                    .get_mut(&receipt.chat_id)
                    .unwrap()
                    .snapshot
                    .operations
                    .push(OperationRecord {
                        task_binding: None,
                        id: operation_id,
                        loop_id: receipt.loop_id,
                        run_id: receipt.run_id,
                        call_id: format!("host-operation:{operation_id}"),
                        name: call.name().into(),
                        input: call.input().clone(),
                        file_change: None,
                        state: OperationState::Proposed,
                        result: None,
                    });
                (session_operation, evaluation, revision, ancestors)
            };
            let request = PermissionRequest {
                id: uuid::Uuid::new_v4(),
                loop_id: receipt.loop_id,
                execution: PermissionExecution::HostOperation {
                    run_id: receipt.run_id,
                    operation_id,
                },
                name: call.name().into(),
                input: call.input().clone(),
                file_change: None,
                approval_reason: Some("Start this session-scoped MCP stdio server once. Its process is not OS-sandboxed.".into()),
                approval_source: PermissionApprovalSource::ExplicitAsk,
            };
            let answer = self
                .wait_for_permission(
                    receipt.chat_id,
                    request,
                    session_operation.clone(),
                    cancel.clone(),
                )
                .await;
            let answer = match answer {
                Ok(answer) => answer,
                Err(error) => {
                    self.finish_mcp_bootstrap_operation(receipt.chat_id, operation_id, false)
                        .await;
                    return Err(error);
                }
            };
            let admission_result = async {
                let mut chats = self.inner.chats.lock().await;
                let sessions = self.inner.session_permissions.lock().await;
                let chat = chats
                    .get(&receipt.chat_id)
                    .ok_or("MCP session unavailable")?;
                let policy = crate::delegation::effective_policy(&chats, receipt)?;
                let current = sessions.evaluate(
                    PermissionOwner::new(receipt.chat_id, receipt.loop_id)
                        .map_err(|e| e.to_string())?,
                    &session_operation,
                );
                let admitted = answer.allow
                    && answer.revision == revision
                    && policy.revision == revision
                    && crate::delegation::ancestor_revisions(&chats, receipt.chat_id)? == ancestors
                    && sessions.revalidate(
                        &evaluation.token,
                        PermissionOwner::new(receipt.chat_id, receipt.loop_id)
                            .map_err(|e| e.to_string())?,
                        &session_operation,
                    )
                    && policy.explicit_decision_with_session(
                        call.name(),
                        call.input(),
                        receipt.loop_id,
                        current.decision,
                    ) != Some(Decision::Deny)
                    && chat.active_run == Some(receipt.run_id)
                    && !cancel.is_cancelled();
                let chat = chats.get_mut(&receipt.chat_id).unwrap();
                chat.snapshot
                    .operations
                    .iter_mut()
                    .find(|op| op.id == operation_id)
                    .unwrap()
                    .state = if admitted {
                    OperationState::Admitted
                } else {
                    OperationState::Failed
                };
                self.emit(
                    chat,
                    receipt,
                    EventKind::PermissionChecked {
                        decision: if admitted {
                            Decision::Allow
                        } else {
                            Decision::Deny
                        },
                        reason: if admitted {
                            "One-time session MCP startup approved".into()
                        } else {
                            "Session MCP startup denied, cancelled or permission revision changed"
                                .into()
                        },
                    },
                );
                Ok::<bool, String>(admitted)
            }
            .await;
            let admitted = match admission_result {
                Ok(admitted) => admitted,
                Err(error) => {
                    self.finish_mcp_bootstrap_operation(receipt.chat_id, operation_id, false)
                        .await;
                    return Err(error);
                }
            };
            if !admitted {
                self.finish_mcp_bootstrap_operation(receipt.chat_id, operation_id, false)
                    .await;
                return Err("MCP startup approval denied, cancelled or expired".into());
            }
            let result = provider.start_session_server(call, cancel.clone()).await;
            self.finish_mcp_bootstrap_operation(receipt.chat_id, operation_id, result.is_ok())
                .await;
            result?;
        }
        // Titling waited for the approval that every model call needs first.
        self.start_title_generation(receipt.chat_id).await;
        Ok(())
    }

    async fn finish_mcp_bootstrap_operation(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        operation_id: uuid::Uuid,
        completed: bool,
    ) {
        let mut chats = self.inner.chats.lock().await;
        if let Some(chat) = chats.get_mut(&chat_id) {
            if let Some(operation) = chat
                .snapshot
                .operations
                .iter_mut()
                .find(|op| op.id == operation_id)
            {
                operation.state = if completed {
                    tickloop_agent_protocol::OperationState::Completed
                } else {
                    tickloop_agent_protocol::OperationState::Failed
                };
            }
            self.save(&chat.snapshot);
        }
    }
}

use crate::host_tools::{
    HostCallResult, HostCallScope, HostToolPermission, HostToolProvider, McpResourceServer,
    McpServerInstructions, PreparedHostCall, WorkspaceHostContext,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tickloop_agent_protocol::{
    ChatId, HostEndpointIdentity, HostToolAuthority, RemoteSkillDefinition, SessionMcpServer,
    SkillSource, ToolDefinition, ToolResult, validate_session_mcp_servers,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MCP_SERVER_START: &str = "MCPServerStart";
const MAX_FRAME: usize = 512 * 1024;
const MAX_TOOLS: usize = 256;
const MAX_SCHEMA: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct NoSchemaRetrieval;
impl jsonschema::Retrieve for NoSchemaRetrieval {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("External MCP schema retrieval is disabled".into())
    }
}
fn schema_validator(schema: &Value) -> Result<jsonschema::Validator, String> {
    // An untrusted tools/list schema must never fetch a URL or read a file through $ref.
    jsonschema::options()
        .with_retriever(NoSchemaRetrieval)
        .build(schema)
        .map_err(|_| "invalid session MCP schema".to_owned())
}

/// The Worker owns this handle; dropping it must close the supervisor's sole control writer.
#[async_trait]
pub trait SessionMcpLifetime: Send + Sync {
    async fn close(&mut self) -> Result<(), String>;
}
pub struct SessionMcpTransport {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    pub lifetime: Box<dyn SessionMcpLifetime>,
}
#[async_trait]
pub trait SessionMcpLauncher: Send + Sync {
    async fn launch(&self, descriptor: &SessionMcpServer) -> Result<SessionMcpTransport, String>;
}

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    path: PathBuf,
    device: u64,
    inode: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    mode: u32,
}
fn identify(path: &std::path::Path, directory: bool) -> Result<Identity, String> {
    use std::os::unix::fs::MetadataExt;
    let path = std::fs::canonicalize(path).map_err(|_| "MCP launch path unavailable")?;
    let meta = std::fs::metadata(&path).map_err(|_| "MCP launch identity unavailable")?;
    if (directory && !meta.is_dir())
        || (!directory && (!meta.is_file() || meta.mode() & 0o111 == 0))
    {
        return Err("MCP launch path has an incompatible type or mode".into());
    }
    Ok(Identity {
        path,
        device: meta.dev(),
        inode: meta.ino(),
        len: meta.len(),
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        mode: meta.mode(),
    })
}
#[derive(Clone)]
struct LaunchIdentity {
    program: Identity,
    cwd: Identity,
}
impl LaunchIdentity {
    fn read(server: &SessionMcpServer) -> Result<Self, String> {
        Ok(Self {
            program: identify(&server.program, false)?,
            cwd: identify(&server.cwd, true)?,
        })
    }
    fn revalidate(&self, server: &SessionMcpServer) -> Result<(), String> {
        // Directory content changes do not replace the directory itself.
        let current = Self::read(server)?;
        if current.program != self.program
            || current.cwd.path != self.cwd.path
            || current.cwd.device != self.cwd.device
            || current.cwd.inode != self.cwd.inode
        {
            return Err("MCP program or cwd changed after preparation".into());
        }
        Ok(())
    }
}
struct PreparedStart {
    index: usize,
    id: Uuid,
    identity: LaunchIdentity,
}
struct PreparedCall {
    index: usize,
    revision: [u8; 32],
    original: String,
    arguments: Value,
}
struct Wire {
    reader: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
}
/// Why a request produced no result. Only a server's own JSON-RPC error answer leaves the
/// connection usable; every other failure closes it.
enum RequestError {
    /// The server answered with this JSON-RPC error object.
    Rejected(Value),
    /// Timeout, cancellation, I/O or framing failure; the connection is closed.
    Failed(String),
}
impl From<RequestError> for String {
    fn from(error: RequestError) -> Self {
        match error {
            RequestError::Rejected(_) => "MCP server rejected the request".into(),
            RequestError::Failed(message) => message,
        }
    }
}
struct Peer {
    wire: Mutex<Wire>,
    lifetime: Mutex<Option<Box<dyn SessionMcpLifetime>>>,
    next: AtomicU64,
    broken: AtomicBool,
}
impl Peer {
    fn new(transport: SessionMcpTransport) -> Self {
        Self {
            wire: Mutex::new(Wire {
                reader: BufReader::new(transport.reader),
                writer: transport.writer,
            }),
            lifetime: Mutex::new(Some(transport.lifetime)),
            next: AtomicU64::new(1),
            broken: AtomicBool::new(false),
        }
    }
    async fn close(&self) -> Result<(), String> {
        self.broken.store(true, Ordering::SeqCst);
        let mut lifetime = self.lifetime.lock().await;
        if let Some(handle) = lifetime.as_mut() {
            handle.close().await?;
        }
        lifetime.take();
        Ok(())
    }
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RequestError> {
        if self.broken.load(Ordering::SeqCst) || cancel.is_cancelled() {
            return Err(RequestError::Failed(
                "MCP connection unavailable or cancelled".into(),
            ));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let operation = async {
            let mut wire = self.wire.lock().await;
            if self.broken.load(Ordering::SeqCst) {
                return Err("MCP connection closed".into());
            }
            let mut bytes = serde_json::to_vec(
                &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            )
            .map_err(|_| "invalid MCP request")?;
            if bytes.len() > MAX_FRAME {
                return Err("MCP request exceeds size limit".into());
            }
            bytes.push(b'\n');
            wire.writer
                .write_all(&bytes)
                .await
                .map_err(|_| "MCP request write failed")?;
            wire.writer
                .flush()
                .await
                .map_err(|_| "MCP request write failed")?;
            for _ in 0..256 {
                let mut frame = Vec::new();
                let read = (&mut wire.reader)
                    .take((MAX_FRAME + 1) as u64)
                    .read_until(b'\n', &mut frame)
                    .await
                    .map_err(|_| "MCP response read failed")?;
                if read == 0 || frame.len() > MAX_FRAME || !frame.ends_with(b"\n") {
                    return Err("MCP response is closed or oversized".into());
                }
                let response: Value =
                    serde_json::from_slice(&frame).map_err(|_| "invalid MCP response")?;
                if response["jsonrpc"] != "2.0" {
                    return Err("invalid MCP response version".into());
                }
                if response.get("method").is_some() {
                    // A ping gets the empty result MCP requires. Sampling, roots, elicitation and
                    // all other client callbacks are unsupported.
                    if let Some(request_id) = response.get("id") {
                        let reply = if response["method"] == "ping" {
                            json!({"jsonrpc":"2.0","id":request_id,"result":{}})
                        } else {
                            json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"Client capability not supported"}})
                        };
                        let mut reply =
                            serde_json::to_vec(&reply).map_err(|_| "invalid MCP callback")?;
                        reply.push(b'\n');
                        wire.writer
                            .write_all(&reply)
                            .await
                            .map_err(|_| "MCP callback reply failed")?;
                        wire.writer
                            .flush()
                            .await
                            .map_err(|_| "MCP callback reply failed")?;
                    }
                    continue;
                }
                if response["id"] != id {
                    return Err("MCP response identity mismatch".into());
                }
                if let Some(error) = response.get("error") {
                    return Ok(Err(error.clone()));
                }
                return response
                    .get("result")
                    .cloned()
                    .map(Ok)
                    .ok_or_else(|| "MCP result missing".into());
            }
            Err("MCP notification limit exceeded".into())
        };
        let result: Result<Result<Value, Value>, String> = tokio::select! { biased;
            () = cancel.cancelled() => Err("MCP request cancelled".into()),
            result = tokio::time::timeout(REQUEST_TIMEOUT, operation) => result.unwrap_or_else(|_| Err("MCP request timed out".into())),
        };
        match result {
            Ok(Ok(result)) => Ok(result),
            // The server answered on an intact connection, which stays open.
            Ok(Err(error)) => Err(RequestError::Rejected(error)),
            Err(failure) => {
                let _ = self.close().await;
                Err(RequestError::Failed(failure))
            }
        }
    }
    async fn initialize(&self, cancel: CancellationToken) -> Result<(bool, bool), String> {
        let result = self.request("initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"tickloop","version":"1"}}), cancel.clone()).await?;
        if !matches!(
            result["protocolVersion"].as_str(),
            Some("2025-11-25" | "2025-06-18" | "2024-11-05")
        ) {
            return Err("unsupported MCP protocol version".into());
        }
        let notification = async {
            let mut wire = self.wire.lock().await;
            wire.writer
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await
                .map_err(|_| "MCP initialization failed")?;
            wire.writer
                .flush()
                .await
                .map_err(|_| "MCP initialization failed")
        };
        tokio::select! { biased;
            () = cancel.cancelled() => return Err("MCP initialization cancelled".into()),
            notified = tokio::time::timeout(REQUEST_TIMEOUT, notification) => {
                notified.map_err(|_| "MCP initialization timed out")??;
            }
        }
        Ok((
            result["capabilities"].get("tools").is_some(),
            result["capabilities"].get("resources").is_some(),
        ))
    }
}
#[derive(Clone)]
struct Directory {
    revision: [u8; 32],
    tools: BTreeMap<String, ToolDefinition>,
    originals: BTreeMap<String, String>,
    resources: bool,
}
struct Server {
    descriptor: SessionMcpServer,
    id: Uuid,
    /// Launch values kept out of everything the model sees, longest first.
    private: Vec<String>,
    state: RwLock<ServerState>,
}
#[derive(Default)]
struct ServerState {
    attempted: bool,
    peer: Option<Arc<Peer>>,
    /// Whether initialization declared tools and resources; a failed refresh keeps these so a
    /// later one can restore the directory.
    capabilities: Option<(bool, bool)>,
    directory: Option<Directory>,
}

/// Shorter values are never treated as private, so common strings such as `.`, `-y` or `run`
/// never redact ordinary output or reject a directory.
const MIN_PRIVATE_VALUE: usize = 8;

/// The launch values kept out of everything the model sees: every explicit argument and
/// environment value, plus the credential a credential-shaped argument embeds (`--token=V`,
/// `API_KEY=V`, `Authorization: Bearer V`, URL user information or credential query parameters),
/// which a server may echo on its own.
fn private_launch_values(server: &SessionMcpServer) -> Vec<String> {
    /// A flag, variable or header name (not a URL or path) that names a credential.
    fn names_credential(name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
            && [
                "key",
                "token",
                "secret",
                "password",
                "passwd",
                "credential",
                "auth",
            ]
            .iter()
            .any(|word| name.contains(word))
    }
    let mut values = server
        .args
        .iter()
        .chain(server.env.values())
        .cloned()
        .collect::<Vec<_>>();
    for argument in &server.args {
        if let Some((name, value)) = argument.split_once('=')
            && names_credential(name)
        {
            values.push(value.into());
        } else if let Some((name, value)) = argument.split_once(':')
            && !value.starts_with("//")
            && names_credential(name.rsplit('=').next().unwrap_or(name))
        {
            let value = value.trim();
            values.push(value.into());
            if let Some((_, token)) = value.split_once(' ') {
                values.push(token.trim().into());
            }
        }
        for (_, rest) in argument
            .match_indices("://")
            .map(|(at, _)| argument.split_at(at + 3))
        {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            if let Some((user_information, _)) = authority.rsplit_once('@') {
                values.push(user_information.into());
            }
        }
        if let Some((_, query)) = argument.split_once('?') {
            for (name, value) in query
                .split(['&', '#'])
                .filter_map(|pair| pair.split_once('='))
            {
                if names_credential(name) {
                    values.push(value.into());
                }
            }
        }
    }
    values.retain(|value| value.len() >= MIN_PRIVATE_VALUE);
    values.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    values.dedup();
    values
}

/// Outer provider: session tools coexist with native host tools and fixed Artifacts/Skill support.
pub struct SessionMcpProvider {
    servers: Vec<Server>,
    native: Arc<dyn HostToolProvider>,
    launcher: Arc<dyn SessionMcpLauncher>,
    closed: AtomicBool,
    startup_gate: Mutex<()>,
}
impl SessionMcpProvider {
    pub fn new(
        servers: Vec<SessionMcpServer>,
        native: Arc<dyn HostToolProvider>,
        launcher: Arc<dyn SessionMcpLauncher>,
    ) -> Result<Self, String> {
        validate_session_mcp_servers(&servers)?;
        Ok(Self {
            servers: servers
                .into_iter()
                .map(|descriptor| Server {
                    private: private_launch_values(&descriptor),
                    descriptor,
                    id: Uuid::new_v4(),
                    state: RwLock::new(ServerState::default()),
                })
                .collect(),
            native,
            launcher,
            closed: AtomicBool::new(false),
            startup_gate: Mutex::new(()),
        })
    }
    fn authority(&self, index: usize) -> HostToolAuthority {
        HostToolAuthority::Mcp {
            server_id: format!("session-mcp:{}", self.servers[index].id),
            account_id: "session".into(),
        }
    }
    /// The server offering `name`. A name two servers' directories both produce, such as
    /// `a_` + `b` and `a` + `_b`, is offered by neither, whatever order they started or
    /// refreshed in.
    fn tool(&self, name: &str) -> Option<(usize, Directory)> {
        let states = self.states();
        let mut offering = states.iter().enumerate().filter_map(|(index, state)| {
            state
                .directory
                .as_ref()
                .filter(|dir| dir.tools.contains_key(name))
                .map(|dir| (index, dir))
        });
        let (index, directory) = offering.next()?;
        offering
            .next()
            .is_none()
            .then(|| (index, directory.clone()))
    }
    /// Every server's state at once, so lookups see one consistent set of directories.
    fn states(&self) -> Vec<std::sync::RwLockReadGuard<'_, ServerState>> {
        self.servers
            .iter()
            .map(|server| server.state.read().unwrap())
            .collect()
    }
    fn peer(&self, index: usize) -> Result<Arc<Peer>, String> {
        self.servers[index]
            .state
            .read()
            .unwrap()
            .peer
            .clone()
            .filter(|peer| !peer.broken.load(Ordering::SeqCst))
            .ok_or_else(|| "session MCP server unavailable".into())
    }
    fn sanitize(&self, index: usize, value: &mut Value) {
        fn visit(value: &mut Value, secrets: &[&str]) {
            match value {
                Value::String(text) => {
                    for secret in secrets {
                        *text = text.replace(secret, "[redacted]");
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        visit(item, secrets);
                    }
                }
                Value::Object(fields) => {
                    let original = std::mem::take(fields);
                    for (mut key, mut item) in original {
                        for secret in secrets {
                            key = key.replace(secret, "[redacted]");
                        }
                        visit(&mut item, secrets);
                        fields.insert(key, item);
                    }
                }
                _ => {}
            }
        }
        let secrets = self.servers[index]
            .private
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        visit(value, &secrets);
    }
    async fn directory(
        &self,
        index: usize,
        tools_supported: bool,
        resources: bool,
        cancel: CancellationToken,
    ) -> Result<Directory, String> {
        if !tools_supported {
            return Ok(Directory {
                revision: Sha256::digest(b"session-mcp:no-tools").into(),
                tools: BTreeMap::new(),
                originals: BTreeMap::new(),
                resources,
            });
        }
        let peer = self.peer(index)?;
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        let mut tools = BTreeMap::new();
        let mut originals = BTreeMap::new();
        for _ in 0..16 {
            let result = peer
                .request(
                    "tools/list",
                    cursor
                        .as_ref()
                        .map_or_else(|| json!({}), |c| json!({"cursor":c})),
                    cancel.clone(),
                )
                .await?;
            let page = result["tools"]
                .as_array()
                .ok_or("invalid session MCP tool directory")?;
            if page.len() + tools.len() > MAX_TOOLS {
                return Err("session MCP tool limit exceeded".into());
            }
            for tool in page {
                let original = tool["name"]
                    .as_str()
                    .filter(|name| {
                        !name.is_empty()
                            && name.len() <= 96
                            && name.bytes().all(|c| {
                                c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')
                            })
                    })
                    .ok_or("invalid session MCP tool name")?;
                let name = format!("mcp__{}__{}", self.servers[index].descriptor.name, original);
                if tools.contains_key(&name)
                    || self.native.definitions().iter().any(|d| d.name == name)
                {
                    return Err("session MCP tool name collision".into());
                }
                // Keep private launch values out of all model-visible directory metadata. A name
                // routes the call, so one that would need redaction is not offered at all.
                let mut public_tool = tool.clone();
                self.sanitize(index, &mut public_tool);
                if public_tool["name"] != tool["name"] {
                    continue;
                }
                let schema = public_tool
                    .get("inputSchema")
                    .cloned()
                    .ok_or("session MCP schema missing")?;
                if serde_json::to_vec(&schema)
                    .map_err(|_| "invalid MCP schema")?
                    .len()
                    > MAX_SCHEMA
                    || schema["type"] != "object"
                    || schema_validator(&schema).is_err()
                {
                    return Err("invalid or oversized session MCP tool schema".into());
                }
                let description = public_tool["description"].as_str().unwrap_or(original);
                if description.len() > 16 * 1024 {
                    return Err("session MCP description too large".into());
                }
                originals.insert(name.clone(), original.to_owned());
                tools.insert(
                    name.clone(),
                    ToolDefinition {
                        name,
                        description: description.into(),
                        input_schema: schema,
                    },
                );
            }
            cursor = match result.get("nextCursor") {
                None | Some(Value::Null) => None,
                Some(Value::String(c)) if c.len() <= 4096 && !c.is_empty() => Some(c.clone()),
                _ => return Err("invalid session MCP cursor".into()),
            };
            if cursor.is_none() {
                // Schemas keep the server's key order, which must not change the revision.
                let revision = Sha256::digest(
                    serde_json::to_value(&tools)
                        .and_then(|tools| tickloop_agent_base::canonical_json_bytes(&tools))
                        .map_err(|_| "invalid MCP directory")?,
                )
                .into();
                return Ok(Directory {
                    revision,
                    tools,
                    originals,
                    resources,
                });
            }
            if !seen.insert(cursor.clone().unwrap()) {
                return Err("session MCP pagination cycle".into());
            }
        }
        Err("session MCP directory pagination limit exceeded".into())
    }
    async fn refresh_server(&self, index: usize, cancel: CancellationToken) -> Result<(), String> {
        let (tools_supported, resources) = self.servers[index]
            .state
            .read()
            .unwrap()
            .capabilities
            .ok_or("session MCP directory unavailable")?;
        let result = self
            .directory(index, tools_supported, resources, cancel)
            .await;
        self.servers[index].state.write().unwrap().directory = result.as_ref().ok().cloned();
        result.map(|_| ())
    }
}

#[async_trait]
impl HostToolProvider for SessionMcpProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut tools = self.native.definitions();
        let states = self.states();
        let directories = states
            .iter()
            .filter_map(|state| state.directory.as_ref())
            .collect::<Vec<_>>();
        for directory in &directories {
            tools.extend(
                directory
                    .tools
                    .values()
                    .filter(|tool| {
                        directories
                            .iter()
                            .filter(|other| other.tools.contains_key(&tool.name))
                            .count()
                            == 1
                    })
                    .cloned(),
            );
        }
        tools
    }
    fn session_bootstrap(&self) -> Result<Vec<PreparedHostCall>, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let mut starts = self.native.session_bootstrap()?;
        for (index, server) in self.servers.iter().enumerate() {
            if server.state.read().unwrap().attempted {
                continue;
            }
            let identity = LaunchIdentity::read(&server.descriptor)?;
            let input = json!({"id":server.id,"server":server.descriptor.name,"program":identity.program.path,"cwd":identity.cwd.path,"argument_count":server.descriptor.args.len(),"environment_keys":server.descriptor.env.keys().collect::<Vec<_>>()});
            starts.push(
                PreparedHostCall::new(
                    MCP_SERVER_START.into(),
                    input.clone(),
                    input,
                    HostToolPermission::Confirmation,
                    "Start one session MCP stdio server (not an OS sandbox)".into(),
                )
                .with_payload(PreparedStart {
                    index,
                    id: server.id,
                    identity,
                }),
            );
        }
        Ok(starts)
    }
    async fn start_session_server(
        &self,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let _gate = self.startup_gate.lock().await;
        if self.closed.load(Ordering::SeqCst) || cancel.is_cancelled() {
            return Err("session MCP startup cancelled".into());
        }
        let start = match call.into_payload::<PreparedStart>() {
            Ok(start) => start,
            Err(call) => return self.native.start_session_server(*call, cancel).await,
        };
        let server = self
            .servers
            .get(start.index)
            .ok_or("session MCP startup target unavailable")?;
        if server.id != start.id || server.state.read().unwrap().attempted {
            return Err("session MCP startup approval was already consumed".into());
        }
        start.identity.revalidate(&server.descriptor)?;
        server.state.write().unwrap().attempted = true;
        let mut descriptor = server.descriptor.clone();
        descriptor.program = start.identity.program.path;
        descriptor.cwd = start.identity.cwd.path;
        let transport = tokio::select! { biased;
            () = cancel.cancelled() => return Err("MCP startup cancelled".into()),
            launched = tokio::time::timeout(REQUEST_TIMEOUT, self.launcher.launch(&descriptor)) => {
                launched.map_err(|_| "MCP launcher timed out")??
            }
        };
        let peer = Arc::new(Peer::new(transport));
        server.state.write().unwrap().peer = Some(peer.clone());
        let result = async {
            let (tools_supported, resources) = peer.initialize(cancel.clone()).await?;
            server.state.write().unwrap().capabilities = Some((tools_supported, resources));
            let directory = self
                .directory(start.index, tools_supported, resources, cancel)
                .await?;
            server.state.write().unwrap().directory = Some(directory);
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = peer.close().await;
        }
        result
    }
    async fn close_session(&self) -> Result<(), String> {
        self.closed.store(true, Ordering::SeqCst);
        let _gate = self.startup_gate.lock().await;
        let mut failure = None;
        for server in &self.servers {
            let peer = {
                let mut state = server.state.write().unwrap();
                state.directory = None;
                state.peer.clone()
            };
            if let Some(peer) = peer {
                match peer.close().await {
                    Ok(()) => {
                        server.state.write().unwrap().peer = None;
                    }
                    Err(error) => {
                        failure = Some(error);
                    }
                }
            }
        }
        if let Err(error) = self.native.close_session().await {
            failure = Some(error);
        }
        failure.map_or(Ok(()), Err)
    }
    async fn shutdown(&self) {
        let _ = self.close_session().await;
        self.native.shutdown().await;
    }
    fn upgrade_checkpoint(&self) -> Result<Option<Value>, String> {
        // Descriptor replay and process transfer are deliberately unsupported. Even dormant
        // session descriptors must not disappear or restart during a Worker replacement.
        if (!self.servers.is_empty() && !self.closed.load(Ordering::SeqCst))
            || self
                .servers
                .iter()
                .any(|server| server.state.read().unwrap().peer.is_some())
        {
            return Ok(None);
        }
        self.native.upgrade_checkpoint()
    }
    fn restore_upgrade(&self, state: Value) -> Result<(), String> {
        self.native.restore_upgrade(state)
    }
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        self.native.host_endpoint()
    }
    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        self.native.workspace_host_context()
    }
    fn artifact_client(&self) -> Option<Arc<tickloop_agent_artifacts::ArtifactClient>> {
        self.native.artifact_client()
    }
    fn remote_skills(&self) -> Vec<RemoteSkillDefinition> {
        self.native.remote_skills()
    }
    fn remote_skill_is_current(&self, source: &SkillSource) -> bool {
        self.native.remote_skill_is_current(source)
    }
    async fn read_remote_skill(
        &self,
        definition: &RemoteSkillDefinition,
        relative: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        self.native
            .read_remote_skill(definition, relative, cancel)
            .await
    }
    fn tool_set_is_confirmed(&self) -> bool {
        self.native.tool_set_is_confirmed()
    }
    fn service_connections(&self) -> Value {
        let mut connections = self.native.service_connections();
        connections["session_mcp"]=json!(self.servers.iter().map(|server| { let state=server.state.read().unwrap(); json!({"id":server.id,"name":server.descriptor.name,"state":if state.directory.is_some(){"ready"}else if state.attempted{"unavailable"}else{"pending"}}) }).collect::<Vec<_>>());
        connections
    }
    async fn refresh(&self, cancel: CancellationToken) -> Result<(), String> {
        let native = self.native.refresh(cancel.clone()).await;
        for (index, server) in self.servers.iter().enumerate() {
            let connected = server.state.read().unwrap().peer.is_some();
            if connected {
                let _ = self.refresh_server(index, cancel.clone()).await;
            }
        }
        native
    }
    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        if self.tool(name).is_some() {
            schema_validator(schema)
                .map_err(|_| "invalid session MCP schema")?
                .validate(input)
                .map_err(|_| "invalid session MCP input".into())
        } else {
            self.native.validate_input(name, schema, input)
        }
    }
    fn permission(&self, chat: ChatId, name: &str, input: &Value) -> HostToolPermission {
        if self.tool(name).is_some() {
            HostToolPermission::Reviewable
        } else {
            self.native.permission(chat, name, input)
        }
    }
    fn is_concurrency_safe(&self, name: &str, input: &Value) -> bool {
        self.tool(name).is_none() && self.native.is_concurrency_safe(name, input)
    }
    fn tool_authorities(&self, name: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        match self.tool(name) {
            Some((index, _)) => Ok(BTreeSet::from([self.authority(index)])),
            None => self.native.tool_authorities(name),
        }
    }
    fn call_authority(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<Option<HostToolAuthority>, String> {
        match self.tool(name) {
            Some((index, _)) => Ok(Some(self.authority(index))),
            None => self.native.call_authority(name, input),
        }
    }
    async fn prepare_call_in_scope(
        &self,
        chat: ChatId,
        scope: &HostCallScope<'_>,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if self.tool(name).is_some() {
            self.prepare_call(chat, name, input, cancel).await
        } else {
            self.native
                .prepare_call_in_scope(chat, scope, name, input, cancel)
                .await
        }
    }
    async fn prepare_call(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let Some((index, _)) = self.tool(name) else {
            return self.native.prepare_call(chat, name, input, cancel).await;
        };
        self.refresh_server(index, cancel).await?;
        let (_, directory) = self.tool(name).ok_or("session MCP tool disappeared")?;
        self.validate_input(name, &directory.tools[name].input_schema, input)?;
        if serde_json::to_vec(input)
            .map_err(|_| "invalid MCP input")?
            .len()
            > 64 * 1024
        {
            return Err("session MCP input exceeds limit".into());
        }
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            input.clone(),
            HostToolPermission::Reviewable,
            "Call a session MCP tool; server annotations do not grant authority".into(),
        )
        .with_payload(PreparedCall {
            index,
            revision: directory.revision,
            original: directory.originals[name].clone(),
            arguments: input.clone(),
        }))
    }
    async fn revalidate_call(
        &self,
        chat: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let Some(prepared) = call.payload::<PreparedCall>() else {
            return self.native.revalidate_call(chat, call, cancel).await;
        };
        self.refresh_server(prepared.index, cancel).await?;
        let (_, directory) = self
            .tool(call.name())
            .ok_or("session MCP tool disappeared")?;
        if directory.revision != prepared.revision {
            return Err("session MCP directory changed; prepare and approve again".into());
        }
        Ok(())
    }
    async fn execute_prepared(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if call.payload::<PreparedCall>().is_none() {
            return self.native.execute_prepared(chat, call, cancel).await;
        }
        if let Err(error) = self.revalidate_call(chat, &call, cancel.clone()).await {
            return HostCallResult::NotDispatched(error);
        }
        let prepared = match call.into_payload::<PreparedCall>() {
            Ok(p) => p,
            Err(_) => return HostCallResult::NotDispatched("invalid MCP prepared call".into()),
        };
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched("MCP call cancelled".into());
        }
        let peer = match self.peer(prepared.index) {
            Ok(p) => p,
            Err(e) => return HostCallResult::NotDispatched(e),
        };
        match peer.request("tools/call",json!({"name":prepared.original,"arguments":prepared.arguments}),cancel).await{
            Ok(mut result)=>{self.sanitize(prepared.index,&mut result);let is_error=result["isError"]==true;HostCallResult::Completed(ToolResult{content:result.to_string(),is_error,output_ref:None})},
            // The server answered, so the outcome is known: it refused the call.
            Err(RequestError::Rejected(error))=>{
                let mut error=json!({"code":error.get("code"),"message":error.get("message")});
                self.sanitize(prepared.index,&mut error);
                HostCallResult::Completed(ToolResult{content:format!("session MCP server rejected the call: {error}"),is_error:true,output_ref:None})
            },
            Err(RequestError::Failed(_))=>HostCallResult::OutcomeUnknown("session MCP response unavailable after dispatch; do not automatically repeat the call".into()),
        }
    }
    async fn execute(
        &self,
        chat: ChatId,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        if self.tool(name).is_some() {
            return ToolResult {
                content: "session MCP tools require a prepared call".into(),
                is_error: true,
                output_ref: None,
            };
        }
        self.native.execute(chat, name, input, cancel).await
    }
    fn mcp_resource_server(&self) -> Option<McpResourceServer> {
        self.native.mcp_resource_server()
    }
    fn mcp_instructions(&self) -> Vec<McpServerInstructions> {
        self.native.mcp_instructions()
    }
    async fn list_mcp_resources(
        &self,
        cursor: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.native.list_mcp_resources(cursor, cancel).await
    }
    async fn read_mcp_resource(
        &self,
        uri: &str,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.native.read_mcp_resource(uri, cancel).await
    }
    fn mcp_resource_servers(&self) -> Vec<McpResourceServer> {
        let mut sources = self.native.mcp_resource_servers();
        for (index, server) in self.servers.iter().enumerate() {
            if server
                .state
                .read()
                .unwrap()
                .directory
                .as_ref()
                .is_some_and(|d| d.resources)
            {
                sources.push(McpResourceServer {
                    name: server.descriptor.name.clone(),
                    authority: self.authority(index),
                });
            }
        }
        sources
    }
    async fn list_mcp_resources_from(
        &self,
        server: &str,
        cursor: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let Some(index) = self
            .servers
            .iter()
            .position(|s| s.descriptor.name == server)
        else {
            return self
                .native
                .list_mcp_resources_from(server, cursor, cancel)
                .await;
        };
        let mut result = self
            .peer(index)?
            .request(
                "resources/list",
                cursor.map_or_else(|| json!({}), |c| json!({"cursor":c})),
                cancel,
            )
            .await?;
        if result["resources"].as_array().is_none_or(|r| r.len() > 256)
            || result
                .get("nextCursor")
                .is_some_and(|c| !c.is_null() && c.as_str().is_none_or(|s| s.len() > 4096))
        {
            return Err("invalid session MCP resource directory".into());
        }
        self.sanitize(index, &mut result);
        Ok(result)
    }
    async fn read_mcp_resource_from(
        &self,
        server: &str,
        uri: &str,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let Some(index) = self
            .servers
            .iter()
            .position(|s| s.descriptor.name == server)
        else {
            return self
                .native
                .read_mcp_resource_from(server, uri, cancel)
                .await;
        };
        if uri.len() > 8192 {
            return Err("MCP resource URI too large".into());
        }
        let mut result = self
            .peer(index)?
            .request("resources/read", json!({"uri":uri}), cancel)
            .await?;
        if result["contents"].as_array().is_none_or(|items| {
            items.is_empty()
                || items.len() > 64
                || items.iter().any(|item| {
                    item["uri"] != uri || !(item["text"].is_string() || item["blob"].is_string())
                })
        }) {
            return Err("invalid session MCP resource content".into());
        }
        self.sanitize(index, &mut result);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tickloop_agent_protocol::{EventKind, PermissionExecution, PermissionScope, Receipt};
    use tickloop_agent_test_support::{ScriptedModel, config, text};
    struct Native;
    #[async_trait]
    impl HostToolProvider for Native {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "NativeFixture".into(),
                description: "native".into(),
                input_schema: json!({"type":"object"}),
            }]
        }
        fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }
        async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
            ToolResult {
                content: "native".into(),
                is_error: false,
                output_ref: None,
            }
        }
    }
    #[test]
    fn untrusted_schemas_never_retrieve_external_resources() {
        for reference in [
            "file:///private/no-schema.json",
            "http://127.0.0.1:1/schema",
        ] {
            assert!(schema_validator(&json!({"type":"object","$ref":reference})).is_err());
        }
        assert!(schema_validator(&json!({"type":"object","$defs":{"value":{"type":"string"}},"properties":{"name":{"$ref":"#/$defs/value"}}})).is_ok());
    }
    #[derive(Default)]
    struct Control {
        starts: AtomicUsize,
        closes: AtomicUsize,
        calls: AtomicUsize,
        revision: AtomicUsize,
        no_response: AtomicBool,
        close_fails: AtomicBool,
        resources_only: AtomicBool,
        reject: AtomicBool,
        ping: AtomicBool,
        ping_reply: std::sync::Mutex<Option<Value>>,
        reorder: AtomicBool,
    }
    struct Lifetime {
        control: Arc<Control>,
        cancel: CancellationToken,
        closed: bool,
    }
    #[async_trait]
    impl SessionMcpLifetime for Lifetime {
        async fn close(&mut self) -> Result<(), String> {
            if self.control.close_fails.load(Ordering::SeqCst) {
                return Err("fixture close failed".into());
            }
            if !self.closed {
                self.closed = true;
                self.control.closes.fetch_add(1, Ordering::SeqCst);
                self.cancel.cancel();
            }
            Ok(())
        }
    }
    struct Launcher(Arc<Control>);
    #[async_trait]
    impl SessionMcpLauncher for Launcher {
        async fn launch(
            &self,
            descriptor: &SessionMcpServer,
        ) -> Result<SessionMcpTransport, String> {
            self.0.starts.fetch_add(1, Ordering::SeqCst);
            let tool = descriptor
                .env
                .get("TOOL")
                .cloned()
                .unwrap_or_else(|| "echo".into());
            let (control, client) = (self.0.clone(), tokio::io::duplex(MAX_FRAME * 2));
            let (client_reader, client_writer) = tokio::io::split(client.0);
            let (server_reader, mut server_writer) = tokio::io::split(client.1);
            let cancel = CancellationToken::new();
            let task_cancel = cancel.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(server_reader).lines();
                loop {
                    let frame = tokio::select! {()=task_cancel.cancelled()=>break,line=lines.next_line()=>match line{Ok(Some(line))=>line,_=>break}};
                    let request: Value = serde_json::from_str(&frame).unwrap();
                    if request.get("id").is_none() {
                        continue;
                    }
                    let method = request["method"].as_str().unwrap();
                    if control.reject.load(Ordering::SeqCst)
                        && matches!(method, "tools/call" | "resources/read")
                    {
                        let response = format!(
                            "{}\n",
                            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32602,"message":"Unknown target for private-value","data":{"private":"private-value"}}})
                        );
                        if server_writer.write_all(response.as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    let result = match method {
                        "initialize" => {
                            let capabilities = if control.resources_only.load(Ordering::SeqCst) {
                                json!({"resources":{}})
                            } else {
                                json!({"tools":{},"resources":{}})
                            };
                            json!({"protocolVersion":"2025-11-25","capabilities":capabilities,"serverInfo":{"name":"fixture","version":"1"}})
                        }
                        "tools/list" => {
                            assert!(!control.resources_only.load(Ordering::SeqCst));
                            // The same schema, with its keys in another order on request.
                            let schema = if control.reorder.load(Ordering::SeqCst) {
                                r#"{"properties":{"path":{"type":"string"}},"type":"object"}"#
                            } else {
                                r#"{"type":"object","properties":{"path":{"type":"string"}}}"#
                            };
                            let schema: Value = serde_json::from_str(schema).unwrap();
                            json!({"tools":[{"name":tool,"description":format!("Reads ./ and /fixture/project at debug level, revision {}",control.revision.load(Ordering::SeqCst)),"inputSchema":schema,"annotations":{"readOnlyHint":true,"destructiveHint":false}}]})
                        }
                        "tools/call" => {
                            control.calls.fetch_add(1, Ordering::SeqCst);
                            if control.no_response.load(Ordering::SeqCst) {
                                continue;
                            }
                            if control.ping.load(Ordering::SeqCst) {
                                let ping =
                                    "{\"jsonrpc\":\"2.0\",\"id\":\"ping\",\"method\":\"ping\"}\n";
                                if server_writer.write_all(ping.as_bytes()).await.is_err() {
                                    break;
                                }
                                let Ok(Some(reply)) = lines.next_line().await else {
                                    break;
                                };
                                *control.ping_reply.lock().unwrap() =
                                    Some(serde_json::from_str(&reply).unwrap());
                            }
                            json!({"content":[{"type":"text","text":"private-argument private-value /fixture/project . debug"}]})
                        }
                        "resources/list" => {
                            json!({"resources":[{"uri":"fixture://info","name":"info"}]})
                        }
                        "resources/read" => {
                            json!({"contents":[{"uri":request["params"]["uri"],"text":"information"}]})
                        }
                        _ => panic!("unexpected method"),
                    };
                    let response = format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                    );
                    if server_writer.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
            Ok(SessionMcpTransport {
                reader: Box::new(client_reader),
                writer: Box::new(client_writer),
                lifetime: Box::new(Lifetime {
                    control: self.0.clone(),
                    cancel,
                    closed: false,
                }),
            })
        }
    }
    fn server(directory: &std::path::Path) -> SessionMcpServer {
        SessionMcpServer {
            name: "fixture".into(),
            program: "/bin/cat".into(),
            args: vec![
                "--token=private-argument".into(),
                "-y".into(),
                ".".into(),
                "/fixture/project".into(),
            ],
            env: BTreeMap::from([
                ("EXPLICIT_KEY".into(), "private-value".into()),
                ("LEVEL".into(), "debug".into()),
            ]),
            cwd: directory.into(),
        }
    }
    fn provider(directory: &std::path::Path) -> (Arc<SessionMcpProvider>, Arc<Control>) {
        provider_with(directory, Arc::new(Native))
    }
    fn provider_with(
        directory: &std::path::Path,
        native: Arc<dyn HostToolProvider>,
    ) -> (Arc<SessionMcpProvider>, Arc<Control>) {
        let control = Arc::new(Control::default());
        (
            Arc::new(
                SessionMcpProvider::new(
                    vec![server(directory)],
                    native,
                    Arc::new(Launcher(control.clone())),
                )
                .unwrap(),
            ),
            control,
        )
    }
    async fn start(provider: &SessionMcpProvider) {
        let call = provider.session_bootstrap().unwrap().pop().unwrap();
        let public = format!("{call:?}");
        assert!(!public.contains("private-argument"));
        assert!(!public.contains("private-value"));
        assert_eq!(call.input()["argument_count"], 4);
        provider
            .start_session_server(call, CancellationToken::new())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn starts_only_after_admission_and_keeps_native_tools_and_session_alive() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        assert_eq!(control.starts.load(Ordering::SeqCst), 0);
        assert_eq!(provider.definitions().len(), 1);
        assert_eq!(provider.session_bootstrap().unwrap().len(), 1);
        assert_eq!(control.starts.load(Ordering::SeqCst), 0);
        assert!(provider.upgrade_checkpoint().unwrap().is_none());
        start(&provider).await;
        assert_eq!(control.starts.load(Ordering::SeqCst), 1);
        assert!(provider.session_bootstrap().unwrap().is_empty());
        assert_eq!(provider.definitions().len(), 2);
        assert_eq!(
            provider.permission(Uuid::new_v4(), "mcp__fixture__echo", &json!({})),
            HostToolPermission::Reviewable
        );
        assert!(!provider.is_concurrency_safe("mcp__fixture__echo", &json!({})));
        provider.refresh(CancellationToken::new()).await.unwrap();
        assert_eq!(control.closes.load(Ordering::SeqCst), 0);
        let call = provider
            .prepare_call(
                Uuid::new_v4(),
                "mcp__fixture__echo",
                &json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let result = provider
            .execute_prepared(Uuid::new_v4(), call, CancellationToken::new())
            .await
            .into_tool_result();
        assert!(!result.is_error);
        assert!(!result.content.contains("private-argument"));
        assert!(!result.content.contains("private-value"));
        assert!(
            result
                .content
                .contains("[redacted] [redacted] [redacted] . debug")
        );
        provider.close_session().await.unwrap();
        assert_eq!(control.closes.load(Ordering::SeqCst), 1);
        assert!(provider.upgrade_checkpoint().unwrap().is_some());
    }
    #[test]
    fn launch_values_are_private_above_a_minimum_length() {
        let mut server = server(std::path::Path::new("/"));
        server.args = [
            "run",
            "-y",
            ".",
            "/Users/me/project",
            "acp-argv-value-1234",
            "--secret-token=equals-value-1234",
            "API_SECRET=key-value-1234",
            "--header=Authorization: Bearer bearer-value-1234",
            "postgres://user:password@db.example/name",
            "https://example.com/mcp?access_token=query-value-1234&mode=fast",
            "--password",
            "short",
        ]
        .map(String::from)
        .into();
        let private = private_launch_values(&server);
        // Every long value, and the credential a credential-shaped argument embeds.
        for expected in [
            "private-value",
            "/Users/me/project",
            "acp-argv-value-1234",
            "--secret-token=equals-value-1234",
            "equals-value-1234",
            "key-value-1234",
            "Bearer bearer-value-1234",
            "bearer-value-1234",
            "user:password",
            "query-value-1234",
            "--password",
        ] {
            assert!(private.iter().any(|value| value == expected), "{expected}");
        }
        for visible in ["run", "-y", ".", "short", "debug", "/bin/cat"] {
            assert!(private.iter().all(|value| value != visible), "{visible}");
        }
    }
    #[tokio::test]
    async fn short_launch_values_never_reject_the_directory() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, _) = provider(directory.path());
        start(&provider).await;
        let tool = provider
            .definitions()
            .into_iter()
            .find(|tool| tool.name == "mcp__fixture__echo")
            .unwrap();
        // A long launch value is redacted in place rather than withdrawing the directory.
        assert_eq!(
            tool.description,
            "Reads ./ and [redacted] at debug level, revision 0"
        );
        provider.close_session().await.unwrap();
    }
    #[tokio::test]
    async fn server_errors_complete_the_call_and_keep_the_connection() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        start(&provider).await;
        control.reject.store(true, Ordering::SeqCst);
        assert_eq!(
            provider
                .read_mcp_resource_from("fixture", "fixture://missing", CancellationToken::new())
                .await
                .unwrap_err(),
            "MCP server rejected the request"
        );
        let call = provider
            .prepare_call(
                Uuid::new_v4(),
                "mcp__fixture__echo",
                &json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let HostCallResult::Completed(result) = provider
            .execute_prepared(Uuid::new_v4(), call, CancellationToken::new())
            .await
        else {
            panic!("a server error is a known outcome");
        };
        assert!(result.is_error);
        assert!(result.content.contains("-32602"));
        assert!(result.content.contains("Unknown target for [redacted]"));
        assert!(!result.content.contains("private-value"));
        assert_eq!(control.closes.load(Ordering::SeqCst), 0);
        control.reject.store(false, Ordering::SeqCst);
        let call = provider
            .prepare_call(
                Uuid::new_v4(),
                "mcp__fixture__echo",
                &json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !provider
                .execute_prepared(Uuid::new_v4(), call, CancellationToken::new())
                .await
                .into_tool_result()
                .is_error
        );
        assert_eq!(
            provider
                .read_mcp_resource_from("fixture", "fixture://info", CancellationToken::new())
                .await
                .unwrap()["contents"][0]["text"],
            "information"
        );
        assert_eq!(control.starts.load(Ordering::SeqCst), 1);
        provider.close_session().await.unwrap();
    }
    #[tokio::test]
    async fn server_ping_gets_an_empty_result() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        start(&provider).await;
        control.ping.store(true, Ordering::SeqCst);
        let call = provider
            .prepare_call(
                Uuid::new_v4(),
                "mcp__fixture__echo",
                &json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let result = provider
            .execute_prepared(Uuid::new_v4(), call, CancellationToken::new())
            .await
            .into_tool_result();
        assert!(!result.is_error);
        assert_eq!(
            control.ping_reply.lock().unwrap().clone().unwrap(),
            json!({"jsonrpc":"2.0","id":"ping","result":{}})
        );
        provider.close_session().await.unwrap();
    }
    #[tokio::test]
    async fn tool_names_two_servers_both_produce_are_offered_by_neither() {
        let directory = tempfile::tempdir().unwrap();
        let control = Arc::new(Control::default());
        let server = |name: &str, tool: &str| SessionMcpServer {
            name: name.into(),
            program: "/bin/cat".into(),
            args: Vec::new(),
            env: BTreeMap::from([("TOOL".into(), tool.into())]),
            cwd: directory.path().into(),
        };
        let provider = SessionMcpProvider::new(
            vec![server("a_", "b"), server("a", "_b"), server("c", "echo")],
            Arc::new(Native),
            Arc::new(Launcher(control.clone())),
        )
        .unwrap();
        for call in provider.session_bootstrap().unwrap() {
            provider
                .start_session_server(call, CancellationToken::new())
                .await
                .unwrap();
        }
        assert_eq!(
            provider
                .definitions()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            ["NativeFixture", "mcp__c__echo"]
        );
        assert!(provider.tool("mcp__a___b").is_none());
        assert_eq!(
            provider.permission(Uuid::new_v4(), "mcp__a___b", &json!({})),
            HostToolPermission::ReadOnly,
            "the colliding name is no session tool"
        );
        assert!(provider.tool("mcp__c__echo").is_some());
        provider.close_session().await.unwrap();
    }
    struct Instructed;
    #[async_trait]
    impl HostToolProvider for Instructed {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        fn mcp_instructions(&self) -> Vec<McpServerInstructions> {
            vec![McpServerInstructions {
                server: McpResourceServer {
                    name: "tickloop-artifacts".into(),
                    authority: HostToolAuthority::Mcp {
                        server_id: "artifacts".into(),
                        account_id: "account".into(),
                    },
                },
                text: "Use the Artifacts tools".into(),
            }]
        }
        fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }
        async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
            ToolResult {
                content: "native".into(),
                is_error: false,
                output_ref: None,
            }
        }
    }
    #[tokio::test]
    async fn native_mcp_instructions_survive_the_session_wrapper() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, _) = provider_with(directory.path(), Arc::new(Instructed));
        assert_eq!(provider.mcp_instructions(), Instructed.mcp_instructions());
        start(&provider).await;
        assert_eq!(provider.mcp_instructions(), Instructed.mcp_instructions());
        provider.close_session().await.unwrap();
    }
    #[test]
    fn private_values_are_redacted_in_keys_and_nested_values() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, _) = provider(directory.path());
        let mut value = json!({"private-argument":{"nested":["private-value"]}});
        provider.sanitize(0, &mut value);
        assert_eq!(value, json!({"[redacted]":{"nested":["[redacted]"]}}));
    }
    #[tokio::test]
    async fn resources_only_server_does_not_require_tools_capability() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        control.resources_only.store(true, Ordering::SeqCst);
        start(&provider).await;
        assert_eq!(provider.definitions().len(), 1);
        assert_eq!(provider.mcp_resource_servers().len(), 1);
        provider.refresh(CancellationToken::new()).await.unwrap();
        let result = provider
            .list_mcp_resources_from("fixture", None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result["resources"][0]["uri"], "fixture://info");
        provider.close_session().await.unwrap();
    }
    #[tokio::test]
    async fn directory_revision_change_rejects_old_prepared_call_without_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        start(&provider).await;
        let call = provider
            .prepare_call(
                Uuid::new_v4(),
                "mcp__fixture__echo",
                &json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        control.revision.fetch_add(1, Ordering::SeqCst);
        let result = provider
            .execute_prepared(Uuid::new_v4(), call, CancellationToken::new())
            .await;
        assert!(matches!(result, HostCallResult::NotDispatched(_)));
        assert_eq!(control.calls.load(Ordering::SeqCst), 0);
        provider.close_session().await.unwrap();
    }
    #[tokio::test]
    async fn directory_revision_ignores_the_servers_schema_key_order() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        start(&provider).await;
        let call = provider
            .prepare_call(
                Uuid::new_v4(),
                "mcp__fixture__echo",
                &json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        control.reorder.store(true, Ordering::SeqCst);
        let result = provider
            .execute_prepared(Uuid::new_v4(), call, CancellationToken::new())
            .await;
        assert!(matches!(result, HostCallResult::Completed(_)));
        assert_eq!(control.calls.load(Ordering::SeqCst), 1);
        provider.close_session().await.unwrap();
    }
    #[tokio::test]
    async fn cancellation_after_dispatch_is_unknown_and_closes_inflight_server() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        start(&provider).await;
        let call = provider
            .prepare_call(
                Uuid::new_v4(),
                "mcp__fixture__echo",
                &json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        control.no_response.store(true, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        let task_provider = provider.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            task_provider
                .execute_prepared(Uuid::new_v4(), call, task_cancel)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while control.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert!(matches!(
            task.await.unwrap(),
            HostCallResult::OutcomeUnknown(_)
        ));
        assert_eq!(control.closes.load(Ordering::SeqCst), 1);
        provider.close_session().await.unwrap();
    }
    #[tokio::test]
    async fn failed_close_remains_visible_and_blocks_worker_upgrade() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        start(&provider).await;
        control.close_fails.store(true, Ordering::SeqCst);
        assert!(provider.close_session().await.is_err());
        assert!(provider.upgrade_checkpoint().unwrap().is_none());
        control.close_fails.store(false, Ordering::SeqCst);
        provider.close_session().await.unwrap();
        assert!(provider.upgrade_checkpoint().unwrap().is_some());
    }
    async fn permission(
        agent: &crate::Agent,
        chat: ChatId,
    ) -> tickloop_agent_protocol::PermissionRequest {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(request) = agent.snapshot(chat).await.unwrap().permissions.first() {
                    return request.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
    async fn completed(agent: &crate::Agent, receipt: &Receipt) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = agent.snapshot(receipt.chat_id).await.unwrap();
                if snapshot.events.iter().any(|event| {
                    event.run_id == receipt.run_id
                        && matches!(
                            event.kind,
                            EventKind::Completed { .. } | EventKind::Failed(_)
                        )
                }) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn runtime_bootstrap_is_a_real_run_host_operation_and_once_only() {
        let directory = tempfile::tempdir().unwrap();
        let (provider, control) = provider(directory.path());
        let model = Arc::new(ScriptedModel::new(vec![text("first"), text("second")]));
        let agent = crate::Agent::new_embedded(config(), model.clone());
        agent.install_host_tools(provider.clone()).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        assert_eq!(control.starts.load(Ordering::SeqCst), 0);
        let receipt = agent
            .submit(chat, None, "Use the approved MCP fixture".into())
            .await
            .unwrap();
        // Not even the title request precedes the startup approval.
        assert!(agent.inner.title_requests.lock().await.is_empty());
        let request = permission(&agent, chat).await;
        assert!(
            matches!(request.execution,PermissionExecution::HostOperation{run_id,..} if run_id==receipt.run_id)
        );
        assert!(model.requests.lock().unwrap().is_empty());
        assert!(model.title_requests.lock().unwrap().is_empty());
        assert!(agent.inner.title_requests.lock().await.is_empty());
        assert_eq!(control.starts.load(Ordering::SeqCst), 0);
        assert!(
            agent
                .answer_permission_with_scope(chat, request.id, true, PermissionScope::Session)
                .await
                .is_err()
        );
        agent
            .answer_permission(chat, request.id, true)
            .await
            .unwrap();
        completed(&agent, &receipt).await;
        assert_eq!(control.starts.load(Ordering::SeqCst), 1);
        assert_eq!(control.closes.load(Ordering::SeqCst), 0);
        tokio::time::timeout(Duration::from_secs(3), async {
            while model.title_requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the admitted startup starts the deferred title");
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(!snapshot.messages.iter().flat_map(|m|&m.content).any(|c|matches!(c,tickloop_agent_protocol::Content::ToolUse{name,..} if name==MCP_SERVER_START)));
        let public = serde_json::to_string(&snapshot).unwrap();
        assert!(!public.contains("private-argument"));
        assert!(!public.contains("private-value"));
        let second = agent
            .submit(chat, None, "Continue using the same session".into())
            .await
            .unwrap();
        completed(&agent, &second).await;
        assert_eq!(control.starts.load(Ordering::SeqCst), 1);
        assert_eq!(control.closes.load(Ordering::SeqCst), 0);
        agent.close_session_mcp(chat).await.unwrap();
        assert_eq!(control.closes.load(Ordering::SeqCst), 1);
        agent.shutdown().await;
    }
    #[tokio::test]
    async fn deny_or_policy_revision_change_never_launches_a_server() {
        for change_revision in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let (provider, control) = provider(directory.path());
            let model = Arc::new(ScriptedModel::new(vec![text("must not reach model")]));
            let agent = crate::Agent::new_embedded(config(), model.clone());
            agent.install_host_tools(provider).unwrap();
            let chat = agent.create_chat(directory.path().into()).await.unwrap();
            let receipt = agent
                .submit(chat, None, "Connect MCP".into())
                .await
                .unwrap();
            let request = permission(&agent, chat).await;
            if change_revision {
                let mut chats = agent.inner.chats.lock().await;
                chats.get_mut(&chat).unwrap().snapshot.policy.revision += 1;
            }
            agent
                .answer_permission(chat, request.id, change_revision)
                .await
                .unwrap();
            completed(&agent, &receipt).await;
            assert_eq!(control.starts.load(Ordering::SeqCst), 0);
            assert!(model.requests.lock().unwrap().is_empty());
            assert!(model.title_requests.lock().unwrap().is_empty());
            assert!(agent.inner.title_requests.lock().await.is_empty());
            agent.shutdown().await;
        }
    }
}
