use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    ActiveConversationItem, ChatSnapshot, Content, DirectedSummaryRequest, EventKind, Message,
    ModelChange, ModelIdentity, ModelRequestKind, Protocol, RequestFailure, RequestOutcome,
    ResponseRejection, RewindMode, SummaryDirection, Usage, UserShellState,
};
use tickloop_agent_runtime::{Agent, AgentError};
use tickloop_agent_test_support::harness::{ChosenModels, embedded_agent};
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

async fn run_prompt(
    agent: &Agent,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    chat: uuid::Uuid,
    prompt: &str,
) {
    let receipt = agent
        .submit(tickloop_ids::Id::from(chat), prompt.into())
        .await
        .unwrap();
    loop {
        let event = next_event(events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("prompt failed: {error}"),
            _ => {}
        }
    }
}

fn message_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

enum SummaryBehavior {
    Output(ModelOutput),
    Error(ModelError),
}

struct SummaryBehaviorModel {
    behaviors: Mutex<VecDeque<SummaryBehavior>>,
}

#[async_trait]
impl ModelClient for SummaryBehaviorModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "summary-behavior".into(),
            protocol: Protocol::Messages,
            requested_model: "summary-behavior".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if !request.tools.is_empty() {
            let _ = events.send(ModelDelta::Text("ordinary answer".into()));
            return Ok(ModelOutput {
                private_source: None,
                content: text("ordinary answer"),
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            });
        }
        match self.behaviors.lock().unwrap().pop_front().unwrap() {
            SummaryBehavior::Output(output) => Ok(output),
            SummaryBehavior::Error(error) => Err(error),
        }
    }
}

