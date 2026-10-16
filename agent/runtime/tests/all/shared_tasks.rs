use serde_json::json;
use std::{sync::Arc, time::Duration};
use tickloop_agent_protocol::{Decision, EventKind, ExecutionMode, OperationState, PermissionRule};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{
    call, config,
    routed::{RoutedModel, has_tool_result, state, tool_result},
    text,
};
use uuid::Uuid;

async fn run_assignment(deny_messages: bool) {
    let directory = tempfile::tempdir().unwrap();
    let create_id = Uuid::new_v4();
    let assign_id = Uuid::new_v4();
    let own_create_id = Uuid::new_v4();
    let claim_id = Uuid::new_v4();
    let complete_id = Uuid::new_v4();
    let model = Arc::new(RoutedModel::new(move |request| {
        let host = state(request);
        let tools = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(!tools.contains(&"TodoWrite"));
        if host["goal"]["goal"] == "new solo" {
            assert!(!tools.contains(&"TaskCreate"));
            return text("solo done");
        }

        if !host["delegation"].is_null() {
            assert_eq!(tools.contains(&"TaskList"), tools.contains(&"TaskCreate"));
            return text("helper done");
        }
        if !has_tool_result(request, "spawn") {
            assert!(tools.contains(&"TaskCreate"));
            return call(
                "spawn",
                "Subagent",
                json!({"name":"helper","description":"Inspect notes","task":"inspect notes"}),
            );
        }
        assert!(tools.contains(&"TaskCreate"));
        if !has_tool_result(request, "create") {
            return call(
                "create",
                "TaskCreate",
                json!({"mutation_id":create_id,"title":"Check","description":"Inspect authorized notes"}),
            );
        }
        let task = tool_result(&request.messages, "create").unwrap()["task"].clone();
        if !has_tool_result(request, "create-own") {
            return call(
                "create-own",
                "TaskCreate",
                json!({"mutation_id":own_create_id,"title":"Summarize","description":"Summarize the authorized checks"}),
            );
        }
        let own_task = tool_result(&request.messages, "create-own").unwrap()["task"].clone();
        if !has_tool_result(request, "claim-own") {
            return call(
                "claim-own",
                "TaskUpdate",
                json!({"mutation_id":claim_id,"task_id":own_task["id"],"expected_revision":1,"action":"claim"}),
            );
        }
        let assignment = json!({"mutation_id":assign_id,"task_id":task["id"],"expected_revision":1,"action":"assign","owner":"helper"});
        if !has_tool_result(request, "assign") {
            return call("assign", "TaskUpdate", assignment);
        }
        if !has_tool_result(request, "retry") {
            return call("retry", "TaskUpdate", assignment);
        }
        if !has_tool_result(request, "complete-own") {
            return call(
                "complete-own",
                "TaskUpdate",
                json!({"mutation_id":complete_id,"task_id":own_task["id"],"expected_revision":2,"action":"complete","result":"Checks summarized"}),
            );
        }
        text("root done")
    }));
    let core =
        Agent::open_embedded(directory.path().join("store"), config(), model.clone()).unwrap();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_task_tools_enabled(root, true).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::FullAccess)
        .await
        .unwrap();
    if deny_messages {
        core.set_permission_rules(
            root,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Deny,
                tool: "SendMessage".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    }
    let mut events = core.subscribe();
    let receipt = core.submit(root, None, "coordinate".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == root && event.run_id == receipt.run_id {
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
    let snapshot = core.snapshot(root).await.unwrap();
    let assigned = tool_result(&snapshot.messages, "assign").unwrap();
    let repeated = tool_result(&snapshot.messages, "retry").unwrap();
    assert_eq!(repeated["status"], "already_applied");
    let child = core.subagents(root).await.unwrap()[0].child_chat_id;
    let child_snapshot = core.snapshot(child).await.unwrap();
    let assignments = child_snapshot
        .agent_message_origins
        .values()
        .filter(|source| source["message_id"] == assign_id.to_string())
        .count()
        + child_snapshot
            .pending_inputs
            .iter()
            .filter(|pending| {
                pending
                    .agent_message
                    .as_ref()
                    .is_some_and(|source| source.message_id == assign_id)
            })
            .count();
    assert_eq!(assignments, usize::from(!deny_messages), "{assigned}");
    if deny_messages {
        assert!(
            assigned["notification"]
                .as_str()
                .unwrap()
                .contains("pending")
        );
    }
    let completed = core
        .operation(root, receipt.run_id, "complete-own")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        completed.state,
        OperationState::Completed,
        "assigning another task must not block the assigner's completion: {:?}",
        completed.result
    );
    let notification_call_id = format!("task-assignment-{assign_id}");
    let notifications = snapshot
        .operations
        .iter()
        .filter(|operation| operation.call_id == notification_call_id)
        .collect::<Vec<_>>();
    assert!(!notifications.is_empty());
    for operation in notifications {
        assert_eq!(
            operation.state,
            if deny_messages {
                OperationState::Failed
            } else {
                OperationState::Completed
            }
        );
        assert_eq!(operation.result.as_ref().unwrap().is_error, deny_messages);
    }
    assert_eq!(snapshot.task_board.unwrap().tasks.len(), 2);
    core.set_task_tools_enabled(root, false).await.unwrap();
    let solo = core.submit(root, None, "new solo".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == root
                && event.run_id == solo.run_id
                && matches!(event.kind, EventKind::Completed { .. })
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(core.status(root).await.unwrap().task_board.is_none());
    core.shutdown().await;
}

#[tokio::test]
async fn task_capability_is_dynamic_and_assignment_delivery_is_idempotent() {
    run_assignment(false).await;
}

#[tokio::test]
async fn assigning_a_task_cannot_bypass_send_message_denial() {
    run_assignment(true).await;
}

#[tokio::test]
async fn a_finished_delegate_with_a_background_shell_blocks_task_release_until_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(RoutedModel::new(|request| {
        let host = state(request);
        if host["delegation"]["task"] == "background-shell" {
            if !has_tool_result(request, "background") {
                return call(
                    "background",
                    "Bash",
                    json!({"command":"sleep 30","run_in_background":true}),
                );
            }
            return text("Started the background command");
        }
        if !host["delegation"].is_null() {
            return text("primer done");
        }
        if !has_tool_result(request, "primer") {
            return call(
                "primer",
                "Subagent",
                json!({"task":"primer","name":"primer"}),
            );
        }
        if !has_tool_result(request, "create") {
            return call(
                "create",
                "TaskCreate",
                json!({"mutation_id":Uuid::from_u128(600),"title":"Bounded work","description":"Wait for all owned execution"}),
            );
        }
        let task_id = tool_result(&request.messages, "create").unwrap()["task"]["id"].clone();
        if !has_tool_result(request, "claim") {
            return call(
                "claim",
                "TaskUpdate",
                json!({"mutation_id":Uuid::from_u128(601),"task_id":task_id,"expected_revision":1,"action":"claim"}),
            );
        }
        if !has_tool_result(request, "worker") {
            return call(
                "worker",
                "Subagent",
                json!({"task":"background-shell","name":"worker","run_in_background":true}),
            );
        }
        if !has_tool_result(request, "gate") {
            return call(
                "gate",
                "Bash",
                json!({"command":"while [ ! -f release-gate ]; do sleep 0.01; done"}),
            );
        }
        let release = json!({"mutation_id":Uuid::from_u128(602),"task_id":task_id,"expected_revision":2,"action":"release","result":"Owned execution is stopped"});
        if !has_tool_result(request, "blocked-release") {
            return call("blocked-release", "TaskUpdate", release);
        }
        if !has_tool_result(request, "stop") {
            return call("stop", "StopSubagent", json!({"to":"worker"}));
        }
        if !has_tool_result(request, "release") {
            return call("release", "TaskUpdate", release);
        }
        text("root done")
    }));
    let core = Agent::new_embedded(config(), model);
    core.install_own_tickhost();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_task_tools_enabled(root, true).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(root, None, "bounded work".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::Completed { .. }) && event.chat_id != root {
                let snapshot = core.snapshot(event.chat_id).await.unwrap();
                if snapshot
                    .delegation
                    .as_ref()
                    .is_some_and(|delegation| delegation.task == "background-shell")
                {
                    std::fs::write(directory.path().join("release-gate"), "go").unwrap();
                }
            }
            if event.chat_id == root && event.run_id == receipt.run_id {
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
    let snapshot = core.snapshot(root).await.unwrap();
    assert!(snapshot.messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,tickloop_agent_protocol::Content::ToolResult {tool_use_id,is_error:true,..} if tool_use_id=="blocked-release")));
    assert_eq!(
        tool_result(&snapshot.messages, "release").unwrap()["status"],
        "applied"
    );
    assert_eq!(
        snapshot.task_board.unwrap().tasks[0].status,
        tickloop_agent_protocol::SharedTaskStatus::Pending
    );
    core.shutdown().await;
}

