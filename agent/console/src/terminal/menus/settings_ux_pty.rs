use super::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tickloop_agent_daemon::EmbeddedAgentAccess;
#[cfg(test)]
use tickloop_agent_settings::local_agent::LocalAgent;

use tickloop_agent_protocol::{
    AccessError, AgentAccess, AgentDetailQuery, CommandEnvelope, CommandReceipt, Cursor,
    DisplayReceipt, DisplayUpdate, LaunchSpec, Observation, ObservationRequest, ObservationValue,
    ViewRef,
};
use tickloop_agent_test_support::screen;

const DIRECTORY: &str = "TICKLOOP_SETTINGS_UX_FIXTURE_DIR";
const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
#[ignore = "owned PTY producer for settings and Provider navigation"]
fn settings_ux_pty_fixture() {
    thread::Builder::new().stack_size(8 * 1024 * 1024).spawn(|| {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let directory = PathBuf::from(std::env::var_os(DIRECTORY).unwrap()).canonicalize().unwrap();
            let root = directory.join("config");
            tickloop_agent_model::credentials::save_api_key(&root, "DEEPSEEK_API_KEY", "fixture-key").unwrap();
            tickloop_agent_model::credentials::save_api_key(&root, "MINIMAX_GLOBAL_API_KEY", "fixture-minimax-key").unwrap();
            let discovery = std::env::var("TICKLOOP_SETTINGS_UX_MODE").as_deref() == Ok("discovery");
            if discovery { tickloop_agent_model::credentials::save_api_key(&root, "ANTHROPIC_API_KEY", "fixture-anthropic-key").unwrap(); }
            let model = if discovery {
                tickloop_agent_model::cards::ModelCatalog::defaults().get("anthropic-api", "claude-opus-5").unwrap()
            } else { tickloop_agent_model::cards::ModelCatalog::defaults().get("deepseek-api", "deepseek-flash").unwrap() };
            let settings = UserSettings::in_root(&root);
            let core = tickloop_agent_runtime::Agent::new_with_user_settings_embedded(model.clone(), Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())), settings.clone());
            let embedded = EmbeddedAgentAccess::new(core.clone());
            let epoch = embedded.daemon_epoch();
            let access = Arc::new(FixtureAccess { inner: embedded, directory: directory.clone(), epoch });
            let mut launch = LaunchSpec::embedded(directory.clone());
            launch.model = Some(model);
            launch.config_root = if std::env::var("TICKLOOP_SETTINGS_UX_MODE").as_deref() == Ok("rootless") { None } else { Some(root.clone()) };
            launch.session_persistence = false;
            let agent = Agent::from_access_for_test(access.clone(), epoch, ViewRef { session_id: Uuid::new_v4(), view_id: Uuid::new_v4() }, launch, None);
            agent.use_user_settings(Some(settings.clone()));
            let chat = agent.create_chat(directory.clone()).await.unwrap();
            let (console, mut input) = Console::start(true).unwrap();
            console.select_chat(Some(chat)).unwrap();
            console.set_prompt_enabled(true).unwrap();
            let mut menus = Menus::new(Some(settings), Some(root));
            std::fs::write(directory.join("ready"), "ready").unwrap();
            let mut login: Option<super::super::login::LoginFlow> = None;
            let mut login_sender = None;
            let mut login_succeeded = false;
            let mut login_reported = false;
            let mut poll = tokio::time::interval(Duration::from_millis(20));
            loop {
                let discovery_active = menus.discovery_active();
                let search_test_active = menus.search_test_active();
                let login_updates_pending = login.as_ref().is_some_and(super::super::login::LoginFlow::updates_pending);
                let event = tokio::select! {
                    _ = poll.tick() => {
                        if directory.join("release-login").exists() && !login_reported
                            && let Some(sender) = &login_sender
                        {
                            let sender: &tokio::sync::mpsc::UnboundedSender<super::super::login::LoginUpdate> = sender;
                            sender.send(super::super::login::LoginUpdate::Finished(Ok("Fixture account".into()))).unwrap();
                            login_reported = true;
                        }
                        continue;
                    }
                    update = async { login.as_mut().unwrap().next().await }, if login_updates_pending => {
                        let Some(super::super::login::LoginUpdate::Finished(result)) = update else { panic!("unexpected fixture login update"); };
                        login_succeeded = result.is_ok();
                        super::super::login::complete_provider_login(login.as_mut().unwrap(), &console, &result).unwrap();
                        std::fs::write(directory.join("login-finished"), "finished").unwrap();
                        continue;
                    }
                    result = menus.next_provider_update(), if discovery_active || search_test_active => {
                        match result {
                            ProviderUpdate::Models(result) => {
                                menus.finish_discovery(&agent, &console, result).await.unwrap();
                                std::fs::write(directory.join("discovery-finished"), "finished").unwrap();
                            },
                            ProviderUpdate::Search(result) => menus.finish_search_test(&console, result).unwrap(),
                        }
                        continue;
                    }
                    event = input.recv() => event.unwrap(),
                };
                writeln!(std::fs::OpenOptions::new().create(true).append(true).open(directory.join("trace")).unwrap(), "{event:?}").unwrap();
                match event {
                    super::super::console::InputEvent::Line(line) | super::super::console::InputEvent::Prompt { text: line, .. } => match line.as_str() {
                        "/settings" => menus.settings(&agent, &console, ConfigTarget::Agent(chat)).await.unwrap(),
                        "/websearch" => menus.search_settings(&agent, &console, ConfigTarget::Agent(chat), false).await.unwrap(),
                        "/model" => menus.models(&agent, &console, ConfigTarget::Agent(chat)).await.unwrap(),
                        "/provider" => menus.providers(&agent, &console, ConfigTarget::Agent(chat)).await.unwrap(),
                        "/exit" => break,
                        _ => panic!("unexpected fixture line {line:?}"),
                    },
                    super::super::console::InputEvent::Choice { menu_id, index, comment } => {
                        if login.as_ref().is_some_and(|flow| flow.id() == menu_id) {
                            login.take().unwrap().close(&console).unwrap();
                            menus.resume_provider_login(&agent, &console, login_succeeded).await.unwrap();
                            continue;
                        }
                        match menus.choose(&agent, &console, menu_id, Some(index), comment).await {
                            Ok(outcome) => if let Some(method) = outcome.provider_login {
                                let (flow, sender) = super::super::login::LoginFlow::start_fixture(&console, method).unwrap();
                                login = Some(flow);
                                login_sender = Some(sender);
                            },
                            Err(error) => { std::fs::write(directory.join("selection-error"), &error).unwrap(); console.set_status(&error).unwrap(); },
                        }
                    }
                    super::super::console::InputEvent::Cancel { menu_id } => {
                        if login.as_ref().is_some_and(|flow| flow.id() == menu_id) {
                            login.take().unwrap().close(&console).unwrap();
                            menus.resume_provider_login(&agent, &console, login_succeeded).await.unwrap();
                            continue;
                        }
                        menus.choose(&agent, &console, menu_id, None, None).await.unwrap();
                    }
                    super::super::console::InputEvent::PickerAction { menu_id, index, action } => {
                        menus.picker_action(&agent, &console, menu_id, index, action).await.unwrap();
                    }
                    super::super::console::InputEvent::Eof => break,
                    super::super::console::InputEvent::Error(error) => panic!("{error}"),
                    _ => {}
                }
            }
            drop(console);
            core.shutdown().await;
        });
    }).unwrap().join().unwrap();
}

