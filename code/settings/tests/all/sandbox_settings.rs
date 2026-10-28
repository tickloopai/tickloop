use serde_json::json;
use tickloop_code_settings::UserSettings;

#[test]
fn folder_sandbox_overrides_scalars_and_merges_lists_without_reading_repository_settings() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("project");
    std::fs::create_dir(&folder).unwrap();
    let settings = UserSettings::in_root(temp.path());
    std::fs::write(
        settings.file_path(),
        json!({
            "sandbox": {"enabled": true, "failIfUnavailable": true,
                "excludedCommands": ["docker", "git:*"],
                "filesystem": {"allowWrite": ["cache"], "denyWrite": ["secret"]},
                "network": {"allowedDomains": ["registry.npmjs.org"]}},
            "projects": {folder.to_str().unwrap(): {"sandbox": {
                "enabled": false, "excludedCommands": ["docker", "podman"],
                "filesystem": {"allowWrite": ["cache", "build"], "denyWrite": ["private"]},
                "network": {"allowedDomains": ["registry.npmjs.org", "*.github.com"]}}}}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        folder.join("settings.json"),
        r#"{"sandbox":{"enabled":true}}"#,
    )
    .unwrap();
    let actual = settings.load().sandbox_for(&folder).unwrap();
    assert!(!actual.enabled);
    assert!(actual.fail_if_unavailable);
    assert_eq!(actual.excluded_commands, ["docker", "git:*", "podman"]);
    assert_eq!(actual.filesystem.allow_write, ["cache", "build"]);
    assert_eq!(actual.filesystem.deny_write, ["secret", "private"]);
    assert_eq!(
        actual.network.allowed_domains,
        ["registry.npmjs.org", "*.github.com"]
    );
    assert!(
        settings
            .load()
            .sandbox_for(&temp.path().join("other"))
            .unwrap()
            .enabled
    );
}

#[test]
fn sandbox_changes_preserve_other_settings_and_reject_stale_global_or_folder_values() {
    use tickloop_code_settings::{
        SettingsError,
        sandbox::{SandboxChange, SandboxMode},
    };
    let temp = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(temp.path());
    let folder = temp.path().join("project");
    std::fs::write(
        settings.file_path(),
        json!({"theme":"keep", "sandbox":{"enabled":false},
        "projects":{folder.to_str().unwrap():{"permissions":{"allow":["Read"]}}}})
        .to_string(),
    )
    .unwrap();
    let snapshot = settings.load().sandbox_snapshot(&folder).unwrap();
    settings
        .update_sandbox(
            &folder,
            &snapshot,
            SandboxChange::Mode(SandboxMode::Regular),
        )
        .unwrap();
    let loaded = settings.load().sandbox_for(&folder).unwrap();
    assert!(loaded.enabled);
    assert!(!loaded.auto_allow_bash_if_sandboxed);
    assert!(!settings.load().sandbox().unwrap().enabled);
    assert!(matches!(
        settings.update_sandbox(
            &folder,
            &snapshot,
            SandboxChange::Exclude("docker:*".into())
        ),
        Err(SettingsError::Conflict)
    ));
    let snapshot = settings.load().sandbox_snapshot(&folder).unwrap();
    settings
        .update_sandbox(
            &folder,
            &snapshot,
            SandboxChange::Exclude("docker:*".into()),
        )
        .unwrap();
    let snapshot = settings.load().sandbox_snapshot(&folder).unwrap();
    settings
        .update_sandbox(
            &folder,
            &snapshot,
            SandboxChange::Exclude("docker:*".into()),
        )
        .unwrap();
    assert_eq!(
        settings
            .load()
            .sandbox_for(&folder)
            .unwrap()
            .excluded_commands,
        ["docker:*"]
    );
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(settings.file_path()).unwrap()).unwrap();
    assert_eq!(saved["theme"], "keep");
    assert_eq!(
        saved["projects"][folder.to_str().unwrap()]["permissions"]["allow"],
        json!(["Read"])
    );
    saved["sandbox"]["enabled"] = json!(true);
    std::fs::write(settings.file_path(), saved.to_string()).unwrap();
    let bytes = std::fs::read(settings.file_path()).unwrap();
    assert!(matches!(
        settings.update_sandbox(
            &folder,
            &snapshot,
            SandboxChange::Mode(SandboxMode::Disabled)
        ),
        Err(SettingsError::Conflict)
    ));
    assert_eq!(std::fs::read(settings.file_path()).unwrap(), bytes);
}

#[test]
fn sandbox_reports_sources_deduplicates_one_scope_and_warns_once_for_unsupported_fields() {
    let temp = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(temp.path());
    let folder = temp.path().join("project");
    std::fs::write(settings.file_path(), json!({
        "sandbox":{"excludedCommands":["docker", "docker"], "filesystem":{"allowRead":["private"]}},
        "projects":{folder.to_str().unwrap():{"sandbox":{"enabled":true,"network":{"allowLocalBinding":true}}}}
    }).to_string()).unwrap();
    let mut loaded = settings.load();
    let snapshot = loaded.sandbox_snapshot(&folder).unwrap();
    assert_eq!(snapshot.settings.excluded_commands, ["docker"]);
    assert_eq!(snapshot.source("/enabled", None), "this Folder");
    assert_eq!(
        snapshot.source("/excludedCommands", Some("docker")),
        "global"
    );
    assert_eq!(snapshot.source("/failIfUnavailable", None), "default");
    loaded.sandbox_for(&folder).unwrap();
    assert_eq!(
        loaded
            .warnings()
            .iter()
            .filter(|warning| warning.contains("allowRead"))
            .count(),
        1
    );
    assert_eq!(
        loaded
            .warnings()
            .iter()
            .filter(|warning| warning.contains("allowLocalBinding"))
            .count(),
        1
    );
}

#[test]
fn malformed_folder_sandbox_cannot_be_ignored_or_overwritten() {
    use tickloop_code_settings::sandbox::{SandboxChange, SandboxMode};
    let temp = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(temp.path());
    let folder = temp.path().join("project");
    let snapshot = settings.load().sandbox_snapshot(&folder).unwrap();
    for value in [
        json!(null),
        json!({"enabled":"yes"}),
        json!({"excludedCommands":"docker"}),
        json!({"filesystem":{"denyWrite":false}}),
    ] {
        let contents = json!({"projects":{folder.to_str().unwrap():{"sandbox":value}}}).to_string();
        std::fs::write(settings.file_path(), &contents).unwrap();
        assert!(settings.load().sandbox_for(&folder).is_err());
        assert!(
            settings
                .update_sandbox(
                    &folder,
                    &snapshot,
                    SandboxChange::Mode(SandboxMode::Disabled)
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(settings.file_path()).unwrap(),
            contents
        );
    }
}
