mod model_http_support;
mod support;

use std::{collections::HashMap, path::Path};

use model_http_support::serve_responses;
use serde_json::{Value, json};
use tickloop_agent::{
    auth::copilot::CopilotAuth,
    image::ImageMediaType,
    model::copilot::CLIENT_HEADERS,
    model::{
        Content, ImageSource, InputModality, Message, ModelAuthSource, ModelConfig, ModelError,
        ModelFailureKind, ModelOutput, ModelRequest, ModelResponseRejection, Protocol,
        ReasoningEffort, Role, ToolDefinition,
        access::AccessCatalog,
        availability::{AccountModelDiscovery, ModelAvailability},
        cards::{
            Adapter, ModelCatalog, ProviderPatch, ThinkingBudgets, ThinkingControl,
            built_in_model_profiles,
        },
        connection::{ModelDiscovery, ProductKind},
        model_client, validate_reasoning_effort,
    },
    runtime::{Agent, EventKind},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PROVIDER: &str = "github-copilot";
const ENDPOINT: &str = "https://api.individual.githubcopilot.com";

#[test]
fn copilot_connection_binds_one_recommended_card_per_series() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    AccessCatalog::defaults().validate(&catalog).unwrap();
    let access = AccessCatalog::defaults().provider(PROVIDER).unwrap();
    let resolved = access
        .series
        .iter()
        .map(|series| {
            let card = catalog.get(PROVIDER, &series.model).unwrap();
            (series.label(), card.name, card.model, card.protocol)
        })
        .collect::<Vec<_>>();
    let expected = [
        (
            "Claude Sonnet (5.5)",
            "Claude Sonnet 5.5",
            "claude-sonnet-5.5",
            Protocol::Messages,
        ),
        (
            "Claude Opus (5.5)",
            "Claude Opus 5.5",
            "claude-opus-5.5",
            Protocol::Messages,
        ),
        (
            "Claude Fable (5.1)",
            "Claude Fable 5.1",
            "claude-fable-5.1",
            Protocol::Messages,
        ),
        (
            "Claude Haiku (4.5)",
            "Claude Haiku 4.5",
            "claude-haiku-4.5",
            Protocol::Messages,
        ),
        (
            "GPT Sol (6.1)",
            "GPT-6.1 Sol",
            "gpt-6.1-sol",
            Protocol::Responses,
        ),
        (
            "GPT Astra (6)",
            "GPT-6 Astra",
            "gpt-6-astra",
            Protocol::Responses,
        ),
        (
            "GPT Terra (5.6)",
            "GPT-5.6 Terra",
            "gpt-5.6-terra",
            Protocol::Responses,
        ),
        (
            "GPT Luna (6)",
            "GPT-6 Luna",
            "gpt-6-luna",
            Protocol::Responses,
        ),
    ];
    assert_eq!(
        resolved,
        expected
            .iter()
            .map(|(label, name, model, protocol)| (
                label.to_string(),
                name.to_string(),
                model.to_string(),
                *protocol
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        AccessCatalog::defaults().recommended_model(PROVIDER),
        Some("claude-sonnet-5.5")
    );

    let visible = built_in_model_profiles()
        .into_iter()
        .filter(|profile| profile.provider_id == PROVIDER)
        .collect::<Vec<_>>();
    assert_eq!(visible.len(), 8);
    for profile in &visible {
        assert_eq!(profile.base_url, ENDPOINT);
        assert_eq!(profile.auth_source, ModelAuthSource::GithubCopilot);
        assert!(profile.api_key_env.is_empty());
        assert!(matches!(
            profile.adapter(),
            Adapter::Messages | Adapter::Responses
        ));
        let card = profile.card.as_ref().unwrap();
        let gpt = profile.adapter() == Adapter::Responses;
        assert_eq!(
            card.context_window_tokens,
            gpt.then_some(272_000),
            "{}",
            profile.model
        );
        assert_eq!(card.max_output_tokens, if gpt { 128_000 } else { 32_000 });
        assert!(!profile.accepts_images());
        let thinking = match (profile.adapter(), profile.model.as_str()) {
            (Adapter::Responses, _) | (_, "claude-fable-5.1") => ThinkingControl::AlwaysOn,
            _ => ThinkingControl::Toggle,
        };
        assert_eq!(card.thinking, thinking, "{}", profile.model);
        let haiku = profile.model == "claude-haiku-4.5";
        assert_eq!(
            card.thinking_budgets,
            haiku.then_some(ThinkingBudgets {
                low: 2048,
                medium: 8192,
                high: 16384
            })
        );
        let mut levels = vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ];
        if !haiku {
            levels.extend([ReasoningEffort::Xhigh, ReasoningEffort::Max]);
        }
        assert_eq!(card.effort_levels, levels, "{}", profile.model);
        if card.thinking == ThinkingControl::AlwaysOn {
            assert!(validate_reasoning_effort(profile, Some(ReasoningEffort::Off)).is_err());
        }
    }
    let connection = visible[0].connection().unwrap();
    assert_eq!(connection.vendor, "GitHub Copilot");
    assert_eq!(connection.kind, ProductKind::Plan);
    assert_eq!(connection.discovery, ModelDiscovery::GithubCopilot);
    assert_eq!(
        connection.label(ENDPOINT),
        "Copilot subscription (api.individual.githubcopilot.com)"
    );
    assert!(access.notice.as_deref().unwrap().contains("Fable"));
}

fn patched(yaml: &str) -> Result<ModelCatalog, String> {
    let patch: ProviderPatch = serde_yaml_ng::from_str(yaml).unwrap();
    ModelCatalog::defaults().with_providers(vec![patch])
}

#[test]
fn copilot_authentication_is_bound_to_its_provider_and_adapters() {
    let foreign_auth = r#"
id: "copilot-mirror"
display_name: "Mirror"
auth: "github_copilot"
endpoints: { messages: "https://api.githubcopilot.com" }
models:
  - { id: "m", service_model_id: "claude-sonnet-5.5", display_name: "M", adapter: "messages" }
"#;
    assert!(patched(foreign_auth).is_err());
    let other_protocol = r#"
id: "github-copilot"
endpoints: { chat_completions: "https://api.githubcopilot.com" }
models:
  - { id: "chat", service_model_id: "gemini-3.8-flash", display_name: "Chat", adapter: "chat_completions" }
"#;
    assert!(patched(other_protocol).is_err());
    let foreign_endpoint = r#"
id: "github-copilot"
endpoints: { messages: "https://gateway.example" }
"#;
    assert!(patched(foreign_endpoint).is_err());
    let api_key_on_copilot = r#"
id: "github-copilot"
auth: "api_key_env"
api_key_env: "GITHUB_TOKEN"
"#;
    assert!(patched(api_key_on_copilot).is_err());
}

const COPILOT_SESSION_TOKEN: &str = "tid=test;exp=4102444800;sku=copilot_pro";

fn issuer_reply(index: usize) -> serde_json::Value {
    match index {
        0 => {
            json!({"device_code":"device","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":0})
        }
        1 => json!({"access_token":"test-github-user-token","token_type":"bearer"}),
        // No `proxy-ep`, so requests keep the configured (loopback) base URL.
        _ => json!({"token":COPILOT_SESSION_TOKEN,"expires_at":4102444800u64}),
    }
}

/// Signs in and exchanges the Copilot token through a loopback GitHub issuer, so the real
/// client reads the saved session without further sign-in traffic.
async fn sign_in(root: &Path) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let issuer = tokio::spawn(async move {
        for index in 0..3 {
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
                .unwrap_or(0);
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).await.unwrap();
            let body = issuer_reply(index).to_string();
            reader.into_inner().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let auth = CopilotAuth::new_for_loopback_test(root, &origin).unwrap();
    let cancel = CancellationToken::new();
    let login = auth.start_device_login(&cancel).await.unwrap();
    auth.finish_device_login(login, &cancel).await.unwrap();
    auth.resolve_access(&cancel).await.unwrap();
    issuer.await.unwrap();
}

fn configured(model: &str, endpoint: &str) -> ModelConfig {
    let mut config = ModelCatalog::defaults().get(PROVIDER, model).unwrap();
    config.base_url = endpoint.trim_end_matches("/anthropic").into();
    config
}

fn header_map(raw: &str) -> HashMap<String, String> {
    raw.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

fn request_line(raw: &str) -> &str {
    raw.lines().next().unwrap()
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text: text.into() }],
    }
}

