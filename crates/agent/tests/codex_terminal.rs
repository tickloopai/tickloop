#[path = "support/screen.rs"]
mod screen;

mod support;

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct TerminalFixture {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn std::io::Write + Send>,
    output: std::sync::Arc<std::sync::Mutex<String>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl TerminalFixture {
    fn start(root: &std::path::Path) -> Self {
        use std::io::Read;
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 35,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.env(
            "TICKLOOP_AGENT_RUNTIME_DIR",
            support::cli_runtime::directory(),
        );
        command.args(["--working-directory-is-trusted", "--mode", "acceptEdits"]);
        command.arg("--cwd");
        command.arg(root);
        command.env("TICKLOOP_CONFIG_DIR", root.join("state"));
        command.env("TICKLOOP_AGENT_TUI", "inline");
        command.env_remove("TMUX");
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured = output.clone();
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                captured
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..count]));
            }
        });
        Self {
            child,
            writer,
            output,
            _master: pty.master,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn output_mark(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn wait_for_frame_after(&self, mark: usize, label: &str, matches: impl Fn(&str) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let output = self.output.lock().unwrap().clone();
            let frame = screen::visible_screen(&output, 120, 35);
            if output[mark..].contains("\x1b[?2026l") && matches(&frame) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing {label}: {frame}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for(&self, text: &str) {
        let ansi = regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let output = self.output.lock().unwrap().clone();
            let visible = ansi.replace_all(&output, "");
            if visible.contains(text) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing {text}: {visible}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn exit(&mut self) {
        self.send(b"/exit\r\r");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "terminal did not exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for TerminalFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn model_menu_cancellation_restores_prompt_and_allows_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = TerminalFixture::start(&root);
    terminal.wait_for("deepseek-flash");
    let opened_menu = terminal.output_mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(opened_menu, "model choices opened", |frame| {
        frame.contains("Choose model ·")
    });
    let returned_provider = terminal.output_mark();
    terminal.send(b"j\x10\x1b");
    terminal.wait_for_frame_after(returned_provider, "model Provider returned", |frame| {
        frame.contains("Choose model Provider")
    });
    let cancelled_menu = terminal.output_mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(cancelled_menu, "model menu closed", |frame| {
        frame.contains("deepseek-flash") && !frame.contains("Choose model")
    });
    let reopened_menu = terminal.output_mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(reopened_menu, "model menu reopened", |frame| {
        frame.contains("Choose model ·")
    });
    let returned_provider = terminal.output_mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(returned_provider, "model Provider returned", |frame| {
        frame.contains("Choose model Provider")
    });
    let cancelled_model = terminal.output_mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(cancelled_model, "model menu closed", |frame| {
        frame.contains("deepseek-flash") && !frame.contains("Choose model")
    });
    terminal.exit();
    assert!(!root.join("state/auth").exists());
}

#[test]
fn logout_can_be_cancelled_while_another_process_holds_the_auth_lock() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let auth_dir = root.join("state/auth");
    std::fs::create_dir_all(&auth_dir).unwrap();
    for path in [root.join("state"), auth_dir.clone()] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let credentials = auth_dir.join("openai-codex-credentials.json");
    std::fs::write(&credentials, "saved credentials must survive cancellation").unwrap();
    std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(auth_dir.join("openai-codex.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut terminal = TerminalFixture::start(&root);
    terminal.wait_for("deepseek-flash");
    terminal.send(b"/provider\r");
    terminal.wait_for("Choose Provider");
    terminal.send(b"j\r");
    terminal.wait_for("Choose connection · OpenAI");
    terminal.send(b"js");
    terminal.wait_for("Removing Codex login");
    let cancelled = terminal.output_mark();
    terminal.send(b"\x03");
    terminal.wait_for("Logout cancelled; saved login remains.");
    assert!(credentials.exists());
    drop(lock);
    terminal.wait_for_frame_after(cancelled, "Provider after logout cancellation", |frame| {
        frame.contains("Choose connection · OpenAI")
    });
    terminal.send(b"\x1b");
    terminal.wait_for("Choose Provider");
    let returned = terminal.output_mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(returned, "prompt after logout cancellation", |frame| {
        !frame.contains("Choose Provider")
    });
    terminal.exit();
}

#[tokio::test]
async fn piped_commands_wait_for_logout_before_model_menu_and_exit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state_dir = root.join("state");
    let auth_dir = state_dir.join("auth");
    std::fs::create_dir_all(&auth_dir).unwrap();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let credential_path = auth_dir.join("openai-codex-credentials.json");
    std::fs::write(
        &credential_path,
        serde_json::json!({
            "access_token": "fake-access",
            "refresh_token": "fake-refresh",
            "account_id": "acct-test",
            "expires_at_ms": u64::MAX,
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&credential_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let lock_path = auth_dir.join("openai-codex.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    lock.lock().unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .env(
            "TICKLOOP_AGENT_RUNTIME_DIR",
            support::cli_runtime::directory(),
        )
        .args(["--working-directory-is-trusted", "--mode", "acceptEdits"])
        .arg("--cwd")
        .arg(&root)
        .arg("--no-session-persistence")
        .env("TICKLOOP_CONFIG_DIR", &state_dir)
        .env_remove("DEEPSEEK_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"/help\n/login\n/provider\n")
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let mut stdout_reader = BufReader::new(child.stdout.take().unwrap());
    let mut captured = String::new();
    let logout_selection = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert_ne!(stdout_reader.read_line(&mut line).await.unwrap(), 0);
            captured.push_str(&line);
            if line.contains("Sign out · Codex subscription") {
                break line
                    .trim()
                    .split_once('.')
                    .unwrap()
                    .0
                    .parse::<usize>()
                    .unwrap();
            }
        }
    })
    .await
    .unwrap();
    stdin
        .write_all(format!("{logout_selection}\ncancel\n/exit\n").as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let stdout_task = tokio::spawn(async move {
        stdout_reader.read_to_string(&mut captured).await.unwrap();
        captured
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(child.try_wait().unwrap().is_none());
    assert!(credential_path.exists());
    drop(lock);

    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let stdout = stdout_task.await.unwrap();
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("/provider"));
    assert!(!stdout.contains("/login"));
    assert!(stdout.contains("Unknown command; use /help."));
    assert!(!stdout.contains("/logout"));
    assert!(stdout.contains("Sign out · Codex subscription"));
    assert!(stdout.contains("Codex subscription"));
    assert!(!stdout.contains("fake-refresh"));
    assert!(!stdout.contains("Codex browser login"));
    let removed = stdout.find("Codex login removed locally").unwrap();
    let model_menu = stdout
        .rfind("Choose Provider")
        .unwrap_or_else(|| panic!("missing model menu: {stdout}"));
    let cancelled = stdout
        .find("Selection cancelled; configuration unchanged.")
        .unwrap();
    assert!(removed < model_menu && model_menu < cancelled, "{stdout}");
    assert!(!credential_path.exists());
}
