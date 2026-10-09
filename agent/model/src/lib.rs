use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use tickloop_agent_base::network::{
    HttpPolicy, NetworkDiagnostic, NetworkRoute, build_http_client, is_loopback,
};
use tickloop_agent_protocol::{
    Adapter, Content, Message, ModelAuthSource, ModelConfig, ModelDiscovery, ModelIdentity,
    ModelProvider, PrivateHistorySource, Protocol, ProviderConnection, ProviderFailureClass,
    ProviderHttpFailure, ReasoningEffort, Role, ThinkingControl, ToolDefinition, ToolResultBlock,
    ToolResultContent, Usage,
};

pub mod access;
pub mod auth;
pub mod availability;
pub mod cards;
mod chat_completions;
mod generation_budget;
pub mod judgment;
pub use generation_budget::GenerationBudget;
mod codex;
#[cfg(any(test, feature = "test-support"))]
pub mod codex_test_support;
pub mod connection;
pub mod copilot;
pub mod credentials;
mod glm;
mod google;
pub mod history;
mod image_limits;
mod kimi;
mod provider_directory;
mod provider_failure;
mod responses;
mod structured_response;

use crate::provider_failure::ProviderHttpFailureExt;
pub use image_limits::strip_images;

const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_STREAM_BYTES: usize = 64 * 1024 * 1024;
const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

/// Model-card and endpoint lookups for a [`ModelConfig`]; the data lives in the protocol crate.
/// Tokens a request needs beyond its messages and output: system prompt framing and tool schemas.
pub const REQUEST_OVERHEAD_TOKENS: usize = 2048;

pub trait ModelConfigExt {
    fn connection(&self) -> Option<&ProviderConnection>;
    fn private_history_source(&self) -> Option<PrivateHistorySource>;
    fn thinking_enabled(&self, effort: Option<ReasoningEffort>, enabled: bool) -> Option<bool>;
    fn maximum_context_window_tokens(&self) -> Option<usize>;
    /// Whether a context window of `tokens` leaves room for the output and request overhead.
    fn is_usable_context_window(&self, tokens: usize) -> bool;
}

impl ModelConfigExt for ModelConfig {
    fn connection(&self) -> Option<&ProviderConnection> {
        self.connection
            .as_ref()
            .or_else(|| cards::provider_connection(&self.provider_id))
    }

    fn private_history_source(&self) -> Option<PrivateHistorySource> {
        let mut endpoint = reqwest::Url::parse(&self.base_url).ok()?;
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return None;
        }
        let path = endpoint.path().trim_end_matches('/').to_owned();
        endpoint.set_path(&path);
        let fingerprint: [u8; 32] = Sha256::digest(endpoint.as_str().as_bytes()).into();
        Some(PrivateHistorySource {
            provider_id: self.provider_id.clone(),
            endpoint_fingerprint: fingerprint
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            adapter: self.adapter(),
            service_model_id: self.model.clone(),
            auth_source: self.auth_source,
            credential_reference: self.api_key_env.clone(),
            credential_identity: None,
        })
    }

    fn thinking_enabled(&self, effort: Option<ReasoningEffort>, enabled: bool) -> Option<bool> {
        let card = cards::model_card(self)?;
        match card.thinking {
            ThinkingControl::Unsupported => None,
            ThinkingControl::AlwaysOn => Some(true),
            ThinkingControl::Toggle => Some(
                enabled
                    && effort != Some(ReasoningEffort::Off)
                    && (self.adapter() != Adapter::MinimaxMessages
                        || effort.is_some()
                        || card.adaptive_by_default),
            ),
        }
    }

    fn maximum_context_window_tokens(&self) -> Option<usize> {
        cards::model_card(self).and_then(|card| card.context_window_tokens)
    }

    fn is_usable_context_window(&self, tokens: usize) -> bool {
        tokens > self.max_output_tokens as usize + REQUEST_OVERHEAD_TOKENS
            && self
                .maximum_context_window_tokens()
                .is_none_or(|maximum| tokens <= maximum)
    }
}

