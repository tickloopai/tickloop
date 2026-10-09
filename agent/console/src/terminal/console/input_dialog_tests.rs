use super::*;
use crate::monitor_rows::{MonitorAction, TickIntent};
use tickloop_agent_protocol::ChatSnapshot;

#[test]
fn input_dialog_top_rule_matches_claude_code_in_both_themes() {
    use crate::terminal::style::{Theme, ThemeName};
    for theme in ThemeName::ALL {
        for (width, height) in [(80, 24), (120, 40), (32, 10)] {
            let mut state = State::new(true, width, height);
            state.display.set_theme(theme);
            state.menu = Some(menu(Uuid::new_v4()));
            state.picker_kind = Some(PickerKind::Actions);
            state.picker_presentation.hint = "Enter open · Esc close".into();
            let capacity = state.input_dialog_height();
            let lines = state.styled_frame().0;
            assert_eq!(lines.len(), capacity);
            assert_eq!(lines[0].text(), "▔".repeat(state.width));
            assert_eq!(lines[0].spans[0].role, Some(Role::Permission));
            assert_eq!(lines[1].spans[0].role, Some(Role::DialogTitle));
            assert_eq!(lines.last().unwrap().spans[0].role, Some(Role::MutedText));
            assert_eq!(
                lines[0].resolve(state.display.theme(), state.width).spans[0].style,
                Theme::new(theme).style(Role::Permission)
            );
        }
    }
}

#[tokio::test]
async fn input_dialog_top_rule_covers_every_surface_with_semantic_theme_roles() {
    use crate::terminal::{
        artifacts_picker::ArtifactsPicker,
        conversation_picker::{ConversationEntry, ConversationPickerPurpose},
        execution::ConsoleExecution,
        style::{Theme, ThemeName},
    };
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    for theme in ThemeName::ALL {
        for surface in [
            "settings",
            "permission",
            "question",
            "plan",
            "plan feedback",
            "login",
            "help",
            "resume",
            "artifacts",
            "tasks",
            "permissions",
            "subscriptions",
            "trust",
            "tick confirmation",
        ] {
            let mut state = State::new(true, 80, 24);
            state.chat = Some(chat);
            state.display.set_theme(theme);
            let console = tests::console_with_state(state);
            let id = Uuid::new_v4();
            match surface {
                "settings" => console.show_focused_menu(menu(id)).unwrap(),
                "permission" => {
                    console.set_request_layout(Some(id)).unwrap();
                    let mut page = menu(id);
                    page.confirmation = true;
                    console.show_focused_menu(page).unwrap();
                }
                "question" => console
                    .show_question_menu(
                        menu(id),
                        QuestionMenu {
                            entry_option: None,
                            entry_text: String::new(),
                            multi_select: false,
                            navigable: false,
                        },
                    )
                    .unwrap(),
                "plan" | "plan feedback" => {
                    let mut page = menu(id);
                    page.options = ["Auto", "Ask", "Full Access", "No, keep planning"]
                        .into_iter()
                        .map(String::from)
                        .collect();
                    console
                        .show_plan_menu(page, surface == "plan feedback")
                        .unwrap();
                }
                "login" => console
                    .show_login_menu(menu(id), "https://example.test/device", false)
                    .unwrap(),
                "help" => console.state.lock().unwrap().help_open = true,
                "resume" => console
                    .show_conversation_picker(ConversationPicker::new(
                        ConversationPickerPurpose::Resume,
                        vec![ConversationEntry::new("Saved Chat", &[], &[])],
                    ))
                    .unwrap(),
                "artifacts" => console
                    .show_artifacts_picker(ArtifactsPicker::new(
                        Some(chat),
                        "fixture@example.test".into(),
                        HashSet::new(),
                    ))
                    .unwrap(),
                "tasks" => {
                    console
                        .show_tasks_at(tests::task_snapshot(chat, 2), None)
                        .unwrap();
                }
                "permissions" => {
                    console.state.lock().unwrap().permissions_surface =
                        Some(PermissionsSurface::open(
                            view.clone(),
                            PermissionsReturn::ModePicker { settings: false },
                        ))
                }
                "subscriptions" => console
                    .show_subscriptions(chat, vec![tests::subscription_row(1)])
                    .unwrap(),
                "trust" => {
                    console.state.lock().unwrap().workspace_trust =
                        Some(WorkspaceTrust::new(PathBuf::from("/fixture/project")))
                }
                "tick confirmation" => {
                    console.state.lock().unwrap().monitor.confirmation =
                        Some(super::super::monitor::TickConfirmation {
                            instance: "fixture-tick".into(),
                            action: crate::terminal::tick_actions::builtin::artifact(
                                "fixture-tick",
                                'o',
                            ),
                            prompt: "Confirm fixture action?".into(),
                        });
                }
                _ => unreachable!(),
            }
            let mut state = console.state.lock().unwrap();
            let role = match surface {
                "plan" | "plan feedback" => Role::PlanMode,
                "trust" => Role::Warning,
                _ => Role::Permission,
            };
            let (lines, row, _) = state.styled_frame();
            let expected = match surface {
                "permission" | "question" | "plan" | "plan feedback" | "trust" => 12,
                "help" => 13,
                "settings" | "login" | "resume" => 6,
                "artifacts" | "tasks" | "tick confirmation" => 5,
                "subscriptions" | "permissions" => 4,
                _ => unreachable!(),
            };
            assert_eq!(lines.len(), expected, "{surface}/{theme:?}");
            assert_eq!(lines[0].text(), "▔".repeat(state.width));
            assert_eq!(lines[0].spans[0].role, Some(role), "{surface}/{theme:?}");
            assert_eq!(
                lines[0].resolve(state.display.theme(), state.width).spans[0].style,
                Theme::new(theme).style(role)
            );
            assert!(row < lines.len());
        }
    }
    agent.shutdown().await;
}

#[test]
fn input_dialog_plan_rule_restores_after_question_and_agent_switch() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let plan = Uuid::new_v4();
    let question = Uuid::new_v4();
    console.select_chat(Some(first)).unwrap();
    console.show_plan_menu(menu(plan), false).unwrap();
    console.set_request_layout(Some(question)).unwrap();
    console
        .show_question_menu(
            menu(question),
            QuestionMenu {
                entry_option: None,
                entry_text: String::new(),
                multi_select: false,
                navigable: false,
            },
        )
        .unwrap();
    assert_eq!(
        console.state.lock().unwrap().styled_frame().0[0].spans[0].role,
        Some(Role::Permission)
    );
    console.select_chat(Some(second)).unwrap();
    console.show_focused_menu(menu(Uuid::new_v4())).unwrap();
    assert_eq!(
        console.state.lock().unwrap().styled_frame().0[0].spans[0].role,
        Some(Role::Permission)
    );
    console.select_chat(Some(first)).unwrap();
    console.clear_menu(question).unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(plan));
    assert_eq!(
        console.state.lock().unwrap().styled_frame().0[0].spans[0].role,
        Some(Role::PlanMode)
    );
}

#[test]
fn input_dialog_rule_click_never_submits_a_menu_or_trust_choice() {
    for trust in [false, true] {
        let mut state = State::new(true, 80, 24);
        state.chat = Some(Uuid::new_v4());
        if trust {
            state.workspace_trust = Some(WorkspaceTrust::new(PathBuf::from("/fixture/project")));
        } else {
            state.menu = Some(menu(Uuid::new_v4()));
        }
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let top = state.height - state.input_dialog_height();
        assert!(state.pointer(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: top as u16,
                modifiers: KeyModifiers::NONE
            },
            &mut output
        ));
        assert!(state.pointer_input.is_none());
        assert!(state.menu_submission_pending.is_none());
    }
}

pub(super) fn fixture_ticks() -> Vec<MonitorEntry> {
    (0..2)
        .map(|index| MonitorEntry {
            instance: tick_protocol::InstanceRef::new("fixture", &format!("tick-{index}")).unwrap(),
            group: TickGroup::Artifacts,
            glyph: Glyph::ArtifactTick,
            title: format!("Fixture Tick {index}"),
            kind: "Document".into(),
            status: "read".into(),
            owner: crate::monitor_rows::TickOwner::Unknown,
            updated_at_ms: None,
            resources: None,
            actions: vec![MonitorAction {
                key: 'o',
                label: "open",
                intent: TickIntent::Artifact {
                    id: format!("fixture-tick-{index}"),
                    key: 'o',
                },
            }],
        })
        .collect()
}

#[test]
fn review_regression_focused_ticks_do_not_swallow_request_field_paste() {
    for question in [false, true] {
        let mut state = State::new(true, 120, 40);
        state.chat = Some(Uuid::new_v4());
        state.prompt_enabled = true;
        state.monitor.replace(TickGroup::Artifacts, fixture_ticks());
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            Some(InputEvent::TickAction { action, .. }) if action.key == 'r'
        ));
        let console = tests::console_with_state(state);
        let request = Uuid::new_v4();
        console.set_request_layout(Some(request)).unwrap();
        let mut request_menu = menu(request);
        request_menu.title = if question {
            "Question"
        } else {
            "Permission request"
        }
        .into();
        request_menu.options = vec![
            if question { "Other" } else { "Allow fixture" }.into(),
            "Deny fixture".into(),
        ];
        if question {
            console
                .show_question_menu(
                    request_menu,
                    QuestionMenu {
                        entry_option: Some(0),
                        entry_text: String::new(),
                        multi_select: false,
                        navigable: false,
                    },
                )
                .unwrap();
        } else {
            console
                .show_focused_commentable_menu(request_menu, vec![0])
                .unwrap();
        }
        let mut state = console.state.lock().unwrap();
        assert!(state.monitor.focused && state.monitor_visible());
        state.key(KeyEvent::new(
            if question {
                KeyCode::Enter
            } else {
                KeyCode::Tab
            },
            KeyModifiers::NONE,
        ));
        assert!(state.menu_comment_is_editing());
        state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        state.paste(" pasted answer 中文");
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, index: 0, comment: Some(comment) }) if menu_id == request && comment == "j pasted answer 中文")
        );
        assert!(state.editor.draft().is_empty());
        state.mark_menu_submission_finished(request);
        state.release_finished_submissions();
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::TickAction { action, .. }) if action.key == 'o' && action.instance == "fixture-tick-0")
        );
        state.paste("Monitor still owns input");
        assert!(state.editor.draft().is_empty());
        state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::TickAction { action, .. }) if action.key == 'o' && action.instance == "fixture-tick-1")
        );
    }
}

