mod support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::json;
use support::{ScriptedModel, call, config, next_event, text};
use tickloop_agent::{
    host_tools::{HostToolPermission, HostToolProvider},
    model::ToolDefinition,
    permissions::{Decision, ExecutionMode, PermissionRule},
    runtime::{
        Agent, ChatId, EventKind, PermissionApprovalSource, PermissionChange,
        PermissionChangeError, PermissionRuleScope, PermissionRuleSource, RunId,
    },
    tools::ToolResult,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn rule(id: Uuid, decision: Decision, persistent: bool) -> PermissionRule {
    PermissionRule {
        id,
        decision,
        tool: "Write".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent,
    }
}

#[derive(Default)]
struct ManagedHostTool {
    executions: AtomicUsize,
}

#[async_trait]
impl HostToolProvider for ManagedHostTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "change_panel".into(),
            description: "Change the active panel.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &serde_json::Value) -> HostToolPermission {
        HostToolPermission::Automatic
    }

    async fn execute(
        &self,
        _: ChatId,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
    ) -> ToolResult {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            content: "panel changed".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

async fn completed_without_request(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
    run_id: RunId,
) -> bool {
    let mut started = false;
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { .. } => panic!("unexpected human approval request"),
            EventKind::ToolStarted { .. } => started = true,
            EventKind::Completed { .. } => return started,
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
}

async fn requested_permission(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
    run_id: RunId,
) -> tickloop_agent::runtime::PermissionRequest {
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => return request,
            EventKind::ToolStarted { .. } => panic!("tool started before approval"),
            EventKind::Completed { .. } => panic!("run completed without approval"),
            EventKind::Failed(error) => panic!("run failed before approval: {error}"),
            _ => {}
        }
    }
}

