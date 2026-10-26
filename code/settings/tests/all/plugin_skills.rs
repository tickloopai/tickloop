use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tick_packages::PackageStore;
use tick_plugins::{InstalledPlugin, PluginStore};
use tickloop_code_settings::skills::{SkillCaller, SkillDiscovery, SkillRegistry};
use tickloop_harness::protocol::{InvokedSkill, SkillSource};

fn skill(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n")
}

/// A Claude Code plugin directory `label` under `directory` with `manifest` and `skills`, each a
/// Skill directory relative to the plugin and its SKILL.md.
fn claude_plugin(
    directory: &Path,
    label: &str,
    manifest: serde_json::Value,
    skills: &[(&str, &str)],
) -> PathBuf {
    let source = directory.join("sources").join(label);
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    fs::write(
        source.join(".claude-plugin/plugin.json"),
        manifest.to_string(),
    )
    .unwrap();
    for (skill_directory, markdown) in skills {
        let skill_directory = source.join(skill_directory);
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

fn names(registry: &SkillRegistry) -> Vec<&str> {
    registry
        .definitions()
        .map(|skill| skill.name.as_str())
        .collect()
}

/// Installs, as an earlier Tickloop would, the Tick package `name` with a `usage` Skill.
fn install_tick_package(directory: &Path, config_root: &Path, name: &str) {
    let source = directory.join("sources").join(name);
    fs::create_dir_all(source.join("bin")).unwrap();
    fs::write(
        source.join("tick-package.json"),
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
    fs::create_dir_all(source.join("skills/usage")).unwrap();
    fs::write(
        source.join("skills/usage/SKILL.md"),
        skill("usage", "Use the package", "Package body"),
    )
    .unwrap();
    PackageStore::in_config_root(config_root)
        .install_path(&source)
        .unwrap();
}

#[test]
fn enabled_plugin_skills_are_named_after_their_plugin_beside_tick_package_skills() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    install_tick_package(directory.path(), &config_root, "quotes");
    let plugin = install(
        &config_root,
        &claude_plugin(
            directory.path(),
            "notes",
            serde_json::json!({"name": "notes", "skills": ["./skills", "./extra/summary"]}),
            &[
                ("skills/write", &skill("write", "Write notes", "Write body")),
                (
                    "extra/summary",
                    &skill("summary", "Summarize notes", "Summary body"),
                ),
            ],
        ),
    );

    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));

    assert_eq!(
        names(&registry),
        [
            "loop",
            "notes:summary",
            "notes:write",
            "quotes:usage",
            "slides",
            "tickloop"
        ]
    );
    assert!(registry.warnings().is_empty(), "{:?}", registry.warnings());
    assert!(
        registry
            .model_listing()
            .contains("- notes:write [Plugin · notes]: Write notes\n"),
        "{}",
        registry.model_listing()
    );
    let expansion = registry
        .expand("notes:write", "", SkillCaller::Model)
        .unwrap();
    assert_eq!(
        expansion.source,
        SkillSource::Plugin {
            plugin: "notes".into(),
            path: plugin.root.join("skills/write/SKILL.md"),
        }
    );
    assert!(expansion.content.starts_with(&format!(
        "Base directory for this skill: {}\n",
        plugin.root.join("skills/write").display()
    )));
    assert!(expansion.content.ends_with("Write body"));

    let store = PluginStore::in_config_root(&config_root);
    store.set_enabled("notes", false).unwrap();
    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    assert_eq!(
        names(&registry),
        ["loop", "quotes:usage", "slides", "tickloop"],
        "a disabled plugin's Skills are not loaded"
    );

    store.set_enabled("notes", true).unwrap();
    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    assert!(registry.contains("notes:write"));
}

