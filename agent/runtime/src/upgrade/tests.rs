use super::*;
use std::time::Duration;
use tickloop_agent_protocol::{CommandState, Trigger};
use tickloop_agent_test_support::{ScriptedModel, call, config, text};

async fn complete(agent: &Agent, chat: ChatId, input: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, input.into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    receipt
}

async fn prepare(agent: &Agent) -> AgentUpgradeCheckpoint {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(checkpoint) = agent.prepare_upgrade().await.unwrap() {
                break checkpoint;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn private_checkpoint_roundtrip_retains_fields_that_history_deliberately_omits() {
    let directory = tempfile::tempdir().unwrap();
    let original = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![text("ready")])));
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let receipt = complete(&original, chat, "ready").await;
    let mut checkpoint = prepare(&original).await;
    let snapshot = &mut checkpoint.chats[0].snapshot;
    snapshot.policy.mode = ExecutionMode::FullAccess;
    snapshot.thinking_enabled = false;
    snapshot.context_window_tokens = Some(4096);
    snapshot.resume_triggers.push(receipt.loop_id);
    snapshot.last_queue_sequence = 13;
    let sequence = snapshot
        .request_usage
        .start(
            tickloop_agent_protocol::RequestOwner {
                chat_id: chat,
                loop_id: None,
                run_id: None,
            },
            tickloop_agent_protocol::ModelRequestKind::Title,
            snapshot.model.identity(),
        )
        .unwrap();
    snapshot
        .request_usage
        .record_serialized_request_bytes(sequence, 1234)
        .unwrap();
    snapshot
        .request_usage
        .finish(sequence, tickloop_agent_protocol::RequestOutcome::Accepted)
        .unwrap();
    snapshot.pending_inputs.push(PendingInput {
        receipt: Receipt {
            run_id: Uuid::new_v4(),
            ..receipt.clone()
        },
        input: "queued skill".into(),
        contexts: Vec::new(),
        agent_message: None,
        directed_human: None,
        denial_reference: None,
        trigger_source: None,
        armed: true,
        queued_sequence: 13,
        task_continuation: false,
        is_skill_invocation: true,
        attached_file_reads: vec![(
            directory.path().join("attached.txt"),
            tickloop_agent_protocol::FileReadRecord {
                content: "approved attachment".into(),
                modified: std::time::SystemTime::now(),
                offset: 1,
                limit: None,
                is_full_read: true,
            },
        )],
    });
    snapshot.operations.push(OperationRecord {
        id: Uuid::new_v4(),
        loop_id: receipt.loop_id,
        run_id: receipt.run_id,
        call_id: "already-dispatched".into(),
        name: "Write".into(),
        input: serde_json::json!({"path":"result"}),
        file_change: None,
        state: OperationState::Unknown,
        result: None,
        task_binding: None,
    });
    snapshot.message_compactions.insert(
        0,
        tickloop_agent_protocol::CompactionMarker::Boundary(
            tickloop_agent_protocol::CompactBoundary {
                trigger: tickloop_agent_protocol::CompactTrigger::Manual,
                pre_tokens: 100,
                user_context: None,
                messages_summarized: Some(1),
                preserved_by_later_summary: false,
                original_user_inputs: None,
                model: Some(Box::new(snapshot.model.clone())),
            },
        ),
    );
    let expected = snapshot.clone();
    let decoded: AgentUpgradeCheckpoint =
        serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    let actual = &decoded.chats[0].snapshot;
    assert_eq!(actual.model, expected.model);
    assert_eq!(actual.policy.mode, ExecutionMode::FullAccess);
    assert_eq!(actual.request_usage, expected.request_usage);
    assert_eq!(
        actual
            .request_usage
            .entries()
            .last()
            .unwrap()
            .serialized_request_bytes,
        Some(1234)
    );
    assert!(!actual.thinking_enabled);
    assert_eq!(actual.context_window_tokens, Some(4096));
    assert_eq!(actual.resume_triggers, vec![receipt.loop_id]);
    assert_eq!(actual.last_queue_sequence, 13);
    assert_eq!(actual.last_sequence, expected.last_sequence);
    assert_eq!(actual.message_compactions, expected.message_compactions);
    assert_eq!(actual.operations[0].state, OperationState::Unknown);
    assert!(actual.pending_inputs[0].is_skill_invocation);
    assert_eq!(
        actual.pending_inputs[0].attached_file_reads[0].1.content,
        "approved attachment"
    );
    assert_eq!(
        serde_json::to_value(&actual.pending_inputs[0].receipt).unwrap(),
        serde_json::to_value(&expected.pending_inputs[0].receipt).unwrap()
    );
    original.cancel_upgrade().await;
    original.shutdown().await;
}

