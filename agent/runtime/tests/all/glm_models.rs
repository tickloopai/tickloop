use tickloop_agent_test_support::model_http_support;

use serde_json::{Value, json};
use tickloop_agent_model::{
    ModelConfigExt, ModelError, ModelFailureKind, ModelRequest, ModelResponseRejection,
    StructuredOutput,
    access::AccessCatalog,
    cards::{ModelCatalog, built_in_model_profiles, provider_group},
    credentials::save_api_key,
    model_client, validate_reasoning_effort,
};
use tickloop_agent_protocol::{
    Adapter, Content, EventKind, ImageMediaType, ImageSource, Message, ModelConfig, Protocol,
    ProviderPatch, ReasoningEffort, Role, ThinkingControl, ToolDefinition,
};
use tickloop_agent_runtime::Agent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const REGIONS: [(&str, &str, &str); 2] = [
    (
        "zhipu-cn-api",
        "https://open.bigmodel.cn/api/paas/v4",
        "ZHIPU_API_KEY",
    ),
    (
        "zai-global-api",
        "https://api.z.ai/api/paas/v4",
        "ZAI_API_KEY",
    ),
];

fn user_request(effort: Option<ReasoningEffort>) -> ModelRequest {
    ModelRequest {
        history_sources: vec![None],
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Hello".into(),
            }],
        }],
        tools: Vec::new(),
        structured_output: None,
        temperature: None,
        reasoning_effort: effort,
    }
}

fn chunk(delta: Value, finish_reason: Value, usage: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"glm-1","created":1,"model":"glm-5.3","choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}],"usage":usage})
    )
}

fn text_reply(reasoning: &str, text: &str) -> String {
    [
        chunk(json!({"role":"assistant","reasoning_content":reasoning}), Value::Null, Value::Null),
        chunk(json!({"content":text}), Value::Null, Value::Null),
        chunk(
            json!({"role":"assistant","content":""}),
            json!("stop"),
            json!({"prompt_tokens":2048,"completion_tokens":64,"total_tokens":2112,"prompt_tokens_details":{"cached_tokens":1536}}),
        ),
        "data: [DONE]\n\n".into(),
    ]
    .concat()
}

fn finish_reply(reason: &str) -> String {
    [
        chunk(json!({"content":"partial"}), Value::Null, Value::Null),
        chunk(json!({}), json!(reason), Value::Null),
        "data: [DONE]\n\n".into(),
    ]
    .concat()
}

fn configured(provider: &str, model: &str, root: &std::path::Path, url: String) -> ModelConfig {
    let mut config = ModelCatalog::defaults().get(provider, model).unwrap();
    save_api_key(root, &config.api_key_env, "test-glm-api-key").unwrap();
    config.base_url = url;
    config
}

