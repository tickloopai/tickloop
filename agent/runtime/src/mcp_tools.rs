//! The Tick Loop Artifacts MCP connection `tickloop-docs`, whose three tools the host presents as
//! first-party built-ins, backed by service-scoped credentials.

use std::{
    collections::{BTreeSet, HashSet},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::{CallToolRequestParams, ClientConfig, ProtocolVersion, Tool},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    host_tools::{
        EventToolContext, HostCallResult, HostEffect, HostToolPermission, HostToolProvider,
        PreparedHostCall, WorkspaceHostContext,
    },
    mcp_http::McpHttpClient,
};
use tickloop_agent_artifacts::{ArtifactClient, ArtifactError, CredentialSnapshot, mcp_endpoint};
use tickloop_agent_protocol::{
    ArtifactAudience, ArtifactPublication, ChatId, HostCallContext, HostEndpointIdentity,
    HostToolAuthority, ToolDefinition, ToolResult,
};

pub const ARTIFACT_READ_TOOL: &str = "ArtifactRead";
pub const ARTIFACT_EDIT_TOOL: &str = "ArtifactEdit";
pub const ARTIFACT_COMMENT_TOOL: &str = "ArtifactComment";
/// Offered instead of the Artifact tools while no account is signed in.
pub const ARTIFACT_LOGIN_TOOL: &str = "ArtifactLogin";
/// The only tools taken from the service directory; the host reserves these names.
pub const ARTIFACT_TOOL_NAMES: [&str; 3] = [
    ARTIFACT_READ_TOOL,
    ARTIFACT_EDIT_TOOL,
    ARTIFACT_COMMENT_TOOL,
];

const SERVER_NAME: &str = "tickloop-docs";
const EVENT_CONTEXT_META: &str = "tickloop.ai/event-bridge";
const PUBLICATION_META: &str = "tickloop.ai/publication";
const MAX_TOOLS: usize = 128;
const DIRECTORY_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_APPROVAL_PREVIEW_BYTES: usize = 160;
const MAX_APPROVAL_INPUT_BYTES: usize = 32 * 1024;
const MAX_APPROVAL_COLLECTION_ITEMS: usize = 64;
const MAX_APPROVAL_DEPTH: usize = 8;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_ARTIFACT_TITLE_UTF16_UNITS: usize = 200;
const ARTIFACT_LOGIN_DESCRIPTION: &str = "Tick Loop Artifacts is not signed in. Call this when the user wants a doc or design; it starts sign-in and returns a link and code for the user. After the user approves, ArtifactRead, ArtifactEdit and ArtifactComment become available automatically.";

#[derive(Clone)]
struct DiscoveredTool {
    definition: ToolDefinition,
    permission: HostToolPermission,
}

struct Directory {
    credential_key: [u8; 32],
    fingerprint: [u8; 32],
    protocol_version: ProtocolVersion,
    tools: Vec<DiscoveredTool>,
}

struct PreparedArtifactLogin;

/// Why the tool directory did not load: the service could not be reached, or it answered with a
/// directory the host does not accept.
enum DirectoryFailure {
    Unavailable(String),
    Rejected(String),
}

impl From<DirectoryFailure> for String {
    fn from(failure: DirectoryFailure) -> Self {
        match failure {
            DirectoryFailure::Unavailable(reason) | DirectoryFailure::Rejected(reason) => reason,
        }
    }
}

/// The one device sign-in the provider polls for; a repeated `ArtifactLogin` returns it.
struct PendingLogin {
    id: u64,
    verification_uri: String,
    user_code: String,
    expires_at: tokio::time::Instant,
    cancel: CancellationToken,
}

impl PendingLogin {
    fn instructions(&self, heading: &str) -> String {
        let minutes = self
            .expires_at
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs()
            .div_ceil(60);
        format!(
            "{heading} Give the user this link and code: open {} and confirm the code {}. The code expires in about {minutes} minutes. Once the user approves, ArtifactRead, ArtifactEdit and ArtifactComment become available automatically on a later request; the user can also sign in by running /artifacts.",
            self.verification_uri, self.user_code
        )
    }
}

struct PreparedMcpCall {
    credential: CredentialSnapshot,
    fingerprint: [u8; 32],
    arguments: Value,
    publication: Option<ArtifactPublication>,
    read_owned: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ArtifactPublicationAdmission {
    chat: ChatId,
    account: String,
    origin: String,
    artifact: String,
    audience: ArtifactAudience,
}

fn artifact_update_id<'a>(name: &str, input: &'a Value) -> Option<&'a str> {
    if name != ARTIFACT_EDIT_TOOL {
        return None;
    }
    let fields = input.as_object()?;
    let valid_fields = match input["action"].as_str()? {
        "patch" => fields.len() == 3 && input["patches"].is_array(),
        "replace" => {
            fields.len() == 4 && input["expected_revision"].is_u64() && input["content"].is_string()
        }
        _ => false,
    };
    valid_fields
        .then(|| bounded_identifier(input.get("artifact_id")))
        .flatten()
}

fn artifact_read_id<'a>(name: &str, input: &'a Value) -> Option<&'a str> {
    (name == ARTIFACT_READ_TOOL && matches!(input["action"].as_str(), Some("read" | "threads")))
        .then(|| bounded_identifier(input.get("artifact_id")))
        .flatten()
}

