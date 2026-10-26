use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_code_settings::skills::SkillDiscovery;

use serde_json::json;
use tick_packages::MANIFEST_FILE_NAME;
use tick_plugins::PluginStore;
use tickloop_harness::model::ModelRequest;
use tickloop_harness::policy::skills::{SkillCaller, SkillError, SkillRegistry};
use tickloop_harness::protocol::{
    Content, EventKind, ExecutionMode, LoopState, Receipt, SkillSource,
};
use tickloop_harness::runtime::Agent;

use crate::support::{assert_extends, guidance_in_effect, mentions};

use tickloop_harness::test_support::routed::{RoutedModel, has_tool_result, state};
use tickloop_harness::test_support::{
    ScriptedModel, auto_classification, call, config, next_event, text,
};

fn write_skill(directory: &std::path::Path, name: &str, frontmatter: &str, body: &str) {
    let skill_directory = directory.join(name);
    fs::create_dir_all(&skill_directory).unwrap();
    fs::write(
        skill_directory.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n{body}\n"),
    )
    .unwrap();
}

#[test]
fn discovers_builtin_user_and_nearest_project_skills_in_stable_order() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    let workdir = root.path().join("nested/project");
    fs::create_dir_all(&workdir).unwrap();
    let config_root = root.path().join("config");
    write_skill(
        &config_root.join("skills"),
        "personal",
        "name: personal\ndescription: Personal workflow",
        "Personal body",
    );
    write_skill(
        &root.path().join(".tickloop/skills"),
        "root-skill",
        "name: root-skill\ndescription: Root workflow",
        "Root body",
    );
    write_skill(
        &workdir.join(".tickloop/skills"),
        "nearest",
        "name: nearest\ndescription: Nearest workflow",
        "Nearest body",
    );

    let registry = SkillRegistry::discover(&workdir, Some(&config_root));
    let names = registry
        .definitions()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        [
            "loop",
            "nearest",
            "personal",
            "root-skill",
            "slides",
            "tickloop"
        ]
    );
    assert!(registry.warnings().is_empty());
    assert!(matches!(
        registry
            .definitions()
            .find(|skill| skill.name == "nearest")
            .unwrap()
            .source,
        SkillSource::Project { .. }
    ));
}

#[test]
fn builtin_name_is_reserved_and_a_broken_skill_only_adds_a_warning() {
    let directory = tempfile::tempdir().unwrap();
    let user_skills = directory.path().join("skills");
    write_skill(
        &user_skills,
        "tickloop",
        "name: tickloop\ndescription: Replacement",
        "Replacement body",
    );
    write_skill(&user_skills, "broken", "name: Broken", "Broken body");

    let registry = SkillRegistry::discover(directory.path(), Some(directory.path()));
    let expansion = registry.expand("tickloop", "", SkillCaller::User).unwrap();

    assert!(matches!(expansion.source, SkillSource::BuiltIn));
    assert!(expansion.content.contains("Tickloop Ticks"));
    assert_eq!(registry.warnings().len(), 2);
}

#[test]
fn listing_discloses_only_model_invocable_skill_metadata() {
    let directory = tempfile::tempdir().unwrap();
    write_skill(
        &directory.path().join("skills"),
        "manual",
        "name: manual\ndescription: Run manually\ndisable-model-invocation: true",
        "Manual body",
    );
    let registry = SkillRegistry::discover_with_builtins(
        directory.path(),
        Some(directory.path()),
        std::iter::empty(),
    );

    assert_eq!(registry.model_listing(), "");
    assert!(registry.tool_definition().is_none());
    assert_eq!(
        registry
            .expand("manual", "", SkillCaller::Model)
            .unwrap_err(),
        SkillError::ModelInvocationDisabled("manual".into())
    );
    assert!(
        registry
            .expand("manual", "", SkillCaller::User)
            .unwrap()
            .content
            .ends_with("Manual body")
    );
}

