use tickloop_agent_test_support::model_http_support;

use serde_json::{Value, json};
use tickloop_agent_model::{
    LocalModelClients, ModelClients, ModelConfigExt, ModelError, ModelRequest, StructuredOutput,
    auth::minimax::{MiniMaxAuth, MiniMaxRegion},
    cards::ModelCatalog,
    credentials::save_api_key,
    validate_reasoning_effort,
};
use tickloop_agent_protocol::{
    Content, ImageMediaType, ImageSource, Message, ReasoningEffort, Role,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

fn request(effort: Option<ReasoningEffort>) -> ModelRequest {
    ModelRequest {
        history_sources: Vec::new(),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Hello".into(),
            }],
        }],
        tools: Vec::new(),
        structured_output: None,
        temperature: Some(0.0),
        reasoning_effort: effort,
    }
}

fn response(blocks: Vec<Value>, reason: &str) -> String {
    let frame = |kind: &str, mut value: Value| {
        value["type"] = json!(kind);
        format!("event: {kind}\ndata: {value}\n\n")
    };
    let mut output = frame(
        "message_start",
        json!({"message":{"role":"assistant","content":[],"usage":{"input_tokens":1024,"cache_read_input_tokens":512,"cache_creation_input_tokens":128}}}),
    );
    for (index, block) in blocks.into_iter().enumerate() {
        output += &frame(
            "content_block_start",
            json!({"index":index,"content_block":block}),
        );
        output += &frame("content_block_stop", json!({"index":index}));
    }
    output += &frame(
        "message_delta",
        json!({"delta":{"stop_reason":reason},"usage":{"output_tokens":32}}),
    );
    output += &frame("message_stop", json!({}));
    output
}

#[test]
fn regional_catalog_and_thinking_controls_match_actual_model_capabilities() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    for region in ["cn", "global"] {
        let api = catalog
            .get(&format!("minimax-{region}-api"), "MiniMax-M3")
            .unwrap();
        assert_eq!(
            api.card.as_ref().unwrap().context_window_tokens.unwrap(),
            1000000
        );
        assert!(api.accepts_images());
        assert_eq!(api.thinking_enabled(None, true), Some(false));
        assert_eq!(
            api.thinking_enabled(Some(ReasoningEffort::On), true),
            Some(true)
        );
        assert_eq!(
            api.thinking_enabled(Some(ReasoningEffort::On), false),
            Some(false)
        );
        assert!(validate_reasoning_effort(&api, Some(ReasoningEffort::On)).is_ok());
        assert!(validate_reasoning_effort(&api, Some(ReasoningEffort::High)).is_err());
        assert!(
            catalog
                .get(
                    &format!("minimax-{region}-api"),
                    "MiniMax-M3.1-Flash-Preview"
                )
                .is_none()
        );
        let preview = catalog
            .get(
                &format!("minimax-{region}-oauth"),
                "MiniMax-M3.1-Flash-Preview",
            )
            .unwrap();
        assert!(validate_reasoning_effort(&preview, Some(ReasoningEffort::Off)).is_err());
        assert!(validate_reasoning_effort(&preview, Some(ReasoningEffort::On)).is_err());
        assert!(validate_reasoning_effort(&preview, Some(ReasoningEffort::Max)).is_ok());
    }
}

#[tokio::test]
async fn regional_api_uses_bearer_messages_images_thinking_and_cache_usage() {
    for region in ["cn", "global"] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let mut config = ModelCatalog::defaults()
            .get(&format!("minimax-{region}-api"), "MiniMax-M3")
            .unwrap();
        save_api_key(&root, &config.api_key_env, "test-minimax-api-key").unwrap();
        let (url, captured) = model_http_support::serve_response(
            200,
            response(
                vec![
                    json!({"type":"thinking","thinking":"MiniMax reasoning"}),
                    json!({"type":"text","text":"answer"}),
                ],
                "end_turn",
            ),
        )
        .await;
        config.base_url = url;
        let mut input = request(Some(ReasoningEffort::On));
        input.messages[0].content.push(Content::Image {
            source: ImageSource::Base64 {
                media_type: ImageMediaType::Png,
                data: "AQ==".into(),
            },
            width: 1,
            height: 1,
        });
        let (events, _) = mpsc::unbounded_channel();
        let output = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap()
            .complete(input, events, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(output.usage.cache_read_input_tokens, Some(512));
        assert_eq!(output.usage.cache_write_input_tokens, Some(128));
        assert!(
            output
                .private_source
                .as_ref()
                .unwrap()
                .credential_identity
                .is_some()
        );
        let (headers, body) = captured.await.unwrap();
        assert!(headers.starts_with("POST /anthropic/v1/messages "));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer test-minimax-api-key")
        );
        assert!(!headers.to_ascii_lowercase().contains("x-api-key:"));
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert!(body.get("output_config").is_none());
        assert_eq!(body["messages"][0]["content"][1]["type"], "image");
        assert_eq!(body["temperature"], 0.0);
        assert!(!body.to_string().contains("credential_identity"));
    }
}

