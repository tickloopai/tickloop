use super::*;
use tickloop_harness::protocol::{
    ApprovalHistoryView, ApprovalPath, ApprovalRecord, ApprovalResult, AutoRule, AutoRuleSource,
    DenialPersistence, DisplaySummary, LoopState, SubagentRegistration,
};

fn picker_menu(options: usize) -> Menu {
    Menu {
        id: Uuid::new_v4(),
        title: "Settings".into(),
        context: vec!["Changes apply immediately".into()],
        markdown: None,
        options: (0..options)
            .map(|index| format!("Option {index}"))
            .collect(),
        selected: 0,
        confirmation: false,
    }
}

#[test]
fn merged_user_menu_commit_hint_survives_content_height_and_resize_only_frames() {
    for width in [40, 80, 140] {
        for height in [3, 5, 6, 10, 24, 40] {
            for (picker_kind, editing) in [
                (Some(PickerKind::Model), false),
                (Some(PickerKind::Model), true),
                (None, false),
                (None, true),
            ] {
                let mut state = State::new(true, width, height);
                let menu = picker_menu(4);
                let id = menu.id;
                state.menu = Some(menu);
                state.menu_focused = true;
                state.picker_kind = picker_kind;
                if picker_kind.is_some() {
                    state.picker_presentation.headers = vec![(0, "Fixture Provider".into())];
                }
                state.picker_presentation.hint = "enter apply · esc back".into();
                if editing {
                    state.menu_comment = Some(MenuCommentState {
                        options: vec![0],
                        values: HashMap::from([(0, "fixture-value".into())]),
                        label: "Thinking",
                        editing: true,
                        cursor: "fixture-value".len(),
                        secret: false,
                        standalone_input: true,
                    });
                }
                let settled_height = state.input_dialog_height();
                state.begin_menu_submission(id);
                let frame = state.styled_frame().0;
                assert_eq!(frame.len(), settled_height);
                assert_eq!(
                    frame.last().unwrap().text(),
                    MENU_SUBMISSION_PENDING_HINT,
                    "{width} columns / {height} rows / kind {picker_kind:?} / editing {editing}: {frame:?}"
                );
                for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('j')] {
                    assert!(state.key(KeyEvent::new(code, KeyModifiers::NONE)).is_none());
                    assert_eq!(state.menu.as_ref().unwrap().id, id);
                    assert_eq!(state.menu.as_ref().unwrap().selected, 0);
                    if editing {
                        let field = state.menu_comment.as_ref().unwrap();
                        assert!(field.editing);
                        assert_eq!(field.values[&0], "fixture-value");
                    }
                }
                state.mark_menu_submission_finished(id);
                assert_eq!(state.input_dialog_height(), settled_height);
                let frame = state.styled_frame().0;
                assert!(
                    !frame
                        .last()
                        .unwrap()
                        .text()
                        .contains(MENU_SUBMISSION_PENDING_HINT)
                );
            }
        }
    }
}

#[test]
fn merged_generic_menu_completion_repaints_the_next_page_without_an_extra_key() {
    let mut state = State::new(true, 80, 24);
    let menu = picker_menu(3);
    let submitted_id = menu.id;
    state.menu = Some(menu);
    state.menu_focused = true;
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InputEvent::Choice { menu_id, .. }) if menu_id == submitted_id
    ));
    let console = tests::console_with_state(state);
    let mut next = picker_menu(3);
    next.title = "Rewind actions".into();
    next.selected = 1;
    let next_id = next.id;
    console.show_focused_menu(next).unwrap();
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .rendered_screen
            .last()
            .unwrap()
            .contains(MENU_SUBMISSION_PENDING_HINT)
    );
    console.finish_menu_submission(submitted_id).unwrap();
    let mut state = console.state.lock().unwrap();
    assert!(
        !state
            .rendered_screen
            .last()
            .unwrap()
            .contains(MENU_SUBMISSION_PENDING_HINT)
    );
    assert!(state.rendered_screen.last().unwrap().contains("esc back"));
    assert_eq!(state.menu.as_ref().unwrap().id, next_id);
    assert_eq!(state.menu.as_ref().unwrap().selected, 1);
    assert!(state.picker_kind.is_none());
    state.release_finished_submissions();
    state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(state.menu.as_ref().unwrap().selected, 2);
}

