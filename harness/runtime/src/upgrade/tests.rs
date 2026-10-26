use super::*;
use std::time::Duration;
use tickloop_harness_protocol::CommandState;
use tickloop_harness_test_support::{ScriptedModel, call, config, text};

async fn complete(agent: &Agent, chat: ChatId, input: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, input.into()).await.unwrap();
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
    snapshot.chat_loop.receiving = tickloop_harness_protocol::Receiving::Paused {
        reason: tickloop_harness_protocol::PauseReason::Stopped,
    };
    snapshot.last_queue_sequence = 13;
    let sequence = snapshot
        .request_usage
        .start(
            tickloop_harness_protocol::RequestOwner {
                chat_id: chat,
                loop_id: None,
                run_id: None,
            },
            tickloop_harness_protocol::ModelRequestKind::Title,
            snapshot.model.identity(),
        )
        .unwrap();
    snapshot
        .request_usage
        .record_serialized_request_bytes(sequence, 1234)
        .unwrap();
    snapshot
        .request_usage
        .finish(
            sequence,
            tickloop_harness_protocol::RequestOutcome::Accepted,
        )
        .unwrap();
    snapshot.pending_inputs.push(PendingInput {
        receipt: Receipt {
            run_id: Uuid::new_v4().into(),
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
            tickloop_harness_protocol::FileReadRecord {
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
        tickloop_harness_protocol::CompactionMarker::Boundary(
            tickloop_harness_protocol::CompactBoundary {
                trigger: tickloop_harness_protocol::CompactTrigger::Manual,
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
    assert_eq!(actual.chat_loop.receiving, expected.chat_loop.receiving);
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
async fn planned_restore_keeps_receiving_and_session_permissions_without_model_calls() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("ready")]));
    let original = Agent::new_embedded(config(), model);
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let receipt = complete(&original, chat, "wait for later").await;
    let owner = PermissionOwner::new(chat.into(), receipt.loop_id.into()).unwrap();
    let operation = session_permissions::SessionPermissionOperation::from_normalized(
        "Read".into(),
        serde_json::json!({"path":"allowed"}),
        tickloop_harness_protocol::WorkspaceScope::new(
            Some(directory.path().canonicalize().unwrap()),
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
            tickloop_harness_tools::permissions::SessionPermissionDecision::Deny,
        )
        .unwrap();
    let read_path = directory.path().join("read-before-upgrade");
    std::fs::write(&read_path, "known contents").unwrap();
    original.inner.chats.lock().await[&chat]
        .tools
        .file_reads()
        .record(
            read_path.clone(),
            tickloop_harness_protocol::FileReadRecord {
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
    // A live handoff is no history load: the Loop keeps receiving.
    assert!(
        successor
            .snapshot(chat)
            .await
            .unwrap()
            .chat_loop
            .receiving
            .is_receiving()
    );
    assert!(successor_model.requests.lock().unwrap().is_empty());
    successor.shutdown().await;
}

#[tokio::test]
async fn parent_answer_does_not_allow_replacement_while_a_child_still_waits_for_input() {
    use tickloop_harness_test_support::{
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
    let receipt = original.submit(chat, "Delegate".into()).await.unwrap();
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
            vec![tickloop_harness_protocol::QuestionAnswer::other("Continue")],
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

#[derive(Clone)]
struct UpgradeStream {
    stream: crate::tick_stream::InMemoryTickStream,
    current: Arc<std::sync::Mutex<crate::tick_stream::TickObservation>>,
}

#[async_trait::async_trait]
impl crate::tick_stream::TickStreamPort for UpgradeStream {
    fn open(
        &self,
        tasks: &crate::tick_stream::StreamTasks,
    ) -> crate::tick_stream::TickStreamHandle {
        self.stream.open(tasks)
    }
    fn set_watch_authorities(
        &self,
        authorities: BTreeMap<
            crate::tick_stream::WatchId,
            tickloop_harness_protocol::HostToolAuthority,
        >,
    ) {
        self.stream.set_watch_authorities(authorities);
    }
    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &tick_protocol::InstanceRef,
        authority: Option<&tickloop_harness_protocol::HostToolAuthority>,
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
        kind: tickloop_harness_protocol::ObservationKind::Protocol,
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
    complete(&original, chat, "watch state").await;
    original
        .configure_subscriptions(
            chat,
            vec![tickloop_harness_protocol::TickSubscription {
                paused: false,
                id: Uuid::new_v4(),
                instance: instance.clone(),
                frequency: tickloop_harness_protocol::TickSubscriptionFrequency::High,
                authority: None,
            }],
        )
        .await
        .unwrap();
    let watch = *first_stream.stream.watches().keys().next().unwrap();
    first_stream.stream.emit(TickStreamEvent::Baseline {
        watch,
        sequence: 10,
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while original.tick_deliveries().upgrade_checkpoint()["baselines"][watch.uuid().to_string()]
            != 10
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
        let history = tickloop_harness_test_support::routed::history_text(&requests[0]);
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

struct LegacyRetainedShellHost {
    ended: tokio::sync::watch::Sender<Option<tickloop_harness_tools::ShellExit>>,
    retained_checks: std::sync::atomic::AtomicUsize,
    stops: Arc<std::sync::atomic::AtomicUsize>,
}

impl Default for LegacyRetainedShellHost {
    fn default() -> Self {
        Self {
            ended: tokio::sync::watch::channel(None).0,
            retained_checks: Default::default(),
            stops: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl tickloop_harness_tools::ShellHost for LegacyRetainedShellHost {
    async fn start(
        &self,
        mut launch: tickloop_harness_tools::ShellLaunch,
    ) -> Result<
        Box<dyn tickloop_harness_tools::ShellProcess>,
        tickloop_harness_tools::ShellStartError,
    > {
        use std::io::Write;
        launch.output.write_all(b"retained legacy output").unwrap();
        Ok(Box::new(LegacyRetainedShellProcess {
            ended: self.ended.subscribe(),
            stops: self.stops.clone(),
        }))
    }

    async fn retained_handoff(
        &self,
        _: Uuid,
    ) -> Result<Option<Value>, tickloop_harness_tools::ToolError> {
        self.retained_checks.fetch_add(1, Ordering::SeqCst);
        Err(tickloop_harness_tools::ToolError::Rejected(
            "the existing Tickhost cannot transfer this retained Shell".into(),
        ))
    }
}

struct LegacyRetainedShellProcess {
    ended: tokio::sync::watch::Receiver<Option<tickloop_harness_tools::ShellExit>>,
    stops: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl tickloop_harness_tools::ShellProcess for LegacyRetainedShellProcess {
    fn pid(&self) -> u32 {
        4242
    }

    async fn exited(&mut self) -> tickloop_harness_tools::ShellExit {
        self.ended
            .wait_for(Option::is_some)
            .await
            .unwrap()
            .clone()
            .unwrap()
    }

    async fn stop(&mut self, _: &str) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn an_unsupported_retained_shell_reports_a_blocker_without_discarding_its_result() {
    let directory = tempfile::tempdir().unwrap();
    const CONTINUE: &str = "continue using the original Agent";
    const CONTINUED: &str = "the original Agent answered the new input";
    let original = Agent::new_embedded(
        config(),
        Arc::new(tickloop_harness_test_support::routed::RoutedModel::new(
            |request| {
                use tickloop_harness_test_support::routed::{has_tool_result, history_text};
                if history_text(request).contains(CONTINUE) {
                    text(CONTINUED)
                } else if !has_tool_result(request, "legacy") {
                    call(
                        "legacy",
                        "Bash",
                        serde_json::json!({
                            "command":"legacy-task", "description":"Hold legacy output", "run_in_background":true,
                        }),
                    )
                } else {
                    text("waiting for the retained result")
                }
            },
        )),
    );
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let host = Arc::new(LegacyRetainedShellHost::default());
    {
        let mut chats = original.inner.chats.lock().await;
        let state = chats.get_mut(&chat).unwrap();
        let output = state.tools.chat_directory().unwrap().clone();
        let family = tickloop_harness_base::private_fs::PrivateDirectory::trusted(
            state.tools.family_temp_directory().unwrap().into(),
        );
        state.tools = Arc::new(tickloop_harness_tools::Tools::for_chat(
            tickloop_harness_tools::ChatDirectory {
                task_output: output.join("tasks"),
                chat: output,
                family_temp: family,
            },
            host.clone(),
        ));
    }
    original
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    complete(&original, chat, "start a legacy background task").await;
    host.ended
        .send_replace(Some(tickloop_harness_tools::ShellExit {
            exit_code: None,
            signal: None,
            reason: Some("cancelled".into()),
            state: CommandState::Failed,
        }));
    let failed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = original.snapshot(chat).await.unwrap();
            if snapshot
                .commands
                .first()
                .is_some_and(|command| command.state == CommandState::Failed)
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let prepared = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let prepared = original.prepare_upgrade().await;
            if host.retained_checks.load(Ordering::SeqCst) > 0 {
                break prepared;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let Err(error) = prepared else {
        panic!(
            "an unsupported retained Shell must report an upgrade blocker, not a busy safe point"
        );
    };
    assert!(
        error
            .to_string()
            .contains("cannot transfer this retained Shell")
    );
    assert!(!original.inner.upgrade_paused.load(Ordering::SeqCst));
    assert_eq!(host.stops.load(Ordering::SeqCst), 0);
    let current = original.snapshot(chat).await.unwrap();
    assert_eq!(current.monitor, failed.monitor);
    assert_eq!(current.commands[0].state, CommandState::Failed);
    assert_eq!(
        current.commands[0].output_ref,
        failed.commands[0].output_ref
    );
    assert_eq!(
        std::fs::read(&current.commands[0].output_ref).unwrap(),
        b"retained legacy output"
    );
    let mut events = original.subscribe();
    original.submit(chat, CONTINUE.into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id != chat { continue; }
            match event.kind {
                EventKind::Failed(error) => panic!("original Agent continuation failed: {error}"),
                EventKind::Completed { .. } => {
                    let current = original.snapshot(chat).await.unwrap();
                    if current.messages.iter().any(|message| message.role == Role::Assistant
                        && message.content.iter().any(|content| matches!(content, Content::Text { text } if text == CONTINUED))) {
                        break;
                    }
                }
                _ => {}
            }
        }
    }).await.expect("the original Agent must answer the new input after rejecting its upgrade");
    original.shutdown().await;
}

/// An idle Chat with a task ahead: once the successor takes over, its timer runs the task when
/// it comes due, with no new input.
#[tokio::test(flavor = "multi_thread")]
async fn the_successor_keeps_time_for_an_idle_chats_scheduled_task() {
    let directory = tempfile::tempdir().unwrap();
    let original = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![text("ready")])));
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    complete(&original, chat, "set up").await;
    let task = original
        .schedule_task(
            chat,
            crate::NewScheduledTask::new("*/10 * * * *", "check the build", true),
        )
        .await
        .unwrap();
    let checkpoint = prepare(&original).await;
    let successor_model = Arc::new(ScriptedModel::new(vec![text("checked")]));
    let successor = Agent::new_embedded(config(), successor_model.clone());
    successor.restore_upgrade(checkpoint).await.unwrap();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    let fire_at = i64::try_from(task.next_fire_at_ms.unwrap()).unwrap();
    successor
        .set_scheduled_now_for_tests(chrono::DateTime::from_timestamp_millis(fire_at).unwrap());
    successor.release_upgrade().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !successor_model
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| format!("{:?}", request.messages.last()).contains("check the build"))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the task ran without new input");
    successor.shutdown().await;
}

#[tokio::test]
async fn planned_upgrade_keeps_individual_task_and_subscription_pauses() {
    let directory = tempfile::tempdir().unwrap();
    let original = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    let task = original
        .schedule_task(
            chat,
            crate::NewScheduledTask::new("0 * * * *", "check build", true),
        )
        .await
        .unwrap();
    original
        .set_scheduled_task_paused(chat, task.id, true)
        .await
        .unwrap();
    let subscription = tickloop_harness_protocol::TickSubscription {
        id: Uuid::new_v4(),
        instance: "hosted:paused".parse().unwrap(),
        frequency: tickloop_harness_protocol::TickSubscriptionFrequency::Medium,
        paused: true,
        authority: None,
    };
    original
        .configure_subscriptions(chat, vec![subscription.clone()])
        .await
        .unwrap();
    let checkpoint = prepare(&original).await;
    let successor = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    successor
        .restore_upgrade(serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap())
        .await
        .unwrap();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    successor.release_upgrade().await.unwrap();
    let restored = successor.snapshot(chat).await.unwrap();
    assert_eq!(restored.chat_loop.subscriptions, vec![subscription]);
    assert!(restored.scheduled_tasks[0].paused_at_ms.is_some());
    assert_eq!(
        restored.scheduled_tasks[0].expires_at_ms,
        task.expires_at_ms
    );
    assert!(
        restored
            .scheduled_tasks
            .iter()
            .all(|task| !task.is_timing())
    );
    successor.shutdown().await;
}