fn request(messages: Vec<Message>, effort: Option<ReasoningEffort>) -> ModelRequest {
    ModelRequest {
        history_sources: vec![None; messages.len()],
        messages,
        tools: vec![ToolDefinition {
            name: "Echo".into(),
            description: "Inert fixture".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
        }],
        structured_output: None,
        temperature: None,
        reasoning_effort: effort,
    }
}

fn messages_reply(text: &str) -> String {
    let frame = |kind: &str, mut value: Value| {
        value["type"] = json!(kind);
        format!("event: {kind}\ndata: {value}\n\n")
    };
    [
        frame("message_start", json!({"message":{"role":"assistant","content":[],"usage":{"input_tokens":900,"cache_read_input_tokens":600,"cache_creation_input_tokens":0}}})),
        frame("content_block_start", json!({"index":0,"content_block":{"type":"text","text":text}})),
        frame("content_block_stop", json!({"index":0})),
        frame("message_delta", json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}})),
        frame("message_stop", json!({})),
    ]
    .concat()
}

/// Copilot gives the response and each item a new opaque id on every event, as observed on
/// 2026-10-01.
fn rotating_responses_reply(output: Vec<Value>) -> String {
    let mut frames = Vec::new();
    let mut sequence = 0;
    let mut event = |kind: &str, mut body: Value| {
        body["type"] = json!(kind);
        body["sequence_number"] = json!(sequence);
        sequence += 1;
        frames.push(format!("event: {kind}\ndata: {body}\n\n"));
    };
    let rotated = |item: &Value, turn: &str| {
        let mut item = item.clone();
        item["id"] = json!(format!("opaque-{turn}"));
        if item["type"] == "reasoning" {
            item["encrypted_content"] = json!(format!("sealed-{turn}"));
        }
        item
    };
    event(
        "response.created",
        json!({"response":{"id":"resp-opaque-created","status":"in_progress"}}),
    );
    let mut completed = Vec::new();
    for (index, item) in output.iter().enumerate() {
        let mut initial = rotated(item, &format!("{index}-added"));
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
        if item["type"] == "message" {
            event(
                "response.output_text.delta",
                json!({"output_index":index,"item_id":format!("opaque-{index}-delta"),"content_index":0,"delta":item["content"][0]["text"]}),
            );
        }
        if item["type"] == "function_call" {
            event(
                "response.function_call_arguments.delta",
                json!({"output_index":index,"item_id":format!("opaque-{index}-arguments"),"delta":item["arguments"]}),
            );
        }
        event(
            "response.output_item.done",
            json!({"output_index":index,"item":rotated(item, &format!("{index}-done"))}),
        );
        completed.push(rotated(item, &format!("{index}-completed")));
    }
    event(
        "response.completed",
        json!({"response":{"id":"resp-opaque-completed","status":"completed","output":completed,"usage":{"input_tokens":900,"output_tokens":12,"input_tokens_details":{"cached_tokens":600}}}}),
    );
    frames.concat()
}

