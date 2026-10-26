use std::sync::Arc;

use serde_json::json;
use tickloop_code_settings::UserSettings;
use tickloop_harness::protocol::{
    AgentHostOptions, ExecutionMode, PromptPolicy, QuestionAnswer, SharedTaskStatus,
};
use tickloop_harness::runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, QuestionReply, ToolPolicy,
    TurnOp, TurnOutcome, TurnRequest, TurnWait, UserMessage, WaitHosting,
    environment::FolderEnvironment, transcript::TranscriptStore,
};
use tickloop_harness::test_support::{
    ScriptedModel, call, config,
    harness::ChosenModels,
    routed::{RoutedModel, has_tool_result, tool_result},
    single_question, text,
    transcripts::InMemoryTranscriptStore,
};
use uuid::Uuid;

fn options(task_tools: Option<bool>, waits: WaitHosting) -> HarnessOptions {
    HarnessOptions {
        host: AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            execution_mode_override: Some(ExecutionMode::FullAccess),
            ..AgentHostOptions::product_defaults()
        },
        tools: ToolPolicy {
            tasks: true,
            questions: true,
            ..ToolPolicy::web_fetch_only()
        },
        task_tools,
        waits,
        ..HarnessOptions::local(config())
    }
}

fn prompt() -> TurnRequest {
    TurnRequest {
        input_id: tickloop_ids::Id::from(Uuid::new_v4()),
        op: TurnOp::Continue {
            after: ParentSelection::CurrentLeaf,
            message: UserMessage {
                text: "Prepare the report.".into(),
                ..Default::default()
            },
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

#[tokio::test]
async fn host_switch_controls_tools_and_remains_bounded_by_capability_policy() {
    for (setting, capability, enabled) in [
        (None, true, false),
        (Some(false), true, false),
        (Some(true), true, true),
        (Some(true), false, false),
    ] {
        let model = Arc::new(ScriptedModel::new(vec![text("Done.")]));
        let mut options = options(setting, WaitHosting::Durable);
        options.tools.tasks = capability;
        let agent = Agent::with_seams(
            HarnessSeams {
                transcripts: None,
                environment: None,
                models: ChosenModels::new(model.clone()),
            },
            options,
        )
        .unwrap();
        let chat = agent.create_chat_without_folder().await.unwrap();
        let turn = agent.advance(chat, prompt()).await.unwrap();
        assert!(matches!(
            turn.outcome().await.unwrap(),
            TurnOutcome::Completed { .. }
        ));
        let names = model.requests.lock().unwrap()[0]
            .tools
            .iter()
            .filter(|tool| tool.name.starts_with("Task"))
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            names.len(),
            if enabled { 4 } else { 0 },
            "{setting:?}, {capability}"
        );
        assert_eq!(
            agent.status(chat).await.unwrap().task_tools_enabled,
            setting.unwrap_or(false)
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn local_settings_override_either_host_value_and_keep_default_off() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    for (saved, host) in [(None, true), (Some(true), false), (Some(false), true)] {
        if let Some(enabled) = saved {
            settings.save_task_tools_enabled(enabled).unwrap();
        }
        let mut options = options(Some(host), WaitHosting::Process);
        options.settings = Some(Arc::new(settings.clone()));
        let agent = Agent::with_seams(
            HarnessSeams {
                transcripts: None,
                environment: Some(Arc::new(FolderEnvironment::bare())),
                models: ChosenModels::new(Arc::new(ScriptedModel::new(vec![]))),
            },
            options,
        )
        .unwrap();
        agent
            .trust_working_directory(directory.path().into())
            .await
            .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        assert_eq!(
            agent.task_tools_setting(chat).await.unwrap(),
            saved.unwrap_or(false)
        );
        agent.shutdown().await;
    }
}

fn claiming_model() -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(|request| {
        if !has_tool_result(request, "create") {
            return call(
                "create",
                "TaskCreate",
                json!({
                    "mutation_id":Uuid::from_u128(1), "title":"Report", "description":"Prepare the authorized report"
                }),
            );
        }
        let task = &tool_result(&request.messages, "create").unwrap()["task"];
        if !has_tool_result(request, "claim") {
            return call(
                "claim",
                "TaskUpdate",
                json!({
                    "mutation_id":Uuid::from_u128(2), "task_id":task["id"], "expected_revision":1, "action":"claim"
                }),
            );
        }
        if !has_tool_result(request, "question") {
            return call(
                "question",
                "AskUserQuestion",
                single_question("Which format?"),
            );
        }
        if !has_tool_result(request, "complete") {
            return call(
                "complete",
                "TaskUpdate",
                json!({
                    "mutation_id":Uuid::from_u128(3), "task_id":task["id"], "expected_revision":2,
                    "action":"complete", "result":"Report prepared."
                }),
            );
        }
        text("Report prepared.")
    }))
}

fn harness(
    store: Arc<dyn TranscriptStore>,
    model: Arc<RoutedModel>,
    task_tools: bool,
    waits: WaitHosting,
) -> Agent {
    let mut options = options(Some(task_tools), waits);
    // Nothing of a Chat without a Folder outlives a process that keeps its waits: such a
    // harness only loads what a durable one wrote.
    if waits == WaitHosting::Process {
        options.tools = ToolPolicy::web_fetch_only();
    }
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        options,
    )
    .unwrap()
}

#[tokio::test]
async fn durable_reload_preserves_claim_board_and_receipts_and_disabled_owner_can_finish() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = claiming_model();
    let agent = harness(store.clone(), model.clone(), true, WaitHosting::Durable);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent.advance(chat, prompt()).await.unwrap();
    let TurnOutcome::Waiting(TurnWait::Question(question)) = turn.outcome().await.unwrap() else {
        panic!("the claiming run asks a question");
    };
    let before = agent.snapshot(chat).await.unwrap().task_boards;
    let board = before.values().next().unwrap();
    assert_eq!(board.tasks[0].status, SharedTaskStatus::InProgress);
    assert_eq!(
        board.tasks[0].claim.as_ref().unwrap().run_id,
        turn.receipt().run_id
    );
    assert_eq!(board.mutations.len(), 2);
    agent.shutdown().await;
    agent.sync().await.unwrap();

    let reopened = harness(store, model.clone(), false, WaitHosting::Durable);
    reopened.resume_chat(chat).await.unwrap();
    assert_eq!(reopened.snapshot(chat).await.unwrap().task_boards, before);
    assert!(!reopened.status(chat).await.unwrap().task_tools_enabled);
    let answered = reopened
        .advance(
            chat,
            TurnRequest {
                input_id: tickloop_ids::Id::from(Uuid::new_v4()),
                op: TurnOp::Answer {
                    request: question.id,
                    reply: QuestionReply::Answered(vec![QuestionAnswer::other("Weekly")]),
                },
                delivery: Delivery::Queue,
                host_context: None,
            },
        )
        .await
        .unwrap();
    answered.outcome().await.unwrap();
    reopened.settled_input(chat, turn.input_id()).await.unwrap();
    let after = reopened.snapshot(chat).await.unwrap();
    let board = after.task_boards.values().next().unwrap();
    assert_eq!(board.tasks[0].status, SharedTaskStatus::Completed);
    assert_eq!(board.mutations.len(), 3);
    {
        let requests = model.requests.lock().unwrap();
        let finishing = &requests[requests.len() - 2];
        let tools = finishing
            .tools
            .iter()
            .filter(|tool| tool.name.starts_with("Task"))
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 3);
        assert!(!tools.contains(&"TaskCreate"));
        let result = tool_result(&requests.last().unwrap().messages, "complete").unwrap();
        assert_eq!(result["status"], "applied");
    }
    reopened.shutdown().await;
}

