#![cfg(unix)]

use crate::support;
use tickloop_harness_test_support::model_http_support;
use tickloop_harness_test_support::screen::SELECTED_MARK;

mod artifact_attachment;
mod auto_review;
mod bash_background;
mod command_feedback;
mod daemon_shutdown;
mod dialog_parity;
mod family_messaging;
mod family_navigation;
mod hosted_monitor;
mod interrupted_prompt;
mod monitor_width;
mod nested_family;
mod other_tickhost;
mod provider_errors;
mod question_decline;
mod reply_visibility;
mod resume_picker;
mod task_tools_setting;
mod unified_footer;

use std::{
    io::{Read, Write},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::FutureExt;
use portable_pty::{PtySize, native_pty_system};
use serde_json::{Value, json};
use tickloop_code_client::LocalAgentClient;

use tickloop_code_access::{
    AgentAccess, AgentCommand, AgentDetailQuery, AgentFilter, AgentSummary, CommandEnvelope,
    CommandResult, ConfigurationChange, ExecutionRef, LaunchSpec, LiveChatSnapshot,
    ObservationRequest, ObservationValue, Page, PendingRequestKind, RequestRef, TaskOrigin,
    TaskRef, UserInput,
};
use tickloop_harness_protocol::UserShellState;

use tickloop_harness_test_support::screen;

const STARTUP_BANNER_TITLE: &str = concat!("Tickloop v", env!("CARGO_PKG_VERSION"));
const TERMINAL_WIDTH: usize = 100;
const TERMINAL_HEIGHT: usize = 32;
const UI_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the topmost history line must stay unchanged before wheel scrolling counts as done.
const WHEEL_SETTLE_TIME: Duration = Duration::from_millis(1500);
const COMPLETED_FRAME_START: &str = "\x1b[?2026h";
const COMPLETED_FRAME_END: &str = "\x1b[?2026l";
const CURSOR_VISIBLE: &str = "\x1b[?25h";
const CURSOR_HIDDEN: &str = "\x1b[?25l";
/// A finished turn's line: one of Claude Code's verbs with the run's time, or restored without it.
static TURN_SUMMARY: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"⏺ (\p{L}+ for |Done)").unwrap());

/// A private CLI environment. Tests stop it with `shutdown`; dropping it after a failure stops the
/// Tickhost and removes the directory, which ends the daemon and its workers.
struct PrivateFixture {
    directory: tempfile::TempDir,
}

