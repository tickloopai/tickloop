use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const ACTIVITY_FIXTURE: &str = "TICKLOOP_WORKING_ACTIVITY_FIXTURE";
const ACTIVITY_ROUTE: &str = "TICKLOOP_WORKING_ACTIVITY_ROUTE";
const ACTIVITY_STAGE: &str = "activity-stage";
const ACTIVITY_QUERIES: &str = "activity-queries";
const ACTIVITY_PHASE: &str = "activity-phase.json";
const ACTIVITY_EXECUTIONS: &str = "activity-executions.json";
const ACTIVITY_CREATED: &str = "activity-created.json";
const ACTIVITY_OBSERVED_START: &str = "activity-observed-start.json";
const ACTIVITY_LOST_END: &str = "activity-lost-end.json";
/// A route whose Agent feeds lose every Completed event to a lagged Reset, while an armed
/// trigger keeps the Loop waiting after its run ends.
const LOST_END_ROUTE: &str = "lost-end";

struct ActivityModel(AtomicUsize);

#[async_trait]
impl ModelClient for ActivityModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "working-fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "working-fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let request = self.0.fetch_add(1, Ordering::Relaxed) + 1;
        std::fs::write(
            ACTIVITY_PHASE,
            serde_json::json!({"phase":format!("running-{request}")}).to_string(),
        )
        .unwrap();
        while !std::fs::read_to_string(ACTIVITY_STAGE)
            .is_ok_and(|stage| stage == format!("finish-{request}"))
        {
            tokio::select! {
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
            }
        }
        events
            .send(ModelDelta::Text(format!("Activity reply {request}")))
            .unwrap();
        Ok(ModelOutput {
            private_source: None,
            content: tickloop_agent_test_support::text(&format!("Activity reply {request}")),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

struct ActivityAccess {
    daemon: EmbeddedAgentAccess,
    queries: AtomicUsize,
}

#[async_trait]
impl AgentAccess for ActivityAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if matches!(
            &query,
            ObservationRequest::Agent {
                detail: AgentDetailQuery::Tasks,
                ..
            }
        ) {
            let count = self.queries.fetch_add(1, Ordering::Relaxed) + 1;
            std::fs::write(ACTIVITY_QUERIES, count.to_string()).unwrap();
        }
        let lossy_feed = match &query {
            ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::Snapshot | AgentDetailQuery::Status,
                ..
            } if std::env::var(ACTIVITY_ROUTE).as_deref() == Ok(LOST_END_ROUTE) => Some(*execution),
            _ => None,
        };
        let mut observation = self.daemon.observe(query).await?;
        if std::env::var(ACTIVITY_ROUTE).as_deref() == Ok("observe-only")
            && let ObservationValue::Overview(rows) = &mut observation.initial
        {
            for row in rows {
                row.capabilities.retain(|capability| capability != "loop");
            }
        }
        if let Some(execution) = lossy_feed
            && observation.has_feed()
        {
            return Ok(lose_completed_events(
                self.daemon.clone(),
                execution,
                observation,
            ));
        }
        Ok(observation)
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let receipt = self.daemon.execute(command).await?;
        if std::env::var(ACTIVITY_ROUTE).as_deref() == Ok(LOST_END_ROUTE)
            && let Some(CommandResult::Submitted(submitted)) = &receipt.result
        {
            self.daemon
                .runtime()
                .configure_trigger(
                    submitted.chat_id,
                    submitted.loop_id,
                    Some(Trigger {
                        id: Uuid::new_v4(),
                        prompt: "Check for updates".into(),
                        prompt_source: TriggerPromptSource::Explicit,
                        interval_seconds: None,
                        expires_at_ms: None,
                        accepts_events: true,
                        armed: true,
                        subscriptions: Vec::new(),
                    }),
                )
                .await
                .unwrap();
        }
        if let Some(CommandResult::Created {
            execution,
            snapshot,
            submitted,
            ..
        }) = &receipt.result
        {
            std::fs::write(
                ACTIVITY_CREATED,
                serde_json::json!({"created":execution}).to_string(),
            )
            .unwrap();
            if submitted.is_some()
                && std::env::var(ACTIVITY_ROUTE).as_deref() == Ok("overview-dispatch")
            {
                let daemon = self.daemon.clone();
                let execution = *execution;
                let chat = snapshot.conversation.id;
                tokio::spawn(async move {
                    loop {
                        if std::fs::read_to_string(ACTIVITY_STAGE)
                            .is_ok_and(|stage| stage == "continue-2")
                            && daemon
                                .runtime()
                                .snapshot(chat)
                                .await
                                .unwrap()
                                .loops
                                .iter()
                                .all(|goal| {
                                    goal.state == tickloop_agent_protocol::LoopState::Completed
                                })
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    daemon
                        .execute(CommandEnvelope {
                            daemon_epoch: daemon.daemon_epoch(),
                            command_id: Uuid::new_v4(),
                            command: AgentCommand::Submit {
                                execution,
                                target_loop: None,
                                input: UserInput::Text {
                                    text: "A genuine later input while Overview remains open"
                                        .into(),
                                    contexts: vec![],
                                    paths: vec![],
                                },
                                origin: None,
                            },
                        })
                        .await
                        .unwrap()
                        .into_result()
                        .unwrap();
                });
            }
        }
        Ok(receipt)
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.daemon.show_view(update).await
    }
}

