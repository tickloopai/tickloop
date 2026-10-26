use std::{collections::BTreeSet, fs, os::unix::fs::symlink, path::PathBuf};

use serde_json::json;
use tick_packages::PackageAuth;
use tick_plugins::{Component, ManifestFormat, PluginManifest, TickProgram};

use crate::support::{Fixture, write_json, write_program, write_skill};

#[test]
fn a_claude_plugin_lists_its_skills_and_the_components_tickloop_does_not_use_yet() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin(
        "review",
        &json!({
            "name": "review",
            "version": "1.2.0",
            "description": "Reviews changes",
            "author": {"name": "Someone"},
            "mcpServers": {"github": {"command": "github-mcp"}},
            "unknownField": true,
        }),
        &["check", "summarize"],
    );
    fs::create_dir_all(root.join("commands")).unwrap();
    write_json(&root.join("hooks/hooks.json"), &json!({"hooks": {}}));

    let manifest = PluginManifest::load(&root, "unused", None).unwrap();

    assert_eq!(manifest.name, "review");
    assert_eq!(manifest.version.as_deref(), Some("1.2.0"));
    assert_eq!(manifest.description.as_deref(), Some("Reviews changes"));
    assert_eq!(manifest.format, ManifestFormat::Claude);
    assert_eq!(manifest.tick, None);
    assert_eq!(manifest.auth, PackageAuth::None);
    assert_eq!(manifest.skill_paths, vec![PathBuf::from("skills")]);
    assert_eq!(
        manifest.components,
        BTreeSet::from([
            Component::Skills,
            Component::Commands,
            Component::Hooks,
            Component::McpServers,
        ])
    );
    assert_eq!(
        manifest.unsupported(),
        vec![Component::Commands, Component::Hooks, Component::McpServers]
    );
    assert!(manifest.warnings.is_empty(), "{:?}", manifest.warnings);
    let root = root.canonicalize().unwrap();
    assert_eq!(
        tick_plugins::skill_directories(&root, &manifest.skill_paths),
        vec![root.join("skills/check"), root.join("skills/summarize")]
    );
}

#[test]
fn the_tickloop_manifest_comes_first_then_the_claude_one_then_tick_package_json() {
    let fixture = Fixture::new();
    let root = fixture.tick_package("all", "sample", "1.0.0");
    write_json(
        &root.join(".claude-plugin/plugin.json"),
        &json!({"name": "from-claude"}),
    );

    let claude = PluginManifest::load(&root, "unused", None).unwrap();
    assert_eq!(claude.name, "from-claude");
    assert_eq!(claude.format, ManifestFormat::Claude);
    assert_eq!(
        claude.warnings,
        vec!["tick-package.json is ignored beside .claude-plugin/plugin.json"]
    );

    write_json(
        &root.join(".tickloop-plugin/plugin.json"),
        &json!({"name": "from-tickloop"}),
    );
    let tickloop = PluginManifest::load(&root, "unused", None).unwrap();
    assert_eq!(tickloop.name, "from-tickloop");
    assert_eq!(tickloop.format, ManifestFormat::Tickloop);
}

#[test]
fn a_tick_package_is_a_plugin_with_a_tick_and_its_login() {
    let fixture = Fixture::new();
    let root = fixture.tick_package("quotes", "quotes", "0.2.7");

    let manifest = PluginManifest::load(&root, "unused", None).unwrap();

    assert_eq!(manifest.format, ManifestFormat::TickPackage);
    assert_eq!(manifest.name, "quotes");
    assert_eq!(manifest.version.as_deref(), Some("0.2.7"));
    assert_eq!(
        manifest.tick,
        Some(TickProgram {
            executable: PathBuf::from("bin/tick-quotes"),
            protocol: true,
        })
    );
    assert!(matches!(manifest.auth, PackageAuth::ApiKey { .. }));
    assert_eq!(
        manifest.components,
        BTreeSet::from([Component::Tick, Component::Skills])
    );
    assert!(manifest.unsupported().is_empty());
}

