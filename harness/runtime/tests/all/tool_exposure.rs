use serde_json::json;
use tickloop_harness_protocol::{ToolDefinition, mcp::ToolExposure};
use tickloop_harness_runtime::tool_exposure::{ExposureDirectory, ExposureTool, SearchSupport};

fn tool(name: &str, description: &str, exposure: ToolExposure) -> ExposureTool {
    ExposureTool {
        server: "office".into(),
        original_name: name.into(),
        definition: ToolDefinition {
            name: format!("mcp__office__{name}"),
            description: description.into(),
            input_schema: json!({"type":"object","properties":{"recipient":{"type":"string"}},"required":["recipient"]}),
        },
        original_schema: json!({"type":"object","properties":{"recipient":{"type":"string"}},"required":["recipient"]}),
        annotations: json!({"readOnlyHint":true}),
        exposure,
    }
}

#[test]
fn fold_searches_without_changing_definitions_or_revealing_hidden_tools() {
    let mut directory = ExposureDirectory::new(vec![], SearchSupport::Fold, false);
    let before = serde_json::to_vec(&directory.definitions()).unwrap();
    directory = ExposureDirectory::new(
        vec![
            tool(
                "mail",
                "Send an email message. More detail.",
                ToolExposure::Search,
            ),
            tool("secret", "Send email", ToolExposure::Hidden),
        ],
        SearchSupport::Fold,
        false,
    );
    assert_eq!(
        serde_json::to_vec(&directory.definitions()).unwrap(),
        before
    );
    let found = directory.search(&json!({"queries":["email"]})).unwrap();
    assert_eq!(
        found["matches"],
        json!([{"name":"mcp__office__mail","server":"office","summary":"Send an email message.","required":["recipient"]}])
    );
    let described = directory
        .describe(&json!({"names":["mcp__office__mail","mcp__office__secret"]}))
        .unwrap();
    assert_eq!(described["not_found"], json!(["mcp__office__secret"]));
    assert_eq!(
        described["tools"][0]["input_schema"]["required"],
        json!(["recipient"])
    );
    assert!(
        described["tools"][0]["annotations_notice"]
            .as_str()
            .unwrap()
            .contains("reference only")
    );
}

