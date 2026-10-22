//! The contract every Trigger source keeps with the wake-up base, run once per source. A source
//! only says how it hands a delivery over, how it ends, and what a crash or a second host is for
//! it; each case below is the same for every source. Goal continuations add their instance when
//! they join the base.
//!
//! The Tick instance runs where Tick streams run, in a process that keeps its own waits; the
//! artifact feedback instance runs where @tickloop requests are pushed, on a durable host, and the
//! scheduled task instance where a stateless host wakes the Chat at the time the Harness
//! reported. Tick Updates have no durable host to push them, so the durable push case is
//! feedback's alone.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tick_protocol::InstanceRef;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Content, EventKind, ExecutionMode, LoopState, ModelIdentity,
    PauseReason, PromptPolicy, Protocol, Receiving, Role, TickSubscriptionChange, Usage,
};
use tickloop_agent_runtime::transcript::{StorePosition, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, HarnessOptions, HarnessSeams, ParentSelection, ToolPolicy, TurnRequest, WaitHosting,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::shell_ticks::ShellTick;
use tickloop_agent_test_support::transcripts::{FaultyTranscriptStore, InMemoryTranscriptStore};
use tickloop_agent_test_support::{config, next_event};
use tickloop_agent_transcript::{DiscardReason, InputState, Line, SettledOutcome};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::durable_deliveries::{claimed, linked};
use super::durable_queue::{prompt, settled};

const WAIT: Duration = Duration::from_secs(10);
/// Long enough for a delivery that was going to start a turn to have started it.
const SETTLE: Duration = Duration::from_millis(500);
/// A prompt containing it holds its turn until the test releases it.
const HOLD: &str = "HOLD this turn";
const STOPPED: Receiving = Receiving::Paused {
    reason: PauseReason::Stopped,
};

/// Answers every turn at once, except one whose prompt holds it, which waits to be released.
#[derive(Default)]
struct ContractModel {
    requests: Mutex<Vec<ModelRequest>>,
    entered: Notify,
    release: Notify,
    released: Mutex<VecDeque<()>>,
}

impl ContractModel {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The text of the newest user message of each conversation request, in order.
    fn prompts(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| !request.tools.is_empty())
            .filter_map(|request| {
                request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::User)
                    .map(|message| {
                        message
                            .content
                            .iter()
                            .filter_map(|content| match content {
                                Content::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
            })
            .collect()
    }

    /// How many turns took a prompt mentioning `marker`.
    fn seen(&self, marker: &str) -> usize {
        self.prompts()
            .iter()
            .filter(|prompt| prompt.contains(marker))
            .count()
    }

    /// The position of the first turn whose prompt mentions `marker`.
    fn first_turn_with(&self, marker: &str) -> Option<usize> {
        self.prompts()
            .iter()
            .position(|prompt| prompt.contains(marker))
    }

    async fn wait_seen(&self, marker: &str, turns: usize) {
        tokio::time::timeout(WAIT, async {
            while self.seen(marker) < turns {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{marker} reached {turns} turn(s): {:?}", self.prompts()));
    }

    fn release_held_turn(&self) {
        self.released.lock().unwrap().push_back(());
        self.release.notify_one();
    }
}

#[async_trait]
impl ModelClient for ContractModel {
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
        let holds = !request.tools.is_empty()
            && request
                .messages
                .last()
                .is_some_and(|message| format!("{:?}", message.content).contains(HOLD));
        self.requests.lock().unwrap().push(request);
        if holds {
            self.entered.notify_one();
            loop {
                if self.released.lock().unwrap().pop_front().is_some() {
                    break;
                }
                tokio::select! {
                    _ = self.release.notified() => {}
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                }
            }
        }
        let _ = events.send(ModelDelta::Text("ok".into()));
        Ok(ModelOutput {
            private_source: None,
            content: vec![Content::Text { text: "ok".into() }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

/// What a Trigger source is to the contract.
#[async_trait]
trait Source: Sized + Send + Sync {
    /// Only the latest delivery waits while the Loop cannot take it.
    const LATEST_ONLY: bool;

    /// An Agent and a Chat whose first turn completed, with this source feeding its Loop.
    async fn new(model: Arc<ContractModel>) -> Self;
    fn agent(&self) -> &Agent;
    fn chat(&self) -> ChatId;
    /// Hands the source's `n`th delivery, carrying `marker`, to the base; a refusal leaves it
    /// with the source, which hands it again when called again.
    async fn deliver(&self, n: u64, marker: &str);
    /// The source stops standing behind what it delivered: what no turn took is withdrawn.
    async fn end(&self);
    /// The host after a crash right after the source's last delivery was accepted: it loads the
    /// Chat again from its history.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self;
    /// Another host of the same Chat, as this source's hosting allows one.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self;
}

async fn complete_turn(agent: &Agent, chat: ChatId, text: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, text.into()).await.unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let event = next_event(&mut events).await;
            if event.run_id == receipt.run_id
                && matches!(
                    event.kind,
                    EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
                )
            {
                return;
            }
        }
    })
    .await
    .expect("the turn ends");
}

/// Starts a turn the model holds, and waits until the model has it.
async fn hold_a_turn<S: Source>(source: &S, model: &ContractModel) {
    let entered = model.entered.notified();
    tokio::pin!(entered);
    entered.as_mut().enable();
    source
        .agent()
        .submit(source.chat(), HOLD.into())
        .await
        .unwrap();
    tokio::time::timeout(WAIT, entered)
        .await
        .expect("the held turn reaches the model");
}

async fn wait_for_state<S: Source>(source: &S, expected: LoopState) {
    tokio::time::timeout(WAIT, async {
        while source
            .agent()
            .snapshot(source.chat())
            .await
            .unwrap()
            .chat_loop
            .state
            != expected
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the Loop reaches {expected:?}"));
}

async fn receiving<S: Source>(source: &S) -> Receiving {
    source
        .agent()
        .snapshot(source.chat())
        .await
        .unwrap()
        .chat_loop
        .receiving
}

async fn delivers_while_receiving<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source.deliver(1, "first-delivery").await;
    model.wait_seen("first-delivery", 1).await;
    source.agent().shutdown().await;
}

/// `/stop` pauses receiving: a delivery queued before it is not taken, one after it is refused
/// and stays with its source. Continuing takes what waited, once, and repeats nothing that
/// already ran.
async fn stop_pauses_and_continuing_takes_what_waited_once<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source.deliver(1, "ran-before-stop").await;
    model.wait_seen("ran-before-stop", 1).await;

    hold_a_turn(&source, &model).await;
    source.deliver(2, "waited-through-stop").await;
    source.agent().stop(source.chat()).await.unwrap();
    model.release_held_turn();
    assert_eq!(receiving(&source).await, STOPPED);
    source.deliver(3, "came-while-stopped").await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("waited-through-stop"), 0);
    assert_eq!(model.seen("came-while-stopped"), 0);

    complete_turn(source.agent(), source.chat(), "carry on").await;
    assert!(receiving(&source).await.is_receiving());
    source.deliver(3, "came-while-stopped").await;
    model.wait_seen("came-while-stopped", 1).await;
    if S::LATEST_ONLY {
        assert_eq!(
            model.seen("waited-through-stop"),
            0,
            "only the latest waits"
        );
    } else {
        model.wait_seen("waited-through-stop", 1).await;
    }
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("ran-before-stop"), 1, "{:?}", model.prompts());
    assert_eq!(model.seen("came-while-stopped"), 1, "{:?}", model.prompts());
    source.agent().shutdown().await;
}

/// Esc interrupts the turn and leaves receiving on.
async fn esc_leaves_receiving_on<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    hold_a_turn(&source, &model).await;
    source.agent().interrupt(source.chat()).await.unwrap();
    model.release_held_turn();
    wait_for_state(&source, LoopState::Interrupted).await;
    assert!(receiving(&source).await.is_receiving());
    source.deliver(1, "after-esc").await;
    model.wait_seen("after-esc", 1).await;
    source.agent().shutdown().await;
}

/// While a turn runs, deliveries wait as their source coalesces them and start a turn only once
/// it ends.
async fn a_busy_loop_waits_as_the_source_coalesces<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    hold_a_turn(&source, &model).await;
    source.deliver(1, "busy-first").await;
    source.deliver(2, "busy-second").await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("busy-first") + model.seen("busy-second"), 0);
    model.release_held_turn();
    model.wait_seen("busy-second", 1).await;
    if S::LATEST_ONLY {
        tokio::time::sleep(SETTLE).await;
        assert_eq!(model.seen("busy-first"), 0, "{:?}", model.prompts());
    } else {
        model.wait_seen("busy-first", 1).await;
    }
    source.agent().shutdown().await;
}

/// A user's input queued behind the same turn goes first.
async fn user_input_goes_before_trigger_input<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    hold_a_turn(&source, &model).await;
    source.deliver(1, "trigger-input").await;
    tokio::time::sleep(SETTLE).await;
    source
        .agent()
        .submit(source.chat(), "user-input".into())
        .await
        .unwrap();
    model.release_held_turn();
    model.wait_seen("trigger-input", 1).await;
    model.wait_seen("user-input", 1).await;
    assert!(
        model.first_turn_with("user-input") < model.first_turn_with("trigger-input"),
        "{:?}",
        model.prompts()
    );
    source.agent().shutdown().await;
}

