use std::time::Duration;

use serde_json::Value;

use super::{AuthError, ModelError, ModelRequestStage};

pub(super) fn auth_error(error: &AuthError) -> ModelError {
    match error {
        AuthError::Cancelled => ModelError::Cancelled,
        AuthError::RefreshBudgetExhausted => ModelError::RequestBudgetExhausted {
            stage: ModelRequestStage::Credentials,
        },
        AuthError::NotLoggedIn => ModelError::CodexNotLoggedIn,
        AuthError::Storage(operation) => {
            ModelError::Configuration(format!("Codex credential storage failed: {operation}"))
        }
        AuthError::InvalidCredentialFile => ModelError::Configuration(
            "Codex credential file is invalid or unsafe; use /model → Provider after checking its permissions"
                .into(),
        ),
        AuthError::Network { .. } => ModelError::ConnectionAt {
            stage: ModelRequestStage::Credentials,
        },
        AuthError::NetworkTimeout { .. } => ModelError::TimedOutAt {
            stage: ModelRequestStage::Credentials,
        },
        AuthError::TimedOut => ModelError::TimedOutAt {
            stage: ModelRequestStage::Credentials,
        },
        AuthError::HttpStatus { status, .. } if matches!(status, 408 | 429 | 500..=599) => {
            ModelError::HttpResponse {
                status: *status,
                description: ": Codex authentication service is temporarily unavailable".into(),
                retry_after: None,
                should_retry: Some(true),
            }
            .at_stage(ModelRequestStage::Credentials)
        }
        _ => ModelError::CodexAuthentication,
    }
}

