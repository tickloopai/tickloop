use std::{
    collections::{BTreeMap, HashSet},
    sync::OnceLock,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    InputModality, ModelAuthSource, ModelConfig, ModelProvider, Protocol, ReasoningEffort,
    text_only, validate_model_config,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingControl {
    #[default]
    Unsupported,
    Toggle,
    AlwaysOn,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Adapter {
    Messages,
    DeepseekMessages,
    MinimaxMessages,
    Responses,
    ChatCompletions,
    DeepseekChatCompletions,
    GlmChatCompletions,
    CodexResponses,
    Google,
    KimiChatCompletions,
    QwenChatCompletions,
    SystemOne,
}

impl Adapter {
    pub const fn protocol(self) -> Protocol {
        match self {
            Self::Google => Protocol::Google,
            Self::SystemOne => Protocol::SystemOne,
            Self::Messages | Self::DeepseekMessages | Self::MinimaxMessages => Protocol::Messages,
            Self::Responses | Self::CodexResponses => Protocol::Responses,
            Self::ChatCompletions
            | Self::DeepseekChatCompletions
            | Self::GlmChatCompletions
            | Self::KimiChatCompletions
            | Self::QwenChatCompletions => Protocol::ChatCompletions,
        }
    }
}

impl From<Protocol> for Adapter {
    fn from(protocol: Protocol) -> Self {
        match protocol {
            Protocol::Google => Self::Google,
            Protocol::SystemOne => Self::SystemOne,
            Protocol::Messages => Self::Messages,
            Protocol::Responses => Self::Responses,
            Protocol::ChatCompletions => Self::ChatCompletions,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCard {
    pub id: String,
    pub service_model_id: String,
    pub display_name: String,
    pub adapter: Adapter,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default)]
    pub context_window_tokens: Option<usize>,
    #[serde(default)]
    pub thinking: ThinkingControl,
    #[serde(default)]
    pub adaptive_by_default: bool,
    #[serde(default)]
    pub effort_levels: Vec<ReasoningEffort>,
    /// Omitted for models whose image input is not verified; like opencode, they stay text only.
    #[serde(default = "text_only")]
    pub input_modalities: Vec<InputModality>,
    /// Messages models without adaptive thinking take a token budget per effort level instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// Anthropic's minimum `budget_tokens`.
pub const MIN_THINKING_BUDGET_TOKENS: u32 = 1024;
/// Output kept for the answer when a thinking budget shares `max_tokens`, as pi does.
pub const MIN_ANSWER_TOKENS: u32 = 1024;

/// Manual thinking budgets for the effort levels a budget-only Messages model offers; pi uses
/// low 2048, medium 8192 and high 16384. Higher levels are not offered for such models.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingBudgets {
    pub low: u32,
    pub medium: u32,
    pub high: u32,
}

impl ThinkingBudgets {
    pub fn for_effort(self, effort: ReasoningEffort) -> Option<u32> {
        match effort {
            ReasoningEffort::Low => Some(self.low),
            ReasoningEffort::Medium => Some(self.medium),
            ReasoningEffort::High => Some(self.high),
            _ => None,
        }
    }
}

const fn default_max_output_tokens() -> u32 {
    4096
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDefinition {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_aliases: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<super::connection::ProviderConnection>,
    pub id: String,
    pub display_name: String,
    pub auth: ModelAuthSource,
    #[serde(default)]
    pub api_key_env: String,
    pub endpoints: BTreeMap<Adapter, String>,
    pub models: Vec<ModelCard>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_aliases: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<super::connection::ProviderConnection>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<ModelAuthSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<BTreeMap<Adapter, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelPatch>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPatch {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Adapter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable_context_window"
    )]
    pub context_window_tokens: Option<Option<usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adaptive_by_default: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort_levels: Option<Vec<ReasoningEffort>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<InputModality>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
}

fn deserialize_nullable_context_window<'de, D>(
    deserializer: D,
) -> Result<Option<Option<usize>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<usize>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    pub providers: Vec<ProviderDefinition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
}

impl ModelCatalog {
    pub fn defaults() -> Self {
        let providers = [
            include_str!("providers/deepseek-api.yaml"),
            include_str!("providers/anthropic-api.yaml"),
            include_str!("providers/openai-api.yaml"),
            include_str!("providers/openai-codex.yaml"),
            include_str!("providers/minimax-cn-api.yaml"),
            include_str!("providers/minimax-global-api.yaml"),
            include_str!("providers/minimax-cn-oauth.yaml"),
            include_str!("providers/minimax-global-oauth.yaml"),
            include_str!("providers/zhipu-cn-api.yaml"),
            include_str!("providers/zhipu-cn-coding.yaml"),
            include_str!("providers/zai-global-api.yaml"),
            include_str!("providers/zai-global-coding.yaml"),
            include_str!("providers/github-copilot.yaml"),
            include_str!("providers/kimi-cn-api.yaml"),
            include_str!("providers/kimi-global-api.yaml"),
            include_str!("providers/kimi-code-oauth.yaml"),
            include_str!("providers/alibaba-cn-api.yaml"),
            include_str!("providers/google-gemini-api.yaml"),
            include_str!("providers/google-gemini-oauth.yaml"),
            include_str!("providers/google-vertex-service-account.yaml"),
            include_str!("providers/google-vertex-oauth.yaml"),
            include_str!("providers/xai-api.yaml"),
            include_str!("providers/xai-oauth.yaml"),
        ]
        .into_iter()
        .map(|yaml| serde_yaml_ng::from_str(yaml).expect("bundled Provider YAML is valid"))
        .collect();
        Self { providers }
    }

    pub fn from_providers(providers: Vec<ProviderDefinition>) -> Result<Self, String> {
        let catalog = Self { providers };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut provider_ids = HashSet::new();
        for provider in &self.providers {
            let location = format!("providers[{}]", provider.id);
            if !valid_provider_id(&provider.id) || provider.display_name.trim().is_empty() {
                return Err(format!(
                    "{location}: id must be a safe lowercase slug and display_name is required"
                ));
            }
            if !provider_ids.insert(&provider.id) {
                return Err(format!("{location}: duplicate provider id"));
            }
            if provider.auth == ModelAuthSource::CodexSubscription && provider.id != "openai-codex"
            {
                return Err(format!(
                    "{location}.auth: Codex subscription is restricted to openai-codex"
                ));
            }
            if provider.id == "openai-codex" && provider.auth != ModelAuthSource::CodexSubscription
            {
                return Err(format!(
                    "{location}.auth: openai-codex requires Codex subscription"
                ));
            }
            if let Some(region) = crate::auth::minimax::MiniMaxRegion::from_auth(provider.auth) {
                let expected = match region {
                    crate::auth::minimax::MiniMaxRegion::China => "minimax-cn-oauth",
                    crate::auth::minimax::MiniMaxRegion::Global => "minimax-global-oauth",
                };
                if provider.id != expected
                    || !provider.api_key_env.is_empty()
                    || provider
                        .models
                        .iter()
                        .any(|model| model.adapter != Adapter::MinimaxMessages)
                {
                    return Err(format!(
                        "{location}: MiniMax OAuth requires its regional Provider and MiniMax Messages adapter"
                    ));
                }
            }
            let copilot_auth = provider.auth == ModelAuthSource::GithubCopilot;
            if copilot_auth != (provider.id == super::copilot::COPILOT_PROVIDER_ID)
                || copilot_auth
                    && (!provider.api_key_env.is_empty()
                        || provider.models.iter().any(|model| {
                            !matches!(model.adapter, Adapter::Messages | Adapter::Responses)
                        }))
            {
                return Err(format!(
                    "{location}: GitHub Copilot sign-in requires the github-copilot Provider with Messages or Responses models"
                ));
            }
            if let Some(connection) = &provider.connection {
                connection
                    .validate()
                    .map_err(|error| format!("{location}: {error}"))?;
                match connection.discovery {
                    super::connection::ModelDiscovery::OpenAiApi
                        if provider.auth != ModelAuthSource::ApiKeyEnv =>
                    {
                        return Err(format!(
                            "{location}: OpenAI API discovery requires API key auth"
                        ));
                    }
                    super::connection::ModelDiscovery::Codex
                        if provider.auth != ModelAuthSource::CodexSubscription =>
                    {
                        return Err(format!(
                            "{location}: Codex discovery requires subscription auth"
                        ));
                    }
                    super::connection::ModelDiscovery::GithubCopilot if !copilot_auth => {
                        return Err(format!(
                            "{location}: GitHub Copilot discovery requires Copilot sign-in"
                        ));
                    }
                    _ => {}
                }
            }
            if provider.models.is_empty() {
                return Err(format!("{location}.models: at least one model is required"));
            }
            let mut env_chars = provider.api_key_env.chars();
            let credentialless_loopback = provider.api_key_env.is_empty()
                && provider.models.iter().all(|model| {
                    provider
                        .endpoints
                        .get(&model.adapter)
                        .and_then(|endpoint| reqwest::Url::parse(endpoint).ok())
                        .is_some_and(|url| crate::network::is_loopback(&url))
                });
            if provider.auth == ModelAuthSource::ApiKeyEnv
                && !credentialless_loopback
                && (!env_chars
                    .next()
                    .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
                    || !env_chars
                        .all(|character| character.is_ascii_alphanumeric() || character == '_'))
            {
                return Err(format!(
                    "{location}.api_key_env: a valid environment variable name is required"
                ));
            }
            for (alias, target) in &provider.model_aliases {
                if alias.trim().is_empty()
                    || target.trim().is_empty()
                    || provider.models.iter().any(|model| model.id == *alias)
                    || !provider.models.iter().any(|model| model.id == *target)
                {
                    return Err(format!(
                        "{location}.model_aliases: aliases must point directly to concrete cards without shadowing card ids"
                    ));
                }
            }
            let mut model_ids = HashSet::new();
            for model in &provider.models {
                let location = format!("{location}.models[{}]", model.id);
                if (provider.auth == ModelAuthSource::CodexSubscription)
                    != (model.adapter == Adapter::CodexResponses)
                {
                    return Err(format!(
                        "{location}.adapter: Codex Responses requires Codex subscription auth"
                    ));
                }
                if model.id.trim().is_empty()
                    || model.service_model_id.trim().is_empty()
                    || model.display_name.trim().is_empty()
                {
                    return Err(format!(
                        "{location}: id, service_model_id and display_name are required"
                    ));
                }
                if !model_ids.insert(&model.id) {
                    return Err(format!("{location}: duplicate model id"));
                }
                if model.max_output_tokens == 0 {
                    return Err(format!("{location}.max_output_tokens: must be positive"));
                }
                if model.context_window_tokens == Some(0) {
                    return Err(format!(
                        "{location}.context_window_tokens: must be positive"
                    ));
                }
                if model.thinking == ThinkingControl::Unsupported
                    && (!model.effort_levels.is_empty() || model.adaptive_by_default)
                {
                    return Err(format!(
                        "{location}: unsupported thinking cannot declare effort or adaptive defaults"
                    ));
                }
                if model.adaptive_by_default
                    && !matches!(model.adapter, Adapter::Messages | Adapter::MinimaxMessages)
                {
                    return Err(format!(
                        "{location}.adaptive_by_default: only Messages supports adaptive thinking"
                    ));
                }
                if let Some(budgets) = model.thinking_budgets
                    && (model.adapter != Adapter::Messages
                        || model.thinking == ThinkingControl::Unsupported
                        || model.adaptive_by_default
                        || model.effort_levels.is_empty()
                        || model
                            .effort_levels
                            .iter()
                            .any(|effort| budgets.for_effort(*effort).is_none())
                        || [budgets.low, budgets.medium, budgets.high]
                            .into_iter()
                            .any(|budget| budget < MIN_THINKING_BUDGET_TOKENS)
                        || model.max_output_tokens < MIN_THINKING_BUDGET_TOKENS + MIN_ANSWER_TOKENS)
                {
                    return Err(format!(
                        "{location}.thinking_budgets: needs a Messages model with controllable, non-adaptive thinking whose effort levels are low/medium/high, budgets of at least {MIN_THINKING_BUDGET_TOKENS} tokens and room for an answer"
                    ));
                }
                if model
                    .effort_levels
                    .iter()
                    .any(|effort| matches!(effort, ReasoningEffort::Off | ReasoningEffort::On))
                {
                    return Err(format!(
                        "{location}.effort_levels: off is a thinking preference, not an effort level"
                    ));
                }
                let Some(base_url) = provider.endpoints.get(&model.adapter) else {
                    return Err(format!("{location}.adapter: missing provider endpoint"));
                };
                if model.adapter == Adapter::CodexResponses
                    && base_url != super::CODEX_RESPONSES_URL
                {
                    return Err(format!(
                        "{location}.adapter: Codex Responses requires the fixed endpoint"
                    ));
                }
                if copilot_auth && !super::copilot::is_allowed_endpoint(base_url) {
                    return Err(format!(
                        "{location}.adapter: GitHub Copilot sign-in requires the GitHub Copilot endpoint"
                    ));
                }
                let profile = self.profile(provider, model, base_url);
                validate_model_config(&profile).map_err(|error| format!("{location}: {error}"))?;
            }
        }
        Ok(())
    }

    fn profile(
        &self,
        provider: &ProviderDefinition,
        model: &ModelCard,
        base_url: &str,
    ) -> ModelConfig {
        ModelConfig {
            connection: provider
                .connection
                .clone()
                .or_else(|| provider_connection(&provider.id).cloned()),
            name: model.display_name.clone(),
            protocol: model.adapter.protocol(),
            base_url: base_url.into(),
            model: model.service_model_id.clone(),
            api_key_env: provider.api_key_env.clone(),
            max_output_tokens: model.max_output_tokens,
            provider: if provider.auth == ModelAuthSource::CodexSubscription {
                ModelProvider::OpenAiCodex
            } else {
                ModelProvider::Api
            },
            auth_source: provider.auth,
            provider_id: provider.id.clone(),
            provider_name: provider.display_name.clone(),
            model_id: model.id.clone(),
            input_modalities: model.input_modalities.clone(),
            card: Some(model.clone()),
        }
    }

    pub fn profiles(&self) -> Vec<ModelConfig> {
        self.providers
            .iter()
            .flat_map(|provider| {
                provider.models.iter().filter_map(|model| {
                    provider
                        .endpoints
                        .get(&model.adapter)
                        .map(|endpoint| self.profile(provider, model, endpoint))
                })
            })
            .collect()
    }

    pub fn with_providers(mut self, providers: Vec<ProviderPatch>) -> Result<Self, String> {
        let mut provider_ids = HashSet::new();
        for patch in &providers {
            if !valid_provider_id(&patch.id) {
                return Err(format!(
                    "providers[{}]: id must be a safe lowercase slug",
                    patch.id
                ));
            }
            if !provider_ids.insert(&patch.id) {
                return Err(format!("providers[{}]: duplicate provider id", patch.id));
            }
            let mut model_ids = HashSet::new();
            for model in patch.models.iter().flatten() {
                if !model_ids.insert(&model.id) {
                    return Err(format!(
                        "providers[{}].models[{}]: duplicate model id",
                        patch.id, model.id
                    ));
                }
            }
        }
        let default_models: Vec<_> = providers
            .iter()
            .filter_map(|patch| {
                patch
                    .default_model
                    .as_ref()
                    .map(|id| (patch.id.clone(), id.clone()))
            })
            .collect();
        for patch in providers {
            let location = format!("providers[{}]", patch.id);
            if let Some(existing) = self.providers.iter_mut().find(|entry| entry.id == patch.id) {
                if let Some(aliases) = patch.model_aliases {
                    existing.model_aliases.extend(aliases);
                }
                if let Some(value) = patch.connection {
                    existing.connection = Some(value);
                }
                if let Some(value) = patch.display_name {
                    existing.display_name = value;
                }
                if let Some(value) = patch.auth {
                    existing.auth = value;
                }
                if let Some(value) = patch.api_key_env {
                    existing.api_key_env = value;
                }
                if let Some(endpoints) = patch.endpoints {
                    existing.endpoints.extend(endpoints);
                }
                if let Some(models) = patch.models {
                    let mut effective_ids = HashSet::new();
                    for mut model in models {
                        if let Some(canonical) = existing.model_aliases.get(&model.id) {
                            model.id = canonical.clone();
                        }
                        if !effective_ids.insert(model.id.clone()) {
                            return Err(format!(
                                "{location}.models: duplicate model id or alias target `{}`",
                                model.id
                            ));
                        }
                        if let Some(current) = existing
                            .models
                            .iter_mut()
                            .find(|entry| entry.id == model.id)
                        {
                            model.apply_to(current);
                        } else {
                            existing.models.push(model.into_new(&location)?);
                        }
                    }
                }
            } else {
                let models = patch
                    .models
                    .ok_or_else(|| format!("{location}.models: required for new provider"))?
                    .into_iter()
                    .map(|model| model.into_new(&location))
                    .collect::<Result<Vec<_>, _>>()?;
                self.providers.push(ProviderDefinition {
                    model_aliases: patch.model_aliases.unwrap_or_default(),
                    connection: patch.connection,
                    id: patch.id,
                    display_name: patch.display_name.ok_or_else(|| {
                        format!("{location}.display_name: required for new provider")
                    })?,
                    auth: patch
                        .auth
                        .ok_or_else(|| format!("{location}.auth: required for new provider"))?,
                    api_key_env: patch.api_key_env.unwrap_or_default(),
                    endpoints: patch.endpoints.ok_or_else(|| {
                        format!("{location}.endpoints: required for new provider")
                    })?,
                    models,
                });
            }
        }
        self.validate()?;
        for (provider_id, model_id) in default_models {
            if self.get(&provider_id, &model_id).is_none() {
                return Err(format!(
                    "providers[{provider_id}].default_model: unknown model `{model_id}`"
                ));
            }
        }
        Ok(self)
    }

    pub fn canonical_model_id(&self, provider_id: &str, model_id: &str) -> Option<&str> {
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)?;
        let target = provider
            .model_aliases
            .get(model_id)
            .map_or(model_id, String::as_str);
        provider
            .models
            .iter()
            .find(|model| model.id == target)
            .map(|model| model.id.as_str())
    }

    pub fn get(&self, provider_id: &str, model_id: &str) -> Option<ModelConfig> {
        let model_id = self.canonical_model_id(provider_id, model_id)?;
        self.profiles()
            .into_iter()
            .find(|profile| profile.provider_id == provider_id && profile.model_id == model_id)
    }
}

