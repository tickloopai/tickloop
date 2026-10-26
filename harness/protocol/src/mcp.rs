use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// A host-resolved configuration. Secrets are deliberately excluded from Debug output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub source: String,
    pub transport: McpTransport,
    pub timeout_ms: u64,
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
