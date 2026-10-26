use super::*;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::{
    policy::settings::SettingsStore,
    protocol::{
        EventKind,
        mcp::{McpServerConfig, McpTransport},
    },
    runtime::{
        Agent as Runtime,
        mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
    },
    test_support::{ScriptedModel, call, config, mcp_service::McpService, next_event, text},
};

#[tokio::test]
async fn mcp_confirmation_choices_save_the_displayed_scope_in_user_settings() {
    for choice in 0..4 {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().canonicalize().unwrap();
        let root = project.join("settings");
        let settings = UserSettings::in_root(&root);
        let service = McpService::start().await;
        service.set_tools(json!([{"name":"read","inputSchema":{"type":"object","properties":{}}}]));
        service.set_result(json!({"content":[{"type":"text","text":"OK"}]}));
        let configs = vec![McpServerConfig {
            exposure: tickloop_harness::protocol::mcp::ToolExposure::Direct,
            tool_exposure: Vec::new(),
            description: String::new(),
            oauth: Default::default(),
            sensitive_values: Vec::new(),
            disabled: false,
            name: "review".into(),
            source: "global".into(),
            transport: McpTransport::Http {
                url: service.url(),
                headers: BTreeMap::new(),
            },
            timeout_ms: 300_000,
        }];
        let connections = McpConnections::new(
            HostCapabilities::default(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        connections.reconcile(configs.clone()).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while connections.snapshot()[0].state != McpServerState::Connected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let core = Runtime::new_with_user_settings_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("read", "mcp__review__read", json!({})),
                text("done"),
            ])),
            settings.clone(),
        );
        core.install_host_tools(Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )))
        .unwrap();
        let view = Agent::from_embedded(core.clone(), config());
        let chat = view.create_chat(project.clone()).await.unwrap();
        let console = Console::test_interactive_for_chat(chat);
        let mut menus = Menus::new(Some(settings.clone()), Some(root.clone()));
        let mut events = core.subscribe();
        core.submit(chat.into(), "Read the service".into())
            .await
            .unwrap();
        let request = loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => break request,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        };
        menus
            .permission(&console, chat, &request, "Agent".into(), true)
            .unwrap();
        let menu = console.menu_snapshot(request.id).unwrap().unwrap();
        assert_eq!(
            menu.options,
            [
                "Yes, once",
                "Always allow this tool in this directory",
                "Always allow this server in this directory",
                "No, and tell the Agent what to do differently (esc)"
            ]
        );
        menus
            .choose(&view, &console, request.id, Some(choice), None)
            .await
            .unwrap();
        loop {
            if let EventKind::ToolCompleted { result, .. } = next_event(&mut events).await.kind {
                assert_eq!(result.is_error, choice == 3);
                break;
            }
        }
        assert_eq!(service.calls().len(), usize::from(choice != 3));
        let saved = UserSettings::in_root(&root)
            .read(Some(&project))
            .project
            .rules;
        if choice == 1 || choice == 2 {
            assert_eq!(saved.len(), 1);
            assert_eq!(
                saved[0].tool,
                if choice == 1 {
                    "mcp__review__read"
                } else {
                    "mcp__review"
                }
            );
            assert!(
                saved[0].input.is_none() && saved[0].specifier.is_none() && saved[0].persistent
            );
            let disk: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("settings.json")).unwrap())
                    .unwrap();
            assert_eq!(
                disk["projects"][project.to_str().unwrap()]["permissions"]["rules"][0]["tool"],
                saved[0].tool
            );
        } else {
            assert!(saved.is_empty());
        }
        assert!(
            settings
                .read(Some(&project.join("other")))
                .project
                .rules
                .is_empty()
        );
        core.shutdown().await;
    }
}

#[tokio::test]
async fn nested_file_change_confirmation_shows_the_entire_parent_batch() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().canonicalize().unwrap();
    let root = project.join("settings");
    let settings = UserSettings::in_root(&root);
    let configs:Vec<McpServerConfig>=vec![serde_json::from_value(json!({"name":"unused","source":"global","transport":{"type":"unsupported","kind":"fixture"},"timeout_ms":1000})).unwrap()];
    let connections = McpConnections::new(
        HostCapabilities::default(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.reconcile(configs.clone()).await;
    let core = Runtime::new_with_user_settings_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "batch-context",
                "ExecuteTools",
                json!({"calls":[
                    {"name":"Write","arguments":{"file_path":project.join("note.txt"),"content":"first operation"}},
                    {"name":"SearchTools","arguments":{"queries":["second-operation-marker"]}}
                ]}),
            ),
            text("done"),
        ])),
        settings.clone(),
    );
    core.install_host_tools(Arc::new(McpToolProvider::new(
        connections,
        Arc::new(move || configs.clone()),
    )))
    .unwrap();
    let view = Agent::from_embedded(core.clone(), config());
    let chat = view.create_chat(project).await.unwrap();
    core.set_permission_rules(
        chat.into(),
        vec![tickloop_harness::protocol::PermissionRule {
            id: tickloop_harness::ids::generate(),
            decision: tickloop_harness::protocol::Decision::Ask,
            tool: "Write".into(),
            specifier: None,
            input: None,
            loop_id: None,
            persistent: false,
        }],
    )
    .await
    .unwrap();
    let console = Console::test_interactive_for_chat(chat);
    let mut menus = Menus::new(Some(settings), Some(root));
    let mut events = core.subscribe();
    core.submit(chat.into(), "Write the note".into())
        .await
        .unwrap();
    let request = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert!(request.file_change.is_some());
    menus
        .permission(&console, chat, &request, "Agent".into(), true)
        .unwrap();
    let menu = console.menu_snapshot(request.id).unwrap().unwrap();
    let context = menu.context.join("\n");
    assert!(context.contains("batch-context"));
    assert!(context.contains("second-operation-marker"));
    core.shutdown().await;
}
