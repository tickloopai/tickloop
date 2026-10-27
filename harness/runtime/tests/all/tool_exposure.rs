use serde_json::json;
use std::sync::Arc;
use tickloop_harness_protocol::{ToolDefinition, mcp::ToolExposure};
use tickloop_harness_runtime::tool_exposure::{ExposureDirectory, ExposureTool, SearchSupport};

fn tool(name: &str, description: &str, exposure: ToolExposure) -> ExposureTool {
    ExposureTool {
        server: "office".into(),
        original_name: name.into(),
        definition: Arc::new(ToolDefinition {
            name: format!("mcp__office__{name}"),
            description: description.into(),
            input_schema: json!({"type":"object","properties":{"recipient":{"type":"string"}},"required":["recipient"]}),
        }),
        original_schema: Arc::new(
            json!({"type":"object","properties":{"recipient":{"type":"string"}},"required":["recipient"]}),
        ),
        annotations: Arc::new(json!({"readOnlyHint":true})),
        exposure,
    }
}

#[test]
fn fold_searches_without_changing_definitions_or_revealing_hidden_tools() {
    let mut directory = ExposureDirectory::new(vec![], SearchSupport::Fold);
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
    );
    assert_eq!(
        serde_json::to_vec(&directory.definitions()).unwrap(),
        before
    );
    let found = directory.search(&json!({"query":"email"})).unwrap();
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
    let mut configs = vec![McpServerConfig {
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
    configs.push(McpServerConfig {
        name: "ui5".into(),
        transport: McpTransport::Failed {
            reason: "missing command".into(),
        },
        exposure: ToolExposure::Search,
        tool_exposure: vec![],
        description: String::new(),
        ..configs[0].clone()
    });
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
        call("find", "SearchTools", json!({"query":"email"})),
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
        let searched = serde_json::to_string(&requests[1].messages).unwrap();
        assert!(
            searched
                .contains(r#"\"unavailable_servers\":[{\"name\":\"ui5\",\"state\":\"failed\"}]"#),
            "{searched}"
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
    );
    for input in [
        json!({}),
        json!({"query":" "}),
        json!({"query":["mail"]}),
        json!({"queries":["mail"]}),
        json!({"query":"mail","limit":0}),
        json!({"query":"mail","limit":2.5}),
        json!({"query":"mail","limit":"3"}),
        json!({"query":"select: , "}),
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
    let found = directory.search(&json!({"query":"recipient"})).unwrap();
    assert_eq!(found["matches"][0]["name"], "mcp__office__recipient");
    let found = directory.search(&json!({"query":"mail"})).unwrap();
    assert_eq!(
        found["matches"][0]["summary"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        160
    );
    assert!(directory.search(&json!({"query":"no_such_word"})).unwrap()["note"].is_string());
    let native = ExposureDirectory::new(
        vec![tool("mail", "Mail", ToolExposure::Search)],
        SearchSupport::Native,
    );
    assert!(native.definitions().is_empty());
    assert_eq!(native.deferred_tools().len(), 1);
}

#[test]
fn fold_calls_read_as_text_once_a_request_no_longer_offers_fold() {
    use tickloop_harness_protocol::{Content, Message, Role};
    use tickloop_harness_runtime::tool_exposure::{EXECUTE_TOOLS, fold_history_as_text};
    let history = vec![
        Message {
            role: Role::Assistant,
            content: vec![
                Content::ToolUse {
                    id: "batch".into(),
                    name: EXECUTE_TOOLS.into(),
                    input: json!({"calls":[{"name":"mcp__office__mail","arguments":{}}]}),
                },
                Content::ToolUse {
                    id: "read".into(),
                    name: "Read".into(),
                    input: json!({"file_path":"notes.md"}),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![
                Content::ToolResult {
                    tool_use_id: "batch".into(),
                    content: "MAIL_OK".into(),
                    is_error: false,
                },
                Content::ToolResult {
                    tool_use_id: "read".into(),
                    content: "notes".into(),
                    is_error: false,
                },
            ],
        },
    ];
    let mut converted = history.clone();
    fold_history_as_text(&mut converted, &[]);
    let Content::Text { text } = &converted[0].content[0] else {
        panic!("the Fold call stayed a tool call: {:?}", converted[0]);
    };
    assert!(text.contains(EXECUTE_TOOLS) && text.contains("mcp__office__mail"));
    assert_eq!(converted[0].content[1], history[0].content[1]);
    assert_eq!(converted[1].content[0], history[1].content[1]);
    assert!(matches!(&converted[1].content[1], Content::Text { text } if text.contains("MAIL_OK")));
    let mut again = converted.clone();
    fold_history_as_text(&mut again, &[]);
    assert_eq!(again, converted);
}

#[test]
fn native_search_offers_original_object_schemas_and_flattened_ones_otherwise() {
    let mut combined = tool("mail", "Mail", ToolExposure::Search);
    combined.original_schema = Arc::new(
        json!({"type":"object","properties":{"to":{"anyOf":[{"type":"string"},{"type":"array"}]}}}),
    );
    let mut rootless = tool("draft", "Draft", ToolExposure::Search);
    rootless.original_schema = Arc::new(json!({"anyOf":[{"type":"object"},{"type":"null"}]}));
    let native = ExposureDirectory::new(
        vec![combined.clone(), rootless.clone()],
        SearchSupport::Native,
    );
    let deferred = native.deferred_tools();
    assert_eq!(deferred[0].input_schema, *combined.original_schema);
    assert_eq!(deferred[1].input_schema, rootless.definition.input_schema);
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
async fn a_batch_cut_off_by_a_process_exit_loads_as_interrupted_with_each_call_outcome() {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use tickloop_harness_protocol::{
        Content, Decision, EventKind, ExecutionMode, OperationState, PermissionRule,
        mcp::{McpServerConfig, McpTransport},
    };
    use tickloop_harness_runtime::{
        Agent, HarnessOptions, HarnessSeams,
        environment::FolderEnvironment,
        mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
        transcript::StorePosition,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, harness::ChosenModels, mcp_service::McpService, next_event,
        transcripts::InMemoryTranscriptStore,
    };
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"write","inputSchema":{"type":"object","properties":{}}}]));
    server.set_result(json!({"content":[{"type":"text","text":"WROTE"}]}));
    server.set_delay(Duration::from_secs(30));
    let configs = vec![McpServerConfig {
        name: "slow".into(),
        source: "fixture".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 60000,
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
    let make_agent = |store: &Arc<InMemoryTranscriptStore>, model: Arc<ScriptedModel>| {
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
    let directory = tempfile::tempdir().unwrap();
    let written = directory.path().join("done.txt");
    let transcripts = Arc::new(InMemoryTranscriptStore::new());
    let first = make_agent(
        &transcripts,
        Arc::new(ScriptedModel::new(vec![call(
            "batch",
            "ExecuteTools",
            json!({"calls":[
                {"name":"Write","arguments":{"file_path":written,"content":"x"}},
                {"name":"mcp__slow__write","arguments":{}},
                {"name":"Glob","arguments":{"pattern":"*.txt"}}
            ]}),
        )])),
    );
    first
        .install_host_tools(Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )))
        .unwrap();
    let chat = first.create_chat(directory.path().into()).await.unwrap();
    first
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    first
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: tickloop_ids::generate(),
                decision: Decision::Ask,
                tool: "Glob".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = first.subscribe();
    first.submit(chat, "Run the batch".into()).await.unwrap();
    let request = loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } if event.chat_id == chat => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert_eq!(request.parent_call.as_ref().unwrap().call_id, "batch");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = first.snapshot(chat).await.unwrap();
            let write_completed = snapshot.operations.iter().any(|operation| {
                operation.name == "Write" && operation.state == OperationState::Completed
            });
            if write_completed && !server.calls().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    first.sync().await.unwrap();

    // The process is gone now: a new holder sees only what the store holds.
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(chat, transcripts.lines(chat), StorePosition(1));
    let restarted_model = Arc::new(ScriptedModel::new(vec![]));
    let restarted = make_agent(&crashed, restarted_model.clone());
    let snapshot = restarted.resume_chat(chat).await.unwrap();
    let result = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == "batch" => Some(format!("{content:?}")),
            _ => None,
        })
        .expect("no result for the batch");
    assert!(result.contains("interrupted"), "{result}");
    assert!(result.contains("will not be rerun"), "{result}");
    for (name, outcome) in [
        ("Write", "completed"),
        ("mcp__slow__write", "outcome unknown"),
        ("Glob", "not executed"),
    ] {
        assert!(
            result
                .lines()
                .chain(result.split("\\n"))
                .any(|line| line.contains(name) && line.contains(outcome)),
            "{name} {outcome}: {result}"
        );
    }
    assert_eq!(
        snapshot
            .operations
            .iter()
            .filter(|operation| operation.parent_call_id.as_deref() == Some("batch"))
            .count(),
        3
    );
    assert!(snapshot.permissions.is_empty());
    assert_eq!(server.calls().len(), 1);
    assert!(restarted_model.requests.lock().unwrap().is_empty());
    restarted.shutdown().await;
    first.shutdown().await;
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
    let requests = model.requests.lock().unwrap().clone();
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
    assert_eq!(server.calls().len(), 1);
    agent.shutdown().await;
}

fn server_tool(server: &str, name: &str, description: &str) -> ExposureTool {
    let mut tool = tool(name, description, ToolExposure::Search);
    tool.server = server.into();
    Arc::make_mut(&mut tool.definition).name = format!("mcp__{server}__{name}");
    tool
}

fn ui5_directory() -> ExposureDirectory {
    ExposureDirectory::new(
        vec![
            server_tool(
                "ui5",
                "get_version_info",
                "Get version information for UI5 (OpenUI5 or SAPUI5)",
            ),
            server_tool(
                "ui5",
                "run_ui5_linter",
                "Run the UI5 linter on a UI5 project and report findings",
            ),
            server_tool("office", "mail", "Send an email message to a recipient"),
        ],
        SearchSupport::Fold,
    )
}

fn found_names(directory: &ExposureDirectory, query: &str) -> Vec<String> {
    directory.search(&json!({"query":query})).unwrap()["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|found| found["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn multi_word_queries_find_tools_matching_any_meaningful_word() {
    let directory = ui5_directory();
    for query in [
        "ui5 tools",
        "UI5 version",
        "UI5 versions",
        "get version info",
        "ui5 mcp server tools",
    ] {
        let names = found_names(&directory, query);
        assert!(
            names.iter().any(|name| name.starts_with("mcp__ui5__")),
            "{query:?} found {names:?}"
        );
        assert!(
            !names.contains(&"mcp__office__mail".to_owned()),
            "{query:?} found {names:?}"
        );
    }
    assert_eq!(
        found_names(&directory, "UI5 version")[0],
        "mcp__ui5__get_version_info"
    );
}

#[test]
fn queries_made_only_of_unrelated_or_generic_words_match_nothing() {
    let directory = ui5_directory();
    for query in [
        "weather forecast",
        "mcp server tools",
        "the tool for a server",
        "this server",
        "mcp__",
    ] {
        let found = directory.search(&json!({"query":query})).unwrap();
        assert_eq!(found["matches"], json!([]), "{query:?}");
        assert!(found["note"].is_string());
    }
    let guidelines = ExposureDirectory::new(
        vec![server_tool(
            "ui5",
            "get_guidelines",
            "This tool MUST be called once to retrieve UI5 guidelines",
        )],
        SearchSupport::Fold,
    );
    assert!(found_names(&guidelines, "this").is_empty());
}

#[test]
fn exact_names_server_prefixes_selections_and_required_words_pick_tools() {
    let directory = ui5_directory();
    assert_eq!(
        found_names(&directory, "mcp__ui5__run_ui5_linter"),
        ["mcp__ui5__run_ui5_linter"]
    );
    assert_eq!(
        found_names(&directory, "mcp__ui5"),
        ["mcp__ui5__get_version_info", "mcp__ui5__run_ui5_linter"]
    );
    let selected = directory
        .search(&json!({"query":"select:mcp__office__mail, mcp__ui5__get_version_info,mcp__nowhere__x"}))
        .unwrap();
    assert_eq!(
        selected["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|found| found["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["mcp__office__mail", "mcp__ui5__get_version_info"]
    );
    assert_eq!(selected["not_found"], json!(["mcp__nowhere__x"]));
    assert_eq!(
        found_names(&directory, "+linter ui5 version"),
        ["mcp__ui5__run_ui5_linter"]
    );
    assert!(found_names(&directory, "+weather ui5").is_empty());
    assert_eq!(
        found_names(&directory, "mcp__ui5__get_version")[0],
        "mcp__ui5__get_version_info"
    );
    let eleven_names = (0..11)
        .map(|index| format!("mcp__office__tool_{index}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(
        directory
            .search(&json!({"query":format!("select:{eleven_names}")}))
            .is_err()
    );
}

#[test]
fn searches_return_eight_matches_by_default_and_at_most_ten() {
    let directory = ExposureDirectory::new(
        (0..12)
            .map(|index| server_tool("ui5", &format!("check_{index}"), "Inspect a UI5 project"))
            .collect(),
        SearchSupport::Fold,
    );
    let count = |input: serde_json::Value| {
        directory.search(&input).unwrap()["matches"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(count(json!({"query":"ui5 project"})), 8);
    assert_eq!(count(json!({"query":"ui5 project","limit":3})), 3);
    assert_eq!(count(json!({"query":"ui5 project","limit":50})), 10);
    assert_eq!(count(json!({"query":"mcp__ui5"})), 12);
}

#[test]
fn words_in_a_tool_name_outrank_the_same_words_in_descriptions() {
    let directory = ExposureDirectory::new(
        vec![
            server_tool(
                "docs",
                "release_notes",
                "Version history: each version, version notes and version changes",
            ),
            server_tool("docs", "get_version", "Show details"),
        ],
        SearchSupport::Fold,
    );
    assert_eq!(
        found_names(&directory, "version"),
        ["mcp__docs__get_version", "mcp__docs__release_notes"]
    );
}

#[test]
fn searches_name_servers_that_cannot_offer_tools_yet() {
    use tickloop_harness_protocol::mcp::McpServerState;
    use tickloop_harness_runtime::tool_exposure::UnavailableServer;
    let directory = ExposureDirectory::new(
        vec![server_tool("office", "mail", "Send an email message")],
        SearchSupport::Fold,
    )
    .with_unavailable_servers(vec![UnavailableServer {
        name: "ui5".into(),
        state: McpServerState::Connecting,
    }]);
    let found = directory.search(&json!({"query":"ui5 tools"})).unwrap();
    assert_eq!(found["matches"], json!([]));
    assert_eq!(
        found["unavailable_servers"],
        json!([{"name":"ui5","state":"connecting"}])
    );
    let note = found["note"].as_str().unwrap();
    assert!(
        note.contains("ui5 (connecting)") && note.contains("again"),
        "{note}"
    );
    let found = directory.search(&json!({"query":"email"})).unwrap();
    assert_eq!(found["matches"][0]["name"], "mcp__office__mail");
    assert_eq!(
        found["unavailable_servers"],
        json!([{"name":"ui5","state":"connecting"}])
    );
    let note = found["note"].as_str().unwrap();
    assert!(
        note.contains("ui5 (connecting)") && !note.contains("No tool matched"),
        "{note}"
    );
    let without_unavailable = ExposureDirectory::new(
        vec![server_tool("office", "mail", "Send an email message")],
        SearchSupport::Fold,
    );
    assert_eq!(
        serde_json::to_vec(&directory.definitions()).unwrap(),
        serde_json::to_vec(&without_unavailable.definitions()).unwrap()
    );
    assert!(
        ui5_directory().search(&json!({"query":"ui5"})).unwrap()["unavailable_servers"].is_null()
    );
}
