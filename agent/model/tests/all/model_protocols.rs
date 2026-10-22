use tickloop_agent_test_support::model_http_support;

use model_http_support::{serve_response, serve_response_with_headers};
use serde_json::{Value, json};
use std::time::Duration;
use tickloop_agent_model::{
    LocalModelClients, ModelClients, ModelDelta, ModelError, ModelFailureKind, ModelRequest,
    ModelRequestStage, ModelResponseRejection,
};
use tickloop_agent_protocol::{
    Content, ImageMediaType, ImageSource, InputModality, Message, ModelConfig, Protocol, Role,
    ToolDefinition, ToolResultBlock, ToolResultContent,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn configuration(base_url: String, protocol: Protocol) -> ModelConfig {
    ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "DeepSeek".into(),
        protocol,
        base_url: base_url.trim_end_matches("/anthropic").into(),
        model: "deepseek-flash".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text, InputModality::Image],
    }
}

#[tokio::test]
async fn responses_raw_reasoning_prefix_rejects_a_conflicting_completed_value() {
    for added_prefix in [true, false] {
        let (url, _) = serve_response(
            200,
            model_http_support::raw_reasoning_prefix_reply(added_prefix, "Contradictory", false),
        )
        .await;
        let client = LocalModelClients::new(None)
            .client(&configuration(url, Protocol::Responses))
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let result = client
            .complete(
                ModelRequest {
                    history_sources: vec![],
                    messages: vec![message(Role::User, "Hello")],
                    tools: vec![],
                    structured_output: None,
                    temperature: None,
                    reasoning_effort: None,
                },
                events,
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(result, Err(ModelError::InvalidResponse(message)) if message == "completed reasoning differs from its stream")
        );
    }
}

#[tokio::test]
async fn responses_raw_reasoning_prefix_completion_delivers_the_normal_answer() {
    for added_prefix in [true, false] {
        let (url, _) = serve_response(
            200,
            model_http_support::raw_reasoning_prefix_reply(added_prefix, "Thinking", false),
        )
        .await;
        let client = LocalModelClients::new(None)
            .client(&configuration(url, Protocol::Responses))
            .unwrap();
        let (events, mut deltas) = mpsc::unbounded_channel();
        let result = client
            .complete(
                ModelRequest {
                    history_sources: vec![],
                    messages: vec![message(Role::User, "Hello")],
                    tools: vec![],
                    structured_output: None,
                    temperature: None,
                    reasoning_effort: None,
                },
                events,
                CancellationToken::new(),
            )
            .await;
        let output =
            result.expect("a reasoning prefix can be completed without failing the normal turn");
        let mut thinking = String::new();
        let mut text = String::new();
        while let Ok(delta) = deltas.try_recv() {
            match delta {
                ModelDelta::Thinking(value) => thinking.push_str(&value),
                ModelDelta::Text(value) => text.push_str(&value),
                _ => {}
            }
        }
        assert_eq!(thinking, "Thinking");
        assert_eq!(text, "Answer");
        assert!(
            matches!(&output.content[0], Content::ProtocolData { value, .. } if value["content"][0]["text"] == "Thinking")
        );
    }
}

fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![Content::Text { text: text.into() }],
    }
}

fn response_event(event_type: &str, sequence: u64, mut value: Value) -> String {
    value["type"] = json!(event_type);
    value["sequence_number"] = json!(sequence);
    format!("event: {event_type}\ndata: {value}\n\n")
}

