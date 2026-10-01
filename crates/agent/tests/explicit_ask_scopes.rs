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
    permissions::{Decision, ExecutionMode, PermissionRule, PermissionScope},
    runtime::{Agent, AgentHostOptions, ChatId, EventKind, PermissionApprovalSource, PromptPolicy},
    tools::ToolResult,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
struct AutomaticHostTool {
    executions: AtomicUsize,
}

#[async_trait]
impl HostToolProvider for AutomaticHostTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "ModifyLayout".into(),
            description: "Modify the current layout".into(),
            input_schema: json!({"type":"object"}),
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
            content: "changed".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

fn explicit_ask(tool: &str) -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision: Decision::Ask,
        tool: tool.into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

#[tokio::test]
async fn explicit_ask_accepts_only_one_operation_and_keeps_request_pending_after_rejected_scopes() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "write",
            "Write",
            json!({"file_path":"report.txt","content":"approved"}),
        ),
        text("Done."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();

    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Write the report".into())
        .await
        .unwrap();
    let request = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::ToolStarted { .. } => panic!("operation started before approval"),
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    };
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );

    for scope in [PermissionScope::Session, PermissionScope::Persistent] {
        let error = agent
            .answer_permission_with_scope(chat, request.id, true, scope)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("explicit Ask"), "{error}");
        assert!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .permissions
                .iter()
                .any(|pending| pending.id == request.id),
            "the original question must remain answerable"
        );
        assert!(!directory.path().join("report.txt").exists());
    }

    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("approved run failed: {error}"),
            _ => {}
        }
    }
    assert_eq!(
        std::fs::read_to_string(directory.path().join("report.txt")).unwrap(),
        "approved"
    );
    assert_eq!(agent.snapshot(chat).await.unwrap().policy.rules.len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn unrestricted_host_still_asks_before_a_local_explicit_ask_operation() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "write",
            "Write",
            json!({"file_path":"report.txt","content":"approved"}),
        ),
        text("Done."),
    ]));
    let agent = Agent::new_with_options(
        config(),
        model,
        AgentHostOptions {
            prompt_policy: PromptPolicy::Unrestricted,
            ..AgentHostOptions::default()
        },
    );
    agent
        .trust_working_directory(directory.path().to_path_buf())
        .await
        .unwrap();
    let chat = agent
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();
    agent
        .set_permission_rules(chat, vec![explicit_ask("Write")])
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Write the report".into())
        .await
        .unwrap();
    let request = loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::ToolStarted { .. } | EventKind::Completed { .. } => {
                panic!("explicit Ask was bypassed")
            }
            EventKind::Failed(error) => panic!("run failed before approval: {error}"),
            _ => {}
        }
    };
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );
    assert!(!directory.path().join("report.txt").exists());
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == run.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert!(directory.path().join("report.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn unrestricted_host_still_asks_before_an_automatic_host_tool_explicit_ask() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("change", "ModifyLayout", json!({"panel":1})),
        text("Done."),
    ]));
    let provider = Arc::new(AutomaticHostTool::default());
    let agent = Agent::new_with_options(
        config(),
        model,
        AgentHostOptions {
            prompt_policy: PromptPolicy::Unrestricted,
            ..AgentHostOptions::default()
        },
    );
    agent.install_host_tools(provider.clone()).unwrap();
    agent
        .trust_working_directory(directory.path().to_path_buf())
        .await
        .unwrap();
    let chat = agent
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();
    agent
        .set_permission_rules(chat, vec![explicit_ask("ModifyLayout")])
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Change the layout".into())
        .await
        .unwrap();
    let request = loop {
        let event = next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::ToolStarted { .. } | EventKind::Completed { .. } => {
                panic!("explicit Ask was bypassed")
            }
            EventKind::Failed(error) => panic!("run failed before approval: {error}"),
            _ => {}
        }
    };
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == run.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    agent.shutdown().await;
}
