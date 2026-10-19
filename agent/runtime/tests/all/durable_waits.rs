//! Durable waits: a Run that asks a question suspends in its Transcript. Under durable wait
//! hosting a shutdown leaves it suspended and another instance rebuilds it and goes on with the
//! reply; under process wait hosting a load ends it as a lost host, as before.

use std::sync::Arc;

use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Content, EventKind, LoopState, PromptPolicy, QuestionAnswer,
    QuestionRequest, Role,
};
use tickloop_agent_runtime::transcript::{StorePosition, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, AgentError, Delivery, HarnessOptions, HarnessSeams, ParentSelection, PersonWait,
    PersonWaitKind, QuestionReply, Quiescence, ToolPolicy, Turn, TurnOp, TurnOutcome, TurnRequest,
    TurnWait, UserMessage, WaitHosting, Waits,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, single_question, text};
use tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME;
use tickloop_agent_transcript::display::{DisplayKind, DisplayStatus, project};
use tickloop_agent_transcript::{
    AbandonReason, InterruptedBy, Line, ResponseEnding, ResponseState, SettledOutcome, WaitState,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const DONE: &str = "Report ready.";
const QUESTION_CALL: &str = "question";

fn harness(
    store: Arc<dyn TranscriptStore>,
    model: Arc<ScriptedModel>,
    waits: WaitHosting,
) -> Agent {
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

/// A Harness without a Folder whose Chats may ask questions.
fn options(waits: WaitHosting) -> HarnessOptions {
    HarnessOptions {
        model: config(),
        settings: None,
        host: AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::product_defaults()
        },
        tools: ToolPolicy {
            questions: true,
            ..ToolPolicy::web_fetch_only()
        },
        host_rules: Vec::new(),
        task_tools: None,
        min_schedule_interval: tickloop_agent_runtime::LOCAL_MIN_SCHEDULE_INTERVAL,
        waits,
    }
}

fn asks() -> Vec<Content> {
    call(
        QUESTION_CALL,
        ASK_USER_QUESTION_TOOL_NAME,
        single_question("Which report?"),
    )
}

fn prompt(text: &str, after: ParentSelection, delivery: Delivery) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
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

fn answer(request: &QuestionRequest, reply: QuestionReply) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Answer {
            request: request.id,
            reply,
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

fn weekly() -> QuestionReply {
    QuestionReply::Answered(vec![QuestionAnswer::other("Weekly")])
}

async fn outcome(turn: &Turn) -> TurnOutcome {
    tokio::time::timeout(std::time::Duration::from_secs(10), turn.outcome())
        .await
        .expect("the turn reaches an outcome")
        .unwrap()
}

/// Asks the question in a durable instance, which then shuts down as a host ends its holding.
async fn asked_and_released(
    store: &Arc<InMemoryTranscriptStore>,
) -> (ChatId, Turn, QuestionRequest) {
    let model = Arc::new(ScriptedModel::new(vec![asks()]));
    let asking = harness(store.clone(), model, WaitHosting::Durable);
    let chat = asking.create_chat_without_folder().await.unwrap();
    let turn = asking
        .advance(
            chat,
            prompt(
                "Prepare the report.",
                ParentSelection::Root,
                Delivery::SendNow,
            ),
        )
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Question(request)) = outcome(&turn).await else {
        panic!("the turn waits for the question");
    };
    assert_eq!(
        asking.quiesce(chat).await,
        Quiescence::Waiting(Waits {
            people: vec![PersonWait {
                chat,
                request: request.id,
                kind: PersonWaitKind::Question,
            }],
            wake_at: None,
        })
    );
    asking.shutdown().await;
    asking.sync().await.unwrap();
    (chat, turn, request)
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

fn wait_states(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Wait(entry) => Some(match &entry.state {
                WaitState::Open => "open".to_string(),
                WaitState::Resolved { .. } => "resolved".into(),
                WaitState::Appended => "appended".into(),
                WaitState::Abandoned { reason } => format!("abandoned {reason:?}"),
            }),
            _ => None,
        })
        .collect()
}

