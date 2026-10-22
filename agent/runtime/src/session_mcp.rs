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
        for call in provider.session_bootstrap()? {
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
    PreparedHostCall, WorkspaceHostContext,
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
    ) -> Result<Value, String> {
        if self.broken.load(Ordering::SeqCst) || cancel.is_cancelled() {
            return Err("MCP connection unavailable or cancelled".into());
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
                    // Sampling, roots, elicitation and all other client callbacks are unsupported.
                    if let Some(request_id) = response.get("id") {
                        let mut refusal = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"Client capability not supported"}})).map_err(|_| "invalid MCP callback")?;
                        refusal.push(b'\n');
                        wire.writer
                            .write_all(&refusal)
                            .await
                            .map_err(|_| "MCP callback refusal failed")?;
                        wire.writer
                            .flush()
                            .await
                            .map_err(|_| "MCP callback refusal failed")?;
                    }
                    continue;
                }
                if response["id"] != id {
                    return Err("MCP response identity mismatch".into());
                }
                if response.get("error").is_some() {
                    return Err("MCP server rejected the request".into());
                }
                return response
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "MCP result missing".into());
            }
            Err("MCP notification limit exceeded".into())
        };
        let result = tokio::select! { biased;
            () = cancel.cancelled() => Err("MCP request cancelled".into()),
            result = tokio::time::timeout(REQUEST_TIMEOUT, operation) => result.unwrap_or_else(|_| Err("MCP request timed out".into())),
        };
        if result.is_err() {
            let _ = self.close().await;
        }
        result
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
    tools_supported: bool,
}
struct Server {
    descriptor: SessionMcpServer,
    id: Uuid,
    state: RwLock<ServerState>,
}
#[derive(Default)]
struct ServerState {
    attempted: bool,
    peer: Option<Arc<Peer>>,
    directory: Option<Directory>,
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
    fn tool(&self, name: &str) -> Option<(usize, Directory)> {
        self.servers.iter().enumerate().find_map(|(index, server)| {
            let state = server.state.read().unwrap();
            state
                .directory
                .as_ref()
                .filter(|dir| dir.tools.contains_key(name))
                .cloned()
                .map(|dir| (index, dir))
        })
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
        let server = &self.servers[index].descriptor;
        let mut secrets = server
            .args
            .iter()
            .chain(server.env.values())
            .map(String::as_str)
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
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
                tools_supported,
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
                let schema = tool
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
                let description = tool["description"].as_str().unwrap_or(original);
                if description.len() > 16 * 1024 {
                    return Err("session MCP description too large".into());
                }
                // Keep private launch values out of all model-visible directory metadata.
                let mut public_tool = tool.clone();
                self.sanitize(index, &mut public_tool);
                if public_tool != *tool {
                    return Err("session MCP directory includes private launch data".into());
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
                let revision = Sha256::digest(
                    serde_json::to_vec(&tools).map_err(|_| "invalid MCP directory")?,
                )
                .into();
                return Ok(Directory {
                    revision,
                    tools,
                    originals,
                    resources,
                    tools_supported,
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
            .directory
            .as_ref()
            .map(|d| (d.tools_supported, d.resources))
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
        for server in &self.servers {
            if let Some(dir) = &server.state.read().unwrap().directory {
                tools.extend(dir.tools.values().cloned());
            }
        }
        tools
    }
    fn session_bootstrap(&self) -> Result<Vec<PreparedHostCall>, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let mut starts = Vec::new();
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
        let start = call
            .into_payload::<PreparedStart>()
            .map_err(|_| "invalid session MCP startup payload")?;
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
            Err(_)=>HostCallResult::OutcomeUnknown("session MCP response unavailable after dispatch; do not automatically repeat the call".into()),
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
        async fn launch(&self, _: &SessionMcpServer) -> Result<SessionMcpTransport, String> {
            self.0.starts.fetch_add(1, Ordering::SeqCst);
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
                    let result = match request["method"].as_str().unwrap() {
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
                            json!({"tools":[{"name":"echo","description":format!("revision {}",control.revision.load(Ordering::SeqCst)),"inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true,"destructiveHint":false}}]})
                        }
                        "tools/call" => {
                            control.calls.fetch_add(1, Ordering::SeqCst);
                            if control.no_response.load(Ordering::SeqCst) {
                                continue;
                            }
                            json!({"content":[{"type":"text","text":"private-argument private-value"}]})
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
    fn provider(directory: &std::path::Path) -> (Arc<SessionMcpProvider>, Arc<Control>) {
        let control = Arc::new(Control::default());
        let server = SessionMcpServer {
            name: "fixture".into(),
            program: "/bin/cat".into(),
            args: vec!["private-argument".into()],
            env: BTreeMap::from([("EXPLICIT_KEY".into(), "private-value".into())]),
            cwd: directory.into(),
        };
        (
            Arc::new(
                SessionMcpProvider::new(
                    vec![server],
                    Arc::new(Native),
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
        assert_eq!(call.input()["argument_count"], 1);
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
        assert!(result.content.contains("[redacted]"));
        provider.close_session().await.unwrap();
        assert_eq!(control.closes.load(Ordering::SeqCst), 1);
        assert!(provider.upgrade_checkpoint().unwrap().is_some());
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
        let request = permission(&agent, chat).await;
        assert!(
            matches!(request.execution,PermissionExecution::HostOperation{run_id,..} if run_id==receipt.run_id)
        );
        assert!(model.requests.lock().unwrap().is_empty());
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
            agent.shutdown().await;
        }
    }
}
