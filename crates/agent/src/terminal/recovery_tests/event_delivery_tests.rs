use super::*;
use crate::runtime::Event;
use std::sync::atomic::{AtomicBool, Ordering};

struct ControlledDelivery {
    owner: EmbeddedAgentAccess,
    feeds: Mutex<HashMap<ExecutionRef, mpsc::Sender<ObservationUpdate>>>,
    fail_catalog: AtomicBool,
    fail_snapshot: AtomicBool,
    delayed_snapshot: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    replacement_snapshot: Mutex<Option<Box<LiveChatSnapshot>>>,
    delayed_compaction: Mutex<Option<DelayedCompaction>>,
}

struct DelayedCompaction {
    snapshot: Box<LiveChatSnapshot>,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[async_trait]
impl AgentAccess for ControlledDelivery {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if matches!(query, ObservationRequest::Overview { .. })
            && self.fail_catalog.load(Ordering::Relaxed)
            || matches!(
                query,
                ObservationRequest::Agent {
                    detail: AgentDetailQuery::Snapshot,
                    ..
                }
            ) && self.fail_snapshot.load(Ordering::Relaxed)
        {
            return Err(AccessError::unavailable(
                "controlled recovery query failure",
            ));
        }
        let execution = match &query {
            ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::Snapshot,
                ..
            } => Some(*execution),
            _ => None,
        };
        let mut observation = self.owner.observe(query).await?;
        if execution.is_some()
            && let Some(replacement) = self.replacement_snapshot.lock().unwrap().take()
        {
            observation.initial = ObservationValue::Agent(replacement);
        }
        let delay = execution.and_then(|_| self.delayed_snapshot.lock().unwrap().take());
        if let Some((entered, release)) = delay {
            entered.send(()).unwrap();
            release.await.unwrap();
        }
        if let Some(execution) = execution {
            let mut feeds = self.feeds.lock().unwrap();
            if feeds.get(&execution).is_none_or(mpsc::Sender::is_closed) {
                let (sender, receiver) = mpsc::channel(FEED_CAPACITY);
                feeds.insert(execution, sender);
                observation.changes = Some(receiver);
            }
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let delayed = matches!(
            command.command,
            AgentCommand::EditHistory {
                action: HistoryAction::Compact { .. },
                ..
            }
        )
        .then(|| self.delayed_compaction.lock().unwrap().take())
        .flatten();
        if let Some(delayed) = delayed {
            let execution = delayed.snapshot.execution;
            delayed.entered.send(()).unwrap();
            delayed.release.await.unwrap();
            return Ok(CommandReceipt {
                owner_epoch: command.owner_epoch,
                command_id: command.command_id,
                target: Some(execution),
                state: ReceiptState::Completed,
                result: Some(CommandResult::Compact {
                    summary: "controlled completed summary".into(),
                    snapshot: delayed.snapshot,
                }),
                error: None,
                persistence: PersistenceState::NotApplicable,
            });
        }
        self.owner.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.owner.show_view(update).await
    }
}

impl ControlledDelivery {
    async fn send(&self, execution: ExecutionRef, event: Event) {
        let sender = self.feeds.lock().unwrap()[&execution].clone();
        sender
            .send(ObservationUpdate {
                cursor: Cursor {
                    owner_epoch: self.owner.owner_epoch(),
                    execution: Some(execution),
                    sequence: event.sequence,
                },
                change: ObservationChange::Event(event),
            })
            .await
            .unwrap();
    }
}

async fn delivery_fixture() -> (
    runtime::Agent,
    Agent,
    Arc<ControlledDelivery>,
    mpsc::UnboundedReceiver<RecoveryFact>,
) {
    let core = runtime::Agent::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(vec![test_support::call(
            "retained-question",
            runtime::ASK_USER_QUESTION_TOOL_NAME,
            test_support::single_question("Which fixture?"),
        )])),
    );
    let access = Arc::new(ControlledDelivery {
        owner: EmbeddedAgentAccess::new(core.clone()),
        feeds: Mutex::new(HashMap::new()),
        fail_catalog: AtomicBool::new(false),
        fail_snapshot: AtomicBool::new(false),
        delayed_snapshot: Mutex::new(None),
        replacement_snapshot: Mutex::new(None),
        delayed_compaction: Mutex::new(None),
    });
    let mut launch = LaunchSpec::embedded(std::env::current_dir().unwrap());
    launch.model = Some(test_support::config());
    launch.session_persistence = false;
    let agent = Agent::from_access_for_test(
        access.clone(),
        access.owner.owner_epoch(),
        ViewRef {
            session_id: Uuid::new_v4(),
            view_id: Uuid::new_v4(),
        },
        launch,
        None,
    );
    let (probe, facts) = mpsc::unbounded_channel();
    agent.set_recovery_probe(probe);
    (core, agent, access, facts)
}