#[tokio::test]
async fn model_clients_recognize_context_limit_http_errors() {
    let cases = [
        (
            Protocol::Messages,
            json!({"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 150000 tokens exceeds the model limit"}}),
        ),
        (
            Protocol::Responses,
            json!({"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"Maximum context length exceeded"}}),
        ),
        (
            Protocol::ChatCompletions,
            json!({"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"Maximum context length exceeded"}}),
        ),
    ];
    for (protocol, error_body) in cases {
        let (base_url, _) = serve_response(400, error_body.to_string()).await;
        let client = LocalModelClients::new(None)
            .client(&configuration(base_url, protocol))
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(
                ModelRequest {
                    history_sources: Vec::new(),
                    messages: vec![message(Role::User, "Review this action")],
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
        assert!(
            matches!(
                &error,
                ModelError::AtStage { error, .. }
                    if matches!(**error, ModelError::ResponseRejected(ModelResponseRejection::ContextLimit))
            ),
            "{protocol:?}: {error}"
        );
        assert_eq!(error.stage(), ModelRequestStage::ResponseHeaders);
    }
}

#[tokio::test]
async fn unrelated_bad_requests_remain_http_errors() {
    let (base_url, _) = serve_response(
        400,
        json!({"error":{"type":"invalid_request_error","message":"temperature is unsupported"}})
            .to_string(),
    )
    .await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Messages))
        .unwrap();
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
    assert_eq!(error.status(), Some(400));
    assert_eq!(error.stage(), ModelRequestStage::ResponseHeaders);
    assert!(error.to_string().contains("temperature is unsupported"));
}

#[tokio::test]
async fn billing_failure_is_distinct_from_a_rate_limit() {
    let (base_url, _) = serve_response(
        402,
        json!({"error":{"type":"billing_error","message":"Insufficient balance"}}).to_string(),
    )
    .await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Messages))
        .unwrap();
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
    assert_eq!(error.kind(), ModelFailureKind::QuotaExhausted);
    assert_eq!(error.stage(), ModelRequestStage::ResponseHeaders);
}

#[tokio::test]
async fn spend_cap_429_disables_automatic_retry() {
    let (base_url, _) = serve_response(
        429,
        json!({"error":{"type":"rate_limit_error","message":"Spend limit reached","details":{"error_code":"enforced_spend_limit_reached"}}}).to_string(),
    )
    .await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Messages))
        .unwrap();
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
    assert_eq!(error.kind(), ModelFailureKind::QuotaExhausted);
    assert_eq!(error.status(), Some(429));
    assert_eq!(error.stage(), ModelRequestStage::ResponseHeaders);
}

#[tokio::test]
async fn http_client_preserves_server_retry_instructions() {
    let (base_url, _) = serve_response_with_headers(
        429,
        json!({"error":{"type":"rate_limit_error"}}).to_string(),
        &[("Retry-After", "2"), ("x-should-retry", "true")],
    )
    .await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Messages))
        .unwrap();
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
    assert_eq!(error.status(), Some(429));
    assert_eq!(error.stage(), ModelRequestStage::ResponseHeaders);
    assert_eq!(error.retry_hint(), (true, Some(Duration::from_secs(2))));
}

#[tokio::test]
async fn streamed_service_errors_keep_retryable_error_categories() {
    let cases = [
        (
            Protocol::Messages,
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n".to_owned(),
            529,
        ),
        (
            Protocol::Responses,
            [
                response_event(
                    "response.created",
                    0,
                    json!({"response":{"id":"resp-1","status":"in_progress"}}),
                ),
                response_event(
                    "response.failed",
                    1,
                    json!({"response":{"id":"resp-1","status":"failed","error":{"code":"server_error"}}}),
                ),
            ]
            .concat(),
            500,
        ),
        (
            Protocol::ChatCompletions,
            "data: {\"error\":{\"type\":\"rate_limit_error\"}}\n\n".to_owned(),
            429,
        ),
    ];
    for (protocol, stream, expected_status) in cases {
        let (base_url, _) = serve_response(200, stream).await;
        let client = LocalModelClients::new(None)
            .client(&configuration(base_url, protocol))
            .unwrap();
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
        if expected_status == 429 {
            assert_eq!(
                error.kind(),
                ModelFailureKind::RateLimited,
                "{protocol:?}: {error}"
            );
        } else {
            assert_eq!(
                error.status(),
                Some(expected_status),
                "{protocol:?}: {error}"
            );
        }
        assert_eq!(error.stage(), ModelRequestStage::ResponseStream);
    }
}

#[tokio::test]
async fn responses_client_recognizes_a_streamed_context_limit_failure() {
    let stream = [
        response_event(
            "response.created",
            0,
            json!({"response":{"id":"resp-1","status":"in_progress"}}),
        ),
        response_event(
            "response.failed",
            1,
            json!({"response":{"id":"resp-1","status":"failed","error":{"code":"context_length_exceeded","message":"Maximum context length exceeded"}}}),
        ),
    ]
    .concat();
    let (base_url, _) = serve_response(200, stream).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Responses))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    let error = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![message(Role::User, "Review this action")],
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
    assert!(matches!(
        error,
        ModelError::ResponseRejected(ModelResponseRejection::ContextLimit)
    ));
}

