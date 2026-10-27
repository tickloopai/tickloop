//! The Tickloop Artifacts MCP connection `tickloop-artifacts`, whose three tools the host presents as
//! first-party built-ins, backed by service-scoped credentials.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::{
        CallToolRequestParams, ClientConfig, ClientRequest, CustomRequest, ProtocolVersion, Tool,
    },
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
        HostCallResult, HostCallScope, HostEffect, HostToolPermission, HostToolProvider,
        PreparedHostCall, WorkspaceHostContext,
    },
    mcp_http::{McpHttpClient, REQUEST_TIMED_OUT, request_failure},
};
use tickloop_harness_artifacts::{
    ArtifactClient, ArtifactError, ArtifactMetadata, CredentialSnapshot, IMAGE_ARTIFACT_FORMAT,
    MAX_IMAGE_BYTES, artifact_image_type, artifact_type, artifact_type_for_tool, mcp_endpoint,
};
use tickloop_harness_protocol::{
    ArtifactAudience, ArtifactPublication, ChatId, HostEndpointIdentity, HostToolAuthority,
    RemoteSkillDefinition, SkillSource, ToolDefinition, ToolResult,
};

pub const ARTIFACT_READ_TOOL: &str = "ArtifactRead";
pub const ARTIFACT_EDIT_TOOL: &str = "ArtifactEdit";
pub const ARTIFACT_COMMENT_TOOL: &str = "ArtifactComment";
/// Offered instead of the Artifact tools while no account is signed in.
pub const ARTIFACT_LOGIN_TOOL: &str = "ArtifactLogin";
/// Uploads a local image to a Slides artifact; the host defines it and offers it along with the
/// directory's tools.
pub const ARTIFACT_UPLOAD_TOOL: &str = "ArtifactUpload";
/// The only tools taken from the service directory; the host reserves these names.
pub const ARTIFACT_TOOL_NAMES: [&str; 3] = [
    ARTIFACT_READ_TOOL,
    ARTIFACT_EDIT_TOOL,
    ARTIFACT_COMMENT_TOOL,
];
/// Every tool that acts for the signed-in Artifact account.
pub const ARTIFACT_ACCOUNT_TOOL_NAMES: [&str; 4] = [
    ARTIFACT_READ_TOOL,
    ARTIFACT_EDIT_TOOL,
    ARTIFACT_COMMENT_TOOL,
    ARTIFACT_UPLOAD_TOOL,
];

const SERVER_NAME: &str = "tickloop-artifacts";
const MCP_UPGRADE_CHECKPOINT_VERSION: u32 = 1;
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
/// The service is busy with this Slides artifact's other uploads; a later upload may succeed.
const UPLOAD_BUSY_STATUS: u16 = 429;
const ARTIFACT_LOGIN_DESCRIPTION: &str = "Tickloop Artifacts is not signed in. Call this when the user wants a doc, page or slides (a PPT, deck or presentation means slides); it starts sign-in and returns a link and code: give the user both, and do not write local files instead. After the user approves, ArtifactRead, ArtifactEdit, ArtifactComment and ArtifactUpload become available automatically.";
/// What the host adds to the service's own ArtifactEdit description.
const ARTIFACT_EDIT_NOTE: &str = " A slides artifact's PPTX download holds one picture per Slide, so its text cannot be edited there.";
/// What the host adds to the service's own ArtifactComment description.
const ARTIFACT_COMMENT_NOTE: &str = " Each @tickloop feedback request on an artifact associated with this Chat (one it created, or one the user linked to it) arrives here and in the user's other linked Agents. The association grants no extra permissions.";
/// How a Chat without a Folder writes Docs and Slides, so the user sees them fill in.
const FOLDERLESS_WRITING_PACE: &str = " In this chat, write Docs and Slides in small steps: first create a small skeleton (a title, one placeholder heading per section and a line saying “待写”), then fill one section at a time with separate small patches, telling the user briefly what you are about to write before each edit. A page supports whole-page replacement only: deliver a readable first version, then iterate with replacements.";
const ARTIFACT_UPLOAD_DESCRIPTION: &str = "Upload a local image file to a Slides artifact so its Slides can show it. Tickloop reads the file at file_path and sends it; its contents never reach you. PNG, JPEG, GIF, WebP or SVG, at most 5 MiB (an SVG at most 2 MiB); a file outside the workspace needs the user's approval. The result gives asset_id, ref (asset:<asset_id>), content_type, bytes, width and height (null when unknown); show the image in a Slide as <img src=\"<ref>\" alt=\"...\">. Upload each file once; a refused upload stores nothing, and after an unknown outcome read the Slides with ArtifactRead to see its images before uploading again.";

/// A host tool as a Chat offers it: one without a Folder also hears how to pace its writing.
pub(crate) fn chat_definition(mut definition: ToolDefinition, has_folder: bool) -> ToolDefinition {
    if !has_folder && definition.name == ARTIFACT_EDIT_TOOL {
        definition.description.push_str(FOLDERLESS_WRITING_PACE);
    }
    definition
}

pub(crate) fn login_definition() -> ToolDefinition {
    ToolDefinition {
        name: ARTIFACT_LOGIN_TOOL.into(),
        description: ARTIFACT_LOGIN_DESCRIPTION.into(),
        input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
    }
}

pub(crate) fn upload_definition() -> ToolDefinition {
    ToolDefinition {
        name: ARTIFACT_UPLOAD_TOOL.into(),
        description: ARTIFACT_UPLOAD_DESCRIPTION.into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "artifact_id": {
                    "type": "string",
                    "description": "The Slides artifact: the artifact_id ArtifactRead or ArtifactEdit returned, or the id in its /artifact/<artifact_id> URL"
                },
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the image file"
                }
            },
            "required": ["artifact_id", "file_path"],
            "additionalProperties": false
        }),
    }
}

#[derive(Clone)]
struct DiscoveredTool {
    definition: ToolDefinition,
    permission: HostToolPermission,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct SkillFileKey {
    credential_key: [u8; 32],
    uri: String,
    digest: String,
}

struct Directory {
    credential_key: [u8; 32],
    fingerprint: [u8; 32],
    protocol_version: ProtocolVersion,
    tools: Vec<DiscoveredTool>,
    skills: Vec<RemoteSkillDefinition>,
    skill_error: Option<String>,
    supports_resources: bool,
    instructions: Option<String>,
}

struct PreparedArtifactLogin;

/// Why the tool directory did not load: session establishment, tool listing, or validation failed.
enum DirectoryFailure {
    SessionUnavailable(String),
    ToolListingUnavailable(String),
    Rejected(String),
}

impl DirectoryFailure {
    fn class(&self) -> &'static str {
        match self {
            Self::SessionUnavailable(_) => "session_unavailable",
            Self::ToolListingUnavailable(_) => "tool_listing_unavailable",
            Self::Rejected(_) => "directory_rejected",
        }
    }
}

impl From<DirectoryFailure> for String {
    fn from(failure: DirectoryFailure) -> Self {
        match failure {
            DirectoryFailure::SessionUnavailable(reason)
            | DirectoryFailure::ToolListingUnavailable(reason)
            | DirectoryFailure::Rejected(reason) => reason,
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
            "{heading} Give the user this link and code: open {} and confirm the code {}. The code expires in about {minutes} minutes. Once the user approves, ArtifactRead, ArtifactEdit, ArtifactComment and ArtifactUpload become available automatically on a later request; the user can also sign in by running /artifacts.",
            self.verification_uri, self.user_code
        )
    }
}

struct PreparedMcpCall {
    credential: CredentialSnapshot,
    fingerprint: [u8; 32],
    arguments: Value,
    publication: Option<ArtifactPublication>,
    /// The role on the named Artifact this call needs, verified at preparation and again before
    /// dispatch.
    needed_role: Option<ArtifactRole>,
}