fn responses_reply(output: Vec<Value>) -> String {
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
        json!({"response":{"id":"resp-copilot","status":"in_progress"}}),
    );
    for (index, item) in output.iter().enumerate() {
        let mut initial = item.clone();
        if item["type"] == "message" {
            initial["content"] = json!([]);
        }
        event(
            "response.output_item.added",
            json!({"output_index":index,"item":initial}),
        );
        event(
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        );
    }
    event(
        "response.completed",
        json!({"response":{"id":"resp-copilot","status":"completed","output":output,"usage":{"input_tokens":900,"output_tokens":12,"input_tokens_details":{"cached_tokens":600}}}}),
    );
    frames.concat()
}

async fn complete(
    config: ModelConfig,
    root: &Path,
    request: ModelRequest,
) -> Result<ModelOutput, ModelError> {
    let client = model_client(config, Some(root)).unwrap();
    let (events, _receiver) = mpsc::unbounded_channel();
    client
        .complete_for_chat(Uuid::new_v4(), request, events, CancellationToken::new())
        .await
}

#[tokio::test]
async fn claude_requests_use_the_copilot_token_and_client_identity() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let (endpoint, mut captured) = serve_responses(vec![
        (200, messages_reply("Hello")),
        (200, messages_reply("Done")),
    ])
    .await;
    let config = configured("claude-sonnet-5.5", &endpoint);
    let first = request(vec![user("Hello")], Some(ReasoningEffort::Medium));
    let output = complete(config.clone(), &root, first).await.unwrap();
    assert!(
        output
            .content
            .iter()
            .any(|block| matches!(block, Content::Text { text } if text == "Hello"))
    );
    let (raw, body) = captured.recv().await.unwrap();
    assert_eq!(request_line(&raw), "POST /v1/messages HTTP/1.1");
    let headers = header_map(&raw);
    assert_eq!(
        headers["authorization"],
        format!("Bearer {COPILOT_SESSION_TOKEN}")
    );
    assert!(!headers.contains_key("x-api-key"));
    for (name, value) in CLIENT_HEADERS {
        assert_eq!(headers[name], value, "{name}");
    }
    assert_eq!(headers["openai-intent"], "conversation-edits");
    assert!(!headers.contains_key("x-github-api-version"));
    assert_eq!(headers["x-initiator"], "user");
    assert!(!headers.contains_key("copilot-vision-request"));
    assert_eq!(body["model"], "claude-sonnet-5.5");
    assert_eq!(body["thinking"], json!({"type":"adaptive"}));
    assert_eq!(body["output_config"]["effort"], "medium");
    assert!(body["tools"][0].get("eager_input_streaming").is_none());

    let continuation = request(
        vec![
            user("Use Echo"),
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "toolu_1".into(),
                    name: "Echo".into(),
                    input: json!({"text":"hi"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "hi".into(),
                    is_error: false,
                }],
            },
        ],
        Some(ReasoningEffort::Off),
    );
    complete(config, &root, continuation).await.unwrap();
    let (raw, body) = captured.recv().await.unwrap();
    assert_eq!(header_map(&raw)["x-initiator"], "agent");
    assert_eq!(body["thinking"], json!({"type":"disabled"}));
}

