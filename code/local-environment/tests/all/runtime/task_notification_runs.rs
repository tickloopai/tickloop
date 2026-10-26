use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tickloop_code_local_environment::AgentLocalExt;

use async_trait::async_trait;
use serde_json::json;
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    ChatId, ChatSnapshot, CommandState, Content, Event, EventKind, LoopState, ModelIdentity,
    Protocol, Role, Usage,
};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::{ScriptedModel, call, config, next_event, text};
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

/// Pauses at `pause_at_request` so tests can finish a background command during an active run.
struct PausedModel {
    requests: Mutex<Vec<ModelRequest>>,
    replies: Mutex<VecDeque<Vec<Content>>>,
    pause_at_request: usize,
    paused: CancellationToken,
    resume: CancellationToken,
}

impl PausedModel {
    fn new(replies: Vec<Vec<Content>>, pause_at_request: usize) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
            pause_at_request,
            paused: CancellationToken::new(),
            resume: CancellationToken::new(),
        }
    }
}

#[async_trait]
impl ModelClient for PausedModel {
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_harness::protocol::ModelProvider::Api,
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
        let sequence = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len()
        };
        let content = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        if sequence == self.pause_at_request {
            self.paused.cancel();
            tokio::select! {
                _ = self.resume.cancelled() => {}
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
        }
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

#[tokio::test]
async fn background_agent_command_completion_continues_its_goal() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "background-shell",
            "Bash",
            json!({
                "command": gated_command(&gate),
                "run_in_background": true,
                "description": "Wait for the release gate"
            }),
        ),
        text("started in the background"),
        text("the background command finished"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "start it".into()).await.unwrap();
    finish_run(&agent, chat, &mut events).await;
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    let started = agent.snapshot(chat).await.unwrap();
    let command = &started.commands[0];
    assert_eq!(
        tool_result_text(&started, "background-shell"),
        format!(
            "Command running in background with ID: {}. Output is being written to: {}",
            tickloop_ids::IdContext::new().model_text(
                tickloop_harness::protocol::ShellTaskId::from(command.operation_id)
            ),
            command.output_ref.display()
        )
    );

    std::fs::write(&gate, b"release").unwrap();

    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .last()
            .is_some_and(|text| text == "the background command finished")
    })
    .await;
    let texts = conversation_texts(&snapshot);
    let notification = texts
        .iter()
        .find(|text| text.starts_with("<task-notification>"))
        .expect("task notification message");
    assert!(
        notification.contains("<status>completed</status>"),
        "{notification}"
    );
    for expected in [
        format!("<task-id>{}</task-id>", tickloop_ids::IdContext::new().model_text(tickloop_harness::protocol::ShellTaskId::from(command.operation_id))),
        "<tool-use-id>background-shell</tool-use-id>".into(),
        format!("<output-file>{}</output-file>", command.output_ref.display()),
        "<summary>Background command \"Wait for the release gate\" completed (exit code 0)</summary>"
            .into(),
    ] {
        assert!(notification.contains(&expected), "{notification}");
    }
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        request_texts(requests.last().unwrap())
            .iter()
            .any(|text| text.starts_with("<task-notification>"))
    );
}

#[tokio::test]
async fn a_foreground_command_past_its_timeout_moves_to_the_background_and_notifies_on_completion()
{
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "slow-shell",
            "Bash",
            json!({
                "command": gated_command(&gate),
                "timeout": 200,
                "description": "Wait for the release gate"
            }),
        ),
        text("the command moved to the background"),
        text("the background command finished"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "run it".into()).await.unwrap();
    finish_run(&agent, chat, &mut events).await;
    let moved = agent.snapshot(chat).await.unwrap();
    let command = &moved.commands[0];
    assert!(command.was_background);
    assert_eq!(
        tool_result_text(&moved, "slow-shell"),
        format!(
            "Command running in background with ID: {}. Output is being written to: {}",
            tickloop_ids::IdContext::new().model_text(
                tickloop_harness::protocol::ShellTaskId::from(command.operation_id)
            ),
            command.output_ref.display()
        )
    );

    std::fs::write(&gate, b"release").unwrap();

    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .last()
            .is_some_and(|text| text == "the background command finished")
    })
    .await;
    let notification = conversation_texts(&snapshot)
        .into_iter()
        .find(|text| text.starts_with("<task-notification>"))
        .expect("task notification message");
    for expected in [
        format!("<task-id>{}</task-id>", tickloop_ids::IdContext::new().model_text(tickloop_harness::protocol::ShellTaskId::from(command.operation_id))),
        "<tool-use-id>slow-shell</tool-use-id>".into(),
        "<status>completed</status>".into(),
        "<summary>Background command \"Wait for the release gate\" completed (exit code 0)</summary>"
            .into(),
    ] {
        assert!(notification.contains(&expected), "{notification}");
    }
}