#[tokio::test]
async fn model_clients_recognize_streamed_context_limit_errors() {
    let cases = [
        (
            Protocol::Messages,
            format!(
                "event: error\ndata: {}\n\n",
                json!({"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 150000 tokens exceeds the model limit"}})
            ),
        ),
        (
            Protocol::Responses,
            response_event(
                "error",
                0,
                json!({"code":"context_length_exceeded","message":"Maximum context length exceeded","param":null}),
            ),
        ),
    ];
    for (protocol, stream) in cases {
        let (base_url, _) = serve_response(200, stream).await;
        let client = LocalModelClients::new(None)
            .client(&configuration(base_url, protocol))
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(
                ModelRequest {
                    history_sources: Vec::new(),
                    messages: vec![message(Role::User, "Review this action")],
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
        assert!(
            matches!(
                error,
                ModelError::ResponseRejected(ModelResponseRejection::ContextLimit)
            ),
            "{protocol:?}: {error}"
        );
    }
}

fn responses_text_stream() -> String {
    responses_text_stream_with_usage(json!({
        "input_tokens": 25,
        "output_tokens": 4,
        "input_tokens_details": {
            "cached_tokens": 11,
            "cache_write_tokens": 0
        }
    }))
}

fn responses_text_stream_with_usage(usage: Value) -> String {
    let item = json!({"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello Sam","annotations":[]}]});
    [
        response_event("response.created",0,json!({"response":{"id":"resp-1","status":"in_progress"}})),
        response_event("response.output_item.added",1,json!({"output_index":0,"item":{"type":"message","id":"msg-1","role":"assistant","status":"in_progress","content":[]}})),
        response_event("response.output_text.delta",2,json!({"output_index":0,"item_id":"msg-1","content_index":0,"delta":"Hello "})),
        response_event("response.output_text.delta",3,json!({"output_index":0,"item_id":"msg-1","content_index":0,"delta":"Sam"})),
        response_event("response.output_item.done",4,json!({"output_index":0,"item":item})),
        response_event("response.completed",5,json!({"response":{"id":"resp-1","status":"completed","output":[item],"usage":usage}})),
    ].concat()
}

#[tokio::test]
async fn responses_streams_text_with_complete_client_side_conversation_history() {
    let (base_url, captured) = serve_response(200, responses_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Responses))
        .unwrap();
    let (events, mut receiver) = mpsc::unbounded_channel();
    let output = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![
                    message(Role::System, "Be helpful."),
                    message(Role::User, "I am Sam"),
                    message(Role::Assistant, "Welcome"),
                    message(Role::User, "Greet me"),
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
            text: "Hello Sam".into()
        }]
    );
    assert_eq!(
        receiver.recv().await,
        Some(ModelDelta::Text("Hello ".into()))
    );
    assert_eq!(receiver.recv().await, Some(ModelDelta::Text("Sam".into())));
    assert_eq!(output.usage.input_tokens, Some(25));
    assert_eq!(output.usage.output_tokens, Some(4));
    assert_eq!(output.usage.cache_read_input_tokens, Some(11));
    assert_eq!(output.usage.cache_write_input_tokens, Some(0));
    let (headers, body) = captured.await.unwrap();
    assert!(headers.starts_with("POST /responses HTTP/1.1"));
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(
        body["input"],
        json!([
            {"type":"message","role":"system","content":"Be helpful."},
            {"type":"message","role":"user","content":"I am Sam"},
            {"type":"message","role":"assistant","content":"Welcome"},
            {"type":"message","role":"user","content":"Greet me"}
        ])
    );
}

