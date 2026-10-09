use super::*;
use crate::terminal::questions::QuestionFlow;
use tickloop_agent_protocol::{Question, QuestionOption, QuestionRequest};

const REQUEST: Uuid = Uuid::from_u128(480);
const DESCRIPTION: &str =
    "用 Subagent 起 4 个并行子 Agent，各自 sleep 对应时长后返回；会占用 4 个模型会话。";

fn question_console(width: u16, height: u16, multi_select: bool) -> Console {
    let mut state = State::new(true, width, height);
    state.prompt_enabled = true;
    state.insert("preserve the parent draft");
    let console = super::tests::console_with_state(state);
    let flow = QuestionFlow::new(
        QuestionRequest {
            id: REQUEST,
            chat_id: Uuid::from_u128(481),
            loop_id: Uuid::from_u128(482),
            run_id: Uuid::from_u128(483),
            call_id: "question-layout".into(),
            questions: vec![Question {
                question: "“4 个 task”指的是哪种？".into(),
                header: "Task 类型".into(),
                options: vec![
                    QuestionOption {
                        label: "后台 Shell 命令（推荐）".into(),
                        description: "分别启动四个后台 Shell 命令，保留模型会话。".into(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "4 个子 Agent".into(),
                        description: DESCRIPTION.into(),
                        preview: Some("First preview row\nSecond preview row".into()),
                    },
                ],
                multi_select,
            }],
        },
        "Agent Choose four tasks".into(),
    );
    console.set_request_layout(Some(flow.request().id)).unwrap();
    let (menu, keys) = flow.menu(console.display_profile().unwrap());
    console.show_question_menu(menu, keys).unwrap();
    console
}

#[test]
fn request_question_descriptions_and_other_use_separate_physical_rows() {
    for width in [40, 140] {
        let console = question_console(width, 23, false);
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let (lines, selected_row, _) = state.styled_frame();
        let text = lines.iter().map(RoleLine::text).collect::<Vec<_>>();
        assert!(text.iter().all(|line| !line.contains('\n')), "{text:?}");
        assert!(text.iter().all(|line| display_width(line) <= state.width));
        assert!(text[selected_row].contains("4 个子 Agent"), "{text:?}");
        let description = text
            .iter()
            .position(|line| line.contains("用 Subagent"))
            .unwrap();
        assert!(selected_row < description, "{text:?}");
        assert!(
            text.iter().any(|line| line
                == if width < 50 {
                    "enter select · esc cancel"
                } else {
                    "enter select · ↑/↓ navigate · esc cancel"
                }),
            "{text:?}"
        );
        for code in [KeyCode::Down, KeyCode::Up] {
            state.key(KeyEvent::new(code, KeyModifiers::NONE));
            if code == KeyCode::Down {
                let (lines, other_row, _) = state.styled_frame();
                let text = lines.iter().map(RoleLine::text).collect::<Vec<_>>();
                assert!(text[other_row].ends_with("Other"), "{text:?}");
                let placeholder = text
                    .iter()
                    .position(|line| line.trim() == "Type something.")
                    .unwrap();
                assert!(other_row < placeholder, "{text:?}");
            }
            let mut output = Vec::new();
            state.draw(&mut output).unwrap();
            assert!(
                !output.contains(&b'\n'),
                "request paints must contain physical rows, not embedded LF"
            );
        }
        assert_eq!(state.editor.draft(), "preserve the parent draft");
    }
}

#[test]
fn request_question_wraps_within_the_row_budget_after_resize() {
    let console = question_console(140, 23, true);
    let mut state = console.state.lock().unwrap();
    state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    for width in [24, 40, 140] {
        for height in [3, 6, 9, 10, 12, 23] {
            state.resize(width, height);
            let (lines, row, column) = state.styled_frame();
            let text = lines.iter().map(RoleLine::text).collect::<Vec<_>>();
            let capacity = input_dialog::fixed_height_for_terminal(state.height);
            assert!(lines.len() <= capacity, "{width}×{height}: {text:?}");
            assert!(
                text.iter()
                    .all(|line| !line.contains('\n') && display_width(line) <= state.width),
                "{text:?}"
            );
            assert!(
                text.iter().any(|line| line.contains("4 个子 Agent")),
                "{text:?}"
            );
            assert!(
                row < lines.len() && column < state.width,
                "{row},{column}: {text:?}"
            );
            if capacity >= 3 && width >= 40 {
                assert!(
                    text.iter()
                        .any(|line| line.starts_with("space/enter toggle")
                            && line.contains("esc cancel")),
                    "{text:?}"
                );
            }
            let mut output = Vec::new();
            state.draw(&mut output).unwrap();
            assert!(!output.contains(&b'\n'), "{width}×{height}: {text:?}");
        }
    }
}

#[test]
fn request_other_field_wraps_and_keeps_its_cursor_and_submit_hint() {
    let console = question_console(80, 23, false);
    let mut state = console.state.lock().unwrap();
    for code in [KeyCode::Down, KeyCode::Down] {
        state.key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    let text = state
        .styled_frame()
        .0
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>();
    assert!(
        text.iter()
            .any(|line| line == "enter type answer · ↑/↓ navigate · esc cancel"),
        "{text:?}"
    );
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let answer = format!("{}Second answer line", "x".repeat(71));
    state.paste(&answer);
    let (lines, row, column) = state.styled_frame();
    let text = lines.iter().map(RoleLine::text).collect::<Vec<_>>();
    assert!(text.iter().all(|line| !line.contains('\n')), "{text:?}");
    assert_eq!(text[row], "Second answer line", "{text:?}");
    assert_eq!(column, "Second answer line".len());
    assert!(
        text.iter()
            .any(|line| line == "Type your answer · enter submit · esc close field"),
        "{text:?}"
    );
    assert_eq!(state.request_layout, Some(REQUEST));
    assert_eq!(state.editor.draft(), "preserve the parent draft");
    for _ in 0.."Second answer line".len() {
        state.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    }
    let (lines, row, column) = state.styled_frame();
    assert_eq!(lines[row].text(), "Second answer line");
    assert_eq!(column, 0, "the next physical row owns its starting cursor");
    let menu_id = state.menu.as_ref().unwrap().id;
    let submitted = state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        matches!(submitted, Some(InputEvent::Choice { menu_id: id, index: 2, comment: Some(ref value) }) if id == menu_id && value == &answer),
        "{submitted:?}"
    );
}

fn report_question() -> Question {
    Question {
        question: "Which report should be shortened?".into(),
        header: "Report".into(),
        options: ["Weekly", "Monthly"]
            .into_iter()
            .map(|label| QuestionOption {
                label: label.into(),
                description: format!("The {label} report"),
                preview: None,
            })
            .collect(),
        multi_select: false,
    }
}

fn present_question(state: State) -> Console {
    let console = super::tests::console_with_state(state);
    let flow = QuestionFlow::new(
        QuestionRequest {
            id: REQUEST,
            chat_id: Uuid::from_u128(481),
            loop_id: Uuid::from_u128(482),
            run_id: Uuid::from_u128(483),
            call_id: "question-keys".into(),
            questions: vec![report_question()],
        },
        "Agent Shorten the report".into(),
    );
    console.set_request_layout(Some(REQUEST)).unwrap();
    let (menu, keys) = flow.menu(console.display_profile().unwrap());
    console.show_question_menu(menu, keys).unwrap();
    console
}

fn wheel(kind: MouseEventKind, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: 0,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn esc_and_ctrl_c_decline_a_question_whose_dialog_hints_only_its_own_keys() {
    for key in [
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut state = State::new(true, 120, 30);
        state.prompt_enabled = true;
        state.switch_view_chat(Some(Uuid::from_u128(481)));
        state.monitor.replace(
            TickGroup::Artifacts,
            super::input_dialog_tests::fixture_ticks(),
        );
        state.monitor.focus(true, None);
        assert!(state.monitor_visible());
        let console = present_question(state);
        let mut state = console.state.lock().unwrap();
        state.draw_frame(&mut Vec::new()).unwrap();
        let screen = state.rendered_screen.join("\n");
        assert!(
            screen.contains("enter select · ↑/↓ navigate · esc cancel"),
            "{screen}"
        );
        for foreign in ["answer later", "esc input", "enter open"] {
            assert!(!screen.contains(foreign), "{foreign}: {screen}");
        }
        let menu_id = state.menu.as_ref().unwrap().id;
        let event = state.key(key);
        assert!(
            matches!(event, Some(InputEvent::Cancel { menu_id: id }) if id == menu_id),
            "{key:?}: {event:?}"
        );
    }
}

#[test]
fn ctrl_c_declines_from_the_other_field_and_esc_first_closes_the_field() {
    let mut state = State::new(true, 100, 30);
    state.prompt_enabled = true;
    let console = present_question(state);
    let mut state = console.state.lock().unwrap();
    for code in [KeyCode::Down, KeyCode::Down, KeyCode::Enter] {
        state.key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    assert!(state.menu_comment_is_editing());
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_none()
    );
    assert!(!state.menu_comment_is_editing() && state.menu.is_some());
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(state.menu_comment_is_editing());
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(InputEvent::Cancel { .. })
    ));
}

