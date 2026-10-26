use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tickloop_harness::protocol::mcp::{McpServerConfig, McpTransport};

/// An enabled plugin's raw entries and host-owned locations.
pub struct PluginServers {
    pub name: String,
    pub root: PathBuf,
    pub data_directory: PathBuf,
    pub entries: BTreeMap<String, Value>,
}

pub struct ResolvedServers {
    pub servers: Vec<McpServerConfig>,
    pub warnings: Vec<String>,
}

impl crate::LoadedSettings {
    pub fn mcp_servers(
        &self,
        directory: &Path,
        environment: &BTreeMap<String, String>,
    ) -> ResolvedServers {
        self.mcp_servers_with_plugins(directory, environment, &[])
    }

    pub fn mcp_servers_with_plugins(
        &self,
        directory: &Path,
        environment: &BTreeMap<String, String>,
        plugins: &[PluginServers],
    ) -> ResolvedServers {
        resolve_servers_with_plugins(
            &Value::Object(self.settings.clone()),
            directory,
            environment,
            plugins,
        )
    }
}

pub fn resolve_servers(
    settings: &Value,
    directory: &Path,
    environment: &BTreeMap<String, String>,
) -> ResolvedServers {
    resolve_servers_with_plugins(settings, directory, environment, &[])
}

pub fn resolve_servers_with_plugins(
    settings: &Value,
    directory: &Path,
    environment: &BTreeMap<String, String>,
    plugins: &[PluginServers],
) -> ResolvedServers {
    let mut resolved = ResolvedServers {
        servers: Vec::new(),
        warnings: Vec::new(),
    };
    let mut entries = BTreeMap::new();
    let project = &settings["projects"][directory.to_string_lossy().as_ref()];
    for (source, layer) in [("global", settings), ("directory", project)] {
        match layer.get("mcpServers") {
            Some(Value::Object(servers)) => {
                for (name, entry) in servers {
                    entries.insert(name, (source, entry));
                }
            }
            Some(_) => resolved
                .warnings
                .push(format!("{source} mcpServers must be an object")),
            None => {}
        }
    }
    let mut signatures = BTreeMap::new();
    for (name, (source, entry)) in entries {
        let valid_name = !name.is_empty()
            && !name.starts_with("plugin_")
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-');
        if !valid_name {
            resolved.warnings.push(
                "invalid user MCP server name (letters, digits, _ and - only; plugin_ is reserved)"
                    .into(),
            );
            continue;
        }
        resolved.push(name.clone(), source.into(), entry, directory, environment);
        if let Some(signature) = signature(entry, directory, environment) {
            signatures.entry(signature).or_insert_with(|| name.clone());
        }
    }
    let mut plugins = plugins.iter().collect::<Vec<_>>();
    plugins.sort_by(|left, right| left.name.cmp(&right.name));
    for plugin in plugins {
        let mut environment = environment.clone();
        for key in ["CLAUDE_PLUGIN_ROOT", "TICKLOOP_PLUGIN_ROOT"] {
            environment.insert(key.into(), plugin.root.to_string_lossy().into_owned());
        }
        for key in ["CLAUDE_PLUGIN_DATA", "TICKLOOP_PLUGIN_DATA"] {
            environment.insert(
                key.into(),
                plugin.data_directory.to_string_lossy().into_owned(),
            );
        }
        for (name, entry) in &plugin.entries {
            let name = format!("plugin_{}_{name}", plugin.name);
            resolved.push(
                name.clone(),
                format!("plugin:{}", plugin.name),
                entry,
                directory,
                &environment,
            );
            if let Some(signature) = signature(entry, directory, &environment) {
                if let Some(preferred) = signatures.get(&signature) {
                    resolved.warnings.push(format!(
                        "MCP {name} skipped: same command or URL as {preferred}"
                    ));
                    resolved.servers.pop();
                } else {
                    signatures.insert(signature, name);
                }
            }
        }
    }
    resolved
}