#[test]
fn merged_provider_diagnostics_resize_with_the_selected_connection_and_live_clear() {
    for height in [24, 40] {
        let chat = Uuid::new_v4();
        let mut state = State::new(true, 160, height);
        let mut menu = picker_menu(2);
        menu.context.clear();
        let id = menu.id;
        state.menu = Some(menu);
        state.menu_focused = true;
        state.picker_kind = Some(PickerKind::Model);
        state.picker_presentation.hint = "enter apply · esc back".into();
        state.picker_presentation.failure_target = Some(chat);
        state.picker_presentation.failure_connections =
            vec![Some("deepseek-api".into()), Some("minimax-api".into())];
        assert_eq!(state.styled_frame().0.len(), 5);
        state.provider_failures.insert(
            chat,
            HashMap::from([(
                "deepseek-api".into(),
                tickloop_harness::protocol::ProviderHttpFailure {
                    provider_id: "deepseek-api".into(),
                    provider_name: "DeepSeek".into(),
                    status: 403,
                    class: tickloop_harness::protocol::ProviderFailureClass::UsageLimit,
                    message: "Bounded provider notice".into(),
                    hint: "Check allowance".into(),
                    retry_after: None,
                    retryable: false,
                },
            )]),
        );
        let frame = state.styled_frame().0;
        assert_eq!(frame.len(), 7);
        assert!(frame.iter().any(|line| line.text().contains("HTTP 403")));
        assert!(
            frame
                .iter()
                .any(|line| line.text().contains("Check allowance"))
        );
        state.picker_presentation.failure_message = true;
        assert_eq!(state.styled_frame().0.len(), 8);
        state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let frame = state.styled_frame().0;
        assert_eq!(frame.len(), 5);
        assert!(!frame.iter().any(|line| line.text().contains("HTTP 403")));
        state.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(state.styled_frame().0.len(), 8);
        state.provider_failures.insert(chat, HashMap::new());
        assert_eq!(state.styled_frame().0.len(), 5);
        assert_eq!(state.menu.as_ref().unwrap().id, id);
        assert_eq!(state.menu.as_ref().unwrap().selected, 0);
    }
}

#[test]
fn merged_dialog_heights_hide_and_restore_the_family_panel_and_failure_status() {
    use crate::agent_console::AttentionGroup;
    for height in [6, 10, 24, 40] {
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut state = State::new(true, 140, height);
        state.chat = Some(root);
        state.prompt_enabled = true;
        state.insert("preserved family draft");
        state.provider_failure = Some("Provider allowance warning".into());
        state
            .status_area
            .update("Ask".into(), "Fixture model".into(), None, None);
        let rows = [root, child]
            .into_iter()
            .map(|id| AgentRow {
                agent_id: id,
                parent_id: (id == child).then_some(root),
                title: "Fixture family".into(),
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
                run_started_at: None,
                subagent: (id == child).then_some(SubagentRegistration {
                    child_chat_id: child.into(),
                    handle: "scoped-child".into(),
                    description: "Controlled panel".into(),
                    created_at: 1,
                    color: None,
                }),
            })
            .collect();
        state.status_area.update_agents(rows, Some(root));
        state.footer.update(state.status_area.available_panels());
        if !state.status_area.is_open() {
            state.status_area.toggle_agents();
        }
        let cursor = state.editor.cursor();
        if height >= 24 {
            let frame = state.styled_frame().0;
            assert!(
                frame
                    .iter()
                    .any(|line| line.text().contains("scoped-child")),
                "{frame:?}"
            );
            assert!(
                frame
                    .iter()
                    .any(|line| line.text().contains("Provider allowance warning")),
                "{frame:?}"
            );
        }
        for count in [2, 80, 2] {
            let menu = picker_menu(count);
            let id = menu.id;
            state.menu = Some(menu);
            state.picker_kind = Some(PickerKind::Actions);
            state.picker_presentation.hint = "enter change · esc close".into();
            state.menu_focused = true;
            let frame = state.styled_frame().0;
            assert!(frame.len() <= usize::from(height).saturating_sub(2).max(1));
            assert!(
                !frame.iter().any(|line| line.text().contains("scoped-child")
                    || line.text().contains("Subagents"))
            );
            state.clear_menu(id);
            assert!(state.status_area.is_open());
            assert_eq!(state.editor.draft(), "preserved family draft");
            assert_eq!(state.editor.cursor(), cursor);
            assert_eq!(
                state.provider_failure.as_deref(),
                Some("Provider allowance warning")
            );
        }
        if height >= 24 {
            let frame = state.styled_frame().0;
            assert!(
                frame
                    .iter()
                    .any(|line| line.text().contains("scoped-child"))
            );
            assert!(
                frame
                    .iter()
                    .any(|line| line.text().contains("Provider allowance warning"))
            );
        }
    }
}

