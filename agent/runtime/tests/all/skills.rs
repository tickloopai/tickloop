use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tick_packages::{MANIFEST_FILE_NAME, PackageStore};
use tickloop_agent_config::skills::{SkillCaller, SkillError, SkillRegistry};
use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{Content, EventKind, ExecutionMode, LoopState, Receipt, SkillSource};
use tickloop_agent_runtime::Agent;

use tickloop_agent_test_support::routed::{RoutedModel, has_tool_result, state};
use tickloop_agent_test_support::{
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
fn repository_model_integration_skill_is_discovered_and_expandable() {
    let workdir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let registry = SkillRegistry::discover(workdir, None);
    let expansion = registry
        .expand("/model-integration", "", SkillCaller::User)
        .unwrap();
    assert!(matches!(expansion.source, SkillSource::Project { .. }));
    assert!(expansion.content.contains("model/history.rs"));
    assert!(registry.model_listing().contains("model-integration"));
    assert!(expansion.allowed_tools.is_empty());
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

    assert_eq!(names, ["nearest", "personal", "root-skill", "tickloop"]);
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
    assert_eq!(
        registry
            .expand("manual", "", SkillCaller::User)
            .unwrap()
            .content,
        "Manual body"
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
    assert_eq!(expansion.content, "Analyze report.md carefully.");
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
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot
                .loops
                .last()
                .is_some_and(|goal| goal.state == LoopState::Completed)
            {
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
    let agent = Agent::new(config(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .submit(chat, None, "Arrange my Workspace".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

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
        assert_eq!(guidance.role, tickloop_agent_protocol::Role::User);
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
    let agent = Agent::new(config(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .submit(chat, None, "Analyze the report".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

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
        ["args", "resource", "skill"]
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
        ("Launching skill: analyze".into(), false)
    );
    let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
    assert!(last.contains("Analyze report.md carefully."));
    assert!(!last.contains("Analyze notes.md carefully."));
}

#[tokio::test]
async fn user_invocation_uses_the_same_expansion_and_records_the_slash_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("ready")]));
    let agent = Agent::new(config(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .invoke_skill(chat, None, "/tickloop", "show the current Workspace")
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    let requests = model.requests.lock().unwrap();
    let messages = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(
        messages
            .contains("Use the installed host tools as the authority for targets and capabilities")
    );
    assert!(messages.contains("/tickloop show the current Workspace"));
    assert!(matches!(
        requests[0].messages.last().unwrap().content.as_slice(),
        [Content::Text { text }] if text == "/tickloop show the current Workspace"
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

    agent
        .invoke_skill(chat, None, "tickloop", "")
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
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
    let agent = Agent::new(config(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Analyze these targets".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let requests = model.requests.lock().unwrap();
    assert!(
        serde_json::to_string(&requests[2].messages)
            .unwrap()
            .contains("Already loaded analyze")
    );
    let guidance = requests[3].messages.iter().find(|message| message.content.iter().any(|block| matches!(block, Content::Text { text } if text.contains("Analyze second.md carefully.")))).unwrap();
    assert_eq!(guidance.role, tickloop_agent_protocol::Role::User);
    assert!(
        !serde_json::to_string(&snapshot.messages)
            .unwrap()
            .contains("Analyze second.md carefully.")
    );
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
    let agent = Agent::new(config(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    for index in 0..6 {
        agent
            .invoke_skill(chat, None, &format!("guide-{index}"), "")
            .await
            .unwrap();
        wait_for_completion(&agent, chat).await;
    }
    agent.invoke_skill(chat, None, "guide-0", "").await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent.compact(chat).await.unwrap();
    agent
        .submit(chat, None, "Continue with these guides".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
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
    packages: &PackageStore,
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
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
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
    let packages = PackageStore::in_config_root(&config_root);
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

    let receipt = agent.submit(chat, None, "Quotes?".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    assert!(!skill_listing(&model, 0).contains("quotes:usage"));

    install_package(&root, &packages, "quotes", "1.0.0", "Read quotes");
    let receipt = agent
        .submit(chat, None, "Quotes now?".into())
        .await
        .unwrap();
    finish(&mut events, &receipt).await;
    assert!(
        skill_listing(&model, 1).contains("- quotes:usage: Read quotes"),
        "{}",
        skill_listing(&model, 1)
    );
    let loaded = serde_json::to_string(&model.requests.lock().unwrap()[2].messages).unwrap();
    assert!(loaded.contains("Launching skill: quotes:usage"), "{loaded}");
    assert!(loaded.contains("Run tick-quotes 1.0.0."), "{loaded}");

    let receipt = agent
        .invoke_skill(chat, None, "/quotes:usage", "")
        .await
        .unwrap();
    finish(&mut events, &receipt).await;
    let invoked = serde_json::to_string(&model.requests.lock().unwrap()[3].messages).unwrap();
    assert!(invoked.contains("/quotes:usage"), "{invoked}");

    packages.uninstall("quotes").unwrap();
    assert!(!agent.has_skill(chat, "quotes:usage").await.unwrap());
    assert!(
        agent
            .invoke_skill(chat, None, "/quotes:usage", "")
            .await
            .is_err()
    );
    let receipt = agent.submit(chat, None, "And now?".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    assert!(!skill_listing(&model, 4).contains("quotes:usage"));
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
    (agent, chat, model)
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
        &PackageStore::in_config_root(&config_root),
        "quotes",
        "1.0.0",
        "Read quotes",
    );
    let (agent, chat, model) = agent_in(&config_root, &workdir, 2).await;
    let mut events = agent.subscribe();

    for prompt in ["One", "Two"] {
        let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
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

    let receipt = agent.submit(chat, None, "One".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    assert!(skill_listing(&model, 0).contains("- notes: Take notes\n"));

    write_skill(
        &user_skills,
        "notes",
        "name: notes\ndescription: Take dated notes",
        "Write dated notes.",
    );
    let receipt = agent.submit(chat, None, "Two".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    assert!(
        skill_listing(&model, 1).contains("- notes: Take dated notes\n"),
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
        .any(|tool| tool.name == "Skill" && tool.description.contains(&format!("- {name}:")))
}

#[tokio::test]
async fn a_subagent_sees_a_package_installed_or_uninstalled_at_its_next_request() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_root = root.join("config");
    let packages = PackageStore::in_config_root(&config_root);
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
    let receipt = agent.submit(chat, None, "Delegate".into()).await.unwrap();
    finish(&mut events, &receipt).await;
    let child = agent.subagents(chat).await.unwrap()[0].child_chat_id;
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
    let receipt = agent.submit(child, None, "Quotes".into()).await.unwrap();
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

    packages.uninstall("quotes").unwrap();
    let receipt = agent.submit(child, None, "Again".into()).await.unwrap();
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
    impl tickloop_agent_model::ModelClient for PausedModel {
        fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
            self.scripted.identity()
        }

        async fn generate_title(
            &self,
            request: ModelRequest,
            events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
            self.scripted.generate_title(request, events, cancel).await
        }

        async fn complete(
            &self,
            request: ModelRequest,
            events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
            if self
                .pause_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.entered.notify_one();
                tokio::select! {
                    () = cancel.cancelled() => return Err(tickloop_agent_model::ModelError::Cancelled),
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
    let agent = Agent::new(config(), model.clone());
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "Use notes".into()).await.unwrap();
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
