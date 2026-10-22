use tickloop_agent_test_support::screen;

use crate::support;

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
    root: std::path::PathBuf,
}

impl TerminalFixture {
    fn start(root: &std::path::Path) -> Self {
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        support::cli_runtime::adopt(&root.join("state"));
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 35,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.args(["--working-directory-is-trusted", "--mode", "acceptEdits"]);
        command.arg("--cwd");
        command.arg(root);
        command.env("TICKLOOP_CONFIG_DIR", root.join("state"));
        command.env("HOME", root.join("home"));
        command.env("TICKLOOP_TMPDIR", root.join("tmp"));
        for model in tickloop_agent_model::cards::built_in_model_profiles() {
            if !model.api_key_env.is_empty() {
                command.env_remove(model.api_key_env);
            }
        }
        command.env("DEEPSEEK_API_KEY", "unused-codex-terminal-fixture-key");
        for name in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"] {
            command.env_remove(name);
        }
        command.env("TICKLOOP_AGENT_TUI", "inline");
        command.env_remove("TMUX");
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured = output.clone();
        std::thread::spawn(move || {
            screen::capture_output(reader, captured);
        });
        Self {
            child,
            writer,
            output,
            _master: pty.master,
            root: root.into(),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn select_row(&mut self, label: &str) {
        for _ in 0..64 {
            let output = self.output.lock().unwrap().clone();
            let frame = screen::visible_screen(&output, 120, 35);
            if frame
                .lines()
                .any(|line| line.trim_start().starts_with(['❯', '›', '>']) && line.contains(label))
            {
                self.send(b"\r");
                return;
            }
            self.send(b"j");
            std::thread::sleep(Duration::from_millis(40));
        }
        panic!("missing selectable row {label}");
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
                "terminal did not exit: {}",
                screen::visible_screen(&self.output.lock().unwrap(), 120, 35)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for TerminalFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args(["daemon", "stop"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("TICKLOOP_CONFIG_DIR", self.root.join("state"))
            .output();
    }
}

#[test]
fn model_menu_cancellation_restores_prompt_and_allows_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = TerminalFixture::start(&root);
    let model_menu_closed = |frame: &str| {
        frame.contains("deepseek-flash")
            && !frame.contains("Choose model")
            && !frame.contains("Choose Provider")
            && frame.lines().any(|line| line.trim() == "❯")
    };
    terminal.wait_for("deepseek-flash");
    let opened_menu = terminal.output_mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(opened_menu, "model choices opened", |frame| {
        frame.contains("Choose model · Signed-in Providers")
    });
    let cancelled_menu = terminal.output_mark();
    terminal.send(b"j\x10\x1b");
    terminal.wait_for_frame_after(cancelled_menu, "model menu closed", model_menu_closed);
    let reopened_menu = terminal.output_mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(reopened_menu, "model menu reopened", |frame| {
        frame.contains("Choose model · Signed-in Providers")
    });
    let cancelled_model = terminal.output_mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(cancelled_model, "model menu closed", model_menu_closed);
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
    std::fs::write(&credentials, serde_json::json!({"access_token":"fake-access", "refresh_token":"fake-refresh", "account_id":"acct-test", "expires_at_ms":u64::MAX}).to_string()).unwrap();
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
    terminal.send(b"jjp");
    terminal.wait_for("Connection · OpenAI");
    terminal.select_row("Sign out…");
    terminal.wait_for("Sign out · OpenAI");
    let signing_out = terminal.output_mark();
    terminal.send(b"j\r");
    terminal.wait_for_frame_after(signing_out, "Connection retained during logout", |frame| {
        frame.contains("Connection · OpenAI") && !frame.contains("Sign out · OpenAI")
    });
    let cancelled = terminal.output_mark();
    terminal.send(b"\x1b");
    terminal.wait_for("Logout cancelled; saved login remains.");
    assert!(credentials.exists());
    drop(lock);
    terminal.wait_for_frame_after(cancelled, "Provider after logout cancellation", |frame| {
        frame.contains("Choose Provider · All") && frame.contains("Signed in")
    });
    let returned = terminal.output_mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(returned, "prompt after logout cancellation", |frame| {
        !frame.contains("Choose Provider")
    });
    terminal.exit();
}

#[tokio::test]
async fn piped_commands_wait_for_logout_before_provider_navigation_and_exit() {
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

    support::cli_runtime::adopt(&state_dir);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickloop"))
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
    let connection_selection = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert_ne!(stdout_reader.read_line(&mut line).await.unwrap(), 0);
            captured.push_str(&line);
            if line.contains("Codex subscription")
                && let Some(number) = line
                    .trim()
                    .split_once('.')
                    .and_then(|(number, _)| number.parse::<usize>().ok())
            {
                break number;
            }
        }
    })
    .await
    .unwrap();
    stdin
        .write_all(format!("{connection_selection}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let details_selection = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert_ne!(stdout_reader.read_line(&mut line).await.unwrap(), 0);
            captured.push_str(&line);
            if line.contains("Connection details")
                && let Some(number) = line
                    .trim()
                    .split_once('.')
                    .and_then(|(number, _)| number.parse::<usize>().ok())
            {
                break number;
            }
        }
    })
    .await
    .unwrap();
    stdin
        .write_all(format!("{details_selection}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let logout_selection = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert_ne!(stdout_reader.read_line(&mut line).await.unwrap(), 0);
            captured.push_str(&line);
            if line.contains("Sign out…")
                && let Some(number) = line
                    .trim()
                    .split_once('.')
                    .and_then(|(number, _)| number.parse::<usize>().ok())
            {
                break number;
            }
        }
    })
    .await
    .unwrap();
    stdin
        .write_all(format!("{logout_selection}\n2\ncancel\ncancel\ncancel\n/exit\n").as_bytes())
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
    assert!(stdout.contains("Sign out · OpenAI"));
    assert!(stdout.contains("Codex subscription"));
    assert!(!stdout.contains("fake-refresh"));
    assert!(!stdout.contains("Codex browser login"));
    let removed = stdout.find("Codex login removed locally").unwrap();
    let provider_directory = stdout
        .rfind("Choose Provider")
        .unwrap_or_else(|| panic!("missing Provider directory: {stdout}"));
    assert!(removed < provider_directory, "{stdout}");
    assert!(!credential_path.exists());
}
