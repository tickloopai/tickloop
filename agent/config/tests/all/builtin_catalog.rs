use serde_json::json;
use std::{fs, path::Path};
use tickloop_agent_config::UserSettings;
use tickloop_agent_model::cards::ModelCatalog;

fn builtin_selection_with_ignored_overrides(root: &Path) {
    fs::create_dir_all(root.join("providers")).unwrap();
    let definition = root.join("providers/deepseek-api.yaml");
    fs::write(&definition,"id: deepseek-api\nmodels:\n  - id: deepseek-flash\n    max_output_tokens: 7\n    context_window_tokens: 8192\n").unwrap();
    let saved = json!({"defaultModel":{"provider":"deepseek-api","model":"deepseek-flash"},"effortLevel":"low","alwaysThinkingEnabled":true,"providers":[{"id":"deepseek-api","models":[{"id":"deepseek-flash","max_output_tokens":8}]}]});
    fs::write(
        root.join("settings.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    let bytes = fs::read(&definition).unwrap();
    let settings = UserSettings::in_root(root);
    let mut loaded = settings.load();
    let selected = loaded.configured_model().unwrap().unwrap();
    assert_eq!(
        selected,
        ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap(),
        "The program version owns every definition parameter"
    );
    assert_eq!(
        loaded.reasoning_effort(),
        Some(tickloop_agent_protocol::ReasoningEffort::Low)
    );
    assert!(loaded.thinking_enabled());
    loaded.configured_model().unwrap();
    let warnings = loaded.into_warnings();
    assert_eq!(
        warnings.len(),
        1,
        "Ignored external definitions must have one clear notice"
    );
    assert!(warnings[0].contains("built-in"));
    assert_eq!(fs::read(&definition).unwrap(), bytes);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(root.join("settings.json")).unwrap())
            .unwrap(),
        saved
    );
}

#[test]
fn builtin_selection_ignores_definition_and_tuning_files_with_one_notice() {
    let root = tempfile::tempdir().unwrap();
    builtin_selection_with_ignored_overrides(root.path());
}

#[test]
fn unknown_external_selection_and_invalid_definition_contents_fall_back_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("providers")).unwrap();
    let file = root.path().join("providers/external.yaml");
    fs::write(&file, "invalid YAML: [").unwrap();
    let state =
        json!({"defaultModel":{"provider":"unknown-external","model":"custom"},"providers":false});
    fs::write(
        root.path().join("settings.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let mut loaded = UserSettings::in_root(root.path()).load();
    assert!(loaded.configured_model().unwrap().is_none());
    assert_eq!(loaded.into_warnings().len(), 1);
    assert_eq!(fs::read_to_string(&file).unwrap(), "invalid YAML: [");
}

#[test]
fn legacy_builtin_snapshot_keeps_identity_but_never_definition_parameters() {
    let root = tempfile::tempdir().unwrap();
    let mut model = ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-v4-pro")
        .unwrap();
    model.base_url = "https://external.invalid".into();
    model.max_output_tokens = 1;
    model.card.as_mut().unwrap().max_output_tokens = 1;
    fs::write(
        root.path().join("settings.json"),
        serde_json::to_vec(&json!({"model":model,"contextWindowTokens":12345})).unwrap(),
    )
    .unwrap();
    let mut loaded = UserSettings::in_root(root.path()).load();
    assert_eq!(
        loaded.configured_model().unwrap().unwrap(),
        ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-v4-pro")
            .unwrap()
    );
    assert_eq!(loaded.context_window_tokens(), None);
    assert_eq!(loaded.into_warnings().len(), 1);
}

#[test]
fn canonical_builtin_and_alias_selections_need_no_definition_file_or_notice() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("settings.json"),serde_json::to_vec(&json!({"defaultModel":{"provider":"deepseek-api","model":"deepseek-flash-chat-completions"}})).unwrap()).unwrap();
    let mut loaded = UserSettings::in_root(root.path()).load();
    assert_eq!(
        loaded.configured_model().unwrap().unwrap(),
        ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap()
    );
    assert!(loaded.into_warnings().is_empty());
}
