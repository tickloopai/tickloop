use super::*;
use crate::terminal::monitor::{MonitorEntry, TickGroup};
use tickloop_agent_protocol::{
    ChatSnapshot, ChatStatus, MonitorMember, MonitorMembership, TickSubscriptionStatus,
};

fn shell(id: Uuid, title: &str) -> MonitorEntry {
    MonitorEntry {
        instance: tick_protocol::InstanceRef::shell(id),
        group: TickGroup::Shells,
        glyph: Glyph::ShellTick,
        title: title.into(),
        kind: "Shell".into(),
        status: "exit 0".into(),
        notes: Vec::new(),
        subscription: None,
        owner: crate::monitor_rows::TickOwner::Unknown,
        tone: crate::monitor_rows::StatusTone::Neutral,
        updated_at_ms: Some(1000),
        update: None,
        resources: None,
        actions: vec![
            crate::monitor_rows::MonitorAction {
                key: 'o',
                label: "open",
                intent: crate::monitor_rows::TickIntent::OpenView(
                    tick_protocol::InstanceRef::shell(id),
                ),
            },
            crate::monitor_rows::remove_from_monitor(&tick_protocol::InstanceRef::shell(id)),
        ],
    }
}

fn chat_snapshot(chat: Uuid) -> ChatSnapshot {
    serde_json::from_value(serde_json::json!({
        "id": chat,
        "workdir": "/fixture",
        "messages": [],
        "loops": []
    }))
    .unwrap()
}

fn chat_status(chat: Uuid) -> ChatStatus {
    ChatStatus::of(&chat_snapshot(chat))
}

fn removed(instance: tick_protocol::InstanceRef) -> MonitorMembership {
    MonitorMembership {
        removed: [instance].into(),
        ..Default::default()
    }
}

/// Monitor shows no Tick content: Enter on a Shell row asks for its view and opens no detail
/// inside Monitor.
#[test]
fn shell_tick_enter_opens_its_view_and_no_monitor_detail() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    let selected = state.monitor.selected().unwrap().instance.clone();
    state.monitor.focus(false, None);
    let chat = state.chat.unwrap();
    assert_eq!(
        tick_action_signature(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))),
        format!(
            "{chat}/{selected}/{}",
            crate::monitor_rows::MONITOR_ENTER_KEY
        )
    );
    assert!(state.monitor.detail.is_none());
    assert!(state.monitor.focused, "Monitor keeps the focus");
}

#[test]
fn shell_ticks_only_list_background_executions_and_x_removes_a_finished_one() {
    use tickloop_agent_protocol::{ShellTaskOrigin, ShellTaskOwner, ShellTaskState};
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
        console
            .refresh_monitor_ticks(snapshot.clone(), None)
            .unwrap();
        assert!(
            !console.monitor_has_entries().unwrap(),
            "running foreground Shell must be hidden"
        );
        snapshot.tasks[0].state = ShellTaskState::Completed;
        console
            .refresh_monitor_ticks(snapshot.clone(), None)
            .unwrap();
        assert!(
            !console.monitor_has_entries().unwrap(),
            "finished foreground Shell must be hidden"
        );
        snapshot.tasks[0].state = ShellTaskState::Background;
        console
            .refresh_monitor_ticks(snapshot.clone(), None)
            .unwrap();
        assert!(
            console.monitor_has_entries().unwrap(),
            "moving foreground to background must show it"
        );
        snapshot.tasks[0].was_background = true;
        snapshot.tasks[0].state = ShellTaskState::Completed;
        snapshot.tasks[0].updated_at_ms =
            Some(tickloop_agent_base::private_fs::unix_time_milliseconds());
        console
            .refresh_monitor_ticks(snapshot.clone(), None)
            .unwrap();
        assert!(
            console.monitor_has_entries().unwrap(),
            "a just finished background Shell still shows its result"
        );
        let task = tick_protocol::InstanceRef::shell(snapshot.tasks[0].id);
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
        console.update_monitor_chat(chat_status(chat)).unwrap();
        console
            .update_monitor_membership(chat, removed(task.clone()))
            .unwrap();
        assert!(!console.monitor_has_entries().unwrap());
        console
            .refresh_monitor_ticks(snapshot.clone(), None)
            .unwrap();
        assert!(
            !console.monitor_has_entries().unwrap(),
            "removed background Shell must not reappear"
        );
        snapshot.tasks[0].id = Uuid::new_v4();
        snapshot.tasks[0].state = ShellTaskState::Running;
        console
            .refresh_monitor_ticks(snapshot.clone(), None)
            .unwrap();
        assert!(
            console.monitor_has_entries().unwrap(),
            "background start is shown even with running state"
        );
    }
}

#[test]
fn monitor_selection_restores_per_agent_and_ignores_obsolete_mouse_targets() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    let a = state.chat.unwrap();
    let selected = state.monitor.selected().unwrap().instance.clone();
    state.monitor.focus(false, None);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.switch_view_chat(Some(a));
    assert_eq!(state.monitor.selected().unwrap().instance, selected);
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
            notes: Vec::new(),
            subscription: None,
            owner: crate::monitor_rows::TickOwner::Unknown,
            tone: crate::monitor_rows::StatusTone::Neutral,
            updated_at_ms: None,
            update: None,
            resources: None,
            actions: crate::monitor_rows::artifact_actions("artifact:https://docs.example/b", true),
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
        entry.instance = "calendar:clickable".parse().unwrap();
        entry.actions = fixture_actions(&entry.instance);
        let mut subscription = tests::subscription_row(1);
        subscription.instance = entry.instance.clone();
        state.monitor.subscriptions = vec![subscription];
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
    for (width, overlay) in [(140, false), (80, false), (60, true)] {
        for group in [TickGroup::Artifacts, TickGroup::Shells, TickGroup::Ticks] {
            for meta in [false, true] {
                let mut state = click_fixture(width, overlay, group);
                // A two-line row's status line is its last target; a one-line row has one.
                let rows = state
                    .monitor_row_targets
                    .iter()
                    .map(|target| target.row)
                    .collect::<Vec<_>>();
                assert_eq!(
                    rows[0],
                    state
                        .rendered_screen
                        .iter()
                        .position(|row| row.contains("Clickable row"))
                        .unwrap()
                );
                let row = if meta { *rows.last().unwrap() } else { rows[0] };
                let column = state.width - state.monitor_columns() + 2;
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
                option: None,
            });
            state.monitor.focus(false, None);
            state.draw_frame(&mut Vec::new()).unwrap();
            let target = &state.monitor_row_targets
                [usize::from(metadata).min(state.monitor_row_targets.len() - 1)];
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
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
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
fn an_artifact_row_routes_copy_and_feedback_to_artifact_actions() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let id = Uuid::new_v4();
    let mut entry = shell(id, "Brief");
    entry.group = TickGroup::Artifacts;
    entry.glyph = Glyph::ArtifactTick;
    entry.kind = "Artifact".into();
    entry.instance = "artifact:https://docs.example/doc".parse().unwrap();
    entry.actions = fixture_actions(&entry.instance);
    state.monitor.replace(TickGroup::Artifacts, vec![entry]);
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
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        std::sync::Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
        core.clone(),
        tickloop_agent_test_support::config(),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let tick = tickloop_agent_protocol::ArtifactTick {
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
        .update_monitor_chat(agent.status(chat).await.unwrap())
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
fn an_artifact_renamed_in_the_chat_replaces_its_cached_metadata_title() {
    let chat = Uuid::new_v4();
    let console = Console::test_for_chat(chat);
    let tick = |title: &str| tickloop_agent_protocol::ArtifactTick {
        artifact_id: "doc".into(),
        origin: "https://docs.example".into(),
        title: title.into(),
        format: "markdown".into(),
        url: "https://docs.example/a/doc".into(),
        access_role: "owner".into(),
        added_at_ms: 1,
    };
    let cached = |title: &str| {
        serde_json::from_value(
            serde_json::json!({"id":"doc","title":title,"format":"markdown","access_role":"owner"}),
        )
        .unwrap()
    };
    let id = tick("Plan").instance_id();
    let mut snapshot = chat_status(chat);
    snapshot.artifact_ticks = vec![tick("Plan")];
    console.update_monitor_chat(snapshot.clone()).unwrap();
    console
        .update_artifact_metadata(chat, id.clone(), Some(cached("Renamed on the web")))
        .unwrap();
    console.update_monitor_chat(snapshot.clone()).unwrap();
    assert_eq!(
        console.state.lock().unwrap().monitor.entries()[0].title,
        "Renamed on the web",
        "an unchanged Chat title leaves the newer metadata title in place"
    );

    snapshot.artifact_ticks = vec![tick("Q3 plan")];
    console.update_monitor_chat(snapshot.clone()).unwrap();
    let state = console.state.lock().unwrap();
    assert_eq!(state.monitor.entries()[0].title, "Q3 plan");
    assert_eq!(
        state.monitor.metadata[&id].as_ref().unwrap().title,
        "Q3 plan"
    );
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
                    status: tickloop_agent_protocol::TodoStatus::Pending,
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
        .select(&tick_protocol::InstanceRef::shell(second));
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
fn monitor_sits_beside_reflowed_conversation_until_the_conversation_minimum() {
    let reply = (0..40)
        .map(|index| format!("reply{index:02} "))
        .collect::<String>();
    // (terminal columns, Monitor columns beside the conversation; None covers it)
    for (terminal_columns, beside) in [
        (140, Some(42)),
        (120, Some(42)),
        (100, Some(42)),
        (90, Some(42)),
        (85, Some(37)),
        (80, Some(32)),
        (79, None),
    ] {
        // A non-interactive State keeps `open_monitor` from painting the test's stdout.
        let mut state = State::new(false, terminal_columns, 30);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state
            .monitor
            .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "A")]);
        state.scrollback.append(&format!("{reply}\n"));
        let console = tests::console_with_state(state);
        console.open_monitor().unwrap();
        let mut state = console.state.lock().unwrap();
        state.draw_frame(&mut Vec::new()).unwrap();
        let content_rows = state.height - state.frame().0.len();
        let rows = &state.rendered_screen[..content_rows];
        assert!(state.monitor_visible(), "{terminal_columns}");
        assert_eq!(
            state.monitor.overlay,
            beside.is_none(),
            "{terminal_columns}"
        );
        let monitor_columns = beside.unwrap_or(MONITOR_COLUMNS);
        let left = state.width - monitor_columns;
        assert!(
            rows.iter()
                .all(|row| display_width(row.split('│').next().unwrap()) == left),
            "{terminal_columns}: {rows:#?}"
        );
        let shown = rows
            .iter()
            .map(|row| row.split('│').next().unwrap())
            .collect::<String>();
        let every_word_shown = (0..40).all(|index| shown.contains(&format!("reply{index:02}")));
        assert_eq!(
            every_word_shown,
            beside.is_some(),
            "a side column reflows the conversation; an overlay covers it: {rows:#?}"
        );
        if beside.is_some() {
            assert!(left >= crate::terminal::monitor::CONVERSATION_MIN_COLUMNS);
        }
    }
}

