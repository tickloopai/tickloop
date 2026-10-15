use tickloop_agent_config::skills::{SkillCaller, SkillRegistry, TICKLOOP_SKILL};

#[test]
fn the_tickloop_skill_teaches_only_ticks_that_run_in_the_tickhost() {
    for contract in [
        "`hosted:`",
        "`shell:<task id>`",
        "`protocol: true`",
        "terminal Tick",
        "`send_input`",
        "`frequency`",
        "`high`",
        "`medium`",
        "`low`",
        "`view_of`",
        "`view.status`",
        "Esc hides the view",
        "An artifact takes no Tick Subscription",
    ] {
        assert!(TICKLOOP_SKILL.contains(contract), "{contract}");
    }
    for retired in [
        "herdr:",
        "every_seconds",
        "5–86400",
        "tickhost_executable",
        "\"open\", \"--\"",
        "Terminal Observation",
        "every 5 seconds",
        "text snapshot",
        "exits that Tick",
        "idle minutes",
        "from idling out",
    ] {
        assert!(!TICKLOOP_SKILL.contains(retired), "{retired}");
    }
}

#[test]
fn artifact_guidance_is_not_duplicated_in_the_builtin_registry() {
    let directory = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover(directory.path(), None);
    assert!(!registry.model_listing().contains("artifact-draft"));
    assert!(
        registry
            .expand("artifact-draft", "", SkillCaller::Model)
            .is_err()
    );
    assert!(registry.model_listing().contains("tickloop"));
}
