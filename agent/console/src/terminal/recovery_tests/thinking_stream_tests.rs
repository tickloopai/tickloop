use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tickloop_agent_protocol::Content;

const THINKING_FIXTURE_VARIABLE: &str = "TICKLOOP_THINKING_STREAM_FIXTURE";
const THINKING_QUERY_FILE: &str = "thinking-queries";
const THINKING_STAGE_FILE: &str = "thinking-stage";
const THINKING_EXECUTIONS_FILE: &str = "thinking-executions.json";
const THINKING_TOOL_VARIABLE: &str = "TICKLOOP_THINKING_STREAM_TOOL";
const THINKING_RESTORE_VARIABLE: &str = "TICKLOOP_THINKING_STREAM_RESTORE";
const THINKING_FLOOD_VARIABLE: &str = "TICKLOOP_THINKING_STREAM_FLOOD";
const THINKING_RECOVERIES_FILE: &str = "thinking-recoveries";
const FLOOD_DELTAS: usize = 8192;
const THINKING_PROGRESS_FILE: &str = "thinking-progress.json";
const HIDDEN_THINKING: &str = "hidden analysis stays in the archive";
const LATEST_PREVIEW: &str = "Latest thinking 🙂";
const STREAM_REPLY: &str = "Thinking stream completed";
const BURST_CHARACTERS: usize = 512;
const STATUS_QUERY_SETTLING_ALLOWANCE: usize = 4;
const STATUS_QUERY_BUDGET_PER_SECOND: f64 = 8.0;

struct ThinkingBurst {
    requests: AtomicUsize,
}

#[async_trait]
impl ModelClient for ThinkingBurst {
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
            profile: "thinking-fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "thinking-fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if self.requests.fetch_add(1, Ordering::Relaxed) > 0 {
            events.send(ModelDelta::Text(STREAM_REPLY.into())).unwrap();
            return Ok(ModelOutput {
                private_source: None,
                content: tickloop_agent_test_support::text(STREAM_REPLY),
                usage: Usage {
                    input_tokens: Some(37),
                    output_tokens: Some(11),
                    ..Usage::default()
                },
                stop_reason: "end_turn".into(),
            });
        }
        events.send(ModelDelta::Thinking("α".into())).unwrap();
        wait_for_stage("burst", &cancel).await;
        let restore = std::env::var(THINKING_RESTORE_VARIABLE).as_deref() == Ok("1");
        let flood = std::env::var(THINKING_FLOOD_VARIABLE).as_deref() == Ok("1");
        if flood {
            for _ in 0..FLOOD_DELTAS {
                events.send(ModelDelta::Thinking("思".into())).unwrap();
                tokio::task::yield_now().await;
            }
        }
        events
            .send(ModelDelta::Thinking(if restore {
                format!("\n{HIDDEN_THINKING}")
            } else {
                format!("{}\n{LATEST_PREVIEW}", "x".repeat(BURST_CHARACTERS))
            }))
            .unwrap();
        write_thinking_progress("burst");
        wait_for_stage("follow-up", &cancel).await;
        events.send(ModelDelta::Thinking("続".into())).unwrap();
        write_thinking_progress("follow-up");
        wait_for_stage("finish", &cancel).await;
        if !restore {
            events
                .send(ModelDelta::Thinking(format!(
                    "{}\nfinal preview",
                    "z".repeat(BURST_CHARACTERS)
                )))
                .unwrap();
        }
        if std::env::var(THINKING_TOOL_VARIABLE).as_deref() == Ok("1") {
            let mut content = tickloop_agent_test_support::call(
                "thinking-read",
                "Read",
                serde_json::json!({"file_path":"thinking-input.txt"}),
            );
            if restore {
                content.insert(
                    0,
                    Content::Thinking {
                        thinking: format!("α\n{HIDDEN_THINKING}続"),
                        signature: None,
                    },
                );
            }
            return Ok(ModelOutput {
                private_source: None,
                content,
                usage: Usage {
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                    ..Usage::default()
                },
                stop_reason: "tool_use".into(),
            });
        }
        events
            .send(ModelDelta::Text(format!("{STREAM_REPLY}\n")))
            .unwrap();
        let mut content = tickloop_agent_test_support::text(STREAM_REPLY);
        if flood {
            content.insert(
                0,
                Content::Thinking {
                    thinking: format!(
                        "α{}{}\n{LATEST_PREVIEW}続{}\nfinal preview",
                        "思".repeat(FLOOD_DELTAS),
                        "x".repeat(BURST_CHARACTERS),
                        "z".repeat(BURST_CHARACTERS)
                    ),
                    signature: None,
                },
            );
        }
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

async fn wait_for_stage(stage: &str, cancel: &CancellationToken) {
    loop {
        if std::fs::read_to_string(THINKING_STAGE_FILE).is_ok_and(|value| value == stage) {
            return;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(5)) => {},
        }
    }
}

