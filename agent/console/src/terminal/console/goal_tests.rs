use super::*;
use tickloop_agent_protocol::{GoalSnapshot, GoalStatus};

#[test]
fn continue_prompt_preserves_typed_and_pasted_messages_before_menu_cancellation_finishes() {
    for pasted in [false, true] {
        for draft in [
            "Which answer did you give?",
            "yes, inspect notes",
            "no, try again",
            "中文追问",
        ] {
            let chat = Uuid::new_v4();
            let console = Console::test_interactive_for_chat(chat);
            let mut status =
                tickloop_agent_protocol::ChatStatus::of(&super::monitor_tests::chat_snapshot(chat));
            status.chat_loop.receiving = tickloop_agent_protocol::Receiving::Paused {
                reason: tickloop_agent_protocol::PauseReason::HistoryLoaded,
            };
            let mut menus = super::super::menus::Menus::default();
            menus.offer_continue(&console, &status).unwrap();
            let id = menus.id().unwrap();
            let mut state = console.state.lock().unwrap();
            state.prompt_enabled = true;
            let event = if pasted {
                state.paste(draft)
            } else {
                let mut characters = draft.chars();
                let event = state.key(KeyEvent::new(
                    KeyCode::Char(characters.next().unwrap()),
                    KeyModifiers::NONE,
                ));
                for character in characters {
                    assert!(
                        state
                            .key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                            .is_none()
                    );
                }
                event
            };
            assert!(matches!(event, Some(InputEvent::Cancel { menu_id }) if menu_id == id));
            assert!(state.menu.is_none());
            assert_eq!(state.editor.draft(), draft);
            assert!(
                matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Prompt { text, .. }) if text == draft)
            );
        }
    }
}

fn goal() -> GoalSnapshot {
    serde_json::from_value(serde_json::json!({
        "id": Uuid::from_u128(707), "objective": "Verify the release and record acceptance evidence",
        "objective_revision": 1, "set_by": "user", "status": {"state": "active"},
        "token_budget": 50000, "tokens_used": 12500, "time_used_ms": 90000,
        "quiet_turns": 0, "created_at_ms": 1, "updated_at_ms": 1
    })).unwrap()
}

fn status(goal: GoalSnapshot) -> tickloop_agent_protocol::ChatStatus {
    let mut snapshot = super::monitor_tests::chat_snapshot(Uuid::from_u128(707));
    snapshot.goal = Some(goal);
    tickloop_agent_protocol::ChatStatus::of(&snapshot)
}

#[test]
fn goal_only_monitor_is_fixed_selectable_and_enter_opens_its_menu() {
    for width in [20, 80, 140] {
        let mut state = State::new(true, width, 30);
        let mut goal = goal();
        goal.objective = "长条件🦀".repeat(100);
        let status = status(goal);
        state.chat = Some(status.id);
        state.prompt_enabled = true;
        state.monitor.set_chat_status(Arc::new(status));
        assert!(state.monitor.is_shown());
        assert_eq!(state.monitor.total(), 0);
        state.monitor.focus(state.monitor_overlay(), None);
        let frame = state
            .monitor
            .frame(usize::from(width), 12, state.display, 0);
        let text = frame
            .lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Goal"));
        assert!(text.contains('…'));
        assert!(text.contains("active"));
        assert!(frame.targets.is_empty(), "Goal is not a Tick member");
        assert!(
            frame
                .lines
                .iter()
                .all(|line| display_width(&line.text()) <= usize::from(width))
        );
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Line(line)) if line == "/goal")
        );
    }
}

#[test]
fn goal_continuation_flashes_once_without_replaying_history_or_usage_updates() {
    let mut monitor = Monitor::default();
    let mut current = goal();
    current.last_continued_at_ms = Some(100);
    monitor.set_chat_status(Arc::new(status(current.clone())));
    assert!(monitor.goal().unwrap().continued_at_ms.is_none());
    current.tokens_used += 1;
    monitor.set_chat_status(Arc::new(status(current.clone())));
    assert!(monitor.goal().unwrap().continued_at_ms.is_none());
    current.last_continued_at_ms = Some(200);
    monitor.set_chat_status(Arc::new(status(current.clone())));
    let since = monitor.goal().unwrap().continued_at_ms.unwrap();
    monitor.set_chat_status(Arc::new(status(current)));
    assert_eq!(monitor.goal().unwrap().continued_at_ms, Some(since));
    assert!(
        monitor
            .next_frame_ms(since, 40, DisplayProfile::default())
            .is_some()
    );
}

