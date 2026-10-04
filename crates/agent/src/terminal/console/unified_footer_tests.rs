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

fn todo(content: &str) -> TodoItem {
    TodoItem {
        content: content.into(),
        status: tickloop_agent::runtime::TodoStatus::InProgress,
        active_form: "Working".into(),
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

fn update(console: &Console, todos: Vec<TodoItem>) {
    console
        .update_status_area(
            "Auto".into(),
            "fixture".into(),
            Some((42_000, 200_000)),
            todos,
        )
        .unwrap();
}

fn agent(id: u128, parent: Option<Uuid>, title: &str) -> AgentRow {
    AgentRow {
        agent_id: Uuid::from_u128(id),
        parent_id: parent,

        title: title.into(),
        status: "running".into(),
        state: crate::runtime::LoopState::Running,
        project: "fixture".into(),
        workspace: None,
        approvals: 0,
        questions: 0,
        group: AttentionGroup::Working,
        glyph: status_glyph(AttentionGroup::Working, false),
        preview: None,
        updated_at: None,
        subagent: parent.map(|_| crate::runtime::SubagentRegistration {
            child_chat_id: Uuid::from_u128(id),
            handle: if title.contains(' ') {
                format!("child-{id}")
            } else {
                title.into()
            },
            description: title.into(),
            created_at: crate::private_fs::unix_time_milliseconds(),
        }),
    }
}

#[test]
fn child_input_badge_placeholder_and_panel_background_are_display_only() {
    use crate::settings::{DisplayPreferences, GlyphSet};
    for glyph_set in [GlyphSet::Unicode, GlyphSet::NerdFont] {
        for theme in ThemeName::ALL {
            let console = fixture();
            update(&console, vec![]);
            console
                .set_agent_family(
                    vec![
                        agent(1, None, "Root title"),
                        agent(2, Some(Uuid::from_u128(1)), "sleep-30-2"),
                    ],
                    Some(Uuid::from_u128(2)),
                )
                .unwrap();
            console.set_visible_delegation(true).unwrap();
            let mut state = console.state.lock().unwrap();
            state.editor.replace_draft(String::new(), 0);
            state.display.set_preferences(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            state.display.set_theme(theme);
            let (lines, _, _) = state.normal_styled_frame();
            let badge = lines
                .iter()
                .find(|line| line.text().contains("@sleep-30-2") && line.text().contains('─'))
                .expect("child handle on upper divider");
            assert!(
                badge
                    .spans
                    .iter()
                    .any(|span| span.text.contains("@sleep-30-2")
                        && span.role
                            == Some(super::super::subagent_panel::agent_color(Uuid::from_u128(
                                2
                            ))))
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.text().contains("Message @sleep-30-2…"))
            );
            assert!(state.editor.draft().is_empty());
            assert_eq!(state.input_display().0, "");
            assert!(
                lines
                    .iter()
                    .filter(
                        |line| line.text().contains("sleep-30-2") || line.text().contains(" main")
                    )
                    .all(|line| line.fill.is_none())
            );
        }
    }
}

#[test]
fn subagent_count_excludes_main_and_includes_the_child_currently_viewed() {
    let console = fixture();
    update(&console, vec![]);
    console
        .set_agent_family(
            vec![
                agent(1, None, "Main count probe"),
                agent(2, Some(Uuid::from_u128(1)), "viewed child count probe"),
                agent(3, Some(Uuid::from_u128(2)), "nested child count probe"),
            ],
            Some(Uuid::from_u128(2)),
        )
        .unwrap();
    let rendered = frame(&console);
    assert!(
        rendered.iter().any(|row| row.contains("Subagents 2")),
        "{rendered:?}"
    );
    for title in [
        "main",
        "viewed child count probe",
        "nested child count probe",
    ] {
        assert!(
            rendered.iter().any(|row| row.contains(title)),
            "{rendered:?}"
        );
    }
    key(&console, KeyCode::Down);
    key(&console, KeyCode::Down);
    console
        .set_agent_family(
            vec![agent(1, None, "Main count probe")],
            Some(Uuid::from_u128(1)),
        )
        .unwrap();
    assert!(!frame(&console).iter().any(|row| row.contains("Subagents")));
}

#[test]
fn expanded_subagents_start_at_the_entry_without_an_items_or_blank_summary_row() {
    let console = fixture();
    update(&console, vec![]);
    console
        .set_agent_family(
            vec![
                agent(1, None, "Main summary probe"),
                agent(2, Some(Uuid::from_u128(1)), "child summary probe"),
            ],
            Some(Uuid::from_u128(1)),
        )
        .unwrap();
    for width in [80, 48] {
        console.state.lock().unwrap().resize(width, 24);
        let rendered = frame(&console);
        let entry = rendered
            .iter()
            .position(|row| row.contains("Subagents 1"))
            .unwrap();
        assert!(rendered[entry + 1].contains("main"), "{rendered:?}");
        assert!(
            rendered[entry + 2].contains("child summary probe"),
            "{rendered:?}"
        );
        key(&console, KeyCode::Down);
        let focused = frame(&console);
        let entry = focused
            .iter()
            .position(|row| row.contains("Subagents 1"))
            .unwrap();
        assert!(focused[entry + 1].contains("main"), "{focused:?}");
        {
            assert!(
                focused.last().unwrap().contains("Enter/Space hide"),
                "{focused:?}"
            );
            key(&console, KeyCode::Down);
            let list_focused = frame(&console);
            assert!(
                list_focused
                    .last()
                    .unwrap()
                    .contains("Enter open · Space fold · Esc entry"),
                "{list_focused:?}"
            );
            key(&console, KeyCode::Esc);
            key(&console, KeyCode::Enter);
            let collapsed = frame(&console);
            assert!(
                collapsed.last().unwrap().contains("Enter/Space expand"),
                "{collapsed:?}"
            );
            key(&console, KeyCode::Enter);
        }
        key(&console, KeyCode::Esc);
        assert!(
            !rendered.iter().any(|row| row.contains("items")
                || row.contains("↓ open")
                || row.contains("Enter list")),
            "{rendered:?}"
        );
    }
}

#[test]
fn subagents_robot_prefix_preserves_divider_bounds_and_hints_in_both_states_and_glyph_sets() {
    use tickloop_agent::settings::GlyphSet;

    for (glyph_set, robot) in [(GlyphSet::Unicode, "⏺"), (GlyphSet::NerdFont, "\u{f06a9}")] {
        let console = fixture();
        console
            .set_display_preferences(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            })
            .unwrap();
        update(&console, vec![]);
        console
            .set_agent_family(
                vec![
                    agent(1, None, "Main robot probe"),
                    agent(2, Some(Uuid::from_u128(1)), "child robot probe"),
                ],
                Some(Uuid::from_u128(1)),
            )
            .unwrap();
        key(&console, KeyCode::Down);
        let label = format!("{robot} Subagents 1");
        for (collapsed, hint) in [(false, "Enter/Space hide"), (true, "Enter/Space expand")] {
            let rendered = frame(&console);
            let state = console.state.lock().unwrap();
            let target = state.footer_bar_target.as_ref().unwrap();
            let lower = &rendered[target.row];
            assert!(lower.starts_with(&format!("─ {label} ")), "{lower}");
            assert!(lower.ends_with("Context 21% used ─"), "{lower}");
            assert_eq!(display_width(lower), state.width);
            assert_eq!(target.categories[0].0, 2..15);
            assert_eq!(
                rendered.iter().any(|row| row.contains("child robot probe")),
                !collapsed
            );
            assert!(rendered.last().unwrap().contains(hint), "{rendered:?}");
            drop(state);
            key(&console, KeyCode::Enter);
        }
    }
}

#[test]
fn divider_edges_use_short_rules_and_exact_label_targets_with_narrow_fallbacks() {
    let console = fixture();
    update(&console, vec![]);
    console
        .set_agent_family(
            vec![
                agent(1, None, "Main edge"),
                agent(2, Some(Uuid::from_u128(1)), "child edge"),
            ],
            Some(Uuid::from_u128(1)),
        )
        .unwrap();
    console.state.lock().unwrap().tick_list.replace(
        TickGroup::Shells,
        vec![TickListEntry {
            id: TASK.to_string(),
            group: TickGroup::Shells,
            glyph: Glyph::ShellTick,
            title: "edge Shell".into(),
            kind: "Shell".into(),
            status: "running".into(),
            updated_at_ms: None,
            actions: Vec::new(),
        }],
    );
    for (width, prefix, suffix) in [
        (80, "─ ⏺ Subagents 1 ", "Context 21% used ─"),
        (55, "⏺ Subagents 1 ", "Context 21% used ─"),
        (53, "⏺ Subagents 1 ", "Context 21% used"),
    ] {
        console.state.lock().unwrap().resize(width, 24);
        let rendered = frame(&console);
        let state = console.state.lock().unwrap();
        let footer = state.footer_bar_target.as_ref().unwrap();
        let lower = &rendered[footer.row];
        assert!(
            lower.starts_with(prefix) && lower.ends_with(suffix),
            "{lower:?}"
        );
        assert_eq!(display_width(lower), usize::from(width) - 1);
        let label = footer.categories[0].0.clone();
        assert_eq!(
            lower
                .chars()
                .skip(label.start)
                .take(label.len())
                .collect::<String>(),
            "⏺ Subagents 1"
        );
        let ticks = state.tick_toggle_target.as_ref().unwrap();
        let upper = &rendered[ticks.row];
        assert!(upper.ends_with(" ◉ Ticks 1 ─"), "{upper:?}");
        assert_eq!(ticks.columns.end, state.width - 2);
        assert_eq!(
            upper
                .chars()
                .skip(ticks.columns.start)
                .take(ticks.columns.len())
                .collect::<String>(),
            "◉ Ticks 1"
        );
    }
    console.set_agent_family(vec![], None).unwrap();
    for (width, suffix) in [(14, " ◉ Ticks 1 ─"), (13, " ◉ Ticks 1")] {
        console.state.lock().unwrap().resize(width, 24);
        let rendered = frame(&console);
        let state = console.state.lock().unwrap();
        let ticks = state.tick_toggle_target.as_ref().unwrap();
        assert!(rendered[ticks.row].ends_with(suffix), "{rendered:?}");
        assert_eq!(ticks.columns.len(), 9);
    }
}

#[test]
fn subagents_and_context_colors_resolve_from_theme_roles() {
    use tickloop_agent::settings::GlyphSet;

    for glyph_set in [GlyphSet::Unicode, GlyphSet::NerdFont] {
        for theme_name in ThemeName::ALL {
            let console = fixture();
            console
                .set_agent_family(
                    vec![
                        agent(1, None, "Main theme probe"),
                        agent(2, Some(Uuid::from_u128(1)), "child theme probe"),
                    ],
                    Some(Uuid::from_u128(1)),
                )
                .unwrap();
            for (context, text, context_role) in [
                (Some((0, 200_000)), "Context 0% used", Role::Success),
                (None, "Context unknown", Role::MutedText),
            ] {
                console
                    .update_status_area("Auto".into(), "fixture".into(), context, vec![])
                    .unwrap();
                let mut state = console.state.lock().unwrap();
                state.display.set_preferences(DisplayPreferences {
                    glyph_set,
                    prefers_reduced_motion: false,
                });
                state.display.set_theme(theme_name);
                let label = format!("{} Subagents 1", state.display.glyph(Glyph::Subagents));
                for (collapsed, focused, entry_role) in [
                    (false, false, Role::Permission),
                    (true, false, Role::MutedText),
                    (false, true, Role::Suggestion),
                    (true, true, Role::Suggestion),
                ] {
                    state.status_area.agents_collapsed = collapsed;
                    state.footer.apply(FooterAction::Clear);
                    if focused {
                        state.footer.select(FooterItemId::Agents);
                    }
                    let (styled, _, _) = state.normal_styled_frame();
                    let lower = styled
                        .iter()
                        .find(|line| line.text().contains(&label))
                        .unwrap();
                    let resolved = lower.resolve(state.display.theme(), state.width);
                    for (segment, role) in [(label.as_str(), entry_role), (text, context_role)] {
                        assert!(
                            lower
                                .spans
                                .iter()
                                .any(|span| span.text.contains(segment) && span.role == Some(role)),
                            "{lower:?}"
                        );
                        let style = resolved
                            .spans
                            .iter()
                            .find(|span| span.text.contains(segment))
                            .unwrap()
                            .style;
                        assert_eq!(
                            style.foreground,
                            state.display.theme().style(role).foreground
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn family_updates_preserve_editor_focus_and_never_expose_todo_or_shell_categories() {
    let console = fixture();
    update(&console, vec![todo("todo unique")]);
    console
        .set_agent_family(
            vec![
                agent(1, None, "Main unique"),
                agent(2, Some(Uuid::from_u128(1)), "child unique"),
            ],
            Some(Uuid::from_u128(1)),
        )
        .unwrap();
    for character in "jk".chars() {
        key(&console, KeyCode::Char(character));
    }
    update(&console, vec![todo("new todo")]);
    let rendered = frame(&console);
    assert!(rendered.iter().any(|row| row.contains("child unique")));
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("keep literal jk draftjk"))
    );
    assert!(
        !rendered
            .iter()
            .any(|row| row.contains("Todo ") || row.contains("Shell "))
    );
    assert!(console.state.lock().unwrap().footer.selected().is_none());
}

#[test]
fn runtime_empty_progress_clears_completed_activity_details_while_working_and_idle() {
    let console = fixture();
    let mut done = todo("retired runtime task");
    done.status = tickloop_agent::runtime::TodoStatus::Completed;
    update(&console, vec![done]);
    console
        .set_activity(Some(&[(
            "Working runtime update".into(),
            Some(Role::MutedText),
        )]))
        .unwrap();
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("retired runtime task"))
    );
    update(&console, vec![]);
    let working = frame(&console);
    assert!(
        working
            .iter()
            .any(|row| row.contains("Working runtime update"))
    );
    assert!(
        !working
            .iter()
            .any(|row| row.contains("retired runtime task"))
    );
    console.set_activity(None).unwrap();
    assert!(
        !frame(&console)
            .iter()
            .any(|row| row.contains("retired runtime task"))
    );
}

#[test]
fn todos_follow_working_and_remain_when_idle_only_for_the_visible_agent() {
    let console = fixture();
    let mut pending = todo("pending unique");
    pending.status = tickloop_agent::runtime::TodoStatus::Pending;
    let active = todo("active unique");
    let mut done = todo("completed unique");
    done.status = tickloop_agent::runtime::TodoStatus::Completed;
    update(&console, vec![pending, active, done]);
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("pending unique"))
    );
    console
        .set_activity(Some(&[(
            "Working (Esc to interrupt)".into(),
            Some(Role::MutedText),
        )]))
        .unwrap();
    let working = frame(&console);
    let activity = working
        .iter()
        .position(|row| row.contains("Working (Esc"))
        .unwrap();
    assert!(
        working[activity + 1].contains("pending unique"),
        "{working:?}"
    );
    assert!(working[activity + 1].starts_with("  ⎿  ◻ "));
    assert!(working[activity + 2].starts_with("     ◼ "));
    assert!(working[activity + 3].starts_with("     ✔ "));
    assert!(working[activity + 2].contains("active unique"));
    assert!(working[activity + 3].contains("completed unique"));
    assert!(!working.iter().any(|row| row.contains("Todo 3")));
    console.activate_agent(Uuid::from_u128(99)).unwrap();
    update(&console, vec![todo("child todo unique")]);
    console
        .set_activity(Some(&[("Working child".into(), Some(Role::MutedText))]))
        .unwrap();
    let child = frame(&console);
    assert!(child.iter().any(|row| row.contains("child todo unique")));
    assert!(!child.iter().any(|row| row.contains("pending unique")));
    console.activate_agent(CHAT).unwrap();
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("pending unique"))
    );
    console.set_activity(None).unwrap();
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("pending unique"))
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
    let Some(InputEvent::TickAction { action, .. }) = key(&console, KeyCode::Enter) else {
        panic!("Shell shortcut did not open the Tick panel");
    };
    assert_eq!(
        action.instance,
        crate::tick_stream::InstanceRef::shell(TASK).to_string()
    );
    let panel = console
        .state
        .lock()
        .unwrap()
        .tick_list
        .detail
        .as_ref()
        .unwrap()
        .id();
    console
        .update_tick_detail::<crate::terminal::tick_shell::TickShell, _>(CHAT, panel, |detail| {
            detail.load(snapshot, None)
        })
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
        matches!(key(&console, KeyCode::Enter), Some(InputEvent::TickAction {action,..}) if action.instance == crate::tick_stream::InstanceRef::shell(second).to_string())
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
        Some(InputEvent::TickAction { action, .. }) if action.key == 'r'
    ));
    let Some(InputEvent::TickAction { action, .. }) = key(&console, KeyCode::Enter) else {
        panic!("Shell shortcut did not open Tick panel");
    };
    assert_eq!(
        action.instance,
        crate::tick_stream::InstanceRef::shell(target).to_string()
    );
    let panel = console
        .state
        .lock()
        .unwrap()
        .tick_list
        .detail
        .as_ref()
        .unwrap()
        .id();
    console
        .update_tick_detail::<crate::terminal::tick_shell::TickShell, _>(CHAT, panel, |detail| {
            detail.load(snapshot, None)
        })
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
        crate::terminal::open_tasks(&center, &console, source, &mut subscription, Some(peer_task)).await.unwrap();
        key(&console, KeyCode::Home);
        let detail = frame(&console);
        assert!(detail.iter().any(|row| row.contains("Status:")), "{detail:?}");
        assert!(detail.iter().any(|row| row.contains("peer-shell-marker")), "{detail:?}");
        assert_eq!(agent.origin(), Some(source_origin));
        key(&console, KeyCode::Left);
        let source_task = agent.submit_user_shell_for_goal(source, None, "printf 'source-shell-marker'; while [ ! -f source-finish ]; do sleep 0.02; done".into(), true).await.unwrap().command_id;
        let fresh = fixture();
        fresh.state.lock().unwrap().chat = Some(source);
        update(&fresh, vec![]);
        let mut subscription = None;
        crate::terminal::open_tasks(&center, &fresh, source, &mut subscription, None).await.unwrap();
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
    update(&console, vec![]);
    let lines = frame(&console);
    let editor = lines
        .iter()
        .position(|row| row.contains("keep literal jk draft"))
        .unwrap();
    assert!(
        lines[editor + 1].starts_with('─') && lines[editor + 1].ends_with("Context 21% used ─"),
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
fn core_status_is_always_right_aligned_with_styled_content_and_fallback() {
    for mode in ["Auto", "Plan", "Ask", "Full Access"] {
        let console = fixture();
        console
            .update_status_area(mode.into(), "模型 fixture".into(), None, vec![])
            .unwrap();
        let rendered = frame(&console);
        let editor = rendered
            .iter()
            .position(|row| row.contains("keep literal jk draft"))
            .unwrap();
        let divider = &rendered[editor + 1];
        assert!(
            divider.starts_with('─') && divider.ends_with("模型 fixture · Context unknown ─"),
            "{rendered:?}"
        );
        assert_eq!(display_width(divider), 79);
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.editor.draft(), "keep literal jk draft");
        let (styled, _, _) = state.normal_styled_frame();
        let core = styled
            .iter()
            .find(|row| row.text().contains("模型 fixture"))
            .unwrap();
        assert!(
            core.spans
                .iter()
                .any(|span| span.text == "模型 fixture" && span.role == Some(Role::Text))
        );
        state.resize(12, 24);
        assert!(state.frame().0.iter().any(|row| row.starts_with("⏵⏵")));
        state.resize(80, 15);
        assert!(state.frame().0.iter().any(|row| row.starts_with("⏵⏵")));
        state.resize(80, 24);
        state.configured_status_lines = Some(vec!["custom user status".into()]);
        let custom = state.frame().0;
        assert!(custom.iter().any(|row| row.contains("custom user status")));
        assert!(
            !custom
                .iter()
                .any(|row| row.starts_with('─') && row.contains("⏵⏵"))
        );
    }
}

