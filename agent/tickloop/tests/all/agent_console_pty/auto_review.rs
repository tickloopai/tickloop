use super::*;

async fn malformed_review_recovery(fixture: &PrivateFixture) {
    let findings = tickloop_agent_test_support::auto_findings(false).to_string();
    let (url, mut requests, release) = model_http_support::serve_responses_with_gated_response(
        vec![
            (200, chat_reply(Some(("Bash", "checked-command", json!({"command":"printf admitted > admission-marker", "description":"controlled approval probe"}))), "Review the requested command.")),
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
