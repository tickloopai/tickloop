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
