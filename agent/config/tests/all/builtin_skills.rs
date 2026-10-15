use tickloop_agent_config::skills::{SkillCaller, SkillRegistry, TICKLOOP_SKILL};
use tickloop_agent_protocol::SkillSource;

const ARTIFACT_TOOLS: [&str; 3] = ["ArtifactRead", "ArtifactEdit", "ArtifactComment"];

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
fn artifact_draft_is_a_model_invocable_builtin_that_names_only_real_artifact_tools() {
    let directory = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover(directory.path(), None);
    assert!(registry.warnings().is_empty());

    let listing = registry.model_listing();
    assert!(listing.contains("artifact-draft"));
    assert!(listing.contains("/artifact/<id>"));

    let expansion = registry
        .expand("artifact-draft", "", SkillCaller::Model)
        .unwrap();
    assert!(matches!(expansion.source, SkillSource::BuiltIn));
    for tool in ARTIFACT_TOOLS {
        assert!(expansion.content.contains(tool), "{tool}");
    }
    for contract in [
        "\"action\":\"read\"",
        "\"action\":\"patch\"",
        "\"action\":\"replace\"",
        "\"action\":\"rename\"",
        "expected_hash",
        "expected_revision",
        "expected_title",
        "content_conflict",
        "# <title>",
    ] {
        assert!(expansion.content.contains(contract), "{contract}");
    }
}
