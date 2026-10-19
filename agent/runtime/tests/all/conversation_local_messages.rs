use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    ChatId, ChatSnapshot, CommandState, Content, Event, EventKind, LoopState, Message, Role,
    Trigger, TriggerPromptSource, UserShellState,
};
use tickloop_agent_runtime::{Agent, AgentError};
use tickloop_agent_test_support::{ScriptedModel, call, config, text};
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

const INTERRUPT_MESSAGE: &str = "[Request interrupted by user]";
const INTERRUPT_MESSAGE_FOR_TOOL_USE: &str = "[Request interrupted by user for tool use]";

struct Reply {
    deltas: Vec<ModelDelta>,
    finish: CancellationToken,
    content: Vec<Content>,
}

struct StreamingModel {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl StreamingModel {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
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
        for delta in reply.deltas {
            events.send(delta).unwrap();
        }
        tokio::select! {
            _ = reply.finish.cancelled() => {},
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
        }
        Ok(ModelOutput {
            private_source: None,
            content: reply.content,
            usage: Default::default(),
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
        }
    })
    .await
    .expect("run event timed out")
}

async fn wait_for_snapshot(
    agent: &Agent,
    chat: ChatId,
    predicate: impl Fn(&ChatSnapshot) -> bool,
) -> ChatSnapshot {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("snapshot condition timed out")
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text: text.into() }],
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![Content::Text { text: text.into() }],
    }
}

