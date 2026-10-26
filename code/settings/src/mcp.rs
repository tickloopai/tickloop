use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tickloop_harness::protocol::mcp::{
    McpOAuthConfig, McpServerConfig, McpTransport, ToolExposure,
};

const PROJECTS_KEY: &str = "projects";
const SERVERS_KEY: &str = "mcpServers";
/// Servers turned off in one directory, user and plugin servers alike, by their resolved name.
const DISABLED_SERVERS_KEY: &str = "disabledMcpServers";
/// The names of plugin servers start with this; user servers cannot.
pub const PLUGIN_SERVER_PREFIX: &str = "plugin_";
pub const ARTIFACTS_SERVER_NAME: &str = "tickloop-artifacts";
/// The sources of servers: the user's global and directory settings, and `plugin:<plugin>`.
pub const GLOBAL_SOURCE: &str = "global";
pub const DIRECTORY_SOURCE: &str = "directory";
pub const PLUGIN_SOURCE_PREFIX: &str = "plugin:";

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
    /// Whether `layer` configures a server called `name`.
    pub fn has_mcp_server(&self, layer: &McpLayer, name: &str) -> bool {
        let servers = match layer {
            McpLayer::Global => self.settings.get(SERVERS_KEY),
            McpLayer::Directory(directory) => self
                .settings
                .get(PROJECTS_KEY)
                .and_then(|projects| projects.get(crate::project_key(directory)))
                .and_then(|project| project.get(SERVERS_KEY)),
        };
        servers.is_some_and(|servers| servers.get(name).is_some())
    }

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

/// Where a user server is configured: for every directory, or for one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpLayer {
    Global,
    Directory(PathBuf),
}

impl McpLayer {
    /// The settings `servers` of this layer, created when missing.
    fn servers<'a>(
        &self,
        settings: &'a mut serde_json::Map<String, Value>,
    ) -> Result<&'a mut serde_json::Map<String, Value>, crate::InvalidEntry> {
        match self {
            Self::Global => crate::object_entry(settings, SERVERS_KEY, SERVERS_KEY.into()),
            Self::Directory(directory) => {
                let key = crate::project_key(directory);
                let projects = crate::object_entry(settings, PROJECTS_KEY, PROJECTS_KEY.into())?;
                let project =
                    crate::object_entry(projects, &key, format!("{PROJECTS_KEY}[{key:?}]"))?;
                crate::object_entry(
                    project,
                    SERVERS_KEY,
                    format!("{PROJECTS_KEY}[{key:?}].{SERVERS_KEY}"),
                )
            }
        }
    }
}

impl crate::UserSettings {
    /// Adds the server `name` to `layer`; false, changing nothing, when the layer has one already.
    pub fn add_mcp_server(
        &self,
        layer: &McpLayer,
        name: &str,
        entry: Value,
    ) -> Result<bool, crate::SettingsError> {
        let mut added = false;
        self.update(|settings| {
            let servers = layer.servers(settings)?;
            if !servers.contains_key(name) {
                servers.insert(name.to_owned(), entry);
                added = true;
            }
            Ok(())
        })?;
        Ok(added)
    }

    /// Removes the server `name` from `layer`; false when the layer has no such server.
    pub fn remove_mcp_server(
        &self,
        layer: &McpLayer,
        name: &str,
    ) -> Result<bool, crate::SettingsError> {
        let mut removed = false;
        self.update(|settings| {
            removed = layer.servers(settings)?.remove(name).is_some();
            Ok(())
        })?;
        Ok(removed)
    }

    /// Turns the server `name`, a user or plugin server, off or on again in `directory` only.
    pub fn set_mcp_server_disabled(
        &self,
        directory: &Path,
        name: &str,
        disabled: bool,
    ) -> Result<(), crate::SettingsError> {
        self.update(|settings| {
            let key = crate::project_key(directory);
            let location = format!("{PROJECTS_KEY}[{key:?}].{DISABLED_SERVERS_KEY}");
            let projects = crate::object_entry(settings, PROJECTS_KEY, PROJECTS_KEY.into())?;
            let project = crate::object_entry(projects, &key, format!("{PROJECTS_KEY}[{key:?}]"))?;
            let names = project
                .entry(DISABLED_SERVERS_KEY)
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or(crate::InvalidEntry {
                    key: location,
                    expected: "an array".into(),
                })?;
            names.retain(|listed| listed.as_str() != Some(name));
            if disabled {
                names.push(Value::String(name.to_owned()));
            }
            Ok(())
        })
    }
}

/// Checks an entry the user is adding: its transport and, unless it names variables to expand
/// later, its address.
pub fn validate_entry(entry: &Value, directory: &Path) -> Result<(), String> {
    if needs_expansion(entry) {
        return Ok(());
    }
    parse_transport(entry, directory).map(|_| ())
}