fn results_of(agent_messages: &[tickloop_agent_protocol::Message], call_id: &str) -> Vec<String> {
    agent_messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.text().into_owned()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_durable_shutdown_leaves_the_question_suspended_and_another_instance_answers_it() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let (chat, turn, request) = asked_and_released(&store).await;
    let run = turn.receipt().run_id;
    let lines = store.lines(chat);
    assert_eq!(
        response_states(&lines, run).last().unwrap().1,
        ResponseState::Suspended
    );
    assert_eq!(wait_states(&lines), ["open"]);

    let model = Arc::new(ScriptedModel::new(vec![text(DONE)]));
    let answering = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let reply = answering
        .advance(chat, answer(&request, weekly()))
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { into } if into == run));
    assert!(matches!(
        answering
            .settled_input(chat, turn.input_id())
            .await
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "the question is not asked again");
    let last = requests[0].messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(format!("{last:?}").contains("Weekly"));
    answering.sync().await.unwrap();
    let lines = store.lines(chat);
    // One attempt throughout; `Resumed` and the end may reach the store in one change.
    let states = response_states(&lines, run);
    assert!(
        states.iter().all(|(attempt, _)| *attempt == 1),
        "{states:?}"
    );
    assert_eq!(
        states
            .iter()
            .map(|(_, state)| state.clone())
            .collect::<Vec<_>>()[..2],
        [ResponseState::Started, ResponseState::Suspended]
    );
    assert_eq!(
        states.last().unwrap().1,
        ResponseState::Ended {
            ending: ResponseEnding::Completed
        }
    );
    // The reply and its result may reach the store in one change too.
    let waits = wait_states(&lines);
    assert_eq!(
        (
            waits.first().unwrap().as_str(),
            waits.last().unwrap().as_str()
        ),
        ("open", "appended")
    );
    assert_eq!(
        results_of(
            &answering.snapshot(chat).await.unwrap().messages,
            QUESTION_CALL
        )
        .len(),
        1
    );
    // The reply is a ledger input: repeating it repeats nothing.
    let repeated = answering.advance(chat, answer(&request, weekly())).await;
    assert!(matches!(
        repeated,
        Err(AgentError::Invalid(message)) if message == "the question is no longer pending"
    ));
    let again = answering
        .advance(
            chat,
            TurnRequest {
                input_id: reply.input_id(),
                ..answer(&request, weekly())
            },
        )
        .await
        .unwrap();
    assert!(matches!(outcome(&again).await, TurnOutcome::Merged { into } if into == run));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    answering.shutdown().await;
}

/// The stored Lines up to the first chain entry carrying the question's result: when `with_it`,
/// that entry is kept and the lifecycle records committed with it are not.
fn cut_at_result(lines: &[Line], with_it: bool) -> Vec<Line> {
    let at = lines
        .iter()
        .position(|line| {
            matches!(line, Line::User(entry) if entry.message.content.iter().any(|content| matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == QUESTION_CALL)))
        })
        .expect("the result reached the chain");
    lines[..at + usize::from(with_it)].to_vec()
}

const HOLD: &str = "Hold the slot.";

