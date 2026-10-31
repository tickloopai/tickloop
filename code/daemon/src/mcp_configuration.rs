use std::os::unix::fs::MetadataExt;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use tickloop_code_access::LaunchSpec;
use tickloop_code_settings::{UserSettings, mcp_credentials::FileCredentials};
use tickloop_harness::{
    base::private_fs::PrivateDirectory,
    connector::ConnectorCredentials,
    protocol::mcp::{McpServerConfig, McpTransport},
    runtime::mcp::{HostCapabilities, McpConnections},
};

/// The MCP servers an Agent takes up, with a credential adapter for each remote server.
pub(crate) struct AgentServers {
    pub servers: Vec<McpServerConfig>,
    pub credentials: BTreeMap<String, Arc<dyn ConnectorCredentials>>,
    pub warnings: Vec<String>,
}

/// The environment `launch` starts its Agent with, which server configuration expands.
pub(crate) fn launch_environment(launch: &LaunchSpec) -> BTreeMap<String, String> {
    launch
        .environment
        .iter()
        .filter_map(|entry| {
            Some((
                String::from_utf8(entry.name.clone()).ok()?,
                String::from_utf8(entry.value.clone()).ok()?,
            ))
        })
        .collect()
}

/// Connections as an Agent of this process holds them: local servers may run, and their error
/// output goes to this process's log directory under `root`.
pub(crate) fn agent_connections(
    root: &Path,
    environment: BTreeMap<String, String>,
) -> McpConnections {
    let log_directory = PrivateDirectory::trusted(root.to_owned())
        .join("logs")
        .join("mcp")
        .join(std::process::id().to_string());
    McpConnections::new(
        HostCapabilities {
            local_processes: true,
            ..Default::default()
        },
        BTreeMap::new(),
        environment,
    )
    .with_stderr_directory(log_directory)
}

/// The servers `settings` and the enabled plugins of `plugins` give an Agent working in `folder`;
/// `paths` collects the plugin files they were read from.
pub(crate) fn agent_servers(
    settings: &UserSettings,
    plugins: &tick_plugins::PluginStore,
    folder: &Path,
    environment: &BTreeMap<String, String>,
    paths: &mut Vec<PathBuf>,
) -> AgentServers {
    let (plugin_servers, mut warnings) = crate::enabled_plugin_mcp_servers(plugins, paths);
    let mut resolved =
        settings
            .load()
            .mcp_servers_with_plugins(folder, environment, &plugin_servers);
    let mut credentials = BTreeMap::new();
    if let Some(root) = settings.root() {
        for server in &mut resolved.servers {
            if !matches!(server.transport, McpTransport::Http { .. }) {
                continue;
            }
            match FileCredentials::new(root, &server.source, &server.name) {
                Ok(adapter) => {
                    credentials.insert(
                        server.name.clone(),
                        Arc::new(adapter) as Arc<dyn ConnectorCredentials>,
                    );
                }
                Err(error) => {
                    warnings.push(format!("MCP {}: {error}", server.name));
                    server.transport = McpTransport::Failed {
                        reason: error.to_string(),
                    };
                }
            }
        }
    }
    warnings.extend(resolved.warnings);
    AgentServers {
        servers: resolved.servers,
        credentials,
        warnings,
    }
}

#[derive(Default)]
pub(crate) struct ConfigurationFiles {
    pub paths: Vec<PathBuf>,
    previous: Option<Revision>,
}
#[derive(PartialEq, Eq)]
pub(crate) struct Revision(BTreeMap<PathBuf, Stamp>);
#[derive(PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
    mode: u32,
}
impl ConfigurationFiles {
    pub fn revision(&self, root: &Path) -> Revision {
        let mut paths = self.paths.clone();
        paths.extend([
            root.join("settings.json"),
            root.join("plugins/installed.json"),
        ]);
        credential_paths(&root.join("credentials"), 3, &mut paths);
        manifest_paths(&root.join("plugins/cache"), 3, &mut paths);
        Revision(
            paths
                .into_iter()
                .filter_map(|path| {
                    fs::symlink_metadata(&path).ok().map(|metadata| {
                        (
                            path,
                            Stamp {
                                modified: metadata.modified().ok(),
                                len: metadata.len(),
                                inode: metadata.ino(),
                                mode: metadata.mode(),
                            },
                        )
                    })
                })
                .collect(),
        )
    }
    pub fn unchanged(&self, root: &Path) -> bool {
        self.previous
            .as_ref()
            .is_some_and(|previous| *previous == self.revision(root))
    }
    pub fn remember(&mut self, root: &Path, before: Revision) {
        self.previous = (self.revision(root) == before).then_some(before);
    }
}
fn credential_paths(directory: &Path, depth: usize, paths: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                credential_paths(&entry.path(), depth - 1, paths);
            } else if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                paths.push(entry.path());
            }
        }
    }
}

fn manifest_paths(directory: &Path, depth: usize, paths: &mut Vec<PathBuf>) {
    if depth == 0 {
        paths.extend(
            [
                ".tickloop-plugin/plugin.json",
                ".claude-plugin/plugin.json",
                "tick-package.json",
                ".mcp.json",
            ]
            .map(|name| directory.join(name)),
        );
    } else if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                manifest_paths(&entry.path(), depth - 1, paths);
            }
        }
    }
}
