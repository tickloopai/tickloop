use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness_protocol::{
    ChatId, EventKind, ExecutionMode, ModelIdentity, ToolDefinition, ToolResult,
    mcp::{McpServerConfig, McpTransport, ToolExposure},
};
use tickloop_harness_runtime::{
    Agent,
    host_tools::{HostToolPermission, HostToolProvider},
    mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
};
use tickloop_harness_test_support::{ScriptedModel, mcp_service::McpService, next_event};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

type RequestHook = Box<dyn Fn(usize) -> BoxFuture<'static, ()> + Send + Sync>;

/// Runs `hook` with each conversation request's index before the scripted reply, so a test can
/// change the host between two requests of the same Run.
struct HookedModel {
    inner: Arc<ScriptedModel>,
    hook: RequestHook,
    requests: AtomicUsize,
}

impl HookedModel {
    fn new(inner: Arc<ScriptedModel>, hook: RequestHook) -> Arc<Self> {
        Arc::new(Self {
            inner,
            hook,
            requests: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ModelClient for HookedModel {
    fn identity(&self) -> ModelIdentity {
        self.inner.identity()
    }

    async fn generate_title(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.inner.generate_title(request, events, cancel).await
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let index = self.requests.fetch_add(1, Ordering::SeqCst);
        (self.hook)(index).await;
        self.inner.complete(request, events, cancel).await
    }
}

/// Host tools from a second source, as the Artifacts tools come from their own source.
struct NamedHostTools(Vec<&'static str>);

#[async_trait]
impl HostToolProvider for NamedHostTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.0
            .iter()
            .map(|name| ToolDefinition {
                name: (*name).into(),
                description: format!("The {name} fixture."),
                input_schema: json!({"type":"object","properties":{}}),
            })
            .collect()
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        ToolResult {
            content: "ok".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

fn server_config(name: &str, server: &McpService, exposure: ToolExposure) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 5000,
        oauth: Default::default(),
        sensitive_values: Vec::new(),
        disabled: false,
        exposure,
        tool_exposure: Vec::new(),
        description: String::new(),
    }
}

fn new_connections() -> McpConnections {
    McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    )
}

async fn connect(connections: &McpConnections, configs: &[McpServerConfig]) {
    connections.reconcile(configs.to_vec()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot().len() != configs.len()
            || connections
                .snapshot()
                .iter()
                .any(|server| server.state != McpServerState::Connected)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every configured server connects");
}

/// A provider that reads `configs` on each refresh, as the Daemon's provider reads settings.
fn mcp_provider(
    connections: McpConnections,
    configs: &Arc<RwLock<Vec<McpServerConfig>>>,
) -> Arc<dyn HostToolProvider> {
    let current = configs.clone();
    Arc::new(McpToolProvider::new(
        connections,
        Arc::new(move || current.read().unwrap().clone()),
    ))
}

async fn full_access_chat(agent: &Agent) -> (ChatId, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    (chat, directory)
}

/// Submits `prompt` and waits until the Run answering it completes.
async fn run_turn(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

fn tool_bytes(request: &ModelRequest) -> Vec<u8> {
    serde_json::to_vec(&request.tools).unwrap()
}

fn tool_names(tools: &[ToolDefinition]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

#[tokio::test]
async fn direct_tools_list_the_same_bytes_whichever_server_connects_first() {
    let alpha = McpService::start().await;
    alpha.set_tools(json!([
        {"name":"zap","inputSchema":{"type":"object"}},
        {"name":"lookup","inputSchema":{"type":"object"}},
    ]));
    let beta = McpService::start().await;
    beta.set_tools(json!([{"name":"fetch","inputSchema":{"type":"object"}}]));
    let mut listed = Vec::new();
    for order in [["alpha", "beta"], ["beta", "alpha"]] {
        let connections = new_connections();
        let mut configs = Vec::new();
        for name in order {
            let server = if name == "alpha" { &alpha } else { &beta };
            configs.push(server_config(name, server, ToolExposure::Direct));
            connect(&connections, &configs).await;
        }
        let configs = Arc::new(RwLock::new(configs));
        let model = Arc::new(ScriptedModel::new(vec![
            tickloop_harness_test_support::text("done"),
        ]));
        let agent = Agent::new_embedded(tickloop_harness_test_support::config(), model.clone());
        agent
            .install_host_tools(vec![
                mcp_provider(connections, &configs),
                Arc::new(NamedHostTools(vec!["memo", "aardvark"])) as Arc<dyn HostToolProvider>,
            ])
            .unwrap();
        let (chat, _directory) = full_access_chat(&agent).await;
        run_turn(&agent, chat, "List your tools").await;
        let request = model.requests.lock().unwrap()[0].clone();
        let names = tool_names(&request.tools);
        assert_eq!(
            names[names.len() - 5..],
            [
                "aardvark",
                "mcp__alpha__lookup",
                "mcp__alpha__zap",
                "mcp__beta__fetch",
                "memo",
            ],
            "{names:?}"
        );
        listed.push(tool_bytes(&request));
        agent.shutdown().await;
    }
    assert!(listed[0] == listed[1], "connection order changed the tools");
}

#[tokio::test]
async fn a_direct_server_connecting_mid_run_joins_the_listed_tools_at_the_next_run() {
    use tickloop_harness_test_support::{call, config, text};
    let alpha = McpService::start().await;
    alpha.set_tools(json!([{"name":"lookup","inputSchema":{"type":"object"}}]));
    alpha.set_result(json!({"content":[{"type":"text","text":"LOOKED_UP"}]}));
    let beta = McpService::start().await;
    beta.set_tools(json!([{"name":"fetch","inputSchema":{"type":"object"}}]));
    let connections = new_connections();
    let alpha_config = server_config("alpha", &alpha, ToolExposure::Direct);
    connect(&connections, std::slice::from_ref(&alpha_config)).await;
    let configs = Arc::new(RwLock::new(vec![alpha_config.clone()]));
    let late = (
        connections.clone(),
        configs.clone(),
        vec![
            alpha_config,
            server_config("beta", &beta, ToolExposure::Direct),
        ],
    );
    let scripted = Arc::new(ScriptedModel::new(vec![
        call("lookup", "mcp__alpha__lookup", json!({})),
        text("first run done"),
        text("second run done"),
    ]));
    let model = HookedModel::new(
        scripted.clone(),
        Box::new(move |index| {
            let (connections, configs, late) = late.clone();
            Box::pin(async move {
                if index == 0 {
                    connect(&connections, &late).await;
                    *configs.write().unwrap() = late;
                }
            })
        }),
    );
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(mcp_provider(connections, &configs))
        .unwrap();
    let (chat, _directory) = full_access_chat(&agent).await;
    run_turn(&agent, chat, "Look it up").await;
    run_turn(&agent, chat, "Again").await;
    let requests = scripted.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(!tool_names(&requests[0].tools).contains(&"mcp__beta__fetch"));
    assert!(
        tool_bytes(&requests[1]) == tool_bytes(&requests[0]),
        "the Run's second request changed its tools: {:?}",
        tool_names(&requests[1].tools)
    );
    let notice = |request: &ModelRequest| {
        serde_json::to_string(&request.messages)
            .unwrap()
            .contains("Now available: mcp__beta__fetch")
    };
    assert!(!notice(&requests[1]));
    assert!(tool_names(&requests[2].tools).contains(&"mcp__beta__fetch"));
    assert!(notice(&requests[2]));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_tool_that_vanishes_mid_run_keeps_its_declaration_and_is_not_executed() {
    use tickloop_harness_protocol::Content;
    use tickloop_harness_test_support::{call, config, text};
    let alpha = McpService::start().await;
    alpha.set_tools(json!([{"name":"lookup","inputSchema":{"type":"object"}}]));
    alpha.set_result(json!({"content":[{"type":"text","text":"LOOKED_UP"}]}));
    let beta = McpService::start().await;
    beta.set_tools(json!([{"name":"fetch","inputSchema":{"type":"object"}}]));
    let connections = new_connections();
    let alpha_config = server_config("alpha", &alpha, ToolExposure::Direct);
    let both = vec![
        alpha_config.clone(),
        server_config("beta", &beta, ToolExposure::Direct),
    ];
    connect(&connections, &both).await;
    let configs = Arc::new(RwLock::new(both));
    let remaining = (connections.clone(), configs.clone(), vec![alpha_config]);
    let scripted = Arc::new(ScriptedModel::new(vec![
        call("lookup", "mcp__alpha__lookup", json!({})),
        call("fetch", "mcp__beta__fetch", json!({})),
        text("first run done"),
        text("second run done"),
    ]));
    let model = HookedModel::new(
        scripted.clone(),
        Box::new(move |index| {
            let (connections, configs, remaining) = remaining.clone();
            Box::pin(async move {
                if index == 0 {
                    *configs.write().unwrap() = remaining.clone();
                    connect(&connections, &remaining).await;
                }
            })
        }),
    );
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(mcp_provider(connections, &configs))
        .unwrap();
    let (chat, _directory) = full_access_chat(&agent).await;
    run_turn(&agent, chat, "Fetch it").await;
    run_turn(&agent, chat, "Again").await;
    let requests = scripted.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(tool_names(&requests[0].tools).contains(&"mcp__beta__fetch"));
    for request in &requests[1..3] {
        assert!(
            tool_bytes(request) == tool_bytes(&requests[0]),
            "the Run's tools changed: {:?}",
            tool_names(&request.tools)
        );
    }
    let fetch_result = requests[2]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == "fetch" => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .expect("the vanished tool's call has a result");
    assert_eq!(
        fetch_result,
        (
            "This tool is unavailable now: its source stopped offering it during this turn. This call was not executed.".to_owned(),
            true
        )
    );
    assert!(beta.calls().is_empty());
    assert_eq!(alpha.calls().len(), 1);
    assert!(!tool_names(&requests[3].tools).contains(&"mcp__beta__fetch"));
    assert!(
        serde_json::to_string(&requests[3].messages)
            .unwrap()
            .contains("No longer available: mcp__beta__fetch")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn native_search_keeps_the_listed_tools_while_deferred_tools_grow_sorted_at_the_end() {
    use tickloop_harness_model::cards::ModelCatalog;
    use tickloop_harness_protocol::{Content, Protocol};
    use tickloop_harness_test_support::{config, text};
    let alpha = McpService::start().await;
    alpha.set_tools(json!([{"name":"lookup","inputSchema":{"type":"object"}}]));
    let gamma = McpService::start().await;
    gamma.set_tools(json!([
        {"name":"zeta","inputSchema":{"type":"object"}},
        {"name":"beta","inputSchema":{"type":"object"}},
    ]));
    gamma.set_result(json!({"content":[{"type":"text","text":"ZETA_OK"}]}));
    let delta = McpService::start().await;
    delta.set_tools(json!([{"name":"omega","inputSchema":{"type":"object"}}]));
    let connections = new_connections();
    let initial = vec![
        server_config("alpha", &alpha, ToolExposure::Direct),
        server_config("gamma", &gamma, ToolExposure::Search),
    ];
    connect(&connections, &initial).await;
    let configs = Arc::new(RwLock::new(initial.clone()));
    let mut late = initial;
    late.push(server_config("delta", &delta, ToolExposure::Search));
    let late = (connections.clone(), configs.clone(), late);
    let search = json!({"type":"server_tool_use","id":"srvtoolu_1","name":"tool_search_tool_bm25","input":{"query":"zeta"}});
    let found = json!({"type":"tool_search_tool_result","tool_use_id":"srvtoolu_1","content":{"type":"tool_search_tool_search_result","tool_references":[{"type":"tool_reference","tool_name":"mcp__gamma__zeta"}]}});
    let scripted = Arc::new(ScriptedModel::new(vec![
        vec![
            Content::ProtocolData {
                protocol: Protocol::Messages,
                value: search,
            },
            Content::ProtocolData {
                protocol: Protocol::Messages,
                value: found,
            },
            Content::ToolUse {
                id: "zeta".into(),
                name: "mcp__gamma__zeta".into(),
                input: json!({}),
            },
        ],
        text("first run done"),
        text("second run done"),
    ]));
    let model = HookedModel::new(
        scripted.clone(),
        Box::new(move |index| {
            let (connections, configs, late) = late.clone();
            Box::pin(async move {
                if index == 0 {
                    connect(&connections, &late).await;
                    *configs.write().unwrap() = late;
                }
            })
        }),
    );
    let mut native = ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    native.base_url = config().base_url;
    native.api_key_env.clear();
    let agent = Agent::new_embedded(native, model);
    agent
        .install_host_tools(mcp_provider(connections, &configs))
        .unwrap();
    let (chat, _directory) = full_access_chat(&agent).await;
    run_turn(&agent, chat, "Use zeta").await;
    run_turn(&agent, chat, "Again").await;
    let requests = scripted.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert_eq!(gamma.calls().len(), 1);
    assert!(tool_names(&requests[0].tools).contains(&"mcp__alpha__lookup"));
    for request in &requests[1..] {
        assert!(
            tool_bytes(request) == tool_bytes(&requests[0]),
            "the tools before the native search tool changed: {:?}",
            tool_names(&request.tools)
        );
    }
    assert_eq!(
        tool_names(&requests[0].deferred_tools),
        ["mcp__gamma__beta", "mcp__gamma__zeta"]
    );
    for request in &requests[1..] {
        assert_eq!(
            tool_names(&request.deferred_tools),
            ["mcp__delta__omega", "mcp__gamma__beta", "mcp__gamma__zeta"]
        );
    }
    agent.shutdown().await;
}

#[derive(Clone, Default)]
struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cache_breaks_are_logged_only_when_the_prefix_changes_or_cache_reads_collapse() {
    use tickloop_harness_protocol::Usage;
    use tickloop_harness_test_support::{call, config, text};
    use tracing_subscriber::util::SubscriberInitExt;
    let log = CapturedLog::default();
    let writer = log.clone();
    let _subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with_writer(move || writer.clone())
        .finish()
        .set_default();
    let alpha = McpService::start().await;
    alpha.set_tools(json!([{"name":"lookup","inputSchema":{"type":"object"}}]));
    let beta = McpService::start().await;
    beta.set_tools(json!([{"name":"fetch","inputSchema":{"type":"object"}}]));
    let connections = new_connections();
    let alpha_config = server_config("alpha", &alpha, ToolExposure::Direct);
    connect(&connections, std::slice::from_ref(&alpha_config)).await;
    let configs = Arc::new(RwLock::new(vec![alpha_config.clone()]));
    let cache_read = |tokens| Usage {
        cache_read_input_tokens: Some(tokens),
        ..Usage::default()
    };
    let model = Arc::new(ScriptedModel::with_usages(
        vec![
            call("lookup", "mcp__alpha__lookup", json!({})),
            text("first run done"),
            text("second run done"),
            call("lookup", "mcp__alpha__lookup", json!({})),
            text("third run done"),
        ],
        vec![
            cache_read(0),
            cache_read(1200),
            cache_read(1300),
            cache_read(1400),
            cache_read(100),
        ],
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(mcp_provider(connections.clone(), &configs))
        .unwrap();
    let (chat, _directory) = full_access_chat(&agent).await;
    let breaks = |log: &CapturedLog| {
        String::from_utf8(log.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains("cache break"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let field = |line: &str, name: &str| {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")).map(str::to_owned))
            .unwrap_or_else(|| panic!("missing {name}: {line}"))
    };
    let chat_field = format!("{chat:?}");
    run_turn(&agent, chat, "Look it up").await;
    assert_eq!(breaks(&log), Vec::<String>::new());
    let both = vec![
        alpha_config,
        server_config("beta", &beta, ToolExposure::Direct),
    ];
    connect(&connections, &both).await;
    *configs.write().unwrap() = both;
    run_turn(&agent, chat, "Again").await;
    let after_tool_change = breaks(&log);
    assert_eq!(after_tool_change.len(), 1, "{after_tool_change:#?}");
    let changed = &after_tool_change[0];
    assert_eq!(field(changed, "chat"), chat_field);
    assert_eq!(field(changed, "reason"), "prefix_changed");
    assert!(
        field(changed, "changed")
            .split(',')
            .any(|part| part == "tools")
    );
    assert_eq!(field(changed, "added_tools"), "mcp__beta__fetch");
    assert_eq!(field(changed, "removed_tools"), "none");
    assert_eq!(field(changed, "changed_tools"), "none");
    assert_eq!(field(changed, "previous_cache_read"), "1200");
    assert_eq!(field(changed, "cache_read"), "1300");
    run_turn(&agent, chat, "Once more").await;
    agent.shutdown().await;
    let breaks = breaks(&log);
    let collapsed = breaks.last().unwrap();
    assert_eq!(field(collapsed, "reason"), "cache_read_dropped");
    assert_eq!(field(collapsed, "changed"), "none");
    assert_eq!(field(collapsed, "added_tools"), "none");
    assert_eq!(field(collapsed, "previous_cache_read"), "1400");
    assert_eq!(field(collapsed, "cache_read"), "100");
    assert!(
        breaks[1..breaks.len() - 1]
            .iter()
            .all(|line| field(line, "added_tools") == "none"
                && field(line, "removed_tools") == "none"),
        "{breaks:#?}"
    );
}

/// Host tools whose names a test changes, answering calls with its own label.
struct ChangingHostTools {
    names: std::sync::Mutex<Vec<&'static str>>,
    label: &'static str,
}

#[async_trait]
impl HostToolProvider for ChangingHostTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        NamedHostTools(self.names.lock().unwrap().clone()).definitions()
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        ToolResult {
            content: self.label.into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn a_tool_name_two_sources_offer_keeps_the_first_sources_tool() {
    use tickloop_harness_protocol::Content;
    use tickloop_harness_test_support::{call, config, text};
    let first = Arc::new(ChangingHostTools {
        names: std::sync::Mutex::new(vec!["memo"]),
        label: "FIRST_SOURCE",
    });
    let second = Arc::new(ChangingHostTools {
        names: std::sync::Mutex::new(vec!["note"]),
        label: "SECOND_SOURCE",
    });
    let model = Arc::new(ScriptedModel::new(vec![
        text("first run done"),
        call("memo", "memo", json!({})),
        text("second run done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(vec![
            first.clone() as Arc<dyn HostToolProvider>,
            second.clone() as Arc<dyn HostToolProvider>,
        ])
        .unwrap();
    let (chat, _directory) = full_access_chat(&agent).await;
    run_turn(&agent, chat, "Hello").await;
    *second.names.lock().unwrap() = vec!["memo", "note"];
    run_turn(&agent, chat, "Write a memo").await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap().clone();
    for request in &requests[1..] {
        let names = tool_names(&request.tools);
        assert_eq!(
            names.iter().filter(|name| **name == "memo").count(),
            1,
            "{names:?}"
        );
        assert!(names.contains(&"note"));
    }
    let memo_result = requests[2]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == "memo" => Some(content.text().into_owned()),
            _ => None,
        })
        .unwrap();
    assert_eq!(memo_result, "FIRST_SOURCE");
}

/// Where a listed tool belongs, in the order the groups are listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ListedGroup {
    BuiltIn,
    Runtime,
    McpResources,
    Fold,
    HostTools,
}

fn listed_group(name: &str) -> ListedGroup {
    let is_built_in = tickloop_harness_tools::Tools::definitions()
        .iter()
        .any(|definition| definition.name == name);
    match name {
        _ if is_built_in => ListedGroup::BuiltIn,
        "ListMcpResourcesTool" | "ReadMcpResourceTool" => ListedGroup::McpResources,
        "SearchTools" | "DescribeTools" | "ExecuteTools" => ListedGroup::Fold,
        _ if name.starts_with("mcp__") || ["memo", "aardvark"].contains(&name) => {
            ListedGroup::HostTools
        }
        _ => ListedGroup::Runtime,
    }
}

#[tokio::test]
async fn every_listed_group_is_sorted_by_name_whatever_the_registration_order() {
    let alpha = McpService::start().await;
    alpha.set_tools(json!([
        {"name":"zap","inputSchema":{"type":"object"}},
        {"name":"lookup","inputSchema":{"type":"object"}},
    ]));
    let beta = McpService::start().await;
    beta.set_tools(json!([{"name":"fetch","inputSchema":{"type":"object"}}]));
    let mut listed = Vec::new();
    for is_reversed in [false, true] {
        let connections = new_connections();
        let mut configs = vec![
            server_config("alpha", &alpha, ToolExposure::Direct),
            server_config("beta", &beta, ToolExposure::Search),
        ];
        if is_reversed {
            configs.reverse();
        }
        for connected in 1..=configs.len() {
            connect(&connections, &configs[..connected]).await;
        }
        let configs = Arc::new(RwLock::new(configs));
        let mut sources = vec![
            mcp_provider(connections, &configs),
            Arc::new(NamedHostTools(vec!["memo", "aardvark"])) as Arc<dyn HostToolProvider>,
        ];
        if is_reversed {
            sources.reverse();
        }
        let model = Arc::new(ScriptedModel::new(vec![
            tickloop_harness_test_support::text("done"),
        ]));
        let agent = Agent::new_embedded(tickloop_harness_test_support::config(), model.clone());
        agent.install_host_tools(sources).unwrap();
        let (chat, _directory) = full_access_chat(&agent).await;
        run_turn(&agent, chat, "List your tools").await;
        let request = model.requests.lock().unwrap()[0].clone();
        listed.push(tool_bytes(&request));
        let names: Vec<String> = tool_names(&request.tools)
            .into_iter()
            .map(str::to_owned)
            .collect();
        agent.shutdown().await;

        let keys: Vec<_> = names
            .iter()
            .map(|name| (listed_group(name), name.as_str()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "each group in order, sorted by name inside");
        let group = |wanted| {
            keys.iter()
                .filter(|(group, _)| *group == wanted)
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
        };
        assert!(group(ListedGroup::BuiltIn).len() > 1, "{names:?}");
        assert!(group(ListedGroup::Runtime).len() > 1, "{names:?}");
        assert_eq!(
            group(ListedGroup::McpResources),
            ["ListMcpResourcesTool", "ReadMcpResourceTool"]
        );
        assert_eq!(
            group(ListedGroup::Fold),
            ["DescribeTools", "ExecuteTools", "SearchTools"]
        );
        assert_eq!(
            group(ListedGroup::HostTools),
            [
                "aardvark",
                "mcp__alpha__lookup",
                "mcp__alpha__zap",
                "memo"
            ]
        );
    }
    assert!(
        listed[0] == listed[1],
        "registration and connection order changed the tools"
    );
}

/// Host tools that record every question the host asks about one of their tools.
struct RecordingHostTools {
    names: std::sync::Mutex<Vec<&'static str>>,
    permission: HostToolPermission,
    label: &'static str,
    consulted: std::sync::Mutex<Vec<String>>,
}

impl RecordingHostTools {
    fn new(names: Vec<&'static str>, permission: HostToolPermission, label: &'static str) -> Self {
        Self {
            names: std::sync::Mutex::new(names),
            permission,
            label,
            consulted: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn record(&self, question: &str, name: &str) {
        self.consulted
            .lock()
            .unwrap()
            .push(format!("{question} {name}"));
    }
}

#[async_trait]
impl HostToolProvider for RecordingHostTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        NamedHostTools(self.names.lock().unwrap().clone()).definitions()
    }

    fn permission(&self, _: ChatId, name: &str, _: &Value) -> HostToolPermission {
        self.record("permission", name);
        self.permission
    }

    fn tool_authorities(
        &self,
        name: &str,
    ) -> Result<std::collections::BTreeSet<tickloop_harness_protocol::HostToolAuthority>, String>
    {
        self.record("authorities", name);
        Ok(Default::default())
    }

    fn call_authority(
        &self,
        name: &str,
        _: &Value,
    ) -> Result<Option<tickloop_harness_protocol::HostToolAuthority>, String> {
        self.record("call authority", name);
        Ok(None)
    }

    async fn execute(&self, _: ChatId, name: &str, _: Value, _: CancellationToken) -> ToolResult {
        self.record("execute", name);
        ToolResult {
            content: self.label.into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn the_source_that_keeps_a_shared_tool_name_alone_decides_its_permission_and_authority() {
    use tickloop_harness_test_support::{call, config, text};
    let first = Arc::new(RecordingHostTools::new(
        vec!["memo"],
        HostToolPermission::ReadOnly,
        "FIRST_SOURCE",
    ));
    let second = Arc::new(RecordingHostTools::new(
        vec!["note"],
        HostToolPermission::Confirmation,
        "SECOND_SOURCE",
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call("memo", "memo", json!({})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(vec![
            first.clone() as Arc<dyn HostToolProvider>,
            second.clone() as Arc<dyn HostToolProvider>,
        ])
        .unwrap();
    // Installing refuses a name two sources offer; a source may start offering one later.
    *second.names.lock().unwrap() = vec!["memo", "note"];
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        run_turn(&agent, chat, "Write a memo"),
    )
    .await
    .expect("the kept source's read-only permission needs no confirmation");
    agent.shutdown().await;
    let consulted = first.consulted.lock().unwrap().clone();
    assert!(
        consulted.contains(&"permission memo".to_owned())
            && consulted.contains(&"execute memo".to_owned()),
        "{consulted:?}"
    );
    let ignored = second.consulted.lock().unwrap().clone();
    assert!(
        ignored.iter().all(|question| !question.ends_with(" memo")),
        "the source whose tool was ignored is never asked about it: {ignored:?}"
    );
}