#[tokio::test]
async fn agent_discovers_describes_and_executes_search_tools_with_stable_definitions() {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use tickloop_harness_protocol::{
        EventKind, ExecutionMode,
        mcp::{McpServerConfig, McpTransport},
    };
    use tickloop_harness_runtime::{
        Agent,
        mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, mcp_service::McpService, next_event, text,
    };
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"mail","description":"Send email","inputSchema":{"type":"object","properties":{"value":{"anyOf":[{"type":"string"},{"type":"object"}]}},"required":["value"]}},{"name":"secret","inputSchema":{"type":"object"}}]));
    server.set_result(json!({"content":[{"type":"text","text":"MAIL_OK"}]}));
    let configs = vec![McpServerConfig {
        name: "office".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 5000,
        oauth: Default::default(),
        sensitive_values: vec![],
        disabled: false,
        exposure: ToolExposure::Codemode,
        tool_exposure: vec![("secret".into(), ToolExposure::Hidden)],
        description: "Office tools".into(),
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
    let configs = Arc::new(std::sync::RwLock::new(configs));
    let current = configs.clone();
    let provider = Arc::new(McpToolProvider::new(
        connections,
        Arc::new(move || current.read().unwrap().clone()),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call("find", "SearchTools", json!({"queries":["email"]})),
        call(
            "describe",
            "DescribeTools",
            json!({"names":["mcp__office__mail"]}),
        ),
        call(
            "batch",
            "ExecuteTools",
            json!({"calls":[{"name":"mcp__office__mail","arguments":{"value":"hello"}},{"name":"mcp__office__secret","arguments":{}}]}),
        ),
        text("done"),
    ]));
    let agent = Agent::with_seams(
        tickloop_harness_runtime::HarnessSeams {
            search: None,
            transcripts: None,
            environment: None,
            models: tickloop_harness_test_support::harness::ChosenModels::new(model.clone()),
        },
        tickloop_harness_runtime::HarnessOptions {
            tools: tickloop_harness_runtime::ToolPolicy::durable_chat(),
            waits: tickloop_harness_runtime::WaitHosting::Durable,
            host: tickloop_harness_protocol::AgentHostOptions {
                prompt_policy: tickloop_harness_protocol::PromptPolicy::DenyOnPrompt,
                ..tickloop_harness_protocol::AgentHostOptions::product_defaults()
            },
            ..tickloop_harness_runtime::HarnessOptions::local(config())
        },
    )
    .unwrap();
    agent.install_host_tools(provider).unwrap();
    let chat = agent.create_chat_without_folder().await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Send the email".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(server.calls().len(), 1);
    configs.write().unwrap().clear();
    model.enqueue(text("updated"));
    agent
        .submit(chat, "Report changed availability".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            !requests[0]
                .tools
                .iter()
                .any(|tool| tool.name.starts_with("mcp__"))
        );
        let first = serde_json::to_vec(&requests[0].tools).unwrap();
        for request in requests.iter() {
            assert!(
                serde_json::to_vec(&request.tools).unwrap() == first,
                "tool definitions changed across requests"
            );
        }
        let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
        assert!(last.contains("MAIL_OK"));
        assert!(last.contains("anyOf"));
        assert!(last.contains("not_executed"));
        assert!(last.contains("have disconnected"));
        let original = requests[0]
            .messages
            .iter()
            .find(|message| {
                serde_json::to_string(message)
                    .unwrap()
                    .contains("MCP server: office")
            })
            .unwrap();
        assert!(
            requests
                .last()
                .unwrap()
                .messages
                .iter()
                .any(|message| serde_json::to_value(message).unwrap()
                    == serde_json::to_value(original).unwrap())
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn batch_waits_for_each_confirmation_while_other_calls_finish() {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use tickloop_harness_protocol::{
        EventKind, ExecutionMode,
        mcp::{McpServerConfig, McpTransport},
    };
    use tickloop_harness_runtime::{
        Agent,
        mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, mcp_service::McpService, next_event, text,
    };
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"mail","inputSchema":{"type":"object","properties":{}}}]));
    server.set_result(json!({"content":[{"type":"text","text":"MAIL_OK"}]}));
    let configs = vec![McpServerConfig {
        name: "office".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 5000,
        oauth: Default::default(),
        sensitive_values: vec![],
        disabled: false,
        exposure: ToolExposure::Search,
        tool_exposure: vec![],
        description: String::new(),
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
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "batch",
            "ExecuteTools",
            json!({"calls":[{"name":"mcp__office__mail","arguments":{}},{"name":"mcp__office__mail","arguments":{}}]}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Send two emails".into()).await.unwrap();
    let mut requests = Vec::new();
    while requests.len() < 2 {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => requests.push(request),
            EventKind::Completed { .. } => panic!("batch lost a confirmation"),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(server.calls().is_empty());
    for request in &requests {
        let request = serde_json::to_value(request).unwrap();
        assert_eq!(request["parent_call"]["call_id"], "batch");
        assert_eq!(
            request["parent_call"]["input"]["calls"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    agent
        .answer_permission_with_comment(
            chat,
            requests[1].id,
            true,
            tickloop_harness_protocol::PermissionScope::Once,
            Some("Keep the audit note".into()),
        )
        .await
        .unwrap();
    loop {
        if let EventKind::ToolCompleted { result, .. } = next_event(&mut events).await.kind
            && result.content.contains("MAIL_OK")
        {
            break;
        }
    }
    assert_eq!(server.calls().len(), 1);
    assert_eq!(agent.snapshot(chat).await.unwrap().permissions.len(), 1);
    agent
        .answer_permission(chat, requests[0].id, true)
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(server.calls().len(), 2);
    assert!(
        serde_json::to_string(&model.requests.lock().unwrap().last().unwrap().messages)
            .unwrap()
            .contains("Keep the audit note")
    );
    agent.shutdown().await;
}

struct ReviewableTools(
    std::sync::atomic::AtomicUsize,
    Option<std::sync::Arc<tokio::sync::Notify>>,
);
#[async_trait::async_trait]
impl tickloop_harness_runtime::host_tools::HostToolProvider for ReviewableTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["Observe", "Review", "Pending"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: "Perform an external operation".into(),
                input_schema: json!({"type":"object","properties":{}}),
            })
            .collect()
    }
    fn has_tool_search(&self) -> bool {
        true
    }
    fn permission(
        &self,
        _: tickloop_harness_protocol::ChatId,
        name: &str,
        _: &serde_json::Value,
    ) -> tickloop_harness_runtime::host_tools::HostToolPermission {
        if name == "Observe" {
            tickloop_harness_runtime::host_tools::HostToolPermission::ReadOnly
        } else if name == "Pending" {
            tickloop_harness_runtime::host_tools::HostToolPermission::ThirdParty
        } else {
            tickloop_harness_runtime::host_tools::HostToolPermission::Reviewable
        }
    }
    async fn prepare(
        &self,
        chat: tickloop_harness_protocol::ChatId,
        name: &str,
        input: &serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_runtime::host_tools::HostToolPermission, String> {
        if name == "Review"
            && let Some(gate) = &self.1
        {
            tokio::select! {_ = cancel.cancelled() => return Err("cancelled".into()), _ = gate.notified()=>{}}
        }
        Ok(self.permission(chat, name, input))
    }
    async fn execute(
        &self,
        _: tickloop_harness_protocol::ChatId,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> tickloop_harness_protocol::ToolResult {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tickloop_harness_protocol::ToolResult {
            content: "EXECUTED".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn auto_rejection_stops_pending_calls_and_sees_the_model_authored_batch() {
    use std::sync::{Arc, atomic::AtomicUsize};
    use tickloop_harness_protocol::EventKind;
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{
        ScriptedModel, auto_classification, call, config, next_event, text,
    };
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "batch",
            "ExecuteTools",
            json!({"calls":[{"name":"Observe","arguments":{}},{"name":"Review","arguments":{}},{"name":"Pending","arguments":{}}]}),
        ),
        text("done"),
    ]));
    let reviewer = Arc::new(ScriptedModel::new(vec![
        text(
            &auto_classification(true).to_string()
        );
        2
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.set_auto_reviewer(reviewer.clone()).await;
    let review_gate = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(ReviewableTools(
        AtomicUsize::new(0),
        Some(review_gate.clone()),
    ));
    agent.install_host_tools(provider.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, tickloop_harness_protocol::ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Inspect the request".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;

        match event.kind {
            EventKind::ToolCompleted { result, .. } if result.content == "EXECUTED" => {
                review_gate.notify_one()
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let snapshots = serde_json::to_string(
        &reviewer
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| &request.messages)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(snapshots.contains("Pending"));
    assert!(snapshots.contains("EXECUTED"));
    assert!(snapshots.contains("Model-authored nested calls"));
    {
        let requests = model.requests.lock().unwrap();
        let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
        assert!(last.contains("denied"));
        assert!(last.contains("not_executed"));
    }
    assert!(agent.snapshot(chat).await.unwrap().permissions.is_empty());
    agent.shutdown().await;
}

#[test]
fn exposure_overrides_and_discovery_limits_follow_the_contract() {
    use tickloop_harness_runtime::tool_exposure::effective_exposure;
    let overrides = vec![
        ("z*".into(), ToolExposure::Hidden),
        ("*".into(), ToolExposure::Direct),
        ("zoom".into(), ToolExposure::Search),
    ];
    assert_eq!(
        effective_exposure(ToolExposure::Search, &overrides, "zoom"),
        ToolExposure::Search
    );
    assert_eq!(
        effective_exposure(ToolExposure::Search, &overrides, "zap"),
        ToolExposure::Hidden
    );
    assert_eq!(
        effective_exposure(ToolExposure::Search, &overrides, "mail"),
        ToolExposure::Direct
    );
    let directory = ExposureDirectory::new(
        vec![
            tool("mail", "Mail ".repeat(200).as_str(), ToolExposure::Search),
            tool("recipient", "Receiver", ToolExposure::Search),
        ],
        SearchSupport::Fold,
        false,
    );
    for input in [
        json!({"queries":[]}),
        json!({"queries":[" "]}),
        json!({"queries":["a","b","c","d","e","f"]}),
    ] {
        assert!(directory.search(&input).is_err());
    }
    for input in [
        json!({"names":[]}),
        json!({"names":[" "]}),
        json!({"names":["a","b","c","d","e","f"]}),
    ] {
        assert!(directory.describe(&input).is_err());
    }
    let found = directory.search(&json!({"queries":["recipient"]})).unwrap();
    assert_eq!(found["matches"][0]["name"], "mcp__office__recipient");
    let found = directory.search(&json!({"queries":["mail"]})).unwrap();
    assert_eq!(
        found["matches"][0]["summary"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        160
    );
    assert!(
        directory
            .search(&json!({"queries":["no_such_word"]}))
            .unwrap()["note"]
            .is_string()
    );
    let native = ExposureDirectory::new(
        vec![tool("mail", "Mail", ToolExposure::Search)],
        SearchSupport::Native,
        false,
    );
    assert!(native.definitions().is_empty());
    assert_eq!(native.deferred_tools().len(), 1);
}

#[test]
fn hosts_without_scripts_discover_codemode_tools_through_search() {
    let exposure: ToolExposure = serde_json::from_value(json!("codemode")).unwrap();
    let directory = ExposureDirectory::new(
        vec![tool("mail", "Send email", exposure)],
        SearchSupport::Fold,
        false,
    );
    assert_eq!(
        directory.search(&json!({"queries":["email"]})).unwrap()["matches"][0]["name"],
        "mcp__office__mail"
    );
    let scripted = ExposureDirectory::new(
        vec![tool("mail", "Send email", exposure)],
        SearchSupport::Fold,
        true,
    );
    assert!(
        scripted.search(&json!({"queries":["email"]})).unwrap()["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn nested_results_survive_reload_with_parent_links_and_no_reexecution() {
    use std::sync::{Arc, atomic::AtomicUsize};
    use tickloop_harness_protocol::{EventKind, ExecutionMode};
    use tickloop_harness_runtime::{
        Agent, HarnessOptions, HarnessSeams, environment::FolderEnvironment,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, harness::ChosenModels, next_event, text,
        transcripts::InMemoryTranscriptStore,
    };
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "batch",
            "ExecuteTools",
            json!({"calls":[{"name":"Review","arguments":{}},{"name":"Missing","arguments":{}}]}),
        ),
        text("done"),
    ]));
    let make_agent = |model: Arc<ScriptedModel>| {
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(store.clone()),
                environment: Some(Arc::new(FolderEnvironment::bare())),
                models: ChosenModels::new(model),
            },
            HarnessOptions::local(config()),
        )
        .unwrap()
    };
    let agent = make_agent(model);
    let provider = Arc::new(ReviewableTools(AtomicUsize::new(0), None));
    agent.install_host_tools(provider.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Execute the request".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.sync().await.unwrap();
    agent.shutdown().await;
    let restored = make_agent(Arc::new(ScriptedModel::new(vec![])));
    let snapshot = restored.resume_chat(chat).await.unwrap();
    let children = snapshot
        .operations
        .iter()
        .filter(|operation| operation.parent_call_id.as_deref() == Some("batch"))
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2);
    assert!(children.iter().any(|operation| operation.name == "Review"
        && operation.result.as_ref().unwrap().content == "EXECUTED"));
    assert!(children.iter().any(
        |operation| operation.name == "Missing" && operation.result.as_ref().unwrap().is_error
    ));
    assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    restored.shutdown().await;
}

#[tokio::test]
async fn rewind_drops_nested_calls_whose_parent_was_removed_from_the_same_run() {
    use std::sync::{Arc, atomic::AtomicUsize};
    use tickloop_harness_protocol::{
        Decision, EventKind, ExecutionMode, PermissionRule, RewindMode,
    };
    use tickloop_harness_runtime::{
        Agent, HarnessOptions, HarnessSeams, environment::FolderEnvironment,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, harness::ChosenModels, next_event, text,
        transcripts::InMemoryTranscriptStore,
    };
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "before",
            "ExecuteTools",
            json!({"calls":[{"name":"Pending","arguments":{}}]}),
        ),
        call(
            "after",
            "ExecuteTools",
            json!({"calls":[{"name":"Review","arguments":{}}]}),
        ),
        text("done"),
    ]));
    let make_agent = |model: Arc<ScriptedModel>| {
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(store.clone()),
                environment: Some(Arc::new(FolderEnvironment::bare())),
                models: ChosenModels::new(model),
            },
            HarnessOptions::local(config()),
        )
        .unwrap()
    };
    let agent = make_agent(model.clone());
    agent
        .install_host_tools(Arc::new(ReviewableTools(AtomicUsize::new(0), None)))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: tickloop_ids::generate(),
                decision: Decision::Ask,
                tool: "Pending".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Initial request".into()).await.unwrap();
    let request = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    agent.submit(chat, "Later request".into()).await.unwrap();
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let before = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "before")
        .unwrap();
    let after = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "after")
        .unwrap();
    assert_eq!(before.run_id, after.run_id);
    let point = snapshot.rewind_points()[1].message_id;
    agent
        .rewind(chat, point, RewindMode::Conversation)
        .await
        .unwrap();
    assert!(
        !agent
            .snapshot(chat)
            .await
            .unwrap()
            .operations
            .iter()
            .any(|operation| operation.parent_call_id.as_deref() == Some("after"))
    );
    model.enqueue(text("new branch"));
    agent
        .submit(chat, "Continue the rewound branch".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.sync().await.unwrap();
    agent.shutdown().await;
    let restored = make_agent(Arc::new(ScriptedModel::new(vec![])));
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert!(
        snapshot
            .operations
            .iter()
            .any(|operation| operation.parent_call_id.as_deref() == Some("before"))
    );
    assert!(
        !snapshot
            .operations
            .iter()
            .any(|operation| operation.parent_call_id.as_deref() == Some("after"))
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn hosts_observe_nested_confirmation_through_the_turn_interface() {
    use std::{
        sync::{Arc, atomic::AtomicUsize},
        time::Duration,
    };
    use tickloop_harness_runtime::{
        Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, TurnOp, TurnOutcome,
        TurnRequest, TurnWait, UserMessage,
    };
    use tickloop_harness_test_support::{ScriptedModel, call, config, harness::ChosenModels, text};
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "batch",
            "ExecuteTools",
            json!({"calls":[{"name":"Pending","arguments":{}}]}),
        ),
        text("done"),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(
                tickloop_harness_runtime::environment::FolderEnvironment::bare(),
            )),
            models: ChosenModels::new(model),
        },
        HarnessOptions::local(config()),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(ReviewableTools(AtomicUsize::new(0), None)))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let turn = agent
        .advance(
            chat,
            TurnRequest {
                input_id: tickloop_ids::generate().into(),
                op: TurnOp::Continue {
                    after: ParentSelection::CurrentLeaf,
                    message: UserMessage {
                        text: "Call the service".into(),
                        ..Default::default()
                    },
                },
                delivery: Delivery::Queue,
                host_context: None,
            },
        )
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), turn.outcome())
        .await
        .expect("nested permission did not reach host")
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(request.parent_call.as_ref().unwrap().call_id, "batch");
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), turn.outcome())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "{outcome:?}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn compacted_parent_ids_do_not_remove_a_new_runs_pending_nested_records() {
    use std::sync::{Arc, atomic::AtomicUsize};
    use tickloop_harness_protocol::{Decision, EventKind, ExecutionMode, PermissionRule};
    use tickloop_harness_runtime::{
        Agent, HarnessOptions, HarnessSeams, environment::FolderEnvironment,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, harness::ChosenModels, next_event, text,
        transcripts::InMemoryTranscriptStore,
    };
    use tickloop_harness_transcript::Line;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "batch",
            "ExecuteTools",
            json!({"calls":[{"name":"Review","arguments":{}}]}),
        ),
        text("done"),
        text("Earlier operation completed."),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store.clone()),
            environment: Some(Arc::new(FolderEnvironment::bare())),
            models: ChosenModels::new(model.clone()),
        },
        HarnessOptions::local(config()),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(ReviewableTools(AtomicUsize::new(0), None)))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let old = agent.submit(chat, "First request".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.compact(chat).await.unwrap();
    assert!(
        !agent
            .snapshot(chat)
            .await
            .unwrap()
            .message_compactions
            .is_empty()
    );
    agent.sync().await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: tickloop_ids::generate(),
                decision: Decision::Ask,
                tool: "Pending".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    model.enqueue(call(
        "batch",
        "ExecuteTools",
        json!({"calls":[{"name":"Pending","arguments":{"payload":"x".repeat(300_000)}}]}),
    ));
    let new = agent.submit(chat, "Second request".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.sync().await.unwrap();
    let lines = store.lines(chat);
    let snapshot = lines
        .iter()
        .rev()
        .find_map(|line| {
            if let Line::RetainedSnapshot(snapshot) = line {
                Some(snapshot)
            } else {
                None
            }
        })
        .expect("large pending call should cause a retained snapshot");
    assert!(
        snapshot.lines.iter().any(
            |line| matches!(line,Line::NestedCall(entry) if entry.operation.run_id==new.run_id)
        ),
        "pending child record was dropped because an older Run reused its parent ID"
    );
    assert!(
        !snapshot.lines.iter().any(
            |line| matches!(line,Line::NestedCall(entry) if entry.operation.run_id==old.run_id)
        ),
        "compacted old children must not be retained by a new Run's parent ID"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn partial_compaction_does_not_keep_old_children_with_reused_parent_ids() {
    use std::sync::{Arc, atomic::AtomicUsize};
    use tickloop_harness_protocol::{
        DirectedSummaryRequest, EventKind, ExecutionMode, SummaryDirection,
    };
    use tickloop_harness_runtime::{
        Agent, HarnessOptions, HarnessSeams, environment::FolderEnvironment,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, harness::ChosenModels, next_event, text,
        transcripts::InMemoryTranscriptStore,
    };
    use tickloop_harness_transcript::Line;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let batch = || {
        call(
            "batch",
            "ExecuteTools",
            json!({"calls":[{"name":"Review","arguments":{}}]}),
        )
    };
    let model = Arc::new(ScriptedModel::new(vec![
        batch(),
        text("first done"),
        batch(),
        text("second done"),
        text("First request completed."),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store.clone()),
            environment: Some(Arc::new(FolderEnvironment::bare())),
            models: ChosenModels::new(model),
        },
        HarnessOptions::local(config()),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(ReviewableTools(AtomicUsize::new(0), None)))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(dir.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let mut receipts = Vec::new();
    for prompt in ["First request", "Second request"] {
        receipts.push(agent.submit(chat, prompt.into()).await.unwrap());
        loop {
            match next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[1].message_id;
    agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: point,
                direction: SummaryDirection::UpToHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    agent.sync().await.unwrap();
    let lines = store.lines(chat);
    let snapshot = lines
        .iter()
        .rev()
        .find_map(|line| {
            if let Line::RetainedSnapshot(snapshot) = line {
                Some(snapshot)
            } else {
                None
            }
        })
        .unwrap();
    assert!(snapshot.lines.iter().any(
        |line| matches!(line,Line::NestedCall(entry) if entry.operation.run_id==receipts[1].run_id)
    ));
    assert!(!snapshot.lines.iter().any(|line|matches!(line,Line::NestedCall(entry) if entry.operation.run_id==receipts[0].run_id)),"old Run's children retained due to new Run's matching parent ID");
    agent.shutdown().await;
}

#[tokio::test]
async fn native_search_models_call_deferred_tools_directly_under_direct_call_approval() {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use tickloop_harness_model::cards::ModelCatalog;
    use tickloop_harness_protocol::{
        EventKind, ExecutionMode,
        mcp::{McpServerConfig, McpTransport},
    };
    use tickloop_harness_runtime::{
        Agent,
        mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
        tool_exposure::{DESCRIBE_TOOLS, EXECUTE_TOOLS, SEARCH_TOOLS},
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, mcp_service::McpService, next_event, text,
    };
    let original_schema = json!({"type":"object","properties":{"value":{"anyOf":[{"type":"string"},{"type":"object"}]}},"required":["value"]});
    let server = McpService::start().await;
    server.set_tools(json!([
        {"name":"mail","description":"Send email","inputSchema":original_schema},
        {"name":"secret","inputSchema":{"type":"object"}},
    ]));
    server.set_result(json!({"content":[{"type":"text","text":"MAIL_OK"}]}));
    let configs = vec![McpServerConfig {
        name: "office".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 5000,
        oauth: Default::default(),
        sensitive_values: vec![],
        disabled: false,
        exposure: ToolExposure::Search,
        tool_exposure: vec![("secret".into(), ToolExposure::Hidden)],
        description: "Office tools".into(),
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
    let model = Arc::new(ScriptedModel::new(vec![
        call("direct", "mcp__office__mail", json!({"value":"hello"})),
        text("done"),
    ]));
    let mut native = ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    native.base_url = config().base_url;
    native.api_key_env.clear();
    let agent = Agent::new_embedded(native, model.clone());
    agent
        .install_host_tools(Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Send the email".into()).await.unwrap();
    let request = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::Completed { .. } => panic!("the direct call skipped confirmation"),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert!(server.calls().is_empty());
    assert_eq!(request.name, "mcp__office__mail");
    assert_eq!(request.mcp_server.as_deref(), Some("office"));
    assert!(request.parent_call.is_none());
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(server.calls().len(), 1);

    agent.set_model(chat, config()).await.unwrap();
    model.enqueue(text("folded"));
    agent.submit(chat, "Again".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let requests = model.requests.lock().unwrap();
    let fold_tools = [SEARCH_TOOLS, DESCRIBE_TOOLS, EXECUTE_TOOLS];
    let native_requests = &requests[..requests.len() - 1];
    for native_request in native_requests {
        assert!(native_request.tools.iter().all(|tool| {
            !tool.name.starts_with("mcp__") && !fold_tools.contains(&tool.name.as_str())
        }));
        assert_eq!(
            serde_json::to_value(&native_request.deferred_tools).unwrap(),
            json!([{"name":"mcp__office__mail","description":"Send email","input_schema":original_schema}])
        );
        assert_eq!(
            serde_json::to_vec(&native_request.tools).unwrap(),
            serde_json::to_vec(&native_requests[0].tools).unwrap()
        );
    }
    let after_call = serde_json::to_string(&native_requests[1].messages).unwrap();
    assert!(after_call.contains("MAIL_OK"));
    assert!(!after_call.contains("host tools available in this conversation now: mcp__"));
    let folded = requests.last().unwrap();
    assert!(folded.deferred_tools.is_empty());
    for name in fold_tools {
        assert!(folded.tools.iter().any(|tool| tool.name == name), "{name}");
    }
    drop(requests);
    assert_eq!(server.calls().len(), 1);
    agent.shutdown().await;
}
