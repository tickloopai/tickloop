mod support;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::json;
use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(15);

struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    output: Arc<Mutex<String>>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Session {
    fn start(
        directory: &std::path::Path,
        origin: Option<&str>,
        bin: Option<&std::path::Path>,
    ) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 140,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.cwd(directory);
        command.args(["--working-directory-is-trusted", "--no-session-persistence"]);
        command.env("TICKLOOP_CONFIG_DIR", directory.join("state"));
        command.env(
            "TICKLOOP_AGENT_RUNTIME_DIR",
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
        let mut input = pty.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let capture = output.clone();
        let reader = thread::spawn(move || {
            let mut bytes = [0_u8; 8192];
            while let Ok(length) = input.read(&mut bytes) {
                if length == 0 {
                    break;
                }
                capture
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&bytes[..length]));
            }
        });
        drop(pty.slave);
        Self {
            child,
            writer,
            master: pty.master,
            output,
            reader: Some(reader),
        }
    }
    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).unwrap();
        self.writer.flush().unwrap();
    }
    fn wait(&self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
        loop {
            if ansi
                .replace_all(&self.output.lock().unwrap(), "")
                .contains(needle)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "PTY did not display {needle:?}: {}",
                self.output.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn exit(&mut self) {
        self.send(b"\x1b");
        thread::sleep(Duration::from_millis(100));
        self.send(b"/exit\r\r");
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "PTY did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.master.resize(PtySize {
            rows: 30,
            cols: 140,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.reader.take();
    }
}

#[test]
fn shell_tick_appears_can_be_focused_and_removed_in_an_isolated_pty() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::start(directory.path(), None, None);
    session.wait("Auto");
    assert!(!session.output.lock().unwrap().contains("No Ticks"));
    session.send(b"!printf shell-tick-pty; while [ ! -f finish ]; do sleep 0.02; done\r");
    session.wait("shell-tick-pty");
    session.send(b"\x02");
    session.wait("Shells (1)");
    std::fs::write(directory.path().join("finish"), "").unwrap();
    session.wait("exit 0");
    thread::sleep(Duration::from_millis(300));
    session.send(b"\x1b[Cx");
    session.send(b"/ticks\r\r");
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
    assert_eq!(saved["tickList"], "auto");
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
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0_u8; 8192];
            let count = socket.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]);
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
    session.wait("Auto");
    session.send(b"/artifacts\r");
    session.wait("ABCD");
    session.wait("[All]");
    session.wait("PTY Research");
    session.send(b"\r");
    session.wait("Artifacts (1)");
    session.wait("Ticks 1");
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
    session.send(b"x\x1b");
    session.exit();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
}
