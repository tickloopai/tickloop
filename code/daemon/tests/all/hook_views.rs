//! What views get of the hooks a Folder has: never a plugin's sensitive configuration values.

use std::sync::Arc;

use serde_json::json;
use tick_plugins::PluginStore;
use tickloop_code_access::{AgentAccess, LaunchSpec, ObservationRequest, ObservationValue};
use tickloop_code_settings::plugin_config::PluginUserConfig;
use tickloop_harness::test_support::{ScriptedModel, harness::ChosenModels};

const HOOK_SECRET_SENTINEL: &str = "HOOK_SECRET_SENTINEL";

#[tokio::test]
async fn the_hooks_a_view_reviews_show_no_sensitive_plugin_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let source = root.join("source");
    std::fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(source.join("hooks")).unwrap();
    std::fs::write(
        source.join(".claude-plugin/plugin.json"),
        json!({
            "name": "guard",
            "userConfig": {"token": {"type": "string", "title": "Token", "sensitive": true}},
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        source.join("hooks/hooks.json"),
        json!({"hooks": {"PreToolUse": [{"hooks": [
            {"type": "command", "command": "check --token ${user_config.token}"}
        ]}]}})
        .to_string(),
    )
    .unwrap();
    let plugin = PluginStore::in_config_root(&root)
        .install_path(&source)
        .unwrap()
        .plugin;
    PluginUserConfig::load(&root, &plugin.manifest)
        .unwrap()
        .save(
            &root,
            &plugin.manifest,
            &[("token".to_owned(), json!(HOOK_SECRET_SENTINEL))].into(),
        )
        .unwrap();

    let mut launch = LaunchSpec::embedded(root.clone());
    launch.config_root = Some(root.clone());
    launch.model = Some(tickloop_harness::test_support::config());
    launch.session_persistence = false;
    let (agent, _) = tickloop_code_daemon::build_runtime_with_models(
        &launch,
        ChosenModels::new(Arc::new(ScriptedModel::new(Vec::new()))),
        None,
    )
    .await
    .unwrap();
    let access = tickloop_code_daemon::EmbeddedAgentAccess::new(agent.clone());

    let observation = access
        .observe(ObservationRequest::Hooks {
            launch: Box::new(launch.clone()),
        })
        .await
        .unwrap();
    let ObservationValue::Hooks(list) = observation.initial else {
        panic!("a hook list answers");
    };
    let [hook] = list.hooks.as_slice() else {
        panic!("the plugin's hook is listed: {list:?}");
    };
    assert_eq!(
        hook.resolved_command.as_deref(),
        Some("check --token [redacted]")
    );
    assert!(hook.sensitive_values.is_empty());
    let shown = serde_json::to_string(&list).unwrap();
    assert!(!shown.contains(HOOK_SECRET_SENTINEL), "{shown}");
    agent.shutdown().await;
}
