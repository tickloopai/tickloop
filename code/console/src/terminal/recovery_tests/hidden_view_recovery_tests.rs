use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tickloop_code_access::LoopAction;

const HIDDEN_FIXTURE_VARIABLE: &str = "TICKLOOP_HIDDEN_VIEW_RECOVERY_FIXTURE";
const LAGGED_OVERVIEW_ROUTE: &str = "lagged-overview";
const PACED_OVERVIEW_ROUTE: &str = "paced-overview";
const PACED_NAVIGATION_ROUTE: &str = "paced-navigation";
const INTERRUPT_OVERVIEW_ROUTE: &str = "interrupt-overview";
const EXIT_OVERVIEW_ROUTE: &str = "exit-overview";
const FIRST_LINE: &str = "A first visible reply";
const SECOND_LINE: &str = "A second buffered reply";
const THIRD_LINE: &str = "A third buffered reply";
const RECOVERED_REPLY: &str = "A reply recovered after the event gap";

struct HiddenReplyModel {
    requests: AtomicUsize,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    flood_acknowledgements: Arc<tokio::sync::Semaphore>,
    lagged: bool,
}

#[async_trait]
impl ModelClient for HiddenReplyModel {
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "hidden-recovery".into(),
            protocol: Protocol::Messages,
            requested_model: "hidden-recovery".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let text = if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            let release = self.release.lock().unwrap().take().unwrap();
            let text = if self.lagged {
                RECOVERED_REPLY.to_owned()
            } else {
                format!("{FIRST_LINE}\n{SECOND_LINE}\n{THIRD_LINE}\n")
            };
            events.send(ModelDelta::Text(text.clone())).unwrap();
            release.await.unwrap();
            text
        } else {
            for _ in 0..REPLAY_CAPACITY + 1 {
                events
                    .send(ModelDelta::Thinking("B bounded event flood".into()))
                    .unwrap();
                events.send(ModelDelta::Usage(Usage::default())).unwrap();
                self.flood_acknowledgements
                    .acquire()
                    .await
                    .unwrap()
                    .forget();
            }
            "B flood completed".into()
        };
        Ok(ModelOutput {
            private_source: None,
            content: tickloop_harness::test_support::text(&text),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

struct HiddenRecoveryAccess {
    daemon: EmbeddedAgentAccess,
    source: Mutex<Option<ExecutionRef>>,
    in_overview: AtomicBool,
    overview_entered: Mutex<Option<oneshot::Sender<()>>>,
    overview_release: Mutex<Option<oneshot::Receiver<()>>>,
    return_entered: Mutex<Option<oneshot::Sender<()>>>,
    return_release: Mutex<Option<oneshot::Receiver<()>>>,
    submits: AtomicUsize,
    interrupts: AtomicUsize,
}

#[async_trait]
impl AgentAccess for HiddenRecoveryAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        self.daemon
            .observe(query)
            .await
            .map(super::listed_as_worked)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        if matches!(command.command, AgentCommand::Submit { .. }) {
            self.submits.fetch_add(1, Ordering::SeqCst);
        }
        if matches!(
            command.command,
            AgentCommand::ManageLoop {
                action: LoopAction::Interrupt,
                ..
            }
        ) {
            self.interrupts.fetch_add(1, Ordering::SeqCst);
        }
        self.daemon.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        let scene = matches!(update, DisplayUpdate::Scene { .. });
        let source_displayed = matches!(&update, DisplayUpdate::Displayed { execution, .. }
            if Some(*execution) == *self.source.lock().unwrap());
        let receipt = self.daemon.show_view(update).await?;
        if scene {
            self.in_overview.store(true, Ordering::SeqCst);
            let release = self.overview_release.lock().unwrap().take();
            if let Some(release) = release {
                self.overview_entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                release.await.unwrap();
            }
        } else if source_displayed && self.in_overview.swap(false, Ordering::SeqCst) {
            let release = self.return_release.lock().unwrap().take();
            if let Some(release) = release {
                self.return_entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                release.await.unwrap();
            }
        }
        Ok(receipt)
    }
}

