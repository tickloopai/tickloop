use tickloop_harness_test_support::screen;

use tickloop_harness_test_support::input_dialog;

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
        if workdir.join("state/.env").exists() {
            Self::start_with_environment(workdir, &[], &[])
        } else {
            Self::start_with_environment(workdir, &[], &[(TEST_KEY_NAME, TEST_KEY)])
        }
    }

    fn start_with_environment(
        workdir: &Path,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> Self {
        if !workdir.join("state/settings.json").exists() {
            save_model(workdir);
        }
        Self::start_first_run(workdir, arguments, environment)
    }

    fn start_first_run(workdir: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Self {
        fs::create_dir_all(workdir.join("home")).unwrap();
        fs::create_dir_all(workdir.join("tmp")).unwrap();
        let runtime = support::cli_runtime::adopt(&workdir.join("state"));
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 38,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
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
        for model in tickloop_harness_model::cards::built_in_model_profiles() {
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

    fn assert_no_chats(&self) {
        use tickloop_code_access::{
            AgentAccess, AgentFilter, ObservationRequest, ObservationValue, Page,
        };

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let client = tickloop_code_client::LocalAgentClient::connect(
                    self.runtime.join("daemon.sock"),
                )
                .await
                .unwrap();
                let observation = client
                    .observe(ObservationRequest::Overview {
                        filter: AgentFilter::default(),
                        page: Page {
                            offset: 0,
                            limit: 1,
                        },
                        after: None,
                    })
                    .await
                    .unwrap();
                let ObservationValue::Overview(chats) = observation.initial else {
                    panic!("expected Chat overview");
                };
                assert!(chats.is_empty(), "model setup must not create a Chat");
            });
    }

    fn only_chat_snapshot(&self) -> Option<Box<tickloop_code_access::LiveChatSnapshot>> {
        let mut snapshots = self.chat_snapshots();
        assert!(snapshots.len() <= 1, "fixture must own at most one Chat");
        snapshots.pop()
    }

    fn chat_snapshots(&self) -> Vec<Box<tickloop_code_access::LiveChatSnapshot>> {
        use tickloop_code_access::{
            AgentAccess, AgentDetailQuery, AgentFilter, ObservationRequest, ObservationValue, Page,
        };

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let client = tickloop_code_client::LocalAgentClient::connect(
                    self.runtime.join("daemon.sock"),
                )
                .await
                .unwrap();
                let observation = client
                    .observe(ObservationRequest::Overview {
                        filter: AgentFilter::default(),
                        page: Page {
                            offset: 0,
                            limit: 3,
                        },
                        after: None,
                    })
                    .await
                    .unwrap();
                let ObservationValue::Overview(chats) = observation.initial else {
                    panic!("expected Chat overview");
                };
                assert!(chats.len() <= 2, "fixture must own at most two Chats");
                let mut snapshots = Vec::new();
                for chat in chats {
                    let observation = client
                        .observe(ObservationRequest::Agent {
                            execution: chat.execution,
                            detail: AgentDetailQuery::Snapshot,
                            after: None,
                        })
                        .await
                        .unwrap();
                    let ObservationValue::Agent(snapshot) = observation.initial else {
                        panic!("expected Chat snapshot");
                    };
                    snapshots.push(snapshot);
                }
                snapshots
            })
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
            !frame
                .lines()
                .any(|line| line.trim_start().starts_with("Choose model ·"))
        });
        self.close_provider_picker();
    }

    fn close_provider_picker(&mut self) {
        for _ in 0..6 {
            let frame =
                screen::visible_screen(&self.output.lock().unwrap(), self.columns, self.rows);
            if !frame.contains("Choose Provider")
                && !frame
                    .lines()
                    .any(|line| line.trim_start().starts_with("Choose model ·"))
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
        panic!(
            "Provider picker did not close:\n{}",
            screen::visible_screen(&self.output.lock().unwrap(), self.columns, self.rows)
        );
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
            .output();
        if !result.is_ok_and(|output| output.status.success()) {
            eprintln!(
                "Fixture daemon stop failed; owned runtime retained at {}",
                self.runtime.display()
            );
        }
    }
}

fn save_model(root: &Path) {
    let model = tickloop_harness_model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash")
        .unwrap();
    tickloop_code_settings::UserSettings::in_root(&root.join("state"))
        .save_model(&model, false)
        .unwrap();
}

