use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tickloop_agent_settings::local_agent::LocalAgent;

use serde_json::json;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    ChatId, Decision, DenialCategory, EventKind, ExecutionMode, ModelIdentity, ModelProvider,
    PermissionApprovalSource, PermissionChange, PermissionRule, Protocol, ToolDefinition,
    ToolResult, Usage,
};
use tickloop_agent_runtime::{
    Agent,
    host_tools::{HostToolPermission, HostToolProvider},
};
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct HeldReviewer {
    entered: Notify,
    release: Notify,
    review_calls: AtomicUsize,
    normal_calls: AtomicUsize,
}

struct HostGate {
    entered: Notify,
    release: Notify,
}

#[derive(Default)]
struct AutomaticHostTool {
    executions: AtomicUsize,
    gate: Option<Arc<HostGate>>,
}

#[async_trait::async_trait]
impl HostToolProvider for AutomaticHostTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "change_panel".into(),
            description: "Change the current panel".into(),
            input_schema: json!({"type":"object"}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &serde_json::Value) -> HostToolPermission {
        HostToolPermission::Automatic
    }

    async fn revalidate(
        &self,
        _: ChatId,
        _: &str,
        _: &serde_json::Value,
        _: HostToolPermission,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            tokio::select! {
                _ = cancel.cancelled() => return Err("cancelled".into()),
                _ = gate.release.notified() => {}
            }
        }
        Ok(())
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
            content: "changed".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[async_trait::async_trait]
impl ModelClient for HeldReviewer {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "held-reviewer".into(),
            protocol: Protocol::Messages,
            requested_model: "held-reviewer".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let content = if request.structured_output.is_some() {
            self.review_calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            tokio::select! {
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                _ = self.release.notified() => {}
            }
            text(&tickloop_agent_test_support::auto_classification(false).to_string())
        } else if self.normal_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            call(
                "held-write",
                "Bash",
                json!({"command":"ln -s missing-target blocked.txt"}),
            )
        } else {
            text("done")
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn project_rule(tool: &str, decision: Decision) -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision,
        tool: tool.into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: true,
    }
}

