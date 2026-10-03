use serde_json::Value;
use std::collections::BTreeMap;

use super::{
    Content, ModelConfig, ModelError, ModelOutput, ModelRequest, ModelResponseRejection, Protocol,
};

#[derive(Default)]
pub(super) struct StructuredResponsePolicy {
    pub expected_tool: Option<String>,
    saw_expected_tool: bool,
    tool_starts: usize,
    chat_tool_names: BTreeMap<u64, String>,
    has_finished_response: bool,
    expected_response_item: Option<ExpectedResponseItem>,
}

struct ExpectedResponseItem {
    output_index: u64,
    item_id: String,
    is_completed: bool,
}

impl StructuredResponsePolicy {
    pub fn for_request(config: &ModelConfig, request: &ModelRequest) -> Option<Self> {
        let format = request.structured_output.as_ref()?;
        if !request.tools.is_empty() {
            return None;
        }
        Some(Self {
            expected_tool: super::uses_structured_response_tool(config, request)
                .then(|| format.name.clone()),
            ..Self::default()
        })
    }

    pub fn for_codex_body(body: &Value) -> Option<Self> {
        let tools = body["tools"].as_array()?;
        if body["tool_choice"] != "required" || tools.len() != 1 || tools[0]["strict"] != true {
            return None;
        }
        Some(Self {
            expected_tool: Some(tools[0]["name"].as_str()?.into()),
            ..Self::default()
        })
    }

    fn check_tool(&mut self, name: Option<&str>) -> Result<(), ModelError> {
        if self.expected_tool.as_deref().is_none() || name != self.expected_tool.as_deref() {
            return Err(ModelError::ResponseRejected(
                ModelResponseRejection::UnexpectedToolOperation,
            ));
        }
        self.saw_expected_tool = true;
        Ok(())
    }

    pub fn check_event(&mut self, protocol: Protocol, data: &Value) -> Result<(), ModelError> {
        match protocol {
            Protocol::Messages => {
                for block in data
                    .get("content_block")
                    .into_iter()
                    .chain(data["message"]["content"].as_array().into_iter().flatten())
                {
                    if block["type"] == "tool_use" {
                        self.tool_starts += 1;
                        if self.tool_starts > 1 {
                            return Err(ModelError::ResponseRejected(
                                ModelResponseRejection::UnexpectedToolOperation,
                            ));
                        }
                        self.check_tool(block["name"].as_str())?;
                    } else if block["type"].as_str().is_some_and(|kind| {
                        kind.ends_with("tool_use") || kind.ends_with("tool_result")
                    }) {
                        return Err(ModelError::ResponseRejected(
                            ModelResponseRejection::UnexpectedToolOperation,
                        ));
                    }
                }
            }
            Protocol::ChatCompletions => {
                for choice in data["choices"].as_array().into_iter().flatten() {
                    if choice["delta"]
                        .get("function_call")
                        .is_some_and(|call| !call.is_null())
                        || choice["message"]
                            .get("function_call")
                            .is_some_and(|call| !call.is_null())
                        || choice["finish_reason"] == "function_call"
                    {
                        return Err(ModelError::ResponseRejected(
                            ModelResponseRejection::UnexpectedToolOperation,
                        ));
                    }
                    for payload in [&choice["delta"], &choice["message"]] {
                        if let Some(calls) =
                            payload.get("tool_calls").filter(|calls| !calls.is_null())
                        {
                            if self.expected_tool.is_none()
                                && calls.as_array().is_none_or(|calls| !calls.is_empty())
                            {
                                return Err(ModelError::ResponseRejected(
                                    ModelResponseRejection::UnexpectedToolOperation,
                                ));
                            }
                            for call in calls.as_array().into_iter().flatten() {
                                let index =
                                    call["index"].as_u64().ok_or(ModelError::ResponseRejected(
                                        ModelResponseRejection::UnexpectedToolOperation,
                                    ))?;
                                if let Some(fragment) = call["function"]["name"].as_str() {
                                    let name = self.chat_tool_names.entry(index).or_default();
                                    if name != fragment {
                                        name.push_str(fragment);
                                    }
                                    let expected =
                                        self.expected_tool.as_deref().unwrap_or_default();
                                    if !expected.starts_with(name.as_str()) {
                                        return Err(ModelError::ResponseRejected(
                                            ModelResponseRejection::UnexpectedToolOperation,
                                        ));
                                    }
                                    if name.as_str() == expected {
                                        self.saw_expected_tool = true;
                                    }
                                }
                                if self.chat_tool_names.len() > 1 {
                                    return Err(ModelError::ResponseRejected(
                                        ModelResponseRejection::UnexpectedToolOperation,
                                    ));
                                }
                            }
                        }
                    }
                    if self.expected_tool.is_none() && choice["finish_reason"] == "tool_calls" {
                        return Err(ModelError::ResponseRejected(
                            ModelResponseRejection::UnexpectedToolOperation,
                        ));
                    }
                }
            }
            Protocol::Responses => {
                let event_type = data["type"].as_str().unwrap_or_default();
                let event_family = event_type
                    .strip_prefix("response.")
                    .and_then(|kind| kind.split('.').next())
                    .unwrap_or_default();
                let is_tool_event = event_family.contains("_call")
                    || event_family.starts_with("mcp_")
                    || event_family.ends_with("tool_use")
                    || event_family.ends_with("tool_result");
                let is_expected_arguments = self.expected_tool.is_some()
                    && matches!(
                        event_type,
                        "response.function_call_arguments.delta"
                            | "response.function_call_arguments.done"
                    );
                if is_tool_event && !is_expected_arguments {
                    return Err(ModelError::ResponseRejected(
                        ModelResponseRejection::UnexpectedToolOperation,
                    ));
                }
                if is_expected_arguments
                    && !self.expected_response_item.as_ref().is_some_and(|item| {
                        !item.is_completed
                            && data["output_index"].as_u64() == Some(item.output_index)
                            && data["item_id"].as_str() == Some(item.item_id.as_str())
                    })
                {
                    return Err(ModelError::ResponseRejected(
                        ModelResponseRejection::UnexpectedToolOperation,
                    ));
                }
                let output = data["response"]["output"].as_array();
                if output.is_some_and(|items| {
                    items
                        .iter()
                        .filter(|item| item["type"] == "function_call")
                        .count()
                        > 1
                }) {
                    return Err(ModelError::ResponseRejected(
                        ModelResponseRejection::UnexpectedToolOperation,
                    ));
                }
                for item in data
                    .get("item")
                    .into_iter()
                    .chain(data["response"]["output"].as_array().into_iter().flatten())
                {
                    if let Some(kind) = item["type"].as_str()
                        && (kind.ends_with("_call")
                            || kind.ends_with("_call_output")
                            || kind.ends_with("tool_use")
                            || kind.ends_with("tool_result")
                            || matches!(kind, "mcp_approval_request" | "mcp_list_tools"))
                    {
                        if kind != "function_call" {
                            return Err(ModelError::ResponseRejected(
                                ModelResponseRejection::UnexpectedToolOperation,
                            ));
                        }
                        if data["type"] == "response.output_item.added" {
                            self.tool_starts += 1;
                            if self.tool_starts > 1 {
                                return Err(ModelError::ResponseRejected(
                                    ModelResponseRejection::UnexpectedToolOperation,
                                ));
                            }
                        }
                        self.check_tool(item["name"].as_str())?;
                        if data["type"] == "response.output_item.added" {
                            let index = data["output_index"].as_u64().ok_or(
                                ModelError::ResponseRejected(
                                    ModelResponseRejection::UnexpectedToolOperation,
                                ),
                            )?;
                            let id = item["id"].as_str().filter(|id| !id.is_empty()).ok_or(
                                ModelError::ResponseRejected(
                                    ModelResponseRejection::UnexpectedToolOperation,
                                ),
                            )?;
                            self.expected_response_item = Some(ExpectedResponseItem {
                                output_index: index,
                                item_id: id.into(),
                                is_completed: false,
                            });
                        } else if data["type"] == "response.output_item.done"
                            && let Some(item) = &mut self.expected_response_item
                        {
                            item.is_completed = true;
                        }
                    }
                    for part in item["content"].as_array().into_iter().flatten() {
                        if part["type"].as_str().is_some_and(|kind| {
                            kind.ends_with("_call")
                                || kind.ends_with("_call_output")
                                || matches!(kind, "tool_use" | "tool_result")
                        }) {
                            return Err(ModelError::ResponseRejected(
                                ModelResponseRejection::UnexpectedToolOperation,
                            ));
                        }
                    }
                }
            }
            Protocol::Google => {
                for candidate in data["candidates"].as_array().into_iter().flatten() {
                    for part in candidate["content"]["parts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        if part.get("functionCall").is_some()
                            || part.get("functionResponse").is_some()
                        {
                            return Err(ModelError::ResponseRejected(
                                ModelResponseRejection::UnexpectedToolOperation,
                            ));
                        }
                    }
                }
            }
            Protocol::SystemOne => {}
        }
        self.has_finished_response |= match protocol {
            Protocol::Messages => data["type"] == "message_stop",
            Protocol::Responses => matches!(
                data["type"].as_str(),
                Some("response.completed" | "response.done")
            ),
            _ => false,
        };
        Ok(())
    }

