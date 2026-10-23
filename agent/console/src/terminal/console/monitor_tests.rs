use super::*;
use crate::terminal::monitor::{MonitorEntry, TickGroup};
use tickloop_agent_protocol::{
    ChatSnapshot, ChatStatus, MonitorMember, MonitorMembership, ShellTaskState,
    TickSubscriptionStatus,
};

fn remove_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
}

fn confirm_removal(state: &mut State) -> Option<InputEvent> {
    assert!(
        state.key(remove_key()).is_none(),
        "the first press only prompts"
    );
    assert!(
        state
            .confirmation_hint()
            .unwrap()
            .starts_with("Press Ctrl+X again to ")
    );
    let event = state.key(remove_key());
    assert!(state.monitor_confirmation.is_none());
    event
}

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

pub(super) fn chat_snapshot(chat: Uuid) -> ChatSnapshot {
    serde_json::from_value(serde_json::json!({
        "id": chat,
        "folder": "/fixture",
        "messages": [],
        "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
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
fn shell_ticks_only_list_background_executions_and_ctrl_x_twice_removes_a_finished_one() {
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
            let removal = confirm_removal(&mut state);
            assert!(
                matches!(&removal, Some(InputEvent::TickAction { action, .. }) if action.key == 'x' && action.instance == task.to_string()),
                "Ctrl+X twice asks the Agent to record the removal"
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
fn mixed_monitor_navigation_orders_ticks_tasks_and_fixed_goal_with_matching_hints() {
    use crate::terminal::monitor::MonitorTarget;

    let mut state = click_fixture(140, false, TickGroup::Shells);
    let tick = state.monitor.entries()[0].clone();
    let mut snapshot = chat_snapshot(state.chat.unwrap());
    snapshot.scheduled_tasks = serde_json::from_value(serde_json::json!([{
        "id":"0123abcd", "kind":{"type":"recurring", "cron":"* * * * *"},
        "prompt":"check weather", "creator":"user", "time_zone":"Asia/Shanghai",
        "created_at_ms":0, "next_fire_at_ms":60000, "scheduled_for_ms":60000,
        "jitter_ms":0, "skipped":0
    }]))
    .unwrap();
    snapshot.goal = Some(
        serde_json::from_value(serde_json::json!({
            "id":Uuid::from_u128(707), "objective":"Verify release evidence",
            "objective_revision":1, "set_by":"user", "status":{"state":"active"},
            "tokens_used":0, "time_used_ms":0, "quiet_turns":0,
            "created_at_ms":0, "updated_at_ms":0
        }))
        .unwrap(),
    );
    state.monitor.chat_status = Some(Arc::new(ChatStatus::of(&snapshot)));
    state.monitor.replace(TickGroup::Shells, vec![tick.clone()]);
    state.monitor.focus(false, None);
    assert_eq!(
        state.monitor.total(),
        3,
        "Ticks, scheduled tasks and Goal all count"
    );

    let expected = [
        (
            MonitorTarget::Tick(tick.instance.clone()),
            "exit 0",
            "enter open · Ctrl+X remove · esc input",
        ),
        (
            MonitorTarget::ScheduledTask("0123abcd".into()),
            "",
            "enter details · Ctrl+X remove · esc input",
        ),
        (
            MonitorTarget::Goal,
            "",
            "enter Goal details · j/k select · esc return to input",
        ),
    ];
    let mut goal_row = None;
    for (key, index) in [('k', 0), ('j', 1), ('j', 2), ('j', 2), ('k', 1), ('k', 0)] {
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                .is_none()
        );
        let (target, footer, hint) = &expected[index];
        match target {
            MonitorTarget::Tick(instance) => {
                assert_eq!(&state.monitor.selected().unwrap().instance, instance)
            }
            MonitorTarget::ScheduledTask(id) => {
                assert_eq!(&state.monitor.selected_task().unwrap().id, id)
            }
            MonitorTarget::MoreScheduled => panic!("one scheduled task has no overflow row"),
            MonitorTarget::Goal => {
                assert!(state.monitor.is_goal_selected());
                assert!(state.monitor.selected().is_none());
                assert!(state.monitor.selected_task().is_none());
            }
        }
        assert_eq!(state.monitor.hint(), *hint);
        let height = state.monitor.side_content_height(80, 15, state.display, 0);
        let frame = state.monitor.frame(80, height, state.display, 0);
        let scheduled_heading = frame
            .lines
            .iter()
            .position(|line| line.text() == "Scheduled (1)")
            .unwrap();
        assert!(frame.lines[scheduled_heading - 1].text().is_empty());
        if !footer.is_empty() {
            assert_eq!(frame.lines.last().unwrap().text(), *footer);
        }
        let row = frame
            .lines
            .iter()
            .position(|line| line.text().contains("Pursuing goal"))
            .unwrap();
        assert_eq!(
            *goal_row.get_or_insert(row),
            row,
            "Goal stays fixed at the bottom"
        );
        assert!(
            !frame
                .lines
                .iter()
                .any(|line| line.text() == "Nothing to monitor")
        );
        assert!(
            frame
                .targets
                .iter()
                .all(|(_, target)| *target != MonitorTarget::Goal)
        );
        assert_eq!(
            frame.lines[row].fill, None,
            "the Goal's state line is a group title"
        );
        assert_eq!(
            frame.lines[row + 1].fill == Some(Role::Selection),
            index == 2
        );
    }

    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::TickAction { action, .. }) if action.instance == tick.instance.to_string() && action.key == crate::monitor_rows::MONITOR_ENTER_KEY)
    );
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .is_none()
    );
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::TickAction { action, .. }) if action.instance == "0123abcd")
    );
    assert!(state.monitor.detail.is_some());
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.monitor.detail.is_none());
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.key(remove_key()).is_none());
    assert!(state.monitor.is_goal_selected());
    assert!(state.monitor.focused);
    assert!(state.editor.draft().is_empty());
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Line(line)) if line == "/goal")
    );
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
    for action in ['c', 'f', 'h'] {
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
fn passive_tasks_allow_monitor_focus_on_right_and_shift_arrows() {
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
                Some(crate::terminal::status_area::test_task_board(&[
                    "own passive task",
                ])),
            )
            .unwrap();
        let mut state = console.state.lock().unwrap();
        state.draw(&mut io::sink()).unwrap();
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|row| row.contains("1 tasks ("))
        );
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|row| row.contains("own passive task"))
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
fn back_to_bottom_centres_over_the_conversation_beside_monitor() {
    // A non-interactive State keeps `open_monitor` from painting the test's stdout.
    let mut state = State::new(false, 140, 30);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state
        .monitor
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "A")]);
    state.scrollback.append(
        &(0..200)
            .map(|row| format!("history {row:03}\n"))
            .collect::<String>(),
    );
    let console = tests::console_with_state(state);
    console.open_monitor().unwrap();
    let mut state = console.state.lock().unwrap();
    state.interactive = true;
    state.key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.monitor_visible() && !state.monitor.overlay);
    let content_columns = state.content_columns();
    assert!(content_columns < state.width);
    let target = state.back_to_bottom_target.as_ref().unwrap();
    let left = target.columns.start;
    let right = content_columns - target.columns.end;
    assert!(
        left.abs_diff(right) <= 1,
        "{left} before and {right} after within {content_columns} columns"
    );
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
    let removal = confirm_removal(&mut state);
    let Some(InputEvent::TickAction { action, .. }) = removal else {
        panic!("Ctrl+X twice must ask the Agent to remove the row");
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
            .update("Auto".into(), "model".into(), Some((10, 100)), None);
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
    let row: TickSubscriptionStatus = serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"loop_state":"Idle","instance":"calendar:fixture","frequency":"high","accumulated":0,"last_delivery_ms":1000,"last_delivery":null,"link":"Live","kind":{"Terminal":{"truncated":false}},"title":"Quote terminal","updated_at_ms":null})).unwrap();
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
            is_waiting: false,
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
    let row: TickSubscriptionStatus = serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"loop_state":"Idle","instance":"artifact:https://docs.example/doc","frequency":"high","accumulated":0,"last_delivery_ms":null,"last_delivery":null,"link":"Live","kind":"Protocol","title":"Subscribed document","updated_at_ms":1000})).unwrap();
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
            terminals: Default::default(),
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
            is_terminal: false,
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
fn monitor_scrolls_continuously_across_pages_and_caches_earlier_rows() {
    let mut state = docked_monitor_fixture(0, 0);
    Arc::make_mut(state.monitor.chat_status.as_mut().unwrap()).goal = None;
    for index in 0..270 {
        Arc::make_mut(state.monitor.chat_status.as_mut().unwrap())
            .monitor
            .members
            .push(MonitorMember {
                is_terminal: false,
                instance: format!("calendar:member-{index:03}").parse().unwrap(),
                title: Some(format!("Tick {index:03}")),
            });
    }
    state.monitor.rebuild();
    assert_eq!(state.monitor.entries().len(), 128);
    assert_eq!(state.monitor.total(), 270);
    let rendered = screen(&mut state);
    assert!(rendered.contains("Ticks (270)"), "{rendered}");
    assert!(!rendered.contains("of 270"), "{rendered}");
    let initial = state.monitor.entries()[0].instance.clone();
    for index in 0..269 {
        assert_eq!(
            state.monitor.selected().unwrap().title,
            format!("Tick {index:03}")
        );
        let before = state.monitor.frame(40, 12, state.display, 0);
        state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let after = state.monitor.frame(40, 12, state.display, 0);
        let before_ticks = before.targets.iter().map(|(_, id)| id).collect::<Vec<_>>();
        assert!(
            after
                .targets
                .iter()
                .any(|(_, id)| before_ticks.contains(&id)),
            "crossing a page keeps neighbouring rows visible at {index}"
        );
        if index == 120 {
            assert_eq!(
                state.monitor.entries().len(),
                256,
                "prefetch before reaching the loaded end"
            );
            assert_eq!(state.monitor.entries()[0].instance, initial);
        }
    }
    assert_eq!(state.monitor.entries().len(), 270);
    for index in (0..269).rev() {
        state.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(
            state.monitor.selected().unwrap().title,
            format!("Tick {index:03}")
        );
        state.monitor.frame(40, 12, state.display, 0);
    }
    assert_eq!(
        state.monitor.entries().len(),
        270,
        "upward scrolling reuses cached pages"
    );
    assert!(screen(&mut state).contains("Ticks (270)"));
}

