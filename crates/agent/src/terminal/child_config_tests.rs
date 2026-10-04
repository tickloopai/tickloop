//! Run-loop regressions for a real delegated child opened in place: its fixed authorization is
//! refused before anything is sent, and its permission requests name it with the same token as
//! its family row.
use super::*;
use crate::{agent_access::*, runtime};
use async_trait::async_trait;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

mod screen {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/screen.rs"
    ));
}
mod input_dialog {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/input_dialog.rs"
    ));
}

const FIXTURE_VARIABLE: &str = "TICKLOOP_CHILD_CONFIG_FIXTURE";
const DIRECTORY_VARIABLE: &str = "TICKLOOP_CHILD_CONFIG_DIRECTORY";
const ROUTE_VARIABLE: &str = "TICKLOOP_CHILD_CONFIG_ROUTE";
const TASK: &str = "Review notes";
const WIDTH: usize = 110;
const HEIGHT: usize = 44;
const CURSOR_VISIBLE: &str = "\x1b[?25h";
const CURSOR_HIDDEN: &str = "\x1b[?25l";
const COMPLETED_FRAME_END: &str = "\x1b[?2026l";
const CHILD_VIEW_HEADING: &str = "Delegated Agent ·";
const TIMEOUT: Duration = Duration::from_secs(15);
const VIEW_REGISTRATION_DELAY: Duration = Duration::from_millis(50);

/// Records every command the View sends, one JSON line each in `commands.jsonl`. On the
/// requester route, the catalog also lists a Lost sibling whose id shares the child's first ten
/// digits, so only a longer token tells the two apart. Slow View registration separates the
/// initial transcript frame from the initialized editor.
struct RecordingOwner {
    owner: EmbeddedAgentAccess,
    directory: PathBuf,
    sibling: bool,
}

/// An id equal to `id` in its first ten digits and different in the eleventh.
fn same_prefix_id(id: Uuid) -> Uuid {
    let mut digits = id.simple().to_string().into_bytes();
    digits[10] = if digits[10] == b'0' { b'1' } else { b'0' };
    Uuid::parse_str(std::str::from_utf8(&digits).unwrap()).unwrap()
}

#[async_trait]
impl AgentAccess for RecordingOwner {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let mut observation = self.owner.observe(query).await?;
        if self.sibling
            && let ObservationValue::Overview(rows) = &mut observation.initial
            && let Some(child) = rows.iter().find(|row| row.parent.is_some()).cloned()
        {
            rows.push(AgentSummary {
                availability: ExecutionAvailability::Lost,
                execution: ExecutionRef {
                    agent_id: same_prefix_id(child.execution.agent_id),
                    ..child.execution
                },
                chat_id: Uuid::new_v4(),
                state: runtime::LoopState::Completed,
                waiting_permissions: 0,
                waiting_questions: 0,
                views: Vec::new(),
                ..child
            });
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let mut commands = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("commands.jsonl"))
            .unwrap();
        writeln!(
            commands,
            "{}",
            serde_json::to_string(&command.command).unwrap()
        )
        .unwrap();
        self.owner.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        tokio::time::sleep(VIEW_REGISTRATION_DELAY).await;
        self.owner.show_view(update).await
    }
}

