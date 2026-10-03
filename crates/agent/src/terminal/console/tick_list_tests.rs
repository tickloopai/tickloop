use super::*;
use crate::terminal::tick_list::{TickGroup, TickListEntry};

fn shell(id: Uuid, title: &str) -> TickListEntry {
    TickListEntry {
        id: id.to_string(),
        group: TickGroup::Shells,
        glyph: Glyph::ShellTick,
        title: title.into(),
        kind: "Shell".into(),
        status: "exit 0".into(),
        updated_at_ms: Some(1000),
        running: false,
    }
}

#[test]
fn shell_tick_enter_opens_inside_the_tick_panel() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    state.tick_list.focus(false, None);
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::OpenTickShell { .. })
    ));
    assert!(state.tick_list.shell_detail.is_some());
    assert!(state.task_surface.is_none());
}

#[test]
fn shell_ticks_only_list_background_executions_and_keep_them_until_removed() {
    use crate::runtime::{ShellTaskOrigin, ShellTaskOwner, ShellTaskState};
    for origin in [ShellTaskOrigin::Agent, ShellTaskOrigin::User] {
        let chat = Uuid::new_v4();
        let mut snapshot = tests::task_snapshot(chat, 1);
        snapshot.tasks[0].origin = origin;
        snapshot.tasks[0].owner = match origin {
            ShellTaskOrigin::Agent => ShellTaskOwner::Agent {
                loop_id: Uuid::new_v4(),
            },
            ShellTaskOrigin::User => ShellTaskOwner::User { chat_id: chat },
        };
        snapshot.tasks[0].was_background = false;
        snapshot.tasks[0].state = ShellTaskState::Running;
        let console = Console::test_for_chat(chat);
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            !console.tick_list_has_entries().unwrap(),
            "running foreground Shell must be hidden"
        );
        snapshot.tasks[0].state = ShellTaskState::Completed;
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            !console.tick_list_has_entries().unwrap(),
            "finished foreground Shell must be hidden"
        );
        snapshot.tasks[0].state = ShellTaskState::Background;
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            console.tick_list_has_entries().unwrap(),
            "moving foreground to background must show it"
        );
        snapshot.tasks[0].was_background = true;
        snapshot.tasks[0].state = ShellTaskState::Completed;
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            console.tick_list_has_entries().unwrap(),
            "finished background Shell must remain"
        );
        {
            let mut state = console.state.lock().unwrap();
            state.tick_list.focus(true, None);
            state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
            assert!(state.tick_list.entries().is_empty());
        }
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            !console.tick_list_has_entries().unwrap(),
            "removed background Shell must not reappear"
        );
        snapshot.tasks[0].id = Uuid::new_v4();
        snapshot.tasks[0].state = ShellTaskState::Running;
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            console.tick_list_has_entries().unwrap(),
            "background start is shown even with running state"
        );
    }
}

#[test]
fn tick_shell_detail_and_selection_restore_per_agent_and_ignore_obsolete_mouse_targets() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    let a = state.chat.unwrap();
    let selected = state.tick_list.selected().unwrap().id.clone();
    state.tick_list.focus(false, None);
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let panel = state.tick_list.shell_detail.as_ref().unwrap().id;
    state.draw_frame(&mut Vec::new()).unwrap();
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.tick_list.selected().unwrap().id, selected);
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_ne!(state.tick_list.shell_detail.as_ref().unwrap().id, panel);
    let panel = state.tick_list.shell_detail.as_ref().unwrap().id;
    let output = (0..40)
        .map(|row| format!("cached-output-{row}\n"))
        .collect::<String>();
    let mut snapshot = tests::task_snapshot(a, 1);
    snapshot.tasks[0].id = Uuid::parse_str(&selected).unwrap();
    snapshot.tasks[0].output_bytes = output.len() as u64;
    snapshot.tasks[0].capabilities.can_read_output = true;
    let detail = state.tick_list.shell_detail.as_mut().unwrap();
    let Some(TaskSurfaceEffect::ReadOutput {
        generation,
        task_id,
        ..
    }) = detail.load(snapshot, Some(1000))
    else {
        panic!("missing cached detail output request");
    };
    detail.output(TaskOutputUpdate::Chunk {
        generation,
        chunk: crate::runtime::ShellTaskOutputChunk {
            task_id,
            offset: 0,
            confirmed_offset: output.len() as u64,
            total_bytes: output.len() as u64,
            next_offset: None,
            truncated: false,
            encoding: "utf-8".into(),
            output,
        },
    });
    detail.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), 40, 10);
    let before = detail
        .lines(40, 10, 2000)
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>();
    state.switch_view_chat(Some(Uuid::new_v4()));
    assert!(state.tick_list.shell_detail.is_none());
    state.switch_view_chat(Some(a));
    assert_eq!(state.tick_list.shell_detail.as_ref().unwrap().id, panel);
    assert_eq!(state.tick_list.selected().unwrap().id, selected);
    assert_eq!(
        state
            .tick_list
            .shell_detail
            .as_mut()
            .unwrap()
            .lines(40, 10, 2000)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>(),
        before
    );
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    state.draw_frame(&mut Vec::new()).unwrap();
    let target = &state.tick_row_targets[0];
    let (row, column) = (target.row, target.columns.start);
    let b = Uuid::new_v4();
    state.switch_view_chat(Some(b));
    state.tick_list.replace(
        TickGroup::Artifacts,
        vec![TickListEntry {
            id: "artifact:https://docs.example/b".into(),
            group: TickGroup::Artifacts,
            glyph: Glyph::ArtifactTick,
            title: "B".into(),
            kind: "Artifact".into(),
            status: String::new(),
            updated_at_ms: None,
            running: false,
        }],
    );
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: row as u16,
            column: column as u16,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(state.pointer_input.is_none());
    assert!(!state.tick_list.focused);
}

