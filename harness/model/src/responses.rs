use super::{
    ModelDelta, ModelError, ModelOutput, ModelRequest, ModelResponseRejection,
    image_outside_user_message, invalid_response, native_tool_search, observe_usage,
    required_string, stream_provider_error, validate_reasoning_effort,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use tickloop_harness_protocol::{
    Content, ModelConfig, Protocol, ReasoningEffort, Role, ToolResultBlock, ToolResultContent,
    Usage,
};
use tokio::sync::mpsc::UnboundedSender;

pub(super) const ASSISTANT_PHASE_TYPE: &str = "_tickloop_assistant_phase";

pub(super) fn encode_request(
    config: &ModelConfig,
    request: ModelRequest,
) -> Result<Value, ModelError> {
    validate_reasoning_effort(config, request.reasoning_effort)?;
    let reasoning_effort = request.reasoning_effort;
    let offers_search = !request.deferred_tools.is_empty();
    let mut namespaces = HashMap::new();
    let mut input = Vec::new();
    for message in request.messages {
        let has_image = message
            .content
            .iter()
            .any(|content| matches!(content, Content::Image { .. }));
        if has_image && message.role != Role::User {
            return Err(image_outside_user_message());
        }
        // A user turn with images keeps its text and images together in one content array.
        let mut user_parts = Vec::new();
        let mut assistant_phase = None;
        for content in message.content {
            match content {
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value,
                } if value["type"] == ASSISTANT_PHASE_TYPE && message.role == Role::Assistant => {
                    assistant_phase = Some(required_string(&value, "phase")?.to_owned());
                }
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value,
                } if native_tool_search::is_record(Protocol::Responses, &value) => {
                    push_user_parts(&mut input, &mut user_parts);
                    if value["type"] == native_tool_search::RESPONSES_CALL_NAMESPACE {
                        if offers_search {
                            namespaces.insert(
                                required_string(&value, "call_id")?.to_owned(),
                                value["namespace"].clone(),
                            );
                        }
                    } else if offers_search {
                        input.push(value);
                    } else if let Some(text) = native_tool_search::readable_text(&value) {
                        input.push(assistant_text_item(text, None));
                    }
                }
                Content::Text { text } if message.role == Role::Assistant => {
                    input.push(assistant_text_item(text, assistant_phase.take()));
                }
                Content::Text { text } if has_image => {
                    user_parts.push(json!({"type":"input_text","text":text}))
                }
                Content::Image { source, .. } => {
                    user_parts.push(json!({"type":"input_image","image_url":source.data_url()}))
                }
                other => {
                    push_user_parts(&mut input, &mut user_parts);
                    let mut item = input_item(message.role, other)?;
                    if item["type"] == "function_call"
                        && let Some(namespace) = item["call_id"]
                            .as_str()
                            .and_then(|call_id| namespaces.remove(call_id))
                    {
                        item["namespace"] = namespace;
                    }
                    input.push(item);
                }
            }
        }
        push_user_parts(&mut input, &mut user_parts);
    }
    let mut body = json!({"model":config.model,"input":input,"max_output_tokens":config.max_output_tokens,"stream":true,"store":false});
    if let Some(temperature) = super::sampling_temperature(
        config,
        request.temperature,
        super::ModelConfigExt::thinking_enabled(config, reasoning_effort, true).unwrap_or(false),
    ) {
        body["temperature"] = json!(temperature);
    }
    if let Some(effort) = reasoning_effort {
        body["reasoning"] = json!({"effort":match effort {
            ReasoningEffort::Off => "none",
            _ => effort.label(),
        }});
    }
    if !request.tools.is_empty() || offers_search {
        body["tools"] =
            native_tool_search::responses_tools(&request.tools, &request.deferred_tools);
    }
    if let Some(format) = request.structured_output {
        body["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": format.name,
                "schema": format.schema,
                "strict": true
            }
        });
    }
    Ok(body)
}

pub(super) fn assistant_text_item(text: String, phase: Option<String>) -> Value {
    let mut item = json!({"type":"message","role":"assistant","content":text});
    if let Some(phase) = phase {
        item["phase"] = json!(phase);
    }
    item
}