#[test]
fn plugin_skill_bodies_name_the_plugin_its_data_and_the_skill_in_either_naming() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    let user_guide = config_root.join("skills/guide");
    fs::create_dir_all(&user_guide).unwrap();
    fs::write(
        user_guide.join("SKILL.md"),
        skill("guide", "The user's guide", "User ${CLAUDE_PLUGIN_ROOT}"),
    )
    .unwrap();
    let body = "Package ${TICK_PACKAGE_ROOT}\n\
                Root ${TICKLOOP_PLUGIN_ROOT} ${CLAUDE_PLUGIN_ROOT}\n\
                Data ${TICKLOOP_PLUGIN_DATA} ${CLAUDE_PLUGIN_DATA}\n\
                Skill ${TICKLOOP_SKILL_DIR} ${CLAUDE_SKILL_DIR}";
    let notes = install(
        &config_root,
        &claude_plugin(
            directory.path(),
            "notes",
            serde_json::json!({"name": "notes"}),
            &[("skills/write", &skill("write", "Write notes", body))],
        ),
    );
    install(
        &config_root,
        &claude_plugin(
            directory.path(),
            "plain",
            serde_json::json!({"name": "plain"}),
            &[(
                "skills/read",
                &skill("read", "Read", "Root ${CLAUDE_PLUGIN_ROOT}"),
            )],
        ),
    );
    let store = PluginStore::in_config_root(&config_root);
    let data = store.data_directory("notes");
    assert!(!data.exists());

    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));

    let expansion = registry
        .expand("notes:write", "", SkillCaller::Model)
        .unwrap();
    let root = notes.root.display();
    let skill_directory = notes.root.join("skills/write");
    let skill_directory = skill_directory.display();
    let data_shown = data.display();
    assert!(
        expansion.content.ends_with(&format!(
            "Package {root}\nRoot {root} {root}\nData {data_shown} {data_shown}\n\
             Skill {skill_directory} {skill_directory}"
        )),
        "{}",
        expansion.content
    );
    assert!(
        data.is_dir(),
        "the data directory is created once a Skill uses it"
    );
    assert!(
        !store.data_directory("plain").exists(),
        "a plugin whose Skills never use its data directory gets none"
    );
    assert!(
        registry
            .expand("guide", "", SkillCaller::Model)
            .unwrap()
            .content
            .ends_with("User ${CLAUDE_PLUGIN_ROOT}"),
        "only a plugin's own Skills see its directories"
    );
}

#[test]
fn a_plugin_skill_gets_no_tools_from_its_allowed_tools() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    install(
        &config_root,
        &claude_plugin(
            directory.path(),
            "notes",
            serde_json::json!({"name": "notes"}),
            &[(
                "skills/tools",
                "---\nname: tools\ndescription: Asks for tools\nallowed-tools: [Bash, Write]\n---\nTools body\n",
            )],
        ),
    );

    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));

    let expansion = registry
        .expand("notes:tools", "", SkillCaller::Model)
        .unwrap();
    assert!(expansion.allowed_tools.is_empty());
    let invoked = InvokedSkill::try_from(expansion).unwrap();
    assert!(invoked.content.ends_with("Tools body"));
}

#[test]
fn an_unusable_plugin_or_registry_only_warns_and_a_disabled_one_is_quiet() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    let source = claude_plugin(
        directory.path(),
        "notes",
        serde_json::json!({"name": "notes"}),
        &[("skills/write", &skill("write", "Write", "Write body"))],
    );
    let notes = install(&config_root, &source);
    fs::remove_dir_all(&notes.root).unwrap();

    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    assert!(registry.contains("tickloop"));
    assert!(!registry.contains("notes:write"));
    assert_eq!(registry.warnings().len(), 1, "{:?}", registry.warnings());
    assert!(
        registry.warnings()[0].starts_with("the Skills of plugin notes are not loaded"),
        "{:?}",
        registry.warnings()
    );

    let store = PluginStore::in_config_root(&config_root);
    install(&config_root, &source);
    store.set_enabled("notes", false).unwrap();
    fs::remove_dir_all(store.get("notes").unwrap().root).unwrap();
    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    assert!(registry.warnings().is_empty(), "{:?}", registry.warnings());

    fs::write(store.directory().join("installed.json"), "not json").unwrap();
    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    assert!(registry.contains("tickloop"));
    assert_eq!(registry.warnings().len(), 1);
    assert!(
        registry.warnings()[0].starts_with("the Skills of installed plugins are not loaded"),
        "{:?}",
        registry.warnings()
    );
}

#[test]
fn an_earlier_package_skill_migrates_as_a_plugin_and_obeys_disabling() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    install_tick_package(directory.path(), &config_root, "sample");
    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    let definition = registry
        .definitions()
        .find(|skill| skill.name.starts_with("sample:"))
        .unwrap();
    assert!(matches!(definition.source, SkillSource::Plugin { .. }));
    PluginStore::in_config_root(&config_root)
        .set_enabled("sample", false)
        .unwrap();
    let registry = SkillRegistry::discover(directory.path(), Some(&config_root));
    assert!(
        !registry
            .definitions()
            .any(|skill| skill.name.starts_with("sample:"))
    );
}

