use super::*;

fn family_rows_are_visible(frame: &str) -> bool {
    frame
        .lines()
        .skip_while(|line| !line.contains("Subagents 2"))
        .skip(1)
        .any(|line| {
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
    let already_visible = catalog(client).await.iter().any(|row| {
        row.execution == target && row.views.iter().any(|location| location.view == view)
    });
    pty.send(b"\x1b[B");
    pty.wait_for("family list expands", |frame| {
        family_rows_are_visible(frame)
    })
    .await;
    pty.send(b"\x1b[B");
    pty.wait_for("family list receives focus", |frame| {
        frame.lines().any(|line| line.starts_with('›'))
    })
    .await;
    for _ in 0..6 {
        let frame = pty.frame();
        if frame
            .lines()
            .any(|line| line.starts_with('›') && line.contains(title))
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
        let rows = frame
            .lines()
            .skip_while(|line| !line.contains("Subagents 2"))
            .skip(1)
            .collect::<Vec<_>>();
        let selected = rows.iter().position(|line| line.starts_with('›')).unwrap();
        let target_row = rows.iter().position(|line| line.contains(title)).unwrap();
        let previous_title = [MAIN_TASK, FIRST_CHILD_TASK, SECOND_CHILD_TASK]
            .into_iter()
            .find(|title| rows[selected].contains(title))
            .unwrap();
        pty.send(if selected < target_row { b"j" } else { b"k" });
        pty.wait_for("family selection moves to the next member", |frame| {
            frame
                .lines()
                .any(|line| line.starts_with('›') && !line.contains(previous_title))
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
    let before = snapshot(client, execution).await;
    let receipt = client
        .execute(CommandEnvelope {
            owner_epoch: client.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                target_loop: Some(before.conversation.loops[0].id),
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
            current.conversation.loops[0].state == tickloop_agent::runtime::LoopState::Running
                && current.events.iter().any(|event| {
                    matches!(&event.kind,
                tickloop_agent::runtime::EventKind::ToolStarted { name, .. }
                    if name == "ReceiveAgentMessage")
                })
        },
    )
    .await
}

async fn run_family_navigation(fixture: &PrivateFixture) {
    let waiting_reply = |id: &str, text: &str| {
        chat_reply(
            Some(("ReceiveAgentMessage", id, json!({"timeout_ms":300_000}))),
            text,
        )
    };
    let responses = vec![
        chat_reply(
            Some((
                "DelegateAgent",
                "family-first-child",
                json!({"task":FIRST_CHILD_TASK}),
            )),
            "",
        ),
        chat_reply(None, FIRST_CHILD_REPLY),
        chat_reply(
            Some((
                "DelegateAgent",
                "family-second-child",
                json!({"task":SECOND_CHILD_TASK}),
            )),
            "",
        ),
        chat_reply(None, SECOND_CHILD_REPLY),
        chat_reply(None, MAIN_REPLY),
        chat_reply(
            Some((
                "TodoWrite",
                "main-todo",
                json!({"todos":[{"content":"MAIN-OWN-TODO","status":"in_progress","activeForm":"Main own work"}]}),
            )),
            "",
        ),
        waiting_reply("main-context-wait", "Main remains busy"),
        chat_reply(
            Some((
                "TodoWrite",
                "first-todo",
                json!({"todos":[{"content":"FIRST-OWN-TODO","status":"in_progress","activeForm":"First own work"}]}),
            )),
            "",
        ),
        waiting_reply("first-context-wait", "First child remains busy"),
        chat_reply(
            Some((
                "TodoWrite",
                "second-todo",
                json!({"todos":[{"content":"SECOND-OWN-TODO","status":"in_progress","activeForm":"Second own work"}]}),
            )),
            "",
        ),
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
                    .all(|row| row.state == tickloop_agent::runtime::LoopState::Completed)
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
            && frame.contains("✓ Done")
            && frame.contains("⏺ Subagents 2")
            && editor_has_focus(&captured)
    })
    .await;
    pty.resize(38, 24).await;
    pty.send(b"\x1b[B");
    let entry_focused = pty
        .wait_for("fallback entry hint follows family members", |frame| {
            frame
                .lines()
                .last()
                .is_some_and(|line| line.contains("Enter/Space hide"))
        })
        .await;
    let rows = entry_focused.lines().collect::<Vec<_>>();
    let entry = rows
        .iter()
        .position(|line| line.contains("Subagents 2"))
        .unwrap();
    assert!(
        rows[entry + 1].contains("⏵⏵ Auto · deepseek-flash"),
        "{entry_focused}"
    );
    assert!(!rows[entry + 1].contains("Context"), "{entry_focused}");
    assert!(rows[entry + 2].contains("Main ·"), "{entry_focused}");
    assert!(!entry_focused.contains("items"));
    pty.send(b"\x1b[B");
    let list_focused = pty
        .wait_for("fallback list hint matches its focus", |frame| {
            frame
                .lines()
                .last()
                .is_some_and(|line| line.contains("Enter open · Space fold · Esc entry"))
        })
        .await;
    assert!(!list_focused.contains("Enter/Space hide"));
    eprintln!("[fallback-family-focused]\n{list_focused}\n[/fallback-family-focused]");
    pty.send(b"\x1b\x1b");
    pty.wait_for("fallback focus returns to input", |_| {
        editor_has_focus(&captured)
    })
    .await;
    pty.resize(TERMINAL_WIDTH, TERMINAL_HEIGHT).await;
    super::unified_footer::click_category(&mut pty, "Subagents 2");
    pty.wait_for("mouse collapses the completed family", |frame| {
        frame.contains("Enter/Space expand")
            && frame.contains("⏺ Subagents 2")
            && !family_rows_are_visible(frame)
    })
    .await;
    assert!(!pty.frame().contains("reply completed"));
    pty.send(b"\r");
    pty.wait_for("focused Enter expands the family", |frame| {
        family_rows_are_visible(frame)
            && frame
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .is_some_and(|line| line.contains("Enter/Space hide"))
    })
    .await;
    pty.send(b" ");
    pty.wait_for("focused Space collapses the family", |frame| {
        frame.contains("Enter/Space expand")
            && frame.contains("⏺ Subagents 2")
            && !family_rows_are_visible(frame)
    })
    .await;
    super::unified_footer::click_category(&mut pty, "Subagents 2");
    pty.wait_for("mouse reopens the same family", |frame| {
        family_rows_are_visible(frame)
            && frame
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .is_some_and(|line| line.contains("Enter/Space hide"))
    })
    .await;
    pty.send(b"\x1b");
    pty.wait_for("toggle returns editor focus", |_| {
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
            .contains("DelegateAgent")
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
        (main_execution, main_wait.conversation.loops[0].run_id),
        (first_execution, first_wait.conversation.loops[0].run_id),
        (second_execution, second_wait.conversation.loops[0].run_id),
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
        let own_todo = if target == main_execution {
            "MAIN-OWN-TODO"
        } else if target == first_execution {
            "FIRST-OWN-TODO"
        } else {
            "SECOND-OWN-TODO"
        };
        let working = pty
            .wait_for("the visible Agent owns the live Todo list", |frame| {
                frame.contains(own_todo)
            })
            .await;
        for other in ["MAIN-OWN-TODO", "FIRST-OWN-TODO", "SECOND-OWN-TODO"] {
            assert_eq!(working.contains(other), other == own_todo, "{working}");
        }
        assert!(pty.frame().contains(draft));
        assert_eq!(catalog(&client).await.len(), 3);
    }
    pty.send(b"\x15\x1b[D");
    pty.wait_for("busy family enters global overview", |frame| {
        frame.contains("Agents · local")
    })
    .await;
    let overview = pty.frame();
    assert!(overview.contains("Main · FAMILY-MAIN"), "{overview}");
    select_overview_row(&mut pty, MAIN_TASK).await;
    pty.send(b" ");
    pty.wait_for("overview unfolds the selected Main family", |frame| {
        frame.contains(FIRST_CHILD_TASK) && frame.contains(SECOND_CHILD_TASK)
    })
    .await;
    select_overview_row(&mut pty, SECOND_CHILD_TASK).await;
    pty.send(b"\r");
    version = displayed_version(&client, second_execution, view, version).await;
    pty.wait_for("overview returns to second child editor", |frame| {
        frame.contains(SECOND_CHILD_DRAFT) && !frame.contains("Agents · local")
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
                    tickloop_agent::model::Content::Text { text } if text == follow_up)
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
        assert_eq!(current.conversation.loops[0].run_id, run);
        assert_eq!(
            current.conversation.loops[0].state,
            tickloop_agent::runtime::LoopState::Running
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