#[test]
fn expansion_applies_arguments_allowed_tools_and_invocation_gates() {
    let directory = tempfile::tempdir().unwrap();
    let user_skills = directory.path().join("skills");
    write_skill(
        &user_skills,
        "analyze",
        "name: analyze\ndescription: Analyze a target\nallowed-tools: [Read, Grep]",
        "Analyze $ARGUMENTS carefully.",
    );
    write_skill(
        &user_skills,
        "model-only",
        "name: model-only\ndescription: Model only\nuser-invocable: false",
        "Model body",
    );
    write_skill(
        &user_skills,
        "forked",
        "name: forked\ndescription: Forked\ncontext: fork",
        "Fork body",
    );
    let registry = SkillRegistry::discover_with_builtins(
        directory.path(),
        Some(directory.path()),
        std::iter::empty(),
    );

    let expansion = registry
        .expand("/analyze", "report.md", SkillCaller::Model)
        .unwrap();
    assert!(expansion.content.contains("Base directory for this skill:"));
    assert!(expansion.content.ends_with("Analyze report.md carefully."));
    assert_eq!(expansion.allowed_tools, ["Read", "Grep"]);
    assert_eq!(
        registry
            .expand("model-only", "", SkillCaller::User)
            .unwrap_err(),
        SkillError::UserInvocationDisabled("model-only".into())
    );
    assert_eq!(
        registry
            .expand("forked", "", SkillCaller::Model)
            .unwrap_err(),
        SkillError::ForkUnsupported("forked".into())
    );
}

#[test]
fn model_listing_retains_complete_names_when_descriptions_exceed_the_budget() {
    let registry = SkillRegistry::discover_with_builtins(
        std::path::Path::new("."),
        None,
        [
            "---\nname: first\ndescription: One\n---\nFirst body",
            "---\nname: second\ndescription: Two\n---\nSecond body",
        ],
    );

    let listing = registry.model_listing_with_budget(13);
    assert!(listing.lines().any(|line| line.starts_with("- first")));
    assert!(listing.lines().any(|line| line.starts_with("- second")));
}

async fn wait_for_completion(agent: &Agent, chat: uuid::Uuid) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = agent.snapshot(tickloop_ids::Id::from(chat)).await.unwrap();
            if snapshot.chat_loop.state == LoopState::Idle {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn model_sees_metadata_then_receives_the_body_only_after_calling_skill() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("skill-1", "Skill", json!({"skill": "tickloop"})),
        text("ready"),
    ]));
    let agent = agent_with_project_skills(directory.path(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .submit(chat, "Arrange my Workspace".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat.uuid()).await;

    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let skill_tool = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "Skill")
            .unwrap();
        assert!(skill_tool.description.contains("tickloop"));
        let first = serde_json::to_string(&requests[0].messages).unwrap();
        assert!(!first.contains(
            "Use the installed host tools as the authority for targets and capabilities"
        ));
        let second = serde_json::to_string(&requests[1].messages).unwrap();
        assert!(second.contains(
            "Use the installed host tools as the authority for targets and capabilities"
        ));
        assert!(second.contains("Launching skill: tickloop"));
        let guidance = requests[1].messages.iter().find(|message| message.content.iter().any(|content| matches!(content, Content::Text { text } if text.contains("Use the installed host tools as the authority for targets and capabilities")))).unwrap();
        assert_eq!(guidance.role, tickloop_harness::protocol::Role::User);
        assert!(
            serde_json::to_string(guidance)
                .unwrap()
                .contains("not a user request")
        );
    }

    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.invoked_skills.len(), 1);
    assert_eq!(snapshot.invoked_skills[0].name, "tickloop");
}

#[tokio::test]
async fn loaded_skill_guidance_follows_its_call_once_and_keeps_every_request_a_prefix() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    write_skill(
        &directory.path().join(".tickloop/skills"),
        "analyze",
        "name: analyze\ndescription: Analyze a target",
        "Analyze $ARGUMENTS carefully.",
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "load",
            "Skill",
            json!({"skill":"analyze","args":"first.md"}),
        ),
        text("Analyzed."),
        text("Still guided."),
    ]));
    let agent = agent_with_project_skills(directory.path(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, "Analyze the target".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat.uuid()).await;
    agent
        .submit(chat, "Analyze the next one".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while model.requests.lock().unwrap().len() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let guidance = "Analyze first.md carefully.";
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(mentions(&requests[0], guidance), 0);
        assert_extends(&requests[0], &requests[1]);
        assert_extends(&requests[1], &requests[2]);
        assert_eq!(mentions(&requests[1], guidance), 1);
        assert_eq!(mentions(&requests[2], guidance), 1);
        let position = |predicate: &dyn Fn(&Content) -> bool| {
            requests[1]
                .messages
                .iter()
                .position(|message| message.content.iter().any(predicate))
                .unwrap()
        };
        let result = position(
            &|block| matches!(block, Content::ToolResult { tool_use_id, .. } if tool_use_id == "load"),
        );
        let note =
            position(&|block| matches!(block, Content::Text { text } if text.contains(guidance)));
        assert!(note > result, "the guidance follows the Skill call");
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.host_notes.keys().any(|index| {
        serde_json::to_string(&snapshot.messages[*index])
            .unwrap()
            .contains(guidance)
    }));
}

