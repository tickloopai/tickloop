use super::*;

use tickloop_agent_access::{AgentAccess, ViewRef};

use tickloop_agent_test_support::screen::SELECTED_MARK;

/// The family panel's rows under its heading; a conversation's `› Message from` lines are not
/// among them.
fn family_rows(frame: &str) -> Vec<&str> {
    frame
        .lines()
        .skip_while(|line| !line.contains("Subagents 2"))
        .skip(1)
        .collect()
}

fn family_rows_are_visible(frame: &str) -> bool {
    family_rows(frame).iter().any(|line| {
        [MAIN_TASK, FIRST_CHILD_TASK, SECOND_CHILD_TASK]
            .iter()
            .any(|title| line.contains(title))
    })
}

const MAIN_TASK: &str = "FAMILY-MAIN";
const FIRST_CHILD_TASK: &str = "FAMILY-CHILD-ONE";
const SECOND_CHILD_TASK: &str = "FAMILY-CHILD-TWO";
const MAIN_REPLY: &str = "Main conversation ready";
const FIRST_CHILD_REPLY: &str = "First child conversation ready";
const SECOND_CHILD_REPLY: &str = "Second child conversation ready";
const MAIN_DRAFT: &str = "MAIN-LOCAL-DRAFT";
const FIRST_CHILD_DRAFT: &str = "FIRST-CHILD-LOCAL-DRAFT";
const SECOND_CHILD_DRAFT: &str = "SECOND-CHILD-LOCAL-DRAFT";

fn editor_has_focus(output: &Arc<Mutex<Vec<u8>>>) -> bool {
    let captured = output.lock().unwrap();
    let terminal = String::from_utf8_lossy(&captured);
    let end = terminal
        .rfind("\x1b[?2026l")
        .map_or(terminal.len(), |end| end + "\x1b[?2026l".len());
    let terminal = &terminal[..end];
    terminal.rfind("\x1b[?25h") > terminal.rfind("\x1b[?25l")
}

async fn displayed_version(
    client: &LocalAgentClient,
    target: ExecutionRef,
    view: ViewRef,
    after: u64,
) -> u64 {
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(client).await;
            let locations = rows
                .iter()
                .flat_map(|row| &row.views)
                .filter(|location| location.view == view)
                .collect::<Vec<_>>();
            if let [location] = locations.as_slice()
                && location.execution == target
                && location.version > after
            {
                return location.version;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("View must display the selected full execution {target:?}"))
}

async fn open_family_member(
    pty: &mut PtyView,
    client: &LocalAgentClient,
    target: ExecutionRef,
    title: &str,
    reply: &str,
    view: ViewRef,
    after: u64,
) -> u64 {
    let panel_title = if title == MAIN_TASK { "main" } else { title };
    let already_visible = catalog(client).await.iter().any(|row| {
        row.execution == target && row.views.iter().any(|location| location.view == view)
    });
    pty.send(b"\x1b[B");
    pty.wait_for("family list receives focus", |frame| {
        family_rows_are_visible(frame)
            && family_rows(frame)
                .iter()
                .any(|line| line.starts_with(SELECTED_MARK))
    })
    .await;
    for _ in 0..6 {
        let frame = pty.frame();
        if family_rows(&frame)
            .iter()
            .any(|line| line.starts_with(SELECTED_MARK) && line.contains(panel_title))
        {
            pty.send(b"\r");
            let version = displayed_version(
                client,
                target,
                view,
                if already_visible {
                    after.saturating_sub(1)
                } else {
                    after
                },
            )
            .await;
            let captured = Arc::clone(&pty.output);
            let opened = pty
                .wait_for("selected conversation is rendered", |frame| {
                    !frame.contains("Agents · local")
                        && frame.contains(reply)
                        && [
                            (MAIN_TASK, MAIN_DRAFT),
                            (FIRST_CHILD_TASK, FIRST_CHILD_DRAFT),
                            (SECOND_CHILD_TASK, SECOND_CHILD_DRAFT),
                        ]
                        .iter()
                        .filter(|(agent, _)| *agent != title)
                        .all(|(_, draft)| !frame.contains(draft))
                        && family_rows_are_visible(frame)
                        && editor_has_focus(&captured)
                })
                .await;
            assert!(
                family_rows_are_visible(&opened) && editor_has_focus(&pty.output),
                "opening {title} must retain the family panel and focus its editor: {opened}"
            );
            return version;
        }
        let rows = family_rows(&frame);
        let selected = rows
            .iter()
            .position(|line| line.starts_with(SELECTED_MARK))
            .unwrap();
        let target_row = rows
            .iter()
            .position(|line| line.contains(panel_title))
            .unwrap();
        let previous_title = ["main", FIRST_CHILD_TASK, SECOND_CHILD_TASK]
            .into_iter()
            .find(|title| rows[selected].contains(title))
            .unwrap();
        pty.send(if selected < target_row { b"j" } else { b"k" });
        pty.wait_for("family selection moves to the next member", |frame| {
            family_rows(frame)
                .iter()
                .any(|line| line.starts_with(SELECTED_MARK) && !line.contains(previous_title))
        })
        .await;
    }
    panic!("family did not select {title}: {}", pty.frame());
}

