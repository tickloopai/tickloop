use tickloop_agent_test_support::screen;

use tickloop_agent_test_support::input_dialog;

use crate::support;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_SETTLE: Duration = Duration::from_millis(40);
const TEST_KEY_NAME: &str = "DEEPSEEK_API_KEY";
const TEST_KEY: &str = "fixture-provider-picker-key";

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<String>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    runtime: PathBuf,
    config_root: PathBuf,
    home: PathBuf,
    columns: usize,
    rows: usize,
}

impl Terminal {
    fn start(workdir: &Path) -> Self {
        Self::start_with_environment(workdir, &[], &[])
    }

    fn start_with_environment(
        workdir: &Path,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> Self {
        fs::create_dir_all(workdir.join("home")).unwrap();
        fs::create_dir_all(workdir.join("tmp")).unwrap();
        let runtime = support::cli_runtime::directory();
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 38,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.env("TICKLOOP_DAEMON_RUNTIME_DIR", &runtime);
        command.args(["--working-directory-is-trusted", "--mode", "acceptEdits"]);
        command.arg("--cwd");
        command.arg(workdir);
        command.args(arguments);
        command.env("HOME", workdir.join("home"));
        command.env("TICKLOOP_TMPDIR", workdir.join("tmp"));
        command.env("TICKLOOP_CONFIG_DIR", workdir.join("state"));
        command.env("TICKLOOP_AGENT_TUI", "inline");
        command.env_remove(TEST_KEY_NAME);
        command.env_remove("DEEPSEEK_API_KEY");
        command.env_remove("TYPESAFE_API_KEY");
        for name in [
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
        ] {
            command.env_remove(name);
        }
        command.env_remove("TMUX");
        for name in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"] {
            command.env_remove(name);
        }
        for model in tickloop_agent_model::cards::built_in_model_profiles() {
            if !model.api_key_env.is_empty() {
                command.env_remove(model.api_key_env);
            }
        }
        command.env("NO_PROXY", "127.0.0.1,localhost");
        command.env("no_proxy", "127.0.0.1,localhost");
        for (name, value) in environment {
            command.env(name, value);
        }
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let captured = output.clone();
        thread::spawn(move || {
            screen::capture_output(reader, captured);
        });
        Self {
            child,
            writer,
            output,
            _master: pty.master,
            runtime,
            config_root: workdir.join("state"),
            home: workdir.join("home"),
            columns: 120,
            rows: 38,
        }
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        self._master
            .resize(PtySize {
                cols: columns,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.columns = usize::from(columns);
        self.rows = usize::from(rows);
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).unwrap();
        self.writer.flush().unwrap();
    }

    fn mark(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn select_row(&mut self, label: &str) {
        for _ in 0..64 {
            let selected = {
                let output = self.output.lock().unwrap();
                let frame = screen::visible_screen(&output, self.columns, self.rows);
                frame.lines().any(|line| {
                    line.trim_start().starts_with(['❯', '›', '>']) && line.contains(label)
                })
            };
            if selected {
                self.send(b"\r");
                return;
            }
            self.send(b"j");
            thread::sleep(FRAME_SETTLE);
        }
        let output = self.output.lock().unwrap();
        panic!(
            "menu did not select {label:?}; latest frame:\n{}",
            screen::visible_screen(&output, self.columns, self.rows)
        );
    }

    fn wait_for(&self, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.visible_output().contains(expected) {
                thread::sleep(FRAME_SETTLE);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = self.output.lock().unwrap();
        let frame = screen::visible_screen(&output, self.columns, self.rows);
        panic!("terminal did not show {expected:?}; latest frame:\n{frame}");
    }

    fn wait_for_frame_after(&self, mark: usize, label: &str, matches: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let output = self.output.lock().unwrap().clone();
            if output[mark..].contains("\x1b[?2026l")
                && matches(&screen::visible_screen(&output, self.columns, self.rows))
            {
                if let Some(directory) = std::env::var_os("TICKLOOP_PICKER_EVIDENCE_DIR") {
                    let directory = PathBuf::from(directory);
                    fs::create_dir_all(&directory).unwrap();
                    let name: String = label
                        .chars()
                        .map(|character| {
                            if character.is_ascii_alphanumeric() {
                                character
                            } else {
                                '-'
                            }
                        })
                        .collect();
                    fs::write(
                        directory.join(format!("{name}.txt")),
                        screen::visible_screen(&output, self.columns, self.rows),
                    )
                    .unwrap();
                }
                thread::sleep(FRAME_SETTLE);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = self.output.lock().unwrap();
        let last_frame = screen::visible_screen(&output, self.columns, self.rows);
        panic!("terminal did not show expected {label} frame; latest:\n{last_frame}");
    }

    fn visible_output(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    fn assert_dialog(&self, label: &str, title: &str) {
        let frame = screen::visible_screen(&self.output.lock().unwrap(), self.columns, self.rows);
        input_dialog::assert_frame(&frame, self.rows, title);
        input_dialog::record_frame(label, &frame);
    }

    fn close_model_picker(&mut self) {
        let mark = self.mark();
        self.send(b"\x1b");
        self.wait_for_frame_after(mark, "model picker returned one level", |frame| {
            !frame.contains("Choose model")
        });
        self.close_provider_picker();
    }

    fn close_provider_picker(&mut self) {
        for _ in 0..6 {
            let frame =
                screen::visible_screen(&self.output.lock().unwrap(), self.columns, self.rows);
            if !frame.contains("Choose Provider")
                && !frame.contains("Choose model")
                && !frame.contains("Connection ·")
                && !frame.lines().any(|line| line == "Settings")
                && !frame.contains("Service account · Google")
            {
                return;
            }
            let mark = self.mark();
            self.send(b"\x1b");
            self.wait_for_frame_after(mark, "provider escape processed", |_| true);
        }
        panic!("Provider picker did not close");
    }

    fn exit(&mut self) {
        self.send(b"/exit\r\r");
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "terminal did not exit; latest frame:\n{}",
                screen::visible_screen(&self.output.lock().unwrap(), self.columns, self.rows)
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args(["daemon", "stop"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("TICKLOOP_CONFIG_DIR", &self.config_root)
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", &self.runtime)
            .output();
        if !result.is_ok_and(|output| output.status.success()) {
            eprintln!(
                "Fixture daemon stop failed; owned runtime retained at {}",
                self.runtime.display()
            );
        }
    }
}

fn save_key(root: &Path, key: &str) {
    tickloop_agent_model::credentials::save_api_key(&root.join("state"), TEST_KEY_NAME, key)
        .unwrap();
}

#[test]
fn saved_builtin_key_is_reused_and_model_browsing_does_not_save_a_selection() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_key(&root, TEST_KEY);
    let original = fs::read(root.join("state/.env")).unwrap();
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    let mark = terminal.mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(mark, "built-in model uses saved key", |frame| {
        frame.contains("Choose model · Signed-in Providers")
            && frame.contains("API key saved")
            && frame.contains("DeepSeek-V4.1-Flash")
    });
    terminal.assert_dialog("slash-model", "Choose model · Signed-in Providers");
    terminal.close_model_picker();
    assert!(!root.join("state/settings.json").exists());
    assert_eq!(fs::read(root.join("state/.env")).unwrap(), original);
    terminal.exit();
}

#[test]
fn builtin_missing_key_details_setup_continues_to_models_without_applying_selection() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    terminal.send(b"/provider\r");
    terminal.wait_for("Choose Provider · All");
    let mark = terminal.mark();
    terminal.send(b"p");
    terminal.wait_for_frame_after(mark, "built-in missing credential details", |frame| {
        frame.contains("Connection · DeepSeek") && frame.contains("Set API key")
    });
    terminal.assert_dialog("missing-credential-details", "Connection · DeepSeek");
    terminal.select_row("Set API key");
    terminal.wait_for("API key · DeepSeek");
    terminal.assert_dialog("missing-credential-editor", "API key · DeepSeek");
    let mark = terminal.mark();
    terminal.send(format!("{TEST_KEY}\r").as_bytes());
    terminal.wait_for_frame_after(mark, "built-in first setup continues to models", |frame| {
        frame.contains("Choose model · DeepSeek") && frame.contains("API key saved")
    });
    terminal.assert_dialog("credential-save-return", "Choose model · DeepSeek");
    assert!(!root.join("state/settings.json").exists());
    assert!(!terminal.output.lock().unwrap().contains(TEST_KEY));
    terminal.close_model_picker();
    terminal.exit();
}

#[test]
fn builtin_key_replacement_cancel_and_commit_are_atomic_and_return_to_details() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_key(&root, TEST_KEY);
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    terminal.send(b"/provider\r");
    terminal.wait_for("Choose Provider · All");
    let mark = terminal.mark();
    terminal.send(b"p");
    terminal.wait_for_frame_after(mark, "built-in saved credential details", |frame| {
        frame.contains("Connection · DeepSeek") && frame.contains("Replace API key")
    });
    terminal.select_row("Replace API key");
    terminal.wait_for("API key · DeepSeek");
    terminal.send(b"candidate-123-key");
    assert_eq!(
        tickloop_agent_model::credentials::load_api_key(&root.join("state"), TEST_KEY_NAME)
            .unwrap()
            .as_deref(),
        Some(TEST_KEY)
    );
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "replacement field closed", |frame| {
        frame.contains("API key · DeepSeek") && frame.contains("esc back")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "cancel replacement keeps details", |frame| {
        frame.contains("Connection · DeepSeek") && frame.contains("Replace API key")
    });
    assert_eq!(
        tickloop_agent_model::credentials::load_api_key(&root.join("state"), TEST_KEY_NAME)
            .unwrap()
            .as_deref(),
        Some(TEST_KEY)
    );
    let mark = terminal.mark();
    terminal.select_row("Replace API key");
    terminal.wait_for_frame_after(mark, "replacement field reopened", |frame| {
        frame.contains("API key · DeepSeek") && frame.contains("esc close field")
    });
    let mark = terminal.mark();
    terminal.send(b"committed-456-key\r");
    terminal.wait_for_frame_after(mark, "atomic replacement returns details", |frame| {
        frame.contains("Connection · DeepSeek") && frame.contains("API key saved")
    });
    assert_eq!(
        tickloop_agent_model::credentials::load_api_key(&root.join("state"), TEST_KEY_NAME)
            .unwrap()
            .as_deref(),
        Some("committed-456-key")
    );
    assert!(!root.join("state/settings.json").exists());
    assert!(
        !terminal
            .output
            .lock()
            .unwrap()
            .contains("committed-456-key")
    );
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "replacement details restores directory", |frame| {
        frame.contains("Choose Provider · All")
    });
    terminal.close_provider_picker();
    terminal.exit();
}

#[test]
fn model_confirmation_saves_only_builtin_identity_and_thinking() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_key(&root, TEST_KEY);
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    terminal.send(b"/model\r");
    terminal.wait_for("Choose model · Signed-in Providers");
    let mark = terminal.mark();
    terminal.send(b"l\r");
    terminal.wait_for_frame_after(mark, "built-in model and thinking applied", |frame| {
        frame.contains("Model selection: Applied")
    });
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("state/settings.json")).unwrap()).unwrap();
    assert_eq!(saved["defaultModel"]["provider"], "deepseek-api");
    assert!(saved.get("model").is_none());
    assert!(saved.get("providers").is_none());
    assert!(!root.join("state/providers").exists());
    terminal.close_model_picker();
    terminal.exit();
}