/// One approved image upload: exactly the bytes read and reviewed at preparation, sent once under
/// its operation ID.
struct PreparedArtifactUpload {
    credential: CredentialSnapshot,
    /// The artifact as the call named it, retained for request and approval correspondence.
    requested_id: String,
    artifact_id: String,
    content_type: &'static str,
    bytes: Vec<u8>,
    operation_id: uuid::Uuid,
    publication: Option<ArtifactPublication>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
struct ArtifactPublicationAdmission {
    chat: ChatId,
    account: String,
    origin: String,
    artifact: String,
    audience: ArtifactAudience,
}

#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct McpUpgradeCheckpoint {
    version: u32,
    publication_admissions: HashSet<ArtifactPublicationAdmission>,
    native: Option<crate::host_tools::HostToolUpgradeCheckpoint>,
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
        "rename" => {
            fields.len() == 4 && input["title"].is_string() && input["expected_title"].is_string()
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

/// An account's role on one Artifact, as the service reports it. An Agent acts with its user's
/// own role: a viewer reads and comments, an editor or the owner also changes the Artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ArtifactRole {
    Viewer,
    Editor,
    Owner,
}

impl ArtifactRole {
    fn parse(role: &str) -> Option<Self> {
        match role {
            "viewer" => Some(Self::Viewer),
            "editor" => Some(Self::Editor),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }
}

/// The existing Artifact a call reads or writes and the role it needs there: reading content or
/// threads and commenting need view access, any other `ArtifactEdit` action than create needs
/// edit access.
fn artifact_access_needed<'a>(name: &str, input: &'a Value) -> Option<(&'a str, ArtifactRole)> {
    if let Some(id) = artifact_read_id(name, input) {
        return Some((id, ArtifactRole::Viewer));
    }
    let needed = match name {
        ARTIFACT_COMMENT_TOOL => ArtifactRole::Viewer,
        ARTIFACT_EDIT_TOOL if !has_action(input, "create") => ArtifactRole::Editor,
        _ => return None,
    };
    bounded_identifier(input.get("artifact_id")).map(|id| (id, needed))
}

fn bounded_identifier(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES)
}

fn has_action(arguments: &Value, action: &str) -> bool {
    arguments.get("action").and_then(Value::as_str) == Some(action)
}

/// A failure before an Artifact call was sent, with what the model may safely do next.
fn nothing_sent(error: &str) -> String {
    format!(
        "{}. Nothing was sent to Tickloop Artifacts and no artifact changed; the call can be retried with the same revision and hashes.",
        error.trim_end_matches('.')
    )
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
    catalog_generation: std::sync::atomic::AtomicU64,
    service_status: RwLock<ArtifactServiceStatus>,
    skill_files: RwLock<BTreeMap<SkillFileKey, String>>,
    publication_admissions: RwLock<HashSet<ArtifactPublicationAdmission>>,
    pending_login: Arc<Mutex<Option<PendingLogin>>>,
    next_login_id: std::sync::atomic::AtomicU64,
    /// The background polling of device sign-ins, which outlives the tool call that started it.
    login_polls: tokio_util::task::TaskTracker,
}

impl Drop for McpHostToolProvider {
    fn drop(&mut self) {
        self.cancel_pending_login();
    }
}