fn conversation_texts(snapshot: &ChatSnapshot) -> Vec<String> {
    snapshot
        .conversation_messages()
        .unwrap()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn non_system(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter(|message| message.role != Role::System)
        .cloned()
        .collect()
}

fn assert_every_tool_use_is_answered(messages: &[Message]) {
    for (index, message) in messages.iter().enumerate() {
        for content in &message.content {
            if let Content::ToolUse { id, .. } = content {
                let answered = messages.get(index + 1).is_some_and(|next| {
                    next.content.iter().any(|content| {
                        matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == id)
                    })
                });
                assert!(
                    answered,
                    "tool_use {id} has no immediate tool_result: {messages:#?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn escape_keeps_the_partial_reply_and_interrupt_marker_for_the_next_request() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let finished = CancellationToken::new();
    finished.cancel();
    let model = Arc::new(StreamingModel::new(vec![
        Reply {
            deltas: vec![
                ModelDelta::Text("部分".into()),
                ModelDelta::Text("回答".into()),
            ],
            finish: CancellationToken::new(),
            content: text("never committed"),
        },
        Reply {
            deltas: Vec::new(),
            finish: finished,
            content: text("Continuing"),
        },
    ]));
    let agent = Agent::open_embedded(archive.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Please answer".into())
        .await
        .unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text.ends_with("回答")),
    )
    .await;
    agent.interrupt(chat).await.unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Interrupted)).await;

    let interrupted = vec![
        user("Please answer"),
        assistant("部分回答"),
        user(INTERRUPT_MESSAGE),
    ];
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .conversation_messages()
            .unwrap(),
        interrupted
    );

    agent
        .submit(chat, Some(receipt.loop_id), "Continue".into())
        .await
        .unwrap();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let mut expected_request = interrupted.clone();
    expected_request.push(user("Continue"));
    assert_eq!(
        non_system(&model.requests.lock().unwrap()[1].messages),
        expected_request
    );
    agent.shutdown().await;
    drop(agent);

    let restored =
        Agent::open_embedded(archive, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    restored.resume_chat(chat).await.unwrap();
    let mut expected_history = expected_request;
    expected_history.push(assistant("Continuing"));
    assert_eq!(
        restored
            .snapshot(chat)
            .await
            .unwrap()
            .conversation_messages()
            .unwrap(),
        expected_history
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn interrupting_a_tool_call_answers_it_and_records_the_tool_use_marker() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "shell-call",
            "Bash",
            json!({"command": "env sh -c 'printf a; printf b'"}),
        ),
        text("Understood"),
    ]));
    let agent =
        Agent::open_embedded(directory.path().join("archive"), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "Run it".into()).await.unwrap();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::PermissionRequested { .. })
    })
    .await;
    agent.interrupt(chat).await.unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Interrupted)).await;

    let messages = agent
        .snapshot(chat)
        .await
        .unwrap()
        .conversation_messages()
        .unwrap();
    assert_every_tool_use_is_answered(&messages);
    assert!(matches!(
        messages[2].content.as_slice(),
        [Content::ToolResult { tool_use_id, is_error: true, .. }] if tool_use_id == "shell-call"
    ));
    assert_eq!(messages.last(), Some(&user(INTERRUPT_MESSAGE_FOR_TOOL_USE)));
    assert!(
        !conversation_texts(&agent.snapshot(chat).await.unwrap())
            .contains(&INTERRUPT_MESSAGE.to_string())
    );

    agent
        .submit(chat, Some(receipt.loop_id), "Do not run it".into())
        .await
        .unwrap();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let request = non_system(&model.requests.lock().unwrap()[1].messages);
    assert_every_tool_use_is_answered(&request);
    assert_eq!(
        request[request.len() - 2..],
        [user(INTERRUPT_MESSAGE_FOR_TOOL_USE), user("Do not run it")]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn user_shell_input_and_output_are_user_messages_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let agent = Agent::open_embedded(
        archive.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit_user_shell_for_goal(chat, None, "printf '<ok>'; printf warn >&2".into(), false)
        .await
        .unwrap();
    let expected = vec![
        user("<bash-input>printf '<ok>'; printf warn >&2</bash-input>"),
        user("<bash-stdout>&lt;ok&gt;warn</bash-stdout><bash-stderr></bash-stderr>"),
    ];
    wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot.conversation_messages().unwrap() == expected
    })
    .await;
    agent.shutdown().await;
    drop(agent);

    let model = Arc::new(ScriptedModel::new(vec![text("Seen")]));
    let restored = Agent::open_embedded(archive, config(), model.clone()).unwrap();
    restored.install_own_tickhost();
    restored.resume_chat(chat).await.unwrap();
    // 照 Claude Code：恢复时末尾是 user 消息，追加不请求回复的 assistant 哨兵。
    let mut expected = expected;
    expected.push(assistant("No response requested."));
    assert_eq!(
        restored
            .snapshot(chat)
            .await
            .unwrap()
            .conversation_messages()
            .unwrap(),
        expected
    );
    let mut events = restored.subscribe();
    restored
        .submit(chat, None, "What happened?".into())
        .await
        .unwrap();
    wait_for(&mut events, |kind| {
        matches!(kind, EventKind::Completed { .. })
    })
    .await;
    let mut expected_request = expected;
    expected_request.push(user("What happened?"));
    assert_eq!(
        non_system(&model.requests.lock().unwrap()[0].messages),
        expected_request
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn a_failing_user_shell_reports_its_output_as_standard_error() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit_user_shell_for_goal(chat, None, "printf broken; exit 3".into(), false)
        .await
        .unwrap();
    let expected = vec![
        user("<bash-input>printf broken; exit 3</bash-input>"),
        user("<bash-stdout></bash-stdout><bash-stderr>broken\nExit code 3</bash-stderr>"),
    ];
    wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot.conversation_messages().unwrap() == expected
    })
    .await;
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_a_foreground_user_shell_replaces_its_output_with_the_interrupt_marker() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let receipt = agent
        .submit_user_shell_for_goal(chat, None, "printf started; sleep 30".into(), false)
        .await
        .unwrap();
    // The command runs once the Tickhost started it and named its process.
    wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot.user_shells.iter().any(|record| {
            record.id == receipt.command_id
                && record.state == UserShellState::Running
                && record.pid.is_some()
        })
    })
    .await;
    agent
        .stop_user_shell(chat, receipt.command_id)
        .await
        .unwrap();
    let expected = vec![
        user("<bash-input>printf started; sleep 30</bash-input>"),
        user(INTERRUPT_MESSAGE),
    ];
    wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot.conversation_messages().unwrap() == expected
    })
    .await;
    agent.shutdown().await;
}

