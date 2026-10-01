mod support;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::*;
use tickloop_agent::{
    model::Content,
    permissions::{Decision, ExecutionMode, PermissionRule},
    runtime::{
        Agent, BranchRequest, ChatId, ChatSnapshot, EXIT_PLAN_MODE_TOOL_NAME, EventKind, LoopId,
        LoopState, RewindMode, TODO_WRITE_TOOL_NAME, TodoItem, TodoStatus,
    },
    tools::ToolResult,
};
use uuid::Uuid;

#[tokio::test]
async fn plans_can_be_revised_and_only_the_current_version_can_be_approved() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan1", "Read notes, then produce a draft."),
        text("review this plan"),
        submit_plan("plan2", "Read notes, then produce a concise draft."),
        text("revised plan ready"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Plan the draft".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(agent.snapshot(chat).await.unwrap().plans[0].version, 1);
    agent
        .submit(chat, Some(receipt.loop_id), "Make the draft concise".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(
        agent
            .approve_plan(chat, receipt.loop_id, 1, ExecutionMode::AcceptEdits)
            .await
            .is_err()
    );
    agent
        .approve_plan(chat, receipt.loop_id, 2, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.policy.mode, ExecutionMode::AcceptEdits);
    assert_eq!(
        snapshot.plans[0].approved_execution_mode,
        Some(ExecutionMode::AcceptEdits)
    );
}

fn system_prompt(request: &tickloop_agent::model::ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == tickloop_agent::model::Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn plan_mode_requests_name_the_current_loop_plan_file_as_the_only_editable_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        text("Exploring."),
        text("Working."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let goal = run_turn(&agent, chat, None, "Plan the report")
        .await
        .loop_id;
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    run_turn(&agent, chat, Some(goal), "Write the report").await;

    let requests = model.requests.lock().unwrap().clone();
    let planning = system_prompt(&requests[0]);
    let plan_file = PathBuf::from(reminded_plan_file(&requests[0]).expect("plan file reminder"));
    assert!(plan_file.is_absolute());
    assert_eq!(
        plan_file.parent().unwrap(),
        root.canonicalize().unwrap().join("plans")
    );
    assert!(
        plan_file
            .to_str()
            .unwrap()
            .ends_with(&format!("-loop-{goal}.md"))
    );
    for expected in [
        "Plan mode is active.",
        "No plan file exists yet. You should create your plan at",
        "NOTE that this is the only file you are allowed to edit",
        "Calling ExitPlanMode when the plan is ready for approval",
    ] {
        assert!(planning.contains(expected), "{expected}\n{planning}");
    }
    let tool_names = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"EnterPlanMode"), "{tool_names:?}");
    assert!(
        tool_names.contains(&EXIT_PLAN_MODE_TOOL_NAME),
        "{tool_names:?}"
    );
    assert!(!tool_names.contains(&"set_plan"), "{tool_names:?}");
    assert!(!system_prompt(&requests[1]).contains("Plan mode is active."));
    assert_eq!(reminded_plan_file(&requests[1]), None);
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_mode_admits_writes_only_to_the_current_plan_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        write_plan("plan-write", "Draft plan."),
        call(
            "plan-edit",
            "Edit",
            json!({"file_path": PLAN_FILE, "old_string": "Draft", "new_string": "Final"}),
        ),
        call(
            "other-write",
            "Write",
            json!({"file_path": "notes.txt", "content": "not a plan"}),
        ),
        call("shell", "Bash", json!({"command": "touch touched.txt"})),
        text("Done."),
    ]));
    let agent = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let results = run_turn_results(&agent, chat, None, "Plan the report").await;

    for call_id in ["plan-write", "plan-edit"] {
        let result = result_of(&results, call_id);
        assert!(!result.is_error, "{call_id}: {}", result.content);
    }
    for call_id in ["other-write", "shell"] {
        assert!(result_of(&results, call_id).is_error, "{call_id}");
    }
    let plan_file = reminded_plan_file(&model.requests.lock().unwrap()[0]).unwrap();
    assert_eq!(std::fs::read_to_string(plan_file).unwrap(), "Final plan.");
    assert!(!directory.path().join("notes.txt").exists());
    assert!(!directory.path().join("touched.txt").exists());
    assert!(agent.snapshot(chat).await.unwrap().plans.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn exit_plan_mode_submits_the_written_plan_file_for_approval() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let markdown = "1. Read the notes.\n2. Draft the report.";
    let model = Arc::new(ScriptedModel::new(vec![
        exit_plan_mode("missing"),
        write_plan("empty-write", ""),
        exit_plan_mode("empty"),
        write_plan("plan-write", markdown),
        call(
            "submit",
            EXIT_PLAN_MODE_TOOL_NAME,
            json!({"allowedPrompts": [{"tool": "Bash", "prompt": "run tests"}], "note": "ignored"}),
        ),
        text("The plan is ready for review."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let mut events = agent.subscribe();
    let results = run_turn_results(&agent, chat, None, "Plan the report").await;
    let plan_file = reminded_plan_file(&model.requests.lock().unwrap()[0]).unwrap();

    let missing = result_of(&results, "missing");
    assert!(missing.is_error);
    assert_eq!(
        missing.content,
        format!(
            "No plan file found at {plan_file}. Please write your plan to this file before calling ExitPlanMode."
        )
    );
    let empty = result_of(&results, "empty");
    assert!(empty.is_error);
    assert_eq!(
        empty.content,
        format!(
            "The plan file at {plan_file} is empty. Please write your plan to this file before calling ExitPlanMode."
        )
    );
    let submitted = result_of(&results, "submit");
    assert!(!submitted.is_error, "{}", submitted.content);
    assert!(
        submitted
            .content
            .starts_with("Your plan has been submitted to the user for approval as version 1."),
        "{}",
        submitted.content
    );
    assert!(
        submitted
            .content
            .contains(&format!("Plan file: {plan_file}"))
    );
    assert!(
        submitted.content.ends_with(markdown),
        "{}",
        submitted.content
    );

    let updated = loop {
        if let EventKind::PlanUpdated { plan } = next_event(&mut events).await.kind {
            break plan;
        }
    };
    assert_eq!((updated.version, updated.markdown.as_str()), (1, markdown));
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = snapshot.loops[0].id;
    assert_eq!(snapshot.loops[0].state, LoopState::WaitingPlan);
    assert_eq!(plan_of(&snapshot, goal), (1, markdown.into()));
    assert_eq!(plan_files(&root, goal), [markdown]);
    agent
        .approve_plan(chat, goal, 1, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::AcceptEdits
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn approving_a_plan_starts_it_with_claude_codes_approval_and_the_plan_file() {
    let directory = tempfile::tempdir().unwrap();
    let markdown = "1. Read the notes.\n2. Draft the report.";
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan", markdown),
        text("The plan is ready for review."),
    ]));
    let agent =
        Agent::open_embedded(directory.path().join("root"), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let goal = run_turn(&agent, chat, None, "Plan the report")
        .await
        .loop_id;
    let plan_file = reminded_plan_file(&model.requests.lock().unwrap()[0]).unwrap();

    let continuation = agent
        .approve_plan(chat, goal, 1, ExecutionMode::AcceptEdits)
        .await
        .unwrap();

    assert_eq!(
        continuation,
        format!(
            "User has approved your plan. You can now start coding. Start with updating your todo list if applicable

Your plan has been saved to: {plan_file}
You can refer back to it if needed during implementation.

## Approved Plan:
{markdown}"
        )
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn exit_plan_mode_outside_plan_mode_explains_that_no_plan_is_being_made() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            exit_plan_mode("exit"),
            text("Continuing."),
        ])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let results = run_turn_results(&agent, chat, None, "Write the report").await;
    let result = result_of(&results, "exit");
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "You are not in plan mode. This tool is only for exiting plan mode after writing a plan. If your plan was already approved, continue with implementation."
    );
    assert!(agent.snapshot(chat).await.unwrap().plans.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_submitted_plan_file_can_be_edited_without_rereading_and_submitted_again() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan-1", "Check the notes."),
        text("Planned."),
        call(
            "plan-edit",
            "Edit",
            json!({"file_path": PLAN_FILE, "old_string": "the notes.", "new_string": "the notes and the calendar."}),
        ),
        exit_plan_mode("plan-2"),
        text("Revised."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let goal = run_turn(&agent, chat, None, "Plan it").await.loop_id;
    let results = run_turn_results(&agent, chat, Some(goal), "Also check the calendar").await;
    let edited = result_of(&results, "plan-edit");
    assert!(!edited.is_error, "{}", edited.content);
    assert!(!result_of(&results, "plan-2").is_error);
    let revised = "Check the notes and the calendar.";
    assert_eq!(
        plan_of(&agent.snapshot(chat).await.unwrap(), goal),
        (2, revised.into())
    );
    assert_eq!(plan_files(&root, goal), [revised]);
    assert!(
        agent
            .approve_plan(chat, goal, 1, ExecutionMode::AcceptEdits)
            .await
            .is_err()
    );
    agent
        .approve_plan(chat, goal, 2, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_file_edits_after_submission_are_restored_only_as_a_new_unapproved_version() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (chat, goal) = run_and_shut_down(
        &root,
        directory.path(),
        vec![
            submit_plan("plan-1", "First plan."),
            text("Planned."),
            call(
                "plan-edit",
                "Edit",
                json!({"file_path": PLAN_FILE, "old_string": "First", "new_string": "Unsubmitted"}),
            ),
            text("Edited without submitting."),
        ],
        &["Plan it", "Edit it"],
    )
    .await;

    let restarted =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    let (version, markdown) = plan_of(&restored, goal);
    assert_eq!(markdown, "Unsubmitted plan.");
    assert!(version > 1, "restored version {version} was already used");
    assert!(!restarted.transcript_load_warnings(chat).is_empty());
    assert!(
        restarted
            .approve_plan(chat, goal, 1, ExecutionMode::AcceptEdits)
            .await
            .is_err()
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn a_plan_file_that_was_never_submitted_is_not_restored_as_a_plan() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (chat, goal) = run_and_shut_down(
        &root,
        directory.path(),
        vec![write_plan("draft", "Draft plan."), text("Drafted.")],
        &["Plan it"],
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(only_plan_file(&root, goal)).unwrap(),
        "Draft plan."
    );

    let restarted =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert!(restored.plans.is_empty());
    assert!(
        restarted.transcript_load_warnings(chat).is_empty(),
        "{:?}",
        restarted.transcript_load_warnings(chat)
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn ordinary_progress_keeps_the_execution_mode_and_survives_file_work_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let model = Arc::new(ScriptedModel::new(vec![
        todo_write(
            "progress-start",
            "Write the report",
            "Writing the report",
            "in_progress",
        ),
        call(
            "write",
            "Write",
            json!({"file_path":"report.txt", "content":"ordinary progress result"}),
        ),
        todo_write(
            "progress-finish",
            "Write the report",
            "Writing the report",
            "completed",
        ),
        text("The report is ready."),
    ]));
    let agent = Agent::open_embedded(store.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Allow,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(
            chat,
            None,
            "Write the report and keep me informed of progress.".into(),
        )
        .await
        .unwrap();
    let mut observed = Vec::new();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::ProgressUpdated { progress } => {
                assert_eq!(progress.loop_id, receipt.loop_id);
                observed.push(progress.todos[0].status.clone());
                let snapshot = agent.snapshot(chat).await.unwrap();
                assert_eq!(snapshot.policy.mode, ExecutionMode::AcceptEdits);
                assert!(snapshot.plans.is_empty());
            }
            EventKind::PermissionRequested { .. } => {
                panic!("ordinary progress must not introduce Plan approval")
            }
            EventKind::Failed(error) => panic!("{error}"),
            EventKind::Completed { .. } => break,
            _ => {}
        }
    }
    assert_eq!(observed, [TodoStatus::InProgress, TodoStatus::Completed]);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("report.txt")).unwrap(),
        "ordinary progress result"
    );
    agent.shutdown().await;
    drop(agent);
    let restored =
        Agent::open_embedded(store, config(), Arc::new(ScriptedModel::new(vec![]))).unwrap();
    restored.resume_chat(chat).await.unwrap();
    let snapshot = restored.snapshot(chat).await.unwrap();
    assert!(snapshot.plans.is_empty());
    assert_eq!(snapshot.progress.len(), 1);
    assert_eq!(snapshot.progress[0].loop_id, receipt.loop_id);
    assert_eq!(
        snapshot.progress[0].todos,
        [TodoItem {
            content: "Write the report".into(),
            status: TodoStatus::Completed,
            active_form: "Writing the report".into(),
        }]
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn todo_write_takes_claude_code_todos_and_rejects_incomplete_ones() {
    let directory = tempfile::tempdir().unwrap();
    let todo = |content: &str, status: &str, active_form: &str| json!({"content": content, "status": status, "activeForm": active_form});
    let rejected_inputs = [
        (
            "old-steps",
            json!({"steps":[{"description":"Read the notes","status":"pending"}]}),
        ),
        (
            "missing-active-form",
            json!({"todos":[{"content":"Read the notes","status":"pending"}]}),
        ),
        (
            "empty-content",
            json!({"todos":[todo("", "pending", "Reading the notes")]}),
        ),
        (
            "blank-active-form",
            json!({"todos":[todo("Read the notes", "pending", "  ")]}),
        ),
        (
            "unknown-status",
            json!({"todos":[todo("Read the notes", "done", "Reading the notes")]}),
        ),
        (
            "extra-field",
            json!({"todos":[todo("Read the notes", "pending", "Reading the notes")], "note": "x"}),
        ),
    ];
    let mut steps = rejected_inputs
        .iter()
        .map(|(id, input)| call(id, TODO_WRITE_TOOL_NAME, input.clone()))
        .collect::<Vec<_>>();
    steps.extend([
        call(
            "accepted",
            TODO_WRITE_TOOL_NAME,
            json!({"todos":[
                todo("Read the notes", "completed", "Reading the notes"),
                todo("Draft the report", "in_progress", "Drafting the report"),
                todo("Check the figures", "in_progress", "Checking the figures"),
            ]}),
        ),
        text("Drafting."),
    ]);
    let model = Arc::new(ScriptedModel::new(steps));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let results = run_turn_results(&agent, chat, None, "Draft the report").await;

    for (id, _) in &rejected_inputs {
        let result = result_of(&results, id);
        assert!(result.is_error, "{id}: {}", result.content);
        assert!(
            result.content.starts_with("invalid TodoWrite input"),
            "{id}: {}",
            result.content
        );
    }
    let accepted = result_of(&results, "accepted");
    assert!(!accepted.is_error, "{}", accepted.content);
    assert_eq!(
        accepted.content,
        "Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress. Please proceed with the current tasks if applicable"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.progress.len(), 1);
    assert_eq!(snapshot.progress[0].loop_id, snapshot.loops[0].id);
    assert_eq!(
        snapshot.progress[0].todos[1],
        TodoItem {
            content: "Draft the report".into(),
            status: TodoStatus::InProgress,
            active_form: "Drafting the report".into(),
        }
    );
    assert_eq!(
        snapshot.progress[0].active_form(),
        Some("Drafting the report")
    );

    let requests = model.requests.lock().unwrap().clone();
    let tool_names = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(!tool_names.contains(&"update_progress"), "{tool_names:?}");
    let todo_write = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == TODO_WRITE_TOOL_NAME)
        .expect("TodoWrite is offered");
    assert!(
        todo_write
            .description
            .starts_with("Use this tool to create and manage a structured task list"),
        "{}",
        todo_write.description
    );
    let item_schema = &todo_write.input_schema["properties"]["todos"]["items"];
    assert_eq!(
        item_schema["required"],
        json!(["content", "status", "activeForm"])
    );
    assert_eq!(
        item_schema["properties"]["status"]["enum"],
        json!(["pending", "in_progress", "completed"])
    );
    assert_eq!(item_schema["properties"]["content"]["minLength"], 1);
    assert_eq!(item_schema["properties"]["activeForm"]["minLength"], 1);
    assert!(
        system_prompt(requests.last().unwrap()).contains(r#""activeForm":"Drafting the report""#)
    );
    agent.shutdown().await;
}

async fn run_turn(
    agent: &Agent,
    chat: ChatId,
    goal: Option<LoopId>,
    prompt: &str,
) -> tickloop_agent::runtime::Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, goal, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return receipt,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            EventKind::PermissionRequested { request } => {
                panic!("unexpected permission request for {}", request.name)
            }
            _ => {}
        }
    }
}

/// The run's tool results in completion order.
async fn run_turn_results(
    agent: &Agent,
    chat: ChatId,
    goal: Option<LoopId>,
    prompt: &str,
) -> Vec<(String, ToolResult)> {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, goal, prompt.into()).await.unwrap();
    let mut results = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolCompleted { call_id, result } => results.push((call_id, result)),
            EventKind::Completed { .. } => return results,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            EventKind::PermissionRequested { request } => {
                panic!("unexpected permission request for {}", request.name)
            }
            _ => {}
        }
    }
}

