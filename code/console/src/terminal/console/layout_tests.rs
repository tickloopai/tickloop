use super::*;
use crate::agent_console::AttentionGroup;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;

use tickloop_harness::protocol::{AgentColor, LoopState, SubagentRegistration};
use tickloop_harness::test_support::screen::{self, SELECTED_MARK};

const ROOT_AGENT: Uuid = Uuid::from_u128(100);
const CHILD_AGENT: Uuid = Uuid::from_u128(101);
const REQUEST_ID: Uuid = Uuid::from_u128(102);
const PTY_TIMEOUT: Duration = Duration::from_secs(8);

#[test]
fn agent_layout_background_waiting_roles_survive_status_configuration_help_and_connection() {
    for width in [40, 120] {
        for configured in [None, Some(Vec::new()), Some(vec!["custom status".into()])] {
            for help_open in [false, true] {
                for connected in [false, true] {
                    let mut state = State::new(true, width, 24);
                    state.prompt_enabled = true;
                    state.help_open = help_open;
                    state.configured_status_lines = configured.clone();
                    state
                        .status_area
                        .update("Auto".into(), "fixture".into(), None, None);
                    state.connection_notice =
                        (!connected).then(|| "Disconnected · cached view · fixture".into());
                    state.background_status_notice = Some(BackgroundStatusNotice {
                        title: Some("Review the implementation".into()),
                        ordinal: None,
                        message: "waiting for approval".into(),
                        message_role: Role::Permission,
                        compact_message: None,
                        necessary: true,
                    });
                    let (lines, _, _) = state.normal_styled_frame();
                    let waiting = lines.iter().find(|line| line.text().contains("waiting for approval"))
                        .unwrap_or_else(|| panic!("{width}, {configured:?}, help {help_open}, connected {connected}: {lines:?}"));
                    assert_eq!(
                        role_of(waiting, "waiting for approval"),
                        Some(Role::Permission),
                        "{waiting:?}"
                    );
                    if waiting.text().contains("Agent ") {
                        assert_eq!(role_of(waiting, "Agent "), Some(Role::Text), "{waiting:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn agent_layout_background_notice_preserves_the_regular_status_and_result() {
    let mut state = State::new(true, 120, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .update_status_area(
            "Auto".into(),
            "deepseek-flash".into(),
            Some((40_000, 1_000_000)),
            None,
        )
        .unwrap();
    console
        .set_agent_family(
            vec![
                agent_row(ROOT_AGENT, None, "visible"),
                agent_row(CHILD_AGENT, Some(ROOT_AGENT), "background"),
            ],
            Some(ROOT_AGENT),
        )
        .unwrap();
    console
        .set_background_status_notice(Some(BackgroundStatusNotice {
            title: Some("Review the implementation in /fixture/project/very/long/worktree/path and report every remaining issue\\nPreserve the original user message".into()),
            ordinal: Some(2),
            message: "Shell task completed".into(),
            message_role: Role::MutedText,
            compact_message: None,
            necessary: false,
        }))
        .unwrap();
    console.set_activity_and_status(None, "").unwrap();
    let frame = frame_lines(&console);
    let status = line_with(&frame, "⏵⏵ Auto").unwrap();
    assert!(status.contains("deepseek-flash"), "{frame:#?}");
    assert!(status.contains("Context 4% used"), "{frame:#?}");
    assert!(
        status.starts_with('─') || status.contains("Agents "),
        "{frame:#?}"
    );
    let notice = line_with(&frame, "Shell task completed").unwrap();
    assert!(notice.contains("Agent Review"), "{frame:#?}");
    assert!(notice.contains("(2)"), "{frame:#?}");
    assert_eq!(
        frame.iter().filter(|line| line.contains("⏵⏵ Auto")).count(),
        1
    );
    assert!(line_with(&frame, "Subagents 1").is_some(), "{frame:#?}");
}

#[test]
fn agent_layout_unsaved_status_and_exit_confirmation_keep_both_required_facts() {
    for configured in [
        None,
        Some(Vec::new()),
        Some(vec!["configured status with a long label".into()]),
        Some(vec!["first".into(), "second".into(), "third".into()]),
    ] {
        for help_open in [false, true] {
            let mut state = State::new(true, 40, 24);
            state.prompt_enabled = true;
            let console = super::tests::console_with_state(state);
            console
                .update_status_area("Auto".into(), "deepseek-flash".into(), None, None)
                .unwrap();
            console.set_status_lines(configured.clone()).unwrap();
            console
                .set_status("Not saved: /fixture/project/very/long/transcript/path: disk full")
                .unwrap();
            assert!(!console.confirm_control_c_exit().unwrap());
            console.state.lock().unwrap().help_open = help_open;
            let frame = frame_lines(&console);
            if help_open {
                let height = console.state.lock().unwrap().input_dialog_height();
                assert_eq!(
                    frame[frame.len() - height + input_dialog::INPUT_DIALOG_RULE_ROWS],
                    "Help"
                );
                let state = console.state.lock().unwrap();
                assert!(state.status.contains("Not saved"));
                assert!(state.exit_confirmation.is_some());
                assert_eq!(state.configured_status_lines, configured);
                continue;
            }
            let status = line_with(&frame, "Not saved")
                .unwrap_or_else(|| panic!("{configured:?}, help {help_open}: {frame:#?}"));
            assert!(
                status.contains("ctrl+c"),
                "{configured:?}, help {help_open}: {frame:#?}"
            );
            assert!(
                status.contains("exit"),
                "{configured:?}, help {help_open}: {frame:#?}"
            );
            assert!(display_width(status) < 40, "{frame:#?}");
            assert_eq!(
                console.state.lock().unwrap().configured_status_lines,
                configured
            );
        }
    }
}

#[test]
fn agent_layout_background_titles_use_display_columns_without_family_reading_numbers() {
    for width in [80, 120] {
        for title in [
            "Review the entire implementation in /fixture/project/very/long/worktree/path",
            "研究财报与投资目标，同时核对全部相关代码和执行状态",
        ] {
            let mut state = State::new(true, width, 24);
            state.prompt_enabled = true;
            let console = super::tests::console_with_state(state);
            console
                .update_status_area(
                    "Auto".into(),
                    "deepseek-flash".into(),
                    Some((40_000, 1_000_000)),
                    None,
                )
                .unwrap();
            console
                .set_agent_family(
                    vec![
                        agent_row(ROOT_AGENT, None, "visible"),
                        agent_row(CHILD_AGENT, Some(ROOT_AGENT), title),
                        agent_row(REQUEST_ID, Some(ROOT_AGENT), title),
                    ],
                    Some(ROOT_AGENT),
                )
                .unwrap();
            let notice = console
                .background_status_notice(
                    Some((REQUEST_ID, title.into())),
                    "Shell task completed".into(),
                    None,
                    false,
                    Role::MutedText,
                )
                .unwrap();
            console.set_background_status_notice(Some(notice)).unwrap();
            let frame = frame_lines(&console);
            let status = line_with(&frame, "⏵⏵ Auto").unwrap();
            assert!(
                status.contains("deepseek-flash") && status.contains("Context 4% used"),
                "{width} columns: {frame:#?}"
            );
            assert!(
                status.starts_with('─') || status.contains("Agents "),
                "{width} columns: {frame:#?}"
            );
            let notice = line_with(&frame, "Shell task completed").unwrap();
            assert!(
                notice.contains("Shell task completed") && !notice.contains("(2)"),
                "{width} columns: {frame:#?}"
            );
            assert!(
                notice.contains(if title.starts_with('研') {
                    "研究"
                } else {
                    "Rev"
                }),
                "{width} columns: {frame:#?}"
            );
            assert!(display_width(status) < usize::from(width), "{frame:#?}");
            assert!(display_width(notice) < usize::from(width), "{frame:#?}");
            assert_eq!(
                frame.iter().filter(|line| line.contains("⏵⏵ Auto")).count(),
                1
            );
            assert!(line_with(&frame, "Subagents 2").is_some(), "{frame:#?}");
        }
    }
}

#[test]
fn agent_layout_necessary_background_facts_survive_narrow_status_configuration_and_help() {
    for (message, compact, expected) in [
        ("failed: fixture", None, "failed: fixture"),
        ("waiting for approval", None, "waiting for approval"),
        ("waiting for your answer", None, "waiting for your answer"),
        (
            "Outcome unknown · execution lost · nothing resent",
            Some("Unknown/lost/no resend"),
            "Unknown/lost/no resend",
        ),
    ] {
        for configured in [
            None,
            Some(Vec::new()),
            Some(vec!["configured status".into()]),
            Some(vec!["one".into(), "two".into(), "three".into()]),
        ] {
            for help_open in [false, true] {
                let mut state = State::new(true, 24, 24);
                state.prompt_enabled = true;
                let console = super::tests::console_with_state(state);
                console
                    .update_status_area("Auto".into(), "deepseek-flash".into(), None, None)
                    .unwrap();
                console.set_status_lines(configured.clone()).unwrap();
                let notice = console
                    .background_status_notice(
                        Some((CHILD_AGENT, "研究财报与完整实现及其所有执行状态".into())),
                        message.into(),
                        compact.map(str::to_owned),
                        true,
                        if message.starts_with("waiting") {
                            Role::Permission
                        } else {
                            Role::Warning
                        },
                    )
                    .unwrap();
                console.set_background_status_notice(Some(notice)).unwrap();
                console.state.lock().unwrap().help_open = help_open;
                let frame = frame_lines(&console);
                if help_open {
                    let height = console.state.lock().unwrap().input_dialog_height();
                    assert_eq!(
                        frame[frame.len() - height + input_dialog::INPUT_DIALOG_RULE_ROWS],
                        "Help"
                    );
                    let state = console.state.lock().unwrap();
                    assert!(state.background_status_notice.is_some());
                    assert_eq!(state.configured_status_lines, configured);
                    continue;
                }
                assert!(
                    line_with(&frame, expected).is_some(),
                    "{message}, {configured:?}, help {help_open}: {frame:#?}"
                );
                assert!(
                    frame.iter().all(|line| display_width(line) < 24),
                    "{frame:#?}"
                );
                assert_eq!(
                    console.state.lock().unwrap().configured_status_lines,
                    configured
                );
            }
        }
    }
}

#[test]
fn agent_layout_connection_and_necessary_background_facts_both_survive_narrow_status() {
    for width in [24, 40] {
        for message in [
            "waiting for approval",
            "Outcome unknown · execution lost · nothing resent",
        ] {
            let mut state = State::new(true, width, 24);
            state.prompt_enabled = true;
            let console = super::tests::console_with_state(state);
            console
                .update_status_area("Auto".into(), "deepseek-flash".into(), None, None)
                .unwrap();
            console.set_status_lines(Some(Vec::new())).unwrap();
            console
                .set_connection_notice(Some("Disconnected · cached view · fixture".into()))
                .unwrap();
            let notice = console
                .background_status_notice(
                    Some((CHILD_AGENT, "研究财报与完整实现".into())),
                    message.into(),
                    (message.starts_with("Outcome")).then(|| "Unknown/lost/no resend".into()),
                    true,
                    if message.starts_with("waiting") {
                        Role::Permission
                    } else {
                        Role::Warning
                    },
                )
                .unwrap();
            console.set_background_status_notice(Some(notice)).unwrap();
            let frame = frame_lines(&console);
            assert!(
                line_with(&frame, "Disconnected").is_some()
                    || line_with(&frame, "Offline").is_some(),
                "{frame:#?}"
            );
            let expected = if message.starts_with("Outcome") {
                "Unknown/lost/no resend"
            } else {
                message
            };
            assert!(
                line_with(&frame, expected).is_some(),
                "{width} columns: {frame:#?}"
            );
            assert!(
                frame
                    .iter()
                    .all(|line| display_width(line) < usize::from(width)),
                "{frame:#?}"
            );
            assert_eq!(
                console.state.lock().unwrap().configured_status_lines,
                Some(Vec::new())
            );
        }
    }
}

#[test]
fn agent_layout_held_overview_refusal_precedes_hints_at_narrow_widths() {
    for width in [24, 40] {
        let mut state = State::new(true, width, 16);
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        console
            .set_global_overview(Some(AgentOverview {
                rows: vec![agent_row(ROOT_AGENT, None, "root")],
                dispatch_directory: PathBuf::from("/fixture/launch"),
                created: None,
                visible: None,
            }))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Tab));
        state.insert("/model");
        assert!(state.key(key(KeyCode::Enter)).is_none());
        let frame = drawn(&mut state);
        assert!(
            line_with(&frame, "/model isn't available").is_some(),
            "{width} columns: {frame:#?}"
        );
        state.key(key(KeyCode::Char('x')));
        let frame = drawn(&mut state);
        assert!(
            line_with(&frame, "enter dispatch").is_some(),
            "hints return on the next key at {width} columns: {frame:#?}"
        );
        for input_focused in [false, true] {
            state.overview.input_focused = input_focused;
            assert!(state.key(key(KeyCode::BackTab)).is_none());
            let frame = drawn(&mut state);
            assert!(
                line_with(&frame, "shift+tab mode isn't").is_some(),
                "{width} columns, input focused {input_focused}: {frame:#?}"
            );
        }
    }
}

#[test]
fn agent_layout_child_refusal_keeps_the_parent_reason_at_narrow_widths() {
    for width in [24, 40, 120] {
        let mut state = State::new(true, width, 24);
        state.prompt_enabled = true;
        state.visible_delegated = true;
        state
            .status_area
            .update("Auto".into(), "fixture".into(), None, None);
        state.insert("/mode full");
        assert!(state.key(key(KeyCode::Enter)).is_none());
        let frame = drawn(&mut state);
        assert!(
            line_with(&frame, "/mode isn't available").is_some(),
            "{width} columns: {frame:#?}"
        );
        assert_eq!(state.editor.draft(), "/mode full");
        assert!(state.key(key(KeyCode::BackTab)).is_none());
        let frame = drawn(&mut state);
        assert!(
            line_with(&frame, "shift+tab mode isn't").is_some(),
            "{width} columns: {frame:#?}"
        );
        if width == 120 {
            assert!(
                line_with(&frame, "shift+tab mode isn't available").is_some(),
                "{frame:#?}"
            );
            assert!(
                line_with(&frame, "authorization is set by the parent").is_some(),
                "{frame:#?}"
            );
        }
    }
}

#[test]
fn child_clear_feedback_has_its_own_row_above_the_composer_and_expanded_family() {
    for width in [80, 120] {
        let mut state = State::new(true, width, 40);
        state.chat = Some(ROOT_AGENT);
        state.prompt_enabled = true;
        state.visible_delegated = true;
        state
            .status_area
            .update("Auto".into(), "fixture".into(), Some((20000, 200000)), None);
        state.status_area.update_agents(
            vec![
                agent_row(ROOT_AGENT, None, "long root title"),
                agent_row(Uuid::from_u128(202), Some(ROOT_AGENT), "First child"),
                agent_row(Uuid::from_u128(203), Some(ROOT_AGENT), "Second child"),
                agent_row(Uuid::from_u128(204), Some(ROOT_AGENT), "Third child"),
            ],
            Some(Uuid::from_u128(202)),
        );
        state.background_status_notice = Some(BackgroundStatusNotice {
            title: Some("background title must stay out of command feedback".into()),
            ordinal: None,
            message: "working".into(),
            message_role: Role::Text,
            compact_message: None,
            necessary: false,
        });
        state.insert("/clear");
        assert!(state.key(key(KeyCode::Enter)).is_none());
        let (lines, _, _) = state.normal_styled_frame();
        let input = lines
            .iter()
            .position(|line| line.text().starts_with('❯'))
            .unwrap();
        let feedback = lines
            .iter()
            .position(|line| line.text().contains("/clear") && !line.text().starts_with('❯'))
            .unwrap();
        let divider = lines
            .iter()
            .position(|line| line.text().contains("Subagents 3"))
            .unwrap();
        assert!(
            feedback < input,
            "feedback belongs above the composer, not between the Subagents divider and rows: {lines:?}"
        );
        assert_eq!(
            feedback + 2,
            input,
            "one feedback row then the input border"
        );
        assert!(lines[feedback].text().contains("/clear"));
        assert!(!lines[feedback].text().contains("background title"));
        assert!(!lines[feedback].text().contains("Auto"));
        assert!(lines[divider].text().contains("Auto · fixture"));
        assert!(lines[divider + 1].text().contains("main"));
        assert!(
            lines
                .iter()
                .all(|line| display_width(&line.text()) <= usize::from(width))
        );
        assert_eq!(state.editor.draft(), "/clear");
    }
}

fn agent_row(id: Uuid, parent: Option<Uuid>, title: &str) -> AgentRow {
    AgentRow {
        agent_id: id,
        parent_id: parent,

        title: title.into(),
        status: "running".into(),
        state: LoopState::Running,
        project: "project-a".into(),
        workspace: Some("workspace-a".into()),
        approvals: usize::from(id == ROOT_AGENT),
        questions: 0,
        group: if id == ROOT_AGENT {
            AttentionGroup::NeedsInput
        } else {
            AttentionGroup::Working
        },
        glyph: status_glyph(
            if id == ROOT_AGENT {
                AttentionGroup::NeedsInput
            } else {
                AttentionGroup::Working
            },
            false,
        ),
        preview: None,
        updated_at: None,
        run_started_at: None,
        subagent: parent.map(|_| SubagentRegistration {
            child_chat_id: id.into(),
            handle: format!("child-{}", id.as_u128() % 1000),
            description: title.into(),
            created_at: tickloop_harness::base::private_fs::unix_time_milliseconds(),
            color: Some(AgentColor::ALL[(id.as_u128() % 8) as usize]),
        }),
    }
}

pub(super) fn set_status_family(area: &mut StatusArea) {
    area.update_agents(
        (0..20)
            .map(|index| {
                agent_row(
                    Uuid::from_u128(100 + index),
                    (index > 0).then_some(ROOT_AGENT),
                    &format!("task {index:02}"),
                )
            })
            .collect(),
        Some(ROOT_AGENT),
    );
}

fn task_board() -> Option<TaskBoard> {
    let titles = (0..20)
        .map(|index| format!("task {index:02}"))
        .collect::<Vec<_>>();
    Some(crate::terminal::status_area::test_task_board(
        &titles.iter().map(String::as_str).collect::<Vec<_>>(),
    ))
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn agent_layout_rejected_shell_restores_shell_mode_and_preserves_newer_text() {
    for newer in ["", "newer prompt"] {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.key(key(KeyCode::Char('!')));
        state.insert("printf rejected");
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::UserShellSubmitted(submission)) if submission.command() == "printf rejected")
        );
        state.insert(newer);
        let console = super::tests::console_with_state(state);
        console
            .restore_rejected_shell("printf rejected", "Shell rejected")
            .unwrap();
        let state = console.state.lock().unwrap();
        if newer.is_empty() {
            assert_eq!(state.editor.draft(), "printf rejected");
            assert_eq!(state.shell_input.kind(), InputKind::Shell);
        } else {
            assert_eq!(state.editor.draft(), newer);
            assert_eq!(state.shell_input.kind(), InputKind::Prompt);
        }
    }
}

#[test]
fn agent_layout_removed_footer_item_chooses_the_nearest_surviving_task() {
    let mut state = State::new(true, 140, 24);
    let chat = Uuid::new_v4();
    state.chat = Some(chat);
    state.prompt_enabled = true;
    state.insert("cached draft");
    let console = super::tests::console_with_state(state);
    let mut snapshot = super::tests::task_snapshot(chat, 20);
    for task in &mut snapshot.tasks {
        task.was_background = true;
    }
    console
        .refresh_monitor_ticks(snapshot.clone(), None)
        .unwrap();
    let selected = snapshot.tasks[12].id;
    console
        .state
        .lock()
        .unwrap()
        .monitor
        .select(&tick_protocol::InstanceRef::shell(selected));
    snapshot.tasks.retain(|task| task.id != selected);
    console
        .refresh_monitor_ticks(snapshot.clone(), None)
        .unwrap();
    let state = console.state.lock().unwrap();
    assert_eq!(state.monitor.entries().len(), 19);
    assert_ne!(
        state.monitor.selected().unwrap().instance,
        tick_protocol::InstanceRef::shell(selected)
    );
    assert_eq!(state.editor.draft(), "cached draft");
}

#[test]
fn agent_layout_narrow_agent_rows_keep_request_types_when_the_title_is_long() {
    let mut state = State::new(true, 40, 24);
    state.prompt_enabled = true;
    state
        .status_area
        .update("Ask".into(), "fixture".into(), None, None);
    let console = super::tests::console_with_state(state);
    let mut child = agent_row(CHILD_AGENT, Some(ROOT_AGENT), &"很长的研究标题".repeat(20));
    child.approvals = 1;
    child.questions = 2;
    console.set_agent_family(vec![child], None).unwrap();
    let mut state = console.state.lock().unwrap();
    state.key(key(KeyCode::Down));
    let frame = state.frame().0;
    let row = frame.iter().find(|line| line.contains("A1 Q2")).unwrap();
    assert!(row.contains("A1 Q2"), "{frame:?}");
    assert!(row.contains("很长"), "{frame:?}");
    state.key(key(KeyCode::Down));
    assert!(
        matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::OpenAgent(id)) if id == CHILD_AGENT)
    );
}

#[test]
fn agent_layout_footer_enters_at_the_bottom_with_following_disabled() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state
        .status_area
        .update("Ask".into(), "fixture".into(), None, task_board());
    set_status_family(&mut state.status_area);
    state
        .footer
        .update(state.status_area.available_panels().collect::<Vec<_>>());
    state.scrollback.append("Current completed reply\n");
    state.scrollback.frame(80, 16);
    state.scrollback.set_auto_scroll(false);
    assert!(!state.scrollback.is_following());
    state.insert("keep this draft");
    state.key(key(KeyCode::Down));
    assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));
    assert!(state.frame().0.iter().any(|row| row.contains("main")));
    assert_eq!(state.editor.draft(), "keep this draft");
    assert!(!state.scrollback.is_following());
}

