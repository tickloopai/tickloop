use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tickloop_harness_protocol::mcp::{McpServerConfig, McpTransport};
use tickloop_harness_runtime::{
    host_tools::HostCallResult,
    mcp::{HostCapabilities, InMemoryCredentials, McpConnections, McpServerState},
};
use tickloop_harness_test_support::mcp_service::McpService;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn http_connection_discovers_and_calls_the_original_tool() {
    let server = McpService::start().await;
    server.set_tools(
        json!([{"name":"say.hello","description":"Greeting","inputSchema":{"type":"object"}}]),
    );
    server.set_result(json!({"content":[{"type":"text","text":"hello from MCP"}]}));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "sample".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::new(),
            },
            oauth: Default::default(),
            timeout_ms: 300_000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if connections.snapshot()[0].state == McpServerState::Connected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = connections.snapshot();
    assert_eq!(snapshot[0].tools[0].name, "mcp__sample__say_hello");
    assert_eq!(
        snapshot[0].tools[0].input_schema,
        json!({"type":"object","properties":{}})
    );
    let result = connections
        .call(
            "mcp__sample__say_hello",
            json!({}),
            CancellationToken::new(),
        )
        .await;
    let HostCallResult::Completed(result) = result else {
        panic!("call did not complete")
    };
    assert_eq!(result.content, "hello from MCP");
    assert_eq!(server.calls()[0]["params"]["name"], "say.hello");
    connections.shutdown().await;
}

#[tokio::test]
async fn schema_unions_are_flattened_and_ambiguous_tools_are_withdrawn() {
    let server = McpService::start().await;
    server.set_tools(json!([
        {"name":"valid","inputSchema":{"oneOf":[{"type":"object","properties":{"a":{"type":"string"}},"required":["a"]},{"type":"object","properties":{"b":{"type":"number"}},"required":["b"]}]}},
        {"name":"invalid","inputSchema":{"type":"object","properties":{"x":{"oneOf":[{"type":"string"},{"type":"object"}]}}}}
    ]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "schema".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::new(),
            },
            oauth: Default::default(),
            timeout_ms: 300_000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = connections.snapshot();
    assert_eq!(snapshot[0].tools.len(), 1);
    assert_eq!(
        snapshot[0].tools[0].input_schema,
        json!({"type":"object", "properties":{"a":{"type":"string"},"b":{"type":"number"}}})
    );
    assert!(!snapshot[0].warnings.is_empty());
    connections.shutdown().await;
}

#[tokio::test]
async fn stdio_connection_refreshes_notified_tools_and_shuts_down() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"first","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities {
            local_processes: true,
            scripts: false,
        },
        BTreeMap::new(),
        std::env::vars().collect(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "local".into(),
            source: "global".into(),
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: server.stdio_args(),
                env: BTreeMap::new(),
                cwd: std::env::current_dir().unwrap(),
            },
            oauth: Default::default(),
            timeout_ms: 300_000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.set_tools(json!([{"name":"second","inputSchema":{"type":"object"}}]));
    server.notify_tools_changed();
    assert!(matches!(
        connections
            .call("mcp__local__first", json!({}), CancellationToken::new())
            .await,
        HostCallResult::Completed(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while connections.snapshot()[0]
            .tools
            .first()
            .is_none_or(|tool| tool.name != "mcp__local__second")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        connections
            .call("mcp__local__first", json!({}), CancellationToken::new())
            .await,
        HostCallResult::NotDispatched(_)
    ));
    connections.shutdown().await;
    assert!(connections.snapshot().is_empty());
}