#[tokio::test]
async fn model_passes_skill_arguments_through_the_args_parameter() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    write_skill(
        &directory.path().join(".tickloop/skills"),
        "analyze",
        "name: analyze\ndescription: Analyze a target",
        "Analyze $ARGUMENTS carefully.",
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "old-arguments",
            "Skill",
            json!({"skill": "analyze", "arguments": "notes.md"}),
        ),
        call(
            "with-args",
            "Skill",
            json!({"skill": "analyze", "args": "report.md"}),
        ),
        text("ready"),
    ]));
    let agent = agent_with_project_skills(directory.path(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .submit(chat, "Analyze the report".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat.uuid()).await;

    let requests = model.requests.lock().unwrap();
    let skill_tool = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "Skill")
        .unwrap();
    assert_eq!(
        skill_tool.input_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["args", "skill"]
    );
    assert_eq!(skill_tool.input_schema["required"], json!(["skill"]));
    assert_eq!(
        skill_tool.input_schema["properties"]["args"]["type"],
        "string"
    );
    let results = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), (content.text(), *is_error))),
            _ => None,
        })
        .collect::<std::collections::HashMap<_, _>>();
    let (old_arguments, old_arguments_failed) = &results["old-arguments"];
    assert!(*old_arguments_failed, "{old_arguments}");
    assert!(old_arguments.contains("arguments"), "{old_arguments}");
    assert_eq!(
        results["with-args"],
        ("Launching skill: analyze [Project]".into(), false)
    );
    let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
    assert!(last.contains("Analyze report.md carefully."));
    assert!(!last.contains("Analyze notes.md carefully."));
}

#[tokio::test]
async fn user_invocation_uses_the_same_expansion_and_records_the_slash_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("ready")]));
    let agent = agent_with_project_skills(directory.path(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .invoke_skill(chat, "/tickloop", "show the current Workspace")
        .await
        .unwrap();
    wait_for_completion(&agent, chat.uuid()).await;

    let requests = model.requests.lock().unwrap();
    let messages = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(
        messages
            .contains("Use the installed host tools as the authority for targets and capabilities")
    );
    assert!(messages.contains("/tickloop show the current Workspace"));
    let [.., prompt, guidance] = requests[0].messages.as_slice() else {
        panic!("the request carries the prompt and the guidance it invoked");
    };
    assert!(matches!(
        prompt.content.as_slice(),
        [Content::Text { text }] if text == "/tickloop show the current Workspace"
    ));
    assert!(matches!(
        guidance.content.as_slice(),
        [Content::Text { text }] if text.starts_with("Reference guidance from tickloop")
    ));
}

#[tokio::test]
async fn invoked_skill_content_survives_compaction_and_transcript_restore() {
    let directory = tempfile::tempdir().unwrap();
    let state_root = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![
        text("ready"),
        text("Conversation summary"),
    ]));
    let agent = Agent::open(state_root.clone(), config(), model).unwrap();
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent.invoke_skill(chat, "tickloop", "").await.unwrap();
    wait_for_completion(&agent, chat.uuid()).await;
    agent.compact(chat).await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let restored = Agent::open(
        state_root,
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();

    assert_eq!(snapshot.invoked_skills.len(), 1);
    assert_eq!(snapshot.invoked_skills[0].name, "tickloop");
    assert!(
        snapshot.invoked_skills[0]
            .content
            .contains("Use the installed host tools as the authority for targets and capabilities")
    );
}

