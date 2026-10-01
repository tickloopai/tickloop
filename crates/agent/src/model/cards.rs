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
    Responses,
    ChatCompletions,
    DeepseekChatCompletions,
    CodexResponses,
    SystemOne,
}

impl Adapter {
    pub const fn protocol(self) -> Protocol {
        match self {
            Self::SystemOne => Protocol::SystemOne,
            Self::Messages | Self::DeepseekMessages => Protocol::Messages,
            Self::Responses | Self::CodexResponses => Protocol::Responses,
            Self::ChatCompletions | Self::DeepseekChatCompletions => Protocol::ChatCompletions,
        }
    }
}

impl From<Protocol> for Adapter {
    fn from(protocol: Protocol) -> Self {
        match protocol {
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
}

const fn default_max_output_tokens() -> u32 {
    4096
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDefinition {
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
}

impl ProviderPatch {
    pub fn merge_from(&mut self, next: Self) {
        if next.default_model.is_some() {
            self.default_model = next.default_model;
        }
        if next.display_name.is_some() {
            self.display_name = next.display_name;
        }
        if next.auth.is_some() {
            self.auth = next.auth;
        }
        if next.api_key_env.is_some() {
            self.api_key_env = next.api_key_env;
        }
        if let Some(endpoints) = next.endpoints {
            self.endpoints
                .get_or_insert_with(BTreeMap::new)
                .extend(endpoints);
        }
        if let Some(models) = next.models {
            let existing = self.models.get_or_insert_with(Vec::new);
            for model in models {
                if let Some(current) = existing.iter_mut().find(|entry| entry.id == model.id) {
                    current.merge_from(model);
                } else {
                    existing.push(model);
                }
            }
        }
    }
}

impl ModelPatch {
    fn merge_from(&mut self, next: Self) {
        if next.service_model_id.is_some() {
            self.service_model_id = next.service_model_id;
        }
        if next.display_name.is_some() {
            self.display_name = next.display_name;
        }
        if next.adapter.is_some() {
            self.adapter = next.adapter;
        }
        if next.max_output_tokens.is_some() {
            self.max_output_tokens = next.max_output_tokens;
        }
        if next.context_window_tokens.is_some() {
            self.context_window_tokens = next.context_window_tokens;
        }
        if next.thinking.is_some() {
            self.thinking = next.thinking;
        }
        if next.adaptive_by_default.is_some() {
            self.adaptive_by_default = next.adaptive_by_default;
        }
        if next.effort_levels.is_some() {
            self.effort_levels = next.effort_levels;
        }
        if next.input_modalities.is_some() {
            self.input_modalities = next.input_modalities;
        }
    }
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelCatalogFile {
    pub providers: Vec<ProviderDefinition>,
    pub default_model: ModelSelection,
}

impl ModelCatalogFile {
    pub fn selected_model(self) -> Result<ModelConfig, String> {
        let mut providers = self.providers;
        for provider in &mut providers {
            migrate_legacy_provider_definition(provider);
        }
        let catalog = ModelCatalog::from_providers(providers)?;
        catalog
            .get(&self.default_model.provider, &self.default_model.model)
            .ok_or_else(|| {
                format!(
                    "defaultModel: unknown provider/model {}/{}",
                    self.default_model.provider, self.default_model.model
                )
            })
    }
}

impl ModelCatalog {
    pub fn selected_provider_yaml(contents: &str) -> Result<ModelConfig, String> {
        let provider: ProviderPatch = serde_yaml_ng::from_str(contents)
            .map_err(|error| format!("invalid provider YAML: {error}"))?;
        Self::resolved_provider(provider, Self::defaults()).map(|(selected, _)| selected)
    }

    pub fn resolved_provider_yaml(contents: &str) -> Result<(ModelConfig, ProviderPatch), String> {
        Self::resolved_provider_yaml_with_catalog(contents, Self::defaults())
    }

    pub fn resolved_provider_yaml_with_catalog(
        contents: &str,
        catalog: Self,
    ) -> Result<(ModelConfig, ProviderPatch), String> {
        let provider: ProviderPatch = serde_yaml_ng::from_str(contents)
            .map_err(|error| format!("invalid provider YAML: {error}"))?;
        Self::resolved_provider(provider, catalog)
    }

    fn resolved_provider(
        provider: ProviderPatch,
        catalog: Self,
    ) -> Result<(ModelConfig, ProviderPatch), String> {
        let selected_id = provider
            .default_model
            .clone()
            .or_else(|| {
                provider
                    .models
                    .as_ref()
                    .filter(|models| models.len() == 1)
                    .map(|models| models[0].id.clone())
            })
            .ok_or_else(|| {
                "provider YAML with multiple models requires `default_model`".to_string()
            })?;
        let provider_id = provider.id.clone();
        let catalog = catalog.with_providers(vec![provider.clone()])?;
        let selected = catalog.get(&provider_id, &selected_id).ok_or_else(|| {
            format!("default_model: unknown model `{selected_id}` in provider `{provider_id}`")
        })?;
        Ok((selected, provider))
    }

    pub fn defaults() -> Self {
        let providers = [
            include_str!("providers/deepseek-api.yaml"),
            include_str!("providers/anthropic-api.yaml"),
            include_str!("providers/openai-codex.yaml"),
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
                if model.adaptive_by_default && model.adapter != Adapter::Messages {
                    return Err(format!(
                        "{location}.adaptive_by_default: only Messages supports adaptive thinking"
                    ));
                }
                if model.effort_levels.contains(&ReasoningEffort::Off) {
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

fn legacy_adapter(provider_id: &str, adapter: Adapter) -> Adapter {
    match (provider_id, adapter) {
        ("deepseek-api", Adapter::Messages) => Adapter::DeepseekMessages,
        ("deepseek-api", Adapter::ChatCompletions) => Adapter::DeepseekChatCompletions,
        ("openai-codex", Adapter::Responses) => Adapter::CodexResponses,
        _ => adapter,
    }
}

fn migrate_legacy_endpoints(provider_id: &str, endpoints: &mut BTreeMap<Adapter, String>) {
    for adapter in [
        Adapter::Messages,
        Adapter::ChatCompletions,
        Adapter::Responses,
    ] {
        let next = legacy_adapter(provider_id, adapter);
        if next != adapter
            && let Some(url) = endpoints.remove(&adapter)
        {
            endpoints.entry(next).or_insert(url);
        }
    }
}

pub fn migrate_legacy_provider_patch(patch: &mut ProviderPatch) {
    if let Some(endpoints) = &mut patch.endpoints {
        migrate_legacy_endpoints(&patch.id, endpoints);
    }
    for model in patch.models.iter_mut().flatten() {
        if let Some(adapter) = model.adapter {
            model.adapter = Some(legacy_adapter(&patch.id, adapter));
        }
    }
}

fn migrate_legacy_provider_definition(provider: &mut ProviderDefinition) {
    migrate_legacy_endpoints(&provider.id, &mut provider.endpoints);
    for model in &mut provider.models {
        model.adapter = legacy_adapter(&provider.id, model.adapter);
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

pub fn is_default_visible(profile: &ModelConfig) -> bool {
    const VISIBLE: &[(&str, &str)] = &[
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
    ];
    VISIBLE.contains(&(profile.provider_id.as_str(), profile.model_id.as_str()))
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