#[tokio::test]
async fn planned_restore_preserves_armed_timer_and_session_permissions_without_model_calls() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("ready")]));
    let original = Agent::new_embedded(config(), model);
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let receipt = complete(&original, chat, "wait for later").await;
    original
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "check later".into(),
                prompt_source: Default::default(),
                interval_seconds: Some(3600),
                accepts_events: false,
                expires_at_ms: None,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    let owner = PermissionOwner::new(chat, receipt.loop_id).unwrap();
    let operation = session_permissions::SessionPermissionOperation::from_normalized(
        "Read".into(),
        serde_json::json!({"path":"allowed"}),
        tickloop_agent_protocol::WorkspaceScope::new(
            directory.path().canonicalize().unwrap(),
            Vec::new(),
        ),
    )
    .unwrap();
    original
        .inner
        .session_permissions
        .lock()
        .await
        .record(
            owner,
            operation,
            tickloop_agent_tools::permissions::SessionPermissionDecision::Deny,
        )
        .unwrap();
    let read_path = directory.path().join("read-before-upgrade");
    std::fs::write(&read_path, "known contents").unwrap();
    original.inner.chats.lock().await[&chat]
        .tools
        .file_reads()
        .record(
            read_path.clone(),
            tickloop_agent_protocol::FileReadRecord {
                content: "known contents".into(),
                modified: std::fs::metadata(&read_path).unwrap().modified().unwrap(),
                offset: 1,
                limit: None,
                is_full_read: true,
            },
        );
    let checkpoint = prepare(&original).await;
    let encoded = serde_json::to_vec(&checkpoint).unwrap();
    let expected = serde_json::to_value(&checkpoint.chats[0].snapshot).unwrap();
    let successor_model = Arc::new(ScriptedModel::new(Vec::new()));
    let successor = Agent::new_embedded(config(), successor_model.clone());
    successor
        .restore_upgrade(serde_json::from_slice(&encoded).unwrap())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(successor.snapshot(chat).await.unwrap()).unwrap(),
        expected
    );
    assert_eq!(
        successor
            .inner
            .session_permissions
            .lock()
            .await
            .freeze_denials(owner)
            .len(),
        1
    );
    assert_eq!(
        successor.inner.chats.lock().await[&chat]
            .tools
            .file_reads()
            .get(&read_path)
            .unwrap()
            .content,
        "known contents"
    );
    successor.activate_upgrade().await.unwrap();
    assert!(successor.inner.upgrade_paused.load(Ordering::SeqCst));
    assert!(successor_model.requests.lock().unwrap().is_empty());
    original.shutdown_for_upgrade().await;
    successor.release_upgrade().await.unwrap();
    assert!(
        successor.snapshot(chat).await.unwrap().loops[0]
            .trigger
            .as_ref()
            .unwrap()
            .armed
    );
    assert!(successor_model.requests.lock().unwrap().is_empty());
    successor.shutdown().await;
}

