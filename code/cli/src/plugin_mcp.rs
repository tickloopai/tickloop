use std::{collections::BTreeMap, path::Path};

use serde_json::{Value, json};
use tick_plugins::{InstalledPlugin, PluginManifest, PluginStore};
use tickloop_code_settings::mcp::{
    PluginServers, ResolvedServers, needs_user_config, resolve_servers_with_plugins, transport_kind,
};
use tickloop_harness::protocol::mcp::McpTransport;

use tickloop_code_console::terminal::safe_line;

pub(super) fn validate(manifest: &PluginManifest, root: &Path) {
    if manifest.mcp_servers.is_empty() {
        return;
    }
    let data_directory = tickloop_code_settings::config_root_directory()
        .map(|config| PluginStore::in_config_root(&config).data_directory(&manifest.name))
        .unwrap_or_else(|| "<plugin data directory>".into());
    let plugin = PluginServers {
        user_config: tickloop_code_settings::plugin_config::PluginUserConfig::defaults(manifest),
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
    render(manifest, true, resolved);
}

pub(super) fn installed(plugin: &InstalledPlugin) {
    if plugin.manifest.mcp_servers.is_empty() {
        return;
    }
    let Some(config_root) = tickloop_code_settings::config_root_directory() else {
        println!("Warning: cannot find the configuration root to report MCP servers");
        return;
    };
    let store = PluginStore::in_config_root(&config_root);
    let installed = match store.installed() {
        Ok(installed) => installed,
        Err(error) => {
            println!(
                "Warning: cannot load enabled MCP servers: {}",
                safe_line(&error.to_string())
            );
            return;
        }
    };
    let plugins = installed
        .plugins
        .into_iter()
        .filter(|plugin| plugin.enabled)
        .map(|plugin| PluginServers {
            user_config: tickloop_code_settings::plugin_config::PluginUserConfig::load(
                &config_root,
                &plugin.manifest,
            )
            .unwrap_or_default(),
            data_directory: store.data_directory(plugin.name()),
            name: plugin.name().to_owned(),
            root: plugin.root,
            entries: plugin.manifest.mcp_servers,
        })
        .collect::<Vec<_>>();
    let directory = std::env::current_dir().unwrap_or_else(|_| plugin.root.clone());
    let settings = tickloop_code_settings::UserSettings::in_root(&config_root).load();
    let mut resolved =
        settings.mcp_servers_with_plugins(&directory, &std::env::vars().collect(), &plugins);
    resolved
        .warnings
        .extend(settings.warnings().iter().cloned());
    render(&plugin.manifest, plugin.enabled, resolved);
}

fn render(manifest: &PluginManifest, enabled: bool, resolved: ResolvedServers) {
    for (name, entry) in &manifest.mcp_servers {
        let name = tickloop_code_settings::mcp::plugin_server_name(&manifest.name, name);
        let kind = transport_kind(entry).unwrap_or("invalid");
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
            None if !enabled => "disabled",
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
        "Ready MCP programs run as your user from the next Agent request; skipped and disabled services do not start. Placeholders resolve from the Agent startup environment. Installing runs nothing."
    );
}