#[test]
fn definition_catalog_keeps_concrete_cards_and_access_catalog_binds_one_per_series() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    let access = AccessCatalog::defaults();
    access.validate(&catalog).unwrap();
    let visible = built_in_model_profiles();
    for (provider, endpoint, key_env) in REGIONS {
        assert_eq!(provider_group(provider), Some("Zhipu GLM"));
        let cards = catalog
            .profiles()
            .into_iter()
            .filter(|profile| profile.provider_id == provider)
            .collect::<Vec<_>>();
        assert_eq!(
            cards
                .iter()
                .map(|card| (card.name.as_str(), card.model.as_str()))
                .collect::<Vec<_>>(),
            [
                ("GLM-5.3", "glm-5.3"),
                ("GLM-5.3-Flash", "glm-5.3-flash"),
                ("GLM-5.3-FlashX", "glm-5.3-flashx"),
                ("GLM-5.2", "glm-5.2"),
                ("GLM-4.5-Air", "glm-4.5-air"),
                ("GLM-4.7-Flash", "glm-4.7-flash"),
            ]
        );
        let series = cards
            .iter()
            .filter_map(|card| {
                access
                    .series_for(card)
                    .map(|series| (series.label(), card.name.clone()))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            series,
            [
                ("GLM (5.3)".to_string(), "GLM-5.3".to_string()),
                ("GLM Flash (5.3)".to_string(), "GLM-5.3-Flash".to_string()),
                ("GLM FlashX (5.3)".to_string(), "GLM-5.3-FlashX".to_string()),
                ("GLM Air (4.5)".to_string(), "GLM-4.5-Air".to_string()),
                (
                    "GLM Flash 30B (4.7)".to_string(),
                    "GLM-4.7-Flash".to_string()
                ),
            ]
        );
        for card in &cards {
            let bound = access.series_for(card).is_some();
            assert_eq!(visible.contains(card), bound, "{}", card.name);
            assert_eq!(access.ordinary_candidate(card, &catalog), Some(bound));
            assert_eq!(card.adapter(), Adapter::GlmChatCompletions);
            assert_eq!(card.protocol, Protocol::ChatCompletions);
            assert_eq!(card.base_url, endpoint);
            assert_eq!(card.api_key_env, key_env);
            if matches!(card.model.as_str(), "glm-4.5-air" | "glm-4.7-flash") {
                continue;
            }
            assert_eq!(card.max_output_tokens, 131072);
            let model_card = card.card.as_ref().unwrap();
            assert_eq!(model_card.context_window_tokens, Some(1_000_000));
            assert_eq!(
                card.accepts_images(),
                matches!(card.model.as_str(), "glm-5.3-flash" | "glm-5.3-flashx")
            );
            assert!(validate_reasoning_effort(card, Some(ReasoningEffort::On)).is_err());
            assert!(validate_reasoning_effort(card, Some(ReasoningEffort::Medium)).is_err());
            assert!(validate_reasoning_effort(card, Some(ReasoningEffort::Max)).is_ok());
            if card.model == "glm-5.2" {
                assert_eq!(model_card.thinking, ThinkingControl::Toggle);
                assert!(validate_reasoning_effort(card, Some(ReasoningEffort::Off)).is_ok());
                assert!(validate_reasoning_effort(card, Some(ReasoningEffort::Low)).is_err());
            } else {
                assert_eq!(model_card.thinking, ThinkingControl::AlwaysOn);
                assert_eq!(card.thinking_enabled(None, false), Some(true));
                assert!(validate_reasoning_effort(card, Some(ReasoningEffort::Off)).is_err());
                assert!(validate_reasoning_effort(card, Some(ReasoningEffort::Low)).is_ok());
            }
        }
        assert_eq!(access.recommended_model(provider), Some("glm-5.3"));
    }
}

#[test]
fn coding_plan_connections_are_separate_providers_with_plan_series_and_notice() {
    let catalog = ModelCatalog::defaults();
    let access = AccessCatalog::defaults();
    for (plan, ordinary, endpoint, key_env) in [
        (
            "zhipu-cn-coding",
            "zhipu-cn-api",
            "https://open.bigmodel.cn/api/coding/paas/v4",
            "ZHIPU_CODING_API_KEY",
        ),
        (
            "zai-global-coding",
            "zai-global-api",
            "https://api.z.ai/api/coding/paas/v4",
            "ZAI_CODING_API_KEY",
        ),
    ] {
        assert_eq!(provider_group(plan), Some("Zhipu GLM"));
        let binding = access.provider(plan).unwrap();
        assert!(binding.notice.as_deref().unwrap().contains("Coding Plan"));
        assert!(access.provider(ordinary).unwrap().notice.is_none());
        let labels = binding
            .series
            .iter()
            .map(|series| {
                let card = catalog.get(plan, &series.model).unwrap();
                assert_eq!(card.base_url, endpoint);
                assert_eq!(card.api_key_env, key_env);
                assert_eq!(card.adapter(), Adapter::GlmChatCompletions);
                (series.label(), card.name)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                ("GLM (5.3)".to_string(), "GLM-5.3".to_string()),
                ("GLM Flash (5.3)".to_string(), "GLM-5.3-Flash".to_string()),
            ]
        );
        assert!(
            catalog
                .get(plan, "glm-5.3-flashx-chat-completions")
                .is_none()
        );
        let plan_card = catalog.get(plan, "glm-5.3-chat-completions").unwrap();
        let ordinary_card = catalog.get(ordinary, "glm-5.3-chat-completions").unwrap();
        assert_ne!(
            plan_card.private_history_source(),
            ordinary_card.private_history_source(),
            "billing connections must not share private replay or cache identity"
        );
    }
}

