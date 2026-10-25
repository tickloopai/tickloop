//! GitHub Copilot model service: the request identity and replay rules that differ from the
//! plain Anthropic Messages and OpenAI Responses adapters. Authentication lives in
//! `auth::copilot`; everything here works on the projected or already-encoded request.

use std::{collections::BTreeMap, path::Path};

use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderName, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    ModelError, ModelRequest, ModelRequestStage,
    availability::{AccountDirectoryAdapter, DiscoveryIdentity},
    credential_fingerprint,
};
use crate::auth::copilot::{CopilotAccess, CopilotAuth, CopilotAuthError};
use tickloop_harness_base::network::{HttpPolicy, build_http_client, is_loopback};
use tickloop_harness_protocol::{
    AccountModelDirectory, Content, DirectorySemantics, ModelAvailability, ModelConfig, Protocol,
};

/// pi's default plan host; a Copilot token's `proxy-ep` names the account's own host.
pub const COPILOT_API_URL: &str = "https://api.individual.githubcopilot.com";
pub(crate) const COPILOT_PROVIDER_ID: &str = "github-copilot";
/// Sent only on `/models`, as pi does.
pub const GITHUB_API_VERSION: &str = "2026-06-01";
pub const CLIENT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
/// The VS Code Copilot Chat identity pi sends on the token exchange and every Copilot request.
pub const CLIENT_HEADERS: [(&str, &str); 4] = [
    ("user-agent", CLIENT_USER_AGENT),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
];
const API_VERSION_HEADER: HeaderName = HeaderName::from_static("x-github-api-version");

/// The configured service address, or a loopback HTTP test server in debug builds.
pub(crate) fn is_allowed_endpoint(endpoint: &str) -> bool {
    if endpoint.trim_end_matches('/') == COPILOT_API_URL {
        return true;
    }
    cfg!(debug_assertions)
        && reqwest::Url::parse(endpoint)
            .is_ok_and(|url| url.scheme() == "http" && is_loopback(&url))
}

pub(crate) fn credential_error(error: CopilotAuthError) -> ModelError {
    match error {
        CopilotAuthError::Cancelled => ModelError::Cancelled,
        CopilotAuthError::NotLoggedIn => ModelError::CopilotNotLoggedIn,
        CopilotAuthError::RefreshFailed => ModelError::CopilotAuthentication,
        CopilotAuthError::NoCopilotAccess => {
            entitlement_denied(403).at_stage(ModelRequestStage::Credentials)
        }
        CopilotAuthError::BudgetExhausted => ModelError::RequestBudgetExhausted {
            stage: ModelRequestStage::Credentials,
        },
        CopilotAuthError::Network => ModelError::ConnectionAt {
            stage: ModelRequestStage::Credentials,
        },
        CopilotAuthError::Http(status) => {
            ModelError::Http { status }.at_stage(ModelRequestStage::Credentials)
        }
        other => ModelError::Configuration(other.to_string()),
    }
}

/// Copilot binds encrypted reasoning and its item ids to the upstream connection, whose token
/// rotates, so even same-source Responses reasoning is not replayed. The canonical history keeps
/// it; only this request copy drops it.
pub(crate) fn without_connection_bound_reasoning(mut request: ModelRequest) -> ModelRequest {
    let has_sources = request.history_sources.len() == request.messages.len();
    let mut sources = std::mem::take(&mut request.history_sources).into_iter();
    let mut kept_sources = Vec::new();
    request.messages.retain_mut(|message| {
        let source = sources.next();
        message.content.retain(|content| {
            !matches!(
                content,
                Content::ProtocolData { protocol: Protocol::Responses, value }
                    if value["type"] == "reasoning"
            )
        });
        let keep = !message.content.is_empty();
        if keep && let Some(source) = source {
            kept_sources.push(source);
        }
        keep
    });
    if has_sources {
        request.history_sources = kept_sources;
    }
    request
}

