//! The Plan hand-off through the real View loop and a PTY: Shift+Tab enters Plan while a Run is
//! working, ExitPlanMode opens the approval in the Agent's Input Dialog by itself, feedback keeps
//! planning, approval resumes the same Run in the chosen mode, leaving Plan by Shift+Tab tells the
//! next request once that plan mode ended, and a cancelled, re-attached or restored approval stays
//! pending until it is approved, a restored one as an explicit continuation.
use super::input_dialog_pty::{FIXTURE_DIRECTORY, Fixture};
use super::*;
use serde_json::{Value, json};
use tickloop_agent_protocol::{Content, ExecutionMode, Role as MessageRole};
use tickloop_agent_test_support::ScriptedModel;

const FIXTURE_STACK_BYTES: usize = 16 * 1024 * 1024;
const FLOW_TIMEOUT: Duration = Duration::from_secs(20);
/// How often the fixture records the Agent state and the test samples the screen.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const ROUTE_FILE: &str = "route";
const SHIFT_TAB_ROUTE: &str = "shift-tab";
const CANCEL_ROUTE: &str = "cancel";
/// The View attaches while ExitPlanMode is already waiting.
const ATTACH_ROUTE: &str = "attach";
/// The daemon stopped while the approval was waiting; the View opens the restored Agent.
const RESTORED_ROUTE: &str = "restored";
/// The Bash tool creates `started` once admitted, then waits for the test to create `go`.
const STARTED_FILE: &str = "started";
const GO_FILE: &str = "go";
const STATE_FILE: &str = "state.json";
const PLAN_REMINDER: &str = "Plan mode is active.";
const EXIT_NOTICE: &str = "You have exited plan mode.";
const SHIFT_TAB: &[u8] = b"\x1b[Z";

