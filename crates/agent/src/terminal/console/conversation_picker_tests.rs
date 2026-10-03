use super::*;
use crate::terminal::conversation_picker::{
    ConversationEntry, ConversationPicker, ConversationPickerPurpose,
};

const DRAFT: &str = "keep this draft";

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn control(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

fn screen(state: &mut State) -> Vec<String> {
    state.draw(&mut Vec::new()).unwrap();
    if state.conversation_picker.is_some() {
        state.rendered_screen[state.height - state.input_dialog_height()..].to_vec()
    } else {
        state.rendered_screen.clone()
    }
}

fn hint_row(lines: &[String]) -> &str {
    lines
        .iter()
        .rev()
        .find(|line| !line.is_empty())
        .expect("the picker shows a hint")
}

fn saved_conversations() -> ConversationPicker {
    ConversationPicker::new(
        ConversationPickerPurpose::Resume,
        vec![
            ConversationEntry::new(
                "请分析腾讯控股最近三个季度的财报，\n比较营收增长与利润率变化并给出风险提示",
                &["2h ago".into(), "main".into()],
                &[],
            ),
            ConversationEntry::new("Duplicate", &["1d ago".into()], &[]),
            ConversationEntry::new("Duplicate", &["3d ago".into()], &[]),
        ],
    )
}

fn state_with_draft(width: u16, height: u16) -> State {
    let mut state = State::new(true, width, height);
    state.prompt_enabled = true;
    state.replace_draft(DRAFT.into(), DRAFT.len());
    state
}

#[test]
fn the_open_picker_owns_every_key_and_cancelling_restores_the_same_screen_and_draft() {
    let mut state = state_with_draft(40, 12);
    let before = screen(&mut state);
    assert!(before.iter().any(|line| line.contains(DRAFT)), "{before:?}");
    let picker = saved_conversations();
    let menu_id = picker.id();
    state.conversation_picker = Some(picker);
    let opened = screen(&mut state);
    assert_eq!(opened.len(), 8);
    assert!(opened[0].starts_with("Resume Chat · 1 of 3"), "{opened:?}");
    assert_eq!(opened[1], "⌕ Search…");
    assert!(opened[2].starts_with("❯ 请分析腾讯控股") && opened[2].ends_with('…'));
    assert_eq!(opened[3], "  2h ago · main");
    assert!(
        !opened.iter().any(|line| line.contains(DRAFT)),
        "{opened:?}"
    );
    assert!(opened.iter().all(|line| display_width(line) <= 40));

    for input in [
        key(KeyCode::Char('j')),
        key(KeyCode::Char('/')),
        key(KeyCode::Char('j')),
        key(KeyCode::Char('k')),
        key(KeyCode::Backspace),
        control('u'),
        control('c'),
        control('v'),
        control('s'),
        key(KeyCode::Tab),
        key(KeyCode::PageUp),
    ] {
        assert!(state.key(input).is_none(), "{input:?}");
    }
    state.paste("pasted\ntext");
    assert_eq!(state.editor.draft(), DRAFT);
    assert_eq!(
        state.conversation_picker.as_ref().unwrap().query(),
        "pasted text"
    );
    assert!(
        !state.pointer(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            &mut Vec::new(),
        ),
        "the wheel must not scroll the hidden conversation"
    );
    assert!(!state.conversation_focused);

    assert!(
        state.key(key(KeyCode::Esc)).is_none(),
        "Esc first clears the search"
    );
    assert!(
        state.key(key(KeyCode::Esc)).is_none(),
        "then returns to the list"
    );
    let cancel = state.key(key(KeyCode::Esc));
    assert!(matches!(cancel, Some(InputEvent::Cancel { menu_id: id }) if id == menu_id));
    assert!(
        state.key(key(KeyCode::Char('x'))).is_none(),
        "keys wait while the cancellation is applied"
    );
    state.mark_menu_submission_finished(menu_id);
    assert!(state.release_finished_submissions());
    assert!(state.conversation_picker.is_none());
    assert_eq!(screen(&mut state), before);
    assert_eq!(state.editor.draft(), DRAFT);
    state.key(key(KeyCode::Char('!')));
    assert_eq!(state.editor.draft(), format!("{DRAFT}!"));
}

#[test]
fn query_edits_and_recovery_never_reach_the_hidden_composer() {
    let mut state = state_with_draft(80, 24);
    state.key(control('u'));
    state.key(control('y'));
    state.key(key(KeyCode::Home));
    let before = screen(&mut state);
    let picker = saved_conversations();
    let menu_id = picker.id();
    state.conversation_picker = Some(picker);
    state.key(key(KeyCode::Char('/')));
    state.paste("Duplicate");
    state.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(state.conversation_picker.as_ref().unwrap().query(), "");
    state.key(control('y'));
    assert_eq!(
        state.conversation_picker.as_ref().unwrap().query(),
        "Duplicate"
    );
    state.key(control('u'));
    state.key(control('y'));
    state.key(control('_'));
    assert_eq!(state.conversation_picker.as_ref().unwrap().query(), "");
    state.key(control('7'));
    assert_eq!(
        state.conversation_picker.as_ref().unwrap().query(),
        "Duplicate"
    );
    state.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    state.key(control('d'));
    assert_eq!(
        state.conversation_picker.as_ref().unwrap().query(),
        "uplicate"
    );
    state.key(KeyEvent::new(
        KeyCode::Char('-'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert_eq!(
        state.conversation_picker.as_ref().unwrap().query(),
        "Duplicate"
    );
    state.key(key(KeyCode::End));
    state.key(control('u'));
    state.key(key(KeyCode::Char('j')));
    state.key(key(KeyCode::Char('k')));
    assert_eq!(state.conversation_picker.as_ref().unwrap().query(), "jk");
    assert_eq!(state.editor.draft(), DRAFT);
    assert_eq!(state.editor.cursor(), 0);

    state.key(key(KeyCode::Esc));
    state.key(key(KeyCode::Esc));
    assert!(
        matches!(state.key(key(KeyCode::Esc)), Some(InputEvent::Cancel { menu_id: id }) if id == menu_id)
    );
    state.mark_menu_submission_finished(menu_id);
    assert!(state.release_finished_submissions());
    assert_eq!(screen(&mut state), before);
    state.key(control('y'));
    assert_eq!(state.editor.draft(), format!("{DRAFT}{DRAFT}"));
    state.key(control('_'));
    assert_eq!(state.editor.draft(), DRAFT);
    assert_eq!(state.editor.cursor(), 0);
}

#[test]
fn enter_reports_the_original_entry_index_after_filtering_duplicate_titles() {
    let mut state = state_with_draft(80, 24);
    let picker = saved_conversations();
    let menu_id = picker.id();
    state.conversation_picker = Some(picker);
    for character in "/3d".chars() {
        state.key(key(KeyCode::Char(character)));
    }
    let filtered = screen(&mut state);
    assert!(filtered[0].ends_with("1 of 1 matching"), "{filtered:?}");
    assert_eq!(filtered[2], "❯ Duplicate");
    assert_eq!(filtered[3], "  3d ago");
    for input in [control('u'), control('y'), control('_'), control('7')] {
        assert!(state.key(input).is_none());
    }
    assert_eq!(state.conversation_picker.as_ref().unwrap().query(), "3d");
    let choice = state.key(key(KeyCode::Enter));
    assert!(
        matches!(choice, Some(InputEvent::Choice { menu_id: id, index: 2, comment: None }) if id == menu_id),
        "{choice:?}"
    );
    assert_eq!(state.editor.draft(), DRAFT);
}

#[test]
fn an_empty_search_result_cannot_open_an_unrelated_conversation() {
    let mut state = state_with_draft(40, 10);
    state.conversation_picker = Some(saved_conversations());
    for character in "/missing".chars() {
        state.key(key(KeyCode::Char(character)));
    }
    let empty = screen(&mut state);
    assert!(empty[0].ends_with("no matches"), "{empty:?}");
    assert!(
        empty
            .iter()
            .any(|line| line == "No conversations match “missing”")
    );
    assert!(state.key(key(KeyCode::Enter)).is_none());
    state.key(key(KeyCode::Down));
    assert!(state.key(key(KeyCode::Enter)).is_none());
    assert!(state.menu_submission_pending.is_none());
}

#[test]
fn control_g_opens_the_search_query_in_the_external_editor_and_never_the_draft() {
    let mut state = state_with_draft(80, 24);
    let opened = Arc::new(Mutex::new(Vec::new()));
    state.external_editor = ExternalEditorLauncher::Scripted {
        opened_drafts: Arc::clone(&opened),
        outcome: ExternalEditorOutcome::Updated {
            content: "Duplicate\n".into(),
        },
    };
    state.conversation_picker = Some(saved_conversations());
    state.key(key(KeyCode::Char('D')));
    assert!(state.key(control('g')).is_none());
    assert_eq!(*opened.lock().unwrap(), vec!["D".to_owned()]);
    assert_eq!(state.editor.draft(), DRAFT);
    let picker = state.conversation_picker.as_ref().unwrap();
    assert_eq!(picker.query(), "Duplicate");
    assert_eq!(picker.selection(), Some(1));
    let lines = screen(&mut state);
    assert!(lines[0].ends_with("1 of 2 matching"), "{lines:?}");
    assert!(hint_row(&lines).contains("Ctrl+G editor"), "{lines:?}");

    state.external_editor = ExternalEditorLauncher::Scripted {
        opened_drafts: Arc::clone(&opened),
        outcome: ExternalEditorOutcome::Unavailable,
    };
    state.key(control('g'));
    let lines = screen(&mut state);
    assert_eq!(
        hint_row(&lines),
        "No external editor found; set VISUAL or EDITOR"
    );
    assert_eq!(state.editor.draft(), DRAFT);
}

#[test]
fn dismissing_the_menu_clears_the_picker_and_returns_keys_to_the_editor() {
    let mut state = state_with_draft(40, 3);
    let picker = saved_conversations();
    let menu_id = picker.id();
    state.conversation_picker = Some(picker);
    let tiny = screen(&mut state);
    assert_eq!(tiny.len(), 1);
    assert_eq!(tiny[0], "❯ 请分析腾讯控股最近三个季度的财报，…");
    assert!(!state.clear_menu(Uuid::new_v4()));
    assert!(state.conversation_picker.is_some());
    assert!(state.clear_menu(menu_id));
    assert!(state.conversation_picker.is_none());
    state.key(key(KeyCode::Char('j')));
    assert_eq!(state.editor.draft(), format!("{DRAFT}j"));
}