fn access_yaml(provider: &str, base: &str, flash: &str) -> String {
    format!(
        "provider: {provider}\ngroup: Zhipu GLM\nseries:\n  - {{id: glm, name: GLM, version: \"{}\", model: {base}}}\n  - {{id: glm-flash, name: GLM Flash, version: \"5.3\", model: {flash}}}\n",
        base.trim_start_matches("glm-")
            .trim_end_matches("-chat-completions")
    )
}

fn catalog_with_card(id: &str, service_model_id: &str, display_name: &str) -> ModelCatalog {
    let patch: ProviderPatch = serde_yaml_ng::from_str(&format!(
        "id: zai-global-api\nmodels:\n  - {{id: {id}, service_model_id: {service_model_id}, display_name: {display_name}, adapter: glm_chat_completions, max_output_tokens: 131072, context_window_tokens: 1000000, thinking: always_on, effort_levels: [low, high, max]}}\n"
    ))
    .unwrap();
    ModelCatalog::defaults()
        .with_providers(vec![patch])
        .unwrap()
}

#[test]
fn series_upgrade_rebinds_one_entry_and_keeps_old_cards_resolvable() {
    for (new_card, service, name, label) in [
        (
            "glm-5.4-chat-completions",
            "glm-5.4",
            "GLM-5.4",
            "GLM (5.4)",
        ),
        ("glm-6-chat-completions", "glm-6", "GLM-6", "GLM (6)"),
    ] {
        let definitions = catalog_with_card(new_card, service, name);
        let access = AccessCatalog::from_yaml(&[&access_yaml(
            "zai-global-api",
            new_card,
            "glm-5.3-flash-chat-completions",
        )])
        .unwrap();
        access.validate(&definitions).unwrap();
        let mut offered = definitions
            .profiles()
            .into_iter()
            .filter(|profile| profile.provider_id == "zai-global-api")
            .filter(|profile| access.ordinary_candidate(profile, &definitions) == Some(true))
            .collect::<Vec<_>>();
        offered.sort_by_key(|profile| access.series_position(profile));
        let offered = offered
            .iter()
            .map(|profile| access.series_for(profile).unwrap().label())
            .collect::<Vec<_>>();
        assert_eq!(offered, [label, "GLM Flash (5.3)"]);
        for retired in ["glm-5.3-chat-completions", "glm-5.2-chat-completions"] {
            let old = definitions.get("zai-global-api", retired).unwrap();
            assert!(old.name.starts_with("GLM-5."));
            assert_eq!(access.series_for(&old), None);
            assert_eq!(access.ordinary_candidate(&old, &definitions), Some(false));
        }
        let upgraded = definitions.get("zai-global-api", new_card).unwrap();
        assert_eq!(upgraded.name, name);
        assert_eq!(upgraded.model, service);
    }
}

#[test]
fn retained_builtin_version_uses_compiled_definition_without_external_example() {
    let pinned = ModelCatalog::defaults()
        .get("zhipu-cn-api", "glm-5.2")
        .unwrap();
    assert_eq!(pinned.model_id, "glm-5.2");
    assert_eq!(pinned.name, "GLM-5.2");
    assert_eq!(pinned.model, "glm-5.2");
    assert_eq!(AccessCatalog::defaults().series_for(&pinned), None);
}

#[test]
fn internal_catalog_fixtures_keep_retired_cards_out_and_custom_models_separate() {
    let patch: ProviderPatch = serde_yaml_ng::from_str(
        "id: zhipu-cn-api\nmodels:\n  - {id: glm-5.2-chat-completions, max_output_tokens: 65536}\n  - {id: glm-5.3-chat-completions, max_output_tokens: 65536}\n  - {id: private-glm, service_model_id: glm-5.3, display_name: Private GLM, adapter: glm_chat_completions, max_output_tokens: 8192}\n",
    )
    .unwrap();
    let defaults = ModelCatalog::defaults();
    let effective = ModelCatalog::defaults()
        .with_providers(vec![patch])
        .unwrap();
    let access = AccessCatalog::defaults();
    access.validate(&effective).unwrap();
    let offered = |model_id: &str| {
        access.ordinary_candidate(&effective.get("zhipu-cn-api", model_id).unwrap(), &defaults)
    };
    assert_eq!(offered("glm-5.2-chat-completions"), Some(false));
    assert_eq!(offered("glm-5.3-chat-completions"), Some(true));
    assert_eq!(offered("private-glm"), Some(true));
    let overridden = effective
        .get("zhipu-cn-api", "glm-5.3-chat-completions")
        .unwrap();
    assert_eq!(overridden.max_output_tokens, 65536);
    assert_eq!(access.series_for(&overridden).unwrap().label(), "GLM (5.3)");
}

