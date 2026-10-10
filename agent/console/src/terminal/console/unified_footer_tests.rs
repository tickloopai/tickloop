use super::*;
use crate::{agent_console::AttentionGroup, terminal::task_surface::TaskSurfaceEffect};
use tickloop_agent_protocol::{
    LoopState, ShellTask, ShellTaskCapabilities, ShellTaskOrigin, ShellTaskOwner,
    ShellTaskSnapshot, ShellTaskState, SubagentRegistration,
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
        status: tickloop_agent_protocol::TodoStatus::InProgress,
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
        state: LoopState::Running,
        project: "fixture".into(),
        workspace: None,
        approvals: 0,
        questions: 0,
        group: AttentionGroup::Working,
        glyph: status_glyph(AttentionGroup::Working, false),
        preview: None,
        updated_at: None,
        subagent: parent.map(|_| SubagentRegistration {
            child_chat_id: Uuid::from_u128(id),
            handle: if title.contains(' ') {
                format!("child-{id}")
            } else {
                title.into()
            },
            description: title.into(),
            created_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
            color: Some(tickloop_agent_protocol::AgentColor::ALL[(id % 8) as usize]),
        }),
    }
}

#[test]
fn command_feedback_keeps_provider_failure_visible_with_an_empty_custom_status() {
    for width in [80, 120] {
        for height in [8, 24] {
            let mut state = State::new(true, width, height);
            state.prompt_enabled = true;
            state
                .editor
                .replace_draft("keep literal jk draft".into(), 0);
            state.configured_status_lines = Some(vec![]);
            state.status = "Cannot clear a child Agent's history".into();
            state.status_held = true;
            state.provider_failure = Some("Kimi Code · HTTP 403 · usage / plan limit".into());
            let (lines, _, _) = state.normal_styled_frame();
            let feedback = lines
                .iter()
                .filter(|line| line.text().contains("Cannot clear"))
                .collect::<Vec<_>>();
            assert_eq!(feedback.len(), 1);
            assert!(
                feedback[0]
                    .spans
                    .iter()
                    .all(|span| span.role == Some(Role::Warning))
            );
            assert!(!feedback[0].text().contains("HTTP 403"));
            assert!(lines.iter().any(|line| line.text().contains("HTTP 403")));
            assert!(
                lines
                    .iter()
                    .any(|line| line.text().contains("keep literal jk draft"))
            );
            assert!(
                lines
                    .iter()
                    .all(|line| display_width(&line.text()) <= usize::from(width))
            );
            assert_eq!(state.editor.draft(), "keep literal jk draft");
        }
    }
}

#[test]
fn child_input_badge_placeholder_and_panel_background_are_display_only() {
    use tickloop_agent_config::{DisplayPreferences, GlyphSet};
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
                    .text()
                    .trim_end_matches('─')
                    .ends_with(" @sleep-30-2 "),
                "the handle ends the upper divider on the right: {badge:?}"
            );
            assert!(badge.spans.iter().any(
                |span| span.text.contains("@sleep-30-2") && span.role == Some(Role::AgentGreen)
            ));
            assert!(
                badge
                    .spans
                    .iter()
                    .filter(|span| span.text.contains('─'))
                    .all(|span| span.role == Some(Role::AgentGreen)),
                "the viewed child's colour frames its input: {badge:?}"
            );
            let prompt = lines
                .iter()
                .find(|line| line.text().starts_with(state.display.glyph(Glyph::Prompt)))
                .expect("input row");
            assert_eq!(prompt.spans[0].text, state.display.glyph(Glyph::Prompt));
            assert_eq!(prompt.spans[0].role, Some(Role::AgentGreen));
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
fn main_input_names_its_task_at_the_right_of_the_upper_divider() {
    let console = fixture();
    update(&console, vec![]);
    console
        .set_task_title(Some("了解项目用途和模块结构".into()))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let (lines, _, _) = state.normal_styled_frame();
    let divider = lines
        .iter()
        .find(|line| line.text().contains("了解项目用途和模块结构"))
        .expect("task name on the upper divider");
    assert!(
        divider.text().starts_with("──")
            && divider
                .text()
                .trim_end_matches('─')
                .ends_with(" 了解项目用途和模块结构 "),
        "the task name sits at the right of the upper divider: {divider:?}"
    );
    assert!(divider.spans.iter().any(|span| {
        span.text.contains("了解项目用途和模块结构") && span.role == Some(Role::MutedText)
    }));
    state.width = 30;
    let (narrow, _, _) = state.normal_styled_frame();
    let divider = narrow
        .iter()
        .find(|line| line.text().starts_with("──") && line.text().contains('…'))
        .expect("a long task name is shortened to a third of the border");
    assert!(unicode_width::UnicodeWidthStr::width(divider.text().as_str()) <= 30);
    drop(state);
    console.set_task_title(None).unwrap();
    let state = console.state.lock().unwrap();
    assert!(
        !state
            .rendered_screen
            .iter()
            .chain(std::iter::once(&String::new()))
            .any(|line| line.contains("了解项目用途和模块结构"))
    );
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
        assert!(
            rendered.last().unwrap().contains("enter send"),
            "the key hints end the screen below the list: {rendered:?}"
        );
        key(&console, KeyCode::Down);
        let focused = frame(&console);
        let entry = focused
            .iter()
            .position(|row| row.contains("Subagents 1"))
            .unwrap();
        assert!(
            focused[entry + 1].starts_with("› ") && focused[entry + 1].contains("main"),
            "the first Down lands on a row: {focused:?}"
        );
        assert_eq!(focused.len(), rendered.len(), "{focused:?}");
        assert!(
            focused
                .last()
                .unwrap()
                .contains("enter open · space fold · esc input"),
            "{focused:?}"
        );
        key(&console, KeyCode::Esc);
        assert!(frame(&console).last().unwrap().contains("enter send"));
        assert!(
            !rendered.iter().any(|row| row.contains("items")
                || row.contains("↓ open")
                || row.contains("enter list")),
            "{rendered:?}"
        );
    }
}

