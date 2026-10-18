use tickloop_agent_test_support::model_http_support;

use serde_json::{Value, json};
use tickloop_agent_model::{
    ModelConfigExt, ModelFailureKind, ModelRequest, ModelRequestContext, StructuredOutput,
    cards::{ModelCatalog, built_in_model_profiles},
    credentials::save_api_key,
    model_client,
};
use tickloop_agent_protocol::{
    Content, ImageMediaType, ImageSource, Message, ReasoningEffort, Role,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn request(effort: Option<ReasoningEffort>) -> ModelRequest {
    ModelRequest {
        history_sources: vec![None],
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "fixture".into(),
            }],
        }],
        tools: vec![],
        structured_output: None,
        temperature: Some(0.0),
        reasoning_effort: effort,
    }
}
fn chat_reply(delta: Value, reason: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":reason}],"usage":{"prompt_tokens":100,"completion_tokens":8,"prompt_tokens_details":{"cached_tokens":50,"cache_write_tokens":10}}})
    )
}

#[test]
fn kimi_code_k3_context_variants_are_distinct_and_selectable() {
    let catalog = ModelCatalog::defaults();
    let visible = built_in_model_profiles();
    for (model_id, window) in [("k3", 1_048_576), ("k3-256k", 262_144)] {
        let model = catalog.get("kimi-code-oauth", model_id).unwrap();
        assert_eq!(model.model, model_id);
        assert_eq!(model.maximum_context_window_tokens(), Some(window));
        assert!(visible.contains(&model));
    }
}

#[test]
fn kimi_for_coding_has_a_verified_context_window() {
    let model = ModelCatalog::defaults()
        .get("kimi-code-oauth", "kimi-for-coding")
        .unwrap();
    assert_eq!(model.maximum_context_window_tokens(), Some(1_048_576));
}

#[test]
fn new_catalogs_are_visible_and_keep_retired_anthropic_cards_resolvable() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    let visible = built_in_model_profiles();
    for provider in [
        "kimi-cn-api",
        "kimi-global-api",
        "kimi-code-oauth",
        "alibaba-cn-api",
        "anthropic-api",
        "xai-api",
        "xai-oauth",
    ] {
        assert!(
            visible.iter().any(|config| config.provider_id == provider),
            "{provider}"
        );
    }
    assert!(catalog.get("anthropic-api", "claude-opus-4-6").is_some());
    assert!(
        !visible.iter().any(
            |config| config.provider_id == "anthropic-api" && config.model == "claude-opus-4-6"
        )
    );
    assert_eq!(
        visible
            .iter()
            .filter(|config| config.provider_id == "anthropic-api")
            .count(),
        4
    );
}

#[tokio::test]
async fn request_output_budget_reaches_every_http_encoder_without_changing_the_next_request() {
    for (provider, model_id, field) in [
        ("anthropic-api", "claude-haiku-4-5-20251001", "max_tokens"),
        ("openai-api", "gpt-6.1-sol", "max_output_tokens"),
        ("kimi-global-api", "kimi-k3", "max_completion_tokens"),
        ("deepseek-api", "deepseek-flash", "max_tokens"),
        ("google-gemini-api", "gemini-3.8-flash", "maxOutputTokens"),
    ] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let refusal = json!({"error":{"message":"fixture refusal"}}).to_string();
        let (endpoint, mut captured) =
            model_http_support::serve_responses(vec![(400, refusal.clone()), (400, refusal)]).await;
        let mut model = ModelCatalog::defaults().get(provider, model_id).unwrap();
        let original_limit = model.max_output_tokens;
        model.base_url = endpoint;
        save_api_key(root.path(), &model.api_key_env, "fixture-budget-key").unwrap();
        let client = model_client(model, Some(root.path())).unwrap();
        let chat = uuid::Uuid::new_v4();
        for limit in [Some(4096), None] {
            let context = ModelRequestContext::new(1);
            let context = limit.map_or_else(
                || context.clone(),
                |limit| context.clone().with_output_token_limit(limit),
            );
            let (events, _) = mpsc::unbounded_channel();
            assert!(
                client
                    .complete_for_chat_with_context(
                        chat,
                        request(Some(ReasoningEffort::High)),
                        events,
                        CancellationToken::new(),
                        context
                    )
                    .await
                    .is_err()
            );
            let (_, body) = captured.recv().await.unwrap();
            let encoded = if field == "maxOutputTokens" {
                &body["generationConfig"][field]
            } else {
                &body[field]
            };
            assert_eq!(
                encoded,
                &json!(limit.unwrap_or(original_limit)),
                "{provider}: {body}"
            );
            if provider == "anthropic-api" && limit.is_some() {
                assert_eq!(body["thinking"]["budget_tokens"], 3072);
            }
        }
    }
}

