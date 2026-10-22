//! @tickloop requests a durable host delivers with `TurnOp::Deliver`: the delivery names the
//! input on every instance, so a repeated delivery runs nothing again, before or after the
//! conversation is compacted. A delivery queues behind the work its Chat restored, goes on with a
//! Loop whose turn was interrupted, failed or completed, loads the family of the member it
//! targets, and is refused while `/stop` pauses the Loop until the Chat is continued. A durable
//! load keeps a Loop's artifact links receiving, for the host to deliver to.

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
use tickloop_agent_transcript::{InputOp, InputState, Line, SettledOutcome};
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

async fn bound_chat(store: Arc<InMemoryTranscriptStore>, model: Arc<Script>) -> (Agent, ChatId) {
    bound_chat_at(store, model, ORIGIN).await
}

/// [`bound_chat`] with its Loop linked to an artifact at `origin`.
async fn bound_chat_at(
    store: Arc<InMemoryTranscriptStore>,
    model: Arc<Script>,
    origin: &str,
) -> (Agent, ChatId) {
    let setup = harness(store.clone(), Script::new(Vec::new()), WaitHosting::Durable);
    let (chat, target) = ready_chat(&setup).await;
    setup.sync().await.unwrap();
    setup.shutdown().await;
    let mut lines = linked_at(store.lines(chat), target, origin);
    for line in &mut lines {
        if let Line::System(entry) = line
            && let Some(record) = &mut entry.loop_record
            && record.loop_id == target
            && !record.event_sources.is_empty()
        {
            record.state = serde_json::to_value(LoopState::Idle).unwrap();
        }
    }
    store.seed(chat, lines, StorePosition(1));
    let agent = harness(store, model, WaitHosting::Durable);
    agent.resume_chat(chat).await.unwrap();
    (agent, chat)
}

async fn loop_state(agent: &Agent, chat: ChatId) -> LoopState {
    agent.snapshot(chat).await.unwrap().chat_loop.state
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
async fn a_delivery_builds_on_artifact_content_the_chat_already_holds() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(Vec::new());
    let (agent, chat) = bound_chat(store, model.clone()).await;
    let delivered = agent
        .advance(chat, TurnRequest::deliver(delivery("request-1")))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, delivered.outcome())
        .await
        .unwrap()
        .unwrap();
    let prompt = &model.requests()[0];
    assert!(!prompt.contains("Read the artifact"), "{prompt}");
    assert!(prompt.contains("Artifact content this Chat already read or wrote stays current"));
    assert!(prompt.contains("on content_conflict read again and merge"));
}