struct FixtureAccess {
    inner: EmbeddedAgentAccess,
    directory: PathBuf,
    epoch: Uuid,
}

#[async_trait::async_trait]
impl AgentAccess for FixtureAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if let ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::AccountModels { config, .. },
            ..
        } = &query
        {
            std::fs::write(
                self.directory.join("discovery-started"),
                &config.provider_id,
            )
            .unwrap();
            while !self.directory.join("release-discovery").exists() {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            let models = tickloop_agent_model::cards::ModelCatalog::defaults()
                .profiles()
                .into_iter()
                .filter(|model| model.provider_id == config.provider_id)
                .map(|model| {
                    (
                        model.model,
                        tickloop_agent_protocol::ModelAvailability::DirectoryListed,
                    )
                })
                .collect();
            return Ok(Observation::single(
                ObservationValue::AccountModels(Ok(
                    tickloop_agent_protocol::AccountModelDirectory {
                        models,
                        semantics: tickloop_agent_protocol::DirectorySemantics::Directory,
                        complete: true,
                    },
                )),
                Cursor {
                    daemon_epoch: self.epoch,
                    execution: Some(*execution),
                    sequence: 0,
                },
                None,
            ));
        }
        if let ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::TestSearch { .. },
            ..
        } = &query
        {
            let cursor = Cursor {
                daemon_epoch: self.epoch,
                execution: Some(*execution),
                sequence: 0,
            };
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            let directory = self.directory.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let status = if directory.join("reject-search").exists() {
                    tickloop_agent_protocol::SearchTestStatus::Unavailable
                } else {
                    tickloop_agent_protocol::SearchTestStatus::Available
                };
                let _ = sender
                    .send(tickloop_agent_protocol::ObservationUpdate {
                        cursor,
                        change: tickloop_agent_protocol::ObservationChange::SearchTest(
                            tickloop_agent_protocol::SearchTestResult {
                                status,
                                detail: "Controlled fixture search test".into(),
                            },
                        ),
                    })
                    .await;
            });
            return Ok(Observation::new(
                ObservationValue::SearchTestPending,
                cursor,
                None,
                Some(receiver),
                None,
            ));
        }
        self.inner.observe(query).await
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.inner.execute(command).await
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.inner.show_view(update).await
    }
}

