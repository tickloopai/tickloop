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
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(provider).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
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
    configs.write().unwrap()[0].disabled = true;
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
    let requests = model.requests.lock().unwrap();
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("mcp__"))
    );
    let first = serde_json::to_vec(&requests[0].tools).unwrap();
    for request in requests.iter() {
        assert_eq!(serde_json::to_vec(&request.tools).unwrap(), first);
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
    drop(requests);
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
    let agent = Agent::new_embedded(config(), model);
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
        .answer_permission(chat, requests[1].id, true)
        .await
        .unwrap();
    loop {
        if let EventKind::ToolCompleted { result, .. } = next_event(&mut events).await.kind {
            if result.content == "MAIL_OK" {
                break;
            }
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
    agent.shutdown().await;
}

struct ReviewableTools(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl tickloop_harness_runtime::host_tools::HostToolProvider for ReviewableTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["Review", "Pending"]
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
        if name == "Pending" {
            tickloop_harness_runtime::host_tools::HostToolPermission::ThirdParty
        } else {
            tickloop_harness_runtime::host_tools::HostToolPermission::Reviewable
        }
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
            json!({"calls":[{"name":"Review","arguments":{}},{"name":"Pending","arguments":{}}]}),
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
    let provider = Arc::new(ReviewableTools(AtomicUsize::new(0)));
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
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 0);
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
    assert!(snapshots.contains("Model-authored nested calls"));
    let requests = model.requests.lock().unwrap();
    let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
    assert!(last.contains("denied"));
    assert!(last.contains("not_executed"));
    drop(requests);
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
    let provider = Arc::new(ReviewableTools(AtomicUsize::new(0)));
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
