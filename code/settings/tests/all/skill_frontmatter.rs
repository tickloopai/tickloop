use tickloop_code_settings::skills::{SkillCaller, SkillDiscovery, SkillRegistry};

#[test]
fn discovery_folds_yaml_block_descriptions_into_catalog_text() {
    let directory = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover_with_builtins(
        directory.path(),
        None,
        [
            "---\nname: folded\ndescription: >\n  Find the schema\n  and explain it.\n---\nKeep this body.\n",
        ],
    );
    assert!(registry.warnings().is_empty(), "{:?}", registry.warnings());
    let definition = registry.definitions().next().unwrap();
    assert_eq!(definition.description, "Find the schema and explain it.");
    assert_eq!(
        registry
            .expand("folded", "", SkillCaller::Model)
            .unwrap()
            .content,
        "Keep this body."
    );
}

fn assert_discovered_text(frontmatter: &str, description: &str) {
    for origin in ["built-in", "user", "project", "plugin"] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let config = directory.path().join("config");
        let markdown = format!("---\n{frontmatter}\n---\nKeep this body.\n");
        let registry = if origin == "built-in" {
            SkillRegistry::discover_with_builtins(directory.path(), None, [markdown.as_str()])
        } else {
            let skill_directory = match origin {
                "user" => config.join("skills/guide"),
                "project" => directory.path().join(".tickloop/skills/guide"),
                "plugin" => directory.path().join("source/skills/guide"),
                _ => unreachable!(),
            };
            std::fs::create_dir_all(&skill_directory).unwrap();
            std::fs::write(skill_directory.join("SKILL.md"), &markdown).unwrap();
            if origin == "plugin" {
                let source = directory.path().join("source");
                std::fs::create_dir(source.join(".claude-plugin")).unwrap();
                std::fs::write(
                    source.join(".claude-plugin/plugin.json"),
                    r#"{"name":"notes"}"#,
                )
                .unwrap();
                tick_plugins::PluginStore::in_config_root(&config)
                    .install_path(&source)
                    .unwrap();
            }
            SkillRegistry::discover_with_builtins(directory.path(), Some(&config), [])
        };
        assert!(
            registry.warnings().is_empty(),
            "{origin}: {:?}",
            registry.warnings()
        );
        let definitions: Vec<_> = registry.definitions().collect();
        assert_eq!(definitions.len(), 1, "{origin}");
        let name = if origin == "plugin" {
            "notes:guide"
        } else {
            "guide"
        };
        assert_eq!(definitions[0].name, name, "{origin}");
        assert_eq!(definitions[0].description, description, "{origin}");
        assert!(
            registry
                .expand(name, "", SkillCaller::Model)
                .unwrap()
                .content
                .ends_with("Keep this body.")
        );
    }
}

#[test]
fn discovery_normalizes_literal_blocks_and_every_chomping_mode() {
    for style in [">", ">-", ">+", "|", "|-", "|+"] {
        assert_discovered_text(
            &format!(
                "name: guide\ndescription: {style}\n  First paragraph.\n\n  Second paragraph.\n\n"
            ),
            "First paragraph. Second paragraph.",
        );
    }
}

#[test]
fn discovery_decodes_single_quoted_yaml_strings() {
    assert_discovered_text(
        "name: 'guide'\ndescription: 'It''s a guide: use #tags and \\paths.'",
        "It's a guide: use #tags and \\paths.",
    );
}

#[test]
fn discovery_decodes_double_quoted_yaml_escapes() {
    assert_discovered_text(
        r#"name: "\x67uide"
description: "Say \"hello\"\nthen\tuse\x20YAML\Nescapes.\u0020\\path""#,
        "Say \"hello\" then use YAML escapes. \\path",
    );
}

#[test]
fn discovery_reads_multiline_plain_and_quoted_scalars() {
    for description in [
        "Explain the schema\n  and its fields.",
        "\n  Explain the schema\n  and its fields.",
        "'Explain the schema\n  and its fields.'",
        "\"Explain the schema\n  and its fields.\"",
    ] {
        assert_discovered_text(
            &format!("name: guide\ndescription: {description}"),
            "Explain the schema and its fields.",
        );
    }
}

#[test]
fn discovery_ignores_unknown_yaml_metadata_without_treating_nested_fields_as_controls() {
    assert_discovered_text(
        "name: guide\ndescription: Read the guide. # a YAML comment\nmetadata:\n  name: not-the-skill\n  disable-model-invocation: true\n  allowed-tools: [Bash]\n  tags: [one, two]\n  count: 42\n  optional: null\nlicense: [MIT]\nhooks: false",
        "Read the guide.",
    );
}

