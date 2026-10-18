//! `advance`: inputs under the host's ids, the display node each one follows, and what a repeated
//! request finds in the input ledger.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Content, Decision, DirectedSummaryRequest, EventKind, LoopState,
    ModelIdentity, PermissionRule, PromptPolicy, Protocol, Role, SummaryDirection, Usage,
};
use tickloop_agent_runtime::transcript::{
    AppendAck, AppendBatch, ChatPlacement, LoadRange, StorePosition, StoredTranscript,
    TranscriptError, TranscriptStore,
};
use tickloop_agent_runtime::{
    Agent, AgentError, Delivery, HarnessOptions, HarnessSeams, MessageId, ParentSelection,
    ToolPolicy, Turn, TurnOp, TurnOutcome, TurnRequest, TurnWait, UserMessage,
    environment::LocalEnvironment,
};
use tickloop_agent_test_support::transcripts::{FaultyTranscriptStore, InMemoryTranscriptStore};
use tickloop_agent_test_support::{call, config, next_event, text};
use tickloop_agent_transcript::display::{DisplayKind, DisplayNode, project};
use tickloop_agent_transcript::{
    InputState, InterruptedBy, Line, ResponseEnding, ResponseState, SettledOutcome,
};
use tokio::sync::{Notify, mpsc::UnboundedSender, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const DEADLINE: Duration = Duration::from_secs(10);

enum Step {
    Reply(Vec<Content>),
    /// Waits until cancelled.
    Hang,
    /// Fails before any content.
    Fail,
}

/// Follows a script, records every request and announces a request it holds.
struct Script {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    holding: Notify,
}

impl Script {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
            holding: Notify::new(),
        })
    }

    fn replies(replies: &[&str]) -> Arc<Self> {
        Self::new(
            replies
                .iter()
                .map(|reply| Step::Reply(text(reply)))
                .collect(),
        )
    }

    fn requests(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// The last request's messages, as text.
    fn last_request(&self) -> String {
        format!(
            "{:?}",
            self.requests.lock().unwrap().last().unwrap().messages
        )
    }

    fn last_messages(&self) -> Vec<tickloop_agent_protocol::Message> {
        self.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .messages
            .clone()
    }

    async fn until_holding(&self) {
        tokio::time::timeout(DEADLINE, self.holding.notified())
            .await
            .expect("the model holds a request");
    }
}

#[async_trait]
impl ModelClient for Script {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        let content = match step {
            Step::Reply(content) => content,
            Step::Hang => {
                self.holding.notify_one();
                cancel.cancelled().await;
                return Err(ModelError::Cancelled);
            }
            Step::Fail => return Err(ModelError::InvalidResponse("no content".into())),
        };
        for block in &content {
            if let Content::Text { text } = block {
                let _ = events.send(ModelDelta::Text(text.clone()));
            }
        }
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

struct Fixture {
    folder: PathBuf,
    _directory: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    Fixture {
        folder: folder.canonicalize().unwrap(),
        _directory: directory,
    }
}

fn harness(
    store: Arc<dyn TranscriptStore>,
    model: Arc<dyn ModelClient>,
    host: AgentHostOptions,
    host_rules: Vec<PermissionRule>,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: Some(Arc::new(LocalEnvironment::bare())),
            models: tickloop_agent_test_support::harness::ChosenModels::new(model),
        },
        HarnessOptions {
            model: config(),
            settings: None,
            host,
            tools: ToolPolicy::local(),
            host_rules,
            waits: tickloop_agent_runtime::WaitHosting::Process,
        },
    )
    .unwrap()
}

fn agent(store: Arc<dyn TranscriptStore>, model: Arc<dyn ModelClient>) -> Agent {
    harness(
        store,
        model,
        AgentHostOptions::embedded_defaults(),
        Vec::new(),
    )
}

fn prompt(input: Uuid, after: ParentSelection, text: &str, delivery: Delivery) -> TurnRequest {
    TurnRequest {
        input_id: input,
        op: TurnOp::Continue {
            after,
            target_loop: None,
            message: UserMessage {
                text: text.into(),
                ..Default::default()
            },
        },
        delivery,
        host_context: None,
    }
}

fn after_leaf(text: &str) -> TurnRequest {
    prompt(
        Uuid::new_v4(),
        ParentSelection::CurrentLeaf,
        text,
        Delivery::Queue,
    )
}

fn after(node: MessageId, text: &str) -> TurnRequest {
    prompt(
        Uuid::new_v4(),
        ParentSelection::Message(node),
        text,
        Delivery::Queue,
    )
}

async fn outcome(turn: &Turn) -> TurnOutcome {
    tokio::time::timeout(DEADLINE, turn.outcome())
        .await
        .expect("the turn reaches an outcome")
        .unwrap()
}

async fn complete(agent: &Agent, chat: ChatId, request: TurnRequest) -> Turn {
    let turn = agent.advance(chat, request).await.unwrap();
    let reached = outcome(&turn).await;
    assert!(
        matches!(reached, TurnOutcome::Completed { .. }),
        "{reached:?}"
    );
    turn
}

/// The latest answer to `turn`'s input.
async fn answer(agent: &Agent, turn: &Turn) -> MessageId {
    agent
        .input_state(turn.receipt().chat_id, turn.input_id())
        .await
        .unwrap()
        .unwrap()
        .responses
        .last()
        .unwrap()
        .response_id
}

fn node(nodes: &[DisplayNode], id: Uuid) -> &DisplayNode {
    nodes.iter().find(|node| node.id == id).unwrap()
}