#[test]
fn a_tickloop_manifest_declares_a_tick_by_the_rules_of_tick_package_json() {
    let fixture = Fixture::new();
    let root = fixture.source("quotes");
    write_program(&root.join("bin/tick-quotes"), "quotes");
    let manifest = json!({
        "name": "quotes",
        "version": "1.0.0",
        "description": "Quotes",
        "tick": {"executable": "bin/tick-quotes", "protocol": true},
        "auth": {"type": "api_key", "env": "QUOTES_KEY", "label": "Quotes key"},
    });
    write_json(&root.join(".tickloop-plugin/plugin.json"), &manifest);

    let loaded = PluginManifest::load(&root, "unused", None).unwrap();
    assert_eq!(
        loaded.tick,
        Some(TickProgram {
            executable: PathBuf::from("bin/tick-quotes"),
            protocol: true,
        })
    );
    assert_eq!(
        loaded.auth,
        PackageAuth::ApiKey {
            env: "QUOTES_KEY".to_owned(),
            label: "Quotes key".to_owned(),
        }
    );
    assert!(loaded.components.contains(&Component::Tick));

    let mut misnamed = manifest.clone();
    misnamed["tick"]["executable"] = json!("bin/quotes");
    write_json(&root.join(".tickloop-plugin/plugin.json"), &misnamed);
    let error = PluginManifest::load(&root, "unused", None).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("tick in .tickloop-plugin/plugin.json is not valid")
            && !error.to_string().contains("tick-package.json"),
        "{error}"
    );

    let mut unversioned = manifest.clone();
    unversioned.as_object_mut().unwrap().remove("version");
    write_json(&root.join(".tickloop-plugin/plugin.json"), &unversioned);
    let error = PluginManifest::load(&root, "unused", None).unwrap_err();
    assert!(error.to_string().contains("needs a version"), "{error}");

    let mut missing = manifest.clone();
    missing["tick"]["executable"] = json!("bin/tick-quotes-missing");
    missing["name"] = json!("quotes-missing");
    write_json(&root.join(".tickloop-plugin/plugin.json"), &missing);
    let error = PluginManifest::load(&root, "unused", None).unwrap_err();
    assert!(error.to_string().contains("does not exist"), "{error}");
}

#[test]
fn auth_without_a_tick_and_ticks_in_other_formats_are_ignored() {
    let fixture = Fixture::new();
    let root = fixture.source("plain");
    write_json(
        &root.join(".tickloop-plugin/plugin.json"),
        &json!({"name": "plain", "auth": {"type": "none"}}),
    );
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    assert_eq!(manifest.tick, None);
    assert_eq!(
        manifest.warnings,
        vec!["auth in .tickloop-plugin/plugin.json is ignored: only a plugin with a tick logs in"]
    );

    let claude = fixture.claude_plugin(
        "claude",
        &json!({"name": "claude", "tick": {"executable": "bin/tick-claude", "protocol": true}}),
        &[],
    );
    assert_eq!(
        PluginManifest::load(&claude, "unused", None).unwrap().tick,
        None
    );
}

#[test]
fn the_skills_field_replaces_the_default_directory_and_drops_paths_that_leave_the_plugin() {
    let fixture = Fixture::new();
    let outside = fixture.source("outside");
    write_skill(&outside.join("stolen"), "stolen");
    let root = fixture.claude_plugin(
        "paths",
        &json!({
            "name": "paths",
            "skills": [
                "./extra",
                "./single",
                "extra",
                "./../outside",
                "/etc",
                "./missing",
                "./escape",
            ],
        }),
        &["default"],
    );
    write_skill(&root.join("extra/one"), "one");
    write_skill(&root.join("single"), "single");
    symlink(&outside, root.join("escape")).unwrap();

    let manifest = PluginManifest::load(&root, "unused", None).unwrap();

    assert_eq!(
        manifest.skill_paths,
        vec![PathBuf::from("extra"), PathBuf::from("single")]
    );
    let root = root.canonicalize().unwrap();
    assert_eq!(
        tick_plugins::skill_directories(&root, &manifest.skill_paths),
        vec![root.join("extra/one"), root.join("single")]
    );
    assert_eq!(
        manifest.warnings,
        vec![
            "skills path \"extra\" is ignored: it must start with ./",
            "skills path \"./../outside\" is ignored: it must stay inside the plugin",
            "skills path \"/etc\" is ignored: it must start with ./",
            "skills path \"./missing\" is ignored: it does not exist",
            "skills path \"./escape\" is ignored: it leads outside the plugin",
        ]
    );

    write_json(
        &root.join(".claude-plugin/plugin.json"),
        &json!({"name": "paths", "skills": {"not": "a path"}}),
    );
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    assert!(manifest.skill_paths.is_empty());
    assert!(!manifest.components.contains(&Component::Skills));
    assert_eq!(
        manifest.warnings,
        vec!["skills is ignored: it must be a path or a list of paths"]
    );
}