struct Fixture {
    _runtime_directories: crate::terminal::test_runtime::RuntimeDirectories,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    directory: tempfile::TempDir,
    height: usize,
    timeout: Duration,
}

impl Fixture {
    fn start() -> Self {
        Self::start_with_mode("ordinary")
    }

    fn start_with_mode(mode: &str) -> Self {
        Self::start_at(mode, 24)
    }

    fn start_at(mode: &str, height: u16) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let runtime_directories = crate::terminal::test_runtime::RuntimeDirectories::of_config_root(
            &directory.path().join("config"),
        );
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: height,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--ignored",
            "--exact",
            "terminal::menus::settings_ux_pty::settings_ux_pty_fixture",
            "--nocapture",
        ]);
        command.env_clear();
        command.env(DIRECTORY, directory.path());
        command.env("TICKLOOP_SETTINGS_UX_MODE", mode);
        command.env("HOME", directory.path());
        command.env("TICKLOOP_CONFIG_DIR", directory.path().join("config"));
        command.env("TICKLOOP_TMPDIR", directory.path());
        command.env("TICKLOOP_AGENT_TUI", "fullscreen");
        command.env("TERM", "xterm-256color");
        command.cwd(directory.path());
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = output.clone();
        thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                captured.lock().unwrap().extend_from_slice(&bytes[..count]);
            }
        });
        let fixture = Self {
            _runtime_directories: runtime_directories,
            child,
            writer,
            output,
            _master: pair.master,
            directory,
            height: height as usize,
            timeout: TIMEOUT,
        };
        fixture
            .wait(|frame| frame.contains("❯") && fixture.directory.path().join("ready").exists());
        fixture
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// Waits for a settled frame: keys are ignored while a submitted choice is still being
    /// applied, so a frame showing that state never satisfies a wait.
    fn wait(&self, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + self.timeout;
        loop {
            let mut frame = screen::visible_screen(
                &String::from_utf8_lossy(&self.output.lock().unwrap()),
                100,
                self.height,
            );
            assert!(
                Instant::now() < deadline,
                "PTY did not reach expected settings state:\n{frame}\nEvents: {}",
                std::fs::read_to_string(self.directory.path().join("trace")).unwrap_or_default()
            );
            if predicate(&frame)
                && !frame.contains(super::super::console::MENU_SUBMISSION_PENDING_HINT)
            {
                thread::sleep(Duration::from_millis(30));
                frame = screen::visible_screen(
                    &String::from_utf8_lossy(&self.output.lock().unwrap()),
                    100,
                    self.height,
                );
                if !predicate(&frame)
                    || frame.contains(super::super::console::MENU_SUBMISSION_PENDING_HINT)
                {
                    continue;
                }
                if let Some(root) = std::env::var_os("TICKLOOP_SETTINGS_UX_EVIDENCE_DIR") {
                    static FRAME: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let root = PathBuf::from(root);
                    std::fs::create_dir_all(&root).unwrap();
                    let name = thread::current()
                        .name()
                        .unwrap_or("settings-ux")
                        .replace(':', "_");
                    let number = FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    std::fs::write(root.join(format!("{name}-{number:04}.txt")), &frame).unwrap();
                }
                return frame;
            }
            thread::sleep(Duration::from_millis(15));
        }
    }

    fn select(&mut self, label: &str) {
        for _ in 0..40 {
            let frame = self.wait(|frame| frame.lines().any(|line| line.starts_with('❯')));
            if frame
                .lines()
                .any(|line| line.starts_with('❯') && line.contains(label))
            {
                return;
            }
            self.send(b"j");
            self.wait(|next| next != frame);
        }
        panic!("option {label} was unreachable");
    }
}

