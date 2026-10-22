use std::sync::Arc;

use tickloop_agent_runtime::{Agent, upgrade::AgentUpgradeCheckpoint};
use tickloop_agent_test_support::{ScriptedModel, config};

#[tokio::test]
async fn an_old_checkpoint_without_stopped_runs_restores_and_releases() {
    let directory = tempfile::tempdir().unwrap();
    let predecessor = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let chat = predecessor
        .create_chat(directory.path().into())
        .await
        .unwrap();
    let checkpoint = predecessor.prepare_upgrade().await.unwrap().unwrap();
    let mut old = serde_json::to_value(checkpoint).unwrap();
    old["family_mailbox"]
        .as_object_mut()
        .unwrap()
        .remove("stopped_runs");
    let checkpoint: AgentUpgradeCheckpoint = serde_json::from_value(old).unwrap();
    let successor = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    successor.restore_upgrade(checkpoint).await.unwrap();
    successor.activate_upgrade().await.unwrap();
    successor.release_upgrade().await.unwrap();
    assert_eq!(successor.snapshot(chat).await.unwrap().id, chat);
    predecessor.cancel_upgrade().await;
    predecessor.shutdown().await;
    successor.shutdown().await;
}
