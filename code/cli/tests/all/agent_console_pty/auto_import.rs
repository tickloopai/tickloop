use super::*;
use tickloop_harness::protocol::{AutoRuleChange, AutoRuleKind};

async fn import_preview_keys_and_conflict(fixture: &PrivateFixture) {
    let source_directory = fixture.path("home/claude-custom");
    std::fs::create_dir(&source_directory).unwrap();
    let source = source_directory.join("settings.json");
    let original = r#"{"autoMode":{"allow":["Allow fixture cleanup","Security Discussion: exported default"],"deny":["Keep release tags"],"environment":["Private research workstation"]}}"#;
    std::fs::write(&source, original).unwrap();
    let (url, _) =
        model_http_support::serve_responses(vec![(200, chat_reply(None, "IMPORT-READY"))]).await;
    fixture.set_chat_model(&url);
    let settings = tickloop_code_settings::UserSettings::in_root(&fixture.path("config"));
    let mut view = PtyView::start_with_environment(
        fixture,
        &["--mode", "auto"],
        &[("CLAUDE_CONFIG_DIR", source_directory.as_path())],
    );
    view.wait_for("new input", new_input_is_visible).await;
    view.send(b"Say IMPORT-READY\r");
    view.wait_for("initial response", |frame| frame.contains("⏺ IMPORT-READY"))
        .await;
    view.send(b"/permissions\r");
    view.wait_for("mode picker", |frame| {
        frame.contains("Permissions · Choose mode")
    })
    .await;
    view.send(b"\r");
    view.wait_for("import action", |frame| {
        frame.contains("Import from Claude Code")
    })
    .await;
    view.send(b"jjjjjj\r");
    let preview = view
        .wait_for("import preview", |frame| {
            frame.contains("Will add") && frame.contains("Will skip")
        })
        .await;
    assert!(preview.contains("Will relax review"), "{preview}");
    assert!(
        preview.contains("space toggle") && preview.contains("esc cancel"),
        "{preview}"
    );
    view.send(b" ");
    view.wait_for("unchecked allow", |frame| {
        frame
            .lines()
            .any(|line| line.contains("❯ [ ] Allow fixture cleanup"))
    })
    .await;
    view.send(b" j ");
    view.wait_for("manually selected builtin", |frame| {
        frame
            .lines()
            .any(|line| line.contains("❯ [x] Security Discussion"))
    })
    .await;
    view.send(b"k");
    view.wait_for("k navigates", |frame| {
        frame.contains("❯ [x] Allow fixture cleanup")
    })
    .await;
    view.send(b"\x0e");
    view.wait_for("ctrl+n navigates", |frame| {
        frame.contains("❯ [x] Security Discussion")
    })
    .await;
    view.send(b"\x10");
    view.wait_for("ctrl+p navigates", |frame| {
        frame.contains("❯ [x] Allow fixture cleanup")
    })
    .await;
    view.send(b"\x1b");
    view.wait_for("cancel returns to overview", |frame| {
        frame.contains("Reviewer") && !frame.contains("Will add")
    })
    .await;
    assert!(
        settings
            .auto_settings_with_revision()
            .unwrap()
            .0
            .rules
            .soft_allow
            .custom
            .is_empty()
    );
    view.send(b"\r");
    view.wait_for("reopened preview", |frame| {
        frame.contains("Will add") && frame.contains("Will skip")
    })
    .await;
    let (mut other, revision) = settings.auto_settings_with_revision().unwrap();
    other
        .rules
        .apply(AutoRuleChange::Add {
            kind: AutoRuleKind::SoftAllow,
            text: "Allow fixture cleanup".into(),
        })
        .unwrap();
    settings
        .update_auto_rules_cas(&revision, &other.rules)
        .unwrap();
    view.send(b"\r");
    let refreshed = view
        .wait_for("conflict refresh", |frame| {
            frame.contains("Permissions changed elsewhere")
                && frame.contains("Duplicate custom text")
        })
        .await;
    assert!(
        refreshed.contains("[ ] Allow fixture cleanup"),
        "{refreshed}"
    );
    assert!(
        settings
            .auto_settings_with_revision()
            .unwrap()
            .0
            .rules
            .soft_deny
            .custom
            .is_empty()
    );
    view.send(b"\r");
    view.wait_for("saved import", |frame| frame.contains("Permissions saved"))
        .await;
    let (saved, _) = settings.auto_settings_with_revision().unwrap();
    assert_eq!(saved.rules.soft_allow.custom.len(), 1);
    assert_eq!(saved.rules.soft_deny.custom[0].text, "Keep release tags");
    assert_eq!(
        saved.rules.environment_entries[0].text,
        "Private research workstation"
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
    view.send(b"\x1b");
    view.wait_for("back to mode picker", |frame| {
        frame.contains("Permissions · Choose mode")
    })
    .await;
    view.send(b"\x1b");
    view.wait_for("permissions closed", |frame| {
        !frame.contains("Permissions ·")
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_import_preview_toggles_cancels_and_refreshes_before_saving() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(import_preview_keys_and_conflict(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn import_errors_leave_settings_unchanged(fixture: &PrivateFixture) {
    let mut view = PtyView::start_with_flags(fixture, &["--mode", "auto"]);
    view.wait_for("new input", new_input_is_visible).await;
    view.send(b"/permissions\r");
    view.wait_for("new Agent mode picker", |frame| {
        frame.contains("Permissions · Choose mode")
    })
    .await;
    view.send(b"\r");
    view.wait_for("new Agent import action", |frame| {
        frame.contains("Import from Claude Code")
    })
    .await;
    let settings = tickloop_code_settings::UserSettings::in_root(&fixture.path("config"));
    let before = settings.auto_settings_with_revision().unwrap();
    view.send(b"jjjjjj\r");
    view.wait_for("missing user settings error", |frame| {
        frame.contains("Cannot read") && frame.contains(".claude/settings.json")
    })
    .await;
    let directory = fixture.path("home/.claude");
    std::fs::create_dir(&directory).unwrap();
    let source = directory.join("settings.json");
    for (invalid, expected) in [
        ("invalid", "Invalid JSON"),
        ("{}", "No autoMode"),
        (
            r#"{"autoMode":{"allow":42}}"#,
            "autoMode.allow must be an array of strings",
        ),
    ] {
        std::fs::write(&source, invalid).unwrap();
        view.send(b"\r");
        view.wait_for(expected, |frame| frame.contains(expected))
            .await;
        assert_eq!(
            settings.auto_settings_with_revision().unwrap().0.rules,
            before.0.rules
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), invalid);
    }
    std::fs::write(&source, r#"{"autoMode":{"allow":["New draft exception"]}}"#).unwrap();
    view.send(b"\r");
    view.wait_for("valid preview after errors", |frame| {
        frame.contains("[x] New draft exception")
    })
    .await;
    view.send(b"\x1b");
    view.wait_for("new Agent cancel", |frame| {
        frame.contains("Reviewer") && frame.contains("Import cancelled")
    })
    .await;
    assert_eq!(
        settings.auto_settings_with_revision().unwrap().0.rules,
        before.0.rules
    );
    view.send(b"\x1b");
    view.wait_for("back to new Agent mode picker", |frame| {
        frame.contains("Permissions · Choose mode")
    })
    .await;
    view.send(b"\x1b");
    view.wait_for("new Agent permissions closed", |frame| {
        !frame.contains("Permissions ·")
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_import_errors_preserve_user_settings_and_new_agent_draft_cancels() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(import_errors_leave_settings_unchanged(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
