use super::*;

fn console() -> tests::InteractiveConsoleFixture {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.chat = Some(Uuid::from_u128(8001));
    tests::InteractiveConsoleFixture::new(state)
}

fn show(console: &Console, text: &str) -> super::super::prompt_suggestion::SuggestionRequest {
    let request = console
        .begin_prompt_suggestion(Uuid::from_u128(8001))
        .unwrap()
        .unwrap();
    console
        .finish_prompt_suggestion(&request, Some(text.into()))
        .unwrap();
    request
}

#[test]
fn prompt_suggestion_is_ghost_text_until_tab_accepts_and_enter_submits() {
    let console = console();
    show(&console, "进度怎么样了");
    let mut state = console.state.lock().unwrap();
    assert!(state.editor.draft().is_empty());
    let (lines, _, column) = state.normal_styled_frame();
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.text.contains("进度怎么样了") && span.role == Some(Role::MutedText))
    );
    assert_eq!(column, 2);
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(state.editor.draft(), "进度怎么样了");
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Prompt { text, .. }) if text == "进度怎么样了")
    );
}

#[test]
fn prompt_suggestion_right_accepts_before_the_tick_list_and_empty_enter_submits_once() {
    for code in [KeyCode::Right, KeyCode::Enter] {
        let console = console();
        console
            .state
            .lock()
            .unwrap()
            .tick_list
            .replace(TickGroup::Artifacts, input_dialog_tests::fixture_ticks());
        show(&console, "run the tests");
        let mut state = console.state.lock().unwrap();
        let event = state.key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(!state.tick_list.focused);
        if code == KeyCode::Right {
            assert!(event.is_none());
            assert_eq!(state.editor.draft(), "run the tests");
        } else {
            assert!(
                matches!(event, Some(InputEvent::Prompt { text, .. }) if text == "run the tests")
            );
            assert!(
                !matches!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(InputEvent::Prompt { text, .. }) if text == "run the tests")
            );
        }
    }
}

#[test]
fn prompt_suggestion_typing_pasting_switching_and_new_turn_cancel_late_results() {
    for action in ["typing", "pasting", "switching", "new turn", "off"] {
        let console = console();
        let request = console
            .begin_prompt_suggestion(Uuid::from_u128(8001))
            .unwrap()
            .unwrap();
        match action {
            "typing" => {
                console
                    .state
                    .lock()
                    .unwrap()
                    .key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
            }
            "pasting" => console.state.lock().unwrap().paste("user draft"),
            "switching" => {
                console.select_chat(Some(Uuid::from_u128(8002))).unwrap();
                console.select_chat(Some(Uuid::from_u128(8001))).unwrap();
            }
            "new turn" => console.clear_prompt_suggestion().unwrap(),
            "off" => console.set_prompt_suggestion_enabled(false).unwrap(),
            _ => unreachable!(),
        }
        assert!(request.cancel.is_cancelled(), "{action}");
        console
            .finish_prompt_suggestion(&request, Some("stale suggestion".into()))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        assert!(
            !state
                .normal_styled_frame()
                .0
                .iter()
                .any(|line| line.text().contains("stale suggestion")),
            "{action}"
        );
        state.edit(EditorCommand::Clear);
        assert!(state.visible_prompt_suggestion().is_none(), "{action}");
    }
}

#[test]
fn prompt_suggestion_dismisses_with_escape_and_dialogs_receive_their_keys_first() {
    let console = console();
    show(&console, "run the tests");
    let menu_id = Uuid::new_v4();
    console
        .show_focused_menu(Menu {
            id: menu_id,
            title: "Question".into(),
            context: vec![],
            options: vec!["Answer".into()],
            selected: 0,
            confirmation: false,
        })
        .unwrap();
    let mut state = console.state.lock().unwrap();
    assert!(state.visible_prompt_suggestion().is_none());
    assert!(
        !state
            .styled_frame()
            .0
            .iter()
            .any(|line| line.text().contains("run the tests"))
    );
    assert!(
        matches!(state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(InputEvent::Cancel { menu_id: id }) if id == menu_id)
    );
    assert_eq!(state.prompt_suggestion.as_deref(), Some("run the tests"));
    drop(state);
    console.finish_menu_submission(menu_id).unwrap();
    console.clear_menu(menu_id).unwrap();
    let mut state = console.state.lock().unwrap();
    state.menu_submission_pending = None;
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.editor.draft().is_empty());
    assert!(state.visible_prompt_suggestion().is_none());
}

#[test]
fn prompt_suggestion_opening_an_input_dialog_cancels_pending_generation() {
    let console = console();
    let request = console
        .begin_prompt_suggestion(Uuid::from_u128(8001))
        .unwrap()
        .unwrap();
    let menu_id = Uuid::new_v4();
    console
        .show_focused_menu(Menu {
            id: menu_id,
            title: "Question".into(),
            context: vec![],
            options: vec!["Answer".into()],
            selected: 0,
            confirmation: false,
        })
        .unwrap();
    assert!(request.cancel.is_cancelled());
    console
        .finish_prompt_suggestion(&request, Some("late suggestion".into()))
        .unwrap();
    assert!(console.state.lock().unwrap().prompt_suggestion.is_none());
}

#[test]
fn prompt_suggestion_is_suppressed_for_typed_noninteractive_delegated_and_dialog_inputs() {
    for condition in [
        "typed",
        "noninteractive",
        "delegated",
        "dialog",
        "off",
        "shell",
    ] {
        let console = console();
        {
            let mut state = console.state.lock().unwrap();
            match condition {
                "typed" => state.insert("existing draft"),
                "noninteractive" => state.interactive = false,
                "delegated" => state.visible_delegated = true,
                "dialog" => state.help_open = true,
                "off" => state.prompt_suggestion_enabled = false,
                "shell" => state.foreground_shell = true,
                _ => unreachable!(),
            }
        }
        assert!(
            console
                .begin_prompt_suggestion(Uuid::from_u128(8001))
                .unwrap()
                .is_none(),
            "{condition}"
        );
    }
}

#[tokio::test]
async fn prompt_suggestion_settings_menu_saves_off_and_cancels_generation_immediately() {
    let directory = tempfile::tempdir().unwrap();
    let settings = tickloop_agent::settings::UserSettings::in_root(directory.path());
    let agent = crate::terminal::execution::ConsoleExecution::new_embedded(
        crate::terminal::test_support::config(),
        Arc::new(crate::terminal::test_support::ScriptedModel::new(vec![])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let console = console();
    console.select_chat(Some(chat)).unwrap();
    let request = console.begin_prompt_suggestion(chat).unwrap().unwrap();
    let mut menus = crate::terminal::menus::Menus::new(Some(settings.clone()), None);
    menus
        .settings(
            &agent,
            &console,
            crate::terminal::new_agent_draft::ConfigTarget::Agent(chat),
        )
        .await
        .unwrap();
    let setting_index = console
        .state
        .lock()
        .unwrap()
        .menu
        .as_ref()
        .unwrap()
        .options
        .iter()
        .position(|option| option == "Prompt suggestions: On")
        .expect("Prompt suggestions is available in Settings");
    menus
        .choose(
            &agent,
            &console,
            menus.id().unwrap(),
            Some(setting_index),
            None,
        )
        .await
        .unwrap();
    assert!(request.cancel.is_cancelled());
    assert!(!settings.load().prompt_suggestion_enabled());
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .menu
            .as_ref()
            .unwrap()
            .options
            .iter()
            .any(|option| option == "Prompt suggestions: Off")
    );
}
