use super::*;
use tickloop_agent_protocol::{SearchConfig, SearchProviderKind};

#[test]
fn search_defaults_and_explicit_off_survive_settings_reload() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    assert_eq!(
        settings.load().search_config(),
        SearchConfig::enabled_default()
    );
    let config = SearchConfig::for_provider(SearchProviderKind::Exa);
    settings.save_search_config(Some(&config)).unwrap();
    assert_eq!(settings.load().search_config(), Some(config));
    settings.save_search_config(None).unwrap();
    assert_eq!(settings.load().search_config(), None);
    let value: Value =
        serde_json::from_slice(&std::fs::read(directory.path().join(SETTINGS_FILE_NAME)).unwrap())
            .unwrap();
    assert!(value["webSearch"].is_null());
}

#[test]
fn invalid_search_settings_fail_closed_without_echoing_endpoint_secrets() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    std::fs::write(directory.path().join(SETTINGS_FILE_NAME), r#"{"webSearch":{"provider":"exa","endpoint":"https://user:fixture-secret@example.com/search"},"otherPreference":42}"#).unwrap();
    let mut loaded = settings.load();
    assert_eq!(loaded.search_config(), None);
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("webSearch"))
    );
    assert!(
        !loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("fixture-secret"))
    );
    settings
        .save_search_config(SearchConfig::enabled_default().as_ref())
        .unwrap();
    let value: Value =
        serde_json::from_slice(&std::fs::read(directory.path().join(SETTINGS_FILE_NAME)).unwrap())
            .unwrap();
    assert_eq!(value["otherPreference"], 42);
}
