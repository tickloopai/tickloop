//! Remote MCP servers whose login the host keeps (a host bearer), and hosts that let the Harness
//! reach only public addresses.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tickloop_harness_base::network::Reach;
use tickloop_harness_protocol::mcp::{McpServerConfig, McpTransport, ToolExposure};
use tickloop_harness_runtime::{
    host_tools::{HostCallResult, HostToolProvider},
    mcp::{
        BearerUnavailable, HostBearer, HostCapabilities, McpConnections, McpServerState,
        McpToolProvider,
    },
};
use tickloop_harness_test_support::mcp_service::McpService;
use tokio_util::sync::CancellationToken;

pub(crate) const FIRST_TOKEN: &str = "host-token-first-7f3a";
pub(crate) const SECOND_TOKEN: &str = "host-token-second-91c2";
const THIRD_TOKEN: &str = "host-token-third-4be8";
const REJECTED_TOKEN: &str = "host-token-refused-0d17";
const FOURTH_TOKEN: &str = "host-token-fourth-c55e";
const TOOL: &str = "mcp__remote__probe";

/// A host's login: offers its current token, and on a rejection moves to the renewal it holds.
#[derive(Default)]
pub(crate) struct HostLogin {
    account: String,
    token: Mutex<Option<String>>,
    renewal: Mutex<Option<String>>,
    unavailable: AtomicBool,
    invalidated: Mutex<Vec<String>>,
}

impl HostLogin {
    pub(crate) fn new(account: &str, token: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            account: account.into(),
            token: Mutex::new(token.map(Into::into)),
            ..Default::default()
        })
    }

    pub(crate) fn offer(&self, token: Option<&str>) {
        *self.token.lock().unwrap() = token.map(Into::into);
    }

    fn renew_to(&self, token: &str) {
        *self.renewal.lock().unwrap() = Some(token.into());
    }

    fn invalidated(&self) -> Vec<String> {
        self.invalidated.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl HostBearer for HostLogin {
    fn account(&self) -> String {
        self.account.clone()
    }

    async fn bearer(&self) -> Result<Option<String>, BearerUnavailable> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(BearerUnavailable);
        }
        Ok(self.token.lock().unwrap().clone())
    }

    async fn invalidate(&self, rejected_token: &str) -> Result<(), BearerUnavailable> {
        self.invalidated.lock().unwrap().push(rejected_token.into());
        let mut token = self.token.lock().unwrap();
        if token.as_deref() == Some(rejected_token) {
            *token = self.renewal.lock().unwrap().take();
        }
        Ok(())
    }
}