fn input_item(role: Role, content: Content) -> Result<Value, ModelError> {
    Ok(match content {
        Content::Text { text } => json!({"type":"message","role":role,"content":text}),
        Content::ToolUse {
            id,
            name,
            input: arguments,
        } => {
            json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments.to_string()})
        }
        Content::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            json!({"type":"function_call_output","call_id":tool_use_id,"output":function_call_output(content)})
        }
        Content::ProtocolData {
            protocol: Protocol::Responses,
            value,
        } if value["type"] == "reasoning" => value,
        _ => {
            return Err(ModelError::Configuration(
                "history contains private reasoning that needs migration before using Responses"
                    .into(),
            ));
        }
    })
}

pub(super) fn push_user_parts(input: &mut Vec<Value>, parts: &mut Vec<Value>) {
    if !parts.is_empty() {
        input.push(json!({"type":"message","role":Role::User,"content":std::mem::take(parts)}));
    }
}

pub(super) fn function_call_output(content: ToolResultContent) -> Value {
    match content {
        ToolResultContent::Text(text) => json!(text),
        ToolResultContent::Blocks(blocks) => json!(
            blocks
                .into_iter()
                .map(|block| match block {
                    ToolResultBlock::Text { text } => json!({"type":"input_text","text":text}),
                    ToolResultBlock::Later(_) => {
                        json!({"type":"input_text","text":tickloop_harness_protocol::LATER_CONTENT_TEXT})
                    }
                    ToolResultBlock::Image { source, .. } => {
                        json!({"type":"input_image","image_url":source.data_url()})
                    }
                })
                .collect::<Vec<_>>()
        ),
    }
}

#[derive(Default)]
pub(super) struct ResponsesStream {
    output: Option<ModelOutput>,
    response_id: Option<String>,
    sequence: Option<u64>,
    items: BTreeMap<u64, ResponseItem>,
    usage: Usage,
    is_codex: bool,
    /// What the Codex client appends to each call's id once the stream ended.
    codex_origin: Option<String>,
    rotating_item_ids: bool,
}

struct ResponseItem {
    initial: Value,
    final_item: Option<Value>,
    /// `None` until the stream carries a function argument fragment.
    arguments: Option<String>,
    text: BTreeMap<u64, String>,
    summary_text: BTreeMap<u64, String>,
}

impl ResponseItem {
    /// Codex can stream parallel tool calls without any argument delta and send the arguments
    /// only on completion, so the first completion stands when nothing was streamed. Streamed
    /// arguments, and every later completion, must match exactly.
    fn complete_arguments(&mut self, completed: &str) -> bool {
        self.arguments.get_or_insert_with(|| completed.into()) == completed
    }

