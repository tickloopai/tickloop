use std::{collections::BTreeSet, fs, os::unix::fs::symlink, path::PathBuf};

use serde_json::json;
use tick_packages::PackageAuth;
use tick_plugins::{Component, ManifestFormat, PluginHooks, PluginManifest, TickProgram};

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
    assert!(manifest.unsupported().is_empty());
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
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "check"}]}]},
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
            Component::Agents,
            Component::LspServers,
            Component::OutputStyles,
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
    assert!(manifest.unsupported().is_empty());
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

#[test]
fn mcp_entries_merge_default_file_and_manifest_objects_paths_and_arrays() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin(
        "mcp",
        &json!({
            "name":"mcp", "mcpServers":["./extra.json", {"inline":{"command":"inline"}}]
        }),
        &[],
    );
    write_json(
        &root.join(".mcp.json"),
        &json!({"mcpServers":{"default":{"command":"default"}}}),
    );
    write_json(
        &root.join("extra.json"),
        &json!({"extra":{"url":"https://example.com/mcp"}}),
    );
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    assert!(!manifest.unsupported().contains(&Component::McpServers));
    assert_eq!(
        serde_json::to_value(&manifest.mcp_servers).unwrap(),
        json!({
            "default":{"command":"default"}, "extra":{"url":"https://example.com/mcp"}, "inline":{"command":"inline"}
        })
    );
    assert!(manifest.warnings.is_empty(), "{:?}", manifest.warnings);
}

#[test]
fn user_config_declarations_keep_supported_types_and_ignore_unknown_types() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin("configured", &json!({
        "name":"configured", "userConfig": {
            "key":{"type":"string","title":"API key","description":"Your key","sensitive":true,"required":true},
            "count":{"type":"number","default":3},
            "active":{"type":"boolean","default":false},
            "folder":{"type":"directory"}, "document":{"type":"file"},
            "unknown":{"type":"future","default":"do-not-print"}
        }
    }), &[]);
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    assert_eq!(manifest.user_config.len(), 5);
    let key = &manifest.user_config["key"];
    assert!(key.sensitive && key.required);
    assert_eq!(key.title.as_deref(), Some("API key"));
    assert_eq!(key.description.as_deref(), Some("Your key"));
    assert_eq!(manifest.user_config["count"].default, Some(json!(3)));
    assert_eq!(manifest.user_config["active"].default, Some(json!(false)));
    assert!(!manifest.unsupported().contains(&Component::UserConfig));
    assert!(
        manifest
            .warnings
            .iter()
            .any(|warning| warning.contains("unknown"))
    );
    assert!(!format!("{:?}", manifest.warnings).contains("do-not-print"));
}

#[test]
fn hook_declarations_list_the_default_file_once_then_manifest_paths_and_inline_objects() {
    let fixture = Fixture::new();
    let inline = json!({"Stop": [{"hooks": [{"type": "command", "command": "inline"}]}]});
    let root = fixture.claude_plugin(
        "hooked",
        &json!({
            "name": "hooked",
            "hooks": ["./extra/more.json", "./hooks/hooks.json", inline, "./missing.json"],
        }),
        &[],
    );
    write_json(&root.join("hooks/hooks.json"), &json!({"hooks": {}}));
    write_json(&root.join("extra/more.json"), &json!({"hooks": {}}));

    let manifest = PluginManifest::load(&root, "unused", None).unwrap();

    assert_eq!(
        manifest.hooks,
        [
            PluginHooks::File(PathBuf::from("hooks/hooks.json")),
            PluginHooks::File(PathBuf::from("extra/more.json")),
            PluginHooks::Inline(inline),
        ]
    );
    assert_eq!(
        manifest.warnings,
        ["hooks path \"./missing.json\" is ignored: it does not exist"]
    );
}

