//! Run-loop regression: an unreachable daemon leaves the View, its drafts and its feedback alive.
use super::*;
use async_trait::async_trait;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tickloop_code_daemon::EmbeddedAgentAccess;
use uuid::Uuid;

use tickloop_code_access::{
    AccessError, AgentAccess, CommandEnvelope, CommandReceipt, DisplayReceipt, DisplayUpdate,
    LaunchSpec, Observation, ObservationRequest, ViewRef,
};

use tickloop_harness::test_support::screen;

const FIXTURE_VARIABLE: &str = "TICKLOOP_DAEMON_LOSS_FIXTURE";
const UNREACHABLE_VARIABLE: &str = "TICKLOOP_DAEMON_LOSS_FLAG";
const WIDTH: usize = 100;
const HEIGHT: usize = 30;
const TIMEOUT: Duration = Duration::from_secs(10);
const OVERVIEW_RESPONSE_DELAY: Duration = Duration::from_millis(250);

/// Fails catalog and Agent observations while the flag file exists, as an unreachable daemon does.
/// Slow catalog replies separate the initial prompt from the initialized cached View.
struct SwitchableDaemon {
    daemon: EmbeddedAgentAccess,
    unreachable: std::path::PathBuf,
}

#[async_trait]
impl AgentAccess for SwitchableDaemon {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if matches!(query, ObservationRequest::Overview { .. }) {
            tokio::time::sleep(OVERVIEW_RESPONSE_DELAY).await;
        }
        if self.unreachable.exists()
            && matches!(
                query,
                ObservationRequest::Overview { .. } | ObservationRequest::Agent { .. }
            )
        {
            return Err(AccessError::unavailable("daemon socket is unreachable"));
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
#[ignore = "owned PTY producer for the daemon-loss run-loop regression"]
fn daemon_loss_pty_fixture() {
    if std::env::var(FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("daemon-loss-fixture".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_daemon_loss_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_daemon_loss_fixture() {
    let core = tickloop_harness::runtime::Agent::new_embedded(
        tickloop_harness::test_support::config(),
        Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
    );
    let workdir = std::env::current_dir().unwrap();
    let adapter = Arc::new(SwitchableDaemon {
        daemon: EmbeddedAgentAccess::new(core.clone()),
        unreachable: std::env::var_os(UNREACHABLE_VARIABLE).unwrap().into(),
    });
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(tickloop_harness::test_support::config());
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
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let (console, input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let startup = StartupState {
        model_selected: true,
        launch_arguments: Vec::new(),
        chat: StartupChat::Resumed(Box::new(agent.snapshot(chat).await.unwrap()), false),
        user_settings: None,
    };
    let result = interact(&agent, workdir, &console, input, startup, None).await;
    drop(console);
    core.shutdown().await;
    result.unwrap();
}

struct DaemonLossPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl DaemonLossPty {
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
        panic!("daemon-loss View never showed {label}: {}", self.frame());
    }
}

impl Drop for DaemonLossPty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
fn an_unreachable_daemon_keeps_the_view_drafts_and_feedback_alive() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let flag = directory.path().join("daemon-unreachable");
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
        "terminal::daemon_loss_tests::daemon_loss_pty_fixture",
        "--nocapture",
    ]);
    command.env(FIXTURE_VARIABLE, "1");
    command.env(UNREACHABLE_VARIABLE, &flag);
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
    let mut view = DaemonLossPty {
        child,
        writer,
        output,
        _master: pair.master,
    };
    view.wait_for("the initialized session input", |frame| {
        frame.contains("❯") && frame.contains("⏵⏵ ")
    });
    std::fs::write(&flag, "unreachable").unwrap();
    view.send(b"\x1b[D");
    view.wait_for("the cached overview with a stale label", |frame| {
        frame.contains("Agents · local") && frame.contains("Disconnected")
    });
    view.send(b"\tkept dispatch draft");
    view.wait_for(
        "the dispatch draft while the daemon is unreachable",
        |frame| frame.contains("kept dispatch draft") && frame.contains("Disconnected"),
    );
    std::fs::remove_file(&flag).unwrap();
    view.wait_for(
        "the stale label to clear once the daemon answers",
        |frame| !frame.contains("Disconnected") && frame.contains("kept dispatch draft"),
    );
    view.send(b"\x15/model\r");
    view.wait_for("the rejected command feedback", |frame| {
        frame.contains("/model isn't available")
    });
    std::thread::sleep(Duration::from_millis(2500));
    let frame = view.frame();
    assert!(
        frame.contains("/model isn't available"),
        "overview refreshes keep the latest action feedback: {frame}"
    );
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