#[test]
fn agent_layout_first_down_enters_the_list_and_list_keys_preserve_the_draft() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state
        .status_area
        .update("Ask".into(), "test model".into(), None, task_board());
    set_status_family(&mut state.status_area);
    state
        .footer
        .update(state.status_area.available_panels().collect::<Vec<_>>());
    state.insert("keep jk");
    assert!(state.frame().0.iter().any(|line| line.contains("main")));
    assert!(state.footer.selected().is_none());
    state.key(key(KeyCode::Down));
    assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));
    assert!(state.frame().0.iter().any(|line| line.contains("main")));
    for _ in 0..12 {
        state.key(key(KeyCode::Char('j')));
    }
    assert!(state.frame().0.iter().any(|line| line.contains("task 12")));
    assert_eq!(state.editor.draft(), "keep jk");
    state.key(key(KeyCode::Esc));
    assert!(state.footer.selected().is_none());
    assert!(state.status_area.is_open());
    state.key(key(KeyCode::Char('j')));
    state.key(key(KeyCode::Char('k')));
    assert_eq!(state.editor.draft(), "keep jkjk");
}

#[test]
fn agent_layout_return_restores_reading_draft_and_focuses_the_editor() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    assert!(!console.activate_agent(ROOT_AGENT).unwrap());
    let mut state = console.state.lock().unwrap();
    let width = state.width;
    state.scrollback.append(
        &(0..80)
            .map(|index| format!("root history {index}\n"))
            .collect::<String>(),
    );
    state.scrollback.frame(width, 16);
    state.insert("parent draft");
    state
        .status_area
        .update("Ask".into(), "test model".into(), None, task_board());
    set_status_family(&mut state.status_area);
    let panels = state.status_area.available_panels().collect::<Vec<_>>();
    state.footer.update(panels);
    state.key(key(KeyCode::Down));
    state.key(key(KeyCode::Char('j')));
    state.scrollback.scroll(ScrollAction::First);
    let selected = state.status_area.selected_agent();
    let reading = state.scrollback.frame(width, 16).lines;
    drop(state);
    assert!(!console.activate_agent(CHILD_AGENT).unwrap());
    let mut state = console.state.lock().unwrap();
    assert!(state.editor.draft().is_empty());
    assert!(state.scrollback.is_following());
    assert!(state.footer.selected().is_none());
    state.insert("child draft");
    drop(state);
    assert!(console.activate_agent(ROOT_AGENT).unwrap());
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.editor.draft(), "parent draft");
    assert!(!state.scrollback.is_following());
    assert_eq!(state.scrollback.frame(width, 16).lines, reading);
    assert_eq!(state.status_area.selected_agent(), selected);
    assert!(state.footer.selected().is_none());
    assert!(state.status_area.is_open());
    assert_eq!(
        state.status_area.selected_panel(),
        Some(FooterItemId::Agents)
    );
    assert!(!state.conversation_focused);
    drop(state);
    assert!(console.activate_agent(CHILD_AGENT).unwrap());
    let state = console.state.lock().unwrap();
    assert_eq!(state.editor.draft(), "child draft");
}

#[test]
fn agent_layout_request_identity_and_idle_gate_preserve_the_editor() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.insert("unsent parent draft");
    let console = super::tests::console_with_state(state);
    assert!(!console.can_present_request().unwrap());
    console.state.lock().unwrap().last_input_edit =
        Some(Instant::now() - agent_layout::REQUEST_IDLE_DELAY);
    assert!(console.can_present_request().unwrap());
    console.set_request_layout(Some(REQUEST_ID)).unwrap();
    console
        .show_commentable_menu(
            Menu {
                id: REQUEST_ID,
                title: "Permission required".into(),
                context: vec!["Requester: parent".into()],
                options: vec!["Allow".into(), "Reject".into()],
                selected: 0,
                confirmation: true,
            },
            vec![1],
        )
        .unwrap();
    assert!(
        console
            .set_request_layout(Some(Uuid::from_u128(103)))
            .is_err()
    );
    let mut state = console.state.lock().unwrap();
    let frame = state.frame().0;
    assert!(
        !frame
            .iter()
            .any(|line| line.contains("unsent parent draft"))
    );
    assert!(state.menu_focused);
    assert!(frame.len() <= 18);
    assert!(
        matches!(state.key(key(KeyCode::Esc)), Some(InputEvent::Cancel { menu_id }) if menu_id == REQUEST_ID)
    );
    assert_eq!(state.editor.draft(), "unsent parent draft");
}

#[test]
fn agent_layout_float_control_is_visible_only_for_its_reading_view() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.activate_agent(ROOT_AGENT).unwrap();
    console
        .write(
            &(0..100)
                .map(|row| format!("root history {row:03}\n"))
                .collect::<String>(),
        )
        .unwrap();
    console
        .set_activity(Some(&[("root Working".into(), Some(Role::Accent))]))
        .unwrap();
    console.set_activity_interruptible(true).unwrap();
    frame_lines(&console);
    console
        .state
        .lock()
        .unwrap()
        .key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    let reading = frame_lines(&console);
    let row = reading
        .iter()
        .position(|line| line.contains("Back to bottom"))
        .unwrap();
    let column = reading[row]
        .chars()
        .position(|character| character == '↓')
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: row as u16,
            column: column as u16,
            modifiers: KeyModifiers::NONE,
        };
        state.pointer(press, &mut std::io::sink());
        state.focus_lost();
        state.pointer(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..press
            },
            &mut std::io::sink(),
        );
        assert!(!state.scrollback.is_following());
        state.insert("root draft");
    }
    console.activate_agent(CHILD_AGENT).unwrap();
    let child = frame_lines(&console);
    assert!(line_with(&child, "Back to bottom").is_none());
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .back_to_bottom_target
            .is_none()
    );
    console.activate_agent(ROOT_AGENT).unwrap();
    let root = frame_lines(&console);
    assert!(line_with(&root, "Back to bottom").is_some());
    let mut state = console.state.lock().unwrap();
    assert!(!state.activity_interruptible);
    assert_eq!(state.editor.draft(), "root draft");
    assert!(state.key(key(KeyCode::Esc)).is_none());
    assert!(state.scrollback.is_following());
    assert!(matches!(
        state.key(key(KeyCode::Esc)),
        Some(InputEvent::Escape)
    ));
    assert_eq!(state.editor.draft(), "root draft");
}

#[test]
fn agent_layout_float_return_keeps_input_geometry_and_escape_priorities() {
    for (width, height) in [(5, 16), (12, 16), (25, 16), (41, 24), (81, 32), (121, 48)] {
        let mut state = State::new(true, width, height);
        state.prompt_enabled = true;
        state.insert("keep draft and caret");
        let console = super::tests::console_with_state(state);
        console
            .write(
                &(0..200)
                    .map(|row| format!("history {row:03}\n"))
                    .collect::<String>(),
            )
            .unwrap();
        console
            .set_activity(Some(&[("Working live".into(), Some(Role::Accent))]))
            .unwrap();
        console.set_activity_interruptible(true).unwrap();
        frame_lines(&console);
        let (bottom_rows, cursor, draft_cursor) = {
            let mut state = console.state.lock().unwrap();
            let (lines, row, column) = state.frame();
            (
                lines.len(),
                (state.height - lines.len() + row, column),
                state.editor.cursor(),
            )
        };
        console
            .state
            .lock()
            .unwrap()
            .key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        let frame = frame_lines(&console);
        let mut state = console.state.lock().unwrap();
        let (lines, row, column) = state.frame();
        let target = state.back_to_bottom_target.as_ref().unwrap();
        let label = state.back_to_bottom_label(state.content_columns()).unwrap();
        let control = &frame[target.row];
        let left = target.columns.start;
        let right = state.width - target.columns.end;
        assert!(left.abs_diff(right) <= 1, "{width}: {control:?}");
        assert_eq!(target.columns.len(), display_width(&label));
        assert_eq!(
            control.trim_end().trim_matches('─'),
            label,
            "{width}: {control:?}"
        );
        assert_eq!(lines.len(), bottom_rows);
        assert_eq!((state.height - lines.len() + row, column), cursor);
        state.help_open = true;
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(!state.help_open);
        assert!(!state.scrollback.is_following());
        state.menu = Some(Menu {
            id: REQUEST_ID,
            title: "Approval".into(),
            context: Vec::new(),
            options: vec!["Allow".into(), "Deny".into()],
            selected: 0,
            confirmation: true,
        });
        state.menu_focused = true;
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Cancel { .. })
        ));
        assert!(!state.scrollback.is_following());
        state.clear_menu(REQUEST_ID);
        state.mark_menu_submission_finished(REQUEST_ID);
        state.release_finished_submissions();
        state
            .status_area
            .update("Ask".into(), "fixture".into(), None, task_board());
        set_status_family(&mut state.status_area);
        let panels = state.status_area.available_panels().collect::<Vec<_>>();
        state.footer.update(panels);
        state.footer.select(FooterItemId::Agents);
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.footer.selected().is_none());
        assert!(!state.scrollback.is_following());
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.scrollback.is_following());
        assert_eq!(state.editor.cursor(), draft_cursor);
        assert_eq!(state.editor.draft(), "keep draft and caret");
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::InterruptActivity)
        ));
        assert!(state.escape_confirmation.is_none());
        state.activity_interruptible = false;
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Escape)
        ));
        assert!(state.escape_confirmation.is_some());
        assert_eq!(state.editor.draft(), "keep draft and caret");
    }
}

#[test]
fn agent_layout_working_activity_scrolls_with_body_and_returns_to_bottom() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.insert("preserved draft");
    let console = super::tests::console_with_state(state);
    console
        .write(
            &(0..200)
                .map(|row| format!("history row {row:03}\n"))
                .collect::<String>(),
        )
        .unwrap();
    console
        .set_activity(Some(&[(
            "Working live activity".into(),
            Some(Role::Accent),
        )]))
        .unwrap();
    let latest = frame_lines(&console);
    assert!(line_with(&latest, "Working live activity").is_some());
    console
        .state
        .lock()
        .unwrap()
        .key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    let reading = frame_lines(&console);
    assert!(
        line_with(&reading, "history row 000").is_some(),
        "{reading:#?}"
    );
    assert!(
        line_with(&reading, "Working live activity").is_none(),
        "{reading:#?}"
    );
    let control = line_with(&reading, "Back to bottom").unwrap();
    assert!(
        control.trim_end().trim_matches('─').ends_with("esc"),
        "{reading:#?}"
    );
    assert!(
        control.starts_with('─'),
        "the return control floats over the input border: {reading:#?}"
    );
    console
        .set_activity(Some(&[(
            "Thinking later activity".into(),
            Some(Role::Accent),
        )]))
        .unwrap();
    console.write("new output while reading\n").unwrap();
    let updated = frame_lines(&console);
    assert_eq!(reading[0], updated[0]);
    assert!(line_with(&updated, "Thinking later activity").is_none());
    {
        let mut state = console.state.lock().unwrap();
        state.resize(100, 32);
        state.key(key(KeyCode::Esc));
        assert!(state.scrollback.is_following());
        assert_eq!(state.editor.draft(), "preserved draft");
    }
    let bottom = frame_lines(&console);
    assert!(
        line_with(&bottom, "Thinking later activity").is_some(),
        "{bottom:#?}"
    );
    assert!(
        line_with(&bottom, "Back to bottom").is_none(),
        "{bottom:#?}"
    );
    console
        .state
        .lock()
        .unwrap()
        .key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    let reading = frame_lines(&console);
    let row = reading
        .iter()
        .position(|line| line.contains("Back to bottom"))
        .unwrap();
    let column = reading[row]
        .chars()
        .position(|character| character == '↓')
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let cursor = state.editor.cursor();
    assert!(state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: row as u16,
            column: column as u16,
            modifiers: KeyModifiers::NONE
        },
        &mut std::io::sink()
    ));
    assert!(state.pointer(
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            row: row as u16,
            column: column as u16,
            modifiers: KeyModifiers::NONE
        },
        &mut std::io::sink()
    ));
    assert!(state.scrollback.is_following());
    assert_eq!(state.editor.cursor(), cursor);
    assert_eq!(state.editor.draft(), "preserved draft");
    assert!(state.selection.is_none());
}

#[test]
fn agent_layout_created_chat_keeps_its_draft_history_and_keeps_other_views_isolated() {
    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .write_roles(&[("original draft banner\n".into(), Some(Role::Title))])
        .unwrap();
    console
        .write_roles(&[("draft warning\n".into(), Some(Role::Error))])
        .unwrap();
    console.record_input("draft echo").unwrap();
    console.select_chat(Some(ROOT_AGENT)).unwrap();
    assert!(line_with(&frame_lines(&console), "original draft banner").is_none());
    console
        .write_roles(&[("existing Agent body\n".into(), Some(Role::Text))])
        .unwrap();
    console.clear_visible_agent().unwrap();
    let created_banner = vec![
        ("Tickloop v0.2.0\n".into(), Some(Role::Title)),
        (
            "current-model · current-protocol\n".into(),
            Some(Role::MutedText),
        ),
        ("/fixture/current-directory\n".into(), Some(Role::MutedText)),
    ];
    console
        .select_created_chat(CHILD_AGENT, &created_banner)
        .unwrap();
    console
        .select_created_chat(CHILD_AGENT, &created_banner)
        .unwrap();
    let frame = frame_lines(&console);
    for line in ["original draft banner", "draft warning", "draft echo"] {
        assert_eq!(
            frame.iter().filter(|text| text.contains(line)).count(),
            1,
            "{frame:#?}"
        );
    }
    for absent in [
        "Tickloop v0.2.0",
        "current-model · current-protocol",
        "/fixture/current-directory",
        "existing Agent body",
    ] {
        assert!(line_with(&frame, absent).is_none(), "{frame:#?}");
    }
    for _ in 0..2 {
        console.reset_chat(CHILD_AGENT).unwrap();
        console.write("rebuilt public conversation\n").unwrap();
        let frame = frame_lines(&console);
        for line in [
            "original draft banner",
            "draft warning",
            "draft echo",
            "rebuilt public conversation",
        ] {
            assert_eq!(
                frame.iter().filter(|text| text.contains(line)).count(),
                1,
                "{frame:#?}"
            );
        }
    }
    console
        .select_created_chat(ROOT_AGENT, &created_banner)
        .unwrap();
    let frame = frame_lines(&console);
    assert!(line_with(&frame, "existing Agent body").is_some());
    assert!(line_with(&frame, "Tickloop v0.2.0").is_none());
    console.clear_visible_agent().unwrap();
    let frame = frame_lines(&console);
    assert!(line_with(&frame, "Tickloop v0.2.0").is_some());
    assert!(line_with(&frame, "current-model").is_some());
    for absent in [
        "original draft banner",
        "draft warning",
        "draft echo",
        "existing Agent body",
    ] {
        assert!(line_with(&frame, absent).is_none(), "{frame:#?}");
    }
    console.reset_chat(Uuid::from_u128(103)).unwrap();
    let frame = frame_lines(&console);
    for absent in [
        "original draft banner",
        "draft warning",
        "draft echo",
        "rebuilt public conversation",
    ] {
        assert!(line_with(&frame, absent).is_none(), "{frame:#?}");
    }
}

#[test]
fn agent_layout_creation_prefix_excludes_overview_and_model_tagged_output() {
    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    let startup = vec![("draft startup\n".into(), Some(Role::Title))];
    console.write_roles(&startup).unwrap();
    console.record_input("/model").unwrap();
    console.write("Model selection: Applied\n").unwrap();
    console
        .for_message(Some("hidden-model-output".into()))
        .write("hidden Agent content\n")
        .unwrap();
    console
        .set_global_overview(Some(dispatch_overview(Vec::new(), None)))
        .unwrap();
    console.record_input("overview dispatch draft").unwrap();
    console.write("overview feedback\n").unwrap();
    console.set_global_overview(None).unwrap();
    console.select_created_chat(CHILD_AGENT, &startup).unwrap();
    console.reset_chat(CHILD_AGENT).unwrap();
    let frame = frame_lines(&console);
    for line in ["draft startup", "/model", "Model selection: Applied"] {
        assert_eq!(
            frame.iter().filter(|text| text.contains(line)).count(),
            1,
            "{frame:#?}"
        );
    }
    for absent in [
        "hidden Agent content",
        "overview dispatch draft",
        "overview feedback",
    ] {
        assert!(line_with(&frame, absent).is_none(), "{frame:#?}");
    }
}

#[test]
fn agent_layout_reopened_banner_survives_rebuilds_and_scrolls_with_the_conversation() {
    for renderer in [renderer::Renderer::Inline, renderer::Renderer::Fullscreen] {
        let mut state = State::new(true, 100, 24);
        state.renderer = renderer;
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        let banner = vec![
            ("Tickloop v1.2.3\n".into(), Some(Role::Title)),
            ("fixture-model · Messages\n".into(), Some(Role::MutedText)),
            ("/fixture/conversation\n".into(), Some(Role::MutedText)),
        ];
        for _ in 0..2 {
            console.ensure_chat_banner(ROOT_AGENT, &banner).unwrap();
            console.reset_chat(ROOT_AGENT).unwrap();
            console
                .record_input("first message after reopening")
                .unwrap();
            console.write("the first reply\n").unwrap();
            let frame = frame_lines(&console);
            for text in [
                "Tickloop v1.2.3",
                "fixture-model · Messages",
                "/fixture/conversation",
            ] {
                assert_eq!(
                    frame.iter().filter(|line| line.contains(text)).count(),
                    1,
                    "{frame:#?}"
                );
            }
        }
        console
            .write(
                &(0..80)
                    .map(|index| format!("later conversation row {index}\n"))
                    .collect::<String>(),
            )
            .unwrap();
        let latest = frame_lines(&console);
        assert!(
            line_with(&latest, "Tickloop v1.2.3").is_none(),
            "the banner must not be pinned: {latest:#?}"
        );
        console
            .state
            .lock()
            .unwrap()
            .key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        let beginning = frame_lines(&console);
        assert_eq!(
            beginning
                .iter()
                .filter(|line| line.contains("Tickloop v1.2.3"))
                .count(),
            1,
            "{beginning:#?}"
        );
        assert!(line_with(&beginning, "/fixture/conversation").is_some());
        console.clear_visible_agent().unwrap();
        assert!(line_with(&frame_lines(&console), "/fixture/conversation").is_none());
    }
}

fn real_banner() -> Vec<(String, Option<Role>)> {
    super::super::startup_banner_spans(
        &tickloop_harness::test_support::config(),
        std::path::Path::new("/fixture/runtime-banner"),
    )
}

/// The frame line showing the banner's title.
fn title_line(console: &Console) -> String {
    let title = format!("Tickloop v{}", env!("CARGO_PKG_VERSION"));
    frame_lines(console)
        .into_iter()
        .find(|line| line.contains(&title))
        .unwrap()
}

#[test]
fn agent_layout_runtime_banner_uses_each_chats_known_worker_build_without_duplicates() {
    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    let banner = real_banner();
    console.ensure_chat_banner(ROOT_AGENT, &banner).unwrap();
    console.reset_chat(ROOT_AGENT).unwrap();
    console.record_input("keep this conversation").unwrap();
    for _ in 0..2 {
        console
            .set_agent_runtime(ROOT_AGENT, Some("0.2.11+aaaaaaaaaaaaaaaa".into()), true)
            .unwrap();
    }
    let old = frame_lines(&console).join("\n");
    assert_eq!(old.matches("Daemon: v").count(), 1, "{old}");
    assert!(
        title_line(&console).ends_with(" (Daemon: v0.2.11+aaaaaaaaaaaaaaaa, upgrade pending)"),
        "{old}"
    );
    assert!(old.contains("keep this conversation"));
    console.ensure_chat_banner(CHILD_AGENT, &banner).unwrap();
    console.reset_chat(CHILD_AGENT).unwrap();
    console.set_agent_runtime(CHILD_AGENT, None, true).unwrap();
    assert!(!frame_lines(&console).join("\n").contains("Daemon: v"));
    console
        .set_agent_runtime(ROOT_AGENT, Some("0.2.12+bbbbbbbbbbbbbbbb".into()), false)
        .unwrap();
    assert!(!frame_lines(&console).join("\n").contains("Daemon: v"));
    console.select_chat(Some(ROOT_AGENT)).unwrap();
    let updated = frame_lines(&console).join("\n");
    assert_eq!(updated.matches("Daemon: v").count(), 1, "{updated}");
    assert!(title_line(&console).ends_with(" (Daemon: v0.2.12+bbbbbbbbbbbbbbbb)"));
    assert!(!updated.contains("aaaaaaaaaaaaaaaa") && !updated.contains("pending"));
    assert!(updated.contains("keep this conversation"));
    console.reset_chat(ROOT_AGENT).unwrap();
    assert!(title_line(&console).ends_with(" (Daemon: v0.2.12+bbbbbbbbbbbbbbbb)"));
    console.set_agent_runtime(ROOT_AGENT, None, false).unwrap();
    let unknown = frame_lines(&console).join("\n");
    assert!(!unknown.contains("Daemon: v") && !unknown.contains("bbbbbbbbbbbbbbbb"));
    console.clear_visible_agent().unwrap();
    assert!(!frame_lines(&console).join("\n").contains("Daemon: v"));
}