fn responses_tool_stream() -> String {
    let reasoning = json!({"type":"reasoning","id":"rs-1","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"Read both files"}],"encrypted_content":"provider-state"});
    let mut sequence = 0;
    let mut frames = Vec::new();
    let mut push = |kind: &str, value: Value| {
        frames.push(response_event(kind, sequence, value));
        sequence += 1;
    };
    push(
        "response.created",
        json!({"response":{"id":"resp-2","status":"in_progress"}}),
    );
    push(
        "response.output_item.added",
        json!({"output_index":0,"item":{"type":"reasoning","id":"rs-1","content":[],"summary":[]}}),
    );
    push(
        "response.reasoning_text.delta",
        json!({"output_index":0,"item_id":"rs-1","content_index":0,"delta":"Read both files"}),
    );
    push(
        "response.output_item.done",
        json!({"output_index":0,"item":reasoning}),
    );
    let mut output = vec![reasoning];
    for (index, name) in [(1, "first.md"), (2, "second.md")] {
        let id = format!("fc-{index}");
        let call_id = format!("call-{index}");
        let arguments = format!("{{\"file_path\":\"{name}\"}}");
        push(
            "response.output_item.added",
            json!({"output_index":index,"item":{"type":"function_call","id":id,"call_id":call_id,"name":"Read","arguments":""}}),
        );
        push(
            "response.function_call_arguments.delta",
            json!({"output_index":index,"item_id":id,"delta":"{\"file_path\":"}),
        );
        push(
            "response.function_call_arguments.delta",
            json!({"output_index":index,"item_id":id,"delta":format!("\"{name}\"}}")}),
        );
        push(
            "response.function_call_arguments.done",
            json!({"output_index":index,"item_id":id,"arguments":arguments}),
        );
        let item = json!({"type":"function_call","id":id,"call_id":call_id,"name":"Read","arguments":arguments,"status":"completed"});
        push(
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        );
        output.push(item);
    }
    push(
        "response.completed",
        json!({"response":{"id":"resp-2","status":"completed","output":output}}),
    );
    frames.concat()
}

#[tokio::test]
async fn responses_round_trips_multiple_calls_and_protocol_specific_reasoning() {
    let (base_url, captured) = serve_response(200, responses_tool_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Responses))
        .unwrap();
    let tools = vec![ToolDefinition {
        name: "Read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{"file_path":{"type":"string"}}}),
    }];
    let (events, mut receiver) = mpsc::unbounded_channel();
    let output = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![message(Role::User, "Read both")],
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
    assert_eq!(output.stop_reason, "tool_use");
    assert_eq!(output.usage.input_tokens, None);
    assert_eq!(output.content.len(), 3);
    assert!(
        matches!(&output.content[0],Content::ProtocolData{protocol:Protocol::Responses,value} if value["encrypted_content"]=="provider-state")
    );
    assert_eq!(
        output.content[1],
        Content::ToolUse {
            id: "call-1".into(),
            name: "Read".into(),
            input: json!({"file_path":"first.md"})
        }
    );
    assert_eq!(
        output.content[2],
        Content::ToolUse {
            id: "call-2".into(),
            name: "Read".into(),
            input: json!({"file_path":"second.md"})
        }
    );
    assert_eq!(
        receiver.recv().await,
        Some(ModelDelta::Thinking("Read both files".into()))
    );
    assert_eq!(
        captured.await.unwrap().1["tools"][0],
        json!({"type":"function","name":"Read","description":"Read a file","parameters":tools[0].input_schema})
    );
    let (base_url, captured) = serve_response(200, responses_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Responses))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![
                    message(Role::User, "Read both"),
                    Message {
                        role: Role::Assistant,
                        content: output.content,
                    },
                    Message {
                        role: Role::User,
                        content: vec![
                            Content::ToolResult {
                                tool_use_id: "call-1".into(),
                                content: "one".into(),
                                is_error: false,
                            },
                            Content::ToolResult {
                                tool_use_id: "call-2".into(),
                                content: "two".into(),
                                is_error: false,
                            },
                        ],
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
    let input = captured.await.unwrap().1["input"].clone();
    assert_eq!(input[1]["encrypted_content"], "provider-state");
    assert_eq!(input[2]["call_id"], "call-1");
    assert_eq!(
        input[4],
        json!({"type":"function_call_output","call_id":"call-1","output":"one"})
    );
    assert_eq!(
        input[5],
        json!({"type":"function_call_output","call_id":"call-2","output":"two"})
    );
}

/// Parallel calls as GPT models stream them on the Codex backend: arguments only on completion.
fn responses_tool_stream_without_argument_deltas() -> String {
    responses_tool_stream()
        .split_inclusive("\n\n")
        .filter(|frame| !frame.contains("function_call_arguments.delta"))
        .collect()
}

#[tokio::test]
async fn responses_accepts_calls_whose_arguments_arrive_only_on_completion() {
    let (base_url, _) = serve_response(200, responses_tool_stream_without_argument_deltas()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Responses))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    let output = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![message(Role::User, "Read both")],
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
    assert_eq!(output.stop_reason, "tool_use");
    assert_eq!(
        output.content[1..],
        [
            Content::ToolUse {
                id: "call-1".into(),
                name: "Read".into(),
                input: json!({"file_path":"first.md"})
            },
            Content::ToolUse {
                id: "call-2".into(),
                name: "Read".into(),
                input: json!({"file_path":"second.md"})
            }
        ]
    );
}

#[tokio::test]
async fn responses_rejects_repeated_incomplete_mismatched_and_failed_streams() {
    let complete = responses_tool_stream();
    let last_frame = complete.rfind("event: response.completed").unwrap();
    let duplicate = complete
        .split("\n\n")
        .find(|frame| frame.contains("function_call_arguments.delta"))
        .unwrap();
    let conflicting_completions = responses_tool_stream_without_argument_deltas()
        .split_inclusive("\n\n")
        .map(|frame| {
            if frame.contains("function_call_arguments.done") {
                frame.replace("first.md", "other.md")
            } else {
                frame.to_owned()
            }
        })
        .collect::<String>();
    let invalid = [
        complete.replace(r#""delta":"{\"file_path\":""#, r#""delta":"{\"path\":""#),
        conflicting_completions,
        complete[..last_frame].to_owned(),
        complete.replace("call-2", "call-1"),
        complete.replace("first.md\\\"}", "first.md"),
        complete.replace("\"sequence_number\":2,", "\"sequence_number\":1,"),
        complete.replace("event: response.completed", "event: response.failed"),
        complete.replace(duplicate, &format!("{duplicate}\n\n{duplicate}")),
        response_event(
            "response.completed",
            0,
            json!({"response":{"id":"orphan","status":"completed","output":[]}}),
        ),
    ];
    for (index, body) in invalid.into_iter().enumerate() {
        let (base_url, _) = serve_response(200, body).await;
        let client = LocalModelClients::new(None)
            .client(&configuration(base_url, Protocol::Responses))
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        assert!(
            client
                .complete(
                    ModelRequest {
                        history_sources: Vec::new(),
                        messages: vec![message(Role::User, "Read")],
                        tools: vec![],
                        structured_output: None,
                        temperature: None,
                        reasoning_effort: None,
                    },
                    events,
                    CancellationToken::new()
                )
                .await
                .is_err(),
            "invalid case {index} was accepted"
        );
    }
}

fn chat_chunk(delta: Value, finish_reason: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"chat-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}]})
    )
}

fn chat_tool_stream() -> String {
    chat_tool_stream_with_usage(json!({
        "prompt_tokens": 31,
        "completion_tokens": 14,
        "prompt_tokens_details": {
            "cached_tokens": 13,
            "cache_write_tokens": 2
        }
    }))
}

fn chat_tool_stream_with_usage(usage: Value) -> String {
    [
        chat_chunk(json!({"role":"assistant","reasoning_content":"Read the file"}),Value::Null),
        chat_chunk(json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"Read","arguments":"{\"file_path\":"}}]}),Value::Null),
        chat_chunk(json!({"tool_calls":[{"index":0,"function":{"arguments":"\"notes.md\"}"}}]}),Value::Null),
        chat_chunk(json!({}),json!("tool_calls")),
        format!("data: {}\n\n",json!({"id":"chat-1","choices":[],"usage":usage})),
        "data: [DONE]\n\n".into(),
    ].concat()
}