#[tokio::test]
async fn solo_background_tasks_finish_in_their_claiming_run_after_waiting() {
    use tickloop_agent_protocol::{Content, SharedTaskStatus};
    use tickloop_agent_test_support::routed::history_text;

    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(RoutedModel::new(|request| {
        if !has_tool_result(request, "create-first") {
            let mut calls = call(
                "create-first",
                "TaskCreate",
                json!({"mutation_id":Uuid::from_u128(700),"title":"First background task","description":"Wait for first output"}),
            );
            calls.extend(call("create-second", "TaskCreate", json!({"mutation_id":Uuid::from_u128(701),"title":"Second background task","description":"Wait for second output"})));
            for name in ["first-shell", "second-shell"] {
                calls.extend(call(name, "Bash", json!({"command":"while [ ! -f finish-background ]; do sleep 0.01; done; printf done","run_in_background":true})));
            }
            return calls;
        }
        let first = tool_result(&request.messages, "create-first").unwrap()["task"]["id"].clone();
        let second = tool_result(&request.messages, "create-second").unwrap()["task"]["id"].clone();
        if !has_tool_result(request, "claim-first") {
            return call(
                "claim-first",
                "TaskUpdate",
                json!({"mutation_id":Uuid::from_u128(702),"task_id":first,"expected_revision":1,"action":"claim"}),
            );
        }
        let history = history_text(request);
        let notified = |name: &str| history.contains(&format!("<tool-use-id>{name}</tool-use-id>"));
        if !notified("first-shell") || !notified("second-shell") {
            return text("Waiting for both background commands");
        }
        if !has_tool_result(request, "complete-first") {
            let mut calls = call(
                "complete-first",
                "TaskUpdate",
                json!({"mutation_id":Uuid::from_u128(703),"task_id":first,"expected_revision":2,"action":"complete","result":"First background command completed"}),
            );
            calls.extend(call("claim-second", "TaskUpdate", json!({"mutation_id":Uuid::from_u128(704),"task_id":second,"expected_revision":1,"action":"claim"})));
            calls.extend(call("complete-second", "TaskUpdate", json!({"mutation_id":Uuid::from_u128(705),"task_id":second,"expected_revision":2,"action":"complete","result":"Second background command completed"})));
            return calls;
        }
        text("All tracked work finished")
    }));
    let core = Agent::new_embedded(config(), model);
    core.install_own_tickhost();
    let chat = core.create_chat(directory.path().into()).await.unwrap();
    core.set_task_tools_enabled(chat, true).await.unwrap();
    core.set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(chat, None, "Track two background commands".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::Text(ref text) if text == "Waiting for both background commands") {
                break;
            }
            if let EventKind::Failed(error) = event.kind {
                panic!("{error}");
            }
        }
    }).await.unwrap();
    // The old runtime ends this Run here; the fixed runtime waits without polling the model.
    let _ = tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await;
    std::fs::write(directory.path().join("finish-background"), "go").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut final_run = None;
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::Text(ref text) if text == "All tracked work finished") {
                final_run = Some(event.run_id);
            }
            if Some(event.run_id) == final_run && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
            if let EventKind::Failed(error) = event.kind {
                panic!("{error}");
            }
        }
    }).await.unwrap();
    let snapshot = core.snapshot(chat).await.unwrap();
    core.shutdown().await;
    assert!(snapshot.delegation.is_none());
    let tasks = &snapshot.task_boards[&receipt.loop_id].tasks;
    assert_eq!(tasks.len(), 2);
    for task in tasks {
        assert_eq!(task.status, SharedTaskStatus::Completed);
        assert_eq!(task.claim.as_ref().unwrap().run_id, receipt.run_id);
        assert_eq!(task.report.as_ref().unwrap().run_id, receipt.run_id);
    }
    assert!(
        !snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::ToolResult { is_error: true, .. }))
    );
}