#[test]
fn probed_members_report_their_host_state_and_owner() {
    let console = Console::test_for_chat(Uuid::new_v4());
    let chat = console.state.lock().unwrap().chat.unwrap();
    let mut snapshot = chat_status(chat);
    for id in ["quotes", "news"] {
        snapshot.monitor.members.push(MonitorMember {
            is_terminal: false,
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
        is_terminal: false,
        instance: tick_protocol::InstanceRef::new("calendar", "opened").unwrap(),
        title: Some("Opened counter".into()),
    });
    state.monitor.chat_status = Some(Arc::new(snapshot));
    state.monitor.rebuild();
    assert_eq!(state.monitor.entries().len(), 1);
    state.monitor.focus(false, None);
    assert!(screen(&mut state).contains("enter details · Ctrl+X remove"));
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
    subscribed.notes = vec!["feedback unavailable".into()];
    subscribed.subscription = Some(crate::monitor_rows::SubscriptionMark {
        frequency: tickloop_agent_protocol::TickSubscriptionFrequency::High,
        is_waiting: true,
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
        (140, None, &subscribed, "feedback unavailable", true),
        (200, Some(80), &subscribed, "feedback unavailable", false),
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
fn row_details_end_at_the_monitor_edge_and_the_mark_leads_the_row_at_any_width() {
    use crate::monitor_rows::{RowUpdate, StatusTone, Unread};
    let unread = Some(RowUpdate {
        at_ms: 1000,
        unread: Some(Unread { count: Some(26) }),
        taken_in_at_ms: None,
    });
    let mut subscribed = shell(Uuid::new_v4(), "python3 -q");
    subscribed.group = TickGroup::Ticks;
    subscribed.instance = "hosted:python".parse().unwrap();
    subscribed.subscription = Some(crate::monitor_rows::SubscriptionMark {
        frequency: tickloop_agent_protocol::TickSubscriptionFrequency::Low,
        is_waiting: true,
        next_check: None,
    });
    subscribed.tone = StatusTone::Live;
    subscribed.update = unread.clone();
    let mut running = shell(Uuid::new_v4(), "cargo nextest run --workspace");
    running.tone = StatusTone::Running {
        since_ms: Some(tickloop_agent_base::private_fs::unix_time_milliseconds() - 119_000),
    };
    running.update = unread;
    for width in [80, 140] {
        let mut state = State::new(true, width, 25);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state
            .monitor
            .replace(TickGroup::Ticks, vec![subscribed.clone()]);
        state
            .monitor
            .replace(TickGroup::Shells, vec![running.clone()]);
        state.draw_frame(&mut Vec::new()).unwrap();
        let side = state.monitor_columns() - 2;
        let screen = &state.rendered_screen;
        let monitor_rows = |title: &str| {
            let first = screen
                .iter()
                .position(|row| row.contains(title))
                .unwrap_or_else(|| panic!("{width}: {screen:#?}"));
            screen[first..]
                .iter()
                .map_while(|row| row.rsplit_once("│ ").map(|(_, side)| side.to_owned()))
                .collect::<Vec<_>>()
        };
        let subscribed_row = monitor_rows("python3 -q").remove(0);
        assert!(
            subscribed_row.starts_with("↻ ") && subscribed_row.matches('↻').count() == 1,
            "{width}: the mark leads the row, a blank column after it: {subscribed_row:?}"
        );
        let running_rows = monitor_rows("cargo nextest");
        let details_row = running_rows
            .iter()
            .find(|row| row.trim_end().ends_with("1m 59s"))
            .unwrap_or_else(|| panic!("{width}: {screen:#?}"));
        assert_eq!(display_width(details_row), side, "{width}: {details_row:?}");
        assert!(
            !details_row.contains('●') && !details_row.contains("26"),
            "{width}: the details reach the edge, without a dot or count: {details_row:?}"
        );
        assert_eq!(
            subscribed_row
                .find("python3")
                .map(|byte| display_width(&subscribed_row[..byte])),
            running_rows[0]
                .find("cargo")
                .map(|byte| display_width(&running_rows[0][..byte])),
            "{width}: names start in one column"
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
        let lines = state
            .monitor
            .lines(60, 10, DisplayProfile::default(), ended + 1_000);
        let text = lines.iter().map(RoleLine::text).collect::<String>();
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.role == Some(Role::UpdateMark))
                && !text.contains('●')
                && !text.contains("1s"),
            "an unread end shows its glyph in the update colour and no time: {text}"
        );
        assert!(
            lines.iter().all(|line| line.band.is_none()),
            "a Shell's end comes at once, so no bar counts toward it"
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
    assert_eq!(
        row_fill(&state, taken_in + 100),
        Some(Role::TakeInFlashStrong)
    );
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
fn reading_a_shells_output_flashes_its_row_cold_and_a_chat_switch_does_not_replay_it() {
    let chat = Uuid::new_v4();
    let console = Console::test_for_chat(chat);
    let mut snapshot = tests::task_snapshot(chat, 1);
    snapshot.tasks[0].was_background = true;
    snapshot.tasks[0].command = "make test".into();
    let shell = tick_protocol::InstanceRef::shell(snapshot.tasks[0].id);
    let mut chat_snapshot = chat_status(chat);
    console.update_monitor_chat(chat_snapshot.clone()).unwrap();
    console.refresh_monitor_ticks(snapshot, None).unwrap();
    let display = DisplayProfile::default();
    let row_fill = |state: &State, now_ms| {
        let lines = state.monitor.frame(40, 10, display, now_ms).lines;
        lines
            .iter()
            .find(|line| line.text().contains("make test"))
            .expect("the Shell keeps its row")
            .fill
    };

    let used = tickloop_agent_base::private_fs::unix_time_milliseconds();
    chat_snapshot.monitor.mark_used(shell.clone(), used);
    chat_snapshot.monitor.mark_handled(shell, used);
    console.update_monitor_chat(chat_snapshot.clone()).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(row_fill(&state, used + 100), Some(Role::UseFlashStrong));
    assert_eq!(row_fill(&state, used + 500), Some(Role::UseFlashSoft));
    // The View comes back to the Chat after the use, never in the same millisecond.
    std::thread::sleep(std::time::Duration::from_millis(2));
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.switch_view_chat(Some(chat));
    drop(state);
    console.update_monitor_chat(chat_snapshot).unwrap();
    let state = console.state.lock().unwrap();
    assert_eq!(row_fill(&state, used + 100), None);
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
        !keys.contains(&'w') && !keys.contains(&'u') && !keys.contains(&'f'),
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
            row.accumulated = index as u64;
            row
        })
        .collect();
    let mut snapshot: tickloop_agent_protocol::ChatSnapshot = serde_json::from_value(
        serde_json::json!({"id":Uuid::from_u128(77),"folder":"/fixture","messages":[],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}}),
    )
    .unwrap();
    snapshot.monitor.record_opened(
        tick_protocol::InstanceRef::new("calendar", "fixture-run").unwrap(),
        Some("calendar".into()),
    );
    state.monitor.chat_status = Some(std::sync::Arc::new(
        tickloop_agent_protocol::ChatStatus::of(&snapshot),
    ));
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
fn subscriptions_have_one_key_and_a_fixed_monitor_footer() {
    let mut state = subscribed_fixture(1);
    let text = monitor_text(&mut state);
    assert!(text.contains("Live · Every 1 min"), "{text}");
    let keys = state
        .monitor
        .selected()
        .unwrap()
        .actions
        .iter()
        .map(|action| action.key)
        .collect::<Vec<_>>();
    assert!(keys.contains(&'s'));
    for old in ['f', 'w', 'u'] {
        assert!(!keys.contains(&old));
        assert!(press(&mut state, KeyCode::Char(old)).is_none());
        state.monitor.focus(false, None);
    }
    assert!(
        state.monitor.hint().contains("s subscriptions"),
        "shortcuts belong in the global hint row"
    );
    for frequency in [Some("10 min"), Some("30 min"), None] {
        assert_eq!(requested_key(press(&mut state, KeyCode::Char('s'))), 's');
        let text = monitor_text(&mut state);
        if let Some(frequency) = frequency {
            assert!(text.contains(&format!("Every {frequency}")));
        } else {
            assert!(
                !state
                    .monitor
                    .frame(60, 10, state.display, 0)
                    .lines
                    .last()
                    .unwrap()
                    .text()
                    .contains("Every")
            );
        }
        assert!(
            !state
                .monitor
                .frame(60, 10, state.display, 0)
                .lines
                .last()
                .unwrap()
                .text()
                .contains("Not subscribed")
        );
        assert!(
            !state
                .monitor
                .frame(60, 10, state.display, 0)
                .lines
                .last()
                .unwrap()
                .text()
                .contains("off")
        );
        assert!(
            state.monitor.detail.is_none(),
            "switching never opens a confirmation or picker"
        );
    }
    assert_eq!(requested_key(press(&mut state, KeyCode::Char('s'))), 's');
}

#[test]
fn the_row_switch_updates_all_periodic_subscriptions_of_the_instance() {
    let mut state = subscribed_fixture(3);
    for frequency in [
        Some(tickloop_agent_protocol::TickSubscriptionFrequency::Medium),
        Some(tickloop_agent_protocol::TickSubscriptionFrequency::Low),
        None,
    ] {
        assert_eq!(requested_key(press(&mut state, KeyCode::Char('s'))), 's');
        let shown = state.monitor.shown_subscriptions();
        match frequency {
            Some(frequency) => assert!(shown.iter().all(|row| row.frequency == frequency)),
            None => assert!(shown.is_empty()),
        }
        assert!(state.monitor.detail.is_none());
    }
}

#[test]
fn a_refused_subscription_switch_restores_the_reported_frequency() {
    let mut state = subscribed_fixture(1);
    let id = state.monitor.subscriptions[0].id;
    press(&mut state, KeyCode::Char('s'));
    assert!(monitor_text(&mut state).contains("Every 10 min"));
    state.monitor.forget_requested_subscription(id);
    assert!(monitor_text(&mut state).contains("Every 1 min"));
    assert!(state.monitor.detail.is_none());
}

#[test]
fn the_subscription_footer_stays_at_the_panel_bottom() {
    let state = subscribed_fixture(1);
    for height in [4, 10, 20] {
        let frame = state.monitor.frame(42, height, state.display, 0);
        assert_eq!(frame.lines.len(), height);
        assert_eq!(frame.lines.last().unwrap().text(), "Live · Every 1 min");
        assert!(frame.targets.iter().all(|(row, _)| *row < height - 1));
    }
}

#[tokio::test]
async fn removing_a_subscribed_row_after_confirmation_cancels_its_subscriptions_and_reports_it() {
    use crate::terminal::{
        execution::ConsoleExecution,
        tick_actions::{TickActionContext, execute},
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
                "TickManage",
                serde_json::json!({"action":"subscribe","tick_instance_id":"cal:watched"}),
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
    core.submit(chat, "watch the calendar".into())
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
        state.monitor.focus(false, None);
        let Some(InputEvent::TickAction { action, .. }) = confirm_removal(&mut state) else {
            panic!("no removal action")
        };
        action
    };
    execute(removal, context).await.unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.refresh_status("Working");
        assert!(state.status_held);
    }

    assert_eq!(
        console.test_status_text(),
        "Removed from Monitor · 1 Tick Subscription cancelled"
    );
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
fn a_gone_row_stays_red_until_removed_and_enter_offers_to_reopen_it() {
    use crate::monitor_rows::{MemberFindings, MemberState, TickIntent};
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
    assert!(!state.monitor.refresh_due(went + 3_600_000));
    let entries = state.monitor.entries();
    assert_eq!(entries.len(), 1, "a gone Tick keeps its row");
    assert_eq!(entries[0].tone, crate::monitor_rows::StatusTone::Gone);
    let lines = state
        .monitor
        .lines(40, 10, DisplayProfile::default(), went + 3_600_000);
    let glyph = lines[2]
        .spans
        .iter()
        .find(|span| span.text == "◉")
        .and_then(|span| span.role);
    assert_eq!(glyph, Some(Role::ErrorMark), "the row shows red");
    assert_eq!(
        entries[0].actions[0].intent,
        TickIntent::ReopenHosted(gone.clone())
    );
    assert_eq!(
        entries[0].actions[1].intent,
        TickIntent::CloseHosted {
            instance: gone,
            ends_process: false,
        },
        "x takes the row out with its launch record"
    );
}

/// A running Shell is stopped from its Monitor row and its row shows it ended.
#[tokio::test]
async fn a_running_shell_is_stopped_from_its_row_after_confirmation_with_feedback() {
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
        .submit_user_shell(
            chat,
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
    console.state.lock().unwrap().monitor.focus(false, None);
    let Some(InputEvent::TickAction { action, .. }) =
        confirm_removal(&mut console.state.lock().unwrap())
    else {
        panic!("Ctrl+X twice did not stop the Shell");
    };
    assert_eq!(action.instance, shell.to_string());
    execute(action, context).await.unwrap();
    assert_eq!(console.test_status_text(), "Shell stopped");
    assert!(console.state.lock().unwrap().monitor.detail.is_none());
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
        actions.iter().all(|action| !matches!(
            action.intent,
            crate::monitor_rows::TickIntent::StopShell { .. }
        )),
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
        "folder": "/fixture",
        "messages": [],
        "artifact_ticks": [{"artifact_id":"doc","origin":"https://docs.example","title":"Brief","format":"markdown","url":"https://docs.example/a/doc","access_role":"owner","added_at_ms":1}],
        "chat_loop": {"id":Uuid::from_u128(900),"state":"Idle","event_sources":[{"origin":"https://docs.example","account_id":"owner","artifact_id":"doc","binding_id":Uuid::from_u128(5),"client_instance_id":Uuid::from_u128(6),"epoch":1,"resume_required":resume_required,"last_request_at_ms":null}]}
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
fn an_artifact_linked_here_shows_no_subscription_mark_and_ctrl_x_twice_unlinks_it() {
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
            ('f', "feedback"),
            ('h', "include earlier feedback")
        ]
    );
    assert_eq!(requested_key(confirm_removal(&mut state)), 'x');

    let mut subscription = tests::subscription_row(3);
    subscription.instance = artifact;
    subscription.accumulated = 3;
    subscription.updated_at_ms = Some(1000);
    state.monitor.subscriptions = vec![subscription];
    state.monitor.rebuild();
    let text = monitor_text(&mut state);
    let row = text.lines().find(|line| line.contains("Brief")).unwrap();
    assert!(
        row.trim_end().ends_with("Brief") && !row.contains('●') && !row.contains('↻'),
        "an artifact never shows a Tick Subscription: {row}"
    );
    assert_eq!(
        action_labels(&state),
        [
            ('o', "open"),
            ('x', "unlink"),
            ('c', "copy"),
            ('f', "feedback"),
            ('h', "include earlier feedback")
        ]
    );
}

#[test]
fn an_unavailable_association_stays_in_words() {
    let (mut state, artifact) = linked_artifact_fixture(true);
    let mut subscription = tests::subscription_row(3);
    subscription.instance = artifact;
    state.monitor.subscriptions = vec![subscription];
    state.monitor.rebuild();
    let text = monitor_text(&mut state);
    let row = text.lines().find(|line| line.contains("Brief")).unwrap();
    assert!(
        row.contains("feedback unavailable") && !row.contains('↻'),
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
            snapshot.folder.as_deref().unwrap(),
            &snapshot.additional_directories,
        )
        .unwrap();
    console.set_visible_delegation(false).unwrap();
    console.set_foreground_shell(changed).unwrap();
    console
        .set_queued_inputs(chat, snapshot.queued_inputs())
        .unwrap();
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
            None,
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
                "TickManage",
                serde_json::json!({"action":"subscribe","tick_instance_id":"cal:watched"}),
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
    core.submit(chat, "watch the calendar".into())
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
async fn rapid_subscription_presses_across_the_first_reply_each_advance_one_step() {
    use crate::monitor_rows::TickIntent;
    use crate::terminal::tick_actions::{TickActionUi, builtin, execute};
    use tickloop_agent_protocol::TickSubscriptionFrequency::Low;
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, console, chat) = subscribed_calendar_chat(directory.path()).await;
    let context = crate::terminal::tick_actions::TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    let instance = tick_protocol::InstanceRef::new("cal", "other").unwrap();
    let press = || {
        let mut state = console.state.lock().unwrap();
        builtin::executor(&TickIntent::Subscribe(instance.clone()))
            .unwrap()
            .prepare(&mut TickActionUi { state: &mut state })
            .unwrap()
    };
    let first = press();
    let second = press();
    execute(first, context).await.unwrap();
    let third = press();
    execute(second, context).await.unwrap();
    execute(third, context).await.unwrap();
    let subscriptions = agent.tick_subscriptions(chat).await.unwrap();
    assert_eq!(
        subscriptions
            .iter()
            .find(|row| row.instance == instance)
            .unwrap()
            .frequency,
        Low
    );
    execute(press(), context).await.unwrap();
    assert!(
        !agent
            .tick_subscriptions(chat)
            .await
            .unwrap()
            .iter()
            .any(|row| row.instance == instance)
    );
    core.shutdown().await;
}

#[tokio::test]
async fn s_subscribes_a_tick_then_cycles_through_frequencies_and_off_without_a_notice() {
    use crate::monitor_rows::TickIntent;
    use tickloop_agent_protocol::TickSubscriptionFrequency::{High, Medium};
    let directory = tempfile::tempdir().unwrap();
    let (core, agent, console, chat) = subscribed_calendar_chat(directory.path()).await;
    let context = crate::terminal::tick_actions::TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    let other = tick_protocol::InstanceRef::new("cal", "other").unwrap();
    let frequency_of_other = || async {
        agent
            .tick_subscriptions(chat)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.instance == other)
            .map(|row| row.frequency)
    };
    let status = || console.state.lock().unwrap().status.clone();

    run_intent(context, TickIntent::Subscribe(other.clone()))
        .await
        .unwrap();
    assert_eq!(frequency_of_other().await, Some(High));
    assert!(
        status().is_empty(),
        "frequency stays in the Monitor footer: {}",
        status()
    );

    run_intent(context, TickIntent::Subscribe(other.clone()))
        .await
        .unwrap();
    assert_eq!(frequency_of_other().await, Some(Medium));
    assert!(status().is_empty());

    let (first, second) = {
        let mut state = console.state.lock().unwrap();
        let mut press = || {
            crate::terminal::tick_actions::builtin::executor(&TickIntent::Subscribe(other.clone()))
                .unwrap()
                .prepare(&mut crate::terminal::tick_actions::TickActionUi { state: &mut state })
                .unwrap()
        };
        (press(), press())
    };
    crate::terminal::tick_actions::execute(first, context)
        .await
        .unwrap();
    crate::terminal::tick_actions::execute(second, context)
        .await
        .unwrap();
    assert_eq!(
        frequency_of_other().await,
        None,
        "two presses before the Agent replies go on to low, then off"
    );
    assert_eq!(agent.tick_subscriptions(chat).await.unwrap().len(), 1);

    console.update_subscribed_ticks(chat, &[]).unwrap();
    run_intent(context, TickIntent::Subscribe(other.clone()))
        .await
        .unwrap();
    assert_eq!(
        agent.tick_subscriptions(chat).await.unwrap().len(),
        2,
        "a View that has not heard of the subscription yet subscribes nothing twice"
    );
    assert!(status().is_empty());

    let fresh = agent.create_chat(directory.path().into()).await.unwrap();
    run_intent(
        crate::terminal::tick_actions::TickActionContext {
            chat: fresh,
            ..context
        },
        TickIntent::Subscribe(tick_protocol::InstanceRef::new("cal", "unsubscribed").unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(
        agent.tick_subscriptions(fresh).await.unwrap().len(),
        1,
        "a new Chat's Loop subscribes before any input"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn closing_and_reopening_a_hosted_tick_execute_directly_and_report_the_result() {
    use crate::monitor_rows::TickIntent;
    use crate::terminal::execution::ConsoleExecution;
    use tickloop_agent_daemon::EmbeddedAgentAccess;
    use tickloop_agent_protocol::*;

    struct HostedRemote {
        inner: EmbeddedAgentAccess,
        commands: Mutex<Vec<AgentCommand>>,
        reopened: tick_protocol::InstanceRef,
    }
    #[async_trait::async_trait]
    impl AgentAccess for HostedRemote {
        async fn observe(&self, request: ObservationRequest) -> Result<Observation, AccessError> {
            self.inner.observe(request).await
        }
        async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
            let result = match &envelope.command {
                AgentCommand::CloseHostedTick { .. } => Some(CommandResult::Done),
                AgentCommand::ReopenHostedTick { .. } => Some(CommandResult::HostedTickReopened {
                    instance: self.reopened.clone(),
                    membership: MonitorMembership::default(),
                    restore_error: None,
                }),
                _ => None,
            };
            if let Some(result) = result {
                self.commands.lock().unwrap().push(envelope.command.clone());
                let mut receipt = self
                    .inner
                    .execute(CommandEnvelope {
                        command: AgentCommand::FlushTranscripts,
                        ..envelope
                    })
                    .await?;
                receipt.result = Some(result);
                return Ok(receipt);
            }
            self.inner.execute(envelope).await
        }
        async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
            self.inner.show_view(update).await
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    let remote = Arc::new(HostedRemote {
        inner: EmbeddedAgentAccess::new(core.clone()),
        commands: Mutex::new(Vec::new()),
        reopened: tick_protocol::InstanceRef::new("hosted", &Uuid::new_v4().to_string()).unwrap(),
    });
    let agent = ConsoleExecution::from_access_for_test(
        remote.clone(),
        remote.inner.daemon_epoch(),
        ViewRef {
            session_id: Uuid::new_v4(),
            view_id: Uuid::new_v4(),
        },
        LaunchSpec::embedded(directory.path().into()),
        None,
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let console = Console::test_for_chat(chat);
    let context = crate::terminal::tick_actions::TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    let watched = tick_protocol::InstanceRef::new("hosted", &Uuid::new_v4().to_string()).unwrap();
    run_intent(context, TickIntent::ReopenHosted(watched.clone()))
        .await
        .unwrap();
    assert!(matches!(remote.commands.lock().unwrap().as_slice(),
        [AgentCommand::ReopenHostedTick { instance, restore_subscriptions: true, .. }] if instance == &watched));
    assert_eq!(
        console.test_status_text(),
        format!("Reopened {watched} as {}", remote.reopened)
    );
    run_intent(
        context,
        TickIntent::CloseHosted {
            instance: watched.clone(),
            ends_process: true,
        },
    )
    .await
    .unwrap();
    assert!(matches!(&remote.commands.lock().unwrap()[1],
        AgentCommand::CloseHostedTick { instance, .. } if instance == &watched));
    assert_eq!(
        console.test_status_text(),
        "Tick closed · Removed from Monitor"
    );
    assert!(
        core.snapshot(chat)
            .await
            .unwrap()
            .monitor
            .is_removed(&watched)
    );
    core.shutdown().await;
}

#[tokio::test]
async fn scheduled_task_details_and_confirmed_delete_use_the_live_agent() {
    use crate::terminal::{
        execution::ConsoleExecution,
        tick_actions::{TickActionContext, execute},
    };
    let directory = tempfile::tempdir().unwrap();
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    let agent =
        ConsoleExecution::from_embedded(core.clone(), tickloop_agent_test_support::config());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    core.schedule_task(chat, "* * * * *", "remind me about weather".into(), true)
        .await
        .unwrap();
    let console = Console::test_for_chat(chat);
    console
        .update_monitor_chat(agent.status(chat).await.unwrap())
        .unwrap();
    let context = TickActionContext {
        agent: &agent,
        console: &console,
        root: Some(directory.path()),
        chat,
    };
    let press = |code| {
        let mut state = console.state.lock().unwrap();
        state.monitor.focus(false, None);
        let Some(InputEvent::TickAction { action, .. }) =
            state.key(KeyEvent::new(code, KeyModifiers::NONE))
        else {
            panic!("no scheduled task action")
        };
        action
    };
    execute(press(KeyCode::Enter), context).await.unwrap();
    {
        let mut state = console.state.lock().unwrap();
        let detail = state.monitor.detail.as_mut().unwrap();
        let text = detail
            .lines(80, 20, 0)
            .into_iter()
            .map(|line| line.line.text())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Cron: * * * * *",
            "remind me about weather",
            "Created:",
            "Expires:",
            "Skipped: 0",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(state.monitor.detail.is_none());
    }
    let Some(InputEvent::TickAction { action, .. }) =
        confirm_removal(&mut console.state.lock().unwrap())
    else {
        panic!("no deletion action")
    };
    execute(action, context).await.unwrap();
    assert_eq!(console.test_status_text(), "Scheduled task removed");
    assert!(
        core.snapshot(chat)
            .await
            .unwrap()
            .scheduled_tasks
            .is_empty()
    );
    assert!(!console.monitor_has_entries().unwrap());
    for index in 0..4 {
        core.schedule_task(
            chat,
            "* * * * *",
            format!("extra instruction {index}"),
            true,
        )
        .await
        .unwrap();
    }
    console
        .update_monitor_chat(agent.status(chat).await.unwrap())
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.monitor.focus(false, None);
        state.monitor.move_selection(3);
        assert!(state.monitor.is_more_selected());
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(state.monitor.task_list_is_open());
    }
    execute(press(KeyCode::Enter), context).await.unwrap();
    let Some(InputEvent::TickAction { action, .. }) =
        confirm_removal(&mut console.state.lock().unwrap())
    else {
        panic!("no deletion action")
    };
    execute(action, context).await.unwrap();
    {
        let state = console.state.lock().unwrap();
        assert!(
            state.menu.is_none(),
            "removing from details has no confirmation"
        );
        assert!(state.monitor.detail.is_none());
        assert!(state.monitor.task_list_is_open());
    }
    assert_eq!(console.test_status_text(), "Scheduled task removed");
    assert_eq!(core.snapshot(chat).await.unwrap().scheduled_tasks.len(), 3);
    core.shutdown().await;
}

#[tokio::test]
async fn all_loop_forms_invoke_the_skill_in_chats_with_and_without_a_folder() {
    use crate::terminal::execution::ConsoleExecution;
    use tickloop_agent_protocol::{Content, EventKind};
    for folder in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("ready"),
            tickloop_agent_test_support::text("ready"),
            tickloop_agent_test_support::text("ready"),
        ]));
        let mut options =
            tickloop_agent_runtime::HarnessOptions::local(tickloop_agent_test_support::config());
        if !folder {
            options.waits = tickloop_agent_runtime::WaitHosting::Durable;
            options.host.prompt_policy = tickloop_agent_protocol::PromptPolicy::DenyOnPrompt;
            options.tools = tickloop_agent_runtime::ToolPolicy {
                user_skills: true,
                ..tickloop_agent_runtime::ToolPolicy::durable_chat()
            };
        }
        let core = tickloop_agent_runtime::Agent::with_seams(
            tickloop_agent_runtime::HarnessSeams {
                transcripts: None,
                environment: folder.then(|| {
                    Arc::new(tickloop_agent_runtime::environment::LocalEnvironment::bare())
                        as Arc<dyn tickloop_agent_runtime::environment::ExecutionEnvironment>
                }),
                models: tickloop_agent_test_support::harness::ChosenModels::new(model.clone()),
            },
            options,
        )
        .unwrap();
        let agent =
            ConsoleExecution::from_embedded(core.clone(), tickloop_agent_test_support::config());
        let chat = if folder {
            agent.create_chat(directory.path().into()).await.unwrap()
        } else {
            core.create_chat_without_folder().await.unwrap()
        };
        let mut events = core.subscribe();
        for (arguments, expected) in [
            ("1m 提醒我看天气", "Form: fixed interval"),
            ("提醒我看天气", "Form: self-paced"),
            ("", "built-in maintenance instructions"),
        ] {
            let receipt = if folder {
                agent.invoke_skill(chat, "loop", arguments).await.unwrap()
            } else {
                core.invoke_skill(chat, "loop", arguments).await.unwrap()
            };
            loop {
                let event = tickloop_agent_test_support::next_event(&mut events).await;
                if event.run_id != receipt.run_id {
                    continue;
                }
                match event.kind {
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            let requests = model.requests.lock().unwrap();
            let text = requests
                .last()
                .unwrap()
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains(expected), "{text}");
        }
        core.shutdown().await;
    }
}

#[test]
fn monitor_keeps_tick_columns_and_left_aligns_scheduled_and_goal_icons() {
    let mut state = State::new(true, 140, 30);
    let chat = Uuid::from_u128(707);
    let mut snapshot = chat_snapshot(chat);
    snapshot.scheduled_tasks = serde_json::from_value(serde_json::json!([{
        "id":"0123abcd", "kind":{"type":"recurring", "cron":"* * * * *"},
        "prompt":"提醒我看天气", "creator":"user", "time_zone":"Asia/Shanghai",
        "created_at_ms":0, "scheduled_for_ms":60000, "next_fire_at_ms":60000,
        "jitter_ms":0, "skipped":0
    }]))
    .unwrap();
    snapshot.goal = Some(
        serde_json::from_value(serde_json::json!({
            "id":Uuid::from_u128(708), "objective":"记录验收证据", "objective_revision":1,
            "set_by":"user", "status":{"state":"active"}, "tokens_used":12500,
            "time_used_ms":90000, "quiet_turns":0, "created_at_ms":0, "updated_at_ms":0
        }))
        .unwrap(),
    );
    state
        .monitor
        .set_chat_status(Arc::new(ChatStatus::of(&snapshot)));
    let mut subscribed = shell(Uuid::from_u128(1), "Subscribed Tick");
    subscribed.group = TickGroup::Ticks;
    subscribed.glyph = Glyph::ProtocolTick;
    subscribed.subscription = Some(crate::monitor_rows::SubscriptionMark {
        frequency: tickloop_agent_protocol::TickSubscriptionFrequency::High,
        is_waiting: false,
        next_check: None,
    });
    let mut plain = shell(Uuid::from_u128(2), "Plain Tick");
    plain.group = TickGroup::Ticks;
    plain.glyph = Glyph::ProtocolTick;
    state
        .monitor
        .replace(TickGroup::Ticks, vec![subscribed, plain]);
    let frame = state.monitor.frame(48, 15, state.display, 0);
    let screen = frame.lines.iter().map(RoleLine::text).collect::<Vec<_>>();
    println!(
        "{}",
        screen
            .iter()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let find = |text: &str| screen.iter().find(|line| line.contains(text)).unwrap();
    assert!(find("Subscribed Tick").starts_with("↻ ◉ "));
    assert!(find("Plain Tick").starts_with("  ◉ "));
    let scheduled = find("◷");
    assert_eq!(display_width(&scheduled[..scheduled.find('◷').unwrap()]), 0);
    for title in ["Subscribed Tick", "Plain Tick"] {
        let line = find(title);
        assert_eq!(
            display_width(&line[..line.find(title).unwrap()]),
            4,
            "{line}"
        );
    }
    for (text, title) in [
        ("◷", "1m"),
        ("提醒我看天气", "提醒我看天气"),
        ("记录验收证据", "记录验收证据"),
    ] {
        let line = find(text);
        assert_eq!(
            display_width(&line[..line.find(title).unwrap()]),
            2,
            "{line}"
        );
    }
    assert!(find("Pursuing goal").starts_with("Pursuing goal"));
    assert!(find("记录验收证据").starts_with("◎ "));
    assert!(screen.iter().any(|line| line == "Scheduled (1)"));
    assert!(find("Pursuing goal").ends_with("1m"));
}

fn docked_monitor_fixture(task_count: usize, tick_count: usize) -> State {
    let mut state = State::new(true, 140, 30);
    let chat = Uuid::from_u128(707);
    state.chat = Some(chat);
    state.prompt_enabled = true;
    let mut snapshot = chat_snapshot(chat);
    snapshot.scheduled_tasks = (0..task_count).map(|index| serde_json::from_value(serde_json::json!({
        "id":format!("{index:08x}"), "kind":{"type":"recurring", "cron":"* * * * *"},
        "prompt":format!("Scheduled instruction {}", index + 1), "creator":"user", "time_zone":"Asia/Shanghai",
        "created_at_ms":0, "scheduled_for_ms":60000, "next_fire_at_ms":60000, "jitter_ms":0, "skipped":0
    })).unwrap()).collect();
    snapshot.goal = Some(serde_json::from_value(serde_json::json!({
        "id":Uuid::from_u128(708), "objective":"Finish the acceptance checks", "objective_revision":1,
        "set_by":"user", "status":{"state":"active"}, "tokens_used":12500,
        "time_used_ms":90000, "quiet_turns":0, "created_at_ms":0, "updated_at_ms":0
    })).unwrap());
    state
        .monitor
        .set_chat_status(Arc::new(ChatStatus::of(&snapshot)));
    let entries = (0..tick_count)
        .map(|index| {
            let mut entry = shell(
                Uuid::from_u128(index as u128 + 1),
                &format!("Tick {:02}", index + 1),
            );
            entry.group = TickGroup::Ticks;
            entry.glyph = Glyph::ProtocolTick;
            entry
        })
        .collect();
    state.monitor.replace(TickGroup::Ticks, entries);
    state.monitor.focus(false, None);
    state
}

fn docked_subscribed_monitor_fixture(task_count: usize, tick_count: usize) -> State {
    let mut state = docked_monitor_fixture(task_count, tick_count);
    let mut entries = state.monitor.entries().to_vec();
    entries[0].tone = crate::monitor_rows::StatusTone::Live;
    entries[0].status = "live".into();
    entries[0].subscription = Some(crate::monitor_rows::SubscriptionMark {
        frequency: tickloop_agent_protocol::TickSubscriptionFrequency::High,
        is_waiting: false,
        next_check: None,
    });
    let selected = entries[0].instance.clone();
    state.monitor.replace(TickGroup::Ticks, entries);
    state.monitor.select(&selected);
    state
}

#[test]
fn selected_tick_status_below_goal_render_evidence() {
    let state = docked_subscribed_monitor_fixture(2, 30);
    let frame = state.monitor.frame(48, 20, state.display, 0);
    println!(
        "{}",
        frame
            .lines
            .iter()
            .map(|line| line.text().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(frame.lines.len(), 20);
    assert_eq!(frame.lines[19].text(), "Live · Every 1 min");
    assert!(frame.lines[18].text().starts_with("◎ Finish"));
    assert!(
        frame.lines[frame.tick_rows.clone()]
            .iter()
            .any(|line| line.text().ends_with('┃'))
    );
    assert!(
        frame
            .lines
            .iter()
            .any(|line| line.fill == Some(Role::Selection) && line.text().contains("Tick 01"))
    );
}

#[test]
fn tick_status_replaces_side_padding_below_goal_without_moving_docked_sections() {
    use crate::terminal::monitor::MonitorTarget;
    let mut state = docked_subscribed_monitor_fixture(2, 30);
    let tick = state.monitor.entries()[0].instance.clone();
    for height in [14, 20] {
        state.monitor.select(&tick);
        let content_height = state
            .monitor
            .side_content_height(48, height, state.display, 0);
        assert_eq!(
            content_height, height,
            "status occupies the former padding row"
        );
        let selected = state.monitor.frame(48, content_height, state.display, 0);
        assert_eq!(selected.lines.last().unwrap().text(), "Live · Every 1 min");
        let goal = selected
            .lines
            .iter()
            .position(|line| line.text().starts_with("Pursuing goal"))
            .unwrap();
        let scheduled = selected
            .lines
            .iter()
            .position(|line| line.text() == "Scheduled (2)")
            .unwrap();
        assert!(selected.lines[goal - 1].text().is_empty());
        assert!(selected.lines[scheduled - 1].text().is_empty());
        assert!(
            selected
                .lines
                .iter()
                .any(|line| line.text().contains("Scheduled instruction 2"))
        );
        assert!(selected.targets.iter().all(|(row, _)| *row < height - 1));
        let footer = selected.lines.last().unwrap();
        assert_eq!(footer.spans[0].role, Some(Role::MutedText));
        assert!(!footer.text().contains(['│', '┃']));
        for target in [
            MonitorTarget::ScheduledTask("00000000".into()),
            MonitorTarget::Goal,
        ] {
            state.monitor.select_target(&target);
            let content_height = state
                .monitor
                .side_content_height(48, height, state.display, 0);
            assert_eq!(content_height, height - 1);
            let frame = state.monitor.frame(48, content_height, state.display, 0);
            assert_eq!(
                frame
                    .lines
                    .iter()
                    .position(|line| line.text().starts_with("Pursuing goal")),
                Some(goal)
            );
            assert_eq!(
                frame
                    .lines
                    .iter()
                    .position(|line| line.text() == "Scheduled (2)"),
                Some(scheduled)
            );
            assert!(
                !frame
                    .lines
                    .iter()
                    .any(|line| line.text().contains("Scheduled task ")
                        || line.text().contains("Every 1 min"))
            );
        }
    }
}

#[test]
fn selected_tick_status_is_immediately_above_the_input_divider_in_side_and_overlay_views() {
    for width in [60, 80, 140] {
        let mut state = docked_subscribed_monitor_fixture(2, 30);
        state.resize(width, 30);
        state.monitor.overlay = width == 60;
        state.task_title = Some("Chat title".into());
        let mut output = Vec::new();
        state.draw_frame(&mut output).unwrap();
        let terminal = tickloop_agent_test_support::screen::visible_screen(
            &String::from_utf8_lossy(&output),
            state.width,
            state.height,
        );
        let rows = terminal.lines().collect::<Vec<_>>();
        let divider = state.monitor_toggle_target.as_ref().unwrap().row;
        assert!(
            rows[divider - 1].contains("Live · Every 1 min"),
            "{width}: {terminal}"
        );
        assert!(
            rows[divider - 2].contains("◎ Finish"),
            "{width}: {terminal}"
        );
        assert!(
            rows[divider - 3].contains("Pursuing goal"),
            "{width}: {terminal}"
        );
        assert!(rows[divider].contains("Chat title"), "{width}: {terminal}");
    }
}

#[test]
fn tick_status_keeps_existing_compression_priorities_at_narrow_heights() {
    let state = docked_subscribed_monitor_fixture(5, 30);
    for height in [4, 5, 6, 7, 8, 10, 12, 16] {
        let frame = state.monitor.frame(30, height, state.display, 0);
        assert_eq!(frame.lines.len(), height);
        let footer = frame.lines.last().unwrap().text() == "Live · Every 1 min";
        let goal_end = height - usize::from(footer);
        assert!(
            frame.lines[goal_end - 2]
                .text()
                .starts_with("Pursuing goal")
        );
        assert!(frame.lines[goal_end - 1].text().starts_with("◎ Finish"));
        assert!(frame.lines.iter().any(|line| line.text().starts_with("◷ ")));
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.text().starts_with("  Scheduled instruction"))
        );
        assert!(
            frame
                .lines
                .iter()
                .all(|line| display_width(&line.text()) <= 30)
        );
        assert_eq!(footer, height >= 8);
    }
}

#[test]
fn scheduled_and_goal_stay_docked_while_ticks_scroll_and_shrink_at_narrow_heights() {
    let mut state = docked_monitor_fixture(5, 0);
    {
        let status = Arc::make_mut(state.monitor.chat_status.as_mut().unwrap());
        for index in 0..130 {
            status.monitor.members.push(MonitorMember {
                is_terminal: false,
                instance: format!("calendar:member-{index:03}").parse().unwrap(),
                title: None,
            });
        }
    }
    state.monitor.rebuild();
    let first_tick = state.monitor.entries()[0].instance.clone();
    state.monitor.select(&first_tick);
    assert_eq!(state.monitor.total(), 136);
    let before = state.monitor.frame(48, 16, state.display, 0);
    for _ in 0..128 {
        state.monitor.move_selection(1);
    }
    assert_eq!(state.monitor.entries().len(), 130);
    let after = state.monitor.frame(48, 16, state.display, 0);
    assert_eq!(
        &before.lines[6..],
        &after.lines[6..],
        "the dock does not move with Tick scrolling"
    );
    let text = after
        .lines
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Scheduled instruction 3"));
    assert!(!text.contains("Scheduled instruction 4"));
    assert!(text.contains("+2 more"));
    assert!(text.starts_with("Monitor\n"));
    assert!(!text.contains("of 130"));
    for height in [4, 5, 6, 7, 8, 10, 12, 16] {
        let frame = state.monitor.frame(30, height, state.display, 0);
        assert_eq!(frame.lines.len(), height);
        assert!(frame.lines[height - 2].text().starts_with("Pursuing goal"));
        assert!(frame.lines[height - 1].text().starts_with("◎ Finish"));
        assert!(frame.lines.iter().any(|line| line.text().starts_with("◷ ")));
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.text().starts_with("  Scheduled instruction"))
        );
        assert!(
            frame
                .lines
                .iter()
                .all(|line| display_width(&line.text()) <= 30)
        );
    }
}

#[test]
fn more_scheduled_opens_a_scrollable_list_and_details_return_to_that_list() {
    use crate::terminal::monitor::MonitorTarget;
    let mut state = docked_monitor_fixture(5, 1);
    let first_tick = state.monitor.entries()[0].instance.clone();
    state.monitor.select(&first_tick);
    for _ in 0..3 {
        state.monitor.move_selection(1);
    }
    assert_eq!(state.monitor.selected_task().unwrap().id, "00000002");
    state.monitor.move_selection(1);
    assert!(state.monitor.is_more_selected());
    let frame = state.monitor.frame(48, 16, state.display, 0);
    let more = frame
        .lines
        .iter()
        .find(|line| line.text() == "+2 more")
        .unwrap();
    assert_eq!(more.fill, Some(Role::Selection));
    assert_eq!(more.spans[0].role, Some(Role::MutedText));
    assert!(
        frame
            .targets
            .iter()
            .any(|(_, target)| *target == MonitorTarget::MoreScheduled)
    );
    state.monitor.move_selection(1);
    assert!(state.monitor.is_goal_selected());
    state.monitor.move_selection(-1);
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.monitor.task_list_is_open());
    assert_eq!(state.monitor.selected_task().unwrap().id, "00000003");
    state.monitor.move_selection(1);
    let expanded = state.monitor.frame(30, 8, state.display, 0);
    assert!(
        expanded
            .lines
            .iter()
            .any(|line| line.text().contains("Scheduled instruction 5"))
    );
    assert!(
        !expanded
            .lines
            .iter()
            .any(|line| line.text().contains("Scheduled instruction 1"))
    );
    assert!(
        expanded
            .targets
            .iter()
            .all(|(_, target)| matches!(target, MonitorTarget::ScheduledTask(_)))
    );
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::TickAction { .. })
    ));
    assert!(state.monitor.detail.is_some());
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(state.monitor.detail.is_none());
    assert!(state.monitor.task_list_is_open());
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.monitor.task_list_is_open());
    assert!(state.monitor.focused);
    assert!(state.monitor.is_more_selected());
}