#[test]
fn access_bindings_reject_missing_foreign_duplicate_and_unknown_entries() {
    let models = ModelCatalog::defaults();
    let binding = |provider: &str, model: &str| {
        format!(
            "provider: {provider}\nseries:\n  - {{id: glm, name: GLM, version: \"5.3\", model: {model}}}\n"
        )
    };
    for files in [
        vec![binding("zai-global-api", "glm-9-chat-completions")],
        vec![binding("zai-global-api", "deepseek-flash-chat-completions")],
        vec![binding("unknown-api", "glm-5.3-chat-completions")],
        vec![
            binding("zai-global-api", "glm-5.3-chat-completions"),
            binding("zai-global-api", "glm-5.3-chat-completions"),
        ],
        vec![
            "provider: zai-global-api\nseries:\n  - {id: glm, name: GLM, version: \"5.3\", model: glm-5.3-chat-completions}\n  - {id: glm-again, name: GLM again, version: \"5.3\", model: glm-5.3-chat-completions}\n".into(),
        ],
        vec![
            "provider: zai-global-api\nseries:\n  - {id: glm, name: GLM, version: \"5.3\", model: glm-5.3-chat-completions}\n  - {id: glm, name: GLM Flash, version: \"5.3\", model: glm-5.3-flash-chat-completions}\n".into(),
        ],
        vec!["provider: zai-global-api\nseries: []\n".into()],
    ] {
        let files = files.iter().map(String::as_str).collect::<Vec<_>>();
        let catalog = AccessCatalog::from_yaml(&files).unwrap();
        assert!(catalog.validate(&models).is_err(), "{files:?}");
    }
    assert!(
        AccessCatalog::from_yaml(&[
            "provider: zai-global-api\nseries:\n  - {id: glm, name: GLM, version: \"5.3\", model: glm-5.3-chat-completions, latest: true}\n"
        ])
        .is_err()
    );
}

#[tokio::test]
async fn regional_api_streams_preserved_thinking_tools_images_and_cached_usage() {
    for (provider, _, _) in REGIONS {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let reply = [
            chunk(json!({"role":"assistant","reasoning_content":"Look at the image."}), Value::Null, Value::Null),
            chunk(json!({"content":"Checking."}), Value::Null, Value::Null),
            chunk(json!({"tool_calls":[{"index":0,"id":"call_glm_1","type":"function","function":{"name":"Echo","arguments":""}}]}), Value::Null, Value::Null),
            chunk(json!({"tool_calls":[{"index":0,"id":"call_glm_1","type":"function","function":{"name":"Echo","arguments":"{\"text\":"}}]}), Value::Null, Value::Null),
            chunk(json!({"tool_calls":[{"index":0,"function":{"arguments":"\"red\"}"}}]}), Value::Null, Value::Null),
            chunk(json!({"role":"assistant","content":""}), json!("tool_calls"), json!({"prompt_tokens":900,"completion_tokens":40,"total_tokens":940,"prompt_tokens_details":{"cached_tokens":512}})),
            "data: [DONE]\n\n".into(),
        ]
        .concat();
        let (url, captured) = model_http_support::serve_response(200, reply).await;
        let config = configured(provider, "glm-5.3-flash-chat-completions", &root, url);
        let mut input = user_request(Some(ReasoningEffort::High));
        input.messages[0].content.push(Content::Image {
            source: ImageSource::Base64 {
                media_type: ImageMediaType::Png,
                data: "AQ==".into(),
            },
            width: 1,
            height: 1,
        });
        input.tools.push(ToolDefinition {
            name: "Echo".into(),
            description: "Returns text unchanged.".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
        });
        let (events, _) = mpsc::unbounded_channel();
        let output = model_client(config, Some(&root))
            .unwrap()
            .complete(input, events, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(output.stop_reason, "tool_use");
        assert_eq!(
            output.content,
            vec![
                Content::Thinking {
                    thinking: "Look at the image.".into(),
                    signature: None
                },
                Content::Text {
                    text: "Checking.".into()
                },
                Content::ToolUse {
                    id: "call_glm_1".into(),
                    name: "Echo".into(),
                    input: json!({"text":"red"})
                },
            ]
        );
        assert_eq!(output.usage.input_tokens, Some(900));
        assert_eq!(output.usage.output_tokens, Some(40));
        assert_eq!(output.usage.cache_read_input_tokens, Some(512));
        assert_eq!(output.usage.cache_write_input_tokens, None);
        let source = output.private_source.unwrap();
        assert_eq!(source.adapter, Adapter::GlmChatCompletions);
        assert!(source.credential_identity.is_some());
        let (headers, body) = captured.await.unwrap();
        assert!(headers.starts_with("POST /anthropic/chat/completions "));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer test-glm-api-key")
        );
        assert_eq!(body["model"], "glm-5.3-flash");
        assert_eq!(body["max_tokens"], 131072);
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["thinking"],
            json!({"type":"enabled","clear_thinking":false})
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["tool_stream"], true);
        assert_eq!(body["tools"][0]["function"]["name"], "Echo");
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("response_format").is_none());
        assert_eq!(
            body["messages"][0]["content"][1],
            json!({"type":"image_url","image_url":{"url":"data:image/png;base64,AQ=="}})
        );
    }
}