#[tokio::test]
async fn kimi_uses_completion_budget_preserved_thought_and_stable_cache_key() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let reply = chat_reply(
        json!({"reasoning_content":"private reasoning","content":"done"}),
        "stop",
    );
    let (endpoint, mut captured) =
        model_http_support::serve_responses(vec![(200, reply.clone()), (200, reply)]).await;
    let mut config = ModelCatalog::defaults()
        .get("kimi-global-api", "kimi-k3")
        .unwrap();
    config.base_url = endpoint;
    save_api_key(root.path(), &config.api_key_env, "test-kimi-key").unwrap();
    let client = model_client(config, Some(root.path())).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    let first = client
        .complete(
            request(Some(ReasoningEffort::Low)),
            events.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut next = request(Some(ReasoningEffort::Low));
    next.messages.push(Message {
        role: Role::Assistant,
        content: first.content,
    });
    next.history_sources.push(first.private_source);
    client
        .complete(next, events, CancellationToken::new())
        .await
        .unwrap();
    let first = captured.recv().await.unwrap();
    let second = captured.recv().await.unwrap();
    assert!(
        first
            .0
            .to_lowercase()
            .contains("authorization: bearer test-kimi-key")
    );
    assert_eq!(first.1["max_completion_tokens"], 131072);
    assert!(first.1.get("max_tokens").is_none());
    assert!(first.1.get("temperature").is_none());
    assert!(first.1.get("thinking").is_none());
    assert_eq!(first.1["reasoning_effort"], "low");
    assert_eq!(first.1["prompt_cache_key"], second.1["prompt_cache_key"]);
    assert_eq!(
        second.1["messages"][1]["reasoning_content"],
        "private reasoning"
    );
}

#[tokio::test]
async fn qwen_thinking_parameters_and_text_schema_follow_its_contract() {
    for (model, effort) in [
        ("qwen3.7-plus-2026-05-26", Some(ReasoningEffort::Off)),
        ("qwen3.7-plus-2026-05-26", Some(ReasoningEffort::On)),
        ("qwen3.8-flash", Some(ReasoningEffort::Xhigh)),
    ] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let (endpoint, captured) = model_http_support::serve_response(
            200,
            chat_reply(json!({"content":"{\"allow\":false}"}), "stop"),
        )
        .await;
        let mut config = ModelCatalog::defaults()
            .get("alibaba-cn-api", model)
            .unwrap();
        config.base_url = endpoint;
        save_api_key(root.path(), &config.api_key_env, "test-qwen-key").unwrap();
        let mut request = request(effort);
        request.structured_output = Some(StructuredOutput {
            name: "fixture".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"]}),
        });
        let (events, _) = mpsc::unbounded_channel();
        model_client(config, Some(root.path()))
            .unwrap()
            .complete(request, events, CancellationToken::new())
            .await
            .unwrap();
        let body = captured.await.unwrap().1;
        assert_eq!(
            body["enable_thinking"],
            effort != Some(ReasoningEffort::Off)
        );
        assert_eq!(body["preserve_thinking"], true);
        assert!(body.get("thinking").is_none());
        if effort == Some(ReasoningEffort::On) {
            assert!(body.get("reasoning_effort").is_none());
        }
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["response_format"]["type"], "json_schema");
    }
}

#[tokio::test]
async fn qwen_image_schema_uses_prompted_json_and_rejects_tool_calls() {
    for extra in [false, true] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let delta = if extra {
            json!({"content":"{\"allow\":false}","tool_calls":[{"index":0,"id":"unsafe","type":"function","function":{"name":"Bash","arguments":"{}"}}]})
        } else {
            json!({"content":"{\"allow\":false}"})
        };
        let (endpoint, captured) = model_http_support::serve_response(
            200,
            chat_reply(delta, if extra { "tool_calls" } else { "stop" }),
        )
        .await;
        let mut config = ModelCatalog::defaults()
            .get("alibaba-cn-api", "qwen3.8-flash")
            .unwrap();
        config.base_url = endpoint;
        save_api_key(root.path(), &config.api_key_env, "test-qwen-key").unwrap();
        let mut request = request(Some(ReasoningEffort::Low));
        request.messages[0].content.push(Content::Image {
            source: ImageSource::Base64 {
                media_type: ImageMediaType::Png,
                data: "image".into(),
            },
            width: 1,
            height: 1,
        });
        request.structured_output = Some(StructuredOutput {
            name: "verdict".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"]}),
        });
        let (events, _) = mpsc::unbounded_channel();
        let result = model_client(config, Some(root.path()))
            .unwrap()
            .complete(request, events, CancellationToken::new())
            .await;
        assert_eq!(result.is_err(), extra);
        let body = captured.await.unwrap().1;
        assert!(body.get("response_format").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body["messages"].to_string().contains("JSON Schema"));
        if !extra {
            assert_eq!(
                result.unwrap().content,
                vec![Content::Text {
                    text: "{\"allow\":false}".into()
                }]
            );
        }
    }
}

