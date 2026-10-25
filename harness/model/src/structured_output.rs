use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

use super::{ModelError, ModelOutput, ModelResponseRejection, StructuredOutput};
use tickloop_harness_protocol::Content;

impl StructuredOutput {
    pub(crate) fn prompt(&self) -> String {
        format!(
            "Return the {} result as exactly one JSON value conforming to this JSON Schema. Use the exact field names and enum values. Emit no prose, Markdown fences, or tool calls.\n{}",
            self.name, self.schema
        )
    }

    pub(crate) fn validator(&self) -> Result<jsonschema::Validator, ModelError> {
        if self.name.trim().is_empty() {
            return Err(ModelError::Configuration(
                "structured output needs a name".into(),
            ));
        }
        jsonschema::validator_for(&self.schema)
            .map_err(|_| ModelError::Configuration("invalid structured output schema".into()))
    }

    /// Validate a completed, tool-free result. Providers and custom ModelClient adapters
    /// use the same validation; business rules remain the caller's responsibility.
    pub fn validate_output(&self, output: ModelOutput) -> Result<ModelOutput, ModelError> {
        validate_output(output, &self.validator()?)
    }
}

pub(super) fn validate_output(
    mut output: ModelOutput,
    validator: &jsonschema::Validator,
) -> Result<ModelOutput, ModelError> {
    if output
        .content
        .iter()
        .any(|block| matches!(block, Content::ToolUse { .. } | Content::ToolResult { .. }))
    {
        return Err(ModelError::ResponseRejected(
            ModelResponseRejection::UnexpectedToolOperation,
        ));
    }
    let invalid =
        || ModelError::ResponseRejected(ModelResponseRejection::MalformedStructuredOutput);
    if !matches!(output.stop_reason.as_str(), "end_turn" | "stop") {
        return Err(invalid());
    }
    for block in &output.content {
        match block {
            Content::Text { .. } | Content::Thinking { .. } => {}
            Content::ProtocolData {
                protocol: tickloop_harness_protocol::Protocol::Responses,
                value,
            } if value["type"] == "reasoning"
                || value["type"] == super::responses::ASSISTANT_PHASE_TYPE => {}
            Content::ProtocolData {
                protocol: tickloop_harness_protocol::Protocol::Google,
                value,
            } if value["part"].get("functionCall").is_none()
                && value["part"].get("functionResponse").is_none()
                && (value["part"]["text"].is_string()
                    || value["part"]["thoughtSignature"].is_string()) => {}
            _ => return Err(invalid()),
        }
    }
    let text = output
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let mut text = text.trim();
    for (opening, closing) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        if let Some(body) = text.strip_prefix(opening) {
            let (thinking, rest) = body.split_once(closing).ok_or_else(invalid)?;
            if thinking.contains(opening) {
                return Err(invalid());
            }
            text = rest.trim();
            break;
        }
    }
    if let Some(body) = text.strip_prefix("```") {
        let (language, body) = body.split_once('\n').ok_or_else(invalid)?;
        if !language.trim().is_empty() && !language.trim().eq_ignore_ascii_case("json") {
            return Err(invalid());
        }
        text = body
            .trim_end()
            .strip_suffix("```")
            .ok_or_else(invalid)?
            .trim();
    }
    let value: UniqueValue = serde_json::from_str(text).map_err(|_| invalid())?;
    if !validator.is_valid(&value.0) {
        return Err(invalid());
    }
    output.content = vec![Content::Text {
        text: value.0.to_string(),
    }];
    Ok(output)
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = UniqueValue;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("JSON with unique object keys")
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueValue(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut source: A) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some(key) = source.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                    let UniqueValue(value) = source.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_phase_metadata_does_not_invalidate_a_json_result() {
        let format = StructuredOutput {
            name: "result".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
        };
        let output = ModelOutput {
            private_source: None,
            content: vec![
                Content::ProtocolData {
                    protocol: tickloop_harness_protocol::Protocol::Responses,
                    value: json!({"type": super::super::responses::ASSISTANT_PHASE_TYPE, "phase":"final_answer"}),
                },
                Content::Text {
                    text: "{\"allow\":true}".into(),
                },
            ],
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        };
        assert_eq!(
            format.validate_output(output).unwrap().content,
            vec![Content::Text {
                text: "{\"allow\":true}".into()
            }]
        );
    }

    #[test]
    fn json_contract_validates_shape_without_hiding_duplicate_keys_or_extra_content() {
        let format = StructuredOutput {
            name: "result".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
        };
        for (text, valid) in [
            ("{\"allow\":true}", true),
            ("```json\n{\"allow\":false}\n```", true),
            ("<think>Review</think>\n{\"allow\":true}", true),
            ("{\"allow\":true,\"allow\":false}", false),
            ("{\"allow\":\"true\"}", false),
            ("{\"allow\":true,\"extra\":1}", false),
            ("{\"allow\":true} {\"allow\":false}", false),
            ("Here: {\"allow\":true}", false),
            ("{}", false),
        ] {
            let output = ModelOutput {
                private_source: None,
                content: vec![Content::Text { text: text.into() }],
                usage: Default::default(),
                stop_reason: "end_turn".into(),
            };
            assert_eq!(format.validate_output(output).is_ok(), valid, "{text}");
        }
        assert!(serde_json::from_str::<UniqueValue>(r#"{"nested":{"x":1,"x":2}}"#).is_err());
    }
}