#[test]
fn subagents_robot_prefix_preserves_divider_bounds_and_hints_in_both_states_and_glyph_sets() {
    use tickloop_agent_config::GlyphSet;

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
        let label = format!("{robot} Subagents 1");
        for collapsed in [false, true] {
            let rendered = frame(&console);
            let mut state = console.state.lock().unwrap();
            let target = state.footer_bar_target.as_ref().unwrap();
            let (row, column) = (target.row, target.categories[0].0.start);
            let lower = &rendered[target.row];
            assert!(lower.starts_with(&format!("─ {label} ")), "{lower}");
            assert!(lower.ends_with("Context 21% used ─"), "{lower}");
            assert_eq!(display_width(lower), state.width);
            assert_eq!(target.categories[0].0, 2..15);
            assert_eq!(
                rendered.iter().any(|row| row.contains("child robot probe")),
                !collapsed
            );
            assert!(
                rendered.last().unwrap().contains("enter send"),
                "folding never moves the key hints: {rendered:?}"
            );
            state.pointer(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: column as u16,
                    row: row as u16,
                    modifiers: KeyModifiers::NONE,
                },
                &mut io::sink(),
            );
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
    console.state.lock().unwrap().monitor.replace(
        TickGroup::Shells,
        vec![MonitorEntry {
            instance: tick_protocol::InstanceRef::shell(TASK),
            group: TickGroup::Shells,
            glyph: Glyph::ShellTick,
            title: "edge Shell".into(),
            kind: "Shell".into(),
            status: "running".into(),
            notes: Vec::new(),
            owner: crate::monitor_rows::TickOwner::Unknown,
            tone: crate::monitor_rows::StatusTone::Neutral,
            updated_at_ms: None,
            update: None,
            resources: None,
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
        let toggle = state.monitor_toggle_target.as_ref().unwrap();
        let upper = &rendered[toggle.row];
        assert!(upper.ends_with(" ◉ Monitor 1 ─"), "{upper:?}");
        assert_eq!(toggle.columns.end, state.width - 2);
        assert_eq!(
            upper
                .chars()
                .skip(toggle.columns.start)
                .take(toggle.columns.len())
                .collect::<String>(),
            "◉ Monitor 1"
        );
    }
    console.set_agent_family(vec![], None).unwrap();
    for (width, suffix) in [(16, " ◉ Monitor 1 ─"), (15, " ◉ Monitor 1")] {
        console.state.lock().unwrap().resize(width, 24);
        let rendered = frame(&console);
        let state = console.state.lock().unwrap();
        let toggle = state.monitor_toggle_target.as_ref().unwrap();
        assert!(rendered[toggle.row].ends_with(suffix), "{rendered:?}");
        assert_eq!(toggle.columns.len(), 11);
    }
}

#[test]
fn subagents_and_context_colors_resolve_from_theme_roles() {
    use tickloop_agent_config::GlyphSet;

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
                    state.footer.clear();
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
    done.status = tickloop_agent_protocol::TodoStatus::Completed;
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
    pending.status = tickloop_agent_protocol::TodoStatus::Pending;
    let active = todo("active unique");
    let mut done = todo("completed unique");
    done.status = tickloop_agent_protocol::TodoStatus::Completed;
    update(&console, vec![pending, active, done]);
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("pending unique"))
    );
    console
        .set_activity(Some(&[(
            "Working (esc to interrupt)".into(),
            Some(Role::MutedText),
        )]))
        .unwrap();
    let working = frame(&console);
    let activity = working
        .iter()
        .position(|row| row.contains("Working (esc"))
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

fn shell_snapshot(revision: u64, tasks: Vec<(Uuid, &str, ShellTaskState)>) -> ShellTaskSnapshot {
    ShellTaskSnapshot {
        chat_id: CHAT,
        live_revision: revision,
        tasks: tasks
            .into_iter()
            .map(|(id, command, state)| ShellTask {
                updated_at_ms: None,
                ended_at_ms: None,
                id,
                chat_id: CHAT,
                origin: ShellTaskOrigin::User,
                owner: ShellTaskOwner::User { chat_id: CHAT },
                call_id: None,
                command: command.into(),
                description: None,
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
    console
        .refresh_monitor_ticks(snapshot.clone(), None)
        .unwrap();
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
        tick_protocol::InstanceRef::shell(TASK).to_string()
    );
    let panel = console
        .state
        .lock()
        .unwrap()
        .monitor
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
    let detail = frame(&console);
    assert!(
        detail
            .iter()
            .any(|row| row.starts_with("$ selected shell") && row.ends_with("⏺ exit 0")),
        "{detail:?}"
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
        .refresh_monitor_ticks(
            shell_snapshot(
                1,
                vec![
                    (TASK, "first shell", ShellTaskState::Background),
                    (second, "second shell", ShellTaskState::Background),
                ],
            ),
            None,
        )
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
        matches!(key(&console, KeyCode::Enter), Some(InputEvent::TickAction {action,..}) if action.instance == tick_protocol::InstanceRef::shell(second).to_string())
    );
    assert_eq!(
        console.state.lock().unwrap().editor.draft(),
        "keep literal jk draft"
    );
}

#[test]
fn single_shell_shortcut_opens_its_exact_detail() {
    let console = fixture();
    let target = TASK;
    let snapshot = shell_snapshot(
        2,
        vec![(target, "sole eligible task", ShellTaskState::Background)],
    );
    console
        .refresh_monitor_ticks(snapshot.clone(), None)
        .unwrap();
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
        tick_protocol::InstanceRef::shell(target).to_string()
    );
    let panel = console
        .state
        .lock()
        .unwrap()
        .monitor
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
        detail
            .iter()
            .any(|row| row.starts_with("$ sole eligible task") && row.ends_with("⏺ running")),
        "{detail:?}"
    );
    key(&console, KeyCode::Esc);
    assert_eq!(
        console.state.lock().unwrap().editor.draft(),
        "keep literal jk draft"
    );
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
        let focused = frame(&console);
        assert!(
            focused.iter().any(|row| row.contains("enter open")),
            "{width}x{height}: {focused:?}"
        );
        assert!(
            !focused.iter().any(|row| row.contains("enter list")),
            "{focused:?}"
        );
        key(&console, KeyCode::Esc);
        assert!(console.state.lock().unwrap().footer.selected().is_none());
    }
}