/// A delivery handed again after a crash finds its ledger record and runs nothing again.
async fn a_repeated_delivery_after_a_crash_runs_once<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source.deliver(1, "delivered-once").await;
    model.wait_seen("delivered-once", 1).await;
    let reopened = source.crash_and_reopen(model.clone()).await;
    reopened.deliver(1, "delivered-once").await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("delivered-once"), 1, "{:?}", model.prompts());
    reopened.agent().shutdown().await;
}

/// Two hosts of one Chat given the same delivery run it once.
async fn two_hosts_run_one_delivery_once<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source.deliver(1, "before-the-second-host").await;
    model.wait_seen("before-the-second-host", 1).await;
    let other_model = ContractModel::new();
    let other = source.another_host(other_model.clone()).await;
    source.deliver(2, "given-to-both").await;
    other.deliver(2, "given-to-both").await;
    tokio::time::timeout(WAIT, async {
        while model.seen("given-to-both") + other_model.seen("given-to-both") == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("one host runs the delivery");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        model.seen("given-to-both") + other_model.seen("given-to-both"),
        1
    );
    other.agent().shutdown().await;
    source.agent().shutdown().await;
}

/// A source that ends takes back what no turn took.
async fn an_ended_source_withdraws_what_waited<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    hold_a_turn(&source, &model).await;
    source.deliver(1, "withdrawn-delivery").await;
    tokio::time::sleep(SETTLE).await;
    source.end().await;
    model.release_held_turn();
    wait_for_state(&source, LoopState::Idle).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("withdrawn-delivery"), 0, "{:?}", model.prompts());
    assert!(
        source
            .agent()
            .snapshot(source.chat())
            .await
            .unwrap()
            .pending_inputs
            .is_empty()
    );
    source.agent().shutdown().await;
}