#[test]
fn hidden_picker_resize_guard_leaves_transcript_navigation_search_and_close_available() {
    let mut state = State::new(true, 80, 24);
    let menu = picker_menu(2);
    let menu_id = menu.id;
    state.menu = Some(menu);
    state.picker_kind = Some(PickerKind::Actions);
    state.menu_focused = true;
    state.start_transcript();
    state.height = 5;
    state.transcript.as_mut().unwrap().1.update(
        (0..40)
            .map(|index| format!("Transcript row {index}"))
            .collect::<Vec<_>>()
            .into(),
    );
    state.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    let frame = state.transcript.as_mut().unwrap().1.frame(80, 5);
    assert!(
        frame
            .texts
            .iter()
            .any(|line| line.contains("Transcript row 0"))
    );
    state.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    state.paste("Transcript row 20");
    let frame = state.transcript.as_mut().unwrap().1.frame(80, 5);
    assert!(
        frame.texts[4].starts_with("  /Transcript row 20"),
        "{:?}",
        frame.texts
    );
    assert!(
        frame.texts[..2]
            .iter()
            .any(|line| line.contains("Transcript row 20"))
    );
    assert!(!state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 4,
            column: 0,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    ));
    state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(state.transcript.is_some());
    state.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(state.transcript.is_none());
    assert_eq!(state.menu.as_ref().unwrap().id, menu_id);
    assert_eq!(state.menu.as_ref().unwrap().selected, 0);
    assert!(state.input_dialog_requires_resize());
}

