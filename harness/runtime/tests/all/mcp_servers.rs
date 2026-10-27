use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tickloop_harness_protocol::mcp::{McpServerConfig, McpTransport};
use tickloop_harness_runtime::{
    host_tools::HostCallResult,
    mcp::{HostCapabilities, InMemoryCredentials, McpConnections, McpServerState},
};
use tickloop_harness_test_support::mcp_service::McpService;
use tokio_util::sync::CancellationToken;

fn discover_result(versions: &[&str]) -> serde_json::Value {
    json!({"resultType":"complete","supportedVersions":versions,"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fixture","version":"1"}}})
}

fn use_stdio(config: &mut McpServerConfig, server: &McpService) {
    config.transport = McpTransport::Stdio {
        command: "python3".into(),
        args: server.stdio_args(),
        env: BTreeMap::new(),
        cwd: std::env::temp_dir(),
    };
}

#[tokio::test]
async fn discover_classification_accepts_modern_results_and_retries_supported_versions() {
    for stdio in [false, true] {
        for status in [200, 400] {
            if stdio && status != 200 {
                continue;
            }
            let server = McpService::start().await;
            server.set_discover_replies(vec![
                (status, json!({"id":"unrelated","error":{"code":-32022,"message":"use supported version","data":{"supported":["2026-07-28"]}}})),
                (200, json!({"result":discover_result(&["2026-07-28"])})),
            ]);
            server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
            let mut config = http_config("modern", &server);
            config.sensitive_values.push(json!("2026-07-28"));
            if stdio {
                use_stdio(&mut config, &server);
            }
            let connections = McpConnections::new(
                HostCapabilities {
                    local_processes: true,
                },
                BTreeMap::new(),
                std::env::vars().collect(),
            );
            connections.reconcile(vec![config]).await;
            wait_for_state(&connections, "modern", McpServerState::Connected).await;
            assert_eq!(connections.statuses()[0].tools.len(), 1);
            assert_eq!(
                server
                    .methods()
                    .iter()
                    .filter(|method| *method == "server/discover")
                    .count(),
                2
            );
            assert!(!server.methods().iter().any(|method| method == "initialize"));
            connections.shutdown().await;
        }
    }
}

#[tokio::test]
async fn discover_classification_falls_back_for_legacy_responses_over_http_sse_and_stdio() {
    let replies = [
        (
            400,
            json!({"id":null,"error":{"code":-32000,"message":"not initialized"}}),
        ),
        (400, json!({"error":{"code":-32601,"message":"no method"}})),
        (
            500,
            json!({"error":{"code":-32000,"message":"legacy endpoint"}}),
        ),
        (
            200,
            json!({"id":"wrong","error":{"code":-32021,"message":"capability required"}}),
        ),
        (
            200,
            json!({"error":{"code":-32020,"message":"header mismatch"}}),
        ),
        (
            200,
            json!({"error":{"code":-32022,"message":"old versions","data":{"supported":["2025-06-18"]}}}),
        ),
        (
            200,
            json!({"error":{"code":-32022,"message":"no versions","data":{"supported":[]}}}),
        ),
        (
            200,
            json!({"error":{"code":-32022,"message":"invalid versions","data":{"supported":[17]}}}),
        ),
        (200, json!({"result":discover_result(&["2099-01-01"])})),
        (200, json!({"result":{}})),
        (
            200,
            json!({"id":"wrong","result":discover_result(&["2026-07-28"])}),
        ),
    ];
    for (status, reply) in replies {
        for transport in ["http", "sse", "stdio"] {
            // The stdio bridge exits on non-2xx, which is separately covered as a closed probe.
            if transport == "stdio" && status != 200 {
                continue;
            }
            let server = McpService::start().await;
            server.set_discover_replies(vec![(status, reply.clone())]);
            if transport == "sse" {
                server.reject_discover_over_sse();
            }
            server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
            let mut config = http_config("legacy", &server);
            if transport == "stdio" {
                use_stdio(&mut config, &server);
            }
            let connections = McpConnections::new(
                HostCapabilities {
                    local_processes: true,
                },
                BTreeMap::new(),
                std::env::vars().collect(),
            );
            connections.reconcile(vec![config]).await;
            wait_for_state(&connections, "legacy", McpServerState::Connected).await;
            assert_eq!(connections.statuses()[0].tools.len(), 1);
            assert_eq!(
                &server.methods()[..3],
                &["server/discover", "initialize", "notifications/initialized"]
            );
            connections.shutdown().await;
        }
    }
}

#[tokio::test]
async fn unsupported_modern_versions_fail_without_initialize_or_unbounded_correction() {
    for versions in [vec!["2099-01-01"], vec!["2026-07-28"]] {
        let server = McpService::start().await;
        let reply = json!({"id":"wrong","error":{"code":-32022,"message":"unsupported","data":{"supported":versions}}});
        server.set_discover_replies(vec![(400, reply.clone()), (200, reply)]);
        let connections = McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        connections
            .reconcile(vec![http_config("rejected", &server)])
            .await;
        wait_for_state(&connections, "rejected", McpServerState::Failed).await;
        assert!(
            connections.statuses()[0]
                .failure
                .as_deref()
                .unwrap()
                .contains("no compatible MCP protocol version")
        );
        assert!(!server.methods().iter().any(|method| method == "initialize"));
        assert!(server.methods().len() <= 2);
        connections.shutdown().await;
    }
}

#[tokio::test]
async fn malformed_responses_and_empty_sse_streams_are_legacy() {
    for sse in [false, true] {
        for body in ["", "not-json", r#"{"jsonrpc":"2.0","method":"unexpected"}"#] {
            // An HTTP JSON notification is malformed; an SSE notification may precede a reply.
            if sse && body.contains("method") {
                continue;
            }
            let server = McpService::start().await;
            if sse {
                server.reject_discover_over_sse();
            }
            server.set_http_response(
                "server/discover",
                200,
                if sse && !body.is_empty() {
                    format!("data: {body}\n\n")
                } else {
                    body.to_owned()
                },
            );
            let connections = McpConnections::new(
                HostCapabilities::default(),
                BTreeMap::new(),
                BTreeMap::new(),
            );
            connections
                .reconcile(vec![http_config("legacy", &server)])
                .await;
            wait_for_state(&connections, "legacy", McpServerState::Connected).await;
            assert_eq!(&server.methods()[..2], &["server/discover", "initialize"]);
            connections.shutdown().await;
        }
    }
}

#[tokio::test]
async fn http_probe_deadline_closes_the_actual_post_and_sse_stream() {
    for sse in [false, true] {
        let server = McpService::start().await;
        if sse {
            server.stall_discover_stream();
        } else {
            server.delay_discover(Duration::from_secs(60));
        }
        let mut config = http_config("blocked", &server);
        config.timeout_ms = 6000;
        let connections = McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        connections.reconcile(vec![config]).await;
        wait_for_state(&connections, "blocked", McpServerState::Failed).await;
        assert!(
            connections.statuses()[0]
                .failure
                .as_deref()
                .unwrap()
                .contains("probe timed out after 2000ms")
        );
        tokio::time::timeout(Duration::from_secs(2), server.discover_disconnected())
            .await
            .expect("the probe must close its socket before the 6s HTTP timeout");
        assert_eq!(server.methods(), ["server/discover"]);
        connections.shutdown().await;
    }
}

#[tokio::test]
async fn shutdown_cancels_an_inflight_discover_without_waiting_for_its_deadline() {
    let server = McpService::start().await;
    server.stall_discover_stream();
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![http_config("blocked", &server)])
        .await;
    tokio::time::timeout(Duration::from_secs(5), server.discover_started())
        .await
        .unwrap();
    connections.shutdown().await;
    tokio::time::timeout(Duration::from_secs(2), server.discover_disconnected())
        .await
        .expect("shutdown must close the discover stream before its 10s probe deadline");
}

#[tokio::test]
async fn truncated_probe_sse_is_a_network_error_and_never_falls_back() {
    let server = McpService::start().await;
    server.reject_discover_over_sse();
    server.set_http_response("server/discover", 200, "data: {\"jsonrpc\":");
    server.truncate_discover();
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![http_config("broken", &server)])
        .await;
    wait_for_state(&connections, "broken", McpServerState::Failed).await;
    assert_eq!(server.methods(), ["server/discover"]);
    connections.shutdown().await;
}