#[tokio::test]
async fn review_regression_one_task_event_refreshes_every_cached_or_suspended_center() {
    use crate::terminal::{
        execution::ConsoleExecution, refresh_task_dialogs, task_access::ShellTaskCenter,
    };
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let first = agent.create_chat(directory.path().into()).await.unwrap();
    let second = agent.create_chat(directory.path().into()).await.unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    let console = tests::console_with_state(State::new(true, 80, 24));
    for chat in [first, second] {
        console.select_chat(Some(chat)).unwrap();
        console
            .show_tasks_at(tests::task_snapshot(chat, 2), None)
            .unwrap();
    }
    let request = Uuid::new_v4();
    console.set_request_layout(Some(request)).unwrap();
    console.show_focused_menu(menu(request)).unwrap();
    refresh_task_dialogs(&center, &console).await.unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
    console.clear_menu(request).unwrap();
    for chat in [second, first] {
        console.select_chat(Some(chat)).unwrap();
        let frame = console
            .state
            .lock()
            .unwrap()
            .styled_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !frame.contains("printf task"),
            "stale tasks for {chat}: {frame}"
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn review_regression_management_loads_finish_behind_the_visible_request() {
    use crate::terminal::execution::ConsoleExecution;
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    for surface in ["tasks", "subscriptions", "permissions"] {
        let console = tests::console_with_state(State::new(true, 80, 24));
        console.select_chat(Some(chat)).unwrap();
        let request = Uuid::new_v4();
        console.set_request_layout(Some(request)).unwrap();
        console.show_focused_menu(menu(request)).unwrap();
        match surface {
            "tasks" => {
                console
                    .show_tasks_at(tests::task_snapshot(chat, 2), None)
                    .unwrap();
            }
            "subscriptions" => console
                .show_subscriptions(chat, vec![tests::subscription_row(1)])
                .unwrap(),
            "permissions" => console
                .show_permissions(view.clone(), Err("fixture history".into()), false, true)
                .unwrap(),
            _ => unreachable!(),
        }
        assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
        assert!(
            matches!(console.state.lock().unwrap().key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, .. }) if menu_id == request)
        );
        console.finish_menu_submission(request).unwrap();
        console.state.lock().unwrap().release_finished_submissions();
        let state = console.state.lock().unwrap();
        assert!(
            match surface {
                "tasks" => state.task_surface.is_some(),
                "subscriptions" => state.subscription_surface.is_some(),
                "permissions" => state.permissions_surface.is_some(),
                _ => false,
            },
            "{surface}"
        );
    }
    agent.shutdown().await;
}

#[test]
fn review_regression_login_and_picker_updates_do_not_change_the_visible_request() {
    use crate::terminal::artifacts_picker::ArtifactsPicker;
    for cached in [false, true] {
        for login in [false, true] {
            let chat = Uuid::new_v4();
            let console = tests::console_with_state(State::new(true, 80, 24));
            console.select_chat(Some(chat)).unwrap();
            let id = Uuid::new_v4();
            let load = if login {
                console
                    .show_login_menu(menu(id), "https://example.test/device", false)
                    .unwrap();
                None
            } else {
                console
                    .show_artifacts_picker(ArtifactsPicker::new(
                        Some(chat),
                        "fixture@example.test".into(),
                        HashSet::new(),
                    ))
                    .unwrap();
                console.take_artifact_load().unwrap()
            };
            let request = Uuid::new_v4();
            console.set_request_layout(Some(request)).unwrap();
            console.show_focused_menu(menu(request)).unwrap();
            if cached {
                console.select_chat(Some(Uuid::new_v4())).unwrap();
            }
            if login {
                let mut update = menu(id);
                update.context = vec![
                    "https://example.test/device".into(),
                    "Device code: SUSPENDED123".into(),
                ];
                console
                    .show_login_menu(update, "https://example.test/device", false)
                    .unwrap();
            } else {
                console
                    .artifact_page_loaded(
                        load.as_ref().unwrap(),
                        Err("suspended catalog response".into()),
                    )
                    .unwrap();
            }
            if cached {
                console.select_chat(Some(chat)).unwrap();
            }
            assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
            console.clear_menu(request).unwrap();
            let frame = console
                .state
                .lock()
                .unwrap()
                .styled_frame()
                .0
                .iter()
                .map(RoleLine::text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                frame.contains(if login {
                    "SUSPENDED123"
                } else {
                    "suspended catalog response"
                }),
                "{frame}"
            );
        }
    }
}

#[test]
fn review_regression_restored_task_detail_reconciles_an_unfinished_output_read() {
    for cached in [false, true] {
        let chat = Uuid::new_v4();
        let console = tests::console_with_state(State::new(true, 80, 24));
        console.select_chat(Some(chat)).unwrap();
        let mut snapshot = tests::task_snapshot(chat, 1);
        let task = snapshot.tasks[0].id;
        snapshot.tasks[0].capabilities.can_read_output = true;
        snapshot.tasks[0].output_bytes = 20;
        console.show_tasks_at(snapshot, None).unwrap();
        console
            .state
            .lock()
            .unwrap()
            .task_surface
            .as_mut()
            .unwrap()
            .open_detail(
                task,
                TaskViewport {
                    width: 80,
                    height: 10,
                },
            );
        let request = Uuid::new_v4();
        console.set_request_layout(Some(request)).unwrap();
        console.show_focused_menu(menu(request)).unwrap();
        assert!(console.take_resumed_task_effect().unwrap().is_none());
        if cached {
            console.select_chat(Some(Uuid::new_v4())).unwrap();
        }
        if cached {
            console.select_chat(Some(chat)).unwrap();
        }
        assert!(console.take_resumed_task_effect().unwrap().is_none());
        console.clear_menu(request).unwrap();
        assert!(
            matches!(console.take_resumed_task_effect().unwrap(), Some((owner, TaskSurfaceEffect::ReadOutput { task_id, .. })) if owner == chat && task_id == task)
        );
        assert!(console.take_resumed_task_effect().unwrap().is_none());
    }
}

#[test]
fn review_regression_task_output_and_action_results_reach_hidden_dialogs() {
    for cached in [false, true] {
        let chat = Uuid::new_v4();
        let console = tests::console_with_state(State::new(true, 80, 24));
        console.select_chat(Some(chat)).unwrap();
        let mut snapshot = tests::task_snapshot(chat, 1);
        let task = snapshot.tasks[0].id;
        snapshot.tasks[0].capabilities.can_read_output = true;
        snapshot.tasks[0].output_bytes = 20;
        console.show_tasks_at(snapshot, None).unwrap();
        let generation = {
            let mut state = console.state.lock().unwrap();
            let TaskSurfaceEffect::ReadOutput { generation, .. } =
                state.task_surface.as_mut().unwrap().open_detail(
                    task,
                    TaskViewport {
                        width: 80,
                        height: 10,
                    },
                )
            else {
                panic!("output read expected")
            };
            generation
        };
        for action_result in [false, true] {
            let request = Uuid::new_v4();
            console.set_request_layout(Some(request)).unwrap();
            console.show_focused_menu(menu(request)).unwrap();
            if cached {
                console.select_chat(Some(Uuid::new_v4())).unwrap();
            }
            if action_result {
                console
                    .task_action_failed(chat, task, "hidden action result".into())
                    .unwrap();
            } else {
                console
                    .update_task_output(
                        chat,
                        TaskOutputUpdate::Unavailable {
                            generation,
                            task_id: task,
                            total_bytes: 20,
                            message: "hidden output result".into(),
                        },
                    )
                    .unwrap();
            }
            if cached {
                console.select_chat(Some(chat)).unwrap();
            }
            console.clear_menu(request).unwrap();
            let state = console.state.lock().unwrap();
            let error = state
                .task_surface
                .as_ref()
                .unwrap()
                .visible_error()
                .unwrap_or("");
            assert!(
                error.contains(if action_result {
                    "hidden action result"
                } else {
                    "hidden output result"
                }),
                "{error}"
            );
        }
    }
}

