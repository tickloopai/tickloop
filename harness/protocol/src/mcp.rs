use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

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