#[test]
fn agent_layout_runtime_banner_updates_preserve_the_reading_anchor_and_full_build_identity() {
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .ensure_chat_banner(ROOT_AGENT, &real_banner())
        .unwrap();
    console.reset_chat(ROOT_AGENT).unwrap();
    console
        .write(
            &(0..80)
                .map(|index| format!("history row {index:03}\n"))
                .collect::<String>(),
        )
        .unwrap();
    console
        .state
        .lock()
        .unwrap()
        .scrollback
        .scroll(ScrollAction::First);
    frame_lines(&console);
    console
        .state
        .lock()
        .unwrap()
        .scrollback
        .scroll(ScrollAction::LinesDown(20));
    let reading = frame_lines(&console);
    assert!(reading.iter().any(|line| line.contains("history row")));
    for (build, pending) in [
        (Some("0.2.12+aaaaaaaaaaaaaaaa"), true),
        (Some("0.2.12+bbbbbbbbbbbbbbbb"), true),
        (Some("0.2.12+bbbbbbbbbbbbbbbb"), false),
        (None, false),
    ] {
        console
            .set_agent_runtime(ROOT_AGENT, build.map(str::to_owned), pending)
            .unwrap();
        assert_eq!(
            frame_lines(&console),
            reading,
            "a banner update must not move the reading viewport"
        );
    }
    console
        .set_agent_runtime(ROOT_AGENT, Some("0.2.12+bbbbbbbbbbbbbbbb".into()), false)
        .unwrap();
    console
        .state
        .lock()
        .unwrap()
        .scrollback
        .scroll(ScrollAction::First);
    let beginning = frame_lines(&console).join("\n");
    assert_eq!(beginning.matches("Daemon: v").count(), 1, "{beginning}");
    assert!(title_line(&console).ends_with(" (Daemon: v0.2.12+bbbbbbbbbbbbbbbb)"));
    assert!(!beginning.contains("aaaaaaaaaaaaaaaa") && !beginning.contains("pending"));
}

#[test]
fn agent_layout_runtime_version_follows_the_title_and_keeps_the_banner_rows() {
    let banner = real_banner();
    let banner_text = banner
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<String>();
    let banner_rows = banner_text.lines().map(str::trim_end).collect::<Vec<_>>();
    let title_row = banner_rows
        .iter()
        .position(|row| row.contains("Tickloop v"))
        .unwrap();
    for created in [false, true] {
        let mut state = State::new(true, 100, 32);
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        if created {
            console.write_roles(&banner).unwrap();
            console.record_input("first message").unwrap();
            console.select_created_chat(ROOT_AGENT, &banner).unwrap();
        } else {
            console.ensure_chat_banner(ROOT_AGENT, &banner).unwrap();
            console.reset_chat(ROOT_AGENT).unwrap();
            console.record_input("first message").unwrap();
        }
        for build in ["0.2.11+aaaaaaaaaaaaaaaa", "0.2.12+bbbbbbbbbbbbbbbb"] {
            console
                .set_agent_runtime(ROOT_AGENT, Some(build.into()), true)
                .unwrap();
            let frame = frame_lines(&console);
            let start = frame
                .iter()
                .position(|line| line.trim_end() == banner_rows[0])
                .unwrap();
            let mut expected = banner_rows
                .iter()
                .map(|row| (*row).to_owned())
                .collect::<Vec<_>>();
            expected[title_row] = format!(
                "{} (Daemon: v{build}, upgrade pending)",
                banner_rows[title_row]
            );
            assert_eq!(
                frame[start..start + banner_rows.len()]
                    .iter()
                    .map(|line| line.trim_end().to_owned())
                    .collect::<Vec<_>>(),
                expected,
                "the logo and banner rows stay together, the build after the title: {frame:#?}"
            );
            assert_eq!(
                frame
                    .iter()
                    .filter(|line| line.contains("Daemon: v"))
                    .count(),
                1
            );
        }
        console.set_agent_runtime(ROOT_AGENT, None, false).unwrap();
        assert!(
            !frame_lines(&console)
                .iter()
                .any(|line| line.contains("Daemon: v"))
        );
    }
}

#[test]
fn agent_layout_draft_overview_refresh_filters_and_heading_folds_preserve_navigation() {
    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    let projection = AgentOverview {
        rows: vec![agent_row(ROOT_AGENT, None, "another Agent")],
        dispatch_directory: PathBuf::from("/fixture/dispatch"),
        created: None,
        visible: None,
    };
    console
        .set_global_overview(Some(projection.clone()))
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.overview.selected, Some(OverviewTarget::CurrentDraft));
        state.key(key(KeyCode::Char('p')));
        state.key(key(KeyCode::Char('w')));
        assert_eq!(state.overview.selected, Some(OverviewTarget::CurrentDraft));
        state.key(key(KeyCode::Down));
        assert_eq!(state.overview.selected_agent(), Some(ROOT_AGENT));
    }
    console
        .set_global_overview(Some(projection.clone()))
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.overview.selected_agent(), Some(ROOT_AGENT));
        state.key(key(KeyCode::Up));
        state.key(key(KeyCode::Up));
        assert_eq!(
            state.overview.selected,
            Some(OverviewTarget::Heading(AttentionGroup::NeedsInput))
        );
        state.key(key(KeyCode::Char(' ')));
        assert!(state.key(key(KeyCode::Enter)).is_none());
        assert_eq!(
            state.overview.selected,
            Some(OverviewTarget::Heading(AttentionGroup::NeedsInput))
        );
        state.key(key(KeyCode::Down));
        assert_eq!(state.overview.selected, Some(OverviewTarget::CurrentDraft));
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::CloseOverview)
        ));
    }
    console.set_global_overview(None).unwrap();
    console.select_chat(Some(ROOT_AGENT)).unwrap();
    console.set_global_overview(Some(projection)).unwrap();
    assert!(!console.state.lock().unwrap().overview.current_draft);
    assert!(line_with(&frame_lines(&console), "current session").is_none());
}

#[test]
fn agent_layout_current_draft_keeps_its_name_and_return_cue_at_supported_sizes() {
    for (width, height) in [(25, 16), (41, 24), (81, 24), (121, 48)] {
        let mut state = State::new(true, width, height);
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        console
            .set_global_overview(Some(AgentOverview {
                rows: Vec::new(),
                dispatch_directory: PathBuf::from("/fixture/draft"),
                created: None,
                visible: None,
            }))
            .unwrap();
        let frame = frame_lines(&console);
        let draft = line_with(&frame, &format!("{SELECTED_MARK} ◆ New Agent")).unwrap();
        assert!(draft.contains("current"), "{frame:#?}");
        if width >= 41 {
            assert!(draft.contains("current session"), "{frame:#?}");
        }
        if width >= 81 {
            assert!(draft.contains("send a prompt to start"), "{frame:#?}");
        }
        assert!(
            frame
                .iter()
                .all(|line| display_width(line) < usize::from(width)),
            "{frame:#?}"
        );
        assert!(frame.len() <= usize::from(height));
        assert!(matches!(
            console.state.lock().unwrap().key(key(KeyCode::Right)),
            Some(InputEvent::CloseOverview)
        ));
    }
}

#[test]
fn agent_layout_new_draft_is_the_overview_return_target() {
    for return_key in [KeyCode::Enter, KeyCode::Right] {
        let mut state = State::new(true, 100, 32);
        state.prompt_enabled = true;
        state.history_project = PathBuf::from("/fixture/current-draft");
        state
            .status_area
            .update("Auto".into(), "draft-model".into(), None, None);
        assert!(matches!(
            state.key(key(KeyCode::Left)),
            Some(InputEvent::OpenOverview)
        ));
        let console = super::tests::console_with_state(state);
        let projection = AgentOverview {
            rows: vec![agent_row(ROOT_AGENT, None, "another Agent")],
            dispatch_directory: PathBuf::from("/fixture/dispatch"),
            created: None,
            visible: None,
        };
        console
            .set_global_overview(Some(projection.clone()))
            .unwrap();
        let frame = frame_lines(&console);
        assert!(line_with(&frame, "Needs input").is_some(), "{frame:#?}");
        let draft = line_with(&frame, "New Agent").unwrap();
        assert!(draft.contains("current session"), "{frame:#?}");
        assert!(draft.contains("send a prompt to start"), "{frame:#?}");
        assert!(draft.starts_with(SELECTED_MARK), "{frame:#?}");
        assert_eq!(
            console
                .state
                .lock()
                .unwrap()
                .overview
                .projection
                .as_ref()
                .unwrap()
                .rows
                .len(),
            1
        );
        console.set_global_overview(Some(projection)).unwrap();
        let mut state = console.state.lock().unwrap();
        assert!(matches!(
            state.key(key(return_key)),
            Some(InputEvent::CloseOverview)
        ));
        state.insert("separate dispatch draft");
        drop(state);
        console.set_global_overview(None).unwrap();
        let state = console.state.lock().unwrap();
        assert!(state.chat.is_none());
        assert!(state.editor.draft().is_empty());
        assert_eq!(
            state.history_project,
            PathBuf::from("/fixture/current-draft")
        );
        drop(state);
        assert!(line_with(&frame_lines(&console), "draft-model").is_some());
    }
}

#[test]
fn agent_layout_overview_context_updates_keep_the_dispatch_draft_and_source_focus() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.chat = Some(ROOT_AGENT);
    state.insert("parent draft");
    state.help_open = true;
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(AgentOverview {
            rows: vec![agent_row(ROOT_AGENT, None, "root")],
            dispatch_directory: PathBuf::from("/fixture/launch"),
            created: None,
            visible: None,
        }))
        .unwrap();
    console.state.lock().unwrap().insert("dispatch draft");
    console
        .set_input_context(Some(ROOT_AGENT), Path::new("/fixture/other"), &[])
        .unwrap();
    assert_eq!(
        console.state.lock().unwrap().history_project,
        PathBuf::from("/fixture/launch")
    );
    assert_eq!(
        console.state.lock().unwrap().editor.draft(),
        "dispatch draft"
    );
    assert!(!console.can_present_request().unwrap());
    console.set_global_overview(None).unwrap();
    let state = console.state.lock().unwrap();
    assert_eq!(state.editor.draft(), "parent draft");
    assert!(state.help_open);
}

#[test]
fn agent_layout_visible_family_summary_remains_navigable_after_footer_reset() {
    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.activate_agent(ROOT_AGENT).unwrap();
    console
        .update_status_area("Auto".into(), "fixture".into(), None, None)
        .unwrap();
    console
        .set_agent_family(
            vec![
                agent_row(ROOT_AGENT, None, "parent work"),
                agent_row(CHILD_AGENT, Some(ROOT_AGENT), "child work"),
            ],
            None,
        )
        .unwrap();
    console.set_footer_items(Vec::new()).unwrap();
    let mut state = console.state.lock().unwrap();
    state.insert("keep parent draft");
    state.key(key(KeyCode::Down));
    let frame = state.frame().0;
    assert!(
        frame.iter().any(|line| line.contains("Subagents 1")),
        "{frame:?}"
    );
    assert!(
        frame.iter().any(|line| line.contains("child work")),
        "{frame:?}"
    );
    assert_eq!(state.editor.draft(), "keep parent draft");
}

#[test]
fn agent_layout_required_connection_notice_borrows_one_row_without_changing_configuration() {
    for height in [4, 12, 24, 48] {
        let mut state = State::new(true, 40, height);
        state.prompt_enabled = true;
        state
            .status_area
            .update("Ask".into(), "fixture".into(), None, task_board());
        set_status_family(&mut state.status_area);
        state
            .footer
            .update(state.status_area.available_panels().collect::<Vec<_>>());
        let console = super::tests::console_with_state(state);
        console.set_status_lines(Some(Vec::new())).unwrap();
        console
            .set_connection_notice(Some("Disconnected · cached Agent".into()))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        let frame = state.frame().0;
        assert!(
            frame.iter().any(|line| line.contains("Disconnected")),
            "{frame:?}"
        );
        assert!(
            frame.iter().any(|line| line.contains("Subagents")),
            "{frame:?}"
        );
        assert!(
            frame.len() <= agent_layout::bottom_capacity(usize::from(height)),
            "{frame:?}"
        );
        assert_eq!(state.configured_status_lines, Some(Vec::new()));
        drop(state);
        console.set_connection_notice(None).unwrap();
        let mut state = console.state.lock().unwrap();
        assert!(
            !state
                .frame()
                .0
                .iter()
                .any(|line| line.contains("Disconnected"))
        );
        assert_eq!(state.configured_status_lines, Some(Vec::new()));
    }
}

#[test]
fn agent_layout_history_shortcut_is_confined_to_the_overview_list() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.overview.update(Some(AgentOverview {
        rows: vec![agent_row(ROOT_AGENT, None, "root")],
        dispatch_directory: PathBuf::from("/fixture/launch"),
        created: None,
        visible: None,
    }));
    assert!(
        matches!(state.key(key(KeyCode::Char('r'))), Some(InputEvent::Line(line)) if line == "/resume")
    );
    state.key(key(KeyCode::Tab));
    assert!(state.key(key(KeyCode::Char('r'))).is_none());
    assert_eq!(state.editor.draft(), "r");
}

#[test]
fn agent_layout_status_configuration_uses_physical_rows_and_help_restores_them() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .set_status_lines(Some(vec![
            "status-one".into(),
            "status-two".into(),
            "status-three".into(),
            "hidden-four".into(),
        ]))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let frame = state.frame().0;
    assert_eq!(frame.len(), 7);
    assert!(frame[5].contains("status-three"));
    assert!(frame[5].ends_with('…'));
    assert!(
        frame.last().unwrap().contains("? help"),
        "the key hints stay below configured status lines: {frame:#?}"
    );
    assert!(!frame.iter().any(|line| line.contains("hidden-four")));
    state.key(key(KeyCode::Char('?')));
    assert!(state.frame().0.iter().all(|line| !line.contains("status-")));
    state.key(key(KeyCode::Esc));
    assert!(
        state
            .frame()
            .0
            .iter()
            .any(|line| line.contains("status-three"))
    );
    drop(state);
    console.set_status_lines(Some(Vec::new())).unwrap();
    assert_eq!(console.state.lock().unwrap().frame().0.len(), 4);
}

#[test]
fn agent_layout_medium_short_screens_compress_status_and_keep_dispatch_directory_tail() {
    for height in [16, 20] {
        let mut state = State::new(true, 40, height);
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        console
            .set_status_lines(Some(vec!["first".into(), "second".into(), "third".into()]))
            .unwrap();
        console
            .set_global_overview(Some(AgentOverview {
                rows: Vec::new(),
                dispatch_directory: PathBuf::from("/very/long/launch/directory/保留/最后项目"),
                created: None,
                visible: None,
            }))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        let frame = state.frame().0;
        assert!(
            frame
                .iter()
                .any(|line| line.starts_with("New Agent · cwd …") && line.ends_with("最后项目")),
            "{frame:?}"
        );
        assert!(
            frame
                .iter()
                .any(|line| line.contains("first") && line.ends_with('…')),
            "{frame:?}"
        );
        assert!(
            frame
                .iter()
                .all(|line| !line.contains("second") && !line.contains("third")),
            "{frame:?}"
        );
        assert!(frame.len() <= agent_layout::bottom_capacity(usize::from(height)));
    }
}

#[test]
fn agent_layout_completion_navigation_does_not_repurpose_text_jk() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.insert("/");
    assert!(state.completion.is_open());
    state.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    state.key(key(KeyCode::Char('j')));
    assert_eq!(state.editor.draft(), "/j");
}

#[test]
fn agent_layout_short_candidate_window_keeps_the_selected_option_visible() {
    let mut state = State::new(true, 80, 12);
    state.prompt_enabled = true;
    state.insert("/");
    for _ in 0..12 {
        state.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    }
    let selected = state.completion.selected_index().unwrap();
    let syntax = state
        .completion
        .candidates()
        .nth(selected)
        .unwrap()
        .syntax();
    assert!(state.frame().0.iter().any(|line| line.contains(&syntax)));
}

#[test]
fn agent_layout_late_file_index_updates_its_cached_owner_without_changing_the_visible_input() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("parent-only.txt"), "parent").unwrap();
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.switch_view_chat(Some(ROOT_AGENT));
    state.insert("@parent");
    let owner = state.input_owner;
    let generation = state.file_mentions.request_index_generation();
    state.switch_view_chat(Some(CHILD_AGENT));
    state.insert("child draft");
    state.file_mentions.request_index_generation();
    let index = FileMentionIndex::build(directory.path(), &[]).unwrap();
    assert!(!state.replace_file_index(owner, generation, index));
    assert_eq!(state.editor.draft(), "child draft");
    assert!(state.file_mentions.candidates().is_empty());
    state.switch_view_chat(Some(ROOT_AGENT));
    assert_eq!(state.editor.draft(), "@parent");
    assert!(
        state
            .file_mentions
            .candidates()
            .iter()
            .any(|candidate| candidate.display_path.contains("parent-only.txt"))
    );
}