#[tokio::test]
async fn structured_response_uses_prompted_json_and_rejects_extra_operations() {
    for extra in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let mut config = ModelCatalog::defaults()
            .get("minimax-cn-api", "MiniMax-M3")
            .unwrap();
        save_api_key(&root, &config.api_key_env, "test-key").unwrap();
        let mut blocks = vec![json!({"type":"text","text":"{\"allow\":false}"})];
        if extra {
            blocks.push(
                json!({"type":"tool_use","id":"other-1","name":"Bash","input":{"command":"false"}}),
            );
        }
        let (url, captured) = model_http_support::serve_response(
            200,
            response(blocks, if extra { "tool_use" } else { "end_turn" }),
        )
        .await;
        config.base_url = url;
        let mut input = request(None);
        input.structured_output = Some(StructuredOutput {
            name: "verdict".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
        });
        let (events, _) = mpsc::unbounded_channel();
        let output = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap()
            .complete(input, events, CancellationToken::new())
            .await;
        if extra {
            assert!(matches!(
                output,
                Err(ModelError::ResponseRejected(
                    tickloop_agent_model::ModelResponseRejection::UnexpectedToolOperation
                ))
            ));
        } else {
            let output = output.unwrap();
            assert_eq!(output.stop_reason, "end_turn");
            assert_eq!(
                output.content,
                vec![Content::Text {
                    text: "{\"allow\":false}".into()
                }]
            );
        }
        let (_, body) = captured.await.unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body["output_config"].get("format").is_none());
        assert!(body["system"].to_string().contains("JSON Schema"));
        assert!(body["system"].to_string().contains("additionalProperties"));
    }
}

async fn login(root: &std::path::Path, region: MiniMaxRegion) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        for step in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut headers = String::new();
            while !headers.ends_with("\r\n\r\n") {
                assert_ne!(reader.read_line(&mut headers).await.unwrap(), 0);
            }
            let length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).await.unwrap();
            let form = reqwest::Url::parse(&format!(
                "http://localhost/?{}",
                String::from_utf8(bytes).unwrap()
            ))
            .unwrap();
            let state = form
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned());
            let payload = if step == 0 {
                json!({"user_code":"code","verification_uri":"https://account.minimax.io/authorize","state":state.unwrap(),"expired_in":4102444800000u64})
            } else {
                json!({"status":"success","access_token":"test-oauth-access","refresh_token":"test-oauth-refresh","expired_in":3600})
            };
            let body = payload.to_string();
            reader.into_inner().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let auth = MiniMaxAuth::new_for_loopback_test(root, region, &origin).unwrap();
    let cancel = CancellationToken::new();
    let device = auth.start_device_login(&cancel).await.unwrap();
    auth.finish_device_login(device, &cancel).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn regional_oauth_credentials_reach_the_model_and_preview_cannot_disable_thinking() {
    for (region, slug) in [
        (MiniMaxRegion::China, "cn"),
        (MiniMaxRegion::Global, "global"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        login(&root, region).await;
        let mut config = ModelCatalog::defaults()
            .get(
                &format!("minimax-{slug}-oauth"),
                "MiniMax-M3.1-Flash-Preview",
            )
            .unwrap();
        let (url, captured) = model_http_support::serve_response(
            200,
            response(
                vec![json!({"type":"text","text":"preview answer"})],
                "end_turn",
            ),
        )
        .await;
        config.base_url = url;
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let result = client
            .complete(
                request(Some(ReasoningEffort::Off)),
                events,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(ModelError::Configuration(_))));
        let (events, _) = mpsc::unbounded_channel();
        let result = client
            .complete(
                request(Some(ReasoningEffort::High)),
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            result
                .private_source
                .as_ref()
                .unwrap()
                .credential_identity
                .as_ref()
                .unwrap()
                .parse::<uuid::Uuid>()
                .is_ok()
        );
        let (headers, body) = captured.await.unwrap();
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer test-oauth-access")
        );
        assert_eq!(body["model"], "MiniMax-M3.1-Flash-Preview");
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert_eq!(body["output_config"]["effort"], "high");
    }
}