fn result_of<'a>(results: &'a [(String, ToolResult)], call_id: &str) -> &'a ToolResult {
    results
        .iter()
        .find(|(id, _)| id == call_id)
        .map(|(_, result)| result)
        .unwrap_or_else(|| panic!("call {call_id} has no result in {results:?}"))
}

async fn enter_plan_mode(agent: &Agent, chat: ChatId) {
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
}

/// Leaves Plan mode and finishes a turn outside it, so the goal no longer waits for plan approval.
async fn leave_plan_mode(agent: &Agent, chat: ChatId, goal: LoopId) {
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    run_turn(agent, chat, Some(goal), "Pause planning").await;
}

fn read_plan(id: &str) -> Vec<Content> {
    call(id, "Read", json!({ "file_path": PLAN_FILE }))
}

fn todo_write(id: &str, content: &str, active_form: &str, status: &str) -> Vec<Content> {
    call(
        id,
        TODO_WRITE_TOOL_NAME,
        json!({"todos":[{"content": content, "status": status, "activeForm": active_form}]}),
    )
}

/// Loop id、计划（版本与内容）与待办项。
type LoopPlanState = (LoopId, Option<(u64, String)>, Vec<String>);

/// 每个 Loop 的计划版本与内容、待办项（按 Loop id 排序），用于比较重启前后。
fn plan_state(snapshot: &ChatSnapshot) -> Vec<LoopPlanState> {
    let mut state = snapshot
        .loops
        .iter()
        .map(|goal| {
            let plan = snapshot
                .plans
                .iter()
                .find(|plan| plan.loop_id == goal.id)
                .map(|plan| (plan.version, plan.markdown.clone()));
            let todos = snapshot
                .progress
                .iter()
                .find(|progress| progress.loop_id == goal.id)
                .map(|progress| {
                    progress
                        .todos
                        .iter()
                        .map(|todo| {
                            format!("{}:{}:{:?}", todo.content, todo.active_form, todo.status)
                        })
                        .collect()
                })
                .unwrap_or_default();
            (goal.id, plan, todos)
        })
        .collect::<Vec<_>>();
    state.sort_by_key(|(loop_id, _, _)| *loop_id);
    state
}