#[test]
fn agents_divider_click_folds_the_panel_and_down_enters_the_list_without_reopening_on_update() {
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
    assert!(console.state.lock().unwrap().footer.selected().is_none());
    key(&console, KeyCode::Down);
    let listed = frame(&console);
    assert!(
        listed.iter().any(|row| row.contains("child unique")),
        "Down opens a folded panel and enters its list: {listed:?}"
    );
    assert!(listed.last().unwrap().contains("enter open"), "{listed:?}");
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
        Some(InputEvent::OpenAgent(id)) if id == Uuid::from_u128(2)
    ));
    assert!(
        console.state.lock().unwrap().footer.selected().is_none(),
        "opening a member returns the keys to the input"
    );
    key(&console, KeyCode::Char('j'));
    assert!(
        frame(&console)
            .iter()
            .any(|row| row.contains("keep literal jk draftj"))
    );
}

#[test]
fn modified_enter_never_opens_a_member_from_the_agents_list() {
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
        key(&console, KeyCode::Down);
        let event = console
            .state
            .lock()
            .unwrap()
            .key(KeyEvent::new(KeyCode::Enter, modifiers));
        assert!(event.is_none(), "{modifiers:?}");
        assert_eq!(
            console.state.lock().unwrap().footer.selected(),
            Some(FooterItemId::Agents),
            "{modifiers:?}"
        );
        assert!(matches!(
            key(&console, KeyCode::Enter),
            Some(InputEvent::OpenAgent(id)) if id == Uuid::from_u128(2)
        ));
    }
}

#[test]
fn todos_use_claude_status_marks_styles_and_width_ellipsis() {
    let console = fixture();
    let mut pending = todo("pending mark");
    pending.status = tickloop_agent_protocol::TodoStatus::Pending;
    let mut done = todo("completed mark");
    done.status = tickloop_agent_protocol::TodoStatus::Completed;
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
fn question_mark_help_matches_agents_focus_layers_and_monitor_shells() {
    let mut state = State::new(true, 160, 40);
    state.prompt_enabled = true;
    state.chat = Some(CHAT);
    let console = tests::console_with_state(state);
    key(&console, KeyCode::Char('?'));
    let help = frame(&console).join("\n");
    for hint in [
        "↓ Subagents list",
        "Click Subagents N fold list",
        "enter open Agent",
        "space fold descendants",
        "x dismiss finished",
        "esc list → input",
        "shift+↑/↓ Monitor Shells",
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