#[test]
fn agents_entry_is_on_lower_divider_and_family_is_expanded_by_default() {
    let console = fixture();
    update(&console, vec![]);
    console
        .set_agent_family(
            vec![agent(1, None, "Main unique")],
            Some(Uuid::from_u128(1)),
        )
        .unwrap();
    assert!(
        !frame(&console)
            .iter()
            .any(|row| row.contains("Subagents 0") || row.contains("Main unique"))
    );
    console
        .set_agent_family(
            vec![
                agent(1, None, "Main unique"),
                agent(2, Some(Uuid::from_u128(1)), "child unique"),
            ],
            Some(Uuid::from_u128(1)),
        )
        .unwrap();
    let rendered = frame(&console);
    let editor = rendered
        .iter()
        .position(|row| row.contains("keep literal jk draft"))
        .unwrap();
    assert!(
        rendered[editor + 1].contains("Subagents 1")
            && rendered[editor + 1].ends_with("Context 21% used ─"),
        "{rendered:?}"
    );
    assert_eq!(
        rendered
            .iter()
            .filter(|row| row.contains("Subagents 1"))
            .count(),
        1
    );
    assert!(rendered.iter().any(|row| row.contains("child unique")));
    for (width, height) in [(24, 24), (48, 24), (80, 15)] {
        console.state.lock().unwrap().resize(width, height);
        let fallback = frame(&console);
        assert!(
            fallback.iter().any(|row| row.contains("Subagents 1")),
            "{fallback:?}"
        );
        assert!(
            fallback
                .iter()
                .all(|row| display_width(row) < usize::from(width))
        );
        key(&console, KeyCode::Down);
        if height == 15 {
            let focused = frame(&console);
            assert!(
                focused.iter().any(|row| row.contains("Enter/Space hide")),
                "{focused:?}"
            );
            assert!(
                !focused.iter().any(|row| row.contains("Enter list")),
                "{focused:?}"
            );
        }
        key(&console, KeyCode::Enter);
        assert!(
            !frame(&console)
                .iter()
                .any(|row| row.contains("child unique"))
        );
        key(&console, KeyCode::Char(' '));
        key(&console, KeyCode::Esc);
    }
}

