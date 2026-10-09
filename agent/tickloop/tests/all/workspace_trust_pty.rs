use crate::support;

use tickloop_agent_test_support::screen;

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use portable_pty::{PtySize, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = support::runtime_dir();
        for name in ["home", "config", "runtime", "scratch", "work"] {
            let directory = root.path().join(name);
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(
            root.path().join("work/model.json"),
            "untrusted invalid configuration",
        )
        .unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn environment(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.path("home")),
            ("TICKLOOP_CONFIG_DIR", self.path("config")),
            ("TICKLOOP_DAEMON_RUNTIME_DIR", self.path("runtime")),
            ("TICKLOOP_TMPDIR", self.path("scratch")),
        ]
    }

    fn valid_model(&self) {
        fs::write(self.path("work/model.json"), r#"{"name":"trust-fixture","protocol":"messages","base_url":"http://127.0.0.1:1","model":"trust-fixture","api_key_env":"","max_output_tokens":32}"#).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = support::fixture_cli::command()
            .args(["daemon", "stop"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .envs(self.environment())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<String>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    size: PtySize,
}

impl Terminal {
    fn start(fixture: &Fixture, renderer: &str, flags: &[&str]) -> Self {
        Self::start_with_size(fixture, renderer, flags, 120, 30)
    }

    fn start_with_size(
        fixture: &Fixture,
        renderer: &str,
        flags: &[&str],
        cols: u16,
        rows: u16,
    ) -> Self {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pty = native_pty_system().openpty(size).unwrap();
        let mut command = support::fixture_cli::pty_command();
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("TERM", "xterm-256color");
        command.env("TICKLOOP_AGENT_TUI", renderer);
        for (name, value) in fixture.environment() {
            command.env(name, value);
        }
        command.arg("--cwd");
        command.arg(fixture.path("work"));
        command.arg("--transport-fixture");
        command.arg(fixture.path("work/model.json"));
        command.args(flags);
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let captured = output.clone();
        thread::spawn(move || screen::capture_output(reader, captured));
        Self {
            child,
            writer,
            output,
            master: pty.master,
            size,
        }
    }

    fn frame(&self) -> String {
        screen::visible_screen(
            &self.output.lock().unwrap(),
            self.size.cols as usize,
            self.size.rows as usize,
        )
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.output.lock().unwrap().clear();
        self.size.cols = cols;
        self.size.rows = rows;
        self.master.resize(self.size).unwrap();
    }

    fn wait(&self, text: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.frame().contains(text) {
            assert!(
                Instant::now() < deadline,
                "missing {text:?}: {}",
                self.frame()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn exit_code(&mut self) -> u32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            assert!(Instant::now() < deadline, "did not exit: {}", self.frame());
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct ModelProbe {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ModelProbe {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let counter = requests.clone();
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = Vec::new();
                    let complete = loop {
                        let mut bytes = [0; 8192];
                        let Ok(length) = stream.read(&mut bytes) else {
                            break false;
                        };
                        if length == 0 || request.len() > 1024 * 1024 {
                            break false;
                        }
                        request.extend_from_slice(&bytes[..length]);
                        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let body_bytes = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            if request.len() >= end + 4 + body_bytes {
                                break true;
                            }
                        }
                    };
                    if complete {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                } else {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        });
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ModelProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn startup_trust_requires_the_complete_directory_before_confirmation_after_resize() {
    use tickloop_agent_config::UserSettings;
    for renderer in ["inline", "fullscreen"] {
        let fixture = Fixture::new();
        fixture.valid_model();
        let mut terminal = Terminal::start_with_size(&fixture, renderer, &[], 80, 4);
        terminal.wait("Resize terminal");
        terminal.send(b"j\r");
        thread::sleep(Duration::from_millis(100));
        assert!(terminal.child.try_wait().unwrap().is_none());
        assert!(!fixture.path("config/settings.json").exists());
        assert!(!fixture.path("config/projects").exists());
        assert_eq!(fs::read_dir(fixture.path("runtime")).unwrap().count(), 0);
        terminal.resize(80, 24);
        terminal.wait(fixture.path("work").to_str().unwrap());
        terminal.wait("Enter confirm");
        terminal.send(b"\r");
        terminal.wait("⏵⏵ ");
        assert!(
            UserSettings::in_root(&fixture.path("config"))
                .is_working_directory_trusted(&fixture.path("work"))
        );
        terminal.send(b"/exit\r");
        assert_eq!(terminal.exit_code(), 0);
    }
}

#[test]
fn startup_trust_real_input_pty_eof_is_quiet_without_persisting_trust() {
    use std::os::fd::{AsRawFd, FromRawFd};

    fn pty_files() -> (fs::File, fs::File) {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Separate input/output PTYs preserve output after the input master closes.
        unsafe {
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size
                ),
                0
            );
            let files = (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave));
            for file in [&files.0, &files.1] {
                assert_eq!(
                    libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC),
                    0
                );
            }
            files
        }
    }

    for renderer in ["inline", "fullscreen"] {
        let fixture = Fixture::new();
        let (input_master, input_slave) = pty_files();
        let (output_master, output_slave) = pty_files();
        let mut child = support::fixture_cli::command()
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("TICKLOOP_AGENT_TUI", renderer)
            .envs(fixture.environment())
            .arg("--cwd")
            .arg(fixture.path("work"))
            .arg("--transport-fixture")
            .arg(fixture.path("work/model.json"))
            .stdin(input_slave)
            .stdout(output_slave)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let captured = output.clone();
        let reader = thread::spawn(move || screen::capture_output(output_master, captured));
        let deadline = Instant::now() + TIMEOUT;
        while !output.lock().unwrap().contains("Enter confirm") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let displayed = output
            .lock()
            .unwrap()
            .contains(fixture.path("work").to_str().unwrap());
        drop(input_master);
        let deadline = Instant::now() + TIMEOUT;
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
        }
        let result = child.wait_with_output().unwrap();
        reader.join().unwrap();
        assert!(displayed, "{}", output.lock().unwrap());
        assert_eq!(result.status.code(), Some(0));
        assert!(result.stderr.is_empty(), "{:?}", result.stderr);
        assert!(!output.lock().unwrap().contains("Error:"));
        assert!(!fixture.path("config/settings.json").exists());
        assert!(!fixture.path("config/projects").exists());
        assert_eq!(fs::read_dir(fixture.path("runtime")).unwrap().count(), 0);
    }
}

#[test]
fn startup_trust_cancellation_is_quiet_and_precedes_project_configuration() {
    for renderer in ["inline", "fullscreen"] {
        for cancellation in [b"\r".as_slice(), b"j\x1b", b"\x03", b"\x04"] {
            let fixture = Fixture::new();
            let mut terminal = Terminal::start(&fixture, renderer, &[]);
            terminal.wait("Accessing workspace:");
            terminal.wait("No, exit");
            assert!(
                terminal
                    .frame()
                    .lines()
                    .any(|line| line.trim_start().starts_with(['❯', '>'])
                        && line.contains("No, exit"))
            );
            assert!(!fixture.path("config/settings.json").exists());
            assert!(!fixture.path("config/projects").exists());
            assert_eq!(fs::read_dir(fixture.path("runtime")).unwrap().count(), 0);
            terminal.send(cancellation);
            assert_eq!(terminal.exit_code(), 0);
            let output = terminal.output.lock().unwrap();
            assert!(!output.contains("Error:"), "{output}");
            assert!(!output.contains("[y/N]"));
            assert!(!fixture.path("config/settings.json").exists());
            assert!(!fixture.path("config/projects").exists());
        }
    }
}

#[test]
fn startup_trust_accepts_explicit_navigation_and_preserves_exact_scope_on_restart() {
    use tickloop_agent_config::UserSettings;
    for navigation in [b"j".as_slice(), b"\x1b[B", b"\x0e"] {
        let fixture = Fixture::new();
        fixture.valid_model();
        let mut terminal = Terminal::start(&fixture, "inline", &["--no-session-persistence"]);
        terminal.wait("No, exit");
        terminal.send(b"y1/exit\x1b[200~yes\x1b[201~");
        thread::sleep(Duration::from_millis(80));
        assert!(terminal.frame().contains("Accessing workspace:"));
        assert!(!fixture.path("config/settings.json").exists());
        terminal.send(navigation);
        terminal.send(b"\r\r");
        terminal.wait("⏵⏵ ");
        thread::sleep(Duration::from_millis(80));
        assert!(!fixture.path("config/projects").exists());
        let settings = UserSettings::in_root(&fixture.path("config"));
        assert!(settings.is_working_directory_trusted(&fixture.path("work")));
        fs::create_dir(fixture.path("work/child")).unwrap();
        assert!(!settings.is_working_directory_trusted(&fixture.path("work/child")));
        let alias = fixture.path("alias");
        std::os::unix::fs::symlink(fixture.path("work"), &alias).unwrap();
        assert!(settings.is_working_directory_trusted(&alias));
        terminal.send(b"/exit\r");
        assert_eq!(terminal.exit_code(), 0);
        drop(terminal);
        let mut restarted = Terminal::start(&fixture, "fullscreen", &[]);
        restarted.wait("⏵⏵ ");
        assert!(
            !restarted
                .output
                .lock()
                .unwrap()
                .contains("Accessing workspace:")
        );
        restarted.send(b"/exit\r");
        assert_eq!(restarted.exit_code(), 0);
    }
}

#[test]
fn startup_trust_short_write_failure_shows_the_error_and_requires_resize_before_retry() {
    for renderer in ["inline", "fullscreen"] {
        let fixture = Fixture::new();
        fixture.valid_model();
        fs::write(fixture.path("config/settings.json"), "invalid JSON").unwrap();
        let mut terminal = Terminal::start_with_size(&fixture, renderer, &[], 80, 5);
        terminal.wait("Accessing workspace:");
        terminal.send(b"j\r");
        terminal.wait("Cannot save directory trust:");
        terminal.wait("Resize terminal");
        terminal.resize(80, 1);
        terminal.wait("Cannot save directory trust:");
        terminal.send(b"j\r");
        thread::sleep(Duration::from_millis(80));
        assert_eq!(
            fs::read_to_string(fixture.path("config/settings.json")).unwrap(),
            "invalid JSON"
        );
        assert_eq!(fs::read_dir(fixture.path("runtime")).unwrap().count(), 0);
        terminal.resize(80, 5);
        terminal.wait("Resize terminal");
        fs::write(fixture.path("config/settings.json"), "{}").unwrap();
        terminal.send(b"j\r");
        thread::sleep(Duration::from_millis(80));
        assert_eq!(
            fs::read_to_string(fixture.path("config/settings.json")).unwrap(),
            "{}"
        );
        assert_eq!(fs::read_dir(fixture.path("runtime")).unwrap().count(), 0);
        terminal.resize(120, 30);
        terminal.wait("Cannot save directory trust:");
        terminal.wait("Yes, I trust this folder");
        terminal.send(b"\x0e\r");
        terminal.wait("⏵⏵ ");
        terminal.send(b"/exit\r");
        assert_eq!(terminal.exit_code(), 0);
        drop(terminal);
    }
}

#[test]
fn startup_trust_write_failure_stays_gated_and_requires_explicit_retry() {
    use tickloop_agent_config::UserSettings;
    let fixture = Fixture::new();
    fixture.valid_model();
    fs::write(fixture.path("config/settings.json"), "invalid JSON").unwrap();
    let mut terminal = Terminal::start(&fixture, "fullscreen", &[]);
    terminal.wait("Accessing workspace:");
    terminal.send(b"j\r");
    terminal.wait("Cannot save directory trust:");
    assert_eq!(
        fs::read_to_string(fixture.path("config/settings.json")).unwrap(),
        "invalid JSON"
    );
    assert_eq!(fs::read_dir(fixture.path("runtime")).unwrap().count(), 0);
    fs::write(fixture.path("config/settings.json"), "{}").unwrap();
    thread::sleep(Duration::from_millis(80));
    assert!(
        !UserSettings::in_root(&fixture.path("config"))
            .is_working_directory_trusted(&fixture.path("work"))
    );
    terminal.send(b"\x0e\r");
    terminal.wait("⏵⏵ ");
    assert!(
        UserSettings::in_root(&fixture.path("config"))
            .is_working_directory_trusted(&fixture.path("work"))
    );
    terminal.send(b"/exit\r");
    assert_eq!(terminal.exit_code(), 0);
}

#[test]
fn startup_trust_rejects_a_directory_replaced_after_the_question() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "inline", &[]);
    terminal.wait("Accessing workspace:");
    fs::rename(fixture.path("work"), fixture.path("previous-work")).unwrap();
    fs::create_dir(fixture.path("work")).unwrap();
    terminal.send(b"j\r");
    terminal.wait("The displayed directory changed");
    assert!(!fixture.path("config/settings.json").exists());
    assert_eq!(fs::read_dir(fixture.path("runtime")).unwrap().count(), 0);
    terminal.send(b"\x1b");
    assert_eq!(terminal.exit_code(), 0);
}

