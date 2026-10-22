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
    let receipt = agent
        .submit(chat, None, "Check the notes when requested".into())
        .await
        .unwrap();
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
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
        agent.snapshot(chat).await.unwrap().loops[0].state,
        LoopState::WaitingTrigger
    );
    agent
        .trigger(
            chat,
            receipt.loop_id,
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
    agent.stop(chat, receipt.loop_id).await.unwrap();
    assert!(
        agent
            .trigger(
                chat,
                receipt.loop_id,
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
    let receipt = agent
        .submit(chat, None, "ongoing goal".into())
        .await
        .unwrap();
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
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
                receipt.loop_id,
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
        .submit(
            chat,
            Some(receipt.loop_id),
            "Please prioritize this user input".into(),
        )
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
async fn manually_resuming_a_restored_goal_rearms_only_its_trigger() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let agent = Agent::open_embedded(
        archive.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("first ready"),
            text("second ready"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "first watched goal".into())
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
    let second = agent
        .submit(chat, None, "second watched goal".into())
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
    for receipt in [&first, &second] {
        agent
            .configure_trigger(
                chat,
                receipt.loop_id,
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
    }
    agent.shutdown().await;
    drop(agent);
    let model = Arc::new(ScriptedModel::new(vec![
        text("first resumed"),
        text("first event check"),
    ]));
    let restored = Agent::open_embedded(archive.clone(), config(), model.clone()).unwrap();
    restored.resume_chat(chat).await.unwrap();
    assert!(
        restored
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .all(|goal| !goal.trigger.as_ref().unwrap().armed)
    );
    assert!(model.requests.lock().unwrap().is_empty());
    let mut events = restored.subscribe();
    restored
        .submit(chat, Some(first.loop_id), "Resume the first goal".into())
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
    let snapshot = restored.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .loops
            .iter()
            .find(|goal| goal.id == first.loop_id)
            .unwrap()
            .trigger
            .as_ref()
            .unwrap()
            .armed
    );
    assert!(
        !snapshot
            .loops
            .iter()
            .find(|goal| goal.id == second.loop_id)
            .unwrap()
            .trigger
            .as_ref()
            .unwrap()
            .armed
    );
    let event = || TriggerSource {
        source: "manual".into(),
        payload: serde_json::json!({"note": "changed"}),
    };
    assert!(
        restored
            .trigger(chat, second.loop_id, event())
            .await
            .is_err()
    );
    restored
        .trigger(chat, first.loop_id, event())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if matches!(event.kind, EventKind::Completed { .. }) {
            assert_eq!(event.loop_id, first.loop_id);
            break;
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    restored.shutdown().await;
    drop(restored);
    let reopened = Agent::open_embedded(
        archive,
        config(),
        Arc::new(ScriptedModel::new(vec![text("second resumed")])),
    )
    .unwrap();
    reopened.resume_chat(chat).await.unwrap();
    let mut events = reopened.subscribe();
    reopened
        .submit(chat, Some(second.loop_id), "Resume the second goal".into())
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
    assert!(
        reopened
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .find(|goal| goal.id == second.loop_id)
            .unwrap()
            .trigger
            .as_ref()
            .unwrap()
            .armed
    );
    reopened.shutdown().await;
}
