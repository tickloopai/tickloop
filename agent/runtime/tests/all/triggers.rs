use std::sync::Arc;
use tickloop_agent_protocol::{EventKind, LoopState, Trigger, TriggerPromptSource, TriggerSource};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::*;
use uuid::Uuid;

#[tokio::test]
async fn a_continuous_goal_waits_for_explicit_events_and_stopped_goals_reject_wakes() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("first check"),
        text("event check"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Check the notes when requested".into())
        .await
        .unwrap();
    agent
        .configure_trigger(
            chat,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "Check the notes".into(),
                prompt_source: TriggerPromptSource::Explicit,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Idle
    );
    agent
        .trigger(
            chat,
            TriggerSource {
                source: "manual-test".into(),
                payload: serde_json::json!({"note":"changed"}),
            },
        )
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    agent.stop(chat).await.unwrap();
    assert!(
        agent
            .trigger(
                chat,
                TriggerSource {
                    source: "late".into(),
                    payload: serde_json::Value::Null
                }
            )
            .await
            .is_err()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn busy_triggers_coalesce_and_user_input_has_priority() {
    use async_trait::async_trait;
    use tickloop_agent_model::*;
    use tokio::sync::{Notify, mpsc::UnboundedSender};
    use tokio_util::sync::CancellationToken;
    struct Gated {
        gate: Arc<Notify>,
        calls: std::sync::atomic::AtomicUsize,
        model: Arc<ScriptedModel>,
    }
    #[async_trait]
    impl ModelClient for Gated {
        async fn generate_title(
            &self,
            request: tickloop_agent_model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
            Ok(tickloop_agent_test_support::title_output(request))
        }

        fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
            self.model.identity()
        }

        async fn complete(
            &self,
            request: ModelRequest,
            events: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.gate.notified().await;
            }
            self.model.complete(request, events, cancel).await
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("initial"),
        text("user first"),
        text("one triggered check"),
    ]));
    let gate = Arc::new(Notify::new());
    let agent = Agent::new_embedded(
        config(),
        Arc::new(Gated {
            gate: gate.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            model: model.clone(),
        }),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "ongoing goal".into()).await.unwrap();
    agent
        .configure_trigger(
            chat,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "event check".into(),
                prompt_source: TriggerPromptSource::Explicit,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    let mut trigger_run = None;
    for _ in 0..5 {
        let triggered = agent
            .trigger(
                chat,
                TriggerSource {
                    source: "file-observation".into(),
                    payload: serde_json::Value::Null,
                },
            )
            .await
            .unwrap();
        if let Some(id) = trigger_run {
            assert_eq!(triggered.run_id, id);
        } else {
            trigger_run = Some(triggered.run_id);
        }
    }
    agent
        .submit(chat, "Please prioritize this user input".into())
        .await
        .unwrap();
    assert_eq!(agent.snapshot(chat).await.unwrap().pending_inputs.len(), 2);
    gate.notify_one();
    let mut completed = 0;
    while completed < 3 {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            completed += 1;
        }
    }
    let requests = model.requests.lock().unwrap();
    assert!(format!("{:?}", requests[1].messages.last().unwrap()).contains("Please prioritize"));
    assert!(format!("{:?}", requests[2].messages.last().unwrap()).contains("event check"));
}