    pub fn check_tool_names<'a>(
        &self,
        names: impl Iterator<Item = &'a str>,
    ) -> Result<(), ModelError> {
        let mut count = 0;
        for name in names {
            count += 1;
            if self.expected_tool.as_deref() != Some(name) || count > 1 {
                return Err(ModelError::ResponseRejected(
                    ModelResponseRejection::UnexpectedToolOperation,
                ));
            }
        }
        Ok(())
    }

    pub fn check_output(&self, output: &ModelOutput) -> Result<(), ModelError> {
        self.check_tool_names(output.content.iter().filter_map(|content| match content {
            Content::ToolUse { name, .. } => Some(name.as_str()),
            _ => None,
        }))?;
        if output
            .content
            .iter()
            .any(|content| matches!(content, Content::ToolResult { .. }))
        {
            return Err(ModelError::ResponseRejected(
                ModelResponseRejection::UnexpectedToolOperation,
            ));
        }
        Ok(())
    }

    pub fn decode_error(&self, error: ModelError) -> ModelError {
        if !self.saw_expected_tool && (self.tool_starts > 0 || !self.chat_tool_names.is_empty()) {
            return ModelError::ResponseRejected(ModelResponseRejection::UnexpectedToolOperation);
        }
        if self.has_finished_response
            && self.saw_expected_tool
            && matches!(
                error,
                ModelError::ResponseRejected(ModelResponseRejection::InvalidToolCall)
            )
        {
            return ModelError::ResponseRejected(ModelResponseRejection::MalformedStructuredOutput);
        }
        error
    }
}

pub(super) fn verified_format_error(error: ModelError) -> ModelError {
    if matches!(error, ModelError::InvalidResponse(_)) {
        return ModelError::ResponseRejected(ModelResponseRejection::MalformedStructuredOutput);
    }
    error
}
