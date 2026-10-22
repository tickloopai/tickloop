use tickloop_agent_test_support::model_http_support;

use serde_json::{Value, json};
use std::sync::Arc;
use tickloop_agent_model::{LocalModelClients, ModelClients, cards::ModelCatalog};
use tickloop_agent_protocol::{
    Decision, EventKind, ExecutionMode, ModelAuthSource, ModelConfig, ModelRequestKind, Protocol,
    RequestOutcome,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::*;

fn messages_response(blocks: Vec<Value>) -> String {
    let frame = |kind: &str, mut value: Value| {
        value["type"] = json!(kind);
        format!("event: {kind}\ndata: {value}\n\n")
    };
    let mut output = frame(
        "message_start",
        json!({"message":{"role":"assistant","content":[],"usage":{"input_tokens":20}}}),
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
        json!({"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),
    );
    output += &frame("message_stop", json!({}));
    output
}

fn chat_response(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"review-fixture","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
        json!({"id":"review-fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":10}})
    )
}

fn responses_response(items: Vec<Value>) -> String {
    let event = |kind: &str, sequence: u64, mut value: Value| {
        value["type"] = json!(kind);
        value["sequence_number"] = json!(sequence);
        format!("event: {kind}\ndata: {value}\n\n")
    };
    let mut output = event(
        "response.created",
        0,
        json!({"response":{"id":"review-response","status":"in_progress"}}),
    );
    let mut sequence = 1;
    for (index, item) in items.iter().enumerate() {
        output += &event(
            "response.output_item.added",
            sequence,
            json!({"output_index":index,"item":item}),
        );
        sequence += 1;
        output += &event(
            "response.output_item.done",
            sequence,
            json!({"output_index":index,"item":item}),
        );
        sequence += 1;
    }
    output += &event(
        "response.completed",
        sequence,
        json!({"response":{"id":"review-response","status":"completed","output":items,"usage":{"input_tokens":20,"output_tokens":10}}}),
    );
    output
}

fn google_response(parts: Vec<Value>) -> String {
    format!(
        "data: {}\n\n",
        json!({"responseId":"review-google","candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":"STOP"}]})
    )
}

#[tokio::test]
async fn legacy_and_server_tool_shapes_are_terminal_before_text_is_parsed() {
    use tickloop_agent_model::{ModelResponseRejection, StructuredOutput};
    use tickloop_agent_protocol::{Message, Role};
    let text = auto_classification(false).to_string();
    for (protocol, response) in [
        (
            Protocol::Messages,
            messages_response(vec![
                json!({"type":"text","text":text}),
                json!({"type":"server_tool_use","id":"unexpected","name":"Bash","input":{}}),
            ]),
        ),
        (
            Protocol::ChatCompletions,
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"id":"legacy","choices":[{"index":0,"delta":{"role":"assistant","content":text,"function_call":{"name":"Bash","arguments":"{broken"}},"finish_reason":"stop"}]})
            ),
        ),
        (
            Protocol::Responses,
            responses_response(vec![
                json!({"type":"tool_use","id":"unexpected","name":"Bash","input":{},"status":"completed"}),
            ]),
        ),
    ] {
        let (url, captured) = model_http_support::serve_response(200, response).await;
        let mut model = json_model(protocol);
        model.base_url = if protocol == Protocol::Messages {
            url
        } else {
            url.trim_end_matches("/anthropic").into()
        };
        let client = LocalModelClients::new(None).client(&model).unwrap();
        let request = tickloop_agent_model::ModelRequest {
            history_sources: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: tickloop_agent_test_support::text("Review the operation"),
            }],
            tools: Vec::new(),
            structured_output: Some(StructuredOutput {
                name: "classify_result".into(),
                schema: json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            }),
            temperature: None,
            reasoning_effort: None,
        };
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        assert!(
            matches!(
                client
                    .complete(request, events, tokio_util::sync::CancellationToken::new())
                    .await,
                Err(tickloop_agent_model::ModelError::ResponseRejected(
                    ModelResponseRejection::UnexpectedToolOperation
                ))
            ),
            "{protocol:?}"
        );
        captured.await.unwrap();
    }
}