#[tokio::test]
async fn probe_network_errors_and_timeouts_do_not_fall_back_over_http_but_stdio_close_and_timeout_do()
 {
    for stdio in [false, true] {
        for timeout in [false, true] {
            let server = McpService::start().await;
            if timeout {
                server.delay_discover(Duration::from_secs(60));
            } else {
                server.close_discover();
            }
            let mut config = http_config("probe", &server);
            config.timeout_ms = 6000;
            if stdio {
                use_stdio(&mut config, &server);
            }
            let connections = McpConnections::new(
                HostCapabilities {
                    local_processes: true,
                },
                BTreeMap::new(),
                std::env::vars().collect(),
            );
            connections.reconcile(vec![config]).await;
            wait_for_state(
                &connections,
                "probe",
                if stdio {
                    McpServerState::Connected
                } else {
                    McpServerState::Failed
                },
            )
            .await;
            if stdio {
                assert_eq!(&server.methods()[..2], &["server/discover", "initialize"]);
            } else {
                assert_eq!(server.methods(), ["server/discover"]);
                if timeout {
                    assert!(
                        connections.statuses()[0]
                            .failure
                            .as_deref()
                            .unwrap()
                            .contains("probe timed out after 2000ms")
                    );
                }
            }
            connections.shutdown().await;
        }
    }
}

#[tokio::test]
async fn legacy_verdict_survives_reconnect_and_failed_initialize_but_changes_with_configuration() {
    for stdio in [false, true] {
        let server = McpService::start().await;
        server.set_status(500);
        let mut config = http_config("legacy", &server);
        if stdio {
            use_stdio(&mut config, &server);
        }
        let connections = McpConnections::new(
            HostCapabilities {
                local_processes: true,
            },
            BTreeMap::new(),
            std::env::vars().collect(),
        );
        connections.reconcile(vec![config.clone()]).await;
        wait_for_state(&connections, "legacy", McpServerState::Failed).await;
        server.set_status(200);
        connections.reconnect(vec![config.clone()], "legacy").await;
        wait_for_state(&connections, "legacy", McpServerState::Connected).await;
        connections.reconnect(vec![config.clone()], "legacy").await;
        wait_for_state(&connections, "legacy", McpServerState::Connected).await;
        assert_eq!(
            server
                .methods()
                .iter()
                .filter(|method| *method == "server/discover")
                .count(),
            1
        );
        let mut changed = config.clone();
        changed.description = "new configuration".into();
        connections.reconnect(vec![changed], "legacy").await;
        wait_for_state(&connections, "legacy", McpServerState::Connected).await;
        assert_eq!(
            server
                .methods()
                .iter()
                .filter(|method| *method == "server/discover")
                .count(),
            2
        );
        connections.reconnect(vec![config], "legacy").await;
        wait_for_state(&connections, "legacy", McpServerState::Connected).await;
        assert_eq!(
            server
                .methods()
                .iter()
                .filter(|method| *method == "server/discover")
                .count(),
            3
        );
        connections.shutdown().await;
    }
}

#[tokio::test]
async fn legacy_initialize_versions_keep_negotiated_headers_and_stdio_routing() {
    for stdio in [false, true] {
        for version in ["2025-06-18", "2025-03-26"] {
            let server = McpService::start().await;
            server.set_protocol_version(version);
            let mut config = http_config("legacy", &server);
            config.sensitive_values.push(json!(version));
            if stdio {
                use_stdio(&mut config, &server);
            }
            let connections = McpConnections::new(
                HostCapabilities {
                    local_processes: true,
                },
                BTreeMap::new(),
                std::env::vars().collect(),
            );
            connections.reconcile(vec![config]).await;
            wait_for_state(&connections, "legacy", McpServerState::Connected).await;
            let messages = server.messages();
            assert_eq!(
                messages[1].0["params"]["protocolVersion"],
                rmcp::model::ProtocolVersion::LATEST_WITH_INITIALIZE.as_str()
            );
            if !stdio {
                assert!(
                    messages
                        .iter()
                        .skip(2)
                        .all(|(_, header)| header.as_deref() == Some(version)),
                    "{messages:?}"
                );
            }
            connections.shutdown().await;
        }
    }
}

#[tokio::test]
async fn artifacts_keep_discover_and_the_modern_skills_protocol() {
    use tickloop_harness_runtime::{host_tools::HostToolProvider, mcp_tools::McpHostToolProvider};
    use tickloop_harness_test_support::artifact_service::ArtifactService;
    let server = ArtifactService::start().await;
    server
        .control
        .skills_enabled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let root = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(server.login(root.path()).await, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(!provider.remote_skills().is_empty());
    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|request| request["body"]["method"] == "server/discover")
    );
    assert!(
        requests
            .iter()
            .all(|request| request["body"]["method"] != "initialize")
    );
    for request in requests
        .iter()
        .filter(|request| request["body"]["method"].as_str().is_some())
    {
        assert!(
            request["headers"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("mcp-protocol-version: 2026-07-28")
        );
    }
    provider.shutdown().await;
}

#[tokio::test]
async fn http_legacy_discover_rejections_fall_back_and_accept_older_initialize() {
    for (status, body) in [
        (
            400,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Server not initialized"}}"#,
        ),
        (
            400,
            r#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"Server not initialized"}}"#,
        ),
        (404, ""),
        (405, "Method not allowed"),
    ] {
        let server = McpService::start().await;
        server.set_http_response("server/discover", status, body);
        server.set_protocol_version("2025-06-18");
        server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
        let connections = McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        connections
            .reconcile(vec![http_config("legacy", &server)])
            .await;
        wait_for_state(&connections, "legacy", McpServerState::Connected).await;
        assert_eq!(
            connections.statuses()[0].tools[0].name,
            "mcp__legacy__probe"
        );
        assert_eq!(
            &server.methods()[..3],
            &["server/discover", "initialize", "notifications/initialized"]
        );
        server.set_http_response("tools/call", 400, "not initialized");
        let result = connections
            .call("mcp__legacy__probe", json!({}), CancellationToken::new())
            .await
            .into_tool_result();
        assert!(result.is_error);
        assert!(
            result.content.contains("outcome unknown"),
            "{}",
            result.content
        );
        assert_eq!(
            server
                .methods()
                .iter()
                .filter(|method| *method == "initialize")
                .count(),
            1
        );
        assert_eq!(
            server
                .methods()
                .iter()
                .filter(|method| *method == "tools/call")
                .count(),
            1
        );
        connections.shutdown().await;
    }
}

#[tokio::test]
async fn http_discover_authentication_errors_keep_their_state_and_cause() {
    for (status, scope, expected_state, category) in [
        (401, false, McpServerState::NeedsLogin, "MCP login required"),
        (403, true, McpServerState::NeedsLogin, "MCP scope required"),
        (403, false, McpServerState::Failed, "MCP connection failed"),
    ] {
        let server = McpService::start().await;
        server.set_http_response("server/discover", status, "sensitive-server-error");
        if scope {
            server.set_challenge("Bearer error=\"insufficient_scope\"");
        }
        let connections = McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        connections
            .reconcile(vec![http_config("rejected", &server)])
            .await;
        wait_for_state(&connections, "rejected", expected_state.clone()).await;
        assert_eq!(server.methods(), ["server/discover"]);
        if expected_state == McpServerState::NeedsLogin {
            connections.shutdown().await;
            continue;
        }
        let failure = connections.statuses()[0].failure.clone().unwrap();
        assert!(failure.starts_with(category), "{failure}");
        assert!(failure.contains(&format!("HTTP {status}")), "{failure}");
        assert!(!failure.contains("sensitive-server-error"));
        assert_eq!(server.methods(), ["server/discover"]);
        connections.shutdown().await;
    }
}

#[tokio::test]
async fn http_legacy_fallback_failure_names_both_phases_without_secrets() {
    let server = McpService::start().await;
    server.set_http_response(
        "server/discover",
        400,
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"secret-token"}}"#,
    );
    server.set_http_response("initialize", 405, "secret-token");
    let mut config = http_config("legacy", &server);
    config.sensitive_values.push("secret-token".into());
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.reconcile(vec![config]).await;
    wait_for_state(&connections, "legacy", McpServerState::Failed).await;
    let failure = connections.statuses()[0].failure.clone().unwrap();
    assert!(
        failure.contains("discover") && failure.contains("HTTP 400"),
        "{failure}"
    );
    assert!(
        failure.contains("initialize") && failure.contains("HTTP 405"),
        "{failure}"
    );
    assert!(!failure.contains("secret-token"));
    assert_eq!(server.methods(), ["server/discover", "initialize"]);
    connections.shutdown().await;
}

