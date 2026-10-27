use serde_json::json;
use tickloop_code_settings::claude_auto_import::{
    ImportKind, prepare_import, preview_auto_mode, read_user_auto_mode,
};
use tickloop_harness::protocol::{AutoRuleChange, AutoRuleKind, AutoRuleSettings};

#[test]
fn import_maps_user_auto_mode_and_preselects_only_new_custom_rules() {
    let mut current = AutoRuleSettings::default();
    current
        .apply(AutoRuleChange::Add {
            kind: AutoRuleKind::SoftDeny,
            text: "Keep release tags".into(),
        })
        .unwrap();
    let preview = preview_auto_mode(
        &json!({
            "allow": ["Allow fixture cleanup", "Security Discussion: exported default"],
            "soft_deny": ["Keep release tags"],
            "deny": ["Never publish reports"],
            "environment": ["Private research workstation"]
        }),
        &current,
    )
    .unwrap();
    assert_eq!(
        preview
            .iter()
            .map(|row| (row.kind, row.text.as_str(), row.selected))
            .collect::<Vec<_>>(),
        vec![
            (ImportKind::SoftAllow, "Allow fixture cleanup", true),
            (
                ImportKind::SoftAllow,
                "Security Discussion: exported default",
                false
            ),
            (ImportKind::SoftDeny, "Keep release tags", false),
            (ImportKind::SoftDeny, "Never publish reports", true),
            (
                ImportKind::Environment,
                "Private research workstation",
                true
            ),
        ]
    );
    assert!(
        preview[1]
            .skip_reason
            .as_ref()
            .unwrap()
            .contains("built-in")
    );
    assert!(
        preview[2]
            .skip_reason
            .as_ref()
            .unwrap()
            .contains("Duplicate")
    );
    assert!(preview[0].skip_reason.is_none());
}

#[test]
fn import_reads_only_the_selected_user_settings_and_rejects_missing_or_malformed_auto_mode() {
    let home = tempfile::tempdir().unwrap();
    let configured = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".claude")).unwrap();
    let default_file = home.path().join(".claude/settings.json");
    let configured_file = configured.path().join("settings.json");
    std::fs::write(
        &default_file,
        r#"{"autoMode":{"allow":["Default user rule"]}}"#,
    )
    .unwrap();
    let original = r#"{"autoMode":{"deny":["Configured user rule"]},"permissions":{"allow":["*"]},"projects":{"/repo":{"autoMode":{"allow":["Never import project rule"]}}}}"#;
    std::fs::write(&configured_file, original).unwrap();
    let (path, auto_mode) =
        read_user_auto_mode(Some(home.path()), Some(configured.path())).unwrap();
    assert_eq!(path, configured_file);
    let rows = preview_auto_mode(&auto_mode, &AutoRuleSettings::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "Configured user rule");
    assert_eq!(std::fs::read_to_string(&configured_file).unwrap(), original);
    assert_eq!(
        read_user_auto_mode(Some(home.path()), None).unwrap().0,
        default_file
    );
    for invalid in [
        "not json",
        "{}",
        r#"{"autoMode":null}"#,
        r#"{"autoMode":{"allow":[7]}}"#,
    ] {
        std::fs::write(&configured_file, invalid).unwrap();
        assert!(
            read_user_auto_mode(Some(home.path()), Some(configured.path())).is_err(),
            "{invalid}"
        );
        assert_eq!(std::fs::read_to_string(&configured_file).unwrap(), invalid);
    }
    std::fs::remove_file(&configured_file).unwrap();
    assert!(read_user_auto_mode(Some(home.path()), Some(configured.path())).is_err());
    assert!(read_user_auto_mode(None, None).is_err());
}

#[test]
fn import_preview_skips_empty_oversized_and_excess_entries_without_losing_them() {
    let mut allow = vec![" ".to_owned(), "界".repeat(1366)];
    allow.extend((0..257).map(|index| format!("Custom permission {index}")));
    let preview =
        preview_auto_mode(&json!({"allow": allow}), &AutoRuleSettings::default()).unwrap();
    assert_eq!(preview.len(), 259);
    assert_eq!(preview.iter().filter(|row| row.selected).count(), 256);
    assert!(preview[0].skip_reason.as_ref().unwrap().contains("Empty"));
    assert!(preview[1].skip_reason.as_ref().unwrap().contains("4096"));
    assert!(preview[258].skip_reason.as_ref().unwrap().contains("256"));
    let duplicate = preview_auto_mode(
        &json!({"deny": ["Keep tags", "Keep tags"]}),
        &AutoRuleSettings::default(),
    )
    .unwrap();
    assert!(duplicate[0].selected);
    assert!(!duplicate[1].selected);
}

#[test]
fn manual_selection_can_restore_duplicates_and_swap_overflow_but_cannot_save_invalid_text() {
    let current = AutoRuleSettings::default();
    let source = std::path::Path::new("/user/claude/settings.json");
    let mut duplicate = preview_auto_mode(
        &json!({"allow": ["Security Discussion: custom exception"]}),
        &current,
    )
    .unwrap();
    duplicate[0].selected = true;
    let mut saved = current.clone();
    saved
        .apply(prepare_import(&duplicate, source, &current).unwrap())
        .unwrap();
    assert!(saved.soft_allow.include_builtins);
    assert_eq!(
        saved.soft_allow.custom[0].text,
        "Security Discussion: custom exception"
    );
    let second_preview = preview_auto_mode(
        &json!({"allow": ["Security Discussion: custom exception"]}),
        &saved,
    )
    .unwrap();
    assert!(!second_preview[0].selected);
    let texts: Vec<_> = (0..257)
        .map(|index| format!("Custom exception {index}"))
        .collect();
    let mut overflow = preview_auto_mode(&json!({"allow":texts}), &current).unwrap();
    overflow[256].selected = true;
    assert!(prepare_import(&overflow, source, &current).is_err());
    overflow[0].selected = false;
    assert!(prepare_import(&overflow, source, &current).is_ok());
    for text in [" ".to_owned(), "a".repeat(4097)] {
        let mut invalid = preview_auto_mode(&json!({"environment":[text]}), &current).unwrap();
        invalid[0].selected = true;
        assert!(prepare_import(&invalid, source, &current).is_err());
    }
    let mut legacy = current.clone();
    legacy
        .apply(AutoRuleChange::ReplaceEnvironment {
            text: "Existing private environment".into(),
        })
        .unwrap();
    let mut preview =
        preview_auto_mode(&json!({"environment":["Imported environment"]}), &legacy).unwrap();
    assert!(!preview[0].selected);
    preview[0].selected = true;
    assert!(prepare_import(&preview, source, &legacy).is_err());
}

#[test]
fn import_resolves_an_explicit_relative_user_config_directory_without_searching() {
    let launch_directory = std::env::current_dir().unwrap();
    let configured = tempfile::tempdir_in(&launch_directory).unwrap();
    let relative_directory = configured.path().strip_prefix(&launch_directory).unwrap();
    let source = configured.path().join("settings.json");
    std::fs::write(
        &source,
        r#"{"autoMode":{"allow":["Explicit relative user rule"]}}"#,
    )
    .unwrap();
    let (read_path, auto_mode) = read_user_auto_mode(None, Some(relative_directory)).unwrap();
    assert_eq!(read_path, source);
    assert_eq!(auto_mode["allow"], json!(["Explicit relative user rule"]));
}