async fn unexpected_tool_response_requires_manual(mut reviewer_config: ModelConfig) {
    let credentials = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    if reviewer_config.provider_id == "kimi-code-oauth" {
        use std::os::unix::fs::PermissionsExt;
        reviewer_config.auth_source = ModelAuthSource::KimiOAuth;
        let auth = credentials.path().join("auth");
        std::fs::create_dir(&auth).unwrap();
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = auth.join("kimi-code-oauth.json");
        std::fs::write(&path, json!({"provider":"kimi","generation":uuid::Uuid::new_v4(),"credentials":{"token":"fixture-subscription","refresh_token":"fixture-refresh","identity":uuid::Uuid::new_v4(),"expires_at_ms":u64::MAX}}).to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    } else {
        reviewer_config.auth_source = ModelAuthSource::ApiKeyEnv;
    }
    reviewer_config.api_key_env.clear();
    reviewer_config.connection = None;
    let findings = auto_classification(false);
    let valid_text = findings.to_string();
    for (input, malformed_tool_first) in [
        (json!({"command":"false"}), false),
        (json!("{truncated arguments"), false),
        (json!("{truncated arguments"), true),
    ] {
        let unexpected = match reviewer_config.protocol {
            Protocol::Messages => {
                let mut blocks = vec![
                    json!({"type":"text","text":valid_text}),
                    json!({"type":"tool_use","id":"unexpected","name":"Bash","input":input}),
                ];
                if malformed_tool_first {
                    blocks.insert(0, json!({"type":"tool_use","id":"malformed-result","name":"classify_result","input":"not an object"}));
                }
                messages_response(blocks)
            }
            Protocol::ChatCompletions => {
                let arguments = if input.is_object() {
                    input.to_string()
                } else {
                    input.as_str().unwrap().into()
                };
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({"id":"unexpected-review","choices":[{"index":0,"delta":{"role":"assistant","content":valid_text,"tool_calls":[{"index":0,"id":"unexpected","type":"function","function":{"name":"Bash","arguments":arguments}}]},"finish_reason":"tool_calls"}]})
                )
            }
            Protocol::Responses => responses_response(vec![
                json!({"type":"message","id":"text","role":"assistant","status":"completed","content":[{"type":"output_text","text":valid_text}]}),
                json!({"type":"function_call","id":"operation","call_id":"unexpected","name":"Bash","arguments":if input.is_object() { input.to_string() } else { input.as_str().unwrap().into() },"status":"completed"}),
            ]),
            Protocol::Google => google_response(vec![
                json!({"text":valid_text,"functionCall":{"name":"Bash","args":input}}),
            ]),
            Protocol::SystemOne => unreachable!(),
        };
        let repair = match reviewer_config.protocol {
            Protocol::Messages => messages_response(vec![json!({"type":"text","text":valid_text})])
                .replace(
                    "\"stop_reason\":\"tool_use\"",
                    "\"stop_reason\":\"end_turn\"",
                ),
            Protocol::ChatCompletions => chat_response(&valid_text),
            Protocol::Responses => responses_response(vec![
                json!({"type":"message","id":"repaired","role":"assistant","status":"completed","content":[{"type":"output_text","text":valid_text}]}),
            ]),
            Protocol::Google => google_response(vec![json!({"text":valid_text})]),
            Protocol::SystemOne => unreachable!(),
        };
        let (url, mut requests) =
            model_http_support::serve_responses(vec![(200, unexpected), (200, repair)]).await;
        reviewer_config.base_url = if reviewer_config.protocol == Protocol::Messages {
            url
        } else {
            url.trim_end_matches("/anthropic").into()
        };
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call(
                    "link",
                    "Bash",
                    json!({"command":"ln -s missing-target blocked"}),
                ),
                text("done"),
            ])),
        );
        let reviewer = if reviewer_config.auth_source == ModelAuthSource::KimiOAuth {
            tickloop_agent_model::LocalModelClients::new(Some(credentials.path().to_path_buf()))
                .client(&reviewer_config)
                .unwrap()
        } else {
            LocalModelClients::new(None)
                .client(&reviewer_config)
                .unwrap()
        };
        agent.set_auto_reviewer(reviewer).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Create the blocked link".into())
            .await
            .unwrap();
        let mut saw_manual = false;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionChecked { decision, reason } => {
                    assert_eq!(
                        decision,
                        Decision::Ask,
                        "{}: {reason}",
                        reviewer_config.name
                    );
                    assert!(
                        reason.contains("review this action manually"),
                        "{reason}; {:?}",
                        agent.snapshot(chat).await.unwrap().request_usage.entries()
                    );
                }
                EventKind::PermissionRequested { request } => {
                    saw_manual = true;
                    agent
                        .answer_permission(chat, request.id, false)
                        .await
                        .unwrap();
                }
                EventKind::ToolStarted { .. } => {
                    panic!("unexpected reviewer operation was repaired into an Allow")
                }
                EventKind::Completed { .. } | EventKind::Interrupted => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(saw_manual);
        assert!(directory.path().join("blocked").symlink_metadata().is_err());
        let ledger = agent.snapshot(chat).await.unwrap().request_usage;
        assert_eq!(
            ledger
                .entries()
                .iter()
                .filter(|entry| entry.kind == ModelRequestKind::PermissionCheck)
                .count(),
            1
        );
        requests.recv().await.unwrap();
        assert!(
            requests.try_recv().is_err(),
            "the subsequent valid Allow must never be requested"
        );
        agent.shutdown().await;
    }
}

