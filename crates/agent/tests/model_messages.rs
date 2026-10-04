use serde_json::{Value, json};
use tickloop_agent::{
    image::ImageMediaType,
    model::{
        Content, HttpModelClient, ImageSource, InputModality, Message, ModelClient, ModelConfig,
        ModelDelta, ModelError, ModelRequest, Protocol, Role, ToolDefinition, ToolResultBlock,
        ToolResultContent, Usage,
    },
};
mod model_http_support;
use model_http_support::serve_response;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn reports_authentication_and_rate_limits_without_echoing_credentials() {
    for (status, expected) in [
        (401, "expired or invalid"),
        (403, "HTTP 403"),
        (429, "Rate limit"),
        (500, "HTTP 500"),
    ] {
        let (base_url, _) = serve_response(
            status,
            "Request refused; Authorization: Bearer secret-api-key".into(),
        )
        .await;
        let client = HttpModelClient::new(configuration(base_url)).unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(
                ModelRequest {
                    history_sources: Vec::new(),
                    messages: vec![message(Role::User, "Hello")],
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
        assert!(error.to_string().contains(expected), "{error}");
        assert!(error.to_string().contains(&format!("HTTP {status}")));
        assert!(error.to_string().contains("Request refused"));
        assert!(!format!("{error:?}").contains("secret-api-key"));
    }
}

#[test]
fn rejects_unsafe_or_unusable_configuration_before_starting_a_request() {
    for base_url in [
        "not a URL",
        "ftp://example.test",
        "http://example.test",
        "https://secret@example.test",
        "https://example.test?api_key=secret",
        "https://example.test#secret",
    ] {
        assert!(
            matches!(
                HttpModelClient::new(configuration(base_url.into())),
                Err(ModelError::Configuration(_))
            ),
            "accepted {base_url}"
        );
    }
    let mut missing_key = configuration("https://api.deepseek.com/anthropic".into());
    missing_key.api_key_env = "TICKLOOP_MODEL_TEST_UNSET_KEY_812730".into();
    assert!(matches!(
        HttpModelClient::new(missing_key),
        Err(ModelError::Configuration(_))
    ));
    let mut zero_tokens = configuration("http://127.0.0.1:9999".into());
    zero_tokens.max_output_tokens = 0;
    assert!(matches!(
        HttpModelClient::new(zero_tokens),
        Err(ModelError::Configuration(_))
    ));
}

#[tokio::test]
async fn cancelled_requests_do_not_produce_a_completed_response() {
    let (base_url, _) = serve_response(200, text_response()).await;
    let client = HttpModelClient::new(configuration(base_url)).unwrap();
    let (events, mut receiver) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![message(Role::User, "Hello")],
                tools: vec![],
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            cancel,
        )
        .await;
    assert!(matches!(result, Err(ModelError::Cancelled)));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn incomplete_or_duplicate_tool_streams_never_return_executable_calls() {
    let start = event(
        "message_start",
        json!({"message":{"role":"assistant","content":[]}}),
    );
    let tool = |index| {
        event(
            "content_block_start",
            json!({"index":index,"content_block":{"type":"tool_use","id":"call-1","name":"Read","input":{}}}),
        )
    };
    let stop = |index| event("content_block_stop", json!({"index":index}));
    let end = [
        event("message_delta", json!({"delta":{"stop_reason":"tool_use"}})),
        event("message_stop", json!({})),
    ]
    .concat();
    let incomplete = event(
        "content_block_delta",
        json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}}),
    );
    let streams = [
        [
            start.clone(),
            tool(0),
            incomplete.clone(),
            stop(0),
            end.clone(),
        ]
        .concat(),
        [
            start.clone(),
            tool(0),
            stop(0),
            tool(1),
            stop(1),
            end.clone(),
        ]
        .concat(),
        [start.clone(), tool(0), incomplete, end.clone()].concat(),
        [start, tool(0), stop(0), end.replace("tool_use", "end_turn")].concat(),
        text_response().replace(&event("message_stop", json!({})), ""),
        text_response().replace("event: message_stop", "event: error"),
    ];
    for body in streams {
        let (base_url, _) = serve_response(200, body).await;
        let client = HttpModelClient::new(configuration(base_url)).unwrap();
        let (events, _) = mpsc::unbounded_channel();
        assert!(
            client
                .complete(
                    ModelRequest {
                        history_sources: Vec::new(),
                        messages: vec![message(Role::User, "Read notes")],
                        tools: vec![],
                        structured_output: None,
                        temperature: None,
                        reasoning_effort: None,
                    },
                    events,
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn assembles_complete_tool_arguments_and_preserves_thinking_for_the_next_turn() {
    let body = [
        event("message_start", json!({"message":{"role":"assistant","content":[]}})),
        event("content_block_start", json!({"index":0,"content_block":{"type":"thinking","thinking":""}})),
        event("content_block_delta", json!({"index":0,"delta":{"type":"thinking_delta","thinking":"I should read the notes."}})),
        event("content_block_delta", json!({"index":0,"delta":{"type":"signature_delta","signature":"signature-1"}})),
        event("content_block_stop", json!({"index":0})),
        event("content_block_start", json!({"index":1,"content_block":{"type":"tool_use","id":"call-1","name":"Read","input":{}}})),
        event("content_block_delta", json!({"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}})),
        event("content_block_delta", json!({"index":1,"delta":{"type":"input_json_delta","partial_json":"\"notes.md\"}"}})),
        event("content_block_stop", json!({"index":1})),
        event("message_delta", json!({"delta":{"stop_reason":"tool_use"}})),
        event("message_stop", json!({})),
    ].concat();
    let (base_url, request_receiver) = serve_response(200, body).await;
    let client = HttpModelClient::new(configuration(base_url)).unwrap();
    let tools = vec![ToolDefinition {
        name: "Read".into(),
        description: "Read a text file".into(),
        input_schema: json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}),
    }];
    let (events, mut receiver) = mpsc::unbounded_channel();
    let output = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![message(Role::User, "Read notes")],
                tools: tools.clone(),
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        output.content,
        vec![
            Content::Thinking {
                thinking: "I should read the notes.".into(),
                signature: Some("signature-1".into())
            },
            Content::ToolUse {
                id: "call-1".into(),
                name: "Read".into(),
                input: json!({"file_path":"notes.md"})
            },
        ]
    );
    assert_eq!(
        receiver.recv().await,
        Some(ModelDelta::Thinking("I should read the notes.".into()))
    );
    assert_eq!(output.usage.input_tokens, None);
    assert_eq!(output.usage.output_tokens, None);
    assert_eq!(output.usage.cache_read_input_tokens, None);
    assert_eq!(output.usage.cache_write_input_tokens, None);
    assert_eq!(
        request_receiver.await.unwrap().1["tools"][0]["input_schema"],
        tools[0].input_schema
    );

    let (base_url, next_request) = serve_response(200, text_response()).await;
    let client = HttpModelClient::new(configuration(base_url)).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![
                    message(Role::User, "Read notes"),
                    Message {
                        role: Role::Assistant,
                        content: output.content,
                    },
                    Message {
                        role: Role::User,
                        content: vec![Content::ToolResult {
                            tool_use_id: "call-1".into(),
                            content: "Notes content".into(),
                            is_error: false,
                        }],
                    },
                ],
                tools,
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = next_request.await.unwrap().1;
    assert_eq!(
        request["messages"][1]["content"][0]["signature"],
        "signature-1"
    );
    assert_eq!(
        request["messages"][2]["content"][0],
        json!({"type":"tool_result","tool_use_id":"call-1","content":"Notes content","is_error":false})
    );
}

#[tokio::test]
async fn tool_results_serialize_to_the_anthropic_string_or_block_array_shape() {
    let (base_url, captured) = serve_response(200, text_response()).await;
    let client = HttpModelClient::new(configuration(base_url)).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![
                    message(Role::User, "Read both"),
                    Message {
                        role: Role::Assistant,
                        content: vec![
                            Content::ToolUse {
                                id: "call-1".into(),
                                name: "read_file".into(),
                                input: json!({"path":"first.md"}),
                            },
                            Content::ToolUse {
                                id: "call-2".into(),
                                name: "read_file".into(),
                                input: json!({"path":"second.md"}),
                            },
                        ],
                    },
                    Message {
                        role: Role::User,
                        content: vec![
                            Content::ToolResult {
                                tool_use_id: "call-1".into(),
                                content: "first".into(),
                                is_error: false,
                            },
                            Content::ToolResult {
                                tool_use_id: "call-2".into(),
                                content: ToolResultContent::Blocks(vec![
                                    ToolResultBlock::Text {
                                        text: "second".into(),
                                    },
                                    ToolResultBlock::Text {
                                        text: "third".into(),
                                    },
                                ]),
                                is_error: true,
                            },
                        ],
                    },
                ],
                tools: vec![],
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        captured.await.unwrap().1["messages"][2]["content"],
        json!([
            {"type":"tool_result","tool_use_id":"call-1","content":"first","is_error":false},
            {"type":"tool_result","tool_use_id":"call-2","content":[
                {"type":"text","text":"second"},
                {"type":"text","text":"third"}
            ],"is_error":true}
        ])
    );
}

#[tokio::test]
async fn messages_rejects_noncomplete_stop_reasons_even_with_complete_content_blocks() {
    for (reason, expected) in [
        ("max_tokens", "output token limit"),
        ("refusal", "refused"),
        ("pause_turn", "paused"),
        ("model_context_window_exceeded", "context limit"),
        ("unknown-provider-reason", "unsupported stop reason"),
    ] {
        let tool_response = [
            event("message_start", json!({"message":{"role":"assistant","content":[]}})),
            event("content_block_start", json!({"index":0,"content_block":{"type":"tool_use","id":"call-1","name":"Read","input":{"file_path":"notes.md"}}})),
            event("content_block_stop", json!({"index":0})),
            event("message_delta", json!({"delta":{"stop_reason":reason}})),
            event("message_stop", json!({})),
        ].concat();
        for body in [text_response().replace("end_turn", reason), tool_response] {
            let (base_url, _) = serve_response(200, body).await;
            let client = HttpModelClient::new(configuration(base_url)).unwrap();
            let (events, _) = mpsc::unbounded_channel();
            let error = client
                .complete(
                    ModelRequest {
                        history_sources: Vec::new(),
                        messages: vec![message(Role::User, "Continue")],
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
            assert!(error.to_string().contains(expected), "{reason}: {error}");
        }
    }
}

fn configuration(base_url: String) -> ModelConfig {
    ModelConfig {
        provider: tickloop_agent::model::ModelProvider::Api,
        auth_source: tickloop_agent::model::ModelAuthSource::ApiKeyEnv,
        name: "DeepSeek test".into(),
        protocol: Protocol::Messages,
        base_url,
        model: "deepseek-flash".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    }
}

fn event(event_type: &str, mut data: Value) -> String {
    data["type"] = json!(event_type);
    format!("event: {event_type}\r\ndata: {data}\r\n\r\n")
}

fn text_response() -> String {
    text_response_with_usage(json!({
        "input_tokens": 21,
        "output_tokens": 1,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 34
    }))
}

fn text_response_with_usage(usage: Value) -> String {
    [
        event(
            "message_start",
            json!({"message":{"role":"assistant","content":[],"usage":usage}}),
        ),
        event(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"text","text":""}}),
        ),
        event(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":"你好"}}),
        ),
        event("ping", json!({})),
        event(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":"，朋友"}}),
        ),
        event("content_block_stop", json!({"index":0})),
        event(
            "message_delta",
            json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}),
        ),
        event("message_stop", json!({})),
    ]
    .concat()
}

fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![Content::Text { text: text.into() }],
    }
}

#[tokio::test]
async fn streams_messages_with_multiround_history_and_actual_cumulative_usage() {
    let (base_url, request_receiver) = serve_response(200, text_response()).await;
    let client = HttpModelClient::new(configuration(base_url)).unwrap();
    let (events, mut receiver) = mpsc::unbounded_channel();
    let output = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![
                    message(Role::System, "Be helpful."),
                    message(Role::User, "My name is Sam."),
                    message(Role::Assistant, "Hello Sam."),
                    message(Role::User, "Say hello again."),
                ],
                tools: vec![],
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        output.content,
        vec![Content::Text {
            text: "你好，朋友".into()
        }]
    );
    assert_eq!(
        receiver.recv().await,
        Some(ModelDelta::Usage(Usage {
            input_tokens: Some(21),
            output_tokens: Some(1),
            cache_read_input_tokens: Some(34),
            cache_write_input_tokens: Some(0),
        }))
    );
    assert_eq!(receiver.recv().await, Some(ModelDelta::Text("你好".into())));
    assert_eq!(
        receiver.recv().await,
        Some(ModelDelta::Text("，朋友".into()))
    );
    assert_eq!(output.usage.input_tokens, Some(21));
    assert_eq!(output.usage.output_tokens, Some(8));
    assert_eq!(output.usage.cache_read_input_tokens, Some(34));
    assert_eq!(output.usage.cache_write_input_tokens, Some(0));
    assert_eq!(output.stop_reason, "end_turn");
    let (headers, request) = request_receiver.await.unwrap();
    assert!(headers.starts_with("POST /anthropic/v1/messages HTTP/1.1"));
    assert!(headers.contains("anthropic-version: 2023-06-01"));
    assert_eq!(request["model"], "deepseek-flash");
    assert_eq!(request["max_tokens"], 1024);
    assert_eq!(request["stream"], true);
    assert_eq!(
        request["system"],
        json!([{"type":"text","text":"Be helpful."}])
    );
    assert_eq!(
        request["messages"],
        json!([
            {"role":"user","content":[{"type":"text","text":"My name is Sam."}]},
            {"role":"assistant","content":[{"type":"text","text":"Hello Sam."}]},
            {"role":"user","content":[{"type":"text","text":"Say hello again."}]}
        ])
    );
}

