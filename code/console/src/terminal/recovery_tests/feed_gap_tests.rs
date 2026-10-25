//! A daemon feed that skips events — a lagged Reset or an oversized event sent as a Resync — must
//! not leave the tool rows it skipped queued or working: the View restores them from the snapshot
//! and draws the conversation an unbroken feed draws.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tickloop_code_access::{FEED_CHANGE_BYTES, ObservationUpdate, ResetReason};
use tickloop_harness_protocol::Content;

const GAP_FIXTURE_VARIABLE: &str = "TICKLOOP_FEED_GAP_FIXTURE";
const GAP_ROUTE_VARIABLE: &str = "TICKLOOP_FEED_GAP_ROUTE";
const GAP_PHASE_FILE: &str = "gap-phase.json";
const GAP_STAGE_FILE: &str = "gap-stage";
const NOTES_FILE: &str = "notes.txt";
/// Every event reaches the View.
const LOSSLESS_ROUTE: &str = "lossless";
/// The View's feed loses every event from the first tool start until the last request begins,
/// and a Reset carrying the current snapshot stands in their place, as a lagged worker feed does.
const LAGGED_ROUTE: &str = "lagged";
/// The notes file is larger than one feed change, so the daemon sends its Read result as a Resync.
const OVERSIZED_ROUTE: &str = "oversized";
/// The Thinking that opens each model request; the last one opens every later request.
const THINKING: [&str; 3] = [
    "Run the check first.",
    "Search the notes next.",
    "Summarize the notes.",
];
const USER_INPUT: &str = "Check the notes";
const FINAL_REPLY: &str = "The notes are checked.";
const BASH_ROW: &str = "Bash · Print one";
const SEARCH_ROW: &str = "Searched for 2 patterns, read 1 file";

/// Runs a Bash call, then two searches and a read in one message, then holds the last request
/// after its first Thinking until the test writes `finish` to the stage file.
struct GapScript {
    requests: AtomicUsize,
}

#[async_trait]
impl ModelClient for GapScript {
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
            profile: "feed-gap-fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "feed-gap-fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let call = tickloop_harness_test_support::call;
        let workdir = std::env::current_dir().unwrap().display().to_string();
        let request = self.requests.fetch_add(1, Ordering::Relaxed);
        let thinking = THINKING[request.min(THINKING.len() - 1)];
        events.send(ModelDelta::Thinking(thinking.into())).unwrap();
        let reply = match request {
            0 => call(
                "print-one",
                "Bash",
                serde_json::json!({"command": "printf 'one\\n'", "description": "Print one"}),
            ),
            1 => [
                call(
                    "search-todo",
                    "Grep",
                    serde_json::json!({"pattern": "TODO", "path": workdir}),
                ),
                call(
                    "search-done",
                    "Grep",
                    serde_json::json!({"pattern": "DONE", "path": workdir}),
                ),
                call(
                    "read-notes",
                    "Read",
                    serde_json::json!({"file_path": format!("{workdir}/{NOTES_FILE}")}),
                ),
            ]
            .concat(),
            _ => {
                std::fs::write(
                    GAP_PHASE_FILE,
                    serde_json::json!({"phase": "summarizing"}).to_string(),
                )
                .unwrap();
                while !std::fs::read_to_string(GAP_STAGE_FILE).is_ok_and(|stage| stage == "finish")
                {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                    }
                }
                events.send(ModelDelta::Text(FINAL_REPLY.into())).unwrap();
                tickloop_harness_test_support::text(FINAL_REPLY)
            }
        };
        let mut content = vec![Content::Thinking {
            thinking: thinking.into(),
            signature: None,
        }];
        content.extend(reply);
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

struct GapAccess {
    daemon: EmbeddedAgentAccess,
    route: String,
}

#[async_trait]
impl AgentAccess for GapAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let lossy_feed = match &query {
            ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::Snapshot | AgentDetailQuery::Status,
                ..
            } if self.route == LAGGED_ROUTE => Some(*execution),
            _ => None,
        };
        let observation = self.daemon.observe(query).await?;
        match lossy_feed {
            Some(execution) if observation.has_feed() => Ok(lose_tool_events(
                self.daemon.clone(),
                execution,
                observation,
            )),
            _ => Ok(observation),
        }
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.daemon.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.daemon.show_view(update).await
    }
}

