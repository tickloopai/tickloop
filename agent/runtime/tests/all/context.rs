use std::sync::Arc;
use tickloop_agent_model::{ModelConfigExt, cards::ModelCatalog};
use tickloop_agent_protocol::{
    Content, Decision, EventKind, ModelChange, PermissionRule, Protocol, ReasoningEffort, Usage,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_runtime::ChatSnapshotExt;
use tickloop_agent_test_support::{ScriptedModel, next_event, persisted_config as config, text};
use uuid::Uuid;

#[tokio::test]
async fn minimax_thinking_toggle_and_on_preference_survive_new_chats() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let model = ModelCatalog::defaults()
        .get("minimax-cn-api", "minimax-m3-messages")
        .unwrap();
    tickloop_agent_model::credentials::save_api_key(&root, &model.api_key_env, "test-minimax-key")
        .unwrap();
    let client = Arc::new(ScriptedModel::new(vec![text("Thinking on")]));
    let agent = Agent::open_embedded(root.clone(), model, client.clone()).unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    let before = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        before
            .model
            .thinking_enabled(before.reasoning_effort, before.thinking_enabled),
        Some(false)
    );
    agent.set_thinking_enabled(chat, true).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Hello".into()).await.unwrap();
    wait_for_completion(&mut events).await;
    assert_eq!(
        client.requests.lock().unwrap()[0].reasoning_effort,
        Some(ReasoningEffort::On)
    );
    let next = agent.create_chat(root).await.unwrap();
    let saved = agent.snapshot(next).await.unwrap();
    assert_eq!(saved.reasoning_effort, Some(ReasoningEffort::On));
    assert_eq!(
        saved
            .model
            .thinking_enabled(saved.reasoning_effort, saved.thinking_enabled),
        Some(true)
    );
    agent.set_thinking_enabled(next, false).await.unwrap();
    agent
        .set_reasoning_effort(next, Some(ReasoningEffort::On))
        .await
        .unwrap();
    assert!(agent.snapshot(next).await.unwrap().thinking_enabled);
    agent.shutdown().await;
}

struct PausedSummary {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    ordinary_requests: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl tickloop_agent_model::ModelClient for PausedSummary {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        if request.tools.is_empty() {
            self.started.notify_one();
            tokio::select! {_=cancel.cancelled()=>return Err(tickloop_agent_model::ModelError::Cancelled),_=self.release.notified()=>{}}
        } else {
            self.ordinary_requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(tickloop_agent_model::ModelOutput {
            private_source: None,
            content: text("A complete response"),
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn manual_compaction_serializes_queued_input_and_interrupt_cancels_the_summary() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(PausedSummary {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        ordinary_requests: std::sync::atomic::AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "First turn".into()).await.unwrap();
    wait_for_completion(&mut events).await;
    let compacting = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.compact(chat).await })
    };
    model.started.notified().await;
    agent
        .submit(chat, Some(receipt.loop_id), "Queued continuation".into())
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        model
            .ordinary_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    agent.interrupt(chat).await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), compacting)
        .await
        .expect("interrupt must cancel the summary")
        .unwrap();
    assert!(result.is_err());
    wait_for_completion(&mut events).await;
    assert_eq!(
        model
            .ordinary_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn shutdown_joins_a_summary_even_when_its_original_caller_is_gone() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(PausedSummary {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        ordinary_requests: std::sync::atomic::AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "First turn".into()).await.unwrap();
    wait_for_completion(&mut events).await;
    let before = agent.snapshot(chat).await.unwrap();
    let compacting = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.compact(chat).await })
    };
    model.started.notified().await;
    compacting.abort();
    let _ = compacting.await;
    tokio::time::timeout(std::time::Duration::from_secs(2), agent.shutdown())
        .await
        .expect("shutdown must own and cancel compaction after its caller is dropped");
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.message_compactions, before.message_compactions);
    assert!(after.events.iter().any(|event| matches!(
        event.kind,
        EventKind::ContextCompaction {
            status: tickloop_agent_protocol::CompactionStatus::Failed,
            ..
        }
    )));
}

