use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tick_packages::PackageStore;
use tick_plugins::{InstalledPlugin, PluginStore};
use tickloop_code_settings::skills::{SkillCaller, SkillDiscovery, SkillRegistry};
use tickloop_harness::protocol::SkillSource;

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
