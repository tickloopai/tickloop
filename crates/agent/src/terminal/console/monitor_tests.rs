use super::*;
use crate::terminal::monitor::{MonitorEntry, TickGroup};

fn shell(id: Uuid, title: &str) -> MonitorEntry {
    MonitorEntry {
        instance: crate::tick_stream::InstanceRef::shell(id),
        group: TickGroup::Shells,
        glyph: Glyph::ShellTick,
        title: title.into(),
        kind: "Shell".into(),
        status: "exit 0".into(),
        owner: crate::tick_adapter::TickOwner::Unknown,
        updated_at_ms: Some(1000),
        resources: None,
        actions: vec![
            crate::tick_adapter::MonitorAction {
                key: 'o',
                label: "open",
                operation: crate::tick_adapter::MonitorOperation::Execute(
                    crate::terminal::tick_actions::builtin::shell(id),
                ),
            },
            crate::tick_adapter::remove_from_monitor(&crate::tick_stream::InstanceRef::shell(id)),
        ],
    }
}

fn chat_snapshot(chat: Uuid) -> crate::runtime::ChatSnapshot {
    serde_json::from_value(serde_json::json!({
        "id": chat,
        "workdir": "/fixture",
        "messages": [],
        "loops": []
    }))
    .unwrap()
}

fn removed(instance: crate::tick_stream::InstanceRef) -> crate::runtime::MonitorMembership {
    crate::runtime::MonitorMembership {
        members: Vec::new(),
        removed: [instance].into(),
    }
}

#[test]
fn shell_tick_enter_opens_inside_the_tick_panel() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    state.monitor.focus(false, None);
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::TickAction { .. })
    ));
    assert!(state.monitor.detail.is_some());
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
            !console.monitor_has_entries().unwrap(),
            "running foreground Shell must be hidden"
        );
        snapshot.tasks[0].state = ShellTaskState::Completed;
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            !console.monitor_has_entries().unwrap(),
            "finished foreground Shell must be hidden"
        );
        snapshot.tasks[0].state = ShellTaskState::Background;
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            console.monitor_has_entries().unwrap(),
            "moving foreground to background must show it"
        );
        snapshot.tasks[0].was_background = true;
        snapshot.tasks[0].state = ShellTaskState::Completed;
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            console.monitor_has_entries().unwrap(),
            "finished background Shell must remain"
        );
        let task = crate::tick_stream::InstanceRef::shell(snapshot.tasks[0].id);
        {
            let mut state = console.state.lock().unwrap();
            state.monitor.focus(true, None);
            let removal = state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
            assert!(
                matches!(&removal, Some(InputEvent::TickAction { action, .. }) if action.key == 'x' && action.instance == task.to_string()),
                "x asks the Agent to record the removal"
            );
            assert_eq!(state.monitor.entries().len(), 1);
        }
        console.update_monitor_chat(&chat_snapshot(chat)).unwrap();
        console
            .update_monitor_membership(chat, removed(task.clone()))
            .unwrap();
        assert!(!console.monitor_has_entries().unwrap());
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            !console.monitor_has_entries().unwrap(),
            "removed background Shell must not reappear"
        );
        snapshot.tasks[0].id = Uuid::new_v4();
        snapshot.tasks[0].state = ShellTaskState::Running;
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(
            console.monitor_has_entries().unwrap(),
            "background start is shown even with running state"
        );
    }
}