async fn check_shell_transfer(exit_during_transfer: bool) {
    let directory = tempfile::tempdir().unwrap();
    let finish = directory.path().join("finish");
    let command = format!(
        "while [ ! -f '{}' ]; do sleep 0.05; done; printf transferred",
        finish.display()
    );
    let original = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "background",
                "Bash",
                serde_json::json!({"command":command,"run_in_background":true}),
            ),
            text("running"),
        ])),
    );
    let grants = Arc::new(crate::hosted::shells::OwnTickhost::new());
    original.install_shell_grants(grants.clone());
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    complete(&original, chat, "run background work").await;
    let before = original.commands(chat).await.unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].state, CommandState::Background);
    let checkpoint = prepare(&original).await;
    if exit_during_transfer {
        std::fs::write(&finish, "finish").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while original.commands(chat).await.unwrap()[0].state != CommandState::Completed {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    let successor = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    successor.install_shell_grants(grants);
    successor.restore_upgrade(checkpoint).await.unwrap();
    let mut notifications = successor.subscribe_task_notifications();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    successor.release_upgrade().await.unwrap();
    let after = successor.commands(chat).await.unwrap();
    assert_eq!(after[0].pid, before[0].pid);
    assert_eq!(after[0].operation_id, before[0].operation_id);
    if !exit_during_transfer {
        std::fs::write(finish, "finish").unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = successor.commands(chat).await.unwrap();
            if current[0].state == CommandState::Completed {
                assert_eq!(
                    std::fs::read_to_string(&current[0].output_ref).unwrap(),
                    "transferred"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(notifications.try_recv().is_err());
    successor.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_shell_keeps_pid_output_and_stop_authority_across_worker_restore() {
    check_shell_transfer(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_exit_during_transfer_delivers_its_original_output_and_one_notification() {
    check_shell_transfer(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claimed_task_wait_survives_upgrade_and_finishes_in_the_same_run() {
    use tickloop_agent_test_support::routed::{
        RoutedModel, has_tool_result, history_text, tool_result,
    };
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(RoutedModel::new(|request| {
        if !has_tool_result(request, "create") {
            let mut calls = call(
                "create",
                "TaskCreate",
                serde_json::json!({
                    "mutation_id":Uuid::from_u128(901),"title":"Wait for output","description":"Complete when the Shell exits",
                }),
            );
            calls.extend(call(
                "background",
                "Bash",
                serde_json::json!({
                    "command":"while [ ! -f finish ]; do sleep 0.05; done; printf done",
                    "description":"Wait for finish marker", "run_in_background":true,
                }),
            ));
            return calls;
        }
        let task = tool_result(&request.messages, "create").unwrap()["task"]["id"].clone();
        if !has_tool_result(request, "claim") {
            return call(
                "claim",
                "TaskUpdate",
                serde_json::json!({
                    "mutation_id":Uuid::from_u128(902),"task_id":task,"expected_revision":1,"action":"claim",
                }),
            );
        }
        if !history_text(request).contains("<tool-use-id>background</tool-use-id>") {
            return text("Waiting for the background command");
        }
        if !has_tool_result(request, "complete") {
            return call(
                "complete",
                "TaskUpdate",
                serde_json::json!({
                    "mutation_id":Uuid::from_u128(903),"task_id":task,"expected_revision":2,"action":"complete","result":"Command completed",
                }),
            );
        }
        text("Task finished")
    }));
    let original = Agent::new_embedded(config(), model.clone());
    let grants = Arc::new(crate::hosted::shells::OwnTickhost::new());
    original.install_shell_grants(grants.clone());
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original.set_task_tools_enabled(chat, true).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = original.subscribe();
    let receipt = original
        .submit(chat, None, "Track background work".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::Text(ref text) if text == "Waiting for the background command") {
                break;
            }
            if let EventKind::Failed(error) = event.kind { panic!("{error}"); }
        }
    }).await.unwrap();
    let checkpoint = prepare(&original).await;
    assert_eq!(
        checkpoint.chats[0]
            .waiting
            .as_ref()
            .map(|waiting| waiting.run_id),
        Some(receipt.run_id)
    );
    let requests = model.requests.lock().unwrap().len();
    let successor = Agent::new_embedded(config(), model.clone());
    successor.install_shell_grants(grants);
    successor
        .restore_upgrade(serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap())
        .await
        .unwrap();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    let mut events = successor.subscribe();
    successor.release_upgrade().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(model.requests.lock().unwrap().len(), requests);
    std::fs::write(directory.path().join("finish"), "go").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
            if let EventKind::Failed(error) = event.kind {
                panic!("{error}");
            }
        }
    })
    .await
    .unwrap();
    let snapshot = successor.snapshot(chat).await.unwrap();
    let task = &snapshot.task_boards[&receipt.loop_id].tasks[0];
    assert_eq!(
        task.status,
        tickloop_agent_protocol::SharedTaskStatus::Completed
    );
    assert_eq!(task.claim.as_ref().unwrap().run_id, receipt.run_id);
    successor.shutdown().await;
}

#[tokio::test]
async fn parent_answer_does_not_allow_replacement_while_a_child_still_waits_for_input() {
    use tickloop_agent_test_support::{
        routed::{RoutedModel, has_tool_result, state},
        single_question,
    };
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(RoutedModel::new(|request| {
        if state(request)["delegation"].is_null() {
            if !has_tool_result(request, "child") {
                return call(
                    "child",
                    "Subagent",
                    serde_json::json!({"name":"helper","task":"wait","run_in_background":true}),
                );
            }
            return text("Parent answer finished");
        }
        if !has_tool_result(request, "question") {
            return call("question", "AskUserQuestion", single_question("Continue?"));
        }
        if !has_tool_result(request, "handback") {
            return call(
                "handback",
                "SubagentHandback",
                serde_json::json!({"message":"Child completed"}),
            );
        }
        text("Child completed")
    }));
    let original = Agent::new_embedded(config(), model);
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = original.subscribe();
    let receipt = original
        .submit(chat, None, "Delegate".into())
        .await
        .unwrap();
    let (child, question) = tokio::time::timeout(Duration::from_secs(10), async {
        let mut parent_finished = false;
        let mut question = None;
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                parent_finished = true;
            }
            if let EventKind::QuestionAsked { request } = event.kind {
                question = Some((event.chat_id, request.id));
            }
            if parent_finished && let Some(question) = question {
                break question;
            }
        }
    })
    .await
    .unwrap();
    assert!(original.prepare_upgrade().await.unwrap().is_none());
    original
        .answer_question(
            child,
            question,
            vec![tickloop_agent_protocol::QuestionAnswer::other("Continue")],
        )
        .await
        .unwrap();
    let checkpoint = prepare(&original).await;
    assert_eq!(checkpoint.chats.len(), 2);
    assert!(
        checkpoint
            .chats
            .iter()
            .any(|saved| saved.snapshot.id == child)
    );
    original.cancel_upgrade().await;
    original.shutdown().await;
}

#[tokio::test]
async fn timer_due_while_prepared_waits_for_successor_and_input_cannot_dispatch_on_old_owner() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("ready")]));
    let original = Agent::new_embedded(config(), model.clone());
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let receipt = complete(&original, chat, "check periodically").await;
    original
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "scheduled check".into(),
                prompt_source: Default::default(),
                interval_seconds: Some(1),
                expires_at_ms: None,
                accepts_events: false,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    let checkpoint = prepare(&original).await;
    let wire = serde_json::to_vec(&checkpoint).unwrap();
    assert!(
        original
            .submit(chat, None, "queued during transfer".into())
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let successor_model = Arc::new(ScriptedModel::new(vec![text("checked"), text("received")]));
    let successor = Agent::new_embedded(config(), successor_model.clone());
    successor
        .restore_upgrade(serde_json::from_slice(&wire).unwrap())
        .await
        .unwrap();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    let mut events = successor.subscribe();
    successor.release_upgrade().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            events.recv().await.unwrap().kind,
            EventKind::Completed { .. }
        ) {}
    })
    .await
    .unwrap();
    successor
        .configure_trigger(chat, receipt.loop_id, None)
        .await
        .unwrap();
    assert_eq!(successor_model.requests.lock().unwrap().len(), 1);
    complete(&successor, chat, "queued during transfer").await;
    assert_eq!(successor_model.requests.lock().unwrap().len(), 2);
    successor.shutdown().await;
}