/// pi's client identity plus headers describing the request. The initiator follows GitHub's
/// billing rule that only prompts the user sends count as premium requests: a request ending in
/// the user's own message is `user`; tool continuations are `agent`.
pub(crate) fn request_headers(body: &Value) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = client_headers();
    headers.extend([
        (
            HeaderName::from_static("openai-intent"),
            HeaderValue::from_static("conversation-edits"),
        ),
        (
            HeaderName::from_static("x-initiator"),
            HeaderValue::from_static(if ends_with_user_prompt(body) {
                "user"
            } else {
                "agent"
            }),
        ),
    ]);
    if contains_image(&body["messages"]) || contains_image(&body["input"]) {
        headers.push((
            HeaderName::from_static("copilot-vision-request"),
            HeaderValue::from_static("true"),
        ));
    }
    headers
}

fn client_headers() -> Vec<(HeaderName, HeaderValue)> {
    CLIENT_HEADERS
        .iter()
        .map(|(name, value)| {
            (
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            )
        })
        .collect()
}

fn ends_with_user_prompt(body: &Value) -> bool {
    let last = body["messages"]
        .as_array()
        .or_else(|| body["input"].as_array())
        .and_then(|items| items.last());
    let Some(last) = last else {
        return true;
    };
    if last["role"] != "user" {
        return false;
    }
    match &last["content"] {
        Value::Array(parts) => parts.iter().any(|part| part["type"] != "tool_result"),
        _ => true,
    }
}

fn contains_image(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_image),
        Value::Object(object) => {
            matches!(
                object.get("type").and_then(Value::as_str),
                Some("image" | "input_image")
            ) || object.values().any(contains_image)
        }
        _ => false,
    }
}

/// GitHub refuses a Copilot request when the plan, organization policy or client may not use
/// the model; the remote body is not shown.
pub(crate) fn entitlement_denied(status: u16) -> ModelError {
    ModelError::EntitlementDenied {
        status,
        description: ": GitHub Copilot does not allow this model for your plan, organization policy or client".into(),
    }
}

/// Copilot answers a model the account's plan or policy does not enable with HTTP 400
/// `model_not_supported`; the account directory marks such models `disabled`.
pub(crate) fn is_model_not_enabled(status: u16, body: &Value) -> bool {
    status == 400 && body["error"]["code"] == "model_not_supported"
}

/// Copilot reports prompt overflow as `prompt token count of N exceeds the limit of M`.
pub(crate) fn is_context_limit(body: &Value) -> bool {
    body["error"]["message"].as_str().is_some_and(|message| {
        let message = message.to_ascii_lowercase();
        message.contains("prompt token count") && message.contains("exceeds the limit")
    })
}

const MAX_DIRECTORY_BYTES: usize = 1024 * 1024;
const MAX_DIRECTORY_MODELS: usize = 10_000;
const MAX_MODEL_ID_BYTES: usize = 512;

/// Copilot's `/models` lists what this account and its client may use, with explicit policy
/// states. GitHub documents neither its completeness nor exclusivity, so an absent model stays
/// unverified rather than refused.
pub(crate) struct CopilotDirectoryAdapter<'a> {
    pub(crate) config: &'a ModelConfig,
    pub(crate) root: Option<&'a Path>,
}

impl CopilotDirectoryAdapter<'_> {
    async fn access(&self, cancel: &CancellationToken) -> Result<CopilotAccess, String> {
        if !is_allowed_endpoint(&self.config.base_url) {
            return Err("GitHub Copilot discovery requires the Copilot API origin".into());
        }
        let root = self.root.ok_or("Agent state directory is required")?;
        CopilotAuth::new(root)
            .resolve_access(cancel)
            .await
            .map_err(|error| match error {
                CopilotAuthError::NotLoggedIn => "Sign in to GitHub Copilot first".to_owned(),
                error => error.to_string(),
            })
    }

    fn endpoint(&self, access: &CopilotAccess) -> Result<Url, String> {
        let base = access.api_base.as_deref().unwrap_or(&self.config.base_url);
        let mut endpoint = Url::parse(base).map_err(|_| "Invalid model discovery endpoint")?;
        endpoint.set_path(&format!("{}/models", endpoint.path().trim_end_matches('/')));
        Ok(endpoint)
    }
}