#[tokio::test]
async fn messages_rejects_invalid_cache_usage_counters() {
    for usage in [
        json!({"cache_read_input_tokens":-1}),
        json!({"cache_creation_input_tokens":"many"}),
    ] {
        let (base_url, _) = serve_response(200, text_response_with_usage(usage)).await;
        let client = HttpModelClient::new(configuration(base_url)).unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(
                ModelRequest {
                    history_sources: Vec::new(),
                    messages: vec![message(Role::User, "Hello")],
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
        assert!(error.to_string().contains("usage counter"));
    }
}

#[test]
fn usage_deserializes_old_records_without_cache_counters() {
    let usage: Usage = serde_json::from_value(json!({
        "input_tokens": 9,
        "output_tokens": 4
    }))
    .unwrap();

    assert_eq!(usage.input_tokens, Some(9));
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.cache_read_input_tokens, None);
    assert_eq!(usage.cache_write_input_tokens, None);
}

#[tokio::test]
async fn messages_sends_image_blocks_in_the_anthropic_shape() {
    let (base_url, request_receiver) = serve_response(200, text_response()).await;
    let client = HttpModelClient::new(ModelConfig {
        input_modalities: vec![InputModality::Text, InputModality::Image],
        ..configuration(base_url)
    })
    .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![Message {
                    role: Role::User,
                    content: vec![
                        Content::Text {
                            text: "chart.png".into(),
                        },
                        Content::Image {
                            source: ImageSource::Base64 {
                                media_type: ImageMediaType::Png,
                                data: "iVBORw0KGgo=".into(),
                            },
                            width: 640,
                            height: 480,
                        },
                    ],
                }],
                tools: vec![],
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let (_, request) = request_receiver.await.unwrap();
    assert_eq!(
        request["messages"],
        json!([{"role":"user","content":[
            {"type":"text","text":"chart.png"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}
        ]}])
    );
}

#[tokio::test]
async fn messages_nests_tool_result_images_in_the_tool_result_content_array() {
    let (base_url, request_receiver) = serve_response(200, text_response()).await;
    let client = HttpModelClient::new(ModelConfig {
        input_modalities: vec![InputModality::Text, InputModality::Image],
        ..configuration(base_url)
    })
    .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![
                    message(Role::User, "Read the chart"),
                    Message {
                        role: Role::Assistant,
                        content: vec![Content::ToolUse {
                            id: "call-1".into(),
                            name: "read_file".into(),
                            input: json!({"path":"chart.png"}),
                        }],
                    },
                    Message {
                        role: Role::User,
                        content: vec![Content::ToolResult {
                            tool_use_id: "call-1".into(),
                            content: ToolResultContent::Blocks(vec![
                                ToolResultBlock::Text {
                                    text: "chart.png summary".into(),
                                },
                                ToolResultBlock::Image {
                                    source: ImageSource::Base64 {
                                        media_type: ImageMediaType::Png,
                                        data: "iVBORw0KGgo=".into(),
                                    },
                                    width: 640,
                                    height: 480,
                                },
                            ]),
                            is_error: false,
                        }],
                    },
                ],
                tools: vec![],
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let (_, request) = request_receiver.await.unwrap();
    assert_eq!(
        request["messages"][2]["content"],
        json!([{"type":"tool_result","tool_use_id":"call-1","content":[
            {"type":"text","text":"chart.png summary"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}
        ],"is_error":false}])
    );
}