/// `<root>/plans` 中以该 Loop 结尾的计划文件路径。
fn plan_file_paths(root: &Path, loop_id: LoopId) -> Vec<PathBuf> {
    let suffix = format!("-loop-{loop_id}.md");
    let mut paths = std::fs::read_dir(root.join("plans"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

/// 计划文件去掉首行版本注释后的正文。
fn plan_body(text: &str) -> String {
    let (header, body) = text
        .split_once('\n')
        .expect("plan file has a version header");
    assert!(
        header.starts_with("<!--") && header.ends_with("-->"),
        "unexpected plan file header {header:?}"
    );
    body.to_owned()
}

/// `<root>/plans` 中以该 Loop 结尾的计划文件正文。
fn plan_files(root: &Path, loop_id: LoopId) -> Vec<String> {
    let mut contents = plan_file_paths(root, loop_id)
        .into_iter()
        .map(|path| plan_body(&std::fs::read_to_string(path).unwrap()))
        .collect::<Vec<_>>();
    contents.sort();
    contents
}

fn plan_of(snapshot: &ChatSnapshot, loop_id: LoopId) -> (u64, String) {
    let plan = snapshot
        .plans
        .iter()
        .find(|plan| plan.loop_id == loop_id)
        .unwrap_or_else(|| panic!("Loop {loop_id} has no plan"));
    (plan.version, plan.markdown.clone())
}

#[tokio::test]
async fn two_loops_keep_separate_plans_and_progress_and_approve_by_version_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("a-plan-1", "Plan A, first draft."),
        todo_write("a-progress-1", "Research A", "Researching A", "in_progress"),
        text("A is planned."),
        submit_plan("b-plan-1", "Plan B, first draft."),
        submit_plan("b-plan-2", "Plan B, second draft."),
        todo_write("b-progress-1", "Research B", "Researching B", "completed"),
        text("B is planned."),
        submit_plan("a-plan-2", "Plan A, second draft."),
        todo_write("a-progress-2", "Research A", "Researching A", "completed"),
        text("A is revised."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let first = run_turn(&agent, chat, None, "Plan goal A").await;
    let second = run_turn(&agent, chat, None, "Plan goal B").await;
    run_turn(&agent, chat, Some(first.loop_id), "Revise goal A").await;
    let before = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        plan_of(&before, first.loop_id),
        (2, "Plan A, second draft.".into())
    );
    assert_eq!(
        plan_of(&before, second.loop_id),
        (2, "Plan B, second draft.".into())
    );
    agent.shutdown().await;
    drop(agent);
    assert_eq!(plan_files(&root, first.loop_id), ["Plan A, second draft."]);
    assert_eq!(plan_files(&root, second.loop_id), ["Plan B, second draft."]);

    let model = Arc::new(ScriptedModel::new(vec![
        // After a restart the plan file must be read before it is replaced.
        read_plan("a-read-3"),
        submit_plan("a-plan-3", "Plan A, third draft."),
        text("A is revised again."),
    ]));
    let restarted = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(plan_state(&restored), plan_state(&before));
    assert!(
        restarted
            .approve_plan(chat, first.loop_id, 1, ExecutionMode::AcceptEdits)
            .await
            .is_err()
    );
    restarted
        .approve_plan(chat, first.loop_id, 2, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    restarted
        .approve_plan(chat, second.loop_id, 2, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    enter_plan_mode(&restarted, chat).await;
    run_turn(&restarted, chat, Some(first.loop_id), "Revise goal A again").await;
    let revised = restarted.snapshot(chat).await.unwrap();
    assert_eq!(
        plan_of(&revised, first.loop_id),
        (3, "Plan A, third draft.".into())
    );
    assert_eq!(
        plan_of(&revised, second.loop_id),
        (2, "Plan B, second draft.".into())
    );
    restarted.shutdown().await;
    drop(restarted);

    let again =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = again.resume_chat(chat).await.unwrap();
    assert_eq!(plan_state(&restored), plan_state(&revised));
    assert!(
        again
            .approve_plan(chat, first.loop_id, 2, ExecutionMode::AcceptEdits)
            .await
            .is_err()
    );
    again
        .approve_plan(chat, first.loop_id, 3, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    again.shutdown().await;
}

#[tokio::test]
async fn branched_and_forked_chats_revise_independent_plan_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan-1", "Shared plan."),
        todo_write(
            "progress-1",
            "Shared step",
            "Taking the shared step",
            "in_progress",
        ),
        text("Planned."),
        text("Paused."),
        // The Branch has not read its copy of the plan file yet.
        read_plan("child-read"),
        submit_plan("child-plan", "Child plan."),
        todo_write(
            "child-progress",
            "Child step",
            "Taking the child step",
            "completed",
        ),
        text("Child revised."),
        submit_plan("source-plan", "Source plan."),
        text("Source revised."),
        text("Paused again."),
        read_plan("fork-read"),
        submit_plan("fork-plan", "Fork plan."),
        text("Fork revised."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let source = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, source).await;
    let receipt = run_turn(&agent, source, None, "Plan the shared goal").await;
    let goal = receipt.loop_id;
    // A goal waiting for plan approval cannot be branched.
    leave_plan_mode(&agent, source, goal).await;
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            source_loop_id: None,
            name: None,
        })
        .await
        .unwrap();
    let child = branch.child_chat_id;
    assert_eq!(plan_of(&branch.snapshot, goal), (1, "Shared plan.".into()));
    assert_eq!(
        plan_state(&branch.snapshot)[0].2,
        ["Shared step:Taking the shared step:InProgress"]
    );
    enter_plan_mode(&agent, child).await;
    run_turn(&agent, child, Some(goal), "Revise in the Branch").await;
    enter_plan_mode(&agent, source).await;
    run_turn(&agent, source, Some(goal), "Revise in the source").await;
    leave_plan_mode(&agent, source, goal).await;
    let second_branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            source_loop_id: None,
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(
        plan_of(&second_branch.snapshot, goal),
        (2, "Source plan.".into())
    );
    let copy = agent.resume_as_new_chat(source).await.unwrap();
    assert_eq!(plan_of(&copy, goal), (2, "Source plan.".into()));
    enter_plan_mode(&agent, copy.id).await;
    run_turn(&agent, copy.id, Some(goal), "Revise in the fork").await;

    let source_before = agent.snapshot(source).await.unwrap();
    let child_before = agent.snapshot(child).await.unwrap();
    let copy_before = agent.snapshot(copy.id).await.unwrap();
    let second_branch_before = agent.snapshot(second_branch.child_chat_id).await.unwrap();
    assert_eq!(plan_of(&source_before, goal), (2, "Source plan.".into()));
    assert_eq!(plan_of(&child_before, goal), (2, "Child plan.".into()));
    assert_eq!(plan_of(&copy_before, goal), (3, "Fork plan.".into()));
    agent.shutdown().await;
    drop(agent);
    assert_eq!(
        plan_files(&root, goal),
        ["Child plan.", "Fork plan.", "Source plan.", "Source plan."]
    );

    let restarted =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    for before in [
        &source_before,
        &child_before,
        &copy_before,
        &second_branch_before,
    ] {
        let restored = restarted.resume_chat(before.id).await.unwrap();
        assert_eq!(
            plan_state(&restored),
            plan_state(before),
            "Chat {}",
            before.id
        );
    }
    restarted
        .approve_plan(child, goal, 2, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    restarted
        .approve_plan(copy.id, goal, 3, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    restarted.shutdown().await;
}

#[tokio::test]
async fn rewinding_keeps_the_current_plan_version_and_restart_agrees() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan-1", "First plan."),
        text("Planned."),
        submit_plan("plan-2", "Second plan."),
        todo_write("progress", "Revise", "Revising", "in_progress"),
        text("Revised."),
        text("Continued after the rewind."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let goal = run_turn(&agent, chat, None, "Plan it").await.loop_id;
    run_turn(&agent, chat, Some(goal), "Revise it").await;
    let point = agent
        .snapshot(chat)
        .await
        .unwrap()
        .rewind_points()
        .into_iter()
        .find(|point| point.prompt == "Revise it")
        .unwrap();
    agent
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    run_turn(&agent, chat, Some(goal), "Continue it").await;
    let before = agent.snapshot(chat).await.unwrap();
    // 计划文件不随 rewind 恢复（照 Claude Code），版本保持单调，批准不会复用旧版本号。
    assert_eq!(plan_of(&before, goal), (2, "Second plan.".into()));
    agent.shutdown().await;
    drop(agent);

    let restarted =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(plan_state(&restored), plan_state(&before));
    restarted
        .approve_plan(chat, goal, 2, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    restarted.shutdown().await;
}

/// 在 `root` 中打开 Agent，新建 Chat 并按顺序运行各轮（每轮沿用第一轮的 Loop），关闭后返回
/// Chat 与 Loop。
async fn run_and_shut_down(
    root: &Path,
    workdir: &Path,
    replies: Vec<Vec<Content>>,
    prompts: &[&str],
) -> (ChatId, LoopId) {
    let agent = Agent::open_embedded(
        root.to_path_buf(),
        config(),
        Arc::new(ScriptedModel::new(replies)),
    )
    .unwrap();
    let chat = agent.create_chat(workdir.into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let mut goal = None;
    for prompt in prompts {
        goal = Some(run_turn(&agent, chat, goal, prompt).await.loop_id);
    }
    agent.shutdown().await;
    drop(agent);
    (chat, goal.unwrap())
}

fn only_plan_file(root: &Path, loop_id: LoopId) -> PathBuf {
    let paths = plan_file_paths(root, loop_id);
    assert_eq!(paths.len(), 1, "{paths:?}");
    paths.into_iter().next().unwrap()
}

fn plan_slug(path: &Path, loop_id: LoopId) -> String {
    path.file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_suffix(&format!("-loop-{loop_id}.md"))
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn a_plan_written_before_its_result_was_recorded_is_restored_as_a_new_unapproved_version() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (chat, goal) = run_and_shut_down(
        &root,
        directory.path(),
        vec![
            submit_plan("plan-1", "First plan."),
            text("Planned."),
            submit_plan("plan-2", "Reviewed plan."),
            text("Revised."),
            submit_plan("plan-3", "Unreviewed plan."),
            text("Revised again."),
        ],
        &["Plan it", "Revise it", "Revise it again"],
    )
    .await;
    // 模拟计划文件已写入第 3 版、进程在结果写入 Transcript 之前崩溃：链上只有第 2 版。
    truncate_transcript_before(&transcript_file(&root, chat), |line| {
        is_tool_result_line(line, "plan-3")
    });

    let model = Arc::new(ScriptedModel::new(vec![
        read_plan("read-after-crash"),
        submit_plan("plan-5", "Plan after the crash."),
        text("Revised after the crash."),
    ]));
    let restarted = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    let (version, markdown) = plan_of(&restored, goal);
    assert_eq!(markdown, "Unreviewed plan.");
    assert!(version > 3, "restored version {version} was already used");
    assert!(!restarted.transcript_load_warnings(chat).is_empty());
    for stale in [2, 3] {
        assert!(
            restarted
                .approve_plan(chat, goal, stale, ExecutionMode::AcceptEdits)
                .await
                .is_err(),
            "version {stale} must not approve content that was not reviewed under it"
        );
    }
    enter_plan_mode(&restarted, chat).await;
    run_turn(&restarted, chat, Some(goal), "Revise after the crash").await;
    let revised = restarted.snapshot(chat).await.unwrap();
    assert_eq!(
        plan_of(&revised, goal),
        (version + 1, "Plan after the crash.".into())
    );
    restarted.shutdown().await;
    drop(restarted);

    let again =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = again.resume_chat(chat).await.unwrap();
    assert_eq!(plan_state(&restored), plan_state(&revised));
    again
        .approve_plan(chat, goal, version + 1, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    again.shutdown().await;
}

#[tokio::test]
async fn a_plan_file_edited_outside_the_conversation_is_restored_as_a_new_unapproved_version() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (chat, goal) = run_and_shut_down(
        &root,
        directory.path(),
        vec![
            submit_plan("plan-1", "First plan."),
            text("Planned."),
            submit_plan("plan-2", "Reviewed plan."),
            text("Revised."),
        ],
        &["Plan it", "Revise it"],
    )
    .await;
    let path = only_plan_file(&root, goal);
    let file_text = std::fs::read_to_string(&path).unwrap();
    let header = file_text.split_once('\n').unwrap().0;
    std::fs::write(&path, format!("{header}\nEdited plan.")).unwrap();

    let restarted =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    let (version, markdown) = plan_of(&restored, goal);
    assert_eq!(markdown, "Edited plan.");
    assert!(version > 2, "restored version {version} was already used");
    assert!(!restarted.transcript_load_warnings(chat).is_empty());
    assert!(
        restarted
            .approve_plan(chat, goal, 2, ExecutionMode::AcceptEdits)
            .await
            .is_err()
    );
    restarted
        .approve_plan(chat, goal, version, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    restarted.shutdown().await;
}

#[tokio::test]
async fn a_missing_plan_file_keeps_the_version_sequence_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (chat, goal) = run_and_shut_down(
        &root,
        directory.path(),
        vec![
            submit_plan("plan-1", "First plan."),
            text("Planned."),
            submit_plan("plan-2", "Second plan."),
            text("Revised."),
        ],
        &["Plan it", "Revise it"],
    )
    .await;
    std::fs::remove_file(only_plan_file(&root, goal)).unwrap();

    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan-3", "Recreated plan."),
        text("Recreated."),
    ]));
    let restarted = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert!(restored.plans.iter().all(|plan| plan.loop_id != goal));
    assert!(!restarted.transcript_load_warnings(chat).is_empty());
    assert!(
        restarted
            .approve_plan(chat, goal, 2, ExecutionMode::AcceptEdits)
            .await
            .is_err()
    );
    enter_plan_mode(&restarted, chat).await;
    run_turn(&restarted, chat, Some(goal), "Recreate the plan").await;
    let revised = restarted.snapshot(chat).await.unwrap();
    assert_eq!(plan_of(&revised, goal), (3, "Recreated plan.".into()));
    restarted.shutdown().await;
    drop(restarted);

    let again =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let restored = again.resume_chat(chat).await.unwrap();
    assert_eq!(plan_state(&restored), plan_state(&revised));
    again
        .approve_plan(chat, goal, 3, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    again.shutdown().await;
}

#[tokio::test]
async fn plan_files_are_replaced_whole_and_a_failed_write_keeps_the_previous_version() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan-1", "First plan."),
        text("Planned."),
        submit_plan("plan-2", "Second plan."),
        text("Revised."),
        submit_plan("plan-3", "Plan that cannot be saved."),
        text("Tried to revise."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    enter_plan_mode(&agent, chat).await;
    let goal = run_turn(&agent, chat, None, "Plan it").await.loop_id;
    let path = only_plan_file(&root, goal);
    let first_file = directory.path().join("first-plan-link");
    std::fs::hard_link(&path, &first_file).unwrap();

    run_turn(&agent, chat, Some(goal), "Revise it").await;
    // 新内容写入新文件后替换，旧文件的其他链接看到的仍是完整的旧内容。
    assert_eq!(
        plan_body(&std::fs::read_to_string(&first_file).unwrap()),
        "First plan."
    );
    assert_eq!(plan_files(&root, goal), ["Second plan."]);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let plans = root.join("plans");
    std::fs::set_permissions(&plans, std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, Some(goal), "Revise it again".into())
        .await
        .unwrap();
    let mut failure = None;
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolCompleted { call_id, result } if call_id == "plan-3" => {
                assert!(result.is_error, "a plan that cannot be saved must fail");
                failure = Some(result.content);
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let failure = failure.expect("the unsaved plan was not reported");
    assert!(failure.contains("cannot save the plan"), "{failure}");
    std::fs::set_permissions(&plans, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        plan_of(&agent.snapshot(chat).await.unwrap(), goal),
        (2, "Second plan.".into())
    );
    let names = std::fs::read_dir(&plans)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(names, [path.file_name().unwrap()]);
    assert_eq!(plan_files(&root, goal), ["Second plan."]);
    agent.shutdown().await;
}

#[tokio::test]
async fn tampered_plan_slugs_cannot_reach_outside_the_plans_directory() {
    let directory = tempfile::tempdir().unwrap();
    for (index, tampered) in ["../x", "ABSOLUTE", "Bad/Slug", "UPPER-case", ""]
        .into_iter()
        .enumerate()
    {
        let root = directory.path().join(format!("root-{index}"));
        let outside = directory.path().join(format!("outside-{index}"));
        std::fs::create_dir(&outside).unwrap();
        let absolute = outside.join("evil").to_str().unwrap().to_owned();
        let tampered = if tampered == "ABSOLUTE" {
            absolute.as_str()
        } else {
            tampered
        };
        let (chat, goal) = run_and_shut_down(
            &root,
            directory.path(),
            vec![submit_plan("plan-1", "Original plan."), text("Planned.")],
            &["Plan it"],
        )
        .await;
        let original = only_plan_file(&root, goal);
        let original_text = std::fs::read_to_string(&original).unwrap();
        let slug = plan_slug(&original, goal);
        let transcript = transcript_file(&root, chat);
        let recorded = format!("\"slug\":\"{slug}\"");
        let transcript_text = std::fs::read_to_string(&transcript).unwrap();
        assert!(transcript_text.contains(&recorded));
        std::fs::write(
            &transcript,
            transcript_text.replace(
                &recorded,
                &format!("\"slug\":{}", serde_json::to_string(tampered).unwrap()),
            ),
        )
        .unwrap();
        // 按篡改后的 slug 直接拼接路径时会读到的计划文件。
        let decoy = root
            .join("plans")
            .join(format!("{tampered}-loop-{goal}.md"));
        let decoy_text = original_text.replace("Original plan.", "Injected plan.");
        let has_decoy = decoy.parent().is_some_and(Path::is_dir);
        if has_decoy {
            std::fs::write(&decoy, &decoy_text).unwrap();
        }

        let model = Arc::new(ScriptedModel::new(vec![
            submit_plan("plan-2", "Replacement plan."),
            text("Replaced."),
        ]));
        let restarted = Agent::open_embedded(root.clone(), config(), model).unwrap();
        let restored = restarted.resume_chat(chat).await.unwrap();
        assert!(
            restored
                .plans
                .iter()
                .all(|plan| plan.markdown != "Injected plan."),
            "{tampered:?}"
        );
        assert!(
            !restarted.transcript_load_warnings(chat).is_empty(),
            "{tampered:?}"
        );
        enter_plan_mode(&restarted, chat).await;
        run_turn(&restarted, chat, Some(goal), "Replace the plan").await;
        assert_eq!(
            plan_of(&restarted.snapshot(chat).await.unwrap(), goal),
            (2, "Replacement plan.".into()),
            "{tampered:?}"
        );
        restarted.shutdown().await;
        drop(restarted);

        let replacements = plan_file_paths(&root, goal)
            .into_iter()
            .filter(|path| *path != original && *path != decoy)
            .collect::<Vec<_>>();
        let [replacement] = replacements.as_slice() else {
            panic!("{tampered:?}: {replacements:?}");
        };
        let new_slug = plan_slug(replacement, goal);
        assert!(
            new_slug != slug
                && new_slug
                    .split('-')
                    .all(|word| !word.is_empty() && word.bytes().all(|b| b.is_ascii_lowercase())),
            "{tampered:?}: {new_slug:?}"
        );
        assert_eq!(
            plan_body(&std::fs::read_to_string(replacement).unwrap()),
            "Replacement plan."
        );
        assert_eq!(std::fs::read_to_string(&original).unwrap(), original_text);
        assert!(!root.join("plans/Bad").exists());
        for entry in std::fs::read_dir(&outside).unwrap() {
            assert_eq!(entry.unwrap().path(), decoy, "{tampered:?}");
        }
        if has_decoy {
            assert_eq!(std::fs::read_to_string(&decoy).unwrap(), decoy_text);
        }
    }
}

#[tokio::test]
async fn symbolic_link_oversized_or_shared_plan_files_are_not_read() {
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("outside.md");
    for case in ["symbolic link", "oversized", "shared directory"] {
        let root = directory.path().join(case.replace(' ', "-"));
        let (chat, goal) = run_and_shut_down(
            &root,
            directory.path(),
            vec![submit_plan("plan-1", "Original plan."), text("Planned.")],
            &["Plan it"],
        )
        .await;
        let path = only_plan_file(&root, goal);
        let original_text = std::fs::read_to_string(&path).unwrap();
        match case {
            "symbolic link" => {
                std::fs::write(&outside, &original_text).unwrap();
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(&outside, &path).unwrap();
            }
            "oversized" => {
                std::fs::write(&path, format!("{original_text}{}", "x".repeat(70 * 1024))).unwrap();
            }
            _ => {
                std::fs::set_permissions(
                    root.join("plans"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
        }

        let model = Arc::new(ScriptedModel::new(vec![
            submit_plan("plan-2", "Replacement plan."),
            text("Replaced."),
        ]));
        let restarted = Agent::open_embedded(root.clone(), config(), model).unwrap();
        let restored = restarted.resume_chat(chat).await.unwrap();
        assert!(
            restored.plans.iter().all(|plan| plan.loop_id != goal),
            "{case}"
        );
        assert!(
            !restarted.transcript_load_warnings(chat).is_empty(),
            "{case}"
        );
        if case == "symbolic link" {
            // Writing through the link would edit another file, so Plan mode refuses it and
            // ExitPlanMode does not follow the link either.
            enter_plan_mode(&restarted, chat).await;
            let results = run_turn_results(&restarted, chat, Some(goal), "Replace the plan").await;
            assert!(result_of(&results, "plan-2-write").is_error);
            let submitted = result_of(&results, "plan-2");
            assert!(submitted.is_error);
            assert!(
                submitted.content.starts_with("cannot read the plan file"),
                "{}",
                submitted.content
            );
            assert!(
                restarted
                    .snapshot(chat)
                    .await
                    .unwrap()
                    .plans
                    .iter()
                    .all(|plan| plan.loop_id != goal)
            );
            assert_eq!(std::fs::read_to_string(&outside).unwrap(), original_text);
            assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
        }
        restarted.shutdown().await;
    }
}