#[test]
#[ignore = "launched only by the isolated PTY layout test"]
fn agent_layout_pty_fixture() {
    if std::env::var("TICKLOOP_LAYOUT_PTY_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    let (console, mut input) = Console::start(true).unwrap();
    console.activate_agent(ROOT_AGENT).unwrap();
    console.set_prompt_enabled(true).unwrap();
    console
        .write(
            &(0..100)
                .map(|index| format!("history {index:02}\n"))
                .collect::<String>(),
        )
        .unwrap();
    console
        .update_status_area("Ask".into(), "test model".into(), Some((20, 100)), None)
        .unwrap();
    console
        .set_agent_family(
            vec![agent_row(CHILD_AGENT, Some(ROOT_AGENT), "child one")],
            None,
        )
        .unwrap();
    console
        .set_activity(Some(&[(
            "Working fixture".into(),
            Some(Role::RunningShade0),
        )]))
        .unwrap();
    while let Some(event) = input.blocking_recv() {
        match event {
            InputEvent::Prompt { text, .. } if text == "turns" => {
                console.record_input("earlier question").unwrap();
                console
                    .write(
                        &(0..100)
                            .map(|index| format!("earlier reply {index:02}\n"))
                            .collect::<String>(),
                    )
                    .unwrap();
                console.record_input("latest question").unwrap();
                console
                    .write(
                        &(0..100)
                            .map(|index| format!("latest reply {index:02}\n"))
                            .collect::<String>(),
                    )
                    .unwrap();
            }
            InputEvent::Prompt { text, .. }
                if matches!(text.as_str(), "counted-overview" | "max-counted-overview") =>
            {
                let mut root = agent_row(ROOT_AGENT, None, "root one");
                root.approvals = 12_345_000;
                root.questions = 87_654_000;
                let mut other = agent_row(Uuid::from_u128(103), None, "other");
                other.approvals = 678;
                other.questions = 321;
                other.project = "project-b".into();
                other.workspace = Some("workspace-b".into());
                let rows = if text == "max-counted-overview" {
                    root.approvals = usize::MAX;
                    root.questions = usize::MAX;
                    vec![root]
                } else {
                    vec![root, other]
                };
                console
                    .set_status("Cached status from the previously displayed Agent")
                    .unwrap();
                console
                    .set_global_overview(Some(AgentOverview {
                        rows,
                        dispatch_directory: PathBuf::from("/fixture/launch"),
                        created: None,
                        visible: None,
                    }))
                    .unwrap();
            }
            InputEvent::Prompt { text, .. }
                if matches!(text.as_str(), "project-overview" | "same-name-overview") =>
            {
                let projects = if text == "same-name-overview" {
                    ["研究-alpha/shared-project", "研究-beta/shared-project"]
                } else {
                    ["研究-alpha/project-one", "研究-beta/project-two"]
                };
                let rows = projects
                    .into_iter()
                    .enumerate()
                    .map(|(index, project)| {
                        let mut row = agent_row(
                            Uuid::from_u128(100 + index as u128),
                            None,
                            if index == 0 { "alpha task" } else { "beta task" },
                        );
                        row.project = format!(
                            "/Users/someone/.herdr/worktrees/tickloop/long-common-parent-prefix/shared-nested-directory/another-shared-directory/{project}"
                        );
                        row.workspace = Some(format!("workspace-{index}"));
                        row.approvals = 1;
                        row.questions = 2;
                        row
                    })
                    .collect();
                console
                    .set_global_overview(Some(AgentOverview {
                        rows,
                        dispatch_directory: PathBuf::from("/fixture/launch"),
                        created: None,
                        visible: None,
                    }))
                    .unwrap();
            }
            InputEvent::Prompt { text, .. } if text == "completion-overview" => {
                console
                    .set_global_overview(Some(AgentOverview {
                        rows: vec![agent_row(ROOT_AGENT, None, "root one")],
                        dispatch_directory: std::env::current_dir().unwrap(),
                        created: None,
                        visible: None,
                    }))
                    .unwrap();
            }
            InputEvent::Prompt { text, .. } if text == "permission" => {
                console.set_request_layout(Some(REQUEST_ID)).unwrap();
                console
                    .show_commentable_menu(
                        Menu {
                            id: REQUEST_ID,
                            title: "Permission required · fixture".into(),
                            context: vec!["Requester: root fixture".into()],
                            options: vec!["Allow once".into(), "Reject request".into()],
                            selected: 0,
                            confirmation: true,
                        },
                        vec![1],
                    )
                    .unwrap();
            }
            InputEvent::Prompt { text, .. } if text == "repeat-empty-notice" => {
                for _ in 0..16 {
                    console.set_creation_notice(None).unwrap();
                }
                console.set_status("Empty notice refresh done").unwrap();
            }
            InputEvent::Prompt { text, .. } if text == "set-notice" => {
                console
                    .set_creation_notice(Some("Creation 42 unknown\nfixture".into()))
                    .unwrap();
                console.set_status("Notice baseline ready").unwrap();
            }
            InputEvent::Prompt { text, .. } if text == "repeat-sanitized-notice" => {
                for _ in 0..16 {
                    console
                        .set_creation_notice(Some("Creation 42 unknown␤fixture".into()))
                        .unwrap();
                }
                console.set_status("Same notice refresh done").unwrap();
            }
            InputEvent::Prompt { text, .. } if text == "quiet-status" => {
                console.set_status_lines(Some(Vec::new())).unwrap();
            }
            InputEvent::Prompt { text, .. } if text == "dispatch-overview" => {
                console
                    .set_global_overview(Some(dispatch_overview(dispatch_rows(false), None)))
                    .unwrap();
            }
            InputEvent::Prompt { text, .. } if text == "accept-dispatch" => {
                console
                    .set_global_overview(Some(dispatch_overview(
                        dispatch_rows(true),
                        Some(NEW_SHELL_AGENT),
                    )))
                    .unwrap();
            }
            InputEvent::Prompt { .. } => console.set_status("Draft submitted").unwrap(),
            InputEvent::OpenOverview => console
                .set_global_overview(Some(AgentOverview {
                    rows: vec![
                        agent_row(ROOT_AGENT, None, "root one"),
                        agent_row(CHILD_AGENT, Some(ROOT_AGENT), "child one"),
                    ],
                    dispatch_directory: PathBuf::from("/fixture/launch"),
                    created: None,
                    visible: None,
                }))
                .unwrap(),
            InputEvent::CloseOverview => console.set_global_overview(None).unwrap(),
            InputEvent::OpenAgent(agent) => {
                console.set_global_overview(None).unwrap();
                let restored = console.activate_agent(agent).unwrap();
                if !restored {
                    console.write("child conversation\n").unwrap();
                }
                console.set_status("Agent opened").unwrap();
            }
            InputEvent::Cancel { menu_id } => {
                assert_eq!(menu_id, REQUEST_ID);
                console.clear_menu(menu_id).unwrap();
                console.finish_menu_submission(menu_id).unwrap();
                console
                    .set_status("Request rejected · execution kept")
                    .unwrap();
            }
            InputEvent::Eof => break,
            _ => {}
        }
    }
}

struct PtyConsole {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<String>>,
    width: usize,
    height: usize,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl PtyConsole {
    fn start(width: u16, height: u16) -> Self {
        Self::start_in_directory(width, height, &std::env::current_dir().unwrap())
    }

    fn start_in_directory(width: u16, height: u16, directory: &Path) -> Self {
        Self::start_with(width, height, directory, &[])
    }

    fn start_with_editor(width: u16, height: u16, editor: &str) -> Self {
        Self::start_with(
            width,
            height,
            &std::env::current_dir().unwrap(),
            &[("VISUAL", editor)],
        )
    }

    fn start_with(width: u16, height: u16, directory: &Path, environment: &[(&str, &str)]) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--ignored",
            "--exact",
            "terminal::console::layout_tests::agent_layout_pty_fixture",
            "--nocapture",
        ]);
        command.env("TICKLOOP_LAYOUT_PTY_FIXTURE", "1");
        command.env("TICKLOOP_AGENT_TUI", "fullscreen");
        command.env("TERM", "xterm-256color");
        for (name, value) in environment {
            command.env(name, value);
        }
        command.cwd(directory);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&output);
        thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                captured
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&bytes[..count]));
            }
        });
        Self {
            child,
            writer,
            output,
            width: usize::from(width),
            height: usize::from(height),
            _master: pair.master,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn resize(&mut self, width: u16, height: u16) {
        self._master
            .resize(PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.width = usize::from(width);
        self.height = usize::from(height);
    }

    fn send_and_wait(&mut self, bytes: &[u8]) {
        let mark = self.output.lock().unwrap().len();
        self.send(bytes);
        let deadline = Instant::now() + PTY_TIMEOUT;
        while Instant::now() < deadline {
            if self.output.lock().unwrap()[mark..].contains("\x1b[?2026l") {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("PTY input produced no completed frame: {}", self.frame());
    }

    fn frame(&self) -> String {
        screen::visible_screen_with_selection(&self.output.lock().unwrap(), self.width, self.height)
    }

    fn wait_for(&self, label: &str, matches: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + PTY_TIMEOUT;
        while Instant::now() < deadline {
            let frame = self.frame();
            if matches(&frame) {
                return frame;
            }
            thread::sleep(Duration::from_millis(15));
        }
        panic!(
            "PTY {label} at {}×{}: {}",
            self.width,
            self.height,
            self.frame()
        );
    }
}

impl Drop for PtyConsole {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn agent_layout_real_pty_float_return_click_and_escape_preserve_the_draft() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut terminal = PtyConsole::start_in_directory(80, 24, directory.path());
    terminal.wait_for("initial working", |frame| frame.contains("Working fixture"));
    terminal.send_and_wait(b"keep this draft");
    terminal.send_and_wait(b"\x1b[1;5H");
    let reading = terminal.wait_for("reading hides working", |frame| {
        frame.contains("history 00")
            && frame.contains("Back to bottom")
            && !frame.contains("Working fixture")
    });
    let border_before = reading
        .lines()
        .position(|line| line.starts_with("──") && line.contains("Back to bottom"))
        .unwrap();
    terminal.resize(40, 32);
    let reading = terminal.wait_for("resize retains the reading anchor", |frame| {
        frame.contains("history 00")
            && frame.contains("Back to bottom")
            && frame.contains("keep this draft")
    });
    assert!(!reading.contains("Working fixture"), "{reading}");
    let row = reading
        .lines()
        .position(|line| line.contains("Back to bottom"))
        .unwrap();
    let column = reading
        .lines()
        .nth(row)
        .unwrap()
        .chars()
        .position(|character| character == '↓')
        .unwrap();
    let control = reading.lines().nth(row).unwrap();
    let label_width = display_width(control.trim_end().trim_matches('─'));
    let content_width = display_width(control.trim_end());
    assert!(column.abs_diff(content_width - column - label_width) <= 1);
    let divider_column = column + label_width + 1;
    terminal.send_and_wait(
        format!(
            "\x1b[<0;{divider_column};{}M\x1b[<0;{divider_column};{}m\x1b[D",
            row + 1,
            row + 1
        )
        .as_bytes(),
    );
    terminal.wait_for(
        "trailing divider does not activate the return control",
        |frame| {
            frame.contains("history 00")
                && frame.contains("Back to bottom")
                && frame.contains("keep this draft")
                && !frame.contains("Working fixture")
        },
    );
    terminal.send_and_wait(b"\x1b[C");
    terminal.send_and_wait(
        format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            column + 1,
            row + 1,
            column + 1,
            row + 1
        )
        .as_bytes(),
    );
    terminal.wait_for("click follows working without changing input", |frame| {
        frame.contains("Working fixture")
            && frame.contains("keep this draft")
            && !frame.contains("Back to bottom")
    });
    terminal.send_and_wait(b"\x1b[1;5H");
    terminal.wait_for("return control appears again", |frame| {
        frame.contains("Back to bottom")
    });
    terminal.send_and_wait(b"\x1b");
    let bottom = terminal.wait_for("Esc follows latest and preserves draft", |frame| {
        frame.contains("Working fixture")
            && frame.contains("keep this draft")
            && !frame.contains("Back to bottom")
    });
    assert!(
        bottom
            .lines()
            .position(|line| line.starts_with("──"))
            .unwrap()
            > border_before
    );
}

#[test]
fn agent_layout_real_pty_budget_and_focus_matrix() {
    for height in [24, 32, 48] {
        for width in [40, 80, 120] {
            let mut terminal = PtyConsole::start(width, height);
            terminal.wait_for("initial", |frame| {
                frame.contains("history 99")
                    && !frame.contains("Shell 20")
                    && frame.contains("child one")
                    && !frame.contains("main")
            });
            terminal.send(b"draftjk");
            terminal.wait_for("text jk", |frame| frame.contains("draftjk"));
            for index in 0..12 {
                terminal.send(format!("\x0aline-{index:02}").as_bytes());
            }
            let frame = terminal.wait_for("multi-line editor", |frame| frame.contains("line-11"));
            let rows = frame.lines().collect::<Vec<_>>();
            let borders = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| {
                    row.starts_with("──") || row.contains("Subagents ") && row.contains("─")
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            assert_eq!(borders.len(), 2, "{frame}");
            assert!(
                borders[1] - borders[0] - 1 <= agent_layout::editor_capacity(usize::from(height)),
                "{frame}"
            );
            assert!(
                usize::from(height) - borders[0]
                    <= agent_layout::bottom_capacity(usize::from(height)),
                "{frame}"
            );
            terminal.send(b"\r");
            terminal.wait_for("submit", |frame| frame.contains("Draft submitted"));
            terminal.send(b"\x1b[B");
            terminal.wait_for("Agents list focus", |frame| {
                frame.contains("child one") && frame.contains("enter open")
            });
            terminal.send_and_wait(b"\x1b");
            let output = Arc::clone(&terminal.output);
            terminal.wait_for("Esc restores the visible editor caret", |_| {
                let captured = output.lock().unwrap();
                captured.rfind("\x1b[?25h").is_some_and(|shown| {
                    captured
                        .rfind("\x1b[?25l")
                        .is_none_or(|hidden| shown > hidden)
                })
            });
            terminal.send(b"jk");
            let focused = terminal.wait_for("Esc restores literal editor input", |frame| {
                frame
                    .lines()
                    .any(|line| line.starts_with('❯') && line.contains("jk"))
            });
            assert!(focused.contains("child one"), "{focused}");
            terminal.send(b"\x15");
            terminal.wait_for("editor is cleared before help", |frame| {
                frame.lines().any(|line| line.trim() == "❯")
            });
            terminal.send(b"?");
            let frame = terminal.wait_for("help", |frame| frame.contains("ctrl+j newline"));
            assert!(
                !frame.contains("Shell 20") && !frame.contains("task 12"),
                "{frame}"
            );
            let newline_help = frame
                .lines()
                .find(|line| line.contains("ctrl+j newline"))
                .unwrap();
            if width == 80 {
                assert!(
                    display_width(newline_help) > usize::from(width / 2),
                    "{frame}"
                );
            } else if width == 120 {
                assert!(
                    display_width(newline_help) > usize::from(width * 2 / 3),
                    "{frame}"
                );
            } else {
                assert_eq!(newline_help.trim(), "ctrl+j newline", "{frame}");
            }
            terminal.send(b"\x1b");
            terminal.wait_for("help closed", |frame| {
                !frame.contains("Shell 20") && !frame.contains("ctrl+j newline")
            });
            terminal.send(b"\x1b[D");
            let overview = terminal.wait_for("overview", |frame| {
                frame.contains("Agents · local") && frame.contains("root one")
            });
            if width == 120 {
                assert!(overview.contains("project-a / workspace-a"), "{overview}");
            }
            terminal.send(b"\tnew dispatch");
            terminal.wait_for("dispatch draft", |frame| frame.contains("new dispatch"));
            terminal.send(b"\x1b");
            terminal.wait_for("overview back", |frame| {
                !frame.contains("Agents · local") && !frame.contains("new dispatch")
            });
            terminal.send(b"permission\r");
            terminal.wait_for("request", |frame| {
                frame.contains("Permission required") && frame.contains("Allow once")
            });
            terminal.send(b"\x1b");
            terminal.wait_for("deny request", |frame| {
                frame.contains("Request rejected") && !frame.contains("Permission required")
            });
        }
    }
}

#[test]
fn agent_layout_real_pty_overview_help_preserves_focus_and_text() {
    for height in [16, 24] {
        let mut terminal = PtyConsole::start(40, height);
        terminal.wait_for("initial", |frame| frame.contains("history 99"));
        terminal.send(b"\x1b[D");
        terminal.wait_for("overview", |frame| {
            frame.lines().any(|line| line.starts_with("Agents")) && frame.contains("root one")
        });
        terminal.send(b"?");
        let mut help = terminal.wait_for("overview list help", |frame| {
            frame.contains("p filter project")
        });
        for _ in 0..12 {
            terminal.send_and_wait(b"j");
            help.push_str(&terminal.frame());
        }
        for action in [
            "w filter Workspace",
            "tab list / new Agent",
            "enter / → open Agent",
            "enter dispatch (input)",
            "r open history",
            "/cwd PATH",
            "esc / ? close help",
        ] {
            assert!(help.contains(action), "{height}×40: {help}");
        }
        terminal.send(b"\x1b");
        terminal.wait_for("help returns to the selected overview row", |frame| {
            selected_row_contains(frame, "root one") && !frame.contains("esc / ? close help")
        });
        terminal.send(b"\t?");
        terminal.wait_for("empty dispatch help", |frame| {
            frame.contains("p filter project")
        });
        terminal.send(b"\x1b");
        terminal.wait_for("dispatch help closes", |frame| {
            !frame.contains("filter project") && !frame.contains("esc / ? close help")
        });
        terminal.send(b"literal?jk");
        terminal.wait_for("dispatch punctuation and jk stay text", |frame| {
            frame.contains("literal?jk") && !frame.contains("filter project")
        });
        terminal.send(b"\t?");
        terminal.wait_for("list help preserves the unsubmitted dispatch", |frame| {
            frame.contains("p filter project")
        });
        terminal.send(b"\x1b");
        terminal.wait_for("list help closes with the draft intact", |frame| {
            selected_row_contains(frame, "root one") && !frame.contains("filter project")
        });
        terminal.send(b"\t");
        terminal.wait_for("help restores dispatch focus and text", |frame| {
            frame.contains("literal?jk") && !frame.contains("filter project")
        });
        terminal.send(b"\x1b");
        terminal.wait_for("overview Esc returns to source", |frame| {
            !frame.contains("New Agent · cwd") && frame.contains("history 99")
        });
        terminal.send(b"ordinary?jk");
        terminal.wait_for("ordinary punctuation and jk stay text", |frame| {
            frame.contains("ordinary?jk")
        });
    }
}

#[test]
fn agent_layout_real_pty_narrow_overview_keeps_global_counts_and_help_reachable() {
    for height in [16, 20, 24] {
        let mut terminal = PtyConsole::start(40, height);
        terminal.wait_for("initial", |frame| frame.contains("history 99"));
        terminal.send(b"counted-overview\r");
        let frame = terminal.wait_for("counted overview", |frame| {
            frame.contains("New Agent · cwd")
        });
        assert!(frame.contains("approvals 12345678"), "{height}×40: {frame}");
        assert!(frame.contains("questions 87654321"), "{height}×40: {frame}");
        assert!(frame.contains("? help"), "{height}×40: {frame}");
        let root = frame
            .lines()
            .find(|line| line.contains("root one"))
            .unwrap();
        assert!(
            root.contains("approvals") && root.contains("questions"),
            "{root}"
        );
        assert!(
            !frame.contains("child one"),
            "the overview lists families: {frame}"
        );
        assert!(frame.contains("other"), "{height}×40: {frame}");
        terminal.send_and_wait(b"p");
        let filtered = terminal.frame();
        assert!(!filtered.contains("other"), "{height}×40: {filtered}");
        assert!(filtered.contains("approvals 12345678"), "{filtered}");
        assert!(filtered.contains("questions 87654321"), "{filtered}");
        terminal.send(b"?");
        let mut help = terminal.wait_for("narrow overview help", |frame| {
            frame.contains("p filter project")
        });
        for _ in 0..12 {
            terminal.send_and_wait(b"j");
            help.push_str(&terminal.frame());
        }
        assert!(help.contains("approvals 12345678"), "{help}");
        assert!(help.contains("questions 87654321"), "{help}");
        assert!(help.contains("enter / → open Agent"), "{help}");
        assert!(help.contains("tab list / new Agent"), "{help}");
        terminal.send(b"\x1b");
        terminal.wait_for("counts help closes within the overview", |frame| {
            frame.contains("New Agent · cwd") && !frame.contains("esc / ? close help")
        });
        terminal.send_and_wait(b"\t");
        let dispatch = terminal.frame();
        assert!(dispatch.contains("? help"), "{dispatch}");
        assert!(dispatch.contains("enter dispatch"), "{dispatch}");
    }
}

/// The root's two request types come from the real tree builder: its own question and its
/// child's approval. Narrow rows reserve 8 title columns beside the reading number that tells the
/// repeated title apart, the folded `+N` and both types; the state glyph, own status, child note,
/// request words, counts and finally the spaced delimiters give way, each only when the richer
/// form does not fit. A cut wide character may leave one reserved column beside the ellipsis
/// unused. At 24 columns the short number leaves room for the counts (`A1 Q1`). The same-title row
/// has no requests and keeps its existing status layout.

#[test]
fn agent_layout_compact_request_letters_are_explained_in_both_helps() {
    let mut session = State::new(true, 24, 32);
    session.prompt_enabled = true;
    session.key(key(KeyCode::Char('?')));
    let help = session.frame().0;
    assert!(
        help.iter()
            .any(|line| line.contains("A/Q approval/question")),
        "{help:#?}"
    );
    let mut asking = agent_row(ROOT_AGENT, None, "root one");
    asking.questions = 2;
    let mut state = State::new(true, 24, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(AgentOverview {
            rows: vec![asking],
            dispatch_directory: PathBuf::from("/fixture/launch"),
            created: None,
            visible: None,
        }))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    state.key(key(KeyCode::Char('?')));
    let help = state.frame().0;
    assert!(
        help.iter().any(|line| line.contains("A approvals 1")),
        "{help:#?}"
    );
    assert!(
        help.iter().any(|line| line.contains("Q questions 2")),
        "{help:#?}"
    );
}

#[test]
fn agent_layout_request_metadata_preserves_title_text_beside_the_reading_number() {
    let title = "研究财报";
    let mut root = agent_row(Uuid::from_u128(0x1a2b << 112), None, title);
    root.approvals = 1;
    root.questions = 1;
    root.project = "/fixture/project-with-a-long-path-to-the-research-directory".into();
    let mut duplicate = agent_row(Uuid::from_u128(0x9c8d << 112), None, title);
    duplicate.approvals = 0;
    for width in [80, 120] {
        let mut state = State::new(true, width, 24);
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        console
            .set_global_overview(Some(AgentOverview {
                rows: vec![root.clone(), duplicate.clone()],
                dispatch_directory: PathBuf::from("/fixture/launch"),
                created: None,
                visible: None,
            }))
            .unwrap();
        let frame = frame_lines(&console);
        let root = line_with(&frame, "研究财报 (1)").expect("the duplicate title needs its number");
        assert!(
            line_with(&frame, "研究财报 (2)").is_some(),
            "{width}: metadata must leave the eight-column title and its number: {frame:#?}"
        );
        assert!(
            root.contains("approvals 1") && root.contains("questions 1"),
            "{width}: {root}"
        );
    }
}

#[test]
fn agent_layout_real_pty_overview_help_reads_counts_that_exceed_the_heading() {
    let mut terminal = PtyConsole::start(40, 16);
    terminal.wait_for("initial", |frame| frame.contains("history 99"));
    terminal.send(b"max-counted-overview\r");
    let frame = terminal.wait_for("full request types remain discoverable", |frame| {
        frame.contains("Agents · approvals / questions · ? help")
    });
    assert!(frame.contains("root one"), "{frame}");
    terminal.send(b"?");
    let approvals = format!("approvals {}", usize::MAX);
    let questions = format!("questions {}", usize::MAX);
    terminal.wait_for("full numeric counts are readable in help", |frame| {
        frame.contains(&approvals) && frame.contains(&questions)
    });
}

#[test]
fn agent_layout_real_pty_long_project_paths_keep_distinct_tails_and_request_details() {
    for (command, project_tails) in [
        (
            "project-overview\r",
            ["研究-alpha/project-one", "研究-beta/project-two"],
        ),
        (
            "same-name-overview\r",
            ["研究-alpha/shared-project", "研究-beta/shared-project"],
        ),
    ] {
        for width in [80, 100, 120] {
            let mut terminal = PtyConsole::start(width, 32);
            terminal.wait_for("initial", |frame| frame.contains("history 99"));
            terminal.send(command.as_bytes());
            let frame = terminal.wait_for("long project overview", |frame| {
                frame.contains("New Agent · cwd /fixture/launch") && frame.contains("beta task")
            });
            for (title, project_tail) in ["alpha task", "beta task"].into_iter().zip(project_tails)
            {
                let row = frame.lines().find(|line| line.contains(title)).unwrap();
                // The directory that tells the projects apart stays readable; a cut may fall
                // inside the wide characters before it.
                let distinct_tail = project_tail.split_once('-').unwrap().1;
                assert!(row.contains(distinct_tail), "{width}×32: {frame}");
                assert!(row.contains('…'), "{width}×32: {frame}");
                assert!(row.contains("approvals 1"), "{width}×32: {frame}");
                assert!(row.contains("questions 2"), "{width}×32: {frame}");
                assert!(display_width(row) <= usize::from(width), "{row}");
                let metadata = row.rsplit(" · ").next().unwrap().trim();
                assert!(display_width(metadata) <= usize::from(width) / 3, "{row}");
            }
            terminal.send_and_wait(b"p");
            let alpha = terminal.frame();
            assert!(
                alpha.contains("alpha task") && !alpha.contains("beta task"),
                "{alpha}"
            );
            terminal.send_and_wait(b"p");
            let beta = terminal.frame();
            assert!(
                beta.contains("beta task") && !beta.contains("alpha task"),
                "{beta}"
            );
            terminal.send_and_wait(b"\t");
            assert!(
                terminal.frame().contains("New Agent · cwd /fixture/launch"),
                "{}",
                terminal.frame()
            );
        }
    }
}

#[test]
fn agent_layout_real_pty_project_filter_labels_keep_the_selected_path_tail() {
    for (command, project_tails, widths) in [
        (
            "project-overview\r",
            ["研究-alpha/project-one", "研究-beta/project-two"],
            vec![40, 80, 100, 120],
        ),
        (
            "same-name-overview\r",
            ["研究-alpha/shared-project", "研究-beta/shared-project"],
            vec![80, 100, 120],
        ),
    ] {
        for width in widths {
            let mut terminal = PtyConsole::start(width, 32);
            terminal.wait_for("initial", |frame| frame.contains("history 99"));
            terminal.send(command.as_bytes());
            terminal.wait_for("long project overview", |frame| {
                frame.contains("New Agent · cwd /fixture/launch") && frame.contains("beta task")
            });
            let mut labels = Vec::new();
            for (selected, hidden, project_tail) in [
                ("alpha", "beta", project_tails[0]),
                ("beta", "alpha", project_tails[1]),
            ] {
                terminal.send_and_wait(b"p");
                let frame = terminal.frame();
                assert!(
                    frame.lines().any(|line| line.contains(selected)
                        && (line.contains("A1 Q2")
                            || line.contains("approvals 1") && line.contains("questions 2"))),
                    "{frame}"
                );
                assert!(
                    !frame.lines().any(|line| line.contains(hidden)
                        && (line.contains("A1 Q2")
                            || line.contains("approvals 1") && line.contains("questions 2"))),
                    "{frame}"
                );
                let heading = frame
                    .lines()
                    .find(|line| line.starts_with("Project: "))
                    .unwrap();
                let project = heading
                    .strip_prefix("Project: ")
                    .unwrap()
                    .split(" · ")
                    .next()
                    .unwrap()
                    .trim();
                if width == 40 {
                    assert!(
                        project.ends_with(if selected == "alpha" { "ne" } else { "wo" }),
                        "{width}×32: {frame}"
                    );
                    assert!(heading.contains("? help"), "{frame}");
                } else {
                    assert!(project.contains(project_tail), "{width}×32: {frame}");
                    assert!(heading.contains("Workspace: all"), "{width}×32: {frame}");
                }
                assert!(project.contains('…'), "{width}×32: {frame}");
                assert!(display_width(heading) <= usize::from(width), "{heading}");
                assert!(frame.contains("? help"), "{frame}");
                assert!(
                    frame.contains("approvals 2") && frame.contains("questions 4"),
                    "{frame}"
                );
                assert!(frame.contains("New Agent · cwd /fixture/launch"), "{frame}");
                labels.push(project.to_owned());
                if width >= 80 {
                    let workspace_index = usize::from(selected == "beta");
                    for _ in 0..=workspace_index {
                        terminal.send_and_wait(b"w");
                    }
                    let filtered = terminal.frame();
                    let heading = filtered
                        .lines()
                        .find(|line| line.starts_with("Project: "))
                        .unwrap();
                    assert!(heading.contains(project_tail), "{filtered}");
                    assert!(
                        heading.contains(&format!("Workspace: workspace-{workspace_index}")),
                        "{filtered}"
                    );
                    assert!(
                        filtered.lines().any(|line| line.contains(selected)
                            && (line.contains("A1 Q2")
                                || line.contains("approvals 1") && line.contains("questions 2"))),
                        "{filtered}"
                    );
                    assert!(filtered.contains("? help"), "{filtered}");
                    assert!(
                        filtered.contains("New Agent · cwd /fixture/launch"),
                        "{filtered}"
                    );
                    for _ in workspace_index..2 {
                        terminal.send_and_wait(b"w");
                    }
                    assert!(
                        terminal.frame().contains("Workspace: all"),
                        "{}",
                        terminal.frame()
                    );
                }
            }
            assert_ne!(labels[0], labels[1]);
            terminal.send_and_wait(b"p");
            let reset = terminal.frame();
            assert!(reset.contains("Project: all"), "{reset}");
            assert!(reset.contains("alpha") && reset.contains("beta"), "{reset}");
        }
    }
}

#[test]
fn agent_layout_real_pty_overview_candidates_handle_escape_before_leaving() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("layout-candidate.txt"), "fixture").unwrap();
    for (width, height) in [(40, 16), (80, 24)] {
        let mut terminal = PtyConsole::start_in_directory(width, height, directory.path());
        terminal.wait_for("initial", |frame| frame.contains("history 99"));
        terminal.send(b"completion-overview\r");
        terminal.wait_for("completion overview", |frame| {
            frame.contains("New Agent · cwd")
        });
        terminal.send(b"\t!cat ./");
        terminal.wait_for("real shell path candidate", |frame| {
            frame.contains("layout-candidate.txt") && frame.contains("!\u{a0}cat ./")
        });
        terminal.send_and_wait(b"\x1b");
        let dismissed = terminal.frame();
        assert!(dismissed.contains("New Agent · cwd"), "{dismissed}");
        assert!(dismissed.contains("!\u{a0}cat ./"), "{dismissed}");
        assert!(!dismissed.contains("layout-candidate.txt"), "{dismissed}");
        terminal.send_and_wait(b"\x1b");
        terminal.wait_for("second Esc leaves the overview", |frame| {
            !frame.contains("New Agent · cwd") && frame.contains("history 99")
        });
        terminal.send(b"\x1b[D\t");
        terminal.wait_for("Shell dispatch draft survives returning", |frame| {
            frame.contains("New Agent · cwd") && frame.contains("!\u{a0}cat ./")
        });
    }
}

#[test]
fn agent_layout_real_pty_overview_candidates_accept_tab_before_switching_focus() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("layout-candidate.txt"), "fixture").unwrap();
    for (input, candidate, accepted) in [
        (
            "!cat ./l",
            "layout-candidate.txt",
            "!\u{a0}cat ./layout-candidate.txtjk",
        ),
        ("/cw", "/cwd", "/cwd jk"),
        (
            "Review @lay",
            "layout-candidate.txt",
            "Review @layout-candidate.txtjk",
        ),
    ] {
        let mut terminal = PtyConsole::start_in_directory(80, 24, directory.path());
        terminal.wait_for("initial", |frame| frame.contains("history 99"));
        terminal.send(b"completion-overview\r");
        terminal.wait_for("completion overview", |frame| {
            frame.contains("New Agent · cwd")
        });
        terminal.send(b"\t");
        terminal.send(input.as_bytes());
        terminal.wait_for("focused real candidate", |frame| frame.contains(candidate));
        terminal.send(b"\tjk");
        terminal.wait_for("Tab accepts and jk remains dispatch text", |frame| {
            frame.contains(accepted)
        });
        terminal.send_and_wait(b"\t");
        let list = terminal.frame();
        assert!(selected_row_contains(&list, "root one"), "{list}");
        assert!(list.contains("enter open"), "{list}");
        terminal.send(b"\t?");
        terminal.wait_for(
            "Tab without candidates restores dispatch editing",
            |frame| frame.contains(&format!("{accepted}?")),
        );
    }
}