pub fn valid_provider_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

impl ModelPatch {
    fn apply_to(self, model: &mut ModelCard) {
        if let Some(value) = self.service_model_id {
            model.service_model_id = value;
        }
        if let Some(value) = self.display_name {
            model.display_name = value;
        }
        if let Some(value) = self.adapter {
            model.adapter = value;
        }
        if let Some(value) = self.max_output_tokens {
            model.max_output_tokens = value;
        }
        if let Some(value) = self.context_window_tokens {
            model.context_window_tokens = value;
        }
        if let Some(value) = self.thinking {
            model.thinking = value;
        }
        if let Some(value) = self.adaptive_by_default {
            model.adaptive_by_default = value;
        }
        if let Some(value) = self.effort_levels {
            model.effort_levels = value;
        }
        if let Some(value) = self.input_modalities {
            model.input_modalities = value;
        }
        if let Some(value) = self.thinking_budgets {
            model.thinking_budgets = Some(value);
        }
    }

    fn into_new(self, provider_location: &str) -> Result<ModelCard, String> {
        let location = format!("{provider_location}.models[{}]", self.id);
        Ok(ModelCard {
            id: self.id,
            service_model_id: self
                .service_model_id
                .ok_or_else(|| format!("{location}.service_model_id: required for new model"))?,
            display_name: self
                .display_name
                .ok_or_else(|| format!("{location}.display_name: required for new model"))?,
            adapter: self
                .adapter
                .ok_or_else(|| format!("{location}.adapter: required for new model"))?,
            max_output_tokens: self
                .max_output_tokens
                .unwrap_or(default_max_output_tokens()),
            context_window_tokens: self.context_window_tokens.unwrap_or(None),
            thinking: self.thinking.unwrap_or_default(),
            adaptive_by_default: self.adaptive_by_default.unwrap_or(false),
            effort_levels: self.effort_levels.unwrap_or_default(),
            input_modalities: self.input_modalities.unwrap_or_else(text_only),
            thinking_budgets: self.thinking_budgets,
        })
    }
}

