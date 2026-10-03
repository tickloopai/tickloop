use super::*;
use crate::tick_adapter::{
    PreparedTickAction, TickActionBackend, TickActionContext, TickActionExecutor,
    TickActionOutcome, TickActionUi, TickListAction, TickListOperation,
};

struct FixtureTickAction(String);
impl TickActionExecutor for FixtureTickAction {
    fn prepare(&self, _: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            &self.0,
            'o',
            Arc::new(FixtureTickBackend),
        ))
    }
}
struct FixtureTickBackend;
#[async_trait::async_trait]
impl TickActionBackend for FixtureTickBackend {
    async fn execute(&self, _: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        Ok(TickActionOutcome::Done)
    }
}

pub(super) fn fixture_ticks() -> Vec<TickListEntry> {
    (0..2)
        .map(|index| TickListEntry {
            id: format!("fixture-tick-{index}"),
            group: TickGroup::Artifacts,
            glyph: Glyph::ArtifactTick,
            title: format!("Fixture Tick {index}"),
            kind: "Document".into(),
            status: "read".into(),
            updated_at_ms: None,
            actions: vec![TickListAction {
                key: 'o',
                label: "open",
                operation: TickListOperation::Execute(Arc::new(FixtureTickAction(format!(
                    "fixture-tick-{index}"
                )))),
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
        state
            .tick_list
            .replace(TickGroup::Artifacts, fixture_ticks());
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
        assert!(state.tick_list.focused && state.tick_list_visible());
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
        state.paste("Tick List still owns input");
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
        test_support,
    };
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(Vec::new())),
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
    use crate::terminal::{execution::ConsoleExecution, test_support};
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(Vec::new())),
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
    use crate::terminal::{execution::ConsoleExecution, test_support};
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(Vec::new())),
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
        test_support,
    };
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(Vec::new())),
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
            "tick list",
        ] {
            for cancel in [false, true] {
                let (width, height) = if surface == "tick list" {
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
                    "tick list" => {
                        let mut state = console.state.lock().unwrap();
                        state
                            .tick_list
                            .replace(TickGroup::Artifacts, fixture_ticks());
                        state.tick_list.focus(true, None);
                        assert!(state.tick_list_visible());
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
                if surface == "tick list" {
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
    use crate::terminal::{execution::ConsoleExecution, test_support};
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(Vec::new())),
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
    use tickloop_agent::runtime::ShellTaskState;
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
fn input_dialog_replaces_the_footer_with_a_fixed_height_and_preserves_the_draft() {
    for (width, height, expected) in [(80, 24, 12), (120, 40, 12), (30, 10, 6)] {
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
        console.state.lock().unwrap().styled_frame().0[0]
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
        state.rendered_screen[..12]
            .iter()
            .any(|line| line.contains("conversation remains visible"))
    );
    assert!(state.rendered_screen[12].starts_with("Resume Chat"));
    assert_eq!(state.styled_frame().0.len(), 12);
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
fn workspace_trust_and_subscriptions_use_the_fixed_dialog_height() {
    let mut state = State::new(true, 80, 24);
    state.workspace_trust = Some(WorkspaceTrust::new(PathBuf::from("/fixture/project")));
    assert_eq!(state.styled_frame().0.len(), 12);
    let console = tests::console_with_state(State::new(true, 120, 40));
    let chat = Uuid::new_v4();
    console.select_chat(Some(chat)).unwrap();
    console.show_subscriptions(chat, Vec::new()).unwrap();
    assert_eq!(console.state.lock().unwrap().styled_frame().0.len(), 12);
}

#[test]
fn sticky_prompt_short_conversation_keeps_the_full_content_height() {
    let console = tests::console_with_state(State::new(true, 160, 24));
    console.select_chat(Some(Uuid::new_v4())).unwrap();
    console.show_focused_menu(menu(Uuid::new_v4())).unwrap();
    let mut state = console.state.lock().unwrap();
    state
        .tick_list
        .replace(TickGroup::Artifacts, fixture_ticks());
    state.scrollback.append("Leading boundary");
    state.record_input("Short user request");
    for index in 0..9 {
        state.scrollback.append(&format!("Short reply {index}\n"));
    }
    state.draw(&mut Vec::new()).unwrap();
    assert!(state.rendered_screen[0].starts_with("Leading boundary"));
    assert!(state.rendered_screen[0].contains('│'));
    assert!(
        state.rendered_screen[..12]
            .iter()
            .any(|row| row.contains("Fixture Tick 0"))
    );
    assert!(state.rendered_screen[1].starts_with("❯ Short user request"));
    assert_eq!(
        state
            .rendered_screen
            .iter()
            .filter(|row| row.contains("Short user request"))
            .count(),
        1
    );
    assert!(state.rendered_screen[12].starts_with("Settings"));
}

#[test]
fn sticky_prompt_loaded_history_tracks_the_original_message_row() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let chat = Uuid::new_v4();
    let agent_message = Uuid::new_v4();
    console.select_chat(Some(chat)).unwrap();
    let snapshot: crate::runtime::ChatSnapshot = serde_json::from_value(serde_json::json!({
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
    state.tick_list.replace(
        TickGroup::Shells,
        vec![TickListEntry {
            id: "fixture".into(),
            group: TickGroup::Shells,
            glyph: Glyph::ShellTick,
            title: "Fixture Shell".into(),
            kind: "Shell".into(),
            status: "running".into(),
            updated_at_ms: None,
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
    assert!(state.rendered_screen[12].starts_with("Settings"));
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
    let console = tests::console_with_state(State::new(true, 80, 24));
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
    state.resize(80, 25);
    for _ in 0..3 {
        state.draw(&mut Vec::new()).unwrap();
        assert!(state.rendered_screen[0].starts_with("❯ Boundary request"));
        assert!(!state.rendered_screen[0].contains("continuation visible"));
        assert!(state.rendered_screen[1].contains("continuation visible"));
    }
    state.resize(80, 24);
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
    let console = tests::console_with_state(State::new(true, 80, 24));
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
    state.resize(40, 24);
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
    state.resize(80, 24);
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
        .select_artifact_tick(picker_id, owner, "fixture-artifact")
        .unwrap();
    assert_eq!(console.dialog_chat().unwrap(), Some(visible));
    assert_eq!(console.dialog_menu_id().unwrap(), Some(visible_menu));
    console.select_chat(None).unwrap();
    assert!(console.state.lock().unwrap().artifacts_picker.is_none());
}

#[test]
fn provider_model_and_connection_dialogs_have_one_fixed_container_and_isolate_input() {
    for kind in [PickerKind::Provider, PickerKind::Model, PickerKind::Actions] {
        for (width, height, expected) in [(80, 24, 12), (120, 40, 12), (32, 10, 6)] {
            let mut state = State::new(true, width, height);
            state.chat = Some(Uuid::new_v4());
            state.prompt_enabled = true;
            state.insert("unsent Chat draft");
            state.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
            let cursor = state.editor.cursor();
            state
                .tick_list
                .replace(TickGroup::Artifacts, fixture_ticks());
            state.tick_list.focus(true, None);
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
            assert!(lines[0].text().contains("Builtin picker"));
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
            assert!(state.tick_list.focused);
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
    assert_eq!(lines.len(), input_dialog::INPUT_DIALOG_ROWS);
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
    assert!(closed.contains("Esc cancel"), "{closed}");
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
