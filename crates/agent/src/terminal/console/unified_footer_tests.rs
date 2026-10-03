use super::*;
use crate::agent_console::AttentionGroup;
use tickloop_agent::runtime::{
    ShellTask, ShellTaskAction, ShellTaskCapabilities, ShellTaskOrigin, ShellTaskOwner,
    ShellTaskState,
};

const CHAT: Uuid = Uuid::from_u128(1010);
const TASK: Uuid = Uuid::from_u128(1011);

fn fixture() -> Console {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.chat = Some(CHAT);
    state.insert("keep literal jk draft");
    tests::console_with_state(state)
}

fn item(id: &str, content: &str, task_id: Option<Uuid>) -> StatusItem {
    StatusItem {
        id: id.into(),
        task_id,
        can_stop: task_id.is_some(),
        title: content.into(),
        state: "in progress",
        role: Role::Warning,
    }
}

fn frame(console: &Console) -> Vec<String> {
    let mut state = console.state.lock().unwrap();
    state.draw(&mut io::sink()).unwrap();
    state.rendered_screen.clone()
}

fn key(console: &Console, code: KeyCode) -> Option<InputEvent> {
    console
        .state
        .lock()
        .unwrap()
        .key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn update(console: &Console, tasks: Vec<StatusItem>, todos: Vec<StatusItem>) {
    console
        .update_status_area(
            "Auto".into(),
            "fixture".into(),
            Some((42_000, 200_000)),
            tasks,
            todos,
        )
        .unwrap();
}

fn agent(id: u128, parent: Option<Uuid>, title: &str) -> AgentTreeRow {
    AgentTreeRow {
        agent_id: Uuid::from_u128(id),
        parent_id: parent,
        depth: usize::from(parent.is_some()),
        title: title.into(),
        status: "running".into(),
        project: "fixture".into(),
        workspace: None,
        approvals: 0,
        questions: 0,
        group: AttentionGroup::Working,
        glyph: status_glyph(AttentionGroup::Working, false),
        preview: None,
        updated_at: None,
    }
}

#[test]
fn automatic_priority_and_manual_selection_survive_updates_without_editor_focus_theft() {
    let console = fixture();
    let shell = item("1.", "printf selected-shell", Some(TASK));
    update(&console, vec![shell.clone()], vec![]);
    assert!(
        !frame(&console)
            .iter()
            .any(|row| row.contains("selected-shell"))
    );
    console
        .set_agent_family(
            vec![
                agent(1, None, "Main · root"),
                agent(2, Some(Uuid::from_u128(1)), "child unique"),
            ],
            Some(Uuid::from_u128(1)),
        )
        .unwrap();
    let agents = frame(&console);
    assert!(
        agents.iter().any(|row| row.contains("child unique")),
        "{agents:?}"
    );
    assert!(!agents.iter().any(|row| row.contains("selected-shell")));
    update(
        &console,
        vec![shell.clone()],
        vec![item("1.", "todo unique", None)],
    );
    let todo = frame(&console);
    assert!(todo.iter().any(|row| row.contains("todo unique")));
    assert!(!todo.iter().any(|row| row.contains("child unique")));
    for character in "jk".chars() {
        key(&console, KeyCode::Char(character));
    }
    key(&console, KeyCode::Down);
    key(&console, KeyCode::Right);
    key(&console, KeyCode::Right);
    key(&console, KeyCode::Esc);
    update(
        &console,
        vec![shell],
        vec![item("1.", "newer todo must not take focus", None)],
    );
    let manual = frame(&console);
    assert!(
        manual.iter().any(|row| row.contains("child unique")),
        "{manual:?}"
    );
    assert!(
        !manual
            .iter()
            .any(|row| row.contains("newer todo must not take focus"))
    );
    assert!(
        manual
            .iter()
            .any(|row| row.contains("keep literal jk draftjk"))
    );
    console.set_agent_family(vec![], None).unwrap();
    update(&console, vec![], vec![item("1.", "fallback todo", None)]);
    let fallback = frame(&console);
    assert!(
        fallback.iter().any(|row| row.contains("fallback todo")),
        "{fallback:?}"
    );
    assert!(
        !fallback
            .iter()
            .any(|row| row.contains("Shell 0") || row.contains("Artifacts"))
    );
    assert!(console.state.lock().unwrap().footer.selected().is_none());
}

#[test]
fn todo_detail_is_captured_read_only_and_returns_to_the_preserved_list_and_draft() {
    let console = fixture();
    let content = "Full Todo content contains user hash 0123456789abcdef and a second line\nSecond line stays available in details";
    update(
        &console,
        vec![],
        vec![item("1.", content, None), item("2.", "other item", None)],
    );
    key(&console, KeyCode::Down);
    key(&console, KeyCode::Enter);
    key(&console, KeyCode::Enter);
    let detail = frame(&console);
    assert!(
        detail.iter().any(|row| row.contains("Todo item 1")),
        "{detail:?}"
    );
    assert!(detail.iter().any(|row| row.contains("0123456789abcdef")));
    assert!(
        detail
            .iter()
            .any(|row| row.contains("Second line stays available"))
    );
    assert!(detail.iter().any(|row| row.contains("Read-only")));
    update(
        &console,
        vec![],
        vec![item("1.", "replacement must not reinterpret detail", None)],
    );
    let replaced = frame(&console);
    assert!(
        replaced
            .iter()
            .any(|row| row.contains("Second line stays available"))
    );
    assert!(
        !replaced
            .iter()
            .any(|row| row.contains("replacement must not reinterpret"))
    );
    assert!(key(&console, KeyCode::Enter).is_none());
    assert!(key(&console, KeyCode::Char('x')).is_none());
    key(&console, KeyCode::Esc);
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("replacement must not reinterpret"))
    );
    key(&console, KeyCode::Esc);
    key(&console, KeyCode::Char('j'));
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("keep literal jk draftj"))
    );
}

