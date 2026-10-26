use std::{collections::BTreeMap, path::Path};

use serde_json::{Value, json};
use tick_plugins::{PluginManifest, PluginStore};
use tickloop_code_settings::mcp::{PluginServers, needs_user_config, resolve_servers_with_plugins};
use tickloop_harness::protocol::mcp::McpTransport;

use tickloop_code_console::terminal::safe_line;

pub(super) fn report(manifest: &PluginManifest, root: &Path, enabled: bool) {
    if manifest.mcp_servers.is_empty() {
        return;
    }
    let data_directory = tickloop_code_settings::config_root_directory()
        .map(|config| PluginStore::in_config_root(&config).data_directory(&manifest.name))
        .unwrap_or_else(|| "<plugin data directory>".into());
    let plugin = PluginServers {
        name: manifest.name.clone(),
        root: root.to_owned(),
        data_directory,
        entries: manifest.mcp_servers.clone(),
    };
    let resolved = resolve_servers_with_plugins(
        &json!({}),
        &std::env::current_dir().unwrap_or_else(|_| root.to_owned()),
        &std::env::vars().collect::<BTreeMap<_, _>>(),
        &[plugin],
    );
    for (name, entry) in &manifest.mcp_servers {
        let name = format!("plugin_{}_{name}", manifest.name);
        let kind = entry["type"]
            .as_str()
            .unwrap_or(if entry.get("command").is_some() {
                "stdio"
            } else {
                "http"
            });
        let (target_kind, target) = if kind == "stdio" {
            let mut command = vec![entry["command"].clone()];
            if let Some(args) = entry["args"].as_array() {
                command.extend(args.iter().cloned());
            }
            ("command", Value::Array(command).to_string())
        } else {
            (
                "address",
                entry["url"].as_str().unwrap_or("(missing)").to_owned(),
            )
        };
        let oauth = if kind != "http" && kind != "streamable-http" {
            "none"
        } else if entry["headers"].as_object().is_some_and(|headers| {
            headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("authorization"))
        }) {
            "none (Authorization header)"
        } else if entry.get("oauth").is_some() || entry.get("authServerMetadataUrl").is_some() {
            "configured"
        } else {
            "discover on connect"
        };
        let user_config = if needs_user_config(entry) {
            "required"
        } else {
            "none"
        };
        let status = match resolved
            .servers
            .iter()
            .find(|server| server.name == name)
            .map(|server| &server.transport)
        {
            Some(McpTransport::Unsupported { .. }) => "unsupported",
            Some(McpTransport::NeedsConfiguration) => "needs configuration",
            Some(McpTransport::Failed { .. }) => "failed",
            Some(_) if !enabled => "disabled",
            Some(_) => "ready",
            None => "skipped duplicate",
        };
        println!(
            "MCP server: {} | transport: {} | {target_kind}: {} | OAuth: {oauth} | userConfig: {user_config} | status: {status}",
            safe_line(&name),
            safe_line(kind),
            safe_line(&target)
        );
    }
    for warning in resolved.warnings {
        println!("Warning: {}", safe_line(&warning));
    }
    println!(
        "Enabled MCP programs run as your user; their commands and addresses above are used from the next Agent request. Placeholders resolve from the Agent startup environment. Installing runs nothing."
    );
}