#[tokio::test]
async fn from_here_replaces_the_selected_suffix_and_restores_only_a_draft() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("second answer"),
        text("condensed selected suffix"),
    ]));
    let agent =
        Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "first prompt").await;
    run_prompt(&agent, &mut events, chat.uuid(), "second prompt").await;
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[1].clone();
    let raw_messages = before.messages.clone();
    let loops = serde_json::to_value(&before.chat_loop).unwrap();
    let prior_usage = before.request_usage.entries().to_vec();

    let outcome = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: Some("  preserve the decision  ".into()),
            },
        )
        .await
        .unwrap();

    assert_eq!(outcome.direction, SummaryDirection::FromHere);
    assert_eq!(outcome.guidance.as_deref(), Some("preserve the decision"));
    assert_eq!(
        outcome.messages_summarized,
        raw_messages.len() - checkpoint.message_index
    );
    // Following Claude Code's partial compact: boundary, messages kept before the pick, summary.
    assert_eq!(
        outcome.snapshot.messages.len(),
        checkpoint.message_index + 2
    );
    assert!(outcome.snapshot.is_compact_boundary(0));
    assert_eq!(
        &outcome.snapshot.messages[1..=checkpoint.message_index],
        &raw_messages[..checkpoint.message_index]
    );
    assert!(
        outcome
            .snapshot
            .is_compact_summary(checkpoint.message_index + 1)
    );
    assert_eq!(
        serde_json::to_value(&outcome.snapshot.events).unwrap(),
        serde_json::to_value(&before.events).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&outcome.snapshot.operations).unwrap(),
        serde_json::to_value(&before.operations).unwrap()
    );
    assert_eq!(outcome.snapshot.user_shells, before.user_shells);
    // Kept prompts are rewritten as new entries in the Transcript, and rewind points switch to the
    // new ids accordingly.
    let kept_points = outcome.snapshot.rewind_points();
    assert_eq!(
        kept_points
            .iter()
            .map(|point| point.prompt.as_str())
            .collect::<Vec<_>>(),
        ["first prompt"]
    );
    assert_ne!(
        kept_points[0].message_id,
        before.rewind_points()[0].message_id
    );
    assert_eq!(outcome.snapshot.last_sequence, before.last_sequence);
    assert_eq!(
        serde_json::to_value(&outcome.snapshot.chat_loop).unwrap(),
        loops
    );
    let draft = outcome.draft.unwrap();
    assert_eq!(draft.prompt, "second prompt");
    assert!(draft.contexts.is_empty());
    assert_eq!(
        outcome.snapshot.request_usage.entries()[..prior_usage.len()],
        prior_usage
    );
    let summary_usage = outcome.snapshot.request_usage.entries().last().unwrap();
    assert_eq!(summary_usage.kind, ModelRequestKind::CompactionDirected);
    assert_eq!(summary_usage.outcome, RequestOutcome::Accepted);
    assert!(summary_usage.loop_id.is_none());
    assert!(summary_usage.run_id.is_none());

    {
        let requests = model.requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert!(request.tools.is_empty());
        let request_text = message_text(&request.messages);
        assert!(!request_text.contains("first prompt"));
        assert!(!request_text.contains("first answer"));
        assert!(request_text.contains("second prompt"));
        assert!(request_text.contains("second answer"));
        assert!(request_text.contains("preserve the decision"));
    }

    let active_text = message_text(&outcome.snapshot.conversation_messages().unwrap());
    assert!(active_text.contains("first prompt"));
    assert!(active_text.contains("first answer"));
    assert!(active_text.contains("condensed selected suffix"));
    assert!(!active_text.contains("second prompt"));
    assert!(!active_text.contains("second answer"));
    assert_eq!(
        outcome.snapshot.active_conversation_projection().unwrap(),
        vec![
            ActiveConversationItem::RawMessage { message_index: 1 },
            ActiveConversationItem::RawMessage { message_index: 2 },
            ActiveConversationItem::Summary {
                message_index: 3,
                direction: Some(SummaryDirection::FromHere),
                summary: "condensed selected suffix".into(),
            },
        ]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn rejected_or_failed_summaries_leave_no_trace_in_the_conversation() {
    let cases = [
        (
            SummaryBehavior::Output(ModelOutput {
                private_source: None,
                content: Vec::new(),
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            }),
            RequestOutcome::HostRejected(ResponseRejection::EmptySummary),
        ),
        (
            SummaryBehavior::Output(ModelOutput {
                private_source: None,
                content: vec![Content::ToolUse {
                    id: "unexpected-tool".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command":"true"}),
                }],
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
            }),
            RequestOutcome::HostRejected(ResponseRejection::InvalidSummary),
        ),
        (
            SummaryBehavior::Error(ModelError::Http { status: 400 }),
            RequestOutcome::Failed(RequestFailure::Http { status: 400 }),
        ),
        (
            SummaryBehavior::Error(ModelError::ProviderFailure),
            RequestOutcome::Failed(RequestFailure::ProviderFailure),
        ),
    ];

    for (behavior, expected_outcome) in cases {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let model = Arc::new(SummaryBehaviorModel {
            behaviors: Mutex::new(vec![behavior].into()),
        });
        let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        run_prompt(
            &agent,
            &mut events,
            chat.uuid(),
            "prompt kept after failure",
        )
        .await;
        let before = agent.snapshot(chat).await.unwrap();
        let checkpoint = before.rewind_points()[0].clone();

        assert!(
            agent
                .summarize_checkpoint(
                    chat,
                    DirectedSummaryRequest {
                        message_id: checkpoint.message_id,
                        direction: SummaryDirection::FromHere,
                        guidance: None,
                    },
                )
                .await
                .is_err()
        );

        let after = agent.snapshot(chat).await.unwrap();
        assert_eq!(after.messages, before.messages);
        assert!(after.message_compactions.is_empty());
        assert_eq!(
            after.request_usage.entries().last().unwrap().outcome,
            expected_outcome
        );
        agent.shutdown().await;
        drop(agent);
        let restored =
            Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new())))
                .unwrap()
                .resume_chat(chat)
                .await
                .unwrap();
        assert!(restored.message_compactions.is_empty());
        assert_eq!(
            restored.conversation_messages().unwrap(),
            before.conversation_messages().unwrap()
        );
    }
}