#[tokio::test]
async fn streamed_tool_call_identity_stays_bound_to_one_index() {
    for second in [
        json!({"index":0,"id":"call_b","type":"function","function":{"name":"Echo","arguments":"}"}}),
        json!({"index":1,"id":"call_a","type":"function","function":{"name":"Echo","arguments":"{}"}}),
        json!({"index":1,"function":{"arguments":"{}"}}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let reply = [
            chunk(json!({"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"Echo","arguments":"{"}}]}), Value::Null, Value::Null),
            chunk(json!({"tool_calls":[second]}), Value::Null, Value::Null),
            chunk(json!({}), json!("tool_calls"), Value::Null),
            "data: [DONE]\n\n".into(),
        ]
        .concat();
        let (url, _) = model_http_support::serve_response(200, reply).await;
        let config = configured("zai-global-api", "glm-5.3-chat-completions", &root, url);
        let (events, _) = mpsc::unbounded_channel();
        let error = model_client(config, Some(&root))
            .unwrap()
            .complete(user_request(None), events, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ModelFailureKind::InvalidResponse);
    }
}

/// The compiled catalog retains GLM-5.2 for explicit identity selection.
fn pinned_glm_5_2(root: &std::path::Path, url: String) -> ModelConfig {
    let mut config = ModelCatalog::defaults()
        .get("zai-global-api", "glm-5.2")
        .unwrap();
    save_api_key(root, &config.api_key_env, "test-glm-api-key").unwrap();
    config.base_url = url;
    config
}

#[tokio::test]
async fn thinking_defaults_and_pinned_toggle_card_off_follow_the_model_card() {
    for (pinned, effort, expected) in [
        (
            false,
            None,
            json!({"type":"enabled","clear_thinking":false}),
        ),
        (
            true,
            Some(ReasoningEffort::Off),
            json!({"type":"disabled","clear_thinking":false}),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let (url, captured) =
            model_http_support::serve_response(200, text_reply("", "answer")).await;
        let config = if pinned {
            pinned_glm_5_2(&root, url)
        } else {
            configured("zai-global-api", "glm-5.3-chat-completions", &root, url)
        };
        let model = config.model.clone();
        let (events, _) = mpsc::unbounded_channel();
        model_client(config, Some(&root))
            .unwrap()
            .complete(user_request(effort), events, CancellationToken::new())
            .await
            .unwrap();
        let (_, body) = captured.await.unwrap();
        assert_eq!(body["thinking"], expected, "{model}");
        assert!(body.get("reasoning_effort").is_none(), "{model}");
        assert!(body.get("tool_stream").is_none(), "{model}");
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let config = configured(
        "zhipu-cn-api",
        "glm-5.3-chat-completions",
        &root,
        "http://127.0.0.1:9/api/paas/v4".into(),
    );
    let (events, _) = mpsc::unbounded_channel();
    let error = model_client(config, Some(&root))
        .unwrap()
        .complete(
            user_request(Some(ReasoningEffort::Off)),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelFailureKind::Configuration);
}

#[tokio::test]
async fn same_source_reasoning_replays_unchanged_while_foreign_thinking_becomes_text() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (url, mut captured) = model_http_support::serve_responses(vec![
        (200, text_reply("First reasoning", "First answer")),
        (200, text_reply("Second reasoning", "Second answer")),
    ])
    .await;
    let config = configured("zhipu-cn-api", "glm-5.3-chat-completions", &root, url);
    let client = model_client(config, Some(&root)).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    let first = client
        .complete(user_request(None), events.clone(), CancellationToken::new())
        .await
        .unwrap();
    let mut follow_up = user_request(None);
    follow_up.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            Content::Thinking {
                thinking: "Foreign signed reasoning".into(),
                signature: Some("foreign-signature".into()),
            },
            Content::Text {
                text: "Foreign answer".into(),
            },
        ],
    });
    follow_up.history_sources.push(None);
    follow_up.messages.push(Message {
        role: Role::User,
        content: vec![Content::Text {
            text: "Continue".into(),
        }],
    });
    follow_up.history_sources.push(None);
    follow_up.messages.push(Message {
        role: Role::Assistant,
        content: first.content,
    });
    follow_up.history_sources.push(first.private_source);
    follow_up.messages.push(Message {
        role: Role::User,
        content: vec![Content::Text {
            text: "Again".into(),
        }],
    });
    follow_up.history_sources.push(None);
    client
        .complete(follow_up, events, CancellationToken::new())
        .await
        .unwrap();
    captured.recv().await.unwrap();
    let (_, body) = captured.recv().await.unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[1],
        json!({"role":"assistant","content":"Foreign signed reasoningForeign answer"})
    );
    assert!(!body.to_string().contains("foreign-signature"));
    assert_eq!(
        messages[3],
        json!({"role":"assistant","content":"First answer","reasoning_content":"First reasoning"})
    );
}

#[tokio::test]
async fn structured_output_uses_json_mode_with_schema_prompt_and_never_tools() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (url, captured) =
        model_http_support::serve_response(200, text_reply("Assess.", "{\"allow\":false}")).await;
    let config = configured("zai-global-api", "glm-5.3-chat-completions", &root, url);
    let schema = json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false});
    let mut input = user_request(None);
    input.messages.insert(
        0,
        Message {
            role: Role::System,
            content: vec![Content::Text {
                text: "Review the operation.".into(),
            }],
        },
    );
    input.history_sources.insert(0, None);
    input.temperature = Some(0.0);
    input.structured_output = Some(StructuredOutput {
        name: "verdict".into(),
        schema: schema.clone(),
    });
    let client = model_client(config, Some(&root)).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    let output = client
        .complete(input.clone(), events.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(output.stop_reason, "end_turn");
    assert!(output.content.contains(&Content::Text {
        text: "{\"allow\":false}".into()
    }));
    let (_, body) = captured.await.unwrap();
    assert_eq!(body["response_format"], json!({"type":"json_object"}));
    assert_eq!(body["temperature"], 0.0);
    assert!(body.get("tools").is_none());
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["content"], "Review the operation.");
    assert_eq!(messages[1]["role"], "system");
    let instruction = messages[1]["content"].as_str().unwrap();
    assert!(instruction.contains("verdict"));
    assert!(instruction.contains(&schema.to_string()));
    assert_eq!(messages[2]["role"], "user");

    input.tools.push(ToolDefinition {
        name: "Bash".into(),
        description: "Runs a command.".into(),
        input_schema: json!({"type":"object"}),
    });
    let error = client
        .complete(input, events, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelFailureKind::Configuration);
}

