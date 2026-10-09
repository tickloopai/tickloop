use tickloop_agent_test_support::model_http_support;

use serde_json::{Value, json};
use tickloop_agent_base::image::ImageMediaType;
use tickloop_agent_model::{
    ModelDelta, ModelError, ModelRequest, StructuredOutput,
    cards::{ModelCatalog, built_in_model_profiles},
    credentials::save_api_key,
    model_client,
};
use tickloop_agent_protocol::{
    Content, ImageSource, Message, ModelConfig, ReasoningEffort, Role, ToolDefinition, Usage,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "hello".into(),
            }],
        }],
        history_sources: vec![None],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: Some(ReasoningEffort::Low),
    }
}

fn frame(parts: Value, finish: Option<&str>, usage: Value) -> String {
    let mut candidate = json!({"index":0,"content":{"role":"model","parts":parts}});
    if let Some(finish) = finish {
        candidate["finishReason"] = json!(finish);
    }
    format!(
        "data: {}\n\n",
        json!({"responseId":"google-response-1","candidates":[candidate],"usageMetadata":usage})
    )
}

fn config(root: &std::path::Path, endpoint: String) -> ModelConfig {
    let mut config = ModelCatalog::defaults()
        .get("google-gemini-api", "gemini-3.8-flash")
        .unwrap();
    config.base_url = endpoint;
    save_api_key(root, "GEMINI_API_KEY", "test-google-key").unwrap();
    config
}

#[test]
fn google_connections_use_native_protocol_and_independent_authentication() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    let visible = built_in_model_profiles();
    for provider in [
        "google-gemini-api",
        "google-gemini-oauth",
        "google-vertex-service-account",
        "google-vertex-oauth",
    ] {
        let models = visible
            .iter()
            .filter(|config| config.provider_id == provider)
            .collect::<Vec<_>>();
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|config| config.protocol
            == tickloop_agent_protocol::Protocol::Google
            && config.max_output_tokens == 65536));
        assert!(!models.iter().any(|config| config.model.contains("preview")));
    }
}

#[tokio::test]
async fn native_wire_encodes_images_thinking_schema_and_inclusive_usage() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let body = [frame(json!([{"thought":true,"text":"reason","thoughtSignature":"opaque-thought"}]), None, json!({"promptTokenCount":100,"cachedContentTokenCount":60,"thoughtsTokenCount":2})),
        frame(json!([{"text":"{\"allow\":false}","thoughtSignature":"opaque-final"}]), Some("STOP"), json!({"promptTokenCount":100,"cachedContentTokenCount":60,"thoughtsTokenCount":3,"candidatesTokenCount":5}))].concat();
    let (endpoint, captured) = model_http_support::serve_response(200, body).await;
    let client = model_client(config(root.path(), endpoint), Some(root.path())).unwrap();
    let mut request = request();
    request.messages.insert(
        0,
        Message {
            role: Role::System,
            content: vec![Content::Text {
                text: "fixture".into(),
            }],
        },
    );
    request.history_sources.insert(0, None);
    request.messages[1].content.push(Content::Image {
        source: ImageSource::Base64 {
            media_type: ImageMediaType::Png,
            data: "image-data".into(),
        },
        width: 1,
        height: 1,
    });
    request.structured_output = Some(StructuredOutput {
        name: "verdict".into(),
        schema: json!({"type":"object","properties":{"allow":{"type":"boolean"}},"required":["allow"],"additionalProperties":false}),
    });
    let (events, mut observed) = mpsc::unbounded_channel();
    let output = client
        .complete(request, events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        output.usage,
        Usage {
            input_tokens: Some(100),
            output_tokens: Some(8),
            cache_read_input_tokens: Some(60),
            cache_write_input_tokens: None
        }
    );
    let (headers, body) = captured.await.unwrap();
    assert!(headers.starts_with(
        "POST /anthropic/models/gemini-3.8-flash:streamGenerateContent?alt=sse HTTP/1.1"
    ));
    assert!(
        headers
            .to_lowercase()
            .contains("x-goog-api-key: test-google-key")
    );
    assert!(!headers.to_lowercase().contains("authorization: bearer"));
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "fixture");
    assert_eq!(
        body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "low"
    );
    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        body["generationConfig"]["responseJsonSchema"]["required"],
        json!(["allow"])
    );
    let mut final_usage = None;
    while let Some(event) = observed.recv().await {
        if let ModelDelta::Usage(usage) = event {
            final_usage = Some(usage);
        }
    }
    assert_eq!(final_usage, Some(output.usage));
}