fn bounded_identifier(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES)
}

fn has_action(arguments: &Value, action: &str) -> bool {
    arguments.get("action").and_then(Value::as_str) == Some(action)
}

#[derive(Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactAuthentication {
    SignedIn,
    SignedOut,
    #[default]
    Unknown,
}
#[derive(Clone, Copy, Default, Serialize)]
#[serde(rename_all = "snake_case")]
enum McpToolDirectory {
    Available,
    #[default]
    Unavailable,
}
#[derive(Clone, Default, Serialize)]
struct ArtifactServiceStatus {
    #[serde(skip)]
    credential_generation: Option<[u8; 32]>,
    #[serde(skip)]
    directory_rejected: bool,
    authentication: ArtifactAuthentication,
    tool_directory: McpToolDirectory,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
}

pub struct McpHostToolProvider {
    artifact: Arc<ArtifactClient>,
    native: Option<Arc<dyn HostToolProvider>>,
    directory: RwLock<Option<Directory>>,
    service_status: RwLock<ArtifactServiceStatus>,
    publication_admissions: RwLock<HashSet<ArtifactPublicationAdmission>>,
    pending_login: Arc<Mutex<Option<PendingLogin>>>,
    next_login_id: std::sync::atomic::AtomicU64,
}

impl Drop for McpHostToolProvider {
    fn drop(&mut self) {
        self.cancel_pending_login();
    }
}

impl McpHostToolProvider {
    async fn artifact_read_is_owned(
        &self,
        id: &str,
        credential: &CredentialSnapshot,
        cancel: CancellationToken,
    ) -> Result<bool, String> {
        tokio::select! {
            () = cancel.cancelled() => Err("MCP call cancelled".into()),
            result = self.artifact.read_with_credential(id, credential) => match result {
                Ok(metadata) if metadata.id != id => Err("Artifact metadata identity does not match the requested artifact".into()),
                Ok(metadata) => Ok(metadata.access_role == "owner"),
                Err(ArtifactError::Connection | ArtifactError::InvalidResponse
                    | ArtifactError::Service { status: 500..=599, .. }
                    | ArtifactError::Http(500..=599)) => Ok(false),
                Err(_) => Err("Artifact access could not be verified for this account".into()),
            }
        }
    }

