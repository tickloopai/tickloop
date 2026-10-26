use std::{sync::Arc, time::Duration};
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_harness::protocol::{ChatId, CommandState, EventKind, ExecutionMode, Receipt};
use tickloop_harness::runtime::{Agent, upgrade::AgentUpgradeCheckpoint};
use tickloop_harness::test_support::{ScriptedModel, call, config, text};
use uuid::Uuid;
async fn complete(agent: &Agent, chat: ChatId, input: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, input.into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    receipt
}
async fn prepare(agent: &Agent) -> AgentUpgradeCheckpoint {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(checkpoint) = agent.prepare_upgrade().await.unwrap() {
                break checkpoint;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
async fn check_shell_transfer(exit_during_transfer: bool) {
    let directory = tempfile::tempdir().unwrap();
    let finish = directory.path().join("finish");
    let command = format!(
        "while [ ! -f '{}' ]; do sleep 0.05; done; printf transferred",
        finish.display()
    );
    let original = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "background",
                "Bash",
                serde_json::json!({"command":command,"run_in_background":true}),
            ),
            text("running"),
        ])),
    );
    let grants = Arc::new(tickloop_code_local_environment::hosted::shells::OwnTickhost::new());
    original.install_shell_grants(grants.clone());
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    complete(&original, chat, "run background work").await;
    let before = original.commands(chat).await.unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].state, CommandState::Background);
    let checkpoint = prepare(&original).await;
    if exit_during_transfer {
        std::fs::write(&finish, "finish").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while original.commands(chat).await.unwrap()[0].state != CommandState::Completed {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    let successor = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    successor.install_shell_grants(grants);
    successor.restore_upgrade(checkpoint).await.unwrap();
    let mut notifications = successor.subscribe_task_notifications();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    successor.release_upgrade().await.unwrap();
    let after = successor.commands(chat).await.unwrap();
    assert_eq!(after[0].pid, before[0].pid);
    assert_eq!(after[0].operation_id, before[0].operation_id);
    if !exit_during_transfer {
        std::fs::write(finish, "finish").unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = successor.commands(chat).await.unwrap();
            if current[0].state == CommandState::Completed {
                assert_eq!(
                    std::fs::read_to_string(&current[0].output_ref).unwrap(),
                    "transferred"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(notifications.try_recv().is_err());
    successor.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_shell_keeps_pid_output_and_stop_authority_across_worker_restore() {
    check_shell_transfer(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_exit_during_transfer_delivers_its_original_output_and_one_notification() {
    check_shell_transfer(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claimed_task_wait_survives_upgrade_and_finishes_in_the_same_run() {
    use tickloop_harness::test_support::routed::{
        RoutedModel, has_tool_result, history_text, tool_result,
    };
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(RoutedModel::new(|request| {
        if !has_tool_result(request, "create") {
            let mut calls = call(
                "create",
                "TaskCreate",
                serde_json::json!({
                    "mutation_id":Uuid::from_u128(901),"title":"Wait for output","description":"Complete when the Shell exits",
                }),
            );
            calls.extend(call(
                "background",
                "Bash",
                serde_json::json!({
                    "command":"while [ ! -f finish ]; do sleep 0.05; done; printf done",
                    "description":"Wait for finish marker", "run_in_background":true,
                }),
            ));
            return calls;
        }
        let task = tool_result(&request.messages, "create").unwrap()["task"]["id"].clone();
        if !has_tool_result(request, "claim") {
            return call(
                "claim",
                "TaskUpdate",
                serde_json::json!({
                    "mutation_id":Uuid::from_u128(902),"task_id":task,"expected_revision":1,"action":"claim",
                }),
            );
        }
        if !history_text(request).contains("<tool-use-id>background</tool-use-id>") {
            return text("Waiting for the background command");
        }
        if !has_tool_result(request, "complete") {
            return call(
                "complete",
                "TaskUpdate",
                serde_json::json!({
                    "mutation_id":Uuid::from_u128(903),"task_id":task,"expected_revision":2,"action":"complete","result":"Command completed",
                }),
            );
        }
        text("Task finished")
    }));
    let original = Agent::new_embedded(config(), model.clone());
    let grants = Arc::new(tickloop_code_local_environment::hosted::shells::OwnTickhost::new());
    original.install_shell_grants(grants.clone());
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original.set_task_tools_enabled(chat, true).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = original.subscribe();
    let receipt = original
        .submit(chat, "Track background work".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::Text(ref text) if text == "Waiting for the background command") {
                break;
            }
            if let EventKind::Failed(error) = event.kind { panic!("{error}"); }
        }
    }).await.unwrap();
    let checkpoint = prepare(&original).await;
    assert_eq!(
        serde_json::to_value(&checkpoint).unwrap()["chats"][0]["waiting"]["run_id"],
        serde_json::json!(receipt.run_id)
    );
    let requests = model.requests.lock().unwrap().len();
    let successor = Agent::new_embedded(config(), model.clone());
    successor.install_shell_grants(grants);
    successor
        .restore_upgrade(serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap())
        .await
        .unwrap();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    let mut events = successor.subscribe();
    successor.release_upgrade().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(model.requests.lock().unwrap().len(), requests);
    std::fs::write(directory.path().join("finish"), "go").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
            if let EventKind::Failed(error) = event.kind {
                panic!("{error}");
            }
        }
    })
    .await
    .unwrap();
    let snapshot = successor.snapshot(chat).await.unwrap();
    let task = &snapshot.task_boards[&receipt.loop_id].tasks[0];
    assert_eq!(
        task.status,
        tickloop_harness::protocol::SharedTaskStatus::Completed
    );
    assert_eq!(task.claim.as_ref().unwrap().run_id, receipt.run_id);
    successor.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_completed_shell_keeps_its_host_view_after_upgrade_and_idle_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let original = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "background",
                "Bash",
                serde_json::json!({
                    "command":"while [ ! -f finish ]; do sleep 0.01; done; printf retained",
                    "description":"Wait for marker", "run_in_background":true,
                }),
            ),
            text("running"),
        ])),
    );
    let grants = Arc::new(
        tickloop_code_local_environment::hosted::shells::OwnTickhost::with_idle_timeout(
            Duration::from_millis(20),
        ),
    );
    original.install_shell_grants(grants.clone());
    let chat = original.create_chat(directory.path().into()).await.unwrap();
    original
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    complete(&original, chat, "run background work").await;
    std::fs::write(directory.path().join("finish"), "finish").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if original
                .snapshot(chat)
                .await
                .unwrap()
                .commands
                .first()
                .is_some_and(|command| command.state == CommandState::Completed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let checkpoint = prepare(&original).await;
    let task = original.snapshot(chat).await.unwrap().commands[0].operation_id;
    let successor = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let shells = successor.install_shell_grants(grants);
    successor.restore_upgrade(checkpoint).await.unwrap();
    successor.activate_upgrade().await.unwrap();
    original.shutdown_for_upgrade().await;
    successor.release_upgrade().await.unwrap();
    tokio::time::sleep(Duration::from_millis(650)).await;
    let instance = tick_protocol::InstanceRef::shell(task);
    assert!(shells.allow_view(chat.into(), &instance).await.is_ok());
    successor.shutdown().await;
}
