use super::*;

fn state() -> State {
    let mut state = State::new(true, 120, 30);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.accepts_images = true;
    state
}

fn image() -> Image {
    Image {
        media_type: tickloop_harness::protocol::ImageMediaType::Png,
        data: "aW1hZ2U=".into(),
        width: 1,
        height: 1,
    }
}

fn drain(state: &mut State) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        state.poll_prepared_images();
        if state.image_preparation.is_idle() {
            return;
        }
        assert!(Instant::now() < deadline, "image worker did not finish");
        thread::yield_now();
    }
}

#[test]
fn blocked_image_read_freezes_once_and_keeps_next_draft_independent() {
    let mut state = state();
    let (release, blocked) = std::sync::mpsc::channel();
    assert!(state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    }));
    assert_eq!(state.editor.draft(), "[Image #1]");
    assert!(state.paste_blocks.has_pending_images());
    assert!(state.paste_blocks.images(state.editor.draft()).is_empty());
    state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(state.editor.draft(), "[Image #1]x");
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert!(state.editor.draft().is_empty());
    assert_eq!(state.delayed_submissions.len(), 1);
    assert!(state.poll_delayed_submission().is_none());
    // Empty Enter cannot duplicate the accepted message.
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(state.delayed_submissions.len(), 1);
    state.insert("next draft");
    release.send(()).unwrap();
    drain(&mut state);
    assert_eq!(state.editor.draft(), "next draft");
    assert!(state.paste_blocks.images(state.editor.draft()).is_empty());
    let frozen = state.delayed_submissions.front().unwrap();
    assert_eq!(frozen.prompt.draft, "[Image #1]x");
    assert_eq!(
        frozen
            .prompt
            .paste_blocks
            .images(&frozen.prompt.draft)
            .len(),
        1
    );
    assert!(matches!(
        state.poll_delayed_submission(),
        Some(InputEvent::DelayedPrompt { .. })
    ));
    assert!(state.poll_delayed_submission().is_none());
    assert_eq!(state.editor.draft(), "next draft");
}

#[test]
fn frozen_image_submission_pauses_away_from_its_chat_and_resumes_on_return() {
    let mut state = state();
    let first = state.chat;
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    });
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.insert("other chat");
    release.send(()).unwrap();
    drain(&mut state);
    assert!(state.poll_delayed_submission().is_none());
    assert_eq!(state.editor.draft(), "other chat");
    state.switch_view_chat(first);
    assert!(state.editor.draft().is_empty());
    assert!(matches!(
        state.poll_delayed_submission(),
        Some(InputEvent::DelayedPrompt { .. })
    ));
    assert!(state.poll_delayed_submission().is_none());
}

#[test]
fn frozen_submission_waits_for_every_image_and_later_text_cannot_overtake() {
    let mut state = state();
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image(String::new(), None, || Ok(Some(image())));
    state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    });
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    state.insert("second submission");
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(state.delayed_submissions.len(), 2);
    assert!(state.poll_delayed_submission().is_none());
    release.send(()).unwrap();
    drain(&mut state);
    let first = state.delayed_submissions.front().unwrap();
    assert_eq!(
        first.prompt.paste_blocks.images(&first.prompt.draft).len(),
        2
    );
    let Some(InputEvent::DelayedPrompt { id: first_id }) = state.poll_delayed_submission() else {
        panic!("first submission not ready");
    };
    // While the first event is in flight, no later event may pass it.
    assert!(state.poll_delayed_submission().is_none());
    let chat = state.chat;
    let console = super::tests::console_with_state(state);
    let (first_event, guard) = console
        .begin_delayed_submission(first_id, chat)
        .unwrap()
        .unwrap();
    assert!(matches!(first_event, InputEvent::Prompt { images, .. } if images.len() == 2));
    guard.mark_accepted();
    drop(guard);
    let Some(InputEvent::DelayedPrompt { id: second_id }) =
        console.state.lock().unwrap().poll_delayed_submission()
    else {
        panic!("next FIFO entry not released after acceptance");
    };
    assert_ne!(first_id, second_id);
    let (second_event, guard) = console
        .begin_delayed_submission(second_id, chat)
        .unwrap()
        .unwrap();
    assert!(
        matches!(second_event, InputEvent::Prompt { text, images, .. } if text == "second submission" && images.is_empty())
    );
    guard.mark_accepted();
    drop(guard);
    assert!(console.state.lock().unwrap().delayed_submissions.is_empty());
}

