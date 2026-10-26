use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};
use tickloop_code_access::LaunchSpec;
use tickloop_harness::{
    model::{LocalModelClients, ModelClients},
    protocol::{EventKind, ExecutionMode, ModelConfig},
    runtime::Agent,
    test_support::{ScriptedModel, call, harness::ChosenModels, mcp_service::McpService, text},
};

async fn complete(agent: &Agent, chat: tickloop_harness::protocol::ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    agent.submit(chat, prompt.into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("model run failed: {error}"),
                EventKind::PermissionRequested { .. } => {
                    panic!("Full Access unexpectedly requested approval")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("model run timed out");
}
async fn configured_agent(
    root: &Path,
    model: ModelConfig,
    models: Arc<dyn ModelClients>,
) -> (Agent, McpService, McpService) {
    let canonical_root = root.canonicalize().unwrap();
    let root = canonical_root.as_path();
    let local = McpService::start().await;
    let remote = McpService::start().await;
    for (service, marker) in [(&local, "LOCAL_MCP_OK"), (&remote, "HTTP_MCP_OK")] {
        service.set_tools(json!([{"name":"probe","description":"Return the acceptance marker","inputSchema":{"type":"object","properties":{}}}]));
        service.set_result(json!({"content":[{"type":"text","text":marker}]}));
    }
    std::fs::write(
        root.join("settings.json"),
        serde_json::to_vec_pretty(&json!({"mcpServers":{
            "local":{"command":"python3","args":local.stdio_args()},
            "remote":{"url":remote.url()}
        }}))
        .unwrap(),
    )
    .unwrap();
    let mut launch = LaunchSpec::embedded(root.to_owned());
    launch.config_root = Some(root.to_owned());
    launch.model = Some(model);
    launch.session_persistence = false;
    launch.environment = std::env::vars()
        .map(|(name, value)| tickloop_code_access::EnvironmentValue {
            name: name.into_bytes(),
            value: value.into_bytes(),
        })
        .collect();
    let (agent, _) = tickloop_code_daemon::build_runtime_with_models(
        &launch,
        models,
        None,
        &std::env::current_exe().unwrap(),
    )
    .await
    .unwrap();
    (agent, local, remote)
}
#[tokio::test]
async fn worker_configuration_exposes_both_transports_to_the_agent() {
    let root = tempfile::tempdir().unwrap();
    let scripted = Arc::new(ScriptedModel::new(Vec::new()));
    tickloop_harness::base::credentials::save_api_key(
        &root.path().canonicalize().unwrap(),
        "ANTHROPIC_API_KEY",
        "fixture",
    )
    .unwrap();
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    let (agent, local, remote) =
        configured_agent(root.path(), model, ChosenModels::new(scripted.clone())).await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            scripted.enqueue(text("ready"));
            complete(&agent, chat, "Report readiness").await;
            let ready = {
                let requests = scripted.requests.lock().unwrap();
                let tools = &requests.last().unwrap().tools;
                tools.iter().any(|tool| tool.name == "mcp__local__probe")
                    && tools.iter().any(|tool| tool.name == "mcp__remote__probe")
            };
            if ready {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    scripted.enqueue(call("local", "mcp__local__probe", json!({})));
    scripted.enqueue(call("remote", "mcp__remote__probe", json!({})));
    scripted.enqueue(text("done"));
    complete(&agent, chat, "Call both MCP probes").await;
    assert_eq!(local.calls().len(), 1);
    assert_eq!(remote.calls().len(), 1);
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .operations
            .iter()
            .all(|op| !op.result.as_ref().unwrap().is_error)
    );
    agent.shutdown().await;
}
#[tokio::test]
#[ignore = "requires an isolated TICKLOOP_CONFIG_DIR containing ANTHROPIC_API_KEY in .env; calls the billed API"]
async fn live_model_calls_configured_stdio_and_http_servers() {
    let root = std::path::PathBuf::from(
        std::env::var("TICKLOOP_CONFIG_DIR").expect("isolated configuration required"),
    );
    assert!(
        root.to_string_lossy().contains(".scratch/plugin-mcp/"),
        "use this task's scratch configuration, never the user's persistent root"
    );
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    let (agent, local, remote) = configured_agent(
        &root,
        model,
        Arc::new(LocalModelClients::new(Some(root.clone()))),
    )
    .await;
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    complete(
        &agent,
        chat,
        "Wait for the configured MCP services. Reply ready; do not call any tools yet.",
    )
    .await;
    complete(&agent, chat, "Call mcp__local__probe and mcp__remote__probe once each. Reply with both returned acceptance markers. Use no other tools.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = serde_json::to_string(&snapshot.messages).unwrap();
    agent.shutdown().await;
    assert_eq!(local.calls().len(), 1);
    assert_eq!(remote.calls().len(), 1);
    assert!(result.contains("LOCAL_MCP_OK") && result.contains("HTTP_MCP_OK"));
    eprintln!("Live MCP acceptance passed: stdio=1, HTTP=1, model=claude-sonnet-5");
}