#[tokio::test]
async fn local_skill_arguments_are_reexpanded_and_identical_repeats_are_elided() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    write_skill(
        &directory.path().join(".tickloop/skills"),
        "analyze",
        "name: analyze\ndescription: Analyze a target",
        "Analyze $ARGUMENTS carefully.",
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "first",
            "Skill",
            json!({"skill":"analyze","args":"first.md"}),
        ),
        call(
            "same",
            "Skill",
            json!({"skill":"analyze","args":"first.md"}),
        ),
        call(
            "changed",
            "Skill",
            json!({"skill":"analyze","args":"second.md"}),
        ),
        text("ready"),
    ]));
    let agent = agent_with_project_skills(directory.path(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, "Analyze these targets".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat.uuid()).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let requests = model.requests.lock().unwrap();
    assert!(
        serde_json::to_string(&requests[2].messages)
            .unwrap()
            .contains("Already loaded analyze")
    );
    let guidance = requests[3].messages.iter().find(|message| message.content.iter().any(|block| matches!(block, Content::Text { text } if text.contains("Analyze second.md carefully.")))).unwrap();
    assert_eq!(guidance.role, tickloop_harness::protocol::Role::User);
    let recorded = snapshot
        .messages
        .iter()
        .position(|message| {
            serde_json::to_string(message)
                .unwrap()
                .contains("Analyze second.md carefully.")
        })
        .unwrap();
    assert!(
        snapshot.host_notes.contains_key(&recorded),
        "the guidance is a host note, not a user prompt"
    );
    assert!(!snapshot.prompt_execution_modes.contains_key(&recorded));
}