pub(crate) fn credential_fingerprint(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn compatible_private_history(current: &ModelConfig, selected: &ModelConfig) -> bool {
    match (
        current.private_history_source(),
        selected.private_history_source(),
    ) {
        (Some(current), Some(selected)) => current == selected,
        (None, None) => {
            current.provider == selected.provider
                && current.provider_id == selected.provider_id
                && current.auth_source == selected.auth_source
                && current.api_key_env == selected.api_key_env
                && current.protocol == selected.protocol
                && current.base_url.trim_end_matches('/') == selected.base_url.trim_end_matches('/')
                && current.model == selected.model
        }
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructuredOutput {
    pub name: String,
    pub schema: Value,
}

#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub history_sources: Vec<Option<PrivateHistorySource>>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub structured_output: Option<StructuredOutput>,
    /// Sampling temperature. The Responses adapter omits it because the
    /// reasoning models served there reject sampling parameters.
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// The reasoning efforts a model's card offers.
pub trait ReasoningEffortExt: Sized {
    fn choices(config: &ModelConfig) -> &[Self];
}

impl ReasoningEffortExt for ReasoningEffort {
    fn choices(config: &ModelConfig) -> &[Self] {
        cards::model_card(config).map_or(&[], |card| card.effort_levels.as_slice())
    }
}

pub fn validate_reasoning_effort(
    config: &ModelConfig,
    effort: Option<ReasoningEffort>,
) -> Result<(), ModelError> {
    let is_supported = effort.is_none_or(|effort| match effort {
        ReasoningEffort::On => {
            matches!(
                config.adapter(),
                Adapter::MinimaxMessages | Adapter::QwenChatCompletions
            ) && cards::model_card(config)
                .is_some_and(|card| card.thinking == ThinkingControl::Toggle)
        }
        ReasoningEffort::Off => {
            cards::model_card(config).is_some_and(|card| card.thinking == ThinkingControl::Toggle)
        }
        _ => ReasoningEffort::choices(config).contains(&effort),
    });
    if !is_supported {
        return Err(ModelError::Configuration(format!(
            "reasoning effort is unsupported by {}",
            config.model
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_source: Option<PrivateHistorySource>,
    pub content: Vec<Content>,
    pub usage: Usage,
    pub stop_reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelDelta {
    Text(String),
    Thinking(String),
    OutputStarted,
    /// Absolute service-reported counters for this request, never an additive delta.
    Usage(Usage),
}

#[derive(Clone)]
pub struct ModelRequestContext {
    used_attempts: Arc<AtomicUsize>,
    max_attempts: usize,
}

impl ModelRequestContext {
    pub fn new(max_attempts: usize) -> Self {
        Self {
            used_attempts: Arc::new(AtomicUsize::new(0)),
            max_attempts,
        }
    }

    pub fn try_begin_attempt(&self) -> bool {
        self.used_attempts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.max_attempts).then_some(used + 1)
            })
            .is_ok()
    }

    pub fn used_attempts(&self) -> usize {
        self.used_attempts.load(Ordering::SeqCst)
    }

    pub fn has_remaining_attempts(&self) -> bool {
        self.used_attempts() < self.max_attempts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ModelResponseRejection {
    #[error("response reached its output token limit and is incomplete")]
    OutputLimit,
    #[error("response reached the model context limit and is incomplete")]
    ContextLimit,
    #[error("response was interrupted by the service content filter")]
    ContentFilter,
    #[error("model refused the requested response")]
    Refusal,
    #[error("model paused its turn; automatic continuation is unsupported")]
    UnsupportedContinuation,
    #[error("model response contains an invalid tool call")]
    InvalidToolCall,
    #[error("structured response contains unexpected tool operations")]
    UnexpectedToolOperation,
    #[error("verified structured response contains a malformed result")]
    MalformedStructuredOutput,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("{0}")]
    ProviderHttp(Box<ProviderHttpFailure>),
    #[error("invalid model configuration: {0}")]
    Configuration(String),
    #[error("model protocol {0:?} is not implemented yet")]
    UnsupportedProtocol(Protocol),
    #[error(
        "model authentication failed; review this connection in /provider → Connection Details"
    )]
    Authentication,
    #[error("model authentication failed during {stage:?}")]
    AuthenticationAt { stage: ModelRequestStage },
    #[error("Codex subscription is not logged in; use /provider")]
    CodexNotLoggedIn,
    #[error("MiniMax is not signed in; open its OAuth connection in /provider")]
    MiniMaxNotLoggedIn,
    #[error(
        "MiniMax credentials need renewed authorization; open its Connection Details in /provider and choose Sign in again"
    )]
    MiniMaxAuthentication,
    #[error("GitHub Copilot is not signed in; open its connection in /provider")]
    CopilotNotLoggedIn,
    #[error(
        "GitHub Copilot sign-in needs renewed authorization; open Connection Details in /provider and choose Sign in again"
    )]
    CopilotAuthentication,
    #[error(
        "Codex subscription authentication failed; open Codex Connection Details in /provider and choose Sign in again"
    )]
    CodexAuthentication,
    #[error("model service rejected the request because the account balance is insufficient")]
    InsufficientBalance,
    #[error("model service denied access (HTTP {status}){description}")]
    EntitlementDenied { status: u16, description: String },
    #[error("model allowance is exhausted (HTTP {status}){description}")]
    QuotaExhausted { status: u16, description: String },
    #[error("model service rate limited the request; retry later")]
    RateLimited,
    #[error("model service returned HTTP {status}")]
    Http { status: u16 },
    #[error("model service returned HTTP {status}{description}")]
    HttpResponse {
        status: u16,
        description: String,
        retry_after: Option<Duration>,
        should_retry: Option<bool>,
    },
    #[error("model connection failed; check the service address and network")]
    Connection,
    #[error("model connection failed during {stage:?}")]
    ConnectionAt { stage: ModelRequestStage },
    #[error("model response stream ended before completion")]
    StreamInterrupted,
    #[error("model stream stopped after output began; request was not retried")]
    StreamInterruptedAfterOutput,
    #[error("model request timed out")]
    TimedOut,
    #[error("model request timed out during {stage:?}")]
    TimedOutAt { stage: ModelRequestStage },
    #[error("model service reported a failure")]
    ProviderFailure,
    #[error("{0}")]
    ResponseRejected(ModelResponseRejection),
    #[error("invalid model response: {0}")]
    InvalidResponse(String),
    #[error("model request was cancelled")]
    Cancelled,
    #[error("model request attempt limit was reached")]
    RequestBudgetExhausted { stage: ModelRequestStage },
    #[error("{error}")]
    AtStage {
        stage: ModelRequestStage,
        error: Box<ModelError>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFailureKind {
    Configuration,
    AuthenticationRequired,
    AuthenticationFailed,
    EntitlementDenied,
    QuotaExhausted,
    RateLimited,
    Connection,
    Timeout,
    StreamInterrupted,
    InvalidResponse,
    ResponseRejected,
    ProviderFailure,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRequestStage {
    Unknown,
    Configuration,
    Credentials,
    Connect,
    RequestSend,
    ResponseHeaders,
    ResponseStream,
    Decode,
    Cancelled,
}

impl ModelError {
    pub fn at_stage(self, stage: ModelRequestStage) -> Self {
        Self::AtStage {
            stage,
            error: Box::new(self),
        }
    }

    pub fn kind(&self) -> ModelFailureKind {
        match self {
            Self::ProviderHttp(failure) => provider_failure::failure_kind(failure),
            Self::AtStage { error, .. } => error.kind(),
            Self::Configuration(_) | Self::UnsupportedProtocol(_) => {
                ModelFailureKind::Configuration
            }
            Self::CodexNotLoggedIn | Self::MiniMaxNotLoggedIn | Self::CopilotNotLoggedIn => {
                ModelFailureKind::AuthenticationRequired
            }
            Self::Authentication
            | Self::AuthenticationAt { .. }
            | Self::CodexAuthentication
            | Self::MiniMaxAuthentication
            | Self::CopilotAuthentication => ModelFailureKind::AuthenticationFailed,
            Self::InsufficientBalance
            | Self::QuotaExhausted { .. }
            | Self::Http { status: 402 }
            | Self::HttpResponse { status: 402, .. } => ModelFailureKind::QuotaExhausted,
            Self::EntitlementDenied { .. }
            | Self::Http { status: 403 }
            | Self::HttpResponse { status: 403, .. } => ModelFailureKind::EntitlementDenied,
            Self::RateLimited
            | Self::Http { status: 429 }
            | Self::HttpResponse { status: 429, .. } => ModelFailureKind::RateLimited,
            Self::Connection | Self::ConnectionAt { .. } => ModelFailureKind::Connection,
            Self::TimedOut | Self::TimedOutAt { .. } => ModelFailureKind::Timeout,
            Self::StreamInterrupted | Self::StreamInterruptedAfterOutput => {
                ModelFailureKind::StreamInterrupted
            }
            Self::InvalidResponse(_) => ModelFailureKind::InvalidResponse,
            Self::ResponseRejected(_) => ModelFailureKind::ResponseRejected,
            Self::Http { .. }
            | Self::HttpResponse { .. }
            | Self::ProviderFailure
            | Self::RequestBudgetExhausted { .. } => ModelFailureKind::ProviderFailure,
            Self::Cancelled => ModelFailureKind::Cancelled,
        }
    }

    pub fn stage(&self) -> ModelRequestStage {
        match self {
            Self::ProviderHttp(_) => ModelRequestStage::ResponseHeaders,
            Self::AtStage { stage, .. } => *stage,
            Self::Configuration(_) | Self::UnsupportedProtocol(_) => {
                ModelRequestStage::Configuration
            }
            Self::CodexNotLoggedIn
            | Self::MiniMaxNotLoggedIn
            | Self::MiniMaxAuthentication
            | Self::CopilotNotLoggedIn
            | Self::CopilotAuthentication => ModelRequestStage::Credentials,
            Self::Authentication | Self::CodexAuthentication | Self::Connection => {
                ModelRequestStage::Unknown
            }
            Self::AuthenticationAt { stage }
            | Self::ConnectionAt { stage }
            | Self::TimedOutAt { stage } => *stage,
            Self::StreamInterrupted | Self::StreamInterruptedAfterOutput => {
                ModelRequestStage::ResponseStream
            }
            Self::InvalidResponse(_) | Self::ResponseRejected(_) => ModelRequestStage::Decode,
            Self::Cancelled => ModelRequestStage::Cancelled,
            Self::RequestBudgetExhausted { stage } => *stage,
            Self::TimedOut => ModelRequestStage::Unknown,
            Self::RateLimited
            | Self::InsufficientBalance
            | Self::EntitlementDenied { .. }
            | Self::QuotaExhausted { .. }
            | Self::Http { .. }
            | Self::HttpResponse { .. }
            | Self::ProviderFailure => ModelRequestStage::Unknown,
        }
    }

    pub fn retry_hint(&self) -> (bool, Option<Duration>) {
        match self {
            Self::ProviderHttp(failure) => (failure.retryable, failure.retry_after),
            Self::AtStage { error, .. } => error.retry_hint(),
            Self::RateLimited
            | Self::Connection
            | Self::ConnectionAt { .. }
            | Self::StreamInterrupted
            | Self::TimedOut
            | Self::TimedOutAt { .. } => (true, None),
            Self::Http { status } => (matches!(status, 408 | 409 | 429 | 500..=599), None),
            Self::HttpResponse {
                status,
                retry_after,
                should_retry,
                ..
            } => (
                should_retry.unwrap_or(matches!(status, 408 | 409 | 429 | 500..=599)),
                *retry_after,
            ),
            _ => (false, None),
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Self::ProviderHttp(failure) => Some(failure.status),
            Self::AtStage { error, .. } => error.status(),
            Self::Http { status }
            | Self::HttpResponse { status, .. }
            | Self::EntitlementDenied { status, .. }
            | Self::QuotaExhausted { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn safe_quota_detail(&self) -> Option<&str> {
        match self {
            Self::AtStage { error, .. } => error.safe_quota_detail(),
            Self::QuotaExhausted { description, .. } => Some(description),
            _ => None,
        }
    }

    pub fn provider_failure(&self) -> Option<&ProviderHttpFailure> {
        match self {
            Self::ProviderHttp(failure) => Some(failure),
            Self::AtStage { error, .. } => error.provider_failure(),
            _ => None,
        }
    }
}

fn request_transport_error(error: reqwest::Error) -> ModelError {
    let stage = if error.is_connect() {
        ModelRequestStage::Connect
    } else {
        ModelRequestStage::ResponseHeaders
    };
    if error.is_timeout() {
        ModelError::TimedOutAt { stage }
    } else {
        ModelError::ConnectionAt { stage }
    }
}

#[async_trait]
pub trait ModelClient: Send + Sync {
    fn connection_id(&self) -> Option<String> {
        None
    }

    fn identity(&self) -> ModelIdentity;

    async fn generate_title(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.complete(request, events, cancel).await
    }

    async fn close_chat(&self, _chat_id: tickloop_agent_protocol::ChatId) {}

    async fn complete_for_chat_with_context(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
        _context: ModelRequestContext,
    ) -> Result<ModelOutput, ModelError> {
        self.complete_for_chat(chat_id, request, events, cancel)
            .await
    }

    async fn complete_for_chat(
        &self,
        _chat_id: tickloop_agent_protocol::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.complete(request, events, cancel).await
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError>;
}

pub fn validate_model_config(config: &ModelConfig) -> Result<(), ModelError> {
    if let Some(connection) = &config.connection {
        connection.validate().map_err(ModelError::Configuration)?;
        match connection.discovery {
            ModelDiscovery::OpenAiApi if config.auth_source != ModelAuthSource::ApiKeyEnv => {
                return Err(ModelError::Configuration(
                    "OpenAI API discovery requires API key authentication".into(),
                ));
            }
            ModelDiscovery::Codex if config.auth_source != ModelAuthSource::CodexSubscription => {
                return Err(ModelError::Configuration(
                    "Codex discovery requires subscription authentication".into(),
                ));
            }
            ModelDiscovery::GithubCopilot
                if config.auth_source != ModelAuthSource::GithubCopilot =>
            {
                return Err(ModelError::Configuration(
                    "GitHub Copilot discovery requires Copilot sign-in".into(),
                ));
            }
            _ => {}
        }
    }
    if config.protocol == Protocol::SystemOne {
        return Err(ModelError::Configuration(
            "Jev is a judgment model; configure it under jev, not as a conversation model".into(),
        ));
    }
    if let Some(card) = &config.card
        && (config.provider_id.is_empty()
            || config.model_id != card.id
            || config.model != card.service_model_id
            || config.name != card.display_name
            || config.protocol != card.adapter.protocol()
            || config.max_output_tokens != card.max_output_tokens)
    {
        return Err(ModelError::Configuration(
            "model card and effective model configuration disagree".into(),
        ));
    }
    if config.card.is_some()
        && (config.auth_source == ModelAuthSource::CodexSubscription)
            != (config.adapter() == Adapter::CodexResponses)
    {
        return Err(ModelError::Configuration(
            "Codex Responses requires subscription authentication".into(),
        ));
    }
    match (config.provider, config.auth_source) {
        (
            ModelProvider::Api,
            ModelAuthSource::ApiKeyEnv
            | ModelAuthSource::MiniMaxOAuthCn
            | ModelAuthSource::MiniMaxOAuthGlobal
            | ModelAuthSource::GithubCopilot
            | ModelAuthSource::GoogleCloud
            | ModelAuthSource::GoogleOAuth
            | ModelAuthSource::KimiOAuth
            | ModelAuthSource::XaiOAuth,
        ) => HttpModelClient::deferred(config.clone()).map(|_| ()),
        (ModelProvider::OpenAiCodex, ModelAuthSource::CodexSubscription) => {
            codex::validate_config(config)
        }
        _ => Err(ModelError::Configuration(
            "model provider and authentication source do not match".into(),
        )),
    }
}

pub fn validate_model_for_resume(config: &ModelConfig) -> Result<(), ModelError> {
    validate_model_for_resume_in_root(config, None)
}

pub fn validate_model_for_resume_in_root(
    config: &ModelConfig,
    state_root: Option<&Path>,
) -> Result<(), ModelError> {
    validate_model_config(config)?;
    match config.provider {
        ModelProvider::Api
            if crate::auth::minimax::MiniMaxRegion::from_auth(config.auth_source).is_some()
                || config.auth_source != ModelAuthSource::ApiKeyEnv =>
        {
            Ok(())
        }
        ModelProvider::Api => api_key(config, state_root).map(|_| ()),
        ModelProvider::OpenAiCodex => Ok(()),
    }
}

pub fn model_client(
    config: ModelConfig,
    state_dir: Option<&Path>,
) -> Result<Arc<dyn ModelClient>, ModelError> {
    validate_model_config(&config)?;
    match config.provider {
        ModelProvider::Api => {
            if crate::auth::minimax::MiniMaxRegion::from_auth(config.auth_source).is_some()
                && state_dir.is_none()
            {
                return Err(ModelError::Configuration(
                    "MiniMax OAuth requires an Agent state directory".into(),
                ));
            }
            if config.auth_source != ModelAuthSource::ApiKeyEnv && state_dir.is_none() {
                return Err(ModelError::Configuration(
                    "Provider sign-in requires an Agent state directory".into(),
                ));
            }
            Ok(Arc::new(HttpModelClient::deferred_with_root(
                config, state_dir,
            )?))
        }
        ModelProvider::OpenAiCodex => {
            let state_dir = state_dir.ok_or_else(|| {
                ModelError::Configuration(
                    "Codex subscription requires an Agent state directory".into(),
                )
            })?;
            let client = codex::CodexModelClient::new(config, state_dir)?;
            #[cfg(any(test, feature = "test-support"))]
            let client = if let Some(endpoint) = std::env::var_os("TICKLOOP_TEST_CODEX_RESPONSES") {
                let endpoint = endpoint.into_string().map_err(|_| {
                    ModelError::Configuration(
                        "Codex test endpoint must be numeric loopback HTTP".into(),
                    )
                })?;
                client.with_loopback_test_endpoint(&endpoint)?
            } else {
                client
            };
            Ok(Arc::new(client))
        }
    }
}

pub struct HttpModelClient {
    config: ModelConfig,
    client: reqwest::Client,
    route: NetworkRoute,
    state_root: Option<PathBuf>,
    cache_id: uuid::Uuid,
}

type PrivateBlockKey = (tickloop_agent_protocol::ChatId, [u8; 32], [u8; 32]);

fn api_private_provenance() -> &'static Mutex<HashMap<PrivateBlockKey, Option<[u8; 32]>>> {
    static PROVENANCE: OnceLock<Mutex<HashMap<PrivateBlockKey, Option<[u8; 32]>>>> =
        OnceLock::new();
    PROVENANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

impl HttpModelClient {
    fn credential_scope(&self) -> [u8; 32] {
        let source = if let Some(source) = self.config.private_history_source() {
            serde_json::to_vec(&source)
        } else {
            serde_json::to_vec(&(
                &self.config.provider_id,
                self.config.base_url.trim_end_matches('/'),
                self.config.adapter(),
                &self.config.model,
                self.config.auth_source,
                &self.config.api_key_env,
            ))
        }
        .expect("model configuration is serializable");
        Sha256::digest(source).into()
    }

    fn private_block_fingerprint(content: &Content) -> [u8; 32] {
        let block = serde_json::to_vec(content).expect("model content is serializable");
        Sha256::digest(block).into()
    }

    fn ensure_credential_history_compatible(
        &self,
        chat_id: Option<tickloop_agent_protocol::ChatId>,
        request: &ModelRequest,
        key: Option<&reqwest::header::HeaderValue>,
    ) -> Result<(), ModelError> {
        let Some(key) = key else {
            return Ok(());
        };
        let fingerprint: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        let scope = self.credential_scope();
        let provenance = api_private_provenance()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (index, message) in request.messages.iter().enumerate() {
            let persisted = request.history_sources.get(index).and_then(Option::as_ref);
            let trusted = persisted.is_some_and(|source| {
                source.credential_identity.as_deref()
                    == Some(credential_fingerprint(key.as_bytes()).as_str())
                    && {
                        let mut expected = source.clone();
                        expected.credential_identity = None;
                        Some(expected) == self.config.private_history_source()
                    }
            });
            for content in &message.content {
                if matches!(
                    content,
                    Content::Thinking { .. } | Content::ProtocolData { .. }
                ) && !trusted
                    && chat_id
                        .and_then(|chat_id| {
                            provenance
                                .get(&(chat_id, scope, Self::private_block_fingerprint(content)))
                                .copied()
                        })
                        .flatten()
                        != Some(fingerprint)
                {
                    return Err(ModelError::Configuration(
                    "the API credential source cannot prove private model history is compatible; restart from public history or compact with the original credential".into(),
                ));
                }
            }
        }
        Ok(())
    }

    fn record_private_output(
        &self,
        chat_id: Option<tickloop_agent_protocol::ChatId>,
        key: Option<&reqwest::header::HeaderValue>,
        output: &ModelOutput,
    ) {
        let (Some(chat_id), Some(key)) = (chat_id, key) else {
            return;
        };
        let fingerprint: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        let scope = self.credential_scope();
        let mut provenance = api_private_provenance()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for content in &output.content {
            if matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            ) {
                let key = (chat_id, scope, Self::private_block_fingerprint(content));
                let recorded = provenance.entry(key).or_insert(Some(fingerprint));
                if *recorded != Some(fingerprint) {
                    *recorded = None;
                }
            }
        }
    }

    pub fn new(config: ModelConfig) -> Result<Self, ModelError> {
        api_key(&config, None)?;
        Self::deferred(config)
    }

    pub fn deferred(config: ModelConfig) -> Result<Self, ModelError> {
        Self::deferred_with_root(config, None)
    }

    fn deferred_with_root(
        config: ModelConfig,
        state_root: Option<&Path>,
    ) -> Result<Self, ModelError> {
        let config = cards::model_for_persistent_selection(config);
        if let Some(region) = crate::auth::minimax::MiniMaxRegion::from_auth(config.auth_source) {
            let endpoint = reqwest::Url::parse(&config.base_url)
                .map_err(|_| ModelError::Configuration("invalid MiniMax endpoint".into()))?;
            let testing =
                cfg!(debug_assertions) && endpoint.scheme() == "http" && is_loopback(&endpoint);
            if config.adapter() != Adapter::MinimaxMessages
                || !config.api_key_env.is_empty()
                || !testing && region.validate_resource(&config.base_url).is_err()
            {
                return Err(ModelError::Configuration("MiniMax OAuth requires its regional Messages endpoint and no API key environment variable".into()));
            }
        }
        if config.auth_source == ModelAuthSource::GithubCopilot
            && (!matches!(config.adapter(), Adapter::Messages | Adapter::Responses)
                || !config.api_key_env.is_empty()
                || !copilot::is_allowed_endpoint(&config.base_url))
        {
            return Err(ModelError::Configuration(
                "GitHub Copilot sign-in requires Messages or Responses, its endpoint and no API key environment variable".into(),
            ));
        }

        let registered_auth = match config.provider_id.as_str() {
            "google-gemini-oauth" | "google-vertex-oauth" => Some(ModelAuthSource::GoogleOAuth),
            "google-vertex-service-account" => Some(ModelAuthSource::GoogleCloud),
            "kimi-code-oauth" => Some(ModelAuthSource::KimiOAuth),
            "xai-oauth" => Some(ModelAuthSource::XaiOAuth),
            _ => None,
        };
        if registered_auth.is_some_and(|source| source != config.auth_source) {
            return Err(ModelError::Configuration(
                "This Provider requires its registered authentication route".into(),
            ));
        }
        if matches!(
            config.auth_source,
            ModelAuthSource::GoogleCloud | ModelAuthSource::GoogleOAuth
        ) {
            let loopback = cfg!(debug_assertions)
                && reqwest::Url::parse(&config.base_url)
                    .is_ok_and(|url| url.scheme() == "http" && is_loopback(&url));
            let valid = config.api_key_env.is_empty()
                && config.adapter() == Adapter::Google
                && match (config.provider_id.as_str(), config.auth_source) {
                    ("google-gemini-oauth", ModelAuthSource::GoogleOAuth) => {
                        loopback
                            || config.base_url == "https://generativelanguage.googleapis.com/v1beta"
                    }
                    ("google-vertex-service-account", ModelAuthSource::GoogleCloud)
                    | ("google-vertex-oauth", ModelAuthSource::GoogleOAuth) => {
                        loopback || config.base_url == "https://aiplatform.googleapis.com/v1"
                    }
                    _ => false,
                };
            if !valid {
                return Err(ModelError::Configuration(
                    "Google OAuth requires its registered Provider and official native endpoint"
                        .into(),
                ));
            }
        }
        if let Some(provider) =
            crate::auth::device_subscriptions::DeviceSubscription::from_auth(config.auth_source)
            && (!config.api_key_env.is_empty()
                || config.base_url != provider.endpoint()
                    && !(cfg!(debug_assertions)
                        && reqwest::Url::parse(&config.base_url)
                            .is_ok_and(|url| url.scheme() == "http" && is_loopback(&url)))
                || match provider {
                    crate::auth::device_subscriptions::DeviceSubscription::Kimi => {
                        config.provider_id != "kimi-code-oauth"
                            || config.adapter() != Adapter::KimiChatCompletions
                    }
                    crate::auth::device_subscriptions::DeviceSubscription::Xai => {
                        config.provider_id != "xai-oauth" || config.adapter() != Adapter::Responses
                    }
                })
        {
            return Err(ModelError::Configuration("Subscription OAuth requires its registered Provider, adapter and official endpoint".into()));
        }
        if config.protocol == Protocol::Google
            && (!config
                .model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)))
        {
            return Err(ModelError::Configuration(
                "invalid Google service model ID".into(),
            ));
        }
        if config.provider != ModelProvider::Api
            || config.auth_source == ModelAuthSource::CodexSubscription
        {
            return Err(ModelError::Configuration(
                "use the model factory for a Codex subscription model".into(),
            ));
        }
        let url = reqwest::Url::parse(&config.base_url).map_err(|_| {
            ModelError::Configuration("service address must be an HTTP(S) URL".into())
        })?;
        let is_loopback = is_loopback(&url);
        if !(url.scheme() == "https" || url.scheme() == "http" && is_loopback)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ModelError::Configuration("use an HTTPS service address without credentials, query or fragment; HTTP is allowed only for loopback".into()));
        }
        if config.model.trim().is_empty()
            || config.name.trim().is_empty()
            || config.max_output_tokens == 0
        {
            return Err(ModelError::Configuration(
                "model name, profile name and a positive output token limit are required".into(),
            ));
        }
        if config.auth_source == ModelAuthSource::ApiKeyEnv
            && config.api_key_env.is_empty()
            && !is_loopback
        {
            return Err(ModelError::Configuration(
                "an API key environment variable is required for remote services".into(),
            ));
        }
        let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
        policy.connect_timeout = Some(Duration::from_secs(15));
        policy.read_timeout = Some(Duration::from_secs(120));
        let (client, route) =
            build_http_client(&url, policy).map_err(|_| ModelError::Connection)?;
        Ok(Self {
            config,
            client,
            route,
            state_root: state_root.map(Path::to_path_buf),
            cache_id: uuid::Uuid::new_v4(),
        })
    }
}

impl HttpModelClient {
    /// Sends the request once, and once more without its images when the service rejects
    /// them for their size or count (Claude Code `isMediaSizeError`).
    async fn complete_inner(
        &self,
        chat_id: Option<tickloop_agent_protocol::ChatId>,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
        context: Option<&ModelRequestContext>,
    ) -> Result<ModelOutput, ModelError> {
        let mut request = without_unsupported_images(&self.config, request);
        image_limits::limit_images_per_request(&mut request.messages);
        let image_free_request = image_limits::image_free_copy(&request);
        let mut rejected_images = false;
        let result = self
            .complete_once(
                chat_id,
                request,
                events.clone(),
                cancel.clone(),
                image_free_request.is_some().then_some(&mut rejected_images),
                context,
            )
            .await;
        match image_free_request {
            Some(image_free_request) if rejected_images => {
                if !context.is_none_or(ModelRequestContext::try_begin_attempt) {
                    return Err(ModelError::RequestBudgetExhausted {
                        stage: ModelRequestStage::RequestSend,
                    });
                }
                self.complete_once(chat_id, image_free_request, events, cancel, None, context)
                    .await
            }
            _ => result,
        }
    }

    /// `rejected_images` is set when the service refused the request because of its images.
    async fn complete_once(
        &self,
        chat_id: Option<tickloop_agent_protocol::ChatId>,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
        rejected_images: Option<&mut bool>,
        context: Option<&ModelRequestContext>,
    ) -> Result<ModelOutput, ModelError> {
        let region = crate::auth::minimax::MiniMaxRegion::from_auth(self.config.auth_source);
        let copilot_auth = self.config.auth_source == ModelAuthSource::GithubCopilot;
        let mut copilot_api_base = None;
        let google_auth = matches!(
            self.config.auth_source,
            ModelAuthSource::GoogleCloud | ModelAuthSource::GoogleOAuth
        );
        let subscription = crate::auth::device_subscriptions::DeviceSubscription::from_auth(
            self.config.auth_source,
        );
        let identity_from_sign_in = self.config.auth_source != ModelAuthSource::ApiKeyEnv;
        let mut google_project = None;
        let mut resolved_base = None;
        let (key, credential_identity) = if let Some(region) = region {
            let root = self.state_root.as_deref().ok_or_else(|| {
                ModelError::Configuration("MiniMax OAuth requires an Agent state directory".into())
            })?;
            let credentials = crate::auth::minimax::MiniMaxAuth::new(root, region)
                .resolve_credentials_with_refresh_gate(&cancel, || {
                    context.is_none_or(ModelRequestContext::try_begin_attempt)
                })
                .await
                .map_err(|error| match error {
                    crate::auth::minimax::MiniMaxAuthError::Cancelled => ModelError::Cancelled,
                    crate::auth::minimax::MiniMaxAuthError::NotLoggedIn => {
                        ModelError::MiniMaxNotLoggedIn
                    }
                    crate::auth::minimax::MiniMaxAuthError::RefreshFailed => {
                        ModelError::MiniMaxAuthentication
                    }
                    crate::auth::minimax::MiniMaxAuthError::BudgetExhausted => {
                        ModelError::RequestBudgetExhausted {
                            stage: ModelRequestStage::Credentials,
                        }
                    }
                    crate::auth::minimax::MiniMaxAuthError::Network => ModelError::ConnectionAt {
                        stage: ModelRequestStage::Credentials,
                    },
                    crate::auth::minimax::MiniMaxAuthError::Http(status) => {
                        ModelError::Http { status }.at_stage(ModelRequestStage::Credentials)
                    }
                    _ => ModelError::Configuration(error.to_string()),
                })?;
            let mut key = reqwest::header::HeaderValue::from_str(&credentials.access_token)
                .map_err(|_| {
                    ModelError::Configuration("invalid MiniMax credential header".into())
                })?;
            key.set_sensitive(true);
            (Some(key), Some(credentials.identity.to_string()))
        } else if copilot_auth {
            let root = self.state_root.as_deref().ok_or_else(|| {
                ModelError::Configuration(
                    "GitHub Copilot sign-in requires an Agent state directory".into(),
                )
            })?;
            let access = crate::auth::copilot::CopilotAuth::new(root)
                .resolve_access_with_refresh_gate(&cancel, || {
                    context.is_none_or(ModelRequestContext::try_begin_attempt)
                })
                .await
                .map_err(copilot::credential_error)?;
            let mut key = reqwest::header::HeaderValue::from_str(&access.token).map_err(|_| {
                ModelError::Configuration("invalid GitHub Copilot credential header".into())
            })?;
            key.set_sensitive(true);
            copilot_api_base = access.api_base;
            (Some(key), Some(access.identity.to_string()))
        } else if google_auth {
            let root = self.state_root.as_deref().ok_or_else(|| {
                ModelError::Configuration(
                    "Google authentication requires an Agent state directory".into(),
                )
            })?;
            let access = crate::auth::google::GoogleAuth::new(root)
                .resolve(&self.config, &cancel, context)
                .await?;
            let mut key = reqwest::header::HeaderValue::from_str(&access.token).map_err(|_| {
                ModelError::Configuration("invalid Google credential header".into())
            })?;
            key.set_sensitive(true);
            google_project = Some(access.project);
            resolved_base = Some(access.base_url);
            (
                Some(key),
                Some(credential_fingerprint(
                    format!(
                        "{}:{}",
                        access.identity,
                        resolved_base.as_deref().unwrap_or("")
                    )
                    .as_bytes(),
                )),
            )
        } else if let Some(provider) = subscription {
            let root = self.state_root.as_deref().ok_or_else(|| {
                ModelError::Configuration(
                    "Subscription authentication requires an Agent state directory".into(),
                )
            })?;
            let access = crate::auth::device_subscriptions::SubscriptionAuth::new(root, provider)
                .resolve(&cancel, context)
                .await?;
            let mut key = reqwest::header::HeaderValue::from_str(&access.token).map_err(|_| {
                ModelError::Configuration("invalid subscription credential header".into())
            })?;
            key.set_sensitive(true);
            (Some(key), Some(access.identity.to_string()))
        } else {
            let key = api_key(&self.config, self.state_root.as_deref())?;
            let identity = key
                .as_ref()
                .map(|key| credential_fingerprint(key.as_bytes()));
            (key, identity)
        };
        let mut source = self.config.private_history_source();
        if let Some(source) = &mut source {
            source.credential_identity = credential_identity;
        }
        let mut request = request;
        if identity_from_sign_in && request.history_sources.len() != request.messages.len() {
            request.history_sources = vec![None; request.messages.len()];
        }
        let mut request = history::project_request(request, &self.config, source.as_ref());
        if copilot_auth && self.config.adapter() == Adapter::Responses {
            request = copilot::without_connection_bound_reasoning(request);
        }
        if !identity_from_sign_in {
            self.ensure_credential_history_compatible(chat_id, &request, key.as_ref())?;
        }
        let structured_response =
            structured_response::StructuredResponsePolicy::for_request(&self.config, &request);
        let (endpoint, mut body) = match self.config.adapter() {
            Adapter::Messages | Adapter::DeepseekMessages | Adapter::MinimaxMessages => {
                ("v1/messages", messages_request(&self.config, request)?)
            }
            Adapter::Responses => (
                "responses",
                responses::encode_request(&self.config, request)?,
            ),
            Adapter::ChatCompletions
            | Adapter::DeepseekChatCompletions
            | Adapter::GlmChatCompletions
            | Adapter::KimiChatCompletions
            | Adapter::QwenChatCompletions => (
                "chat/completions",
                chat_completions::encode_request(&self.config, request)?,
            ),
            Adapter::Google => ("", google::encode_request(&self.config, request)?),
            Adapter::SystemOne => {
                return Err(ModelError::UnsupportedProtocol(Protocol::SystemOne));
            }
            Adapter::CodexResponses => {
                return Err(ModelError::Configuration(
                    "Codex Responses requires subscription authentication".into(),
                ));
            }
        };
        if matches!(self.config.adapter(), Adapter::KimiChatCompletions)
            || self.config.provider_id.starts_with("xai-")
        {
            body["prompt_cache_key"] = json!(credential_fingerprint(
                format!(
                    "{}:{}",
                    self.config.provider_id,
                    chat_id.unwrap_or(self.cache_id)
                )
                .as_bytes()
            ));
        }
        let base_url = resolved_base
            .as_deref()
            .or(copilot_api_base.as_deref())
            .unwrap_or(&self.config.base_url);
        let target = reqwest::Url::parse(&if self.config.protocol == Protocol::Google {
            format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                base_url.trim_end_matches('/'),
                self.config.model
            )
        } else {
            format!("{}/{endpoint}", base_url.trim_end_matches('/'))
        })
        .map_err(|_| ModelError::Configuration("invalid model endpoint".into()))?;
        let mut outgoing = self
            .client
            .post(target.clone())
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        if let Some(key) = key.as_ref() {
            outgoing = match self.config.protocol {
                Protocol::Google if self.config.auth_source == ModelAuthSource::ApiKeyEnv => {
                    outgoing.header("x-goog-api-key", key.clone())
                }
                Protocol::Messages
                    if self.config.adapter() != Adapter::MinimaxMessages
                        && self.config.auth_source == ModelAuthSource::ApiKeyEnv =>
                {
                    outgoing.header("x-api-key", key.clone())
                }
                _ => {
                    let mut value = reqwest::header::HeaderValue::from_str(&format!(
                        "Bearer {}",
                        key.to_str().map_err(|_| ModelError::Configuration(
                            "API key must be ASCII".into()
                        ))?
                    ))
                    .map_err(|_| ModelError::Configuration("invalid API key header".into()))?;
                    value.set_sensitive(true);
                    outgoing.header(reqwest::header::AUTHORIZATION, value)
                }
            };
        }
        if let Some(project) = google_project {
            outgoing = outgoing.header("x-goog-user-project", project);
        }
        if self.config.auth_source == ModelAuthSource::XaiOAuth {
            outgoing = outgoing
                .header("X-XAI-Token-Auth", "xai-grok-cli")
                .header("x-grok-model-override", &self.config.model);
        }
        if copilot_auth {
            for (name, value) in copilot::request_headers(&body) {
                outgoing = outgoing.header(name, value);
            }
        }
        let started = Instant::now();
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            result = outgoing.send() => result.map_err(request_transport_error),
        };
        NetworkDiagnostic::new(
            "model_response_headers",
            &target,
            self.route,
            sent.as_ref()
                .ok()
                .map(|response| response.status().as_u16()),
            started.elapsed(),
        )
        .emit();
        let mut response = sent?;
        let (retry_after, should_retry) = provider_failure::retry_headers(response.headers());
        let status = response.status().as_u16();
        if !(200..=299).contains(&status) {
            let error_body = provider_failure::read_error_body(&mut response, &cancel).await?;
            let error_bytes = &error_body.bytes;
            if let Some(rejected_images) = rejected_images
                && error_body.complete
                && matches!(status, 400 | 413)
                && image_limits::rejects_images(&String::from_utf8_lossy(error_bytes))
            {
                *rejected_images = true;
                return Err(
                    ModelError::Http { status }.at_stage(ModelRequestStage::ResponseHeaders)
                );
            }
            let body = serde_json::from_slice::<Value>(error_bytes).ok();
            if error_body.complete
                && (body.as_ref().is_some_and(is_context_limit_error)
                    || copilot_auth && body.as_ref().is_some_and(copilot::is_context_limit))
            {
                return Err(
                    ModelError::ResponseRejected(ModelResponseRejection::ContextLimit)
                        .at_stage(ModelRequestStage::ResponseHeaders),
                );
            }
            let secrets = key
                .as_ref()
                .and_then(|key| std::str::from_utf8(key.as_bytes()).ok())
                .into_iter()
                .collect::<Vec<_>>();
            let mut failure = ProviderHttpFailure::from_response(
                &self.config,
                status,
                error_bytes,
                retry_after,
                should_retry,
                &secrets,
            );
            if copilot_auth
                && body
                    .as_ref()
                    .is_some_and(|body| copilot::is_model_not_enabled(status, body))
            {
                failure.set_class(ProviderFailureClass::ModelUnavailable, should_retry);
            }
            if self.config.adapter() == Adapter::GlmChatCompletions
                && let Some(error) = glm::http_error(status, body.as_ref())
            {
                let class = match error.kind() {
                    ModelFailureKind::QuotaExhausted
                        if glm::business_code(body.as_ref())
                            == Some(glm::INSUFFICIENT_BALANCE_CODE) =>
                    {
                        ProviderFailureClass::Billing
                    }
                    ModelFailureKind::QuotaExhausted => ProviderFailureClass::UsageLimit,
                    ModelFailureKind::EntitlementDenied => ProviderFailureClass::ModelUnavailable,
                    _ if error_body.complete => {
                        return Err(error.at_stage(ModelRequestStage::ResponseHeaders));
                    }
                    _ => ProviderFailureClass::Refused,
                };
                failure.set_class(class, should_retry);
                failure.hint.push_str(&format!(" {}", error));
            }
            if self.config.adapter() == Adapter::KimiChatCompletions
                && kimi::http_error(status, body.as_ref()).is_some()
            {
                failure.set_class(
                    if self.config.auth_source == ModelAuthSource::KimiOAuth && status == 403 {
                        ProviderFailureClass::UsageOrPlan
                    } else {
                        ProviderFailureClass::UsageLimit
                    },
                    should_retry,
                );
            }
            failure.require_complete_body(error_body.complete, &secrets);
            return Err(ModelError::ProviderHttp(Box::new(failure)));
        }
        let mut output = consume_sse_response(
            response,
            self.config.protocol,
            events,
            cancel,
            structured_response,
            self.config.adapter(),
            copilot_auth,
        )
        .await?;
        output.private_source = source;
        self.record_private_output(chat_id, key.as_ref(), &output);
        Ok(output)
    }
}