#[test]
fn taking_back_a_frozen_pending_submission_revokes_auto_send() {
    let mut state = state();
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    });
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(state.take_back_delayed());
    assert_eq!(state.editor.draft(), "[Image #1]");
    assert!(state.delayed_submissions.is_empty());
    release.send(()).unwrap();
    drain(&mut state);
    assert!(state.poll_delayed_submission().is_none());
    assert_eq!(state.paste_blocks.images(state.editor.draft()).len(), 1);
}

#[test]
fn failed_frozen_image_never_sends_a_text_only_message() {
    let mut state = state();
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image("missing.png".into(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(None)
    });
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    state.insert("new draft");
    release.send(()).unwrap();
    drain(&mut state);
    assert!(state.poll_delayed_submission().is_none());
    assert_eq!(state.delayed_submissions.len(), 1);
    assert_eq!(state.editor.draft(), "new draft");
    assert!(state.delayed_submissions.front().unwrap().failed.is_some());
}

#[test]
fn delayed_ctrl_enter_retains_the_original_send_now_intent() {
    let mut state = state();
    state
        .scrollback
        .set_activity(Some(RoleLine::role("Working", Role::MutedText)));
    state.activity_interruptible = true;
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    });
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))
            .is_none()
    );
    assert!(state.delayed_submissions.front().unwrap().send_now);
    // Readiness must not infer send_now from the later activity state.
    state.activity_interruptible = false;
    state.scrollback.set_activity(None);
    release.send(()).unwrap();
    drain(&mut state);
    assert!(state.delayed_submissions.front().unwrap().send_now);
    assert!(matches!(
        state.poll_delayed_submission(),
        Some(InputEvent::DelayedPrompt { .. })
    ));
}

#[test]
fn separate_frozen_image_submissions_keep_distinct_marker_ids() {
    let mut state = state();
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    });
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(state.prepare_image(String::new(), None, || Ok(Some(image()))));
    assert_eq!(state.editor.draft(), "[Image #2]");
    state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    release.send(()).unwrap();
    drain(&mut state);
    assert_eq!(state.delayed_submissions.len(), 2);
    for (index, pending) in state.delayed_submissions.iter().enumerate() {
        assert_eq!(pending.prompt.draft, format!("[Image #{}]", index + 1));
        assert_eq!(
            pending
                .prompt
                .paste_blocks
                .images(&pending.prompt.draft)
                .len(),
            1
        );
    }
    assert!(state.editor.draft().is_empty());
}

#[test]
fn delayed_dispatch_uses_frozen_retry_and_contexts_not_the_new_draft() {
    let mut state = state();
    let chat = state.chat;
    let retry = Uuid::new_v4();
    let context = InputContext::File(tickloop_harness::protocol::TextContext {
        path: "frozen.txt".into(),
        content: "frozen context".into(),
        truncated: false,
    });
    state.insert("frozen");
    state.retry_of = Some(retry);
    state.restored_contexts = vec![context.clone()];
    assert!(state.freeze_delayed_submission(vec!["accepted.txt".into()], None));
    state.insert("next draft");
    let next_context = InputContext::File(tickloop_harness::protocol::TextContext {
        path: "next.txt".into(),
        content: "new draft context".into(),
        truncated: false,
    });
    state.restored_contexts = vec![next_context.clone()];
    let Some(InputEvent::DelayedPrompt { id }) = state.poll_delayed_submission() else {
        panic!("ready event missing");
    };
    let console = super::tests::console_with_state(state);
    let (event, guard) = console.begin_delayed_submission(id, chat).unwrap().unwrap();
    assert!(
        matches!(event, InputEvent::Prompt { text, file_references, .. } if text == "frozen" && file_references == vec![PathBuf::from("accepted.txt")])
    );
    assert_eq!(console.take_submitted_retry().unwrap(), Some(retry));
    assert_eq!(
        console.take_restored_contexts("frozen").unwrap(),
        vec![context]
    );
    guard.mark_accepted();
    drop(guard);
    let state = console.state.lock().unwrap();
    assert!(state.delayed_submissions.is_empty());
    assert_eq!(state.editor.draft(), "next draft");
    assert_eq!(state.restored_contexts, vec![next_context]);
}