#[tokio::test]
async fn directed_summary_retries_a_transient_service_failure() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(SummaryBehaviorModel {
        behaviors: Mutex::new(
            vec![
                SummaryBehavior::Error(ModelError::Http { status: 503 }),
                SummaryBehavior::Output(ModelOutput {
                    private_source: None,
                    content: text("recovered summary"),
                    usage: Usage::default(),
                    stop_reason: "end_turn".into(),
                }),
            ]
            .into(),
        ),
    });
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "Summarize this answer").await;
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = &before.rewind_points()[0];
    let outcome = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    let new_requests =
        &outcome.snapshot.request_usage.entries()[before.request_usage.entries().len()..];
    assert_eq!(new_requests.len(), 2);
    assert_eq!(
        new_requests[0].outcome,
        RequestOutcome::Failed(RequestFailure::Http { status: 503 })
    );
    assert_eq!(new_requests[1].outcome, RequestOutcome::Accepted);
    assert!(
        message_text(&outcome.snapshot.conversation_messages().unwrap())
            .contains("recovered summary")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_summary_that_exceeds_the_verified_context_keeps_the_previous_projection() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(SummaryBehaviorModel {
        behaviors: Mutex::new(
            vec![SummaryBehavior::Output(ModelOutput {
                private_source: None,
                content: text(&"oversized summary ".repeat(20_000)),
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            })]
            .into(),
        ),
    });
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "small prompt").await;
    agent.set_context_window(chat, 10_000).await.unwrap();
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[0].clone();

    let result = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(AgentError::Invalid(message)) if message.contains("configured model capacity")
    ));
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.message_compactions, before.message_compactions);
    assert_eq!(
        after.request_usage.entries().last().unwrap().outcome,
        RequestOutcome::HostRejected(ResponseRejection::SummaryTooLarge)
    );
    agent.shutdown().await;
}

fn summary_texts(snapshot: &ChatSnapshot) -> Vec<String> {
    snapshot
        .active_conversation_projection()
        .unwrap()
        .into_iter()
        .filter_map(|item| match item {
            ActiveConversationItem::Summary { summary, .. } => Some(summary),
            ActiveConversationItem::RawMessage { .. } => None,
        })
        .collect()
}

