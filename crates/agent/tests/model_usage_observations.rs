mod model_http_support;

use model_http_support::serve_response;
use serde_json::{Value, json};
use tickloop_agent::model::{
    HttpModelClient, InputModality, ModelClient, ModelConfig, ModelDelta, ModelError,
    ModelFailureKind, ModelIdentity, ModelOutput, ModelRequest, ModelRequestStage,
    ModelResponseRejection, Protocol, Usage,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn configuration(base_url: String, protocol: Protocol) -> ModelConfig {
    ModelConfig {
        provider: tickloop_agent::model::ModelProvider::Api,
        auth_source: tickloop_agent::model::ModelAuthSource::ApiKeyEnv,
        name: "DeepSeek fixture".into(),
        protocol,
        base_url,
        model: "deepseek-fixture".into(),
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

fn event(mut value: Value, kind: &str) -> String {
    value["type"] = json!(kind);
    format!("data: {value}\n\n")
}

fn messages_start(usage: Value) -> String {
    event(
        json!({"message":{"id":"msg-1","usage":usage}}),
        "message_start",
    )
}

fn messages_delta(usage: Value, reason: Option<&str>) -> String {
    event(
        json!({"usage":usage,"delta":{"stop_reason":reason}}),
        "message_delta",
    )
}

fn response_start() -> String {
    event(
        json!({"response":{"id":"resp-1"},"sequence_number":0}),
        "response.created",
    )
}

fn response_terminal(kind: &str, usage: Value, extra: Value) -> String {
    let mut response = json!({"id":"resp-1","status":kind,"usage":usage,"output":[]});
    response
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    event(
        json!({"response":response,"sequence_number":1}),
        &format!("response.{kind}"),
    )
}

fn chat_chunk(usage: Value, reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"chat-1","choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":usage})
    )
}

async fn complete(
    protocol: Protocol,
    body: String,
) -> (Result<ModelOutput, ModelError>, Vec<Usage>) {
    let (base_url, _) = serve_response(200, body).await;
    let client = HttpModelClient::new(configuration(base_url, protocol)).unwrap();
    let (events, mut receiver) = mpsc::unbounded_channel();
    let result = client
        .complete(
            ModelRequest {
                history_sources: Vec::new(),
                messages: vec![],
                tools: vec![],
                structured_output: None,
                temperature: None,
                reasoning_effort: None,
            },
            events,
            CancellationToken::new(),
        )
        .await;
    let mut observations = Vec::new();
    while let Ok(delta) = receiver.try_recv() {
        if let ModelDelta::Usage(usage) = delta {
            observations.push(usage);
        }
    }
    (result, observations)
}

#[test]
fn identity_is_the_requested_profile_protocol_and_model_without_service_configuration() {
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
    ] {
        let config = configuration(
            "http://127.0.0.1:1234/private-service-path".into(),
            protocol,
        );
        let client = HttpModelClient::new(config).unwrap();
        let identity = client.identity();
        assert_eq!(
            identity,
            ModelIdentity {
                provider: tickloop_agent::model::ModelProvider::Api,
                profile: "DeepSeek fixture".into(),
                protocol,
                requested_model: "deepseek-fixture".into()
            }
        );
        let encoded = serde_json::to_value(&identity).unwrap();
        assert_eq!(encoded.as_object().unwrap().len(), 4);
        assert_eq!(encoded["provider"], "api");
        assert_eq!(
            serde_json::from_value::<ModelIdentity>(encoded).unwrap(),
            identity
        );
    }
}

#[tokio::test]
async fn messages_observations_are_absolute_and_preserve_missing_fields_and_explicit_zero() {
    let body = [
        messages_start(json!({"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":7,"cache_creation_input_tokens":0})),
        messages_delta(json!({"output_tokens":2}), None),
        messages_delta(json!({"output_tokens":2}), None),
        messages_delta(json!({"output_tokens":4,"cache_read_input_tokens":null}), Some("end_turn")),
        event(json!({}), "message_stop"),
    ].concat();
    let (result, observations) = complete(Protocol::Messages, body).await;
    assert_eq!(observations.len(), 4);
    for (usage, output_tokens) in observations.iter().zip([0, 2, 2, 4]) {
        assert_eq!(
            *usage,
            Usage {
                input_tokens: Some(10),
                output_tokens: Some(output_tokens),
                cache_read_input_tokens: Some(7),
                cache_write_input_tokens: Some(0)
            }
        );
    }
    assert_eq!(result.unwrap().usage, observations[3]);
}