async fn continue_waiting(
    client: &LocalAgentClient,
    execution: ExecutionRef,
    label: &str,
) -> Box<LiveChatSnapshot> {
    let receipt = client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                input: UserInput::Text {
                    text: format!("Continue {label} and wait for bounded context."),
                    contexts: Vec::new(),
                    paths: Vec::new(),
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    assert!(receipt.error.is_none());
    wait_snapshot(
        client,
        execution,
        "same Agent waits for context",
        |current| {
            current.conversation.chat_loop.state == tickloop_agent_protocol::LoopState::Running
                && current.events.iter().any(|event| {
                    matches!(&event.kind,
                tickloop_agent_protocol::EventKind::ToolStarted { name, .. }
                    if name == "Bash")
                })
        },
    )
    .await
}

async fn release_family_boundary(path: &std::path::Path) {
    use std::os::fd::FromRawFd;
    let path = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let descriptor =
                unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
            if descriptor >= 0 {
                let mut file = unsafe { std::fs::File::from_raw_fd(descriptor) };
                file.write_all(b"Released owned tool boundary\n").unwrap();
                break;
            }
            let error = std::io::Error::last_os_error();
            assert_eq!(error.raw_os_error(), Some(libc::ENXIO), "{error}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the test-owned command must open its FIFO");
}

async fn run_family_navigation(fixture: &PrivateFixture) {
    let waiting_reply = |id: &str, text: &str| {
        chat_reply(
            Some((
                "Bash",
                id,
                json!({"command":format!("cat {id}.pipe"),"timeout":300_000,"description":"Hold the controlled read-only tool boundary"}),
            )),
            text,
        )
    };
    for id in [
        "main-context-wait",
        "first-context-wait",
        "second-context-wait",
        "second-follow-up-wait",
        "main-follow-up-wait",
        "first-follow-up-wait",
    ] {
        let path = std::ffi::CString::new(
            fixture
                .path(&format!("work/{id}.pipe"))
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    let responses = vec![
        chat_reply(
            Some((
                "Subagent",
                "family-first-child",
                json!({"task":FIRST_CHILD_TASK,"name":"family-one","description":FIRST_CHILD_TASK}),
            )),
            "",
        ),
        chat_reply(None, FIRST_CHILD_REPLY),
        chat_reply(
            Some((
                "Subagent",
                "family-second-child",
                json!({"task":SECOND_CHILD_TASK,"name":"family-two","description":SECOND_CHILD_TASK}),
            )),
            "",
        ),
        chat_reply(None, SECOND_CHILD_REPLY),
        chat_reply(None, MAIN_REPLY),
        chat_reply(Some(("ListSubagents", "main-todo", json!({}))), ""),
        waiting_reply("main-context-wait", "Main remains busy"),
        chat_reply(Some(("ListSubagents", "first-todo", json!({}))), ""),
        waiting_reply("first-context-wait", "First child remains busy"),
        chat_reply(Some(("ListSubagents", "second-todo", json!({}))), ""),
        waiting_reply("second-context-wait", "Second child remains busy"),
        waiting_reply(
            "second-follow-up-wait",
            "Selected second child received context",
        ),
        waiting_reply("main-follow-up-wait", "Selected Main received context"),
        waiting_reply(
            "first-follow-up-wait",
            "Selected first child received context",
        ),
    ];
    let (base_url, mut requests) = model_http_support::serve_responses(
        responses.into_iter().map(|reply| (200, reply)).collect(),
    )
    .await;
    fixture.set_chat_model(&base_url);
    let mut pty = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    pty.wait_for("family View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    pty.send(format!("{MAIN_TASK}\r").as_bytes());
    let rows = tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let rows = catalog(&client).await;
            if rows.len() == 3
                && rows
                    .iter()
                    .all(|row| row.state == tickloop_agent_protocol::LoopState::Idle)
            {
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "two real mapped delegations must finish: {error}; frame: {}",
            pty.frame()
        )
    });
    let main = rows.iter().find(|row| row.title == MAIN_TASK).unwrap();
    let first = rows
        .iter()
        .find(|row| row.title == FIRST_CHILD_TASK)
        .unwrap();
    let second = rows
        .iter()
        .find(|row| row.title == SECOND_CHILD_TASK)
        .unwrap();
    assert_eq!(first.parent, Some(main.execution));
    assert_eq!(second.parent, Some(main.execution));
    let main_execution = main.execution;
    let first_execution = first.execution;
    let second_execution = second.execution;
    let view = main.views[0].view;
    let mut version = main.views[0].version;
    let first_metadata = serde_json::to_value(
        snapshot(&client, first_execution)
            .await
            .conversation
            .delegation,
    )
    .unwrap();
    let second_metadata = serde_json::to_value(
        snapshot(&client, second_execution)
            .await
            .conversation
            .delegation,
    )
    .unwrap();
    let captured = Arc::clone(&pty.output);
    pty.wait_for("Main completed editor and family summary", |frame| {
        frame.contains(MAIN_REPLY)
            && TURN_SUMMARY.is_match(frame)
            && frame.contains("Subagents 2")
            && editor_has_focus(&captured)
    })
    .await;
    pty.resize(38, 24).await;
    pty.send(b"\x1b[B");
    let list_focused = pty
        .wait_for("fallback list hint follows the list focus", |frame| {
            frame
                .lines()
                .last()
                .is_some_and(|line| line.contains("enter open · space fold"))
        })
        .await;
    let rows = list_focused.lines().collect::<Vec<_>>();
    let entry = rows
        .iter()
        .position(|line| line.contains("Subagents 2"))
        .unwrap();
    assert!(
        rows[entry + 1].starts_with(&format!("{SELECTED_MARK} "))
            && rows[entry + 1].contains("main"),
        "the first Down lands on the viewed member: {list_focused}"
    );
    assert!(!list_focused.contains("items"));
    eprintln!("[fallback-family-focused]\n{list_focused}\n[/fallback-family-focused]");
    pty.send(b"\x1b");
    pty.wait_for("fallback focus returns to input", |_| {
        editor_has_focus(&captured)
    })
    .await;
    pty.resize(TERMINAL_WIDTH, TERMINAL_HEIGHT).await;
    super::unified_footer::click_category(&mut pty, "Subagents 2");
    pty.wait_for("mouse collapses the completed family", |frame| {
        frame.contains("Subagents 2") && !family_rows_are_visible(frame)
    })
    .await;
    assert!(!pty.frame().contains("reply completed"));
    pty.send(b"\x1b[B");
    pty.wait_for("Down unfolds the family and enters its list", |frame| {
        family_rows_are_visible(frame)
            && frame
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .is_some_and(|line| line.contains("enter open · space fold"))
    })
    .await;
    pty.send(b"\x1b");
    pty.wait_for("Esc returns editor focus", |_| editor_has_focus(&captured))
        .await;
    super::unified_footer::click_category(&mut pty, "Subagents 2");
    pty.wait_for("mouse collapses the family again", |frame| {
        frame.contains("Subagents 2") && !family_rows_are_visible(frame)
    })
    .await;
    super::unified_footer::click_category(&mut pty, "Subagents 2");
    pty.wait_for("mouse reopens the same family", |frame| {
        family_rows_are_visible(frame) && !frame.contains("enter open · space fold")
    })
    .await;
    pty.wait_for("the click leaves editor focus", |_| {
        editor_has_focus(&captured)
    })
    .await;
    pty.send(MAIN_DRAFT.as_bytes());
    pty.wait_for("Main draft remains local", |frame| {
        frame.contains(MAIN_DRAFT)
    })
    .await;
    version = open_family_member(
        &mut pty,
        &client,
        main_execution,
        MAIN_TASK,
        MAIN_REPLY,
        view,
        version,
    )
    .await;
    assert!(pty.frame().contains(MAIN_DRAFT));
    version = open_family_member(
        &mut pty,
        &client,
        first_execution,
        FIRST_CHILD_TASK,
        FIRST_CHILD_REPLY,
        view,
        version,
    )
    .await;
    pty.send(FIRST_CHILD_DRAFT.as_bytes());
    pty.wait_for("first child draft remains local", |frame| {
        frame.contains(FIRST_CHILD_DRAFT)
    })
    .await;
    version = open_family_member(
        &mut pty,
        &client,
        main_execution,
        MAIN_TASK,
        MAIN_REPLY,
        view,
        version,
    )
    .await;
    assert!(pty.frame().contains(MAIN_DRAFT));
    assert!(!pty.frame().contains(FIRST_CHILD_DRAFT));
    version = open_family_member(
        &mut pty,
        &client,
        second_execution,
        SECOND_CHILD_TASK,
        SECOND_CHILD_REPLY,
        view,
        version,
    )
    .await;
    pty.send(SECOND_CHILD_DRAFT.as_bytes());
    pty.wait_for("second child draft remains local", |frame| {
        frame.contains(SECOND_CHILD_DRAFT)
    })
    .await;
    let initial_requests = (0..5)
        .map(|_| requests.try_recv().unwrap().1)
        .collect::<Vec<_>>();
    assert!(
        initial_requests[0]["tools"]
            .to_string()
            .contains("Subagent")
    );
    assert!(
        initial_requests[1]["messages"]
            .to_string()
            .contains(FIRST_CHILD_TASK)
    );
    assert!(
        initial_requests[3]["messages"]
            .to_string()
            .contains(SECOND_CHILD_TASK)
    );

    let main_wait = continue_waiting(&client, main_execution, "Main").await;
    let first_wait = continue_waiting(&client, first_execution, "first child").await;
    let second_wait = continue_waiting(&client, second_execution, "second child").await;
    for _ in 0..6 {
        requests.try_recv().unwrap();
    }
    let runs = [
        (main_execution, main_wait.conversation.chat_loop.run_id),
        (first_execution, first_wait.conversation.chat_loop.run_id),
        (second_execution, second_wait.conversation.chat_loop.run_id),
    ];
    for (target, title, reply, draft) in [
        (main_execution, MAIN_TASK, MAIN_REPLY, MAIN_DRAFT),
        (
            first_execution,
            FIRST_CHILD_TASK,
            FIRST_CHILD_REPLY,
            FIRST_CHILD_DRAFT,
        ),
        (
            second_execution,
            SECOND_CHILD_TASK,
            SECOND_CHILD_REPLY,
            SECOND_CHILD_DRAFT,
        ),
        (main_execution, MAIN_TASK, MAIN_REPLY, MAIN_DRAFT),
    ] {
        version = open_family_member(&mut pty, &client, target, title, reply, view, version).await;
        assert!(pty.frame().contains(draft));
        assert_eq!(catalog(&client).await.len(), 3);
    }
    version = open_family_member(
        &mut pty,
        &client,
        second_execution,
        SECOND_CHILD_TASK,
        SECOND_CHILD_REPLY,
        view,
        version,
    )
    .await;
    pty.send(b"\x15\x1b[D");
    pty.wait_for("busy family enters global overview", |frame| {
        frame.contains("Agents · local")
    })
    .await;
    let overview = pty.frame();
    assert!(overview.contains(MAIN_TASK), "{overview}");
    assert!(
        !overview.contains(FIRST_CHILD_TASK) && !overview.contains(SECOND_CHILD_TASK),
        "{overview}"
    );
    select_overview_row(&mut pty, MAIN_TASK).await;
    pty.send(b"\r");
    version = displayed_version(&client, second_execution, view, version).await;
    pty.wait_for("overview returns to second child editor", |frame| {
        frame.contains("@family-two")
            && frame.contains(SECOND_CHILD_REPLY)
            && !frame.contains("Agents · local")
    })
    .await;
    assert!(editor_has_focus(&pty.output));
    for (target, title, reply, follow_up) in [
        (
            second_execution,
            SECOND_CHILD_TASK,
            SECOND_CHILD_REPLY,
            "ONLY-SECOND-CHILD-CONTEXT",
        ),
        (main_execution, MAIN_TASK, MAIN_REPLY, "ONLY-MAIN-CONTEXT"),
        (
            first_execution,
            FIRST_CHILD_TASK,
            FIRST_CHILD_REPLY,
            "ONLY-FIRST-CHILD-CONTEXT",
        ),
    ] {
        if target != second_execution {
            version =
                open_family_member(&mut pty, &client, target, title, reply, view, version).await;
        }
        pty.send(format!("\x15{follow_up}\r").as_bytes());
        let id = if target == main_execution {
            "main-context-wait"
        } else if target == first_execution {
            "first-context-wait"
        } else {
            "second-context-wait"
        };
        wait_snapshot(&client,target,"follow-up is admitted before releasing the tool",|current| {
            current.pending_inputs.iter().any(|input|input.input == follow_up)
                || current.conversation.messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,tickloop_agent_protocol::Content::Text {text} if text==follow_up))
        }).await;
        release_family_boundary(&fixture.path(&format!("work/{id}.pipe"))).await;
        let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(request["messages"].to_string().contains(follow_up));
        for execution in [main_execution, first_execution, second_execution] {
            let current = snapshot(&client, execution).await;
            let has_context = current.conversation.messages.iter().any(|message| {
                message.content.iter().any(|content| {
                    matches!(content,
                    tickloop_agent_protocol::Content::Text { text } if text == follow_up)
                })
            });
            assert_eq!(
                has_context,
                execution == target,
                "context must follow the selected execution"
            );
        }
    }
    for (execution, run) in runs {
        let current = snapshot(&client, execution).await;
        assert_eq!(current.conversation.chat_loop.run_id, run);
        assert_eq!(
            current.conversation.chat_loop.state,
            tickloop_agent_protocol::LoopState::Running
        );
    }
    assert_eq!(
        serde_json::to_value(
            snapshot(&client, first_execution)
                .await
                .conversation
                .delegation
        )
        .unwrap(),
        first_metadata
    );
    assert_eq!(
        serde_json::to_value(
            snapshot(&client, second_execution)
                .await
                .conversation
                .delegation
        )
        .unwrap(),
        second_metadata
    );
    assert!(requests.try_recv().is_err());
    pty.close().await;
}

#[tokio::test]
async fn actual_cli_family_navigation_preserves_identity_drafts_and_editor_focus_while_busy() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_family_navigation(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