#[test]
fn monitor_entry_counts_ticks_schedules_and_goal_and_preserves_the_empty_case() {
    for (tasks, ticks, goal_present, expected) in
        [(0, 0, true, 1), (2, 0, false, 2), (5, 8, true, 14)]
    {
        let mut state = docked_monitor_fixture(tasks, ticks);
        if !goal_present {
            Arc::make_mut(state.monitor.chat_status.as_mut().unwrap()).goal = None;
            state.monitor.rebuild();
        }
        assert_eq!(state.monitor.total(), expected);
        let frame = screen(&mut state);
        assert!(frame.contains(&format!("Monitor {expected}")), "{frame}");
    }
    assert_eq!(Monitor::default().total(), 0);
    assert!(!Monitor::default().has_content());
}

#[test]
fn expanded_scheduled_list_scrolls_with_the_wheel_inside_its_visible_column() {
    let mut state = docked_monitor_fixture(5, 1);
    let first_tick = state.monitor.entries()[0].instance.clone();
    state.monitor.select(&first_tick);
    state.monitor.move_selection(4);
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(state.monitor.task_list_is_open());
    state.draw_frame(&mut Vec::new()).unwrap();
    let target = &state.monitor_row_targets[0];
    let (row, column) = (target.row as u16, target.columns.start as u16);
    let before = state.monitor.selected_task().unwrap().id.clone();
    assert!(state.pointer(
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            row,
            column,
            modifiers: KeyModifiers::NONE
        },
        &mut Vec::new()
    ));
    assert_ne!(state.monitor.selected_task().unwrap().id, before);
    assert!(state.monitor.task_list_is_open());
    let before_blank_scroll = state.monitor.selected_task().unwrap().id.clone();
    let blank_row = state.detail_area().1.saturating_sub(2) as u16;
    assert!(state.pointer(
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            row: blank_row,
            column,
            modifiers: KeyModifiers::NONE
        },
        &mut Vec::new()
    ));
    assert_ne!(
        state.monitor.selected_task().unwrap().id,
        before_blank_scroll
    );
}