async fn summarize_last_prompt(
    agent: &Agent,
    chat: uuid::Uuid,
    direction: SummaryDirection,
) -> tickloop_agent_protocol::DirectedSummaryOutcome {
    let checkpoint = agent
        .snapshot(tickloop_ids::Id::from(chat))
        .await
        .unwrap()
        .rewind_points()
        .last()
        .unwrap()
        .clone();
    agent
        .summarize_checkpoint(
            tickloop_ids::Id::from(chat),
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction,
                guidance: None,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn repeated_summaries_follow_claude_code_partial_compaction_layouts() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("answer A"),
        text("answer B"),
        text("summary B"),
        text("answer C"),
        text("summary C"),
        text("answer D"),
        text("summary through C"),
        text("answer E"),
        text("summary E"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "prompt A").await;
    run_prompt(&agent, &mut events, chat.uuid(), "prompt B").await;
    summarize_last_prompt(&agent, chat.uuid(), SummaryDirection::FromHere).await;

    // When keeping forward, the earlier boundary and summary stay unchanged after the new boundary
    // as kept messages.
    run_prompt(&agent, &mut events, chat.uuid(), "prompt C").await;
    let summary_c = summarize_last_prompt(&agent, chat.uuid(), SummaryDirection::FromHere).await;
    assert_eq!(
        summary_texts(&summary_c.snapshot),
        ["summary B", "summary C"]
    );
    assert!(summary_c.snapshot.is_compact_boundary(0));
    assert!(summary_c.snapshot.is_compact_boundary(1));

    // When keeping backward, the summarized range starts at the last boundary, and the kept
    // messages no longer contain the old boundary and summary.
    run_prompt(&agent, &mut events, chat.uuid(), "prompt D").await;
    let summary_d = summarize_last_prompt(&agent, chat.uuid(), SummaryDirection::UpToHere).await;
    assert_eq!(summary_texts(&summary_d.snapshot), ["summary through C"]);
    assert_eq!(summary_d.snapshot.messages.len(), 4);
    let d_text = message_text(&summary_d.snapshot.conversation_messages().unwrap());
    assert!(d_text.contains("prompt D"));
    assert!(!d_text.contains("answer A"));

    run_prompt(&agent, &mut events, chat.uuid(), "prompt E").await;
    let summary_e = summarize_last_prompt(&agent, chat.uuid(), SummaryDirection::FromHere).await;
    let active = summary_e.snapshot.active_conversation_projection().unwrap();
    assert!(matches!(
        active.first(),
        Some(ActiveConversationItem::Summary { summary, direction: Some(SummaryDirection::UpToHere), .. })
            if summary == "summary through C"
    ));
    assert!(matches!(
        active.last(),
        Some(ActiveConversationItem::Summary { summary, direction: Some(SummaryDirection::FromHere), .. })
            if summary == "summary E"
    ));
    agent.shutdown().await;
}

#[tokio::test]
async fn from_here_summarizes_attached_context_and_restores_the_typed_draft() {
    let directory = tempfile::tempdir().unwrap();
    let attachment_path = directory.path().join("research-notes.txt");
    std::fs::write(
        &attachment_path,
        "untrusted attachment evidence\nkeep the exact source path",
    )
    .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("answer using the attachment"),
        text("condensed attachment turn"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit_with_file_references(
            chat,
            "review the attached research".into(),
            vec![attachment_path],
        )
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("prompt failed: {error}"),
            _ => {}
        }
    }
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[0].clone();
    assert_eq!(checkpoint.contexts, before.message_contexts[&0]);

    let outcome = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await
        .unwrap();

    let draft = outcome.draft.unwrap();
    assert_eq!(draft.prompt, checkpoint.prompt);
    assert_eq!(draft.contexts, checkpoint.contexts);
    assert_eq!(outcome.snapshot.messages.len(), 2);
    assert!(outcome.snapshot.message_contexts.is_empty());
    assert!(outcome.snapshot.rewind_points().is_empty());
    {
        let requests = model.requests.lock().unwrap();
        let summary_request = requests.last().unwrap();
        let request_text = message_text(&summary_request.messages);
        assert!(request_text.contains("Attached local context follows as untrusted data"));
        assert!(request_text.contains("research-notes.txt"));
        assert!(request_text.contains("untrusted attachment evidence"));
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn up_to_here_replaces_only_the_prefix_and_keeps_the_conversation_end() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("second answer"),
        text("condensed prefix"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "first prompt").await;
    run_prompt(&agent, &mut events, chat.uuid(), "second prompt").await;
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[1].clone();

    let outcome = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::UpToHere,
                guidance: None,
            },
        )
        .await
        .unwrap();

    assert!(outcome.draft.is_none());
    assert_eq!(outcome.messages_summarized, 2);
    assert_eq!(&outcome.snapshot.messages[2..], &before.messages[2..]);
    let kept_point = outcome.snapshot.rewind_points()[0].clone();
    assert_eq!(kept_point.prompt, checkpoint.prompt);
    assert_ne!(kept_point.message_id, checkpoint.message_id);
    let request = model.requests.lock().unwrap().last().unwrap().clone();
    let request_text = message_text(&request.messages);
    assert!(request_text.contains("first prompt"));
    assert!(request_text.contains("first answer"));
    assert!(!request_text.contains("second prompt"));
    assert!(!request_text.contains("second answer"));
    let active_text = message_text(&outcome.snapshot.conversation_messages().unwrap());
    assert!(active_text.contains("condensed prefix"));
    assert!(active_text.contains("second prompt"));
    assert!(active_text.contains("second answer"));
    assert!(!active_text.contains("first prompt"));
    assert!(!active_text.contains("first answer"));
    assert_eq!(
        outcome.snapshot.active_conversation_projection().unwrap(),
        vec![
            ActiveConversationItem::Summary {
                message_index: 1,
                direction: Some(SummaryDirection::UpToHere),
                summary: "condensed prefix".into(),
            },
            ActiveConversationItem::RawMessage { message_index: 2 },
            ActiveConversationItem::RawMessage { message_index: 3 },
        ]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_empty_or_already_compacted_range_does_not_start_a_model_request() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("answer"),
        text("manual compact summary"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "first prompt").await;
    let checkpoint = agent.snapshot(chat).await.unwrap().rewind_points()[0].clone();
    let usage_before = agent
        .snapshot(chat)
        .await
        .unwrap()
        .request_usage
        .entries()
        .len();
    let empty = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::UpToHere,
                guidance: None,
            },
        )
        .await;
    assert!(
        matches!(empty, Err(AgentError::Invalid(message)) if message.contains("no visible conversation"))
    );
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .request_usage
            .entries()
            .len(),
        usage_before
    );

    agent.compact(chat).await.unwrap();
    let compacted = agent.snapshot(chat).await.unwrap();
    let usage_after_compact = compacted.request_usage.entries().len();
    let old = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await;
    assert!(
        matches!(old, Err(AgentError::Invalid(message)) if message.contains("no longer in the active context"))
    );
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .request_usage
            .entries()
            .len(),
        usage_after_compact
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_prompt_abandoned_by_conversation_rewind_cannot_summarize_the_new_branch() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("abandoned second answer"),
        text("replacement branch answer"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "first prompt").await;
    run_prompt(&agent, &mut events, chat.uuid(), "second prompt").await;
    let before_rewind = agent.snapshot(chat).await.unwrap();
    let abandoned_checkpoint = before_rewind.rewind_points()[1].clone();
    agent.stop(chat).await.unwrap();
    let rewind = agent
        .rewind(
            chat,
            abandoned_checkpoint.message_id,
            RewindMode::Conversation,
        )
        .await
        .unwrap();
    assert!(rewind.draft.is_some());
    run_prompt(
        &agent,
        &mut events,
        chat.uuid(),
        "replacement branch prompt",
    )
    .await;
    let branched = agent.snapshot(chat).await.unwrap();
    let usage_before = branched.request_usage.entries().to_vec();
    let compactions_before = branched.message_compactions.clone();
    let requests_before = model.requests.lock().unwrap().len();

    for direction in [SummaryDirection::FromHere, SummaryDirection::UpToHere] {
        let result = agent
            .summarize_checkpoint(
                chat,
                DirectedSummaryRequest {
                    message_id: abandoned_checkpoint.message_id,
                    direction,
                    guidance: None,
                },
            )
            .await;
        assert!(
            matches!(result, Err(AgentError::Invalid(message)) if message.contains("current visible conversation"))
        );
    }

    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.request_usage.entries(), usage_before);
    assert_eq!(after.message_compactions, compactions_before);
    assert_eq!(model.requests.lock().unwrap().len(), requests_before);
    let active_text = message_text(&after.conversation_messages().unwrap());
    assert!(active_text.contains("replacement branch prompt"));
    assert!(!active_text.contains("second prompt"));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_background_shell_notification_is_summarized_with_the_suffix_it_finished_in() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("second answer"),
        text("noted the shell"),
        text("suffix summary"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "first prompt").await;

    let release = directory.path().join("release-background-shell");
    let shell = agent
        .submit_user_shell(
            chat,
            format!(
                "while [ ! -e '{}' ]; do sleep 0.01; done; printf shell-finished",
                release.display()
            ),
            true,
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let state = agent
                .snapshot(chat)
                .await
                .unwrap()
                .user_shells
                .iter()
                .find(|record| record.id == shell.command_id)
                .unwrap()
                .state
                .clone();
            if state == UserShellState::Background {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    run_prompt(&agent, &mut events, chat.uuid(), "second prompt").await;
    std::fs::write(&release, b"release").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            // The notification continues the Chat's Loop, which answers it.
            if message_text(&snapshot.messages).contains("noted the shell")
                && snapshot.chat_loop.state == tickloop_agent_protocol::LoopState::Idle
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[1].clone();

    let outcome = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.direction, SummaryDirection::FromHere);
    let summary_source = message_text(&model.requests.lock().unwrap()[3].messages);
    assert!(summary_source.contains("second prompt"));
    assert!(summary_source.contains(&format!(
            "<task-id>{}</task-id>",
            tickloop_ids::IdContext::new()
                .model_text(tickloop_agent_protocol::ShellTaskId::from(shell.command_id))
        )));
    assert!(!summary_source.contains("<bash-input>"));
    let active = message_text(&outcome.snapshot.conversation_messages().unwrap());
    assert!(active.contains("<bash-input>"));
    assert!(active.contains("suffix summary"));
    assert!(!active.contains("<task-notification>"));
    agent.shutdown().await;
}