#[tokio::test]
async fn glm_finish_reasons_map_to_existing_rejections() {
    for (reason, expected) in [
        ("sensitive", "content_filter"),
        ("model_context_window_exceeded", "context_limit"),
        ("network_error", "retryable_server"),
        ("length", "output_limit"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let (url, _) = model_http_support::serve_response(200, finish_reply(reason)).await;
        let config = configured("zhipu-cn-api", "glm-5.3-chat-completions", &root, url);
        let (events, _) = mpsc::unbounded_channel();
        let error = model_client(config, Some(&root))
            .unwrap()
            .complete(user_request(None), events, CancellationToken::new())
            .await
            .unwrap_err();
        let actual = match error {
            ModelError::ResponseRejected(ModelResponseRejection::ContentFilter) => "content_filter",
            ModelError::ResponseRejected(ModelResponseRejection::ContextLimit) => "context_limit",
            ModelError::ResponseRejected(ModelResponseRejection::OutputLimit) => "output_limit",
            ref error if error.retry_hint().0 && error.status() == Some(500) => "retryable_server",
            error => panic!("{reason}: {error:?}"),
        };
        assert_eq!(actual, expected, "{reason}");
    }
}

#[tokio::test]
async fn business_error_codes_decide_retry_and_failure_kind() {
    for (status, code, kind, retry) in [
        (429, json!("1113"), ModelFailureKind::QuotaExhausted, false),
        (429, json!("1308"), ModelFailureKind::QuotaExhausted, false),
        (
            429,
            json!("1311"),
            ModelFailureKind::EntitlementDenied,
            false,
        ),
        (429, json!("1302"), ModelFailureKind::RateLimited, true),
        (429, json!("1305"), ModelFailureKind::RateLimited, true),
        (400, json!(1261), ModelFailureKind::ResponseRejected, false),
        (
            400,
            json!("1301"),
            ModelFailureKind::ResponseRejected,
            false,
        ),
        (400, json!("1214"), ModelFailureKind::ProviderFailure, false),
        (500, json!("1234"), ModelFailureKind::ProviderFailure, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let body =
            json!({"error":{"code":code,"message":"Request refused by the provider"}}).to_string();
        let (url, _) = model_http_support::serve_response(status, body).await;
        let config = configured("zai-global-api", "glm-5.3-chat-completions", &root, url);
        let (events, _) = mpsc::unbounded_channel();
        let error = model_client(config, Some(&root))
            .unwrap()
            .complete(user_request(None), events, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), kind, "{code}");
        assert_eq!(error.retry_hint().0, retry, "{code}");
        if kind != ModelFailureKind::ResponseRejected {
            assert!(
                error
                    .to_string()
                    .contains("Request refused by the provider"),
                "{code}"
            );
        }
    }
}

fn signed_messages_reply() -> String {
    let frame = |kind: &str, mut data: Value| {
        data["type"] = json!(kind);
        format!("event: {kind}\ndata: {data}\n\n")
    };
    [
        frame("message_start", json!({"message":{"role":"assistant","content":[]}})),
        frame("content_block_start", json!({"index":0,"content_block":{"type":"thinking","thinking":"Claude reasoning","signature":"claude-signed-block"}})),
        frame("content_block_stop", json!({"index":0})),
        frame("content_block_start", json!({"index":1,"content_block":{"type":"text","text":"Claude answer"}})),
        frame("content_block_stop", json!({"index":1})),
        frame("message_delta", json!({"delta":{"stop_reason":"end_turn"}})),
        frame("message_stop", json!({})),
    ]
    .concat()
}

async fn finish_turn(agent: &Agent, chat: Uuid, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => return,
            EventKind::Failed(error) => panic!("{prompt}: {error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn glm_messages_restart_glm_keeps_original_history_and_same_source_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("state");
    let (glm_url, mut glm_requests) = model_http_support::serve_responses(vec![
        (200, text_reply("GLM first reasoning", "GLM first answer")),
        (200, text_reply("GLM second reasoning", "GLM second answer")),
        (200, text_reply("GLM returned reasoning", "GLM returned")),
    ])
    .await;
    let (messages_url, mut messages_requests) =
        model_http_support::serve_responses(vec![(200, signed_messages_reply())]).await;
    let glm = configured("zhipu-cn-api", "glm-5.3-chat-completions", &root, glm_url);
    let mut claude = ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-4-6")
        .unwrap();
    claude.base_url = messages_url;
    claude.api_key_env = glm.api_key_env.clone();
    let agent = Agent::open_embedded(
        root.clone(),
        glm.clone(),
        model_client(glm.clone(), Some(&root)).unwrap(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    finish_turn(&agent, chat, "First prompt").await;
    finish_turn(&agent, chat, "Second prompt").await;
    agent
        .set_model(
            chat,
            claude.clone(),
            model_client(claude.clone(), Some(&root)).unwrap(),
        )
        .await
        .unwrap();
    finish_turn(&agent, chat, "Use Claude").await;
    let mixed = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;

    let resumed = Agent::open_embedded(
        root.clone(),
        claude.clone(),
        model_client(claude, Some(&root)).unwrap(),
    )
    .unwrap();
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert_eq!(restored.messages, mixed.messages);
    assert_eq!(restored.message_sources, mixed.message_sources);
    resumed
        .set_model(chat, glm.clone(), model_client(glm, Some(&root)).unwrap())
        .await
        .unwrap();
    finish_turn(&resumed, chat, "Return to GLM").await;
    resumed.shutdown().await;

    glm_requests.recv().await.unwrap();
    let (_, second) = glm_requests.recv().await.unwrap();
    let (_, returned) = glm_requests.recv().await.unwrap();
    let (_, claude_request) = messages_requests.recv().await.unwrap();
    let assistant_with_reasoning = |body: &Value| {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["reasoning_content"] == "GLM first reasoning")
            .cloned()
            .unwrap()
    };
    assert_eq!(
        assistant_with_reasoning(&second),
        assistant_with_reasoning(&returned)
    );
    let second_messages = second["messages"].as_array().unwrap();
    let returned_messages = returned["messages"].as_array().unwrap();
    // The system prompt carries live host state; the conversation after it must be stable.
    let shared = second_messages.len() - 1;
    assert_eq!(
        second_messages[1..shared],
        returned_messages[1..shared],
        "same-source GLM history must replay unchanged"
    );
    let returned_text = returned.to_string();
    assert!(!returned_text.contains("claude-signed-block"));
    assert!(returned_text.contains("Claude reasoning"));
    assert!(returned_text.contains("GLM second reasoning"));
    let claude_text = claude_request.to_string();
    assert!(!claude_text.contains("reasoning_content"));
    assert!(claude_text.contains("GLM first reasoning"));
    assert!(claude_text.contains("GLM first answer"));
}

#[tokio::test]
async fn network_error_on_the_documented_empty_final_chunk_is_retried_by_the_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("state");
    let failure = [
        chunk(
            json!({"role":"assistant","content":""}),
            Value::Null,
            Value::Null,
        ),
        chunk(
            json!({"role":"assistant","content":""}),
            json!("network_error"),
            Value::Null,
        ),
        "data: [DONE]\n\n".into(),
    ]
    .concat();
    let (url, mut requests) = model_http_support::serve_responses(vec![
        (200, failure),
        (200, text_reply("Retry reasoning", "Recovered answer")),
    ])
    .await;
    let glm = configured("zai-global-api", "glm-5.3-chat-completions", &root, url);
    let agent = Agent::open_embedded(
        root.clone(),
        glm.clone(),
        model_client(glm, Some(&root)).unwrap(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    finish_turn(&agent, chat, "Try once more").await;
    requests.recv().await.unwrap();
    requests.recv().await.unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| message.content.contains(&Content::Text {
                text: "Recovered answer".into()
            }))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn typed_balance_and_allowance_codes_determine_http_classification() {
    for (code, class) in [
        (
            json!(1113),
            tickloop_agent_protocol::ProviderFailureClass::Billing,
        ),
        (
            json!("1113"),
            tickloop_agent_protocol::ProviderFailureClass::Billing,
        ),
        (
            json!(1308),
            tickloop_agent_protocol::ProviderFailureClass::UsageLimit,
        ),
        (
            json!("1310"),
            tickloop_agent_protocol::ProviderFailureClass::UsageLimit,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let body = json!({"error":{"code":code,"message":"Account request refused"}}).to_string();
        let (url, _) = model_http_support::serve_response(429, body).await;
        let config = configured("zai-global-api", "glm-5.3-chat-completions", &root, url);
        let (events, _) = mpsc::unbounded_channel();
        let error = model_client(config, Some(&root))
            .unwrap()
            .complete(user_request(None), events, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.provider_failure().unwrap().class, class, "{error}");
        assert!(!error.retry_hint().0);
    }
}
