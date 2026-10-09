use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    ArtifactTick, ChatId, Content, EventKind, ExecutionMode, HostCallContext, HostEndpointIdentity,
    LoopId, LoopState, ModelIdentity, OperationState, ToolDefinition, ToolResult,
};
use tickloop_agent_runtime::{
    Agent,
    host_tools::{
        HostToolPermission, HostToolProvider, PreparedHostCall, WorkspaceHostContext,
        WorkspaceHostName, WorkspaceIntegration,
    },
    mcp_tools::McpHostToolProvider,
};
use tokio_util::sync::CancellationToken;

use tickloop_agent_test_support::{ScriptedModel, call, config, text};

#[path = "support/artifact_service.rs"]
mod artifact_service;
use artifact_service::{ArtifactService, CREATED_ARTIFACT};

const WAIT: Duration = Duration::from_secs(15);

fn current_host_state(request: &ModelRequest) -> Value {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            Content::Text { text } => text
                .rsplit_once("Current host state: ")
                .map(|(_, state)| serde_json::from_str(state).unwrap()),
            _ => None,
        })
        .unwrap()
}

fn tool_names(request: &ModelRequest) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

fn artifact_tick(service: &ArtifactService, artifact_id: &str) -> ArtifactTick {
    ArtifactTick {
        artifact_id: artifact_id.into(),
        origin: service.origin.clone(),
        title: "Fixture".into(),
        format: "markdown".into(),
        url: format!("{}/a/{artifact_id}", service.origin),
        access_role: "owner".into(),
        added_at_ms: 1,
    }
}

async fn signed_in_agent(
    service: &ArtifactService,
    directory: &std::path::Path,
    model: Arc<dyn ModelClient>,
) -> Agent {
    let account = service.login(directory).await;
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    agent
}

async fn eventually(mut condition: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition was not reached");
}

async fn wait_for_loop_state(agent: &Agent, chat: ChatId, loop_id: LoopId, state: LoopState) {
    eventually(async || {
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .any(|goal| goal.id == loop_id && goal.state == state)
    })
    .await;
}

async fn wait_for_completion(agent: &Agent, chat: ChatId) {
    eventually(async || {
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .is_some_and(|goal| goal.state == LoopState::Completed)
    })
    .await;
}

async fn next_permission(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) -> tickloop_agent_protocol::PermissionRequest {
    loop {
        if let EventKind::PermissionRequested { request } =
            tickloop_agent_test_support::next_event(events).await.kind
        {
            return request;
        }
    }
}

/// Links the fixture artifact to a fresh Chat that runs feedback without approval prompts.
async fn linked_chat(
    service: &ArtifactService,
    directory: &std::path::Path,
    model: Arc<dyn ModelClient>,
) -> (Agent, ChatId, LoopId) {
    let agent = signed_in_agent(service, directory, model).await;
    let chat = agent.create_chat(directory.into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .link_artifact(chat, &artifact_tick(service, "doc"))
        .await
        .unwrap();
    let loop_id = agent.snapshot(chat).await.unwrap().loops[0].id;
    (agent, chat, loop_id)
}

#[tokio::test]
async fn directory_presents_exactly_the_three_artifact_tools_as_built_ins() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider
            .definitions()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["ArtifactComment", "ArtifactEdit", "ArtifactRead"]
    );
    let chat = uuid::Uuid::new_v4();
    assert_eq!(
        provider.permission(chat, "ArtifactRead", &json!({"action":"read"})),
        HostToolPermission::ReadOnly
    );
    for name in ["ArtifactEdit", "ArtifactComment"] {
        assert_eq!(
            provider.permission(chat, name, &json!({"action":"reply"})),
            HostToolPermission::Reviewable
        );
    }
    assert_eq!(provider.negotiated_version().as_deref(), Some("2026-07-28"));

    let model = Arc::new(ScriptedModel::new(vec![text("Ready.")]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.submit(chat, None, "Inspect".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    let names = tool_names(&requests[0]);
    for name in ["ArtifactRead", "ArtifactEdit", "ArtifactComment"] {
        assert!(names.contains(&name), "{name}");
    }
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("mcp__") || *name == "unrelated_remote_tool"),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_server_other_than_tickloop_docs_provides_no_tools() {
    let service = ArtifactService::start().await;
    *service.control.server_name.lock().unwrap() = Some("tickloop-artifacts".into());
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "unavailable"
    );
}

#[tokio::test]
async fn a_directory_without_all_three_artifact_tools_provides_none() {
    let service = ArtifactService::start().await;
    service
        .control
        .omit_comment_tool
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "unavailable"
    );
}

