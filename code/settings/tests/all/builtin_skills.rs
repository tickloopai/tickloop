use tickloop_code_settings::skills::SkillDiscovery;
use tickloop_code_settings::skills::{SLIDES_SKILL, SkillCaller, SkillError, SkillRegistry};

#[test]
fn discovered_builtins_have_no_generic_tickloop_skill() {
    let directory = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover(directory.path(), None);
    assert_eq!(
        registry
            .definitions()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["loop", "slides"]
    );
    assert!(!registry.contains("tickloop"));
    assert!(!registry.model_listing().contains("- tickloop [Built-in]"));
    assert!(
        registry
            .user_definitions()
            .all(|skill| skill.name != "tickloop")
    );
    for caller in [SkillCaller::Model, SkillCaller::User] {
        assert_eq!(
            registry.expand("tickloop", "", caller),
            Err(SkillError::Unknown("tickloop".into()))
        );
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
    assert!(registry.model_listing().contains("- loop [Built-in]"));
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
