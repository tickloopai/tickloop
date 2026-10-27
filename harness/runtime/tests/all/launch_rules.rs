use std::sync::Arc;

use serde_json::json;
use tickloop_harness_protocol::{
    AgentHostOptions, Decision, EventKind, ExecutionMode, PermissionRule, PromptPolicy,
};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};

fn rule(tool: &str, decision: Decision) -> PermissionRule {
    PermissionRule {
        id: tickloop_ids::generate(),
        tool: tool.into(),
        decision,
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

#[tokio::test]
async fn launch_allow_admits_a_write_that_would_otherwise_be_denied_on_prompt() {
    for allowed in [false, true] {
        let folder = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("report.txt");
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "write",
                "Write",
                json!({"file_path":target,"content":"approved"}),
            ),
            text("Done"),
        ]));
        let agent = Agent::new_with_options(
            config(),
            model,
            AgentHostOptions {
                execution_mode_override: Some(ExecutionMode::AcceptEdits),
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::embedded_defaults()
            },
        );
        let rules = if allowed {
            vec![rule("Write", Decision::Allow)]
        } else {
            Vec::new()
        };
        let chat = agent
            .create_chat_with_launch_rules(folder.path().into(), rules)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, "Write the report".into()).await.unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { .. } => panic!("unattended execution prompted"),
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(target.exists(), allowed);
        if allowed {
            assert_eq!(std::fs::read_to_string(target).unwrap(), "approved");
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn launch_rules_are_visible_read_only_and_cannot_be_saved() {
    use tickloop_harness_protocol::PermissionChange;
    let folder = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let launch = rule("Bash", Decision::Deny);
    let chat = agent
        .create_chat_with_launch_rules(folder.path().into(), vec![launch.clone()])
        .await
        .unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    let displayed = view
        .rules
        .iter()
        .find(|entry| entry.rule.id == launch.id)
        .expect("launch rule is visible");
    assert!(!displayed.editable);
    assert_eq!(
        serde_json::to_value(displayed.source).unwrap(),
        "LaunchArguments"
    );
    let mut saved = launch.clone();
    saved.persistent = true;
    for change in [
        PermissionChange::UpsertRule { rule: saved },
        PermissionChange::RemoveRule { id: launch.id },
    ] {
        assert!(
            agent
                .apply_permission_change(chat, change, view.revision.clone())
                .await
                .is_err()
        );
    }
    assert!(
        agent
            .permissions_view(chat)
            .await
            .unwrap()
            .rules
            .iter()
            .any(|entry| entry.rule.id == launch.id)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn launch_rules_do_not_reappear_when_history_is_loaded_by_a_new_agent() {
    use tickloop_harness_runtime::{HarnessOptions, HarnessSeams, environment::FolderEnvironment};
    use tickloop_harness_test_support::{
        harness::ChosenModels, transcripts::InMemoryTranscriptStore,
    };
    let folder = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryTranscriptStore::default());
    let build = || {
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(store.clone()),
                environment: Some(Arc::new(FolderEnvironment::bare())),
                models: ChosenModels::new(Arc::new(ScriptedModel::new(vec![text("Done")]))),
            },
            HarnessOptions::local(config()),
        )
        .unwrap()
    };
    let first = build();
    let launch = rule("Read", Decision::Deny);
    let chat = first
        .create_chat_with_launch_rules(folder.path().into(), vec![launch.clone()])
        .await
        .unwrap();
    let other = first.create_chat(folder.path().into()).await.unwrap();
    assert!(
        first
            .permissions_view(other)
            .await
            .unwrap()
            .rules
            .is_empty()
    );
    let mut events = first.subscribe();
    first
        .submit(chat, "Remember this conversation".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    first.sync().await.unwrap();
    first.shutdown().await;
    let next = build();
    next.resume_chat(chat).await.unwrap();
    assert!(
        !next
            .permissions_view(chat)
            .await
            .unwrap()
            .rules
            .iter()
            .any(|entry| entry.rule.id == launch.id)
    );
    next.shutdown().await;
}

#[tokio::test]
async fn launch_rules_preserve_saved_deny_ask_and_sensitive_path_checks() {
    use tickloop_harness_policy::settings::{MemorySettingsStore, SettingsStore};
    use tickloop_harness_runtime::{HarnessOptions, HarnessSeams, environment::FolderEnvironment};
    use tickloop_harness_test_support::harness::ChosenModels;
    for mode in [ExecutionMode::AcceptEdits, ExecutionMode::Auto] {
        for (launch_decision, saved_decision, sensitive) in [
            (Decision::Allow, Decision::Deny, false),
            (Decision::Allow, Decision::Ask, false),
            (Decision::Deny, Decision::Allow, false),
            (Decision::Allow, Decision::Allow, true),
        ] {
            let folder = tempfile::tempdir().unwrap();
            let target = folder.path().join(if sensitive {
                ".git/hooks/pre-commit"
            } else {
                "report.txt"
            });
            if sensitive {
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            }
            let model = Arc::new(ScriptedModel::new(vec![
                call(
                    "write",
                    "Write",
                    json!({"file_path":target,"content":"must not be written"}),
                ),
                text("Done"),
            ]));
            let settings = Arc::new(MemorySettingsStore::default());
            let agent = Agent::with_seams(
                HarnessSeams {
                    search: None,
                    transcripts: None,
                    environment: Some(Arc::new(FolderEnvironment::bare())),
                    models: ChosenModels::new(model),
                },
                HarnessOptions {
                    settings: Some(settings.clone()),
                    host: AgentHostOptions {
                        execution_mode_override: Some(mode),
                        prompt_policy: PromptPolicy::DenyOnPrompt,
                        ..AgentHostOptions::embedded_defaults()
                    },
                    ..HarnessOptions::local(config())
                },
            )
            .unwrap();
            let verdict = json!({"decision":"block_hard", "reason":"fixture hard boundary", "rule_id":"builtin.hard_deny.data_exfiltration"}).to_string();
            let reviewer = Arc::new(ScriptedModel::new(vec![text(&verdict); 2]));
            agent.set_auto_reviewer(reviewer.clone()).await;
            let base = agent.create_chat(folder.path().into()).await.unwrap();
            let mut saved = rule("Write", saved_decision);
            saved.persistent = true;
            agent
                .set_permission_rules(base, vec![saved.clone()])
                .await
                .unwrap();
            let chat = agent
                .create_chat_with_launch_rules(
                    folder.path().into(),
                    vec![rule("Write", launch_decision)],
                )
                .await
                .unwrap();
            let mut events = agent.subscribe();
            agent.submit(chat, "Write the report".into()).await.unwrap();
            loop {
                match next_event(&mut events).await.kind {
                    EventKind::PermissionRequested { .. } => panic!("deny-on-prompt must not ask"),
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            assert!(
                !target.exists(),
                "{mode:?} {launch_decision:?} {saved_decision:?} sensitive={sensitive}"
            );
            assert_eq!(
                !reviewer.requests.lock().unwrap().is_empty(),
                sensitive && mode == ExecutionMode::Auto
            );
            let persisted = settings
                .read(Some(&folder.path().canonicalize().unwrap()))
                .project
                .rules;
            assert_eq!(persisted.len(), 1);
            assert_eq!(persisted[0].id, saved.id);
            agent.shutdown().await;
        }
    }
}

#[tokio::test]
async fn launch_rules_constrain_subagents_but_do_not_survive_their_restart() {
    use tickloop_harness_runtime::{HarnessOptions, HarnessSeams, environment::FolderEnvironment};
    use tickloop_harness_test_support::{
        harness::ChosenModels,
        routed::{RoutedModel, has_tool_result, state},
        transcripts::InMemoryTranscriptStore,
    };
    let folder = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryTranscriptStore::default());
    let model = Arc::new(RoutedModel::new(|request| {
        if !state(request)["delegation"].is_null() {
            if has_tool_result(request, "child-write") {
                return text("Child finished");
            }
            return call(
                "child-write",
                "Write",
                json!({"file_path":"child.txt", "content":"must not be written"}),
            );
        }
        if has_tool_result(request, "delegate") {
            return text("Root finished");
        }
        call(
            "delegate",
            "Subagent",
            json!({"task":"Write child.txt", "name":"writer"}),
        )
    }));
    let build = || {
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(store.clone()),
                environment: Some(Arc::new(FolderEnvironment::bare())),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions {
                host: AgentHostOptions {
                    execution_mode_override: Some(ExecutionMode::AcceptEdits),
                    prompt_policy: PromptPolicy::DenyOnPrompt,
                    ..AgentHostOptions::embedded_defaults()
                },
                ..HarnessOptions::local(config())
            },
        )
        .unwrap()
    };
    let agent = build();
    let launch = rule("Write", Decision::Deny);
    let root = agent
        .create_chat_with_launch_rules(folder.path().into(), vec![launch.clone()])
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(root, "Delegate the write".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        assert!(!matches!(event.kind, EventKind::PermissionRequested { .. }));
        if event.run_id == receipt.run_id {
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }
    let child = agent.subagents(root).await.unwrap()[0].child_chat_id;
    assert!(!folder.path().join("child.txt").exists());
    assert!(model.requests.lock().unwrap().iter().any(|request| {
        !state(request)["delegation"].is_null() && request.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_harness_protocol::Content::ToolResult { tool_use_id, content, is_error: true, .. } if tool_use_id == "child-write" && content.text().contains("denial")))
    }));
    assert!(
        agent
            .permissions_view(child)
            .await
            .unwrap()
            .rules
            .iter()
            .any(|entry| entry.rule.id == launch.id && !entry.editable)
    );
    agent.sync().await.unwrap();
    agent.shutdown().await;
    let restarted = build();
    restarted.resume_chat(root).await.unwrap();
    restarted.resume_chat(child).await.unwrap();
    assert!(
        !restarted
            .permissions_view(child)
            .await
            .unwrap()
            .rules
            .iter()
            .any(|entry| entry.rule.id == launch.id)
    );
    restarted.shutdown().await;
}