#[test]
fn goal_status_reaches_wide_and_short_console_frames() {
    for (goal_status, expected) in [
        (GoalStatus::Active, "Pursuing goal (12.5K / 50K)"),
        (
            GoalStatus::Paused {
                by: tickloop_agent_protocol::GoalPause::User,
            },
            "Goal paused (/goal resume)",
        ),
        (
            GoalStatus::Blocked {
                by: tickloop_agent_protocol::GoalBlock::NoProgress,
            },
            "Goal stalled (/goal resume)",
        ),
        (GoalStatus::BudgetLimited, "Goal unmet (12.5K / 50K tokens)"),
        (GoalStatus::Complete, "Goal achieved (12.5K tokens)"),
    ] {
        for height in [10, 30] {
            let mut state = State::new(true, 140, height);
            state
                .status_area
                .update("Auto".into(), "fixture".into(), None, None);
            let mut goal = goal();
            goal.status = goal_status.clone();
            let status = status(goal);
            state.chat = Some(status.id);
            state.monitor.set_chat_status(Arc::new(status));
            let frame = state.frame().0.join("\n");
            assert!(frame.contains(expected), "{height}: {frame}");
        }
    }
}

#[test]
fn goal_menu_esc_closes_before_it_interrupts_and_full_condition_scrolls() {
    let console = Console::test_interactive_for_chat(Uuid::from_u128(707));
    let menu = Menu {
        id: Uuid::new_v4(),
        title: "Goal".into(),
        context: (0..100)
            .map(|index| format!("Condition line {index}"))
            .collect(),
        options: vec![
            "Edit".into(),
            "Pause".into(),
            "Clear".into(),
            "Adjust token budget".into(),
        ],
        selected: 0,
        confirmation: false,
    };
    console.show_focused_menu(menu).unwrap();
    let mut state = console.state.lock().unwrap();
    state.frame();
    assert!(
        state
            .key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.context_offset > 0);
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        Some(InputEvent::Cancel { .. })
    ));
}

#[test]
fn goal_condition_ctrl_g_preserves_multiline_text_and_draft() {
    let console = Console::test_interactive_for_chat(Uuid::from_u128(707));
    console
        .show_input_menu(
            Menu {
                id: Uuid::new_v4(),
                title: "Edit Goal".into(),
                context: vec![],
                options: vec!["Save".into()],
                selected: 0,
                confirmation: false,
            },
            "Condition",
            "original",
        )
        .unwrap();
    let opened_drafts = Arc::new(Mutex::new(Vec::new()));
    let mut state = console.state.lock().unwrap();
    state.external_editor = ExternalEditorLauncher::Scripted {
        opened_drafts: opened_drafts.clone(),
        outcome: ExternalEditorOutcome::Updated {
            content: "first line\nsecond line".into(),
        },
    };
    state.replace_draft("unsent input".into(), 12);
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .is_none()
    );
    assert_eq!(*opened_drafts.lock().unwrap(), ["original"]);
    assert_eq!(
        state.selected_menu_comment().as_deref(),
        Some("first line\nsecond line")
    );
    assert_eq!(state.editor.draft(), "unsent input");
}