async fn wait_for_completion(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) {
    loop {
        match next_event(events).await.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn switching_an_idle_chat_keeps_the_goal_and_uses_the_new_model_with_history() {
    let directory = tempfile::tempdir().unwrap();
    let original = Arc::new(ScriptedModel::new(vec![text("Original answer")]));
    let agent = Agent::new_embedded(config(), original.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "My goal".into()).await.unwrap();
    wait_for_completion(&mut events).await;
    let replacement = Arc::new(ScriptedModel::new(vec![text("New answer")]));
    let mut selected = config();
    selected.name = "replacement".into();
    selected.model = "replacement".into();
    let selected = tickloop_agent_model::cards::model_for_persistent_selection(selected);
    assert_eq!(
        agent
            .set_model(chat, selected.clone(), replacement.clone())
            .await
            .unwrap(),
        ModelChange::Applied
    );
    agent
        .submit(chat, Some(receipt.loop_id), "Continue".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.model, selected);
    assert_eq!(snapshot.loops.len(), 1);
    assert_eq!(snapshot.loops[0].id, receipt.loop_id);
    assert_eq!(original.requests.lock().unwrap().len(), 1);
    assert!(
        replacement.requests.lock().unwrap()[0]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content,Content::Text{text} if text=="Original answer"))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn switching_model_and_thinking_during_tool_approval_waits_until_the_old_round_closes() {
    let directory = tempfile::tempdir().unwrap();
    let original = Arc::new(ScriptedModel::new(vec![
        tickloop_agent_test_support::call(
            "write-1",
            "Write",
            serde_json::json!({"file_path":"result.txt","content":"done"}),
        ),
        text("Old round completed"),
    ]));
    let agent = Agent::new_embedded(config(), original.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Write the result".into())
        .await
        .unwrap();
    let permission = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    let replacement = Arc::new(ScriptedModel::new(vec![text("New round completed")]));
    let mut selected = ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash-responses")
        .unwrap();
    selected.api_key_env = "PATH".into();
    assert_eq!(
        agent
            .set_model_with_thinking(
                chat,
                selected.clone(),
                replacement.clone(),
                Some(ReasoningEffort::High),
            )
            .await
            .unwrap(),
        ModelChange::Pending
    );
    let pending = agent.snapshot(chat).await.unwrap();
    assert_eq!(pending.model.protocol, Protocol::Messages);
    assert_eq!(pending.pending_model, Some(selected.clone()));
    assert_eq!(pending.reasoning_effort, None);
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    assert_eq!(
        std::fs::read_to_string(directory.path().join("result.txt")).unwrap(),
        "done"
    );
    assert_eq!(original.requests.lock().unwrap().len(), 2);
    assert!(
        original
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.reasoning_effort.is_none())
    );
    assert!(replacement.requests.lock().unwrap().is_empty());
    let applied = agent.snapshot(chat).await.unwrap();
    assert_eq!(applied.model, selected);
    assert_eq!(applied.reasoning_effort, Some(ReasoningEffort::High));
    assert!(applied.thinking_enabled);
    agent
        .submit(chat, Some(receipt.loop_id), "Continue".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    assert_eq!(replacement.requests.lock().unwrap().len(), 1);
    assert_eq!(
        replacement.requests.lock().unwrap()[0].reasoning_effort,
        Some(ReasoningEffort::High)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn model_switch_projects_private_history_and_rejects_invalid_configuration_without_mutation()
{
    let directory = tempfile::tempdir().unwrap();
    let original = Arc::new(ScriptedModel::new(vec![
        vec![
            Content::Thinking {
                thinking: "readable historical reasoning".into(),
                signature: Some("signature".into()),
            },
            Content::Text {
                text: "Public answer".into(),
            },
        ],
        text("Back on A"),
    ]));
    let agent = Agent::new_embedded(config(), original.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "My goal".into()).await.unwrap();
    wait_for_completion(&mut events).await;
    let before = agent.snapshot(chat).await.unwrap();
    let mut selected = config();
    selected.protocol = Protocol::Responses;
    let replacement = Arc::new(ScriptedModel::new(vec![text("Model B reply")]));
    assert_eq!(
        agent
            .set_model(chat, selected.clone(), replacement.clone())
            .await
            .unwrap(),
        ModelChange::Applied
    );
    let switched = agent.snapshot(chat).await.unwrap();
    assert_eq!(before.messages, switched.messages);
    selected.base_url = "invalid".into();
    assert!(
        agent
            .set_model(chat, selected, replacement.clone())
            .await
            .is_err()
    );
    assert_eq!(agent.snapshot(chat).await.unwrap().model, switched.model);
    agent
        .submit(chat, Some(receipt.loop_id), "Continue on B".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    {
        let requests = replacement.requests.lock().unwrap();
        assert!(requests[0].messages.iter().flat_map(|message| &message.content).any(|block| matches!(block, Content::Text {text} if text == "readable historical reasoning")));
        assert!(
            !requests[0]
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(
                    block,
                    Content::Thinking { .. } | Content::ProtocolData { .. }
                ))
        );
    }
    agent
        .set_model(chat, config(), original.clone())
        .await
        .unwrap();
    agent
        .submit(chat, Some(receipt.loop_id), "Continue on A".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    assert!(original.requests.lock().unwrap()[1].messages.iter().flat_map(|message| &message.content).any(|block| matches!(block, Content::Thinking {signature:Some(signature),..} if signature == "signature")));
    agent.shutdown().await;
}

#[tokio::test]
async fn compaction_appends_a_boundary_and_restart_loads_only_the_compacted_chain() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("history.sqlite");
    let original = Arc::new(ScriptedModel::new(vec![
        vec![
            Content::Thinking {
                thinking: "private chain".into(),
                signature: Some("signature".into()),
            },
            Content::Text {
                text: "Original public progress".into(),
            },
        ],
        text("Summary: preserve the source; generate a separate report."),
    ]));
    let agent = Agent::open_embedded(database.clone(), config(), original.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(
            chat,
            None,
            "Preserve the source. Generate a separate report.".into(),
        )
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let before = agent.snapshot(chat).await.unwrap();
    let outcome = agent.compact(chat).await.unwrap();
    assert!(outcome.summary.contains("separate report"));
    let compacted = agent.snapshot(chat).await.unwrap();
    assert!(compacted.messages.starts_with(&before.messages));
    assert_eq!(compacted.messages.len(), before.messages.len() + 2);
    assert!(compacted.is_compact_boundary(before.messages.len()));
    assert!(compacted.is_compact_summary(before.messages.len() + 1));
    let projected = compacted.conversation_messages().unwrap();
    assert_eq!(projected.len(), 1);
    assert!(
        !projected
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            ))
    );
    {
        let requests = original.requests.lock().unwrap();
        let summary_request = &requests[1];
        assert!(summary_request.tools.is_empty());
        assert!(summary_request.messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,Content::Text{text} if text.contains("Preserve the source"))));
    }
    let mut selected = config();
    selected.protocol = Protocol::Responses;
    let replacement = Arc::new(ScriptedModel::new(vec![text("Continued from the summary")]));
    agent
        .set_model(chat, selected.clone(), replacement.clone())
        .await
        .unwrap();
    agent
        .submit(chat, Some(receipt.loop_id), "Continue carefully".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    assert!(replacement.requests.lock().unwrap()[0].messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,Content::Text{text} if text.contains("Summary: preserve the source"))));
    let in_process = agent
        .snapshot(chat)
        .await
        .unwrap()
        .conversation_messages()
        .unwrap();
    agent.shutdown().await;
    drop(agent);
    let restored =
        Agent::open_embedded(database, selected, Arc::new(ScriptedModel::new(vec![]))).unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert!(snapshot.is_compact_boundary(0));
    assert_eq!(
        snapshot.latest_compact_summary(),
        compacted.latest_compact_summary()
    );
    assert_eq!(snapshot.conversation_messages().unwrap(), in_process);
    assert!(!snapshot.messages.contains(&before.messages[0]));
    restored.shutdown().await;
}

#[tokio::test]
async fn failed_compaction_keeps_the_last_valid_context_and_raw_history() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("Answer"),
        text("A valid public summary"),
        Vec::new(),
        text("Continued after summary failure"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Keep my instructions".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    agent.compact(chat).await.unwrap();
    let before = agent.snapshot(chat).await.unwrap();
    assert!(
        agent
            .compact(chat)
            .await
            .unwrap_err()
            .to_string()
            .contains("empty summary")
    );
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.message_compactions, before.message_compactions);
    agent
        .submit(chat, Some(receipt.loop_id), "Continue".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    assert!(model.requests.lock().unwrap()[3].messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,Content::Text{text} if text.contains("valid public summary"))));
    agent.shutdown().await;
}

