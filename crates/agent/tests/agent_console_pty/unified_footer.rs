use super::*;
use tickloop_agent::runtime::ShellTask;

const TODO_FIRST: &str = "Inspect the source-owned footer behavior";
const TODO_SECOND: &str = "Explain the verified layout result";
const TODO_REPLY: &str = "FOOTER-TODO-READY";
const TODO_DRAFT: &str = "jk-FOOTER-EDITOR-DRAFT";
const SHELL_POLL_INTERVAL: Duration = Duration::from_millis(20);
const COMPLETED_FRAME_START: &str = "\x1b[?2026h";
const COMPLETED_FRAME_END: &str = "\x1b[?2026l";
const CURSOR_VISIBLE: &str = "\x1b[?25h";
const CURSOR_HIDDEN: &str = "\x1b[?25l";

async fn wait_editor_ready(pty: &mut PtyView, label: &str, draft: &str) {
    let output = Arc::clone(&pty.output);
    pty.wait_for(label, |frame| {
        let bytes = output.lock().unwrap();
        let ansi = String::from_utf8_lossy(&bytes);
        let Some(end) = ansi.rfind(COMPLETED_FRAME_END) else {
            return false;
        };
        let Some(start) = ansi[..end].rfind(COMPLETED_FRAME_START) else {
            return false;
        };
        let completed = &ansi[start..end];
        let cursor_is_visible = completed.rfind(CURSOR_VISIBLE).is_some_and(|shown| {
            completed
                .rfind(CURSOR_HIDDEN)
                .is_none_or(|hidden| shown > hidden)
        });
        cursor_is_visible
            && !frame.contains("Shell running")
            && frame.lines().any(|line| {
                line.strip_prefix('❯')
                    .is_some_and(|text| text.trim() == draft)
            })
    })
    .await;
}

