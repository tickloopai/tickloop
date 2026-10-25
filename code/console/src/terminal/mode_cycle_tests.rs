//! Run-loop regression: Shift+Tab shows the mode its change confirmed at once, on the New Agent
//! and on a Chat, without a status refresh that reads the whole Chat again; a refused change is
//! still reported and leaves the shown mode alone.
use super::*;
use async_trait::async_trait;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tickloop_code_daemon::EmbeddedAgentAccess;
use uuid::Uuid;

use tickloop_code_access::{
    AccessError, AgentAccess, AgentCommand, AgentDetailQuery, CommandEnvelope, CommandReceipt,
    DisplayReceipt, DisplayUpdate, LaunchSpec, Observation, ObservationRequest, ViewRef,
};

use tickloop_harness_test_support::screen;

const FIXTURE_VARIABLE: &str = "TICKLOOP_MODE_CYCLE_FIXTURE";
const DIRECTORY_VARIABLE: &str = "TICKLOOP_MODE_CYCLE_DIRECTORY";
/// Set to start the View on a New Agent draft instead of an existing Agent.
const DRAFT_VARIABLE: &str = "TICKLOOP_MODE_CYCLE_DRAFT";
const TRAFFIC_FILE: &str = "traffic";
/// While this file exists the daemon refuses every configuration change.
const REFUSE_FLAG: &str = "refuse-configure";
const REFUSAL: &str = "fixture daemon refuses the change";
const WIDTH: usize = 100;
const HEIGHT: usize = 30;
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long the traffic must stay unchanged before the View counts as idle.
const QUIET_PERIOD: Duration = Duration::from_millis(400);

/// The daemon the View talks to. It appends one line to `traffic` for each whole-Chat read
/// (`snapshot`), Chat status read (`status`), Tick subscription read (`subscriptions`, asked only
/// by a status refresh) and configuration change (`configure`).
struct CountingDaemon {
    daemon: EmbeddedAgentAccess,
    directory: PathBuf,
}

impl CountingDaemon {
    fn note(&self, entry: &str) {
        let mut traffic = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join(TRAFFIC_FILE))
            .unwrap();
        writeln!(traffic, "{entry}").unwrap();
    }
}

#[async_trait]
impl AgentAccess for CountingDaemon {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        match &query {
            ObservationRequest::Agent {
                detail: AgentDetailQuery::Snapshot,
                ..
            } => self.note("snapshot"),
            ObservationRequest::Agent {
                detail: AgentDetailQuery::Status,
                ..
            } => self.note("status"),
            ObservationRequest::Agent {
                detail: AgentDetailQuery::Subscriptions,
                ..
            } => self.note("subscriptions"),
            _ => {}
        }
        self.daemon.observe(query).await
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        if matches!(command.command, AgentCommand::Configure { .. }) {
            self.note("configure");
            if self.directory.join(REFUSE_FLAG).exists() {
                return Err(AccessError::invalid(REFUSAL));
            }
        }
        self.daemon.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.daemon.show_view(update).await
    }
}