#[tokio::test]
async fn process_reload_keeps_existing_needs_attention_behavior() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = claiming_model();
    let agent = harness(store.clone(), model.clone(), true, WaitHosting::Durable);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent.advance(chat, prompt()).await.unwrap();
    assert!(matches!(
        turn.outcome().await.unwrap(),
        TurnOutcome::Waiting(_)
    ));
    agent.sync().await.unwrap();
    // Load the durable prefix as if the process died, before shutdown settles the live claim.
    let restored_store = Arc::new(InMemoryTranscriptStore::new());
    restored_store.seed(chat, store.lines(chat), store.position(chat).unwrap());
    let reopened = harness(restored_store, model, true, WaitHosting::Process);
    reopened.resume_chat(chat).await.unwrap();
    let snapshot = reopened.snapshot(chat).await.unwrap();
    let board = snapshot.task_boards.values().next().unwrap();
    assert_eq!(board.tasks[0].status, SharedTaskStatus::NeedsAttention);
    assert_eq!(board.mutations.len(), 2);
    reopened.shutdown().await;
    agent.shutdown().await;
}

#[tokio::test]
async fn durable_reload_marks_claims_without_a_restored_owner_run_needs_attention() {
    use tickloop_harness::runtime::transcript::StorePosition;
    use tickloop_harness::transcript::Line;
    for missing_member in [false, true] {
        let source = Arc::new(InMemoryTranscriptStore::new());
        let model = claiming_model();
        let agent = harness(source.clone(), model.clone(), true, WaitHosting::Durable);
        let chat = agent.create_chat_without_folder().await.unwrap();
        let turn = agent.advance(chat, prompt()).await.unwrap();
        assert!(matches!(
            turn.outcome().await.unwrap(),
            TurnOutcome::Waiting(_)
        ));
        agent.shutdown().await;
        agent.sync().await.unwrap();
        let mut lines = source.lines(chat);
        let board = lines
            .iter_mut()
            .rev()
            .find_map(|line| match line {
                Line::TaskBoards(entry) => entry.boards.values_mut().next(),
                _ => None,
            })
            .unwrap();
        let receipts = board.mutations.clone();
        let previous_revision = board.tasks[0].revision;
        if missing_member {
            board.tasks[0].owner.as_mut().unwrap().chat_id = tickloop_ids::Id::from(Uuid::new_v4());
        } else {
            board.tasks[0].claim.as_mut().unwrap().run_id = tickloop_ids::Id::from(Uuid::new_v4());
        }
        let store = Arc::new(InMemoryTranscriptStore::new());
        store.seed(chat, lines, StorePosition(1000));
        let reopened = harness(store, model, true, WaitHosting::Durable);
        reopened.resume_chat(chat).await.unwrap();
        let snapshot = reopened.snapshot(chat).await.unwrap();
        let restored = snapshot.task_boards.values().next().unwrap();
        assert_eq!(restored.tasks[0].status, SharedTaskStatus::NeedsAttention);
        assert_eq!(restored.tasks[0].revision, previous_revision + 1);
        assert_eq!(restored.mutations, receipts);
        reopened.shutdown().await;
    }
}
