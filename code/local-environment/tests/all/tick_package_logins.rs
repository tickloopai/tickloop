//! The login state of each installed Tick package, as every model request sees it in
//! `workspace_host.plugins`; reading it from the saved credentials is covered by
//! `tick/packages`.

use tickloop_harness::runtime::host_tools::WorkspacePluginLogin;

use std::{fs, os::unix::fs::PermissionsExt, path::Path, sync::Arc};
use tickloop_code_settings::local_agent::LocalAgent;

use serde_json::{Value, json};
use tick_packages::{ApiKey, MANIFEST_FILE_NAME, OAuthToken};
use tick_plugins::PluginStore;
use tickloop_code_local_environment::hosted::{
    tools::HostedTickTools,
    worker_control::{DaemonControl, WorkerControlGrant},
};
use tickloop_harness::policy::skills::TICKLOOP_SKILL;
use tickloop_harness::protocol::{Content, EventKind, Role};
use tickloop_harness::runtime::{Agent, host_tools::HostToolProvider};
use tickloop_harness::test_support::{ScriptedModel, config, next_event, text};

/// Hosted Tick tools whose daemon is never reached: reading login states asks nothing of it.
fn tools(directory: &Path, packages: PluginStore) -> HostedTickTools {
    HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.join("unused-daemon.sock"),
            daemon_epoch: uuid::Uuid::new_v4(),
            binding: uuid::Uuid::new_v4(),
            capability: "unused".into(),
            host_endpoint: directory.join("unused-host.sock"),
        }),
        Vec::new(),
    )
    .with_plugins(packages)
}