#[test]
fn tick_shell_detail_and_selection_restore_per_agent_and_ignore_obsolete_mouse_targets() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    let a = state.chat.unwrap();
    let selected = state.monitor.selected().unwrap().instance.clone();
    state.monitor.focus(false, None);
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let panel = state.monitor.detail.as_ref().unwrap().id();
    state.draw_frame(&mut Vec::new()).unwrap();
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.monitor.selected().unwrap().instance, selected);
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_ne!(state.monitor.detail.as_ref().unwrap().id(), panel);
    let panel = state.monitor.detail.as_ref().unwrap().id();
    let output = (0..40)
        .map(|row| format!("cached-output-{row}\n"))
        .collect::<String>();
    let mut snapshot = tests::task_snapshot(a, 1);
    snapshot.tasks[0].id = Uuid::parse_str(selected.id()).unwrap();
    snapshot.tasks[0].output_bytes = output.len() as u64;
    snapshot.tasks[0].capabilities.can_read_output = true;
    let detail = state
        .monitor
        .detail
        .as_mut()
        .unwrap()
        .as_any_mut()
        .downcast_mut::<crate::terminal::tick_shell::TickShell>()
        .unwrap();
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
    assert!(state.monitor.detail.is_none());
    state.switch_view_chat(Some(a));
    assert_eq!(state.monitor.detail.as_ref().unwrap().id(), panel);
    assert_eq!(state.monitor.selected().unwrap().instance, selected);
    assert_eq!(
        state
            .monitor
            .detail
            .as_mut()
            .unwrap()
            .as_any_mut()
            .downcast_mut::<crate::terminal::tick_shell::TickShell>()
            .unwrap()
            .lines(40, 10, 2000)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>(),
        before
    );
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    state.draw_frame(&mut Vec::new()).unwrap();
    let target = &state.monitor_row_targets[0];
    let (row, column) = (target.row, target.columns.start);
    let b = Uuid::new_v4();
    state.switch_view_chat(Some(b));
    state.monitor.replace(
        TickGroup::Artifacts,
        vec![MonitorEntry {
            instance: "artifact:https://docs.example/b".parse().unwrap(),
            group: TickGroup::Artifacts,
            glyph: Glyph::ArtifactTick,
            title: "B".into(),
            kind: "Artifact".into(),
            status: String::new(),
            owner: crate::tick_adapter::TickOwner::Unknown,
            updated_at_ms: None,
            resources: None,
            actions: crate::tick_adapter::artifact_actions("artifact:https://docs.example/b"),
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
    assert!(!state.monitor.focused);
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
    let center = ShellTaskCenter::new(agent.clone());
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
    let action = {
        let mut state = console.state.lock().unwrap();
        state.monitor.focus(false, None);
        let Some(InputEvent::TickAction { action, .. }) =
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Shell did not open in panel");
        };
        action
    };
    let mut subscription = center.subscribe(chat).await.unwrap();
    super::super::tick_actions::execute(
        action,
        super::super::tick_actions::TickActionContext {
            agent: &agent,
            console: &console,
            root: Some(directory.path()),
            chat,
        },
    )
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
    tokio::time::sleep(Duration::from_millis(260)).await;
    let (owner, action) = console
        .poll_tick_detail()
        .unwrap()
        .expect("production detail poll");
    assert_eq!(owner, chat);
    super::super::tick_actions::execute(
        action,
        super::super::tick_actions::TickActionContext {
            agent: &agent,
            console: &console,
            root: Some(directory.path()),
            chat,
        },
    )
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
        apply_task_surface_effect,
        execution::ConsoleExecution,
        task_access::ShellTaskCenter,
        test_support,
        tick_actions::{TickActionContext, execute},
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
        let (action, effect) = if panel_mode {
            console.update_shell_ticks(&snapshot).unwrap();
            let action = {
                let mut state = console.state.lock().unwrap();
                state.monitor.focus(false, None);
                let Some(InputEvent::TickAction { action, .. }) =
                    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                else {
                    panic!("missing panel");
                };
                action
            };
            (Some(action), None)
        } else {
            (
                None,
                Some(
                    console
                        .show_tasks_at(snapshot.clone(), Some(receipt.command_id))
                        .unwrap(),
                ),
            )
        };
        let context = TickActionContext {
            agent: &agent,
            console: &console,
            root: Some(directory.path()),
            chat,
        };
        let unavailable = path.with_extension("held-by-test");
        std::fs::rename(&path, &unavailable).unwrap();
        if let Some(action) = action {
            execute(action, context).await.unwrap();
        } else {
            apply_task_surface_effect(&center, &console, chat, effect.clone().unwrap())
                .await
                .unwrap();
        }
        {
            let state = console.state.lock().unwrap();
            if panel_mode {
                let hint = state.monitor.detail.as_ref().unwrap().hint();
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
        if panel_mode {
            tokio::time::sleep(Duration::from_millis(260)).await;
            let (_, action) = console
                .poll_tick_detail()
                .unwrap()
                .expect("detail retry poll");
            execute(action, context).await.unwrap();
        } else {
            apply_task_surface_effect(&center, &console, chat, effect.unwrap())
                .await
                .unwrap();
        }
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
                assert_eq!(state.monitor.detail.as_ref().unwrap().hint(), "Esc back");
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
        entry.instance = "artifact:https://docs.example/doc".parse().unwrap();
        entry.glyph = Glyph::ArtifactTick;
        entry.actions = fixture_actions(&entry.instance);
    } else if group == TickGroup::Ticks {
        entry.glyph = Glyph::ProtocolTick;
        entry.kind = "Tick".into();
        entry.instance = "herdr:clickable".parse().unwrap();
        entry.actions = fixture_actions(&entry.instance);
    }
    state.monitor.replace(group, vec![entry]);
    state.monitor.overlay = overlay;
    state.draw_frame(&mut Vec::new()).unwrap();
    state
}

fn tick_action_signature(input: Option<InputEvent>) -> String {
    match input {
        Some(InputEvent::TickAction { chat, action }) => {
            format!("{chat}/{}/{}", action.instance, action.key)
        }
        _ => panic!("expected a Tick row action"),
    }
}

#[test]
fn monitor_row_title_and_metadata_clicks_perform_the_same_enter_action() {
    for (width, overlay) in [(140, false), (100, true)] {
        for group in [TickGroup::Artifacts, TickGroup::Shells, TickGroup::Ticks] {
            for meta in [false, true] {
                let mut state = click_fixture(width, overlay, group);
                let row = state
                    .rendered_screen
                    .iter()
                    .position(|row| row.contains("Clickable row"))
                    .unwrap()
                    + usize::from(meta);
                let column = state.width.saturating_sub(MONITOR_COLUMNS) + 2;
                assert!(!state.monitor.focused);
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
                state.monitor.detail = None;
                state.monitor.focus(overlay, None);
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
    for group in [TickGroup::Artifacts, TickGroup::Shells, TickGroup::Ticks] {
        for metadata in [false, true] {
            let mut state = click_fixture(140, false, group);
            let previous = "artifact:https://docs.example/previous".to_owned();
            state.monitor.confirmation = Some(crate::terminal::monitor::TickConfirmation {
                instance: previous.clone(),
                action: crate::terminal::tick_actions::builtin::artifact(&previous, 'X'),
                prompt: "Remove artifact?".into(),
            });
            state.monitor.focus(false, None);
            state.draw_frame(&mut Vec::new()).unwrap();
            let target = &state.monitor_row_targets[usize::from(metadata)];
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
            assert_eq!(
                state
                    .monitor
                    .confirmation
                    .as_ref()
                    .map(|confirmation| &confirmation.instance),
                Some(&previous)
            );
            assert!(
                matches!(state.key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),Some(InputEvent::TickAction {action,..}) if action.instance==previous && action.key=='X')
            );
        }
    }
}

#[test]
fn monitor_row_clicks_ignore_non_targets_and_other_input_surfaces() {
    let mut state = click_fixture(140, false, TickGroup::Artifacts);
    let title_row = state
        .rendered_screen
        .iter()
        .position(|row| row.contains("Clickable row"))
        .unwrap();
    let separator = state.width - MONITOR_COLUMNS;
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
    let target = &state.monitor_row_targets[0];
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
    assert!(!state.monitor.focused);
    core.shutdown().await;
}

#[test]
fn monitor_hint_omits_basic_navigation() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "Task")]);
    state.monitor.focus(false, None);
    assert!(
        !state
            .monitor
            .lines(140, 25, state.display, 0)
            .iter()
            .any(|line| line.text().contains("j/k move"))
    );
}