#[tokio::test]
async fn background_user_shell_completion_notifies_its_chat() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let agent = Agent::open_embedded(
        archive.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut notifications = agent.subscribe_task_notifications();
    let receipt = agent
        .submit_user_shell_for_goal(chat, None, "printf done".into(), true)
        .await
        .unwrap();
    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .iter()
            .any(|text| text.starts_with("<task-notification>"))
    })
    .await;
    let output_file = snapshot
        .user_shells
        .iter()
        .find(|record| record.id == receipt.command_id)
        .and_then(|record| record.output_ref.clone())
        .expect("background shell output file");
    let expected_notification = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<output-file>{}</output-file>\n<status>completed</status>\n<summary>Background command \"printf done\" completed (exit code 0)</summary>\n</task-notification>",
        receipt.command_id,
        output_file.display()
    );
    let expected_texts = vec![
        "<bash-input>printf done</bash-input>".to_string(),
        format!(
            "<bash-stdout>Command running in background with ID: {}. Output is being written to: {}</bash-stdout><bash-stderr></bash-stderr>",
            receipt.command_id,
            output_file.display()
        ),
        expected_notification.clone(),
    ];
    assert_eq!(conversation_texts(&snapshot), expected_texts);
    let notification = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(notification.chat_id, chat);
    assert_eq!(notification.task_id, receipt.command_id);
    agent.shutdown().await;
    drop(agent);

    let restored =
        Agent::open_embedded(archive, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    restored.install_own_tickhost();
    restored.resume_chat(chat).await.unwrap();
    let restored_snapshot = restored.snapshot(chat).await.unwrap();
    let mut expected_texts = expected_texts;
    expected_texts.push("No response requested.".into());
    assert_eq!(conversation_texts(&restored_snapshot), expected_texts);
    assert!(restored_snapshot.user_shells.is_empty());
    restored.shutdown().await;
}

#[tokio::test]
async fn background_agent_command_completion_notifies_with_its_tool_use_id() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "background-shell",
            "Bash",
            json!({
                "command": format!("printf before; while [ ! -e '{}' ]; do sleep 0.05; done; exit 4", gate.display()),
                "run_in_background": true
            }),
        ),
        text("The task is running."),
        // The completion notice continues this Loop; see task_notification_runs.rs.
        text("The background command failed."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "start it".into()).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
    let running = wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot
            .commands
            .first()
            .is_some_and(|command| command.state == CommandState::Background)
    })
    .await;
    assert!(
        !conversation_texts(&running)
            .iter()
            .any(|text| text.contains("<task-notification>"))
    );
    let command = running.commands[0].clone();

    std::fs::write(&gate, b"release").unwrap();
    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .iter()
            .any(|text| text.starts_with("<task-notification>"))
    })
    .await;
    let command_text = format!(
        "printf before; while [ ! -e '{}' ]; do sleep 0.05; done; exit 4",
        gate.display()
    );
    assert_eq!(
        conversation_texts(&snapshot)
            .iter()
            .find(|text| text.starts_with("<task-notification>"))
            .unwrap(),
        &format!(
            "<task-notification>\n<task-id>{}</task-id>\n<tool-use-id>background-shell</tool-use-id>\n<output-file>{}</output-file>\n<status>failed</status>\n<summary>Background command \"{}\" failed with exit code 4</summary>\n</task-notification>",
            command.operation_id,
            command.output_ref.display(),
            command_text.replace('<', "&lt;").replace('>', "&gt;"),
        )
    );
    agent.shutdown().await;
}

fn gated_command(output: &str, gate: &std::path::Path) -> String {
    format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done; printf {output}",
        gate.display()
    )
}

async fn wait_for_user_shell_state(
    agent: &Agent,
    chat: ChatId,
    command_id: uuid::Uuid,
    state: UserShellState,
) -> ChatSnapshot {
    wait_for_snapshot(agent, chat, |snapshot| {
        snapshot
            .user_shells
            .iter()
            .any(|record| record.id == command_id && record.state == state)
    })
    .await
}