#[async_trait]
impl ModelClient for HttpModelClient {
    fn connection_id(&self) -> Option<String> {
        Some(self.config.provider_id.clone())
    }

    fn identity(&self) -> ModelIdentity {
        self.config.identity()
    }

    async fn close_chat(&self, chat_id: tickloop_agent_protocol::ChatId) {
        api_private_provenance()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(recorded_chat, _, _), _| *recorded_chat != chat_id);
    }

    async fn complete_for_chat_with_context(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
        context: ModelRequestContext,
    ) -> Result<ModelOutput, ModelError> {
        self.complete_inner(Some(chat_id), request, events, cancel, Some(&context))
            .await
    }

    async fn complete_for_chat(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.complete_inner(Some(chat_id), request, events, cancel, None)
            .await
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.complete_inner(None, request, events, cancel, None)
            .await
    }
}

async fn consume_sse_response(
    mut response: reqwest::Response,
    protocol: Protocol,
    events: UnboundedSender<ModelDelta>,
    cancel: CancellationToken,
    mut structured_response: Option<structured_response::StructuredResponsePolicy>,
    adapter: Adapter,
    rotating_response_item_ids: bool,
) -> Result<ModelOutput, ModelError> {
    let codex = adapter == Adapter::CodexResponses;
    let has_event_stream_content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    // Codex follows pi's body-based SSE decoding when intermediaries omit or alter MIME.
    if !has_event_stream_content_type && !codex {
        return Err(invalid_response(
            "service did not return an event stream; check the protocol and service address",
        ));
    }
    let mut pending = Vec::new();
    let mut event_data = String::new();
    let mut event_name = String::new();
    let mut received_bytes = 0usize;
    let mut received_sse_data = false;
    let mut output_started = false;
    let mut stream = match protocol {
        Protocol::Google => ProtocolStream::Google(google::GoogleStream::default()),
        Protocol::SystemOne => return Err(ModelError::UnsupportedProtocol(protocol)),
        Protocol::Messages => ProtocolStream::Messages(MessagesStream {
            provisional_input_usage: adapter == Adapter::MinimaxMessages,
            ..MessagesStream::default()
        }),
        Protocol::Responses => ProtocolStream::Responses(if codex {
            responses::ResponsesStream::for_codex()
        } else if rotating_response_item_ids {
            responses::ResponsesStream::with_rotating_item_ids()
        } else {
            responses::ResponsesStream::default()
        }),
        Protocol::ChatCompletions => {
            ProtocolStream::Chat(chat_completions::ChatStream::for_adapter(adapter))
        }
    };
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            result = response.chunk() => result.map_err(|error| if output_started {
                ModelError::StreamInterruptedAfterOutput
            } else if error.is_timeout() {
                ModelError::TimedOutAt { stage: ModelRequestStage::ResponseStream }
            } else {
                ModelError::StreamInterrupted
            })?,
        };
        let Some(chunk) = chunk else { break };
        received_bytes = received_bytes.saturating_add(chunk.len());
        if received_bytes > MAX_RESPONSE_STREAM_BYTES {
            return Err(invalid_response(
                "response stream exceeded the supported size limit",
            ));
        }
        pending.extend_from_slice(&chunk);
        if pending.len() > MAX_SSE_EVENT_BYTES {
            return Err(invalid_response(
                "response event exceeded the supported size limit",
            ));
        }
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            let line = pending.drain(..=end).collect::<Vec<_>>();
            let line = std::str::from_utf8(&line)
                .map_err(|_| ModelError::InvalidResponse("invalid UTF-8".into()))?
                .trim_end_matches(['\r', '\n']);
            if let Some(name) = line.strip_prefix("event:") {
                event_name = name.strip_prefix(' ').unwrap_or(name).into();
            } else if let Some(data) = line.strip_prefix("data:") {
                received_sse_data = true;
                if !event_data.is_empty() {
                    event_data.push('\n');
                }
                event_data.push_str(data.strip_prefix(' ').unwrap_or(data));
                if event_data.len() > MAX_SSE_EVENT_BYTES {
                    return Err(invalid_response(
                        "response event exceeded the supported size limit",
                    ));
                }
            } else if line.is_empty() && !event_data.is_empty() {
                if protocol == Protocol::ChatCompletions && event_data == "[DONE]" {
                    return finish_structured_response(stream, structured_response.as_ref(), codex);
                }
                let mut data: Value = serde_json::from_str(&event_data)
                    .map_err(|_| ModelError::InvalidResponse("invalid event JSON".into()))?;
                event_data.clear();
                if let Some(policy) = &mut structured_response {
                    policy.check_event(protocol, &data)?;
                }
                if !event_name.is_empty() && data["type"].as_str() != Some(&event_name) {
                    return Err(invalid_response(
                        "SSE event name does not match its data type",
                    ));
                }
                event_name.clear();
                if codex && data["type"] == "response.done" {
                    let status = data["response"]["status"]
                        .as_str()
                        .ok_or_else(|| invalid_response("Codex response ended without a status"))?;
                    if !matches!(status, "completed" | "failed" | "incomplete" | "cancelled") {
                        return Err(invalid_response("unsupported Codex response status"));
                    }
                    data["type"] = json!(format!("response.{status}"));
                }
                if sse_event_starts_output(protocol, &data) {
                    output_started = true;
                }
                if sse_event_needs_output_marker(protocol, &data) {
                    let _ = events.send(ModelDelta::OutputStarted);
                }
                if stream.accept(data, &events).map_err(|error| {
                    let error = match &structured_response {
                        Some(policy) => policy.decode_error(error),
                        None => error,
                    };
                    if error.stage() == ModelRequestStage::Unknown {
                        error.at_stage(ModelRequestStage::ResponseStream)
                    } else {
                        error
                    }
                })? {
                    return finish_structured_response(stream, structured_response.as_ref(), codex);
                }
            } else if line.is_empty() {
                event_name.clear();
            }
        }
    }
    if codex && !has_event_stream_content_type && !received_sse_data {
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(|value| value.trim().to_ascii_lowercase());
        let safe_mime = match mime.as_deref() {
            Some("text/html") => "text/html",
            Some("application/json") => "application/json",
            Some("text/plain") => "text/plain",
            Some("application/octet-stream") => "application/octet-stream",
            None => "missing",
            Some(_) => "other",
        };
        return Err(invalid_response(&format!(
            "Codex returned a non-stream response (HTTP {}; Content-Type: {safe_mime}); check the service or network proxy",
            response.status().as_u16()
        )));
    }
    if protocol == Protocol::Google && pending.is_empty() && event_data.is_empty() {
        return finish_structured_response(stream, structured_response.as_ref(), codex);
    }
    Err(if output_started {
        ModelError::StreamInterruptedAfterOutput
    } else {
        ModelError::StreamInterrupted
    })
}