#[tokio::test]
async fn context_status_tracks_the_active_window_and_drops_immediately_after_compaction() {
    let directory = tempfile::tempdir().unwrap();
    let client = Arc::new(ScriptedModel::with_usages(
        vec![
            text(&"older material ".repeat(10_000)),
            text("A compact summary"),
        ],
        vec![Usage {
            input_tokens: Some(40_000),
            output_tokens: Some(50_000),
            ..Usage::default()
        }],
    ));
    let agent = Agent::new_embedded(config(), client);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.set_context_window(chat, 1_000_000).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Collect the material".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let before = agent.status(chat).await.unwrap().context_tokens.unwrap();
    assert_eq!(
        before, 90_000,
        "current context includes measured input and the completed reply"
    );
    agent.compact(chat).await.unwrap();
    let after = agent.status(chat).await.unwrap().context_tokens.unwrap();
    assert!(
        after < before,
        "compaction must immediately reduce current context: {before} -> {after}"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot.messages.iter().any(|message| message
            .content
            .iter()
            .any(|content| matches!(content, Content::Text {text} if text.len() > 30_000))),
        "raw history is retained"
    );
    let execution = tickloop_agent_protocol::ExecutionRef {
        daemon_epoch: Uuid::new_v4(),
        agent_id: Uuid::new_v4(),
        generation: 1,
    };
    let live =
        tickloop_agent_protocol::LiveChatSnapshot::from_snapshot(execution, snapshot, Vec::new());
    let restored: tickloop_agent_protocol::LiveChatSnapshot =
        serde_json::from_value(serde_json::to_value(&live).unwrap()).unwrap();
    assert_eq!(live.status().chat.context_tokens, Some(after));
    assert_eq!(restored.status().chat.context_tokens, Some(after));
    let mut reopened = live.conversation.clone();
    reopened.request_usage = Default::default();
    let estimated = tickloop_agent_protocol::ChatStatus::of(&reopened)
        .context_tokens
        .unwrap();
    assert!(
        estimated > 0 && estimated < before,
        "restored history must exclude archived context: {estimated}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn context_status_counts_tool_results_streamed_usage_and_model_changes() {
    use tickloop_agent_protocol::{
        ChatStatus, Content, ContextMeasurement, Message, ModelRequestKind, RequestOutcome,
        RequestOwner, Role, ToolResultContent,
    };
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut snapshot = agent.snapshot(chat).await.unwrap();
    snapshot.messages.push(Message {
        role: Role::User,
        content: text("Read the source"),
    });
    snapshot.message_ids.insert(0, Uuid::new_v4());
    let sequence = snapshot
        .request_usage
        .start(
            RequestOwner {
                chat_id: chat,
                loop_id: Some(Uuid::new_v4()),
                run_id: Some(Uuid::new_v4()),
            },
            ModelRequestKind::Conversation,
            snapshot.model.identity(),
        )
        .unwrap();
    let measurement = ContextMeasurement::capture(&snapshot, 10_000, 2_000);
    snapshot
        .request_usage
        .record_context_measurement(sequence, measurement)
        .unwrap();
    snapshot
        .request_usage
        .observe(
            sequence,
            Usage {
                input_tokens: Some(20_000),
                output_tokens: None,
                ..Usage::default()
            },
        )
        .unwrap();
    snapshot
        .request_usage
        .add_context_output_bytes(sequence, 4000)
        .unwrap();
    assert_eq!(ChatStatus::of(&snapshot).context_tokens, Some(22_000));
    snapshot
        .request_usage
        .observe(
            sequence,
            Usage {
                output_tokens: Some(3000),
                ..Usage::default()
            },
        )
        .unwrap();
    assert_eq!(ChatStatus::of(&snapshot).context_tokens, Some(23_000));
    snapshot
        .request_usage
        .finish(sequence, RequestOutcome::Accepted)
        .unwrap();
    snapshot.messages.extend([
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolUse {
                id: "read".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path":"source.txt"}),
            }],
        },
        Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "read".into(),
                content: ToolResultContent::Text("source material ".repeat(3000)),
                is_error: false,
            }],
        },
    ]);
    assert!(ChatStatus::of(&snapshot).context_tokens.unwrap() > 23_000);
    snapshot.model.name = "another model".into();
    let switched = ChatStatus::of(&snapshot).context_tokens;
    snapshot.request_usage = Default::default();
    assert_eq!(
        switched,
        ChatStatus::of(&snapshot).context_tokens,
        "another model's calibration must not be reused"
    );
    assert!(switched.unwrap() > 0);
    agent.shutdown().await;
}