#[test]
#[ignore = "owned PTY producer for the Working activity regression"]
fn working_activity_pty_fixture() {
    if std::env::var(ACTIVITY_FIXTURE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("activity-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let core = tickloop_agent_runtime::Agent::new_embedded(
                    tickloop_agent_test_support::config(),
                    Arc::new(ActivityModel(AtomicUsize::new(0))),
                );
                let access = Arc::new(ActivityAccess {
                    daemon: EmbeddedAgentAccess::new(core),
                    queries: AtomicUsize::new(0),
                });
                std::fs::write(ACTIVITY_QUERIES, "0").unwrap();
                let workdir = std::env::current_dir().unwrap();
                let mut launch = LaunchSpec::embedded(workdir.clone());
                launch.model = Some(tickloop_agent_test_support::config());
                launch.session_persistence = false;
                let agent = Agent::from_access_for_test(
                    access.clone(),
                    access.daemon.daemon_epoch(),
                    ViewRef {
                        session_id: Uuid::new_v4(),
                        view_id: Uuid::new_v4(),
                    },
                    launch,
                    None,
                );
                let (probe,mut facts)=mpsc::unbounded_channel();
                agent.set_recovery_probe(probe);
                tokio::spawn(async move {
                    while let Some(fact)=facts.recv().await {
                        if let RecoveryFact::LiveRunStarted {chat,run,overview}=fact {
                            std::fs::write(ACTIVITY_OBSERVED_START,serde_json::json!({"phase":"observed-start","chat":chat,"run":run,"overview":overview}).to_string()).unwrap();
                        }
                    }
                });
                let chat = agent.create_chat(workdir.clone()).await.unwrap();
                let other = agent.create_chat(workdir.clone()).await.unwrap();
                std::fs::write(ACTIVITY_EXECUTIONS, serde_json::json!({"execution":agent.reference(chat).await.unwrap(),"other":agent.reference(other).await.unwrap()}).to_string()).unwrap();
                if std::env::var(ACTIVITY_ROUTE).as_deref() == Ok("attach") {
                    agent
                        .submit(chat, None, "Already running before this View".into())
                        .await
                        .unwrap();
                    while !std::path::Path::new(ACTIVITY_PHASE).exists() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                let startup = StartupState {
                    launch_arguments: Vec::new(),
                    chat: StartupChat::Resumed(
                        Box::new(agent.snapshot(chat).await.unwrap()),
                        false,
                    ),
                                user_settings: None,
                };
                let (console, input) = Console::start_with_display(true, tickloop_agent_config::DisplayPreferences {
                    prefers_reduced_motion: std::env::var(ACTIVITY_ROUTE).as_deref() == Ok("observe-only"),
                }).unwrap();
                console.set_prompt_enabled(true).unwrap();
                interact(&agent, workdir, &console, input, startup, None)
                    .await
                    .unwrap();
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn start_activity_fixture(route: &str) -> (RecoveryPty, tempfile::TempDir) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--ignored",
        "--exact",
        "terminal::recovery_tests::working_activity_tests::working_activity_pty_fixture",
        "--nocapture",
    ]);
    command.env(ACTIVITY_FIXTURE, "1");
    command.env(ACTIVITY_ROUTE, route);
    command.env("TICKLOOP_AGENT_TUI", "fullscreen");
    command.env("TERM", "xterm-256color");
    command.env("HOME", directory.path());
    command.cwd(directory.path());
    let child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            captured.lock().unwrap().extend_from_slice(&bytes[..count]);
        }
    });
    (
        RecoveryPty {
            child,
            writer,
            output,
            _master: pair.master,
            timeout: RECOVERY_TIMEOUT,
        },
        directory,
    )
}

