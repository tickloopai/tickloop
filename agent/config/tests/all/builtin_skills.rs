use tickloop_agent_config::skills::{SkillCaller, SkillRegistry};
use tickloop_agent_protocol::SkillSource;

const ARTIFACT_TOOLS: [&str; 3] = ["ArtifactRead", "ArtifactEdit", "ArtifactComment"];

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
        "expected_hash",
        "expected_revision",
        "content_conflict",
        "# <title>",
    ] {
        assert!(expansion.content.contains(contract), "{contract}");
    }
}