#[tokio::test]
async fn shell_panel_live_subscription_updates_tail_without_covering_the_input() {
    use crate::terminal::{
        execution::ConsoleExecution, task_access::ShellTaskCenter, test_support,
    };
    let directory = tempfile::tempdir().unwrap();
    let core = crate::runtime::Agent::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(Vec::new())),
    );
    let agent = ConsoleExecution::from_embedded(core.clone(), test_support::config());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let receipt = agent
        .submit_user_shell_for_goal(
            chat,
            None,
            "printf 'first\n'; while [ ! -f release ]; do sleep 0.02; done; printf 'live-tail\n'"
                .into(),
            true,
        )
        .await
        .unwrap();
    let center = ShellTaskCenter::new(agent);
    let initial = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = center.snapshot(chat).await.unwrap();
            if snapshot
                .tasks
                .iter()
                .any(|task| task.id == receipt.command_id && task.output_bytes > 0)
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let started = center.started_at_ms(chat, receipt.command_id).await;
    assert!(started.is_some());
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    console.update_shell_ticks(&initial).unwrap();
    let panel = {
        let mut state = console.state.lock().unwrap();
        state.tick_list.focus(false, None);
        let Some(InputEvent::OpenTickShell { panel, .. }) =
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Shell did not open in panel");
        };
        panel
    };
    let mut subscription = center.subscribe(chat).await.unwrap();
    let effect = console
        .load_tick_shell(chat, panel, initial, started)
        .unwrap();
    crate::terminal::apply_tick_shell_effect(&center, &console, chat, panel, effect)
        .await
        .unwrap();
    std::fs::write(directory.path().join("release"), "").unwrap();
    let updated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            subscription.recv().await.unwrap();
            let snapshot = center.snapshot(chat).await.unwrap();
            if snapshot
                .tasks
                .iter()
                .any(|task| task.id == receipt.command_id && task.output_bytes > 6)
            {
                break snapshot;
            }
        }
    })
    .await
    .unwrap();
    console.update_shell_ticks(&updated).unwrap();
    let effect = console.update_tick_shell(chat, panel, updated).unwrap();
    crate::terminal::apply_tick_shell_effect(&center, &console, chat, panel, effect)
        .await
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.draw_frame(&mut Vec::new()).unwrap();
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|row| row.contains("live-tail"))
        );
        assert!(state.task_surface.is_none());
        let (input, _, _) = state.styled_frame();
        let start = state.height - input.len();
        assert!(
            state.rendered_screen[..start]
                .iter()
                .all(|row| row.contains('│'))
        );
        assert!(
            state.rendered_screen[start..]
                .iter()
                .all(|row| !row.contains('│'))
        );
    }
    core.shutdown().await;
}