#[test]
fn failed_artifact_refresh_retry_key_only_routes_from_the_focused_list() {
    let mut state = click_fixture(140, false, TickGroup::Artifacts);
    state.monitor.focus(false, None);
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        Some(InputEvent::TickAction { action, .. }) if action.key == 'r'
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
    entry.group = TickGroup::Ticks;
    entry.glyph = Glyph::ArtifactTick;
    entry.kind = "Artifact".into();
    entry.instance = "artifact:https://docs.example/subscribed".parse().unwrap();
    entry.actions = fixture_actions(&entry.instance);
    state.monitor.replace(TickGroup::Ticks, vec![entry]);
    state.monitor.focus(false, None);
    for action in ['c', 'f'] {
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Char(action), KeyModifiers::NONE)), Some(InputEvent::TickAction { action: received, .. }) if received.key == action)
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
        .update_monitor_chat(&agent.snapshot(chat).await.unwrap())
        .unwrap();
    {
        let state = console.state.lock().unwrap();
        assert_eq!(state.monitor.entries()[0].title, "Cached title");
        assert!(state.monitor.entries()[0].status.contains("refresh failed"));
        assert!(state.status.contains("Artifact refresh failed"));
    }
    core.shutdown().await;
}

#[test]
fn focused_monitor_does_not_paste_into_composer_or_show_basic_navigation_hints() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "Task")]);
    state.monitor.focus(false, None);
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
            .monitor
            .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "Task")]);
        state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(state.history_search.is_some());
        state.key(key);
        assert!(!state.monitor.focused);
    }
}