#[tokio::test]
async fn the_host_reserves_the_artifact_tool_names() {
    struct Impostor;
    #[async_trait]
    impl HostToolProvider for Impostor {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "ArtifactRead".into(),
                description: "Not the Artifact Service".into(),
                input_schema: json!({"type":"object"}),
            }]
        }
        fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }
        async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
            unreachable!()
        }
    }
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    assert!(agent.install_host_tools(Arc::new(Impostor)).is_err());

    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = Arc::new(McpHostToolProvider::new(account, Some(Arc::new(Impostor))));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let reads = provider
        .definitions()
        .into_iter()
        .filter(|tool| tool.name == "ArtifactRead")
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 1);
    assert!(
        reads[0]
            .description
            .starts_with("Read Tick Loop Docs you can access."),
        "only the service directory provides an Artifact name"
    );
    agent
        .install_host_tools(provider)
        .expect("the Artifact provider keeps its own names after refresh");
}

#[tokio::test]
async fn reading_runs_without_approval_while_edits_ask_under_their_own_name() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "read",
            "ArtifactRead",
            json!({"action":"read","artifact_id":"doc"}),
        ),
        call(
            "patch",
            "ArtifactEdit",
            json!({"action":"patch","artifact_id":"doc","patches":[{"type":"replace","target":"b1","expected_hash":"h1","markdown":"# Tighter"}]}),
        ),
        text("Edited."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Tighten it".into()).await.unwrap();
    let permission = next_permission(&mut events).await;
    assert_eq!(permission.name, "ArtifactEdit");
    assert_eq!(
        service
            .tool_calls()
            .iter()
            .map(|call| call["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ["ArtifactRead"],
        "the read ran without approval and the edit waited"
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let calls = service.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1]["name"], "ArtifactEdit");
    assert!(
        calls
            .iter()
            .all(|call| call["_meta"]["tickloop.ai/event-bridge"].is_null())
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .operations
            .iter()
            .any(|operation| operation.name == "ArtifactEdit"
                && operation.state == OperationState::Completed)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_mode_denies_artifact_writes_and_keeps_reads() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "read",
            "ArtifactRead",
            json!({"action":"read","artifact_id":"doc"}),
        ),
        call(
            "reply",
            "ArtifactComment",
            json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Planned"}),
        ),
        text("Planned."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Plan a reply".into())
        .await
        .unwrap();
    let reply = loop {
        if let EventKind::ToolCompleted { call_id, result } =
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
            && call_id == "reply"
        {
            break result;
        }
    };
    assert!(reply.is_error, "{}", reply.content);
    assert_eq!(
        service
            .tool_calls()
            .iter()
            .map(|call| call["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ["ArtifactRead"]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn delegated_chat_inherits_the_artifact_tools() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Inspect available Artifact tools"}),
        ),
        text("Child inspected tools."),
        text("Done."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Delegate tool inspection".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests[..2] {
        assert!(
            tool_names(request).contains(&"ArtifactEdit"),
            "Artifact tools missing from parent or delegated child"
        );
        let host = current_host_state(request);
        assert_eq!(
            host["service_connections"]["artifacts"]["authentication"],
            "signed_in"
        );
        assert_eq!(
            host["service_connections"]["artifacts"]["email"],
            "one@example.test"
        );
        assert!(
            !serde_json::to_string(&request.messages)
                .unwrap()
                .contains("test-service-token")
        );
    }
}

#[tokio::test]
async fn delegated_artifact_calls_obey_frozen_allow_ask_and_deny() {
    use tickloop_agent_protocol::{Decision, PermissionRule};
    for decision in [Decision::Allow, Decision::Ask, Decision::Deny] {
        let service = ArtifactService::start().await;
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "delegate",
                "Subagent",
                json!({"task":"Reply in the fixture thread"}),
            ),
            call(
                "child-reply",
                "ArtifactComment",
                json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Child"}),
            ),
            text("Child done."),
            text("Parent done."),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let parent = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(parent, ExecutionMode::FullAccess)
            .await
            .unwrap();
        agent
            .set_permission_rules(
                parent,
                vec![PermissionRule {
                    id: uuid::Uuid::new_v4(),
                    decision,
                    tool: "ArtifactComment".into(),
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
            .submit(parent, None, "Delegate the reply".into())
            .await
            .unwrap();
        let mut asked = 0;
        let mut child_result = None;
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    assert_ne!(event.chat_id, parent);
                    assert_eq!(request.name, "ArtifactComment");
                    asked += 1;
                    agent
                        .answer_permission(event.chat_id, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted { call_id, result } if call_id == "child-reply" => {
                    child_result = Some(result)
                }
                EventKind::Completed { .. } if event.chat_id == parent => break,
                _ => {}
            }
        }
        assert_eq!(asked, usize::from(decision == Decision::Ask));
        assert_eq!(child_result.unwrap().is_error, decision == Decision::Deny);
        assert_eq!(
            service.tool_calls().len(),
            usize::from(decision != Decision::Deny)
        );
        agent.shutdown().await;
    }
}

struct GatedDelegatedModel {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    child_turns: AtomicUsize,
    parent_turns: AtomicUsize,
}

#[async_trait]
impl ModelClient for GatedDelegatedModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let host = current_host_state(&request);
        let content = if host["delegation"].is_object() {
            if self.child_turns.fetch_add(1, Ordering::SeqCst) == 0 {
                assert!(tool_names(&request).contains(&"ArtifactEdit"));
                self.entered.notify_one();
                self.release.notified().await;
                call(
                    "frozen-write",
                    "ArtifactEdit",
                    json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"# Child"}),
                )
            } else {
                text("Child done.")
            }
        } else if self.parent_turns.fetch_add(1, Ordering::SeqCst) == 0 {
            call(
                "delegate",
                "Subagent",
                json!({"task":"Edit the fixture under inherited authority"}),
            )
        } else {
            text("Parent done.")
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn delegated_artifact_authority_cannot_expand_after_parent_policy_or_account_changes() {
    use tickloop_agent_protocol::{Decision, PermissionRule};
    for change_account in [false, true] {
        let service = ArtifactService::start().await;
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(GatedDelegatedModel {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            child_turns: AtomicUsize::new(0),
            parent_turns: AtomicUsize::new(0),
        });
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let parent = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(parent, ExecutionMode::FullAccess)
            .await
            .unwrap();
        let rules = |decision| {
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision,
                tool: "ArtifactEdit".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }]
        };
        if !change_account {
            agent
                .set_permission_rules(parent, rules(Decision::Deny))
                .await
                .unwrap();
        }
        let mut events = agent.subscribe();
        agent
            .submit(parent, None, "Delegate the edit".into())
            .await
            .unwrap();
        tokio::time::timeout(WAIT, model.entered.notified())
            .await
            .unwrap();
        if change_account {
            service.control.other_account.store(true, Ordering::SeqCst);
            service.login(directory.path()).await;
        } else {
            agent
                .set_permission_rules(parent, rules(Decision::Allow))
                .await
                .unwrap();
        }
        model.release.notify_one();
        let result = loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            if let EventKind::ToolCompleted { call_id, result } = event.kind
                && call_id == "frozen-write"
            {
                break result;
            }
        };
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains(if change_account {
                "account is outside"
            } else {
                "ceiling denies"
            }),
            "{}",
            result.content
        );
        wait_for_completion(&agent, parent).await;
        assert!(service.tool_calls().is_empty());
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn artifact_login_state_is_independent_of_directory_and_connection_failures() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(service.client(directory.path()), None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out"
    );
    service.login(directory.path()).await;
    service
        .control
        .directory_unavailable
        .store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "unavailable"
    );
    service
        .control
        .session_unavailable
        .store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    service
        .control
        .session_unavailable
        .store(false, Ordering::SeqCst);
    service
        .control
        .directory_unavailable
        .store(false, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "available"
    );
}