#[tokio::test]
async fn summarized_private_model_blocks_do_not_enter_the_request_or_new_context() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        vec![
            Content::Thinking {
                thinking: "private reasoning".into(),
                signature: Some("private signature".into()),
            },
            Content::ProtocolData {
                protocol: Protocol::Messages,
                value: serde_json::json!({"secret":"protocol continuation"}),
            },
            Content::Text {
                text: "public answer".into(),
            },
        ],
        text("public condensed history"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "public prompt").await;
    let checkpoint = agent.snapshot(chat).await.unwrap().rewind_points()[0].clone();

    let outcome = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: checkpoint.message_id,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await
        .unwrap();

    let request = model.requests.lock().unwrap().last().unwrap().clone();
    assert!(
        request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .all(|content| !matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            ))
    );
    assert!(
        outcome
            .snapshot
            .conversation_messages()
            .unwrap()
            .iter()
            .flat_map(|message| &message.content)
            .all(|content| !matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            ))
    );
    assert!(!message_text(&outcome.snapshot.messages).contains("public answer"));
    agent.shutdown().await;
}

struct PausedSuccessfulSummaryModel {
    requests: Mutex<Vec<ModelRequest>>,
    ordinary_replies: Mutex<VecDeque<Vec<Content>>>,
    summary_started: Notify,
    release_summary: Notify,
}