fn json_model(protocol: Protocol) -> ModelConfig {
    let mut model = config();
    model.protocol = protocol;
    model
}

#[tokio::test]
async fn messages_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(json_model(Protocol::Messages)).await;
}
#[tokio::test]
async fn responses_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(json_model(Protocol::Responses)).await;
}
#[tokio::test]
async fn google_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(json_model(Protocol::Google)).await;
}
#[tokio::test]
async fn chat_json_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(json_model(Protocol::ChatCompletions)).await;
}
#[tokio::test]
async fn minimax_chat_json_unexpected_operations_are_terminal() {
    let mut model = json_model(Protocol::ChatCompletions);
    model.model = "MiniMax-M3.1-Flash-Preview".into();
    unexpected_tool_response_requires_manual(model).await;
}
#[tokio::test]
async fn deepseek_chat_json_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(
        ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap(),
    )
    .await;
}
#[tokio::test]
async fn anthropic_messages_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(
        ModelCatalog::defaults()
            .get("anthropic-api", "claude-haiku-5-5")
            .unwrap(),
    )
    .await;
}
#[tokio::test]
async fn minimax_messages_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(
        ModelCatalog::defaults()
            .get("minimax-cn-oauth", "MiniMax-M3.1-Flash-Preview")
            .unwrap(),
    )
    .await;
}
#[tokio::test]
async fn glm_json_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(
        ModelCatalog::defaults()
            .get("zai-global-api", "glm-5.3")
            .unwrap(),
    )
    .await;
}
#[tokio::test]
async fn kimi_json_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(
        ModelCatalog::defaults()
            .get("kimi-global-api", "kimi-k3")
            .unwrap(),
    )
    .await;
}
#[tokio::test]
async fn qwen_json_unexpected_operations_are_terminal() {
    unexpected_tool_response_requires_manual(
        ModelCatalog::defaults()
            .get("alibaba-cn-api", "qwen3.8-flash")
            .unwrap(),
    )
    .await;
}
#[tokio::test]
async fn kimi_oauth_prompted_json_rejects_unexpected_operations() {
    unexpected_tool_response_requires_manual(
        ModelCatalog::defaults()
            .get("kimi-code-oauth", "k3")
            .unwrap(),
    )
    .await;
}

