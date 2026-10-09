use super::*;

#[test]
fn workspace_trust_requires_visible_path_and_positions_the_real_choice_cursor() {
    let mut state = State::new(true, 80, 4);
    state.workspace_trust = Some(WorkspaceTrust::new(PathBuf::from("/workspace/project")));
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    let mut output = Vec::new();
    state.draw(&mut output).unwrap();
    assert!(state.rendered_screen.join("\n").contains("Resize"));
    state.resize(80, 24);
    output.clear();
    state.draw(&mut output).unwrap();
    let row = state
        .rendered_screen
        .iter()
        .position(|line| line.contains("Yes, I trust this folder"))
        .unwrap();
    let ansi = std::str::from_utf8(&output).unwrap();
    assert!(
        ansi.ends_with(&format!("\x1b[{};1H\x1b[?25h", row + 1)),
        "{ansi:?}"
    );
    assert!(
        state
            .rendered_screen
            .iter()
            .any(|line| line == "/workspace/project")
    );
    assert!(
        !state
            .rendered_screen
            .iter()
            .any(|line| line.contains("Select: arrows"))
    );
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::Choice { .. })
    ));
    output.clear();
    state.draw(&mut output).unwrap();
    assert!(String::from_utf8(output).unwrap().ends_with("\x1b[?25l"));
}

#[test]
fn workspace_trust_reserves_the_entire_wrapped_sanitized_path_before_prose() {
    let path = format!("/workspace/{}\x1b[31m", "long目录-segment/".repeat(9));
    let mut state = State::new(true, 30, 24);
    state.workspace_trust = Some(WorkspaceTrust::new(PathBuf::from(&path)));
    let (lines, row, column) = state.styled_frame();
    let displayed_path = lines
        .iter()
        .flat_map(|line| &line.spans)
        .filter(|span| span.role == Some(Role::Strong))
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert_eq!(displayed_path, super::super::safe_line(&path));
    assert!(lines[row].text().contains("No, exit"));
    assert_eq!(column, 0);
    let visible = lines
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(visible.contains("Yes, I trust this folder"));
    assert!(visible.contains("Enter confirm · Esc cancel"));
    state.resize(30, 8);
    state.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        Some(InputEvent::Cancel { .. })
    ));
}

#[test]
fn workspace_trust_modal_owns_input_and_keeps_choices_visible_across_sizes() {
    for (width, height) in [(120, 30), (80, 24), (40, 12), (20, 4), (2, 1)] {
        let mut state = State::new(true, width, height);
        state.prompt_enabled = true;
        state.insert("hidden jk draft");
        state.chat = Some(Uuid::from_u128(9001));
        state.monitor_preference = crate::settings::MonitorPreference::Auto;
        state.monitor.replace(
            TickGroup::Shells,
            vec![MonitorEntry {
                instance: crate::tick_stream::InstanceRef::new("shell", "hidden").unwrap(),
                group: TickGroup::Shells,
                glyph: Glyph::ShellTick,
                title: "hidden Shell Tick".into(),
                kind: "Shell".into(),
                status: "running".into(),
                owner: crate::tick_adapter::TickOwner::Unknown,
                updated_at_ms: None,
                resources: None,
                actions: Vec::new(),
            }],
        );
        state.monitor.overlay = true;
        assert!(state.monitor_visible());
        state.workspace_trust = Some(WorkspaceTrust::new(PathBuf::from(
            "/workspace/unsafe\x1b[31m-name",
        )));
        state.draw(&mut Vec::new()).unwrap();
        let frame = state.rendered_screen.join("\n");
        assert!(!frame.contains("hidden jk draft"));
        assert!(!frame.contains("hidden Shell Tick"));
        assert!(!frame.contains("\x1b"));
        assert!(
            state
                .rendered_screen
                .iter()
                .all(|line| display_width(line) <= state.width)
        );
        assert_eq!(state.rendered_screen.len(), state.height);
        if width >= 40 {
            assert!(frame.contains("Accessing workspace:"));
            assert!(frame.contains("No, exit") && frame.contains("Yes, I trust this folder"));
        }
        for event in [
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        ] {
            assert!(state.key(event).is_none());
        }
        state.paste("yes\n/exit");
        assert_eq!(state.editor.draft(), "hidden jk draft");
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .is_none()
        );
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(InputEvent::Cancel { .. })
        ));
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
    }
}

#[test]
fn workspace_trust_acceptance_uses_navigation_and_retains_semantic_path_styles() {
    for navigation in [
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
    ] {
        let mut state = State::new(true, 80, 24);
        state.workspace_trust = Some(WorkspaceTrust::new(PathBuf::from("/workspace/project")));
        let (lines, _, _) = state.styled_frame();
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.text == "/workspace/project" && span.role == Some(Role::Strong))
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.text == "Accessing workspace:"
                    && span.role == Some(Role::DialogTitle))
        );
        state.key(navigation);
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(InputEvent::Choice {
                index: TRUST_ACCEPT_CHOICE_INDEX,
                ..
            })
        ));
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        state
            .workspace_trust
            .as_mut()
            .unwrap()
            .failed("cannot save \x1b[31mtrust\nretry".into());
        let text = state
            .styled_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains('\x1b'));
        assert!(text.contains("cannot save"));
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(InputEvent::Cancel { .. })
        ));
    }
}
