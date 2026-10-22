use tickloop_agent_config::UserSettings;

fn settings_with(contents: &str) -> (tempfile::TempDir, UserSettings) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("settings.json"), contents).unwrap();
    let settings = UserSettings::in_root(directory.path());
    (directory, settings)
}

#[test]
fn a_goal_token_budget_is_a_positive_integer() {
    let (_directory, settings) = settings_with(r#"{"goalTokenBudget":250000}"#);
    assert_eq!(settings.goal_token_budget(), Some(250_000));

    let (_directory, settings) = settings_with("{}");
    let mut loaded = settings.load();
    assert_eq!(loaded.goal_token_budget(), None);
    assert!(loaded.into_warnings().is_empty());

    for invalid in ["0", "-5", "12.5", r#""lots""#] {
        let (_directory, settings) = settings_with(&format!(r#"{{"goalTokenBudget":{invalid}}}"#));
        let mut loaded = settings.load();
        assert_eq!(loaded.goal_token_budget(), None, "{invalid}");
        assert!(
            loaded
                .into_warnings()
                .iter()
                .any(|warning| warning.contains("goalTokenBudget")),
            "{invalid}"
        );
    }
}

#[test]
fn the_goal_check_model_names_a_built_in_model_or_falls_back_to_the_chat_model() {
    let (_directory, settings) =
        settings_with(r#"{"goalCheckModel":{"provider":"deepseek-api","model":"deepseek-flash"}}"#);
    let model = settings.goal_check_model().expect("a built-in model");
    assert_eq!(model.provider_id, "deepseek-api");
    assert_eq!(model.model_id, "deepseek-flash");

    let (_directory, settings) = settings_with("{}");
    assert!(settings.goal_check_model().is_none());

    let (_directory, settings) =
        settings_with(r#"{"goalCheckModel":{"provider":"nowhere","model":"none"}}"#);
    let mut loaded = settings.load();
    assert!(loaded.goal_check_model().is_none());
    assert!(
        loaded
            .into_warnings()
            .iter()
            .any(|warning| warning.contains("goalCheckModel"))
    );
}
