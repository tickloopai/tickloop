use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tickloop_code_settings::skills::SkillDiscovery;

use tick_packages::MANIFEST_FILE_NAME;
use tick_plugins::{InstalledPlugin, PluginStore, REGISTRY_FILE_NAME};
use tickloop_code_settings::skills::{
    PACKAGE_ROOT_VARIABLE, SkillCaller, SkillError, SkillRegistry,
};
use tickloop_harness::protocol::{InvokedSkill, SkillSource};

/// A package directory `name` under `directory` with its program and `skills`, each a Skill
/// directory name and its SKILL.md.
fn package_source(directory: &Path, name: &str, skills: &[(&str, &str)]) -> PathBuf {
    let source = directory.join("sources").join(name);
    fs::create_dir_all(source.join("bin")).unwrap();
    fs::write(
        source.join(MANIFEST_FILE_NAME),
        serde_json::json!({
            "name": name,
            "version": "1.0.0",
            "description": "A test package",
            "executable": format!("bin/tick-{name}"),
            "protocol": true,
        })
        .to_string(),
    )
    .unwrap();
    let program = source.join("bin").join(format!("tick-{name}"));
    fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    for (skill, markdown) in skills {
        let skill_directory = source.join("skills").join(skill);
        fs::create_dir_all(&skill_directory).unwrap();
        fs::write(skill_directory.join("SKILL.md"), markdown).unwrap();
    }
    source
}

fn install(config_root: &Path, source: &Path) -> InstalledPlugin {
    PluginStore::in_config_root(config_root)
        .install_path(source)
        .unwrap()
        .plugin
}

fn skill(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n")
}

#[test]
fn package_skills_are_named_after_their_package_with_its_install_directory_in_their_body() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    let user_guide = config_root.join("skills/guide");
    fs::create_dir_all(&user_guide).unwrap();
    fs::write(
        user_guide.join("SKILL.md"),
        skill(
            "guide",
            "The user's guide",
            "User body ${TICK_PACKAGE_ROOT}",
        ),
    )
    .unwrap();
    let package = install(
        &config_root,
        &package_source(
            directory.path(),
            "sample",
            &[
                (
                    "guide",
                    &skill(
                        "guide",
                        "Open sample Ticks",
                        "The fields are listed in `${TICK_PACKAGE_ROOT}/docs/fields.md`.",
                    ),
                ),
                (
                    "tickloop",
                    &skill("tickloop", "Not the system Skill", "Package body"),
                ),
                ("broken", &skill("Broken", "Badly named", "Broken body")),
                ("other", &skill("guide", "A second guide", "Second body")),
            ],
        ),
    );

    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));

    assert_eq!(
        registry
            .definitions()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["guide", "loop", "sample:guide", "sample:tickloop", "slides"]
    );
    assert!(
        registry
            .model_listing()
            .contains("- sample:guide [Plugin · sample]: Open sample Ticks\n")
    );
    let expansion = registry
        .expand("sample:guide", "", SkillCaller::Model)
        .unwrap();
    assert!(expansion.content.contains(&format!(
        "Base directory for this skill: {}",
        package.root.join("skills/guide").display()
    )));
    assert!(expansion.content.ends_with(&format!(
        "The fields are listed in `{}/docs/fields.md`.",
        package.root.display()
    )));
    assert_eq!(
        expansion.source,
        SkillSource::Plugin {
            plugin: "sample".into(),
            path: package.root.join("skills/guide/SKILL.md"),
        }
    );
    assert_eq!(
        registry
            .expand("/sample:guide", "", SkillCaller::User)
            .unwrap()
            .content,
        expansion.content
    );
    let user_expansion = registry.expand("guide", "", SkillCaller::User).unwrap();
    assert!(
        user_expansion
            .content
            .ends_with(&format!("User body {PACKAGE_ROOT_VARIABLE}")),
        "only a package's own Skills see its install directory"
    );
    assert!(
        !user_expansion
            .content
            .contains(package.root.to_string_lossy().as_ref())
    );
    assert_eq!(
        registry.expand("tickloop", "", SkillCaller::Model),
        Err(SkillError::Unknown("tickloop".into()))
    );
    for caller in [SkillCaller::Model, SkillCaller::User] {
        let expansion = registry.expand("sample:tickloop", "", caller).unwrap();
        assert_eq!(
            expansion.source,
            SkillSource::Plugin {
                plugin: "sample".into(),
                path: package.root.join("skills/tickloop/SKILL.md"),
            }
        );
        assert!(expansion.content.ends_with("Package body"));
    }
    assert_eq!(
        registry
            .expand("tickloop", "", SkillCaller::User)
            .unwrap()
            .name,
        "sample:tickloop"
    );
    let warnings = registry.warnings();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("skills/broken/SKILL.md is ignored")),
        "{warnings:?}"
    );
    let duplicate = format!(
        "Skill `sample:guide` from {} is ignored because that name is already registered",
        package.root.join("skills/other/SKILL.md").display()
    );
    assert!(warnings.contains(&duplicate), "{warnings:?}");
}

#[test]
fn package_skills_get_no_authority_a_user_skill_lacks() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    install(
        &config_root,
        &package_source(
            directory.path(),
            "sample",
            &[
                (
                    "manual",
                    "---\nname: manual\ndescription: Manual\ndisable-model-invocation: true\n---\nManual body\n",
                ),
                (
                    "tools",
                    "---\nname: tools\ndescription: Asks for tools\nallowed-tools: [Bash]\n---\nTools body\n",
                ),
            ],
        ),
    );

    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));

    assert!(!registry.model_listing().contains("sample:manual"));
    assert_eq!(
        registry
            .expand("sample:manual", "", SkillCaller::Model)
            .unwrap_err(),
        SkillError::ModelInvocationDisabled("sample:manual".into())
    );
    let expansion = registry
        .expand("sample:tools", "", SkillCaller::User)
        .unwrap();
    assert_eq!(
        InvokedSkill::try_from(expansion).unwrap().name,
        "sample:tools"
    );
}

#[test]
fn an_unusable_package_or_registry_only_warns() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    let package = install(
        &config_root,
        &package_source(
            directory.path(),
            "sample",
            &[("guide", &skill("guide", "Guide", "Guide body"))],
        ),
    );
    fs::write(package.root.join(MANIFEST_FILE_NAME), "not json").unwrap();

    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    assert!(!registry.contains("sample:guide"));
    assert!(registry.contains("loop"));
    assert_eq!(registry.warnings().len(), 1);
    assert!(
        registry.warnings()[0].starts_with("the Skills of plugin sample are not loaded"),
        "{:?}",
        registry.warnings()
    );

    fs::write(
        config_root.join("plugins").join(REGISTRY_FILE_NAME),
        "not json",
    )
    .unwrap();
    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    assert!(registry.contains("loop"));
    assert_eq!(registry.warnings().len(), 1);
    assert!(
        registry.warnings()[0].starts_with("the Skills of installed plugins are not loaded"),
        "{:?}",
        registry.warnings()
    );
}