#[tokio::test]
async fn model_tool_inventory_keeps_artifact_shell_and_herdr_names() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let native = tickloop_agent_runtime::native_host::NativeHostToolProvider::new(Arc::new(
        tickhost_connect::access::TickAccess::new(
            tickhost_connect::herdr::HerdrContext::without_view(
                directory.path().join("unused.sock"),
            ),
        ),
    ));
    let model = Arc::new(ScriptedModel::new(vec![text("Ready.")]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            account,
            Some(Arc::new(native)),
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Inspect native tool inventory".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    for name in [
        "ArtifactRead",
        "ArtifactEdit",
        "ArtifactComment",
        "Bash",
        "Read",
        "TaskStop",
        "CallTickTool",
        "ReadTick",
    ] {
        assert!(tool_names(&requests[0]).contains(&name), "{name}");
    }
    let prompt = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(prompt.contains("Tool absence is not evidence of being signed out"));
    assert!(
        prompt.contains("Operate artifacts with ArtifactRead, ArtifactEdit and ArtifactComment")
    );
    for name in ["CallTickTool", "ReadTick"] {
        let definition = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap();
        assert!(definition.description.contains("Herdr"));
        assert!(definition.description.contains("ArtifactRead"));
    }
}

#[tokio::test]
async fn login_reveals_the_tools_and_an_approved_edit_is_sent_once() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("Sign in first."),
        call(
            "replace",
            "ArtifactEdit",
            json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"# Private"}),
        ),
        text("Replaced."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.submit(chat, None, "Check".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(
        !tool_names(&model.requests.lock().unwrap()[0]).contains(&"ArtifactEdit"),
        "tools appear only after login"
    );
    service.login(directory.path()).await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Replace the document".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    assert!(
        !permission.input.to_string().contains("# Private"),
        "the approval shows a digest of the body"
    );
    assert!(service.tool_calls().is_empty());
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let calls = service
        .requests()
        .into_iter()
        .filter(|request| request["body"]["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["body"]["params"]["name"], "ArtifactEdit");
    assert_eq!(
        calls[0]["body"]["params"]["arguments"]["content"],
        "# Private"
    );
    assert!(
        calls[0]["headers"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer test-service-token-0")
    );
    let model_messages =
        serde_json::to_string(&model.requests.lock().unwrap()[2].messages).unwrap();
    assert!(model_messages.contains(r#"\"revision\":4"#));
    assert!(!model_messages.contains("test-service-token"));
    agent.shutdown().await;
}

#[tokio::test]
async fn logout_after_an_approval_request_prevents_dispatch_and_hides_the_tools() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "replace",
            "ArtifactEdit",
            json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"private"}),
        ),
        text("Stopped."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Replace".into()).await.unwrap();
    let permission = next_permission(&mut events).await;
    service.client(directory.path()).logout().await.unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(service.tool_calls().is_empty());
    assert!(!tool_names(&model.requests.lock().unwrap()[1]).contains(&"ArtifactEdit"));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_changed_directory_rejects_a_prepared_write() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let chat = ChatId::new_v4();
    let prepared = provider
        .prepare_call(
            chat,
            "ArtifactEdit",
            &json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"initial"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    service
        .control
        .directory_revision
        .store(1, Ordering::SeqCst);
    assert!(
        provider
            .revalidate_call(chat, &prepared, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(service.tool_calls().is_empty());
}

#[tokio::test]
async fn a_lost_call_response_is_unknown_and_never_replayed() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service
        .control
        .drop_call_response
        .store(true, Ordering::SeqCst);
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "replace",
            "ArtifactEdit",
            json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"do not replay"}),
        ),
        text("The outcome needs checking."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Replace".into()).await.unwrap();
    let permission = next_permission(&mut events).await;
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let operation = &agent.snapshot(chat).await.unwrap().operations[0];
    assert_eq!(operation.state, OperationState::Unknown);
    let content = &operation.result.as_ref().unwrap().content;
    assert!(content.contains("outcome unknown"), "{content}");
    assert!(content.contains("read the artifact again"), "{content}");
    assert_eq!(service.tool_calls().len(), 1);
    agent.shutdown().await;
}

struct NativeFixture;

#[async_trait]
impl HostToolProvider for NativeFixture {
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        Ok(Some(HostEndpointIdentity::TrustedAdapter {
            scope: uuid::Uuid::nil(),
        }))
    }

    async fn prepare_call_in_context(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        Ok(self
            .prepare_call(chat_id, name, input, cancel)
            .await?
            .with_payload(context.clone()))
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "native_fixture".into(),
            description: "Native tool".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }]
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        Some(WorkspaceHostContext::attached_herdr(None, None))
    }

    fn permission(
        &self,
        _: tickloop_agent_protocol::ChatId,
        _: &str,
        _: &Value,
    ) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(
        &self,
        _: tickloop_agent_protocol::ChatId,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> ToolResult {
        ToolResult {
            content: "native".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn native_tools_and_workspace_metadata_survive_mcp_composition() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(
        service.client(directory.path()),
        Some(Arc::new(NativeFixture)),
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider
            .definitions()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["native_fixture"]
    );
    let context = provider.workspace_host_context().unwrap();
    assert_eq!(context.name, WorkspaceHostName::Herdr);
    assert_eq!(context.integration, WorkspaceIntegration::Available);
    assert_eq!(
        provider.host_endpoint().unwrap(),
        NativeFixture.host_endpoint().unwrap()
    );
    let context = HostCallContext {
        workdir: directory.path().into(),
        workspace_id: Some("work-context".into()),
        interaction: None,
    };
    let prepared = provider
        .prepare_call_in_context(
            uuid::Uuid::new_v4(),
            &context,
            "native_fixture",
            &json!({}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let captured = prepared
        .payload::<HostCallContext>()
        .expect("Native preparation receives its context");
    assert_eq!(captured.workdir, context.workdir);
    assert_eq!(captured.workspace_id, context.workspace_id);
}

#[tokio::test]
async fn remote_unauthorized_hides_tools_and_invalidates_local_login() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(!provider.definitions().is_empty());
    service.control.reject_mcp.store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert!(
        service
            .client(directory.path())
            .status()
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn stale_session_401_never_reports_a_replaced_login_as_signed_out() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = Arc::new(McpHostToolProvider::new(account, None));
    service.control.signed_out.store(true, Ordering::SeqCst);
    service
        .control
        .hold_session_response
        .store(true, Ordering::SeqCst);
    let running = provider.clone();
    let refresh =
        tokio::spawn(async move { running.refresh(CancellationToken::new()).await.unwrap() });
    tokio::time::timeout(WAIT, service.control.session_response_started.notified())
        .await
        .unwrap();
    service
        .control
        .credential_revision
        .store(1, Ordering::SeqCst);
    let account = service.login(directory.path()).await;
    service.control.release_session_response.notify_one();
    refresh.await.unwrap();
    assert!(
        account.status().await.unwrap().is_some(),
        "the new login is valid and retained"
    );
    assert_ne!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out",
        "a stale 401 response labelled the valid newer login signed_out"
    );
}

#[tokio::test]
async fn account_status_cache_requires_the_current_credential_generation() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(service.client(directory.path()), None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out"
    );
    service.login(directory.path()).await;
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    service
        .control
        .credential_revision
        .store(1, Ordering::SeqCst);
    service.login(directory.path()).await;
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
}

#[tokio::test]
async fn linking_puts_the_account_link_and_claims_a_pending_request_into_its_loop() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "reply",
            "ArtifactComment",
            json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Tightened."}),
        ),
        call(
            "done",
            "ArtifactComment",
            json!({"action":"done","artifact_id":"doc","outcome":"done","summary":"Tightened the introduction."}),
        ),
        text("Handled."),
    ]));
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    let link = service.link("doc").expect("the link was put");
    let put = &service.lifecycle_requests("PUT /api/v1/agent/links/doc")[0];
    assert_eq!(put["body"]["chat_id"], chat.to_string());
    assert_eq!(put["body"]["loop_id"], loop_id.to_string());
    assert_eq!(link.epoch, 1);
    eventually(async || service.finished() == [("request-1".into(), "done".into())]).await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;

    let calls = service.tool_calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        let context = &call["_meta"]["tickloop.ai/event-bridge"];
        assert_eq!(context["version"], 1);
        assert_eq!(context["source_key"], "doc");
        assert_eq!(context["binding_id"], link.binding_id);
        assert_eq!(context["association_epoch"], 1);
        assert_eq!(context["event_id"], "request-1");
        assert_eq!(context["claim_id"], "claim-request-1");
        assert_eq!(context["fencing_token"], "fence-request-1");
    }
    let conversations = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| serde_json::to_string(&request.messages).unwrap())
        .collect::<Vec<_>>();
    assert!(conversations[0].contains("@tickloop request request-1 on artifact doc"));
    assert!(conversations[0].contains("tighten the introduction"));
    assert!(
        conversations
            .iter()
            .all(|conversation| !conversation.contains("fence-request-1"))
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    let binding = &snapshot.loops[0].event_sources[0];
    assert_eq!(binding.artifact_id, "doc");
    assert_eq!(binding.account_id, "user-one");
    assert!(!binding.resume_required);
    assert_eq!(
        binding.last_request_at_ms,
        Some(artifact_service::REQUEST_CREATED_AT_MS)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        service.releases().is_empty(),
        "a done request is never released"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_run_that_ends_without_done_fails_its_request() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(ScriptedModel::new(vec![text("I looked at it.")]));
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model).await;
    eventually(async || service.releases() == [("request-1".into(), "failed".into())]).await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    assert!(!service.is_pending("request-1"));
    agent.shutdown().await;
}

