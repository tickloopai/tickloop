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

#[tokio::test]
async fn plugin_mcp_tools_use_install_names_deduplicate_and_stop_on_the_next_request() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    tickloop_harness::base::credentials::save_api_key(&canonical, "ANTHROPIC_API_KEY", "fixture")
        .unwrap();
    let scripted = Arc::new(ScriptedModel::new(Vec::new()));
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    let (agent, _, remote) =
        configured_agent(&canonical, model, ChosenModels::new(scripted.clone())).await;
    let store = tick_plugins::PluginStore::in_config_root(&canonical);
    let alpha = McpService::start().await;
    let beta = McpService::start().await;
    for (name, service) in [("alpha", &alpha), ("beta", &beta)] {
        service.set_tools(json!([{"name":"probe", "description":"probe", "inputSchema":{"type":"object","properties":{}}}]));
        let source = canonical.join(format!("source-{name}"));
        std::fs::create_dir_all(source.join(".claude-plugin")).unwrap();
        std::fs::write(source.join(".claude-plugin/plugin.json"), json!({"name":name,"mcpServers":{
            "shared":{"url":service.url(), "autoApprove":["*"], "permissions":{"allow":["*"]}}, "duplicate":{"url":remote.url()},
            "config":{"command":"never-run-this", "args":["${user_config.key}"]}
        }}).to_string()).unwrap();
        store.install_path(&source).unwrap();
    }
    let chat = agent.create_chat(canonical.clone()).await.unwrap();
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
                tools
                    .iter()
                    .any(|t| t.name == "mcp__plugin_alpha_shared__probe")
                    && tools
                        .iter()
                        .any(|t| t.name == "mcp__plugin_beta_shared__probe")
            };
            if ready {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("enabled plugin tools should reach the model");
    {
        let requests = scripted.requests.lock().unwrap();
        assert!(
            !requests
                .last()
                .unwrap()
                .tools
                .iter()
                .any(|t| t.name.contains("duplicate") || t.name.contains("_config__"))
        );
    }
    {
        let requests = scripted.requests.lock().unwrap();
        let context = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
        assert!(
            context.contains("plugin_alpha_duplicate skipped"),
            "deduplication must be visible to the model"
        );
        assert!(context.contains("autoApprove ignored"));
    }
    scripted.enqueue(call("alpha", "mcp__plugin_alpha_shared__probe", json!({})));
    scripted.enqueue(text("done"));
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Call alpha probe".into()).await.unwrap();
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } => break request,
                EventKind::ToolStarted { .. } => panic!("plugin bypassed approval"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(alpha.calls().is_empty());
    agent
        .answer_permission(chat, approval.id, true)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
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
    .unwrap();
    assert_eq!(alpha.calls().len(), 1);
    let settings_path = canonical.join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    settings["mcpServers"]["mine"] = json!({"url":alpha.url()});
    std::fs::write(settings_path, settings.to_string()).unwrap();
    scripted.enqueue(text("configuration changed"));
    complete(&agent, chat, "Report the updated services").await;
    {
        let requests = scripted.requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert!(
            !request
                .tools
                .iter()
                .any(|tool| tool.name == "mcp__plugin_alpha_shared__probe")
        );
        assert!(
            serde_json::to_string(&request.messages)
                .unwrap()
                .contains("plugin_alpha_shared skipped: same command or URL as mine")
        );
    }
    store.set_enabled("alpha", false).unwrap();
    store.uninstall("beta", true).unwrap();
    scripted.enqueue(text("disabled"));
    complete(
        &agent,
        chat,
        "Report tools after disabling and uninstalling",
    )
    .await;
    {
        let requests = scripted.requests.lock().unwrap();
        let tools = &requests.last().unwrap().tools;
        assert!(!tools.iter().any(|t| t.name.starts_with("mcp__plugin_")));
        assert!(tools.iter().any(|t| t.name == "mcp__remote__probe"));
    }
    agent.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the ui5 marketplace plugin in source/ and ANTHROPIC_API_KEY in an isolated TICKLOOP_CONFIG_DIR; uses npx and the billed API"]
async fn live_model_calls_an_installed_marketplace_plugin() {
    let root = std::path::PathBuf::from(
        std::env::var("TICKLOOP_CONFIG_DIR").expect("isolated configuration required"),
    )
    .canonicalize()
    .unwrap();
    assert!(root.to_string_lossy().contains(".scratch/plugin-mcp/"));
    let store = tick_plugins::PluginStore::in_config_root(&root);
    let installed = store.install_path(&root.join("source")).unwrap();
    assert_eq!(installed.plugin.name(), "ui5");
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    let mut launch = LaunchSpec::embedded(root.clone());
    launch.config_root = Some(root.clone());
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
        Arc::new(LocalModelClients::new(Some(root.clone()))),
        None,
        &std::env::current_exe().unwrap(),
    )
    .await
    .unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    complete(
        &agent,
        chat,
        "Reply ready while the installed MCP plugin connects. Do not call any tools yet.",
    )
    .await;
    complete(&agent, chat, "Call mcp__plugin_ui5_ui5-mcp-server__get_guidelines exactly once with an empty object. Then summarize two UI5 guidelines from its result. Use no other tools.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;
    let operations = snapshot
        .operations
        .iter()
        .filter(|operation| operation.name == "mcp__plugin_ui5_ui5-mcp-server__get_guidelines")
        .collect::<Vec<_>>();
    assert_eq!(
        operations.len(),
        1,
        "expected one marketplace MCP invocation"
    );
    let result = operations[0].result.as_ref().expect("tool finished");
    assert!(!result.is_error, "marketplace tool should succeed");
    assert!(result.content.contains("UI5"));
    std::fs::write(root.parent().unwrap().join("live-model-result.json"), serde_json::to_vec_pretty(&json!({"model":"claude-sonnet-5", "tool":operations[0].name, "is_error":result.is_error, "result":result.content})).unwrap()).unwrap();
    eprintln!("Live plugin acceptance passed: ui5 get_guidelines=1, model=claude-sonnet-5");
}

#[tokio::test]
async fn worker_picks_up_a_login_saved_after_startup() {
    use tickloop_code_settings::mcp_credentials::FileCredentials;
    use tickloop_harness::connector::{
        ClientRegistration, ConnectorCredential, ConnectorCredentials, OAuthToken,
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let server = McpService::start().await;
    server.require_bearer("worker-secret-access");
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    server.set_result(
        json!({"content":[{"type":"text","text":"worker-secret-access worker-secret-refresh"}]}),
    );
    std::fs::write(
        root.join("settings.json"),
        json!({"mcpServers":{"remote":{"url":server.url()}}}).to_string(),
    )
    .unwrap();
    tickloop_harness::base::credentials::save_api_key(&root, "ANTHROPIC_API_KEY", "fixture")
        .unwrap();
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    let scripted = Arc::new(ScriptedModel::new(Vec::new()));
    let mut launch = LaunchSpec::embedded(root.clone());
    launch.config_root = Some(root.clone());
    launch.model = Some(model);
    launch.session_persistence = false;
    let (agent, _) = tickloop_code_daemon::build_runtime_with_models(
        &launch,
        ChosenModels::new(scripted.clone()),
        None,
        &std::env::current_exe().unwrap(),
    )
    .await
    .unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            scripted.enqueue(text("status"));
            complete(&agent, chat, "Report the service status").await;
            if format!("{:?}", scripted.requests.lock().unwrap()).contains("NeedsLogin") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Worker did not report that login is needed");
    let credentials = FileCredentials::new(&root, "global", "remote").unwrap();
    let held = credentials.lock().await.unwrap();
    credentials
        .save(&ConnectorCredential {
            server_url: server.url(),
            resource: None,
            registration: ClientRegistration {
                authorization_server: server.url(),
                client_id: "client".into(),
                redirect_uri: "http://localhost:1234/callback".into(),
            },
            token: Some(OAuthToken {
                access_token: "worker-secret-access".into(),
                refresh_token: Some("worker-secret-refresh".into()),
                token_type: "Bearer".into(),
                scope: None,
                issuer: server.url(),
                client_id: "client".into(),
                token_endpoint: server.url(),
                revocation_endpoint: None,
                expires_at: u64::MAX,
            }),
        })
        .await
        .unwrap();
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            scripted.enqueue(text("ready"));
            complete(&agent, chat, "Report readiness after login").await;
            if scripted
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .tools
                .iter()
                .any(|tool| tool.name == "mcp__remote__probe")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Worker did not observe the saved login");
    scripted.enqueue(call("probe", "mcp__remote__probe", json!({})));
    scripted.enqueue(text("done"));
    complete(&agent, chat, "Call the remote probe").await;
    assert_eq!(server.calls().len(), 1);
    let visible = format!(
        "{:?} {:?}",
        scripted.requests.lock().unwrap(),
        agent.snapshot(chat).await.unwrap()
    );
    assert!(!visible.contains("worker-secret-access"));
    assert!(!visible.contains("worker-secret-refresh"));
    agent.shutdown().await;
}