fn sse_event_starts_output(protocol: Protocol, data: &Value) -> bool {
    if sse_event_needs_output_marker(protocol, data) {
        return true;
    }
    match protocol {
        Protocol::Google => data["candidates"].as_array().is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate["content"]["parts"]
                    .as_array()
                    .is_some_and(|parts| !parts.is_empty())
            })
        }),
        Protocol::SystemOne => false,
        Protocol::Messages => data["type"] == "content_block_delta",
        Protocol::Responses => data["type"].as_str().is_some_and(|kind| {
            kind.starts_with("response.output_text.") || kind.starts_with("response.reasoning_")
        }),
        Protocol::ChatCompletions => data["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                let delta = &choice["delta"];
                ["content", "reasoning_content"].into_iter().any(|field| {
                    delta
                        .get(field)
                        .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
                })
            })
        }),
    }
}

fn sse_event_needs_output_marker(protocol: Protocol, data: &Value) -> bool {
    match protocol {
        Protocol::Google => data["candidates"].as_array().is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate["content"]["parts"]
                    .as_array()
                    .is_some_and(|parts| !parts.is_empty())
            })
        }),
        Protocol::SystemOne => false,
        Protocol::Messages => {
            data["type"] == "content_block_start" && data["content_block"]["type"] == "tool_use"
                || data["type"] == "content_block_delta"
                    && data["delta"]["type"] == "input_json_delta"
        }
        Protocol::Responses => {
            data["type"] == "response.output_item.added" && data["item"]["type"] == "function_call"
                || data["type"] == "response.output_item.done"
                    && data["item"]["type"] == "reasoning"
                || data["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("response.function_call_"))
        }
        Protocol::ChatCompletions => data["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                let delta = &choice["delta"];
                delta
                    .get("tool_calls")
                    .is_some_and(|value| !value.is_null())
            })
        }),
    }
}