#[tokio::test]
async fn shared_shell_effect_handler_recovers_output_errors_for_task_center_and_tick_panel() {
    use crate::terminal::{
        ShellEffectTarget, apply_shell_effect, execution::ConsoleExecution,
        task_access::ShellTaskCenter, test_support,
    };
    let directory = tempfile::tempdir().unwrap();
    let core = crate::runtime::Agent::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(Vec::new())),
    );
    let agent = ConsoleExecution::from_embedded(core.clone(), test_support::config());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let receipt = agent
        .submit_user_shell_for_goal(chat, None, "printf recovered-output".into(), true)
        .await
        .unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = center.snapshot(chat).await.unwrap();
            if snapshot
                .tasks
                .iter()
                .any(|task| task.id == receipt.command_id && task.state.is_terminal())
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let path = agent
        .snapshot(chat)
        .await
        .unwrap()
        .user_shells
        .into_iter()
        .find(|record| record.id == receipt.command_id)
        .unwrap()
        .output_ref
        .unwrap();
    for panel_mode in [false, true] {
        let mut state = State::new(true, 140, 25);
        state.switch_view_chat(Some(chat));
        state.prompt_enabled = true;
        let console = tests::console_with_state(state);
        let (target, effect) = if panel_mode {
            console.update_shell_ticks(&snapshot).unwrap();
            let panel = {
                let mut state = console.state.lock().unwrap();
                state.tick_list.focus(false, None);
                let Some(InputEvent::OpenTickShell { panel, .. }) =
                    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                else {
                    panic!("missing panel");
                };
                panel
            };
            (
                ShellEffectTarget::TickPanel(panel),
                console
                    .load_tick_shell(chat, panel, snapshot.clone(), None)
                    .unwrap(),
            )
        } else {
            (
                ShellEffectTarget::TaskCenter,
                console
                    .show_tasks_at(snapshot.clone(), Some(receipt.command_id))
                    .unwrap(),
            )
        };
        let unavailable = path.with_extension("held-by-test");
        std::fs::rename(&path, &unavailable).unwrap();
        apply_shell_effect(&center, &console, chat, target, effect.clone())
            .await
            .unwrap();
        {
            let state = console.state.lock().unwrap();
            if panel_mode {
                let hint = state.tick_list.shell_detail.as_ref().unwrap().hint();
                assert!(hint.contains("Esc back"));
                assert_ne!(
                    hint, "Esc back",
                    "failed output must be surfaced in the panel"
                );
            } else {
                assert!(
                    state
                        .task_surface
                        .as_ref()
                        .unwrap()
                        .visible_error()
                        .is_some()
                );
            }
        }
        std::fs::rename(&unavailable, &path).unwrap();
        apply_shell_effect(&center, &console, chat, target, effect)
            .await
            .unwrap();
        {
            let mut state = console.state.lock().unwrap();
            state.draw_frame(&mut Vec::new()).unwrap();
            assert!(
                state
                    .rendered_screen
                    .iter()
                    .any(|row| row.contains("recovered-output"))
            );
            if panel_mode {
                assert_eq!(
                    state.tick_list.shell_detail.as_ref().unwrap().hint(),
                    "Esc back"
                );
            } else {
                assert!(
                    state
                        .task_surface
                        .as_ref()
                        .unwrap()
                        .visible_error()
                        .is_none()
                );
            }
        }
    }
    core.shutdown().await;
}