async fn completion(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
    run_id: RunId,
) {
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn editing_rules_changes_the_next_tool_admission_and_exposes_scope_and_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "ask",
            "Write",
            json!({"file_path":"ask.txt","content":"ask"}),
        ),
        text("ask approved"),
        call(
            "deny",
            "Write",
            json!({"file_path":"deny.txt","content":"deny"}),
        ),
        text("denied"),
        call(
            "allow",
            "Write",
            json!({"file_path":"allow.txt","content":"allow"}),
        ),
        text("allowed"),
        call(
            "removed",
            "Write",
            json!({"file_path":"removed.txt","content":"removed"}),
        ),
        text("default mode admitted"),
    ]));
    let agent = Agent::open_embedded(root, config(), model).unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let mut view = agent.permissions_view(chat).await.unwrap();
    let stale_revision = view.revision.clone();
    view = agent
        .apply_permission_change(
            chat,
            PermissionChange::UpsertRule {
                rule: rule(id, Decision::Ask, false),
            },
            view.revision,
        )
        .await
        .unwrap();
    assert!(matches!(
        agent
            .apply_permission_change(
                chat,
                PermissionChange::UpsertRule {
                    rule: rule(id, Decision::Deny, false)
                },
                stale_revision,
            )
            .await,
        Err(PermissionChangeError::Conflict { .. })
    ));
    let displayed = view.rules.iter().find(|entry| entry.rule.id == id).unwrap();
    assert_eq!(displayed.source, PermissionRuleSource::ChatSession);
    assert_eq!(displayed.scope, PermissionRuleScope::Chat);
    assert!(displayed.editable);
    assert!(!displayed.rule.persistent);
    let mut events = agent.subscribe();
    let attempt = agent.submit(chat, None, "write ask".into()).await.unwrap();
    let request = requested_permission(&mut events, attempt.run_id).await;
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    completion(&mut events, attempt.run_id).await;
    assert_eq!(
        std::fs::read_to_string(workdir.join("ask.txt")).unwrap(),
        "ask"
    );

    view = agent
        .apply_permission_change(
            chat,
            PermissionChange::UpsertRule {
                rule: rule(id, Decision::Deny, false),
            },
            view.revision,
        )
        .await
        .unwrap();
    let attempt = agent.submit(chat, None, "write deny".into()).await.unwrap();
    assert!(!completed_without_request(&mut events, attempt.run_id).await);
    assert!(!workdir.join("deny.txt").exists());

    view = agent
        .apply_permission_change(
            chat,
            PermissionChange::UpsertRule {
                rule: rule(id, Decision::Allow, false),
            },
            view.revision,
        )
        .await
        .unwrap();
    let attempt = agent
        .submit(chat, None, "write allow".into())
        .await
        .unwrap();
    assert!(completed_without_request(&mut events, attempt.run_id).await);
    assert_eq!(
        std::fs::read_to_string(workdir.join("allow.txt")).unwrap(),
        "allow"
    );

    agent
        .apply_permission_change(chat, PermissionChange::RemoveRule { id }, view.revision)
        .await
        .unwrap();
    let attempt = agent
        .submit(chat, None, "write after removal".into())
        .await
        .unwrap();
    assert!(completed_without_request(&mut events, attempt.run_id).await);
    assert_eq!(
        std::fs::read_to_string(workdir.join("removed.txt")).unwrap(),
        "removed"
    );
    assert!(
        agent
            .permissions_view(chat)
            .await
            .unwrap()
            .rules
            .iter()
            .all(|entry| entry.rule.id != id)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn conflicting_project_edits_preserve_unrelated_settings_and_latest_rules() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(root.join("settings.json"), r#"{"futureSetting":true}"#).unwrap();
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let second = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let first_chat = first.create_chat(workdir.clone()).await.unwrap();
    let second_chat = second.create_chat(workdir.clone()).await.unwrap();
    let first_view = first.permissions_view(first_chat).await.unwrap();
    let stale = second.permissions_view(second_chat).await.unwrap();
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let saved = first
        .apply_permission_change(
            first_chat,
            PermissionChange::UpsertRule {
                rule: rule(first_id, Decision::Ask, true),
            },
            first_view.revision,
        )
        .await
        .unwrap();
    assert_eq!(
        saved
            .rules
            .iter()
            .find(|entry| entry.rule.id == first_id)
            .unwrap()
            .source,
        PermissionRuleSource::ProjectUser
    );
    assert_eq!(
        saved
            .rules
            .iter()
            .find(|entry| entry.rule.id == first_id)
            .unwrap()
            .scope,
        PermissionRuleScope::Project(workdir.canonicalize().unwrap())
    );
    let before_conflict = std::fs::read(root.join("settings.json")).unwrap();
    let conflict = second
        .apply_permission_change(
            second_chat,
            PermissionChange::UpsertRule {
                rule: rule(second_id, Decision::Allow, true),
            },
            stale.revision,
        )
        .await
        .unwrap_err();
    let PermissionChangeError::Conflict { latest } = conflict else {
        panic!("stale writer was not offered the latest projection: {conflict:?}");
    };
    assert!(latest.rules.iter().any(|entry| entry.rule.id == first_id));
    assert!(latest.rules.iter().all(|entry| entry.rule.id != second_id));
    assert_eq!(
        std::fs::read(root.join("settings.json")).unwrap(),
        before_conflict
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&before_conflict).unwrap()["futureSetting"],
        true
    );
    first.shutdown().await;
    second.shutdown().await;

    let restarted = Agent::open_embedded(
        root,
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "restored",
                "Write",
                json!({"file_path":"restored.txt","content":"restored"}),
            ),
            text("done"),
        ])),
    )
    .unwrap();
    let restored_chat = restarted.create_chat(workdir.clone()).await.unwrap();
    let restored = restarted.permissions_view(restored_chat).await.unwrap();
    assert!(restored.rules.iter().any(|entry| entry.rule.id == first_id));
    assert!(
        restored
            .rules
            .iter()
            .all(|entry| entry.rule.id != second_id)
    );
    restarted
        .set_execution_mode(restored_chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = restarted.subscribe();
    let attempt = restarted
        .submit(restored_chat, None, "write after restart".into())
        .await
        .unwrap();
    let request = requested_permission(&mut events, attempt.run_id).await;
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );
    assert!(!workdir.join("restored.txt").exists());
    restarted
        .answer_permission(restored_chat, request.id, true)
        .await
        .unwrap();
    completion(&mut events, attempt.run_id).await;
    assert!(workdir.join("restored.txt").exists());
    restarted.shutdown().await;
}