#[tokio::test]
async fn http_initialize_error_redacts_credentials_before_bounding_the_cause() {
    let server = McpService::start().await;
    let secret = format!("private credential/{}+token", "x".repeat(240));
    let encoded =
        percent_encoding::utf8_percent_encode(&secret, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    server.set_initialize_error(
        json!({"code":-32602,"message":format!("Denied {secret} {encoded} {}", secret.replace(' ', "\t"))}),
    );
    let mut config = http_config("legacy", &server);
    if let McpTransport::Http { headers, .. } = &mut config.transport {
        headers.insert("X-Api-Key".into(), secret.clone());
    }
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.reconcile(vec![config]).await;
    wait_for_state(&connections, "legacy", McpServerState::Failed).await;
    let failure = connections.statuses()[0].failure.clone().unwrap();
    assert!(
        failure.contains("legacy initialize failed: MCP server error -32602"),
        "{failure}"
    );
    assert!(
        failure.contains("Denied [redacted] [redacted] [redacted]"),
        "{failure}"
    );
    assert!(
        !failure.contains("private") && !failure.contains("xxxxx"),
        "{failure}"
    );
    connections.shutdown().await;
}

#[tokio::test]
async fn http_connection_falls_back_to_initialize_when_discover_is_rejected_over_sse() {
    let server = McpService::start().await;
    server.reject_discover_over_sse();
    server.set_tools(json!([{"name":"say.hello","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
            name: "legacy".into(),
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
        while connections.snapshot()[0].state == McpServerState::Connecting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = connections.snapshot();
    connections.shutdown().await;
    assert_eq!(snapshot[0].state, McpServerState::Connected);
    assert_eq!(snapshot[0].tools.len(), 1);
    assert_eq!(snapshot[0].tools[0].name, "mcp__legacy__say_hello");
}

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
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
        },
        BTreeMap::new(),
        std::env::vars().collect(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
async fn third_party_tools_go_to_auto_review_without_trusting_read_only_annotations() {
    use tickloop_harness_protocol::{EventKind, ExecutionMode};
    use tickloop_harness_runtime::{Agent, mcp::McpToolProvider};
    use tickloop_harness_test_support::{ScriptedModel, auto_classification, call, config, text};
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"read","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]));
    server.set_result(json!({"content":[{"type":"text","text":"approved result"}]}));
    let configs = vec![McpServerConfig {
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
    let reviewer = Arc::new(ScriptedModel::new(vec![
        text(
            &auto_classification(true).to_string()
        );
        2
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.set_auto_reviewer(reviewer.clone()).await;
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
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { .. } => {
                    panic!("a rejection returns to the model")
                }
                EventKind::ToolStarted { .. } => panic!("MCP executed without review"),
                EventKind::Completed { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(server.calls().is_empty());
    assert_eq!(reviewer.requests.lock().unwrap().len(), 2);
    assert!(
        serde_json::to_string(&model.requests.lock().unwrap()[1].messages)
            .unwrap()
            .contains("external-code restriction")
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
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
async fn expired_session_is_not_dispatched_and_the_server_reconnects() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"change","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let configs = vec![McpServerConfig {
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
    wait_for_state(&connections, "expiry", McpServerState::Connected).await;
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
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
    second.set_tools(json!([{"name":"b_c","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections
        .reconcile(vec![
            McpServerConfig {
                exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
                tool_exposure: Vec::new(),
                description: String::new(),
                sensitive_values: Vec::new(),
                disabled: false,
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
                exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
                tool_exposure: Vec::new(),
                description: String::new(),
                sensitive_values: Vec::new(),
                disabled: false,
                name: "a_".into(),
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
    assert_eq!(second.calls()[0]["params"]["name"], "b_c");
    connections.shutdown().await;
}

#[tokio::test]
async fn credentials_are_removed_from_catalogs_and_encoded_result_text() {
    let server = McpService::start().await;
    let secret = "private/token+value";
    server.set_instructions(format!("Use {secret}"));
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
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
        },
        BTreeMap::new(),
        std::env::vars().collect(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
            },
            BTreeMap::new(),
            std::env::vars().collect(),
        );
        connections
            .reconcile(vec![McpServerConfig {
                exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
                tool_exposure: Vec::new(),
                description: String::new(),
                sensitive_values: Vec::new(),
                disabled: false,
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
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
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
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
    assert_eq!(
        authorization.requests_to(TOKEN_PATH).len(),
        3,
        "the rejected token was renewed on the open connection"
    );
    connections.reconcile(configs.clone()).await;
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    assert_eq!(authorization.requests_to(TOKEN_PATH).len(), 3);

    let initializations = server.arrivals("initialize").len();
    let mut saved = credentials.current().unwrap();
    saved.token.as_mut().unwrap().expires_at = 0;
    credentials.replace(Some(saved));
    server.require_bearer("access-4");
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while authorization.requests_to(TOKEN_PATH).len() < 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the expiring token is renewed");
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    assert!(matches!(
        connections
            .call("mcp__remote__probe", json!({}), cancel.clone())
            .await,
        HostCallResult::Completed(_)
    ));
    assert_eq!(
        server.arrivals("initialize").len(),
        initializations,
        "the renewed token is used on the open connection"
    );

    authorization.answer_polls([TokenReply::Error(400, "invalid_grant".into())]);
    let mut saved = credentials.current().unwrap();
    saved.token.as_mut().unwrap().expires_at = 0;
    credentials.replace(Some(saved));
    connections.reconcile(configs.clone()).await;
    wait_for_state(&connections, "remote", McpServerState::NeedsLogin).await;
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

#[tokio::test]
async fn resource_authentication_failure_requires_login_before_more_requests() {
    let server = McpService::start().await;
    let configs = vec![McpServerConfig {
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    server.set_status(401);
    assert!(
        connections
            .read_resource("remote", "fixture://text", CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    let requests = server.request_count();
    assert!(
        connections
            .read_resource("remote", "fixture://text", CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        connections
            .list_resources("remote", None, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(server.request_count(), requests);
    connections.reconcile(configs).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    connections.shutdown().await;
}

#[derive(Clone, Default)]
struct ProtocolLog(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for ProtocolLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_secrets_stay_redacted_in_sdk_logs_and_after_invalidation() {
    use tick_oauth_test_server::{AuthorizationServer, METADATA_PATH, TokenReply};
    use tickloop_harness_connector::{Connector, OAuthSettings};
    let log = ProtocolLog::default();
    let writer = log.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();
    let server = McpService::start().await;
    let authorization = AuthorizationServer::start();
    let credentials = Arc::new(InMemoryCredentials::default());
    let cancel = CancellationToken::new();
    let connector = Connector::new(
        &server.url(),
        OAuthSettings {
            authorization_server_metadata_url: Some(format!(
                "{}{METADATA_PATH}",
                authorization.issuer()
            )),
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
    let token = credentials.current().unwrap().token.unwrap();
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    server.set_resources(json!([{"uri":"fixture://text", "name":token.refresh_token}]));
    server.notify_log_with_unicode_escapes(&format!(
        "{} {}",
        token.access_token,
        token.refresh_token.as_ref().unwrap()
    ));
    let configs = vec![McpServerConfig {
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
        BTreeMap::from([(
            "remote".into(),
            credentials as Arc<dyn tickloop_harness_connector::ConnectorCredentials>,
        )]),
        Default::default(),
    );
    connections.reconcile(configs).await;
    settled(&connections).await;
    connections
        .call("mcp__remote__probe", json!({}), cancel.clone())
        .await;
    server.notify_custom(&format!("fixture/{}", token.access_token));
    connections
        .call("mcp__remote__probe", json!({}), cancel.clone())
        .await;
    let logged = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(
        !logged.contains(&token.access_token),
        "SDK logging exposed the access token"
    );
    assert!(
        !logged.contains(token.refresh_token.as_ref().unwrap()),
        "SDK logging exposed the refresh token"
    );
    authorization.answer_polls([TokenReply::Error(400, "invalid_grant".into())]);
    server.set_status(401);
    connections
        .call("mcp__remote__probe", json!({}), cancel)
        .await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    let visible = serde_json::to_string(&connections.snapshot()).unwrap();
    assert!(
        !visible.contains(token.refresh_token.as_ref().unwrap()),
        "invalidation forgot a secret still present in cached resources"
    );
    connections.shutdown().await;
}

#[tokio::test]
async fn renewal_timing_out_at_the_connection_deadline_requires_login() {
    use tickloop_harness_connector::{
        ConnectorCredential, ConnectorCredentials, CredentialError, CredentialLock,
    };
    #[derive(Default)]
    struct StalledCredentials {
        inner: InMemoryCredentials,
        entered: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl ConnectorCredentials for StalledCredentials {
        async fn lock(&self) -> Result<CredentialLock, CredentialError> {
            self.entered.notify_one();
            std::future::pending().await
        }
        async fn load(&self) -> Result<Option<ConnectorCredential>, CredentialError> {
            self.inner.load().await
        }
        async fn save(&self, credential: &ConnectorCredential) -> Result<(), CredentialError> {
            self.inner.save(credential).await
        }
    }
    let server = McpService::start().await;
    let credentials = Arc::new(StalledCredentials::default());
    credentials.inner.replace(Some(ConnectorCredential {
        server_url:server.url(), registration:tickloop_harness_connector::ClientRegistration {authorization_server:server.url(),client_id:"client".into(),redirect_uri:"http://localhost/callback".into()}, resource:None,
        token:Some(serde_json::from_value(json!({"access_token":"expired-access","refresh_token":"expired-refresh","token_type":"Bearer","issuer":server.url(),"expires_at":0,"client_id":"client","token_endpoint":server.url()})).unwrap()),
    }));
    let configs = vec![McpServerConfig {
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
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
        BTreeMap::from([(
            "remote".into(),
            credentials.clone() as Arc<dyn ConnectorCredentials>,
        )]),
        Default::default(),
    );
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), credentials.entered.notified())
        .await
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    connections.reconcile(configs).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::NeedsLogin);
    assert_eq!(server.request_count(), 0);
    connections.shutdown().await;
}

#[tokio::test]
async fn shutdown_releases_host_credentials_and_numeric_credentials_preserve_rpc_ids() {
    let server = McpService::start().await;
    server.require_bearer("1");
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}},{"name":"v1_probe","inputSchema":{"type":"object"}}]));
    let credentials = Arc::new(InMemoryCredentials::default());
    let released = Arc::downgrade(&credentials);
    credentials.replace(Some(tickloop_harness_connector::ConnectorCredential {
        server_url:server.url(), resource:None,
        registration:tickloop_harness_connector::ClientRegistration{authorization_server:server.url(),client_id:"client".into(),redirect_uri:"http://localhost/callback".into()},
        token:Some(serde_json::from_value(json!({"access_token":"1","token_type":"Bearer","issuer":server.url(),"expires_at":u64::MAX,"client_id":"client","token_endpoint":server.url()})).unwrap()),
    }));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::from([(
            "remote".into(),
            credentials as Arc<dyn tickloop_harness_connector::ConnectorCredentials>,
        )]),
        Default::default(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            sensitive_values: Vec::new(),
            disabled: false,
            name: "remote".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: Default::default(),
            },
            timeout_ms: 1000,
            oauth: Default::default(),
        }])
        .await;
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    assert_eq!(
        connections.snapshot()[0].tools.len(),
        1,
        "a sensitive tool name must be omitted instead of rewritten for dispatch"
    );
    assert_eq!(
        connections.snapshot()[0].tools[0].name,
        "mcp__remote__probe"
    );
    assert!(matches!(
        connections
            .call("mcp__remote__probe", json!({}), CancellationToken::new())
            .await,
        HostCallResult::Completed(_)
    ));
    connections.shutdown().await;
    drop(connections);
    assert!(
        released.upgrade().is_none(),
        "a closed connection retained its host credential adapter"
    );
}

#[tokio::test]
async fn http_private_configuration_preserves_protocol_flags_and_original_schema_validation() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object","properties":{"pin":{"type":"number","minimum":839271,"default":839271},"flag":{"type":"boolean","default":false}}}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        Default::default(),
        Default::default(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            name: "remote".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: Default::default(),
            },
            timeout_ms: 1000,
            oauth: Default::default(),
            sensitive_values: vec![json!(true), json!(false), json!(839271)],
            disabled: false,
        }])
        .await;
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    let shown = &connections.snapshot()[0].tools[0].input_schema;
    assert!(shown["properties"]["pin"].get("minimum").is_none());
    assert!(shown["properties"]["flag"].get("default").is_none());
    assert!(matches!(
        connections
            .call(
                "mcp__remote__probe",
                json!({"pin":0}),
                CancellationToken::new()
            )
            .await,
        HostCallResult::NotDispatched(_)
    ));
    assert!(server.calls().is_empty());
    assert!(matches!(
        connections
            .call(
                "mcp__remote__probe",
                json!({"pin":839272}),
                CancellationToken::new()
            )
            .await,
        HostCallResult::Completed(_)
    ));
    assert_eq!(server.calls().len(), 1);
    connections.shutdown().await;
}

#[tokio::test]
async fn a_resumed_directory_keeps_the_original_schema_for_validation() {
    let server = McpService::start().await;
    server.resume_tool_directory_once();
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object","properties":{"pin":{"type":"number","minimum":839271}}}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        Default::default(),
        Default::default(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            name: "remote".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: server.url(),
                headers: Default::default(),
            },
            timeout_ms: 1000,
            oauth: Default::default(),
            sensitive_values: vec![json!(839271)],
            disabled: false,
        }])
        .await;
    settled(&connections).await;
    assert_eq!(server.directory_resumes(), 1);
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    assert_eq!(connections.snapshot()[0].tools.len(), 1);
    assert!(matches!(
        connections
            .call(
                "mcp__remote__probe",
                json!({"pin":0}),
                CancellationToken::new()
            )
            .await,
        HostCallResult::NotDispatched(_)
    ));
    assert!(server.calls().is_empty());
    assert!(matches!(
        connections
            .call(
                "mcp__remote__probe",
                json!({"pin":839272}),
                CancellationToken::new()
            )
            .await,
        HostCallResult::Completed(_)
    ));
    connections.shutdown().await;
}

fn http_config(name: &str, server: &McpService) -> McpServerConfig {
    McpServerConfig {
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
        name: name.into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        oauth: Default::default(),
        timeout_ms: 300_000,
    }
}

#[tokio::test]
async fn service_instructions_and_summary_redact_secrets_through_the_host_provider() {
    use tickloop_harness_runtime::{host_tools::HostToolProvider, mcp::McpToolProvider};

    let server = McpService::start().await;
    server.set_instructions("Use fixture-private-key\nDetails fixture-private-key");
    for stdio in [false, true] {
        for description in ["", "Configured fixture-private-key"] {
            let mut config = http_config("notes", &server);
            config.description = description.into();
            config.transport = if stdio {
                McpTransport::Stdio {
                    command: "python3".into(),
                    args: server.stdio_args(),
                    env: BTreeMap::from([("FIXTURE_KEY".into(), "fixture-private-key".into())]),
                    cwd: std::env::temp_dir(),
                }
            } else {
                McpTransport::Http {
                    url: server.url(),
                    headers: BTreeMap::from([("x-api-key".into(), "fixture-private-key".into())]),
                }
            };
            let connections = McpConnections::new(
                HostCapabilities {
                    local_processes: true,
                },
                BTreeMap::new(),
                BTreeMap::new(),
            );
            let provider =
                McpToolProvider::new(connections, Arc::new(move || vec![config.clone()]));
            provider.refresh(CancellationToken::new()).await.unwrap();
            let messages = provider.mcp_instructions();
            assert_eq!(messages.len(), 1);
            let text = &messages[0].text;
            assert!(text.contains("status: Connected"));
            assert!(!text.contains("fixture-private-key"));
            assert!(text.ends_with("Use [redacted]\nDetails [redacted]"));
            let summary = if description.is_empty() {
                "Use [redacted]"
            } else {
                "Configured [redacted]"
            };
            assert!(text.contains(&format!("summary: {summary}.")));
            provider.shutdown().await;
        }
    }
}

#[tokio::test]
async fn service_instructions_redact_secrets_before_the_length_limit() {
    use tickloop_harness_runtime::{host_tools::HostToolProvider, mcp::McpToolProvider};

    let server = McpService::start().await;
    let prefix = "界".repeat(2030);
    server.set_instructions(format!(
        "{prefix}fixture-private-key-crossing-the-limit{}",
        "x".repeat(100)
    ));
    let mut config = http_config("notes", &server);
    config.transport = McpTransport::Stdio {
        command: "python3".into(),
        args: server.stdio_args(),
        env: BTreeMap::new(),
        cwd: std::env::temp_dir(),
    };
    config.sensitive_values = vec![json!("fixture-private-key-crossing-the-limit")];
    let connections = McpConnections::new(
        HostCapabilities {
            local_processes: true,
        },
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let provider = McpToolProvider::new(connections, Arc::new(move || vec![config.clone()]));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let messages = provider.mcp_instructions();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].text.contains("status: Connected"));
    let instructions = messages[0]
        .text
        .split_once("(untrusted data):\n")
        .unwrap()
        .1;
    assert_eq!(
        instructions.strip_prefix(&prefix),
        Some("[redacted]xxxxxxxx")
    );
    assert_eq!(instructions.chars().count(), 2048);
    provider.shutdown().await;
}

#[tokio::test]
async fn tool_descriptions_redact_secrets_before_the_length_limit() {
    use tickloop_harness_runtime::{
        host_tools::HostToolProvider,
        mcp::McpToolProvider,
        tool_exposure::{ExposureDirectory, SearchSupport},
    };

    let server = McpService::start().await;
    let prefix = "界".repeat(2030);
    server.set_tools(json!([{"name":"probe","description":format!("{prefix}fixture-private-key-crossing-the-limit{}", "x".repeat(100)),"inputSchema":{"type":"object"}}]));
    let mut config = http_config("notes", &server);
    config.transport = McpTransport::Stdio {
        command: "python3".into(),
        args: server.stdio_args(),
        env: BTreeMap::new(),
        cwd: std::env::temp_dir(),
    };
    config.sensitive_values = vec![json!("fixture-private-key-crossing-the-limit")];
    let connections = McpConnections::new(
        HostCapabilities {
            local_processes: true,
        },
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let provider = McpToolProvider::new(connections, Arc::new(move || vec![config.clone()]));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let definitions = provider.definitions();
    assert_eq!(definitions.len(), 1);
    assert_eq!(
        definitions[0].description.strip_prefix(&prefix),
        Some("[redacted]xxxxxxxx")
    );
    assert_eq!(definitions[0].description.chars().count(), 2048);
    let directory = ExposureDirectory::new(provider.discovery_tools(), SearchSupport::Fold);
    let described = directory
        .describe(&json!({"names":["mcp__notes__probe"]}))
        .unwrap();
    assert_eq!(
        described["tools"][0]["description"],
        definitions[0].description
    );
    let statuses = provider.mcp_server_statuses(CancellationToken::new()).await;
    assert_eq!(statuses[0].tools[0].description, definitions[0].description);
    assert!(matches!(
        provider
            .execute(
                tickloop_harness_protocol::ChatId::new(),
                "mcp__notes__probe",
                json!({}),
                CancellationToken::new()
            )
            .await,
        tickloop_harness_protocol::ToolResult {
            is_error: false,
            ..
        }
    ));
    assert_eq!(server.calls().len(), 1);
    provider.shutdown().await;
}

#[tokio::test]
async fn discovery_resources_and_failures_redact_server_text() {
    use tickloop_harness_runtime::{
        host_tools::HostToolProvider,
        mcp::McpToolProvider,
        tool_exposure::{ExposureDirectory, SearchSupport},
    };

    let server = McpService::start().await;
    server.set_tools(json!([{
        "name":"probe", "description":"Find fixture-private-key. More detail.",
        "inputSchema":{"type":"object","properties":{"query":{"type":"string","description":"Query fixture-private-key"}},"required":["query"]},
        "annotations":{"title":"Title fixture-private-key"}
    }]));
    server.set_resources(json!([{"uri":"fixture://text","name":"Resource fixture-private-key"}]));
    server.set_resource_text("Contents fixture-private-key");
    let mut config = http_config("notes", &server);
    config.transport = McpTransport::Stdio {
        command: "python3".into(),
        args: server.stdio_args(),
        env: BTreeMap::new(),
        cwd: std::env::temp_dir(),
    };
    config.sensitive_values = vec![json!("fixture-private-key")];
    config.tool_exposure = vec![(
        "probe".into(),
        tickloop_harness_protocol::mcp::ToolExposure::Search,
    )];
    let mut failed = config.clone();
    failed.name = "broken".into();
    failed.transport = McpTransport::Failed {
        reason: "Failure fixture-private-key".into(),
    };
    let connections = McpConnections::new(
        HostCapabilities {
            local_processes: true,
        },
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let provider = McpToolProvider::new(
        connections,
        Arc::new(move || vec![config.clone(), failed.clone()]),
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    let definitions = provider.definitions();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].description, "Find [redacted]. More detail.");
    assert_eq!(
        definitions[0].input_schema["properties"]["query"]["description"],
        "Query [redacted]"
    );
    let directory = ExposureDirectory::new(provider.discovery_tools(), SearchSupport::Fold);
    let found = directory.search(&json!({"query":"probe"})).unwrap();
    assert_eq!(found["matches"][0]["summary"], "Find [redacted].");
    let described = directory
        .describe(&json!({"names":["mcp__notes__probe"]}))
        .unwrap();
    assert_eq!(
        described["tools"][0]["description"],
        "Find [redacted]. More detail."
    );
    assert_eq!(
        described["tools"][0]["input_schema"]["properties"]["query"]["description"],
        "Query [redacted]"
    );
    assert_eq!(
        described["tools"][0]["annotations"]["title"],
        "Title [redacted]"
    );
    let resources = provider
        .list_mcp_resources_for("notes", None, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resources["resources"][0]["name"], "Resource [redacted]");
    let content = provider
        .read_mcp_resource_for("notes", "fixture://text", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(content["contents"][0]["text"], "Contents [redacted]");
    let statuses = provider.mcp_server_statuses(CancellationToken::new()).await;
    assert_eq!(statuses[0].failure.as_deref(), Some("Failure [redacted]"));
    assert_eq!(
        statuses[1].tools[0].description,
        "Find [redacted]. More detail."
    );
    provider.shutdown().await;
}

async fn wait_for_state(connections: &McpConnections, name: &str, state: McpServerState) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !connections
            .statuses()
            .iter()
            .any(|status| status.name == name && status.state == state)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{name} never became {state:?}: {:?}",
            connections.statuses()
        )
    });
}

#[tokio::test]
async fn statuses_report_each_server_and_a_disabled_server_never_runs() {
    let server = McpService::start().await;
    server.set_tools(
        json!([{"name":"search","description":"Find notes","inputSchema":{"type":"object"}}]),
    );
    let idle = McpService::start().await;
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let mut disabled = http_config("idle", &idle);
    disabled.disabled = true;
    connections
        .reconcile(vec![http_config("notes", &server), disabled])
        .await;
    wait_for_state(&connections, "notes", McpServerState::Connected).await;
    assert_eq!(
        serde_json::to_value(connections.statuses()).unwrap(),
        json!([
            {"name":"idle","source":"global","transport":"http","state":"disabled","exposure":"direct","tools":[],"oauth":true,"logged_in":false},
            {"name":"notes","source":"global","transport":"http","state":"connected","exposure":"direct",
             "tools":[{"name":"mcp__notes__search","description":"Find notes"}],"oauth":true,"logged_in":false}
        ])
    );
    assert_eq!(idle.request_count(), 0);
    connections.shutdown().await;
}

#[tokio::test]
async fn a_settled_status_waits_for_the_connection_and_reports_its_tools_or_its_failure() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"search","inputSchema":{"type":"object"}}]));
    server.delay_discover(Duration::from_millis(200));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let mut refused = http_config("refused", &server);
    refused.transport = McpTransport::Http {
        url: "http://127.0.0.1:1/mcp".into(),
        headers: BTreeMap::new(),
    };
    connections
        .reconcile(vec![http_config("notes", &server), refused])
        .await;
    assert!(connections.settled_status("absent").await.is_none());
    let notes = connections.settled_status("notes").await.unwrap();
    assert_eq!(notes.state, McpServerState::Connected);
    assert_eq!(notes.tools.len(), 1);
    let refused = connections.settled_status("refused").await.unwrap();
    assert_eq!(refused.state, McpServerState::Failed);
    assert!(refused.failure.is_some());
    connections.shutdown().await;
}

#[tokio::test]
async fn reconnecting_a_server_reads_its_tools_again() {
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"first","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let configs = vec![http_config("notes", &server)];
    connections.reconcile(configs.clone()).await;
    wait_for_state(&connections, "notes", McpServerState::Connected).await;
    server.set_tools(json!([{"name":"second","inputSchema":{"type":"object"}}]));
    assert!(!connections.reconnect(configs.clone(), "absent").await);
    assert!(connections.reconnect(configs, "notes").await);
    tokio::time::timeout(Duration::from_secs(10), async {
        while connections.statuses()[0]
            .tools
            .first()
            .map(|tool| tool.name.as_str())
            != Some("mcp__notes__second")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the reconnected server lists its new tools");
    connections.shutdown().await;
}

#[tokio::test]
async fn reading_states_leaves_a_failed_server_failed_with_its_reason() {
    let connections = McpConnections::new(
        HostCapabilities {
            local_processes: true,
        },
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let configs = vec![McpServerConfig {
        exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        sensitive_values: Vec::new(),
        disabled: false,
        name: "broken".into(),
        source: "directory".into(),
        transport: McpTransport::Stdio {
            command: "tickloop-missing-mcp-program".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: std::env::temp_dir(),
        },
        oauth: Default::default(),
        timeout_ms: 300_000,
    }];
    connections.reconcile(configs.clone()).await;
    wait_for_state(&connections, "broken", McpServerState::Failed).await;
    connections.reconcile(configs.clone()).await;
    let status = &connections.statuses()[0];
    assert_eq!(
        (status.state.clone(), status.transport.as_str()),
        (McpServerState::Failed, "stdio")
    );
    assert!(status.failure.is_some());
    assert!(connections.reconnect(configs, "broken").await);
    assert_ne!(connections.statuses()[0].state, McpServerState::Failed);
    connections.shutdown().await;
}

#[tokio::test]
async fn schema_metadata_reaches_mcp_approval_while_invalid_arguments_never_dispatch() {
    use tickloop_harness_protocol::{EventKind, ExecutionMode, mcp::ToolExposure};
    use tickloop_harness_runtime::{Agent, mcp::McpToolProvider};
    use tickloop_harness_test_support::{ScriptedModel, call, config, text};

    for exposure in [ToolExposure::Direct, ToolExposure::Search] {
        let server = McpService::start().await;
        server.set_tools(json!([{
            "name":"search",
            "inputSchema":{
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "title":"Search input",
                "type":"object",
                "properties":{"query":{"type":"string","pattern":"^allowed$","maxLength":7}},
                "required":["query"],
                "additionalProperties":false
            }
        }]));
        server.set_result(json!({"content":[{"type":"text","text":"APPROVED_SEARCH"}]}));
        let mut configuration = http_config("schema", &server);
        configuration.exposure = exposure;
        let connections = McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        connections.reconcile(vec![configuration.clone()]).await;
        wait_for_state(&connections, "schema", McpServerState::Connected).await;
        let invoke = |id, query| {
            let arguments = json!({"query":query});
            if exposure == ToolExposure::Direct {
                call(id, "mcp__schema__search", arguments)
            } else {
                call(
                    id,
                    "ExecuteTools",
                    json!({"calls":[{"name":"mcp__schema__search","arguments":arguments}]}),
                )
            }
        };
        let model = Arc::new(ScriptedModel::new(vec![
            invoke("invalid", "blocked"),
            invoke("valid", "allowed"),
            text("done"),
        ]));
        let agent = Agent::new_embedded(config(), model.clone());
        agent
            .install_host_tools(Arc::new(McpToolProvider::new(
                connections,
                Arc::new(move || vec![configuration.clone()]),
            )))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::AcceptEdits)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Search the service once with valid arguments".into())
            .await
            .unwrap();
        let mut approvals = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.unwrap().kind {
                    EventKind::PermissionRequested { request } => {
                        assert_eq!(request.name, "mcp__schema__search");
                        assert_eq!(request.input, json!({"query":"allowed"}));
                        assert!(server.calls().is_empty());
                        agent
                            .answer_permission(chat, request.id, true)
                            .await
                            .unwrap();
                        approvals += 1;
                    }
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        agent.shutdown().await;
        assert_eq!(
            approvals, 1,
            "{exposure:?}: valid schema never reached approval"
        );
        assert_eq!(server.calls().len(), 1);
        assert_eq!(
            server.calls()[0]["params"]["arguments"],
            json!({"query":"allowed"})
        );
        let requests = model.requests.lock().unwrap();
        let final_messages = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
        assert!(final_messages.contains("APPROVED_SEARCH"));
        assert!(final_messages.contains("invalid"));
    }
}

#[tokio::test]
async fn stdio_secrets_stay_redacted_before_sdk_logging_without_weakening_validation() {
    let log = ProtocolLog::default();
    let writer = log.clone();
    let _subscriber = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish(),
    );
    let secret = "stdio-private-credential";
    let server = McpService::start().await;
    server.set_instructions(format!("INITIALIZE_MARKER {secret}"));
    server.set_tools(json!([{"name":"probe","description":format!("DESCRIPTION_MARKER {secret}"),"inputSchema":{"type":"object","properties":{"pin":{"type":"number","minimum":839271}}}}]));
    server
        .set_result(json!({"content":[{"type":"text","text":format!("RESULT_MARKER {secret}")}]}));
    let connections = McpConnections::new(
        HostCapabilities {
            local_processes: true,
        },
        Default::default(),
        std::env::vars().collect(),
    );
    connections
        .reconcile(vec![McpServerConfig {
            name: "local".into(),
            source: "global".into(),
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: server.stdio_args(),
                env: BTreeMap::from([("FIXTURE_CREDENTIAL".into(), secret.into())]),
                cwd: std::env::current_dir().unwrap(),
            },
            timeout_ms: 5000,
            oauth: Default::default(),
            sensitive_values: vec![json!(secret), json!(839271), json!(true), json!(false)],
            disabled: false,
            exposure: tickloop_harness_protocol::mcp::ToolExposure::Direct,
            tool_exposure: vec![],
            description: String::new(),
        }])
        .await;
    settled(&connections).await;
    assert_eq!(connections.snapshot()[0].state, McpServerState::Connected);
    assert!(
        connections.snapshot()[0].tools[0].input_schema["properties"]["pin"]
            .get("minimum")
            .is_none()
    );
    assert!(matches!(
        connections
            .call(
                "mcp__local__probe",
                json!({"pin":0}),
                CancellationToken::new()
            )
            .await,
        HostCallResult::NotDispatched(_)
    ));
    assert!(server.calls().is_empty());
    let HostCallResult::Completed(result) = connections
        .call(
            "mcp__local__probe",
            json!({"pin":839272}),
            CancellationToken::new(),
        )
        .await
    else {
        panic!("valid call did not complete")
    };
    assert!(!result.content.contains(secret));
    assert_eq!(server.calls().len(), 1);
    connections.shutdown().await;
    let logged = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    for marker in ["INITIALIZE_MARKER", "DESCRIPTION_MARKER", "RESULT_MARKER"] {
        assert!(logged.contains(marker), "TRACE capture missed {marker}");
    }
    assert!(
        !logged.contains(secret),
        "SDK logging exposed the stdio credential"
    );
    assert!(
        !logged.contains("839271"),
        "SDK logging exposed a sensitive schema constraint"
    );
}

#[tokio::test]
async fn the_first_refresh_waits_for_search_servers_and_reports_unavailable_ones() {
    use tickloop_harness_protocol::mcp::ToolExposure;
    use tickloop_harness_runtime::{
        host_tools::HostToolProvider, mcp::McpToolProvider, tool_exposure::UnavailableServer,
    };
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"get_version_info","description":"Get version information for UI5","inputSchema":{"type":"object"}}]));
    server.set_directory_delay(Duration::from_millis(500));
    let mut config = http_config("ui5", &server);
    config.exposure = ToolExposure::Search;
    let mut broken = config.clone();
    broken.name = "broken".into();
    broken.transport = McpTransport::Failed {
        reason: "missing command".into(),
    };
    let mut hidden = broken.clone();
    hidden.name = "hidden".into();
    hidden.exposure = ToolExposure::Hidden;
    let provider = McpToolProvider::new(
        McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        ),
        Arc::new(move || vec![config.clone(), broken.clone(), hidden.clone()]),
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider
            .discovery_tools()
            .iter()
            .map(|tool| tool.definition.name.as_str())
            .collect::<Vec<_>>(),
        ["mcp__ui5__get_version_info"]
    );
    assert_eq!(
        provider.unavailable_tool_servers(),
        [UnavailableServer {
            name: "broken".into(),
            state: McpServerState::Failed,
        }]
    );
    provider.shutdown().await;

    let slow = McpService::start().await;
    slow.set_directory_delay(Duration::from_secs(30));
    let mut config = http_config("ui5", &slow);
    config.exposure = ToolExposure::Search;
    let provider = McpToolProvider::new(
        McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        ),
        Arc::new(move || vec![config.clone()]),
    );
    provider.mcp_server_statuses(CancellationToken::new()).await;
    assert_eq!(
        provider.unavailable_tool_servers(),
        [UnavailableServer {
            name: "ui5".into(),
            state: McpServerState::Connecting,
        }]
    );
    provider.shutdown().await;
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn unchanged_catalog_reads_reuse_published_work() {
    use tickloop_harness_runtime::{host_tools::HostToolProvider, mcp::McpToolProvider};
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"quote","inputSchema":{"type":"object"}}]));
    let config = http_config("cached", &server);
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let provider =
        McpToolProvider::new(connections.clone(), Arc::new(move || vec![config.clone()]));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let before = connections.performance_counts();
    for _ in 0..2 {
        provider.refresh(CancellationToken::new()).await.unwrap();
        assert_eq!(provider.definitions().len(), 1);
        assert_eq!(provider.discovery_tools().len(), 1);
        provider.mcp_instructions();
        connections.statuses();
    }
    let after = connections.performance_counts();
    connections.shutdown().await;
    assert_eq!(
        before, after,
        "reads must not rebuild the catalog or redactor"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn request_timing_lines_split_preparation_per_chat_run_and_request() {
    use tickloop_harness_protocol::EventKind;
    use tickloop_harness_runtime::{Agent, host_tools::HostToolProvider, mcp::McpToolProvider};
    use tickloop_harness_test_support::{ScriptedModel, config, text};
    use tracing_subscriber::util::SubscriberInitExt;
    #[derive(Clone, Default)]
    struct Log(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = Log::default();
    let writer = log.clone();
    let _subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with_writer(move || writer.clone())
        .finish()
        .set_default();
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"quote","inputSchema":{"type":"object"}}]));
    let server_config = http_config("timed", &server);
    let provider = Arc::new(McpToolProvider::new(
        McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        ),
        Arc::new(move || vec![server_config.clone()]),
    ));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![text("done")])));
    agent.install_host_tools(provider).unwrap();
    let root = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "hello".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    agent.shutdown().await;
    let log = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    let timing: Vec<_> = log
        .lines()
        .filter(|line| line.contains("request_preparation{"))
        .collect();
    let chat = format!("chat={chat:?}");
    assert!(
        !timing.is_empty()
            && timing.iter().all(|line| line.contains(&chat)
                && line.contains("run=")
                && line.contains("request=")),
        "{log}"
    );
    for phase in [
        "refresh",
        "configuration",
        "reconcile_lock_wait",
        "reconcile",
        "wait_for_startup",
        "chats_lock_wait",
        "ToolRegistry::for_chat",
        "mcp_instructions",
        "announce_guidance",
        "model_dispatch",
    ] {
        let phase = format!("phase=\"{phase}\"");
        assert!(
            timing.iter().any(|line| line.contains(&phase)),
            "missing {phase}: {log}"
        );
    }
    assert!(
        timing
            .iter()
            .any(|line| line.contains("request catalog summary")
                && line.contains("catalog_reads=")
                && line.contains("catalog_builds=0")),
        "{log}"
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
#[ignore = "isolated MCP performance scenario; run explicitly in debug and release"]
async fn mcp_catalog_performance_scenario() {
    use tickloop_harness_runtime::{host_tools::HostToolProvider, mcp::McpToolProvider};
    let count = if cfg!(debug_assertions) { 30 } else { 200 };
    let server = McpService::start().await;
    let properties: serde_json::Map<String, serde_json::Value> = (0..12).map(|i| (format!("field_{i}"), json!({"type":"string","description":format!("The field number {i} used by the tool, e.g. 700.HK or AAPL.US; see docs for details."),"enum":["a","b","c"]}))).collect();
    server.set_tools(json!((0..count).map(|i| json!({"name":format!("tool_{i}"),"description":"Get the latest quote of a security; returns price, volume and turnover.","inputSchema":{"type":"object","properties":properties,"required":["field_0"],"additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"}})).collect::<Vec<_>>()));
    let environment = [43, 40, 41, 39, 44, 42, 1580, 1581]
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            (
                format!("SECRET_{i}"),
                (0..n).map(|j| (b'a' + (j % 26) as u8) as char).collect(),
            )
        })
        .collect();
    let connections =
        McpConnections::new(HostCapabilities::default(), BTreeMap::new(), environment);
    // As the Daemon does, the host reports its configuration once and then "unchanged".
    let configuration =
        std::sync::Mutex::new(Some(tickloop_harness_runtime::mcp::McpConfiguration {
            servers: vec![http_config("bench", &server)],
            warnings: Vec::new(),
        }));
    let provider = McpToolProvider::with_configuration_updates(
        connections.clone(),
        Arc::new(move || configuration.lock().unwrap().take()),
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    let provider = Arc::new(provider);
    let agent = tickloop_harness_runtime::Agent::new_embedded(
        tickloop_harness_test_support::config(),
        Arc::new(tickloop_harness_test_support::ScriptedModel::new(Vec::new())),
    );
    agent.install_host_tools(provider.clone()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    assert_eq!(provider.definitions().len(), count);
    let median = |mut samples: Vec<std::time::Duration>| {
        samples.sort();
        samples[samples.len() / 2]
    };
    let (mut preparation, mut refresh, mut instructions) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..21 {
        let started = std::time::Instant::now();
        assert!(agent.benchmark_host_tool_preparation(chat).await.unwrap() >= count);
        preparation.push(started.elapsed());
        let started = std::time::Instant::now();
        provider.refresh(CancellationToken::new()).await.unwrap();
        refresh.push(started.elapsed());
        let started = std::time::Instant::now();
        provider.mcp_instructions();
        instructions.push(started.elapsed());
    }
    eprintln!(
        "MCP benchmark tools={count} preparation_p50={:?} refresh_p50={:?} mcp_instructions_p50={:?} preparation_max={:?}",
        median(preparation.clone()),
        median(refresh),
        median(instructions),
        preparation.iter().max().unwrap()
    );
    let secret = "replacement/token+value".repeat(75);
    let credential = tickloop_harness_runtime::mcp::ConnectorCredential {
        server_url: server.url(),
        registration: tickloop_harness_connector::ClientRegistration {
            authorization_server: server.url(), client_id: "fixture".into(), redirect_uri: "http://localhost/callback".into(),
        },
        resource: None,
        token: Some(serde_json::from_value(json!({"access_token":secret,"token_type":"Bearer","issuer":server.url(),"expires_at":u64::MAX,"client_id":"fixture","token_endpoint":server.url()})).unwrap()),
    };
    let before_rotation = connections.performance_counts();
    let rebuild_started = std::time::Instant::now();
    connections.benchmark_remember_credential("bench", credential);
    let credential_rebuild = rebuild_started.elapsed();
    let rebuild_us = connections.last_catalog_build_micros();
    assert_eq!(
        connections.performance_counts(),
        (before_rotation.0 + 1, before_rotation.1 + 1)
    );
    let mut result = json!(
        (0..50_000)
            .map(|i| if i % 1000 == 0 {
                secret.clone()
            } else {
                "a harmless fixture result with no private content. ".repeat(2)
            })
            .collect::<Vec<_>>()
    );
    let bytes = result.to_string().len();
    let started = std::time::Instant::now();
    connections.benchmark_redact_result("bench", &mut result);
    let redaction = started.elapsed();
    assert_eq!(result[0], json!("[redacted]"));
    assert!(
        result
            .as_array()
            .unwrap()
            .iter()
            .all(|value| !value.as_str().unwrap().contains(&secret))
    );
    eprintln!(
        "MCP benchmark rebuild_us={rebuild_us} credential_rebuild={credential_rebuild:?} result_bytes={bytes} nodes=50000 redaction={redaction:?}"
    );
    agent.shutdown().await;
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn catalog_notifications_and_credentials_rebuild_only_the_changed_server() {
    use tickloop_harness_runtime::{host_tools::HostToolProvider, mcp::McpToolProvider};
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    let unchanged = McpService::start().await;
    unchanged.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    let credentials = Arc::new(InMemoryCredentials::default());
    let saved = |secret: &str| {
        tickloop_harness_runtime::mcp::ConnectorCredential {
        server_url: server.url(),
        registration: tickloop_harness_connector::ClientRegistration { authorization_server: server.url(), client_id: "fixture".into(), redirect_uri: "http://localhost/callback".into() },
        resource: None,
        token: Some(serde_json::from_value(json!({"access_token":secret,"token_type":"Bearer","issuer":server.url(),"expires_at":u64::MAX,"client_id":"fixture","token_endpoint":server.url()})).unwrap()),
    }
    };
    credentials.replace(Some(saved("old-token")));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::from([(
            "changed".into(),
            credentials.clone() as Arc<dyn tickloop_harness_runtime::mcp::ConnectorCredentials>,
        )]),
        BTreeMap::new(),
    );
    let configs = Arc::new(std::sync::Mutex::new(vec![
        http_config("changed", &server),
        http_config("unchanged", &unchanged),
    ]));
    let source = configs.clone();
    let provider = McpToolProvider::new(
        connections.clone(),
        Arc::new(move || source.lock().unwrap().clone()),
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    let before = connections.performance_counts();
    server.set_tools(json!([{"name":"second","inputSchema":{"type":"object"}}]));
    server.notify_tools_changed();
    assert!(matches!(
        connections
            .call("mcp__changed__probe", json!({}), CancellationToken::new())
            .await,
        HostCallResult::Completed(_)
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while provider
            .definitions()
            .iter()
            .filter(|tool| tool.name.ends_with("__second"))
            .count()
            != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let listed = connections.performance_counts();
    assert_eq!(listed, (before.0 + 1, before.1));
    configs.lock().unwrap()[0].description = "new description".into();
    provider.refresh(CancellationToken::new()).await.unwrap();
    settled(&connections).await;
    let configured = connections.performance_counts();
    assert_eq!(configured, (listed.0 + 1, listed.1));
    let secret = "new/private+token\"value";
    let encoded = percent_encoding::utf8_percent_encode(secret, percent_encoding::NON_ALPHANUMERIC)
        .to_string();
    let hex: String = secret.bytes().map(|byte| format!("%{byte:02X}")).collect();
    let escaped = serde_json::to_string(secret).unwrap();
    credentials.replace(Some(saved(secret)));
    provider.refresh(CancellationToken::new()).await.unwrap();
    settled(&connections).await;
    assert_eq!(
        connections.performance_counts(),
        (configured.0 + 1, configured.1 + 1),
        "a renewed login republishes the catalog with its new secret, without listing again"
    );
    server.set_tools(json!([{"name":"second","description":format!("{} {} {} {}", secret, encoded, hex, escaped.trim_matches('"')),"inputSchema":{"type":"object"}}]));
    server.notify_tools_changed();
    assert!(matches!(
        connections
            .call("mcp__changed__second", json!({}), CancellationToken::new())
            .await,
        HostCallResult::Completed(_)
    ));
    let definition =
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(definition) = provider.definitions().into_iter().find(|tool| {
                    tool.name == "mcp__changed__second" && !tool.description.is_empty()
                }) {
                    return definition;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    assert_eq!(
        definition.description,
        "[redacted] [redacted] [redacted] [redacted]"
    );
    connections.shutdown().await;
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn model_requests_and_tick_artifact_mcp_calls_reuse_third_party_catalogs() {
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tickloop_harness_protocol::{ChatId, EventKind, ExecutionMode, ToolDefinition, ToolResult};
    use tickloop_harness_runtime::{
        Agent,
        host_tools::{HostToolPermission, HostToolProvider},
        mcp::McpToolProvider,
        mcp_tools::McpHostToolProvider,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, artifact_service::ArtifactService, call, config, text,
    };
    struct TickFixture(AtomicU64);
    #[async_trait]
    impl HostToolProvider for TickFixture {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "TickRead".into(),
                description: "Fixture Tick state".into(),
                input_schema: json!({"type":"object","properties":{}}),
            }]
        }
        fn catalog_generation(&self) -> Option<u64> {
            Some(0)
        }
        fn validate_input(
            &self,
            _: &str,
            _: &serde_json::Value,
            _: &serde_json::Value,
        ) -> Result<(), String> {
            self.0.fetch_or(1, Ordering::SeqCst);
            Ok(())
        }
        fn permission(&self, _: ChatId, _: &str, _: &serde_json::Value) -> HostToolPermission {
            self.0.fetch_or(2, Ordering::SeqCst);
            HostToolPermission::ReadOnly
        }
        async fn prepare(
            &self,
            chat: ChatId,
            name: &str,
            input: &serde_json::Value,
            _: CancellationToken,
        ) -> Result<HostToolPermission, String> {
            self.0.fetch_or(4, Ordering::SeqCst);
            Ok(self.permission(chat, name, input))
        }
        async fn revalidate(
            &self,
            _: ChatId,
            _: &str,
            _: &serde_json::Value,
            _: HostToolPermission,
            _: CancellationToken,
        ) -> Result<(), String> {
            self.0.fetch_or(8, Ordering::SeqCst);
            Ok(())
        }
        async fn execute(
            &self,
            _: ChatId,
            _: &str,
            _: serde_json::Value,
            _: CancellationToken,
        ) -> ToolResult {
            self.0.fetch_or(16, Ordering::SeqCst);
            ToolResult {
                content: "fixture state".into(),
                is_error: false,
                output_ref: None,
            }
        }
    }
    let root = tempfile::tempdir().unwrap();
    let service = ArtifactService::start().await;
    let native = Arc::new(TickFixture(AtomicU64::new(0)));
    let artifact = Arc::new(McpHostToolProvider::new(
        service.login(root.path()).await,
        Some(native.clone()),
    ));
    artifact.refresh(CancellationToken::new()).await.unwrap();
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"probe","inputSchema":{"type":"object"}}]));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let configuration =
        std::sync::Mutex::new(Some(tickloop_harness_runtime::mcp::McpConfiguration {
            servers: vec![http_config("cached", &server)],
            warnings: Vec::new(),
        }));
    let provider = Arc::new(McpToolProvider::with_configuration_updates(
        connections.clone(),
        Arc::new(move || configuration.lock().unwrap().take()),
    ));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(vec![artifact as Arc<dyn HostToolProvider>, provider])
        .unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let before = agent.mcp_performance_counts();
    let mut events = agent.subscribe();
    for name in [
        None,
        None,
        Some("TickRead"),
        Some("ArtifactRead"),
        Some("mcp__cached__probe"),
    ] {
        if let Some(name) = name {
            model.enqueue(call(
                name,
                name,
                if name == "ArtifactRead" {
                    json!({"action":"list"})
                } else {
                    json!({})
                },
            ));
        }
        model.enqueue(text("done"));
        agent.submit(chat, "run fixture".into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.unwrap().kind {
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.operations.len(), 3);
    assert!(snapshot.operations.iter().all(|operation| {
        operation
            .result
            .as_ref()
            .is_some_and(|result| !result.is_error)
    }));
    assert_eq!(native.0.load(Ordering::SeqCst), 31);
    assert_eq!(agent.mcp_performance_counts(), before);
    agent.shutdown().await;
}