pub(crate) fn remote(url: &str) -> McpServerConfig {
    McpServerConfig {
        exposure: ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
        name: "remote".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: url.into(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 1000,
        oauth: Default::default(),
    }
}

fn connections_with(login: &Arc<HostLogin>, reach: Reach) -> McpConnections {
    let connections = McpConnections::new(
        HostCapabilities {
            reach,
            ..Default::default()
        },
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.set_bearers(BTreeMap::from([(
        "remote".to_owned(),
        login.clone() as Arc<dyn HostBearer>,
    )]));
    connections
}

async fn wait_for_state(connections: &McpConnections, state: McpServerState) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while connections.snapshot()[0].state != state {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never became {state:?}: {:?}", connections.statuses()));
}

async fn call(connections: &McpConnections) -> HostCallResult {
    connections
        .call(TOOL, json!({}), CancellationToken::new())
        .await
}

fn authority(provider: &McpToolProvider) -> String {
    format!("{:?}", provider.call_authority(TOOL, &Value::Null).unwrap())
}

#[tokio::test]
async fn a_host_login_renews_on_the_open_connection_and_a_rejection_is_retried_once() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    server.require_bearer(FIRST_TOKEN);
    let login = HostLogin::new("account-a", Some(FIRST_TOKEN));
    let connections = connections_with(&login, Reach::Anywhere);
    let configs = vec![remote(&server.url())];
    let provider = McpToolProvider::new(connections.clone(), {
        let configs = configs.clone();
        Arc::new(move || configs.clone())
    });
    connections.reconcile(configs.clone()).await;
    wait_for_state(&connections, McpServerState::Connected).await;
    assert!(matches!(
        call(&connections).await,
        HostCallResult::Completed(_)
    ));
    let status = &connections.statuses()[0];
    assert!(status.oauth && status.logged_in, "{status:?}");
    let initializations = server.arrivals("initialize").len();
    let first_authority = authority(&provider);

    // The host renewed the token itself: the next request carries it on the same connection.
    login.offer(Some(SECOND_TOKEN));
    server.require_bearer(SECOND_TOKEN);
    server.set_result(json!({"content":[{"type":"text","text":format!("echo {SECOND_TOKEN}")}]}));
    connections.reconcile(configs.clone()).await;
    let HostCallResult::Completed(result) = call(&connections).await else {
        panic!("the renewed token is accepted");
    };
    assert!(!result.content.contains(SECOND_TOKEN), "{}", result.content);
    assert_eq!(server.arrivals("initialize").len(), initializations);
    assert_eq!(authority(&provider), first_authority);
    assert!(login.invalidated().is_empty());

    // The server rejects the token: the host drops it and the request goes once more.
    server.require_bearer(THIRD_TOKEN);
    login.renew_to(THIRD_TOKEN);
    assert!(matches!(
        call(&connections).await,
        HostCallResult::Completed(_)
    ));
    assert_eq!(login.invalidated(), [SECOND_TOKEN]);
    assert_eq!(server.arrivals("initialize").len(), initializations);
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    assert_eq!(authority(&provider), first_authority);

    // The renewed token is refused as well: the server waits for the user to log in again.
    server.require_bearer(FOURTH_TOKEN);
    login.renew_to(REJECTED_TOKEN);
    assert!(!matches!(
        call(&connections).await,
        HostCallResult::Completed(_)
    ));
    assert_eq!(login.invalidated(), [SECOND_TOKEN, THIRD_TOKEN]);
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    assert!(!connections.statuses()[0].logged_in);
    let requests = server.request_count();
    connections.reconcile(configs.clone()).await;
    assert_eq!(
        connections.snapshot()[0].state,
        McpServerState::NeedsLogin,
        "the host still offers the refused token"
    );
    assert_eq!(server.request_count(), requests);

    login.offer(Some(FOURTH_TOKEN));
    connections.reconcile(configs.clone()).await;
    wait_for_state(&connections, McpServerState::Connected).await;
    assert!(matches!(
        call(&connections).await,
        HostCallResult::Completed(_)
    ));
    assert_eq!(authority(&provider), first_authority);
    for shown in [
        format!("{:?}", connections.statuses()),
        format!("{:?}", connections.snapshot()),
        first_authority,
    ] {
        for token in [
            FIRST_TOKEN,
            SECOND_TOKEN,
            THIRD_TOKEN,
            REJECTED_TOKEN,
            FOURTH_TOKEN,
        ] {
            assert!(!shown.contains(token), "{shown}");
        }
    }
    connections.shutdown().await;
}

#[tokio::test]
async fn another_account_is_another_authority_and_connects_again() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    let first = HostLogin::new("account-a", Some(FIRST_TOKEN));
    let connections = connections_with(&first, Reach::Anywhere);
    let configs = vec![remote(&server.url())];
    let provider = McpToolProvider::new(connections.clone(), {
        let configs = configs.clone();
        Arc::new(move || configs.clone())
    });
    connections.reconcile(configs.clone()).await;
    wait_for_state(&connections, McpServerState::Connected).await;
    let first_authority = authority(&provider);
    let initializations = server.arrivals("initialize").len();

    let second = HostLogin::new("account-b", Some(SECOND_TOKEN));
    connections.set_bearers(BTreeMap::from([(
        "remote".to_owned(),
        second.clone() as Arc<dyn HostBearer>,
    )]));
    connections.reconcile(configs).await;
    wait_for_state(&connections, McpServerState::Connected).await;
    assert!(server.arrivals("initialize").len() > initializations);
    assert_ne!(authority(&provider), first_authority);
    connections.shutdown().await;
}

#[tokio::test]
async fn without_a_host_token_the_server_needs_login_and_the_host_says_where() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    let login = HostLogin::new("account-a", None);
    let connections = connections_with(&login, Reach::Anywhere);
    let configs = vec![remote(&server.url())];
    let provider = McpToolProvider::new(connections.clone(), {
        let configs = configs.clone();
        Arc::new(move || configs.clone())
    })
    .with_login_guidance(|name| {
        format!("ask the user to reconnect {name} in Settings → Connectors.")
    });
    provider.refresh(CancellationToken::new()).await.unwrap();
    wait_for_state(&connections, McpServerState::NeedsLogin).await;
    assert_eq!(server.request_count(), 0, "no request goes without a token");
    assert_eq!(
        provider.mcp_instructions()[0].status.as_deref(),
        Some("needs login; ask the user to reconnect remote in Settings → Connectors.")
    );

    login.unavailable.store(true, Ordering::SeqCst);
    login.offer(Some(FIRST_TOKEN));
    provider.refresh(CancellationToken::new()).await.unwrap();
    wait_for_state(&connections, McpServerState::NeedsLogin).await;
    assert_eq!(server.request_count(), 0);

    login.unavailable.store(false, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    wait_for_state(&connections, McpServerState::Connected).await;
    assert_eq!(provider.mcp_instructions()[0].status, None);
    connections.shutdown().await;
}