#[tokio::test]
async fn third_party_tools_ask_in_auto_without_trusting_read_only_annotations() {
    use tickloop_harness_protocol::{EventKind, ExecutionMode};
    use tickloop_harness_runtime::{Agent, mcp::McpToolProvider};
    use tickloop_harness_test_support::{ScriptedModel, call, config, text};
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"read","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]));
    server.set_result(json!({"content":[{"type":"text","text":"approved result"}]}));
    let configs = vec![McpServerConfig {
        name: "review".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        oauth: Default::default(),
        timeout_ms: 300_000,
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
        call("read-1", "mcp__review__read", json!({})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read from the service".into())
        .await
        .unwrap();
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } => break request,
                EventKind::ToolStarted { .. } => panic!("MCP executed without asking"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(server.calls().is_empty());
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
    assert_eq!(server.calls().len(), 1);
    assert!(
        serde_json::to_string(&model.requests.lock().unwrap()[1].messages)
            .unwrap()
            .contains("approved result")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn resources_and_unknown_call_outcomes_remain_bound_to_the_server() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"change","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "resources".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::new(),
            },
            oauth: Default::default(),
            timeout_ms: 1000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        connections
            .read_resource("resources", "fixture://text", CancellationToken::new())
            .await
            .unwrap()["contents"][0]["text"],
        "resource text"
    );
    server.set_delay(Duration::from_secs(3));
    assert!(matches!(
        connections
            .call(
                "mcp__resources__change",
                json!({}),
                CancellationToken::new()
            )
            .await,
        HostCallResult::OutcomeUnknown(_)
    ));
    assert_eq!(server.calls().len(), 1);
    connections.shutdown().await;
}

#[tokio::test]
async fn expired_session_is_not_dispatched_and_next_use_reconnects() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"change","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let configs = vec![McpServerConfig {
        name: "expiry".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        oauth: Default::default(),
        timeout_ms: 1000,
    }];
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.set_status(404);
    assert!(matches!(
        connections
            .call("mcp__expiry__change", json!({}), CancellationToken::new())
            .await,
        HostCallResult::NotDispatched(_)
    ));
    server.set_status(200);
    assert!(matches!(
        connections
            .call("mcp__expiry__change", json!({}), CancellationToken::new())
            .await,
        HostCallResult::Completed(_)
    ));
    server.set_status(401);
    let _ = connections
        .call("mcp__expiry__change", json!({}), CancellationToken::new())
        .await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    let count = server.calls().len();
    let _ = connections
        .call("mcp__expiry__change", json!({}), CancellationToken::new())
        .await;
    assert_eq!(server.calls().len(), count);
    connections.shutdown().await;
}

#[tokio::test]
async fn text_embedded_resources_and_images_reach_the_host_result() {
    use base64::Engine;
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"image","inputSchema":{"type":"object"}}]));
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    server.set_result(json!({"content":[{"type":"text","text":"first"},{"type":"resource","resource":{"uri":"fixture://text","text":"second"}},{"type":"image","mimeType":"image/png","data":base64::engine::general_purpose::STANDARD.encode(png.into_inner())}]}));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "mixed".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::new(),
            },
            oauth: Default::default(),
            timeout_ms: 1000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let HostCallResult::CompletedWithImages(result, images) = connections
        .call("mcp__mixed__image", json!({}), CancellationToken::new())
        .await
    else {
        panic!("images were discarded")
    };
    assert_eq!(result.content, "first\nsecond");
    assert_eq!(images.len(), 1);
    assert_eq!((images[0].width, images[0].height), (1, 1));
    connections.shutdown().await;
}

