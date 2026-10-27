use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tickloop_harness_protocol::{
    Decision, EventKind, ExecutionMode, PermissionRule,
    mcp::{McpServerConfig, McpTransport},
};
use tickloop_harness_runtime::{
    Agent,
    mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
};
use tickloop_harness_test_support::{
    ScriptedModel, auto_classification, call, config, mcp_service::McpService, next_event, text,
};
use uuid::Uuid;

async fn provider(server: &McpService, name: &str) -> (Arc<McpToolProvider>, String) {
    provider_with_secrets(server, name, Vec::new()).await
}

async fn provider_with_secrets(
    server: &McpService,
    name: &str,
    sensitive_values: Vec<serde_json::Value>,
) -> (Arc<McpToolProvider>, String) {
    server.set_tools(json!([{"name":"read","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true}}]));
    server.set_result(json!({"content":[{"type":"text","text":"MCP_OK"}]}));
    let configs = vec![McpServerConfig {
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        oauth: Default::default(),
        sensitive_values,
        disabled: false,
        name: name.into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 300_000,
    }];
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let tool = connections.snapshot()[0].tools[0].name.clone();
    (
        Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )),
        tool,
    )
}

fn rule(tool: &str, decision: Decision) -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision,
        tool: tool.into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

#[tokio::test]
async fn server_allow_rules_admit_mcp_in_auto_even_for_hashed_names() {
    for server_name in [
        "review",
        "team_review",
        "a_service_name_long_enough_to_require_hashing_the_complete_model_tool_name",
    ] {
        for suffix in ["", "__*"] {
            let server = McpService::start().await;
            let (provider, tool) = provider(&server, server_name).await;
            let model = Arc::new(ScriptedModel::new(vec![
                call("read", &tool, json!({})),
                text("done"),
            ]));
            let agent = Agent::new_embedded(config(), model);
            agent.install_host_tools(provider).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let chat = agent.create_chat(dir.path().into()).await.unwrap();
            agent
                .set_execution_mode(chat, ExecutionMode::Auto)
                .await
                .unwrap();
            agent
                .set_permission_rules(
                    chat,
                    vec![rule(
                        &format!("mcp__{server_name}{suffix}"),
                        Decision::Allow,
                    )],
                )
                .await
                .unwrap();
            let mut events = agent.subscribe();
            agent.submit(chat, "Read the service".into()).await.unwrap();
            loop {
                match next_event(&mut events).await.kind {
                    EventKind::PermissionRequested { .. } => {
                        panic!("saved server rule did not admit {server_name}{suffix}")
                    }
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            assert_eq!(server.calls().len(), 1);
            agent.shutdown().await;
        }
    }
}

#[tokio::test]
async fn confirming_a_server_persists_its_rule_only_for_the_current_directory() {
    use tickloop_harness_policy::settings::{MemorySettingsStore, SettingsStore};
    use tickloop_harness_runtime::{HarnessOptions, HarnessSeams, environment::FolderEnvironment};
    use tickloop_harness_test_support::harness::ChosenModels;
    let settings = Arc::new(MemorySettingsStore::default());
    let server = McpService::start().await;
    let (provider, tool) = provider(&server, "review").await;
    let model = Arc::new(ScriptedModel::new(vec![
        call("first", &tool, json!({})),
        call("second", &tool, json!({})),
        text("done"),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(FolderEnvironment::bare())),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            settings: Some(settings.clone()),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    agent.install_host_tools(provider).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    let chat = agent.create_chat(project.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Read twice".into()).await.unwrap();
    let mut asked = false;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                assert!(
                    !asked,
                    "saved service permission must cover subsequent calls"
                );
                assert!(server.calls().is_empty());
                agent
                    .answer_permission_with_rule(chat, request.id, "mcp__review".into())
                    .await
                    .unwrap();
                asked = true;
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(asked);
    assert_eq!(server.calls().len(), 2);
    let saved = settings.read(Some(&project)).project.rules;
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].tool, "mcp__review");
    assert!(saved[0].input.is_none() && saved[0].specifier.is_none());
    assert!(
        settings
            .read(Some(&project.join("other")))
            .project
            .rules
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn mcp_modes_and_explicit_rules_preserve_human_boundaries() {
    use tickloop_harness_protocol::{PermissionApprovalSource, PermissionScope};
    for mode in [
        ExecutionMode::AcceptEdits,
        ExecutionMode::Auto,
        ExecutionMode::Plan,
        ExecutionMode::FullAccess,
    ] {
        for decision in [
            None,
            Some(Decision::Allow),
            Some(Decision::Ask),
            Some(Decision::Deny),
        ] {
            for target in ["mcp__review", "mcp__review__*", "mcp__review__read"] {
                let server = McpService::start().await;
                let (provider, tool) = provider(&server, "review").await;
                let agent = Agent::new_embedded(
                    config(),
                    Arc::new(ScriptedModel::new(vec![
                        call("read", &tool, json!({})),
                        text("done"),
                    ])),
                );
                let reviewer = Arc::new(ScriptedModel::new(vec![text(
                    &auto_classification(false).to_string(),
                )]));
                agent.set_auto_reviewer(reviewer.clone()).await;
                agent.install_host_tools(provider).unwrap();
                let dir = tempfile::tempdir().unwrap();
                let chat = agent.create_chat(dir.path().into()).await.unwrap();
                agent.set_execution_mode(chat, mode).await.unwrap();
                if let Some(decision) = decision {
                    agent
                        .set_permission_rules(chat, vec![rule(target, decision)])
                        .await
                        .unwrap();
                }
                // Plan is entered from Ask here, so third-party calls are decided as in Ask.
                let should_review = mode == ExecutionMode::Auto && decision.is_none();
                let should_ask = decision == Some(Decision::Ask)
                    || (decision.is_none()
                        && matches!(mode, ExecutionMode::AcceptEdits | ExecutionMode::Plan));
                let mut events = agent.subscribe();
                agent.submit(chat, "Read the service".into()).await.unwrap();
                let mut asked = false;
                loop {
                    match next_event(&mut events).await.kind {
                        EventKind::PermissionRequested { request } => {
                            assert!(should_ask && !asked, "{mode:?} {decision:?} {target}");
                            assert!(server.calls().is_empty());
                            assert_eq!(request.mcp_server.as_deref(), Some("review"));
                            if decision == Some(Decision::Ask) {
                                assert_eq!(
                                    request.approval_source,
                                    PermissionApprovalSource::ExplicitAsk
                                );
                                for scope in [PermissionScope::Session, PermissionScope::Persistent]
                                {
                                    assert!(
                                        agent
                                            .answer_permission_with_scope(
                                                chat, request.id, true, scope
                                            )
                                            .await
                                            .is_err()
                                    );
                                }
                                assert!(
                                    agent
                                        .answer_permission_with_rule(
                                            chat,
                                            request.id,
                                            "mcp__review".into()
                                        )
                                        .await
                                        .is_err()
                                );
                            }
                            agent
                                .answer_permission(chat, request.id, true)
                                .await
                                .unwrap();
                            asked = true;
                        }
                        EventKind::Completed { .. } => break,
                        EventKind::Failed(error) => panic!("{error}"),
                        _ => {}
                    }
                }
                assert_eq!(asked, should_ask, "{mode:?} {decision:?}");
                assert_eq!(
                    reviewer.requests.lock().unwrap().len(),
                    usize::from(should_review),
                    "{mode:?} {decision:?} {target}"
                );
                assert_eq!(
                    server.calls().len(),
                    usize::from(decision != Some(Decision::Deny))
                );
                agent.shutdown().await;
            }
        }
    }
}

#[tokio::test]
async fn another_servers_prefix_never_grants_or_denies_this_service() {
    for decision in [Decision::Allow, Decision::Ask, Decision::Deny] {
        let server = McpService::start().await;
        let (provider, tool) = provider(&server, "team_review").await;
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", &tool, json!({})),
                text("done"),
            ])),
        );
        agent.install_host_tools(provider).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![rule("mcp__team", decision), rule("mcp__team__*", decision)],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, "Read".into()).await.unwrap();
        let mut asked = false;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    assert!(!asked);
                    assert_eq!(
                        request.approval_source,
                        tickloop_harness_protocol::PermissionApprovalSource::PermissionPolicy
                    );
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                    asked = true;
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(asked);
        assert_eq!(server.calls().len(), 1);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn children_receive_mcp_tools_without_expanding_parent_permissions() {
    for denied in [false, true] {
        let server = McpService::start().await;
        let (provider, tool) = provider(&server, "review").await;
        let model = Arc::new(ScriptedModel::new(vec![
            call("delegate", "Subagent", json!({"task":"Read the service"})),
            call("read", &tool, json!({})),
            text("child done"),
            text("parent done"),
        ]));
        let agent = Agent::new_embedded(config(), model.clone());
        agent.install_host_tools(provider).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let parent = agent.create_chat(dir.path().into()).await.unwrap();
        agent
            .set_execution_mode(parent, ExecutionMode::FullAccess)
            .await
            .unwrap();
        if denied {
            agent
                .set_permission_rules(parent, vec![rule("mcp__review", Decision::Deny)])
                .await
                .unwrap();
        }
        let mut events = agent.subscribe();
        agent
            .submit(parent, "Delegate reading".into())
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { .. } => {
                    panic!("Full Access or Deny must not prompt")
                }
                EventKind::Completed { .. } if event.chat_id == parent => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(agent.subagents(parent).await.unwrap().len(), 1);
        assert!(
            model.requests.lock().unwrap()[1]
                .tools
                .iter()
                .any(|definition| definition.name == tool)
        );
        assert_eq!(server.calls().len(), usize::from(!denied));
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn mcp_rule_validation_rejects_unsupported_wildcards_and_specifiers() {
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    for target in ["mcp__review", "mcp__review__*", "mcp__review__read"] {
        agent
            .set_permission_rules(chat, vec![rule(target, Decision::Allow)])
            .await
            .unwrap();
    }
    for target in [
        "mcp__",
        "mcp__*",
        "mcp__review*",
        "mcp__review__r*",
        "mcp__bad/name",
    ] {
        assert!(
            agent
                .set_permission_rules(chat, vec![rule(target, Decision::Allow)])
                .await
                .is_err(),
            "{target}"
        );
    }
    let mut invalid = rule("mcp__review", Decision::Allow);
    invalid.specifier = Some("read".into());
    assert!(
        agent
            .set_permission_rules(chat, vec![invalid])
            .await
            .is_err()
    );
    agent.shutdown().await;
}

struct PausedChildModel {
    script: ScriptedModel,
    requests: std::sync::atomic::AtomicUsize,
    child_entered: tokio::sync::Notify,
    release_child: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl tickloop_harness_model::ModelClient for PausedChildModel {
    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        self.script.identity()
    }
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        self.script.generate_title(request, events, cancel).await
    }
    async fn complete(
        &self,
        request: tickloop_harness_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        if self
            .requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            self.child_entered.notify_one();
            self.release_child.notified().await;
        }
        self.script.complete(request, events, cancel).await
    }
}

#[tokio::test]
async fn mcp_children_keep_frozen_denials_and_take_new_parent_restrictions() {
    for frozen_deny in [true, false] {
        let server = McpService::start().await;
        let (provider, tool) = provider(&server, "review").await;
        let model = Arc::new(PausedChildModel {
            script: ScriptedModel::new(vec![
                call("delegate", "Subagent", json!({"task":"Read the service"})),
                call("read", &tool, json!({})),
                text("child done"),
                text("parent done"),
            ]),
            requests: Default::default(),
            child_entered: Default::default(),
            release_child: Default::default(),
        });
        let agent = Agent::new_embedded(config(), model.clone());
        agent.install_host_tools(provider).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let parent = agent.create_chat(dir.path().into()).await.unwrap();
        agent
            .set_execution_mode(parent, ExecutionMode::FullAccess)
            .await
            .unwrap();
        agent
            .set_permission_rules(
                parent,
                vec![rule(
                    "mcp__review",
                    if frozen_deny {
                        Decision::Deny
                    } else {
                        Decision::Allow
                    },
                )],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(parent, "Delegate reading".into())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), model.child_entered.notified())
            .await
            .unwrap();
        agent
            .set_permission_rules(
                parent,
                vec![rule(
                    "mcp__review",
                    if frozen_deny {
                        Decision::Allow
                    } else {
                        Decision::Deny
                    },
                )],
            )
            .await
            .unwrap();
        model.release_child.notify_one();
        let mut denied = false;
        loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { .. } => {
                    panic!("parent denial must not be overridable by a child confirmation")
                }
                EventKind::ToolCompleted {
                    call_id, result, ..
                } if call_id == "read" => {
                    assert!(result.is_error);
                    denied = true;
                }
                EventKind::Completed { .. } if event.chat_id == parent => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(denied);
        assert!(server.calls().is_empty());
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn mcp_explicit_ask_and_deny_override_wider_allows() {
    use tickloop_harness_protocol::{AgentHostOptions, PromptPolicy};
    for restrictive in [None, Some(Decision::Ask), Some(Decision::Deny)] {
        let server = McpService::start().await;
        let (provider, tool) = provider(&server, "review").await;
        let agent = Agent::new_with_options(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", &tool, json!({})),
                text("done"),
            ])),
            AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::embedded_defaults()
            },
        );
        agent.install_host_tools(provider).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        if let Some(decision) = restrictive {
            agent
                .set_permission_rules(
                    chat,
                    vec![
                        rule("mcp__review", Decision::Allow),
                        rule(&tool, Decision::Allow),
                        rule("mcp__review__*", decision),
                    ],
                )
                .await
                .unwrap();
        }
        let mut events = agent.subscribe();
        agent.submit(chat, "Read".into()).await.unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { .. } => panic!("this host cannot ask"),
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(server.calls().is_empty());
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn allowing_one_tool_does_not_allow_a_different_service() {
    use tickloop_harness_policy::settings::MemorySettingsStore;
    use tickloop_harness_runtime::{HarnessOptions, HarnessSeams, environment::FolderEnvironment};
    use tickloop_harness_test_support::harness::ChosenModels;
    let first = McpService::start().await;
    let second = McpService::start().await;
    let (first_provider, first_tool) = provider(&first, "team").await;
    let (second_provider, second_tool) = provider(&second, "team_read").await;
    let model = Arc::new(ScriptedModel::new(vec![
        call("first", &first_tool, json!({})),
        call("second", &second_tool, json!({})),
        text("done"),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(FolderEnvironment::bare())),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            settings: Some(Arc::new(MemorySettingsStore::default())),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    let providers: Vec<Arc<dyn tickloop_harness_runtime::host_tools::HostToolProvider>> =
        vec![first_provider, second_provider];
    agent.install_host_tools(providers).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read only the first service".into())
        .await
        .unwrap();
    let mut approved_first = false;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } if !approved_first => {
                assert_eq!(request.name, first_tool);
                agent
                    .answer_permission_with_rule(chat, request.id, first_tool.clone())
                    .await
                    .unwrap();
                approved_first = true;
            }
            EventKind::PermissionRequested { request } => {
                assert_eq!(request.name, second_tool);
                assert!(second.calls().is_empty());
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
                break;
            }
            EventKind::Completed { .. } => {
                panic!("the tool grant silently authorized another service")
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn service_rules_appear_in_permission_history_with_their_original_identity() {
    for decision in [Decision::Allow, Decision::Ask, Decision::Deny] {
        for suffix in ["", "__*"] {
            let service = McpService::start().await;
            let server =
                "a_service_name_long_enough_to_require_hashing_the_complete_model_tool_name";
            let (provider, tool) = provider(&service, server).await;
            assert!(tool.strip_prefix("mcp__").unwrap().contains("__"));
            let agent = Agent::new_embedded(
                config(),
                Arc::new(ScriptedModel::new(vec![
                    call("read", &tool, json!({})),
                    text("done"),
                ])),
            );
            agent.install_host_tools(provider).unwrap();
            let directory = tempfile::tempdir().unwrap();
            let chat = agent.create_chat(directory.path().into()).await.unwrap();
            let rule = rule(&format!("mcp__{server}{suffix}"), decision);
            agent
                .set_permission_rules(chat, vec![rule.clone()])
                .await
                .unwrap();
            let mut events = agent.subscribe();
            agent.submit(chat, "Read".into()).await.unwrap();
            loop {
                match next_event(&mut events).await.kind {
                    EventKind::PermissionRequested { request } => agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap(),
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            let history = agent.approval_history_view(chat).await.unwrap();
            let record = history
                .records
                .iter()
                .find(|record| record.call_id == "read")
                .unwrap();
            assert_eq!(record.matching_rules.len(), 1, "{decision:?} {suffix}");
            assert_eq!(record.matching_rules[0].id, rule.id);
            assert_eq!(record.matching_rules[0].decision, decision);
            assert_eq!(
                record.matching_rules[0].matcher.text,
                format!("{} (any input)", rule.tool)
            );
            agent.shutdown().await;
        }
    }
}

#[tokio::test]
async fn invalid_resolved_service_names_cannot_expose_ambiguous_tools() {
    let service = McpService::start().await;
    service.set_tools(json!([{"name":"read","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            name: "team__read".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: service.url(),
                headers: BTreeMap::new(),
            },
            timeout_ms: 1000,
            oauth: Default::default(),
            sensitive_values: Vec::new(),
            disabled: false,
        }])
        .await;
    let statuses = connections.statuses();
    assert_eq!(statuses[0].state, McpServerState::Failed);
    assert!(statuses[0].failure.as_deref().unwrap().contains("__"));
    assert!(statuses[0].tools.is_empty());
    assert!(matches!(
        connections
            .call(
                "mcp__team__read__read",
                json!({}),
                tokio_util::sync::CancellationToken::new()
            )
            .await,
        tickloop_harness_runtime::host_tools::HostCallResult::NotDispatched(_)
    ));
    assert!(service.calls().is_empty());
    connections.shutdown().await;
}

#[tokio::test]
async fn saved_tool_permissions_do_not_move_between_separately_connected_services() {
    use tickloop_harness_runtime::host_tools::HostToolProvider;
    for (first_name, second_name) in [
        ("review", "review_"),
        ("_review", "review"),
        ("_", "unnamed"),
    ] {
        let first = McpService::start().await;
        let second = McpService::start().await;
        let (first_provider, first_tool) = provider(&first, first_name).await;
        first_provider.shutdown().await;
        let (second_provider, second_tool) = provider(&second, second_name).await;
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", &second_tool, json!({})),
                text("done"),
            ])),
        );
        agent.install_host_tools(second_provider).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_permission_rules(chat, vec![rule(&first_tool, Decision::Allow)])
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, "Read".into()).await.unwrap();
        let mut asked = false;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    assert_eq!(request.mcp_server.as_deref(), Some(second_name));
                    assert!(second.calls().is_empty());
                    asked = true;
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolStarted { .. } => {
                    assert!(asked, "{first_name} permission authorized {second_name}")
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(asked);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn auto_review_decides_unruled_mcp_calls_and_escalates_repeated_rejections() {
    use tickloop_harness_protocol::PermissionApprovalSource;
    const SECRET: &str = "fixture-mcp-secret";
    let server = McpService::start().await;
    let (provider, tool) = provider_with_secrets(&server, "review", vec![json!(SECRET)]).await;
    server.set_result(json!({"content":[{"type":"text","text":format!("MCP_OK {SECRET}")}]}));
    let model = Arc::new(ScriptedModel::new(vec![
        call("read-1", &tool, json!({"query":"first quarterly figures"})),
        call("read-2", &tool, json!({"query":"second"})),
        call("read-3", &tool, json!({"query":"third"})),
        call("read-4", &tool, json!({"query":"fourth"})),
        text("done"),
    ]));
    let allow = text(&auto_classification(false).to_string());
    let block = text(&auto_classification(true).to_string());
    // A rejection takes both review stages; an admission ends after the first.
    let reviewer = Arc::new(ScriptedModel::new(
        std::iter::once(allow)
            .chain(std::iter::repeat_n(block, 6))
            .collect(),
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.set_auto_reviewer(reviewer.clone()).await;
    agent.install_host_tools(provider).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Read the figures".into()).await.unwrap();
    let mut prompted = Vec::new();
    let mut rejected = Vec::new();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                assert_eq!(
                    request.approval_source,
                    PermissionApprovalSource::AutoReviewer
                );
                assert_eq!(request.mcp_server.as_deref(), Some("review"));
                prompted.push(request.input["query"].clone());
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } if result.is_error => rejected.push(call_id),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(rejected, ["read-2", "read-3"]);
    assert_eq!(
        prompted,
        [json!("fourth")],
        "the third rejection in a row goes to a person"
    );
    assert_eq!(server.calls().len(), 2);
    let reviews = reviewer.requests.lock().unwrap();
    assert_eq!(reviews.len(), 7);
    let first_review = serde_json::to_string(&reviews[0].messages).unwrap();
    assert!(first_review.contains(&tool));
    assert!(first_review.contains("first quarterly figures"));
    let every_request = reviews
        .iter()
        .chain(model.requests.lock().unwrap().iter())
        .map(|request| serde_json::to_string(&request.messages).unwrap())
        .collect::<String>();
    assert!(!every_request.contains(SECRET));
    drop(reviews);
    agent.shutdown().await;
}

#[tokio::test]
async fn an_unrestricted_host_neither_skips_mcp_confirmation_nor_auto_review() {
    use tickloop_harness_protocol::{AgentHostOptions, PromptPolicy};
    let rejection = auto_classification(true).to_string();
    // A reviewer that never produces a verdict leaves the call to a person.
    let unusable = "no verdict".to_owned();
    for (mode, verdict) in [
        (ExecutionMode::AcceptEdits, &rejection),
        (ExecutionMode::Auto, &rejection),
        (ExecutionMode::Auto, &unusable),
    ] {
        let server = McpService::start().await;
        let (provider, tool) = provider(&server, "review").await;
        let agent = Agent::new_with_options(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", &tool, json!({})),
                text("done"),
            ])),
            AgentHostOptions {
                prompt_policy: PromptPolicy::Unrestricted,
                ..AgentHostOptions::embedded_defaults()
            },
        );
        let reviewer = Arc::new(ScriptedModel::new(vec![text(verdict); 8]));
        agent.set_auto_reviewer(reviewer.clone()).await;
        agent.install_host_tools(provider).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        agent.set_execution_mode(chat, mode).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, "Read".into()).await.unwrap();
        let mut asked = false;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    asked = true;
                    agent
                        .answer_permission(chat, request.id, false)
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } | EventKind::Interrupted => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let reviews = reviewer.requests.lock().unwrap().len();
        if mode == ExecutionMode::AcceptEdits {
            assert!(asked);
            assert_eq!(reviews, 0);
        } else if verdict == &rejection {
            assert!(!asked, "a rejection returns to the model");
            assert_eq!(reviews, 2);
        } else {
            assert!(asked, "an unusable review is escalated to a person");
            assert!(reviews > 0);
        }
        assert!(server.calls().is_empty(), "{mode:?}");
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn a_host_that_cannot_ask_runs_mcp_calls_the_auto_review_admits_and_denies_the_rest() {
    use tickloop_harness_protocol::{AgentHostOptions, PromptPolicy};
    let admission = auto_classification(false).to_string();
    for verdict in [admission.clone(), "no verdict".into()] {
        let server = McpService::start().await;
        let (provider, tool) = provider(&server, "review").await;
        let agent = Agent::new_with_options(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", &tool, json!({})),
                text("done"),
            ])),
            AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::embedded_defaults()
            },
        );
        agent
            .set_auto_reviewer(Arc::new(ScriptedModel::new(vec![text(&verdict); 8])))
            .await;
        agent.install_host_tools(provider).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, "Read".into()).await.unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { .. } => panic!("this host cannot ask"),
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(
            server.calls().len(),
            usize::from(verdict == admission),
            "{verdict}"
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn auto_reviews_each_batched_mcp_call_against_the_whole_model_authored_batch() {
    for admitted in [true, false] {
        let server = McpService::start().await;
        let (provider, tool) = provider(&server, "review").await;
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "batch",
                "ExecuteTools",
                json!({"calls":[
                    {"name":tool,"arguments":{"query":"BATCH_FIRST"}},
                    {"name":tool,"arguments":{"query":"BATCH_SECOND"}}
                ]}),
            ),
            text("done"),
        ]));
        let verdict = auto_classification(!admitted).to_string();
        let reviewer = Arc::new(ScriptedModel::new(vec![text(&verdict); 4]));
        let agent = Agent::new_embedded(config(), model.clone());
        agent.set_auto_reviewer(reviewer.clone()).await;
        agent.install_host_tools(provider).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, "Read the figures".into()).await.unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { .. } => {
                    panic!("Auto review decides these calls")
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(server.calls().len(), if admitted { 2 } else { 0 });
        let reviews = reviewer.requests.lock().unwrap();
        assert!(!reviews.is_empty());
        for review in reviews.iter() {
            let review = serde_json::to_string(&review.messages).unwrap();
            assert!(review.contains("Model-authored nested calls"));
            assert!(review.contains("BATCH_FIRST") && review.contains("BATCH_SECOND"));
        }
        drop(reviews);
        let last = serde_json::to_string(&model.requests.lock().unwrap().last().unwrap().messages)
            .unwrap();
        assert_eq!(last.contains("external-code restriction"), !admitted);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn plan_decides_mcp_calls_as_the_mode_it_was_entered_from() {
    for (before_plan, rule_decision, expect_review, expect_ask, expect_call) in [
        (ExecutionMode::Auto, None, true, false, true),
        (ExecutionMode::FullAccess, None, false, false, true),
        (ExecutionMode::AcceptEdits, None, false, true, true),
        (
            ExecutionMode::FullAccess,
            Some(Decision::Ask),
            false,
            true,
            true,
        ),
        (
            ExecutionMode::FullAccess,
            Some(Decision::Deny),
            false,
            false,
            false,
        ),
        (
            ExecutionMode::Auto,
            Some(Decision::Deny),
            false,
            false,
            false,
        ),
    ] {
        let label = format!("{before_plan:?} {rule_decision:?}");
        let server = McpService::start().await;
        let (provider, tool) = provider(&server, "review").await;
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", &tool, json!({})),
                text("done"),
            ])),
        );
        let reviewer = Arc::new(ScriptedModel::new(vec![text(
            &auto_classification(false).to_string(),
        )]));
        agent.set_auto_reviewer(reviewer.clone()).await;
        agent.install_host_tools(provider).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        agent.set_execution_mode(chat, before_plan).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Plan)
            .await
            .unwrap();
        if let Some(decision) = rule_decision {
            agent
                .set_permission_rules(chat, vec![rule("mcp__review", decision)])
                .await
                .unwrap();
        }
        let mut events = agent.subscribe();
        agent.submit(chat, "Look it up".into()).await.unwrap();
        let mut asked = false;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    assert!(!asked, "{label}");
                    asked = true;
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(asked, expect_ask, "{label}");
        assert_eq!(
            reviewer.requests.lock().unwrap().len(),
            usize::from(expect_review),
            "{label}"
        );
        assert_eq!(server.calls().len(), usize::from(expect_call), "{label}");
        assert_eq!(
            agent.snapshot(chat).await.unwrap().policy.mode,
            ExecutionMode::Plan,
            "{label}"
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn entering_plan_through_the_tool_keeps_the_auto_review_for_mcp_calls() {
    let server = McpService::start().await;
    let (provider, tool) = provider(&server, "review").await;
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("plan", "EnterPlanMode", json!({})),
            call("read", &tool, json!({})),
            text("done"),
        ])),
    );
    let reviewer = Arc::new(ScriptedModel::new(vec![text(
        &auto_classification(false).to_string(),
    )]));
    agent.set_auto_reviewer(reviewer.clone()).await;
    agent.install_host_tools(provider).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Plan the lookup".into()).await.unwrap();
    let mut prompts = Vec::new();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                prompts.push(request.name.clone());
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(prompts, ["EnterPlanMode"]);
    assert_eq!(reviewer.requests.lock().unwrap().len(), 1);
    assert_eq!(server.calls().len(), 1);
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn reentering_plan_forgets_the_mode_an_earlier_plan_was_entered_from() {
    let server = McpService::start().await;
    let (provider, tool) = provider(&server, "review").await;
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("read", &tool, json!({})),
            text("done"),
        ])),
    );
    agent.install_host_tools(provider).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    for mode in [
        ExecutionMode::FullAccess,
        ExecutionMode::Plan,
        ExecutionMode::Plan,
        ExecutionMode::AcceptEdits,
        ExecutionMode::Plan,
    ] {
        agent.set_execution_mode(chat, mode).await.unwrap();
    }
    let mut events = agent.subscribe();
    agent.submit(chat, "Look it up".into()).await.unwrap();
    let mut asked = false;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                asked = true;
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(asked, "Plan was last entered from Ask");
    assert_eq!(server.calls().len(), 1);
    agent.shutdown().await;
}
