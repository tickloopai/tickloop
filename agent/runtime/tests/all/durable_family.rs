//! Durable families: a root Chat and the members it delegated load, wait and go on together.
//! A foreground call waits for its child in the Transcript, a child's end reaches its parent
//! exactly once, whichever instance it ends in, and a load settles the family's calls by its
//! records. Under process wait hosting a load ends the waits it finds, as a local Worker
//! always has.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Content, ExecutionMode, FamilyCompletion, LoopState, PromptPolicy,
    QuestionAnswer, QuestionRequest, Role, SearchConfig, SearchProviderKind,
    SubagentNotificationDelivery,
};
use tickloop_agent_runtime::transcript::{LoadRange, StorePosition, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, PersonWait, PersonWaitKind,
    QuestionReply, Quiescence, SearchDefaults, ToolPolicy, Turn, TurnOp, TurnOutcome, TurnRequest,
    TurnWait, UserMessage, WaitHosting, Waits,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::routed::{has_tool_result, tool_result};
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{ScriptedModel, call, config, single_question, text};
use tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME;
use tickloop_agent_transcript::{
    FamilyCompletionsEntry, InterruptedBy, Line, ResponseEnding, ResponseState, SettledOutcome,
    WaitKind, WaitState,
};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CHILD_TASK: &str = "child";
const CHILD_REPORT: &str = "Weekly format chosen.";
const ROOT_DONE: &str = "Report ready.";
const DELEGATION_CALL: &str = "delegate";
const QUESTION_CALL: &str = "question";

/// What the family's model does with a request.
enum Reply {
    Says(Vec<Content>),
    /// Holds the request until its Run is cancelled, as a host that dies mid-request would.
    Holds,
    After(Arc<Notify>, Vec<Content>),
}

type Route = dyn Fn(&ModelRequest) -> Reply + Send + Sync;

/// Answers each family member from its own state, so the members of one family can share it.
struct FamilyModel {
    route: Box<Route>,
    requests: Mutex<Vec<ModelRequest>>,
    holding: Notify,
}

impl FamilyModel {
    fn new(route: impl Fn(&ModelRequest) -> Reply + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            route: Box::new(route),
            requests: Mutex::new(Vec::new()),
            holding: Notify::new(),
        })
    }

    fn requests_of(&self, task: Option<&str>) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| task_of(request).as_deref() == task)
            .count()
    }
}

#[async_trait::async_trait]
impl ModelClient for FamilyModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        ScriptedModel::new(Vec::new()).identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request.clone());
        match (self.route)(&request) {
            Reply::Says(content) => Ok(ModelOutput {
                private_source: None,
                content,
                usage: Default::default(),
                stop_reason: "end_turn".into(),
            }),
            Reply::After(ready, content) => {
                tokio::select! {
                    () = ready.notified() => Ok(ModelOutput {
                        private_source: None, content, usage: Default::default(), stop_reason: "end_turn".into(),
                    }),
                    () = cancel.cancelled() => Err(ModelError::Cancelled),
                }
            }
            Reply::Holds => {
                self.holding.notify_one();
                cancel.cancelled().await;
                Err(ModelError::Cancelled)
            }
        }
    }
}

/// The task the requesting member was delegated, from the Agent message that opens its
/// conversation; `None` for the root.
fn task_of(request: &ModelRequest) -> Option<String> {
    let first = request
        .messages
        .iter()
        .find(|message| message.role != Role::System)?;
    let Content::Text { text } = first.content.first()? else {
        return None;
    };
    let body = text
        .strip_prefix("<agent-message>")?
        .split_once("</agent-message>")?
        .0;
    serde_json::from_str::<Value>(body).ok()?["message"]
        .as_str()
        .map(str::to_owned)
}

/// A root that delegates in the foreground to a child that asks the user first.
fn asking_family() -> Arc<FamilyModel> {
    FamilyModel::new(|request| match task_of(request).as_deref() {
        Some(CHILD_TASK) if !has_tool_result(request, QUESTION_CALL) => Reply::Says(call(
            QUESTION_CALL,
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Which format?"),
        )),
        Some(CHILD_TASK) => Reply::Says(text(CHILD_REPORT)),
        _ if !has_tool_result(request, DELEGATION_CALL) => Reply::Says(call(
            DELEGATION_CALL,
            "Subagent",
            json!({"task": CHILD_TASK, "name": "writer"}),
        )),
        _ => Reply::Says(text(ROOT_DONE)),
    })
}

/// A root that delegates in the foreground to a child whose first `held` requests are held.
fn held_child_family(held: usize) -> Arc<FamilyModel> {
    let child_requests = Arc::new(Mutex::new(0));
    FamilyModel::new(move |request| match task_of(request).as_deref() {
        Some(CHILD_TASK) => {
            let mut seen = child_requests.lock().unwrap();
            *seen += 1;
            if *seen <= held {
                Reply::Holds
            } else {
                Reply::Says(text(CHILD_REPORT))
            }
        }
        _ if !has_tool_result(request, DELEGATION_CALL) => Reply::Says(call(
            DELEGATION_CALL,
            "Subagent",
            json!({"task": CHILD_TASK, "name": "writer"}),
        )),
        _ => Reply::Says(text(ROOT_DONE)),
    })
}

fn harness(store: Arc<dyn TranscriptStore>, model: Arc<FamilyModel>, waits: WaitHosting) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        options(waits),
    )
    .unwrap()
}

/// A Harness without a Folder whose Chats ask questions and delegate; the host lets every
/// delegation through without review.
fn options(waits: WaitHosting) -> HarnessOptions {
    HarnessOptions {
        host: AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            execution_mode_override: Some(ExecutionMode::FullAccess),
            ..AgentHostOptions::product_defaults()
        },
        tools: match waits {
            WaitHosting::Durable => ToolPolicy {
                questions: true,
                delegation: true,
                family_messages: true,
                ..ToolPolicy::web_fetch_only()
            },
            // Nothing of a Chat without a Folder outlives a process that keeps its waits: such a
            // harness only loads what a durable one wrote.
            WaitHosting::Process => ToolPolicy::web_fetch_only(),
        },
        waits,
        ..HarnessOptions::local(config())
    }
}

