use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

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
        const FRAME_BEGIN: &str = "\x1b[?2026h";
        const FRAME_END: &str = "\x1b[?2026l";
        let ansi = regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let output = self.output.lock().unwrap().clone();
            let recent = &output[mark..];
            let latest_complete_frame = recent
                .match_indices(FRAME_BEGIN)
                .filter_map(|(start, _)| {
                    let frame_start = start + FRAME_BEGIN.len();
                    let end = recent[frame_start..].find(FRAME_END)? + frame_start;
                    Some(&recent[frame_start..end])
                })
                .last();
            if let Some(frame) = latest_complete_frame
                && matches(&ansi.replace_all(frame, ""))
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing {label} in latest frame: {}",
                latest_complete_frame
                    .map(|frame| ansi.replace_all(frame, "").into_owned())
                    .unwrap_or_else(|| "<no complete frame>".into())
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
    terminal.send(b"/model\r");
    terminal.wait_for("Choose Provider");
    let cancelled_menu = terminal.output_mark();
    terminal.send(b"j\x10\x1b");
    terminal.wait_for_frame_after(cancelled_menu, "model menu closed", |frame| {
        frame.contains("deepseek-flash") && !frame.contains("Choose Provider")
    });
    let reopened_menu = terminal.output_mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(reopened_menu, "model menu reopened", |frame| {
        frame.contains("Choose Provider")
    });
    let cancelled_model = terminal.output_mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(cancelled_model, "model menu closed", |frame| {
        frame.contains("deepseek-flash") && !frame.contains("Choose Provider")
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
    terminal.send(b"/logout\r");
    terminal.wait_for("Removing Codex login");
    terminal.send(b"\x03");
    terminal.wait_for("Logout cancelled; saved login remains.");
    assert!(credentials.exists());
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/help\n/login\n/logout\n/model\ncancel\n/exit\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(child.try_wait().unwrap().is_none());
    assert!(credential_path.exists());
    drop(lock);

    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("/model → Provider"));
    assert!(!stdout.contains("/login"));
    assert!(stdout.contains("Unknown command; use /help."));
    assert!(stdout.contains("/logout"));
    assert!(stdout.contains("Codex subscription"));
    assert!(!stdout.contains("fake-refresh"));
    let removed = stdout.find("Codex login removed locally").unwrap();
    let model_menu = stdout
        .find("Choose Provider")
        .unwrap_or_else(|| panic!("missing model menu: {stdout}"));
    let cancelled = stdout
        .find("Selection cancelled; configuration unchanged.")
        .unwrap();
    assert!(removed < model_menu && model_menu < cancelled, "{stdout}");
    assert!(!credential_path.exists());
}
