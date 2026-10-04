use super::*;
use crate::terminal::questions::QuestionFlow;
use tickloop_agent::runtime::{Question, QuestionOption, QuestionRequest};

const REQUEST: Uuid = Uuid::from_u128(480);
const DESCRIPTION: &str =
    "用 DelegateAgent 起 4 个并行子 Agent，各自 sleep 对应时长后返回；会占用 4 个模型会话。";

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
        "Agent Main · choose how to run four tasks".into(),
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
            .position(|line| line.contains("用 DelegateAgent"))
            .unwrap();
        assert!(selected_row < description, "{text:?}");
        assert!(
            text.iter()
                .any(|line| line == "Enter select · Esc answer later"),
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
                        .any(|line| line.starts_with("Space/Enter toggle · Esc answer later")),
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
            .any(|line| line == "Enter type answer · Esc answer later"),
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
            .any(|line| line == "Type your answer · Enter submit · Esc close field"),
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
