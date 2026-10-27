//! `/mcp` before a New Agent starts: connects one server the Agent would connect, with the
//! Agent's configuration, credentials and connection code, lists its tools and closes the
//! connection. The Agent runs in a Worker process of its own, so it cannot take this connection
//! over; it connects again when it starts.

use tickloop_code_access::{AccessError, LaunchSpec};
use tickloop_code_settings::UserSettings;
use tickloop_harness::protocol::mcp::{McpServerState, McpServerStatus};

use crate::mcp_configuration::{agent_connections, agent_servers, launch_environment};

/// Why a connection that outlasted its wait is reported as failed, as the Agent words it.
const CONNECTION_TIMED_OUT: &str = "MCP connection timed out";

pub(crate) async fn connect(
    launch: &LaunchSpec,
    name: &str,
) -> Result<McpServerStatus, AccessError> {
    let root = launch
        .config_root
        .clone()
        .ok_or_else(|| AccessError::unavailable("MCP servers need a state directory"))?;
    let environment = launch_environment(launch);
    let (folder, server_name) = (launch.folder.clone(), name.to_owned());
    let resolved = {
        let (root, environment) = (root.clone(), environment.clone());
        tokio::task::spawn_blocking(move || {
            let settings = UserSettings::in_root(&root).with_home_directory(std::env::home_dir());
            let plugins = tick_plugins::PluginStore::in_config_root(&root);
            agent_servers(&settings, &plugins, &folder, &environment, &mut Vec::new())
        })
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?
    };
    let config = resolved
        .servers
        .into_iter()
        .find(|server| server.name == server_name)
        .ok_or_else(|| {
            AccessError::invalid(format!(
                "{server_name} is not configured or enabled in this directory"
            ))
        })?;
    let connections = agent_connections(&root, environment);
    connections.set_credentials(resolved.credentials);
    connections.reconcile(vec![config]).await;
    let status = connections.settled_status(name).await;
    connections.shutdown().await;
    let mut status =
        status.ok_or_else(|| AccessError::unavailable(format!("{name} did not start")))?;
    if status.state == McpServerState::Connecting {
        status.state = McpServerState::Failed;
        status.failure = Some(CONNECTION_TIMED_OUT.into());
    }
    Ok(status)
}
