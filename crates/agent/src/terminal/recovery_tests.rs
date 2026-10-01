use super::*;
use crate::{agent_access::*, model::*, runtime};
use async_trait::async_trait;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod screen {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/screen.rs"
    ));
}

const REPLY: &str = "child follow-up acknowledged";
const FIRST_REPLY_DELTA: &str = "child follow-";
const FIXTURE_VARIABLE: &str = "TICKLOOP_REPLY_RECOVERY_FIXTURE";
const ROUTE_VARIABLE: &str = "TICKLOOP_REPLY_RECOVERY_ROUTE";
const PHASE_VARIABLE: &str = "TICKLOOP_REPLY_RECOVERY_PHASE";
const PROOF_VARIABLE: &str = "TICKLOOP_REPLY_RECOVERY_PROOF";
const SYNCHRONIZED_OUTPUT_END: &str = "\x1b[?2026l";
const UNARMED_INPUT_FILE: &str = "unarmed-input";
const OVERVIEW_ROUTE: &str = "overview";
const TRANSCRIPT_ROUTE: &str = "transcript";
const PENDING_INPUT_ROUTE: &str = "pending";
const RESET_ROUTE: &str = "reset";
const RAW_TOOL_ROUTE: &str = "raw-tool";
const PLAIN_ROUTE: &str = "plain";

const RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const LAGGED_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const FIXTURE_STACK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, serde::Serialize)]
pub(super) enum RecoveryFact {
    Forwarded {
        chat: Uuid,
        sequence: u64,
    },
    OverviewOpened,
    AgentOpened {
        chat: Uuid,
    },
    Lagged,
    Buffered {
        chat: Uuid,
        run: Uuid,
        sequence: u64,
        complete_line: bool,
    },
    Restored {
        chat: Uuid,
        after: u64,
        last_sequence: u64,
        reset_required: bool,
    },
    Completed {
        chat: Uuid,
        run: Uuid,
        sequence: u64,
    },
    Skipped {
        chat: Uuid,
        run: Uuid,
        sequence: u64,
        completed: bool,
    },
    /// The View followed a live InputMerged event, after deciding whether it joins the block.
    MergeObserved {
        chat: Uuid,
        run: Uuid,
        sequence: u64,
        into_run: Uuid,
    },
}