#[tokio::test]
async fn a_foreground_command_the_user_backgrounds_returns_at_once_and_notifies_on_completion() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "long-shell",
            "Bash",
            json!({
                "command": format!("printf began; {}; printf finished", gated_command(&gate)),
                "description": "Wait for the release gate"
            }),
        ),
        text("the user moved the command to the background"),
        text("the background command finished"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "run it".into()).await.unwrap();
    let running = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::CommandUpdated { command } if command.state == CommandState::Running => {
                break command;
            }
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    };

    let backgrounded = agent
        .background_command(chat, running.operation_id)
        .await
        .unwrap();
    assert_eq!(backgrounded.state, CommandState::Background);
    assert_eq!(backgrounded.pid, running.pid);
    finish_run(&agent, chat, &mut events).await;
    let moved = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        tool_result_text(&moved, "long-shell"),
        format!(
            "Command was manually backgrounded by user with ID: {}. Output is being written to: {}",
            tickloop_ids::IdContext::new().model_text(
                tickloop_harness::protocol::ShellTaskId::from(running.operation_id)
            ),
            running.output_ref.display()
        )
    );
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert_eq!(
        agent.commands(chat).await.unwrap()[0].state,
        CommandState::Background,
        "the tool returned while its command keeps running"
    );

    std::fs::write(&gate, b"release").unwrap();

    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .last()
            .is_some_and(|text| text == "the background command finished")
    })
    .await;
    let notification = conversation_texts(&snapshot)
        .into_iter()
        .find(|text| text.starts_with("<task-notification>"))
        .expect("task notification message");
    for expected in [
        format!("<task-id>{}</task-id>", tickloop_ids::IdContext::new().model_text(tickloop_harness::protocol::ShellTaskId::from(running.operation_id))),
        "<tool-use-id>long-shell</tool-use-id>".into(),
        "<status>completed</status>".into(),
        "<summary>Background command \"Wait for the release gate\" completed (exit code 0)</summary>"
            .into(),
    ] {
        assert!(notification.contains(&expected), "{notification}");
    }
    assert_eq!(
        std::fs::read_to_string(&running.output_ref).unwrap(),
        "beganfinished",
        "output written after backgrounding lands in the same file"
    );
    assert_eq!(model.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn background_user_shell_completion_continues_the_goal_that_started_it() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(ScriptedModel::new(vec![
        text("hello there"),
        text("the shell command finished"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "hello".into()).await.unwrap();
    finish_run(&agent, chat, &mut events).await;

    agent
        .submit_user_shell(chat, gated_command(&gate), true)
        .await
        .unwrap();
    std::fs::write(&gate, b"release").unwrap();

    wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .last()
            .is_some_and(|text| text == "the shell command finished")
    })
    .await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        request_texts(requests.last().unwrap())
            .iter()
            .any(|text| text.starts_with("<task-notification>"))
    );
}

#[tokio::test]
async fn background_completion_does_not_resume_an_interrupted_loop() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(PausedModel::new(
        vec![
            call(
                "background-shell",
                "Bash",
                json!({ "command": gated_command(&gate), "run_in_background": true }),
            ),
            text("interrupted reply"),
            text("unexpected continuation"),
        ],
        2,
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "start it".into()).await.unwrap();

    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
                break;
            }
            EventKind::Failed(error) => panic!("run failed before interruption: {error}"),
            _ => {}
        }
    }

    tokio::time::timeout(Duration::from_secs(30), model.paused.cancelled())
        .await
        .expect("model did not reach the paused reply");
    agent.interrupt(chat).await.unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::Interrupted) {
            break;
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Interrupted
    );

    std::fs::write(&gate, b"release").unwrap();
    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .iter()
            .any(|text| text.starts_with("<task-notification>"))
    })
    .await;
    assert_eq!(snapshot.chat_loop.state, LoopState::Interrupted);
    assert_eq!(model.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn background_completion_does_not_resume_a_failed_loop() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(ScriptedModel::new(vec![call(
        "background-shell",
        "Bash",
        json!({ "command": gated_command(&gate), "run_in_background": true }),
    )]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "start it".into()).await.unwrap();

    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::Failed(_) => break,
            _ => {}
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Failed
    );

    std::fs::write(&gate, b"release").unwrap();
    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .iter()
            .any(|text| text.starts_with("<task-notification>"))
    })
    .await;
    assert_eq!(snapshot.chat_loop.state, LoopState::Failed);
    assert_eq!(model.requests.lock().unwrap().len(), 2);
}

/// A `!` command before the Loop's first turn only records its completion.
#[tokio::test]
async fn a_background_user_shell_completion_before_any_turn_only_records_the_notification() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .submit_user_shell(chat, "printf done".into(), true)
        .await
        .unwrap();

    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .iter()
            .any(|text| text.starts_with("<task-notification>"))
    })
    .await;
    assert!(snapshot.chat_loop.run_id.is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(model.requests.lock().unwrap().is_empty());
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .pending_inputs
            .is_empty()
    );
}