#[tokio::test]
async fn conversation_output_budget_is_bounded_on_the_actual_http_request() {
    let directory = tempfile::tempdir().unwrap();
    let (endpoint, captured) = tickloop_agent_test_support::model_http_support::serve_response(
        400,
        serde_json::json!({"error":{"message":"fixture refusal"}}).to_string(),
    )
    .await;
    let mut model = tickloop_agent_test_support::config_with_window(200_000);
    model.base_url = endpoint;
    model.max_output_tokens = 128_000;
    model.card.as_mut().unwrap().max_output_tokens = 128_000;
    let client = Arc::new(tickloop_agent_model::HttpModelClient::new(model.clone()).unwrap());
    let agent = Agent::new_embedded(model, client);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "source ".repeat(35_000))
        .await
        .unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::Failed(_)) {
            break;
        }
    }
    let (_, body) = captured.await.unwrap();
    assert!(
        body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    let limit = body["max_tokens"].as_u64().unwrap();
    assert!((1..80_000).contains(&limit), "{limit}");
    assert_eq!(
        agent.snapshot(chat).await.unwrap().model.max_output_tokens,
        128_000
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn large_output_limits_do_not_consume_the_input_compaction_budget() {
    for (provider, model_id) in [
        ("openai-codex", "gpt-6.1-sol"),
        ("github-copilot", "gpt-6-astra"),
        ("kimi-code-oauth", "k3-256k"),
        ("anthropic-api", "claude-haiku-4-5-20251001"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(ScriptedModel::new(vec![
            text("First"),
            text("Second"),
            text("Continue"),
        ]));
        let mut model = ModelCatalog::defaults().get(provider, model_id).unwrap();
        if provider == "anthropic-api" {
            model.base_url = "http://127.0.0.1:9".into();
            model.api_key_env.clear();
        }
        let agent = Agent::new_embedded(model, client.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let receipt = agent.submit(chat, None, "Hello".into()).await.unwrap();
        wait_for_completion(&mut events).await;
        agent
            .submit(chat, Some(receipt.loop_id), "source ".repeat(35_000))
            .await
            .unwrap();
        wait_for_completion(&mut events).await;
        assert!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .latest_compact_summary()
                .is_none(),
            "{provider}/{model_id}"
        );
        assert_eq!(client.requests.lock().unwrap().len(), 2);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn kimi_k3_context_variants_use_their_own_compaction_thresholds() {
    for (model_id, should_compact) in [("k3", false), ("k3-256k", true)] {
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(ScriptedModel::new(vec![
            text("Initial answer"),
            text("Follow-up answer or compact summary"),
            text("Continue"),
        ]));
        let model = ModelCatalog::defaults()
            .get("kimi-code-oauth", model_id)
            .unwrap();
        let agent = Agent::new_embedded(model, client.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, None, "source ".repeat(6_000))
            .await
            .unwrap();
        wait_for_completion(&mut events).await;
        agent
            .submit(chat, Some(receipt.loop_id), "source ".repeat(70_000))
            .await
            .unwrap();
        wait_for_completion(&mut events).await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            snapshot.latest_compact_summary().is_some(),
            should_compact,
            "{model_id}"
        );
        assert_eq!(
            client.requests.lock().unwrap().len(),
            if should_compact { 3 } else { 2 }
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn automatic_compaction_does_not_treat_serialized_bytes_as_tokens() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::with_usages(
        vec![
            text("Initial answer"),
            text("Follow-up answer"),
            text("Compact summary"),
            text("Answer after compaction"),
        ],
        vec![
            Usage {
                input_tokens: Some(200_000),
                output_tokens: Some(10),
                ..Usage::default()
            },
            Usage::default(),
        ],
    ));
    let mut model_config = config();
    model_config.max_output_tokens = 64_000;
    let agent = Agent::new_embedded(model_config, model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "word ".repeat(150_000))
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    agent.set_context_window(chat, 1_000_000).await.unwrap();
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .request_usage
            .entries()
            .last()
            .and_then(|entry| entry.usage.input_tokens),
        Some(200_000)
    );

    agent
        .submit(chat, Some(receipt.loop_id), "Follow-up".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .latest_compact_summary()
            .is_none()
    );

    agent
        .submit(chat, Some(receipt.loop_id), "word ".repeat(400_000))
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    assert_eq!(model.requests.lock().unwrap().len(), 4);
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .latest_compact_summary()
            .is_some_and(|summary| summary.contains("Compact summary"))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn context_window_maximum_follows_the_selected_model() {
    let directory = tempfile::tempdir().unwrap();
    let mut model_config = config();
    model_config.base_url = "https://api.deepseek.com/anthropic".into();
    model_config.model = "deepseek-flash".into();
    model_config.api_key_env = "DEEPSEEK_API_KEY".into();
    model_config.max_output_tokens = 64_000;
    let agent = Agent::new_embedded(
        model_config.clone(),
        Arc::new(ScriptedModel::new(Vec::new())),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .effective_context_window_tokens(),
        Some(1_000_000)
    );
    let error = agent.set_context_window(chat, 100_000).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("determined by the selected model")
    );
    let mut snapshot = agent.snapshot(chat).await.unwrap();
    snapshot.context_window_tokens = Some(100_000);
    assert_eq!(snapshot.effective_context_window_tokens(), Some(1_000_000));

    agent
        .set_model(chat, config(), Arc::new(ScriptedModel::new(Vec::new())))
        .await
        .unwrap();
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .effective_context_window_tokens(),
        None
    );
    agent
        .set_model(chat, model_config, Arc::new(ScriptedModel::new(Vec::new())))
        .await
        .unwrap();
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .effective_context_window_tokens(),
        Some(1_000_000)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn saved_window_override_does_not_reduce_the_selected_model_window() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("settings.json"),
        r#"{"contextWindowTokens":100000}"#,
    )
    .unwrap();
    let mut model_config = config();
    model_config.base_url = "https://api.deepseek.com/anthropic".into();
    model_config.model = "deepseek-flash".into();
    model_config.api_key_env = "DEEPSEEK_API_KEY".into();
    let agent =
        Agent::open_embedded(root, model_config, Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.context_window_tokens, None);
    assert_eq!(snapshot.effective_context_window_tokens(), Some(1_000_000));
    agent.shutdown().await;
}

#[tokio::test]
async fn automatic_compaction_uses_the_configured_window_and_preserves_recent_tool_pairs() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "source value").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text(&"older material ".repeat(23_000)),
        text("Second response"),
        text("Third response"),
        text("Compact summary with instructions"),
        tickloop_agent_test_support::call(
            "read-1",
            "Read",
            serde_json::json!({"file_path":"note.txt"}),
        ),
        text("The file says source value"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Keep the source unchanged".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    for prompt in ["Second step", "Third step"] {
        agent
            .submit(chat, Some(receipt.loop_id), prompt.into())
            .await
            .unwrap();
        wait_for_completion(&mut events).await;
    }
    agent.set_context_window(chat, 200_000).await.unwrap();
    agent
        .submit(chat, Some(receipt.loop_id), "Read note.txt".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .latest_compact_summary()
            .is_some_and(|summary| summary.contains("Compact summary"))
    );
    assert!(
        snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content,Content::Text{text} if text.len()>20_000))
    );
    let requests = model.requests.lock().unwrap().clone();
    assert!(requests[3].tools.is_empty());
    assert!(!requests[4].tools.is_empty());
    assert!(requests[5].messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,Content::ToolResult{tool_use_id,..} if tool_use_id=="read-1")));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_summary_that_exceeds_capacity_does_not_replace_the_valid_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("Answer"),
        text(&"oversized summary ".repeat(10_000)),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "A small goal".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    agent.set_context_window(chat, 20_000).await.unwrap();
    let before = agent.snapshot(chat).await.unwrap();
    assert!(
        agent
            .compact(chat)
            .await
            .unwrap_err()
            .to_string()
            .contains("previous context was retained")
    );
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.message_compactions, before.message_compactions);
    assert_eq!(after.messages, before.messages);
    agent.shutdown().await;
}