pub fn model_card(config: &ModelConfig) -> Option<&ModelCard> {
    config.card.as_ref()
}

pub fn built_in_model_profiles() -> Vec<ModelConfig> {
    static PROFILES: OnceLock<Vec<ModelConfig>> = OnceLock::new();
    PROFILES
        .get_or_init(|| {
            let catalog = ModelCatalog::defaults();
            catalog
                .validate()
                .expect("bundled model catalog passes validation");
            catalog
                .profiles()
                .into_iter()
                .filter(is_default_visible)
                .collect()
        })
        .clone()
}

/// The connection is the sole authority for vendor/product organization.
pub fn provider_connection(
    provider_id: &str,
) -> Option<&'static super::connection::ProviderConnection> {
    static DEFAULTS: OnceLock<ModelCatalog> = OnceLock::new();
    DEFAULTS
        .get_or_init(ModelCatalog::defaults)
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)?
        .connection
        .as_ref()
}

pub fn builtin_canonical_model_id(provider_id: &str, model_id: &str) -> Option<&'static str> {
    static DEFAULTS: OnceLock<ModelCatalog> = OnceLock::new();
    DEFAULTS
        .get_or_init(ModelCatalog::defaults)
        .canonical_model_id(provider_id, model_id)
}

fn canonicalize_builtin_identity(config: &mut ModelConfig) {
    let Some(canonical) = builtin_canonical_model_id(&config.provider_id, &config.model_id) else {
        return;
    };
    config.model_id = canonical.to_owned();
    if let Some(card) = &mut config.card {
        card.id = canonical.to_owned();
    }
}