fn traffic(chat: Uuid, sequence: u64) -> Event {
    Event {
        chat_id: chat,
        sequence,
        loop_id: Uuid::nil(),
        run_id: Uuid::nil(),
        kind: EventKind::Thinking("bounded traffic".into()),
    }
}

#[tokio::test]
async fn failed_catalog_and_cached_snapshot_keep_retained_background_requests() {
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, access, mut facts) = delivery_fixture().await;
    let visible = agent.create_chat(directory.path().into()).await.unwrap();
    let background = agent.create_chat(directory.path().into()).await.unwrap();
    let visible_execution = agent.reference(visible).await.unwrap();
    let background_execution = agent.reference(background).await.unwrap();
    let mut events = agent.subscribe();
    for sequence in 1..=REPLAY_CAPACITY as u64 + 1 {
        access
            .send(visible_execution, traffic(visible, sequence))
            .await;
        next_fact(&mut facts, |fact| matches!(fact, RecoveryFact::Forwarded { chat, sequence: received } if *chat == visible && *received == sequence)).await;
    }
    let receipt = core
        .submit(background, None, "Ask a controlled question".into())
        .await
        .unwrap();
    let snapshot = tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let snapshot = core.snapshot(background).await.unwrap();
            if !snapshot.questions.is_empty() {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for event in &snapshot.events {
        access.send(background_execution, event.clone()).await;
        next_fact(&mut facts, |fact| matches!(fact, RecoveryFact::Forwarded { chat, sequence } if *chat == background && *sequence == event.sequence)).await;
    }
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    let mut recovery = agent.pause_event_delivery().await;
    access.fail_catalog.store(true, Ordering::Relaxed);
    access.fail_snapshot.store(true, Ordering::Relaxed);
    assert!(agent.list_chats().await.is_empty());
    let cached = agent.snapshot(background).await.unwrap();
    assert!(cached.last_sequence < snapshot.last_sequence);
    recovery.cover(background_execution, &cached);
    let mut retained = VecDeque::new();
    recovery.retain_uncovered(&mut events, &mut retained);
    drop(recovery);
    let mut pending = HashMap::new();
    let mut permissions = HashMap::new();
    let mut questions = HashMap::new();
    for event in &retained {
        track_run_requests(event, &mut pending, &mut permissions, &mut questions);
    }
    let retained_receipt = pending.get(&receipt.run_id).unwrap();
    assert_eq!(
        (
            retained_receipt.chat_id,
            retained_receipt.loop_id,
            retained_receipt.run_id
        ),
        (receipt.chat_id, receipt.loop_id, receipt.run_id)
    );
    assert!(questions.contains_key(&snapshot.questions[0].id));
    assert!(
        retained
            .iter()
            .any(|event| event.chat_id == background && matches!(event.kind, EventKind::Started))
    );
    agent.leave_view().await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn a_burst_queued_during_recovery_is_covered_once_and_newer_events_survive() {
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, access, mut facts) = delivery_fixture().await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let execution = agent.reference(chat).await.unwrap();
    let mut events = agent.subscribe();
    let mut recovery = agent.pause_event_delivery().await;
    let burst_end = REPLAY_CAPACITY as u64 + 1;
    let producer_access = access.clone();
    let producer = tokio::spawn(async move {
        for sequence in 1..=burst_end {
            producer_access
                .send(execution, traffic(chat, sequence))
                .await;
        }
    });
    tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            if access.feeds.lock().unwrap()[&execution].capacity() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!producer.is_finished());
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    let mut snapshot = agent.snapshot(chat).await.unwrap();
    snapshot.last_sequence = burst_end;
    recovery.cover(execution, &snapshot);
    recovery.retain_uncovered(&mut events, &mut VecDeque::new());
    drop(recovery);
    tokio::time::timeout(RECOVERY_TIMEOUT, producer)
        .await
        .unwrap()
        .unwrap();
    next_fact(
        &mut facts,
        |fact| matches!(fact, RecoveryFact::Skipped { sequence, .. } if *sequence == burst_end),
    )
    .await;
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    access.send(execution, traffic(chat, burst_end + 1)).await;
    let newer = tokio::time::timeout(RECOVERY_TIMEOUT, events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(newer.sequence, burst_end + 1);
    agent.leave_view().await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn a_delayed_reset_cannot_roll_back_the_snapshot_covered_by_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, access, _facts) = delivery_fixture().await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let execution = agent.reference(chat).await.unwrap();
    let initial = access
        .owner
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(older) = initial.initial else {
        panic!("expected initial snapshot");
    };
    core.submit(chat, None, "Ask a controlled question".into())
        .await
        .unwrap();
    let current = tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if !snapshot.questions.is_empty() {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut recovery = agent.pause_event_delivery().await;
    recovery.cover(execution, &current);
    let mut updates = agent.subscribe_facts();
    let sender = access.feeds.lock().unwrap()[&execution].clone();
    sender
        .send(ObservationUpdate {
            cursor: initial.cursor,
            change: ObservationChange::Reset {
                reason: ResetReason::Lagged,
                snapshot: older,
            },
        })
        .await
        .unwrap();
    tokio::time::timeout(RECOVERY_TIMEOUT, updates.recv())
        .await
        .unwrap()
        .unwrap();
    access.fail_snapshot.store(true, Ordering::Relaxed);
    let fallback = agent.snapshot(chat).await.unwrap();
    assert!(
        fallback.last_sequence >= current.last_sequence,
        "a stale reset rolled the cached cursor back below the covered prefix"
    );
    assert_eq!(fallback.questions[0].id, current.questions[0].id);
    drop(recovery);
    agent.leave_view().await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn delayed_changed_and_resync_queries_cannot_roll_back_recovered_requests() {
    for change in [
        ObservationChange::Changed,
        ObservationChange::Resync {
            reason: ResetReason::Lagged,
        },
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (core, agent, access, _facts) = delivery_fixture().await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let execution = agent.reference(chat).await.unwrap();
        let (entered, captured) = oneshot::channel();
        let (release, delayed) = oneshot::channel();
        *access.delayed_snapshot.lock().unwrap() = Some((entered, delayed));
        let mut updates = agent.subscribe_facts();
        let sender = access.feeds.lock().unwrap()[&execution].clone();
        sender
            .send(ObservationUpdate {
                cursor: Cursor {
                    owner_epoch: access.owner.owner_epoch(),
                    execution: Some(execution),
                    sequence: 0,
                },
                change,
            })
            .await
            .unwrap();
        tokio::time::timeout(RECOVERY_TIMEOUT, captured)
            .await
            .unwrap()
            .unwrap();
        core.submit(chat, None, "Ask a controlled question".into())
            .await
            .unwrap();
        let current = tokio::time::timeout(RECOVERY_TIMEOUT, async {
            loop {
                let snapshot = agent.snapshot(chat).await.unwrap();
                if !snapshot.questions.is_empty() {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut recovery = agent.pause_event_delivery().await;
        recovery.cover(execution, &current);
        release.send(()).unwrap();
        tokio::time::timeout(RECOVERY_TIMEOUT, updates.recv())
            .await
            .unwrap()
            .unwrap();
        access.fail_snapshot.store(true, Ordering::Relaxed);
        let fallback = agent.snapshot(chat).await.unwrap();
        assert!(
            fallback.last_sequence >= current.last_sequence,
            "a delayed query rolled the cached cursor back below the covered prefix"
        );
        assert_eq!(fallback.questions[0].id, current.questions[0].id);
        drop(recovery);
        agent.leave_view().await.unwrap();
        core.shutdown().await;
    }
}

#[tokio::test]
async fn replaced_execution_keeps_new_requests_when_recovery_catalog_fails() {
    replaced_execution_recovery(true).await;
}

#[tokio::test]
async fn reset_between_observation_and_cover_keeps_new_execution_requests() {
    replaced_execution_recovery(false).await;
}

async fn replaced_execution_recovery(cover_before_reset: bool) {
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, access, _facts) = delivery_fixture().await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let old_execution = agent.reference(chat).await.unwrap();
    let initial = access
        .owner
        .observe(ObservationRequest::Agent {
            execution: old_execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(mut replacement) = initial.initial else {
        panic!("expected initial snapshot");
    };
    let new_execution = ExecutionRef {
        generation: old_execution.generation + 1,
        ..old_execution
    };
    replacement.execution = new_execution;
    let mut observed = agent.observe(chat, 0).await.unwrap();
    observed.snapshot.last_sequence = 100;
    let mut recovery = agent.pause_event_delivery().await;
    if cover_before_reset {
        recovery.cover(old_execution, &observed.snapshot);
    }
    let mut updates = agent.subscribe_facts();
    let sender = access.feeds.lock().unwrap()[&old_execution].clone();
    sender
        .send(ObservationUpdate {
            cursor: Cursor {
                execution: Some(new_execution),
                sequence: 0,
                ..initial.cursor
            },
            change: ObservationChange::Reset {
                reason: ResetReason::ConversationChanged,
                snapshot: replacement,
            },
        })
        .await
        .unwrap();
    tokio::time::timeout(RECOVERY_TIMEOUT, updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(agent.reference(chat).await.unwrap(), new_execution);
    if !cover_before_reset {
        recovery.cover(observed.execution, &observed.snapshot);
    }
    let receipt = core
        .submit(chat, None, "Ask after execution replacement".into())
        .await
        .unwrap();
    let snapshot = tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let snapshot = core.snapshot(chat).await.unwrap();
            if !snapshot.questions.is_empty() {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    access.fail_catalog.store(true, Ordering::Relaxed);
    assert!(agent.list_chats().await.is_empty());
    let mut deferred = VecDeque::from(snapshot.events.clone());
    let mut receiver = agent.subscribe();
    recovery.retain_uncovered(&mut receiver, &mut deferred);
    assert!(
        deferred
            .iter()
            .any(|event| matches!(event.kind, EventKind::Started)),
        "an old execution prefix discarded the replacement's Started event"
    );
    let mut pending = HashMap::new();
    let mut permissions = HashMap::new();
    let mut questions = HashMap::new();
    for event in &deferred {
        track_run_requests(event, &mut pending, &mut permissions, &mut questions);
    }
    assert!(pending.contains_key(&receipt.run_id));
    assert!(questions.contains_key(&snapshot.questions[0].id));
    drop(recovery);
    agent.leave_view().await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn a_delayed_old_execution_query_cannot_replace_a_new_binding() {
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, access, _facts) = delivery_fixture().await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let old_execution = agent.reference(chat).await.unwrap();
    let initial = access
        .owner
        .observe(ObservationRequest::Agent {
            execution: old_execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(mut replacement) = initial.initial else {
        panic!("expected initial snapshot");
    };
    let new_execution = ExecutionRef {
        generation: old_execution.generation + 1,
        ..old_execution
    };
    replacement.execution = new_execution;
    let (entered, captured) = oneshot::channel();
    let (release, delayed) = oneshot::channel();
    *access.delayed_snapshot.lock().unwrap() = Some((entered, delayed));
    let mut updates = agent.subscribe_facts();
    let sender = access.feeds.lock().unwrap()[&old_execution].clone();
    sender
        .send(ObservationUpdate {
            cursor: initial.cursor,
            change: ObservationChange::Changed,
        })
        .await
        .unwrap();
    tokio::time::timeout(RECOVERY_TIMEOUT, captured)
        .await
        .unwrap()
        .unwrap();
    *access.replacement_snapshot.lock().unwrap() = Some(replacement);
    assert!(
        agent.live_snapshot(chat).await.is_err(),
        "the controlled new execution has no real owner watch"
    );
    assert_eq!(agent.reference(chat).await.unwrap(), new_execution);
    release.send(()).unwrap();
    tokio::time::timeout(RECOVERY_TIMEOUT, updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        agent.reference(chat).await.unwrap(),
        new_execution,
        "a stale query reverted the current execution binding"
    );
    agent.leave_view().await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn a_delayed_old_execution_compaction_cannot_replace_a_new_binding() {
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, access, _facts) = delivery_fixture().await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let old_execution = agent.reference(chat).await.unwrap();
    let initial = access
        .owner
        .observe(ObservationRequest::Agent {
            execution: old_execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(mut replacement) = initial.initial else {
        panic!("expected initial snapshot");
    };
    let (entered, captured) = oneshot::channel();
    let (release, delayed) = oneshot::channel();
    *access.delayed_compaction.lock().unwrap() = Some(DelayedCompaction {
        snapshot: replacement.clone(),
        entered,
        release: delayed,
    });
    let command_agent = agent.clone();
    let command =
        tokio::spawn(async move { command_agent.compact_with_instructions(chat, None).await });
    tokio::time::timeout(RECOVERY_TIMEOUT, captured)
        .await
        .unwrap()
        .unwrap();
    let new_execution = ExecutionRef {
        generation: old_execution.generation + 1,
        ..old_execution
    };
    replacement.execution = new_execution;
    *access.replacement_snapshot.lock().unwrap() = Some(replacement);
    assert!(agent.live_snapshot(chat).await.is_err());
    assert_eq!(agent.reference(chat).await.unwrap(), new_execution);
    release.send(()).unwrap();
    tokio::time::timeout(RECOVERY_TIMEOUT, command)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        agent.reference(chat).await.unwrap(),
        new_execution,
        "a stale history command reverted the current execution binding"
    );
    agent.leave_view().await.unwrap();
    core.shutdown().await;
}