#[tokio::test]
async fn copilot_raw_reasoning_prefix_completion_delivers_normal_answers_and_rejects_conflicts() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    for added_prefix in [true, false] {
        for completed in ["Thinking", "Contradictory"] {
            let (url, mut received) = serve_responses(vec![(
                200,
                model_http_support::raw_reasoning_prefix_reply(added_prefix, completed, true),
            )])
            .await;
            let client = model_client(configured("gpt-6-luna", &url), Some(&root)).unwrap();
            let (events, mut deltas) = mpsc::unbounded_channel();
            let result = client
                .complete(
                    request(vec![user("Hello")], None),
                    events,
                    CancellationToken::new(),
                )
                .await;
            received.recv().await.unwrap();
            if completed == "Contradictory" {
                assert!(
                    matches!(result, Err(ModelError::InvalidResponse(_))),
                    "inconsistent Copilot reasoning must fail"
                );
                continue;
            }
            let output =
                result.expect("Copilot reasoning prefixes can complete across rotating item IDs");
            let mut thinking = String::new();
            let mut text = String::new();
            while let Ok(delta) = deltas.try_recv() {
                match delta {
                    tickloop_agent::model::ModelDelta::Thinking(value) => thinking.push_str(&value),
                    tickloop_agent::model::ModelDelta::Text(value) => text.push_str(&value),
                    _ => {}
                }
            }
            assert_eq!(thinking, "Thinking");
            assert_eq!(text, "Answer");
            assert!(
                matches!(&output.content[0], Content::ProtocolData { value, .. } if value["content"][0]["text"] == "Thinking" && value["encrypted_content"] == "opaque-completed")
            );
        }
    }
}

#[tokio::test]
async fn gpt_requests_use_responses_without_replaying_connection_bound_reasoning() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let reasoning = json!({"type":"reasoning","id":"rs_bound_to_connection","summary":[{"type":"summary_text","text":"Thinking"}],"encrypted_content":"opaque"});
    let (endpoint, mut captured) = serve_responses(vec![
        (200, responses_reply(vec![
            reasoning.clone(),
            json!({"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"First"}]}),
        ])),
        (200, responses_reply(vec![
            json!({"type":"message","id":"msg_2","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Second"}]}),
        ])),
    ])
    .await;
    let config = configured("gpt-6-luna", &endpoint);
    let first = complete(
        config.clone(),
        &root,
        request(vec![user("One")], Some(ReasoningEffort::High)),
    )
    .await
    .unwrap();
    let (raw, body) = captured.recv().await.unwrap();
    assert_eq!(request_line(&raw), "POST /responses HTTP/1.1");
    let headers = header_map(&raw);
    assert_eq!(
        headers["authorization"],
        format!("Bearer {COPILOT_SESSION_TOKEN}")
    );
    assert_eq!(headers["x-initiator"], "user");
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(
        first
            .content
            .iter()
            .any(|block| matches!(block, Content::ProtocolData { .. }))
    );

    let mut history = vec![
        user("One"),
        Message {
            role: Role::Assistant,
            content: first.content.clone(),
        },
        user("Two"),
    ];
    let mut follow_up = request(std::mem::take(&mut history), Some(ReasoningEffort::Low));
    follow_up.history_sources = vec![None, first.private_source.clone(), None];
    complete(config, &root, follow_up).await.unwrap();
    let (_, body) = captured.recv().await.unwrap();
    let input = body["input"].as_array().unwrap();
    assert!(
        input.iter().all(|item| item["type"] != "reasoning"),
        "{input:?}"
    );
    assert!(
        input
            .iter()
            .any(|item| item["role"] == "assistant" && item["content"] == "First")
    );
    assert!(!body.to_string().contains("opaque"));
}

