use tickloop_harness_test_support::model_http_support;

use serde_json::{Value, json};
use tickloop_harness_model::{
    LocalModelClients, ModelClients, ModelConfigExt, ModelFailureKind, ModelRequest,
    ModelRequestContext, StructuredOutput,
    access::AccessCatalog,
    cards::{ModelCatalog, built_in_model_profiles},
    credentials::save_api_key,
    validate_reasoning_effort,
};
use tickloop_harness_protocol::{
    Adapter, Content, ImageMediaType, ImageSource, InputModality, Message, ReasoningEffort, Role,
    TemperatureControl, ThinkingControl, ToolDefinition,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn request(effort: Option<ReasoningEffort>) -> ModelRequest {
    ModelRequest {
        deferred_tools: Vec::new(),
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
fn kimi_k31_pro_series_preserve_older_cards_code_and_oauth_defaults() {
    let catalog = ModelCatalog::defaults();
    let access = AccessCatalog::defaults();
    catalog.validate().unwrap();
    access.validate(&catalog).unwrap();
    let visible = built_in_model_profiles();
    for provider in ["kimi-cn-api", "kimi-global-api"] {
        let selection = access.provider(provider).unwrap();
        assert_eq!(selection.auxiliary_model.as_deref(), Some("kimi-k3"));
        assert_eq!(selection.series.len(), 3);
        assert_eq!(access.recommended_model(provider), Some("kimi-k3.1-pro"));
        for (model, series_id) in [
            ("kimi-k3.1-pro", "kimi"),
            ("kimi-k3.1-pro-highspeed", "kimi-highspeed"),
        ] {
            let config = catalog.get(provider, model).unwrap();
            assert_eq!(config.model, model);
            assert!(visible.contains(&config));
            assert_eq!(access.series_for(&config).unwrap().id, series_id);
            assert_eq!(config.maximum_context_window_tokens(), Some(1_048_576));
            assert_eq!(config.max_output_tokens, 131_072);
            let card = config.card.as_ref().unwrap();
            assert_eq!(card.adapter, Adapter::KimiChatCompletions);
            assert_eq!(card.thinking, ThinkingControl::AlwaysOn);
            assert_eq!(
                card.effort_levels,
                [
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max
                ]
            );
            assert_eq!(
                card.input_modalities,
                [InputModality::Text, InputModality::Image]
            );
            assert_eq!(
                card.temperature_control,
                Some(Some(TemperatureControl::Unsupported))
            );
            assert!(validate_reasoning_effort(&config, None).is_ok());
            for effort in [
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ] {
                assert!(validate_reasoning_effort(&config, Some(effort)).is_ok());
            }
            for effort in [
                ReasoningEffort::Off,
                ReasoningEffort::Medium,
                ReasoningEffort::Xhigh,
            ] {
                assert!(validate_reasoning_effort(&config, Some(effort)).is_err());
            }
        }
        let retained = catalog.get(provider, "kimi-k3").unwrap();
        assert_eq!(retained.model, "kimi-k3");
        assert_eq!(retained.maximum_context_window_tokens(), Some(1_048_576));
        assert_eq!(retained.max_output_tokens, 131_072);
        assert!(access.series_for(&retained).is_none());
        assert!(!visible.contains(&retained));
        let code = catalog.get(provider, "kimi-k2.7-code").unwrap();
        assert_eq!(access.series_for(&code).unwrap().id, "kimi-code");
        assert!(visible.contains(&code));
        assert_eq!(code.maximum_context_window_tokens(), Some(262_144));
        assert_eq!(code.max_output_tokens, 32_768);
    }
    assert_eq!(access.recommended_model("kimi-code-oauth"), Some("k3"));
    assert_eq!(
        access
            .provider("kimi-code-oauth")
            .unwrap()
            .auxiliary_model
            .as_deref(),
        Some("k3-256k")
    );
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
        "alibaba-cn-token-plan",
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
        let client = LocalModelClients::new(Some(root.path().to_path_buf()))
            .client(&model)
            .unwrap();
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
                        chat.into(),
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
    let client = LocalModelClients::new(Some(root.path().to_path_buf()))
        .client(&config)
        .unwrap();
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
async fn kimi_k31_pro_variants_preserve_wire_efforts_images_cache_and_tool_reasoning() {
    for provider in ["kimi-cn-api", "kimi-global-api"] {
        for model in ["kimi-k3.1-pro", "kimi-k3.1-pro-highspeed"] {
            for effort in [
                None,
                Some(ReasoningEffort::Low),
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::Max),
            ] {
                let root =
                    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
                let responses = vec![
                    (
                        200,
                        chat_reply(
                            json!({"reasoning_content":"private reasoning","tool_calls":[{"index":0,"id":"echo-1","type":"function","function":{"name":"Echo","arguments":"{}"}}]}),
                            "tool_calls",
                        ),
                    ),
                    (200, chat_reply(json!({"content":"done"}), "stop")),
                ];
                let (endpoint, mut captured) = model_http_support::serve_responses(responses).await;
                let mut config = ModelCatalog::defaults().get(provider, model).unwrap();
                config.base_url = endpoint;
                save_api_key(root.path(), &config.api_key_env, "test-kimi-key").unwrap();
                let client = LocalModelClients::new(Some(root.path().to_path_buf()))
                    .client(&config)
                    .unwrap();
                let mut next = request(effort);
                next.messages[0].content.push(Content::Image {
                    source: ImageSource::Base64 {
                        media_type: ImageMediaType::Png,
                        data: "image".into(),
                    },
                    width: 1,
                    height: 1,
                });
                next.tools.push(ToolDefinition {
                    name: "Echo".into(),
                    description: "Inert fixture".into(),
                    input_schema: json!({"type":"object","properties":{}}),
                });
                let (events, _) = mpsc::unbounded_channel();
                let first = client
                    .complete(next.clone(), events.clone(), CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(first.stop_reason, "tool_use");
                assert_eq!(first.usage.cache_read_input_tokens, Some(50));
                assert_eq!(first.usage.cache_write_input_tokens, Some(10));
                assert!(first.content.iter().any(
                    |content| matches!(content, Content::ToolUse { id, name, input } if id == "echo-1" && name == "Echo" && input == &json!({}))
                ));
                next.messages.push(Message {
                    role: Role::Assistant,
                    content: first.content,
                });
                next.history_sources.push(first.private_source);
                next.messages.push(Message {
                    role: Role::User,
                    content: vec![Content::ToolResult {
                        tool_use_id: "echo-1".into(),
                        content: "done".into(),
                        is_error: false,
                    }],
                });
                next.history_sources.push(None);
                let output = client
                    .complete(next, events, CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(
                    output.content,
                    vec![Content::Text {
                        text: "done".into()
                    }]
                );
                let (headers, first) = captured.recv().await.unwrap();
                let (_, second) = captured.recv().await.unwrap();
                assert!(headers.starts_with("POST /anthropic/chat/completions "));
                assert!(
                    headers
                        .to_lowercase()
                        .contains("authorization: bearer test-kimi-key")
                );
                for body in [&first, &second] {
                    assert_eq!(body["model"], model);
                    assert_eq!(body["max_completion_tokens"], 131_072);
                    for field in ["max_tokens", "temperature", "thinking"] {
                        assert!(
                            body.get(field).is_none(),
                            "{provider}/{model}: {field}: {body}"
                        );
                    }
                    assert_eq!(
                        body.get("reasoning_effort"),
                        effort.map(|effort| json!(effort.label())).as_ref()
                    );
                    assert_eq!(body["tools"][0]["function"]["name"], "Echo");
                    assert_eq!(
                        body["messages"][0]["content"][1],
                        json!({"type":"image_url","image_url":{"url":"data:image/png;base64,image"}})
                    );
                }
                assert!(
                    first["prompt_cache_key"]
                        .as_str()
                        .is_some_and(|key| !key.is_empty())
                );
                assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
                assert_eq!(
                    second["messages"][1]["reasoning_content"],
                    "private reasoning"
                );
                assert_eq!(second["messages"][1]["tool_calls"][0]["id"], "echo-1");
                assert_eq!(second["messages"][2]["role"], "tool");
                assert_eq!(second["messages"][2]["tool_call_id"], "echo-1");
                assert_eq!(second["messages"][2]["content"], "done");
            }
        }
    }
}

#[tokio::test]
async fn kimi_k31_pro_variants_encode_structured_output_without_sampling() {
    for provider in ["kimi-cn-api", "kimi-global-api"] {
        for model in ["kimi-k3.1-pro", "kimi-k3.1-pro-highspeed"] {
            let root = tempfile::tempdir().unwrap();
            let (endpoint, captured) = model_http_support::serve_response(
                200,
                chat_reply(json!({"content":"{\"allow\":false}"}), "stop"),
            )
            .await;
            let mut config = ModelCatalog::defaults().get(provider, model).unwrap();
            config.base_url = endpoint;
            save_api_key(root.path(), &config.api_key_env, "test-kimi-key").unwrap();
            let mut input = request(Some(ReasoningEffort::Low));
            input.structured_output = Some(StructuredOutput {
                name: "verdict".into(),
                schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
            });
            let (events, _) = mpsc::unbounded_channel();
            LocalModelClients::new(Some(root.path().to_path_buf()))
                .client(&config)
                .unwrap()
                .complete(input, events, CancellationToken::new())
                .await
                .unwrap();
            let (_, body) = captured.await.unwrap();
            assert_eq!(body["model"], model);
            assert_eq!(body["max_completion_tokens"], 131_072);
            assert_eq!(body["response_format"]["type"], "json_schema");
            assert_eq!(body["response_format"]["json_schema"]["strict"], true);
            assert!(body.get("temperature").is_none());
            assert!(body.get("max_tokens").is_none());
        }
    }
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
        LocalModelClients::new(Some(root.path().to_path_buf()))
            .client(&config)
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
        let result = LocalModelClients::new(Some(root.path().to_path_buf()))
            .client(&config)
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

#[tokio::test]
async fn token_plan_models_keep_their_model_studio_thinking_and_json_contracts() {
    for (model, effort, replays_reasoning) in [
        ("qwen3.8-max", Some(ReasoningEffort::Xhigh), true),
        ("deepseek-v4-pro", Some(ReasoningEffort::Max), true),
        ("deepseek-v4.1-flash", Some(ReasoningEffort::Off), true),
        ("glm-5.3", Some(ReasoningEffort::Low), false),
    ] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let responses = vec![
            (
                200,
                chat_reply(
                    json!({"reasoning_content":"original thought","tool_calls":[{"index":0,"id":"echo-1","type":"function","function":{"name":"Echo","arguments":"{}"}}]}),
                    "tool_calls",
                ),
            ),
            (
                200,
                chat_reply(json!({"content":"{\"allow\":false}"}), "stop"),
            ),
        ];
        let (endpoint, mut captured) = model_http_support::serve_responses(responses).await;
        let mut config = ModelCatalog::defaults()
            .get("alibaba-cn-token-plan", model)
            .unwrap();
        assert_eq!(
            config.base_url,
            "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"
        );
        assert_eq!(config.api_key_env, "ALIBABA_TOKEN_PLAN_API_KEY");
        config.base_url = endpoint;
        save_api_key(root.path(), &config.api_key_env, "sk-sp-fixture").unwrap();
        let client = LocalModelClients::new(Some(root.path().to_path_buf()))
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let first = client
            .complete(request(effort), events.clone(), CancellationToken::new())
            .await
            .unwrap();
        let mut next = request(effort);
        next.messages.push(Message {
            role: Role::Assistant,
            content: first.content,
        });
        next.history_sources.push(first.private_source);
        next.messages.push(Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "echo-1".into(),
                content: "done".into(),
                is_error: false,
            }],
        });
        next.history_sources.push(None);
        next.structured_output = Some(StructuredOutput {
            name: "verdict".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"]}),
        });
        client
            .complete(next, events, CancellationToken::new())
            .await
            .unwrap();
        let (headers, first) = captured.recv().await.unwrap();
        let (_, second) = captured.recv().await.unwrap();
        assert!(
            headers
                .to_lowercase()
                .contains("authorization: bearer sk-sp-fixture")
        );
        assert_eq!(
            first["enable_thinking"],
            effort != Some(ReasoningEffort::Off),
            "{model}"
        );
        match effort {
            Some(ReasoningEffort::Off) => assert!(first.get("reasoning_effort").is_none()),
            effort => assert_eq!(
                first["reasoning_effort"],
                effort.unwrap().label(),
                "{model}"
            ),
        }
        assert_eq!(first["max_completion_tokens"], config.max_output_tokens);
        assert!(first.get("max_tokens").is_none());
        assert_eq!(
            first.get("preserve_thinking").is_some(),
            model.starts_with("qwen"),
            "{model}"
        );
        let replayed = second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert_eq!(
            replayed.get("reasoning_content").is_some(),
            replays_reasoning,
            "{model}"
        );
        assert_eq!(
            second.get("response_format").is_some(),
            model.starts_with("qwen"),
            "{model}"
        );
        if !model.starts_with("qwen") {
            assert!(
                second["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("JSON Schema")
            );
        }
    }
}

#[test]
fn token_plan_recommends_the_latest_qwen_flagship_by_default() {
    assert_eq!(
        tickloop_harness_model::access::AccessCatalog::defaults()
            .recommended_model("alibaba-cn-token-plan"),
        Some("qwen3.8-max")
    );
}

#[test]
fn token_plan_glm_thinking_cannot_be_turned_off() {
    let config = ModelCatalog::defaults()
        .get("alibaba-cn-token-plan", "glm-5.3")
        .unwrap();
    for effort in [
        ReasoningEffort::Off,
        ReasoningEffort::On,
        ReasoningEffort::Xhigh,
    ] {
        assert!(
            tickloop_harness_model::validate_reasoning_effort(&config, Some(effort)).is_err(),
            "{effort:?}"
        );
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
        config.auth_source = tickloop_harness_protocol::ModelAuthSource::ApiKeyEnv;
        config.api_key_env = "TEST_API_KEY".into();
        assert!(
            LocalModelClients::new(None).client(&config).is_err(),
            "{provider}"
        );
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
    let output = LocalModelClients::new(Some(root.path().to_path_buf()))
        .client(&config)
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
        let error = LocalModelClients::new(Some(root.path().to_path_buf()))
            .client(&config)
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
            == tickloop_harness_protocol::ProviderFailureClass::UsageOrPlan
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
        .get("xai-oauth", "grok-4.7")
        .unwrap();
    config.base_url = endpoint;
    let client = LocalModelClients::new(Some(root.path().to_path_buf()))
        .client(&config)
        .unwrap();
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
        assert!(headers.contains("x-grok-model-override: grok-4.7"));
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
    LocalModelClients::new(Some(root.path().to_path_buf()))
        .client(&config)
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