#[test]
fn a_plugin_without_a_manifest_takes_its_marketplace_entry_or_its_directory_name() {
    let fixture = Fixture::new();
    let root = fixture.source("bare");
    write_skill(&root.join("skills/tidy"), "tidy");
    fs::create_dir_all(root.join("custom-commands")).unwrap();
    fs::write(root.join("custom-commands/tidy.md"), "Tidy up").unwrap();
    let entry = json!({
        "name": "from-entry",
        "description": "Tidies up",
        "source": "./plugins/bare",
        "strict": false,
        "commands": ["./custom-commands/tidy.md"],
    });

    let from_entry = PluginManifest::load(&root, "bare", Some(entry.as_object().unwrap())).unwrap();
    assert_eq!(from_entry.format, ManifestFormat::MarketplaceEntry);
    assert_eq!(from_entry.name, "from-entry");
    assert_eq!(from_entry.description.as_deref(), Some("Tidies up"));
    assert_eq!(
        from_entry.components,
        BTreeSet::from([Component::Skills, Component::Commands])
    );

    let from_directory = PluginManifest::load(&root, "bare", None).unwrap();
    assert_eq!(from_directory.format, ManifestFormat::Directory);
    assert_eq!(from_directory.name, "bare");
    assert_eq!(
        from_directory.components,
        BTreeSet::from([Component::Skills])
    );

    let error = PluginManifest::load(&root, "Not A Name", None).unwrap_err();
    assert!(
        error.to_string().contains("is not a plugin name"),
        "{error}"
    );
}

#[test]
fn a_manifest_that_cannot_name_the_plugin_fails() {
    let fixture = Fixture::new();
    let root = fixture.source("broken");
    let manifest_path = root.join(".claude-plugin/plugin.json");

    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    fs::write(&manifest_path, "{not json").unwrap();
    let error = PluginManifest::load(&root, "broken", None).unwrap_err();
    assert!(error.to_string().contains("is not valid JSON"), "{error}");

    write_json(&manifest_path, &json!(["a list"]));
    let error = PluginManifest::load(&root, "broken", None).unwrap_err();
    assert!(
        error.to_string().contains("must hold a JSON object"),
        "{error}"
    );

    write_json(&manifest_path, &json!({"version": "1.0.0"}));
    let error = PluginManifest::load(&root, "broken", None).unwrap_err();
    assert_eq!(error.to_string(), ".claude-plugin/plugin.json has no name");

    write_json(&manifest_path, &json!({"name": "Has Spaces"}));
    let error = PluginManifest::load(&root, "broken", None).unwrap_err();
    assert!(
        error.to_string().contains("is not a plugin name"),
        "{error}"
    );

    write_json(&manifest_path, &json!({"name": "broken", "version": "1 0"}));
    let error = PluginManifest::load(&root, "broken", None).unwrap_err();
    assert!(error.to_string().starts_with("version in"), "{error}");

    write_json(
        &manifest_path,
        &json!({"name": "broken", "description": "line\u{7}bell"}),
    );
    let manifest = PluginManifest::load(&root, "broken", None).unwrap();
    assert_eq!(manifest.description, None);
    assert_eq!(manifest.warnings.len(), 1, "{:?}", manifest.warnings);
}