#[test]
fn content_height_short_settings_retains_rule_title_choices_and_hint() {
    for (width, height, expected) in [(80, 24, 6), (80, 40, 6), (20, 24, 7), (80, 8, 6)] {
        let mut state = State::new(true, width, height);
        state.prompt_enabled = true;
        let console = tests::console_with_state(state);
        console
            .show_presented_picker_menu(
                picker_menu(2),
                PickerKind::Actions,
                PickerPresentation {
                    hint: "enter · esc close".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut state = console.state.lock().unwrap();
        state.width = usize::from(width);
        state.height = usize::from(height);
        let (lines, cursor, _) = state.styled_frame();
        assert_eq!(lines.len(), expected, "{width} columns / {height} rows");
        assert_eq!(lines[0].text(), "▔".repeat(usize::from(width)));
        assert_eq!(lines[1].text(), "Settings");
        assert!(lines.last().unwrap().text().contains("esc close"));
        assert!(lines.iter().any(|line| line.text().contains("Option 0")));
        assert!(lines.iter().any(|line| line.text().contains("Option 1")));
        assert!(cursor < lines.len());
    }
}

#[test]
fn content_height_user_confirmation_is_natural_without_agent_request_ownership() {
    for height in [6, 24, 40] {
        let mut state = State::new(true, 100, height);
        let mut menu = picker_menu(2);
        menu.confirmation = true;
        state.menu = Some(menu);
        state.picker_kind = Some(PickerKind::Actions);
        state.picker_presentation.hint = "enter confirm · esc back".into();
        state.menu_focused = true;
        let frame = state.styled_frame().0;
        assert_eq!(frame.len(), if height == 6 { 4 } else { 6 });
        assert!(
            frame
                .iter()
                .any(|line| line.text().starts_with("❯ Option 0"))
        );
        assert!(frame.last().unwrap().text().contains("esc"));
    }
}

#[tokio::test]
async fn content_height_permission_rule_editor_stays_visible_or_blocks_input_when_tiny() {
    let directory = tempfile::tempdir().unwrap();
    let agent = crate::terminal::Agent::new_embedded(
        tickloop_harness::test_support::config(),
        Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        )),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut view = agent.permissions_view(chat).await.unwrap();
    view.execution_mode = tickloop_harness::protocol::ExecutionMode::Auto;
    let mut surface =
        PermissionsSurface::open(view, PermissionsReturn::ModePicker { settings: false });
    surface.open_auto();
    for code in [
        KeyCode::Down,
        KeyCode::Down,
        KeyCode::Enter,
        KeyCode::Char('a'),
    ] {
        surface.key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    let mut state = State::new(true, 100, 6);
    state.permissions_surface = Some(surface);
    let lines = state.styled_frame().0;
    assert!(
        lines.iter().any(|line| line.text().contains("Text")),
        "{lines:?}"
    );
    assert!(lines.last().unwrap().text().contains("esc"));
    state.paste("controlled rule");
    assert!(
        state
            .styled_frame()
            .0
            .iter()
            .any(|line| line.text().contains("controlled rule"))
    );
    state.height = 5;
    let before = state
        .permissions_surface
        .as_ref()
        .unwrap()
        .content_rows(99, 100);
    assert!(
        state
            .styled_frame()
            .0
            .last()
            .unwrap()
            .text()
            .contains("Resize")
    );
    state.paste("hidden input");
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(
        state
            .permissions_surface
            .as_ref()
            .unwrap()
            .content_rows(99, 100),
        before
    );
    state.height = 24;
    assert!(state.styled_frame().0.iter().any(
        |line| line.text().contains("controlled rule") && !line.text().contains("hidden input")
    ));
    agent.shutdown().await;
}

#[test]
fn content_height_resume_fits_empty_short_and_overflow_lists_without_moving_focus() {
    use crate::terminal::conversation_picker::{ConversationEntry, ConversationPickerPurpose};
    for (height, count, expected) in [
        (24, 0, 5),
        (24, 2, 8),
        (24, 20, 22),
        (40, 20, 38),
        (10, 2, 8),
    ] {
        let mut state = State::new(true, 80, height);
        let entries = (0..count)
            .map(|index| {
                ConversationEntry::new(
                    &format!("Chat {index}"),
                    &["Saved conversation".into()],
                    &[],
                )
            })
            .collect();
        let mut picker = ConversationPicker::new(ConversationPickerPurpose::Resume, entries);
        if count > 0 {
            assert!(picker.select_entry(count - 1));
        }
        state.conversation_picker = Some(picker);
        let id = state.conversation_picker.as_ref().unwrap().id();
        let (lines, _, _) = state.styled_frame();
        assert_eq!(lines.len(), expected, "{count} entries / {height} rows");
        assert!(lines[1].text().contains("Resume"));
        assert!(lines.last().unwrap().text().contains("esc"));
        assert_eq!(state.conversation_picker.as_ref().unwrap().id(), id);
        if count > 0 {
            assert_eq!(
                state.conversation_picker.as_ref().unwrap().selection(),
                Some(count - 1)
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.text().contains(&format!("Chat {}", count - 1)))
            );
        }
    }
}

#[test]
fn content_height_artifacts_fits_loading_groups_and_overflow() {
    use crate::terminal::artifacts_picker::ArtifactsPicker;
    use tickloop_harness::artifacts::{ArtifactMetadata, ArtifactPage};
    for (height, count, expected) in [
        (24, 0, 5),
        (24, 2, 10),
        (24, 20, 22),
        (40, 20, 38),
        (10, 2, 8),
    ] {
        let mut state = State::new(true, 100, height);
        let mut picker = ArtifactsPicker::new(
            None,
            "fixture@example.test".into(),
            HashSet::from(["item-0".into()]),
        );
        if count > 0 {
            let rows: Vec<ArtifactMetadata> = (0..count).map(|index| serde_json::from_value(serde_json::json!({"id":format!("item-{index}"),"title":format!("Artifact {index}"),"format":"markdown","access_role":"owner"})).unwrap()).collect();
            let request = picker.take_load().unwrap();
            picker.loaded(
                &request,
                Ok((
                    ArtifactPage {
                        artifacts: rows,
                        next_cursor: None,
                    },
                    HashSet::new(),
                    false,
                )),
            );
        }
        state.artifacts_picker = Some(picker);
        let (lines, _, _) = state.styled_frame();
        assert_eq!(lines.len(), expected, "{count} artifacts / {height} rows");
        assert!(lines[1].text().contains("Artifacts"));
        assert!(lines[2].text().contains("Search"));
        assert!(lines.last().unwrap().text().contains("esc"));
        if count == 2 && height >= 24 {
            assert!(lines.iter().any(|line| line.text() == "This Chat"));
            assert!(lines.iter().any(|line| line.text() == "Other artifacts"));
            assert!(lines.iter().any(|line| line.text().contains("Artifact 1")));
        }
    }
}

#[tokio::test]
async fn content_height_permission_management_history_fits_loaded_and_empty_records() {
    let directory = tempfile::tempdir().unwrap();
    let agent = crate::terminal::Agent::new_embedded(
        tickloop_harness::test_support::config(),
        Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        )),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut view = agent.permissions_view(chat).await.unwrap();
    view.execution_mode = tickloop_harness::protocol::ExecutionMode::AcceptEdits;
    let record = ApprovalRecord {
        human_reason: None,
        id: Uuid::new_v4(),
        chat_id: chat.into(),
        loop_id: Uuid::new_v4().into(),
        run_id: Uuid::new_v4().into(),
        operation_id: Uuid::new_v4(),
        call_id: "controlled".into(),
        at_ms: 1,
        tool: "Bash".into(),
        input_summary: DisplaySummary {
            text: "controlled operation".into(),
            truncated: false,
        },
        request_details: None,
        outcome: ApprovalResult::Deny,
        path: ApprovalPath::Human,
        direct_reason: None,
        denial_category: None,
        approval_source: None,
        reason: "controlled denial".into(),
        policy_revision: 1,
        matching_rules: Vec::new(),
        matching_rules_truncated: false,
        review: None,
    };
    for (height, count, expected) in [
        (24, 0, 6),
        (40, 2, 6),
        (10, 2, 6),
        (24, 40, 22),
        (40, 40, 38),
        (10, 40, 8),
    ] {
        let mut state = State::new(true, 100, height);
        let mut surface = PermissionsSurface::open(
            view.clone(),
            PermissionsReturn::ModePicker { settings: false },
        );
        surface.replace_denials(ApprovalHistoryView {
            records: (0..count)
                .map(|index| {
                    let mut entry = record.clone();
                    entry.id = Uuid::new_v4();
                    entry.tool = format!("Controlled {index}");
                    entry
                })
                .collect(),
            persistence: DenialPersistence::Disabled,
            load_warning: None,
        });
        surface.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        state.permissions_surface = Some(surface);
        let (lines, _, _) = state.styled_frame();
        assert_eq!(lines.len(), expected);
        let selected = if count == 0 {
            "No permission decisions".into()
        } else {
            format!("Controlled {}", count - 1)
        };
        assert!(
            lines.iter().any(|line| line.text().contains(&selected)),
            "{lines:?}"
        );
        assert!(lines.last().unwrap().text().contains("esc"));
    }
    for detail in [false, true] {
        let mut surface = PermissionsSurface::open(
            view.clone(),
            PermissionsReturn::ModePicker { settings: false },
        );
        surface.replace_denials(ApprovalHistoryView {
            records: vec![record.clone()],
            persistence: DenialPersistence::Disabled,
            load_warning: Some("Controlled load warning".into()),
        });
        if detail {
            surface.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        let mut state = State::new(true, 100, 6);
        state.permissions_surface = Some(surface);
        assert!(
            state
                .styled_frame()
                .0
                .last()
                .unwrap()
                .text()
                .contains("Resize")
        );
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
                .is_none()
        );
        state.height = 24;
        let frame = state.styled_frame().0;
        assert!(
            frame
                .iter()
                .any(|line| line.text().contains("Controlled load warning"))
        );
        assert!(frame.last().unwrap().text().contains("esc"));
        if detail {
            assert!(frame.last().unwrap().text().contains("r retry"));
        }
    }
    view.execution_mode = tickloop_harness::protocol::ExecutionMode::Auto;
    for (height, count, expected) in [
        (24, 0, 10),
        (40, 2, 12),
        (24, 40, 22),
        (40, 40, 38),
        (10, 40, 8),
    ] {
        view.auto_policy.rules = (0..count)
            .map(|index| AutoRule {
                clears: None,
                id: format!("controlled-{index}"),
                kind: tickloop_harness::protocol::AutoRuleKind::SoftAllow,
                text: format!("Controlled rule {index}"),
                source: AutoRuleSource::User,
            })
            .collect();
        let mut surface = PermissionsSurface::open(
            view.clone(),
            PermissionsReturn::ModePicker { settings: false },
        );
        surface.open_auto();
        for code in [KeyCode::Down, KeyCode::Down, KeyCode::Enter, KeyCode::End] {
            surface.key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        let mut state = State::new(true, 100, height);
        state.permissions_surface = Some(surface);
        let frame = state.styled_frame().0;
        assert_eq!(frame.len(), expected, "Auto group {count}/{height}");
        assert!(
            frame
                .iter()
                .any(|line| line.text().starts_with("❯ Restore group defaults")),
            "{frame:?}"
        );
        assert!(frame.last().unwrap().text().contains("esc"));
    }
    agent.shutdown().await;
}

#[test]
fn content_height_mouse_targets_use_resolved_height_in_both_renderers() {
    for renderer in [Renderer::Inline, Renderer::Fullscreen] {
        for (count, expected_height, selected) in [(2, 6, 1), (80, 22, 79), (2, 6, 1)] {
            let mut state = State::new(true, 140, 24);
            state.renderer = renderer;
            state.prompt_enabled = true;
            let mut menu = picker_menu(count);
            menu.selected = selected;
            let id = menu.id;
            state.menu = Some(menu);
            state.picker_kind = Some(PickerKind::Actions);
            state.menu_focused = true;
            state.draw(&mut Vec::new()).unwrap();
            assert_eq!(state.input_dialog_height(), expected_height);
            let rule = 24 - expected_height;
            state.pointer(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    row: rule as u16,
                    column: 3,
                    modifiers: KeyModifiers::NONE,
                },
                &mut Vec::new(),
            );
            assert!(state.pointer_input.is_none());
            let row = state
                .rendered_screen
                .iter()
                .position(|line| line.starts_with(&format!("❯ Option {selected}")))
                .unwrap();
            state.pointer(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    row: row as u16,
                    column: 3,
                    modifiers: KeyModifiers::NONE,
                },
                &mut Vec::new(),
            );
            assert!(
                matches!(state.pointer_input, Some(InputEvent::Choice { menu_id, index, .. }) if menu_id == id && index == selected)
            );
        }
    }
}

