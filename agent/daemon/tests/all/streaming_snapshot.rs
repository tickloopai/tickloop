use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tickloop_agent_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{AgentAccess, Content, Event, EventKind, Role, Usage};
use tickloop_agent_runtime::Agent;
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct ReplyControl {
    begin: CancellationToken,
    finish: CancellationToken,
}

struct ControlledReply {
    control: ReplyControl,
    deltas: Vec<&'static str>,
    outcome: Result<Vec<Content>, ModelError>,
}

struct StreamingModel {
    replies: Mutex<VecDeque<ControlledReply>>,
    requests: Mutex<Vec<ModelRequest>>,
}

struct FloodedReply {
    thinking: bool,
    paced: bool,
    fragment: String,
    deltas: usize,
}

const FLOOD_DELTAS: usize = 8192;

#[async_trait]
impl ModelClient for FloodedReply {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "stream-flood-fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "stream-flood-fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        for _ in 0..self.deltas {
            events
                .send(if self.thinking {
                    ModelDelta::Thinking(self.fragment.clone())
                } else {
                    ModelDelta::Text(self.fragment.clone())
                })
                .unwrap();
            if self.paced {
                tokio::task::yield_now().await;
            }
        }
        let mut content = Vec::new();
        if self.thinking {
            content.push(Content::Thinking {
                thinking: self.fragment.repeat(self.deltas),
                signature: None,
            });
            events.send(ModelDelta::Text("完整答案".into())).unwrap();
            content.extend(tickloop_agent_test_support::text("完整答案"));
        } else {
            content.extend(tickloop_agent_test_support::text(
                &self.fragment.repeat(self.deltas),
            ));
        }
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

async fn stream_flood_retains_reply_without_overrunning_live_events(thinking: bool, paced: bool) {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(FloodedReply {
            thinking,
            paced,
            fragment: if thinking { "思" } else { "答" }.into(),
            deltas: FLOOD_DELTAS,
        }),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "这个项目是做什么的".into())
        .await
        .unwrap();
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.messages.len() > 1
                && snapshot.chat_loop.state == tickloop_agent_protocol::LoopState::Idle
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut streamed_text = String::new();
    let mut streamed_thinking = String::new();
    let mut completed = false;
    loop {
        match events.try_recv() {
            Ok(event) => match event.kind {
                EventKind::Thinking(value) => streamed_thinking.push_str(&value),
                EventKind::Text(value) => streamed_text.push_str(&value),
                EventKind::Completed { .. } => completed = true,
                _ => {}
            },
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(error) => {
                panic!("normal first-turn streaming overran the live event receiver: {error:?}")
            }
        }
    }
    assert!(completed);
    assert_eq!(
        streamed_text,
        if thinking {
            "完整答案".into()
        } else {
            "答".repeat(FLOOD_DELTAS)
        }
    );
    assert_eq!(
        streamed_thinking,
        if thinking {
            "思".repeat(FLOOD_DELTAS)
        } else {
            String::new()
        }
    );
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| message.role == Role::Assistant)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn first_turn_thinking_flood_preserves_output_without_live_event_loss() {
    stream_flood_retains_reply_without_overrunning_live_events(true, false).await;
}

#[tokio::test]
async fn first_turn_text_flood_preserves_output_without_live_event_loss() {
    stream_flood_retains_reply_without_overrunning_live_events(false, false).await;
}

#[tokio::test]
async fn first_turn_paced_thinking_flood_preserves_output_without_live_event_loss() {
    stream_flood_retains_reply_without_overrunning_live_events(true, true).await;
}