impl McpHostToolProvider {
    /// The metadata of Artifact `id` once the service confirms this account holds at least
    /// `needed` on it. A call the account may not make, or whose access cannot be verified now,
    /// is refused instead of asked about: no approval grants access the user does not have.
    async fn verified_artifact(
        &self,
        id: &str,
        needed: ArtifactRole,
        credential: &CredentialSnapshot,
        cancel: CancellationToken,
    ) -> Result<ArtifactMetadata, String> {
        let metadata = tokio::select! {
            () = cancel.cancelled() => return Err("MCP call cancelled".into()),
            result = self.artifact.read_with_credential(id, credential) => result,
        };
        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(
                ArtifactError::Service {
                    status: 403 | 404, ..
                }
                | ArtifactError::Http(403 | 404),
            ) => return Err(format!("This account has no access to artifact {id}")),
            Err(error) => {
                return Err(format!(
                    "Access to artifact {id} could not be verified for this account: {error}"
                ));
            }
        };
        let role = ArtifactRole::parse(&metadata.access_role).ok_or_else(|| {
            format!("Access to artifact {id} could not be verified for this account: the service reported an unknown role")
        })?;
        if role < needed {
            return Err(format!(
                "This account can view artifact {id} and comment on it but not edit it"
            ));
        }
        Ok(metadata)
    }

    /// The permission of a write to `artifact` in this Chat, when its metadata reports a private
    /// or public audience: the first write to that audience, or a continuation of one this Chat
    /// already made.
    fn audience_permission(
        &self,
        chat_id: ChatId,
        credential: &CredentialSnapshot,
        artifact: &str,
        publication: Option<&ArtifactPublication>,
    ) -> Option<HostToolPermission> {
        let audience = publication?.audience;
        if !matches!(
            audience,
            ArtifactAudience::Private | ArtifactAudience::Public
        ) {
            return None;
        }
        let admission = ArtifactPublicationAdmission {
            chat: chat_id,
            account: credential.account_id.clone(),
            origin: credential.origin.clone(),
            artifact: tickloop_harness_artifacts::canonical_artifact_id(artifact),
            audience,
        };
        Some(
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
            },
        )
    }

    fn admit_publication(
        &self,
        chat_id: ChatId,
        credential: &CredentialSnapshot,
        artifact: String,
        audience: ArtifactAudience,
    ) {
        self.publication_admissions
            .write()
            .unwrap()
            .insert(ArtifactPublicationAdmission {
                chat: chat_id,
                account: credential.account_id.clone(),
                origin: credential.origin.clone(),
                artifact: tickloop_harness_artifacts::canonical_artifact_id(&artifact),
                audience,
            });
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
            && input["format"]
                .as_str()
                .is_some_and(|format| artifact_type_for_tool(format).is_some())
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
            catalog_generation: std::sync::atomic::AtomicU64::new(0),
            service_status: RwLock::new(ArtifactServiceStatus::default()),
            skill_files: RwLock::new(BTreeMap::new()),
            publication_admissions: RwLock::new(HashSet::new()),
            pending_login: Arc::new(Mutex::new(None)),
            next_login_id: std::sync::atomic::AtomicU64::new(0),
            login_polls: tokio_util::task::TaskTracker::new(),
        }
    }

    /// Constructs the Artifact tools a host installs on one Chat's Harness. The credential
    /// source and verified account belong to that Chat; native host tools remain composed.
    pub fn with_credentials(
        origin: &str,
        account_id: impl Into<String>,
        credentials: Arc<dyn tickloop_harness_artifacts::ArtifactCredentials>,
        native: Option<Arc<dyn HostToolProvider>>,
    ) -> Result<Self, ArtifactError> {
        Ok(Self::new(
            ArtifactClient::with_credentials(origin, account_id, credentials)?,
            native,
        ))
    }

    /// Sign-in is offered only while the last refresh confirmed that no account is signed in.
    fn offers_login(&self) -> bool {
        !self.artifact.is_host_managed()
            && self.service_status.read().unwrap().authentication
                == ArtifactAuthentication::SignedOut
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
                        "Tickloop Artifacts sign-in could not start: {error}. The user can run /artifacts to sign in."
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
        self.login_polls.spawn(async move {
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
        let previous = self.directory.write().unwrap().take();
        if previous.is_some() {
            self.catalog_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        drop(previous);
        self.skill_files.write().unwrap().clear();
    }

    fn credential_key(&self, snapshot: &CredentialSnapshot) -> [u8; 32] {
        self.artifact.credential_key(snapshot)
    }

    fn credential_is_current(&self, expected: &CredentialSnapshot) -> bool {
        self.artifact
            .credential_snapshot()
            .ok()
            .flatten()
            .is_some_and(|current| self.artifact.credentials_match(expected, &current))
    }

    fn is_mcp_name(&self, name: &str) -> bool {
        self.directory_tool(name).is_some()
    }

    /// The Artifact account the MCP tools act for.
    fn mcp_authority(&self) -> Result<Option<HostToolAuthority>, String> {
        Ok(self
            .artifact
            .signed_in_account_id()
            .map_err(|_| "Artifact credential unavailable")?
            .map(|account_id| HostToolAuthority::Mcp {
                server_id: self.artifact.origin().to_owned(),
                account_id,
            }))
    }

    /// The credential a call sends. A file-backed login renews an expired or expiring resource
    /// token here, so a long model turn does not read as signed out.
    async fn call_credential(&self) -> Result<Option<CredentialSnapshot>, ArtifactError> {
        if self.artifact.is_host_managed() {
            self.artifact.credential_snapshot()
        } else {
            self.artifact.access_snapshot().await
        }
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
        let http = McpHttpClient::new(endpoint.clone(), credential.access_token.clone())?
            .with_artifact_credentials(self.artifact.as_ref().clone());
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
            .map_err(DirectoryFailure::SessionUnavailable)?;
        let peer = service.peer().peer_info().ok_or_else(|| {
            DirectoryFailure::SessionUnavailable(
                "MCP discovery did not negotiate a protocol version".into(),
            )
        })?;
        if peer.server_info.as_ref().map(|info| info.name.as_str()) != Some(SERVER_NAME) {
            return Err(DirectoryFailure::Rejected(format!(
                "MCP server is not {SERVER_NAME}"
            )));
        }
        let protocol_version = peer.protocol_version.clone();
        let instructions = peer
            .instructions
            .clone()
            .filter(|instructions| !instructions.trim().is_empty());
        let tools =
            match tokio::time::timeout(DIRECTORY_TIMEOUT, service.peer().list_all_tools()).await {
                Ok(Ok(tools)) => tools,
                _ => {
                    if http.unauthorized() {
                        self.invalidate(&credential.access_token).await;
                    }
                    return Err(DirectoryFailure::ToolListingUnavailable(
                        "MCP tool directory unavailable".into(),
                    ));
                }
            };
        let capabilities = serde_json::to_value(&peer.capabilities)
            .map_err(|_| DirectoryFailure::Rejected("invalid MCP capabilities".into()))?;
        let supports_resources = capabilities.get("resources").is_some();
        let supports_skills = protocol_version == ProtocolVersion::V_2026_07_28
            && supports_resources
            && capabilities["extensions"]
                .get(crate::remote_skills::EXTENSION)
                .is_some();
        let mut directory = self
            .validate_directory(tools, credential, protocol_version)
            .map_err(DirectoryFailure::Rejected)?;
        directory.supports_resources = supports_resources;
        directory.instructions = instructions;
        if supports_skills {
            match Self::mcp_request(&service, "skills/list", json!({}))
                .await
                .and_then(|value| crate::remote_skills::parse_directory(&value, credential))
            {
                Ok(catalog) => {
                    directory.skills = catalog.definitions;
                    if !catalog.errors.is_empty() {
                        directory.skill_error = Some(catalog.errors.join("; "));
                    }
                }
                Err(error) => directory.skill_error = Some(error),
            }
        }
        Ok(directory)
    }

    async fn mcp_request(
        service: &RunningService<RoleClient, ClientConfig>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let resource_uri = params["uri"].as_str().map(str::to_owned);
        let request = ClientRequest::CustomRequest(CustomRequest::new(method, Some(params)));
        let result = tokio::time::timeout(DIRECTORY_TIMEOUT, service.peer().send_request(request))
            .await
            .map_err(|_| REQUEST_TIMED_OUT)?
            .map_err(|error| request_failure(&error, resource_uri.as_deref()))?;
        serde_json::to_value(result).map_err(|_| "invalid MCP response".into())
    }

    async fn resource_request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let source = self
            .mcp_resource_server()
            .ok_or("MCP resource source unavailable")?;
        let credential = self
            .artifact
            .credential_snapshot()
            .map_err(|_| "MCP credential unavailable")?
            .ok_or("Artifact account signed out")?;
        let operation = async {
            let (service, http) = self.open_session(&credential).await?;
            let peer = service
                .peer()
                .peer_info()
                .ok_or("MCP discovery incomplete")?;
            if peer.server_info.as_ref().map(|info| info.name.as_str()) != Some(SERVER_NAME)
                || serde_json::to_value(&peer.capabilities)
                    .ok()
                    .is_none_or(|capabilities| capabilities.get("resources").is_none())
            {
                return Err("MCP resource source or capabilities changed".into());
            }
            let result = Self::mcp_request(&service, method, params).await;
            if http.unauthorized() {
                self.invalidate(&credential.access_token).await;
            }
            result
        };
        let result = tokio::select! {
            () = cancel.cancelled() => return Err("MCP resource request cancelled".into()),
            result = operation => result?,
        };
        if !self.credential_is_current(&credential)
            || self.mcp_resource_server().as_ref() != Some(&source)
        {
            return Err("MCP resource source changed during reading".into());
        }
        Ok(result)
    }

    async fn invalidate(&self, token: &str) {
        if !self.artifact.is_host_managed() {
            let _ = self.artifact.invalidate_credential(token).await;
        }
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
            let host_note = match name {
                ARTIFACT_EDIT_TOOL => ARTIFACT_EDIT_NOTE,
                ARTIFACT_COMMENT_TOOL => ARTIFACT_COMMENT_NOTE,
                _ => "",
            };
            mapped.push(DiscoveredTool {
                definition: ToolDefinition {
                    name: name.to_owned(),
                    description: tool
                        .description
                        .as_ref()
                        .map_or_else(|| name.to_owned(), ToString::to_string)
                        + host_note,
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
            credential_key: self.credential_key(credential),
            fingerprint: Sha256::digest(serialized).into(),
            protocol_version,
            tools: mapped,
            skills: Vec::new(),
            skill_error: None,
            supports_resources: false,
            instructions: None,
        })
    }

    /// The artifact a confirmed `ArtifactEdit` create made, as a Tick of the calling account.
    fn creation_effect(
        result: &rmcp::model::CallToolResult,
        prepared: &PreparedMcpCall,
    ) -> Option<tickloop_harness_protocol::ArtifactTick> {
        if result.is_error == Some(true) || !has_action(&prepared.arguments, "create") {
            return None;
        }
        let structured = result.structured_content.as_ref()?;
        let artifact_id = bounded_identifier(structured.get("artifact_id"))?;
        if artifact_id.contains(['/', '?', '#']) {
            return None;
        }
        let title = structured.get("title")?.as_str()?;
        // Tool results use public format names; Monitor retains storage names.
        let format = artifact_type_for_tool(structured.get("format")?.as_str()?)?.stored_format;
        let url = structured.get("url")?.as_str()?;
        let parsed_url = reqwest::Url::parse(url).ok()?;
        let origin = reqwest::Url::parse(&prepared.credential.origin).ok()?;
        if parsed_url.origin() != origin.origin() {
            return None;
        }
        Some(tickloop_harness_protocol::ArtifactTick {
            artifact_id: artifact_id.into(),
            origin: prepared.credential.origin.clone(),
            title: title.into(),
            format: format.into(),
            url: url.into(),
            access_role: "owner".into(),
            added_at_ms: tickloop_harness_base::private_fs::unix_time_milliseconds(),
        })
    }

    /// The title a confirmed `ArtifactEdit` rename gave the artifact it named. The service answers
    /// with the artifact's canonical ID, also when the call named it by its short ID.
    fn rename_effect(
        result: &rmcp::model::CallToolResult,
        prepared: &PreparedMcpCall,
    ) -> Option<HostEffect> {
        if result.is_error == Some(true) || !has_action(&prepared.arguments, "rename") {
            return None;
        }
        let requested = artifact_update_id(ARTIFACT_EDIT_TOOL, &prepared.arguments)?;
        let saved = result.structured_content.as_ref()?;
        let artifact_id = bounded_identifier(saved.get("artifact_id"))?;
        if !tickloop_harness_artifacts::artifact_ids_match(requested, artifact_id) {
            return None;
        }
        let title = saved.get("title")?.as_str()?;
        if title.trim().is_empty() || title.encode_utf16().count() > MAX_ARTIFACT_TITLE_UTF16_UNITS
        {
            return None;
        }
        let origin = prepared.credential.origin.trim_end_matches('/');
        Some(HostEffect::RenamedArtifact {
            instance: tick_protocol::InstanceRef::artifact(
                origin,
                &tickloop_harness_artifacts::canonical_artifact_id(artifact_id),
            )
            .ok()?,
            title: title.to_owned(),
        })
    }

    fn read_effect(
        result: &rmcp::model::CallToolResult,
        prepared: &PreparedMcpCall,
    ) -> Option<HostEffect> {
        if result.is_error == Some(true) || !has_action(&prepared.arguments, "read") {
            return None;
        }
        let requested = artifact_read_id(ARTIFACT_READ_TOOL, &prepared.arguments)?;
        let read = result.structured_content.as_ref()?;
        let artifact_id = bounded_identifier(read.get("artifact_id"))?;
        if artifact_id.contains(['/', '?', '#'])
            || !tickloop_harness_artifacts::artifact_ids_match(requested, artifact_id)
        {
            return None;
        }
        let title = read.get("title")?.as_str()?;
        if title.trim().is_empty() || title.encode_utf16().count() > MAX_ARTIFACT_TITLE_UTF16_UNITS
        {
            return None;
        }
        read.get("revision")?.as_u64()?;
        let format = match read.get("format")?.as_str()? {
            format @ ("doc" | "slides") if read.get("blocks")?.is_array() => format,
            format @ "page" if read.get("source")?.is_string() => format,
            _ => return None,
        };
        let format = artifact_type_for_tool(format)?.stored_format;
        let access_role = match read.get("access_role")?.as_str()? {
            "owner" => "owner",
            "edit" => "editor",
            "view" => "viewer",
            _ => return None,
        };
        let origin = prepared.credential.origin.trim_end_matches('/');
        let url = read.get("url")?.as_str()?;
        let expected = tickloop_harness_artifacts::artifact_url(origin, artifact_id);
        if reqwest::Url::parse(url).ok()? != reqwest::Url::parse(&expected).ok()? {
            return None;
        }
        Some(HostEffect::ReadArtifact(
            tickloop_harness_protocol::ArtifactTick {
                artifact_id: tickloop_harness_artifacts::canonical_artifact_id(artifact_id),
                origin: origin.into(),
                title: title.into(),
                format: format.into(),
                url: url.into(),
                access_role: access_role.into(),
                added_at_ms: tickloop_harness_base::private_fs::unix_time_milliseconds(),
            },
        ))
    }

    /// The result for the model and transcript with the bearer token redacted: like Claude Code's
    /// `transformMCPResult`, the structured value alone when the server sent one, which already
    /// carries what its text content repeats, and otherwise the content blocks.
    fn model_result(result: rmcp::model::CallToolResult, access_token: &str) -> ToolResult {
        let content = match &result.structured_content {
            Some(structured) => structured.to_string(),
            None => result
                .content
                .iter()
                .map(|block| match block.as_text() {
                    Some(text) => text.text.clone(),
                    None => serde_json::to_string(block)
                        .unwrap_or_else(|_| "MCP content could not be displayed".into()),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
        .replace(access_token, "[redacted]");
        ToolResult {
            content,
            is_error: result.is_error == Some(true),
            output_ref: None,
        }
    }

    /// `ArtifactUpload` is offered while the signed-in account's Artifact tools are.
    fn offers_upload(&self) -> bool {
        self.directory.read().unwrap().is_some()
    }

    /// Resolves the file by the calling Chat's `Read` rules and reads it whole, so the approval
    /// covers the exact bytes later sent. The approval joins the write to the Slides, judged by
    /// the Artifact's audience like `ArtifactEdit`, with the decision a `Read` of the file gets.
    async fn prepare_upload(
        &self,
        chat_id: ChatId,
        scope: &HostCallScope<'_>,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if cancel.is_cancelled() {
            return Err("ArtifactUpload was cancelled".into());
        }
        let expected_key = self
            .directory
            .read()
            .unwrap()
            .as_ref()
            .map(|directory| directory.credential_key)
            .ok_or("ArtifactUpload needs the Artifact tools, which are unavailable now")?;
        let requested = bounded_identifier(input.get("artifact_id"))
            .filter(|id| !id.contains(['/', '?', '#', '%']))
            .ok_or("artifact_id must name one artifact")?;
        let file_path = input["file_path"]
            .as_str()
            .ok_or("file_path must be a string")?;
        let credential = self
            .call_credential()
            .await
            .map_err(|_| "Artifact credential unavailable")?
            .ok_or("Artifact login is no longer active")?;
        if self.credential_key(&credential) != expected_key {
            return Err("Artifact account changed before the upload was prepared".into());
        }
        let file = scope
            .local_files
            .ok_or("This Chat has no Folder to upload a local file from")?
            .read(file_path, MAX_IMAGE_BYTES)
            .await
            .map_err(|error| error.to_string())?;
        let content_type = artifact_image_type(&file.bytes)
            .map_err(|reason| format!("{} cannot be uploaded: {reason}", file.path.display()))?;
        let metadata = self
            .verified_artifact(requested, ArtifactRole::Editor, &credential, cancel)
            .await?;
        if metadata.format != IMAGE_ARTIFACT_FORMAT {
            return Err(format!(
                "ArtifactUpload adds images to Slides only; this artifact is a {}",
                artifact_type(&metadata.format).map_or(metadata.format.as_str(), |kind| kind.name)
            ));
        }
        let permission = self
            .audience_permission(
                chat_id,
                &credential,
                requested,
                metadata.publication.as_ref(),
            )
            .unwrap_or(HostToolPermission::Reviewable);
        let approval_input = json!({
            "artifact_id": requested,
            "file_path": file.path,
            "content_type": content_type,
            "bytes": file.bytes.len(),
            "sha256": format!("{:x}", Sha256::digest(&file.bytes)),
        });
        let summary = format!(
            "Upload {} ({content_type}, {}) to Slides {requested} at {}{}",
            file.path.display(),
            tickloop_harness_tools::format_file_size(file.bytes.len()),
            credential.origin,
            if file.read.requires_permission {
                "; the file is outside the workspace"
            } else {
                ""
            }
        );
        let facts = tickloop_harness_protocol::PreparedApprovalFacts {
            artifact_publication: metadata.publication.clone(),
            normalized_input: approval_input.clone(),
            canonical_path: Some(file.path.clone()),
            in_workspace: Some(file.read.in_workspace),
            read_only: Some(false),
            ..Default::default()
        };
        Ok(PreparedHostCall::new(
            ARTIFACT_UPLOAD_TOOL.into(),
            input.clone(),
            approval_input,
            permission,
            summary,
        )
        .with_approval_facts(facts)
        .with_permission_input(json!({"artifact_id": requested, "file_path": file.path}))
        .with_local_read(file.read, file.read_asks)
        .with_payload(PreparedArtifactUpload {
            credential,
            requested_id: requested.to_owned(),
            artifact_id: metadata.id,
            content_type,
            bytes: file.bytes,
            operation_id: tickloop_ids::generate(),
            publication: metadata.publication,
        }))
    }

    async fn revalidate_upload(
        &self,
        upload: &PreparedArtifactUpload,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("ArtifactUpload was cancelled".into());
        }
        let current = self
            .call_credential()
            .await
            .map_err(|_| "Artifact credential unavailable")?
            .ok_or("Artifact login ended before the upload")?;
        if !self
            .artifact
            .credentials_match(&upload.credential, &current)
        {
            return Err("Artifact account changed before the upload".into());
        }
        let metadata = self
            .verified_artifact(&upload.artifact_id, ArtifactRole::Editor, &current, cancel)
            .await?;
        if metadata.publication != upload.publication {
            return Err(
                "Artifact audience changed before the upload; prepare and check permission again"
                    .into(),
            );
        }
        Ok(())
    }

    /// Sends the approved bytes once. A lost response is settled by the service's receipt for the
    /// upload's operation, never by sending the image again.
    async fn upload(
        &self,
        chat_id: ChatId,
        upload: PreparedArtifactUpload,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched("ArtifactUpload was cancelled".into());
        }
        if !self
            .call_credential()
            .await
            .ok()
            .flatten()
            .is_some_and(|current| {
                self.artifact
                    .credentials_match(&upload.credential, &current)
            })
        {
            return HostCallResult::NotDispatched(
                "Artifact account changed before the upload".into(),
            );
        }
        let unknown = "read the Slides with ArtifactRead to see whether the image arrived before uploading it again";
        let outcome = tokio::select! {
            () = cancel.cancelled() => {
                return HostCallResult::OutcomeUnknown(format!("the upload was cancelled after it was sent; {unknown}"));
            }
            result = self.artifact.upload_image(
                &upload.artifact_id,
                upload.content_type,
                upload.bytes,
                upload.operation_id,
                &upload.credential,
            ) => result,
        };
        let refused = |content: String| {
            HostCallResult::Completed(ToolResult {
                content,
                is_error: true,
                output_ref: None,
            })
        };
        match outcome {
            Ok(image) => {
                if let Some(publication) = upload.publication.filter(|publication| {
                    matches!(
                        publication.audience,
                        ArtifactAudience::Private | ArtifactAudience::Public
                    )
                }) {
                    self.admit_publication(
                        chat_id,
                        &upload.credential,
                        upload.requested_id,
                        publication.audience,
                    );
                }
                HostCallResult::Completed(ToolResult {
                    content: json!({
                        "asset_id": image.asset_id,
                        "ref": image.reference,
                        "content_type": image.content_type,
                        "bytes": image.bytes,
                        "width": image.width,
                        "height": image.height,
                    })
                    .to_string(),
                    is_error: false,
                    output_ref: None,
                })
            }
            Err(ArtifactError::AccountChanged) => {
                HostCallResult::NotDispatched("Artifact account changed before the upload".into())
            }
            Err(
                error @ (ArtifactError::Storage
                | ArtifactError::InvalidOrigin
                | ArtifactError::InvalidResponse),
            ) => HostCallResult::NotDispatched(format!("the upload was not sent: {error}")),
            Err(ArtifactError::Service {
                status,
                code,
                message,
                ..
            }) => {
                let explanation = if message.is_empty() {
                    String::new()
                } else {
                    format!(": {}", message.trim_end_matches('.'))
                };
                let next_step = if status == UPLOAD_BUSY_STATUS {
                    " Nothing was stored; wait a moment, then upload the same file again."
                } else {
                    ""
                };
                refused(format!(
                    "Tickloop Artifacts refused the upload (HTTP {status} {code}){explanation}.{next_step}"
                ))
            }
            Err(ArtifactError::AccessDenied) => refused(
                "Tickloop Artifacts no longer accepts this sign-in; nothing was uploaded. The user can sign in again with /artifacts".into(),
            ),
            Err(ArtifactError::OutcomeUnknown { outcome, .. }) => HostCallResult::OutcomeUnknown(
                format!("the upload's result could not be confirmed ({outcome}); {unknown}"),
            ),
            Err(error) => HostCallResult::OutcomeUnknown(format!("{error}; {unknown}")),
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

    async fn prepare_mcp_call(
        &self,
        chat_id: ChatId,
        name: &str,
        input: &Value,
        (tool, fingerprint, expected_key): (DiscoveredTool, [u8; 32], [u8; 32]),
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if cancel.is_cancelled() {
            return Err("MCP call cancelled".into());
        }
        let credential = self
            .call_credential()
            .await
            .map_err(|_| "Artifact credential unavailable")?
            .ok_or("Artifact login is no longer active")?;
        if self.credential_key(&credential) != expected_key {
            return Err("Artifact account changed before MCP preparation".into());
        }
        let access = artifact_access_needed(name, input);
        let metadata = match access {
            Some((id, needed)) => Some(
                self.verified_artifact(id, needed, &credential, cancel.clone())
                    .await?,
            ),
            None => None,
        };
        let publication = artifact_update_id(name, input)
            .and(metadata)
            .and_then(|metadata| metadata.publication);
        let permission = artifact_update_id(name, input)
            .and_then(|artifact| {
                self.audience_permission(chat_id, &credential, artifact, publication.as_ref())
            })
            .unwrap_or_else(|| Self::operation_permission(&tool, input));
        let summary = if name == ARTIFACT_EDIT_TOOL && has_action(input, "create") {
            format!(
                "Call {name} at {}; the created artifact joins this Chat's Monitor and is linked to the current Loop for @tickloop requests",
                credential.origin
            )
        } else {
            format!("Call {name} at {}", credential.origin)
        };
        let approval_input = if name == ARTIFACT_EDIT_TOOL {
            input.clone()
        } else {
            Self::approval_input(input)?
        };
        let facts = tickloop_harness_protocol::PreparedApprovalFacts {
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
            needed_role: access.map(|(_, needed)| needed),
        }))
    }

    async fn revalidate_mcp_call(
        &self,
        call: &PreparedHostCall,
        prepared: &PreparedMcpCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("MCP call cancelled".into());
        }
        let current = self
            .call_credential()
            .await
            .map_err(|_| "Artifact credential unavailable")?
            .ok_or("Artifact login ended before MCP dispatch")?;
        if !self
            .artifact
            .credentials_match(&prepared.credential, &current)
        {
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
        if call.artifact_account().is_some_and(|context| {
            context.origin != prepared.credential.origin
                || context.account_id != prepared.credential.account_id
        }) {
            return Err("Feedback input belongs to another Artifact account".into());
        }
        let Some(needed) = prepared.needed_role else {
            return Ok(());
        };
        let (id, _) = artifact_access_needed(call.name(), &prepared.arguments)
            .ok_or("Artifact call identity changed")?;
        let metadata = self.verified_artifact(id, needed, &current, cancel).await?;
        if let Some(expected) = &prepared.publication
            && metadata.publication.as_ref() != Some(expected)
        {
            return Err(
                "Artifact audience changed before dispatch; prepare and check permission again"
                    .into(),
            );
        }
        Ok(())
    }

    async fn dispatch_mcp_call(
        &self,
        chat_id: ChatId,
        name: String,
        permission: HostToolPermission,
        prepared: PreparedMcpCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched("MCP call cancelled".into());
        }
        let current = match self.call_credential().await {
            Ok(Some(current))
                if self
                    .artifact
                    .credentials_match(&prepared.credential, &current) =>
            {
                current
            }
            _ => {
                return HostCallResult::NotDispatched(
                    "Artifact account changed before MCP dispatch".into(),
                );
            }
        };
        let (service, http) = match self.open_session(&current).await {
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
            self.invalidate(&current.access_token).await;
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
            Ok(Err(error)) => {
                let failure = request_failure(&error, None);
                return HostCallResult::OutcomeUnknown(format!(
                    "{failure} after dispatch; read the artifact again before deciding"
                ));
            }
            Err(_) => {
                return HostCallResult::OutcomeUnknown(format!(
                    "{REQUEST_TIMED_OUT} after dispatch; read the artifact again before deciding"
                ));
            }
        };
        let effect = match name.as_str() {
            ARTIFACT_READ_TOOL => Self::read_effect(&result, &prepared),
            ARTIFACT_EDIT_TOOL => Self::creation_effect(&result, &prepared)
                .map(HostEffect::CreatedArtifact)
                .or_else(|| Self::rename_effect(&result, &prepared)),
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
                    (saved["artifact_id"].as_str().is_some_and(|saved| {
                        tickloop_harness_artifacts::artifact_ids_match(id, saved)
                    }) && saved["revision"].is_u64()
                        && matches!(
                            publication.audience,
                            ArtifactAudience::Private | ArtifactAudience::Public
                        ))
                    .then(|| (id.to_owned(), publication.audience))
                }),
            };
            if let Some((artifact, audience)) = admitted {
                self.admit_publication(chat_id, &prepared.credential, artifact, audience);
            }
        }
        let mut tool_result = Self::model_result(result, &current.access_token);
        if let Ok(Some(latest)) = self.artifact.credential_snapshot() {
            tool_result.content = tool_result
                .content
                .replace(&latest.access_token, "[redacted]");
        }
        match effect {
            Some(effect) => HostCallResult::CompletedWithEffect(tool_result, Box::new(effect)),
            None => HostCallResult::Completed(tool_result),
        }
    }
}

