use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent::{
    host_tools::{HostCallResult, HostToolPermission, HostToolProvider, PreparedHostCall},
    permissions::{Decision, ExecutionMode, PermissionRule},
    protocol::{ChatId, ToolDefinition},
    runtime::{
        Agent, AgentHostOptions, EventKind, LoopState, OperationState, PermissionApprovalSource,
        PromptPolicy,
    },
    tools::ToolResult,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod support;
use support::{ScriptedModel, call, config, text};

#[derive(Default)]
struct WorkspaceTools {
    calls: Mutex<Vec<(ChatId, String, Value)>>,
}

struct ConfirmationTools(WorkspaceTools);

#[derive(Default)]
struct ValidatedTool {
    permission_checks: AtomicUsize,
    executions: AtomicUsize,
}

#[derive(Default)]
struct PermissionChangingTool {
    is_confirmation: std::sync::atomic::AtomicBool,
    executions: AtomicUsize,
}

#[derive(Default)]
struct ResolvedTargetTool {
    target: AtomicUsize,
    executed: Mutex<Vec<usize>>,
    uncertain: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl HostToolProvider for ResolvedTargetTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "change_current".into(),
            description: "Change the current target.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::Confirmation
    }

    async fn prepare_call(
        &self,
        _: ChatId,
        name: &str,
        input: &Value,
        _: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let target = self.target.load(Ordering::SeqCst);
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            json!({"target":target}),
            HostToolPermission::Confirmation,
            format!("Change target {target}"),
        )
        .with_payload(target))
    }

    async fn revalidate_call(
        &self,
        _: ChatId,
        call: &PreparedHostCall,
        _: CancellationToken,
    ) -> Result<(), String> {
        if call.payload::<usize>() == Some(&self.target.load(Ordering::SeqCst)) {
            Ok(())
        } else {
            Err("host target changed before execution".into())
        }
    }

    async fn execute_prepared(
        &self,
        _: ChatId,
        call: PreparedHostCall,
        _: CancellationToken,
    ) -> HostCallResult {
        let target = call.into_payload::<usize>().unwrap();
        self.executed.lock().unwrap().push(target);
        if self.uncertain.load(Ordering::SeqCst) {
            return HostCallResult::OutcomeUnknown("connection lost after dispatch".into());
        }
        HostCallResult::Completed(ToolResult {
            content: format!("changed {target}"),
            is_error: false,
            output_ref: None,
        })
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        panic!("the prepared operation must be dispatched")
    }
}

#[tokio::test]
async fn identical_host_calls_keep_distinct_resolved_targets_through_approval() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ResolvedTargetTool::default());
    provider.target.store(1, Ordering::SeqCst);
    let first = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("same", "change_current", json!({})),
            text("Finished."),
        ])),
    );
    first.install_host_tools(provider.clone()).unwrap();
    let first_chat = first.create_chat(directory.path().into()).await.unwrap();
    let mut first_events = first.subscribe();
    first
        .submit(first_chat, None, "Change current".into())
        .await
        .unwrap();
    let first_request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } =
                first_events.recv().await.unwrap().kind
            {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(first_request.input, json!({"target":1}));

    provider.target.store(2, Ordering::SeqCst);
    let second = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("same", "change_current", json!({})),
            text("Finished."),
        ])),
    );
    second.install_host_tools(provider.clone()).unwrap();
    let second_chat = second.create_chat(directory.path().into()).await.unwrap();
    let mut second_events = second.subscribe();
    second
        .submit(second_chat, None, "Change current".into())
        .await
        .unwrap();
    let second_request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } =
                second_events.recv().await.unwrap().kind
            {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(second_request.input, json!({"target":2}));

    second
        .answer_permission(second_chat, second_request.id, true)
        .await
        .unwrap();
    wait_for_completion(&second, second_chat).await;
    first
        .answer_permission(first_chat, first_request.id, true)
        .await
        .unwrap();
    wait_for_completion(&first, first_chat).await;

    assert_eq!(provider.executed.lock().unwrap().as_slice(), [2]);
    assert_eq!(
        first.snapshot(first_chat).await.unwrap().operations[0].input,
        json!({"target":1})
    );
    assert_eq!(
        second.snapshot(second_chat).await.unwrap().operations[0].input,
        json!({"target":2})
    );
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn dispatched_host_call_with_lost_response_keeps_unknown_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ResolvedTargetTool::default());
    provider.target.store(7, Ordering::SeqCst);
    provider.uncertain.store(true, Ordering::SeqCst);
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("uncertain", "change_current", json!({})),
            text("The outcome is unknown."),
        ])),
    );
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Change current".into())
        .await
        .unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    let operation = &agent.snapshot(chat).await.unwrap().operations[0];
    assert_eq!(operation.state, OperationState::Unknown);
    assert!(
        operation
            .result
            .as_ref()
            .unwrap()
            .content
            .contains("outcome unknown")
    );
    assert_eq!(provider.executed.lock().unwrap().as_slice(), [7]);
    agent.shutdown().await;
}