#[test]
fn content_height_tiny_picker_requires_resize_and_never_submits_hidden_controls() {
    for height in 1..=5 {
        let mut state = State::new(true, 80, height);
        let menu = picker_menu(2);
        let id = menu.id;
        state.menu = Some(menu);
        state.picker_kind = Some(PickerKind::Actions);
        state.menu_focused = true;
        state.prompt_enabled = true;
        let lines = state.styled_frame().0;
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none(),
            "{height} rows must not confirm without chrome"
        );
        assert!(lines.last().unwrap().text().contains("esc"));
        assert!(lines.last().unwrap().text().contains("Resize"));
        assert!(
            matches!(state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(InputEvent::Cancel { menu_id }) if menu_id == id)
        );
    }
}

#[test]
fn content_height_help_counts_columns_and_uses_the_full_available_budget() {
    for (width, height, expected) in [(160, 40, 12), (80, 24, 16), (32, 24, 22), (32, 10, 8)] {
        let mut state = State::new(true, width, height);
        state.help_open = true;
        let lines = state.styled_frame().0;
        assert_eq!(lines.len(), expected);
        assert_eq!(lines[1].text(), "Help");
        assert!(lines.last().unwrap().text().contains("esc"));
        state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!state.help_open);
    }
}

#[test]
fn content_height_model_provider_groups_fit_and_keep_the_last_choice_visible() {
    for kind in [PickerKind::Model, PickerKind::Provider] {
        for (width, height, count, expected) in [
            (140, 24, 4, 9),
            (80, 24, 4, 0),
            (80, 24, 50, 22),
            (140, 40, 50, 38),
            (32, 10, 50, 8),
        ] {
            let mut state = State::new(true, width, height);
            let mut menu = picker_menu(count);
            menu.title = if kind == PickerKind::Model {
                "Models"
            } else {
                "Providers"
            }
            .into();
            menu.context.clear();
            menu.selected = count - 1;
            state.menu = Some(menu);
            state.menu_focused = true;
            state.picker_kind = Some(kind);
            state.picker_presentation.headers =
                vec![(0, "Provider A".into()), (count / 2, "Provider B".into())];
            state.picker_presentation.hint = "enter · esc back".into();
            if kind == PickerKind::Provider {
                state.picker_presentation.compact_rows = (0..count)
                    .map(|index| CompactPickerRow {
                        label: format!("Option {index}"),
                        detail: "Signed in".into(),
                    })
                    .collect();
            }
            let expected = if expected == 0 {
                if kind == PickerKind::Provider { 13 } else { 9 }
            } else {
                expected
            };
            let id = state.menu.as_ref().unwrap().id;
            let before = state.input_dialog_height();
            let (lines, cursor, _) = state.styled_frame();
            assert_eq!(lines.len(), expected, "{kind:?}/{width}/{height}/{count}");
            assert_eq!(before, state.input_dialog_height());
            assert_eq!(state.menu.as_ref().unwrap().id, id);
            assert_eq!(state.menu.as_ref().unwrap().selected, count - 1);
            assert!(lines.iter().any(|line| line.text().contains("Provider B")));
            assert!(
                lines
                    .iter()
                    .any(|line| line.text().contains(&format!("Option {}", count - 1)))
            );
            assert!(lines.last().unwrap().text().contains("esc"));
            assert!(cursor < lines.len());
        }
    }
}