#[test]
fn passive_todos_allow_monitor_focus_on_right_and_shift_arrows() {
    for key in [
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
    ] {
        let mut state = State::new(true, 140, 25);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state
            .monitor
            .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "Task")]);
        let console = tests::console_with_state(state);
        console
            .update_status_area(
                "Auto".into(),
                "fixture".into(),
                None,
                vec![TodoItem {
                    content: "own passive todo".into(),
                    status: tickloop_agent::runtime::TodoStatus::Pending,
                    active_form: "Working".into(),
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
        assert!(state.monitor.focused);
    }
}

#[test]
fn monitor_follows_agent_and_restores_selection_without_leaking_old_results() {
    let mut state = State::new(true, 140, 25);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    state.switch_view_chat(Some(a));
    state.monitor.replace(
        TickGroup::Shells,
        vec![shell(first, "A1"), shell(second, "A2")],
    );
    state.monitor.focus(false, None);
    state
        .monitor
        .select(&crate::tick_stream::InstanceRef::shell(second));
    state.switch_view_chat(Some(b));
    assert!(state.monitor.entries().is_empty());
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "B")]);
    state.update_tick_entries(
        a,
        TickGroup::Shells,
        vec![shell(first, "A1"), shell(second, "A2")],
    );
    assert_eq!(state.monitor.entries()[0].title, "B");
    state.switch_view_chat(Some(a));
    assert_eq!(state.monitor.entries().len(), 2);
    assert_eq!(state.monitor.selected().unwrap().title, "A2");
    assert!(state.monitor.focused);
    state.switch_view_chat(None);
    assert!(!state.monitor_visible());
}

#[test]
fn monitor_width_boundary_and_overlay_do_not_change_visibility_preference() {
    let mut state = State::new(true, 121, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "A")]);
    assert!(state.monitor_visible());
    state.width = 119;
    assert!(!state.monitor_visible());
    state.prompt_enabled = true;
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(state.monitor_visible());
    assert!(state.monitor.overlay);
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.monitor_visible());
    assert_eq!(
        state.monitor_preference,
        tickloop_agent::settings::MonitorPreference::Auto
    );
}

#[test]
fn shell_tick_renders_in_right_list_and_finished_remove_keeps_other_rows() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.monitor.replace(
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
    let removal = state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    let Some(InputEvent::TickAction { action, .. }) = removal else {
        panic!("x must ask the Agent to remove the row");
    };
    assert_eq!(state.monitor.entries().len(), 1);
    state.monitor.chat_snapshot = Some(chat_snapshot(state.chat.unwrap()));
    state.monitor.set_membership(removed(
        serde_json::from_value(serde_json::json!(action.instance)).unwrap(),
    ));
    assert!(state.monitor.entries().is_empty());
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.editor.draft(), "j");
}

#[test]
fn empty_monitor_has_no_column_overlay_hint_or_mouse_target() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(!state.monitor.overlay);
    assert!(!state.monitor.focused);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.monitor_toggle_target.is_none());
    assert!(
        !state
            .rendered_screen
            .iter()
            .any(|line| line.contains("Monitor") || line.contains("/artifacts"))
    );
}

#[test]
fn monitor_toggle_mouse_target_requests_saved_visibility_without_touching_the_editor() {
    let mut state = State::new(true, 100, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
    state.draw_frame(&mut Vec::new()).unwrap();
    let target = state.monitor_toggle_target.as_ref().unwrap();
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
        Some(InputEvent::ToggleMonitor { show: true })
    ));
    assert!(!state.monitor.overlay);
}