fn click_fixture(width: u16, overlay: bool, group: TickGroup) -> State {
    let mut state = State::new(true, width, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    let mut entry = shell(Uuid::new_v4(), "Clickable row");
    entry.group = group;
    if group == TickGroup::Artifacts {
        entry.id = "artifact:https://docs.example/doc".into();
        entry.glyph = Glyph::ArtifactTick;
    } else if group == TickGroup::Subscribed {
        entry.glyph = Glyph::ProtocolTick;
        entry.kind = "Tick".into();
    }
    state.tick_list.replace(group, vec![entry]);
    state.tick_list.overlay = overlay;
    state.draw_frame(&mut Vec::new()).unwrap();
    state
}

fn tick_action_signature(input: Option<InputEvent>) -> String {
    match input {
        Some(InputEvent::TickListAction { chat, id, action }) => format!("{chat}/{id}/{action}"),
        Some(InputEvent::OpenStatusTask(id)) => format!("shell/{id}"),
        Some(InputEvent::OpenTickShell { chat, task_id, .. }) => format!("{chat}/shell/{task_id}"),
        _ => panic!("expected a Tick row action"),
    }
}

#[test]
fn tick_row_title_and_metadata_clicks_perform_the_same_enter_action() {
    for (width, overlay) in [(140, false), (100, true)] {
        for group in [
            TickGroup::Artifacts,
            TickGroup::Shells,
            TickGroup::Subscribed,
        ] {
            for meta in [false, true] {
                let mut state = click_fixture(width, overlay, group);
                let row = state
                    .rendered_screen
                    .iter()
                    .position(|row| row.contains("Clickable row"))
                    .unwrap()
                    + usize::from(meta);
                let column = state.width.saturating_sub(TICK_LIST_COLUMNS) + 2;
                assert!(!state.tick_list.focused);
                state.pointer(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        row: row as u16,
                        column: column as u16,
                        modifiers: KeyModifiers::NONE,
                    },
                    &mut Vec::new(),
                );
                let clicked = tick_action_signature(state.pointer_input.take());
                state.tick_list.shell_detail = None;
                state.tick_list.focus(overlay, None);
                let entered = tick_action_signature(
                    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                );
                assert_eq!(clicked, entered);
            }
        }
    }
}

#[test]
fn pending_tick_removal_confirmation_cannot_be_approved_by_clicking_any_row() {
    for group in [
        TickGroup::Artifacts,
        TickGroup::Shells,
        TickGroup::Subscribed,
    ] {
        for metadata in [false, true] {
            let mut state = click_fixture(140, false, group);
            let previous = "artifact:https://docs.example/previous".to_owned();
            state.tick_list.confirmation = Some(previous.clone());
            state.tick_list.focus(false, None);
            state.draw_frame(&mut Vec::new()).unwrap();
            let target = &state.tick_row_targets[usize::from(metadata)];
            state.pointer(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    row: target.row as u16,
                    column: target.columns.start as u16,
                    modifiers: KeyModifiers::NONE,
                },
                &mut Vec::new(),
            );
            assert!(
                state.pointer_input.is_none(),
                "row click must never confirm removal"
            );
            assert_eq!(state.tick_list.confirmation, Some(previous.clone()));
            assert!(
                matches!(state.key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),Some(InputEvent::TickListAction {id,action:'X',..}) if id==previous)
            );
        }
    }
}

#[test]
fn tick_row_clicks_ignore_non_targets_and_other_input_surfaces() {
    let mut state = click_fixture(140, false, TickGroup::Artifacts);
    let title_row = state
        .rendered_screen
        .iter()
        .position(|row| row.contains("Clickable row"))
        .unwrap();
    let separator = state.width - TICK_LIST_COLUMNS;
    for (row, column) in [
        (0, separator + 2),
        (title_row - 1, separator + 2),
        (title_row + 2, separator + 2),
        (title_row, separator),
        (title_row, separator - 1),
    ] {
        state.pointer(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row: row as u16,
                column: column as u16,
                modifiers: KeyModifiers::NONE,
            },
            &mut Vec::new(),
        );
        assert!(state.pointer_input.is_none());
    }
    for surface in ["menu", "search", "picker"] {
        let mut state = click_fixture(140, false, TickGroup::Artifacts);
        match surface {
            "menu" => {
                state.menu = Some(Menu {
                    id: Uuid::new_v4(),
                    title: "Modal".into(),
                    context: Vec::new(),
                    options: vec!["Close".into()],
                    selected: 0,
                    confirmation: false,
                })
            }
            "search" => {
                let root = tempfile::tempdir().unwrap();
                state.input_history = Some(InputHistory::open(root.path()));
                state.open_history_search();
            }
            "picker" => {
                state.artifacts_picker =
                    Some(crate::terminal::artifacts_picker::ArtifactsPicker::new(
                        state.chat,
                        "owner@example.test".into(),
                        Default::default(),
                    ))
            }
            _ => unreachable!(),
        }
        state.pointer(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row: title_row as u16,
                column: (separator + 2) as u16,
                modifiers: KeyModifiers::NONE,
            },
            &mut Vec::new(),
        );
        assert!(state.pointer_input.is_none());
    }
}