#[test]
fn agents_divider_toggle_uses_mouse_hitbox_and_focused_enter_space_without_reopening_on_update() {
    let console = fixture();
    update(&console, vec![]);
    let family = vec![
        agent(1, None, "Main unique"),
        agent(2, Some(Uuid::from_u128(1)), "child unique"),
    ];
    console
        .set_agent_family(family.clone(), Some(Uuid::from_u128(1)))
        .unwrap();
    frame(&console);
    let click = |column| {
        let mut state = console.state.lock().unwrap();
        let row = state.footer_bar_target.as_ref().unwrap().row;
        state.pointer(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            },
            &mut io::sink(),
        );
    };
    click(5);
    assert!(
        !frame(&console)
            .iter()
            .any(|row| row.contains("child unique"))
    );
    console
        .set_agent_family(family, Some(Uuid::from_u128(1)))
        .unwrap();
    update(&console, vec![]);
    assert!(
        !frame(&console)
            .iter()
            .any(|row| row.contains("child unique"))
    );
    console.activate_agent(Uuid::from_u128(98)).unwrap();
    console.activate_agent(CHAT).unwrap();
    assert!(
        !frame(&console)
            .iter()
            .any(|row| row.contains("child unique")),
        "switching Agent keeps this View collapsed"
    );
    click(15);
    assert!(
        !frame(&console)
            .iter()
            .any(|row| row.contains("child unique")),
        "outside the entry must not toggle"
    );
    key(&console, KeyCode::Esc);
    key(&console, KeyCode::Down);
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("Enter/Space expand"))
    );
    key(&console, KeyCode::Enter);
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("child unique"))
    );
    key(&console, KeyCode::Char(' '));
    assert!(
        !frame(&console)
            .iter()
            .any(|row| row.contains("child unique"))
    );
    key(&console, KeyCode::Char(' '));
    key(&console, KeyCode::Down);
    assert!(matches!(
        key(&console, KeyCode::Enter),
        Some(InputEvent::OpenAgent(_))
    ));
    key(&console, KeyCode::Esc);
    assert!(!console.state.lock().unwrap().footer_list_focused);
    key(&console, KeyCode::Esc);
    key(&console, KeyCode::Char('j'));
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("keep literal jk draftj"))
    );
}