#[test]
fn working_activity_tracks_observed_runs_without_model_duplication_and_esc_interrupts() {
    let (mut fixture, directory) = start_activity_fixture("live");
    fixture.wait_for("Enter a message to");
    fixture.send(b"Continue the same controlled activity\r");
    fixture.wait_until_shown("Working elapsed and actual Esc action", |frame| {
        frame.contains("Working (0s · esc to interrupt)")
    });
    let first = screen::visible_screen(&fixture.raw_output(), 80, 24);
    let activity = first
        .lines()
        .find(|line| line.contains("Working ("))
        .unwrap();
    assert!(
        !activity.contains("fixture") && !activity.contains("Messages"),
        "{activity}"
    );
    let count = || {
        std::fs::read_to_string(directory.path().join(ACTIVITY_QUERIES))
            .unwrap()
            .parse::<usize>()
            .unwrap()
    };
    let queries = count();
    fixture.wait_until_shown("elapsed advances locally", |frame| {
        frame.contains("Working (2s · esc to interrupt)")
    });
    println!(
        "ACTIVITY_ELAPSED seconds=2 total_task_queries={}",
        count() - queries
    );
    let animation_queries = count();
    std::thread::sleep(Duration::from_millis(200));
    let animation_queries = count() - animation_queries;
    println!("ACTIVITY_ANIMATION interval_ms=200 task_queries={animation_queries}");
    assert!(
        animation_queries == 0,
        "local animation queried global tasks {animation_queries} times"
    );
    let executions: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join(ACTIVITY_EXECUTIONS)).unwrap())
            .unwrap();
    fixture.send(b"\x1b[D");
    fixture.open_overview_agent(&executions, "other");
    fixture.wait_until_shown("idle Agent without the prior activity", |frame| {
        !frame.contains("Agents · local") && !frame.contains("Working (")
    });
    fixture.send(b"\x1b");
    fixture.wait_until_shown("idle Esc does not interrupt background work", |frame| {
        !frame.contains("Interrupted") && !frame.contains("Working (")
    });
    fixture.send(b"\x1b[D");
    fixture.open_overview_agent(&executions, "execution");
    fixture.wait_until_shown("returning preserves the observed run clock", |frame| {
        frame.contains("Working (")
            && !frame.contains("Working (0s")
            && !frame.contains("Working (1s")
    });
    std::fs::write(directory.path().join(ACTIVITY_STAGE), "finish-1").unwrap();
    fixture.wait_for("Activity reply 1");
    fixture.wait_for_turn_summary();
    fixture.send(b"Second controlled activity\r");
    fixture.wait_for_phase(&directory.path().join(ACTIVITY_PHASE), "running-2");
    fixture.wait_until_shown("second run starts a new elapsed clock", |frame| {
        frame.contains("Working (0s · esc to interrupt)")
    });
    fixture.send(b"keep this draft");
    fixture.send(b"\x1b");
    fixture.wait_until_shown("Esc interrupts and retains the draft", |frame| {
        frame.contains("Interrupted")
            && frame.contains("keep this draft")
            && !frame.contains("Working (")
    });
    fixture.send(b"\x04\x04");
}

