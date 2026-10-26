use std::{sync::Arc, time::Duration};
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::model::ModelRequest;
use tickloop_harness::protocol::{ChatId, EventKind, Receipt};
use tickloop_harness::runtime::{Agent, AgentError};
use tickloop_harness::test_support::{ScriptedModel, config, text};
async fn finish(core: &Agent, chat: ChatId) -> Receipt {
    let mut events = core.subscribe();
    let receipt = core.submit(chat, "authorized work".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id {
                match event.kind {
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
    })
    .await
    .unwrap();
    receipt
}
fn task_count(request: &ModelRequest) -> usize {
    request
        .tools
        .iter()
        .filter(|tool| {
            matches!(
                tool.name.as_str(),
                "TaskCreate" | "TaskGet" | "TaskList" | "TaskUpdate"
            )
        })
        .count()
}
#[tokio::test]
async fn externally_saved_setting_updates_loaded_members_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let settings = tickloop_code_settings::UserSettings::in_root(directory.path());
    let model = Arc::new(ScriptedModel::new(vec![text("done"); 2]));
    let core = Agent::new_with_user_settings_embedded(config(), model.clone(), settings.clone());
    let chat = core
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();
    settings.save_task_tools_enabled(true).unwrap();
    assert!(!core.status(chat).await.unwrap().task_tools_enabled);
    finish(&core, chat).await;
    assert!(core.status(chat).await.unwrap().task_tools_enabled);
    assert_eq!(task_count(&model.requests.lock().unwrap()[0]), 4);
    settings.save_task_tools_enabled(false).unwrap();
    finish(&core, chat).await;
    assert_eq!(task_count(&model.requests.lock().unwrap()[1]), 0);
    core.set_task_tools_enabled(chat, true).await.unwrap();
    core.shutdown().await;
    let reopened = Agent::new_with_user_settings_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![])),
        settings,
    );
    let chat = reopened
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();
    assert!(reopened.status(chat).await.unwrap().task_tools_enabled);
    reopened.shutdown().await;
}

#[tokio::test]
async fn explicit_setting_reads_refresh_only_the_target_configuration_without_running_work() {
    let target_root = tempfile::tempdir().unwrap();
    let other_root = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let target_settings = tickloop_code_settings::UserSettings::in_root(target_root.path());
    let other_settings = tickloop_code_settings::UserSettings::in_root(other_root.path());
    let model = Arc::new(ScriptedModel::new(vec![]));
    let target =
        Agent::new_with_user_settings_embedded(config(), model.clone(), target_settings.clone());
    let other =
        Agent::new_with_user_settings_embedded(config(), model.clone(), other_settings.clone());
    let target_chat = target.create_chat(workdir.path().into()).await.unwrap();
    let other_chat = other.create_chat(workdir.path().into()).await.unwrap();
    target_settings.save_task_tools_enabled(true).unwrap();
    other_settings.save_task_tools_enabled(false).unwrap();
    assert!(!target.status(target_chat).await.unwrap().task_tools_enabled);
    assert!(target.task_tools_setting(target_chat).await.unwrap());
    assert!(!other.task_tools_setting(other_chat).await.unwrap());
    assert!(target.status(target_chat).await.unwrap().task_tools_enabled);
    target_settings.save_task_tools_enabled(false).unwrap();
    other_settings.save_task_tools_enabled(true).unwrap();
    assert!(!target.task_tools_setting(target_chat).await.unwrap());
    assert!(other.task_tools_setting(other_chat).await.unwrap());
    assert!(matches!(
        target.task_tools_setting(other_chat).await,
        Err(AgentError::NotFound)
    ));
    let snapshot = target.snapshot(target_chat).await.unwrap();
    assert_eq!(snapshot.chat_loop.run_id, None);
    assert!(snapshot.messages.is_empty());
    assert!(snapshot.task_boards.is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
    target.shutdown().await;
    other.shutdown().await;
}