fn assert_saved_model(root: &Path) {
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("state/settings.json")).unwrap()).unwrap();
    assert_eq!(saved["defaultModel"]["provider"], "deepseek-api");
    assert_eq!(saved["defaultModel"]["model"], "deepseek-flash");
}

fn save_key(root: &Path, key: &str) {
    tickloop_harness_model::credentials::save_api_key(&root.join("state"), TEST_KEY_NAME, key)
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
            && frame.contains("DeepSeek Flash (deepseek-flash)")
    });
    terminal.assert_dialog("slash-model", "Choose model · Signed-in Providers");
    terminal.close_model_picker();
    assert_saved_model(&root);
    assert_eq!(fs::read(root.join("state/.env")).unwrap(), original);
    terminal.exit();
}

#[test]
fn builtin_missing_key_details_setup_continues_to_models_without_applying_selection() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_model(&root);
    let mut terminal = Terminal::start_first_run(&root, &[], &[]);
    terminal.wait_for("Connection · DeepSeek");
    terminal.wait_for("Set API key");
    terminal.assert_dialog("missing-credential-details", "Connection · DeepSeek");
    terminal.select_row("Set API key");
    terminal.wait_for("API key · DeepSeek");
    terminal.assert_dialog("missing-credential-editor", "API key · DeepSeek");
    terminal.wait_for(TEST_KEY_NAME);
    let mark = terminal.mark();
    terminal.send(format!("{TEST_KEY}\r").as_bytes());
    terminal.wait_for_frame_after(mark, "built-in first setup continues to models", |frame| {
        frame.contains("Choose model · DeepSeek") && frame.contains("API key saved")
    });
    terminal.assert_dialog("credential-save-return", "Choose model · DeepSeek");
    assert_saved_model(&root);
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
        tickloop_harness_model::credentials::load_api_key(&root.join("state"), TEST_KEY_NAME)
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
        tickloop_harness_model::credentials::load_api_key(&root.join("state"), TEST_KEY_NAME)
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
        tickloop_harness_model::credentials::load_api_key(&root.join("state"), TEST_KEY_NAME)
            .unwrap()
            .as_deref(),
        Some("committed-456-key")
    );
    assert_saved_model(&root);
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
fn directory_and_model_search_filter_rows_and_escape_clears_before_closing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_key(&root, TEST_KEY);
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
    let mark = terminal.mark();
    terminal.send(b"/provider\r");
    terminal.wait_for_frame_after(mark, "directory offers search", |frame| {
        frame.contains("Choose Provider · All") && frame.contains("⌕ Search…")
    });
    let mark = terminal.mark();
    terminal.send(b"/kimi");
    terminal.wait_for_frame_after(mark, "directory filtered to Kimi", |frame| {
        frame.contains("⌕ kimi")
            && frame.contains("Kimi · 0/3 configured")
            && frame.contains("api.kimi.com")
            && !frame.contains("api.deepseek.com")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[B");
    terminal.wait_for_frame_after(mark, "down returns to the searched list", |frame| {
        frame.contains("⌕ kimi") && !frame.contains("esc clear")
    });
    let mark = terminal.mark();
    terminal.send(b"p");
    terminal.wait_for_frame_after(mark, "searched connection details", |frame| {
        frame.contains("Connection · Kimi")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "details return to the searched directory", |frame| {
        frame.contains("Choose Provider · All")
            && frame.contains("⌕ kimi")
            && !frame.contains("api.deepseek.com")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "escape clears the directory search", |frame| {
        frame.contains("Choose Provider · All")
            && frame.contains("⌕ Search…")
            && frame.contains("api.deepseek.com")
    });
    terminal.close_provider_picker();

    let mark = terminal.mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(mark, "models name their connection", |frame| {
        frame.contains("Choose model · Signed-in Providers") && frame.contains("· DeepSeek")
    });
    let mark = terminal.mark();
    terminal.send(b"/zzz");
    terminal.wait_for_frame_after(mark, "model search without a match", |frame| {
        frame.contains("⌕ zzz") && frame.contains("No models match “zzz”")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "escape clears the model search", |frame| {
        frame.contains("⌕ Search…") && frame.contains("· DeepSeek")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "second escape returns to the list", |frame| {
        frame.contains("Choose model") && !frame.contains("esc list")
    });
    terminal.close_model_picker();
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
    assert_saved_model(&root);
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
    terminal.close_provider_picker();
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
fn configured_settings_model_uses_the_input_dialog() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = Terminal::start(&root);
    terminal.wait_for("deepseek-flash");
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

#[test]
fn first_run_without_credentials_opens_provider_setup_without_creating_a_chat() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = Terminal::start_first_run(&root, &[], &[]);
    terminal.wait_for("Choose Provider · All");
    terminal.assert_dialog("first-run-provider-setup", "Choose Provider · All");
    assert!(!root.join("state/settings.json").exists());
    assert!(!root.join("state/.env").exists());
    terminal.assert_no_chats();
    terminal.close_provider_picker();
    terminal.exit();
}

#[test]
fn first_run_with_environment_key_requires_model_confirmation_then_restart_skips_setup() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let environment = [(TEST_KEY_NAME, TEST_KEY)];
    let mut terminal = Terminal::start_first_run(&root, &[], &environment);
    terminal.wait_for("Choose model · Signed-in Providers");
    terminal.assert_dialog(
        "first-run-model-setup",
        "Choose model · Signed-in Providers",
    );
    assert!(!root.join("state/settings.json").exists());
    assert!(!root.join("state/.env").exists());
    terminal.assert_no_chats();
    let mark = terminal.mark();
    terminal.select_row("DeepSeek Flash (deepseek-flash)");
    terminal.wait_for_frame_after(mark, "first model confirmed", |frame| {
        frame.contains("Model selection: Applied")
    });
    assert_saved_model(&root);
    terminal.assert_no_chats();
    terminal.close_provider_picker();
    terminal.exit();
    drop(terminal);

    let mut restarted = Terminal::start_first_run(&root, &[], &environment);
    restarted.wait_for("deepseek-flash");
    let frame = screen::visible_screen(
        &restarted.visible_output(),
        restarted.columns,
        restarted.rows,
    );
    assert!(
        !frame.contains("Choose Provider") && !frame.contains("Choose model"),
        "{frame}"
    );
    restarted.assert_no_chats();
    restarted.exit();
}

#[test]
fn model_selected_for_shell_created_chat_is_saved_and_restart_skips_setup() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let environment = [(TEST_KEY_NAME, TEST_KEY)];
    let mut terminal = Terminal::start_first_run(&root, &[], &environment);
    terminal.wait_for("Choose model · Signed-in Providers");
    terminal.close_model_picker();
    terminal.send(b"!true\r");
    let deadline = Instant::now() + TIMEOUT;
    let shell_chat = loop {
        if let Some(snapshot) = terminal.only_chat_snapshot()
            && snapshot
                .user_shells
                .iter()
                .any(|shell| shell.command == "true" && shell.exit_code == Some(0))
        {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "local Shell did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!root.join("state/settings.json").exists());

    let mark = terminal.mark();
    terminal.send(b"/model\r");
    terminal.wait_for_frame_after(mark, "Shell Chat model setup opened", |frame| {
        frame.contains("Choose model · Signed-in Providers")
    });
    let mark = terminal.mark();
    terminal.select_row("DeepSeek Flash (deepseek-flash)");
    terminal.wait_for_frame_after(mark, "Shell Chat model selection saved", |frame| {
        frame.contains("Model selection: Applied")
    });
    assert_saved_model(&root);
    let snapshot = terminal.only_chat_snapshot().expect("Shell created a Chat");
    assert_eq!(snapshot.execution, shell_chat.execution);
    assert!(snapshot.last_model_run.is_none());
    assert!(snapshot.request_usage.entries().is_empty());
    assert!(snapshot.pending_inputs.is_empty());
    terminal.close_provider_picker();
    terminal.exit();
    drop(terminal);

    let mut restarted = Terminal::start_first_run(&root, &[], &environment);
    restarted.wait_for("deepseek-flash");
    let frame = screen::visible_screen(
        &restarted.visible_output(),
        restarted.columns,
        restarted.rows,
    );
    assert!(
        !frame.contains("Choose Provider") && !frame.contains("Choose model"),
        "{frame}"
    );
    assert_saved_model(&root);
    restarted.exit();
}

#[test]
fn overview_dispatch_requires_setup_even_when_a_historical_chat_is_selected() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let environment = [(TEST_KEY_NAME, TEST_KEY)];
    let mut original = Terminal::start_first_run(&root, &[], &environment);
    original.wait_for("Choose model · Signed-in Providers");
    original.close_model_picker();
    original.send(b"!true\r");
    let deadline = Instant::now() + TIMEOUT;
    let historical_chat = loop {
        if let Some(snapshot) = original.only_chat_snapshot()
            && snapshot
                .user_shells
                .iter()
                .any(|shell| shell.command == "true" && shell.exit_code == Some(0))
        {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "local Shell did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    original.exit();
    // Keep this fixture's daemon alive: the next View resumes an existing Chat,
    // not one registered by that View's first-run setup state.
    let mut terminal = Terminal::start_first_run(&root, &[], &environment);
    terminal.wait_for("Choose model · Signed-in Providers");
    terminal.close_model_picker();
    let mark = terminal.mark();
    terminal.send(b"/resume\r");
    terminal.wait_for_frame_after(mark, "historical Chat picker opened", |frame| {
        frame.contains("Resume Chat")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "historical Chat opened without new work", |frame| {
        frame.contains("History only; selecting this Chat starts no work.")
            && frame.lines().any(|line| line.trim() == "❯")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[D");
    terminal.wait_for_frame_after(mark, "overview with historical Chat selected", |frame| {
        frame.contains("Agents ·") && frame.contains("tab new Agent")
    });
    let mark = terminal.mark();
    terminal.send(b"\t\x1b[200~keep this overview prompt unsent\x1b[201~");
    terminal.wait_for_frame_after(mark, "overview dispatch draft entered", |frame| {
        frame.contains("keep this overview prompt unsent")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(
        mark,
        "overview dispatch requires model selection",
        |frame| frame.contains("Choose model · Signed-in Providers"),
    );
    terminal.assert_dialog("overview-model-setup", "Choose model · Signed-in Providers");
    let snapshot = terminal
        .only_chat_snapshot()
        .expect("historical Shell Chat remains");
    assert_eq!(snapshot.execution, historical_chat.execution);
    assert!(snapshot.last_model_run.is_none());
    assert!(snapshot.request_usage.entries().is_empty());
    assert!(snapshot.pending_inputs.is_empty());
    assert!(!root.join("state/settings.json").exists());
    terminal.close_model_picker();
    let frame = screen::visible_screen(&terminal.visible_output(), terminal.columns, terminal.rows);
    assert!(
        frame.contains("keep this overview prompt unsent"),
        "{frame}"
    );
    terminal.send(b"\x15");
    terminal.exit();
}

#[test]
fn cancelled_first_run_preserves_prompt_and_resubmitting_reopens_setup_without_chat() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = Terminal::start_first_run(&root, &[], &[]);
    terminal.wait_for("Choose Provider · All");
    terminal.close_provider_picker();
    let mark = terminal.mark();
    terminal.send(b"keep this unsent prompt\r");
    terminal.wait_for_frame_after(mark, "submit reopens model setup", |frame| {
        frame.contains("Choose Provider · All")
    });
    terminal.assert_no_chats();
    terminal.close_provider_picker();
    let frame = screen::visible_screen(&terminal.visible_output(), terminal.columns, terminal.rows);
    assert!(frame.contains("keep this unsent prompt"), "{frame}");
    assert!(!root.join("state/settings.json").exists());
    // Clear the preserved prompt before issuing the exit command.
    terminal.send(b"\x15");
    terminal.exit();
}

#[test]
fn shell_before_model_setup_does_not_allow_a_prompt_to_bypass_setup() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = Terminal::start_first_run(&root, &[], &[]);
    terminal.wait_for("Choose Provider · All");
    terminal.close_provider_picker();
    terminal.send(b"!true\r");
    let deadline = Instant::now() + TIMEOUT;
    let shell_chat = loop {
        if let Some(snapshot) = terminal.only_chat_snapshot()
            && snapshot
                .user_shells
                .iter()
                .any(|shell| shell.command == "true" && shell.exit_code == Some(0))
        {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "local Shell did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(shell_chat.last_model_run.is_none());
    assert!(shell_chat.request_usage.entries().is_empty());

    let mark = terminal.mark();
    terminal.send(b"/compact\r");
    terminal.wait_for_frame_after(mark, "compaction also requires model selection", |frame| {
        frame.contains("Choose Provider · All")
    });
    let snapshot = terminal.only_chat_snapshot().expect("Shell created a Chat");
    assert!(snapshot.last_model_run.is_none());
    assert!(snapshot.request_usage.entries().is_empty());
    terminal.close_provider_picker();
    terminal.send(b"\x15");

    let mark = terminal.mark();
    terminal.send(b"do not send this to a model\r");
    terminal.wait_for_frame_after(mark, "Shell Chat remains gated by model setup", |frame| {
        frame.contains("Choose Provider · All")
    });
    let snapshot = terminal.only_chat_snapshot().expect("Shell created a Chat");
    assert_eq!(snapshot.execution, shell_chat.execution);
    assert!(snapshot.last_model_run.is_none());
    assert!(snapshot.request_usage.entries().is_empty());
    assert!(snapshot.pending_inputs.is_empty());
    terminal.close_provider_picker();
    let frame = screen::visible_screen(&terminal.visible_output(), terminal.columns, terminal.rows);
    assert!(frame.contains("do not send this to a model"), "{frame}");
    assert!(!root.join("state/settings.json").exists());
    terminal.send(b"\x15");
    terminal.exit();
}

#[test]
fn branch_of_shell_chat_before_model_setup_still_requires_model_selection() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut terminal = Terminal::start_first_run(&root, &[], &[]);
    terminal.wait_for("Choose Provider · All");
    terminal.close_provider_picker();
    terminal.send(b"!true\r");
    let deadline = Instant::now() + TIMEOUT;
    let source = loop {
        if let Some(snapshot) = terminal.only_chat_snapshot()
            && snapshot
                .user_shells
                .iter()
                .any(|shell| shell.command == "true" && shell.exit_code == Some(0))
        {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "local Shell did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(source.last_model_run.is_none());
    assert!(source.request_usage.entries().is_empty());

    let mark = terminal.mark();
    terminal.send(b"/branch pending-model-setup\r");
    terminal.wait_for_frame_after(mark, "pending setup Chat branched", |frame| {
        frame.contains("Branched conversation")
            && frame.contains("pending-model-setup")
            && frame.contains("Continue Chat?")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(
        mark,
        "branch input restored with receiving paused",
        |frame| !frame.contains("Continue Chat?") && frame.lines().any(|line| line.trim() == "❯"),
    );
    let snapshots = terminal.chat_snapshots();
    assert_eq!(snapshots.len(), 2, "branch must create a second Chat");
    let branch = snapshots
        .iter()
        .find(|snapshot| snapshot.execution != source.execution)
        .expect("branch has its own Chat identity")
        .execution;

    // Branch confirmation is emitted before the input editor is restored. Observe
    // the complete draft in the editor before submitting it to the setup gate.
    let mark = terminal.mark();
    terminal.send(b"\x1b[200~keep this branch prompt unsent\x1b[201~");
    terminal.wait_for_frame_after(mark, "branch prompt ready to submit", |frame| {
        frame.contains("keep this branch prompt unsent") && frame.contains("enter send")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "branch remains gated by model setup", |frame| {
        frame.contains("Choose Provider · All")
    });
    terminal.assert_dialog("branch-model-setup", "Choose Provider · All");
    let snapshots = terminal.chat_snapshots();
    assert_eq!(snapshots.len(), 2);
    assert!(
        snapshots
            .iter()
            .any(|snapshot| snapshot.execution == branch)
    );
    for snapshot in snapshots {
        assert!(snapshot.last_model_run.is_none());
        assert!(snapshot.request_usage.entries().is_empty());
        assert!(snapshot.pending_inputs.is_empty());
    }
    terminal.close_provider_picker();
    let frame = screen::visible_screen(&terminal.visible_output(), terminal.columns, terminal.rows);
    assert!(frame.contains("keep this branch prompt unsent"), "{frame}");
    assert!(!root.join("state/settings.json").exists());
    terminal.send(b"\x15");
    terminal.exit();
}

#[test]
fn saved_key_without_model_also_opens_model_setup() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    save_key(&root, TEST_KEY);
    let mut terminal = Terminal::start_first_run(&root, &[], &[]);
    terminal.wait_for("Choose model · Signed-in Providers");
    terminal.wait_for("API key saved");
    assert!(!root.join("state/settings.json").exists());
    terminal.assert_no_chats();
    terminal.close_provider_picker();
    terminal.exit();
}