#[tokio::test]
async fn minimax_final_input_usage_corrects_provisional_cache_without_ledger_regression() {
    for (provider, initial_output, final_output, accepted) in [
        ("minimax-cn-api", 0, 92, true),
        ("minimax-cn-api", 5, 4, false),
        ("anthropic-api", 0, 92, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let model_id = if provider == "anthropic-api" {
            "claude-haiku-5-5"
        } else {
            "MiniMax-M3"
        };
        let mut config = ModelCatalog::defaults().get(provider, model_id).unwrap();
        save_api_key(&root, &config.api_key_env, "test-key").unwrap();
        let frame = |kind: &str, mut value: Value| {
            value["type"] = json!(kind);
            format!("event: {kind}\ndata: {value}\n\n")
        };
        let body = frame(
            "message_start",
            json!({"message":{"role":"assistant","content":[],"usage":{"input_tokens":0,"output_tokens":initial_output,"cache_read_input_tokens":466,"cache_creation_input_tokens":0}}}),
        ) + &frame(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"text","text":"answer"}}),
        ) + &frame("content_block_stop", json!({"index":0}))
            + &frame(
                "message_delta",
                json!({"delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":275,"output_tokens":final_output,"cache_read_input_tokens":191}}),
            )
            + &frame("message_stop", json!({}));
        let (url, captured) = model_http_support::serve_response(200, body).await;
        config.base_url = url;
        let (events, mut receiver) = mpsc::unbounded_channel();
        let result = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap()
            .complete(request(None), events, CancellationToken::new())
            .await;
        captured.await.unwrap();
        if accepted {
            let output = result.unwrap();
            assert_eq!(output.usage.input_tokens, Some(275));
            assert_eq!(output.usage.output_tokens, Some(92));
            assert_eq!(output.usage.cache_read_input_tokens, Some(191));
            assert_eq!(output.usage.cache_write_input_tokens, Some(0));
            let mut usages = Vec::new();
            while let Ok(delta) = receiver.try_recv() {
                if let tickloop_agent_model::ModelDelta::Usage(usage) = delta {
                    usages.push(usage);
                }
            }
            assert_eq!(
                usages,
                vec![
                    tickloop_agent_protocol::Usage {
                        output_tokens: Some(0),
                        ..Default::default()
                    },
                    output.usage
                ]
            );
        } else {
            assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
        }
    }
}

#[tokio::test]
async fn minimax_usage_preserves_nullable_cache_fallback_and_initial_output_before_disconnect() {
    for complete in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let mut config = ModelCatalog::defaults()
            .get("minimax-cn-api", "MiniMax-M3")
            .unwrap();
        save_api_key(&root, &config.api_key_env, "test-key").unwrap();
        let frame = |kind: &str, mut value: Value| {
            value["type"] = json!(kind);
            format!("event: {kind}\ndata: {value}\n\n")
        };
        let mut body = frame(
            "message_start",
            json!({"message":{"role":"assistant","content":[],"usage":{"input_tokens":0,"output_tokens":7,"cache_read_input_tokens":466,"cache_creation_input_tokens":0}}}),
        ) + &frame(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"text","text":"answer"}}),
        ) + &frame("content_block_stop", json!({"index":0}));
        if complete {
            body += &frame(
                "message_delta",
                json!({"delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":8,"cache_read_input_tokens":null,"cache_creation_input_tokens":null}}),
            );
            body += &frame("message_stop", json!({}));
        }
        let (url, captured) = model_http_support::serve_response(200, body).await;
        config.base_url = url;
        let (events, mut receiver) = mpsc::unbounded_channel();
        let result = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap()
            .complete(request(None), events, CancellationToken::new())
            .await;
        captured.await.unwrap();
        let mut usages = Vec::new();
        while let Ok(delta) = receiver.try_recv() {
            if let tickloop_agent_model::ModelDelta::Usage(usage) = delta {
                usages.push(usage);
            }
        }
        assert_eq!(
            usages[0],
            tickloop_agent_protocol::Usage {
                output_tokens: Some(7),
                ..Default::default()
            }
        );
        if complete {
            let output = result.unwrap();
            assert_eq!(output.usage.cache_read_input_tokens, Some(466));
            assert_eq!(output.usage.cache_write_input_tokens, Some(0));
            assert_eq!(usages[1], output.usage);
        } else {
            assert!(result.is_err());
            assert_eq!(usages.len(), 1);
        }
    }
}