#[tokio::test]
async fn review_regression_management_updates_reach_suspended_and_cached_dialogs() {
    use crate::terminal::execution::ConsoleExecution;
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    for cached in [false, true] {
        for permissions in [false, true] {
            let console = tests::console_with_state(State::new(true, 80, 24));
            console.select_chat(Some(chat)).unwrap();
            let mut view = agent.permissions_view(chat).await.unwrap();
            if permissions {
                let mut surface = PermissionsSurface::open(
                    view.clone(),
                    PermissionsReturn::ModePicker { settings: false },
                );
                surface.open_auto();
                console.state.lock().unwrap().permissions_surface = Some(surface);
            } else {
                console
                    .show_subscriptions(chat, vec![tests::subscription_row(1)])
                    .unwrap();
            }
            let request = Uuid::new_v4();
            console.set_request_layout(Some(request)).unwrap();
            console.show_focused_menu(menu(request)).unwrap();
            if cached {
                console.select_chat(Some(Uuid::new_v4())).unwrap();
            }
            if permissions {
                view.warnings.push("updated suspended policy".into());
                console.permissions_refreshed(view).unwrap();
            } else {
                assert!(console.subscription_dialog_chats().unwrap().contains(&chat));
                let mut row = tests::subscription_row(1);
                row.every_seconds = 321;
                console.update_subscriptions(chat, vec![row]).unwrap();
            }
            if cached {
                console.select_chat(Some(chat)).unwrap();
            }
            console.clear_menu(request).unwrap();
            let frame = console
                .state
                .lock()
                .unwrap()
                .styled_frame()
                .0
                .iter()
                .map(RoleLine::text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                frame.contains(if permissions {
                    "updated suspended policy"
                } else {
                    "321s"
                }),
                "{frame}"
            );
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn review_regression_only_the_displayed_request_consumes_input() {
    use crate::terminal::{
        artifacts_picker::ArtifactsPicker,
        conversation_picker::{ConversationEntry, ConversationPickerPurpose},
        execution::ConsoleExecution,
    };
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    for title in ["Permission request", "Question", "Plan approval"] {
        for surface in [
            "history",
            "resume",
            "artifacts",
            "login",
            "tasks",
            "subscriptions",
            "permissions",
            "help",
            "foreground shell",
            "monitor",
        ] {
            for cancel in [false, true] {
                let (width, height) = if surface == "monitor" {
                    (120, 40)
                } else {
                    (80, 24)
                };
                let mut state = State::new(true, width, height);
                state.chat = Some(chat);
                state.prompt_enabled = true;
                let mut history = InputHistory::open(directory.path());
                history
                    .record("matrix", directory.path(), "history fixture", &[])
                    .unwrap();
                state.input_history = Some(history);
                state.history_project = directory.path().into();
                state.insert("preserved draft");
                let console = tests::console_with_state(state);
                match surface {
                    "history" => {
                        console
                            .state
                            .lock()
                            .unwrap()
                            .key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
                    }
                    "resume" => console
                        .show_conversation_picker(ConversationPicker::new(
                            ConversationPickerPurpose::Resume,
                            vec![ConversationEntry::new("Saved Chat", &[], &[])],
                        ))
                        .unwrap(),
                    "artifacts" => console
                        .show_artifacts_picker(ArtifactsPicker::new(
                            Some(chat),
                            "fixture@example.test".into(),
                            HashSet::new(),
                        ))
                        .unwrap(),
                    "login" => console
                        .show_login_menu(menu(Uuid::new_v4()), "https://example.test/device", false)
                        .unwrap(),
                    "tasks" => {
                        console
                            .show_tasks_at(tests::task_snapshot(chat, 2), None)
                            .unwrap();
                    }
                    "subscriptions" => console
                        .show_subscriptions(
                            chat,
                            vec![tests::subscription_row(1), tests::subscription_row(2)],
                        )
                        .unwrap(),
                    "permissions" => {
                        console.state.lock().unwrap().permissions_surface =
                            Some(PermissionsSurface::open(
                                view.clone(),
                                PermissionsReturn::ModePicker { settings: false },
                            ))
                    }
                    "help" => console.state.lock().unwrap().help_open = true,
                    "foreground shell" => console.state.lock().unwrap().foreground_shell = true,
                    "monitor" => {
                        let mut state = console.state.lock().unwrap();
                        state.monitor.replace(TickGroup::Artifacts, fixture_ticks());
                        state.monitor.focus(true, None);
                        assert!(state.monitor_visible());
                    }
                    _ => unreachable!(),
                }
                let before = console
                    .state
                    .lock()
                    .unwrap()
                    .styled_frame()
                    .0
                    .iter()
                    .map(RoleLine::text)
                    .collect::<Vec<_>>();
                let request = Uuid::new_v4();
                console.set_request_layout(Some(request)).unwrap();
                let mut request_menu = menu(request);
                request_menu.title = title.into();
                console
                    .show_focused_commentable_menu(request_menu, vec![1])
                    .unwrap();
                let mut state = console.state.lock().unwrap();
                state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
                state.paste("hidden paste");
                state.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
                assert!(state.menu_comment_is_editing(), "{title}/{surface}");
                state.paste("visible request paste");
                assert!(
                    state
                        .styled_frame()
                        .0
                        .iter()
                        .any(|line| line.text().contains("visible request paste")),
                    "{title}/{surface}"
                );
                if cancel {
                    assert!(
                        state
                            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                            .is_none()
                    );
                }
                let event = state.key(KeyEvent::new(
                    if cancel { KeyCode::Esc } else { KeyCode::Enter },
                    KeyModifiers::NONE,
                ));
                if cancel {
                    assert!(
                        matches!(event, Some(InputEvent::Cancel { menu_id }) if menu_id == request),
                        "{title}/{surface}"
                    );
                } else {
                    assert!(
                        matches!(event, Some(InputEvent::Choice { menu_id, index: 1, comment: Some(comment) }) if menu_id == request && comment == "visible request paste"),
                        "{title}/{surface}"
                    );
                }
                assert_eq!(state.editor.draft(), "preserved draft", "{title}/{surface}");
                state.mark_menu_submission_finished(request);
                state.release_finished_submissions();
                let after = state
                    .styled_frame()
                    .0
                    .iter()
                    .map(RoleLine::text)
                    .collect::<Vec<_>>();
                assert_eq!(before, after, "{title}/{surface}");
                if surface == "monitor" {
                    let event = state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    assert!(
                        matches!(&event, Some(InputEvent::TickAction { action, .. }) if action.key == 'o' && action.instance == "fixture-tick-0"),
                        "{title}/{cancel}: {event:?}"
                    );
                }
            }
        }
    }
    agent.shutdown().await;
}

#[test]
fn review_regression_history_search_cannot_consume_a_visible_request() {
    let directory = tempfile::tempdir().unwrap();
    let mut history = InputHistory::open(directory.path());
    history
        .record("fixture", directory.path(), "historical answer", &[])
        .unwrap();
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.input_history = Some(history);
    state.history_project = directory.path().into();
    state.history_session = "fixture".into();
    state.insert("unchanged draft");
    state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    let console = tests::console_with_state(state);
    let request = Uuid::new_v4();
    console.set_request_layout(Some(request)).unwrap();
    console.show_focused_menu(menu(request)).unwrap();
    let mut state = console.state.lock().unwrap();
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    state.paste("hidden search must not receive this");
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, index: 1, .. }) if menu_id == request)
    );
    assert_eq!(state.editor.draft(), "unchanged draft");
    state.mark_menu_submission_finished(request);
    state.release_finished_submissions();
    assert_eq!(state.history_search.as_ref().unwrap().query(), "");
}

#[tokio::test]
async fn review_regression_permissions_click_opens_an_unselected_option_like_enter() {
    use crate::terminal::execution::ConsoleExecution;
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    let mut clicked = PermissionsSurface::open(
        view.clone(),
        PermissionsReturn::ModePicker { settings: false },
    );
    let mut keyboard =
        PermissionsSurface::open(view, PermissionsReturn::ModePicker { settings: false });
    clicked.open_auto();
    keyboard.open_auto();
    let row = clicked
        .styled_lines(80, 12)
        .iter()
        .position(|line| line.text().contains("Soft deny"))
        .unwrap();
    clicked.click(row, 80, 12);
    for _ in 0..3 {
        keyboard.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    keyboard.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        clicked
            .styled_lines(80, 12)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>(),
        keyboard
            .styled_lines(80, 12)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
    );
    agent.shutdown().await;
}

#[test]
fn review_regression_task_snapshots_reach_suspended_and_cached_dialogs() {
    use tickloop_agent_protocol::ShellTaskState;
    for cached in [false, true] {
        let chat = Uuid::new_v4();
        let console = tests::console_with_state(State::new(true, 80, 24));
        console.select_chat(Some(chat)).unwrap();
        let mut snapshot = tests::task_snapshot(chat, 2);
        console.show_tasks_at(snapshot.clone(), None).unwrap();
        let request = Uuid::new_v4();
        console.set_request_layout(Some(request)).unwrap();
        console.show_focused_menu(menu(request)).unwrap();
        if cached {
            console.select_chat(Some(Uuid::new_v4())).unwrap();
        }
        snapshot.live_revision += 1;
        snapshot.tasks[0].state = ShellTaskState::Completed;
        snapshot.tasks[0].exit_code = Some(0);
        console.update_tasks(snapshot).unwrap();
        console
            .task_updates_failed(chat, "scoped update error".into())
            .unwrap();
        if cached {
            console.select_chat(Some(chat)).unwrap();
        }
        console.clear_menu(request).unwrap();
        let mut state = console.state.lock().unwrap();
        let frame = state
            .styled_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(frame.contains("completed"), "{frame}");
        assert!(frame.contains("scoped update error"), "{frame}");
    }
}

#[test]
fn review_regression_post_login_picker_waits_behind_the_request() {
    use crate::terminal::artifacts_picker::ArtifactsPicker;
    for cached in [false, true] {
        let chat = Uuid::new_v4();
        let console = tests::console_with_state(State::new(true, 80, 24));
        console.select_chat(Some(chat)).unwrap();
        let login = Uuid::new_v4();
        console
            .show_login_menu(menu(login), "https://example.test/device", false)
            .unwrap();
        let request = Uuid::new_v4();
        console.set_request_layout(Some(request)).unwrap();
        console.show_focused_menu(menu(request)).unwrap();
        if cached {
            console.select_chat(Some(Uuid::new_v4())).unwrap();
        }
        console.clear_menu(login).unwrap();
        let picker =
            ArtifactsPicker::new(Some(chat), "fixture@example.test".into(), HashSet::new());
        let picker_id = picker.id();
        console.show_artifacts_picker(picker).unwrap();
        if cached {
            console.select_chat(Some(chat)).unwrap();
        }
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, index: 1, .. }) if menu_id == request)
        );
        state.mark_menu_submission_finished(request);
        state.release_finished_submissions();
        assert_eq!(state.artifacts_picker.as_ref().unwrap().id(), picker_id);
    }
}

fn menu(id: Uuid) -> Menu {
    Menu {
        id,
        title: "Settings".into(),
        context: vec!["Choose a setting".into()],
        options: vec!["Theme".into(), "Output style".into()],
        selected: 0,
        confirmation: false,
    }
}

#[test]
fn input_dialog_replaces_the_footer_with_content_height_and_preserves_the_draft() {
    for (width, height, expected) in [(80, 24, 6), (120, 40, 6), (30, 10, 6)] {
        let mut state = State::new(true, width, height);
        state.prompt_enabled = true;
        state.insert("draft jk");
        state.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        let cursor = state.editor.cursor();
        let console = tests::console_with_state(state);
        let id = Uuid::new_v4();
        console.show_focused_menu(menu(id)).unwrap();
        let mut state = console.state.lock().unwrap();
        let lines = state.styled_frame().0;
        assert_eq!(lines.len(), expected);
        assert!(!lines.iter().any(|line| line.text().contains("draft jk")));
        state.clear_menu(id);
        assert_eq!(state.editor.draft(), "draft jk");
        assert_eq!(state.editor.cursor(), cursor);
    }
}

