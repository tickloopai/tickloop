//! @tickloop requests a durable host delivers with `TurnOp::Deliver`: the delivery names the
//! input on every instance, so a repeated delivery runs nothing again, before or after the
//! conversation is compacted. A delivery queues behind the work its Loop's Chat restored, goes
//! on with a Loop that was interrupted, failed or completed, loads the family of the member it
//! targets, and is discarded by a Loop that has ended. A durable load keeps a Loop's artifact
//! links receiving, for the host to deliver to.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;
use tickloop_agent_artifacts::ClaimedFeedback;
use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Content, EventSourceBinding, ExecutionMode, LoopId, LoopState,
    PromptPolicy, Role,
};
use tickloop_agent_runtime::transcript::{StorePosition, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, AgentError, DeliveryBinding, ExternalDelivery, HarnessOptions, HarnessSeams,
    ParentSelection, Quiescence, ToolPolicy, TurnOutcome, TurnRequest, WaitHosting, Waits,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::routed::{RoutedModel, has_tool_result};
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{call, config, text};
use tickloop_agent_transcript::{DiscardReason, InputOp, InputState, Line, SettledOutcome};
use uuid::Uuid;

use super::durable_queue::{
    Script, Step, WAIT, harness, input_state, lost_holder, prompt, settled,
};

const ORIGIN: &str = "https://tickloop.ai/artifact";
const ACCOUNT: &str = "owner";
const ARTIFACT: &str = "doc";

/// The delivery of the request `feedback` that the service handed this receiver.
fn delivery(feedback: &str) -> ExternalDelivery {
    claimed(
        feedback,
        Uuid::new_v4(),
        "fence-secret",
        "Tighten the title",
    )
}

/// One claim of the delivery `delivery_id`, under its own fencing token, with the thread as it
/// stood then.
fn claimed(feedback: &str, delivery_id: Uuid, fence: &str, thread: &str) -> ExternalDelivery {
    ExternalDelivery::from_claim(
        ORIGIN,
        ACCOUNT,
        ARTIFACT,
        &ClaimedFeedback {
            delivery_id: delivery_id.to_string(),
            feedback_id: feedback.into(),
            claim_id: Uuid::new_v4().to_string(),
            fencing_token: fence.into(),
            lease_expires_at: "2026-10-20T00:00:00Z".into(),
            input: json!({"messages": [{"text": thread}]}),
        },
    )
    .expect("the claim reaches the model")
}

fn request_delivery_id(request: &TurnRequest) -> Uuid {
    match &request.op {
        tickloop_agent_runtime::TurnOp::Deliver { delivery, .. } => delivery.delivery_id(),
        _ => unreachable!("a delivery"),
    }
}

fn heading(feedback: &str) -> String {
    format!("@tickloop feedback {feedback} on artifact {ARTIFACT}")
}

async fn outcome(agent: &Agent, chat: ChatId, request: TurnRequest) -> TurnOutcome {
    let turn = agent.advance(chat, request).await.unwrap();
    tokio::time::timeout(WAIT, turn.outcome())
        .await
        .expect("the turn reaches an outcome")
        .unwrap()
}

/// A Chat on `agent` whose first prompt has completed, and that prompt's Loop.
async fn ready_chat(agent: &Agent) -> (ChatId, LoopId) {
    let chat = agent.create_chat_without_folder().await.unwrap();
    let ready = prompt("get ready", ParentSelection::CurrentLeaf);
    let turn = agent.advance(chat, ready.clone()).await.unwrap();
    settled(agent, chat, ready.input_id).await;
    (chat, turn.receipt().loop_id)
}

async fn loop_state(agent: &Agent, chat: ChatId, loop_id: LoopId) -> LoopState {
    agent
        .snapshot(chat)
        .await
        .unwrap()
        .loops
        .into_iter()
        .find(|goal| goal.id == loop_id)
        .expect("the Loop is recorded")
        .state
}

fn feedback_inputs(lines: &[Line]) -> HashSet<Uuid> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Input(entry) if entry.op == InputOp::Feedback => Some(entry.input_id),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_delivery_repeated_on_another_instance_or_after_compaction_runs_once() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(Vec::new());
    let first = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let (chat, target) = ready_chat(&first).await;
    let request = TurnRequest::deliver(target, delivery("request-1"));
    let delivered = first.advance(chat, request.clone()).await.unwrap();
    let delivery_id = request_delivery_id(&request);
    assert!(matches!(
        tokio::time::timeout(WAIT, delivered.outcome())
            .await
            .unwrap()
            .unwrap(),
        TurnOutcome::Completed { .. }
    ));
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains(&heading("request-1")));
    assert!(
        !requests[1].contains("fence-secret"),
        "the claim's fencing token never reaches the model"
    );

    first.compact(chat).await.unwrap();
    let compacted = model.requests().len();
    let repeated = first.advance(chat, request.clone()).await.unwrap();
    assert!(matches!(
        repeated.outcome().await.unwrap(),
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(model.requests().len(), compacted, "the repeat runs nothing");
    first.sync().await.unwrap();
    first.shutdown().await;

    let model = Script::new(Vec::new());
    let next = harness(store.clone(), model.clone(), WaitHosting::Durable);
    assert!(matches!(
        outcome(&next, chat, request.clone()).await,
        TurnOutcome::Completed { .. }
    ));
    assert!(model.requests().is_empty(), "another instance runs nothing");
    // The service hands the delivery out again under a new claim, with the thread grown since.
    let reclaimed = TurnRequest::deliver(
        target,
        claimed(
            "request-1",
            delivery_id,
            "another-fence",
            "Tighten the title. Done: Tickloop (Agent) replied.",
        ),
    );
    assert_eq!(reclaimed.input_id, request.input_id);
    assert!(matches!(
        outcome(&next, chat, reclaimed).await,
        TurnOutcome::Completed { .. }
    ));
    assert!(model.requests().is_empty(), "a later claim runs nothing");
    next.sync().await.unwrap();
    assert_eq!(
        feedback_inputs(&store.lines(chat)),
        HashSet::from([request.input_id]),
        "one ledger input under the id the delivery names"
    );
    let mut renamed = request;
    renamed.input_id = Uuid::new_v4();
    assert!(matches!(
        next.advance(chat, renamed).await,
        Err(AgentError::Invalid(_))
    ));
    next.shutdown().await;
}

#[tokio::test]
async fn interrupted_failed_and_completed_loops_take_a_delivery_and_a_stopped_one_discards_it() {
    // Interrupted by the user.
    let model = Script::new(vec![Step::Hang]);
    let agent = harness(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        WaitHosting::Durable,
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let long = agent
        .advance(chat, prompt("a long task", ParentSelection::CurrentLeaf))
        .await
        .unwrap();
    model.until_holding().await;
    agent.interrupt(chat).await.unwrap();
    settled(&agent, chat, long.input_id()).await;
    let target = long.receipt().loop_id;
    assert_eq!(
        loop_state(&agent, chat, target).await,
        LoopState::Interrupted
    );
    let request = TurnRequest::deliver(target, delivery("after-interrupt"));
    assert!(matches!(
        outcome(&agent, chat, request).await,
        TurnOutcome::Completed { .. }
    ));
    assert!(
        model
            .requests()
            .last()
            .unwrap()
            .contains(&heading("after-interrupt"))
    );
    agent.shutdown().await;

    // Failed.
    let model = Script::new(vec![Step::Fail]);
    let agent = harness(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        WaitHosting::Durable,
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let broken = prompt("a broken task", ParentSelection::CurrentLeaf);
    let failing = agent.advance(chat, broken.clone()).await.unwrap();
    assert!(matches!(
        settled(&agent, chat, broken.input_id).await,
        SettledOutcome::Failed { .. }
    ));
    let target = failing.receipt().loop_id;
    assert_eq!(loop_state(&agent, chat, target).await, LoopState::Failed);
    let request = TurnRequest::deliver(target, delivery("after-failure"));
    assert!(matches!(
        outcome(&agent, chat, request).await,
        TurnOutcome::Completed { .. }
    ));
    assert!(
        model
            .requests()
            .last()
            .unwrap()
            .contains(&heading("after-failure"))
    );
    agent.shutdown().await;

    // Completed, then stopped.
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(Vec::new());
    let agent = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let (chat, target) = ready_chat(&agent).await;
    assert_eq!(loop_state(&agent, chat, target).await, LoopState::Completed);
    assert!(matches!(
        outcome(
            &agent,
            chat,
            TurnRequest::deliver(target, delivery("after-completion"))
        )
        .await,
        TurnOutcome::Completed { .. }
    ));
    agent.stop(chat, target).await.unwrap();
    let requests = model.requests().len();
    let late = TurnRequest::deliver(target, delivery("after-stop"));
    for _ in 0..2 {
        assert!(matches!(
            outcome(&agent, chat, late.clone()).await,
            TurnOutcome::Discarded {
                reason: DiscardReason::LoopEnded
            }
        ));
    }
    assert_eq!(
        model.requests().len(),
        requests,
        "a stopped Loop runs nothing"
    );
    agent.sync().await.unwrap();
    assert_eq!(
        input_state(&store.lines(chat), late.input_id),
        Some(InputState::Settled {
            outcome: SettledOutcome::Discarded {
                reason: DiscardReason::LoopEnded
            }
        })
    );
    // A Loop the Chat never had is a misrouted delivery, not an ended Loop.
    assert!(matches!(
        agent
            .advance(
                chat,
                TurnRequest::deliver(Uuid::new_v4(), delivery("to-nowhere"))
            )
            .await,
        Err(AgentError::Invalid(_))
    ));
    agent.shutdown().await;
}

#[tokio::test]
async fn deliveries_queue_behind_the_work_a_load_restores() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang]);
    let first = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let chat = first.create_chat_without_folder().await.unwrap();
    let lost = prompt("first prompt", ParentSelection::CurrentLeaf);
    let running = first.advance(chat, lost.clone()).await.unwrap();
    model.until_holding().await;
    let target = running.receipt().loop_id;
    let queued_delivery = TurnRequest::deliver(target, delivery("request-1"));
    first.advance(chat, queued_delivery.clone()).await.unwrap();
    let queued_prompt = prompt("second prompt", ParentSelection::CurrentLeaf);
    first.advance(chat, queued_prompt.clone()).await.unwrap();
    let store = lost_holder(&first, &store, chat).await;
    assert_eq!(
        input_state(&store.lines(chat), queued_delivery.input_id),
        Some(InputState::Accepted),
        "the queued delivery is accepted with its content"
    );

    let model = Script::new(Vec::new());
    let next = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let later = TurnRequest::deliver(target, delivery("request-2"));
    next.advance(chat, later.clone()).await.unwrap();
    for input in [
        lost.input_id,
        queued_prompt.input_id,
        queued_delivery.input_id,
        later.input_id,
    ] {
        assert!(matches!(
            settled(&next, chat, input).await,
            SettledOutcome::Completed { .. }
        ));
    }
    let requests = model.requests();
    assert_eq!(requests.len(), 4, "{requests:#?}");
    assert!(requests[0].contains("first prompt") && !requests[0].contains("second prompt"));
    assert!(requests[1].contains("second prompt") && !requests[1].contains("request-1"));
    assert!(requests[2].contains(&heading("request-1")) && !requests[2].contains("request-2"));
    assert!(requests[3].contains(&heading("request-2")));
    // The restored delivery is the ledger's: delivering it again runs nothing.
    assert!(matches!(
        outcome(&next, chat, queued_delivery).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(model.requests().len(), 4);
    next.shutdown().await;
    first.shutdown().await;
}

const DELEGATION_CALL: &str = "delegate";

/// A root that delegates in the foreground to a child, which answers whatever it is given.
fn delegating_model() -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(|request| {
        if is_member(request) {
            text("child done")
        } else if !has_tool_result(request, DELEGATION_CALL) {
            call(
                DELEGATION_CALL,
                "Subagent",
                json!({"task": "watch the document", "name": "watcher"}),
            )
        } else {
            text("root done")
        }
    }))
}

