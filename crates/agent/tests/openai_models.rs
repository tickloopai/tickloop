mod model_http_support;

use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};
use tickloop_agent::{
    image::ImageMediaType,
    model::{
        Content, ImageSource, Message, ModelConfig, ModelRequest, ReasoningEffort, Role,
        StructuredOutput, ToolDefinition,
        access::AccessCatalog,
        availability::{AccountModelDiscovery, ModelAvailability},
        cards::{Adapter, ModelCatalog, ProviderPatch, ThinkingControl, built_in_model_profiles},
        credentials::{load_api_key, save_api_key},
        model_client, validate_reasoning_effort,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

fn config(root: &Path, endpoint: String, model: &str) -> ModelConfig {
    let mut config = ModelCatalog::defaults().get("openai-api", model).unwrap();
    save_api_key(root, &config.api_key_env, "test-openai-key").unwrap();
    config.base_url = endpoint;
    config
}

#[test]
fn api_cards_are_independent_and_recommendations_keep_exact_versions() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    let access = AccessCatalog::defaults();
    access.validate(&catalog).unwrap();
    let api = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "openai-api")
        .unwrap();
    assert_eq!(api.api_key_env, "OPENAI_API_KEY");
    assert_eq!(
        api.endpoints,
        BTreeMap::from([(Adapter::Responses, "https://api.openai.com/v1".into())])
    );
    for model in [
        "gpt-6-astra-responses",
        "gpt-6.1-sol-responses",
        "gpt-6-luna-responses",
    ] {
        let profile = catalog.get("openai-api", model).unwrap();
        let card = profile.card.as_ref().unwrap();
        assert_eq!(card.context_window_tokens, Some(1_050_000));
        assert_eq!(card.max_output_tokens, 128_000);
        assert!(profile.accepts_images());
        assert!(access.series_for(&profile).is_some());
        assert_eq!(card.display_name, profile.name);
        assert_eq!(
            validate_reasoning_effort(&profile, Some(ReasoningEffort::Off)).is_ok(),
            model.contains("luna")
        );
        assert_eq!(
            card.thinking,
            if model.contains("luna") {
                ThinkingControl::Toggle
            } else {
                ThinkingControl::AlwaysOn
            }
        );
    }
    assert_eq!(
        built_in_model_profiles()
            .iter()
            .filter(|profile| profile.provider_id == "openai-api")
            .count(),
        3
    );
    assert_eq!(
        catalog
            .get("openai-codex", "gpt-5.5-responses")
            .unwrap()
            .card
            .unwrap()
            .context_window_tokens,
        Some(272_000)
    );
    assert!(
        serde_yaml_ng::from_str::<ProviderPatch>(
            "id: openai-api\nconnection:\n  vendor: OpenAI\n  product: API\n  kind: invalid\n"
        )
        .is_err()
    );
}

fn stream(output: Vec<Value>) -> String {
    let mut frames = Vec::new();
    let mut sequence = 0;
    let mut event = |kind: &str, mut body: Value| {
        body["type"] = json!(kind);
        body["sequence_number"] = json!(sequence);
        sequence += 1;
        frames.push(format!("event: {kind}\ndata: {body}\n\n"));
    };
    event(
        "response.created",
        json!({"response":{"id":"resp-test","status":"in_progress"}}),
    );
    for (index, item) in output.iter().enumerate() {
        let mut initial = item.clone();
        if item["type"] == "message" {
            initial["content"] = json!([]);
        }
        if item["type"] == "function_call" {
            initial["arguments"] = json!("");
        }
        event(
            "response.output_item.added",
            json!({"output_index":index,"item":initial}),
        );
        if item["type"] == "function_call" {
            event(
                "response.function_call_arguments.delta",
                json!({"output_index":index,"item_id":item["id"],"delta":item["arguments"]}),
            );
        }
        event(
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        );
    }
    event(
        "response.completed",
        json!({"response":{"id":"resp-test","status":"completed","output":output,"usage":{"input_tokens":2000,"output_tokens":80,"input_tokens_details":{"cached_tokens":1000}}}}),
    );
    frames.concat()
}

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message {
            role: Role::User,
            content: vec![
                Content::Text {
                    text: "Use Echo".into(),
                },
                Content::Image {
                    source: ImageSource::Base64 {
                        media_type: ImageMediaType::Png,
                        data: "aW1hZ2U=".into(),
                    },
                    width: 1,
                    height: 1,
                },
            ],
        }],
        history_sources: vec![None],
        tools: vec![ToolDefinition {
            name: "Echo".into(),
            description: "Inert fixture".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
        }],
        structured_output: None,
        temperature: None,
        reasoning_effort: Some(ReasoningEffort::Low),
    }
}

