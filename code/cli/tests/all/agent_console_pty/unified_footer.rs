use super::*;
use tickloop_code_access::AgentAccess;
use tickloop_harness::protocol::ShellTask;

use tickloop_harness::test_support::screen::SELECTED_MARK;

const TASK_TITLE: &str = "Inspect the shared task panel";
const TASK_REPLY: &str = "SHARED-TASK-READY";
const TASK_DRAFT: &str = "literal-jk-shared-draft";
const SHELL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Waits until the terminal no longer shows the Shell view headed by `command`.
async fn shell_view_closed(pty: &PtyView, command: &str) {
    tokio::time::timeout(UI_TIMEOUT, async {
        while terminal_screen(pty).contains(&format!("$ {command}")) {
            tokio::time::sleep(SHELL_POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the Shell view never closed:\n{}", terminal_screen(pty)));
}

/// Waits until a Shell's own view holds the terminal: its full command over its output, with its
/// status.
async fn shell_view_shows(pty: &PtyView, command: &str, output: &str, status: &str) {
    view_shows(pty, &[&format!("$ {command}"), output, status]).await;
}

async fn run_shared_task_list(fixture: &PrivateFixture) {
    tickloop_code_settings::UserSettings::in_root(&fixture.path("config"))
        .save_task_tools_enabled(true)
        .unwrap();
    let (base_url, mut requests, mut releases) = model_http_support::serve_responses_with_gated_responses(vec![
        (200, chat_reply(Some(("Subagent", "helper", json!({"name":"helper","description":"Inspect notes","task":"Inspect the notes"}))), "")),
        (200, chat_reply(None, "Helper finished")),
        (200, chat_reply(Some(("TaskCreate", "create-task", json!({"mutation_id":uuid::Uuid::new_v4(),"title":TASK_TITLE,"description":"Verify the shared status and dependency details"}))), "")),
        (200, chat_reply(None, TASK_REPLY)),
    ], &[3]).await;
    fixture.set_chat_model(&base_url);
    let mut pty = PtyView::launch(fixture, &["--dangerously-skip-permissions"], &[], true);
    pty.wait_for("new shared Task View", new_input_is_visible)
        .await;
    pty.resize(100, 30).await;
    let client = fixture.client().await;
    pty.send(b"Inspect with a helper.\r");
    pty.wait_for("shared Task summary appears", |frame| {
        frame.contains("1 tasks (0 done, 1 open)")
    })
    .await;
    let root = catalog(&client)
        .await
        .into_iter()
        .find(|row| row.parent.is_none())
        .unwrap()
        .execution;
    let actual = wait_snapshot(&client, root, "one persisted shared task", |current| {
        current
            .conversation
            .task_board
            .as_ref()
            .is_some_and(|board| board.tasks.len() == 1)
    })
    .await;
    assert_eq!(
        actual.conversation.task_board.as_ref().unwrap().tasks[0].title,
        TASK_TITLE
    );
    pty.send(b"\x14");
    pty.wait_for("Task list expands with owner and state", |frame| {
        frame.contains(TASK_TITLE)
            && !frame
                .lines()
                .any(|line| line.contains(TASK_TITLE) && line.contains("pending"))
            && !frame.contains("unassigned")
            && frame.contains("enter details")
    })
    .await;
    pty.send(b"\r");
    pty.wait_for("Task details show dependencies", |frame| {
        frame.contains("Dependencies: none")
    })
    .await;
    pty.send(b"\x1b");
    pty.wait_for("Task details close first", |frame| {
        !frame.contains("Dependencies: none") && frame.contains("enter details")
    })
    .await;
    pty.send(b"\x1b");
    pty.wait_for("Task list returns focus to editing", |frame| {
        frame.contains("ctrl+t focus")
    })
    .await;
    pty.send(TASK_DRAFT.as_bytes());
    pty.wait_for("Task panel returns focus to ordinary editing", |frame| {
        frame.contains(TASK_DRAFT)
    })
    .await;
    releases.remove(0).send(()).unwrap();
    pty.wait_for("shared Task persists while idle", |frame| {
        frame.contains(TASK_REPLY)
            && frame.contains("1 tasks (0 done, 1 open)")
            && frame.contains(TASK_DRAFT)
    })
    .await;
    let mut captured = Vec::new();
    for _ in 0..4 {
        captured.push(
            tokio::time::timeout(UI_TIMEOUT, requests.recv())
                .await
                .unwrap()
                .unwrap()
                .1,
        );
    }
    assert!(
        captured[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "TaskCreate")
    );
    assert!(
        captured[2]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "TaskCreate")
    );
    assert!(requests.try_recv().is_err());
    pty.close().await;
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::FlushTranscripts,
        })
        .await
        .unwrap();
    fixture.shutdown().await;
    let chat_id = actual.conversation.id.uuid().to_string();
    let mut resumed = PtyView::launch(fixture, &["--resume", &chat_id], &[], true);
    resumed
        .wait_for(
            "shared Task survives history load without model execution",
            |frame| frame.contains("History opened.") && frame.contains("1 tasks (0 done, 1 open)"),
        )
        .await;
    assert!(requests.try_recv().is_err());
    resumed.keep_history_paused().await;
    assert!(requests.try_recv().is_err());
    resumed.close().await;
}

pub(super) fn click_category(pty: &mut PtyView, label: &str) {
    let frame = pty.frame();
    let (row, column) = frame
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            line.find(label)
                .map(|column| (row, unicode_width::UnicodeWidthStr::width(&line[..column])))
        })
        .expect("rendered category exists");
    let click = format!(
        "\x1b[<0;{};{}M\x1b[<0;{};{}m",
        column + 1,
        row + 1,
        column + 1,
        row + 1
    );
    pty.send(click.as_bytes());
}