#[test]
fn environment_key_browsing_does_not_create_credentials_or_provider_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = Terminal::start_with_environment(&root, &[], &[(TEST_KEY_NAME, TEST_KEY)]);
    terminal.wait_for("deepseek-flash");
    let mark = terminal.mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(mark, "built-in environment credential reused", |frame| {
        frame.contains("Choose model · Signed-in Providers")
            && frame.contains("API key from environment")
    });
    assert!(!root.join("state/.env").exists());
    assert!(!root.join("state/providers").exists());
    terminal.close_model_picker();
    terminal.exit();
}

#[test]
fn shared_builtin_directory_keeps_region_product_headers_and_real_shortcuts_at_small_sizes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_key(&root, TEST_KEY);
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    terminal.resize(64, 18);
    let mark = terminal.mark();
    terminal.send(b"/provider\r");
    terminal.wait_for_frame_after(mark, "narrow built-in connection directory", |frame| {
        frame.contains("Choose Provider · All") && frame.contains("API key saved")
    });
    let mark = terminal.mark();
    terminal.send(b"f");
    terminal.wait_for_frame_after(mark, "configured built-in filter", |frame| {
        frame.contains("Choose Provider · Configured") && frame.contains("DeepSeek")
    });
    let frame = screen::visible_screen(&terminal.output.lock().unwrap(), 64, 18);
    assert!(
        !frame
            .lines()
            .any(|line| line.trim_start().starts_with("1. "))
    );
    assert!(!frame.contains("r Refresh"));
    terminal.close_provider_picker();
    terminal.exit();
}