impl PrivateFixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory = support::runtime_dir();
        for name in ["config", "home", "work", "scratch"] {
            std::fs::create_dir(directory.path().join(name)).unwrap();
            std::fs::set_permissions(
                directory.path().join(name),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let mut model = tickloop_harness_test_support::config();
        model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
        std::fs::write(
            directory.path().join("model.json"),
            serde_json::to_vec(&model).unwrap(),
        )
        .unwrap();
        let runtime = support::cli_runtime::adopt(&directory.path().join("config"));
        tickloop_code_client::daemon_endpoint::validate_directory(&runtime).unwrap();
        Self { directory }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        let path = std::path::Path::new(name);
        let (directory, suffix) = if let Ok(suffix) = path.strip_prefix("runtime") {
            (
                support::cli_runtime::daemon_directory_of(&self.directory.path().join("config")),
                suffix,
            )
        } else if let Ok(suffix) = path.strip_prefix("tickhost") {
            (
                tickhost::paths::HostPaths::of_config_root(Some(
                    &self.directory.path().join("config"),
                ))
                .runtime,
                suffix,
            )
        } else {
            return self.directory.path().join(path);
        };
        if suffix.as_os_str().is_empty() {
            directory
        } else {
            directory.join(suffix)
        }
    }

    fn set_model(&self, model: &tickloop_harness_protocol::ModelConfig) {
        std::fs::write(self.path("model.json"), serde_json::to_vec(model).unwrap()).unwrap();
    }

    fn set_chat_model(&self, base_url: &str) -> tickloop_harness_protocol::ModelConfig {
        let mut model = tickloop_harness_test_support::config();
        model.protocol = tickloop_harness_protocol::Protocol::ChatCompletions;
        model.base_url = base_url.trim_end_matches("/anthropic").into();
        model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
        self.set_model(&model);
        tickloop_harness_model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap()
    }

    async fn client(&self) -> LocalAgentClient {
        let endpoint = self.path("runtime/daemon.sock");
        tokio::time::timeout(UI_TIMEOUT, async {
            loop {
                if let Ok(client) = LocalAgentClient::connect(&endpoint).await {
                    break client;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the private CLI daemon must become ready")
    }

    async fn shutdown(&self) {
        support::stop_tickhost(&self.path("tickhost"), &self.path("runtime/tickhost.key"));
        let endpoint = self.path("runtime/daemon.sock");
        if endpoint.exists() {
            let client = self.client().await;
            client.shutdown_daemon().await.unwrap();
            tokio::time::timeout(UI_TIMEOUT, async {
                loop {
                    let sockets = std::fs::read_dir(self.path("runtime"))
                        .unwrap()
                        .filter_map(Result::ok)
                        .any(|entry| {
                            entry
                                .path()
                                .extension()
                                .is_some_and(|value| value == "sock")
                        });
                    if !sockets {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("private daemon and worker sockets must close gracefully");
        }
    }
}

impl Drop for PrivateFixture {
    fn drop(&mut self) {
        support::stop_tickhost(&self.path("tickhost"), &self.path("runtime/tickhost.key"));
    }
}

struct PtyView {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    width: usize,
    height: usize,
}

impl PtyView {
    fn start(fixture: &PrivateFixture) -> Self {
        Self::start_with_flags(fixture, &[])
    }

    fn start_with_flags(fixture: &PrivateFixture, flags: &[&str]) -> Self {
        Self::start_with_environment(fixture, flags, &[])
    }

    fn start_with_environment(
        fixture: &PrivateFixture,
        flags: &[&str],
        environment: &[(&str, &std::path::Path)],
    ) -> Self {
        Self::launch(fixture, flags, environment, false)
    }

    /// Keeps Chat storage on, so conversations saved under the fixture's config directory can be
    /// listed and opened.
    fn start_saving_chats(fixture: &PrivateFixture) -> Self {
        Self::launch(fixture, &[], &[], true)
    }

    fn launch(
        fixture: &PrivateFixture,
        flags: &[&str],
        environment: &[(&str, &std::path::Path)],
        saves_chats: bool,
    ) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: TERMINAL_HEIGHT as u16,
                cols: TERMINAL_WIDTH as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = support::fixture_cli::pty_command();
        command.args(["--working-directory-is-trusted", "--transport-fixture"]);
        command.arg(fixture.path("model.json"));
        if !saves_chats {
            command.arg("--no-session-persistence");
        }
        command.args(flags);
        command.cwd(fixture.path("work"));
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("HOME", fixture.path("home"));
        command.env("TICKLOOP_CONFIG_DIR", fixture.path("config"));
        command.env("TICKLOOP_TMPDIR", fixture.path("scratch"));
        command.env("TMPDIR", fixture.path("scratch"));
        command.env("TICKLOOP_FIXTURE_API_KEY", "fixture-key");
        command.env("TICKLOOP_AGENT_TUI", "fullscreen");
        command.env("TERM", "xterm-256color");
        for (name, value) in environment {
            command.env(name, value);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
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
            master: pair.master,
            width: TERMINAL_WIDTH,
            height: TERMINAL_HEIGHT,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// The screen, with the selection fill marked by [`screen::SELECTED_MARK`].
    fn frame(&self) -> String {
        screen::visible_screen_with_selection(
            &String::from_utf8_lossy(&self.output.lock().unwrap()),
            self.width,
            self.height,
        )
    }

    async fn resize(&mut self, width: usize, height: usize) {
        let mark = self.output.lock().unwrap().len();
        self.master
            .resize(PtySize {
                rows: height as u16,
                cols: width as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.width = width;
        self.height = height;
        tokio::time::timeout(UI_TIMEOUT, async {
            loop {
                if String::from_utf8_lossy(&self.output.lock().unwrap()[mark..])
                    .contains("\x1b[?2026l")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("OS PTY resize must produce a completed terminal frame");
    }

    async fn wait_for(&mut self, label: &str, matches: impl Fn(&str) -> bool) -> String {
        self.wait_for_within(label, UI_TIMEOUT, matches).await
    }

    async fn wait_for_within(
        &mut self,
        label: &str,
        timeout: Duration,
        matches: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let frame = self.frame();
            if matches(&frame) {
                return frame;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "CLI exited while waiting for {label}: {frame}\nRaw output: {}",
                String::from_utf8_lossy(&self.output.lock().unwrap())
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let frame = self.frame();
        assert!(matches(&frame), "CLI PTY {label}: {frame}");
        frame
    }

    async fn wait_for_editor(&mut self, label: &str, expected_draft: Option<&str>) {
        let output = Arc::clone(&self.output);
        self.wait_for(label, |frame| {
            let bytes = output.lock().unwrap();
            let ansi = String::from_utf8_lossy(&bytes);
            let Some(end) = ansi.rfind(COMPLETED_FRAME_END) else {
                return false;
            };
            let Some(start) = ansi[..end].rfind(COMPLETED_FRAME_START) else {
                return false;
            };
            let completed = &ansi[start..end];
            let cursor_is_visible = completed.rfind(CURSOR_VISIBLE).is_some_and(|shown| {
                completed
                    .rfind(CURSOR_HIDDEN)
                    .is_none_or(|hidden| shown > hidden)
            });
            cursor_is_visible
                && !frame.contains("Shell running")
                && frame.lines().any(|line| {
                    line.strip_prefix('❯').is_some_and(|text| {
                        expected_draft.is_none_or(|draft| {
                            text.trim() == draft
                                || (draft.is_empty()
                                    && text.trim().starts_with("Message @")
                                    && text.trim().ends_with('…'))
                        })
                    })
                })
        })
        .await;
    }

    async fn keep_history_paused(&mut self) {
        self.wait_for("history offers Continue", |frame| {
            frame.contains("Continue Chat?") && frame.contains("Keep paused")
        })
        .await;
        self.send(b"\x1b");
        self.wait_for_editor("Esc returns to the history editor", Some(""))
            .await;
        self.wait_for("Esc keeps Trigger receiving paused", |frame| {
            !frame.contains("Continue Chat?")
                && frame.contains("Triggers paused · send a message to resume")
        })
        .await;
    }

    async fn close(&mut self) {
        self.wait_for_editor("editor owns the close input", None)
            .await;
        self.send(b"\x15\x7f");
        self.wait_for_editor("cleared editor is ready to close", Some(""))
            .await;
        self.send(b"\x04\x04");
        tokio::time::timeout(UI_TIMEOUT, async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "CLI closed unsuccessfully: {}",
                        self.frame()
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Ctrl+D twice must close the View without stopping its Agent: {}",
                self.frame()
            )
        });
    }
}

impl Drop for PtyView {
    fn drop(&mut self) {
        let _ = self.writer.write_all(b"\x1b\x15\x04\x04");
        let _ = self.writer.flush();
    }
}

#[tokio::test]
async fn pty_wait_accepts_ready_output_captured_before_a_delayed_final_check() {
    const READY: &str = "PTY-WAIT-FINAL-READY";
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let gate = directory.path().join("ready");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: TERMINAL_HEIGHT as u16,
            cols: TERMINAL_WIDTH as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.args([
        "-c",
        r#"printf '\033[?2026h\033[2J\033[HWAITING\r\n\033[?2026l'; while [ ! -e "$1" ]; do sleep 0.01; done; printf '\033[?2026h\033[2J\033[HPTY-WAIT-FINAL-READY\r\n\033[?2026l'; read -r finish"#,
        "view-wait-fixture",
    ]);
    command.arg(&gate);
    let child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    let (ready, captured_ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut ready = Some(ready);
        let mut bytes = [0; 1024];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            let mut output = captured.lock().unwrap();
            output.extend_from_slice(&bytes[..count]);
            if screen::visible_screen_with_selection(
                &String::from_utf8_lossy(&output),
                TERMINAL_WIDTH,
                TERMINAL_HEIGHT,
            )
            .contains(READY)
                && let Some(ready) = ready.take()
            {
                let _ = ready.send(());
            }
        }
    });
    let mut view = PtyView {
        child,
        writer,
        output,
        master: pair.master,
        width: TERMINAL_WIDTH,
        height: TERMINAL_HEIGHT,
    };
    let result = AssertUnwindSafe(async {
        view.wait_for("controlled PTY waiting", |frame| frame.contains("WAITING"))
            .await;
        let polls = std::cell::Cell::new(0);
        let polling_timeout = Duration::from_secs(1);
        let frame = view
            .wait_for_within("captured ready frame", polling_timeout, |frame| {
                polls.set(polls.get() + 1);
                if polls.get() == 1 {
                    std::fs::write(&gate, []).unwrap();
                    captured_ready.recv_timeout(UI_TIMEOUT).unwrap();
                    // The child and PTY reader progress while the polling task cannot run.
                    std::thread::sleep(polling_timeout);
                }
                frame.contains(READY)
            })
            .await;
        assert!(frame.contains(READY));
        assert_eq!(
            polls.get(),
            2,
            "the final captured frame must be checked once"
        );
    })
    .catch_unwind()
    .await;
    let _ = view.child.kill();
    let _ = view.child.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn catalog(client: &LocalAgentClient) -> Vec<AgentSummary> {
    let observation = client
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 256,
            },
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Overview(rows) = observation.initial else {
        panic!("expected live Agent catalog");
    };
    rows
}

async fn wait_for_display_views(
    client: &LocalAgentClient,
    execution: ExecutionRef,
    expected: usize,
) {
    let mut latest = None;
    let ready = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            latest = catalog(client)
                .await
                .into_iter()
                .find(|row| row.execution.agent_id == execution.agent_id)
                .map(|row| (row.execution, row.views.len()));
            if latest == Some((execution, expected)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        ready.is_ok(),
        "expected {expected} registered Views for {execution:?}; latest: {latest:?}"
    );
}

/// A user message the person typed, not guidance the host wrote into the history for the model.
fn is_user_prompt(message: &Value) -> bool {
    let text = message["content"]
        .as_str()
        .or_else(|| message["content"][0]["text"].as_str());
    message["role"] == "user"
        && !text.is_some_and(|text| text.starts_with("Reference guidance from "))
}
fn family_panel_has_viewing_row(frame: &str) -> bool {
    frame
        .lines()
        .skip_while(|line| !line.contains("Subagents "))
        .skip(1)
        .any(|line| line.contains('●'))
}

fn new_input_is_visible(frame: &str) -> bool {
    frame.contains("Tickloop")
        && frame.contains("⏵⏵ ")
        && frame.lines().any(|line| line.starts_with('❯'))
}

async fn snapshot(client: &LocalAgentClient, execution: ExecutionRef) -> Box<LiveChatSnapshot> {
    let observation = client
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(snapshot) = observation.initial else {
        panic!("expected live Agent snapshot");
    };
    snapshot
}

async fn run_lifecycle(fixture: &PrivateFixture) {
    let mut first = PtyView::start(fixture);
    first.wait_for("new View ready", new_input_is_visible).await;
    let client = fixture.client().await;
    assert!(
        catalog(&client).await.is_empty(),
        "default startup must create no execution"
    );
    first.send(b"\x1b[D");
    first
        .wait_for("empty Left opens overview", |frame| {
            frame.contains("Agents · local")
        })
        .await;
    assert!(
        catalog(&client).await.is_empty(),
        "opening overview must create no execution"
    );
    let overview = first.frame();
    assert!(overview.contains("Needs input"), "{overview}");
    assert!(
        overview.contains(&format!(
            "{SELECTED_MARK} ◆ New Agent · current session · send a prompt to start"
        )),
        "{overview}"
    );
    assert!(overview.contains("approvals 0 · questions 0"), "{overview}");
    for return_key in [b"\r".as_slice(), b"\x1b[C".as_slice()] {
        first.send(return_key);
        first
            .wait_for("current draft returns without creating an Agent", |frame| {
                frame.contains(STARTUP_BANNER_TITLE) && !frame.contains("Agents · local")
            })
            .await;
        assert!(catalog(&client).await.is_empty());
        first.send(b"\x1b[D");
        first
            .wait_for("Left selects current draft again", |frame| {
                frame.contains(&format!("{SELECTED_MARK} ◆ New Agent · current session"))
            })
            .await;
    }
    first.send(b"\t!printf 'shell began'; while [ ! -f finish ]; do sleep 0.05; done; printf 'shell ended'\r");
    let execution = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(&client).await;
            if let Some(row) = rows.first() {
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    row.folder,
                    Some(fixture.path("work").canonicalize().unwrap())
                );
                break row.execution;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("overview Shell dispatch must create one Agent");
    let running = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let snapshot = snapshot(&client, execution).await;
            if let Some(shell) = snapshot.user_shells.first()
                && shell.state == UserShellState::Background
            {
                assert!(shell.starts_in_background);
                assert!(shell.output_ref.as_ref().is_some_and(|path| {
                    path.starts_with(fixture.path("scratch").canonicalize().unwrap())
                }));
                assert!(
                    snapshot.last_model_run.is_none(),
                    "user Shell must not invoke a model"
                );
                assert!(snapshot.events.iter().all(|event| !matches!(
                    event.kind,
                    tickloop_harness_protocol::EventKind::Started
                )));
                break shell.id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("real worker must start the Shell");
    first
        .wait_for("a Shell dispatch keeps overview", |frame| {
            frame.contains("Agents · local")
        })
        .await;
    first.close().await;
    assert_eq!(
        snapshot(&client, execution).await.user_shells[0].state,
        UserShellState::Background
    );
    assert!(catalog(&client).await[0].views.is_empty());
    let mut reopened = PtyView::start(fixture);
    reopened
        .wait_for("replacement View ready", new_input_is_visible)
        .await;
    assert_eq!(
        catalog(&client).await.len(),
        1,
        "reopening the CLI must not create another execution"
    );
    reopened.send(b"\x1b[D");
    reopened
        .wait_for("replacement overview", |frame| {
            frame.contains("Agents · local")
        })
        .await;
    reopened.send(b"\x1b[B\x1b[B\r");
    reopened
        .wait_for(
            "open live Agent exposes its real background Task",
            |frame| frame.contains("Monitor 1") && !frame.contains("Agents · local"),
        )
        .await;
    let rows = catalog(&client).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].execution, execution);
    assert_eq!(rows[0].views.len(), 1);
    reopened.send(b"?");
    reopened
        .wait_for("help replaces the real Task summary", |frame| {
            frame.contains("ctrl+j newline") && !frame.contains("Monitor 1")
        })
        .await;
    reopened.send(b"\x1b");
    reopened
        .wait_for("help restores the real Task summary", |frame| {
            frame.contains("Monitor 1")
        })
        .await;
    reopened.send(b"\x1b[C");
    reopened
        .wait_for("Right focuses the real Shell Tick", |frame| {
            frame.contains("Shells (1)")
        })
        .await;
    reopened.send(b"\r");
    // The Shell's own view, drawn by the Tickhost, takes the terminal.
    view_shows(
        &reopened,
        &["$ printf 'shell began'; while [ ! -f finish ]", "running"],
    )
    .await;
    std::fs::write(fixture.path("work/finish"), b"finish").unwrap();
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let snapshot = snapshot(&client, execution).await;
            if snapshot
                .user_shells
                .iter()
                .any(|shell| shell.id == running && shell.state == UserShellState::Completed)
            {
                assert!(snapshot.last_model_run.is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the background Shell must complete after the first View closes");
    let output = client
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::TaskOutput {
                task: TaskRef {
                    execution,
                    task_id: running,
                    origin: TaskOrigin::User,
                },
                offset: 0,
                limit: 1024,
            },
            after: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(output.initial, ObservationValue::TaskOutput(output) if output.output == "shell beganshell ended")
    );
    // The view follows the completion after the View detach.
    view_shows(&reopened, &["shell beganshell ended", "exit 0"]).await;
    reopened.send(b"\x1b");
    tokio::time::timeout(UI_TIMEOUT, async {
        while terminal_screen(&reopened).contains("$ printf 'shell began'") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Esc closes the Shell's view");
    reopened
        .wait_for("the Shell's row is selected again", |frame| {
            frame.contains("Shells (1)")
        })
        .await;
    let captured_output = Arc::clone(&reopened.output);
    reopened.send(b"\x1b");
    reopened
        .wait_for("Task Esc returns to ordinary input", |frame| {
            let captured = captured_output.lock().unwrap();
            let terminal = String::from_utf8_lossy(&captured);
            frame.lines().any(|line| line.trim() == "❯")
                && terminal.rfind("\x1b[?25h") > terminal.rfind("\x1b[?25l")
        })
        .await;
    reopened.close().await;
}

#[tokio::test]
async fn actual_cli_dispatch_detach_and_reopen_preserve_the_same_execution() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_lifecycle(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_shell_external_editor(fixture: &PrivateFixture) {
    use std::os::unix::fs::PermissionsExt;

    let editor = fixture.path("editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nset -eu\ncat \"$1\" > editor-draft\npwd > editor-cwd\nprintf '%s' \"$1\" > editor-path\nprintf \"printf 'edited 中文'\\n\" > \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut view = PtyView::start_with_environment(fixture, &[], &[("VISUAL", &editor)]);
    view.wait_for("editor View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    view.send(b"!printf original");
    view.wait_for("unsubmitted Shell draft", |frame| {
        frame.contains("!\u{a0}printf original")
    })
    .await;
    view.send(b"\x07");
    view.wait_for("Shell editor returns to the focused Shell draft", |frame| {
        frame.contains("!\u{a0}printf 'edited 中文'")
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(fixture.path("work/editor-draft")).unwrap(),
        "printf original"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.path("work/editor-cwd"))
            .unwrap()
            .trim(),
        fixture
            .path("work")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
    let temporary = std::fs::read_to_string(fixture.path("work/editor-path")).unwrap();
    assert!(!std::path::Path::new(&temporary).exists());
    assert!(catalog(&client).await.is_empty());
    view.send(b"j");
    view.wait_for("ordinary typing remains in edited Shell input", |frame| {
        frame.contains("!\u{a0}printf 'edited 中文'j")
    })
    .await;
    view.send(b"\x7f\r");
    let execution = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let Some(row) = catalog(&client).await.first() {
                break row.execution;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("edited Shell submission must create one Agent");
    let completed = wait_snapshot(&client, execution, "edited Shell completes", |snapshot| {
        snapshot
            .user_shells
            .iter()
            .any(|shell| shell.state == UserShellState::Completed)
    })
    .await;
    assert_eq!(completed.user_shells.len(), 1);
    assert_eq!(completed.user_shells[0].command, "printf 'edited 中文'");
    assert!(completed.last_model_run.is_none());
    let shown = view
        .wait_for("edited Shell output", |frame| {
            frame
                .lines()
                .any(|line| screen::response_line(line) == "edited 中文")
        })
        .await;
    assert!(
        shown.contains("  ⎿  edited 中文") && !shown.contains("⏺ Shell"),
        "a `!` command shows its echo and its output only: {shown}"
    );
    view.close().await;
}

#[tokio::test]
async fn actual_cli_shell_external_editor_preserves_mode_focus_and_submission() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_shell_external_editor(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_working_agent_detach(fixture: &PrivateFixture) {
    let (base_url, mut requests) = model_http_support::serve_responses(vec![
        (200, chat_reply(Some(("Bash", "hold-running", json!({
            "command":"printf 'work started\\n'; while [ ! -f finish ]; do sleep 0.02; done; printf 'work finished\\n'",
        }))), "")),
        (200, chat_reply(None, "original execution completed after reattach")),
    ]).await;
    let mut model = tickloop_harness_test_support::config();
    model.protocol = tickloop_harness_protocol::Protocol::ChatCompletions;
    model.base_url = base_url.trim_end_matches("/anthropic").into();
    model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
    fixture.set_model(&model);
    let mut first = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    first
        .wait_for("working View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    first.send(b"Run the bounded work and report completion.\r");
    let execution = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let Some(row) = catalog(&client).await.first() {
                break row.execution;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("ordinary prompt must create the working Agent");
    let running = wait_snapshot(&client, execution, "original model work remains Running", |snapshot| {
        snapshot.conversation.chat_loop.state == tickloop_harness_protocol::LoopState::Running
            && snapshot.events.iter().any(|event| matches!(&event.kind, tickloop_harness_protocol::EventKind::ToolStarted { name, .. } if name == "Bash"))
    }).await;
    let loop_id = running.conversation.chat_loop.id;
    let run_id = running.conversation.chat_loop.run_id;
    first
        .wait_for("working Agent visible before detach", |frame| {
            frame.contains("work started")
        })
        .await;
    assert!(!fixture.path("work/finish").exists());
    tokio::time::timeout(Duration::from_secs(3), first.close())
        .await
        .expect("interactive Ctrl+D must detach before the unfinished Agent completes");
    let detached = snapshot(&client, execution).await;
    assert_eq!(
        detached.conversation.chat_loop.state,
        tickloop_harness_protocol::LoopState::Running
    );
    assert_eq!(detached.conversation.chat_loop.id, loop_id);
    assert_eq!(detached.conversation.chat_loop.run_id, run_id);
    let rows = catalog(&client).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].views.is_empty());
    let mut reopened = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    reopened
        .wait_for("reattach new View ready", new_input_is_visible)
        .await;
    reopened.send(b"\x1b[D");
    reopened
        .wait_for("reattach overview", |frame| {
            frame.contains("Agents · local")
        })
        .await;
    select_overview_row(&mut reopened, &rows[0].title).await;
    assert!(
        reopened.frame().lines().any(|line| {
            line.starts_with(SELECTED_MARK) && row_title(line).starts_with(&rows[0].title)
        }),
        "the running Agent row is selected: {}",
        reopened.frame()
    );
    reopened.send(b"\r");
    reopened
        .wait_for("reattach shows the same running work", |frame| {
            frame.contains("work started") && !frame.contains("Agents · local")
        })
        .await;
    assert_eq!(catalog(&client).await[0].execution, execution);
    assert_eq!(catalog(&client).await[0].views.len(), 1);
    assert_eq!(
        snapshot(&client, execution)
            .await
            .conversation
            .chat_loop
            .run_id,
        run_id
    );
    std::fs::write(fixture.path("work/finish"), "finish").unwrap();
    let completed = wait_snapshot(
        &client,
        execution,
        "original run completes after reattach",
        |snapshot| {
            snapshot.conversation.chat_loop.state == tickloop_harness_protocol::LoopState::Idle
        },
    )
    .await;
    assert_eq!(completed.conversation.chat_loop.id, loop_id);
    reopened
        .wait_for("original result reaches the replacement View", |frame| {
            frame.contains("original execution completed after reattach")
        })
        .await;
    for _ in 0..2 {
        let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["role"] == "user"
                        && message
                            .to_string()
                            .contains("Run the bounded work and report completion.")
                })
        );
    }
    assert!(requests.try_recv().is_err());
    reopened.close().await;
}

#[tokio::test]
async fn actual_cli_empty_control_d_detaches_working_agent_and_reattaches_the_same_run() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_working_agent_detach(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_resize_matrix(fixture: &PrivateFixture) {
    let mut view = PtyView::start(fixture);
    view.wait_for("resize View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    for (height, editor_limit, bottom_limit) in [(24, 6, 12), (32, 8, 16), (48, 10, 24)] {
        for width in [40, 80, 120] {
            view.resize(width, height).await;
            view.send(b"matrix-text-jk");
            for index in 0..12 {
                view.send(format!("\x0amatrix-line-{index:02}").as_bytes());
            }
            let frame = view
                .wait_for("OS-sized long ordinary draft", |frame| {
                    frame.contains("matrix-line-11")
                })
                .await;
            let rows = frame.lines().collect::<Vec<_>>();
            let borders = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.starts_with("──"))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            assert_eq!(borders.len(), 2, "{height}×{width}: {frame}");
            assert!(
                borders[1] - borders[0] - 1 <= editor_limit,
                "{height}×{width}: {frame}"
            );
            assert!(
                height - borders[0] <= bottom_limit,
                "{height}×{width}: {frame}"
            );
            assert!(borders[0] >= 6, "{height}×{width}: {frame}");
            assert!(
                rows[rows.len() - 2].contains("DeepSeek Flash"),
                "{height}×{width}: the core status sits right above the key hints: {frame}"
            );
            assert!(
                rows.last().unwrap().contains("enter send"),
                "{height}×{width}: {frame}"
            );
            view.send(b"\x03");
            view.wait_for("ordinary draft cleared", |frame| {
                !frame.contains("matrix-line-")
            })
            .await;
            view.send(b"?");
            let help = view
                .wait_for("OS-sized help remains reachable", |frame| {
                    frame.contains("ctrl+j newline") && frame.contains("ctrl+g editor")
                })
                .await;
            assert!(
                !help.lines().last().unwrap().contains("Context"),
                "{height}×{width}: {help}"
            );
            view.send(b"\x1b");
            view.wait_for("help restores actual status and key hints", |frame| {
                let rows = frame.lines().collect::<Vec<_>>();
                !frame.contains("ctrl+j newline")
                    && rows[rows.len() - 2].contains("DeepSeek Flash")
                    && rows[rows.len() - 1].contains("? help")
            })
            .await;
            view.send(b"\x1b[D");
            view.wait_for("OS-sized local overview", |frame| {
                frame.contains("Agents · local")
            })
            .await;
            view.send(b"\x1b[A\tdispatchjk");
            let dispatch = view
                .wait_for("OS-sized dispatch draft", |frame| {
                    frame.contains("dispatchjk")
                })
                .await;
            assert!(
                dispatch.contains("New Agent · cwd"),
                "{height}×{width}: {dispatch}"
            );
            assert!(
                dispatch.contains("Agents · local"),
                "{height}×{width}: {dispatch}"
            );
            view.send(b"\x03\x1b");
            view.wait_for("dispatch Esc restores ordinary input", |frame| {
                !frame.contains("Agents · local") && !frame.contains("dispatchjk")
            })
            .await;
        }
    }
    view.send(b"/mo");
    view.wait_for("actual completion candidates", |frame| {
        frame.contains("/model")
    })
    .await;
    view.send(b"\x0e\x10jk");
    view.wait_for("candidate navigation keeps jk as ordinary text", |frame| {
        frame.contains("/mojk")
    })
    .await;
    assert!(
        catalog(&client).await.is_empty(),
        "size, help, and drafts must create no execution or model work"
    );
    view.close().await;
}

#[tokio::test]
async fn actual_cli_os_resize_matrix_preserves_ordinary_and_dispatch_input_reachability() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_resize_matrix(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_sustained_daemon_loss(
    fixture: &PrivateFixture,
    daemon_stopped: &std::sync::atomic::AtomicBool,
) {
    let mut view = PtyView::start(fixture);
    view.wait_for("daemon-loss View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    view.send(b"!printf 'cached shell output\\n'\r");
    view.wait_for(
        "finished Shell releases the input and retains its output",
        |frame| {
            frame
                .lines()
                .any(|line| screen::response_line(line) == "cached shell output")
                && frame.lines().any(|line| line.trim() == "❯")
                && !frame.contains("Shell running · ctrl+b background · ctrl+c stop")
        },
    )
    .await;
    let rows = catalog(&client).await;
    assert_eq!(rows.len(), 1);
    let before = snapshot(&client, rows[0].execution).await;
    assert_eq!(before.user_shells.len(), 1);
    assert_eq!(before.user_shells[0].state, UserShellState::Completed);
    assert!(before.last_model_run.is_none());
    view.send(b"!printf 'background shell began\\n'; while [ ! -f daemon-loss-finish ]; do sleep 0.02; done\r");
    wait_snapshot(
        &client,
        rows[0].execution,
        "foreground Shell remains running before backgrounding",
        |snapshot| {
            snapshot.user_shells.len() == 2
                && snapshot.user_shells[1].state == UserShellState::Running
        },
    )
    .await;
    view.wait_for("running Shell output is ready to background", |frame| {
        frame
            .lines()
            .any(|line| screen::response_line(line) == "background shell began")
    })
    .await;
    view.send(b"\x02");
    let busy = wait_snapshot(
        &client,
        rows[0].execution,
        "background Shell is busy before daemon loss",
        |snapshot| {
            snapshot.user_shells.len() == 2
                && snapshot.user_shells[1].state == UserShellState::Background
        },
    )
    .await;
    assert!(busy.user_shells[1].starts_in_background);
    assert!(busy.last_model_run.is_none());
    view.wait_for("actual busy Shell task is cached in the View", |frame| {
        frame.contains("Monitor 1")
    })
    .await;
    view.send(b"UNSENT-DAEMON-LOSS-DRAFT-jk");
    view.wait_for("unsent draft before daemon loss", |frame| {
        frame.contains("UNSENT-DAEMON-LOSS-DRAFT-jk")
    })
    .await;
    let ObservationValue::RuntimeVersion(version) = client
        .observe(ObservationRequest::RuntimeVersion)
        .await
        .unwrap()
        .initial
    else {
        panic!("the fixture daemon must report its owned Workers");
    };
    assert!(!version.workers.is_empty());
    let owned_processes = std::iter::once(client.server_process_id())
        .chain(version.workers.iter().map(|worker| worker.process_id))
        .collect::<Vec<_>>();
    std::fs::rename(fixture.path("runtime"), fixture.path("owned-runtime")).unwrap();
    std::fs::write(fixture.path("runtime"), "fixture blocks daemon restart").unwrap();
    let shutdown = tokio::time::timeout(UI_TIMEOUT, client.shutdown_daemon())
        .await
        .expect("the owned daemon must acknowledge stop or close its connection");
    tokio::time::timeout(UI_TIMEOUT, async {
        while owned_processes.iter().any(|pid| !support::has_exited(*pid)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("owned daemon/Workers did not exit after runtime removal: {owned_processes:?}; shutdown result: {shutdown:?}"));
    daemon_stopped.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Err(error) = shutdown {
        assert!(
            error.code == tickloop_code_access::AccessErrorCode::Unavailable
                && error.message == "daemon connection closed",
            "unexpected stop error after the owned processes exited: {error}"
        );
    }
    view.wait_for(
        "sustained loss retains a disconnected cached View",
        |frame| frame.contains("Disconnected") && frame.contains("UNSENT-DAEMON-LOSS-DRAFT-jk"),
    )
    .await;
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            view.child.try_wait().unwrap().is_none(),
            "daemon loss must not exit the interactive View"
        );
        let frame = view.frame();
        assert!(frame.contains("Disconnected"), "{frame}");
        assert!(frame.contains("UNSENT-DAEMON-LOSS-DRAFT-jk"), "{frame}");
        assert!(
            frame
                .lines()
                .filter_map(|line| line.split('│').next())
                .any(|conversation| screen::response_line(conversation) == "cached shell output"),
            "the cached output stays in the conversation beside Monitor: {frame}"
        );
        assert!(
            fixture.path("runtime").is_file(),
            "the unavailable daemon must remain blocked"
        );
    }
    view.send(b"x");
    view.wait_for("cached View still accepts ordinary editing", |frame| {
        frame.contains("UNSENT-DAEMON-LOSS-DRAFT-jkx")
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_sustained_daemon_loss_keeps_cached_output_draft_and_editing_alive() {
    let fixture = PrivateFixture::new();
    let daemon_stopped = std::sync::atomic::AtomicBool::new(false);
    let result = AssertUnwindSafe(run_sustained_daemon_loss(&fixture, &daemon_stopped))
        .catch_unwind()
        .await;
    let cleanup = AssertUnwindSafe(async {
        if fixture.path("owned-runtime").exists() {
            std::fs::remove_file(fixture.path("runtime")).unwrap();
            std::fs::rename(fixture.path("owned-runtime"), fixture.path("runtime")).unwrap();
            if daemon_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                for entry in std::fs::read_dir(fixture.path("runtime"))
                    .unwrap()
                    .filter_map(Result::ok)
                {
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "sock")
                    {
                        std::fs::remove_file(entry.path()).unwrap();
                    }
                }
            }
        }
        fixture.shutdown().await;
    })
    .catch_unwind()
    .await;
    if let Err(panic) = result.and(cleanup) {
        std::panic::resume_unwind(panic);
    }
}

/// The screen the terminal shows now for everything the View wrote, including what an attached
/// view drew without the Console's synchronized frames.
fn terminal_screen(view: &PtyView) -> String {
    let mut parser = vt100::Parser::new(view.height as u16, view.width as u16, 0);
    parser.process(&view.output.lock().unwrap());
    parser.screen().contents()
}

/// Waits until an attached view holds the terminal, the Console's Monitor hints gone from it,
/// and shows every one of `texts`.
async fn view_shows(view: &PtyView, texts: &[&str]) {
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let screen = terminal_screen(view);
            if !screen.contains("enter open") && texts.iter().all(|text| screen.contains(text)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{texts:?} never showed in a view:\n{}",
            terminal_screen(view)
        )
    });
}

async fn terminal_shows(view: &PtyView, text: &str) {
    tokio::time::timeout(UI_TIMEOUT, async {
        while !terminal_screen(view).contains(text) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{text:?} never appeared:\n{}", terminal_screen(view)));
}

fn chat_reply(tool: Option<(&str, &str, Value)>, text: &str) -> String {
    let delta = match &tool {
        Some((name, id, input)) => json!({
            "role":"assistant",
            "content":text,
            "tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}],
        }),
        None => json!({"role":"assistant","content":text}),
    };
    let frame = |delta: Value, reason: Value| {
        format!(
            "data: {}\n\n",
            json!({"id":"cli-fixture","choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
        )
    };
    [
        frame(delta, Value::Null),
        frame(
            json!({}),
            json!(if tool.is_some() { "tool_calls" } else { "stop" }),
        ),
        "data: [DONE]\n\n".into(),
    ]
    .concat()
}

async fn wait_snapshot(
    client: &LocalAgentClient,
    execution: ExecutionRef,
    label: &str,
    matches: impl Fn(&LiveChatSnapshot) -> bool,
) -> Box<LiveChatSnapshot> {
    let mut latest = String::new();
    let mut queries = 0;
    let mut query_time = Duration::ZERO;
    let mut longest_query = Duration::ZERO;
    let mut query_started = None;
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let started = Instant::now();
            query_started = Some(started);
            let snapshot = snapshot(client, execution).await;
            let elapsed = started.elapsed();
            query_started = None;
            queries += 1;
            query_time += elapsed;
            longest_query = longest_query.max(elapsed);
            if matches(&snapshot) {
                break snapshot;
            }
            latest = format!(
                "queries={queries} query_time_ms={} longest_query_ms={} {}",
                query_time.as_millis(),
                longest_query.as_millis(),
                snapshot_diagnostic(&snapshot),
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "private Agent snapshot did not reach {label}: in_flight_query_ms={:?}; {latest}",
            query_started.map(|started| started.elapsed().as_millis())
        )
    })
}

fn snapshot_diagnostic(snapshot: &LiveChatSnapshot) -> String {
    format!(
        "sequence={} loop={:?} permissions={:?} questions={} pending_thinking_chars={:?} operations={:?} commands={:?}",
        snapshot.last_sequence,
        (
            snapshot.conversation.chat_loop.run_id,
            &snapshot.conversation.chat_loop.state
        ),
        snapshot
            .permissions
            .iter()
            .map(|request| request.name.as_str())
            .collect::<Vec<_>>(),
        snapshot.questions.len(),
        snapshot
            .pending_thinking
            .iter()
            .map(|(run, text)| (run, text.chars().count()))
            .collect::<Vec<_>>(),
        snapshot
            .operations
            .iter()
            .map(|operation| (
                operation.call_id.as_str(),
                operation.name.as_str(),
                &operation.state
            ))
            .collect::<Vec<_>>(),
        snapshot
            .commands
            .iter()
            .map(|command| (command.operation_id, &command.state, command.output_bytes))
            .collect::<Vec<_>>(),
    )
}

fn request_dialog_visible(frame: &str, question: &str, requester: &str) -> bool {
    frame.lines().any(|line| line.trim() == question)
        && frame.lines().any(|line| line.trim() == requester)
        && frame.contains("enter select")
        && frame.contains("esc cancel")
}

async fn open_family_child(pty: &mut PtyView, task: &str) {
    pty.send(b"\x1b[1;5F\x1b[B");
    let list = pty
        .wait_for("family list receives focus", |frame| {
            frame.contains("Subagents 1")
                && frame.contains(task)
                && frame.contains("enter open · space fold")
                && frame.lines().any(|line| line.starts_with(SELECTED_MARK))
        })
        .await;
    if !list
        .lines()
        .any(|line| line.starts_with(SELECTED_MARK) && line.contains("cadence"))
    {
        pty.send(b"j");
        pty.wait_for("child row is selected", |frame| {
            frame
                .lines()
                .any(|line| line.starts_with(SELECTED_MARK) && line.contains("cadence"))
        })
        .await;
    }
    pty.send(b"\r");
    pty.wait_for(
        "child is displayed before releasing its question",
        |frame| {
            frame.contains(&format!("Delegated Agent · {task}"))
                && !frame.contains("Agent family · root")
                && !frame.contains("Agents · local")
        },
    )
    .await;
}

async fn run_child_views(fixture: &PrivateFixture) {
    const CHILD_TASK: &str = "Ask bounded cadence.";
    const RAW_CHILD_CONTENT: &str =
        "123e4567-e89b-12d3-a456-426614174000 deadbeef /tmp/a --id deadbeef";
    const FOLLOW_UP: &str = "Continue the same bounded cadence task.";
    const FIRST_CHILD_RESPONSE_INDEX: usize = 2;
    const PARENT_RESULT_RESPONSE_INDEX: usize = 6;
    let replies = vec![
        chat_reply(
            Some((
                "Subagent",
                "delegate-cli",
                json!({"task":CHILD_TASK,"name":"cadence","description":CHILD_TASK}),
            )),
            "",
        ),
        chat_reply(
            None,
            &tickloop_harness_test_support::auto_classification(false).to_string(),
        ),
        chat_reply(
            Some((
                "AskUserQuestion",
                "question-first",
                tickloop_harness_test_support::single_question("Which bounded cadence?"),
            )),
            RAW_CHILD_CONTENT,
        ),
        chat_reply(
            Some((
                "AskUserQuestion",
                "question-second",
                tickloop_harness_test_support::single_question("Which second window?"),
            )),
            "",
        ),
        chat_reply(
            Some((
                "SubagentHandback",
                "child-report",
                json!({"message":"child weekly result"}),
            )),
            "child weekly result",
        ),
        chat_reply(
            None,
            &tickloop_harness_test_support::auto_classification(false).to_string(),
        ),
        chat_reply(None, "parent delegated result received"),
        chat_reply(
            Some((
                "SubagentHandback",
                "follow-up-report",
                json!({"message":"child follow-up acknowledged"}),
            )),
            "child follow-up acknowledged",
        ),
        chat_reply(
            None,
            &tickloop_harness_test_support::auto_classification(false).to_string(),
        ),
        chat_reply(None, "parent processed the follow-up report"),
    ];
    let (base_url, mut requests, mut releases) =
        model_http_support::serve_responses_with_gated_responses(
            replies.into_iter().map(|reply| (200, reply)).collect(),
            &[FIRST_CHILD_RESPONSE_INDEX, PARENT_RESULT_RESPONSE_INDEX],
        )
        .await;
    let finish_parent = releases.pop().unwrap();
    let show_child_question = releases.pop().unwrap();
    let mut model = tickloop_harness_test_support::config();
    model.protocol = tickloop_harness_protocol::Protocol::ChatCompletions;
    model.base_url = base_url.trim_end_matches("/anthropic").into();
    model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
    fixture.set_model(&model);
    let mut first = PtyView::start(fixture);
    first
        .wait_for("delegation View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    let other_project = fixture.path("other-project");
    std::fs::create_dir(&other_project).unwrap();
    let other_project = other_project.canonicalize().unwrap();
    let parent_project = fixture.path("delegation-project");
    std::fs::create_dir(&parent_project).unwrap();
    let parent_project = parent_project.canonicalize().unwrap();
    let launch_project = fixture.path("work").canonicalize().unwrap();
    let mut other_launch = LaunchSpec::embedded(other_project.clone());
    other_launch.config_root = Some(fixture.path("config"));
    other_launch.model = Some(model);
    other_launch.session_persistence = false;
    let CommandResult::Created {
        execution: other, ..
    } = client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: other_launch,
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap()
    else {
        panic!("the second project must create an independent root");
    };
    first.send(b"\x1b[D");
    first
        .wait_for("delegation overview", |frame| {
            frame.contains("Agents · local") && frame.contains("▸ Empty · 1")
        })
        .await;
    first.send(b"p");
    let filtered = first
        .wait_for("project filter keeps the launch directory", |frame| {
            frame.contains("Project:") && !frame.contains("Project: all")
        })
        .await;
    assert!(
        filtered.lines().any(|line| line.contains("New Agent · cwd")
            && line.contains(launch_project.to_str().unwrap())),
        "{filtered}"
    );
    first.send(format!("\t/cwd {}\r", parent_project.display()).as_bytes());
    first
        .wait_for(
            "explicit dispatch directory while project filter stays active",
            |frame| {
                frame.lines().any(|line| {
                    line.contains("New Agent · cwd")
                        && line.contains(parent_project.to_str().unwrap())
                }) && frame.contains("Project:")
                    && !frame.contains("Project: all")
            },
        )
        .await;
    assert_eq!(catalog(&client).await.len(), 1);
    assert!(requests.try_recv().is_err());
    first.send(b"Delegate bounded cadence.\r");
    let (parent, child) = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(&client).await;
            if let Some(child) = rows.iter().find(|row| row.parent.is_some()) {
                assert_eq!(rows.len(), 3);
                break (child.parent.unwrap(), child.execution);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("actual model-visible Subagent must create a real child");
    let child_before = snapshot(&client, child).await;
    assert_eq!(
        child_before.conversation.folder,
        Some(parent_project.clone())
    );
    assert_eq!(
        snapshot(&client, other).await.conversation.folder,
        Some(other_project.clone())
    );
    let child_loop = child_before.conversation.chat_loop.id;
    let metadata =
        serde_json::to_value(child_before.conversation.delegation.as_ref().unwrap()).unwrap();
    first
        .wait_for("dispatch opens the new Agent", |frame| {
            !frame.contains("Agents · local")
        })
        .await;
    first.send(b"\x1b[D");
    first
        .wait_for("back in the filtered overview", |frame| {
            frame.contains("Agents · local")
                && frame.contains("Project:")
                && !frame.contains("Project: all")
        })
        .await;
    first.send(b"\tp");
    let folded = first
        .wait_for("real child folded under its root", |frame| {
            frame.contains("Agents · local") && frame.contains("Project: all")
        })
        .await;
    assert!(!folded.contains(CHILD_TASK), "{folded}");
    assert!(
        folded.contains("▸ Empty · 1"),
        "the never-used root stays discoverable behind its heading: {folded}"
    );
    select_overview_row(&mut first, "Delegate bounded cadence.").await;
    let overview = first.frame();
    assert!(
        !overview.contains(CHILD_TASK),
        "children are selected inside the family: {overview}"
    );
    first.send(b"\r");
    first
        .wait_for("overview opens the family root", |frame| {
            frame.contains("Subagents 1") && !frame.contains("Agents · local")
        })
        .await;
    let child_label = "Agent cadence".to_owned();
    open_family_child(&mut first, CHILD_TASK).await;
    wait_for_display_views(&client, child, 1).await;
    let mut second = PtyView::start(fixture);
    second
        .wait_for("independent View ready", new_input_is_visible)
        .await;
    second.send(b"\x1b[D");
    second
        .wait_for("second overview", |frame| {
            frame.contains("Agents · local") && frame.contains("Delegate bounded cadence.")
        })
        .await;
    select_overview_row(&mut second, "Delegate bounded cadence.").await;
    assert!(
        second
            .frame()
            .lines()
            .any(|line| line.starts_with(SELECTED_MARK)
                && line.contains("Delegate bounded cadence.")),
        "the parent row is selected in the second View: {}",
        second.frame()
    );
    second.send(b"\r");
    second
        .wait_for("second View opens the family root", |frame| {
            frame.contains("Subagents 1") && !frame.contains("Agents · local")
        })
        .await;
    open_family_child(&mut second, CHILD_TASK).await;
    wait_for_display_views(&client, child, 2).await;
    show_child_question.send(()).unwrap();
    let child_with_question = wait_snapshot(
        &client,
        child,
        "the gated question is pending in the selected child",
        |snapshot| !snapshot.questions.is_empty(),
    )
    .await;
    let question = child_with_question
        .request_refs
        .iter()
        .find(|request| request.kind == PendingRequestKind::Question)
        .copied()
        .unwrap();
    assert_eq!(question.execution, child);
    assert!(snapshot(&client, parent).await.questions.is_empty());
    let child_frame = first
        .wait_for("overview enters child requester", |frame| {
            request_dialog_visible(frame, "Which bounded cadence?", &child_label)
                && frame.contains(RAW_CHILD_CONTENT)
                && frame.contains(&format!("Delegated Agent · {CHILD_TASK}"))
        })
        .await;
    assert!(child_frame.contains(RAW_CHILD_CONTENT), "{child_frame}");
    assert!(
        child_frame.contains(&format!("Delegated Agent · {CHILD_TASK}")),
        "{child_frame}"
    );
    assert!(
        !child_frame.contains("Agent family · root"),
        "{child_frame}"
    );
    assert!(
        !child_frame.contains(
            &snapshot(&client, parent)
                .await
                .conversation
                .id
                .uuid()
                .to_string()
        ),
        "{child_frame}"
    );
    assert!(
        !child_frame.contains(&child_before.conversation.id.uuid().to_string()),
        "{child_frame}"
    );
    assert_eq!(
        snapshot(&client, parent).await.conversation.chat_loop.state,
        tickloop_harness_protocol::LoopState::Running
    );
    second
        .wait_for("same real request in second View", |frame| {
            request_dialog_visible(frame, "Which bounded cadence?", &child_label)
        })
        .await;
    let wrong = client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::AnswerQuestion {
                request: RequestRef {
                    execution: parent,
                    ..question
                },
                answers: vec![tickloop_harness_protocol::QuestionAnswer::option("Weekly")],
            },
        })
        .await
        .unwrap();
    assert!(wrong.error.is_some());
    first.send(b"\r");
    second.send(b"\r");
    let second_question = wait_snapshot(
        &client,
        child,
        "second question remains pending after two old Enter keys",
        |snapshot| {
            snapshot
                .questions
                .iter()
                .any(|request| request.id != question.request_id)
        },
    )
    .await;
    assert_eq!(second_question.questions.len(), 1);
    assert_ne!(second_question.questions[0].id, question.request_id);
    first
        .wait_for("next bound child request", |frame| {
            request_dialog_visible(frame, "Which second window?", &child_label)
        })
        .await;
    first.send(b"\r");
    wait_snapshot(
        &client,
        child,
        "second child question answered",
        |snapshot| snapshot.questions.is_empty(),
    )
    .await;
    let mut seen = Vec::new();
    for _ in 0..=PARENT_RESULT_RESPONSE_INDEX {
        seen.push(
            tokio::time::timeout(UI_TIMEOUT, requests.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let parent_running = wait_snapshot(
        &client,
        parent,
        "parent waits on its model response after receiving the child result",
        |snapshot| {
            snapshot.conversation.chat_loop.state == tickloop_harness_protocol::LoopState::Running
                && snapshot.events.iter().any(|event| matches!(&event.kind,
                    tickloop_harness_protocol::EventKind::ToolCompleted { call_id, .. } if call_id == "delegate-cli"))
        },
    )
    .await;
    let parent_loop = parent_running.conversation.chat_loop.id;
    let parent_run = parent_running.conversation.chat_loop.run_id;
    first
        .wait_for("child request resolves", |frame| {
            frame.contains("child weekly result")
                && !request_dialog_visible(frame, "Which second window?", &child_label)
        })
        .await;
    second
        .wait_for("other View drops its solved request", |frame| {
            frame.contains("child weekly result")
                && !request_dialog_visible(frame, "Which bounded cadence?", &child_label)
                && !request_dialog_visible(frame, "Which second window?", &child_label)
        })
        .await;
    first.send(b"CHILD-DRAFT-ONE");
    first
        .wait_for("first child draft", |frame| {
            frame.contains("CHILD-DRAFT-ONE")
        })
        .await;
    second.send(b"CHILD-DRAFT-TWO");
    let independent = second
        .wait_for("second child draft", |frame| {
            frame.contains("CHILD-DRAFT-TWO")
        })
        .await;
    assert!(!independent.contains("CHILD-DRAFT-ONE"));
    first.send(b"\x1b[B");
    first
        .wait_for("real family list focuses the remembered child", |frame| {
            frame.contains("Subagents 1")
                && family_panel_has_viewing_row(frame)
                && frame.contains("enter open · space fold")
                && frame
                    .lines()
                    .any(|line| line.starts_with(SELECTED_MARK) && line.contains("cadence"))
        })
        .await;
    first.send(b"k\r");
    first
        .wait_for("lower family enters real parent", |frame| {
            frame.contains("Delegate bounded cadence.") && !frame.contains("CHILD-DRAFT-ONE")
        })
        .await;
    first.send(b"PARENT-DRAFT-ONE");
    first
        .wait_for("parent local draft", |frame| {
            frame.contains("PARENT-DRAFT-ONE")
        })
        .await;
    first.send(b"\x1b[B");
    first
        .wait_for(
            "parent family list focuses without consuming its draft",
            |frame| {
                family_panel_has_viewing_row(frame)
                    && frame.contains("PARENT-DRAFT-ONE")
                    && frame.contains("enter open · space fold")
            },
        )
        .await;
    first.send(b"j\r");
    first
        .wait_for(
            "lower family returns to real child with its cached draft",
            |frame| frame.contains("CHILD-DRAFT-ONE") && !frame.contains("PARENT-DRAFT-ONE"),
        )
        .await;
    assert!(
        catalog(&client)
            .await
            .iter()
            .find(|row| row.execution == child)
            .unwrap()
            .views
            .len()
            == 2
    );
    let captured_output = Arc::clone(&first.output);
    first
        .wait_for(
            "opening the cached child retains its family and focuses its editor",
            |frame| {
                let captured = captured_output.lock().unwrap();
                let terminal = String::from_utf8_lossy(&captured);
                frame.contains("CHILD-DRAFT-ONE")
                    && frame.contains("Subagents 1")
                    && family_panel_has_viewing_row(frame)
                    && terminal.rfind("\x1b[?25h") > terminal.rfind("\x1b[?25l")
            },
        )
        .await;
    let clear_mark = captured_output.lock().unwrap().len();
    first.send(b"\x15");
    first
        .wait_for(
            "fresh child draft is empty before same-Loop follow-up",
            |frame| {
                let captured = captured_output.lock().unwrap();
                let fresh = String::from_utf8_lossy(&captured[clear_mark..]);
                fresh.contains("\x1b[?2026l")
                    && !frame.contains("CHILD-DRAFT-ONE")
                    && frame.lines().any(|line| line.contains("Message @cadence…"))
            },
        )
        .await;
    first.send(format!("{FOLLOW_UP}\r").as_bytes());
    let followed = wait_snapshot(&client, child, "original child Loop follow-up", |snapshot| snapshot.events.iter().any(|event| matches!(&event.kind, tickloop_harness_protocol::EventKind::Text(text) if text.contains("child follow-up acknowledged")))).await;
    assert_eq!(followed.conversation.chat_loop.id, child_loop);
    assert_eq!(
        serde_json::to_value(followed.conversation.delegation.as_ref().unwrap()).unwrap(),
        metadata
    );
    let parent_after_follow_up = wait_snapshot(
        &client,
        parent,
        "one audit for the completed child continuation without a parent model turn",
        |snapshot| {
            snapshot.conversation.family_completions.len()
                == parent_running.conversation.family_completions.len() + 1
        },
    )
    .await;
    assert!(parent_after_follow_up.last_sequence > parent_running.last_sequence);
    assert_eq!(
        parent_after_follow_up.conversation.messages.len(),
        parent_running.conversation.messages.len()
    );
    assert_eq!(
        parent_after_follow_up.conversation.chat_loop.state,
        tickloop_harness_protocol::LoopState::Running
    );
    assert_eq!(
        parent_after_follow_up.conversation.chat_loop.id,
        parent_loop
    );
    assert_eq!(
        parent_after_follow_up.conversation.chat_loop.run_id,
        parent_run
    );
    assert_eq!(
        parent_after_follow_up.conversation.folder,
        Some(parent_project.clone())
    );
    finish_parent.send(()).unwrap();
    let parent_done = wait_snapshot(
        &client,
        parent,
        "parent processes the delivered follow-up report",
        |snapshot| {
            snapshot.conversation.chat_loop.state == tickloop_harness_protocol::LoopState::Idle
                && snapshot.events.iter().any(|event| matches!(&event.kind,
                    tickloop_harness_protocol::EventKind::Text(text) if text.contains("parent processed the follow-up report")))
        },
    )
    .await;
    assert_eq!(parent_done.conversation.chat_loop.id, parent_loop);
    assert_ne!(parent_done.conversation.chat_loop.run_id, parent_run);
    assert!(parent_done.events.iter().any(|event| matches!(&event.kind, tickloop_harness_protocol::EventKind::Text(text) if text.contains("parent delegated result received"))));
    let shared_fact = second
        .wait_for("shared child fact keeps the second View draft", |frame| {
            frame.contains("child follow-up acknowledged") && frame.contains("CHILD-DRAFT-TWO")
        })
        .await;
    assert!(!shared_fact.contains("PARENT-DRAFT-ONE"));
    first
        .wait_for("follow-up visible in issuing View", |frame| {
            frame.contains("child follow-up acknowledged")
        })
        .await;
    let configured = client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Configure {
                provider: None,
                execution: parent,
                change: ConfigurationChange::ExecutionMode {
                    mode: tickloop_harness_protocol::ExecutionMode::AcceptEdits,
                },
            },
        })
        .await
        .unwrap();
    assert!(configured.error.is_none());
    first.send(b"!python3 -c 'print(123)'\r");
    first
        .wait_for("child Shell permission is visible", |frame| {
            frame.contains("python3") && frame.contains("Permission required")
        })
        .await;
    let waiting = wait_snapshot(
        &client,
        child,
        "direct child Shell awaits normal approval",
        |snapshot| !snapshot.permissions.is_empty(),
    )
    .await;
    assert!(matches!(
        waiting.permissions[0].execution,
        tickloop_harness_protocol::PermissionExecution::UserShell { .. }
    ));
    assert!(waiting.user_shells.iter().all(|shell| shell.pid.is_none()));
    first.send(b"\r");
    wait_snapshot(
        &client,
        child,
        "approved direct child Shell completed",
        |snapshot| {
            snapshot
                .user_shells
                .iter()
                .any(|shell| shell.state == tickloop_harness_protocol::UserShellState::Completed)
        },
    )
    .await;
    first
        .wait_for("approved child Shell output is visible", |frame| {
            frame.contains("123")
        })
        .await;
    let forbidden = client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Configure {
                provider: None,
                execution: child,
                change: ConfigurationChange::ReplaceRules { rules: Vec::new() },
            },
        })
        .await
        .unwrap();
    assert!(forbidden.error.unwrap().message.contains("Unsupported"));
    for _ in 0..3 {
        seen.push(
            tokio::time::timeout(UI_TIMEOUT, requests.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(requests.try_recv().is_err());
    assert!(
        seen.iter()
            .all(|(headers, _)| headers.starts_with("POST /chat/completions HTTP/1.1"))
    );
    assert!(
        seen[0].1["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "Subagent")
    );
    assert!(
        seen[2].1["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "Subagent")
    );
    let follow_up_input = seen[7].1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| is_user_prompt(message))
        .unwrap();
    assert_eq!(
        follow_up_input["content"].as_str(),
        Some(FOLLOW_UP),
        "the real child model request must receive the cleared editor's exact follow-up"
    );
    first.close().await;
    second.close().await;
}

#[tokio::test]
async fn actual_cli_real_child_family_requests_drafts_and_follow_up_keep_the_ceiling() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_child_views(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_reading_and_draft_return(fixture: &PrivateFixture) {
    const DRAFT: &str = "KEEP-READING-DRAFT-jk";
    const CURSOR_DISTANCE_FROM_END: usize = 4;
    const HISTORY_WHEEL_EVENTS: usize = 8;
    const SCROLL_UP_WHEEL: &[u8] = b"\x1b[<64;1;1M";
    let history = (0..80)
        .map(|index| format!("stable history {index:02}\n"))
        .collect::<String>();
    let (base_url, mut requests) = model_http_support::serve_responses(vec![
        (200, chat_reply(Some(("Bash", "reading-gate", json!({
            "command":"printf 'A gate active\\n'; while [ ! -f reading-finish ]; do sleep 0.02; done; printf 'A background output\\n'",
        }))), &history)),
        (200, chat_reply(None, "A final output after switching away")),
    ]).await;
    let mut model = tickloop_harness_test_support::config();
    model.protocol = tickloop_harness_protocol::Protocol::ChatCompletions;
    model.base_url = base_url.trim_end_matches("/anthropic").into();
    model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
    fixture.set_model(&model);
    let mut view = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    view.wait_for("reading View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    view.send(b"Produce bounded history and wait for the file gate.\r");
    view.wait_for("bounded history is visible before switching", |frame| {
        frame.contains("A gate active")
    })
    .await;
    let first = catalog(&client).await[0].execution;
    let first_running = snapshot(&client, first).await;
    assert_eq!(
        first_running.conversation.chat_loop.state,
        tickloop_harness_protocol::LoopState::Running
    );
    assert!(first_running.events.iter().any(|event| matches!(
        &event.kind, tickloop_harness_protocol::EventKind::Text(text) if text.contains("stable history 00")
    )));
    let other_project = fixture.path("reading-project");
    std::fs::create_dir(&other_project).unwrap();
    let other_project = other_project.canonicalize().unwrap();
    let mut launch = LaunchSpec::embedded(other_project.clone());
    launch.config_root = Some(fixture.path("config"));
    launch.model = Some(model);
    launch.session_persistence = false;
    let CommandResult::Created {
        execution: other, ..
    } = client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial: Some(UserInput::Shell {
                    command: "printf 'B independent output\\n'".into(),
                    background: false,
                }),
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap()
    else {
        panic!("the independent reading target must be created");
    };
    wait_snapshot(
        &client,
        other,
        "independent target Shell completes",
        |snapshot| {
            snapshot
                .user_shells
                .iter()
                .any(|shell| shell.state == UserShellState::Completed)
        },
    )
    .await;
    view.send(DRAFT.as_bytes());
    view.send(b"\x1b[D\x1b[D\x1b[D\x1b[D");
    // Wheel scrolling keeps the editor focused; reading Esc returns to bottom.
    for _ in 0..HISTORY_WHEEL_EVENTS {
        view.send(SCROLL_UP_WHEEL);
    }
    let reading = view
        .wait_for("manual history with unsent draft", |frame| {
            frame.contains(DRAFT)
                && frame.contains("stable history")
                && !frame.contains("A gate active")
        })
        .await;
    // The reading anchor is the topmost visible history line. The wheel events can still be
    // applying when the draft first shows, so the anchor is taken once the topmost line stays
    // unchanged; later waits compare the topmost line itself.
    let topmost_history = |frame: &str| {
        frame
            .lines()
            .find(|line| line.contains("stable history"))
            .map(|line| line.trim().to_owned())
    };
    let first_visible_history = {
        let mut anchor = topmost_history(&reading).unwrap();
        let mut unchanged_since = std::time::Instant::now();
        let deadline = std::time::Instant::now() + UI_TIMEOUT;
        while unchanged_since.elapsed() < WHEEL_SETTLE_TIME {
            assert!(
                std::time::Instant::now() < deadline,
                "the reading anchor never settled: {}",
                view.frame()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(current) = topmost_history(&view.frame())
                && current != anchor
            {
                anchor = current;
                unchanged_since = std::time::Instant::now();
            }
        }
        anchor
    };
    let captured_output = Arc::clone(&view.output);
    view.wait_for(
        "wheel reading keeps editor focus without changing the anchor",
        |frame| {
            let captured = captured_output.lock().unwrap();
            let terminal = String::from_utf8_lossy(&captured);
            frame.contains(&first_visible_history)
                && terminal.rfind("\x1b[?25h") > terminal.rfind("\x1b[?25l")
        },
    )
    .await;
    view.send(b"\x13");
    view.wait_for("the unsent draft is stashed before navigation", |frame| {
        !frame.contains(DRAFT) && frame.contains(&first_visible_history)
    })
    .await;
    view.send(b"\x1b[D");
    view.wait_for("stash permits the empty-input overview", |frame| {
        frame.contains("Agents · local") && !frame.contains(DRAFT)
    })
    .await;
    select_overview_row(&mut view, "reading-project").await;
    view.send(b"\r");
    view.wait_for("independent Agent is visible", |frame| {
        frame.contains("B independent output")
            && !frame.contains("Agents · local")
            && !frame.contains(DRAFT)
    })
    .await;
    assert_eq!(
        snapshot(&client, other).await.conversation.folder,
        Some(other_project.clone())
    );
    std::fs::write(fixture.path("work/reading-finish"), "finish").unwrap();
    let completed = wait_snapshot(
        &client,
        first,
        "first Agent completes while another is visible",
        |snapshot| {
            snapshot.conversation.chat_loop.state == tickloop_harness_protocol::LoopState::Idle
        },
    )
    .await;
    assert_eq!(
        completed.conversation.chat_loop.id,
        first_running.conversation.chat_loop.id
    );
    assert_eq!(
        completed.conversation.chat_loop.run_id,
        first_running.conversation.chat_loop.run_id
    );
    assert!(completed.events.iter().any(|event| matches!(&event.kind,
        tickloop_harness_protocol::EventKind::Text(text) if text.contains("A final output after switching away"))));
    assert!(!view.frame().contains("A final output after switching away"));
    view.send(b"\x1b[D");
    view.wait_for("return overview has both independent roots", |frame| {
        frame.contains("Agents · local") && frame.contains("Produce bounded history")
    })
    .await;
    select_overview_row(&mut view, "Produce bounded history").await;
    view.send(b"\r");
    let returned = view
        .wait_for("return restores the original reading anchor", |frame| {
            !frame.contains("Agents · local")
                && topmost_history(frame).as_ref() == Some(&first_visible_history)
        })
        .await;
    assert!(!returned.contains("A final output after switching away"));
    view.send(b"\x13");
    view.wait_for(
        "original stashed draft returns only to its Agent",
        |frame| frame.contains(DRAFT),
    )
    .await;
    view.send(b"X");
    let cursor = DRAFT.len() - CURSOR_DISTANCE_FROM_END;
    let expected = format!("{}X{}", &DRAFT[..cursor], &DRAFT[cursor..]);
    let restored = view
        .wait_for(
            "original cursor survives the full root round trip",
            |frame| {
                frame.contains(&expected)
                    && topmost_history(frame).as_ref() == Some(&first_visible_history)
            },
        )
        .await;
    assert!(!restored.contains("A final output after switching away"));
    assert!(snapshot(&client, other).await.last_model_run.is_none());
    for _ in 0..2 {
        let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["role"] == "user"
                        && message
                            .to_string()
                            .contains("Produce bounded history and wait for the file gate.")
                })
        );
    }
    assert!(requests.try_recv().is_err());
    view.send(b"\x05");
    view.close().await;
}

#[tokio::test]
async fn actual_cli_cross_root_return_restores_draft_cursor_and_history_after_background_output() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_reading_and_draft_return(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_stashed_image_payload(fixture: &PrivateFixture, switch_roots: bool) {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

    const DRAFT_SUFFIX: &str = "AFTER-TEXT";
    const CURSOR_DISTANCE_FROM_END: usize = 4;
    let mut png = Vec::new();
    ::image::DynamicImage::ImageRgba8(::image::RgbaImage::from_pixel(
        4,
        3,
        ::image::Rgba([180, 40, 90, 255]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut png),
        ::image::ImageFormat::Png,
    )
    .unwrap();
    let image_path = fixture.path("work/chart.png");
    std::fs::write(&image_path, &png).unwrap();
    let expected_data_url = format!("data:image/png;base64,{}", BASE64.encode(&png));
    let baseline_reply = if switch_roots {
        format!(
            "{}baseline image received",
            (0..80)
                .map(|index| format!("image history {index:02}\n"))
                .collect::<String>()
        )
    } else {
        "baseline image received".into()
    };
    let (base_url, mut requests) = model_http_support::serve_responses(vec![
        (200, chat_reply(None, &baseline_reply)),
        (200, chat_reply(None, "restored image received")),
    ])
    .await;
    let model = fixture.set_chat_model(&base_url);
    let mut view = PtyView::start(fixture);
    view.wait_for("image View ready", new_input_is_visible)
        .await;

    view.send(format!("\x1b[200~BEFORE-STASH {}\x1b[201~", image_path.display()).as_bytes());
    view.wait_for("real pasted image before the control submission", |frame| {
        frame.contains("BEFORE-STASH") && frame.contains("[Image #1]")
    })
    .await;
    view.send(b"\r");
    let (_, baseline) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let user_content = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| is_user_prompt(message))
            .unwrap()["content"]
            .clone()
    };
    let image_urls = |content: &Value| {
        content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|part| part["type"] == "image_url")
            .map(|part| part["image_url"]["url"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        image_urls(&user_content(&baseline)),
        std::slice::from_ref(&expected_data_url),
        "the unstashed control must send the exact real PNG to the fixture model"
    );
    view.wait_for("control image response", |frame| {
        frame.contains("baseline image received")
    })
    .await;
    let client = fixture.client().await;
    let first = catalog(&client).await[0].execution;
    let first_before = snapshot(&client, first).await;
    let other = if switch_roots {
        let other_project = fixture.path("image-peer");
        std::fs::create_dir(&other_project).unwrap();
        let other_project = other_project.canonicalize().unwrap();
        let mut launch = LaunchSpec::embedded(other_project.clone());
        launch.config_root = Some(fixture.path("config"));
        launch.model = Some(model);
        launch.session_persistence = false;
        let CommandResult::Created { execution, .. } = client
            .execute(CommandEnvelope {
                daemon_epoch: client.daemon_epoch(),
                command_id: uuid::Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch,
                    initial: Some(UserInput::Shell {
                        command: "printf 'B independent image peer\\n'".into(),
                        background: false,
                    }),
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap()
        else {
            panic!("the independent image peer must be created");
        };
        let peer = wait_snapshot(
            &client,
            execution,
            "image peer Shell completes",
            |snapshot| {
                snapshot
                    .user_shells
                    .iter()
                    .any(|shell| shell.state == UserShellState::Completed)
            },
        )
        .await;
        assert_eq!(peer.conversation.folder, Some(other_project.clone()));
        Some(execution)
    } else {
        None
    };
    view.send(format!("\x1b[200~AFTER-STASH {}\x1b[201~", image_path.display()).as_bytes());
    view.wait_for("second real pasted image before stash", |frame| {
        frame.lines().any(|line| {
            line.contains("❯") && line.contains("AFTER-STASH") && line.contains("[Image #2]")
        })
    })
    .await;
    view.send(format!(" {DRAFT_SUFFIX}").as_bytes());
    view.wait_for("ordinary text follows the real image block", |frame| {
        frame.contains("[Image #2]") && frame.contains(DRAFT_SUFFIX)
    })
    .await;
    let history_anchor = if switch_roots {
        const HISTORY_WHEEL_EVENTS: usize = 8;
        const SCROLL_UP_WHEEL: &[u8] = b"\x1b[<64;1;1M";
        view.send(b"\x1b[D\x1b[D\x1b[D\x1b[D");
        for _ in 0..HISTORY_WHEEL_EVENTS {
            view.send(SCROLL_UP_WHEEL);
        }
        let reading = view
            .wait_for(
                "old image history with a pending image and middle cursor",
                |frame| {
                    frame.contains("AFTER-STASH")
                        && frame.contains("[Image #2]")
                        && frame.contains("image history")
                        && !frame.contains("baseline image received")
                },
            )
            .await;
        let anchor = reading
            .lines()
            .find(|line| line.contains("image history"))
            .unwrap()
            .trim()
            .to_owned();
        let captured_output = Arc::clone(&view.output);
        view.wait_for(
            "wheel reading keeps the image editor focused at the same anchor",
            |frame| {
                let captured = captured_output.lock().unwrap();
                let terminal = String::from_utf8_lossy(&captured);
                frame.contains(&anchor) && terminal.rfind("\x1b[?25h") > terminal.rfind("\x1b[?25l")
            },
        )
        .await;
        Some(anchor)
    } else {
        None
    };
    view.send(b"\x13");
    view.wait_for("stash removes the second draft from the editor", |frame| {
        !frame.contains("AFTER-STASH")
            && history_anchor
                .as_ref()
                .is_none_or(|anchor| frame.contains(anchor))
    })
    .await;
    if let Some(other) = other {
        view.send(b"\x1b[D");
        view.wait_for("stashed image permits the empty-input overview", |frame| {
            frame.contains("Agents · local") && frame.contains("BEFORE-STASH")
        })
        .await;
        select_overview_row(&mut view, "image-peer").await;
        view.send(b"\r");
        view.wait_for("peer displays only its independent Shell result", |frame| {
            frame.contains("B independent image peer")
                && !frame.contains("Agents · local")
                && !frame.contains("AFTER-STASH")
                && !frame.contains("[Image #2]")
        })
        .await;
        view.send(b"\x1b[D");
        view.wait_for("return overview still contains both image roots", |frame| {
            frame.contains("Agents · local")
                && frame.contains("image-peer")
                && frame.contains("BEFORE-STASH")
        })
        .await;
        select_overview_row(&mut view, "BEFORE-STASH").await;
        view.send(b"\r");
        view.wait_for(
            "return to the original image Agent before unstash",
            |frame| {
                frame.contains(history_anchor.as_ref().unwrap())
                    && !frame.contains("Agents · local")
                    && !frame.contains("AFTER-STASH")
                    && !frame.contains("baseline image received")
            },
        )
        .await;
        let peer = snapshot(&client, other).await;
        assert!(peer.last_model_run.is_none());
        assert!(peer.conversation.chat_loop.run_id.is_none());
        assert_eq!(peer.user_shells.len(), 1);
        assert_eq!(catalog(&client).await.len(), 2);
        let returned = snapshot(&client, first).await;
        assert_eq!(
            returned.conversation.folder,
            first_before.conversation.folder
        );
        assert_eq!(
            returned.conversation.chat_loop.id,
            first_before.conversation.chat_loop.id
        );
    }
    view.send(b"\x13");
    view.wait_for("unstash restores the second image label", |frame| {
        frame.lines().any(|line| {
            line.contains("❯") && line.contains("AFTER-STASH") && line.contains("[Image #2]")
        })
    })
    .await;
    let restored_suffix = if let Some(anchor) = &history_anchor {
        view.send(b"X");
        let cursor = DRAFT_SUFFIX.len() - CURSOR_DISTANCE_FROM_END;
        let expected = format!("{}X{}", &DRAFT_SUFFIX[..cursor], &DRAFT_SUFFIX[cursor..]);
        let restored = view
            .wait_for(
                "image draft restores the original cursor and reading anchor",
                |frame| {
                    frame.contains(&expected)
                        && frame.contains("[Image #2]")
                        && frame
                            .lines()
                            .find(|line| line.contains("image history"))
                            .is_some_and(|line| line.trim() == anchor)
                },
            )
            .await;
        assert_eq!(
            restored
                .lines()
                .find(|line| line.contains("image history"))
                .unwrap()
                .trim(),
            anchor
        );
        assert!(!restored.contains("baseline image received"));
        expected
    } else {
        DRAFT_SUFFIX.into()
    };
    view.send(b"\r");
    let (_, restored) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        user_content(&restored).to_string().contains("AFTER-STASH"),
        "the model request must be the restored draft, not its earlier control"
    );
    assert!(
        user_content(&restored)
            .to_string()
            .contains(&restored_suffix)
    );
    assert_eq!(
        image_urls(&user_content(&restored)),
        [expected_data_url],
        "Ctrl+S twice must preserve the image payload, not only its visible label"
    );
    view.wait_for("restored image response", |frame| {
        frame.contains("restored image received")
    })
    .await;
    let completed = wait_snapshot(&client, first, "restored image completes on its original Agent", |snapshot| {
        snapshot.conversation.chat_loop.state == tickloop_harness_protocol::LoopState::Idle
            && snapshot.events.iter().any(|event| matches!(
                &event.kind,
                tickloop_harness_protocol::EventKind::Text(text) if text.contains("restored image received")
            ))
    })
    .await;
    assert_eq!(completed.conversation.id, first_before.conversation.id);
    assert_eq!(
        completed.conversation.chat_loop.id,
        first_before.conversation.chat_loop.id
    );
    assert!(completed.last_model_run.is_some());
    assert_ne!(completed.last_model_run, first_before.last_model_run);
    view.close().await;
}

#[tokio::test]
async fn actual_cli_stash_round_trip_preserves_the_pasted_image_payload() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_stashed_image_payload(&fixture, false))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_stashed_image_survives_cross_root_navigation_and_restore() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_stashed_image_payload(&fixture, true))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_stashed_folded_paste(fixture: &PrivateFixture) {
    const FIRST_PASTE: &str = "first line\nsecond line\nthird line\nfourth line";
    const SECOND_PASTE: &str = "next first\nnext second\nnext third\nnext fourth";
    let (base_url, mut requests) = model_http_support::serve_responses(vec![(
        200,
        chat_reply(None, "both full pastes received"),
    )])
    .await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start(fixture);
    view.wait_for("folded paste View ready", new_input_is_visible)
        .await;
    view.send(b"SUMMARIZE ");
    view.send(format!("\x1b[200~{FIRST_PASTE}\x1b[201~ END").as_bytes());
    view.wait_for("large paste is folded before stash", |frame| {
        frame.contains("SUMMARIZE [Pasted text #1 +3 lines] END")
    })
    .await;
    view.send(b"\x13");
    view.wait_for("folded draft is stashed", |frame| {
        !frame.contains("SUMMARIZE")
    })
    .await;
    view.send(b"\x13");
    view.wait_for("unstash preserves the original folded block", |frame| {
        frame.contains("SUMMARIZE [Pasted text #1 +3 lines] END")
    })
    .await;
    view.send(format!(" NEXT \x1b[200~{SECOND_PASTE}\x1b[201~").as_bytes());
    view.wait_for("restored paste numbering is not reused", |frame| {
        frame.contains("[Pasted text #1 +3 lines]") && frame.contains("[Pasted text #2 +3 lines]")
    })
    .await;
    view.send(b"\r");
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let user = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| is_user_prompt(message))
        .unwrap();
    assert_eq!(
        user["content"].as_str().unwrap(),
        format!("SUMMARIZE {FIRST_PASTE} END NEXT {SECOND_PASTE}"),
        "the model must receive both exact paste originals, without display labels"
    );
    view.wait_for("full paste response", |frame| {
        frame.contains("both full pastes received")
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_stash_preserves_folded_paste_originals_and_numbering() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_stashed_folded_paste(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// The editor row; the conversation body above it also starts sent prompts with `❯`.
fn composer_line(frame: &str) -> &str {
    frame
        .lines()
        .rfind(|line| line.trim_start().starts_with('❯'))
        .expect("the composer prompt is painted")
}

async fn run_stashed_accepted_mention(fixture: &PrivateFixture) {
    const ORIGINAL_CONTENT: &str = "NOTES-BEFORE-STASH";
    const CURRENT_CONTENT: &str = "NOTES-AT-RESTORED-SUBMISSION";
    let notes = fixture.path("work/notes.txt");
    std::fs::write(&notes, ORIGINAL_CONTENT).unwrap();
    let (base_url, mut requests) = model_http_support::serve_responses(vec![(
        200,
        chat_reply(None, "accepted notes received"),
    )])
    .await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start(fixture);
    view.wait_for("mention View ready", new_input_is_visible)
        .await;
    view.send(b"Read @notes");
    view.wait_for("real file mention candidate is available", |frame| {
        frame.lines().any(|line| line.contains("notes.txt"))
    })
    .await;
    view.send(b"\t now");
    view.wait_for("file mention was accepted into the draft", |frame| {
        frame
            .lines()
            .any(|line| line.contains("❯") && line.contains("@notes.txt") && line.contains("now"))
    })
    .await;
    view.send(b"\x13");
    view.wait_for("accepted mention draft is stashed", |frame| {
        !frame.contains("Read @notes.txt")
    })
    .await;
    view.send(b"TEMPORARY-DRAFT");
    view.wait_for(
        "temporary editing keeps the mention stash separate",
        |frame| frame.contains("TEMPORARY-DRAFT"),
    )
    .await;
    view.send(b"\x03");
    view.wait_for("temporary draft is cleared before restoring", |frame| {
        !frame.contains("TEMPORARY-DRAFT")
    })
    .await;
    std::fs::write(&notes, CURRENT_CONTENT).unwrap();
    view.send(b"\x13");
    view.wait_for("accepted mention draft is restored", |frame| {
        frame
            .lines()
            .any(|line| line.contains("❯") && line.contains("@notes.txt") && line.contains("now"))
    })
    .await;
    view.send(b"\r");
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let user = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| is_user_prompt(message))
        .unwrap();
    let content = user["content"].as_str().unwrap();
    assert!(content.contains("Read @notes.txt now"));
    assert!(
        content.contains(CURRENT_CONTENT),
        "a restored accepted mention must still attach the file's current contents"
    );
    assert!(
        !content.contains(ORIGINAL_CONTENT),
        "restoring an accepted mention must retain ordinary submission revalidation"
    );
    view.wait_for("accepted file context response", |frame| {
        frame.contains("accepted notes received")
    })
    .await;
    let client = fixture.client().await;
    let execution = catalog(&client).await[0].execution;
    let before_root_change = snapshot(&client, execution).await;
    view.send(b"Read @notes");
    view.wait_for(
        "the original root still offers its accepted file",
        |frame| frame.lines().any(|line| line.contains("notes.txt")),
    )
    .await;
    view.send(b"\t now");
    view.wait_for("another accepted mention is ready to stash", |frame| {
        let composer = composer_line(frame);
        composer.contains("@notes.txt") && composer.contains("now")
    })
    .await;
    view.send(b"\x13");
    view.wait_for("accepted mention is stashed before roots change", |frame| {
        !composer_line(frame).contains("@notes.txt")
    })
    .await;
    let new_root = fixture.path("mention-new-root");
    std::fs::create_dir(&new_root).unwrap();
    std::fs::write(new_root.join("fresh-root.txt"), "NEW-ROOT-ONLY").unwrap();
    let new_root = new_root.canonicalize().unwrap();
    view.send(format!("/cwd {}\r", new_root.display()).as_bytes());
    view.wait_for("actual cwd command updates the allowed roots", |frame| {
        frame.contains("Working directory:")
            && frame.contains(&new_root.to_string_lossy().to_string())
    })
    .await;
    view.send(b"@fresh");
    view.wait_for("the live file index has the new root generation", |frame| {
        frame.contains("fresh-root.txt")
    })
    .await;
    view.send(b"\x1b");
    view.wait_for(
        "new-root completion is dismissed before clearing",
        |frame| !frame.contains("fresh-root.txt"),
    )
    .await;
    view.send(b"\x03");
    view.wait_for("temporary new-root query is cleared", |frame| {
        !frame.contains("@fresh")
    })
    .await;
    view.send(b"\x13");
    view.wait_for(
        "the original accepted mention is restored under current roots",
        |frame| {
            let composer = composer_line(frame);
            composer.contains("@notes.txt") && composer.contains("now")
        },
    )
    .await;
    view.send(b"\r");
    view.wait_for(
        "restored mention is revalidated against current allowed roots",
        |frame| {
            frame.contains("Cannot attach referenced path:")
                && frame.contains("outside the allowed directories")
                && composer_line(frame).contains("@notes.txt")
        },
    )
    .await;
    let after_root_change = snapshot(&client, execution).await;
    assert_eq!(
        after_root_change.conversation.folder,
        Some(new_root.clone())
    );
    assert_eq!(
        after_root_change.last_model_run,
        before_root_change.last_model_run
    );
    assert!(requests.try_recv().is_err());
    view.close().await;
}

#[tokio::test]
async fn actual_cli_stash_preserves_accepted_file_mentions_and_revalidates_submission() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_stashed_accepted_mention(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_stashed_shell_mode(fixture: &PrivateFixture) {
    const COMMAND: &str = "printf 'SHELL-MODE-RESTORED\\n'; pwd";
    let expected_directory = fixture.path("work").canonicalize().unwrap();
    let mut view = PtyView::start(fixture);
    view.wait_for("Shell stash View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    view.send(format!("!{COMMAND}").as_bytes());
    view.wait_for("unsubmitted Shell draft before stash", |frame| {
        frame.contains("!\u{a0}printf 'SHELL-MODE-RESTORED")
    })
    .await;
    view.send(b"\x13");
    view.wait_for("Shell stash leaves an empty ordinary prompt", |frame| {
        !frame.contains("SHELL-MODE-RESTORED")
    })
    .await;
    view.send(b"\x1b[D");
    view.wait_for(
        "stashed Shell permits the ordinary empty-input overview",
        |frame| frame.contains("Agents · local"),
    )
    .await;
    assert!(catalog(&client).await.is_empty());
    view.send(b"\x1b");
    view.wait_for("overview returns to the original new-input slot", |frame| {
        !frame.contains("Agents · local")
    })
    .await;
    view.send(b"\x13");
    view.wait_for("unstash restores the original Shell mode", |frame| {
        frame.contains("!\u{a0}printf 'SHELL-MODE-RESTORED")
    })
    .await;
    view.send(b"\r");
    let execution = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let Some(row) = catalog(&client).await.first() {
                break row.execution;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("restored Shell input must create one real execution");
    let submitted = wait_snapshot(
        &client,
        execution,
        "restored Shell executes without a model",
        |snapshot| {
            snapshot.user_shells.len() == 1
                && snapshot.user_shells[0].state == UserShellState::Completed
        },
    )
    .await;
    assert_eq!(submitted.user_shells[0].command, COMMAND);
    assert_eq!(
        submitted.conversation.folder,
        Some(expected_directory.clone())
    );
    assert!(submitted.last_model_run.is_none());
    assert!(submitted.conversation.chat_loop.run_id.is_none());
    view.wait_for("restored Shell output reaches the same View", |frame| {
        frame
            .lines()
            .any(|line| screen::response_line(line) == "SHELL-MODE-RESTORED")
            && frame
                .lines()
                .any(|line| screen::response_line(line) == expected_directory.to_string_lossy())
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_stash_restores_shell_mode_after_an_overview_round_trip() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_stashed_shell_mode(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn completed_shell_output(client: &LocalAgentClient, execution: ExecutionRef) -> String {
    let snapshot = wait_snapshot(client, execution, "user Shell completes", |snapshot| {
        snapshot
            .user_shells
            .first()
            .is_some_and(|shell| shell.state == UserShellState::Completed)
    })
    .await;
    let output = client
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::TaskOutput {
                task: TaskRef {
                    execution,
                    task_id: snapshot.user_shells[0].id,
                    origin: TaskOrigin::User,
                },
                offset: 0,
                limit: 4096,
            },
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::TaskOutput(output) = output.initial else {
        panic!("expected user Shell output");
    };
    output.output
}

async fn wait_new_execution(
    client: &LocalAgentClient,
    known: &[ExecutionRef],
    label: &str,
) -> AgentSummary {
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let Some(row) = catalog(client)
                .await
                .into_iter()
                .find(|row| !known.contains(&row.execution))
            {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label} must create one Agent"))
}

/// Selects the overview row showing `title` with real keys: up to the first line, then down,
/// opening each folded group heading it passes. Fails after a bounded number of keys.
async fn select_overview_row(view: &mut PtyView, title: &str) {
    const MAX_KEYS: usize = 80;
    let list = |frame: &str| {
        frame
            .lines()
            .take_while(|line| !line.starts_with("New Agent · cwd"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    view.send(&b"k".repeat(MAX_KEYS / 2));
    view.wait_for("overview selection reaches its first row", |frame| {
        frame
            .lines()
            .nth(2)
            .is_some_and(|line| line.starts_with(SELECTED_MARK))
    })
    .await;
    for _ in 0..MAX_KEYS {
        let lines = list(&view.frame());
        let selected_index = lines
            .iter()
            .position(|line| line.starts_with(SELECTED_MARK))
            .unwrap_or_else(|| panic!("no overview row is selected: {}", view.frame()));
        let selected = &lines[selected_index];
        if row_title(selected).contains(title)
            && !selected.starts_with(&format!("{SELECTED_MARK} ▾ "))
        {
            return;
        }
        if selected.starts_with(&format!("{SELECTED_MARK} ▸ ")) {
            let opened = selected.replacen('▸', "▾", 1);
            view.send(b"\r");
            view.wait_for("the selected overview group opens", |frame| {
                frame.lines().any(|line| line == opened)
            })
            .await;
        } else {
            let next = lines
                .get(selected_index + 1)
                .map(|line| row_title(line).split(" · ").next().unwrap())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| panic!("no overview row below {selected:?}: {}", view.frame()))
                .to_owned();
            view.send(b"j");
            view.wait_for("overview navigation selects the next listed row", |frame| {
                selected_row_title(frame).and_then(|title| title.split(" · ").next())
                    == Some(next.as_str())
            })
            .await;
        }
    }
    panic!("overview never selected {title}: {}", view.frame());
}

/// Opens the folded `group` heading with real keys: up to the first line, then down to it.
async fn open_group_heading(view: &mut PtyView, group: &str) {
    view.send(b"\x1b[5~");
    // New Agent is the second row; accepting it can acknowledge a frame from before PageUp.
    view.wait_for("overview selection reaches its first group", |frame| {
        selected_row_title(frame).is_some_and(|title| title.starts_with("Needs input"))
    })
    .await;
    let folded = format!("{SELECTED_MARK} ▸ {group} · ");
    for _ in 0..40 {
        let frame = view.frame();
        if frame.contains(&folded) {
            view.send(b"\r");
            view.wait_for("the group heading opens", |frame| {
                frame.contains(&format!("{SELECTED_MARK} ▾ {group} · "))
            })
            .await;
            return;
        }
        let next = frame
            .lines()
            .skip_while(|line| !line.starts_with(SELECTED_MARK))
            .nth(1)
            .map(row_title)
            .and_then(|title| title.split(" · ").next())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| panic!("no row below the overview selection: {frame}"))
            .to_owned();
        view.send(b"j");
        view.wait_for("overview navigation selects the next listed row", |frame| {
            selected_row_title(frame).and_then(|title| title.split(" · ").next())
                == Some(next.as_str())
        })
        .await;
    }
    panic!("no folded {group} heading: {}", view.frame());
}

/// A listed row's text after its selection marker, indentation and state glyph.
fn row_title(line: &str) -> &str {
    let rest = line.trim_start_matches([SELECTED_MARK, ' ']);
    rest.split_once(' ')
        .filter(|(glyph, _)| glyph.chars().count() == 1)
        .map_or(rest, |(_, title)| title)
}

/// The selected overview row's text after its marker and state glyph.
fn selected_row_title(frame: &str) -> Option<&str> {
    frame
        .lines()
        .find(|line| line.starts_with(SELECTED_MARK))
        .map(row_title)
}

async fn run_filter_hidden_dispatch(fixture: &PrivateFixture) {
    let launch = fixture.path("work").canonicalize().unwrap();
    std::fs::create_dir(fixture.path("bravo")).unwrap();
    let bravo_directory = fixture.path("bravo").canonicalize().unwrap();
    let mut view = PtyView::start(fixture);
    view.wait_for("new View ready", new_input_is_visible).await;
    view.resize(80, 24).await;
    let client = fixture.client().await;
    view.send(b"\x1b[D");
    view.wait_for("overview", |frame| frame.contains("Agents · local"))
        .await;
    view.send(b"\t!pwd\r");
    let older = wait_new_execution(&client, &[], "the first launch-directory Shell").await;
    assert_eq!(older.folder, Some(launch.clone()));
    view.send(format!("/cwd {}\r", bravo_directory.display()).as_bytes());
    view.wait_for("dispatch directory moves to bravo", |frame| {
        frame
            .lines()
            .any(|line| line.starts_with("New Agent · cwd") && line.ends_with("bravo"))
    })
    .await;
    view.send(b"!printf bravo\r");
    let bravo = wait_new_execution(&client, &[older.execution], "the bravo Shell").await;
    assert_eq!(bravo.folder, Some(bravo_directory.clone()));
    view.send(format!("/cwd {}\r", launch.display()).as_bytes());
    view.wait_for("dispatch directory returns to launch", |frame| {
        frame
            .lines()
            .any(|line| line.starts_with("New Agent · cwd") && line.ends_with("work"))
    })
    .await;
    let older_output = completed_shell_output(&client, older.execution).await;
    let bravo_output = completed_shell_output(&client, bravo.execution).await;
    assert_eq!(older_output.trim(), launch.to_str().unwrap());
    assert_eq!(bravo_output, "bravo");
    view.send(b"\t");
    view.send(b"p");
    view.wait_for("bravo filter keeps its Shell listed", |frame| {
        frame
            .lines()
            .any(|line| line.starts_with("Project: ") && line.contains("bravo"))
            && frame
                .lines()
                .any(|line| row_title(line).starts_with("!printf bravo"))
    })
    .await;
    view.send(b"jj");
    view.wait_for("monitor bravo", |frame| {
        frame
            .lines()
            .any(|line| line.starts_with("Project: ") && line.contains("bravo"))
            && selected_row_title(frame).is_some_and(|title| title.starts_with("!printf bravo"))
    })
    .await;
    view.send(b"\t!pwd\r");
    let frame = view
        .wait_for("visible accept cue for the hidden Agent", |frame| {
            frame.lines().any(|line| {
                line.contains("Created !pwd (") && line.contains("hidden by Project filter")
            })
        })
        .await;
    let cue = frame
        .lines()
        .find_map(|line| line.find("Created !pwd (").map(|start| &line[start..]))
        .unwrap();
    // The cue names the new Agent by its title and the reading number its row carries; no
    // execution hash appears. Opening that row below proves which execution the number names.
    let number = cue["Created !pwd (".len()..]
        .split(')')
        .next()
        .unwrap()
        .to_owned();
    assert!(number.parse::<usize>().is_ok(), "{cue}");
    let label = format!("!pwd ({number})");
    assert!(
        frame
            .lines()
            .any(|line| line.starts_with("Project: ") && line.contains("bravo")),
        "{frame}"
    );
    assert!(
        !frame
            .lines()
            .any(|line| row_title(line).starts_with("!pwd")),
        "the monitored filter still hides both launch-directory Agents: {frame}"
    );
    assert!(
        frame
            .lines()
            .any(|line| line.starts_with("New Agent · cwd") && line.ends_with("work")),
        "{frame}"
    );
    // An Agent is listed as soon as it exists and takes its first input's title just after, so
    // wait until both launch-directory Shells carry theirs.
    let rows = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(&client).await;
            if rows.len() == 3 && rows.iter().filter(|row| row.title == "!pwd").count() == 2 {
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both launch-directory Shells are titled by their command");
    let created = rows
        .iter()
        .find(|row| row.execution != older.execution && row.execution != bravo.execution)
        .unwrap()
        .clone();
    assert!(
        !frame.contains(&created.execution.agent_id.simple().to_string()[..4]),
        "no execution hash names the created Agent: {frame}"
    );
    assert_eq!(created.folder, Some(launch.clone()));
    view.send(b"\x1b[A");
    view.wait_for("empty Up keeps the monitored selection", |frame| {
        selected_row_title(frame).is_some_and(|title| title.starts_with("!printf bravo"))
    })
    .await;
    assert_eq!(
        completed_shell_output(&client, created.execution)
            .await
            .trim(),
        launch.to_str().unwrap()
    );
    assert_eq!(
        completed_shell_output(&client, older.execution).await,
        older_output
    );
    assert_eq!(
        completed_shell_output(&client, bravo.execution).await,
        bravo_output
    );
    view.send(b"p");
    view.wait_for(
        "launch filter has projected both completed Shells",
        |frame| {
            frame
                .lines()
                .any(|line| line.starts_with("Project: ") && line.contains("work"))
                && frame.contains("▸ Idle · 2")
        },
    )
    .await;
    // Both finished Shell Agents are Idle; Idle starts folded behind its heading.
    open_group_heading(&mut view, "Idle").await;
    let mut frame = view
        .wait_for(
            "launch filter lists both identical tasks with reading numbers",
            |frame| {
                frame
                    .lines()
                    .filter(|line| row_title(line).starts_with("!pwd ("))
                    .count()
                    == 2
                    && frame
                        .lines()
                        .any(|line| line.contains(&format!("Created {label} · ")))
            },
        )
        .await;
    if !selected_row_title(&frame).is_some_and(|title| title.starts_with("!pwd (")) {
        view.send(b"j");
        frame = view
            .wait_for("the cursor enters the listed tasks", |frame| {
                selected_row_title(frame).is_some_and(|title| title.starts_with("!pwd ("))
            })
            .await;
    }
    let selected_new =
        |frame: &str| selected_row_title(frame).is_some_and(|title| title.starts_with(&label));
    let task_rows = frame
        .lines()
        .filter(|line| row_title(line).starts_with("!pwd ("))
        .collect::<Vec<_>>();
    assert_eq!(
        task_rows
            .iter()
            .filter(|line| row_title(line).starts_with(&label))
            .count(),
        1,
        "the number names exactly one row: {frame}"
    );
    // Rows re-sort as the Agents' states change (most recent first), so every key is chosen
    // from the latest frame, and a key that lands elsewhere after a re-sort is retried.
    let task_positions = |frame: &str| {
        let rows = frame
            .lines()
            .filter(|line| row_title(line).starts_with("!pwd ("))
            .collect::<Vec<_>>();
        let created = rows
            .iter()
            .position(|line| row_title(line).starts_with(&label));
        let selected = rows.iter().position(|line| line.starts_with(SELECTED_MARK));
        (created, selected, rows.len())
    };
    let mut fresh = None;
    for _ in 0..8 {
        let latest = view.frame();
        let (Some(created), selected, 2) = task_positions(&latest) else {
            tokio::time::sleep(Duration::from_millis(40)).await;
            continue;
        };
        let Some(selected) = selected else {
            select_overview_row(&mut view, "!pwd (").await;
            continue;
        };
        let previous_selection = latest
            .lines()
            .find(|line| line.starts_with(SELECTED_MARK))
            .unwrap()
            .to_owned();
        let away_from_new = selected == created;
        view.send(match (away_from_new, created == 0) {
            (true, true) | (false, false) => b"j",
            (true, false) | (false, true) => b"k",
        });
        let moved = tokio::time::timeout(UI_TIMEOUT, async {
            loop {
                let frame = view.frame();
                if !frame.lines().any(|line| line == previous_selection) {
                    break frame;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a navigation key moves the selection");
        if !away_from_new && selected_new(&moved) {
            fresh = Some(moved);
            break;
        }
    }
    let frame = fresh.unwrap_or_else(|| {
        panic!(
            "CLI PTY fresh selection of the row named by the cue: {}",
            view.frame()
        )
    });
    assert!(selected_new(&frame), "{frame}");
    view.send(b"\r");
    view.wait_for("opened the created Agent", |frame| {
        !frame.contains("Agents · local")
            && frame.contains("! pwd")
            && frame.contains("Background command \"pwd\" completed")
    })
    .await;
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(&client).await;
            let views = |execution: ExecutionRef| {
                rows.iter()
                    .find(|row| row.execution == execution)
                    .map_or(0, |row| row.views.len())
            };
            if views(created.execution) == 1 {
                assert_eq!(views(older.execution), 0);
                assert_eq!(views(bravo.execution), 0);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the View must register on the created execution");
    eprintln!(
        "W3 execution {:?} cue {label} selected and bound",
        created.execution
    );
    view.close().await;
}

#[tokio::test]
async fn actual_cli_filter_hidden_dispatch_names_the_created_agent_until_it_is_found() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_filter_hidden_dispatch(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_dense_overview(fixture: &PrivateFixture) {
    let mut view = PtyView::start(fixture);
    view.wait_for("dense View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    let mut model = tickloop_harness_test_support::config();
    model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
    let project = fixture.path("work").canonicalize().unwrap();
    let create = |initial: Option<UserInput>| {
        let mut launch = LaunchSpec::embedded(project.clone());
        launch.config_root = Some(fixture.path("config"));
        launch.model = Some(model.clone());
        launch.session_persistence = false;
        CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial,
                origin: None,
            },
        }
    };
    let mut empty = Vec::new();
    for _ in 0..5 {
        let CommandResult::Created { execution, .. } = client
            .execute(create(None))
            .await
            .unwrap()
            .into_result()
            .unwrap()
        else {
            panic!("a configured session registers an execution");
        };
        empty.push(execution);
    }
    for index in 0..3 {
        client
            .execute(create(Some(UserInput::Shell {
                command: format!("printf dense-{index}"),
                background: false,
            })))
            .await
            .unwrap()
            .into_result()
            .unwrap();
    }
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(&client).await;
            if rows.iter().filter(|row| row.has_work == Some(true)).count() == 3
                && rows
                    .iter()
                    .filter(|row| row.has_work == Some(false))
                    .count()
                    == 5
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the daemon reports which Agents have worked");
    view.send(b"\x1b[D");
    for (width, height) in [(40, 16), (80, 24), (120, 48)] {
        view.resize(width, height).await;
        let frame = view
            .wait_for("dense overview", |frame| {
                frame.contains("▸ Idle · 3") && frame.contains("▸ Empty · 5")
            })
            .await;
        assert!(
            frame
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width),
            "{width}×{height}: {frame}"
        );
        assert!(
            frame.lines().position(|line| line.contains("Idle · 3"))
                < frame.lines().position(|line| line.contains("Empty · 5")),
            "{width}×{height}: worked Agents come before never-used sessions: {frame}"
        );
        assert!(
            !frame.lines().any(|line| line.contains("· Empty")),
            "{width}×{height}: never-used sessions stay folded: {frame}"
        );
    }
    for _ in 0..8 {
        if view.frame().contains(&format!("{SELECTED_MARK} ▸ Empty")) {
            break;
        }
        view.send(b"j");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    view.wait_for("the Empty heading is selected", |frame| {
        frame.contains(&format!("{SELECTED_MARK} ▸ Empty · 5"))
    })
    .await;
    view.send(b"\r");
    let opened = view
        .wait_for("every never-used session becomes discoverable", |frame| {
            frame.contains(&format!("{SELECTED_MARK} ▾ Empty · 5"))
                && frame
                    .lines()
                    .filter(|line| line.contains("· Empty"))
                    .count()
                    == 5
        })
        .await;
    assert!(opened.contains("▸ Idle · 3"), "{opened}");
    view.send(b"j\r");
    let opened_execution = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let Some(row) = catalog(&client)
                .await
                .into_iter()
                .find(|row| empty.contains(&row.execution) && !row.views.is_empty())
            {
                break row.execution;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Enter on a revealed empty session connects to that execution");
    assert!(empty.contains(&opened_execution));
    assert_eq!(
        catalog(&client).await.len(),
        8,
        "opening creates nothing new"
    );
    view.close().await;
}

#[tokio::test]
async fn actual_cli_dense_overview_folds_never_used_sessions_and_keeps_them_openable() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_dense_overview(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Summary fields a daemon or worker built before them never sends.
const POST_LEGACY_SUMMARY_FIELDS: [&str; 3] = ["has_work", "updated_at", "preview"];

/// Rewrites the summaries of `legacy` executions into the schema an older worker sends, the way
/// a mixed old/new worker fleet reaches the View; other summaries keep their current facts.
fn strip_work_facts(value: &mut Value, legacy: &[ExecutionRef]) {
    match value {
        Value::Object(object) => {
            let is_legacy = object.contains_key("title")
                && object
                    .get("execution")
                    .and_then(|execution| {
                        serde_json::from_value::<ExecutionRef>(execution.clone()).ok()
                    })
                    .is_some_and(|execution| legacy.contains(&execution));
            if is_legacy {
                for field in POST_LEGACY_SUMMARY_FIELDS {
                    object.remove(field);
                }
            }
            object
                .values_mut()
                .for_each(|value| strip_work_facts(value, legacy));
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|value| strip_work_facts(value, legacy)),
        _ => {}
    }
}

/// Relays one View connection to the private daemon, rewriting replies into the legacy schema.
async fn relay_legacy(
    downstream: tokio::net::UnixStream,
    upstream: tokio::net::UnixStream,
    legacy: Arc<Vec<ExecutionRef>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut view_read, mut view_write) = downstream.into_split();
    let (mut daemon_read, mut daemon_write) = upstream.into_split();
    let requests = tokio::io::copy(&mut view_read, &mut daemon_write);
    let replies = async {
        while let Ok(length) = daemon_read.read_u32().await {
            let mut bytes = vec![0; length as usize];
            if daemon_read.read_exact(&mut bytes).await.is_err() {
                break;
            }
            let mut reply: Value = serde_json::from_slice(&bytes).unwrap();
            strip_work_facts(&mut reply, &legacy);
            let bytes = serde_json::to_vec(&reply).unwrap();
            if view_write.write_u32(bytes.len() as u32).await.is_err()
                || view_write.write_all(&bytes).await.is_err()
            {
                break;
            }
        }
    };
    tokio::select! {
        _ = requests => {}
        _ = replies => {}
    }
}

/// A private config root whose daemon endpoint relays to `fixture`'s daemon, rewriting the
/// `legacy` executions' summaries into the older schema. Only this fixture's own daemon is reached.
fn legacy_daemon_endpoint(
    fixture: &PrivateFixture,
    legacy: Vec<ExecutionRef>,
) -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let config_root = fixture.path("legacy");
    std::fs::create_dir(&config_root).unwrap();
    let directory = support::cli_runtime::adopt(&config_root);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let endpoint = directory.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
    let upstream = fixture.path("runtime/daemon.sock");
    let legacy = Arc::new(legacy);
    let relay = tokio::spawn(async move {
        while let Ok((downstream, _)) = listener.accept().await {
            let upstream = tokio::net::UnixStream::connect(&upstream).await.unwrap();
            tokio::spawn(relay_legacy(downstream, upstream, legacy.clone()));
        }
    });
    (config_root, relay)
}

/// Overview rows named `New Agent`; the dispatch line below the list (`New Agent · cwd …`) names
/// the draft, not a listed session.
fn unreported_rows(frame: &str) -> usize {
    frame
        .lines()
        .filter(|line| {
            line.contains("New Agent")
                && !line.contains("New Agent · cwd")
                && !line.contains("current session")
        })
        .count()
}

async fn run_legacy_dense_overview(fixture: &PrivateFixture) {
    let mut daemon_view = PtyView::start(fixture);
    daemon_view
        .wait_for("daemon View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    let mut model = tickloop_harness_test_support::config();
    model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
    let project = fixture.path("work").canonicalize().unwrap();
    let create = |initial: Option<UserInput>| {
        let mut launch = LaunchSpec::embedded(project.clone());
        launch.config_root = Some(fixture.path("config"));
        launch.model = Some(model.clone());
        launch.session_persistence = false;
        CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial,
                origin: None,
            },
        }
    };
    let mut unreported = Vec::new();
    for _ in 0..17 {
        let CommandResult::Created { execution, .. } = client
            .execute(create(None))
            .await
            .unwrap()
            .into_result()
            .unwrap()
        else {
            panic!("a configured session registers an execution");
        };
        unreported.push(execution);
    }
    for index in 0..3 {
        client
            .execute(create(Some(UserInput::Shell {
                command: format!("printf legacy-{index}"),
                background: false,
            })))
            .await
            .unwrap()
            .into_result()
            .unwrap();
    }
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(&client).await;
            if rows.len() == 20
                && rows
                    .iter()
                    .filter(|row| row.state == tickloop_harness_protocol::LoopState::Idle)
                    .count()
                    == 20
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the daemon lists every Agent as Idle");
    // Only the seventeen configured sessions arrive in the old schema; the three Shell Agents keep
    // their explicit work facts as known-worked controls.
    let (legacy, relay) = legacy_daemon_endpoint(fixture, unreported.clone());
    let mut view =
        PtyView::start_with_environment(fixture, &[], &[("TICKLOOP_CONFIG_DIR", &legacy)]);
    view.wait_for("legacy View ready", new_input_is_visible)
        .await;
    view.send(b"\x1b[D");
    for (width, height) in [(40, 16), (80, 24), (120, 48)] {
        view.resize(width, height).await;
        let frame = view
            .wait_for("legacy overview", |frame| {
                frame.lines().any(|line| line.starts_with("Agents"))
            })
            .await;
        let unknown = frame
            .lines()
            .find(|line| line.contains("Unknown · 17"))
            .unwrap_or_else(|| {
                panic!("{width}×{height}: the unreported count stays visible: {frame}")
            });
        assert!(unknown.contains('▸'), "{width}×{height}: {unknown}");
        assert!(
            frame.lines().any(|line| line.contains("Idle · 3")),
            "{width}×{height}: worked Idle Agents keep their own group: {frame}"
        );
        assert_eq!(
            unreported_rows(&frame),
            0,
            "{width}×{height}: unreported sessions are not listed one by one: {frame}"
        );
        assert!(
            frame
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width),
            "{width}×{height}: {frame}"
        );
    }
    for _ in 0..8 {
        if view.frame().contains(&format!("{SELECTED_MARK} ▸ Unknown")) {
            break;
        }
        view.send(b"j");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    view.wait_for("the Unknown heading is selected", |frame| {
        frame.contains(&format!("{SELECTED_MARK} ▸ Unknown"))
    })
    .await;
    view.send(b"\r");
    view.wait_for("the unreported sessions become discoverable", |frame| {
        frame.contains("▾ Unknown · 17") && unreported_rows(frame) >= 3
    })
    .await;
    view.send(b"j\r");
    let opened = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let Some(row) = catalog(&client)
                .await
                .into_iter()
                .find(|row| unreported.contains(&row.execution) && !row.views.is_empty())
            {
                break row.execution;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Enter on a revealed session connects to that execution");
    assert!(unreported.contains(&opened));
    assert_eq!(
        catalog(&client).await.len(),
        20,
        "nothing was created, stopped or removed"
    );
    view.close().await;
    relay.abort();
    daemon_view.close().await;
}

#[tokio::test]
async fn actual_cli_legacy_dense_overview_preserves_unknown_work_facts() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_legacy_dense_overview(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// What the composer's first row starts with: `❯`, or `!` while it takes a Shell command.
const COMPOSER_PROMPTS: [&str; 2] = ["❯", "!\u{a0}"];

/// Rows above the composer's upper rule; a sent prompt must be echoed there, not only left in the
/// editor or the provider request.
fn conversation_body(frame: &str) -> String {
    try_conversation_body(frame).expect("the composer prompt is painted")
}

fn try_conversation_body(frame: &str) -> Option<String> {
    let frame = screen::message_flow(frame);
    let lines: Vec<&str> = frame.lines().collect();
    let composer = lines.iter().rposition(|line| {
        let line = line.trim_start();
        COMPOSER_PROMPTS
            .iter()
            .any(|prompt| line.starts_with(prompt))
    })?;
    Some(lines[..composer.saturating_sub(1)].join("\n"))
}

/// Each marker appears exactly once in the conversation body, in the given order.
fn body_shows_once_in_order(frame: &str, markers: &[&str]) -> bool {
    let Some(body) = try_conversation_body(frame) else {
        return false;
    };
    let mut earliest = 0;
    markers.iter().all(|marker| {
        let found = body.matches(marker).count() == 1
            && body
                .find(marker)
                .is_some_and(|position| position >= earliest);
        if found {
            earliest = body.find(marker).unwrap();
        }
        found
    })
}

const PROMPTS: [&str; 2] = ["echo probe first submission", "echo probe live follow-up"];
const REPLIES: [&str; 2] = ["first reply arrives", "follow-up reply arrives"];

async fn run_sent_message_echo(fixture: &PrivateFixture, prompts: [&str; 2], gated_turn: usize) {
    let (base_url, mut requests, release) =
        model_http_support::serve_responses_with_gated_response(
            REPLIES
                .iter()
                .map(|reply| (200, chat_reply(None, reply)))
                .collect(),
            gated_turn,
        )
        .await;
    let model = fixture.set_chat_model(&base_url);
    let mut view = PtyView::start(fixture);
    view.wait_for("echo View ready", new_input_is_visible).await;
    let banner_model = format!("{} · {:?}", model.model, model.protocol);
    let banner_directory = fixture.path("work").to_string_lossy().into_owned();
    let assert_startup_banner = |frame: &str| {
        assert!(frame.contains(STARTUP_BANNER_TITLE), "{frame}");
        assert!(frame.contains(&banner_model), "{frame}");
        assert!(frame.contains(&banner_directory), "{frame}");
    };
    assert_startup_banner(&view.frame());
    let client = fixture.client().await;
    let mut release = Some(release);
    let mut conversation = Vec::new();
    for (turn, prompt) in prompts.iter().enumerate().take(gated_turn + 1) {
        view.send(format!("{prompt}\r").as_bytes());
        let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .expect("the provider receives the submitted turn")
            .unwrap();
        // A Skill reaches the model as its arguments, not as the typed command.
        let sent_text = prompt.strip_prefix("/tickloop ").unwrap_or(prompt);
        assert!(
            request["messages"].to_string().contains(sent_text),
            "turn {turn} reaches the model request"
        );
        let rows = catalog(&client).await;
        assert_eq!(rows.len(), 1, "one submission creates exactly one Agent");
        let stored = snapshot(&client, rows[0].execution).await;
        assert!(
            format!("{:?}", stored.conversation).contains(sent_text),
            "turn {turn} is stored in the live conversation"
        );
        conversation.push(*prompt);
        if turn < gated_turn {
            conversation.push(REPLIES[turn]);
            view.wait_for("ungated reply follows its prompt", |frame| {
                body_shows_once_in_order(frame, &conversation)
            })
            .await;
            continue;
        }
        let deadline = Instant::now() + UI_TIMEOUT;
        let mut frame = view.frame();
        while Instant::now() < deadline && !body_shows_once_in_order(&frame, &conversation) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            frame = view.frame();
        }
        eprintln!("[echo-frame turn {turn} before reply]\n{frame}\n[/echo-frame]");
        assert_startup_banner(&frame);
        assert!(
            body_shows_once_in_order(&frame, &conversation),
            "turn {turn}: each sent message is in the body once, in order, before the reply: {frame}"
        );
        release.take().unwrap().send(()).unwrap();
        conversation.push(REPLIES[turn]);
        let frame = view
            .wait_for("gated reply is displayed", |frame| {
                frame.contains(REPLIES[turn])
            })
            .await;
        eprintln!("[echo-frame turn {turn} after reply]\n{frame}\n[/echo-frame]");
        assert_startup_banner(&frame);
        assert!(
            body_shows_once_in_order(&frame, &conversation),
            "turn {turn}: each sent message stays once, in order, above its reply: {frame}"
        );
    }
    view.send(b"/clear\r");
    let frame = view
        .wait_for("a fresh New Agent draft", |frame| {
            frame.contains("Tickloop v") && !frame.contains(REPLIES[0])
        })
        .await;
    eprintln!("[echo-frame new draft after /clear]\n{frame}\n[/echo-frame]");
    assert_startup_banner(&frame);
    assert!(
        prompts
            .iter()
            .all(|prompt| !conversation_body(&frame).contains(prompt)),
        "the next New Agent draft does not show the created Agent's messages: {frame}"
    );
    assert_eq!(catalog(&client).await.len(), 1);
    view.close().await;
}

#[tokio::test]
async fn actual_cli_first_submission_is_visible_before_the_reply() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_sent_message_echo(&fixture, PROMPTS, 0))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_first_skill_submission_is_visible_before_the_reply() {
    let fixture = PrivateFixture::new();
    let prompts = ["/tickloop echo probe skill submission", PROMPTS[1]];
    let result = AssertUnwindSafe(run_sent_message_echo(&fixture, prompts, 0))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_live_follow_up_is_visible_before_the_reply() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_sent_message_echo(&fixture, PROMPTS, 1))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

const BUSY_INPUTS: [&str; 3] = ["busy alpha", "busy bravo", "busy charlie"];

enum BusyShape {
    /// The active run is in a tool when the inputs arrive, so they join it at the tool boundary.
    ToolBoundary { status_between: bool },
    /// The active run is waiting on a text-only reply; waiting inputs share the next run.
    TextOnly,
}

/// The index of the one conversation-body row that shows `text`.
fn body_row(body: &[&str], text: &str) -> usize {
    let rows = body
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains(text))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "{text} is shown once: {body:#?}");
    rows[0]
}

async fn run_busy_inputs(fixture: &PrivateFixture, shape: BusyShape) {
    let tool_boundary = matches!(shape, BusyShape::ToolBoundary { .. });
    let replies = if tool_boundary {
        vec![
            (
                200,
                chat_reply(
                    Some((
                        "Bash",
                        "hold-busy",
                        json!({"command":"printf 'work started\\n'; while [ ! -f finish ]; do sleep 0.02; done"}),
                    )),
                    "",
                ),
            ),
            (200, chat_reply(None, "answered all three")),
        ]
    } else {
        ["first reply", "batched reply"]
            .map(|reply| (200, chat_reply(None, reply)))
            .to_vec()
    };
    let queued_image = if tool_boundary {
        None
    } else {
        use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

        let mut png = Vec::new();
        ::image::DynamicImage::ImageRgba8(::image::RgbaImage::from_pixel(
            4,
            3,
            ::image::Rgba([180, 40, 90, 255]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut png),
            ::image::ImageFormat::Png,
        )
        .unwrap();
        let path = fixture.path("work/busy-chart.png");
        std::fs::write(&path, &png).unwrap();
        Some((
            path,
            format!("data:image/png;base64,{}", BASE64.encode(&png)),
        ))
    };
    let (base_url, mut requests, release) =
        model_http_support::serve_responses_with_gated_response(replies, 0).await;
    let mut release = Some(release);
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    view.wait_for("busy View ready", new_input_is_visible).await;
    let client = fixture.client().await;
    view.send(b"start the busy work\r");
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let execution = catalog(&client).await[0].execution;
    if tool_boundary {
        release.take().unwrap().send(()).unwrap();
        view.wait_for("the active run is in its tool", |frame| {
            frame.contains("work started")
        })
        .await;
    }
    let mut queued_receipts = Vec::new();
    for (index, input) in BUSY_INPUTS.iter().enumerate() {
        if index == 1
            && matches!(
                shape,
                BusyShape::ToolBoundary {
                    status_between: true
                }
            )
        {
            view.send(b"/status\r");
            view.wait_for("the local status between the inputs", |frame| {
                frame.contains("Chat ") && frame.contains("queued")
            })
            .await;
        }
        if index == 0
            && let Some((path, _)) = &queued_image
        {
            view.send(format!("\x1b[200~{input} {}\x1b[201~", path.display()).as_bytes());
            view.wait_for("the waiting message has its pasted image", |frame| {
                frame.contains(input) && frame.contains("[Image #1]")
            })
            .await;
            view.send(b"\r");
        } else {
            view.send(format!("{input}\r").as_bytes());
        }
        let queued = wait_snapshot(
            &client,
            execution,
            "the busy input is accepted",
            |snapshot| snapshot.pending_inputs.len() == index + 1,
        )
        .await;
        if !tool_boundary {
            queued_receipts = queued
                .pending_inputs
                .iter()
                .map(|pending| pending.receipt.clone())
                .collect();
        }
    }
    if tool_boundary {
        std::fs::write(fixture.path("work/finish"), "finish").unwrap();
    } else {
        assert_eq!(queued_receipts.len(), BUSY_INPUTS.len());
        assert!(
            queued_receipts
                .iter()
                .all(|receipt| receipt.loop_id == queued_receipts[0].loop_id)
        );
        assert_eq!(
            queued_receipts
                .iter()
                .map(|receipt| receipt.run_id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            BUSY_INPUTS.len()
        );
        let queued_frame = view
            .wait_for("all waiting inputs are only in the queue", |frame| {
                screen::queued_inputs(frame).len() == 3
                    && BUSY_INPUTS.iter().all(|input| frame.contains(input))
            })
            .await;
        let (body, preview) = screen::split_at_queue(&queued_frame).unwrap();
        for input in BUSY_INPUTS {
            assert!(
                !body.contains(input),
                "waiting input is not echoed before consumption: {queued_frame}"
            );
            assert_eq!(
                preview.matches(input).count(),
                1,
                "each waiting input has one preview: {queued_frame}"
            );
        }
        assert_eq!(
            preview.matches("[Image #1]").count(),
            1,
            "the image stays with its waiting input: {queued_frame}"
        );
        assert!(
            requests.try_recv().is_err(),
            "waiting inputs must not cause another model request before the current reply ends"
        );
        release.take().unwrap().send(()).unwrap();
    }
    let last_reply = if tool_boundary {
        "answered all three"
    } else {
        "batched reply"
    };
    let frame = view
        .wait_for("the busy work is answered", |frame| {
            frame.contains(last_reply)
        })
        .await;
    eprintln!("[busy-frame]\n{frame}\n[/busy-frame]");
    let mut sends = 1;
    let mut follow_up_requests = Vec::new();
    while let Ok(Some((_, request))) =
        tokio::time::timeout(Duration::from_millis(300), requests.recv()).await
    {
        sends += 1;
        follow_up_requests.push(request);
    }
    assert_eq!(sends, 2, "provider sends");
    if let Some((_, expected_data_url)) = queued_image {
        let request = &follow_up_requests[0];
        let user_parts = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .collect::<Vec<_>>();
        let texts = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .flat_map(|message| {
                message["content"].as_str().into_iter().chain(
                    message["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part["text"].as_str()),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut previous = 0;
        for input in BUSY_INPUTS {
            assert_eq!(
                texts.matches(input).count(),
                1,
                "the one follow-up request keeps every accepted message: {request}"
            );
            let position = texts.find(input).unwrap();
            assert!(
                position >= previous,
                "accepted message order is preserved: {texts}"
            );
            previous = position;
        }
        let image_urls = user_parts
            .iter()
            .filter(|part| part["type"] == "image_url")
            .map(|part| part["image_url"]["url"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            image_urls,
            [expected_data_url.as_str()],
            "the original pasted PNG reaches the same follow-up request once"
        );
        let completed = wait_snapshot(
            &client,
            execution,
            "the waiting batch completes as one run",
            |snapshot| {
                snapshot.pending_inputs.is_empty()
                    && snapshot.events.iter().any(|event| {
                        event.run_id == queued_receipts[0].run_id
                            && matches!(
                                event.kind,
                                tickloop_harness_protocol::EventKind::Completed { .. }
                            )
                    })
            },
        )
        .await;
        let started = completed
            .events
            .iter()
            .filter(|event| matches!(event.kind, tickloop_harness_protocol::EventKind::Started))
            .map(|event| event.run_id)
            .collect::<Vec<_>>();
        assert_eq!(started.len(), 2, "initial run plus one waiting batch");
        assert_eq!(started[1], queued_receipts[0].run_id);
        let merged = completed
            .events
            .iter()
            .filter_map(|event| match event.kind {
                tickloop_harness_protocol::EventKind::InputMerged { into_run_id } => {
                    Some((event.run_id, into_run_id))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            merged,
            queued_receipts[1..]
                .iter()
                .map(|receipt| (receipt.run_id, queued_receipts[0].run_id))
                .collect::<Vec<_>>()
        );
    }
    let body = conversation_body(&frame);
    let body = body.lines().collect::<Vec<_>>();
    let rows = BUSY_INPUTS.map(|input| body_row(&body, input));
    let opens_block = |row: usize| body[row].trim_start().starts_with('❯');
    let continues = |previous: usize, row: usize| {
        row == previous + 1 && body[row].starts_with("  ") && !opens_block(row)
    };
    match shape {
        BusyShape::ToolBoundary {
            status_between: false,
        } => assert!(
            opens_block(rows[0]) && continues(rows[0], rows[1]) && continues(rows[1], rows[2]),
            "inputs merged into one run read as one block: {body:#?}"
        ),
        BusyShape::ToolBoundary {
            status_between: true,
        } => assert!(
            opens_block(rows[0])
                && opens_block(rows[1])
                && rows[1] > rows[0] + 1
                && continues(rows[1], rows[2]),
            "a local command echo between merged inputs keeps them apart: {body:#?}"
        ),
        BusyShape::TextOnly => assert!(
            rows[0] < rows[1] && rows[1] < rows[2],
            "waiting inputs are shown once in accepted order: {body:#?}"
        ),
    }
    view.close().await;
}

#[tokio::test]
async fn actual_cli_inputs_merged_at_a_tool_boundary_read_as_one_block() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_busy_inputs(
        &fixture,
        BusyShape::ToolBoundary {
            status_between: false,
        },
    ))
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_a_local_command_between_merged_inputs_keeps_them_apart() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_busy_inputs(
        &fixture,
        BusyShape::ToolBoundary {
            status_between: true,
        },
    ))
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_waiting_messages_after_a_text_reply_enter_one_follow_up_request() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_busy_inputs(&fixture, BusyShape::TextOnly))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_busy_skill_barrier(fixture: &PrivateFixture) {
    let (base_url, mut requests,release) = model_http_support::serve_responses_with_gated_response(vec![
        (200,chat_reply(
            Some((
                "Bash",
                "hold-busy",
                json!({"command":"printf 'work started\\n'; while [ ! -f finish ]; do sleep 0.02; done"}),
            )),
            "",
        )),
        (200,chat_reply(None,"queued message finished")),
        (200,chat_reply(None,"queued skill finished")),
    ],2)
    .await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    view.wait_for("busy View ready", new_input_is_visible).await;
    let client = fixture.client().await;
    view.send(b"start the busy work\r");
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    view.wait_for("the active run is in its tool", |frame| {
        frame.contains("work started")
    })
    .await;
    let execution = catalog(&client).await[0].execution;
    view.send(b"busy alpha\r");
    wait_snapshot(
        &client,
        execution,
        "the message is a pending input",
        |snapshot| snapshot.pending_inputs.len() == 1,
    )
    .await;
    view.send(b"/tickloop busy skill\r");
    let pending_frame = view
        .wait_for("the Skill waits behind the turn", |frame| {
            screen::queued_inputs(frame).len() == 2
        })
        .await;
    let (pending_body, pending_preview) = screen::split_at_queue(&pending_frame).unwrap();
    assert!(
        !pending_body.contains("busy alpha") && !pending_body.contains("/tickloop busy skill"),
        "{pending_frame}"
    );
    assert!(
        pending_preview.contains("busy alpha") && pending_preview.contains("/tickloop busy skill"),
        "{pending_frame}"
    );
    let queued = wait_snapshot(
        &client,
        execution,
        "the Skill is a queued command",
        |snapshot| snapshot.pending_commands.len() == 1,
    )
    .await;
    assert_eq!(
        queued.pending_inputs.len(),
        1,
        "a busy Skill is a command barrier, not a message that can join the run"
    );
    assert!(
        requests.try_recv().is_err(),
        "nothing else reached the model"
    );
    std::fs::write(fixture.path("work/finish"), "finish").unwrap();
    for _ in 0..2 {
        tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    wait_snapshot(&client,execution,"the Skill is actually consumed",|snapshot| {
        snapshot.pending_commands.is_empty() && snapshot.conversation.messages.iter().any(|message| message.content.iter().any(|content| matches!(content,tickloop_harness_protocol::Content::Text {text} if text=="/tickloop busy skill")))
    }).await;
    let consumed = view
        .wait_for("the consumed Skill is echoed before its reply", |frame| {
            conversation_body(frame).contains("/tickloop busy skill")
                && screen::queued_inputs(frame).is_empty()
        })
        .await;
    let body = conversation_body(&consumed);
    assert_eq!(
        body.matches("/tickloop busy skill").count(),
        1,
        "{consumed}"
    );
    assert_eq!(body.matches("busy alpha").count(), 1, "{consumed}");
    release.send(()).unwrap();
    view.wait_for("the consumed Skill replies", |frame| {
        frame.contains("queued skill finished")
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_a_busy_skill_waits_as_a_command_barrier() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_busy_skill_barrier(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