#[test]
fn delayed_unknown_outcome_retains_snapshot_and_retry_without_overwriting_new_draft() {
    let mut state = state();
    let chat = state.chat;
    state.insert("frozen");
    assert!(state.freeze_delayed_submission(Vec::new(), None));
    state.insert("next draft");
    let Some(InputEvent::DelayedPrompt { id }) = state.poll_delayed_submission() else {
        panic!("ready event missing");
    };
    let console = super::tests::console_with_state(state);
    let (_, guard) = console.begin_delayed_submission(id, chat).unwrap().unwrap();
    let retry = Uuid::new_v4();
    console
        .restore_unknown_prompt(Vec::new(), "acceptance unknown", Some(retry))
        .unwrap();
    drop(guard);
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.editor.draft(), "next draft");
    assert_eq!(state.delayed_submissions.len(), 1);
    let frozen = state.delayed_submissions.front().unwrap();
    assert_eq!(frozen.prompt.draft, "frozen");
    assert_eq!(frozen.retry_of, Some(retry));
    assert!(frozen.failed.is_some());
    assert!(state.poll_delayed_submission().is_none());
}

#[test]
fn delayed_event_claim_revalidates_target_and_reparks_on_navigation() {
    let mut state = state();
    let chat = state.chat;
    state.insert("original chat only");
    assert!(state.freeze_delayed_submission(Vec::new(), None));
    let Some(InputEvent::DelayedPrompt { id }) = state.poll_delayed_submission() else {
        panic!("ready event missing");
    };
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.insert("other chat draft");
    let console = super::tests::console_with_state(state);
    assert!(
        console
            .begin_delayed_submission(id, chat)
            .unwrap()
            .is_none()
    );
    let mut state = console.state.lock().unwrap();
    assert_eq!(state.editor.draft(), "other chat draft");
    assert!(!state.delayed_submissions.front().unwrap().in_flight);
    state.switch_view_chat(chat);
    assert!(matches!(
        state.poll_delayed_submission(),
        Some(InputEvent::DelayedPrompt { .. })
    ));
}

#[test]
fn completion_updates_original_chat_not_the_new_chat() {
    let mut state = state();
    let first = state.chat;
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    });
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.insert("other chat");
    release.send(()).unwrap();
    drain(&mut state);
    assert_eq!(state.editor.draft(), "other chat");
    assert!(state.paste_blocks.images(state.editor.draft()).is_empty());
    state.switch_view_chat(first);
    assert_eq!(state.editor.draft(), "[Image #1]");
    assert_eq!(state.paste_blocks.images(state.editor.draft()).len(), 1);
}

#[test]
fn deleted_or_cleared_pending_image_is_not_resurrected() {
    for clear in [false, true] {
        let mut state = state();
        let (release, blocked) = std::sync::mpsc::channel();
        state.prepare_image(String::new(), None, move || {
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(Some(image()))
        });
        if clear {
            state.editor.apply(EditorCommand::Clear);
            state.paste_blocks.clear();
        } else {
            state.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        assert!(state.editor.draft().is_empty());
        state.insert("new draft");
        release.send(()).unwrap();
        drain(&mut state);
        assert_eq!(state.editor.draft(), "new draft");
        assert!(!state.paste_blocks.has_pending_images());
        assert!(state.paste_blocks.images(state.editor.draft()).is_empty());
    }
}

#[test]
fn failed_path_read_restores_path_without_moving_later_cursor_or_attachments() {
    let mut state = state();
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image("missing.png".into(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(None)
    });
    state.insert(" then ");
    state.prepare_image(String::new(), None, || Ok(Some(image())));
    state.insert(" tail");
    release.send(()).unwrap();
    drain(&mut state);
    assert_eq!(state.editor.draft(), "missing.png then [Image #2] tail");
    assert_eq!(state.editor.cursor(), state.editor.draft().len());
    assert_eq!(state.paste_blocks.images(state.editor.draft()).len(), 1);
}

#[test]
fn image_queue_is_bounded_and_rejects_extra_work_without_inserting_markers() {
    let mut state = state();
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    });
    for _ in 1..8 {
        assert!(state.prepare_image(String::new(), None, || Ok(Some(image()))));
    }
    let draft = state.editor.draft().to_owned();
    assert!(!state.prepare_image(String::new(), None, || panic!("rejected job ran")));
    assert_eq!(state.editor.draft(), draft);
    release.send(()).unwrap();
    drain(&mut state);
    assert_eq!(state.paste_blocks.images(state.editor.draft()).len(), 8);
}