#[derive(Clone)]
struct UpgradeStream {
    stream: crate::tick_stream::InMemoryTickStream,
    current: Arc<std::sync::Mutex<crate::tick_stream::TickObservation>>,
}

#[async_trait::async_trait]
impl crate::tick_stream::TickStreamPort for UpgradeStream {
    fn open(&self) -> crate::tick_stream::TickStreamHandle {
        self.stream.open()
    }
    fn set_watch_authorities(
        &self,
        authorities: BTreeMap<Uuid, tickloop_agent_protocol::HostToolAuthority>,
    ) {
        self.stream.set_watch_authorities(authorities);
    }
    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &tick_protocol::InstanceRef,
        authority: Option<&tickloop_agent_protocol::HostToolAuthority>,
    ) -> Result<(), String> {
        self.stream
            .revalidate_watch(chat, instance, authority)
            .await
    }
    async fn read_current(
        &self,
        _: Uuid,
        _: &tick_protocol::InstanceRef,
    ) -> Result<Option<crate::tick_stream::TickObservation>, String> {
        Ok(Some(self.current.lock().unwrap().clone()))
    }
}

#[tokio::test(start_paused = true)]
async fn watch_reconnect_delivers_one_latest_update_since_the_original_baseline() {
    use crate::tick_stream::{InMemoryTickStream, TickObservation, TickStreamEvent};
    let directory = tempfile::tempdir().unwrap();
    let instance = tick_protocol::InstanceRef::new("calendar", "upgrade-baseline").unwrap();
    let observation = |sequence| TickObservation {
        instance: instance.clone(),
        sequence,
        observed_at: "2026-10-19T10:00:00Z".into(),
        dropped: 0,
        data: serde_json::json!({"upgrade_value":sequence}),
        kind: tickloop_agent_protocol::ObservationKind::Protocol,
        recover_with: None,
        gap: false,
    };
    let current = Arc::new(std::sync::Mutex::new(observation(10)));
    let first_stream = UpgradeStream {
        stream: InMemoryTickStream::new(),
        current: current.clone(),
    };
    let model = Arc::new(ScriptedModel::new(vec![text("watching")]));
    let original = Agent::new_embedded(config(), model.clone());
    original
        .install_tick_stream(Arc::new(first_stream.clone()))
        .await;
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let receipt = complete(&original, chat, "watch state").await;
    let mut trigger = Trigger::for_loop_goal("watch state");
    trigger
        .subscriptions
        .push(tickloop_agent_protocol::TickSubscription {
            id: Uuid::new_v4(),
            instance: instance.clone(),
            frequency: tickloop_agent_protocol::TickSubscriptionFrequency::High,
            authority: None,
        });
    original
        .configure_trigger(chat, receipt.loop_id, Some(trigger))
        .await
        .unwrap();
    let watch = *first_stream.stream.watches().keys().next().unwrap();
    first_stream.stream.emit(TickStreamEvent::Baseline {
        watch,
        sequence: 10,
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while original.tick_deliveries().upgrade_checkpoint()["baselines"][watch.to_string()] != 10
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let checkpoint = prepare(&original).await;
    first_stream.stream.push(observation(11));
    *current.lock().unwrap() = observation(12);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let second_stream = UpgradeStream {
        stream: InMemoryTickStream::new(),
        current,
    };
    let successor_model = Arc::new(ScriptedModel::new(vec![text("latest state received")]));
    let successor = Agent::new_embedded(config(), successor_model.clone());
    successor
        .install_tick_stream(Arc::new(second_stream.clone()))
        .await;
    successor
        .restore_upgrade(serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap())
        .await
        .unwrap();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    let mut events = successor.subscribe();
    successor.release_upgrade().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while successor.tick_content_bytes() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        second_stream.stream.watches(),
        first_stream.stream.watches()
    );
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            events.recv().await.unwrap().kind,
            EventKind::Completed { .. }
        ) {}
    })
    .await
    .unwrap();
    {
        let requests = successor_model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let history = tickloop_agent_test_support::routed::history_text(&requests[0]);
        assert!(history.contains("upgrade_value"));
        assert!(history.contains("12"));
    }
    second_stream.stream.push(observation(12));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(61)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(successor_model.requests.lock().unwrap().len(), 1);
    successor.shutdown().await;
}