#[tokio::test]
async fn a_delivery_repeated_on_another_instance_or_after_compaction_runs_once() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(Vec::new());
    let (first, chat) = bound_chat(store.clone(), model.clone()).await;
    let request = TurnRequest::deliver(delivery("request-1"));
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
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains(&heading("request-1")));
    assert!(
        !requests[0].contains("fence-secret"),
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
    let reclaimed = TurnRequest::deliver(claimed(
        "request-1",
        delivery_id,
        "another-fence",
        "Tighten the title. Done: Tickloop (Agent) replied.",
    ));
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
async fn interrupted_failed_and_completed_turns_take_a_delivery_and_a_paused_loop_refuses_it() {
    // Interrupted by the user.
    let model = Script::new(vec![Step::Hang]);
    let (agent, chat) = bound_chat(Arc::new(InMemoryTranscriptStore::new()), model.clone()).await;
    let long = agent
        .advance(chat, prompt("a long task", ParentSelection::CurrentLeaf))
        .await
        .unwrap();
    model.until_holding().await;
    agent.interrupt(chat).await.unwrap();
    settled(&agent, chat, long.input_id()).await;
    assert_eq!(loop_state(&agent, chat).await, LoopState::Interrupted);
    let request = TurnRequest::deliver(delivery("after-interrupt"));
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
    let (agent, chat) = bound_chat(Arc::new(InMemoryTranscriptStore::new()), model.clone()).await;
    let broken = prompt("a broken task", ParentSelection::CurrentLeaf);
    agent.advance(chat, broken.clone()).await.unwrap();
    assert!(matches!(
        settled(&agent, chat, broken.input_id).await,
        SettledOutcome::Failed { .. }
    ));
    assert_eq!(loop_state(&agent, chat).await, LoopState::Failed);
    let request = TurnRequest::deliver(delivery("after-failure"));
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

    // Completed, then paused by `/stop` until a new user input continues the Chat.
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(Vec::new());
    let (agent, chat) = bound_chat(store.clone(), model.clone()).await;
    assert_eq!(loop_state(&agent, chat).await, LoopState::Idle);
    assert!(matches!(
        outcome(
            &agent,
            chat,
            TurnRequest::deliver(delivery("after-completion"))
        )
        .await,
        TurnOutcome::Completed { .. }
    ));
    agent.stop(chat).await.unwrap();
    assert_eq!(agent.quiesce(chat).await, Quiescence::Idle);
    let requests = model.requests().len();
    let late = TurnRequest::deliver(delivery("after-stop"));
    assert!(matches!(
        agent.advance(chat, late.clone()).await,
        Err(AgentError::Invalid(_))
    ));
    assert_eq!(
        model.requests().len(),
        requests,
        "a paused Loop runs nothing"
    );
    agent.sync().await.unwrap();
    assert_eq!(input_state(&store.lines(chat), late.input_id), None);
    let resume = prompt("go on", ParentSelection::CurrentLeaf);
    agent.advance(chat, resume.clone()).await.unwrap();
    settled(&agent, chat, resume.input_id).await;
    assert!(matches!(
        outcome(&agent, chat, late).await,
        TurnOutcome::Completed { .. }
    ));
    assert!(
        model
            .requests()
            .last()
            .unwrap()
            .contains(&heading("after-stop"))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn deliveries_queue_behind_the_work_a_load_restores() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang]);
    let (first, chat) = bound_chat(store.clone(), model.clone()).await;
    let lost = prompt("first prompt", ParentSelection::CurrentLeaf);
    first.advance(chat, lost.clone()).await.unwrap();
    model.until_holding().await;
    let queued_delivery = TurnRequest::deliver(delivery("request-1"));
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
    let later = TurnRequest::deliver(delivery("request-2"));
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
    let requests = model.conversation_requests();
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
            waits: WaitHosting::Durable,
            ..HarnessOptions::local(config())
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
    store.seed(
        member.child_chat_id,
        linked(store.lines(member.child_chat_id), member.child_loop_id),
        StorePosition(1),
    );

    let model = delegating_model();
    let next = family_harness(store.clone(), model.clone());
    assert!(
        next.snapshot(root).await.is_err(),
        "nothing is loaded before the delivery"
    );
    let request = TurnRequest::deliver(delivery("for-the-member"));
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
    let Quiescence::Waiting(waits) = next.quiesce(root).await else {
        panic!("the family waits for feedback");
    };
    assert_eq!(waits.deliveries.len(), 1);
    next.shutdown().await;
}

/// The Lines with the Loop `loop_id` linked to the artifact for the account's requests, waiting
/// for them, as linking it records the Loop.
fn linked(lines: Vec<Line>, loop_id: LoopId) -> Vec<Line> {
    linked_at(lines, loop_id, ORIGIN)
}