#[test]
fn settings_ux_pty_unstable_frames_cannot_extend_the_wait_deadline() {
    let mut fixture = Fixture::start();
    fixture.timeout = Duration::from_millis(150);
    let started = Instant::now();
    let calls = std::cell::Cell::new(0);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.wait(|_| {
            calls.set(calls.get() + 1);
            started.elapsed() < Duration::from_millis(700) && calls.get() % 2 == 1
        });
    }));
    assert!(failure.is_err(), "unstable frames must time out");
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "stability retries extended the deadline"
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn settings_ux_pty_selections_close_and_toggles_stay_open() {
    let mut fixture = Fixture::start();
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.contains("Settings"));
    fixture.send(b"jj\r");
    fixture.wait(|frame| frame.lines().any(|line| line.trim() == "Effort"));
    fixture.send(b"j\r");
    fixture.wait(|frame| {
        !frame.lines().any(|line| line == "Settings")
            && !frame.lines().any(|line| line.trim() == "Effort")
    });
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.select("Execution mode");
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Choose execution mode"));
    fixture.send(b"j\r");
    fixture.wait(|frame| {
        !frame.lines().any(|line| line == "Settings") && !frame.contains("Choose execution mode")
    });
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.select("Auto-scroll");
    fixture.send(b"\r");
    fixture.wait(|frame| {
        frame
            .lines()
            .any(|line| line.starts_with('❯') && line.contains("Auto-scroll: Off"))
    });
    fixture.select("Reduce motion");
    fixture.send(b"\r");
    fixture.wait(|frame| {
        frame
            .lines()
            .any(|line| line.starts_with('❯') && line.contains("Reduce motion: On"))
    });
    assert!(
        UserSettings::in_root(&fixture.directory.path().join("config"))
            .display_preferences()
            .prefers_reduced_motion
    );
}

#[test]
fn settings_ux_pty_model_groups_signed_in_providers_and_switches_atomically() {
    let mut fixture = Fixture::start();
    fixture.send(b"/model\r");
    let frame = fixture.wait(|frame| frame.contains("Choose model"));
    assert!(
        frame.contains("● DeepSeek"),
        "Current model must remain marked: {frame}"
    );
    fixture.select("MiniMax-M3");
    let frame = fixture.wait(|frame| {
        frame
            .lines()
            .any(|line| line.starts_with('❯') && line.contains("MiniMax-M3"))
    });
    assert!(
        frame.contains("MiniMax Global API"),
        "Provider header must accompany its models: {frame}"
    );
    assert!(
        !frame.contains("Setup required"),
        "Unsigned Providers belong to /provider: {frame}"
    );
    fixture.send(b"\r");
    fixture.wait(|frame| !frame.contains("Choose model") && !frame.contains("Choose Provider"));
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture.directory.path().join("config/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved["defaultModel"],
        serde_json::json!({"provider":"minimax-global-api", "model":"MiniMax-M3"})
    );
}