fn shell_snapshot(revision: u64, tasks: Vec<(Uuid, &str, ShellTaskState)>) -> TaskSurfaceSnapshot {
    TaskSurfaceSnapshot {
        chat_id: CHAT,
        live_revision: revision,
        tasks: tasks
            .into_iter()
            .map(|(id, command, state)| ShellTask {
                updated_at_ms: None,
                id,
                chat_id: CHAT,
                origin: ShellTaskOrigin::User,
                owner: ShellTaskOwner::User { chat_id: CHAT },
                command: command.into(),
                cwd: PathBuf::from("/fixture"),
                state,
                was_background: true,
                pid: None,
                exit_code: None,
                signal: None,
                reason: None,
                output_bytes: 0,
                capabilities: ShellTaskCapabilities {
                    can_background: false,
                    can_stop: !state.is_terminal(),
                    can_read_output: false,
                },
            })
            .collect(),
    }
}

#[test]
fn single_shell_activation_and_detail_updates_keep_the_exact_eligible_task() {
    let console = fixture();
    let snapshot = shell_snapshot(
        1,
        vec![(TASK, "selected shell", ShellTaskState::Background)],
    );
    console.update_shell_ticks(&snapshot).unwrap();
    console
        .state
        .lock()
        .unwrap()
        .key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    let Some(InputEvent::OpenTickShell { task_id, panel, .. }) = key(&console, KeyCode::Enter)
    else {
        panic!("Shell shortcut did not open the Tick panel");
    };
    assert_eq!(task_id, TASK);
    console
        .load_tick_shell(CHAT, panel, snapshot, None)
        .unwrap();
    let mut completed =
        shell_snapshot(2, vec![(TASK, "selected shell", ShellTaskState::Completed)]);
    completed.tasks[0].exit_code = Some(0);
    let effect = console.update_tick_shell(CHAT, panel, completed).unwrap();
    assert!(matches!(
        effect,
        TaskSurfaceEffect::None | TaskSurfaceEffect::ReadOutput { .. }
    ));
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("State: exit 0"))
    );
    key(&console, KeyCode::Esc);
    assert_eq!(
        console.state.lock().unwrap().editor.draft(),
        "keep literal jk draft"
    );
}