#[test]
fn page_keys_and_the_wheel_scroll_the_conversation_behind_a_question() {
    let mut state = State::new(true, 80, 30);
    state.prompt_enabled = true;
    state.scrollback.append(
        &(0..100)
            .map(|index| format!("history-{index}\n"))
            .collect::<String>(),
    );
    let console = present_question(state);
    let mut state = console.state.lock().unwrap();
    state.draw(&mut Vec::new()).unwrap();
    let conversation = |state: &mut State| {
        let width = state.width;
        state.scrollback.frame(width, 10).lines
    };
    let at_tail = conversation(&mut state);

    assert!(
        state
            .key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .is_none()
    );
    assert_ne!(
        conversation(&mut state),
        at_tail,
        "PageUp moves the conversation"
    );
    assert!(state.menu.is_some() && state.menu_focused);
    state.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(conversation(&mut state), at_tail);

    let dialog_top = u16::try_from(state.height - state.input_dialog_height()).unwrap();
    for row in [0, dialog_top + 2] {
        assert!(state.pointer(wheel(MouseEventKind::ScrollUp, row), &mut Vec::new()));
        assert_ne!(conversation(&mut state), at_tail, "the wheel on row {row}");
        state.pointer(wheel(MouseEventKind::ScrollDown, row), &mut Vec::new());
        assert_eq!(conversation(&mut state), at_tail);
    }
    assert!(state.menu.is_some() && !state.conversation_focused);
}