struct BlockingModel {
    started: tokio::sync::Notify,
    turns: AtomicUsize,
}

#[async_trait]
impl ModelClient for BlockingModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if self.turns.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            cancel.cancelled().await;
            return Err(ModelError::Cancelled);
        }
        Ok(ModelOutput {
            private_source: None,
            content: text("Second attempt."),
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn an_interrupted_run_returns_its_request_and_continue_claims_it_again() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
    });
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    eventually(async || service.releases() == [("request-1".into(), "retry".into())]).await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::Interrupted).await;
    assert!(service.is_pending("request-1"));
    assert!(
        agent.snapshot(chat).await.unwrap().loops[0].event_sources[0].resume_required,
        "the interrupted Loop shows its feedback as paused"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        model.turns.load(Ordering::SeqCst),
        1,
        "an interrupted Loop does not listen until Continue"
    );
    agent
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.loops.len(),
        1,
        "Continue keeps the link in its Loop"
    );
    assert!(!snapshot.loops[0].event_sources[0].resume_required);
    eventually(async || model.turns.load(Ordering::SeqCst) == 2).await;
    eventually(async || service.releases().last() == Some(&("request-1".into(), "failed".into())))
        .await;
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_the_loop_hands_its_claimed_request_back_once() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
    });
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    agent.stop(chat, loop_id).await.unwrap();
    eventually(async || !service.releases().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(service.releases(), [("request-1".into(), "retry".into())]);
    assert!(service.is_pending("request-1"));
    agent.shutdown().await;
}