struct GatedReply {
    release: Mutex<Option<oneshot::Receiver<()>>>,
    finish: Mutex<Option<oneshot::Receiver<()>>>,
    flood_acknowledgements: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl ModelClient for GatedReply {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let release = self.release.lock().unwrap().take();
        if let Some(release) = release {
            let finish = self.finish.lock().unwrap().take();
            events
                .send(ModelDelta::Text(if finish.is_some() {
                    FIRST_REPLY_DELTA.into()
                } else {
                    REPLY.into()
                }))
                .unwrap();
            release.await.unwrap();
            if let Some(finish) = finish {
                events
                    .send(ModelDelta::Text(REPLY[FIRST_REPLY_DELTA.len()..].into()))
                    .unwrap();
                finish.await.unwrap();
            }
        } else {
            for _ in 0..REPLAY_CAPACITY + 1 {
                events
                    .send(ModelDelta::Thinking("background".into()))
                    .unwrap();
                self.flood_acknowledgements
                    .acquire()
                    .await
                    .unwrap()
                    .forget();
            }
        }
        Ok(ModelOutput {
            private_source: None,
            content: test_support::text(REPLY),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

struct DelayedTerminalEvents {
    owner: EmbeddedAgentAccess,
    release: CancellationToken,
}

#[async_trait]
impl AgentAccess for DelayedTerminalEvents {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let mut observation = self.owner.observe(query).await?;
        if let Some(mut changes) = observation.changes.take() {
            let (forward, receiver) = mpsc::channel(FEED_CAPACITY);
            let release = self.release.clone();
            tokio::spawn(async move {
                loop {
                    let update = tokio::select! {
                        _ = forward.closed() => break,
                        update = changes.recv() => match update {
                            Some(update) => update,
                            None => break,
                        },
                    };
                    if matches!(&update.change, ObservationChange::Event(event)
                        if matches!(event.kind, EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted))
                    {
                        tokio::select! {
                            _ = forward.closed() => break,
                            _ = release.cancelled() => {},
                        }
                    }
                    if forward.send(update).await.is_err() {
                        break;
                    }
                }
            });
            observation.changes = Some(receiver);
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.owner.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.owner.show_view(update).await
    }
}

#[test]
#[ignore = "owned PTY producer for the terminal-recovery caller regression"]
fn completed_reply_recovery_pty_fixture() {
    if std::env::var(FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("reply-recovery-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_completed_reply_recovery_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_completed_reply_recovery_fixture() {
    let (release_model, gate) = oneshot::channel();
    let route = std::env::var(ROUTE_VARIABLE).unwrap();
    let (finish_model, finish) = oneshot::channel();
    let flood_acknowledgements = Arc::new(tokio::sync::Semaphore::new(0));
    let core = runtime::Agent::new_embedded(
        test_support::config(),
        Arc::new(GatedReply {
            release: Mutex::new(Some(gate)),
            finish: Mutex::new(if route == "running" {
                Some(finish)
            } else {
                None
            }),
            flood_acknowledgements: flood_acknowledgements.clone(),
        }),
    );
    let workdir = std::env::current_dir().unwrap();
    let adapter = Arc::new(DelayedTerminalEvents {
        owner: EmbeddedAgentAccess::new(core.clone()),
        release: CancellationToken::new(),
    });
    let view = ViewRef {
        session_id: Uuid::new_v4(),
        view_id: Uuid::new_v4(),
    };
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(test_support::config());
    launch.session_persistence = false;
    let agent = Agent::from_access_for_test(
        adapter.clone(),
        adapter.owner.owner_epoch(),
        view,
        launch,
        None,
    );
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let execution = agent.reference(chat).await.unwrap();
    let other_chat = if route == "navigation" || route == "lagged" {
        Some(agent.create_chat(workdir.clone()).await.unwrap())
    } else {
        None
    };
    let other_execution = if let Some(other_chat) = other_chat {
        Some(agent.reference(other_chat).await.unwrap())
    } else {
        None
    };
    let mut owner_events = core.subscribe();
    let checkpoint = Arc::new(tokio::sync::Semaphore::new(0));
    if route == "lagged" {
        agent.hold_recovery_checkpoint(checkpoint.clone());
    }
    let (probe, mut facts) = mpsc::unbounded_channel();
    agent.set_recovery_probe(probe);
    let (console, input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let startup = StartupState {
        chat: StartupChat::Resumed(Box::new(agent.snapshot(chat).await.unwrap()), false),
        explicit_provider: None,
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
        panic!("expected actual caller Text-buffered fact: {buffered:?}");
    };
    assert_eq!(buffered_chat, chat);
    assert!(!complete_line);
    let running = core.snapshot(chat).await.unwrap();
    assert_eq!(running.loops[0].run_id, Some(run));
    assert_eq!(running.loops[0].state, runtime::LoopState::Running);
    assert!(running.events.iter().any(|event| event.sequence == sequence
        && event.run_id == run
        && matches!(&event.kind, EventKind::Text(text) if text == if route=="running" {FIRST_REPLY_DELTA}else{REPLY})));
    if route == OVERVIEW_ROUTE || route == "navigation" || route == "running" {
        write_phase(
            serde_json::json!({"phase":"buffered","execution":execution,"other":other_execution}),
        );
        console.write("\nBuffered reply ready\n").unwrap();
        next_fact(&mut facts, |fact| {
            matches!(fact, RecoveryFact::OverviewOpened)
        })
        .await;
        if route == "navigation" {
            write_phase(
                serde_json::json!({"phase":"open-other","execution":execution,"other":other_execution}),
            );
            next_fact(
                &mut facts,
                |fact| matches!(fact,RecoveryFact::AgentOpened {chat} if Some(*chat)==other_chat),
            )
            .await;
        }
    }
    if route == "interrupted" {
        agent.interrupt(chat).await.unwrap();
    } else {
        release_model.send(()).unwrap();
    }
    if route != "global" && route != "interrupted" {
        tokio::time::timeout(RECOVERY_TIMEOUT, async {
            loop {
                let event = owner_events.recv().await.unwrap();
                if event.chat_id == chat
                    && event.run_id == run
                    && if route == "running" {
                        matches!(event.kind, EventKind::Text(_)) && event.sequence > sequence
                    } else {
                        matches!(event.kind, EventKind::Completed { .. })
                    }
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        if route == "lagged" {
            let background = adapter
                .execute(CommandEnvelope {
                    owner_epoch: adapter.owner.owner_epoch(),
                    command_id: Uuid::new_v4(),
                    command: AgentCommand::Submit {
                        execution: other_execution.unwrap(),
                        target_loop: None,
                        input: UserInput::Text {
                            text: "bounded background traffic".into(),
                            contexts: Vec::new(),
                            paths: Vec::new(),
                        },
                        origin: None,
                    },
                })
                .await
                .unwrap();
            let Some(CommandResult::Submitted(background)) = background.result else {
                panic!("{background:?}");
            };
            let mut forwarded = 0;
            while forwarded < REPLAY_CAPACITY + 1 {
                let fact = next_fact(&mut facts, |fact| matches!(fact,RecoveryFact::Forwarded {chat,..} if Some(*chat)==other_chat)).await;
                if matches!(fact, RecoveryFact::Forwarded {sequence,..} if sequence > 1) {
                    forwarded += 1;
                    flood_acknowledgements.add_permits(1);
                }
            }
            assert_ne!(background.chat_id, chat);
            checkpoint.add_permits(1);
            next_fact(&mut facts, |fact| matches!(fact, RecoveryFact::Lagged)).await;
        } else {
            write_phase(
                serde_json::json!({"phase":"return-primary","execution":execution,"other":other_execution}),
            );
        }
    }
    let restored = next_fact(&mut facts, |fact| matches!(fact,RecoveryFact::Restored {chat:restored_chat,..} if *restored_chat==chat)).await;
    let RecoveryFact::Restored {
        chat: restored_chat,
        after,
        last_sequence,
        reset_required,
    } = restored
    else {
        panic!("expected actual global recovery fact: {restored:?}");
    };
    assert_eq!(restored_chat, chat);
    assert!(!reset_required);
    assert_eq!(after, sequence);
    if route == "running" {
        let recovered = core.snapshot(chat).await.unwrap();
        assert_eq!(recovered.loops[0].state, runtime::LoopState::Running);
        assert_eq!(recovered.last_sequence, last_sequence);
        finish_model.send(()).unwrap();
        tokio::time::timeout(RECOVERY_TIMEOUT, async {
            loop {
                let event = owner_events.recv().await.unwrap();
                if event.chat_id == chat
                    && event.run_id == run
                    && matches!(event.kind, EventKind::Completed { .. })
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
    let completed = core.snapshot(chat).await.unwrap();
    assert_eq!(completed.loops.len(), 1);
    assert_eq!(completed.loops[0].id, running.loops[0].id);
    assert_eq!(completed.loops[0].run_id, Some(run));
    assert_eq!(
        completed.loops[0].state,
        if route == "interrupted" {
            runtime::LoopState::Interrupted
        } else {
            runtime::LoopState::Completed
        }
    );
    assert_eq!(
        last_sequence + u64::from(route == "running"),
        completed.last_sequence
    );
    assert!(completed.events.iter().any(|event| event.run_id == run
        && matches!(
            event.kind,
            EventKind::Completed { .. } | EventKind::Interrupted
        )));
    let directory = agent.catalog().await.unwrap();
    assert!(
        directory.iter().any(|row| row.execution == execution
            && row.views.iter().any(|display| display.view == view))
    );
    adapter.release.cancel();
    let skipped = next_fact_with_timeout(
        &mut facts,
        if route == "lagged" {
            LAGGED_DRAIN_TIMEOUT
        } else {
            RECOVERY_TIMEOUT
        },
        |fact| matches!(fact,RecoveryFact::Skipped {chat:skipped_chat,..} | RecoveryFact::Completed {chat:skipped_chat,..} if *skipped_chat==chat),
    )
    .await;
    assert!(
        matches!(skipped,RecoveryFact::Skipped {chat:received_chat,run:received_run,sequence:received_sequence,completed:terminal_completed} if terminal_completed==(route!="interrupted") && received_chat==chat && received_run==run && received_sequence==completed.last_sequence)
            || route == "running"
                && matches!(skipped,RecoveryFact::Completed {chat:received_chat,run:received_run,sequence:received_sequence} if received_chat==chat && received_run==run && received_sequence==completed.last_sequence),
        "the caller must process the delayed terminal event exactly at its owner cursor: {skipped:?}"
    );
    std::fs::write(
        std::env::var_os(PROOF_VARIABLE).unwrap(),
        serde_json::to_vec(&serde_json::json!({"route":route,"execution":execution,"view":view,"run":run,"buffered":buffered,"restored":restored,"skipped":skipped,"owner_last_sequence":completed.last_sequence})).unwrap(),
    ).unwrap();
    console.write("\nRecovery facts verified\n").unwrap();
    interaction.await.unwrap().unwrap();
    core.shutdown().await;
}

async fn next_fact(
    facts: &mut mpsc::UnboundedReceiver<RecoveryFact>,
    accepts: impl Fn(&RecoveryFact) -> bool,
) -> RecoveryFact {
    next_fact_with_timeout(facts, RECOVERY_TIMEOUT, accepts).await
}

async fn next_fact_with_timeout(
    facts: &mut mpsc::UnboundedReceiver<RecoveryFact>,
    timeout: Duration,
    accepts: impl Fn(&RecoveryFact) -> bool,
) -> RecoveryFact {
    tokio::time::timeout(timeout, async {
        loop {
            let fact = facts.recv().await.unwrap();
            if accepts(&fact) {
                break fact;
            }
        }
    })
    .await
    .unwrap()
}

fn write_phase(phase: serde_json::Value) {
    let path = PathBuf::from(std::env::var_os(PHASE_VARIABLE).unwrap());
    let temporary = path.with_extension("pending");
    std::fs::write(&temporary, serde_json::to_vec(&phase).unwrap()).unwrap();
    std::fs::rename(temporary, path).unwrap();
}

#[test]
fn completed_snapshot_recovery_keeps_the_issuing_view_reply_once() {
    run_recovery_pty("global");
}
#[test]
fn running_overview_return_merges_unseen_text_in_run_order() {
    run_recovery_pty("running");
}
#[test]
fn live_interrupted_snapshot_recovery_keeps_the_buffered_reply_once() {
    run_recovery_pty("interrupted");
}
#[test]
fn overview_return_keeps_the_issuing_view_reply_once() {
    run_recovery_pty(OVERVIEW_ROUTE);
}
#[test]
fn cached_agent_return_keeps_the_issuing_view_reply_once() {
    run_recovery_pty("navigation");
}
#[test]
fn lagged_stream_recovery_keeps_the_issuing_view_reply_once() {
    run_recovery_pty("lagged");
}

fn run_recovery_pty(route: &str) {
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
        "terminal::recovery_tests::completed_reply_recovery_pty_fixture",
        "--nocapture",
    ]);
    command.env(FIXTURE_VARIABLE, "1");
    command.env(ROUTE_VARIABLE, route);
    let phase_path = directory.path().join("recovery-phase.json");
    command.env(PHASE_VARIABLE, &phase_path);
    command.env(PROOF_VARIABLE, directory.path().join("recovery-proof.json"));
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
    let mut fixture = RecoveryPty {
        child,
        writer,
        output,
        _master: pair.master,
        timeout: if route == "lagged" {
            LAGGED_DRAIN_TIMEOUT
        } else {
            RECOVERY_TIMEOUT
        },
    };
    fixture.wait_for("Enter a message to");
    fixture
        .writer
        .write_all(b"Continue the same bounded cadence task.\r")
        .unwrap();
    fixture.writer.flush().unwrap();
    if route == OVERVIEW_ROUTE || route == "navigation" || route == "running" {
        fixture.wait_for("Buffered reply ready");
        fixture.send(b"\x1b[D");
        if route == "navigation" {
            let phase = fixture.wait_for_phase(&phase_path, "open-other");
            fixture.open_overview_agent(&phase, "other");
        }
        let phase = fixture.wait_for_phase(&phase_path, "return-primary");
        if route == OVERVIEW_ROUTE || route == "running" {
            fixture.send(b"\x1b[27u");
        } else {
            fixture.send(b"\x1b[D");
            fixture.open_overview_agent(&phase, "execution");
        }
    }
    let frame = fixture.wait_for("Recovery facts verified");
    let proof = std::fs::read_to_string(directory.path().join("recovery-proof.json")).unwrap();
    println!("RECOVERY_PROOF {proof}\nRECOVERY_FRAME_BEGIN\n{frame}\nRECOVERY_FRAME_END");
    assert_eq!(
        frame.matches(REPLY).count(),
        1,
        "terminal snapshot recovery must present the issuing View's buffered reply exactly once: {frame}\n{}",
        proof
    );
    fixture.writer.write_all(b"\x04\x04").unwrap();
    fixture.writer.flush().unwrap();
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "recovery producer failed: {}",
                fixture.raw_output()
            );
            break;
        }
        assert!(Instant::now() < deadline, "recovery producer did not close");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct RecoveryPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    timeout: Duration,
}

impl RecoveryPty {
    fn raw_output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }
    fn repaint_after_recovery(&mut self, label: &str, ready: impl Fn(&str) -> bool) -> String {
        let after = self.output.lock().unwrap().len();
        self.send(b"\x0c");
        let deadline = Instant::now() + self.timeout;
        loop {
            let raw = self.raw_output();
            if raw
                .rfind(SYNCHRONIZED_OUTPUT_END)
                .is_some_and(|end| end + SYNCHRONIZED_OUTPUT_END.len() > after)
            {
                let frame = screen::visible_screen(&raw, 80, 24);
                if ready(&frame) {
                    return frame;
                }
            }
            assert!(
                Instant::now() < deadline,
                "no fresh completed {label}: {raw}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait_for_phase(&mut self, path: &std::path::Path, phase: &str) -> serde_json::Value {
        let deadline = Instant::now() + RECOVERY_TIMEOUT;
        loop {
            if let Ok(bytes) = std::fs::read(path)
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && value["phase"] == phase
            {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "recovery fixture did not reach phase {phase}: {}",
                self.raw_output()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn open_overview_agent(&mut self, phase: &serde_json::Value, key: &str) {
        let _execution: ExecutionRef = serde_json::from_value(phase[key].clone()).unwrap();
        // Rows may cut long titles; the start is enough to find them.
        let title = if key == "other" {
            "New Agent"
        } else {
            "Continue the same"
        };
        self.wait_for("Project");
        let deadline = Instant::now() + RECOVERY_TIMEOUT;
        loop {
            let frame = screen::visible_screen(
                &String::from_utf8_lossy(&self.output.lock().unwrap()),
                80,
                24,
            );
            if let Some(selected) = frame
                .lines()
                .find(|line| line.trim_start().starts_with('›'))
            {
                if selected.contains(title) {
                    self.send(b"\r");
                    return;
                }
                let selected = selected.to_owned();
                // Never-used sessions are folded behind their group heading until Enter opens it.
                let lines = frame.lines().collect::<Vec<_>>();
                let target_is_above = lines
                    .iter()
                    .position(|line| line.contains(title))
                    .zip(
                        lines
                            .iter()
                            .position(|line| line.trim_start().starts_with('›')),
                    )
                    .is_some_and(|(target, cursor)| target < cursor);
                self.send(if target_is_above {
                    b"k"
                } else if selected.contains("▸ Empty") {
                    b"\r"
                } else {
                    b"j"
                });
                self.wait_until(|frame| {
                    frame
                        .lines()
                        .find(|line| line.trim_start().starts_with('›'))
                        .is_some_and(|line| line != selected)
                });
            }
            assert!(
                Instant::now() < deadline,
                "overview did not select {title}: {frame}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Like `wait_until`, but fails the test when `label` never shows.
    fn wait_until_shown(&mut self, label: &str, ready: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            let frame = screen::visible_screen(&self.raw_output(), 80, 24);
            if ready(&frame) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "never showed {label}: {}",
            screen::visible_screen(&self.raw_output(), 80, 24)
        );
    }

    fn wait_until(&mut self, ready: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            let frame = screen::visible_screen(
                &String::from_utf8_lossy(&self.output.lock().unwrap()),
                80,
                24,
            );
            if ready(&frame) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait_for(&mut self, marker: &str) -> String {
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            let frame = screen::visible_screen(
                &String::from_utf8_lossy(&self.output.lock().unwrap()),
                80,
                24,
            );
            if frame.contains(marker) {
                return frame;
            }
            let output = self.output.lock().unwrap().clone();
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "recovery producer exited: {}",
                String::from_utf8_lossy(&output)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = self.output.lock().unwrap().clone();
        panic!(
            "recovery producer did not show {marker}: {}",
            String::from_utf8_lossy(&output)
        );
    }
}

impl Drop for RecoveryPty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            if let Some(pid) = self.child.process_id() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            } else {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

struct GuardedReplies {
    replies: Mutex<std::collections::VecDeque<(String, oneshot::Receiver<()>, bool)>>,
}

#[async_trait]
impl ModelClient for GuardedReplies {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let (text, release, fails) = self.replies.lock().unwrap().pop_front().unwrap();
        events.send(ModelDelta::Text(text.clone())).unwrap();
        release.await.unwrap();
        if fails {
            return Err(ModelError::Configuration("controlled failed reply".into()));
        }
        Ok(ModelOutput {
            private_source: None,
            content: test_support::text(&text),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn recovery_buffers_follow_exact_run_outcomes_and_chat_reset_boundaries() {
    for (fails, resets) in [(false, false), (true, false), (false, true)] {
        let directory = tempfile::tempdir().unwrap();
        let (release_prior, prior_gate) = oneshot::channel();
        let (release_current, current_gate) = oneshot::channel();
        let (release_other, other_gate) = oneshot::channel();
        let agent = Agent::new_embedded(
            test_support::config(),
            Arc::new(GuardedReplies {
                replies: Mutex::new(std::collections::VecDeque::from([
                    ("prior reply".into(), prior_gate, fails),
                    ("running same Actor".into(), current_gate, false),
                    ("running other Actor".into(), other_gate, false),
                ])),
            }),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let other_chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let mut buffered = HashMap::new();
        let prior = agent.submit(chat, None, "prior goal".into()).await.unwrap();
        buffer_actual_text(&mut events, &prior, &mut buffered).await;
        release_prior.send(()).unwrap();
        loop {
            let event = test_support::next_event(&mut events).await;
            if event.run_id == prior.run_id
                && matches!(
                    event.kind,
                    EventKind::Completed { .. } | EventKind::Failed(_)
                )
            {
                break;
            }
        }
        let current = agent
            .submit(chat, None, "new running goal".into())
            .await
            .unwrap();
        let after = buffer_actual_text(&mut events, &current, &mut buffered).await;
        let other = agent
            .submit(other_chat, None, "other running goal".into())
            .await
            .unwrap();
        buffer_actual_text(&mut events, &other, &mut buffered).await;
        let (console, _input) = Console::start(false).unwrap();
        let observation = restore_view(
            &console,
            &agent,
            &mut ToolPresentation::new(true),
            chat,
            after + u64::from(resets),
            &mut buffered,
        )
        .await
        .unwrap();
        assert_eq!(observation.reset_required, resets);
        assert!(
            !buffered.contains_key(&prior.run_id),
            "the observed prior run must finish or discard its own buffer"
        );
        assert_eq!(
            buffered.contains_key(&current.run_id),
            !resets,
            "a successful/failed prior run must not finish this Actor's new running reply"
        );
        let retained_other = buffered.get_mut(&other.run_id).unwrap();
        assert_eq!(retained_other.chat_id, other_chat);
        assert!(
            !retained_other.has_complete_line(),
            "target recovery must leave the other Actor's incomplete reply alone"
        );
        retained_other.finish();
        assert_eq!(
            retained_other.next_line(),
            Some(("running other Actor".into(), true))
        );
        if let Some(retained_current) = buffered.get_mut(&current.run_id) {
            assert!(!retained_current.has_complete_line());
            retained_current.finish();
            assert_eq!(
                retained_current.next_line(),
                Some(("running same Actor".into(), true))
            );
        }
        release_current.send(()).unwrap();
        release_other.send(()).unwrap();
        agent.shutdown().await;
    }
}

async fn buffer_actual_text(
    events: &mut broadcast::Receiver<runtime::Event>,
    receipt: &Receipt,
    buffered: &mut HashMap<Uuid, ReplyLines>,
) -> u64 {
    loop {
        let event = test_support::next_event(events).await;
        if event.chat_id == receipt.chat_id
            && event.run_id == receipt.run_id
            && let EventKind::Text(text) = event.kind
        {
            let mut reply = ReplyLines::new(event.chat_id, None);
            reply.push(&text);
            assert!(!reply.has_complete_line());
            buffered.insert(event.run_id, reply);
            return event.sequence;
        }
    }
}

const MERGE_FIXTURE_VARIABLE: &str = "TICKLOOP_MERGE_BOUNDARY_FIXTURE";
const MERGE_ROUTE_VARIABLE: &str = "TICKLOOP_MERGE_BOUNDARY_ROUTE";
const MERGE_INPUTS: [&str; 3] = ["busy alpha", "busy bravo", "busy charlie"];
const ROUTINE_FIXTURE_VARIABLE: &str = "TICKLOOP_ROUTINE_RESTORE_FIXTURE";
const ROUTINE_INPUT: &str = "Keep 22222222-2222-4222-8222-222222222222 deadbeef";
const ROUTINE_REPLY: &str = "Raw 11111111-1111-4111-8111-111111111111\ndeadbeef /tmp/raw --resume 11111111-1111-4111-8111-111111111111";
const UNARMED_INPUT: &str = "Original 33333333-3333-4333-8333-333333333333 deadbeef\n/tmp/pending --ack 33333333-3333-4333-8333-333333333333";
const RAW_TOOL_CONTENT: &str = "44444444-4444-4444-8444-444444444444 deadbeef /tmp/tool --ack";

struct SavedInputObservation {
    owner: EmbeddedAgentAccess,
    pending_run: Uuid,
}

#[async_trait]
impl AgentAccess for SavedInputObservation {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let mut observation = self.owner.observe(query).await?;
        if std::path::Path::new(UNARMED_INPUT_FILE).exists()
            && let ObservationValue::Agent(snapshot) = &mut observation.initial
            && let Some(goal) = snapshot.conversation.loops.first()
        {
            snapshot.pending_inputs = vec![serde_json::from_value(serde_json::json!({
                "receipt": {"chat_id":snapshot.conversation.id,"loop_id":goal.id,"run_id":self.pending_run},
                "input":UNARMED_INPUT,"armed":false
            }))
            .unwrap()];
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.owner.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.owner.show_view(update).await
    }
}

#[test]
#[ignore = "owned PTY producer for routine first-input Overview recovery"]
fn routine_restore_pty_fixture() {
    if std::env::var(ROUTINE_FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("routine-restore-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let route = std::env::var(ROUTE_VARIABLE).unwrap_or_default();
                let mut replies = Vec::new();
                if route == RAW_TOOL_ROUTE {
                    std::fs::write("raw-tool.txt", RAW_TOOL_CONTENT).unwrap();
                    replies.push(test_support::call(
                        "raw-read",
                        "Read",
                        serde_json::json!({"file_path":"raw-tool.txt"}),
                    ));
                }
                replies.push(test_support::text(ROUTINE_REPLY));
                let expected_requests = replies.len();
                let model = Arc::new(test_support::ScriptedModel::new(replies));
                let core = runtime::Agent::new_embedded(test_support::config(), model.clone());
                let workdir = std::env::current_dir().unwrap();
                let access = EmbeddedAgentAccess::new(core.clone());
                let adapter = Arc::new(SavedInputObservation {
                    owner: access.clone(),
                    pending_run: Uuid::new_v4(),
                });
                let mut launch = LaunchSpec::embedded(workdir.clone());
                launch.model = Some(test_support::config());
                launch.session_persistence = false;
                let agent = Agent::from_access_for_test(
                    adapter,
                    access.owner_epoch(),
                    ViewRef {
                        session_id: Uuid::new_v4(),
                        view_id: Uuid::new_v4(),
                    },
                    launch,
                    None,
                );
                let (probe, mut facts) = mpsc::unbounded_channel();
                agent.set_recovery_probe(probe);
                if std::env::var(ROUTE_VARIABLE).as_deref() == Ok(PLAIN_ROUTE) {
                    let chat = agent.create_chat(workdir).await.unwrap();
                    let mut events = core.subscribe();
                    agent.submit(chat, None, ROUTINE_INPUT.into()).await.unwrap();
                    loop {
                        let event = test_support::next_event(&mut events).await;
                        if matches!(event.kind, EventKind::Completed { .. }) {
                            break;
                        }
                    }
                    std::fs::write(UNARMED_INPUT_FILE, "").unwrap();
                    let (console, _input) = Console::start(false).unwrap();
                    let after = core.snapshot(chat).await.unwrap().last_sequence;
                    let observation = restore_view(
                        &console,
                        &agent,
                        &mut ToolPresentation::new(true),
                        chat,
                        after,
                        &mut HashMap::new(),
                    )
                    .await
                    .unwrap();
                    println!("PIPE_RESTORE_PROOF {}", serde_json::json!({"chat":chat,"saved":observation.snapshot.pending_inputs[0],"goal":observation.snapshot.loops[0].id}));
                    assert_eq!(model.requests.lock().unwrap().len(), 1);
                    core.shutdown().await;
                    return;
                }
                let (console, input) = Console::start(true).unwrap();
                console.set_prompt_enabled(true).unwrap();
                let interaction_agent = agent.clone();
                let interaction_console = console.clone();
                let interaction = tokio::task::spawn_local(async move {
                    interact(
                        &interaction_agent,
                        workdir,
                        &interaction_console,
                        input,
                        StartupState {
                            chat: StartupChat::New,
                            explicit_provider: None,
                            user_settings: None,
                        },
                        None,
                    )
                    .await
                });
                let completed = next_fact(&mut facts, |fact| {
                    matches!(fact, RecoveryFact::Completed { .. })
                })
                .await;
                let RecoveryFact::Completed { chat, run, sequence } = completed else {
                    unreachable!()
                };
                let snapshot = core.snapshot(chat).await.unwrap();
                assert_eq!(snapshot.loops.len(), 1);
                assert_eq!(snapshot.loops[0].run_id, Some(run));
                assert_eq!(snapshot.loops[0].state, runtime::LoopState::Completed);
                write_phase(serde_json::json!({"phase":"completed","completed":completed,"goal":snapshot.loops[0].id}));
                let passes = if route == PENDING_INPUT_ROUTE || route == RESET_ROUTE {
                    1
                } else {
                    2
                };
                for pass in 1..=passes {
                    next_fact(&mut facts, |fact| {
                        matches!(fact, RecoveryFact::OverviewOpened)
                    })
                    .await;
                    let mut restored = next_fact(&mut facts, |fact| {
                        matches!(fact, RecoveryFact::Restored { chat: restored, .. } if *restored == chat)
                    })
                    .await;
                    assert!(matches!(restored, RecoveryFact::Restored {after,last_sequence,reset_required:false,..} if after == sequence && last_sequence == sequence));
                    if route == RESET_ROUTE {
                        let observation = restore_view(
                            &console,
                            &agent,
                            &mut ToolPresentation::new(false),
                            chat,
                            sequence + 1,
                            &mut HashMap::new(),
                        )
                        .await
                        .unwrap();
                        assert!(observation.reset_required);
                        restored = next_fact(&mut facts, |fact| {
                            matches!(fact, RecoveryFact::Restored { reset_required: true, .. })
                        })
                        .await;
                    }
                    console.set_auto_scroll(console.auto_scroll().unwrap()).unwrap();
                    let saved = agent.snapshot(chat).await.unwrap().pending_inputs;
                    {
                        let requests = model.requests.lock().unwrap();
                        assert_eq!(requests.len(), expected_requests);
                        assert!(format!("{:?}", requests[0].messages).contains(ROUTINE_INPUT));
                        if route == RAW_TOOL_ROUTE {
                            assert_eq!(format!("{:?}", requests[1].messages).matches(RAW_TOOL_CONTENT).count(), 1);
                        }
                    }
                    assert_eq!(core.snapshot(chat).await.unwrap().last_sequence, sequence);
                    assert!(core.snapshot(chat).await.unwrap().pending_inputs.is_empty());
                    write_phase(serde_json::json!({"phase":format!("restored-{pass}"),"restored":restored,"saved_inputs":saved,"model_requests":expected_requests}));
                }
                interaction.await.unwrap().unwrap();
                core.shutdown().await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn routine_overview_returns_after_first_input_do_not_append_identity_diagnostics() {
    run_routine_restore_pty("quiet");
}

#[test]
fn routine_restore_keeps_unarmed_input_manual_fact_and_raw_text() {
    run_routine_restore_pty(PENDING_INPUT_ROUTE);
}

#[test]
fn routine_restore_preserves_raw_user_model_and_read_tool_identifiers() {
    run_routine_restore_pty(RAW_TOOL_ROUTE);
}

#[test]
fn interactive_reset_keeps_the_retained_history_notice_and_raw_text() {
    run_routine_restore_pty(RESET_ROUTE);
}

#[test]
fn plain_restore_keeps_full_identity_diagnostics_and_unarmed_input_prefixes() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "terminal::recovery_tests::routine_restore_pty_fixture",
            "--nocapture",
        ])
        .env(ROUTINE_FIXTURE_VARIABLE, "1")
        .env(ROUTE_VARIABLE, PLAIN_ROUTE)
        .env("HOME", directory.path())
        .current_dir(directory.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}\n{:?}", output.stderr);
    let proof = stdout
        .lines()
        .find_map(|line| line.strip_prefix("PIPE_RESTORE_PROOF "))
        .unwrap();
    let proof: serde_json::Value = serde_json::from_str(proof).unwrap();
    let chat: Uuid = serde_json::from_value(proof["chat"].clone()).unwrap();
    let receipt: Receipt = serde_json::from_value(proof["saved"]["receipt"].clone()).unwrap();
    assert!(
        stdout.contains(&format!(
            "Chat {chat} restored. Current model: fixture · Messages"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "Goal: {} · Completed · {ROUTINE_INPUT}",
            receipt.loop_id
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "Saved input awaiting manual continuation: Goal {} · Run {}\n{UNARMED_INPUT}",
            receipt.loop_id, receipt.run_id
        )),
        "{stdout}"
    );
    println!("PLAIN_RESTORE_BEGIN\n{stdout}\nPLAIN_RESTORE_END");
}

fn run_routine_restore_pty(route: &str) {
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
        "terminal::recovery_tests::routine_restore_pty_fixture",
        "--nocapture",
    ]);
    let phase_path = directory.path().join("routine-phase.json");
    command.env(ROUTINE_FIXTURE_VARIABLE, "1");
    command.env(ROUTE_VARIABLE, route);
    command.env(PHASE_VARIABLE, &phase_path);
    command.env("TICKLOOP_AGENT_TUI", "fullscreen");
    command.env("TERM", "xterm-256color");
    command.env("HOME", directory.path());
    command.cwd(directory.path());
    let child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
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
        writer: pair.master.take_writer().unwrap(),
        output,
        _master: pair.master,
        timeout: RECOVERY_TIMEOUT,
    };
    fixture.wait_for("fixture");
    fixture.send(format!("{ROUTINE_INPUT}\r").as_bytes());
    let completed = fixture.wait_for_phase(&phase_path, "completed");
    fixture.wait_for("✓ Done");
    let original = screen::visible_screen(&fixture.raw_output(), 80, 24);
    let body = |frame: &str| {
        frame
            .lines()
            .take_while(|line| !line.starts_with('─'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned()
    };
    println!(
        "ROUTINE_FIRST_INPUT {completed}\nROUTINE_BEFORE_BEGIN\n{original}\nROUTINE_BEFORE_END"
    );
    let passes = if route == PENDING_INPUT_ROUTE || route == RESET_ROUTE {
        if route == PENDING_INPUT_ROUTE {
            std::fs::write(directory.path().join(UNARMED_INPUT_FILE), "").unwrap();
        }
        1
    } else {
        2
    };
    for pass in 1..=passes {
        fixture.send(b"\x1b[D");
        fixture.wait_for("Agents · local");
        fixture.send(b"\x1b[27u");
        let restored = fixture.wait_for_phase(&phase_path, &format!("restored-{pass}"));
        let frame = fixture.repaint_after_recovery("restored conversation", |frame| {
            !frame.contains("Agents · local")
                && if route == RESET_ROUTE {
                    frame.contains("The saved event range changed")
                } else {
                    frame.contains("✓ Done")
                }
        });
        println!(
            "ROUTINE_RESTORE {restored}\nROUTINE_AFTER_{pass}_BEGIN\n{frame}\nROUTINE_AFTER_{pass}_END"
        );
        if route == PENDING_INPUT_ROUTE {
            let saved = &restored["saved_inputs"][0];
            assert_eq!(saved["armed"], false);
            assert_eq!(saved["input"], UNARMED_INPUT);
            assert!(
                frame.contains("Saved input awaiting manual continuation:"),
                "{frame}"
            );
            for line in UNARMED_INPUT.lines() {
                assert!(frame.contains(line), "{frame}");
            }
            assert!(
                !frame.contains("Saved input awaiting manual continuation: Goal"),
                "{frame}"
            );
            let receipt: Receipt = serde_json::from_value(saved["receipt"].clone()).unwrap();
            assert!(
                !frame.contains(&format!("Run {}", receipt.run_id)),
                "{frame}"
            );
            assert!(
                !frame.contains(&format!("Goal: {}", receipt.loop_id)),
                "{frame}"
            );
        } else if route == RESET_ROUTE {
            assert_eq!(restored["restored"]["Restored"]["reset_required"], true);
            assert!(
                frame.contains(
                    "The saved event range changed; displaying the retained conversation."
                ),
                "{frame}"
            );
        } else {
            assert_eq!(
                body(&frame),
                body(&original),
                "routine return {pass} must not append conversation text"
            );
        }
        assert!(frame.contains(ROUTINE_INPUT), "{frame}");
        assert!(
            frame.contains("11111111-1111-4111-8111-111111111111"),
            "{frame}"
        );
        assert!(frame.contains("deadbeef /tmp/raw --resume"), "{frame}");
        if route == RAW_TOOL_ROUTE {
            assert_eq!(frame.matches(RAW_TOOL_CONTENT).count(), 1, "{frame}");
        }
    }
    fixture.send(b"\x04\x04");
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(status.success(), "{}", fixture.raw_output());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "routine restore producer did not close"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Holds each model request until the test releases it, in arrival order.
struct HeldRequests {
    model: Arc<test_support::ScriptedModel>,
    gates: Mutex<std::collections::VecDeque<oneshot::Receiver<()>>>,
    entered: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl ModelClient for HeldRequests {
    fn identity(&self) -> ModelIdentity {
        self.model.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let gate = self.gates.lock().unwrap().pop_front();
        let _ = self.entered.send(());
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        self.model.complete(request, events, cancel).await
    }
}

/// Forwards the first InputMerged change of each stream and holds every later change, in order,
/// until `release` is cancelled.
struct HeldMerges {
    owner: EmbeddedAgentAccess,
    release: CancellationToken,
}

#[async_trait]
impl AgentAccess for HeldMerges {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let mut observation = self.owner.observe(query).await?;
        if let Some(mut changes) = observation.changes.take() {
            let (forward, receiver) = mpsc::channel(FEED_CAPACITY);
            let release = self.release.clone();
            tokio::spawn(async move {
                let mut merged = false;
                let mut held = std::collections::VecDeque::new();
                loop {
                    let update = tokio::select! {
                        _ = forward.closed() => break,
                        _ = release.cancelled(), if !held.is_empty() => {
                            while let Some(update) = held.pop_front() {
                                if forward.send(update).await.is_err() {
                                    return;
                                }
                            }
                            continue;
                        },
                        update = changes.recv() => match update {
                            Some(update) => update,
                            None => break,
                        },
                    };
                    if merged && !release.is_cancelled() {
                        held.push_back(update);
                        continue;
                    }
                    merged |= matches!(&update.change, ObservationChange::Event(event)
                        if matches!(event.kind, EventKind::InputMerged { .. }));
                    if forward.send(update).await.is_err() {
                        break;
                    }
                }
            });
            observation.changes = Some(receiver);
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.owner.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.owner.show_view(update).await
    }
}

#[test]
#[ignore = "owned PTY producer for the merged-input View boundary regression"]
fn merge_boundary_pty_fixture() {
    if std::env::var(MERGE_FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("merge-boundary-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_merge_boundary_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn wait_for_file(name: &str) {
    tokio::time::timeout(RECOVERY_TIMEOUT, async {
        while !std::path::Path::new(name).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the test never created {name}"));
}

async fn run_merge_boundary_fixture() {
    let (release_first, first) = oneshot::channel();
    let (release_second, second) = oneshot::channel();
    let (entered, mut requests) = mpsc::unbounded_channel();
    let model = Arc::new(test_support::ScriptedModel::new(vec![
        test_support::call(
            "shell",
            "Bash",
            serde_json::json!({"command": "printf merged"}),
        ),
        test_support::text("answered all three"),
    ]));
    let core = runtime::Agent::new_embedded(
        test_support::config(),
        Arc::new(HeldRequests {
            model: model.clone(),
            gates: Mutex::new([first, second].into()),
            entered,
        }),
    );
    let workdir = std::env::current_dir().unwrap();
    let adapter = Arc::new(HeldMerges {
        owner: EmbeddedAgentAccess::new(core.clone()),
        release: CancellationToken::new(),
    });
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(test_support::config());
    launch.session_persistence = false;
    let agent = Agent::from_access_for_test(
        adapter.clone(),
        adapter.owner.owner_epoch(),
        ViewRef {
            session_id: Uuid::new_v4(),
            view_id: Uuid::new_v4(),
        },
        launch,
        None,
    );
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    core.set_execution_mode(chat, crate::permissions::ExecutionMode::FullAccess)
        .await
        .unwrap();
    let (probe, mut facts) = mpsc::unbounded_channel();
    agent.set_recovery_probe(probe);
    let (console, input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let startup = StartupState {
        chat: StartupChat::Resumed(Box::new(agent.snapshot(chat).await.unwrap()), false),
        explicit_provider: None,
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
    requests.recv().await.unwrap();
    write_phase(serde_json::json!({"phase": "busy"}));
    wait_for_file("release-model").await;
    release_first.send(()).unwrap();
    let first = next_fact(&mut facts, |fact| {
        matches!(fact, RecoveryFact::MergeObserved { .. })
    })
    .await;
    let RecoveryFact::MergeObserved {
        sequence: first_sequence,
        into_run,
        ..
    } = first
    else {
        unreachable!()
    };
    let merges = core
        .snapshot(chat)
        .await
        .unwrap()
        .events
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::InputMerged { into_run_id } => Some((event.sequence, into_run_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        merges,
        (0..3)
            .map(|offset| (first_sequence + offset, into_run))
            .collect::<Vec<_>>(),
        "the runtime merged the three inputs by consecutive events into one run"
    );
    write_phase(serde_json::json!({"phase": "merged-one"}));
    if std::env::var(MERGE_ROUTE_VARIABLE).as_deref() == Ok(OVERVIEW_ROUTE) {
        let restored = next_fact(&mut facts, |fact| {
            matches!(fact, RecoveryFact::Restored { chat: restored, .. } if *restored == chat)
        })
        .await;
        let last_sequence = core.snapshot(chat).await.unwrap().last_sequence;
        assert!(
            matches!(restored, RecoveryFact::Restored {after,last_sequence: restored_sequence,reset_required:false,..} if after == first_sequence && restored_sequence == last_sequence && restored_sequence >= first_sequence + 2)
        );
        console
            .set_auto_scroll(console.auto_scroll().unwrap())
            .unwrap();
        write_phase(
            serde_json::json!({"phase":"restored","restored":restored,"chat":chat,"run":into_run,"merges":merges,"last_sequence":last_sequence}),
        );
    }
    wait_for_file("release-merges").await;
    adapter.release.cancel();
    next_fact(&mut facts, |fact| {
        matches!(fact, RecoveryFact::MergeObserved { sequence, .. } if *sequence == first_sequence + 2)
    })
    .await;
    write_phase(serde_json::json!({"phase": "merged-all"}));
    wait_for_file("finish").await;
    requests.recv().await.unwrap();
    release_second.send(()).unwrap();
    tokio::time::timeout(RECOVERY_TIMEOUT, async {
        while !core
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .all(|goal| goal.state == runtime::LoopState::Completed)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let merged = {
        let requests = model.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "one tool round, then one request for all three"
        );
        format!("{:?}", requests[1].messages)
    };
    assert!(
        MERGE_INPUTS
            .iter()
            .all(|input| merged.matches(input).count() == 1),
        "{merged}"
    );
    console.write("\nMerge boundary verified\n").unwrap();
    interaction.await.unwrap().unwrap();
    core.shutdown().await;
}

#[test]
fn a_view_action_between_merged_inputs_keeps_them_apart_while_later_merges_still_join() {
    run_merge_boundary_pty(TRANSCRIPT_ROUTE);
}

#[test]
fn overview_return_preserves_merged_input_echo_boundaries_after_actual_restore() {
    run_merge_boundary_pty(OVERVIEW_ROUTE);
}

fn run_merge_boundary_pty(route: &str) {
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
        "terminal::recovery_tests::merge_boundary_pty_fixture",
        "--nocapture",
    ]);
    command.env(MERGE_FIXTURE_VARIABLE, "1");
    command.env(MERGE_ROUTE_VARIABLE, route);
    let phase_path = directory.path().join("merge-phase.json");
    command.env(PHASE_VARIABLE, &phase_path);
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
    let mut fixture = RecoveryPty {
        child,
        writer,
        output,
        _master: pair.master,
        timeout: RECOVERY_TIMEOUT,
    };
    fixture.wait_for("Enter a message to");
    fixture.send(b"start the busy work\r");
    fixture.wait_for_phase(&phase_path, "busy");
    for input in MERGE_INPUTS {
        fixture.send(format!("{input}\r").as_bytes());
        fixture.wait_for(input);
    }
    fixture.wait_for("Queued 3");
    std::fs::write(directory.path().join("release-model"), "").unwrap();
    fixture.wait_for_phase(&phase_path, "merged-one");
    let restored_frame = if route == OVERVIEW_ROUTE {
        fixture.send(b"\x1b[D");
        fixture.wait_for("Agents · local");
        fixture.send(b"\x1b[27u");
        let restored = fixture.wait_for_phase(&phase_path, "restored");
        let frame = fixture.repaint_after_recovery("Overview restore", |frame| {
            !frame.contains("Agents · local")
                && !frame.contains("Transcript ·")
                && frame.contains("busy charlie")
        });
        println!(
            "MERGE_RESTORE_PROOF {restored}\nMERGE_RESTORE_FRAME_BEGIN\n{frame}\nMERGE_RESTORE_FRAME_END"
        );
        Some(frame)
    } else {
        // A real View action between the first merge and the rest: Ctrl+O loads the transcript.
        fixture.send(b"\x0f");
        fixture.wait_until_shown("the loaded transcript", |frame| {
            frame.contains("Transcript ·")
                && frame.contains("You: busy charlie")
                && !frame.contains("Loading conversation")
        });
        None
    };
    std::fs::write(directory.path().join("release-merges"), "").unwrap();
    fixture.wait_for_phase(&phase_path, "merged-all");
    if route != OVERVIEW_ROUTE {
        fixture.send(b"\x1b");
    }
    fixture.wait_until_shown("the conversation again", |frame| {
        !frame.contains("Transcript ·") && frame.contains("busy charlie")
    });
    let frame = screen::visible_screen(&fixture.raw_output(), 80, 24);
    println!("MERGE_BOUNDARY_FRAME_BEGIN\n{frame}\nMERGE_BOUNDARY_FRAME_END");
    let lines = frame.lines().collect::<Vec<_>>();
    let row = |text: &str| {
        let rows = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(text))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "{text} is shown once: {frame}");
        rows[0]
    };
    let [alpha, bravo, charlie] = MERGE_INPUTS.map(row);
    assert!(
        lines[alpha].trim_start().starts_with('❯')
            && lines[bravo].trim_start().starts_with('❯')
            && lines[bravo - 1].trim().is_empty(),
        "an input merged after a View action starts its own block: {frame}"
    );
    if let Some(restored) = restored_frame {
        assert!(
            lines[charlie].trim_start().starts_with('❯') && lines[charlie - 1].trim().is_empty(),
            "a restored input keeps its own prefix and separator: {frame}"
        );
        assert_eq!(
            restored, frame,
            "late replayed merges must leave the restored frame unchanged"
        );
    } else {
        assert!(
            charlie == bravo + 1 && lines[charlie].starts_with("  busy charlie"),
            "merges that follow each other live still read as one block: {frame}"
        );
    }
    std::fs::write(directory.path().join("finish"), "").unwrap();
    fixture.wait_for("Merge boundary verified");
    fixture.send(b"\x04\x04");
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "merge boundary producer failed: {}",
                fixture.raw_output()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "merge boundary producer did not close"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