/// Forwards `observation`'s feed, but loses every update from the first tool start until the
/// Thinking of the request after the second assistant message, and sends a Reset with the current
/// snapshot in their place before that Thinking.
fn lose_tool_events(
    daemon: EmbeddedAgentAccess,
    execution: ExecutionRef,
    observation: Observation,
) -> Observation {
    let (initial, cursor, reset, changes, failure) = observation.into_parts();
    let mut changes = changes.expect("an Agent observation feed");
    let (sender, receiver) = mpsc::channel(FEED_CAPACITY);
    tokio::spawn(async move {
        let mut committed_messages = 0;
        let mut losing = false;
        let mut lost = false;
        while let Some(update) = changes.recv().await {
            if !lost && let ObservationChange::Event(event) = &update.change {
                match event.kind {
                    EventKind::ToolStarted { .. } => losing = true,
                    EventKind::AssistantMessageCommitted { .. } => committed_messages += 1,
                    EventKind::Thinking(_) if losing && committed_messages == 2 => {
                        let current = daemon
                            .observe(ObservationRequest::Agent {
                                execution,
                                detail: AgentDetailQuery::Snapshot,
                                after: None,
                            })
                            .await
                            .unwrap();
                        let ObservationValue::Agent(snapshot) = current.initial else {
                            panic!("the Agent snapshot");
                        };
                        let reset = ObservationUpdate {
                            cursor: update.cursor,
                            change: ObservationChange::Reset {
                                reason: ResetReason::Lagged,
                                snapshot,
                            },
                        };
                        if sender.send(reset).await.is_err() {
                            break;
                        }
                        losing = false;
                        lost = true;
                    }
                    _ => {}
                }
            }
            if losing {
                continue;
            }
            if sender.send(update).await.is_err() {
                break;
            }
        }
    });
    Observation::new(initial, cursor, reset, Some(receiver), failure)
}

#[test]
#[ignore = "owned PTY producer for the skipped feed event regression"]
fn feed_gap_pty_fixture() {
    if std::env::var(GAP_FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("feed-gap-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_feed_gap_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_feed_gap_fixture() {
    let route = std::env::var(GAP_ROUTE_VARIABLE).unwrap();
    let notes = if route == OVERSIZED_ROUTE {
        (1..=FEED_CHANGE_BYTES / 16)
            .map(|line| format!("TODO note {line:05}\n"))
            .collect::<String>()
    } else {
        "TODO: fix cache\nDONE: ship the build\n".into()
    };
    std::fs::write(NOTES_FILE, notes).unwrap();
    let core = tickloop_harness_runtime::Agent::new_embedded(
        tickloop_harness_test_support::config(),
        Arc::new(GapScript {
            requests: AtomicUsize::new(0),
        }),
    );
    let access = Arc::new(GapAccess {
        daemon: EmbeddedAgentAccess::new(core.clone()),
        route,
    });
    let workdir = std::env::current_dir().unwrap();
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(tickloop_harness_test_support::config());
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
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Allow,
                tool: "Bash".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let startup = StartupState {
        model_selected: true,
        launch_arguments: Vec::new(),
        chat: StartupChat::Resumed(Box::new(agent.snapshot(chat).await.unwrap()), false),
        user_settings: None,
    };
    let (console, input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    interact(&agent, workdir, &console, input, startup, None)
        .await
        .unwrap();
    core.shutdown().await;
}

fn start_gap_fixture(route: &str) -> (RecoveryPty, tempfile::TempDir) {
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
        "terminal::recovery_tests::feed_gap_tests::feed_gap_pty_fixture",
        "--nocapture",
    ]);
    command.env(GAP_FIXTURE_VARIABLE, "1");
    command.env(GAP_ROUTE_VARIABLE, route);
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

/// The conversation rows `route` shows from the input through the collapsed searches while the
/// run still works on its last request; afterwards the run finishes with each row shown once.
fn tool_rows_while_working(route: &str) -> String {
    let (mut fixture, directory) = start_gap_fixture(route);
    fixture.wait_for("Enter a message to");
    fixture.send(format!("{USER_INPUT}\r").as_bytes());
    fixture.wait_for_phase(&directory.path().join(GAP_PHASE_FILE), "summarizing");
    fixture.wait_until_shown("every tool row finished while the run works", |frame| {
        frame.contains(SEARCH_ROW) && frame.contains(BASH_ROW) && !frame.contains("Queued")
    });
    let frame = screen::visible_screen(&fixture.raw_output(), 80, 24);
    let lines = frame.lines().collect::<Vec<_>>();
    let first = lines
        .iter()
        .position(|line| line.contains(USER_INPUT))
        .unwrap();
    let last = lines
        .iter()
        .position(|line| line.contains(SEARCH_ROW))
        .unwrap();
    let rows = lines[first..=last].join("\n");
    println!("FEED_GAP_ROWS {route}\n{rows}");
    std::fs::write(directory.path().join(GAP_STAGE_FILE), "finish").unwrap();
    fixture.wait_for(FINAL_REPLY);
    fixture.wait_for_turn_summary();
    let finished = screen::visible_screen(&fixture.raw_output(), 80, 24);
    let input_echoes = finished
        .lines()
        .filter(|line| line.trim_start().starts_with('❯') && line.contains(USER_INPUT))
        .count();
    assert_eq!(input_echoes, 1, "{route}: {finished}");
    for row in [BASH_ROW, SEARCH_ROW, FINAL_REPLY] {
        assert_eq!(finished.matches(row).count(), 1, "{route}: {finished}");
    }
    fixture.send(b"\x04\x04");
    rows
}

#[test]
fn a_lagged_daemon_reset_restores_the_tool_rows_it_skipped() {
    assert_eq!(
        tool_rows_while_working(LAGGED_ROUTE),
        tool_rows_while_working(LOSSLESS_ROUTE)
    );
}

#[test]
fn an_oversized_tool_result_restores_its_row_from_the_snapshot() {
    assert_eq!(
        tool_rows_while_working(OVERSIZED_ROUTE),
        tool_rows_while_working(LOSSLESS_ROUTE)
    );
}