#[tokio::test]
async fn restart_waits_for_explicit_continue_which_links_again_and_claims_pending_requests() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    service.login(directory.path()).await;
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("Ready.")])),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.submit(chat, None, "Get ready".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let first_binding = service.link("doc").unwrap().binding_id;
    agent.shutdown().await;
    drop(agent);

    service.request_feedback("doc", "request-1");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "done",
            "ArtifactComment",
            json!({"action":"done","artifact_id":"doc","outcome":"done"}),
        ),
        text("Handled after restart."),
    ]));
    let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
    restored
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    restored
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let goal = &snapshot.loops[0];
    assert_eq!(goal.state, LoopState::Interrupted);
    assert!(goal.event_sources[0].resume_required);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        service.is_pending("request-1"),
        "history loading never listens"
    );
    restored
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let continued = restored.snapshot(chat).await.unwrap();
    assert_eq!(continued.loops.len(), 1);
    assert_eq!(continued.loops[0].id, goal.id);
    let renewed = service.link("doc").unwrap();
    assert_ne!(renewed.binding_id, first_binding);
    assert_eq!(renewed.epoch, 2);
    eventually(async || service.finished() == [("request-1".into(), "done".into())]).await;
    let binding = &restored.snapshot(chat).await.unwrap().loops[0].event_sources[0];
    assert_eq!(binding.binding_id.to_string(), renewed.binding_id);
    assert_eq!(binding.epoch, 2);
    restored.shutdown().await;
}

