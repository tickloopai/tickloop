use std::sync::Arc;

use serde_json::json;
use tickloop_code_settings::UserSettings;
use tickloop_harness::protocol::{AgentHostOptions, EventKind};
use tickloop_harness::runtime::{
    Agent, HarnessOptions, HarnessSeams, environment::FolderEnvironment,
};
use tickloop_harness::test_support::{
    ScriptedModel, call, config, harness::ChosenModels, next_event, text,
};

#[tokio::test]
async fn user_settings_additional_directories_allow_the_first_write() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("project");
    let shared = temp.path().join("shared");
    let root = temp.path().join("config");
    for path in [&folder, &shared, &root] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(
        root.join("settings.json"),
        serde_json::to_vec(&json!({
            "defaultMode": "acceptEdits",
            "permissions": {"additionalDirectories": ["../shared"]}
        }))
        .unwrap(),
    )
    .unwrap();
    let target = shared.join("report.txt");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "write",
            "Write",
            json!({"file_path": target, "content": "shared report"}),
        ),
        text("Finished."),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            models: ChosenModels::new(model),
            environment: Some(Arc::new(FolderEnvironment::bare())),
            transcripts: None,
            search: None,
        },
        HarnessOptions {
            settings: Some(Arc::new(UserSettings::in_root(&root))),
            host: AgentHostOptions::embedded_defaults(),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    let chat = agent.create_chat(folder).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Write the report".into()).await.unwrap();
    let mut prompts = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                prompts.push(request.name);
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } | EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(
        prompts.is_empty(),
        "unexpected approval requests: {prompts:?}"
    );
    assert_eq!(std::fs::read_to_string(target).unwrap(), "shared report");
    agent.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn additional_directories_preserve_modes_rules_sensitive_paths_and_symlink_boundaries() {
    use tickloop_harness::protocol::{Decision, ExecutionMode, PermissionRule};
    use tickloop_harness::test_support::auto_classification;

    for mode in [
        ExecutionMode::AcceptEdits,
        ExecutionMode::Auto,
        ExecutionMode::Plan,
    ] {
        for scenario in ["ordinary", "ask", "deny", "sensitive", "escape"] {
            let temp = tempfile::tempdir().unwrap();
            let folder = temp.path().join("project");
            let shared = temp.path().join("shared");
            let outside = temp.path().join("outside");
            let root = temp.path().join("config");
            for path in [&folder, &shared, &outside, &root] {
                std::fs::create_dir(path).unwrap();
            }
            std::fs::create_dir_all(shared.join(".git/hooks")).unwrap();
            std::os::unix::fs::symlink(&outside, shared.join("escape")).unwrap();
            std::fs::write(
                root.join("settings.json"),
                serde_json::to_vec(&json!({
                    "permissions": {"additionalDirectories": ["../shared"]}
                }))
                .unwrap(),
            )
            .unwrap();
            let target = shared.join(match scenario {
                "sensitive" => ".git/hooks/pre-commit",
                "escape" => "escape/report.txt",
                _ => "report.txt",
            });
            let model = Arc::new(ScriptedModel::new(vec![
                call(
                    "write",
                    "Write",
                    json!({"file_path": target, "content": "authorized report"}),
                ),
                text("Finished."),
            ]));
            let reviewer = Arc::new(ScriptedModel::new(vec![text(
                &auto_classification(false).to_string(),
            )]));
            let agent = Agent::with_seams(
                HarnessSeams {
                    models: ChosenModels::new(model),
                    environment: Some(Arc::new(FolderEnvironment::bare())),
                    transcripts: None,
                    search: None,
                },
                HarnessOptions {
                    settings: Some(Arc::new(UserSettings::in_root(&root))),
                    host: AgentHostOptions::embedded_defaults(),
                    ..HarnessOptions::local(config())
                },
            )
            .unwrap();
            agent.set_auto_reviewer(reviewer.clone()).await;
            let chat = agent.create_chat(folder).await.unwrap();
            agent.set_execution_mode(chat, mode).await.unwrap();
            if matches!(scenario, "ask" | "deny") {
                agent
                    .set_permission_rules(
                        chat,
                        vec![PermissionRule {
                            id: uuid::Uuid::new_v4(),
                            tool: "Write".into(),
                            decision: if scenario == "ask" {
                                Decision::Ask
                            } else {
                                Decision::Deny
                            },
                            specifier: None,
                            input: None,
                            loop_id: None,
                            persistent: false,
                        }],
                    )
                    .await
                    .unwrap();
            }
            let mut events = agent.subscribe();
            let receipt = agent.submit(chat, "Write the report".into()).await.unwrap();
            let mut prompts = 0;
            loop {
                let event = next_event(&mut events).await;
                if event.run_id != receipt.run_id {
                    continue;
                }
                match event.kind {
                    EventKind::PermissionRequested { request } => {
                        prompts += 1;
                        agent
                            .answer_permission(chat, request.id, false)
                            .await
                            .unwrap();
                    }
                    EventKind::Completed { .. } | EventKind::Interrupted => break,
                    EventKind::Failed(error) => panic!("{mode:?}/{scenario}: {error}"),
                    _ => {}
                }
            }
            let asks = mode != ExecutionMode::Plan
                && (scenario == "ask"
                    || mode == ExecutionMode::AcceptEdits
                        && matches!(scenario, "sensitive" | "escape"));
            let reviewed =
                mode == ExecutionMode::Auto && matches!(scenario, "sensitive" | "escape");
            let written = mode != ExecutionMode::Plan && scenario == "ordinary" || reviewed;
            assert_eq!(prompts, usize::from(asks), "{mode:?}/{scenario}");
            assert_eq!(
                !reviewer.requests.lock().unwrap().is_empty(),
                reviewed,
                "{mode:?}/{scenario}"
            );
            assert_eq!(target.exists(), written, "{mode:?}/{scenario}");
            agent.shutdown().await;
        }
    }
}

