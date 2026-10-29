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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_recorded_card"
    )]
    pub card: Option<ModelCard>,
    /// Omitted in older recorded snapshots; like opencode for unknown models, such a
    /// model is treated as text only.
    #[serde(default = "text_only")]
    pub input_modalities: Vec<InputModality>,
}

fn deserialize_recorded_card<'de, D>(deserializer: D) -> Result<Option<ModelCard>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut value = Option::<Value>::deserialize(deserializer)?;
    if let Some(Value::Object(card)) = value.as_mut() {
        // Older recorded snapshots duplicated the service ID; definition YAML stays strict.
        card.remove("service_model_id");
    }
    value
        .map(serde_json::from_value)
        .transpose()
        .map_err(serde::de::Error::custom)
}

pub(crate) fn text_only() -> Vec<InputModality> {
    vec![InputModality::Text]
}

impl ModelConfig {
    /// Placeholder model when rebuilding from the Transcript or deserializing a snapshot; the
    /// resume flow then swaps in the host's currently selected model.
    pub fn unselected() -> Self {
        Self {
            provider: ModelProvider::Api,
            auth_source: ModelAuthSource::ApiKeyEnv,
            name: String::new(),
            protocol: Protocol::Messages,
            base_url: String::new(),
            model: String::new(),
            api_key_env: String::new(),
            max_output_tokens: 0,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: String::new(),
            connection: None,
            card: None,
            input_modalities: vec![InputModality::Text],
        }
    }

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
    /// A block a later version wrote, kept in the Transcript; a request or a view shows
    /// [`LATER_CONTENT_TEXT`] in its place.
    #[serde(untagged)]
    Later(crate::LaterVariant<Content>),
}

/// What a model request or a view shows in place of content a later version wrote.
pub const LATER_CONTENT_TEXT: &str = "[Content from a later Tickloop version was omitted here.]";

impl crate::TaggedRecord for Content {
    const TAG: &'static str = "type";
    const KNOWN: &'static [&'static str] = &[
        "text",
        "tool_use",
        "tool_result",
        "thinking",
        "protocol_data",
        "image",
    ];

    /// Protocol data of a protocol only a later version speaks.
    fn holds_later_value(value: &Value) -> bool {
        value["type"] == "protocol_data"
            && crate::names_later_variant::<Protocol>(&value["protocol"])
    }
}

impl Content {
    /// The id a later version's block gives the call it makes, if it makes one.
    pub fn later_call_id(&self) -> Option<&str> {
        match self {
            Self::Later(later) => later.value()["id"].as_str(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 {
        media_type: ImageMediaType,
        data: String,
    },
    /// A source a later version knows; the image is shown as [`LATER_CONTENT_TEXT`].
    #[serde(untagged)]
    Later(crate::LaterVariant<ImageSource>),
}

impl crate::TaggedRecord for ImageSource {
    const TAG: &'static str = "type";
    const KNOWN: &'static [&'static str] = &["base64"];
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
    /// The inline `data:` URL form OpenAI-style protocols take for images; none for a source
    /// only a later version reads.
    pub fn data_url(&self) -> Option<String> {
        match self {
            Self::Base64 { media_type, data } => {
                Some(format!("data:{};base64,{data}", media_type.mime_type()))
            }
            Self::Later(_) => None,
        }
    }
}

/// Follows the `content` of Anthropic's tool_result (Codex `FunctionCallOutputBody`): a plain
/// string or an array of content blocks. The string form matches how older Transcripts were
/// written, and both forms deserialize directly.
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
    /// A block a later version wrote; a request or a view shows [`LATER_CONTENT_TEXT`].
    #[serde(untagged)]
    Later(crate::LaterVariant<ToolResultBlock>),
}

impl crate::TaggedRecord for ToolResultBlock {
    const TAG: &'static str = "type";
    const KNOWN: &'static [&'static str] = &["text", "image"];
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
    /// The text read by places that use tool results as text (UI, budgets, summaries): the array
    /// form takes only text blocks, joined with newlines.
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Self::Text(text) => Cow::Borrowed(text),
            Self::Blocks(blocks) => Cow::Owned(
                blocks
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultBlock::Text { text } => Some(text.as_str()),
                        ToolResultBlock::Later(_) => Some(LATER_CONTENT_TEXT),
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

/// `messages` as a model reads them: each block a later version wrote, and each image whose
/// source only a later version reads, becomes [`LATER_CONTENT_TEXT`], and so does the result of a
/// call a later block made, so every call still pairs with its result.
pub fn without_later_content(messages: &mut [Message]) {
    let later_calls = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(Content::later_call_id)
        .map(str::to_owned)
        .collect::<std::collections::HashSet<_>>();
    let placeholder = || Content::Text {
        text: LATER_CONTENT_TEXT.into(),
    };
    for block in messages.iter_mut().flat_map(|message| &mut message.content) {
        match block {
            Content::Later(_)
            | Content::Image {
                source: ImageSource::Later(_),
                ..
            } => *block = placeholder(),
            Content::ToolResult { tool_use_id, .. } if later_calls.contains(tool_use_id) => {
                *block = placeholder();
            }
            Content::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => {
                for result_block in blocks {
                    if matches!(
                        result_block,
                        ToolResultBlock::Later(_)
                            | ToolResultBlock::Image {
                                source: ImageSource::Later(_),
                                ..
                            }
                    ) {
                        *result_block = ToolResultBlock::Text {
                            text: LATER_CONTENT_TEXT.into(),
                        };
                    }
                }
            }
            _ => {}
        }
    }
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
    /// The part of `output_tokens` the model spent reasoning, when the service reports it.
    /// Absent from usage recorded before it was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
}

impl Usage {
    /// The whole input of the request, in the service's tokens. Messages reports the input it
    /// read from and wrote to the prompt cache apart from `input_tokens`; the other protocols
    /// count cached input within it.
    pub fn context_input_tokens(&self, protocol: Protocol) -> Option<u64> {
        let input = self.input_tokens?;
        Some(if protocol == Protocol::Messages {
            input
                .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
                .saturating_add(self.cache_write_input_tokens.unwrap_or(0))
        } else {
            input
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ApiKeyStatus {
    Saved,
    Environment,
    Missing,
}
