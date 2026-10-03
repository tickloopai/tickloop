use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const THINKING_FIXTURE_VARIABLE: &str = "TICKLOOP_THINKING_STREAM_FIXTURE";
const THINKING_QUERY_FILE: &str = "thinking-queries";
const THINKING_STAGE_FILE: &str = "thinking-stage";
const THINKING_EXECUTIONS_FILE: &str = "thinking-executions.json";
const THINKING_TOOL_VARIABLE: &str = "TICKLOOP_THINKING_STREAM_TOOL";
const THINKING_RESTORE_VARIABLE: &str = "TICKLOOP_THINKING_STREAM_RESTORE";
const THINKING_PROGRESS_FILE: &str = "thinking-progress.json";
const HIDDEN_THINKING: &str = "hidden analysis stays in the archive";
const LATEST_PREVIEW: &str = "Latest thinking 🙂";
const STREAM_REPLY: &str = "Thinking stream completed";
const BURST_CHARACTERS: usize = 512;
const BURST_DEADLINE: Duration = Duration::from_millis(750);

struct ThinkingBurst {
    requests: AtomicUsize,
}

#[async_trait]
impl ModelClient for ThinkingBurst {
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
                content: test_support::text(STREAM_REPLY),
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
            let mut content = test_support::call(
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
        Ok(ModelOutput {
            private_source: None,
            content: test_support::text(STREAM_REPLY),
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
    owner: EmbeddedAgentAccess,
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
        self.owner.observe(query).await
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.owner.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.owner.show_view(update).await
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
                let core = runtime::Agent::new_embedded(test_support::config(), Arc::new(ThinkingBurst {requests: AtomicUsize::new(0)}));
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
                    owner: EmbeddedAgentAccess::new(core.clone()),
                    task_queries: AtomicUsize::new(0),
                });
                let workdir = std::env::current_dir().unwrap();
                let mut launch = LaunchSpec::embedded(workdir.clone());
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

fn run_thinking_stream(route: &str) {
    let tool = route != "live";
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
    fixture.timeout = BURST_DEADLINE;
    fixture.wait_until(|frame| frame.contains(LATEST_PREVIEW));
    let burst_elapsed = started.elapsed();
    let burst_queries = query_count() - initial_queries;
    let frame = screen::visible_screen(&fixture.raw_output(), 80, 24);
    println!(
        "THINKING_BURST characters={BURST_CHARACTERS} elapsed_ms={} task_queries={burst_queries}",
        burst_elapsed.as_millis()
    );
    assert!(
        frame.contains(LATEST_PREVIEW),
        "Thinking burst fell behind: elapsed_ms={} task_queries={burst_queries}\n{frame}",
        burst_elapsed.as_millis()
    );
    let idle_queries = query_count();
    std::thread::sleep(Duration::from_millis(200));
    let animation_queries = query_count() - idle_queries;
    assert!(
        animation_queries <= 1,
        "animation reread heavy state {animation_queries} times in 200 ms"
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
        fixture.timeout = BURST_DEADLINE;
    }
    stage("follow-up");
    fixture.wait_for(&format!("{LATEST_PREVIEW}続"));
    stage("finish");
    if tool {
        fixture.wait_for("Read");
    }
    fixture.wait_for(STREAM_REPLY);
    fixture.wait_for("Done");
    fixture.send(b"\x04\x04");
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
    let done = frame
        .find("Done · input tokens: 37, output tokens: 11")
        .unwrap();
    assert!(
        input < thinking && thinking < tool && tool < text && text < done,
        "restored events changed order: {frame}"
    );
    fixture.send(b"\x0f");
    fixture.wait_until_shown("the full Thinking archive", |frame| {
        frame.contains("Transcript ·") && frame.contains(HIDDEN_THINKING) && frame.contains('続')
    });
    fixture.send(b"\x1b");
    fixture.wait_until_shown("the collapsed conversation again", |frame| {
        !frame.contains("Transcript ·") && frame.contains(STREAM_REPLY)
    });
    let after = screen::visible_screen(&fixture.raw_output(), 80, 24);
    assert!(!after.contains(HIDDEN_THINKING), "{after}");
    fixture.send(b"\x04\x04");
}