#[test]
fn input_dialog_request_preempts_and_restores_selection_scroll_and_agent_state() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let user_menu = Uuid::new_v4();
    let request = Uuid::new_v4();
    console.select_chat(Some(first)).unwrap();
    let mut settings = menu(user_menu);
    settings.context = (0..40)
        .map(|row| format!("setting context {row}"))
        .collect();
    console.show_focused_menu(settings).unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    }
    assert!(console.can_present_request().unwrap());
    console.set_request_layout(Some(request)).unwrap();
    console.set_request_queue_count(3).unwrap();
    let mut request_menu = menu(request);
    request_menu.title = "Question".into();
    console.show_focused_menu(request_menu).unwrap();
    assert!(
        console.state.lock().unwrap().styled_frame().0[input_dialog::INPUT_DIALOG_RULE_ROWS]
            .text()
            .contains("(1/3)")
    );
    console.select_chat(Some(second)).unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), None);
    console.select_chat(Some(first)).unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
    console.clear_menu(request).unwrap();
    let state = console.state.lock().unwrap();
    assert_eq!(state.menu.as_ref().unwrap().id, user_menu);
    assert_eq!(state.menu.as_ref().unwrap().selected, 1);
    assert_eq!(state.context_offset, 1);
}

#[test]
fn input_dialog_click_submits_only_the_clicked_option() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let id = Uuid::new_v4();
    console.show_focused_menu(menu(id)).unwrap();
    let mut state = console.state.lock().unwrap();
    state.draw(&mut Vec::new()).unwrap();
    let row = state
        .rendered_screen
        .iter()
        .position(|line| line.contains("Output style"))
        .unwrap();
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(
        matches!(state.pointer_input, Some(InputEvent::Choice { menu_id, index: 1, .. }) if menu_id == id)
    );
}

#[test]
fn permission_full_diff_returns_to_the_same_request_without_approving_it() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let id = Uuid::new_v4();
    console.set_request_layout(Some(id)).unwrap();
    console
        .show_reviewable_menu(menu(id), vec![1], Err("review diagnostic ".repeat(100)))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    state.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].contains("Diff"));
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(state.menu.as_ref().unwrap().id, id);
    assert_eq!(state.styled_frame().0.len(), 12);
}

#[test]
fn resume_picker_uses_the_bottom_container_and_keeps_the_conversation_visible() {
    use crate::terminal::conversation_picker::{ConversationEntry, ConversationPickerPurpose};
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.scrollback.append("conversation remains visible\n");
    let console = tests::console_with_state(state);
    console
        .show_conversation_picker(ConversationPicker::new(
            ConversationPickerPurpose::Resume,
            vec![ConversationEntry::new("Saved Chat", &[], &[])],
        ))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    state.draw(&mut Vec::new()).unwrap();
    assert!(
        state.rendered_screen[..state.height - state.input_dialog_height()]
            .iter()
            .any(|line| line.contains("conversation remains visible"))
    );
    assert!(
        state.rendered_screen
            [state.height - state.input_dialog_height() + input_dialog::INPUT_DIALOG_RULE_ROWS]
            .starts_with("Resume Chat")
    );
    assert_eq!(state.styled_frame().0.len(), 6);
}

#[test]
fn resume_search_and_scroll_resume_after_an_agent_request() {
    use crate::terminal::conversation_picker::{ConversationEntry, ConversationPickerPurpose};
    let console = tests::console_with_state(State::new(true, 120, 40));
    let picker = ConversationPicker::new(
        ConversationPickerPurpose::Resume,
        (0..30)
            .map(|index| ConversationEntry::new(&format!("Chat {index}"), &[], &[]))
            .collect(),
    );
    console.show_conversation_picker(picker).unwrap();
    let before = {
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        state.paste("Chat 1");
        state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        for _ in 0..5 {
            state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        state
            .styled_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
    };
    let request = Uuid::new_v4();
    console.set_request_layout(Some(request)).unwrap();
    console.show_focused_menu(menu(request)).unwrap();
    console.clear_menu(request).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(
        state.conversation_picker.as_ref().unwrap().query(),
        "Chat 1"
    );
    assert_eq!(
        state
            .styled_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>(),
        before
    );
}

#[test]
fn artifact_load_returns_to_its_hidden_agent_without_changing_the_visible_dialog() {
    use crate::terminal::artifacts_picker::ArtifactsPicker;
    let console = tests::console_with_state(State::new(true, 80, 24));
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    console.select_chat(Some(owner)).unwrap();
    console
        .show_artifacts_picker(ArtifactsPicker::new(
            Some(owner),
            "fixture@example.test".into(),
            HashSet::new(),
        ))
        .unwrap();
    let request = console.take_artifact_load().unwrap().unwrap();
    console.select_chat(Some(other)).unwrap();
    console
        .artifact_page_loaded(&request, Err("Scoped artifact load error".into()))
        .unwrap();
    assert!(console.state.lock().unwrap().artifacts_picker.is_none());
    console.select_chat(Some(owner)).unwrap();
    let mut state = console.state.lock().unwrap();
    assert!(
        state
            .styled_frame()
            .0
            .iter()
            .any(|line| line.text().contains("Scoped artifact load error"))
    );
}

#[test]
fn workspace_trust_stays_fixed_and_subscriptions_fit_their_content() {
    let mut state = State::new(true, 80, 24);
    state.workspace_trust = Some(WorkspaceTrust::new(PathBuf::from("/fixture/project")));
    assert_eq!(state.styled_frame().0.len(), 12);
    let console = tests::console_with_state(State::new(true, 120, 40));
    let chat = Uuid::new_v4();
    console.select_chat(Some(chat)).unwrap();
    console.show_subscriptions(chat, Vec::new()).unwrap();
    assert_eq!(console.state.lock().unwrap().styled_frame().0.len(), 4);
}

#[test]
fn sticky_prompt_short_conversation_keeps_the_full_content_height() {
    let console = tests::console_with_state(State::new(true, 160, 24));
    console.select_chat(Some(Uuid::new_v4())).unwrap();
    console.show_focused_menu(menu(Uuid::new_v4())).unwrap();
    let mut state = console.state.lock().unwrap();
    state.monitor.replace(TickGroup::Artifacts, fixture_ticks());
    state.scrollback.append("Leading boundary");
    state.record_input("Short user request");
    for index in 0..9 {
        state.scrollback.append(&format!("Short reply {index}\n"));
    }
    state.scrollback.append("Trailing boundary");
    state.draw(&mut Vec::new()).unwrap();
    assert!(
        state.rendered_screen[..state.height - state.input_dialog_height()]
            .iter()
            .any(|line| line.starts_with("Leading boundary"))
    );
    assert!(state.rendered_screen[0].contains('│'));
    assert!(
        state.rendered_screen[..12]
            .iter()
            .any(|row| row.contains("Fixture Tick 0"))
    );
    assert!(
        state.rendered_screen[..state.height - state.input_dialog_height()]
            .iter()
            .any(|line| line.starts_with("❯ Short user request"))
    );
    assert_eq!(
        state
            .rendered_screen
            .iter()
            .filter(|row| row.contains("Short user request"))
            .count(),
        1
    );
    assert_eq!(state.input_dialog_height(), 6);
    let dialog_top = state.height - state.input_dialog_height();
    assert!(state.rendered_screen[dialog_top - 1].starts_with("Trailing boundary"));
    assert!(state.rendered_screen[dialog_top - 1].contains('│'));
    assert_eq!(state.rendered_screen[dialog_top], "▔".repeat(state.width));
    assert!(
        state.rendered_screen[dialog_top + input_dialog::INPUT_DIALOG_RULE_ROWS]
            .starts_with("Settings")
    );
}

#[test]
fn sticky_prompt_loaded_history_tracks_the_original_message_row() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let chat = Uuid::new_v4();
    let agent_message = Uuid::new_v4();
    console.select_chat(Some(chat)).unwrap();
    let snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
        "id": chat,
        "workdir": "/fixture",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Loaded user request"}]},
            {"role": "user", "content": [{"type": "text", "text": "Legacy Agent message"}]},
            {"role": "assistant", "content": [{"type": "text", "text": (0..40).map(|index| format!("Loaded reply {index}\n\n")).collect::<String>()}]}
        ],
        "message_ids": {"1": agent_message},
        "agent_message_origins": {(agent_message.to_string()): {}},
        "loops": []
    })).unwrap();
    super::super::workflows::write_public_history(&console, &snapshot).unwrap();
    let mut state = console.state.lock().unwrap();
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Loaded user request"));
    state.scroll_conversation(ScrollAction::First);
    state.draw(&mut Vec::new()).unwrap();
    assert!(!state.rendered_screen[0].contains("Loaded user request"));
    assert_eq!(
        state
            .rendered_screen
            .iter()
            .filter(|row| row.contains("Loaded user request"))
            .count(),
        1
    );
}

#[test]
fn latest_user_message_stays_at_the_top_left_with_ticks_and_an_input_dialog() {
    let mut state = State::new(true, 160, 24);
    state.chat = Some(Uuid::new_v4());
    state.record_input("Most recent user request\nwith more detail");
    for index in 0..30 {
        state.scrollback.append(&format!("Agent reply {index}\n"));
    }
    state.monitor.replace(
        TickGroup::Shells,
        vec![MonitorEntry {
            instance: tick_protocol::InstanceRef::new("shell", "fixture").unwrap(),
            group: TickGroup::Shells,
            glyph: Glyph::ShellTick,
            title: "Fixture Shell".into(),
            kind: "Shell".into(),
            status: "running".into(),
            owner: crate::monitor_rows::TickOwner::Unknown,
            updated_at_ms: None,
            resources: None,
            actions: Vec::new(),
        }],
    );
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Most recent user request"));
    assert!(state.rendered_screen[0].contains('│'));
    let console = tests::console_with_state(state);
    console.show_focused_menu(menu(Uuid::new_v4())).unwrap();
    let mut state = console.state.lock().unwrap();
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Most recent user request"));
    assert!(state.rendered_screen[0].contains('│'));
    assert!(
        state.rendered_screen
            [state.height - state.input_dialog_height() + input_dialog::INPUT_DIALOG_RULE_ROWS]
            .starts_with("Settings")
    );
}

#[test]
fn latest_user_message_is_scoped_to_its_agent_and_falls_back_on_short_screens() {
    let console = tests::console_with_state(State::new(true, 20, 8));
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    console.select_chat(Some(first)).unwrap();
    console
        .record_input(&format!("First user {}\nnext line", "界".repeat(40)))
        .unwrap();
    console.select_chat(Some(second)).unwrap();
    console.record_input("Second user").unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.draw(&mut Vec::new()).unwrap();
        assert!(!state.rendered_screen[0].contains("Second user"));
        assert_eq!(
            state
                .rendered_screen
                .iter()
                .filter(|row| row.contains("Second user"))
                .count(),
            1
        );
    }
    console.select_chat(Some(first)).unwrap();
    let mut state = console.state.lock().unwrap();
    for (width, height) in [(20, 8), (4, 5), (2, 2), (1, 1)] {
        state.resize(width, height);
        state.draw(&mut Vec::new()).unwrap();
        assert_eq!(state.rendered_screen.len(), height as usize);
        assert!(
            state
                .rendered_screen
                .iter()
                .all(|line| !line.contains('\n') && display_width(line) <= state.width)
        );
        if height >= 8 {
            assert!(
                state.rendered_screen[0].starts_with("❯ First user")
                    && state.rendered_screen[0].ends_with('…')
            );
        }
    }
}