#[test]
#[ignore = "owned PTY producer for the Shift+Tab run-loop regression"]
fn mode_cycle_pty_fixture() {
    if std::env::var(FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("mode-cycle-fixture".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_mode_cycle_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_mode_cycle_fixture() {
    let directory = PathBuf::from(std::env::var_os(DIRECTORY_VARIABLE).unwrap());
    let core = tickloop_harness_runtime::Agent::new_embedded(
        tickloop_harness_test_support::config(),
        Arc::new(tickloop_harness_test_support::ScriptedModel::new(vec![])),
    );
    let workdir = std::env::current_dir().unwrap();
    let adapter = Arc::new(CountingDaemon {
        daemon: EmbeddedAgentAccess::new(core.clone()),
        directory,
    });
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(tickloop_harness_test_support::config());
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
    // Without it nothing but the key itself may paint the mode or read the Chat.
    agent.stop_global_refresh();
    let chat = if std::env::var_os(DRAFT_VARIABLE).is_some() {
        StartupChat::New
    } else {
        let chat = agent.create_chat(workdir.clone()).await.unwrap();
        StartupChat::Resumed(Box::new(agent.snapshot(chat).await.unwrap()), false)
    };
    let (console, input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let startup = StartupState {
        model_selected: true,
        launch_arguments: Vec::new(),
        chat,
        user_settings: None,
    };
    let result = interact(&agent, workdir, &console, input, startup, None).await;
    drop(console);
    core.shutdown().await;
    result.unwrap();
}

struct ModeCyclePty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    directory: PathBuf,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl ModeCyclePty {
    /// Starts the fixture View in a PTY, on a New Agent draft when `draft` is set.
    fn start(directory: &Path, draft: bool) -> Self {
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
            "terminal::mode_cycle_tests::mode_cycle_pty_fixture",
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
        Self {
            child,
            writer,
            output,
            directory: directory.to_path_buf(),
            _master: pair.master,
        }
    }

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

    fn wait_for(&mut self, label: &str, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let frame = self.frame();
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
        panic!("the View never showed {label}: {}", self.frame());
    }

    /// The View's daemon traffic so far, one entry per counted request.
    fn traffic(&self) -> Vec<String> {
        std::fs::read_to_string(self.directory.join(TRAFFIC_FILE))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Waits until the View has sent nothing counted for [`QUIET_PERIOD`] and returns how much
    /// it had sent by then.
    fn settled_traffic(&self) -> usize {
        let deadline = Instant::now() + TIMEOUT;
        let mut seen = self.traffic().len();
        let mut quiet_since = Instant::now();
        while quiet_since.elapsed() < QUIET_PERIOD {
            assert!(Instant::now() < deadline, "the View never settled");
            std::thread::sleep(Duration::from_millis(20));
            let now = self.traffic().len();
            if now != seen {
                seen = now;
                quiet_since = Instant::now();
            }
        }
        seen
    }

    /// Presses Shift+Tab and waits until the status bar shows a mode other than `shown`.
    fn cycle_from(&mut self, shown: &str) -> String {
        self.send(b"\x1b[Z");
        let frame = self.wait_for("the next execution mode", |frame| {
            status_mode(frame).is_some_and(|mode| mode != shown)
        });
        status_mode(&frame).unwrap()
    }
}

impl Drop for ModeCyclePty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn status_mode(frame: &str) -> Option<String> {
    frame
        .lines()
        .find_map(|line| line.split_once("⏵⏵ ").map(|(_, rest)| rest))
        .and_then(|rest| rest.split(" · ").next())
        .map(str::to_owned)
}

#[test]
fn shift_tab_on_the_new_agent_paints_the_staged_mode_without_another_frame() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = ModeCyclePty::start(directory.path(), true);
    let initial = view.wait_for("the New Agent's mode", |frame| {
        frame.contains('❯') && status_mode(frame).is_some()
    });
    let initial = status_mode(&initial).unwrap();
    let next = view.cycle_from(&initial);
    view.cycle_from(&next);
    assert!(
        view.traffic().iter().all(|entry| entry != "configure"),
        "a New Agent stages its mode without the daemon: {:?}",
        view.traffic()
    );
}

#[test]
fn shift_tab_on_a_chat_reads_it_once_and_skips_the_status_refresh() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = ModeCyclePty::start(directory.path(), false);
    let initial = view.wait_for("the Chat's mode", |frame| {
        frame.contains('❯') && status_mode(frame).is_some()
    });
    let initial = status_mode(&initial).unwrap();

    std::fs::write(directory.path().join(REFUSE_FLAG), "").unwrap();
    view.send(b"\x1b[Z");
    let refused = view.wait_for("the refused change", |frame| {
        frame.contains("Cannot change execution mode") && frame.contains(REFUSAL)
    });
    assert_eq!(
        status_mode(&refused).as_deref(),
        Some(initial.as_str()),
        "a refused change keeps the shown mode: {refused}"
    );
    std::fs::remove_file(directory.path().join(REFUSE_FLAG)).unwrap();

    let before = view.settled_traffic();
    let next = view.cycle_from(&initial);
    let traffic = view.traffic()[before..].to_vec();
    let configure = traffic
        .iter()
        .position(|entry| entry == "configure")
        .unwrap_or_else(|| panic!("the change reached the daemon: {traffic:?}"));
    assert_eq!(
        traffic[..configure],
        ["status"],
        "one status read of the Chat decides the next mode: {traffic:?}"
    );
    assert!(
        !traffic.iter().any(|entry| entry == "subscriptions"),
        "no status refresh runs before the confirmed mode shows: {traffic:?}"
    );
    view.cycle_from(&next);
}