async fn run_default_todo_panel(fixture: &PrivateFixture) {
    let todos = json!({"todos":[
        {"content":TODO_FIRST,"status":"in_progress","activeForm":"Inspecting the source-owned footer behavior"},
        {"content":TODO_SECOND,"status":"pending","activeForm":"Explaining the verified layout result"}
    ]});
    let replies = [
        chat_reply(Some(("TodoWrite", "footer-todos", todos)), ""),
        chat_reply(None, TODO_REPLY),
    ];
    let (base_url, mut requests) = model_http_support::serve_responses(
        replies.into_iter().map(|reply| (200, reply)).collect(),
    )
    .await;
    fixture.set_chat_model(&base_url);
    let mut pty = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    pty.wait_for("new footer View ready", new_input_is_visible)
        .await;
    pty.resize(80, 24).await;
    let client = fixture.client().await;
    pty.send(b"Create the two-item checklist and report readiness.\r");
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
    .expect("one actual execution is created");
    let actual = wait_snapshot(&client, execution, "TodoWrite completes", |current| {
        current.conversation.loops[0].state == tickloop_agent::runtime::LoopState::Completed
            && current.conversation.progress.len() == 1
            && current.conversation.progress[0].todos.len() == 2
    })
    .await;
    assert_eq!(actual.conversation.progress[0].todos[0].content, TODO_FIRST);
    assert_eq!(
        actual.conversation.progress[0].todos[1].content,
        TODO_SECOND
    );
    assert!(actual.events.iter().any(|event| matches!(&event.kind,
        tickloop_agent::runtime::EventKind::ToolStarted { name, .. } if name == "TodoWrite"
    )));
    pty.wait_for("the controlled-model reply is rendered", |frame| {
        frame.contains(TODO_REPLY)
    })
    .await;
    pty.send(TODO_DRAFT.as_bytes());
    pty.wait_for("the editor keeps its original draft", |frame| {
        frame
            .lines()
            .any(|line| line.starts_with('❯') && line.contains(TODO_DRAFT))
    })
    .await;
    let before_open = pty.frame();
    assert!(!before_open.contains("Choose a model"));
    assert!(!before_open.contains("Permission"));
    assert!(!before_open.contains("Artifacts"));
    let opened = pty
        .wait_for(
            "Todo is expanded by default without a footer activation",
            |frame| {
                let rows = frame.lines().collect::<Vec<_>>();
                let Some(bar) = rows.iter().position(|line| line.contains("Todo 2")) else {
                    return false;
                };
                let panel = &rows[bar + 1..];
                panel.iter().any(|line| line.contains(TODO_FIRST))
                    && panel.iter().any(|line| line.contains(TODO_SECOND))
                    && rows
                        .iter()
                        .any(|line| line.starts_with('❯') && line.contains(TODO_DRAFT))
            },
        )
        .await;
    assert_eq!(
        opened
            .lines()
            .filter(|line| line.contains("Todo 2"))
            .count(),
        1
    );
    let rows = opened.lines().collect::<Vec<_>>();
    let editor = rows
        .iter()
        .position(|line| line.starts_with('❯') && line.contains(TODO_DRAFT))
        .unwrap();
    assert!(
        rows[editor + 1].starts_with('─') && rows[editor + 1].contains("Context"),
        "{opened}"
    );
    assert_eq!(
        rows.iter().filter(|line| line.contains("⏵⏵ Auto")).count(),
        1
    );
    pty.send(b"\x1b[B\r\r");
    pty.wait_for("Todo item opens read-only in the shared panel", |frame| {
        frame.contains("Todo item 1") && frame.contains(TODO_FIRST) && frame.contains("Read-only")
    })
    .await;
    pty.resize(40, 24).await;
    pty.wait_for("narrow Todo details retain the captured item", |frame| {
        frame.contains("Todo item 1") && frame.contains("Read-only")
    })
    .await;
    pty.send(b"\x1b");
    pty.wait_for("Esc returns from Todo detail to its list", |frame| {
        frame.contains("Todo 2") && !frame.contains("Read-only")
    })
    .await;
    pty.send(b"\x1b");
    wait_editor_ready(
        &mut pty,
        "Esc restores the ordinary editor caret",
        TODO_DRAFT,
    )
    .await;
    pty.send(b"jk");
    pty.wait_for(
        "Esc returns to the editor without consuming literal j/k",
        |frame| {
            frame
                .lines()
                .any(|line| line.starts_with('❯') && line.contains(&format!("{TODO_DRAFT}jk")))
        },
    )
    .await;
    assert_eq!(
        snapshot(&client, execution).await.conversation.progress[0]
            .todos
            .len(),
        2
    );
    for _ in 0..2 {
        tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(requests.try_recv().is_err());
    pty.close().await;
}

fn click_category(pty: &mut PtyView, label: &str) {
    let frame = pty.frame();
    let (row, column) = frame
        .lines()
        .enumerate()
        .find_map(|(row, line)| line.find(label).map(|column| (row, column)))
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

fn shell_panel(frame: &str) -> &str {
    let mut offset = 0;
    for line in frame.split_inclusive('\n') {
        if line.contains("Shell 1") || line.contains("Shell 2") {
            return &frame[offset..];
        }
        offset += line.len();
    }
    panic!("Shell classification bar is missing:\n{frame}");
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
        panic!("the public owner returned an unexpected task response");
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
    pty.resize(80, 24).await;
    let client = fixture.client().await;
    pty.send(b"!printf 'prior-completed\\n'\r");
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
    wait_editor_ready(
        &mut pty,
        "completed Shell releases the foreground editor",
        "",
    )
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
            .any(|line| line.trim() == "shell-first-0123456789abcdef")
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
    pty.wait_for("one eligible Shell appears automatically", |frame| {
        frame.contains("Shell 1") && !frame.contains("Shell 2")
    })
    .await;
    wait_editor_ready(&mut pty, "background Shell releases the editor", "").await;
    pty.send(b"jk-SHELL-DRAFT");
    pty.wait_for("Shell preview retains normal editor ownership", |frame| {
        frame
            .lines()
            .any(|line| line.starts_with('❯') && line.contains("jk-SHELL-DRAFT"))
    })
    .await;
    click_category(&mut pty, "Shell 1");
    pty.wait_for("one Shell click establishes its actual detail", |frame| {
        shell_panel(frame)
            .lines()
            .any(|line| line.trim() == "Shell task")
            && shell_panel(frame).contains("shell-first-0123456789abcdef")
    })
    .await;
    pty.send(b"\x1b[H");
    let detail = pty
        .wait_for("one Shell click opens exactly its eligible task", |frame| {
            let panel = shell_panel(frame);
            panel.contains("Status:") && panel.contains("shell-first-0123456789abcdef")
        })
        .await;
    let detail = shell_panel(&detail);
    assert!(
        !detail.contains("prior-completed") && !detail.contains(&first.to_string()),
        "{detail}"
    );
    assert!(detail.contains("0123456789abcdef"));
    pty.send(b"\x1b[D");
    pty.wait_for("Back returns to the same shared Shell list", |frame| {
        frame.contains("Shell 1") && !frame.contains("Status:") && frame.contains("jk-SHELL-DRAFT")
    })
    .await;
    pty.send(b"\x1b");
    wait_editor_ready(
        &mut pty,
        "Esc returns from the Shell list to input",
        "jk-SHELL-DRAFT",
    )
    .await;
    pty.send(b"\x15");
    wait_editor_ready(
        &mut pty,
        "the Shell draft is cleared before the next command",
        "",
    )
    .await;
    pty.send(b"!printf 'shell-second\\n'; while [ ! -f second-finish ]; do sleep 0.02; done\r");
    wait_snapshot(
        &client,
        execution,
        "second real Shell is foreground-running",
        |current| {
            current.user_shells.iter().any(|task| {
                task.command.contains("shell-second") && task.state == UserShellState::Running
            })
        },
    )
    .await;
    pty.wait_for("second Shell output is visible", |frame| {
        frame.lines().any(|line| line.trim() == "shell-second")
    })
    .await;
    pty.send(b"\x02");
    let multiple = wait_shell_tasks(
        &client,
        execution,
        "two eligible Shell tasks are present",
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
    pty.wait_for("two-task count matches the shared list", |frame| {
        frame.contains("Shell 2")
    })
    .await;
    wait_editor_ready(
        &mut pty,
        "the second background Shell releases the editor before its category is clicked",
        "",
    )
    .await;
    click_category(&mut pty, "Shell 2");
    let list = pty
        .wait_for("multiple Shell click stays in its eligible list", |frame| {
            let panel = shell_panel(frame);
            panel.contains("shell-first")
                && panel.contains("shell-second")
                && panel.contains("2 items · Enter details")
                && panel.lines().any(|line| line.trim_start().starts_with('›'))
                && !panel.contains("Status:")
        })
        .await;
    let list = shell_panel(&list);
    assert!(!list.contains("prior-completed"), "{list}");
    let rows = list.lines().collect::<Vec<_>>();
    let selected = rows
        .iter()
        .position(|line| line.trim_start().starts_with('›'))
        .expect("the clicked shared list acknowledges focus");
    let target = rows
        .iter()
        .position(|line| line.contains("shell-second"))
        .expect("the exact second Shell appears in the eligible list");
    if target < selected {
        pty.send(b"k");
    } else if target > selected {
        pty.send(b"j");
    }
    pty.wait_for("second exact Shell is selected", |frame| {
        shell_panel(frame)
            .lines()
            .any(|line| line.trim_start().starts_with('›') && line.contains("shell-second"))
    })
    .await;
    pty.send(b"\r");
    pty.wait_for(
        "selected second Shell establishes its actual detail",
        |frame| {
            let panel = shell_panel(frame);
            panel.lines().any(|line| line.trim() == "Shell task")
                && panel.contains("shell-second")
                && !panel.contains("shell-first")
        },
    )
    .await;
    pty.send(b"\x1b[H");
    pty.wait_for("selected second Shell detail opens", |frame| {
        let panel = shell_panel(frame);
        panel.contains("Status:")
            && panel.contains("shell-second")
            && !panel.contains("shell-first")
    })
    .await;
    pty.send(b"x");
    let stopped = wait_shell_tasks(
        &client,
        execution,
        "Stop affects only the selected second task",
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
    pty.wait_for("completed detail remains the second task", |frame| {
        let panel = shell_panel(frame);
        panel.contains("shell-second")
            && panel.contains("Status:")
            && !panel.contains("shell-first")
    })
    .await;
    pty.send(b"\x1b");
    pty.wait_for(
        "Esc returns the completed detail to its shared list",
        |frame| {
            frame.contains("Shell 1")
                && !shell_panel(frame)
                    .lines()
                    .any(|line| line.trim() == "Shell task")
        },
    )
    .await;
    pty.send(b"\x1b");
    wait_editor_ready(
        &mut pty,
        "Esc restores input before the explicit task command",
        "",
    )
    .await;
    pty.send(b"\x15/tasks\r");
    pty.wait_for("explicit tasks reopens the saved task surface", |frame| {
        frame.contains("Shell task")
    })
    .await;
    pty.send(b"\x1b[D");
    pty.wait_for(
        "explicit tasks retains the broader completed catalog",
        |frame| {
            let rows = frame.lines().collect::<Vec<_>>();
            let Some(heading) = rows.iter().position(|line| line.trim() == "Shell tasks") else {
                return false;
            };
            let list = &rows[heading + 1..];
            ["prior-completed", "shell-first", "shell-second"]
                .iter()
                .all(|command| list.iter().any(|line| line.contains(command)))
        },
    )
    .await;
    pty.send(b"\x1b");
    wait_editor_ready(
        &mut pty,
        "Esc closes the broader task list before View exit",
        "",
    )
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
async fn actual_cli_todo_panel_is_visible_by_default_without_taking_the_editor() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_default_todo_panel(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