#[test]
#[ignore = "PTY producer launched by goal_ui_pty_records_monitor_menu_and_escape"]
fn goal_ui_pty_fixture() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus};
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let core = tickloop_agent_runtime::Agent::new_embedded(
                tickloop_agent_test_support::config(),
                Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
            );
            core.hold_goal_continuations_for_tests();
            let root =
                PathBuf::from(std::env::var_os("TICKLOOP_DIALOG_HEIGHT_FIXTURE_DIR").unwrap());
            let agent =
                ConsoleExecution::from_embedded(core, tickloop_agent_test_support::config());
            let chat = agent.create_chat(root).await.unwrap();
            let (console, mut input) = Console::start(true).unwrap();
            console.select_chat(Some(chat)).unwrap();
            console.set_prompt_enabled(true).unwrap();
            let mut menus = Menus::default();
            menus
                .goal_command(
                    &agent,
                    &console,
                    chat,
                    "/goal Verify release evidence in the terminal",
                )
                .await
                .unwrap();
            console
                .update_monitor_chat(agent.status(chat).await.unwrap())
                .unwrap();
            console
                .update_status_area("Auto".into(), "fixture".into(), None, None)
                .unwrap();
            console.write("Goal PTY ready\n").unwrap();
            while let Some(event) = input.recv().await {
                match event {
                    InputEvent::Line(line) | InputEvent::Prompt { text: line, .. } => {
                        if line == "/exit" {
                            break;
                        }
                        menus
                            .goal_command(&agent, &console, chat, &line)
                            .await
                            .unwrap();
                        console
                            .update_monitor_chat(agent.status(chat).await.unwrap())
                            .unwrap();
                        console
                            .state
                            .lock()
                            .unwrap()
                            .draw(&mut io::stdout().lock())
                            .unwrap();
                    }
                    InputEvent::Cancel { menu_id } => {
                        menus
                            .choose(&agent, &console, menu_id, None, None)
                            .await
                            .unwrap();
                    }
                    InputEvent::Choice {
                        menu_id,
                        index,
                        comment,
                    } => {
                        menus
                            .choose(&agent, &console, menu_id, Some(index), comment)
                            .await
                            .unwrap();
                    }
                    InputEvent::Eof => break,
                    _ => {}
                }
            }
            agent.shutdown().await;
        });
}

#[test]
fn goal_ui_pty_records_monitor_menu_and_escape() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;
    for renderer in ["inline", "fullscreen"] {
        let directory = tempfile::tempdir().unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 140,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--ignored",
            "--exact",
            "terminal::console::goal_tests::goal_ui_pty_fixture",
            "--nocapture",
        ]);
        command.env_clear();
        command.env("TICKLOOP_DIALOG_HEIGHT_FIXTURE_DIR", directory.path());
        command.env("HOME", directory.path());
        command.env("TICKLOOP_CONFIG_DIR", directory.path().join("config"));
        command.env(
            "TICKLOOP_DAEMON_RUNTIME_DIR",
            directory.path().join("runtime"),
        );
        command.env("TICKLOOP_TMPDIR", directory.path());
        command.env("TICKLOOP_AGENT_TUI", renderer);
        command.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
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
        let wait = |expected: &str| -> Result<String, String> {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let frame = tickloop_agent_test_support::screen::visible_screen(
                    &String::from_utf8_lossy(&output.lock().unwrap()),
                    140,
                    30,
                );
                if frame.contains(expected) {
                    return Ok(frame);
                }
                if Instant::now() > deadline {
                    return Err(format!("Expected {expected}:\n{frame}"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let result = (|| -> Result<(), String> {
            let frame = wait("Goal PTY ready")?;
            assert!(
                frame.contains("Goal") && frame.contains("Pursuing goal"),
                "{frame}"
            );
            println!("GOAL_PTY_{renderer}_MONITOR\n{frame}");
            writer.write_all(b"/goal\r").unwrap();
            writer.flush().unwrap();
            let frame = wait("Adjust token budget")?;
            assert!(frame.contains('❯'), "{frame}");
            println!("GOAL_PTY_{renderer}_MENU\n{frame}");
            writer.write_all(b"\x1b").unwrap();
            writer.flush().unwrap();
            std::thread::sleep(Duration::from_millis(150));
            writer.write_all(b"/goal pause\r").unwrap();
            writer.flush().unwrap();
            let frame = wait("Goal paused (/goal resume)")?;
            println!("GOAL_PTY_{renderer}_PAUSED\n{frame}");
            Ok(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        result.unwrap();
    }
}
