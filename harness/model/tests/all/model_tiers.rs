use serde_json::{Value, json};
use tickloop_harness_model::{
    LocalModelClients, ModelClients, ModelRequest, StructuredOutput, access::AccessCatalog,
    cards::ModelCatalog, credentials::save_api_key, validate_reasoning_effort,
};
use tickloop_harness_protocol::{Content, Message, ReasoningEffort, Role, ToolDefinition};
use tickloop_harness_test_support::model_http_support;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn request(effort: Option<ReasoningEffort>) -> ModelRequest {
    ModelRequest {
        deferred_tools: Vec::new(),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "fixture".into(),
            }],
        }],
        history_sources: vec![None],
        tools: vec![],
        structured_output: None,
        temperature: Some(0.0),
        reasoning_effort: effort,
    }
}

fn reply(delta: Value, reason: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    )
}

#[test]
fn missing_tiers_are_selectable_without_rebinding_existing_defaults_or_plan_models() {
    let catalog = ModelCatalog::defaults();
    let access = AccessCatalog::defaults();
    catalog.validate().unwrap();
    access.validate(&catalog).unwrap();
    for provider in [
        "google-gemini-api",
        "google-gemini-oauth",
        "google-vertex-oauth",
        "google-vertex-service-account",
    ] {
        let config = catalog.get(provider, "gemini-4-argon").unwrap();
        assert!(access.series_for(&config).is_some());
        assert_eq!(access.recommended_model(provider), Some("gemini-3.8-flash"));
        assert_eq!(
            config.card.as_ref().unwrap().context_window_tokens,
            Some(1048576)
        );
        assert_eq!(config.max_output_tokens, 65536);
        assert!(validate_reasoning_effort(&config, Some(ReasoningEffort::Off)).is_err());
    }
    for provider in ["zhipu-cn-api", "zai-global-api"] {
        assert_eq!(access.recommended_model(provider), Some("glm-5.3"));
        for (model, context, output) in [
            ("glm-4.5-air", 131072, 98304),
            ("glm-4.7-flash", 200000, 131072),
        ] {
            let config = catalog.get(provider, model).unwrap();
            assert!(access.series_for(&config).is_some());
            assert_eq!(
                config.card.as_ref().unwrap().context_window_tokens,
                Some(context)
            );
            assert_eq!(config.max_output_tokens, output);
            assert!(validate_reasoning_effort(&config, Some(ReasoningEffort::On)).is_ok());
            assert!(validate_reasoning_effort(&config, Some(ReasoningEffort::Off)).is_ok());
            assert!(validate_reasoning_effort(&config, Some(ReasoningEffort::High)).is_err());
        }
    }
    for provider in ["zhipu-cn-coding", "zai-global-coding"] {
        assert!(catalog.get(provider, "glm-4.5-air").is_none());
        assert!(catalog.get(provider, "glm-4.7-flash").is_none());
    }
    for provider in ["kimi-cn-api", "kimi-global-api"] {
        let config = catalog.get(provider, "kimi-k2.7-code").unwrap();
        assert!(access.series_for(&config).is_some());
        assert_eq!(access.recommended_model(provider), Some("kimi-k3.1-pro"));
        assert_eq!(
            config.card.as_ref().unwrap().context_window_tokens,
            Some(262144)
        );
        assert_eq!(config.max_output_tokens, 32768);
        assert!(validate_reasoning_effort(&config, Some(ReasoningEffort::Low)).is_err());
        assert!(validate_reasoning_effort(&config, Some(ReasoningEffort::Off)).is_err());
    }
}

