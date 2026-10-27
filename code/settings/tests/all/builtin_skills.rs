use tickloop_code_settings::skills::SkillDiscovery;
use tickloop_code_settings::skills::{
    SLIDES_SKILL, SkillCaller, SkillError, SkillRegistry, TICKLOOP_SKILL,
};

#[test]
fn the_tickloop_skill_teaches_only_ticks_that_run_in_the_tickhost() {
    for contract in [
        "`hosted:`",
        "`shell:<task id>`",
        "`artifact:<origin>/<artifact_id>`",
        "`protocol: true`",
        "terminal Tick",
        "`frequency`",
        "`high`",
        "`medium`",
        "`low`",
        "`ready: true`",
        "Esc hides the view",
        "An artifact takes no Tick Subscription",
        "`workspace_host.plugins`",
        "`<package>:usage`",
        "`<package>:<skill>`",
        "`tick-<package>`",
        "`login`",
        "`logged_in`",
        "`not_required`",
        "from `/ticks` in this Agent",
        "Available page of `/ticks`",
        "Installed page of `/ticks`",
        "No Agent restart is needed",
        "never suggest `!` for a login",
        "never log in, log out, install, update or uninstall a package yourself",
        "Null `data` means no content yet",
        "read it again shortly",
        "`data.connection`",
        "`retrying`",
        "`not_authorized`",
        "`failed`",
        "`Disconnected`",
    ] {
        assert!(TICKLOOP_SKILL.contains(contract), "{contract}");
    }
    for retired in [
        "read the body with `ArtifactRead` when the task needs it",
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
        "doc or page",
        "reader_executable",
        "tick-reader",
        "longbridge",
        "futu",
        "K-line",
        "stock",
        "tickloop plugin enable",
        "tickloop plugin configure",
        "nest delegation",
        "directly delegated children",
        "names your own root",
        "newest Loop",
        "`armed`",
        "SearchTools",
        "ExecuteTools",
        "ListMcpResourcesTool",
        "Subagent",
        "MCP",
        "Loop",
        "16 KiB",
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

#[test]
fn slides_is_a_user_only_skill_that_starts_from_the_artifact_slides_guidance() {
    let directory = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover(directory.path(), None);
    assert!(registry.is_user_only("slides"));
    assert!(!registry.is_user_only("tickloop"));
    assert!(!registry.model_listing().contains("slides"));
    assert!(
        registry
            .tool_definition()
            .unwrap()
            .description
            .lines()
            .all(|line| !line.starts_with("- slides"))
    );
    assert_eq!(
        registry.expand("slides", "", SkillCaller::Model),
        Err(SkillError::ModelInvocationDisabled("slides".into()))
    );
    let expansion = registry
        .expand("/slides", "Q3 revenue review", SkillCaller::User)
        .unwrap();
    assert!(
        expansion.content.contains(
            "The user ran /slides to make a new Slides artifact. Topic: Q3 revenue review"
        )
    );
    for step in [
        "ask the user with AskUserQuestion what these Slides should present",
        "such as the audience or the length, so every answer comes back while you work on these Slides",
        "call ArtifactLogin and give the user its link and code; then ask with AskUserQuestion whether they have signed in",
        "load `tickloop-artifacts:artifact-slides` with the Skill tool",
        "Create the Slides with ArtifactEdit (`action: \"create\"`, `format: \"slides\"`)",
        "upload it with ArtifactUpload",
    ] {
        assert!(SLIDES_SKILL.contains(step), "{step}");
    }
    assert!(
        registry
            .expand("slides", "", SkillCaller::User)
            .unwrap()
            .content
            .contains("Topic: \n")
    );
}
