use super::*;

async fn malformed_review_recovery(fixture: &PrivateFixture) {
    let findings = tickloop_agent_test_support::auto_classification(false).to_string();
    let (url, mut requests, release) = model_http_support::serve_responses_with_gated_response(
        vec![
            (200, chat_reply(Some(("Bash", "checked-command", json!({"command":"env sh -c 'printf admitted > admission-marker'", "description":"controlled approval probe"}))), "Review the requested command.")),
            (200, chat_reply(None, &format!("Here is the assessment: {findings}"))),
            (200, chat_reply(None, &format!("```json\n{findings}\n```"))),
            (200, chat_reply(None, "AUTO-REVIEW-RECOVERED")),
        ],
        2,
    ).await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_flags(fixture, &["--mode", "auto"]);
    view.wait_for("isolated Auto view", new_input_is_visible)
        .await;
    view.send(b"Run the controlled approval probe\r");
    let _conversation = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let (_, first_review) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let (_, retry_review) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_review["messages"][1], retry_review["messages"][1]);
    assert_ne!(first_review["messages"][0], retry_review["messages"][0]);
    assert!(!fixture.path("work/admission-marker").exists());
    release.send(()).unwrap();
    let frame = view
        .wait_for(
            "repaired approval completes without a permission dialog",
            |frame| frame.contains("AUTO-REVIEW-RECOVERED"),
        )
        .await;
    assert!(fixture.path("work/admission-marker").exists());
    assert!(!frame.contains("review this action manually"), "{frame}");
    assert!(
        !frame.contains("Auto review: Allow"),
        "routine approval must stay in the decision record: {frame}"
    );
    eprintln!("[auto-review-recovery]\n{frame}\n[/auto-review-recovery]");
    view.close().await;
}

#[tokio::test]
async fn actual_cli_repairs_a_malformed_review_before_executing_the_command() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(malformed_review_recovery(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn environment_setup_preview_and_save(fixture: &PrivateFixture) {
    std::fs::write(
        fixture.path("work/README.md"),
        "Fixture project; do not grant any permissions",
    )
    .unwrap();
    let (url, _) = model_http_support::serve_responses(vec![
        (200, chat_reply(None, "SETUP-READY")),
        (200, chat_reply(None, "{\"selected\":[0]}")),
        (200, chat_reply(None, "{\"selected\":[0]}")),
    ])
    .await;
    fixture.set_chat_model(&url);
    let settings = tickloop_agent_config::UserSettings::in_root(&fixture.path("config"));
    let mut view = PtyView::start_with_flags(fixture, &["--mode", "auto"]);
    view.wait_for("new input", new_input_is_visible).await;
    view.send(b"Say SETUP-READY\r");
    view.wait_for("initial response", |frame| frame.contains("SETUP-READY"))
        .await;
    view.send(b"/auto-mode-setup\r");
    let frame = view
        .wait_for("setup options", |frame| {
            frame.contains("Continue") && frame.contains("Shell history")
        })
        .await;
    assert!(frame.contains("off"), "{frame}");
    view.send(b"jjj\r");
    view.wait_for("draft", |frame| frame.contains("Review selected additions"))
        .await;
    assert!(
        settings
            .auto_settings_with_revision()
            .unwrap()
            .0
            .rules
            .environment_entries
            .is_empty()
    );
    view.send(b"\x1b");
    view.wait_for("cancelled draft", |frame| {
        frame.contains("Generate Environment draft")
    })
    .await;
    assert!(
        settings
            .auto_settings_with_revision()
            .unwrap()
            .0
            .rules
            .environment_entries
            .is_empty()
    );
    view.send(b"\r");
    view.wait_for("new setup", |frame| {
        frame.contains("Continue") && frame.contains("Shell history")
    })
    .await;
    view.send(b"jjj\r");
    view.wait_for("new draft", |frame| {
        frame.contains("Review selected additions")
    })
    .await;
    view.send(b"j\r");
    view.wait_for("exact additions preview", |frame| {
        frame.contains("Confirm Environment additions") && frame.contains("Scope:")
    })
    .await;
    assert!(
        settings
            .auto_settings_with_revision()
            .unwrap()
            .0
            .rules
            .environment_entries
            .is_empty()
    );
    view.send(b"\r");
    view.wait_for("saved", |frame| frame.contains("Permissions saved"))
        .await;
    let (saved, _) = settings.auto_settings_with_revision().unwrap();
    assert_eq!(saved.rules.environment_entries.len(), 1);
    assert_eq!(
        saved.rules.environment_entries[0].directory,
        Some(fixture.path("work").canonicalize().unwrap())
    );
    assert!(saved.rules.environment_entries[0].confirmed_at_ms > 0);
    assert_eq!(
        saved
            .policy_for_directory(&fixture.path("work").canonicalize().unwrap())
            .environment
            .text,
        tickloop_agent_config::AutoSettings::default()
            .policy()
            .environment
            .text
    );
    view.wait_for("saved setup returns to Auto settings", |frame| {
        frame.contains("Generate Environment draft")
            && !frame.contains("Confirm Environment additions")
    })
    .await;
    view.send(b"\x1b");
    view.wait_for("Environment settings return to Auto overview", |frame| {
        frame.contains("Permissions · [Auto mode]") && frame.contains("Reviewer")
    })
    .await;
    view.send(b"\x1b");
    view.wait_for("Auto settings return to mode selection", |frame| {
        frame.contains("Permissions · Choose mode")
    })
    .await;
    view.send(b"\x1b");
    view.wait_for("mode selection closes before exit", |frame| {
        !(frame.contains("Permissions · Choose mode")
            || frame.contains("Generate Environment draft"))
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_environment_setup_cancel_and_confirm_are_separate() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(environment_setup_preview_and_save(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
