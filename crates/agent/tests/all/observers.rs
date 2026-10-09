use crate::support;
use std::sync::Arc;
use support::*;
use tickloop_agent::runtime::{Agent, EventKind, LoopState, RewindMode};
use tickloop_agent_protocol::{Decision, PermissionRule};
use uuid::Uuid;

#[tokio::test]
async fn a_detached_observer_can_replay_completion_from_its_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::open_embedded(
        directory.path().join("store"),
        config(),
        Arc::new(ScriptedModel::new(vec![text("finished without the view")])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let original = agent.observe(chat, 0).await.unwrap();
    let mut second = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "continue while detached".into())
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut second).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    let reattached = agent
        .observe(chat, original.snapshot.last_sequence)
        .await
        .unwrap();
    assert_eq!(reattached.snapshot.loops[0].id, receipt.loop_id);
    assert_eq!(reattached.snapshot.loops[0].state, LoopState::Completed);
    assert!(
        reattached
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::Completed { .. }))
    );
    let cursor = reattached.snapshot.last_sequence;
    assert!(agent.observe(chat, cursor).await.unwrap().events.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn conversation_rewind_requires_an_observer_reset_without_replaying_hidden_events() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("store");
    let agent = Agent::open_embedded(
        state.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("retained answer"),
            text("abandoned answer"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();

    let mut abandoned_run = None;
    for prompt in ["retained prompt", "abandoned prompt"] {
        let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
        abandoned_run = Some(receipt.run_id);
    }

    let before = agent.snapshot(chat).await.unwrap();
    let checkpoint = before.rewind_points()[1].clone();
    let checkpoint_cursor = before
        .events
        .iter()
        .find(|event| Some(event.run_id) == abandoned_run)
        .unwrap()
        .sequence
        - 1;
    let observer_cursor = before.last_sequence;
    let rewind = agent
        .rewind(chat, checkpoint.message_id, RewindMode::Conversation)
        .await
        .unwrap();

    let observed = agent.observe(chat, checkpoint_cursor).await.unwrap();
    assert!(observed.reset_required);
    assert_eq!(observed.events.len(), 1);
    assert_eq!(observed.events[0].sequence, rewind.snapshot.last_sequence);
    assert!(matches!(
        &observed.events[0].kind,
        EventKind::ConversationRewound { message_id } if *message_id == checkpoint.message_id
    ));
    assert!(
        !observed.events.iter().any(
            |event| matches!(&event.kind, EventKind::Text(text) if text == "abandoned answer")
        )
    );

    let observer_after_rewind = agent.observe(chat, observer_cursor).await.unwrap();
    assert!(observer_after_rewind.reset_required);
    assert!(matches!(
        observer_after_rewind.events.as_slice(),
        [event] if matches!(event.kind, EventKind::ConversationRewound { .. })
    ));
    let rewind_cursor = rewind.snapshot.last_sequence;
    let caught_up = agent.observe(chat, rewind_cursor).await.unwrap();
    assert!(!caught_up.reset_required);
    assert!(caught_up.events.is_empty());
    agent.shutdown().await;
}

struct BurstAfterCommand;

#[async_trait::async_trait]
impl tickloop_agent_model::ModelClient for BurstAfterCommand {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(support::title_output(request))
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
        sender: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        use tickloop_agent_model::*;
        use tickloop_agent_protocol::{Content, Usage};
        let content = if request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::ToolResult { .. }))
        {
            for _ in 0..300 {
                let _ = sender.send(ModelDelta::Text("part ".into()));
                let _ = sender.send(ModelDelta::Usage(Usage::default()));
            }
            text("The real command finished while observers were slow.")
        } else {
            call(
                "Bash",
                "Bash",
                serde_json::json!({"command":"printf start; sleep 0.2; printf end"}),
            )
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn two_slow_observers_recover_the_same_real_command_without_stalling_execution() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(BurstAfterCommand));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, tickloop_agent_protocol::ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
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
    let cursor = agent.observe(chat, 0).await.unwrap().snapshot.last_sequence;
    let mut first = agent.subscribe();
    let mut second = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Observe a long command".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if let Some(request) = snapshot.permissions.first() {
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
                continue;
            }
            if snapshot.loops[0].state == LoopState::Completed {
                break;
            }
            assert_ne!(snapshot.loops[0].state, LoopState::Failed);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        first.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    assert!(matches!(
        second.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    drop(first);
    for _ in 0..2 {
        let observed = agent.observe(chat, cursor).await.unwrap();
        assert_eq!(observed.snapshot.loops[0].id, receipt.loop_id);
        assert!(
            observed
                .events
                .iter()
                .any(|event| matches!(event.kind, EventKind::Completed { .. }))
        );
        assert!(observed.events.iter().any(
            |event| matches!(&event.kind, EventKind::ToolStarted {name, ..} if name == "Bash")
        ));
        assert!(
            agent
                .observe(chat, observed.snapshot.last_sequence)
                .await
                .unwrap()
                .events
                .is_empty()
        );
    }
    let commands = agent.commands(chat).await.unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].state,
        tickloop_agent_protocol::CommandState::Completed
    );
    let output = agent
        .read_command(chat, commands[0].operation_id, 0, 1024)
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(output["output"], "startend");
    agent.shutdown().await;
}