fn chat_text_stream() -> String {
    [
        chat_chunk(json!({"role":"assistant","content":"Read "}), Value::Null),
        chat_chunk(json!({"content":"the notes"}), Value::Null),
        chat_chunk(json!({}), json!("stop")),
        format!(
            "data: {}\n\n",
            json!({"id":"chat-1","choices":[],"usage":{"prompt_tokens":17,"completion_tokens":3,"prompt_cache_hit_tokens":5,"prompt_cache_miss_tokens":12}})
        ),
        "data: [DONE]\n\n".into(),
    ]
    .concat()
}

#[tokio::test]
async fn chat_completions_round_trips_thinking_and_tools_then_streams_the_answer() {
    let (base_url, captured) = serve_response(200, chat_tool_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::ChatCompletions))
        .unwrap();
    let tools = vec![ToolDefinition {
        name: "Read".into(),
        description: "Read file".into(),
        input_schema: json!({"type":"object"}),
    }];
    let (events, mut receiver) = mpsc::unbounded_channel();
    let output = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![
                    message(Role::System, "Be helpful"),
                    message(Role::User, "Read notes"),
                ],
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
                thinking: "Read the file".into(),
                signature: None
            },
            Content::ToolUse {
                id: "call-1".into(),
                name: "Read".into(),
                input: json!({"file_path":"notes.md"})
            }
        ]
    );
    assert_eq!(output.usage.input_tokens, Some(31));
    assert_eq!(output.usage.output_tokens, Some(14));
    assert_eq!(output.usage.cache_read_input_tokens, Some(13));
    assert_eq!(output.usage.cache_write_input_tokens, Some(2));
    assert_eq!(
        receiver.recv().await,
        Some(ModelDelta::Thinking("Read the file".into()))
    );
    let (headers, body) = captured.await.unwrap();
    assert!(headers.starts_with("POST /chat/completions HTTP/1.1"));
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["tools"][0]["function"]["name"], "Read");
    let (base_url, captured) = serve_response(200, chat_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::ChatCompletions))
        .unwrap();
    let (events, mut receiver) = mpsc::unbounded_channel();
    let answer = client
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
                            content: "The notes".into(),
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
    assert_eq!(
        answer.content,
        vec![Content::Text {
            text: "Read the notes".into()
        }]
    );
    assert_eq!(answer.usage.input_tokens, Some(17));
    assert_eq!(answer.usage.output_tokens, Some(3));
    assert_eq!(answer.usage.cache_read_input_tokens, Some(5));
    assert_eq!(answer.usage.cache_write_input_tokens, None);
    assert_eq!(answer.stop_reason, "end_turn");
    assert_eq!(
        receiver.recv().await,
        Some(ModelDelta::Text("Read ".into()))
    );
    assert_eq!(
        receiver.recv().await,
        Some(ModelDelta::Text("the notes".into()))
    );
    let body = captured.await.unwrap().1;
    assert_eq!(body["messages"][1]["reasoning_content"], "Read the file");
    assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call-1");
    assert_eq!(
        body["messages"][2],
        json!({"role":"tool","tool_call_id":"call-1","content":"The notes"})
    );
}