#[test]
fn settings_ux_pty_settings_escape_returns_one_page_and_omits_numbers() {
    let mut fixture = Fixture::start();
    for (label, title) in [
        ("Effort: auto", "Effort levels depend"),
        ("Execution mode", "Choose execution mode"),
        ("Session status", "Session status"),
        ("Usage", "Usage"),
        ("Theme:", "Choose theme"),
        ("Output style:", "Preferred output style"),
        ("Artifacts:", "Sign out of Tickloop Accounts?"),
    ] {
        fixture.send(b"/settings\r");
        let frame = fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
        assert!(
            !frame.contains("1. Model") && !frame.contains("2. Thinking"),
            "Settings ordinals are noise: {frame}"
        );
        fixture.select(label);
        fixture.send(b"\r");
        fixture.wait(|frame| {
            frame.lines().any(|line| line.contains(title))
                && !frame.lines().any(|line| line == "Settings")
        });
        fixture.send(b"\x1b");
        fixture.wait(|frame| {
            frame.lines().any(|line| line == "Settings")
                && frame
                    .lines()
                    .any(|line| line.starts_with('❯') && line.contains(label))
        });
        fixture.send(b"\x1b");
        fixture.wait(|frame| {
            !frame.lines().any(|line| line == "Settings") && frame.contains("ctrl+g editor")
        });
    }
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.send(b"3j");
    fixture.wait(|frame| {
        frame.lines().any(|line| line == "Settings")
            && frame
                .lines()
                .any(|line| line.starts_with('❯') && line.contains("Thinking:"))
    });
}

#[test]
fn settings_ux_pty_failed_save_keeps_the_setting_focused() {
    let mut fixture = Fixture::start();
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.select("Reduce motion");
    std::fs::write(
        fixture.directory.path().join("config/settings.json"),
        "invalid JSON",
    )
    .unwrap();
    fixture.send(b"\r");
    fixture.wait(|frame| {
        frame.lines().any(|line| line == "Settings")
            && frame
                .lines()
                .any(|line| line.starts_with('❯') && line.contains("Reduce motion: Off"))
    });
    fixture.send(b"j");
    fixture.wait(|frame| {
        frame
            .lines()
            .any(|line| line.starts_with('❯') && line.contains("Output style:"))
    });
}

#[test]
fn settings_ux_pty_discovery_preserves_action_focus_and_settings_parent() {
    let mut fixture = Fixture::start_with_mode("discovery");
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.send(b"\r");
    fixture.wait(|frame| {
        frame.contains("Choose model")
            && fixture.directory.path().join("discovery-started").exists()
    });
    fixture.select("Connection details");
    std::fs::write(
        fixture.directory.path().join("release-discovery"),
        "release",
    )
    .unwrap();
    fixture.wait(|frame| {
        fixture.directory.path().join("discovery-finished").exists()
            && frame.contains("Choose model")
            && frame
                .lines()
                .any(|line| line.starts_with('❯') && line.contains("Connection details"))
    });
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Connection ·"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| {
        frame.contains("Choose model")
            && frame
                .lines()
                .any(|line| line.starts_with('❯') && line.contains("Connection details"))
    });
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("ctrl+g editor") && !frame.contains("Choose model"));
}

