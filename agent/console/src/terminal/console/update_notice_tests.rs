use super::*;

fn state(width: usize, height: usize) -> State {
    let mut state = State::new(true, width.try_into().unwrap(), height.try_into().unwrap());
    state.prompt_enabled = true;
    state
        .status_area
        .update("Auto".into(), "fixture".into(), None, None);
    state
}

#[test]
fn update_notice_shares_the_hint_row_right_edge_without_adding_rows() {
    for width in [24, 40, 120] {
        let mut state = state(width, 24);
        let before = state.normal_styled_frame().0.len();
        state.update_notice = Some(UpdateNotice::Ready {
            version: "1.2.3".into(),
        });
        let lines = state.normal_styled_frame().0;
        assert_eq!(lines.len(), before);
        assert!(
            lines
                .iter()
                .all(|line| display_width(&line.text()) <= width)
        );
        let notice = lines
            .iter()
            .find(|line| line.text().contains("Update") || line.text().contains("Installed"))
            .unwrap_or_else(|| panic!("{width}: {lines:?}"));
        assert_eq!(display_width(&notice.text()), state.width);
        assert!(
            notice
                .spans
                .iter()
                .any(|span| span.role == Some(Role::Success))
        );
        if width == 120 {
            assert!(notice.text().starts_with("← Agents"), "{notice:?}");
            assert!(
                notice
                    .text()
                    .ends_with("Installed 1.2.3 · switching automatically"),
                "{notice:?}"
            );
        }
    }
}

#[test]
fn connection_errors_background_permissions_and_input_feedback_take_priority() {
    for priority in ["connection", "background", "feedback"] {
        let mut state = state(120, 24);
        state.update_notice = Some(UpdateNotice::Ready {
            version: "1.2.3".into(),
        });
        match priority {
            "connection" => state.connection_notice = Some("Disconnected · cached view".into()),
            "background" => {
                state.background_status_notice = Some(BackgroundStatusNotice {
                    title: Some("Review".into()),
                    ordinal: None,
                    message: "waiting for approval".into(),
                    message_role: Role::Permission,
                    compact_message: None,
                    necessary: true,
                })
            }
            _ => state.status = "Not saved: disk unavailable".into(),
        }
        let text = state
            .normal_styled_frame()
            .0
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("Installed"), "{priority}: {text}");
        let expected = match priority {
            "connection" => "Disconnected",
            "background" => "waiting for approval",
            _ => "Not saved",
        };
        assert!(text.contains(expected), "{priority}: {text}");
    }
}

#[test]
fn updates_remain_on_the_overview_hint_row_and_do_not_steal_input_focus() {
    let console = super::tests::console_with_state(state(120, 24));
    console
        .set_global_overview(Some(AgentOverview {
            rows: Vec::new(),
            dispatch_directory: PathBuf::from("/fixture/launch"),
            created: None,
            visible: None,
        }))
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.insert("keep typing");
    }
    console
        .set_update_notice(Some(UpdateNotice::Ready {
            version: "1.2.3".into(),
        }))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.editor.draft(), "keep typing");
    let lines = state.normal_styled_frame().0;
    assert!(
        lines
            .iter()
            .any(|line| line.text().contains("Installed 1.2.3")),
        "{lines:?}"
    );
    assert!(lines.iter().all(|line| display_width(&line.text()) <= 120));
}

#[test]
fn checking_installing_failure_and_a_cleared_notice_keep_the_same_layout() {
    let mut state = state(120, 24);
    let before = state.normal_styled_frame().0.len();
    for (notice, expected) in [
        (Some(UpdateNotice::Checking), "Checking for updates"),
        (
            Some(UpdateNotice::Installing {
                progress: Default::default(),
            }),
            "Updating Tickloop",
        ),
        (Some(UpdateNotice::Activating), "Applying update"),
        (
            Some(UpdateNotice::Deferred {
                reason: "waiting for the current turn".into(),
            }),
            "Update pending",
        ),
        (
            Some(UpdateNotice::Failed),
            "Update failed · tickloop update",
        ),
    ] {
        state.update_notice = notice;
        let lines = state.normal_styled_frame().0;
        assert_eq!(lines.len(), before);
        assert!(
            lines.iter().any(|line| line.text().contains(expected)),
            "{lines:?}"
        );
    }
    state.update_notice = None;
    assert_eq!(state.normal_styled_frame().0.len(), before);
}

#[test]
fn progress_keeps_hint_layout_and_input_across_download_and_install_stages() {
    use tickloop_agent_updater::{UpdateProgress, UpdateStage};
    for width in [24, 40, 80, 120] {
        let mut state = state(width, 24);
        state.insert("keep typing");
        let before = state.normal_styled_frame().0.len();
        for progress in [
            UpdateProgress {
                stage: UpdateStage::Downloading,
                percent: Some(58),
                downloaded_bytes: Some(58_000_000),
            },
            UpdateProgress {
                stage: UpdateStage::Downloading,
                downloaded_bytes: Some(1_048_576),
                ..Default::default()
            },
            UpdateProgress {
                stage: UpdateStage::Verifying,
                ..Default::default()
            },
            UpdateProgress::default(),
        ] {
            state.update_notice = Some(UpdateNotice::Installing { progress });
            let lines = state.normal_styled_frame().0;
            assert_eq!(lines.len(), before);
            assert!(
                lines
                    .iter()
                    .all(|line| display_width(&line.text()) <= width)
            );
            assert_eq!(state.editor.draft(), "keep typing");
            if width >= 40 {
                assert!(
                    lines
                        .iter()
                        .any(|line| line.text().contains('[') && line.text().contains('━')),
                    "{width}: {lines:?}"
                );
            }
        }
    }
}
