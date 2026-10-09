use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedSender;

use super::{
    ModelDelta, ModelError, ModelOutput, ModelRequest, ModelResponseRejection, invalid_response,
    validate_reasoning_effort,
};
use tickloop_agent_protocol::{
    Content, ImageSource, ModelConfig, Protocol, ReasoningEffort, Role, ToolResultBlock,
    ToolResultContent, Usage,
};

pub(super) fn encode_request(
    config: &ModelConfig,
    request: ModelRequest,
) -> Result<Value, ModelError> {
    validate_reasoning_effort(config, request.reasoning_effort)?;
    let mut system = Vec::new();
    let mut contents = Vec::new();
    let mut tools_by_id = HashMap::new();
    for message in request.messages {
        let mut parts = Vec::new();
        let mut blocks = message.content.into_iter().peekable();
        while let Some(block) = blocks.next() {
            let part = match block {
                Content::ProtocolData {
                    protocol: Protocol::Google,
                    value,
                } => {
                    let part = value
                        .get("part")
                        .filter(|part| part.is_object())
                        .ok_or_else(|| invalid_response("invalid Google private history"))?
                        .clone();
                    if let Some(Content::ToolUse { id, name, .. }) = blocks.peek() {
                        tools_by_id.insert(
                            id.clone(),
                            (
                                name.clone(),
                                part["functionCall"]["id"].as_str().map(str::to_owned),
                            ),
                        );
                    }
                    if value["paired"] == true
                        && matches!(
                            blocks.peek(),
                            Some(
                                Content::Text { .. }
                                    | Content::Thinking { .. }
                                    | Content::ToolUse { .. }
                            )
                        )
                    {
                        blocks.next();
                    }
                    part
                }
                Content::Text { text } => json!({"text": text}),
                Content::Thinking {
                    thinking,
                    signature: None,
                } => json!({"text": thinking, "thought": true}),
                Content::Image {
                    source: ImageSource::Base64 { media_type, data },
                    ..
                } => {
                    json!({"inlineData": {"mimeType": media_type.mime_type(), "data": data}})
                }
                Content::ToolUse { id, name, input } => {
                    tools_by_id.insert(id.clone(), (name.clone(), Some(id.clone())));
                    json!({"functionCall": {"id": id, "name": name, "args": input}})
                }
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let (name, wire_id) = tools_by_id.get(&tool_use_id).ok_or_else(|| {
                        ModelError::Configuration("Google tool result has no preceding call".into())
                    })?;
                    let response = if is_error {
                        json!({"error": content.text()})
                    } else {
                        json!({"output": content.text()})
                    };
                    let mut result =
                        json!({"functionResponse": {"name": name, "response": response}});
                    if let Some(id) = wire_id {
                        result["functionResponse"]["id"] = json!(id);
                    }
                    parts.push(result);
                    if let ToolResultContent::Blocks(blocks) = content {
                        for block in blocks {
                            if let ToolResultBlock::Image {
                                source: ImageSource::Base64 { media_type, data },
                                ..
                            } = block
                            {
                                parts.push(json!({"inlineData": {"mimeType": media_type.mime_type(), "data": data}}));
                            }
                        }
                    }
                    continue;
                }
                _ => {
                    return Err(ModelError::Configuration(
                        "unsupported private history for Google API".into(),
                    ));
                }
            };
            parts.push(part);
        }
        if message.role == Role::System {
            system.extend(parts);
        } else if !parts.is_empty() {
            let role = if message.role == Role::Assistant {
                "model"
            } else {
                "user"
            };
            contents.push(json!({"role": role, "parts": parts}));
        }
    }
    let mut body = json!({"contents": contents, "generationConfig": {"maxOutputTokens": config.max_output_tokens}});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": system});
    }
    if let Some(temperature) = super::sampling_temperature(
        config,
        request.temperature,
        super::ModelConfigExt::thinking_enabled(config, request.reasoning_effort, true)
            .unwrap_or(false),
    ) {
        body["generationConfig"]["temperature"] = json!(temperature);
    }
    if let Some(effort) = request.reasoning_effort {
        body["generationConfig"]["thinkingConfig"] = if effort == ReasoningEffort::Off {
            json!({"thinkingBudget": 0})
        } else {
            json!({"thinkingLevel": effort.label(), "includeThoughts": true})
        };
    }
    if !request.tools.is_empty() {
        body["tools"] = json!([{"functionDeclarations": request.tools.into_iter().map(|tool| json!({
            "name": tool.name, "description": tool.description, "parametersJsonSchema": tool.input_schema
        })).collect::<Vec<_>>()}]);
    }
    if let Some(format) = request.structured_output {
        if body.get("tools").is_some() {
            return Err(ModelError::Configuration(
                "Google structured output cannot be combined with executable tools".into(),
            ));
        }
        body["generationConfig"]["responseMimeType"] = json!("application/json");
        body["generationConfig"]["responseJsonSchema"] = format.schema;
    }
    Ok(body)
}

#[derive(Default)]
pub(super) struct GoogleStream {
    content: Vec<Content>,
    response_id: Option<String>,
    finish_reason: Option<String>,
    usage: Usage,
    call_count: usize,
    tool_ids: HashSet<String>,
    candidate_tokens: Option<u64>,
    thought_tokens: Option<u64>,
}