#[test]
fn unsupported_component_paths_are_checked_without_rejecting_the_plugin() {
    let fixture = Fixture::new();
    let outside = fixture.source("outside");
    let fields = [
        "commands",
        "agents",
        "outputStyles",
        "hooks",
        "mcpServers",
        "lspServers",
        "settings",
    ];
    for field in fields {
        let root = fixture.claude_plugin(field, &json!({"name": "paths"}), &[]);
        symlink(&outside, root.join("escape")).unwrap();
        for (path, reason) in [
            ("./missing", "does not exist"),
            ("./../outside", "must stay inside"),
            ("/outside", "must start with ./"),
            ("./escape", "leads outside"),
        ] {
            write_json(
                &root.join(".claude-plugin/plugin.json"),
                &json!({"name": "paths", field: path}),
            );
            let manifest = PluginManifest::load(&root, "unused", None).unwrap();
            assert!(manifest.unsupported().is_empty(), "{field}: {path}");
            assert_eq!(manifest.warnings.len(), 1, "{field}: {path}");
            assert!(manifest.warnings[0].starts_with(&format!("{field} path")));
            assert!(manifest.warnings[0].contains(reason));
        }
    }
}

#[test]
fn component_paths_keep_valid_entries_and_inline_configurations() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin(
        "mixed",
        &json!({
            "name": "mixed",
            "commands": {"valid": {"source": "./guide.md"}, "missing": {"source": "./missing.md"}},
            "agents": ["./guide.md", "./missing.md"],
            "outputStyles": "./guide.md",
            "hooks": {"hooks": {}},
            "mcpServers": ["./missing.json", {"server": {"command": "server"}}],
            "lspServers": {"rust": {"command": "rust-analyzer"}},
            "settings": {"model": "default"},
            "userConfig": {"key": {"type": "string"}},
            "channels": [{"server": "server"}],
            "dependencies": ["another-plugin@market"],
        }),
        &[],
    );
    fs::write(root.join("guide.md"), "Guide").unwrap();
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    assert_eq!(
        manifest.unsupported(),
        vec![
            Component::Commands,
            Component::Agents,
            Component::Hooks,
            Component::McpServers,
            Component::LspServers,
            Component::OutputStyles,
            Component::UserConfig,
            Component::Channels,
            Component::Settings,
            Component::Dependencies
        ]
    );
    assert_eq!(manifest.warnings.len(), 3, "{:?}", manifest.warnings);
    assert!(
        manifest
            .warnings
            .iter()
            .any(|warning| warning.contains("commands.missing.source path"))
    );
}

#[test]
fn inline_commands_are_recognized_without_a_source_path() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin(
        "inline",
        &json!({
            "name": "inline",
            "commands": {"hello": {"content": "Say hello", "description": "Greeting"}},
        }),
        &[],
    );
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    assert_eq!(manifest.unsupported(), vec![Component::Commands]);
    assert!(manifest.warnings.is_empty(), "{:?}", manifest.warnings);
}

#[test]
fn invalid_component_paths_are_not_rediscovered_at_default_locations() {
    let fixture = Fixture::new();
    let outside = fixture.source("outside");
    for (field, location) in [
        ("commands", "commands"),
        ("agents", "agents"),
        ("outputStyles", "output-styles"),
        ("hooks", "hooks/hooks.json"),
        ("mcpServers", ".mcp.json"),
        ("lspServers", ".lsp.json"),
        ("settings", "settings.json"),
    ] {
        for (kind, target) in [
            ("escape", outside.clone()),
            ("dangling", outside.join("missing")),
        ] {
            let root = fixture.claude_plugin(
                &format!("{field}-{kind}"),
                &json!({"name": "paths", field: format!("./{location}")}),
                &[],
            );
            fs::create_dir_all(root.join(location).parent().unwrap()).unwrap();
            symlink(target, root.join(location)).unwrap();
            let manifest = PluginManifest::load(&root, "unused", None).unwrap();
            assert!(manifest.unsupported().is_empty(), "{field}: {kind}");
            assert!(
                manifest
                    .warnings
                    .iter()
                    .any(|warning| warning.starts_with(&format!("{field} path"))),
                "{:?}",
                manifest.warnings
            );
        }
    }
}