#[test]
fn agent_layout_real_pty_short_screens_keep_request_actions_reachable() {
    for height in [4, 8, 12, 16] {
        let mut terminal = PtyConsole::start(40, height);
        terminal.wait_for("short ready", |frame| frame.contains("Ask"));
        terminal.send(b"jk");
        terminal.wait_for("short input", |frame| frame.contains("jk"));
        terminal.send(b"\r");
        terminal.wait_for("short send", |frame| frame.contains("Draft submitted"));
        terminal.send(b"permission\r");
        terminal.wait_for("short request", |frame| frame.contains("Allow once"));
        terminal.send(b"j");
        terminal.wait_for("short request list", |frame| {
            frame.contains("Reject request")
        });
        terminal.send(b"\x1b");
        terminal.wait_for("short request cancel", |frame| {
            frame.contains("Request rejected")
        });
    }
}

#[test]
fn agent_layout_real_pty_latest_user_message_is_pinned_only_above_the_viewport() {
    let mut terminal = PtyConsole::start(80, 24);
    terminal.wait_for("ready", |frame| frame.contains("history 99"));
    terminal.send(b"turns\r");
    let following = terminal.wait_for("latest reply", |frame| frame.contains("latest reply 99"));
    assert!(
        following
            .lines()
            .next()
            .unwrap()
            .contains("latest question")
    );
    terminal.send_and_wait(b"\x1b[5~");
    assert!(
        terminal
            .frame()
            .lines()
            .next()
            .unwrap()
            .contains("latest question")
    );
    for _ in 0..25 {
        terminal.send_and_wait(b"\x1b[5~");
        let frame = terminal.frame();
        if frame
            .lines()
            .skip(1)
            .any(|line| line.contains("latest question"))
        {
            assert!(
                !frame.lines().next().unwrap().contains("latest question"),
                "{frame}"
            );
        }
        if terminal
            .frame()
            .lines()
            .skip(1)
            .any(|line| line.contains("earlier question"))
        {
            break;
        }
    }
    let earlier = terminal.wait_for("earlier conversation", |frame| {
        frame
            .lines()
            .skip(1)
            .any(|line| line.contains("earlier question"))
    });
    assert!(!earlier.lines().next().unwrap().contains("latest question"));
    terminal.send_and_wait(b"\x1b[1;5F");
    terminal.wait_for("follow latest keeps pinned message", |frame| {
        frame.contains("latest reply 99")
            && frame.lines().next().unwrap().contains("latest question")
    });
}

const OLD_SHELL_AGENT: Uuid = Uuid::from_u128(0x937b_0000_0000_0000_0000_0000_0000_0001);
const NEW_SHELL_AGENT: Uuid = Uuid::from_u128(0x2c7e_0000_0000_0000_0000_0000_0000_0002);
const BRAVO_AGENT: Uuid = Uuid::from_u128(0x77b4_0000_0000_0000_0000_0000_0000_0003);

/// A root Agent whose turn was interrupted; Done stays expanded, so these rows are listed
/// without unfolding.
fn finished_row(id: Uuid, title: &str, project: &str) -> AgentRow {
    let mut row = agent_row(id, None, title);
    row.status = "Interrupted".into();
    row.state = LoopState::Interrupted;
    row.approvals = 0;
    row.group = AttentionGroup::Done;
    row.glyph = status_glyph(AttentionGroup::Done, false);
    row.project = project.into();
    row
}

fn dispatch_rows(include_new: bool) -> Vec<AgentRow> {
    let mut rows = vec![
        finished_row(OLD_SHELL_AGENT, "!pwd", "project-a"),
        finished_row(BRAVO_AGENT, "!printf bravo", "project-b"),
    ];
    if include_new {
        rows.insert(1, finished_row(NEW_SHELL_AGENT, "!pwd", "project-a"));
    }
    rows
}

fn dispatch_overview(rows: Vec<AgentRow>, created: Option<Uuid>) -> AgentOverview {
    AgentOverview {
        rows,
        dispatch_directory: PathBuf::from("/fixture/launch"),
        created,
        visible: None,
    }
}

fn frame_lines(console: &Console) -> Vec<String> {
    let mut state = console.state.lock().unwrap();
    state.draw(&mut std::io::sink()).unwrap();
    super::tests::selection_marked_screen(&state)
}

fn line_with<'a>(frame: &'a [String], needle: &str) -> Option<&'a String> {
    frame.iter().find(|line| line.contains(needle))
}

fn monitor_bravo_with_dispatch_draft(width: u16, height: u16) -> Console {
    let mut state = State::new(true, width, height);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(dispatch_overview(dispatch_rows(false), None)))
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Char('p')));
        state.key(key(KeyCode::Char('p')));
        assert_eq!(state.overview.project.as_deref(), Some("project-b"));
        state.key(key(KeyCode::Down));
        state.key(key(KeyCode::Down));
        assert_eq!(state.overview.selected_agent(), Some(BRAVO_AGENT));
        state.key(key(KeyCode::Tab));
        state.insert("next draft");
    }
    console
}

#[test]
fn agent_layout_duplicate_titles_gain_reading_numbers_in_any_row_order() {
    for width in [40, 80, 120] {
        let console = super::tests::console_with_state(State::new(true, width, 24));
        let mut reordered = dispatch_rows(true);
        reordered.reverse();
        for rows in [dispatch_rows(true), reordered] {
            console
                .set_global_overview(Some(dispatch_overview(rows, None)))
                .unwrap();
            let frame = frame_lines(&console);
            let mut state = console.state.lock().unwrap();
            for (agent, number) in [(NEW_SHELL_AGENT, "!pwd (1)"), (OLD_SHELL_AGENT, "!pwd (2)")] {
                select_agent(&mut state, agent);
                let selected = drawn(&mut state)
                    .into_iter()
                    .find(|line| line.starts_with(SELECTED_MARK))
                    .unwrap();
                assert!(selected.contains(number), "{width}: {selected}: {frame:#?}");
            }
            assert!(
                !line_with(&frame, "!printf bravo").unwrap().contains('('),
                "a unique title needs no number: {frame:#?}"
            );
            assert!(!frame.iter().any(|line| has_hex_token(line)), "{frame:#?}");
        }
    }
}

#[test]
fn agent_layout_reading_numbers_cover_identical_and_cut_titles() {
    let console = super::tests::console_with_state(State::new(true, 80, 24));
    let sharing = |suffix: u128| Uuid::from_u128((0x1_2345_6789_u128 << 92) | (suffix << 88) | 7);
    console
        .set_global_overview(Some(dispatch_overview(
            vec![
                finished_row(sharing(0xa), "!pwd", "project-a"),
                finished_row(sharing(0xb), "!pwd", "project-a"),
                finished_row(sharing(0xc), "!printf unique", "project-a"),
            ],
            None,
        )))
        .unwrap();
    let frame = frame_lines(&console);
    assert!(line_with(&frame, "!pwd (1)").is_some(), "{frame:#?}");
    assert!(line_with(&frame, "!pwd (2)").is_some(), "{frame:#?}");
    assert!(!line_with(&frame, "!printf unique").unwrap().contains('('));
    assert!(!frame.iter().any(|line| has_hex_token(line)), "{frame:#?}");

    let drift = "Investigate quarterly alpha revenue drift in the ledger";
    let variance = "Investigate quarterly alpha revenue variance in the ledger";
    let decline = "Investigate quarterly alpha revenue decline in the ledger";
    let arriving = Uuid::from_u128(0x1000);
    for (width, numbers_expected) in [(40, true), (120, false)] {
        let console = super::tests::console_with_state(State::new(true, width, 24));
        let overview = |rows: Vec<AgentRow>| dispatch_overview(rows, None);
        console
            .set_global_overview(Some(overview(vec![
                finished_row(OLD_SHELL_AGENT, drift, "project-a"),
                finished_row(NEW_SHELL_AGENT, variance, "project-a"),
            ])))
            .unwrap();
        let frame = frame_lines(&console);
        let rows = listed_rows(&frame, "Investigate quar");
        assert_eq!(rows.len(), 2, "{frame:#?}");
        assert_eq!(
            rows.iter().all(|line| line.contains(") ·")),
            numbers_expected,
            "{width} columns: {frame:#?}"
        );
        assert!(
            rows.iter()
                .all(|line| display_width(line) <= usize::from(width))
        );
        if !numbers_expected {
            continue;
        }
        // Different titles cut alike carry different numbers that survive a lower-identity
        // arrival and a removal; the cursor stays on its execution.
        let selected_number = |state: &mut State| {
            drawn(state)
                .into_iter()
                .find(|line| line.starts_with(SELECTED_MARK))
                .unwrap()
        };
        let mut state = console.state.lock().unwrap();
        select_agent(&mut state, NEW_SHELL_AGENT);
        assert!(selected_number(&mut state).contains("(1)"));
        drop(state);
        console
            .set_global_overview(Some(overview(vec![
                finished_row(arriving, decline, "project-a"),
                finished_row(OLD_SHELL_AGENT, drift, "project-a"),
                finished_row(NEW_SHELL_AGENT, variance, "project-a"),
            ])))
            .unwrap();
        let frame = frame_lines(&console);
        for number in ["(1)", "(2)", "(3)"] {
            assert_eq!(
                listed_rows(&frame, number).len(),
                1,
                "{number} names one row: {frame:#?}"
            );
        }
        console
            .set_global_overview(Some(overview(vec![
                finished_row(arriving, decline, "project-a"),
                finished_row(NEW_SHELL_AGENT, variance, "project-a"),
            ])))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        assert!(selected_number(&mut state).contains("(1)"));
        let frame = drawn(&mut state);
        assert!(line_with(&frame, "(3)").is_some(), "{frame:#?}");
        assert!(line_with(&frame, "(2)").is_none(), "{frame:#?}");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::OpenAgent(NEW_SHELL_AGENT))
        ));
    }
}

#[test]
fn agent_layout_created_row_and_cue_share_a_reading_number_when_titles_read_alike() {
    let console = super::tests::console_with_state(State::new(true, 40, 24));
    let overview = |rows: Vec<AgentRow>| dispatch_overview(rows, Some(NEW_SHELL_AGENT));
    let alpha = || {
        finished_row(
            OLD_SHELL_AGENT,
            "Check NVDA options expiry alpha",
            "project-a",
        )
    };
    let bravo = || {
        finished_row(
            NEW_SHELL_AGENT,
            "Check NVDA options expiry bravo",
            "project-a",
        )
    };
    let shared = |frame: &[String], number: &str| {
        let created_row = listed_rows(list_lines(frame), "Check NVDA")
            .into_iter()
            .find(|line| line.contains(number))
            .unwrap_or_else(|| panic!("created row must carry the cue number: {frame:#?}"));
        let cue = line_with(frame, "Created").unwrap();
        assert!(cue.contains(number), "{frame:#?}");
        assert!(display_width(created_row) <= 39 && display_width(cue) <= 39);
        assert!(!frame.iter().any(|line| has_hex_token(line)), "{frame:#?}");
    };
    console
        .set_global_overview(Some(overview(vec![alpha(), bravo()])))
        .unwrap();
    let frame = frame_lines(&console);
    shared(&frame, "(1)");
    assert!(
        listed_rows(list_lines(&frame), "(2)").len() == 1,
        "the other row reads apart too: {frame:#?}"
    );
    let arriving = Uuid::from_u128(0x1000);
    console
        .set_global_overview(Some(overview(vec![
            finished_row(arriving, "Check NVDA options expiry charlie", "project-a"),
            alpha(),
            bravo(),
        ])))
        .unwrap();
    shared(&frame_lines(&console), "(1)");
    console
        .set_global_overview(Some(overview(vec![
            finished_row(arriving, "Check NVDA options expiry charlie", "project-a"),
            bravo(),
        ])))
        .unwrap();
    let frame = frame_lines(&console);
    shared(&frame, "(1)");
    assert!(line_with(&frame, "(3)").is_some(), "{frame:#?}");
}

#[test]
fn agent_layout_creation_cue_names_a_filter_hidden_agent_without_moving_monitor_state() {
    let console = monitor_bravo_with_dispatch_draft(80, 24);
    console
        .set_global_overview(Some(dispatch_overview(
            dispatch_rows(false),
            Some(NEW_SHELL_AGENT),
        )))
        .unwrap();
    let frame = frame_lines(&console);
    assert!(
        line_with(&frame, "New Agent accepted · not listed yet").is_some(),
        "{frame:#?}"
    );
    assert!(line_with(&frame, "Created").is_none());
    console
        .set_global_overview(Some(dispatch_overview(
            dispatch_rows(true),
            Some(NEW_SHELL_AGENT),
        )))
        .unwrap();
    let frame = frame_lines(&console);
    let cue = line_with(&frame, "Created !pwd (2)").unwrap();
    assert!(cue.contains("hidden by Project filter"), "{frame:#?}");
    assert!(line_with(&frame, "Project: project-b").is_some());
    assert!(line_with(&frame, "New Agent · cwd /fixture/launch").is_some());
    assert!(
        !frame.iter().any(|line| row_title(line).starts_with("!pwd")),
        "the filtered list keeps showing only the monitored Agent: {frame:#?}"
    );
    {
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.overview.selected_agent(), Some(BRAVO_AGENT));
        assert_eq!(state.overview.project.as_deref(), Some("project-b"));
        assert!(state.overview.input_focused);
        assert_eq!(state.editor.draft(), "next draft");
        state.key(key(KeyCode::Tab));
        state.key(key(KeyCode::Char('p')));
        assert!(state.overview.project.is_none());
    }
    let frame = frame_lines(&console);
    assert!(
        line_with(&frame, "Created !pwd (2) · Interrupted").is_some(),
        "{frame:#?}"
    );
    assert!(line_with(&frame, "!pwd (2) · Interrupted · project-a").is_some());
    assert!(line_with(&frame, "!pwd (1) · Interrupted · project-a").is_some());
}

#[test]
fn agent_layout_creation_cue_uses_the_existing_status_row_at_supported_sizes() {
    let configurations: [Option<Vec<String>>; 3] = [
        None,
        Some(Vec::new()),
        Some(vec!["one".into(), "two".into(), "three".into()]),
    ];
    for (width, height) in [(40, 24), (80, 24), (120, 48)] {
        for configured in configurations.clone() {
            let console = monitor_bravo_with_dispatch_draft(width, height);
            console.set_status_lines(configured.clone()).unwrap();
            let before = frame_lines(&console);
            console
                .set_global_overview(Some(dispatch_overview(
                    dispatch_rows(true),
                    Some(NEW_SHELL_AGENT),
                )))
                .unwrap();
            let frame = frame_lines(&console);
            let label = format!("{width}×{height} {configured:?}: {frame:#?}");
            let cue = frame
                .iter()
                .find(|line| line.contains("Created !pwd (2)"))
                .unwrap_or_else(|| panic!("{label}"));
            assert!(cue.contains("hidden by"), "{label}");
            assert!(display_width(cue) <= usize::from(width), "{label}");
            let directory_row = |frame: &[String]| {
                frame
                    .iter()
                    .position(|line| line.starts_with("New Agent · cwd"))
                    .unwrap()
            };
            if configured.as_ref().is_none_or(|lines| !lines.is_empty()) {
                assert_eq!(directory_row(&frame), directory_row(&before), "{label}");
            }
            assert_eq!(frame.len(), usize::from(height), "{label}");
            {
                let mut state = console.state.lock().unwrap();
                assert_eq!(state.editor.draft(), "next draft", "{label}");
                state.key(key(KeyCode::Tab));
                state.key(key(KeyCode::Char('?')));
                assert!(state.help_open, "{label}");
            }
            assert!(
                line_with(&frame_lines(&console), "Created").is_none(),
                "{label}"
            );
            console.state.lock().unwrap().key(key(KeyCode::Esc));
            assert!(
                line_with(&frame_lines(&console), "Created !pwd (2)").is_some(),
                "{label}"
            );
        }
    }
}

