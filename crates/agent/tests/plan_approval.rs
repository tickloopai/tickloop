mod support;

use std::sync::Arc;
use support::*;
use tickloop_agent::{
    model::Content,
    permissions::{ExecutionMode, PermissionScope},
    runtime::{
        Agent, ChatId, EXIT_PLAN_MODE_TOOL_NAME, Event, EventKind, PermissionRequest, Receipt,
    },
};
use tokio::sync::broadcast::Receiver;

async fn request(events: &mut Receiver<Event>) -> (ChatId, PermissionRequest) {
    loop {
        let event = next_event(events).await;
        match event.kind {
            EventKind::PermissionRequested { request }
                if request.name == EXIT_PLAN_MODE_TOOL_NAME =>
            {
                return (event.chat_id, request);
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

async fn completed(events: &mut Receiver<Event>, receipt: &Receipt) {
    loop {
        let event = next_event(events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("{error}"),
            EventKind::Interrupted => panic!("run interrupted"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn every_approval_mode_returns_the_reviewed_plan_and_continues_the_same_run() {
    for mode in [
        ExecutionMode::Auto,
        ExecutionMode::AcceptEdits,
        ExecutionMode::FullAccess,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            submit_plan("plan", "Create the reviewed report."),
            text("Implementation started."),
        ]));
        let agent = Agent::new_embedded(config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Plan)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, None, "Plan the report".into())
            .await
            .unwrap();
        let (owner, approval) = request(&mut events).await;
        assert_eq!(owner, chat);
        assert_eq!(approval.input["plan_version"], 1);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        agent
            .approve_plan(chat, receipt.loop_id, 1, mode)
            .await
            .unwrap();
        completed(&mut events, &receipt).await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.policy.mode, mode);
        assert_eq!(snapshot.loops[0].run_id, Some(receipt.run_id));
        assert_eq!(snapshot.plans[0].approved_execution_mode, Some(mode));
        assert!(!snapshot.plans[0].approval_pending);
        let requests = model.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult { tool_use_id, content, is_error: false } if tool_use_id == "plan" && content.text().contains("User has approved your plan.") && content.text().ends_with("Create the reviewed report."))));
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn keep_planning_returns_feedback_as_a_tool_result_and_requires_a_new_decision() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("first", "First plan."),
        submit_plan("revised", "Revised plan with tests."),
        text("Implementation started."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "Plan".into()).await.unwrap();
    request(&mut events).await;
    agent
        .decline_plan(
            chat,
            receipt.loop_id,
            1,
            Some("Include regression tests.".into()),
        )
        .await
        .unwrap();
    let (_, revised) = request(&mut events).await;
    assert_eq!(revised.input["plan_version"], 2);
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult { tool_use_id, content, is_error: true } if tool_use_id == "first" && content.text().contains("Include regression tests."))));
    assert!(
        agent
            .approve_plan(chat, receipt.loop_id, 1, ExecutionMode::Auto)
            .await
            .is_err()
    );
    agent
        .approve_plan(chat, receipt.loop_id, 2, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    completed(&mut events, &receipt).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn stale_versions_disk_edits_and_generic_allow_cannot_approve_a_plan() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan", "Reviewed body."),
        text("Must not execute."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "Plan".into()).await.unwrap();
    let (_, approval) = request(&mut events).await;
    assert!(
        agent
            .approve_plan(chat, receipt.loop_id, 2, ExecutionMode::Auto)
            .await
            .is_err()
    );
    assert!(
        agent
            .answer_permission_with_comment(chat, approval.id, true, PermissionScope::Once, None)
            .await
            .is_err()
    );
    let path = reminded_plan_file(&model.requests.lock().unwrap()[0]).unwrap();
    std::fs::write(path, "Unreviewed disk edit.").unwrap();
    assert!(
        agent
            .approve_plan(chat, receipt.loop_id, 1, ExecutionMode::Auto)
            .await
            .unwrap_err()
            .to_string()
            .contains("revised")
    );
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    agent
        .decline_plan(chat, receipt.loop_id, 1, None)
        .await
        .unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::Interrupted) {
            break;
        }
    }
    assert!(agent.snapshot(chat).await.unwrap().plans[0].approval_pending);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn approval_decisions_are_isolated_by_agent() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("first", "Plan A."),
        submit_plan("second", "Plan B."),
        text("A started."),
        text("B started."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let first = agent.create_chat(directory.path().into()).await.unwrap();
    let second = agent.create_chat(directory.path().into()).await.unwrap();
    for chat in [first, second] {
        agent
            .set_execution_mode(chat, ExecutionMode::Plan)
            .await
            .unwrap();
    }
    let mut events = agent.subscribe();
    let a = agent.submit(first, None, "Plan A".into()).await.unwrap();
    request(&mut events).await;
    let b = agent.submit(second, None, "Plan B".into()).await.unwrap();
    request(&mut events).await;
    assert!(
        agent
            .approve_plan(second, a.loop_id, 1, ExecutionMode::Auto)
            .await
            .is_err()
    );
    agent
        .approve_plan(first, a.loop_id, 1, ExecutionMode::Auto)
        .await
        .unwrap();
    completed(&mut events, &a).await;
    assert_eq!(
        agent.snapshot(second).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    assert!(agent.snapshot(second).await.unwrap().plans[0].approval_pending);
    agent
        .approve_plan(second, b.loop_id, 1, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    completed(&mut events, &b).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn restart_restores_a_pending_approval_without_resuming_execution() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan", "Saved pending plan."),
        text("Must not execute."),
    ]));
    let agent = Agent::open_embedded(root.clone(), persisted_config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, "Plan".into()).await.unwrap();
    request(&mut events).await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    agent.shutdown().await;
    let model = Arc::new(ScriptedModel::new(vec![text(
        "Restored implementation started.",
    )]));
    let restarted = Agent::open_embedded(root, persisted_config(), model.clone()).unwrap();
    let snapshot = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.plans[0].version, 1);
    assert!(snapshot.plans[0].approval_pending);
    assert_eq!(snapshot.policy.mode, ExecutionMode::Plan);
    assert!(model.requests.lock().unwrap().is_empty());
    let approval = restarted
        .approve_plan(chat, receipt.loop_id, 1, ExecutionMode::AcceptEdits)
        .await
        .unwrap()
        .unwrap();
    let mut events = restarted.subscribe();
    let resumed = restarted
        .submit(chat, Some(receipt.loop_id), approval)
        .await
        .unwrap();
    completed(&mut events, &resumed).await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    restarted.shutdown().await;
}

