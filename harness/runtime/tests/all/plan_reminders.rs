//! Plan mode instructions reach the model as host notes at the end of the conversation, never in
//! the instructions, following Claude Code 2.1.302's `plan_mode`, `plan_mode_reentry` and
//! `plan_mode_exit` attachments; the task tools get Claude Code's occasional `task_reminder`.

use std::sync::Arc;

use serde_json::json;
use tickloop_harness_model::ModelRequest;
use tickloop_harness_protocol::{ChatId, Content, EventKind, ExecutionMode, PermissionRequest};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::{
    ScriptedModel, call, config, exit_plan_mode, host_note_texts, next_event, reminded_plan_file,
    submit_plan, text, write_plan,
};

const FULL: &str = "## Plan Workflow";
const SPARSE: &str = "Plan mode still active (see full instructions earlier in conversation).";
const REENTRY: &str = "## Re-entering Plan Mode";
const EXITED: &str = "## Exited Plan Mode";
const TASK_REMINDER: &str = "The task tools haven't been used recently.";

/// Submits `prompt` and drives it to completion, handing each permission request to `answer`.
async fn run_turn(
    agent: &Agent,
    chat: ChatId,
    prompt: &str,
    mut answer: impl AsyncFnMut(PermissionRequest),
) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => answer(request).await,
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

async fn no_requests(request: PermissionRequest) {
    panic!("unexpected permission request {}", request.name);
}

fn tool_result(request: &ModelRequest, call_id: &str) -> (String, bool) {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == call_id => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {call_id}"))
}

/// The notes a request carries beyond those of the request before it.
fn new_notes(requests: &[ModelRequest], index: usize) -> Vec<String> {
    let before = index
        .checked_sub(1)
        .map_or(0, |previous| host_note_texts(&requests[previous]).len());
    host_note_texts(&requests[index])[before..].to_vec()
}

fn kinds(notes: &[String]) -> Vec<&'static str> {
    notes
        .iter()
        .filter_map(|note| {
            [FULL, SPARSE, REENTRY, EXITED, TASK_REMINDER]
                .into_iter()
                .find(|marker| note.contains(marker))
        })
        .collect()
}