#[test]
fn agent_layout_first_status_row_keeps_independent_creation_and_connection_facts() {
    for width in [40, 80] {
        for configured in [None, Some(Vec::new()), Some(vec!["custom status".into()])] {
            let console = monitor_bravo_with_dispatch_draft(width, 24);
            console.set_status_lines(configured.clone()).unwrap();
            console
                .set_global_overview(Some(dispatch_overview(
                    dispatch_rows(true),
                    Some(NEW_SHELL_AGENT),
                )))
                .unwrap();
            console
                .set_status(
                    "New Agent created; local connection unavailable · fixture attach failure",
                )
                .unwrap();
            let frame = frame_lines(&console);
            let cue = line_with(&frame, "Created").unwrap();
            assert!(cue.contains("(2)"), "{width}: {frame:#?}");
            assert!(
                cue.contains("attach unavailable")
                    || cue.contains("no attach")
                    || cue.contains("local connection unavailable"),
                "accepted creation and local attach failure must both be visible: {width}: {frame:#?}"
            );
            assert_eq!(frame.len(), 24);
            assert_eq!(console.state.lock().unwrap().editor.draft(), "next draft");
            console
                .set_connection_notice(Some(
                    "Disconnected · cached view · fixture connection failure".into(),
                ))
                .unwrap();
            let frame = frame_lines(&console);
            let combined = line_with(&frame, "Created").unwrap();
            assert!(
                combined.contains("(2)")
                    && (combined.contains("Disconnected") || combined.contains("Offline")),
                "all accepted and connection facts must fit: {width}: {frame:#?}"
            );
            assert!(
                combined.contains("attach unavailable")
                    || combined.contains("no attach")
                    || combined.contains("local connection unavailable"),
                "{width}: {frame:#?}"
            );

            console.set_status("").unwrap();
            console
                .set_connection_notice(Some(
                    "Disconnected · cached view · fixture connection failure".into(),
                ))
                .unwrap();
            let frame = frame_lines(&console);
            let cue = line_with(&frame, "Created").unwrap();
            assert!(
                cue.contains("(2)") && cue.contains("Disconnected") && cue.contains("cached"),
                "{width}: {frame:#?}"
            );

            console
                .set_global_overview(Some(dispatch_overview(dispatch_rows(false), None)))
                .unwrap();
            console
                .set_creation_notice(Some("Creation 00000000-0000-0000-0000-000000000042 unknown; inspect Agents, then /acknowledge-create 00000000-0000-0000-0000-000000000042 to allow another submission without replay".into()))
                .unwrap();
            let frame = frame_lines(&console);
            let notice = line_with(&frame, "Creation").unwrap();
            assert!(
                notice.contains("unknown")
                    && notice.contains("Disconnected")
                    && notice.contains("cached"),
                "{width}: {frame:#?}"
            );
            assert!(
                line_with(&frame, "Created").is_none(),
                "unknown is not accepted: {frame:#?}"
            );
            assert!(display_width(notice) < usize::from(width));
            assert_eq!(frame.len(), 24);
        }
    }
}

#[test]
fn agent_layout_configured_conversation_status_keeps_unknown_and_disconnected_facts() {
    let mut state = State::new(true, 40, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .update_status_area("Ask".into(), "fixture".into(), None, None)
        .unwrap();
    console
        .set_connection_notice(Some("Disconnected · cached view · fixture".into()))
        .unwrap();
    console.set_creation_notice(Some("Creation 00000000-0000-0000-0000-000000000042 unknown; inspect Agents before allowing another submission without replay".into())).unwrap();
    let frame = frame_lines(&console);
    let notice = line_with(&frame, "Disconnected").unwrap();
    assert!(
        notice.starts_with("Disconnected/cached · Creation unknown"),
        "{frame:#?}"
    );
}

#[test]
fn agent_layout_disconnected_creation_keeps_its_reading_number_at_forty_columns() {
    let console = super::tests::console_with_state(State::new(true, 40, 24));
    let old = Uuid::parse_str("12345678-9a00-0000-0000-000000000001").unwrap();
    let created = Uuid::parse_str("12345678-9b00-0000-0000-000000000002").unwrap();
    console
        .set_global_overview(Some(dispatch_overview(
            vec![
                finished_row(old, "!pwd", "project-a"),
                finished_row(created, "!pwd", "project-a"),
            ],
            Some(created),
        )))
        .unwrap();
    console
        .set_connection_notice(Some(
            "Disconnected · cached view · fixture connection failure".into(),
        ))
        .unwrap();
    let frame = frame_lines(&console);
    let cue = line_with(&frame, "Created").unwrap();
    assert!(
        cue.contains("!pwd (2)"),
        "the created row's readable label must survive: {frame:#?}"
    );
    assert!(
        cue.contains("cached") && (cue.contains("Disconnected") || cue.contains("Offline")),
        "{frame:#?}"
    );
    assert!(display_width(cue) <= 39);
    assert!(
        listed_rows(list_lines(&frame), "!pwd (2)").len() == 1,
        "{frame:#?}"
    );
    console
        .set_status("New Agent created; local connection unavailable · fixture attach failure")
        .unwrap();
    let frame = frame_lines(&console);
    let cue = line_with(&frame, "Created").unwrap();
    assert!(
        cue.contains("!pwd (2)") && cue.contains("Offline") && cue.contains("no attach"),
        "all three facts and the readable label must fit: {frame:#?}"
    );
    assert!(display_width(cue) <= 39);
    assert!(!frame.iter().any(|line| has_hex_token(line)), "{frame:#?}");
}

#[test]
fn agent_layout_filter_hints_follow_the_focused_overview_region() {
    let console = monitor_bravo_with_dispatch_draft(120, 48);
    console
        .set_global_overview(Some(dispatch_overview(
            dispatch_rows(true),
            Some(NEW_SHELL_AGENT),
        )))
        .unwrap();
    let frame = frame_lines(&console);
    assert!(
        line_with(&frame, "tab list · esc back").is_some(),
        "{frame:#?}"
    );
    assert!(
        line_with(&frame, "hidden by Project filter").is_some(),
        "{frame:#?}"
    );
    assert!(
        line_with(&frame, "p cycles").is_none(),
        "input focus types p: {frame:#?}"
    );
    assert!(
        line_with(&frame, "p/w filter").is_none(),
        "input focus types p/w: {frame:#?}"
    );
    assert!(
        line_with(&frame, "r history").is_none(),
        "input focus types r: {frame:#?}"
    );
    {
        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Char('p')));
        state.key(key(KeyCode::Char('w')));
        assert_eq!(state.editor.draft(), "next draftpw");
        assert_eq!(state.overview.project.as_deref(), Some("project-b"));
        state.key(key(KeyCode::Tab));
    }
    let frame = frame_lines(&console);
    assert!(line_with(&frame, "p/w filter").is_some(), "{frame:#?}");
    assert!(line_with(&frame, "r history").is_some(), "{frame:#?}");
    assert!(line_with(&frame, "p cycles").is_some(), "{frame:#?}");
    console.state.lock().unwrap().key(key(KeyCode::Char('p')));
    assert!(line_with(&frame_lines(&console), "Project: all").is_some());
    assert_eq!(console.state.lock().unwrap().editor.draft(), "next draftpw");
}

#[test]
fn agent_layout_overview_help_hides_inactive_header_hints_and_restores_list_focus() {
    let console = monitor_bravo_with_dispatch_draft(120, 48);
    {
        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Tab));
        state.key(key(KeyCode::Char('?')));
        assert!(state.help_open);
    }
    let frame = frame_lines(&console);
    let header = line_with(&frame, "Project:").unwrap();
    for hint in ["p/w filter", "r history", "tab new Agent", "esc back"] {
        assert!(!header.contains(hint), "help owns keys: {frame:#?}");
    }
    {
        let mut state = console.state.lock().unwrap();
        for code in [
            KeyCode::Char('p'),
            KeyCode::Char('w'),
            KeyCode::Char('r'),
            KeyCode::Tab,
        ] {
            assert!(state.key(key(code)).is_none());
        }
        assert_eq!(state.overview.project.as_deref(), Some("project-b"));
        assert!(state.overview.workspace.is_none());
        assert_eq!(state.overview.selected_agent(), Some(BRAVO_AGENT));
        assert_eq!(state.editor.draft(), "next draft");
        assert!(!state.overview.input_focused);
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(!state.help_open);
    }
    let frame = frame_lines(&console);
    let header = line_with(&frame, "Project:").unwrap();
    assert!(
        header.contains("p/w filter")
            && header.contains("r history")
            && header.contains("esc back"),
        "{frame:#?}"
    );
    assert!(matches!(
        console.state.lock().unwrap().key(key(KeyCode::Esc)),
        Some(InputEvent::CloseOverview)
    ));
}

#[test]
fn agent_layout_overview_help_with_a_leading_fact_offers_no_list_actions() {
    let console = monitor_bravo_with_dispatch_draft(80, 24);
    console
        .set_connection_notice(Some("Disconnected · cached view · fixture".into()))
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Tab));
        state.key(key(KeyCode::Char('?')));
        assert!(state.help_open);
    }
    let frame = frame_lines(&console);
    let fact = line_with(&frame, "Disconnected").expect("the fact keeps its row");
    for inactive in [
        "enter open",
        "tab new Agent",
        "? help",
        "enter dispatch",
        "↑ list",
    ] {
        assert!(
            !fact.contains(inactive),
            "help owns the keys, so {inactive} is not offered: {frame:#?}"
        );
    }
    assert!(
        frame
            .iter()
            .any(|line| line.contains("esc") && line.contains("close")),
        "help still says how to close it: {frame:#?}"
    );
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .key(key(KeyCode::Esc))
            .is_none()
    );
    let frame = frame_lines(&console);
    let fact = line_with(&frame, "Disconnected").expect("the fact keeps its row");
    assert!(
        fact.contains("? help"),
        "closing help brings the list actions back beside the fact: {frame:#?}"
    );
}

#[test]
fn agent_layout_creation_notice_is_cleared_without_erasing_connection_notices() {
    let console = monitor_bravo_with_dispatch_draft(80, 24);
    console
        .set_connection_notice(Some("View connection unavailable · fixture".into()))
        .unwrap();
    console
        .set_creation_notice(Some("Creation 42 unknown; inspect Agents".into()))
        .unwrap();
    let frame = frame_lines(&console);
    assert!(
        line_with(&frame, "Creation 42 unknown").is_some(),
        "{frame:#?}"
    );
    assert!(line_with(&frame, "View connection unavailable").is_some());
    assert!(line_with(&frame, "Created").is_none());
    console.set_creation_notice(None).unwrap();
    let frame = frame_lines(&console);
    assert!(
        line_with(&frame, "Creation 42 unknown").is_none(),
        "{frame:#?}"
    );
    assert!(line_with(&frame, "View connection unavailable").is_some());
}

#[test]
fn agent_layout_real_pty_accepted_dispatch_cue_keeps_the_monitored_filter_and_selection() {
    for (width, height, quiet) in [
        (40, 24, false),
        (80, 24, false),
        (80, 24, true),
        (120, 48, false),
    ] {
        let label = format!("{width}×{height} quiet={quiet}");
        let mut terminal = PtyConsole::start(width, height);
        terminal.wait_for("initial", |frame| frame.contains("history 99"));
        if quiet {
            terminal.send_and_wait(b"quiet-status\r");
        }
        terminal.send(b"dispatch-overview\r");
        terminal.wait_for("dispatch overview", |frame| {
            frame.contains("New Agent · cwd /fixture/launch") && frame.contains("!printf bravo")
        });
        terminal.send_and_wait(b"p");
        terminal.send_and_wait(b"p");
        let monitoring_bravo = |frame: &str| {
            frame
                .lines()
                .any(|line| line.starts_with("Project: ") && line.contains("-b · Workspace"))
        };
        terminal.wait_for("monitor bravo", monitoring_bravo);
        terminal.send_and_wait(b"\t");
        terminal.send(b"accept-dispatch\r");
        let frame = terminal.wait_for("visible accept cue", |frame| {
            frame
                .lines()
                .any(|line| line.contains("Created !pwd (2)") && line.contains("hidden by"))
        });
        eprintln!("Console PTY {label}: accepted hidden dispatch\n{frame}");
        assert!(monitoring_bravo(&frame), "{label}: {frame}");
        assert!(
            frame.contains("New Agent · cwd /fixture/launch"),
            "{label}: {frame}"
        );
        assert!(
            !frame
                .lines()
                .any(|line| row_title(line).starts_with("!pwd")),
            "{label}: {frame}"
        );
        assert!(
            frame
                .lines()
                .all(|line| display_width(line) <= usize::from(width))
        );
        terminal.send_and_wait(b"\x1b[A");
        let frame = terminal.wait_for("list focus keeps the monitored row", |frame| {
            frame.lines().any(|line| {
                line.starts_with(SELECTED_MARK) && row_title(line).starts_with("!printf bravo")
            })
        });
        assert!(frame.contains("Created !pwd (2)"), "{label}: {frame}");
        terminal.send_and_wait(b"p");
        let frame = terminal.wait_for("cleared filter lists both identical tasks", |frame| {
            frame.contains("Project: all")
                && frame
                    .lines()
                    .any(|line| line.ends_with("Created !pwd (2) · Interrupted"))
        });
        eprintln!("Console PTY {label}: filter cleared, created row identified\n{frame}");
        let tree = frame
            .lines()
            .filter(|line| row_title(line).starts_with("!pwd"))
            .collect::<Vec<_>>();
        assert_eq!(tree.len(), 2, "{label}: {frame}");
        assert!(
            tree.iter().any(|line| line.contains("!pwd (2)")),
            "{label}: {frame}"
        );
        assert!(
            tree.iter().any(|line| line.contains("!pwd (1)")),
            "{label}: {frame}"
        );
    }
}

#[test]
fn agent_layout_real_pty_unchanged_creation_notice_does_not_clear_or_repaint_the_view() {
    let mut terminal = PtyConsole::start(80, 24);
    terminal.wait_for("initial", |frame| frame.contains("history 99"));
    terminal.send(b"settle-notice-layout\r");
    terminal.wait_for("completion-status geometry is settled", |frame| {
        frame.contains("Draft submitted")
    });
    for (command, completion) in [
        ("repeat-empty-notice", "Empty notice refresh done"),
        ("repeat-sanitized-notice", "Same notice refresh done"),
    ] {
        if command == "repeat-sanitized-notice" {
            terminal.send(b"set-notice\r");
            terminal.wait_for("notice baseline", |frame| {
                frame.contains("Notice baseline ready")
            });
        }
        let mark = terminal.output.lock().unwrap().len();
        terminal.send(format!("{command}\r").as_bytes());
        let frame = terminal.wait_for("unchanged notice refreshed", |frame| {
            frame.contains(completion)
        });
        assert!(
            !frame.contains("Agents · local"),
            "test the ordinary Agent View: {frame}"
        );
        let painted = terminal.output.lock().unwrap()[mark..].to_owned();
        assert!(
            !painted.contains("\x1b[2J") && !painted.contains("history 99"),
            "unchanged notices must preserve line diff rendering: {painted:?}"
        );
    }
}

const ASKING_ROOT: Uuid = Uuid::from_u128(0x5100);
const FAILED_FAMILY: Uuid = Uuid::from_u128(0x5200);
const INTERRUPTED_FAMILY: Uuid = Uuid::from_u128(0x5300);
const RUNNING_CHILD: Uuid = Uuid::from_u128(0x5301);
const VISIBLE_EMPTY: Uuid = Uuid::from_u128(0x5f00);

fn fleet_row(
    id: Uuid,
    parent: Option<Uuid>,
    title: &str,
    (state, status): (LoopState, &str),
    group: AttentionGroup,
) -> AgentRow {
    let glyph = status_glyph(group, false);
    AgentRow {
        agent_id: id,
        parent_id: parent,

        title: title.into(),
        status: status.into(),
        state,
        project: "/work/fleet".into(),
        workspace: Some("w1".into()),
        approvals: 0,
        questions: 0,
        group,
        glyph,
        preview: None,
        updated_at: None,
        run_started_at: None,
        subagent: parent.map(|_| SubagentRegistration {
            child_chat_id: id.into(),
            handle: format!("child-{}", id.as_u128() % 1000),
            description: title.into(),
            created_at: tickloop_harness::base::private_fs::unix_time_milliseconds(),
            color: Some(AgentColor::ALL[(id.as_u128() % 8) as usize]),
        }),
    }
}

fn done_task(index: u128) -> Uuid {
    Uuid::from_u128(0x6000 + index)
}

fn empty_session(index: u128) -> Uuid {
    Uuid::from_u128(0x7000 + index)
}

/// A screenshot-scale fleet: many never-used sessions, many finished roots with children, and a
/// few families whose urgent member is a child.
fn fleet_overview() -> AgentOverview {
    let mut asking = fleet_row(
        ASKING_ROOT,
        None,
        "Research NVDA",
        (LoopState::WaitingQuestion, "Needs answer"),
        AttentionGroup::NeedsInput,
    );
    asking.questions = 1;
    let mut rows = vec![
        asking,
        fleet_row(
            Uuid::from_u128(0x5400),
            None,
            "Plan the hedge",
            (LoopState::WaitingPlan, "Plan review"),
            AttentionGroup::NeedsInput,
        ),
        fleet_row(
            FAILED_FAMILY,
            None,
            "Rebalance",
            (LoopState::Failed, "Failed"),
            AttentionGroup::Errors,
        ),
        fleet_row(
            INTERRUPTED_FAMILY,
            None,
            "Long scan",
            (LoopState::Running, "Running"),
            AttentionGroup::Working,
        ),
        fleet_row(
            Uuid::from_u128(0x5500),
            None,
            "Watch earnings",
            (LoopState::Queued, "Queued"),
            AttentionGroup::Working,
        ),
        fleet_row(
            Uuid::from_u128(0x5600),
            None,
            "!tail -f log",
            (LoopState::Idle, "Idle"),
            AttentionGroup::Idle,
        ),
    ];
    rows.extend((0..16).map(|index| {
        fleet_row(
            done_task(index),
            None,
            &format!("Done task {index:02}"),
            (LoopState::Interrupted, "Interrupted"),
            AttentionGroup::Done,
        )
    }));
    rows.extend((0..14).map(|index| {
        fleet_row(
            if index == 7 {
                VISIBLE_EMPTY
            } else {
                empty_session(index)
            },
            None,
            "New Agent",
            (LoopState::Idle, "Empty"),
            AttentionGroup::Empty,
        )
    }));
    AgentOverview {
        rows,
        dispatch_directory: PathBuf::from("/work/fleet"),
        created: None,
        visible: Some(VISIBLE_EMPTY),
    }
}

fn drawn(state: &mut State) -> Vec<String> {
    state.draw(&mut std::io::sink()).unwrap();
    super::tests::selection_marked_screen(state)
}

