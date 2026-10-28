use std::sync::Arc;
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::protocol::{
    ChatId, Content, Event, EventKind, ExecutionMode, HostNote, LoopState, PermissionRequest,
    PermissionScope, QuestionAnswer, Receipt, Role,
};
use tickloop_harness::runtime::{Agent, SubagentLaunch};
use tickloop_harness::test_support::*;
use tickloop_harness::tools::EXIT_PLAN_MODE_TOOL_NAME;
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
fn exit_notices(request: &tickloop_harness::model::ModelRequest) -> usize {
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

fn system_prompt(request: &tickloop_harness::model::ModelRequest) -> String {
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
        let receipt = agent.submit(chat, "Plan the report".into()).await.unwrap();
        let (owner, approval) = request(&mut events).await;
        assert_eq!(owner, chat);
        assert_eq!(approval.input["plan_version"], 1);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        agent.approve_plan(chat, 1, mode).await.unwrap();
        completed(&mut events, &receipt).await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.policy.mode, mode);
        assert_eq!(snapshot.chat_loop.run_id, Some(receipt.run_id));
        assert_eq!(snapshot.plans[0].approved_execution_mode, Some(mode));
        assert!(!snapshot.plans[0].approval_pending);
        let requests = model.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult { tool_use_id, content, is_error: false } if tool_use_id == "plan" && content.text().contains("User has approved your plan.") && content.text().ends_with("Create the reviewed report."))));
        // As in Claude Code, the request after the approval result also says Plan mode ended,
        // and names the mode the user approved in.
        assert_eq!(exit_notices(&requests[1]), 1);
        let note =
            tickloop_harness::test_support::latest_execution_mode_note(&requests[1]).unwrap();
        assert!(note.contains(EXIT_NOTICE), "{note}");
        assert!(
            note.contains(&format!("## {} Mode Active", mode.label())),
            "{note}"
        );
        assert!(tickloop_harness::test_support::is_host_note(
            requests[1].messages.last().unwrap()
        ));
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
    let receipt = agent.submit(chat, "Plan".into()).await.unwrap();
    request(&mut events).await;
    agent
        .decline_plan(chat, 1, Some("Include regression tests.".into()))
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
            .approve_plan(chat, 1, ExecutionMode::Auto)
            .await
            .is_err()
    );
    agent
        .approve_plan(chat, 2, ExecutionMode::AcceptEdits)
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
    agent.submit(chat, "Plan".into()).await.unwrap();
    let (_, approval) = request(&mut events).await;
    assert!(
        agent
            .approve_plan(chat, 2, ExecutionMode::Auto)
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
            .approve_plan(chat, 1, ExecutionMode::Auto)
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
    agent.decline_plan(chat, 1, None).await.unwrap();
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
    let a = agent.submit(first, "Plan A".into()).await.unwrap();
    request(&mut events).await;
    let b = agent.submit(second, "Plan B".into()).await.unwrap();
    request(&mut events).await;
    agent
        .approve_plan(first, 1, ExecutionMode::Auto)
        .await
        .unwrap();
    completed(&mut events, &a).await;
    assert_eq!(
        agent.snapshot(second).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    assert!(agent.snapshot(second).await.unwrap().plans[0].approval_pending);
    agent
        .approve_plan(second, 1, ExecutionMode::AcceptEdits)
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
    agent.submit(chat, "Plan".into()).await.unwrap();
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
        .approve_plan(chat, 1, ExecutionMode::AcceptEdits)
        .await
        .unwrap()
        .unwrap();
    let mut events = restarted.subscribe();
    let resumed = restarted.submit(chat, approval).await.unwrap();
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
    let run = agent.submit(chat, "Plan".into()).await.unwrap();
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
    agent.submit(chat, "Plan".into()).await.unwrap();
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
        .approve_plan(chat, 1, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    for feedback in [None, Some("Stale feedback".into())] {
        assert!(
            restored.decline_plan(chat, 1, feedback).await.is_err(),
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
    assert!(restored.decline_plan(chat, 1, None).await.is_err());
    assert!(
        restored
            .approve_plan(chat, 1, ExecutionMode::Auto)
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
        .submit(chat, "Prepare the report".into())
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
    let requests = model.requests.lock().unwrap().clone();
    let planning = requests
        .iter()
        .map(|request| reminded_plan_file(request).is_some())
        .collect::<Vec<_>>();
    assert_eq!(planning, [false, true, false]);
    assert_eq!(
        requests.iter().map(exit_notices).collect::<Vec<_>>(),
        [0, 0, 1],
        "the request after leaving Plan says so"
    );
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
    agent.submit(chat, "Plan".into()).await.unwrap();
    request(&mut events).await;
    agent.decline_plan(chat, 1, None).await.unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::Interrupted) {
            break;
        }
    }
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.decline_plan(chat, 1, None).await.unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.policy.mode, ExecutionMode::FullAccess);
    assert!(snapshot.plans[0].approval_pending);
    let continuation = agent
        .approve_plan(chat, 1, ExecutionMode::FullAccess)
        .await
        .unwrap()
        .expect("the ended Run continues explicitly");
    let resumed = agent.submit(chat, continuation).await.unwrap();
    completed(&mut events, &resumed).await;
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        exit_notices(&requests[1]),
        1,
        "the continuation carries the approved plan and one note that Plan ended"
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
        .submit(parent, "Delegate the check".into())
        .await
        .unwrap();
    completed(&mut events, &parent_run).await;
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let lists = |request: &tickloop_harness::model::ModelRequest, name: &str| {
        request.tools.iter().any(|tool| tool.name == name)
    };
    let defers_enter_plan_mode = |request: &tickloop_harness::model::ModelRequest| {
        request
            .messages
            .iter()
            .filter(|message| message.role == tickloop_harness::protocol::Role::System)
            .any(|message| {
                serde_json::to_string(message)
                    .unwrap()
                    .contains("# Deferred tools")
            })
    };
    assert!(
        lists(&requests[0], "ExitPlanMode"),
        "the root keeps ExitPlanMode"
    );
    assert!(
        defers_enter_plan_mode(&requests[0]),
        "the root reaches EnterPlanMode through tool search"
    );
    for name in ["EnterPlanMode", "ExitPlanMode", "SearchTools"] {
        assert!(!lists(&requests[1], name), "a child never gets {name}");
    }
    assert!(
        !defers_enter_plan_mode(&requests[1]),
        "a child never hears of EnterPlanMode"
    );
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
    let error = agent
        .approve_plan(child.id, 1, ExecutionMode::AcceptEdits)
        .await
        .expect_err("an approval naming the child cannot replace its delegated mode");
    assert!(
        error.to_string().contains("delegated execution mode"),
        "{error}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_child_frozen_in_plan_reports_its_findings_and_the_parent_submits_the_plan() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            serde_json::json!({
                "task": "Research the report change and propose a plan.",
                "run_in_background": false
            }),
        ),
        text("Child findings: change report.rs and add a regression test."),
        submit_plan("plan", "Change report.rs and add a regression test."),
    ]));
    let agent = Agent::new_embedded_with_subagent_launch(
        config(),
        model.clone(),
        SubagentLaunch::BackgroundByDefault,
    );
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
        .submit(parent, "Plan with a child".into())
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
    assert_eq!(child.chat_loop.state, LoopState::Idle);
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let mode_note = tickloop_harness::test_support::latest_execution_mode_note;
    assert!(mode_note(&requests[0]).unwrap().contains(
        "Subagent children you create while planning stay read-only and return their findings or proposed design in their final reply; only you write the plan file and call ExitPlanMode."
    ));
    let child_note = mode_note(&requests[1]).unwrap();
    assert!(child_note.contains("Answer the query you were given comprehensively in your final message, and do not write a plan or anything else to a file."));
    assert!(!child_note.contains("ExitPlanMode"), "{child_note}");
    assert!(!system_prompt(&requests[1]).contains("ExitPlanMode"));
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
    agent.submit(chat, "Plan".into()).await.unwrap();
    request(&mut events).await;
    agent.decline_plan(chat, 1, None).await.unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::Interrupted) {
            break;
        }
    }
    // Returning to Plan before the next request withdraws the notice.
    for mode in [ExecutionMode::Auto, ExecutionMode::Plan] {
        agent.set_execution_mode(chat, mode).await.unwrap();
    }
    let run = agent.submit(chat, "Keep planning".into()).await.unwrap();
    completed(&mut events, &run).await;
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    for prompt in ["Implement it", "Continue"] {
        let run = agent.submit(chat, prompt.into()).await.unwrap();
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
        matches!(&notice.content[..], [Content::Text { text }] if text.starts_with("<system-reminder>\n## Exited Plan Mode\n\n") && text.contains(&format!("The plan file is located at {plan_file} if you need to reference it.\n\n## Full Access Mode Active\n\n")))
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot
            .host_notes
            .values()
            .map(|note| match note {
                HostNote::ExecutionMode(note) => (
                    note.mode,
                    note.plan_file
                        .as_ref()
                        .map(|path| path.display().to_string())
                ),
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>(),
        [
            (ExecutionMode::Plan, Some(plan_file.clone())),
            (ExecutionMode::FullAccess, None)
        ]
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
            tickloop_harness::protocol::RewindMode::Conversation,
        )
        .await
        .unwrap()
        .draft
        .unwrap();
    let mut events = restarted.subscribe();
    let run = restarted.submit(chat, draft.prompt).await.unwrap();
    completed(&mut events, &run).await;
    assert_eq!(exit_notices(&model.requests.lock().unwrap()[0]), 1);
    restarted.shutdown().await;
}

/// Entering Plan, planning, reviewing, leaving and returning keep the request prefix byte for
/// byte the same: the plan, its approval and the mode reach the model only through tool results
/// and host notes at the end of the conversation.
#[tokio::test]
async fn every_planning_phase_keeps_the_prefix_and_returning_to_plan_reenters_it() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan", "First plan."),
        submit_plan("revised", "Second plan."),
        text("Implementing."),
        text("Replanning."),
        text("Still planning."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Plan the report".into()).await.unwrap();
    request(&mut events).await;
    agent
        .decline_plan(chat, 1, Some("Tighten it".into()))
        .await
        .unwrap();
    request(&mut events).await;
    agent
        .approve_plan(chat, 2, ExecutionMode::Auto)
        .await
        .unwrap();
    completed(&mut events, &receipt).await;
    for prompt in ["Plan the next step", "Keep planning"] {
        agent
            .set_execution_mode(chat, ExecutionMode::Plan)
            .await
            .unwrap();
        let receipt = agent.submit(chat, prompt.into()).await.unwrap();
        completed(&mut events, &receipt).await;
    }

    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 5);
    for request in &requests {
        assert_eq!(
            serde_json::to_vec(&request.messages[..2]).unwrap(),
            serde_json::to_vec(&requests[0].messages[..2]).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&requests[0].tools).unwrap()
        );
    }
    let system = system_prompt(&requests[0]);
    for absent in ["\"plan\"", "approval_pending", "First plan."] {
        assert!(!system.contains(absent), "{absent}");
    }
    let new_note = |index: usize| {
        let last = requests[index].messages.last().unwrap();
        is_host_note(last).then(|| match &last.content[0] {
            Content::Text { text } => text.clone(),
            other => panic!("{other:?}"),
        })
    };
    assert!(new_note(0).unwrap().contains("Plan mode is active."));
    assert_eq!(
        new_note(1),
        None,
        "feedback keeps planning without a new note"
    );
    let left = new_note(2).unwrap();
    assert!(left.contains("## Exited Plan Mode\n") && left.contains("## Auto Mode Active\n"));
    let reentered = new_note(3).unwrap();
    let plan_file = reminded_plan_file(&requests[0]).unwrap();
    assert!(
        reentered.starts_with(&format!(
            "<system-reminder>\n## Re-entering Plan Mode\n\nYou are returning to plan mode after having previously exited it. A plan file exists at {plan_file} from your previous planning session."
        )),
        "{reentered}"
    );
    assert!(
        reentered.contains("A plan file already exists at"),
        "{reentered}"
    );
    assert_eq!(new_note(4), None);
    agent.shutdown().await;
}
