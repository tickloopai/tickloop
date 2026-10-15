//! Run-loop regression: an input whose reply was lost is restored, and pressing Enter on the
//! unchanged draft adopts the original acceptance once its receipt proves it, never sending twice.
use super::*;
use async_trait::async_trait;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tickloop_agent_daemon::EmbeddedAgentAccess;
use uuid::Uuid;

use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AgentAccess, AgentCommand, AgentDetailQuery, CommandEnvelope,
    CommandReceipt, CommandResult, ConfigurationChange, DisplayReceipt, DisplayUpdate,
    ExecutionRef, LaunchSpec, LoopState, Observation, ObservationRequest, ObservationValue,
    ReceiptState, TaskAction, ViewRef,
};
use tickloop_agent_test_support::screen;

const FIXTURE_VARIABLE: &str = "TICKLOOP_LATE_RECEIPT_FIXTURE";
const DIRECTORY_VARIABLE: &str = "TICKLOOP_LATE_RECEIPT_DIRECTORY";
/// Set to start the View on a New Agent draft instead of an existing Agent.
const DRAFT_VARIABLE: &str = "TICKLOOP_LATE_RECEIPT_DRAFT";
const WIDTH: usize = 100;
const HEIGHT: usize = 30;
const TIMEOUT: Duration = Duration::from_secs(10);

/// The daemon the View talks to: while `lose-replies`, `lose-create` or `lose-configure` exists a
/// Submit, Create or Configure runs but its reply is lost, and while `hide-receipts` exists
/// receipt lookups find nothing (each counted in `hidden-receipt-reads`). Submits, Shell
/// submissions and after-turn queues are counted in `submits`, `shell-submits` and
/// `after-turn-queues`.
struct LateReceiptDaemon {
    daemon: EmbeddedAgentAccess,
    directory: PathBuf,
}

impl LateReceiptDaemon {
    fn flag(&self, name: &str) -> bool {
        self.directory.join(name).exists()
    }

    /// Appends one line to the `name` counter file.
    fn count(&self, name: &str) {
        let mut counter = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join(name))
            .unwrap();
        writeln!(counter).unwrap();
    }

    fn record(&self, name: &str, value: serde_json::Value) {
        let pending = self.directory.join(format!("{name}.pending"));
        std::fs::write(&pending, value.to_string()).unwrap();
        std::fs::rename(pending, self.directory.join(name)).unwrap();
    }
}