fn prompt(text: &str) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Continue {
            after: ParentSelection::CurrentLeaf,
            message: UserMessage {
                text: text.into(),
                ..Default::default()
            },
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

fn answer(request: &QuestionRequest) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Answer {
            request: request.id,
            reply: QuestionReply::Answered(vec![QuestionAnswer::other("Weekly")]),
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

async fn outcome(turn: &Turn) -> TurnOutcome {
    tokio::time::timeout(std::time::Duration::from_secs(10), turn.outcome())
        .await
        .expect("the turn reaches an outcome")
        .unwrap()
}

async fn settled(agent: &Agent, chat: ChatId, input: Uuid) -> SettledOutcome {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        agent.settled_input(chat, input),
    )
    .await
    .expect("the input settles")
    .unwrap()
}

/// The member and question a turn waits on through its foreground call.
fn member_question(outcome: TurnOutcome) -> (ChatId, QuestionRequest) {
    let TurnOutcome::Waiting(TurnWait::Member { mut chat, mut wait }) = outcome else {
        panic!("the turn waits for its child: {outcome:?}");
    };
    while let TurnWait::Member {
        chat: member,
        wait: next,
    } = *wait
    {
        chat = member;
        wait = next;
    }
    let TurnWait::Question(request) = *wait else {
        panic!("the child waits for its question");
    };
    (chat, request)
}

/// Waits until the root's foreground call waits for its child in the store, as a host whose
/// holding ends there would find it.
async fn until_parent_waits(agent: &Agent, store: &InMemoryTranscriptStore, root: ChatId) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            agent.sync().await.unwrap();
            if subagent_waits(&store.lines(root))
                .iter()
                .any(|(_, state)| matches!(state, WaitState::Open))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the parent waits for its child");
}