#[test]
fn returning_from_the_full_task_list_keeps_selection_when_removal_eliminates_overflow() {
    let mut state = docked_monitor_fixture(4, 1);
    state
        .monitor
        .select_target(&crate::terminal::monitor::MonitorTarget::MoreScheduled);
    state.monitor.open_task_list();
    state.monitor.remove_task("00000003");
    assert!(state.monitor.task_list_is_open());
    assert_eq!(state.monitor.selected_task().unwrap().id, "00000000");
    state.monitor.close_task_list();
    assert!(!state.monitor.task_list_is_open());
    assert_eq!(state.monitor.selected_task().unwrap().id, "00000000");
    let frame = state.monitor.frame(48, 16, state.display, 0);
    assert!(!frame.lines.iter().any(|line| line.text().contains("more")));
}

fn removal_fixture(intent: crate::monitor_rows::TickIntent) -> State {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    let mut entry = shell(Uuid::new_v4(), "News · TSLA.US");
    entry.actions = vec![crate::monitor_rows::MonitorAction {
        key: 'x',
        label: "remove",
        intent,
    }];
    state.monitor.replace(TickGroup::Shells, vec![entry]);
    state.monitor.focus(false, None);
    state
}

#[test]
fn destructive_monitor_intents_require_two_control_x_presses_and_show_a_red_footer() {
    use crate::monitor_rows::TickIntent;
    let instance: tick_protocol::InstanceRef = "hosted:watched".parse().unwrap();
    for (intent, consequence) in [
        (
            TickIntent::RemoveFromMonitor(instance.clone()),
            "remove News · TSLA.US from Monitor",
        ),
        (
            TickIntent::CloseHosted {
                instance: instance.clone(),
                ends_process: true,
            },
            "close News · TSLA.US",
        ),
        (
            TickIntent::CloseHosted {
                instance,
                ends_process: false,
            },
            "remove News · TSLA.US from Monitor",
        ),
        (
            TickIntent::StopShell {
                task: Uuid::new_v4(),
            },
            "stop this Shell",
        ),
        (
            TickIntent::Artifact {
                id: "doc".into(),
                key: 'x',
            },
            "remove News · TSLA.US",
        ),
    ] {
        let mut state = removal_fixture(intent);
        assert!(state.key(remove_key()).is_none());
        let expected = format!("Press Ctrl+X again to {consequence}");
        assert_eq!(
            state.confirmation_hint().as_deref(),
            Some(expected.as_str())
        );
        let (lines, _, _) = state.styled_frame();
        let footer = lines.last().unwrap();
        assert_eq!(footer.text(), expected);
        assert!(
            footer
                .spans
                .iter()
                .all(|span| span.role == Some(Role::Error))
        );
        assert_eq!(state.monitor.entries().len(), 1);
        state.monitor_confirmation.as_mut().unwrap().requested_at -= Duration::from_secs(2);
        assert!(matches!(
            state.key(remove_key()),
            Some(InputEvent::TickAction { .. })
        ));
        assert!(state.confirmation_hint().is_none());
    }
}

