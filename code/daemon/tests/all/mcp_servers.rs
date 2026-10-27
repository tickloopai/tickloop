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
        .set_execution_mode(chat, ExecutionMode::Auto)
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
        .set_execution_mode(chat, ExecutionMode::Auto)
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
    assert!(
        root.to_string_lossy()
            .contains(".scratch/plugin-mcp/evidence/07-codex/")
    );
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
#[ignore = "requires an isolated TICKLOOP_CONFIG_DIR with MOONSHOT_CN_API_KEY; calls the billed API"]
async fn live_codemode_composes_tools_and_resumes_after_confirmation() {
    let root = std::path::PathBuf::from(
        std::env::var("TICKLOOP_CONFIG_DIR").expect("isolated configuration required"),
    );
    assert!(
        root.to_string_lossy()
            .contains(".scratch/plugin-mcp/evidence/09-codex/")
    );
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
        entry["exposure"] = json!("codemode");
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat,"Use Codemode to call tools.mcp__local__probe({}) and tools.mcp__remote__probe({}) exactly once each inside one JavaScript script. Both return MCP CallToolResult objects. Print their text content with text(). Set the script first line to // @options: {\"timeout_ms\":1000}. Each call will be confirmed by the test operator; wait for confirmation. Do not use Bash, search, or any other tools. Finally report the two acceptance markers.".into()).await.unwrap();
    let mut confirmations = 0;
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } => {
                    assert_eq!(request.parent_call.as_ref().unwrap().name, "Codemode");
                    if confirmations == 0 {
                        tokio::time::sleep(Duration::from_millis(1200)).await;
                        assert!(
                            agent
                                .snapshot(chat)
                                .await
                                .unwrap()
                                .permissions
                                .iter()
                                .any(|permission| permission.id == request.id)
                        );
                    }
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                    confirmations += 1;
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("live Codemode run timed out");
    let snapshot = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;
    let scripts = snapshot
        .operations
        .iter()
        .filter(|operation| operation.name == "Codemode")
        .collect::<Vec<_>>();
    assert_eq!(scripts.len(), 1);
    assert!(
        !scripts[0].result.as_ref().unwrap().is_error,
        "{:?}",
        scripts[0].result
    );
    assert_eq!(confirmations, 2);
    assert_eq!(local.calls().len(), 1);
    assert_eq!(remote.calls().len(), 1);
    let result = &scripts[0].result.as_ref().unwrap().content;
    assert!(
        result.contains("LOCAL_MCP_OK") && result.contains("HTTP_MCP_OK"),
        "{result}"
    );
    eprintln!(
        "Live Codemode acceptance passed: kimi-k3; one script; stdio=1; HTTP=1; two confirmations; first confirmation paused beyond timeout; LOCAL_MCP_OK; HTTP_MCP_OK"
    );
}