#[tokio::test]
async fn a_host_that_cannot_answer_for_its_login_leaves_the_server_failed() {
    let server = McpService::start().await;
    let login = HostLogin::new("account-a", Some(FIRST_TOKEN));
    login.unavailable.store(true, Ordering::SeqCst);
    let connections = connections_with(&login, Reach::Anywhere);
    connections.reconcile(vec![remote(&server.url())]).await;
    wait_for_state(&connections, McpServerState::Failed).await;
    assert_eq!(
        connections.statuses()[0].failure.as_deref(),
        Some("MCP credential unavailable")
    );
    assert_eq!(server.request_count(), 0);
    connections.shutdown().await;
}

#[tokio::test]
async fn public_reach_refuses_servers_on_this_machine_and_private_networks() {
    let server = McpService::start().await;
    let port = reqwest::Url::parse(&server.url()).unwrap().port().unwrap();
    for (url, refused) in [
        (server.url(), "127.0.0.1 is not a public address".to_owned()),
        (
            "https://10.0.0.1/mcp".to_owned(),
            "10.0.0.1 is not a public address".to_owned(),
        ),
        (
            "https://169.254.169.254/mcp".to_owned(),
            "169.254.169.254 is not a public address".to_owned(),
        ),
        (
            format!("http://localhost:{port}/mcp"),
            "localhost resolves to ".to_owned(),
        ),
    ] {
        let connections = McpConnections::new(
            HostCapabilities {
                reach: Reach::PublicAddresses,
                ..Default::default()
            },
            BTreeMap::new(),
            BTreeMap::new(),
        );
        connections.reconcile(vec![remote(&url)]).await;
        wait_for_state(&connections, McpServerState::Failed).await;
        let status = &connections.statuses()[0];
        let failure = status.failure.clone().unwrap_or_default();
        assert!(
            failure.contains(&refused)
                && failure.contains("this Chat reaches only public addresses"),
            "{url}: {failure}"
        );
        assert!(!status.oauth, "the Harness logs in to nothing itself");
        connections.shutdown().await;
    }
    assert_eq!(server.request_count(), 0);
}

/// A host that writes rules for a server's tools before it connects names them as the catalog
/// will: normalized, or hashed when too long or when two tools would share a name.
#[tokio::test]
async fn a_host_names_a_servers_tools_as_the_catalog_does() {
    let long = "summarize_every_document_in_the_shared_quarterly_reporting_folder";
    let originals = ["read", "notes.read", "notes_read", long];
    let server = McpService::start().await;
    server.set_tools(Value::Array(
        originals
            .iter()
            .map(|name| json!({"name": name, "inputSchema": {"type": "object"}}))
            .collect(),
    ));
    let login = HostLogin::new("account-a", Some(FIRST_TOKEN));
    let connections = connections_with(&login, Reach::Anywhere);
    connections.reconcile(vec![remote(&server.url())]).await;
    wait_for_state(&connections, McpServerState::Connected).await;
    let names = tickloop_harness_runtime::mcp::mcp_tool_names("remote", originals);
    assert_eq!(names["read"], "mcp__remote__read");
    assert_ne!(names["notes.read"], names["notes_read"]);
    assert!(names[long].len() <= 64, "{}", names[long]);
    let listed = connections.snapshot()[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        listed,
        names
            .into_values()
            .collect::<std::collections::BTreeSet<_>>()
    );
    connections.shutdown().await;
}

/// Two requests take up a newly rotated token at once, as an event stream reconnecting beside a
/// call does: the second goes on only once the redactor the first is installing covers the
/// token, so a server's echo of it is redacted for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rotated_token_is_redacted_from_every_echo_while_its_redactor_is_installed() {
    use tickloop_harness_runtime::mcp::test_pauses::{PausePoint, pause_next};
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    let login = HostLogin::new("account-a", Some(FIRST_TOKEN));
    let connections = connections_with(&login, Reach::Anywhere);
    connections.reconcile(vec![remote(&server.url())]).await;
    wait_for_state(&connections, McpServerState::Connected).await;
    let echo = format!("echo {SECOND_TOKEN}");

    let pause = pause_next(PausePoint::RedactorBuild, "remote");
    let first = tokio::task::spawn_blocking({
        let connections = connections.clone();
        let echo = echo.clone();
        move || {
            connections.send_host_token("remote", SECOND_TOKEN);
            connections.redact_for("remote", &echo)
        }
    });
    let entered = pause.entered;
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || entered.recv()),
    )
    .await
    .expect("the first request installs the new token's redactor")
    .unwrap()
    .unwrap();
    let second = tokio::task::spawn_blocking({
        let connections = connections.clone();
        let echo = echo.clone();
        move || {
            connections.send_host_token("remote", SECOND_TOKEN);
            connections.redact_for("remote", &echo)
        }
    });
    // Long enough for the second request to go ahead if it did not wait for the redactor.
    tokio::time::sleep(Duration::from_millis(300)).await;
    pause.release.send(()).unwrap();
    for redacted in [first.await.unwrap(), second.await.unwrap()] {
        assert_eq!(redacted, "echo [redacted]");
    }
    connections.shutdown().await;
}