/// Holds the execution slot with the request of a prompt saying [`HOLD`] until released.
struct SlotHolder {
    script: ScriptedModel,
    holding: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ModelClient for SlotHolder {
    async fn generate_title(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.script.generate_title(request, events, cancel).await
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        self.script.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if format!("{:?}", request.messages.last()).contains(HOLD) {
            self.holding.notify_one();
            tokio::select! {
                _ = self.release.notified() => {}
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
        }
        self.script.complete(request, events, cancel).await
    }
}

/// Stored Lines of an answered question whose Run, after the reply committed, waited for the
/// slot another Loop's Run held: up to the reply, and up to the result with the lifecycle
/// records committed with it left out.
async fn replied_while_the_slot_was_held() -> (ChatId, Turn, TurnRequest, Vec<Line>, Vec<Line>) {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let (chat, turn, request) = asked_and_released(&store).await;
    let model = Arc::new(SlotHolder {
        script: ScriptedModel::new(vec![text("Held."), text(DONE)]),
        holding: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let answering = Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store.clone()),
            environment: None,
            models: ChosenModels::new(model.clone()),
        },
        options(WaitHosting::Durable),
    )
    .unwrap();
    answering
        .advance(
            chat,
            prompt(HOLD, ParentSelection::CurrentLeaf, Delivery::Queue),
        )
        .await
        .unwrap();
    model.holding.notified().await;
    let reply = answer(&request, weekly());
    answering.advance(chat, reply.clone()).await.unwrap();
    answering.sync().await.unwrap();
    let replied = store.lines(chat);
    model.release.notify_one();
    answering
        .settled_input(chat, turn.input_id())
        .await
        .unwrap();
    answering.sync().await.unwrap();
    let with_result = cut_at_result(&store.lines(chat), true);
    answering.shutdown().await;
    (chat, turn, reply, replied, with_result)
}

fn lost_store(chat: ChatId, lines: Vec<Line>) -> Arc<InMemoryTranscriptStore> {
    let lost = Arc::new(InMemoryTranscriptStore::new());
    lost.seed(chat, lines, StorePosition(1));
    lost
}

#[tokio::test]
async fn a_reply_committed_before_its_result_reached_the_chain_is_appended_by_the_next_holder() {
    let (chat, turn, reply, replied, _) = replied_while_the_slot_was_held().await;
    let store = lost_store(chat, replied);
    assert_eq!(wait_states(&store.lines(chat)), ["open", "resolved"]);
    let model = Arc::new(ScriptedModel::new(vec![text(DONE), text("Held again.")]));
    let next = harness(store.clone(), model.clone(), WaitHosting::Durable);
    // The repeated reply only attaches; loading the Chat rebuilds the Run, which goes on.
    let repeated = next.advance(chat, reply).await.unwrap();
    assert!(matches!(
        outcome(&repeated).await,
        TurnOutcome::Merged { .. }
    ));
    assert!(matches!(
        next.settled_input(chat, turn.input_id()).await.unwrap(),
        SettledOutcome::Completed { .. }
    ));
    // The rebuilt Run goes first; the slot holder lost its host and goes on after it.
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(format!("{:?}", requests[0].messages).contains("Weekly"));
    let snapshot = next.snapshot(chat).await.unwrap();
    assert_eq!(results_of(&snapshot.messages, QUESTION_CALL).len(), 1);
    next.sync().await.unwrap();
    let states = response_states(&store.lines(chat), turn.receipt().run_id);
    assert!(
        states.iter().all(|(attempt, _)| *attempt == 1),
        "no other attempt: {states:?}"
    );
    assert_eq!(
        states.last().unwrap().1,
        ResponseState::Ended {
            ending: ResponseEnding::Completed
        }
    );
    next.shutdown().await;
}

#[tokio::test]
async fn a_result_on_the_chain_whose_resume_was_lost_is_not_appended_again() {
    let (chat, turn, reply, _, with_result) = replied_while_the_slot_was_held().await;
    let store = lost_store(chat, with_result);
    let lines = store.lines(chat);
    assert_eq!(wait_states(&lines), ["open", "resolved"]);
    assert_eq!(
        response_states(&lines, turn.receipt().run_id)
            .last()
            .unwrap()
            .1,
        ResponseState::Suspended
    );
    let model = Arc::new(ScriptedModel::new(vec![text(DONE), text("Held again.")]));
    let next = harness(store.clone(), model.clone(), WaitHosting::Durable);
    next.advance(chat, reply).await.unwrap();
    assert!(matches!(
        next.settled_input(chat, turn.input_id()).await.unwrap(),
        SettledOutcome::Completed { .. }
    ));
    let snapshot = next.snapshot(chat).await.unwrap();
    assert_eq!(results_of(&snapshot.messages, QUESTION_CALL).len(), 1);
    next.sync().await.unwrap();
    let lines = store.lines(chat);
    assert_eq!(wait_states(&lines), ["open", "resolved", "appended"]);
    let states = response_states(&lines, turn.receipt().run_id);
    assert!(
        states.iter().all(|(attempt, _)| *attempt == 1),
        "{states:?}"
    );
    assert_eq!(
        states.last().unwrap().1,
        ResponseState::Ended {
            ending: ResponseEnding::Completed
        }
    );
    next.shutdown().await;
}

/// The question's call result when a prompt sent now interrupts the waiting Run, with ids
/// replaced by their order of appearance.
async fn interrupted_result(agent: &Agent, chat: ChatId) -> String {
    let turn = agent
        .advance(
            chat,
            prompt(
                "Never mind.",
                ParentSelection::CurrentLeaf,
                Delivery::SendNow,
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. }
    ));
    let snapshot = agent.snapshot(chat).await.unwrap();
    let results = results_of(&snapshot.messages, QUESTION_CALL);
    assert_eq!(results.len(), 1);
    results[0].clone()
}