#[async_trait]
impl ModelClient for PausedSuccessfulSummaryModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let is_summary = request.tools.is_empty();
        self.requests.lock().unwrap().push(request);
        let content = if is_summary {
            self.summary_started.notify_one();
            tokio::select! {
                _ = self.release_summary.notified() => text("public directed summary"),
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
        } else {
            self.ordinary_replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("ordinary reply is available")
        };
        for block in &content {
            if let Content::Text { text } = block {
                let _ = events.send(ModelDelta::Text(text.clone()));
            }
        }
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

struct UnusedReplacementModel {
    identity: ModelIdentity,
}

#[async_trait]
impl ModelClient for UnusedReplacementModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.identity.clone()
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Err(ModelError::InvalidResponse(
            "replacement model should not be called by summary completion".into(),
        ))
    }
}

fn private_answer(text: &str) -> Vec<Content> {
    vec![
        Content::Thinking {
            thinking: format!("private reasoning for {text}"),
            signature: Some(format!("private signature for {text}")),
        },
        Content::ProtocolData {
            protocol: Protocol::Messages,
            value: serde_json::json!({"private_continuation": text}),
        },
        Content::Text { text: text.into() },
    ]
}

async fn assert_pending_model_after_summary(
    direction: SummaryDirection,
    first_reply: Vec<Content>,
    second_reply: Vec<Content>,
    private_history_is_summarized: bool,
) {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(PausedSuccessfulSummaryModel {
        requests: Mutex::new(Vec::new()),
        ordinary_replies: Mutex::new(VecDeque::from([first_reply, second_reply])),
        summary_started: Notify::new(),
        release_summary: Notify::new(),
    });
    let models = ChosenModels::new(model.clone());
    let agent = embedded_agent(config(), models.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "first prompt").await;
    run_prompt(&agent, &mut events, chat.uuid(), "second prompt").await;
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[1].clone();
    let summary = {
        let agent = agent.clone();
        tokio::spawn(async move {
            agent
                .summarize_checkpoint(
                    chat,
                    DirectedSummaryRequest {
                        message_id: checkpoint.message_id,
                        direction,
                        guidance: None,
                    },
                )
                .await
        })
    };
    model.summary_started.notified().await;

    let mut replacement = config();
    replacement.name = "replacement".into();
    replacement.protocol = Protocol::Responses;
    replacement.model = "replacement".into();
    let replacement = tickloop_agent_model::cards::model_for_persistent_selection(replacement);
    models.choose(Arc::new(UnusedReplacementModel {
        identity: ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: replacement.name.clone(),
            protocol: replacement.protocol,
            requested_model: replacement.model.clone(),
        },
    }));
    assert_eq!(
        agent.set_model(chat, replacement.clone()).await.unwrap(),
        ModelChange::Pending
    );
    let gated = agent.snapshot(chat).await.unwrap();
    assert_eq!(gated.model, before.model);
    assert_eq!(gated.pending_model, Some(replacement.clone()));

    model.release_summary.notify_one();
    let outcome = summary.await.unwrap().unwrap();
    let request = model.requests.lock().unwrap().last().unwrap().clone();
    assert!(
        request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .all(|content| !matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            ))
    );
    let active_messages = outcome.snapshot.conversation_messages().unwrap();
    let active_has_private = active_messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| {
            matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            )
        });
    assert_eq!(active_has_private, !private_history_is_summarized);
    assert_eq!(outcome.snapshot.model, replacement);
    assert!(outcome.snapshot.pending_model.is_none());
    assert!(outcome.continuation_error.is_none());
    agent.shutdown().await;
}

#[tokio::test]
async fn pending_adapter_applies_when_the_new_context_excludes_private_history() {
    assert_pending_model_after_summary(
        SummaryDirection::FromHere,
        text("first public answer"),
        private_answer("second public answer"),
        true,
    )
    .await;
    assert_pending_model_after_summary(
        SummaryDirection::UpToHere,
        private_answer("first public answer"),
        text("second public answer"),
        true,
    )
    .await;
}