fn is_context_limit_error(body: &Value) -> bool {
    let error = body
        .get("error")
        .filter(|error| error.is_object())
        .unwrap_or(body);
    if error["code"].as_str().is_some_and(|code| {
        matches!(
            code,
            "context_length_exceeded" | "model_context_window_exceeded" | "prompt_too_long"
        )
    }) {
        return true;
    }
    if error["type"].as_str() != Some("invalid_request_error") {
        return false;
    }
    error["message"].as_str().is_some_and(|message| {
        let message = message.to_ascii_lowercase();
        message.contains("prompt is too long")
            || message.contains("maximum context length")
            || message.contains("context window exceeded")
    })
}

fn stream_provider_error(payload: &Value) -> ModelError {
    if is_context_limit_error(payload) {
        return ModelError::ResponseRejected(ModelResponseRejection::ContextLimit);
    }
    let error = payload
        .get("error")
        .filter(|error| error.is_object())
        .unwrap_or(payload);
    let error_type = error["type"].as_str().or_else(|| error["code"].as_str());
    match error_type {
        Some("rate_limit_error" | "rate_limit_exceeded") => ModelError::RateLimited,
        Some("overloaded_error") => ModelError::Http { status: 529 },
        Some("api_error" | "server_error" | "internal_server_error") => {
            ModelError::Http { status: 500 }
        }
        Some("authentication_error") => ModelError::AuthenticationAt {
            stage: ModelRequestStage::ResponseStream,
        },
        Some("billing_error" | "insufficient_balance") => ModelError::InsufficientBalance,
        _ => ModelError::ProviderFailure,
    }
}

const UNSUPPORTED_IMAGE_TEXT: &str =
    "ERROR: Cannot read image (this model does not support image input). Inform the user.";

/// Like opencode `unsupportedParts` and Codex `strip_images_when_unsupported`: every request
/// path passes through here, so a model that does not accept images sees an explanation it
/// can relay to the user while the Transcript keeps the original images.
fn without_unsupported_images(config: &ModelConfig, mut request: ModelRequest) -> ModelRequest {
    if config.accepts_images() {
        return request;
    }
    for content in request
        .messages
        .iter_mut()
        .flat_map(|message| message.content.iter_mut())
    {
        match content {
            Content::Image { .. } => {
                *content = Content::Text {
                    text: UNSUPPORTED_IMAGE_TEXT.into(),
                };
            }
            Content::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => {
                for block in blocks {
                    if matches!(block, ToolResultBlock::Image { .. }) {
                        *block = ToolResultBlock::Text {
                            text: UNSUPPORTED_IMAGE_TEXT.into(),
                        };
                    }
                }
            }
            _ => {}
        }
    }
    request
}

fn messages_request(config: &ModelConfig, request: ModelRequest) -> Result<Value, ModelError> {
    validate_reasoning_effort(config, request.reasoning_effort)?;
    let mut messages = Vec::new();
    let mut system = Vec::new();
    for message in request.messages {
        if message
            .content
            .iter()
            .any(|content| matches!(content, Content::ProtocolData { .. }))
        {
            return Err(ModelError::Configuration("history contains protocol-specific content that needs migration before using Messages".into()));
        }
        let content = message
            .content
            .iter()
            .map(messages_content)
            .collect::<Vec<_>>();
        if message.role == Role::System {
            system.extend(content);
        } else {
            messages.push(json!({"role":message.role,"content":content}));
        }
    }
    let mut body = json!({"model":config.model,"max_tokens":config.max_output_tokens,"stream":true,"messages":messages});
    if config.adapter() == Adapter::MinimaxMessages {
        match request.reasoning_effort {
            Some(ReasoningEffort::Off) => body["thinking"] = json!({"type":"disabled"}),
            Some(ReasoningEffort::On) => body["thinking"] = json!({"type":"adaptive"}),
            Some(effort) => {
                body["thinking"] = json!({"type":"adaptive"});
                body["output_config"] = json!({"effort":effort.label()});
            }
            None if cards::model_card(config).is_some_and(|card| card.adaptive_by_default) => {
                body["thinking"] = json!({"type":"adaptive"})
            }
            None => {}
        }
    } else if config.adapter() == Adapter::DeepseekMessages {
        match request.reasoning_effort {
            Some(ReasoningEffort::Off) => body["thinking"] = json!({"type":"disabled"}),
            Some(effort) => {
                body["thinking"] = json!({"type":"enabled"});
                body["output_config"] = json!({"effort":effort.label()});
            }
            None => {}
        }
    } else if let Some(effort) = request.reasoning_effort {
        let budget = cards::model_card(config)
            .and_then(|card| card.thinking_budgets)
            .and_then(|budgets| budgets.for_effort(effort));
        match (effort, budget) {
            (ReasoningEffort::Off, _) => body["thinking"] = json!({"type":"disabled"}),
            // pi keeps room for the answer inside the shared `max_tokens`.
            (_, Some(budget)) => {
                let budget = budget.min(
                    config
                        .max_output_tokens
                        .saturating_sub(cards::MIN_ANSWER_TOKENS),
                );
                body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
            }
            (_, None) => {
                body["thinking"] = json!({"type":"adaptive"});
                body["output_config"] = json!({"effort":effort.label()});
            }
        }
    } else if cards::model_card(config).is_some_and(|card| card.adaptive_by_default) {
        body["thinking"] = json!({"type":"adaptive"});
    }
    // Anthropic rejects a sampling temperature while thinking is on.
    let thinking_on = matches!(
        body["thinking"]["type"].as_str(),
        Some("enabled" | "adaptive")
    );
    if let Some(temperature) = request.temperature
        && !(config.adapter() == Adapter::Messages && thinking_on)
    {
        body["temperature"] = json!(temperature);
    }
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if let Some(format) = request.structured_output {
        if config.adapter() == Adapter::MinimaxMessages {
            if !request.tools.is_empty() {
                return Err(ModelError::Configuration(
                    "MiniMax structured output cannot be combined with executable tools".into(),
                ));
            }
            body["tools"] = json!([{"name":format.name,"description":"Return exactly one structured response using this tool and no other content.","input_schema":format.schema}]);
            body["tool_choice"] = json!({"type":"auto"});
            if body["system"].is_null() {
                body["system"] = json!([]);
            }
            body["system"].as_array_mut().expect("Messages system content is an array").push(json!({"type":"text","text":format!("Call the {} tool exactly once to return the structured response. Emit no ordinary text or other tool calls.", format.name)}));
        } else if config.adapter() == Adapter::DeepseekMessages {
            if !request.tools.is_empty() {
                return Err(ModelError::Configuration(
                    "DeepSeek Messages cannot combine structured output with tools".into(),
                ));
            }
            if request
                .reasoning_effort
                .is_some_and(|effort| effort != ReasoningEffort::Off)
            {
                return Err(ModelError::Configuration(
                    "DeepSeek Messages structured output requires thinking to be disabled".into(),
                ));
            }
            body["thinking"] = json!({"type":"disabled"});
            body["tools"] = json!([{
                "name": format.name,
                "description": "Return the structured response.",
                "input_schema": format.schema
            }]);
            body["tool_choice"] = json!({
                "type": "tool",
                "name": format.name,
                "disable_parallel_tool_use": true
            });
        } else {
            body["output_config"]["format"] = json!({
                "type": "json_schema",
                "schema": format.schema
            });
            if !request.tools.is_empty() {
                body["tools"] = messages_tools(&request.tools);
            }
        }
    } else if !request.tools.is_empty() {
        body["tools"] = messages_tools(&request.tools);
    }
    Ok(body)
}

pub(crate) fn uses_structured_response_tool(config: &ModelConfig, request: &ModelRequest) -> bool {
    matches!(
        config.adapter(),
        Adapter::DeepseekMessages | Adapter::MinimaxMessages
    ) || config.provider_id == "kimi-code-oauth"
        || config.adapter() == Adapter::QwenChatCompletions
            && request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| match block {
                    Content::Image { .. } => true,
                    Content::ToolResult { content, .. } => content.has_images(),
                    _ => false,
                })
}

fn finish_structured_response(
    stream: ProtocolStream,
    policy: Option<&structured_response::StructuredResponsePolicy>,
    codex: bool,
) -> Result<ModelOutput, ModelError> {
    let output = stream.finish(policy)?;
    if let Some(policy) = policy {
        policy.check_output(&output)?;
    }
    if codex {
        return Ok(output);
    }
    normalize_structured_tool_output(
        output,
        policy.and_then(|policy| policy.expected_tool.as_deref()),
    )
    .map_err(structured_response::verified_format_error)
}