#[tokio::test]
async fn background_completions_during_a_run_continue_the_goal_once() {
    let directory = tempfile::tempdir().unwrap();
    let first_gate = directory.path().join("first");
    let second_gate = directory.path().join("second");
    let model = Arc::new(PausedModel::new(
        vec![
            call(
                "first-shell",
                "Bash",
                json!({ "command": gated_command(&first_gate), "run_in_background": true }),
            ),
            call(
                "second-shell",
                "Bash",
                json!({ "command": gated_command(&second_gate), "run_in_background": true }),
            ),
            text("both are running"),
            text("both background commands finished"),
        ],
        3,
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "start both".into()).await.unwrap();
    let approvals = tokio::spawn({
        let agent = agent.clone();
        async move {
            let mut answered = 0;
            while answered < 2 {
                if let EventKind::PermissionRequested { request } =
                    next_event(&mut events).await.kind
                {
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                    answered += 1;
                }
            }
        }
    });
    approvals.await.unwrap();

    tokio::time::timeout(Duration::from_secs(30), model.paused.cancelled())
        .await
        .expect("model did not reach the paused reply");
    std::fs::write(&first_gate, b"release").unwrap();
    std::fs::write(&second_gate, b"release").unwrap();
    wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot
            .commands
            .iter()
            .filter(|command| command.state == tickloop_harness::protocol::CommandState::Completed)
            .count()
            == 2
    })
    .await;
    model.resume.cancel();

    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .last()
            .is_some_and(|text| text == "both background commands finished")
    })
    .await;
    assert_eq!(
        conversation_texts(&snapshot)
            .iter()
            .filter(|text| text.starts_with("<task-notification>"))
            .count(),
        2
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 4, "only one continuation run is expected");
    assert_eq!(
        request_texts(requests.last().unwrap())
            .iter()
            .filter(|text| text.starts_with("<task-notification>"))
            .count(),
        2
    );
}

/// A run still calling tools consumes its own background completion without starting another run.
#[tokio::test]
async fn a_completion_inside_a_running_tool_loop_does_not_add_a_run() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(PausedModel::new(
        vec![
            call(
                "background-shell",
                "Bash",
                json!({ "command": gated_command(&gate), "run_in_background": true }),
            ),
            call("follow-up", "Bash", json!({ "command": "printf ok" })),
            text("the background command finished while I was working"),
        ],
        2,
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "start it".into()).await.unwrap();
    let approvals = tokio::spawn({
        let agent = agent.clone();
        async move {
            loop {
                match next_event(&mut events).await.kind {
                    EventKind::PermissionRequested { request } => {
                        agent
                            .answer_permission(chat, request.id, true)
                            .await
                            .unwrap();
                    }
                    EventKind::Completed { .. } => return,
                    EventKind::Failed(error) => panic!("run failed: {error}"),
                    _ => {}
                }
            }
        }
    });

    tokio::time::timeout(Duration::from_secs(30), model.paused.cancelled())
        .await
        .expect("model did not reach the paused reply");
    std::fs::write(&gate, b"release").unwrap();
    wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot
            .commands
            .iter()
            .any(|command| command.state == tickloop_harness::protocol::CommandState::Completed)
    })
    .await;
    model.resume.cancel();
    approvals.await.unwrap();

    let snapshot = wait_for_snapshot(&agent, chat, |snapshot| {
        conversation_texts(snapshot)
            .last()
            .is_some_and(|text| text == "the background command finished while I was working")
    })
    .await;
    assert!(snapshot.pending_inputs.is_empty());
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "the running turn carries the notification"
    );
    assert!(
        request_texts(requests.last().unwrap())
            .iter()
            .any(|text| text.starts_with("<task-notification>"))
    );
}

#[tokio::test]
async fn background_completion_does_not_continue_another_chat() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(ScriptedModel::new(vec![
        text("first chat ready"),
        text("second chat ready"),
        text("the shell command finished"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let first = agent.create_chat(directory.path().into()).await.unwrap();
    let second = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(first, "hello".into()).await.unwrap();
    finish_run(&agent, first, &mut events).await;
    agent.submit(second, "hello".into()).await.unwrap();
    finish_run(&agent, second, &mut events).await;
    let untouched = conversation_texts(&agent.snapshot(second).await.unwrap());

    agent
        .submit_user_shell(first, gated_command(&gate), true)
        .await
        .unwrap();
    std::fs::write(&gate, b"release").unwrap();

    wait_for_snapshot(&agent, first, |snapshot| {
        conversation_texts(snapshot)
            .last()
            .is_some_and(|text| text == "the shell command finished")
    })
    .await;
    assert_eq!(
        conversation_texts(&agent.snapshot(second).await.unwrap()),
        untouched
    );
    assert_eq!(model.requests.lock().unwrap().len(), 3);
}

fn tool_result_text(snapshot: &ChatSnapshot, call_id: &str) -> String {
    snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.text().into_owned()),
            _ => None,
        })
        .expect("tool result")
}

fn gated_command(gate: &std::path::Path) -> String {
    format!("while [ ! -e '{}' ]; do sleep 0.05; done", gate.display())
}

async fn finish_run(agent: &Agent, chat: ChatId, events: &mut broadcast::Receiver<Event>) {
    loop {
        match next_event(events).await.kind {
            EventKind::PermissionRequested { request } => {
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
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

fn request_texts(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}