fn saved_subscription(root: &std::path::Path, provider: &str, kind: &str) {
    use std::os::unix::fs::PermissionsExt;
    let directory = root.join("auth");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join(format!("{provider}.json"));
    std::fs::write(&path,json!({"provider":kind,"generation":uuid::Uuid::new_v4(),"credentials":{"token":"fxm72vq84ep61aw9k","refresh_token":"fixture-refresh","identity":uuid::Uuid::new_v4(),"expires_at_ms":u64::MAX}}).to_string()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn registered_oauth_providers_cannot_be_overridden_to_api_key_auth() {
    for provider in [
        "kimi-code-oauth",
        "xai-oauth",
        "google-gemini-oauth",
        "google-vertex-oauth",
        "google-vertex-service-account",
    ] {
        let catalog = ModelCatalog::defaults();
        let mut config = catalog
            .profiles()
            .into_iter()
            .find(|config| config.provider_id == provider)
            .unwrap();
        config.auth_source = tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv;
        config.api_key_env = "TEST_API_KEY".into();
        assert!(model_client(config, None).is_err(), "{provider}");
    }
}

#[tokio::test]
async fn kimi_code_oauth_schema_uses_prompted_json_on_its_own_route() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    saved_subscription(root.path(), "kimi-code-oauth", "kimi");
    let (endpoint, captured) = model_http_support::serve_response(
        200,
        chat_reply(json!({"content":"{\"allow\":false}"}), "stop"),
    )
    .await;
    let mut config = ModelCatalog::defaults()
        .get("kimi-code-oauth", "k3")
        .unwrap();
    config.base_url = endpoint;
    let mut request = request(Some(ReasoningEffort::Low));
    request.structured_output = Some(StructuredOutput {
        name: "fixture".into(),
        schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"]}),
    });
    let (events, _) = mpsc::unbounded_channel();
    let output = model_client(config, Some(root.path()))
        .unwrap()
        .complete(request, events, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        output
            .content
            .iter()
            .any(|block| matches!(block,Content::Text{text} if text=="{\"allow\":false}"))
    );
    let (headers, body) = captured.await.unwrap();
    assert!(
        headers
            .to_lowercase()
            .contains("authorization: bearer fxm72vq84ep61aw9k")
    );
    assert!(headers.starts_with("POST /anthropic/chat/completions "));
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("response_format").is_none());
    assert!(body.get("tools").is_none());
    assert!(body["messages"].to_string().contains("JSON Schema"));
}

#[tokio::test]
async fn kimi_code_403_preserves_safe_provider_text_and_ambiguous_plan_guidance() {
    let quota_body = json!({"error":{"type":"access_terminated_error","message":"You've reached your monthly usage limit for this billing cycle. Your quota will be refreshed in the next cycle. To continue now, purchase extra usage or upgrade your plan: https://www.kimi.com/membership/subscription?tab=quota"}});
    let denied_body =
        json!({"error":{"type":"access_terminated_error","message":"remote-secret-diagnostic"}});
    for (body, expected_kind, expected_text) in [
        (
            quota_body,
            ModelFailureKind::EntitlementDenied,
            "Usage limit OR the current plan does not include this model",
        ),
        (
            denied_body,
            ModelFailureKind::EntitlementDenied,
            "Model / plan access denied",
        ),
    ] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        saved_subscription(root.path(), "kimi-code-oauth", "kimi");
        let (endpoint, _) = model_http_support::serve_response(403, body.to_string()).await;
        let mut config = ModelCatalog::defaults()
            .get("kimi-code-oauth", "k3")
            .unwrap();
        config.base_url = endpoint;
        let (events, _) = mpsc::unbounded_channel();
        let error = model_client(config, Some(root.path()))
            .unwrap()
            .complete(request(None), events, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), expected_kind);
        assert!(!error.retry_hint().0);
        let message = error.to_string();
        assert!(
            message.contains("Kimi Code") && message.contains("HTTP 403"),
            "{message}"
        );
        assert!(message.contains(expected_text), "{message}");
        if error.provider_failure().unwrap().class
            == tickloop_agent_protocol::ProviderFailureClass::UsageOrPlan
        {
            assert!(message.contains("purchase extra usage"));
            assert!(message.contains("https://www.kimi.com/membership/subscription?tab=quota"));
            assert!(message.contains("Check K3 plan access"));
            assert!(!message.contains("Try kimi-for-coding"));
        }
        assert!(!message.contains("remote-secret-diagnostic"), "{message}");
    }
}

