//! `/context` before a New Agent's first message: what its first request would hold, counted in
//! a runtime that lives only for the count. That runtime installs no host tools, so it connects
//! no MCP server and starts no Tick; their tools join when the Agent starts. The MCP servers the
//! Agent would connect are listed from configuration, without their tools.

use std::collections::BTreeMap;
use std::sync::Arc;

use tickloop_code_access::{AccessError, LaunchSpec};
use tickloop_code_local_environment::environment::LocalEnvironment;
use tickloop_code_settings::UserSettings;
use tickloop_harness::model::{LocalModelClients, cards::launch_model};
use tickloop_harness::protocol::mcp::{McpServerState, McpTransport};
use tickloop_harness::protocol::{
    ContextBreakdown, ContextDraft, ToolSearchMode, ToolSearchPool, UnavailableToolServer,
};
use tickloop_harness::runtime::{Agent, HarnessOptions, HarnessSeams};

pub(crate) async fn breakdown(
    launch: &LaunchSpec,
    draft: ContextDraft,
) -> Result<ContextBreakdown, AccessError> {
    let settings = launch
        .config_root
        .as_ref()
        .map(|root| UserSettings::in_root(root).with_home_directory(std::env::home_dir()));
    let mode = if tickloop_harness::model::native_tool_search::offered(&draft.model).is_some() {
        ToolSearchMode::Native
    } else {
        ToolSearchMode::SearchTools
    };
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            // Bare: no Tickhost and no Shells, so counting starts nothing.
            environment: Some(Arc::new(LocalEnvironment::bare())),
            models: Arc::new(LocalModelClients::new(launch.config_root.clone())),
        },
        HarnessOptions {
            settings: settings.clone().map(|settings| {
                Arc::new(settings) as Arc<dyn tickloop_harness::policy::settings::SettingsStore>
            }),
            host: launch.host_options,
            ..HarnessOptions::local(launch_model(launch.model.as_ref()))
        },
    )
    .map_err(|error| AccessError::unavailable(error.to_string()))?;
    let counted = agent
        .draft_context_breakdown(Some(launch.folder.clone()), draft)
        .await;
    agent.shutdown().await;
    let mut breakdown = counted.map_err(|error| AccessError::invalid(error.to_string()))?;
    breakdown.host_tools_pending = true;
    let servers = settings
        .map(|settings| configured_servers(&settings, launch))
        .unwrap_or_default();
    if breakdown.tool_search.is_none() && !servers.is_empty() {
        breakdown.tool_search = Some(ToolSearchPool {
            mode,
            servers: Vec::new(),
            unavailable_servers: servers,
        });
    }
    Ok(breakdown)
}

/// The MCP servers an Agent started in `launch` would connect, as they stand before connecting.
fn configured_servers(settings: &UserSettings, launch: &LaunchSpec) -> Vec<UnavailableToolServer> {
    let Some(root) = settings.root() else {
        return Vec::new();
    };
    let environment = launch
        .environment
        .iter()
        .filter_map(|entry| {
            Some((
                String::from_utf8(entry.name.clone()).ok()?,
                String::from_utf8(entry.value.clone()).ok()?,
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let plugins = tick_plugins::PluginStore::in_config_root(root);
    let (plugin_servers, _) = crate::enabled_plugin_mcp_servers(&plugins, &mut Vec::new());
    settings
        .load()
        .mcp_servers_with_plugins(&launch.folder, &environment, &plugin_servers)
        .servers
        .into_iter()
        .filter(|server| !server.disabled)
        .map(|server| UnavailableToolServer {
            state: match server.transport {
                McpTransport::Failed { .. } | McpTransport::Unsupported { .. } => {
                    McpServerState::Failed
                }
                _ => McpServerState::Connecting,
            },
            name: server.name,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness::protocol::ExecutionMode;

    #[tokio::test]
    async fn a_new_agent_is_counted_without_starting_and_lists_the_servers_it_would_connect() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("settings.json"),
            r#"{"mcpServers":{"remote":{"type":"http","url":"https://mcp.example.com/mcp"}}}"#,
        )
        .unwrap();
        let mut launch = LaunchSpec::embedded(folder.path().into());
        launch.config_root = Some(root.path().into());
        launch.session_persistence = false;
        let model = launch_model(None);
        let breakdown = breakdown(
            &launch,
            ContextDraft {
                model,
                execution_mode: ExecutionMode::AcceptEdits,
                search_config: None,
                task_tools_enabled: false,
            },
        )
        .await
        .unwrap();
        assert!(breakdown.host_tools_pending);
        assert!(!breakdown.calibrated);
        assert!(breakdown.used_tokens > 0);
        let pool = breakdown.tool_search.unwrap();
        assert_eq!(pool.tool_count(), 0);
        assert_eq!(
            pool.unavailable_servers,
            [UnavailableToolServer {
                name: "remote".into(),
                state: McpServerState::Connecting,
            }]
        );
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "counting writes nothing beside the settings"
        );
    }
}