#[test]
#[ignore]
fn plan_flow_pty_fixture() {
    let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
    thread::Builder::new()
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_plan_flow_fixture(directory))
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_plan_flow_fixture(directory: PathBuf) {
    let route = std::fs::read_to_string(directory.join(ROUTE_FILE)).unwrap();
    let (initial_mode, replies) = match route.as_str() {
        SHIFT_TAB_ROUTE => (
            ExecutionMode::Auto,
            vec![
                tickloop_agent_test_support::call(
                    "wait",
                    "Bash",
                    json!({"command": format!("touch {STARTED_FILE}; while [ ! -f {GO_FILE} ]; do sleep 0.05; done"), "description": "Wait for the go file"}),
                ),
                tickloop_agent_test_support::submit_plan(
                    "plan-1",
                    "# First plan\nCreate implemented.txt.",
                ),
                tickloop_agent_test_support::submit_plan(
                    "plan-2",
                    "# Revised plan\nCreate implemented.txt with a test note.",
                ),
                tickloop_agent_test_support::call(
                    "implement",
                    "Write",
                    json!({"file_path": "implemented.txt", "content": "approved"}),
                ),
                tickloop_agent_test_support::text("Implementation complete."),
                tickloop_agent_test_support::text("Wrapped up outside Plan."),
            ],
        ),
        CANCEL_ROUTE => (
            ExecutionMode::Plan,
            vec![
                tickloop_agent_test_support::submit_plan(
                    "plan",
                    "# Pending plan\nShip the reviewed change.",
                ),
                tickloop_agent_test_support::text("Continuing the approved plan."),
            ],
        ),
        ATTACH_ROUTE => (
            ExecutionMode::Plan,
            vec![
                tickloop_agent_test_support::submit_plan(
                    "plan",
                    "# Live plan\nImplement after approval.",
                ),
                tickloop_agent_test_support::text("Implementation started."),
            ],
        ),
        RESTORED_ROUTE => (
            ExecutionMode::Plan,
            vec![tickloop_agent_test_support::text(
                "Restored implementation started.",
            )],
        ),
        route => panic!("unknown plan flow route {route}"),
    };
    // These flows walk through every execution mode, not only the default Auto and Plan.
    std::fs::write(
        directory.join("settings.json"),
        json!({
            "promptSuggestionEnabled": false,
            "shiftTabModes": ["auto", "acceptEdits", "plan", "fullAccess"],
        })
        .to_string(),
    )
    .unwrap();
    let scripted = Arc::new(ScriptedModel::new(replies));
    let (core, agent, model, restored_chat) = if route == RESTORED_ROUTE {
        let model = tickloop_agent_test_support::persisted_config();
        let root = directory.join("agent-state");
        let chat = raise_plan_approval_then_stop(&root, &directory, model.clone()).await;
        let core = tickloop_agent_runtime::Agent::open_embedded(
            root.clone(),
            model.clone(),
            scripted.clone(),
        )
        .unwrap();
        core.install_own_tickhost();
        let mut launch = tickloop_agent_protocol::LaunchSpec::embedded(directory.clone());
        launch.model = Some(model.clone());
        launch.config_root = Some(root);
        let agent = crate::terminal::execution::ConsoleExecution::from_embedded_launch(
            core.clone(),
            launch,
        );
        (core, agent, model, Some(chat))
    } else {
        let model = tickloop_agent_test_support::config();
        let core = tickloop_agent_runtime::Agent::new_embedded(model.clone(), scripted.clone());
        core.install_own_tickhost();
        let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
            core.clone(),
            model.clone(),
        );
        (core, agent, model, None)
    };
    core.set_auto_reviewer(Arc::new(ScriptedModel::new(vec![
        tickloop_agent_test_support::text(
            &tickloop_agent_test_support::auto_classification(false).to_string(),
        );
        3
    ])))
    .await;
    let chat = match restored_chat {
        Some(chat) => {
            agent.resume_chat(chat).await.unwrap();
            chat
        }
        None => {
            let chat = agent.create_chat(directory.clone()).await.unwrap();
            agent.set_execution_mode(chat, initial_mode).await.unwrap();
            chat
        }
    };
    if route == ATTACH_ROUTE {
        let mut events = core.subscribe();
        core.submit(chat, "Plan the change".into()).await.unwrap();
        wait_for_plan_approval(&mut events).await;
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let (console, input) = Console::start(true).unwrap();
    crate::terminal::initialize_startup_console(&console, &model, &directory, initial_mode, vec![])
        .unwrap();
    let interaction = crate::terminal::interact(
        &agent,
        directory.clone(),
        &console,
        input,
        crate::terminal::StartupState {
            launch_arguments: Vec::new(),
            chat: crate::terminal::StartupChat::Resumed(Box::new(snapshot), false),
            user_settings: Some(tickloop_agent_config::UserSettings::in_root(&directory)),
        },
        Some(directory.clone()),
    );
    tokio::pin!(interaction);
    let mut inspection = tokio::time::interval(POLL_INTERVAL);
    loop {
        tokio::select! {
            result = &mut interaction => {
                result.unwrap();
                break;
            }
            _ = inspection.tick() => {
                let snapshot = core.snapshot(chat).await.unwrap();
                let state = json!({
                    "mode": snapshot.policy.mode,
                    "plans": snapshot.plans.iter().map(|plan| json!({
                        "version": plan.version,
                        "pending": plan.approval_pending,
                        "approved": plan.approved_execution_mode,
                    })).collect::<Vec<_>>(),
                    "loop": format!("{:?}", snapshot.chat_loop.state),
                    "permissions": snapshot.permissions.iter().map(|request| request.name.clone()).collect::<Vec<_>>(),
                    "requests": request_summaries(&scripted),
                });
                std::fs::write(directory.join("state.tmp"), state.to_string()).unwrap();
                std::fs::rename(directory.join("state.tmp"), directory.join(STATE_FILE)).unwrap();
            }
        }
    }
    core.shutdown().await;
}