/// The request comes from a delegated member, whose conversation opens with its task.
fn is_member(request: &ModelRequest) -> bool {
    request
        .messages
        .iter()
        .find(|message| message.role != Role::System)
        .and_then(|message| message.content.first())
        .is_some_and(
            |content| matches!(content, Content::Text { text } if text.starts_with("<agent-message>")),
        )
}

fn family_harness(store: Arc<dyn TranscriptStore>, model: Arc<RoutedModel>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            model: config(),
            settings: None,
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                execution_mode_override: Some(ExecutionMode::FullAccess),
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                delegation: true,
                family_messages: true,
                ..ToolPolicy::web_fetch_only()
            },
            host_rules: Vec::new(),
            task_tools: None,
            min_schedule_interval: tickloop_agent_runtime::LOCAL_MIN_SCHEDULE_INTERVAL,
            waits: WaitHosting::Durable,
        },
    )
    .unwrap()
}

#[tokio::test]
async fn a_delivery_to_a_member_loads_its_whole_family() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let delegating = family_harness(store.clone(), delegating_model());
    let root = delegating.create_chat_without_folder().await.unwrap();
    let task = prompt("delegate the watching", ParentSelection::CurrentLeaf);
    delegating.advance(root, task.clone()).await.unwrap();
    assert!(matches!(
        settled(&delegating, root, task.input_id).await,
        SettledOutcome::Completed { .. }
    ));
    let member = delegating.snapshot(root).await.unwrap().family_completions[0].clone();
    delegating.sync().await.unwrap();
    delegating.shutdown().await;

    let model = delegating_model();
    let next = family_harness(store.clone(), model.clone());
    assert!(
        next.snapshot(root).await.is_err(),
        "nothing is loaded before the delivery"
    );
    let request = TurnRequest::deliver(member.child_loop_id, delivery("for-the-member"));
    next.advance(member.child_chat_id, request.clone())
        .await
        .unwrap();
    assert!(matches!(
        settled(&next, member.child_chat_id, request.input_id).await,
        SettledOutcome::Completed { .. }
    ));
    assert!(
        next.snapshot(root).await.is_ok(),
        "the delivery loaded the member's family with it"
    );
    let requests = model.requests.lock().unwrap().clone();
    assert!(requests.iter().any(|request| is_member(request)
        && format!("{:?}", request.messages).contains(&heading("for-the-member"))));
    tokio::time::timeout(WAIT, async {
        while next.quiesce(root).await != Quiescence::Idle {}
    })
    .await
    .expect("the family settles");
    next.shutdown().await;
}