#[tokio::test]
async fn pending_adapter_applies_and_preserves_original_private_history() {
    assert_pending_model_after_summary(
        SummaryDirection::UpToHere,
        text("first public answer"),
        private_answer("second public answer"),
        false,
    )
    .await;
    // Following Claude Code, messages kept forward stay unchanged, and their private reasoning
    // remains in the model-visible conversation.
    assert_pending_model_after_summary(
        SummaryDirection::FromHere,
        private_answer("first public answer"),
        text("second public answer"),
        false,
    )
    .await;
}

#[tokio::test]
async fn a_context_window_set_while_summary_runs_applies_to_the_summarized_context() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(PausedSuccessfulSummaryModel {
        requests: Mutex::new(Vec::new()),
        ordinary_replies: Mutex::new(VecDeque::from([text("ordinary answer")])),
        summary_started: Notify::new(),
        release_summary: Notify::new(),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(
        &agent,
        &mut events,
        chat.uuid(),
        "prompt before context change",
    )
    .await;
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[0].clone();
    let summary = {
        let agent = agent.clone();
        tokio::spawn(async move {
            agent
                .summarize_checkpoint(
                    chat,
                    DirectedSummaryRequest {
                        message_id: checkpoint.message_id,
                        direction: SummaryDirection::FromHere,
                        guidance: None,
                    },
                )
                .await
        })
    };
    model.summary_started.notified().await;
    agent.set_context_window(chat, 3_100).await.unwrap();

    model.release_summary.notify_one();
    let result = summary.await.unwrap();
    assert!(matches!(
        result,
        Err(AgentError::Invalid(message)) if message.contains("configured model capacity")
    ));
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.context_window_tokens, Some(3_100));
    assert!(after.message_compactions.is_empty());
    assert_eq!(
        after.request_usage.entries().last().unwrap().outcome,
        RequestOutcome::HostRejected(ResponseRejection::SummaryTooLarge)
    );
    agent.shutdown().await;
}

struct BlockingSummaryModel {
    requests: AtomicUsize,
    summary_started: Notify,
}

#[async_trait]
impl ModelClient for BlockingSummaryModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "blocking".into(),
            protocol: Protocol::Messages,
            requested_model: "blocking".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        if request.tools.is_empty() {
            self.summary_started.notify_one();
            cancel.cancelled().await;
            return Err(ModelError::Cancelled);
        }
        let _ = events.send(ModelDelta::Text("ordinary answer".into()));
        Ok(ModelOutput {
            private_source: None,
            content: text("ordinary answer"),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn interrupt_cancels_the_summary_and_keeps_the_previous_projection() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(BlockingSummaryModel {
        requests: AtomicUsize::new(0),
        summary_started: Notify::new(),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "keep this prompt").await;
    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[0].clone();
    let summary = {
        let agent = agent.clone();
        tokio::spawn(async move {
            agent
                .summarize_checkpoint(
                    chat,
                    DirectedSummaryRequest {
                        message_id: checkpoint.message_id,
                        direction: SummaryDirection::FromHere,
                        guidance: None,
                    },
                )
                .await
        })
    };
    model.summary_started.notified().await;
    agent.interrupt(chat).await.unwrap();
    let result = summary.await.unwrap();
    assert!(matches!(
        result,
        Err(AgentError::Model(ModelError::Cancelled))
    ));
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.messages, before.messages);
    assert!(after.message_compactions.is_empty());
    assert_eq!(
        after.request_usage.entries().last().unwrap().outcome,
        RequestOutcome::Cancelled
    );
    assert_eq!(model.requests.load(Ordering::SeqCst), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn summary_keeps_a_visible_tool_pair_and_excludes_a_rewound_pair() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("retained.txt"),
        "retained tool evidence",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("abandoned.txt"),
        "abandoned tool evidence",
    )
    .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "retained-read",
            "Read",
            serde_json::json!({"file_path":"retained.txt"}),
        ),
        text("retained answer"),
        call(
            "abandoned-read",
            "Read",
            serde_json::json!({"file_path":"abandoned.txt"}),
        ),
        text("abandoned answer"),
        text("replacement answer"),
        text("summary of the retained tool pair"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "read retained.txt").await;
    run_prompt(&agent, &mut events, chat.uuid(), "read abandoned.txt").await;

    let before_rewind = agent.snapshot(chat).await.unwrap();
    let abandoned_checkpoint = before_rewind.rewind_points()[1].clone();
    agent.stop(chat).await.unwrap();
    let rewind = agent
        .rewind(
            chat,
            abandoned_checkpoint.message_id,
            RewindMode::Conversation,
        )
        .await
        .unwrap();
    assert!(rewind.draft.is_some());
    run_prompt(
        &agent,
        &mut events,
        chat.uuid(),
        "replacement branch prompt",
    )
    .await;
    let before_summary = agent.snapshot(chat).await.unwrap();
    let replacement_checkpoint = before_summary.rewind_points().last().unwrap().clone();

    let outcome = agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: replacement_checkpoint.message_id,
                direction: SummaryDirection::UpToHere,
                guidance: None,
            },
        )
        .await
        .unwrap();

    let summary_request = model.requests.lock().unwrap().last().unwrap().clone();
    let summary_content = summary_request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .collect::<Vec<_>>();
    assert!(summary_request.tools.is_empty());
    assert!(
        summary_content
            .iter()
            .any(|content| matches!(content, Content::ToolUse { id, .. } if id == "retained-read"))
    );
    assert!(summary_content.iter().any(|content| {
        matches!(content, Content::ToolResult { tool_use_id, content, .. }
            if tool_use_id == "retained-read" && content.text().contains("retained tool evidence"))
    }));
    assert!(!summary_content.iter().any(|content| {
        matches!(content,
            Content::ToolUse { id, .. } if id == "abandoned-read"
        ) || matches!(content,
            Content::ToolResult { tool_use_id, .. } if tool_use_id == "abandoned-read"
        )
    }));
    assert!(!message_text(&summary_request.messages).contains("abandoned tool evidence"));
    assert_eq!(
        &outcome.snapshot.messages[2..],
        &before_summary.messages[replacement_checkpoint.message_index..]
    );
    assert!(!outcome.snapshot.messages.iter().any(|message| {
        message.content.iter().any(|content| {
            matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == "abandoned-read")
        })
    }));
    agent.shutdown().await;
}

