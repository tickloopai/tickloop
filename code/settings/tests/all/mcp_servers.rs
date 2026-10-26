use serde_json::json;
use std::{collections::BTreeMap, path::Path};
use tickloop_code_settings::mcp::resolve_servers;

#[test]
fn directory_overrides_global_and_expands_only_the_startup_environment() {
    let settings = json!({"mcpServers": {
        "local": {"command":"old"},
        "remote": {"url":"https://example.com/mcp", "headers":{"X-Key":"${KEY}"}}
    }, "projects":{"/work":{"mcpServers":{
        "local":{"command":"${BIN:-python3}","args":["${SCRIPT}"],"env":{"MODE":"${MODE:-test}"}}
    }}}});
    let environment = BTreeMap::from([
        ("KEY".into(), "secret".into()),
        ("SCRIPT".into(), "server.py".into()),
    ]);
    let resolved = resolve_servers(&settings, Path::new("/work"), &environment);
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    assert_eq!(resolved.servers.len(), 2);
    let local = &resolved.servers[0];
    assert_eq!(local.name, "local");
    assert_eq!(
        serde_json::to_value(&local.transport).unwrap(),
        json!({"type":"stdio", "command":"python3", "args":["server.py"], "env":{"MODE":"test"}, "cwd":"/work"})
    );
    assert_eq!(resolved.servers[1].name, "remote");
}

#[test]
fn invalid_entries_do_not_hide_valid_servers_or_leak_substituted_values() {
    use tickloop_harness::protocol::mcp::McpTransport;
    let settings = json!({"mcpServers":{
        "good":{"type":"streamable-http","url":"http://127.0.0.1:3210/mcp","timeout":5000},
        "missing":{"command":"${ABSENT}"},
        "unsupported":{"type":"sse","url":"https://example.com"},
        "unsafe":{"url":"http://example.com/${KEY}"},
        "plugin_reserved":{"command":"no"},
        "bad.name":{"command":"no"}
    }});
    let resolved = resolve_servers(
        &settings,
        Path::new("/work"),
        &BTreeMap::from([("KEY".into(), "private-value".into())]),
    );
    assert_eq!(resolved.servers.len(), 4);
    assert!(matches!(
        resolved.servers[0].transport,
        McpTransport::Http { .. }
    ));
    assert_eq!(resolved.servers[0].timeout_ms, 5000);
    assert!(
        matches!(&resolved.servers[1].transport, McpTransport::Failed {reason} if reason.contains("ABSENT"))
    );
    assert!(matches!(
        resolved.servers[2].transport,
        McpTransport::Failed { .. }
    ));
    assert!(matches!(
        resolved.servers[3].transport,
        McpTransport::Unsupported { .. }
    ));
    assert!(!resolved.warnings.join(" ").contains("private-value"));
}