#[tokio::test]
async fn permission_surface_blocks_stale_tick_row_clicks() {
    let directory = tempfile::tempdir().unwrap();
    let core = crate::runtime::Agent::new_embedded(
        crate::terminal::test_support::config(),
        Arc::new(crate::terminal::test_support::ScriptedModel::new(Vec::new())),
    );
    let chat = core.create_chat(directory.path().into()).await.unwrap();
    let view = core.permissions_view(chat).await.unwrap();
    let mut state = click_fixture(140, false, TickGroup::Artifacts);
    let target = &state.tick_row_targets[0];
    let (row, column) = (target.row, target.columns.start);
    state.permissions_surface = Some(PermissionsSurface::open(
        view,
        PermissionsReturn::ModePicker { settings: false },
    ));
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: row as u16,
            column: column as u16,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(state.pointer_input.is_none());
    assert!(!state.tick_list.focused);
    core.shutdown().await;
}

#[test]
fn tick_list_hint_omits_basic_navigation() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "Task")]);
    state.tick_list.focus(false, None);
    assert!(
        !state
            .tick_list
            .lines(140, 25, state.display, 0)
            .iter()
            .any(|line| line.text().contains("j/k move"))
    );
}

#[test]
fn failed_artifact_refresh_retry_key_only_routes_from_the_focused_list() {
    let mut state = click_fixture(140, false, TickGroup::Artifacts);
    state.tick_list.focus(false, None);
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        Some(InputEvent::TickListAction { action: 'r', .. })
    ));
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    assert_eq!(state.editor.draft(), "r");
}

#[test]
fn subscribed_artifact_routes_copy_and_feedback_to_artifact_actions() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let id = Uuid::new_v4();
    let mut entry = shell(id, "Subscribed document");
    entry.group = TickGroup::Subscribed;
    entry.glyph = Glyph::ArtifactTick;
    entry.kind = "Artifact".into();
    state.tick_list.replace(TickGroup::Subscribed, vec![entry]);
    state.tick_list.focus(false, None);
    for action in ['c', 'f'] {
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Char(action), KeyModifiers::NONE)), Some(InputEvent::TickListAction { action: received, .. }) if received == action)
        );
    }
}

#[tokio::test]
async fn failed_artifact_refresh_keeps_cached_title_but_marks_the_row_and_notifies() {
    let directory = tempfile::tempdir().unwrap();
    let core = crate::runtime::Agent::new_embedded(
        crate::terminal::test_support::config(),
        std::sync::Arc::new(crate::terminal::test_support::ScriptedModel::new(Vec::new())),
    );
    let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
        core.clone(),
        crate::terminal::test_support::config(),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let tick = crate::artifacts::ArtifactTick {
        artifact_id: "doc".into(),
        origin: "https://docs.example".into(),
        title: "Original".into(),
        format: "markdown".into(),
        url: "https://docs.example/a/doc".into(),
        access_role: "owner".into(),
        added_at_ms: 1,
    };
    agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
    let console = Console::test_for_chat(chat);
    console.update_artifact_metadata(chat, tick.instance_id(), Some(serde_json::from_value(serde_json::json!({"id":"doc","title":"Cached title","format":"markdown","access_role":"owner"})).unwrap())).unwrap();
    crate::terminal::artifact_ui::refresh(&agent, &console, Some(directory.path()), chat)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    console
        .update_artifact_ticks(&agent.snapshot(chat).await.unwrap())
        .unwrap();
    {
        let state = console.state.lock().unwrap();
        assert_eq!(state.tick_list.entries()[0].title, "Cached title");
        assert!(
            state.tick_list.entries()[0]
                .status
                .contains("refresh failed")
        );
        assert!(state.status.contains("Artifact refresh failed"));
    }
    core.shutdown().await;
}

#[test]
fn focused_tick_list_does_not_paste_into_composer_or_show_basic_navigation_hints() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "Task")]);
    state.tick_list.focus(false, None);
    state.paste("pasted into another surface");
    assert!(state.editor.draft().is_empty());
    let mut output = Vec::new();
    state.draw_frame(&mut output).unwrap();
    assert!(
        !state
            .rendered_screen
            .iter()
            .any(|row| row.contains("j/k move"))
    );
}

#[test]
fn history_search_keeps_focus_on_right_and_shift_arrows() {
    let root = tempfile::tempdir().unwrap();
    for key in [
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
    ] {
        let mut state = State::new(true, 140, 25);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state.input_history = Some(InputHistory::open(root.path()));
        state
            .tick_list
            .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "Task")]);
        state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(state.history_search.is_some());
        state.key(key);
        assert!(!state.tick_list.focused);
    }
}