#[tokio::test]
async fn large_results_keep_both_ends_and_save_full_text_in_family_temporary_storage() {
    use tickloop_harness_protocol::{EventKind, ExecutionMode};
    use tickloop_harness_runtime::{Agent, mcp::McpToolProvider};
    use tickloop_harness_test_support::{ScriptedModel, call, config, text};
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"large","inputSchema":{"type":"object"}}]));
    let full = format!("START{}END", "中".repeat(8000));
    server.set_result(json!({"content":[{"type":"text","text":full}]}));
    let configs = vec![McpServerConfig {
        name: "output".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        oauth: Default::default(),
        timeout_ms: 1000,
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
        call("large-1", "mcp__output__large", json!({})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read the large result".into())
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
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot.operations[0].result.as_ref().unwrap();
    assert!(result.content.len() < 20 * 1024);
    assert!(result.content.contains("START") && result.content.contains("END"));
    let path = result
        .output_ref
        .as_ref()
        .expect("large MCP output was not saved");
    assert_eq!(std::fs::read_to_string(path).unwrap(), full);
    assert!(!path.starts_with(directory.path()));
    agent.shutdown().await;
}

#[tokio::test]
async fn normalized_tool_names_are_unique_across_server_boundaries() {
    let first = McpService::start().await;
    let second = McpService::start().await;
    first.set_tools(json!([{"name":"b__c","inputSchema":{"type":"object"}}]));
    second.set_tools(json!([{"name":"c","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![
            McpServerConfig {
                name: "a".into(),
                source: "global".into(),
                transport: McpTransport::Http {
                    url: first.url(),
                    headers: BTreeMap::new(),
                },
                oauth: Default::default(),
                timeout_ms: 1000,
            },
            McpServerConfig {
                name: "a__b".into(),
                source: "global".into(),
                transport: McpTransport::Http {
                    url: second.url(),
                    headers: BTreeMap::new(),
                },
                oauth: Default::default(),
                timeout_ms: 1000,
            },
        ])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections
            .snapshot()
            .iter()
            .any(|s| s.state != McpServerState::Connected)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshots = connections.snapshot();
    assert_ne!(snapshots[0].tools[0].name, snapshots[1].tools[0].name);
    for snapshot in snapshots {
        assert!(matches!(
            connections
                .call(&snapshot.tools[0].name, json!({}), CancellationToken::new())
                .await,
            HostCallResult::Completed(_)
        ));
    }
    assert_eq!(first.calls()[0]["params"]["name"], "b__c");
    assert_eq!(second.calls()[0]["params"]["name"], "c");
    connections.shutdown().await;
}

#[tokio::test]
async fn credentials_are_removed_from_catalogs_and_encoded_result_text() {
    let server = McpService::start().await;
    let secret = "private/token+value";
    server.set_tools(json!([{"name":"echo","description":format!("description {secret}"),"inputSchema":{"type":"object"}}]));
    server.set_result(
        json!({"content":[{"type":"text","text":"private/token+value private%2Ftoken%2Bvalue"}]}),
    );
    let credentials = Arc::new(InMemoryCredentials::default());
    credentials.replace(Some(tickloop_harness_runtime::mcp::ConnectorCredential {
        server_url:server.url(),
        registration:tickloop_harness_connector::ClientRegistration {authorization_server:server.url(),client_id:"fixture".into(),redirect_uri:"http://localhost/callback".into()},
        resource:None,
        token:Some(serde_json::from_value(json!({"access_token":secret,"token_type":"Bearer","issuer":server.url(),"expires_at":u64::MAX,"client_id":"fixture","token_endpoint":server.url()})).unwrap()),
    }));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::from([(
            "secrets".into(),
            credentials as Arc<dyn tickloop_harness_runtime::mcp::ConnectorCredentials>,
        )]),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "secrets".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::new(),
            },
            oauth: Default::default(),
            timeout_ms: 1000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !serde_json::to_string(&connections.snapshot())
            .unwrap()
            .contains(secret)
    );
    let result = connections
        .call("mcp__secrets__echo", json!({}), CancellationToken::new())
        .await
        .into_tool_result();
    assert_eq!(result.content, "[redacted] [redacted]");
    connections.shutdown().await;
}

#[tokio::test]
async fn a_resource_404_cannot_reclassify_a_later_timed_out_mutation() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"change","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "outcome".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::new(),
            },
            oauth: Default::default(),
            timeout_ms: 1000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.set_status(404);
    assert!(
        connections
            .read_resource("outcome", "fixture://missing", CancellationToken::new())
            .await
            .is_err()
    );
    server.set_status(200);
    server.set_delay(Duration::from_secs(3));
    assert!(matches!(
        connections
            .call("mcp__outcome__change", json!({}), CancellationToken::new())
            .await,
        HostCallResult::OutcomeUnknown(_)
    ));
    assert_eq!(server.calls().len(), 1);
    connections.shutdown().await;
}

