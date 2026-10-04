mod support;

use async_trait::async_trait;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use support::{ScriptedModel, config, next_event, text};
use tickloop_agent::{
    model::{Content, ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest, Protocol},
    permissions::{Decision, ExecutionMode, PermissionRule},
    runtime::{Agent, EventKind, QueuedInputKind},
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

struct PendingModel;

struct GatedModel {
    calls: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl ModelClient for PendingModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

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
        _: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        cancel.cancelled().await;
        Err(ModelError::Cancelled)
    }
}

#[async_trait]
impl ModelClient for GatedModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

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
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        tokio::select! {
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            _ = self.release.notified() => {}
        }
        Ok(ModelOutput {
            private_source: None,
            content: text("done"),
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn queued_messages_join_the_active_tool_turn_with_a_terminal_redirect() {
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut model_config = config();
        model_config.protocol = protocol;
        let model = Arc::new(ScriptedModel::new(vec![
            vec![Content::ToolUse {
                id: "slow-shell".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command":"sleep 0.2"}),
            }],
            text("finished with the correction"),
        ]));
        let agent = Agent::new_embedded(model_config, model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::AcceptEdits)
            .await
            .unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: uuid::Uuid::new_v4(),
                    decision: Decision::Allow,
                    tool: "Bash".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let active = agent
            .submit(chat, None, "inspect the workspace".into())
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } => agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap(),
                EventKind::ToolStarted { .. } => break,
                _ => {}
            }
        }
        let queued = agent
            .submit(
                chat,
                Some(active.loop_id),
                "include the newly queued correction".into(),
            )
            .await
            .unwrap();
        let command_id = agent
            .queue_command(chat, Some(active.loop_id), "/compact".into())
            .await
            .unwrap();
        let after_command = agent
            .submit(
                chat,
                Some(active.loop_id),
                "do not cross the queued command".into(),
            )
            .await
            .unwrap();
        let mut redirected = false;
        loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::InputMerged { into_run_id } if event.run_id == queued.run_id => {
                    assert_eq!(into_run_id, active.run_id);
                    redirected = true;
                }
                EventKind::Completed { .. } if event.run_id == active.run_id => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(redirected);
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let second = &requests[1].messages;
            let tool_result = second.iter().position(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, Content::ToolResult { .. }))
            });
            let correction = second.iter().position(|message| {
                message.content.iter().any(|block| {
                    matches!(block, Content::Text { text } if text == "include the newly queued correction")
                })
            });
            assert!(tool_result < correction);
            assert!(!second.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(block, Content::Text { text } if text == "do not cross the queued command")
                })
            }));
        }
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            snapshot
                .queued_inputs()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![command_id, after_command.run_id]
        );
        let correction_index = snapshot
            .messages
            .iter()
            .position(|message| {
                message.content.iter().any(|block| {
                    matches!(block, Content::Text { text } if text == "include the newly queued correction")
                })
            })
            .unwrap();
        assert_eq!(
            snapshot.message_runs.get(&correction_index),
            Some(&queued.run_id)
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn takeback_preserves_typed_order_and_only_restores_an_isolated_shell() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(PendingModel));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let active = agent
        .submit(chat, None, "hold the execution slot".into())
        .await
        .unwrap();
    let queued = agent
        .submit(chat, Some(active.loop_id), "edit this message".into())
        .await
        .unwrap();
    let shell = agent
        .submit_user_shell_for_goal(chat, Some(active.loop_id), "printf shell".into(), false)
        .await
        .unwrap();

    let before = agent.snapshot(chat).await.unwrap().queued_inputs();
    assert_eq!(before.len(), 2);
    assert_eq!(before[0].id, queued.run_id);
    assert_eq!(before[0].kind, QueuedInputKind::Message);
    assert_eq!(before[1].id, shell.command_id);
    assert_eq!(before[1].kind, QueuedInputKind::Shell);

    let messages = agent.take_queued_inputs(chat, true).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, queued.run_id);
    assert!(
        agent
            .take_queued_inputs(chat, false)
            .await
            .unwrap()
            .is_empty()
    );
    let shells = agent.take_queued_inputs(chat, true).await.unwrap();
    assert_eq!(shells.len(), 1);
    assert_eq!(shells[0].id, shell.command_id);
    assert_eq!(shells[0].kind, QueuedInputKind::Shell);
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .queued_inputs()
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn queued_commands_form_a_persistent_barrier_between_model_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(GatedModel {
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let active = agent.submit(chat, None, "active".into()).await.unwrap();
    model.started.notified().await;
    let before = agent
        .submit(chat, Some(active.loop_id), "before command".into())
        .await
        .unwrap();
    let command_id = agent
        .queue_command(chat, Some(active.loop_id), "/compact".into())
        .await
        .unwrap();
    let after = agent
        .submit(chat, Some(active.loop_id), "after command".into())
        .await
        .unwrap();
    let queued = agent.snapshot(chat).await.unwrap().queued_inputs();
    assert_eq!(
        queued.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![before.run_id, command_id, after.run_id]
    );

    model.release.notify_one();
    model.started.notified().await;
    model.release.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let command = agent.take_next_queued_command(chat).await.unwrap().unwrap();
    assert_eq!(command.id, command_id);
    assert_eq!(command.kind, QueuedInputKind::Command);
    agent.finish_queued_command(chat, command_id).await.unwrap();
    model.started.notified().await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    model.release.notify_one();
    agent.shutdown().await;
}

#[tokio::test]
async fn interrupt_preserves_waiting_messages_in_one_follow_up_request() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(GatedModel {
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let active = agent.submit(chat, None, "active".into()).await.unwrap();
    model.started.notified().await;
    let first = agent
        .submit(chat, Some(active.loop_id), "first queued".into())
        .await
        .unwrap();
    let second = agent
        .submit(chat, Some(active.loop_id), "second queued".into())
        .await
        .unwrap();

    assert_ne!(first.run_id, second.run_id);
    assert_eq!(first.loop_id, second.loop_id);
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .queued_inputs()
            .iter()
            .map(|queued| queued.id)
            .collect::<Vec<_>>(),
        vec![first.run_id, second.run_id],
    );
    let mut events = agent.subscribe();
    agent.interrupt(chat).await.unwrap();
    model.started.notified().await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let consumed = agent.snapshot(chat).await.unwrap();
    assert!(consumed.queued_inputs().is_empty());
    let request = model.requests.lock().unwrap()[1].clone();
    let queued_texts = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } if text == "first queued" || text == "second queued" => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        queued_texts,
        ["first queued", "second queued"],
        "both accepted inputs appear once in the same model request"
    );
    for (expected, receipt) in [("first queued", &first), ("second queued", &second)] {
        let index = consumed
            .messages
            .iter()
            .position(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::Text { text } if text == expected))
            })
            .unwrap();
        assert_eq!(consumed.message_runs.get(&index), Some(&receipt.run_id));
    }
    assert!(
        consumed
            .events
            .iter()
            .any(|event| event.run_id == active.run_id
                && matches!(event.kind, EventKind::Interrupted))
    );
    let started = consumed
        .events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::Started))
        .map(|event| event.run_id)
        .collect::<Vec<_>>();
    assert_eq!(started, [active.run_id, first.run_id]);
    let merged = consumed
        .events
        .iter()
        .filter_map(|event| match event.kind {
            EventKind::InputMerged { into_run_id } => Some((event.run_id, into_run_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(merged, [(second.run_id, first.run_id)]);

    model.release.notify_one();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == first.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .queued_inputs()
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_queued_shell_is_not_restored_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let agent = Agent::open_embedded(archive.clone(), config(), Arc::new(PendingModel)).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let active = agent.submit(chat, None, "active".into()).await.unwrap();
    let shell = agent
        .submit_user_shell_for_goal(chat, Some(active.loop_id), "printf restored".into(), false)
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(archive, config(), Arc::new(PendingModel)).unwrap();
    reopened.resume_chat(chat).await.unwrap();
    let snapshot = reopened.snapshot(chat).await.unwrap();
    assert!(
        !snapshot
            .queued_inputs()
            .iter()
            .any(|entry| entry.id == shell.command_id || entry.kind == QueuedInputKind::Shell)
    );
    assert!(snapshot.user_shells.is_empty());
    assert!(
        reopened
            .take_queued_inputs(chat, true)
            .await
            .unwrap()
            .is_empty()
    );
    reopened.shutdown().await;
}