#[test]
fn content_height_permission_question_and_plan_requests_remain_fixed_for_every_content_size() {
    for kind in ["permission", "question", "plan"] {
        for (height, expected) in [(4, 1), (10, 6), (24, 12), (40, 12)] {
            for count in [2, 30] {
                let mut state = State::new(true, 100, height);
                let mut menu = picker_menu(count);
                menu.context = (0..count)
                    .map(|index| format!("Request details {index}"))
                    .collect();
                if kind == "permission" {
                    menu.confirmation = true;
                    state.request_layout = Some(menu.id);
                }
                state.menu = Some(menu);
                state.menu_focused = true;
                if kind == "question" {
                    state.question_menu = Some(QuestionMenu {
                        entry_option: None,
                        entry_text: String::new(),
                        multi_select: false,
                        navigable: false,
                    });
                }
                if kind == "plan" {
                    state.menu_plan = true;
                    state.menu_rule_role = Some(Role::PlanMode);
                }
                assert_eq!(
                    state.styled_frame().0.len(),
                    expected,
                    "{kind}/{height}/{count}"
                );
            }
        }
    }
}

#[test]
fn content_height_login_field_wraps_and_scopes_its_cursor_while_chrome_stays_visible() {
    for (width, height, draft, expected) in [
        (100, 24, "short".to_owned(), 8),
        (32, 40, "x".repeat(200), 14),
        (32, 10, "x".repeat(200), 8),
        (32, 6, "short".into(), 4),
    ] {
        let mut state = State::new(true, width, height);
        let mut menu = picker_menu(2);
        menu.title = "Browser login".into();
        menu.context = vec!["Authorize account".into(), "Then return here".into()];
        state.menu_link = Some((menu.id, "https://fixture.test".into()));
        state.menu = Some(menu);
        state.menu_focused = true;
        state.picker_kind = Some(PickerKind::Actions);
        state.menu_comment = Some(MenuCommentState {
            options: vec![0],
            values: HashMap::from([(0, draft.clone())]),
            label: "Callback URL",
            editing: true,
            cursor: draft.len(),
            secret: false,
            standalone_input: false,
        });
        let (lines, row, column) = state.styled_frame();
        assert_eq!(lines.len(), expected, "{width}/{height}");
        assert!(lines[1].text().contains("Browser login"));
        assert!(lines.last().unwrap().text().contains("esc"));
        assert!(row < lines.len());
        assert!(column < usize::from(width));
        assert_eq!(state.menu_comment.as_ref().unwrap().values[&0], draft);
    }
}