#[test]
fn sticky_prompt_first_line_boundary_is_stable_on_scroll_and_resize() {
    let console = tests::console_with_state(State::new(true, 80, 18));
    console.select_chat(Some(Uuid::new_v4())).unwrap();
    console.show_focused_menu(menu(Uuid::new_v4())).unwrap();
    let mut state = console.state.lock().unwrap();
    state.record_input("Boundary request\ncontinuation visible");
    for index in 0..9 {
        state.scrollback.append(&format!("Reply {index}\n"));
    }
    for _ in 0..3 {
        state.draw(&mut Vec::new()).unwrap();
        assert!(state.rendered_screen[0].starts_with("❯ Boundary request"));
        assert_eq!(
            state
                .rendered_screen
                .iter()
                .filter(|row| row.contains("Boundary request"))
                .count(),
            1
        );
        assert!(state.rendered_screen[1].contains("continuation visible"));
    }
    state.scrollback.append("Reply 9\n");
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Boundary request continuation visible"));
    state.resize(80, 19);
    for _ in 0..3 {
        state.draw(&mut Vec::new()).unwrap();
        assert!(state.rendered_screen[0].starts_with("❯ Boundary request"));
        assert!(!state.rendered_screen[0].contains("continuation visible"));
        assert!(state.rendered_screen[1].contains("continuation visible"));
    }
    state.resize(80, 18);
    state.draw(&mut Vec::new()).unwrap();
    state.scroll_conversation(ScrollAction::First);
    state.scroll_conversation(ScrollAction::LinesDown(2));
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Boundary request continuation visible"));
    assert!(state.rendered_screen[1].contains("continuation visible"));
    state.scroll_conversation(ScrollAction::LinesUp(1));
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Boundary request"));
    assert!(!state.rendered_screen[0].contains("continuation visible"));
    assert!(state.rendered_screen[1].contains("continuation visible"));
}

#[test]
fn sticky_prompt_latest_message_below_the_viewport_has_no_header() {
    let mut state = State::new(true, 80, 24);
    state.chat = Some(Uuid::new_v4());
    state.record_input("Earlier request");
    for index in 0..40 {
        state.scrollback.append(&format!("Earlier reply {index}\n"));
    }
    state.record_input("Latest request");
    for index in 0..40 {
        state.scrollback.append(&format!("Latest reply {index}\n"));
    }
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Latest request"));
    state.scroll_conversation(ScrollAction::First);
    state.scroll_conversation(ScrollAction::LinesDown(10));
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("Earlier reply"));
    assert!(
        !state
            .rendered_screen
            .iter()
            .any(|row| row.contains("Latest request"))
    );
}

#[test]
fn sticky_prompt_reflows_with_width_and_respects_auto_scroll_switches() {
    let console = tests::console_with_state(State::new(true, 80, 18));
    console.select_chat(Some(Uuid::new_v4())).unwrap();
    console.show_focused_menu(menu(Uuid::new_v4())).unwrap();
    let mut state = console.state.lock().unwrap();
    state.record_input("Reflow request");
    state
        .scrollback
        .append(&format!("{}\n", "width ".repeat(80)));
    state.draw(&mut Vec::new()).unwrap();
    assert!(!state.rendered_screen[0].contains("Reflow request"));
    assert_eq!(
        state
            .rendered_screen
            .iter()
            .filter(|row| row.contains("Reflow request"))
            .count(),
        1
    );
    state.resize(40, 18);
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Reflow request"));
    state.scrollback.set_auto_scroll(false);
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Reflow request"));
    state.scroll_conversation(ScrollAction::First);
    state.draw(&mut Vec::new()).unwrap();
    assert!(!state.rendered_screen[0].contains("Reflow request"));
    state.scrollback.set_auto_scroll(true);
    state.draw(&mut Vec::new()).unwrap();
    assert!(!state.rendered_screen[0].contains("Reflow request"));
    state.scroll_conversation(ScrollAction::Last);
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("❯ Reflow request"));
    state.resize(80, 18);
    state.draw(&mut Vec::new()).unwrap();
    assert!(!state.rendered_screen[0].contains("Reflow request"));
    assert_eq!(
        state
            .rendered_screen
            .iter()
            .filter(|row| row.contains("Reflow request"))
            .count(),
        1
    );
}

#[test]
fn hidden_login_updates_preserve_the_focused_request_and_resume_the_device_code() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let login = Uuid::new_v4();
    let request = Uuid::new_v4();
    let address = "https://example.test/device";
    let mut login_menu = menu(login);
    login_menu.context = vec![address.into(), "Waiting for device code".into()];
    console.show_login_menu(login_menu, address, false).unwrap();
    console.set_request_layout(Some(request)).unwrap();
    console.show_focused_menu(menu(request)).unwrap();
    let mut login_menu = menu(login);
    login_menu.context = vec![address.into(), "Device code: FIXTURE123".into()];
    console.show_login_menu(login_menu, address, false).unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
    console.clear_menu(request).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.menu.as_ref().unwrap().id, login);
    assert!(
        state
            .styled_frame()
            .0
            .iter()
            .any(|line| line.text().contains("FIXTURE123"))
    );
}

#[test]
fn workspace_trust_click_uses_the_visible_choice_and_the_keyboard_contract() {
    let mut state = State::new(true, 80, 24);
    state.workspace_trust = Some(WorkspaceTrust::new(PathBuf::from("/fixture/project")));
    state.draw(&mut Vec::new()).unwrap();
    let row = state
        .rendered_screen
        .iter()
        .position(|line| line.contains("Yes, I trust this folder"))
        .unwrap();
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: row as u16,
            column: 3,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(matches!(
        state.pointer_input,
        Some(InputEvent::Choice {
            index: TRUST_ACCEPT_CHOICE_INDEX,
            ..
        })
    ));
}

#[test]
fn a_captured_user_choice_finishes_before_an_agent_request_can_suspend_its_dialog() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let user_menu = Uuid::new_v4();
    let request = Uuid::new_v4();
    console.show_focused_menu(menu(user_menu)).unwrap();
    assert!(
        matches!(console.state.lock().unwrap().key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, .. }) if menu_id == user_menu)
    );
    assert_eq!(
        console.set_request_layout(Some(request)).unwrap(),
        RequestPresentation::Deferred
    );
    assert_eq!(console.dialog_menu_id().unwrap(), Some(user_menu));
    console.finish_menu_submission(user_menu).unwrap();
    console.state.lock().unwrap().release_finished_submissions();
    console.set_request_layout(Some(request)).unwrap();
    console.show_focused_menu(menu(request)).unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
}

#[test]
fn a_completed_choice_closes_its_cached_agent_dialog_and_drains_queued_input() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let id = Uuid::new_v4();
    console.select_chat(Some(first)).unwrap();
    console.show_focused_menu(menu(id)).unwrap();
    assert!(matches!(
        console
            .state
            .lock()
            .unwrap()
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::Choice { .. })
    ));
    console.select_chat(Some(second)).unwrap();
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .is_none()
    );
    console.finish_menu_submission(id).unwrap();
    console.state.lock().unwrap().release_finished_submissions();
    console.select_chat(Some(first)).unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), None);
    assert!(!console.state.lock().unwrap().menu_submission_is_pending());
}

#[test]
fn a_completed_artifact_selection_closes_its_original_picker_without_switching_agents() {
    use crate::terminal::artifacts_picker::ArtifactsPicker;
    let console = tests::console_with_state(State::new(true, 80, 24));
    let owner = Uuid::new_v4();
    let visible = Uuid::new_v4();
    let visible_menu = Uuid::new_v4();
    let picker = ArtifactsPicker::new(None, "fixture@example.test".into(), HashSet::new());
    let picker_id = picker.id();
    console.show_artifacts_picker(picker).unwrap();
    console.select_chat(Some(visible)).unwrap();
    console.show_focused_menu(menu(visible_menu)).unwrap();
    console
        .select_artifact_tick(
            picker_id,
            owner,
            "artifact:https://docs.example/fixture-artifact",
        )
        .unwrap();
    assert_eq!(console.dialog_chat().unwrap(), Some(visible));
    assert_eq!(console.dialog_menu_id().unwrap(), Some(visible_menu));
    console.select_chat(None).unwrap();
    assert!(console.state.lock().unwrap().artifacts_picker.is_none());
}