#[tokio::test]
async fn xai_oauth_uses_responses_proxy_headers_and_preserves_encrypted_replay() {
    fn frame(kind: &str, mut value: Value) -> String {
        value["type"] = json!(kind);
        format!("event: {kind}\ndata: {value}\n\n")
    }
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    saved_subscription(root.path(), "xai-oauth", "xai");
    let reasoning =
        json!({"type":"reasoning","id":"r1","summary":[],"encrypted_content":"opaque-xai-fixture"});
    let message = json!({"type":"message","id":"m1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]});
    let reply=[frame("response.created",json!({"response":{"id":"response-1","status":"in_progress"}})),frame("response.output_item.added",json!({"output_index":0,"item":{"type":"reasoning","id":"r1","summary":[]}})),frame("response.output_item.done",json!({"output_index":0,"item":reasoning.clone()})),frame("response.output_item.added",json!({"output_index":1,"item":{"type":"message","id":"m1","role":"assistant","content":[]}})),frame("response.output_text.delta",json!({"output_index":1,"item_id":"m1","content_index":0,"delta":"done"})),frame("response.output_item.done",json!({"output_index":1,"item":message.clone()})),frame("response.completed",json!({"response":{"id":"response-1","status":"completed","output":[reasoning,message]}}))].concat();
    let (endpoint, mut captured) =
        model_http_support::serve_responses(vec![(200, reply.clone()), (200, reply)]).await;
    let mut config = ModelCatalog::defaults()
        .get("xai-oauth", "grok-4.6")
        .unwrap();
    config.base_url = endpoint;
    let client = model_client(config, Some(root.path())).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    let first = client
        .complete(
            request(Some(ReasoningEffort::Low)),
            events.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut next = request(Some(ReasoningEffort::Low));
    next.messages.push(Message {
        role: Role::Assistant,
        content: first.content,
    });
    next.history_sources.push(first.private_source);
    client
        .complete(next, events, CancellationToken::new())
        .await
        .unwrap();
    let (headers, first) = captured.recv().await.unwrap();
    let (second_headers, second) = captured.recv().await.unwrap();
    for headers in [headers, second_headers] {
        assert!(headers.starts_with("POST /anthropic/responses "));
        let headers = headers.to_lowercase();
        assert!(headers.contains("x-xai-token-auth: xai-grok-cli"));
        assert!(headers.contains("x-grok-client-version: 1.0.62"));
        assert!(headers.contains("user-agent: xai-grok-workspace/1.0.62"));
        assert!(headers.contains("x-grok-model-override: grok-4.6"));
        assert!(headers.contains("authorization: bearer fxm72vq84ep61aw9k"));
    }
    assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
    assert!(second.to_string().contains("opaque-xai-fixture"));
}

#[tokio::test]
async fn xai_api_does_not_send_grok_subscription_headers() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let (endpoint, captured) =
        model_http_support::serve_response(400, json!({"error":"fixture refusal"}).to_string())
            .await;
    let mut config = ModelCatalog::defaults().get("xai-api", "grok-4.7").unwrap();
    config.base_url = endpoint;
    save_api_key(root.path(), &config.api_key_env, "fixture-xai-api-key").unwrap();
    let (events, _) = mpsc::unbounded_channel();
    model_client(config, Some(root.path()))
        .unwrap()
        .complete(request(None), events, CancellationToken::new())
        .await
        .unwrap_err();
    let (headers, _) = captured.await.unwrap();
    let headers = headers.to_lowercase();
    assert!(headers.starts_with("post /anthropic/responses "));
    assert!(headers.contains("authorization: bearer fixture-xai-api-key"));
    assert!(!headers.contains("x-xai-token-auth:"));
    assert!(!headers.contains("x-grok-client-version:"));
    assert!(!headers.contains("x-grok-model-override:"));
    assert!(!headers.contains("xai-grok-workspace/"));
}