#[test]
fn plugins_resolve_after_users_with_roots_and_deterministic_signature_priority() {
    use tickloop_code_settings::mcp::{PluginServers, resolve_servers_with_plugins};
    let entries: BTreeMap<String, serde_json::Value> = serde_json::from_value(json!({
        "local":{"command":"${TICKLOOP_PLUGIN_ROOT}/server", "args":["${CLAUDE_PLUGIN_DATA}","${MODE:-test}"], "env":{"ROOT":"${CLAUDE_PLUGIN_ROOT}","DATA":"${TICKLOOP_PLUGIN_DATA}"}},
        "remote":{"url":"https://example.com/mcp", "headers":{"X-Key":"ignored-for-signature"}},
        "shared":{"command":"shared","args":["serve"],"autoApprove":["*"]},
        "configure":{"command":"node", "args":["${user_config.key}"]}
    })).unwrap();
    let plugins = ["zeta", "alpha"].map(|name| PluginServers {
        name: name.into(),
        root: format!("/plugins/{name}").into(),
        data_directory: format!("/data/{name}").into(),
        entries: entries.clone(),
    });
    let result = resolve_servers_with_plugins(
        &json!({"mcpServers":{"mine":{"url":"https://example.com/mcp"}}}),
        Path::new("/work"),
        &BTreeMap::new(),
        &plugins,
    );
    assert_eq!(
        result
            .servers
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        [
            "mine",
            "plugin_alpha_configure",
            "plugin_alpha_local",
            "plugin_alpha_shared",
            "plugin_zeta_configure",
            "plugin_zeta_local"
        ]
    );
    assert_eq!(
        serde_json::to_value(&result.servers[2].transport).unwrap(),
        json!({"type":"stdio","command":"/plugins/alpha/server","args":["/data/alpha","test"],"env":{"ROOT":"/plugins/alpha","DATA":"/data/alpha"},"cwd":"/work"})
    );
    assert_eq!(
        serde_json::to_value(&result.servers[1].transport).unwrap()["type"],
        "needs_configuration"
    );
    let warnings = result.warnings.join("\n");
    assert!(warnings.contains("plugin_alpha_remote") && warnings.contains("mine"));
    assert!(warnings.contains("plugin_zeta_shared") && warnings.contains("plugin_alpha_shared"));
    assert!(warnings.contains("autoApprove") && warnings.contains("ignored"));
}

#[test]
fn signature_priority_does_not_depend_on_credentials_and_unsupported_stays_unsupported() {
    use tickloop_code_settings::mcp::{PluginServers, resolve_servers_with_plugins};
    let plugin = PluginServers {
        name: "demo".into(),
        root: "/plugin".into(),
        data_directory: "/data".into(),
        entries: serde_json::from_value(json!({
            "remote":{"url":"https://example.com/mcp"},
            "local":{"command":"same", "args":["serve"]},
            "legacy":{"type":"sse", "url":"${user_config.url}"},
            "safe":{"command":"safe", "always_allow":true, "auto_approve":["*"]}
        }))
        .unwrap(),
    };
    let result = resolve_servers_with_plugins(
        &json!({"mcpServers":{
            "remote":{"url":"https://example.com/mcp", "headers":{"Authorization":"${MISSING}"}},
            "local":{"command":"same", "args":["serve"], "env":{"KEY":"${MISSING}"}}
        }}),
        Path::new("/work"),
        &BTreeMap::new(),
        &[plugin],
    );
    assert_eq!(
        result
            .servers
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["local", "remote", "plugin_demo_legacy", "plugin_demo_safe"]
    );
    assert_eq!(
        serde_json::to_value(&result.servers[2].transport).unwrap()["type"],
        "unsupported"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("always_allow") && w.contains("ignored"))
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("auto_approve") && w.contains("ignored"))
    );
}

#[test]
fn remote_oauth_settings_survive_resolution() {
    let resolved = tickloop_code_settings::mcp::resolve_servers(
        &serde_json::json!({"mcpServers":{"remote":{"url":"https://example.com/mcp", "oauth":{"clientId":"client", "callbackPort":9911}, "authServerMetadataUrl":"https://example.com/metadata"}}}),
        std::path::Path::new("/workspace"),
        &Default::default(),
    );
    let oauth = &resolved.servers[0].oauth;
    assert_eq!(oauth.client_id.as_deref(), Some("client"));
    assert_eq!(oauth.callback_port, Some(9911));
    assert_eq!(
        oauth.authorization_server_metadata_url.as_deref(),
        Some("https://example.com/metadata")
    );
}

#[test]
fn a_fixed_authorization_header_does_not_require_valid_oauth_configuration() {
    let resolved = tickloop_code_settings::mcp::resolve_servers(
        &serde_json::json!({"mcpServers":{"fixed":{"url":"https://example.com/mcp","headers":{"authorization":"Bearer fixed"},"oauth":{"clientId":3}}}}),
        std::path::Path::new("/workspace"),
        &Default::default(),
    );
    assert!(matches!(
        resolved.servers[0].transport,
        tickloop_harness::protocol::mcp::McpTransport::Http { .. }
    ));
}