#[test]
fn multiple_shell_shortcut_opens_the_list_after_an_earlier_single_detail() {
    let console = fixture();
    let second = Uuid::from_u128(1012);
    console
        .update_shell_ticks(&shell_snapshot(
            1,
            vec![
                (TASK, "first shell", ShellTaskState::Background),
                (second, "second shell", ShellTaskState::Background),
            ],
        ))
        .unwrap();
    console
        .state
        .lock()
        .unwrap()
        .key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    let list = frame(&console);
    assert!(
        list.iter().any(|row| row.contains("Shells (2)")),
        "{list:?}"
    );
    key(&console, KeyCode::Char('j'));
    assert!(
        matches!(key(&console, KeyCode::Enter), Some(InputEvent::OpenTickShell {task_id,..}) if task_id == second)
    );
    assert_eq!(
        console.state.lock().unwrap().editor.draft(),
        "keep literal jk draft"
    );
}

fn assert_single_shell_shortcut(cached_detail: bool) {
    let console = fixture();
    let target = if cached_detail {
        Uuid::from_u128(1013)
    } else {
        TASK
    };
    if cached_detail {
        console
            .show_tasks_at(
                shell_snapshot(1, vec![(TASK, "earlier task", ShellTaskState::Background)]),
                Some(TASK),
            )
            .unwrap();
        key(&console, KeyCode::Esc);
    }
    let snapshot = shell_snapshot(
        2,
        vec![(target, "sole eligible task", ShellTaskState::Background)],
    );
    console.update_shell_ticks(&snapshot).unwrap();
    let event = console
        .state
        .lock()
        .unwrap()
        .key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    assert!(matches!(
        event,
        Some(InputEvent::TickListAction { action: 'r', .. })
    ));
    let Some(InputEvent::OpenTickShell { task_id, panel, .. }) = key(&console, KeyCode::Enter)
    else {
        panic!("Shell shortcut did not open Tick panel");
    };
    assert_eq!(task_id, target);
    console
        .load_tick_shell(CHAT, panel, snapshot, None)
        .unwrap();
    let detail = frame(&console);
    assert!(
        detail.iter().any(|row| row.contains("sole eligible task")),
        "{detail:?}"
    );
    assert!(
        detail.iter().any(|row| row.contains("State: running")),
        "{detail:?}"
    );
    key(&console, KeyCode::Esc);
    assert_eq!(
        console.state.lock().unwrap().editor.draft(),
        "keep literal jk draft"
    );
}

#[test]
fn fresh_single_shell_shortcut_opens_its_exact_detail() {
    assert_single_shell_shortcut(false);
}

#[test]
fn cached_single_shell_shortcut_opens_the_current_exact_eligible_detail() {
    assert_single_shell_shortcut(true);
}

