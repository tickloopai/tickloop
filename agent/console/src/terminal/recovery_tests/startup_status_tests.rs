use super::*;

const STARTUP_FIXTURE: &str = "TICKLOOP_STARTUP_STATUS_FIXTURE";
const STATUS_ROWS: &str = "TICKLOOP_STARTUP_STATUS_ROWS";
const STARTUP_MODEL: &str = "deepseek-flash";
const EXTERNAL_MODEL: &str = "external-fixture";

fn startup_config(model: &str) -> ModelConfig {
    if model == STARTUP_MODEL {
        return tickloop_agent_model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap();
    }
    let mut config = tickloop_agent_test_support::config();
    config.model = model.into();
    config.name = model.into();
    config.protocol = Protocol::ChatCompletions;
    config
}

#[test]
#[ignore = "owned PTY producer for New Agent startup status"]
fn startup_status_pty_fixture() {
    if std::env::var(STARTUP_FIXTURE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("startup-status-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let config = startup_config(STARTUP_MODEL);
                let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new()));
                let core =
                    tickloop_agent_runtime::Agent::new_embedded(config.clone(), model.clone());
                core.hold_goal_continuations_for_tests();
                let access = Arc::new(EmbeddedAgentAccess::new(core.clone()));
                let workdir = std::env::current_dir().unwrap();
                let mut launch = LaunchSpec::embedded(workdir.clone());
                launch.model = Some(config.clone());
                launch.host_options.execution_mode_override = Some(ExecutionMode::Auto);
                launch.session_persistence = false;
                let agent = Agent::from_access_for_test(
                    access.clone(),
                    access.daemon_epoch(),
                    ViewRef {
                        session_id: Uuid::new_v4(),
                        view_id: Uuid::new_v4(),
                    },
                    launch,
                    None,
                );
                let (console, input) = Console::start(true).unwrap();
                let status_lines = match std::env::var(STATUS_ROWS).unwrap().as_str() {
                    "default" => None,
                    "0" => Some(Vec::new()),
                    "1" => Some(vec!["configured first".into()]),
                    "3" => Some(vec![
                        "configured first".into(),
                        "configured second".into(),
                        "configured third".into(),
                    ]),
                    rows => panic!("unknown status rows {rows}"),
                };
                console.set_status_lines(status_lines).unwrap();
                console.set_input_context(None, &workdir, &[]).unwrap();
                initialize_startup_console(
                    &console,
                    Some(&config),
                    &workdir,
                    ExecutionMode::Auto,
                    Vec::new(),
                )
                .unwrap();
                interact(
                    &agent,
                    workdir,
                    &console,
                    input,
                    StartupState {
                        model_selected: true,
                        launch_arguments: Vec::new(),
                        chat: StartupChat::New,
                        user_settings: None,
                    },
                    None,
                )
                .await
                .unwrap();
                assert!(model.requests.lock().unwrap().is_empty());
                core.shutdown().await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn start_startup_fixture(rows: &str) -> (RecoveryPty, tempfile::TempDir) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::write(
        directory.path().join("model.json"),
        serde_json::to_vec(&startup_config(EXTERNAL_MODEL)).unwrap(),
    )
    .unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--ignored",
        "--exact",
        "terminal::recovery_tests::startup_status_tests::startup_status_pty_fixture",
        "--nocapture",
    ]);
    command.env(STARTUP_FIXTURE, "1");
    command.env(STATUS_ROWS, rows);
    command.env("TICKLOOP_AGENT_TUI", "fullscreen");
    command.env("TERM", "xterm-256color");
    command.env("HOME", directory.path());
    command.env("TICKLOOP_TMPDIR", directory.path());
    command.cwd(directory.path());
    let child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            captured.lock().unwrap().extend_from_slice(&bytes[..count]);
        }
    });
    (
        RecoveryPty {
            child,
            writer,
            output,
            _master: pair.master,
            timeout: RECOVERY_TIMEOUT,
        },
        directory,
    )
}

fn assert_clean_startup_status(frame: &str, rows: &str, model: &str) {
    assert!(!frame.contains("Ready"), "{rows}: {frame}");
    if rows == "default" {
        let status = frame
            .lines()
            .find(|line| line.contains("⏵⏵ Auto"))
            .unwrap_or_else(|| panic!("missing core status: {frame}"));
        let label = if model == STARTUP_MODEL {
            "DeepSeek Flash"
        } else {
            model
        };
        assert_eq!(status.matches(label).count(), 1, "{status}");
        assert!(status.contains("Context 0% used"), "{status}");
        assert!(!status.contains("ChatCompletions"), "{status}");
    }
    if matches!(rows, "1" | "3") {
        assert!(frame.contains("configured first"), "{rows}: {frame}");
    }
    if rows == "3" {
        assert!(frame.contains("configured second"), "{frame}");
        assert!(frame.contains("configured third"), "{frame}");
    }
}

#[test]
fn new_agent_startup_has_one_model_and_no_ordinary_ready_status() {
    let (mut fixture, _directory) = start_startup_fixture("default");
    fixture.wait_for("Tickloop");
    let frame = fixture.repaint_after_recovery("New Agent startup", |frame| {
        frame.contains("Context 0% used")
    });
    assert_clean_startup_status(&frame, "default", STARTUP_MODEL);
}

#[test]
fn startup_status_configuration_help_and_removed_model_file_notice_survive_refresh() {
    for rows in ["default", "0", "1", "3"] {
        let (mut fixture, _directory) = start_startup_fixture(rows);
        fixture.wait_for("Tickloop");
        let frame = fixture
            .repaint_after_recovery("configured startup", |frame| frame.contains("Tickloop"));
        assert_clean_startup_status(&frame, rows, STARTUP_MODEL);
        fixture.send(b"?");
        fixture.wait_for("esc / ? close help");
        let help = fixture
            .repaint_after_recovery("startup help", |frame| frame.contains("esc / ? close help"));
        assert!(!help.contains("Ready"), "{rows}: {help}");
        fixture.send(b"?");
        fixture.send(b"/model model.json\r");
        fixture.wait_for("Model files are not supported");
        std::thread::sleep(Duration::from_millis(1100));
        let selected = fixture
            .repaint_after_recovery("removed model file notice refresh", |frame| {
                frame.contains("Model files are not supported")
            });
        assert_clean_startup_status(&selected, rows, STARTUP_MODEL);
        assert!(
            selected.contains("Model files are not supported"),
            "{rows}: {selected}"
        );
        assert!(!selected.contains("Model selection: Applied"));
    }
}

#[test]
fn new_chat_goal_only_monitor_opens_with_right_arrow() {
    let (mut fixture, _directory) = start_startup_fixture("default");
    fixture.wait_for("Tickloop");
    fixture.send(b"/goal Verify the release and record acceptance evidence\r");
    fixture.wait_for("Goal set");
    fixture.send(b"/goal resume\r");
    fixture.wait_for("Goal is already active.");
    let refusal = fixture.repaint_after_recovery("Goal refusal", |frame| {
        frame.contains("Goal is already active.")
    });
    assert!(!refusal.contains("invalid request:"), "{refusal}");
    assert!(!refusal.contains("Cannot manage Goal:"), "{refusal}");
    fixture.send(b"\x1b[C");
    fixture.wait_for("enter Goal details");
    fixture.send(b"\r");
    fixture.wait_for("Adjust token budget");
}