#[test]
fn monitor_confirmation_cancels_on_timeout_other_keys_escape_selection_and_disappearance() {
    use crate::monitor_rows::TickIntent;
    let instance: tick_protocol::InstanceRef = "hosted:watched".parse().unwrap();
    for key in [
        KeyCode::Down,
        KeyCode::Char('j'),
        KeyCode::Char('r'),
        KeyCode::Esc,
        KeyCode::Left,
    ] {
        let mut state = removal_fixture(TickIntent::RemoveFromMonitor(instance.clone()));
        state.key(remove_key());
        state.key(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(state.monitor_confirmation.is_none(), "{key:?}");
        assert!(state.confirmation_hint().is_none());
    }
    let mut state = removal_fixture(TickIntent::RemoveFromMonitor(instance.clone()));
    state.key(remove_key());
    state.monitor_confirmation.as_mut().unwrap().requested_at -=
        MONITOR_CONFIRMATION_WINDOW + Duration::from_millis(1);
    assert!(state.confirmation_hint().is_none());
    assert!(state.expire_key_confirmations());
    assert!(!state.expire_key_confirmations());
    assert!(
        state.key(remove_key()).is_none(),
        "an expired second press starts over"
    );
    state.monitor.replace(TickGroup::Shells, Vec::new());
    assert!(state.confirmation_hint().is_none(), "the target vanished");
    assert!(state.expire_key_confirmations());
    assert!(!state.monitor.focused, "an empty Monitor releases focus");
    let mut state = docked_monitor_fixture(0, 0);
    assert!(state.monitor.is_goal_selected());
    assert!(state.key(remove_key()).is_none());
    assert_eq!(
        state.confirmation_hint().as_deref(),
        Some("No removable Monitor item selected")
    );
    state.monitor_notice.as_mut().unwrap().until = Instant::now() - Duration::from_millis(1);
    assert!(state.expire_key_confirmations());
    assert!(state.confirmation_hint().is_none());
}

#[test]
fn control_x_confirmation_follows_row_identity_and_plain_x_cannot_remove() {
    use crate::monitor_rows::TickIntent;
    let mut state = removal_fixture(TickIntent::RemoveFromMonitor(
        "hosted:first".parse().unwrap(),
    ));
    state.key(remove_key());
    let second = shell(Uuid::new_v4(), "Second");
    state
        .monitor
        .replace(TickGroup::Shells, vec![second.clone()]);
    assert!(state.confirmation_hint().is_none());
    assert!(
        state.key(remove_key()).is_none(),
        "a new row needs its own first press"
    );
    assert!(state.confirmation_hint().unwrap().contains("Second"));
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.monitor_confirmation.is_none());
    assert_eq!(state.monitor.entries().len(), 1);
    assert_eq!(state.editor.draft(), "x");
}

#[test]
fn monitor_control_x_does_not_start_or_complete_the_foreground_shell_chord() {
    use crate::monitor_rows::TickIntent;
    let mut state = removal_fixture(TickIntent::RemoveFromMonitor(
        "hosted:first".parse().unwrap(),
    ));
    state.set_foreground_shell(true);
    state.monitor.focus(false, None);
    assert!(state.key(remove_key()).is_none());
    assert!(state.shell_background_chord_started_at.is_none());
    assert!(state.monitor_confirmation.is_some());
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .is_none()
    );
    assert!(state.monitor_confirmation.is_none());
    state.monitor.focused = false;
    assert!(state.key(remove_key()).is_none());
    assert!(state.monitor_confirmation.is_none());
    assert!(state.shell_background_chord_started_at.is_some());
    state.monitor.focus(false, None);
    assert!(
        state.key(remove_key()).is_none(),
        "Monitor takes focus from the chord"
    );
    assert!(state.shell_background_chord_started_at.is_none());
    assert!(state.monitor_confirmation.is_some());
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    state.key(remove_key());
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        Some(InputEvent::BackgroundForegroundShell)
    ));
}

#[test]
fn scheduled_task_deletion_from_list_and_detail_requires_the_same_double_press() {
    for detail in [false, true] {
        let mut state = docked_monitor_fixture(1, 0);
        state.monitor.focus(false, None);
        if detail {
            assert!(matches!(
                state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Some(InputEvent::TickAction { .. })
            ));
        }
        let id = state.monitor.selected_task().unwrap().id.clone();
        assert!(state.key(remove_key()).is_none());
        assert_eq!(
            state.confirmation_hint().as_deref(),
            Some("Press Ctrl+X again to delete this scheduled task")
        );
        assert_eq!(state.monitor.selected_task().unwrap().id, id);
        assert!(
            matches!(state.key(remove_key()), Some(InputEvent::TickAction { action, .. }) if action.instance == id && action.key == 'x')
        );
    }
}

#[test]
fn monitor_confirmation_names_the_subscriptions_it_will_cancel() {
    let mut state = subscribed_fixture(2);
    state.key(remove_key());
    assert!(
        state
            .confirmation_hint()
            .unwrap()
            .ends_with("and cancel 2 subscriptions")
    );
    assert!(state.monitor.detail.is_none());
}

fn auto_reveal_sources(chat: Uuid, content: &str) -> crate::terminal::monitor::MonitorSources {
    let mut snapshot = chat_snapshot(chat);
    let mut shells = tests::task_snapshot(chat, 0);
    let mut subscriptions = Vec::new();
    match content {
        "goal" => {
            snapshot.goal = Some(
                serde_json::from_value(serde_json::json!({
                    "id": Uuid::new_v4(), "objective": "Verify Monitor behavior",
                    "objective_revision": 1, "set_by": "user", "status": {"state": "active"},
                    "tokens_used": 0, "time_used_ms": 0, "quiet_turns": 0,
                    "created_at_ms": 0, "updated_at_ms": 0
                }))
                .unwrap(),
            );
        }
        "scheduled" => {
            snapshot.scheduled_tasks = serde_json::from_value(serde_json::json!([{
                "id": "0123abcd", "kind": {"type": "recurring", "cron": "* * * * *"},
                "prompt": "check progress", "creator": "user", "time_zone": "Asia/Shanghai",
                "created_at_ms": 0, "next_fire_at_ms": 60000, "scheduled_for_ms": 60000,
                "jitter_ms": 0, "skipped": 0
            }]))
            .unwrap();
        }
        "hosted" | "terminal" => snapshot.monitor.members.push(MonitorMember {
            instance: tick_protocol::InstanceRef::hosted(Uuid::new_v4()),
            is_terminal: content == "terminal",
            title: Some("New Tick".into()),
        }),
        "artifact" => snapshot
            .artifact_ticks
            .push(tickloop_agent_protocol::ArtifactTick {
                artifact_id: "doc".into(),
                origin: "https://docs.example".into(),
                title: "New Artifact".into(),
                format: "markdown".into(),
                url: "https://docs.example/a/doc".into(),
                access_role: "owner".into(),
                added_at_ms: 1,
            }),
        "shell" => {
            shells = tests::task_snapshot(chat, 1);
            shells.tasks[0].state = ShellTaskState::Background;
            shells.tasks[0].was_background = true;
        }
        "subscription" => subscriptions.push(tests::subscription_row(1)),
        "empty" => {}
        _ => panic!("unknown Monitor fixture: {content}"),
    }
    crate::terminal::monitor::MonitorSources {
        chat: Arc::new(ChatStatus::of(&snapshot)),
        shells,
        subscriptions: Some(subscriptions),
    }
}

