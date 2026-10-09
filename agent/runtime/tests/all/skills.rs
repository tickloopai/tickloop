use std::fs;
use std::sync::Arc;

use serde_json::json;
use tickloop_agent_config::skills::{SkillCaller, SkillError, SkillRegistry};
use tickloop_agent_protocol::{Content, LoopState, SkillSource};
use tickloop_agent_runtime::Agent;

use tickloop_agent_test_support::{ScriptedModel, call, config, text};

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
    let user = root.path().join("user-skills");
    write_skill(
        &user,
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

    let registry = SkillRegistry::discover(&workdir, Some(&user));
    let names = registry
        .definitions()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        [
            "artifact-draft",
            "nearest",
            "personal",
            "root-skill",
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
    write_skill(
        directory.path(),
        "tickloop",
        "name: tickloop\ndescription: Replacement",
        "Replacement body",
    );
    write_skill(directory.path(), "broken", "name: Broken", "Broken body");

    let registry = SkillRegistry::discover(directory.path(), Some(directory.path()));
    let expansion = registry.expand("tickloop", "", SkillCaller::User).unwrap();

    assert!(matches!(expansion.source, SkillSource::BuiltIn));
    assert!(expansion.content.contains("Tick Loop Ticks"));
    assert_eq!(registry.warnings().len(), 2);
}

#[test]
fn listing_discloses_only_model_invocable_skill_metadata() {
    let directory = tempfile::tempdir().unwrap();
    write_skill(
        directory.path(),
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
    write_skill(
        directory.path(),
        "analyze",
        "name: analyze\ndescription: Analyze a target\nallowed-tools: [Read, Grep]",
        "Analyze $ARGUMENTS carefully.",
    );
    write_skill(
        directory.path(),
        "model-only",
        "name: model-only\ndescription: Model only\nuser-invocable: false",
        "Model body",
    );
    write_skill(
        directory.path(),
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
fn model_listing_stops_at_the_budget_without_cutting_an_entry() {
    let registry = SkillRegistry::discover_with_builtins(
        std::path::Path::new("."),
        None,
        [
            "---\nname: first\ndescription: One\n---\nFirst body",
            "---\nname: second\ndescription: Two\n---\nSecond body",
        ],
    );

    assert_eq!(registry.model_listing_with_budget(13), "- first: One\n");
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
    }

    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.invoked_skills.len(), 1);
    assert_eq!(snapshot.invoked_skills[0].name, "tickloop");
}

#[tokio::test]
async fn model_passes_skill_arguments_as_args_like_claude_code() {
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
        ["args", "skill"]
    );
    assert_eq!(skill_tool.input_schema["required"], json!(["skill"]));
    assert!(skill_tool.description.contains("args: \""));
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