#[test]
fn pending_image_cannot_be_stashed_and_lost_before_completion() {
    let mut state = state();
    let (release, blocked) = std::sync::mpsc::channel();
    state.prepare_image(String::new(), None, move || {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Some(image()))
    });
    state.stash_input();
    assert_eq!(state.editor.draft(), "[Image #1]");
    assert!(state.status.contains("wait before stashing"));
    release.send(()).unwrap();
    drain(&mut state);
    state.stash_input();
    assert!(state.editor.draft().is_empty());
    state.stash_input();
    assert_eq!(state.paste_blocks.images(state.editor.draft()).len(), 1);
}

#[test]
fn orphaned_pending_marker_is_cancelled_instead_of_blocking_send_forever() {
    let mut state = state();
    let id = state.paste_blocks.next_id();
    state.insert("[Image #1]");
    state
        .paste_blocks
        .register_pending_image(state.editor.draft(), 0..10, id)
        .unwrap();
    assert!(state.poll_prepared_images());
    assert!(state.editor.draft().is_empty());
    assert!(!state.paste_blocks.has_pending_images());
    assert!(state.status.contains("cancelled"));
}

/// Runs the real terminal input thread in an isolated PTY. The release signal is a file,
/// deliberately not a terminal key: readiness must wake dispatch by itself.
#[test]
#[ignore]
fn delayed_image_input_loop_pty_fixture() {
    let directory =
        PathBuf::from(std::env::var(super::input_dialog_pty::FIXTURE_DIRECTORY).unwrap());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (console, mut input) = Console::start(true).unwrap();
            console.select_chat(Some(Uuid::from_u128(7400))).unwrap();
            console.set_prompt_enabled(true).unwrap();
            let (release, blocked) = std::sync::mpsc::channel();
            {
                let mut state = console.state.lock().unwrap();
                state.accepts_images = true;
                state.insert("frozen message ");
                assert!(state.prepare_image(String::new(), None, move || {
                    blocked.recv_timeout(Duration::from_secs(20)).unwrap();
                    Ok(Some(image()))
                }));
            }
            std::fs::write(directory.join("phase"), "ready").unwrap();
            let mut release = Some(release);
            let mut poll = tokio::time::interval(Duration::from_millis(20));
            let deadline = tokio::time::sleep(Duration::from_secs(20));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    event = input.recv() => match event {
                        Some(InputEvent::DelayedPrompt { .. }) => {
                            {
                                let state = console.state.lock().unwrap();
                                assert_eq!(state.editor.draft(), "next draft");
                                let pending = state.delayed_submissions.front().unwrap();
                                assert_eq!(pending.prompt.draft, "frozen message [Image #1]");
                                assert_eq!(pending.prompt.paste_blocks.images(&pending.prompt.draft).len(), 1);
                            }
                            // The same completion must not emit again on subsequent idle polls.
                            assert!(tokio::time::timeout(Duration::from_millis(250), input.recv()).await.is_err());
                            std::fs::write(directory.join("phase"), "sent").unwrap();
                            break;
                        }
                        Some(event) => panic!("unexpected input event: {event:?}"),
                        None => panic!("input thread ended before image was ready"),
                    },
                    _ = poll.tick() => {
                        if std::fs::read_to_string(directory.join("trigger")).is_ok_and(|value| value == "release")
                            && let Some(release) = release.take()
                        {
                            release.send(()).unwrap();
                        }
                    }
                    _ = &mut deadline => panic!("terminal input thread did not auto-send"),
                }
            }
        });
}

#[test]
fn image_completion_delivers_input_event_without_another_terminal_key() {
    for (width, height) in [(80, 24), (120, 40)] {
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = super::input_dialog_pty::Fixture::start(
            directory.path(),
            width,
            height,
            "terminal::console::image_preparation_tests::delayed_image_input_loop_pty_fixture",
        );
        fixture.wait(|_| {
            std::fs::read_to_string(directory.path().join("phase"))
                .is_ok_and(|phase| phase == "ready")
        });
        fixture.send(b"\r");
        fixture.send(b"next draft");
        fixture.wait(|frame| frame.contains("next draft"));
        // No terminal input follows: the worker's completion alone must emit the event.
        std::fs::write(directory.path().join("trigger"), "release").unwrap();
        fixture.wait(|_| {
            std::fs::read_to_string(directory.path().join("phase"))
                .is_ok_and(|phase| phase == "sent")
        });
        assert!(
            fixture.child.wait().unwrap().success(),
            "{}",
            fixture.raw_output()
        );
    }
}