#[tokio::test]
async fn compacted_skill_guidance_is_restored_with_per_skill_and_total_budgets() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    for index in 0..6 {
        let name = format!("guide-{index}");
        write_skill(
            &directory.path().join(".tickloop/skills"),
            &name,
            &format!("name: {name}\ndescription: Restore guide {index}"),
            &format!("RESTORE_GUIDE_{index} {}", "x".repeat(24_000)),
        );
    }
    let mut outputs = (0..7).map(|_| text("ready")).collect::<Vec<_>>();
    outputs.extend([text("A compact summary"), text("continued")]);
    let model = Arc::new(ScriptedModel::new(outputs));
    let agent = agent_with_project_skills(directory.path(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    for index in 0..6 {
        agent
            .invoke_skill(chat, &format!("guide-{index}"), "")
            .await
            .unwrap();
        wait_for_completion(&agent, chat.uuid()).await;
    }
    agent.invoke_skill(chat, "guide-0", "").await.unwrap();
    wait_for_completion(&agent, chat.uuid()).await;
    agent.compact(chat).await.unwrap();
    agent
        .submit(chat, "Continue with these guides".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat.uuid()).await;
    let requests = model.requests.lock().unwrap();
    let restored = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::Text { text } if text.contains("RESTORE_GUIDE_") => {
                Some(text.split_once("\n\n").unwrap().1)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(restored.len(), 5);
    assert!(restored.iter().any(|body| body.contains("RESTORE_GUIDE_0")));
    assert!(!restored.iter().any(|body| body.contains("RESTORE_GUIDE_1")));
    assert!(restored.iter().all(|body| body.len() <= 20_000));
    assert!(restored.iter().map(|body| body.len()).sum::<usize>() <= 100_000);
    assert!(restored.iter().any(|body| body.contains("truncated")));
}

/// Installs version `version` of the Tick package `name` into `packages`, as `/ticks` or
/// `tickloop tick` would from another process, with a `usage` Skill described as `description`.
fn install_package(
    directory: &Path,
    packages: &PluginStore,
    name: &str,
    version: &str,
    description: &str,
) {
    let source = directory.join("sources").join(name).join(version);
    fs::create_dir_all(source.join("bin")).unwrap();
    fs::write(
        source.join(MANIFEST_FILE_NAME),
        json!({
            "name": name,
            "version": version,
            "description": format!("The {name} package"),
            "executable": format!("bin/tick-{name}"),
            "protocol": true,
        })
        .to_string(),
    )
    .unwrap();
    let program = source.join("bin").join(format!("tick-{name}"));
    fs::write(&program, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    write_skill(
        &source.join("skills"),
        "usage",
        &format!("name: usage\ndescription: {description}"),
        &format!("Run tick-{name} {version}."),
    );
    packages.install_path(&source).unwrap();
}

/// Waits for the run `receipt` started to end.
async fn finish(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_harness::protocol::Event>,
    receipt: &Receipt,
) {
    loop {
        let event = next_event(events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => return,
            EventKind::Failed(error) if event.run_id == receipt.run_id => panic!("{error}"),
            _ => {}
        }
    }
}

/// The Skill tool's description the model saw in request `index`.
fn skill_listing(model: &ScriptedModel, index: usize) -> String {
    model.requests.lock().unwrap()[index]
        .tools
        .iter()
        .find(|tool| tool.name == "Skill")
        .map(|tool| tool.description.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_package_installed_mid_chat_reaches_its_next_request_and_leaves_with_its_uninstall() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_root = root.join("config");
    let packages = PluginStore::in_config_root(&config_root);
    let workdir = root.join("work");
    fs::create_dir(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("No package yet."),
        call("usage", "Skill", json!({"skill": "quotes:usage"})),
        text("Loaded."),
        text("Read it."),
        text("Gone."),
    ]));
    let agent = Agent::open(config_root.clone(), config(), model.clone()).unwrap();
    agent
        .trust_working_directory(workdir.clone())
        .await
        .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();

    let receipt = agent.submit(chat, "Quotes?".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    assert!(!skill_listing(&model, 0).contains("quotes:usage"));

    install_package(&root, &packages, "quotes", "1.0.0", "Read quotes");
    let receipt = agent.submit(chat, "Quotes now?".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    assert!(
        skill_listing(&model, 1).contains("- quotes:usage [Plugin · quotes]: Read quotes"),
        "{}",
        skill_listing(&model, 1)
    );
    let loaded = serde_json::to_string(&model.requests.lock().unwrap()[2].messages).unwrap();
    assert!(loaded.contains("Launching skill: quotes:usage"), "{loaded}");
    assert!(loaded.contains("Run tick-quotes 1.0.0."), "{loaded}");

    let receipt = agent.invoke_skill(chat, "/quotes:usage", "").await.unwrap();
    finish(&mut events, &receipt).await;
    let invoked = serde_json::to_string(&model.requests.lock().unwrap()[3].messages).unwrap();
    assert!(invoked.contains("/quotes:usage"), "{invoked}");

    packages.uninstall("quotes", false).unwrap();
    assert!(!agent.has_skill(chat, "quotes:usage").await.unwrap());
    assert!(agent.invoke_skill(chat, "/quotes:usage", "").await.is_err());
    let receipt = agent.submit(chat, "And now?".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    assert!(!skill_listing(&model, 4).contains("quotes:usage"));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_plugin_skill_with_allowed_tools_loads_but_its_tools_still_ask_first() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_root = root.join("config");
    let workdir = root.join("work");
    fs::create_dir(&workdir).unwrap();
    let source = root.join("sources/notes");
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    fs::write(
        source.join(".claude-plugin/plugin.json"),
        json!({"name": "notes"}).to_string(),
    )
    .unwrap();
    write_skill(
        &source.join("skills"),
        "mark",
        "name: mark\ndescription: Marks the folder\nallowed-tools: [Write]",
        "Create the marker with Write.",
    );
    tick_plugins::PluginStore::in_config_root(&config_root)
        .install_path(&source)
        .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("load", "Skill", json!({"skill": "notes:mark"})),
        call(
            "write",
            "Write",
            json!({"file_path": root.join("marker"), "content": "marked"}),
        ),
    ]));
    let agent = Agent::open(config_root.clone(), config(), model.clone()).unwrap();
    agent
        .trust_working_directory(workdir.clone())
        .await
        .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();

    agent.submit(chat, "Mark the folder".into()).await.unwrap();
    let mut asked = false;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                assert!(!asked, "asked twice");
                asked = true;
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "load" => {
                assert!(!result.is_error, "{}", result.content);
            }
            EventKind::Interrupted => break,
            EventKind::Completed { .. } => panic!("the denied write did not stop the turn"),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }

    assert!(asked, "the Skill's allowed-tools did not approve Write");
    assert!(!root.join("marker").exists());
    let loaded = serde_json::to_string(&model.requests.lock().unwrap()[1].messages).unwrap();
    assert!(loaded.contains("Create the marker with Write."), "{loaded}");
    agent.shutdown().await;
}

/// An Agent whose config root is `config_root`, with a Chat in `workdir` and the replies it gives,
/// one per request.
async fn agent_in(
    config_root: &Path,
    workdir: &Path,
    replies: usize,
) -> (Agent, uuid::Uuid, Arc<ScriptedModel>) {
    let model = Arc::new(ScriptedModel::new(
        (0..replies)
            .map(|reply| text(&format!("Reply {reply}.")))
            .collect(),
    ));
    let agent = Agent::open(config_root.to_path_buf(), config(), model.clone()).unwrap();
    agent
        .trust_working_directory(workdir.to_path_buf())
        .await
        .unwrap();
    let chat = agent.create_chat(workdir.to_path_buf()).await.unwrap();
    (agent, chat.uuid(), model)
}

#[tokio::test]
async fn unchanged_skills_send_the_same_skill_tool_definition_so_the_prompt_cache_holds() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_root = root.join("config");
    let workdir = root.join("work");
    fs::create_dir(&workdir).unwrap();
    install_package(
        &root,
        &PluginStore::in_config_root(&config_root),
        "quotes",
        "1.0.0",
        "Read quotes",
    );
    let (agent, chat, model) = agent_in(&config_root, &workdir, 2).await;
    let mut events = agent.subscribe();

    for prompt in ["One", "Two"] {
        let receipt = agent
            .submit(tickloop_ids::Id::from(chat), prompt.into())
            .await
            .unwrap();
        finish(&mut events, &receipt).await;
    }

    let skill_tool = |index: usize| {
        model.requests.lock().unwrap()[index]
            .tools
            .iter()
            .find(|tool| tool.name == "Skill")
            .map(|tool| serde_json::to_string(tool).unwrap())
            .unwrap()
    };
    assert!(skill_tool(0).contains("quotes:usage"));
    assert_eq!(skill_tool(0), skill_tool(1));
    agent.shutdown().await;
}

