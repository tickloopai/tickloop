use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tickloop_harness::protocol::mcp::{McpOAuthConfig, McpServerConfig, McpTransport};

/// An enabled plugin's raw entries and host-owned locations.
pub struct PluginServers {
    pub name: String,
    pub root: PathBuf,
    pub data_directory: PathBuf,
    pub entries: BTreeMap<String, Value>,
    pub user_config: crate::plugin_config::PluginUserConfig,
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
        environment.extend(plugin.user_config.variables());
        for (name, entry) in &plugin.entries {
            let name = format!("plugin_{}_{name}", plugin.name);
            resolved.push(
                name.clone(),
                format!("plugin:{}", plugin.name),
                entry,
                directory,
                &environment,
            );
            if let Some(server) = resolved.servers.last_mut() {
                server.sensitive_values = plugin.user_config.secrets_for(entry);
            }
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
        let mut entry = entry.clone();
        if plugin && let Some(fields) = entry.as_object_mut() {
            fields.retain(|field, _| !is_approval_field(field));
        }
        let expanded = if transport_kind(&entry).is_ok_and(|kind| {
            !matches!(kind, "stdio" | "http" | "streamable-http") && !kind.contains("${")
        }) {
            Ok(entry.clone())
        } else if plugin && missing_user_config(&entry, environment) {
            Err("missing userConfig; configure the plugin".into())
        } else {
            expand_value(&entry, environment)
        };
        let mut transport = match &expanded {
            Ok(entry) => parse_transport(entry, directory)
                .unwrap_or_else(|reason| McpTransport::Failed { reason }),
            Err(reason) if plugin && reason.starts_with("missing userConfig") => {
                McpTransport::NeedsConfiguration
            }
            Err(reason) => McpTransport::Failed {
                reason: reason.clone(),
            },
        };
        let expanded_entry = expanded.as_ref().unwrap_or(&entry);
        let oauth = if matches!(&transport, McpTransport::Http { headers, .. } if !headers.keys().any(|name| name.eq_ignore_ascii_case("authorization")))
        {
            match parse_oauth(expanded_entry) {
                Ok(oauth) => oauth,
                Err(reason) => {
                    transport = McpTransport::Failed { reason };
                    McpOAuthConfig::default()
                }
            }
        } else {
            McpOAuthConfig::default()
        };

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
            sensitive_values: Vec::new(),
            oauth,
            timeout_ms: expanded_entry["timeout"]
                .as_u64()
                .or_else(|| expanded_entry["timeout"].as_str()?.parse().ok())
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
    match parse_transport(&expand_value(&identity, environment).ok()?, directory).ok()? {
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
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains("${user_config.") || needs_user_config(value)),
        _ => false,
    }
}

#[derive(Deserialize)]
struct Entry {
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

/// The declared transport, including the same inference used when connecting.
pub fn transport_kind(entry: &Value) -> Result<&str, &'static str> {
    match entry.get("type") {
        Some(Value::String(kind)) => Ok(kind),
        None | Some(Value::Null) => Ok(if entry["command"].is_string() {
            "stdio"
        } else {
            "http"
        }),
        Some(_) => Err("invalid MCP transport type"),
    }
}

fn parse_transport(value: &Value, directory: &Path) -> Result<McpTransport, String> {
    let entry: Entry = serde_json::from_value(value.clone())
        .map_err(|_| "invalid MCP server fields".to_owned())?;
    let kind = transport_kind(value)?;
    match kind {
        "stdio" => {
            let command = entry.command.ok_or("stdio server needs command")?;
            if command.is_empty() {
                return Err("stdio server needs command".into());
            }
            let cwd = entry
                .cwd
                .map(|cwd| directory.join(cwd))
                .unwrap_or_else(|| directory.to_owned());
            Ok(McpTransport::Stdio {
                command,
                args: entry.args,
                env: entry.env,
                cwd,
            })
        }
        "http" | "streamable-http" => {
            let url = entry.url.ok_or("HTTP server needs url")?;
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
            Ok(McpTransport::Http {
                url,
                headers: entry.headers,
            })
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
        if name.is_empty()
            || (!name.starts_with("user_config.")
                && !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
        {
            return Err("invalid MCP environment variable name".into());
        }
        let value = environment
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .or(default)
            .ok_or_else(|| {
                if name.starts_with("user_config.") {
                    "missing userConfig; configure the plugin".into()
                } else {
                    format!("missing environment variable {name}")
                }
            })?;
        output.push_str(value);
        rest = &variable[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn expand_value(value: &Value, environment: &BTreeMap<String, String>) -> Result<Value, String> {
    match value {
        Value::String(text) => expand_variables(text, environment).map(Value::String),
        Value::Array(items) => items
            .iter()
            .map(|item| expand_value(item, environment))
            .collect(),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| {
                Ok((
                    expand_variables(key, environment)?,
                    expand_value(value, environment)?,
                ))
            })
            .collect::<Result<serde_json::Map<_, _>, String>>()
            .map(Value::Object),
        _ => Ok(value.clone()),
    }
}

pub(crate) fn user_config_references(entry: &Value) -> BTreeMap<String, bool> {
    fn text_references(text: &str, found: &mut BTreeMap<String, bool>) {
        for (start, _) in text.match_indices("${user_config.") {
            let expression = &text[start + "${user_config.".len()..];
            let Some(end) = expression.find('}') else {
                continue;
            };
            let expression = &expression[..end];
            let (name, default) = expression
                .split_once(":-")
                .map_or((expression, None), |(name, default)| (name, Some(default)));
            found
                .entry(name.to_owned())
                .and_modify(|has_default| *has_default &= default.is_some())
                .or_insert(default.is_some());
        }
    }
    fn visit(value: &Value, found: &mut BTreeMap<String, bool>) {
        match value {
            Value::String(text) => text_references(text, found),
            Value::Array(items) => {
                for item in items {
                    visit(item, found);
                }
            }
            Value::Object(fields) => {
                for (key, value) in fields {
                    text_references(key, found);
                    visit(value, found);
                }
            }
            _ => {}
        }
    }
    let mut found = BTreeMap::new();
    if let Some(fields) = entry.as_object() {
        for (key, value) in fields.iter().filter(|(key, _)| !is_approval_field(key)) {
            text_references(key, &mut found);
            visit(value, &mut found);
        }
    } else {
        visit(entry, &mut found);
    }
    found
}

pub(crate) fn missing_user_config(value: &Value, variables: &BTreeMap<String, String>) -> bool {
    user_config_references(value)
        .iter()
        .any(|(key, has_default)| {
            !has_default && !variables.contains_key(&format!("user_config.{key}"))
        })
}

fn parse_oauth(entry: &Value) -> Result<McpOAuthConfig, String> {
    let mut value = entry
        .get("oauth")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(port) = value.get("callbackPort").and_then(Value::as_str) {
        let port = port
            .parse::<u16>()
            .map_err(|_| "invalid MCP OAuth callback port")?;
        value["callbackPort"] = Value::from(port);
    }
    let mut oauth: McpOAuthConfig =
        serde_json::from_value(value).map_err(|_| "invalid MCP OAuth settings")?;
    if let Some(metadata) = entry.get("authServerMetadataUrl") {
        oauth.authorization_server_metadata_url = Some(
            metadata
                .as_str()
                .ok_or("invalid MCP OAuth metadata URL")?
                .into(),
        );
    }
    Ok(oauth)
}