#[test]
fn discovery_warns_and_skips_invalid_yaml_or_invalid_required_metadata() {
    for (frontmatter, reason) in [
        ("name: guide\ndescription: [unclosed", "YAML"),
        (
            "name: guide\ndescription: fine\nmetadata: [unclosed",
            "YAML",
        ),
        ("name: guide\nname: other\ndescription: fine", "duplicate"),
        (
            "name: guide\ndescription: one\ndescription: two",
            "duplicate",
        ),
        ("name: 42\ndescription: fine", "string"),
        ("name: guide\ndescription: [one, two]", "string"),
        ("name: guide\ndescription: null", "string"),
        ("name: Guide\ndescription: fine", "lowercase"),
        ("name: guide\ndescription: |\n  \n", "empty"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".tickloop/skills/broken/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        std::fs::write(&path, format!("---\n{frontmatter}\n---\nBody")).unwrap();
        let registry = SkillRegistry::discover_with_builtins(directory.path(), None, []);
        assert_eq!(registry.definitions().count(), 0, "{frontmatter}");
        assert_eq!(registry.warnings().len(), 1, "{frontmatter}");
        let warning = &registry.warnings()[0];
        assert!(warning.contains("broken/SKILL.md is ignored"), "{warning}");
        assert!(warning.contains(reason), "{warning}");
    }
}

#[test]
fn discovery_rejects_non_boolean_invocation_controls() {
    for field in ["user-invocable", "disable-model-invocation"] {
        for value in ["yes", "no", "null", "42", "[]", "\"false\""] {
            let directory = tempfile::tempdir().unwrap();
            let markdown =
                format!("---\nname: guide\ndescription: Fine\n{field}: {value}\n---\nBody");
            let registry =
                SkillRegistry::discover_with_builtins(directory.path(), None, [markdown.as_str()]);
            assert_eq!(registry.definitions().count(), 0, "{field}: {value}");
            assert_eq!(registry.warnings().len(), 1);
        }
    }
}

#[test]
fn discovery_accepts_yaml_frontmatter_with_windows_line_endings_and_bom() {
    let directory = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover_with_builtins(
        directory.path(),
        None,
        [
            "\u{feff}---\r\nname: guide\r\ndescription: |\r\n  Read the guide.\r\n---\r\nBody\r\nnext line\r\n",
        ],
    );
    assert!(registry.warnings().is_empty(), "{:?}", registry.warnings());
    assert_eq!(
        registry.definitions().next().unwrap().description,
        "Read the guide."
    );
    assert_eq!(
        registry
            .expand("guide", "", SkillCaller::Model)
            .unwrap()
            .content,
        "Body\r\nnext line"
    );
}

#[test]
fn yaml_metadata_preserves_invocation_restrictions_and_never_grants_tools() {
    use tickloop_code_settings::skills::SkillError;
    let directory = tempfile::tempdir().unwrap();
    for (field, caller, expected) in [
        (
            "disable-model-invocation: true",
            SkillCaller::Model,
            SkillError::ModelInvocationDisabled("guide".into()),
        ),
        (
            "user-invocable: false",
            SkillCaller::User,
            SkillError::UserInvocationDisabled("guide".into()),
        ),
        (
            "context: fork",
            SkillCaller::Model,
            SkillError::ForkUnsupported("guide".into()),
        ),
        (
            "allowed-tools:\n  - Bash\n  - Write",
            SkillCaller::Model,
            SkillError::AllowedToolsUnsupported("guide".into()),
        ),
        (
            "allowed-tools: [Bash, Write]",
            SkillCaller::User,
            SkillError::AllowedToolsUnsupported("guide".into()),
        ),
        (
            "allowed-tools: Bash, Write",
            SkillCaller::Model,
            SkillError::AllowedToolsUnsupported("guide".into()),
        ),
    ] {
        let markdown =
            format!("---\nname: guide\ndescription: >-\n  Read the guide.\n{field}\n---\nBody");
        let registry =
            SkillRegistry::discover_with_builtins(directory.path(), None, [markdown.as_str()]);
        assert!(registry.warnings().is_empty(), "{:?}", registry.warnings());
        assert_eq!(
            registry
                .expand("guide", "", caller)
                .and_then(tickloop_harness::protocol::InvokedSkill::try_from)
                .unwrap_err(),
            expected,
            "{field}"
        );
    }
}