#[tokio::test]
async fn an_edited_user_skill_description_shows_at_the_next_request() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_root = root.join("config");
    let workdir = root.join("work");
    fs::create_dir(&workdir).unwrap();
    let user_skills = config_root.join("skills");
    write_skill(
        &user_skills,
        "notes",
        "name: notes\ndescription: Take notes",
        "Write notes.",
    );
    let (agent, chat, model) = agent_in(&config_root, &workdir, 2).await;
    let mut events = agent.subscribe();

    let receipt = agent
        .submit(tickloop_ids::Id::from(chat), "One".into())
        .await
        .unwrap();
    finish(&mut events, &receipt).await;
    assert!(skill_listing(&model, 0).contains("- notes [User]: Take notes\n"));

    write_skill(
        &user_skills,
        "notes",
        "name: notes\ndescription: Take dated notes",
        "Write dated notes.",
    );
    let receipt = agent
        .submit(tickloop_ids::Id::from(chat), "Two".into())
        .await
        .unwrap();
    finish(&mut events, &receipt).await;
    assert!(
        skill_listing(&model, 1).contains("- notes [User]: Take dated notes\n"),
        "{}",
        skill_listing(&model, 1)
    );
    agent.shutdown().await;
}

/// Whether `request` offers the Skill `name`.
fn lists_skill(request: &ModelRequest, name: &str) -> bool {
    request
        .tools
        .iter()
        .any(|tool| tool.name == "Skill" && tool.description.contains(&format!("- {name} [")))
}

