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

fn command_registry(body: &str) -> SkillRegistry {
    let mut registry = SkillRegistry::default();
    registry.register_command(
        tickloop_harness_policy::skills::CommandDocument {
            name: "check".into(),
            markdown: body.into(),
            source: SkillSource::Plugin {
                plugin: "demo".into(),
                path: "/plugin/commands/check.md".into(),
            },
            base_directory: "/plugin/commands".into(),
            metadata: Default::default(),
            public_config: Default::default(),
        },
        &[],
    );
    registry
}

#[test]
fn command_arguments_are_zero_based_quoted_nonrecursive_and_escaped() {
    let registry = command_registry(
        "---\ndescription: Test\narguments: [target, other]\n---\n$0|$1|$ARGUMENTS[0]|$ARGUMENTS[9]|$9|\\$0|$target|$other|$ARGUMENTS",
    );
    let expansion = registry
        .expand("demo:check", "'two words' '$0'", SkillCaller::Model)
        .unwrap();
    assert!(
        expansion
            .content
            .ends_with("two words|$0|two words|$ARGUMENTS[9]|$9|$0|two words|$0|'two words' '$0'"),
        "{}",
        expansion.content
    );
    let registry = command_registry("Only $9");
    assert!(
        registry
            .expand("demo:check", "value", SkillCaller::User)
            .unwrap()
            .content
            .ends_with("Only $9\n\nARGUMENTS: value")
    );
}

#[test]
fn command_shell_plans_cannot_activate_before_execution_or_reinterpret_output() {
    let registry = command_registry(
        "---\ndescription: Test\nallowed-tools: {anything: true}\nmodel: opus\n---\nBefore !`printf '%s' $0`\n```!\nprintf second\n```\nAfter $ARGUMENTS",
    );
    assert!(
        registry
            .warnings()
            .iter()
            .any(|warning| warning.contains("model"))
    );
    let mut expansion = registry
        .expand("demo:check", "hello", SkillCaller::Model)
        .unwrap();
    assert_eq!(
        expansion
            .shell_commands
            .iter()
            .map(|shell| shell.command.as_str())
            .collect::<Vec<_>>(),
        ["printf '%s' hello", "printf second"]
    );
    assert!(tickloop_harness_protocol::InvokedSkill::try_from(expansion.clone()).is_err());
    assert!(expansion.complete_shells(&["incomplete".into()]).is_err());
    expansion
        .complete_shells(&["output !`do-not-run`".into(), "second".into()])
        .unwrap();
    assert!(
        expansion
            .content
            .contains("Before output !`do-not-run`\nsecond\nAfter hello")
    );
    assert!(tickloop_harness_protocol::InvokedSkill::try_from(expansion).is_ok());
    let expansion = command_registry("$ARGUMENTS")
        .expand("demo:check", "!`injected`", SkillCaller::User)
        .unwrap();
    assert!(expansion.shell_commands.is_empty());
}

#[test]
fn command_description_is_discovery_metadata_and_permissions_are_not_inherited() {
    let registry = command_registry(
        "---\nallowed-tools: [Bash]\nmodel: inherit\ncontext: fork\n---\nPlain command",
    );
    assert!(!registry.model_listing().contains("demo:check"));
    let expansion = registry
        .expand("demo:check", "", SkillCaller::Model)
        .unwrap();
    assert!(expansion.allowed_tools.is_empty());
    assert!(
        registry
            .warnings()
            .iter()
            .any(|warning| warning.contains("model"))
    );
    assert!(
        registry
            .warnings()
            .iter()
            .any(|warning| warning.contains("context"))
    );
    let registry = command_registry(
        "---\ndescription: No model\ndisable-model-invocation: true\n---\nDo this",
    );
    assert!(
        registry
            .expand("demo:check", "", SkillCaller::Model)
            .is_err()
    );
    assert!(registry.expand("demo:check", "", SkillCaller::User).is_ok());
}

#[test]
fn command_examples_inside_inline_code_and_base_directory_never_execute() {
    let registry =
        command_registry("Text `Example !`printf forbidden` tail`\nActual !`printf allowed`");
    let expansion = registry
        .expand("demo:check", "", SkillCaller::User)
        .unwrap();
    assert_eq!(
        expansion
            .shell_commands
            .iter()
            .map(|shell| shell.command.as_str())
            .collect::<Vec<_>>(),
        ["printf allowed"]
    );
    let registry = command_registry("---\nallowed-tools: { [Bash, Write]: anything }\n---\nGuide");
    assert!(
        registry
            .expand("demo:check", "", SkillCaller::User)
            .unwrap()
            .allowed_tools
            .is_empty()
    );
}

#[test]
fn command_parameters_do_not_rewrite_shell_variables_or_partial_named_placeholders() {
    let registry = command_registry(
        "---\narguments: [foo, foo-bar]\n---\n$foo $foo-bar $foo_suffix $foo[0] \\$HOME $HOME \\$foo",
    );
    let expansion = registry
        .expand("demo:check", "one two", SkillCaller::User)
        .unwrap();
    assert!(
        expansion
            .content
            .ends_with("one two $foo_suffix $foo[0] \\$HOME $HOME $foo"),
        "{}",
        expansion.content
    );
}

#[test]
fn command_public_configuration_cannot_introduce_new_shell_blocks() {
    use tickloop_harness_policy::skills::CommandDocument;
    let mut registry = SkillRegistry::default();
    registry.register_command(
        CommandDocument {
            name: "config".into(),
            markdown: "---\n---\n${user_config.note}".into(),
            source: SkillSource::Plugin {
                plugin: "demo".into(),
                path: "/plugin/commands/config.md".into(),
            },
            base_directory: "/plugin/commands".into(),
            metadata: Default::default(),
            public_config: [("note".into(), "!`printf injected` $0".into())].into(),
        },
        &[],
    );
    let expansion = registry
        .expand("demo:config", "value", SkillCaller::User)
        .unwrap();
    assert!(expansion.shell_commands.is_empty());
    assert!(expansion.content.contains("$0"));
}

#[test]
fn invalid_named_parameters_are_filtered_before_assigning_positions() {
    let registry = command_registry("---\narguments: ['', ' ', '0', target]\n---\n$target");
    assert!(
        registry
            .expand("demo:check", "first second", SkillCaller::User)
            .unwrap()
            .content
            .ends_with("first")
    );
}

#[test]
fn empty_command_metadata_does_not_hide_a_file_description() {
    let mut registry = SkillRegistry::default();
    registry.register_command(
        tickloop_harness_policy::skills::CommandDocument {
            name: "check".into(),
            markdown: "---\ndescription: File description\n---\nGuide".into(),
            source: SkillSource::Plugin {
                plugin: "demo".into(),
                path: "/plugin/check.md".into(),
            },
            base_directory: "/plugin".into(),
            metadata: serde_json::json!({"description":""})
                .as_object()
                .unwrap()
                .clone(),
            public_config: Default::default(),
        },
        &[],
    );
    assert!(registry.model_listing().contains("File description"));
}

#[test]
fn command_argument_hints_accept_the_arrays_used_by_official_plugins() {
    let registry = command_registry(
        "---\ndescription: SDK application\nargument-hint: [project-name]\n---\nCreate project",
    );
    assert_eq!(
        registry
            .definitions()
            .next()
            .unwrap()
            .argument_hint
            .as_deref(),
        Some("project-name")
    );
    assert!(
        registry
            .expand("demo:check", "demo", SkillCaller::User)
            .is_ok()
    );
}