#[tokio::test]
async fn global_shell_count_and_open_keep_the_peer_task_and_stop_its_actual_owner() {
    use crate::agent_access::{EmbeddedAgentAccess, LaunchSpec, ViewRef};
    use crate::terminal::{
        execution::ConsoleExecution, task_access::ShellTaskCenter, test_support,
    };
    use futures_util::FutureExt;
    use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};

    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(test_support::ScriptedModel::new(vec![]));
    let runtime = crate::runtime::Agent::new_embedded(test_support::config(), model.clone());
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let agent = ConsoleExecution::from_access_for_test(
        access.clone(),
        access.owner_epoch(),
        ViewRef {
            session_id: Uuid::new_v4(),
            view_id: Uuid::new_v4(),
        },
        LaunchSpec::embedded(directory.path().into()),
        None,
    );
    let result = AssertUnwindSafe(async {
        let source = agent.create_chat(directory.path().into()).await.unwrap();
        let peer = agent.create_chat(directory.path().into()).await.unwrap();
        agent.display_chat(Some(peer)).await.unwrap();
        let peer_task = agent.submit_user_shell_for_goal(peer, None, "printf 'peer-shell-marker'; while [ ! -f peer-finish ]; do sleep 0.02; done".into(), true).await.unwrap().command_id;
        agent.display_chat(Some(source)).await.unwrap();
        let source_origin = agent.origin().unwrap();
        let center = ShellTaskCenter::new(agent.clone());
        let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = center.snapshot(source).await.unwrap();
                if snapshot.tasks.iter().any(|task| task.id == peer_task && task.was_background && !task.state.is_terminal()) {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert_eq!(snapshot.chat_id, source);
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].chat_id, peer);
        let console = fixture();
        console.state.lock().unwrap().chat = Some(source);
        console.update_shell_ticks(&snapshot).unwrap();
        assert!(console.state.lock().unwrap().tick_list.entries().is_empty());
        let mut subscription = None;
        crate::terminal::open_tasks(&center, &console, source, &mut subscription, Some(peer_task), false).await.unwrap();
        key(&console, KeyCode::Home);
        let detail = frame(&console);
        assert!(detail.iter().any(|row| row.contains("Status:")), "{detail:?}");
        assert!(detail.iter().any(|row| row.contains("peer-shell-marker")), "{detail:?}");
        assert_eq!(agent.origin(), Some(source_origin));
        key(&console, KeyCode::Left);
        let source_task = agent.submit_user_shell_for_goal(source, None, "printf 'source-shell-marker'; while [ ! -f source-finish ]; do sleep 0.02; done".into(), true).await.unwrap().command_id;
        let multiple = center.snapshot(source).await.unwrap();
        assert_eq!(crate::terminal::task_status_items(&multiple).len(), 2);
        let fresh = fixture();
        fresh.state.lock().unwrap().chat = Some(source);
        update(&fresh, crate::terminal::task_status_items(&multiple), vec![]);
        let mut subscription = None;
        crate::terminal::open_tasks(&center, &fresh, source, &mut subscription, None, false).await.unwrap();
        let list = frame(&fresh);
        assert!(list.iter().any(|row| row.contains("Shell")), "{list:?}");
        assert!(list.iter().any(|row| row.contains("peer-shell-marker")), "{list:?}");
        assert!(list.iter().any(|row| row.contains("source-shell-marker")), "{list:?}");
        assert!(!list.iter().any(|row| row.contains("Status:")), "{list:?}");
        let selected_glyph = fresh.display_profile().unwrap().glyph(Glyph::Prompt);
        for _ in 0..2 {
            if frame(&fresh).iter().any(|row| row.trim_start().starts_with(selected_glyph) && row.contains("peer-shell-marker")) {
                break;
            }
            key(&fresh, KeyCode::Char('j'));
        }
        assert!(frame(&fresh).iter().any(|row| row.trim_start().starts_with(selected_glyph) && row.contains("peer-shell-marker")));
        let Some(InputEvent::TaskSurface(effect)) = key(&fresh, KeyCode::Char('x')) else { panic!("selected peer task must expose Stop"); };
        assert!(matches!(effect, TaskSurfaceEffect::Control { task_id, action: ShellTaskAction::Stop } if task_id == peer_task));
        crate::terminal::apply_task_surface_effect(&center, &fresh, source, effect).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if runtime.snapshot(peer).await.unwrap().user_shells.iter().any(|task| task.id == peer_task && matches!(task.state, crate::runtime::UserShellState::Completed | crate::runtime::UserShellState::Failed | crate::runtime::UserShellState::Unknown)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert!(runtime.snapshot(source).await.unwrap().user_shells.iter().any(|task| task.id == source_task && matches!(task.state, crate::runtime::UserShellState::Queued | crate::runtime::UserShellState::Running | crate::runtime::UserShellState::Background)));
        assert!(runtime.snapshot(source).await.unwrap().loops.is_empty());
        assert!(runtime.snapshot(peer).await.unwrap().loops.is_empty());
        assert!(model.requests.lock().unwrap().is_empty());
    }).catch_unwind().await;
    runtime.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn core_status_floats_on_the_lower_divider_with_roles_and_preserved_fallback_notices() {
    let console = fixture();
    update(&console, vec![], vec![]);
    let lines = frame(&console);
    let editor = lines
        .iter()
        .position(|row| row.contains("keep literal jk draft"))
        .unwrap();
    assert!(
        lines[editor + 1].starts_with('─') && lines[editor + 1].ends_with("Context 21% used"),
        "{lines:?}"
    );
    assert_eq!(
        lines.iter().filter(|row| row.contains("⏵⏵ Auto")).count(),
        1
    );
    console
        .set_connection_notice(Some("Disconnected · cached view".into()))
        .unwrap();
    let notices = frame(&console);
    assert!(
        notices
            .iter()
            .any(|row| row.contains("Disconnected · cached view"))
    );
    assert_eq!(
        notices.iter().filter(|row| row.contains("⏵⏵ Auto")).count(),
        1
    );
    let mut state = console.state.lock().unwrap();
    let (styled, _, _) = state.normal_styled_frame();
    let lower = styled
        .iter()
        .find(|line| line.text().contains("Context 21% used"))
        .unwrap();
    assert!(
        lower
            .spans
            .iter()
            .any(|span| span.text.contains("⏵⏵ Auto") && span.role == Some(Role::Warning))
    );
    assert!(
        lower
            .spans
            .iter()
            .any(|span| span.text.contains("Context 21% used") && span.role == Some(Role::Success))
    );
    drop(state);
    console.set_connection_notice(None).unwrap();
    let mut state = console.state.lock().unwrap();
    state.resize(24, 24);
    let narrow = state.frame().0;
    assert!(narrow.iter().any(|row| row.starts_with("⏵⏵ Auto")));
    state.resize(80, 24);
    state.configured_status_lines = Some(vec!["custom user status".into()]);
    let custom = state.frame().0;
    assert!(custom.iter().any(|row| row.contains("custom user status")));
    assert!(
        !custom
            .iter()
            .any(|row| row.starts_with('─') && row.contains("⏵⏵ Auto"))
    );
}