#[test]
fn attached_working_activity_does_not_invent_a_run_start_time() {
    let (mut fixture, _directory) = start_activity_fixture("attach");
    fixture.wait_until_shown("attached run with unknown elapsed", |frame| {
        frame.contains("Working (esc to interrupt)")
    });
    let frame = screen::visible_screen(&fixture.raw_output(), 80, 24);
    assert!(
        !frame.contains("Working (0s") && !frame.contains("Working (2s"),
        "{frame}"
    );
    fixture.send(b"\x1b");
    fixture.wait_until_shown("attached run is interruptible", |frame| {
        frame.contains("Interrupted") && !frame.contains("Working (")
    });
    fixture.send(b"\x04\x04");
}

#[test]
fn overview_dispatch_records_a_live_observed_run_clock_before_opening_the_agent() {
    let (mut fixture, directory) = start_activity_fixture("overview-dispatch");
    fixture.wait_for("Enter a message to");
    fixture.send(b"\x1b[D");
    fixture.wait_for("Agents · local");
    fixture.send(b"\t");
    fixture.wait_until_shown("Overview composer before dispatch", |frame| {
        frame.contains("New Agent") && frame.contains("❯")
    });
    fixture.send(b"Continue the same controlled Overview activity\r");
    fixture.wait_for_phase(&directory.path().join(ACTIVITY_PHASE), "running-1");
    assert!(
        !directory.path().join(ACTIVITY_OBSERVED_START).exists(),
        "initial Create has a snapshot start, not a live event observed by this View"
    );
    fixture.wait_until_shown("the dispatched Agent opens", |frame| {
        frame.contains("Continue the same controlled Overview activity")
            && !frame.contains("Agents · local")
    });
    std::fs::write(directory.path().join(ACTIVITY_STAGE), "finish-1").unwrap();
    fixture.wait_for_turn_summary();
    // Back in Overview, the next run starts while only the list is open.
    fixture.send(b"\x1b[D");
    fixture.wait_for("Agents · local");
    std::fs::write(directory.path().join(ACTIVITY_STAGE), "continue-2").unwrap();
    fixture.wait_for_phase(&directory.path().join(ACTIVITY_PHASE), "running-2");
    let observed = fixture.wait_for_phase(
        &directory.path().join(ACTIVITY_OBSERVED_START),
        "observed-start",
    );
    assert_eq!(
        observed["overview"], true,
        "the actual View must consume Started while Overview is still open"
    );
    std::thread::sleep(Duration::from_secs(2));
    let executions: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join(ACTIVITY_CREATED)).unwrap())
            .unwrap();
    fixture.send(b"\t");
    fixture.open_overview_agent(&executions, "created");
    fixture.wait_until_shown("Overview-observed run elapsed survives opening", |frame| {
        frame.contains("Working (")
            && frame.contains("s · esc to interrupt)")
            && !frame.contains("Working (0s")
            && !frame.contains("Working (1s")
    });
    std::fs::write(directory.path().join(ACTIVITY_STAGE), "finish-2").unwrap();
    fixture.wait_for("Activity reply 2");
    fixture.wait_for_turn_summary();
    fixture.send(b"\x04\x04");
}