#[tokio::test]
async fn compaction_pins_the_approved_plan_even_when_the_model_omits_it() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        tickloop_agent_test_support::submit_plan(
            "plan-1",
            "Preserve original.txt; write only report.txt.",
        ),
        text("Plan ready"),
        text("Brief summary that omits the plan"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, tickloop_agent_protocol::ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Prepare my report".into())
        .await
        .unwrap();
    loop {
        if let EventKind::PermissionRequested { request } = next_event(&mut events).await.kind {
            assert_eq!(request.name, "ExitPlanMode");
            break;
        }
    }
    agent
        .approve_plan(
            chat,
            receipt.loop_id,
            1,
            tickloop_agent_protocol::ExecutionMode::AcceptEdits,
        )
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let outcome = agent.compact(chat).await.unwrap();
    assert!(outcome.snapshot.conversation_messages().unwrap().iter().flat_map(|message|&message.content).any(|content|matches!(content,Content::Text{text} if text.contains("Brief summary") && text.contains("Preserve original.txt; write only report.txt.") && text.contains("acceptEdits"))));
    assert_eq!(agent.snapshot(chat).await.unwrap().plans[0].version, 1);
    agent.shutdown().await;
}

const CONTINUE_WITHOUT_QUESTIONS: &str = "Continue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, do not preface with \"I'll continue\" or similar. Pick up the last task as if the break never happened.";
const CONTINUED_SESSION: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.";