#[async_trait]
impl AgentAccess for LateReceiptDaemon {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let hides =
            matches!(query, ObservationRequest::Receipt { .. }) && self.flag("hide-receipts");
        if hides {
            self.count("hidden-receipt-reads");
        }
        let mut observation = self.daemon.observe(query).await?;
        if hides {
            observation.initial = ObservationValue::Receipt(None);
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let (record_name, configured_choice) = match &command.command {
            AgentCommand::Create { .. } => (Some("created-receipt.json"), None),
            AgentCommand::Configure {
                execution, change, ..
            } => (
                Some("configured-receipt.json"),
                Some((*execution, change.clone())),
            ),
            _ => (None, None),
        };
        let loses_reply = match command.command {
            AgentCommand::Submit { .. } => self.flag("lose-replies"),
            AgentCommand::Create { .. } => self.flag("lose-create"),
            AgentCommand::Configure { .. } => self.flag("lose-configure"),
            _ => false,
        };
        match &command.command {
            AgentCommand::Create { .. } => self.count("creates"),
            AgentCommand::Configure { .. } => self.count("configures"),
            AgentCommand::Submit { .. } => self.count("submits"),
            AgentCommand::ManageTask {
                action: TaskAction::SubmitShell { .. },
                ..
            } => self.count("shell-submits"),
            AgentCommand::EnqueueAfterTurn { .. } => self.count("after-turn-queues"),
            _ => {}
        }
        let receipt = self.daemon.execute(command).await?;
        if self.flag("lose-configure")
            && let Some(name) = record_name
        {
            self.record(name, serde_json::to_value(&receipt).unwrap());
        }
        if self.flag("lose-configure")
            && let Some((execution, change)) = configured_choice
        {
            let observation = self
                .daemon
                .observe(ObservationRequest::Agent {
                    execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await?;
            let ObservationValue::Agent(snapshot) = observation.initial else {
                panic!("the daemon's configured Agent snapshot");
            };
            self.record("configured-state.json", serde_json::json!({
                "execution": snapshot.execution,
                "command_id": receipt.command_id,
                "change": change,
                "mode": snapshot.policy.mode,
                "messages": snapshot.conversation.messages.len(),
                "running": snapshot.conversation.loops.iter().any(|goal| goal.state == LoopState::Running),
                "pending_inputs": snapshot.pending_inputs.len(),
                "pending_commands": snapshot.pending_commands.len(),
            }));
        }
        if loses_reply {
            return Err(AccessError::new(
                AccessErrorCode::OutcomeUnknown,
                "fixture reply was lost",
            ));
        }
        Ok(receipt)
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        let execution = match &update {
            DisplayUpdate::Displayed { execution, .. } => Some(*execution),
            _ => None,
        };
        let receipt = self.daemon.show_view(update).await?;
        if self.flag("lose-configure")
            && let Some(execution) = execution
        {
            assert!(receipt.displayed);
            self.record(
                "displayed-execution.json",
                serde_json::to_value(execution).unwrap(),
            );
        }
        Ok(receipt)
    }
}

/// The fixture model; while `hold-model` exists a request waits, so the run stays active.
struct HeldWhileFlagged {
    model: Arc<tickloop_agent_test_support::ScriptedModel>,
    flag: PathBuf,
}

#[async_trait]
impl tickloop_agent_model::ModelClient for HeldWhileFlagged {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        self.model.identity()
    }

    async fn complete(
        &self,
        request: tickloop_agent_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        while self.flag.exists() {
            tokio::select! {
                _ = cancel.cancelled() => return Err(tickloop_agent_model::ModelError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        self.model.complete(request, events, cancel).await
    }
}

#[test]
#[ignore = "owned PTY producer for the late-receipt run-loop regression"]
fn late_receipt_pty_fixture() {
    if std::env::var(FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("late-receipt-fixture".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_late_receipt_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_late_receipt_fixture() {
    let directory = PathBuf::from(std::env::var_os(DIRECTORY_VARIABLE).unwrap());
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::text("late reply"),
        tickloop_agent_test_support::text("edited reply"),
    ]));
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(HeldWhileFlagged {
            model: model.clone(),
            flag: directory.join("hold-model"),
        }),
    );
    core.install_own_tickhost();
    let workdir = std::env::current_dir().unwrap();
    let adapter = Arc::new(LateReceiptDaemon {
        daemon: EmbeddedAgentAccess::new(core.clone()),
        directory: directory.clone(),
    });
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(tickloop_agent_test_support::config());
    launch.session_persistence = false;
    let agent = Agent::from_access_for_test(
        adapter.clone(),
        adapter.daemon.daemon_epoch(),
        ViewRef {
            session_id: Uuid::new_v4(),
            view_id: Uuid::new_v4(),
        },
        launch,
        None,
    );
    let chat = if std::env::var_os(DRAFT_VARIABLE).is_some() {
        StartupChat::New
    } else {
        let chat = agent.create_chat(workdir.clone()).await.unwrap();
        let execution = agent.reference(chat).await.unwrap();
        tokio::task::spawn_local(answer_daemon_probes(adapter.clone(), execution));
        StartupChat::Resumed(Box::new(agent.snapshot(chat).await.unwrap()), false)
    };
    let (console, input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let startup = StartupState {
        chat,
        user_settings: None,
    };
    let result = interact(&agent, workdir, &console, input, startup, None).await;
    drop(console);
    std::fs::write(
        directory.join("model-requests"),
        model.requests.lock().unwrap().len().to_string(),
    )
    .unwrap();
    core.shutdown().await;
    result.unwrap();
}

/// Writes the daemon's own state of `execution` to `daemon-state.json` whenever the test asks
/// for it with `probe-daemon`.
async fn answer_daemon_probes(daemon: Arc<LateReceiptDaemon>, execution: ExecutionRef) {
    let request = daemon.directory.join("probe-daemon");
    loop {
        if request.exists()
            && let Ok(observation) = daemon
                .daemon
                .observe(ObservationRequest::Agent {
                    execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
            && let ObservationValue::Agent(snapshot) = observation.initial
        {
            let state = serde_json::json!({
                "running": snapshot
                    .conversation
                    .loops
                    .iter()
                    .any(|goal| goal.state == LoopState::Running),
                "pending_inputs": snapshot.pending_inputs.len(),
                "pending_commands": snapshot.pending_commands.len(),
            });
            let pending = daemon.directory.join("daemon-state.pending");
            std::fs::write(&pending, state.to_string()).unwrap();
            std::fs::rename(pending, daemon.directory.join("daemon-state.json")).unwrap();
            std::fs::remove_file(&request).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct LateReceiptPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl LateReceiptPty {
    fn frame(&self) -> String {
        screen::visible_screen(
            &String::from_utf8_lossy(&self.output.lock().unwrap()),
            WIDTH,
            HEIGHT,
        )
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// Waits until the screen, with wrapped rows joined, satisfies `ready`.
    fn wait_for(&mut self, label: &str, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let frame = self.frame().replace('\n', "");
            if ready(&frame) {
                return frame;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "the View exited while waiting for {label}: {}",
                String::from_utf8_lossy(&self.output.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("late-receipt View never showed {label}: {}", self.frame());
    }

    /// Waits until the screen, row by row, satisfies `ready`.
    fn wait_for_rows(&mut self, label: &str, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let frame = self.frame();
            if ready(&frame) {
                return frame;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("late-receipt View never showed {label}: {}", self.frame());
    }
}

impl Drop for LateReceiptPty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// The rows above the composer's upper rule, found by the rules themselves, so a Shell-mode
/// composer is told apart from echoed `❯` lines.
fn body_above_rules(frame: &str) -> Vec<&str> {
    let frame = screen::message_flow(frame);
    let lines = frame.lines().collect::<Vec<_>>();
    let rules = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with('─'))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    lines[..rules[rules.len() - 2]].to_vec()
}

/// The composer row between the two rules above the footer.
fn composer_row(frame: &str) -> &str {
    let lines = frame.lines().collect::<Vec<_>>();
    let rules = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with('─'))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    lines[rules[rules.len() - 2] + 1]
}

/// Body rows that echo `input` as a submitted line.
fn echoed(frame: &str, input: &str) -> usize {
    body_above_rules(frame)
        .iter()
        .filter(|line| {
            let is_echo = !screen::is_queued_input(line);
            let line = line.trim_start();
            is_echo && (line.starts_with('❯') || line.starts_with("! ")) && line.contains(input)
        })
        .count()
}

/// The rows above the composer's upper rule; restored drafts sit in the composer, not here.
fn conversation_body(frame: &str) -> String {
    let frame = screen::message_flow(frame);
    let lines = frame.lines().collect::<Vec<_>>();
    let composer = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with('❯'))
        .expect("the composer prompt is painted");
    lines[..composer.saturating_sub(1)].join("\n")
}

fn close(view: &mut LateReceiptPty) {
    view.send(b"\x15\x04\x04");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = view.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "the View closed with an error: {}",
                String::from_utf8_lossy(&view.output.lock().unwrap())
            );
            break;
        }
        assert!(Instant::now() < deadline, "the View did not close");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn submits(directory: &std::path::Path) -> usize {
    counted(directory, "submits")
}

fn counted(directory: &std::path::Path, name: &str) -> usize {
    std::fs::read_to_string(directory.join(name))
        .map(|counter| counter.lines().count())
        .unwrap_or(0)
}

/// Waits until the View has read the hidden receipt more often than `before` times, which it
/// does before holding a new input behind the unknown one.
fn wait_for_hidden_receipt_read(directory: &std::path::Path, before: usize) {
    let deadline = Instant::now() + TIMEOUT;
    while counted(directory, "hidden-receipt-reads") <= before {
        assert!(
            Instant::now() < deadline,
            "the View never reread the receipt"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The daemon's own state of the fixture Agent: whether a Loop runs, and its pending inputs and
/// commands.
fn daemon_state(directory: &std::path::Path) -> serde_json::Value {
    let state = directory.join("daemon-state.json");
    let _ = std::fs::remove_file(&state);
    std::fs::write(directory.join("probe-daemon"), "").unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while !state.exists() {
        assert!(
            Instant::now() < deadline,
            "the fixture never reported the daemon state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap()
}

/// Starts the fixture View in a PTY, on a New Agent draft when `draft` is set.
fn start_view(directory: &std::path::Path, draft: bool) -> LateReceiptPty {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: HEIGHT as u16,
            cols: WIDTH as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--ignored",
        "--exact",
        "terminal::late_receipt_tests::late_receipt_pty_fixture",
        "--nocapture",
    ]);
    command.env(FIXTURE_VARIABLE, "1");
    command.env(DIRECTORY_VARIABLE, directory);
    if draft {
        command.env(DRAFT_VARIABLE, "1");
    }
    command.env("TICKLOOP_AGENT_TUI", "fullscreen");
    command.env("TERM", "xterm-256color");
    command.env("HOME", directory);
    command.cwd(directory);
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
    LateReceiptPty {
        child,
        writer,
        output,
        _master: pair.master,
    }
}

#[test]
fn an_unchanged_restored_input_adopts_its_late_acceptance_instead_of_sending_again() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = start_view(directory.path(), false);
    view.wait_for("the session input", |frame| frame.contains("❯"));
    std::fs::write(directory.path().join("lose-replies"), "").unwrap();
    std::fs::write(directory.path().join("hide-receipts"), "").unwrap();

    view.send(b"late input \x1b[200~alpha\nbeta\ngamma\ndelta\x1b[201~\r");
    view.wait_for("the restored draft and the unknown result", |frame| {
        frame.contains("has an unknown acceptance")
            && frame.contains("late input [Pasted text #1 +3 lines]")
    });
    assert_eq!(submits(directory.path()), 1);
    std::fs::remove_file(directory.path().join("lose-replies")).unwrap();

    view.send(b"\r");
    view.wait_for(
        "the retry held while the receipt is still missing",
        |frame| {
            frame.contains("still has an unknown acceptance")
                && frame.contains("late input [Pasted text #1 +3 lines]")
        },
    );
    assert_eq!(
        submits(directory.path()),
        1,
        "an unknown input is never resent"
    );

    std::fs::remove_file(directory.path().join("hide-receipts")).unwrap();
    view.send(b"\r");
    view.wait_for("the original acceptance adopted", |frame| {
        frame.contains("was already accepted; nothing was resent")
            && frame.contains("late reply")
            && !frame.contains("late input [Pasted")
    });
    assert_eq!(submits(directory.path()), 1, "adopting sends nothing");
    let frame = view.frame();
    assert_eq!(
        conversation_body(&frame).matches("❯ late input").count(),
        1,
        "the adopted input is shown once: {frame}"
    );

    view.send(b"edited input\r");
    view.wait_for("the edited input sent as new", |frame| {
        frame.contains("edited reply")
    });
    assert_eq!(submits(directory.path()), 2);

    close(&mut view);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("model-requests")).unwrap(),
        "2",
        "the model ran the original input once and the edited input once"
    );
}

#[test]
fn an_unconfirmed_creation_keeps_its_input_out_of_the_body_until_it_is_sent_once() {
    const FIRST: &str = "first draft input";
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let flag = |name: &str| directory.path().join(name);
    let mut view = start_view(directory.path(), true);
    view.wait_for("the New Agent draft", |frame| frame.contains("❯"));

    // The Create's reply and receipt are lost: each Enter rereads its receipt, nothing is resent.
    std::fs::write(flag("lose-create"), "").unwrap();
    std::fs::write(flag("hide-receipts"), "").unwrap();
    view.send(format!("{FIRST}\r").as_bytes());
    view.wait_for("the unconfirmed creation", |frame| {
        frame.contains("is unconfirmed; nothing was resent")
    });
    view.send(b"\r");
    std::thread::sleep(Duration::from_millis(500));
    let frame = view.frame();
    assert_eq!(
        conversation_body(&frame).matches(FIRST).count(),
        0,
        "an input kept for another attempt is not shown as sent: {frame}"
    );
    std::fs::remove_file(flag("lose-create")).unwrap();
    std::fs::remove_file(flag("hide-receipts")).unwrap();
    view.send(b"\r");
    view.wait_for("the confirmed creation answers", |frame| {
        frame.contains("late reply")
    });
    let frame = view.frame();
    assert_eq!(
        conversation_body(&frame).matches(FIRST).count(),
        1,
        "the created Agent shows its first input once: {frame}"
    );
    assert_eq!(submits(directory.path()), 1);
    view.send(b"/clear\r");
    view.wait_for("the next New Agent draft", |frame| {
        !frame.contains("late reply")
    });
    std::thread::sleep(Duration::from_millis(200));
    let frame = view.frame();
    assert!(
        !conversation_body(&frame).contains(FIRST),
        "the next draft keeps no copy of the created Agent's input: {frame}"
    );

    close(&mut view);
    assert_eq!(
        std::fs::read_to_string(flag("model-requests")).unwrap(),
        "1",
        "the first input reached the model once"
    );
}

#[test]
fn an_unknown_staged_choice_keeps_its_input_out_of_the_body_until_it_is_sent_once() {
    const SECOND: &str = "second draft input";
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let flag = |name: &str| directory.path().join(name);
    let mut view = start_view(directory.path(), true);
    view.wait_for("the New Agent draft", |frame| frame.contains("❯"));

    // A staged choice's reply and receipt are lost after the Agent was created and selected.
    view.send(b"/mode auto\r");
    view.wait_for("the staged execution mode", |frame| {
        frame.contains("Execution mode: Auto")
    });
    std::fs::write(flag("lose-configure"), "").unwrap();
    std::fs::write(flag("hide-receipts"), "").unwrap();
    view.send(format!("{SECOND}\r").as_bytes());
    let frame = view.wait_for_rows(
        "the created Agent selected with the input kept in the composer",
        |frame| {
            let composer = frame
                .lines()
                .rfind(|line| line.trim_start().starts_with('❯'))
                .unwrap_or_default();
            composer.contains(SECOND)
                && flag("displayed-execution.json").exists()
                && flag("configured-state.json").exists()
        },
    );
    let created: CommandReceipt =
        serde_json::from_slice(&std::fs::read(flag("created-receipt.json")).unwrap()).unwrap();
    let configured: CommandReceipt =
        serde_json::from_slice(&std::fs::read(flag("configured-receipt.json")).unwrap()).unwrap();
    let displayed: ExecutionRef =
        serde_json::from_slice(&std::fs::read(flag("displayed-execution.json")).unwrap()).unwrap();
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(flag("configured-state.json")).unwrap()).unwrap();
    assert!(matches!(created.state, ReceiptState::Completed));
    assert!(matches!(configured.state, ReceiptState::Completed));
    assert!(created.error.is_none());
    assert!(configured.error.is_none());
    let Some(CommandResult::Created { execution, .. }) = created.result else {
        panic!("the actual Create result");
    };
    assert_eq!(displayed, execution);
    assert_eq!(configured.target, Some(execution));
    assert_eq!(state["execution"], serde_json::to_value(execution).unwrap());
    assert_eq!(
        state["command_id"],
        serde_json::to_value(configured.command_id).unwrap()
    );
    let change: ConfigurationChange = serde_json::from_value(state["change"].clone()).unwrap();
    assert!(matches!(
        change,
        ConfigurationChange::ExecutionMode {
            mode: ExecutionMode::Auto
        }
    ));
    assert_eq!(
        state["mode"],
        serde_json::to_value(ExecutionMode::Auto).unwrap()
    );
    assert_eq!(state["messages"], 0);
    assert_eq!(state["running"], false);
    assert_eq!(state["pending_inputs"], 0);
    assert_eq!(state["pending_commands"], 0);
    assert_eq!(counted(directory.path(), "creates"), 1);
    assert_eq!(counted(directory.path(), "configures"), 1);
    assert_eq!(submits(directory.path()), 0);
    assert_eq!(conversation_body(&frame).matches("❯ /mode auto").count(), 1);
    assert_eq!(
        conversation_body(&frame)
            .matches("Execution mode: Auto")
            .count(),
        1
    );
    assert_eq!(
        conversation_body(&frame).matches(SECOND).count(),
        0,
        "an input kept and not sent is not shown as sent: {frame}"
    );
    std::fs::remove_file(flag("lose-configure")).unwrap();
    std::fs::remove_file(flag("hide-receipts")).unwrap();
    view.send(b"\r");
    view.wait_for("the settled choice lets the input through", |frame| {
        frame.contains("late reply")
    });
    let frame = view.frame();
    assert_eq!(
        conversation_body(&frame).matches(SECOND).count(),
        1,
        "the created Agent shows the input once: {frame}"
    );
    assert_eq!(submits(directory.path()), 1);
    assert_eq!(
        counted(directory.path(), "creates"),
        1,
        "settling creates no second Agent"
    );
    assert_eq!(
        counted(directory.path(), "configures"),
        1,
        "the unknown choice is not resent"
    );
    view.send(b"/clear\r");
    view.wait_for("another New Agent draft", |frame| {
        !frame.contains("late reply")
    });
    std::thread::sleep(Duration::from_millis(200));
    let frame = view.frame();
    assert!(
        !conversation_body(&frame).contains(SECOND),
        "the next draft keeps no copy of the created Agent's input: {frame}"
    );
    close(&mut view);
    assert_eq!(
        std::fs::read_to_string(flag("model-requests")).unwrap(),
        "1",
        "the first input reached the model once"
    );
}

#[test]
fn an_edited_input_held_behind_an_unknown_one_is_shown_only_once_it_is_sent() {
    const FIRST: &str = "unknown first input";
    const EDITED: &str = "edited second input";
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let flag = |name: &str| directory.path().join(name);
    let mut view = start_view(directory.path(), false);
    view.wait_for("the session input", |frame| frame.contains("❯"));
    std::fs::write(flag("lose-replies"), "").unwrap();
    std::fs::write(flag("hide-receipts"), "").unwrap();
    view.send(format!("{FIRST}\r").as_bytes());
    view.wait_for("the unknown first input", |frame| {
        frame.contains("has an unknown acceptance")
    });
    assert_eq!(submits(directory.path()), 1);
    std::fs::remove_file(flag("lose-replies")).unwrap();

    // Editing the restored draft makes it a new input, held while the first is unknown.
    view.send(format!("\x15{EDITED}\r").as_bytes());
    let frame = view.wait_for_rows("the edited input held and kept in the composer", |frame| {
        let composer = frame
            .lines()
            .rfind(|line| line.trim_start().starts_with('❯'))
            .unwrap_or_default();
        composer.contains(EDITED) && frame.matches("unknown acceptance").count() >= 2
    });
    let body = conversation_body(&frame);
    assert_eq!(body.matches(FIRST).count(), 1, "{frame}");
    assert_eq!(
        body.matches(EDITED).count(),
        0,
        "an input held back and not sent is not shown as sent: {frame}"
    );
    assert_eq!(submits(directory.path()), 1);

    std::fs::remove_file(flag("hide-receipts")).unwrap();
    view.send(b"\r");
    view.wait_for("the edited input answered", |frame| {
        frame.contains("edited reply")
    });
    let frame = view.frame();
    let body = conversation_body(&frame);
    assert_eq!(body.matches(FIRST).count(), 1, "{frame}");
    assert_eq!(
        body.matches(EDITED).count(),
        1,
        "the edited input is shown once it is sent: {frame}"
    );
    assert_eq!(
        submits(directory.path()),
        2,
        "the first input was not sent again"
    );
    close(&mut view);
    assert_eq!(
        std::fs::read_to_string(flag("model-requests")).unwrap(),
        "2",
        "each input reached the model once"
    );
}

#[test]
fn an_edited_shell_command_held_behind_an_unknown_input_is_shown_only_once_it_runs() {
    const FIRST: &str = "unknown first input";
    const SHELL: &str = "printf held-shell-ran";
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let flag = |name: &str| directory.path().join(name);
    let mut view = start_view(directory.path(), false);
    view.wait_for("the session input", |frame| frame.contains("❯"));
    std::fs::write(flag("lose-replies"), "").unwrap();
    std::fs::write(flag("hide-receipts"), "").unwrap();
    view.send(format!("{FIRST}\r").as_bytes());
    view.wait_for("the unknown first input", |frame| {
        frame.contains("has an unknown acceptance")
    });
    std::fs::remove_file(flag("lose-replies")).unwrap();

    // Editing the restored draft into a Shell command makes it a new input, held meanwhile.
    let reads = counted(directory.path(), "hidden-receipt-reads");
    view.send(format!("\x15!{SHELL}\r").as_bytes());
    wait_for_hidden_receipt_read(directory.path(), reads);
    let frame = view.wait_for_rows("the held Shell command kept in the composer", |frame| {
        frame.contains("Cannot run user shell") && composer_row(frame).contains(SHELL)
    });
    assert_eq!(echoed(&frame, FIRST), 1, "{frame}");
    assert_eq!(
        echoed(&frame, SHELL),
        0,
        "a Shell command held back and not run is not shown as sent: {frame}"
    );
    assert_eq!(submits(directory.path()), 1);
    assert_eq!(
        counted(directory.path(), "shell-submits"),
        0,
        "not run while held"
    );

    std::fs::remove_file(flag("hide-receipts")).unwrap();
    view.send(b"\r");
    let frame = view.wait_for_rows("the Shell command ran", |frame| {
        frame.lines().any(|line| line.trim() == "⎿  held-shell-ran")
    });
    assert_eq!(echoed(&frame, FIRST), 1, "{frame}");
    assert_eq!(
        echoed(&frame, SHELL),
        1,
        "the Shell command is shown once it runs: {frame}"
    );
    assert_eq!(
        submits(directory.path()),
        1,
        "the first input was not sent again"
    );
    assert_eq!(counted(directory.path(), "shell-submits"), 1, "run once");
    close(&mut view);
}

#[test]
fn a_busy_command_held_behind_an_unknown_input_is_queued_and_shown_once() {
    const FIRST: &str = "unknown busy input";
    const COMMAND: &str = "/tickloop check the busy work";
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let flag = |name: &str| directory.path().join(name);
    let mut view = start_view(directory.path(), false);
    view.wait_for("the session input", |frame| frame.contains("❯"));
    std::fs::write(flag("hold-model"), "").unwrap();
    std::fs::write(flag("lose-replies"), "").unwrap();
    std::fs::write(flag("hide-receipts"), "").unwrap();
    view.send(format!("{FIRST}\r").as_bytes());
    view.wait_for("the unknown first input", |frame| {
        frame.contains("has an unknown acceptance")
    });
    std::fs::remove_file(flag("lose-replies")).unwrap();

    // The first input's run is still active, so the edited command would wait for the turn.
    assert_eq!(
        daemon_state(directory.path())["running"],
        true,
        "the first input runs"
    );
    let reads = counted(directory.path(), "hidden-receipt-reads");
    view.send(format!("\x15{COMMAND}\r").as_bytes());
    wait_for_hidden_receipt_read(directory.path(), reads);
    let frame = view.wait_for_rows("the held command kept in the composer", |frame| {
        composer_row(frame).contains(COMMAND)
    });
    assert_eq!(echoed(&frame, FIRST), 1, "{frame}");
    assert_eq!(
        echoed(&frame, COMMAND),
        0,
        "a command held back and not queued is not shown as sent: {frame}"
    );
    assert_eq!(
        counted(directory.path(), "after-turn-queues"),
        0,
        "nothing was queued"
    );
    let held = daemon_state(directory.path());
    assert_eq!(held["pending_commands"], 0, "{held}");

    std::fs::remove_file(flag("hide-receipts")).unwrap();
    view.send(b"\r");
    let frame = view.wait_for_rows("the command queued behind the active turn", |frame| {
        screen::split_at_queue(frame).is_some_and(|(_, preview)| {
            preview
                .lines()
                .take_while(|line| !line.starts_with('─'))
                .any(|line| line.contains(COMMAND))
        })
    });
    let (body, preview) = screen::split_at_queue(&frame).unwrap();
    let preview = preview
        .lines()
        .take_while(|line| !line.starts_with('─'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!body.contains(COMMAND), "{frame}");
    assert_eq!(
        preview.matches(COMMAND).count(),
        1,
        "the queued command appears once in the pending preview: {frame}"
    );
    assert_eq!(echoed(&frame, FIRST), 1, "{frame}");
    assert_eq!(
        echoed(&frame, COMMAND),
        0,
        "the queued command is not shown as consumed: {frame}"
    );
    assert_eq!(
        submits(directory.path()),
        1,
        "the first input was not sent again"
    );
    assert_eq!(
        counted(directory.path(), "after-turn-queues"),
        1,
        "queued once"
    );
    let queued = daemon_state(directory.path());
    assert_eq!(
        (
            &queued["running"],
            &queued["pending_commands"],
            &queued["pending_inputs"]
        ),
        (
            &serde_json::json!(true),
            &serde_json::json!(1),
            &serde_json::json!(0)
        ),
        "the command waits behind the active turn: {queued}"
    );

    std::fs::remove_file(flag("hold-model")).unwrap();
    let frame = view.wait_for_rows("the queued command ran after the turn", |frame| {
        frame.contains("edited reply") && screen::queued_inputs(frame).is_empty()
    });
    assert_eq!(echoed(&frame, FIRST), 1, "{frame}");
    assert_eq!(
        echoed(&frame, COMMAND),
        1,
        "the consumed command is shown once: {frame}"
    );
    assert_eq!(submits(directory.path()), 1, "nothing was resent");
    assert_eq!(
        counted(directory.path(), "after-turn-queues"),
        1,
        "queued once"
    );
    close(&mut view);
    assert_eq!(
        std::fs::read_to_string(flag("model-requests")).unwrap(),
        "2",
        "the first input once, then the queued command once"
    );
}
