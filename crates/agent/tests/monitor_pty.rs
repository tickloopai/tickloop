mod support;

mod screen {
    include!("support/screen.rs");
}

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(15);
const TERMINAL_WIDTH: usize = 140;
const TERMINAL_HEIGHT: usize = 30;
const COMPLETED_FRAME_END: &str = "\x1b[?2026l";
const CURSOR_VISIBLE: &str = "\x1b[?25h";
const CURSOR_HIDDEN: &str = "\x1b[?25l";

struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    output: Arc<Mutex<String>>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Session {
    fn start(
        directory: &std::path::Path,
        origin: Option<&str>,
        bin: Option<&std::path::Path>,
    ) -> Self {
        Self::spawn(
            directory,
            origin,
            bin,
            &["--working-directory-is-trusted", "--no-session-persistence"],
        )
    }
    /// Resumes a saved Chat, so its persisted Monitor membership is what the View shows.
    fn resume(directory: &std::path::Path, chat: uuid::Uuid) -> Self {
        Self::spawn(
            directory,
            None,
            None,
            &[
                "--working-directory-is-trusted",
                "--resume",
                &chat.to_string(),
            ],
        )
    }
    fn spawn(
        directory: &std::path::Path,
        origin: Option<&str>,
        bin: Option<&std::path::Path>,
        arguments: &[&str],
    ) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: TERMINAL_HEIGHT as u16,
                cols: TERMINAL_WIDTH as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.cwd(directory);
        command.args(arguments);
        command.env("TICKLOOP_CONFIG_DIR", directory.join("state"));
        command.env(
            "TICKLOOP_DAEMON_RUNTIME_DIR",
            support::cli_runtime::directory(),
        );
        command.env("HERDR_ENV", "0");
        command.env("TICKLOOP_AGENT_TUI", "fullscreen");
        if let Some(origin) = origin {
            command.env("TICKLOOP_ARTIFACT_ORIGIN", origin);
        }
        if let Some(bin) = bin {
            command.env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
            command.env("TICK_TEST_BROWSER_LOG", directory.join("browser.log"));
        }
        let child = pty.slave.spawn_command(command).unwrap();
        let writer = pty.master.take_writer().unwrap();
        let input = pty.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let capture = output.clone();
        let reader = thread::spawn(move || screen::capture_output(input, capture));
        drop(pty.slave);
        Self {
            child,
            writer,
            _master: pty.master,
            output,
            reader: Some(reader),
        }
    }
    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).unwrap();
        self.writer.flush().unwrap();
    }
    fn wait(&self, needle: &str) {
        self.wait_until_shown(needle, true);
    }
    fn wait_without(&self, needle: &str) {
        self.wait_until_shown(needle, false);
    }
    fn wait_until_shown(&self, needle: &str, shown: bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let frame = screen::visible_screen(
                &self.output.lock().unwrap(),
                TERMINAL_WIDTH,
                TERMINAL_HEIGHT,
            );
            if frame.contains(needle) == shown {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "PTY {} {needle:?}: {}",
                if shown {
                    "did not display"
                } else {
                    "still displays"
                },
                frame
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn frame(&self) -> String {
        screen::visible_screen(
            &self.output.lock().unwrap(),
            TERMINAL_WIDTH,
            TERMINAL_HEIGHT,
        )
    }
    fn wait_for_editor(&self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let captured = self.output.lock().unwrap().clone();
            let completed = captured
                .rfind(COMPLETED_FRAME_END)
                .map(|end| &captured[..end + COMPLETED_FRAME_END.len()]);
            let frame = screen::visible_screen(&captured, TERMINAL_WIDTH, TERMINAL_HEIGHT);
            let editor_has_focus = completed
                .is_some_and(|output| output.rfind(CURSOR_VISIBLE) > output.rfind(CURSOR_HIDDEN));
            if editor_has_focus
                && frame.contains("⏵⏵ ")
                && frame.lines().any(|line| line.trim() == "❯")
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "PTY editor did not become ready: {frame}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn exit(&mut self) {
        self.wait_for_editor();
        self.send(b"/exit\r\r");
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "PTY did not exit: {}",
                screen::visible_screen(
                    &self.output.lock().unwrap(),
                    TERMINAL_WIDTH,
                    TERMINAL_HEIGHT
                )
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn shell_tick_appears_can_be_focused_and_removed_in_an_isolated_pty() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(directory.path(), None, None);
    session.wait_for_editor();
    assert!(!session.output.lock().unwrap().contains("Monitor 0"));
    session.send(b"!printf shell-tick-pty; while [ ! -f finish ]; do sleep 0.02; done\r");
    session.wait("Shell running · Ctrl+B background");
    session.send(b"\x02");
    session.wait("Shells (1)");
    std::fs::write(directory.path().join("finish"), "").unwrap();
    session.wait("exit 0");
    session.send(b"\x1b[Cx");
    session.wait_without("Shells (1)");
    session.wait_for_editor();
    session.send(b"/monitor\r\r");
    let settings_path = directory.path().join("state/settings.json");
    let deadline = Instant::now() + TIMEOUT;
    while !settings_path.exists() {
        assert!(
            Instant::now() < deadline,
            "Tick preference was not saved: {}",
            session.output.lock().unwrap()
        );
        thread::sleep(Duration::from_millis(20));
    }
    session.exit();
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("state/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["monitor"], "auto");
}

#[test]
fn artifact_login_opens_picker_adds_tick_and_uses_a_substitute_browser() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let endpoint = origin.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_server = stop.clone();
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let server = thread::spawn(move || {
        while !stop_server.load(std::sync::atomic::Ordering::Relaxed) {
            let Ok((mut socket, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(&mut socket).read_line(&mut request).unwrap();
            let user = json!({"id":"owner","name":"Owner","email":"owner@example.test"});
            let artifact = json!({"id":id,"title":"PTY Research","format":"markdown","access_role":"owner","url":"https://docs.example/a/test","updated_at":"2026-10-06T00:00:00Z","metadata_revision":1});
            let body = if request.starts_with("POST /api/v1/auth/device ") {
                json!({"device_code":"test-device","user_code":"ABCD","verification_uri":format!("{endpoint}/device?code=ABCD"),"expires_in":60,"interval":1})
            } else if request.starts_with("POST /api/v1/auth/device/token ") {
                json!({"access_token":"isolated-test-token","token_type":"Bearer","expires_in":600,"user":user})
            } else if request.starts_with("GET /api/v1/auth/session ") {
                json!({"user":user,"expires_at":"2099-01-01T00:00:00Z"})
            } else if request.starts_with("GET /api/v1/artifacts/pinned ") {
                json!({"artifacts":[],"next_cursor":null})
            } else if request.starts_with("GET /api/v1/artifacts?") {
                json!({"artifacts":[artifact],"next_cursor":null})
            } else {
                artifact
            };
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
        }
    });
    let result = std::panic::catch_unwind(|| {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let browser = bin.join(if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        });
        std::fs::write(
            &browser,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$TICK_TEST_BROWSER_LOG\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut session = Session::start(directory.path(), Some(&origin), Some(&bin));
        session.wait_for_editor();
        session.send(b"/artifacts\r");
        session.wait("ABCD");
        session.wait("[All]");
        session.wait("PTY Research");
        session.send(b"\r");
        session.wait("Artifacts (1)");
        session.wait("Monitor 1");
        let focus_deadline = Instant::now() + TIMEOUT;
        loop {
            let frame = screen::visible_screen(
                &session.output.lock().unwrap(),
                TERMINAL_WIDTH,
                TERMINAL_HEIGHT,
            );
            if frame
                .lines()
                .filter_map(|line| line.split_once('│').map(|(_, monitor)| monitor))
                .any(|monitor| monitor.contains('›') && monitor.contains("PTY Research"))
                && frame
                    .lines()
                    .filter_map(|line| line.split_once('│').map(|(_, monitor)| monitor))
                    .any(|monitor| monitor.contains("Enter open"))
            {
                break;
            }
            assert!(
                Instant::now() < focus_deadline,
                "added artifact must receive Monitor focus before opening: {frame}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        session.send(b"\r");
        let deadline = Instant::now() + TIMEOUT;
        let log = directory.path().join("browser.log");
        while !log.exists() {
            assert!(Instant::now() < deadline, "substitute browser not invoked");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(log).unwrap().trim(),
            "https://docs.example/a/test"
        );
        session.send(b"x");
        session.exit();
    });
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn a_saved_member_is_shown_detailed_removed_and_restored_across_restarts_in_a_real_pty() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let chat = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let agent = tickloop_agent::runtime::Agent::open_embedded(
                workdir.join("state"),
                support::config(),
                Arc::new(support::ScriptedModel::new(Vec::new())),
            )
            .unwrap();
            let chat = agent.create_chat(workdir.clone()).await.unwrap();
            agent
                .add_monitor_member(
                    chat,
                    "herdr:pty-member".parse().unwrap(),
                    Some("PTY member".into()),
                )
                .await
                .unwrap();
            agent.shutdown().await;
            chat
        });

    let mut session = Session::resume(&workdir, chat);
    session.wait_for_editor();
    session.wait("Monitor 1");
    session.wait("PTY member");
    session.send(b"jk");
    session.wait("❯\u{a0}jk");
    assert!(
        session.frame().contains("PTY member"),
        "typing j/k edits the input while Monitor is not focused"
    );
    session.send(b"\x7f\x7f");
    session.wait_without("❯\u{a0}jk");
    session.send(b"\x1b[C");
    session.wait("Enter details · x remove · Esc");
    session.send(b"\r");
    session.wait("Owner     Herdr");
    session.wait("Resources unavailable");
    session.send(b"\x1b");
    session.wait_without("Owner     Herdr");
    session.send(b"x");
    session.wait_without("PTY member");
    session.wait_without("Monitor 1");
    session.exit();
    drop(session);

    let mut restarted = Session::resume(&workdir, chat);
    restarted.wait_for_editor();
    thread::sleep(Duration::from_millis(500));
    let frame = restarted.frame();
    assert!(
        !frame.contains("Monitor 1") && !frame.contains("PTY member"),
        "the removal survives a restart: {frame}"
    );
    restarted.send(b"/monitor add\r");
    restarted.wait("Add to Monitor · Removed");
    restarted.send(b"\r");
    restarted.wait("Added to Monitor");
    restarted.send(b"\x1b");
    restarted.wait("Monitor 1");
    restarted.wait("Enter details · x remove · Esc");
    restarted.send(b"\x1b");
    restarted.exit();
}