fn summary_message_text(request: &tickloop_agent_model::ModelRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } if text.starts_with(CONTINUED_SESSION) => Some(text.clone()),
            _ => None,
        })
        .next()
        .expect("the request carries the compact summary message")
}

#[tokio::test]
async fn automatic_compaction_tells_the_model_to_continue_but_manual_compaction_does_not() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text(&"older material ".repeat(23_000)),
        text("Automatic summary"),
        text("Answer after automatic compaction"),
        text("Manual summary"),
        text("Answer after manual compaction"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Collect the material".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    agent.set_context_window(chat, 200_000).await.unwrap();
    agent
        .submit(chat, Some(receipt.loop_id), "Keep going".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let requests = model.requests.lock().unwrap().clone();
    assert!(requests[1].tools.is_empty());
    let automatic = summary_message_text(&requests[2]);
    assert!(automatic.contains("Automatic summary"));
    assert!(automatic.ends_with(CONTINUE_WITHOUT_QUESTIONS));
    assert!(!automatic.contains("historical data"));

    agent.compact(chat).await.unwrap();
    agent
        .submit(chat, Some(receipt.loop_id), "Next step".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let requests = model.requests.lock().unwrap().clone();
    let manual = summary_message_text(&requests[4]);
    assert!(manual.contains("Manual summary"));
    assert!(!manual.contains(CONTINUE_WITHOUT_QUESTIONS));
    assert!(!manual.contains("historical data"));
    agent.shutdown().await;
}