/// The request prefix (system message, user context and tools) stays byte for byte the same
/// while the Chat enters Plan through EnterPlanMode, writes its plan file, submits, is declined, submits again and is
/// approved: the plan, its mode and every reminder live in the conversation.
#[tokio::test]
async fn the_request_prefix_stays_byte_identical_through_a_plan_cycle() {
    let model = Arc::new(ScriptedModel::new(vec![
        text("Before planning."),
        call("enter", "EnterPlanMode", json!({})),
        write_plan("draft", "1. Draft."),
        submit_plan("first", "1. Read.\n2. Write."),
        submit_plan("second", "1. Read.\n2. Write concisely."),
        text("Implementing."),
        text("After planning."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    run_turn(&agent, chat, "Look around", no_requests).await;
    let agent_ref = &agent;
    run_turn(&agent, chat, "Plan the report", async |request| {
        let version = request.input["plan_version"].as_u64().unwrap();
        if version == 1 {
            agent_ref
                .decline_plan(chat, 1, Some("Be concise".into()))
                .await
                .unwrap();
        } else {
            agent_ref
                .approve_plan(chat, version, ExecutionMode::AcceptEdits)
                .await
                .unwrap();
        }
    })
    .await;
    run_turn(&agent, chat, "Anything else?", no_requests).await;

    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 7);
    let prefix = |request: &ModelRequest| {
        serde_json::to_vec(&(
            &request.messages[..2],
            &request.tools,
            &request.deferred_tools,
        ))
        .unwrap()
    };
    for (index, request) in requests.iter().enumerate() {
        assert!(
            prefix(request) == prefix(&requests[0]),
            "request {index} changed the prefix"
        );
    }
    assert!(new_notes(&requests, 1).is_empty());
    assert_eq!(kinds(&new_notes(&requests, 2)), [FULL]);
    assert!(new_notes(&requests, 3).is_empty());
    // Like Claude Code, the approval leaves Plan, so the next request says so once.
    assert_eq!(kinds(&new_notes(&requests, 5)), [EXITED]);
    assert!(new_notes(&requests, 6).is_empty());
    let (approved, is_error) = tool_result(&requests[5], "second");
    assert!(!is_error);
    assert!(approved.starts_with(
        "User has approved your plan. You can now start coding.\n\nYour plan has been saved to: "
    ));
    assert!(approved.ends_with("## Approved Plan:\n1. Read.\n2. Write concisely."));
    agent.shutdown().await;
}

/// A reminder at most every five user turns while in Plan: the first in full, then sparse ones
/// naming the plan file.
#[tokio::test]
async fn plan_reminders_follow_claude_codes_five_turn_cadence() {
    let model = Arc::new(ScriptedModel::new(
        (0..12).map(|turn| text(&format!("Turn {turn}."))).collect(),
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    for turn in 0..12 {
        run_turn(&agent, chat, &format!("Prompt {turn}"), no_requests).await;
    }

    let requests = model.requests.lock().unwrap().clone();
    let reminded = (0..requests.len())
        .map(|index| kinds(&new_notes(&requests, index)))
        .collect::<Vec<_>>();
    let mut expected = vec![Vec::<&str>::new(); 12];
    expected[0] = vec![FULL];
    expected[5] = vec![SPARSE];
    expected[10] = vec![SPARSE];
    assert_eq!(reminded, expected);
    let plan_file = reminded_plan_file(&requests[0]).unwrap();
    assert_eq!(reminded_plan_file(&requests[5]).unwrap(), plan_file);
    agent.shutdown().await;
}

/// Approving a plan leaves Plan like any other way out: the next request tells the model once.
/// Returning to Plan with that plan file then leads the full reminder with Claude Code's re-entry
/// notice.
#[tokio::test]
async fn returning_to_plan_after_an_approved_plan_announces_the_reentry() {
    let model = Arc::new(ScriptedModel::new(vec![
        submit_plan("plan", "1. Ship it."),
        text("Implementing."),
        text("Next."),
        text("Planning again."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let agent_ref = &agent;
    run_turn(&agent, chat, "Plan it", async |request| {
        agent_ref
            .approve_plan(
                chat,
                request.input["plan_version"].as_u64().unwrap(),
                ExecutionMode::Auto,
            )
            .await
            .unwrap();
    })
    .await;
    run_turn(&agent, chat, "Go on", no_requests).await;
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    run_turn(&agent, chat, "Plan something else", no_requests).await;

    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert_eq!(kinds(&new_notes(&requests, 1)), [EXITED]);
    assert!(new_notes(&requests, 2).is_empty());
    let reentry = new_notes(&requests, 3);
    assert_eq!(reentry.len(), 1, "{reentry:?}");
    let plan_file = reminded_plan_file(&requests[0]).unwrap();
    let reentry = &reentry[0];
    assert!(reentry.contains(&format!(
        "{REENTRY}\n\nYou are returning to plan mode after having previously exited it. A plan file exists at {plan_file} from your previous planning session."
    )));
    assert!(reentry.contains(FULL));
    assert!(reentry.contains(&format!("A plan file already exists at {plan_file}.")));
    agent.shutdown().await;
}

/// ExitPlanMode without a plan asks whether to leave Plan: yes restores the mode Plan was
/// entered from and lets the model proceed; no keeps Plan.
#[tokio::test]
async fn exit_plan_mode_without_a_plan_asks_to_leave_plan_mode() {
    let model = Arc::new(ScriptedModel::new(vec![
        exit_plan_mode("kept"),
        text("Staying."),
        write_plan("blank", "  \n"),
        exit_plan_mode("left"),
        text("Proceeding."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let agent_ref = &agent;
    let asked = std::cell::RefCell::new(Vec::new());
    let answer = async |allow: bool, request: PermissionRequest| {
        asked.borrow_mut().push(request.input.clone());
        agent_ref
            .answer_permission_with_comment(
                chat,
                request.id,
                allow,
                tickloop_harness_protocol::PermissionScope::Once,
                (!allow).then(|| "Keep going".into()),
            )
            .await
            .unwrap();
    };
    run_turn(&agent, chat, "Plan it", async |request| {
        answer(false, request).await
    })
    .await;
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    run_turn(&agent, chat, "Never mind", async |request| {
        answer(true, request).await
    })
    .await;

    assert_eq!(
        *asked.borrow(),
        [
            json!({"exit_without_plan": true}),
            json!({"exit_without_plan": true})
        ]
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.policy.mode, ExecutionMode::FullAccess);
    assert!(snapshot.plans.is_empty());
    let requests = model.requests.lock().unwrap().clone();
    let (kept, is_error) = tool_result(&requests[1], "kept");
    assert!(is_error);
    assert_eq!(
        kept,
        "User declined to exit plan mode. Stay in plan mode and keep planning.\n\nUser feedback: Keep going"
    );
    assert_eq!(
        tool_result(&requests[4], "left"),
        (
            "User has approved exiting plan mode. You can now proceed.".into(),
            false
        )
    );
    agent.shutdown().await;
}

/// Outside Plan, ExitPlanMode tells the model to enter Plan first, as Claude Code 2.1.302 does.
#[tokio::test]
async fn exit_plan_mode_outside_plan_points_to_enter_plan_mode() {
    let model = Arc::new(ScriptedModel::new(vec![
        exit_plan_mode("exit"),
        text("Continuing."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run_turn(&agent, chat, "Write it", no_requests).await;
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(
        tool_result(&requests[1], "exit"),
        (
            "You are not in plan mode. To enter plan mode, call the EnterPlanMode tool first. If your plan was already approved, continue with implementation.".into(),
            true
        )
    );
    agent.shutdown().await;
}

/// A side effect Plan refuses tells the model it is planning, where its plan file is and how to
/// finish.
#[tokio::test]
async fn a_refused_write_in_plan_mode_guides_the_model_to_its_plan_file() {
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "write",
            "Write",
            json!({"file_path": "notes.md", "content": "x"}),
        ),
        text("Planning."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    run_turn(&agent, chat, "Plan it", no_requests).await;

    let requests = model.requests.lock().unwrap().clone();
    let plan_file = reminded_plan_file(&requests[0]).unwrap();
    let (refused, is_error) = tool_result(&requests[1], "write");
    assert!(is_error);
    assert!(refused.starts_with(&format!("Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits (with the exception of the plan file at {plan_file})")), "{refused}");
    assert!(refused.ends_with(&format!(
        "write your plan to {plan_file}, and call ExitPlanMode when it is ready for approval."
    )));
    assert!(!directory.path().join("notes.md").exists());
    agent.shutdown().await;
}

/// With the task tools on, ten replies without creating or updating a task bring Claude Code's
/// task reminder once; with them off, never.
#[tokio::test]
async fn the_task_reminder_follows_ten_replies_without_task_writes() {
    for enabled in [true, false] {
        let mut replies = (0..12)
            .map(|reply| call(&format!("glob-{reply}"), "Glob", json!({"pattern": "*.md"})))
            .collect::<Vec<_>>();
        replies.push(text("Done."));
        let model = Arc::new(ScriptedModel::new(replies));
        let agent = Agent::new_embedded(config(), model.clone());
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent.set_task_tools_enabled(chat, enabled).await.unwrap();
        run_turn(&agent, chat, "Look around", no_requests).await;

        let requests = model.requests.lock().unwrap().clone();
        let reminded = (0..requests.len())
            .filter(|index| kinds(&new_notes(&requests, *index)) == [TASK_REMINDER])
            .collect::<Vec<_>>();
        if enabled {
            assert_eq!(reminded, [10]);
        } else {
            assert!(reminded.is_empty());
        }
        let system = serde_json::to_vec(&requests[0].messages[0]).unwrap();
        for request in &requests {
            assert!(serde_json::to_vec(&request.messages[0]).unwrap() == system);
        }
        agent.shutdown().await;
    }
}

/// Like Claude Code 2.1.302, the Loop's plan file stays writable without asking after the Chat
/// leaves Plan, in every mode.
#[tokio::test]
async fn the_plan_file_stays_writable_without_asking_outside_plan_mode() {
    for mode in [ExecutionMode::AcceptEdits, ExecutionMode::Auto] {
        let model = Arc::new(ScriptedModel::new(vec![
            write_plan("draft", "1. Draft."),
            text("Drafted."),
        ]));
        let agent = Agent::new_embedded(config(), model.clone());
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Plan)
            .await
            .unwrap();
        run_turn(&agent, chat, "Plan it", no_requests).await;
        let plan_file = reminded_plan_file(&model.requests.lock().unwrap()[0]).unwrap();
        model.enqueue(call(
            "revise",
            "Write",
            json!({"file_path": plan_file, "content": "1. Revised."}),
        ));
        model.enqueue(text("Revised."));
        agent.set_execution_mode(chat, mode).await.unwrap();
        run_turn(&agent, chat, "Revise the plan", no_requests).await;

        let requests = model.requests.lock().unwrap().clone();
        assert!(!tool_result(&requests[3], "revise").1, "{mode:?}");
        assert_eq!(std::fs::read_to_string(plan_file).unwrap(), "1. Revised.");
        agent.shutdown().await;
    }
}

/// Declining to leave without feedback ends the Run in Plan, and a later yes still restores the
/// mode Plan was entered from.
#[tokio::test]
async fn a_bare_no_ends_the_run_and_a_later_yes_restores_the_earlier_mode() {
    let model = Arc::new(ScriptedModel::new(vec![
        exit_plan_mode("kept"),
        exit_plan_mode("left"),
        text("Proceeding."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Plan it".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, false)
                .await
                .unwrap(),
            EventKind::Interrupted => break,
            EventKind::Completed { .. } => panic!("a bare no ends the Run"),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    let agent_ref = &agent;
    run_turn(&agent, chat, "Never mind", async |request| {
        agent_ref
            .answer_permission(chat, request.id, true)
            .await
            .unwrap();
    })
    .await;
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::FullAccess
    );
    agent.shutdown().await;
}

/// Reads name the mode Plan was entered from, which the plan approval continues in, only while
/// the Chat is in Plan.
#[tokio::test]
async fn reads_name_the_mode_plan_was_entered_from_while_in_plan() {
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    for mode in [ExecutionMode::FullAccess, ExecutionMode::Plan] {
        agent.set_execution_mode(chat, mode).await.unwrap();
    }
    assert_eq!(
        agent.status(chat).await.unwrap().mode_before_plan,
        Some(ExecutionMode::FullAccess)
    );
    assert_eq!(
        agent.snapshot(chat).await.unwrap().mode_before_plan,
        Some(ExecutionMode::FullAccess)
    );
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    assert_eq!(agent.status(chat).await.unwrap().mode_before_plan, None);
    assert_eq!(agent.snapshot(chat).await.unwrap().mode_before_plan, None);
    agent.shutdown().await;
}