#[tokio::test]
async fn a_subagent_sees_a_package_installed_or_uninstalled_at_its_next_request() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_root = root.join("config");
    let packages = PluginStore::in_config_root(&config_root);
    let workdir = root.join("work");
    fs::create_dir(&workdir).unwrap();
    let model = Arc::new(RoutedModel::new(|request| {
        if state(request)["delegation"].is_null() {
            return if has_tool_result(request, "delegate") {
                text("parent done")
            } else {
                call(
                    "delegate",
                    "Subagent",
                    json!({"task": "Wait for quotes", "name": "quoter", "run_in_background": true}),
                )
            };
        }
        if lists_skill(request, "quotes:usage") && !has_tool_result(request, "child-usage") {
            return call("child-usage", "Skill", json!({"skill": "quotes:usage"}));
        }
        text("child idle")
    }));
    let agent = Agent::open(config_root.clone(), config(), model.clone()).unwrap();
    agent
        .set_auto_reviewer(Arc::new(ScriptedModel::new(
            (0..8)
                .map(|_| text(&auto_classification(false).to_string()))
                .collect(),
        )))
        .await;
    agent
        .trust_working_directory(workdir.clone())
        .await
        .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Delegate".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    let child = agent.subagents(chat).await.unwrap()[0].child_chat_id;
    // The delegated turn ends first, so the next input starts a turn of its own.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let chat_loop = agent.snapshot(child).await.unwrap().chat_loop;
            if chat_loop.state == LoopState::Idle && chat_loop.run_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let child_requests = || {
        model
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| !state(request)["delegation"].is_null())
            .cloned()
            .collect::<Vec<_>>()
    };

    install_package(&root, &packages, "quotes", "1.0.0", "Read quotes");
    let receipt = agent.submit(child, "Quotes".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    let requests = child_requests();
    let installed = requests
        .iter()
        .position(|request| lists_skill(request, "quotes:usage"))
        .expect("the subagent's next request lists the installed package's Skill");
    assert!(
        requests[..installed]
            .iter()
            .all(|request| !lists_skill(request, "quotes:usage"))
    );
    let loaded = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
    assert!(loaded.contains("Launching skill: quotes:usage"), "{loaded}");
    assert!(loaded.contains("Run tick-quotes 1.0.0."), "{loaded}");

    packages.uninstall("quotes", false).unwrap();
    let receipt = agent.submit(child, "Again".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    assert!(!lists_skill(
        child_requests().last().unwrap(),
        "quotes:usage"
    ));
    agent.shutdown().await;
}

#[tokio::test]
async fn rediscovery_during_a_model_request_preserves_its_skill_body_snapshot() {
    struct PausedModel {
        scripted: ScriptedModel,
        entered: tokio::sync::Notify,
        resume: tokio::sync::Notify,
        pause_next: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl tickloop_harness::model::ModelClient for PausedModel {
        fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
            self.scripted.identity()
        }

        async fn generate_title(
            &self,
            request: ModelRequest,
            events: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError>
        {
            self.scripted.generate_title(request, events, cancel).await
        }

        async fn complete(
            &self,
            request: ModelRequest,
            events: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError>
        {
            if self
                .pause_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.entered.notify_one();
                tokio::select! {
                    () = cancel.cancelled() => return Err(tickloop_harness::model::ModelError::Cancelled),
                    () = self.resume.notified() => {},
                }
            }
            self.scripted.complete(request, events, cancel).await
        }
    }

    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    let skill_root = directory.path().join(".tickloop/skills");
    write_skill(
        &skill_root,
        "notes",
        "name: notes\ndescription: Original notes",
        "Original skill guidance.",
    );
    let model = Arc::new(PausedModel {
        scripted: ScriptedModel::new(vec![
            call("load", "Skill", json!({"skill":"notes"})),
            text("done"),
        ]),
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
        pause_next: std::sync::atomic::AtomicBool::new(true),
    });
    let agent = agent_with_project_skills(directory.path(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Use notes".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), model.entered.notified())
        .await
        .unwrap();
    write_skill(
        &skill_root,
        "notes",
        "name: notes\ndescription: Updated notes",
        "Updated skill guidance.",
    );
    assert!(agent.has_skill(chat, "notes").await.unwrap());
    model.resume.notify_one();
    finish(&mut events, &receipt).await;
    {
        let requests = model.scripted.requests.lock().unwrap();
        let listing = |index: usize| {
            &requests[index]
                .tools
                .iter()
                .find(|tool| tool.name == "Skill")
                .unwrap()
                .description
        };
        assert!(listing(0).contains("Original notes"));
        assert!(listing(1).contains("Updated notes"));
        let history = serde_json::to_string(&requests[1].messages).unwrap();
        assert!(history.contains("Original skill guidance."));
        assert!(!history.contains("Updated skill guidance."));
    }
    agent.shutdown().await;
}

const SLIDES_GUIDANCE: &str = "The user ran /slides to make a new Slides artifact.";

fn carries_slides_guidance(request: &ModelRequest) -> bool {
    let guided = guidance_in_effect(request, "slides");
    assert!(
        !guided
            || serde_json::to_string(&request.messages)
                .unwrap()
                .contains(SLIDES_GUIDANCE)
    );
    guided
}

#[tokio::test]
async fn slides_guides_the_run_its_user_invocation_starts_through_its_questions_and_no_later_run() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "topic",
            tickloop_harness::tools::ASK_USER_QUESTION_TOOL_NAME,
            json!({"questions":[{"question":"What should these Slides present?","header":"Topic","options":[
                {"label":"Quarterly results","description":"Revenue and margins"},
                {"label":"Product launch","description":"What ships and when"}
            ]}]}),
        ),
        text("Writing the Slides about the Q3 review."),
        call("load", "Skill", json!({"skill": "slides"})),
        text("Done."),
    ]));
    let agent = agent_with_project_skills(directory.path(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();

    agent.invoke_skill(chat, "slides", "").await.unwrap();
    let question = loop {
        if let EventKind::QuestionAsked { request } = next_event(&mut events).await.kind {
            break request;
        }
    };
    agent
        .answer_question(
            chat,
            question.id,
            vec![tickloop_harness::protocol::QuestionAnswer::other(
                "The Q3 review",
            )],
        )
        .await
        .unwrap();
    wait_for_completion(&agent, chat.uuid()).await;
    agent
        .submit(chat, "Something unrelated".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat.uuid()).await;

    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(carries_slides_guidance(&requests[0]));
    assert!(
        serde_json::to_string(&requests[0].messages)
            .unwrap()
            .contains("Topic: \\n")
    );
    assert!(
        carries_slides_guidance(&requests[1])
            && serde_json::to_string(&requests[1].messages)
                .unwrap()
                .contains("The Q3 review"),
        "the answer returns within the run /slides guides"
    );
    assert!(
        !carries_slides_guidance(&requests[2]) && !carries_slides_guidance(&requests[3]),
        "a later run is not guided by /slides"
    );
    let skill_tool = requests[2]
        .tools
        .iter()
        .find(|tool| tool.name == "Skill")
        .unwrap();
    assert!(
        !skill_tool
            .description
            .lines()
            .any(|line| line.starts_with("- slides"))
    );
    assert!(
        serde_json::to_string(&requests[3].messages)
            .unwrap()
            .contains("skill `slides` cannot be invoked by the model")
    );
}

#[tokio::test]
async fn slides_invoked_while_its_loop_works_guides_the_run_it_joins() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "slow",
            "Bash",
            json!({"command":"while [ ! -f slides-release ]; do sleep 0.01; done"}),
        ),
        text("Started the Slides."),
        text("Next task done."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let active = agent
        .submit(chat, "Check the workspace".into())
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::ToolStarted { .. }
        ) {
            break;
        }
    }
    let joined = agent
        .invoke_skill(chat, "slides", "the Q3 review")
        .await
        .unwrap();
    fs::write(directory.path().join("slides-release"), "release").unwrap();
    let mut merged = false;
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::InputMerged { into_run_id } if event.run_id == joined.run_id => {
                assert_eq!(into_run_id, active.run_id);
                merged = true;
            }
            EventKind::Completed { .. } if event.run_id == active.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(merged, "the invocation joined the working run");
    agent.submit(chat, "Next task".into()).await.unwrap();
    wait_for_completion(&agent, chat.uuid()).await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!carries_slides_guidance(&requests[0]));
    assert!(carries_slides_guidance(&requests[1]));
    assert!(
        serde_json::to_string(&requests[1].messages)
            .unwrap()
            .contains("Topic: the Q3 review")
    );
    assert!(!carries_slides_guidance(&requests[2]));
}

fn agent_with_project_skills(
    directory: &std::path::Path,
    model: Arc<dyn tickloop_harness::model::ModelClient>,
) -> Agent {
    Agent::with_seams(
        tickloop_harness::runtime::HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(
                tickloop_harness::runtime::environment::FolderEnvironment::bare(),
            )),
            models: tickloop_harness::test_support::harness::ChosenModels::new(model),
        },
        tickloop_harness::runtime::HarnessOptions {
            settings: Some(Arc::new(tickloop_code_settings::UserSettings::in_root(
                &directory.join("config"),
            ))),
            host: tickloop_harness::protocol::AgentHostOptions::default(),
            ..tickloop_harness::runtime::HarnessOptions::local(config())
        },
    )
    .unwrap()
}