/// Group headings in screen order, without selection marker or fold glyph: `Done · 16`.
fn heading_order(frame: &[String]) -> Vec<String> {
    frame
        .iter()
        .filter_map(|line| {
            line.trim_start_matches([SELECTED_MARK, ' '])
                .strip_prefix("▸ ")
                .or_else(|| {
                    line.trim_start_matches([SELECTED_MARK, ' '])
                        .strip_prefix("▾ ")
                })
        })
        .filter(|line| {
            AttentionGroup::ALL
                .iter()
                .any(|group| line.starts_with(&format!("{} · ", group.label())))
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn agent_layout_dense_overview_folds_done_and_empty_without_hiding_attention() {
    let mut state = State::new(true, 100, 32);
    state.chat = Some(VISIBLE_EMPTY);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.set_global_overview(Some(fleet_overview())).unwrap();
    let frame = frame_lines(&console);
    assert_eq!(frame.len(), 32);
    assert_eq!(
        heading_order(&frame),
        [
            "Needs input · 2",
            "Errors · 1",
            "Working · 2",
            "Idle · 1",
            "Done · 16",
            "Empty · 14 · never used",
        ],
        "every group heading and the Done counter fit the first screen: {frame:#?}"
    );
    assert!(line_with(&frame, "Rebalance").unwrap().contains("Failed"));
    assert!(line_with(&frame, "Long scan").unwrap().contains("Running"));
    let asking = line_with(&frame, "Research NVDA").unwrap();
    assert!(asking.contains("questions 1"), "{asking}");
    let more = line_with(&frame, "more done").unwrap();
    let shown_done = frame
        .iter()
        .filter(|line| line.contains("Done task"))
        .count();
    assert!(shown_done < 16);
    assert!(
        more.contains(&format!("{} more done", 16 - shown_done)),
        "{more}"
    );

    let mut state = console.state.lock().unwrap();
    assert_eq!(
        state.overview.selected,
        Some(OverviewTarget::Agent(VISIBLE_EMPTY)),
        "entering from the empty session's conversation locates it"
    );
    let frame = drawn(&mut state);
    let listed_empty = listed_rows(list_lines(&frame), "New Agent");
    assert_eq!(
        listed_empty.len(),
        1,
        "only this View's own empty session stays listed: {frame:#?}"
    );
    assert!(listed_empty[0].contains("current"), "{frame:#?}");
    select_target(&mut state, OverviewTarget::Heading(AttentionGroup::Empty));
    assert!(state.key(key(KeyCode::Enter)).is_none());
    let frame = drawn(&mut state);
    assert!(line_with(&frame, "▾ Empty · 14").is_some(), "{frame:#?}");
    assert_eq!(
        state.overview.selected,
        Some(OverviewTarget::Heading(AttentionGroup::Empty))
    );
    state.key(key(KeyCode::Char(' ')));
    let frame = drawn(&mut state);
    assert!(line_with(&frame, "▸ Empty · 14").is_some(), "{frame:#?}");
    assert!(
        listed_rows(list_lines(&frame), "New Agent").is_empty(),
        "an explicit fold hides the current row too: {frame:#?}"
    );
    assert!(
        line_with(&frame, "Current New Agent (").is_some_and(|cue| cue.contains("collapsed")),
        "the status line still says where the current Agent is: {frame:#?}"
    );
}

fn select_agent(state: &mut State, id: Uuid) {
    select_target(state, OverviewTarget::Agent(id));
}

/// Selects `target` by real list navigation: up to the first line, then down until it is
/// selected. Fails instead of looping when the target is not listed.
fn select_target(state: &mut State, target: OverviewTarget) {
    const MAX_LINES: usize = 200;
    for _ in 0..MAX_LINES {
        if state.overview.selected == Some(target) {
            return;
        }
        state.key(key(KeyCode::Char('k')));
    }
    for _ in 0..MAX_LINES {
        if state.overview.selected == Some(target) {
            return;
        }
        state.key(key(KeyCode::Char('j')));
    }
    panic!("{target:?} never became selectable");
}

fn screen_row(frame: &[String], needle: &str) -> Option<usize> {
    frame.iter().position(|line| line.contains(needle))
}

#[test]
fn agent_layout_rows_arriving_above_keep_the_selection_on_its_screen_line() {
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.set_global_overview(Some(fleet_overview())).unwrap();
    let before = {
        let mut state = console.state.lock().unwrap();
        state.overview.opened_groups.insert(AttentionGroup::Done);
        select_agent(&mut state, done_task(9));
        let frame = drawn(&mut state);
        assert!(
            line_with(&frame, "Needs input").is_none(),
            "the selection is in a scrolled list: {frame:#?}"
        );
        screen_row(&frame, &format!("{SELECTED_MARK} ✖ Done task 09")).unwrap()
    };
    let mut arrived = fleet_overview();
    let mut asking = fleet_row(
        Uuid::from_u128(0x5700),
        None,
        "Approve the order preview",
        (LoopState::WaitingPermission, "Needs approval"),
        AttentionGroup::NeedsInput,
    );
    asking.approvals = 1;
    arrived.rows.insert(0, asking);
    console.set_global_overview(Some(arrived)).unwrap();
    let mut state = console.state.lock().unwrap();
    let frame = drawn(&mut state);
    assert_eq!(
        state.overview.selected,
        Some(OverviewTarget::Agent(done_task(9)))
    );
    assert_eq!(
        screen_row(&frame, &format!("{SELECTED_MARK} ✖ Done task 09")),
        Some(before),
        "{frame:#?}"
    );
    assert!(line_with(&frame, "folded").is_none() || frame[0].contains("folded"));

    state.width = 39;
    state.height = 16;
    let frame = drawn(&mut state);
    assert_eq!(
        state.overview.selected,
        Some(OverviewTarget::Agent(done_task(9)))
    );
    assert!(
        line_with(&frame, &format!("{SELECTED_MARK} ✖ Done task 09")).is_some(),
        "resize keeps the selection in view: {frame:#?}"
    );
    assert!(
        frame.iter().all(|line| display_width(line) <= 39),
        "{frame:#?}"
    );
}

#[test]
fn agent_layout_a_visited_row_that_finishes_stays_listed_while_the_cursor_moves_on() {
    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.set_global_overview(Some(fleet_overview())).unwrap();
    console
        .state
        .lock()
        .map(|mut state| select_agent(&mut state, INTERRUPTED_FAMILY))
        .unwrap();
    let mut finished = fleet_overview();
    let family = finished
        .rows
        .iter()
        .filter(|row| [INTERRUPTED_FAMILY, RUNNING_CHILD].contains(&row.agent_id))
        .cloned()
        .map(|mut row| {
            row.status = "Interrupted".into();
            row.state = LoopState::Interrupted;
            row.group = AttentionGroup::Done;
            row.glyph = status_glyph(AttentionGroup::Done, false);
            row
        })
        .collect::<Vec<_>>();
    finished
        .rows
        .retain(|row| ![INTERRUPTED_FAMILY, RUNNING_CHILD].contains(&row.agent_id));
    finished.rows.extend(family);
    console.set_global_overview(Some(finished)).unwrap();
    let mut state = console.state.lock().unwrap();
    let frame = drawn(&mut state);
    assert!(
        line_with(&frame, &format!("{SELECTED_MARK} ✖ Long scan")).is_some(),
        "{frame:#?}"
    );
    state.key(key(KeyCode::Char('k')));
    let frame = drawn(&mut state);
    assert!(
        line_with(&frame, "Long scan").is_some(),
        "the row just left stays listed during this visit: {frame:#?}"
    );
    drop(state);
    console.set_global_overview(None).unwrap();
    console.set_global_overview(Some(fleet_overview())).unwrap();
    let state = console.state.lock().unwrap();
    assert!(state.overview.projection.is_some());
}

#[test]
fn agent_layout_folded_counts_stay_visible_when_priority_groups_fill_the_list() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    let mut rows = (0..20)
        .map(|index| {
            fleet_row(
                Uuid::from_u128(0x8000 + index),
                None,
                &format!("Working task {index:02}"),
                (LoopState::Running, "Running"),
                AttentionGroup::Working,
            )
        })
        .collect::<Vec<_>>();
    rows.extend((0..5).map(|index| {
        fleet_row(
            done_task(index),
            None,
            &format!("Done task {index:02}"),
            (LoopState::Interrupted, "Interrupted"),
            AttentionGroup::Done,
        )
    }));
    rows.extend((0..3).map(|index| {
        fleet_row(
            empty_session(index),
            None,
            "New Agent",
            (LoopState::Idle, "Empty"),
            AttentionGroup::Empty,
        )
    }));
    console
        .set_global_overview(Some(AgentOverview {
            rows,
            dispatch_directory: PathBuf::from("/work/fleet"),
            created: None,
            visible: None,
        }))
        .unwrap();
    let frame = frame_lines(&console);
    assert!(
        frame[0].contains("folded +5 done +3 empty"),
        "folded counters below the list remain readable: {frame:#?}"
    );
    let mut state = console.state.lock().unwrap();
    for _ in 0..40 {
        state.key(key(KeyCode::Char('j')));
    }
    let frame = drawn(&mut state);
    assert!(line_with(&frame, "… 5 more done").is_some(), "{frame:#?}");
    assert!(!frame[0].contains("+5 done"), "{frame:#?}");
}

#[test]
fn agent_layout_a_fully_pinned_empty_group_reserves_no_counter_row_for_done() {
    let mut state = State::new(true, 100, 24);
    state.chat = Some(VISIBLE_EMPTY);
    let console = super::tests::console_with_state(state);
    let mut rows = vec![fleet_row(
        Uuid::from_u128(0x8000),
        None,
        "Working task",
        (LoopState::Running, "Running"),
        AttentionGroup::Working,
    )];
    rows.extend((0..30).map(|index| {
        fleet_row(
            done_task(index),
            None,
            &format!("Done task {index:02}"),
            (LoopState::Interrupted, "Interrupted"),
            AttentionGroup::Done,
        )
    }));
    rows.push(fleet_row(
        VISIBLE_EMPTY,
        None,
        "New Agent",
        (LoopState::Idle, "Empty"),
        AttentionGroup::Empty,
    ));
    console
        .set_global_overview(Some(AgentOverview {
            rows,
            dispatch_directory: PathBuf::from("/work/fleet"),
            created: None,
            visible: Some(VISIBLE_EMPTY),
        }))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let frame = drawn(&mut state);
    let dispatch = frame
        .iter()
        .position(|line| line.starts_with("New Agent · cwd"))
        .expect("the dispatch line");
    assert!(
        !frame[dispatch - 1].trim().is_empty(),
        "an Empty group whose rows are all pinned draws no counter, so Done uses that row: {frame:#?}"
    );
    assert!(line_with(&frame, "… 19 more done").is_some(), "{frame:#?}");
    assert!(line_with(&frame, "empty sessions").is_none(), "{frame:#?}");
}

#[test]
fn agent_layout_overview_never_routes_keys_to_a_surface_it_does_not_draw() {
    let root = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut history = InputHistory::open(root.path());
    history
        .record("overview-dispatch", project.path(), "earlier dispatch", &[])
        .unwrap();
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.input_history = Some(history);
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(AgentOverview {
            rows: vec![finished_row(BRAVO_AGENT, "!printf bravo", "project-b")],
            dispatch_directory: project.path().into(),
            created: None,
            visible: None,
        }))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    state.key(key(KeyCode::Tab));
    state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    state.key(key(KeyCode::Char('e')));
    let frame = drawn(&mut state);
    assert!(
        line_with(&frame, "Search: e").is_some(),
        "history search typed from the dispatch input is drawn: {frame:#?}"
    );
    state.key(key(KeyCode::Esc));
    assert!(state.history_search.is_none());
    assert!(line_with(&drawn(&mut state), "!printf bravo").is_some());

    state.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(
        state.transcript.is_none(),
        "the overview has no transcript to open"
    );
    state.insert("/details");
    let event = state.key(key(KeyCode::Enter));
    assert!(state.transcript.is_none());
    assert!(
        event.is_none() && state.status.contains("/details isn't available"),
        "the overview explains the command instead of opening a hidden transcript: {event:?}"
    );
    state.key(key(KeyCode::Char('x')));
    assert!(
        state.editor.draft().ends_with('x'),
        "typing reaches the visible dispatch input"
    );
}

fn candidate_texts(state: &State) -> Vec<String> {
    state
        .candidate_rows(40)
        .iter()
        .map(RoleLine::text)
        .collect()
}

fn submit(state: &mut State, text: &str) -> Option<InputEvent> {
    state.editor.apply(EditorCommand::Clear);
    state.insert(text);
    state.key(key(KeyCode::Enter))
}

#[test]
fn agent_layout_commands_show_and_enforce_their_scope_before_sending() {
    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(dispatch_overview(dispatch_rows(false), None)))
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Tab));
        state.insert("/");
        let candidates = candidate_texts(&state);
        assert!(
            candidates.iter().any(|line| line.contains("/cwd")),
            "{candidates:#?}"
        );
        assert!(
            candidates.iter().any(|line| line.contains("new Agent")),
            "{candidates:#?}"
        );
        assert!(
            !candidates.iter().any(|line| line.contains("/model")),
            "the overview lists only commands it handles: {candidates:#?}"
        );
        assert!(
            submit(&mut state, "/model").is_none(),
            "an Agent command never becomes a Skill dispatch"
        );
        assert_eq!(state.editor.draft(), "/model");
        assert!(state.status.contains("open Agent"), "{}", state.status);
        assert!(submit(&mut state, "/help").is_none());
        assert!(state.help_open, "/help opens the overview help");
        state.key(key(KeyCode::Esc));
        assert!(matches!(
            submit(&mut state, "/tickloop check the layout"),
            Some(InputEvent::Prompt { text, .. }) if text == "/tickloop check the layout"
        ));
        assert!(
            matches!(
                submit(&mut state, "/project-skill"),
                Some(InputEvent::Prompt { .. })
            ),
            "unknown names stay Skill input for the daemon to validate"
        );
    }
    console.set_global_overview(None).unwrap();
    console.activate_agent(CHILD_AGENT).unwrap();
    console
        .set_agent_family(
            vec![
                agent_row(ROOT_AGENT, None, "parent work"),
                agent_row(CHILD_AGENT, Some(ROOT_AGENT), "child work"),
            ],
            Some(CHILD_AGENT),
        )
        .unwrap();
    // The scope follows the daemon's delegation fact from the visible Agent's snapshot.
    console.set_visible_delegation(true).unwrap();
    let mut state = console.state.lock().unwrap();
    state.editor.apply(EditorCommand::Clear);
    state.insert("/cl");
    let candidates = candidate_texts(&state);
    assert!(
        !candidates.iter().any(|line| line.contains("/clear")),
        "a child offers no command it cannot run: {candidates:#?}"
    );
    assert!(submit(&mut state, "/clear").is_none());
    assert!(state.status.contains("child"), "{}", state.status);
    assert!(matches!(
        submit(&mut state, "/model"),
        Some(InputEvent::Prompt { text, .. }) if text == "/model"
    ));
    drop(state);
    console
        .set_agent_family(
            vec![
                agent_row(ROOT_AGENT, None, "parent work"),
                agent_row(CHILD_AGENT, Some(ROOT_AGENT), "child work"),
            ],
            Some(ROOT_AGENT),
        )
        .unwrap();
    console.set_visible_delegation(false).unwrap();
    let mut state = console.state.lock().unwrap();
    assert!(matches!(
        submit(&mut state, "/clear"),
        Some(InputEvent::Prompt { text, .. }) if text == "/clear"
    ));
}

fn hint_frame(state: &mut State) -> String {
    state.frame().0.join("\n")
}

#[test]
fn agent_layout_session_hints_name_the_action_of_the_focused_region() {
    let mut plain = State::new(true, 100, 32);
    plain.prompt_enabled = true;
    let empty = hint_frame(&mut plain);
    assert!(
        empty.contains("← Agents") && empty.contains("? help"),
        "{empty}"
    );
    plain.insert("draft");
    let typing = hint_frame(&mut plain);
    assert!(
        !typing.contains("← Agents") && !typing.contains("? help"),
        "Left and ? edit a non-empty draft: {typing}"
    );
    assert!(typing.contains("enter send"), "{typing}");

    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    state
        .status_area
        .update("Ask".into(), "test model".into(), None, task_board());
    set_status_family(&mut state.status_area);
    state
        .footer
        .update(state.status_area.available_panels().collect::<Vec<_>>());
    let editor = hint_frame(&mut state);
    assert!(
        !editor.contains("enter/space expand"),
        "Enter submits from the editor, so the summary cannot promise Enter: {editor}"
    );
    assert!(editor.contains("main"), "{editor}");
    assert!(
        editor.lines().last().unwrap().contains("? help"),
        "the key hints end the screen: {editor}"
    );
    assert!(state.footer.selected().is_none());
    state.insert("draft");
    state.key(key(KeyCode::Down));
    assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));
    let list = hint_frame(&mut state);
    let last = list.lines().last().unwrap();
    assert!(
        last.contains("enter open") && last.contains("esc input"),
        "the first Down enters the list, whose keys replace the input's: {list}"
    );
    assert!(!list.contains("enter/space hide"), "{list}");
    assert_eq!(state.editor.draft(), "draft");
}

#[test]
fn agent_layout_overview_hints_follow_the_selected_row_kind_and_draft() {
    let mut state = State::new(true, 100, 32);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.set_global_overview(Some(fleet_overview())).unwrap();
    let mut state = console.state.lock().unwrap();
    // `Workspace` in the filter also contains the lowercase key name.
    let offers_space = |hint: &str| hint.contains("/space") || hint.contains("· space");
    select_agent(&mut state, FAILED_FAMILY);
    let frame = drawn(&mut state);
    assert!(
        frame[1].contains("enter open") && !offers_space(&frame[1]),
        "{frame:#?}"
    );
    select_agent(&mut state, Uuid::from_u128(0x5500));
    let frame = drawn(&mut state);
    assert!(
        frame[1].contains("enter open") && !offers_space(&frame[1]),
        "{frame:#?}"
    );
    select_target(&mut state, OverviewTarget::Disclosure(AttentionGroup::Done));
    let frame = drawn(&mut state);
    assert!(frame[1].contains("enter/space show"), "{frame:#?}");
    assert!(
        frame
            .last()
            .is_some_and(|line| line.contains("enter/space show")),
        "{frame:#?}"
    );
    state.key(key(KeyCode::Tab));
    let frame = drawn(&mut state);
    assert!(
        frame.last().is_some_and(|line| line.contains("? help")),
        "{frame:#?}"
    );
    state.insert("draft");
    let frame = drawn(&mut state);
    let hint = frame.last().unwrap();
    assert!(
        !hint.contains("? help") && !hint.contains("Up list"),
        "{hint}"
    );
    assert!(hint.contains("enter dispatch"), "{hint}");
}

fn role_of(line: &RoleLine, text: &str) -> Option<Role> {
    line.spans
        .iter()
        .find(|span| span.text.contains(text))
        .and_then(|span| span.role)
}

#[test]
fn agent_layout_overview_rows_carry_state_in_words_and_semantic_roles() {
    let mut state = State::new(true, 120, 40);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.set_global_overview(Some(fleet_overview())).unwrap();
    let mut state = console.state.lock().unwrap();
    let lines = state
        .overview
        .lines(120, 32, 40, 121, false, DisplayProfile::default());
    let row = |title: &str| {
        lines
            .iter()
            .find(|line| line.text().contains(title))
            .unwrap_or_else(|| panic!("{title} is listed"))
    };
    let failed = row("Rebalance");
    assert!(failed.text().contains("Failed"));
    assert_eq!(role_of(failed, "Failed"), Some(Role::Error));
    assert_eq!(role_of(failed, "/work/fleet"), Some(Role::MutedText));
    assert_eq!(role_of(failed, "Rebalance"), Some(Role::Text));
    let asking = row("Research NVDA");
    assert_eq!(role_of(asking, "questions 1"), Some(Role::Permission));
    assert_eq!(
        role_of(row("Plan the hedge"), "Plan review"),
        Some(Role::Warning)
    );
    assert_eq!(role_of(row("Watch earnings"), "Queued"), Some(Role::Accent));
    assert_eq!(role_of(row("Long scan"), "Running"), Some(Role::Accent));
}

#[test]
fn agent_layout_typing_a_long_dispatch_scrolls_the_list_without_refolding_done() {
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.set_global_overview(Some(fleet_overview())).unwrap();
    let mut state = console.state.lock().unwrap();
    let folded_done = |frame: &[String]| {
        frame
            .iter()
            .find_map(|line| {
                line.split("… ")
                    .nth(1)
                    .filter(|rest| rest.contains("more done"))
                    .map(str::to_owned)
            })
            .or_else(|| frame[0].split("folded ").nth(1).map(str::to_owned))
    };
    let before = folded_done(&drawn(&mut state)).expect("Done is folded at 100×24");
    state.key(key(KeyCode::Tab));
    for line in 0..5 {
        state.insert(&format!("dispatch line {line}"));
        state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    }
    let frame = drawn(&mut state);
    assert_eq!(frame.len(), 24);
    let after = folded_done(&frame).expect("Done stays folded while typing");
    let count = |text: &str| {
        text.split_whitespace()
            .find_map(|word| word.trim_start_matches('+').parse::<usize>().ok())
    };
    assert_eq!(
        count(&after),
        count(&before),
        "before {before:?} after {after:?}: {frame:#?}"
    );
}

#[test]
fn agent_layout_a_vanished_counter_selects_the_last_row_of_its_group() {
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.set_global_overview(Some(fleet_overview())).unwrap();
    {
        let mut state = console.state.lock().unwrap();
        drawn(&mut state);
        select_target(&mut state, OverviewTarget::Disclosure(AttentionGroup::Done));
    }
    let mut fewer = fleet_overview();
    fewer.rows.retain(|row| {
        row.group != AttentionGroup::Done
            || row.agent_id == done_task(0)
            || row.parent_id == Some(done_task(0))
    });
    console.set_global_overview(Some(fewer)).unwrap();
    let state = console.state.lock().unwrap();
    assert_eq!(
        state.overview.selected,
        Some(OverviewTarget::Agent(done_task(0))),
        "the counter's own group keeps the selection"
    );
}

#[test]
fn agent_layout_overview_redraws_only_the_rows_that_changed() {
    let mut state = State::new_with_display(
        true,
        80,
        24,
        DisplayPreferences {
            prefers_reduced_motion: true,
        },
    );
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    let overview = |worker_title: &str| AgentOverview {
        rows: vec![
            agent_row(ROOT_AGENT, None, "anchor agent"),
            agent_row(Uuid::from_u128(103), None, worker_title),
        ],
        dispatch_directory: PathBuf::from("/fixture/launch"),
        created: None,
        visible: None,
    };
    console
        .set_global_overview(Some(overview("worker before")))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let mut output = Vec::new();
    state.draw(&mut output).unwrap();
    let unchanged = String::from_utf8(output).unwrap();
    assert!(
        !unchanged.contains("\x1b[2J")
            && !unchanged.contains("\x1b[2K")
            && !unchanged.contains("anchor agent"),
        "{unchanged:?}"
    );

    // Console methods draw to the process's stdout. While the State is not
    // interactive they only update it, so the frame the refresh leaves pending
    // is the one captured below.
    state.interactive = false;
    drop(state);
    console
        .set_global_overview(Some(overview("worker after")))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    state.interactive = true;
    let mut output = Vec::new();
    state.draw(&mut output).unwrap();
    let refreshed = String::from_utf8(output).unwrap();
    let renamed_rows = state
        .rendered_screen
        .iter()
        .filter(|line| line.contains("worker after"))
        .count();
    assert!(renamed_rows > 0);
    assert!(!refreshed.contains("\x1b[2J"), "{refreshed:?}");
    assert_eq!(
        refreshed.matches("\x1b[2K").count(),
        renamed_rows,
        "{refreshed:?}"
    );
    assert!(!refreshed.contains("anchor agent"), "{refreshed:?}");
}

#[test]
fn agent_layout_real_pty_an_unchanged_external_edit_restores_every_row() {
    let mut terminal = PtyConsole::start_with_editor(80, 24, "true");
    terminal.wait_for("initial", |frame| frame.contains("history 99"));
    terminal.send(b"kept draft");
    terminal.wait_for("draft", |frame| frame.contains("kept draft"));
    terminal.send(b"\x07");
    // Taking the alternate screen back starts it blank, so only what was written
    // after the second entry is on screen; the shared screen model does not
    // track buffers.
    let deadline = Instant::now() + PTY_TIMEOUT;
    let mut screen = String::new();
    while Instant::now() < deadline {
        let output = terminal.output.lock().unwrap().clone();
        if let Some((reentered, _)) = output.match_indices("\x1b[?1049h").nth(1) {
            screen = screen::visible_screen(&output[reentered..], terminal.width, terminal.height);
            if screen.contains("External editor left the draft unchanged") {
                break;
            }
        }
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        screen.contains("External editor left the draft unchanged")
            && screen.contains("history 99")
            && screen.contains("kept draft"),
        "{screen}"
    );
}

