use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tickloop_code_access::AccessErrorCode;

const LOST_FIXTURE_VARIABLE: &str = "TICKLOOP_LOST_PRESENTATION_FIXTURE";
const LOST_COMPLETED_VARIABLE: &str = "TICKLOOP_LOST_COMPLETED_FIXTURE";
const LOST_UNKNOWN_VARIABLE: &str = "TICKLOOP_LOST_UNKNOWN_INPUT_FIXTURE";
const LOST_UNKNOWN_FLAG: &str = "unknown-input";
const DAEMON_UNAVAILABLE_FLAG: &str = "daemon-unavailable";
const DAEMON_UNAVAILABLE_CAUSE: &str = "owned daemon catalog is unavailable";
const LOST_FLAG: &str = "execution-lost";
const LOST_PROOF: &str = "loss-proof.json";
const LOST_MODEL: &str = "loss-model";
const LOST_TASK: &str = "Investigate controlled daemon loss";
const LOST_RAW_ERROR: &str = "execution worker lost";
const LOST_WIDTH: usize = 180;
const LOST_HEIGHT: usize = 30;

struct LostPresentationAccess {
    daemon: EmbeddedAgentAccess,
    commands: AtomicUsize,
    live_snapshots_during_daemon_failure: AtomicUsize,
}

#[async_trait]
impl AgentAccess for LostPresentationAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let lost = std::path::Path::new(LOST_FLAG).exists();
        let daemon_unavailable = std::path::Path::new(DAEMON_UNAVAILABLE_FLAG).exists();
        if daemon_unavailable && matches!(query, ObservationRequest::Overview { .. }) {
            return Err(AccessError::unavailable(DAEMON_UNAVAILABLE_CAUSE));
        }
        let snapshot_query = matches!(
            query,
            ObservationRequest::Agent {
                detail: AgentDetailQuery::Snapshot,
                ..
            }
        );
        if lost
            && matches!(
                query,
                ObservationRequest::Agent {
                    detail: AgentDetailQuery::Snapshot
                        | AgentDetailQuery::Status
                        | AgentDetailQuery::Events { .. },
                    ..
                }
            )
        {
            return Err(AccessError::invalid("execution worker lost"));
        }
        let mut observation = self.daemon.observe(query).await?;
        if daemon_unavailable && snapshot_query {
            self.live_snapshots_during_daemon_failure
                .fetch_add(1, Ordering::SeqCst);
        }
        if lost && let ObservationValue::Overview(rows) = &mut observation.initial {
            for row in rows {
                row.availability = ExecutionAvailability::Lost;
            }
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.commands.fetch_add(1, Ordering::SeqCst);
        if std::path::Path::new(LOST_UNKNOWN_FLAG).exists()
            && matches!(command.command, AgentCommand::Submit { .. })
        {
            return Err(AccessError::new(
                AccessErrorCode::OutcomeUnknown,
                "owned fixture lost the input acknowledgement",
            ));
        }
        self.daemon.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.daemon.show_view(update).await
    }
}

struct LostPresentationModel {
    calls: AtomicUsize,
}

#[async_trait]
impl ModelClient for LostPresentationModel {
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        Ok(tickloop_harness_model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "lost-presentation-fixture".into(),
            protocol: Protocol::Messages,
            requested_model: LOST_MODEL.into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if std::env::var(LOST_COMPLETED_VARIABLE).as_deref() == Ok("1") {
            return Ok(ModelOutput {
                private_source: None,
                content: tickloop_harness_test_support::text(
                    "The controlled investigation completed.",
                ),
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            });
        }
        cancel.cancelled().await;
        Err(ModelError::Cancelled)
    }
}