#[test]
fn provider_model_and_connection_dialogs_fit_their_content_and_isolate_input() {
    for kind in [PickerKind::Provider, PickerKind::Model, PickerKind::Actions] {
        for (width, height, expected) in [(80, 24, 22), (120, 40, 26), (32, 10, 8)] {
            let mut state = State::new(true, width, height);
            state.chat = Some(Uuid::new_v4());
            state.prompt_enabled = true;
            state.insert("unsent Chat draft");
            state.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
            let cursor = state.editor.cursor();
            state.monitor.replace(TickGroup::Artifacts, fixture_ticks());
            state.monitor.focus(true, None);
            let console = tests::console_with_state(state);
            let id = Uuid::new_v4();
            console
                .show_presented_picker_menu(
                    Menu {
                        id,
                        title: "Builtin picker".into(),
                        context: vec!["Connection scope".into()],
                        options: (0..20).map(|index| format!("Choice {index}")).collect(),
                        selected: 12,
                        confirmation: false,
                    },
                    kind,
                    PickerPresentation {
                        headers: vec![
                            (0, "DeepSeek · 1/10 configured".into()),
                            (10, "OpenAI · 2/10 configured".into()),
                        ],
                        hint: "Enter apply · Esc back".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            let mut state = console.state.lock().unwrap();
            let lines = state.styled_frame().0;
            assert_eq!(lines.len(), expected, "{kind:?}/{width}x{height}");
            assert!(lines[1].text().contains("Builtin picker"));
            assert!(
                !lines
                    .iter()
                    .any(|line| line.text().contains("unsent Chat draft"))
            );
            assert!(lines.iter().any(|line| line.text().contains("Choice 12")));
            assert!(lines.iter().any(|line| line.text().contains("OpenAI")));
            assert!(!lines.iter().any(|line| line.text().contains("13. Choice")));
            state.paste("must not reach Chat input");
            state.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
            assert_eq!(state.menu.as_ref().unwrap().selected, 12);
            state.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
            assert_eq!(state.menu.as_ref().unwrap().selected, 13);
            state.clear_menu(id);
            assert_eq!(state.editor.draft(), "unsent Chat draft");
            assert_eq!(state.editor.cursor(), cursor);
            assert!(state.monitor.focused);
        }
    }
}

#[test]
fn provider_credential_dialog_hides_ordinals_and_keeps_secret_editing_hints() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.insert("unsent Chat draft");
    let console = tests::console_with_state(state);
    let id = Uuid::new_v4();
    console
        .show_secret_menu_with_value(
            Menu {
                id,
                title: "API key · DeepSeek".into(),
                context: vec!["Credential belongs to this connection".into()],
                options: vec!["Save API key".into(), "Cancel".into()],
                selected: 0,
                confirmation: false,
            },
            "secret-value",
        )
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let lines = state.styled_frame().0;
    let text = lines
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(lines.len(), 7);
    assert!(!text.contains("secret-value"));
    assert!(!text.contains("1. Save API key"), "{text}");
    assert!(text.contains("Ctrl+G"), "{text}");
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let closed = state
        .styled_frame()
        .0
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        closed.contains(&format!("API key: {}", "*".repeat("secret-value".len()))),
        "{closed}"
    );
    assert!(closed.contains("Esc back"), "{closed}");
    state.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, comment: Some(value), .. }) if menu_id == id && value == "secret-value1j")
    );
    assert_eq!(state.editor.draft(), "unsent Chat draft");
}

#[test]
fn provider_dialog_preemption_restores_headers_focus_context_and_private_field() {
    for secret in [false, true] {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let console = tests::console_with_state(State::new(true, 80, 24));
        console.select_chat(Some(first)).unwrap();
        let id = Uuid::new_v4();
        let mut page = menu(id);
        page.title = "Connection dialog".into();
        page.context = (0..15).map(|index| format!("Detail {index}")).collect();
        if secret {
            console
                .show_secret_menu_with_value(page, "private-credential")
                .unwrap();
            console
                .state
                .lock()
                .unwrap()
                .key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        } else {
            page.options = (0..15).map(|index| format!("Model {index}")).collect();
            page.selected = 10;
            console
                .show_presented_picker_menu(
                    page,
                    PickerKind::Model,
                    PickerPresentation {
                        headers: vec![(0, "Builtin family".into())],
                        hint: "p Connection · Enter apply · Esc back".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            console
                .state
                .lock()
                .unwrap()
                .key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        }
        let before = console
            .state
            .lock()
            .unwrap()
            .styled_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>();
        let request = Uuid::new_v4();
        console.set_request_layout(Some(request)).unwrap();
        console.show_focused_menu(menu(request)).unwrap();
        console
            .state
            .lock()
            .unwrap()
            .paste("must not edit hidden credential");
        console.select_chat(Some(second)).unwrap();
        console.show_focused_menu(menu(Uuid::new_v4())).unwrap();
        console.select_chat(Some(first)).unwrap();
        assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
        console.clear_menu(request).unwrap();
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.menu.as_ref().unwrap().id, id);
        let after = state
            .styled_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>();
        assert_eq!(before, after);
        if secret {
            state.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
            assert!(
                matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { comment: Some(value), .. }) if value == "private-credentia1l")
            );
        }
    }
}

#[test]
fn provider_vendor_headers_never_submit_and_model_rows_keep_mouse_identity() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    let id = Uuid::new_v4();
    console
        .show_presented_picker_menu(
            Menu {
                id,
                title: "Providers".into(),
                context: Vec::new(),
                options: vec!["First connection".into(), "Second connection".into()],
                selected: 0,
                confirmation: false,
            },
            PickerKind::Provider,
            PickerPresentation {
                headers: vec![(0, "Vendor · 1/2 configured".into())],
                hint: "Enter models · Esc close".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let mut state = console.state.lock().unwrap();
    state.draw(&mut Vec::new()).unwrap();
    let header_row = state
        .rendered_screen
        .iter()
        .position(|line| line.contains("Vendor ·"))
        .unwrap();
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: header_row as u16,
            column: 3,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(state.pointer_input.is_none());
    let row = state
        .rendered_screen
        .iter()
        .position(|line| line.contains("Second connection"))
        .unwrap();
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: row as u16,
            column: 3,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(
        matches!(state.pointer_input, Some(InputEvent::Choice { menu_id, index: 1, .. }) if menu_id == id)
    );
}

#[tokio::test]
async fn exit_plan_mode_automatically_opens_the_agent_dialog_and_preserves_it_across_switches() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus};
    use tickloop_agent_protocol::EventKind;
    use tickloop_agent_protocol::ExecutionMode;
    let directory = tempfile::tempdir().unwrap();
    let markdown = (0..30)
        .map(|index| format!("Plan line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::submit_plan("plan", &markdown),
        tickloop_agent_test_support::text("Implementation started."),
    ]));
    let agent =
        ConsoleExecution::new_embedded(tickloop_agent_test_support::config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let other = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Plan the task".into())
        .await
        .unwrap();
    let request = loop {
        if let EventKind::PermissionRequested { request } =
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
        {
            break request;
        }
    };
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    console.select_chat(Some(chat)).unwrap();
    console
        .state
        .lock()
        .unwrap()
        .insert("Unsent planning draft");
    let user_menu = menu(Uuid::new_v4());
    let user_id = user_menu.id;
    console.show_menu(user_menu).unwrap();
    let mut menus = Menus::default();
    let pending = HashMap::from([(
        request.id,
        crate::terminal::PermissionRequestOwner::new(chat, &request),
    )]);
    menus
        .refresh_family_permissions(&agent, &console, &[chat], &pending, &HashMap::new())
        .await
        .unwrap();
    let approval_id = console.dialog_menu_id().unwrap().unwrap();
    {
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.request_layout, Some(request.id));
        assert_eq!(
            state.styled_frame().0[0].spans[0].role,
            Some(Role::PlanMode)
        );
        assert!(state.menu_plan);
        assert_eq!(
            state.menu_comment.as_ref().unwrap().options,
            [super::super::workflows::PlanApprovalChoice::KeepPlanning.option_index()]
        );
        assert_eq!(
            state.input_dialog_frame().0.len(),
            input_dialog::FIXED_INPUT_DIALOG_ROWS
        );
        let frame = state
            .input_dialog_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(frame.contains("Plan approval"), "{frame}");
        assert!(frame.contains("Plan line 0"), "{frame}");
        state.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(state.full_review);
        state.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let full = state
            .full_review_frame()
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full.contains("Plan line 29"), "{full}");
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .is_none()
        );
        assert!(!state.full_review);
    }
    console.select_chat(Some(other)).unwrap();
    menus
        .refresh_family_permissions(&agent, &console, &[other], &pending, &HashMap::new())
        .await
        .unwrap();
    assert!(console.dialog_menu_id().unwrap().is_none());
    console.select_chat(Some(chat)).unwrap();
    menus
        .refresh_family_permissions(&agent, &console, &[chat], &pending, &HashMap::new())
        .await
        .unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(approval_id));
    let choice = {
        let mut state = console.state.lock().unwrap();
        assert_eq!(
            state.styled_frame().0[0].spans[0].role,
            Some(Role::PlanMode)
        );
        assert!(state.menu_plan);
        assert_eq!(state.menu_comment.as_ref().unwrap().label, "Feedback");
        state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap()
    };
    let InputEvent::Choice {
        menu_id,
        index,
        comment,
    } = choice
    else {
        panic!("expected plan choice");
    };
    assert_eq!(index, 1);
    menus
        .choose(&agent, &console, menu_id, Some(index), comment)
        .await
        .unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    {
        let mut state = console.state.lock().unwrap();
        state.release_finished_submissions();
        assert_eq!(state.editor.draft(), "Unsent planning draft");
        assert_eq!(state.menu.as_ref().unwrap().id, user_id);
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::AcceptEdits
    );
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    let saved = super::super::workflows::Workflow::plan(&agent, chat, request.loop_id)
        .await
        .unwrap();
    menus.workflow(&console, saved).unwrap();
    {
        let mut state = console.state.lock().unwrap();
        assert_eq!(
            state.styled_frame().0[0].spans[0].role,
            Some(Role::PlanMode)
        );
        assert!(state.menu_plan);
        assert!(state.menu_comment.is_none());
        assert_eq!(state.menu.as_ref().unwrap().options, ["Close"]);
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn restored_pending_plan_automatically_uses_the_same_approval_dialog() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus};
    use tickloop_agent_protocol::EventKind;
    use tickloop_agent_protocol::ExecutionMode;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let agent = ConsoleExecution::open_embedded(
        root.clone(),
        tickloop_agent_test_support::persisted_config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::submit_plan("plan", "Saved plan for restored approval."),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "Plan".into()).await.unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::PermissionRequested { .. }
        ) {
            break;
        }
    }
    agent.shutdown().await;
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::text("Restored execution started."),
    ]));
    let restored = ConsoleExecution::open_embedded(
        root,
        tickloop_agent_test_support::persisted_config(),
        model.clone(),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    let console = tests::console_with_state(State::new(true, 80, 24));
    console.select_chat(Some(chat)).unwrap();
    let mut menus = Menus::default();
    menus
        .refresh_family_permissions(
            &restored,
            &console,
            &[chat],
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();
    let id = console
        .dialog_menu_id()
        .unwrap()
        .expect("restored approval opens automatically");
    assert!(model.requests.lock().unwrap().is_empty());
    assert_eq!(
        console.state.lock().unwrap().menu.as_ref().unwrap().title,
        "Plan approval"
    );
    let mut events = restored.subscribe();
    let effect = menus
        .choose(&restored, &console, id, Some(1), None)
        .await
        .unwrap();
    assert_eq!(effect.receipt.unwrap().loop_id, receipt.loop_id);
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(
        restored.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::AcceptEdits
    );
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    restored.shutdown().await;
}

#[test]
fn plan_feedback_uses_text_editing_and_esc_closes_the_field_before_cancelling() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let mut plan = menu(Uuid::new_v4());
    plan.options = vec![
        "Auto".into(),
        "Ask".into(),
        "Full Access".into(),
        "No, keep planning".into(),
    ];
    plan.selected = 3;
    plan.confirmation = true;
    let id = plan.id;
    console.show_plan_menu(plan, true).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(
        state.styled_frame().0[0].spans[0].role,
        Some(Role::PlanMode)
    );
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.menu_comment_is_editing());
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    state.paste(" feedback 中文");
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_none()
    );
    assert!(!state.menu_comment_is_editing());
    assert_eq!(
        state.selected_menu_comment().as_deref(),
        Some("j feedback 中文")
    );
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, index: 3, comment: Some(feedback) }) if menu_id == id && feedback == "j feedback 中文")
    );
}