#[async_trait]
impl HostToolProvider for McpHostToolProvider {
    async fn shutdown(&self) {
        self.cancel_pending_login();
        self.login_polls.close();
        self.login_polls.wait().await;
        if let Some(native) = &self.native {
            native.shutdown().await;
        }
    }

    fn upgrade_checkpoint(&self) -> Result<Option<Value>, String> {
        if self.pending_login.lock().unwrap().is_some() {
            return Ok(None);
        }
        let native = match &self.native {
            Some(native) => match native.upgrade_checkpoint()? {
                Some(state) => Some(crate::host_tools::HostToolUpgradeCheckpoint { state }),
                None => return Ok(None),
            },
            None => None,
        };
        serde_json::to_value(McpUpgradeCheckpoint {
            version: MCP_UPGRADE_CHECKPOINT_VERSION,
            publication_admissions: self.publication_admissions.read().unwrap().clone(),
            native,
        })
        .map(Some)
        .map_err(|error| error.to_string())
    }

    fn restore_upgrade(&self, state: Value) -> Result<(), String> {
        let checkpoint: McpUpgradeCheckpoint =
            serde_json::from_value(state).map_err(|error| error.to_string())?;
        if checkpoint.version != MCP_UPGRADE_CHECKPOINT_VERSION
            || self.pending_login.lock().unwrap().is_some()
        {
            return Err("MCP provider is not ready for this upgrade checkpoint".into());
        }
        match (&self.native, checkpoint.native) {
            (Some(native), Some(checkpoint)) => native.restore_upgrade(checkpoint.state)?,
            (None, None) => {}
            _ => return Err("host tool provider composition changed during upgrade".into()),
        }
        *self.publication_admissions.write().unwrap() = checkpoint.publication_admissions;
        Ok(())
    }

