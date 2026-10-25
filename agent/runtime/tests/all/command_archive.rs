use std::sync::Arc;
use tickloop_agent_protocol::{CommandState, EventKind};
use tickloop_agent_runtime::Agent;
use tickloop_agent_settings::local_agent::LocalAgent;
use tickloop_agent_test_support::*;

#[tokio::test]
async fn command_output_is_readable_only_in_the_process_that_ran_it() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let agent = Agent::open_embedded(
        state.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "Bash",
                "Bash",
                serde_json::json!({"command":"printf 'saved-command-output'"}),
            ),
            text("output recorded"),
        ])),
    )
    .unwrap();
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Run the command".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let operation = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(command) = agent
                .snapshot(chat)
                .await
                .unwrap()
                .commands
                .iter()
                .find(|command| command.state == CommandState::Completed)
            {
                break command.operation_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let result = agent.read_command(chat, operation, 6, 7).await.unwrap();
    let result: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(result["output"], "command");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["state"], "Completed");
    assert!(
        agent
            .read_command(chat, operation, 0, 100_000)
            .await
            .is_err()
    );
    agent.shutdown().await;
    drop(agent);
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let restored = Agent::open_embedded(state, config(), model.clone()).unwrap();
    restored.install_own_tickhost();
    restored.resume_chat(chat).await.unwrap();
    assert!(restored.commands(chat).await.unwrap().is_empty());
    assert!(restored.read_command(chat, operation, 6, 7).await.is_err());
    assert!(restored.stop_command(chat, operation).await.is_err());
    assert!(model.requests.lock().unwrap().is_empty());
    restored.shutdown().await;
}