#[tokio::test]
async fn resource_uri_collisions_do_not_redirect_to_another_provider() {
    use tickloop_harness_protocol::{EventKind, SkillSource};
    use tickloop_harness_runtime::{
        Agent, host_tools::HostToolProvider, mcp::McpToolProvider, mcp_tools::McpHostToolProvider,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, artifact_service::ArtifactService, call, config, text,
    };
    let artifact = ArtifactService::start().await;
    artifact
        .control
        .skills_enabled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let root = tempfile::tempdir().unwrap();
    let artifact = Arc::new(McpHostToolProvider::new(
        artifact.login(root.path()).await,
        None,
    ));
    artifact.refresh(CancellationToken::new()).await.unwrap();
    let SkillSource::Remote { uri, .. } = artifact.remote_skills()[0].source.clone() else {
        panic!("expected remote Skill")
    };
    let server = McpService::start().await;
    let configs = vec![McpServerConfig {
        name: "other".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        oauth: Default::default(),
        timeout_ms: 1000,
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
            "resource",
            "ReadMcpResourceTool",
            json!({"server":"other","uri":uri}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(vec![
            artifact as Arc<dyn HostToolProvider>,
            Arc::new(McpToolProvider::new(
                connections,
                Arc::new(move || configs.clone()),
            )),
        ])
        .unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read the resource from other".into())
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
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot.operations[0]
            .result
            .as_ref()
            .unwrap()
            .content
            .contains("resource text")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn shutdown_kills_a_stdio_server_even_when_its_input_pipe_is_full() {
    let script = r#"
import json, sys, time
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if 'id' not in request: continue
    reply = {'jsonrpc':'2.0','id':request['id']}
    if method == 'server/discover': reply['error'] = {'code':-32601,'message':'legacy'}
    elif method == 'initialize': reply['result'] = {'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'blocked','version':'1'}}
    elif method == 'tools/list': reply['result'] = {'tools':[{'name':'block','inputSchema':{'type':'object','properties':{'data':{'type':'string'}}}}]}
    print(json.dumps(reply),flush=True)
    if method == 'tools/list': time.sleep(600)
"#;
    let connections = McpConnections::new(
        HostCapabilities {
            local_processes: true,
            scripts: false,
        },
        BTreeMap::new(),
        std::env::vars().collect(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "blocked".into(),
            source: "global".into(),
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), script.into()],
                env: BTreeMap::new(),
                cwd: std::env::current_dir().unwrap(),
            },
            oauth: Default::default(),
            timeout_ms: 1000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        connections
            .call(
                "mcp__blocked__block",
                json!({"data":"x".repeat(1024*1024)}),
                CancellationToken::new()
            )
            .await,
        HostCallResult::OutcomeUnknown(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), connections.shutdown())
        .await
        .expect("shutdown was blocked by the full pipe");
}

#[tokio::test]
async fn bearer_scheme_spelling_and_stdio_arguments_do_not_expose_tokens() {
    for stdio in [false, true] {
        let server = McpService::start().await;
        let secret = "private-argument-token";
        server.set_tools(json!([{"name":"echo","inputSchema":{"type":"object"}}]));
        server.set_result(json!({"content":[{"type":"text","text":secret}]}));
        let transport = if stdio {
            let mut args = server.stdio_args();
            args.push("--token".into());
            args.push(secret.into());
            McpTransport::Stdio {
                command: "python3".into(),
                args,
                env: BTreeMap::new(),
                cwd: std::env::current_dir().unwrap(),
            }
        } else {
            McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::from([("Authorization".into(), format!("bEaReR   {secret}"))]),
            }
        };
        let connections = McpConnections::new(
            HostCapabilities {
                local_processes: true,
                scripts: false,
            },
            BTreeMap::new(),
            std::env::vars().collect(),
        );
        connections
            .reconcile(vec![McpServerConfig {
                name: "secret".into(),
                source: "global".into(),
                transport,
                oauth: Default::default(),
                timeout_ms: 1000,
            }])
            .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while connections.snapshot()[0].state != McpServerState::Connected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let result = connections
            .call("mcp__secret__echo", json!({}), CancellationToken::new())
            .await
            .into_tool_result();
        assert_eq!(result.content, "[redacted]");
        connections.shutdown().await;
    }
}

#[tokio::test]
async fn credential_renewal_does_not_block_background_connection_startup() {
    let server = McpService::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let stalled = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let credentials = Arc::new(InMemoryCredentials::default());
    credentials.replace(Some(tickloop_harness_runtime::mcp::ConnectorCredential {
        server_url:server.url(),
        registration:tickloop_harness_connector::ClientRegistration {authorization_server:endpoint.clone(),client_id:"fixture".into(),redirect_uri:"http://localhost/callback".into()},
        resource:None,
        token:Some(serde_json::from_value(json!({"access_token":"old-token","refresh_token":"refresh-token","token_type":"Bearer","issuer":endpoint,"expires_at":0,"client_id":"fixture","token_endpoint":endpoint})).unwrap()),
    }));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::from([(
            "renew".into(),
            credentials as Arc<dyn tickloop_harness_runtime::mcp::ConnectorCredentials>,
        )]),
        BTreeMap::new(),
    );
    let started = tokio::time::timeout(
        Duration::from_millis(500),
        connections.reconcile(vec![McpServerConfig {
            name: "renew".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::new(),
            },
            oauth: Default::default(),
            timeout_ms: 1000,
        }]),
    )
    .await;
    stalled.abort();
    let _ = stalled.await;
    assert!(
        started.is_ok(),
        "renewal blocked startup instead of running in the background"
    );
    tokio::time::timeout(Duration::from_secs(2), connections.shutdown())
        .await
        .unwrap();
}

