use super::*;
use crate::terminal::tick_list::{TickGroup, TickListEntry};

fn shell(id: Uuid, title: &str) -> TickListEntry {
    TickListEntry {
        id: id.to_string(),
        group: TickGroup::Shells,
        glyph: Glyph::ShellTick,
        title: title.into(),
        kind: "Shell".into(),
        status: "exit 0".into(),
        updated_at_ms: Some(1000),
        running: false,
    }
}

#[test]
fn tick_list_follows_agent_and_restores_selection_without_leaking_old_results() {
    let mut state = State::new(true, 140, 25);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    state.switch_view_chat(Some(a));
    state.tick_list.replace(
        TickGroup::Shells,
        vec![shell(first, "A1"), shell(second, "A2")],
    );
    state.tick_list.focus(false, None);
    state.tick_list.select(&second.to_string());
    state.switch_view_chat(Some(b));
    assert!(state.tick_list.entries().is_empty());
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "B")]);
    state.update_tick_entries(
        a,
        TickGroup::Shells,
        vec![shell(first, "A1"), shell(second, "A2")],
    );
    assert_eq!(state.tick_list.entries()[0].title, "B");
    state.switch_view_chat(Some(a));
    assert_eq!(state.tick_list.entries().len(), 2);
    assert_eq!(state.tick_list.selected().unwrap().title, "A2");
    assert!(state.tick_list.focused);
    state.switch_view_chat(None);
    assert!(!state.tick_list_visible());
}

#[test]
fn tick_list_width_boundary_and_overlay_do_not_change_visibility_preference() {
    let mut state = State::new(true, 121, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "A")]);
    assert!(state.tick_list_visible());
    state.width = 119;
    assert!(!state.tick_list_visible());
    state.prompt_enabled = true;
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(state.tick_list_visible());
    assert!(state.tick_list.overlay);
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.tick_list_visible());
    assert_eq!(
        state.tick_list_preference,
        tickloop_agent::settings::TickListPreference::Auto
    );
}

#[test]
fn shell_tick_renders_in_right_list_and_finished_remove_keeps_other_rows() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.tick_list.replace(
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
    state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(state.tick_list.entries().is_empty());
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.editor.draft(), "j");
}

#[test]
fn empty_tick_list_has_no_column_overlay_hint_or_mouse_target() {
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(!state.tick_list.overlay);
    assert!(!state.tick_list.focused);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.tick_toggle_target.is_none());
    assert!(
        !state
            .rendered_screen
            .iter()
            .any(|line| line.contains("Ticks") || line.contains("/artifacts"))
    );
}

#[test]
fn ticks_toggle_mouse_target_requests_saved_visibility_without_touching_the_editor() {
    let mut state = State::new(true, 100, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
    state.draw_frame(&mut Vec::new()).unwrap();
    let target = state.tick_toggle_target.as_ref().unwrap();
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
        Some(InputEvent::ToggleTickList { show: true })
    ));
    assert!(!state.tick_list.overlay);
}

#[test]
fn tick_columns_preserve_full_width_input_dividers_cursor_and_mouse_geometry() {
    let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    for width in [140, 100, 25] {
        let mut state = State::new(true, width, 30);
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.prompt_enabled = true;
        state
            .editor
            .replace_draft("unchanged input across list toggle".into(), 20);
        state.status_area.update(
            "Auto".into(),
            "model".into(),
            Some((10, 100)),
            Vec::new(),
            Vec::new(),
        );
        state
            .tick_list
            .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
        state
            .scrollback
            .append(&format!("{}\n", "conversation ".repeat(25)));
        state.tick_list_preference = tickloop_agent::settings::TickListPreference::Hidden;
        let (hidden, hidden_row, hidden_column) = state.frame();
        let hidden_target = state
            .tick_toggle_target
            .as_ref()
            .map(|target| (target.row, target.columns.clone()));
        state.tick_list_preference = tickloop_agent::settings::TickListPreference::Auto;
        state.tick_list.overlay = width <= 120;
        let (shown, shown_row, shown_column) = state.frame();
        assert_eq!(shown, hidden, "input rows changed at {width} columns");
        assert_eq!((shown_row, shown_column), (hidden_row, hidden_column));
        assert_eq!(
            state
                .tick_toggle_target
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
    assert_eq!(state.tick_list.entries()[0].title, "Quote terminal");
    assert_eq!(state.tick_list.entries()[0].kind, "Terminal");
    assert_eq!(state.tick_list.entries()[0].status, "live · 60s");
    let id = state.tick_list.entries()[0].id.clone();
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.update_tick_entries(chat, TickGroup::Subscribed, vec![]);
    assert!(state.tick_list.entries().is_empty());
    state.switch_view_chat(Some(chat));
    assert!(state.tick_list.entries().iter().all(|entry| entry.id != id));
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
    let mut state = console.state.lock().unwrap();
    assert!(!state.tick_list.metadata.contains_key(&id));
    assert!(state.tick_list.entries().is_empty());
    state.switch_view_chat(Some(a));
    assert_eq!(
        state.tick_list.metadata[&id].as_ref().unwrap().title,
        "A metadata"
    );
}

#[test]
fn failed_tick_list_save_keeps_the_current_visibility_and_focus() {
    let directory = tempfile::tempdir().unwrap();
    let settings = crate::settings::UserSettings::in_root(directory.path());
    std::fs::write(directory.path().join("settings.json"), "invalid JSON").unwrap();
    let mut state = State::new(true, 140, 25);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state
        .tick_list
        .replace(TickGroup::Shells, vec![shell(Uuid::new_v4(), "echo A")]);
    state.tick_list.focus(false, None);
    let console = tests::console_with_state(state);
    assert!(crate::terminal::set_tick_list_visible(&console, Some(&settings), false).is_err());
    let state = console.state.lock().unwrap();
    assert!(state.tick_list_visible());
    assert!(state.tick_list.focused);
    assert_eq!(
        state.tick_list_preference,
        crate::settings::TickListPreference::Auto
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
    assert_eq!(state.tick_list.entries().len(), 1);
    assert_eq!(state.tick_list.entries()[0].kind, "Artifact");
    assert_eq!(state.tick_list.entries()[0].updated_at_ms, Some(1000));
    let mut artifact = shell(Uuid::new_v4(), "Added document");
    artifact.id = "artifact:https://docs.example/doc".into();
    artifact.group = TickGroup::Artifacts;
    state
        .tick_list
        .replace(TickGroup::Artifacts, vec![artifact]);
    assert_eq!(state.tick_list.entries().len(), 1);
    assert_eq!(state.tick_list.entries()[0].group, TickGroup::Artifacts);
}