#[tokio::test]
async fn an_interrupted_waiting_run_is_not_checkpointed_as_fresh_execution() {
    let directory = tempfile::tempdir().unwrap();
    let original = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![text("done")])));
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let receipt = complete(&original, chat, "work").await;
    {
        let mut chats = original.inner.chats.lock().await;
        let state = chats.get_mut(&chat).unwrap();
        state.active = Some(CancellationToken::new());
        state.active_run = Some(receipt.run_id);
        state.upgrade_wait = Some(receipt);
    }
    original.interrupt(chat).await.unwrap();
    assert!(original.prepare_upgrade().await.unwrap().is_none());
    original.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_completed_shell_keeps_its_host_view_after_upgrade_and_idle_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let original = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "background",
                "Bash",
                serde_json::json!({
                    "command":"while [ ! -f finish ]; do sleep 0.01; done; printf retained",
                    "description":"Wait for marker", "run_in_background":true,
                }),
            ),
            text("running"),
        ])),
    );
    let grants = Arc::new(crate::hosted::shells::OwnTickhost::with_idle_timeout(
        Duration::from_millis(20),
    ));
    original.install_shell_grants(grants.clone());
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    complete(&original, chat, "run background work").await;
    std::fs::write(directory.path().join("finish"), "finish").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if original
                .snapshot(chat)
                .await
                .unwrap()
                .commands
                .first()
                .is_some_and(|command| command.state == CommandState::Completed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let checkpoint = prepare(&original).await;
    let task = checkpoint.chats[0].snapshot.commands[0].operation_id;
    let successor = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    successor.install_shell_grants(grants);
    successor.restore_upgrade(checkpoint).await.unwrap();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    successor.release_upgrade().await.unwrap();
    tokio::time::sleep(Duration::from_millis(650)).await;
    let instance = tick_protocol::InstanceRef::shell(task);
    assert!(
        successor
            .inner
            .shells
            .allow_view(chat, &instance)
            .await
            .is_ok()
    );
    successor.shutdown().await;
}