#[test]
fn content_height_resize_blocks_hidden_fields_paste_and_mouse_and_preserves_escape_layers() {
    let mut state = State::new(true, 80, 5);
    state.prompt_enabled = true;
    state.menu = Some(picker_menu(2));
    state.menu_focused = true;
    state.picker_kind = Some(PickerKind::Actions);
    state.menu_comment = Some(MenuCommentState {
        options: vec![0],
        values: HashMap::from([(0, "draft".into())]),
        label: "API key",
        editing: true,
        cursor: 5,
        secret: true,
        standalone_input: false,
    });
    state.styled_frame();
    state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    state.paste("hidden paste");
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 4,
            column: 0,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert!(state.pointer_input.is_none());
    assert_eq!(state.menu_comment.as_ref().unwrap().values[&0], "draft");
    assert_eq!(state.menu_comment.as_ref().unwrap().cursor, 5);
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .is_none()
    );
    assert!(!state.menu_comment_is_editing());
    assert!(matches!(
        state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        Some(InputEvent::Cancel { .. })
    ));
}

#[test]
fn content_height_request_preemption_and_agent_restore_recompute_without_resetting_user_state() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut state = State::new(true, 100, 24);
    state.prompt_enabled = true;
    let console = tests::console_with_state(state);
    console.select_chat(Some(first)).unwrap();
    let mut page = picker_menu(2);
    page.selected = 1;
    let id = page.id;
    console.show_picker_menu(page, PickerKind::Actions).unwrap();
    assert_eq!(console.state.lock().unwrap().styled_frame().0.len(), 6);
    let request = Uuid::new_v4();
    console.set_request_layout(Some(request)).unwrap();
    console.set_request_queue_count(2).unwrap();
    let mut page = picker_menu(30);
    page.id = request;
    page.confirmation = true;
    console.show_focused_menu(page).unwrap();
    assert_eq!(console.state.lock().unwrap().styled_frame().0.len(), 12);
    console.select_chat(Some(second)).unwrap();
    console
        .show_picker_menu(picker_menu(60), PickerKind::Actions)
        .unwrap();
    assert_eq!(console.state.lock().unwrap().styled_frame().0.len(), 22);
    console.select_chat(Some(first)).unwrap();
    assert_eq!(console.dialog_menu_id().unwrap(), Some(request));
    console.clear_menu(request).unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.styled_frame().0.len(), 6);
    assert_eq!(state.menu.as_ref().unwrap().id, id);
    assert_eq!(state.menu.as_ref().unwrap().selected, 1);
}

