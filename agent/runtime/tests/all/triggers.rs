use std::sync::Arc;
use tickloop_agent_protocol::{
    Decision, EventKind, LoopState, PermissionRule, QuestionAnswer, Trigger, TriggerSource,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::*;
use tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME;
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
                interval_seconds: None,
                expires_at_ms: None,
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
                prompt: "scheduled check".into(),
                interval_seconds: None,
                expires_at_ms: None,
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
    assert!(format!("{:?}", requests[2].messages.last().unwrap()).contains("scheduled check"));
}

#[tokio::test(start_paused = true)]
async fn an_interval_wakes_the_same_goal_and_shutdown_cancels_future_ticks() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("first check"),
        text("timed check"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "periodic check".into())
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
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "timed check".into(),
                interval_seconds: Some(10),
                expires_at_ms: None,
                accepts_events: false,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert_eq!(agent.snapshot(chat).await.unwrap().loops.len(), 1);
    agent.shutdown().await;
    tokio::time::advance(std::time::Duration::from_secs(100)).await;
    assert_eq!(model.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_event_only_goal_expires_without_waiting_for_another_event() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![text("ready to wait")])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "watch for events until the deadline".into())
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "check".into(),
                interval_seconds: None,
                expires_at_ms: Some(now + 50),
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(90)).await;
    assert_eq!(
        agent.snapshot(chat).await.unwrap().loops[0].state,
        LoopState::Expired
    );
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
        .submit(chat, None, "first periodic goal".into())
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
        .submit(chat, None, "second periodic goal".into())
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
                    prompt: "periodic check".into(),
                    interval_seconds: Some(10),
                    expires_at_ms: None,
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
        text("first timed check"),
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
    tokio::time::advance(std::time::Duration::from_secs(20)).await;
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
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
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

#[tokio::test(start_paused = true)]
async fn replacing_a_trigger_cancels_its_old_interval_and_deadline_even_with_the_same_id() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("ready"),
        text("new schedule ran"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "scheduled work".into())
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
    let id = Uuid::new_v4();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id,
                prompt: "old schedule".into(),
                interval_seconds: Some(10),
                expires_at_ms: Some(now + 20_000),
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id,
                prompt: "new schedule".into(),
                interval_seconds: Some(30),
                expires_at_ms: None,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(20)).await;
    tokio::task::yield_now().await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(
        agent.snapshot(chat).await.unwrap().loops[0].state,
        LoopState::WaitingTrigger
    );
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(format!("{:?}", requests[1].messages.last().unwrap()).contains("new schedule"));
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn expiration_clears_the_selected_goals_waiters_and_preserves_other_queued_work() {
    for question in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let call = if question {
            call(
                "waiting",
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which report?"),
            )
        } else {
            call(
                "waiting",
                "Write",
                serde_json::json!({"file_path":"report.txt","content":"pending approval"}),
            )
        };
        let model = Arc::new(ScriptedModel::new(vec![call, text("other goal completed")]));
        let agent = Agent::new_embedded(config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision: Decision::Ask,
                    tool: "Write".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, None, "first goal waits".into())
            .await
            .unwrap();
        let waiting = loop {
            match next_event(&mut events).await.kind {
                EventKind::QuestionAsked { request } => break request.id,
                EventKind::PermissionRequested { request } => break request.id,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        };
        agent
            .submit(
                chat,
                Some(receipt.loop_id),
                "discard this queued continuation on expiry".into(),
            )
            .await
            .unwrap();
        let other = agent
            .submit(chat, None, "independent second goal".into())
            .await
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        agent
            .configure_trigger(
                chat,
                receipt.loop_id,
                Some(Trigger {
                    id: Uuid::new_v4(),
                    prompt: "check".into(),
                    interval_seconds: None,
                    expires_at_ms: Some(now + 1000),
                    accepts_events: true,
                    armed: true,
                    subscriptions: Vec::new(),
                }),
            )
            .await
            .unwrap();
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let mut ended = false;
        let mut other_completed = false;
        while !ended || !other_completed {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::GoalEnded { reason, .. } => {
                    assert_eq!(event.loop_id, receipt.loop_id);
                    assert_eq!(reason, "deadline reached");
                    ended = true;
                }
                EventKind::Completed { .. } if event.loop_id == other.loop_id => {
                    other_completed = true
                }
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(ended);
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            snapshot
                .loops
                .iter()
                .find(|goal| goal.id == receipt.loop_id)
                .unwrap()
                .state,
            LoopState::Expired
        );
        assert_eq!(
            snapshot
                .loops
                .iter()
                .find(|goal| goal.id == other.loop_id)
                .unwrap()
                .state,
            LoopState::Completed
        );
        assert!(
            snapshot.questions.is_empty()
                && snapshot.permissions.is_empty()
                && snapshot.pending_inputs.is_empty()
        );
        assert!(
            agent
                .answer_question(chat, waiting, vec![QuestionAnswer::option("Weekly")])
                .await
                .is_err()
        );
        assert!(agent.answer_permission(chat, waiting, true).await.is_err());
        assert!(
            agent
                .submit(chat, Some(receipt.loop_id), "reopen expired goal".into())
                .await
                .is_err()
        );
        assert!(!directory.path().join("report.txt").exists());
        assert_eq!(model.requests.lock().unwrap().len(), 2);
        agent.stop(chat, other.loop_id).await.unwrap();
        let event = loop {
            let event = next_event(&mut events).await;
            if matches!(event.kind, EventKind::GoalEnded { .. }) && event.loop_id == other.loop_id {
                break event;
            }
        };
        assert_eq!(event.loop_id, other.loop_id);
        assert!(matches!(event.kind,EventKind::GoalEnded{reason,..} if reason=="stopped by user"));
        agent.shutdown().await;
    }
}