#[test]
#[ignore = "owned PTY producer for the child configuration run-loop regressions"]
fn child_config_pty_fixture() {
    if std::env::var(FIXTURE_VARIABLE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("child-config-fixture".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, run_child_config_fixture());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_child_config_fixture() {
    let directory = PathBuf::from(std::env::var_os(DIRECTORY_VARIABLE).unwrap());
    let route = std::env::var(ROUTE_VARIABLE).unwrap();
    let requester = route == "requester";
    let auto_permissions = route == "auto-permissions";
    let model_dialog = route == "model-dialog";
    let config_root = (auto_permissions || model_dialog)
        .then(|| directory.canonicalize().unwrap().join("config-root"));
    let model = if model_dialog {
        crate::model::credentials::save_api_key(
            config_root.as_ref().unwrap(),
            "DEEPSEEK_API_KEY",
            "fixture-key",
        )
        .unwrap();
        crate::model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap()
    } else {
        test_support::config()
    };
    let replies = if requester {
        vec![
            test_support::call("delegate", "DelegateAgent", json!({"task":TASK})),
            test_support::call(
                "child-question",
                "AskUserQuestion",
                test_support::single_question("Which window?"),
            ),
            test_support::call(
                "child-write",
                "Write",
                json!({"file_path":".bashrc","content":"fixture=approved"}),
            ),
            test_support::text("child approved"),
            test_support::text("parent done"),
        ]
    } else {
        vec![
            test_support::call("delegate", "DelegateAgent", json!({"task":TASK})),
            test_support::text("child reviewed"),
            test_support::text("parent done"),
            test_support::text("child follow-up done"),
        ]
    };
    let core = runtime::Agent::new_embedded(
        model.clone(),
        Arc::new(test_support::ScriptedModel::new(replies)),
    );
    let workdir = std::env::current_dir().unwrap();
    let adapter = Arc::new(RecordingOwner {
        owner: EmbeddedAgentAccess::new(core.clone()),
        directory: directory.clone(),
        sibling: requester,
    });
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(model);
    launch.session_persistence = false;
    launch.config_root = config_root.clone();
    let agent = Agent::from_access_for_test(
        adapter.clone(),
        adapter.owner.owner_epoch(),
        ViewRef {
            session_id: Uuid::new_v4(),
            view_id: Uuid::new_v4(),
        },
        launch,
        None,
    );
    let root = agent.create_chat(workdir.clone()).await.unwrap();
    core.set_execution_mode(
        root,
        if auto_permissions {
            crate::permissions::ExecutionMode::Auto
        } else {
            crate::permissions::ExecutionMode::AcceptEdits
        },
    )
    .await
    .unwrap();
    agent.submit(root, None, TASK.into()).await.unwrap();
    let latest = Mutex::new(String::new());
    let child = tokio::time::timeout(TIMEOUT / 2, async {
        loop {
            let rows = agent.catalog().await.unwrap();
            *latest.lock().unwrap() = format!(
                "root mode {:?}; rows {:?}",
                core.snapshot(root).await.unwrap().policy.mode,
                rows.iter()
                    .map(|row| (
                        row.parent.is_some(),
                        row.state.clone(),
                        row.waiting_permissions
                    ))
                    .collect::<Vec<_>>()
            );
            let child = rows.iter().find(|row| {
                row.parent.is_some()
                    && row.availability == ExecutionAvailability::Live
                    && if requester {
                        row.waiting_questions == 1
                    } else {
                        row.state == runtime::LoopState::Completed
                    }
            });
            let root_done = core
                .snapshot(root)
                .await
                .unwrap()
                .loops
                .iter()
                .all(|goal| goal.state == runtime::LoopState::Completed);
            if let Some(child) = child
                && (requester || root_done)
            {
                break child.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the scripted root delegates a real child: {}",
            latest.lock().unwrap()
        )
    });
    std::fs::write(
        directory.join("child.json"),
        serde_json::to_vec(&json!({
            "root": agent.reference(root).await.unwrap(),
            "execution": child.execution,
            "chat": child.chat_id,
            "sibling": same_prefix_id(child.execution.agent_id),
            "policy": core.snapshot(child.chat_id).await.unwrap().policy,
            "delegation": core.snapshot(child.chat_id).await.unwrap().delegation,
        }))
        .unwrap(),
    )
    .unwrap();
    let (console, input) = Console::start(true).unwrap();
    console.set_prompt_enabled(true).unwrap();
    let startup = StartupState {
        chat: StartupChat::Resumed(Box::new(agent.snapshot(root).await.unwrap()), false),
        user_settings: config_root
            .as_deref()
            .map(crate::settings::UserSettings::in_root),
    };
    let result = interact(&agent, workdir, &console, input, startup, config_root).await;
    std::fs::write(
        directory.join("final.json"),
        serde_json::to_vec(&json!({
            "root_mode": format!("{:?}", core.snapshot(root).await.unwrap().policy.mode),
            "child_mode": format!("{:?}", core.snapshot(child.chat_id).await.unwrap().policy.mode),
            "child_policy": core.snapshot(child.chat_id).await.unwrap().policy,
            "child_delegation": core.snapshot(child.chat_id).await.unwrap().delegation,
        }))
        .unwrap(),
    )
    .unwrap();
    drop(console);
    core.shutdown().await;
    result.unwrap();
}

struct ChildPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl ChildPty {
    fn start(directory: &Path, route: &str) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: HEIGHT as u16,
                cols: WIDTH as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        if route == "model-dialog" {
            command.env_clear();
        }
        command.args([
            "--ignored",
            "--exact",
            "terminal::child_config_tests::child_config_pty_fixture",
            "--nocapture",
        ]);
        command.env(FIXTURE_VARIABLE, "1");
        command.env(DIRECTORY_VARIABLE, directory);
        command.env(ROUTE_VARIABLE, route);
        command.env("TICKLOOP_AGENT_TUI", "fullscreen");
        command.env("TERM", "xterm-256color");
        command.env("HOME", directory);
        command.env("TICKLOOP_CONFIG_DIR", directory.join("config-root"));
        command.env("TICKLOOP_AGENT_RUNTIME_DIR", directory.join("runtime"));
        command.env("TICKLOOP_TMPDIR", directory);
        for model in crate::model::cards::built_in_model_profiles() {
            if !model.api_key_env.is_empty() {
                command.env_remove(model.api_key_env);
            }
        }
        if route == "model-dialog" {
            command.env("DEEPSEEK_API_KEY", "fixture-key");
        }
        if route == "auto-permissions" {
            command.env_remove(crate::judgment::JEV_API_KEY_ENV);
        }
        command.cwd(directory);
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
        Self {
            child,
            writer,
            output,
            _master: pair.master,
        }
    }

    fn frame(&self) -> String {
        screen::visible_screen(
            &String::from_utf8_lossy(&self.output.lock().unwrap()),
            WIDTH,
            HEIGHT,
        )
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn wait_for(&mut self, label: &str, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let frame = self.frame();
            if ready(&frame) {
                return frame;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "the View exited while waiting for {label}: {}",
                self.frame()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("child View never showed {label}: {}", self.frame());
    }

    fn wait_for_root_editor(&mut self) -> String {
        self.wait_for("the initialized root editor", |frame| {
            frame.contains("❯") && status_mode(frame).is_some()
        })
    }

    fn wait_for_child_editor(&mut self) {
        let captured = self.output.clone();
        self.wait_for("the child editor owns input", |frame| {
            let bytes = captured.lock().unwrap();
            let output = String::from_utf8_lossy(&bytes);
            let end = output
                .rfind(COMPLETED_FRAME_END)
                .map_or(output.len(), |end| end + COMPLETED_FRAME_END.len());
            let completed = &output[..end];
            frame.contains(CHILD_VIEW_HEADING)
                && completed.rfind(CURSOR_VISIBLE).is_some_and(|shown| {
                    completed
                        .rfind(CURSOR_HIDDEN)
                        .is_none_or(|hidden| shown > hidden)
                })
        });
    }

    fn assert_alive(&mut self, label: &str) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "the View exited after {label}: {}",
            self.frame()
        );
    }

    fn close(mut self) {
        self.send(b"\x15\x04\x04");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "the View closed with an error: {}",
                    String::from_utf8_lossy(&self.output.lock().unwrap())
                );
                return;
            }
            assert!(Instant::now() < deadline, "the View did not close");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Opens the delegated child in place from the overview.
    fn open_child(&mut self) {
        self.wait_for_root_editor();
        self.send(b"\x1b[D");
        self.wait_for("the overview", |frame| frame.contains("Agents · local"));
        self.send(b" ");
        self.wait_for("the child under its root", |frame| {
            frame.matches(TASK).count() >= 2
        });
        self.send(b"\x1b[B\r");
    }
}

impl Drop for ChildPty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn commands(directory: &Path) -> Vec<Value> {
    std::fs::read_to_string(directory.join("commands.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn child_fixture(directory: &Path, view: &ChildPty) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Ok(bytes) = std::fs::read(directory.join("child.json")) {
            return serde_json::from_slice(&bytes).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "the fixture never delegated: {}",
            String::from_utf8_lossy(&view.output.lock().unwrap())
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn child_model_provider_and_settings_entries_use_the_input_dialog() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = ChildPty::start(directory.path(), "model-dialog");
    child_fixture(directory.path(), &view);
    view.open_child();
    view.wait_for("the child in place", |frame| {
        frame.contains("child reviewed")
    });
    view.wait_for_child_editor();
    for (command, title, label) in [
        (b"/model\r".as_slice(), "Choose model", "child-model"),
        (
            b"/provider\r".as_slice(),
            "Choose Provider",
            "child-provider",
        ),
        (b"/settings\r".as_slice(), "Settings", "child-settings"),
    ] {
        view.send(command);
        let frame = view.wait_for(label, |frame| frame.contains(title));
        input_dialog::assert_frame(&frame, HEIGHT, title);
        input_dialog::record_frame(label, &frame);
        if title == "Settings" {
            view.send(b"\r");
            let frame = view.wait_for("child settings model", |frame| {
                frame.contains("Choose model")
            });
            input_dialog::assert_frame(&frame, HEIGHT, "Choose model");
            input_dialog::record_frame("child-settings-model", &frame);
        }
        if title == "Settings" {
            view.send(b"\x1b");
            view.wait_for("settings parent", |frame| {
                frame.lines().any(|line| line == "Settings") && !frame.contains("Choose model")
            });
        }
        view.send(b"\x1b");
        view.wait_for("child composer", |frame| {
            !frame.contains("Choose Provider")
                && !frame.contains("Choose model")
                && !frame.lines().any(|line| line == "Settings")
        });
    }
    view.close();
}

/// The execution mode label on the status row or lower input divider.
fn status_mode(frame: &str) -> Option<String> {
    frame
        .lines()
        .find_map(|line| line.split_once("⏵⏵ ").map(|(_, core)| core))
        .and_then(|line| line.split(" · ").next())
        .map(str::to_owned)
}

/// The overview's Project filter value; focus hints after it vary.
fn project_filter(frame: &str) -> Option<String> {
    frame
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("Project: "))
        .and_then(|line| line.split(" · Workspace").next())
        .map(str::to_owned)
}

fn selected_row(frame: &str) -> Option<String> {
    frame
        .lines()
        .find(|line| line.trim_start().starts_with("› "))
        .map(str::to_owned)
}

fn configures_of(commands: &[Value], execution: &Value) -> Vec<String> {
    commands
        .iter()
        .filter(|command| command["kind"] == "configure" && command["execution"] == *execution)
        .map(|command| command["change"]["kind"].as_str().unwrap().to_owned())
        .collect()
}

const ROOT_ONLY: &[&str] = &[
    "execution_mode",
    "directories",
    "permission_change",
    "replace_rules",
    "project_constraints",
    "managed_constraints",
];

#[test]
fn child_permission_candidates_and_help_describe_the_read_only_view() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = ChildPty::start(directory.path(), "child-config");
    child_fixture(directory.path(), &view);
    view.wait_for_root_editor();
    view.send(b"/per");
    let root = view.wait_for("the root permission candidate", |frame| {
        frame.contains("/permissions") && frame.contains("Choose Auto")
    });
    assert!(
        root.contains("Ask or Full Access"),
        "the root keeps its choices: {root}"
    );
    view.send(b"\x15");
    view.wait_for("the root candidate closed", |frame| {
        !frame.contains("/permissions")
    });
    view.open_child();
    view.wait_for("the child in place", |frame| {
        frame.contains("child reviewed")
    });
    view.wait_for_child_editor();
    let baseline = commands(directory.path()).len();
    view.send(b"/per");
    let candidate = view.wait_for("the child permission candidate", |frame| {
        frame.contains("/permissions")
    });
    assert!(
        candidate.contains("current mode and recent activity") && candidate.contains("read-only"),
        "the child candidate describes its read-only view: {candidate}"
    );
    assert!(!candidate.contains("Choose Auto") && !candidate.contains("configure Auto"));
    view.send(b"\x15/help\r");
    view.wait_for("command groups", |frame| {
        frame.contains("Commands") && frame.contains("Tools & permissions")
    });
    view.send(b"jj\r");
    let help = view.wait_for("the child permission help", |frame| {
        frame.contains("Tools & permissions") && frame.contains("/permissions")
    });
    assert!(
        help.contains("current mode and recent activity") && help.contains("read-only"),
        "the child help describes its read-only view: {help}"
    );
    assert!(!help.contains("Choose Auto") && !help.contains("configure Auto"));
    view.send(b"\x1b");
    view.wait_for("command groups again", |frame| {
        frame.contains("Advanced commands and arguments")
    });
    view.send(b"\x1b");
    view.wait_for("the child input again", |frame| {
        !frame.contains("Choose an action")
    });
    view.close();
    assert!(
        commands(directory.path())[baseline..]
            .iter()
            .all(|command| command["kind"] != "configure"),
        "reading candidates and help sends no configuration"
    );
}

#[test]
fn inherited_auto_permissions_keep_judgment_read_only_and_root_key_settings_editable() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let config_root = directory.path().join("config-root");
    let mut view = ChildPty::start(directory.path(), "auto-permissions");
    let child = child_fixture(directory.path(), &view);
    view.wait_for("the Auto root View", |frame| {
        frame.contains("parent done") && status_mode(frame).as_deref() == Some("Auto")
    });
    view.send(b"/permissions\r");
    view.wait_for("the root mode list", |frame| {
        frame
            .lines()
            .any(|line| line.contains("Auto · Use LLM") && line.contains("Current"))
    });
    view.send(b"\r");
    view.wait_for("the root Auto permissions", |frame| {
        frame.contains("Enter configure")
    });
    view.send(b"\x1b[B\r");
    view.wait_for("the root Judgment settings", |frame| {
        frame.contains("Judgment settings · Jev")
    });
    view.send(b"\r");
    view.wait_for("the private root key form", |frame| {
        frame.contains("Its contents stay hidden")
    });
    view.send(b"fixture-private-jev-key\r");
    view.wait_for("the root key saved locally", |frame| {
        frame.contains("Saved in Agent settings")
    });
    let env_file = config_root.join(".env");
    let saved_key = std::fs::read(&env_file).unwrap();
    assert!(String::from_utf8_lossy(&saved_key).contains("fixture-private-jev-key"));
    let settings_file = config_root.join(crate::settings::SETTINGS_FILE_NAME);
    let saved_settings = std::fs::read(&settings_file).ok();
    view.send(b"\x1b");
    view.wait_for("the root Auto permissions again", |frame| {
        frame.contains("Enter configure") && !frame.contains("Judgment settings")
    });
    view.send(b"\x1b");
    view.wait_for("the root mode list again", |frame| {
        frame
            .lines()
            .any(|line| line.contains("Auto · Use LLM") && line.contains("Current"))
    });
    view.send(b"\x1b");
    view.wait_for("the root input again", |frame| {
        !frame
            .lines()
            .any(|line| line.contains("Auto · Use LLM") && line.contains("Current"))
    });
    view.open_child();
    view.wait_for("the Auto child in place", |frame| {
        frame.contains("child reviewed") && status_mode(frame).as_deref() == Some("Auto")
    });
    let baseline = commands(directory.path()).len();
    view.send(b"/permissions\r");
    view.wait_for("the fixed child Auto mode", |frame| {
        frame.contains("Mode is fixed for this child")
    });
    view.send(b"\r");
    let auto = view.wait_for("the readable child Auto permissions", |frame| {
        frame.contains("Auto mode") && frame.contains("Jev (TypeSafe) key")
    });
    assert!(
        !auto.contains("Enter configure"),
        "the child offers no editing: {auto}"
    );
    view.send(b"\x1b[B");
    view.wait_for("the child's Jev key row selected", |frame| {
        frame
            .lines()
            .any(|line| line.trim_start().starts_with("❯ Jev (TypeSafe) key"))
    });
    view.send(b"\r\x1b[C");
    let refused = view.wait_for("the child Judgment entry handled", |frame| {
        frame.contains("[Recent activity]") || frame.contains("Judgment settings · Jev")
    });
    assert!(
        !refused.contains("Judgment settings · Jev"),
        "the child cannot enter editable Judgment settings: {refused}"
    );
    assert!(!refused.contains("Save key") && !refused.contains("Update Jev"));
    assert!(
        refused.contains("[Recent activity]"),
        "permissions navigation remains active: {refused}"
    );
    view.send(b"\x1b[D");
    let readonly = view.wait_for("child Auto overview after the refused entry", |frame| {
        frame.contains("[Auto mode]") && frame.contains("Jev (TypeSafe) key")
    });
    assert!(
        readonly.contains("Read-only:"),
        "Auto explains the refused editing: {readonly}"
    );
    view.send(b"\x1b[B\x1b[B\r");
    let details = view.wait_for("readable child Auto rules", |frame| {
        frame.contains("Built-in v1 included") && !frame.contains("Reviewer")
    });
    assert!(
        details.contains("Soft allow"),
        "Auto rule details remain readable: {details}"
    );
    view.send(b"\x1b");
    view.wait_for("child Auto overview again", |frame| {
        frame.contains("Reviewer") && frame.contains("Jev (TypeSafe) key")
    });
    view.send(b"\x1b");
    view.wait_for("fixed child mode list again", |frame| {
        frame.contains("Mode is fixed for this child")
    });
    view.send(b"\x1b[B\r");
    view.wait_for("child recent activity", |frame| {
        frame.contains("[Recent activity]")
    });
    view.send(b"\x1b[D");
    view.wait_for("child Auto from recent activity", |frame| {
        frame.contains("Jev (TypeSafe) key")
    });
    view.send(b"\x1b[B");
    view.wait_for("Jev selected from recent activity", |frame| {
        frame
            .lines()
            .any(|line| line.trim_start().starts_with("❯ Jev (TypeSafe) key"))
    });
    view.send(b"\r\x1b[C");
    let recent_refused = view.wait_for("Judgment blocked from recent activity too", |frame| {
        frame.contains("[Recent activity]") || frame.contains("Judgment settings · Jev")
    });
    assert!(
        recent_refused.contains("[Recent activity]")
            && !recent_refused.contains("Judgment settings · Jev")
    );
    view.send(b"\x1b[D");
    let readonly = view.wait_for("Auto explains the recent-activity entry refusal", |frame| {
        frame.contains("[Auto mode]") && frame.contains("Jev (TypeSafe) key")
    });
    assert!(readonly.contains("Read-only:"));
    view.send(b"\x1b");
    view.wait_for("the fixed mode list after viewing", |frame| {
        frame.contains("Mode is fixed for this child")
    });
    view.send(b"\x1b");
    view.wait_for("the child input after viewing", |frame| {
        !frame.contains("Mode is fixed for this child")
    });
    view.close();
    assert_eq!(
        std::fs::read(env_file).unwrap(),
        saved_key,
        "the child leaves the private root key unchanged"
    );
    assert_eq!(
        std::fs::read(settings_file).ok(),
        saved_settings,
        "the child leaves shared settings unchanged"
    );
    assert!(
        commands(directory.path())[baseline..]
            .iter()
            .all(|command| command["kind"] != "configure"),
        "viewing child permissions sends no hidden configuration"
    );
    let final_state: Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("final.json")).unwrap())
            .unwrap();
    assert_eq!(final_state["root_mode"], "Auto");
    assert_eq!(final_state["child_mode"], "Auto");
    assert_eq!(final_state["child_policy"], child["policy"]);
    assert_eq!(final_state["child_delegation"], child["delegation"]);
    assert!(configures_of(&commands(directory.path())[baseline..], &child["execution"]).is_empty());
}

