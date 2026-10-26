//! `tickloop plugin` as a user runs it, against a private config root, with plugins from local
//! directories.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::tick_package_commands::{Fixture, failed, path_text, succeeded};

/// A Claude Code plugin directory `label` with `manifest`, a Skill per name in `skills/`, and the
/// commands and MCP servers Tickloop does not use yet.
fn claude_plugin(fixture: &Fixture, label: &str, manifest: Value, skills: &[&str]) -> PathBuf {
    let directory = fixture.root.join("sources").join(label);
    fs::create_dir_all(directory.join(".claude-plugin")).unwrap();
    fs::write(
        directory.join(".claude-plugin/plugin.json"),
        manifest.to_string(),
    )
    .unwrap();
    for skill in skills {
        let skill_directory = directory.join("skills").join(skill);
        fs::create_dir_all(&skill_directory).unwrap();
        fs::write(
            skill_directory.join("SKILL.md"),
            format!("---\nname: {skill}\ndescription: Uses {skill}\n---\nBody\n"),
        )
        .unwrap();
    }
    fs::create_dir_all(directory.join("commands")).unwrap();
    fs::write(directory.join("commands/tidy.md"), "Tidy up\n").unwrap();
    fs::write(directory.join(".mcp.json"), "{}").unwrap();
    directory
}

fn installed_root(fixture: &Fixture, name: &str) -> String {
    path_text(
        &fixture
            .root
            .join("config/plugins/cache/local")
            .join(name)
            .join("local"),
    )
}

fn plugin(fixture: &Fixture, arguments: &[&str]) -> std::process::Output {
    let arguments = [&["plugin"][..], arguments].concat();
    fixture.tickloop(&arguments)
}

#[test]
fn a_claude_plugin_installs_lists_toggles_and_uninstalls_through_the_cli() {
    let fixture = Fixture::new();
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "No plugins are installed.\n"
    );
    let source = claude_plugin(
        &fixture,
        "notes",
        json!({"name": "notes", "version": "1.0.0", "skills": ["./skills", "./missing"]}),
        &["write"],
    );
    let source_path = path_text(&source);
    let root = installed_root(&fixture, "notes");

    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "--path", &source_path])),
        format!(
            "Installed plugin notes@local 1.0.0 in {root}.\n\
             Not supported yet, so not used: commands, MCP servers.\n\
             Warning: skills path \"./missing\" is ignored: it does not exist\n"
        )
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "notes@local  1.0.0  enabled  no login needed  not supported yet: commands, MCP servers\n"
    );

    assert_eq!(
        succeeded(&plugin(&fixture, &["disable", "notes"])),
        "Disabled plugin notes@local; its Skills stop loading from the next request.\n"
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["disable", "notes"])),
        "Plugin notes@local is already disabled.\n"
    );
    let reinstalled = succeeded(&plugin(&fixture, &["install", "--path", &source_path]));
    assert!(
        reinstalled.starts_with(&format!(
            "Reinstalled plugin notes@local 1.0.0 in {root}.\n\
             It stays disabled; tickloop plugin enable notes turns it on.\n"
        )),
        "{reinstalled}"
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "notes@local  1.0.0  disabled  no login needed  not supported yet: commands, MCP servers\n"
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["enable", "notes"])),
        "Enabled plugin notes@local; its Skills load from the next request.\n"
    );

    fs::write(
        source.join(".claude-plugin/plugin.json"),
        json!({"name": "notes", "version": "1.1.0"}).to_string(),
    )
    .unwrap();
    fs::remove_dir_all(source.join("commands")).unwrap();
    fs::remove_file(source.join(".mcp.json")).unwrap();
    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "--path", &source_path])),
        format!("Replaced plugin notes@local 1.0.0 with 1.1.0 in {root}.\n")
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "notes@local  1.1.0  enabled  no login needed\n"
    );

    let data = fixture.root.join("config/plugins/data/notes");
    fs::create_dir_all(&data).unwrap();
    assert_eq!(
        succeeded(&plugin(&fixture, &["uninstall", "notes", "--keep-data"])),
        format!(
            "Uninstalled plugin notes@local 1.1.0.\nIts data stays in {}.\n",
            path_text(&data)
        )
    );
    assert!(data.is_dir());
    assert!(!Path::new(&root).exists());
    assert_eq!(
        failed(&plugin(&fixture, &["uninstall", "notes"])),
        "Error: plugin notes is not installed\n"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["enable", "notes"])),
        "Error: plugin notes is not installed\n"
    );

    succeeded(&plugin(&fixture, &["install", "--path", &source_path]));
    assert_eq!(
        succeeded(&plugin(&fixture, &["uninstall", "notes"])),
        "Uninstalled plugin notes@local 1.1.0.\n"
    );
    assert!(!data.exists());
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "No plugins are installed.\n"
    );
}
