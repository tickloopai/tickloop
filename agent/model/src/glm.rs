//! Zhipu GLM / Z.AI request policy on OpenAI Chat Completions: preserved thinking, the
//! official effort parameter, schema-in-prompt JSON mode and business error codes.

use serde_json::{Value, json};

use super::{ModelError, ModelResponseRejection, StructuredOutput};
use tickloop_agent_protocol::{ReasoningEffort, Role};

/// Applies the GLM-specific fields to an encoded Chat Completions body.
pub(super) fn shape_request(
    body: &mut Value,
    model: &str,
    effort: Option<ReasoningEffort>,
    structured_output: Option<StructuredOutput>,
) -> Result<(), ModelError> {
    // Preserved Thinking keeps the replayed reasoning_content in context, so the same-source
    // prefix stays identical between requests (official Thinking Mode guide).
    let thinking_type = if effort == Some(ReasoningEffort::Off) {
        "disabled"
    } else {
        "enabled"
    };
    body["thinking"] = json!({"type": thinking_type});
    // GLM-4.5-Air predates Preserved Thinking and streamed function arguments.
    let supports_preserved_thinking = model != "glm-4.5-air";
    if supports_preserved_thinking {
        body["thinking"]["clear_thinking"] = json!(false);
    }
    if let Some(effort) =
        effort.filter(|effort| !matches!(effort, ReasoningEffort::Off | ReasoningEffort::On))
    {
        body["reasoning_effort"] = json!(effort.label());
    }
    if supports_preserved_thinking && body.get("tools").is_some() {
        body["tool_stream"] = json!(true);
    }
    if let Some(format) = structured_output {
        if body.get("tools").is_some() {
            return Err(ModelError::Configuration(
                "GLM structured output cannot be combined with executable tools".into(),
            ));
        }
        let messages = body["messages"]
            .as_array_mut()
            .expect("Chat Completions messages are an array");
        let position = messages
            .iter()
            .take_while(|message| message["role"] == json!(Role::System))
            .count();
        messages.insert(
            position,
            json!({"role": "system", "content": format!(
                "Return the {} result as one JSON object that conforms to this JSON Schema. Emit no other text and call no tools.\n{}",
                format.name, format.schema
            )}),
        );
        body["response_format"] = json!({"type": "json_object"});
    }
    Ok(())
}

pub(super) const INSUFFICIENT_BALANCE_CODE: u16 = 1113;

pub(super) fn business_code(body: Option<&Value>) -> Option<u16> {
    match &body?.get("error")?["code"] {
        Value::String(code) => code.parse::<u16>().ok(),
        Value::Number(code) => u16::try_from(code.as_u64()?).ok(),
        _ => None,
    }
}

/// Classifies documented GLM business codes whose HTTP status alone would be retried or
/// reported as a generic invalid request.
pub(super) fn http_error(status: u16, body: Option<&Value>) -> Option<ModelError> {
    let code = business_code(body)?;
    let error = body?.get("error")?;
    // Hermes, OpenClaw and pi treat this 1210 text as a prompt over the context window.
    let reports_prompt_overflow = code == 1210
        && error["message"].as_str().is_some_and(|message| {
            message
                .to_ascii_lowercase()
                .contains("more than max tokens allowed")
        });
    match code {
        // The ordinary API also answers 1113 to GLM Coding Plan keys, whose quota only applies
        // to the plan's own endpoints (observed with a real plan key on both sites).
        INSUFFICIENT_BALANCE_CODE => Some(ModelError::QuotaExhausted {
            status,
            description: ": no pay-as-you-go balance or resource package (1113); GLM Coding Plan keys do not apply to this API".into(),
        }),
        1261 => Some(ModelError::ResponseRejected(
            ModelResponseRejection::ContextLimit,
        )),
        1210 if reports_prompt_overflow => Some(ModelError::ResponseRejected(
            ModelResponseRejection::ContextLimit,
        )),
        1301 => Some(ModelError::ResponseRejected(
            ModelResponseRejection::ContentFilter,
        )),
        1308 | 1310 | 1316..=1321 => Some(ModelError::QuotaExhausted {
            status,
            description: format!(": usage limit reached ({code})"),
        }),
        1309 | 1311 | 1313 | 1314 | 1315 => Some(ModelError::EntitlementDenied {
            status,
            description: format!(": plan or model access unavailable ({code})"),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_body(code: Value) -> Value {
        json!({"error": {"code": code, "message": "remote text is never surfaced"}})
    }

    #[test]
    fn business_codes_override_retryable_http_statuses() {
        let balance = http_error(429, Some(&error_body(json!("1113")))).unwrap();
        assert_eq!(
            balance.kind(),
            super::super::ModelFailureKind::QuotaExhausted
        );
        assert!(balance.to_string().contains("GLM Coding Plan keys"));
        assert!(!balance.retry_hint().0);
        for code in ["1308", "1310", "1316", "1321"] {
            let quota = http_error(429, Some(&error_body(json!(code)))).unwrap();
            assert_eq!(quota.kind(), super::super::ModelFailureKind::QuotaExhausted);
            assert!(!quota.retry_hint().0);
            assert!(!quota.to_string().contains("remote text"));
        }
        for code in [1309, 1311, 1313, 1314, 1315] {
            let denied = http_error(429, Some(&error_body(json!(code)))).unwrap();
            assert_eq!(
                denied.kind(),
                super::super::ModelFailureKind::EntitlementDenied
            );
        }
        assert!(matches!(
            http_error(400, Some(&error_body(json!("1261")))),
            Some(ModelError::ResponseRejected(
                ModelResponseRejection::ContextLimit
            ))
        ));
        assert!(matches!(
            http_error(400, Some(&error_body(json!(1301)))),
            Some(ModelError::ResponseRejected(
                ModelResponseRejection::ContentFilter
            ))
        ));
        let overflow = json!({"error": {"code": "1210", "message": "Tokens in request more than max tokens allowed"}});
        assert!(matches!(
            http_error(400, Some(&overflow)),
            Some(ModelError::ResponseRejected(
                ModelResponseRejection::ContextLimit
            ))
        ));
        assert!(http_error(400, Some(&error_body(json!("1210")))).is_none());
        for code in ["1302", "1305", "1234", "1214", "unknown"] {
            assert!(http_error(429, Some(&error_body(json!(code)))).is_none());
        }
        assert!(http_error(429, Some(&json!({"code": "1113"}))).is_none());
        assert!(http_error(429, None).is_none());
    }
}
