mod support;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent::{
    model::{Content, ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest, Role, Usage},
    runtime::{ASK_USER_QUESTION_TOOL_NAME, Agent, Event, EventKind, QuestionAnswer},
};
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

#[async_trait]
impl ModelClient for StreamingModel {
    fn identity(&self) -> tickloop_agent::model::ModelIdentity {
        tickloop_agent::model::ModelIdentity {
            provider: tickloop_agent::model::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent::model::Protocol::Messages,
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
        let event = support::next_event(events).await;
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
                outcome: Ok(support::text("The file says hello.")),
            },
        ])),
    });
    let agent = Agent::open_embedded(archive.clone(), support::config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Read notes.txt".into())
        .await
        .unwrap();

    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == " the read."),
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
        |kind| matches!(kind, EventKind::Text(text) if text == " says hello."),
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
        support::config(),
        Arc::new(support::ScriptedModel::new(Vec::new())),
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
                outcome: Err(ModelError::StreamInterrupted),
            },
            ControlledReply {
                control: retry.clone(),
                deltas: vec!["Recovered reply"],
                outcome: Ok(support::text("Recovered reply")),
            },
        ])),
    });
    let agent = Agent::new_embedded(support::config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Please answer".into())
        .await
        .unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == " reply"),
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

    agent
        .submit(chat, Some(first.loop_id), "Try again".into())
        .await
        .unwrap();
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

#[tokio::test]
async fn a_parked_run_does_not_commit_another_runs_failed_reply_when_it_regains_the_chat() {
    let directory = tempfile::tempdir().unwrap();
    let ask = ReplyControl::default();
    let failed = ReplyControl::default();
    let resumed = ReplyControl::default();
    ask.begin.cancel();
    ask.finish.cancel();
    failed.begin.cancel();
    let model = Arc::new(StreamingModel {
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(VecDeque::from([
            ControlledReply {
                control: ask,
                deltas: vec!["A needs clarification."],
                outcome: Ok(vec![
                    Content::Text {
                        text: "A needs clarification.".into(),
                    },
                    Content::ToolUse {
                        id: "ask-a".into(),
                        name: ASK_USER_QUESTION_TOOL_NAME.into(),
                        input: support::single_question("Which format?"),
                    },
                ]),
            },
            ControlledReply {
                control: failed.clone(),
                deltas: vec!["B unfinished text"],
                outcome: Err(ModelError::StreamInterrupted),
            },
            ControlledReply {
                control: resumed.clone(),
                deltas: vec!["A resumed", " independently."],
                outcome: Ok(support::text("A resumed independently.")),
            },
        ])),
    });
    let agent =
        Agent::open_embedded(directory.path().join("archive"), support::config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Prepare report A".into())
        .await
        .unwrap();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::QuestionAsked { .. })
    })
    .await;
    let question = agent
        .snapshot(chat)
        .await
        .unwrap()
        .questions
        .into_iter()
        .find(|question| question.run_id == first.run_id)
        .unwrap();

    let second = agent
        .submit(chat, None, "Research B while A waits".into())
        .await
        .unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == "B unfinished text"),
    )
    .await;
    failed.finish.cancel();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Failed(_))).await;
    agent
        .answer_question(chat, question.id, vec![QuestionAnswer::other("Markdown")])
        .await
        .unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::ToolCompleted { call_id, .. } if call_id == "ask-a"),
    )
    .await;
    resumed.begin.cancel();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == " independently."),
    )
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        !serde_json::to_string(&snapshot.messages)
            .unwrap()
            .contains("B unfinished text")
    );
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.run_id == second.run_id
                && matches!(&event.kind, EventKind::Text(text) if text == "B unfinished text"))
    );
    assert!(
        snapshot.events.iter().any(
            |event| event.run_id == second.run_id && matches!(event.kind, EventKind::Failed(_))
        )
    );
    assert_eq!(snapshot.events.iter().filter(|event| event.run_id == first.run_id && matches!(event.kind, EventKind::Started)).count(), 1);

    resumed.finish.cancel();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        serde_json::to_string(&snapshot.messages)
            .unwrap()
            .contains("A resumed independently.")
    );
    assert!(
        !serde_json::to_string(&snapshot.messages)
            .unwrap()
            .contains("B unfinished text")
    );
    agent.shutdown().await;
}
