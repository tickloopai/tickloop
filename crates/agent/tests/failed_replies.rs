mod support;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent::{
    model::{Content, ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest, Role, Usage},
    runtime::{Agent, Event, EventKind, LoopState},
};
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

struct Reply {
    deltas: Vec<ModelDelta>,
    finish: CancellationToken,
    outcome: Result<Vec<Content>, ModelError>,
}

struct StreamingModel {
    replies: Mutex<VecDeque<Reply>>,
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
        for delta in reply.deltas {
            events.send(delta).unwrap();
        }
        tokio::select! {
            _ = reply.finish.cancelled() => {},
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
        }
        Ok(ModelOutput {
            content: reply.outcome?,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

async fn wait_for(
    events: &mut broadcast::Receiver<Event>,
    predicate: impl Fn(&EventKind) -> bool,
) -> Event {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(error) => panic!("event channel closed: {error}"),
            };
            if predicate(&event.kind) {
                return event;
            }
            if let EventKind::Failed(error) = event.kind {
                panic!("unexpected run failure: {error}");
            }
        }
    })
    .await
    .expect("run event timed out")
}

#[tokio::test]
async fn failed_reply_text_stays_out_of_the_conversation_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let failed_finish = CancellationToken::new();
    failed_finish.cancel();
    let model = Arc::new(StreamingModel {
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(VecDeque::from([Reply {
            deltas: vec![
                ModelDelta::Text("尚未提交的回答：".into()),
                ModelDelta::Text("第一行\n第二行".into()),
            ],
            finish: failed_finish,
            outcome: Err(ModelError::StreamInterrupted),
        }])),
    });
    let agent = Agent::open_embedded(archive.clone(), support::config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let failed = agent
        .submit(chat, None, "Please answer".into())
        .await
        .unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Failed(_))).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops[0].state, LoopState::Failed);
    assert!(
        snapshot
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    agent.shutdown().await;
    drop(agent);

    let restored_model = Arc::new(support::ScriptedModel::new(vec![support::text(
        "After restart",
    )]));
    let restored =
        Agent::open_embedded(archive, support::config(), restored_model.clone()).unwrap();
    restored.resume_chat(chat).await.unwrap();
    assert!(
        !serde_json::to_string(&restored.snapshot(chat).await.unwrap().messages)
            .unwrap()
            .contains("尚未提交的回答")
    );
    let mut events = restored.subscribe();
    restored
        .submit(chat, Some(failed.loop_id), "Continue after restart".into())
        .await
        .unwrap();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    assert!(
        restored_model
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !serde_json::to_string(&request.messages)
                .unwrap()
                .contains("尚未提交的回答"))
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn invalid_tool_reply_is_neither_committed_nor_executed() {
    verify_rejected_tool_reply(false).await;
}

#[tokio::test]
async fn repeated_tool_reply_keeps_only_the_completed_tool_step() {
    verify_rejected_tool_reply(true).await;
}

async fn verify_rejected_tool_reply(repeat_prior_call: bool) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("notes.txt"),
        "Read-only fixture content",
    )
    .unwrap();
    let finish = CancellationToken::new();
    finish.cancel();
    let mut replies = VecDeque::new();
    if repeat_prior_call {
        replies.push_back(Reply {
            deltas: vec![ModelDelta::Text("Committed earlier reply".into())],
            finish: finish.clone(),
            outcome: Ok(vec![
                Content::Text {
                    text: "Committed earlier reply".into(),
                },
                Content::ToolUse {
                    id: "read-notes".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "notes.txt"}),
                },
            ]),
        });
    }
    replies.push_back(Reply {
        deltas: vec![ModelDelta::Text("已显示但工具调用无效".into())],
        finish,
        outcome: Ok(vec![
            Content::Text {
                text: "已显示但工具调用无效".into(),
            },
            Content::ToolUse {
                id: if repeat_prior_call {
                    "read-notes".into()
                } else {
                    String::new()
                },
                name: "Write".into(),
                input: json!({"file_path": "must-not-exist.txt", "content": "unauthorized result"}),
            },
        ]),
    });
    let model = Arc::new(StreamingModel {
        replies: Mutex::new(replies),
        requests: Mutex::new(Vec::new()),
    });
    let agent =
        Agent::open_embedded(directory.path().join("archive"), support::config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Read notes before proceeding".into())
        .await
        .unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == "已显示但工具调用无效"),
    )
    .await;
    wait_for(&mut events, |kind| matches!(kind, EventKind::Failed(error) if error.contains("invalid or repeated tool call"))).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        !serde_json::to_string(&snapshot.messages)
            .unwrap()
            .contains("已显示但工具调用无效")
    );
    assert!(!directory.path().join("must-not-exist.txt").exists());
    assert!(snapshot.permissions.is_empty());
    assert!(
        snapshot
            .operations
            .iter()
            .all(|operation| operation.name == "Read")
    );
    assert_eq!(
        snapshot
            .messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count(),
        usize::from(repeat_prior_call)
    );
    if repeat_prior_call {
        assert!(
            serde_json::to_string(&snapshot.messages)
                .unwrap()
                .contains("Committed earlier reply")
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn an_empty_failed_reply_does_not_persist_private_thinking() {
    let directory = tempfile::tempdir().unwrap();
    let finish = CancellationToken::new();
    finish.cancel();
    let model = Arc::new(StreamingModel {
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(VecDeque::from([Reply {
            deltas: vec![ModelDelta::Thinking("Hidden model reasoning".into())],
            finish,
            outcome: Err(ModelError::ProviderFailure),
        }])),
    });
    let agent = Agent::new_embedded(support::config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Please answer".into())
        .await
        .unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Failed(_))).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("Hidden model reasoning")
    );
    agent.shutdown().await;
}