fn answers_under(nodes: &[DisplayNode], parent: Uuid) -> Vec<&DisplayNode> {
    nodes
        .iter()
        .filter(|node| {
            node.parent == Some(parent) && matches!(node.kind, DisplayKind::Answer { .. })
        })
        .collect()
}

#[tokio::test]
async fn a_root_starts_an_empty_chat_and_repeating_its_input_returns_the_outcome() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["hello"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let request = prompt(
        Uuid::new_v4(),
        ParentSelection::Root,
        "greet me",
        Delivery::SendNow,
    );
    let turn = complete(&agent, chat, request.clone()).await;
    let repeated = agent.advance(chat, request.clone()).await.unwrap();
    assert_eq!(repeated.receipt().run_id, turn.receipt().run_id);
    assert!(matches!(
        outcome(&repeated).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(model.requests(), 1);

    let mut changed = request;
    changed.op = TurnOp::Continue {
        after: ParentSelection::Root,
        target_loop: None,
        message: UserMessage {
            text: "greet someone else".into(),
            ..Default::default()
        },
    };
    assert!(matches!(
        agent.advance(chat, changed).await,
        Err(AgentError::Invalid(message)) if message.contains("different payload")
    ));

    // Each batch names the display node the Chat shows as current.
    agent.sync().await.unwrap();
    let answer = answer(&agent, &turn).await;
    let leaves = store
        .received(chat)
        .into_iter()
        .filter(|batch| !batch.lines.is_empty())
        .map(|batch| batch.display_leaf)
        .collect::<Vec<_>>();
    // The answer starts in the change that appends its prompt, so every batch shows it.
    assert!(!leaves.is_empty());
    assert!(
        leaves.iter().all(|leaf| *leaf == Some(answer)),
        "{leaves:?}"
    );
    let nodes = project(&store.lines(chat));
    assert_eq!(node(&nodes, turn.input_id()).parent, None);
    agent.shutdown().await;
}

#[tokio::test]
async fn an_explicit_parent_is_refused_while_busy_unless_sent_now() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![
        Step::Reply(text("first answer")),
        Step::Hang,
        Step::Reply(text("after the interruption")),
    ]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let first = complete(&agent, chat, after_leaf("first prompt")).await;
    let first_answer = answer(&agent, &first).await;
    let running = agent
        .advance(chat, after_leaf("second prompt"))
        .await
        .unwrap();
    model.until_holding().await;
    let second_answer = answer(&agent, &running).await;

    let queued = after(second_answer, "queued after the running answer");
    assert!(matches!(
        agent.advance(chat, queued.clone()).await,
        Err(AgentError::Invalid(message)) if message == "chat is busy"
    ));
    assert!(matches!(
        agent.advance(chat, after(first_answer, "a branch")).await,
        Err(AgentError::Invalid(message)) if message == "chat is busy"
    ));
    assert!(
        agent
            .input_state(chat, queued.input_id)
            .await
            .unwrap()
            .is_none(),
        "a refused input leaves no record"
    );

    let mut sent_now = queued;
    sent_now.delivery = Delivery::SendNow;
    let follow_up = complete(&agent, chat, sent_now).await;
    assert!(matches!(
        outcome(&running).await,
        TurnOutcome::Interrupted {
            by: InterruptedBy::User,
            ..
        }
    ));
    agent.sync().await.unwrap();
    let nodes = project(&store.lines(chat));
    assert_eq!(
        node(&nodes, follow_up.input_id()).parent,
        Some(second_answer),
        "the interrupted answer became the follow-up's parent"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_older_sibling_is_continued_in_place_after_a_store_switch_and_after_reload() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["answer one", "answer two", "answer three", "answer four"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let one = complete(&agent, chat, after_leaf("prompt one")).await;
    let answer_one = answer(&agent, &one).await;
    let two = complete(&agent, chat, after_leaf("prompt two")).await;
    let answer_two = answer(&agent, &two).await;

    // On the current chain: the conversation is cut after answer one.
    let three = complete(&agent, chat, after(answer_one, "prompt three")).await;
    let request = model.last_request();
    assert!(request.contains("answer one") && !request.contains("prompt two"));
    let answer_three = answer(&agent, &three).await;

    // Answer two is off the current chain: the Chat is rebuilt on its branch from the store.
    let four = complete(&agent, chat, after(answer_two, "prompt four")).await;
    let request = model.last_request();
    assert!(
        request.contains("answer two") && !request.contains("prompt three"),
        "{request}"
    );
    agent.sync().await.unwrap();
    let nodes = project(&store.lines(chat));
    assert_eq!(node(&nodes, three.input_id()).parent, Some(answer_one));
    assert_eq!(node(&nodes, four.input_id()).parent, Some(answer_two));
    agent.shutdown().await;
    drop(agent);

    // A new holder resumes the newest leaf and switches back to answer three.
    let model = Script::replies(&["answer five"]);
    let next = self::agent(store.clone(), model.clone());
    let five = complete(&next, chat, after(answer_three, "prompt five")).await;
    let request = model.last_request();
    assert!(
        request.contains("answer three") && !request.contains("prompt four"),
        "{request}"
    );
    next.sync().await.unwrap();
    let nodes = project(&store.lines(chat));
    assert_eq!(node(&nodes, five.input_id()).parent, Some(answer_three));
    next.shutdown().await;
}

#[tokio::test]
async fn a_leaf_before_the_latest_compaction_boundary_is_continued_after_reload() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["answer one", "the full summary", "answer two"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let one = complete(&agent, chat, after_leaf("prompt one")).await;
    let answer_one = answer(&agent, &one).await;
    agent.compact(chat).await.unwrap();
    complete(&agent, chat, after_leaf("prompt two")).await;
    agent.sync().await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let model = Script::replies(&["from before the summary"]);
    let next = self::agent(store.clone(), model.clone());
    let branch = complete(&next, chat, after(answer_one, "prompt three")).await;
    let request = model.last_request();
    assert!(
        request.contains("answer one")
            && !request.contains("the full summary")
            && !request.contains("prompt two"),
        "{request}"
    );
    next.sync().await.unwrap();
    let nodes = project(&store.lines(chat));
    assert_eq!(node(&nodes, branch.input_id()).parent, Some(answer_one));
    next.shutdown().await;
}

#[tokio::test]
async fn a_repeated_regeneration_adds_exactly_one_sibling_answer() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["first take", "second take"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let asked = complete(&agent, chat, after_leaf("the question")).await;
    let first_take = answer(&agent, &asked).await;
    let regenerate = TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::RegenerateAnswer { answer: first_take },
        delivery: Delivery::Queue,
        host_context: None,
    };
    let regenerated = complete(&agent, chat, regenerate.clone()).await;
    let messages = model.last_messages();
    assert_eq!(messages.last().unwrap().role, Role::User);
    assert!(!model.last_request().contains("first take"));
    let again = agent.advance(chat, regenerate).await.unwrap();
    assert_eq!(again.receipt().run_id, regenerated.receipt().run_id);
    assert!(matches!(
        outcome(&again).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(model.requests(), 2);

    agent.sync().await.unwrap();
    let nodes = project(&store.lines(chat));
    let siblings = answers_under(&nodes, asked.input_id());
    assert_eq!(siblings.len(), 2, "{nodes:#?}");
    let second_take = answer(&agent, &regenerated).await;
    assert!(siblings.iter().any(|answer| answer.id == second_take));
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .find(|goal| goal.id == regenerated.receipt().loop_id)
            .unwrap()
            .state,
        LoopState::Completed
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_answer_to_a_prompt_a_directed_summary_rewrote_is_regenerated() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&[
        "answer one",
        "answer two",
        "answer three",
        "the directed summary",
        "answer three again",
    ]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    complete(&agent, chat, after_leaf("prompt one")).await;
    complete(&agent, chat, after_leaf("prompt two")).await;
    let three = complete(&agent, chat, after_leaf("prompt three")).await;
    let answer_three = answer(&agent, &three).await;
    agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: three.input_id(),
                direction: SummaryDirection::UpToHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    let rewritten = agent
        .physical_endpoint(chat, three.input_id())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(rewritten, three.input_id(), "the prompt was rewritten");

    let regenerated = complete(
        &agent,
        chat,
        TurnRequest {
            input_id: Uuid::new_v4(),
            op: TurnOp::RegenerateAnswer {
                answer: answer_three,
            },
            delivery: Delivery::SendNow,
            host_context: None,
        },
    )
    .await;
    let request = model.last_request();
    assert!(
        request.contains("the directed summary")
            && request.contains("prompt three")
            && !request.contains("answer three\""),
        "{request}"
    );
    assert_eq!(model.last_messages().last().unwrap().role, Role::User);
    agent.sync().await.unwrap();
    let nodes = project(&store.lines(chat));
    let siblings = answers_under(&nodes, three.input_id());
    assert_eq!(siblings.len(), 2, "{nodes:#?}");
    assert!(
        siblings.iter().any(|node| node.id == answer_three)
            && siblings.iter().any(|node| node.kind
                == DisplayKind::Answer {
                    input_id: Some(regenerated.input_id()),
                    attempt: 1
                })
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_replaced_prompt_gets_a_sibling() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["answer one", "answer two", "answer to the edit"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let one = complete(&agent, chat, after_leaf("prompt one")).await;
    let answer_one = answer(&agent, &one).await;
    let two = complete(&agent, chat, after_leaf("prompt two")).await;
    let edit = complete(
        &agent,
        chat,
        TurnRequest {
            input_id: Uuid::new_v4(),
            op: TurnOp::ReplacePrompt {
                prompt: two.input_id(),
                message: UserMessage {
                    text: "prompt two, edited".into(),
                    ..Default::default()
                },
            },
            delivery: Delivery::Queue,
            host_context: None,
        },
    )
    .await;
    let request = model.last_request();
    assert!(request.contains("answer one") && !request.contains("answer two"));
    agent.sync().await.unwrap();
    let nodes = project(&store.lines(chat));
    assert_eq!(node(&nodes, edit.input_id()).parent, Some(answer_one));
    assert_eq!(node(&nodes, two.input_id()).parent, Some(answer_one));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_repeated_send_now_attaches_to_the_queued_input() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang, Step::Reply(text("answered now"))]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let running = agent.advance(chat, after_leaf("long work")).await.unwrap();
    model.until_holding().await;
    let mut urgent = after_leaf("urgent");
    urgent.delivery = Delivery::SendNow;
    let sent = agent.advance(chat, urgent.clone()).await.unwrap();
    let repeated = agent.advance(chat, urgent).await.unwrap();
    assert_eq!(repeated.receipt().run_id, sent.receipt().run_id);
    assert!(matches!(
        outcome(&running).await,
        TurnOutcome::Interrupted {
            by: InterruptedBy::User,
            ..
        }
    ));
    assert!(matches!(
        outcome(&sent).await,
        TurnOutcome::Completed { .. }
    ));
    assert!(matches!(
        outcome(&repeated).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(model.requests(), 2);
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot
            .message_ids
            .values()
            .filter(|id| **id == sent.input_id())
            .count(),
        1
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_input_repeated_after_compaction_and_reload_is_not_run_again() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["answer", "the summary", "later"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let request = after_leaf("once only");
    let turn = complete(&agent, chat, request.clone()).await;
    agent.compact(chat).await.unwrap();
    complete(&agent, chat, after_leaf("later prompt")).await;
    let repeated = agent.advance(chat, request.clone()).await.unwrap();
    assert_eq!(repeated.receipt().run_id, turn.receipt().run_id);
    assert!(matches!(
        outcome(&repeated).await,
        TurnOutcome::Completed { .. }
    ));
    agent.sync().await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let model = Script::replies(&[]);
    let next = self::agent(store.clone(), model.clone());
    let repeated = next.advance(chat, request).await.unwrap();
    assert_eq!(repeated.receipt().run_id, turn.receipt().run_id);
    assert!(matches!(
        outcome(&repeated).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(model.requests(), 0);
    next.shutdown().await;
}

#[tokio::test]
async fn a_withdrawn_input_answers_withdrawn_when_repeated() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let running = agent.advance(chat, after_leaf("long work")).await.unwrap();
    model.until_holding().await;
    let request = after_leaf("taken back");
    let queued = agent.advance(chat, request.clone()).await.unwrap();
    let taken = agent.take_queued_inputs(chat, true).await.unwrap();
    assert_eq!(taken.len(), 1);
    assert!(matches!(outcome(&queued).await, TurnOutcome::Withdrawn));
    let repeated = agent.advance(chat, request).await.unwrap();
    assert!(matches!(outcome(&repeated).await, TurnOutcome::Withdrawn));
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .pending_inputs
            .is_empty(),
        "a withdrawn input is not queued again"
    );
    agent.interrupt(chat).await.unwrap();
    outcome(&running).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn an_input_whose_answer_lost_its_host_continues_as_a_sibling_attempt() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang]);
    let first = agent(store.clone(), model.clone());
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let request = after_leaf("a question");
    let turn = first.advance(chat, request.clone()).await.unwrap();
    model.until_holding().await;
    tokio::time::timeout(DEADLINE, async {
        while !store
            .lines(chat)
            .iter()
            .any(|line| matches!(line, Line::Response(_)))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the start reaches the store");
    // The host is lost now: a new holder sees only what the store holds.
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(chat, store.lines(chat), StorePosition(1));

    let model = Script::replies(&["the continued answer"]);
    let next = self::agent(crashed.clone(), model.clone());
    let continued = complete(&next, chat, request.clone()).await;
    assert_eq!(continued.receipt().run_id, turn.receipt().run_id);
    let messages = model.last_messages();
    let last = messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(
        format!("{last:?}").contains("a question")
            && !model.last_request().contains("No response requested."),
        "the attempt answers the prompt itself: {messages:?}"
    );
    let state = next
        .input_state(chat, turn.input_id())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(state.entry.state, InputState::Settled { .. }));
    assert_eq!(
        state
            .responses
            .iter()
            .map(|response| (
                response.attempt,
                response.display_parent,
                response.state.clone()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                1,
                Some(turn.input_id()),
                ResponseState::Ended {
                    ending: ResponseEnding::Interrupted {
                        by: InterruptedBy::Host
                    }
                }
            ),
            (
                2,
                Some(turn.input_id()),
                ResponseState::Ended {
                    ending: ResponseEnding::Completed
                }
            ),
        ]
    );
    // Repeating it once more returns the settled outcome.
    let repeated = next.advance(chat, request).await.unwrap();
    assert!(matches!(
        outcome(&repeated).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(model.requests(), 1);
    next.sync().await.unwrap();
    let nodes = project(&crashed.lines(chat));
    assert_eq!(answers_under(&nodes, turn.input_id()).len(), 2);
    next.shutdown().await;
    first.shutdown().await;
    assert!(matches!(
        crashed
            .lines(chat)
            .iter()
            .rev()
            .find_map(|line| match line {
                Line::Input(entry) if entry.input_id == turn.input_id() =>
                    Some(entry.state.clone()),
                _ => None,
            }),
        Some(InputState::Settled {
            outcome: SettledOutcome::Completed { .. }
        })
    ));
}

fn ask_before_writing() -> Vec<PermissionRule> {
    vec![PermissionRule {
        id: Uuid::new_v4(),
        decision: Decision::Ask,
        tool: "Write".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }]
}

fn writes_a_draft() -> Arc<Script> {
    Script::new(vec![
        Step::Reply(call(
            "write",
            "Write",
            json!({"file_path": "draft.txt", "content": "draft"}),
        )),
        Step::Reply(text("done")),
    ])
}

#[tokio::test]
async fn a_call_needing_approval_waits_when_interactive_and_is_denied_otherwise() {
    let fixture = fixture();
    let interactive = harness(
        Arc::new(InMemoryTranscriptStore::new()),
        writes_a_draft(),
        AgentHostOptions::embedded_defaults(),
        ask_before_writing(),
    );
    let chat = interactive
        .create_chat(fixture.folder.clone())
        .await
        .unwrap();
    let turn = interactive
        .advance(chat, after_leaf("write the draft"))
        .await
        .unwrap();
    let reached = outcome(&turn).await;
    assert!(
        matches!(&reached, TurnOutcome::Waiting(TurnWait::Permission(request)) if request.name == "Write"),
        "{reached:?}"
    );
    interactive.shutdown().await;

    let unattended = harness(
        Arc::new(InMemoryTranscriptStore::new()),
        writes_a_draft(),
        AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::embedded_defaults()
        },
        ask_before_writing(),
    );
    let chat = unattended
        .create_chat(fixture.folder.clone())
        .await
        .unwrap();
    let mut events = unattended.subscribe();
    let turn = complete(&unattended, chat, after_leaf("write the draft")).await;
    assert!(!fixture.folder.join("draft.txt").exists());
    loop {
        let event = next_event(&mut events).await;
        assert!(!matches!(event.kind, EventKind::PermissionRequested { .. }));
        if event.run_id == turn.receipt().run_id
            && matches!(event.kind, EventKind::Completed { .. })
        {
            break;
        }
    }
    unattended.shutdown().await;
}

#[tokio::test]
async fn a_refused_input_at_an_explicit_parent_leaves_the_conversation_uncut() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["answer one", "answer two"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let one = complete(&agent, chat, after_leaf("prompt one")).await;
    let answer_one = answer(&agent, &one).await;
    let two = complete(&agent, chat, after_leaf("prompt two")).await;
    let before = agent.snapshot(chat).await.unwrap().messages.len();

    // The Loop of prompt two starts after answer one, so the cut would remove it.
    let mut request = after(answer_one, "into a removed Loop");
    request.op = TurnOp::Continue {
        after: ParentSelection::Message(answer_one),
        target_loop: Some(two.receipt().loop_id),
        message: UserMessage {
            text: "into a removed Loop".into(),
            ..Default::default()
        },
    };
    assert!(matches!(
        agent.advance(chat, request).await,
        Err(AgentError::NotFound)
    ));
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.messages.len(), before);
    assert!(format!("{:?}", snapshot.messages).contains("answer two"));
    assert_eq!(model.requests(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn continuing_after_an_older_answer_to_the_same_prompt_switches_to_its_branch() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["first take", "second take", "after the first take"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let asked = complete(&agent, chat, after_leaf("the question")).await;
    let first_take = answer(&agent, &asked).await;
    complete(
        &agent,
        chat,
        TurnRequest {
            input_id: Uuid::new_v4(),
            op: TurnOp::RegenerateAnswer { answer: first_take },
            delivery: Delivery::Queue,
            host_context: None,
        },
    )
    .await;
    let follow_up = complete(&agent, chat, after(first_take, "follow the first take")).await;
    let request = model.last_request();
    assert!(
        request.contains("first take") && !request.contains("second take"),
        "{request}"
    );
    agent.sync().await.unwrap();
    let nodes = project(&store.lines(chat));
    assert_eq!(node(&nodes, follow_up.input_id()).parent, Some(first_take));
    agent.shutdown().await;
}

#[tokio::test]
async fn an_input_still_queued_at_shutdown_is_interrupted_by_its_host() {
    let fixture = fixture();
    let model = Script::new(vec![Step::Hang]);
    let agent = agent(Arc::new(InMemoryTranscriptStore::new()), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let running = agent.advance(chat, after_leaf("long work")).await.unwrap();
    model.until_holding().await;
    let queued = agent.advance(chat, after_leaf("behind it")).await.unwrap();
    agent.shutdown().await;
    for turn in [&running, &queued] {
        assert!(matches!(
            outcome(turn).await,
            TurnOutcome::Interrupted {
                by: InterruptedBy::Host,
                ..
            }
        ));
    }
}

/// Holds the next load that reads `range` (any range when `None`) after it has read the store,
/// until the test releases it.
struct GatedStore {
    inner: Arc<dyn TranscriptStore>,
    gate: Mutex<Option<LoadGate>>,
}

struct LoadGate {
    range: Option<LoadRange>,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

impl GatedStore {
    fn new(inner: Arc<dyn TranscriptStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            gate: Mutex::new(None),
        })
    }

    /// Gives the signal that the load read the store, and the release it then waits for.
    fn hold_next_load(
        &self,
        range: Option<LoadRange>,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_sender, entered) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        *self.gate.lock().unwrap() = Some(LoadGate {
            range,
            entered: entered_sender,
            release: release_receiver,
        });
        (entered, release)
    }
}

#[async_trait]
impl TranscriptStore for GatedStore {
    async fn register(
        &self,
        chat: ChatId,
        placement: &ChatPlacement,
    ) -> Result<StorePosition, TranscriptError> {
        self.inner.register(chat, placement).await
    }

    async fn load(
        &self,
        chat: ChatId,
        range: LoadRange,
    ) -> Result<Option<StoredTranscript>, TranscriptError> {
        let loaded = self.inner.load(chat, range).await;
        let gate = {
            let mut gate = self.gate.lock().unwrap();
            match gate.as_ref() {
                Some(held) if held.range.is_none_or(|held| held == range) => gate.take(),
                _ => None,
            }
        };
        if let Some(gate) = gate {
            let _ = gate.entered.send(());
            let _ = gate.release.await;
        }
        loaded
    }

    async fn append(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.inner.append(chat, batch).await
    }

    fn append_blocking(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.inner.append_blocking(chat, batch)
    }
}

/// P1 → A1, P2 → A2, then a prompt after A1: the current chain is P1 A1 P3 A3, and A2 is on an
/// abandoned branch.
struct Branches {
    chat: ChatId,
    answer_two: MessageId,
    answer_three: MessageId,
}

async fn branches(agent: &Agent, folder: PathBuf) -> Branches {
    let chat = agent.create_chat(folder).await.unwrap();
    let one = complete(agent, chat, after_leaf("prompt one")).await;
    let answer_one = answer(agent, &one).await;
    let two = complete(agent, chat, after_leaf("prompt two")).await;
    let answer_two = answer(agent, &two).await;
    let three = complete(agent, chat, after(answer_one, "prompt three")).await;
    let answer_three = answer(agent, &three).await;
    Branches {
        chat,
        answer_two,
        answer_three,
    }
}

const BRANCH_REPLIES: [&str; 3] = ["answer one", "answer two", "answer three"];

/// The next prompt after the current leaf follows answer three, and the model sees that chain.
async fn assert_still_after_answer_three(
    agent: &Agent,
    model: &Script,
    lines: impl Fn() -> Vec<Line>,
    branches: &Branches,
) {
    let next = complete(agent, branches.chat, after_leaf("still here")).await;
    let request = model.last_request();
    assert!(
        request.contains("answer three") && !request.contains("answer two"),
        "{request}"
    );
    agent.sync().await.unwrap();
    let nodes = project(&lines());
    assert_eq!(
        node(&nodes, next.input_id()).parent,
        Some(branches.answer_three)
    );
}

#[tokio::test]
async fn a_branch_switch_reserves_the_chat_before_it_reads_the_branch() {
    let fixture = fixture();
    let memory = Arc::new(InMemoryTranscriptStore::new());
    let store = GatedStore::new(memory.clone());
    let replies = [
        BRANCH_REPLIES.as_slice(),
        &["answer four", "after the switch"],
    ]
    .concat();
    let model = Script::replies(&replies);
    let agent = agent(store.clone(), model.clone());
    let branches = branches(&agent, fixture.folder.clone()).await;
    let chat = branches.chat;
    let before = agent.snapshot(chat).await.unwrap().last_sequence;

    let (entered, release) = store.hold_next_load(Some(LoadRange::Full));
    let switching = tokio::spawn({
        let agent = agent.clone();
        let answer_two = branches.answer_two;
        async move {
            let turn = agent
                .advance(chat, after(answer_two, "prompt four"))
                .await
                .unwrap();
            let reached = outcome(&turn).await;
            (turn, reached)
        }
    });
    tokio::time::timeout(DEADLINE, entered)
        .await
        .unwrap()
        .unwrap();
    // The branch is being read: the Chat takes nothing in the meantime.
    assert!(matches!(
        agent.advance(chat, after_leaf("racing")).await,
        Err(AgentError::Invalid(message)) if message == "chat is busy"
    ));
    assert!(agent.compact(chat).await.is_err());
    release.send(()).unwrap();
    let (four, reached) = tokio::time::timeout(DEADLINE, switching)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(reached, TurnOutcome::Completed { .. }));
    let request = model.last_request();
    assert!(
        request.contains("answer two") && !request.contains("racing"),
        "{request}"
    );
    assert!(agent.snapshot(chat).await.unwrap().last_sequence > before);

    let next = complete(&agent, chat, after_leaf("after the switch")).await;
    assert!(model.last_request().contains("answer four"));
    assert_eq!(model.requests(), 5);
    agent.sync().await.unwrap();
    let lines = memory.lines(chat);
    assert!(!format!("{lines:?}").contains("racing"));
    let nodes = project(&lines);
    assert_eq!(
        node(&nodes, four.input_id()).parent,
        Some(branches.answer_two)
    );
    let answer_four = answer(&agent, &four).await;
    assert_eq!(node(&nodes, next.input_id()).parent, Some(answer_four));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_storage_failure_while_switching_branches_leaves_the_chat_usable() {
    let fixture = fixture();
    let memory = Arc::new(InMemoryTranscriptStore::new());
    let faulty = Arc::new(FaultyTranscriptStore::new(memory.clone()));
    let store = GatedStore::new(faulty.clone());
    let replies = [
        BRANCH_REPLIES.as_slice(),
        &["answer four", "still here", "answer five"],
    ]
    .concat();
    let model = Script::replies(&replies);
    let agent = agent(store.clone(), model.clone());
    let branches = branches(&agent, fixture.folder.clone()).await;
    let chat = branches.chat;

    // The branch is read, then the store fails every write.
    let (entered, release) = store.hold_next_load(Some(LoadRange::Full));
    let switching = tokio::spawn({
        let agent = agent.clone();
        let answer_two = branches.answer_two;
        async move {
            let turn = agent
                .advance(chat, after(answer_two, "prompt four"))
                .await?;
            Ok::<_, AgentError>(outcome(&turn).await)
        }
    });
    tokio::time::timeout(DEADLINE, entered)
        .await
        .unwrap()
        .unwrap();
    faulty.fail_appends();
    release.send(()).unwrap();
    let switched = tokio::time::timeout(DEADLINE, switching)
        .await
        .expect("a failed write does not hang the switch")
        .unwrap();
    faulty.heal();
    // Accepted before the writes failed, the input still ran; its Lines reach the store now.
    assert!(
        matches!(switched, Ok(TurnOutcome::Completed { .. })),
        "{switched:?}"
    );
    agent.sync().await.unwrap();
    let next = tokio::time::timeout(
        DEADLINE,
        agent.advance(chat, after(branches.answer_three, "after the failure")),
    )
    .await
    .expect("the Chat stays usable")
    .unwrap();
    outcome(&next).await;
    agent.shutdown().await;

    // Without a failure in its commit, a switch that cannot commit what came before refuses
    // itself and leaves the Chat where it was.
    let fixture = self::fixture();
    let memory = Arc::new(InMemoryTranscriptStore::new());
    let faulty = Arc::new(FaultyTranscriptStore::new(memory.clone()));
    let model = Script::replies(&[BRANCH_REPLIES.as_slice(), &["still here"]].concat());
    let agent = self::agent(faulty.clone(), model.clone());
    let branches = self::branches(&agent, fixture.folder.clone()).await;
    faulty.fail_appends();
    assert!(matches!(
        tokio::time::timeout(
            DEADLINE,
            agent.advance(branches.chat, after(branches.answer_two, "prompt four")),
        )
        .await
        .expect("the refusal does not hang"),
        Err(AgentError::Storage(_))
    ));
    faulty.heal();
    assert_still_after_answer_three(&agent, &model, || memory.lines(branches.chat), &branches)
        .await;
    agent.shutdown().await;
}

#[tokio::test]
async fn a_refused_input_off_the_current_chain_leaves_the_conversation_where_it_was() {
    let fixture = fixture();
    let memory = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&[BRANCH_REPLIES.as_slice(), &["still here"]].concat());
    let agent = agent(memory.clone(), model.clone());
    let branches = branches(&agent, fixture.folder.clone()).await;
    let chat = branches.chat;
    let before = agent.snapshot(chat).await.unwrap().messages;

    let empty = prompt(
        Uuid::new_v4(),
        ParentSelection::Message(branches.answer_two),
        "",
        Delivery::Queue,
    );
    let unknown_skill = TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Skill {
            after: ParentSelection::Message(branches.answer_two),
            target_loop: None,
            skill: tickloop_agent_runtime::SkillInvocation {
                name: "no-such-skill".into(),
                arguments: String::new(),
            },
        },
        delivery: Delivery::Queue,
        host_context: None,
    };
    let missing_attachment = TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Continue {
            after: ParentSelection::Message(branches.answer_two),
            target_loop: None,
            message: UserMessage {
                text: "read @missing.txt".into(),
                contexts: Vec::new(),
                paths: vec![fixture.folder.join("missing.txt")],
            },
        },
        delivery: Delivery::Queue,
        host_context: None,
    };
    for request in [empty, unknown_skill, missing_attachment] {
        assert!(agent.advance(chat, request).await.is_err());
        assert_eq!(agent.snapshot(chat).await.unwrap().messages, before);
    }
    assert_still_after_answer_three(&agent, &model, || memory.lines(chat), &branches).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn the_display_leaf_is_an_answer_awaiting_its_first_delta() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let turn = agent.advance(chat, after_leaf("a question")).await.unwrap();
    model.until_holding().await;
    agent.sync().await.unwrap();
    let started = answer(&agent, &turn).await;
    let leaf = store
        .received(chat)
        .into_iter()
        .rev()
        .find(|batch| !batch.lines.is_empty())
        .unwrap()
        .display_leaf;
    assert_eq!(leaf, Some(started));
    agent.shutdown().await;
}

#[tokio::test]
async fn an_answer_that_failed_without_content_is_the_display_leaf_to_continue_after() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Fail, Step::Reply(text("after the failure"))]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let turn = agent.advance(chat, after_leaf("a question")).await.unwrap();
    assert!(matches!(outcome(&turn).await, TurnOutcome::Failed { .. }));
    agent.sync().await.unwrap();
    let failed = answer(&agent, &turn).await;
    let leaf = store
        .received(chat)
        .into_iter()
        .rev()
        .find(|batch| !batch.lines.is_empty())
        .unwrap()
        .display_leaf
        .expect("a leaf");
    assert_eq!(leaf, failed);
    complete(&agent, chat, after(leaf, "again")).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn compaction_and_a_new_holder_keep_the_display_leaf() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::replies(&["answer one", "answer two", "the summary"]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    complete(&agent, chat, after_leaf("prompt one")).await;
    let request = after_leaf("prompt two");
    let two = complete(&agent, chat, request.clone()).await;
    let answer_two = answer(&agent, &two).await;
    agent.sync().await.unwrap();
    let written = store.received(chat).len();
    agent.compact(chat).await.unwrap();
    agent.sync().await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let next = self::agent(store.clone(), Script::replies(&[]));
    let repeated = next.advance(chat, request).await.unwrap();
    assert!(matches!(
        outcome(&repeated).await,
        TurnOutcome::Completed { .. }
    ));
    next.sync().await.unwrap();
    let leaves = store.received(chat)[written..]
        .iter()
        .map(|batch| batch.display_leaf)
        .collect::<Vec<_>>();
    assert!(!leaves.is_empty());
    assert!(
        leaves.iter().all(|leaf| *leaf == Some(answer_two)),
        "{leaves:?}"
    );
    next.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_an_advance_restoring_its_chat() {
    let fixture = fixture();
    let memory = Arc::new(InMemoryTranscriptStore::new());
    let faulty = Arc::new(FaultyTranscriptStore::new(memory.clone()));
    let store = GatedStore::new(faulty.clone());
    let first = agent(store.clone(), Script::replies(&["answer"]));
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let request = after_leaf("once");
    complete(&first, chat, request.clone()).await;
    first.shutdown().await;
    drop(first);

    let next = agent(store.clone(), Script::replies(&[]));
    let (entered, release) = store.hold_next_load(None);
    let advancing = tokio::spawn({
        let next = next.clone();
        async move {
            next.advance(chat, request)
                .await
                .map(|turn| turn.receipt().run_id)
        }
    });
    tokio::time::timeout(DEADLINE, entered)
        .await
        .unwrap()
        .unwrap();
    let shutting_down = tokio::spawn({
        let next = next.clone();
        async move { next.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !shutting_down.is_finished(),
        "shutdown waits for the admitted restoration"
    );
    release.send(()).unwrap();
    tokio::time::timeout(DEADLINE, advancing)
        .await
        .unwrap()
        .unwrap()
        .expect("the settled input answers with its receipt");
    tokio::time::timeout(DEADLINE, shutting_down)
        .await
        .unwrap()
        .unwrap();
    let appended = faulty.appends();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        faulty.appends(),
        appended,
        "nothing is written after shutdown"
    );
    assert!(matches!(
        next.advance(chat, after_leaf("too late")).await,
        Err(AgentError::Invalid(message)) if message.contains("shut down")
    ));
}

/// The host crashed while the input waited in the queue: a new holder runs it once.
#[tokio::test]
async fn an_input_lost_before_it_started_runs_once_for_a_new_holder() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang]);
    let first = agent(store.clone(), model.clone());
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    first.advance(chat, after_leaf("long work")).await.unwrap();
    model.until_holding().await;
    let request = after_leaf("queued behind it");
    let queued = first.advance(chat, request.clone()).await.unwrap();
    first.sync().await.unwrap();
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(chat, store.lines(chat), StorePosition(1));

    let model = Script::replies(&["the queued answer"]);
    let next = agent(crashed.clone(), model.clone());
    let recovered = complete(&next, chat, request.clone()).await;
    assert_ne!(recovered.receipt().run_id, queued.receipt().run_id);
    assert!(model.last_request().contains("queued behind it"));
    let again = next.advance(chat, request).await.unwrap();
    assert_eq!(again.receipt().run_id, recovered.receipt().run_id);
    assert!(matches!(
        outcome(&again).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(model.requests(), 1);
    assert_eq!(
        next.snapshot(chat)
            .await
            .unwrap()
            .message_ids
            .values()
            .filter(|id| **id == queued.input_id())
            .count(),
        1
    );
    next.shutdown().await;
    first.shutdown().await;
}

/// Two holders' worth of the same recovery at once: one attempt, one model request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_duplicates_of_a_lost_answer_continue_it_once() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang]);
    let first = agent(store.clone(), model.clone());
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let request = after_leaf("a question");
    first.advance(chat, request.clone()).await.unwrap();
    model.until_holding().await;
    tokio::time::timeout(DEADLINE, async {
        while !store
            .lines(chat)
            .iter()
            .any(|line| matches!(line, Line::Response(_)))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(chat, store.lines(chat), StorePosition(1));

    let model = Script::replies(&["the continued answer"]);
    let next = agent(crashed.clone(), model.clone());
    let (left, right) = tokio::join!(
        next.advance(chat, request.clone()),
        next.advance(chat, request.clone())
    );
    let (left, right) = (left.unwrap(), right.unwrap());
    assert_eq!(left.receipt().run_id, right.receipt().run_id);
    for turn in [&left, &right] {
        assert!(matches!(outcome(turn).await, TurnOutcome::Completed { .. }));
    }
    assert_eq!(model.requests(), 1);
    let attempts = next
        .input_state(chat, request.input_id)
        .await
        .unwrap()
        .unwrap()
        .responses
        .len();
    assert_eq!(attempts, 2, "the lost attempt and one continuation");
    next.shutdown().await;
    first.shutdown().await;
}

/// A provider may number calls from the start again in a new attempt. A call id the lost attempt
/// already used fails the continued attempt instead of repeating a call within the Run, which
/// would leave the Chat's conversation invalid.
#[tokio::test]
async fn a_continued_answer_reusing_a_call_id_of_its_lost_attempt_fails_and_the_chat_stays_usable()
{
    let fixture = fixture();
    std::fs::write(fixture.folder.join("notes.txt"), "the notes").unwrap();
    let read_notes = || call("read", "Read", json!({"file_path": "notes.txt"}));
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Reply(read_notes()), Step::Hang]);
    let first = agent(store.clone(), model.clone());
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let request = after_leaf("summarize my notes");
    first.advance(chat, request.clone()).await.unwrap();
    model.until_holding().await;
    first.sync().await.unwrap();
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(chat, store.lines(chat), StorePosition(1));

    let model = Script::new(vec![
        Step::Reply(read_notes()),
        Step::Reply(text("a later answer")),
    ]);
    let next = agent(crashed.clone(), model.clone());
    let continued = next.advance(chat, request).await.unwrap();
    match outcome(&continued).await {
        TurnOutcome::Failed { error } => assert!(error.contains("repeated tool call"), "{error}"),
        other => panic!("{other:?}"),
    }
    let snapshot = next.snapshot(chat).await.unwrap();
    tickloop_agent_protocol::projection::history_calls(&snapshot)
        .expect("the conversation stays valid");
    complete(&next, chat, after_leaf("try again")).await;
    next.shutdown().await;
    first.shutdown().await;
}
