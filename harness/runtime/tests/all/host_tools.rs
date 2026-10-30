use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_harness_protocol::{
    AgentHostOptions, ChatId, Content, Decision, EventKind, ExecutionMode, LoopState,
    OperationState, PermissionApprovalSource, PermissionRule, PromptPolicy, ToolDefinition,
    ToolResult,
};
use tickloop_harness_runtime::{
    Agent,
    host_tools::{HostCallResult, HostToolPermission, HostToolProvider, PreparedHostCall},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_harness_test_support::{ScriptedModel, call, config, text};

mod guidance {
    use std::sync::{Arc, Mutex};
    use tickloop_harness_protocol::{ChatId, EventKind};
    use tickloop_harness_runtime::{
        Agent, HarnessOptions, HarnessSeams, ToolPolicy, environment::*,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, config, execution_environment::MemoryEnvironment, harness::ChosenModels,
        host_note_texts, next_event, text,
    };
    use tokio_util::sync::CancellationToken;

    const ACTIVE: &str = "Commands in this fixture use restricted writes.";
    const WITHDRAWN: &str = "The fixture's earlier execution guidance no longer applies.";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Response {
        Active,
        BindError,
        InspectError,
        Unsupported,
        WrongVariant,
        Withdrawn,
    }

    struct ControlledEnvironment {
        inner: MemoryEnvironment,
        state: Arc<Mutex<(Response, usize)>>,
    }

    struct ControlledPort {
        inner: Arc<dyn ExecutionPort>,
        state: Arc<Mutex<(Response, usize)>>,
    }

    impl ExecutionEnvironment for ControlledEnvironment {
        fn bind(
            &self,
            scope: ExecutionScope,
            authority: ExecutionAuthority,
        ) -> Result<Arc<dyn ExecutionPort>, ExecutionError> {
            if self.state.lock().unwrap().0 == Response::BindError {
                return Err(ExecutionError::new("Unavailable", "fixture bind failed"));
            }
            Ok(Arc::new(ControlledPort {
                inner: self.inner.bind(scope, authority)?,
                state: self.state.clone(),
            }))
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for ControlledPort {
        async fn inspect(
            &self,
            query: ExecutionQuery,
        ) -> Result<ExecutionInspection, ExecutionError> {
            if matches!(query, ExecutionQuery::Guidance) {
                let mut state = self.state.lock().unwrap();
                state.1 += 1;
                return match state.0 {
                    Response::Active | Response::Withdrawn => {
                        Ok(ExecutionInspection::Guidance(ExecutionGuidance {
                            content: (state.0 == Response::Active).then(|| ACTIVE.into()),
                            withdrawal: WITHDRAWN.into(),
                        }))
                    }
                    Response::InspectError => Err(ExecutionError::new(
                        "Unavailable",
                        "fixture inspection failed",
                    )),
                    Response::Unsupported => Err(ExecutionError::new(
                        "Unsupported",
                        "fixture has no guidance",
                    )),
                    Response::WrongVariant => Ok(ExecutionInspection::Instances(Vec::new())),
                    Response::BindError => panic!("a failed bind supplies no Port"),
                };
            }
            self.inner.inspect(query).await
        }

        async fn prepare(
            &self,
            intent: ExecutionIntent,
        ) -> Result<PreparedExecution, ExecutionError> {
            self.inner.prepare(intent).await
        }

        async fn execute(
            &self,
            prepared: PreparedExecution,
            admission: ExecutionAdmission,
            cancel: CancellationToken,
        ) -> ExecutionOutcome {
            self.inner.execute(prepared, admission, cancel).await
        }

        async fn observe(
            &self,
            request: ObserveRequest,
        ) -> Result<ExecutionObservation, ExecutionError> {
            self.inner.observe(request).await
        }
    }

    async fn run_turn(agent: &Agent, chat: ChatId) {
        let mut events = agent.subscribe();
        let receipt = agent.submit(chat, "Continue".into()).await.unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                EventKind::PermissionRequested { .. } => panic!("text-only turn needs no approval"),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn unconfirmed_environment_preserves_guidance_until_authoritative_withdrawal() {
        let responses = [
            Response::Active,
            Response::BindError,
            Response::InspectError,
            Response::Unsupported,
            Response::WrongVariant,
            Response::Active,
            Response::Withdrawn,
        ];
        let state = Arc::new(Mutex::new((Response::Active, 0)));
        let model = Arc::new(ScriptedModel::new(
            responses.iter().map(|_| text("Ready")).collect(),
        ));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(ControlledEnvironment {
                    inner: MemoryEnvironment::new(),
                    state: state.clone(),
                })),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions::local(config()),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        for response in responses {
            *state.lock().unwrap() = (response, 0);
            run_turn(&agent, chat).await;
            assert_eq!(
                state.lock().unwrap().1,
                usize::from(response != Response::BindError),
                "{response:?}"
            );
        }
        agent.shutdown().await;
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), responses.len());
        for (index, request) in requests.iter().enumerate() {
            let notes = host_note_texts(request).join("\n");
            assert_eq!(notes.matches(ACTIVE).count(), 1, "request {index}");
            assert_eq!(
                notes.matches(WITHDRAWN).count(),
                usize::from(index == responses.len() - 1),
                "request {index}"
            );
            assert_eq!(request.messages[..2], requests[0].messages[..2]);
            assert_eq!(request.tools, requests[0].tools);
            if index > 0 {
                let previous = &requests[index - 1].messages;
                assert_eq!(request.messages[..previous.len()], previous[..]);
            }
        }
    }

    #[tokio::test]
    async fn folderless_chat_has_no_bash_or_environment_guidance() {
        let model = Arc::new(ScriptedModel::new(vec![text("Ready")]));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: None,
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions {
                tools: ToolPolicy::durable_chat(),
                waits: tickloop_harness_runtime::WaitHosting::Durable,
                host: tickloop_harness_protocol::AgentHostOptions {
                    prompt_policy: tickloop_harness_protocol::PromptPolicy::DenyOnPrompt,
                    ..tickloop_harness_protocol::AgentHostOptions::product_defaults()
                },
                ..HarnessOptions::local(config())
            },
        )
        .unwrap();
        let chat = agent.create_chat_without_folder().await.unwrap();
        run_turn(&agent, chat).await;
        agent.shutdown().await;
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].tools.iter().any(|tool| tool.name == "Bash"));
        assert!(
            !host_note_texts(&requests[0])
                .iter()
                .any(|note| note.contains("Current execution environment guidance"))
        );
    }
}

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
    panics: std::sync::atomic::AtomicBool,
    panics_before_admission: std::sync::atomic::AtomicBool,
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
        assert!(
            !self.panics_before_admission.load(Ordering::SeqCst),
            "fixture-private-host-panic"
        );
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
        assert!(
            !self.panics.load(Ordering::SeqCst),
            "fixture-private-host-panic"
        );
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
        .submit(first_chat, "Change current".into())
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
        .submit(second_chat, "Change current".into())
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
    let model = Arc::new(ScriptedModel::new(vec![
        call("uncertain", "change_current", json!({})),
        text("The outcome is unknown."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Change current".into()).await.unwrap();
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
    // The unknown outcome answers the call where it was made; the system prompt names no
    // uncertain operation and stays byte for byte the same.
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        serde_json::to_vec(&requests[1].messages[0]).unwrap(),
        serde_json::to_vec(&requests[0].messages[0]).unwrap()
    );
    let system = serde_json::to_string(&requests[0].messages[0]).unwrap();
    assert!(!system.contains("uncertain_operations"));
    let call_at = requests[1]
        .messages
        .iter()
        .position(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolUse { id, .. } if id == "uncertain"))
        })
        .unwrap();
    assert!(matches!(
        requests[1].messages[call_at + 1].content.as_slice(),
        [Content::ToolResult { tool_use_id, content, is_error: true }]
            if tool_use_id == "uncertain" && content.text().contains("outcome unknown")
    ));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_host_panic_preserves_admission_and_does_not_repeat_the_operation() {
    for before_admission in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let provider = Arc::new(ResolvedTargetTool::default());
        provider.target.store(7, Ordering::SeqCst);
        provider.panics.store(true, Ordering::SeqCst);
        provider
            .panics_before_admission
            .store(before_admission, Ordering::SeqCst);
        let model = Arc::new(ScriptedModel::new(vec![call(
            "panicked",
            "change_current",
            json!({}),
        )]));
        let agent = Agent::new_embedded(config(), model.clone());
        agent.install_host_tools(provider.clone()).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, "Change current".into()).await.unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await.unwrap().kind {
                    EventKind::PermissionRequested { request } => {
                        assert!(!before_admission);
                        agent
                            .answer_permission(chat, request.id, true)
                            .await
                            .unwrap();
                    }
                    EventKind::Failed(error) => break error,
                    _ => {}
                }
            }
        })
        .await
        .expect("host panic left the run Running");
        assert!(error.contains("internal"), "{error}");
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.chat_loop.state, LoopState::Failed);
        if before_admission {
            assert!(snapshot.operations.is_empty());
        } else {
            assert_eq!(snapshot.operations[0].state, OperationState::Unknown);
        }
        assert_eq!(
            snapshot.current_operation_uncertainties().len(),
            usize::from(!before_admission)
        );
        assert_eq!(
            provider.executed.lock().unwrap().as_slice(),
            if before_admission { &[][..] } else { &[7][..] }
        );
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("fixture-private-host-panic")
        );
        agent.shutdown().await;
    }
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
            if agent.snapshot(chat).await.unwrap().chat_loop.state == LoopState::Idle {
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

    agent.submit(chat, "Run the tool".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.permission_checks.load(Ordering::SeqCst), 0);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    assert!(model.requests.lock().unwrap()[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_harness_protocol::Content::ToolResult { is_error: true, content, .. } if content.text().contains("invalid"))));
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
    agent.submit(chat, "Run the tool".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.permission_checks.load(Ordering::SeqCst), 0);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    assert!(model.requests.lock().unwrap()[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_harness_protocol::Content::ToolResult { is_error: true, content, .. } if content.text().contains("not ready"))));
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
    agent.submit(chat, "Run the tool".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
    assert!(model.requests.lock().unwrap()[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_harness_protocol::Content::ToolResult { is_error: true, content, .. } if content.text().contains("denied"))));
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
    agent.submit(chat, "Run the tool".into()).await.unwrap();
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
            directory_trust: tickloop_harness_protocol::DirectoryTrustPolicy::CallerManaged,
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
    agent.submit(chat, "Run the tool".into()).await.unwrap();
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

    agent.submit(chat, "Change it".into()).await.unwrap();
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
    agent.submit(chat, "Close it".into()).await.unwrap();
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
    agent.submit(chat, "Run both steps".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_harness_protocol::Content::ToolResult { tool_use_id, .. } if tool_use_id == "step-1")));
    assert!(requests[2].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_harness_protocol::Content::ToolResult { tool_use_id, .. } if tool_use_id == "step-2")));
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
        .submit(chat, "What is in this Space?".into())
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
    agent.submit(chat, "Close it".into()).await.unwrap();

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
    agent.submit(chat, "Close it".into()).await.unwrap();
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
        .submit(chat, "Rearrange the panels".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    assert!(provider.0.calls.lock().unwrap().is_empty());
    assert!(agent.snapshot(chat).await.unwrap().permissions.is_empty());
}