#[async_trait]
impl HostToolProvider for PermissionChangingTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "changing_tool".into(),
            description: "Change a resource.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        if self.is_confirmation.load(Ordering::SeqCst) {
            HostToolPermission::Confirmation
        } else {
            HostToolPermission::Automatic
        }
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            content: "changed".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[async_trait]
impl HostToolProvider for ValidatedTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "validated_tool".into(),
            description: "Accept one positive integer.".into(),
            input_schema: json!({"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"],"additionalProperties":false}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        self.permission_checks.fetch_add(1, Ordering::SeqCst);
        HostToolPermission::Automatic
    }

    async fn prepare(
        &self,
        chat_id: ChatId,
        name: &str,
        input: &Value,
        _: CancellationToken,
    ) -> Result<HostToolPermission, String> {
        if input["count"] == 1 {
            return Err("the resource is not ready".into());
        }
        Ok(self.permission(chat_id, name, input))
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            content: "executed".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[async_trait]
impl HostToolProvider for ConfirmationTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "close_panel".into(),
            description: "Close a Panel.".into(),
            input_schema: json!({"type":"object"}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::Confirmation
    }

    async fn execute(
        &self,
        chat_id: ChatId,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        self.0.execute(chat_id, name, input, cancel).await
    }
}

struct AutomaticTools(WorkspaceTools);

#[async_trait]
impl HostToolProvider for AutomaticTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "ModifyLayout".into(),
            description: "Modify the Workspace layout.".into(),
            input_schema: json!({"type":"object"}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::Automatic
    }

    async fn execute(
        &self,
        chat_id: ChatId,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        self.0.execute(chat_id, name, input, cancel).await
    }
}

#[async_trait]
impl HostToolProvider for WorkspaceTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "ReadWorkspace".into(),
            description: "Read the Workspace containing this Chat.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(
        &self,
        chat_id: ChatId,
        name: &str,
        input: Value,
        _: CancellationToken,
    ) -> ToolResult {
        self.calls
            .lock()
            .unwrap()
            .push((chat_id, name.into(), input));
        ToolResult {
            content: json!({"space":"research","ticks":["chat","quote"]}).to_string(),
            is_error: false,
            output_ref: None,
        }
    }
}

async fn wait_for_completion(agent: &Agent, chat: ChatId) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|loop_| loop_.state == LoopState::Completed)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_host_input_never_reaches_permission_or_execution() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("invalid-1", "validated_tool", json!({"count":"one"})),
        text("The input was invalid."),
    ]));
    let provider = Arc::new(ValidatedTool::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .submit(chat, None, "Run the tool".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.permission_checks.load(Ordering::SeqCst), 0);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    assert!(model.requests.lock().unwrap()[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_agent::model::Content::ToolResult { is_error: true, content, .. } if content.text().contains("invalid"))));
}

#[tokio::test]
async fn host_prepare_rejects_semantically_invalid_input_before_authorization() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("invalid-2", "validated_tool", json!({"count":1})),
        text("The resource was unavailable."),
    ]));
    let provider = Arc::new(ValidatedTool::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Run the tool".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.permission_checks.load(Ordering::SeqCst), 0);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    assert!(model.requests.lock().unwrap()[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_agent::model::Content::ToolResult { is_error: true, content, .. } if content.text().contains("not ready"))));
}

#[tokio::test]
async fn a_deny_rule_blocks_a_registered_host_tool() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("denied-1", "validated_tool", json!({"count":2})),
        text("The tool was denied."),
    ]));
    let provider = Arc::new(ValidatedTool::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Deny,
                tool: "validated_tool".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    agent
        .submit(chat, None, "Run the tool".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    assert!(model.requests.lock().unwrap()[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_agent::model::Content::ToolResult { is_error: true, content, .. } if content.text().contains("denied"))));
}

