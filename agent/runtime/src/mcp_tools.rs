//! A namespaced MCP Host tool provider backed by service-scoped credentials.

use std::{
    collections::{BTreeSet, HashSet},
    sync::{Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::{
        CallToolRequestParams, ClientConfig, ProtocolVersion, ReadResourceRequestParams,
        ReadResourceResponse, ResourceContents, Tool,
    },
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    host_tools::{
        ClaimedEvent, ConnectAction, ConnectSourceEffect, EventAssociationOutcome,
        EventAssociationRequest, EventClaimOutcome, EventClaimRequest, EventLeaseOutcome,
        EventLeaseRequest, EventPendingOutcome, EventSourceLocator, HostCallResult, HostEffect,
        HostToolPermission, HostToolProvider, PendingEvent, PreparedHostCall,
        PublishedSourceEffect, RemoteAssociation, RemoteAssociationOperation, WorkspaceHostContext,
        connection_result,
    },
    mcp_http::McpHttpClient,
};
use tickloop_agent_artifacts::{ArtifactClient, CredentialSnapshot};
use tickloop_agent_protocol::{
    ChatId, HostCallContext, HostEndpointIdentity, HostToolAuthority, ToolDefinition, ToolResult,
};

const SERVER_NAMESPACE: &str = "artifacts";
const ARTIFACT_ATTACHMENT_URI: &str = "tickloop://artifact-tick-attachments/v1";
const MAX_TOOLS: usize = 128;
const DIRECTORY_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_DESCRIPTOR_BYTES: usize = 64 * 1024;
const MAX_SAFE_EPOCH: u64 = 9_007_199_254_740_991;
const MAX_APPROVAL_PREVIEW_BYTES: usize = 160;
const MAX_APPROVAL_INPUT_BYTES: usize = 32 * 1024;
const MAX_APPROVAL_COLLECTION_ITEMS: usize = 64;
const MAX_APPROVAL_DEPTH: usize = 8;
const MAX_IDENTIFIER_BYTES: usize = 256;
/// Fixed argument of the v2 connection protocol; the descriptor maps the other three.
const CONNECTION_REPLACE_ARGUMENT: &str = "replace_existing";
const CONNECT_CAPABILITY_META: &str = "tickloop.ai/source-connect-capability";
const CONNECT_ENVELOPE_META: &str = "tickloop.ai/source-connect/v1";
/// Authenticated association errors that prove the request did not commit.
const DEFINITE_ASSOCIATION_REJECTIONS: [&str; 4] = [
    "association_conflict",
    "operation_conflict",
    "not_found",
    "invalid_request",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EventBridgeDescriptor {
    version: u8,
    publication_tools: Vec<PublicationTool>,
    tools: EventRoleTools,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EventBridgeDescriptorV2 {
    version: u8,
    publication_tools: Vec<PublicationTool>,
    connection_tool: ConnectionTool,
    tools: EventRoleTools,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConnectionTool {
    tool: String,
    source_key_argument: String,
    operation_id_argument: String,
    action_argument: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PublicationTool {
    tool: String,
    operation_id_argument: String,
    source_key_result: String,
    version_key_result: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactAttachmentDescriptor {
    version: u8,
    creation_tools: Vec<CreationTool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreationTool {
    tool: String,
    operation_id_argument: String,
    artifact_id_result: String,
    title_argument: String,
    format_argument: String,
    url_result: String,
}

struct ArtifactAttachments {
    creation_tools: Vec<CreationTool>,
    fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EventRoleTools {
    associate: String,
    pending: String,
    claim: String,
    renew: String,
    release: String,
    operation: String,
}

impl EventRoleTools {
    fn names(&self) -> [&str; 6] {
        [
            &self.associate,
            &self.pending,
            &self.claim,
            &self.renew,
            &self.release,
            &self.operation,
        ]
    }
}

struct EventBridge {
    publication_tools: Vec<PublicationTool>,
    connection: Option<ConnectionTool>,
    tools: EventRoleTools,
    fingerprint: [u8; 32],
}

enum DescriptorRead {
    Text(String),
    Missing,
    Failed(String),
}

#[derive(Clone, Copy)]
enum EventRole {
    Pending,
    Claim,
    Renew,
    Release,
}

enum EventRoleError {
    NotDispatched(String),
    OutcomeUnknown(String),
}

#[derive(Clone)]
struct DiscoveredTool {
    remote_name: String,
    definition: ToolDefinition,
    permission: HostToolPermission,
}

struct Directory {
    credential_key: [u8; 32],
    fingerprint: [u8; 32],
    protocol_version: ProtocolVersion,
    tools: Vec<DiscoveredTool>,
    bridge: Option<EventBridge>,
    attachments: Option<ArtifactAttachments>,
}

struct PreparedMcpCall {
    credential: CredentialSnapshot,
    fingerprint: [u8; 32],
    bridge_fingerprint: Option<[u8; 32]>,
    attachment_fingerprint: Option<[u8; 32]>,
    creation: Option<CreationTool>,
    remote_name: String,
    arguments: Value,
    publication: Option<PublicationTool>,
    connection: Option<PreparedConnection>,
}

#[derive(Clone)]
struct ConnectionRequest {
    identity: serde_json::Map<String, Value>,
    source_key: String,
    operation_id: String,
    action: ConnectAction,
}

/// A connect is read once before approval and that frozen read is what execution consumes.
struct PreparedConnection {
    request: ConnectionRequest,
    snapshot: Option<ConnectionSnapshot>,
}

struct ConnectionSnapshot {
    result: ToolResult,
    current_association: Option<RemoteAssociation>,
    association_operation: Option<RemoteAssociationOperation>,
}

enum ConnectionReply {
    Rejected(ToolResult),
    Read(ConnectionSnapshot),
}

fn bounded_identifier(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES)
}

fn safe_epoch(value: Option<&Value>) -> Option<u64> {
    value
        .and_then(Value::as_u64)
        .filter(|epoch| (1..=MAX_SAFE_EPOCH).contains(epoch))
}

fn has_exact_keys(value: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    value.len() == keys.len() && keys.iter().all(|key| value.contains_key(*key))
}

fn is_argument_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

impl ConnectionRequest {
    fn parse(input: &Value, mapping: &ConnectionTool) -> Result<Self, String> {
        let fields = input
            .as_object()
            .ok_or("connection arguments must be an object")?;
        let allowed = [
            mapping.source_key_argument.as_str(),
            mapping.operation_id_argument.as_str(),
            mapping.action_argument.as_str(),
            CONNECTION_REPLACE_ARGUMENT,
        ];
        if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err("connection arguments cannot select a Chat, Loop, epoch or claim".into());
        }
        let source_key = bounded_identifier(fields.get(&mapping.source_key_argument))
            .ok_or("connection requires an artifact")?;
        let operation_id = bounded_identifier(fields.get(&mapping.operation_id_argument))
            .ok_or("connection requires an operation ID")?;
        let replace_existing = match fields.get(CONNECTION_REPLACE_ARGUMENT) {
            None => None,
            Some(Value::Bool(replace)) => Some(*replace),
            Some(_) => return Err("replace_existing must be a boolean".into()),
        };
        let action = match fields.get(&mapping.action_argument).map(Value::as_str) {
            None | Some(Some("connect")) => ConnectAction::Connect {
                replace_existing: replace_existing.unwrap_or(false),
            },
            Some(Some("check")) if replace_existing.is_none() => ConnectAction::Check,
            Some(Some("check")) => return Err("check does not accept replace_existing".into()),
            _ => return Err("connection action must be connect or check".into()),
        };
        let mut identity = serde_json::Map::new();
        identity.insert(
            mapping.source_key_argument.clone(),
            Value::String(source_key.into()),
        );
        identity.insert(
            mapping.operation_id_argument.clone(),
            Value::String(operation_id.into()),
        );
        Ok(Self {
            identity,
            source_key: source_key.to_owned(),
            operation_id: operation_id.to_owned(),
            action,
        })
    }
}

#[derive(Clone, Copy, Default, Serialize)]
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
    event_bridge_uri: Option<String>,
    event_bridge_fallback_uri: Option<String>,
}

impl McpHostToolProvider {
    pub fn new(artifact: ArtifactClient, native: Option<Arc<dyn HostToolProvider>>) -> Self {
        Self {
            artifact: Arc::new(artifact),
            native,
            directory: RwLock::new(None),
            service_status: RwLock::new(ArtifactServiceStatus::default()),
            event_bridge_uri: None,
            event_bridge_fallback_uri: None,
        }
    }

    pub fn with_event_bridge(mut self, descriptor_uri: &str) -> Self {
        self.event_bridge_uri = Some(descriptor_uri.to_owned());
        self
    }

    /// Used only when the configured descriptor resource is reported missing; authentication,
    /// timeout and malformed descriptors still fail closed.
    pub fn with_event_bridge_fallback(mut self, descriptor_uri: &str) -> Self {
        self.event_bridge_fallback_uri = Some(descriptor_uri.to_owned());
        self
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

    fn is_mcp_name(name: &str) -> bool {
        name.starts_with("mcp__artifacts__")
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
            .find(|tool| {
                tool.definition.name == name
                    && !directory.bridge.as_ref().is_some_and(|bridge| {
                        bridge.tools.names().contains(&tool.remote_name.as_str())
                    })
            })?
            .clone();
        Some((tool, directory.fingerprint, directory.credential_key))
    }

    fn connection_mapping(&self, name: &str) -> Option<ConnectionTool> {
        let directory = self.directory.read().unwrap();
        let directory = directory.as_ref()?;
        let connection = directory.bridge.as_ref()?.connection.as_ref()?;
        directory
            .tools
            .iter()
            .any(|tool| tool.definition.name == name && tool.remote_name == connection.tool)
            .then(|| connection.clone())
    }

    async fn connect_directory(
        &self,
        credential: &CredentialSnapshot,
    ) -> Result<
        (
            Directory,
            RunningService<RoleClient, ClientConfig>,
            McpHttpClient,
        ),
        String,
    > {
        let endpoint = reqwest::Url::parse(&format!("{}/mcp", credential.origin))
            .map_err(|_| "invalid MCP endpoint")?;
        let http = McpHttpClient::new(endpoint.clone(), credential.access_token.clone())?;
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint.as_str().to_owned())
            .auth_header(credential.access_token.clone())
            .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(http.clone(), config);
        let service = match tokio::time::timeout(
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
            Ok(Ok(service)) => service,
            _ => {
                if http.unauthorized() {
                    self.invalidate(&credential.access_token).await;
                }
                return Err("MCP discovery unavailable".into());
            }
        };
        let protocol_version = service
            .peer()
            .peer_info()
            .ok_or("MCP discovery did not negotiate a protocol version")?
            .protocol_version
            .clone();
        let tools =
            match tokio::time::timeout(DIRECTORY_TIMEOUT, service.peer().list_all_tools()).await {
                Ok(Ok(tools)) => tools,
                _ => {
                    if http.unauthorized() {
                        self.invalidate(&credential.access_token).await;
                    }
                    return Err("MCP tool directory unavailable".into());
                }
            };
        let mut directory = self.validate_directory(tools, credential, protocol_version)?;
        if let Some(uri) = &self.event_bridge_uri {
            let mut read = Self::read_descriptor(service.peer(), uri).await;
            if matches!(read, DescriptorRead::Missing)
                && let Some(fallback) = &self.event_bridge_fallback_uri
            {
                read = Self::read_descriptor(service.peer(), fallback).await;
            }
            let text = match read {
                DescriptorRead::Text(text) => text,
                DescriptorRead::Missing => {
                    return Err("MCP event bridge descriptor unavailable".into());
                }
                DescriptorRead::Failed(error) => {
                    if http.unauthorized() {
                        self.invalidate(&credential.access_token).await;
                    }
                    return Err(error);
                }
            };
            directory.bridge = Some(Self::parse_bridge(&text, &directory)?);
        }
        if service
            .peer()
            .peer_info()
            .is_some_and(|info| info.capabilities.resources.is_some())
        {
            let resources =
                tokio::time::timeout(DIRECTORY_TIMEOUT, service.peer().list_all_resources())
                    .await
                    .map_err(|_| "MCP resource directory unavailable")?
                    .map_err(|_| "MCP resource directory unavailable")?;
            if resources
                .iter()
                .any(|resource| resource.uri == ARTIFACT_ATTACHMENT_URI)
            {
                let DescriptorRead::Text(text) =
                    Self::read_descriptor(service.peer(), ARTIFACT_ATTACHMENT_URI).await
                else {
                    return Err("MCP artifact attachment descriptor unavailable".into());
                };
                directory.attachments = Some(Self::parse_attachments(&text, &directory)?);
            }
        }
        Ok((directory, service, http))
    }

    async fn load_directory(&self, credential: &CredentialSnapshot) -> Result<Directory, String> {
        self.connect_directory(credential)
            .await
            .map(|(directory, _, _)| directory)
    }

    async fn read_descriptor(peer: &rmcp::Peer<rmcp::RoleClient>, uri: &str) -> DescriptorRead {
        let response = tokio::time::timeout(
            DIRECTORY_TIMEOUT,
            peer.read_resource_once(ReadResourceRequestParams::new(uri.to_owned())),
        )
        .await;
        let contents = match response {
            Ok(Ok(ReadResourceResponse::Complete(result))) => result.contents,
            Ok(Err(rmcp::ServiceError::McpError(error)))
                if (error.code == rmcp::model::ErrorCode::RESOURCE_NOT_FOUND
                    || error.code == rmcp::model::ErrorCode::INVALID_PARAMS)
                    && error
                        .data
                        .as_ref()
                        .and_then(|data| data.get("uri"))
                        .and_then(Value::as_str)
                        == Some(uri) =>
            {
                return DescriptorRead::Missing;
            }
            _ => {
                return DescriptorRead::Failed("MCP event bridge descriptor unavailable".into());
            }
        };
        let [
            ResourceContents::TextResourceContents {
                uri: returned_uri,
                mime_type,
                text,
                ..
            },
        ] = contents.as_slice()
        else {
            return DescriptorRead::Failed(
                "MCP event bridge descriptor must be one text resource".into(),
            );
        };
        if returned_uri != uri
            || mime_type.as_deref() != Some("application/json")
            || text.len() > MAX_DESCRIPTOR_BYTES
        {
            return DescriptorRead::Failed("MCP event bridge descriptor is invalid".into());
        }
        DescriptorRead::Text(text.clone())
    }

    fn parse_bridge(text: &str, directory: &Directory) -> Result<EventBridge, String> {
        const INVALID: &str = "MCP event bridge descriptor is invalid";
        let value: Value = serde_json::from_str(text).map_err(|_| INVALID)?;
        let (publication_tools, connection, tools, serialized) =
            match value.get("version").and_then(Value::as_u64) {
                Some(1) => {
                    let descriptor: EventBridgeDescriptor =
                        serde_json::from_value(value).map_err(|_| INVALID)?;
                    let serialized = serde_json::to_vec(&descriptor).map_err(|_| INVALID)?;
                    (
                        descriptor.publication_tools,
                        None,
                        descriptor.tools,
                        serialized,
                    )
                }
                Some(2) => {
                    let descriptor: EventBridgeDescriptorV2 =
                        serde_json::from_value(value).map_err(|_| INVALID)?;
                    let serialized = serde_json::to_vec(&descriptor).map_err(|_| INVALID)?;
                    (
                        descriptor.publication_tools,
                        Some(descriptor.connection_tool),
                        descriptor.tools,
                        serialized,
                    )
                }
                _ => {
                    return Err(
                        "MCP event bridge descriptor version or publication tools are invalid"
                            .into(),
                    );
                }
            };
        Self::validate_bridge(
            EventBridge {
                publication_tools,
                connection,
                tools,
                fingerprint: Sha256::digest(serialized).into(),
            },
            directory,
        )
    }

    async fn invalidate(&self, token: &str) {
        let _ = self.artifact.invalidate_credential(token).await;
        self.clear_directory();
    }

    fn validate_directory(
        &self,
        mut tools: Vec<Tool>,
        credential: &CredentialSnapshot,
        protocol_version: ProtocolVersion,
    ) -> Result<Directory, String> {
        if tools.len() > MAX_TOOLS {
            return Err("MCP tool directory is too large".into());
        }
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        let mut names = HashSet::new();
        let native_names = self
            .native
            .as_ref()
            .map(|native| {
                native
                    .definitions()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let reserved_names = crate::reserved_host_tool_names();
        let mut mapped = Vec::with_capacity(tools.len());
        for tool in &tools {
            let remote = tool.name.as_ref();
            if remote.is_empty()
                || remote.len() > 64
                || !remote
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                || !remote.as_bytes()[0].is_ascii_alphabetic()
                || !names.insert(remote.to_owned())
            {
                return Err("MCP tool directory contains an unsafe or duplicate name".into());
            }
            let name = format!("mcp__{SERVER_NAMESPACE}__{remote}");
            if native_names.contains(&name) || reserved_names.contains(&name) {
                return Err("MCP tool directory conflicts with an existing tool".into());
            }
            let schema = Value::Object(tool.input_schema.as_ref().clone());
            if schema["type"] != "object" || jsonschema::validator_for(&schema).is_err() {
                return Err("MCP tool directory contains an incompatible input schema".into());
            }
            let permission = if tool
                .annotations
                .as_ref()
                .and_then(|annotation| annotation.read_only_hint)
                == Some(true)
            {
                HostToolPermission::ReadOnly
            } else {
                HostToolPermission::Reviewable
            };
            mapped.push(DiscoveredTool {
                remote_name: remote.to_owned(),
                definition: ToolDefinition {
                    name,
                    description: tool
                        .description
                        .as_ref()
                        .map_or_else(|| remote.to_owned(), ToString::to_string),
                    input_schema: schema,
                },
                permission,
            });
        }
        let serialized =
            serde_json::to_vec(&tools).map_err(|_| "MCP tool directory cannot be fingerprinted")?;
        let fingerprint = Sha256::digest(serialized).into();
        Ok(Directory {
            credential_key: Self::credential_key(credential),
            fingerprint,
            protocol_version,
            tools: mapped,
            bridge: None,
            attachments: None,
        })
    }

    fn validate_bridge(
        descriptor: EventBridge,
        directory: &Directory,
    ) -> Result<EventBridge, String> {
        if descriptor.publication_tools.is_empty() || descriptor.publication_tools.len() > MAX_TOOLS
        {
            return Err(
                "MCP event bridge descriptor version or publication tools are invalid".into(),
            );
        }
        let tool_names: HashSet<&str> = directory
            .tools
            .iter()
            .map(|tool| tool.remote_name.as_str())
            .collect();
        let role_names = descriptor.tools.names();
        let role_set: HashSet<&str> = role_names.iter().copied().collect();
        if role_set.len() != role_names.len()
            || !role_set.iter().all(|name| tool_names.contains(name))
        {
            return Err(
                "MCP event bridge descriptor references missing or duplicate role tools".into(),
            );
        }
        let mut publication_set = HashSet::new();
        for publication in &descriptor.publication_tools {
            if !tool_names.contains(publication.tool.as_str())
                || role_set.contains(publication.tool.as_str())
                || !publication_set.insert(publication.tool.as_str())
                || [
                    &publication.operation_id_argument,
                    &publication.source_key_result,
                    &publication.version_key_result,
                ]
                .iter()
                .any(|key| !is_argument_key(key))
            {
                return Err("MCP event bridge publication mapping is invalid".into());
            }
        }
        if let Some(connection) = &descriptor.connection {
            let arguments = [
                connection.source_key_argument.as_str(),
                connection.operation_id_argument.as_str(),
                connection.action_argument.as_str(),
            ];
            let declared = directory
                .tools
                .iter()
                .find(|tool| tool.remote_name == connection.tool)
                .and_then(|tool| tool.definition.input_schema.get("properties"))
                .and_then(Value::as_object);
            if role_set.contains(connection.tool.as_str())
                || publication_set.contains(connection.tool.as_str())
                || arguments.iter().collect::<HashSet<_>>().len() != arguments.len()
                || arguments.contains(&CONNECTION_REPLACE_ARGUMENT)
                || !arguments.iter().all(|key| {
                    is_argument_key(key)
                        && declared.is_some_and(|properties| properties.contains_key(*key))
                })
            {
                return Err("MCP event bridge connection mapping is invalid".into());
            }
        }
        Ok(descriptor)
    }

    fn parse_attachments(text: &str, directory: &Directory) -> Result<ArtifactAttachments, String> {
        const INVALID: &str = "MCP artifact attachment descriptor is invalid";
        let descriptor: ArtifactAttachmentDescriptor =
            serde_json::from_str(text).map_err(|_| INVALID)?;
        if descriptor.version != 1
            || descriptor.creation_tools.is_empty()
            || descriptor.creation_tools.len() > MAX_TOOLS
        {
            return Err(INVALID.into());
        }
        let mut names = HashSet::new();
        for mapping in &descriptor.creation_tools {
            let arguments = [
                &mapping.operation_id_argument,
                &mapping.title_argument,
                &mapping.format_argument,
            ];
            let tool = directory
                .tools
                .iter()
                .find(|tool| tool.remote_name == mapping.tool)
                .ok_or(INVALID)?;
            let properties = tool
                .definition
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .ok_or(INVALID)?;
            if !names.insert(&mapping.tool)
                || arguments.iter().collect::<HashSet<_>>().len() != arguments.len()
                || !arguments
                    .iter()
                    .all(|key| is_argument_key(key) && properties.contains_key(key.as_str()))
                || !is_argument_key(&mapping.artifact_id_result)
                || !is_argument_key(&mapping.url_result)
                || directory.bridge.as_ref().is_some_and(|bridge| {
                    bridge.tools.names().contains(&mapping.tool.as_str())
                        || bridge
                            .publication_tools
                            .iter()
                            .any(|tool| tool.tool == mapping.tool)
                        || bridge
                            .connection
                            .as_ref()
                            .is_some_and(|tool| tool.tool == mapping.tool)
                })
            {
                return Err(INVALID.into());
            }
        }
        let fingerprint =
            Sha256::digest(serde_json::to_vec(&descriptor).map_err(|_| INVALID)?).into();
        Ok(ArtifactAttachments {
            creation_tools: descriptor.creation_tools,
            fingerprint,
        })
    }

    fn creation_effect(
        result: &rmcp::model::CallToolResult,
        prepared: &PreparedMcpCall,
    ) -> Option<tickloop_agent_protocol::ArtifactTick> {
        if result.is_error == Some(true) {
            return None;
        }
        let mapping = prepared.creation.as_ref()?;
        let structured = result.structured_content.as_ref()?;
        if structured.get("status")?.as_str()? != "committed"
            || bounded_identifier(structured.get("operation_id"))?
                != bounded_identifier(prepared.arguments.get(&mapping.operation_id_argument))?
        {
            return None;
        }
        let artifact_id = bounded_identifier(structured.get(&mapping.artifact_id_result))?;
        if artifact_id.contains(['/', '?', '#']) {
            return None;
        }
        let title = prepared.arguments.get(&mapping.title_argument)?.as_str()?;
        let format = prepared.arguments.get(&mapping.format_argument)?.as_str()?;
        if !["markdown", "html"].contains(&format) {
            return None;
        }
        let url = structured.get(&mapping.url_result)?.as_str()?;
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

    fn publication_effect(
        result: &rmcp::model::CallToolResult,
        publication: &PublicationTool,
        prepared: &PreparedMcpCall,
    ) -> Option<PublishedSourceEffect> {
        if result.is_error == Some(true) {
            return None;
        }
        let structured = result.structured_content.as_ref()?;
        if structured.get("status")?.as_str()? != "committed" {
            return None;
        }
        let operation_id = structured.get("operation_id")?.as_str()?;
        let expected_operation_id = prepared
            .arguments
            .get(&publication.operation_id_argument)?
            .as_str()?;
        if operation_id != expected_operation_id
            || operation_id.is_empty()
            || operation_id.len() > 256
        {
            return None;
        }
        let source_key = structured.get(&publication.source_key_result)?.as_str()?;
        let version_id = structured.get(&publication.version_key_result)?.as_str()?;
        if prepared
            .arguments
            .get(&publication.source_key_result)
            .is_some_and(|source| source.as_str() != Some(source_key))
            || source_key.is_empty()
            || source_key.len() > 256
            || version_id.is_empty()
            || version_id.len() > 256
        {
            return None;
        }
        Some(PublishedSourceEffect {
            server_id: prepared.credential.origin.clone(),
            owner_account_id: prepared.credential.account_id.clone(),
            source_key: source_key.to_owned(),
            operation_id: operation_id.to_owned(),
            version_id: version_id.to_owned(),
            credential_generation: Self::credential_key(&prepared.credential),
            directory_fingerprint: prepared.fingerprint,
            bridge_fingerprint: prepared.bridge_fingerprint?,
            artifact_tick: None,
        })
    }

    async fn lookup_association_operation(
        &self,
        credential: &CredentialSnapshot,
        request: &EventAssociationRequest,
        role_name: &str,
        cancel: &CancellationToken,
    ) -> Option<u64> {
        if cancel.is_cancelled() {
            return None;
        }
        let endpoint = reqwest::Url::parse(&format!("{}/mcp", credential.origin)).ok()?;
        let http = McpHttpClient::new(endpoint.clone(), credential.access_token.clone()).ok()?;
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint.as_str().to_owned())
            .auth_header(credential.access_token.clone())
            .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(http.clone(), config);
        let service = tokio::time::timeout(
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
        .ok()?
        .ok()?;
        if cancel.is_cancelled() || !self.credential_is_current(credential) {
            return None;
        }
        let input = json!({"operation_id":request.operation_id,"source_key":request.source_key});
        let params = CallToolRequestParams::new(role_name.to_owned())
            .with_arguments(input.as_object()?.clone());
        let response = tokio::select! {
            () = cancel.cancelled() => return None,
            result = tokio::time::timeout(CALL_TIMEOUT, service.peer().call_tool_once(params)) => result.ok()?.ok()?,
        };
        if http.unauthorized() {
            self.invalidate(&credential.access_token).await;
            return None;
        }
        let rmcp::model::CallToolResponse::Complete(result) = response else {
            return None;
        };
        if result.is_error == Some(true) {
            return None;
        }
        let structured = result.structured_content?;
        if structured.get("status")?.as_str()? != "committed"
            || structured.get("operation_id")?.as_str()? != request.operation_id
        {
            return None;
        }
        let committed = structured.get("result")?;
        if committed.get("source_key")?.as_str()? != request.source_key {
            return None;
        }
        let epoch = committed.get("association_epoch")?.as_u64()?;
        (1..=MAX_SAFE_EPOCH).contains(&epoch).then_some(epoch)
    }

    async fn call_event_role(
        &self,
        source: &EventSourceLocator,
        role: EventRole,
        input: Value,
        cancel: &CancellationToken,
    ) -> Result<rmcp::model::CallToolResult, EventRoleError> {
        if cancel.is_cancelled() {
            return Err(EventRoleError::NotDispatched(
                "event request cancelled".into(),
            ));
        }
        if !(1..=MAX_SAFE_EPOCH).contains(&source.association_epoch) {
            return Err(EventRoleError::NotDispatched(
                "invalid association epoch".into(),
            ));
        }
        let credential = match self.artifact.credential_snapshot() {
            Ok(Some(credential))
                if credential.origin == source.server_id
                    && credential.account_id == source.owner_account_id
                    && Self::credential_key(&credential) == source.credential_generation =>
            {
                credential
            }
            _ => {
                return Err(EventRoleError::NotDispatched(
                    "Artifact account changed".into(),
                ));
            }
        };
        let (directory, service, http) = self
            .connect_directory(&credential)
            .await
            .map_err(EventRoleError::NotDispatched)?;
        let bridge = directory
            .bridge
            .as_ref()
            .ok_or_else(|| EventRoleError::NotDispatched("MCP event bridge unavailable".into()))?;
        if directory.fingerprint != source.directory_fingerprint
            || bridge.fingerprint != source.bridge_fingerprint
        {
            return Err(EventRoleError::NotDispatched(
                "MCP event bridge changed".into(),
            ));
        }
        let role_name = match role {
            EventRole::Pending => &bridge.tools.pending,
            EventRole::Claim => &bridge.tools.claim,
            EventRole::Renew => &bridge.tools.renew,
            EventRole::Release => &bridge.tools.release,
        };
        let role_tool = directory
            .tools
            .iter()
            .find(|tool| &tool.remote_name == role_name)
            .ok_or_else(|| EventRoleError::NotDispatched("MCP event role unavailable".into()))?;
        if jsonschema::validator_for(&role_tool.definition.input_schema)
            .map_or(true, |validator| validator.validate(&input).is_err())
        {
            return Err(EventRoleError::NotDispatched(
                "MCP event role input rejected by schema".into(),
            ));
        }
        if cancel.is_cancelled() {
            return Err(EventRoleError::NotDispatched(
                "event request cancelled".into(),
            ));
        }
        if !self.credential_is_current(&credential) {
            return Err(EventRoleError::NotDispatched(
                "Artifact account changed before event dispatch".into(),
            ));
        }
        let params = CallToolRequestParams::new(role_name.clone())
            .with_arguments(input.as_object().expect("event input is object").clone());
        let outcome = tokio::select! {
            () = cancel.cancelled() => return Err(EventRoleError::OutcomeUnknown("event request cancelled after dispatch".into())),
            result = tokio::time::timeout(CALL_TIMEOUT, service.peer().call_tool_once(params)) => result,
        };
        if http.unauthorized() {
            self.invalidate(&credential.access_token).await;
        }
        let result = match outcome {
            Ok(Ok(rmcp::model::CallToolResponse::Complete(result))) => result,
            _ => {
                return Err(EventRoleError::OutcomeUnknown(
                    "MCP event response unavailable after dispatch".into(),
                ));
            }
        };
        if !self.credential_is_current(&credential) {
            return Err(EventRoleError::OutcomeUnknown(
                "Artifact account changed during event request".into(),
            ));
        }
        Ok(result)
    }

    async fn event_lease_call(
        &self,
        request: EventLeaseRequest,
        role: EventRole,
        cancel: CancellationToken,
    ) -> EventLeaseOutcome {
        let source = &request.source;
        let mut input = json!({
            "operation_id": request.operation_id,
            "source_key": source.source_key,
            "binding_id": source.binding_id.to_string(),
            "association_epoch": source.association_epoch,
            "event_id": request.event_id,
            "claim_id": request.claim_id,
            "fencing_token": request.fencing_token,
        });
        if matches!(role, EventRole::Release) {
            input["outcome"] = Value::String("needs-attention".into());
        }
        let result = match self.call_event_role(source, role, input, &cancel).await {
            Ok(result) if result.is_error != Some(true) => result,
            Ok(_) => {
                return EventLeaseOutcome::OutcomeUnknown("MCP claim lease request failed".into());
            }
            Err(EventRoleError::NotDispatched(error)) => {
                return EventLeaseOutcome::NotDispatched(error);
            }
            Err(EventRoleError::OutcomeUnknown(error)) => {
                return EventLeaseOutcome::OutcomeUnknown(error);
            }
        };
        let Some(data) = result.structured_content else {
            return EventLeaseOutcome::OutcomeUnknown(
                "MCP claim lease result is incomplete".into(),
            );
        };
        if data.get("status").and_then(Value::as_str) != Some("committed")
            || data.get("operation_id").and_then(Value::as_str)
                != Some(request.operation_id.as_str())
        {
            return EventLeaseOutcome::OutcomeUnknown(
                "MCP claim lease result did not match operation".into(),
            );
        }
        if let Some(status) = data.get("claim_status") {
            if status.as_str() == Some("completed")
                && data.get("source_key").and_then(Value::as_str)
                    == Some(source.source_key.as_str())
                && data.get("binding_id").and_then(Value::as_str)
                    == Some(source.binding_id.to_string().as_str())
                && data.get("association_epoch").and_then(Value::as_u64)
                    == Some(source.association_epoch)
                && data.get("event_id").and_then(Value::as_str) == Some(request.event_id.as_str())
                && data.get("claim_id").and_then(Value::as_str) == Some(request.claim_id.as_str())
                && data.get("lease_expires_at").is_none()
            {
                return EventLeaseOutcome::Completed;
            }
            return EventLeaseOutcome::OutcomeUnknown(
                "MCP terminal claim result did not match claim".into(),
            );
        }
        let expiry = data
            .get("lease_expires_at")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if matches!(role, EventRole::Renew)
            && expiry
                .as_deref()
                .is_none_or(|value| value.is_empty() || value.len() > 64)
        {
            return EventLeaseOutcome::OutcomeUnknown("MCP renewed lease expiry is invalid".into());
        }
        EventLeaseOutcome::Committed(expiry)
    }

    /// Serializes a result for the model and transcript after removing the private connection
    /// envelope and redacting the bearer token.
    fn model_result(
        mut result: rmcp::model::CallToolResult,
        access_token: &str,
        strip_metadata: bool,
    ) -> ToolResult {
        if strip_metadata {
            result.meta = None;
        } else if let Some(meta) = result.meta.as_mut() {
            meta.0.remove(CONNECT_ENVELOPE_META);
            if meta.0.is_empty() {
                result.meta = None;
            }
        }
        let content = serde_json::to_string(&result)
            .unwrap_or_else(|_| "MCP result could not be displayed".into())
            .replace(access_token, "[redacted]");
        ToolResult {
            content,
            is_error: result.is_error == Some(true),
            output_ref: None,
        }
    }

    /// Validates the private envelope of a successful connection read against the frozen request.
    fn connection_snapshot(
        result: &rmcp::model::CallToolResult,
        request: &ConnectionRequest,
    ) -> Option<(
        Option<RemoteAssociation>,
        Option<RemoteAssociationOperation>,
    )> {
        let (action, status) = match request.action {
            ConnectAction::Connect { .. } => ("connect", "prepared"),
            ConnectAction::Check => ("check", "checked"),
        };
        let structured = result.structured_content.as_ref()?.as_object()?;
        if structured.len() != request.identity.len() + 1
            || request
                .identity
                .iter()
                .any(|(key, value)| structured.get(key) != Some(value))
            || structured.get("connection_status")?.as_str()? != status
        {
            return None;
        }
        let envelope = result
            .meta
            .as_ref()?
            .0
            .get(CONNECT_ENVELOPE_META)?
            .as_object()?;
        if !has_exact_keys(
            envelope,
            &[
                "version",
                "action",
                "source_key",
                "operation_id",
                "head_version_id",
                "current_association",
                "association_operation",
            ],
        ) || envelope.get("version")?.as_u64()? != 1
            || envelope.get("action")?.as_str()? != action
            || envelope.get("source_key")?.as_str()? != request.source_key
            || envelope.get("operation_id")?.as_str()? != request.operation_id
        {
            return None;
        }
        if let head @ Value::String(_) = envelope.get("head_version_id")? {
            bounded_identifier(Some(head))?;
        } else if !envelope.get("head_version_id")?.is_null() {
            return None;
        }
        let current_association = match envelope.get("current_association")? {
            Value::Null => None,
            Value::Object(tuple)
                if has_exact_keys(
                    tuple,
                    &[
                        "binding_id",
                        "client_instance_id",
                        "chat_id",
                        "loop_id",
                        "association_epoch",
                    ],
                ) =>
            {
                Some(RemoteAssociation {
                    binding_id: bounded_identifier(tuple.get("binding_id"))?.to_owned(),
                    client_instance_id: bounded_identifier(tuple.get("client_instance_id"))?
                        .to_owned(),
                    chat_id: bounded_identifier(tuple.get("chat_id"))?.to_owned(),
                    loop_id: bounded_identifier(tuple.get("loop_id"))?.to_owned(),
                    association_epoch: safe_epoch(tuple.get("association_epoch"))?,
                })
            }
            _ => return None,
        };
        let association_operation = match envelope.get("association_operation")? {
            Value::Null => None,
            Value::Object(operation)
                if operation.contains_key("status")
                    && operation.keys().all(|key| {
                        matches!(
                            key.as_str(),
                            "status" | "source_key" | "association_epoch" | "error_code"
                        )
                    }) =>
            {
                if operation
                    .get("source_key")
                    .is_some_and(|source| source.as_str() != Some(request.source_key.as_str()))
                    || operation
                        .get("association_epoch")
                        .is_some_and(|epoch| safe_epoch(Some(epoch)).is_none())
                    || operation
                        .get("error_code")
                        .is_some_and(|code| bounded_identifier(Some(code)).is_none())
                {
                    return None;
                }
                Some(match operation.get("status")?.as_str()? {
                    "pending" => RemoteAssociationOperation::Pending,
                    "unknown" => RemoteAssociationOperation::Unknown,
                    "failed" => RemoteAssociationOperation::Failed {
                        error_code: bounded_identifier(operation.get("error_code"))
                            .map(ToOwned::to_owned),
                    },
                    "committed" if operation.contains_key("source_key") => {
                        RemoteAssociationOperation::Committed {
                            association_epoch: safe_epoch(operation.get("association_epoch"))?,
                        }
                    }
                    _ => return None,
                })
            }
            _ => return None,
        };
        Some((current_association, association_operation))
    }

    /// One owner-authority read of the connection tool with the v2 capability marker and never
    /// with claim metadata.
    async fn read_connection(
        &self,
        credential: &CredentialSnapshot,
        remote_name: &str,
        arguments: &Value,
        request: &ConnectionRequest,
        cancel: &CancellationToken,
    ) -> Result<ConnectionReply, String> {
        const UNAVAILABLE: &str = "Connection read was unavailable; nothing was associated. Report it instead of retrying automatically.";
        let endpoint = reqwest::Url::parse(&format!("{}/mcp", credential.origin))
            .map_err(|_| "invalid MCP endpoint")?;
        let http = McpHttpClient::new(endpoint.clone(), credential.access_token.clone())?;
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint.as_str().to_owned())
            .auth_header(credential.access_token.clone())
            .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(http.clone(), config);
        let service = tokio::time::timeout(
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
        .map_err(|_| UNAVAILABLE)?
        .map_err(|_| UNAVAILABLE)?;
        if cancel.is_cancelled() || !self.credential_is_current(credential) {
            return Err("Connection read cancelled or the Artifact account changed".into());
        }
        let mut params = CallToolRequestParams::new(remote_name.to_owned()).with_arguments(
            arguments
                .as_object()
                .ok_or("connection arguments must be an object")?
                .clone(),
        );
        let mut metadata = rmcp::model::RequestMetaObject::new();
        metadata.0.0.insert(
            CONNECT_CAPABILITY_META.into(),
            json!({"version": 1, "event_bridge_version": 2}),
        );
        params.meta = Some(metadata);
        let outcome = tokio::select! {
            () = cancel.cancelled() => return Err("Connection read cancelled".into()),
            result = tokio::time::timeout(CALL_TIMEOUT, service.peer().call_tool_once(params)) => result,
        };
        if http.unauthorized() {
            self.invalidate(&credential.access_token).await;
        }
        if !self.credential_is_current(credential) {
            return Err("Artifact account changed during the connection read".into());
        }
        let Ok(Ok(rmcp::model::CallToolResponse::Complete(result))) = outcome else {
            return Err(UNAVAILABLE.into());
        };
        if result.is_error == Some(true) {
            return Ok(ConnectionReply::Rejected(Self::model_result(
                result,
                &credential.access_token,
                true,
            )));
        }
        let (current_association, association_operation) = Self::connection_snapshot(
            &result, request,
        )
        .ok_or("Connection read returned an invalid private result; nothing was associated")?;
        Ok(ConnectionReply::Read(ConnectionSnapshot {
            result: Self::model_result(result, &credential.access_token, true),
            current_association,
            association_operation,
        }))
    }

    fn connect_effect(
        prepared: &PreparedMcpCall,
        request: &ConnectionRequest,
        snapshot: &ConnectionSnapshot,
    ) -> Option<ConnectSourceEffect> {
        Some(ConnectSourceEffect {
            result_identity: request.identity.clone(),
            server_id: prepared.credential.origin.clone(),
            owner_account_id: prepared.credential.account_id.clone(),
            source_key: request.source_key.clone(),
            operation_id: request.operation_id.clone(),
            action: request.action,
            current_association: snapshot.current_association.clone(),
            association_operation: snapshot.association_operation.clone(),
            credential_generation: Self::credential_key(&prepared.credential),
            directory_fingerprint: prepared.fingerprint,
            bridge_fingerprint: prepared.bridge_fingerprint?,
        })
    }

    fn connection_not_dispatched(request: &ConnectionRequest, reason: &str) -> HostCallResult {
        HostCallResult::NotDispatched(
            connection_result(
                &request.identity,
                "not_dispatched",
                json!({"reason": reason}),
                true,
                &format!("The connection was not dispatched: {reason}"),
            )
            .content,
        )
    }

    /// Connect consumes the read frozen at preparation without contacting the service again;
    /// check performs its one read now. Neither ever carries claim metadata.
    async fn execute_connection(
        &self,
        prepared: PreparedMcpCall,
        connection: PreparedConnection,
        has_claim_context: bool,
        cancel: CancellationToken,
    ) -> HostCallResult {
        let request = &connection.request;
        if cancel.is_cancelled() {
            return Self::connection_not_dispatched(
                request,
                "the call was cancelled before execution",
            );
        }
        if !self.credential_is_current(&prepared.credential) {
            return Self::connection_not_dispatched(
                request,
                "the Artifact account changed before execution",
            );
        }
        let snapshot = match (request.action, connection.snapshot) {
            (ConnectAction::Connect { .. }, _) if has_claim_context => {
                return Self::connection_not_dispatched(
                    request,
                    "a claimed feedback run cannot connect an artifact",
                );
            }
            (ConnectAction::Connect { .. }, Some(snapshot)) => snapshot,
            (ConnectAction::Connect { .. }, None) => {
                return Self::connection_not_dispatched(request, "the connection was not prepared");
            }
            (ConnectAction::Check, _) => match self
                .read_connection(
                    &prepared.credential,
                    &prepared.remote_name,
                    &prepared.arguments,
                    request,
                    &cancel,
                )
                .await
            {
                Ok(ConnectionReply::Read(snapshot)) => snapshot,
                Ok(ConnectionReply::Rejected(result)) => return HostCallResult::Completed(result),
                Err(error) => {
                    return HostCallResult::Completed(ToolResult {
                        content: error,
                        is_error: true,
                        output_ref: None,
                    });
                }
            },
        };
        match Self::connect_effect(&prepared, request, &snapshot) {
            Some(effect) => HostCallResult::CompletedWithEffect(
                snapshot.result,
                Box::new(HostEffect::ConnectSource(effect)),
            ),
            None => Self::connection_not_dispatched(request, "the event bridge is unavailable"),
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
            "source" | "body" | "content" | "markdown" | "html"
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
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .native
            .as_ref()
            .map(|provider| provider.definitions())
            .unwrap_or_default();
        if let Some(directory) = self.directory.read().unwrap().as_ref() {
            definitions.extend(
                directory
                    .tools
                    .iter()
                    .filter(|tool| {
                        !directory.bridge.as_ref().is_some_and(|bridge| {
                            bridge.tools.names().contains(&tool.remote_name.as_str())
                        })
                    })
                    .map(|tool| tool.definition.clone()),
            );
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

    fn tool_authorities(&self, name: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        if Self::is_mcp_name(name) {
            return Ok(self.mcp_authority()?.into_iter().collect());
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
        if Self::is_mcp_name(name) {
            return self.mcp_authority();
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
                        Err(_) => self.clear_directory(),
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
        *self.service_status.write().unwrap() = status;
        Ok(())
    }

    async fn associate_event_source(
        &self,
        request: EventAssociationRequest,
        cancel: CancellationToken,
    ) -> EventAssociationOutcome {
        if cancel.is_cancelled() {
            return EventAssociationOutcome::NotDispatched("association cancelled".into());
        }
        let credential = match self.artifact.credential_snapshot() {
            Ok(Some(credential))
                if credential.origin == request.server_id
                    && credential.account_id == request.owner_account_id
                    && Self::credential_key(&credential) == request.credential_generation =>
            {
                credential
            }
            _ => {
                return EventAssociationOutcome::NotDispatched(
                    "Artifact account changed before association".into(),
                );
            }
        };
        let directory = match self.load_directory(&credential).await {
            Ok(directory) => directory,
            Err(error) => return EventAssociationOutcome::NotDispatched(error),
        };
        let Some(bridge) = directory.bridge.as_ref() else {
            return EventAssociationOutcome::NotDispatched(
                "MCP event bridge is unavailable".into(),
            );
        };
        if directory.fingerprint != request.directory_fingerprint
            || bridge.fingerprint != request.bridge_fingerprint
        {
            return EventAssociationOutcome::NotDispatched(
                "MCP event bridge changed before association".into(),
            );
        }
        let role_name = &bridge.tools.associate;
        let Some(role_tool) = directory
            .tools
            .iter()
            .find(|tool| &tool.remote_name == role_name)
        else {
            return EventAssociationOutcome::NotDispatched(
                "MCP association role disappeared".into(),
            );
        };
        let mut input = json!({
            "operation_id": request.operation_id,
            "source_key": request.source_key,
            "binding_id": request.binding_id.to_string(),
            "client_instance_id": request.client_instance_id.to_string(),
            "chat_id": request.chat_id.to_string(),
            "loop_id": request.loop_id.to_string(),
        });
        if let Some(epoch) = request.expected_epoch {
            if !(1..=MAX_SAFE_EPOCH).contains(&epoch) {
                return EventAssociationOutcome::NotDispatched(
                    "invalid expected association epoch".into(),
                );
            }
            input["expected_epoch"] = json!(epoch);
        }
        if jsonschema::validator_for(&role_tool.definition.input_schema)
            .map_or(true, |validator| validator.validate(&input).is_err())
        {
            return EventAssociationOutcome::NotDispatched(
                "MCP association input rejected by directory schema".into(),
            );
        }
        let endpoint = match reqwest::Url::parse(&format!("{}/mcp", credential.origin)) {
            Ok(endpoint) => endpoint,
            Err(_) => return EventAssociationOutcome::NotDispatched("invalid MCP endpoint".into()),
        };
        let http = match McpHttpClient::new(endpoint.clone(), credential.access_token.clone()) {
            Ok(http) => http,
            Err(error) => return EventAssociationOutcome::NotDispatched(error),
        };
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint.as_str().to_owned())
            .auth_header(credential.access_token.clone())
            .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(http.clone(), config);
        let service = match tokio::time::timeout(
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
            Ok(Ok(service)) => service,
            _ => {
                if http.unauthorized() {
                    self.invalidate(&credential.access_token).await;
                }
                return EventAssociationOutcome::NotDispatched(
                    "MCP connection unavailable before association".into(),
                );
            }
        };
        if cancel.is_cancelled() {
            return EventAssociationOutcome::NotDispatched("association cancelled".into());
        }
        if !self.credential_is_current(&credential) {
            return EventAssociationOutcome::NotDispatched(
                "Artifact account changed before association dispatch".into(),
            );
        }
        let params = CallToolRequestParams::new(role_name.clone()).with_arguments(
            input
                .as_object()
                .expect("association input is object")
                .clone(),
        );
        let outcome = tokio::select! {
            () = cancel.cancelled() => return EventAssociationOutcome::OutcomeUnknown("association cancelled after dispatch".into()),
            result = tokio::time::timeout(CALL_TIMEOUT, service.peer().call_tool_once(params)) => result,
        };
        if http.unauthorized() {
            self.invalidate(&credential.access_token).await;
        }
        if !self.credential_is_current(&credential) {
            return EventAssociationOutcome::OutcomeUnknown(
                "Artifact account changed during association request".into(),
            );
        }
        let association = match outcome {
            Ok(Ok(rmcp::model::CallToolResponse::Complete(result)))
                if result.is_error == Some(true) =>
            {
                match result
                    .structured_content
                    .as_ref()
                    .and_then(|data| data.get("error"))
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_str)
                {
                    Some(code) if DEFINITE_ASSOCIATION_REJECTIONS.contains(&code) => {
                        EventAssociationOutcome::Rejected(code.to_owned())
                    }
                    _ => EventAssociationOutcome::OutcomeUnknown(
                        "association failed without a definite rejection; look up its operation ID"
                            .into(),
                    ),
                }
            }
            Ok(Ok(rmcp::model::CallToolResponse::Complete(result))) => {
                let Some(data) = result.structured_content else {
                    return EventAssociationOutcome::OutcomeUnknown(
                        "association returned no structured result".into(),
                    );
                };
                let epoch = data.get("association_epoch").and_then(Value::as_u64);
                if data.get("status").and_then(Value::as_str) == Some("committed")
                    && data.get("operation_id").and_then(Value::as_str)
                        == Some(request.operation_id.as_str())
                    && data.get("source_key").and_then(Value::as_str)
                        == Some(request.source_key.as_str())
                    && epoch.is_some_and(|epoch| (1..=MAX_SAFE_EPOCH).contains(&epoch))
                {
                    EventAssociationOutcome::Committed(epoch.expect("validated epoch"))
                } else {
                    EventAssociationOutcome::OutcomeUnknown(
                        "association result did not match the prepared operation".into(),
                    )
                }
            }
            _ => EventAssociationOutcome::OutcomeUnknown(
                "association response unavailable; look up its operation ID".into(),
            ),
        };
        if matches!(association, EventAssociationOutcome::OutcomeUnknown(_))
            && !cancel.is_cancelled()
            && let Some(epoch) = self
                .lookup_association_operation(
                    &credential,
                    &request,
                    &bridge.tools.operation,
                    &cancel,
                )
                .await
        {
            return EventAssociationOutcome::Committed(epoch);
        }
        association
    }

    async fn refresh_event_source(
        &self,
        mut source: EventSourceLocator,
    ) -> Result<EventSourceLocator, String> {
        let credential = self
            .artifact
            .credential_snapshot()
            .map_err(|_| "Artifact credential unavailable")?
            .ok_or("Artifact login is not active")?;
        if credential.origin != source.server_id || credential.account_id != source.owner_account_id
        {
            return Err("Artifact account differs from the saved feedback source".into());
        }
        let directory = self.load_directory(&credential).await?;
        let Some(bridge) = directory.bridge else {
            return Err("MCP event bridge unavailable".into());
        };
        source.credential_generation = Self::credential_key(&credential);
        source.directory_fingerprint = directory.fingerprint;
        source.bridge_fingerprint = bridge.fingerprint;
        Ok(source)
    }

    fn event_source_credentials_match(&self, source: &EventSourceLocator) -> bool {
        self.artifact
            .credential_snapshot()
            .ok()
            .flatten()
            .is_some_and(|credential| {
                credential.origin == source.server_id
                    && credential.account_id == source.owner_account_id
                    && Self::credential_key(&credential) == source.credential_generation
            })
    }

    async fn pending_event_source(
        &self,
        source: EventSourceLocator,
        cursor: Option<String>,
        cancel: CancellationToken,
    ) -> EventPendingOutcome {
        let mut input = json!({
            "source_key": source.source_key,
            "binding_id": source.binding_id.to_string(),
            "association_epoch": source.association_epoch,
        });
        if let Some(cursor) = cursor {
            input["cursor"] = Value::String(cursor);
        }
        let result = match self
            .call_event_role(&source, EventRole::Pending, input, &cancel)
            .await
        {
            Ok(result) if result.is_error != Some(true) => result,
            Ok(_) => {
                return EventPendingOutcome::Unavailable("MCP pending event request failed".into());
            }
            Err(EventRoleError::NotDispatched(error) | EventRoleError::OutcomeUnknown(error)) => {
                return EventPendingOutcome::Unavailable(error);
            }
        };
        let Some(structured) = result.structured_content else {
            return EventPendingOutcome::Unavailable("MCP pending result is incomplete".into());
        };
        let Some(events) = structured.get("events").and_then(Value::as_array) else {
            return EventPendingOutcome::Unavailable("MCP pending result is malformed".into());
        };
        if events.len() > 30 {
            return EventPendingOutcome::Unavailable("MCP pending page is too large".into());
        }
        let mut pending = Vec::with_capacity(events.len());
        for event in events {
            let Some(event_id) = event.get("event_id").and_then(Value::as_str) else {
                return EventPendingOutcome::Unavailable("MCP pending event lacks ID".into());
            };
            if event_id.is_empty()
                || event_id.len() > 256
                || event.get("source_key").and_then(Value::as_str)
                    != Some(source.source_key.as_str())
                || event.get("association_epoch").and_then(Value::as_u64)
                    != Some(source.association_epoch)
                || event.get("kind").and_then(Value::as_str) != Some("pending")
            {
                return EventPendingOutcome::Unavailable(
                    "MCP pending event does not match binding".into(),
                );
            }
            pending.push(PendingEvent {
                event_id: event_id.to_owned(),
            });
        }
        let cursor = structured
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        EventPendingOutcome::Events(pending, cursor)
    }

    async fn claim_event_source(
        &self,
        request: EventClaimRequest,
        cancel: CancellationToken,
    ) -> EventClaimOutcome {
        let source = &request.source;
        let input = json!({
            "operation_id": request.operation_id,
            "source_key": source.source_key,
            "binding_id": source.binding_id.to_string(),
            "association_epoch": source.association_epoch,
            "event_id": request.event_id,
            "client_instance_id": source.client_instance_id.to_string(),
        });
        let result = match self
            .call_event_role(source, EventRole::Claim, input, &cancel)
            .await
        {
            Ok(result) if result.is_error != Some(true) => result,
            Ok(_) => return EventClaimOutcome::NeedsAttention,
            Err(EventRoleError::NotDispatched(error)) => {
                return EventClaimOutcome::NotDispatched(error);
            }
            Err(EventRoleError::OutcomeUnknown(error)) => {
                return EventClaimOutcome::OutcomeUnknown(error);
            }
        };
        let Some(data) = result.structured_content else {
            return EventClaimOutcome::OutcomeUnknown("claim returned no structured result".into());
        };
        if data.get("status").and_then(Value::as_str) == Some("busy") {
            return EventClaimOutcome::Busy;
        }
        if data.get("status").and_then(Value::as_str) == Some("needs-attention") {
            return EventClaimOutcome::NeedsAttention;
        }
        if data.get("status").and_then(Value::as_str) != Some("committed")
            || data.get("operation_id").and_then(Value::as_str)
                != Some(request.operation_id.as_str())
        {
            return EventClaimOutcome::OutcomeUnknown(
                "claim result did not match operation".into(),
            );
        }
        let Some(claim) = data.get("claim") else {
            return EventClaimOutcome::OutcomeUnknown("claim result is incomplete".into());
        };
        let Some(event_id) = claim.get("event_id").and_then(Value::as_str) else {
            return EventClaimOutcome::OutcomeUnknown("claim event ID missing".into());
        };
        let Some(claim_id) = claim.get("claim_id").and_then(Value::as_str) else {
            return EventClaimOutcome::OutcomeUnknown("claim ID missing".into());
        };
        let Some(fencing_token) = claim.get("fencing_token").and_then(Value::as_str) else {
            return EventClaimOutcome::OutcomeUnknown("claim fencing token missing".into());
        };
        let Some(lease_expires_at) = claim.get("lease_expires_at").and_then(Value::as_str) else {
            return EventClaimOutcome::OutcomeUnknown("claim lease missing".into());
        };
        let Some(event_input) = claim.get("input").filter(|input| input.is_object()) else {
            return EventClaimOutcome::OutcomeUnknown("claim input missing".into());
        };
        if event_id != request.event_id
            || claim_id.is_empty()
            || claim_id.len() > 256
            || fencing_token.is_empty()
            || fencing_token.len() > 256
            || lease_expires_at.len() > 64
            || event_input.get("source_key").and_then(Value::as_str)
                != Some(source.source_key.as_str())
        {
            return EventClaimOutcome::OutcomeUnknown("claim identity does not match event".into());
        }
        EventClaimOutcome::Claimed(ClaimedEvent {
            event_id: event_id.to_owned(),
            claim_id: claim_id.to_owned(),
            fencing_token: fencing_token.to_owned(),
            lease_expires_at: lease_expires_at.to_owned(),
            input: event_input.clone(),
        })
    }

    async fn renew_event_claim(
        &self,
        request: EventLeaseRequest,
        cancel: CancellationToken,
    ) -> EventLeaseOutcome {
        self.event_lease_call(request, EventRole::Renew, cancel)
            .await
    }

    async fn release_event_claim(
        &self,
        request: EventLeaseRequest,
        cancel: CancellationToken,
    ) -> EventLeaseOutcome {
        self.event_lease_call(request, EventRole::Release, cancel)
            .await
    }

    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        if !Self::is_mcp_name(name) {
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
        if Self::is_mcp_name(name) {
            if let Some(mapping) = self.connection_mapping(name) {
                return match ConnectionRequest::parse(input, &mapping).map(|request| request.action)
                {
                    Ok(ConnectAction::Check) => HostToolPermission::ReadOnly,
                    _ => HostToolPermission::Reviewable,
                };
            }
            self.directory_tool(name)
                .map_or(HostToolPermission::Reviewable, |(tool, _, _)| {
                    tool.permission
                })
        } else {
            self.native
                .as_ref()
                .map_or(HostToolPermission::Reviewable, |native| {
                    native.permission(chat_id, name, input)
                })
        }
    }

    async fn prepare_call_in_context(
        &self,
        chat_id: ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if Self::is_mcp_name(name) {
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
        if !Self::is_mcp_name(name) {
            return self
                .native
                .as_ref()
                .ok_or_else(|| "Host tool unavailable".to_owned())?
                .prepare_call(chat_id, name, input, cancel)
                .await;
        }
        if cancel.is_cancelled() {
            return Err("MCP call cancelled".into());
        }
        let (
            tool,
            fingerprint,
            expected_key,
            bridge_fingerprint,
            attachment_fingerprint,
            creation,
            publication,
            connection,
        ) = {
            let directory = self.directory.read().unwrap();
            let directory = directory
                .as_ref()
                .ok_or("MCP tool is no longer available")?;
            let tool = directory
                .tools
                .iter()
                .find(|tool| {
                    tool.definition.name == name
                        && !directory.bridge.as_ref().is_some_and(|bridge| {
                            bridge.tools.names().contains(&tool.remote_name.as_str())
                        })
                })
                .ok_or("MCP tool is no longer available")?
                .clone();
            let bridge_fingerprint = directory.bridge.as_ref().map(|bridge| bridge.fingerprint);
            let publication = directory
                .bridge
                .as_ref()
                .and_then(|bridge| {
                    bridge
                        .publication_tools
                        .iter()
                        .find(|publication| publication.tool == tool.remote_name)
                })
                .cloned();
            let connection = directory
                .bridge
                .as_ref()
                .and_then(|bridge| bridge.connection.as_ref())
                .filter(|connection| connection.tool == tool.remote_name)
                .cloned();
            let creation = directory
                .attachments
                .as_ref()
                .and_then(|attachments| {
                    attachments
                        .creation_tools
                        .iter()
                        .find(|mapping| mapping.tool == tool.remote_name)
                })
                .cloned();
            (
                tool,
                directory.fingerprint,
                directory.credential_key,
                bridge_fingerprint,
                directory
                    .attachments
                    .as_ref()
                    .map(|attachments| attachments.fingerprint),
                creation,
                publication,
                connection,
            )
        };
        let credential = self
            .artifact
            .credential_snapshot()
            .map_err(|_| "Artifact credential unavailable")?
            .ok_or("Artifact login is no longer active")?;
        if Self::credential_key(&credential) != expected_key {
            return Err("Artifact account changed before MCP preparation".into());
        }
        if let Some(mapping) = connection {
            let request = ConnectionRequest::parse(input, &mapping)?;
            let approval_input = Self::approval_input(input)?;
            let (permission, summary, snapshot) = match request.action {
                ConnectAction::Check => (
                    HostToolPermission::ReadOnly,
                    format!(
                        "Check the connection of artifact {} at {}; read only",
                        request.source_key, credential.origin
                    ),
                    None,
                ),
                ConnectAction::Connect { replace_existing } => {
                    let snapshot = match self
                        .read_connection(&credential, &tool.remote_name, input, &request, &cancel)
                        .await?
                    {
                        ConnectionReply::Rejected(result) => return Err(result.content),
                        ConnectionReply::Read(snapshot) => snapshot,
                    };
                    let takeover = match (&snapshot.current_association, replace_existing) {
                        (Some(current), true) => format!(
                            "; this replaces the existing association at epoch {} and its running claims need attention",
                            current.association_epoch
                        ),
                        (Some(_), false) => "; an association already exists, so this only confirms one already held by this Chat or reports a conflict".into(),
                        (None, _) => String::new(),
                    };
                    (
                        HostToolPermission::Reviewable,
                        format!(
                            "Connect existing artifact {} at {} without changing, publishing or copying it; after approval, associate it with the current Loop and wait for owner-requested feedback{takeover}",
                            request.source_key, credential.origin
                        ),
                        Some(snapshot),
                    )
                }
            };
            return Ok(PreparedHostCall::new(
                name.to_owned(),
                input.clone(),
                approval_input,
                permission,
                summary,
            )
            .with_permission_input(input.clone())
            .with_payload(PreparedMcpCall {
                credential,
                fingerprint,
                bridge_fingerprint,
                attachment_fingerprint,
                creation: None,
                remote_name: tool.remote_name,
                arguments: input.clone(),
                publication: None,
                connection: Some(PreparedConnection { request, snapshot }),
            }));
        }
        let summary = if publication.is_some() {
            format!(
                "Call {name} at {}; on confirmed publication, associate a newly introduced source with the current Loop and wait for owner feedback; preserve any existing explicit connection and its listening state",
                credential.origin
            )
        } else {
            format!("Call {} at {}", name, credential.origin)
        };
        let approval_input = Self::approval_input(input)?;
        Ok(PreparedHostCall::new(
            name.to_owned(),
            input.clone(),
            approval_input,
            tool.permission,
            summary,
        )
        .with_permission_input(input.clone())
        .with_payload(PreparedMcpCall {
            credential,
            fingerprint,
            bridge_fingerprint,
            attachment_fingerprint,
            creation,
            remote_name: tool.remote_name,
            arguments: input.clone(),
            publication,
            connection: None,
        }))
    }

    async fn revalidate_call(
        &self,
        chat_id: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
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
            || directory.bridge.as_ref().map(|bridge| bridge.fingerprint)
                != prepared.bridge_fingerprint
            || directory
                .attachments
                .as_ref()
                .map(|attachments| attachments.fingerprint)
                != prepared.attachment_fingerprint
            || !directory.tools.iter().any(|tool| {
                tool.remote_name == prepared.remote_name && tool.definition.name == call.name()
            })
        {
            return Err("MCP tool directory changed before dispatch".into());
        }
        if let Some(context) = call.event_context()
            && (context.source.server_id != prepared.credential.origin
                || context.source.credential_generation
                    != Self::credential_key(&prepared.credential)
                || Some(context.source.bridge_fingerprint) != prepared.bridge_fingerprint)
        {
            return Err("MCP feedback claim no longer matches the prepared server".into());
        }
        if call.event_context().is_some()
            && prepared.connection.as_ref().is_some_and(|connection| {
                matches!(connection.request.action, ConnectAction::Connect { .. })
            })
        {
            return Err("A claimed feedback run cannot connect an artifact".into());
        }
        Ok(())
    }

    async fn execute_prepared(
        &self,
        chat_id: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if call.payload::<PreparedMcpCall>().is_none() {
            return match &self.native {
                Some(native) => native.execute_prepared(chat_id, call, cancel).await,
                None => HostCallResult::NotDispatched("Host tool unavailable".into()),
            };
        }
        let event_context = call.event_context().cloned();
        let Ok(mut prepared) = call.into_payload::<PreparedMcpCall>() else {
            unreachable!()
        };
        if let Some(connection) = prepared.connection.take() {
            return self
                .execute_connection(prepared, connection, event_context.is_some(), cancel)
                .await;
        }
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched("MCP call cancelled".into());
        }
        let current = match self.artifact.credential_snapshot() {
            Ok(Some(current)) => current,
            _ => {
                return HostCallResult::NotDispatched(
                    "Artifact login ended before MCP dispatch".into(),
                );
            }
        };
        if !prepared.credential.identity_matches(&current) {
            return HostCallResult::NotDispatched(
                "Artifact account changed before MCP dispatch".into(),
            );
        }
        let endpoint = match reqwest::Url::parse(&format!("{}/mcp", prepared.credential.origin)) {
            Ok(endpoint) => endpoint,
            Err(_) => return HostCallResult::NotDispatched("invalid MCP endpoint".into()),
        };
        let http =
            match McpHttpClient::new(endpoint.clone(), prepared.credential.access_token.clone()) {
                Ok(http) => http,
                Err(error) => return HostCallResult::NotDispatched(error),
            };
        let config = StreamableHttpClientTransportConfig::with_uri(endpoint.as_str().to_owned())
            .auth_header(prepared.credential.access_token.clone())
            .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(http.clone(), config);
        let service = match tokio::time::timeout(
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
            Ok(Ok(service)) => service,
            _ => {
                if http.unauthorized() {
                    self.invalidate(&prepared.credential.access_token).await;
                }
                return HostCallResult::NotDispatched(
                    "MCP connection unavailable before tool dispatch".into(),
                );
            }
        };
        let arguments = match prepared.arguments.as_object() {
            Some(arguments) => arguments.clone(),
            None => return HostCallResult::NotDispatched("MCP arguments must be an object".into()),
        };
        let mut params =
            CallToolRequestParams::new(prepared.remote_name.clone()).with_arguments(arguments);
        if let Some(context) = &event_context {
            let mut metadata = rmcp::model::RequestMetaObject::new();
            metadata.0.0.insert(
                "tickloop.ai/event-bridge".into(),
                json!({
                    "version": 1,
                    "source_key": context.source.source_key,
                    "binding_id": context.source.binding_id.to_string(),
                    "association_epoch": context.source.association_epoch,
                    "event_id": context.event_id,
                    "claim_id": context.claim_id,
                    "fencing_token": context.fencing_token,
                }),
            );
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
            () = cancel.cancelled() => return HostCallResult::OutcomeUnknown("MCP call was cancelled after dispatch; query its operation ID".into()),
            result = tokio::time::timeout(CALL_TIMEOUT, service.peer().call_tool_once(params)) => result,
        };
        if http.unauthorized() {
            self.invalidate(&prepared.credential.access_token).await;
        }
        if !self.credential_is_current(&prepared.credential) {
            return HostCallResult::OutcomeUnknown(
                "Artifact account changed after MCP dispatch; query its operation ID".into(),
            );
        }
        match outcome {
            Ok(Ok(rmcp::model::CallToolResponse::Complete(result))) => {
                let creation = Self::creation_effect(&result, &prepared);
                let mut effect = prepared.publication.as_ref().and_then(|publication| {
                    Self::publication_effect(&result, publication, &prepared)
                });
                if let Some(effect) = &mut effect {
                    let metadata = tokio::select! {
                        _ = cancel.cancelled() => None,
                        metadata = self.artifact.read(&effect.source_key) => metadata.ok(),
                    };
                    if let Some(metadata) = metadata {
                        effect.artifact_tick = Some(metadata.tick(
                            &prepared.credential.origin,
                            tickloop_agent_base::private_fs::unix_time_milliseconds(),
                        ));
                    }
                    if !self.credential_is_current(&prepared.credential) {
                        return HostCallResult::OutcomeUnknown(
                            "Artifact account changed after publication; query its operation ID"
                                .into(),
                        );
                    }
                }
                let mut tool_result =
                    Self::model_result(result, &prepared.credential.access_token, false);
                if let Some(context) = &event_context {
                    tool_result.content = tool_result
                        .content
                        .replace(&context.fencing_token, "[redacted]");
                }
                match effect {
                    Some(effect) => HostCallResult::CompletedWithEffect(
                        tool_result,
                        Box::new(HostEffect::PublishedSource(effect)),
                    ),
                    None => match creation {
                        Some(tick) => HostCallResult::CompletedWithEffect(
                            tool_result,
                            Box::new(HostEffect::CreatedArtifact(tick)),
                        ),
                        None => HostCallResult::Completed(tool_result),
                    },
                }
            }
            Ok(Ok(_)) => HostCallResult::OutcomeUnknown(
                "MCP call did not return a final result; query its operation ID".into(),
            ),
            _ => HostCallResult::OutcomeUnknown(
                "MCP response unavailable after dispatch; query its operation ID".into(),
            ),
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
            Some(native) if !Self::is_mcp_name(name) => {
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
