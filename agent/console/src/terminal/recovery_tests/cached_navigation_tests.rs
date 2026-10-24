use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tickloop_agent_protocol::{AgentFilter, Content, Page};

const FIXTURE_VARIABLE: &str = "TICKLOOP_CACHED_NAVIGATION_FIXTURE";
const SOURCE_TASK: &str = "NAV-SOURCE-TASK";
const TARGET_TASK: &str = "NAV-TARGET-TASK";
const SOURCE_REPLY: &str = "Source conversation reply";
const TARGET_REPLY: &str = "Target conversation reply";
const ADVANCED_REPLY: &str = "Target newer conversation reply";
const SOURCE_DRAFT: &str = "source-local-draft";
const TARGET_DRAFT: &str = "target-local-draft";
const FOLLOW_UP: &str = "NAV-EDITOR-FOLLOW-UP";
const FOLLOW_UP_REPLY: &str = "Editor follow-up received";
const RESTORE_ROUTE: &str = "restore";
const DISPLAY_ROUTE: &str = "display";

struct RejectedNavigationAccess {
    daemon: EmbeddedAgentAccess,
    target: Mutex<Option<ExecutionRef>>,
    armed: AtomicBool,
    target_snapshots: AtomicUsize,
    rejected: AtomicUsize,
    submits: Mutex<Vec<(ExecutionRef, String)>>,
    route: String,
}

#[async_trait]
impl AgentAccess for RejectedNavigationAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let target_snapshot = matches!(&query, ObservationRequest::Agent {
            execution, detail: AgentDetailQuery::Snapshot, after: None,
        } if Some(*execution) == *self.target.lock().unwrap());
        let reject = self.route == RESTORE_ROUTE
            && self.armed.load(Ordering::SeqCst)
            && target_snapshot
            && self.target_snapshots.fetch_add(1, Ordering::SeqCst) == 1;
        let mut observation = self.daemon.observe(query).await?;
        if reject {
            self.armed.store(false, Ordering::SeqCst);
            self.rejected.fetch_add(1, Ordering::SeqCst);
            observation.initial = ObservationValue::Receipt(None);
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let submitted = match &command.command {
            AgentCommand::Submit {
                execution,
                input: UserInput::Text { text, .. },
                ..
            } => Some((*execution, text.clone())),
            _ => None,
        };
        let receipt = self.daemon.execute(command).await?;
        if receipt.error.is_none()
            && let Some(submitted) = submitted
        {
            self.submits.lock().unwrap().push(submitted);
        }
        Ok(receipt)
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        if self.route == DISPLAY_ROUTE
            && self.armed.load(Ordering::SeqCst)
            && matches!(&update, DisplayUpdate::Displayed { execution, .. }
                if Some(*execution) == *self.target.lock().unwrap())
        {
            self.armed.store(false, Ordering::SeqCst);
            self.rejected.fetch_add(1, Ordering::SeqCst);
            return Err(AccessError::invalid(
                "owned fixture rejected target display",
            ));
        }
        self.daemon.show_view(update).await
    }
}