pub fn provider_group(provider_id: &str) -> Option<&'static str> {
    provider_connection(provider_id).map(|connection| connection.vendor.as_str())
}

pub fn is_default_visible(profile: &ModelConfig) -> bool {
    super::access::AccessCatalog::defaults()
        .series_for(profile)
        .is_some()
}

fn legacy_builtin_name(profile: &ModelConfig) -> Option<String> {
    let name = match (profile.provider_id.as_str(), profile.model.as_str()) {
        ("deepseek-api", "deepseek-flash") => "DeepSeek",
        ("deepseek-api", "deepseek-v4-pro") => "DeepSeek V4 Pro",
        ("deepseek-api", "deepseek-v4-flash") => "DeepSeek V4 Flash",
        ("openai-codex", "gpt-5.5") => "Codex subscription GPT-5.5",
        ("openai-codex", "gpt-5.6-sol") => "Codex subscription GPT-5.6 Sol",
        ("openai-codex", "gpt-5.6-terra") => "Codex subscription GPT-5.6 Terra",
        ("openai-codex", "gpt-5.6-luna") => "Codex subscription GPT-5.6 Luna",
        _ => return None,
    };
    let suffix = if profile.provider_id == "deepseek-api" {
        match profile.protocol {
            Protocol::Messages => "",
            Protocol::Responses => " Responses",
            Protocol::ChatCompletions => " Chat Completions",
            Protocol::Google => " Google API",
            Protocol::SystemOne => " System One",
        }
    } else {
        ""
    };
    Some(format!("{name}{suffix}"))
}

