use super::{
    Content, ModelConfig, ModelDelta, ModelError, ModelOutput, ModelRequest,
    ModelResponseRejection, Protocol, ReasoningEffort, Role, ToolResultBlock, ToolResultContent,
    Usage, cards::Adapter, image_outside_user_message, invalid_response, observe_usage,
    required_string, stream_provider_error, validate_reasoning_effort,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use tokio::sync::mpsc::UnboundedSender;

/// opencode `SYNTHETIC_ATTACHMENT_PROMPT`: introduces tool-result images moved into a user turn.
const TOOL_RESULT_IMAGES_PROMPT: &str = "Attached media from tool result:";

pub(super) fn encode_request(
    config: &ModelConfig,
    request: ModelRequest,
) -> Result<Value, ModelError> {
    validate_reasoning_effort(config, request.reasoning_effort)?;
    let reasoning_effort = request.reasoning_effort;
    let structured_tool = super::uses_structured_response_tool(config, &request);
    let mut messages = Vec::new();
    for message in request.messages {
        let has_image = message
            .content
            .iter()
            .any(|content| matches!(content, Content::Image { .. }));
        if has_image && message.role != Role::User {
            return Err(image_outside_user_message());
        }
        // Text-only turns stay one string; a turn with images becomes a content-part array.
        let mut text = String::new();
        let mut user_parts = Vec::new();
        let mut thinking = String::new();
        let mut tool_calls = Vec::new();
        let mut results = Vec::new();
        let mut result_images = Vec::new();
        for content in message.content {
            match content {
                Content::Text {text:value} if has_image => user_parts.push(json!({"type":"text","text":value})),
                Content::Text {text:value} => text.push_str(&value),
                Content::Image {source,..} => user_parts.push(json!({"type":"image_url","image_url":{"url":source.data_url()}})),
                Content::Thinking {thinking:value,signature:None} if message.role == Role::Assistant => thinking.push_str(&value),
                Content::ToolUse {id,name,input} if message.role == Role::Assistant => tool_calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})),
                Content::ToolResult {tool_use_id,content,..} if message.role == Role::User => {
                    results.push(json!({"role":"tool","tool_call_id":tool_use_id,"content":content.text()}));
                    if let ToolResultContent::Blocks(blocks) = content {
                        result_images.extend(blocks.into_iter().filter_map(|block| match block {
                            ToolResultBlock::Image {source,..} => Some(json!({"type":"image_url","image_url":{"url":source.data_url()}})),
                            ToolResultBlock::Text {..} => None,
                        }));
                    }
                }
                _ => return Err(ModelError::Configuration("history contains private or unsupported content that needs migration before using Chat Completions".into())),
            }
        }
        if !results.is_empty() {
            if !text.is_empty() || !user_parts.is_empty() {
                return Err(ModelError::Configuration(
                    "tool results and ordinary text need separate messages".into(),
                ));
            }
            messages.extend(results);
            // Tool messages only carry text, so their images follow in a user turn (opencode).
            if !result_images.is_empty() {
                let mut parts = vec![json!({"type":"text","text":TOOL_RESULT_IMAGES_PROMPT})];
                parts.extend(result_images);
                messages.push(json!({"role":"user","content":parts}));
            }
        } else {
            let content = if has_image {
                json!(user_parts)
            } else {
                json!(text)
            };
            let mut encoded = json!({"role":message.role,"content":content});
            if !thinking.is_empty() {
                encoded["reasoning_content"] = json!(thinking);
            }
            if !tool_calls.is_empty() {
                encoded["tool_calls"] = json!(tool_calls);
            }
            messages.push(encoded);
        }
    }
    let mut body = json!({"model":config.model,"messages":messages,"max_tokens":config.max_output_tokens,"stream":true,"stream_options":{"include_usage":true}});
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools.into_iter().map(|tool|json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}})).collect::<Vec<_>>());
    }
    if matches!(
        config.adapter(),
        Adapter::KimiChatCompletions | Adapter::QwenChatCompletions
    ) {
        body.as_object_mut()
            .expect("request object")
            .remove("max_tokens");
        body["max_completion_tokens"] = json!(config.max_output_tokens);
        if config.adapter() == Adapter::KimiChatCompletions {
            body.as_object_mut()
                .expect("request object")
                .remove("temperature");
            if let Some(effort) = reasoning_effort {
                body["reasoning_effort"] = json!(effort.label());
            }
        } else {
            body["enable_thinking"] = json!(reasoning_effort != Some(ReasoningEffort::Off));
            if let Some(effort) = reasoning_effort
                .filter(|effort| !matches!(effort, ReasoningEffort::Off | ReasoningEffort::On))
            {
                body["reasoning_effort"] = json!(effort.label());
            }
            let preserved = config.model.starts_with("qwen3.8-max")
                || config.model.starts_with("qwen3.7-plus")
                || config.model.starts_with("qwen3.8-flash");
            if preserved {
                body["preserve_thinking"] = json!(true);
            } else {
                for message in body["messages"].as_array_mut().expect("messages array") {
                    message
                        .as_object_mut()
                        .expect("message object")
                        .remove("reasoning_content");
                }
            }
        }
        if let Some(format) = request.structured_output {
            if body.get("tools").is_some() {
                return Err(ModelError::Configuration(
                    "Structured output cannot be combined with executable tools".into(),
                ));
            }
            if structured_tool {
                body["tools"] = json!([{"type":"function","function":{"name":format.name,"description":"Return exactly one structured result using this tool; no other content or tools.","parameters":format.schema}}]);
                body["tool_choice"] = json!("auto");
                body["messages"].as_array_mut().expect("messages array").insert(0, json!({"role":"system","content":format!("Call {} exactly once to return the structured result. Emit no ordinary text and no other tool calls.", format.name)}));
            } else {
                body["response_format"] = json!({"type":"json_schema","json_schema":{"name":format.name,"schema":format.schema,"strict":true}});
            }
        }
        return Ok(body);
    }
    if config.adapter() == Adapter::GlmChatCompletions {
        super::glm::shape_request(&mut body, reasoning_effort, request.structured_output)?;
        return Ok(body);
    }
    if let Some(effort) = reasoning_effort {
        match effort {
            ReasoningEffort::Off => body["thinking"] = json!({"type":"disabled"}),
            _ => {
                body["thinking"] = json!({"type":"enabled"});
                body["reasoning_effort"] = json!(effort.label());
            }
        }
    }
    if let Some(format) = request.structured_output {
        body["response_format"] = if config.adapter() == Adapter::DeepseekChatCompletions {
            json!({"type":"json_object"})
        } else {
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": format.name,
                    "schema": format.schema,
                    "strict": true
                }
            })
        };
    }
    Ok(body)
}