#[tokio::test]
async fn list_change_withdraws_old_tools_while_the_new_directory_is_loading() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"old","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "revision".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: BTreeMap::new(),
            },
            oauth: Default::default(),
            timeout_ms: 1000,
        }])
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.set_directory_delay(Duration::from_secs(2));
    server.set_tools(json!([]));
    server.notify_tools_changed();
    assert!(matches!(
        connections
            .call("mcp__revision__old", json!({}), CancellationToken::new())
            .await,
        HostCallResult::Completed(_)
    ));
    assert!(matches!(
        connections
            .call("mcp__revision__old", json!({}), CancellationToken::new())
            .await,
        HostCallResult::NotDispatched(_)
    ));
    assert_eq!(server.calls().len(), 1);
    connections.shutdown().await;
}

#[tokio::test]
async fn insufficient_scope_withdraws_tools_and_stops_connection_retries() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    let configs = vec![McpServerConfig {
        name: "remote".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: Default::default(),
        },
        timeout_ms: 1000,
        oauth: Default::default(),
    }];
    let connections = McpConnections::new(
        HostCapabilities::default(),
        Default::default(),
        Default::default(),
    );
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.set_status(403);
    server.set_challenge("Bearer error=\"insufficient_scope\", scope=\"write\"");
    connections
        .call("mcp__remote__probe", json!({}), CancellationToken::new())
        .await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    assert!(connections.snapshot()[0].tools.is_empty());
    let requests = server.request_count();
    connections.reconcile(configs).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    assert_eq!(server.request_count(), requests);
    connections.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_login_renews_rejected_and_expiring_tokens_and_stops_after_invalid_grant() {
    use tick_oauth_test_server::{AuthorizationServer, METADATA_PATH, TOKEN_PATH, TokenReply};
    use tickloop_harness_connector::{Connector, OAuthSettings};
    let server = McpService::start().await;
    let authorization = AuthorizationServer::start();
    let credentials = Arc::new(InMemoryCredentials::default());
    let cancel = CancellationToken::new();
    let metadata = format!("{}{METADATA_PATH}", authorization.issuer());
    let connector = Connector::new(
        &server.url(),
        OAuthSettings {
            authorization_server_metadata_url: Some(metadata.clone()),
            ..Default::default()
        },
        credentials.clone(),
    )
    .unwrap();
    let login = connector.start_login(None, &cancel).await.unwrap();
    login
        .paste()
        .submit(&authorization.approve(login.authorization_url()))
        .unwrap();
    login.finish(&cancel).await.unwrap();
    server.require_bearer("access-2");
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    let configs = vec![McpServerConfig {
        name: "remote".into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: Default::default(),
        },
        timeout_ms: 1000,
        oauth: tickloop_harness_protocol::mcp::McpOAuthConfig {
            authorization_server_metadata_url: Some(metadata),
            ..Default::default()
        },
    }];
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::from([(
            "remote".into(),
            credentials.clone() as Arc<dyn tickloop_harness_connector::ConnectorCredentials>,
        )]),
        Default::default(),
    );
    connections.reconcile(configs.clone()).await;
    settled(&connections).await;
    assert_eq!(
        connections.snapshot()[0].state,
        McpServerState::Connected,
        "a rejected saved token should be renewed during connection"
    );
    assert_eq!(authorization.requests_to(TOKEN_PATH).len(), 2);

    server.require_bearer("access-3");
    let outcome = connections
        .call("mcp__remote__probe", json!({}), cancel.clone())
        .await;
    assert!(matches!(outcome, HostCallResult::OutcomeUnknown(_)));
    assert_eq!(
        server.calls().len(),
        1,
        "the dispatched call must not be replayed"
    );
    connections.reconcile(configs.clone()).await;
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    assert_eq!(authorization.requests_to(TOKEN_PATH).len(), 3);

    let mut saved = credentials.current().unwrap();
    saved.token.as_mut().unwrap().expires_at = 0;
    credentials.replace(Some(saved));
    server.require_bearer("access-4");
    connections.reconcile(configs.clone()).await;
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    assert_eq!(authorization.requests_to(TOKEN_PATH).len(), 4);

    authorization.answer_polls([TokenReply::Error(400, "invalid_grant".into())]);
    let mut saved = credentials.current().unwrap();
    saved.token.as_mut().unwrap().expires_at = 0;
    credentials.replace(Some(saved));
    connections.reconcile(configs.clone()).await;
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    assert!(credentials.current().unwrap().token.is_none());
    let requests = server.request_count();
    connections.reconcile(configs).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    assert_eq!(server.request_count(), requests);
    connections.shutdown().await;
}

async fn settled(connections: &McpConnections) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state == McpServerState::Connecting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
