use crate::support;
use crate::support::provider_transport;
use tickloop_agent_test_support::model_http_support;

use tickloop_agent_model::{HttpModelClient, ModelClient, ModelRequest, model_client};
use tickloop_agent_protocol::{Protocol, ProviderFailureClass};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const KIMI_ERROR: &str = r#"{"error":{"message":"You've reached your monthly usage limit for this billing cycle. Your quota will be refreshed in the next cycle. To continue now, purchase extra usage or upgrade your plan: https://www.kimi.com/membership/subscription?tab=quota","type":"access_terminated_error"}}"#;

async fn failure(protocol: Protocol, status: u16, body: &str) -> tickloop_agent_model::ModelError {
    let (base_url, _) = model_http_support::serve_response(status, body.into()).await;
    let mut config = support::config();
    config.protocol = protocol;
    config.base_url = base_url;
    config.provider_id = "fixture-api".into();
    config.provider_name = "Kimi Code subscription".into();
    let client = HttpModelClient::new(config).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            ModelRequest {
                history_sources: vec![],
                messages: vec![],
                tools: vec![],
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap_err()
}

#[tokio::test]
async fn kimi_monthly_limit_explains_the_account_problem() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let auth = root_path.join("auth");
    std::fs::create_dir(&auth).unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = auth.join("kimi-code-oauth.json");
    std::fs::write(&path, serde_json::json!({"provider":"kimi", "generation":uuid::Uuid::new_v4(), "credentials":{"token":"fxA91q7Z83mP46r2T", "refresh_token":"fixture-refresh", "identity":uuid::Uuid::new_v4(), "expires_at_ms":u64::MAX}}).to_string()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    for selected_model in ["k3", "kimi-for-coding"] {
        let (url, _) = model_http_support::serve_response(403, KIMI_ERROR.into()).await;
        let mut config = tickloop_agent_model::cards::ModelCatalog::defaults()
            .get("kimi-code-oauth", selected_model)
            .unwrap();
        config.base_url = url;
        let client = model_client(config, Some(&root_path)).unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(
                ModelRequest {
                    history_sources: vec![],
                    messages: vec![],
                    tools: vec![],
                    structured_output: None,
                    temperature: None,
                    reasoning_effort: None,
                },
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        eprintln!("Kimi fixture observed: {error}");
        assert!(
            error
                .to_string()
                .contains("You've reached your monthly usage limit"),
            "{error}"
        );
        let text = error.to_string();
        assert!(
            text.contains("Kimi Code subscription") && text.contains("HTTP 403"),
            "{text}"
        );
        assert!(
            text.contains("You've reached your monthly usage limit"),
            "{text}"
        );
        assert!(text.contains("https://www.kimi.com/membership/subscription?tab=quota"));
        assert!(text.contains("/provider"));
        assert!(text.contains("Usage limit OR") && text.contains("kimi-for-coding"));
        assert_eq!(
            error.provider_failure().unwrap().class,
            ProviderFailureClass::UsageOrPlan
        );
        assert!(!error.retry_hint().0);
    }
}

#[tokio::test]
async fn compatible_shapes_explain_each_failure_class_and_control_retries() {
    for protocol in [
        Protocol::ChatCompletions,
        Protocol::Messages,
        Protocol::Responses,
        Protocol::Google,
    ] {
        for (status, code, message, class, retryable, hint) in [
            (
                429,
                "insufficient_quota",
                "Monthly usage limit reached",
                ProviderFailureClass::UsageLimit,
                false,
                "Usage limit",
            ),
            (
                402,
                "billing_error",
                "Insufficient balance",
                ProviderFailureClass::Billing,
                false,
                "Billing",
            ),
            (
                401,
                "authentication_error",
                "Your credential has expired",
                ProviderFailureClass::Authentication,
                false,
                "expired or invalid",
            ),
            (
                429,
                "rate_limit_error",
                "Too many requests",
                ProviderFailureClass::RateLimit,
                true,
                "Rate limit",
            ),
            (
                403,
                "permission_error",
                "This model is not included in your plan",
                ProviderFailureClass::ModelUnavailable,
                false,
                "plan access",
            ),
            (
                503,
                "api_error",
                "Service is temporarily unavailable",
                ProviderFailureClass::ServerError,
                true,
                "server error",
            ),
        ] {
            for anthropic in [false, true] {
                let error_body =
                    serde_json::json!({"error":{"code":code,"type":code,"message":message}});
                let body = if anthropic {
                    serde_json::json!({"type":"error","error":error_body["error"]})
                } else {
                    error_body
                };
                let error = failure(protocol, status, &body.to_string()).await;
                let detail = error.provider_failure().unwrap();
                assert_eq!(detail.class, class, "{protocol:?}: {error}");
                assert_eq!(error.retry_hint().0, retryable, "{protocol:?}: {error}");
                assert_eq!(error.status(), Some(status));
                assert!(
                    error.to_string().contains(message) && error.to_string().contains(hint),
                    "{error}"
                );
            }
        }
    }
}

#[tokio::test]
async fn non_json_errors_are_bounded_and_sanitized() {
    let body = format!(
        "<html>Gateway unavailable\x1b[31m Authorization: Bearer fixture-secret sk-fake-secret api_key=fixture-key https://support.example/help?access_token=private&tab=quota {}</html>",
        "界".repeat(2000)
    );
    let error = failure(Protocol::Messages, 503, &body).await;
    let text = error.to_string();
    assert!(text.contains("Gateway unavailable"), "{text}");
    for secret in [
        "fixture-secret",
        "sk-fake-secret",
        "fixture-key",
        "private",
        "\x1b",
        "<html>",
    ] {
        assert!(!text.contains(secret), "{secret}: {text}");
    }
    assert!(error.provider_failure().unwrap().message.chars().count() <= 1024);
    let empty = failure(Protocol::ChatCompletions, 502, "").await;
    assert!(empty.to_string().contains("empty error body"));
}

#[tokio::test]
async fn rate_limit_preserves_retry_after_and_billing_ignores_server_retry_true() {
    for (status, body, expected_retry) in [
        (
            429,
            r#"{"error":{"type":"rate_limit_error","message":"Slow down"}}"#,
            true,
        ),
        (
            429,
            r#"{"error":{"code":"insufficient_quota","message":"Monthly usage limit reached"}}"#,
            false,
        ),
        (
            402,
            r#"{"error":{"type":"billing_error","message":"Add balance"}}"#,
            false,
        ),
    ] {
        let (url, _) = model_http_support::serve_response_with_headers(
            status,
            body.into(),
            &[("retry-after", "7"), ("x-should-retry", "true")],
        )
        .await;
        let mut config = support::config();
        config.base_url = url;
        let client = HttpModelClient::new(config).unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.retry_hint().0, expected_retry, "{error}");
        if expected_retry {
            assert_eq!(
                error.retry_hint().1,
                Some(std::time::Duration::from_secs(7))
            );
            assert!(error.to_string().contains("Retry after 7 seconds"));
        }
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        history_sources: vec![],
        messages: vec![],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

#[test]
fn kimi_code_recommends_the_model_available_on_every_plan() {
    let catalog = tickloop_agent_model::cards::ModelCatalog::defaults();
    assert!(catalog.get("kimi-code-oauth", "kimi-for-coding").is_some());
    assert_eq!(
        tickloop_agent_model::access::AccessCatalog::defaults()
            .recommended_model("kimi-code-oauth"),
        Some("kimi-for-coding")
    );
}

#[tokio::test]
async fn quota_is_not_retried_and_success_clears_only_its_connection_failure() {
    use std::sync::Arc;
    use tickloop_agent::runtime::{Agent, EventKind};
    let (url,mut requests) = model_http_support::serve_responses(vec![
        (429,serde_json::json!({"error":{"code":"insufficient_quota","message":"Monthly usage limit reached"}}).to_string()),
        (200,"data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Recovered\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()),
        (200,"data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Recovered\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()),
    ]).await;
    let mut config = support::config();
    config.base_url = url;
    config.protocol = Protocol::ChatCompletions;
    config.provider_id = "fixture-api".into();
    config.provider_name = "Fixture API".into();
    let client = Arc::new(HttpModelClient::new(config.clone()).unwrap());
    let agent = Agent::new_embedded(
        config.clone(),
        provider_transport::without_title_requests(client),
    );
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "First attempt".into())
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        match event.kind {
            EventKind::Failed(text) => {
                assert!(
                    text.contains("Fixture API · HTTP 429")
                        && text.contains("Monthly usage limit reached")
                );
                break;
            }
            EventKind::ModelRetry { .. } => panic!("quota must not be automatically retried"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.provider_failures["fixture-api"].class,
        ProviderFailureClass::UsageLimit
    );
    assert_eq!(
        snapshot.messages.len(),
        1,
        "provider failures must not become assistant messages"
    );
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(
        serialized.contains("Monthly usage limit reached"),
        "snapshot/transcript error event must retain the sanitized explanation"
    );
    requests.recv().await.unwrap();
    assert!(
        requests.try_recv().is_err(),
        "no blind replay on the failed quota request"
    );
    let mut other = config.clone();
    other.provider_id = "other-fixture".into();
    other.provider_name = "Other Fixture API".into();
    agent
        .set_model(
            chat,
            other.clone(),
            provider_transport::without_title_requests(Arc::new(
                HttpModelClient::new(other).unwrap(),
            )),
        )
        .await
        .unwrap();
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .provider_failures
            .contains_key("fixture-api")
    );
    agent
        .submit(chat, None, "Second explicit attempt".into())
        .await
        .unwrap();
    loop {
        match support::next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(text) => panic!("{text}"),
            _ => {}
        }
    }
    let (_, request) = requests.recv().await.unwrap();
    assert!(
        !request.to_string().contains("Monthly usage limit reached"),
        "transport failures must not be inserted into model context"
    );
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .provider_failures
            .contains_key("fixture-api"),
        "success on another connection must not clear this diagnostic"
    );
    agent
        .set_model(
            chat,
            config.clone(),
            provider_transport::without_title_requests(Arc::new(
                HttpModelClient::new(config).unwrap(),
            )),
        )
        .await
        .unwrap();
    agent
        .submit(chat, None, "Third explicit attempt".into())
        .await
        .unwrap();
    loop {
        match support::next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(text) => panic!("{text}"),
            _ => {}
        }
    }
    requests.recv().await.unwrap();
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .provider_failures
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn persisted_transcript_retains_provider_explanation_after_reload() {
    use std::sync::Arc;
    use tickloop_agent::runtime::{Agent, EventKind};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let (url, _) = model_http_support::serve_response(403, KIMI_ERROR.into()).await;
    let mut config = support::config();
    config.protocol = Protocol::ChatCompletions;
    config.base_url = url;
    config.provider_id = "fixture-api".into();
    config.provider_name = "Fixture Provider".into();
    let agent = Agent::open_embedded(
        root.clone(),
        config.clone(),
        provider_transport::without_title_requests(Arc::new(
            HttpModelClient::new(config.clone()).unwrap(),
        )),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Transcript error probe".into())
        .await
        .unwrap();
    loop {
        if matches!(
            support::next_event(&mut events).await.kind,
            EventKind::Failed(_)
        ) {
            break;
        }
    }
    agent.shutdown().await;
    let file = support::transcript_file(&root, chat);
    let transcript = std::fs::read_to_string(file).unwrap();
    assert!(
        transcript.contains("You've reached your monthly usage limit"),
        "persisted transcript must retain the provider explanation"
    );
    let restored = Agent::open_embedded(
        root,
        config,
        Arc::new(support::ScriptedModel::new(vec![support::text(
            "Recovered",
        )])),
    )
    .unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert!(snapshot.events.iter().any(|event| matches!(&event.kind,EventKind::Failed(text) if text.contains("monthly usage limit"))),"reopened Transcript must display its past failure");
    assert!(
        !serde_json::to_string(&snapshot.messages)
            .unwrap()
            .contains("monthly usage limit"),
        "provider failures must not become model messages on recovery"
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn echoed_request_credentials_never_enter_display_or_debug_text() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let secret = "opaque-fixture-credential";
    tickloop_agent_model::credentials::save_api_key(
        &root_path,
        "PROVIDER_ERROR_FIXTURE_KEY",
        secret,
    )
    .unwrap();
    let body=serde_json::json!({"error":{"message":format!("Rejected {secret}; https://user:password@support.example/help?token=private&tab=quota&q=opaque%2Dfixture%2Dcredential; access_token=other-secret"),"type":"authentication_error"},"request":{"private_field":"do-not-display"}}).to_string();
    let (url, _) = model_http_support::serve_response(401, body).await;
    let mut config = support::config();
    config.base_url = url;
    config.api_key_env = "PROVIDER_ERROR_FIXTURE_KEY".into();
    let client = model_client(config, Some(&root_path)).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    let error = client
        .complete(request(), events, CancellationToken::new())
        .await
        .unwrap_err();
    let text = format!("{error}\n{error:?}");
    for private in [
        secret,
        "user:password",
        "token=private",
        "other-secret",
        "do-not-display",
    ] {
        assert!(!text.contains(private), "{text}");
    }
    assert!(
        text.contains("https://support.example/help?tab=quota"),
        "{text}"
    );
}

#[tokio::test]
async fn retry_after_http_date_is_preserved_and_hostile_delays_disable_automatic_retry() {
    for header in ["Wed, 21 Oct 2099 07:28:00 GMT", "18446744073709551615"] {
        let (url, _) = model_http_support::serve_response_with_headers(
            429,
            r#"{"error":{"message":"Slow down","type":"rate_limit_error"}}"#.into(),
            &[("retry-after", header)],
        )
        .await;
        let mut config = support::config();
        config.base_url = url;
        let client = HttpModelClient::new(config).unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.retry_hint().1.unwrap().as_secs() > 86400, "{error}");
        assert!(
            !error.retry_hint().0,
            "long provider delays must not be shortened or overflow the timer"
        );
        assert!(error.to_string().contains("Automatic retry is paused"));
    }
}

#[tokio::test]
async fn per_minute_quota_is_rate_limiting_while_account_quota_stays_terminal() {
    for (code, message, class, retryable) in [
        (
            "RESOURCE_EXHAUSTED",
            "Quota exceeded for requests per minute",
            ProviderFailureClass::RateLimit,
            true,
        ),
        (
            "rate_limit_error",
            "Quota exceeded for requests per second",
            ProviderFailureClass::RateLimit,
            true,
        ),
        (
            "insufficient_quota",
            "Account usage limit reached, rather than a rate limit",
            ProviderFailureClass::UsageLimit,
            false,
        ),
    ] {
        let error = failure(
            Protocol::Google,
            429,
            &serde_json::json!({"error":{"status":code,"message":message}}).to_string(),
        )
        .await;
        assert_eq!(error.provider_failure().unwrap().class, class, "{error}");
        assert_eq!(error.retry_hint().0, retryable, "{error}");
    }
}

#[tokio::test]
async fn provider_links_survive_markup_and_sentence_punctuation() {
    let link = "https://support.example/quota?tab=usage";
    for body in [
        serde_json::json!({"error":{"message":format!("Review ({link}).")}}).to_string(),
        serde_json::json!({"error":{"message":format!("Review <{link}>.")}}).to_string(),
        format!("<html><body>Please <a href=\"{link}\">review your allowance</a>.</body></html>"),
    ] {
        let error = failure(Protocol::Messages, 403, &body).await;
        let message = &error.provider_failure().unwrap().message;
        assert!(message.contains(link), "{message}");
        assert!(
            !message.contains("usage%29"),
            "punctuation must not become part of the URL: {message}"
        );
    }
}

#[tokio::test]
async fn normalization_and_url_decoding_cannot_restore_credentials() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let secret = "opaque-fixture-credential";
    tickloop_agent_model::credentials::save_api_key(
        &root_path,
        "PROVIDER_ERROR_FIXTURE_KEY",
        secret,
    )
    .unwrap();
    for message in [
        "Rejected opaque-\nfixture-credential",
        "Rejected opaque-\u{200b}fixture-credential",
        "Bea\u{200b}rer pattern-only-token",
        "See https://support.example/opaque%2Dfixture%2Dcredential?tab=quota",
        "See https://support.example/opaque-%0Afixture-credential?tab=quota",
        "See https://support.example/help?q=opaque-%E2%80%8Bfixture-credential&tab=quota",
        "<html>Rejected opaque-&#8203;fixture-credential</html>",
    ] {
        let (url, _) = model_http_support::serve_response(
            401,
            serde_json::json!({"error":{"message":message}}).to_string(),
        )
        .await;
        let mut config = support::config();
        config.base_url = url;
        config.api_key_env = "PROVIDER_ERROR_FIXTURE_KEY".into();
        let client = model_client(config, Some(&root_path)).unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        let displayed = format!("{error}\n{error:?}");
        assert!(
            !displayed.contains(secret),
            "normalized credentials must remain masked: {displayed}"
        );
        assert!(
            !displayed.contains("opaque%2Dfixture%2Dcredential"),
            "encoded paths must remain masked: {displayed}"
        );
        assert!(
            !displayed.contains("pattern-only-token"),
            "normalization must precede credential-pattern matching: {displayed}"
        );
        assert!(
            !displayed.contains("opaque-"),
            "decoded paths/query values must be normalized and masked: {displayed}"
        );
    }
}

#[tokio::test]
async fn glm_numeric_business_codes_do_not_apply_to_other_providers() {
    let error = failure(
        Protocol::ChatCompletions,
        500,
        r#"{"error":{"code":"1113","message":"Upstream request 1113 failed"}}"#,
    )
    .await;
    assert_eq!(
        error.provider_failure().unwrap().class,
        ProviderFailureClass::ServerError
    );
    assert!(error.retry_hint().0);
}
