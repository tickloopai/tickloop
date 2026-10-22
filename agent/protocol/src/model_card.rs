use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    InputModality, ModelAuthSource, Protocol, ProviderConnection, ReasoningEffort, model::text_only,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TemperatureControl {
    Supported,
    Unsupported,
    WithoutThinking,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingOffEncoding {
    Disabled,
    BetweenTools,
}

impl ThinkingOffEncoding {
    pub fn wire_value(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::BetweenTools => "between_tools",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCard {
    pub id: String,
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
    /// Missing inherits catalog capabilities; explicit null selects the adapter default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable"
    )]
    pub temperature_control: Option<Option<TemperatureControl>>,
    /// Missing inherits catalog capabilities; explicit null selects the adapter default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable"
    )]
    pub thinking_off: Option<Option<ThinkingOffEncoding>>,
}

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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<ProviderConnection>,
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
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Adapter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable"
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable"
    )]
    pub temperature_control: Option<Option<TemperatureControl>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable"
    )]
    pub thinking_off: Option<Option<ThinkingOffEncoding>>,
}

pub(crate) fn deserialize_nullable<'de, D, T>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

impl ModelPatch {
    pub fn apply_to(self, model: &mut ModelCard) {
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
        if let Some(value) = self.temperature_control {
            model.temperature_control = Some(value);
        }
        if let Some(value) = self.thinking_off {
            model.thinking_off = Some(value);
        }
    }

    pub fn into_new(self, provider_location: &str) -> Result<ModelCard, String> {
        let location = format!("{provider_location}.models[{}]", self.id);
        Ok(ModelCard {
            id: self.id,
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
            temperature_control: self.temperature_control,
            thinking_off: self.thinking_off,
        })
    }
}