#[tokio::test]
async fn child_agent_requests_approval_without_expanding_its_delegated_authority() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "DelegateAgent",
            serde_json::json!({"task":"Plan the child task and wait for approval."}),
        ),
        call("enter", "EnterPlanMode", serde_json::json!({})),
        submit_plan("child-plan", "Reviewed child plan."),
        text("Child implementation complete."),
        text("Parent observed completion."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let parent_run = agent
        .submit(parent, None, "Delegate the planned task".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } if request.name == "EnterPlanMode" => {
                assert_ne!(event.chat_id, parent);
                agent
                    .answer_permission_with_comment(
                        event.chat_id,
                        request.id,
                        true,
                        PermissionScope::Once,
                        None,
                    )
                    .await
                    .unwrap();
                break;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let (child, approval) = request(&mut events).await;
    assert_ne!(child, parent);
    assert!(agent.snapshot(parent).await.unwrap().plans.is_empty());
    assert!(
        agent
            .approve_plan(child, approval.loop_id, 1, ExecutionMode::Auto)
            .await
            .is_err()
    );
    agent
        .approve_plan(child, approval.loop_id, 1, ExecutionMode::FullAccess)
        .await
        .unwrap();
    completed(&mut events, &parent_run).await;
    assert_eq!(
        agent.snapshot(child).await.unwrap().policy.mode,
        ExecutionMode::FullAccess
    );
    assert_eq!(
        agent.snapshot(parent).await.unwrap().policy.mode,
        ExecutionMode::FullAccess
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn recovery_normalization_survives_two_idle_restarts_without_revising_again() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![submit_plan(
        "plan",
        "Original plan.",
    )]));
    let agent = Agent::open_embedded(root.clone(), persisted_config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let run = agent.submit(chat, None, "Plan".into()).await.unwrap();
    request(&mut events).await;
    let path = reminded_plan_file(&model.requests.lock().unwrap()[0]).unwrap();
    agent.shutdown().await;
    std::fs::write(path, "Externally revised plan.").unwrap();
    let mut versions = Vec::new();
    for _ in 0..2 {
        let model = Arc::new(ScriptedModel::new(Vec::new()));
        let restored =
            Agent::open_embedded(root.clone(), persisted_config(), model.clone()).unwrap();
        let snapshot = restored.resume_chat(chat).await.unwrap();
        versions.push(snapshot.plans[0].version);
        assert!(snapshot.plans[0].approval_pending);
        assert_eq!(snapshot.plans[0].loop_id, run.loop_id);
        assert_eq!(snapshot.plans[0].markdown, "Externally revised plan.");
        assert!(model.requests.lock().unwrap().is_empty());
        restored.shutdown().await;
    }
    assert_eq!(versions, [2, 2]);
}

#[tokio::test]
async fn processed_plans_reject_stale_cancel_feedback_and_approval_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let agent = Agent::open_embedded(
        root.clone(),
        persisted_config(),
        Arc::new(ScriptedModel::new(vec![submit_plan(
            "plan",
            "Reviewed plan.",
        )])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let run = agent.submit(chat, None, "Plan".into()).await.unwrap();
    request(&mut events).await;
    agent.shutdown().await;
    let restored = Agent::open_embedded(
        root.clone(),
        persisted_config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    restored
        .approve_plan(chat, run.loop_id, 1, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    for feedback in [None, Some("Stale feedback".into())] {
        assert!(
            restored
                .decline_plan(chat, run.loop_id, 1, feedback)
                .await
                .is_err(),
            "a processed approval must not be re-pended"
        );
    }
    assert!(!restored.snapshot(chat).await.unwrap().plans[0].approval_pending);
    assert_eq!(
        restored.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::AcceptEdits
    );
    restored.shutdown().await;
    let restored = Agent::open_embedded(
        root,
        persisted_config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    assert!(
        restored
            .decline_plan(chat, run.loop_id, 1, None)
            .await
            .is_err()
    );
    assert!(
        restored
            .approve_plan(chat, run.loop_id, 1, ExecutionMode::Auto)
            .await
            .is_err()
    );
    assert!(!restored.snapshot(chat).await.unwrap().plans[0].approval_pending);
    restored.shutdown().await;
}