/// Runs a daemon until its Agent's ExitPlanMode waits for approval, then stops it undecided.
async fn raise_plan_approval_then_stop(
    root: &Path,
    workdir: &Path,
    model: tickloop_agent_protocol::ModelConfig,
) -> Uuid {
    let core = tickloop_agent_runtime::Agent::open_embedded(
        root.to_owned(),
        model,
        Arc::new(ScriptedModel::new(vec![
            tickloop_agent_test_support::submit_plan(
                "plan",
                "# Restored plan\nResume after restart.",
            ),
        ])),
    )
    .unwrap();
    let chat = core.create_chat(workdir.to_owned()).await.unwrap();
    core.set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = core.subscribe();
    core.submit(chat, "Plan the change".into()).await.unwrap();
    wait_for_plan_approval(&mut events).await;
    core.shutdown().await;
    chat
}

async fn wait_for_plan_approval(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) {
    loop {
        match tickloop_agent_test_support::next_event(events).await.kind {
            tickloop_agent_protocol::EventKind::PermissionRequested { request }
                if request.name == tickloop_agent_tools::EXIT_PLAN_MODE_TOOL_NAME =>
            {
                return;
            }
            tickloop_agent_protocol::EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

/// Whether each model request was made in Plan mode, how many of its messages carry the
/// exited-plan-mode notice, and the texts and tool results of its latest message.
fn request_summaries(model: &ScriptedModel) -> Vec<Value> {
    model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| {
            let system = request
                .messages
                .iter()
                .filter(|message| message.role == MessageRole::System)
                .flat_map(|message| &message.content)
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            let latest = request
                .messages
                .last()
                .into_iter()
                .flat_map(|message| &message.content)
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.clone()),
                    Content::ToolResult { content, .. } => Some(content.text().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let exit_notices = request
                .messages
                .iter()
                .filter(|message| {
                    message.content.iter().any(|content| {
                        matches!(content, Content::Text { text } if text.contains(EXIT_NOTICE))
                    })
                })
                .count();
            json!({
                "plan_mode": system.contains(PLAN_REMINDER),
                "exit_notices": exit_notices,
                "offers_exit_plan_mode": request.tools.iter().any(|tool| tool.name == tickloop_agent_tools::EXIT_PLAN_MODE_TOOL_NAME),
                "latest": latest,
            })
        })
        .collect()
}

struct PlanFlow {
    fixture: Fixture,
    directory: tempfile::TempDir,
}

impl PlanFlow {
    fn start(route: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(ROUTE_FILE), route).unwrap();
        let fixture = Fixture::start(
            directory.path(),
            120,
            40,
            "terminal::console::plan_flow_pty::plan_flow_pty_fixture",
        );
        Self { fixture, directory }
    }

    fn state(&self) -> Value {
        std::fs::read_to_string(self.directory.path().join(STATE_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null)
    }

    fn requests(&self) -> Vec<Value> {
        self.state()["requests"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Waits until both the screen and the recorded Agent state satisfy `ready`.
    fn wait(&self, label: &str, ready: impl Fn(&str, &Value) -> bool) -> (String, Value) {
        let deadline = Instant::now() + FLOW_TIMEOUT;
        loop {
            let frame = self.fixture.frame();
            let state = self.state();
            if state.is_object() && ready(&frame, &state) {
                return (frame, state);
            }
            assert!(
                Instant::now() < deadline,
                "never reached {label}\nstate: {state}\nframe:\n{frame}"
            );
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn wait_for_mode(&self, mode: &str) -> String {
        let route = std::fs::read_to_string(self.directory.path().join(ROUTE_FILE)).unwrap();
        let model = if route == RESTORED_ROUTE {
            "DeepSeek Flash"
        } else {
            "fixture"
        };
        self.wait(&format!("mode {mode}"), |frame, _| {
            frame.contains(&format!("⏵⏵ {mode} · {model}"))
        })
        .0
    }

    fn exit(mut self) {
        self.fixture.send(b"/exit\r");
        let deadline = Instant::now() + FLOW_TIMEOUT;
        loop {
            if let Some(status) = self.fixture.child.try_wait().unwrap() {
                assert!(status.success(), "{}", self.fixture.frame());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the View did not exit: {}",
                self.fixture.frame()
            );
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// The Input Dialog at the bottom of the screen, from its plan approval title on.
fn approval_dialog(frame: &str) -> Option<&str> {
    frame.find("Plan approval").map(|start| &frame[start..])
}

fn latest_contains(request: &Value, text: &str) -> bool {
    request["latest"].as_array().is_some_and(|latest| {
        latest
            .iter()
            .any(|entry| entry.as_str().is_some_and(|entry| entry.contains(text)))
    })
}

#[test]
fn plan_flow_pty_shift_tab_plans_mid_run_then_feedback_and_approval_resume_the_same_run() {
    let mut flow = PlanFlow::start(SHIFT_TAB_ROUTE);
    flow.wait_for_mode("Auto");
    flow.fixture.send(b"Implement the change\r");
    // Shift+Tab waits for the admitted tool: a mode changed earlier decides its permission.
    flow.wait("the running Bash wait", |_, _| {
        flow.directory.path().join(STARTED_FILE).exists()
    });

    flow.fixture.send(SHIFT_TAB);
    flow.wait_for_mode("Ask");
    flow.fixture.send(SHIFT_TAB);
    flow.wait_for_mode("Plan");
    let (_, state) = flow.wait("Plan recorded during the Run", |_, state| {
        state["mode"] == "plan"
    });
    assert_eq!(state["requests"].as_array().unwrap().len(), 1);
    assert_eq!(state["requests"][0]["plan_mode"], false);

    std::fs::write(flow.directory.path().join(GO_FILE), "").unwrap();
    // The terminal and the periodically recorded state reach each approval independently.
    let (frame, state) = flow.wait("the first plan approval", |frame, state| {
        approval_dialog(frame).is_some_and(|dialog| dialog.contains("First plan"))
            && state["plans"][0]["version"] == 1
            && state["permissions"] == json!(["ExitPlanMode"])
    });
    let requests = state["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "{state}");
    assert_eq!(
        requests[1]["plan_mode"], true,
        "the next request plans: {state}"
    );
    assert_eq!(requests[1]["offers_exit_plan_mode"], true);
    assert_eq!(state["permissions"], json!(["ExitPlanMode"]));
    assert_eq!(frame.matches("Plan approval").count(), 1, "{frame}");
    for option in [
        "Approve and start · Auto",
        "Approve and start · Ask",
        "Approve and start · Full Access",
        "No, keep planning",
    ] {
        assert!(frame.contains(option), "{option}\n{frame}");
    }

    flow.fixture.send(b"jjj\r");
    flow.fixture.send(b"Add a test note");
    flow.wait("typed feedback", |frame, _| {
        frame.contains("Add a test note")
    });
    flow.fixture.send(b"\r");
    let (frame, state) = flow.wait("the revised plan approval", |frame, state| {
        approval_dialog(frame).is_some_and(|dialog| dialog.contains("Revised plan"))
            && state["plans"][0]["version"] == 2
            && state["permissions"] == json!(["ExitPlanMode"])
    });
    let requests = state["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 3, "{state}");
    assert_eq!(
        requests[2]["plan_mode"], true,
        "feedback keeps planning: {state}"
    );
    assert!(
        latest_contains(&requests[2], "User feedback: Add a test note"),
        "{state}"
    );
    let dialog = approval_dialog(&frame).unwrap();
    assert!(dialog.contains("Plan version: 2"), "{frame}");
    assert!(!dialog.contains("First plan"), "{frame}");
    assert!(!dialog.contains("Add a test note"), "{frame}");
    assert_eq!(state["mode"], "plan");

    flow.fixture.send(b"j\r");
    let (_, state) = flow.wait("the approved implementation in Ask", |frame, state| {
        frame.contains("Implementation complete.")
            && frame.contains("⏵⏵ Ask · fixture")
            && approval_dialog(frame).is_none()
            && state["loop"] == "Idle"
    });
    let requests = state["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 5, "{state}");
    assert_eq!(requests[3]["plan_mode"], false, "{state}");
    assert!(
        latest_contains(&requests[3], "User has approved your plan."),
        "{state}"
    );
    assert!(latest_contains(&requests[3], "Revised plan"), "{state}");
    assert_eq!(requests[3]["exit_notices"], 0, "{state}");
    assert_eq!(state["mode"], "acceptEdits");
    assert_eq!(
        state["plans"],
        json!([{"version": 2, "pending": false, "approved": "acceptEdits"}])
    );
    assert!(state["permissions"].as_array().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(flow.directory.path().join("implemented.txt")).unwrap(),
        "approved"
    );

    flow.fixture.send(SHIFT_TAB);
    let frame = flow.wait_for_mode("Plan");
    assert!(!frame.contains("Plan approval"), "{frame}");
    flow.fixture.send(SHIFT_TAB);
    flow.wait_for_mode("Full Access");
    let (_, state) = flow.wait("Full Access recorded", |_, state| {
        state["mode"] == "fullAccess"
    });
    assert_eq!(state["requests"].as_array().unwrap().len(), 5);

    // Leaving Plan by Shift+Tab tells the next request once that plan mode ended.
    flow.fixture.send(b"Wrap up\r");
    let (_, state) = flow.wait("the reply outside Plan", |frame, state| {
        frame.contains("Wrapped up outside Plan.")
            && state["requests"].as_array().map(Vec::len) == Some(6)
            && state["loop"] == "Idle"
    });
    let request = &state["requests"][5];
    assert_eq!(request["plan_mode"], false, "{state}");
    assert_eq!(request["exit_notices"], 1, "{state}");
    assert!(latest_contains(request, EXIT_NOTICE), "{state}");
    flow.exit();
}

#[test]
fn plan_flow_pty_cancelled_approval_stays_pending_until_plan_approves_a_continuation() {
    let mut flow = PlanFlow::start(CANCEL_ROUTE);
    flow.wait_for_mode("Plan");
    flow.fixture.send(b"Plan the change\r");
    flow.wait("the plan approval", |frame, _| {
        approval_dialog(frame).is_some_and(|dialog| dialog.contains("Pending plan"))
    });

    // As in Claude Code's `PermissionRequest`, Ctrl+C rejects the plan as Esc does.
    flow.fixture.send(b"\x03");
    let (_, state) = flow.wait("the cancelled approval", |frame, state| {
        !frame.contains("Plan approval") && state["loop"] == "Interrupted"
    });
    assert_eq!(
        state["plans"],
        json!([{"version": 1, "pending": true, "approved": null}])
    );
    assert!(state["permissions"].as_array().unwrap().is_empty());
    assert_eq!(state["mode"], "plan");
    thread::sleep(Duration::from_millis(300));
    let frame = flow.fixture.frame();
    assert!(
        !frame.contains("Plan approval"),
        "a cancelled approval reopened itself: {frame}"
    );
    assert_eq!(flow.requests().len(), 1);

    flow.fixture.send(SHIFT_TAB);
    flow.wait_for_mode("Full Access");
    flow.fixture.send(b"/plan\r");
    flow.wait("the reviewed plan", |frame, _| {
        approval_dialog(frame).is_some_and(|dialog| dialog.contains("Pending plan"))
    });
    flow.fixture.send(b"\x1b");
    flow.wait("the closed review", |frame, _| {
        approval_dialog(frame).is_none()
    });
    thread::sleep(Duration::from_millis(300));
    let (frame, state) = (flow.fixture.frame(), flow.state());
    assert!(
        frame.contains("⏵⏵ Full Access · fixture"),
        "closing a review keeps the mode: {frame}"
    );
    assert_eq!(state["mode"], "fullAccess");
    assert_eq!(
        state["plans"],
        json!([{"version": 1, "pending": true, "approved": null}])
    );

    flow.fixture.send(b"/plan\r");
    flow.wait("the reopened plan approval", |frame, _| {
        approval_dialog(frame).is_some_and(|dialog| dialog.contains("Pending plan"))
    });
    flow.fixture.send(b"jj\r");
    let (_, state) = flow.wait(
        "the approved continuation in Full Access",
        |frame, state| {
            frame.contains("Continuing the approved plan.")
                && frame.contains("⏵⏵ Full Access · fixture")
                && approval_dialog(frame).is_none()
                && state["loop"] == "Idle"
        },
    );
    let requests = state["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "{state}");
    assert_eq!(requests[1]["plan_mode"], false, "{state}");
    assert!(
        latest_contains(&requests[1], "User has approved your plan."),
        "{state}"
    );
    assert!(latest_contains(&requests[1], "Pending plan"), "{state}");
    assert_eq!(
        requests[1]["exit_notices"], 0,
        "approval after Shift+Tab left Plan resumes with the approved plan alone: {state}"
    );
    assert_eq!(
        state["plans"],
        json!([{"version": 1, "pending": false, "approved": "fullAccess"}])
    );
    flow.exit();
}

#[test]
fn plan_flow_pty_a_view_attaching_to_a_waiting_approval_resumes_the_same_run() {
    let mut flow = PlanFlow::start(ATTACH_ROUTE);
    let (_, state) = flow.wait("the waiting approval on attach", |frame, _| {
        approval_dialog(frame).is_some_and(|dialog| dialog.contains("Live plan"))
    });
    assert_eq!(state["permissions"], json!(["ExitPlanMode"]), "{state}");
    assert_eq!(state["requests"].as_array().unwrap().len(), 1, "{state}");

    flow.fixture.send(b"\r");
    let (_, state) = flow.wait("the approved Run in Auto", |frame, state| {
        frame.contains("Implementation started.")
            && frame.contains("⏵⏵ Auto · fixture")
            && approval_dialog(frame).is_none()
            && state["loop"] == "Idle"
    });
    let requests = state["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "{state}");
    assert_eq!(requests[1]["plan_mode"], false, "{state}");
    assert!(
        latest_contains(&requests[1], "User has approved your plan."),
        "{state}"
    );
    assert_eq!(
        state["plans"],
        json!([{"version": 1, "pending": false, "approved": "auto"}])
    );
    flow.exit();
}

#[test]
fn plan_flow_pty_a_restarted_daemon_reopens_the_pending_approval_and_continues_explicitly() {
    let mut flow = PlanFlow::start(RESTORED_ROUTE);
    let (_, state) = flow.wait("the restored approval", |frame, _| {
        approval_dialog(frame).is_some_and(|dialog| dialog.contains("Restored plan"))
    });
    assert!(
        state["permissions"].as_array().unwrap().is_empty(),
        "{state}"
    );
    assert!(state["requests"].as_array().unwrap().is_empty(), "{state}");
    assert_eq!(state["mode"], "plan");
    assert_eq!(
        state["plans"],
        json!([{"version": 1, "pending": true, "approved": null}])
    );

    flow.fixture.send(b"j\r");
    let (_, state) = flow.wait("the restored continuation in Ask", |frame, state| {
        frame.contains("Restored implementation started.")
            && frame.contains("⏵⏵ Ask · DeepSeek Flash")
            && approval_dialog(frame).is_none()
            && state["loop"] == "Idle"
    });
    let requests = state["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 1, "{state}");
    assert_eq!(requests[0]["plan_mode"], false, "{state}");
    assert!(
        latest_contains(&requests[0], "User has approved your plan."),
        "{state}"
    );
    assert!(latest_contains(&requests[0], "Restored plan"), "{state}");
    assert_eq!(
        state["plans"],
        json!([{"version": 1, "pending": false, "approved": "acceptEdits"}])
    );
    flow.exit();
}
