use super::*;

const COMMAND: Uuid = Uuid::from_u128(1);
const LATER_COMMAND: Uuid = Uuid::from_u128(2);
const BASH_CALL: &str = "● Bash · Run one failing PTY test";
const BASH_CALL_ID: &str = "call-bash";
const HINT: &str = "(ctrl+b to run in background)";

fn control(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

fn frame_lines(console: &Console) -> Vec<String> {
    let mut state = console.state.lock().unwrap();
    state.draw(&mut std::io::sink()).unwrap();
    state.rendered_screen.clone()
}

/// A console showing a running Bash call above the Working row, with the call first seen at the
/// returned instant.
fn running_bash_console() -> (Console, Instant) {
    let mut state = State::new(true, 80, 24);
    state.prompt_enabled = true;
    state.chat = Some(Uuid::from_u128(100));
    let console = super::tests::console_with_state(state);
    let (dot, call) = BASH_CALL.split_at("● ".len());
    console
        .write_tool_call(
            Uuid::from_u128(200),
            BASH_CALL_ID,
            &[
                (dot.to_owned(), Some(Role::MutedText)),
                (call.to_owned(), None),
            ],
            ToolCallState::Running,
            None,
        )
        .unwrap();
    console
        .set_activity(Some(
            &[(
                "• Working (1s · esc to interrupt)".to_owned(),
                Some(Role::MutedText),
            )][..],
        ))
        .unwrap();
    let seen = Instant::now();
    assert!(
        !console
            .state
            .lock()
            .unwrap()
            .set_foreground_commands(&[(COMMAND, Some(BASH_CALL_ID.to_owned()))], seen)
    );
    (console, seen)
}

#[test]
fn the_hint_appears_under_the_running_bash_call_once_it_passes_claude_codes_progress_threshold() {
    let (console, seen) = running_bash_console();
    {
        let mut state = console.state.lock().unwrap();
        assert!(!state.refresh_foreground_command_hint(seen + Duration::from_millis(1999)));
    }
    assert!(!frame_lines(&console).iter().any(|line| line.contains(HINT)));

    assert!(
        console
            .state
            .lock()
            .unwrap()
            .refresh_foreground_command_hint(seen + FOREGROUND_COMMAND_HINT_DELAY)
    );
    let frame = frame_lines(&console);
    let call = frame
        .iter()
        .position(|line| line.contains(BASH_CALL))
        .unwrap_or_else(|| panic!("{frame:#?}"));
    assert_eq!(
        frame[call + 1].trim_end(),
        format!("     {HINT}"),
        "{frame:#?}"
    );
    let working = frame
        .iter()
        .position(|line| line.contains("• Working"))
        .unwrap_or_else(|| panic!("{frame:#?}"));
    assert!(call + 1 < working, "{frame:#?}");
}

#[test]
fn the_hint_stays_under_its_bash_call_when_a_message_follows_the_call() {
    let (console, seen) = running_bash_console();
    console
        .write_roles(&[("\n❯ check the logs too\n".to_owned(), None)])
        .unwrap();
    console
        .state
        .lock()
        .unwrap()
        .refresh_foreground_command_hint(seen + FOREGROUND_COMMAND_HINT_DELAY);

    let frame = frame_lines(&console);
    let call = frame
        .iter()
        .position(|line| line.contains(BASH_CALL))
        .unwrap_or_else(|| panic!("{frame:#?}"));
    assert_eq!(
        frame[call + 1].trim_end(),
        format!("     {HINT}"),
        "{frame:#?}"
    );
    let message = frame
        .iter()
        .position(|line| line.contains("check the logs too"))
        .unwrap_or_else(|| panic!("{frame:#?}"));
    assert!(call + 1 < message, "{frame:#?}");
    assert_eq!(
        frame.iter().filter(|line| line.contains(HINT)).count(),
        1,
        "{frame:#?}"
    );

    assert!(
        console
            .state
            .lock()
            .unwrap()
            .set_foreground_commands(&[], seen + FOREGROUND_COMMAND_HINT_DELAY)
    );
    let frame = frame_lines(&console);
    assert!(!frame.iter().any(|line| line.contains(HINT)), "{frame:#?}");
    let call = frame
        .iter()
        .position(|line| line.contains(BASH_CALL))
        .unwrap_or_else(|| panic!("{frame:#?}"));
    assert!(frame[call + 1].trim().is_empty(), "{frame:#?}");
}

#[test]
fn only_commands_past_the_threshold_move_to_the_background() {
    let (console, seen) = running_bash_console();
    let mut state = console.state.lock().unwrap();
    let later = seen + Duration::from_secs(1);
    state.set_foreground_commands(
        &[
            (COMMAND, Some(BASH_CALL_ID.to_owned())),
            (LATER_COMMAND, None),
        ],
        later,
    );

    let backgroundable =
        |state: &State, now| state.backgroundable_commands(now).collect::<Vec<_>>();
    assert_eq!(
        backgroundable(&state, seen + FOREGROUND_COMMAND_HINT_DELAY),
        [COMMAND]
    );
    assert_eq!(
        backgroundable(&state, later + FOREGROUND_COMMAND_HINT_DELAY),
        [COMMAND, LATER_COMMAND]
    );
    state.set_foreground_commands(&[(LATER_COMMAND, None)], later);
    assert_eq!(
        backgroundable(&state, later + FOREGROUND_COMMAND_HINT_DELAY),
        [LATER_COMMAND]
    );
}

#[test]
fn ctrl_b_backgrounds_the_command_only_while_its_hint_shows() {
    let (console, seen) = running_bash_console();
    let mut state = console.state.lock().unwrap();
    state.insert("ab");

    assert!(state.key(control('b')).is_none());
    assert_eq!(
        state.editor.cursor(),
        1,
        "before the hint ctrl+b moves the cursor"
    );

    state.refresh_foreground_command_hint(seen + FOREGROUND_COMMAND_HINT_DELAY);
    assert!(matches!(
        state.key(control('b')),
        Some(InputEvent::BackgroundForegroundShell)
    ));
    assert_eq!(state.editor.draft(), "ab");
    assert_eq!(state.editor.cursor(), 1);

    assert!(state.set_foreground_commands(&[], seen + FOREGROUND_COMMAND_HINT_DELAY));
    assert!(!state.scrollback.shows_foreground_command_hint());
    assert!(state.key(control('b')).is_none());
    assert_eq!(
        state.editor.cursor(),
        0,
        "after the command leaves, ctrl+b edits again"
    );
}

#[test]
fn ctrl_x_ctrl_b_backgrounds_the_command_and_a_mismatch_stays_out_of_the_draft() {
    let (console, seen) = running_bash_console();
    let mut state = console.state.lock().unwrap();
    state.refresh_foreground_command_hint(seen + FOREGROUND_COMMAND_HINT_DELAY);

    assert!(state.key(control('x')).is_none());
    assert!(matches!(
        state.key(control('b')),
        Some(InputEvent::BackgroundForegroundShell)
    ));

    assert!(state.key(control('x')).is_none());
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.editor.draft().is_empty());
    assert!(state.shell_background_chord_started_at.is_none());

    state.key(control('x'));
    state.paste("pasted");
    assert_eq!(state.editor.draft(), "pasted");
    assert!(state.shell_background_chord_started_at.is_none());
}