#[tokio::test]
async fn a_task_notification_waits_until_the_running_foreground_shell_writes_its_input_and_output()
{
    let directory = tempfile::tempdir().unwrap();
    let background_gate = directory.path().join("background-release");
    let foreground_gate = directory.path().join("foreground-release");
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let background_command = gated_command("background", &background_gate);
    let background = agent
        .submit_user_shell_for_goal(chat, None, background_command.clone(), true)
        .await
        .unwrap();
    wait_for_user_shell_state(
        &agent,
        chat,
        background.command_id,
        UserShellState::Background,
    )
    .await;
    let foreground_command = gated_command("foreground", &foreground_gate);
    let foreground = agent
        .submit_user_shell_for_goal(chat, None, foreground_command.clone(), false)
        .await
        .unwrap();
    wait_for_user_shell_state(&agent, chat, foreground.command_id, UserShellState::Running).await;

    std::fs::write(&background_gate, b"release").unwrap();
    let finished_background = wait_for_user_shell_state(
        &agent,
        chat,
        background.command_id,
        UserShellState::Completed,
    )
    .await;
    let background_texts = conversation_texts(&finished_background);
    assert_eq!(background_texts.len(), 2, "{background_texts:#?}");
    assert!(
        !background_texts
            .iter()
            .any(|text| text.starts_with("<task-notification>") || text.contains("foreground"))
    );

    std::fs::write(&foreground_gate, b"release").unwrap();
    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot).len() == 5
    })
    .await;
    let texts = conversation_texts(&snapshot);
    assert_eq!(
        texts[2..4],
        [
            format!("<bash-input>{foreground_command}</bash-input>"),
            "<bash-stdout>foreground</bash-stdout><bash-stderr></bash-stderr>".to_string(),
        ]
    );
    assert!(texts[4].starts_with("<task-notification>"), "{texts:#?}");
    agent.shutdown().await;
}

#[tokio::test]
async fn compaction_is_rejected_while_a_foreground_shell_is_running() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let command = gated_command("done", &gate);
    let receipt = agent
        .submit_user_shell_for_goal(chat, None, command.clone(), false)
        .await
        .unwrap();
    wait_for_user_shell_state(&agent, chat, receipt.command_id, UserShellState::Running).await;

    assert!(matches!(agent.compact(chat).await, Err(AgentError::Busy)));

    std::fs::write(&gate, b"release").unwrap();
    let expected = vec![
        user(&format!("<bash-input>{command}</bash-input>")),
        user("<bash-stdout>done</bash-stdout><bash-stderr></bash-stderr>"),
    ];
    wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot.conversation_messages().unwrap() == expected
    })
    .await;
    agent.shutdown().await;
}

#[tokio::test]
async fn an_expired_loop_cancels_its_model_request_without_interrupt_messages() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(StreamingModel::new(vec![Reply {
        deltas: vec![ModelDelta::Text("部分回答".into())],
        finish: CancellationToken::new(),
        content: text("never committed"),
    }]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Watch until the deadline".into())
        .await
        .unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == "部分回答"),
    )
    .await;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id: uuid::Uuid::new_v4(),
                prompt: "check".into(),
                prompt_source: TriggerPromptSource::Explicit,
                schedule: None,
                armed_at_ms: None,
                last_fired_at_ms: None,
                expires_at_ms: Some(now_ms + 100),
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Interrupted)).await;

    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops[0].state, LoopState::Expired);
    assert_eq!(
        snapshot.conversation_messages().unwrap(),
        vec![user("Watch until the deadline")]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_a_loop_keeps_the_partial_reply_and_interrupt_marker() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(StreamingModel::new(vec![Reply {
        deltas: vec![ModelDelta::Text("部分回答".into())],
        finish: CancellationToken::new(),
        content: text("never committed"),
    }]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Please answer".into())
        .await
        .unwrap();
    wait_for(
        &mut events,
        |kind| matches!(kind, EventKind::Text(text) if text == "部分回答"),
    )
    .await;
    agent.stop(chat, receipt.loop_id).await.unwrap();
    wait_for(&mut events, |kind| matches!(kind, EventKind::Interrupted)).await;

    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .conversation_messages()
            .unwrap(),
        vec![
            user("Please answer"),
            assistant("部分回答"),
            user(INTERRUPT_MESSAGE),
        ]
    );
    agent.shutdown().await;
}