#[test]
fn below_the_minimum_width_monitor_overlays_a_conversation_that_keeps_the_full_width() {
    let reply = (0..40)
        .map(|index| format!("reply{index:02} "))
        .collect::<String>();
    let every_word_shown = |rows: &[String]| {
        let shown = rows.concat();
        (0..40).all(|index| shown.contains(&format!("reply{index:02}")))
    };
    let mut state = State::new(false, 60, 30);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "A")]);
    state.scrollback.append(&format!("{reply}\n"));
    let (input, _, _) = state.frame();
    let console = tests::console_with_state(state);
    console.open_monitor().unwrap();
    let mut state = console.state.lock().unwrap();
    assert!(state.monitor_visible());
    assert!(state.monitor.overlay);
    assert_eq!(state.content_columns(), state.width);
    state.draw_frame(&mut Vec::new()).unwrap();
    let content_rows = state.height - input.len();
    assert_eq!(&state.rendered_screen[content_rows..], input.as_slice());
    assert!(state.rendered_screen[..content_rows].iter().all(|row| {
        display_width(row.split('│').next().unwrap()) == state.width - MONITOR_COLUMNS
    }));
    assert!(
        state.rendered_screen[..content_rows]
            .iter()
            .any(|row| row.contains("Shells (1)"))
    );
    state.prompt_enabled = true;
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(state.monitor.focused);
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.monitor_visible());
    state.draw_frame(&mut Vec::new()).unwrap();
    let rows = state.rendered_screen[..content_rows].to_vec();
    assert!(rows.iter().all(|row| !row.contains('│')));
    assert!(
        every_word_shown(&rows),
        "closing the overlay leaves the conversation wrapped to the full width: {rows:#?}"
    );
}

#[test]
fn monitor_width_boundary_and_overlay_do_not_change_visibility_preference() {
    let mut state = State::new(true, 80, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "A")]);
    assert!(state.monitor_visible());
    state.width = 78;
    assert!(!state.monitor_visible());
    state.prompt_enabled = true;
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(state.monitor_visible());
    assert!(state.monitor.overlay);
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.monitor_visible());
    assert_eq!(
        state.monitor_preference,
        tickloop_agent_config::MonitorPreference::Auto
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
    state.monitor.chat_status = Some(Arc::new(chat_status(state.chat.unwrap())));
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
    let mut state = State::new(true, 60, 25);
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
        state.monitor_preference = tickloop_agent_config::MonitorPreference::Hidden;
        let (hidden, hidden_row, hidden_column) = state.frame();
        let hidden_target = state
            .monitor_toggle_target
            .as_ref()
            .map(|target| (target.row, target.columns.clone()));
        state.monitor_preference = tickloop_agent_config::MonitorPreference::Auto;
        state.monitor.overlay = state.monitor_side_columns().is_none();
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
    let row: TickSubscriptionStatus = serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":"calendar:fixture","frequency":"high","accumulated":0,"last_delivery_ms":1000,"last_delivery":null,"link":"Live","kind":{"Terminal":{"truncated":false}},"title":"Quote terminal","updated_at_ms":null})).unwrap();
    let console = tests::console_with_state(state);
    console.update_subscribed_ticks(chat, &[row]).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.monitor.entries()[0].title, "Quote terminal");
    assert_eq!(state.monitor.entries()[0].kind, "Terminal");
    assert_eq!(state.monitor.entries()[0].status, "live");
    assert!(state.monitor.entries()[0].notes.is_empty());
    assert_eq!(
        state.monitor.entries()[0].subscription,
        Some(crate::monitor_rows::SubscriptionMark {
            frequency: tickloop_agent_protocol::TickSubscriptionFrequency::High,
            next_check: None,
        })
    );
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
    let settings = tickloop_agent_config::UserSettings::in_root(directory.path());
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
        tickloop_agent_config::MonitorPreference::Auto
    );
}

/// An artifact enters Monitor only as one of the Chat's artifacts: a Tick Subscription, which an
/// artifact never takes, brings in no row of its own.
#[test]
fn an_artifact_tick_subscription_brings_no_row() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.chat = Some(chat);
    let console = tests::console_with_state(state);
    let row: TickSubscriptionStatus = serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":"artifact:https://docs.example/doc","frequency":"high","accumulated":0,"last_delivery_ms":null,"last_delivery":null,"link":"Live","kind":"Protocol","title":"Subscribed document","updated_at_ms":1000})).unwrap();
    console.update_subscribed_ticks(chat, &[row]).unwrap();
    assert!(console.state.lock().unwrap().monitor.entries().is_empty());
}

#[test]
fn background_rows_never_paint_past_the_left_column_or_hide_the_tick_separator() {
    let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    for width in [140, 80, 60] {
        for fill in [Role::UserMessageBg, Role::ShellMessageBg, Role::Selection] {
            let mut state = State::new(true, width, 24);
            state.chat = Some(Uuid::new_v4());
            state.prompt_enabled = true;
            state.monitor.overlay = state.monitor_side_columns().is_none();
            state.monitor.replace(
                TickGroup::Shells,
                vec![shell(Uuid::new_v4(), "right column tick")],
            );
            let left_width = state.width - state.monitor_columns();
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

/// The production row actions for a fixture instance: Artifact actions, or a subscribed row whose
/// Enter shows its subscriptions, with the shared Monitor removal.
fn fixture_actions(
    instance: &tick_protocol::InstanceRef,
) -> Vec<crate::monitor_rows::MonitorAction> {
    if instance.access() == "artifact" {
        return crate::monitor_rows::artifact_actions(instance.as_str(), false);
    }
    vec![
        crate::monitor_rows::MonitorAction {
            key: 'o',
            label: "subscriptions",
            intent: crate::monitor_rows::TickIntent::ManageSubscriptions {
                instance: instance.clone(),
                operation: crate::monitor_rows::SubscriptionOperation::View,
            },
        },
        crate::monitor_rows::remove_from_monitor(instance),
    ]
}

#[test]
fn a_newly_registered_row_intent_runs_through_enter_and_click() {
    use crate::monitor_rows::*;
    struct Calendar;
    impl MonitorRows for Calendar {
        fn access(&self) -> &'static str {
            "cal"
        }
        fn list(&self, _: &MonitorContext) -> Vec<TickDescriptor> {
            let instance = tick_protocol::InstanceRef::new("cal", "remote").unwrap();
            vec![TickDescriptor {
                instance: instance.clone(),
                group: TickGroup {
                    order: 4,
                    name: "Calendars",
                },
                glyph: TickGlyph::Protocol,
                title: "Remote calendar".into(),
                kind: "Calendar".into(),
                status: "live".into(),
                notes: Vec::new(),
                subscription: None,
                owner: TickOwner::Unknown,
                tone: crate::monitor_rows::StatusTone::Neutral,
                updated_at_ms: None,
                update: None,
                resources: None,
                running: true,
                actions: vec![MonitorAction {
                    key: MONITOR_ENTER_KEY,
                    label: "details",
                    intent: TickIntent::ShowDetails(instance),
                }],
            }]
        }
    }
    let chat = Uuid::new_v4();
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let rows = MonitorRowsRegistry::new([Arc::new(Calendar) as Arc<dyn MonitorRows>])
        .unwrap()
        .list(&MonitorContext::default());
    state.monitor.replace(
        TickGroup {
            order: 4,
            name: "Calendars",
        },
        rows.into_iter().map(MonitorEntry::from).collect(),
    );
    for click in [false, true] {
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
        assert_eq!(
            tick_action_signature(event),
            format!("{chat}/cal:remote/{MONITOR_ENTER_KEY}")
        );
        state.draw_frame(&mut Vec::new()).unwrap();
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|line| line.contains("Instance  cal:remote"))
        );
        state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(state.monitor.detail.is_none());
    }
}