#[tokio::test]
async fn multiple_host_sources_keep_their_tools_and_route_calls() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("one", "ReadWorkspace", json!({})),
        call("two", "ModifyLayout", json!({})),
        text("done"),
    ]));
    let read = Arc::new(WorkspaceTools::default());
    let write = Arc::new(AutomaticTools(WorkspaceTools::default()));
    let agent = Agent::new(config(), model.clone());
    agent
        .install_host_tools(vec![
            read.clone() as Arc<dyn HostToolProvider>,
            write.clone(),
        ])
        .unwrap();
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, "Read and change the layout".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(read.calls.lock().unwrap().len(), 1);
    assert_eq!(write.0.calls.lock().unwrap().len(), 1);
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "ReadWorkspace")
        );
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "ModifyLayout")
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn composed_host_sources_allow_delegation_without_available_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Return the child report", "name":"peer", "run_in_background":false}),
        ),
        text("child report"),
        text("done"),
    ]));
    // The call returns the child's report only on a non-interactive host, where it may wait.
    let agent = Agent::new_embedded_with_subagent_launch(
        config(),
        model,
        tickloop_harness_runtime::SubagentLaunch::BackgroundByDefault,
    );
    agent
        .install_host_tools(vec![
            Arc::new(WorkspaceTools::default()) as Arc<dyn HostToolProvider>,
            Arc::new(AutomaticTools(WorkspaceTools::default())),
        ])
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .submit(chat, "Delegate the bounded report".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot.operations[0].result.as_ref().unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("child report"),
        "{}",
        result.content
    );
    agent.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_host_refresh_leaves_other_chat_submission_status_and_stop_responsive() {
    struct BlockedProvider {
        entered: tokio::sync::Notify,
        release: CancellationToken,
    }
    #[async_trait]
    impl HostToolProvider for BlockedProvider {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        async fn refresh(&self, cancel: CancellationToken) -> Result<(), String> {
            self.entered.notify_one();
            tokio::select! { _ = self.release.cancelled() => {}, _ = cancel.cancelled() => {} }
            Ok(())
        }
        fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }
        async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
            unreachable!()
        }
    }
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(BlockedProvider {
        entered: tokio::sync::Notify::new(),
        release: CancellationToken::new(),
    });
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![text("done"), text("done")])),
    );
    agent.install_host_tools(provider.clone()).unwrap();
    let first = agent.create_chat(root.path().into()).await.unwrap();
    let second = agent.create_chat(root.path().into()).await.unwrap();
    agent.submit(first, "wait".into()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.entered.notified(),
    )
    .await
    .unwrap();
    let responsive = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        agent.submit(second, "hello".into()).await.unwrap();
        agent.snapshot(second).await.unwrap();
        agent.stop(second).await.unwrap();
    })
    .await;
    assert!(!provider.release.is_cancelled());
    provider.release.cancel();
    agent.shutdown().await;
    responsive.expect("another Chat must remain responsive while refreshing host tools");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_host_catalog_capture_does_not_hold_the_chats_lock() {
    // Only the first refresh's capture blocks: the other Chat must be able to finish its own
    // preparation, or `/stop` would wait for a Run stuck in the same capture. The capture
    // blocks in place so that tasks queued on its worker thread still run elsewhere.
    struct BlockedCatalog {
        armed: std::sync::atomic::AtomicBool,
        block: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        gate: (Mutex<bool>, std::sync::Condvar),
    }
    #[async_trait]
    impl HostToolProvider for BlockedCatalog {
        fn definitions(&self) -> Vec<ToolDefinition> {
            if self.block.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                tokio::task::block_in_place(|| {
                    let (lock, changed) = &self.gate;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = changed.wait(released).unwrap();
                    }
                });
            }
            Vec::new()
        }
        async fn refresh(&self, _: CancellationToken) -> Result<(), String> {
            if !self.armed.swap(true, Ordering::SeqCst) {
                self.block.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
        fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }
        async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
            unreachable!()
        }
    }
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(BlockedCatalog {
        armed: std::sync::atomic::AtomicBool::new(false),
        block: std::sync::atomic::AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        gate: (Mutex::new(false), std::sync::Condvar::new()),
    });
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![text("done"), text("done")])),
    );
    agent.install_host_tools(provider.clone()).unwrap();
    let first = agent.create_chat(root.path().into()).await.unwrap();
    let second = agent.create_chat(root.path().into()).await.unwrap();
    agent.submit(first, "wait".into()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.entered.notified(),
    )
    .await
    .unwrap();
    let responsive = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        agent.submit(second, "hello".into()).await.unwrap();
        agent.snapshot(second).await.unwrap();
        agent.stop(second).await.unwrap();
    })
    .await;
    assert!(!*provider.gate.0.lock().unwrap());
    *provider.gate.0.lock().unwrap() = true;
    provider.gate.1.notify_all();
    agent.shutdown().await;
    responsive.expect("a blocked catalog capture must not hold the Chats lock");
}