#[test]
fn first_new_monitor_content_reveals_once_without_taking_input_focus_even_when_hidden() {
    use tickloop_agent_config::MonitorPreference;
    for preference in [MonitorPreference::Auto, MonitorPreference::Hidden] {
        for content in [
            "goal",
            "scheduled",
            "hosted",
            "terminal",
            "artifact",
            "shell",
            "subscription",
        ] {
            let chat = Uuid::new_v4();
            let console = Console::test_interactive_for_chat(chat);
            console.set_monitor_preference(preference).unwrap();
            console
                .refresh_monitor(auto_reveal_sources(chat, "empty"))
                .unwrap();
            {
                let mut state = console.state.lock().unwrap();
                state.editor.replace_draft("keep typing".into(), 4);
                assert!(!state.monitor_visible());
            }
            console
                .refresh_monitor(auto_reveal_sources(chat, content))
                .unwrap();
            {
                let mut state = console.state.lock().unwrap();
                assert!(state.monitor_visible(), "{preference:?}: {content}");
                assert!(!state.monitor.overlay);
                assert!(!state.monitor.focused);
                assert_eq!(state.monitor_preference, preference);
                assert_eq!(state.editor.draft(), "keep typing");
                assert_eq!(state.editor.cursor(), 4);
                state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
                assert_eq!(state.editor.draft(), "keepx typing");
            }
            console
                .set_monitor_preference(MonitorPreference::Hidden)
                .unwrap();
            console
                .refresh_monitor(auto_reveal_sources(chat, "empty"))
                .unwrap();
            console
                .refresh_monitor(auto_reveal_sources(chat, "goal"))
                .unwrap();
            let state = console.state.lock().unwrap();
            assert!(!state.monitor_visible(), "must stay hidden after {content}");
            assert!(state.monitor.entry_flash_at_ms.is_none());
        }
    }
}

#[test]
fn narrow_terminal_flashes_monitor_entry_once_without_overlay_or_focus_change() {
    for preference in [
        tickloop_agent_config::MonitorPreference::Auto,
        tickloop_agent_config::MonitorPreference::Hidden,
    ] {
        let chat = Uuid::new_v4();
        let console = Console::test_interactive_for_chat(chat);
        console.state.lock().unwrap().width = 60;
        console.set_monitor_preference(preference).unwrap();
        console
            .refresh_monitor(auto_reveal_sources(chat, "empty"))
            .unwrap();
        console
            .refresh_monitor(auto_reveal_sources(chat, "goal"))
            .unwrap();
        let since = {
            let mut state = console.state.lock().unwrap();
            assert!(!state.monitor_visible());
            assert!(!state.monitor.overlay);
            assert!(!state.monitor.focused);
            let since = state.monitor.entry_flash_at_ms.unwrap();
            assert!(state.next_row_clock(since).is_some());
            let (lines, _, _) = state.styled_frame();
            assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
                span.text == "◉ Monitor 1" && span.role == Some(Role::Permission)
            }));
            state.monitor.entry_flash_at_ms = Some(since.saturating_sub(10_000));
            let (lines, _, _) = state.styled_frame();
            assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
                span.text == "◉ Monitor 1" && span.role == Some(Role::MutedText)
            }));
            state.monitor.entry_flash_at_ms = Some(since);
            since
        };
        console
            .refresh_monitor(auto_reveal_sources(chat, "scheduled"))
            .unwrap();
        let state = console.state.lock().unwrap();
        assert_eq!(state.monitor.entry_flash_at_ms, Some(since));
        assert!(!state.monitor_visible());
        assert!(!state.monitor.overlay);
    }
}

#[test]
fn existing_monitor_content_is_a_baseline_and_later_additions_can_reveal_it() {
    for content in [
        "goal",
        "scheduled",
        "hosted",
        "terminal",
        "artifact",
        "shell",
        "subscription",
    ] {
        let chat = Uuid::new_v4();
        let console = Console::test_interactive_for_chat(chat);
        console
            .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
            .unwrap();
        console
            .refresh_monitor(auto_reveal_sources(chat, content))
            .unwrap();
        {
            let state = console.state.lock().unwrap();
            assert!(
                !state.monitor_visible(),
                "initial {content} must stay hidden"
            );
            assert!(state.monitor.entry_flash_at_ms.is_none());
        }
        let mut sources = auto_reveal_sources(chat, "scheduled");
        sources.chat = console
            .state
            .lock()
            .unwrap()
            .monitor
            .chat_status
            .clone()
            .unwrap();
        Arc::make_mut(&mut sources.chat)
            .monitor
            .members
            .push(MonitorMember {
                instance: tick_protocol::InstanceRef::hosted(Uuid::new_v4()),
                is_terminal: false,
                title: Some("Later Tick".into()),
            });
        console.refresh_monitor(sources).unwrap();
        assert!(
            console.monitor_is_visible().unwrap(),
            "new Tick after {content}"
        );
    }
}

#[test]
fn monitor_reveal_budget_survives_chat_switches_and_is_independent_for_each_chat() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(first);
    console
        .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
        .unwrap();
    console
        .refresh_monitor(auto_reveal_sources(first, "empty"))
        .unwrap();
    console
        .refresh_monitor(auto_reveal_sources(first, "goal"))
        .unwrap();
    console
        .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
        .unwrap();
    console.state.lock().unwrap().switch_view_chat(Some(second));
    console
        .refresh_monitor(auto_reveal_sources(second, "empty"))
        .unwrap();
    console
        .refresh_monitor(auto_reveal_sources(second, "scheduled"))
        .unwrap();
    assert!(console.monitor_is_visible().unwrap());
    console.state.lock().unwrap().switch_view_chat(Some(first));
    console
        .refresh_monitor(auto_reveal_sources(first, "hosted"))
        .unwrap();
    assert!(!console.monitor_is_visible().unwrap());
}

#[test]
fn backgrounding_a_foreground_shell_reveals_monitor_on_tick_refresh() {
    let chat = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    console
        .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
        .unwrap();
    let mut sources = auto_reveal_sources(chat, "empty");
    sources.shells = tests::task_snapshot(chat, 1);
    console.refresh_monitor(sources).unwrap();
    let mut shells = tests::task_snapshot(chat, 1);
    shells.tasks[0].state = ShellTaskState::Background;
    console.refresh_monitor_ticks(shells, None).unwrap();
    let state = console.state.lock().unwrap();
    assert!(state.monitor_visible());
    assert!(!state.monitor.focused);
    assert!(!state.monitor.overlay);
}

#[test]
fn subscribing_to_existing_member_and_updating_existing_goal_do_not_reveal_monitor() {
    let chat = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    console
        .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
        .unwrap();
    let mut sources = auto_reveal_sources(chat, "goal");
    let row = tests::subscription_row(1);
    Arc::make_mut(&mut sources.chat)
        .monitor
        .members
        .push(MonitorMember {
            instance: row.instance.clone(),
            is_terminal: false,
            title: None,
        });
    console.refresh_monitor(sources).unwrap();
    console.update_subscribed_ticks(chat, &[row]).unwrap();
    let mut status = console
        .state
        .lock()
        .unwrap()
        .monitor
        .chat_status
        .as_deref()
        .unwrap()
        .clone();
    status.goal.as_mut().unwrap().tokens_used += 1;
    console.update_monitor_chat(status).unwrap();
    assert!(!console.monitor_is_visible().unwrap());
}

#[test]
fn narrow_shell_refresh_draws_the_entry_flash_and_schedules_its_frames() {
    let chat = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    console.state.lock().unwrap().width = 60;
    console
        .refresh_monitor(auto_reveal_sources(chat, "empty"))
        .unwrap();
    let before = console.state.lock().unwrap().frames_drawn;
    let shells = auto_reveal_sources(chat, "shell").shells;
    console.refresh_monitor_ticks(shells, None).unwrap();
    let state = console.state.lock().unwrap();
    assert!(state.frames_drawn > before);
    assert!(state.monitor.entry_flash_at_ms.is_some());
    assert!(state.row_clock_at.is_some());
    assert!(!state.monitor_visible());
    assert!(!state.monitor.overlay);
}

#[test]
fn initial_monitor_sources_arriving_separately_do_not_reveal_existing_content() {
    let chat = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    console
        .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
        .unwrap();
    let sources = auto_reveal_sources(chat, "shell");
    console.update_monitor_chat(sources.chat).unwrap();
    console.refresh_monitor_ticks(sources.shells, None).unwrap();
    console
        .update_subscribed_ticks(chat, &[tests::subscription_row(1)])
        .unwrap();
    assert!(!console.monitor_is_visible().unwrap());
    console
        .update_monitor_chat(auto_reveal_sources(chat, "goal").chat)
        .unwrap();
    assert!(console.monitor_is_visible().unwrap());
}

#[test]
fn monitor_section_spacing_keeps_ticks_scheduled_and_goal_as_peers() {
    let state = docked_monitor_fixture(1, 30);
    let frame = state.monitor.frame(48, 15, state.display, 0);
    let lines = frame.lines.iter().map(RoleLine::text).collect::<Vec<_>>();
    let scheduled = lines
        .iter()
        .position(|line| line == "Scheduled (1)")
        .unwrap();
    let goal = lines
        .iter()
        .position(|line| line.contains("Pursuing goal"))
        .unwrap();
    assert!(lines[scheduled - 1].trim().is_empty(), "{lines:#?}");
    assert!(!lines[scheduled - 2].trim().is_empty(), "{lines:#?}");
    assert!(lines[goal - 1].trim().is_empty(), "{lines:#?}");
    assert!(!lines[goal - 2].trim().is_empty(), "{lines:#?}");
    assert!(lines[goal].starts_with("Pursuing goal"));
    assert!(lines[goal + 1].starts_with("◎ Finish"));
}

#[test]
fn monitor_separator_continues_through_queued_input_to_input_divider() {
    let mut state = docked_monitor_fixture(1, 1);
    state.monitor.focused = false;
    state.activity_interruptible = true;
    state
        .scrollback
        .set_activity(Some(RoleLine::role("• Working", Role::MutedText)));
    state.queued_inputs = vec![tickloop_agent_protocol::QueuedInput {
        id: Uuid::new_v4(),
        chat_id: state.chat.unwrap(),
        loop_id: Some(Uuid::new_v4()),
        kind: tickloop_agent_protocol::QueuedInputKind::Message,
        text: "/loop 每分钟看看 qqq.us 的股价是多少".into(),
        contexts: Vec::new(),
        queued_sequence: 1,
        armed: true,
    }];
    let mut output = Vec::new();
    state.draw_frame(&mut output).unwrap();
    let terminal = tickloop_agent_test_support::screen::visible_screen(
        &String::from_utf8_lossy(&output),
        state.width,
        state.height,
    );
    let queue = state
        .rendered_screen
        .iter()
        .position(|line| line.contains("/loop 每分钟"))
        .unwrap();
    let divider = state
        .rendered_screen
        .iter()
        .enumerate()
        .skip(queue)
        .find(|(_, line)| line.starts_with('─'))
        .unwrap()
        .0;
    let separator = state.width - state.monitor_columns();
    assert!(terminal.contains("ctrl+enter to send now"), "{terminal}");
    let terminal_rows = terminal.lines().collect::<Vec<_>>();
    assert!(terminal_rows[queue].contains("/loop 每分钟"), "{terminal}");
    assert!(
        terminal_rows[divider - 2]
            .split_once('│')
            .unwrap()
            .1
            .contains("◎ Finish"),
        "{terminal}"
    );
    for row in &terminal_rows[queue..divider] {
        assert_eq!(
            row.chars().find(|character| *character == '│'),
            Some('│'),
            "{row}"
        );
        let at = row.find('│').unwrap();
        assert_eq!(display_width(&row[..at]), separator, "{row}");
    }
}

#[test]
fn monitor_removes_section_gaps_before_compressing_scheduled_content() {
    let state = docked_monitor_fixture(3, 30);
    for height in [13, 14, 15] {
        let frame = state.monitor.frame(48, height, state.display, 0);
        let lines = frame.lines.iter().map(RoleLine::text).collect::<Vec<_>>();
        assert_eq!(lines.len(), height);
        for index in 1..=3 {
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains(&format!("Scheduled instruction {index}"))),
                "height {height}: {lines:#?}"
            );
        }
        let goal = lines
            .iter()
            .position(|line| line.starts_with("Pursuing goal"))
            .unwrap();
        if height == 13 {
            assert!(!lines[goal - 1].trim().is_empty());
        }
        if height == 15 {
            assert!(lines[goal - 1].trim().is_empty());
        }
    }
}