/// The Lines with the Loop `loop_id` linked to the artifact for the account's requests, waiting
/// for them, as linking it records the Loop.
fn linked(lines: Vec<Line>, loop_id: LoopId) -> Vec<Line> {
    let binding = EventSourceBinding {
        origin: ORIGIN.into(),
        account_id: ACCOUNT.into(),
        artifact_id: ARTIFACT.into(),
        binding_id: Uuid::new_v4(),
        client_instance_id: Uuid::new_v4(),
        epoch: 1,
        resume_required: false,
        last_request_at_ms: None,
    };
    let last = lines
        .iter()
        .rposition(|line| matches!(line, Line::System(entry) if entry.loop_record.as_ref().is_some_and(|record| record.loop_id == loop_id)))
        .expect("the Loop is recorded");
    let mut lines = lines;
    let Line::System(entry) = &mut lines[last] else {
        unreachable!();
    };
    let record = entry.loop_record.as_mut().unwrap();
    record.event_sources = vec![serde_json::to_value(&binding).unwrap()];
    record.state = serde_json::to_value(LoopState::WaitingTrigger).unwrap();
    lines
}

#[tokio::test]
async fn a_durable_load_keeps_a_linked_loop_receiving_for_its_host_to_deliver_to() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(Vec::new());
    let first = harness(store.clone(), model, WaitHosting::Durable);
    let (chat, target) = ready_chat(&first).await;
    first.sync().await.unwrap();
    first.shutdown().await;
    let lines = linked(store.lines(chat), target);
    let expected = Quiescence::Waiting(Waits {
        deliveries: vec![DeliveryBinding {
            chat,
            loop_id: target,
            origin: ORIGIN.into(),
            account_id: ACCOUNT.into(),
            artifact_id: ARTIFACT.into(),
        }],
        ..Waits::default()
    });

    let durable = Arc::new(InMemoryTranscriptStore::new());
    durable.seed(chat, lines.clone(), StorePosition(1));
    let model = Script::new(Vec::new());
    let next = harness(durable, model.clone(), WaitHosting::Durable);
    next.wake(chat, None).await.unwrap();
    let goal = next.snapshot(chat).await.unwrap().loops[0].clone();
    assert_eq!(goal.state, LoopState::WaitingTrigger);
    assert!(!goal.event_sources[0].resume_required);
    assert_eq!(next.quiesce(chat).await, expected);
    assert!(matches!(
        outcome(
            &next,
            chat,
            TurnRequest::deliver(target, delivery("linked-request"))
        )
        .await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(
        next.quiesce(chat).await,
        expected,
        "the Loop keeps receiving"
    );
    next.shutdown().await;

    // A local Worker's load waits for an explicit Continue, as before.
    let local = Arc::new(InMemoryTranscriptStore::new());
    local.seed(chat, lines, StorePosition(1));
    let worker = harness(local, Script::new(Vec::new()), WaitHosting::Process);
    worker.wake(chat, None).await.unwrap();
    let goal = worker.snapshot(chat).await.unwrap().loops[0].clone();
    assert_eq!(goal.state, LoopState::Interrupted);
    assert!(goal.event_sources[0].resume_required);
    assert_eq!(worker.quiesce(chat).await, Quiescence::Idle);
    worker.shutdown().await;
}