#[tokio::test]
async fn copied_loop_ids_keep_distinct_pending_plan_dialogs_for_each_agent() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus};
    use tickloop_agent_protocol::ExecutionMode;
    use tickloop_agent_protocol::{BranchRequest, EventKind};
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::open_embedded(
        directory.path().join("state"),
        tickloop_agent_test_support::persisted_config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::submit_plan("plan", "Shared pending plan."),
            tickloop_agent_test_support::text("Copy implementation started."),
        ])),
    )
    .unwrap();
    let source = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(source, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(source, None, "Plan".into()).await.unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::PermissionRequested { .. }
        ) {
            break;
        }
    }
    agent
        .decline_plan(source, receipt.loop_id, 1, None)
        .await
        .unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::Interrupted
        ) {
            break;
        }
    }
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            source_loop_id: None,
            name: None,
        })
        .await
        .unwrap();
    let copy = branch.child_chat_id;
    assert_eq!(branch.snapshot.plans[0].loop_id, receipt.loop_id);
    agent.snapshot(source).await.unwrap();
    agent.snapshot(copy).await.unwrap();
    let console = tests::console_with_state(State::new(true, 80, 24));
    let mut menus = Menus::default();
    console.select_chat(Some(source)).unwrap();
    menus
        .refresh_family_permissions(
            &agent,
            &console,
            &[source],
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();
    let source_menu = console.dialog_menu_id().unwrap().unwrap();
    console.select_chat(Some(copy)).unwrap();
    menus
        .refresh_family_permissions(&agent, &console, &[copy], &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    let copy_menu = console.dialog_menu_id().unwrap().unwrap();
    assert_ne!(copy_menu, source_menu);
    menus
        .choose(&agent, &console, copy_menu, Some(1), None)
        .await
        .unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert!(agent.snapshot(source).await.unwrap().plans[0].approval_pending);
    assert_eq!(
        agent.snapshot(source).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    console.select_chat(Some(source)).unwrap();
    menus
        .refresh_family_permissions(
            &agent,
            &console,
            &[source],
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(source_menu));
    agent.shutdown().await;
}

#[tokio::test]
async fn another_view_decision_retires_a_restored_plan_dialog_and_stale_esc_cannot_repend_it() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus};
    use tickloop_agent_protocol::EventKind;
    use tickloop_agent_protocol::ExecutionMode;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let original = ConsoleExecution::open_embedded(
        root.clone(),
        tickloop_agent_test_support::persisted_config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::submit_plan("plan", "Review this plan."),
        ])),
    )
    .unwrap();
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = original.subscribe();
    let run = original.submit(chat, None, "Plan".into()).await.unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::PermissionRequested { .. }
        ) {
            break;
        }
    }
    original.shutdown().await;
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::text("Implementation started."),
    ]));
    let agent = ConsoleExecution::open_embedded(
        root,
        tickloop_agent_test_support::persisted_config(),
        model.clone(),
    )
    .unwrap();
    agent.resume_chat(chat).await.unwrap();
    let first = tests::console_with_state(State::new(true, 80, 24));
    let second = tests::console_with_state(State::new(true, 80, 24));
    first.select_chat(Some(chat)).unwrap();
    second.select_chat(Some(chat)).unwrap();
    {
        let mut state = second.state.lock().unwrap();
        state.insert("Keep this second View draft");
        state.last_input_edit = None;
    }
    let mut first_menus = Menus::default();
    let mut second_menus = Menus::default();
    first_menus
        .refresh_family_permissions(&agent, &first, &[chat], &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    second_menus
        .refresh_family_permissions(&agent, &second, &[chat], &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    let first_id = first.dialog_menu_id().unwrap().unwrap();
    let stale_id = second.dialog_menu_id().unwrap().unwrap();
    let mut events = agent.subscribe();
    first_menus
        .choose(&agent, &first, first_id, Some(1), None)
        .await
        .unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    agent.snapshot(chat).await.unwrap();
    second_menus
        .refresh_family_permissions(&agent, &second, &[chat], &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert!(
        second.dialog_menu_id().unwrap().is_none(),
        "the other View still owns a processed request"
    );
    assert_eq!(second.state.lock().unwrap().request_queue_count, 0);
    second_menus
        .choose(&agent, &second, stale_id, None, None)
        .await
        .unwrap();
    assert!(
        agent
            .decline_plan(chat, run.loop_id, 1, None)
            .await
            .is_err()
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(!snapshot.plans[0].approval_pending);
    assert_eq!(snapshot.policy.mode, ExecutionMode::AcceptEdits);
    assert_eq!(
        second.state.lock().unwrap().editor.draft(),
        "Keep this second View draft"
    );
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    agent.shutdown().await;
}

#[test]
fn a_child_can_request_an_existing_plan_without_unlocking_mode_changes() {
    use crate::terminal::completion::{CommandScope, unavailable_command};
    assert!(unavailable_command("/plan", CommandScope::Child).is_none());
    assert!(
        unavailable_command(
            "/plan 00000000-0000-0000-0000-000000000001",
            CommandScope::Child
        )
        .is_none()
    );
    for command in ["/mode auto", "/rule allow Bash", "/add-dir /tmp"] {
        assert!(unavailable_command(command, CommandScope::Child).is_some());
    }
}

#[tokio::test]
async fn a_child_reopens_its_dismissed_approval_but_cannot_enter_plan_mode_without_a_plan() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus, open_current_plan};
    use tickloop_agent_protocol::EventKind;
    use tickloop_agent_protocol::{ExecutionMode, PermissionScope};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call(
            "delegate",
            "Subagent",
            serde_json::json!({"task":"Plan the delegated task."}),
        ),
        tickloop_agent_test_support::call("enter", "EnterPlanMode", serde_json::json!({})),
        tickloop_agent_test_support::submit_plan("plan", "Child plan to review."),
        tickloop_agent_test_support::text("The parent observed the interrupted child."),
        tickloop_agent_test_support::call(
            "execute",
            "Write",
            serde_json::json!({"file_path":"child-executed.txt","content":"approved child implementation"}),
        ),
        tickloop_agent_test_support::text("Approved child implementation started."),
        tickloop_agent_test_support::text("The parent observed completion."),
    ]));
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        model.clone(),
    );
    let agent =
        ConsoleExecution::from_embedded(core.clone(), tickloop_agent_test_support::config());
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    agent
        .submit(parent, None, "Delegate a plan".into())
        .await
        .unwrap();
    let (child, enter) = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => break (event.chat_id, request),
            EventKind::ToolCompleted { result, .. } if result.is_error => {
                panic!("child setup failed: {}", result.content)
            }
            EventKind::Completed { .. } => panic!("parent completed before child approval request"),
            EventKind::Failed(error) => panic!("child setup failed: {error}"),
            _ => {}
        }
    };
    let console = tests::console_with_state(State::new(true, 80, 24));
    console.select_chat(Some(child)).unwrap();
    console.set_visible_delegation(true).unwrap();
    let mut menus = Menus::default();
    assert!(
        open_current_plan(
            &agent,
            &console,
            &mut menus,
            Some(child),
            Some(enter.loop_id)
        )
        .await
        .is_err()
    );
    assert_eq!(
        agent.snapshot(child).await.unwrap().policy.mode,
        ExecutionMode::FullAccess
    );
    agent
        .answer_permission_with_comment(child, enter.id, true, PermissionScope::Once, None)
        .await
        .unwrap();
    let approval = loop {
        if let EventKind::PermissionRequested { request } =
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
        {
            break request;
        }
    };
    assert_eq!(approval.name, "ExitPlanMode");
    agent
        .decline_plan(child, approval.loop_id, 1, None)
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == child && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert!(
        crate::terminal::completion::unavailable_command("/plan", console.command_scope())
            .is_none()
    );
    open_current_plan(
        &agent,
        &console,
        &mut menus,
        Some(child),
        Some(approval.loop_id),
    )
    .await
    .unwrap();
    let id = console
        .dialog_menu_id()
        .unwrap()
        .expect("child pending approval must reopen");
    assert_eq!(
        console.state.lock().unwrap().menu.as_ref().unwrap().title,
        "Plan approval"
    );
    assert_eq!(
        agent.snapshot(child).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    assert!(!directory.path().join("child-executed.txt").exists());
    menus
        .choose(&agent, &console, id, Some(2), None)
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == child && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert!(!agent.snapshot(child).await.unwrap().plans[0].approval_pending);
    assert_eq!(
        agent.snapshot(child).await.unwrap().policy.mode,
        ExecutionMode::FullAccess
    );
    assert_eq!(
        agent.snapshot(parent).await.unwrap().policy.mode,
        ExecutionMode::FullAccess
    );
    let child_snapshot = core.snapshot(child).await.unwrap();
    let parent_snapshot = core.snapshot(parent).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.path().join("child-executed.txt")).unwrap_or_else(
            |error| panic!(
                "{error}: child operations {:?}; parent operations {:?}; model requests {}",
                child_snapshot.operations,
                parent_snapshot.operations,
                model.requests.lock().unwrap().len()
            )
        ),
        "approved child implementation"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_new_plan_version_replaces_a_stale_restored_request_in_another_view() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus};
    use tickloop_agent_protocol::EventKind;
    use tickloop_agent_protocol::ExecutionMode;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let original = ConsoleExecution::open_embedded(
        root.clone(),
        tickloop_agent_test_support::persisted_config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::submit_plan("plan", "First plan."),
        ])),
    )
    .unwrap();
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = original.subscribe();
    let run = original.submit(chat, None, "Plan".into()).await.unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::PermissionRequested { .. }
        ) {
            break;
        }
    }
    original.shutdown().await;
    let agent = ConsoleExecution::open_embedded(
        root,
        tickloop_agent_test_support::persisted_config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::call(
                "read",
                "Read",
                serde_json::json!({"file_path":tickloop_agent_test_support::PLAN_FILE}),
            ),
            tickloop_agent_test_support::submit_plan("revised", "Revised plan."),
            tickloop_agent_test_support::text("Implementation started."),
        ])),
    )
    .unwrap();
    agent.resume_chat(chat).await.unwrap();
    let console = tests::console_with_state(State::new(true, 80, 24));
    console.select_chat(Some(chat)).unwrap();
    let mut menus = Menus::default();
    menus
        .refresh_family_permissions(&agent, &console, &[chat], &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    let old_id = console.dialog_menu_id().unwrap().unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, Some(run.loop_id), "Revise in another View".into())
        .await
        .unwrap();
    let revised = loop {
        if let EventKind::PermissionRequested { request } =
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
        {
            break request;
        }
    };
    agent.snapshot(chat).await.unwrap();
    menus.note_request(chat, revised.id);
    let pending = HashMap::from([(
        revised.id,
        crate::terminal::PermissionRequestOwner::new(chat, &revised),
    )]);
    menus
        .refresh_family_permissions(&agent, &console, &[chat], &pending, &HashMap::new())
        .await
        .unwrap();
    let new_id = console.dialog_menu_id().unwrap().unwrap();
    assert_ne!(old_id, new_id);
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .menu
            .as_ref()
            .unwrap()
            .context
            .iter()
            .any(|line| line == "Plan version: 2")
    );
    assert_eq!(console.state.lock().unwrap().request_queue_count, 1);
    menus
        .choose(&agent, &console, old_id, None, None)
        .await
        .unwrap();
    assert!(
        agent
            .decline_plan(chat, run.loop_id, 1, None)
            .await
            .is_err()
    );
    assert!(agent.snapshot(chat).await.unwrap().plans[0].approval_pending);
    menus
        .choose(&agent, &console, new_id, Some(1), None)
        .await
        .unwrap();
    loop {
        if matches!(
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(agent.snapshot(chat).await.unwrap().plans[0].version, 2);
    agent.shutdown().await;
}

#[test]
fn input_dialog_finished_settings_transition_accepts_the_next_key_without_idle() {
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    let first = Uuid::new_v4();
    let next = Uuid::new_v4();
    console
        .show_picker_menu(menu(first), PickerKind::Actions)
        .unwrap();
    assert!(
        matches!(console.state.lock().unwrap().key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, .. }) if menu_id == first)
    );
    console
        .show_picker_menu(menu(next), PickerKind::Actions)
        .unwrap();
    console.finish_menu_submission(first).unwrap();
    assert!(
        matches!(console.state.lock().unwrap().key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(InputEvent::Cancel { menu_id }) if menu_id == next)
    );
}