#[test]
fn a_plugin_without_a_hooks_field_declares_only_its_default_hooks_file() {
    let fixture = Fixture::new();
    let plain = fixture.claude_plugin("plain", &json!({"name": "plain"}), &[]);
    let hooked = fixture.claude_plugin("hooked", &json!({"name": "hooked"}), &[]);
    write_json(&hooked.join("hooks/hooks.json"), &json!({"hooks": {}}));

    assert!(
        PluginManifest::load(&plain, "unused", None)
            .unwrap()
            .hooks
            .is_empty()
    );
    assert_eq!(
        PluginManifest::load(&hooked, "unused", None).unwrap().hooks,
        [PluginHooks::File(PathBuf::from("hooks/hooks.json"))]
    );
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn hooks_are_supported_per_event_type_and_field_with_the_rest_named_in_warnings() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin(
        "guard",
        &json!({
            "name": "guard",
            "hooks": {"Stop": [{"hooks": [
                {"type": "command", "command": "check-done"},
                {"type": "agent", "prompt": "Is it done?"},
            ]}]},
        }),
        &[],
    );
    write_json(
        &root.join("hooks/hooks.json"),
        &json!({"description": "Guards", "hooks": {
            "PreToolUse": [{"matcher": "Bash|Edit", "hooks": [
                {"type": "command", "command": "guard", "timeout": 10, "if": "Bash(rm *)"},
                {"type": "prompt", "prompt": "Is this safe?"},
                {"type": "command", "command": "later", "async": true, "once": true},
                {"type": "command", "command": "review", "asyncRewake": true,
                 "rewakeMessage": "Review:", "rewakeSummary": "Review found issues"},
            ]}],
            "Notification": [{"hooks": [{"type": "command", "command": "notify"}]}],
            "PermissionRequest": [{"hooks": [{"type": "command", "command": "ask"}]}],
            "PostToolUse": [{"hooks": [{"type": "http", "url": "https://example.com"}]}],
        }}),
    );

    let manifest = PluginManifest::load(&root, "unused", None).unwrap();

    assert!(manifest.components.contains(&Component::Hooks));
    assert!(manifest.unsupported().is_empty());
    let support = &manifest.hook_support;
    assert_eq!(support.runnable, 3);
    assert_eq!(support.events, names(&["PreToolUse", "Stop"]));
    assert_eq!(support.types, names(&["command"]));
    assert_eq!(
        support.fields,
        names(&[
            "asyncRewake",
            "command",
            "if",
            "rewakeMessage",
            "rewakeSummary",
            "timeout"
        ])
    );
    assert_eq!(
        support.unsupported_events,
        names(&["Notification", "PermissionRequest"])
    );
    assert_eq!(
        support.unsupported_types,
        names(&["agent", "http", "prompt"])
    );
    assert_eq!(support.unsupported_fields, names(&["once"]));
    assert_eq!(
        manifest.warnings,
        [
            "hook events not supported yet, so their hooks are not used: Notification, \
             PermissionRequest",
            "hook types not supported yet, so those hooks are not used: agent, http, prompt",
            "hook fields not supported yet, so the hooks using them are not used: once",
        ]
    );
}

#[test]
fn hooks_tickloop_can_run_none_of_stay_an_unsupported_component() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin("notify", &json!({"name": "notify"}), &[]);
    write_json(
        &root.join("hooks/hooks.json"),
        &json!({"hooks": {"Notification": [{"hooks": [{"type": "command", "command": "say"}]}]}}),
    );

    let manifest = PluginManifest::load(&root, "unused", None).unwrap();

    assert_eq!(manifest.hook_support.runnable, 0);
    assert_eq!(manifest.unsupported(), vec![Component::Hooks]);
    assert_eq!(
        manifest.warnings,
        ["hook events not supported yet, so their hooks are not used: Notification"]
    );
}