#[tokio::test]
async fn batched_and_large_unicode_packets_reach_agent_access_without_resync() {
    use tickloop_agent_daemon::EmbeddedAgentAccess;
    use tickloop_agent_protocol::{
        AgentCommand, AgentDetailQuery, CommandEnvelope, CommandResult, LaunchSpec,
        ObservationChange, ObservationRequest, ObservationValue,
    };
    for thinking in [false, true] {
        for (fragment, deltas) in [
            ("a".repeat(64), 256),
            ("\u{1}".repeat(4096), 1),
            ("🙂".repeat(4096), 1),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let expected = fragment.repeat(deltas);
            let agent = Agent::new_embedded(
                tickloop_agent_test_support::config(),
                Arc::new(FloodedReply {
                    thinking,
                    paced: false,
                    fragment,
                    deltas,
                }),
            );
            let access = EmbeddedAgentAccess::new(agent.clone());
            let mut launch = LaunchSpec::embedded(directory.path().into());
            launch.model = Some(tickloop_agent_test_support::config());
            let created = access
                .execute(CommandEnvelope {
                    daemon_epoch: access.daemon_epoch(),
                    command_id: uuid::Uuid::new_v4(),
                    command: AgentCommand::Create {
                        launch,
                        initial: None,
                        origin: None,
                    },
                })
                .await
                .unwrap();
            let Some(CommandResult::Created { execution, .. }) = created.result else {
                panic!("expected created execution: {created:?}");
            };
            let initial = access
                .observe(ObservationRequest::Agent {
                    execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap();
            let ObservationValue::Agent(snapshot) = initial.initial else {
                panic!("expected initial snapshot");
            };
            let chat = snapshot.conversation.id;
            let mut observation = access
                .observe(ObservationRequest::Agent {
                    execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap();
            agent
                .submit(chat, "Stream the controlled packets".into())
                .await
                .unwrap();
            let mut text = String::new();
            let mut analysis = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    match observation.recv().await.unwrap().change {
                        ObservationChange::Event(event) => match event.kind {
                            EventKind::Text(value) => text.push_str(&value),
                            EventKind::Thinking(value) => analysis.push_str(&value),
                            EventKind::Completed { .. } => break,
                            _ => {}
                        },
                        ObservationChange::Resync { reason }
                        | ObservationChange::Reset { reason, .. } => {
                            panic!("valid stream packets exceeded the delivery budget: {reason:?}")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            if thinking {
                assert_eq!(analysis, expected);
                assert_eq!(text, "完整答案");
            } else {
                assert_eq!(text, expected);
                assert!(analysis.is_empty());
            }
            agent.shutdown().await;
        }
    }
}

#[async_trait]
impl ModelClient for StreamingModel {
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
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request");
        tokio::select! {
            _ = reply.control.begin.cancelled() => {},
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
        }
        for delta in reply.deltas {
            events.send(ModelDelta::Text(delta.into())).unwrap();
        }
        tokio::select! {
            _ = reply.control.finish.cancelled() => {},
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
        }
        Ok(ModelOutput {
            private_source: None,
            content: reply.outcome?,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

async fn wait_for(events: &mut broadcast::Receiver<Event>, predicate: impl Fn(&EventKind) -> bool) {
    loop {
        let event = tickloop_agent_test_support::next_event(events).await;
        if predicate(&event.kind) {
            return;
        }
        if let EventKind::Failed(error) = event.kind {
            panic!("unexpected run failure: {error}");
        }
    }
}

#[tokio::test]
async fn snapshots_move_each_streamed_reply_into_history_once_across_a_real_tool_step() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    std::fs::write(directory.path().join("notes.txt"), "hello-from-real-file").unwrap();
    let first = ReplyControl::default();
    let second = ReplyControl::default();
    first.begin.cancel();
    let model = Arc::new(StreamingModel {
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(VecDeque::from([
            ControlledReply {
                control: first.clone(),
                deltas: vec!["Planning", " the read."],
                outcome: Ok(vec![
                    Content::Text {
                        text: "Planning the read.".into(),
                    },
                    Content::ToolUse {
                        id: "read-notes".into(),
                        name: "Read".into(),
                        input: json!({"file_path":"notes.txt"}),
                    },
                ]),
            },
            ControlledReply {
                control: second.clone(),
                deltas: vec!["The file", " says hello."],
                outcome: Ok(tickloop_agent_test_support::text("The file says hello.")),
            },
        ])),
    });
    let agent = Agent::open_embedded(
        archive.clone(),
        tickloop_agent_test_support::config(),
        model.clone(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Read notes.txt".into()).await.unwrap();

    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text.ends_with(" the read.")),
    )
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );

    first.finish.cancel();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::ToolCompleted { call_id, .. } if call_id == "read-notes"),
    )
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot
            .messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count(),
        1
    );
    assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult { content, is_error: false, .. } if content.text().contains("hello-from-real-file"))));

    second.begin.cancel();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text.ends_with(" says hello.")),
    )
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(!snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text } if text == "The file says hello.")));

    second.finish.cancel();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let assistant_text = snapshot
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_text,
        ["Planning the read.", "The file says hello."]
    );
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    agent.shutdown().await;
    drop(agent);

    let restored = Agent::open_embedded(
        archive,
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    let restored_snapshot = restored.snapshot(chat).await.unwrap();
    assert_eq!(restored_snapshot.messages, snapshot.messages);
    restored.shutdown().await;
}

#[tokio::test]
async fn failed_partial_text_is_not_committed_before_an_explicit_new_run_starts() {
    let directory = tempfile::tempdir().unwrap();
    let failed = ReplyControl::default();
    let retry = ReplyControl::default();
    failed.begin.cancel();
    let model = Arc::new(StreamingModel {
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(VecDeque::from([
            ControlledReply {
                control: failed.clone(),
                deltas: vec!["Incomplete", " reply"],
                outcome: Err(ModelError::ProviderFailure),
            },
            ControlledReply {
                control: retry.clone(),
                deltas: vec!["Recovered reply"],
                outcome: Ok(tickloop_agent_test_support::text("Recovered reply")),
            },
        ])),
    });
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Please answer".into()).await.unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text.ends_with(" reply")),
    )
    .await;
    failed.finish.cancel();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Failed(_))).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );

    agent.submit(chat, "Try again".into()).await.unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Started)).await;
    retry.begin.cancel();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == "Recovered reply"),
    )
    .await;
    retry.finish.cancel();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        !serde_json::to_string(&snapshot.messages)
            .unwrap()
            .contains("Incomplete reply")
    );
    assert_eq!(
        snapshot
            .messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count(),
        1
    );
    agent.shutdown().await;
}