#[test]
fn core_status_position_moves_the_complete_styled_segment_and_preserves_fallback() {
    use tickloop_agent::settings::StatusLinePosition;
    for mode in ["Auto", "Plan", "Ask", "Full Access"] {
        let console = fixture();
        console
            .update_status_area(mode.into(), "模型 fixture".into(), None, vec![], vec![])
            .unwrap();
        let right = frame(&console);
        let editor = right
            .iter()
            .position(|row| row.contains("keep literal jk draft"))
            .unwrap();
        let core = right[editor + 1]
            .trim_start_matches('─')
            .trim_start()
            .to_owned();
        console
            .set_display_preferences(DisplayPreferences {
                status_line_position: StatusLinePosition::Left,
                ..Default::default()
            })
            .unwrap();
        let left = frame(&console);
        assert_eq!(left.len(), right.len());
        assert!(left[editor + 1].starts_with(&core), "{left:?}");
        assert!(left[editor + 1].ends_with('─'));
        assert_eq!(display_width(&left[editor + 1]), 79);
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.editor.draft(), "keep literal jk draft");
        let (styled, _, _) = state.normal_styled_frame();
        let styled_editor = styled
            .iter()
            .position(|line| line.text().contains("keep literal jk draft"))
            .unwrap();
        assert_eq!(styled[styled_editor + 1].fill, None);
        assert!(
            styled[styled_editor + 1]
                .spans
                .iter()
                .any(|span| span.text == "模型 fixture" && span.role == Some(Role::Text))
        );
        for width in [24, display_width(&core) + 1, display_width(&core) + 2] {
            state.resize((width + 1).try_into().unwrap(), 24);
            let left_frame = state.frame().0;
            state.display.set_preferences(DisplayPreferences::default());
            let right_frame = state.frame().0;
            if width == display_width(&core) + 2 {
                assert!(
                    left_frame
                        .iter()
                        .any(|line| line.starts_with(&core) && line.ends_with('─'))
                );
                assert!(
                    right_frame
                        .iter()
                        .any(|line| line.starts_with('─') && line.ends_with(&core))
                );
            } else {
                assert_eq!(left_frame, right_frame);
            }
            state.display.set_preferences(DisplayPreferences {
                status_line_position: StatusLinePosition::Left,
                ..Default::default()
            });
        }
        state.resize(80, 15);
        let short_left = state.frame().0;
        state.display.set_preferences(DisplayPreferences::default());
        assert_eq!(short_left, state.frame().0);
        state.resize(80, 24);
        state.configured_status_lines = Some(vec!["custom user status".into()]);
        let custom_right = state.frame().0;
        state.display.set_preferences(DisplayPreferences {
            status_line_position: StatusLinePosition::Left,
            ..Default::default()
        });
        assert_eq!(custom_right, state.frame().0);
    }
}