    fn recover_readable_reasoning(
        &mut self,
        value: &Value,
        events: &UnboundedSender<ModelDelta>,
    ) -> Result<(), ModelError> {
        if value["type"] != "reasoning" {
            return Ok(());
        }
        for (parts, kind, observed) in [
            (&value["summary"], "summary_text", &mut self.summary_text),
            (&value["content"], "reasoning_text", &mut self.text),
        ] {
            for (index, part) in parts.as_array().into_iter().flatten().enumerate() {
                if part["type"] == kind {
                    Self::recover_reasoning_part(
                        observed,
                        index as u64,
                        required_string(part, "text")?,
                        events,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn recover_reasoning_part(
        observed: &mut BTreeMap<u64, String>,
        index: u64,
        text: &str,
        events: &UnboundedSender<ModelDelta>,
    ) -> Result<(), ModelError> {
        let streamed = observed.entry(index).or_default();
        let suffix = text
            .strip_prefix(streamed.as_str())
            .ok_or_else(|| invalid_response("completed reasoning differs from its stream"))?;
        if !suffix.is_empty() {
            let _ = events.send(ModelDelta::Thinking(suffix.into()));
        }
        *streamed = text.into();
        Ok(())
    }
}

impl ResponsesStream {
    pub(super) fn for_codex() -> Self {
        Self {
            is_codex: true,
            ..Self::default()
        }
    }

    /// Announces each started call with the id the Codex client gives it after this origin.
    pub(super) fn with_codex_origin(self, origin: &str) -> Self {
        Self {
            codex_origin: Some(origin.into()),
            ..self
        }
    }

    /// GitHub Copilot gives the response and each item a new opaque id, and reasoning a new
    /// `encrypted_content`, on every event, so items are matched by their output index; order,
    /// type, tool identity and content are still checked. Copilot reasoning is never replayed.
    pub(super) fn with_rotating_item_ids() -> Self {
        Self {
            rotating_item_ids: true,
            ..Self::default()
        }
    }

    /// OpenAI seals reasoning again in the terminal response, whose sealing the output keeps.
    fn same_item(&self, left: &Value, right: &Value) -> bool {
        let stable_fields = |item: &Value| {
            let mut item = item.clone();
            if let Some(object) = item.as_object_mut() {
                if self.rotating_item_ids {
                    object.remove("id");
                }
                if self.rotating_item_ids || object.get("type") == Some(&json!("reasoning")) {
                    object.remove("encrypted_content");
                }
            }
            item
        };
        stable_fields(left) == stable_fields(right)
    }

    pub(super) fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    pub(super) fn accept(
        &mut self,
        data: Value,
        events: &UnboundedSender<ModelDelta>,
    ) -> Result<bool, ModelError> {
        let event_type = required_string(&data, "type")?;
        if matches!(
            event_type,
            "response.completed" | "response.failed" | "response.incomplete" | "response.cancelled"
        ) {
            let response_id = required_string(&data["response"], "id")?;
            if self.response_id.is_none()
                || !self.rotating_item_ids && self.response_id.as_deref() != Some(response_id)
            {
                return Err(invalid_response(
                    "terminal response does not match its start",
                ));
            }
            observe_usage(
                &mut self.usage,
                &data["response"]["usage"],
                Protocol::Responses,
                Some(events),
            )?;
        }
        if let Some(sequence) = data["sequence_number"].as_u64() {
            if self.sequence.is_some_and(|previous| sequence <= previous) {
                return Err(invalid_response("repeated or out-of-order Responses event"));
            }
            self.sequence = Some(sequence);
        }
        match event_type {
            "response.created" => {
                if self.response_id.is_some() {
                    return Err(invalid_response("duplicate response creation"));
                }
                let id = required_string(&data["response"], "id")?;
                if id.is_empty() {
                    return Err(invalid_response("response identity is empty"));
                }
                self.response_id = Some(id.into());
            }
            "response.output_item.added" => {
                let index = data["output_index"]
                    .as_u64()
                    .ok_or_else(|| invalid_response("missing output index"))?;
                let item = &data["item"];
                let id = required_string(item, "id")?;
                if self.response_id.is_none()
                    || index != self.items.len() as u64
                    || !self.rotating_item_ids
                        && self.items.values().any(|old| old.initial["id"] == id)
                {
                    return Err(invalid_response("duplicate or out-of-order response item"));
                }
                let mut tracked = ResponseItem {
                    initial: item.clone(),
                    final_item: None,
                    arguments: item["arguments"]
                        .as_str()
                        .filter(|arguments| !arguments.is_empty())
                        .map(Into::into),
                    text: BTreeMap::new(),
                    summary_text: BTreeMap::new(),
                };
                tracked.recover_readable_reasoning(item, events)?;
                if item["type"] == "function_call"
                    && let (Some(call_id), Some(name)) =
                        (item["call_id"].as_str(), item["name"].as_str())
                    && !call_id.is_empty()
                    && !name.is_empty()
                {
                    // The same identity the completed output gives the call.
                    let id = match (self.is_codex, &self.codex_origin) {
                        (true, Some(origin)) => format!("{call_id}|{id}|{origin}"),
                        (true, None) => format!("{call_id}|{id}"),
                        (false, _) => call_id.to_owned(),
                    };
                    let _ = events.send(ModelDelta::ToolUseStarted {
                        id,
                        name: name.to_owned(),
                    });
                }
                self.items.insert(index, tracked);
            }
            "response.output_text.delta" | "response.reasoning_text.delta" => {
                let item = self.item_mut(&data)?;
                let content_index = data["content_index"]
                    .as_u64()
                    .ok_or_else(|| invalid_response("missing content index"))?;
                let text = required_string(&data, "delta")?;
                item.text.entry(content_index).or_default().push_str(text);
                let delta = if data["type"] == "response.output_text.delta" {
                    ModelDelta::Text(text.into())
                } else {
                    ModelDelta::Thinking(text.into())
                };
                let _ = events.send(delta);
            }
            "response.reasoning_summary_text.delta" => {
                let item = self.item_mut(&data)?;
                if item.initial["type"] != "reasoning" {
                    return Err(invalid_response(
                        "reasoning summary targets a non-reasoning item",
                    ));
                }
                let index = data["summary_index"]
                    .as_u64()
                    .ok_or_else(|| invalid_response("missing summary index"))?;
                let text = required_string(&data, "delta")?;
                item.summary_text.entry(index).or_default().push_str(text);
                let _ = events.send(ModelDelta::Thinking(text.into()));
            }
            "response.reasoning_summary_part.done" => {
                let item = self.item_mut(&data)?;
                if item.initial["type"] != "reasoning" {
                    return Err(invalid_response(
                        "reasoning summary targets a non-reasoning item",
                    ));
                }
                let index = data["summary_index"]
                    .as_u64()
                    .ok_or_else(|| invalid_response("missing summary index"))?;
                ResponseItem::recover_reasoning_part(
                    &mut item.summary_text,
                    index,
                    required_string(&data["part"], "text")?,
                    events,
                )?;
                let _ = events.send(ModelDelta::Thinking("\n\n".into()));
            }
            "response.function_call_arguments.delta" => {
                let item = self.function_call_mut(&data)?;
                item.arguments
                    .get_or_insert_default()
                    .push_str(required_string(&data, "delta")?);
            }
            "response.function_call_arguments.done" => {
                let item = self.function_call_mut(&data)?;
                if !item.complete_arguments(required_string(&data, "arguments")?) {
                    return Err(invalid_response(
                        "completed function arguments differ from their stream",
                    ));
                }
            }
            "response.output_item.done" => {
                let index = data["output_index"]
                    .as_u64()
                    .ok_or_else(|| invalid_response("missing output index"))?;
                let item = self
                    .items
                    .get_mut(&index)
                    .ok_or_else(|| invalid_response("completed item has no start"))?;
                let completed = &data["item"];
                if item.final_item.is_some()
                    || !self.rotating_item_ids && completed["id"] != item.initial["id"]
                    || completed["type"] != item.initial["type"]
                {
                    return Err(invalid_response("duplicate or mismatched completed item"));
                }
                item.recover_readable_reasoning(completed, events)?;
                if completed["type"] == "function_call" {
                    if completed["call_id"] != item.initial["call_id"]
                        || completed["name"] != item.initial["name"]
                        || !completed["arguments"]
                            .as_str()
                            .is_some_and(|arguments| item.complete_arguments(arguments))
                    {
                        return Err(invalid_response(
                            "completed tool call differs from its stream",
                        ));
                    }
                } else {
                    for (content_index, text) in &item.text {
                        if completed["content"]
                            .get(*content_index as usize)
                            .and_then(|part| part["text"].as_str())
                            != Some(text)
                        {
                            return Err(invalid_response("completed text differs from its stream"));
                        }
                    }
                }
                item.final_item = Some(completed.clone());
            }
            "response.completed" => {
                let response = &data["response"];
                if response["status"] != "completed" {
                    return Err(invalid_response(
                        "response completion does not match its start",
                    ));
                }
                let mut content = Vec::new();
                let completed_items;
                let output = if self.is_codex
                    && (response["output"].is_null()
                        || response["output"].as_array().is_some_and(Vec::is_empty))
                {
                    // Codex can omit terminal output after sending complete output_item.done events.
                    completed_items = self
                        .items
                        .values()
                        .map(|item| {
                            item.final_item.clone().ok_or_else(|| {
                                invalid_response("response contains an incomplete item")
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    &completed_items
                } else {
                    response["output"]
                        .as_array()
                        .ok_or_else(|| invalid_response("missing response output"))?
                };
                if output.len() != self.items.len() {
                    return Err(invalid_response(
                        "response output differs from the streamed items",
                    ));
                }
                let mut call_ids = HashSet::new();
                for (index, item) in output.iter().enumerate() {
                    if !self
                        .items
                        .get(&(index as u64))
                        .and_then(|tracked| tracked.final_item.as_ref())
                        .is_some_and(|tracked| self.same_item(tracked, item))
                    {
                        return Err(invalid_response(
                            "response contains an incomplete or altered item",
                        ));
                    }
                    match required_string(item, "type")? {
                        "message" => {
                            for part in item["content"]
                                .as_array()
                                .ok_or_else(|| invalid_response("missing response text"))?
                            {
                                if part["type"] == "refusal" {
                                    return Err(ModelError::ResponseRejected(
                                        ModelResponseRejection::Refusal,
                                    ));
                                }
                                if part["type"] != "output_text" {
                                    return Err(invalid_response(
                                        "unsupported response content type",
                                    ));
                                }
                                if !item["phase"].is_null() {
                                    content.push(Content::ProtocolData {
                                        protocol: Protocol::Responses,
                                        value: json!({"type":ASSISTANT_PHASE_TYPE,"phase":required_string(item, "phase")?}),
                                    });
                                }
                                content.push(Content::Text {
                                    text: required_string(part, "text")?.into(),
                                });
                            }
                        }
                        "reasoning" => content.push(Content::ProtocolData {
                            protocol: Protocol::Responses,
                            value: item.clone(),
                        }),
                        native_tool_search::RESPONSES_SEARCH_CALL
                        | native_tool_search::RESPONSES_SEARCH_OUTPUT
                            if item["execution"]
                                == native_tool_search::RESPONSES_HOSTED_EXECUTION =>
                        {
                            content.push(Content::ProtocolData {
                                protocol: Protocol::Responses,
                                value: item.clone(),
                            })
                        }
                        "function_call" => {
                            let invalid_call = || {
                                ModelError::ResponseRejected(
                                    ModelResponseRejection::InvalidToolCall,
                                )
                            };
                            let id = item["call_id"].as_str().ok_or_else(invalid_call)?;
                            let name = item["name"].as_str().ok_or_else(invalid_call)?;
                            if id.is_empty() || name.is_empty() || !call_ids.insert(id.to_owned()) {
                                return Err(invalid_call());
                            }
                            let model_id = if self.is_codex {
                                let item_id = item["id"].as_str().ok_or_else(invalid_call)?;
                                if id.contains('|') || item_id.is_empty() || item_id.contains('|') {
                                    return Err(invalid_call());
                                }
                                format!("{id}|{item_id}")
                            } else {
                                id.to_owned()
                            };
                            let input: Value = serde_json::from_str(
                                item["arguments"].as_str().ok_or_else(invalid_call)?,
                            )
                            .map_err(|_| invalid_call())?;
                            if !input.is_object() {
                                return Err(invalid_call());
                            }
                            if let Some(namespace) = item["namespace"].as_str() {
                                content.push(Content::ProtocolData {
                                    protocol: Protocol::Responses,
                                    value: json!({"type":native_tool_search::RESPONSES_CALL_NAMESPACE,"call_id":model_id,"namespace":namespace}),
                                });
                            }
                            content.push(Content::ToolUse {
                                id: model_id,
                                name: name.into(),
                                input,
                            });
                        }
                        _ => return Err(invalid_response("unsupported response output item")),
                    }
                }
                let stop_reason = if call_ids.is_empty() {
                    "end_turn"
                } else {
                    "tool_use"
                }
                .into();
                self.output = Some(ModelOutput {
                    private_source: None,
                    content,
                    usage: self.usage.clone(),
                    stop_reason,
                });
                return Ok(true);
            }
            "response.failed" | "response.incomplete" | "response.cancelled" => {
                let response = &data["response"];
                if response["status"].as_str() != event_type.strip_prefix("response.") {
                    return Err(invalid_response(
                        "terminal response status does not match its event",
                    ));
                }
                if event_type == "response.incomplete" {
                    return Err(match response["incomplete_details"]["reason"].as_str() {
                        Some("max_output_tokens") => {
                            ModelError::ResponseRejected(ModelResponseRejection::OutputLimit)
                        }
                        Some("content_filter") => {
                            ModelError::ResponseRejected(ModelResponseRejection::ContentFilter)
                        }
                        _ => {
                            invalid_response("response ended with an unsupported incomplete reason")
                        }
                    });
                }
                if event_type == "response.failed" {
                    return Err(stream_provider_error(response));
                }
                return Err(ModelError::ProviderFailure);
            }
            "error" => {
                return Err(stream_provider_error(&data));
            }
            _ => {}
        }
        Ok(false)
    }

    pub(super) fn finish(self) -> Result<ModelOutput, ModelError> {
        self.output.ok_or(ModelError::StreamInterrupted)
    }

    fn item_mut(&mut self, data: &Value) -> Result<&mut ResponseItem, ModelError> {
        let index = data["output_index"]
            .as_u64()
            .ok_or_else(|| invalid_response("missing output index"))?;
        let item = self
            .items
            .get_mut(&index)
            .ok_or_else(|| invalid_response("delta has no output item"))?;
        if item.final_item.is_some()
            || !self.rotating_item_ids && data["item_id"] != item.initial["id"]
        {
            return Err(invalid_response(
                "delta targets a completed or different item",
            ));
        }
        Ok(item)
    }

    fn function_call_mut(&mut self, data: &Value) -> Result<&mut ResponseItem, ModelError> {
        let item = self.item_mut(data)?;
        if item.initial["type"] != "function_call" {
            return Err(invalid_response(
                "function arguments target a non-function item",
            ));
        }
        Ok(item)
    }
}