fn block_tool_result_request() -> ModelRequest {
    ModelRequest {
        history_sources: Vec::new(),
        messages: vec![
            message(Role::User, "Read notes"),
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    input: json!({"path":"notes.md"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: ToolResultContent::Blocks(vec![
                        ToolResultBlock::Text {
                            text: "first".into(),
                        },
                        ToolResultBlock::Text {
                            text: "second".into(),
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
    }
}

#[tokio::test]
async fn responses_sends_block_tool_results_as_input_text_items() {
    let (base_url, captured) = serve_response(200, responses_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Responses))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            block_tool_result_request(),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        captured.await.unwrap().1["input"][2],
        json!({"type":"function_call_output","call_id":"call-1","output":[
            {"type":"input_text","text":"first"},
            {"type":"input_text","text":"second"}
        ]})
    );
}

#[tokio::test]
async fn chat_completions_sends_block_tool_results_as_their_text() {
    let (base_url, captured) = serve_response(200, chat_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::ChatCompletions))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            block_tool_result_request(),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        captured.await.unwrap().1["messages"][2],
        json!({"role":"tool","tool_call_id":"call-1","content":"first\nsecond"})
    );
}

/// Two tool results in one turn, the first carrying an image, then a follow-up prompt.
fn tool_result_image_request() -> ModelRequest {
    let read = |id: &str, path: &str| Content::ToolUse {
        id: id.into(),
        name: "read_file".into(),
        input: json!({ "path": path }),
    };
    ModelRequest {
        history_sources: Vec::new(),
        messages: vec![
            message(Role::User, "Compare the chart with the notes"),
            Message {
                role: Role::Assistant,
                content: vec![read("call-1", "chart.png"), read("call-2", "notes.md")],
            },
            Message {
                role: Role::User,
                content: vec![
                    Content::ToolResult {
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
                    },
                    Content::ToolResult {
                        tool_use_id: "call-2".into(),
                        content: "The notes".into(),
                        is_error: false,
                    },
                ],
            },
            message(Role::User, "And the trend?"),
        ],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn responses_sends_tool_result_images_as_input_image_items_of_the_call_output() {
    let (base_url, captured) = serve_response(200, responses_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Responses))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            tool_result_image_request(),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let input = captured.await.unwrap().1["input"].clone();
    assert_eq!(
        input.as_array().unwrap()[3..],
        [
            json!({"type":"function_call_output","call_id":"call-1","output":[
                {"type":"input_text","text":"chart.png summary"},
                {"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="}
            ]}),
            json!({"type":"function_call_output","call_id":"call-2","output":"The notes"}),
            json!({"type":"message","role":"user","content":"And the trend?"}),
        ]
    );
}

#[tokio::test]
async fn chat_completions_moves_tool_result_images_into_a_user_message_after_the_tool_messages() {
    let (base_url, captured) = serve_response(200, chat_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::ChatCompletions))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(
            tool_result_image_request(),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let messages = captured.await.unwrap().1["messages"].clone();
    assert_eq!(
        messages.as_array().unwrap()[2..],
        [
            json!({"role":"tool","tool_call_id":"call-1","content":"chart.png summary"}),
            json!({"role":"tool","tool_call_id":"call-2","content":"The notes"}),
            json!({"role":"user","content":[
                {"type":"text","text":"Attached media from tool result:"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}
            ]}),
            json!({"role":"user","content":"And the trend?"}),
        ]
    );
}

fn chart_image() -> Content {
    Content::Image {
        source: ImageSource::Base64 {
            media_type: ImageMediaType::Png,
            data: "iVBORw0KGgo=".into(),
        },
        width: 640,
        height: 480,
    }
}

/// A text-only user turn followed by one that carries typed text, an image label and the image.
fn user_image_request() -> ModelRequest {
    ModelRequest {
        history_sources: Vec::new(),
        messages: vec![
            message(Role::System, "Be helpful."),
            Message {
                role: Role::User,
                content: vec![
                    Content::Text {
                        text: "Hello ".into(),
                    },
                    Content::Text {
                        text: "there".into(),
                    },
                ],
            },
            message(Role::Assistant, "Welcome"),
            Message {
                role: Role::User,
                content: vec![
                    Content::Text {
                        text: "Describe the chart".into(),
                    },
                    Content::Text {
                        text: "[Image: chart.png]".into(),
                    },
                    chart_image(),
                ],
            },
        ],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn responses_sends_user_images_as_input_image_data_urls() {
    let (base_url, captured) = serve_response(200, responses_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::Responses))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(user_image_request(), events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        captured.await.unwrap().1["input"],
        json!([
            {"type":"message","role":"system","content":"Be helpful."},
            {"type":"message","role":"user","content":"Hello "},
            {"type":"message","role":"user","content":"there"},
            {"type":"message","role":"assistant","content":"Welcome"},
            {"type":"message","role":"user","content":[
                {"type":"input_text","text":"Describe the chart"},
                {"type":"input_text","text":"[Image: chart.png]"},
                {"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="}
            ]}
        ])
    );
}

#[tokio::test]
async fn chat_completions_sends_user_images_as_image_url_parts_and_keeps_text_only_turns_as_strings()
 {
    let (base_url, captured) = serve_response(200, chat_text_stream()).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::ChatCompletions))
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(user_image_request(), events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        captured.await.unwrap().1["messages"],
        json!([
            {"role":"system","content":"Be helpful."},
            {"role":"user","content":"Hello there"},
            {"role":"assistant","content":"Welcome"},
            {"role":"user","content":[
                {"type":"text","text":"Describe the chart"},
                {"type":"text","text":"[Image: chart.png]"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}
            ]}
        ])
    );
}

#[tokio::test]
async fn responses_and_chat_completions_refuse_images_outside_user_messages() {
    for protocol in [Protocol::Responses, Protocol::ChatCompletions] {
        for role in [Role::System, Role::Assistant] {
            let (base_url, _) = serve_response(200, String::new()).await;
            let client = LocalModelClients::new(None)
                .client(&configuration(base_url, protocol))
                .unwrap();
            let (events, _) = mpsc::unbounded_channel();
            let error = client
                .complete(
                    ModelRequest {
                        history_sources: Vec::new(),
                        messages: vec![Message {
                            role,
                            content: vec![chart_image()],
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
                .unwrap_err();
            assert!(
                matches!(&error, ModelError::Configuration(reason) if reason.contains("images are only supported in user messages")),
                "{error}"
            );
        }
    }
}

#[tokio::test]
async fn responses_and_chat_completions_reject_invalid_cache_usage() {
    let cases = [
        (
            Protocol::Responses,
            responses_text_stream_with_usage(json!({
                "input_tokens_details":{"cached_tokens":-1}
            })),
        ),
        (
            Protocol::Responses,
            responses_text_stream_with_usage(json!({
                "input_tokens_details":"invalid"
            })),
        ),
        (
            Protocol::ChatCompletions,
            chat_tool_stream_with_usage(json!({
                "prompt_tokens_details":{"cache_write_tokens":"many"}
            })),
        ),
        (
            Protocol::ChatCompletions,
            chat_tool_stream_with_usage(json!({
                "prompt_cache_hit_tokens":-1
            })),
        ),
    ];
    for (protocol, body) in cases {
        let (base_url, _) = serve_response(200, body).await;
        let client = LocalModelClients::new(None)
            .client(&configuration(base_url, protocol))
            .unwrap();
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
        assert!(error.to_string().contains("usage"));
    }
}

#[tokio::test]
async fn chat_completions_rejects_unfinished_duplicate_and_malformed_calls() {
    let complete = chat_tool_stream();
    let first = complete
        .split("\n\n")
        .find(|frame| frame.contains("call-1"))
        .unwrap();
    for body in [
        complete.replace("data: [DONE]\n\n", ""),
        complete.replace(first, &format!("{first}\n\n{first}")),
        complete.replace("notes.md\\\"}", "notes.md"),
        complete.replace(
            "\"finish_reason\":\"tool_calls\"",
            "\"finish_reason\":\"stop\"",
        ),
        "data: [DONE]\n\n".into(),
    ] {
        let (base_url, _) = serve_response(200, body).await;
        let client = LocalModelClients::new(None)
            .client(&configuration(base_url, Protocol::ChatCompletions))
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        assert!(
            client
                .complete(
                    ModelRequest {
                        history_sources: Vec::new(),
                        messages: vec![message(Role::User, "Read")],
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
async fn chat_completions_rejects_noncomplete_finish_reasons_before_returning_text_or_tools() {
    for (reason, expected) in [
        ("length", "output token limit"),
        ("content_filter", "content filter"),
        ("insufficient_system_resource", "HTTP 503"),
        ("unknown-provider-reason", "unsupported finish reason"),
    ] {
        for body in [
            chat_text_stream().replace(
                "\"finish_reason\":\"stop\"",
                &format!("\"finish_reason\":\"{reason}\""),
            ),
            chat_tool_stream().replace(
                "\"finish_reason\":\"tool_calls\"",
                &format!("\"finish_reason\":\"{reason}\""),
            ),
        ] {
            let (base_url, _) = serve_response(200, body).await;
            let client = LocalModelClients::new(None)
                .client(&configuration(base_url, Protocol::ChatCompletions))
                .unwrap();
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

/// Every delta a finished request sent, in order.
pub(crate) fn sent_deltas(receiver: &mut mpsc::UnboundedReceiver<ModelDelta>) -> Vec<ModelDelta> {
    std::iter::from_fn(|| receiver.try_recv().ok()).collect()
}

/// The calls `deltas` announced as started, as `(id, name)`.
pub(crate) fn started_tool_uses(deltas: &[ModelDelta]) -> Vec<(String, String)> {
    deltas
        .iter()
        .filter_map(|delta| match delta {
            ModelDelta::ToolUseStarted { id, name } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

/// The calls an output carries, as `(id, name)`.
pub(crate) fn output_tool_uses(content: &[Content]) -> Vec<(String, String)> {
    content
        .iter()
        .filter_map(|block| match block {
            Content::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

fn prompt_request(prompt: &str) -> ModelRequest {
    ModelRequest {
        history_sources: Vec::new(),
        messages: vec![message(Role::User, prompt)],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn chat_completions_announces_each_call_once_its_name_is_whole() {
    let stream = [
        chat_chunk(json!({"role":"assistant","content":"Making the page."}), Value::Null),
        chat_chunk(json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"Read","arguments":"{\"file_path\":"}}]}), Value::Null),
        chat_chunk(json!({"tool_calls":[{"index":0,"function":{"arguments":"\"notes.md\"}"}}]}), Value::Null),
        chat_chunk(json!({"tool_calls":[{"index":1,"id":"call-2","type":"function","function":{"name":"Artifact","arguments":""}}]}), Value::Null),
        chat_chunk(json!({"tool_calls":[{"index":1,"function":{"name":"Edit","arguments":"{}"}}]}), Value::Null),
        chat_chunk(json!({}), json!("tool_calls")),
        format!("data: {}\n\n", json!({"id":"chat-1","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4}})),
        "data: [DONE]\n\n".into(),
    ]
    .concat();
    let (base_url, _) = serve_response(200, stream).await;
    let client = LocalModelClients::new(None)
        .client(&configuration(base_url, Protocol::ChatCompletions))
        .unwrap();
    let (events, mut receiver) = mpsc::unbounded_channel();
    let output = client
        .complete(
            prompt_request("Make a page"),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let deltas = sent_deltas(&mut receiver);
    let started = started_tool_uses(&deltas);
    assert_eq!(
        started,
        [
            ("call-1".to_owned(), "Read".to_owned()),
            ("call-2".to_owned(), "ArtifactEdit".to_owned())
        ]
    );
    assert_eq!(started, output_tool_uses(&output.content));
    let text = deltas
        .iter()
        .position(|delta| *delta == ModelDelta::Text("Making the page.".into()));
    let first_start = deltas
        .iter()
        .position(|delta| matches!(delta, ModelDelta::ToolUseStarted { .. }));
    assert!(text < first_start, "{deltas:?}");
}

#[tokio::test]
async fn responses_announce_each_call_once_with_the_id_of_its_output() {
    for stream in [
        responses_tool_stream(),
        responses_tool_stream_without_argument_deltas(),
    ] {
        let (base_url, _) = serve_response(200, stream).await;
        let client = LocalModelClients::new(None)
            .client(&configuration(base_url, Protocol::Responses))
            .unwrap();
        let (events, mut receiver) = mpsc::unbounded_channel();
        let output = client
            .complete(
                prompt_request("Read both"),
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let started = started_tool_uses(&sent_deltas(&mut receiver));
        assert_eq!(
            started,
            [
                ("call-1".to_owned(), "Read".to_owned()),
                ("call-2".to_owned(), "Read".to_owned())
            ]
        );
        assert_eq!(started, output_tool_uses(&output.content));
    }
}