pub fn migrate_legacy_model(mut config: ModelConfig) -> ModelConfig {
    canonicalize_builtin_identity(&mut config);
    if config.card.is_some() {
        return config;
    }
    let defaults = ModelCatalog::defaults();
    if let Some(default) = defaults.profiles().into_iter().find(|default| {
        (default.name == config.name
            || legacy_builtin_name(default).as_deref() == Some(config.name.as_str()))
            && default.protocol == config.protocol
            && default.base_url == config.base_url
            && default.model == config.model
            && default.api_key_env == config.api_key_env
            && default.provider == config.provider
            && default.auth_source == config.auth_source
            && default.max_output_tokens == config.max_output_tokens
            && (config.provider_id.is_empty() || config.provider_id == default.provider_id)
            && (config.provider_name.is_empty() || config.provider_name == default.provider_name)
            && (config.model_id.is_empty() || config.model_id == default.model_id)
    }) {
        config.name = default.name;
        config.provider_id = default.provider_id;
        config.provider_name = default.provider_name;
        config.model_id = default.model_id;
        config.input_modalities = default.input_modalities;
        config.card = default.card;
        return config;
    }
    let known_source = match (
        config.provider,
        config.auth_source,
        config.protocol,
        config.base_url.trim_end_matches('/'),
    ) {
        (
            ModelProvider::Api,
            ModelAuthSource::ApiKeyEnv,
            Protocol::Messages,
            "https://api.deepseek.com/anthropic",
        ) => Some((
            Adapter::DeepseekMessages,
            "deepseek-api",
            "legacy-deepseek-api",
            "Legacy DeepSeek API",
        )),
        (
            ModelProvider::Api,
            ModelAuthSource::ApiKeyEnv,
            Protocol::Responses,
            "https://api.deepseek.com",
        ) => Some((
            Adapter::Responses,
            "deepseek-api",
            "legacy-deepseek-api",
            "Legacy DeepSeek API",
        )),
        (
            ModelProvider::Api,
            ModelAuthSource::ApiKeyEnv,
            Protocol::ChatCompletions,
            "https://api.deepseek.com",
        ) => Some((
            Adapter::DeepseekChatCompletions,
            "deepseek-api",
            "legacy-deepseek-api",
            "Legacy DeepSeek API",
        )),
        (
            ModelProvider::OpenAiCodex,
            ModelAuthSource::CodexSubscription,
            Protocol::Responses,
            endpoint,
        ) if endpoint == super::CODEX_RESPONSES_URL && config.api_key_env.is_empty() => Some((
            Adapter::CodexResponses,
            "openai-codex",
            "legacy-openai-codex",
            "Legacy Codex subscription",
        )),
        _ => None,
    };
    if let Some((adapter, default_provider_id, legacy_provider_id, legacy_provider_name)) =
        known_source
    {
        if config.provider_id.is_empty() {
            config.provider_id = legacy_provider_id.into();
        }
        if config.provider_name.is_empty() {
            config.provider_name = legacy_provider_name.into();
        }
        if config.model_id.is_empty() {
            config.model_id = config.model.clone();
        }
        let mut card = defaults
            .providers
            .iter()
            .find(|provider| provider.id == default_provider_id)
            .and_then(|provider| {
                provider.models.iter().find(|model| {
                    model.adapter == adapter && model.service_model_id == config.model
                })
            })
            .cloned()
            .unwrap_or(ModelCard {
                id: config.model_id.clone(),
                service_model_id: config.model.clone(),
                display_name: config.name.clone(),
                adapter,
                max_output_tokens: config.max_output_tokens,
                context_window_tokens: None,
                thinking: ThinkingControl::Unsupported,
                adaptive_by_default: false,
                effort_levels: Vec::new(),
                input_modalities: config.input_modalities.clone(),
                thinking_budgets: None,
            });
        card.id = config.model_id.clone();
        card.display_name = config.name.clone();
        card.max_output_tokens = config.max_output_tokens;
        card.input_modalities = config.input_modalities.clone();
        config.card = Some(card);
    }
    config
}

