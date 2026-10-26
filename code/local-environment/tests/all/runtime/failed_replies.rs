use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tickloop_code_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use serde_json::json;
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{Content, Event, EventKind, LoopState, Role, Usage};
use tickloop_harness::runtime::Agent;
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
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
        tickloop_harness::protocol::ModelIdentity {
            provider: tickloop_harness::protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_harness::protocol::Protocol::Messages,
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
            private_source: None,
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
    let agent = Agent::open_embedded(
        archive.clone(),
        tickloop_harness::test_support::config(),
        model,
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Please answer".into()).await.unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Failed(_))).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.chat_loop.state, LoopState::Failed);
    assert!(
        snapshot
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    agent.shutdown().await;
    drop(agent);

    let restored_model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
        tickloop_harness::test_support::text("After restart"),
    ]));
    let restored = Agent::open_embedded(
        archive,
        tickloop_harness::test_support::config(),
        restored_model.clone(),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    assert!(
        !serde_json::to_string(&restored.snapshot(chat).await.unwrap().messages)
            .unwrap()
            .contains("尚未提交的回答")
    );
    let mut events = restored.subscribe();
    restored
        .submit(chat, "Continue after restart".into())
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
    let agent = Agent::open_embedded(
        directory.path().join("archive"),
        tickloop_harness::test_support::config(),
        model,
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read notes before proceeding".into())
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
    let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Please answer".into()).await.unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Failed(_))).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("Hidden model reasoning")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn disabling_task_tools_returns_an_unavailable_result_for_a_stale_task_call() {
    let directory = tempfile::tempdir().unwrap();
    let ready = CancellationToken::new();
    ready.cancel();
    let model = Arc::new(StreamingModel {
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(VecDeque::from([
            Reply {
                deltas: Vec::new(),
                finish: ready.clone(),
                outcome: Ok(vec![Content::ToolUse {
                    id: "create-before-disable".into(),
                    name: "TaskCreate".into(),
                    input: json!({"mutation_id":uuid::Uuid::new_v4(),"title":"Existing task","description":"Preserve this task"}),
                }]),
            },
            Reply {
                deltas: Vec::new(),
                finish: ready.clone(),
                outcome: Ok(vec![Content::Text {
                    text: "Task recorded".into(),
                }]),
            },
            Reply {
                deltas: Vec::new(),
                finish: ready.clone(),
                outcome: Ok(["TaskList", "TaskGet", "TaskCreate", "TaskUpdate"]
                    .into_iter()
                    .map(|name| Content::ToolUse {
                        id: format!("stale-{name}"),
                        name: name.into(),
                        input: json!({}),
                    })
                    .collect()),
            },
            Reply {
                deltas: Vec::new(),
                finish: ready,
                outcome: Ok(vec![Content::Text {
                    text: "Continuing without task tools".into(),
                }]),
            },
        ])),
    });
    let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.set_task_tools_enabled(chat, true).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Record a task".into()).await.unwrap();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let before = agent.snapshot(chat).await.unwrap().task_boards;
    agent.set_task_tools_enabled(chat, false).await.unwrap();
    agent
        .submit(chat, "Continue without tasks".into())
        .await
        .unwrap();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.task_boards, before);
    assert!(
        !snapshot
            .operations
            .iter()
            .any(|operation| operation.call_id.starts_with("stale-"))
    );
    for name in ["TaskList", "TaskGet", "TaskCreate", "TaskUpdate"] {
        assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(
            content,
            Content::ToolResult { tool_use_id, content, is_error: true }
                if *tool_use_id == format!("stale-{name}") && content.text().contains("Task tools are disabled")
        )));
    }
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            !requests[2]
                .tools
                .iter()
                .any(|tool| tool.name.starts_with("Task") && tool.name != "TaskStop")
        );
        assert_eq!(requests.len(), 4);
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn disabled_task_calls_still_reject_duplicate_ids_and_malformed_or_unknown_calls() {
    let task_call = Content::ToolUse {
        id: "stale-task".into(),
        name: "TaskList".into(),
        input: json!({}),
    };
    for content in [
        vec![task_call.clone(), task_call.clone()],
        vec![Content::ToolUse {
            id: String::new(),
            name: "TaskList".into(),
            input: json!({}),
        }],
        vec![Content::ToolUse {
            id: "malformed".into(),
            name: "TaskList".into(),
            input: json!([]),
        }],
        vec![Content::ToolUse {
            id: "unknown".into(),
            name: "TaskDeleteEverything".into(),
            input: json!({}),
        }],
    ] {
        let directory = tempfile::tempdir().unwrap();
        let finish = CancellationToken::new();
        finish.cancel();
        let model = Arc::new(StreamingModel {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(VecDeque::from([Reply {
                deltas: Vec::new(),
                finish,
                outcome: Ok(content),
            }])),
        });
        let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Continue without task tools".into())
            .await
            .unwrap();
        wait_for(&mut events, |kind| matches!(kind, EventKind::Failed(error) if error.contains("invalid or repeated tool call"))).await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(snapshot.operations.is_empty());
        assert!(snapshot.task_boards.is_empty());
        assert!(
            snapshot
                .messages
                .iter()
                .all(|message| message.role != Role::Assistant)
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn disabling_task_tools_during_a_model_request_does_not_execute_its_task_create() {
    let directory = tempfile::tempdir().unwrap();
    let in_flight = CancellationToken::new();
    let done = CancellationToken::new();
    done.cancel();
    let model = Arc::new(StreamingModel {
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(VecDeque::from([
            Reply {
                deltas: vec![ModelDelta::Text("request-in-flight".into())],
                finish: in_flight.clone(),
                outcome: Ok(vec![Content::ToolUse {
                    id: "create-after-disable".into(),
                    name: "TaskCreate".into(),
                    input: json!({"mutation_id":uuid::Uuid::new_v4(),"title":"Do not create","description":"Disabled before dispatch"}),
                }]),
            },
            Reply {
                deltas: Vec::new(),
                finish: done,
                outcome: Ok(vec![Content::Text {
                    text: "Continuing without tasks".into(),
                }]),
            },
        ])),
    });
    let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.set_task_tools_enabled(chat, true).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Record a task".into()).await.unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == "request-in-flight"),
    )
    .await;
    agent.set_task_tools_enabled(chat, false).await.unwrap();
    in_flight.cancel();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .task_boards
            .values()
            .all(|board| board.tasks.is_empty())
    );
    assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(
        content,
        Content::ToolResult { tool_use_id, content, is_error: true }
            if tool_use_id == "create-after-disable" && content.text().contains("Task tools are disabled")
    )));
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "TaskCreate")
        );
        assert!(
            !requests[1]
                .tools
                .iter()
                .any(|tool| tool.name == "TaskCreate")
        );
    }
    agent.shutdown().await;
}