#[async_trait::async_trait]
impl AccountDirectoryAdapter for CopilotDirectoryAdapter<'_> {
    async fn identity(&self, cancel: &CancellationToken) -> Result<DiscoveryIdentity, String> {
        let access = self.access(cancel).await?;
        let endpoint = self.endpoint(&access)?;
        Ok(DiscoveryIdentity {
            provider: self.config.provider_id.clone(),
            origin: endpoint.origin().ascii_serialization(),
            resource: credential_fingerprint(endpoint.as_str().as_bytes()),
            account: access.identity.to_string(),
        })
    }

    async fn fetch(&self, cancel: &CancellationToken) -> Result<AccountModelDirectory, String> {
        let access = self.access(cancel).await?;
        let endpoint = self.endpoint(&access)?;
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", access.token))
            .map_err(|_| "Invalid discovery credentials")?;
        bearer.set_sensitive(true);
        // `AccountModelDiscovery` bounds the whole discovery, this request included.
        let (client, _) = build_http_client(&endpoint, HttpPolicy::new(Policy::none()))
            .map_err(|_| "Could not create discovery HTTP client")?;
        let mut response = client
            .get(endpoint)
            .headers(client_headers().into_iter().collect())
            .header(AUTHORIZATION, bearer)
            .header(API_VERSION_HEADER, GITHUB_API_VERSION)
            .send()
            .await
            .map_err(|_| "Model discovery connection failed")?;
        if !response.status().is_success() {
            return Err(format!(
                "Model discovery failed (HTTP {}); login is unchanged",
                response.status().as_u16()
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "Model discovery response interrupted")?
        {
            if body.len().saturating_add(chunk.len()) > MAX_DIRECTORY_BYTES {
                return Err("Model directory exceeds the response size limit".into());
            }
            body.extend_from_slice(&chunk);
        }
        parse_directory(&body)
    }
}

#[derive(Deserialize)]
struct Models {
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
    #[serde(default)]
    policy: Option<ModelPolicy>,
    #[serde(default)]
    capabilities: Option<ModelCapabilities>,
}

#[derive(Deserialize)]
struct ModelPolicy {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
struct ModelCapabilities {
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

fn parse_directory(body: &[u8]) -> Result<AccountModelDirectory, String> {
    let response: Models =
        serde_json::from_slice(body).map_err(|_| "Invalid GitHub Copilot model directory")?;
    if response.data.len() > MAX_DIRECTORY_MODELS
        || response
            .data
            .iter()
            .any(|model| model.id.trim().is_empty() || model.id.len() > MAX_MODEL_ID_BYTES)
    {
        return Err("Invalid model directory entries".into());
    }
    let mut models = BTreeMap::new();
    for model in response.data {
        let chat = model
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.kind.as_deref())
            .is_none_or(|kind| kind == "chat");
        if !chat {
            continue;
        }
        let availability = match model.policy.and_then(|policy| policy.state).as_deref() {
            Some("disabled") => ModelAvailability::Unavailable {
                reason: "Disabled by your Copilot plan or organization policy".into(),
            },
            Some("unconfigured") => ModelAvailability::Unknown {
                reason: "Not enabled yet in your GitHub Copilot settings".into(),
            },
            _ => ModelAvailability::DirectoryListed,
        };
        if models.insert(model.id, availability).is_some() {
            return Err("Duplicate model directory entry".into());
        }
    }
    Ok(AccountModelDirectory {
        models,
        semantics: DirectorySemantics::Directory,
        complete: true,
    })
}