#[tokio::test]
async fn signed_google_tool_roundtrip_survives_foreign_request_and_serialized_restore() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let signed_call = json!({"functionCall":{"name":"Echo","args":{"text":"hello"}},"thoughtSignature":"google-private-signature"});
    let first_reply = frame(json!([signed_call]), Some("STOP"), json!({}));
    let text_reply = frame(json!([{"text":"finished"}]), Some("STOP"), json!({}));
    let (endpoint, mut captured) = model_http_support::serve_responses(vec![
        (200, first_reply),
        (200, text_reply.clone()),
        (200, text_reply),
    ])
    .await;
    let config = config(root.path(), endpoint);
    let client = model_client(config.clone(), Some(root.path())).unwrap();
    let mut request = request();
    request.tools.push(ToolDefinition { name:"Echo".into(), description:"Fixture".into(), input_schema:json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}) });
    let (events, _) = mpsc::unbounded_channel();
    let first = client
        .complete(request.clone(), events.clone(), CancellationToken::new())
        .await
        .unwrap();
    let id = first
        .content
        .iter()
        .find_map(|block| {
            if let Content::ToolUse { id, .. } = block {
                Some(id.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(first.stop_reason, "tool_use");
    request.messages.push(Message {
        role: Role::Assistant,
        content: serde_json::from_str(&serde_json::to_string(&first.content).unwrap()).unwrap(),
    });
    request.history_sources.push(first.private_source.clone());
    request.messages.push(Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            tool_use_id: id,
            content: "hello".into(),
            is_error: false,
        }],
    });
    request.history_sources.push(None);
    let canonical = serde_json::to_value(&request.messages).unwrap();
    let foreign_reply = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"kimi-reply","choices":[{"index":0,"delta":{"content":"foreign"},"finish_reason":"stop"}]})
    );
    let (foreign_endpoint, foreign_capture) =
        model_http_support::serve_response(200, foreign_reply).await;
    let mut foreign = ModelCatalog::defaults()
        .get("kimi-global-api", "kimi-k3")
        .unwrap();
    foreign.base_url = foreign_endpoint;
    save_api_key(root.path(), &foreign.api_key_env, "test-kimi-key").unwrap();
    model_client(foreign, Some(root.path()))
        .unwrap()
        .complete(request.clone(), events.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !foreign_capture
            .await
            .unwrap()
            .1
            .to_string()
            .contains("google-private-signature")
    );
    let restored = model_client(config, Some(root.path())).unwrap();
    restored
        .complete(request.clone(), events.clone(), CancellationToken::new())
        .await
        .unwrap();
    restored
        .complete(request.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    let first_wire = captured.recv().await.unwrap().1;
    let second_wire = captured.recv().await.unwrap().1;
    let third_wire = captured.recv().await.unwrap().1;
    assert_eq!(second_wire["contents"][0], first_wire["contents"][0]);
    assert_eq!(second_wire["contents"][1]["parts"], json!([signed_call]));
    assert_eq!(
        second_wire["contents"][2]["parts"][0]["functionResponse"]["name"],
        "Echo"
    );
    assert_eq!(second_wire, third_wire);
    assert_eq!(serde_json::to_value(&request.messages).unwrap(), canonical);
}

#[tokio::test]
async fn google_rejects_blocked_or_incomplete_stream_and_malformed_usage() {
    for body in [
        frame(json!([{"text":"partial"}]), None, json!({})),
        frame(json!([]), Some("SAFETY"), json!({})),
        frame(
            json!([{"text":"done"}]),
            Some("STOP"),
            json!({"promptTokenCount":-1}),
        ),
    ] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let (endpoint, _) = model_http_support::serve_response(200, body).await;
        let (events, _) = mpsc::unbounded_channel();
        let result = model_client(config(root.path(), endpoint), Some(root.path()))
            .unwrap()
            .complete(request(), events, CancellationToken::new())
            .await;
        assert!(result.is_err());
    }
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let (endpoint, _) =
        model_http_support::serve_response(200, frame(json!([]), Some("STOP"), json!({}))).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (events, _) = mpsc::unbounded_channel();
    assert!(matches!(
        model_client(config(root.path(), endpoint), Some(root.path()))
            .unwrap()
            .complete(request(), events, cancel)
            .await,
        Err(ModelError::Cancelled)
    ));
}

#[tokio::test]
async fn native_usage_keeps_independent_cumulative_counts_and_reports_valid_fields_before_failure()
{
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let body = [
        frame(
            json!([{"thought":true,"text":"thought"}]),
            None,
            json!({"promptTokenCount":100,"thoughtsTokenCount":3}),
        ),
        frame(
            json!([{"text":"done"}]),
            Some("STOP"),
            json!({"candidatesTokenCount":5,"cachedContentTokenCount":60}),
        ),
    ]
    .concat();
    let (endpoint, _) = model_http_support::serve_response(200, body).await;
    let (events, _) = mpsc::unbounded_channel();
    let output = model_client(config(root.path(), endpoint), Some(root.path()))
        .unwrap()
        .complete(request(), events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(output.usage.output_tokens, Some(8));
    assert_eq!(output.usage.input_tokens, Some(100));
    let (endpoint, _) = model_http_support::serve_response(
        200,
        frame(
            json!([]),
            Some("STOP"),
            json!({"promptTokenCount":42,"candidatesTokenCount":-1,"thoughtsTokenCount":3}),
        ),
    )
    .await;
    let (events, mut observations) = mpsc::unbounded_channel();
    assert!(
        model_client(config(root.path(), endpoint), Some(root.path()))
            .unwrap()
            .complete(request(), events, CancellationToken::new())
            .await
            .is_err()
    );
    let mut known_input = None;
    while let Ok(event) = observations.try_recv() {
        if let ModelDelta::Usage(usage) = event {
            known_input = usage.input_tokens;
        }
    }
    assert_eq!(known_input, Some(42));
}

#[tokio::test]
async fn truncated_google_tool_call_never_becomes_executable_output() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let (endpoint, _) = model_http_support::serve_response(
        200,
        frame(
            json!([{"functionCall":{"name":"Echo","args":{"text":"fixture"}}}]),
            Some("MAX_TOKENS"),
            json!({}),
        ),
    )
    .await;
    let (events, _) = mpsc::unbounded_channel();
    assert!(
        model_client(config(root.path(), endpoint), Some(root.path()))
            .unwrap()
            .complete(request(), events, CancellationToken::new())
            .await
            .is_err()
    );
}
