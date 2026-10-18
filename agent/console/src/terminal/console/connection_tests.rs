use super::*;

fn console_frame(console: &Console) -> String {
    let mut state = console.state.lock().unwrap();
    state.draw(&mut Vec::new()).unwrap();
    state.rendered_screen.join("\n")
}

#[test]
fn execution_connection_facts_fit_custom_status_help_and_short_screens() {
    for width in [24, 180] {
        for height in [4, 12, 24] {
            for configured in [
                None,
                Some(vec![]),
                Some(vec!["custom".into()]),
                Some(vec!["first".into(), "second".into(), "third".into()]),
            ] {
                for help_open in [false, true] {
                    for connection in [
                        ExecutionConnectionNotice::Lost,
                        ExecutionConnectionNotice::LostHistory,
                        ExecutionConnectionNotice::CachedExecution,
                        ExecutionConnectionNotice::CachedView,
                    ] {
                        let mut state = State::new(true, width, height);
                        state.prompt_enabled = true;
                        state.configured_status_lines = configured.clone();
                        state.help_open = help_open;
                        state
                            .status_area
                            .update("Auto".into(), "loss-model".into(), None, None);
                        state.execution_connection_notice = Some(connection);
                        let (lines, _, _) = state.normal_styled_frame();
                        let text = lines
                            .iter()
                            .map(RoleLine::text)
                            .collect::<Vec<_>>()
                            .join("\n");
                        let fact = match connection {
                            ExecutionConnectionNotice::Lost => "Lost · outcome unknown",
                            ExecutionConnectionNotice::LostHistory => "Lost · cached history",
                            ExecutionConnectionNotice::CachedExecution if width == 24 => {
                                "Offline · cached"
                            }
                            ExecutionConnectionNotice::CachedExecution => {
                                "Disconnected · cached execution"
                            }
                            ExecutionConnectionNotice::CachedView if width == 24 => {
                                "Offline · cached view"
                            }
                            ExecutionConnectionNotice::CachedView => "Disconnected · cached view",
                        };
                        assert!(
                            text.contains(fact),
                            "{width}x{height}, {configured:?}, help={help_open}: {text}"
                        );
                        assert!(
                            lines.len() <= agent_layout::bottom_capacity(usize::from(height)),
                            "{lines:?}"
                        );
                        assert!(
                            lines
                                .iter()
                                .all(|line| display_width(&line.text()) <= state.width),
                            "{lines:?}"
                        );
                        if width == 180 && height == 24 && configured.is_none() && !help_open {
                            assert!(
                                text.contains("⏵⏵ Auto")
                                    && text.contains("loss-model")
                                    && text.contains("Context unknown"),
                                "{text}"
                            );
                        }
                        let fact_line = lines
                            .iter()
                            .find(|line| line.text().contains(fact))
                            .unwrap();
                        assert!(
                            fact_line
                                .spans
                                .iter()
                                .filter(|span| span.text.contains("Lost")
                                    || span.text.contains("cached"))
                                .all(|span| span.role == Some(Role::Warning)),
                            "{fact_line:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn connection_recovery_removes_the_short_notice_and_restores_core() {
    let mut state = State::new(true, 180, 24);
    state.prompt_enabled = true;
    state
        .status_area
        .update("Auto".into(), "loss-model".into(), None, None);
    let console = super::tests::console_with_state(state);
    console
        .set_execution_connection_notice(
            Some(ExecutionConnectionNotice::CachedExecution),
            Some("private diagnostic".into()),
        )
        .unwrap();
    assert!(console_frame(&console).contains("Disconnected · cached execution"));
    console.set_execution_connection_notice(None, None).unwrap();
    let frame = console_frame(&console);
    assert!(
        !frame.contains("Disconnected") && !frame.contains("private diagnostic"),
        "{frame}"
    );
    assert!(
        frame.contains("⏵⏵ Auto")
            && frame.contains("loss-model")
            && frame.contains("Context unknown"),
        "{frame}"
    );
}

#[test]
fn lost_diagnostics_follow_the_chat_across_cache_reset_and_details() {
    let chat = Uuid::new_v4();
    let other = Uuid::new_v4();
    let run = Uuid::new_v4();
    let mut state = State::new(true, 180, 30);
    state.prompt_enabled = true;
    state.switch_view_chat(Some(chat));
    let console = super::tests::console_with_state(state);
    let notice = ExecutionLossNotice {
        run_id: run,
        summary: "\nAgent Investigate · Execution connection was lost · outcome unknown; nothing was resent. Use /resume to open saved history.\n".into(),
        technical: format!("Run {run} has an unknown outcome. /resume {chat} loads its history"),
    };
    console
        .set_execution_connection_notice(
            Some(ExecutionConnectionNotice::Lost),
            Some("invalid request: execution worker lost".into()),
        )
        .unwrap();
    console.record_execution_loss(chat, notice.clone()).unwrap();
    console.record_execution_loss(chat, notice).unwrap();
    assert_eq!(
        console_frame(&console)
            .matches("Execution connection was lost")
            .count(),
        1
    );
    console.select_chat(Some(other)).unwrap();
    assert!(!console_frame(&console).contains("Execution connection was lost"));
    console.select_chat(Some(chat)).unwrap();
    assert_eq!(
        console_frame(&console)
            .matches("Execution connection was lost")
            .count(),
        1
    );
    let snapshot = serde_json::from_value(
        serde_json::json!({"id":chat,"folder":"/owned","messages":[],"loops":[]}),
    )
    .unwrap();
    for _ in 0..2 {
        console.reset_chat(chat).unwrap();
        crate::terminal::workflows::write_public_history(&console, &snapshot).unwrap();
        assert_eq!(
            console_frame(&console)
                .matches("Execution connection was lost")
                .count(),
            1
        );
    }
    let id = console.start_transcript().unwrap();
    console
        .update_transcript(id, vec!["Loaded conversation".into()].into())
        .unwrap();
    let details = console_frame(&console);
    assert!(
        details.contains(&run.to_string())
            && details.contains(&format!("/resume {chat}"))
            && details.contains("invalid request: execution worker lost"),
        "{details}"
    );
    console.close_transcript().unwrap();
    console.set_execution_connection_notice(None, None).unwrap();
    let refreshed = console_frame(&console);
    assert!(
        !refreshed.contains("Lost · outcome unknown"),
        "new live execution clears the current fact: {refreshed}"
    );
    assert!(
        refreshed.contains("Execution connection was lost"),
        "past attempt stays in history: {refreshed}"
    );
    console.select_chat(Some(other)).unwrap();
    let id = console.start_transcript().unwrap();
    console
        .update_transcript(id, vec!["Other archive".into()].into())
        .unwrap();
    assert!(!console_frame(&console).contains(&run.to_string()));
}

#[test]
fn a_loss_first_seen_while_hidden_is_replayed_once_when_its_chat_opens() {
    let chat = Uuid::new_v4();
    let other = Uuid::new_v4();
    let mut state = State::new(true, 180, 30);
    state.prompt_enabled = true;
    state.switch_view_chat(Some(other));
    let console = super::tests::console_with_state(state);
    for _ in 0..2 {
        let run = Uuid::new_v4();
        console.record_execution_loss(chat, ExecutionLossNotice {
            run_id: run,
            summary:"\nAgent Hidden · Execution connection was lost · outcome unknown for that attempt.\n".into(),
            technical:format!("Run {run}; /resume {chat} loads history"),
        }).unwrap();
    }
    assert!(!console_frame(&console).contains("Execution connection was lost"));
    let snapshot = serde_json::from_value(
        serde_json::json!({"id":chat,"folder":"/owned","messages":[],"loops":[]}),
    )
    .unwrap();
    console.select_chat(Some(chat)).unwrap();
    crate::terminal::workflows::write_public_history(&console, &snapshot).unwrap();
    assert_eq!(
        console_frame(&console)
            .matches("Execution connection was lost")
            .count(),
        2,
        "two different runs with the same text both remain visible"
    );
    crate::terminal::workflows::write_public_history(&console, &snapshot).unwrap();
    assert_eq!(
        console_frame(&console)
            .matches("Execution connection was lost")
            .count(),
        2
    );
    console.select_chat(Some(other)).unwrap();
    console.select_chat(Some(chat)).unwrap();
    assert_eq!(
        console_frame(&console)
            .matches("Execution connection was lost")
            .count(),
        2
    );
}

#[test]
fn a_creation_prefix_history_and_loss_rebuild_in_order_once_and_stay_with_the_chat() {
    let chat = Uuid::new_v4();
    let other = Uuid::new_v4();
    let mut state = State::new(true, 180, 40);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.write("Tickloop · owned startup\n").unwrap();
    console.record_input("/model").unwrap();
    console
        .write("Model selection: Applied · owned model\n")
        .unwrap();
    console
        .select_created_chat(chat, &[("fallback startup\n".into(), None)])
        .unwrap();
    console.record_execution_loss(chat,ExecutionLossNotice {
        run_id:Uuid::new_v4(),
        summary:"\nAgent owned · Execution connection was lost · outcome unknown for that attempt.\n".into(),
        technical:format!("/resume {chat} loads saved history"),
    }).unwrap();
    let snapshot = serde_json::from_value(serde_json::json!({
        "id":chat,"folder":"/owned","loops":[],
        "messages":[
            {"role":"user","content":[{"type":"text","text":"History user"}]},
            {"role":"assistant","content":[{"type":"text","text":"History answer"}]},
        ],
    }))
    .unwrap();
    for _ in 0..2 {
        console.select_chat(Some(other)).unwrap();
        assert!(!console_frame(&console).contains("Model selection: Applied"));
        assert!(!console_frame(&console).contains("Execution connection was lost"));
        console.reset_chat(chat).unwrap();
        crate::terminal::workflows::write_public_history(&console, &snapshot).unwrap();
        let rendered = console_frame(&console);
        assert!(!rendered.lines().next().unwrap().contains("History user"));
        let frame = rendered;
        let mut previous = None;
        for part in [
            "Tickloop · owned startup",
            "❯ /model",
            "Model selection: Applied · owned model",
            "History user",
            "History answer",
            "Execution connection was lost",
        ] {
            assert_eq!(frame.matches(part).count(), 1, "{frame}");
            let index = frame.find(part).unwrap();
            assert!(previous.is_none_or(|previous| previous < index), "{frame}");
            previous = Some(index);
        }
        assert!(!frame.contains("fallback startup"), "{frame}");
    }
}

#[test]
fn a_loss_received_in_overview_returns_to_its_source_chat_without_a_history_reset() {
    let chat = Uuid::new_v4();
    let other = Uuid::new_v4();
    let mut state = State::new(true, 180, 40);
    state.prompt_enabled = true;
    state.switch_view_chat(Some(chat));
    let console = super::tests::console_with_state(state);
    console.write("A recorded reply\n").unwrap();
    console.select_chat(Some(other)).unwrap();
    console.write("B recorded reply\n").unwrap();
    console.select_chat(Some(chat)).unwrap();
    let projection = AgentOverview {
        rows: vec![],
        dispatch_directory: PathBuf::from("/owned/dispatch"),
        created: None,
        visible: None,
    };
    let notice = ExecutionLossNotice {
        run_id: Uuid::new_v4(),
        summary: "\nAgent A · Execution connection was lost · outcome unknown for that attempt.\n"
            .into(),
        technical: format!("/resume {chat} loads saved history"),
    };
    let other_notice = ExecutionLossNotice {
        run_id: Uuid::new_v4(),
        summary: "\nAgent B · Execution connection was lost · outcome unknown for that attempt.\n"
            .into(),
        technical: format!("/resume {other} loads saved history"),
    };
    console
        .set_global_overview(Some(projection.clone()))
        .unwrap();
    {
        let state = console.state.lock().unwrap();
        assert_eq!(state.chat, Some(chat));
        assert!(state.overview_source_view.is_some());
    }
    for _ in 0..3 {
        console.record_execution_loss(chat, notice.clone()).unwrap();
        console
            .record_execution_loss(other, other_notice.clone())
            .unwrap();
        console
            .set_global_overview(Some(projection.clone()))
            .unwrap();
    }
    let dispatch_text = console
        .state
        .lock()
        .unwrap()
        .scrollback
        .frame(180, 40)
        .lines
        .join("\n");
    assert!(!console_frame(&console).contains("Execution connection was lost"));
    console.set_global_overview(None).unwrap();
    let returned = console_frame(&console);
    println!("OVERVIEW_LOSS_RETURN_BEGIN\n{returned}\nOVERVIEW_LOSS_RETURN_END");
    assert_eq!(
        returned.matches("A recorded reply").count(),
        1,
        "the source conversation was restored without rebuilding history"
    );
    assert_eq!(
        returned.matches("Execution connection was lost").count(),
        1,
        "the loss belongs to the source Chat, not the discarded dispatch presentation: {returned}"
    );
    assert!(!dispatch_text.contains("Execution connection was lost"));
    assert!(!returned.contains("Agent B"));
    console.select_chat(Some(other)).unwrap();
    let other_frame = console_frame(&console);
    assert_eq!(other_frame.matches("B recorded reply").count(), 1);
    assert_eq!(other_frame.matches("Agent B").count(), 1);
    assert!(!other_frame.contains("Agent A"));
    console.select_chat(Some(chat)).unwrap();
    assert_eq!(
        console_frame(&console)
            .matches("Execution connection was lost")
            .count(),
        1
    );
    console
        .set_global_overview(Some(projection.clone()))
        .unwrap();
    console.record_execution_loss(chat, notice).unwrap();
    console.record_execution_loss(other, other_notice).unwrap();
    console.set_global_overview(Some(projection)).unwrap();
    console.set_global_overview(None).unwrap();
    assert_eq!(
        console_frame(&console)
            .matches("Execution connection was lost")
            .count(),
        1
    );
    console.select_chat(Some(other)).unwrap();
    let other_frame = console_frame(&console);
    assert_eq!(other_frame.matches("Agent B").count(), 1);
    assert!(!other_frame.contains("Agent A"));
}
