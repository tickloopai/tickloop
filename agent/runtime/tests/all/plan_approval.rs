use std::sync::Arc;
use tickloop_agent_protocol::{
    ChatId, Content, Event, EventKind, ExecutionMode, HostNote, LoopState, PermissionRequest,
    PermissionScope, QuestionAnswer, Receipt, Role,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::*;
use tickloop_agent_tools::EXIT_PLAN_MODE_TOOL_NAME;
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

const EXIT_NOTICE: &str =
    "You have exited plan mode. You can now make edits, run tools, and take actions.";

/// How many messages of a model request carry the exited-plan-mode notice.
fn exit_notices(request: &tickloop_agent_model::ModelRequest) -> usize {
    request
        .messages
        .iter()
        .filter(|message| {
            message.content.iter().any(
                |content| matches!(content, Content::Text { text } if text.contains(EXIT_NOTICE)),
            )
        })
        .count()
}

fn system_prompt(request: &tickloop_agent_model::ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
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
        assert_eq!(
            exit_notices(&requests[1]),
            0,
            "the approval result already resumes the model"
        );
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

#[tokio::test]
async fn a_mode_change_during_a_run_applies_from_the_next_model_request() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "before-planning",
            "AskUserQuestion",
            single_question("Which report?"),
        ),
        call(
            "while-planning",
            "AskUserQuestion",
            single_question("Which window?"),
        ),
        text("Ready to implement."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Prepare the report".into())
        .await
        .unwrap();
    for mode in [ExecutionMode::Plan, ExecutionMode::Auto] {
        let question = loop {
            match next_event(&mut events).await.kind {
                EventKind::QuestionAsked { request } => break request,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        };
        agent.set_execution_mode(chat, mode).await.unwrap();
        agent
            .answer_question(chat, question.id, vec![QuestionAnswer::other("Weekly")])
            .await
            .unwrap();
    }
    completed(&mut events, &receipt).await;
    let planning = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| reminded_plan_file(request).is_some())
        .collect::<Vec<_>>();
    assert_eq!(planning, [false, true, false]);
    agent.shutdown().await;
}

#[tokio::test]
async fn closing_a_plan_review_keeps_the_plan_pending_and_the_chosen_mode() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan", "Pending plan."),
        text("Approved plan continued."),
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
        .decline_plan(chat, receipt.loop_id, 1, None)
        .await
        .unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::Interrupted) {
            break;
        }
    }
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .decline_plan(chat, receipt.loop_id, 1, None)
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.policy.mode, ExecutionMode::FullAccess);
    assert!(snapshot.plans[0].approval_pending);
    let continuation = agent
        .approve_plan(chat, receipt.loop_id, 1, ExecutionMode::FullAccess)
        .await
        .unwrap()
        .expect("the ended Run continues explicitly");
    let resumed = agent
        .submit(chat, Some(receipt.loop_id), continuation)
        .await
        .unwrap();
    completed(&mut events, &resumed).await;
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        exit_notices(&requests[1]),
        0,
        "approving after Shift+Tab left Plan resumes with the approved plan alone"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_delegated_child_gets_no_plan_mode_tools() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            serde_json::json!({"task":"Check the report inputs."}),
        ),
        text("Child checked the inputs."),
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
        .submit(parent, None, "Delegate the check".into())
        .await
        .unwrap();
    completed(&mut events, &parent_run).await;
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let offers = |request: &tickloop_agent_model::ModelRequest, name: &str| {
        request.tools.iter().any(|tool| tool.name == name)
    };
    for name in ["EnterPlanMode", "ExitPlanMode"] {
        assert!(offers(&requests[0], name), "the root keeps {name}");
        assert!(!offers(&requests[1], name), "a child never gets {name}");
    }
    assert!(
        requests[1]
            .tools
            .iter()
            .all(|tool| !tool.description.contains("ExitPlanMode")),
        "no child tool description points at ExitPlanMode"
    );
    let children = agent.subagents(parent).await.unwrap();
    let child = agent.snapshot(children[0].child_chat_id).await.unwrap();
    assert_eq!(child.policy.mode, ExecutionMode::FullAccess);
    assert!(child.plans.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_child_frozen_in_plan_reports_its_findings_and_the_parent_submits_the_plan() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            serde_json::json!({"task":"Research the report change and propose a plan."}),
        ),
        text("Child findings: change report.rs and add a regression test."),
        submit_plan("plan", "Change report.rs and add a regression test."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .set_auto_reviewer(Arc::new(ScriptedModel::new(vec![text(
            &auto_classification(false).to_string(),
        )])))
        .await;
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(parent, None, "Plan with a child".into())
        .await
        .unwrap();
    let mut child = None;
    loop {
        let event = next_event(&mut events).await;
        if event.chat_id != parent {
            child = Some(event.chat_id);
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                assert_eq!(request.name, EXIT_PLAN_MODE_TOOL_NAME);
                assert_eq!(event.chat_id, parent, "only the parent submits the plan");
                break;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let child = agent.snapshot(child.unwrap()).await.unwrap();
    assert_eq!(child.policy.mode, ExecutionMode::Plan);
    assert!(child.plans.is_empty());
    assert!(
        child
            .loops
            .iter()
            .all(|goal| goal.state == LoopState::Completed)
    );
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(system_prompt(&requests[0]).contains(
        "Children you create while planning stay read-only and return their findings or proposed design in their final reply; only you write the plan file and call ExitPlanMode."
    ));
    let child_prompt = system_prompt(&requests[1]);
    assert!(child_prompt.contains("Your parent Agent writes the plan"));
    assert!(!child_prompt.contains("ExitPlanMode"), "{child_prompt}");
    assert!(reminded_plan_file(&requests[1]).is_none());
    assert!(
        requests[1]
            .tools
            .iter()
            .all(|tool| !tool.name.ends_with("PlanMode")
                && !tool.description.contains("ExitPlanMode"))
    );
    assert!(requests[2].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult { tool_use_id, content, is_error: false } if tool_use_id == "delegate" && content.text().contains("Child findings"))));
    assert_eq!(
        agent.snapshot(parent).await.unwrap().plans[0].markdown,
        "Change report.rs and add a regression test."
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn leaving_plan_without_approval_tells_the_next_request_once() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan", "Pending plan."),
        text("Still planning."),
        text("Implementing now."),
        text("Still implementing."),
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
    agent
        .decline_plan(chat, receipt.loop_id, 1, None)
        .await
        .unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::Interrupted) {
            break;
        }
    }
    // Returning to Plan before the next request withdraws the notice.
    for mode in [ExecutionMode::Auto, ExecutionMode::Plan] {
        agent.set_execution_mode(chat, mode).await.unwrap();
    }
    let run = agent
        .submit(chat, Some(receipt.loop_id), "Keep planning".into())
        .await
        .unwrap();
    completed(&mut events, &run).await;
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    for prompt in ["Implement it", "Continue"] {
        let run = agent
            .submit(chat, Some(receipt.loop_id), prompt.into())
            .await
            .unwrap();
        completed(&mut events, &run).await;
    }
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(
        requests.iter().map(exit_notices).collect::<Vec<_>>(),
        [0, 0, 1, 1],
        "the notice is written once and stays in the history"
    );
    let plan_file = reminded_plan_file(&requests[1]).unwrap();
    let notice = requests[2].messages.last().unwrap();
    assert_eq!(notice.role, Role::User);
    assert!(
        matches!(&notice.content[..], [Content::Text { text }] if text.starts_with("## Exited Plan Mode\n\n") && text.ends_with(&format!("The plan file is located at {plan_file} if you need to reference it.")))
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.host_notes.values().collect::<Vec<_>>(),
        [&HostNote::PlanModeExit]
    );
    let public = serde_json::to_string(&snapshot.public_conversation_items().unwrap()).unwrap();
    assert!(!public.contains(EXIT_NOTICE), "{public}");
    agent.shutdown().await;

    let model = Arc::new(ScriptedModel::new(vec![text("Implemented again.")]));
    let restarted = Agent::open_embedded(root, persisted_config(), model.clone()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.host_notes, snapshot.host_notes);
    // Rewinding to the prompt whose request carried the notice owes it again.
    let point = restored
        .rewind_points()
        .into_iter()
        .find(|point| point.prompt == "Implement it")
        .unwrap();
    let draft = restarted
        .rewind(
            chat,
            point.message_id,
            tickloop_agent_protocol::RewindMode::Conversation,
        )
        .await
        .unwrap()
        .draft
        .unwrap();
    let mut events = restarted.subscribe();
    let run = restarted
        .submit(chat, Some(draft.loop_id), draft.prompt)
        .await
        .unwrap();
    completed(&mut events, &run).await;
    assert_eq!(exit_notices(&model.requests.lock().unwrap()[0]), 1);
    restarted.shutdown().await;
}
