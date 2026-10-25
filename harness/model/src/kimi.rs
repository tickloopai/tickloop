//! Moonshot Kimi error classification on OpenAI Chat Completions.

use serde_json::Value;

use super::ModelError;

const ACCESS_TERMINATED: &str = "access_terminated_error";
const USAGE_LIMIT_PHRASE: &str = "usage limit";
pub(super) const CODE_MODEL_ID: &str = "kimi-k2.7-code";

/// Kimi Code answers HTTP 403 `access_terminated_error` with a "monthly usage limit" message both
/// when the plan allowance is spent and when the plan does not include the requested model
/// (observed with a real Kimi Code OAuth account whose plan excludes K3, 2026-10-07). The same
/// type also covers other access denials, so only the message identifies the allowance case.
pub(super) fn http_error(status: u16, body: Option<&Value>) -> Option<ModelError> {
    let error = body?.get("error")?;
    let reports_usage_limit = error["type"].as_str() == Some(ACCESS_TERMINATED)
        && error["message"]
            .as_str()
            .is_some_and(|message| message.to_ascii_lowercase().contains(USAGE_LIMIT_PHRASE));
    reports_usage_limit.then(|| ModelError::QuotaExhausted {
        status,
        description: ": the Kimi Code plan has no allowance for this model; it may not include the model or its usage limit is reached (see kimi.com/membership)".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn error_body(kind: &str, message: &str) -> Value {
        json!({"error": {"type": kind, "message": message}})
    }

    #[test]
    fn only_usage_limit_terminations_are_quota_exhaustion() {
        let quota = http_error(
            403,
            Some(&error_body(
                ACCESS_TERMINATED,
                "You've reached your monthly Usage Limit for this billing cycle.",
            )),
        )
        .unwrap();
        assert_eq!(quota.kind(), super::super::ModelFailureKind::QuotaExhausted);
        assert!(!quota.retry_hint().0);
        assert!(!quota.to_string().contains("billing cycle."));
        assert!(
            http_error(
                403,
                Some(&error_body(ACCESS_TERMINATED, "only available for agents"))
            )
            .is_none()
        );
        assert!(
            http_error(
                429,
                Some(&error_body("rate_limit_reached_error", "usage limit"))
            )
            .is_none()
        );
        assert!(http_error(403, None).is_none());
    }
}