#[tokio::test]
async fn a_prompt_sent_now_interrupts_a_rebuilt_question_as_it_does_the_asking_run() {
    let in_process = {
        let model = Arc::new(ScriptedModel::new(vec![asks(), text(DONE)]));
        let agent = harness(
            Arc::new(InMemoryTranscriptStore::new()),
            model,
            WaitHosting::Durable,
        );
        let chat = agent.create_chat_without_folder().await.unwrap();
        let turn = agent
            .advance(
                chat,
                prompt(
                    "Prepare the report.",
                    ParentSelection::Root,
                    Delivery::SendNow,
                ),
            )
            .await
            .unwrap();
        assert!(matches!(outcome(&turn).await, TurnOutcome::Waiting(_)));
        let result = interrupted_result(&agent, chat).await;
        agent.shutdown().await;
        result
    };
    let store = Arc::new(InMemoryTranscriptStore::new());
    let (chat, turn, _) = asked_and_released(&store).await;
    let model = Arc::new(ScriptedModel::new(vec![text(DONE)]));
    let next = harness(store.clone(), model, WaitHosting::Durable);
    let rebuilt = interrupted_result(&next, chat).await;
    let shape = |result: &str| {
        result
            .split_whitespace()
            .map(|word| match Uuid::parse_str(word) {
                Ok(_) => "<id>",
                Err(_) => word,
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(shape(&rebuilt), shape(&in_process));
    let snapshot = next.snapshot(chat).await.unwrap();
    let goal = snapshot
        .loops
        .iter()
        .find(|goal| goal.id == turn.receipt().loop_id)
        .unwrap();
    assert_eq!(goal.state, LoopState::Interrupted);
    next.sync().await.unwrap();
    let states = response_states(&store.lines(chat), turn.receipt().run_id);
    assert_eq!(
        states.last().unwrap().1,
        ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::User
            }
        }
    );
    assert_eq!(
        wait_states(&store.lines(chat)).last().unwrap(),
        &format!("abandoned {:?}", AbandonReason::Interrupted)
    );
    next.shutdown().await;
}

#[tokio::test]
async fn a_worker_restarted_mid_question_ends_it_as_a_lost_host_as_before() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Arc::new(ScriptedModel::new(vec![asks()]));
    let worker = harness(store.clone(), model, WaitHosting::Process);
    let chat = worker.create_chat_without_folder().await.unwrap();
    let mut events = worker.subscribe();
    let turn = worker
        .advance(
            chat,
            prompt(
                "Prepare the report.",
                ParentSelection::Root,
                Delivery::SendNow,
            ),
        )
        .await
        .unwrap();
    loop {
        if let EventKind::QuestionAsked { .. } = next_event(&mut events).await.kind {
            break;
        }
    }
    worker.sync().await.unwrap();
    let run = turn.receipt().run_id;
    assert_eq!(
        response_states(&store.lines(chat), run).last().unwrap().1,
        ResponseState::Suspended
    );
    // The Worker dies without shutting down; a new one loads what the store holds.
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(chat, store.lines(chat), StorePosition(1));
    let restarted = harness(
        crashed.clone(),
        Arc::new(ScriptedModel::new(Vec::new())),
        WaitHosting::Process,
    );
    let snapshot = restarted.resume_chat(chat).await.unwrap();
    assert!(snapshot.questions.is_empty());
    assert_eq!(
        snapshot
            .loops
            .iter()
            .find(|goal| goal.id == turn.receipt().loop_id)
            .unwrap()
            .state,
        LoopState::Interrupted
    );
    // The batch had no result yet, so the load drops it, as it always has.
    assert!(!format!("{:?}", snapshot.messages).contains(ASK_USER_QUESTION_TOOL_NAME));
    restarted.sync().await.unwrap();
    let lines = crashed.lines(chat);
    assert_eq!(
        response_states(&lines, run).last().unwrap().1,
        ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::Host
            }
        }
    );
    assert_eq!(
        wait_states(&lines).last().unwrap(),
        &format!("abandoned {:?}", AbandonReason::HostLost)
    );
    let answer_node = project(&lines)
        .into_iter()
        .find(|node| matches!(node.kind, DisplayKind::Answer { .. }))
        .unwrap();
    assert_eq!(
        answer_node.status,
        DisplayStatus::Interrupted {
            by: InterruptedBy::Host
        }
    );
    restarted.shutdown().await;
    worker.shutdown().await;
}

/// Two deliveries of one reply racing each other: both find the one reply, and it is applied
/// once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_deliveries_of_one_reply_apply_it_once() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let (chat, turn, request) = asked_and_released(&store).await;
    let model = Arc::new(ScriptedModel::new(vec![text(DONE)]));
    let answering = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let reply = answer(&request, weekly());
    let (first, second) = tokio::join!(
        answering.advance(chat, reply.clone()),
        answering.advance(chat, reply.clone())
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.receipt().run_id, second.receipt().run_id);
    assert!(matches!(
        answering
            .settled_input(chat, turn.input_id())
            .await
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let snapshot = answering.snapshot(chat).await.unwrap();
    assert_eq!(results_of(&snapshot.messages, QUESTION_CALL).len(), 1);
    answering.shutdown().await;
}
