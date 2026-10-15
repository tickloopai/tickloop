use super::*;
use tickloop_agent_protocol::{AgentAccess, ShellTask};
use tickloop_agent_test_support::screen::SELECTED_MARK;

const TODO_FIRST: &str = "Inspect the source-owned footer behavior";
const TODO_SECOND: &str = "Explain the verified layout result";
const TODO_REPLY: &str = "FOOTER-TODO-READY";
const TODO_DRAFT: &str = "jk-FOOTER-EDITOR-DRAFT";
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

async fn run_working_todo_list(fixture: &PrivateFixture) {
    let todos = json!({"todos":[
        {"content":TODO_FIRST,"status":"in_progress","activeForm":"Inspecting the source-owned footer behavior"},
        {"content":TODO_SECOND,"status":"pending","activeForm":"Explaining the verified layout result"}
    ]});
    let completed = json!({"todos":[
        {"content":TODO_FIRST,"status":"completed","activeForm":"Inspecting the source-owned footer behavior"},
        {"content":TODO_SECOND,"status":"completed","activeForm":"Explaining the verified layout result"}
    ]});
    let (base_url, mut requests, mut releases) =
        model_http_support::serve_responses_with_gated_responses(
            vec![
                (
                    200,
                    chat_reply(Some(("TodoWrite", "active-todos", todos)), ""),
                ),
                (200, chat_reply(None, TODO_REPLY)),
                (
                    200,
                    chat_reply(Some(("TodoWrite", "completed-todos", completed)), ""),
                ),
                (200, chat_reply(None, "TODO-ALL-DONE")),
                (200, chat_reply(None, "TODO-THIRD-REPLY")),
                (200, chat_reply(None, "OTHER-GRACE-AGENT-REPLY")),
                (200, chat_reply(Some(("TodoWrite", "idle-done", json!({"todos":[{"content":"IDLE-GRACE-TASK","status":"completed","activeForm":"Finishing idle task"}]}))), "")),
                (200, chat_reply(None, "TODO-IDLE-DONE")),
                (200, chat_reply(Some(("TodoWrite", "invalid-todo", json!({"todos":"invalid"}))), "")),
                (200, chat_reply(None, "TODO-ERROR-REPLY")),
            ],
            &[1, 4],
        )
        .await;
    fixture.set_chat_model(&base_url);
    let mut pty = PtyView::launch(fixture, &["--dangerously-skip-permissions"], &[], true);
    pty.wait_for("new Todo View ready with known empty context", |frame| {
        new_input_is_visible(frame) && frame.contains("Context 0% used")
    })
    .await;
    pty.resize(80, 24).await;
    let client = fixture.client().await;
    pty.send(b"Todo grace probe.\r");
    let execution = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            if let [row] = catalog(&client).await.as_slice() {
                break row.execution;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let actual = wait_snapshot(
        &client,
        execution,
        "TodoWrite updates own progress",
        |current| {
            current
                .conversation
                .progress
                .first()
                .is_some_and(|progress| progress.todos.len() == 2)
        },
    )
    .await;
    assert_eq!(actual.conversation.progress[0].todos[0].content, TODO_FIRST);
    let working = pty
        .wait_for(
            "live Todos immediately follow the activity above input",
            |frame| {
                let rows = frame.lines().collect::<Vec<_>>();
                rows.iter()
                    .position(|line| {
                        // The first frames may not show the elapsed time yet.
                        line.contains("Working") && line.contains("s · esc to interrupt)")
                    })
                    .is_some_and(|activity| {
                        rows.get(activity + 1)
                            .is_some_and(|line| line.contains(TODO_FIRST))
                            && rows
                                .get(activity + 2)
                                .is_some_and(|line| line.contains(TODO_SECOND))
                    })
            },
        )
        .await;
    assert!(!working.contains("Todo 2"));
    assert!(
        !working.contains("TodoWrite")
            && !working.contains("Todos have been modified successfully")
    );
    assert!(
        working
            .lines()
            .any(|line| line.starts_with("  ⎿  ◼ ") && line.contains(TODO_FIRST))
    );
    assert!(
        working
            .lines()
            .any(|line| line.starts_with("     ◻ ") && line.contains(TODO_SECOND))
    );
    assert!(
        working
            .lines()
            .any(|line| { line.contains("Working") && line.contains("s · esc to interrupt)") }),
        "{working}"
    );
    releases.remove(0).send(()).unwrap();
    pty.wait_for("idle keeps the same owned Todo list", |frame| {
        frame.contains(TODO_REPLY)
            && frame.contains(TODO_FIRST)
            && frame.contains(TODO_SECOND)
            && frame.contains("2 tasks (0 done, 1 in progress, 1 open)")
    })
    .await;
    pty.wait_for_editor("idle input accepts the next turn", Some(""))
        .await;
    pty.send(b"Complete the checklist.\r");
    let done = pty
        .wait_for("all-done list remains during idle grace", |frame| {
            frame.contains("TODO-ALL-DONE")
                && frame
                    .lines()
                    .any(|row| row.contains("✔") && row.contains(TODO_FIRST))
        })
        .await;
    assert!(!done.contains("TodoWrite") && !done.contains("Todos have been modified successfully"));
    pty.send(b"A third real message while grace is active.\r");
    pty.wait_for(
        "next Working preserves completed rows until the same deadline",
        |frame| {
            frame.contains("Working")
                && frame
                    .lines()
                    .any(|row| row.contains("✔") && row.contains(TODO_FIRST))
        },
    )
    .await;
    let main_title = catalog(&client)
        .await
        .into_iter()
        .find(|row| row.execution == execution)
        .unwrap()
        .title;
    let mut other = PtyView::start(fixture);
    other
        .wait_for("other grace Agent draft", new_input_is_visible)
        .await;
    other.send(b"Other grace Agent.\r");
    other
        .wait_for("other real Agent completes", |frame| {
            frame.contains("OTHER-GRACE-AGENT-REPLY")
        })
        .await;
    let other_title = catalog(&client)
        .await
        .into_iter()
        .find(|row| row.execution != execution)
        .unwrap()
        .title;
    pty.send(b"\x1b[D");
    pty.wait_for("grace overview", |frame| frame.contains("Agents · local"))
        .await;
    select_overview_row(&mut pty, &other_title).await;
    pty.send(b"\r");
    pty.wait_for("other Agent does not inherit completed rows", |frame| {
        frame.contains("OTHER-GRACE-AGENT-REPLY") && !frame.contains(TODO_FIRST)
    })
    .await;
    pty.send(b"\x1b[D");
    pty.wait_for("return overview", |frame| frame.contains("Agents · local"))
        .await;
    select_overview_row(&mut pty, &main_title).await;
    pty.send(b"\r");
    // The completion grace is 30 seconds.
    pty.wait_for_within(
        "grace expires while the third turn is still Working",
        Duration::from_secs(40),
        |frame| {
            !frame.contains("Agents · local")
                && frame.contains("Working")
                && !frame.contains(TODO_FIRST)
                && !frame.contains(TODO_SECOND)
        },
    )
    .await;
    let completed = wait_snapshot(&client, execution, "completed list retired", |current| {
        current
            .conversation
            .progress
            .iter()
            .any(|progress| progress.todos.is_empty())
    })
    .await;
    assert!(completed.events.iter().any(|event| matches!(&event.kind, tickloop_agent_protocol::EventKind::ProgressUpdated { progress } if progress.todos.is_empty())));
    releases.remove(0).send(()).unwrap();
    pty.wait_for("idle retains the empty retired state", |frame| {
        frame.contains("TODO-THIRD-REPLY") && !frame.contains(TODO_FIRST)
    })
    .await;
    pty.send(b"\x0f");
    let archive = pty
        .wait_for("TodoWrite stays model-only in Transcript", |frame| {
            frame.contains("Showing detailed transcript") && frame.contains("TODO-THIRD-REPLY")
        })
        .await;
    assert!(
        !archive.contains("TodoWrite")
            && !archive.contains("Todos have been modified successfully")
    );
    pty.send(b"\x0f");
    pty.wait_for_editor("completed checklist returns editor focus", Some(""))
        .await;
    pty.send(b"Finish a checklist and stay idle.\r");
    pty.wait_for("idle completed rows retain their grace", |frame| {
        frame.contains("TODO-IDLE-DONE") && frame.contains("IDLE-GRACE-TASK")
    })
    .await;
    pty.wait_for_within(
        "idle completed rows expire without another message",
        Duration::from_secs(40),
        |frame| frame.contains("TODO-IDLE-DONE") && !frame.contains("IDLE-GRACE-TASK"),
    )
    .await;
    pty.send(b"Show a validation failure.\r");
    let failed = pty
        .wait_for("failed TodoWrite remains visible", |frame| {
            frame.contains("TODO-ERROR-REPLY")
                && frame.contains("⏺ TodoWrite")
                && frame.contains("⎿  Failed")
        })
        .await;
    assert!(!failed.contains("Todos have been modified successfully"));
    pty.send(TODO_DRAFT.as_bytes());
    pty.wait_for("literal editor keys survive layout", |frame| {
        frame.contains(TODO_DRAFT)
    })
    .await;
    let mut captured = Vec::new();
    for _ in 0..10 {
        captured.push(
            tokio::time::timeout(UI_TIMEOUT, requests.recv())
                .await
                .unwrap()
                .unwrap()
                .1,
        );
    }
    assert!(captured[4]["messages"].to_string().contains("TodoWrite"));
    assert!(
        captured[4]["messages"]
            .to_string()
            .contains("Todos have been modified successfully")
    );
    assert!(requests.try_recv().is_err());
    other.close().await;
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
    let chat_id = completed.conversation.id.to_string();
    let mut resumed = PtyView::launch(fixture, &["--resume", &chat_id], &[], true);
    let restored = resumed
        .wait_for(
            "expired Todos do not return after daemon restart",
            |frame| frame.contains("History opened.") && frame.contains("TODO-THIRD-REPLY"),
        )
        .await;
    assert!(
        !restored.contains(TODO_FIRST)
            && !restored.contains("TodoWrite · Completed")
            && !restored.contains("Todos have been modified successfully")
    );
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
        |frame| frame.contains("Shells (2)") && frame.contains("s stop"),
    )
    .await;
    pty.send(b"s");
    pty.wait_for("Tick Shell stop is confirmed", |frame| {
        frame.contains("Stop this Shell?")
    })
    .await;
    pty.send(b"\r");
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
            .loops
            .is_empty()
    );
    assert!(requests.try_recv().is_err());
    pty.wait_for(
        "the stopped Shell keeps its row with how it ended",
        |frame| frame.contains("Shells (2)") && frame.lines().any(|line| line.contains("SIGTERM")),
    )
    .await;
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
async fn actual_cli_completed_todos_keep_grace_then_retire_across_working_switch_and_restart() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_working_todo_list(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