#[tokio::test]
async fn summary_resumes_the_input_queued_while_it_ran() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(PausedSuccessfulSummaryModel {
        requests: Mutex::new(Vec::new()),
        ordinary_replies: Mutex::new(VecDeque::from([
            text("first answer"),
            text("second answer"),
            text("continuation answer"),
        ])),
        summary_started: Notify::new(),
        release_summary: Notify::new(),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat.uuid(), "first prompt").await;
    run_prompt(&agent, &mut events, chat.uuid(), "second prompt").await;
    let before = agent.snapshot(chat).await.unwrap();
    let loop_id = before.chat_loop.id;
    let checkpoint_id = before.rewind_points()[1].message_id;

    let summary = {
        let agent = agent.clone();
        tokio::spawn(async move {
            agent
                .summarize_checkpoint(
                    chat,
                    DirectedSummaryRequest {
                        message_id: checkpoint_id,
                        direction: SummaryDirection::FromHere,
                        guidance: None,
                    },
                )
                .await
        })
    };
    model.summary_started.notified().await;
    let continuation = agent
        .submit(chat, "continue the work".into())
        .await
        .unwrap();
    assert_eq!(continuation.loop_id, loop_id);
    assert_eq!(model.requests.lock().unwrap().len(), 3);

    model.release_summary.notify_one();
    let outcome = summary.await.unwrap().unwrap();
    let summary_usage = outcome.snapshot.request_usage.entries().last().unwrap();
    assert_eq!(summary_usage.kind, ModelRequestKind::CompactionDirected);
    assert!(summary_usage.loop_id.is_none());
    assert!(summary_usage.run_id.is_none());

    loop {
        let event = next_event(&mut events).await;
        if matches!(event.kind, EventKind::Completed { .. }) && event.run_id == continuation.run_id
        {
            break;
        }
    }
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.chat_loop.id, loop_id);
    let usage = after
        .request_usage
        .entries()
        .iter()
        .find(|usage| usage.run_id == Some(continuation.run_id))
        .unwrap();
    assert_eq!(usage.loop_id, Some(loop_id));
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(message_text(&requests[3].messages).contains("continue the work"));
    }
    agent.shutdown().await;
}
