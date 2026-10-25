use super::*;
use tickloop_agent_protocol::{GoalSnapshot, GoalStatus};
#[cfg(test)]
use tickloop_agent_settings::local_agent::LocalAgent;

#[test]
fn goal_set_history_shows_its_recorded_condition_on_one_clipped_line() {
    let chat = Uuid::new_v4();
    let mut snapshot = super::monitor_tests::chat_snapshot(chat);
    snapshot.goal = Some(goal());
    snapshot.goal_history = vec![tickloop_agent_protocol::GoalHistoryEntry {
        after_message_id: None,
        objective: "不用tick查询到特斯拉现在的夜盘股价\n第二行\t尾部\x1b".into(),
        notice: tickloop_agent_protocol::GoalNotice::Set,
        timestamp: "2026-10-23T00:00:00Z".into(),
    }];
    let console = Console::test_interactive_for_chat(chat);
    console.state.lock().unwrap().resize(40, 25);
    super::super::workflows::write_public_history(&console, &snapshot).unwrap();
    let live = Console::test_interactive_for_chat(chat);
    live.write_pieces(&super::super::goal_view::notice_pieces(
        &snapshot.goal_history[0].notice,
        &snapshot.goal_history[0].objective,
    ))
    .unwrap();
    for width in [40, 18, 80, 14, 40] {
        let rows = console
            .state
            .lock()
            .unwrap()
            .scrollback
            .frame(width, 100)
            .lines
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        let live_rows = live
            .state
            .lock()
            .unwrap()
            .scrollback
            .frame(width, 100)
            .lines
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(rows, live_rows, "width {width}");
        assert_eq!(rows.len(), 1, "width {width}: {rows:?}");
        assert!(unicode_width::UnicodeWidthStr::width(rows[0].as_str()) <= width);
        if width == 40 {
            assert_eq!(rows, ["● Goal set 不用tick查询到特斯拉现在的夜…"]);
        }
        if width == 80 {
            assert_eq!(
                rows,
                ["● Goal set 不用tick查询到特斯拉现在的夜盘股价 第二行 尾部"]
            );
        }
    }
}

#[test]
fn goal_event_clipping_keeps_whole_emoji_and_restores_text_when_widened() {
    for emoji in ["👩‍💻", "👍🏽", "🇨🇳"] {
        let console = Console::test_interactive_for_chat(Uuid::new_v4());
        console
            .write_pieces(&super::super::goal_view::notice_pieces(
                &tickloop_agent_protocol::GoalNotice::Set,
                &emoji.repeat(3),
            ))
            .unwrap();
        for (width, expected) in [
            (14, format!("● Goal set {emoji}…")),
            (30, format!("● Goal set {}", emoji.repeat(3))),
            (1, "…".into()),
        ] {
            let rows = console
                .state
                .lock()
                .unwrap()
                .scrollback
                .frame(width, 100)
                .lines
                .into_iter()
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            assert_eq!(rows, [expected], "width {width}");
        }
    }
}

#[test]
fn goal_history_uses_each_events_condition_after_replacement() {
    use tickloop_agent_protocol::{GoalHistoryEntry, GoalNotice, GoalPause};
    let chat = Uuid::new_v4();
    let mut snapshot = super::monitor_tests::chat_snapshot(chat);
    snapshot.goal = Some(goal());
    snapshot.goal_history = [
        ("First condition", GoalNotice::Set),
        ("First condition", GoalNotice::Completed),
        ("Replacement condition", GoalNotice::Set),
        (
            "Replacement condition",
            GoalNotice::Paused {
                by: GoalPause::User,
            },
        ),
        ("Replacement condition", GoalNotice::Cleared),
    ]
    .into_iter()
    .map(|(objective, notice)| GoalHistoryEntry {
        after_message_id: None,
        objective: objective.into(),
        notice,
        timestamp: "2026-10-23T00:00:00Z".into(),
    })
    .collect();
    let console = Console::test_interactive_for_chat(chat);
    super::super::workflows::write_public_history(&console, &snapshot).unwrap();
    let text = console.test_conversation_text();
    assert_eq!(
        text.lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>(),
        [
            "● Goal set First condition",
            "● Goal achieved First condition",
            "● Goal set Replacement condition",
            "● Goal paused · /goal resume",
            "● Goal cleared",
        ]
    );
}

