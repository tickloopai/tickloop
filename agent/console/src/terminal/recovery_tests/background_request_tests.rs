use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tickloop_agent_protocol::Content;

const BACKGROUND_FIXTURE_VARIABLE: &str = "TICKLOOP_BACKGROUND_REQUEST_FIXTURE";
const BACKGROUND_STAGE_FILE: &str = "background-request-stage";
const BACKGROUND_PHASE_FILE: &str = "background-request-phase.json";
const BACKGROUND_TASK: &str = "Approve background write";
const BACKGROUND_QUESTION_VARIABLE: &str = "TICKLOOP_BACKGROUND_QUESTION";
const BACKGROUND_PENDING_SHORT_VARIABLE: &str = "TICKLOOP_BACKGROUND_PENDING_SHORT";

struct AfterAnswerGate {
    requests: AtomicUsize,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    question_first: bool,
}

struct RecoveringBackgroundAccess {
    daemon: EmbeddedAgentAccess,
    background: Mutex<Option<ExecutionRef>>,
    unavailable: AtomicBool,
}

#[async_trait]
impl AgentAccess for RecoveringBackgroundAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if self.unavailable.load(Ordering::Relaxed)
            && matches!(&query, ObservationRequest::Agent {execution,detail:AgentDetailQuery::Snapshot,..}
                if Some(*execution) == *self.background.lock().unwrap())
        {
            return Err(AccessError::unavailable(
                "fixture background connection unavailable",
            ));
        }
        self.daemon.observe(query).await
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.daemon.execute(command).await
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.daemon.show_view(update).await
    }
}

#[async_trait]
impl ModelClient for AfterAnswerGate {
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
            profile: "background-request-fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "background-request-fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let index = self.requests.fetch_add(1, Ordering::Relaxed);
        let write = || {
            tickloop_agent_test_support::call(
                "background-write",
                "Write",
                serde_json::json!({"file_path":"approved.txt", "content":"fixture approved"}),
            )
        };
        let content = if index == 0 {
            if self.question_first {
                tickloop_agent_test_support::call(
                    "background-question",
                    "AskUserQuestion",
                    tickloop_agent_test_support::single_question("Which fixture?"),
                )
            } else {
                write()
            }
        } else {
            self.entered.add_permits(1);
            events
                .send(ModelDelta::Thinking("Still running after approval".into()))
                .unwrap();
            tokio::select! {
                permit = self.release.acquire() => {permit.unwrap().forget();},
                _ = cancel.cancelled() => {},
            }
            if self.question_first && index == 1 {
                write()
            } else {
                tickloop_agent_test_support::text("Background work finished")
            }
        };
        Ok(ModelOutput {
            private_source: None,
            stop_reason: if content
                .iter()
                .any(|part| matches!(part, Content::ToolUse { .. }))
            {
                "tool_use".into()
            } else {
                "end_turn".into()
            },
            content,
            usage: Usage::default(),
        })
    }
}