fn normalize_structured_tool_output(
    mut output: ModelOutput,
    structured_tool: Option<&str>,
) -> Result<ModelOutput, ModelError> {
    let Some(structured_tool) = structured_tool else {
        return Ok(output);
    };
    if output.content.iter().any(|content| {
        matches!(content, Content::ToolUse { name, .. } if name != structured_tool)
            || matches!(content, Content::ToolResult { .. })
    }) {
        return Err(ModelError::ResponseRejected(
            ModelResponseRejection::UnexpectedToolOperation,
        ));
    }
    let mut structured_inputs = output.content.iter().filter_map(|content| match content {
        Content::ToolUse { name, input, .. } if name == structured_tool => Some(input),
        _ => None,
    });
    let input = structured_inputs
        .next()
        .filter(|_| structured_inputs.next().is_none())
        .ok_or_else(|| invalid_response("structured output tool was not called exactly once"))?;
    if output
        .content
        .iter()
        .any(|content| matches!(content, Content::Text { text } if !text.trim().is_empty()))
    {
        return Err(invalid_response(
            "structured output contained unexpected content",
        ));
    }
    output.content = vec![Content::Text {
        text: input.to_string(),
    }];
    output.stop_reason = "end_turn".into();
    Ok(output)
}

const SCHEMA_COMBINATORS: [&str; 3] = ["oneOf", "anyOf", "allOf"];

fn messages_tools(tools: &[ToolDefinition]) -> Value {
    json!(
        tools
            .iter()
            .map(|tool| json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": messages_input_schema(&tool.input_schema),
            }))
            .collect::<Vec<_>>()
    )
}

/// Anthropic Messages accepts only a plain object at the top of a tool's `input_schema`, so a
/// top-level `oneOf`/`anyOf`/`allOf` (Herdr's ModifyTick has one variant per action) becomes
/// one object: properties are merged in order, fixed values of a shared property form one
/// `enum`, and only properties every alternative requires stay required. The tool still
/// validates its own arguments, and other protocols receive the original schema.
fn messages_input_schema(schema: &Value) -> Value {
    let Some(object) = schema.as_object() else {
        return schema.clone();
    };
    if !SCHEMA_COMBINATORS
        .iter()
        .any(|keyword| object.contains_key(*keyword))
    {
        return schema.clone();
    }
    let mut merged = object.clone();
    let mut properties = match merged.remove("properties") {
        Some(Value::Object(properties)) => properties,
        _ => serde_json::Map::new(),
    };
    let mut required = merged
        .remove("required")
        .and_then(|required| required.as_array().cloned())
        .unwrap_or_default();
    let mut required_by_every_alternative: Option<Vec<Value>> = None;
    let mut every_variant_closed = true;
    for keyword in SCHEMA_COMBINATORS {
        let Some(Value::Array(variants)) = merged.remove(keyword) else {
            continue;
        };
        for variant in variants {
            for (name, property) in variant["properties"].as_object().into_iter().flatten() {
                merge_schema_property(&mut properties, name, property);
            }
            let variant_required = variant["required"].as_array().cloned().unwrap_or_default();
            if keyword == "allOf" {
                required.extend(variant_required);
            } else {
                required_by_every_alternative = Some(match required_by_every_alternative {
                    None => variant_required,
                    Some(previous) => previous
                        .into_iter()
                        .filter(|name| variant_required.contains(name))
                        .collect(),
                });
            }
            every_variant_closed &= variant["additionalProperties"] == false;
        }
    }
    required.extend(required_by_every_alternative.unwrap_or_default());
    let mut unique_required = Vec::new();
    for name in required {
        if !unique_required.contains(&name) {
            unique_required.push(name);
        }
    }
    merged.insert("type".into(), json!("object"));
    merged.insert("properties".into(), Value::Object(properties));
    if unique_required.is_empty() {
        merged.remove("required");
    } else {
        merged.insert("required".into(), Value::Array(unique_required));
    }
    if every_variant_closed && !merged.contains_key("additionalProperties") {
        merged.insert("additionalProperties".into(), json!(false));
    }
    Value::Object(merged)
}

fn merge_schema_property(
    properties: &mut serde_json::Map<String, Value>,
    name: &str,
    incoming: &Value,
) {
    let fixed_values = |property: &Value| {
        property
            .get("const")
            .map(|value| vec![value.clone()])
            .or_else(|| property["enum"].as_array().cloned())
    };
    let Some(existing) = properties.get_mut(name) else {
        properties.insert(name.into(), incoming.clone());
        return;
    };
    let (Some(mut values), Some(added)) = (fixed_values(existing), fixed_values(incoming)) else {
        return;
    };
    for value in added {
        if !values.contains(&value) {
            values.push(value);
        }
    }
    if let Some(property) = existing.as_object_mut() {
        property.remove("const");
        property.insert("enum".into(), Value::Array(values));
        if let Some(kind) = incoming.get("type") {
            property.entry("type").or_insert_with(|| kind.clone());
        }
    }
}

fn messages_content(content: &Content) -> Value {
    match content {
        Content::Image { source, .. } => json!({"type":"image","source":source}),
        Content::ToolResult {
            tool_use_id,
            content: ToolResultContent::Blocks(blocks),
            is_error,
        } => {
            let blocks = blocks
                .iter()
                .map(|block| match block {
                    ToolResultBlock::Image { source, .. } => {
                        json!({"type":"image","source":source})
                    }
                    text => json!(text),
                })
                .collect::<Vec<_>>();
            json!({"type":"tool_result","tool_use_id":tool_use_id,"content":blocks,"is_error":is_error})
        }
        other => json!(other),
    }
}

enum ProtocolStream {
    Messages(MessagesStream),
    Responses(responses::ResponsesStream),
    Chat(chat_completions::ChatStream),
    Google(google::GoogleStream),
}

impl ProtocolStream {
    fn accept(
        &mut self,
        data: Value,
        events: &UnboundedSender<ModelDelta>,
    ) -> Result<bool, ModelError> {
        match self {
            Self::Messages(stream) => stream.accept(data, events),
            Self::Responses(stream) => stream.accept(data, events),
            Self::Chat(stream) => stream.accept(data, events),
            Self::Google(stream) => stream.accept(data, events),
        }
    }

    fn finish(
        self,
        policy: Option<&structured_response::StructuredResponsePolicy>,
    ) -> Result<ModelOutput, ModelError> {
        let output = match self {
            Self::Messages(stream) => stream.finish(),
            Self::Responses(stream) => stream.finish(),
            Self::Chat(stream) => stream.finish_with_policy(policy),
            Self::Google(stream) => stream.finish(),
        };
        output.map_err(|error| match policy {
            Some(policy) => policy.decode_error(error),
            None => error,
        })
    }
}

#[derive(Default)]
struct MessagesStream {
    provisional_input_usage: bool,
    started: bool,
    content: Vec<Content>,
    current: Option<PendingBlock>,
    usage: Usage,
    stop_reason: Option<String>,
}

struct PendingBlock {
    content: Content,
    partial_json: String,
}

impl MessagesStream {
    fn accept(
        &mut self,
        data: Value,
        events: &UnboundedSender<ModelDelta>,
    ) -> Result<bool, ModelError> {
        let event_type = required_string(&data, "type")?;
        match event_type {
            "message_start" => {
                if self.started {
                    return Err(invalid_response("duplicate message start"));
                }
                self.started = true;
                let observed = observe_usage(
                    &mut self.usage,
                    &data["message"]["usage"],
                    Protocol::Messages,
                    (!self.provisional_input_usage).then_some(events),
                );
                if self.provisional_input_usage && self.usage.output_tokens.is_some() {
                    let _ = events.send(ModelDelta::Usage(Usage {
                        output_tokens: self.usage.output_tokens,
                        ..Usage::default()
                    }));
                }
                observed?;
            }
            "content_block_start" => {
                self.check_index(&data)?;
                if self.current.is_some() || self.stop_reason.is_some() {
                    return Err(invalid_response("overlapping or late content block"));
                }
                let block = &data["content_block"];
                let content = match required_string(block, "type")? {
                    "text" => {
                        let text = required_string(block, "text")?.to_owned();
                        if !text.is_empty() {
                            let _ = events.send(ModelDelta::Text(text.clone()));
                        }
                        Content::Text { text }
                    }
                    "thinking" => {
                        let thinking = required_string(block, "thinking")?.to_owned();
                        if !thinking.is_empty() {
                            let _ = events.send(ModelDelta::Thinking(thinking.clone()));
                        }
                        Content::Thinking {
                            thinking,
                            signature: block["signature"].as_str().map(str::to_owned),
                        }
                    }
                    "tool_use" => {
                        let id = required_string(block, "id")?.to_owned();
                        let name = required_string(block, "name")?.to_owned();
                        if id.is_empty() || name.is_empty() || self.content.iter().any(|item| matches!(item, Content::ToolUse { id: previous, .. } if previous == &id)) {
                            return Err(invalid_response("missing or duplicate tool call identity"));
                        }
                        let input = block["input"].clone();
                        if !input.is_object() {
                            return Err(invalid_response("tool input must be an object"));
                        }
                        Content::ToolUse { id, name, input }
                    }
                    _ => return Err(invalid_response("unsupported content block type")),
                };
                self.current = Some(PendingBlock {
                    content,
                    partial_json: String::new(),
                });
            }
            "content_block_delta" => {
                self.check_index(&data)?;
                let current = self
                    .current
                    .as_mut()
                    .ok_or_else(|| invalid_response("content delta has no open block"))?;
                let delta = &data["delta"];
                match (&mut current.content, required_string(delta, "type")?) {
                    (Content::Text { text }, "text_delta") => {
                        let value = required_string(delta, "text")?;
                        text.push_str(value);
                        let _ = events.send(ModelDelta::Text(value.into()));
                    }
                    (Content::Thinking { thinking, .. }, "thinking_delta") => {
                        let value = required_string(delta, "thinking")?;
                        thinking.push_str(value);
                        let _ = events.send(ModelDelta::Thinking(value.into()));
                    }
                    (Content::Thinking { signature, .. }, "signature_delta") => {
                        signature
                            .get_or_insert_default()
                            .push_str(required_string(delta, "signature")?);
                    }
                    (Content::ToolUse { .. }, "input_json_delta") => {
                        current
                            .partial_json
                            .push_str(required_string(delta, "partial_json")?);
                    }
                    _ => return Err(invalid_response("content delta does not match its block")),
                }
            }
            "content_block_stop" => {
                self.check_index(&data)?;
                let mut current = self
                    .current
                    .take()
                    .ok_or_else(|| invalid_response("content stop has no open block"))?;
                if let Content::ToolUse { input, .. } = &mut current.content
                    && !current.partial_json.is_empty()
                {
                    if input.as_object().is_some_and(|object| !object.is_empty()) {
                        return Err(invalid_response("tool input was supplied twice"));
                    }
                    *input = serde_json::from_str(&current.partial_json).map_err(|_| {
                        invalid_response("tool input is incomplete or invalid JSON")
                    })?;
                    if !input.is_object() {
                        return Err(invalid_response("tool input must be an object"));
                    }
                }
                self.content.push(current.content);
            }
            "message_delta" => {
                if !self.started {
                    return Err(invalid_response(
                        "message delta arrived before message start",
                    ));
                }
                let mut final_usage = data["usage"].clone();
                if self.provisional_input_usage {
                    // MiniMax may reclassify provisional cached input as uncached at stream end.
                    // Publish input counters only after this correction, keeping output monotonic.
                    if final_usage.is_null() {
                        final_usage = json!({});
                    }
                    for (name, provisional) in [
                        ("input_tokens", &mut self.usage.input_tokens),
                        (
                            "cache_read_input_tokens",
                            &mut self.usage.cache_read_input_tokens,
                        ),
                        (
                            "cache_creation_input_tokens",
                            &mut self.usage.cache_write_input_tokens,
                        ),
                    ] {
                        if final_usage.is_object()
                            && (final_usage.get(name).is_none()
                                || name != "input_tokens" && final_usage[name].is_null())
                            && let Some(value) = provisional.take()
                        {
                            final_usage[name] = json!(value);
                        }
                        *provisional = None;
                    }
                    self.provisional_input_usage = false;
                }
                observe_usage(
                    &mut self.usage,
                    &final_usage,
                    Protocol::Messages,
                    Some(events),
                )?;
                if self.current.is_some() {
                    return Err(invalid_response(
                        "message delta arrived before content completed",
                    ));
                }
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.into());
                }
            }
            "message_stop" => {
                if !self.started || self.current.is_some() || self.stop_reason.is_none() {
                    return Err(invalid_response(
                        "message stopped without complete content and stop reason",
                    ));
                }
                return Ok(true);
            }
            "error" => {
                return Err(stream_provider_error(&data));
            }
            _ => {}
        }
        Ok(false)
    }

    fn check_index(&self, data: &Value) -> Result<(), ModelError> {
        if !self.started || data["index"].as_u64() != Some(self.content.len() as u64) {
            return Err(invalid_response(
                "content block index is missing, repeated or out of order",
            ));
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelOutput, ModelError> {
        let stop_reason = self
            .stop_reason
            .ok_or_else(|| invalid_response("missing stop reason"))?;
        match stop_reason.as_str() {
            "end_turn" | "stop_sequence" | "tool_use" => {}
            "max_tokens" => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::OutputLimit,
                ));
            }
            "model_context_window_exceeded" => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::ContextLimit,
                ));
            }
            "refusal" => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::Refusal,
                ));
            }
            "pause_turn" => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::UnsupportedContinuation,
                ));
            }
            _ => {
                return Err(invalid_response(
                    "message ended with an unsupported stop reason",
                ));
            }
        }
        let has_tool_calls = self
            .content
            .iter()
            .any(|content| matches!(content, Content::ToolUse { .. }));
        if has_tool_calls != (stop_reason == "tool_use") {
            return Err(invalid_response(
                "tool calls do not match the message stop reason",
            ));
        }
        Ok(ModelOutput {
            private_source: None,
            content: self.content,
            usage: self.usage,
            stop_reason,
        })
    }
}

