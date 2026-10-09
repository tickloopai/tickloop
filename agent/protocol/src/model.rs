use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Adapter, Image, ImageMediaType, ModelCard, ProviderConnection};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Messages,
    Responses,
    ChatCompletions,
    Google,
    SystemOne,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProvider {
    #[default]
    Api,
    #[serde(rename = "openai_codex")]
    OpenAiCodex,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAuthSource {
    #[default]
    ApiKeyEnv,
    CodexSubscription,
    #[serde(rename = "minimax_oauth_cn")]
    MiniMaxOAuthCn,
    #[serde(rename = "minimax_oauth_global")]
    MiniMaxOAuthGlobal,
    GithubCopilot,
    GoogleCloud,
    #[serde(rename = "google_oauth")]
    GoogleOAuth,
    #[serde(rename = "kimi_oauth")]
    KimiOAuth,
    #[serde(rename = "xai_oauth")]
    XaiOAuth,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ModelIdentity {
    pub profile: String,
    pub protocol: Protocol,
    pub requested_model: String,
    #[serde(default)]
    pub provider: ModelProvider,
}

/// Kinds of input a model accepts, as in Codex `InputModality`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ProviderConnection>,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub max_output_tokens: u32,
    #[serde(default)]
    pub provider: ModelProvider,
    #[serde(default)]
    pub auth_source: ModelAuthSource,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<ModelCard>,
    /// Omitted in older recorded snapshots; like opencode for unknown models, such a
    /// model is treated as text only.
    #[serde(default = "text_only")]
    pub input_modalities: Vec<InputModality>,
}

pub(crate) fn text_only() -> Vec<InputModality> {
    vec![InputModality::Text]
}

impl ModelConfig {
    pub fn adapter(&self) -> Adapter {
        self.card
            .as_ref()
            .map_or_else(|| self.protocol.into(), |card| card.adapter)
    }

    pub fn accepts_images(&self) -> bool {
        self.input_modalities.contains(&InputModality::Image)
    }

    pub fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            profile: self.name.clone(),
            protocol: self.protocol,
            requested_model: self.model.clone(),
            provider: self.provider,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrivateHistorySource {
    pub provider_id: String,
    pub endpoint_fingerprint: String,
    pub adapter: Adapter,
    pub service_model_id: String,
    pub auth_source: ModelAuthSource,
    #[serde(default)]
    pub credential_reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        is_error: bool,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ProtocolData {
        protocol: Protocol,
        value: Value,
    },
    /// Anthropic-shaped image block; `width` and `height` only feed token estimates and are
    /// left out of the wire format.
    Image {
        source: ImageSource,
        width: u32,
        height: u32,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 {
        media_type: ImageMediaType,
        data: String,
    },
}

impl From<Image> for Content {
    fn from(image: Image) -> Self {
        Self::Image {
            source: ImageSource::Base64 {
                media_type: image.media_type,
                data: image.data,
            },
            width: image.width,
            height: image.height,
        }
    }
}

impl ImageSource {
    /// The inline `data:` URL form OpenAI-style protocols take for images.
    pub fn data_url(&self) -> String {
        match self {
            Self::Base64 { media_type, data } => {
                format!("data:{};base64,{data}", media_type.mime_type())
            }
        }
    }
}

/// 照 Anthropic tool_result 的 `content`（Codex `FunctionCallOutputBody`）：纯字符串或内容块数组。
/// 字符串形式与旧 Transcript 的写法相同，两种形式都能直接反序列化。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text {
        text: String,
    },
    /// Same shape as [`Content::Image`]: `width` and `height` stay off the wire.
    Image {
        source: ImageSource,
        width: u32,
        height: u32,
    },
}

impl From<Image> for ToolResultBlock {
    fn from(image: Image) -> Self {
        Self::Image {
            source: ImageSource::Base64 {
                media_type: image.media_type,
                data: image.data,
            },
            width: image.width,
            height: image.height,
        }
    }
}

impl ToolResultContent {
    /// 按文字使用工具结果的地方（界面、预算、摘要）读取的文本：数组形式只取文字块，以换行连接。
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Self::Text(text) => Cow::Borrowed(text),
            Self::Blocks(blocks) => Cow::Owned(
                blocks
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultBlock::Text { text } => Some(text.as_str()),
                        ToolResultBlock::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }
    }

    pub fn has_images(&self) -> bool {
        matches!(self, Self::Blocks(blocks) if blocks.iter().any(|block| matches!(block, ToolResultBlock::Image { .. })))
    }
}

impl From<String> for ToolResultContent {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for ToolResultContent {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Off,
    On,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_input_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ApiKeyStatus {
    Saved,
    Environment,
    Missing,
}