async fn shell_tasks(client: &LocalAgentClient, execution: ExecutionRef) -> Vec<ShellTask> {
    let observed = client
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Tasks,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Tasks(tasks) = observed.initial else {
        panic!("the public daemon returned an unexpected task response");
    };
    tasks
}

async fn wait_shell_tasks(
    client: &LocalAgentClient,
    execution: ExecutionRef,
    label: &str,
    ready: impl Fn(&[ShellTask]) -> bool,
) -> Vec<ShellTask> {
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let tasks = shell_tasks(client, execution).await;
            if ready(&tasks) {
                break tasks;
            }
            tokio::time::sleep(SHELL_POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label}"))
}

async fn run_shell_panel(fixture: &PrivateFixture) {
    let (base_url, mut requests) = model_http_support::serve_responses(vec![(
        200,
        chat_reply(None, "UNEXPECTED MODEL REQUEST"),
    )])
    .await;
    fixture.set_chat_model(&base_url);
    let mut pty = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    pty.wait_for("Shell fixture editor ready", new_input_is_visible)
        .await;
    // Below the 80-column minimum layout width Monitor stays out of the way until the category
    // click opens it as the overlay this flow drives.
    pty.resize(79, 24).await;
    let client = fixture.client().await;
    pty.send(b"!printf 'prior-completed\\n'; while [ ! -f prior-finish ]; do sleep 0.02; done\r");
    let execution = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(&client).await;
            if let [row] = rows.as_slice() {
                break row.execution;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    pty.wait_for(
        "setup Shell owns the foreground before completion",
        |frame| frame.contains("Shell running"),
    )
    .await;
    std::fs::write(fixture.path("work/prior-finish"), "finish").unwrap();
    wait_snapshot(
        &client,
        execution,
        "the broader catalog has a completed task",
        |current| {
            current.user_shells.len() == 1
                && current.user_shells[0].state == UserShellState::Completed
        },
    )
    .await;
    pty.wait_for("completed user Shell restores input", |frame| {
        frame.contains("prior-completed") && frame.lines().any(|line| line.starts_with('❯'))
    })
    .await;
    pty.wait_for_editor("completed Shell releases the foreground editor", Some(""))
        .await;
    pty.send(b"!printf 'shell-first-0123456789abcdef\\n'; while [ ! -f first-finish ]; do sleep 0.02; done\r");
    wait_snapshot(
        &client,
        execution,
        "first real Shell is foreground-running",
        |current| {
            current.user_shells.len() == 2
                && current.user_shells[1].state == UserShellState::Running
        },
    )
    .await;
    pty.wait_for("first Shell output is visible", |frame| {
        frame
            .lines()
            .any(|line| screen::response_line(line) == "shell-first-0123456789abcdef")
    })
    .await;
    pty.send(b"\x02");
    let running = wait_shell_tasks(
        &client,
        execution,
        "first Shell is genuinely background",
        |tasks| {
            tasks.iter().any(|task| {
                task.command.contains("shell-first")
                    && task.was_background
                    && !task.state.is_terminal()
            })
        },
    )
    .await;
    let first = running
        .iter()
        .find(|task| task.command.contains("shell-first"))
        .unwrap()
        .id;
    pty.wait_for("running and completed Shell Ticks are counted", |frame| {
        frame.contains("Monitor 1")
    })
    .await;
    pty.wait_for_editor("background Shell releases the editor", Some(""))
        .await;
    pty.send(b"jk-SHELL-DRAFT");
    pty.wait_for("Shell draft remains editable", |frame| {
        frame.contains("jk-SHELL-DRAFT")
    })
    .await;
    click_category(&mut pty, "Monitor 1");
    pty.wait_for("click opens the content-only Monitor", |frame| {
        frame.contains("Shells (1)")
    })
    .await;
    pty.send(b"\x1b[1;2B");
    pty.wait_for("Shift Down focuses the newest Shell", |frame| {
        frame.lines().any(|line| {
            line.contains(SELECTED_MARK) && line.contains("while [ ! -f first-finish ]")
        })
    })
    .await;
    pty.send(b"\r");
    shell_view_shows(
        &pty,
        "printf 'shell-first-0123456789abcdef\\n'; while",
        "shell-first-0123456789abcdef",
        "running",
    )
    .await;
    pty.send(b"\x1b");
    shell_view_closed(&pty, "printf 'shell-first-0123456789abcdef\\n'; while").await;
    pty.wait_for("Esc returns to the selected Tick row", |frame| {
        frame.contains("Shells (1)")
    })
    .await;
    pty.send(b"\x1b");
    pty.wait_for_editor(
        "Esc returns to the full-width draft",
        Some("jk-SHELL-DRAFT"),
    )
    .await;
    pty.send(b"\x15");
    pty.wait_for_editor("draft cleared", Some("")).await;
    pty.send(b"!printf 'shell-second\\n'; while [ ! -f second-finish ]; do sleep 0.02; done\r");
    pty.wait_for("second Shell output", |frame| {
        frame
            .lines()
            .any(|line| screen::response_line(line) == "shell-second")
    })
    .await;
    pty.send(b"\x02");
    let multiple = wait_shell_tasks(
        &client,
        execution,
        "both Shells are background-running",
        |tasks| {
            tasks
                .iter()
                .filter(|task| task.was_background && !task.state.is_terminal())
                .count()
                == 2
        },
    )
    .await;
    let second = multiple
        .iter()
        .find(|task| task.command.contains("shell-second"))
        .unwrap()
        .id;
    assert_ne!(first, second);
    pty.wait_for("completed Shell remains in the Tick count", |frame| {
        frame.contains("Monitor 2")
    })
    .await;
    pty.wait_for_editor("second Shell releases input", Some(""))
        .await;
    click_category(&mut pty, "Monitor 2");
    pty.wait_for("all three Shell Ticks appear", |frame| {
        frame.contains("Shells (2)")
    })
    .await;
    pty.send(b"\x1b[1;2B");
    pty.wait_for("newest exact Shell selected", |frame| {
        frame.lines().any(|line| {
            line.contains(SELECTED_MARK) && line.contains("while [ ! -f second-finish ]")
        })
    })
    .await;
    pty.send(b"\r");
    shell_view_shows(
        &pty,
        "printf 'shell-second\\n'; while",
        "shell-second",
        "running",
    )
    .await;
    pty.send(b"\x1b");
    shell_view_closed(&pty, "printf 'shell-second\\n'; while").await;
    pty.wait_for(
        "Esc returns to the second Shell's row, which can stop it",
        |frame| frame.contains("Shells (2)") && frame.contains("ctrl+x stop"),
    )
    .await;
    pty.send(b"\x18");
    pty.wait_for("Tick Shell stop needs confirmation", |frame| {
        frame.contains("Press ctrl+x again to stop this Shell")
    })
    .await;
    assert!(
        shell_tasks(&client, execution)
            .await
            .iter()
            .any(|task| task.id == second && !task.state.is_terminal())
    );
    pty.send(b"\x18");
    let stopped = wait_shell_tasks(
        &client,
        execution,
        "Stop affects only the selected task",
        |tasks| {
            tasks
                .iter()
                .any(|task| task.id == second && task.state.is_terminal())
        },
    )
    .await;
    assert!(
        stopped
            .iter()
            .any(|task| task.id == first && !task.state.is_terminal())
    );
    assert!(
        snapshot(&client, execution)
            .await
            .conversation
            .chat_loop
            .run_id
            .is_none()
    );
    assert!(requests.try_recv().is_err());
    pty.wait_for(
        "the stopped Shell keeps its row with how it ended",
        |frame| frame.contains("Shells (2)") && frame.lines().any(|line| line.contains("SIGTERM")),
    )
    .await;
    assert!(!pty.frame().contains("Shell stopped"), "{}", pty.frame());
    pty.send(b"\x1b");
    pty.wait_for_editor("Monitor closes to input", Some(""))
        .await;
    std::fs::write(fixture.path("work/first-finish"), "done").unwrap();
    std::fs::write(fixture.path("work/second-finish"), "done").unwrap();
    pty.close().await;
}

#[tokio::test]
async fn actual_cli_shell_category_uses_its_exact_eligible_count_and_task() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_shell_panel(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_shared_tasks_expand_and_persist_without_personal_todos() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_shared_task_list(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
