use tickloop_code_settings::UserSettings;
use tickloop_harness::policy::settings::SettingsStore;

fn settings_with(contents: &str) -> (tempfile::TempDir, UserSettings) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("settings.json"), contents).unwrap();
    let settings = UserSettings::in_root(directory.path());
    (directory, settings)
}

#[test]
fn plan_uses_auto_unless_settings_turn_it_off() {
    for (contents, expected) in [
        ("{}", true),
        (r#"{"useAutoModeDuringPlan":true}"#, true),
        (r#"{"useAutoModeDuringPlan":false}"#, false),
    ] {
        let (_directory, settings) = settings_with(contents);
        let snapshot = settings.read(None);
        assert_eq!(snapshot.use_auto_mode_during_plan, expected, "{contents}");
        assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
    }
}

#[test]
fn a_non_boolean_plan_setting_keeps_auto_with_a_warning() {
    let (_directory, settings) = settings_with(r#"{"useAutoModeDuringPlan":"no"}"#);
    let snapshot = settings.read(None);
    assert!(snapshot.use_auto_mode_during_plan);
    assert!(
        snapshot
            .warnings
            .iter()
            .any(|warning| warning.contains("useAutoModeDuringPlan")),
        "{:?}",
        snapshot.warnings
    );
}