#[test]
fn monitor_columns_preserve_full_width_input_dividers_cursor_and_mouse_geometry() {
    let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    for width in [140, 100, 25] {
        let mut state = State::new(true, width, 30);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state
            .editor
            .replace_draft("unchanged input across list toggle".into(), 20);
        state
            .status_area
            .update("Auto".into(), "model".into(), Some((10, 100)), Vec::new());
        state
            .monitor
            .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
        state
            .scrollback
            .append(&format!("{}\n", "conversation ".repeat(25)));
        state.monitor_preference = tickloop_agent::settings::MonitorPreference::Hidden;
        let (hidden, hidden_row, hidden_column) = state.frame();
        let hidden_target = state
            .monitor_toggle_target
            .as_ref()
            .map(|target| (target.row, target.columns.clone()));
        state.monitor_preference = tickloop_agent::settings::MonitorPreference::Auto;
        state.monitor.overlay = width <= 120;
        let (shown, shown_row, shown_column) = state.frame();
        assert_eq!(shown, hidden, "input rows changed at {width} columns");
        assert_eq!((shown_row, shown_column), (hidden_row, hidden_column));
        assert_eq!(
            state
                .monitor_toggle_target
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
    assert_eq!(state.monitor.entries()[0].title, "Quote terminal");
    assert_eq!(state.monitor.entries()[0].kind, "Terminal");
    assert_eq!(state.monitor.entries()[0].status, "live · 60s");
    let instance = state.monitor.entries()[0].instance.clone();
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.update_tick_entries(chat, TickGroup::Ticks, vec![]);
    assert!(state.monitor.entries().is_empty());
    state.switch_view_chat(Some(chat));
    assert!(
        state
            .monitor
            .entries()
            .iter()
            .all(|entry| entry.instance != instance)
    );
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
    assert!(!state.monitor.metadata.contains_key(&id));
    assert!(state.monitor.entries().is_empty());
    assert!(!state.status.contains("A refresh failed"));
    state.switch_view_chat(Some(a));
    assert_eq!(
        state.monitor.metadata[&id].as_ref().unwrap().title,
        "A metadata"
    );
    assert_eq!(state.monitor.metadata_failures[&id], "A refresh failed");
}

#[test]
fn failed_monitor_save_keeps_the_current_visibility_and_focus() {
    let directory = tempfile::tempdir().unwrap();
    let settings = crate::settings::UserSettings::in_root(directory.path());
    std::fs::write(directory.path().join("settings.json"), "invalid JSON").unwrap();
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
    state.monitor.focus(false, None);
    let console = tests::console_with_state(state);
    assert!(crate::terminal::set_monitor_visible(&console, Some(&settings), false).is_err());
    let state = console.state.lock().unwrap();
    assert!(state.monitor_visible());
    assert!(state.monitor.focused);
    assert_eq!(
        state.monitor_preference,
        crate::settings::MonitorPreference::Auto
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
    assert_eq!(state.monitor.entries().len(), 1);
    assert_eq!(state.monitor.entries()[0].kind, "Artifact");
    assert_eq!(state.monitor.entries()[0].updated_at_ms, Some(1000));
    let mut artifact = shell(Uuid::new_v4(), "Added document");
    artifact.instance = "artifact:https://docs.example/doc".parse().unwrap();
    artifact.group = TickGroup::Artifacts;
    state.monitor.replace(TickGroup::Artifacts, vec![artifact]);
    assert_eq!(state.monitor.entries().len(), 1);
    assert_eq!(state.monitor.entries()[0].group, TickGroup::Artifacts);
}

#[test]
fn background_rows_never_paint_past_the_left_column_or_hide_the_tick_separator() {
    let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    for width in [140, 100] {
        for fill in [Role::UserMessageBg, Role::ShellMessageBg, Role::Selection] {
            let mut state = State::new(true, width, 24);
            state.chat = Some(Uuid::new_v4());
            state.prompt_enabled = true;
            state.monitor.overlay = width <= 120;
            state.monitor.replace(
                TickGroup::Shells,
                vec![shell(Uuid::new_v4(), "right column tick")],
            );
            let left_width = state.width - MONITOR_COLUMNS;
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
    let left = state.width - MONITOR_COLUMNS;
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

/// The production row actions for a fixture instance: Artifact actions, or a subscription that
/// opens on Enter with the shared Monitor removal.
fn fixture_actions(
    instance: &crate::tick_stream::InstanceRef,
) -> Vec<crate::tick_adapter::MonitorAction> {
    if instance.tick_type() == "artifact" {
        return crate::tick_adapter::artifact_actions(instance.as_str());
    }
    vec![
        crate::tick_adapter::MonitorAction {
            key: 'o',
            label: "open",
            operation: crate::tick_adapter::MonitorOperation::Execute(
                crate::terminal::tick_actions::builtin::subscription(&Uuid::new_v4().to_string()),
            ),
        },
        crate::tick_adapter::remove_from_monitor(instance),
    ]
}

#[tokio::test]
async fn a_new_adapter_remote_action_and_detail_work_through_enter_and_click() {
    use crate::tick_adapter::*;
    use std::{
        any::Any,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    struct Calendar {
        endpoint: String,
    }
    struct OpenCalendar {
        endpoint: String,
    }
    struct ReadCalendar {
        endpoint: String,
        panel: Uuid,
    }
    struct CalendarDetail {
        id: Uuid,
        chat: Uuid,
        text: String,
    }
    impl TickDetailView for CalendarDetail {
        fn id(&self) -> Uuid {
            self.id
        }
        fn chat(&self) -> Uuid {
            self.chat
        }
        fn lines(&mut self, _width: usize, _height: usize, _now: u64) -> Vec<TickDetailLine> {
            vec![
                TickDetailLine::new("Calendar detail"),
                TickDetailLine::new(&self.text),
            ]
        }
        fn hint(&self) -> String {
            "Esc back".into()
        }
        fn key(&mut self, key: KeyEvent, _width: usize, _height: usize) -> TickDetailKey {
            if matches!(key.code, KeyCode::Esc | KeyCode::Left) {
                TickDetailKey::Back
            } else {
                TickDetailKey::None
            }
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }
    impl TickActionExecutor for OpenCalendar {
        fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
            let panel = Uuid::new_v4();
            ui.open_detail(Box::new(CalendarDetail {
                id: panel,
                chat: ui.chat().unwrap(),
                text: "loading".into(),
            }));
            Ok(PreparedTickAction::new(
                "cal:remote",
                'o',
                Arc::new(ReadCalendar {
                    endpoint: self.endpoint.clone(),
                    panel,
                }),
            ))
        }
    }
    #[async_trait::async_trait]
    impl TickActionBackend for ReadCalendar {
        async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
            let text = reqwest::get(&self.endpoint)
                .await
                .map_err(|error| error.to_string())?
                .text()
                .await
                .map_err(|error| error.to_string())?;
            ctx.update_detail::<CalendarDetail, _>(self.panel, |view| {
                view.text = text;
                Some(())
            })?;
            Ok(TickActionOutcome::Done)
        }
    }
    impl TickAdapter for Calendar {
        fn tick_type(&self) -> &'static str {
            "cal"
        }
        fn list(&self, _: &MonitorContext) -> Vec<TickDescriptor> {
            vec![TickDescriptor {
                instance: crate::tick_stream::InstanceRef::new("cal", "remote").unwrap(),
                group: TickGroup {
                    order: 4,
                    name: "Calendars",
                },
                glyph: TickGlyph::Protocol,
                title: "Remote calendar".into(),
                kind: "Calendar".into(),
                status: "live".into(),
                owner: crate::tick_adapter::TickOwner::Unknown,
                updated_at_ms: None,
                resources: None,
                running: true,
                actions: vec![MonitorAction {
                    key: 'o',
                    label: "open",
                    operation: MonitorOperation::Execute(Arc::new(OpenCalendar {
                        endpoint: self.endpoint.clone(),
                    })),
                }],
            }]
        }
        fn stream(&self) -> Option<Arc<dyn crate::tick_stream::TickStreamPort>> {
            None
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/calendar", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let count = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /calendar "));
            observed.fetch_add(1, Ordering::SeqCst);
            let body = "remote earnings detail";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let core = crate::runtime::Agent::new_embedded(
        crate::terminal::test_support::config(),
        Arc::new(crate::terminal::test_support::ScriptedModel::new(vec![])),
    );
    let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
        core.clone(),
        crate::terminal::test_support::config(),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let rows = TickAdapters::new([Arc::new(Calendar { endpoint }) as Arc<dyn TickAdapter>])
        .unwrap()
        .list(&MonitorContext::default());
    state.monitor.replace(
        TickGroup {
            order: 4,
            name: "Calendars",
        },
        rows.into_iter().map(MonitorEntry::from).collect(),
    );
    let console = tests::console_with_state(state);
    for click in [false, true] {
        let action = {
            let mut state = console.state.lock().unwrap();
            state.monitor.focus(false, None);
            state.draw_frame(&mut Vec::new()).unwrap();
            let event = if click {
                let target = &state.monitor_row_targets[0];
                let (row, column) = (target.row, target.columns.start);
                state.pointer(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        row: row as u16,
                        column: column as u16,
                        modifiers: KeyModifiers::NONE,
                    },
                    &mut Vec::new(),
                );
                state.pointer_input.take()
            } else {
                state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            };
            let Some(InputEvent::TickAction { action, .. }) = event else {
                panic!("adapter action was not dispatched")
            };
            action
        };
        crate::terminal::tick_actions::execute(
            action,
            TickActionContext {
                agent: &agent,
                console: &console,
                root: None,
                chat,
            },
        )
        .await
        .unwrap();
        let mut state = console.state.lock().unwrap();
        state.draw_frame(&mut Vec::new()).unwrap();
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|line| line.contains("remote earnings detail"))
        );
        state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(state.monitor.detail.is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
    core.shutdown().await;
}

#[tokio::test]
async fn shell_detail_confirmation_runs_the_production_adapter_control_callback() {
    use crate::terminal::{
        execution::ConsoleExecution,
        task_access::ShellTaskCenter,
        test_support,
        tick_actions::{TickActionContext, execute},
    };
    let directory = tempfile::tempdir().unwrap();
    let core = crate::runtime::Agent::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(vec![])),
    );
    let agent = ConsoleExecution::from_embedded(core.clone(), test_support::config());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell_for_goal(chat, None, "printf ready; sleep 30".into(), true)
        .await
        .unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    let initial = center.snapshot(chat).await.unwrap();
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    console.update_shell_ticks(&initial).unwrap();
    let action = {
        let mut state = console.state.lock().unwrap();
        state.monitor.focus(false, None);
        let Some(InputEvent::TickAction { action, .. }) =
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("missing detail callback");
        };
        action
    };
    let context = TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    execute(action, context).await.unwrap();
    let action = {
        let mut state = console.state.lock().unwrap();
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
                .is_none()
        );
        assert!(
            state
                .monitor
                .detail
                .as_ref()
                .unwrap()
                .hint()
                .contains("Enter confirm")
        );
        let Some(InputEvent::TickAction { action, .. }) =
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("missing control callback");
        };
        action
    };
    assert!(
        !center
            .snapshot(chat)
            .await
            .unwrap()
            .tasks
            .iter()
            .find(|row| row.id == task.command_id)
            .unwrap()
            .state
            .is_terminal()
    );
    execute(action, context).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if center
                .snapshot(chat)
                .await
                .unwrap()
                .tasks
                .iter()
                .find(|row| row.id == task.command_id)
                .unwrap()
                .state
                .is_terminal()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (_, action) = console
        .poll_tick_detail()
        .unwrap()
        .expect("production detail poll");
    execute(action, context).await.unwrap();
    assert_eq!(
        console
            .state
            .lock()
            .unwrap()
            .monitor
            .detail
            .as_ref()
            .unwrap()
            .hint(),
        "Esc back"
    );
    core.shutdown().await;
}

