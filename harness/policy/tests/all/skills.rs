use tickloop_harness_policy::skills::{SkillCaller, SkillRegistry};
use tickloop_harness_protocol::SkillSource;

#[test]
fn user_short_names_resolve_uniquely_without_changing_model_names() {
    let mut registry = SkillRegistry::default();
    registry.register_markdown(
        "---\nname: frontend-design\ndescription: Design a page\n---\nDesign $ARGUMENTS",
        SkillSource::Plugin {
            plugin: "frontend-design".into(),
            path: "/plugin/SKILL.md".into(),
        },
        &[],
    );
    let expanded = registry
        .expand("/frontend-design", "a landing page", SkillCaller::User)
        .unwrap();
    assert_eq!(expanded.name, "frontend-design:frontend-design");
    assert!(expanded.content.ends_with("Design a landing page"));
    assert!(
        registry
            .expand("frontend-design", "", SkillCaller::Model)
            .is_err()
    );
    assert!(
        registry
            .expand("frontend-design:frontend-design", "", SkillCaller::Model)
            .is_ok()
    );
}

#[test]
fn user_short_name_collisions_list_full_names_and_exact_names_win() {
    let mut registry = SkillRegistry::default();
    for plugin in ["alpha", "beta"] {
        registry.register_markdown(
            "---\nname: design\ndescription: Design a page\n---\nDesign",
            SkillSource::Plugin {
                plugin: plugin.into(),
                path: format!("/{plugin}/SKILL.md").into(),
            },
            &[],
        );
    }
    let error = registry
        .expand("design", "", SkillCaller::User)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "ambiguous skill `design`; use one of: /alpha:design, /beta:design"
    );
    assert_eq!(
        registry
            .expand("beta:design", "", SkillCaller::User)
            .unwrap()
            .name,
        "beta:design"
    );
    registry.register_markdown(
        "---\nname: design\ndescription: Local design\n---\nLocal",
        SkillSource::Project {
            path: "/project/SKILL.md".into(),
        },
        &[],
    );
    assert_eq!(
        registry
            .expand("design", "", SkillCaller::User)
            .unwrap()
            .name,
        "design"
    );
}

#[test]
fn user_discovery_and_short_names_exclude_non_user_invocable_skills() {
    let mut registry = SkillRegistry::default();
    for (plugin, invocable) in [("alpha", true), ("beta", false)] {
        registry.register_markdown(
            &format!("---\nname: design\ndescription: Design a page\nuser-invocable: {invocable}\n---\nDesign"),
            SkillSource::Plugin { plugin: plugin.into(), path: format!("/{plugin}/SKILL.md").into() }, &[],
        );
    }
    assert_eq!(
        registry
            .user_definitions()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha:design"]
    );
    assert_eq!(
        registry
            .expand("design", "", SkillCaller::User)
            .unwrap()
            .name,
        "alpha:design"
    );
    assert!(
        registry
            .expand("beta:design", "", SkillCaller::User)
            .is_err()
    );
}