#[test]
fn passive_todos_allow_tick_list_focus_on_right_and_shift_arrows() {
    for key in [
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
    ] {
        let mut state = State::new(true, 140, 25);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state
            .tick_list
            .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "Task")]);
        let console = tests::console_with_state(state);
        console
            .update_status_area(
                "Auto".into(),
                "fixture".into(),
                None,
                Vec::new(),
                vec![StatusItem {
                    id: "1".into(),
                    task_id: None,
                    can_stop: false,
                    title: "own passive todo".into(),
                    state: "pending",
                    role: Role::MutedText,
                }],
            )
            .unwrap();
        let mut state = console.state.lock().unwrap();
        state.draw(&mut io::sink()).unwrap();
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|row| row.contains("own passive todo"))
        );
        state.key(key);
        assert!(state.tick_list.focused);
    }
}

#[test]
fn tick_list_follows_agent_and_restores_selection_without_leaking_old_results() {
    let mut state = State::new(true, 140, 25);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    state.switch_view_chat(Some(a));
    state.tick_list.replace(
        TickGroup::Shells,
        vec![shell(first, "A1"), shell(second, "A2")],
    );
    state.tick_list.focus(false, None);
    state.tick_list.select(&second.to_string());
    state.switch_view_chat(Some(b));
    assert!(state.tick_list.entries().is_empty());
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "B")]);
    state.update_tick_entries(
        a,
        TickGroup::Shells,
        vec![shell(first, "A1"), shell(second, "A2")],
    );
    assert_eq!(state.tick_list.entries()[0].title, "B");
    state.switch_view_chat(Some(a));
    assert_eq!(state.tick_list.entries().len(), 2);
    assert_eq!(state.tick_list.selected().unwrap().title, "A2");
    assert!(state.tick_list.focused);
    state.switch_view_chat(None);
    assert!(!state.tick_list_visible());
}

#[test]
fn tick_list_width_boundary_and_overlay_do_not_change_visibility_preference() {
    let mut state = State::new(true, 121, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "A")]);
    assert!(state.tick_list_visible());
    state.width = 119;
    assert!(!state.tick_list_visible());
    state.prompt_enabled = true;
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(state.tick_list_visible());
    assert!(state.tick_list.overlay);
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.tick_list_visible());
    assert_eq!(
        state.tick_list_preference,
        tickloop_agent::settings::TickListPreference::Auto
    );
}

#[test]
fn shell_tick_renders_in_right_list_and_finished_remove_keeps_other_rows() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.tick_list.replace(
        TickGroup::Shells,
        vec![shell(Uuid::new_v4(), "echo shell tick")],
    );
    let mut output = Vec::new();
    state.draw_frame(&mut output).unwrap();
    assert!(
        state
            .rendered_screen
            .iter()
            .any(|line| line.contains("Shells (1)"))
    );
    assert!(
        state
            .rendered_screen
            .iter()
            .any(|line| line.contains("echo shell tick"))
    );
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(state.tick_list.entries().is_empty());
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.editor.draft(), "j");
}

#[test]
fn empty_tick_list_has_no_column_overlay_hint_or_mouse_target() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(!state.tick_list.overlay);
    assert!(!state.tick_list.focused);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.tick_toggle_target.is_none());
    assert!(
        !state
            .rendered_screen
            .iter()
            .any(|line| line.contains("Ticks") || line.contains("/artifacts"))
    );
}

