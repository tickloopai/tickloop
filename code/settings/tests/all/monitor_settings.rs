use tickloop_code_settings::UserSettings;

#[test]
fn monitor_width_round_trips_and_ignores_values_that_are_not_column_counts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.json");
    let settings = UserSettings::in_root(directory.path());
    for invalid in [r#""wide""#, "0", "-3", "70000", "12.5"] {
        std::fs::write(&path, format!(r#"{{"monitorWidth":{invalid},"keep":1}}"#)).unwrap();
        let mut loaded = settings.load();
        assert_eq!(loaded.monitor_width(), None, "{invalid}");
        assert!(
            loaded
                .into_warnings()
                .iter()
                .any(|warning| warning.contains("monitorWidth")),
            "{invalid}"
        );
    }
    settings.save_monitor_width(64).unwrap();
    assert_eq!(settings.load().monitor_width(), Some(64));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["monitorWidth"], 64);
    assert_eq!(saved["keep"], 1, "saving keeps the other settings");
    std::fs::write(&path, "{}").unwrap();
    let mut loaded = settings.load();
    assert_eq!(loaded.monitor_width(), None);
    assert!(loaded.into_warnings().is_empty());
}

#[test]
fn shift_tab_modes_default_to_auto_and_plan_and_keep_the_cycle_order() {
    use tickloop_harness_protocol::ExecutionMode;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.json");
    let settings = UserSettings::in_root(directory.path());
    assert_eq!(
        settings.load().shift_tab_modes(),
        [ExecutionMode::Auto, ExecutionMode::Plan]
    );
    settings
        .save_shift_tab_modes(&[ExecutionMode::FullAccess, ExecutionMode::Auto])
        .unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        saved["shiftTabModes"],
        serde_json::json!(["auto", "fullAccess"])
    );
    assert_eq!(
        settings.load().shift_tab_modes(),
        [ExecutionMode::Auto, ExecutionMode::FullAccess]
    );
    std::fs::write(&path, r#"{"shiftTabModes":["plan","ask"]}"#).unwrap();
    assert_eq!(
        settings.load().shift_tab_modes(),
        [ExecutionMode::AcceptEdits, ExecutionMode::Plan],
        "the Claude Code alias `ask` reads as Ask, and the list follows the cycle order"
    );
    for invalid in ["[]", r#"["sometimes"]"#, r#""auto""#] {
        std::fs::write(&path, format!(r#"{{"shiftTabModes":{invalid}}}"#)).unwrap();
        let mut loaded = settings.load();
        assert_eq!(
            loaded.shift_tab_modes(),
            [ExecutionMode::Auto, ExecutionMode::Plan],
            "{invalid}"
        );
        assert!(
            loaded
                .into_warnings()
                .iter()
                .any(|warning| warning.contains("shiftTabModes")),
            "{invalid}"
        );
    }
}