#[tokio::test]
async fn removed_extra_directory_revokes_a_held_approval() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    let extra = temp.path().join("extra");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&extra).unwrap();
    let outside_file = extra.join("held.txt");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "held",
            "Write",
            json!({"file_path":outside_file,"content":"must remain absent"}),
        ),
        text("could not write"),
    ]));
    let agent = Agent::open_embedded(root, config(), model).unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let mut view = agent.permissions_view(chat).await.unwrap();
    assert_eq!(view.working_directory, workdir.canonicalize().unwrap());
    assert!(view.additional_directories.is_empty());
    let invalid = agent
        .apply_permission_change(
            chat,
            PermissionChange::AddDirectory {
                path: temp.path().join("missing"),
            },
            view.revision.clone(),
        )
        .await;
    assert!(invalid.is_err());
    assert!(
        agent
            .permissions_view(chat)
            .await
            .unwrap()
            .additional_directories
            .is_empty()
    );
    view = agent
        .apply_permission_change(
            chat,
            PermissionChange::AddDirectory {
                path: extra.clone(),
            },
            view.revision,
        )
        .await
        .unwrap();
    assert_eq!(
        view.additional_directories,
        vec![extra.canonicalize().unwrap()]
    );
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let ask_id = Uuid::new_v4();
    let view = agent.permissions_view(chat).await.unwrap();
    let view = agent
        .apply_permission_change(
            chat,
            PermissionChange::UpsertRule {
                rule: rule(ask_id, Decision::Ask, false),
            },
            view.revision,
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let attempt = agent
        .submit(chat, None, "write in the extra directory".into())
        .await
        .unwrap();
    let request = requested_permission(&mut events, attempt.run_id).await;
    agent
        .apply_permission_change(
            chat,
            PermissionChange::RemoveDirectory {
                path: extra.clone(),
            },
            view.revision,
        )
        .await
        .unwrap();
    assert!(
        agent
            .permissions_view(chat)
            .await
            .unwrap()
            .additional_directories
            .is_empty()
    );
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != attempt.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolStarted { .. } => panic!("removed directory retained stale access"),
            EventKind::Failed(reason) => {
                assert!(reason.contains("permission policy changed"), "{reason}");
                break;
            }
            EventKind::Completed { .. } => panic!("stale approval completed after removal"),
            _ => {}
        }
    }
    assert!(!extra.join("held.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn relative_directory_uses_chat_workdir_and_removal_survives_missing_path() {
    let temp = tempfile::tempdir().unwrap();
    let workdir = temp.path().join("work");
    let extra = workdir.join("relative-extra");
    std::fs::create_dir_all(&extra).unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    let added = agent
        .apply_permission_change(
            chat,
            PermissionChange::AddDirectory {
                path: "relative-extra".into(),
            },
            view.revision,
        )
        .await
        .unwrap();
    assert_eq!(
        added.additional_directories,
        vec![extra.canonicalize().unwrap()]
    );
    std::fs::remove_dir(&extra).unwrap();
    let removed = agent
        .apply_permission_change(
            chat,
            PermissionChange::RemoveDirectory {
                path: "relative-extra".into(),
            },
            added.revision,
        )
        .await
        .unwrap();
    assert!(removed.additional_directories.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn read_only_constraints_and_invalid_matchers_do_not_change_policy() {
    let temp = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    let managed_id = Uuid::new_v4();
    let project_id = Uuid::new_v4();
    agent
        .set_managed_permission_constraints(chat, vec![rule(managed_id, Decision::Deny, false)])
        .await
        .unwrap();
    agent
        .set_project_permission_constraints(chat, vec![rule(project_id, Decision::Ask, false)])
        .await
        .unwrap();
    let original = agent.permissions_view(chat).await.unwrap();
    for (id, source) in [
        (managed_id, PermissionRuleSource::ManagedConstraint),
        (project_id, PermissionRuleSource::ProjectConstraint),
    ] {
        let displayed = original
            .rules
            .iter()
            .find(|entry| entry.rule.id == id)
            .unwrap();
        assert_eq!(displayed.source, source);
        assert!(!displayed.editable);
        assert!(displayed.effective_note.is_some());
        assert!(matches!(
            agent
                .apply_permission_change(
                    chat,
                    PermissionChange::RemoveRule { id },
                    original.revision.clone(),
                )
                .await,
            Err(PermissionChangeError::Invalid(_))
        ));
        assert!(matches!(
            agent
                .apply_permission_change(
                    chat,
                    PermissionChange::UpsertRule {
                        rule: rule(id, Decision::Allow, false)
                    },
                    original.revision.clone(),
                )
                .await,
            Err(PermissionChangeError::Invalid(_))
        ));
    }
    let mut invalid = rule(Uuid::new_v4(), Decision::Allow, false);
    invalid.specifier = Some("echo *".into());
    assert!(matches!(
        agent
            .apply_permission_change(
                chat,
                PermissionChange::UpsertRule { rule: invalid },
                original.revision.clone(),
            )
            .await,
        Err(PermissionChangeError::Invalid(_))
    ));
    assert_eq!(
        agent.permissions_view(chat).await.unwrap().revision,
        original.revision
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn management_ask_rule_requires_human_before_host_tool_execution() {
    let temp = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("change", "change_panel", json!({})),
        text("panel changed"),
    ]));
    let reviewer = Arc::new(ScriptedModel::new(Vec::new()));
    let host = Arc::new(ManagedHostTool::default());
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(host.clone()).unwrap();
    agent.set_auto_reviewer(reviewer.clone()).await;
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut shell_allow = rule(Uuid::new_v4(), Decision::Allow, false);
    shell_allow.tool = "*".into();
    shell_allow.specifier = Some("cargo *".into());
    let view = agent.permissions_view(chat).await.unwrap();
    let view = agent
        .apply_permission_change(
            chat,
            PermissionChange::UpsertRule {
                rule: shell_allow.clone(),
            },
            view.revision,
        )
        .await
        .unwrap();
    assert!(
        view.rules
            .iter()
            .any(|entry| entry.rule.id == shell_allow.id)
    );
    let mut broad_allow = rule(Uuid::new_v4(), Decision::Allow, false);
    broad_allow.tool = "*".into();
    let view = agent
        .apply_permission_change(
            chat,
            PermissionChange::UpsertRule {
                rule: broad_allow.clone(),
            },
            view.revision,
        )
        .await
        .unwrap();
    assert!(
        view.rules
            .iter()
            .find(|entry| entry.rule.id == broad_allow.id)
            .unwrap()
            .effective_note
            .as_deref()
            .is_some_and(|note| note.contains("Auto review"))
    );
    let mut ask = rule(Uuid::new_v4(), Decision::Ask, false);
    ask.tool = "change_panel".into();
    agent
        .apply_permission_change(
            chat,
            PermissionChange::UpsertRule { rule: ask },
            view.revision,
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let attempt = agent
        .submit(chat, None, "change panel".into())
        .await
        .unwrap();
    let request = requested_permission(&mut events, attempt.run_id).await;
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );
    assert_eq!(host.executions.load(Ordering::SeqCst), 0);
    assert!(reviewer.requests.lock().unwrap().is_empty());
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    completion(&mut events, attempt.run_id).await;
    assert_eq!(host.executions.load(Ordering::SeqCst), 1);
    agent.shutdown().await;
}