/// Plan mode takes deliveries, and the turn they start stays in Plan mode.
async fn plan_mode_takes_deliveries_under_plan<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source
        .agent()
        .set_execution_mode(source.chat(), ExecutionMode::Plan)
        .await
        .unwrap();
    source.deliver(1, "while-planning").await;
    model.wait_seen("while-planning", 1).await;
    wait_for_state(&source, LoopState::Idle).await;
    assert_eq!(
        source
            .agent()
            .snapshot(source.chat())
            .await
            .unwrap()
            .policy
            .mode,
        ExecutionMode::Plan
    );
    source.agent().shutdown().await;
}

macro_rules! contract {
    ($instance:ident, $source:ty) => {
        mod $instance {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn delivers_while_receiving() {
                super::delivers_while_receiving::<$source>().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn stop_pauses_and_continuing_takes_what_waited_once() {
                super::stop_pauses_and_continuing_takes_what_waited_once::<$source>().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn esc_leaves_receiving_on() {
                super::esc_leaves_receiving_on::<$source>().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_busy_loop_waits_as_the_source_coalesces() {
                super::a_busy_loop_waits_as_the_source_coalesces::<$source>().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn user_input_goes_before_trigger_input() {
                super::user_input_goes_before_trigger_input::<$source>().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_repeated_delivery_after_a_crash_runs_once() {
                super::a_repeated_delivery_after_a_crash_runs_once::<$source>().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn two_hosts_run_one_delivery_once() {
                super::two_hosts_run_one_delivery_once::<$source>().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn an_ended_source_withdraws_what_waited() {
                super::an_ended_source_withdraws_what_waited::<$source>().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn plan_mode_takes_deliveries_under_plan() {
                super::plan_mode_takes_deliveries_under_plan::<$source>().await;
            }
        }
    };
}

contract!(tick_subscription, TickSource);
contract!(artifact_feedback, FeedbackSource);
contract!(scheduled_task, ScheduledSource);

/// A Shell Tick subscription of a saved Chat, in a process that keeps its own waits.
struct TickSource {
    _directory: Arc<tempfile::TempDir>,
    root: std::path::PathBuf,
    agent: Agent,
    chat: ChatId,
    shell: ShellTick,
    task: Uuid,
}

impl TickSource {
    /// An instance on `root` resuming `chat`, continued by its user and watching the Shell Tick.
    async fn resumed(
        directory: Arc<tempfile::TempDir>,
        root: std::path::PathBuf,
        chat: ChatId,
        task: Uuid,
        model: Arc<ContractModel>,
    ) -> Self {
        let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
        let shell = ShellTick::new(task);
        shell.install(&agent).await;
        agent.resume_chat(chat).await.unwrap();
        complete_turn(&agent, chat, "resume here").await;
        shell.wait_until_live(&agent, chat).await;
        Self {
            _directory: directory,
            root,
            agent,
            chat,
            shell,
            task,
        }
    }
}

#[async_trait]
impl Source for TickSource {
    const LATEST_ONLY: bool = true;

    async fn new(model: Arc<ContractModel>) -> Self {
        let directory = Arc::new(tempfile::tempdir().unwrap());
        let root = directory.path().join("state");
        let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
        let task = Uuid::new_v4();
        let shell = ShellTick::new(task);
        shell.install(&agent).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        complete_turn(&agent, chat, "watch the Shell").await;
        shell.subscribe(&agent, chat).await;
        Self {
            _directory: directory,
            root,
            agent,
            chat,
            shell,
            task,
        }
    }

    fn agent(&self) -> &Agent {
        &self.agent
    }

    fn chat(&self) -> ChatId {
        self.chat
    }

    async fn deliver(&self, n: u64, marker: &str) {
        self.shell.update(n, marker);
    }

    async fn end(&self) {
        let subscription = self.agent.tick_subscriptions(self.chat).await.unwrap()[0].id;
        self.agent
            .update_tick_subscription(self.chat, subscription, TickSubscriptionChange::Cancel)
            .await
            .unwrap();
    }

    /// The watermark a delivered update leaves is lost with the crash, so only the ledger
    /// stands between the repeated update and a second run.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        self.agent.shutdown().await;
        for watermark in files_under(&self.root, "tick-watermarks") {
            std::fs::remove_file(watermark).unwrap();
        }
        Self::resumed(self._directory, self.root, self.chat, self.task, model).await
    }

    /// Another process holding the same Chat at the same time; only the lease holder delivers.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        Self::resumed(
            self._directory.clone(),
            self.root.clone(),
            self.chat,
            self.task,
            model,
        )
        .await
    }
}

fn files_under(directory: &std::path::Path, parent: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .parent()
                .is_some_and(|parent_path| parent_path.ends_with(parent))
            {
                found.push(path);
            }
        }
    }
    found
}

/// A Chat on a durable host whose Loop is linked to an artifact; the host pushes each
/// @tickloop request with `TurnOp::Deliver`.
struct FeedbackSource {
    store: Arc<InMemoryTranscriptStore>,
    faults: Arc<FaultyTranscriptStore>,
    agent: Agent,
    chat: ChatId,
}

const FEEDBACK_ORIGIN: &str = "https://tickloop.ai/artifact";
const FEEDBACK_ARTIFACT: &str = "doc";

fn durable_harness(store: Arc<dyn TranscriptStore>, model: Arc<ContractModel>) -> Agent {
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
            tools: ToolPolicy {
                questions: true,
                ..ToolPolicy::web_fetch_only()
            },
            waits: WaitHosting::Durable,
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

impl FeedbackSource {
    async fn loaded(
        store: Arc<InMemoryTranscriptStore>,
        chat: ChatId,
        model: Arc<ContractModel>,
    ) -> Self {
        let faults = Arc::new(FaultyTranscriptStore::new(store.clone()));
        let agent = durable_harness(faults.clone(), model);
        agent.resume_chat(chat).await.unwrap();
        Self {
            store,
            faults,
            agent,
            chat,
        }
    }
}

#[async_trait]
impl Source for FeedbackSource {
    const LATEST_ONLY: bool = false;

    async fn new(model: Arc<ContractModel>) -> Self {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let setup = durable_harness(store.clone(), model.clone());
        let chat = setup.create_chat_without_folder().await.unwrap();
        let ready = prompt("get ready", ParentSelection::CurrentLeaf);
        let loop_id = setup
            .advance(chat, ready.clone())
            .await
            .unwrap()
            .receipt()
            .loop_id;
        settled(&setup, chat, ready.input_id).await;
        setup.sync().await.unwrap();
        setup.shutdown().await;
        store.seed(chat, linked(store.lines(chat), loop_id), StorePosition(1));
        Self::loaded(store, chat, model).await
    }

    fn agent(&self) -> &Agent {
        &self.agent
    }

    fn chat(&self) -> ChatId {
        self.chat
    }

    async fn deliver(&self, n: u64, marker: &str) {
        let delivery = claimed(
            &format!("feedback-{n}"),
            Uuid::from_u128(u128::from(n)),
            "fence-secret",
            marker,
        );
        // A refusal leaves the request with the host, which pushes it again later.
        let _ = self
            .agent
            .advance(self.chat, TurnRequest::deliver(delivery))
            .await;
    }

    async fn end(&self) {
        let instance = InstanceRef::artifact(FEEDBACK_ORIGIN, FEEDBACK_ARTIFACT).unwrap();
        self.agent
            .remove_artifact_tick(self.chat, &instance.to_string())
            .await
            .unwrap();
    }

    /// The store commits the host's last write and loses its acknowledgement, and the host goes
    /// down; the next host loads what was committed.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        self.faults.lose_next_acknowledgement();
        let _ = self.agent.sync().await;
        self.agent.shutdown().await;
        Self::loaded(self.store, self.chat, model).await
    }

    /// The host the family moves to: this one hands the Chat over first.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        self.agent.sync().await.unwrap();
        self.agent.shutdown().await;
        Self::loaded(self.store.clone(), self.chat, model).await
    }
}

/// The durable host pushes a request while the Loop is paused: it is refused and stays with the
/// host, and pushed again after the Chat is continued it runs once.
#[tokio::test(flavor = "multi_thread")]
async fn a_durable_hosts_push_goes_through_the_same_entry() {
    let model = ContractModel::new();
    let source = FeedbackSource::new(model.clone()).await;
    source.agent.stop(source.chat).await.unwrap();
    let delivery = claimed(
        "feedback-9",
        Uuid::from_u128(9),
        "fence-secret",
        "pushed-while-paused",
    );
    assert!(
        source
            .agent
            .advance(source.chat, TurnRequest::deliver(delivery.clone()))
            .await
            .is_err(),
        "a paused Loop refuses the push"
    );
    complete_turn(&source.agent, source.chat, "continue").await;
    let turn = source
        .agent
        .advance(source.chat, TurnRequest::deliver(delivery.clone()))
        .await
        .unwrap();
    model.wait_seen("pushed-while-paused", 1).await;
    let again = source
        .agent
        .advance(source.chat, TurnRequest::deliver(delivery))
        .await
        .unwrap();
    assert_eq!(again.receipt().run_id, turn.receipt().run_id);
    source.agent.shutdown().await;
}

/// Unlinking withdraws the request no turn took, with the reason in its ledger record.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlinked_artifacts_waiting_request_is_withdrawn_with_its_reason() {
    let model = ContractModel::new();
    let source = FeedbackSource::new(model.clone()).await;
    hold_a_turn(&source, &model).await;
    source.deliver(4, "unlinked-request").await;
    source.end().await;
    model.release_held_turn();
    wait_for_state(&source, LoopState::Idle).await;
    source.agent.sync().await.unwrap();
    let input_id = claimed("feedback-4", Uuid::from_u128(4), "fence-secret", "").input_id();
    let state = source
        .store
        .lines(source.chat)
        .into_iter()
        .rev()
        .find_map(|line| match line {
            Line::Input(entry) if entry.input_id == input_id => Some(entry.state),
            _ => None,
        });
    assert_eq!(
        state,
        Some(InputState::Settled {
            outcome: SettledOutcome::Discarded {
                reason: DiscardReason::Unlinked
            }
        })
    );
    source.agent.shutdown().await;
}

/// Scheduled tasks of a Chat on a durable host, which wakes it at each task's next fire time.
/// Each delivery is the run of a task of its own, so deliveries do not merge.
struct ScheduledSource {
    store: Arc<InMemoryTranscriptStore>,
    faults: Arc<FaultyTranscriptStore>,
    agent: Agent,
    chat: ChatId,
    /// The fire time each delivery was handed at, to hand the same run again.
    fired: Mutex<std::collections::HashMap<u64, u64>>,
}

impl ScheduledSource {
    async fn loaded(
        store: Arc<InMemoryTranscriptStore>,
        chat: ChatId,
        model: Arc<ContractModel>,
        fired: std::collections::HashMap<u64, u64>,
    ) -> Self {
        let faults = Arc::new(FaultyTranscriptStore::new(store.clone()));
        let agent = durable_harness(faults.clone(), model);
        agent.resume_chat(chat).await.unwrap();
        Self {
            store,
            faults,
            agent,
            chat,
            fired: Mutex::new(fired),
        }
    }

    async fn task_with(&self, prompt: &str) -> Option<tickloop_agent_protocol::ScheduledTask> {
        self.agent
            .snapshot(self.chat)
            .await
            .ok()?
            .scheduled_tasks
            .into_iter()
            .find(|task| task.prompt == prompt)
    }
}

#[async_trait]
impl Source for ScheduledSource {
    const LATEST_ONLY: bool = false;

    async fn new(model: Arc<ContractModel>) -> Self {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let setup = durable_harness(store.clone(), model.clone());
        let chat = setup.create_chat_without_folder().await.unwrap();
        let ready = prompt("get ready", ParentSelection::CurrentLeaf);
        setup.advance(chat, ready.clone()).await.unwrap();
        settled(&setup, chat, ready.input_id).await;
        setup.sync().await.unwrap();
        setup.shutdown().await;
        Self::loaded(store, chat, model, Default::default()).await
    }

    fn agent(&self) -> &Agent {
        &self.agent
    }

    fn chat(&self) -> ChatId {
        self.chat
    }

    /// The `n`th delivery is the run of its own every-minute task: the host wakes the Chat at
    /// the task's fire time, and for a run it handed before, at that run's time again.
    async fn deliver(&self, n: u64, marker: &str) {
        let prompt = format!("{marker} (task {n})");
        let task = match self.task_with(&prompt).await {
            Some(task) => task,
            None => match self
                .agent
                .schedule_task(self.chat, "* * * * *", prompt.clone(), true)
                .await
            {
                Ok(task) => task,
                Err(_) => return,
            },
        };
        let repeated = self.fired.lock().unwrap().get(&n).copied();
        let Some(due_at) = repeated.or(task.next_fire_at_ms) else {
            return;
        };
        let _ = self.agent.wake_scheduled_tasks(self.chat, due_at).await;
        if repeated.is_none()
            && self
                .task_with(&prompt)
                .await
                .is_some_and(|task| task.last_scheduled_for_ms.is_some())
        {
            self.fired.lock().unwrap().insert(n, due_at);
        }
    }

    async fn end(&self) {
        let tasks = self
            .agent
            .snapshot(self.chat)
            .await
            .unwrap()
            .scheduled_tasks;
        for task in tasks {
            self.agent
                .delete_scheduled_task(self.chat, &task.id)
                .await
                .unwrap();
        }
    }

    /// The store commits the host's last write and loses its acknowledgement; the next host
    /// loads what was committed and is woken for the same run again.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        self.faults.lose_next_acknowledgement();
        let _ = self.agent.sync().await;
        self.agent.shutdown().await;
        let fired = self.fired.into_inner().unwrap();
        Self::loaded(self.store, self.chat, model, fired).await
    }

    /// The host the family moves to: this one hands the Chat over first.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        self.agent.sync().await.unwrap();
        self.agent.shutdown().await;
        Self::loaded(self.store.clone(), self.chat, model, Default::default()).await
    }
}