#[test]
fn removed_model_file_command_is_rejected_and_preserves_user_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let path = root.join("external.yaml");
    fs::write(&path, "id: external-provider\nmodels: []\n").unwrap();
    let original = fs::read(&path).unwrap();
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    let mark = terminal.mark();
    terminal.send(format!("/model {}\r", path.display()).as_bytes());
    terminal.wait_for_frame_after(mark, "removed model file rejected", |frame| {
        frame.contains("Model files are not supported") && frame.contains("built-in catalog")
    });
    assert_eq!(fs::read(path).unwrap(), original);
    assert!(!root.join("state/settings.json").exists());
    terminal.exit();
}

#[test]
fn unsupported_saved_definition_has_one_notice_and_uses_builtin_default() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("state/providers")).unwrap();
    let path = root.join("state/providers/external.yaml");
    fs::write(&path, "not even valid YAML: [").unwrap();
    fs::write(
        root.join("state/settings.json"),
        r#"{"defaultModel":{"provider":"external","model":"private"},"providers":false}"#,
    )
    .unwrap();
    let original = fs::read(&path).unwrap();
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    terminal.wait_for("only the program's built-in catalog");
    assert_eq!(
        terminal
            .output
            .lock()
            .unwrap()
            .matches("only the program's built-in catalog")
            .count(),
        1
    );
    assert_eq!(fs::read(path).unwrap(), original);
    terminal.exit();
}

