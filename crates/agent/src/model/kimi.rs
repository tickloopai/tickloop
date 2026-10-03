//! Moonshot Kimi error classification on OpenAI Chat Completions.

use serde_json::Value;

use super::ModelError;

const ACCESS_TERMINATED: &str = "access_terminated_error";
const USAGE_LIMIT_PHRASE: &str = "usage limit";

/// Kimi Code reports an exhausted plan allowance as HTTP 403 `access_terminated_error`, the same
/// type it uses for other access denials, so only the message distinguishes them (observed with
/// a real Kimi Code OAuth account on 2026-10-07).
pub(super) fn http_error(status: u16, body: Option<&Value>) -> Option<ModelError> {
    let error = body?.get("error")?;
    let reports_usage_limit = error["type"].as_str() == Some(ACCESS_TERMINATED)
        && error["message"]
            .as_str()
            .is_some_and(|message| message.to_ascii_lowercase().contains(USAGE_LIMIT_PHRASE));
    reports_usage_limit.then(|| ModelError::QuotaExhausted {
        status,
        description: ": Kimi Code plan usage limit reached; wait for the next billing cycle or add quota at kimi.com/membership".into(),
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