#[test]
fn goal_only_monitor_has_no_extra_section_separator() {
    let mut state = docked_monitor_fixture(0, 0);
    state.monitor.focused = false;
    let frame = state.monitor.frame(48, 2, state.display, 0);
    assert_eq!(frame.lines.len(), 2);
    assert!(frame.lines[0].text().starts_with("Pursuing goal"));
    assert!(frame.lines[1].text().starts_with("◎ Finish"));
}

#[test]
fn queued_input_layout_is_bounded_by_conversation_width_before_rendering() {
    let mut state = docked_monitor_fixture(1, 1);
    state.monitor.focused = false;
    state.activity_interruptible = true;
    state
        .scrollback
        .set_activity(Some(RoleLine::role("• Working", Role::MutedText)));
    state.queued_inputs = vec![tickloop_agent_protocol::QueuedInput {
        id: Uuid::new_v4(),
        chat_id: state.chat.unwrap(),
        loop_id: Some(Uuid::new_v4()),
        kind: tickloop_agent_protocol::QueuedInputKind::Message,
        text: format!("/loop {}", "每分钟看看 qqq.us 的股价是多少".repeat(20)),
        contexts: Vec::new(),
        queued_sequence: 1,
        armed: true,
    }];
    let lines = state.visible_pending_input_lines(MAX_PENDING_INPUT_ROWS);
    assert!(
        lines
            .iter()
            .any(|line| line.text().contains("ctrl+enter to send now"))
    );
    let width = state.content_columns();
    assert!(
        lines
            .iter()
            .all(|line| display_width(&line.text()) <= width),
        "width {width}: {lines:?}"
    );
    assert!(lines.iter().any(|line| line.text().ends_with('…')));
    let frame = state.layout_frame();
    assert!(
        frame
            .conversation_footer
            .iter()
            .all(|line| display_width(&line.text()) <= width)
    );
    assert!(
        frame
            .conversation_footer
            .iter()
            .any(|line| line.text().contains("/loop"))
    );
    assert!(!frame.input.iter().any(|line| line.text().contains("/loop")));
}

#[test]
fn prompt_suggestion_layout_stays_inside_the_monitored_conversation() {
    let mut state = docked_monitor_fixture(1, 1);
    state.monitor.focused = false;
    state.prompt_suggestion = Some(format!("suggested-next-step {}", "继续核对结果".repeat(40)));
    assert!(state.visible_prompt_suggestion().is_some());
    let (lines, _, _) = state.normal_styled_frame();
    let suggestion = lines
        .iter()
        .position(|line| line.text().contains("suggested-next-step"))
        .unwrap();
    let divider = lines
        .iter()
        .position(|line| line.text().starts_with('─'))
        .unwrap();
    assert!(suggestion < divider, "{lines:#?}");
    assert!(display_width(&lines[suggestion].text()) <= state.content_columns());
}

#[test]
fn monitored_conversation_footer_and_working_activity_share_the_side_column() {
    for width in [80, 100, 140] {
        for suggestion in [false, true] {
            let mut state = docked_monitor_fixture(1, 1);
            state.resize(width, 30);
            state.monitor.focused = false;
            if suggestion {
                state.prompt_suggestion = Some(format!("suggestion {}", "检查全部结果".repeat(40)));
            } else {
                state.scrollback.set_activity(Some(RoleLine::role(
                    format!("• Working {}", "progress ".repeat(40)),
                    Role::MutedText,
                )));
                state.status_held = true;
                state.status = format!("status-feedback {}", "details ".repeat(40));
            }
            let frame = state.layout_frame();
            assert!(
                frame
                    .conversation_footer
                    .iter()
                    .all(|line| display_width(&line.text()) <= state.content_columns())
            );
            let mut output = Vec::new();
            state.draw_frame(&mut output).unwrap();
            let terminal = tickloop_agent_test_support::screen::visible_screen(
                &String::from_utf8_lossy(&output),
                state.width,
                state.height,
            );
            let rows = terminal.lines().collect::<Vec<_>>();
            let prefix = if suggestion {
                "suggestion "
            } else {
                "status-feedback "
            };
            let content = rows.iter().position(|line| line.contains(prefix)).unwrap();
            let divider = rows
                .iter()
                .enumerate()
                .skip(content)
                .find(|(_, line)| line.starts_with('─'))
                .unwrap()
                .0;
            for row in &rows[content..divider] {
                let (left, _) = row.split_once('│').unwrap();
                assert_eq!(display_width(left), state.content_columns(), "{terminal}");
            }
            assert!(rows[divider - 2].contains("◎ Finish"), "{terminal}");
            assert!(
                rows[divider - 1]
                    .split_once('│')
                    .unwrap()
                    .1
                    .trim()
                    .is_empty(),
                "{terminal}"
            );
            if !suggestion {
                assert!(
                    rows.iter()
                        .any(|line| line.contains("• Working") && line.contains('│')),
                    "{terminal}"
                );
            } else {
                assert!(state.editor.draft().is_empty());
                state.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
                assert!(state.editor.draft().starts_with("suggestion "));
            }
        }
    }
}

#[test]
fn side_monitor_leaves_a_bottom_row_above_the_chat_title_and_yields_it_when_short() {
    for width in [80, 140] {
        for height in [5, 6, 7, 10, 30] {
            let mut state = docked_monitor_fixture(0, 0);
            state.resize(width, height);
            state.monitor.focused = false;
            state.task_title = Some("Chat title".into());
            let mut output = Vec::new();
            state.draw_frame(&mut output).unwrap();
            let terminal = tickloop_agent_test_support::screen::visible_screen(
                &String::from_utf8_lossy(&output),
                state.width,
                state.height,
            );
            let rows = terminal.lines().collect::<Vec<_>>();
            let divider = state.monitor_toggle_target.as_ref().unwrap().row;
            let last = rows[divider - 1].split_once('│').unwrap().1.trim();
            if divider > 3 {
                assert!(last.is_empty(), "{width}x{height}: {terminal}");
                assert!(rows[divider - 2].contains("◎ Finish"), "{terminal}");
            } else {
                assert!(last.starts_with("◎ Finish"), "{width}x{height}: {terminal}");
            }
            assert!(rows[divider].contains("Chat title"), "{terminal}");
        }
    }
}

#[test]
fn side_monitor_bottom_padding_yields_before_section_gaps_and_pinned_content() {
    let mut state = docked_monitor_fixture(3, 1);
    let width = 48;
    let minimum = 15;
    for select_goal in [false, true] {
        if select_goal {
            state
                .monitor
                .select_target(&crate::terminal::monitor::MonitorTarget::Goal);
        }
        for height in [minimum, minimum + 1] {
            let content_height = state
                .monitor
                .side_content_height(width, height, state.display, 0);
            assert_eq!(content_height, minimum);
            let frame = state.monitor.frame(width, content_height, state.display, 0);
            let lines = frame.lines.iter().map(RoleLine::text).collect::<Vec<_>>();
            for index in 1..=3 {
                assert!(
                    lines
                        .iter()
                        .any(|line| line.contains(&format!("Scheduled instruction {index}"))),
                    "{lines:#?}"
                );
            }
            let scheduled = lines
                .iter()
                .position(|line| line == "Scheduled (3)")
                .unwrap();
            let goal = lines
                .iter()
                .position(|line| line.starts_with("Pursuing goal"))
                .unwrap();
            assert!(lines[scheduled - 1].is_empty());
            assert!(lines[goal - 1].is_empty());
            assert!(lines.last().unwrap().starts_with("◎ Finish"));
        }
    }
}

#[test]
fn side_monitor_bottom_padding_preserves_a_wrapped_selected_tick() {
    let mut state = docked_monitor_fixture(0, 1);
    state.monitor.replace(
        TickGroup::Ticks,
        vec![shell(Uuid::new_v4(), &"long selected title ".repeat(20))],
    );
    state.monitor.focus(false, None);
    let tick = state.monitor.entries()[0].instance.clone();
    state.monitor.select(&tick);
    assert!(state.monitor.selected().is_some());
    let minimum = 8;
    for height in [minimum, minimum + 1] {
        let content_height = state
            .monitor
            .side_content_height(48, height, state.display, 0);
        assert_eq!(content_height, height);
        let frame = state.monitor.frame(48, content_height, state.display, 0);
        assert_eq!(frame.lines.last().unwrap().text(), "exit 0");
        assert!(
            frame.lines[content_height - 2]
                .text()
                .starts_with("◎ Finish")
        );
        assert_eq!(
            frame.targets.len(),
            2,
            "both rows of the selected Tick remain visible"
        );
    }
}

#[test]
fn monitor_bottom_padding_does_not_reduce_the_overlay() {
    let mut state = docked_monitor_fixture(0, 0);
    state.resize(60, 20);
    state.monitor.focus(true, None);
    let mut output = Vec::new();
    state.draw_frame(&mut output).unwrap();
    let terminal = tickloop_agent_test_support::screen::visible_screen(
        &String::from_utf8_lossy(&output),
        state.width,
        state.height,
    );
    let rows = terminal.lines().collect::<Vec<_>>();
    let divider = state.monitor_toggle_target.as_ref().unwrap().row;
    assert!(rows[divider - 1].contains("◎ Finish"), "{terminal}");
}

#[test]
fn wheel_scrolls_only_the_tick_viewport_without_moving_selection_or_the_dock() {
    for overlay in [false, true] {
        let mut state = docked_monitor_fixture(2, 40);
        let selected = state.monitor.entries()[0].instance.clone();
        state.monitor.select(&selected);
        if overlay {
            state.width = 70;
            state.monitor.overlay = true;
        }
        state.draw_frame(&mut Vec::new()).unwrap();
        let selected = state.monitor.selected().unwrap().instance.clone();
        let (rows, columns) = state.monitor_tick_area.clone().unwrap();
        let before = state.monitor.frame(40, 16, state.display, 0);
        let wheel = |kind, row| MouseEvent {
            kind,
            row,
            column: columns.start as u16 + 2,
            modifiers: KeyModifiers::NONE,
        };
        assert!(state.pointer(
            wheel(MouseEventKind::ScrollDown, rows.start as u16),
            &mut Vec::new()
        ));
        let after = state.monitor.frame(40, 16, state.display, 0);
        assert_eq!(state.monitor.selected().unwrap().instance, selected);
        assert_ne!(
            before.lines[before.tick_rows.clone()],
            after.lines[after.tick_rows.clone()]
        );
        assert_eq!(
            before.lines[before.tick_rows.end..],
            after.lines[after.tick_rows.end..]
        );
        assert!(state.pointer(
            wheel(MouseEventKind::ScrollUp, rows.start as u16),
            &mut Vec::new()
        ));
        let returned = state.monitor.frame(40, 16, state.display, 0);
        assert_eq!(before.lines, returned.lines);
        state.pointer(
            wheel(MouseEventKind::ScrollDown, rows.end as u16 + 1),
            &mut Vec::new(),
        );
        assert_eq!(
            before.lines,
            state.monitor.frame(40, 16, state.display, 0).lines
        );
        state.pointer(
            wheel(MouseEventKind::ScrollDown, rows.start as u16),
            &mut Vec::new(),
        );
        state.monitor.move_selection(1);
        let resumed = state.monitor.frame(40, 16, state.display, 0);
        let selected = crate::terminal::monitor::MonitorTarget::Tick(
            state.monitor.selected().unwrap().instance.clone(),
        );
        assert!(resumed.targets.iter().any(|(_, id)| id == &selected));
    }
}

fn monitor_entry_span(state: &mut State) -> RoleLine {
    state
        .styled_frame()
        .0
        .into_iter()
        .find(|line| line.text().contains("◉ Monitor "))
        .unwrap()
}

#[test]
fn collapsed_monitor_entry_uses_row_colours_but_expanded_entry_stays_static() {
    for theme in super::super::style::ThemeName::ALL {
        for (elapsed, expected) in [
            (0, Role::TakeInFlashStrong),
            (400, Role::TakeInFlashSoft),
            (800, Role::TakeInFlashStrong),
            (1300, Role::MutedText),
        ] {
            for expanded in [false, true] {
                let mut state = click_fixture(140, false, TickGroup::Shells);
                state.display.set_theme(theme);
                state.monitor.focused = false;
                state.monitor_preference = if expanded {
                    tickloop_agent_config::MonitorPreference::Auto
                } else {
                    tickloop_agent_config::MonitorPreference::Hidden
                };
                let instance = state.monitor.entries()[0].instance.clone();
                let since = tickloop_agent_base::private_fs::unix_time_milliseconds() - elapsed;
                state.monitor.flash_received(instance, since);
                let line = monitor_entry_span(&mut state);
                let entry = line
                    .spans
                    .iter()
                    .find(|span| span.text.contains("◉ Monitor "))
                    .unwrap();
                let role = if expanded { Role::Permission } else { expected };
                let has_background = state.display.theme().style(role).background.is_some();
                assert_eq!(
                    entry.role,
                    Some(if has_background { Role::Text } else { role })
                );
                let resolved = line.resolve(state.display.theme(), state.width);
                let label = resolved
                    .spans
                    .iter()
                    .find(|span| span.text.contains("◉ Monitor "))
                    .unwrap();
                assert_eq!(
                    label.style.background,
                    state
                        .display
                        .theme()
                        .style(if expanded { Role::Permission } else { expected })
                        .background
                );
                if has_background {
                    assert_eq!(
                        label.style.foreground,
                        state.display.theme().style(Role::Text).foreground
                    );
                }
                if !expanded && elapsed < 1200 {
                    assert_eq!(
                        state.next_row_clock(since + elapsed),
                        Some(since + (elapsed / 400 + 1) * 400)
                    );
                }
            }
        }
    }
    let mut state = click_fixture(140, false, TickGroup::Shells);
    state.monitor_preference = tickloop_agent_config::MonitorPreference::Hidden;
    state
        .display
        .set_preferences(tickloop_agent_config::DisplayPreferences {
            prefers_reduced_motion: true,
        });
    let instance = state.monitor.entries()[0].instance.clone();
    state.monitor.flash_received(
        instance,
        tickloop_agent_base::private_fs::unix_time_milliseconds(),
    );
    assert!(
        monitor_entry_span(&mut state)
            .spans
            .iter()
            .any(|span| span.text.contains("◉ Monitor ") && span.role == Some(Role::MutedText))
    );
    assert_eq!(
        state.next_row_clock(tickloop_agent_base::private_fs::unix_time_milliseconds()),
        None
    );
}