pub fn model_for_persistent_selection(config: ModelConfig) -> ModelConfig {
    let mut config = migrate_legacy_model(config);
    if config.auth_source == ModelAuthSource::CodexSubscription
        && config.provider_id == "legacy-openai-codex"
    {
        config.provider_id = "openai-codex".into();
        config.provider_name = ModelCatalog::defaults()
            .providers
            .into_iter()
            .find(|provider| provider.id == "openai-codex")
            .expect("bundled Codex Provider exists")
            .display_name;
    }
    if config.card.is_some() || config.provider != ModelProvider::Api {
        return config;
    }
    if config.provider_id.is_empty() {
        let source = format!(
            "{}\n{}\n{:?}\n{:?}",
            config.base_url, config.api_key_env, config.protocol, config.auth_source
        );
        let digest = Sha256::digest(source.as_bytes());
        let suffix = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        config.provider_id = format!("legacy-api-{suffix}");
    }
    if config.provider_name.is_empty() {
        config.provider_name = "Custom API".into();
    }
    if config.model_id.is_empty() {
        config.model_id = config.model.clone();
    }
    config.card = Some(ModelCard {
        id: config.model_id.clone(),
        service_model_id: config.model.clone(),
        display_name: config.name.clone(),
        adapter: config.protocol.into(),
        max_output_tokens: config.max_output_tokens,
        context_window_tokens: None,
        thinking: ThinkingControl::Unsupported,
        adaptive_by_default: false,
        effort_levels: Vec::new(),
        input_modalities: config.input_modalities.clone(),
        thinking_budgets: None,
    });
    config
}