struct FixtureDiscovery;
#[async_trait::async_trait]
impl crate::tick_adapter::TickDiscovery for FixtureDiscovery {
    fn tick_type(&self) -> &'static str {
        "herdr"
    }
    async fn page(
        &self,
        _: Option<tick_connect::page::PageCursor>,
    ) -> Result<
        tick_connect::page::Page<crate::tick_adapter::DiscoveredTick>,
        crate::tick_adapter::DiscoveryError,
    > {
        Err(crate::tick_adapter::DiscoveryError::Unavailable(
            "not listed in this fixture".into(),
        ))
    }
    async fn probe(
        &self,
        instances: &[crate::tick_stream::InstanceRef],
    ) -> Result<
        HashMap<crate::tick_stream::InstanceRef, crate::tick_adapter::MemberState>,
        crate::tick_adapter::DiscoveryError,
    > {
        Ok(instances
            .iter()
            .map(|instance| (instance.clone(), crate::tick_adapter::MemberState::Gone))
            .collect())
    }
}

fn screen(state: &mut State) -> String {
    state.draw_frame(&mut Vec::new()).unwrap();
    state.rendered_screen.join("\n")
}

#[test]
fn adding_from_the_workspace_opens_even_on_an_empty_monitor_and_only_where_discovery_exists() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    assert!(state.open_monitor_discovery().is_none());
    assert!(screen(&mut state).contains("Nothing to add"));
    assert!(!state.monitor_visible());
    assert!(!state.monitor.focused);

    state.tick_discovery = Some(Arc::new(FixtureDiscovery));
    assert!(matches!(
        state.open_monitor_discovery(),
        Some(InputEvent::TickAction { action, .. }) if action.key == 'a'
    ));
    assert!(
        state.monitor_visible(),
        "the picker shows on an empty Monitor"
    );
    assert!(screen(&mut state).contains("Add to Monitor"));
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.editor.draft(), "", "the picker owns its keys");
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(state.monitor.detail.is_none());
    assert!(!state.monitor_visible());
    assert!(
        !state.monitor.focused,
        "an empty Monitor gives focus back to the input"
    );
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.editor.draft(), "j", "text input owns j again");
}

