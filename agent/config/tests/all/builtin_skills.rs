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
        "`reader_executable`",
        "`tick_packages`",
        "`<package>:usage`",
        "`<package>:<skill>`",
        "`tick-<package>`",
        "`login`",
        "`logged_in`",
        "`not_required`",
        "from `/ticks` in this Agent",
        "marketplace page of `/ticks`",
        "No Agent restart is needed",
        "never suggest `!` for a login",
        "never log in, log out, install, update or uninstall a package yourself",
        "Null `data` means no content yet",
        "read it again shortly",
        "`data.connection`",
        "`retrying`",
        "`not_authorized`",
        "`failed`",
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
        "builtin",
        "tick_executable",
        "TICK_EXECUTABLE",
        "PROVIDER TICK",
        "tick-longbridge",
        "tick-futu",
        "\"quote\"",
        "\"kline\"",
        "--period",
        "MEMBER_ID",
        "TSLA.US",
        "on PATH",
        "tickloop tick",
        "bin/tick-longbridge auth",
        "TICK_PACKAGE_ROOT",
        "auth status",
        "login command the package's Skill names",
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