#[test]
fn a_child_opened_in_place_refuses_root_only_changes_before_sending_and_keeps_its_view() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = ChildPty::start(directory.path(), "child-config");
    let child = child_fixture(directory.path(), &view);
    view.open_child();
    view.wait_for("the child in place", |frame| {
        frame.contains("child reviewed")
    });
    view.wait_for_child_editor();
    let baseline = commands(directory.path()).len();

    view.send(b"/permissions\r");
    let modes = view.wait_for("the fixed child mode", |frame| {
        frame.contains("Mode is fixed for this child")
    });
    assert!(
        !modes.contains("Full Access") && !modes.contains("Plan"),
        "only the current mode is offered: {modes}"
    );
    view.send(b"\x1b[B\r");
    let activity = view.wait_for("recent activity", |frame| frame.contains("Recent activity"));
    assert!(
        !activity.contains("a add") && !activity.contains("d remove"),
        "the child's activity offers no editing: {activity}"
    );
    view.send(b"\x1b");
    view.wait_for("the mode list again", |frame| {
        frame.contains("Mode is fixed for this child")
    });
    view.send(b"\x1b");
    view.wait_for("the input again", |frame| {
        !frame.contains("Mode is fixed for this child") && !frame.contains("Recent activity")
    });

    view.send(b"\x15/settings\r");
    view.wait_for("settings with the fixed mode", |frame| {
        frame.contains("· fixed by parent")
    });
    view.send(b"jjj\r");
    view.wait_for("the reason instead of a mode list", |frame| {
        frame.contains("Execution mode Ask is fixed here")
    });
    view.send(b"\x1b");
    view.wait_for("the settings menu closed", |frame| {
        !frame.contains("Glyph Set")
    });

    view.send(b"x");
    view.wait_for("the input focused", |frame| {
        frame.lines().any(|line| line.ends_with("x"))
    });
    view.send(b"\x15");
    std::thread::sleep(Duration::from_millis(200));
    view.send(b"\x1b[Z");
    let deadline = Instant::now() + TIMEOUT;
    while !view.frame().contains("Shift+Tab mode is unavailable here") {
        assert!(
            Instant::now() < deadline,
            "the refused Shift+Tab never showed: {}\n{:?}",
            view.frame(),
            &commands(directory.path())[baseline..]
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        view.frame().contains("Shift+Tab mode is unavailable here"),
        "status refreshes keep the reason: {}",
        view.frame()
    );
    view.assert_alive("Shift+Tab on a child");
    view.send(b"still here");
    view.wait_for("the View keeps its draft", |frame| {
        frame.contains("still here")
    });

    view.send(b"\x15/cwd sub\r");
    view.wait_for("the refused /cwd PATH", |frame| {
        frame.contains("/cwd PATH is unavailable here")
    });
    view.send(b"\x15/cwd\r");
    view.wait_for("the readable working directory", |frame| {
        frame.contains("Working directory:")
    });

    view.send(b"\x15/thinking auto\r");
    view.wait_for("the supported Thinking change", |frame| {
        frame.contains("Thinking: auto")
    });
    view.send(b"\x15follow up\r");
    view.wait_for("the supported follow-up", |frame| {
        frame.contains("child follow-up done")
    });
    view.close();

    let sent = commands(directory.path())[baseline..].to_vec();
    let child_changes = configures_of(&sent, &child["execution"]);
    assert!(
        child_changes
            .iter()
            .all(|change| !ROOT_ONLY.contains(&change.as_str())),
        "no root-only change left the View: {child_changes:?}"
    );
    assert!(
        child_changes
            .iter()
            .any(|change| change == "reasoning_effort")
    );
    assert_eq!(
        sent.iter()
            .filter(
                |command| command["kind"] == "submit" && command["execution"] == child["execution"]
            )
            .count(),
        1
    );
}