#[tokio::test]
async fn real_client_encodes_api_auth_images_tools_schema_and_preserves_opaque_prefix() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let reasoning = json!({"type":"reasoning","id":"rs-test","status":"completed","summary":[],"encrypted_content":"opaque-api-state"});
    let tool = json!({"type":"function_call","id":"fc-test","call_id":"call-test","name":"Echo","arguments":"{\"text\":\"hello\"}","status":"completed"});
    let text = json!({"type":"message","id":"msg-test","role":"assistant","status":"completed","content":[{"type":"output_text","text":"{\"allow\":false,\"reason\":\"probe\"}"}]});
    let commentary = json!({"type":"message","id":"msg-commentary","role":"assistant","phase":"commentary","status":"completed","content":[{"type":"output_text","text":"Checking the input."}]});
    let final_answer = json!({"type":"message","id":"msg-answer","role":"assistant","phase":"final_answer","status":"completed","content":[{"type":"output_text","text":"Ready to call Echo."}]});
    let (endpoint, mut captured) = model_http_support::serve_responses(vec![
        (
            200,
            stream(vec![reasoning.clone(), commentary, final_answer, tool]),
        ),
        (200, stream(vec![text.clone()])),
        (200, stream(vec![text.clone()])),
        (200, stream(vec![text])),
    ])
    .await;
    let configuration = config(
        &root,
        endpoint.trim_end_matches("/anthropic").to_owned() + "/v1",
        "gpt-6.1-sol-responses",
    );
    let client = model_client(configuration.clone(), Some(&root)).unwrap();
    let first_request = request();
    let (events, _) = mpsc::unbounded_channel();
    let first = client
        .complete(first_request.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(first.stop_reason, "tool_use");
    assert_eq!(first.usage.cache_read_input_tokens, Some(1000));
    assert!(
        matches!(&first.content[0], Content::ProtocolData { value, .. } if value == &reasoning)
    );
    let (headers, wire) = captured.recv().await.unwrap();
    assert!(headers.starts_with("POST /v1/responses "));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-openai-key")
    );
    assert!(!headers.to_ascii_lowercase().contains("chatgpt-account-id"));
    assert_eq!(wire["model"], "gpt-6.1-sol");
    assert_eq!(wire["store"], false);
    assert_eq!(wire["reasoning"]["effort"], "low");
    assert_eq!(
        wire["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,aW1hZ2U="
    );
    assert_eq!(wire["tools"][0]["type"], "function");
    let mut continuation = first_request;
    continuation.messages.push(Message {
        role: Role::Assistant,
        content: first.content,
    });
    continuation.history_sources.push(first.private_source);
    continuation.messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            tool_use_id: "call-test".into(),
            content: "hello".into(),
            is_error: false,
        }],
    });
    continuation.history_sources.push(None);
    continuation.tools.clear();
    continuation.structured_output = Some(StructuredOutput {
        name: "probe_verdict".into(),
        schema: json!({"type":"object","properties":{"allow":{"type":"boolean"},"reason":{"type":"string"}},"required":["allow","reason"],"additionalProperties":false}),
    });
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(continuation.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    let mut other_account = configuration.clone();
    other_account.provider_id = "other-account-api".into();
    other_account.api_key_env = "OTHER_ACCOUNT_KEY".into();
    save_api_key(&root, &other_account.api_key_env, "test-other-account-key").unwrap();
    let other = model_client(other_account, Some(&root)).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    other
        .complete(continuation.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(continuation.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    let second = captured.recv().await.unwrap().1;
    let detour = captured.recv().await.unwrap().1;
    assert!(!detour["input"].to_string().contains("opaque-api-state"));
    assert!(
        detour["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item.get("phase").is_none())
    );
    assert_eq!(detour["input"][1]["content"], "Checking the input.");
    assert_eq!(detour["input"][2]["content"], "Ready to call Echo.");
    let third = captured.recv().await.unwrap().1;
    assert_eq!(second, third);
    assert_eq!(second["input"][1], reasoning);
    assert_eq!(
        second["input"][2],
        json!({"type":"message","role":"assistant","phase":"commentary","content":"Checking the input."})
    );
    assert_eq!(
        second["input"][3],
        json!({"type":"message","role":"assistant","phase":"final_answer","content":"Ready to call Echo."})
    );
    assert_eq!(second["input"].as_array().unwrap().len(), 6);
    assert_eq!(
        second["input"][5],
        json!({"type":"function_call_output","call_id":"call-test","output":"hello"})
    );
    assert_eq!(second["text"]["format"]["type"], "json_schema");
    assert_eq!(second["text"]["format"]["strict"], true);
}

async fn directory_server(
    status: u16,
    body: String,
    headers: &str,
) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let headers = headers.to_owned();
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).await.unwrap();
            if count == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..count]);
        }
        let _ = sender.send(String::from_utf8(request).unwrap());
        let _ = stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len()).as_bytes()).await;
    });
    (format!("http://{address}/v1"), receiver)
}

#[tokio::test]
async fn discovery_uses_same_private_key_and_failures_preserve_it() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (endpoint, captured) = directory_server(
        200,
        json!({"object":"list","data":[{"id":"gpt-6-astra"},{"id":"unknown-service-id"}]})
            .to_string(),
        "",
    )
    .await;
    let config = config(&root, endpoint, "gpt-6-astra-responses");
    let cache = AccountModelDiscovery::default();
    let directory = cache
        .discover(&config, Some(&root), false, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        directory.availability("gpt-6-astra"),
        ModelAvailability::DirectoryListed
    );
    assert!(matches!(
        directory.availability("gpt-6-luna"),
        ModelAvailability::Unknown { .. }
    ));
    assert!(
        ModelCatalog::defaults()
            .get("openai-api", "unknown-service-id")
            .is_none()
    );
    let headers = captured.await.unwrap().to_ascii_lowercase();
    assert!(headers.starts_with("get /v1/models "));
    assert!(headers.contains("authorization: bearer test-openai-key"));
    for (status, body, headers) in [
        (403, "secret remote diagnostic".into(), ""),
        (429, "quota".into(), ""),
        (200, "malformed".into(), ""),
        (200, "x".repeat(1024 * 1024 + 1), ""),
        (
            302,
            "".into(),
            "Location: https://unrelated.example/models\r\n",
        ),
    ] {
        let (endpoint, _) = directory_server(status, body, headers).await;
        let mut changed = config.clone();
        changed.base_url = endpoint;
        let error = cache
            .discover(&changed, Some(&root), true, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!error.contains("secret"));
        assert!(!error.contains("test-openai-key"));
        assert_eq!(
            load_api_key(&root, "OPENAI_API_KEY").unwrap().as_deref(),
            Some("test-openai-key")
        );
    }
}