#[test]
fn ticks_toggle_mouse_target_requests_saved_visibility_without_touching_the_editor() {
    let mut state = State::new(true, 100, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
    state.draw_frame(&mut Vec::new()).unwrap();
    let target = state.tick_toggle_target.as_ref().unwrap();
    let row = target.row as u16;
    let column = target.columns.start as u16;
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(matches!(
        state.pointer_input.take(),
        Some(InputEvent::ToggleTickList { show: true })
    ));
    assert!(!state.tick_list.overlay);
}

#[test]
fn tick_columns_preserve_full_width_input_dividers_cursor_and_mouse_geometry() {
    let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    for width in [140, 100, 25] {
        let mut state = State::new(true, width, 30);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state
            .editor
            .replace_draft("unchanged input across list toggle".into(), 20);
        state.status_area.update(
            "Auto".into(),
            "model".into(),
            Some((10, 100)),
            Vec::new(),
            Vec::new(),
        );
        state
            .tick_list
            .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
        state
            .scrollback
            .append(&format!("{}\n", "conversation ".repeat(25)));
        state.tick_list_preference = tickloop_agent::settings::TickListPreference::Hidden;
        let (hidden, hidden_row, hidden_column) = state.frame();
        let hidden_target = state
            .tick_toggle_target
            .as_ref()
            .map(|target| (target.row, target.columns.clone()));
        state.tick_list_preference = tickloop_agent::settings::TickListPreference::Auto;
        state.tick_list.overlay = width <= 120;
        let (shown, shown_row, shown_column) = state.frame();
        assert_eq!(shown, hidden, "input rows changed at {width} columns");
        assert_eq!((shown_row, shown_column), (hidden_row, hidden_column));
        assert_eq!(
            state
                .tick_toggle_target
                .as_ref()
                .map(|target| (target.row, target.columns.clone())),
            hidden_target
        );
        state.draw_frame(&mut Vec::new()).unwrap();
        assert!(
            state
                .rendered_rows
                .iter()
                .all(|row| display_width(&ansi.replace_all(row, "")) <= state.width)
        );
        let bottom = state.height - shown.len();
        assert_eq!(&state.rendered_screen[bottom..], shown.as_slice());
        assert!(
            state.rendered_screen[..bottom]
                .iter()
                .any(|line| line.contains('│'))
        );
        assert!(
            state
                .rendered_screen
                .iter()
                .all(|line| display_width(line) <= state.width)
        );
    }
}

#[test]
fn subscribed_ticks_show_live_status_and_ignore_another_chats_refresh() {
    let mut state = State::new(true, 140, 30);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    let row: crate::runtime::TickSubscriptionStatus = serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":"herdr:fixture","every_seconds":60,"accumulated":0,"last_delivery_ms":1000,"last_delivery":null,"link":"Live","kind":{"Terminal":{"truncated":false}},"title":"Quote terminal","updated_at_ms":null})).unwrap();
    let console = tests::console_with_state(state);
    console.update_subscribed_ticks(chat, &[row]).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.tick_list.entries()[0].title, "Quote terminal");
    assert_eq!(state.tick_list.entries()[0].kind, "Terminal");
    assert_eq!(state.tick_list.entries()[0].status, "live · 60s");
    let id = state.tick_list.entries()[0].id.clone();
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.update_tick_entries(chat, TickGroup::Subscribed, vec![]);
    assert!(state.tick_list.entries().is_empty());
    state.switch_view_chat(Some(chat));
    assert!(state.tick_list.entries().iter().all(|entry| entry.id != id));
}

#[test]
fn artifact_metadata_callback_is_filed_under_its_agent_after_a_view_switch() {
    let mut state = State::new(true, 140, 25);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let id = "artifact:https://docs.example/doc".to_owned();
    state.switch_view_chat(Some(a));
    let console = tests::console_with_state(state);
    let sink = console.artifact_metadata_sink();
    console.state.lock().unwrap().switch_view_chat(Some(b));
    let metadata = serde_json::from_value(serde_json::json!({"id":"doc","title":"A metadata","format":"markdown","access_role":"owner"})).unwrap();
    sink.update(a, id.clone(), Some(metadata));
    sink.failed(a, id.clone(), "A refresh failed".into());
    let mut state = console.state.lock().unwrap();
    assert!(!state.tick_list.metadata.contains_key(&id));
    assert!(state.tick_list.entries().is_empty());
    assert!(!state.status.contains("A refresh failed"));
    state.switch_view_chat(Some(a));
    assert_eq!(
        state.tick_list.metadata[&id].as_ref().unwrap().title,
        "A metadata"
    );
    assert_eq!(state.tick_list.metadata_failures[&id], "A refresh failed");
}

#[test]
fn failed_tick_list_save_keeps_the_current_visibility_and_focus() {
    let directory = tempfile::tempdir().unwrap();
    let settings = crate::settings::UserSettings::in_root(directory.path());
    std::fs::write(directory.path().join("settings.json"), "invalid JSON").unwrap();
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
    state.tick_list.focus(false, None);
    let console = tests::console_with_state(state);
    assert!(crate::terminal::set_tick_list_visible(&console, Some(&settings), false).is_err());
    let state = console.state.lock().unwrap();
    assert!(state.tick_list_visible());
    assert!(state.tick_list.focused);
    assert_eq!(
        state.tick_list_preference,
        crate::settings::TickListPreference::Auto
    );
}

