//! The first request's shared startup grace, servers that connect after it, the remembered tool
//! lists and how failed servers reconnect. Times are tokio's paused virtual time: tests assert
//! deadlines and order, never wall-clock durations.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness_protocol::{
    ChatId, Content, EventKind, ModelIdentity,
    mcp::{McpServerConfig, McpTransport, ToolExposure},
};
#[cfg(feature = "test-support")]
use tickloop_harness_runtime::mcp::ConnectionEvent;
use tickloop_harness_runtime::{
    Agent,
    host_tools::HostToolProvider,
    mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
};
use tickloop_harness_test_support::{ScriptedModel, call, config, mcp_service::McpService, text};
use tokio::{sync::mpsc::UnboundedSender, time::Instant};
use tokio_util::sync::CancellationToken;

const GRACE: Duration = Duration::from_secs(2);

/// Moves paused time forward one millisecond per scheduler turn. Paused time otherwise jumps to
/// the next timer whenever the runtime waits for a socket, which would end the grace before a
/// loopback server could answer; this keeps the runtime busy so that time passes evenly.
struct Ticker(tokio::task::JoinHandle<()>);

impl Ticker {
    fn start() -> Self {
        Self(tokio::spawn(async {
            loop {
                tokio::time::advance(Duration::from_millis(1)).await;
                tokio::task::yield_now().await;
            }
        }))
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Records when, after the fixture started, each model request went out.
struct TimedModel {
    script: ScriptedModel,
    started: Instant,
    sent_at: Mutex<Vec<Duration>>,
}

#[async_trait]
impl ModelClient for TimedModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.script.generate_title(request, events, cancel).await
    }

    fn identity(&self) -> ModelIdentity {
        self.script.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.sent_at.lock().unwrap().push(self.started.elapsed());
        self.script.complete(request, events, cancel).await
    }
}

struct Fixture {
    agent: Agent,
    model: Arc<TimedModel>,
    provider: Arc<McpToolProvider>,
    chat: ChatId,
    _folder: tempfile::TempDir,
    _ticker: Ticker,
}

impl Fixture {
    /// An Agent whose Worker starts connecting `servers` at once, as the Daemon does when it
    /// builds the Agent.
    async fn new(servers: Vec<McpServerConfig>, replies: Vec<Vec<Content>>) -> Self {
        Self::with_capabilities(servers, replies, HostCapabilities::default()).await
    }

    async fn with_capabilities(
        servers: Vec<McpServerConfig>,
        replies: Vec<Vec<Content>>,
        capabilities: HostCapabilities,
    ) -> Self {
        let ticker = Ticker::start();
        let model = Arc::new(TimedModel {
            script: ScriptedModel::new(replies),
            started: Instant::now(),
            sent_at: Mutex::new(Vec::new()),
        });
        let provider = Arc::new(McpToolProvider::new(
            McpConnections::new(capabilities, BTreeMap::new(), BTreeMap::new()),
            Arc::new(move || servers.clone()),
        ));
        provider.mcp_server_statuses(CancellationToken::new()).await;
        let agent = Agent::new_embedded(config(), model.clone());
        agent.install_host_tools(provider.clone()).unwrap();
        let folder = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        Self {
            agent,
            model,
            provider,
            chat,
            _folder: folder,
            _ticker: ticker,
        }
    }

    /// Submits `prompt` and returns the Run's events up to its end.
    async fn run(&self, prompt: &str) -> Vec<EventKind> {
        let mut events = self.agent.subscribe();
        self.agent.submit(self.chat, prompt.into()).await.unwrap();
        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(120), events.recv())
                .await
                .expect("the Run ended")
                .unwrap();
            if event.chat_id != self.chat {
                continue;
            }
            let ended = matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
            );
            seen.push(event.kind);
            if ended {
                return seen;
            }
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.model.script.requests.lock().unwrap().clone()
    }

    fn sent_at(&self) -> Vec<Duration> {
        self.model.sent_at.lock().unwrap().clone()
    }

    async fn wait_for(&self, name: &str, state: McpServerState) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self
            .provider
            .mcp_server_statuses(CancellationToken::new())
            .await
            .iter()
            .any(|status| status.name == name && status.state == state)
        {
            assert!(Instant::now() < deadline, "{name} never became {state:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn shutdown(self) {
        self.agent.shutdown().await;
    }
}

