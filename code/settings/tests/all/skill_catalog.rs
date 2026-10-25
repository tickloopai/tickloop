use std::fs;
use tickloop_code_settings::skills::SkillDiscovery;

use tickloop_code_settings::skills::SkillRegistry;
use tickloop_harness::protocol::{RemoteSkillDefinition, SkillSource};

fn registry_with_project_skills(
    count: usize,
    description: &str,
) -> (tempfile::TempDir, SkillRegistry) {
    let directory = tempfile::tempdir().unwrap();
    for index in 0..count {
        let name = format!("workflow-{index:02}");
        let path = directory.path().join(".tickloop/skills").join(&name);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nPrivate body\n"),
        )
        .unwrap();
    }
    let registry = SkillRegistry::discover_with_builtins(directory.path(), None, []);
    (directory, registry)
}

fn remote(name: &str, description: &str) -> RemoteSkillDefinition {
    RemoteSkillDefinition {
        name: format!("tickloop-artifacts:{name}"),
        description: description.into(),
        source: SkillSource::Remote {
            server_id: "https://artifacts.example.test".into(),
            account_id: "test-account".into(),
            uri: format!("skill://tickloop-artifacts/{name}/SKILL.md"),
            resources: Vec::new(),
        },
    }
}

#[test]
fn skill_catalog_preserves_names_when_even_the_minimum_exceeds_budget() {
    let (_directory, mut registry) = registry_with_project_skills(30, "Useful guidance");
    registry.replace_remote(vec![remote("artifact-design", "Page guidance")]);
    let listing = registry.model_listing_with_budget(1);
    for skill in registry.definitions() {
        assert!(
            listing
                .lines()
                .any(|line| line == format!("- {} [{}]", skill.name, skill.source.display_label()))
        );
    }
    assert!(!listing.contains("Private body"));
    assert!(!listing.contains("Useful guidance"));
    assert!(!listing.contains("Page guidance"));
}

#[test]
fn skill_catalog_keeps_long_descriptions_until_the_current_cap() {
    let description = "a".repeat(1_200);
    let (_directory, registry) = registry_with_project_skills(1, &description);
    assert!(registry.model_listing().contains(&description));
}

#[test]
fn skill_catalog_reports_unicode_description_capping_without_cutting_characters() {
    let description = "\u{1f680}".repeat(2_000);
    let (_directory, registry) = registry_with_project_skills(1, &description);
    let catalog = registry.model_catalog_for_context(Some(1_000_000));
    assert_eq!(catalog.capped_descriptions, ["workflow-00"]);
    assert!(catalog.omitted_descriptions.is_empty());
    let shown = catalog
        .content
        .lines()
        .next()
        .unwrap()
        .split_once(": ")
        .unwrap()
        .1;
    assert_eq!(shown.chars().count(), 1_536);
    assert!(shown.ends_with('\u{2026}'));
}

#[test]
fn skill_catalog_budgets_remote_and_project_descriptions_together() {
    let (_directory, mut registry) = registry_with_project_skills(4, &"Local ".repeat(80));
    registry.replace_remote(vec![
        remote("artifact-design", &"Design ".repeat(80)),
        remote("dataviz", &"Chart ".repeat(80)),
    ]);
    let small = registry.model_catalog_for_context(Some(8_192));
    let large = registry.model_catalog_for_context(Some(200_000));
    assert_eq!(small.budget_bytes, 327);
    assert!(small.content.len() <= small.budget_bytes);
    assert_eq!(small.omitted_descriptions.len(), 6);
    assert!(large.omitted_descriptions.is_empty());
    assert!(large.content.contains("Local Local"));
    assert!(large.content.contains("Design Design"));
    assert!(large.content.contains("Chart Chart"));
    for skill in registry.definitions() {
        assert!(small.content.contains(&skill.name));
        assert!(large.content.contains(&skill.name));
    }
}

#[test]
fn skill_catalog_uses_available_space_without_losing_later_names() {
    let (_directory, registry) = registry_with_project_skills(40, &"Guidance ".repeat(60));
    let catalog = registry.model_catalog_for_context(Some(50_000));
    assert_eq!(catalog.budget_bytes, 2_000);
    assert!(catalog.content.len() <= catalog.budget_bytes);
    assert!(!catalog.omitted_descriptions.is_empty());
    assert!(catalog.omitted_descriptions.len() < 40);
    assert!(catalog.content.contains("workflow-39"));
    assert!(!catalog.content.contains("Private body"));
}

#[test]
fn skill_catalog_preserves_builtin_guidance_even_when_the_budget_is_exhausted() {
    let directory = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover_with_builtins(
        directory.path(),
        None,
        ["---\nname: core\ndescription: Core workflow guidance\n---\nPrivate body"],
    );
    assert!(
        registry
            .model_listing_with_budget(1)
            .contains("Core workflow guidance")
    );
}

#[test]
fn skill_catalog_has_a_stable_fallback_when_model_context_is_unknown() {
    let (_directory, registry) = registry_with_project_skills(20, &"Guidance ".repeat(60));
    assert_eq!(
        registry.model_catalog_for_context(None),
        registry.model_catalog_for_context(Some(200_000)),
    );
}

#[test]
fn skill_catalog_keeps_invocation_filters_independent_from_budgeting() {
    let directory = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover_with_builtins(
        directory.path(),
        None,
        [
            "---\nname: user-only\ndescription: Explicit workflow\ndisable-model-invocation: true\n---\nPrivate body",
            "---\nname: model-only\ndescription: Automatic workflow\nuser-invocable: false\n---\nPrivate body",
        ],
    );
    let catalog = registry.model_catalog_for_context(Some(8_192));
    assert!(!catalog.content.contains("user-only"));
    assert!(catalog.content.contains("model-only"));
    assert!(!catalog.content.contains("Private body"));
}