#[test]
fn hooks_that_cannot_be_read_are_ignored_with_a_warning() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin(
        "broken",
        &json!({
            "name": "broken",
            "hooks": ["./extra.json", {"Stop": [{"hooks": [{"type": "command", "command": ""}]}]}],
        }),
        &[],
    );
    fs::create_dir_all(root.join("hooks")).unwrap();
    fs::write(root.join("hooks/hooks.json"), "{not json").unwrap();
    write_json(&root.join("extra.json"), &json!(["not", "an", "object"]));

    let manifest = PluginManifest::load(&root, "unused", None).unwrap();

    assert_eq!(manifest.hook_support.runnable, 0);
    assert!(manifest.unsupported().is_empty());
    assert_eq!(manifest.warnings.len(), 3, "{:?}", manifest.warnings);
    assert!(
        manifest.warnings[0]
            .starts_with("hooks file hooks/hooks.json is ignored: it is not valid JSON"),
        "{:?}",
        manifest.warnings
    );
    assert_eq!(
        manifest.warnings[1..],
        [
            "hooks file extra.json is ignored: it must hold a JSON object",
            "hooks in .claude-plugin/plugin.json that do not follow the Claude Code hooks format \
             are ignored",
        ]
    );
}

#[test]
fn command_selection_names_recursion_metadata_and_inline_content() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin("commands", &json!({"name":"commands"}), &[]);
    fs::create_dir_all(root.join("commands/deep/task")).unwrap();
    fs::write(root.join("commands/check.md"), "check").unwrap();
    fs::write(root.join("commands/deep/task/SKILL.md"), "skill command").unwrap();
    fs::write(root.join("commands/deep/task/ignored.md"), "ignored").unwrap();
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    let discovery = tick_plugins::discover_commands(&root, &manifest);
    assert_eq!(
        discovery
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["check", "deep:task"]
    );
    write_json(
        &root.join(".claude-plugin/plugin.json"),
        &json!({"name":"commands", "commands": {
            "renamed":{"source":"./commands/check.md","description":"override"},
            "inline":{"content":"inline body"}
        }}),
    );
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    let discovery = tick_plugins::discover_commands(&root, &manifest);
    assert_eq!(discovery.commands.len(), 2);
    assert!(
        discovery
            .commands
            .iter()
            .any(|command| command.name == "renamed"
                && command.metadata["description"] == "override")
    );
    assert!(
        !discovery
            .commands
            .iter()
            .any(|command| command.name == "deep:task")
    );
    assert!(
        discovery
            .commands
            .iter()
            .any(|command| command.name == "inline"
                && command.base_directory == root.canonicalize().unwrap())
    );
}

#[test]
fn command_discovery_rejects_escaping_links_and_stops_directory_cycles() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin("commands", &json!({"name":"commands"}), &[]);
    fs::create_dir_all(root.join("commands")).unwrap();
    let outside = fixture.source("outside");
    fs::write(outside.join("secret.md"), "secret").unwrap();
    symlink(outside.join("secret.md"), root.join("commands/escape.md")).unwrap();
    symlink(root.join("commands"), root.join("commands/cycle")).unwrap();
    symlink(root.join("missing.md"), root.join("commands/missing.md")).unwrap();
    fs::write(root.join("commands/good.md"), "good").unwrap();
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    let discovery = tick_plugins::discover_commands(&root, &manifest);
    assert_eq!(discovery.commands.len(), 1);
    assert_eq!(discovery.commands[0].name, "good");
    assert_eq!(discovery.warnings.len(), 2);
}

#[test]
fn explicit_command_files_keep_their_stem_and_invalid_arrays_are_not_loaded() {
    let fixture = Fixture::new();
    let root = fixture.claude_plugin(
        "commands",
        &json!({"name":"commands", "commands":"./custom/SKILL.md"}),
        &[],
    );
    fs::create_dir_all(root.join("custom")).unwrap();
    fs::write(root.join("custom/SKILL.md"), "Guide").unwrap();
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    assert_eq!(
        tick_plugins::discover_commands(&root, &manifest).commands[0].name,
        "SKILL"
    );
    write_json(
        &root.join(".claude-plugin/plugin.json"),
        &json!({"name":"commands", "commands":["./custom/SKILL.md",7]}),
    );
    let manifest = PluginManifest::load(&root, "unused", None).unwrap();
    assert!(
        tick_plugins::discover_commands(&root, &manifest)
            .commands
            .is_empty()
    );
    assert!(
        manifest
            .warnings
            .iter()
            .any(|warning| warning.contains("commands is ignored"))
    );
}