/// A probe of the members of one access that finds every member gone.
struct FixtureGoneProbe(&'static str);
#[async_trait::async_trait]
impl crate::monitor_rows::TickProbe for FixtureGoneProbe {
    fn access(&self) -> &'static str {
        self.0
    }
    async fn probe(
        &self,
        _chat: Uuid,
        instances: &[tick_protocol::InstanceRef],
    ) -> Result<crate::monitor_rows::MemberFindings, crate::monitor_rows::ProbeError> {
        Ok(crate::monitor_rows::MemberFindings {
            states: instances
                .iter()
                .map(|instance| (instance.clone(), crate::monitor_rows::MemberState::Gone))
                .collect(),
            ..Default::default()
        })
    }
}

/// A hosted probe that finds every member live with a sample, or fails when told to.
struct FixtureHostedProbe {
    fails: bool,
}
#[async_trait::async_trait]
impl crate::monitor_rows::TickProbe for FixtureHostedProbe {
    fn access(&self) -> &'static str {
        tick_protocol::HOSTED_ACCESS
    }
    async fn probe(
        &self,
        _chat: Uuid,
        instances: &[tick_protocol::InstanceRef],
    ) -> Result<crate::monitor_rows::MemberFindings, crate::monitor_rows::ProbeError> {
        if self.fails {
            return Err(crate::monitor_rows::ProbeError("host unreachable".into()));
        }
        Ok(crate::monitor_rows::MemberFindings {
            updates: Default::default(),
            states: instances
                .iter()
                .map(|instance| (instance.clone(), crate::monitor_rows::MemberState::Live))
                .collect(),
            resources: instances
                .iter()
                .map(|instance| {
                    (
                        instance.clone(),
                        crate::monitor_rows::TickResources {
                            pid: 42,
                            cpu_time_ms: None,
                            cpu_percent: None,
                            rss_bytes: Some(1024),
                            sampled_at_ms: 1,
                        },
                    )
                })
                .collect(),
        })
    }
}

fn run_probes(
    probe: &MemberProbe,
) -> Vec<(&'static str, Option<crate::monitor_rows::MemberFindings>)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    probe
        .probes
        .iter()
        .map(|typed| {
            (
                typed.prober.access(),
                runtime
                    .block_on(typed.prober.probe(probe.chat, &typed.targets))
                    .ok(),
            )
        })
        .collect()
}

/// Members of each access are probed by that access's own probe; a probe that fails keeps its
/// own earlier findings and never wipes the other's.
#[test]
fn each_access_probes_its_own_members_beside_the_others() {
    let console = Console::test_for_chat(Uuid::new_v4());
    let chat = console.state.lock().unwrap().chat.unwrap();
    let mut snapshot = chat_status(chat);
    let hosted: tick_protocol::InstanceRef = "hosted:1a26e59c-a887-457d-8540-5079aea79ca0"
        .parse()
        .unwrap();
    for (instance, title) in [
        ("calendar:pane".parse().unwrap(), "pane"),
        (hosted.clone(), "hidden"),
    ] {
        snapshot.monitor.members.push(MonitorMember {
            instance,
            title: Some(title.into()),
        });
    }
    console.update_monitor_chat(snapshot.clone()).unwrap();
    console
        .add_member_probe(Arc::new(FixtureGoneProbe("calendar")))
        .unwrap();
    console
        .add_member_probe(Arc::new(FixtureHostedProbe { fails: false }))
        .unwrap();
    let probe = console.claim_member_probe().unwrap().unwrap();
    assert_eq!(probe.probes.len(), 2, "one probe per access with members");
    console
        .update_member_findings(chat, run_probes(&probe))
        .unwrap();
    let hidden_row = |console: &Console| {
        console
            .state
            .lock()
            .unwrap()
            .monitor
            .entries()
            .iter()
            .find(|entry| entry.title == "hidden")
            .cloned()
            .unwrap()
    };
    assert_eq!(hidden_row(&console).status, "live");
    assert_eq!(hidden_row(&console).resources.unwrap().pid, 42);
    console
        .update_member_findings(chat, vec![("calendar", None), ("hosted", None)])
        .unwrap();
    assert_eq!(
        hidden_row(&console).status,
        "live",
        "a failed probe keeps the earlier findings"
    );
}

fn screen(state: &mut State) -> String {
    state.draw_frame(&mut Vec::new()).unwrap();
    state.rendered_screen.join("\n")
}

#[test]
fn monitor_pages_hold_at_most_128_rows_while_n_counts_the_full_set() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let mut snapshot = chat_status(chat);
    for index in 0..130 {
        snapshot.monitor.members.push(MonitorMember {
            instance: format!("calendar:member-{index:03}").parse().unwrap(),
            title: None,
        });
    }
    state.monitor.chat_status = Some(Arc::new(snapshot));
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
fn probed_members_report_their_host_state_and_owner() {
    let console = Console::test_for_chat(Uuid::new_v4());
    let chat = console.state.lock().unwrap().chat.unwrap();
    let mut snapshot = chat_status(chat);
    for id in ["quotes", "news"] {
        snapshot.monitor.members.push(MonitorMember {
            instance: format!("hosted:{id}").parse().unwrap(),
            title: Some(id.into()),
        });
    }
    console.update_monitor_chat(snapshot.clone()).unwrap();
    assert!(
        console.claim_member_probe().unwrap().is_none(),
        "nothing probes members of a type without a probe"
    );
    console
        .add_member_probe(Arc::new(FixtureGoneProbe(tick_protocol::HOSTED_ACCESS)))
        .unwrap();
    let probe = console.claim_member_probe().unwrap().unwrap();
    assert_eq!(probe.chat, chat);
    assert_eq!(probe.probes.len(), 1);
    assert_eq!(probe.probes[0].targets.len(), 2);
    assert!(
        console.claim_member_probe().unwrap().is_none(),
        "one probe at a time"
    );
    let findings = run_probes(&probe);
    console.update_member_findings(chat, findings).unwrap();
    console
        .update_member_findings(chat, vec![(tick_protocol::HOSTED_ACCESS, None)])
        .unwrap();
    let state = console.state.lock().unwrap();
    for entry in state.monitor.entries() {
        assert_eq!(entry.status, "gone", "{}", entry.title);
        assert_eq!(entry.owner, crate::monitor_rows::TickOwner::ThisChat);
    }
}

#[test]
fn a_focused_monitor_has_no_add_key_and_types_a_into_the_input() {
    let mut state = click_fixture(140, false, TickGroup::Shells);
    state.monitor.focus(false, None);
    assert!(!screen(&mut state).contains("a add"));
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .is_none()
    );
    assert!(!state.monitor.focused);
    assert!(state.monitor.detail.is_none());
    assert_eq!(state.editor.draft(), "a");
}