#[derive(Default)]
pub(super) struct ChatStream {
    id: Option<String>,
    text: String,
    thinking: String,
    tools: BTreeMap<u64, ToolCall>,
    finish_reason: Option<String>,
    usage: Usage,
    refused: bool,
    /// GLM streamed tool calls follow the official client: a call is keyed by its index, and
    /// later chunks for that index may repeat the identity from the first chunk.
    tool_identity_by_index: bool,
}

struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ChatStream {
    pub(super) fn for_adapter(adapter: Adapter) -> Self {
        Self {
            tool_identity_by_index: adapter == Adapter::GlmChatCompletions,
            ..Self::default()
        }
    }

    pub(super) fn accept(
        &mut self,
        data: Value,
        events: &UnboundedSender<ModelDelta>,
    ) -> Result<bool, ModelError> {
        let provider_failed = data.get("error").is_some_and(|error| !error.is_null());
        if provider_failed && data.get("id").is_none() {
            return Err(stream_provider_error(&data));
        }
        let id = required_string(&data, "id")?;
        if id.is_empty() {
            return Err(invalid_response("completion identity is empty"));
        }
        if self.id.as_deref().is_some_and(|previous| previous != id) {
            return Err(invalid_response(
                "Chat Completions stream changed response identity",
            ));
        }
        self.id = Some(id.into());
        observe_usage(
            &mut self.usage,
            &data["usage"],
            Protocol::ChatCompletions,
            Some(events),
        )?;
        if provider_failed {
            return Err(stream_provider_error(&data));
        }
        let choices = data["choices"]
            .as_array()
            .ok_or_else(|| invalid_response("missing Chat Completions choices"))?;
        if choices.is_empty() {
            return Ok(false);
        }
        if choices.len() != 1 || choices[0]["index"] != 0 {
            return Err(invalid_response("unexpected completion choice"));
        }
        let choice = &choices[0];
        if self.finish_reason.is_some() {
            return Err(invalid_response(
                "content arrived after the completion ended",
            ));
        }
        let delta = &choice["delta"];
        if delta["refusal"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
        {
            self.refused = true;
        }
        // Final chunks commonly carry `content: ""`; empty strings are not output and must
        // not block a retry of a failure reported by the same chunk.
        if let Some(text) = delta["reasoning_content"]
            .as_str()
            .filter(|text| !text.is_empty())
        {
            self.thinking.push_str(text);
            let _ = events.send(ModelDelta::Thinking(text.into()));
        }
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            self.text.push_str(text);
            let _ = events.send(ModelDelta::Text(text.into()));
        }
        if let Some(calls) = delta.get("tool_calls").filter(|calls| !calls.is_null()) {
            for call in calls
                .as_array()
                .ok_or_else(|| invalid_response("invalid tool call list"))?
            {
                let index = call["index"]
                    .as_u64()
                    .ok_or_else(|| invalid_response("missing tool call index"))?;
                let repeated_identity = self.tool_identity_by_index
                    && self.tools.get(&index).is_some_and(|tool| {
                        call["id"]
                            .as_str()
                            .is_none_or(|id| id.is_empty() || id == tool.id)
                    });
                if let Some(id) = call["id"]
                    .as_str()
                    .filter(|id| !id.is_empty() && !repeated_identity)
                {
                    if self.tools.contains_key(&index)
                        || self.tools.values().any(|tool| tool.id == id)
                        || index != self.tools.len() as u64
                    {
                        return Err(invalid_response("duplicate or out-of-order tool call"));
                    }
                    if call["type"] != "function" {
                        return Err(invalid_response("unsupported tool call type"));
                    }
                    self.tools.insert(
                        index,
                        ToolCall {
                            id: id.into(),
                            name: String::new(),
                            arguments: String::new(),
                        },
                    );
                }
                let tool = self.tools.get_mut(&index).ok_or_else(|| {
                    invalid_response("tool arguments arrived without an identity")
                })?;
                if let Some(name) = call["function"]["name"].as_str()
                    && (!repeated_identity || tool.name.is_empty())
                {
                    tool.name.push_str(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    tool.arguments.push_str(arguments);
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.into());
        }
        Ok(false)
    }

    pub(super) fn finish(self) -> Result<ModelOutput, ModelError> {
        let finish = self
            .finish_reason
            .ok_or_else(|| invalid_response("completion ended without a finish reason"))?;
        let stop_reason = match finish.as_str() {
            "stop" => "end_turn",
            "tool_calls" => "tool_use",
            "length" => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::OutputLimit,
                ));
            }
            "content_filter" | "sensitive" => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::ContentFilter,
                ));
            }
            "model_context_window_exceeded" => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::ContextLimit,
                ));
            }
            "network_error" => {
                return Err(ModelError::Http { status: 500 });
            }
            "insufficient_system_resource" => {
                return Err(ModelError::Http { status: 503 });
            }
            _ => {
                return Err(invalid_response(
                    "completion ended with an unsupported finish reason",
                ));
            }
        }
        .into();
        if self.refused {
            return Err(ModelError::ResponseRejected(
                ModelResponseRejection::Refusal,
            ));
        }
        if self.tools.is_empty() == (finish == "tool_calls") {
            return Err(invalid_response(
                "tool calls do not match the finish reason",
            ));
        }
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(Content::Thinking {
                thinking: self.thinking,
                signature: None,
            });
        }
        if !self.text.is_empty() {
            content.push(Content::Text { text: self.text });
        }
        let mut identities = HashSet::new();
        for tool in self.tools.into_values() {
            if tool.id.is_empty() || tool.name.is_empty() || !identities.insert(tool.id.clone()) {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::InvalidToolCall,
                ));
            }
            let input: Value = serde_json::from_str(&tool.arguments).map_err(|_| {
                ModelError::ResponseRejected(ModelResponseRejection::InvalidToolCall)
            })?;
            if !input.is_object() {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::InvalidToolCall,
                ));
            }
            content.push(Content::ToolUse {
                id: tool.id,
                name: tool.name,
                input,
            });
        }
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: self.usage,
            stop_reason,
        })
    }
}