#[test]
fn shift_tab_on_the_root_still_changes_its_mode() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = ChildPty::start(directory.path(), "child-config");
    child_fixture(directory.path(), &view);
    let before = view.wait_for_root_editor();
    let initial = status_mode(&before);
    let baseline = commands(directory.path()).len();
    view.send(b"\x1b[Z");
    let deadline = Instant::now() + TIMEOUT;
    while status_mode(&view.frame()) == initial {
        assert!(
            Instant::now() < deadline,
            "Shift+Tab never changed the root mode: {}\n{:?}",
            view.frame(),
            &commands(directory.path())[baseline..]
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    view.close();
    let sent = commands(directory.path())[baseline..].to_vec();
    assert!(
        sent.iter().any(|command| command["kind"] == "configure"
            && command["change"]["kind"] == "execution_mode"),
        "the root keeps its mode control: {sent:?}"
    );
}

/// The overview shows no Agent, so Shift+Tab there must not cycle the root it hides, however often
/// it is pressed, nor disturb the dispatch draft, filter or selection; the root keeps its own key.
#[test]
fn shift_tab_in_the_overview_changes_no_hidden_agent_and_the_root_keeps_its_key() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = ChildPty::start(directory.path(), "child-config");
    let fixture = child_fixture(directory.path(), &view);
    let root = view.wait_for_root_editor();
    let root_mode = status_mode(&root);
    let baseline = commands(directory.path()).len();

    view.send(b"\x1b[D");
    view.wait_for("the overview list", |frame| {
        frame.contains("Agents · local") && frame.contains("Tab new Agent")
    });
    view.send(b"p");
    let listed = view.wait_for("a Project filter", |frame| {
        project_filter(frame).is_some_and(|project| project != "all")
    });
    let (project, selection) = (project_filter(&listed), selected_row(&listed));
    assert!(selection.is_some(), "the list has a selection: {listed}");
    view.send(b"\tnext task");
    view.wait_for("the dispatch draft", |frame| frame.contains("next task"));
    view.send(b"\x1b[Z");
    view.send(b"\x1b[Z");
    let refused = view.wait_for("Shift+Tab refused in the dispatch input", |frame| {
        frame.contains("Shift+Tab mode is unavailable here: acts on an open Agent")
    });
    view.assert_alive("Shift+Tab in the overview");
    assert!(
        refused.contains("next task"),
        "the dispatch draft stays: {refused}"
    );
    assert_eq!(
        project_filter(&refused),
        project,
        "the filter stays: {refused}"
    );
    view.send(b"\t");
    let relisted = view.wait_for("the list again", |frame| selected_row(frame).is_some());
    assert_eq!(
        selected_row(&relisted),
        selection,
        "the selection stays: {relisted}"
    );

    view.send(b"\x1b");
    view.wait_for("the root with its periodic status", |frame| {
        frame.contains("parent done") && !frame.contains("Shift+Tab mode is unavailable here")
    });
    view.send(b"\x1b[D");
    view.wait_for("the overview list again", |frame| {
        frame.contains("Agents · local") && frame.contains("Tab new Agent")
    });
    view.send(b"\x1b[Z");
    view.wait_for("Shift+Tab refused in the list", |frame| {
        frame.contains("Shift+Tab mode is unavailable here")
    });
    view.send(b"\t");
    view.wait_for("the dispatch input", |frame| {
        frame.contains("Enter dispatch")
    });
    // Advanced commands are never offered as completions; their reference shows each one's
    // meaning in the current scope.
    view.send(b"\x15/help advanced\r");
    view.wait_for("the advanced reference over the overview", |frame| {
        frame.contains("Advanced commands") && frame.contains("PgUp/PgDn")
    });
    for _ in 0..30 {
        view.send(b"\x1b[6~");
    }
    view.wait_for("the overview meaning of /acknowledge-create", |frame| {
        frame.contains("unconfirmed New Agent dispatch")
    });
    view.send(b"\x1b");
    view.wait_for("the parent Commands menu", |frame| {
        frame.contains("Commands")
            && frame.contains("Choose an action")
            && !frame.contains("unconfirmed New Agent dispatch")
    });
    view.send(b"\x1b");
    view.wait_for("the dispatch input after the reference", |frame| {
        frame.contains("Enter dispatch") && !frame.contains("Choose an action")
    });
    view.send(b"\x1b");
    let overview_sent = commands(directory.path())[baseline..].to_vec();
    assert!(
        overview_sent
            .iter()
            .all(|command| command["kind"] != "configure"),
        "the overview changed no Agent: {overview_sent:?}"
    );

    let back = view.wait_for("the root again", |frame| {
        frame.contains("parent done") && status_mode(frame).is_some()
    });
    assert_eq!(
        status_mode(&back),
        root_mode,
        "the hidden root kept its mode: {back}"
    );
    let control = commands(directory.path()).len();
    view.send(b"\x1b[Z");
    let deadline = Instant::now() + TIMEOUT;
    while status_mode(&view.frame()) == root_mode {
        assert!(
            Instant::now() < deadline,
            "Shift+Tab on the root never changed its mode: {}",
            view.frame()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    view.close();
    let sent = commands(directory.path())[control..].to_vec();
    assert_eq!(
        configures_of(&sent, &fixture["root"]),
        ["execution_mode"],
        "only the root's own key changed its mode: {sent:?}"
    );
    let final_state: Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("final.json")).unwrap())
            .unwrap();
    assert_eq!(
        final_state["root_mode"], "FullAccess",
        "one cycle from Ask; the overview presses never reached the owner"
    );
}