#[tokio::test]
async fn persisted_goal_continuation_keeps_model_text_and_reopens_as_one_event() {
    use tickloop_agent_protocol::{GoalAction, Role as MessageRole, TriggerSource};
    use tickloop_agent_runtime::Agent;
    use tickloop_agent_test_support::{ScriptedModel, config, text};
    let root = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("Progress recorded")]));
    let agent = Agent::open(root.path().into(), config(), model.clone()).unwrap();
    agent.hold_goal_continuations_for_tests();
    agent
        .trust_working_directory(folder.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent
        .manage_goal(
            chat,
            GoalAction::Set {
                objective: "Ship the release".into(),
                token_budget: None,
                replace: false,
            },
        )
        .await
        .unwrap();
    let goal = agent.snapshot(chat).await.unwrap().goal.unwrap();
    let receipt = agent.deliver_trigger_for_tests(chat, TriggerSource::GoalContinuation {
        goal_id: goal.id, after_run: Uuid::new_v4().into(), objective_revision: goal.objective_revision,
    }, "Continue working toward the active goal of this Chat.\n<objective>Ship the release</objective>\nUsage\nContinuation behavior").await.unwrap();
    let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.chat_loop.state == tickloop_agent_protocol::LoopState::Idle
                && snapshot
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Assistant)
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let raw = snapshot.messages.iter().find(|message| message.role == MessageRole::User && message.content.iter().any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text.contains("<objective>")))).unwrap().clone();
    assert!(
        model
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.messages.contains(&raw))
    );
    assert!(snapshot.trigger_origins.contains_key(&receipt.run_id));
    assert!(
        agent
            .resume_chat(chat)
            .await
            .unwrap()
            .trigger_origins
            .contains_key(&receipt.run_id)
    );
    let live = Console::test_interactive_for_chat(chat.into());
    super::super::consume_input_echo_from_snapshot(&live, &snapshot, receipt.run_id.into())
        .unwrap();
    assert_eq!(
        live.test_conversation_text()
            .matches("↻ Continuing Goal")
            .count(),
        1
    );
    agent.shutdown().await;

    let restored = Agent::open(
        root.path().into(),
        config(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert!(
        snapshot.messages.contains(&raw),
        "Transcript keeps the full prompt"
    );
    assert!(snapshot.trigger_origins.contains_key(&receipt.run_id));
    let replay = Console::test_interactive_for_chat(chat.into());
    super::super::workflows::write_public_history(&replay, &snapshot).unwrap();
    super::super::consume_input_echo_from_snapshot(&replay, &snapshot, receipt.run_id.into())
        .unwrap();
    let history = replay.test_conversation_text();
    assert_eq!(history.matches("↻ Continuing Goal").count(), 1);
    assert!(!history.contains("<objective>"));
    assert!(!history.contains("Continuation behavior"));
    restored.shutdown().await;
}

#[test]
fn goal_continuation_live_and_reopened_history_show_one_event() {
    use tickloop_agent_protocol::{Content, Message, Role as MessageRole, TriggerIdentity};
    let chat = Uuid::new_v4();
    let run = Uuid::new_v4();
    let mut snapshot = super::monitor_tests::chat_snapshot(chat);
    snapshot.messages = vec![Message { role: MessageRole::User, content: vec![Content::Text {
        text: "Continue working toward the active goal of this Chat.\n<objective>Ship it</objective>\nUsage\nContinuation behavior".into(),
    }] }];
    snapshot.message_runs.insert(0, run.into());
    snapshot.trigger_origins.insert(
        run.into(),
        TriggerIdentity::GoalContinuation {
            goal_id: Uuid::new_v4(),
            after_run: Uuid::new_v4().into(),
        },
    );
    let live = Console::test_interactive_for_chat(chat);
    for _ in 0..2 {
        super::super::consume_input_echo_from_snapshot(&live, &snapshot, run).unwrap();
    }
    let replay = Console::test_interactive_for_chat(chat);
    super::super::workflows::write_public_history(&replay, &snapshot).unwrap();
    super::super::consume_input_echo_from_snapshot(&replay, &snapshot, run).unwrap();
    for console in [&live, &replay] {
        let text = console.test_conversation_text();
        assert_eq!(
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>(),
            ["↻ Continuing Goal"]
        );
        assert!(console.test_conversation_rows().iter().all(|line| {
            line.fill != Some(Role::UserMessageBg)
                && line
                    .spans
                    .iter()
                    .all(|span| span.role == Some(Role::MutedText))
        }));
    }
    let detailed = super::super::conversation::project(&snapshot);
    assert_eq!(detailed.blocks.len(), 1);
    assert!(matches!(
        detailed.blocks[0].kind,
        super::super::conversation::ConversationBlockKind::GoalContinuation
    ));

    snapshot.trigger_origins.clear();
    let user = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
    assert!(
        user.contains("<objective>Ship it</objective>"),
        "identical user-written text stays visible"
    );
}

#[test]
fn scheduled_skill_live_and_history_show_the_users_instruction() {
    use tickloop_agent_protocol::{Content, Message, Role as MessageRole, TriggerIdentity};
    let chat = Uuid::new_v4();
    let run = Uuid::new_v4();
    let mut snapshot = super::monitor_tests::chat_snapshot(chat);
    snapshot.messages = vec![Message { role: MessageRole::User, content: vec![Content::Text {
        text: "Scheduled task 00000000-0000-0000-0000-000012345678: invoke the `review` Skill with the Skill tool with the arguments `main`.\n\nThis is the final run of this scheduled task: it was created seven days ago and is deleted after this run. Do not schedule it again; tell the user it ended and that they can start a new /loop to continue.".into(),
    }] }];
    snapshot.message_runs.insert(0, run.into());
    snapshot.trigger_origins.insert(
        run.into(),
        TriggerIdentity::ScheduledTask {
            task_id: uuid::Uuid::from_u128(0x1234_5678),
            scheduled_for_ms: 1,
            wrappers: vec![
                tickloop_agent_protocol::ScheduledInputWrapper::Skill {
                    instruction: "/review main".into(),
                },
                tickloop_agent_protocol::ScheduledInputWrapper::FinalRun,
            ],
        },
    );
    for history in [false, true] {
        let console = Console::test_interactive_for_chat(chat);
        if history {
            super::super::workflows::write_public_history(&console, &snapshot).unwrap();
        }
        super::super::consume_input_echo_from_snapshot(&console, &snapshot, run).unwrap();
        let text = console.test_conversation_text();
        assert!(text.contains("/review main"), "{text}");
        assert!(!text.contains("Skill tool"));
        assert!(!text.contains("final run"));
        assert_eq!(text.matches("/review main").count(), 1);
    }
}

#[test]
fn scheduled_live_and_history_preserve_unrecorded_wrappers() {
    use tickloop_agent_protocol::{Content, Message, Role as MessageRole, TriggerIdentity};
    for instruction in [
        tickloop_agent_protocol::scheduled_skill_prompt(
            &uuid::Uuid::from_u128(0x1234_5678).to_string(),
            "review",
            "main",
        ),
        tickloop_agent_protocol::loop_instructions_prompt(
            "loop.md. Hidden instruction",
            "Visible instruction",
        ),
        tickloop_agent_protocol::loop_instructions_error_prompt(
            &uuid::Uuid::from_u128(0x1234_5678).to_string(),
            "permission denied",
        ),
        tickloop_agent_protocol::final_scheduled_run_prompt("Check the release"),
    ] {
        let chat = Uuid::new_v4();
        let run = Uuid::new_v4();
        let mut snapshot = super::monitor_tests::chat_snapshot(chat);
        snapshot.messages = vec![Message {
            role: MessageRole::User,
            content: vec![Content::Text {
                text: instruction.clone(),
            }],
        }];
        snapshot.message_runs.insert(0, run.into());
        snapshot.trigger_origins.insert(
            run.into(),
            TriggerIdentity::ScheduledTask {
                task_id: uuid::Uuid::from_u128(0x1234_5678),
                scheduled_for_ms: 1,
                wrappers: Vec::new(),
            },
        );
        let mut original = snapshot.clone();
        original.trigger_origins.clear();
        for history in [false, true] {
            let console = Console::test_interactive_for_chat(chat);
            let expected = Console::test_interactive_for_chat(chat);
            if history {
                super::super::workflows::write_public_history(&console, &snapshot).unwrap();
                super::super::workflows::write_public_history(&expected, &original).unwrap();
            }
            super::super::consume_input_echo_from_snapshot(&console, &snapshot, run).unwrap();
            super::super::consume_input_echo_from_snapshot(&expected, &original, run).unwrap();
            assert_eq!(
                console.test_conversation_text(),
                expected.test_conversation_text()
            );
        }
    }
}

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
        state.chat = Some(status.id.into());
        state.prompt_enabled = true;
        state.monitor.set_chat_status(Arc::new(status));
        assert!(state.monitor.is_shown());
        assert_eq!(state.monitor.total(), 1);
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            Some(InputEvent::TickAction { .. })
        ));
        assert!(state.monitor.is_goal_selected());
        assert!(state.monitor.focused);
        assert!(state.monitor_visible());
        let frame = state
            .monitor
            .frame(usize::from(width), 12, state.display, 0);
        let text = frame
            .lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Pursuing goal"));
        assert!(text.contains('…'));
        assert!(text.contains("1m"));
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
fn status_bar_omits_every_goal_state_in_wide_and_short_console_frames() {
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
        for (width, height) in [(140, 10), (140, 30), (60, 10), (60, 30)] {
            let mut state = State::new(true, width, height);
            state
                .status_area
                .update("Auto".into(), "fixture".into(), Some((21000, 100000)), None);
            let mut goal = goal();
            goal.status = goal_status.clone();
            let status = status(goal);
            state.chat = Some(status.id.into());
            state.monitor.set_chat_status(Arc::new(status));
            let frame = state.frame().0.join("\n");
            assert!(!frame.contains(expected), "{width}x{height}: {frame}");
            let status = frame.lines().find(|line| line.contains("fixture")).unwrap();
            assert!(!status.to_lowercase().contains("goal"), "{status}");
            assert!(status.contains("fixture"), "{width}x{height}: {frame}");
            if width == 140 {
                assert!(status.contains("Context 21% used"), "{frame}");
            }
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
            let frame = wait("Goal paused")?;
            assert!(!frame.contains("Goal paused (/goal resume)"), "{frame}");
            println!("GOAL_PTY_{renderer}_PAUSED\n{frame}");
            Ok(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        result.unwrap();
    }
}

#[test]
fn goal_monitor_render_evidence() {
    let mut monitor = Monitor::default();
    let mut goal = goal();
    goal.token_budget = None;
    monitor.set_chat_status(Arc::new(status(goal)));
    monitor.focus(false, None);
    let frame = monitor.frame(48, 9, DisplayProfile::default(), 0);
    let screen = frame
        .lines
        .iter()
        .map(|line| line.text().trim_end().to_owned())
        .collect::<Vec<_>>();
    println!("{}", screen.join("\n"));
    assert_eq!(
        screen,
        [
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "Pursuing goal                                 1m",
            "◎ Verify the release and record acceptance evid…"
        ]
    );
    assert!(
        frame.lines[7]
            .spans
            .iter()
            .any(|span| span.text.contains("Pursuing goal") && span.role == Some(Role::Accent))
    );
    assert_eq!(frame.lines[7].fill, None);
    assert_eq!(frame.lines[8].fill, Some(Role::Selection));
}

#[test]
fn goal_only_monitor_counts_as_content_for_periodic_refresh() {
    let console = Console::test_interactive_for_chat(Uuid::from_u128(707));
    console.update_monitor_chat(status(goal())).unwrap();
    assert!(
        console.monitor_has_entries().unwrap(),
        "Goal-only Monitor must keep periodic refresh enabled"
    );
}

#[test]
fn paused_triggers_keep_model_in_the_same_status_line() {
    for (width, height) in [(140, 10), (140, 30), (60, 10), (60, 30)] {
        let mut state = State::new(true, width, height);
        state
            .status_area
            .update("Auto".into(), "fixture".into(), Some((21000, 100000)), None);
        let mut status = status(goal());
        status.chat_loop.receiving = tickloop_agent_protocol::Receiving::Paused {
            reason: tickloop_agent_protocol::PauseReason::HistoryLoaded,
        };
        state.chat = Some(status.id.into());
        state.monitor.set_chat_status(Arc::new(status));
        let frame = state.frame().0.join("\n");
        let line = frame
            .lines()
            .find(|line| line.contains("Triggers paused"))
            .unwrap();
        assert!(line.contains("fixture"), "{width}x{height}: {frame}");
    }
}

#[tokio::test]
async fn goal_budget_input_renders_compact_prefill_and_unit_help() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus};
    use tickloop_agent_protocol::GoalAction;
    let config = tickloop_agent_test_support::config();
    let core = tickloop_agent_runtime::Agent::new_embedded(
        config.clone(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    core.hold_goal_continuations_for_tests();
    let agent = ConsoleExecution::from_embedded(core.clone(), config);
    let chat = agent
        .create_chat(std::env::current_dir().unwrap())
        .await
        .unwrap();
    core.manage_goal(
        chat.into(),
        GoalAction::Set {
            objective: "verify".into(),
            token_budget: Some(1500000),
            replace: false,
        },
    )
    .await
    .unwrap();
    let console = Console::test_interactive_for_chat(chat);
    let mut menus = Menus::default();
    menus
        .goal_command(&agent, &console, chat, "/goal")
        .await
        .unwrap();
    menus
        .choose(&agent, &console, menus.id().unwrap(), Some(3), None)
        .await
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.selected_menu_comment().as_deref(), Some("1.5M"));
        let screen = state.frame().0.join("\n");
        assert!(
            screen.contains("Token budget (e.g. 50K, 1.5M, or none):"),
            "{screen}"
        );
        assert!(screen.contains("1.5M"));
    }
    agent.shutdown().await;
}