#[tokio::test]
async fn images_set_the_copilot_vision_header() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let (endpoint, mut captured) = serve_responses(vec![(200, messages_reply("Seen"))]).await;
    let mut config = configured("claude-sonnet-5.5", &endpoint);
    config.input_modalities.push(InputModality::Image);
    config
        .card
        .as_mut()
        .unwrap()
        .input_modalities
        .push(InputModality::Image);
    let mut with_image = request(vec![user("Look")], None);
    with_image.messages[0].content.push(Content::Image {
        source: ImageSource::Base64 {
            media_type: ImageMediaType::Png,
            data: "aW1hZ2U=".into(),
        },
        width: 1,
        height: 1,
    });
    complete(config, &root, with_image).await.unwrap();
    let (raw, _) = captured.recv().await.unwrap();
    assert_eq!(header_map(&raw)["copilot-vision-request"], "true");
}

#[tokio::test]
async fn missing_sign_in_fails_before_any_request_and_prompt_overflow_is_a_context_limit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (endpoint, mut captured) = serve_responses(vec![(
        400,
        json!({"error":{"message":"prompt token count of 140000 exceeds the limit of 128000","code":"model_max_prompt_tokens_exceeded"}}).to_string(),
    )])
    .await;
    let config = configured("claude-opus-5.5", &endpoint);
    let error = complete(config.clone(), &root, request(vec![user("Hi")], None))
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::CopilotNotLoggedIn), "{error:?}");
    assert!(captured.try_recv().is_err());

    sign_in(&root).await;
    let error = complete(config, &root, request(vec![user("Hi")], None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ModelError::AtStage { error, .. }
                if matches!(**error, ModelError::ResponseRejected(ModelResponseRejection::ContextLimit))
        ),
        "{error:?}"
    );
}

/// Answers one GET with the given status and JSON body, returning the raw request head.
async fn directory_server(
    status: u16,
    body: String,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut head = String::new();
        while !head.ends_with("\r\n\r\n") {
            assert_ne!(reader.read_line(&mut head).await.unwrap(), 0);
        }
        let _ = sender.send(head);
        reader.into_inner().write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    (format!("http://{address}"), receiver)
}

#[tokio::test]
async fn account_directory_reports_listed_disabled_unconfigured_and_absent_models() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let listing = json!({"object":"list","data":[
        {"id":"claude-sonnet-5.5","object":"model","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"type":"chat"},"supported_endpoints":["/v1/messages","/chat/completions"]},
        {"id":"gpt-6-luna","object":"model","model_picker_enabled":true,"capabilities":{"type":"chat"},"supported_endpoints":["/responses"]},
        {"id":"claude-opus-5.5","object":"model","model_picker_enabled":true,"policy":{"state":"disabled"},"capabilities":{"type":"chat"}},
        {"id":"claude-fable-5.1","object":"model","model_picker_enabled":false,"policy":{"state":"unconfigured"},"capabilities":{"type":"chat"}},
        {"id":"text-embedding-3-small","object":"model","capabilities":{"type":"embeddings"}},
        {"id":"codename-evaluation","object":"model","model_picker_enabled":true,"capabilities":{"type":"chat"}}
    ]});
    let (endpoint, captured) = directory_server(200, listing.to_string()).await;
    let config = configured("claude-sonnet-5.5", &endpoint);
    let discovery = AccountModelDiscovery::default();
    let found = discovery
        .discover(&config, Some(&root), false, &CancellationToken::new())
        .await
        .unwrap();
    let raw = captured.await.unwrap();
    assert_eq!(request_line(&raw), "GET /models HTTP/1.1");
    let headers = header_map(&raw);
    assert_eq!(
        headers["authorization"],
        format!("Bearer {COPILOT_SESSION_TOKEN}")
    );
    assert_eq!(headers["x-github-api-version"], "2026-06-01");
    assert_eq!(headers["copilot-integration-id"], "vscode-chat");
    assert_eq!(
        found.availability("claude-sonnet-5.5"),
        ModelAvailability::DirectoryListed
    );
    assert_eq!(
        found.availability("gpt-6-luna"),
        ModelAvailability::DirectoryListed
    );
    assert!(!found.availability("claude-opus-5.5").selectable());
    let unconfigured = found.availability("claude-fable-5.1");
    assert!(unconfigured.selectable() && unconfigured != ModelAvailability::DirectoryListed);
    assert!(matches!(
        found.availability("gpt-6.1-sol"),
        ModelAvailability::Unknown { .. }
    ));
    assert!(!found.models.contains_key("text-embedding-3-small"));
    assert!(found.models.contains_key("codename-evaluation"));
    assert!(
        ModelCatalog::defaults()
            .get(PROVIDER, "codename-evaluation")
            .is_none()
    );
    assert!(
        built_in_model_profiles()
            .iter()
            .all(|profile| profile.model != "codename-evaluation")
    );
}