#[test]
fn removed_rows_can_be_restored_without_a_workspace() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let mut snapshot = chat_snapshot(chat);
    let shell = crate::tick_stream::InstanceRef::shell(Uuid::new_v4());
    snapshot.monitor.removed.insert(shell.clone());
    state.monitor.chat_snapshot = Some(snapshot);
    state.monitor.rebuild();
    assert!(matches!(
        state.open_monitor_discovery(),
        Some(InputEvent::TickAction { .. })
    ));
    let picker = screen(&mut state);
    assert!(
        picker.contains("Add to Monitor · Removed · page 1"),
        "{picker}"
    );
    assert!(picker.contains("› shell:"), "{picker}");
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::TickAction { action, .. }) if action.instance == shell.to_string()
    ));
}

#[test]
fn monitor_pages_hold_at_most_128_rows_while_n_counts_the_full_set() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let mut snapshot = chat_snapshot(chat);
    for index in 0..130 {
        snapshot
            .monitor
            .members
            .push(crate::runtime::MonitorMember {
                instance: format!("herdr:member-{index:03}").parse().unwrap(),
                source: crate::runtime::MonitorMemberSource::Added,
                title: None,
            });
    }
    state.monitor.chat_snapshot = Some(snapshot);
    state.monitor.rebuild();
    assert_eq!(state.monitor.entries().len(), 128);
    assert_eq!(state.monitor.total(), 130);
    let rendered = screen(&mut state);
    assert!(rendered.contains("Monitor 130"), "{rendered}");
    assert!(rendered.contains("Monitor 1–128 of 130"), "{rendered}");
    state.monitor.focus(false, None);
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..140 {
        seen.insert(state.monitor.selected().unwrap().instance.clone());
        state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    }
    assert_eq!(seen.len(), 130, "j walks across pages to every row once");
    assert_eq!(state.monitor.entries().len(), 2);
    assert!(screen(&mut state).contains("Monitor 129–130 of 130"));
    state.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    state.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    assert_eq!(
        state.monitor.entries().len(),
        128,
        "k returns to the earlier page"
    );
}