#[test]
fn member_details_show_the_recorded_owner_and_no_invented_resources() {
    let mut state = State::new(true, 140, 25);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let mut snapshot = chat_status(chat);
    snapshot.monitor.members.push(MonitorMember {
        instance: tick_protocol::InstanceRef::new("calendar", "opened").unwrap(),
        title: Some("Opened counter".into()),
    });
    state.monitor.chat_status = Some(Arc::new(snapshot));
    state.monitor.rebuild();
    assert_eq!(state.monitor.entries().len(), 1);
    state.monitor.focus(false, None);
    assert!(screen(&mut state).contains("enter details · x remove"));
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::TickAction { .. })
    ));
    let detail = screen(&mut state);
    for fact in [
        "Opened counter",
        "Instance  calendar:opened",
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

fn monitor_state(width: u16, entries: Vec<MonitorEntry>) -> State {
    let mut state = State::new(true, width, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.monitor.replace(TickGroup::Shells, entries);
    state.draw_frame(&mut Vec::new()).unwrap();
    state
}

fn separator_column(state: &State) -> Option<usize> {
    state.rendered_screen[0]
        .chars()
        .position(|cell| cell == '│')
}

#[test]
fn monitor_rows_take_one_line_when_the_status_fits_beside_the_name_and_two_otherwise() {
    use crate::monitor_rows::{StatusTone, Unread};
    let mut finished = shell(Uuid::new_v4(), "Run tests");
    finished.status = "exit 101".into();
    finished.notes = vec!["101".into()];
    finished.tone = StatusTone::Failed;
    let mut subscribed = shell(Uuid::new_v4(), "Quote board for the whole watchlist");
    subscribed.group = TickGroup::Ticks;
    subscribed.instance = "calendar:quotes".parse().unwrap();
    subscribed.status = "live".into();
    subscribed.subscription = Some(crate::monitor_rows::SubscriptionMark {
        frequency: tickloop_agent_protocol::TickSubscriptionFrequency::High,
        next_check: None,
    });
    subscribed.tone = StatusTone::Live;
    subscribed.update = Some(crate::monitor_rows::RowUpdate {
        at_ms: 1000,
        unread: Some(Unread { count: Some(3) }),
        taken_in_at_ms: None,
    });
    for (width, preferred, entry, details, two_lines) in [
        (140, None, &finished, "101", false),
        (80, None, &finished, "101", false),
        (140, None, &subscribed, "⟳ ● 3", true),
        (200, Some(80), &subscribed, "⟳ ● 3", false),
    ] {
        let mut state = State::new(true, width, 25);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state.monitor_width = preferred;
        state.monitor.replace(entry.group, vec![entry.clone()]);
        state.draw_frame(&mut Vec::new()).unwrap();
        let screen = &state.rendered_screen;
        let name = screen
            .iter()
            .position(|row| row.contains(&entry.title))
            .unwrap();
        let status = screen.iter().position(|row| row.contains(details)).unwrap();
        assert_eq!(status - name, usize::from(two_lines), "{screen:#?}");
        assert!(
            !screen.iter().any(|row| row.contains("Shell ·")
                || row.contains("Tick ·")
                || row.contains(&entry.status)),
            "the group names the kind and the glyph's colour the state: {screen:#?}"
        );
    }
}

#[test]
fn a_monitor_row_glyph_carries_its_state_and_blinks_while_it_runs() {
    use crate::monitor_rows::StatusTone;
    let mut running = shell(Uuid::new_v4(), "Build");
    running.tone = StatusTone::Running {
        since_ms: Some(1_000),
    };
    let mut failed = shell(Uuid::new_v4(), "Lint");
    failed.tone = StatusTone::Failed;
    let mut monitor = Monitor::default();
    monitor.replace(TickGroup::Shells, vec![running, failed]);
    // Rows of equal standing order by their random ids, so only the Build-first sort is fixed.
    let glyph_role = |now_ms, display| {
        let mut roles = monitor
            .lines(40, 10, display, now_ms)
            .iter()
            .filter_map(|line| {
                let title = line.text();
                let role = line
                    .spans
                    .iter()
                    .find(|span| span.text == "$")
                    .and_then(|span| span.role)?;
                Some((title.contains("Build"), role))
            })
            .collect::<Vec<_>>();
        roles.sort_by_key(|(is_build, _)| !is_build);
        roles
    };
    let display = DisplayProfile::default();
    assert_eq!(
        glyph_role(2_000, display),
        [(true, Role::RunningMark), (false, Role::ErrorMark)]
    );
    assert_eq!(
        glyph_role(2_500, display),
        [(true, Role::RunningMarkDim), (false, Role::ErrorMark)]
    );
    assert_eq!(monitor.next_frame_ms(2_100, 40, display), Some(2_500));
    let still = DisplayProfile::new(tickloop_agent_config::DisplayPreferences {
        prefers_reduced_motion: true,
        ..Default::default()
    });
    assert_eq!(
        glyph_role(2_500, still),
        [(true, Role::RunningMark), (false, Role::ErrorMark)]
    );
    assert_eq!(
        monitor.next_frame_ms(2_100, 40, still),
        Some(3_000),
        "a steady glyph still ticks its elapsed time"
    );
}

#[test]
fn the_row_clock_redraws_while_a_visible_row_blinks_and_stops_when_none_runs() {
    use crate::monitor_rows::StatusTone;
    let mut running = shell(Uuid::new_v4(), "Build");
    running.tone = StatusTone::Running { since_ms: None };
    let mut state = monitor_state(140, vec![running]);
    let clock = state.row_clock_at.expect("a running row schedules a frame");
    assert!(!state.refresh_row_clocks(clock - 1));
    assert!(state.refresh_row_clocks(clock));
    state.monitor.replace(TickGroup::Shells, vec![]);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(state.row_clock_at, None, "nothing blinks");
}

#[test]
fn running_background_shells_are_named_by_key_command_and_count_from_their_start() {
    let chat = Uuid::new_v4();
    let console = Console::test_for_chat(chat);
    let now = tickloop_agent_base::private_fs::unix_time_milliseconds();
    let mut snapshot = tests::task_snapshot(chat, 1);
    snapshot.tasks[0].state = tickloop_agent_protocol::ShellTaskState::Background;
    snapshot.tasks[0].command = "cd /tmp && cargo test -p console 2>&1 | tail -20".into();
    snapshot.tasks[0].description = Some("Run the console tests".into());
    let task = snapshot.tasks[0].id;
    console
        .refresh_monitor(crate::terminal::monitor::MonitorSources {
            chat: Arc::new(chat_status(chat)),
            shells: snapshot,
            subscriptions: None,
        })
        .unwrap();
    let mut state = console.state.lock().unwrap();
    // The start its output file or command event records.
    state.monitor.shell_starts = HashMap::from([(task, now.saturating_sub(123_000))]);
    state.monitor.rebuild();
    let text = state
        .monitor
        .lines(40, 20, DisplayProfile::default(), now)
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("$ cargo test -p console"), "{text}");
    assert!(text.contains(" 2m 3s"), "{text}");
    assert!(
        !text.contains("Run the console tests") && !text.contains("tail"),
        "{text}"
    );
}

/// A detail inside Monitor about one row, such as its subscriptions.
struct RowDetail {
    chat: Uuid,
    instance: tick_protocol::InstanceRef,
}
impl crate::terminal::tick_actions::TickDetailView for RowDetail {
    fn id(&self) -> Uuid {
        Uuid::nil()
    }
    fn chat(&self) -> Uuid {
        self.chat
    }
    fn instance(&self) -> Option<tick_protocol::InstanceRef> {
        Some(self.instance.clone())
    }
    fn lines(
        &mut self,
        _width: usize,
        _height: usize,
        _now: u64,
    ) -> Vec<crate::terminal::tick_actions::TickDetailLine> {
        Vec::new()
    }
    fn hint(&self) -> String {
        "esc back".into()
    }
    fn key(
        &mut self,
        _key: KeyEvent,
        _width: usize,
        _height: usize,
    ) -> crate::terminal::tick_actions::TickDetailKey {
        crate::terminal::tick_actions::TickDetailKey::Back
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[test]
fn a_finished_background_shell_leaves_once_handled_unless_its_detail_is_open() {
    use tickloop_agent_protocol::FINISHED_SHELL_DISPLAY_MS;
    let chat = Uuid::new_v4();
    let console = Console::test_for_chat(chat);
    let ended = tickloop_agent_base::private_fs::unix_time_milliseconds();
    let mut snapshot = tests::task_snapshot(chat, 1);
    snapshot.tasks[0].was_background = true;
    snapshot.tasks[0].state = tickloop_agent_protocol::ShellTaskState::Completed;
    snapshot.tasks[0].exit_code = Some(0);
    snapshot.tasks[0].updated_at_ms = Some(ended);
    console
        .refresh_monitor_ticks(snapshot.clone(), None)
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state
            .monitor
            .refresh_due(ended + 10 * FINISHED_SHELL_DISPLAY_MS);
        assert_eq!(
            state.monitor.entries().len(),
            1,
            "an end no one handled keeps its Shell"
        );
        let text = state
            .monitor
            .lines(60, 10, DisplayProfile::default(), ended + 1_000)
            .iter()
            .map(RoleLine::text)
            .collect::<String>();
        assert!(
            text.trim_end().ends_with('●') && !text.contains("1s"),
            "an unread end shows its mark and no time: {text}"
        );
        assert!(
            !text.contains("ended") && !text.contains("exit"),
            "the glyph's colour tells how it ended: {text}"
        );
    }
    let handled = ended + 2_000;
    let mut chat_snapshot = chat_status(chat);
    chat_snapshot.monitor.mark_handled(
        tick_protocol::InstanceRef::shell(snapshot.tasks[0].id),
        handled,
    );
    console.update_monitor_chat(chat_snapshot.clone()).unwrap();
    let mut state = console.state.lock().unwrap();
    let leaves = handled + FINISHED_SHELL_DISPLAY_MS;
    assert!(!state.monitor.refresh_due(leaves - 1));
    assert_eq!(state.monitor.entries().len(), 1);
    state.monitor.detail = Some(Box::new(RowDetail {
        chat,
        instance: tick_protocol::InstanceRef::shell(snapshot.tasks[0].id),
    }));
    assert!(state.monitor.refresh_due(leaves));
    assert_eq!(
        state.monitor.entries().len(),
        1,
        "the open detail keeps its Shell"
    );
    state.monitor.close_detail();
    assert!(state.monitor.refresh_due(leaves));
    assert!(state.monitor.entries().is_empty());
}

#[test]
fn a_shell_end_flashes_as_a_turn_takes_it_in_and_a_chat_switch_does_not_replay_it() {
    let chat = Uuid::new_v4();
    let console = Console::test_for_chat(chat);
    let ended = tickloop_agent_base::private_fs::unix_time_milliseconds();
    let mut snapshot = tests::task_snapshot(chat, 1);
    snapshot.tasks[0].was_background = true;
    snapshot.tasks[0].state = tickloop_agent_protocol::ShellTaskState::Completed;
    snapshot.tasks[0].exit_code = Some(0);
    snapshot.tasks[0].updated_at_ms = Some(ended);
    snapshot.tasks[0].command = "make test".into();
    let shell = tick_protocol::InstanceRef::shell(snapshot.tasks[0].id);
    let mut chat_snapshot = chat_status(chat);
    console.update_monitor_chat(chat_snapshot.clone()).unwrap();
    console
        .refresh_monitor_ticks(snapshot.clone(), None)
        .unwrap();
    let display = DisplayProfile::default();
    let row_fill = |state: &State, now_ms| {
        let lines = state.monitor.frame(40, 10, display, now_ms).lines;
        let row = lines
            .iter()
            .find(|line| line.text().contains("make test"))
            .expect("the Shell keeps its row");
        row.fill
    };
    {
        let state = console.state.lock().unwrap();
        assert_eq!(
            row_fill(&state, ended + 500),
            None,
            "an unread row has no fill"
        );
        assert_eq!(state.monitor.next_frame_ms(ended + 500, 40, display), None);
    }

    let taken_in = ended + 1_000;
    chat_snapshot.monitor.mark_taken_in(shell, taken_in);
    console.update_monitor_chat(chat_snapshot.clone()).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(row_fill(&state, taken_in + 100), Some(Role::FlashStrong));
    assert_eq!(
        state.monitor.next_frame_ms(taken_in + 100, 40, display),
        Some(taken_in + 400)
    );
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.switch_view_chat(Some(chat));
    drop(state);
    console.update_monitor_chat(chat_snapshot.clone()).unwrap();
    let state = console.state.lock().unwrap();
    assert_eq!(row_fill(&state, taken_in + 100), None);
    assert_eq!(
        state.monitor.next_frame_ms(taken_in + 100, 40, display),
        None
    );
}

#[test]
fn an_emptied_monitor_keeps_its_column_and_its_label_still_hides_it() {
    let mut state = monitor_state(140, vec![shell(Uuid::new_v4(), "Short lived")]);
    let conversation = state.content_columns();
    assert!(conversation < state.width);
    state.monitor.replace(TickGroup::Shells, Vec::new());
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(state.content_columns(), conversation);
    let screen = state.rendered_screen.join("\n");
    assert!(screen.contains("Nothing to monitor"), "{screen}");
    assert!(screen.contains("Monitor 0"), "{screen}");
    let (row, column) = state
        .monitor_toggle_target
        .as_ref()
        .map(|target| (target.row, target.columns.start))
        .unwrap();
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: row as u16,
            column: column as u16,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(matches!(
        state.pointer_input.take(),
        Some(InputEvent::ToggleMonitor { show: false })
    ));
    state.monitor_preference = tickloop_agent_config::MonitorPreference::Hidden;
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(state.content_columns(), state.width);
    assert!(
        !state
            .rendered_screen
            .join("\n")
            .contains("Nothing to monitor")
    );
}

#[test]
fn dragging_the_separator_resizes_monitor_within_its_bounds_and_saves_on_release() {
    let mut state = monitor_state(140, vec![shell(Uuid::new_v4(), "Drag target")]);
    let mouse = |kind, column: usize| MouseEvent {
        kind,
        row: 2,
        column: column as u16,
        modifiers: KeyModifiers::NONE,
    };
    let start = separator_column(&state).unwrap();
    assert_eq!(start, state.width - MONITOR_COLUMNS);
    assert!(!state.pointer(
        mouse(MouseEventKind::Down(MouseButton::Left), start),
        &mut Vec::new()
    ));
    assert!(state.selection.is_none(), "the press does not select text");
    assert!(state.pointer(
        mouse(MouseEventKind::Drag(MouseButton::Left), 80),
        &mut Vec::new()
    ));
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(separator_column(&state), Some(80));
    assert!(
        !state.pointer(
            mouse(MouseEventKind::Drag(MouseButton::Left), 80),
            &mut Vec::new()
        ),
        "motion within the same column costs no frame"
    );
    for (column, monitor) in [(10, state.width - 47), (state.width, 32)] {
        state.pointer(
            mouse(MouseEventKind::Drag(MouseButton::Left), column),
            &mut Vec::new(),
        );
        assert_eq!(state.monitor_columns(), monitor);
    }
    state.pointer(
        mouse(MouseEventKind::Up(MouseButton::Left), state.width),
        &mut Vec::new(),
    );
    assert!(matches!(
        state.pointer_input.take(),
        Some(InputEvent::SaveMonitorWidth { columns: 32 })
    ));
    state.draw_frame(&mut Vec::new()).unwrap();
    state.pointer(
        mouse(MouseEventKind::Down(MouseButton::Left), 5),
        &mut Vec::new(),
    );
    assert!(
        state.selection.is_some(),
        "a press elsewhere still selects text"
    );
}

#[test]
fn a_finished_background_shell_a_loop_subscribes_to_stays_as_its_shell_row() {
    use tickloop_agent_protocol::FINISHED_SHELL_DISPLAY_MS;
    let chat = Uuid::new_v4();
    let console = Console::test_for_chat(chat);
    let ended = tickloop_agent_base::private_fs::unix_time_milliseconds();
    let mut snapshot = tests::task_snapshot(chat, 1);
    snapshot.tasks[0].was_background = true;
    snapshot.tasks[0].state = tickloop_agent_protocol::ShellTaskState::Completed;
    snapshot.tasks[0].exit_code = Some(0);
    snapshot.tasks[0].updated_at_ms = Some(ended);
    let shell = tick_protocol::InstanceRef::shell(snapshot.tasks[0].id);
    console
        .refresh_monitor_ticks(snapshot.clone(), None)
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let mut subscription = tests::subscription_row(3);
    subscription.instance = shell.clone();
    state.monitor.subscriptions = vec![subscription];
    state.monitor.rebuild();
    state
        .monitor
        .refresh_due(ended + FINISHED_SHELL_DISPLAY_MS + 1);
    let entries = state.monitor.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].instance, shell);
    assert_eq!(
        entries[0].group,
        TickGroup::Shells,
        "still the Shell row, not a generic Tick"
    );
}

