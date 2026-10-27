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
            "local":{"exposure":"direct","command":"python3","args":local.stdio_args()},
            "remote":{"exposure":"direct","url":remote.url()}
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
            "shared":{"exposure":"direct","url":service.url(), "autoApprove":["*"], "permissions":{"allow":["*"]}}, "duplicate":{"url":remote.url()},
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
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
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
    settings["mcpServers"]["mine"] = json!({"exposure":"direct","url":alpha.url()});
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
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let servers = agent.mcp_servers().await;
            if servers.iter().any(|server| {
                server.name == "plugin_ui5_ui5-mcp-server"
                    && server.state == tickloop_harness::protocol::mcp::McpServerState::Connected
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("installed UI5 MCP server did not connect");
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
    complete(&agent, chat, "Use your native tool search to find mcp__plugin_ui5_ui5-mcp-server__get_guidelines and call it exactly once with an empty object. Then summarize two UI5 guidelines from its result. Do not call any other MCP tool or use shell, files or scripts.").await;
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
async fn plugin_user_config_gates_connections_and_never_reaches_model_or_transcript() {
    use tickloop_code_settings::plugin_config::PluginUserConfig;
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    tickloop_harness::base::credentials::save_api_key(&canonical, "ANTHROPIC_API_KEY", "fixture")
        .unwrap();
    let scripted = Arc::new(ScriptedModel::new(Vec::new()));
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    let (agent, _, _) =
        configured_agent(&canonical, model, ChosenModels::new(scripted.clone())).await;
    let service = McpService::start().await;
    let secret = "sensitive-config-marker";
    service.set_tools(json!([{"name":"probe","description":format!("Returns {secret}"),"inputSchema":{"type":"object","properties":{"note":{"type":"string","description":secret},"pin":{"type":"number","default":839271},"flag":{"type":"boolean","default":false}}}}]));
    service.set_result(json!({"content":[{"type":"text","text":format!("result {secret}")}],"structuredContent":{"value":secret}}));
    let source = canonical.join("source");
    std::fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    let mut args = service.stdio_args();
    args.push("prefix${user_config.key}suffix".into());
    args.push("prefix${user_config.pin}suffix".into());
    args.push("${user_config.flag}".into());
    args[2] = format!(
        "import sys\nsys.stderr.write('sensitive-config-marker\\n')\n{}",
        args[2]
    );
    std::fs::write(source.join(".claude-plugin/plugin.json"), json!({
        "name":"configured", "userConfig":{"key":{"type":"string","required":true,"sensitive":true},"pin":{"type":"number","sensitive":true},"flag":{"type":"boolean","sensitive":true}},
        "mcpServers":{"probe":{"exposure":"direct","command":"python3","args":args}}
    }).to_string()).unwrap();
    let plugin = tick_plugins::PluginStore::in_config_root(&canonical)
        .install_path(&source)
        .unwrap()
        .plugin;
    let chat = agent.create_chat(canonical.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    scripted.enqueue(text("missing configuration"));
    complete(&agent, chat, "Report services").await;
    {
        let requests = scripted.requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert!(
            !request
                .tools
                .iter()
                .any(|tool| tool.name.contains("plugin_configured"))
        );
        assert!(
            serde_json::to_string(&request.messages)
                .unwrap()
                .contains("NeedsConfiguration")
        );
    }
    let mut config = PluginUserConfig::load(&canonical, &plugin.manifest).unwrap();
    config
        .save(
            &canonical,
            &plugin.manifest,
            &serde_json::from_value(json!({"key":secret,"pin":839271,"flag":false})).unwrap(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            scripted.enqueue(text("ready"));
            complete(&agent, chat, "Report services").await;
            if scripted
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .tools
                .iter()
                .any(|tool| tool.name == "mcp__plugin_configured_probe__probe")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("configured plugin should connect on the next request");
    scripted.enqueue(call(
        "configured",
        "mcp__plugin_configured_probe__probe",
        json!({}),
    ));
    scripted.enqueue(text("done"));
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Call the configured tool".into())
        .await
        .unwrap();
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            assert!(!serde_json::to_string(&event).unwrap().contains(secret));
            match event.kind {
                EventKind::PermissionRequested { request } => break request,
                EventKind::ToolStarted { .. } => panic!("configuration bypassed approval"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(!serde_json::to_string(&approval).unwrap().contains(secret));
    agent
        .answer_permission(chat, approval.id, true)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            assert!(!serde_json::to_string(&event).unwrap().contains(secret));
            if matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(service.calls().len(), 1);
    {
        let requests = scripted.requests.lock().unwrap();
        for request in requests.iter() {
            assert!(
                !serde_json::to_string(&request.tools)
                    .unwrap()
                    .contains("839271")
            );
            if let Some(tool) = request
                .tools
                .iter()
                .find(|tool| tool.name == "mcp__plugin_configured_probe__probe")
            {
                assert!(
                    tool.input_schema["properties"]["flag"]
                        .get("default")
                        .is_none()
                );
            }
            assert!(
                !serde_json::to_string(&request.messages)
                    .unwrap()
                    .contains(secret)
            );
            assert!(
                !serde_json::to_string(&request.tools)
                    .unwrap()
                    .contains(secret)
            );
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(secret));
    agent.shutdown().await;
    let logs = canonical
        .join("logs/mcp")
        .join(std::process::id().to_string());
    if let Ok(files) = std::fs::read_dir(logs) {
        for file in files {
            assert!(
                !std::fs::read_to_string(file.unwrap().path())
                    .unwrap()
                    .contains(secret)
            );
        }
    }
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
        json!({"mcpServers":{"remote":{"exposure":"direct","url":server.url()}}}).to_string(),
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
    assert!(format!("{:?}", scripted.requests.lock().unwrap()).contains("tickloop mcp login"));
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
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    scripted.enqueue(call("probe", "mcp__remote__probe", json!({})));
    scripted.enqueue(text("done"));
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Call the remote probe".into())
        .await
        .unwrap();
    let mut approved = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } => {
                    let evidence = format!("{request:?}");
                    assert!(!evidence.contains("worker-secret-access"));
                    assert!(!evidence.contains("worker-secret-refresh"));
                    approved = true;
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
    })
    .await
    .unwrap();
    assert!(approved);
    assert_eq!(server.calls().len(), 1);
    let snapshot = agent.snapshot(chat).await.unwrap();
    let visible = format!("{:?} {snapshot:?}", scripted.requests.lock().unwrap());
    assert!(!visible.contains("worker-secret-access"));
    assert!(!visible.contains("worker-secret-refresh"));
    agent.shutdown().await;
}

#[tokio::test]
async fn before_an_agent_starts_agent_access_connects_only_the_named_server_and_lists_its_tools() {
    use tickloop_code_access::{AgentAccess, ObservationRequest, ObservationValue};
    use tickloop_harness::protocol::mcp::{McpServerState, McpServerStatus};
    let agent_directory = tempfile::tempdir().unwrap();
    let agent_root = agent_directory.path().canonicalize().unwrap();
    let mut agent_launch = LaunchSpec::embedded(agent_root.clone());
    agent_launch.config_root = Some(agent_root);
    agent_launch.model = Some(tickloop_harness::test_support::config());
    agent_launch.session_persistence = false;
    let (agent, _) = tickloop_code_daemon::build_runtime_with_models(
        &agent_launch,
        ChosenModels::new(Arc::new(ScriptedModel::new(Vec::new()))),
        None,
        &std::env::current_exe().unwrap(),
    )
    .await
    .unwrap();
    let access = tickloop_code_daemon::EmbeddedAgentAccess::new(agent.clone());

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let remote = McpService::start().await;
    let local = McpService::start().await;
    for service in [&remote, &local] {
        service.set_tools(
            json!([{"name":"probe","description":"Probe","inputSchema":{"type":"object"}}]),
        );
    }
    std::fs::write(
        root.join("settings.json"),
        json!({"mcpServers":{
            "remote":{"url":remote.url()},
            "local":{"command":"python3","args":local.stdio_args()},
            "broken":{"command":"tickloop-missing-mcp-program"}
        }})
        .to_string(),
    )
    .unwrap();
    let mut launch = LaunchSpec::embedded(root.clone());
    launch.config_root = Some(root.clone());
    launch.environment = std::env::vars()
        .map(|(name, value)| tickloop_code_access::EnvironmentValue {
            name: name.into_bytes(),
            value: value.into_bytes(),
        })
        .collect();
    let connect = |name: &str| {
        access.observe(ObservationRequest::DraftMcpServer {
            launch: Box::new(launch.clone()),
            name: name.into(),
        })
    };
    let status = |observation: tickloop_code_access::Observation| -> McpServerStatus {
        match observation.initial {
            ObservationValue::McpServers(mut statuses) if statuses.len() == 1 => statuses.remove(0),
            other => panic!("one server answers: {other:?}"),
        }
    };

    let connected = status(connect("remote").await.unwrap());
    assert_eq!(connected.state, McpServerState::Connected);
    assert_eq!(
        connected
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["mcp__remote__probe"]
    );
    assert_eq!(local.request_count(), 0, "only the named server connects");

    let connected = status(connect("local").await.unwrap());
    assert_eq!(
        (connected.state, connected.tools.len()),
        (McpServerState::Connected, 1)
    );
    let failed = status(connect("broken").await.unwrap());
    assert_eq!(failed.state, McpServerState::Failed);
    assert!(failed.failure.is_some(), "{failed:?}");
    assert!(connect("absent").await.is_err());
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_access_reports_servers_takes_up_disabling_and_reconnects_one() {
    use tickloop_code_access::{
        AgentAccess, AgentCommand, AgentDetailQuery, CommandEnvelope, CommandResult,
        ObservationRequest, ObservationValue,
    };
    use tickloop_harness::protocol::mcp::{McpServerState, McpServerStatus};
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let model = tickloop_harness::test_support::config();
    let (agent, _local, _remote) = configured_agent(
        &root,
        model,
        ChosenModels::new(Arc::new(ScriptedModel::new(Vec::new()))),
    )
    .await;
    let access = tickloop_code_daemon::EmbeddedAgentAccess::new(agent.clone());
    let mut launch = LaunchSpec::embedded(root.clone());
    launch.session_persistence = false;
    let created = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap();
    let Some(CommandResult::Created { execution, .. }) = created.result else {
        panic!("Create returns the execution: {created:?}");
    };
    let statuses = || async {
        match access
            .observe(ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::McpServers,
                after: None,
            })
            .await
            .unwrap()
            .initial
        {
            ObservationValue::McpServers(statuses) => statuses,
            other => panic!("an McpServers query answers with servers: {other:?}"),
        }
    };
    let state = |statuses: &[McpServerStatus], name: &str| {
        statuses
            .iter()
            .find(|status| status.name == name)
            .map(|status| status.state.clone())
    };
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("settings.json")).unwrap()).unwrap();
    settings["projects"][root.to_string_lossy().as_ref()]["disabledMcpServers"] = json!(["remote"]);
    std::fs::write(
        root.join("settings.json"),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let listed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let listed = statuses().await;
            if state(&listed, "local") == Some(McpServerState::Connected) {
                break listed;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stdio server connects");
    assert_eq!(state(&listed, "remote"), Some(McpServerState::Disabled));
    let local = listed.iter().find(|status| status.name == "local").unwrap();
    assert_eq!(
        (local.transport.as_str(), local.tools[0].name.as_str()),
        ("stdio", "mcp__local__probe")
    );
    let disabled = listed
        .iter()
        .find(|status| status.name == "remote")
        .unwrap();
    assert!(disabled.tools.is_empty());
    let reconnect = |name: &str| AgentCommand::ReconnectMcpServer {
        execution,
        name: name.into(),
    };
    let receipt = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: reconnect("local"),
        })
        .await
        .unwrap();
    assert!(
        matches!(receipt.result, Some(CommandResult::Done)),
        "{receipt:?}"
    );
    assert_ne!(state(&statuses().await, "local"), None);
    let refused = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: reconnect("absent"),
        })
        .await
        .unwrap();
    assert!(refused.error.is_some(), "{refused:?}");
    agent.shutdown().await;
}

#[tokio::test]
#[ignore = "requires an isolated TICKLOOP_CONFIG_DIR with MOONSHOT_CN_API_KEY; calls the billed API"]
async fn live_fold_search_describe_execute_with_kimi() {
    let root = std::path::PathBuf::from(
        std::env::var("TICKLOOP_CONFIG_DIR").expect("isolated configuration required"),
    );
    assert!(root.to_string_lossy().contains(".scratch/plugin-mcp/"));
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("kimi-cn-api", "kimi-k3")
        .unwrap();
    let (agent, local, remote) = configured_agent(
        &root,
        model,
        Arc::new(LocalModelClients::new(Some(root.clone()))),
    )
    .await;
    let path = root.join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for entry in settings["mcpServers"].as_object_mut().unwrap().values_mut() {
        entry.as_object_mut().unwrap().remove("exposure");
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if agent.mcp_servers().await.iter().all(|server| {
                server.state == tickloop_harness::protocol::mcp::McpServerState::Connected
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    complete(&agent,chat,"Use SearchTools to find the configured local and remote acceptance marker tools, then DescribeTools to inspect their parameters, and ExecuteTools to call both once. Reply with both returned markers. Use only these three Fold tools; no shell, scripts, web or other tools.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;
    for name in ["SearchTools", "DescribeTools", "ExecuteTools"] {
        assert!(
            snapshot
                .operations
                .iter()
                .any(|operation| operation.name == name
                    && operation
                        .result
                        .as_ref()
                        .is_some_and(|result| !result.is_error)),
            "missing successful {name}"
        );
    }
    assert_eq!(local.calls().len(), 1);
    assert_eq!(remote.calls().len(), 1);
    let messages = serde_json::to_string(&snapshot.messages).unwrap();
    assert!(messages.contains("LOCAL_MCP_OK") && messages.contains("HTTP_MCP_OK"));
    eprintln!(
        "Live Fold acceptance passed: model=kimi-k3; SearchTools, DescribeTools, ExecuteTools; stdio=1; HTTP=1; LOCAL_MCP_OK; HTTP_MCP_OK"
    );
}

#[tokio::test]
#[ignore = "requires an isolated TICKLOOP_CONFIG_DIR with ANTHROPIC_API_KEY and OPENAI_API_KEY; calls the billed APIs"]
async fn live_native_tool_search_with_anthropic_and_openai_across_switches() {
    let root = std::path::PathBuf::from(
        std::env::var("TICKLOOP_CONFIG_DIR").expect("isolated configuration required"),
    );
    let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
    let anthropic = catalog.get("anthropic-api", "claude-sonnet-5").unwrap();
    let openai = catalog.get("openai-api", "gpt-5.6-terra").unwrap();
    let (agent, local, remote) = configured_agent(
        &root,
        anthropic.clone(),
        Arc::new(LocalModelClients::new(Some(root.clone()))),
    )
    .await;
    let path = root.join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for entry in settings["mcpServers"].as_object_mut().unwrap().values_mut() {
        entry.as_object_mut().unwrap().remove("exposure");
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if agent.mcp_servers().await.iter().all(|server| {
                server.state == tickloop_harness::protocol::mcp::McpServerState::Connected
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let first = "Use your tool search to find the acceptance marker probe tools of the local and the remote MCP servers, call each probe exactly once, and reply with both returned markers. Use no shell, scripts, web or other tools.";
    let again = "Use your tool search to find the acceptance marker probe tools of the local and the remote MCP servers, call each probe exactly once, and reply with both returned markers. Use no shell, scripts, web or other tools. The tool set may have changed since the previous turn: use the tool definitions supplied for this request and make actual tool calls. Text describing a call does not execute it. Markers can change between checks, so call both probes again now even though you called them before.";
    let mut anthropic_without_native = anthropic.clone();
    anthropic_without_native
        .card
        .as_mut()
        .unwrap()
        .native_tool_search = Some(None);
    let fold_operations = |snapshot: &tickloop_harness::protocol::ChatSnapshot| {
        snapshot
            .operations
            .iter()
            .filter(|operation| {
                ["SearchTools", "DescribeTools", "ExecuteTools"].contains(&operation.name.as_str())
            })
            .count()
    };
    // Fold first, then native across vendors; returning to Claude may reuse the tools it
    // discovered earlier without searching again.
    let turns = [
        (&anthropic_without_native, first, None, 1),
        (&anthropic, again, Some("server_tool_use"), 2),
        (&openai, again, Some("tool_search_call"), 3),
        (&anthropic, again, None, 4),
    ];
    let mut folded_before = 0;
    for (turn, (model, prompt, record, calls)) in turns.into_iter().enumerate() {
        agent.set_model(chat, model.clone()).await.unwrap();
        complete(&agent, chat, prompt).await;
        assert_eq!(local.calls().len(), calls, "turn {turn} local calls");
        assert_eq!(remote.calls().len(), calls, "turn {turn} remote calls");
        let snapshot = agent.snapshot(chat).await.unwrap();
        let folded = fold_operations(&snapshot);
        assert_eq!(
            folded > folded_before,
            turn == 0,
            "turn {turn} used Fold: {}",
            folded > folded_before
        );
        folded_before = folded;
        let latest = snapshot
            .messages
            .iter()
            .rev()
            .take_while(|message| {
                !serde_json::to_string(message)
                    .unwrap()
                    .contains("Use your tool search")
            })
            .map(|message| serde_json::to_string(message).unwrap())
            .collect::<String>();
        if let Some(record) = record {
            assert!(latest.contains(record), "{} left no {record}", model.model);
        }
        assert!(latest.contains("LOCAL_MCP_OK") && latest.contains("HTTP_MCP_OK"));
        eprintln!(
            "Live tool search turn {turn} passed: model={}; Fold={}; search records in turn: server_tool_use={} tool_search_call={}; stdio={calls}; HTTP={calls}",
            model.model,
            turn == 0,
            latest.contains("server_tool_use"),
            latest.contains("tool_search_call")
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the user's already logged-in notion plugin and ANTHROPIC_API_KEY in an isolated TICKLOOP_CONFIG_DIR; calls the billed API and one read-only search"]
async fn live_model_calls_logged_in_notion_search_without_recording_content() {
    let root = std::path::PathBuf::from(
        std::env::var("TICKLOOP_CONFIG_DIR").expect("isolated configuration required"),
    )
    .canonicalize()
    .unwrap();
    assert!(root.ends_with("notion-login/config"));
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
    let tool = tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            if let Some(server) = agent.mcp_servers().await.into_iter().find(|server| {
                server.name == "plugin_notion_notion"
                    && server.state == tickloop_harness::protocol::mcp::McpServerState::Connected
            }) {
                return server
                    .tools
                    .into_iter()
                    .find(|tool| tool.name.ends_with("__notion-search"))
                    .expect("Notion did not offer its read-only search")
                    .name;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("already logged-in Notion MCP server did not connect");
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, format!("Use your native tool search to find {tool}. Call it exactly once with query tickloop-mcp-acceptance to verify the existing connection. This is a read-only search; do not create or change anything, fetch page bodies, or call other tools. Do not repeat the call. Finally report only whether the tool succeeded, without quoting any returned content.")).await.unwrap();
    let mut confirmations = 0;
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } => {
                    let allowed = request.name == tool
                        && request.mcp_server.as_deref() == Some("plugin_notion_notion")
                        && confirmations == 0;
                    agent
                        .answer_permission(chat, request.id, allowed)
                        .await
                        .unwrap();
                    assert!(allowed, "only one Notion read-only search is authorized");
                    confirmations += 1;
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(_) => panic!("Notion model run failed; response content omitted"),
                _ => {}
            }
        }
    })
    .await
    .expect("Notion read-only acceptance timed out");
    let snapshot = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;
    let calls = snapshot
        .operations
        .iter()
        .filter(|operation| operation.name == tool)
        .collect::<Vec<_>>();
    assert_eq!(confirmations, 1);
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]
            .result
            .as_ref()
            .is_some_and(|result| !result.is_error),
        "Notion search did not succeed; response content omitted"
    );
    eprintln!(
        "Live Notion acceptance passed: model=claude-sonnet-5; path=native; tool={tool}; calls=1; success=true; response content not recorded"
    );
}

#[tokio::test]
#[ignore = "requires a private temporary TICKLOOP_CONFIG_DIR with Codex subscription credentials; consumes subscription usage"]
async fn live_codex_native_search_calls_mcp_fixture_once() {
    let root = std::path::PathBuf::from(
        std::env::var("TICKLOOP_CONFIG_DIR").expect("isolated configuration required"),
    )
    .canonicalize()
    .unwrap();
    assert!(
        root.ends_with("codex-native-config"),
        "use a temporary codex-native-config directory"
    );
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("openai-codex", "gpt-6.1-sol")
        .unwrap();
    let (agent, local, remote) = configured_agent(
        &root,
        model,
        Arc::new(LocalModelClients::new(Some(root.clone()))),
    )
    .await;
    let path = root.join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for entry in settings["mcpServers"].as_object_mut().unwrap().values_mut() {
        entry["exposure"] = json!("search");
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if agent.mcp_servers().await.iter().all(|server| {
                server.state == tickloop_harness::protocol::mcp::McpServerState::Connected
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    complete(&agent, chat, "Use native tool search to find the local MCP server's acceptance marker probe, call it exactly once, and reply with the returned marker. Do not call the remote server or any other tools.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;
    assert_eq!(local.calls().len(), 1);
    assert!(remote.calls().is_empty());
    assert!(snapshot.operations.iter().all(|operation| {
        !["SearchTools", "DescribeTools", "ExecuteTools"].contains(&operation.name.as_str())
    }));
    let messages = serde_json::to_string(&snapshot.messages).unwrap();
    assert!(messages.contains("tool_search_call") && messages.contains("tool_search_output"));
    assert!(messages.contains("LOCAL_MCP_OK"));
    eprintln!(
        "Codex native MCP acceptance passed: gpt-6.1-sol, hosted search, stdio calls=1, Fold calls=0, result=LOCAL_MCP_OK"
    );
}

#[tokio::test]
async fn unchanged_worker_requests_reuse_configuration_and_only_changed_settings_rebuild() {
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
    let (agent, _, remote) =
        configured_agent(root.path(), model, ChosenModels::new(scripted.clone())).await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    scripted.enqueue(text("ready"));
    complete(&agent, chat, "ready").await;
    let before = agent.mcp_performance_counts();
    for _ in 0..2 {
        scripted.enqueue(text("unchanged"));
        complete(&agent, chat, "unchanged").await;
    }
    scripted.enqueue(call("probe", "mcp__remote__probe", json!({})));
    scripted.enqueue(text("done"));
    complete(&agent, chat, "call probe").await;
    assert_eq!(remote.calls().len(), 1);
    assert_eq!(agent.mcp_performance_counts(), before);
    let path = root.path().join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    settings["mcpServers"]["remote"]["description"] = json!("A changed service description");
    std::fs::write(path, serde_json::to_vec(&settings).unwrap()).unwrap();
    scripted.enqueue(text("changed"));
    complete(&agent, chat, "changed").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while agent.mcp_servers().await.iter().any(|server| {
            server.state == tickloop_harness::protocol::mcp::McpServerState::Connecting
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        agent.mcp_performance_counts(),
        (before.0 + 1, before.1, before.2 + 1)
    );
    agent.shutdown().await;
}