#[test]
fn startup_trust_navigation_and_repeated_enter_never_submit_a_model_request() {
    let fixture = Fixture::new();
    let probe = ModelProbe::new();
    fs::write(fixture.path("work/model.json"), serde_json::json!({
        "name":"trust-fixture", "protocol":"messages", "base_url": format!("http://{}", probe.address),
        "model":"trust-fixture", "api_key_env":"", "max_output_tokens":32,
    }).to_string()).unwrap();
    let mut terminal = Terminal::start(&fixture, "fullscreen", &[]);
    terminal.wait("Accessing workspace:");
    assert_eq!(probe.requests.load(Ordering::SeqCst), 0);
    terminal.send(b"j\r\r");
    terminal.wait("⏵⏵ ");
    thread::sleep(Duration::from_millis(100));
    assert_eq!(probe.requests.load(Ordering::SeqCst), 0);
    assert!(!fixture.path("config/projects").exists());
    terminal.send(b"jk\r");
    terminal.wait("Failed:");
    assert_eq!(probe.requests.load(Ordering::SeqCst), 1);
    terminal.send(b"/exit\r");
    assert_eq!(terminal.exit_code(), 0);
}

#[test]
fn startup_trust_noninteractive_refusal_and_host_assertion_keep_their_contracts() {
    use tickloop_agent_config::UserSettings;
    for host_assertion in [false, true] {
        let fixture = Fixture::new();
        fixture.valid_model();
        let mut command = support::fixture_cli::command();
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .envs(fixture.environment());
        command.arg("--cwd").arg(fixture.path("work"));
        command
            .arg("--transport-fixture")
            .arg(fixture.path("work/model.json"));
        if host_assertion {
            command.arg("--working-directory-is-trusted");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child.stdin.take().unwrap().write_all(b"/exit\n");
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.success(),
            host_assertion,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Accessing workspace:"));
        assert!(
            !UserSettings::in_root(&fixture.path("config"))
                .is_working_directory_trusted(&fixture.path("work"))
        );
        if !host_assertion {
            assert!(String::from_utf8_lossy(&output.stderr).contains("not trusted"));
        }
    }
}

#[test]
fn startup_trust_persists_the_exact_home_directory() {
    use tickloop_agent_config::UserSettings;
    let fixture = Fixture::new();
    fixture.valid_model();
    let home = fixture.path("home");
    let mut terminal = Terminal::start(&fixture, "inline", &["--cwd", home.to_str().unwrap()]);
    terminal.wait("Accessing workspace:");
    terminal.send(b"j\r");
    terminal.wait("⏵⏵ ");
    let settings = UserSettings::in_root(&fixture.path("config"));
    assert!(settings.is_working_directory_trusted(&home));
    assert!(!settings.is_working_directory_trusted(&fixture.path("work")));
    terminal.send(b"/exit\r");
    assert_eq!(terminal.exit_code(), 0);
}