async fn wait_for_background_stage(stage: &str) {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if std::fs::read_to_string(BACKGROUND_STAGE_FILE).is_ok_and(|value| value == stage) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn background_phase(phase: &str, proof: serde_json::Value) {
    std::fs::write(
        BACKGROUND_PHASE_FILE,
        serde_json::to_vec(&serde_json::json!({"phase":phase,"proof":proof})).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "owned PTY producer for cross-View background request resolution"]
fn background_request_pty_fixture() {
    if std::env::var(BACKGROUND_FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("background-request-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_background_request_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_background_request_fixture() {
    let question_first = std::env::var(BACKGROUND_QUESTION_VARIABLE).as_deref() == Ok("1");
    let model = Arc::new(AfterAnswerGate {
        requests: AtomicUsize::new(0),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
        question_first,
    });
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        model.clone(),
    );
    let daemon = Arc::new(EmbeddedAgentAccess::new(core.clone()));
    let monitor_access = Arc::new(RecoveringBackgroundAccess {
        daemon: (*daemon).clone(),
        background: Mutex::new(None),
        unavailable: AtomicBool::new(false),
    });
    let workdir = std::env::current_dir().unwrap();
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(tickloop_agent_test_support::config());
    launch.session_persistence = false;
    let view = || ViewRef {
        session_id: Uuid::new_v4(),
        view_id: Uuid::new_v4(),
    };
    let monitor_view = view();
    let answerer_view = view();
    let monitor = Agent::from_access_for_test(
        monitor_access.clone(),
        daemon.daemon_epoch(),
        monitor_view,
        launch.clone(),
        None,
    );
    let answerer = Agent::from_access_for_test(
        daemon.clone(),
        daemon.daemon_epoch(),
        answerer_view,
        launch,
        None,
    );
    let visible = monitor.create_chat(workdir.clone()).await.unwrap();
    let background = monitor.create_chat(workdir.clone()).await.unwrap();
    *monitor_access.background.lock().unwrap() = Some(monitor.reference(background).await.unwrap());
    core.set_permission_rules(
        background,
        vec![tickloop_agent_protocol::PermissionRule {
            id: Uuid::new_v4(),
            decision: tickloop_agent_protocol::Decision::Ask,
            tool: "Write".into(),
            specifier: None,
            input: None,
            loop_id: None,
            persistent: false,
        }],
    )
    .await
    .unwrap();
    let mut daemon_events = core.subscribe();
    let (console, input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let startup = StartupState {
        launch_arguments: Vec::new(),
        chat: StartupChat::Resumed(Box::new(monitor.snapshot(visible).await.unwrap()), false),
        user_settings: None,
    };
    let interaction_agent = monitor.clone();
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
    wait_for_background_stage("start").await;
    answerer.display_chat(Some(background)).await.unwrap();
    answerer
        .submit(background, None, BACKGROUND_TASK.into())
        .await
        .unwrap();
    let request_id = loop {
        let event = tickloop_agent_test_support::next_event(&mut daemon_events).await;
        match event.kind {
            EventKind::PermissionRequested { request } if !question_first => break request.id,
            EventKind::QuestionAsked { request } if question_first => break request.id,
            EventKind::Failed(error) => panic!("background fixture failed: {error}"),
            _ => {}
        }
    };
    monitor.snapshot(background).await.unwrap();
    background_phase("requested", serde_json::json!({"request":request_id}));
    if std::env::var(BACKGROUND_PENDING_SHORT_VARIABLE).as_deref() == Ok("1") {
        answerer.display_chat(Some(visible)).await.unwrap();
        let started = answerer
            .submit(visible, None, "Visible initial input".into())
            .await
            .unwrap();
        model.entered.acquire().await.unwrap().forget();
        for _ in 0..2 {
            answerer
                .submit(visible, Some(started.loop_id), "Hello".into())
                .await
                .unwrap();
        }
        let snapshot = core.snapshot(visible).await.unwrap();
        assert_eq!(snapshot.queued_inputs().len(), 2);
        background_phase(
            "pending-preview",
            serde_json::json!({"pending":2,"question":question_first,"request":request_id}),
        );
        wait_for_background_stage("finish").await;
        interaction.await.unwrap().unwrap();
        drop(console);
        core.shutdown().await;
        return;
    }
    wait_for_background_stage("failed-answer").await;
    let rejected = if question_first {
        answerer
            .answer_question(
                background,
                request_id,
                vec![tickloop_agent_protocol::QuestionAnswer::option(
                    "Unknown fixture",
                )],
            )
            .await
    } else {
        answerer
            .answer_permission_with_scope(
                background,
                request_id,
                true,
                tickloop_agent_protocol::PermissionScope::Session,
            )
            .await
    };
    assert!(rejected.is_err());
    let snapshot = core.snapshot(background).await.unwrap();
    assert_eq!(snapshot.permissions.len() + snapshot.questions.len(), 1);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    background_phase("failed-answer", serde_json::json!({"pending":1}));
    wait_for_background_stage("answer").await;
    if question_first {
        answerer
            .answer_question(
                background,
                request_id,
                vec![tickloop_agent_protocol::QuestionAnswer::option("Weekly")],
            )
            .await
            .unwrap();
    } else {
        answerer
            .answer_permission(background, request_id, true)
            .await
            .unwrap();
    }
    tokio::time::timeout(RECOVERY_TIMEOUT, model.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let snapshot = core.snapshot(background).await.unwrap();
    assert!(snapshot.permissions.is_empty());
    assert!(snapshot.questions.is_empty());
    assert_eq!(
        snapshot.loops[0].state,
        tickloop_agent_protocol::LoopState::Running
    );
    background_phase(
        "answered",
        serde_json::json!({
            "permissions":snapshot.permissions.len(),"questions":snapshot.questions.len(),"state":snapshot.loops[0].state,
            "monitor":monitor_view,"answerer":answerer_view,
        }),
    );
    if question_first {
        wait_for_background_stage("next-request").await;
        monitor_access.unavailable.store(true, Ordering::Relaxed);
        model.release.add_permits(1);
        let next = loop {
            let event = tickloop_agent_test_support::next_event(&mut daemon_events).await;
            if let EventKind::PermissionRequested { request } = event.kind {
                break request;
            }
        };
        assert_ne!(request_id, next.id);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        background_phase(
            "next-request",
            serde_json::json!({"request":next.id,"unavailable":true}),
        );
        wait_for_background_stage("answer-next").await;
        assert!(
            answerer
                .answer_question(
                    background,
                    request_id,
                    vec![tickloop_agent_protocol::QuestionAnswer::option("Weekly")]
                )
                .await
                .is_err()
        );
        assert_eq!(
            core.snapshot(background).await.unwrap().permissions[0].id,
            next.id
        );
        monitor_access.unavailable.store(false, Ordering::Relaxed);
        answerer
            .answer_permission(background, next.id, true)
            .await
            .unwrap();
        tokio::time::timeout(RECOVERY_TIMEOUT, model.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let snapshot = core.snapshot(background).await.unwrap();
        assert!(snapshot.permissions.is_empty());
        assert_eq!(
            snapshot.loops[0].state,
            tickloop_agent_protocol::LoopState::Running
        );
        background_phase(
            "answered-next",
            serde_json::json!({"permissions":0,"state":snapshot.loops[0].state}),
        );
    }
    wait_for_background_stage("finish").await;
    model.release.add_permits(1);
    interaction.await.unwrap().unwrap();
    drop(console);
    core.shutdown().await;
}

#[test]
fn another_view_resolves_a_background_permission_while_its_run_keeps_working() {
    run_background_request_pty(false);
}

#[test]
fn resolving_a_background_question_keeps_the_next_request_until_its_own_answer() {
    run_background_request_pty(true);
}

fn start_background_request_pty(
    question_first: bool,
    pending_short: bool,
) -> (RecoveryPty, tempfile::TempDir) {
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
        "terminal::recovery_tests::background_request_tests::background_request_pty_fixture",
        "--nocapture",
    ]);
    command.env(BACKGROUND_FIXTURE_VARIABLE, "1");
    command.env(
        BACKGROUND_PENDING_SHORT_VARIABLE,
        if pending_short { "1" } else { "0" },
    );
    command.env(
        BACKGROUND_QUESTION_VARIABLE,
        if question_first { "1" } else { "0" },
    );
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
        timeout: RECOVERY_TIMEOUT,
    };
    fixture.wait_for("Enter a message to");
    (fixture, directory)
}

fn run_background_request_pty(question_first: bool) {
    let (mut fixture, directory) = start_background_request_pty(question_first, false);
    let stage =
        |stage| std::fs::write(directory.path().join(BACKGROUND_STAGE_FILE), stage).unwrap();
    let phase_path = directory.path().join(BACKGROUND_PHASE_FILE);
    stage("start");
    fixture.wait_for_phase(&phase_path, "requested");
    let waiting = if question_first {
        "waiting for your answer"
    } else {
        "waiting for approval"
    };
    fixture.wait_for(waiting);
    stage("failed-answer");
    fixture.wait_for_phase(&phase_path, "failed-answer");
    fixture.repaint_after_recovery("failed answer remains pending", |frame| {
        frame.contains(waiting)
    });
    stage("answer");
    let proof = fixture.wait_for_phase(&phase_path, "answered");
    assert_eq!(proof["proof"]["permissions"], 0);
    assert_eq!(proof["proof"]["questions"], 0);
    assert_ne!(proof["proof"]["monitor"], proof["proof"]["answerer"]);
    let frame = fixture.repaint_after_recovery("resolved background approval", |frame| {
        !frame.contains(waiting)
    });
    println!("BACKGROUND_REQUEST_PROOF {proof}\nBACKGROUND_REQUEST_FRAME {frame}");
    if question_first {
        stage("next-request");
        fixture.wait_for_phase(&phase_path, "next-request");
        fixture.wait_for("waiting for approval");
        stage("answer-next");
        let proof = fixture.wait_for_phase(&phase_path, "answered-next");
        assert_eq!(proof["proof"]["permissions"], 0);
        fixture.repaint_after_recovery("next request resolved without ending the run", |frame| {
            !frame.contains("waiting for approval") && !frame.contains("waiting for your answer")
        });
    }
    finish_background_request_pty(&mut fixture, directory.path());
}

fn finish_background_request_pty(fixture: &mut RecoveryPty, directory: &std::path::Path) {
    std::fs::write(directory.join(BACKGROUND_STAGE_FILE), "finish").unwrap();
    fixture.send(b"/exit\r");
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(status.success(), "{}", fixture.raw_output());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not close: {}",
            fixture.raw_output()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn actual_typed_background_wait_keeps_its_fact_when_short_pending_count_cannot_fit() {
    for question_first in [false, true] {
        let (mut fixture, directory) = start_background_request_pty(question_first, true);
        std::fs::write(directory.path().join(BACKGROUND_STAGE_FILE), "start").unwrap();
        let proof = fixture.wait_for_phase(
            &directory.path().join(BACKGROUND_PHASE_FILE),
            "pending-preview",
        );
        assert_eq!(proof["proof"]["pending"], 2);
        fixture.wait_until_shown("two queued inputs", |frame| {
            screen::queued_inputs(frame).len() == 2
        });
        let after = fixture.output.lock().unwrap().len();
        fixture
            ._master
            .resize(PtySize {
                rows: 4,
                cols: 24,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        fixture.send(b"\x0c");
        let waiting = if question_first {
            "waiting for your answer"
        } else {
            "waiting for approval"
        };
        let deadline = Instant::now() + RECOVERY_TIMEOUT;
        let frame = loop {
            let raw = fixture.raw_output();
            let frame = screen::visible_screen(&raw, 24, 4);
            if raw
                .rfind(SYNCHRONIZED_OUTPUT_END)
                .is_some_and(|end| end + SYNCHRONIZED_OUTPUT_END.len() > after)
                && frame.contains(waiting)
                && frame.contains('❯')
                && !frame.contains("Hello")
            {
                break frame;
            }
            assert!(
                Instant::now() < deadline,
                "short typed wait was not drawn: {raw}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!frame.contains("Pending"), "{frame}");
        assert!(!frame.contains("Approval"), "{frame}");
        println!("SHORT_TYPED_WAIT_PROOF {proof}\nSHORT_TYPED_WAIT_FRAME {frame}");
        finish_background_request_pty(&mut fixture, directory.path());
    }
}