fn required_string<'a>(value: &'a Value, name: &str) -> Result<&'a str, ModelError> {
    value[name]
        .as_str()
        .ok_or_else(|| invalid_response("required string field is missing"))
}

/// OpenAI-style protocols accept images only in user turns (DeepSeek answers HTTP 400 otherwise).
fn image_outside_user_message() -> ModelError {
    ModelError::Configuration("images are only supported in user messages".into())
}

fn invalid_response(message: &str) -> ModelError {
    ModelError::InvalidResponse(message.into())
}

fn observe_usage(
    usage: &mut Usage,
    value: &Value,
    protocol: Protocol,
    events: Option<&UnboundedSender<ModelDelta>>,
) -> Result<(), ModelError> {
    if value.is_null() {
        return Ok(());
    }
    if !value.is_object() {
        return Err(invalid_response("usage is not an object"));
    }
    let mut observation = Usage::default();
    let mut failure = None;
    let (input_name, output_name) = match protocol {
        Protocol::Messages | Protocol::Responses | Protocol::Google | Protocol::SystemOne => {
            ("input_tokens", "output_tokens")
        }
        Protocol::ChatCompletions => ("prompt_tokens", "completion_tokens"),
    };
    for (name, target, nullable) in [
        (input_name, &mut observation.input_tokens, false),
        (output_name, &mut observation.output_tokens, false),
        (
            if protocol == Protocol::ChatCompletions {
                "prompt_cache_hit_tokens"
            } else {
                "cache_read_input_tokens"
            },
            &mut observation.cache_read_input_tokens,
            true,
        ),
    ] {
        parse_usage_counter(target, value.get(name), nullable, &mut failure);
    }
    if protocol != Protocol::ChatCompletions {
        parse_usage_counter(
            &mut observation.cache_write_input_tokens,
            value.get("cache_creation_input_tokens"),
            true,
            &mut failure,
        );
    }
    let details_name = match protocol {
        Protocol::Messages | Protocol::Google | Protocol::SystemOne => None,
        Protocol::Responses => Some("input_tokens_details"),
        Protocol::ChatCompletions => Some("prompt_tokens_details"),
    };
    if let Some(details) = details_name
        .and_then(|name| value.get(name))
        .filter(|value| !value.is_null())
    {
        if details.is_object() {
            for (name, target) in [
                ("cached_tokens", &mut observation.cache_read_input_tokens),
                (
                    "cache_write_tokens",
                    &mut observation.cache_write_input_tokens,
                ),
            ] {
                parse_usage_counter(target, details.get(name), true, &mut failure);
            }
        } else {
            failure.get_or_insert_with(|| invalid_response("usage details are not an object"));
        }
    }
    let mut has_observation = false;
    for (target, observed) in [
        (&mut usage.input_tokens, observation.input_tokens),
        (&mut usage.output_tokens, observation.output_tokens),
        (
            &mut usage.cache_read_input_tokens,
            observation.cache_read_input_tokens,
        ),
        (
            &mut usage.cache_write_input_tokens,
            observation.cache_write_input_tokens,
        ),
    ] {
        if let Some(observed) = observed {
            if target.is_some_and(|previous| observed < previous) {
                failure
                    .get_or_insert_with(|| invalid_response("cumulative usage counter decreased"));
            }
            *target = Some(observed);
            has_observation = true;
        }
    }
    if has_observation && let Some(events) = events {
        // Report validated fields before a protocol failure so the host can retain them.
        let _ = events.send(ModelDelta::Usage(usage.clone()));
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn parse_usage_counter(
    target: &mut Option<u64>,
    value: Option<&Value>,
    nullable: bool,
    failure: &mut Option<ModelError>,
) {
    let Some(value) = value.filter(|value| !(nullable && value.is_null())) else {
        return;
    };
    match value.as_u64() {
        Some(count) => *target = Some(count),
        None => {
            failure.get_or_insert_with(|| {
                invalid_response("usage counter is not a nonnegative integer")
            });
        }
    }
}

fn api_key(
    config: &ModelConfig,
    state_root: Option<&Path>,
) -> Result<Option<reqwest::header::HeaderValue>, ModelError> {
    if config.api_key_env.is_empty() {
        return Ok(None);
    }
    let saved = state_root
        .map(|root| credentials::load_api_key(root, &config.api_key_env))
        .transpose()
        .map_err(|_| ModelError::Configuration("saved API key could not be read safely".into()))?
        .flatten();
    let raw = saved
        .map(Ok)
        .unwrap_or_else(|| std::env::var(&config.api_key_env))
        .map_err(|_| {
            ModelError::Configuration(
                "configured API key environment variable is unset or not valid Unicode".into(),
            )
        })?;
    if raw.trim().is_empty() {
        return Err(ModelError::Configuration(
            "configured API key environment variable is empty".into(),
        ));
    }
    let mut key = reqwest::header::HeaderValue::from_str(&raw).map_err(|_| {
        ModelError::Configuration("configured API key cannot be used as an HTTP header".into())
    })?;
    key.set_sensitive(true);
    Ok(Some(key))
}

#[cfg(any(test, feature = "test-support"))]
pub fn fixture_client(client: Arc<dyn ModelClient>) -> Arc<dyn ModelClient> {
    struct FixtureClient(Arc<dyn ModelClient>);
    #[async_trait]
    impl ModelClient for FixtureClient {
        fn identity(&self) -> ModelIdentity {
            self.0.identity()
        }
        async fn generate_title(
            &self,
            request: ModelRequest,
            _: UnboundedSender<ModelDelta>,
            _: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            Ok(fixture_title(request))
        }
        async fn close_chat(&self, chat: tickloop_agent_protocol::ChatId) {
            self.0.close_chat(chat).await;
        }
        async fn complete(
            &self,
            request: ModelRequest,
            events: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            self.0.complete(request, events, cancel).await
        }
        async fn complete_for_chat(
            &self,
            chat: tickloop_agent_protocol::ChatId,
            request: ModelRequest,
            events: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            self.0
                .complete_for_chat(chat, request, events, cancel)
                .await
        }
        async fn complete_for_chat_with_context(
            &self,
            chat: tickloop_agent_protocol::ChatId,
            request: ModelRequest,
            events: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
            context: ModelRequestContext,
        ) -> Result<ModelOutput, ModelError> {
            self.0
                .complete_for_chat_with_context(chat, request, events, cancel, context)
                .await
        }
    }
    Arc::new(FixtureClient(client))
}

#[cfg(any(test, feature = "test-support"))]
pub fn fixture_title(request: ModelRequest) -> ModelOutput {
    let content = request
        .messages
        .into_iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.content)
        .unwrap_or_default();
    ModelOutput {
        private_source: None,
        content,
        usage: Usage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
        },
        stop_reason: "end_turn".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tickloop_agent_protocol::InputModality;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};

    fn structured_request() -> ModelRequest {
        ModelRequest {
            history_sources: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "Return JSON".into(),
                }],
            }],
            tools: Vec::new(),
            structured_output: Some(StructuredOutput {
                name: "verdict".into(),
                schema: json!({
                    "type": "object",
                    "properties": {"decision": {"type": "string"}},
                    "required": ["decision"],
                    "additionalProperties": false
                }),
            }),
            temperature: None,
            reasoning_effort: None,
        }
    }

    fn protocol_config(protocol: Protocol, base_url: &str) -> ModelConfig {
        ModelConfig {
            provider: ModelProvider::Api,
            auth_source: ModelAuthSource::ApiKeyEnv,
            name: "fixture".into(),
            protocol,
            base_url: base_url.into(),
            model: "fixture-model".into(),
            api_key_env: String::new(),
            max_output_tokens: 32,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: String::new(),
            connection: None,
            card: None,
            input_modalities: vec![InputModality::Text],
        }
    }

    fn default_model(provider: &str, model: &str) -> ModelConfig {
        cards::ModelCatalog::defaults()
            .get(provider, model)
            .unwrap()
    }

    #[test]
    fn known_deepseek_models_share_the_published_context_window() {
        for model in ["deepseek-flash-messages", "deepseek-v4-pro-messages"] {
            let config = default_model("deepseek-api", model);
            assert_eq!(config.maximum_context_window_tokens(), Some(1_000_000));
        }
        let mut config = default_model("deepseek-api", "deepseek-v4-flash-messages");
        assert_eq!(config.maximum_context_window_tokens(), None);
        config.base_url = "https://gateway.example/anthropic".into();
        assert!(validate_model_config(&config).is_ok());
    }

    #[test]
    fn private_history_source_distinguishes_provider_adapter_and_nondefault_port() {
        let mut source = default_model("deepseek-api", "deepseek-flash-messages");
        source.base_url = "https://api.deepseek.com:443/anthropic/".into();
        let mut same = source.clone();
        same.base_url = "https://api.deepseek.com/anthropic".into();
        assert_eq!(
            source.private_history_source(),
            same.private_history_source()
        );

        same.base_url = "https://api.deepseek.com:8443/anthropic".into();
        assert_ne!(
            source.private_history_source(),
            same.private_history_source()
        );
        same.base_url = "https://api.deepseek.com:9443/anthropic".into();
        assert_ne!(
            source.private_history_source(),
            same.private_history_source()
        );

        same = source.clone();
        same.provider_id = "another-provider".into();
        assert!(!compatible_private_history(&source, &same));
        same = source.clone();
        same.card.as_mut().unwrap().adapter = Adapter::Messages;
        assert!(!compatible_private_history(&source, &same));
        same = source.clone();
        same.api_key_env = "OTHER_API_KEY".into();
        assert!(!compatible_private_history(&source, &same));
    }

    #[test]
    fn credential_rotation_cannot_be_laundered_by_another_chats_public_request() {
        let mut config = protocol_config(Protocol::Messages, "http://127.0.0.1:49321");
        config.provider_id = "credential-rotation-test".into();
        config.api_key_env = "TICKLOOP_TEST_CREDENTIAL_ROTATION".into();
        let first_chat = HttpModelClient::deferred(config.clone()).unwrap();
        let second_chat = HttpModelClient::deferred(config).unwrap();
        let public = structured_request();
        let private_content = Content::Thinking {
            thinking: "private reasoning".into(),
            signature: None,
        };
        let mut private = public.clone();
        private.messages.push(Message {
            role: Role::Assistant,
            content: vec![private_content.clone()],
        });
        let first_key = reqwest::header::HeaderValue::from_static("first-secret-key");
        let second_key = reqwest::header::HeaderValue::from_static("second-secret-key");
        let chat_a = uuid::Uuid::new_v4();
        let chat_b = uuid::Uuid::new_v4();
        let output = ModelOutput {
            private_source: None,
            content: vec![private_content],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        };
        first_chat.record_private_output(Some(chat_a), Some(&first_key), &output);
        first_chat
            .ensure_credential_history_compatible(Some(chat_a), &private, Some(&first_key))
            .unwrap();
        second_chat
            .ensure_credential_history_compatible(Some(chat_b), &public, Some(&second_key))
            .unwrap();
        second_chat.record_private_output(Some(chat_b), Some(&second_key), &output);
        second_chat
            .ensure_credential_history_compatible(Some(chat_b), &private, Some(&second_key))
            .unwrap();
        let error = first_chat
            .ensure_credential_history_compatible(Some(chat_a), &private, Some(&second_key))
            .unwrap_err();
        assert_eq!(error.kind(), ModelFailureKind::Configuration);
        assert!(!format!("{error:?}").contains("secret-key"));
        first_chat
            .ensure_credential_history_compatible(Some(chat_a), &private, Some(&first_key))
            .unwrap();

        let mut unknown = protocol_config(Protocol::Messages, "http://127.0.0.1:49322");
        unknown.api_key_env = "TICKLOOP_TEST_UNKNOWN_CREDENTIAL".into();
        let unknown = HttpModelClient::deferred(unknown).unwrap();
        let unknown_chat = uuid::Uuid::new_v4();
        assert!(
            unknown
                .ensure_credential_history_compatible(
                    Some(unknown_chat),
                    &private,
                    Some(&first_key)
                )
                .is_err()
        );
        assert!(
            unknown
                .ensure_credential_history_compatible(Some(unknown_chat), &public, Some(&first_key))
                .is_ok()
        );
        assert!(
            unknown
                .ensure_credential_history_compatible(
                    Some(unknown_chat),
                    &private,
                    Some(&first_key)
                )
                .is_err()
        );
    }

    #[test]
    fn credential_provenance_uses_the_same_normalized_endpoint_as_private_history() {
        let original = default_model("anthropic-api", "claude-sonnet-4-6-messages");
        let mut equivalent = original.clone();
        equivalent.base_url = format!("{}:443", original.base_url);
        let first = HttpModelClient::deferred(original).unwrap();
        let second = HttpModelClient::deferred(equivalent).unwrap();
        assert_eq!(first.credential_scope(), second.credential_scope());
    }

    #[test]
    fn reasoning_effort_reaches_each_deepseek_protocol() {
        let mut request = structured_request();
        request.structured_output = None;
        request.reasoning_effort = Some(ReasoningEffort::Max);
        let mut config = default_model("deepseek-api", "deepseek-flash-messages");

        let messages = messages_request(&config, request.clone()).unwrap();
        assert_eq!(messages["thinking"], json!({"type":"enabled"}));
        assert_eq!(messages["output_config"]["effort"], "max");

        config = default_model("deepseek-api", "deepseek-flash-responses");
        let responses = responses::encode_request(&config, request.clone()).unwrap();
        assert_eq!(responses["reasoning"]["effort"], "max");

        config = default_model("deepseek-api", "deepseek-flash-chat-completions");
        let chat = chat_completions::encode_request(&config, request.clone()).unwrap();
        assert_eq!(chat["thinking"], json!({"type":"enabled"}));
        assert_eq!(chat["reasoning_effort"], "max");

        request.reasoning_effort = Some(ReasoningEffort::Off);
        assert_eq!(
            chat_completions::encode_request(&config, request.clone()).unwrap()["thinking"],
            json!({"type":"disabled"})
        );
        config = default_model("deepseek-api", "deepseek-flash-responses");
        assert_eq!(
            responses::encode_request(&config, request).unwrap()["reasoning"]["effort"],
            "none"
        );
    }

    #[test]
    fn effort_choices_match_deepseek_and_claude_capabilities() {
        let mut config = default_model("deepseek-api", "deepseek-v4-pro-messages");
        assert_eq!(
            ReasoningEffort::choices(&config),
            &[
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ]
        );

        config = default_model("anthropic-api", "claude-sonnet-5-messages");
        assert_eq!(
            ReasoningEffort::choices(&config),
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Xhigh,
                ReasoningEffort::Max,
            ]
        );

        config = default_model("anthropic-api", "claude-opus-4-6-messages");
        assert_eq!(ReasoningEffort::choices(&config).len(), 4);
        config = default_model("anthropic-api", "claude-opus-5-messages");
        assert_eq!(ReasoningEffort::choices(&config).len(), 5);
        config = default_model("anthropic-api", "claude-fable-5-messages");
        assert!(validate_reasoning_effort(&config, Some(ReasoningEffort::Off)).is_err());
    }

    #[test]
    fn model_card_lists_preferred_deepseek_protocol_and_keeps_legacy_adapters() {
        let profiles = cards::built_in_model_profiles();
        for model in ["deepseek-flash", "deepseek-v4-pro"] {
            let protocols = profiles
                .iter()
                .filter(|profile| profile.model == model)
                .map(|profile| profile.protocol)
                .collect::<Vec<_>>();
            assert_eq!(protocols, [Protocol::ChatCompletions]);
            let legacy_protocols = cards::ModelCatalog::defaults()
                .profiles()
                .into_iter()
                .filter(|profile| profile.model == model)
                .map(|profile| profile.protocol)
                .collect::<Vec<_>>();
            assert_eq!(
                legacy_protocols,
                [
                    Protocol::Messages,
                    Protocol::Responses,
                    Protocol::ChatCompletions
                ]
            );
        }

        let config = default_model("anthropic-api", "claude-opus-5-messages");
        assert_eq!(config.protocol, Protocol::Messages);
        let config = default_model("anthropic-api", "claude-fable-5-messages");
        assert_eq!(
            cards::model_card(&config).unwrap().thinking,
            ThinkingControl::AlwaysOn
        );
    }

    #[test]
    fn claude_messages_can_disable_thinking_without_an_effort_level() {
        let mut config = default_model("anthropic-api", "claude-opus-5-messages");
        let mut request = structured_request();
        let default_body = messages_request(&config, request.clone()).unwrap();
        assert_eq!(default_body["thinking"], json!({"type":"adaptive"}));
        config = default_model("anthropic-api", "claude-opus-4-8-messages");
        assert_eq!(
            messages_request(&config, request.clone()).unwrap()["thinking"],
            json!({"type":"adaptive"})
        );
        config = default_model("anthropic-api", "claude-opus-5-messages");
        request.reasoning_effort = Some(ReasoningEffort::Off);
        let body = messages_request(&config, request).unwrap();
        assert_eq!(body["thinking"], json!({"type":"disabled"}));
        assert!(body["output_config"].get("effort").is_none());
    }

    #[test]
    fn unsupported_effort_is_rejected_before_sending() {
        let mut request = structured_request();
        request.structured_output = None;
        request.reasoning_effort = Some(ReasoningEffort::Medium);
        let mut config = default_model("deepseek-api", "deepseek-flash-messages");
        assert!(matches!(
            messages_request(&config, request.clone()),
            Err(ModelError::Configuration(_))
        ));
        config = default_model("anthropic-api", "claude-sonnet-4-6-messages");
        let body = messages_request(&config, request).unwrap();
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    #[test]
    fn claude_effort_and_structured_output_share_output_config() {
        let mut request = structured_request();
        request.reasoning_effort = Some(ReasoningEffort::Medium);
        let config = default_model("anthropic-api", "claude-sonnet-4-6-messages");
        let body = messages_request(&config, request).unwrap();
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn native_protocols_encode_json_schema_output_constraints() {
        let schema = structured_request().structured_output.unwrap().schema;
        let messages = messages_request(
            &protocol_config(Protocol::Messages, "http://127.0.0.1"),
            structured_request(),
        )
        .unwrap();
        assert_eq!(
            messages["output_config"]["format"],
            json!({"type":"json_schema","schema":schema.clone()})
        );

        let responses = responses::encode_request(
            &protocol_config(Protocol::Responses, "http://127.0.0.1"),
            structured_request(),
        )
        .unwrap();
        assert_eq!(
            responses["text"]["format"],
            json!({"type":"json_schema","name":"verdict","schema":schema.clone(),"strict":true})
        );

        let chat = chat_completions::encode_request(
            &protocol_config(Protocol::ChatCompletions, "http://127.0.0.1"),
            structured_request(),
        )
        .unwrap();
        assert_eq!(
            chat["response_format"],
            json!({
                "type":"json_schema",
                "json_schema":{"name":"verdict","schema":schema,"strict":true}
            })
        );
    }

    #[test]
    fn deepseek_compatibility_protocols_use_their_supported_structured_channels() {
        let mut messages_config = default_model("deepseek-api", "deepseek-flash-messages");
        messages_config.base_url = "https://compatible.example/anthropic".into();
        let messages = messages_request(&messages_config, structured_request()).unwrap();
        assert_eq!(messages["tools"][0]["name"], "verdict");
        assert_eq!(messages["thinking"], json!({"type":"disabled"}));
        assert_eq!(
            messages["tool_choice"],
            json!({"type":"tool","name":"verdict","disable_parallel_tool_use":true})
        );
        assert!(messages.get("output_config").is_none());

        let mut thinking_request = structured_request();
        thinking_request.reasoning_effort = Some(ReasoningEffort::High);
        assert!(matches!(
            messages_request(&messages_config, thinking_request,),
            Err(ModelError::Configuration(_))
        ));

        let mut chat_config = default_model("deepseek-api", "deepseek-flash-chat-completions");
        chat_config.base_url = "https://compatible.example".into();
        let chat = chat_completions::encode_request(&chat_config, structured_request()).unwrap();
        assert_eq!(chat["response_format"], json!({"type":"json_object"}));
        let schema = structured_request()
            .structured_output
            .unwrap()
            .schema
            .to_string();
        assert!(
            chat["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains(&schema))
            }),
            "JSON mode must communicate the result schema to DeepSeek"
        );

        let generic = chat_completions::encode_request(
            &protocol_config(Protocol::ChatCompletions, "https://api.deepseek.com"),
            structured_request(),
        )
        .unwrap();
        assert_eq!(generic["response_format"]["type"], "json_schema");
    }

    #[test]
    fn deepseek_messages_normalizes_the_forced_output_tool_to_json_text() {
        let output = normalize_structured_tool_output(
            ModelOutput {
                private_source: None,
                content: vec![Content::ToolUse {
                    id: "tool-1".into(),
                    name: "verdict".into(),
                    input: json!({"decision":"allow"}),
                }],
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
            },
            Some("verdict"),
        )
        .unwrap();
        assert_eq!(
            output.content,
            vec![Content::Text {
                text: "{\"decision\":\"allow\"}".into()
            }]
        );
        assert_eq!(output.stop_reason, "end_turn");
    }

    #[tokio::test]
    async fn reqwest_header_and_stream_timeouts_remain_local_timeouts() {
        for send_stream_headers in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (release, released) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                assert!(socket.read(&mut buffer).await.unwrap() > 0);
                if send_stream_headers {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100\r\n\r\n").await.unwrap();
                }
                let _ = released.await;
            });
            let client = HttpModelClient {
                cache_id: uuid::Uuid::new_v4(),
                config: ModelConfig {
                    provider: ModelProvider::Api,
                    auth_source: ModelAuthSource::ApiKeyEnv,
                    name: "timeout fixture".into(),
                    protocol: Protocol::Messages,
                    base_url: format!("http://{address}"),
                    model: "fixture-model".into(),
                    api_key_env: String::new(),
                    max_output_tokens: 32,
                    provider_id: String::new(),
                    provider_name: String::new(),
                    model_id: String::new(),
                    connection: None,
                    card: None,
                    input_modalities: vec![InputModality::Text],
                },
                client: reqwest::Client::builder()
                    .no_proxy()
                    .read_timeout(Duration::from_millis(50))
                    .build()
                    .unwrap(),
                route: tickloop_agent_base::network::NetworkRoute {
                    proxy_source: tickloop_agent_base::network::ProxySource::Direct,
                    direct_websocket_eligible: true,
                },
                state_root: None,
            };
            let (events, mut receiver) = mpsc::unbounded_channel();
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                client.complete(
                    ModelRequest {
                        history_sources: Vec::new(),
                        messages: vec![],
                        tools: vec![],
                        structured_output: None,
                        temperature: None,
                        reasoning_effort: None,
                    },
                    events,
                    CancellationToken::new(),
                ),
            )
            .await;
            let _ = release.send(());
            server.await.unwrap();
            let expected_stage = if send_stream_headers {
                ModelRequestStage::ResponseStream
            } else {
                ModelRequestStage::ResponseHeaders
            };
            assert!(
                matches!(result.unwrap(), Err(ModelError::TimedOutAt { stage }) if stage == expected_stage),
                "headers sent: {send_stream_headers}"
            );
            assert!(receiver.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn refused_loopback_connection_is_reported_at_connect_stage() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = HttpModelClient::new(protocol_config(
            Protocol::Messages,
            &format!("http://{address}"),
        ))
        .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(
                ModelRequest {
                    history_sources: Vec::new(),
                    messages: vec![],
                    tools: vec![],
                    structured_output: None,
                    temperature: None,
                    reasoning_effort: None,
                },
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.stage(), ModelRequestStage::Connect);
        assert_eq!(error.kind(), ModelFailureKind::Connection);
    }
}