    fn operation_permission(tool: &DiscoveredTool, input: &Value) -> HostToolPermission {
        let ordinary_creation = tool.definition.name == ARTIFACT_EDIT_TOOL
            && has_action(input, "create")
            && input.as_object().is_some_and(|fields| {
                fields.len() == 4
                    && fields.keys().all(|key| {
                        matches!(key.as_str(), "action" | "title" | "format" | "content")
                    })
            })
            && input["title"].as_str().is_some_and(|title| {
                !title.trim().is_empty()
                    && title.encode_utf16().count() <= MAX_ARTIFACT_TITLE_UTF16_UNITS
            })
            && matches!(input["format"].as_str(), Some("doc" | "design"))
            && input["content"].is_string();
        if ordinary_creation {
            HostToolPermission::PrivateArtifactWrite
        } else {
            tool.permission
        }
    }
    pub fn new(artifact: ArtifactClient, native: Option<Arc<dyn HostToolProvider>>) -> Self {
        Self {
            artifact: Arc::new(artifact),
            native,
            directory: RwLock::new(None),
            service_status: RwLock::new(ArtifactServiceStatus::default()),
            publication_admissions: RwLock::new(HashSet::new()),
            pending_login: Arc::new(Mutex::new(None)),
            next_login_id: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Sign-in is offered only while the last refresh confirmed that no account is signed in.
    fn offers_login(&self) -> bool {
        self.service_status.read().unwrap().authentication == ArtifactAuthentication::SignedOut
            && matches!(self.artifact.credential_snapshot(), Ok(None))
    }

    fn cancel_pending_login(&self) {
        if let Some(pending) = self.pending_login.lock().unwrap().take() {
            pending.cancel.cancel();
        }
    }

    fn current_pending_login(&self) -> Option<String> {
        self.pending_login
            .lock()
            .unwrap()
            .as_ref()
            .filter(|pending| pending.expires_at > tokio::time::Instant::now())
            .map(|pending| pending.instructions("Sign-in is already pending."))
    }

    /// Starts a device sign-in and polls for its approval in the background; the credential it
    /// saves is the one `/artifacts` writes, so the next refresh offers the Artifact tools.
    async fn start_login(&self, cancel: CancellationToken) -> ToolResult {
        let result = |content: String, is_error: bool| ToolResult {
            content,
            is_error,
            output_ref: None,
        };
        if let Some(instructions) = self.current_pending_login() {
            return result(instructions, false);
        }
        let device = tokio::select! {
            () = cancel.cancelled() => return result("Sign-in was cancelled before it started".into(), true),
            device = self.artifact.start_device_login() => device,
        };
        let device = match device {
            Ok(device) => device,
            Err(error) => {
                return result(
                    format!(
                        "Tick Loop Artifacts sign-in could not start: {error}. The user can run /artifacts to sign in."
                    ),
                    true,
                );
            }
        };
        let mut pending_login = self.pending_login.lock().unwrap();
        if let Some(pending) = pending_login
            .as_ref()
            .filter(|pending| pending.expires_at > tokio::time::Instant::now())
        {
            return result(pending.instructions("Sign-in is already pending."), false);
        }
        let pending = PendingLogin {
            id: self
                .next_login_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            verification_uri: device.verification_uri.clone(),
            user_code: device.user_code.clone(),
            expires_at: tokio::time::Instant::now() + device.expires_in(),
            cancel: CancellationToken::new(),
        };
        let instructions = pending.instructions("Sign-in started.");
        let (id, poll_cancel) = (pending.id, pending.cancel.clone());
        if let Some(replaced) = pending_login.replace(pending) {
            replaced.cancel.cancel();
        }
        drop(pending_login);
        let artifact = self.artifact.clone();
        let pending_login = self.pending_login.clone();
        tokio::spawn(async move {
            let _ = artifact
                .finish_background_device_login(device, &poll_cancel)
                .await;
            let mut pending_login = pending_login.lock().unwrap();
            if pending_login
                .as_ref()
                .is_some_and(|pending| pending.id == id)
            {
                *pending_login = None;
            }
        });
        result(instructions, false)
    }

    pub fn negotiated_version(&self) -> Option<String> {
        self.directory
            .read()
            .unwrap()
            .as_ref()
            .map(|directory| directory.protocol_version.to_string())
    }

    fn clear_directory(&self) {
        *self.directory.write().unwrap() = None;
    }

    fn credential_key(snapshot: &CredentialSnapshot) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(snapshot.origin.as_bytes());
        digest.update([0]);
        digest.update(snapshot.account_id.as_bytes());
        digest.update([0]);
        digest.update(snapshot.access_token.as_bytes());
        digest.finalize().into()
    }

    fn credential_is_current(&self, expected: &CredentialSnapshot) -> bool {
        self.artifact
            .credential_snapshot()
            .ok()
            .flatten()
            .is_some_and(|current| expected.identity_matches(&current))
    }

    fn is_mcp_name(&self, name: &str) -> bool {
        self.directory_tool(name).is_some()
    }

    /// The Artifact account the MCP tools act for.
    fn mcp_authority(&self) -> Result<Option<HostToolAuthority>, String> {
        Ok(self
            .artifact
            .credential_snapshot()
            .map_err(|_| "Artifact credential unavailable")?
            .map(|credential| HostToolAuthority::Mcp {
                server_id: credential.origin,
                account_id: credential.account_id,
            }))
    }

    fn directory_tool(&self, name: &str) -> Option<(DiscoveredTool, [u8; 32], [u8; 32])> {
        let directory = self.directory.read().unwrap();
        let directory = directory.as_ref()?;
        let tool = directory
            .tools
            .iter()
            .find(|tool| tool.definition.name == name)?
            .clone();
        Some((tool, directory.fingerprint, directory.credential_key))
    }

    async fn open_session(
        &self,
        credential: &CredentialSnapshot,
    ) -> Result<(RunningService<RoleClient, ClientConfig>, McpHttpClient), String> {
        let endpoint = reqwest::Url::parse(&mcp_endpoint(&credential.origin))
            .map_err(|_| "invalid MCP endpoint")?;
        let http = McpHttpClient::new(endpoint.clone(), credential.access_token.clone())?;
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint.as_str().to_owned())
            .auth_header(credential.access_token.clone())
            .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(http.clone(), config);
        match tokio::time::timeout(
            DIRECTORY_TIMEOUT,
            ClientConfig::default().serve_with_lifecycle(
                transport,
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![
                        ProtocolVersion::V_2026_07_28,
                        ProtocolVersion::V_2025_11_25,
                    ],
                },
            ),
        )
        .await
        {
            Ok(Ok(service)) => Ok((service, http)),
            _ => {
                if http.unauthorized() {
                    self.invalidate(&credential.access_token).await;
                }
                Err("MCP connection unavailable".into())
            }
        }
    }

    async fn load_directory(
        &self,
        credential: &CredentialSnapshot,
    ) -> Result<Directory, DirectoryFailure> {
        let (service, http) = self
            .open_session(credential)
            .await
            .map_err(DirectoryFailure::Unavailable)?;
        let peer = service.peer().peer_info().ok_or_else(|| {
            DirectoryFailure::Unavailable(
                "MCP discovery did not negotiate a protocol version".into(),
            )
        })?;
        if peer.server_info.as_ref().map(|info| info.name.as_str()) != Some(SERVER_NAME) {
            return Err(DirectoryFailure::Rejected(format!(
                "MCP server is not {SERVER_NAME}"
            )));
        }
        let protocol_version = peer.protocol_version.clone();
        let tools =
            match tokio::time::timeout(DIRECTORY_TIMEOUT, service.peer().list_all_tools()).await {
                Ok(Ok(tools)) => tools,
                _ => {
                    if http.unauthorized() {
                        self.invalidate(&credential.access_token).await;
                    }
                    return Err(DirectoryFailure::Unavailable(
                        "MCP tool directory unavailable".into(),
                    ));
                }
            };
        self.validate_directory(tools, credential, protocol_version)
            .map_err(DirectoryFailure::Rejected)
    }

    async fn invalidate(&self, token: &str) {
        let _ = self.artifact.invalidate_credential(token).await;
        self.clear_directory();
    }

    /// Keeps exactly the fixed Artifact tools; anything else the service lists is ignored.
    fn validate_directory(
        &self,
        tools: Vec<Tool>,
        credential: &CredentialSnapshot,
        protocol_version: ProtocolVersion,
    ) -> Result<Directory, String> {
        if tools.len() > MAX_TOOLS {
            return Err("MCP tool directory is too large".into());
        }
        let mut accepted = tools
            .into_iter()
            .filter(|tool| ARTIFACT_TOOL_NAMES.contains(&tool.name.as_ref()))
            .collect::<Vec<_>>();
        accepted.sort_by(|left, right| left.name.cmp(&right.name));
        let mut names = HashSet::new();
        let mut mapped = Vec::with_capacity(accepted.len());
        for tool in &accepted {
            let name = tool.name.as_ref();
            if !names.insert(name) {
                return Err("MCP tool directory contains a duplicate name".into());
            }
            let schema = Value::Object(tool.input_schema.as_ref().clone());
            if schema["type"] != "object" || jsonschema::validator_for(&schema).is_err() {
                return Err("MCP tool directory contains an incompatible input schema".into());
            }
            let permission = if name == ARTIFACT_READ_TOOL {
                HostToolPermission::ReadOnly
            } else {
                HostToolPermission::Reviewable
            };
            mapped.push(DiscoveredTool {
                definition: ToolDefinition {
                    name: name.to_owned(),
                    description: tool
                        .description
                        .as_ref()
                        .map_or_else(|| name.to_owned(), ToString::to_string),
                    input_schema: schema,
                },
                permission,
            });
        }
        if names.len() != ARTIFACT_TOOL_NAMES.len() {
            return Err("MCP tool directory lacks an Artifact tool".into());
        }
        let serialized = serde_json::to_vec(&accepted)
            .map_err(|_| "MCP tool directory cannot be fingerprinted")?;
        Ok(Directory {
            credential_key: Self::credential_key(credential),
            fingerprint: Sha256::digest(serialized).into(),
            protocol_version,
            tools: mapped,
        })
    }

    /// The artifact a confirmed `ArtifactEdit` create made, as a Tick of the calling account.
    fn creation_effect(
        result: &rmcp::model::CallToolResult,
        prepared: &PreparedMcpCall,
    ) -> Option<tickloop_agent_protocol::ArtifactTick> {
        if result.is_error == Some(true) || !has_action(&prepared.arguments, "create") {
            return None;
        }
        let structured = result.structured_content.as_ref()?;
        let artifact_id = bounded_identifier(structured.get("artifact_id"))?;
        if artifact_id.contains(['/', '?', '#']) {
            return None;
        }
        let title = structured.get("title")?.as_str()?;
        // The tool contract names types by intent; the catalog keeps its stored format names.
        let format = match structured.get("format")?.as_str()? {
            "doc" => "markdown",
            "design" => "html",
            _ => return None,
        };
        let url = structured.get("url")?.as_str()?;
        let parsed_url = reqwest::Url::parse(url).ok()?;
        let origin = reqwest::Url::parse(&prepared.credential.origin).ok()?;
        if parsed_url.origin() != origin.origin() {
            return None;
        }
        Some(tickloop_agent_protocol::ArtifactTick {
            artifact_id: artifact_id.into(),
            origin: prepared.credential.origin.clone(),
            title: title.into(),
            format: format.into(),
            url: url.into(),
            access_role: "owner".into(),
            added_at_ms: tickloop_agent_base::private_fs::unix_time_milliseconds(),
        })
    }

    /// A confirmed `ArtifactComment` done for the request this run claimed.
    fn finished_request_effect(
        result: &rmcp::model::CallToolResult,
        arguments: &Value,
        context: &EventToolContext,
    ) -> Option<HostEffect> {
        if result.is_error == Some(true) || !has_action(arguments, "done") {
            return None;
        }
        let request_id = bounded_identifier(result.structured_content.as_ref()?.get("request_id"))?;
        (request_id == context.request_id).then(|| HostEffect::FinishedFeedbackRequest {
            request_id: request_id.to_owned(),
        })
    }

    /// Serializes a result for the model and transcript with the bearer token redacted.
    fn model_result(result: rmcp::model::CallToolResult, access_token: &str) -> ToolResult {
        let content = serde_json::to_string(&result)
            .unwrap_or_else(|_| "MCP result could not be displayed".into())
            .replace(access_token, "[redacted]");
        ToolResult {
            content,
            is_error: result.is_error == Some(true),
            output_ref: None,
        }
    }

    fn approval_input(input: &Value) -> Result<Value, String> {
        let review = Self::approval_value(input, None, 0);
        let bytes =
            serde_json::to_vec(&review).map_err(|_| "MCP approval input cannot be serialized")?;
        if bytes.len() > MAX_APPROVAL_INPUT_BYTES {
            return Err("MCP arguments are too large to review safely".into());
        }
        Ok(review)
    }

    fn approval_value(value: &Value, name: Option<&str>, depth: usize) -> Value {
        if name.is_some_and(Self::is_sensitive_argument) {
            return Self::approval_digest(value, false);
        }
        match value {
            Value::String(content) if name.is_some_and(Self::is_body_argument) => {
                Self::approval_digest(value, content.len() > MAX_APPROVAL_PREVIEW_BYTES)
            }
            Value::String(content) if content.len() > MAX_APPROVAL_PREVIEW_BYTES => {
                Self::approval_digest(value, true)
            }
            Value::Object(fields)
                if depth >= MAX_APPROVAL_DEPTH || fields.len() > MAX_APPROVAL_COLLECTION_ITEMS =>
            {
                Self::approval_digest(value, false)
            }
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            Self::approval_value(value, Some(key), depth + 1),
                        )
                    })
                    .collect(),
            ),
            Value::Array(items)
                if depth >= MAX_APPROVAL_DEPTH || items.len() > MAX_APPROVAL_COLLECTION_ITEMS =>
            {
                Self::approval_digest(value, false)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| Self::approval_value(item, name, depth + 1))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }

    fn is_sensitive_argument(name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        name.contains("token")
            || name.contains("secret")
            || name.contains("password")
            || name.contains("credential")
            || name.contains("authorization")
            || name.contains("cookie")
            || name.contains("api_key")
            || name.contains("private_key")
    }

    fn is_body_argument(name: &str) -> bool {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "source" | "body" | "content" | "text"
        )
    }

    fn approval_digest(value: &Value, include_preview: bool) -> Value {
        let serialized;
        let bytes = if let Some(content) = value.as_str() {
            content.as_bytes()
        } else {
            serialized = serde_json::to_vec(value).expect("valid MCP arguments serialize");
            &serialized
        };
        let mut summary = json!({
            "bytes": bytes.len(),
            "sha256": format!("{:x}", Sha256::digest(bytes)),
            "truncated": true,
        });
        if include_preview && let Some(content) = value.as_str() {
            let end = content.floor_char_boundary(MAX_APPROVAL_PREVIEW_BYTES.min(content.len()));
            summary["preview"] = Value::String(content[..end].to_owned());
        }
        summary
    }
}