#[tokio::test]
async fn directory_failures_keep_the_sign_in_and_require_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (endpoint, _) = directory_server(403, "{}".into()).await;
    let config = configured("gpt-6-luna", &endpoint);
    let discovery = AccountModelDiscovery::default();
    let cancel = CancellationToken::new();
    assert!(
        discovery
            .discover(&config, Some(&root), false, &cancel)
            .await
            .is_err()
    );
    sign_in(&root).await;
    let error = discovery
        .discover(&config, Some(&root), false, &cancel)
        .await
        .unwrap_err();
    assert!(error.contains("403"), "{error}");
    assert!(CopilotAuth::new(&root).has_saved_login().unwrap());
}

fn signed_claude_reply(answer: &str) -> String {
    let frame = |kind: &str, mut data: Value| {
        data["type"] = json!(kind);
        format!("event: {kind}\ndata: {data}\n\n")
    };
    [
        frame("message_start", json!({"message":{"role":"assistant","content":[]}})),
        frame("content_block_start", json!({"index":0,"content_block":{"type":"thinking","thinking":"Claude reasoning","signature":"claude-signed-block"}})),
        frame("content_block_stop", json!({"index":0})),
        frame("content_block_start", json!({"index":1,"content_block":{"type":"text","text":answer}})),
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
        let event = support::next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => return,
            EventKind::Failed(error) => panic!("{prompt}: {error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn claude_gpt_restart_claude_keeps_history_sources_and_same_source_signatures() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("state");
    std::fs::create_dir_all(&root).unwrap();
    sign_in(&root).await;
    let (claude_url, mut claude_requests) = serve_responses(vec![
        (200, signed_claude_reply("Claude first")),
        (200, signed_claude_reply("Claude second")),
        (200, signed_claude_reply("Claude returned")),
    ])
    .await;
    let (gpt_url, mut gpt_requests) = serve_responses(vec![(
        200,
        responses_reply(vec![
            json!({"type":"reasoning","id":"rs_gpt","summary":[{"type":"summary_text","text":"GPT summary"}],"encrypted_content":"gpt-opaque"}),
            json!({"type":"message","id":"msg_gpt","role":"assistant","status":"completed","content":[{"type":"output_text","text":"GPT answer"}]}),
        ]),
    )])
    .await;
    let claude = configured("claude-sonnet-5.5", &claude_url);
    let gpt = configured("gpt-6-luna", &gpt_url);
    let agent = Agent::open_embedded(
        root.clone(),
        claude.clone(),
        model_client(claude.clone(), Some(&root)).unwrap(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    finish_turn(&agent, chat, "First prompt").await;
    finish_turn(&agent, chat, "Second prompt").await;
    agent
        .set_model(
            chat,
            gpt.clone(),
            model_client(gpt.clone(), Some(&root)).unwrap(),
        )
        .await
        .unwrap();
    finish_turn(&agent, chat, "Use GPT").await;
    let mixed = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;

    let resumed = Agent::open_embedded(
        root.clone(),
        gpt.clone(),
        model_client(gpt, Some(&root)).unwrap(),
    )
    .unwrap();
    let restored = resumed.resume_chat(chat).await.unwrap();
    assert_eq!(restored.messages, mixed.messages);
    assert_eq!(restored.message_sources, mixed.message_sources);
    resumed
        .set_model(
            chat,
            claude.clone(),
            model_client(claude, Some(&root)).unwrap(),
        )
        .await
        .unwrap();
    finish_turn(&resumed, chat, "Return to Claude").await;
    resumed.shutdown().await;

    claude_requests.recv().await.unwrap();
    let (_, second) = claude_requests.recv().await.unwrap();
    let (_, returned) = claude_requests.recv().await.unwrap();
    let (_, gpt_request) = gpt_requests.recv().await.unwrap();
    let first_assistant = |body: &Value| {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "assistant")
            .cloned()
            .unwrap()
    };
    assert_eq!(first_assistant(&second), first_assistant(&returned));
    assert!(
        first_assistant(&second)
            .to_string()
            .contains("claude-signed-block")
    );
    let returned_text = returned.to_string();
    assert!(returned_text.contains("GPT answer"));
    assert!(!returned_text.contains("gpt-opaque"));
    let gpt_text = gpt_request.to_string();
    assert!(!gpt_text.contains("claude-signed-block"));
    assert!(gpt_text.contains("Claude reasoning"));
    assert!(gpt_text.contains("Claude second"));
}

#[tokio::test]
async fn revoked_sign_in_and_entitlement_refusals_are_not_retried_as_service_errors() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let (endpoint, _) = serve_responses(vec![
        (
            401,
            json!({"error":{"message":"bad credentials"}}).to_string(),
        ),
        (
            403,
            json!({"error":{"message":"model not enabled for this plan"}}).to_string(),
        ),
        (
            400,
            json!({"error":{"message":"The requested model is not supported.","code":"model_not_supported"}}).to_string(),
        ),
    ])
    .await;
    let config = configured("claude-opus-5.5", &endpoint);
    let revoked = complete(config.clone(), &root, request(vec![user("Hi")], None))
        .await
        .unwrap_err();
    assert!(
        matches!(&revoked, ModelError::AtStage { error, .. } if matches!(**error, ModelError::CopilotAuthentication)),
        "{revoked:?}"
    );
    assert_eq!(revoked.kind(), ModelFailureKind::AuthenticationFailed);
    let refused = complete(config.clone(), &root, request(vec![user("Hi")], None))
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), ModelFailureKind::EntitlementDenied);
    assert!(!refused.retry_hint().0);
    assert!(!refused.to_string().contains("model not enabled"));
    let not_enabled = complete(config, &root, request(vec![user("Hi")], None))
        .await
        .unwrap_err();
    assert_eq!(not_enabled.kind(), ModelFailureKind::EntitlementDenied);
    assert!(!not_enabled.retry_hint().0);
    assert!(
        not_enabled
            .to_string()
            .contains("plan, organization policy or client")
    );
}