#[test]
fn a_subscribed_artifact_is_visible_and_merges_with_its_added_tick() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.chat = Some(chat);
    let console = tests::console_with_state(state);
    let row: crate::runtime::TickSubscriptionStatus = serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":"artifact:https://docs.example/doc","every_seconds":60,"accumulated":0,"last_delivery_ms":null,"last_delivery":null,"link":"Live","kind":"Protocol","title":"Subscribed document","updated_at_ms":1000})).unwrap();
    console.update_subscribed_ticks(chat, &[row]).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.tick_list.entries().len(), 1);
    assert_eq!(state.tick_list.entries()[0].kind, "Artifact");
    assert_eq!(state.tick_list.entries()[0].updated_at_ms, Some(1000));
    let mut artifact = shell(Uuid::new_v4(), "Added document");
    artifact.id = "artifact:https://docs.example/doc".into();
    artifact.group = TickGroup::Artifacts;
    state
        .tick_list
        .replace(TickGroup::Artifacts, vec![artifact]);
    assert_eq!(state.tick_list.entries().len(), 1);
    assert_eq!(state.tick_list.entries()[0].group, TickGroup::Artifacts);
}

#[test]
fn background_rows_never_paint_past_the_left_column_or_hide_the_tick_separator() {
    let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    for width in [140, 100] {
        for fill in [Role::UserMessageBg, Role::ShellMessageBg, Role::Selection] {
            let mut state = State::new(true, width, 24);
            state.chat = Some(Uuid::new_v4());
            state.prompt_enabled = true;
            state.tick_list.overlay = width <= 120;
            state.tick_list.replace(
                TickGroup::Shells,
                vec![shell(Uuid::new_v4(), "right column tick")],
            );
            let left_width = state.width - TICK_LIST_COLUMNS;
            let text = format!("› 卡了吗\n{}中\n", "x".repeat(left_width - 1));
            state
                .scrollback
                .append_as(&text.repeat(state.height), Some(Role::Text), Some(fill));
            state.draw_frame(&mut Vec::new()).unwrap();
            let bottom = state.height - state.frame().0.len();
            for row in &state.rendered_rows[..bottom] {
                let plain = ansi.replace_all(row, "");
                let separator = plain
                    .find('│')
                    .expect("every content row retains the separator");
                assert_eq!(
                    display_width(&plain[..separator]),
                    left_width,
                    "{fill:?}: {plain:?}"
                );
                assert_eq!(
                    display_width(&plain),
                    state.width,
                    "left backgrounds and right list must occupy exactly one terminal row"
                );
                assert!(
                    row.contains("\x1b[0m│"),
                    "separator must start with reset styles"
                );
            }
            let row = state.rendered_rows[..bottom]
                .iter()
                .find(|row| ansi.replace_all(row, "").contains("right column tick"))
                .expect("right Tick content survives on a background-styled conversation row");
            assert!(
                row.contains("48;"),
                "the test must exercise a real background style"
            );
        }
    }
}

#[test]
fn tick_column_spans_content_top_through_working_and_generic_todo_rows() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    state.scrollback.append_as(
        "最新用户消息 卡了吗\n消息流\nTodos: 1 检查\n",
        Some(Role::Text),
        Some(Role::UserMessageBg),
    );
    state.scrollback.set_activity(Some(RoleLine::role(
        format!("✢ Working {}", "界".repeat(100)),
        Role::Accent,
    )));
    state.draw_frame(&mut Vec::new()).unwrap();
    let (bottom, _, _) = state.styled_frame();
    let content = state.height - bottom.len();
    let left = state.width - TICK_LIST_COLUMNS;
    for row in &state.rendered_screen[..content] {
        let (text, _) = row
            .split_once('│')
            .expect("Tick column must start at top content row and continue through Working/Todos");
        assert_eq!(display_width(text), left);
    }
    assert!(
        state.rendered_screen[..content].iter().any(|row| row
            .split('│')
            .next()
            .unwrap()
            .contains("Todos"))
    );
    assert!(
        state.rendered_screen[..content].iter().any(|row| row
            .split('│')
            .next()
            .unwrap()
            .contains("Working"))
    );
    assert!(
        state.rendered_screen[content..]
            .iter()
            .all(|row| !row.contains('│'))
    );
}