fn needs_expansion(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("${"),
        Value::Array(items) => items.iter().any(needs_expansion),
        Value::Object(fields) => fields.values().any(needs_expansion),
        _ => false,
    }
}

/// The MCP servers of the enabled plugins in `store`, without preparing their data directories.
pub fn enabled_plugin_servers(
    store: &tick_plugins::PluginStore,
) -> (Vec<PluginServers>, Vec<String>) {
    let installed = match store.installed() {
        Ok(installed) => installed,
        Err(error) => {
            return (
                Vec::new(),
                vec![format!("Cannot load plugin MCP servers: {error}")],
            );
        }
    };
    let plugins = installed
        .plugins
        .into_iter()
        .filter(|plugin| plugin.enabled && !plugin.manifest.mcp_servers.is_empty())
        .map(|plugin| PluginServers {
            user_config: crate::plugin_config::PluginUserConfig::load(
                store.config_root(),
                &plugin.manifest,
            )
            .unwrap_or_default(),
            data_directory: store.data_directory(plugin.name()),
            name: plugin.name().to_owned(),
            root: plugin.root,
            entries: plugin.manifest.mcp_servers,
        })
        .collect();
    (plugins, Vec::new())
}

/// Every server an Agent working in `directory` would have, from the settings and enabled plugins
/// in the config root `root`, with variables taken from `environment`.
pub fn configured_servers(
    root: &Path,
    directory: &Path,
    environment: &BTreeMap<String, String>,
) -> ResolvedServers {
    let (plugins, mut warnings) =
        enabled_plugin_servers(&tick_plugins::PluginStore::in_config_root(root));
    let mut resolved = crate::UserSettings::in_root(root)
        .load()
        .mcp_servers_with_plugins(directory, environment, &plugins);
    warnings.append(&mut resolved.warnings);
    resolved.warnings = warnings;
    resolved
}