#[test]
fn a_subscribed_shell_row_is_marked_immediate_and_offers_no_frequency() {
    let chat = Uuid::from_u128(77);
    let console = Console::test_for_chat(chat);
    let mut snapshot = tests::task_snapshot(chat, 1);
    snapshot.tasks[0].was_background = true;
    let shell = tick_protocol::InstanceRef::shell(snapshot.tasks[0].id);
    console
        .refresh_monitor_ticks(snapshot.clone(), None)
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let mut subscription = tests::subscription_row(3);
    subscription.instance = shell.clone();
    subscription.frequency = tickloop_agent_protocol::TickSubscriptionFrequency::Immediate;
    state.monitor.subscriptions = vec![subscription];
    state.monitor.rebuild();
    let entries = state.monitor.entries();
    let row = entries
        .iter()
        .find(|entry| entry.instance == shell)
        .unwrap();
    assert_eq!(
        row.subscription, None,
        "a Shell's immediate subscription shows no mark"
    );
    let keys = row
        .actions
        .iter()
        .map(|action| action.key)
        .collect::<Vec<_>>();
    assert!(
        keys.contains(&'w') && keys.contains(&'u') && !keys.contains(&'e'),
        "an immediate subscription has no frequency to change: {keys:?}"
    );
}

fn subscribed_fixture(subscriptions: usize) -> State {
    let mut state = State::new(true, 140, 30);
    state.switch_view_chat(Some(Uuid::from_u128(77)));
    state.prompt_enabled = true;
    state.monitor.subscriptions = (0..subscriptions)
        .map(|index| {
            let mut row = tests::subscription_row(3 + index as u64);
            row.instance = tick_protocol::InstanceRef::new("calendar", "fixture-run").unwrap();
            row.loop_id = Uuid::from_u128(900 + index as u128);
            row.accumulated = index as u64;
            row
        })
        .collect();
    state.monitor.rebuild();
    state.monitor.focus(false, None);
    state
}

fn monitor_text(state: &mut State) -> String {
    state.draw_frame(&mut Vec::new()).unwrap();
    state.rendered_screen.join("\n")
}