#[derive(Default)]
struct OptionalFieldsTool {
    executed: Mutex<Vec<Value>>,
}

#[async_trait]
impl HostToolProvider for OptionalFieldsTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "search_notes".into(),
            description: "Search notes.".into(),
            input_schema: json!({"type":"object","properties":{
                "query":{"type":"string","minLength":1},
                "scope":{"type":"string","enum":["all","open"]},
                "limit":{"type":"integer","minimum":1},
                "label":{"type":"string","minLength":1},
                "note":{"type":"string"}
            },"required":["query"],"additionalProperties":false}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(&self, _: ChatId, _: &str, input: Value, _: CancellationToken) -> ToolResult {
        self.executed.lock().unwrap().push(input);
        ToolResult {
            content: "found".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

/// Models fill every property of a flat schema: an optional property left `null` or `""` where
/// its schema cannot take that value was not given, so the call runs without it. Required
/// properties and values the schema accepts are still judged as written.
#[tokio::test]
async fn optional_host_tool_fields_left_empty_are_not_given() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "filled",
            "search_notes",
            json!({"query":"q","scope":"","limit":null,"label":"","note":""}),
        ),
        call("empty-query", "search_notes", json!({"query":""})),
        call(
            "bad-scope",
            "search_notes",
            json!({"query":"q","scope":"closed"}),
        ),
        text("done"),
    ]));
    let provider = Arc::new(OptionalFieldsTool::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.submit(chat, "Search".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;

    assert_eq!(
        provider.executed.lock().unwrap().as_slice(),
        [json!({"query":"q","note":""})]
    );
    let requests = model.requests.lock().unwrap();
    let result = |id: &str| {
        requests
            .last()
            .unwrap()
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == id => Some((content.text(), *is_error)),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(result("filled"), ("found".into(), false));
    assert_eq!(
        result("empty-query"),
        (
            "invalid search_notes input: input.query is too short".into(),
            true
        )
    );
    assert_eq!(
        result("bad-scope"),
        (
            "invalid search_notes input: input.scope has an invalid value; expected one of \"all\", \"open\"".into(),
            true
        )
    );
}