    fn is_concurrency_safe(&self, name: &str, input: &Value) -> bool {
        if ARTIFACT_ACCOUNT_TOOL_NAMES.contains(&name) || name == ARTIFACT_LOGIN_TOOL {
            name == ARTIFACT_READ_TOOL
        } else {
            self.native
                .as_ref()
                .is_some_and(|provider| provider.is_concurrency_safe(name, input))
        }
    }

    /// Native tools never take the Artifact names; only the service directory provides them,
    /// together with the provider's own upload, and the provider offers sign-in while signed out.
    fn catalog_generation(&self) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let native = match &self.native {
            Some(native) => native.catalog_generation()?,
            None => 0,
        };
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        native.hash(&mut hash);
        self.catalog_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            .hash(&mut hash);
        (self.service_status.read().unwrap().authentication == ArtifactAuthentication::SignedOut)
            .hash(&mut hash);
        Some(hash.finish())
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .native
            .as_ref()
            .map(|provider| provider.definitions())
            .unwrap_or_default();
        definitions.retain(|definition| {
            !ARTIFACT_ACCOUNT_TOOL_NAMES.contains(&definition.name.as_str())
                && definition.name != ARTIFACT_LOGIN_TOOL
        });
        if let Some(directory) = self.directory.read().unwrap().as_ref() {
            definitions.extend(directory.tools.iter().map(|tool| tool.definition.clone()));
            definitions.push(upload_definition());
        } else if self.offers_login() {
            definitions.push(login_definition());
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
        if self.is_mcp_name(name) || ARTIFACT_ACCOUNT_TOOL_NAMES.contains(&name) {
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
        if self.is_mcp_name(name) || name == ARTIFACT_UPLOAD_TOOL && self.offers_upload() {
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
        let generation = self.artifact.credential_snapshot().map(|credential| {
            credential
                .as_ref()
                .map(|credential| self.credential_key(credential))
        });
        connections["artifacts"] = if generation
            .as_ref()
            .is_ok_and(|generation| *generation == status.credential_generation)
        {
            json!(*status)
        } else {
            json!(ArtifactServiceStatus::default())
        };
        let directory = self.directory.read().unwrap();
        connections["artifacts"]["skills"] = directory
            .as_ref()
            .filter(|directory| {
                generation
                    .as_ref()
                    .is_ok_and(|key| *key == Some(directory.credential_key))
            })
            .map_or_else(
                || json!({"state":"unavailable"}),
                |directory| match &directory.skill_error {
                    Some(error) if !directory.skills.is_empty() => {
                        json!({"state":"partial","count":directory.skills.len(),"error":error})
                    }
                    Some(error) => json!({"state":"unavailable","error":error}),
                    None if directory.skills.is_empty() => json!({"state":"not_supported"}),
                    None => json!({"state":"available","count":directory.skills.len()}),
                },
            );
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

    fn mcp_resource_server(&self) -> Option<crate::host_tools::McpResourceServer> {
        let credential = self.artifact.credential_snapshot().ok()??;
        let directory = self.directory.read().unwrap();
        let directory = directory.as_ref()?;
        if !directory.supports_resources
            || directory.credential_key != self.credential_key(&credential)
        {
            return None;
        }
        Some(crate::host_tools::McpResourceServer {
            name: SERVER_NAME.into(),
            authority: HostToolAuthority::Mcp {
                server_id: credential.origin,
                account_id: credential.account_id,
            },
            third_party: false,
        })
    }

    fn mcp_instructions(&self) -> Vec<crate::host_tools::McpServerInstructions> {
        let Ok(Some(credential)) = self.artifact.credential_snapshot() else {
            return Vec::new();
        };
        let directory = self.directory.read().unwrap();
        directory
            .as_ref()
            .filter(|directory| directory.credential_key == self.credential_key(&credential))
            .and_then(|directory| directory.instructions.clone())
            .map(|text| crate::host_tools::McpServerInstructions {
                server: crate::host_tools::McpResourceServer {
                    name: SERVER_NAME.into(),
                    authority: HostToolAuthority::Mcp {
                        server_id: credential.origin,
                        account_id: credential.account_id,
                    },
                    third_party: false,
                },
                status: None,
                text,
            })
            .into_iter()
            .collect()
    }

    async fn list_mcp_resources(
        &self,
        cursor: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let params = cursor.map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
        let result = self
            .resource_request("resources/list", params, cancel)
            .await?;
        let resources = result["resources"]
            .as_array()
            .ok_or("invalid MCP resource directory")?;
        if resources.iter().any(|resource| {
            resource["uri"].as_str().is_none() || resource["name"].as_str().is_none()
        }) || result
            .get("nextCursor")
            .is_some_and(|cursor| !cursor.is_string())
        {
            return Err("invalid MCP resource directory".into());
        }
        Ok(result)
    }

    async fn read_mcp_resource(
        &self,
        uri: &str,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let result = self
            .resource_request("resources/read", json!({"uri":uri}), cancel)
            .await?;
        let contents = result["contents"]
            .as_array()
            .ok_or("invalid MCP resource content")?;
        if contents.is_empty()
            || contents.iter().any(|content| {
                content["uri"] != uri
                    || !(content["text"].is_string() || content["blob"].is_string())
            })
        {
            return Err("invalid MCP resource content".into());
        }
        Ok(result)
    }

    fn remote_skills(&self) -> Vec<RemoteSkillDefinition> {
        let Ok(Some(credential)) = self.artifact.credential_snapshot() else {
            return Vec::new();
        };
        self.directory
            .read()
            .unwrap()
            .as_ref()
            .filter(|directory| directory.credential_key == self.credential_key(&credential))
            .map(|directory| directory.skills.clone())
            .unwrap_or_default()
    }

    fn remote_skill_is_current(&self, source: &SkillSource) -> bool {
        self.remote_skills()
            .iter()
            .any(|definition| &definition.source == source)
    }

    async fn read_remote_skill(
        &self,
        definition: &RemoteSkillDefinition,
        relative: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        if !self.remote_skill_is_current(&definition.source) {
            return Err("Skill source, account or manifest changed".into());
        }
        let credential = self
            .artifact
            .credential_snapshot()
            .map_err(|_| "Skill credential unavailable")?
            .ok_or("Artifact account signed out")?;
        let resource = crate::remote_skills::resource_entry(definition, relative)?;
        let key = SkillFileKey {
            credential_key: self.credential_key(&credential),
            uri: resource.uri.clone(),
            digest: resource.digest.clone(),
        };
        if let Some(text) = self.skill_files.read().unwrap().get(&key).cloned() {
            return Ok(text);
        }
        let operation = async {
            let (service, http) = self.open_session(&credential).await?;
            let result =
                Self::mcp_request(&service, "resources/read", json!({"uri": resource.uri})).await;
            if http.unauthorized() {
                self.invalidate(&credential.access_token).await;
            }
            crate::remote_skills::verify_text(resource, &result?)
        };
        let text = tokio::select! {
            () = cancel.cancelled() => return Err("Skill loading cancelled".into()),
            result = operation => result?,
        };
        if !self.credential_is_current(&credential)
            || !self.remote_skill_is_current(&definition.source)
        {
            return Err("Skill source changed during loading".into());
        }
        let mut cache = self.skill_files.write().unwrap();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, text.clone());
        Ok(text)
    }

    fn awaited_mcp_servers(&self) -> Option<tokio::sync::watch::Receiver<Vec<String>>> {
        self.native
            .as_ref()
            .and_then(|native| native.awaited_mcp_servers())
    }

    async fn refresh(&self, cancel: CancellationToken) -> Result<(), String> {
        if let Some(native) = &self.native {
            native.refresh(cancel.clone()).await?;
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        let mut refresh_result = Ok(());
        let signed_out = || ArtifactServiceStatus {
            authentication: ArtifactAuthentication::SignedOut,
            ..Default::default()
        };
        let (mut status, credential) = match Box::pin(self.artifact.verified_snapshot()).await {
            Ok(Some((user, verified))) => match self.artifact.credential_snapshot() {
                Ok(Some(credential)) if self.artifact.credentials_match(&verified, &credential) => {
                    (
                        ArtifactServiceStatus {
                            authentication: ArtifactAuthentication::SignedIn,
                            credential_generation: Some(self.credential_key(&credential)),
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
            if let Err(failure) = &directory {
                let error_class = failure.class();
                tracing::warn!(
                    origin = self.artifact.origin(),
                    error_class,
                    "MCP tool directory refresh failed"
                );
                refresh_result = Err(format!(
                    "MCP tool directory refresh failed (origin={}, error_class={error_class})",
                    self.artifact.origin()
                ));
            }
            match self.artifact.credential_snapshot() {
                Ok(Some(current))
                    if self.credential_key(&current) == self.credential_key(&credential) =>
                {
                    match directory {
                        Ok(directory) => {
                            status.tool_directory = McpToolDirectory::Available;
                            let previous = {
                                let mut current = self.directory.write().unwrap();
                                if current.as_ref().is_none_or(|current| {
                                    current.fingerprint != directory.fingerprint
                                        || current.credential_key != directory.credential_key
                                }) {
                                    self.catalog_generation
                                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                }
                                current.replace(directory)
                            };
                            drop(previous);
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
                credential
                    .as_ref()
                    .map(|credential| self.credential_key(credential))
                    == status.credential_generation
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
        refresh_result
    }

    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        if name == ARTIFACT_LOGIN_TOOL {
            return input
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
                .then_some(())
                .ok_or_else(|| format!("{ARTIFACT_LOGIN_TOOL} takes no input"));
        }
        if name == ARTIFACT_UPLOAD_TOOL {
            return crate::validate_host_schema_input(name, schema, input);
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
        if name == ARTIFACT_UPLOAD_TOOL {
            return HostToolPermission::Reviewable;
        }
        self.native
            .as_ref()
            .map_or(HostToolPermission::Reviewable, |native| {
                native.permission(chat_id, name, input)
            })
    }

    async fn prepare_call_in_scope(
        &self,
        chat_id: ChatId,
        scope: &HostCallScope<'_>,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if name == ARTIFACT_UPLOAD_TOOL {
            self.prepare_upload(chat_id, scope, input, cancel).await
        } else if self.is_mcp_name(name) || name == ARTIFACT_LOGIN_TOOL {
            self.prepare_call(chat_id, name, input, cancel).await
        } else {
            self.native
                .as_ref()
                .ok_or_else(|| "Host tool unavailable".to_owned())?
                .prepare_call_in_scope(chat_id, scope, name, input, cancel)
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
                return Err("Tickloop Artifacts is not signed out; sign-in is unavailable".into());
            }
            return Ok(PreparedHostCall::new(
                name.to_owned(),
                input.clone(),
                input.clone(),
                HostToolPermission::Automatic,
                format!(
                    "Start Tickloop Artifacts sign-in at {}",
                    self.artifact.origin()
                ),
            )
            .with_payload(PreparedArtifactLogin));
        }
        if name == ARTIFACT_UPLOAD_TOOL {
            return Err(format!(
                "{ARTIFACT_UPLOAD_TOOL} reads its file in the calling Chat's workspace"
            ));
        }
        let Some(directory_tool) = self.directory_tool(name) else {
            return self
                .native
                .as_ref()
                .ok_or_else(|| "Host tool unavailable".to_owned())?
                .prepare_call(chat_id, name, input, cancel)
                .await;
        };
        self.prepare_mcp_call(chat_id, name, input, directory_tool, cancel)
            .await
            .map_err(|error| nothing_sent(&error))
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
        if let Some(upload) = call.payload::<PreparedArtifactUpload>() {
            if call.artifact_account().is_some_and(|context| {
                context.origin != upload.credential.origin
                    || context.account_id != upload.credential.account_id
            }) {
                return Err("Feedback input belongs to another Artifact account".into());
            }
            return self.revalidate_upload(upload, cancel).await;
        }
        let Some(prepared) = call.payload::<PreparedMcpCall>() else {
            return self
                .native
                .as_ref()
                .ok_or_else(|| "Host tool unavailable".to_owned())?
                .revalidate_call(chat_id, call, cancel)
                .await;
        };
        self.revalidate_mcp_call(call, prepared, cancel)
            .await
            .map_err(|error| nothing_sent(&error))
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
        let call = match call.into_payload::<PreparedArtifactUpload>() {
            Ok(upload) => return self.upload(chat_id, upload, cancel).await,
            Err(call) => *call,
        };
        if call.payload::<PreparedMcpCall>().is_none() {
            return match &self.native {
                Some(native) => native.execute_prepared(chat_id, call, cancel).await,
                None => HostCallResult::NotDispatched("Host tool unavailable".into()),
            };
        }
        let name = call.name().to_owned();
        let permission = call.permission();
        let Ok(prepared) = call.into_payload::<PreparedMcpCall>() else {
            unreachable!()
        };
        match self
            .dispatch_mcp_call(chat_id, name, permission, prepared, cancel)
            .await
        {
            HostCallResult::NotDispatched(error) => {
                HostCallResult::NotDispatched(nothing_sent(&error))
            }
            dispatched => dispatched,
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
            Some(native)
                if !self.is_mcp_name(name)
                    && name != ARTIFACT_LOGIN_TOOL
                    && name != ARTIFACT_UPLOAD_TOOL =>
            {
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

#[cfg(test)]
mod upgrade_tests {
    use super::*;
    use crate::Agent;
    use tickloop_harness_test_support::{ScriptedModel, config};

    struct StatelessNative;

    #[async_trait]
    impl HostToolProvider for StatelessNative {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }
        async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
            panic!("restoring a provider must not execute tools");
        }
    }

    #[tokio::test]
    async fn stateless_provider_presence_survives_the_private_checkpoint_roundtrip() {
        let directory = tempfile::tempdir().unwrap();
        let original = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
        original
            .install_host_tools(Arc::new(StatelessNative))
            .unwrap();
        original.create_chat(directory.path().into()).await.unwrap();
        let checkpoint = original.prepare_upgrade().await.unwrap().unwrap();
        let successor = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
        successor
            .install_host_tools(Arc::new(StatelessNative))
            .unwrap();
        successor
            .restore_upgrade(
                serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap(),
            )
            .await
            .unwrap();
        successor.activate_upgrade().await.unwrap();
        original.shutdown_for_upgrade().await;
        successor.release_upgrade().await.unwrap();
        successor.shutdown().await;
    }

    #[tokio::test]
    async fn pending_device_login_defers_worker_upgrade_without_cancelling_it() {
        let directory = tempfile::tempdir().unwrap();
        let provider = Arc::new(McpHostToolProvider::new(
            ArtifactClient::new(directory.path()).unwrap(),
            None,
        ));
        let login_cancel = CancellationToken::new();
        *provider.pending_login.lock().unwrap() = Some(PendingLogin {
            id: 1,
            verification_uri: "https://example.test/device".into(),
            user_code: "FIXTURE".into(),
            expires_at: tokio::time::Instant::now() + Duration::from_secs(300),
            cancel: login_cancel.clone(),
        });
        let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
        agent.install_host_tools(provider.clone()).unwrap();
        agent.create_chat(directory.path().into()).await.unwrap();
        assert!(agent.prepare_upgrade().await.unwrap().is_none());
        assert!(!login_cancel.is_cancelled());
        provider.pending_login.lock().unwrap().take();
        assert!(agent.prepare_upgrade().await.unwrap().is_some());
        agent.cancel_upgrade().await;
        agent.shutdown().await;
    }

    #[test]
    fn publication_grants_survive_only_with_their_original_owner_and_scope() {
        let directory = tempfile::tempdir().unwrap();
        let original = McpHostToolProvider::new(
            ArtifactClient::new(directory.path()).unwrap(),
            Some(Arc::new(StatelessNative)),
        );
        let chat = uuid::Uuid::new_v4();
        let credential = CredentialSnapshot {
            origin: "https://artifacts.example.test".into(),
            account_id: "account-a".into(),
            access_token: "fixture".into(),
        };
        original.admit_publication(
            chat.into(),
            &credential,
            "artifact-a".into(),
            ArtifactAudience::Public,
        );
        let state = original.upgrade_checkpoint().unwrap().unwrap();
        let restored = McpHostToolProvider::new(
            ArtifactClient::new(directory.path()).unwrap(),
            Some(Arc::new(StatelessNative)),
        );
        restored
            .restore_upgrade(serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap())
            .unwrap();
        let publication = ArtifactPublication {
            audience: ArtifactAudience::Public,
            membership_revision: 7,
        };
        assert_eq!(
            restored.audience_permission(
                chat.into(),
                &credential,
                "artifact-a",
                Some(&publication)
            ),
            Some(HostToolPermission::ArtifactContinuation)
        );
        for (owner, identity, artifact) in [
            (
                uuid::Uuid::new_v4(),
                CredentialSnapshot {
                    origin: "https://artifacts.example.test".into(),
                    account_id: "account-a".into(),
                    access_token: "fixture".into(),
                },
                "artifact-a",
            ),
            (
                chat,
                CredentialSnapshot {
                    origin: "https://artifacts.example.test".into(),
                    account_id: "account-b".into(),
                    access_token: "fixture".into(),
                },
                "artifact-a",
            ),
            (
                chat,
                CredentialSnapshot {
                    origin: "https://other.example.test".into(),
                    account_id: "account-a".into(),
                    access_token: "fixture".into(),
                },
                "artifact-a",
            ),
            (
                chat,
                CredentialSnapshot {
                    origin: "https://artifacts.example.test".into(),
                    account_id: "account-a".into(),
                    access_token: "fixture".into(),
                },
                "artifact-b",
            ),
        ] {
            assert_eq!(
                restored.audience_permission(owner.into(), &identity, artifact, Some(&publication)),
                Some(HostToolPermission::PublicArtifactWrite)
            );
        }
        assert_eq!(
            restored.audience_permission(
                chat.into(),
                &credential,
                "artifact-a",
                Some(&ArtifactPublication {
                    audience: ArtifactAudience::Private,
                    membership_revision: 7,
                })
            ),
            Some(HostToolPermission::PrivateArtifactWrite)
        );
        assert!(restored.directory.read().unwrap().is_none());
        assert!(restored.skill_files.read().unwrap().is_empty());
    }
}

#[cfg(test)]
mod chat_definition_tests {
    use super::*;

    fn edit() -> ToolDefinition {
        ToolDefinition {
            name: ARTIFACT_EDIT_TOOL.into(),
            description: "Write Tickloop Artifacts.".into(),
            input_schema: json!({"type":"object"}),
        }
    }

    /// Only a Chat without a Folder writes Docs and Slides at the pace its viewer watches; the
    /// other tools keep their text.
    #[test]
    fn only_a_folderless_chat_hears_how_to_pace_artifact_writing() {
        assert_eq!(chat_definition(edit(), true), edit());
        let folderless = chat_definition(edit(), false);
        assert!(folderless.description.ends_with(FOLDERLESS_WRITING_PACE));
        assert!(folderless.description.contains("separate small patches"));
        assert!(
            folderless
                .description
                .contains("whole-page replacement only")
        );
        assert_eq!(
            chat_definition(upload_definition(), false),
            upload_definition()
        );
    }

    /// Signed out, ArtifactLogin alone carries what an artifact request means.
    #[test]
    fn the_login_tool_routes_artifact_requests_while_signed_out() {
        let login = login_definition().description;
        for current in [
            "when the user wants a doc, page or slides",
            "a PPT, deck or presentation means slides",
            "give the user both, and do not write local files instead",
        ] {
            assert!(login.contains(current), "{current}");
        }
        assert!(ARTIFACT_EDIT_NOTE.contains("one picture per Slide, so its text cannot be edited"));
        assert!(ARTIFACT_COMMENT_NOTE.contains("in the user's other linked Agents"));
    }
}
