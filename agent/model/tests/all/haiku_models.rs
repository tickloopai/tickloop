use serde_json::{Value, json};
use tickloop_agent_model::{
    ModelConfigExt, ModelRequest, ModelRequestContext, StructuredOutput,
    access::AccessCatalog,
    auth::copilot::CopilotAuth,
    cards::{ModelCatalog, built_in_model_profiles},
    credentials::save_api_key,
    history, model_client, validate_reasoning_effort,
};
use tickloop_agent_protocol::{
    Content, Message, ModelConfig, ReasoningEffort, Role, ToolDefinition,
};
use tickloop_agent_test_support::model_http_support;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

const CONNECTIONS: [(&str, &str, &str); 2] = [
    (
        "anthropic-api",
        "claude-haiku-5-5",
        "claude-haiku-4-5-20251001",
    ),
    ("github-copilot", "claude-haiku-5.5", "claude-haiku-4.5"),
];

fn frame(kind: &str, mut value: Value) -> String {
    value["type"] = json!(kind);
    format!("event: {kind}\ndata: {value}\n\n")
}

fn reply(blocks: Vec<Value>, stop: &str) -> String {
    let mut frames = vec![frame(
        "message_start",
        json!({"message":{"role":"assistant","content":[]}}),
    )];
    for (index, mut block) in blocks.into_iter().enumerate() {
        let signature = block.as_object_mut().unwrap().remove("signature");
        frames.push(frame(
            "content_block_start",
            json!({"index":index,"content_block":block}),
        ));
        if let Some(signature) = signature {
            frames.push(frame(
                "content_block_delta",
                json!({"index":index,"delta":{"type":"signature_delta","signature":signature}}),
            ));
        }
        frames.push(frame("content_block_stop", json!({"index":index})));
    }
    frames.push(frame(
        "message_delta",
        json!({"delta":{"stop_reason":stop}}),
    ));
    frames.push(frame("message_stop", json!({})));
    frames.concat()
}

