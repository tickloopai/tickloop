//! Code-owned execution sandbox settings, read only from the user's settings file.
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::LoadedSettings;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SandboxSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub fail_if_unavailable: bool,
    #[serde(default)]
    pub filesystem: SandboxFilesystem,
    #[serde(default)]
    pub network: SandboxNetwork,
    pub auto_allow_bash_if_sandboxed: bool,
    pub allow_unsandboxed_commands: bool,
    pub excluded_commands: Vec<String>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            fail_if_unavailable: false,
            filesystem: SandboxFilesystem::default(),
            network: SandboxNetwork::default(),
            auto_allow_bash_if_sandboxed: true,
            allow_unsandboxed_commands: true,
            excluded_commands: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxNetwork {
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxFilesystem {
    #[serde(default)]
    pub allow_git_config: bool,
    #[serde(default)]
    pub allow_write: Vec<String>,
    #[serde(default)]
    pub deny_write: Vec<String>,
}

const DEFAULT_DENY_WRITE_PATHS: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ripgreprc",
    ".mcp.json",
    ".vscode",
    ".idea",
    ".git/hooks",
    ".git/config",
    ".tickloop",
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".claude/skills",
    ".claude/hooks",
    ".claude/commands",
    ".claude/agents",
    ".claude/rules",
    "AGENTS.md",
    "CLAUDE.md",
    "CLAUDE.local.md",
];

impl SandboxFilesystem {
    /// Built-in write protections, matched case-insensitively at every depth below writable roots.
    /// The execution adapter also preserves credential protections and the user's settings root.
    pub fn default_deny_write_paths(&self) -> impl Iterator<Item = &'static str> {
        DEFAULT_DENY_WRITE_PATHS
            .iter()
            .copied()
            .filter(|path| !self.allow_git_config || *path != ".git/config")
    }
}

impl LoadedSettings {
    pub fn sandbox(&mut self) -> Result<SandboxSettings, String> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        let Some(value) = self.settings.get("sandbox").cloned() else {
            return Ok(SandboxSettings::default());
        };
        if let Some(object) = value.as_object() {
            self.warn_unsupported_sandbox_fields(
                "sandbox",
                object,
                &[
                    "enabled",
                    "failIfUnavailable",
                    "filesystem",
                    "network",
                    "autoAllowBashIfSandboxed",
                    "allowUnsandboxedCommands",
                    "excludedCommands",
                ],
            );
            if let Some(filesystem) = object.get("filesystem").and_then(Value::as_object) {
                self.warn_unsupported_sandbox_fields(
                    "sandbox.filesystem",
                    filesystem,
                    &["allowWrite", "denyWrite", "allowGitConfig"],
                );
            }
            if let Some(network) = object.get("network").and_then(Value::as_object) {
                self.warn_unsupported_sandbox_fields(
                    "sandbox.network",
                    network,
                    &["allowedDomains"],
                );
            }
        }
        serde_json::from_value(value).map_err(|error| format!("sandbox: {error}"))
    }

    fn warn_unsupported_sandbox_fields(
        &mut self,
        prefix: &str,
        fields: &Map<String, Value>,
        supported: &[&str],
    ) {
        for name in fields
            .keys()
            .filter(|name| !supported.contains(&name.as_str()))
        {
            self.warn(format!("{prefix}.{name}: 暂不支持"));
        }
    }
}