#[tokio::test]
async fn launch_directories_are_chat_local_and_directory_sources_survive_session_changes() {
    use tickloop_harness::protocol::{AdditionalDirectorySource, PermissionChange};
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("project");
    let shared = temp.path().join("shared");
    let launch = temp.path().join("launch");
    let session = temp.path().join("session");
    let root = temp.path().join("config");
    for path in [&folder, &shared, &launch, &session, &root] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(
        root.join("settings.json"),
        serde_json::to_vec(&json!({
            "permissions": {"additionalDirectories": ["../shared"]}
        }))
        .unwrap(),
    )
    .unwrap();
    let agent = Agent::with_seams(
        HarnessSeams {
            models: ChosenModels::new(Arc::new(ScriptedModel::new(vec![]))),
            environment: Some(Arc::new(FolderEnvironment::bare())),
            transcripts: None,
            search: None,
        },
        HarnessOptions {
            settings: Some(Arc::new(UserSettings::in_root(&root))),
            host: AgentHostOptions::embedded_defaults(),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    let chat = agent.create_chat(folder.clone()).await.unwrap();
    agent
        .add_launch_directories(
            chat,
            vec![
                "../shared/.".into(),
                "../launch".into(),
                "../missing".into(),
            ],
        )
        .await
        .unwrap();
    assert!(
        agent
            .user_settings_warnings(chat)
            .iter()
            .any(|warning| warning.contains("missing") && warning.contains("skipped"))
    );
    let shared = shared.canonicalize().unwrap();
    let launch = launch.canonicalize().unwrap();
    let session = session.canonicalize().unwrap();
    agent
        .set_directories(
            chat,
            folder.clone(),
            vec![
                shared.clone(),
                launch.clone(),
                session.clone(),
                session.clone(),
            ],
        )
        .await
        .unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    assert_eq!(view.additional_directories.len(), 3);
    assert_eq!(
        view.additional_directory_sources[&shared],
        [
            AdditionalDirectorySource::UserSettings,
            AdditionalDirectorySource::LaunchArgument
        ]
        .into()
    );
    assert_eq!(
        view.additional_directory_sources[&session],
        [AdditionalDirectorySource::Session].into()
    );
    let removed = agent
        .apply_permission_change(
            chat,
            PermissionChange::RemoveDirectory {
                path: shared.clone(),
            },
            view.revision,
        )
        .await
        .unwrap();
    assert!(!removed.additional_directories.contains(&shared));
    assert!(!removed.additional_directory_sources.contains_key(&shared));
    assert!(
        !agent
            .permissions_view(chat)
            .await
            .unwrap()
            .additional_directories
            .contains(&shared)
    );
    let next = agent.create_chat(folder).await.unwrap();
    assert_eq!(
        agent
            .permissions_view(next)
            .await
            .unwrap()
            .additional_directories,
        vec![shared]
    );
    agent.shutdown().await;
}
