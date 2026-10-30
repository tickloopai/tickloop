use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

/// A host-resolved configuration. Secrets are deliberately excluded from Debug output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub source: String,
    pub transport: McpTransport,
    pub timeout_ms: u64,
    #[serde(default)]
    pub exposure: ToolExposure,
    #[serde(default)]
    pub tool_exposure: Vec<(String, ToolExposure)>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub oauth: McpOAuthConfig,
    /// Host-provided secrets to remove from every server response.
    #[serde(default)]
    pub sensitive_values: Vec<serde_json::Value>,
    /// The user turned the server off where the Agent works: it stays listed but never runs.
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposure {
    #[default]
    Search,
    Direct,
    Hidden,
}

impl ToolExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Direct => "direct",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Connecting,
    Connected,
    Failed,
    Unsupported,
    NeedsLogin,
    NeedsConfiguration,
    Disabled,
}

/// What a View shows of one of the Agent's MCP Servers. It carries no secret: tool descriptions
/// and the failure are redacted like everything else the server sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    /// `global`, `directory` or `plugin:<plugin>`.
    pub source: String,
    /// `stdio`, `http`, or the unsupported type the configuration names.
    pub transport: String,
    pub state: McpServerState,
    /// How the server's tools reach the model.
    pub exposure: String,
    pub tools: Vec<McpToolSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Whether the user logs in to the server with OAuth, and whether a login is saved.
    pub oauth: bool,
    pub logged_in: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolSummary {
    pub name: String,
    pub description: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: PathBuf,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
    Unsupported {
        kind: String,
    },
    NeedsConfiguration,
    Failed {
        reason: String,
    },
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub callback_port: Option<u16>,
    #[serde(rename = "authServerMetadataUrl")]
    pub authorization_server_metadata_url: Option<String>,
}

/// A resolved service name cannot contain the separator used in model tool names.
pub fn is_valid_mcp_server_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("__")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Normalizes one name component, preserving the separators added by the tool naming layer.
pub fn normalize_mcp_name_component(name: &str) -> String {
    let mut normalized = String::new();
    for character in name.chars() {
        let character = if character.is_ascii_alphanumeric() || character == '-' {
            character
        } else {
            '_'
        };
        if character != '_' || !normalized.ends_with('_') {
            normalized.push(character);
        }
    }
    let normalized = normalized.trim_matches('_');
    if normalized.is_empty() {
        "unnamed".into()
    } else {
        normalized.into()
    }
}

// Private, session-scoped MCP stdio launch descriptors. Never place these in history or events.
pub const MAX_SESSION_MCP_SERVERS: usize = 8;
pub const MAX_SESSION_MCP_DESCRIPTOR_BYTES: usize = 128 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMcpServer {
    pub name: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

// Even names and paths may contain user data. Debug never emits descriptor-derived values.
impl fmt::Debug for SessionMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionMcpServer([redacted])")
    }
}

/// Pure validation: does not resolve programs, read credentials, or start a process.
pub fn validate_session_mcp_servers(servers: &[SessionMcpServer]) -> Result<(), String> {
    if servers.len() > MAX_SESSION_MCP_SERVERS {
        return Err("too many session MCP servers".into());
    }
    let mut names = BTreeSet::new();
    for server in servers {
        if server.name.is_empty()
            || server.name.len() > 64
            || server.name.contains("__")
            || !server
                .name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            || server.name == "tickloop-artifacts"
            || !names.insert(&server.name)
        {
            return Err("invalid, reserved or duplicate session MCP server name".into());
        }
        if !server.program.is_absolute()
            || !server.cwd.is_absolute()
            || server.program.as_os_str().len() > 4096
            || server.cwd.as_os_str().len() > 4096
            || server.program.as_os_str().as_encoded_bytes().contains(&0)
            || server.cwd.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err("session MCP program and cwd must be absolute paths".into());
        }
        if server.args.len() > 128
            || server
                .args
                .iter()
                .any(|v| v.len() > 16 * 1024 || v.contains('\0'))
            || server.env.len() > 64
            || server.env.iter().any(|(key, value)| {
                !valid_session_mcp_env_key(key) || value.len() > 16 * 1024 || value.contains('\0')
            })
        {
            return Err("invalid or oversized session MCP arguments or environment".into());
        }
    }
    if serde_json::to_vec(servers)
        .map_err(|_| "invalid session MCP descriptors")?
        .len()
        > MAX_SESSION_MCP_DESCRIPTOR_BYTES
    {
        return Err("session MCP descriptors exceed the size limit".into());
    }
    Ok(())
}

pub fn valid_session_mcp_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        && !["LD_", "DYLD_", "TICKLOOP_", "TICKHOST_"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        && !matches!(
            upper.as_str(),
            "BASH_ENV"
                | "ENV"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "RUBYOPT"
                | "PERL5OPT"
                | "JAVA_TOOL_OPTIONS"
                | "JDK_JAVA_OPTIONS"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn descriptor() -> SessionMcpServer {
        SessionMcpServer {
            name: "demo".into(),
            program: "/bin/demo".into(),
            args: vec!["private-arg".into()],
            env: BTreeMap::from([("KEY".into(), "private-value".into())]),
            cwd: "/tmp".into(),
        }
    }
    #[test]
    fn descriptors_debug_is_fully_redacted() {
        assert_eq!(
            format!("{:?}", descriptor()),
            "SessionMcpServer([redacted])"
        );
    }
    #[test]
    fn descriptors_are_bounded_and_environment_is_not_a_control_channel() {
        let server = descriptor();
        assert!(validate_session_mcp_servers(std::slice::from_ref(&server)).is_ok());
        assert!(validate_session_mcp_servers(&[server.clone(), server]).is_err());
        for key in [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "TICKLOOP_CONTROL_FD",
            "NODE_OPTIONS",
            "BASH_ENV",
        ] {
            assert!(!valid_session_mcp_env_key(key));
        }
        assert!(valid_session_mcp_env_key("EXPLICIT_API_KEY"));
        let mut ambiguous = descriptor();
        ambiguous.name = "a__b".into();
        assert!(validate_session_mcp_servers(&[ambiguous]).is_err());
    }
}