impl GoogleStream {
    pub(super) fn accept(
        &mut self,
        data: Value,
        events: &UnboundedSender<ModelDelta>,
    ) -> Result<bool, ModelError> {
        if data.get("error").is_some() {
            return Err(super::stream_provider_error(&data));
        }
        if data["promptFeedback"].get("blockReason").is_some() {
            return Err(ModelError::ResponseRejected(
                ModelResponseRejection::ContentFilter,
            ));
        }
        if let Some(id) = data["responseId"].as_str() {
            if self.response_id.as_deref().is_some_and(|prior| prior != id) {
                return Err(invalid_response("Google stream changed response identity"));
            }
            self.response_id = Some(id.to_owned());
        }
        if let Some(usage) = data.get("usageMetadata") {
            if !usage.is_null() && !usage.is_object() {
                return Err(invalid_response("invalid Google usage metadata"));
            }
            let mut failure = None;
            let mut observation = json!({});
            for (native, normalized) in [
                ("promptTokenCount", "input_tokens"),
                ("cachedContentTokenCount", "cache_read_input_tokens"),
            ] {
                if let Some(value) = usage.get(native) {
                    observation[normalized] = value.clone();
                }
            }
            for (name, target) in [
                ("candidatesTokenCount", &mut self.candidate_tokens),
                ("thoughtsTokenCount", &mut self.thought_tokens),
            ] {
                let mut observed = None;
                super::parse_usage_counter(&mut observed, usage.get(name), true, &mut failure);
                if let Some(observed) = observed {
                    if target.is_some_and(|prior| observed < prior) {
                        failure.get_or_insert_with(|| {
                            invalid_response("cumulative Google usage counter decreased")
                        });
                    }
                    *target = Some(observed);
                }
            }
            if self.candidate_tokens.is_some() || self.thought_tokens.is_some() {
                match self
                    .candidate_tokens
                    .unwrap_or(0)
                    .checked_add(self.thought_tokens.unwrap_or(0))
                {
                    Some(output) => observation["output_tokens"] = json!(output),
                    None => {
                        failure.get_or_insert_with(|| invalid_response("Google usage overflow"));
                    }
                }
            }
            super::observe_usage(
                &mut self.usage,
                &observation,
                Protocol::Google,
                Some(events),
            )?;
            if let Some(error) = failure {
                return Err(error);
            }
        }
        let Some(candidates) = data.get("candidates") else {
            if data.get("usageMetadata").is_some() {
                return Ok(false);
            }
            return Err(invalid_response("missing Google candidates"));
        };
        let candidates = candidates
            .as_array()
            .ok_or_else(|| invalid_response("invalid Google candidates"))?;
        if candidates.len() > 1 {
            return Err(invalid_response("multiple Google candidates"));
        }
        for candidate in candidates {
            if candidate["index"].as_u64().is_some_and(|index| index != 0) {
                return Err(invalid_response("unexpected Google candidate index"));
            }
            if let Some(parts) = candidate["content"].get("parts") {
                for part in parts
                    .as_array()
                    .ok_or_else(|| invalid_response("invalid Google parts"))?
                {
                    let public = if let Some(text) = part["text"].as_str() {
                        if part["thought"] == true {
                            let _ = events.send(ModelDelta::Thinking(text.into()));
                            Content::Thinking {
                                thinking: text.into(),
                                signature: None,
                            }
                        } else {
                            let _ = events.send(ModelDelta::Text(text.into()));
                            Content::Text { text: text.into() }
                        }
                    } else if let Some(call) = part.get("functionCall") {
                        let name = super::required_string(call, "name")?.to_owned();
                        let input = call
                            .get("args")
                            .filter(|args| args.is_object())
                            .ok_or_else(|| invalid_response("invalid Google function arguments"))?
                            .clone();
                        let id = call["id"].as_str().map(str::to_owned).unwrap_or_else(|| {
                            let digest = Sha256::digest(
                                format!(
                                    "{}:{}:{}",
                                    self.response_id.as_deref().unwrap_or(""),
                                    self.call_count,
                                    call
                                )
                                .as_bytes(),
                            );
                            format!("google_{:x}", digest)[..63].to_owned()
                        });
                        if id.is_empty() || !self.tool_ids.insert(id.clone()) {
                            return Err(invalid_response(
                                "missing or duplicate Google function call identity",
                            ));
                        }
                        self.call_count += 1;
                        Content::ToolUse { id, name, input }
                    } else {
                        if part.get("thoughtSignature").is_some() {
                            self.content.push(Content::ProtocolData {
                                protocol: Protocol::Google,
                                value: json!({"part": part}),
                            });
                            continue;
                        }
                        return Err(invalid_response("unsupported Google response part"));
                    };
                    self.content.push(Content::ProtocolData {
                        protocol: Protocol::Google,
                        value: json!({"part": part, "paired": true}),
                    });
                    self.content.push(public);
                }
            }
            if let Some(reason) = candidate["finishReason"].as_str() {
                if self
                    .finish_reason
                    .as_deref()
                    .is_some_and(|prior| prior != reason)
                {
                    return Err(invalid_response("Google stream changed finish reason"));
                }
                self.finish_reason = Some(reason.to_owned());
            }
        }
        Ok(false)
    }

    pub(super) fn finish(self) -> Result<ModelOutput, ModelError> {
        let reason = self
            .finish_reason
            .as_deref()
            .ok_or(if self.content.is_empty() {
                ModelError::StreamInterrupted
            } else {
                ModelError::StreamInterruptedAfterOutput
            })?;
        let stop_reason = match reason {
            "STOP" if self.call_count > 0 => "tool_use",
            "STOP" => "end_turn",
            "MAX_TOKENS" if self.call_count > 0 => {
                return Err(invalid_response("Google function call was truncated"));
            }
            "MAX_TOKENS" => "max_tokens",
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
            | "IMAGE_SAFETY" => {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::ContentFilter,
                ));
            }
            _ => return Err(invalid_response("unsupported Google finish reason")),
        };
        Ok(ModelOutput {
            private_source: None,
            content: self.content,
            usage: self.usage,
            stop_reason: stop_reason.into(),
        })
    }
}