#[tokio::test]
async fn an_explicit_ask_requires_human_approval_before_an_automatic_host_tool_runs() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("host-1", "validated_tool", json!({"count":2})),
        text("The tool ran."),
    ]));
    let reviewer = Arc::new(ScriptedModel::new(Vec::new()));
    let provider = Arc::new(ValidatedTool::default());
    let agent = Agent::new_embedded(config(), model);
    agent.set_auto_reviewer(reviewer.clone()).await;
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
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
                tool: "validated_tool".into(),
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
        .submit(chat, None, "Run the tool".into())
        .await
        .unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } => return request,
                EventKind::ToolStarted { .. } => panic!("host tool started before approval"),
                EventKind::Completed { .. } => panic!("run completed without approval"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        request.approval_source,
        PermissionApprovalSource::ExplicitAsk
    );
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    assert!(reviewer.requests.lock().unwrap().is_empty());
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn headless_auto_denies_an_explicit_ask_for_an_automatic_host_tool() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("host-1", "validated_tool", json!({"count":2})),
        text("The tool was denied."),
    ]));
    let provider = Arc::new(ValidatedTool::default());
    let agent = Agent::new_with_options(
        config(),
        model,
        AgentHostOptions {
            execution_mode_override: Some(ExecutionMode::Auto),
            directory_trust: tickloop_agent::runtime::DirectoryTrustPolicy::CallerManaged,
            prompt_policy: PromptPolicy::DenyOnPrompt,
        },
    );
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "validated_tool".into(),
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
        .submit(chat, None, "Run the tool".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Completed { .. } => break,
                EventKind::PermissionRequested { .. } => panic!("headless run requested approval"),
                EventKind::ToolStarted { .. } => panic!("host tool ran without approval"),
                EventKind::Failed(error) => panic!("headless Ask failed the run: {error}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    agent.shutdown().await;
}

#[tokio::test]
async fn host_permission_change_during_approval_blocks_execution() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("changing-1", "changing_tool", json!({})),
        text("The resource was not changed."),
    ]));
    let provider = Arc::new(PermissionChangingTool::default());
    provider.is_confirmation.store(true, Ordering::SeqCst);
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();

    agent.submit(chat, None, "Change it".into()).await.unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                return request;
            }
        }
    })
    .await
    .unwrap();
    provider.is_confirmation.store(false, Ordering::SeqCst);
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelling_a_host_permission_request_prevents_execution() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![call(
        "cancelled-1",
        "validated_tool",
        json!({"count":2}),
    )]));
    let provider = Arc::new(ValidatedTool::default());
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "validated_tool".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Close it".into()).await.unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                return request;
            }
        }
    })
    .await
    .unwrap();

    agent.interrupt(chat).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(events.recv().await.unwrap().kind, EventKind::Interrupted) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        agent
            .answer_permission(chat, request.id, true)
            .await
            .is_err()
    );
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn registered_host_tool_runs_sequentially_across_model_turns() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("step-1", "validated_tool", json!({"count":2})),
        call("step-2", "validated_tool", json!({"count":3})),
        text("Both steps finished."),
    ]));
    let provider = Arc::new(ValidatedTool::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Run both steps".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_agent::model::Content::ToolResult { tool_use_id, .. } if tool_use_id == "step-1")));
    assert!(requests[2].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_agent::model::Content::ToolResult { tool_use_id, .. } if tool_use_id == "step-2")));
}

#[tokio::test]
async fn model_can_call_a_read_only_host_tool_and_receive_its_result() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("workspace-1", "ReadWorkspace", json!({})),
        text("The current Space contains Chat and Quote."),
    ]));
    let provider = Arc::new(WorkspaceTools::default());
    let agent = Agent::new(config(), model.clone());
    agent.install_host_tools(provider.clone()).unwrap();
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    agent
        .submit(chat, None, "What is in this Space?".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    let requests = model.requests.lock().unwrap();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "ReadWorkspace")
    );
    assert!(
        serde_json::to_string(&requests[1].messages)
            .unwrap()
            .contains("research")
    );
    assert_eq!(
        provider.calls.lock().unwrap().as_slice(),
        [(chat, "ReadWorkspace".into(), json!({}))]
    );
}

#[tokio::test]
async fn confirmation_host_tools_prompt_even_with_unrestricted_host_and_generic_allow_rule() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("close-1", "close_panel", json!({"panel_id":7})),
        text("Panel closed."),
    ]));
    let provider = Arc::new(ConfirmationTools(WorkspaceTools::default()));
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
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Allow,
                tool: "close_panel".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Close it".into()).await.unwrap();

    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                return request;
            }
        }
    })
    .await
    .unwrap();
    assert!(provider.0.calls.lock().unwrap().is_empty());
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(provider.0.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn full_access_still_requires_host_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("close", "close_panel", json!({"panel_id":7})),
        text("Panel closed."),
    ]));
    let provider = Arc::new(ConfirmationTools(WorkspaceTools::default()));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Close it".into()).await.unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } => return request,
                EventKind::ToolStarted { .. } => panic!("host confirmation was bypassed"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(provider.0.calls.lock().unwrap().is_empty());
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(provider.0.calls.lock().unwrap().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_mode_denies_mutating_host_tools_without_prompting() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("layout-1", "ModifyLayout", json!({})),
        text("No layout change was made."),
    ]));
    let provider = Arc::new(AutomaticTools(WorkspaceTools::default()));
    let agent = Agent::new(config(), model);
    agent.install_host_tools(provider.clone()).unwrap();
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();

    agent
        .submit(chat, None, "Rearrange the panels".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    assert!(provider.0.calls.lock().unwrap().is_empty());
    assert!(agent.snapshot(chat).await.unwrap().permissions.is_empty());
}