#[test]
#[ignore = "owned PTY producer for Lost execution presentation"]
fn lost_presentation_pty_fixture() {
    if std::env::var(LOST_FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("lost-presentation-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let mut config = tickloop_harness_test_support::config();
                config.model = LOST_MODEL.into();
                config.name = LOST_MODEL.into();
                let model = Arc::new(LostPresentationModel {
                    calls: AtomicUsize::new(0),
                });
                let core = tickloop_harness_runtime::Agent::new_embedded(config.clone(), model.clone());
                let access = Arc::new(LostPresentationAccess {
                    daemon: EmbeddedAgentAccess::new(core.clone()),
                    commands: AtomicUsize::new(0),
                    live_snapshots_during_daemon_failure: AtomicUsize::new(0),
                });
                let workdir = std::env::current_dir().unwrap();
                let mut launch = LaunchSpec::embedded(workdir.clone());
                launch.model = Some(config);
                launch.host_options.execution_mode_override = Some(ExecutionMode::Auto);
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
                let chat = agent.create_chat(workdir.clone()).await.unwrap();
                agent
                    .set_execution_mode(chat, ExecutionMode::Auto)
                    .await
                    .unwrap();
                let receipt = agent.submit(chat, LOST_TASK.into()).await.unwrap();
                while model.calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
                if std::env::var(LOST_COMPLETED_VARIABLE).as_deref() == Ok("1") {
                    while core.snapshot(chat.into()).await.unwrap().chat_loop.state
                        != tickloop_harness_protocol::LoopState::Idle
                    {
                        tokio::task::yield_now().await;
                    }
                }
                if std::env::var(LOST_UNKNOWN_VARIABLE).as_deref() == Ok("1") {
                    std::fs::write(LOST_UNKNOWN_FLAG, "owned fixture").unwrap();
                    assert!(
                        agent
                            .submit(chat, "Unconfirmed follow-up".into())
                            .await
                            .is_err()
                    );
                    assert!(agent.unknown_input(chat).is_some());
                }
                let unknown_input_at_start = agent.unknown_input(chat).is_some();
                let snapshot = agent.snapshot(chat).await.unwrap();
                let (console, input) = Console::start(true).unwrap();
                console.set_prompt_enabled(true).unwrap();
                let proof_access = access.clone();
                let proof_model = model.clone();
                let proof_core = core.clone();
                let proof_agent = agent.clone();
                let proof_task = tokio::task::spawn_local(async move {
                    loop {
                        if std::path::Path::new(DAEMON_UNAVAILABLE_FLAG).exists() {
                            proof_agent.snapshot(chat).await.unwrap();
                        }
                        let snapshot = proof_core.snapshot(chat.into()).await.unwrap();
                        std::fs::write(
                            LOST_PROOF,
                            serde_json::to_vec(&serde_json::json!({
                                "chat": chat,
                                "run": receipt.run_id,
                                "commands": proof_access.commands.load(Ordering::SeqCst),
                                "model_calls": proof_model.calls.load(Ordering::SeqCst),
                                "state": snapshot.chat_loop.state,
                                "unknown_input_at_start": unknown_input_at_start,
                                "live_snapshots_during_daemon_failure": proof_access.live_snapshots_during_daemon_failure.load(Ordering::SeqCst),
                            }))
                            .unwrap(),
                        )
                        .unwrap();
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
                let outcome = interact(
                    &agent,
                    workdir,
                    &console,
                    input,
                    StartupState {
                        model_selected: true,
                        launch_arguments: Vec::new(),
                        chat: StartupChat::Resumed(Box::new(snapshot), true),
                                        user_settings: None,
                    },
                    None,
                )
                .await;
                proof_task.abort();
                drop(console);
                core.shutdown().await;
                assert!(
                    outcome.is_ok()
                        || outcome.as_ref().err().map(String::as_str)
                            == Some("one or more tasks failed; review the errors above"),
                    "{outcome:?}"
                );
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

enum LostFixtureState {
    Running,
    Completed,
    CompletedWithUnknownInput,
}

fn start_lost_presentation_fixture(state: LostFixtureState) -> (RecoveryPty, tempfile::TempDir) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: LOST_HEIGHT as u16,
            cols: LOST_WIDTH as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--ignored",
        "--exact",
        "terminal::recovery_tests::lost_presentation_tests::lost_presentation_pty_fixture",
        "--nocapture",
    ]);
    command.env(LOST_FIXTURE_VARIABLE, "1");
    if matches!(
        state,
        LostFixtureState::Completed | LostFixtureState::CompletedWithUnknownInput
    ) {
        command.env(LOST_COMPLETED_VARIABLE, "1");
    }
    if matches!(state, LostFixtureState::CompletedWithUnknownInput) {
        command.env(LOST_UNKNOWN_VARIABLE, "1");
    }
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
    (
        RecoveryPty {
            child,
            writer,
            output,
            _master: pair.master,
            timeout: Duration::from_secs(10),
        },
        directory,
    )
}

fn lost_presentation_frame(view: &RecoveryPty) -> String {
    screen::visible_screen(&view.raw_output(), LOST_WIDTH, LOST_HEIGHT)
}

fn wait_for_lost_presentation(view: &mut RecoveryPty, ready: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + view.timeout;
    loop {
        let frame = lost_presentation_frame(view);
        if ready(&frame) {
            return frame;
        }
        assert!(
            view.child.try_wait().unwrap().is_none(),
            "Lost fixture exited before its frame: {}",
            view.raw_output()
        );
        assert!(
            Instant::now() < deadline,
            "Lost fixture never rendered the expected frame: {frame}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn lost_presentation_proof(directory: &std::path::Path) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Ok(bytes) = std::fs::read(directory.join(LOST_PROOF))
            && let Ok(proof) = serde_json::from_slice(&bytes)
        {
            return proof;
        }
        assert!(
            Instant::now() < deadline,
            "the owned fixture did not publish its proof"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn lost_execution_has_one_readable_notice_and_keeps_the_core_status() {
    let (mut view, directory) = start_lost_presentation_fixture(LostFixtureState::Running);
    wait_for_lost_presentation(&mut view, |frame| {
        frame.contains("Working") && frame.contains(LOST_MODEL)
    });
    let before = lost_presentation_proof(directory.path());
    std::fs::write(directory.path().join(LOST_FLAG), "lost").unwrap();
    let frame = wait_for_lost_presentation(&mut view, |frame| {
        (frame.contains("nothing was resent") || frame.contains("nothing resent"))
            && frame.contains("Lost · outcome unknown")
    });
    println!("LOST_PRESENTATION_FRAME_BEGIN\n{frame}\nLOST_PRESENTATION_FRAME_END");
    std::thread::sleep(Duration::from_millis(60));
    let after = lost_presentation_proof(directory.path());
    assert_eq!(
        after["commands"], before["commands"],
        "no command is resent on loss"
    );
    assert_eq!(after["model_calls"], before["model_calls"]);
    assert_eq!(
        after["state"], "Running",
        "the presentation fixture leaves runtime execution untouched"
    );
    let status = frame
        .lines()
        .rev()
        .find(|line| {
            line.contains("cached execution")
                || line.contains("outcome unknown")
                || line.contains("Outcome unknown")
        })
        .unwrap();
    assert_eq!(
        status.matches(LOST_RAW_ERROR).count(),
        0,
        "the raw daemon-loss cause belongs in explicit details: {status}"
    );
    let core = frame
        .lines()
        .find(|line| line.starts_with('─') && line.contains("⏵⏵ Auto"))
        .unwrap();
    assert!(
        core.contains(LOST_MODEL) && core.contains("Context unknown"),
        "loss feedback must preserve readable core status on the lower divider: {core}"
    );
    assert_eq!(frame.matches("⏵⏵ Auto").count(), 1, "{frame}");
    assert!(
        !status.contains("⏵⏵ Auto") && !status.contains(LOST_MODEL),
        "the connection notice stays separate from floated core status: {status}"
    );
    assert!(
        !frame.contains(&public(&before["run"])),
        "the default interactive loss notice uses the readable Agent label: {frame}"
    );
    assert!(
        !frame.contains(&format!("/resume {}", public(&before["chat"]))),
        "the default action uses the history picker: {frame}"
    );
    assert!(
        frame.contains("/resume")
            && (frame.contains("unknown outcome")
                || frame.contains("Outcome unknown")
                || frame.contains("outcome unknown")),
        "the loss and explicit history action stay visible: {frame}"
    );
    view.writer.write_all(b"\x0f").unwrap();
    view.writer.flush().unwrap();
    let details = wait_for_lost_presentation(&mut view, |frame| {
        frame.contains("Execution connection details") && frame.contains(LOST_RAW_ERROR)
    });
    assert!(details.contains(&public(&before["run"])), "{details}");
    assert!(
        details.contains(&format!("/resume {}", public(&before["chat"]))),
        "{details}"
    );
    view.writer.write_all(b"\x1b").unwrap();
    view.writer.flush().unwrap();
    let returned = wait_for_lost_presentation(&mut view, |frame| {
        !frame.contains("Showing detailed transcript") && frame.contains("nothing was resent")
    });
    assert_eq!(
        returned.matches("Execution connection was lost").count(),
        1,
        "{returned}"
    );
    let after_details = lost_presentation_proof(directory.path());
    assert_eq!(after_details["commands"], before["commands"]);
    assert_eq!(after_details["model_calls"], before["model_calls"]);
}

#[test]
fn completed_cached_execution_keeps_its_known_outcome_and_exposes_connection_details() {
    let (mut view, directory) = start_lost_presentation_fixture(LostFixtureState::Completed);
    wait_for_lost_presentation(&mut view, |frame| {
        frame.contains("The controlled investigation completed.") && frame.contains(LOST_MODEL)
    });
    let before = lost_presentation_proof(directory.path());
    assert_eq!(before["state"], "Idle");
    std::fs::write(directory.path().join(LOST_FLAG), "lost").unwrap();
    let frame =
        wait_for_lost_presentation(&mut view, |frame| frame.contains("Lost · cached history"));
    assert!(
        frame.contains("⏵⏵ Auto")
            && frame.contains(LOST_MODEL)
            && frame.contains("Context unknown"),
        "{frame}"
    );
    assert!(frame.contains("ctrl+o details"), "{frame}");
    assert!(
        !frame.contains(LOST_RAW_ERROR) && !frame.contains("outcome unknown"),
        "{frame}"
    );
    assert!(
        frame.contains("The controlled investigation completed."),
        "{frame}"
    );
    view.writer.write_all(b"\x0f").unwrap();
    view.writer.flush().unwrap();
    let details = wait_for_lost_presentation(&mut view, |frame| {
        frame.contains("Last connection diagnostic (this View)") && frame.contains(LOST_RAW_ERROR)
    });
    assert!(
        details.contains(&format!("/resume {}", public(&before["chat"]))),
        "{details}"
    );
    assert!(
        !details.contains("Execution connection details"),
        "no run loss event was observed: {details}"
    );
    let after = lost_presentation_proof(directory.path());
    assert_eq!(after["state"], "Idle");
    assert_eq!(after["commands"], before["commands"]);
    assert_eq!(after["model_calls"], before["model_calls"]);
}

#[test]
fn an_unknown_input_after_a_completed_run_still_has_an_unknown_outcome_on_loss() {
    let (mut view, directory) =
        start_lost_presentation_fixture(LostFixtureState::CompletedWithUnknownInput);
    wait_for_lost_presentation(&mut view, |frame| {
        frame.contains("The controlled investigation completed.") && frame.contains(LOST_MODEL)
    });
    let before = lost_presentation_proof(directory.path());
    assert_eq!(before["state"], "Idle");
    assert_eq!(before["unknown_input_at_start"], true);
    std::fs::write(directory.path().join(LOST_FLAG), "lost").unwrap();
    let frame =
        wait_for_lost_presentation(&mut view, |frame| frame.contains("Lost · outcome unknown"));
    assert!(!frame.contains("Lost · cached history"), "{frame}");
    assert!(
        frame.contains("⏵⏵ Auto")
            && frame.contains(LOST_MODEL)
            && frame.contains("Context unknown"),
        "{frame}"
    );
    let after = lost_presentation_proof(directory.path());
    assert_eq!(after["state"], "Idle");
    assert_eq!(after["commands"], before["commands"]);
    assert_eq!(after["model_calls"], before["model_calls"]);
}

#[test]
fn an_unavailable_daemon_catalog_preserves_the_live_worker_core_and_connection_details() {
    let (mut view, directory) = start_lost_presentation_fixture(LostFixtureState::Running);
    wait_for_lost_presentation(&mut view, |frame| {
        frame.contains("Working") && frame.contains(LOST_MODEL)
    });
    let before = lost_presentation_proof(directory.path());
    std::fs::write(
        directory.path().join(DAEMON_UNAVAILABLE_FLAG),
        "unavailable",
    )
    .unwrap();
    wait_for_lost_presentation(&mut view, |frame| {
        frame.contains(DAEMON_UNAVAILABLE_CAUSE) || frame.contains("Disconnected · cached view")
    });
    std::thread::sleep(Duration::from_millis(60));
    let frame = lost_presentation_frame(&view);
    println!("DAEMON_UNAVAILABLE_FRAME_BEGIN\n{frame}\nDAEMON_UNAVAILABLE_FRAME_END");
    let after = lost_presentation_proof(directory.path());
    assert_eq!(after["state"], "Running");
    assert!(
        after["live_snapshots_during_daemon_failure"]
            .as_u64()
            .unwrap()
            > 0,
        "the same worker still answers real Snapshot observations: {after}"
    );
    assert_eq!(after["commands"], before["commands"]);
    assert_eq!(after["model_calls"], before["model_calls"]);
    assert!(
        !frame.contains(DAEMON_UNAVAILABLE_CAUSE),
        "raw connection diagnosis belongs in details: {frame}"
    );
    assert!(
        frame.contains("⏵⏵ Auto")
            && frame.contains(LOST_MODEL)
            && frame.contains("Context unknown"),
        "{frame}"
    );
    assert!(
        frame.contains("Working") && frame.contains("Disconnected · cached view"),
        "{frame}"
    );
    assert!(
        !frame.contains("Lost ·") && !frame.contains("Execution connection was lost"),
        "the observed worker has not been lost: {frame}"
    );
    view.writer.write_all(b"\x0f").unwrap();
    view.writer.flush().unwrap();
    let details = wait_for_lost_presentation(&mut view, |frame| {
        frame.contains("Last connection diagnostic (this View)")
            && frame.contains(DAEMON_UNAVAILABLE_CAUSE)
    });
    assert!(
        details.contains(&format!("/resume {}", public(&before["chat"]))),
        "{details}"
    );
    view.writer.write_all(b"\x1b").unwrap();
    view.writer.flush().unwrap();
    std::fs::remove_file(directory.path().join(DAEMON_UNAVAILABLE_FLAG)).unwrap();
    let recovered = wait_for_lost_presentation(&mut view, |frame| {
        !frame.contains("Showing detailed transcript")
            && !frame.contains("Disconnected")
            && frame.contains("Working")
            && frame.contains(LOST_MODEL)
    });
    assert!(
        recovered.contains("⏵⏵ Auto") && recovered.contains("Context unknown"),
        "{recovered}"
    );
    let restored = lost_presentation_proof(directory.path());
    assert_eq!(restored["commands"], before["commands"]);
    assert_eq!(restored["model_calls"], before["model_calls"]);
}

/// A stored id from the fixture's proof as the Console shows it.
fn public(id: &serde_json::Value) -> String {
    tickloop_ids::base62::encode(id.as_str().unwrap().parse().unwrap())
}