#[test]
fn plugin_skills_ignore_non_string_tool_permission_metadata() {
    for value in [
        "null",
        "false",
        "42",
        "[Bash, 42, {command: Write}]",
        "{Bash: true}",
        "{[Bash, Write]: true}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let config_root = directory.path().join("config");
        let markdown = format!(
            "---\nname: guide\ndescription: >-\n  Read the guide.\nallowed-tools: {value}\n---\nBody"
        );
        install(
            &config_root,
            &claude_plugin(
                directory.path(),
                "notes",
                serde_json::json!({"name": "notes"}),
                &[("skills/guide", &markdown)],
            ),
        );
        let registry =
            SkillRegistry::discover_with_builtins(directory.path(), Some(&config_root), []);
        assert!(
            registry.warnings().is_empty(),
            "{value}: {:?}",
            registry.warnings()
        );
        let expansion = registry
            .expand("notes:guide", "", SkillCaller::Model)
            .unwrap();
        assert!(expansion.allowed_tools.is_empty());
        assert!(
            InvokedSkill::try_from(expansion)
                .unwrap()
                .content
                .ends_with("Body")
        );
    }
}

#[test]
fn cached_discovery_notices_a_skill_added_to_an_existing_plugin_directory() {
    use tickloop_code_settings::skills::SkillDiscoveryCache;
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    let source = claude_plugin(
        directory.path(),
        "design",
        serde_json::json!({"name":"design"}),
        &[],
    );
    fs::create_dir_all(source.join("skills/empty")).unwrap();
    let plugin = install(&config, &source);
    let mut cache = SkillDiscoveryCache::default();
    assert!(cache.refresh(directory.path(), Some(&config)).is_some());
    assert!(cache.refresh(directory.path(), Some(&config)).is_none());
    fs::write(
        plugin.root.join("skills/empty/SKILL.md"),
        skill("added", "Added guidance", "New body"),
    )
    .unwrap();
    let changed = cache
        .refresh(directory.path(), Some(&config))
        .expect("new Skill must invalidate cached discovery");
    assert!(changed.contains("design:added"));
}

#[test]
fn cached_discovery_refreshes_plugin_enablement_and_project_edits() {
    use tickloop_code_settings::skills::SkillDiscoveryCache;
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    let source = claude_plugin(
        directory.path(),
        "design",
        serde_json::json!({"name":"design"}),
        &[("skills/design", &skill("design", "Design", "body"))],
    );
    let store = PluginStore::in_config_root(&config);
    let mut cache = SkillDiscoveryCache::default();
    cache.refresh(directory.path(), Some(&config)).unwrap();
    install(&config, &source);
    assert!(
        cache
            .refresh(directory.path(), Some(&config))
            .unwrap()
            .contains("design:design")
    );
    store.set_enabled("design", false).unwrap();
    assert!(
        !cache
            .refresh(directory.path(), Some(&config))
            .unwrap()
            .contains("design:design")
    );
    store.set_enabled("design", true).unwrap();
    assert!(
        cache
            .refresh(directory.path(), Some(&config))
            .unwrap()
            .contains("design:design")
    );
    let project = directory.path().join(".tickloop/skills/local");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("SKILL.md"), skill("local", "Initial", "body")).unwrap();
    assert!(
        cache
            .refresh(directory.path(), Some(&config))
            .unwrap()
            .contains("local")
    );
    fs::write(
        project.join("SKILL.md"),
        skill("local", "Changed description", "body"),
    )
    .unwrap();
    let changed = cache.refresh(directory.path(), Some(&config)).unwrap();
    assert_eq!(
        changed
            .user_definitions()
            .find(|skill| skill.name == "local")
            .unwrap()
            .description,
        "Changed description"
    );
    fs::remove_file(project.join("SKILL.md")).unwrap();
    assert!(
        !cache
            .refresh(directory.path(), Some(&config))
            .unwrap()
            .contains("local")
    );
    assert!(cache.refresh(directory.path(), Some(&config)).is_none());
}