#[test]
fn modified_enter_cannot_focus_or_activate_a_collapsed_agents_list() {
    for modifiers in [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
    ] {
        let console = fixture();
        update(&console, vec![]);
        console
            .set_agent_family(
                vec![
                    agent(1, None, "Main unique"),
                    agent(2, Some(Uuid::from_u128(1)), "child unique"),
                ],
                Some(Uuid::from_u128(1)),
            )
            .unwrap();
        key(&console, KeyCode::Down);
        key(&console, KeyCode::Enter);
        assert!(
            !frame(&console)
                .iter()
                .any(|row| row.contains("child unique"))
        );
        let event = console
            .state
            .lock()
            .unwrap()
            .key(KeyEvent::new(KeyCode::Enter, modifiers));
        assert!(event.is_none());
        assert!(
            !console.state.lock().unwrap().footer_list_focused,
            "{modifiers:?}"
        );
        for code in [KeyCode::Down, KeyCode::Char('j'), KeyCode::PageDown] {
            assert!(key(&console, code).is_none());
            assert!(!console.state.lock().unwrap().footer_list_focused);
        }
        let hidden = frame(&console);
        assert!(
            !hidden
                .iter()
                .any(|row| row.contains("Enter open") || row.contains("child unique"))
        );
        assert!(key(&console, KeyCode::Enter).is_none());
        assert!(
            frame(&console)
                .iter()
                .any(|row| row.contains("child unique"))
        );
        key(&console, KeyCode::Down);
        assert!(matches!(
            key(&console, KeyCode::Enter),
            Some(InputEvent::OpenAgent(_))
        ));
    }
}