#[tokio::test]
async fn messages_regression_reports_other_valid_fields_before_protocol_failure() {
    let (result, observations) = complete(
        Protocol::Messages,
        [
            messages_start(json!({"input_tokens":10})),
            messages_delta(
                json!({"input_tokens":9,"output_tokens":3}),
                Some("end_turn"),
            ),
            event(json!({}), "message_stop"),
        ]
        .concat(),
    )
    .await;
    assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].input_tokens, Some(10));
    assert_eq!(
        observations[1],
        Usage {
            input_tokens: Some(9),
            output_tokens: Some(3),
            ..Usage::default()
        }
    );
}

#[tokio::test]
async fn every_adapter_preserves_independently_valid_counters_in_malformed_usage() {
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
    ] {
        let (input_name, output_name, read_name, write_name) = match protocol {
            Protocol::SystemOne => unreachable!("judgments do not stream conversation usage"),
            Protocol::Messages | Protocol::Responses => (
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ),
            Protocol::ChatCompletions => (
                "prompt_tokens",
                "completion_tokens",
                "cached_tokens",
                "cache_write_tokens",
            ),
        };
        for invalid in [
            json!(-1),
            json!(1.5),
            json!("9"),
            json!(true),
            serde_json::from_str("18446744073709551616").unwrap(),
        ] {
            for invalid_index in 0..4 {
                let mut counters = [json!(10), json!(4), json!(3), json!(0)];
                counters[invalid_index] = invalid.clone();
                let mut usage = json!({input_name:counters[0],output_name:counters[1]});
                if protocol == Protocol::ChatCompletions {
                    usage["prompt_tokens_details"] =
                        json!({read_name:counters[2],write_name:counters[3]});
                } else {
                    usage[read_name] = counters[2].clone();
                    usage[write_name] = counters[3].clone();
                }
                let body = match protocol {
                    Protocol::SystemOne => {
                        unreachable!("judgments do not stream conversation usage")
                    }
                    Protocol::Messages => messages_start(usage),
                    Protocol::Responses => [
                        response_start(),
                        response_terminal("completed", usage, json!({})),
                    ]
                    .concat(),
                    Protocol::ChatCompletions => chat_chunk(usage, Some("stop")),
                };
                let (result, observations) = complete(protocol, body).await;
                assert!(
                    matches!(result, Err(ModelError::InvalidResponse(_))),
                    "{protocol:?} {invalid_index}: {result:?}"
                );
                assert_eq!(observations.len(), 1);
                let expected = [Some(10), Some(4), Some(3), Some(0)];
                let actual = [
                    observations[0].input_tokens,
                    observations[0].output_tokens,
                    observations[0].cache_read_input_tokens,
                    observations[0].cache_write_input_tokens,
                ];
                for index in 0..4 {
                    assert_eq!(
                        actual[index],
                        if index == invalid_index {
                            None
                        } else {
                            expected[index]
                        },
                        "{protocol:?} counter {index}, invalid {invalid_index}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn malformed_later_message_usage_keeps_previous_values_and_new_valid_fields() {
    let (result, observations) = complete(
        Protocol::Messages,
        [
            messages_start(json!({"input_tokens":10,"cache_read_input_tokens":5})),
            messages_delta(
                json!({"input_tokens":-1,"output_tokens":4,"cache_creation_input_tokens":0}),
                None,
            ),
        ]
        .concat(),
    )
    .await;
    assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
    assert_eq!(
        observations.last().unwrap(),
        &Usage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            cache_read_input_tokens: Some(5),
            cache_write_input_tokens: Some(0)
        }
    );
}

#[tokio::test]
async fn message_stop_rejections_and_provider_errors_preserve_reported_usage() {
    for (reason, expected) in [
        ("max_tokens", ModelResponseRejection::OutputLimit),
        (
            "model_context_window_exceeded",
            ModelResponseRejection::ContextLimit,
        ),
        ("refusal", ModelResponseRejection::Refusal),
        (
            "pause_turn",
            ModelResponseRejection::UnsupportedContinuation,
        ),
    ] {
        let (result, observations) = complete(
            Protocol::Messages,
            [
                messages_start(json!({"input_tokens":10})),
                messages_delta(json!({"output_tokens":4}), Some(reason)),
                event(json!({}), "message_stop"),
            ]
            .concat(),
        )
        .await;
        assert!(matches!(result, Err(ModelError::ResponseRejected(reason)) if reason == expected));
        assert_eq!(observations.last().unwrap().output_tokens, Some(4));
    }
    let (result, observations) = complete(
        Protocol::Messages,
        [
            messages_start(json!({"input_tokens":10})),
            event(json!({"error":{"message":"secret-body"}}), "error"),
        ]
        .concat(),
    )
    .await;
    assert!(
        matches!(&result, Err(error) if error.kind() == ModelFailureKind::ProviderFailure && error.stage() == ModelRequestStage::ResponseStream)
    );
    assert_eq!(observations[0].input_tokens, Some(10));
    assert!(!format!("{result:?}").contains("secret-body"));
}

#[tokio::test]
async fn responses_failure_and_incomplete_envelopes_report_usage_before_typed_failure() {
    for (kind, extra, rejection) in [
        ("failed", json!({"error":{"message":"secret-body"}}), None),
        ("cancelled", json!({}), None),
        (
            "incomplete",
            json!({"incomplete_details":{"reason":"max_output_tokens"}}),
            Some(ModelResponseRejection::OutputLimit),
        ),
        (
            "incomplete",
            json!({"incomplete_details":{"reason":"content_filter"}}),
            Some(ModelResponseRejection::ContentFilter),
        ),
    ] {
        let body = [response_start(), response_terminal(kind, json!({"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":0}}), extra)].concat();
        let (result, observations) = complete(Protocol::Responses, body).await;
        match rejection {
            Some(expected) => assert!(
                matches!(result, Err(ModelError::ResponseRejected(reason)) if reason == expected)
            ),
            None => assert!(
                matches!(&result, Err(error) if error.kind() == ModelFailureKind::ProviderFailure && error.stage() == ModelRequestStage::ResponseStream)
            ),
        }
        assert_eq!(
            observations,
            vec![Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                cache_read_input_tokens: Some(3),
                cache_write_input_tokens: Some(0)
            }]
        );
        assert!(!format!("{result:?}").contains("secret-body"));
    }
}

#[tokio::test]
async fn response_usage_survives_output_status_and_sequence_validation_but_not_foreign_identity() {
    for extra in [
        json!({"output":"malformed"}),
        json!({"output":[{"id":"untracked"}]}),
        json!({"status":"failed"}),
    ] {
        let (result, observations) = complete(
            Protocol::Responses,
            [
                response_start(),
                response_terminal("completed", json!({"input_tokens":10}), extra),
            ]
            .concat(),
        )
        .await;
        assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
        assert_eq!(observations[0].input_tokens, Some(10));
    }
    let terminal = response_terminal("completed", json!({"input_tokens":10}), json!({}))
        .replace("\"sequence_number\":1", "\"sequence_number\":0");
    let (result, observations) =
        complete(Protocol::Responses, [response_start(), terminal].concat()).await;
    assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
    assert_eq!(observations[0].input_tokens, Some(10));
    for kind in ["completed", "failed", "incomplete", "cancelled"] {
        let (result, observations) = complete(
            Protocol::Responses,
            [
                response_start(),
                response_terminal(
                    kind,
                    json!({"input_tokens":999}),
                    json!({"id":"foreign-response"}),
                ),
            ]
            .concat(),
        )
        .await;
        assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
        assert!(observations.is_empty());
    }
}

#[tokio::test]
async fn chat_finish_rejections_preserve_usage_and_cache_aliases_are_not_added() {
    for (reason, rejection) in [
        ("length", Some(ModelResponseRejection::OutputLimit)),
        (
            "content_filter",
            Some(ModelResponseRejection::ContentFilter),
        ),
        ("insufficient_system_resource", None),
    ] {
        let (result, observations) = complete(Protocol::ChatCompletions, [chat_chunk(json!({"prompt_tokens":10,"completion_tokens":4,"prompt_cache_hit_tokens":3,"prompt_cache_miss_tokens":7,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":0}}), Some(reason)), "data: [DONE]\n\n".into()].concat()).await;
        match rejection {
            Some(expected) => assert!(
                matches!(result, Err(ModelError::ResponseRejected(reason)) if reason == expected)
            ),
            None => assert!(matches!(result, Err(ModelError::Http { status: 503 }))),
        }
        assert_eq!(
            observations,
            vec![Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                cache_read_input_tokens: Some(3),
                cache_write_input_tokens: Some(0)
            }]
        );
    }
}

#[tokio::test]
async fn chat_usage_survives_invalid_choices_and_provider_failure_but_not_foreign_identity() {
    for extra in [
        json!({"choices":"malformed"}),
        json!({"error":{"message":"provider failed"}}),
    ] {
        let mut chunk = json!({"id":"chat-1","usage":{"prompt_tokens":10},"choices":[]});
        chunk
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let (result, observations) =
            complete(Protocol::ChatCompletions, format!("data: {chunk}\n\n")).await;
        assert!(result.is_err());
        assert_eq!(observations[0].input_tokens, Some(10));
    }
    let foreign = chat_chunk(json!({"prompt_tokens":999}), Some("stop"))
        .replace("chat-1", "foreign-response");
    let (result, observations) = complete(
        Protocol::ChatCompletions,
        [chat_chunk(Value::Null, None), foreign].concat(),
    )
    .await;
    assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
    assert!(observations.is_empty());
}

#[tokio::test]
async fn interrupted_streams_keep_received_usage_without_inventing_unreported_fields() {
    for (protocol, body) in [
        (
            Protocol::Messages,
            messages_start(json!({"input_tokens":10})),
        ),
        (
            Protocol::ChatCompletions,
            chat_chunk(json!({"prompt_tokens":10}), None),
        ),
    ] {
        let (result, observations) = complete(protocol, body).await;
        assert!(matches!(result, Err(ModelError::StreamInterrupted)));
        assert_eq!(
            observations,
            vec![Usage {
                input_tokens: Some(10),
                ..Usage::default()
            }]
        );
    }
    let (result, observations) = complete(Protocol::Responses, response_start()).await;
    assert!(matches!(result, Err(ModelError::StreamInterrupted)));
    assert!(observations.is_empty());
}

#[tokio::test]
async fn completed_invalid_tool_calls_are_rejected_without_reclassifying_unfinished_streams() {
    for (name, arguments) in [("", "{}"), ("Read", "[1]"), ("Read", "{broken")] {
        let chat = format!(
            "data: {}\n\n",
            json!({
                "id":"chat-1", "usage":{"prompt_tokens":10},
                "choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":name,"arguments":arguments}}]},"finish_reason":"tool_calls"}]
            })
        );
        let item = json!({"type":"function_call","id":"fc-1","call_id":"call-1","name":name,"arguments":arguments});
        let responses = [
            response_start(),
            event(
                json!({"output_index":0,"item":item}),
                "response.output_item.added",
            ),
            event(
                json!({"output_index":0,"item":item}),
                "response.output_item.done",
            ),
        ]
        .concat();
        for (protocol, prefix, terminal) in [
            (Protocol::ChatCompletions, chat, "data: [DONE]\n\n".into()),
            (
                Protocol::Responses,
                responses,
                response_terminal(
                    "completed",
                    json!({"input_tokens":10}),
                    json!({"output":[item]}),
                ),
            ),
        ] {
            let (result, _) = complete(protocol, prefix.clone()).await;
            assert!(matches!(
                result,
                Err(ModelError::StreamInterruptedAfterOutput)
            ));
            let (result, observations) = complete(protocol, [prefix, terminal].concat()).await;
            assert!(
                matches!(
                    result,
                    Err(ModelError::ResponseRejected(
                        ModelResponseRejection::InvalidToolCall
                    ))
                ),
                "{protocol:?}: {result:?}"
            );
            assert_eq!(
                observations,
                vec![Usage {
                    input_tokens: Some(10),
                    ..Usage::default()
                }]
            );
        }
    }
    let (result, observations) = complete(Protocol::Messages, [
        messages_start(json!({"input_tokens":10})),
        event(json!({"index":0,"content_block":{"type":"tool_use","id":"call-1","name":"Read","input":{}}}), "content_block_start"),
        event(json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{broken"}}), "content_block_delta"),
        event(json!({"index":0}), "content_block_stop"),
    ].concat()).await;
    assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
    assert_eq!(observations[0].input_tokens, Some(10));
}

#[tokio::test]
async fn completed_responses_and_chat_refusals_keep_terminal_usage() {
    let item = json!({"type":"message","id":"msg-1","content":[{"type":"refusal","refusal":"Cannot help"}]});
    let responses = [
        response_start(),
        event(
            json!({"output_index":0,"item":item}),
            "response.output_item.added",
        ),
        event(
            json!({"output_index":0,"item":item}),
            "response.output_item.done",
        ),
        response_terminal(
            "completed",
            json!({"input_tokens":10}),
            json!({"output":[item]}),
        ),
    ]
    .concat();
    let chat = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"chat-1","choices":[{"index":0,"delta":{"refusal":"Cannot help"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10}})
    );
    for (protocol, body) in [
        (Protocol::Responses, responses),
        (Protocol::ChatCompletions, chat),
    ] {
        let (result, observations) = complete(protocol, body).await;
        assert!(matches!(
            result,
            Err(ModelError::ResponseRejected(
                ModelResponseRejection::Refusal
            ))
        ));
        assert_eq!(
            observations,
            vec![Usage {
                input_tokens: Some(10),
                ..Usage::default()
            }]
        );
    }
}