fn press(state: &mut State, code: KeyCode) -> Option<InputEvent> {
    state.key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn requested_key(event: Option<InputEvent>) -> char {
    match event {
        Some(InputEvent::TickAction { action, .. }) => action.key,
        other => panic!("expected a Tick action, got {other:?}"),
    }
}

#[test]
fn a_subscribed_row_shows_its_marker_and_manages_subscriptions_inside_monitor() {
    let mut state = subscribed_fixture(1);
    let text = monitor_text(&mut state);
    let row = text.lines().find(|line| line.contains("calendar")).unwrap();
    assert!(
        row.trim_end().ends_with('⟳') && !row.contains("high") && !text.contains("live"),
        "{text}"
    );
    assert!(
        text.contains("enter open") && text.contains("w subscriptions"),
        "{text}"
    );
    assert_eq!(
        state
            .monitor
            .selected()
            .unwrap()
            .actions
            .iter()
            .map(|action| (action.key, action.label))
            .collect::<Vec<_>>(),
        [
            ('o', "open"),
            ('x', "remove"),
            ('f', "subscribe"),
            ('w', "subscriptions"),
            ('e', "frequency"),
            ('u', "unsubscribe")
        ]
    );

    assert_eq!(requested_key(press(&mut state, KeyCode::Char('w'))), 'w');
    let text = monitor_text(&mut state);
    assert!(text.contains("Tick Subscriptions 1"), "{text}");
    assert!(text.contains("› ⟳ high · every 1 min"), "{text}");
    assert!(!state.input_dialog_visible(), "the detail stays in Monitor");

    press(&mut state, KeyCode::Char('e'));
    press(&mut state, KeyCode::Char('j'));
    let text = monitor_text(&mut state);
    assert!(
        text.contains("  high · every 1 min")
            && text.contains("› medium · every 10 min")
            && text.contains("  low · every 30 min")
            && !text.contains("immediate"),
        "{text}"
    );
    assert_eq!(requested_key(press(&mut state, KeyCode::Enter)), 'e');

    press(&mut state, KeyCode::Char('u'));
    let text = monitor_text(&mut state);
    assert!(text.contains("Cancel Tick Subscription"), "{text}");
    assert!(press(&mut state, KeyCode::Esc).is_none());
    assert!(!monitor_text(&mut state).contains("Cancel Tick Subscription"));
    press(&mut state, KeyCode::Char('u'));
    assert_eq!(requested_key(press(&mut state, KeyCode::Enter)), 'u');

    press(&mut state, KeyCode::Esc);
    assert!(state.monitor.detail.is_none());
    assert!(state.monitor.focused, "Esc only closes the detail");
}

#[test]
fn several_subscriptions_on_one_row_show_a_summary_and_open_as_a_list() {
    let mut state = subscribed_fixture(3);
    let text = monitor_text(&mut state);
    let row = text.lines().find(|line| line.contains("calendar")).unwrap();
    assert!(
        row.trim_end().ends_with('⟳') && !row.contains('3'),
        "several subscriptions show one mark without a count: {text}"
    );
    assert!(
        !text.contains('●'),
        "a row with no update time shows no unread mark: {text}"
    );

    assert_eq!(requested_key(press(&mut state, KeyCode::Char('u'))), 'u');
    let text = monitor_text(&mut state);
    assert!(text.contains("Tick Subscriptions 3"), "{text}");
    assert!(
        !text.contains("Cancel Tick Subscription"),
        "with several subscriptions the user picks one first: {text}"
    );
    press(&mut state, KeyCode::Char('j'));
    press(&mut state, KeyCode::Char('u'));
    assert!(monitor_text(&mut state).contains("Cancel Tick Subscription"));
}

#[tokio::test]
async fn removing_a_subscribed_row_confirms_then_cancels_its_subscriptions() {
    use crate::terminal::{
        execution::ConsoleExecution,
        tick_actions::{TickActionContext, TickActionUi, builtin, execute},
    };
    use tickloop_agent_runtime::{
        tick_adapter::{TickAdapter, TickAdapters},
        tick_stream::{InMemoryTickStream, TickStreamPort},
    };
    struct Calendar(InMemoryTickStream);
    impl TickAdapter for Calendar {
        fn access(&self) -> &'static str {
            "cal"
        }
        fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
            Some(Arc::new(self.0.clone()))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::call(
                "subscribe",
                "SubscribeTick",
                serde_json::json!({"tick_instance_id":"cal:watched"}),
            ),
            tickloop_agent_test_support::text("watching"),
        ])),
    );
    core.install_tick_stream(Arc::new(
        TickAdapters::new([Arc::new(Calendar(InMemoryTickStream::new())) as Arc<dyn TickAdapter>])
            .unwrap(),
    ))
    .await;
    let agent =
        ConsoleExecution::from_embedded(core.clone(), tickloop_agent_test_support::config());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    core.submit(chat, None, "watch the calendar".into())
        .await
        .unwrap();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            tickloop_agent_protocol::EventKind::Completed { .. } => break,
            tickloop_agent_protocol::EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let watched = tick_protocol::InstanceRef::new("cal", "watched").unwrap();
    let console = Console::test_for_chat(chat);
    console
        .update_monitor_chat(agent.status(chat).await.unwrap())
        .unwrap();
    console
        .update_subscribed_ticks(chat, &agent.tick_subscriptions(chat).await.unwrap())
        .unwrap();
    assert!(
        console.monitor_has_entries().unwrap(),
        "a subscription has a row even without Monitor rows of its access"
    );
    let context = TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    let removal = {
        let mut state = console.state.lock().unwrap();
        builtin::executor(&crate::monitor_rows::TickIntent::RemoveFromMonitor(
            watched.clone(),
        ))
        .unwrap()
        .prepare(&mut TickActionUi { state: &mut state })
        .unwrap()
    };
    execute(removal, context).await.unwrap();
    assert_eq!(
        agent.tick_subscriptions(chat).await.unwrap().len(),
        1,
        "nothing changes before the user confirms"
    );
    let confirmed = {
        let mut state = console.state.lock().unwrap();
        assert!(
            state
                .monitor
                .confirmation
                .as_ref()
                .is_some_and(|confirmation| confirmation
                    .prompt
                    .starts_with("Remove from Monitor and cancel its 1 Tick Subscription?"))
        );
        let Some(InputEvent::TickAction { action, .. }) =
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("confirming did not run the removal");
        };
        action
    };
    execute(confirmed, context).await.unwrap();
    assert!(agent.tick_subscriptions(chat).await.unwrap().is_empty());
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .monitor
            .is_removed(&watched)
    );
    assert!(!console.monitor_has_entries().unwrap());
    core.shutdown().await;
}

#[tokio::test]
async fn opening_a_row_handles_its_updates_and_other_keys_do_not() {
    use crate::terminal::{
        execution::ConsoleExecution,
        tick_actions::{TickActionContext, execute},
    };
    let directory = tempfile::tempdir().unwrap();
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let agent =
        ConsoleExecution::from_embedded(core.clone(), tickloop_agent_test_support::config());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let quotes = tick_protocol::InstanceRef::new("calendar", "quotes-run").unwrap();
    let joined = core
        .record_reopened_tick(
            chat,
            &tick_protocol::InstanceRef::new("calendar", "lost-run").unwrap(),
            quotes.clone(),
        )
        .await
        .unwrap()
        .handled_at(&quotes)
        .unwrap();
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    console
        .update_monitor_chat(agent.status(chat).await.unwrap())
        .unwrap();
    let context = TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    tokio::time::sleep(Duration::from_millis(5)).await;
    let opened = {
        let mut state = console.state.lock().unwrap();
        state.monitor.focus(false, None);
        let Some(InputEvent::TickAction { action, .. }) =
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Enter did not open the row");
        };
        action
    };
    execute(opened, context).await.unwrap();
    let handled = agent
        .snapshot(chat)
        .await
        .unwrap()
        .monitor
        .handled_at(&quotes)
        .unwrap();
    assert!(handled > joined, "opening the row handled its updates");
    assert_eq!(
        console
            .state
            .lock()
            .unwrap()
            .monitor
            .chat_status
            .as_ref()
            .unwrap()
            .monitor
            .handled_at(&quotes),
        Some(handled),
        "the View shows the handled row at once"
    );
    core.shutdown().await;
}

#[test]
fn a_gone_row_leaves_monitor_thirty_seconds_after_it_went() {
    use crate::monitor_rows::{GONE_DISPLAY_MS, MemberFindings, MemberState};
    let chat = Uuid::new_v4();
    let console = Console::test_for_chat(chat);
    let gone = tick_protocol::InstanceRef::new("hosted", "gone-run").unwrap();
    let mut snapshot = chat_status(chat);
    snapshot
        .monitor
        .record_opened(gone.clone(), Some("Quotes".into()));
    console.update_monitor_chat(snapshot.clone()).unwrap();
    let went = tickloop_agent_base::private_fs::unix_time_milliseconds();
    console
        .update_member_findings(
            chat,
            vec![(
                tick_protocol::HOSTED_ACCESS,
                Some(MemberFindings {
                    states: [(gone.clone(), MemberState::Gone)].into(),
                    ..Default::default()
                }),
            )],
        )
        .unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.monitor.entries().len(), 1, "a gone Tick shows first");
    let leaves = went + GONE_DISPLAY_MS + 1_000;
    assert!(state.monitor.refresh_due(leaves));
    assert!(state.monitor.entries().is_empty(), "then it leaves");
}