#[test]
fn builtin_provider_models_details_and_credentials_share_the_input_dialog() {
    fn change_page(terminal: &mut Terminal, keys: &[u8], title: &str) {
        let mark = terminal.mark();
        terminal.send(keys);
        terminal.wait_for_frame_after(mark, title, |frame| frame.contains(title));
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_key(&root, TEST_KEY);
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    terminal.resize(80, 24);
    change_page(&mut terminal, b"/provider\r", "Choose Provider · All");
    terminal.assert_dialog("slash-provider", "Choose Provider");
    change_page(&mut terminal, b"\r", "Choose model · DeepSeek");
    terminal.assert_dialog("provider-enter-models", "Choose model");
    change_page(&mut terminal, b"\x1b", "Choose Provider");
    change_page(&mut terminal, b"p", "Connection · DeepSeek");
    terminal.assert_dialog("provider-details", "Connection");
    let mark = terminal.mark();
    terminal.select_row("Replace API key");
    terminal.wait_for_frame_after(mark, "credential editor", |frame| {
        frame.contains("API key · DeepSeek")
    });
    terminal.assert_dialog("replace-credential-editor", "API key");
    let frame = screen::visible_screen(
        &terminal.output.lock().unwrap(),
        terminal.columns,
        terminal.rows,
    );
    assert!(!frame.contains(TEST_KEY));
    assert!(!frame.contains("1. Save API key"));
    assert!(frame.contains("ctrl+g"));
    change_page(&mut terminal, b"\x1b", "tab edit");
    change_page(&mut terminal, b"\x1b", "Connection · DeepSeek");
    change_page(&mut terminal, b"\x1b", "Choose Provider");
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "return to input", |frame| {
        !frame.contains("Choose Provider")
            && !frame.contains("Choose model")
            && !frame.contains("Connection ·")
    });
    terminal.exit();
}

#[test]
fn first_run_model_directory_and_settings_model_use_the_input_dialog() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    let frame = screen::visible_screen(&terminal.visible_output(), terminal.columns, terminal.rows);
    assert!(
        !frame.contains("Choose Provider") && !frame.contains("Choose model"),
        "{frame}"
    );
    input_dialog::record_frame("first-run-composer", &frame);
    let model_row = frame
        .lines()
        .enumerate()
        .filter_map(|(row, line)| line.find("deepseek-flash").map(|column| (row, column)))
        .last()
        .unwrap();
    terminal.send(
        format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            model_row.1 + 1,
            model_row.0 + 1,
            model_row.1 + 1,
            model_row.0 + 1
        )
        .as_bytes(),
    );
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "model status click escape processed", |frame| {
        frame.contains("esc again") && frame.contains("rewind")
    });
    let frame = screen::visible_screen(&terminal.visible_output(), terminal.columns, terminal.rows);
    assert!(
        !frame.contains("Choose Provider") && !frame.contains("Choose model"),
        "Model status is not a picker action:\n{frame}"
    );
    input_dialog::record_frame("model-status-click", &frame);
    let mark = terminal.mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(mark, "empty signed-in model list", |frame| {
        frame.contains("Choose model · Signed-in Providers")
    });
    terminal.assert_dialog(
        "first-run-model-directory",
        "Choose model · Signed-in Providers",
    );
    let mark = terminal.mark();
    terminal.select_row("Manage Providers");
    terminal.wait_for_frame_after(mark, "add connection directory", |frame| {
        frame.contains("Choose Provider · All")
    });
    terminal.assert_dialog("first-run-add-connection", "Choose Provider · All");
    terminal.close_provider_picker();
    let mark = terminal.mark();
    terminal.send(b"/settings\r");
    terminal.wait_for_frame_after(mark, "settings entry", |frame| frame.contains("Settings"));
    terminal.assert_dialog("settings", "Settings");
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "settings model", |frame| {
        frame.contains("Choose model · Signed-in Providers")
    });
    terminal.assert_dialog("settings-model", "Choose model · Signed-in Providers");
    terminal.close_provider_picker();
    terminal.exit();
}