#[test]
#[ignore = "owned PTY producer for hidden conversation recovery"]
fn hidden_view_recovery_pty_fixture() {
    if std::env::var(HIDDEN_FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("hidden-view-recovery".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_hidden_view_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_hidden_view_fixture() {
    let route = std::env::var(ROUTE_VARIABLE).unwrap();
    let lagged = route == LAGGED_OVERVIEW_ROUTE;
    let (release_model, gate) = oneshot::channel();
    let flood_acknowledgements = Arc::new(tokio::sync::Semaphore::new(0));
    let model = Arc::new(HiddenReplyModel {
        requests: AtomicUsize::new(0),
        release: Mutex::new(Some(gate)),
        flood_acknowledgements: flood_acknowledgements.clone(),
        lagged,
    });
    let core = tickloop_harness::runtime::Agent::new_embedded(
        tickloop_harness::test_support::config(),
        model.clone(),
    );
    let (overview_entered, entered) = oneshot::channel();
    let (release_overview, overview_gate) = oneshot::channel();
    let (return_sender, returning) = oneshot::channel();
    let (release_return, return_gate) = oneshot::channel();
    let access = Arc::new(HiddenRecoveryAccess {
        daemon: EmbeddedAgentAccess::new(core.clone()),
        source: Mutex::new(None),
        in_overview: AtomicBool::new(false),
        overview_entered: Mutex::new(lagged.then_some(overview_entered)),
        overview_release: Mutex::new(lagged.then_some(overview_gate)),
        return_entered: Mutex::new(Some(return_sender)),
        return_release: Mutex::new(Some(return_gate)),
        submits: AtomicUsize::new(0),
        interrupts: AtomicUsize::new(0),
    });
    let workdir = std::env::current_dir().unwrap();
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(tickloop_harness::test_support::config());
    launch.session_persistence = false;
    let view = ViewRef {
        session_id: Uuid::new_v4(),
        view_id: Uuid::new_v4(),
    };
    let agent = Agent::from_access_for_test(
        access.clone(),
        access.daemon.daemon_epoch(),
        view,
        launch,
        None,
    );
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let other = agent.create_chat(workdir.clone()).await.unwrap();
    let execution = agent.reference(chat).await.unwrap();
    let other_execution = agent.reference(other).await.unwrap();
    *access.source.lock().unwrap() = Some(execution);
    let checkpoint = Arc::new(tokio::sync::Semaphore::new(0));
    agent.hold_recovery_checkpoint(checkpoint.clone());
    let (probe, mut facts) = mpsc::unbounded_channel();
    agent.set_recovery_probe(probe);
    let (console, mut console_input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let (forward, input) = mpsc::channel(FEED_CAPACITY);
    let (overview_queued, queued) = oneshot::channel();
    tokio::task::spawn_local(async move {
        let mut overview_queued = Some(overview_queued);
        while let Some(event) = console_input.recv().await {
            let overview = matches!(event, InputEvent::OpenOverview);
            if forward.send(event).await.is_err() {
                break;
            }
            if overview && let Some(queued) = overview_queued.take() {
                queued.send(()).unwrap();
            }
        }
    });
    let startup = StartupState {
        model_selected: true,
        launch_arguments: Vec::new(),
        chat: StartupChat::Resumed(Box::new(agent.snapshot(chat).await.unwrap())),
        user_settings: None,
    };
    let interaction_agent = agent.clone();
    let interaction_console = console.clone();
    let interaction = tokio::task::spawn_local(async move {
        interact(
            &interaction_agent,
            workdir,
            &interaction_console,
            input,
            startup,
            None,
        )
        .await
    });
    let buffered = next_fact(&mut facts, |fact| {
        matches!(fact, RecoveryFact::Buffered { .. })
    })
    .await;
    let RecoveryFact::Buffered {
        chat: buffered_chat,
        run,
        sequence,
        complete_line,
    } = buffered
    else {
        unreachable!();
    };
    assert_eq!(buffered_chat, chat);
    assert_eq!(complete_line, !lagged);
    let running = core.snapshot(chat.into()).await.unwrap();
    assert_eq!(
        running.chat_loop.state,
        tickloop_harness::protocol::LoopState::Running
    );
    assert_eq!(running.chat_loop.run_id, Some(Into::into(run)));
    assert_eq!(model.requests.load(Ordering::SeqCst), 1);
    assert_eq!(access.submits.load(Ordering::SeqCst), 1);
    write_phase(
        serde_json::json!({"phase":"buffered","execution":execution,"other":other_execution}),
    );
    console.write("\nBuffered hidden reply ready\n").unwrap();
    tokio::time::timeout(RECOVERY_TIMEOUT, queued)
        .await
        .unwrap()
        .unwrap();
    checkpoint.add_permits(1);
    let mut release_model = Some(release_model);
    let mut forwarded = 0;
    if lagged {
        tokio::time::timeout(RECOVERY_TIMEOUT, entered)
            .await
            .unwrap()
            .unwrap();
        release_model.take().unwrap().send(()).unwrap();
        tokio::time::timeout(RECOVERY_TIMEOUT, async {
            while core.snapshot(chat.into()).await.unwrap().chat_loop.state
                != tickloop_harness::protocol::LoopState::Idle
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let receipt = access
            .execute(CommandEnvelope {
                daemon_epoch: access.daemon.daemon_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Submit {
                    execution: other_execution,
                    input: UserInput::Text {
                        text: "B bounded traffic".into(),
                        contexts: vec![],
                        paths: vec![],
                    },
                    origin: None,
                },
            })
            .await
            .unwrap();
        assert!(
            matches!(receipt.result,Some(CommandResult::Submitted(ref receipt)) if receipt.chat_id == other)
        );
        while forwarded < REPLAY_CAPACITY + 1 {
            let fact = next_fact(&mut facts, |fact| matches!(fact,RecoveryFact::Forwarded {chat:forwarded_chat,sequence,..} if *forwarded_chat == other && *sequence > 1)).await;
            assert!(matches!(fact, RecoveryFact::Forwarded { .. }));
            forwarded += 1;
            flood_acknowledgements.add_permits(1);
        }
        release_overview.send(()).unwrap();
    }
    next_fact(&mut facts, |fact| {
        matches!(fact, RecoveryFact::OverviewOpened)
    })
    .await;
    if lagged {
        next_fact(&mut facts, |fact| matches!(fact, RecoveryFact::Lagged)).await;
    }
    write_phase(
        serde_json::json!({"phase":"overview-ready","execution":execution,"other":other_execution,"lagged":lagged,"forwarded":forwarded}),
    );
    if route == EXIT_OVERVIEW_ROUTE {
        interaction.await.unwrap().unwrap();
        let snapshot = core.snapshot(chat.into()).await.unwrap();
        std::fs::write(
            std::env::var_os(PROOF_VARIABLE).unwrap(),
            serde_json::to_vec(&serde_json::json!({
                "route":route,"execution":execution,"view":view,"run":run,
                "first_sequence":sequence,"last_sequence":snapshot.last_sequence,
                "state":snapshot.chat_loop.state,
                "submits":access.submits.load(Ordering::SeqCst),
                "model_requests":model.requests.load(Ordering::SeqCst),
                "interrupts":access.interrupts.load(Ordering::SeqCst),
            }))
            .unwrap(),
        )
        .unwrap();
        core.shutdown().await;
        return;
    }
    if route == PACED_NAVIGATION_ROUTE {
        next_fact(
            &mut facts,
            |fact| matches!(fact,RecoveryFact::AgentOpened {chat} if *chat == other),
        )
        .await;
        write_phase(
            serde_json::json!({"phase":"other-opened","execution":execution,"other":other_execution}),
        );
    }
    tokio::time::timeout(RECOVERY_TIMEOUT, returning)
        .await
        .unwrap()
        .unwrap();
    let mut overview_restores = Vec::new();
    while let Ok(fact) = facts.try_recv() {
        if matches!(fact, RecoveryFact::Restored { chat: restored, .. } if restored == chat) {
            overview_restores.push(fact);
        }
    }
    release_return.send(()).unwrap();
    let returned = next_fact(&mut facts, |fact| {
        if route == PACED_NAVIGATION_ROUTE {
            matches!(fact, RecoveryFact::AgentOpened { chat: opened } if *opened == chat)
        } else {
            matches!(fact, RecoveryFact::Restored { chat: restored, .. } if *restored == chat)
        }
    })
    .await;
    let return_after = match &returned {
        RecoveryFact::Restored { after, .. } => Some(*after),
        RecoveryFact::AgentOpened { .. } => None,
        _ => unreachable!(),
    };
    let snapshot = core.snapshot(chat.into()).await.unwrap();
    assert_eq!(snapshot.chat_loop.run_id, Some(Into::into(run)));
    if route != INTERRUPT_OVERVIEW_ROUTE {
        assert_eq!(
            snapshot.chat_loop.state,
            if lagged {
                tickloop_harness::protocol::LoopState::Idle
            } else {
                tickloop_harness::protocol::LoopState::Running
            }
        );
    }
    if route == PACED_NAVIGATION_ROUTE {
        assert_eq!(snapshot.last_sequence, sequence);
    } else {
        assert!(
            matches!(returned, RecoveryFact::Restored {last_sequence,reset_required:false,..} if last_sequence == snapshot.last_sequence)
        );
    }
    let expected_calls = if lagged { 2 } else { 1 };
    assert_eq!(access.submits.load(Ordering::SeqCst), expected_calls);
    assert_eq!(model.requests.load(Ordering::SeqCst), expected_calls);
    std::fs::write(std::env::var_os(PROOF_VARIABLE).unwrap(), serde_json::to_vec(&serde_json::json!({
        "route":route,"execution":execution,"other":other_execution,"view":view,"run":run,
        "first_sequence":sequence,"return_after":return_after,"last_sequence":snapshot.last_sequence,
        "forwarded":forwarded,"return_fact":returned,"submits":expected_calls,"model_requests":expected_calls,
        "state":snapshot.chat_loop.state,"overview_restores":overview_restores,
        "interrupts":access.interrupts.load(Ordering::SeqCst),
    })).unwrap()).unwrap();
    console.write("\nHidden recovery facts verified\n").unwrap();
    if route == INTERRUPT_OVERVIEW_ROUTE {
        tokio::time::timeout(RECOVERY_TIMEOUT, async {
            while core.snapshot(chat.into()).await.unwrap().chat_loop.state
                != tickloop_harness::protocol::LoopState::Interrupted
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let snapshot = core.snapshot(chat.into()).await.unwrap();
        write_phase(serde_json::json!({
            "phase":"visible-interrupted","state":snapshot.chat_loop.state,
            "interrupts":access.interrupts.load(Ordering::SeqCst),
            "submits":access.submits.load(Ordering::SeqCst),
            "model_requests":model.requests.load(Ordering::SeqCst),
        }));
    } else if let Some(release) = release_model {
        release.send(()).unwrap();
    }
    interaction.await.unwrap().unwrap();
    if route == INTERRUPT_OVERVIEW_ROUTE {
        let snapshot = core.snapshot(chat.into()).await.unwrap();
        let path = PathBuf::from(std::env::var_os(PROOF_VARIABLE).unwrap());
        let mut proof: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        proof["view_exit_state"] = serde_json::to_value(&snapshot.chat_loop.state).unwrap();
        proof["view_exit_interrupts"] = access.interrupts.load(Ordering::SeqCst).into();
        std::fs::write(path, serde_json::to_vec(&proof).unwrap()).unwrap();
    }
    core.shutdown().await;
}

#[test]
fn an_event_gap_in_overview_restores_the_source_reply_on_return() {
    run_hidden_view_pty(LAGGED_OVERVIEW_ROUTE);
}

#[test]
fn a_paced_multiline_reply_stays_with_its_source_while_overview_is_open() {
    run_hidden_view_pty(PACED_OVERVIEW_ROUTE);
}

#[test]
fn a_paced_multiline_reply_survives_switching_to_another_chat_and_back() {
    run_hidden_view_pty(PACED_NAVIGATION_ROUTE);
}

#[test]
fn overview_control_c_keeps_hidden_work_and_visible_control_c_still_interrupts() {
    run_hidden_view_pty(INTERRUPT_OVERVIEW_ROUTE);
}

#[test]
fn double_control_c_in_overview_exits_only_the_view() {
    run_hidden_view_pty(EXIT_OVERVIEW_ROUTE);
}

fn run_hidden_view_pty(route: &str) {
    let directory = tempfile::tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT).unwrap();
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
        "terminal::recovery_tests::hidden_view_recovery_tests::hidden_view_recovery_pty_fixture",
        "--nocapture",
    ]);
    command.env(HIDDEN_FIXTURE_VARIABLE, "1");
    command.env(ROUTE_VARIABLE, route);
    let phase_path = directory.path().join("phase.json");
    let proof_path = directory.path().join("proof.json");
    command.env(PHASE_VARIABLE, &phase_path);
    command.env(PROOF_VARIABLE, &proof_path);
    command.env("TICKLOOP_AGENT_TUI", "fullscreen");
    command.env("TERM", "xterm-256color");
    command.env("HOME", directory.path());
    command.env("TICKLOOP_TMPDIR", directory.path());
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
    let mut fixture = RecoveryPty {
        child,
        writer,
        output,
        _master: pair.master,
        timeout: LAGGED_DRAIN_TIMEOUT,
    };
    fixture.wait_for("Tickloop v");
    fixture.send(b"Continue the same bounded cadence task.\r");
    fixture.wait_for("Buffered hidden reply ready");
    fixture.send(b"\x1b[D");
    let phase = fixture.wait_for_phase(&phase_path, "overview-ready");
    fixture.wait_for("Agents");
    if matches!(route, INTERRUPT_OVERVIEW_ROUTE | EXIT_OVERVIEW_ROUTE) {
        fixture.send(b"\t");
        fixture.wait_for("enter dispatch");
        fixture.send(b"\x03");
        let overview_frame = fixture.wait_for("Press ctrl+c again to exit");
        assert!(
            !overview_frame.contains(FIRST_LINE)
                && !overview_frame.contains(SECOND_LINE)
                && !overview_frame.contains(THIRD_LINE),
            "hidden replies stay out of Overview: {overview_frame}"
        );
        if route == EXIT_OVERVIEW_ROUTE {
            fixture.send(b"\x03");
            wait_for_hidden_fixture_exit(&mut fixture);
            let proof: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&proof_path).unwrap()).unwrap();
            println!(
                "OVERVIEW_EXIT_PROOF {proof}\nOVERVIEW_EXIT_FRAME_BEGIN\n{overview_frame}\nOVERVIEW_EXIT_FRAME_END"
            );
            assert_eq!(
                proof["state"], "Running",
                "leaving the View must not interrupt its hidden Agent"
            );
            assert_eq!(proof["last_sequence"], proof["first_sequence"]);
            assert_eq!(proof["interrupts"], 0);
            assert_eq!(proof["submits"], 1);
            assert_eq!(proof["model_requests"], 1);
            return;
        }
    }
    if route == PACED_NAVIGATION_ROUTE {
        fixture.open_overview_agent(&phase, "other");
        let phase = fixture.wait_for_phase(&phase_path, "other-opened");
        let other_frame = fixture.repaint_after_recovery("other Chat", |frame| {
            frame.contains("Context unknown") && !frame.contains("Project:")
        });
        assert!(
            !other_frame.contains(FIRST_LINE)
                && !other_frame.contains(SECOND_LINE)
                && !other_frame.contains(THIRD_LINE),
            "A's reply must not appear in B: {other_frame}"
        );
        fixture.send(b"\x1b[D");
        fixture.open_overview_agent(&phase, "execution");
    } else {
        fixture.send(b"\x1b[27u");
    }
    fixture.wait_for("Hidden recovery facts verified");
    let frame = fixture.repaint_after_recovery("returned source Chat", |frame| {
        frame.contains("Hidden recovery facts verified")
    });
    let proof: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&proof_path).unwrap()).unwrap();
    println!(
        "HIDDEN_RECOVERY_PROOF {proof}\nHIDDEN_RECOVERY_FRAME_BEGIN\n{frame}\nHIDDEN_RECOVERY_FRAME_END"
    );
    if route == INTERRUPT_OVERVIEW_ROUTE {
        assert_eq!(
            proof["state"], "Running",
            "Ctrl+C in Overview must not interrupt its hidden source: {proof}"
        );
        assert_eq!(proof["interrupts"], 0);
        assert_eq!(proof["last_sequence"], proof["first_sequence"]);
    }
    for line in if route == LAGGED_OVERVIEW_ROUTE {
        vec![RECOVERED_REPLY]
    } else {
        vec![FIRST_LINE, SECOND_LINE, THIRD_LINE]
    } {
        assert_eq!(
            frame.matches(line).count(),
            1,
            "source reply must remain visible once after returning: {line}\n{frame}\n{proof}"
        );
    }
    if route == LAGGED_OVERVIEW_ROUTE {
        assert!(proof["forwarded"].as_u64().unwrap() > REPLAY_CAPACITY as u64);
        assert_eq!(
            proof["return_after"], proof["first_sequence"],
            "Overview must not advance the source's displayed cursor"
        );
    } else {
        assert!(frame.find(FIRST_LINE).unwrap() < frame.find(SECOND_LINE).unwrap());
        assert!(frame.find(SECOND_LINE).unwrap() < frame.find(THIRD_LINE).unwrap());
        assert_eq!(proof["model_requests"], 1);
        assert_eq!(proof["submits"], 1);
    }
    if route == INTERRUPT_OVERVIEW_ROUTE {
        fixture.send(b"\x03");
        let interrupted = fixture.wait_for_phase(&phase_path, "visible-interrupted");
        let visible_frame = fixture.wait_for("Interrupted");
        assert_eq!(interrupted["state"], "Interrupted");
        assert_eq!(interrupted["interrupts"], 1);
        assert_eq!(interrupted["submits"], 1);
        assert_eq!(interrupted["model_requests"], 1);
        for line in [FIRST_LINE, SECOND_LINE, THIRD_LINE] {
            assert_eq!(
                visible_frame.matches(line).count(),
                1,
                "visible interruption keeps the reply once: {visible_frame}"
            );
        }
        fixture.send(b"\x03");
    } else {
        fixture.send(b"\x04\x04");
    }
    wait_for_hidden_fixture_exit(&mut fixture);
    if route == INTERRUPT_OVERVIEW_ROUTE {
        let exited: serde_json::Value =
            serde_json::from_slice(&std::fs::read(proof_path).unwrap()).unwrap();
        println!("VISIBLE_CONTROL_C_EXIT_PROOF {exited}");
        assert_eq!(exited["view_exit_state"], "Interrupted");
        assert_eq!(
            exited["view_exit_interrupts"], 1,
            "the second Ctrl+C only leaves the View"
        );
    }
}

fn wait_for_hidden_fixture_exit(fixture: &mut RecoveryPty) {
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "hidden recovery producer failed: {}",
                fixture.raw_output()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "hidden recovery producer did not close"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
