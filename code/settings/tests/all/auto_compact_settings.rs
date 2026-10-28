use tickloop_code_settings::UserSettings;
use tickloop_harness::policy::settings::SettingsStore;

fn settings_with(contents: &str) -> (tempfile::TempDir, UserSettings) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("settings.json"), contents).unwrap();
    let settings = UserSettings::in_root(directory.path());
    (directory, settings)
}

#[test]
fn the_auto_compact_window_is_a_positive_integer_of_tokens_and_unset_by_default() {
    let (_directory, settings) = settings_with(r#"{"autoCompactWindow":400000}"#);
    assert_eq!(settings.auto_compact_window(), Some(400_000));
    let snapshot = settings.read(None);
    assert_eq!(snapshot.auto_compact_window, Some(400_000));
    assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);

    let (_directory, settings) = settings_with("{}");
    let snapshot = settings.read(None);
    assert_eq!(snapshot.auto_compact_window, None);
    assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
}

#[test]
fn an_invalid_auto_compact_window_is_ignored_with_a_warning() {
    for invalid in ["0", "-5", "12.5", r#""400k""#, "null", "true"] {
        let (_directory, settings) =
            settings_with(&format!(r#"{{"autoCompactWindow":{invalid}}}"#));
        let snapshot = settings.read(None);
        assert_eq!(snapshot.auto_compact_window, None, "{invalid}");
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|warning| warning.contains("autoCompactWindow")),
            "{invalid}: {:?}",
            snapshot.warnings
        );
    }
}