#[tokio::test]
async fn copilot_responses_streams_with_rotating_item_ids_are_matched_by_position() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let (endpoint, _) = serve_responses(vec![(
        200,
        rotating_responses_reply(vec![
            json!({"type":"reasoning","summary":[]}),
            json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Calling Echo"}]}),
            json!({"type":"function_call","call_id":"call_echo","name":"Echo","arguments":"{\"text\":\"hi\"}","status":"completed"}),
        ]),
    )])
    .await;
    let output = complete(
        configured("gpt-6-luna", &endpoint),
        &root,
        request(vec![user("Use Echo")], None),
    )
    .await
    .unwrap();
    assert!(
        output
            .content
            .iter()
            .any(|block| matches!(block, Content::Text { text } if text == "Calling Echo"))
    );
    assert!(output.content.iter().any(|block| matches!(
        block,
        Content::ToolUse { id, name, input } if id == "call_echo" && name == "Echo" && input["text"] == "hi"
    )));
}

/// Shaped like Herdr's ModifyTick: each action is a top-level `oneOf` variant.
fn variant_tool() -> ToolDefinition {
    ToolDefinition {
        name: "ModifyTick".into(),
        description: "Open, move, send input or close.".into(),
        input_schema: json!({"type":"object","oneOf":[
            {"type":"object","properties":{"action":{"const":"open"},"command":{"type":"array","items":{"type":"string"}},"placement":{"type":"object","oneOf":[{"type":"object","properties":{"type":{"const":"new_tab"}}}]}},"required":["action","command"],"additionalProperties":false},
            {"type":"object","properties":{"action":{"const":"move"},"tick_instance_id":{"type":"string"}},"required":["action","tick_instance_id"],"additionalProperties":false},
            {"type":"object","properties":{"tick_instance_id":{"type":"string"},"action":{"type":"string","enum":["send_input","close"]},"text":{"type":"string"}},"required":["tick_instance_id","action"],"additionalProperties":false}
        ]}),
    }
}

