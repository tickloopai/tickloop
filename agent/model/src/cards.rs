use std::{
    collections::{BTreeMap, HashSet},
    sync::OnceLock,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::validate_model_config;
use tickloop_agent_protocol::{
    Adapter, LaunchSpec, ModelAuthSource, ModelCard, ModelConfig, ModelDiscovery, ModelProvider,
    Protocol, ProviderConnection, ProviderPatch, ReasoningEffort, ThinkingControl,
};

/// Anthropic's minimum `budget_tokens`.
pub const MIN_THINKING_BUDGET_TOKENS: u32 = 1024;
/// Output kept for the answer when a thinking budget shares `max_tokens`, as pi does.
pub const MIN_ANSWER_TOKENS: u32 = 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<super::access::ProviderSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ProviderConnection>,
    pub id: String,
    pub display_name: String,
    pub auth: ModelAuthSource,
    #[serde(default)]
    pub api_key_env: String,
    pub endpoints: BTreeMap<Adapter, String>,
    pub models: Vec<ModelCard>,
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
        static BUNDLED: OnceLock<ModelCatalog> = OnceLock::new();
        BUNDLED
            .get_or_init(|| {
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
            })
            .clone()
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
                    ModelDiscovery::OpenAiApi if provider.auth != ModelAuthSource::ApiKeyEnv => {
                        return Err(format!(
                            "{location}: OpenAI API discovery requires API key auth"
                        ));
                    }
                    ModelDiscovery::Codex
                        if provider.auth != ModelAuthSource::CodexSubscription =>
                    {
                        return Err(format!(
                            "{location}: Codex discovery requires subscription auth"
                        ));
                    }
                    ModelDiscovery::GithubCopilot if !copilot_auth => {
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
                        .is_some_and(|url| tickloop_agent_base::network::is_loopback(&url))
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
                if model.id.trim().is_empty() || model.display_name.trim().is_empty() {
                    return Err(format!("{location}: id and display_name are required"));
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
                if model.thinking_off.flatten().is_some()
                    && (model.adapter.protocol() != Protocol::Messages
                        || model.thinking != ThinkingControl::Toggle
                        || model.thinking_budgets.is_some())
                {
                    return Err(format!(
                        "{location}.thinking_off: requires a controllable Messages model without manual thinking budgets"
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
        super::access::AccessCatalog::from_catalog(self).validate(self)
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
            model: model.id.clone(),
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
                    for model in models {
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
                    selection: None,
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

    pub fn get(&self, provider_id: &str, model_id: &str) -> Option<ModelConfig> {
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
pub fn provider_connection(provider_id: &str) -> Option<&'static ProviderConnection> {
    static DEFAULTS: OnceLock<ModelCatalog> = OnceLock::new();
    DEFAULTS
        .get_or_init(ModelCatalog::defaults)
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)?
        .connection
        .as_ref()
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
                provider
                    .models
                    .iter()
                    .find(|model| model.adapter == adapter && model.id == config.model)
            })
            .cloned()
            .unwrap_or(ModelCard {
                id: config.model_id.clone(),
                display_name: config.name.clone(),
                adapter,
                max_output_tokens: config.max_output_tokens,
                context_window_tokens: None,
                thinking: ThinkingControl::Unsupported,
                adaptive_by_default: false,
                effort_levels: Vec::new(),
                input_modalities: config.input_modalities.clone(),
                thinking_budgets: None,
                temperature_control: None,
                thinking_off: None,
            });
        card.id = config.model_id.clone();
        card.display_name = config.name.clone();
        card.max_output_tokens = config.max_output_tokens;
        card.input_modalities = config.input_modalities.clone();
        config.card = Some(card);
    }
    config
}

/// The model a worker for `launch` starts with: the launch's catalog model, or the bundled
/// default when it names none or one the catalog no longer has.
pub fn launch_model(launch: &LaunchSpec) -> ModelConfig {
    let catalog = ModelCatalog::defaults();
    launch
        .model
        .as_ref()
        .and_then(|model| catalog.get(&model.provider_id, &model.model_id))
        .unwrap_or_else(|| {
            catalog
                .get("deepseek-api", "deepseek-flash")
                .expect("bundled default model")
        })
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
    if let Some(card) = config.card.as_mut() {
        if let Some(current) = ModelCatalog::defaults().get(&config.provider_id, &config.model_id)
            && current.model == config.model
            && let Some(current_card) = current.card
            && current_card.adapter == card.adapter
        {
            card.temperature_control = card
                .temperature_control
                .or(current_card.temperature_control);
            if card.thinking == current_card.thinking
                && card.thinking_budgets == current_card.thinking_budgets
            {
                card.thinking_off = card.thinking_off.or(current_card.thinking_off);
            }
        }
        return config;
    }
    if config.provider != ModelProvider::Api {
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
    config.model_id = config.model.clone();
    config.card = Some(ModelCard {
        id: config.model_id.clone(),
        display_name: config.name.clone(),
        adapter: config.protocol.into(),
        max_output_tokens: config.max_output_tokens,
        context_window_tokens: None,
        thinking: ThinkingControl::Unsupported,
        adaptive_by_default: false,
        effort_levels: Vec::new(),
        input_modalities: config.input_modalities.clone(),
        thinking_budgets: None,
        temperature_control: None,
        thinking_off: None,
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
    fn default_catalog_copies_keep_nested_changes_local_to_each_caller() {
        let original = ModelCatalog::defaults();
        let mut edited = ModelCatalog::defaults();
        let mut removed = ModelCatalog::defaults();
        edited.providers[0].display_name = "Local display override".into();
        edited.providers[0].endpoints.clear();
        edited.providers[0].models[0].display_name = "Local model override".into();
        edited.providers[0].models[0].input_modalities.clear();
        removed.providers.clear();

        assert_ne!(edited, original);
        assert!(removed.providers.is_empty());
        assert_eq!(ModelCatalog::defaults(), original);
    }

    #[test]
    #[ignore = "manual construction-cost measurement; no machine-dependent pass threshold"]
    fn measure_repeated_default_catalog_construction() {
        use std::{hint::black_box, time::Instant};
        const ITERATIONS: usize = 129;
        let reference = ModelCatalog::defaults();
        assert!(!reference.providers.is_empty());
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(ModelCatalog::defaults());
        }
        let defaults = started.elapsed();
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(reference.clone());
        }
        let cloning = started.elapsed();
        eprintln!(
            "catalog construction: iterations={ITERATIONS}, defaults_us={}, clone_us={}, defaults_to_clone_ratio={:.2}",
            defaults.as_micros(),
            cloning.as_micros(),
            defaults.as_secs_f64() / cloning.as_secs_f64()
        );
    }

    #[test]
    fn saved_builtin_cards_inherit_new_capabilities_without_replacing_explicit_overrides() {
        let mut saved = ModelCatalog::defaults()
            .get("anthropic-api", "claude-sonnet-5-5")
            .unwrap();
        saved.card.as_mut().unwrap().temperature_control = None;
        saved.card.as_mut().unwrap().thinking_off = None;
        let upgraded = model_for_persistent_selection(saved.clone());
        assert_eq!(
            upgraded.card.as_ref().unwrap().temperature_control,
            Some(Some(
                tickloop_agent_protocol::TemperatureControl::Unsupported
            ))
        );
        assert_eq!(
            upgraded.card.as_ref().unwrap().thinking_off,
            Some(Some(
                tickloop_agent_protocol::ThinkingOffEncoding::BetweenTools
            ))
        );
        saved.card.as_mut().unwrap().temperature_control =
            Some(Some(tickloop_agent_protocol::TemperatureControl::Supported));
        saved.card.as_mut().unwrap().thinking_off =
            Some(Some(tickloop_agent_protocol::ThinkingOffEncoding::Disabled));
        let overridden = model_for_persistent_selection(saved);
        assert_eq!(
            overridden.card.as_ref().unwrap().temperature_control,
            Some(Some(tickloop_agent_protocol::TemperatureControl::Supported))
        );
        assert_eq!(
            overridden.card.as_ref().unwrap().thinking_off,
            Some(Some(tickloop_agent_protocol::ThinkingOffEncoding::Disabled))
        );
    }

    #[test]
    fn model_card_capability_patches_can_override_and_clear_protocol_defaults() {
        let mut config = ModelCatalog::defaults()
            .get("anthropic-api", "claude-sonnet-5-5")
            .unwrap();
        let card = config.card.as_mut().unwrap();
        assert_eq!(
            card.temperature_control,
            Some(Some(
                tickloop_agent_protocol::TemperatureControl::Unsupported
            ))
        );
        assert_eq!(
            card.thinking_off,
            Some(Some(
                tickloop_agent_protocol::ThinkingOffEncoding::BetweenTools
            ))
        );
        let patch: tickloop_agent_protocol::ModelPatch =
            serde_json::from_value(serde_json::json!({
                "id":card.id,"temperature_control":null,"thinking_off":null
            }))
            .unwrap();
        patch.apply_to(card);
        assert_eq!(card.temperature_control, Some(None));
        assert_eq!(card.thinking_off, Some(None));
        let patch: tickloop_agent_protocol::ModelPatch =
            serde_json::from_value(serde_json::json!({
                "id":card.id,"temperature_control":"supported","thinking_off":"disabled"
            }))
            .unwrap();
        patch.apply_to(card);
        assert_eq!(
            card.temperature_control,
            Some(Some(tickloop_agent_protocol::TemperatureControl::Supported))
        );
        assert_eq!(
            card.thinking_off,
            Some(Some(tickloop_agent_protocol::ThinkingOffEncoding::Disabled))
        );
    }

    #[test]
    fn historical_builtin_names_upgrade_to_current_model_cards() {
        let defaults = ModelCatalog::defaults();
        for (provider_id, model_id) in [
            ("deepseek-api", "deepseek-flash"),
            ("deepseek-api", "deepseek-v4-pro"),
            ("openai-codex", "gpt-5.5"),
            ("openai-codex", "gpt-5.6-sol"),
            ("openai-codex", "gpt-5.6-terra"),
            ("openai-codex", "gpt-5.6-luna"),
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
            .get("deepseek-api", "deepseek-flash")
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
        assert_eq!(migrated.name, legacy.name);

        legacy.max_output_tokens = 4096;
        assert_eq!(upgrade_saved_legacy_builtin_model(legacy), current);
    }
}