fn linked_at(lines: Vec<Line>, loop_id: LoopId, origin: &str) -> Vec<Line> {
    let binding = EventSourceBinding {
        origin: origin.into(),
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
    record.state = serde_json::to_value(LoopState::Idle).unwrap();
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
    next.resume_chat(chat).await.unwrap();
    let chat_loop = next.snapshot(chat).await.unwrap().chat_loop.clone();
    assert_eq!(chat_loop.state, LoopState::Idle);
    assert!(!chat_loop.event_sources[0].resume_required);
    assert_eq!(next.quiesce(chat).await, expected);
    assert!(matches!(
        outcome(
            &next,
            chat,
            TurnRequest::deliver(delivery("linked-request"))
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

    // A local Worker's load pauses receiving until the Chat is continued.
    let local = Arc::new(InMemoryTranscriptStore::new());
    local.seed(chat, lines, StorePosition(1));
    let worker = harness(local, Script::new(Vec::new()), WaitHosting::Process);
    worker.resume_chat(chat).await.unwrap();
    let chat_loop = worker.snapshot(chat).await.unwrap().chat_loop.clone();
    assert_eq!(chat_loop.state, LoopState::Idle);
    assert!(chat_loop.event_sources[0].resume_required);
    assert_eq!(worker.quiesce(chat).await, Quiescence::Idle);
    worker.shutdown().await;
}

#[test]
fn delivery_identity_and_input_id_are_independent_of_the_service_id_representation() {
    let uuid = Uuid::parse_str("e1b85a39-a57b-4c28-a8a5-ae9233aead3e").unwrap();
    let make = |id: String| {
        ExternalDelivery::from_claim(
            ORIGIN,
            ACCOUNT,
            ARTIFACT,
            &ClaimedFeedback {
                delivery_id: id,
                feedback_id: "feedback".into(),
                claim_id: "opaque-claim".into(),
                fencing_token: "opaque-fence".into(),
                lease_expires_at: "2099-01-01T00:00:00Z".into(),
                input: json!({"text":"hello"}),
            },
        )
    };
    let canonical = make(uuid.to_string()).unwrap();
    let compact = make(tickloop_agent_artifacts::short_artifact_id(
        &uuid.to_string(),
    ))
    .unwrap();
    assert_eq!(canonical.delivery_id(), compact.delivery_id());
    assert_eq!(canonical.input_id(), compact.input_id());
    assert!(make("zzzzzzzzzzzzzzzzzzzzzz".into()).is_none());
}

/// A host's delegated token for the account; nothing has asked for it yet.
struct HostToken;

#[async_trait::async_trait]
impl tickloop_agent_artifacts::ArtifactCredentials for HostToken {
    async fn bearer(&self) -> Result<Option<String>, tickloop_agent_artifacts::ArtifactError> {
        Ok(Some("delegated-token".into()))
    }
}

#[tokio::test]
async fn a_hosted_account_runs_a_delivery_before_anything_warmed_its_credentials() {
    // Loopback, so the Artifact tools' directory fails fast instead of reaching a service.
    let origin = "http://127.0.0.1:9";
    let model = Script::new(Vec::new());
    let (agent, chat) = bound_chat_at(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        origin,
    )
    .await;
    let provider = tickloop_agent_runtime::mcp_tools::McpHostToolProvider::with_credentials(
        origin,
        ACCOUNT,
        Arc::new(HostToken),
        None,
    )
    .unwrap();
    agent.install_host_tools(Arc::new(provider)).unwrap();
    let delivery = ExternalDelivery::from_claim(
        origin,
        ACCOUNT,
        ARTIFACT,
        &ClaimedFeedback {
            delivery_id: Uuid::new_v4().to_string(),
            feedback_id: "request-1".into(),
            claim_id: Uuid::new_v4().to_string(),
            fencing_token: "fence-secret".into(),
            lease_expires_at: "2026-10-20T00:00:00Z".into(),
            input: json!({"messages": [{"text": "Tighten the title"}]}),
        },
    )
    .unwrap();
    let outcome = outcome(&agent, chat, TurnRequest::deliver(delivery)).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert!(
        model
            .requests()
            .last()
            .is_some_and(|request| request.contains(&heading("request-1")))
    );
    agent.shutdown().await;
}