fn response_states(lines: &[Line], run: Uuid) -> Vec<(u32, ResponseState)> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Response(entry) if entry.run_id == run => {
                Some((entry.attempt, entry.state.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The latest state of every wait on a child run, as `lines` record them.
fn subagent_waits(lines: &[Line]) -> Vec<(Uuid, WaitState)> {
    let mut waits: Vec<(Uuid, Uuid, WaitState)> = Vec::new();
    for line in lines {
        if let Line::Wait(entry) = line
            && let WaitKind::Subagent { child_run, .. } = entry.kind
        {
            waits.retain(|(id, ..)| *id != entry.wait_id);
            waits.push((entry.wait_id, child_run, entry.state.clone()));
        }
    }
    waits
        .into_iter()
        .map(|(_, child_run, state)| (child_run, state))
        .collect()
}

fn last_completions(lines: &[Line]) -> Vec<FamilyCompletion> {
    lines
        .iter()
        .rev()
        .find_map(|line| match line {
            Line::FamilyCompletions(entry) => Some(entry.completions.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn results_of(lines: &[Line], call_id: &str) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::User(entry) => Some(&entry.message.content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => serde_json::from_str(&content.text()).ok(),
            _ => None,
        })
        .collect()
}

/// What a foreground call's result says of its child's run, without the ids and times one
/// family's run has and another's does not.
fn reported(result: &Value) -> Value {
    json!({
        "status": result["status"],
        "child_state": result["child_state"],
        "result": result["result"],
        "error": result["error"],
        "handle": result["handle"],
        "description": result["description"],
        "usage": result["usage"],
    })
}

/// The family asks and answers in one instance; gives what the foreground call returned.
async fn answered_in_process() -> Value {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = harness(store, asking_family(), WaitHosting::Durable);
    let root = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    let (child, request) = member_question(outcome(&turn).await);
    let reply = agent.advance(child, answer(&request)).await.unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        settled(&agent, root, turn.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    let snapshot = agent.snapshot(root).await.unwrap();
    agent.shutdown().await;
    tool_result(&snapshot.messages, DELEGATION_CALL).unwrap()
}

/// The family asks in one instance, which then shuts down as a host ends its holding.
async fn asked_and_released(
    store: &Arc<InMemoryTranscriptStore>,
    model: Arc<FamilyModel>,
) -> (ChatId, ChatId, Turn, QuestionRequest) {
    let asking = harness(store.clone(), model, WaitHosting::Durable);
    let root = asking.create_chat_without_folder().await.unwrap();
    let turn = asking
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    let (child, request) = member_question(outcome(&turn).await);
    assert_eq!(
        asking.quiesce(root).await,
        Quiescence::Waiting(Waits {
            people: vec![PersonWait {
                chat: child,
                request: request.id,
                kind: PersonWaitKind::Question,
            }],
            deliveries: Vec::new(),
        })
    );
    asking.shutdown().await;
    asking.sync().await.unwrap();
    (root, child, turn, request)
}

#[tokio::test]
async fn subagent_search_inherits_the_direct_parent_without_following_later_root_changes() {
    const GRANDCHILD_TASK: &str = "grandchild";
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = FamilyModel::new(|request| match task_of(request).as_deref() {
        Some(CHILD_TASK) if !has_tool_result(request, QUESTION_CALL) => Reply::Says(call(
            QUESTION_CALL,
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Delegate further?"),
        )),
        Some(CHILD_TASK) => Reply::Says(call(
            "nested",
            "Subagent",
            json!({"task": GRANDCHILD_TASK, "name": "grandchild"}),
        )),
        Some(GRANDCHILD_TASK) => Reply::Says(call(
            "nested-question",
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Which source?"),
        )),
        _ => Reply::Says(call(
            DELEGATION_CALL,
            "Subagent",
            json!({"task": CHILD_TASK, "name": "child"}),
        )),
    });
    let agent = harness(store, model, WaitHosting::Durable);
    let root = agent.create_chat_without_folder().await.unwrap();
    let search = Some(SearchConfig::for_provider(SearchProviderKind::Tavily));
    agent.set_search(root, search.clone()).await.unwrap();
    let turn = agent.advance(root, prompt("Delegate.")).await.unwrap();
    let (child, question) = member_question(outcome(&turn).await);
    assert_eq!(agent.snapshot(child).await.unwrap().search_config, search);

    agent.set_search(root, None).await.unwrap();
    assert_eq!(agent.snapshot(child).await.unwrap().search_config, search);
    let reply = agent.advance(child, answer(&question)).await.unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    let (grandchild, _) = member_question(outcome(&turn).await);
    assert_ne!(grandchild, child);
    let snapshot = agent.snapshot(grandchild).await.unwrap();
    assert_eq!(snapshot.delegation.unwrap().parent_chat_id, child);
    assert_eq!(snapshot.search_config, search);
    agent.shutdown().await;
}

#[tokio::test]
async fn subagent_search_disabled_on_the_parent_stays_disabled() {
    let agent = harness(
        Arc::new(InMemoryTranscriptStore::new()),
        asking_family(),
        WaitHosting::Durable,
    );
    let root = agent.create_chat_without_folder().await.unwrap();
    assert_eq!(
        agent.snapshot(root).await.unwrap().search_config,
        SearchConfig::enabled_default()
    );
    agent.set_search(root, None).await.unwrap();
    let turn = agent.advance(root, prompt("Delegate.")).await.unwrap();
    let (child, _) = member_question(outcome(&turn).await);
    assert_eq!(agent.snapshot(child).await.unwrap().search_config, None);
    agent
        .set_search(root, SearchConfig::enabled_default())
        .await
        .unwrap();
    assert_eq!(agent.snapshot(child).await.unwrap().search_config, None);
    agent.shutdown().await;
}

#[tokio::test]
async fn subagent_search_survives_durable_family_reload_including_disabled() {
    for search in [
        Some(SearchConfig::for_provider(SearchProviderKind::Tavily)),
        None,
    ] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let model = asking_family();
        let agent = harness(store.clone(), model.clone(), WaitHosting::Durable);
        let root = agent.create_chat_without_folder().await.unwrap();
        agent.set_search(root, search.clone()).await.unwrap();
        let turn = agent.advance(root, prompt("Delegate.")).await.unwrap();
        let (child, question) = member_question(outcome(&turn).await);
        agent
            .set_search(root, SearchConfig::enabled_default())
            .await
            .unwrap();
        until_parent_waits(&agent, &store, root).await;
        agent.shutdown().await;
        agent.sync().await.unwrap();

        let reloaded_store = Arc::new(InMemoryTranscriptStore::new());
        for member in [root, child] {
            let serialized = serde_json::to_vec(&store.lines(member)).unwrap();
            reloaded_store.seed(
                member,
                serde_json::from_slice(&serialized).unwrap(),
                StorePosition(1),
            );
        }
        let restored = harness(reloaded_store, model, WaitHosting::Durable);
        restored.resume_chat(child).await.unwrap();
        assert_eq!(
            restored.snapshot(child).await.unwrap().search_config,
            search
        );
        let reply = restored.advance(child, answer(&question)).await.unwrap();
        assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
        assert!(matches!(
            settled(&restored, root, turn.input_id()).await,
            SettledOutcome::Completed { .. }
        ));
        assert_eq!(
            restored.snapshot(child).await.unwrap().search_config,
            search
        );
        restored.shutdown().await;
    }
}

#[tokio::test]
async fn subagent_search_local_reload_still_uses_saved_user_settings() {
    let directory = tempfile::tempdir().unwrap();
    let settings = tickloop_agent_config::UserSettings::in_root(directory.path());
    settings.trust_working_directory(directory.path()).unwrap();
    let search = Some(SearchConfig::for_provider(SearchProviderKind::Tavily));
    settings.save_search_config(search.as_ref()).unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = asking_family();
    let local_options = || {
        let mut options = options(WaitHosting::Durable);
        options.settings = Some(settings.clone());
        options
    };
    let seams = || HarnessSeams {
        transcripts: Some(store.clone()),
        environment: Some(Arc::new(
            tickloop_agent_runtime::environment::LocalEnvironment::bare(),
        )),
        models: ChosenModels::new(model.clone()),
    };
    let agent = Agent::with_seams(seams(), local_options()).unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    assert_eq!(agent.snapshot(root).await.unwrap().search_config, search);
    let turn = agent.advance(root, prompt("Delegate.")).await.unwrap();
    let (child, _) = member_question(outcome(&turn).await);
    assert_eq!(agent.snapshot(child).await.unwrap().search_config, search);
    until_parent_waits(&agent, &store, root).await;
    agent.shutdown().await;
    agent.sync().await.unwrap();

    settings.save_search_config(None).unwrap();
    let restored = Agent::with_seams(seams(), local_options()).unwrap();
    restored.resume_chat(child).await.unwrap();
    assert_eq!(restored.snapshot(root).await.unwrap().search_config, None);
    assert_eq!(restored.snapshot(child).await.unwrap().search_config, None);
    restored.shutdown().await;
}

#[tokio::test]
async fn fixed_search_defaults_reach_every_chat_and_override_stored_family_settings() {
    const GRANDCHILD_TASK: &str = "grandchild";
    for search in [
        Some(SearchConfig::for_provider(SearchProviderKind::Tavily)),
        None,
    ] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let model = FamilyModel::new(|request| match task_of(request).as_deref() {
            Some(GRANDCHILD_TASK) if !has_tool_result(request, QUESTION_CALL) => Reply::Says(call(
                QUESTION_CALL,
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which source?"),
            )),
            Some(CHILD_TASK) if !has_tool_result(request, "nested") => Reply::Says(call(
                "nested",
                "Subagent",
                json!({"task": GRANDCHILD_TASK, "name": "grandchild"}),
            )),
            None if !has_tool_result(request, DELEGATION_CALL) => Reply::Says(call(
                DELEGATION_CALL,
                "Subagent",
                json!({"task": CHILD_TASK, "name": "child"}),
            )),
            _ => Reply::Says(text("Done.")),
        });
        let fixed_options = || {
            let mut options = options(WaitHosting::Durable);
            options.search = SearchDefaults::Fixed(search.clone());
            options.tools.web_search = true;
            options
        };
        let seams = || HarnessSeams {
            transcripts: Some(store.clone()),
            environment: None,
            models: ChosenModels::new(model.clone()),
        };
        let agent = Agent::with_seams(seams(), fixed_options()).unwrap();
        let root = agent.create_chat_without_folder().await.unwrap();
        assert_eq!(agent.snapshot(root).await.unwrap().search_config, search);
        let turn = agent.advance(root, prompt("Delegate.")).await.unwrap();
        let (grandchild, question) = member_question(outcome(&turn).await);
        let child = agent
            .snapshot(grandchild)
            .await
            .unwrap()
            .delegation
            .unwrap()
            .parent_chat_id;
        assert_ne!(root, child);
        for member in [root, child, grandchild] {
            assert_eq!(agent.snapshot(member).await.unwrap().search_config, search);
        }
        for request in model.requests.lock().unwrap().iter() {
            assert_eq!(
                request.tools.iter().any(|tool| tool.name == "WebSearch"),
                search.is_some(),
                "WebSearch availability follows the fixed host default for every member"
            );
        }

        let explicit = if search.is_some() {
            None
        } else {
            SearchConfig::enabled_default()
        };
        for member in [root, child, grandchild] {
            agent.set_search(member, explicit.clone()).await.unwrap();
            assert_eq!(
                agent.snapshot(member).await.unwrap().search_config,
                explicit
            );
        }
        let another_root = agent.create_chat_without_folder().await.unwrap();
        assert_eq!(
            agent.snapshot(another_root).await.unwrap().search_config,
            search
        );
        until_parent_waits(&agent, &store, root).await;
        agent.shutdown().await;
        agent.sync().await.unwrap();

        let restored = Agent::with_seams(seams(), fixed_options()).unwrap();
        restored.resume_chat(grandchild).await.unwrap();
        for member in [root, child, grandchild] {
            assert_eq!(
                restored.snapshot(member).await.unwrap().search_config,
                search
            );
        }
        let reply = restored
            .advance(grandchild, answer(&question))
            .await
            .unwrap();
        assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
        assert!(matches!(
            settled(&restored, root, turn.input_id()).await,
            SettledOutcome::Completed { .. }
        ));
        restored.shutdown().await;
    }
}

#[tokio::test]
async fn a_foreground_childs_question_suspends_the_family_and_another_instance_finishes_it() {
    let in_process = answered_in_process().await;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = asking_family();
    let (root, child, turn, request) = asked_and_released(&store, model.clone()).await;

    // The shutdown released the waiting parent: its call still waits for the child, which was
    // not moved to the background.
    let root_lines = store.lines(root);
    let root_run = turn.receipt().run_id;
    assert_eq!(
        response_states(&root_lines, root_run).last().unwrap().1,
        ResponseState::Suspended
    );
    let waits = subagent_waits(&root_lines);
    assert_eq!(waits.len(), 1);
    assert!(matches!(waits[0].1, WaitState::Open));
    assert!(results_of(&root_lines, DELEGATION_CALL).is_empty());
    assert!(last_completions(&root_lines).is_empty());
    assert!(store.lines(child).iter().any(|line| matches!(
        line,
        Line::Wait(entry) if matches!(entry.kind, WaitKind::Question { .. })
            && matches!(entry.state, WaitState::Open)
    )));

    // Hours later another instance takes the reply; the parent goes on with the child's report.
    let answering = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let reply = answering.advance(child, answer(&request)).await.unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        settled(&answering, root, turn.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(answering.quiesce(root).await, Quiescence::Idle);
    let snapshot = answering.snapshot(root).await.unwrap();
    let result = tool_result(&snapshot.messages, DELEGATION_CALL).unwrap();
    assert_eq!(reported(&result), reported(&in_process));
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"], CHILD_REPORT);
    // The report reached the parent once: as the call's result, not also as a notification.
    let [completion] = snapshot.family_completions.as_slice() else {
        panic!("one completion: {:?}", snapshot.family_completions);
    };
    assert!(matches!(
        completion.notification,
        SubagentNotificationDelivery::Foreground
    ));
    assert!(snapshot.agent_message_origins.is_empty());
    assert_eq!(model.requests_of(None), 2);
    answering.shutdown().await;
    answering.sync().await.unwrap();
    let root_lines = store.lines(root);
    assert_eq!(
        response_states(&root_lines, root_run)
            .iter()
            .map(|(attempt, _)| *attempt)
            .max(),
        Some(1),
        "the parent's answer went on in one attempt"
    );
    assert!(matches!(
        subagent_waits(&root_lines)[0].1,
        WaitState::Appended
    ));
    assert_eq!(results_of(&root_lines, DELEGATION_CALL).len(), 1);
}

#[tokio::test]
async fn a_worker_restarted_during_a_foreground_subagent_ends_it_as_a_lost_host_as_before() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = asking_family();
    // Waits are recorded alike under either hosting; only the load differs.
    let worker = harness(store.clone(), model, WaitHosting::Durable);
    let root = worker.create_chat_without_folder().await.unwrap();
    let turn = worker
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    let (child, _) = member_question(outcome(&turn).await);
    worker.sync().await.unwrap();
    // The Worker dies without shutting down; a new one loads what the store holds.
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(root, store.lines(root), StorePosition(1));
    crashed.seed(child, store.lines(child), StorePosition(1));
    let restarted = harness(crashed.clone(), asking_family(), WaitHosting::Process);
    let snapshot = restarted.resume_chat(root).await.unwrap();
    let goal = snapshot.chat_loop;
    assert_eq!(goal.state, LoopState::Interrupted);
    // The batch had no result yet, so the load drops it, as it always has, and the child is not
    // loaded with its parent.
    assert!(!format!("{:?}", snapshot.messages).contains(DELEGATION_CALL));
    assert!(restarted.snapshot(child).await.is_err());
    restarted.sync().await.unwrap();
    let lines = crashed.lines(root);
    assert_eq!(
        response_states(&lines, turn.receipt().run_id)
            .last()
            .unwrap()
            .1,
        ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::Host
            }
        }
    );
    assert!(matches!(
        subagent_waits(&lines)[0].1,
        WaitState::Abandoned { .. }
    ));
    restarted.shutdown().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn a_pending_completion_of_a_foreground_call_is_returned_not_notified() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = asking_family();
    let (root, child, turn, _) = asked_and_released(&store, model.clone()).await;
    // A completion of the child's run recorded as pending, as one a host recorded before it
    // knew the call waited on it: the load finds the call waiting and returns it.
    let child_lines = store.lines(child);
    let child_receipt = child_lines
        .iter()
        .find_map(|line| match line {
            Line::Input(entry) => Some((entry.loop_id, entry.run_id)),
            _ => None,
        })
        .unwrap();
    let mut root_lines = store.lines(root);
    root_lines.push(Line::FamilyCompletions(FamilyCompletionsEntry {
        chat_id: root,
        completions: vec![FamilyCompletion {
            child_chat_id: child,
            child_loop_id: child_receipt.0,
            child_run_id: child_receipt.1,
            handle: "writer".into(),
            state: LoopState::Idle,
            result: CHILD_REPORT.into(),
            error: None,
            started_at: None,
            finished_at: 1,
            triggering_message_ids: Vec::new(),
            notification: SubagentNotificationDelivery::Pending,
            usage: Default::default(),
            run_status: Some(tickloop_agent_protocol::ChildRunStatus::Completed),
        }],
    }));
    let seeded = Arc::new(InMemoryTranscriptStore::new());
    seeded.seed(root, root_lines, StorePosition(1));
    seeded.seed(child, child_lines, StorePosition(1));
    let loading = harness(seeded.clone(), model.clone(), WaitHosting::Durable);
    loading.resume_chat(root).await.unwrap();
    assert!(matches!(
        settled(&loading, root, turn.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    let snapshot = loading.snapshot(root).await.unwrap();
    assert!(matches!(
        snapshot.family_completions[0].notification,
        SubagentNotificationDelivery::Foreground
    ));
    assert!(snapshot.agent_message_origins.is_empty());
    assert!(snapshot.pending_inputs.is_empty());
    assert_eq!(
        tool_result(&snapshot.messages, DELEGATION_CALL).unwrap()["result"],
        CHILD_REPORT
    );
    loading.shutdown().await;
}

#[tokio::test]
async fn a_child_that_lost_its_host_once_reports_to_its_foreground_parent_once() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = held_child_family(1);
    let first = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let root = first.create_chat_without_folder().await.unwrap();
    let turn = first
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    model.holding.notified().await;
    until_parent_waits(&first, &store, root).await;
    first.shutdown().await;
    first.sync().await.unwrap();
    assert!(last_completions(&store.lines(root)).is_empty());

    let second = harness(store.clone(), model.clone(), WaitHosting::Durable);
    second.resume_chat(root).await.unwrap();
    assert!(matches!(
        settled(&second, root, turn.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    let snapshot = second.snapshot(root).await.unwrap();
    let result = tool_result(&snapshot.messages, DELEGATION_CALL).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"], CHILD_REPORT);
    assert_eq!(snapshot.family_completions.len(), 1);
    assert!(snapshot.agent_message_origins.is_empty());
    second.shutdown().await;
    second.sync().await.unwrap();
    let completion = &snapshot.family_completions[0];
    let attempts = response_states(
        &store.lines(completion.child_chat_id),
        completion.child_run_id,
    );
    assert_eq!(
        attempts.last().unwrap(),
        &(
            2,
            ResponseState::Ended {
                ending: ResponseEnding::Completed
            }
        )
    );
}

#[tokio::test]
async fn a_child_failed_by_three_lost_hosts_at_load_resolves_its_foreground_parent() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = held_child_family(usize::MAX);
    let first = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let root = first.create_chat_without_folder().await.unwrap();
    let turn = first
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    model.holding.notified().await;
    until_parent_waits(&first, &store, root).await;
    first.shutdown().await;
    first.sync().await.unwrap();
    // Two more holdings lose the child's continued attempt the same way.
    for _ in 0..2 {
        let holder = harness(store.clone(), model.clone(), WaitHosting::Durable);
        holder.resume_chat(root).await.unwrap();
        model.holding.notified().await;
        holder.shutdown().await;
        holder.sync().await.unwrap();
    }
    let last = harness(store.clone(), model.clone(), WaitHosting::Durable);
    last.resume_chat(root).await.unwrap();
    assert!(matches!(
        settled(&last, root, turn.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    let snapshot = last.snapshot(root).await.unwrap();
    let result = tool_result(&snapshot.messages, DELEGATION_CALL).unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"], "lost its host 3 times");
    let [completion] = snapshot.family_completions.as_slice() else {
        panic!("{:?}", snapshot.family_completions);
    };
    assert!(matches!(
        completion.notification,
        SubagentNotificationDelivery::Foreground
    ));
    last.shutdown().await;
}

#[tokio::test]
async fn a_member_registered_without_a_transcript_does_not_fail_the_load() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = asking_family();
    let (root, _child, turn, _) = asked_and_released(&store, model.clone()).await;
    // The child's Transcript is gone; its parent's records still name it.
    let lost = Arc::new(InMemoryTranscriptStore::new());
    lost.seed(root, store.lines(root), StorePosition(1));
    let loading = harness(lost.clone(), model.clone(), WaitHosting::Durable);
    loading.resume_chat(root).await.unwrap();
    let outcome = settled(&loading, root, turn.input_id()).await;
    assert!(
        matches!(outcome, SettledOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let snapshot = loading.snapshot(root).await.unwrap();
    let result = tool_result(&snapshot.messages, DELEGATION_CALL).unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"], "lost before it started");
    loading.shutdown().await;
}

/// The root sends its child a message for every prompt, and the child's every run reports back.
fn messaging_family() -> Arc<FamilyModel> {
    FamilyModel::new(|request| {
        let last = request
            .messages
            .last()
            .map(|message| format!("{:?}", message.content))
            .unwrap_or_default();
        match task_of(request).as_deref() {
            Some(_) => Reply::Says(text("pong")),
            None if request.tools.is_empty() => Reply::Says(text("summary")),
            None if !has_tool_result(request, DELEGATION_CALL) => Reply::Says(call(
                DELEGATION_CALL,
                "Subagent",
                json!({"task": "watch", "name": "watcher", "run_in_background": true}),
            )),
            None if last.contains("ping round") => Reply::Says(call(
                "send",
                "SendMessage",
                json!({"to": "watcher", "message": "ping"}),
            )),
            None => Reply::Says(text("noted")),
        }
    })
}

/// Sends `rounds` prompts to the root of a messaging family on `agent`, each waiting until the
/// family is quiet again.
async fn message_rounds(agent: &Agent, root: ChatId, rounds: std::ops::Range<usize>) {
    for round in rounds {
        let turn = agent
            .advance(root, prompt(&format!("ping round {round}")))
            .await
            .unwrap();
        settled(agent, root, turn.input_id()).await;
        assert_eq!(agent.quiesce(root).await, Quiescence::Idle);
    }
}

#[tokio::test]
async fn pruned_family_lists_deliver_nothing_again_after_a_load() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = messaging_family();
    let agent = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let root = agent.create_chat_without_folder().await.unwrap();
    let start = agent
        .advance(root, prompt("Delegate a watcher."))
        .await
        .unwrap();
    settled(&agent, root, start.input_id()).await;
    assert_eq!(agent.quiesce(root).await, Quiescence::Idle);
    message_rounds(&agent, root, 0..70).await;
    let snapshot = agent.snapshot(root).await.unwrap();
    assert!(snapshot.family_completions.len() <= 65);
    assert!(snapshot.sent_agent_messages.len() <= 64);
    agent.shutdown().await;
    agent.sync().await.unwrap();
    let lines = store.lines(root);
    assert!(last_completions(&lines).len() <= 65);
    let requests = model.requests.lock().unwrap().len();

    let loading = harness(store.clone(), model.clone(), WaitHosting::Durable);
    loading.resume_chat(root).await.unwrap();
    assert_eq!(loading.quiesce(root).await, Quiescence::Idle);
    assert_eq!(
        model.requests.lock().unwrap().len(),
        requests,
        "a load delivers no completion and no message again"
    );
    loading.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_family_messaging_over_many_holdings_leaves_bounded_loads() {
    const ROUNDS_PER_HOLDING: usize = 60;
    const HOLDINGS: usize = 4;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = messaging_family();
    let agent = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let root = agent.create_chat_without_folder().await.unwrap();
    let start = agent
        .advance(root, prompt("Delegate a watcher."))
        .await
        .unwrap();
    settled(&agent, root, start.input_id()).await;
    agent.shutdown().await;
    agent.sync().await.unwrap();
    for holding in 0..HOLDINGS {
        let agent = harness(store.clone(), model.clone(), WaitHosting::Durable);
        agent.resume_chat(root).await.unwrap();
        message_rounds(
            &agent,
            root,
            holding * ROUNDS_PER_HOLDING..(holding + 1) * ROUNDS_PER_HOLDING,
        )
        .await;
        agent.compact(root).await.unwrap();
        agent.sync().await.unwrap();
        agent.shutdown().await;
        let loaded = store
            .load(root, LoadRange::FromLatestBoundary)
            .await
            .unwrap()
            .unwrap();
        let retained = loaded
            .retained
            .iter()
            .chain(loaded.lines.iter().filter(|line| line.is_retained()))
            .map(|line| serde_json::to_vec(line).unwrap().len())
            .collect::<Vec<_>>();
        let family_line = loaded
            .retained
            .iter()
            .chain(loaded.lines.iter())
            .filter(|line| {
                matches!(
                    line,
                    Line::FamilyCompletions(_) | Line::SentAgentMessages(_)
                )
            })
            .map(|line| serde_json::to_vec(line).unwrap().len())
            .max()
            .unwrap_or_default();
        // One rewrite of either list stays at the size of its kept records, and what a load
        // reads of the retained Lines stays bounded however many rounds the family had.
        assert!(
            family_line < 96 * 1024,
            "a family list line takes {family_line} bytes"
        );
        let retained_bytes = retained.iter().sum::<usize>();
        assert!(
            retained_bytes < 512 * 1024,
            "a load reads {retained_bytes} bytes of retained Lines"
        );
    }
}

/// A root that delegates in the foreground to a member that delegates a leaf in the background
/// and then waits for it; the leaf asks the user first.
fn nested_family() -> Arc<FamilyModel> {
    FamilyModel::new(|request| match task_of(request).as_deref() {
        Some("member") if !has_tool_result(request, "leaf") => Reply::Says(call(
            "leaf",
            "Subagent",
            json!({"task": "leaf", "name": "leaf", "run_in_background": true}),
        )),
        Some("member") if format!("{:?}", request.messages).contains("leaf report") => {
            Reply::Says(text("member report"))
        }
        Some("member") => Reply::Says(text("Waiting for the leaf.")),
        Some("leaf") if !has_tool_result(request, QUESTION_CALL) => Reply::Says(call(
            QUESTION_CALL,
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Which source?"),
        )),
        Some("leaf") => Reply::Says(text("leaf report")),
        _ if !has_tool_result(request, DELEGATION_CALL) => Reply::Says(call(
            DELEGATION_CALL,
            "Subagent",
            json!({"task": "member", "name": "member"}),
        )),
        _ => Reply::Says(text(ROOT_DONE)),
    })
}

#[tokio::test]
async fn a_member_waiting_for_its_children_holds_on_across_instances_without_polling() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = nested_family();
    let asking = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let root = asking.create_chat_without_folder().await.unwrap();
    let turn = asking
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    // The root waits for the member, which waits for the leaf, which waits for the user.
    let TurnOutcome::Waiting(TurnWait::Member { chat: member, wait }) = outcome(&turn).await else {
        panic!("the root waits for its member");
    };
    let TurnWait::Member { chat: leaf, wait } = *wait else {
        panic!("the member waits for its leaf");
    };
    let TurnWait::Question(request) = *wait else {
        panic!("the leaf waits for its question");
    };
    asking.shutdown().await;
    asking.sync().await.unwrap();
    assert!(store.lines(member).iter().any(|line| matches!(
        line,
        Line::Wait(entry) if matches!(entry.kind, WaitKind::Children { .. })
            && matches!(entry.state, WaitState::Open)
    )));
    let member_requests = model.requests_of(Some("member"));

    let answering = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let reply = answering.advance(leaf, answer(&request)).await.unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        settled(&answering, root, turn.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    let snapshot = answering.snapshot(root).await.unwrap();
    let result = tool_result(&snapshot.messages, DELEGATION_CALL).unwrap();
    assert_eq!(result["result"], "member report");
    // The member asked the model again once, after the leaf reported: waiting did not poll it.
    assert_eq!(model.requests_of(Some("member")), member_requests + 1);
    answering.shutdown().await;
    answering.sync().await.unwrap();
    assert!(store.lines(member).iter().any(|line| matches!(
        line,
        Line::Wait(entry) if matches!(entry.kind, WaitKind::Children { .. })
            && matches!(entry.state, WaitState::Appended)
    )));
}

/// The cloud's composition: Auto without a reviewer, where a Subagent call is admitted by the
/// host's rule and a member's report reaches its parent only through SubagentHandback.
fn cloud_harness(store: Arc<dyn TranscriptStore>, model: Arc<FamilyModel>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::product_defaults()
            },
            host_rules: tickloop_agent_runtime::durable_chat_host_rules(),
            ..options(WaitHosting::Durable)
        },
    )
    .unwrap()
}

/// [`nested_family`] where each member hands its report back, as Auto requires: the member's
/// first SubagentHandback waits there for its leaf, and once the leaf reports it hands back
/// again with what it read.
fn handing_back_family() -> Arc<FamilyModel> {
    FamilyModel::new(|request| match task_of(request).as_deref() {
        Some("member") if !has_tool_result(request, "leaf") => Reply::Says(call(
            "leaf",
            "Subagent",
            json!({"task": "leaf", "name": "leaf", "run_in_background": true}),
        )),
        Some("member") if !has_tool_result(request, "handback") => Reply::Says(call(
            "handback",
            "SubagentHandback",
            json!({"message": "too early"}),
        )),
        Some("member")
            if format!("{:?}", request.messages).contains("leaf report")
                && !has_tool_result(request, "handback-again") =>
        {
            Reply::Says(call(
                "handback-again",
                "SubagentHandback",
                json!({"message": "member report"}),
            ))
        }
        Some("member") => Reply::Says(text("Reported.")),
        Some("leaf") if !has_tool_result(request, QUESTION_CALL) => Reply::Says(call(
            QUESTION_CALL,
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Which source?"),
        )),
        Some("leaf") if !has_tool_result(request, "leaf-handback") => Reply::Says(call(
            "leaf-handback",
            "SubagentHandback",
            json!({"message": "leaf report"}),
        )),
        Some("leaf") => Reply::Says(text("Reported.")),
        _ if !has_tool_result(request, DELEGATION_CALL) => Reply::Says(call(
            DELEGATION_CALL,
            "Subagent",
            json!({"task": "member", "name": "member"}),
        )),
        _ => Reply::Says(text(ROOT_DONE)),
    })
}

/// Under the cloud's composition a member's SubagentHandback waits for its own children where it
/// was called: the wait outlives the instance, another instance resumes it once the leaf
/// reports, and the member's report reaches the root through the handback.
#[tokio::test]
async fn a_handback_waits_for_the_members_children_across_instances_under_the_cloud_composition() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = handing_back_family();
    let asking = cloud_harness(store.clone(), model.clone());
    let root = asking.create_chat_without_folder().await.unwrap();
    let turn = asking
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Member { chat: member, wait }) = outcome(&turn).await else {
        panic!("the root waits for its member");
    };
    let TurnWait::Member { chat: leaf, wait } = *wait else {
        panic!("the member waits for its leaf");
    };
    let TurnWait::Question(request) = *wait else {
        panic!("the leaf waits for its question");
    };
    asking.shutdown().await;
    asking.sync().await.unwrap();
    let children_wait = |lines: &[Line]| {
        lines.iter().rev().find_map(|line| match line {
            Line::Wait(entry) if matches!(entry.kind, WaitKind::Children { .. }) => {
                Some((entry.resume.clone(), entry.state.clone()))
            }
            _ => None,
        })
    };
    let (resume, state) = children_wait(&store.lines(member)).expect("the member waits");
    assert_eq!(resume.call_id(), Some("handback"));
    assert!(matches!(state, WaitState::Open));

    let answering = cloud_harness(store.clone(), model.clone());
    let reply = answering.advance(leaf, answer(&request)).await.unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        settled(&answering, root, turn.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    let snapshot = answering.snapshot(root).await.unwrap();
    let result = tool_result(&snapshot.messages, DELEGATION_CALL).unwrap();
    assert_eq!(
        result["result"],
        tickloop_agent_protocol::SUBAGENT_REPORT_RECEIPT
    );
    // The report itself arrives as the member's Agent message, the one it handed back after its
    // leaf reported.
    let conversation = format!("{:?}", snapshot.messages);
    assert!(conversation.contains("member report"), "{conversation}");
    assert!(!conversation.contains("too early"), "{conversation}");
    answering.shutdown().await;
    answering.sync().await.unwrap();
    let (_, state) = children_wait(&store.lines(member)).unwrap();
    assert!(matches!(state, WaitState::Appended), "{state:?}");
}

fn host_rule(tool: &str) -> tickloop_agent_protocol::PermissionRule {
    tickloop_agent_protocol::PermissionRule {
        id: Uuid::new_v4(),
        decision: tickloop_agent_protocol::Decision::Allow,
        tool: tool.into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

fn harness_with_rules(
    store: Arc<dyn TranscriptStore>,
    model: Arc<FamilyModel>,
    host_rules: Vec<tickloop_agent_protocol::PermissionRule>,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host_rules,
            ..options(WaitHosting::Durable)
        },
    )
    .unwrap()
}

/// The host's rules reach every member as they are on the instance that holds the family:
/// never recorded with a child's delegation, and replaced for every member when they change.
#[tokio::test]
async fn host_rules_reach_a_child_live_and_are_never_recorded_with_its_delegation() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = asking_family();
    let first_rule = host_rule("WebFetch");
    let asking = harness_with_rules(store.clone(), model.clone(), vec![first_rule.clone()]);
    let root = asking.create_chat_without_folder().await.unwrap();
    let turn = asking
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    let (child, _) = member_question(outcome(&turn).await);
    let rule_ids = |agent_policy: &tickloop_agent_protocol::PermissionPolicy| {
        agent_policy
            .host_rules
            .iter()
            .chain(&agent_policy.rules)
            .map(|rule| rule.id)
            .collect::<Vec<_>>()
    };
    let policy = asking.snapshot(child).await.unwrap().policy;
    assert!(rule_ids(&policy).contains(&first_rule.id));
    assert!(!policy.rules.iter().any(|rule| rule.id == first_rule.id));
    asking.shutdown().await;
    asking.sync().await.unwrap();
    let delegation = store
        .lines(child)
        .into_iter()
        .find_map(|line| match line {
            Line::Delegation(entry) => Some(entry.delegation),
            _ => None,
        })
        .unwrap();
    assert!(!rule_ids(&delegation.ceiling.policy).contains(&first_rule.id));
    assert_eq!(delegation.ceiling.folder, None);

    let second_rule = host_rule("WebFetch");
    let loading = harness_with_rules(store.clone(), model, vec![second_rule.clone()]);
    loading.resume_chat(root).await.unwrap();
    let snapshot = loading.snapshot(child).await.unwrap();
    // The member's own policy and the ceiling its delegation checks both hold the current
    // rules, and only those.
    for policy in [
        &snapshot.policy,
        &snapshot.delegation.unwrap().ceiling.policy,
    ] {
        assert!(rule_ids(policy).contains(&second_rule.id));
        assert!(!rule_ids(policy).contains(&first_rule.id));
    }
    loading.shutdown().await;
}

const HELD_FAMILY_MESSAGE: &str = "Message retained across the stop";
const OUTSIDE_FAMILY_MESSAGE: &str = "Outside family message";

/// Stops either the root or, through StopSubagent, a middle member with a child. The receiver
/// holds a model request so the ordinary message remains queued until the stop disarms it.
async fn stopped_message_family(
    subtree: bool,
) -> (
    Arc<InMemoryTranscriptStore>,
    Agent,
    ChatId,
    ChatId,
    Vec<ChatId>,
) {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let receiver_holding = Arc::new(Notify::new());
    let model = FamilyModel::new(move |request| match task_of(request).as_deref() {
        None if subtree
            && tickloop_agent_test_support::routed::history_text(request)
                .contains(OUTSIDE_FAMILY_MESSAGE)
            && !has_tool_result(request, "outside-send") =>
        {
            Reply::Says(call(
                "outside-send",
                "SendMessage",
                json!({"to":"middle", "message":OUTSIDE_FAMILY_MESSAGE}),
            ))
        }
        Some("leaf") if !has_tool_result(request, "send") => Reply::After(
            receiver_holding.clone(),
            call(
                "send",
                "SendMessage",
                json!({"to": if subtree { "middle" } else { "main" }, "message": HELD_FAMILY_MESSAGE}),
            ),
        ),
        Some("leaf") => Reply::Holds,
        Some("middle") if !has_tool_result(request, "delegate-leaf") => Reply::Says(call(
            "delegate-leaf",
            "Subagent",
            json!({"task":"leaf", "name":"leaf", "run_in_background":true}),
        )),
        Some("middle") => {
            receiver_holding.notify_one();
            Reply::Holds
        }
        _ if !has_tool_result(request, DELEGATION_CALL) => Reply::Says(call(
            DELEGATION_CALL,
            "Subagent",
            json!({"task":if subtree {"middle"} else {"leaf"}, "name":if subtree {"middle"} else {"leaf"}, "run_in_background":true}),
        )),
        _ if !subtree => {
            receiver_holding.notify_one();
            Reply::Holds
        }
        _ if !has_tool_result(request, QUESTION_CALL) => Reply::Says(call(
            QUESTION_CALL,
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Stop the subtree?"),
        )),
        _ if !has_tool_result(request, "stop") => {
            Reply::Says(call("stop", "StopSubagent", json!({"to":"middle"})))
        }
        _ => Reply::Says(text(ROOT_DONE)),
    });
    let agent = harness(store.clone(), model, WaitHosting::Durable);
    let root = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(root, prompt("Start the family"))
        .await
        .unwrap();
    let target = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let members = agent.family_agents(root).await.unwrap();
            let target = if subtree {
                members
                    .iter()
                    .find(|member| member.handle == "middle")
                    .map(|member| member.chat_id)
            } else {
                Some(root)
            };
            if let Some(target) = target {
                let snapshot = agent.snapshot(target).await.unwrap();
                if snapshot
                    .pending_inputs
                    .iter()
                    .any(|pending| pending.input.contains(HELD_FAMILY_MESSAGE))
                {
                    break target;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("ordinary family message queued before stop");
    if subtree {
        let TurnOutcome::Waiting(TurnWait::Question(question)) = outcome(&turn).await else {
            panic!("root asks before stopping the subtree");
        };
        let stopping = agent.advance(root, answer(&question)).await.unwrap();
        assert!(matches!(
            outcome(&stopping).await,
            TurnOutcome::Merged { .. }
        ));
        assert!(matches!(
            settled(&agent, root, turn.input_id()).await,
            SettledOutcome::Completed { .. }
        ));
        let snapshot = agent.snapshot(root).await.unwrap();
        let result = tool_result(&snapshot.messages, "stop").unwrap();
        assert_eq!(result["status"], "stopped", "{result}");
    } else {
        agent.stop(root).await.unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(target).await.unwrap();
            if snapshot.pending_inputs.len() == 2
                && snapshot.pending_inputs.iter().all(|pending| !pending.armed)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("ordinary message and stopped completion held together");
    let members = agent
        .family_agents(root)
        .await
        .unwrap()
        .into_iter()
        .map(|member| member.chat_id)
        .collect();
    agent.sync().await.unwrap();
    (store, agent, root, target, members)
}

async fn assert_stopped_family_recovery(
    subtree: bool,
    waits: WaitHosting,
    pending_completion: bool,
) {
    let (store, original, root, target, members) = stopped_message_family(subtree).await;
    let held = original.snapshot(target).await.unwrap();
    let expected_origins = held.agent_message_origins.len() + held.pending_inputs.len();
    let message_ids: Vec<_> = held
        .pending_inputs
        .iter()
        .map(|pending| pending.agent_message.as_ref().unwrap().message_id)
        .collect();
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    for member in members {
        let mut lines = store.lines(member);
        if pending_completion && member == target {
            let mut completions = held.family_completions.clone();
            let completion = completions.last_mut().unwrap();
            completion.notification = SubagentNotificationDelivery::Pending;
            let id = completion.child_run_id;
            lines.retain(|line| !matches!(line, Line::Input(entry) if entry.input_id == id));
            lines.push(Line::FamilyCompletions(FamilyCompletionsEntry {
                chat_id: target,
                completions,
            }));
        }
        crashed.seed(member, lines, StorePosition(1));
    }
    let model = FamilyModel::new(|_| Reply::Says(text("Recovered once")));
    let restarted = harness(crashed.clone(), model.clone(), waits);
    restarted
        .resume_chat(if waits == WaitHosting::Durable {
            root
        } else {
            target
        })
        .await
        .unwrap();
    assert_eq!(restarted.quiesce(target).await, Quiescence::Idle);
    let snapshot = restarted.snapshot(target).await.unwrap();
    assert_eq!(
        snapshot.pending_inputs.len(),
        2,
        "both family inputs survive restart"
    );
    assert!(
        snapshot.pending_inputs.iter().all(|pending| !pending.armed),
        "restart must wait for Continue"
    );
    assert!(
        model.requests.lock().unwrap().is_empty(),
        "held messages must not run before Continue"
    );
    restarted.continue_loop(target).await.unwrap();
    assert_eq!(restarted.quiesce(target).await, Quiescence::Idle);
    let snapshot = restarted.snapshot(target).await.unwrap();
    for id in &message_ids {
        assert!(
            snapshot
                .agent_message_origins
                .values()
                .any(|source| source["message_id"] == id.to_string())
        );
    }
    assert_eq!(snapshot.agent_message_origins.len(), expected_origins);
    restarted.sync().await.unwrap();
    let delivered_requests = model.requests.lock().unwrap().len();
    assert!(delivered_requests > 0);
    let after_continue = harness(crashed, model.clone(), waits);
    after_continue
        .resume_chat(if waits == WaitHosting::Durable {
            root
        } else {
            target
        })
        .await
        .unwrap();
    assert_eq!(after_continue.quiesce(target).await, Quiescence::Idle);
    assert_eq!(
        after_continue
            .snapshot(target)
            .await
            .unwrap()
            .agent_message_origins
            .len(),
        expected_origins
    );
    assert_eq!(
        model.requests.lock().unwrap().len(),
        delivered_requests,
        "a second restart must not deliver the messages again"
    );
    after_continue.shutdown().await;
    restarted.shutdown().await;
    original.shutdown().await;
}

#[tokio::test]
async fn durable_restart_preserves_queued_family_holds_from_both_stop_entries() {
    for subtree in [false, true] {
        assert_stopped_family_recovery(subtree, WaitHosting::Durable, false).await;
    }
}

#[tokio::test]
async fn durable_restart_holds_pending_completion_replay_from_both_stop_entries() {
    for subtree in [false, true] {
        assert_stopped_family_recovery(subtree, WaitHosting::Durable, true).await;
    }
}

#[tokio::test]
async fn durable_restart_after_continue_keeps_family_inputs_deliverable() {
    use tickloop_agent_runtime::trigger_pauses::PausePoint;
    for subtree in [false, true] {
        let (store, original, root, target, members) = stopped_message_family(subtree).await;
        let before = original.snapshot(target).await.unwrap();
        let expected_origins = before.agent_message_origins.len() + before.pending_inputs.len();
        let pause = original.pause_next_for_tests(target, PausePoint::ReceivingRecorded);
        let continuing = tokio::spawn({
            let original = original.clone();
            async move { original.continue_loop(target).await.unwrap() }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), pause.entered)
            .await
            .unwrap()
            .unwrap();
        original.sync().await.unwrap();
        let crashed = Arc::new(InMemoryTranscriptStore::new());
        for member in members {
            crashed.seed(member, store.lines(member), StorePosition(1));
        }
        let model = FamilyModel::new(|_| Reply::Says(text("Continued once")));
        let restarted = harness(crashed, model.clone(), WaitHosting::Durable);
        restarted.resume_chat(root).await.unwrap();
        assert_eq!(restarted.quiesce(target).await, Quiescence::Idle);
        assert_eq!(
            restarted
                .snapshot(target)
                .await
                .unwrap()
                .agent_message_origins
                .len(),
            expected_origins,
            "Continue must persist rearming before the inputs start"
        );
        assert!(!model.requests.lock().unwrap().is_empty());
        drop(pause.release);
        continuing.await.unwrap();
        restarted.shutdown().await;
        original.shutdown().await;
    }
}

#[tokio::test]
async fn process_restart_preserves_family_inputs_from_both_stop_entries() {
    for subtree in [false, true] {
        assert_stopped_family_recovery(subtree, WaitHosting::Process, false).await;
    }
}

#[tokio::test]
async fn ordinary_family_messages_follow_the_stopped_subtree_exception_independently_of_receiving()
{
    use tickloop_agent_protocol::Receiving;
    let (_, agent, root, target, _) = stopped_message_family(true).await;
    let held = agent.snapshot(target).await.unwrap();
    let ordinary = held
        .pending_inputs
        .iter()
        .find(|pending| {
            pending
                .agent_message
                .as_ref()
                .is_some_and(|source| !source.is_result)
        })
        .unwrap();
    assert!(!ordinary.armed);
    let message_id = ordinary.agent_message.as_ref().unwrap().message_id;
    assert!(!held.agent_message_origins.contains_key(&message_id));
    let outside = agent
        .advance(root, prompt(OUTSIDE_FAMILY_MESSAGE))
        .await
        .unwrap();
    assert!(matches!(
        settled(&agent, root, outside.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    let awakened = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(target).await.unwrap();
            if snapshot
                .agent_message_origins
                .values()
                .any(|source| source["message_id"] == message_id.to_string())
            {
                break snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a message from outside the stopped subtree releases held family messages");
    assert!(matches!(
        awakened.chat_loop.receiving,
        Receiving::Paused { .. }
    ));
    assert!(awakened.pending_inputs.iter().all(|pending| pending.armed));
    agent.shutdown().await;
}