fn request(effort: Option<ReasoningEffort>) -> ModelRequest {
    ModelRequest {
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

async fn configured(
    provider: &str,
    model: &str,
    root: &std::path::Path,
    endpoint: String,
) -> ModelConfig {
    let mut config = ModelCatalog::defaults().get(provider, model).unwrap();
    if provider == "github-copilot" {
        let responses = vec![
            json!({"device_code":"fixture","user_code":"FIXTURE","verification_uri":"https://github.com/login/device","expires_in":900,"interval":0}),
            json!({"access_token":"fixture-user-token","token_type":"bearer"}),
            json!({"token":"tid=fixture;exp=4102444800;sku=copilot_pro","expires_at":4102444800u64}),
        ];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for body in responses {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut headers = String::new();
                while !headers.ends_with("\r\n\r\n") {
                    assert_ne!(reader.read_line(&mut headers).await.unwrap(), 0);
                }
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                reader.read_exact(&mut vec![0; length]).await.unwrap();
                let body = body.to_string();
                reader.into_inner().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let auth = CopilotAuth::new_for_loopback_test(root, issuer.trim_end_matches("/anthropic"))
            .unwrap();
        let cancel = CancellationToken::new();
        let login = auth.start_device_login(&cancel).await.unwrap();
        auth.finish_device_login(login, &cancel).await.unwrap();
        auth.resolve_access(&cancel).await.unwrap();
        server.await.unwrap();
        config.base_url = endpoint.trim_end_matches("/anthropic").into();
    } else {
        save_api_key(root, &config.api_key_env, "fixture-anthropic-key").unwrap();
        config.base_url = endpoint;
    }
    config
}

#[test]
fn haiku_upgrade_rebinds_each_series_and_preserves_original_connection_limits_and_old_identity() {
    let catalog = ModelCatalog::defaults();
    let access = AccessCatalog::defaults();
    access.validate(&catalog).unwrap();
    let visible = built_in_model_profiles();
    for (provider, model, old) in CONNECTIONS {
        let new = catalog.get(provider, model).unwrap();
        let retained = catalog.get(provider, old).unwrap();
        assert_eq!(retained.model, old);
        assert!(visible.contains(&new));
        assert!(!visible.contains(&retained));
        assert!(access.series_for(&retained).is_none());
        let series = access.series_for(&new).unwrap();
        assert_eq!(series.model, model);
        let native = provider == "anthropic-api";
        assert_eq!(new.max_output_tokens, if native { 128000 } else { 32000 });
        assert_eq!(
            new.maximum_context_window_tokens(),
            native.then_some(1000000)
        );
        assert_eq!(new.accepts_images(), native);
        let card = new.card.as_ref().unwrap();
        assert!(card.adaptive_by_default);
        assert!(card.thinking_budgets.is_none());
        for effort in [
            ReasoningEffort::Off,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ] {
            assert!(validate_reasoning_effort(&new, Some(effort)).is_ok());
        }
        assert!(retained.card.as_ref().unwrap().thinking_budgets.is_some());
        assert_eq!(
            access.recommended_model(provider),
            Some(if native {
                "claude-fable-5-1"
            } else {
                "claude-sonnet-5.5"
            })
        );
    }
}

#[tokio::test]
async fn haiku_adaptive_effort_schema_sampling_and_per_request_output_budget_follow_each_connection()
 {
    for (provider, model, _) in CONNECTIONS {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let modes = [
            None,
            Some(ReasoningEffort::Off),
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::Medium),
            Some(ReasoningEffort::High),
            Some(ReasoningEffort::Xhigh),
            Some(ReasoningEffort::Max),
        ];
        let response = reply(
            vec![json!({"type":"text","text":"{\"allow\":false}"})],
            "end_turn",
        );
        let (endpoint, mut captured) =
            model_http_support::serve_responses(vec![(200, response); modes.len()]).await;
        let config = configured(provider, model, root.path(), endpoint).await;
        let original_limit = config.max_output_tokens;
        let client = model_client(config, Some(root.path())).unwrap();
        for mode in modes {
            let mut query = request(mode);
            query.structured_output = Some(StructuredOutput {
                name: "verdict".into(),
                schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
            });
            let (events, _) = mpsc::unbounded_channel();
            let context = ModelRequestContext::new(1).with_output_token_limit(4096);
            client
                .complete_for_chat_with_context(
                    uuid::Uuid::new_v4(),
                    query,
                    events,
                    CancellationToken::new(),
                    context,
                )
                .await
                .unwrap();
            let (headers, body) = captured.recv().await.unwrap();
            assert_eq!(body["model"], model);
            assert_eq!(body["max_tokens"], 4096);
            assert_eq!(
                body["thinking"]["type"],
                if mode == Some(ReasoningEffort::Off) {
                    "disabled"
                } else {
                    "adaptive"
                }
            );
            assert!(body["thinking"].get("budget_tokens").is_none());
            for field in ["temperature", "top_p", "top_k"] {
                assert!(body.get(field).is_none());
            }
            assert_eq!(body["output_config"]["format"]["type"], "json_schema");
            if let Some(effort) = mode.filter(|mode| *mode != ReasoningEffort::Off) {
                assert_eq!(body["output_config"]["effort"], effort.label());
            } else {
                assert!(body["output_config"].get("effort").is_none());
            }
            if provider == "github-copilot" {
                assert!(headers.to_lowercase().contains("x-initiator: user"));
            }
        }
        assert_eq!(
            ModelCatalog::defaults()
                .get(provider, model)
                .unwrap()
                .max_output_tokens,
            original_limit
        );
    }
}

#[tokio::test]
async fn haiku_signature_only_thinking_roundtrips_without_foreign_replay_or_rewriting_original_history()
 {
    for (provider, model, old) in CONNECTIONS {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let tool_reply = reply(
            vec![
                json!({"type":"thinking","thinking":"","signature":"opaque-signature"}),
                json!({"type":"tool_use","id":"echo-1","name":"Echo","input":{"text":"hello"}}),
            ],
            "tool_use",
        );
        let response = reply(vec![json!({"type":"text","text":"hello"})], "end_turn");
        let (endpoint, mut captured) =
            model_http_support::serve_responses(vec![(200, tool_reply), (200, response)]).await;
        let config = configured(provider, model, root.path(), endpoint).await;
        let client = model_client(config.clone(), Some(root.path())).unwrap();
        let mut query = request(Some(ReasoningEffort::Low));
        query.tools.push(ToolDefinition {name:"Echo".into(),description:"Inert fixture".into(),input_schema:json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})});
        let (events, _) = mpsc::unbounded_channel();
        let first = client
            .complete(query.clone(), events.clone(), CancellationToken::new())
            .await
            .unwrap();
        let (_, original_wire) = captured.recv().await.unwrap();
        assert!(
            matches!(&first.content[0], Content::Thinking{thinking, signature:Some(signature)} if thinking.is_empty() && signature == "opaque-signature")
        );
        query.messages.push(Message {
            role: Role::Assistant,
            content: first.content,
        });
        query.history_sources.push(first.private_source);
        query.messages.push(Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "echo-1".into(),
                content: "hello".into(),
                is_error: false,
            }],
        });
        query.history_sources.push(None);
        let original = serde_json::to_value(&query.messages).unwrap();
        query.messages = serde_json::from_value(original.clone()).unwrap();
        let foreign = ModelCatalog::defaults().get(provider, old).unwrap();
        let projected = history::project_request(
            query.clone(),
            &foreign,
            foreign.private_history_source().as_ref(),
        );
        assert!(
            !serde_json::to_string(&projected.messages)
                .unwrap()
                .contains("opaque-signature")
        );
        assert_eq!(serde_json::to_value(&query.messages).unwrap(), original);
        client
            .complete(query, events, CancellationToken::new())
            .await
            .unwrap();
        let (_, continued) = captured.recv().await.unwrap();
        assert_eq!(continued["messages"][0], original_wire["messages"][0]);
        assert_eq!(continued["tools"], original_wire["tools"]);
        assert_eq!(
            continued["messages"][1]["content"][0],
            json!({"type":"thinking","thinking":"","signature":"opaque-signature"})
        );
        assert_eq!(
            continued["messages"][2]["content"][0]["tool_use_id"],
            "echo-1"
        );
    }
}