#[tokio::test]
async fn a_link_replaced_elsewhere_pauses_listening_until_continue() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    service.replace_link("doc");
    // The idle watcher checks again within its 15 second interval.
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.loops[0].event_sources[0].resume_required {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    wait_for_loop_state(&agent, chat, loop_id, LoopState::Interrupted).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn signing_out_pauses_feedback_listening() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    agent.disable_event_sources().await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.loops[0].event_sources[0].resume_required);
    assert_eq!(snapshot.loops[0].state, LoopState::Interrupted);
    agent.shutdown().await;
}

#[tokio::test]
async fn relinking_an_artifact_keeps_it_in_the_loop_that_holds_it() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![text("Second goal.")])),
    )
    .await;
    agent
        .submit(chat, None, "An unrelated goal".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 2);
    assert_eq!(snapshot.loops[0].id, loop_id);
    assert_eq!(snapshot.loops[0].event_sources.len(), 1);
    assert_eq!(
        snapshot.loops[0].event_sources[0].binding_id.to_string(),
        service.link("doc").unwrap().binding_id
    );
    assert!(snapshot.loops[1].event_sources.is_empty());

    agent
        .link_artifact(chat, &artifact_tick(&service, "other"))
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.loops[1].event_sources[0].artifact_id, "other",
        "a new link goes to the newest open Loop"
    );
    assert_eq!(snapshot.loops[1].state, LoopState::WaitingTrigger);
    agent.shutdown().await;
}