#[test]
fn working_activity_without_loop_control_omits_the_action_and_esc_keeps_it_running() {
    let (mut fixture, directory) = start_activity_fixture("observe-only");
    fixture.wait_for("Enter a message to");
    fixture.send(b"Activity without advertised control\r");
    fixture.wait_until_shown("Working without an unavailable Esc action", |frame| {
        working_seconds(frame).is_some() && !frame.contains("esc to interrupt")
    });
    // A loaded machine may draw the first frame or skip a reduced-motion tick a second late, so
    // the elapsed time is compared rather than matched.
    let before_escape =
        working_seconds(&screen::visible_screen(&fixture.raw_output(), 80, 24)).unwrap();
    fixture.send(b"\x1b");
    fixture.wait_until_shown(
        "reduced-motion elapsed advances without interrupting",
        |frame| {
            working_seconds(frame).is_some_and(|seconds| seconds >= before_escape + 2)
                && !frame.contains("esc to interrupt")
                && !frame.contains("Interrupted")
        },
    );
    std::fs::write(directory.path().join(ACTIVITY_STAGE), "finish-1").unwrap();
    fixture.wait_for("Activity reply 1");
    fixture.wait_for_turn_summary();
    fixture.send(b"\x04\x04");
}

/// The elapsed seconds of the Working row, when it shows a known start time.
fn working_seconds(frame: &str) -> Option<u64> {
    let (_, elapsed) = frame.split_once("Working (")?;
    elapsed.split_once('s')?.0.parse().ok()
}

/// Forwards `observation`'s feed, but answers each Completed event the way the worker's feed
/// answers a receiver that lagged past it: a Reset carrying only the current snapshot.
fn lose_completed_events(
    daemon: EmbeddedAgentAccess,
    execution: ExecutionRef,
    observation: Observation,
) -> Observation {
    let (initial, cursor, reset, changes, failure) = observation.into_parts();
    let mut changes = changes.expect("an Agent observation feed");
    let (sender, receiver) = mpsc::channel(FEED_CAPACITY);
    tokio::spawn(async move {
        while let Some(mut update) = changes.recv().await {
            if matches!(&update.change, ObservationChange::Event(event) if matches!(event.kind, EventKind::Completed { .. }))
            {
                let observation = daemon
                    .observe(ObservationRequest::Agent {
                        execution,
                        detail: AgentDetailQuery::Snapshot,
                        after: None,
                    })
                    .await
                    .unwrap();
                let ObservationValue::Agent(snapshot) = observation.initial else {
                    panic!("the Agent snapshot");
                };
                let loops = snapshot
                    .conversation
                    .loops
                    .iter()
                    .map(|goal| format!("{:?}", goal.state))
                    .collect::<Vec<_>>();
                std::fs::write(
                    ACTIVITY_LOST_END,
                    serde_json::json!({"phase":"lost-end","loops":loops}).to_string(),
                )
                .unwrap();
                update.change = ObservationChange::Reset {
                    reason: tickloop_agent_protocol::ResetReason::Lagged,
                    snapshot,
                };
            }
            if sender.send(update).await.is_err() {
                break;
            }
        }
    });
    Observation::new(initial, cursor, reset, Some(receiver), failure)
}

#[test]
fn a_run_whose_end_event_was_lost_stops_working_once_the_snapshot_shows_it_ended() {
    let (mut fixture, directory) = start_activity_fixture(LOST_END_ROUTE);
    fixture.wait_for("Enter a message to");
    fixture.send(b"Watch for updates\r");
    fixture.wait_for_phase(&directory.path().join(ACTIVITY_PHASE), "running-1");
    fixture.wait_until_shown("the live run", |frame| frame.contains("Working ("));
    std::fs::write(directory.path().join(ACTIVITY_STAGE), "finish-1").unwrap();
    fixture.wait_for("Activity reply 1");
    let lost = fixture.wait_for_phase(&directory.path().join(ACTIVITY_LOST_END), "lost-end");
    assert_eq!(
        lost["loops"],
        serde_json::json!(["WaitingTrigger"]),
        "the ended run leaves its Loop waiting rather than finished"
    );
    fixture.wait_until_shown("the ended run without Working", |frame| {
        frame.contains("Activity reply 1") && !frame.contains("Working")
    });
    let frame = screen::visible_screen(&fixture.raw_output(), 80, 24);
    assert_eq!(frame.matches("Activity reply 1").count(), 1, "{frame}");
    fixture.send(b"\x04\x04");
}