/// A running Shell is stopped from its Monitor row: `s` asks first, Esc keeps it running, and
/// once confirmed the Shell's own control stops it and its row shows it ended.
#[tokio::test]
async fn a_running_shell_is_stopped_from_its_row_once_confirmed() {
    use crate::terminal::{
        execution::ConsoleExecution,
        task_access::ShellTaskCenter,
        tick_actions::{TickActionContext, execute},
    };
    let directory = tempfile::tempdir().unwrap();
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let agent =
        ConsoleExecution::from_embedded(core.clone(), tickloop_agent_test_support::config());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let gate = directory.path().join("release");
    let receipt = agent
        .submit_user_shell_for_goal(
            chat,
            None,
            format!("while [ ! -e '{}' ]; do sleep 0.02; done", gate.display()),
            true,
        )
        .await
        .unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    let task_state = |snapshot: &tickloop_agent_protocol::ShellTaskSnapshot| {
        snapshot
            .tasks
            .iter()
            .find(|task| task.id == receipt.command_id)
            .map(|task| task.state)
    };
    let running = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = center.snapshot(chat).await.unwrap();
            if snapshot
                .tasks
                .iter()
                .any(|task| task.id == receipt.command_id && task.capabilities.can_stop)
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let shell = tick_protocol::InstanceRef::shell(receipt.command_id);
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    console
        .update_monitor_chat(agent.status(chat).await.unwrap())
        .unwrap();
    console
        .refresh_monitor_ticks(running.clone(), None)
        .unwrap();
    let context = TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    let press = |code| {
        console
            .state
            .lock()
            .unwrap()
            .key(KeyEvent::new(code, KeyModifiers::NONE))
    };
    console.state.lock().unwrap().monitor.focus(false, None);
    for confirms in [false, true] {
        let Some(InputEvent::TickAction { action, .. }) = press(KeyCode::Char('s')) else {
            panic!("s did not ask to stop the Shell");
        };
        assert_eq!(action.instance, shell.to_string());
        execute(action, context).await.unwrap();
        {
            let state = console.state.lock().unwrap();
            assert!(
                state.status.contains("Stop this Shell?"),
                "{}",
                state.status
            );
            assert!(state.monitor.detail.is_none(), "nothing opens in Monitor");
        }
        assert!(
            !task_state(&center.snapshot(chat).await.unwrap())
                .unwrap()
                .is_terminal(),
            "asking stops nothing"
        );
        if !confirms {
            assert!(press(KeyCode::Esc).is_none());
            assert!(console.state.lock().unwrap().monitor.confirmation.is_none());
            continue;
        }
        let Some(InputEvent::TickAction { action, .. }) = press(KeyCode::Enter) else {
            panic!("confirming did not stop the Shell");
        };
        execute(action, context).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while !task_state(&center.snapshot(chat).await.unwrap())
            .unwrap()
            .is_terminal()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let ended = center.snapshot(chat).await.unwrap();
    console.refresh_monitor_ticks(ended.clone(), None).unwrap();
    let actions = console
        .state
        .lock()
        .unwrap()
        .monitor
        .entries()
        .iter()
        .find(|entry| entry.instance == shell)
        .expect("the stopped Shell keeps its row")
        .actions
        .clone();
    assert!(
        actions.iter().all(|action| action.key != 's'),
        "an ended Shell has nothing to stop"
    );
    std::fs::write(&gate, "").unwrap();
    core.shutdown().await;
}

#[test]
fn a_character_the_focused_row_does_not_bind_is_typed_into_the_input() {
    let mut state = monitor_state(140, vec![shell(Uuid::new_v4(), "Build")]);
    state.monitor.focus(false, None);
    assert!(state.monitor.focused);
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))
            .is_some(),
        "a key the row binds keeps its row action"
    );
    state.monitor.detail = None;
    state.monitor.focus(false, None);
    for (code, modifiers) in [
        (KeyCode::Char('H'), KeyModifiers::SHIFT),
        (KeyCode::Char('i'), KeyModifiers::NONE),
    ] {
        state.key(KeyEvent::new(code, modifiers));
    }
    assert!(!state.monitor.focused, "typing leaves the list");
    assert_eq!(state.editor.draft(), "Hi");
}

/// A Chat whose one Loop holds the association with its artifact `doc`, live or paused.
fn linked_artifact_fixture(resume_required: bool) -> (State, tick_protocol::InstanceRef) {
    let mut state = State::new(true, 140, 30);
    let chat = Uuid::from_u128(78);
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
        "id": chat,
        "workdir": "/fixture",
        "messages": [],
        "artifact_ticks": [{"artifact_id":"doc","origin":"https://docs.example","title":"Brief","format":"markdown","url":"https://docs.example/a/doc","access_role":"owner","added_at_ms":1}],
        "loops": [{"id":Uuid::from_u128(900),"goal":"Write a brief","state":"WaitingTrigger","run_id":null,"run_started_at":null,"event_sources":[{"origin":"https://docs.example","account_id":"owner","artifact_id":"doc","binding_id":Uuid::from_u128(5),"client_instance_id":Uuid::from_u128(6),"epoch":1,"resume_required":resume_required,"last_request_at_ms":null}]}]
    }))
    .unwrap();
    state.monitor.chat_status = Some(Arc::new(ChatStatus::of(&snapshot)));
    state.monitor.rebuild();
    state.monitor.focus(false, None);
    (
        state,
        tick_protocol::InstanceRef::artifact("https://docs.example", "doc").unwrap(),
    )
}

fn action_labels(state: &State) -> Vec<(char, &'static str)> {
    state
        .monitor
        .selected()
        .unwrap()
        .actions
        .iter()
        .map(|action| (action.key, action.label))
        .collect()
}

#[test]
fn an_artifact_linked_here_shows_no_subscription_mark_and_x_unlinks_it() {
    let (mut state, artifact) = linked_artifact_fixture(false);
    let text = monitor_text(&mut state);
    let row = text.lines().find(|line| line.contains("Brief")).unwrap();
    assert!(
        row.trim_end().ends_with("Brief") && !row.contains("feedback"),
        "the association is the artifact's default subscription: {row}"
    );
    assert_eq!(
        action_labels(&state),
        [
            ('o', "open"),
            ('x', "unlink"),
            ('c', "copy"),
            ('f', "feedback")
        ]
    );
    assert_eq!(requested_key(press(&mut state, KeyCode::Char('x'))), 'x');

    let mut subscription = tests::subscription_row(3);
    subscription.instance = artifact;
    subscription.accumulated = 3;
    subscription.updated_at_ms = Some(1000);
    state.monitor.subscriptions = vec![subscription];
    state.monitor.rebuild();
    let text = monitor_text(&mut state);
    let row = text.lines().find(|line| line.contains("Brief")).unwrap();
    assert!(
        row.trim_end().ends_with("Brief") && !row.contains('●') && !row.contains('⟳'),
        "an artifact never shows a Tick Subscription: {row}"
    );
    assert_eq!(
        action_labels(&state),
        [
            ('o', "open"),
            ('x', "unlink"),
            ('c', "copy"),
            ('f', "feedback")
        ]
    );
}

#[test]
fn a_paused_association_stays_in_words() {
    let (mut state, artifact) = linked_artifact_fixture(true);
    let mut subscription = tests::subscription_row(3);
    subscription.instance = artifact;
    state.monitor.subscriptions = vec![subscription];
    state.monitor.rebuild();
    let text = monitor_text(&mut state);
    let row = text.lines().find(|line| line.contains("Brief")).unwrap();
    assert!(
        row.contains("feedback paused") && !row.contains('⟳'),
        "{row}"
    );
    assert_eq!(action_labels(&state)[1], ('x', "unlink"));
}

/// A Chat of `messages` messages, as a long conversation's snapshot brings.
fn chat_with_messages(chat: Uuid, messages: usize) -> ChatSnapshot {
    let mut snapshot = chat_snapshot(chat);
    snapshot.messages = serde_json::from_value(serde_json::Value::Array(
        (0..messages)
            .map(|index| {
                serde_json::json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": [{
                        "type": "text",
                        "text": format!("message {index}: {}", "conversation text ".repeat(4)),
                    }],
                })
            })
            .collect(),
    ))
    .unwrap();
    snapshot
}

/// The console side of one status refresh after an event: every setter the refresh calls with
/// what it read, half of them showing a change, as when a run moves on.
fn refresh_status(
    console: &Console,
    snapshot: &Arc<ChatStatus>,
    tasks: &tickloop_agent_protocol::ShellTaskSnapshot,
    changed: bool,
) {
    let chat = snapshot.id;
    console.set_activity_interruptible(false).unwrap();
    let _frame_hold = console.hold_frames().unwrap();
    console
        .set_input_context(
            Some(chat),
            &snapshot.workdir,
            &snapshot.additional_directories,
        )
        .unwrap();
    console.set_visible_delegation(false).unwrap();
    console.set_foreground_shell(changed).unwrap();
    console.set_queued_inputs(snapshot.queued_inputs()).unwrap();
    console.set_chat_images(chat, true, None).unwrap();
    let commands = if changed {
        vec![(tasks.tasks[0].id, None)]
    } else {
        Vec::new()
    };
    console.set_foreground_commands(&commands).unwrap();
    console
        .refresh_monitor(crate::terminal::monitor::MonitorSources {
            chat: snapshot.clone(),
            shells: tasks.clone(),
            subscriptions: Some(Vec::new()),
        })
        .unwrap();
    console
        .set_provider_failures(chat, &snapshot.provider_failures)
        .unwrap();
    console.set_provider_failure(None).unwrap();
    console
        .update_status_area(
            if changed { "plan" } else { "default" }.into(),
            "model".into(),
            None,
            Vec::new(),
        )
        .unwrap();
    console
        .set_activity_and_status(Some(&[("• Working".into(), Some(Role::MutedText))]), "")
        .unwrap();
}