/// Installs the package `name` that authenticates by `auth`, whose program records each run in
/// `runs` beside the sources.
fn install(directory: &Path, packages: &PluginStore, name: &str, auth: Value) {
    let source = directory.join("sources").join(name);
    fs::create_dir_all(source.join("bin")).unwrap();
    let manifest = json!({
        "name": name,
        "version": "1.0.0",
        "description": "A sample data source",
        "executable": format!("bin/tick-{name}"),
        "protocol": true,
        "auth": auth,
    });
    fs::write(source.join(MANIFEST_FILE_NAME), manifest.to_string()).unwrap();
    let program = source.join("bin").join(format!("tick-{name}"));
    fs::write(
        &program,
        format!(
            "#!/bin/sh\necho run >> '{}'\n",
            directory.join("runs").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    packages.install_path(&source).unwrap();
}

fn oauth2() -> Value {
    json!({"type": "oauth2", "issuers": [{"label": "Server", "issuer": "https://auth.example.com"}]})
}

fn token(expires_in: i64, refresh_token: Option<&str>) -> OAuthToken {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    OAuthToken {
        access_token: "access".into(),
        token_type: "Bearer".into(),
        refresh_token: refresh_token.map(str::to_owned),
        scope: None,
        issuer: "https://auth.example.com".into(),
        client_id: "client".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        revocation_endpoint: None,
        expires_at: now.saturating_add_signed(expires_in),
    }
}

fn login_of(tools: &HostedTickTools, name: &str) -> WorkspacePluginLogin {
    tools
        .workspace_host_context()
        .unwrap()
        .plugins
        .into_iter()
        .find(|package| package.name == name)
        .unwrap()
        .login
}

#[test]
fn a_package_login_state_follows_its_saved_credentials_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let packages = PluginStore::in_config_root(&cwd.join("config"));
    install(&cwd, &packages, "sample", oauth2());
    install(
        &cwd,
        &packages,
        "keyed",
        json!({"type": "api_key", "env": "KEYED_KEY", "label": "Keyed key"}),
    );
    install(&cwd, &packages, "plain", json!({"type": "none"}));
    let tools = tools(&cwd, packages.clone());
    let sample = packages.credentials("sample").unwrap();

    assert_eq!(login_of(&tools, "sample"), WorkspacePluginLogin::LoggedOut);
    assert_eq!(login_of(&tools, "keyed"), WorkspacePluginLogin::LoggedOut);
    assert_eq!(login_of(&tools, "plain"), WorkspacePluginLogin::NotRequired);
    for (saved, state) in [
        (token(600, None), WorkspacePluginLogin::LoggedIn),
        (token(-600, Some("refresh")), WorkspacePluginLogin::LoggedIn),
        (token(-600, None), WorkspacePluginLogin::Expired),
    ] {
        sample.lock_oauth().unwrap().save_token(&saved).unwrap();
        assert_eq!(login_of(&tools, "sample"), state, "{saved:?}");
    }
    fs::write(sample.oauth_token_path(), "{").unwrap();
    assert_eq!(login_of(&tools, "sample"), WorkspacePluginLogin::Unknown);
    sample.lock_oauth().unwrap().remove_token().unwrap();
    assert_eq!(login_of(&tools, "sample"), WorkspacePluginLogin::LoggedOut);

    packages
        .credentials("keyed")
        .unwrap()
        .save_api_key(&ApiKey::new("key").unwrap())
        .unwrap();
    assert_eq!(login_of(&tools, "keyed"), WorkspacePluginLogin::LoggedIn);
    assert!(
        !cwd.join("runs").exists(),
        "reading login states runs no package program"
    );
}

#[tokio::test]
async fn each_model_request_sees_the_login_states_with_the_fields_the_tickloop_skill_names() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let packages = PluginStore::in_config_root(&cwd.join("config"));
    install(&cwd, &packages, "sample", oauth2());
    packages
        .credentials("sample")
        .unwrap()
        .lock_oauth()
        .unwrap()
        .save_token(&token(600, None))
        .unwrap();
    install(&cwd, &packages, "plain", json!({"type": "none"}));
    let model = Arc::new(ScriptedModel::new(vec![text("Ready.")]));
    let agent = Agent::open(cwd.join("state"), config(), model.clone()).unwrap();
    agent.trust_working_directory(cwd.clone()).await.unwrap();
    agent
        .install_host_tools(Arc::new(tools(&cwd, packages)))
        .unwrap();
    let chat = agent.create_chat(cwd.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Check quotes".into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;

    let requests = model.requests.lock().unwrap();
    let system = &requests[0].messages[0];
    assert_eq!(system.role, Role::System);
    let Content::Text { text } = &system.content[0] else {
        panic!("the first model message must be system text");
    };
    let state: Value =
        serde_json::from_str(text.rsplit_once("Current host state: ").unwrap().1).unwrap();
    let listed = &state["workspace_host"]["plugins"];
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|package| (package["name"].clone(), package["login"].clone()))
            .collect::<Vec<_>>(),
        [
            (json!("plain"), json!("not_required")),
            (json!("sample"), json!("logged_in")),
        ]
    );
    for field in listed[0].as_object().unwrap().keys() {
        assert!(
            TICKLOOP_SKILL.contains(&format!("`{field}`")),
            "the tickloop Skill names the package field {field}"
        );
    }
    for login in [
        WorkspacePluginLogin::LoggedIn,
        WorkspacePluginLogin::LoggedOut,
        WorkspacePluginLogin::Expired,
        WorkspacePluginLogin::NotRequired,
        WorkspacePluginLogin::Unknown,
    ] {
        let login = serde_json::to_value(login).unwrap();
        assert!(
            TICKLOOP_SKILL.contains(&format!("`{}`", login.as_str().unwrap())),
            "the tickloop Skill names the login state {login}"
        );
    }
}

#[test]
fn the_model_sees_disabled_and_skill_only_plugins_without_running_them() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let store = PluginStore::in_config_root(&root.join("config"));
    install(root, &store, "sample", json!({"type":"none"}));
    store.set_enabled("sample", false).unwrap();
    let notes = root.join("notes");
    fs::create_dir_all(notes.join(".claude-plugin")).unwrap();
    fs::write(
        notes.join(".claude-plugin/plugin.json"),
        r#"{"name":"notes","version":"2.0.0"}"#,
    )
    .unwrap();
    store.install_path(&notes).unwrap();
    let context = tools(root, store).workspace_host_context().unwrap();
    assert_eq!(
        context
            .plugins
            .iter()
            .map(|plugin| (plugin.id.as_str(), plugin.enabled, plugin.has_tick))
            .collect::<Vec<_>>(),
        [("notes@local", true, false), ("sample@local", false, true)]
    );
    assert!(context.plugins[0].executable.is_none());
    assert!(!root.join("runs").exists());
}