#[test]
fn ctrl_b_leaves_the_command_alone_where_its_hint_does_not_have_the_keys() {
    let (console, seen) = running_bash_console();
    let mut state = console.state.lock().unwrap();
    state.refresh_foreground_command_hint(seen + FOREGROUND_COMMAND_HINT_DELAY);

    state.menu = Some(Menu {
        id: Uuid::new_v4(),
        title: "Run this command?".into(),
        context: Vec::new(),
        markdown: None,
        options: vec!["Allow once".into(), "Deny".into()],
        selected: 0,
        confirmation: true,
    });
    state.menu_focused = true;
    assert!(state.key(control('b')).is_none());
    state.menu = None;
    state.menu_focused = false;

    state.start_transcript();
    assert!(state.key(control('b')).is_none());
    state.transcript = None;

    assert!(matches!(
        state.key(control('b')),
        Some(InputEvent::BackgroundForegroundShell)
    ));
}

#[test]
fn switching_the_shown_chat_drops_the_previous_chats_commands() {
    let (console, seen) = running_bash_console();
    let mut state = console.state.lock().unwrap();
    state.refresh_foreground_command_hint(seen + FOREGROUND_COMMAND_HINT_DELAY);

    state.switch_view_chat(Some(Uuid::from_u128(101)));

    assert!(!state.scrollback.shows_foreground_command_hint());
    assert!(
        state
            .backgroundable_commands(seen + FOREGROUND_COMMAND_HINT_DELAY)
            .next()
            .is_none()
    );
    state.insert("ab");
    assert!(state.key(control('b')).is_none());
    assert_eq!(state.editor.cursor(), 1);
}