#[tokio::test]
async fn a_created_artifact_joins_monitor_and_links_to_the_creating_loop() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create",
            "ArtifactEdit",
            json!({"action":"create","title":"Brief","format":"markdown","content":"# Brief"}),
        ),
        text("Created."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Write a brief".into())
        .await
        .unwrap();
    eventually(async || {
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
    })
    .await;
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(
                event.kind,
                EventKind::PermissionRequested { .. } | EventKind::ApprovalAssessed { .. }
            ),
            "ordinary first-party creation must not ask or invoke a reviewer"
        );
    }
    assert!(
        agent.approval_history_view(chat).await.unwrap().records[0]
            .review
            .is_none()
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks.len(), 1);
    assert_eq!(snapshot.artifact_ticks[0].artifact_id, CREATED_ARTIFACT);
    assert_eq!(snapshot.artifact_ticks[0].title, "Brief");
    let goal = snapshot.loops.last().unwrap();
    assert_eq!(goal.event_sources[0].artifact_id, CREATED_ARTIFACT);
    let link = service.link(CREATED_ARTIFACT).unwrap();
    assert_eq!(link.loop_id, goal.id.to_string());
    assert_eq!(link.chat_id, chat.to_string());
    assert!(tick_protocol::InstanceRef::artifact(&service.origin, CREATED_ARTIFACT).is_ok());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_failed_creation_link_is_reported_to_the_model_and_monitor_still_shows_it() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.control.fail_link.store(true, Ordering::SeqCst);
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create",
            "ArtifactEdit",
            json!({"action":"create","title":"Brief","format":"html","content":"<p>Brief</p>"}),
        ),
        text("Created."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .submit(chat, None, "Write a brief".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks.len(), 1);
    assert!(snapshot.loops[0].event_sources.is_empty());
    let result = serde_json::to_string(&model.requests.lock().unwrap()[1].messages).unwrap();
    assert!(
        result.contains("linking it for @tickloop requests failed"),
        "{result}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn continuing_after_an_account_switch_links_the_artifact_for_the_new_account() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    service.control.other_account.store(true, Ordering::SeqCst);
    service.login(directory.path()).await;
    tokio::time::timeout(Duration::from_secs(25), async {
        while !agent.snapshot(chat).await.unwrap().loops[0].event_sources[0].resume_required {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the previous account's link pauses");
    agent
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 1);
    let sources = &snapshot.loops[0].event_sources;
    assert_eq!(
        sources.len(),
        1,
        "the new account's link replaces the old one"
    );
    assert_eq!(sources[0].account_id, "user-two");
    assert!(!sources[0].resume_required);
    assert_eq!(snapshot.loops[0].state, LoopState::WaitingTrigger);
    agent.shutdown().await;
}