#[test]
fn content_height_pinned_message_and_monitor_column_share_the_upper_remainder() {
    for renderer in [Renderer::Inline, Renderer::Fullscreen] {
        let mut state = State::new(true, 140, 24);
        state.renderer = renderer;
        state.chat = Some(Uuid::new_v4());
        state
            .monitor
            .replace(TickGroup::Artifacts, input_dialog_tests::fixture_ticks());
        state.record_input("Height pinned message");
        for index in 0..5 {
            state.scrollback.append(&format!("Reply {index}\n"));
        }
        let mut page = picker_menu(2);
        page.context.clear();
        state.menu = Some(page);
        state.picker_kind = Some(PickerKind::Actions);
        state.menu_focused = true;
        state.picker_presentation.hint = "enter · esc back".into();
        state.draw(&mut Vec::new()).unwrap();
        assert_eq!(state.input_dialog_height(), 5);
        assert!(!state.rendered_screen[0].starts_with("❯ Height pinned message"));
        assert_eq!(
            state
                .rendered_screen
                .iter()
                .filter(|line| line.contains("Height pinned message"))
                .count(),
            1
        );
        state.menu.as_mut().unwrap().options =
            (0..80).map(|index| format!("Choice {index}")).collect();
        state.draw(&mut Vec::new()).unwrap();
        assert_eq!(state.input_dialog_height(), 22);
        assert!(state.rendered_screen[0].starts_with("❯ Height pinned message"));
        assert!(state.rendered_screen[0].contains("│ Artifacts (2)"));
        assert_eq!(state.rendered_screen[2], "▔".repeat(state.width));
        assert!(
            !state.rendered_screen[2..]
                .iter()
                .any(|line| line.contains("│"))
        );
        state.menu.as_mut().unwrap().options = vec!["First".into(), "Second".into()];
        state.draw(&mut Vec::new()).unwrap();
        assert_eq!(state.input_dialog_height(), 5);
        assert!(!state.rendered_screen[0].starts_with("❯ Height pinned message"));
        assert!(
            state.rendered_screen[..19]
                .iter()
                .any(|line| line.contains("Fixture Tick 0"))
        );
    }
}

#[test]
fn merged_monitor_column_and_overlay_stop_at_the_content_sized_dialog_rule() {
    for renderer in [Renderer::Inline, Renderer::Fullscreen] {
        for width in [80, 140] {
            for height in [6, 10, 24, 40] {
                let mut state = State::new(true, width, height);
                state.renderer = renderer;
                state.chat = Some(Uuid::new_v4());
                state
                    .monitor
                    .replace(TickGroup::Artifacts, input_dialog_tests::fixture_ticks());
                state.monitor.overlay = width == 80;
                state.record_input("Monitor height prompt");
                for index in 0..40 {
                    state.scrollback.append(&format!("History {index}\n"));
                }
                state.picker_kind = Some(PickerKind::Actions);
                state.menu_focused = true;
                state.picker_presentation.hint = "enter · esc back".into();
                for count in [2, 80, 2] {
                    state.menu = Some(picker_menu(count));
                    state.draw(&mut Vec::new()).unwrap();
                    let remainder = state.height - state.input_dialog_height();
                    assert!(remainder >= 2);
                    assert!(state.rendered_screen[0].contains("│ Artifacts (2)"));
                    assert_eq!(state.rendered_screen[remainder], "▔".repeat(state.width));
                    assert!(
                        !state.rendered_screen[remainder..]
                            .iter()
                            .any(|line| line.contains('│'))
                    );
                }
            }
        }
    }
}
