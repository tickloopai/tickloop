use serde_json::json;
use tickloop_agent_model::{
    ModelRequest,
    auxiliary::{AuxiliaryTask, auxiliary_model_config},
    cards::ModelCatalog,
    credentials::save_api_key,
    model_client, validate_model_config, validate_reasoning_effort,
};
use tickloop_agent_protocol::{Content, Message, ModelRequestKind, Protocol, Role};
use tickloop_agent_test_support::model_http_support;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[test]
fn every_connection_resolves_an_auxiliary_card_with_legal_task_limits() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    for provider in &catalog.providers {
        let current = catalog.get(&provider.id, &provider.models[0].id).unwrap();
        for task in [
            AuxiliaryTask::Title,
            AuxiliaryTask::ShellDescription,
            AuxiliaryTask::AutoSetup,
            AuxiliaryTask::PermissionCheck,
            AuxiliaryTask::WebFetch,
            AuxiliaryTask::PromptSuggestion,
        ] {
            let model = auxiliary_model_config(&current, task).unwrap();
            let selected = provider
                .selection
                .as_ref()
                .unwrap()
                .auxiliary_model
                .as_ref()
                .unwrap();
            assert_eq!(&model.model_id, selected);
            assert_eq!(model.provider_id, current.provider_id);
            assert_eq!(model.auth_source, current.auth_source);
            assert_eq!(model.api_key_env, current.api_key_env);
            validate_model_config(&model).unwrap();
            assert!(
                model.max_output_tokens
                    <= catalog
                        .get(&provider.id, selected)
                        .unwrap()
                        .max_output_tokens
            );
            assert_eq!(model.model, model.card.as_ref().unwrap().id);
        }
    }
    for kind in [
        ModelRequestKind::Conversation,
        ModelRequestKind::CompactionAutomatic,
        ModelRequestKind::CompactionManual,
        ModelRequestKind::CompactionDirected,
    ] {
        assert!(AuxiliaryTask::for_request(kind).is_none());
    }
    let mut local = catalog.get("deepseek-api", "deepseek-flash").unwrap();
    local.protocol = Protocol::Messages;
    local.card.as_mut().unwrap().adapter = tickloop_agent_protocol::Adapter::Messages;
    local.base_url = "http://127.0.0.1:49321/fixture".into();
    validate_model_config(&local).unwrap();
    assert_eq!(
        auxiliary_model_config(&local, AuxiliaryTask::Title)
            .unwrap()
            .base_url,
        local.base_url
    );
}

fn sse_reply(protocol: Protocol) -> String {
    let frames = if protocol == Protocol::Responses {
        let item = json!({"type":"message","id":"msg","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]});
        vec![
            (
                "response.created",
                json!({"response":{"id":"aux","status":"in_progress"}}),
            ),
            (
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"message","id":"msg","role":"assistant","content":[]}}),
            ),
            (
                "response.output_item.done",
                json!({"output_index":0,"item":item}),
            ),
            (
                "response.completed",
                json!({"response":{"id":"aux","status":"completed","output":[item],"usage":{"input_tokens":12,"output_tokens":2}}}),
            ),
        ]
    } else {
        vec![
            (
                "message_start",
                json!({"message":{"role":"assistant","content":[],"usage":{"input_tokens":12}}}),
            ),
            (
                "content_block_start",
                json!({"index":0,"content_block":{"type":"text","text":"done"}}),
            ),
            ("content_block_stop", json!({"index":0})),
            (
                "message_delta",
                json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ),
            ("message_stop", json!({})),
        ]
    };
    frames
        .into_iter()
        .enumerate()
        .map(|(sequence, (kind, mut value))| {
            value["type"] = json!(kind);
            if protocol == Protocol::Responses {
                value["sequence_number"] = json!(sequence);
            }
            format!("event: {kind}\ndata: {value}\n\n")
        })
        .collect()
}

#[tokio::test]
async fn auxiliary_http_clients_share_credentials_and_transport_but_leave_chat_model_unchanged() {
    for (provider, main, auxiliary, protocol, budget_field) in [
        (
            "openai-api",
            "gpt-6.1-sol",
            "gpt-6-luna",
            Protocol::Responses,
            "max_output_tokens",
        ),
        (
            "anthropic-api",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            Protocol::Messages,
            "max_tokens",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let (endpoint, mut captured) =
            model_http_support::serve_responses(vec![(200, sse_reply(protocol)); 4]).await;
        let mut configuration = ModelCatalog::defaults().get(provider, main).unwrap();
        configuration.base_url = endpoint;
        save_api_key(&root, &configuration.api_key_env, "auxiliary-fixture-key").unwrap();
        let client = model_client(configuration.clone(), Some(&root)).unwrap();
        for (task, tokens) in [
            (AuxiliaryTask::Title, 512),
            (AuxiliaryTask::PermissionCheck, 4096),
            (AuxiliaryTask::WebFetch, 8192),
        ] {
            let fast = client.auxiliary_client(task).unwrap().unwrap();
            assert_eq!(fast.identity().requested_model, auxiliary);
            let effort = fast.minimum_reasoning_effort();
            validate_reasoning_effort(
                &auxiliary_model_config(&configuration, task).unwrap(),
                effort,
            )
            .unwrap();
            let (events, _) = mpsc::unbounded_channel();
            fast.complete(
                ModelRequest {
                    messages: vec![Message {
                        role: Role::User,
                        content: vec![Content::Text {
                            text: "Summarize the supplied data".into(),
                        }],
                    }],
                    history_sources: vec![],
                    tools: vec![],
                    structured_output: None,
                    temperature: None,
                    reasoning_effort: effort,
                },
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let (headers, body) = captured.recv().await.unwrap();
            assert!(headers.contains("auxiliary-fixture-key"));
            assert_eq!(body["model"], auxiliary);
            assert_eq!(body[budget_field], tokens);
            assert!(body.get("temperature").is_none());
            assert_eq!(client.identity().requested_model, main);
        }
        let (events, _) = mpsc::unbounded_channel();
        client
            .complete(
                ModelRequest {
                    messages: vec![Message {
                        role: Role::User,
                        content: vec![Content::Text {
                            text: "Continue the Chat".into(),
                        }],
                    }],
                    history_sources: vec![],
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
        let (_, body) = captured.recv().await.unwrap();
        assert_eq!(body["model"], main);
        assert_eq!(body[budget_field], json!(configuration.max_output_tokens));
        assert!(configuration.accepts_images());
    }
}