fn http_server(name: &str, server: &McpService, exposure: ToolExposure) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 300_000,
        exposure,
        tool_exposure: Vec::new(),
        description: String::new(),
        oauth: Default::default(),
        sensitive_values: Vec::new(),
        disabled: false,
    }
}

fn tool_names(request: &ModelRequest) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

fn deferred_names(request: &ModelRequest) -> Vec<String> {
    request
        .deferred_tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

/// The host notes that describe the MCP server `name`, in conversation order.
fn server_notes(request: &ModelRequest, name: &str) -> Vec<String> {
    let opening = format!("MCP server: {name};");
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } if text.contains(&opening) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// What each Waiting-for-MCP-servers event named, in order.
fn waits(events: &[EventKind]) -> Vec<Vec<String>> {
    events
        .iter()
        .filter_map(|event| match event {
            EventKind::WaitingForMcpServers { servers } => Some(servers.clone()),
            _ => None,
        })
        .collect()
}

fn tool_results(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult { .. } => Some(serde_json::to_string(content).unwrap()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_search_server_connecting_after_the_grace_is_found_from_the_next_request() {
    let late = McpService::start().await;
    late.set_tools(json!([{"name":"quote","description":"Latest stock quote","inputSchema":{"type":"object"}}]));
    late.set_directory_delay(Duration::from_secs(3));
    let fixture = Fixture::new(
        vec![http_server("late", &late, ToolExposure::Search)],
        vec![
            text("first"),
            call("find", "SearchTools", json!({"query":"quote"})),
            text("found"),
        ],
    )
    .await;

    let first_run = fixture.run("hello").await;
    let sent_at = fixture.sent_at();
    assert!(
        sent_at[0] >= GRACE && sent_at[0] < Duration::from_secs(3),
        "the first request waits exactly until the shared grace ends: {sent_at:?}"
    );
    assert_eq!(waits(&first_run), [vec!["late".to_owned()], vec![]]);
    let first = &fixture.requests()[0];
    assert!(!tool_names(first).iter().any(|name| name.contains("late")));
    assert!(
        !deferred_names(first)
            .iter()
            .any(|name| name.contains("late"))
    );
    let connecting = server_notes(first, "late");
    assert_eq!(connecting.len(), 1, "{connecting:?}");
    assert!(
        connecting[0].contains("status: Connecting"),
        "{connecting:?}"
    );

    fixture.wait_for("late", McpServerState::Connected).await;
    let second_run = fixture.run("find the quote tool").await;
    assert!(waits(&second_run).is_empty(), "later requests never wait");
    let requests = fixture.requests();
    assert!(
        tool_results(&requests[2]).contains("mcp__late__quote"),
        "{}",
        tool_results(&requests[2])
    );
    let notes = server_notes(&requests[1], "late");
    assert_eq!(notes[0], connecting[0], "the earlier note is not rewritten");
    assert!(
        notes.len() == 2
            && notes[1].contains("status: Connected")
            && notes[1].contains("can now be found"),
        "{notes:?}"
    );
    let fold = serde_json::to_vec(&requests[0].tools).unwrap();
    assert!(
        requests
            .iter()
            .all(|request| serde_json::to_vec(&request.tools).unwrap() == fold),
        "the Fold definitions never change"
    );
    fixture.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_direct_server_connecting_after_the_grace_is_listed_from_the_next_run() {
    let late = McpService::start().await;
    late.set_tools(json!([{"name":"quote","description":"Latest stock quote","inputSchema":{"type":"object"}}]));
    late.set_directory_delay(Duration::from_secs(3));
    let fixture = Fixture::new(
        vec![http_server("late", &late, ToolExposure::Direct)],
        vec![text("first"), text("second")],
    )
    .await;

    let first_run = fixture.run("hello").await;
    let sent_at = fixture.sent_at();
    assert!(
        sent_at[0] >= GRACE && sent_at[0] < Duration::from_secs(3),
        "a direct server gets no longer than the shared grace: {sent_at:?}"
    );
    assert_eq!(waits(&first_run), [vec!["late".to_owned()], vec![]]);
    assert!(!tool_names(&fixture.requests()[0]).contains(&"mcp__late__quote".to_owned()));

    fixture.wait_for("late", McpServerState::Connected).await;
    fixture.run("again").await;
    let next_run = &fixture.requests()[1];
    assert!(tool_names(next_run).contains(&"mcp__late__quote".to_owned()));
    assert!(!deferred_names(next_run).contains(&"mcp__late__quote".to_owned()));
    let notes = server_notes(next_run, "late");
    assert!(
        notes.len() == 2
            && notes[0].contains("status: Connecting")
            && notes[1].contains("status: Connected")
            && notes[1].contains("can be called once they appear in your tool list"),
        "{notes:?}"
    );
    fixture.shutdown().await;
}

#[tokio::test(start_paused = true)]
#[ignore = "needs ticket 16: the tools array is frozen within a Run"]
async fn a_direct_server_joining_during_a_run_leaves_that_runs_tools_unchanged() {
    use tickloop_harness_protocol::ExecutionMode;
    let late = McpService::start().await;
    late.set_tools(json!([{"name":"quote","inputSchema":{"type":"object"}}]));
    late.set_directory_delay(Duration::from_secs(3));
    let clock = McpService::start().await;
    clock.set_tools(json!([{"name":"tick","inputSchema":{"type":"object"}}]));
    clock.set_delay(Duration::from_secs(2));
    let fixture = Fixture::new(
        vec![
            http_server("late", &late, ToolExposure::Direct),
            http_server("clock", &clock, ToolExposure::Direct),
        ],
        vec![
            call("tick", "mcp__clock__tick", json!({})),
            text("done"),
            text("next"),
        ],
    )
    .await;
    fixture
        .agent
        .set_execution_mode(fixture.chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    fixture.run("tick").await;
    let requests = fixture.requests();
    assert!(
        fixture.sent_at()[1] > Duration::from_secs(3),
        "late joined mid-Run"
    );
    assert_eq!(
        tool_names(&requests[1]),
        tool_names(&requests[0]),
        "the tools array does not change within a Run"
    );
    assert!(
        serde_json::to_vec(&requests[1].tools).unwrap()
            == serde_json::to_vec(&requests[0].tools).unwrap()
    );
    fixture.run("next").await;
    assert!(tool_names(&fixture.requests()[2]).contains(&"mcp__late__quote".to_owned()));
    fixture.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_direct_server_connecting_within_the_grace_is_listed_in_the_first_request() {
    let early = McpService::start().await;
    early.set_tools(json!([{"name":"quote","inputSchema":{"type":"object"}}]));
    early.set_directory_delay(Duration::from_secs(1));
    let fixture = Fixture::new(
        vec![http_server("early", &early, ToolExposure::Direct)],
        vec![text("first")],
    )
    .await;
    let run = fixture.run("hello").await;
    let sent_at = fixture.sent_at();
    assert!(
        sent_at[0] >= Duration::from_secs(1) && sent_at[0] < GRACE,
        "the request goes out once the server connects: {sent_at:?}"
    );
    assert_eq!(waits(&run), [vec!["early".to_owned()], vec![]]);
    assert!(tool_names(&fixture.requests()[0]).contains(&"mcp__early__quote".to_owned()));
    fixture.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn failed_servers_do_not_hold_the_first_request_and_are_reported_failed() {
    let down = McpService::start().await;
    down.set_status(500);
    let mut missing = http_server("missing", &down, ToolExposure::Direct);
    missing.transport = McpTransport::Stdio {
        command: "/nonexistent/tickloop-mcp-server".into(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: std::env::temp_dir(),
    };
    let fixture = Fixture::with_capabilities(
        vec![http_server("down", &down, ToolExposure::Direct), missing],
        vec![text("first")],
        HostCapabilities {
            local_processes: true,
        },
    )
    .await;
    fixture.run("hello").await;
    let sent_at = fixture.sent_at();
    assert!(sent_at[0] <= GRACE, "{sent_at:?}");
    let first = &fixture.requests()[0];
    for name in ["down", "missing"] {
        let notes = server_notes(first, name);
        assert!(
            notes.len() == 1 && notes[0].contains("status: Failed"),
            "{name}: {notes:?}"
        );
    }
    fixture.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn interrupting_during_the_grace_ends_the_run_without_a_request() {
    let late = McpService::start().await;
    late.set_directory_delay(Duration::from_secs(3));
    let fixture = Fixture::new(
        vec![http_server("late", &late, ToolExposure::Search)],
        Vec::new(),
    )
    .await;
    let mut events = fixture.agent.subscribe();
    fixture
        .agent
        .submit(fixture.chat, "hello".into())
        .await
        .unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if matches!(event.kind, EventKind::WaitingForMcpServers { ref servers } if !servers.is_empty())
        {
            break;
        }
    }
    let interrupted_at = fixture.model.started.elapsed();
    fixture.agent.interrupt(fixture.chat).await.unwrap();
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Interrupted => break,
            EventKind::Completed { .. } | EventKind::Failed(_) => {
                panic!("the Run was not interrupted")
            }
            _ => {}
        }
    }
    assert!(interrupted_at < GRACE);
    assert!(fixture.requests().is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_local_server_that_exits_starts_once_until_reconnected() {
    let folder = tempfile::tempdir().unwrap();
    let starts = folder.path().join("starts");
    let mut exits = http_server("exits", &McpService::start().await, ToolExposure::Search);
    exits.transport = McpTransport::Stdio {
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            format!("echo started >> '{}'", starts.display()),
        ],
        env: BTreeMap::new(),
        cwd: folder.path().into(),
    };
    let fixture = Fixture::with_capabilities(
        vec![exits],
        (0..10).map(|_| text("ok")).collect(),
        HostCapabilities {
            local_processes: true,
        },
    )
    .await;
    let started = || {
        std::fs::read_to_string(&starts)
            .unwrap_or_default()
            .lines()
            .count()
    };
    fixture.wait_for("exits", McpServerState::Failed).await;
    for request in 0..10 {
        fixture.run(&format!("request {request}")).await;
    }
    assert_eq!(fixture.requests().len(), 10);
    assert_eq!(started(), 1, "requests never start a failed program again");
    fixture.agent.reconnect_mcp_server("exits").await.unwrap();
    fixture.wait_for("exits", McpServerState::Failed).await;
    assert_eq!(started(), 2, "reconnecting starts it once more");
    fixture.shutdown().await;
}

#[cfg(feature = "test-support")]
/// When each connection attempt of `name` began, after `since`.
fn attempts(connections: &McpConnections, name: &str, since: Instant) -> Vec<Duration> {
    connections
        .connection_events(name)
        .into_iter()
        .filter(|(event, _)| *event == ConnectionEvent::Attempt)
        .map(|(_, at)| at - since)
        .collect()
}

#[cfg(feature = "test-support")]
/// How long each reconnect waited after the failure before it.
fn backoffs(connections: &McpConnections, name: &str) -> Vec<Duration> {
    connections
        .connection_events(name)
        .windows(2)
        .filter_map(|pair| match pair {
            [
                (ConnectionEvent::Failure, failed),
                (ConnectionEvent::Attempt, attempted),
            ] => Some(*attempted - *failed),
            _ => None,
        })
        .collect()
}

#[cfg(feature = "test-support")]
async fn wait_for_connection_state(connections: &McpConnections, state: McpServerState) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while connections.statuses()[0].state != state {
        assert!(Instant::now() < deadline, "never became {state:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(feature = "test-support")]
#[tokio::test(start_paused = true)]
async fn a_remote_server_reconnects_on_the_backoff_schedule_then_stays_failed() {
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let server = McpService::start().await;
    let mut unreachable = http_server("remote", &server, ToolExposure::Search);
    // Refused before any network traffic, so that only the backoff takes time.
    unreachable.transport = McpTransport::Http {
        url: "http://example.invalid/mcp".into(),
        headers: BTreeMap::new(),
    };
    let started = Instant::now();
    connections
        .reconcile_keeping_failures(vec![unreachable.clone()])
        .await;
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(
        attempts(&connections, "remote", started),
        [0, 1, 3, 7, 15, 31].map(Duration::from_secs),
        "the first attempt and five reconnects after 1, 2, 4, 8 and 16 seconds"
    );
    assert_eq!(connections.statuses()[0].state, McpServerState::Failed);
    connections
        .reconcile_keeping_failures(vec![unreachable.clone()])
        .await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(
        attempts(&connections, "remote", started).len(),
        6,
        "an exhausted server stays failed until something changes"
    );
    assert!(connections.reconnect(vec![unreachable], "remote").await);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        attempts(&connections, "remote", started).len(),
        7,
        "reconnecting from /mcp starts again"
    );
    connections.shutdown().await;
}

#[cfg(feature = "test-support")]
#[tokio::test(start_paused = true)]
async fn a_successful_connection_restarts_the_backoff() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"quote","inputSchema":{"type":"object"}}]));
    server.set_status(500);
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile_keeping_failures(vec![http_server("remote", &server, ToolExposure::Search)])
        .await;
    // Answer from the third attempt on, after two failed ones.
    while connections
        .connection_events("remote")
        .iter()
        .filter(|(event, _)| *event == ConnectionEvent::Failure)
        .count()
        < 2
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    server.set_status(200);
    wait_for_connection_state(&connections, McpServerState::Connected).await;
    server.set_status(404);
    let _ = connections
        .call("mcp__remote__quote", json!({}), CancellationToken::new())
        .await;
    while backoffs(&connections, "remote").len() < 3 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        backoffs(&connections, "remote"),
        [1, 2, 1].map(Duration::from_secs),
        "after a successful connection the backoff starts again at one second"
    );
    connections.shutdown().await;
}

#[tokio::test]
async fn tools_stay_listed_while_listed_again_and_after_listing_fails() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"quote","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let config = http_server("remote", &server, ToolExposure::Search);
    let provider =
        McpToolProvider::new(connections.clone(), Arc::new(move || vec![config.clone()]));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let searchable = || {
        provider
            .discovery_tools()
            .iter()
            .any(|tool| tool.definition.name == "mcp__remote__quote")
    };
    assert!(searchable());
    server.set_directory_delay(Duration::from_millis(500));
    server.set_directory_error(Some(
        json!({"code":-32603,"message":"directory unavailable"}),
    ));
    server.notify_tools_changed();
    let listings = server.arrivals("tools/list").len();
    assert!(matches!(
        connections
            .call("mcp__remote__quote", json!({}), CancellationToken::new())
            .await,
        tickloop_harness_runtime::host_tools::HostCallResult::Completed(_)
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            searchable(),
            "the tools stay listed while they are listed again"
        );
        let status = provider
            .mcp_server_statuses(CancellationToken::new())
            .await
            .remove(0);
        if server.arrivals("tools/list").len() > listings
            && connections.snapshot()[0]
                .warnings
                .iter()
                .any(|warning| warning.contains("last listed tools remain"))
        {
            assert_eq!(status.state, McpServerState::Connected);
            break;
        }
        assert!(Instant::now() < deadline, "listing again never failed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(searchable(), "a failed listing keeps the last listed tools");
    provider.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_stay_listed_while_a_login_is_renewed_on_the_open_connection() {
    use tick_oauth_test_server::{AuthorizationServer, METADATA_PATH, TOKEN_PATH};
    use tickloop_harness_connector::{Connector, ConnectorCredentials, OAuthSettings};
    use tickloop_harness_runtime::mcp::InMemoryCredentials;
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"quote","inputSchema":{"type":"object"}}]));
    let authorization = AuthorizationServer::start();
    let credentials = Arc::new(InMemoryCredentials::default());
    let metadata = format!("{}{METADATA_PATH}", authorization.issuer());
    let oauth = tickloop_harness_protocol::mcp::McpOAuthConfig {
        authorization_server_metadata_url: Some(metadata.clone()),
        ..Default::default()
    };
    let connector = Connector::new(
        &server.url(),
        OAuthSettings {
            authorization_server_metadata_url: Some(metadata),
            ..Default::default()
        },
        credentials.clone(),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let login = connector.start_login(None, &cancel).await.unwrap();
    login
        .paste()
        .submit(&authorization.approve(login.authorization_url()))
        .unwrap();
    login.finish(&cancel).await.unwrap();
    server.require_bearer("access-1");
    let mut config = http_server("remote", &server, ToolExposure::Search);
    config.oauth = oauth;
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::from([(
            "remote".to_owned(),
            credentials.clone() as Arc<dyn ConnectorCredentials>,
        )]),
        BTreeMap::new(),
    );
    let provider =
        McpToolProvider::new(connections.clone(), Arc::new(move || vec![config.clone()]));
    let searchable = || {
        provider
            .discovery_tools()
            .iter()
            .any(|tool| tool.definition.name == "mcp__remote__quote")
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !searchable() {
        provider.refresh(CancellationToken::new()).await.unwrap();
        assert!(Instant::now() < deadline, "the server never connected");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let renewals = authorization.requests_to(TOKEN_PATH).len();
    let connected = server.arrivals("initialize").len();
    let mut expiring = credentials.current().unwrap();
    expiring.token.as_mut().unwrap().expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 10;
    credentials.replace(Some(expiring));
    server.require_bearer("access-2");
    let deadline = Instant::now() + Duration::from_secs(10);
    while authorization.requests_to(TOKEN_PATH).len() == renewals {
        provider.refresh(CancellationToken::new()).await.unwrap();
        assert!(
            searchable(),
            "the tools stay listed while the login is renewed"
        );
        assert!(Instant::now() < deadline, "the login was never renewed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for _ in 0..20 {
        provider.refresh(CancellationToken::new()).await.unwrap();
        assert!(
            searchable(),
            "the tools stay listed after the login is renewed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(matches!(
        connections
            .call("mcp__remote__quote", json!({}), CancellationToken::new())
            .await,
        tickloop_harness_runtime::host_tools::HostCallResult::Completed(_)
    ));
    assert_eq!(
        server.arrivals("initialize").len(),
        connected,
        "the renewed token is used on the open connection"
    );
    provider.shutdown().await;
}

/// The arrival every combination of server `exposure` and `toolExposure` gives a tool.
#[tokio::test(start_paused = true)]
async fn the_users_configuration_alone_decides_how_each_tool_arrives() {
    use tickloop_harness_runtime::tool_exposure::{ExposureDirectory, SearchSupport};
    let server = McpService::start().await;
    // Hints the server gives about loading its tools change nothing.
    let hint = json!({"anthropic/alwaysLoad": true});
    server.set_tools(json!([
        {"name":"exact","inputSchema":{"type":"object"},"_meta":hint},
        {"name":"wild_one","inputSchema":{"type":"object"},"_meta":hint},
        {"name":"plain","inputSchema":{"type":"object"},"_meta":hint}
    ]));
    let others = |value: ToolExposure| {
        [
            ToolExposure::Search,
            ToolExposure::Direct,
            ToolExposure::Hidden,
        ]
        .into_iter()
        .find(|other| *other != value)
        .unwrap()
    };
    let server_exposures = [
        ("unset", None),
        ("search", Some(ToolExposure::Search)),
        ("direct", Some(ToolExposure::Direct)),
        ("hidden", Some(ToolExposure::Hidden)),
    ];
    let mut configs = Vec::new();
    let mut expected = BTreeMap::new();
    for (label, server_exposure) in server_exposures {
        for value in [
            ToolExposure::Search,
            ToolExposure::Direct,
            ToolExposure::Hidden,
        ] {
            let name = format!("{label}_{}", value.as_str());
            let mut config = http_server(&name, &server, server_exposure.unwrap_or_default());
            config.tool_exposure = vec![
                // A wildcard declared before the exact entry still loses to it.
                ("*t".into(), others(value)),
                // The first matching wildcard in declaration order decides.
                ("wild_*".into(), value),
                ("*w*".into(), others(value)),
                ("exact".into(), value),
            ];
            expected.insert(format!("mcp__{name}__exact"), value);
            expected.insert(format!("mcp__{name}__wild_one"), value);
            expected.insert(
                format!("mcp__{name}__plain"),
                server_exposure.unwrap_or(ToolExposure::Search),
            );
            configs.push(config);
        }
    }
    let names: Vec<_> = configs.iter().map(|config| config.name.clone()).collect();
    let fixture = Fixture::new(configs, vec![text("listed")]).await;
    for name in &names {
        fixture.wait_for(name, McpServerState::Connected).await;
    }
    fixture.run("hello").await;
    let request = &fixture.requests()[0];
    let listed = tool_names(request);
    let directory = ExposureDirectory::new(fixture.provider.discovery_tools(), SearchSupport::Fold);
    for (tool, exposure) in &expected {
        let found = directory
            .search(&json!({"query": format!("select:{tool}")}))
            .unwrap()["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|found| found["name"] == tool.as_str());
        let arrival = match (listed.contains(tool), found) {
            (true, _) => ToolExposure::Direct,
            (false, true) => ToolExposure::Search,
            (false, false) => ToolExposure::Hidden,
        };
        assert_eq!(arrival, *exposure, "{tool}");
        if *exposure == ToolExposure::Hidden {
            let call = fixture
                .provider
                .execute(fixture.chat, tool, json!({}), CancellationToken::new())
                .await;
            assert!(call.is_error, "{tool}: {}", call.content);
        }
    }
    fixture.shutdown().await;
}