#[tokio::test]
async fn glm_lightweight_tools_continue_with_supported_thinking_fields_and_json_mode() {
    for provider in ["zhipu-cn-api", "zai-global-api"] {
        for model in ["glm-4.5-air", "glm-4.7-flash"] {
            let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let responses = vec![
                (
                    200,
                    reply(
                        json!({"reasoning_content":"original thought", "tool_calls":[{"index":0,"id":"echo-1","type":"function","function":{"name":"Echo","arguments":"{\"text\":\"hello\"}"}}]}),
                        "tool_calls",
                    ),
                ),
                (200, reply(json!({"content":"hello"}), "stop")),
                (200, reply(json!({"content":"{\"allow\":false}"}), "stop")),
            ];
            let (endpoint, mut captured) = model_http_support::serve_responses(responses).await;
            let mut config = ModelCatalog::defaults().get(provider, model).unwrap();
            config.base_url = endpoint;
            save_api_key(root.path(), &config.api_key_env, "fixture-key").unwrap();
            let client = LocalModelClients::new(Some(root.path().to_path_buf()))
                .client(&config)
                .unwrap();
            let (events, _) = mpsc::unbounded_channel();
            let mut first = request(Some(ReasoningEffort::On));
            first.tools.push(ToolDefinition { name: "Echo".into(), description: "Inert fixture".into(), input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}) });
            let output = client
                .complete(first.clone(), events.clone(), CancellationToken::new())
                .await
                .unwrap();
            let (_, body) = captured.recv().await.unwrap();
            assert_eq!(body["model"], model);
            assert_eq!(body["thinking"]["type"], "enabled");
            assert!(body.get("reasoning_effort").is_none());
            assert_eq!(
                body.get("tool_stream"),
                (model == "glm-4.7-flash").then_some(&json!(true))
            );
            assert_eq!(
                body["thinking"].get("clear_thinking"),
                (model == "glm-4.7-flash").then_some(&json!(false))
            );
            first.messages.push(Message {
                role: Role::Assistant,
                content: output.content,
            });
            first.history_sources.push(output.private_source);
            first.messages.push(Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "echo-1".into(),
                    content: "hello".into(),
                    is_error: false,
                }],
            });
            first.history_sources.push(None);
            client
                .complete(first, events.clone(), CancellationToken::new())
                .await
                .unwrap();
            let (_, body) = captured.recv().await.unwrap();
            assert_eq!(body["messages"][1]["reasoning_content"], "original thought");
            assert_eq!(body["messages"][2]["tool_call_id"], "echo-1");
            let mut schema = request(Some(ReasoningEffort::Off));
            schema.structured_output = Some(StructuredOutput {
                name: "verdict".into(),
                schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
            });
            client
                .complete(schema, events, CancellationToken::new())
                .await
                .unwrap();
            let (_, body) = captured.recv().await.unwrap();
            assert_eq!(body["thinking"]["type"], "disabled");
            assert_eq!(body["response_format"]["type"], "json_object");
            assert!(body.get("tools").is_none());
            assert!(body.get("reasoning_effort").is_none());
        }
    }
}

#[tokio::test]
async fn kimi_coding_line_uses_its_token_budget_and_prompted_schema_without_sampling_or_effort() {
    for provider in ["kimi-cn-api", "kimi-global-api"] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let (endpoint, captured) = model_http_support::serve_response(
            200,
            reply(json!({"content":"{\"allow\":false}"}), "stop"),
        )
        .await;
        let mut config = ModelCatalog::defaults()
            .get(provider, "kimi-k2.7-code")
            .unwrap();
        config.base_url = endpoint;
        save_api_key(root.path(), &config.api_key_env, "fixture-key").unwrap();
        let client = LocalModelClients::new(Some(root.path().to_path_buf()))
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let mut schema = request(None);
        schema.structured_output = Some(StructuredOutput {
            name: "verdict".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
        });
        client
            .complete(schema, events, CancellationToken::new())
            .await
            .unwrap();
        let (_, body) = captured.await.unwrap();
        assert_eq!(body["model"], "kimi-k2.7-code");
        assert_eq!(body["max_tokens"], 32768);
        assert_eq!(body["thinking"]["type"], "enabled");
        for field in [
            "max_completion_tokens",
            "temperature",
            "reasoning_effort",
            "response_format",
            "tools",
        ] {
            assert!(body.get(field).is_none(), "{field}: {body}");
        }
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("allow")
        );
    }
}

#[tokio::test]
async fn argon_native_schema_requests_use_only_documented_levels_and_single_decode_budget() {
    for effort in [
        None,
        Some(ReasoningEffort::Low),
        Some(ReasoningEffort::Medium),
        Some(ReasoningEffort::High),
    ] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let response = format!(
            "data: {}\n\n",
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"{\"allow\":false}"}]},"finishReason":"STOP"}]})
        );
        let (endpoint, captured) = model_http_support::serve_response(200, response).await;
        let mut config = ModelCatalog::defaults()
            .get("google-gemini-api", "gemini-4-argon")
            .unwrap();
        config.base_url = endpoint;
        save_api_key(root.path(), &config.api_key_env, "fixture-key").unwrap();
        let client = LocalModelClients::new(Some(root.path().to_path_buf()))
            .client(&config)
            .unwrap();
        let mut schema = request(effort);
        schema.structured_output = Some(StructuredOutput {
            name: "verdict".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
        });
        let (events, _) = mpsc::unbounded_channel();
        client
            .complete(schema, events, CancellationToken::new())
            .await
            .unwrap();
        let (headers, body) = captured.await.unwrap();
        assert!(headers.contains("/models/gemini-4-argon:streamGenerateContent?alt=sse"));
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 65536);
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert!(body["generationConfig"].get("temperature").is_none());
        assert!(
            body["generationConfig"]["thinkingConfig"]
                .get("thinkingBudget")
                .is_none()
        );
        if let Some(effort) = effort {
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                effort.label()
            );
        }
    }
}
