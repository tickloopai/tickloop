use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};
use tickloop_harness::protocol::mcp::{McpServerConfig, McpTransport};

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
        resolve_servers(
            &Value::Object(self.settings.clone()),
            directory,
            environment,
        )
    }
}

pub fn resolve_servers(
    settings: &Value,
    directory: &Path,
    environment: &BTreeMap<String, String>,
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
        let transport = parse_transport(entry, directory, environment)
            .unwrap_or_else(|reason| McpTransport::Failed { reason });
        match &transport {
            McpTransport::Failed { reason } => {
                resolved.warnings.push(format!("MCP {name}: {reason}"))
            }
            McpTransport::Unsupported { .. } => resolved
                .warnings
                .push(format!("MCP {name}: transport is not supported yet")),
            _ => {}
        }
        resolved.servers.push(McpServerConfig {
            name: name.clone(),
            source: source.into(),
            transport,
            timeout_ms: entry["timeout"]
                .as_u64()
                .filter(|n| *n >= 1000)
                .unwrap_or(300_000),
        });
    }
    resolved
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