fn write_thinking_progress(phase: &str) {
    std::fs::write(
        THINKING_PROGRESS_FILE,
        serde_json::to_vec(&serde_json::json!({"phase":phase})).unwrap(),
    )
    .unwrap();
}

struct MeasuredThinkingAccess {
    daemon: EmbeddedAgentAccess,
    task_queries: AtomicUsize,
}

#[async_trait]
impl AgentAccess for MeasuredThinkingAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if matches!(
            query,
            ObservationRequest::Agent {
                detail: AgentDetailQuery::Tasks,
                ..
            }
        ) {
            let queries = self.task_queries.fetch_add(1, Ordering::Relaxed) + 1;
            std::fs::write(THINKING_QUERY_FILE, queries.to_string()).unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
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

#[test]
#[ignore = "owned PTY producer for the Thinking stream performance regression"]
fn thinking_stream_pty_fixture() {
    if std::env::var(THINKING_FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("thinking-stream-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let core = tickloop_agent_runtime::Agent::new_embedded(tickloop_agent_test_support::config(), Arc::new(ThinkingBurst {requests: AtomicUsize::new(0)}));
                let mut events = core.subscribe();
                tokio::spawn(async move {
                    while let Ok(event) = events.recv().await {
                        if matches!(event.kind, EventKind::Completed {..}) {
                            write_thinking_progress("completed");
                            break;
                        }
                    }
                });
                let access = Arc::new(MeasuredThinkingAccess {
                    daemon: EmbeddedAgentAccess::new(core.clone()),
                    task_queries: AtomicUsize::new(0),
                });
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
                let (probe, mut facts) = mpsc::unbounded_channel();
                agent.set_recovery_probe(probe);
                std::fs::write(THINKING_RECOVERIES_FILE, "pending").unwrap();
                tokio::spawn(async move {
                    let mut recoveries = 0;
                    while let Some(fact) = facts.recv().await {
                        if matches!(fact, RecoveryFact::Lagged) {
                            recoveries += 1;
                        }
                        if matches!(fact, RecoveryFact::Completed { .. }) {
                            std::fs::write(THINKING_RECOVERIES_FILE, recoveries.to_string()).unwrap();
                        }
                    }
                });
                let chat = agent.create_chat(workdir.clone()).await.unwrap();
                let other = agent.create_chat(workdir.clone()).await.unwrap();
                std::fs::write(THINKING_EXECUTIONS_FILE, serde_json::to_vec(&serde_json::json!({"execution":agent.reference(chat).await.unwrap(),"other":agent.reference(other).await.unwrap()})).unwrap()).unwrap();
                std::fs::write("thinking-input.txt", "controlled tool input").unwrap();
                let (console, input) = Console::start(true).unwrap();
                console.set_prompt_enabled(true).unwrap();
                let startup = StartupState {
                    chat: StartupChat::Resumed(
                        Box::new(agent.snapshot(chat).await.unwrap()),
                        false,
                    ),
                                user_settings: None,
                };
                interact(&agent, workdir, &console, input, startup, None)
                    .await
                    .unwrap();
                core.shutdown().await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn thinking_bursts_follow_the_received_stream_without_requerying_each_character() {
    run_thinking_stream("live");
}

#[test]
fn a_tool_after_a_thinking_burst_does_not_wait_for_character_playback() {
    run_thinking_stream("tool");
}

#[test]
fn returning_to_an_agent_preserves_collapsed_thinking_tools_and_completion() {
    run_thinking_stream("restore");
}

#[test]
fn long_thinking_stream_does_not_lag_or_print_internal_recovery_notices() {
    run_thinking_stream("flood");
}

fn run_thinking_stream(route: &str) {
    let tool = matches!(route, "tool" | "restore");
    let restore = route == "restore";
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
        "terminal::recovery_tests::thinking_stream_tests::thinking_stream_pty_fixture",
        "--nocapture",
    ]);
    command.env(THINKING_FIXTURE_VARIABLE, "1");
    command.env(
        THINKING_FLOOD_VARIABLE,
        if route == "flood" { "1" } else { "0" },
    );
    command.env(THINKING_TOOL_VARIABLE, if tool { "1" } else { "0" });
    command.env(THINKING_RESTORE_VARIABLE, if restore { "1" } else { "0" });
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
    let query_count = || {
        std::fs::read_to_string(directory.path().join(THINKING_QUERY_FILE))
            .unwrap()
            .parse::<usize>()
            .unwrap()
    };
    let stage = |stage: &str| {
        std::fs::write(directory.path().join(THINKING_STAGE_FILE), stage).unwrap();
    };
    fixture.wait_for("Enter a message to");
    fixture.send(b"Continue the same Thinking stream.\r");
    fixture.wait_for("• α");
    if restore {
        run_thinking_restore(&mut fixture, directory.path());
        return;
    }
    let initial_queries = query_count();
    let started = Instant::now();
    stage("burst");
    fixture.timeout = RECOVERY_TIMEOUT;
    let frame = fixture.wait_for(LATEST_PREVIEW);
    let burst_elapsed = started.elapsed();
    let burst_queries = query_count() - initial_queries;
    println!(
        "THINKING_BURST characters={BURST_CHARACTERS} elapsed_ms={} task_queries={burst_queries}",
        burst_elapsed.as_millis()
    );
    assert!(
        burst_queries <= status_query_budget(burst_elapsed),
        "Thinking burst reread heavy state per character: elapsed_ms={} task_queries={burst_queries}\n{frame}",
        burst_elapsed.as_millis()
    );
    let idle_queries = query_count();
    let idle_started = Instant::now();
    std::thread::sleep(Duration::from_millis(200));
    let idle_elapsed = idle_started.elapsed();
    let animation_queries = query_count() - idle_queries;
    assert!(
        animation_queries <= status_query_budget(idle_elapsed),
        "animation reread heavy state {animation_queries} times in {} ms",
        idle_elapsed.as_millis()
    );
    if !tool {
        fixture.timeout = RECOVERY_TIMEOUT;
        let executions: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join(THINKING_EXECUTIONS_FILE)).unwrap(),
        )
        .unwrap();
        fixture.send(b"\x1b[D");
        fixture.open_overview_agent(&executions, "other");
        fixture.wait_until_shown("the other Agent", |frame| {
            !frame.contains("Agents · local") && !frame.contains(LATEST_PREVIEW)
        });
        let other_frame = screen::visible_screen(&fixture.raw_output(), 80, 24);
        assert!(
            !other_frame.contains(LATEST_PREVIEW),
            "another Agent inherited the cached Thinking activity: {other_frame}"
        );
        fixture.send(b"\x1b[D");
        fixture.open_overview_agent(&executions, "execution");
        fixture.wait_for(LATEST_PREVIEW);
        fixture.timeout = RECOVERY_TIMEOUT;
    }
    stage("follow-up");
    fixture.wait_for(&format!("{LATEST_PREVIEW}続"));
    stage("finish");
    if tool {
        fixture.wait_for("Read");
    }
    fixture.wait_for(STREAM_REPLY);
    fixture.wait_for_turn_summary();
    if route == "flood" {
        assert!(
            !fixture
                .raw_output()
                .contains("The view fell behind; restoring it from saved execution events.")
        );
        let proof = directory.path().join(THINKING_RECOVERIES_FILE);
        let deadline = Instant::now() + RECOVERY_TIMEOUT;
        while std::fs::read_to_string(&proof).unwrap() == "pending" {
            assert!(
                Instant::now() < deadline,
                "recovery probe did not observe completion"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            std::fs::read_to_string(&proof).unwrap(),
            "0",
            "a normal long Thinking stream triggered recovery"
        );
        fixture.send(b"\x0f");
        fixture.wait_until_shown("the complete Thinking archive", |frame| {
            frame.contains("Showing detailed transcript") && frame.contains("final preview")
        });
        fixture.send(b"\x1b");
        fixture.wait_for(STREAM_REPLY);
    }
    fixture.send(b"\x04\x04");
}

fn status_query_budget(elapsed: Duration) -> usize {
    STATUS_QUERY_SETTLING_ALLOWANCE
        + (elapsed.as_secs_f64() * STATUS_QUERY_BUDGET_PER_SECOND).ceil() as usize
}

fn run_thinking_restore(fixture: &mut RecoveryPty, directory: &std::path::Path) {
    let executions: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join(THINKING_EXECUTIONS_FILE)).unwrap())
            .unwrap();
    let progress = directory.join(THINKING_PROGRESS_FILE);
    fixture.send(b"\x1b[D");
    fixture.open_overview_agent(&executions, "other");
    fixture.wait_until_shown("the other Agent", |frame| {
        !frame.contains("Agents · local") && !frame.contains("Continue the same Thinking stream.")
    });
    for phase in ["burst", "follow-up", "finish"] {
        std::fs::write(directory.join(THINKING_STAGE_FILE), phase).unwrap();
        fixture.wait_for_phase(
            &progress,
            if phase == "finish" {
                "completed"
            } else {
                phase
            },
        );
    }
    fixture.send(b"\x1b[D");
    fixture.open_overview_agent(&executions, "execution");
    fixture.wait_for(STREAM_REPLY);
    let frame = fixture.repaint_after_recovery("restored Thinking stream", |frame| {
        frame.contains(STREAM_REPLY)
    });
    println!("THINKING_RESTORE_FRAME\n{frame}");
    assert!(
        !frame.contains(HIDDEN_THINKING),
        "restored Thinking leaked into the conversation: {frame}"
    );
    assert_eq!(
        screen::message_flow(&frame)
            .matches("Continue the same Thinking stream.")
            .count(),
        1,
        "{frame}"
    );
    assert_eq!(frame.matches(STREAM_REPLY).count(), 1, "{frame}");
    let input = frame.find("Continue the same Thinking stream.").unwrap();
    let thinking = frame.find("Thinking (ctrl+o to expand)").unwrap();
    let tool = frame.find("Read").unwrap();
    let text = frame.find(STREAM_REPLY).unwrap();
    let done = frame.find("Done · ↓ 11 tokens").unwrap();
    assert!(
        input < thinking && thinking < tool && tool < text && text < done,
        "restored events changed order: {frame}"
    );
    fixture.send(b"\x0f");
    fixture.wait_until_shown("the full Thinking archive", |frame| {
        frame.contains("Showing detailed transcript")
            && frame.contains(HIDDEN_THINKING)
            && frame.contains('続')
    });
    fixture.send(b"\x1b");
    fixture.wait_until_shown("the collapsed conversation again", |frame| {
        !frame.contains("Showing detailed transcript") && frame.contains(STREAM_REPLY)
    });
    let after = screen::visible_screen(&fixture.raw_output(), 80, 24);
    assert!(!after.contains(HIDDEN_THINKING), "{after}");
    fixture.send(b"\x04\x04");
}