#[tokio::test]
async fn unnormalized_model_tools_never_repair_into_allowing_findings() {
    for operation in [
        call("unexpected", "Bash", json!({"command":"false"})),
        vec![tickloop_agent_protocol::Content::ToolResult {
            tool_use_id: "unexpected".into(),
            content: "result".into(),
            is_error: false,
        }],
    ] {
        for malformed_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let agent = Agent::new_embedded(
                config(),
                Arc::new(ScriptedModel::new(vec![
                    call(
                        "link",
                        "Bash",
                        json!({"command":"ln -s missing-target blocked"}),
                    ),
                    text("done"),
                ])),
            );
            let mut mixed = text(&auto_classification(false).to_string());
            mixed.extend(operation.clone());
            let mut replies = Vec::new();
            if malformed_first {
                replies.push(text("invalid classification"));
            }
            replies.extend([mixed, text(&auto_classification(false).to_string())]);
            let reviewer = Arc::new(ScriptedModel::new(replies));
            agent.set_auto_reviewer(reviewer.clone()).await;
            let chat = agent.create_chat(directory.path().into()).await.unwrap();
            agent
                .set_execution_mode(chat, ExecutionMode::Auto)
                .await
                .unwrap();
            let mut events = agent.subscribe();
            agent
                .submit(chat, "Create the blocked link".into())
                .await
                .unwrap();
            let mut saw_manual = false;
            loop {
                match next_event(&mut events).await.kind {
                    EventKind::PermissionChecked { decision, reason } => {
                        assert_eq!(decision, Decision::Ask);
                        assert!(reason.contains("unexpected tool operations"), "{reason}");
                    }
                    EventKind::PermissionRequested { request } => {
                        saw_manual = true;
                        agent
                            .answer_permission(chat, request.id, false)
                            .await
                            .unwrap();
                    }
                    EventKind::ToolStarted { .. } => {
                        panic!("unexpected reviewer tool admitted the original operation")
                    }
                    EventKind::Completed { .. } | EventKind::Interrupted => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            assert!(saw_manual);
            assert_eq!(
                reviewer.requests.lock().unwrap().len(),
                1 + usize::from(malformed_first)
            );
            assert!(directory.path().join("blocked").symlink_metadata().is_err());
            agent.shutdown().await;
        }
    }
}

#[tokio::test]
async fn minimax_preview_and_deepseek_recover_malformed_review_without_relaxing_policy() {
    for (provider, model) in [
        ("minimax-cn-oauth", "MiniMax-M3.1-Flash-Preview"),
        ("deepseek-api", "deepseek-flash"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut reviewer_config = ModelCatalog::defaults().get(provider, model).unwrap();
        reviewer_config.auth_source = ModelAuthSource::ApiKeyEnv;
        reviewer_config.api_key_env.clear();
        reviewer_config.connection = None;
        let findings = auto_classification(false);
        let mut invalid = findings.clone();
        invalid["decision"] = json!("invalid_decision");
        let (malformed, valid) = if reviewer_config.protocol == Protocol::ChatCompletions {
            (
                chat_response(&invalid.to_string()),
                chat_response(&findings.to_string()),
            )
        } else {
            let json_response = |value: &serde_json::Value| {
                messages_response(vec![json!({"type":"text","text":value.to_string()})]).replace(
                    "\"stop_reason\":\"tool_use\"",
                    "\"stop_reason\":\"end_turn\"",
                )
            };
            (json_response(&invalid), json_response(&findings))
        };
        let (url, mut requests) =
            model_http_support::serve_responses(vec![(200, malformed), (200, valid)]).await;
        reviewer_config.base_url = if reviewer_config.protocol == Protocol::ChatCompletions {
            url.trim_end_matches("/anthropic").into()
        } else {
            url
        };
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call(
                    "link",
                    "Bash",
                    json!({"command":"ln -s missing-target checked"}),
                ),
                text("done"),
            ])),
        );
        agent.install_own_tickhost();
        agent
            .set_auto_reviewer(
                LocalModelClients::new(None)
                    .client(&reviewer_config)
                    .unwrap(),
            )
            .await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Create the checked link".into())
            .await
            .unwrap();
        let mut checked = false;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionChecked { decision, .. } => {
                    assert_eq!(decision, Decision::Allow, "{model}");
                    checked = true;
                }
                EventKind::PermissionRequested { .. } => {
                    panic!("{model}: malformed review was not repaired")
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{model}: {error}"),
                _ => {}
            }
        }
        assert!(checked);
        assert!(directory.path().join("checked").symlink_metadata().is_ok());
        let (_, first) = requests.recv().await.unwrap();
        let (_, second) = requests.recv().await.unwrap();
        assert_ne!(first, second, "the retry must provide format feedback");
        assert!(!second.to_string().contains("UNTRUSTED-REVIEW-TEXT"));
        let ledger = agent.snapshot(chat).await.unwrap().request_usage;
        let reviews = ledger
            .entries()
            .iter()
            .filter(|record| record.kind == ModelRequestKind::PermissionCheck)
            .collect::<Vec<_>>();
        assert_eq!(reviews.len(), 2);
        assert_ne!(reviews[0].outcome, RequestOutcome::Accepted);
        assert_eq!(reviews[1].outcome, RequestOutcome::Accepted);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn default_llm_review_uses_the_provider_auxiliary_model_and_records_its_identity() {
    use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
    use tokio::sync::mpsc::UnboundedSender;
    use tokio_util::sync::CancellationToken;

    struct RoutedConversation {
        conversation: ScriptedModel,
        transport: Arc<dyn ModelClient>,
    }
    #[async_trait::async_trait]
    impl ModelClient for RoutedConversation {
        fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
            self.transport.identity()
        }
        fn auxiliary_client(
            &self,
            task: tickloop_agent_model::auxiliary::AuxiliaryTask,
        ) -> Result<Option<Arc<dyn ModelClient>>, ModelError> {
            self.transport.auxiliary_client(task)
        }
        async fn complete(
            &self,
            request: ModelRequest,
            events: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            self.conversation.complete(request, events, cancel).await
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().canonicalize().unwrap().join("state");
    tickloop_agent_base::private_fs::create_private_directory(&state).unwrap();
    let response = messages_response(vec![
        json!({"type":"text","text":auto_classification(false).to_string()}),
    ])
    .replace(
        "\"stop_reason\":\"tool_use\"",
        "\"stop_reason\":\"end_turn\"",
    );
    let (endpoint, mut captured) = model_http_support::serve_responses(vec![(200, response)]).await;
    let mut model = ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5-5")
        .unwrap();
    model.base_url = endpoint;
    tickloop_agent_model::credentials::save_api_key(
        &state,
        &model.api_key_env,
        "auxiliary-review-fixture",
    )
    .unwrap();
    let transport = tickloop_agent_model::LocalModelClients::new(Some(state.to_path_buf()))
        .client(&model)
        .unwrap();
    let client = Arc::new(RoutedConversation {
        conversation: ScriptedModel::new(vec![
            call(
                "link",
                "Bash",
                json!({"command":"ln -s missing-target auxiliary-link"}),
            ),
            text("done"),
        ]),
        transport,
    });
    let agent =
        Agent::open_embedded(state, model, tickloop_agent_model::fixture_client(client)).unwrap();
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            chat,
            "Create the auxiliary-link symlink to missing-target".into(),
        )
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            EventKind::PermissionRequested { .. } => {
                panic!("fixture reviewer should allow the isolated test operation")
            }
            _ => {}
        }
    }
    let (_, body) = captured.recv().await.unwrap();
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["thinking"]["type"], "disabled");
    assert!(body.get("temperature").is_none());
    assert!(body["output_config"]["format"].is_object());
    let snapshot = agent.snapshot(chat).await.unwrap();
    let reviews = snapshot
        .request_usage
        .entries()
        .iter()
        .filter(|entry| entry.kind == ModelRequestKind::PermissionCheck)
        .collect::<Vec<_>>();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].model.requested_model, "claude-haiku-5-5");
    assert_eq!(reviews[0].outcome, RequestOutcome::Accepted);
    assert_eq!(snapshot.model.model, "claude-sonnet-5-5");
    assert!(
        directory
            .path()
            .join("auxiliary-link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    agent.shutdown().await;
}