#[test]
fn todos_use_claude_status_marks_styles_and_width_ellipsis() {
    let console = fixture();
    let mut pending = todo("pending mark");
    pending.status = tickloop_agent::runtime::TodoStatus::Pending;
    let mut done = todo("completed mark");
    done.status = tickloop_agent::runtime::TodoStatus::Completed;
    update(
        &console,
        vec![pending, todo(&"active mark ".repeat(20)), done],
    );
    let rows = frame(&console);
    assert!(
        rows.iter().any(|row| row.contains("◻ pending mark")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("◼ active mark") && row.ends_with('…')),
        "{rows:?}"
    );
    assert!(rows.iter().any(|row| row.contains("✔ completed mark")));
    let mut bytes = Vec::new();
    let mut state = console.state.lock().unwrap();
    state.rendered_region = None;
    state.draw(&mut bytes).unwrap();
    let ansi = String::from_utf8(bytes).unwrap();
    assert!(
        ansi.contains("\x1b[9m"),
        "completed text must keep its strike decoration: {ansi:?}"
    );
}

#[test]
fn question_mark_help_matches_agents_focus_layers_and_tick_list_shells() {
    let mut state = State::new(true, 160, 40);
    state.prompt_enabled = true;
    state.chat = Some(CHAT);
    let console = tests::console_with_state(state);
    key(&console, KeyCode::Char('?'));
    let help = frame(&console).join("\n");
    for hint in [
        "↓ Subagents entry",
        "Enter/Space toggle Subagents (entry)",
        "↓ Agent list",
        "Enter open Agent",
        "Space fold descendants",
        "Esc list → entry → input",
        "Shift+↑/↓ Tick List Shells",
    ] {
        assert!(help.contains(hint), "missing {hint}: {help}");
    }
    assert!(!help.contains("tasks / children"));
    key(&console, KeyCode::Esc);
    key(&console, KeyCode::Char('j'));
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.starts_with('❯') && row.contains('j'))
    );
}