#[test]
#[ignore = "owned PTY producer for rejected cached Agent navigation"]
fn cached_navigation_pty_fixture() {
    if std::env::var(FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("cached-navigation".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_cached_navigation_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn complete_input(
    access: &RejectedNavigationAccess,
    core: &tickloop_agent_runtime::Agent,
    execution: ExecutionRef,
    chat: Uuid,
    text: &str,
) {
    let accepted = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon.daemon_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                input: UserInput::Text {
                    text: text.into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    assert!(accepted.error.is_none(), "{accepted:?}");
    let Some(CommandResult::Submitted(receipt)) = accepted.result else {
        panic!("missing Submit receipt");
    };
    tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let snapshot = core.snapshot(chat.into()).await.unwrap();
            if snapshot.chat_loop.id == receipt.loop_id
                && snapshot.chat_loop.run_id == Some(receipt.run_id)
                && snapshot.chat_loop.state == tickloop_agent_protocol::LoopState::Idle
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_control(name: &str) {
    tokio::time::timeout(RECOVERY_TIMEOUT, async {
        while !std::path::Path::new(name).exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("consumer did not send {name}"));
}

async fn run_cached_navigation_fixture() {
    let route = std::env::var(ROUTE_VARIABLE).unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::text(SOURCE_REPLY),
        tickloop_agent_test_support::text(TARGET_REPLY),
        tickloop_agent_test_support::text(ADVANCED_REPLY),
        tickloop_agent_test_support::text(FOLLOW_UP_REPLY),
    ]));
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        model.clone(),
    );
    let access = Arc::new(RejectedNavigationAccess {
        daemon: EmbeddedAgentAccess::new(core.clone()),
        target: Mutex::new(None),
        armed: AtomicBool::new(false),
        target_snapshots: AtomicUsize::new(0),
        rejected: AtomicUsize::new(0),
        submits: Mutex::new(Vec::new()),
        route: route.clone(),
    });
    let workdir = std::env::current_dir().unwrap();
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(tickloop_agent_test_support::config());
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
    let source = agent.create_chat(workdir.clone()).await.unwrap();
    let target = agent.create_chat(workdir.clone()).await.unwrap();
    let source_execution = agent.reference(source).await.unwrap();
    let target_execution = agent.reference(target).await.unwrap();
    *access.target.lock().unwrap() = Some(target_execution);
    complete_input(&access, &core, source_execution, source, SOURCE_TASK).await;
    complete_input(&access, &core, target_execution, target, TARGET_TASK).await;
    let initial = agent.snapshot(source).await.unwrap();
    let (console, mut keyboard) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let (forward, input) = mpsc::channel(FEED_CAPACITY);
    let keyboard_forward = forward.clone();
    tokio::task::spawn_local(async move {
        while let Some(event) = keyboard.recv().await {
            if keyboard_forward.send(event).await.is_err() {
                break;
            }
        }
    });
    let (probe, mut facts) = mpsc::unbounded_channel();
    agent.set_recovery_probe(probe);
    let interaction_agent = agent.clone();
    let interaction_console = console.clone();
    let interaction = tokio::task::spawn_local(async move {
        interact(
            &interaction_agent,
            workdir,
            &interaction_console,
            input,
            StartupState {
                launch_arguments: Vec::new(),
                chat: StartupChat::Resumed(Box::new(initial), true),
                user_settings: None,
            },
            None,
        )
        .await
    });
    write_phase(
        serde_json::json!({"phase":"initial", "source":source_execution,"target":target_execution}),
    );
    wait_for_control("cache-target").await;
    forward
        .send(InputEvent::OpenAgent(target_execution.agent_id))
        .await
        .unwrap();
    next_fact(
        &mut facts,
        |fact| matches!(fact, RecoveryFact::AgentOpened { chat } if *chat == target),
    )
    .await;
    let cached_sequence = core.snapshot(target.into()).await.unwrap().last_sequence;
    write_phase(serde_json::json!({"phase":"target-cached"}));
    wait_for_control("return-source").await;
    forward
        .send(InputEvent::OpenAgent(source_execution.agent_id))
        .await
        .unwrap();
    next_fact(
        &mut facts,
        |fact| matches!(fact, RecoveryFact::AgentOpened { chat } if *chat == source),
    )
    .await;
    write_phase(serde_json::json!({"phase":"source-returned"}));
    wait_for_control("reject-navigation").await;
    // The target advances after its View cache was left; its retained event range is real.
    complete_input(
        &access,
        &core,
        target_execution,
        target,
        "NAV-TARGET-ADVANCE",
    )
    .await;
    let advanced_sequence = core.snapshot(target.into()).await.unwrap().last_sequence;
    assert!(advanced_sequence > cached_sequence);
    access.armed.store(true, Ordering::SeqCst);
    forward
        .send(InputEvent::OpenAgent(target_execution.agent_id))
        .await
        .unwrap();
    let rejected = next_fact(&mut facts, |fact| {
        matches!(fact, RecoveryFact::NavigationRejected { .. })
    })
    .await;
    write_phase(
        serde_json::json!({"phase":"rejected", "source":source_execution,"target":target_execution,"fact":rejected}),
    );
    tokio::time::timeout(RECOVERY_TIMEOUT, async {
        while !access
            .submits
            .lock()
            .unwrap()
            .iter()
            .any(|(_, text)| text == FOLLOW_UP)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let source_snapshot = core.snapshot(source.into()).await.unwrap();
    let target_snapshot = core.snapshot(target.into()).await.unwrap();
    let ObservationValue::Overview(rows) = access
        .daemon
        .observe(ObservationRequest::Overview {
            page: Page {
                offset: 0,
                limit: 20,
            },
            filter: AgentFilter::default(),
            after: None,
        })
        .await
        .unwrap()
        .initial
    else {
        panic!("missing actual catalog");
    };
    let displayed = rows
        .iter()
        .flat_map(|row| &row.views)
        .filter(|location| location.view == view)
        .map(|location| location.execution)
        .collect::<Vec<_>>();
    let accepted = access.submits.lock().unwrap().clone();
    let contains_follow_up = |snapshot: &tickloop_agent_protocol::ChatSnapshot| {
        snapshot.messages.iter().any(|message| {
            message.role == tickloop_agent_protocol::Role::User
                && message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::Text { text } if text == FOLLOW_UP))
        })
    };
    std::fs::write(std::env::var_os(PROOF_VARIABLE).unwrap(), serde_json::to_vec(&serde_json::json!({
        "source":source_execution,"target":target_execution,"source_chat":source,"target_chat":target,"view":view,"displayed":displayed,
        "accepted":accepted,"source_has_input":contains_follow_up(&source_snapshot),
        "target_has_input":contains_follow_up(&target_snapshot),"rejected":access.rejected.load(Ordering::SeqCst),
        "snapshot_reads":access.target_snapshots.load(Ordering::SeqCst),"fact":rejected,
        "cached_sequence":cached_sequence,"advanced_sequence":advanced_sequence,
    })).unwrap()).unwrap();
    write_phase(serde_json::json!({"phase":"submitted"}));
    interaction.await.unwrap().unwrap();
    core.shutdown().await;
}

#[test]
fn a_rejected_cached_restore_keeps_the_visible_editor_and_submit_on_the_source() {
    run_rejected_navigation_pty(RESTORE_ROUTE);
}

#[test]
fn a_rejected_display_registration_keeps_the_visible_editor_and_submit_on_the_source() {
    run_rejected_navigation_pty(DISPLAY_ROUTE);
}

fn run_rejected_navigation_pty(route: &str) {
    let directory = tempfile::tempdir_in(tickloop_agent_test_support::SHORT_TEMP_ROOT).unwrap();
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
        "terminal::recovery_tests::cached_navigation_tests::cached_navigation_pty_fixture",
        "--nocapture",
    ]);
    let phase = directory.path().join("phase.json");
    let proof = directory.path().join("proof.json");
    command.env(FIXTURE_VARIABLE, "1");
    command.env(ROUTE_VARIABLE, route);
    command.env(PHASE_VARIABLE, &phase);
    command.env(PROOF_VARIABLE, &proof);
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
    fixture.wait_for_phase(&phase, "initial");
    fixture.wait_for(SOURCE_REPLY);
    std::fs::write(directory.path().join("cache-target"), []).unwrap();
    fixture.wait_for_phase(&phase, "target-cached");
    fixture.wait_for(TARGET_REPLY);
    fixture.send(TARGET_DRAFT.as_bytes());
    fixture.wait_for(TARGET_DRAFT);
    std::fs::write(directory.path().join("return-source"), []).unwrap();
    fixture.wait_for_phase(&phase, "source-returned");
    fixture.wait_for(SOURCE_REPLY);
    fixture.send(SOURCE_DRAFT.as_bytes());
    fixture.wait_for(SOURCE_DRAFT);
    std::fs::write(directory.path().join("reject-navigation"), []).unwrap();
    let rejected = fixture.wait_for_phase(&phase, "rejected");
    let failed_frame = fixture.repaint_after_recovery("rejected navigation", |frame| {
        frame.contains(SOURCE_DRAFT) || frame.contains(TARGET_DRAFT)
    });
    fixture.send(b"\x15");
    fixture.send(format!("{FOLLOW_UP}\r").as_bytes());
    fixture.wait_for_phase(&phase, "submitted");
    fixture.wait_for(FOLLOW_UP_REPLY);
    let proof: serde_json::Value = serde_json::from_slice(&std::fs::read(proof).unwrap()).unwrap();
    println!(
        "NAVIGATION_REJECTED {rejected}\nNAVIGATION_PROOF {proof}\nNAVIGATION_FRAME_BEGIN\n{failed_frame}\nNAVIGATION_FRAME_END"
    );
    fixture.send(b"\x04\x04");
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(status.success(), "{}", fixture.raw_output());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "producer did not exit: {}",
            fixture.raw_output()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(proof["rejected"], 1);
    assert_eq!(proof["displayed"], serde_json::json!([proof["source"]]));
    assert_eq!(
        proof["fact"]["NavigationRejected"]["selected_chat"],
        proof["source_chat"]
    );
    assert_eq!(
        proof["fact"]["NavigationRejected"]["visible"],
        proof["source"]
    );
    assert_eq!(proof["source_has_input"], true);
    assert_eq!(proof["target_has_input"], false);
    let follow_ups = proof["accepted"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row[1] == FOLLOW_UP)
        .collect::<Vec<_>>();
    assert_eq!(follow_ups.len(), 1, "{proof}");
    assert_eq!(follow_ups[0][0], proof["source"]);
    assert!(
        failed_frame.contains(SOURCE_REPLY)
            && failed_frame.contains(SOURCE_DRAFT)
            && !failed_frame.contains(TARGET_REPLY)
            && !failed_frame.contains(TARGET_DRAFT),
        "a rejected navigation must keep its source body/editor aligned with the accepted source Submit: {failed_frame}\n{proof}"
    );
}