#[async_trait]
impl HostToolProvider for McpHostToolProvider {
    /// Native tools never take the Artifact names; only the service directory provides them, and
    /// the provider itself offers sign-in while signed out.
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .native
            .as_ref()
            .map(|provider| provider.definitions())
            .unwrap_or_default();
        definitions.retain(|definition| {
            !ARTIFACT_TOOL_NAMES.contains(&definition.name.as_str())
                && definition.name != ARTIFACT_LOGIN_TOOL
        });
        if let Some(directory) = self.directory.read().unwrap().as_ref() {
            definitions.extend(directory.tools.iter().map(|tool| tool.definition.clone()));
        } else if self.offers_login() {
            definitions.push(ToolDefinition {
                name: ARTIFACT_LOGIN_TOOL.into(),
                description: ARTIFACT_LOGIN_DESCRIPTION.into(),
                input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            });
        }
        definitions
    }

    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        self.native
            .as_ref()
            .map_or(Ok(None), |native| native.host_endpoint())
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        self.native
            .as_ref()
            .and_then(|native| native.workspace_host_context())
    }

    /// The Artifact names belong to the signed-in account even while its directory is unavailable.
    fn tool_authorities(&self, name: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        if self.is_mcp_name(name) || ARTIFACT_TOOL_NAMES.contains(&name) {
            return Ok(self.mcp_authority()?.into_iter().collect());
        }
        if name == ARTIFACT_LOGIN_TOOL {
            return Ok(BTreeSet::new());
        }
        self.native
            .as_ref()
            .map_or(Ok(BTreeSet::new()), |native| native.tool_authorities(name))
    }

    fn call_authority(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<Option<HostToolAuthority>, String> {
        if self.is_mcp_name(name) {
            return self.mcp_authority();
        }
        if name == ARTIFACT_LOGIN_TOOL {
            return Ok(None);
        }
        self.native
            .as_ref()
            .map_or(Ok(None), |native| native.call_authority(name, input))
    }

    fn service_connections(&self) -> Value {
        let mut connections = self
            .native
            .as_ref()
            .map(|native| native.service_connections())
            .unwrap_or_else(|| json!({}));
        let status = self.service_status.read().unwrap();
        let generation = self
            .artifact
            .credential_snapshot()
            .map(|credential| credential.as_ref().map(Self::credential_key));
        connections["artifacts"] =
            if generation.is_ok_and(|generation| generation == status.credential_generation) {
                json!(*status)
            } else {
                json!(ArtifactServiceStatus::default())
            };
        connections
    }

    /// Confirmed when signed out or when the signed-in account's directory loaded or was rejected;
    /// an unknown account or an unreachable directory may be a passing outage.
    fn tool_set_is_confirmed(&self) -> bool {
        let is_native_confirmed = self
            .native
            .as_ref()
            .is_none_or(|native| native.tool_set_is_confirmed());
        is_native_confirmed
            && (self.offers_login()
                || self.directory.read().unwrap().is_some()
                || self.service_status.read().unwrap().directory_rejected)
    }

    fn artifact_client(&self) -> Option<Arc<ArtifactClient>> {
        Some(self.artifact.clone())
    }

    async fn refresh(&self, cancel: CancellationToken) -> Result<(), String> {
        if let Some(native) = &self.native {
            native.refresh(cancel.clone()).await?;
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        let signed_out = || ArtifactServiceStatus {
            authentication: ArtifactAuthentication::SignedOut,
            ..Default::default()
        };
        let checked_generation = self
            .artifact
            .credential_snapshot()
            .ok()
            .flatten()
            .as_ref()
            .map(Self::credential_key);
        let (mut status, credential) = match self.artifact.status().await {
            Ok(Some(user)) => match self.artifact.credential_snapshot() {
                Ok(Some(credential))
                    if credential.account_id == user.id
                        && Some(Self::credential_key(&credential)) == checked_generation =>
                {
                    (
                        ArtifactServiceStatus {
                            authentication: ArtifactAuthentication::SignedIn,
                            credential_generation: Some(Self::credential_key(&credential)),
                            account_id: Some(user.id),
                            email: Some(user.email),
                            ..Default::default()
                        },
                        Some(credential),
                    )
                }
                Ok(None) => (signed_out(), None),
                _ => (ArtifactServiceStatus::default(), None),
            },
            Ok(None) => (signed_out(), None),
            Err(_) => (ArtifactServiceStatus::default(), None),
        };
        if let Some(credential) = credential {
            let directory = self.load_directory(&credential).await;
            match self.artifact.credential_snapshot() {
                Ok(Some(current))
                    if Self::credential_key(&current) == Self::credential_key(&credential) =>
                {
                    match directory {
                        Ok(directory) => {
                            status.tool_directory = McpToolDirectory::Available;
                            *self.directory.write().unwrap() = Some(directory);
                        }
                        Err(failure) => {
                            status.directory_rejected =
                                matches!(failure, DirectoryFailure::Rejected(_));
                            self.clear_directory();
                        }
                    }
                }
                Ok(None) => {
                    self.clear_directory();
                    status = signed_out();
                }
                _ => {
                    self.clear_directory();
                    status = ArtifactServiceStatus::default();
                }
            }
        } else {
            self.clear_directory();
        }
        if !self
            .artifact
            .credential_snapshot()
            .map(|credential| {
                credential.as_ref().map(Self::credential_key) == status.credential_generation
            })
            .unwrap_or(false)
        {
            self.clear_directory();
            status = ArtifactServiceStatus::default();
        }
        if status.authentication == ArtifactAuthentication::SignedIn {
            self.cancel_pending_login();
        }
        *self.service_status.write().unwrap() = status;
        Ok(())
    }

    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        if name == ARTIFACT_LOGIN_TOOL {
            return input
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
                .then_some(())
                .ok_or_else(|| format!("{ARTIFACT_LOGIN_TOOL} takes no input"));
        }
        if !self.is_mcp_name(name) {
            return self
                .native
                .as_ref()
                .ok_or_else(|| "Host tool unavailable".to_owned())?
                .validate_input(name, schema, input);
        }
        let validator =
            jsonschema::validator_for(schema).map_err(|_| format!("invalid {name} schema"))?;
        validator
            .validate(input)
            .map_err(|_| format!("invalid {name} input"))
    }

    fn permission(&self, chat_id: ChatId, name: &str, input: &Value) -> HostToolPermission {
        if let Some((tool, _, _)) = self.directory_tool(name) {
            return Self::operation_permission(&tool, input);
        }
        if name == ARTIFACT_LOGIN_TOOL {
            return HostToolPermission::Automatic;
        }
        self.native
            .as_ref()
            .map_or(HostToolPermission::Reviewable, |native| {
                native.permission(chat_id, name, input)
            })
    }

    async fn prepare_call_in_context(
        &self,
        chat_id: ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if self.is_mcp_name(name) || name == ARTIFACT_LOGIN_TOOL {
            self.prepare_call(chat_id, name, input, cancel).await
        } else {
            self.native
                .as_ref()
                .ok_or_else(|| "Host tool unavailable".to_owned())?
                .prepare_call_in_context(chat_id, context, name, input, cancel)
                .await
        }
    }

    async fn prepare_call(
        &self,
        chat_id: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if name == ARTIFACT_LOGIN_TOOL {
            if cancel.is_cancelled() {
                return Err("Sign-in was cancelled".into());
            }
            if !self.offers_login() {
                return Err("Tick Loop Artifacts is not signed out; sign-in is unavailable".into());
            }
            return Ok(PreparedHostCall::new(
                name.to_owned(),
                input.clone(),
                input.clone(),
                HostToolPermission::Automatic,
                format!(
                    "Start Tick Loop Artifacts sign-in at {}",
                    self.artifact.origin()
                ),
            )
            .with_payload(PreparedArtifactLogin));
        }
        let Some((tool, fingerprint, expected_key)) = self.directory_tool(name) else {
            return self
                .native
                .as_ref()
                .ok_or_else(|| "Host tool unavailable".to_owned())?
                .prepare_call(chat_id, name, input, cancel)
                .await;
        };
        if cancel.is_cancelled() {
            return Err("MCP call cancelled".into());
        }
        let credential = self
            .artifact
            .credential_snapshot()
            .map_err(|_| "Artifact credential unavailable")?
            .ok_or("Artifact login is no longer active")?;
        if Self::credential_key(&credential) != expected_key {
            return Err("Artifact account changed before MCP preparation".into());
        }
        let read_owned = if let Some(id) = artifact_read_id(name, input) {
            Some(
                self.artifact_read_is_owned(id, &credential, cancel.clone())
                    .await?,
            )
        } else {
            None
        };
        let publication = if let Some(id) = artifact_update_id(name, input) {
            tokio::select! {
                () = cancel.cancelled() => return Err("MCP call cancelled".into()),
                result = self.artifact.read_with_credential(id, &credential) => {
                    result.map_err(|error| format!("Artifact publication state unavailable: {error}"))?.publication
                }
            }
        } else {
            None
        };
        let permission = if let Some(owned) = read_owned {
            if owned {
                HostToolPermission::ReadOnly
            } else {
                HostToolPermission::Confirmation
            }
        } else {
            match publication.as_ref().map(|state| state.audience) {
                Some(audience @ (ArtifactAudience::Private | ArtifactAudience::Public)) => {
                    let admission = ArtifactPublicationAdmission {
                        chat: chat_id,
                        account: credential.account_id.clone(),
                        origin: credential.origin.clone(),
                        artifact: artifact_update_id(name, input)
                            .expect("publication belongs to an update")
                            .to_owned(),
                        audience,
                    };
                    if self
                        .publication_admissions
                        .read()
                        .unwrap()
                        .contains(&admission)
                    {
                        HostToolPermission::ArtifactContinuation
                    } else if audience == ArtifactAudience::Private {
                        HostToolPermission::PrivateArtifactWrite
                    } else {
                        HostToolPermission::PublicArtifactWrite
                    }
                }
                _ => Self::operation_permission(&tool, input),
            }
        };
        let summary = if name == ARTIFACT_EDIT_TOOL && has_action(input, "create") {
            format!(
                "Call {name} at {}; the created artifact joins this Chat's Monitor and is linked to the current Loop for @tickloop requests",
                credential.origin
            )
        } else if read_owned == Some(false) {
            format!(
                "Read artifact content at {}; ownership or a trusted organization could not be confirmed",
                credential.origin
            )
        } else {
            format!("Call {name} at {}", credential.origin)
        };
        let approval_input = Self::approval_input(input)?;
        let facts = tickloop_agent_protocol::PreparedApprovalFacts {
            artifact_publication: publication.clone(),
            normalized_input: approval_input.clone(),
            external_declaration: Some(
                serde_json::to_value(&tool.definition)
                    .map_err(|_| "Artifact tool declaration cannot be serialized")?,
            ),
            ..Default::default()
        };
        Ok(PreparedHostCall::new(
            name.to_owned(),
            input.clone(),
            approval_input,
            permission,
            summary,
        )
        .with_approval_facts(facts)
        .with_permission_input(input.clone())
        .with_payload(PreparedMcpCall {
            credential,
            fingerprint,
            arguments: input.clone(),
            publication,
            read_owned,
        }))
    }

    async fn revalidate_call(
        &self,
        chat_id: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if call.payload::<PreparedArtifactLogin>().is_some() {
            return if cancel.is_cancelled() {
                Err("Sign-in was cancelled".into())
            } else {
                Ok(())
            };
        }
        let Some(prepared) = call.payload::<PreparedMcpCall>() else {
            return self
                .native
                .as_ref()
                .ok_or_else(|| "Host tool unavailable".to_owned())?
                .revalidate_call(chat_id, call, cancel)
                .await;
        };
        if cancel.is_cancelled() {
            return Err("MCP call cancelled".into());
        }
        let current = self
            .artifact
            .credential_snapshot()
            .map_err(|_| "Artifact credential unavailable")?
            .ok_or("Artifact login ended before MCP dispatch")?;
        if !prepared.credential.identity_matches(&current) {
            return Err("Artifact account changed before MCP dispatch".into());
        }
        let directory = self.load_directory(&current).await?;
        if directory.fingerprint != prepared.fingerprint
            || !directory
                .tools
                .iter()
                .any(|tool| tool.definition.name == call.name())
        {
            return Err("MCP tool directory changed before dispatch".into());
        }
        if call.event_context().is_some_and(|context| {
            context.origin != prepared.credential.origin
                || context.account_id != prepared.credential.account_id
        }) {
            return Err("MCP feedback claim belongs to another Artifact account".into());
        }
        if let Some(expected) = prepared.read_owned {
            let id = artifact_read_id(call.name(), &prepared.arguments)
                .ok_or("Artifact read identity changed")?;
            if self
                .artifact_read_is_owned(id, &current, cancel.clone())
                .await?
                != expected
            {
                return Err("Artifact read ownership changed before dispatch; prepare and check permission again".into());
            }
        }
        if let Some(expected) = &prepared.publication {
            let id = artifact_update_id(call.name(), &prepared.arguments)
                .ok_or("Artifact update identity changed")?;
            let current = tokio::select! {
                () = cancel.cancelled() => return Err("MCP call cancelled".into()),
                result = self.artifact.read_with_credential(id, &current) => {
                    result.map_err(|error| format!("Artifact publication state unavailable: {error}"))?
                }
            };
            if current.publication.as_ref() != Some(expected) {
                return Err(
                    "Artifact audience changed before dispatch; prepare and check permission again"
                        .into(),
                );
            }
        }
        Ok(())
    }

    async fn execute_prepared(
        &self,
        chat_id: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if call.payload::<PreparedArtifactLogin>().is_some() {
            if cancel.is_cancelled() {
                return HostCallResult::NotDispatched("Sign-in was cancelled".into());
            }
            return HostCallResult::Completed(self.start_login(cancel).await);
        }
        if call.payload::<PreparedMcpCall>().is_none() {
            return match &self.native {
                Some(native) => native.execute_prepared(chat_id, call, cancel).await,
                None => HostCallResult::NotDispatched("Host tool unavailable".into()),
            };
        }
        let name = call.name().to_owned();
        let permission = call.permission();
        let event_context = call.event_context().cloned();
        let Ok(prepared) = call.into_payload::<PreparedMcpCall>() else {
            unreachable!()
        };
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched("MCP call cancelled".into());
        }
        if !self.credential_is_current(&prepared.credential) {
            return HostCallResult::NotDispatched(
                "Artifact account changed before MCP dispatch".into(),
            );
        }
        let (service, http) = match self.open_session(&prepared.credential).await {
            Ok(session) => session,
            Err(error) => return HostCallResult::NotDispatched(error),
        };
        let arguments = match prepared.arguments.as_object() {
            Some(arguments) => arguments.clone(),
            None => return HostCallResult::NotDispatched("MCP arguments must be an object".into()),
        };
        let mut params = CallToolRequestParams::new(name.clone()).with_arguments(arguments);
        let mut metadata = rmcp::model::RequestMetaObject::new();
        if let Some(publication) = &prepared.publication {
            metadata
                .0
                .0
                .insert(PUBLICATION_META.into(), json!(publication));
        }
        if let Some(context) = &event_context {
            metadata.0.0.insert(
                EVENT_CONTEXT_META.into(),
                json!({
                    "version": 1,
                    "source_key": context.artifact_id,
                    "binding_id": context.binding_id.to_string(),
                    "association_epoch": context.epoch,
                    "event_id": context.request_id,
                    "claim_id": context.claim_id,
                    "fencing_token": context.fencing_token,
                }),
            );
        }
        if !metadata.0.0.is_empty() {
            params.meta = Some(metadata);
        }
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched("MCP call cancelled".into());
        }
        if !self.credential_is_current(&prepared.credential) {
            return HostCallResult::NotDispatched(
                "Artifact account changed before MCP dispatch".into(),
            );
        }
        let outcome = tokio::select! {
            () = cancel.cancelled() => return HostCallResult::OutcomeUnknown("MCP call was cancelled after dispatch; read the artifact again before deciding".into()),
            result = tokio::time::timeout(CALL_TIMEOUT, service.peer().call_tool_once(params)) => result,
        };
        if http.unauthorized() {
            self.invalidate(&prepared.credential.access_token).await;
        }
        if !self.credential_is_current(&prepared.credential) {
            return HostCallResult::OutcomeUnknown(
                "Artifact account changed after MCP dispatch; read the artifact again before deciding"
                    .into(),
            );
        }
        let result = match outcome {
            Ok(Ok(rmcp::model::CallToolResponse::Complete(result))) => result,
            Ok(Ok(_)) => {
                return HostCallResult::OutcomeUnknown(
                    "MCP call did not return a final result; read the artifact again before deciding"
                        .into(),
                );
            }
            _ => {
                return HostCallResult::OutcomeUnknown(
                    "MCP response unavailable after dispatch; read the artifact again before deciding"
                        .into(),
                );
            }
        };
        let effect = match name.as_str() {
            ARTIFACT_EDIT_TOOL => {
                Self::creation_effect(&result, &prepared).map(HostEffect::CreatedArtifact)
            }
            ARTIFACT_COMMENT_TOOL => event_context.as_ref().and_then(|context| {
                Self::finished_request_effect(&result, &prepared.arguments, context)
            }),
            _ => None,
        };
        if result.is_error != Some(true) {
            let admitted = match &effect {
                Some(HostEffect::CreatedArtifact(tick))
                    if permission == HostToolPermission::PrivateArtifactWrite =>
                {
                    Some((tick.artifact_id.clone(), ArtifactAudience::Private))
                }
                _ => prepared.publication.as_ref().and_then(|publication| {
                    let id = artifact_update_id(&name, &prepared.arguments)?;
                    let saved = result.structured_content.as_ref()?;
                    (saved["artifact_id"] == id
                        && saved["revision"].is_u64()
                        && matches!(
                            publication.audience,
                            ArtifactAudience::Private | ArtifactAudience::Public
                        ))
                    .then(|| (id.to_owned(), publication.audience))
                }),
            };
            if let Some((artifact, audience)) = admitted {
                self.publication_admissions
                    .write()
                    .unwrap()
                    .insert(ArtifactPublicationAdmission {
                        chat: chat_id,
                        account: prepared.credential.account_id.clone(),
                        origin: prepared.credential.origin.clone(),
                        artifact,
                        audience,
                    });
            }
        }
        let mut tool_result = Self::model_result(result, &prepared.credential.access_token);
        if let Some(context) = &event_context {
            tool_result.content = tool_result
                .content
                .replace(&context.fencing_token, "[redacted]");
        }
        match effect {
            Some(effect) => HostCallResult::CompletedWithEffect(tool_result, Box::new(effect)),
            None => HostCallResult::Completed(tool_result),
        }
    }

    async fn execute(
        &self,
        chat_id: ChatId,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        match &self.native {
            Some(native) if !self.is_mcp_name(name) && name != ARTIFACT_LOGIN_TOOL => {
                native.execute(chat_id, name, input, cancel).await
            }
            _ => ToolResult {
                content: "MCP tools require prepared dispatch".into(),
                is_error: true,
                output_ref: None,
            },
        }
    }
}