#[test]
fn probed_herdr_members_report_their_host_state_and_owner() {
    let console = Console::test_for_chat(Uuid::new_v4());
    let chat = console.state.lock().unwrap().chat.unwrap();
    let mut snapshot = chat_snapshot(chat);
    for (id, source) in [
        ("added", crate::runtime::MonitorMemberSource::Added),
        ("opened", crate::runtime::MonitorMemberSource::Opened),
    ] {
        snapshot
            .monitor
            .members
            .push(crate::runtime::MonitorMember {
                instance: format!("herdr:{id}").parse().unwrap(),
                source,
                title: Some(id.into()),
            });
    }
    console.update_monitor_chat(&snapshot).unwrap();
    assert!(
        console.claim_member_probe().unwrap().is_none(),
        "no discovery, no probe"
    );
    console
        .set_tick_discovery(Arc::new(FixtureDiscovery))
        .unwrap();
    let probe = console.claim_member_probe().unwrap().unwrap();
    assert_eq!(probe.chat, chat);
    assert_eq!(probe.targets.len(), 2);
    assert!(
        console.claim_member_probe().unwrap().is_none(),
        "one probe at a time"
    );
    let states = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(probe.discovery.probe(&probe.targets))
        .ok();
    console.update_member_states(chat, states).unwrap();
    console.update_member_states(chat, None).unwrap();
    let state = console.state.lock().unwrap();
    let added = state
        .monitor
        .entries()
        .iter()
        .find(|entry| entry.title == "added")
        .unwrap();
    assert_eq!(added.status, "gone");
    assert_eq!(added.owner, crate::tick_adapter::TickOwner::Herdr);
    let opened = state
        .monitor
        .entries()
        .iter()
        .find(|entry| entry.title == "opened")
        .unwrap();
    assert_eq!(opened.owner, crate::tick_adapter::TickOwner::ThisChat);
}

#[test]
fn a_focused_monitor_advertises_and_routes_add_only_with_discovery() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    state.monitor.focus(false, None);
    assert!(!screen(&mut state).contains("a add"));
    state.tick_discovery = Some(Arc::new(FixtureDiscovery));
    assert!(screen(&mut state).contains("a add"));
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        Some(InputEvent::TickAction { action, .. }) if action.key == 'a'
    ));
    assert!(screen(&mut state).contains("Add to Monitor"));
}

#[test]
fn member_details_show_the_recorded_owner_and_no_invented_resources() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let mut snapshot = chat_snapshot(chat);
    snapshot
        .monitor
        .members
        .push(crate::runtime::MonitorMember {
            instance: crate::tick_stream::InstanceRef::new("herdr", "opened").unwrap(),
            source: crate::runtime::MonitorMemberSource::Opened,
            title: Some("Opened counter".into()),
        });
    state.monitor.chat_snapshot = Some(snapshot);
    state.monitor.rebuild();
    assert_eq!(state.monitor.entries().len(), 1);
    state.monitor.focus(false, None);
    assert!(screen(&mut state).contains("Enter details · x remove"));
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::TickAction { .. })
    ));
    let detail = screen(&mut state);
    for fact in [
        "Opened counter",
        "Instance  herdr:opened",
        "Owner     this Chat",
        "Resources unavailable",
    ] {
        assert!(detail.contains(fact), "{fact}: {detail}");
    }
    assert!(!detail.contains("PID"));
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(state.monitor.detail.is_none());
    assert!(state.monitor.focused, "Esc only closes the detail");
}
