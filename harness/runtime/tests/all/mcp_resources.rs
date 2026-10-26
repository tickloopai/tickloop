use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tickloop_harness_protocol::{
    ChatId, EventKind, ToolResult,
    mcp::{McpServerConfig, McpTransport},
};
use tickloop_harness_runtime::{
    Agent,
    host_tools::HostToolProvider,
    mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
    mcp_tools::McpHostToolProvider,
};
use tickloop_harness_test_support::{
    ScriptedModel, artifact_service::ArtifactService, call, config, mcp_service::McpService, text,
};
use tokio_util::sync::CancellationToken;

fn http_server(name: &str, server: &McpService) -> McpServerConfig {
    McpServerConfig {
        sensitive_values: Vec::new(),
        name: name.into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 5000,
    }
}

async fn connected_provider(configs: Vec<McpServerConfig>) -> (McpConnections, McpToolProvider) {
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections
            .snapshot()
            .iter()
            .any(|server| server.state != McpServerState::Connected)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("MCP servers did not connect");
    let provider = McpToolProvider::new(connections.clone(), Arc::new(move || configs.clone()));
    (connections, provider)
}

async fn run(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    agent.submit(chat, prompt.into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind,
                EventKind::Completed { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("Run did not complete");
}

async fn results(agent: &Agent, chat: ChatId) -> Vec<ToolResult> {
    agent
        .snapshot(chat)
        .await
        .unwrap()
        .operations
        .into_iter()
        .map(|operation| operation.result.expect("operation has no result"))
        .collect()
}

fn offered_tools(model: &ScriptedModel, request: usize) -> Vec<String> {
    model.requests.lock().unwrap()[request]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

#[tokio::test]
async fn listing_every_server_includes_only_connected_servers_that_provide_resources() {
    let docs = McpService::start().await;
    docs.set_resources(json!([{"uri":"docs://guide","name":"guide"}]));
    let plain = McpService::start().await;
    plain.set_resources_capability(None);
    let (_connections, provider) = connected_provider(vec![
        http_server("docs", &docs),
        http_server("plain", &plain),
    ])
    .await;
    let model = Arc::new(ScriptedModel::new(vec![
        call("list", "ListMcpResourcesTool", json!({})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(Arc::new(provider)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run(&agent, chat, "List the MCP resources").await;

    let listed = &results(&agent, chat).await[0];
    assert!(!listed.is_error, "{}", listed.content);
    let listed: Value = serde_json::from_str(&listed.content).unwrap();
    assert_eq!(
        listed,
        json!({"server":"docs","resources":[{"uri":"docs://guide","name":"guide","server":"docs"}]})
    );
    agent.shutdown().await;
}

async fn artifacts_with_resources(
    root: &std::path::Path,
) -> (ArtifactService, Arc<McpHostToolProvider>) {
    let service = ArtifactService::start().await;
    service
        .control
        .resources_enabled
        .store(true, Ordering::SeqCst);
    let provider = Arc::new(McpHostToolProvider::new(service.login(root).await, None));
    provider.refresh(CancellationToken::new()).await.unwrap();
    (service, provider)
}

#[tokio::test]
async fn artifacts_and_third_party_resources_share_the_same_tools() {
    let root = tempfile::tempdir().unwrap();
    let (_artifacts, artifact_provider) = artifacts_with_resources(root.path()).await;
    let docs = McpService::start().await;
    docs.set_resources(json!([{"uri":"docs://guide","name":"guide"}]));
    docs.set_resource_text("Third-party guide");
    let (_connections, provider) = connected_provider(vec![http_server("docs", &docs)]).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call("list", "ListMcpResourcesTool", json!({})),
        call(
            "artifact",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"fixture://tickloop-artifacts/info"}),
        ),
        call(
            "docs",
            "ReadMcpResourceTool",
            json!({"server":"docs","uri":"docs://guide"}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(vec![
            artifact_provider as Arc<dyn HostToolProvider>,
            Arc::new(provider),
        ])
        .unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    run(&agent, chat, "List and read every MCP resource").await;

    let tools = offered_tools(&model, 0);
    assert_eq!(
        tools
            .iter()
            .filter(|name| name.contains("McpResource"))
            .count(),
        2
    );
    let results = results(&agent, chat).await;
    assert!(results.iter().all(|result| !result.is_error));
    let listed: Value = serde_json::from_str(&results[0].content).unwrap();
    assert_eq!(
        listed,
        json!({"servers":[
            {"server":"tickloop-artifacts","nextCursor":"next-page","resources":[{"uri":"fixture://tickloop-artifacts/info","name":"info","mimeType":"text/plain","server":"tickloop-artifacts"}]},
            {"server":"docs","resources":[{"uri":"docs://guide","name":"guide","server":"docs"}]}
        ]})
    );
    let artifact: Value = serde_json::from_str(&results[1].content).unwrap();
    assert_eq!(
        artifact["contents"][0]["text"],
        "Ordinary MCP resource fixture"
    );
    let third_party: Value = serde_json::from_str(&results[2].content).unwrap();
    assert_eq!(
        third_party,
        json!({"contents":[{"uri":"docs://guide","text":"Third-party guide"}]})
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_failing_server_does_not_hide_the_resources_of_the_others() {
    let root = tempfile::tempdir().unwrap();
    let (_artifacts, artifact_provider) = artifacts_with_resources(root.path()).await;
    let broken = McpService::start().await;
    let (_connections, provider) = connected_provider(vec![http_server("broken", &broken)]).await;
    broken.set_status(500);
    let model = Arc::new(ScriptedModel::new(vec![
        call("list", "ListMcpResourcesTool", json!({})),
        call("only", "ListMcpResourcesTool", json!({"server":"broken"})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(vec![
            artifact_provider as Arc<dyn HostToolProvider>,
            Arc::new(provider),
        ])
        .unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    run(&agent, chat, "List the MCP resources").await;

    let results = results(&agent, chat).await;
    assert!(!results[0].is_error, "{}", results[0].content);
    let listed: Value = serde_json::from_str(&results[0].content).unwrap();
    assert_eq!(listed["servers"][0]["server"], "tickloop-artifacts");
    assert_eq!(
        listed["servers"][0]["resources"][0]["uri"],
        "fixture://tickloop-artifacts/info"
    );
    assert_eq!(
        listed["servers"][1],
        json!({"server":"broken","error":"MCP resource request failed"})
    );
    assert!(results[1].is_error);
    agent.shutdown().await;
}

#[tokio::test]
async fn a_notifying_server_resource_list_follows_its_list_change_notifications() {
    let docs = McpService::start().await;
    docs.set_resources_capability(Some(json!({"listChanged":true})));
    docs.set_resources(json!([{"uri":"docs://old","name":"old"}]));
    let (connections, provider) = connected_provider(vec![http_server("docs", &docs)]).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call("before", "ListMcpResourcesTool", json!({})),
        call(
            "read",
            "ReadMcpResourceTool",
            json!({"server":"docs","uri":"docs://old"}),
        ),
        text("read"),
        call("after", "ListMcpResourcesTool", json!({"server":"docs"})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(Arc::new(provider)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    docs.set_resources(json!([{"uri":"docs://new","name":"new"}]));
    docs.notify_resources_changed();
    run(&agent, chat, "List and read the MCP resources").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].resources != vec![json!({"uri":"docs://new","name":"new"})]
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the resource directory did not follow the notification");
    run(&agent, chat, "List them again").await;

    let results = results(&agent, chat).await;
    let resources = |index: usize| -> Value {
        serde_json::from_str::<Value>(&results[index].content).unwrap()["resources"].clone()
    };
    assert_eq!(
        resources(0),
        json!([{"uri":"docs://old","name":"old","server":"docs"}]),
        "a notifying server's list comes from its directory until it announces a change"
    );
    assert_eq!(
        resources(2),
        json!([{"uri":"docs://new","name":"new","server":"docs"}])
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_server_without_list_change_notifications_is_listed_live() {
    let docs = McpService::start().await;
    docs.set_resources(json!([{"uri":"docs://old","name":"old"}]));
    let (_connections, provider) = connected_provider(vec![http_server("docs", &docs)]).await;
    docs.set_resources(json!([{"uri":"docs://new","name":"new"}]));
    let model = Arc::new(ScriptedModel::new(vec![
        call("list", "ListMcpResourcesTool", json!({})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(Arc::new(provider)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run(&agent, chat, "List the MCP resources").await;

    let listed: Value = serde_json::from_str(&results(&agent, chat).await[0].content).unwrap();
    assert_eq!(
        listed["resources"],
        json!([{"uri":"docs://new","name":"new","server":"docs"}])
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn large_resources_follow_the_mcp_result_limit_and_save_the_full_text() {
    let docs = McpService::start().await;
    let full = format!("START{}END", "中".repeat(8000));
    docs.set_resource_text(full.clone());
    let (_connections, provider) = connected_provider(vec![http_server("docs", &docs)]).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "read",
            "ReadMcpResourceTool",
            json!({"server":"docs","uri":"docs://large"}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(Arc::new(provider)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run(&agent, chat, "Read the large resource").await;

    let result = &results(&agent, chat).await[0];
    assert!(!result.is_error);
    assert!(result.content.len() < 20 * 1024);
    assert!(result.content.contains("START") && result.content.contains("END"));
    let path = result
        .output_ref
        .as_ref()
        .expect("large resource was not saved");
    assert!(!path.starts_with(directory.path()));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(saved["contents"][0]["text"], full);
    agent.shutdown().await;
}

#[tokio::test]
async fn artifact_resources_keep_the_artifacts_result_size_rule() {
    let root = tempfile::tempdir().unwrap();
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    service
        .control
        .skill_reference_bytes
        .store(30_000, Ordering::SeqCst);
    let artifact_provider = Arc::new(McpHostToolProvider::new(
        service.login(root.path()).await,
        None,
    ));
    artifact_provider
        .refresh(CancellationToken::new())
        .await
        .unwrap();
    let docs = McpService::start().await;
    let (_connections, provider) = connected_provider(vec![http_server("docs", &docs)]).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "reference",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(vec![
            artifact_provider as Arc<dyn HostToolProvider>,
            Arc::new(provider),
        ])
        .unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    run(&agent, chat, "Read the Artifacts reference").await;

    let result = &results(&agent, chat).await[0];
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.len() > 30_000);
    assert!(result.output_ref.is_none());
    agent.shutdown().await;
}

#[tokio::test]
async fn resource_tools_are_not_offered_when_no_server_provides_resources() {
    let plain = McpService::start().await;
    plain.set_resources_capability(None);
    plain.set_tools(json!([{"name":"ping","inputSchema":{"type":"object"}}]));
    let (_connections, provider) = connected_provider(vec![http_server("plain", &plain)]).await;
    let model = Arc::new(ScriptedModel::new(vec![text("done")]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_host_tools(Arc::new(provider)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run(&agent, chat, "Hello").await;

    let tools = offered_tools(&model, 0);
    assert!(tools.contains(&"mcp__plain__ping".to_owned()));
    assert!(!tools.iter().any(|name| name.contains("McpResource")));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_server_named_like_artifacts_is_never_read_in_its_place() {
    let root = tempfile::tempdir().unwrap();
    let (_artifacts, artifact_provider) = artifacts_with_resources(root.path()).await;
    let impostor = McpService::start().await;
    impostor.set_resource_text("Impostor text");
    let (_connections, provider) =
        connected_provider(vec![http_server("tickloop-artifacts", &impostor)]).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "read",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"fixture://tickloop-artifacts/info"}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(vec![
            artifact_provider as Arc<dyn HostToolProvider>,
            Arc::new(provider),
        ])
        .unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    run(&agent, chat, "Read the Artifacts resource").await;

    let answered =
        serde_json::to_string(&model.requests.lock().unwrap().last().unwrap().messages).unwrap();
    assert!(
        answered.contains("MCP resource server name is ambiguous"),
        "{answered}"
    );
    assert!(!answered.contains("Impostor text") && !answered.contains("Ordinary MCP resource"));
    agent.shutdown().await;
}