#[tokio::test]
async fn messages_flattens_top_level_schema_variants_that_anthropic_rejects() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let (claude_url, mut claude_requests) =
        serve_responses(vec![(200, messages_reply("ok"))]).await;
    let (gpt_url, mut gpt_requests) = serve_responses(vec![(
        200,
        responses_reply(vec![json!({"type":"message","id":"msg","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok"}]})]),
    )])
    .await;
    let mut with_variants = request(vec![user("Hi")], None);
    with_variants.tools = vec![variant_tool()];
    complete(
        configured("claude-haiku-4.5", &claude_url),
        &root,
        with_variants.clone(),
    )
    .await
    .unwrap();
    complete(configured("gpt-6-luna", &gpt_url), &root, with_variants)
        .await
        .unwrap();
    let (_, claude) = claude_requests.recv().await.unwrap();
    assert_eq!(
        claude["tools"][0]["input_schema"],
        json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["open","move","send_input","close"]},
                "command":{"type":"array","items":{"type":"string"}},
                "placement":{"type":"object","oneOf":[{"type":"object","properties":{"type":{"const":"new_tab"}}}]},
                "tick_instance_id":{"type":"string"},
                "text":{"type":"string"}
            },
            "required":["action"],
            "additionalProperties":false
        })
    );
    let (_, gpt) = gpt_requests.recv().await.unwrap();
    assert_eq!(gpt["tools"][0]["parameters"], variant_tool().input_schema);
}

#[tokio::test]
async fn budget_thinking_sends_budget_tokens_and_drops_temperature_while_thinking() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let (endpoint, mut captured) = serve_responses(vec![
        (200, messages_reply("high")),
        (200, messages_reply("off")),
        (200, messages_reply("review")),
    ])
    .await;
    let mut high = request(vec![user("Hi")], Some(ReasoningEffort::High));
    high.temperature = Some(0.0);
    complete(configured("claude-haiku-4.5", &endpoint), &root, high)
        .await
        .unwrap();
    let (_, body) = captured.recv().await.unwrap();
    assert_eq!(
        body["thinking"],
        json!({"type":"enabled","budget_tokens":16384})
    );
    assert!(body.get("output_config").is_none());
    assert!(body.get("temperature").is_none());
    assert_eq!(body["max_tokens"], 32000);

    let mut off = request(vec![user("Hi")], Some(ReasoningEffort::Off));
    off.temperature = Some(0.0);
    complete(configured("claude-haiku-4.5", &endpoint), &root, off)
        .await
        .unwrap();
    let (_, body) = captured.recv().await.unwrap();
    assert_eq!(body["thinking"], json!({"type":"disabled"}));
    assert_eq!(body["temperature"], 0.0);

    // An auto-approval review asks for temperature 0 with the model's default thinking.
    let mut review = request(vec![user("Review")], None);
    review.temperature = Some(0.0);
    complete(configured("claude-sonnet-5.5", &endpoint), &root, review)
        .await
        .unwrap();
    let (_, body) = captured.recv().await.unwrap();
    assert_eq!(body["thinking"], json!({"type":"adaptive"}));
    assert!(body.get("temperature").is_none());
}

#[tokio::test]
async fn budget_is_capped_to_leave_room_for_the_answer() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    sign_in(&root).await;
    let (endpoint, mut captured) = serve_responses(vec![(200, messages_reply("ok"))]).await;
    let mut config = configured("claude-haiku-4.5", &endpoint);
    config.max_output_tokens = 8192;
    config.card.as_mut().unwrap().max_output_tokens = 8192;
    complete(
        config,
        &root,
        request(vec![user("Hi")], Some(ReasoningEffort::High)),
    )
    .await
    .unwrap();
    let (_, body) = captured.recv().await.unwrap();
    assert_eq!(body["thinking"]["budget_tokens"], 8192 - 1024);
}

#[test]
fn thinking_budgets_are_validated_and_survive_provider_patches() {
    let override_budgets = r#"
id: "github-copilot"
models:
  - id: "claude-haiku-4.5"
    thinking_budgets: { low: 1024, medium: 4096, high: 12000 }
"#;
    let catalog = patched(override_budgets).unwrap();
    let haiku = catalog.get(PROVIDER, "claude-haiku-4.5").unwrap();
    assert_eq!(
        haiku.card.unwrap().thinking_budgets,
        Some(ThinkingBudgets {
            low: 1024,
            medium: 4096,
            high: 12000
        })
    );
    for invalid in [
        r#"{ id: "claude-haiku-4.5", thinking_budgets: { low: 512, medium: 4096, high: 12000 } }"#,
        r#"{ id: "claude-haiku-4.5", effort_levels: ["low", "medium", "high", "max"] }"#,
        r#"{ id: "claude-sonnet-5.5", thinking_budgets: { low: 2048, medium: 8192, high: 16384 } }"#,
        r#"{ id: "gpt-6-luna", thinking_budgets: { low: 2048, medium: 8192, high: 16384 } }"#,
        r#"{ id: "claude-haiku-4.5", thinking: "unsupported", effort_levels: [] }"#,
    ] {
        let yaml = format!("id: \"github-copilot\"\nmodels:\n  - {invalid}\n");
        assert!(patched(&yaml).is_err(), "{invalid}");
    }
}