pub(super) fn http_error(
    status: u16,
    body: Option<&Value>,
    retry_after: Option<Duration>,
    server_retry: Option<bool>,
) -> ModelError {
    let error = body.and_then(|body| {
        body.get("error")
            .filter(|error| error.is_object())
            .or_else(|| body.is_object().then_some(body))
    });
    let code = error.and_then(|error| {
        ["code", "type"]
            .into_iter()
            .filter_map(|key| error.get(key).and_then(Value::as_str))
            .find_map(classify_code)
    });

    if status == 401 || matches!(code, Some(ErrorCode::Authentication)) {
        return ModelError::CodexAuthentication;
    }

    let (description, should_retry) = match code {
        Some(ErrorCode::Entitlement) => (": Codex is not included in this subscription", false),
        Some(ErrorCode::Quota) => (": Codex subscription usage limit reached", false),
        Some(ErrorCode::RateLimit) => (": Codex request rate limited", true),
        Some(ErrorCode::Authentication) => unreachable!(),
        None => match status {
            403 => (": Codex access forbidden; check account permissions", false),
            429 => (": Codex request rate limited", true),
            408 | 409 | 500..=599 => (": Codex service temporarily unavailable", true),
            _ => (": Codex request rejected", false),
        },
    };
    let should_retry = should_retry && server_retry != Some(false);
    let reset_at = if matches!(code, Some(ErrorCode::Quota)) {
        error.and_then(|error| error.get("resets_at"))
    } else {
        None
    };
    let description = match reset_at.and_then(format_reset_at) {
        Some(reset_at) => format!("{description}; resets at {reset_at}"),
        None => description.into(),
    };
    match code {
        Some(ErrorCode::Entitlement) => ModelError::EntitlementDenied {
            status,
            description,
        },
        Some(ErrorCode::Quota) => ModelError::QuotaExhausted {
            status,
            description,
        },
        _ if status == 403 => ModelError::EntitlementDenied {
            status,
            description,
        },
        _ => ModelError::HttpResponse {
            status,
            description,
            retry_after: if should_retry { retry_after } else { None },
            should_retry: Some(should_retry),
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorCode {
    Authentication,
    Entitlement,
    Quota,
    RateLimit,
}

fn classify_code(code: &str) -> Option<ErrorCode> {
    match code {
        "authentication_error" | "invalid_token" | "invalid_api_key" => {
            Some(ErrorCode::Authentication)
        }
        "usage_not_included" => Some(ErrorCode::Entitlement),
        "usage_limit_reached"
        | "insufficient_quota"
        | "quota_exceeded"
        | "GoUsageLimitError"
        | "FreeUsageLimitError" => Some(ErrorCode::Quota),
        "rate_limit_exceeded" | "rate_limit_error" => Some(ErrorCode::RateLimit),
        _ => None,
    }
}

pub(super) fn inferred_status_for_code(code: &str) -> Option<u16> {
    match classify_code(code)? {
        ErrorCode::Authentication => Some(401),
        ErrorCode::Entitlement => Some(403),
        ErrorCode::Quota | ErrorCode::RateLimit => Some(429),
    }
}

fn format_reset_at(value: &Value) -> Option<String> {
    let seconds = value.as_i64()?;
    if seconds < 0 {
        return None;
    }
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_auth_storage_and_refresh_without_exposing_details() {
        assert!(matches!(
            auth_error(&AuthError::NotLoggedIn),
            ModelError::CodexNotLoggedIn
        ));
        assert!(matches!(
            auth_error(&AuthError::RefreshFailed),
            ModelError::CodexAuthentication
        ));
        assert!(matches!(
            auth_error(&AuthError::Network {
                operation: "refresh",
                reason: "connection failed",
            }),
            ModelError::ConnectionAt {
                stage: ModelRequestStage::Credentials
            }
        ));
        assert!(matches!(
            auth_error(&AuthError::NetworkTimeout {
                operation: "refresh"
            }),
            ModelError::TimedOutAt {
                stage: ModelRequestStage::Credentials
            }
        ));
        assert!(matches!(
            auth_error(&AuthError::RefreshBudgetExhausted),
            ModelError::RequestBudgetExhausted {
                stage: ModelRequestStage::Credentials
            }
        ));
        let storage = auth_error(&AuthError::Storage("lock timed out"));
        assert!(matches!(storage, ModelError::Configuration(_)));
        assert!(storage.to_string().contains("lock timed out"));
    }

    #[test]
    fn quota_and_entitlement_are_terminal_with_only_real_reset_time() {
        let quota = http_error(
            429,
            Some(
                &json!({"error": {"code": "usage_limit_reached", "resets_at": 0, "message": "bearer-secret"}}),
            ),
            Some(Duration::from_secs(2)),
            None,
        );
        let ModelError::QuotaExhausted { description, .. } = quota else {
            panic!("expected quota failure")
        };
        assert!(description.contains("1970-01-01T00:00:00Z"));
        assert!(!description.contains("bearer-secret"));

        let entitlement = http_error(
            403,
            Some(&json!({"error": {"type": "usage_not_included"}})),
            None,
            None,
        );
        assert!(entitlement.to_string().contains("not included"));
        assert!(!entitlement.to_string().contains("resets at"));
    }

    #[test]
    fn unknown_429_is_retryable_and_ignores_untrusted_body() {
        let result = http_error(
            429,
            Some(
                &json!({"error": {"code": "bearer-secret", "message": "token-secret", "resets_at": 123}}),
            ),
            Some(Duration::from_secs(7)),
            None,
        );
        let ModelError::HttpResponse {
            description,
            should_retry,
            retry_after,
            ..
        } = result
        else {
            panic!("expected HTTP response")
        };
        assert_eq!(should_retry, Some(true));
        assert_eq!(retry_after, Some(Duration::from_secs(7)));
        assert_eq!(description, ": Codex request rate limited");
    }

    #[test]
    fn auth_and_transient_service_errors_are_distinct() {
        assert!(matches!(
            http_error(401, None, None, None),
            ModelError::CodexAuthentication
        ));
        let forbidden = http_error(403, None, None, None);
        assert!(forbidden.to_string().contains("access forbidden"));
        let unavailable = http_error(503, None, None, None);
        assert!(matches!(
            unavailable,
            ModelError::HttpResponse {
                should_retry: Some(true),
                ..
            }
        ));

        let server_disabled_retry =
            http_error(503, None, Some(Duration::from_secs(30)), Some(false));
        assert!(matches!(
            server_disabled_retry,
            ModelError::HttpResponse {
                should_retry: Some(false),
                retry_after: None,
                ..
            }
        ));
        let terminal_quota = http_error(
            429,
            Some(&json!({"error": {"code": "usage_limit_reached"}})),
            None,
            Some(true),
        );
        assert!(matches!(terminal_quota, ModelError::QuotaExhausted { .. }));
    }
}
