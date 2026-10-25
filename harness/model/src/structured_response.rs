use serde_json::Value;

use super::{ModelError, ModelOutput, ModelRequest, ModelResponseRejection};
use tickloop_harness_protocol::{Content, Protocol};

#[derive(Default)]
pub(super) struct StructuredResponsePolicy {
    validator: Option<jsonschema::Validator>,
}

impl StructuredResponsePolicy {
    pub fn for_request(request: &ModelRequest) -> Result<Option<Self>, ModelError> {
        let Some(format) = &request.structured_output else {
            return Ok(None);
        };
        if !request.tools.is_empty() {
            return Err(ModelError::Configuration(
                "structured output cannot be combined with executable tools".into(),
            ));
        }
        Ok(Some(Self {
            validator: Some(format.validator()?),
        }))
    }

    pub fn for_codex_body(body: &Value) -> Option<Self> {
        (body["parallel_tool_calls"] == false && body["tools"].as_array().is_none_or(Vec::is_empty))
            .then(Self::default)
    }

    pub fn check_event(&mut self, protocol: Protocol, data: &Value) -> Result<(), ModelError> {
        let forbidden = match protocol {
            Protocol::Messages => data
                .get("content_block")
                .into_iter()
                .chain(data["message"]["content"].as_array().into_iter().flatten())
                .any(|block| block["type"].as_str().is_some_and(is_tool_type)),
            Protocol::ChatCompletions => {
                data["choices"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|choice| {
                        matches!(
                            choice["finish_reason"].as_str(),
                            Some("function_call" | "tool_calls")
                        ) || [&choice["delta"], &choice["message"]]
                            .into_iter()
                            .any(|payload| {
                                payload
                                    .get("function_call")
                                    .is_some_and(|call| !call.is_null())
                                    || payload.get("tool_calls").is_some_and(|calls| {
                                        !calls.is_null()
                                            && calls
                                                .as_array()
                                                .is_none_or(|calls| !calls.is_empty())
                                    })
                            })
                    })
            }
            Protocol::Responses => {
                let family = data["type"]
                    .as_str()
                    .and_then(|kind| kind.strip_prefix("response."))
                    .and_then(|kind| kind.split('.').next())
                    .unwrap_or_default();
                is_tool_type(family)
                    || data
                        .get("item")
                        .into_iter()
                        .chain(data["response"]["output"].as_array().into_iter().flatten())
                        .any(|item| {
                            item["type"].as_str().is_some_and(is_tool_type)
                                || item["content"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .any(|part| part["type"].as_str().is_some_and(is_tool_type))
                        })
            }
            Protocol::Google => {
                data["candidates"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|candidate| {
                        candidate["content"]["parts"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|part| {
                                part.get("functionCall").is_some()
                                    || part.get("functionResponse").is_some()
                            })
                    })
            }
            Protocol::SystemOne => false,
        };
        if forbidden {
            return Err(unexpected_tool());
        }
        Ok(())
    }

    pub fn check_tool_names<'a>(
        &self,
        mut names: impl Iterator<Item = &'a str>,
    ) -> Result<(), ModelError> {
        if names.next().is_some() {
            return Err(unexpected_tool());
        }
        Ok(())
    }

    pub fn check_output(&self, output: &ModelOutput) -> Result<(), ModelError> {
        if output.content.iter().any(|content| {
            matches!(
                content,
                Content::ToolUse { .. } | Content::ToolResult { .. }
            )
        }) {
            return Err(unexpected_tool());
        }
        Ok(())
    }

    pub fn validate_output(&self, output: ModelOutput) -> Result<ModelOutput, ModelError> {
        match &self.validator {
            Some(validator) => super::structured_output::validate_output(output, validator),
            None => Ok(output),
        }
    }
}

fn is_tool_type(kind: &str) -> bool {
    kind.contains("_call")
        || kind.starts_with("mcp_")
        || kind.ends_with("tool_use")
        || kind.ends_with("tool_result")
}

fn unexpected_tool() -> ModelError {
    ModelError::ResponseRejected(ModelResponseRejection::UnexpectedToolOperation)
}