pub fn upgrade_saved_legacy_builtin_model(config: ModelConfig) -> ModelConfig {
    if config.card.is_some() {
        return config;
    }
    if config.max_output_tokens == 4096 {
        let mut matching = config.clone();
        matching.max_output_tokens = 64_000;
        let upgraded = migrate_legacy_model(matching);
        if upgraded.card.is_some() && upgraded.provider_id == "deepseek-api" {
            return upgraded;
        }
    }
    migrate_legacy_model(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_builtin_names_upgrade_to_current_model_cards() {
        let defaults = ModelCatalog::defaults();
        for (provider_id, model_id) in [
            ("deepseek-api", "deepseek-flash-messages"),
            ("deepseek-api", "deepseek-flash-responses"),
            ("deepseek-api", "deepseek-flash-chat-completions"),
            ("deepseek-api", "deepseek-v4-pro-messages"),
            ("deepseek-api", "deepseek-v4-pro-responses"),
            ("deepseek-api", "deepseek-v4-pro-chat-completions"),
            ("openai-codex", "gpt-5.5-responses"),
            ("openai-codex", "gpt-5.6-sol-responses"),
            ("openai-codex", "gpt-5.6-terra-responses"),
            ("openai-codex", "gpt-5.6-luna-responses"),
        ] {
            let current = defaults.get(provider_id, model_id).unwrap();
            let mut legacy = current.clone();
            legacy.name = legacy_builtin_name(&current).unwrap();
            legacy.provider_id.clear();
            legacy.provider_name.clear();
            legacy.model_id.clear();
            legacy.card = None;
            let migrated = migrate_legacy_model(legacy);
            assert_eq!(migrated, current, "{provider_id}/{model_id}");
            assert_eq!(migrate_legacy_model(migrated.clone()), migrated);
        }
    }

    #[test]
    fn historical_builtin_name_does_not_override_custom_fields() {
        let current = ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash-messages")
            .unwrap();
        let mut legacy = current.clone();
        legacy.name = legacy_builtin_name(&current).unwrap();
        legacy.provider_id.clear();
        legacy.provider_name.clear();
        legacy.model_id.clear();
        legacy.card = None;

        let mut custom_name = legacy.clone();
        custom_name.name = "Research DeepSeek".into();
        let migrated = migrate_legacy_model(custom_name.clone());
        assert_eq!(migrated.name, custom_name.name);
        assert_ne!(migrated.provider_id, current.provider_id);

        let mut custom_endpoint = legacy.clone();
        custom_endpoint.base_url = "https://gateway.example/anthropic".into();
        let migrated = migrate_legacy_model(custom_endpoint.clone());
        assert_eq!(migrated.base_url, custom_endpoint.base_url);
        assert_ne!(migrated.provider_id, current.provider_id);

        let mut custom_limit = legacy.clone();
        custom_limit.max_output_tokens = 8192;
        let migrated = migrate_legacy_model(custom_limit.clone());
        assert_eq!(migrated.max_output_tokens, custom_limit.max_output_tokens);
        assert_eq!(migrated.name, custom_limit.name);
        assert_ne!(migrated.provider_id, current.provider_id);

        let mut custom_identity = legacy.clone();
        custom_identity.provider_id = "research-gateway".into();
        let migrated = migrate_legacy_model(custom_identity);
        assert_eq!(migrated.provider_id, "research-gateway");
        assert_eq!(migrated.name, "DeepSeek");

        legacy.max_output_tokens = 4096;
        assert_eq!(upgrade_saved_legacy_builtin_model(legacy), current);
    }
}
