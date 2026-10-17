//! A busy turn in a long Chat reads no whole Chat: the status row, Monitor, request dialogs and
//! tool results all come from the Chat's status, single records and the feed.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tickloop_agent_protocol::{Content, FEED_CHANGE_BYTES};

const BUSY_FIXTURE_VARIABLE: &str = "TICKLOOP_BUSY_TURN_FIXTURE";
const BUSY_STAGE_FILE: &str = "busy-stage";
const BUSY_TRAFFIC_FILE: &str = "busy-traffic.json";
const BUSY_FILE: &str = "busy-input.txt";
const BUSY_FIRST_LINE: &str = "first line of the busy input";
const BUSY_EDITED_LINE: &str = "edited by the busy turn";
const BUSY_REPLY: &str = "Busy turn finished";
const BUSY_MARKER: &str = "busy-marker";
const HISTORY_TURNS: usize = 120;
const HISTORY_REPLY_BYTES: usize = 4_000;
const THINKING_DELTAS: usize = 64;
const BUSY_FILE_LINES: usize = 2_000;

/// Answers the history turns at once, then holds the busy turn until the test starts it: a
/// Thinking stream with text, a Read whose result is too long for one feed change, an Edit, a
/// Shell command the user approves, and a closing reply.
struct BusyTurnModel {
    requests: AtomicUsize,
}

#[async_trait]
impl ModelClient for BusyTurnModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "busy-fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "busy-fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let request = self.requests.fetch_add(1, Ordering::SeqCst);
        let content = match request.checked_sub(HISTORY_TURNS) {
            None => tickloop_agent_test_support::text(&"h".repeat(HISTORY_REPLY_BYTES)),
            Some(0) => {
                wait_for_busy_stage("go", &cancel).await;
                for index in 0..THINKING_DELTAS {
                    events
                        .send(ModelDelta::Thinking(format!("weighing step {index} ")))
                        .unwrap();
                    tokio::task::yield_now().await;
                }
                events
                    .send(ModelDelta::Text("Reading the input.\n".into()))
                    .unwrap();
                tickloop_agent_test_support::call(
                    "busy-read",
                    "Read",
                    serde_json::json!({"file_path": BUSY_FILE}),
                )
            }
            Some(1) => tickloop_agent_test_support::call(
                "busy-edit",
                "Edit",
                serde_json::json!({
                    "file_path": BUSY_FILE,
                    "old_string": BUSY_FIRST_LINE,
                    "new_string": BUSY_EDITED_LINE,
                }),
            ),
            Some(2) => tickloop_agent_test_support::call(
                "busy-shell",
                "Bash",
                serde_json::json!({"command": format!("touch {BUSY_MARKER}")}),
            ),
            Some(_) => {
                events
                    .send(ModelDelta::Text(format!("{BUSY_REPLY}\n")))
                    .unwrap();
                tickloop_agent_test_support::text(BUSY_REPLY)
            }
        };
        let stop_reason = if content
            .iter()
            .any(|block| matches!(block, Content::ToolUse { .. }))
        {
            "tool_use"
        } else {
            "end_turn"
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: stop_reason.into(),
        })
    }
}

async fn wait_for_busy_stage(stage: &str, cancel: &CancellationToken) {
    loop {
        if std::fs::read_to_string(BUSY_STAGE_FILE).is_ok_and(|value| value == stage) {
            return;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(5)) => {},
        }
    }
}

/// Counts what the View asks the daemon about Chats: whole Chats, statuses, single operations,
/// events read by sequence, Shell tasks and the execution catalog.
#[derive(Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct BusyTraffic {
    snapshots: usize,
    statuses: usize,
    operations: usize,
    events: usize,
    tasks: usize,
    catalogs: usize,
}

struct BusyTurnAccess {
    daemon: EmbeddedAgentAccess,
    snapshots: AtomicUsize,
    statuses: AtomicUsize,
    operations: AtomicUsize,
    events: AtomicUsize,
    tasks: AtomicUsize,
    catalogs: AtomicUsize,
}

impl BusyTurnAccess {
    fn record(&self) {
        let traffic = BusyTraffic {
            snapshots: self.snapshots.load(Ordering::SeqCst),
            statuses: self.statuses.load(Ordering::SeqCst),
            operations: self.operations.load(Ordering::SeqCst),
            events: self.events.load(Ordering::SeqCst),
            tasks: self.tasks.load(Ordering::SeqCst),
            catalogs: self.catalogs.load(Ordering::SeqCst),
        };
        let temporary = format!("{BUSY_TRAFFIC_FILE}.tmp");
        std::fs::write(&temporary, serde_json::to_vec(&traffic).unwrap()).unwrap();
        std::fs::rename(temporary, BUSY_TRAFFIC_FILE).unwrap();
    }
}