#[tokio::test]
async fn cross_process_project_deny_invalidates_a_held_auto_allow() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(HeldReviewer {
        entered: Notify::new(),
        release: Notify::new(),
        review_calls: AtomicUsize::new(0),
        normal_calls: AtomicUsize::new(0),
    });
    let first = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let second =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let first_chat = first.create_chat(workdir.clone()).await.unwrap();
    let second_chat = second.create_chat(workdir.clone()).await.unwrap();
    first
        .set_execution_mode(first_chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = first.subscribe();
    let run = first
        .submit(first_chat, "Write blocked.txt".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.entered.notified())
        .await
        .expect("Auto review did not start");
    let view = second.permissions_view(second_chat).await.unwrap();
    second
        .apply_permission_change(
            second_chat,
            PermissionChange::UpsertRule {
                rule: project_rule("Bash", Decision::Deny),
            },
            view.revision,
        )
        .await
        .unwrap();
    model.release.notify_one();

    let mut started = false;
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolStarted { .. } => started = true,
            EventKind::Completed { .. } | EventKind::Failed(_) => break,
            _ => {}
        }
    }
    assert!(!started);
    assert!(!workdir.join("blocked.txt").exists());
    assert_eq!(model.review_calls.load(Ordering::SeqCst), 1);
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn cross_process_project_ask_requires_human_on_next_operation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![call(
        "new-ask",
        "Write",
        json!({"file_path":"ask.txt","content":"requires approval"}),
    )]));
    let first = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let second =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let first_chat = first.create_chat(workdir.clone()).await.unwrap();
    let second_chat = second.create_chat(workdir.clone()).await.unwrap();
    first
        .set_execution_mode(first_chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let view = second.permissions_view(second_chat).await.unwrap();
    second
        .apply_permission_change(
            second_chat,
            PermissionChange::UpsertRule {
                rule: project_rule("Write", Decision::Ask),
            },
            view.revision,
        )
        .await
        .unwrap();
    let mut events = first.subscribe();
    let run = first
        .submit(first_chat, "Write ask.txt".into())
        .await
        .unwrap();
    let request = loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::ToolStarted { .. } => panic!("stale policy admitted the operation"),
            EventKind::Completed { .. } | EventKind::Failed(_) => {
                panic!("stale policy skipped the explicit Ask")
            }
            _ => {}
        }
    };
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );
    assert!(
        model
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.structured_output.is_none())
    );
    assert!(!workdir.join("ask.txt").exists());
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn cross_process_project_ask_applies_to_host_tools() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("host-ask", "change_panel", json!({})),
        text("done"),
    ]));
    let host = Arc::new(AutomaticHostTool::default());
    let first = Agent::open_embedded(root.clone(), config(), model).unwrap();
    first.install_host_tools(host.clone()).unwrap();
    let second =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let first_chat = first.create_chat(workdir.clone()).await.unwrap();
    let second_chat = second.create_chat(workdir).await.unwrap();
    first
        .set_execution_mode(first_chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let view = second.permissions_view(second_chat).await.unwrap();
    second
        .apply_permission_change(
            second_chat,
            PermissionChange::UpsertRule {
                rule: project_rule("change_panel", Decision::Ask),
            },
            view.revision,
        )
        .await
        .unwrap();
    let mut events = first.subscribe();
    let run = first
        .submit(first_chat, "Change the panel".into())
        .await
        .unwrap();
    let request = loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::ToolStarted { .. } => panic!("stale policy admitted the Host Tool"),
            EventKind::Completed { .. } | EventKind::Failed(_) => {
                panic!("stale policy skipped the Host Tool Ask")
            }
            _ => {}
        }
    };
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );
    assert_eq!(host.executions.load(Ordering::SeqCst), 0);
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn cross_process_project_deny_invalidates_held_host_admission() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("held-host", "change_panel", json!({})),
        text("done"),
    ]));
    let gate = Arc::new(HostGate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let host = Arc::new(AutomaticHostTool {
        executions: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let first = Agent::open_embedded(root.clone(), config(), model).unwrap();
    first.install_host_tools(host.clone()).unwrap();
    let second =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let first_chat = first.create_chat(workdir.clone()).await.unwrap();
    let second_chat = second.create_chat(workdir).await.unwrap();
    first
        .set_execution_mode(first_chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = first.subscribe();
    let run = first
        .submit(first_chat, "Change the panel".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("Host Tool admission did not start");
    let view = second.permissions_view(second_chat).await.unwrap();
    second
        .apply_permission_change(
            second_chat,
            PermissionChange::UpsertRule {
                rule: project_rule("change_panel", Decision::Deny),
            },
            view.revision,
        )
        .await
        .unwrap();
    gate.release.notify_one();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolStarted { .. } => panic!("stale Host Tool approval was admitted"),
            EventKind::Completed { .. } | EventKind::Failed(_) => break,
            _ => {}
        }
    }
    assert_eq!(host.executions.load(Ordering::SeqCst), 0);
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn unreadable_project_rules_fail_closed_before_execution() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "malformed",
            "Write",
            json!({"file_path":"not-written.txt","content":"no"}),
        ),
        text("done"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    std::fs::write(root.join("settings.json"), b"{invalid JSON").unwrap();
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, "Write not-written.txt".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolStarted { .. } => panic!("malformed settings admitted the tool"),
            EventKind::Completed { .. } | EventKind::Failed(_) => break,
            _ => {}
        }
    }
    assert!(!workdir.join("not-written.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn unreadable_project_rules_during_local_approval_record_final_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "held-local",
            "Write",
            json!({"file_path":"held.txt","content":"must not write"}),
        ),
        text("done"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                persistent: false,
                ..project_rule("Write", Decision::Ask)
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let run = agent.submit(chat, "Write held.txt".into()).await.unwrap();
    let request = loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::ToolStarted { .. } => panic!("local operation ran before approval"),
            _ => {}
        }
    };
    std::fs::write(root.join("settings.json"), b"{invalid JSON").unwrap();
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolStarted { .. } => panic!("unreadable rules admitted the local tool"),
            EventKind::Completed { .. } | EventKind::Failed(_) => break,
            _ => {}
        }
    }
    assert!(!workdir.join("held.txt").exists());
    let records = agent.recent_denials(chat).await.unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].category, DenialCategory::FinalAdmission);
    assert_eq!(
        records[0].reason,
        "Project permission rules became unavailable before final admission"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn unreadable_project_rules_during_host_admission_record_final_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("held-host", "change_panel", json!({})),
        text("done"),
    ]));
    let gate = Arc::new(HostGate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let host = Arc::new(AutomaticHostTool {
        executions: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    agent.install_host_tools(host.clone()).unwrap();
    let chat = agent.create_chat(workdir).await.unwrap();
    let mut events = agent.subscribe();
    let run = agent.submit(chat, "Change the panel".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("Host Tool admission did not start");
    std::fs::write(root.join("settings.json"), b"{invalid JSON").unwrap();
    gate.release.notify_one();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolStarted { .. } => panic!("unreadable rules admitted the Host Tool"),
            EventKind::Completed { .. } | EventKind::Failed(_) => break,
            _ => {}
        }
    }
    assert_eq!(host.executions.load(Ordering::SeqCst), 0);
    let records = agent.recent_denials(chat).await.unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].category, DenialCategory::FinalAdmission);
    assert_eq!(
        records[0].reason,
        "Project permission rules became unavailable before final admission"
    );
    agent.shutdown().await;
}