#[test]
fn settings_ux_pty_provider_credential_escape_leaves_field_then_pages() {
    let mut fixture = Fixture::start();
    fixture.send(b"/provider\r");
    fixture.wait(|frame| frame.contains("Choose Provider · All"));
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Choose model · DeepSeek"));
    fixture.send(b"p");
    fixture.wait(|frame| frame.contains("Connection · DeepSeek"));
    fixture.send(b"j\r");
    fixture.wait(|frame| frame.contains("API key · DeepSeek") && frame.contains("esc close field"));
    fixture.send(b"candidate123");
    fixture.wait(|frame| frame.contains("****") && !frame.contains("candidate123"));
    fixture.send(b"\x1b");
    let frame = fixture
        .wait(|frame| frame.contains("API key · DeepSeek") && frame.contains("tab edit API key"));
    assert!(!frame.contains("1. Save API key"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Connection · DeepSeek"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Choose model · DeepSeek"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Choose Provider · All"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| !frame.contains("Choose Provider") && frame.contains("ctrl+g editor"));
    assert_eq!(
        tickloop_agent_model::credentials::load_api_key(
            &fixture
                .directory
                .path()
                .join("config")
                .canonicalize()
                .unwrap(),
            "DEEPSEEK_API_KEY"
        )
        .unwrap()
        .as_deref(),
        Some("fixture-key")
    );
}

#[test]
fn settings_ux_pty_login_completion_stays_open_until_enter_or_escape() {
    let mut fixture = Fixture::start();
    fixture.send(b"/provider\r");
    fixture.wait(|frame| frame.contains("Choose Provider · All"));
    fixture.select("Codex subscription");
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Sign in · OpenAI") && frame.contains("Device code"));
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Log in to Codex subscription"));
    std::fs::write(fixture.directory.path().join("release-login"), "release").unwrap();
    fixture.wait(|frame| {
        fixture.directory.path().join("login-finished").exists()
            && frame.contains("Log in to Codex subscription")
            && frame.contains("Login saved")
    });
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Sign in · OpenAI") && frame.contains("Device code"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Choose Provider · All"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("ctrl+g editor") && !frame.contains("Choose Provider"));
}

#[test]
fn settings_ux_pty_failed_provider_entry_does_not_add_an_escape_layer() {
    let mut fixture = Fixture::start_with_mode("rootless");
    fixture.send(b"/provider\r");
    fixture.wait(|frame| frame.contains("Choose Provider · All"));
    fixture.send(b"\r");
    fixture.wait(|frame| {
        frame.contains("Choose Provider · All")
            && fixture.directory.path().join("selection-error").exists()
    });
    fixture.send(b"\x1b");
    fixture.wait(|frame| {
        !frame.contains("Choose Provider") && frame.lines().any(|line| line.starts_with('❯'))
    });
}

#[test]
fn settings_ux_pty_manage_providers_hint_matches_model_parent() {
    let mut fixture = Fixture::start();
    for from_settings in [false, true] {
        fixture.send(if from_settings {
            b"/settings\r"
        } else {
            b"/model\r"
        });
        if from_settings {
            fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
            fixture.send(b"\r");
        }
        fixture.wait(|frame| frame.contains("Choose model"));
        fixture.select("Manage Providers");
        fixture.send(b"\r");
        let frame = fixture.wait(|frame| frame.contains("Choose Provider"));
        assert!(
            frame.contains("esc back to models") && !frame.contains("esc close"),
            "Manage Providers hint: {frame}"
        );
        fixture.send(b"\x1b");
        fixture.wait(|frame| frame.contains("Choose model"));
        fixture.send(b"\x1b");
        if from_settings {
            fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
            fixture.send(b"\x1b");
        }
        fixture.wait(|frame| frame.contains("ctrl+g editor") && !frame.contains("Choose model"));
    }
    fixture.send(b"/provider\r");
    let frame = fixture.wait(|frame| frame.contains("Choose Provider"));
    assert!(frame.contains("esc close") && !frame.contains("esc back to models"));
    fixture.select("Codex subscription");
    fixture.send(b"\r");
    let frame = fixture.wait(|frame| frame.contains("Sign in · OpenAI"));
    assert!(
        frame.contains("esc back to Providers"),
        "First-setup methods footer: {frame}"
    );
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Choose Provider"));
    fixture.send(b"p");
    fixture.wait(|frame| frame.contains("Connection · OpenAI"));
    fixture.select("Sign in");
    fixture.send(b"\r");
    let frame = fixture.wait(|frame| frame.contains("Sign in · OpenAI"));
    assert!(
        frame.contains("esc back to Connection"),
        "Connection methods footer: {frame}"
    );
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Connection · OpenAI"));
}

#[test]
fn settings_scroll_pty_survives_a_successful_setting_change() {
    let mut fixture = Fixture::start_at("ordinary", 14);
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.select("Reduce motion");
    let before = fixture.wait(|frame| frame.contains("Reduce motion: Off"));
    fixture.send(b"\x1b[6~");
    let scrolled = fixture.wait(|frame| frame != before);
    let rows: Vec<_> = scrolled.lines().collect();
    let title = rows.iter().position(|line| *line == "Settings").unwrap();
    let context: Vec<_> = rows[title + 1..title + 3]
        .iter()
        .map(|line| line.to_string())
        .collect();
    fixture.send(b"\r");
    let changed = fixture.wait(|frame| frame.contains("Reduce motion: On"));
    let rows: Vec<_> = changed.lines().collect();
    let title = rows.iter().position(|line| *line == "Settings").unwrap();
    assert_eq!(
        &rows[title + 1..title + 3],
        context,
        "Successful save reset the Settings context viewport"
    );
}

#[test]
fn settings_scroll_pty_restores_parent_context_after_child_cancel_and_confirm_closes() {
    let mut fixture = Fixture::start_at("ordinary", 14);
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.select("Theme:");
    let before = fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.send(b"\x1b[6~");
    let scrolled = fixture.wait(|frame| frame != before);
    let rows: Vec<_> = scrolled.lines().collect();
    let title = rows.iter().position(|line| *line == "Settings").unwrap();
    let context: Vec<_> = rows[title + 1..title + 3]
        .iter()
        .map(|line| line.to_string())
        .collect();
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Choose theme"));
    fixture.send(b"\x1b");
    {
        let returned = fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
        let rows: Vec<_> = returned.lines().collect();
        let title = rows.iter().position(|line| *line == "Settings").unwrap();
        assert_eq!(
            &rows[title + 1..title + 3],
            context,
            "Child return reset parent explanation scroll"
        );
        assert!(
            returned
                .lines()
                .any(|line| line.starts_with('❯') && line.contains("Theme:"))
        );
    }
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Choose theme"));
    fixture.send(b"j\r");
    fixture.wait(|frame| {
        !frame.lines().any(|line| line == "Settings") && !frame.contains("Choose theme")
    });
}

#[test]
fn search_pty_key_entry_is_masked_and_cancel_does_not_switch_sources() {
    let mut fixture = Fixture::start();
    fixture.send(b"/websearch\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Web search"));
    fixture.wait(|frame| frame.contains("DuckDuckGo · Free") && !frame.contains("no key needed"));
    fixture.select("Tavily");
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("API key · Tavily"));
    fixture.send(b"discarded-search-secret");
    fixture.wait(|frame| frame.contains("****") && !frame.contains("discarded-search-secret"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("API key · Tavily") && frame.contains("tab edit API key"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Web search · Tavily"));
    let root = fixture
        .directory
        .path()
        .canonicalize()
        .unwrap()
        .join("config");
    assert!(
        tickloop_agent_model::credentials::load_api_key(&root, "TAVILY_API_KEY")
            .unwrap()
            .is_none()
    );
    fixture.select("Set or replace API key");
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("API key · Tavily"));
    fixture.send(b"fixture-search-secret");
    let frame = fixture.wait(|frame| frame.contains("Save API key"));
    assert!(!frame.contains("fixture-search-secret"));
    fixture.send(b"\r");
    fixture.wait(|frame| {
        frame.contains("Web search · Tavily")
            && frame.contains("Use this search provider")
            && !frame.contains("(checking…)")
            && !frame.contains("(unavailable)")
    });
    let root = fixture
        .directory
        .path()
        .canonicalize()
        .unwrap()
        .join("config");
    assert_eq!(
        tickloop_agent_model::credentials::load_api_key(&root, "TAVILY_API_KEY")
            .unwrap()
            .as_deref(),
        Some("fixture-search-secret")
    );
    assert_eq!(
        UserSettings::in_root(&root).load().search_config(),
        tickloop_agent_protocol::SearchConfig::enabled_default()
    );
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.lines().any(|line| line == "Web search"));
    fixture.select("Tavily");
    fixture.send(b"\r");
    fixture.wait(|frame| {
        frame.contains("Web search · Tavily")
            && !frame.contains("(checking…)")
            && !frame.contains("(unavailable)")
    });
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Tavily · key saved · active"));
    assert_eq!(
        UserSettings::in_root(&root)
            .load()
            .search_config()
            .unwrap()
            .provider,
        tickloop_agent_protocol::SearchProviderKind::Tavily
    );
    std::fs::write(fixture.directory.path().join("reject-search"), "reject").unwrap();
    fixture.select("Tavily");
    fixture.send(b"\r");
    let frame = fixture.wait(|frame| frame.contains("Use this search provider (unavailable)"));
    assert!(frame.contains("Website: https://www.tavily.com/"));
    assert!(!frame.contains("Test search connection"));
    assert!(!frame.contains("Connection:") && !frame.contains("Credential:"));
    fixture.send(b"\r");
    fixture.wait(|frame| {
        frame.contains("Web search · Tavily")
            && frame.contains("Use this search provider (unavailable)")
    });
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Tavily · key saved · unavailable · active"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| !frame.lines().any(|line| line == "Web search"));
    fixture.send(b"/settings\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    fixture.select("Web search:");
    fixture.send(b"\r");
    fixture.wait(|frame| frame.lines().any(|line| line == "Web search"));
    fixture.select("Off");
    fixture.send(b"\r");
    fixture.wait(|frame| frame.contains("Off · active"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.lines().any(|line| line == "Settings"));
    assert_eq!(UserSettings::in_root(&root).load().search_config(), None);
}