#[test]
fn a_child_request_names_the_child_with_its_family_number_and_no_persistent_grant() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut view = ChildPty::start(directory.path(), "requester");
    let child = child_fixture(directory.path(), &view);
    let child_id = Uuid::parse_str(child["execution"]["agent_id"].as_str().unwrap()).unwrap();
    let sibling_id = Uuid::parse_str(child["sibling"].as_str().unwrap()).unwrap();
    let question = view.wait_for("the child's question", |frame| {
        frame.contains("Which window?")
    });
    let named = question
        .split(&format!("Agent {TASK} ("))
        .nth(1)
        .and_then(|rest| rest.split_once(')'))
        .is_some_and(|(number, _)| number.parse::<usize>().is_ok());
    assert!(
        named,
        "the request names the child by its title and the family's reading number, since its family repeats the title: {question}"
    );
    for id in [child_id, sibling_id] {
        assert!(
            !question.contains(&id.simple().to_string()[..4]),
            "no execution hash names a requester: {question}"
        );
    }
    view.send(b"\r");
    let dialog = view.wait_for("the child's permission request", |frame| {
        frame.contains("Permission required")
    });
    assert!(
        !dialog.contains("don't ask again"),
        "a child cannot save a grant as a rule: {dialog}"
    );
    view.send(b"\r");
    let finished = view.wait_for(
        "the parent finishing with the child's list state",
        |frame| {
            frame.contains("Permission accepted")
                && frame.contains("parent done")
                && !frame.contains("Permission required")
                && frame
                    .lines()
                    .any(|line| line.contains(TASK) && line.contains("Completed"))
        },
    );
    assert!(
        !finished.contains("reply completed"),
        "completion is already in the family list: {finished}"
    );
    view.send(b"\x1b[B");
    let focused = view.wait_for("the completed family entry remains usable", |frame| {
        frame.contains("Enter/Space hide")
    });
    assert!(!focused.contains("reply completed"), "{focused}");
    view.send(b" ");
    let hidden = view.wait_for(
        "the collapsed family keeps completion out of the status area",
        |frame| frame.contains("Enter/Space expand"),
    );
    assert!(!hidden.contains("reply completed"), "{hidden}");
    view.send(b" ");
    view.wait_for("family reopens", |frame| frame.contains("Enter/Space hide"));
    view.send(b"\x1b");
    view.wait_for("focus returns to input", |frame| {
        !frame.contains("Enter/Space hide") && !frame.contains("Enter/Space expand")
    });

    view.close();
    let decisions = commands(directory.path())
        .into_iter()
        .filter(|command| command["kind"] == "decide_permission")
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0]["request"]["execution"], child["execution"]);
    assert_eq!(decisions[0]["scope"], "Once");
    assert!(directory.path().join(".bashrc").exists());
}