impl ResolvedServers {
    fn push(
        &mut self,
        name: String,
        source: String,
        entry: &Value,
        directory: &Path,
        environment: &BTreeMap<String, String>,
    ) {
        let plugin = source.starts_with("plugin:");
        if plugin && let Some(fields) = entry.as_object() {
            for field in fields.keys().filter(|field| is_approval_field(field)) {
                self.warnings
                    .push(format!("MCP {name}: approval field {field} ignored"));
            }
        }
        let mut transport = parse_transport(entry, directory, environment)
            .unwrap_or_else(|reason| McpTransport::Failed { reason });
        if plugin
            && needs_user_config(entry)
            && !matches!(transport, McpTransport::Unsupported { .. })
        {
            transport = McpTransport::NeedsConfiguration;
        }
        match &transport {
            McpTransport::Failed { reason } => self.warnings.push(format!("MCP {name}: {reason}")),
            McpTransport::NeedsConfiguration => self.warnings.push(format!(
                "MCP {name}: needs userConfig; configure the plugin before connecting"
            )),
            McpTransport::Unsupported { .. } => self
                .warnings
                .push(format!("MCP {name}: transport is not supported yet")),
            _ => {}
        }
        self.servers.push(McpServerConfig {
            name,
            source,
            transport,
            timeout_ms: entry["timeout"]
                .as_u64()
                .filter(|n| *n >= 1000)
                .unwrap_or(300_000),
        });
    }
}

fn is_approval_field(field: &str) -> bool {
    let normalized = field
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    normalized.contains("approv")
        || normalized.contains("permission")
        || matches!(
            normalized.as_str(),
            "alwaysallow" | "allowedtools" | "trust" | "trusted"
        )
}

fn signature(
    entry: &Value,
    directory: &Path,
    environment: &BTreeMap<String, String>,
) -> Option<String> {
    let identity = serde_json::json!({
        "type": entry["type"], "command": entry["command"],
        "args": entry.get("args").cloned().unwrap_or_else(|| serde_json::json!([])), "url": entry["url"]
    });
    match parse_transport(&identity, directory, environment).ok()? {
        McpTransport::Stdio { command, args, .. } => {
            Some(serde_json::to_string(&("stdio", command, args)).expect("strings serialize"))
        }
        McpTransport::Http { url, .. } => {
            Some(serde_json::to_string(&("http", url)).expect("strings serialize"))
        }
        _ => None,
    }
}

pub fn needs_user_config(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("${user_config."),
        Value::Array(items) => items.iter().any(needs_user_config),
        Value::Object(fields) => fields.values().any(needs_user_config),
        _ => false,
    }
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

fn parse_transport(
    value: &Value,
    directory: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<McpTransport, String> {
    let entry: Entry = serde_json::from_value(value.clone())
        .map_err(|_| "invalid MCP server fields".to_owned())?;
    let expand = |text: &str| expand_variables(text, environment);
    let kind = entry.kind.as_deref().unwrap_or(if entry.command.is_some() {
        "stdio"
    } else {
        "http"
    });
    match kind {
        "stdio" => {
            let command = expand(
                entry
                    .command
                    .as_deref()
                    .ok_or("stdio server needs command")?,
            )?;
            if command.is_empty() {
                return Err("stdio server needs command".into());
            }
            let args = entry
                .args
                .iter()
                .map(|arg| expand(arg))
                .collect::<Result<_, _>>()?;
            let env = entry
                .env
                .iter()
                .map(|(key, value)| Ok((key.clone(), expand(value)?)))
                .collect::<Result<_, String>>()?;
            let cwd = entry
                .cwd
                .as_deref()
                .map(expand)
                .transpose()?
                .map(|cwd| directory.join(cwd))
                .unwrap_or_else(|| directory.to_owned());
            Ok(McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            })
        }
        "http" | "streamable-http" => {
            let url = expand(entry.url.as_deref().ok_or("HTTP server needs url")?)?;
            let parsed = reqwest::Url::parse(&url).map_err(|_| "invalid MCP URL")?;
            let local = parsed.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            if !(parsed.scheme() == "https" || parsed.scheme() == "http" && local)
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err("MCP URL requires HTTPS (HTTP is allowed only on loopback), without userinfo or fragment".into());
            }
            let headers = entry
                .headers
                .iter()
                .map(|(key, value)| Ok((key.clone(), expand(value)?)))
                .collect::<Result<_, String>>()?;
            Ok(McpTransport::Http { url, headers })
        }
        _ => Ok(McpTransport::Unsupported { kind: kind.into() }),
    }
}

fn expand_variables(text: &str, environment: &BTreeMap<String, String>) -> Result<String, String> {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let variable = &rest[start + 2..];
        let end = variable
            .find('}')
            .ok_or("unterminated MCP environment variable")?;
        let expression = &variable[..end];
        let (name, default) = expression
            .split_once(":-")
            .map_or((expression, None), |(name, default)| (name, Some(default)));
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return Err("invalid MCP environment variable name".into());
        }
        let value = environment
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .or(default)
            .ok_or_else(|| format!("missing environment variable {name}"))?;
        output.push_str(value);
        rest = &variable[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}