#[test]
fn live_tick_and_artifact_content_flash_the_monitor_once_even_before_status_delivery() {
    for artifact in [false, true] {
        let chat = Uuid::new_v4();
        let console = Console::test_interactive_for_chat(chat);
        let mut snapshot = chat_snapshot(chat);
        let instance = if artifact {
            snapshot
                .artifact_ticks
                .push(tickloop_agent_protocol::ArtifactTick {
                    artifact_id: "doc".into(),
                    origin: "https://docs.example".into(),
                    title: "Stream Artifact".into(),
                    format: "markdown".into(),
                    url: "https://docs.example/a/doc".into(),
                    access_role: "owner".into(),
                    added_at_ms: 1,
                });
            tick_protocol::InstanceRef::artifact("https://docs.example", "doc").unwrap()
        } else {
            let instance = tick_protocol::InstanceRef::new("calendar", "stream").unwrap();
            snapshot.monitor.members.push(MonitorMember {
                instance: instance.clone(),
                is_terminal: false,
                title: Some("Stream Tick".into()),
            });
            instance
        };
        console
            .update_monitor_chat(ChatStatus::of(&snapshot))
            .unwrap();
        console
            .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
            .unwrap();
        let message = if artifact {
            tickloop_agent_protocol::LocalMessage::ArtifactRequest {
                artifact_id: "doc".into(),
                request_id: "feedback".into(),
                title: None,
                body: "Review this".into(),
            }
        } else {
            tickloop_agent_protocol::LocalMessage::TickDelivery {
                items: vec![tickloop_agent_protocol::TickDeliveryLine {
                    instance: instance.clone(),
                    title: None,
                    kind: None,
                    seq: None,
                    updates: 1,
                    is_trimmed: false,
                }],
            }
        };
        let input = Uuid::new_v4();
        console
            .record_local_deliveries(&snapshot, input, std::slice::from_ref(&message))
            .unwrap();
        let now = tickloop_agent_base::private_fs::unix_time_milliseconds();
        {
            let mut state = console.state.lock().unwrap();
            assert!(!state.monitor_visible());
            assert_eq!(
                state.monitor.entry_flash_role(state.display, now),
                Some(Role::TakeInFlashStrong)
            );
            assert!(
                state
                    .monitor
                    .frame(48, 12, state.display, now)
                    .lines
                    .iter()
                    .any(|line| line.text().contains("Stream ")
                        && line.fill == Some(Role::TakeInFlashStrong))
            );
            assert!(state.row_clock_at.is_some());
            state.monitor.settle_flashes(now + 2000);
        }
        console
            .record_local_deliveries(&snapshot, input, &[message])
            .unwrap();
        let state = console.state.lock().unwrap();
        assert_eq!(
            state.monitor.entry_flash_role(state.display, now + 2000),
            None,
            "the same content echo never flashes twice"
        );
    }
}

#[test]
fn a_shell_completion_flashes_its_row_and_collapsed_entry_before_the_next_snapshot() {
    let chat = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    let mut shells = tests::task_snapshot(chat, 1);
    shells.tasks[0].was_background = true;
    shells.tasks[0].state = ShellTaskState::Background;
    shells.tasks[0].command = "make test".into();
    let instance = tick_protocol::InstanceRef::shell(shells.tasks[0].id);
    console.update_monitor_chat(chat_status(chat)).unwrap();
    console.refresh_monitor_ticks(shells.clone(), None).unwrap();
    console
        .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
        .unwrap();
    console
        .write_shell_notification(&tickloop_agent_protocol::TaskNotification {
            chat_id: chat,
            task_id: shells.tasks[0].id,
            tool_use_id: None,
            output_file: "output.txt".into(),
            status: tickloop_agent_protocol::TaskNotificationStatus::Completed,
            summary: "make test completed".into(),
        })
        .unwrap();
    let now = tickloop_agent_base::private_fs::unix_time_milliseconds();
    {
        let state = console.state.lock().unwrap();
        assert_eq!(
            state.monitor.entry_flash_role(state.display, now),
            Some(Role::TakeInFlashStrong)
        );
        let row = state
            .monitor
            .frame(48, 12, state.display, now)
            .lines
            .into_iter()
            .find(|line| line.text().contains("make test"))
            .unwrap();
        assert_eq!(row.fill, Some(Role::TakeInFlashStrong));
    }
    shells.tasks[0].state = ShellTaskState::Completed;
    shells.tasks[0].updated_at_ms = Some(now + 1);
    shells.tasks[0].exit_code = Some(0);
    let before = console.state.lock().unwrap().frames_drawn;
    console.refresh_monitor_ticks(shells, None).unwrap();
    let state = console.state.lock().unwrap();
    assert!(
        state.frames_drawn > before,
        "a hidden Monitor still repaints its entry on source refresh"
    );
    assert!(
        state
            .monitor
            .entries()
            .iter()
            .any(|entry| entry.instance == instance)
    );
    assert_eq!(
        state.monitor.entry_flash_role(state.display, now + 1),
        Some(Role::TakeInFlashStrong)
    );
}

#[test]
fn artifact_metadata_updates_flash_live_rows_and_hidden_entries_without_replaying_history() {
    let chat = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    console
        .refresh_monitor(auto_reveal_sources(chat, "artifact"))
        .unwrap();
    console
        .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
        .unwrap();
    let instance = tick_protocol::InstanceRef::artifact("https://docs.example", "doc").unwrap();
    let metadata = |updated: &str| {
        serde_json::from_value(serde_json::json!({
        "id":"doc", "title":"Edited Artifact", "format":"markdown", "access_role":"owner", "human_updated_at":updated
    })).unwrap()
    };
    console
        .update_artifact_metadata(
            chat,
            instance.to_string(),
            Some(metadata("2020-01-01T00:00:00Z")),
        )
        .unwrap();
    let now = tickloop_agent_base::private_fs::unix_time_milliseconds();
    assert_eq!(
        console
            .state
            .lock()
            .unwrap()
            .monitor
            .entry_flash_role(DisplayProfile::default(), now),
        None
    );
    let before = console.state.lock().unwrap().frames_drawn;
    console.artifact_metadata_sink().update(
        chat,
        instance.to_string(),
        Some(metadata("2099-01-01T00:00:00Z")),
    );
    let now = tickloop_agent_base::private_fs::unix_time_milliseconds();
    let first_frame;
    {
        let state = console.state.lock().unwrap();
        assert!(state.frames_drawn > before);
        assert_eq!(
            state.monitor.entry_flash_role(state.display, now),
            Some(Role::TakeInFlashStrong)
        );
        let row = state
            .monitor
            .frame(48, 12, state.display, now)
            .lines
            .into_iter()
            .find(|line| line.text().contains("Edited Artifact"))
            .unwrap();
        assert_eq!(row.fill, Some(Role::TakeInFlashStrong));
        first_frame = state.monitor.next_entry_flash_ms(state.display, now);
    }
    console.artifact_metadata_sink().update(
        chat,
        instance.to_string(),
        Some(metadata("2099-01-01T00:00:00Z")),
    );
    let state = console.state.lock().unwrap();
    assert_eq!(
        state.monitor.next_entry_flash_ms(state.display, now),
        first_frame,
        "polling unchanged content does not restart a flash"
    );
}

#[test]
fn consumed_scheduled_and_goal_inputs_flash_rows_and_entries_but_history_does_not() {
    use tickloop_agent_protocol::{Content, Message, Role as MessageRole, TriggerIdentity};
    for goal in [false, true] {
        let chat = Uuid::new_v4();
        let source = auto_reveal_sources(chat, if goal { "goal" } else { "scheduled" });
        let mut snapshot = chat_snapshot(chat);
        snapshot.goal = source.chat.goal.clone();
        snapshot.scheduled_tasks = source.chat.scheduled_tasks.clone();
        let input = Uuid::new_v4();
        snapshot.trigger_origins.insert(
            input,
            if goal {
                TriggerIdentity::GoalContinuation {
                    goal_id: snapshot.goal.as_ref().unwrap().id,
                    after_run: Uuid::new_v4(),
                }
            } else {
                TriggerIdentity::ScheduledTask {
                    task_id: "0123abcd".into(),
                    scheduled_for_ms: 1,
                    wrappers: Vec::new(),
                }
            },
        );
        snapshot.messages.push(Message {
            role: MessageRole::User,
            content: vec![Content::Text {
                text: if goal {
                    "Continue working toward the active goal of this Chat.".into()
                } else {
                    "Scheduled task 0123abcd: check progress".into()
                },
            }],
        });
        snapshot.message_runs.insert(0, input);
        for history in [false, true] {
            let console = Console::test_interactive_for_chat(chat);
            console
                .refresh_monitor(auto_reveal_sources(chat, "empty"))
                .unwrap();
            console
                .update_monitor_chat(ChatStatus::of(&snapshot))
                .unwrap();
            console
                .set_monitor_preference(tickloop_agent_config::MonitorPreference::Hidden)
                .unwrap();
            if history {
                super::super::workflows::write_public_history(&console, &snapshot).unwrap();
            }
            super::super::consume_input_echo_from_snapshot(&console, &snapshot, input).unwrap();
            let now = tickloop_agent_base::private_fs::unix_time_milliseconds();
            let mut state = console.state.lock().unwrap();
            assert_eq!(
                state.monitor.entry_flash_role(state.display, now),
                if history {
                    None
                } else {
                    Some(Role::TakeInFlashStrong)
                }
            );
            if !history {
                let frame = state.monitor.frame(48, 12, state.display, now);
                let text = if goal {
                    "◎ Verify Monitor behavior"
                } else {
                    "check progress"
                };
                assert!(
                    frame
                        .lines
                        .iter()
                        .any(|line| line.text().starts_with(text)
                            || line.text().trim_start() == text)
                );
                assert!(
                    frame
                        .lines
                        .iter()
                        .any(|line| line.fill == Some(Role::TakeInFlashStrong))
                );
                state.monitor.settle_flashes(now + 2000);
            }
            drop(state);
            super::super::consume_input_echo_from_snapshot(&console, &snapshot, input).unwrap();
            let state = console.state.lock().unwrap();
            assert_eq!(
                state.monitor.entry_flash_role(state.display, now + 2000),
                None
            );
        }
    }
}

#[test]
fn collapsed_monitor_flash_render_evidence() {
    use crate::terminal::style::{Colour, Rgb};
    fn colour(value: Option<Colour>) -> Option<String> {
        match value {
            Some(Colour::Rgb(Rgb { red, green, blue })) => {
                Some(format!("#{red:02x}{green:02x}{blue:02x}"))
            }
            _ => None,
        }
    }
    for (phase, elapsed) in [
        ("warm strong", 0),
        ("warm soft", 400),
        ("warm strong again", 800),
        ("settled", 1300),
    ] {
        let mut state = docked_subscribed_monitor_fixture(2, 30);
        state.monitor.focused = false;
        state.monitor_preference = tickloop_agent_config::MonitorPreference::Hidden;
        let instance = state.monitor.entries()[0].instance.clone();
        let since = tickloop_agent_base::private_fs::unix_time_milliseconds() - elapsed;
        state.monitor.flash_received(instance, since);
        let line = monitor_entry_span(&mut state);
        let resolved = line.resolve(state.display.theme(), state.width);
        let spans = resolved.spans.iter().map(|span| serde_json::json!({
            "text":span.text, "foreground":colour(span.style.foreground), "background":colour(span.style.background)
        })).collect::<Vec<_>>();
        let label = resolved
            .spans
            .iter()
            .find(|span| span.text.contains("◉ Monitor "))
            .unwrap();
        let expected = if elapsed == 1300 {
            Role::MutedText
        } else if elapsed == 400 {
            Role::TakeInFlashSoft
        } else {
            Role::TakeInFlashStrong
        };
        assert_eq!(
            label.style.background,
            state.display.theme().style(expected).background
        );
        println!("{phase}: {}", line.text());
        println!(
            "MONITOR_RENDER_JSON {}",
            serde_json::json!({"phase":phase,"spans":spans})
        );
    }
}