#[test]
fn agent_layout_conversation_occupancy_toggling_agents_avoids_whole_screen_redraw() {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state
        .status_area
        .update("Ask".into(), "fixture".into(), None, task_board());
    set_status_family(&mut state.status_area);
    state.footer.update(state.status_area.available_panels());
    state.scrollback.append("first line\n");
    state.insert("draft line");
    let expanded = super::tests::output_after_input_area_change(&mut state, |state| {
        let target = state.footer_bar_target.as_ref().unwrap();
        let (row, column) = (target.row, target.inset + target.categories[0].0.start);
        state.pointer(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: column as u16,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            },
            &mut io::sink(),
        );
    });
    assert_eq!(
        state.status_area.selected_panel(),
        Some(FooterItemId::Agents)
    );
    assert_eq!(state.editor.draft(), "draft line");
    assert!(
        !state.frame().0.iter().any(|line| line.contains("main")),
        "{expanded:?}"
    );
}

const SCREENSHOT_RUNNING: Uuid = Uuid::from_u128(0x9587);
const SCREENSHOT_SOURCE: Uuid = Uuid::from_u128(0x9100);
const SCREENSHOT_TITLE: &str = "这个项目是做什么的，你可以让两个subagent分别调查前端和后端吗";

fn screenshot_idle(index: u128) -> Uuid {
    Uuid::from_u128(0x28ba_0000 + index)
}

fn screenshot_done(index: u128) -> Uuid {
    Uuid::from_u128(0x07cb_0000 + index)
}

/// The fleet from the user's screenshots: one running task, one armed goal, seventeen idle
/// `New Agent` rows and a Done group led by six repeats of the same long Chinese prompt.
fn screenshot_overview(visible: Option<Uuid>) -> AgentOverview {
    let mut rows = vec![
        fleet_row(
            SCREENSHOT_RUNNING,
            None,
            SCREENSHOT_TITLE,
            (LoopState::Running, "Running"),
            AttentionGroup::Working,
        ),
        fleet_row(
            Uuid::from_u128(0x5500),
            None,
            "Keep this task",
            (LoopState::Queued, "Queued"),
            AttentionGroup::Working,
        ),
    ];
    rows.extend((0..17).map(|index| {
        fleet_row(
            screenshot_idle(index),
            None,
            "New Agent",
            (LoopState::Idle, "Idle"),
            AttentionGroup::Idle,
        )
    }));
    rows.extend((0..6).map(|index| {
        fleet_row(
            screenshot_done(index),
            None,
            SCREENSHOT_TITLE,
            (LoopState::Interrupted, "Interrupted"),
            AttentionGroup::Done,
        )
    }));
    rows.push(fleet_row(
        SCREENSHOT_SOURCE,
        None,
        "Tick 的标准化设计",
        (LoopState::Interrupted, "Interrupted"),
        AttentionGroup::Done,
    ));
    AgentOverview {
        rows,
        dispatch_directory: PathBuf::from("/work/fleet"),
        created: None,
        visible,
    }
}

/// Overview rows showing `title`; the dispatch line (`New Agent · cwd …`) is not a listed row.
fn listed_rows<'a>(frame: &'a [String], title: &str) -> Vec<&'a String> {
    frame
        .iter()
        .filter(|line| line.contains(title) && !line.contains("New Agent · cwd"))
        .collect()
}

/// Whether `line` shows a bracketed execution hash, including one whose closing bracket was cut.
fn has_hex_token(line: &str) -> bool {
    line.split('[')
        .skip(1)
        .any(|rest| rest.chars().take_while(char::is_ascii_hexdigit).count() >= 4)
}

#[test]
fn agent_layout_low_priority_groups_fold_behind_selectable_headings() {
    let mut state = State::new(true, 80, 24);
    state.chat = Some(SCREENSHOT_SOURCE);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(screenshot_overview(None)))
        .unwrap();
    let frame = frame_lines(&console);
    let idle_heading = line_with(&frame, "Idle · 17").expect("the folded Idle count stays visible");
    assert!(
        listed_rows(&frame, "New Agent").is_empty(),
        "seventeen idle rows stay behind their heading by default: {frame:#?}"
    );
    assert!(idle_heading.contains('▸'), "{idle_heading}");
    let mut state = console.state.lock().unwrap();
    select_target(&mut state, OverviewTarget::Heading(AttentionGroup::Idle));
    assert_eq!(state.overview.row_hints(), ["enter/→ expand"]);
    assert!(
        state.key(key(KeyCode::Enter)).is_none(),
        "Enter on a heading toggles it and opens no Agent"
    );
    let frame = drawn(&mut state);
    assert!(
        !listed_rows(&frame, "New Agent").is_empty(),
        "the expanded group lists its Agents: {frame:#?}"
    );
    assert!(line_with(&frame, "▾ Idle · 17").is_some(), "{frame:#?}");
    assert!(state.key(key(KeyCode::Enter)).is_none());
    let frame = drawn(&mut state);
    assert!(listed_rows(&frame, "New Agent").is_empty(), "{frame:#?}");
    drop(state);
    console
        .set_global_overview(Some(screenshot_overview(None)))
        .unwrap();
    let frame = frame_lines(&console);
    assert!(
        listed_rows(&frame, "New Agent").is_empty(),
        "the fold survives a refresh: {frame:#?}"
    );
}

#[test]
fn agent_layout_overview_marks_the_agent_it_returns_to_apart_from_the_selection() {
    let mut state = State::new(true, 80, 24);
    state.chat = Some(SCREENSHOT_SOURCE);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(screenshot_overview(Some(SCREENSHOT_SOURCE))))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(
        state.overview.selected,
        Some(OverviewTarget::Agent(SCREENSHOT_SOURCE)),
        "entering from a conversation locates that Agent"
    );
    state.key(key(KeyCode::Char('k')));
    let frame = drawn(&mut state);
    let source = line_with(&frame, "Tick 的标准化设计").expect("the source row stays listed");
    assert!(
        !source.starts_with(SELECTED_MARK) && source.contains("current"),
        "the returning Agent keeps its own cue after the cursor moves on: {source}"
    );
    let selected = state.overview.selected;
    drop(state);
    let mut reordered = screenshot_overview(Some(SCREENSHOT_SOURCE));
    reordered.rows.reverse();
    console.set_global_overview(Some(reordered)).unwrap();
    let state = console.state.lock().unwrap();
    assert_eq!(
        state.overview.selected, selected,
        "a refresh neither moves the cursor nor snaps it back to the source"
    );
}

#[test]
fn agent_layout_repeated_long_titles_stay_distinguishable_without_hashes() {
    for (width, height) in [(40, 24), (80, 24), (120, 32)] {
        let mut state = State::new(true, width, height);
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        console
            .set_global_overview(Some(screenshot_overview(None)))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        let done = OverviewTarget::Disclosure(AttentionGroup::Done);
        for _ in 0..80 {
            if state.overview.selected == Some(done) {
                state.key(key(KeyCode::Enter));
                break;
            }
            state.key(key(KeyCode::Char('j')));
        }
        let frame = drawn(&mut state);
        assert!(
            !frame.iter().any(|line| has_hex_token(line)),
            "{width} columns: everyday rows never show execution hashes: {frame:#?}"
        );
        assert!(
            frame
                .iter()
                .filter(|line| line.contains("这个项目"))
                .count()
                >= 3,
            "{width} columns: several repeats are on screen: {frame:#?}"
        );
        let repeats = frame
            .iter()
            .filter(|line| line.contains("这个项目"))
            .map(|line| line.trim_start_matches([SELECTED_MARK, ' ']).to_owned())
            .collect::<Vec<_>>();
        let distinct = repeats.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(
            distinct.len(),
            repeats.len(),
            "{width} columns: repeated prompts stay readable apart: {frame:#?}"
        );
        assert!(
            frame
                .iter()
                .all(|line| display_width(line) <= width as usize),
            "{width} columns: {frame:#?}"
        );
    }
}

fn repeated_done(id: Uuid, parent: Option<Uuid>, title: &str) -> AgentRow {
    fleet_row(
        id,
        parent,
        title,
        (LoopState::Interrupted, "Interrupted"),
        AttentionGroup::Done,
    )
}

#[test]
fn agent_layout_reading_numbers_stay_bound_through_arrivals_and_removals() {
    const TITLE: &str = "Summarize filings";
    let (first, current, created) = (
        Uuid::from_u128(0xa000),
        Uuid::from_u128(0xb000),
        Uuid::from_u128(0xc000),
    );
    let overview = |ids: &[Uuid]| AgentOverview {
        rows: ids
            .iter()
            .map(|id| repeated_done(*id, None, TITLE))
            .collect(),
        dispatch_directory: PathBuf::from("/work/fleet"),
        created: Some(created),
        visible: Some(current),
    };
    let mut state = State::new(true, 100, 32);
    state.chat = Some(current);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(overview(&[first, current, created])))
        .unwrap();
    let numbered = |frame: &[String]| {
        let current_line = line_with(frame, "current").expect("the current row is listed");
        assert!(current_line.starts_with(SELECTED_MARK), "{frame:#?}");
        assert!(current_line.contains(&format!("{TITLE} (2)")), "{frame:#?}");
        assert!(
            line_with(frame, &format!("Created {TITLE} (3)")).is_some(),
            "the receipt cue names the same number as its row: {frame:#?}"
        );
        assert!(!frame.iter().any(|line| has_hex_token(line)), "{frame:#?}");
    };
    numbered(&frame_lines(&console));
    // A same-title Agent with a lower identity arrives, then the first one disappears.
    let arriving = Uuid::from_u128(0x1000);
    console
        .set_global_overview(Some(overview(&[arriving, first, current, created])))
        .unwrap();
    let frame = frame_lines(&console);
    numbered(&frame);
    assert!(
        line_with(&frame, &format!("{TITLE} (4)")).is_some(),
        "{frame:#?}"
    );
    console
        .set_global_overview(Some(overview(&[current, arriving, created])))
        .unwrap();
    let frame = frame_lines(&console);
    numbered(&frame);
    assert!(
        line_with(&frame, &format!("{TITLE} (1)")).is_none(),
        "{frame:#?}"
    );
    let mut state = console.state.lock().unwrap();
    assert!(matches!(
        state.key(key(KeyCode::Enter)),
        Some(InputEvent::OpenAgent(id)) if id == current
    ));
}

/// A listed row's text after its selection marker, indentation and state glyph.
fn row_title(line: &str) -> &str {
    let rest = line.trim_start_matches([SELECTED_MARK, ' ']);
    rest.split_once(' ')
        .filter(|(glyph, _)| glyph.chars().count() == 1)
        .map_or(rest, |(_, title)| title)
}

/// Whether the selected overview row of a frame shows `text`.
fn selected_row_contains(frame: &str, text: &str) -> bool {
    frame
        .lines()
        .any(|line| line.starts_with(SELECTED_MARK) && line.contains(text))
}

/// The overview list region of a frame: everything above the dispatch line.
fn list_lines(frame: &[String]) -> &[String] {
    let end = frame
        .iter()
        .position(|line| line.contains("New Agent · cwd"))
        .unwrap_or(frame.len());
    &frame[..end]
}

/// Titles that differ only in the column a narrow cut gives to the ellipsis read alike there;
/// they share one sequence of reading numbers, so the overview and receipt cue remain distinct.
#[test]
fn agent_layout_titles_cut_alike_at_the_ellipsis_never_share_a_number() {
    for (first, second) in [
        ("Abcd long task", "Abce long task"),
        ("研究财报", "研发投入"),
    ] {
        let created = Uuid::from_u128(0xe200);
        let state = State::new(true, 24, 32);
        let console = super::tests::console_with_state(state);
        console
            .set_global_overview(Some(AgentOverview {
                rows: vec![
                    repeated_done(created, None, first),
                    repeated_done(Uuid::from_u128(0xe300), None, second),
                ],
                dispatch_directory: PathBuf::from("/work/fleet"),
                created: Some(created),
                visible: None,
            }))
            .unwrap();
        let frame = frame_lines(&console);
        let cue = line_with(&frame, "Created").expect("the receipt cue is shown");
        let number = cue
            .split_once(" (")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(number, _)| format!("({number})"))
            .unwrap_or_else(|| panic!("the cue carries a number: {frame:#?}"));
        let numbered = listed_rows(list_lines(&frame), &number);
        assert_eq!(numbered.len(), 1, "{number} names one row: {frame:#?}");
        assert!(
            row_title(numbered[0]).starts_with(&first[..first.char_indices().nth(1).unwrap().0]),
            "the created row carries the cue's number: {frame:#?}"
        );
        assert!(!frame.iter().any(|line| has_hex_token(line)), "{frame:#?}");
    }
}

/// A receipt cue and a hidden current Agent each keep their own fact: the current row is folded
/// under an explicitly collapsed group or filtered away while another row is selected and the
/// dispatch draft waits; Esc still leaves for the source.
#[test]
fn agent_layout_receipt_and_hidden_current_cues_survive_together() {
    let (scan, root, child, created) = (
        Uuid::from_u128(0xf100),
        Uuid::from_u128(0xf200),
        Uuid::from_u128(0xf201),
        Uuid::from_u128(0xf300),
    );
    let overview = || {
        let mut source = repeated_done(child, Some(root), "Check hedges");
        source.project = "/work/other".into();
        let mut source_root = repeated_done(root, None, "Rebalance plan");
        source_root.project = "/work/other".into();
        AgentOverview {
            rows: vec![
                fleet_row(
                    scan,
                    None,
                    "Scan quotes",
                    (LoopState::Running, "Running"),
                    AttentionGroup::Working,
                ),
                fleet_row(
                    created,
                    None,
                    "Draft summary",
                    (LoopState::Running, "Running"),
                    AttentionGroup::Working,
                ),
                source_root,
                source,
            ],
            dispatch_directory: PathBuf::from("/work/fleet"),
            created: Some(created),
            visible: Some(child),
        }
    };
    for hide_by_filter in [false, true] {
        for width in [24, 40, 80] {
            let label = format!("{width} columns, filter {hide_by_filter}");
            let mut state = State::new(true, width, 24);
            state.chat = Some(child);
            state.prompt_enabled = true;
            let console = super::tests::console_with_state(state);
            console.set_global_overview(Some(overview())).unwrap();
            let mut state = console.state.lock().unwrap();
            assert_eq!(state.overview.selected, Some(OverviewTarget::Agent(child)));
            if hide_by_filter {
                state.key(key(KeyCode::Char('p')));
                assert_eq!(state.overview.project.as_deref(), Some("/work/fleet"));
            } else {
                select_target(&mut state, OverviewTarget::Heading(AttentionGroup::Done));
                assert!(state.key(key(KeyCode::Enter)).is_none());
            }
            select_agent(&mut state, scan);
            state.key(key(KeyCode::Tab));
            state.insert("held draft");
            state.key(key(KeyCode::Tab));
            let frame = drawn(&mut state);
            assert!(
                line_with(list_lines(&frame), "Check hedges").is_none(),
                "{label}: {frame:#?}"
            );
            let status = frame
                .iter()
                .find(|line| line.starts_with("Created"))
                .unwrap_or_else(|| panic!("{label}: the receipt cue stays: {frame:#?}"));
            assert!(
                status.contains("Current"),
                "{label}: the hidden current Agent keeps its own cue: {frame:#?}"
            );
            if !hide_by_filter && width >= 40 {
                assert!(
                    line_with(&frame, "▸ Done").is_some_and(|heading| heading.contains("current")),
                    "{label}: the collapsed heading names the current Agent: {frame:#?}"
                );
            }
            assert!(
                frame
                    .iter()
                    .all(|line| display_width(line) <= width as usize),
                "{label}: {frame:#?}"
            );
            assert_eq!(state.overview.selected, Some(OverviewTarget::Agent(scan)));
            assert_eq!(state.editor.draft(), "held draft");
            assert!(matches!(
                state.key(key(KeyCode::Esc)),
                Some(InputEvent::CloseOverview)
            ));
        }
    }
}

/// Cycling a filter keeps any selection the new filter still lists, a heading included; only a
/// selection the filter hides falls back to the first listed Agent.
#[test]
fn agent_layout_filter_cycling_keeps_a_still_listed_heading_selected() {
    let running = Uuid::from_u128(0xa1);
    let overview = || {
        let mut scan = fleet_row(
            running,
            None,
            "Scan quotes",
            (LoopState::Running, "Running"),
            AttentionGroup::Working,
        );
        scan.project = "project-b".into();
        dispatch_overview(
            vec![
                scan,
                finished_row(OLD_SHELL_AGENT, "!pwd", "project-a"),
                finished_row(BRAVO_AGENT, "!printf bravo", "project-b"),
            ],
            None,
        )
    };
    let console_selecting = |target: OverviewTarget| {
        let mut state = State::new(true, 100, 24);
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        console.set_global_overview(Some(overview())).unwrap();
        select_target(&mut console.state.lock().unwrap(), target);
        console
    };
    let done = OverviewTarget::Heading(AttentionGroup::Done);
    let console = console_selecting(done);
    let mut state = console.state.lock().unwrap();
    state.key(key(KeyCode::Char('p')));
    assert_eq!(state.overview.project.as_deref(), Some("project-a"));
    assert_eq!(
        state.overview.selected,
        Some(done),
        "project-a still lists the Done heading"
    );
    let frame = drawn(&mut state);
    assert!(
        line_with(&frame, &format!("{SELECTED_MARK} ▾ Done · 1")).is_some(),
        "{frame:#?}"
    );
    drop(state);

    let console = console_selecting(OverviewTarget::Heading(AttentionGroup::Working));
    let mut state = console.state.lock().unwrap();
    state.key(key(KeyCode::Char('p')));
    assert_eq!(state.overview.project.as_deref(), Some("project-a"));
    assert_eq!(
        state.overview.selected,
        Some(OverviewTarget::Agent(OLD_SHELL_AGENT)),
        "the hidden Working heading falls back to the first listed Agent"
    );
}

#[test]
fn agent_layout_known_age_ends_the_row_and_the_glyph_carries_the_state_role() {
    let now = tickloop_harness::base::private_fs::unix_time_milliseconds();
    let six_minutes_ago = now - 6 * 60_000 - 30_000;
    let finished = Uuid::from_u128(0xa100);
    let asking = Uuid::from_u128(0xa200);
    for (width, height) in [(24, 24), (40, 24), (80, 24), (120, 32)] {
        let mut done = fleet_row(
            finished,
            None,
            "Alpha",
            (LoopState::Interrupted, "Interrupted"),
            AttentionGroup::Done,
        );
        done.updated_at = Some(six_minutes_ago);
        let mut approval = fleet_row(
            asking,
            None,
            "Bravo",
            (LoopState::Running, "Running"),
            AttentionGroup::NeedsInput,
        );
        approval.approvals = 1;
        approval.questions = 1;
        approval.updated_at = Some(six_minutes_ago);
        let untimed = fleet_row(
            Uuid::from_u128(0xa300),
            None,
            "Charlie",
            (LoopState::Interrupted, "Interrupted"),
            AttentionGroup::Done,
        );
        let mut state = State::new(true, width, height);
        state.prompt_enabled = true;
        let console = super::tests::console_with_state(state);
        console
            .set_global_overview(Some(AgentOverview {
                rows: vec![done, approval, untimed],
                dispatch_directory: PathBuf::from("/work/fleet"),
                created: None,
                visible: Some(asking),
            }))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        let screen = drawn(&mut state);
        let top_height = state.height - state.styled_frame().0.len();
        let (list_width, terminal_height, help_open, display) =
            (state.width, state.height, state.help_open, state.display);
        let lines = state.overview.lines(
            list_width,
            top_height,
            terminal_height,
            list_width + 1,
            help_open,
            display,
        );
        let screen_row = |title: &str| {
            screen
                .iter()
                .find(|line| line.contains(title))
                .cloned()
                .unwrap_or_else(|| panic!("{width} columns: {title} is listed: {screen:#?}"))
        };
        let content_columns = usize::from(width) - 1;
        for (title, glyph, role) in [("Alpha", '✖', Role::Warning), ("Bravo", '◆', Role::Warning)]
        {
            let text = screen_row(title);
            let line = lines
                .iter()
                .find(|line| line.text().contains(title))
                .expect("the painted row comes from the overview lines");
            if width >= 40 {
                assert!(
                    text.ends_with("6m") && display_width(&text) == content_columns,
                    "{width} columns: a known age ends at the row's right edge: {text:?}"
                );
                let shape = line
                    .spans
                    .iter()
                    .find(|span| span.text.contains(glyph))
                    .unwrap_or_else(|| {
                        panic!("{width} columns: {title} keeps its shape: {text:?}")
                    });
                assert_eq!(
                    shape.role,
                    Some(role),
                    "{width} columns: the shape carries its state's role, not the title's: {line:?}"
                );
            } else if text.ends_with("6m") {
                assert_eq!(display_width(&text), content_columns, "{text:?}");
            }
        }
        let requests = screen_row("Bravo");
        assert!(
            ["approvals 1", "approvals", "A1", "A"]
                .iter()
                .any(|label| requests.contains(label))
                && requests.contains("cur"),
            "{width} columns: request and current labels win over the age: {requests:?}"
        );
        let untimed = screen_row("Charlie");
        let last_word = untimed.split_whitespace().last().unwrap_or_default();
        let reads_as_age = last_word == "now"
            || ["m", "h", "d"].iter().any(|unit| {
                last_word
                    .strip_suffix(unit)
                    .is_some_and(|count| count.parse::<u64>().is_ok())
            });
        assert!(
            !reads_as_age,
            "{width} columns: an unknown time is never shown as an age: {untimed:?}"
        );
    }
}