/// Mean cost and frame count of the console side of one status refresh beside a conversation of
/// `size` rows whose Chat has as many messages. Frames go to stdout and results to stderr.
#[test]
#[ignore = "timing benchmark: cargo test --release --config profile.release.debug-assertions=true -p tickloop-agent-console --lib bench_ -- --ignored --nocapture >/dev/null"]
fn bench_status_refresh_cost_by_conversation_size() {
    const ROUNDS: u32 = 20;
    for size in [1_000, 10_000, 50_000] {
        let chat = Uuid::new_v4();
        let console = Console::test_interactive_for_chat(chat);
        console.state.lock().unwrap().scrollback.append(
            &(0..size)
                .map(|index| format!("row {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let snapshot = Arc::new(ChatStatus::of(&chat_with_messages(chat, size)));
        let tasks = tests::task_snapshot(chat, 1);
        refresh_status(&console, &snapshot, &tasks, false);
        let frames = console.state.lock().unwrap().frames_drawn;
        let started = std::time::Instant::now();
        for round in 0..ROUNDS {
            refresh_status(&console, &snapshot, &tasks, round % 2 == 0);
        }
        let elapsed = started.elapsed().as_secs_f64() * 1e6 / f64::from(ROUNDS);
        let frames = console.state.lock().unwrap().frames_drawn - frames;
        eprintln!(
            "BENCH status_refresh size={size} per_refresh_us={elapsed:.1} frames_per_refresh={:.1}",
            frames as f64 / f64::from(ROUNDS)
        );
    }
}

#[test]
fn monitor_groups_list_artifacts_then_ticks_then_shells() {
    let mut tick = shell(Uuid::new_v4(), "Quotes");
    tick.group = TickGroup::Ticks;
    tick.instance = "calendar:quotes".parse().unwrap();
    let mut artifact = shell(Uuid::new_v4(), "Brief");
    artifact.group = TickGroup::Artifacts;
    artifact.instance = "artifact:https://docs.example/brief".parse().unwrap();
    let mut monitor = Monitor::default();
    for entry in [shell(Uuid::new_v4(), "make test"), tick, artifact] {
        monitor.replace(entry.group, vec![entry]);
    }
    let headings = monitor
        .lines(40, 20, DisplayProfile::default(), 0)
        .iter()
        .map(RoleLine::text)
        .filter(|line| line.ends_with("(1)"))
        .collect::<Vec<_>>();
    assert_eq!(headings, ["Artifacts (1)", "Ticks (1)", "Shells (1)"]);
}

#[test]
fn a_confirmation_option_shows_checked_and_space_turns_it_off_before_enter() {
    let mut state = State::new(true, 100, 25);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let instance = "hosted:00000000-0000-0000-0000-000000000001".to_owned();
    state.monitor.confirmation = Some(crate::terminal::monitor::TickConfirmation {
        instance: instance.clone(),
        action: crate::terminal::tick_actions::builtin::artifact(&instance, 'A'),
        prompt: "Open a new instance?".into(),
        option: Some(crate::terminal::monitor::ConfirmationOption {
            label: "Also restore its 1 Tick Subscription at their frequencies".into(),
            is_chosen: true,
            action_without: crate::terminal::tick_actions::builtin::artifact(&instance, 'B'),
        }),
    });
    let screen = |state: &mut State| {
        state.draw_frame(&mut Vec::new()).unwrap();
        state.rendered_screen.join("\n")
    };
    let text = screen(&mut state);
    assert!(
        text.contains("  [x] Also restore its 1 Tick Subscription at their frequencies"),
        "{text}"
    );
    assert!(
        text.contains("enter confirm · space toggle · esc back"),
        "{text}"
    );
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .is_none()
    );
    let text = screen(&mut state);
    assert!(text.contains("  [ ] Also restore"), "{text}");
    let Some(InputEvent::TickAction { action, .. }) =
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("confirming runs an action");
    };
    assert_eq!(
        action.key, 'B',
        "the option turned off runs the action without it"
    );
    assert!(state.monitor.confirmation.is_none());
}

/// An embedded Agent whose Chat has one Loop subscribed to `cal:watched`, with a Console showing
/// that Chat's Monitor.
async fn subscribed_calendar_chat(
    directory: &std::path::Path,
) -> (
    tickloop_agent_runtime::Agent,
    crate::terminal::execution::ConsoleExecution,
    Console,
    Uuid,
) {
    use tickloop_agent_runtime::{
        tick_adapter::{TickAdapter, TickAdapters},
        tick_stream::{InMemoryTickStream, TickStreamPort},
    };
    struct Calendar(InMemoryTickStream);
    impl TickAdapter for Calendar {
        fn access(&self) -> &'static str {
            "cal"
        }
        fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
            Some(Arc::new(self.0.clone()))
        }
    }
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::call(
                "subscribe",
                "SubscribeTick",
                serde_json::json!({"tick_instance_id":"cal:watched"}),
            ),
            tickloop_agent_test_support::text("watching"),
        ])),
    );
    core.install_tick_stream(Arc::new(
        TickAdapters::new([Arc::new(Calendar(InMemoryTickStream::new())) as Arc<dyn TickAdapter>])
            .unwrap(),
    ))
    .await;
    let agent = crate::terminal::execution::ConsoleExecution::from_embedded(
        core.clone(),
        tickloop_agent_test_support::config(),
    );
    let chat = agent.create_chat(directory.into()).await.unwrap();
    let mut events = core.subscribe();
    core.submit(chat, None, "watch the calendar".into())
        .await
        .unwrap();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            tickloop_agent_protocol::EventKind::Completed { .. } => break,
            tickloop_agent_protocol::EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let console = Console::test_for_chat(chat);
    console
        .update_monitor_chat(agent.status(chat).await.unwrap())
        .unwrap();
    console
        .update_subscribed_ticks(chat, &agent.tick_subscriptions(chat).await.unwrap())
        .unwrap();
    (core, agent, console, chat)
}

/// Prepares `intent` as the Console would on its key and runs it.
async fn run_intent(
    context: crate::terminal::tick_actions::TickActionContext<'_>,
    intent: crate::monitor_rows::TickIntent,
) -> Result<(), String> {
    use crate::terminal::tick_actions::{TickActionUi, builtin, execute};
    let action = {
        let mut state = context.console.state.lock().unwrap();
        builtin::executor(&intent)
            .unwrap()
            .prepare(&mut TickActionUi { state: &mut state })?
    };
    execute(action, context).await
}

#[tokio::test]
async fn f_subscribes_a_tick_on_the_newest_loop_and_only_tells_when_it_already_is() {
    use crate::monitor_rows::TickIntent;
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, console, chat) = subscribed_calendar_chat(directory.path()).await;
    let context = crate::terminal::tick_actions::TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    let other = tick_protocol::InstanceRef::new("cal", "other").unwrap();
    let loop_id = agent.tick_subscriptions(chat).await.unwrap()[0].loop_id;

    run_intent(context, TickIntent::Subscribe(other.clone()))
        .await
        .unwrap();
    let subscriptions = agent.tick_subscriptions(chat).await.unwrap();
    let added = subscriptions
        .iter()
        .find(|row| row.instance == other)
        .expect("f subscribes the Tick");
    assert_eq!(added.loop_id, loop_id);
    assert_eq!(
        added.frequency,
        tickloop_agent_protocol::TickSubscriptionFrequency::High
    );
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .status
            .contains("Subscribed at high")
    );

    run_intent(context, TickIntent::Subscribe(other.clone()))
        .await
        .unwrap();
    assert_eq!(agent.tick_subscriptions(chat).await.unwrap().len(), 2);
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .status
            .starts_with("Already subscribed")
    );

    let without_loop = agent.create_chat(directory.path().into()).await.unwrap();
    let refused = run_intent(
        crate::terminal::tick_actions::TickActionContext {
            chat: without_loop,
            ..context
        },
        TickIntent::Subscribe(other),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("no Loop"), "{refused}");
    core.shutdown().await;
}

#[tokio::test]
async fn closing_and_reopening_a_hosted_tick_ask_first_and_name_its_subscriptions() {
    use crate::monitor_rows::TickIntent;
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, console, chat) = subscribed_calendar_chat(directory.path()).await;
    let context = crate::terminal::tick_actions::TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    let watched = tick_protocol::InstanceRef::new("cal", "watched").unwrap();
    let confirmation = || {
        let state = console.state.lock().unwrap();
        state.monitor.confirmation.as_ref().map(|confirmation| {
            (
                confirmation.prompt.clone(),
                confirmation
                    .option
                    .as_ref()
                    .map(|option| (option.label.clone(), option.is_chosen)),
            )
        })
    };

    run_intent(
        context,
        TickIntent::CloseHosted {
            instance: watched.clone(),
            ends_process: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        confirmation(),
        Some((
            "Close this Tick, end its process and cancel its 1 Tick Subscription? enter confirm · esc cancel"
                .into(),
            None
        ))
    );
    console.state.lock().unwrap().monitor.confirmation = None;

    run_intent(context, TickIntent::ReopenHosted(watched.clone()))
        .await
        .unwrap();
    assert_eq!(
        confirmation(),
        Some((
            "Open a new instance of this Tick with its command? It takes this row's place. enter confirm · esc cancel"
                .into(),
            Some((
                "Also restore its 1 Tick Subscription at their frequencies".into(),
                true
            ))
        )),
        "restoring the subscriptions is offered, checked"
    );
    assert_eq!(
        agent.tick_subscriptions(chat).await.unwrap().len(),
        1,
        "nothing changes before the user confirms"
    );
    core.shutdown().await;
}