#[tokio::test(start_paused = true)]
async fn a_restored_chat_pauses_its_trigger_until_a_new_input_continues_it() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let agent = Agent::open_embedded(
        archive.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("ready")])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "watch the notes".into()).await.unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    agent
        .configure_trigger(
            chat,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "event check".into(),
                prompt_source: TriggerPromptSource::Explicit,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);
    let model = Arc::new(ScriptedModel::new(vec![
        text("resumed"),
        text("event checked"),
    ]));
    let restored = Agent::open_embedded(archive, config(), model.clone()).unwrap();
    restored.resume_chat(chat).await.unwrap();
    let trigger_armed = |snapshot: &tickloop_agent_protocol::ChatSnapshot| {
        snapshot.chat_loop.trigger.as_ref().unwrap().armed
    };
    assert!(!trigger_armed(&restored.snapshot(chat).await.unwrap()));
    let event = || TriggerSource {
        source: "manual".into(),
        payload: serde_json::json!({"note": "changed"}),
    };
    assert!(
        restored.trigger(chat, event()).await.is_err(),
        "a loaded Chat receives nothing until it is continued"
    );
    assert!(model.requests.lock().unwrap().is_empty());
    let mut events = restored.subscribe();
    restored
        .submit(chat, "Go on watching".into())
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert!(trigger_armed(&restored.snapshot(chat).await.unwrap()));
    restored.trigger(chat, event()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if matches!(event.kind, EventKind::Completed { .. }) {
            assert_eq!(event.loop_id, first.loop_id);
            break;
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    restored.shutdown().await;
}

/// Holds its first request until released, so the Loop stays running meanwhile.
struct HeldFirstTurn {
    script: ScriptedModel,
    entered: tokio::sync::Notify,
    release: tokio_util::sync::CancellationToken,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl tickloop_agent_model::ModelClient for HeldFirstTurn {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        self.script.identity()
    }

    async fn complete(
        &self,
        request: tickloop_agent_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            tokio::select! {
                _ = self.release.cancelled() => {}
                _ = cancel.cancelled() => return Err(tickloop_agent_model::ModelError::Cancelled),
            }
        }
        self.script.complete(request, events, cancel).await
    }
}

/// `/stop` keeps a Trigger input queued before it but takes none of it, and a configuration
/// change does not continue the Chat; the next user input does, and the queued input follows it.
#[tokio::test]
async fn stop_holds_queued_trigger_input_until_a_new_input_continues_the_chat() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(HeldFirstTurn {
        script: ScriptedModel::new(vec![text("going on"), text("event checked")]),
        entered: tokio::sync::Notify::new(),
        release: tokio_util::sync::CancellationToken::new(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .configure_trigger(
            chat,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "Check the notes".into(),
                prompt_source: TriggerPromptSource::Explicit,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Work on it".into()).await.unwrap();
    model.entered.notified().await;
    let queued = agent
        .trigger(
            chat,
            TriggerSource {
                source: "manual".into(),
                payload: serde_json::json!({"note": "changed"}),
            },
        )
        .await
        .unwrap();
    agent.stop(chat).await.unwrap();
    let paused = |snapshot: &tickloop_agent_protocol::ChatSnapshot| {
        snapshot.trigger_awaits_resume
            && !snapshot.chat_loop.trigger.as_ref().unwrap().armed
            && snapshot.pending_inputs.len() == 1
            && snapshot.pending_inputs[0].receipt.run_id == queued.run_id
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let stopped = agent.snapshot(chat).await.unwrap();
    assert_eq!(stopped.chat_loop.state, LoopState::Interrupted);
    assert!(paused(&stopped), "{:?}", stopped.pending_inputs);
    agent
        .set_execution_mode(chat, tickloop_agent_protocol::ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        paused(&agent.snapshot(chat).await.unwrap()),
        "a configuration change is not a continue"
    );
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    let resumed = agent.submit(chat, "Go on".into()).await.unwrap();
    let mut finished = Vec::new();
    while finished.len() < 2 {
        let event = next_event(&mut events).await;
        if matches!(event.kind, EventKind::Completed { .. }) {
            finished.push(event.run_id);
        }
    }
    assert_eq!(finished, [resumed.run_id, queued.run_id]);
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(!snapshot.trigger_awaits_resume);
    assert!(snapshot.chat_loop.trigger.as_ref().unwrap().armed);
    let delivered = {
        let requests = model.script.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        format!("{:?}", requests[1].messages.last())
    };
    assert!(delivered.contains("Check the notes"));
    agent.shutdown().await;
}