/// What the configuration alone says of each server, before any Agent connects to it: a server
/// that can run is `Connecting`, as it is when an Agent starts.
pub async fn configured_statuses(
    root: &Path,
    servers: &[McpServerConfig],
) -> Vec<tickloop_harness::protocol::mcp::McpServerStatus> {
    use tickloop_harness::{
        connector::ConnectorCredentials,
        protocol::mcp::{McpServerState, McpServerStatus},
    };
    let mut statuses = Vec::new();
    for server in servers {
        let (transport, state, oauth) = match &server.transport {
            McpTransport::Stdio { .. } => ("stdio".to_owned(), McpServerState::Connecting, false),
            McpTransport::Http { headers, .. } => (
                "http".to_owned(),
                McpServerState::Connecting,
                !headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("authorization")),
            ),
            McpTransport::Unsupported { kind } => {
                (kind.clone(), McpServerState::Unsupported, false)
            }
            McpTransport::NeedsConfiguration => {
                (String::new(), McpServerState::NeedsConfiguration, false)
            }
            McpTransport::Failed { .. } => (String::new(), McpServerState::Failed, false),
        };
        let logged_in = match (&server.transport, oauth) {
            (McpTransport::Http { url, .. }, true) => {
                match crate::mcp_credentials::FileCredentials::new(
                    root,
                    &server.source,
                    &server.name,
                ) {
                    Ok(credentials) => {
                        credentials
                            .load()
                            .await
                            .ok()
                            .flatten()
                            .is_some_and(|saved| {
                                saved.token.is_some()
                                    && reqwest::Url::parse(url)
                                        .is_ok_and(|url| saved.server_url == url.as_str())
                            })
                    }
                    Err(_) => false,
                }
            }
            _ => false,
        };
        statuses.push(McpServerStatus {
            name: server.name.clone(),
            source: server.source.clone(),
            transport,
            state: if server.disabled {
                McpServerState::Disabled
            } else {
                state
            },
            exposure: server.exposure.as_str().into(),
            tools: Vec::new(),
            failure: match &server.transport {
                McpTransport::Failed { reason } => Some(reason.clone()),
                _ => None,
            },
            oauth,
            logged_in,
        });
    }
    statuses
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
    let project = &settings[PROJECTS_KEY][directory.to_string_lossy().as_ref()];
    for (source, layer) in [(GLOBAL_SOURCE, settings), (DIRECTORY_SOURCE, project)] {
        match layer.get(SERVERS_KEY) {
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
        if let Err(problem) = validate_user_server_name(name) {
            resolved.warnings.push(problem);
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
            let name = plugin_server_name(&plugin.name, name);
            if resolved.servers.iter().any(|server| server.name == name) {
                resolved.warnings.push(format!(
                    "MCP {name} skipped: normalized server name already exists"
                ));
                continue;
            }
            resolved.push(
                name.clone(),
                format!("{PLUGIN_SOURCE_PREFIX}{}", plugin.name),
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
    let disabled = disabled_servers(settings, directory);
    for server in &mut resolved.servers {
        server.disabled = disabled.contains(&server.name);
    }
    resolved
}

pub fn plugin_server_name(plugin: &str, server: &str) -> String {
    use tickloop_harness::protocol::mcp::normalize_mcp_name_component;
    format!(
        "{PLUGIN_SERVER_PREFIX}{}_{}",
        normalize_mcp_name_component(plugin),
        normalize_mcp_name_component(server)
    )
}

/// The names of user servers: letters, digits, `_` and `-`, without the prefix of plugin servers
/// or the name of the Artifacts server, whose resources are read under that name.
pub fn validate_user_server_name(name: &str) -> Result<(), String> {
    if name == ARTIFACTS_SERVER_NAME {
        return Err(format!(
            "MCP server name {ARTIFACTS_SERVER_NAME} is reserved for Tickloop Artifacts"
        ));
    }
    let valid = tickloop_harness::protocol::mcp::is_valid_mcp_server_name(name)
        && !name.starts_with(PLUGIN_SERVER_PREFIX);
    if valid {
        Ok(())
    } else {
        Err(
            "invalid user MCP server name (letters, digits, _ and - only; __ is not allowed; plugin_ is reserved)"
                .into(),
        )
    }
}

fn disabled_servers(settings: &Value, directory: &Path) -> Vec<String> {
    settings[PROJECTS_KEY][directory.to_string_lossy().as_ref()][DISABLED_SERVERS_KEY]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect()
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
        let plugin = source.starts_with(PLUGIN_SOURCE_PREFIX);
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

        let exposure = expanded_entry
            .get("exposure")
            .map(|value| serde_json::from_value::<ToolExposure>(value.clone()))
            .transpose();
        let tool_exposure = expanded_entry
            .get("toolExposure")
            .map(|value| {
                value.as_object().ok_or(()).and_then(|overrides| {
                    overrides
                        .iter()
                        .map(|(pattern, value)| {
                            serde_json::from_value::<ToolExposure>(value.clone())
                                .map(|exposure| (pattern.clone(), exposure))
                                .map_err(|_| ())
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
            })
            .transpose();
        if exposure.is_err() || tool_exposure.is_err() {
            transport = McpTransport::Failed {
                reason: "invalid tool exposure configuration".into(),
            };
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
            sensitive_values: Vec::new(),
            disabled: false,
            oauth,
            exposure: exposure.ok().flatten().unwrap_or_default(),
            tool_exposure: tool_exposure.ok().flatten().unwrap_or_default(),
            description: expanded_entry["description"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(2048)
                .collect(),
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

/// The Connector that logs the user in to `server` and out again, with its saved credential in
/// the config root `root`.
pub fn login_connector(
    root: &Path,
    server: &McpServerConfig,
) -> Result<tickloop_harness::connector::Connector, String> {
    let McpTransport::Http { url, headers } = &server.transport else {
        return Err("MCP login requires a configured streamable HTTP server".into());
    };
    if headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
    {
        return Err("This MCP server uses an Authorization header; manage that credential in its configuration instead of OAuth login or logout.".into());
    }
    let credentials = std::sync::Arc::new(
        crate::mcp_credentials::FileCredentials::new(root, &server.source, &server.name)
            .map_err(|error| error.to_string())?,
    );
    tickloop_harness::connector::Connector::new(
        url,
        tickloop_harness::connector::OAuthSettings {
            client_id: server.oauth.client_id.clone(),
            callback_port: server.oauth.callback_port,
            authorization_server_metadata_url: server
                .oauth
                .authorization_server_metadata_url
                .clone(),
        },
        credentials,
    )
    .map_err(|error| error.to_string())
}

// A resource URI may already contain escapes before it is encoded into the login URL.
const AUTHORIZATION_URL_ENCODING_LAYERS: usize = 2;

/// Refuses an authorization link that carries one of the server's sensitive plugin values, in
/// any of the encodings a link can give it, so it is never shown.
pub fn check_authorization_link(link: &str, sensitive_values: &[Value]) -> Result<(), String> {
    let secrets = sensitive_values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let mut candidates = std::collections::BTreeSet::from([link.to_owned()]);
    for _ in 0..=AUTHORIZATION_URL_ENCODING_LAYERS {
        if candidates
            .iter()
            .any(|candidate| secrets.iter().any(|secret| candidate.contains(secret)))
        {
            return Err("The authorization link contains sensitive plugin configuration and cannot be displayed; OAuth client identifiers and resource addresses must be public.".into());
        }
        let mut next = std::collections::BTreeSet::new();
        for candidate in &candidates {
            for encoded in [candidate.clone(), candidate.replace('+', " ")] {
                next.insert(
                    percent_encoding::percent_decode_str(&encoded)
                        .decode_utf8_lossy()
                        .into_owned(),
                );
            }
        }
        if next == candidates {
            break;
        }
        candidates = next;
    }
    Ok(())
}