#[test]
fn input_dialog_login_updates_keep_callback_editing_and_origin() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let login = Uuid::new_v4();
    let request = Uuid::new_v4();
    let mut state = State::new(true, 100, 24);
    state.chat = Some(first);
    let console = tests::console_with_state(state);
    let mut page = menu(login);
    page.title = "Browser login".into();
    page.context = vec!["https://fixture.test/authorize".into()];
    page.options = vec!["Submit callback".into(), "Cancel login".into()];
    console
        .show_login_menu(page.clone(), "https://fixture.test/authorize", true)
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        state.paste("fixture callback draft");
    }
    page.context.push("Callback validation failed".into());
    console
        .show_login_menu(page, "https://fixture.test/authorize", true)
        .unwrap();
    {
        let state = console.state.lock().unwrap();
        assert!(state.menu_comment_is_editing());
        assert_eq!(
            state.selected_menu_comment().as_deref(),
            Some("fixture callback draft")
        );
    }
    console.set_request_layout(Some(request)).unwrap();
    console.show_focused_menu(menu(request)).unwrap();
    let mut hidden_update = menu(login);
    hidden_update.context = vec!["https://fixture.test/new-authorize".into()];
    console
        .show_login_menu(
            hidden_update.clone(),
            "https://fixture.test/new-authorize",
            true,
        )
        .unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
    console
        .complete_login_menu(login, "Login saved", "Fixture account")
        .unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
    console.clear_menu(request).unwrap();
    console.select_chat(Some(second)).unwrap();
    console
        .show_login_menu(hidden_update, "https://fixture.test/new-authorize", true)
        .unwrap();
    console
        .complete_login_menu(login, "Login failed", "Controlled late result")
        .unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), None);
    console.select_chat(Some(first)).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.menu.as_ref().unwrap().id, login);
    assert!(
        state
            .menu
            .as_ref()
            .unwrap()
            .context
            .iter()
            .any(|line| line.contains("Esc leaves editing first"))
    );
    assert_eq!(
        state
            .menu_link
            .as_ref()
            .map(|(_, address)| address.as_str()),
        Some("https://fixture.test/new-authorize")
    );
    assert!(state.menu_comment_is_editing());
    assert!(
        state
            .styled_frame()
            .0
            .last()
            .unwrap()
            .text()
            .contains("Enter return")
    );
    assert_eq!(
        state.selected_menu_comment().as_deref(),
        Some("fixture callback draft")
    );
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_none()
    );
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(InputEvent::Cancel { menu_id }) if menu_id == login)
    );
}

#[test]
fn input_dialog_grouped_models_refresh_only_supported_rows() {
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    let id = Uuid::new_v4();
    console
        .show_presented_picker_menu(
            menu(id),
            PickerKind::Model,
            PickerPresentation {
                refresh: true,
                refresh_options: Some(vec![1]),
                row_hints: vec!["p Connection".into(), "p Connection · r Refresh".into()],
                ..Default::default()
            },
        )
        .unwrap();
    let mut state = console.state.lock().unwrap();
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .is_none()
    );
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        Some(InputEvent::PickerAction {
            index: 1,
            action: PickerAction::RefreshAccountModels,
            ..
        })
    ));
}

#[test]
fn input_dialog_continuing_submission_releases_only_its_fence() {
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    let login = Uuid::new_v4();
    let mut page = menu(login);
    page.context = vec!["https://fixture.test/authorize".into()];
    console
        .show_login_menu(page, "https://fixture.test/authorize", true)
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        state.paste("callback draft");
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Choice { menu_id, .. }) if menu_id == login)
        );
    }
    console.continue_menu_submission(Uuid::new_v4()).unwrap();
    assert_eq!(
        console.state.lock().unwrap().menu_submission_pending,
        Some(login)
    );
    console.continue_menu_submission(login).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.menu.as_ref().unwrap().id, login);
    assert!(state.menu_submission_pending.is_none());
    assert!(state.menu_comment_is_editing());
    assert_eq!(
        state.selected_menu_comment().as_deref(),
        Some("callback draft")
    );
    state.release_finished_submissions();
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_none()
    );
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(InputEvent::Cancel { menu_id }) if menu_id == login)
    );
}

#[tokio::test]
async fn input_dialog_completed_login_keeps_other_agent_composer_enabled() {
    use crate::terminal::login::{LoginFlow, LoginMethod, complete_provider_login};
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut state = State::new(true, 100, 24);
    state.chat = Some(first);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    let (flow, _) = LoginFlow::start_fixture(&console, LoginMethod::Device).unwrap();
    let id = flow.id();
    let mut login = Some(flow);
    complete_provider_login(
        login.as_mut().unwrap(),
        &console,
        &Ok("Fixture account".into()),
    )
    .unwrap();
    console.select_chat(Some(second)).unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            state.editor.draft(),
            "x",
            "A hidden login must not disable B's Composer"
        );
    }
    console.select_chat(Some(first)).unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(id));
}

#[tokio::test]
async fn input_dialog_background_provider_completion_updates_only_its_origin_page() {
    use crate::terminal::login::{LoginFlow, LoginMethod, LoginSession};
    let directory = tempfile::tempdir().unwrap();
    let agent = crate::terminal::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let first = agent.create_chat(directory.path().into()).await.unwrap();
    let second = agent.create_chat(directory.path().into()).await.unwrap();
    let mut state = State::new(true, 100, 24);
    state.chat = Some(first);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    for result in [
        Ok("Fixture source account".into()),
        Err("Fixture source refusal".into()),
    ] {
        console.select_chat(Some(first)).unwrap();
        let (flow, _) = LoginFlow::start_fixture(&console, LoginMethod::Device).unwrap();
        let id = flow.id();
        let mut session = LoginSession {
            flow,
            from_provider: true,
            succeeded: false,
        };
        console.select_chat(Some(second)).unwrap();
        let before = console.state.lock().unwrap().scrollback.line_count();
        crate::terminal::finish_provider_login(&agent, &console, &mut session, &result)
            .await
            .unwrap();
        assert_eq!(
            console.state.lock().unwrap().scrollback.line_count(),
            before,
            "A's result must not enter B's transcript"
        );
        assert_eq!(console.dialog_menu_id().unwrap(), None);
        console.select_chat(Some(first)).unwrap();
        assert_eq!(console.dialog_menu_id().unwrap(), Some(id));
        console.clear_menu(id).unwrap();
    }
    agent.shutdown().await;
}

#[test]
fn settings_scroll_new_generation_preserves_context_without_reusing_row_identity() {
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    let mut first = menu(Uuid::new_v4());
    first.title = "Settings".into();
    first.context = (0..20)
        .map(|index| format!("Explanation {index}"))
        .collect();
    first.options = vec!["Auto-scroll".into(), "Reduce motion".into()];
    console
        .show_picker_menu(first.clone(), PickerKind::Actions)
        .unwrap();
    let before = {
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        state.context_offset
    };
    assert!(before > 0);
    let mut next = first;
    next.id = Uuid::new_v4();
    next.selected = 1;
    console
        .show_picker_menu(next.clone(), PickerKind::Actions)
        .unwrap();
    {
        let state = console.state.lock().unwrap();
        assert_eq!(state.context_offset, before);
        assert_eq!(state.menu.as_ref().unwrap().id, next.id);
        assert_eq!(state.menu.as_ref().unwrap().selected, 1);
    }
    next.id = Uuid::new_v4();
    next.title = "Thinking".into();
    console.show_picker_menu(next, PickerKind::Actions).unwrap();
    assert_eq!(console.state.lock().unwrap().context_offset, 0);
}