#[async_trait]
impl AgentAccess for BusyTurnAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let counter = match &query {
            ObservationRequest::Agent { detail, .. } => match detail {
                AgentDetailQuery::Snapshot => Some(&self.snapshots),
                AgentDetailQuery::Status => Some(&self.statuses),
                AgentDetailQuery::Operation { .. } => Some(&self.operations),
                AgentDetailQuery::Events { .. } => Some(&self.events),
                AgentDetailQuery::Tasks => Some(&self.tasks),
                _ => None,
            },
            ObservationRequest::Overview { .. } => Some(&self.catalogs),
            _ => None,
        };
        if let Some(counter) = counter {
            counter.fetch_add(1, Ordering::SeqCst);
            self.record();
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
#[ignore = "owned PTY producer for the busy turn traffic regression"]
fn busy_turn_pty_fixture() {
    if std::env::var(BUSY_FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("busy-turn-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let core = tickloop_agent_runtime::Agent::new_embedded(
                    tickloop_agent_test_support::config(),
                    Arc::new(BusyTurnModel {
                        requests: AtomicUsize::new(0),
                    }),
                );
                core.install_own_tickhost();
                let workdir = std::env::current_dir().unwrap();
                let access = Arc::new(BusyTurnAccess {
                    daemon: EmbeddedAgentAccess::new(core.clone()),
                    snapshots: AtomicUsize::new(0),
                    statuses: AtomicUsize::new(0),
                    operations: AtomicUsize::new(0),
                    events: AtomicUsize::new(0),
                    tasks: AtomicUsize::new(0),
                    catalogs: AtomicUsize::new(0),
                });
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
                let chat = agent.create_chat(workdir.clone()).await.unwrap();
                agent
                    .set_permission_rules(
                        chat,
                        vec![PermissionRule {
                            id: Uuid::new_v4(),
                            decision: Decision::Ask,
                            tool: "Bash".into(),
                            specifier: None,
                            input: None,
                            loop_id: None,
                            persistent: false,
                        }],
                    )
                    .await
                    .unwrap();
                let mut events = core.subscribe();
                for turn in 0..HISTORY_TURNS {
                    core.submit(chat, None, format!("history turn {turn}"))
                        .await
                        .unwrap();
                    loop {
                        let event = events.recv().await.unwrap();
                        if event.chat_id == chat
                            && matches!(event.kind, EventKind::Completed { .. })
                        {
                            break;
                        }
                    }
                }
                let restored = agent.snapshot(chat).await.unwrap();
                let (console, input) = Console::start(true).unwrap();
                console.set_prompt_enabled(true).unwrap();
                // A prompt suggestion after the turn sends the whole conversation to its model,
                // as Claude Code's does, so it reads the Chat once the turn ended; it is not part
                // of the busy turn measured here.
                let settings = tickloop_agent_config::UserSettings::in_root(&workdir);
                settings.save_prompt_suggestion_enabled(false).unwrap();
                let startup = StartupState {
                    chat: StartupChat::Resumed(Box::new(restored), true),
                    user_settings: Some(settings),
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

/// The traffic once the submission's own reads have landed, which on a loaded machine can trail
/// the Working row: the counts hold still across one interval.
fn settled_traffic(directory: &std::path::Path) -> BusyTraffic {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut previous = busy_traffic(directory);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let current = busy_traffic(directory);
        if current == previous || Instant::now() >= deadline {
            return current;
        }
        previous = current;
    }
}

fn busy_traffic(directory: &std::path::Path) -> BusyTraffic {
    std::fs::read(directory.join(BUSY_TRAFFIC_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[test]
fn a_busy_turn_in_a_long_chat_reads_no_whole_chat() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let busy_input = std::iter::once(BUSY_FIRST_LINE.to_owned())
        .chain((1..BUSY_FILE_LINES).map(|line| format!("busy input line {line}")))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(busy_input.len() > FEED_CHANGE_BYTES * 2);
    std::fs::write(directory.path().join(BUSY_FILE), busy_input).unwrap();
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
        "terminal::recovery_tests::busy_turn_tests::busy_turn_pty_fixture",
        "--nocapture",
    ]);
    command.env(BUSY_FIXTURE_VARIABLE, "1");
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
        // The fixture starts a second copy of this test binary and loads a long Chat; under a fully
        // loaded test run that alone can take most of a minute.
        timeout: Duration::from_secs(60),
    };
    fixture.wait_for("Enter a message to");
    fixture.send(b"Work through the busy input.\r");
    fixture.wait_until_shown("the busy turn running", |frame| frame.contains("Working"));
    let before = settled_traffic(directory.path());
    let started = Instant::now();
    std::fs::write(directory.path().join(BUSY_STAGE_FILE), "go").unwrap();
    fixture.wait_for("Permission required");
    fixture.send(b"\r");
    fixture.wait_for(BUSY_REPLY);
    fixture.wait_for_turn_summary();
    std::thread::sleep(Duration::from_millis(300));
    let elapsed = started.elapsed();
    let after = busy_traffic(directory.path());
    let frame = screen::visible_screen(&fixture.raw_output(), 80, 24);
    println!(
        "BUSY_TURN elapsed_ms={} snapshots={} statuses={} operations={} events={} tasks={} catalogs={}",
        elapsed.as_millis(),
        after.snapshots - before.snapshots,
        after.statuses - before.statuses,
        after.operations - before.operations,
        after.events - before.events,
        after.tasks - before.tasks,
        after.catalogs - before.catalogs,
    );
    assert_eq!(
        after.snapshots - before.snapshots,
        0,
        "a busy turn read the whole Chat: {frame}"
    );
    assert!(
        after.events > before.events,
        "the Read result too long for one feed change was read by sequence"
    );
    assert!(
        after.operations > before.operations,
        "the Edit's review was read as its operation"
    );
    assert!(
        after.catalogs - before.catalogs <= elapsed.as_secs() as usize + 2,
        "only the once-a-second refresh reads the execution catalog: {}",
        after.catalogs - before.catalogs
    );
    assert!(
        !fixture
            .raw_output()
            .contains("The view fell behind; restoring it from saved execution events."),
        "{frame}"
    );
    assert!(directory.path().join(BUSY_MARKER).exists());
    assert_eq!(
        std::fs::read_to_string(directory.path().join(BUSY_FILE))
            .unwrap()
            .lines()
            .next(),
        Some(BUSY_EDITED_LINE)
    );
    fixture.send(b"\x04\x04");
}
