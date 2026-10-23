//! The contract every Trigger source keeps with the wake-up base, run once per source. A source
//! says how its deliveries wait and what Plan mode does to them, how it hands a delivery over,
//! how it ends, and what a crash or a second host is for it; each case below is the same for
//! every source.
//!
//! Tick Subscriptions run where Tick streams run, in a process that keeps its own waits.
//! Artifact feedback runs there too, and on a durable host that pushes each request. Scheduled
//! tasks run where a stateless host wakes the Chat at the time the Harness reported, a task of
//! its own per delivery, and in such a process too, the runs of one task. Goal continuations run
//! for a Chat's real goal, handed over by the test instead of at each turn's end.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tick_protocol::InstanceRef;
use tickloop_agent_artifacts::ClaimedFeedback;
use tickloop_agent_protocol::{
    AgentHostOptions, ArtifactFeedbackHistory, ArtifactTick, ChatId, ExecutionMode, GoalAction,
    LoopState, PauseReason, PromptPolicy, Receipt, Receiving, TickSubscriptionChange,
    TriggerSource,
};
use tickloop_agent_runtime::transcript::TranscriptStore;
use tickloop_agent_runtime::{
    Agent, ExternalDelivery, HarnessOptions, HarnessSeams, ToolPolicy, TurnRequest, WaitHosting,
    mcp_tools::McpHostToolProvider,
};
use tickloop_agent_test_support::config;
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::shell_ticks::ShellTick;
use tickloop_agent_test_support::transcripts::{FaultyTranscriptStore, InMemoryTranscriptStore};
use tickloop_agent_transcript::{DiscardReason, InputState, Line, SettledOutcome};
use uuid::Uuid;

use super::mcp_tools::artifact_service::ArtifactService;
use super::wakeup_support::*;

const STOPPED: Receiving = Receiving::Paused {
    reason: PauseReason::Stopped,
};

/// How a source's deliveries wait while the Loop cannot take them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coalescing {
    /// Only the latest waits, at the source.
    LatestOnly,
    /// Each waits on its own, queued.
    EachItem,
    /// One per key waits, queued; a later one of the key merges into it.
    OnePerKey,
}

/// What Plan mode does to a source's deliveries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnderPlan {
    /// They are taken; the turn they start stays in Plan mode.
    Delivered,
    /// They wait at the source until Plan mode ends.
    Held,
}

/// What Esc and `/stop` do to a source's deliveries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserStop {
    /// Esc leaves them coming; `/stop` holds them until the Chat is continued.
    Receiving,
    /// Esc and `/stop` pause the source itself: what waited is withdrawn, and nothing more comes
    /// until the user resumes the source, which continuing the Chat does not do.
    PausesSource,
}

/// Whether a source's deliveries wait for the Chat's background children and Shells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundWork {
    /// They are taken whatever runs in the background.
    Ignored,
    /// They are refused while background work runs; the source hands them over again once it
    /// ended.
    Waits,
}

/// What a Trigger source is to the contract.
#[async_trait]
trait Source: Sized + Send + Sync {
    const COALESCING: Coalescing;
    const UNDER_PLAN: UnderPlan;
    const USER_STOP: UserStop;
    const BACKGROUND_WORK: BackgroundWork;

    /// An Agent and a Chat whose first turn completed, with this source feeding its Loop.
    async fn new(model: Arc<ContractModel>) -> Self;
    fn agent(&self) -> &Agent;
    fn chat(&self) -> ChatId;
    /// Hands the source's `n`th delivery, carrying `marker`, to the base; a refusal leaves it
    /// with the source, which hands it again when called again.
    async fn deliver(&self, n: u64, marker: &str);
    /// The source stops standing behind what it delivered: what no turn took is withdrawn.
    async fn end(&self);
    /// Whether delivery `n`, handed over again, was accepted as the ledger record of its first
    /// hand-over, rather than refused or run anew.
    async fn replayed(&self, n: u64) -> bool;
    /// The host of the Chat after a crash: it loads what the store held when the host went down,
    /// with nothing written by a graceful end.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self;
    /// Another host of the same Chat, as this source's hosting allows one.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self;
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

/// Waits until the deliveries `ran` started turns and the rest of `markers` did not.
async fn only_these_ran(model: &ContractModel, markers: &[&str], ran: &[&str]) {
    for marker in ran {
        model.wait_seen(marker, 1).await;
    }
    tokio::time::sleep(SETTLE).await;
    for marker in markers {
        let expected = usize::from(ran.contains(marker));
        assert_eq!(
            model.seen(marker),
            expected,
            "{marker}: {:?}",
            model.prompts()
        );
    }
}

async fn delivers_while_receiving<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source.deliver(1, "first-delivery").await;
    model.wait_seen("first-delivery", 1).await;
    source.agent().shutdown().await;
}

/// `/stop` pauses receiving: a delivery queued before it is not taken, one after it is refused
/// and stays with its source. Continuing takes what waited, as the source coalesces it, once,
/// and repeats nothing that already ran.
async fn stop_pauses_and_continuing_takes_what_waited_once<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source.deliver(1, "ran-before-stop").await;
    model.wait_seen("ran-before-stop", 1).await;

    hold_a_turn(source.agent(), source.chat(), &model).await;
    source.deliver(2, "waited-through-stop").await;
    source.agent().stop(source.chat()).await.unwrap();
    model.release_held_turn();
    assert_eq!(receiving(&source).await, STOPPED);
    source.deliver(3, "came-while-stopped").await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("waited-through-stop"), 0);
    assert_eq!(model.seen("came-while-stopped"), 0);

    // The user's input continues the Chat while its own turn runs, so what waited and what the
    // source hands again meet before either is taken.
    hold_a_turn(source.agent(), source.chat(), &model).await;
    assert!(receiving(&source).await.is_receiving());
    source.deliver(3, "came-while-stopped").await;
    model.release_held_turn();
    let markers = ["waited-through-stop", "came-while-stopped"];
    match (S::USER_STOP, S::COALESCING) {
        (UserStop::PausesSource, _) => only_these_ran(&model, &markers, &[]).await,
        (UserStop::Receiving, Coalescing::LatestOnly) => {
            only_these_ran(&model, &markers, &["came-while-stopped"]).await
        }
        (UserStop::Receiving, Coalescing::EachItem) => {
            only_these_ran(&model, &markers, &markers).await
        }
        (UserStop::Receiving, Coalescing::OnePerKey) => {
            only_these_ran(&model, &markers, &["waited-through-stop"]).await
        }
    }
    assert_eq!(model.seen("ran-before-stop"), 1, "{:?}", model.prompts());
    source.agent().shutdown().await;
}

/// Esc interrupts the turn and leaves receiving on; a source Esc pauses delivers nothing more.
async fn esc_leaves_receiving_on<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    hold_a_turn(source.agent(), source.chat(), &model).await;
    source.agent().interrupt(source.chat()).await.unwrap();
    model.release_held_turn();
    wait_for_state(source.agent(), source.chat(), LoopState::Interrupted).await;
    assert!(receiving(&source).await.is_receiving());
    source.deliver(1, "after-esc").await;
    match S::USER_STOP {
        UserStop::Receiving => model.wait_seen("after-esc", 1).await,
        UserStop::PausesSource => only_these_ran(&model, &["after-esc"], &[]).await,
    }
    source.agent().shutdown().await;
}

/// While a turn runs, deliveries wait as their source coalesces them and start a turn only once
/// it ends.
async fn a_busy_loop_waits_as_the_source_coalesces<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    hold_a_turn(source.agent(), source.chat(), &model).await;
    source.deliver(1, "busy-first").await;
    source.deliver(2, "busy-second").await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("busy-first") + model.seen("busy-second"), 0);
    model.release_held_turn();
    let markers = ["busy-first", "busy-second"];
    match S::COALESCING {
        Coalescing::LatestOnly => only_these_ran(&model, &markers, &["busy-second"]).await,
        Coalescing::EachItem => only_these_ran(&model, &markers, &markers).await,
        Coalescing::OnePerKey => only_these_ran(&model, &markers, &["busy-first"]).await,
    }
    source.agent().shutdown().await;
}

/// A user's input queued behind the same turn goes first.
async fn user_input_goes_before_trigger_input<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    hold_a_turn(source.agent(), source.chat(), &model).await;
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

/// An Agent message, here a background child's completion, queued behind the same turn goes
/// before Trigger input.
async fn an_agent_message_goes_before_trigger_input<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    // Delegation goes through without review.
    source
        .agent()
        .set_execution_mode(source.chat(), ExecutionMode::FullAccess)
        .await
        .unwrap();
    source
        .agent()
        .submit(source.chat(), SPAWN_HELD_CHILD.into())
        .await
        .unwrap();
    model.wait_held(HELD_CHILD).await;
    wait_for_state(source.agent(), source.chat(), LoopState::Idle).await;
    hold_a_turn(source.agent(), source.chat(), &model).await;
    source.deliver(1, "trigger-input").await;
    model.release(HELD_CHILD);
    tokio::time::timeout(WAIT, async {
        while !source
            .agent()
            .snapshot(source.chat())
            .await
            .unwrap()
            .pending_inputs
            .iter()
            .any(|pending| pending.agent_message.is_some())
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the child's completion queues behind the running turn");
    let turns_before = model.prompts().len();
    model.release_held_turn();
    match S::BACKGROUND_WORK {
        BackgroundWork::Ignored => {}
        BackgroundWork::Waits => {
            // Handed over while the child ran, it was refused; the source hands it again.
            wait_for_state(source.agent(), source.chat(), LoopState::Idle).await;
            source.deliver(1, "trigger-input").await;
        }
    }
    model.wait_seen("trigger-input", 1).await;
    let after = &model.prompts()[turns_before..];
    let completion = after
        .iter()
        .position(|prompt| !prompt.contains("trigger-input") && !prompt.contains("child task"));
    let trigger = after
        .iter()
        .position(|prompt| prompt.contains("trigger-input"));
    assert!(
        completion.is_some() && completion < trigger,
        "{:?}",
        model.prompts()
    );
    source.agent().shutdown().await;
}

/// A delivery handed again after a crash finds its ledger record: it is accepted as that
/// record, and runs once.
async fn a_repeated_delivery_after_a_crash_runs_once<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source.deliver(1, "delivered-once").await;
    model.wait_seen("delivered-once", 1).await;
    wait_for_state(source.agent(), source.chat(), LoopState::Idle).await;
    let reopened = source.crash_and_reopen(model.clone()).await;
    reopened.deliver(1, "delivered-once").await;
    assert!(
        reopened.replayed(1).await,
        "the repeated delivery is answered by its record"
    );
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
    hold_a_turn(source.agent(), source.chat(), &model).await;
    source.deliver(1, "withdrawn-delivery").await;
    tokio::time::sleep(SETTLE).await;
    source.end().await;
    model.release_held_turn();
    wait_for_state(source.agent(), source.chat(), LoopState::Idle).await;
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

/// Plan mode takes the deliveries of a source that keeps going under it, and the turn they
/// start stays in Plan mode; a source that only continues the work waits.
async fn plan_mode_takes_or_holds_as_the_source_declares<S: Source>() {
    let model = ContractModel::new();
    let source = S::new(model.clone()).await;
    source
        .agent()
        .set_execution_mode(source.chat(), ExecutionMode::Plan)
        .await
        .unwrap();
    source.deliver(1, "while-planning").await;
    match S::UNDER_PLAN {
        UnderPlan::Delivered => model.wait_seen("while-planning", 1).await,
        UnderPlan::Held => {
            tokio::time::sleep(SETTLE).await;
            assert_eq!(model.seen("while-planning"), 0, "{:?}", model.prompts());
        }
    }
    wait_for_state(source.agent(), source.chat(), LoopState::Idle).await;
    let snapshot = source.agent().snapshot(source.chat()).await.unwrap();
    assert_eq!(snapshot.policy.mode, ExecutionMode::Plan);
    assert!(snapshot.pending_inputs.is_empty());
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
            async fn an_agent_message_goes_before_trigger_input() {
                super::an_agent_message_goes_before_trigger_input::<$source>().await;
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
            async fn plan_mode_takes_or_holds_as_the_source_declares() {
                super::plan_mode_takes_or_holds_as_the_source_declares::<$source>().await;
            }
        }
    };
}

contract!(tick_subscription, TickSource);
contract!(artifact_feedback_local, LocalFeedbackSource);
contract!(artifact_feedback_durable, DurableFeedbackSource);
contract!(scheduled_task, ScheduledSource);
contract!(scheduled_task_one_key, OneTaskSource);
contract!(goal_continuation, GoalSource);

/// The receipts each hand-over of a delivery got, for sources that hand over synchronously.
#[derive(Default)]
struct HandOvers(Mutex<HashMap<u64, Vec<Result<Receipt, String>>>>);

impl HandOvers {
    fn record(&self, n: u64, outcome: Result<Receipt, String>) {
        self.0.lock().unwrap().entry(n).or_default().push(outcome);
    }

    /// The last hand-over of `n` was accepted under the receipt of the first accepted one.
    fn replayed(&self, n: u64) -> bool {
        let hand_overs = self.0.lock().unwrap();
        let Some(outcomes) = hand_overs.get(&n) else {
            return false;
        };
        let first = outcomes.iter().find_map(|outcome| outcome.as_ref().ok());
        match (first, outcomes.last()) {
            (Some(first), Some(Ok(last))) => outcomes.len() > 1 && first.run_id == last.run_id,
            (Some(_), Some(Err(_)) | None) | (None, _) => false,
        }
    }

    fn refused(&self, n: u64) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(&n)
            .and_then(|outcomes| outcomes.last())
            .is_some_and(Result::is_err)
    }
}

/// A saved Chat in a process that keeps its own waits, its state root inside `directory`.
struct ProcessChat {
    directory: Arc<tempfile::TempDir>,
    root: std::path::PathBuf,
    agent: Agent,
    chat: ChatId,
}

impl ProcessChat {
    async fn new(model: Arc<ContractModel>) -> Self {
        let directory = Arc::new(tempfile::tempdir().unwrap());
        let root = directory.path().join("state");
        let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        complete_turn(&agent, chat, "get ready").await;
        Self {
            directory,
            root,
            agent,
            chat,
        }
    }

    /// The same Chat in another process on `root`, set up by `prepare` before it loads the
    /// Chat, and continued by its user.
    async fn resumed(
        &self,
        root: std::path::PathBuf,
        model: Arc<ContractModel>,
        prepare: impl AsyncFnOnce(&Agent),
    ) -> Self {
        let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
        prepare(&agent).await;
        agent.resume_chat(self.chat).await.unwrap();
        complete_turn(&agent, self.chat, "resume here").await;
        Self {
            directory: self.directory.clone(),
            root,
            agent,
            chat: self.chat,
        }
    }

    /// The Chat's state root as a process that crashes now leaves it; the crashed process is
    /// gone.
    async fn crashed(&self) -> std::path::PathBuf {
        self.agent.flush_transcripts().await;
        let image = crash_image(&self.root, self.directory.path());
        self.agent.shutdown().await;
        image
    }
}

/// A Shell Tick subscription of a saved Chat, in a process that keeps its own waits.
struct TickSource {
    process: ProcessChat,
    shell: ShellTick,
    task: Uuid,
    /// What the ledger recorded before the crash this process reopened from.
    before_crash: Vec<tickloop_agent_transcript::display::DisplayTriggerDelivery>,
}

impl TickSource {
    async fn resumed(&self, root: std::path::PathBuf, model: Arc<ContractModel>) -> Self {
        let shell = ShellTick::new(self.task);
        let process = self
            .process
            .resumed(root, model, async |agent: &Agent| {
                agent.register_trigger_source_state_for_tests(Default::default());
                shell.install(agent).await
            })
            .await;
        shell.wait_until_live(&process.agent, process.chat).await;
        Self {
            process,
            shell,
            task: self.task,
            before_crash: Vec::new(),
        }
    }
}

#[async_trait]
impl Source for TickSource {
    const COALESCING: Coalescing = Coalescing::LatestOnly;
    const UNDER_PLAN: UnderPlan = UnderPlan::Delivered;
    const USER_STOP: UserStop = UserStop::Receiving;
    const BACKGROUND_WORK: BackgroundWork = BackgroundWork::Ignored;

    async fn new(model: Arc<ContractModel>) -> Self {
        let directory = Arc::new(tempfile::tempdir().unwrap());
        let root = directory.path().join("state");
        let task = Uuid::new_v4();
        let (agent, shell, chat) = subscribed_process(&root, directory.path(), task, model).await;
        // As in the product, where the Goal and the scheduled tasks register theirs.
        agent.register_trigger_source_state_for_tests(Default::default());
        Self {
            process: ProcessChat {
                directory,
                root,
                agent,
                chat,
            },
            shell,
            task,
            before_crash: Vec::new(),
        }
    }

    fn agent(&self) -> &Agent {
        &self.process.agent
    }

    fn chat(&self) -> ChatId {
        self.process.chat
    }

    async fn deliver(&self, n: u64, marker: &str) {
        self.shell.update(n, marker);
    }

    async fn end(&self) {
        let subscription = self.agent().tick_subscriptions(self.chat()).await.unwrap()[0].id;
        self.agent()
            .update_tick_subscription(self.chat(), subscription, TickSubscriptionChange::Cancel)
            .await
            .unwrap();
    }

    /// The update the stream gave again reached this process's subscription, which let it go as
    /// delivered, and left the ledger as it was: one delivery carries it, under the receipt it
    /// got before the crash. A process that took the Chat over before the update came, as the
    /// sources' part of a Continue does, knows it delivered from its watermark and lets it go
    /// as it arrives.
    async fn replayed(&self, n: u64) -> bool {
        let reached = tokio::time::timeout(WAIT, async {
            loop {
                let status = &self.agent().tick_subscriptions(self.chat()).await.unwrap()[0];
                if status.updated_at_ms.is_some()
                    || self
                        .agent()
                        .tick_delivered_sequence_for_tests(status.id)
                        .is_some_and(|sequence| sequence >= n)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok();
        tokio::time::sleep(SETTLE).await;
        let status = &self.agent().tick_subscriptions(self.chat()).await.unwrap()[0];
        let source_let_it_go = status.accumulated == 0 && status.pending_since_ms.is_none();
        self.agent().flush_transcripts().await;
        let first = runs_delivering_tick_update(&self.before_crash, n);
        let now = runs_delivering_tick_update(
            &stored_trigger_deliveries(&self.process.root, self.chat()),
            n,
        );
        reached && source_let_it_go && first.len() == 1 && now == first
    }

    /// The crash came after the delivery's acceptance was durable and before its watermark was
    /// written, so only the ledger stands between the repeated update and a second run.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        let image = self.process.crashed().await;
        for watermark in files_under(&image, "tick-watermarks") {
            std::fs::remove_file(watermark).unwrap();
        }
        let before_crash = stored_trigger_deliveries(&image, self.chat());
        Self {
            before_crash,
            ..self.resumed(image, model).await
        }
    }

    /// Another process holding the same Chat at the same time; only the lease holder delivers.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        self.resumed(self.process.root.clone(), model).await
    }
}

/// An artifact linked to a saved Chat in a process that keeps its own waits; each request is
/// taken in as the watcher takes a claimed one, through the local inbox.
struct LocalFeedbackSource {
    process: ProcessChat,
    service: Arc<ArtifactService>,
    origin: String,
    account: String,
    hand_overs: HandOvers,
}

fn fixture_artifact(service: &ArtifactService) -> ArtifactTick {
    ArtifactTick {
        artifact_id: "doc".into(),
        origin: service.origin.clone(),
        title: "Fixture".into(),
        format: "markdown".into(),
        url: tickloop_agent_artifacts::artifact_url(&service.origin, "doc"),
        access_role: "owner".into(),
        added_at_ms: 1,
    }
}

fn claim(origin: &str, account: &str, n: u64, marker: &str) -> ExternalDelivery {
    ExternalDelivery::from_claim(
        origin,
        account,
        "doc",
        &ClaimedFeedback {
            delivery_id: Uuid::from_u128(u128::from(n)).to_string(),
            feedback_id: format!("feedback-{n}"),
            claim_id: Uuid::new_v4().to_string(),
            fencing_token: "fence-secret".into(),
            lease_expires_at: "2099-01-01T00:00:00Z".into(),
            input: serde_json::json!({"messages": [{"text": marker}]}),
        },
    )
    .expect("the claim reaches the model")
}

async fn hand_over(
    agent: &Agent,
    chat: ChatId,
    delivery: ExternalDelivery,
) -> Result<Receipt, String> {
    agent
        .advance(chat, TurnRequest::deliver(delivery))
        .await
        .map(|turn| turn.receipt().clone())
        .map_err(|error| error.to_string())
}

impl LocalFeedbackSource {
    fn sign_in(agent: &Agent, service: &ArtifactService, directory: &std::path::Path) {
        agent
            .install_host_tools(Arc::new(McpHostToolProvider::new(
                service.client(directory),
                None,
            )))
            .unwrap();
    }

    async fn resumed(
        &self,
        root: std::path::PathBuf,
        model: Arc<ContractModel>,
        hand_overs: HandOvers,
    ) -> Self {
        let service = self.service.clone();
        let directory = self.process.directory.clone();
        let process = self
            .process
            .resumed(root, model, async |agent: &Agent| {
                Self::sign_in(agent, &service, directory.path())
            })
            .await;
        Self {
            process,
            service: self.service.clone(),
            origin: self.origin.clone(),
            account: self.account.clone(),
            hand_overs,
        }
    }
}

#[async_trait]
impl Source for LocalFeedbackSource {
    const COALESCING: Coalescing = Coalescing::EachItem;
    const UNDER_PLAN: UnderPlan = UnderPlan::Delivered;
    const USER_STOP: UserStop = UserStop::Receiving;
    const BACKGROUND_WORK: BackgroundWork = BackgroundWork::Ignored;

    async fn new(model: Arc<ContractModel>) -> Self {
        let service = Arc::new(ArtifactService::start().await);
        let process = ProcessChat::new(model).await;
        service.login(process.directory.path()).await;
        Self::sign_in(&process.agent, &service, process.directory.path());
        process
            .agent
            .link_artifact(
                process.chat,
                &fixture_artifact(&service),
                ArtifactFeedbackHistory::NewOnly,
            )
            .await
            .unwrap();
        let binding = process
            .agent
            .snapshot(process.chat)
            .await
            .unwrap()
            .chat_loop
            .event_sources[0]
            .clone();
        Self {
            process,
            service,
            origin: binding.origin,
            account: binding.account_id,
            hand_overs: HandOvers::default(),
        }
    }

    fn agent(&self) -> &Agent {
        &self.process.agent
    }

    fn chat(&self) -> ChatId {
        self.process.chat
    }

    async fn deliver(&self, n: u64, marker: &str) {
        let delivery = claim(&self.origin, &self.account, n, marker);
        let outcome = self
            .agent()
            .receive_feedback_for_tests(self.chat(), delivery)
            .await
            .map_err(|error| error.to_string());
        self.hand_overs.record(n, outcome);
    }

    async fn end(&self) {
        let instance = InstanceRef::artifact(&self.origin, "doc").unwrap();
        self.agent()
            .remove_artifact_tick(self.chat(), &instance.to_string())
            .await
            .unwrap();
    }

    async fn replayed(&self, n: u64) -> bool {
        self.hand_overs.replayed(n)
    }

    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        let image = self.process.crashed().await;
        let hand_overs = HandOvers(Mutex::new(self.hand_overs.0.lock().unwrap().clone()));
        self.resumed(image, model, hand_overs).await
    }

    /// Another process holding the same Chat at the same time; only the lease holder delivers.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        self.resumed(self.process.root.clone(), model, HandOvers::default())
            .await
    }
}

/// A Chat on a durable host whose Loop is linked to an artifact; the host pushes each
/// @tickloop request with `TurnOp::Deliver`.
struct DurableFeedbackSource {
    store: Arc<InMemoryTranscriptStore>,
    faults: Arc<FaultyTranscriptStore>,
    agent: Agent,
    chat: ChatId,
    hand_overs: HandOvers,
}

const DURABLE_ORIGIN: &str = "https://tickloop.ai/artifact";

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
                delegation: true,
                ..ToolPolicy::web_fetch_only()
            },
            waits: WaitHosting::Durable,
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

impl DurableFeedbackSource {
    async fn loaded(
        store: Arc<InMemoryTranscriptStore>,
        chat: ChatId,
        model: Arc<ContractModel>,
        hand_overs: HandOvers,
    ) -> Self {
        let faults = Arc::new(FaultyTranscriptStore::new(store.clone()));
        let agent = durable_harness(faults.clone(), model);
        agent.resume_chat(chat).await.unwrap();
        Self {
            store,
            faults,
            agent,
            chat,
            hand_overs,
        }
    }
}

#[async_trait]
impl Source for DurableFeedbackSource {
    const COALESCING: Coalescing = Coalescing::EachItem;
    const UNDER_PLAN: UnderPlan = UnderPlan::Delivered;
    const USER_STOP: UserStop = UserStop::Receiving;
    const BACKGROUND_WORK: BackgroundWork = BackgroundWork::Ignored;

    async fn new(model: Arc<ContractModel>) -> Self {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let setup = durable_harness(store.clone(), model.clone());
        let chat = setup.create_chat_without_folder().await.unwrap();
        complete_turn(&setup, chat, "get ready").await;
        let loop_id = setup.snapshot(chat).await.unwrap().chat_loop.id;
        setup.sync().await.unwrap();
        setup.shutdown().await;
        store.seed(
            chat,
            super::durable_deliveries::linked(store.lines(chat), loop_id),
            store.position(chat).unwrap(),
        );
        Self::loaded(store, chat, model, HandOvers::default()).await
    }

    fn agent(&self) -> &Agent {
        &self.agent
    }

    fn chat(&self) -> ChatId {
        self.chat
    }

    /// A refusal leaves the request with the host, which pushes it again later.
    async fn deliver(&self, n: u64, marker: &str) {
        let delivery = claim(DURABLE_ORIGIN, "owner", n, marker);
        self.hand_overs
            .record(n, hand_over(&self.agent, self.chat, delivery).await);
    }

    async fn end(&self) {
        let instance = InstanceRef::artifact(DURABLE_ORIGIN, "doc").unwrap();
        self.agent
            .remove_artifact_tick(self.chat, &instance.to_string())
            .await
            .unwrap();
    }

    async fn replayed(&self, n: u64) -> bool {
        self.hand_overs.replayed(n)
    }

    /// The store commits the host's last write and loses its acknowledgement, and the host
    /// goes down there: the next host loads what was committed, nothing more.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        self.faults.lose_next_acknowledgement();
        let _ = self.agent.sync().await;
        let image = Arc::new(InMemoryTranscriptStore::new());
        image.seed(
            self.chat,
            self.store.lines(self.chat),
            self.store.position(self.chat).unwrap(),
        );
        self.agent.shutdown().await;
        let hand_overs = HandOvers(Mutex::new(self.hand_overs.0.lock().unwrap().clone()));
        Self::loaded(image, self.chat, model, hand_overs).await
    }

    /// The host the family moves to: this one hands the Chat over first.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        self.agent.sync().await.unwrap();
        self.agent.shutdown().await;
        Self::loaded(self.store.clone(), self.chat, model, HandOvers::default()).await
    }
}

/// The continuations of a Chat's real goal: one waits at a time, Plan mode holds them back, and
/// Esc and `/stop` pause the goal. Each turn's end would hand one over by itself; here the test
/// hands them over instead, so each carries its marker.
struct GoalSource {
    process: ProcessChat,
    goal_id: Uuid,
    objective_revision: u64,
    hand_overs: HandOvers,
}

impl GoalSource {
    async fn with_goal(process: ProcessChat, hand_overs: HandOvers) -> Self {
        let goal = process
            .agent
            .snapshot(process.chat)
            .await
            .unwrap()
            .goal
            .expect("the Chat has its goal");
        Self {
            process,
            goal_id: goal.id,
            objective_revision: goal.objective_revision,
            hand_overs,
        }
    }
}

#[async_trait]
impl Source for GoalSource {
    const COALESCING: Coalescing = Coalescing::OnePerKey;
    const UNDER_PLAN: UnderPlan = UnderPlan::Held;
    const USER_STOP: UserStop = UserStop::PausesSource;
    const BACKGROUND_WORK: BackgroundWork = BackgroundWork::Waits;

    async fn new(model: Arc<ContractModel>) -> Self {
        let process = ProcessChat::new(model).await;
        process.agent.hold_goal_continuations_for_tests();
        process
            .agent
            .manage_goal(
                process.chat,
                GoalAction::Set {
                    objective: "keep the contract".into(),
                    token_budget: None,
                    replace: false,
                },
            )
            .await
            .unwrap();
        Self::with_goal(process, HandOvers::default()).await
    }

    fn agent(&self) -> &Agent {
        &self.process.agent
    }

    fn chat(&self) -> ChatId {
        self.process.chat
    }

    /// The continuation after the `n`th ended turn.
    async fn deliver(&self, n: u64, marker: &str) {
        let outcome = self
            .process
            .agent
            .deliver_trigger_for_tests(
                self.process.chat,
                TriggerSource::GoalContinuation {
                    goal_id: self.goal_id,
                    after_run: Uuid::from_u128(u128::from(n)),
                    objective_revision: self.objective_revision,
                },
                marker,
            )
            .await;
        self.hand_overs
            .record(n, outcome.map_err(|error| error.to_string()));
    }

    /// The user clears the goal.
    async fn end(&self) {
        self.process
            .agent
            .manage_goal(self.process.chat, GoalAction::Clear)
            .await
            .unwrap();
    }

    async fn replayed(&self, n: u64) -> bool {
        self.hand_overs.replayed(n)
    }

    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        let image = self.process.crashed().await;
        let hand_overs = HandOvers(Mutex::new(self.hand_overs.0.lock().unwrap().clone()));
        let process = self
            .process
            .resumed(image, model, async |agent: &Agent| {
                agent.hold_goal_continuations_for_tests()
            })
            .await;
        Self::with_goal(process, hand_overs).await
    }

    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        let process = self
            .process
            .resumed(self.process.root.clone(), model, async |agent: &Agent| {
                agent.hold_goal_continuations_for_tests()
            })
            .await;
        Self::with_goal(process, HandOvers::default()).await
    }
}

/// The durable host pushes a request while the Loop is paused: it is refused and stays with the
/// host, and pushed again after the Chat is continued it runs once.
#[tokio::test(flavor = "multi_thread")]
async fn a_durable_hosts_push_goes_through_the_same_entry() {
    let model = ContractModel::new();
    let source = DurableFeedbackSource::new(model.clone()).await;
    source.agent.stop(source.chat).await.unwrap();
    source.deliver(9, "pushed-while-paused").await;
    assert!(
        source.hand_overs.refused(9),
        "a paused Loop refuses the push"
    );
    complete_turn(&source.agent, source.chat, "continue").await;
    source.deliver(9, "pushed-while-paused").await;
    model.wait_seen("pushed-while-paused", 1).await;
    source.deliver(9, "pushed-while-paused").await;
    assert!(source.hand_overs.replayed(9));
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("pushed-while-paused"), 1);
    source.agent.shutdown().await;
}

/// Unlinking withdraws the request no turn took, with the reason in its ledger record.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlinked_artifacts_waiting_request_is_withdrawn_with_its_reason() {
    let model = ContractModel::new();
    let source = DurableFeedbackSource::new(model.clone()).await;
    hold_a_turn(&source.agent, source.chat, &model).await;
    source.deliver(4, "unlinked-request").await;
    source.end().await;
    model.release_held_turn();
    wait_for_state(&source.agent, source.chat, LoopState::Idle).await;
    source.agent.sync().await.unwrap();
    let input_id = claim(DURABLE_ORIGIN, "owner", 4, "").input_id();
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
/// Each delivery is the run of a task of its own, so they wait each on its own; one task's runs
/// merging into one waiting run is the wiring tests' (`scheduled_tasks::wiring`).
struct ScheduledSource {
    store: Arc<InMemoryTranscriptStore>,
    agent: Agent,
    chat: ChatId,
    /// The fire time each delivery was handed at, to hand the same run again.
    fired: Mutex<HashMap<u64, u64>>,
    hand_overs: HandOvers,
}

impl ScheduledSource {
    async fn loaded(
        store: Arc<InMemoryTranscriptStore>,
        chat: ChatId,
        model: Arc<ContractModel>,
        fired: HashMap<u64, u64>,
        hand_overs: HandOvers,
    ) -> Self {
        let agent = durable_harness(store.clone(), model);
        agent.resume_chat(chat).await.unwrap();
        Self {
            store,
            agent,
            chat,
            fired: Mutex::new(fired),
            hand_overs,
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
    const COALESCING: Coalescing = Coalescing::EachItem;
    const UNDER_PLAN: UnderPlan = UnderPlan::Delivered;
    const USER_STOP: UserStop = UserStop::Receiving;
    const BACKGROUND_WORK: BackgroundWork = BackgroundWork::Ignored;

    async fn new(model: Arc<ContractModel>) -> Self {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let setup = durable_harness(store.clone(), model.clone());
        let chat = setup.create_chat_without_folder().await.unwrap();
        complete_turn(&setup, chat, "get ready").await;
        setup.sync().await.unwrap();
        setup.shutdown().await;
        Self::loaded(store, chat, model, HashMap::new(), HandOvers::default()).await
    }

    fn agent(&self) -> &Agent {
        &self.agent
    }

    fn chat(&self) -> ChatId {
        self.chat
    }

    /// The `n`th delivery is the run of its own every-minute task: the host wakes the Chat at
    /// the task's fire time, and for a run it handed before, at that run's time again. A wake
    /// that hands nothing over leaves the run with the task, which comes due again.
    async fn deliver(&self, n: u64, marker: &str) {
        let prompt = format!("{marker} (task {n})");
        let task = match self.task_with(&prompt).await {
            Some(task) => task,
            None => match self
                .agent
                .schedule_task(
                    self.chat,
                    tickloop_agent_runtime::NewScheduledTask::new(
                        "* * * * *",
                        prompt.clone(),
                        true,
                    ),
                )
                .await
            {
                Ok(task) => task,
                Err(error) => {
                    self.hand_overs.record(n, Err(error.to_string()));
                    return;
                }
            },
        };
        let repeated = self.fired.lock().unwrap().get(&n).copied();
        let Some(due_at) = repeated.or(task.next_fire_at_ms) else {
            return;
        };
        let outcome = match self.agent.wake_scheduled_tasks(self.chat, due_at).await {
            Ok(receipts) => receipts
                .into_iter()
                .next()
                .ok_or_else(|| "nothing was due".to_owned()),
            Err(error) => Err(error.to_string()),
        };
        if outcome.is_ok() {
            self.fired.lock().unwrap().entry(n).or_insert(due_at);
        }
        self.hand_overs.record(n, outcome);
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
                .delete_scheduled_task(self.chat, task.id)
                .await
                .unwrap();
        }
    }

    async fn replayed(&self, n: u64) -> bool {
        self.hand_overs.replayed(n)
    }

    /// The host goes down after the run was handed over and before the task's next time was
    /// recorded: the next host loads the task as it was, finds the same run due again, and the
    /// ledger record of the run answers it.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        let _ = self.agent.sync().await;
        let mut lines = self.store.lines(self.chat);
        if let Some(advanced) = lines
            .iter()
            .rposition(|line| matches!(line, Line::ScheduledTasks(_)))
        {
            lines.remove(advanced);
        }
        let image = Arc::new(InMemoryTranscriptStore::new());
        image.seed(self.chat, lines, self.store.position(self.chat).unwrap());
        self.agent.shutdown().await;
        let fired = self.fired.into_inner().unwrap();
        let hand_overs = HandOvers(Mutex::new(self.hand_overs.0.lock().unwrap().clone()));
        Self::loaded(image, self.chat, model, fired, hand_overs).await
    }

    /// The host the family moves to: this one hands the Chat over first.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        self.agent.sync().await.unwrap();
        self.agent.shutdown().await;
        Self::loaded(
            self.store.clone(),
            self.chat,
            model,
            HashMap::new(),
            HandOvers::default(),
        )
        .await
    }
}

/// The clock the processes of [`OneTaskSource`] run their tasks by: its daily task's next run
/// is the next midnight.
fn one_task_clock() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_792_749_600, 0).unwrap()
}

/// The runs of one daily task of a saved Chat, in a process that keeps its own waits, which a
/// wake-up hands over at each run's time. The task runs the Chat's loop.md, read again at every
/// run, so each run carries what the file says when it comes due: one waits at a time, and a
/// later one merges into it.
struct OneTaskSource {
    process: ProcessChat,
    /// The fire time each delivery was handed at, to hand the same run again.
    fired: Mutex<HashMap<u64, u64>>,
    hand_overs: HandOvers,
}

impl OneTaskSource {
    fn loop_file(&self) -> std::path::PathBuf {
        self.process.directory.path().join(".tickloop/loop.md")
    }

    async fn task(&self) -> Option<tickloop_agent_protocol::ScheduledTask> {
        self.process
            .agent
            .snapshot(self.process.chat)
            .await
            .ok()?
            .scheduled_tasks
            .into_iter()
            .next()
    }

    async fn resumed(&self, root: std::path::PathBuf, model: Arc<ContractModel>) -> ProcessChat {
        self.process
            .resumed(root, model, async |agent: &Agent| {
                agent.set_scheduled_now_for_tests(one_task_clock())
            })
            .await
    }
}

#[async_trait]
impl Source for OneTaskSource {
    const COALESCING: Coalescing = Coalescing::OnePerKey;
    const UNDER_PLAN: UnderPlan = UnderPlan::Delivered;
    const USER_STOP: UserStop = UserStop::Receiving;
    const BACKGROUND_WORK: BackgroundWork = BackgroundWork::Ignored;

    async fn new(model: Arc<ContractModel>) -> Self {
        let process = ProcessChat::new(model).await;
        process.agent.set_scheduled_now_for_tests(one_task_clock());
        process
            .agent
            .schedule_task(
                process.chat,
                tickloop_agent_runtime::NewScheduledTask::new(
                    "0 0 * * *",
                    tickloop_agent_config::loop_skill::DEFAULT_INSTRUCTIONS_PROMPT,
                    true,
                ),
            )
            .await
            .unwrap();
        std::fs::create_dir_all(process.directory.path().join(".tickloop")).unwrap();
        Self {
            process,
            fired: Mutex::new(HashMap::new()),
            hand_overs: HandOvers::default(),
        }
    }

    fn agent(&self) -> &Agent {
        &self.process.agent
    }

    fn chat(&self) -> ChatId {
        self.process.chat
    }

    /// The `n`th delivery is the run the task comes due for next, carrying `marker` from
    /// loop.md; a run handed over before is handed over again at its own time.
    async fn deliver(&self, n: u64, marker: &str) {
        std::fs::write(self.loop_file(), marker).unwrap();
        let repeated = self.fired.lock().unwrap().get(&n).copied();
        let Some(due_at) = repeated.or(self.task().await.and_then(|task| task.next_fire_at_ms))
        else {
            self.hand_overs.record(n, Err("no task".into()));
            return;
        };
        let outcome = match self
            .process
            .agent
            .wake_scheduled_tasks(self.process.chat, due_at)
            .await
        {
            Ok(receipts) => receipts
                .into_iter()
                .next()
                .ok_or_else(|| "nothing was due".to_owned()),
            Err(error) => Err(error.to_string()),
        };
        if outcome.is_ok() {
            self.fired.lock().unwrap().entry(n).or_insert(due_at);
        }
        self.hand_overs.record(n, outcome);
    }

    async fn end(&self) {
        if let Some(task) = self.task().await {
            self.process
                .agent
                .delete_scheduled_task(self.process.chat, task.id)
                .await
                .unwrap();
        }
    }

    async fn replayed(&self, n: u64) -> bool {
        self.hand_overs.replayed(n)
    }

    /// The process goes down after the run's ledger was stored and before the task's next time
    /// was: the next process loads the task as it was, and the same run, due again, is answered
    /// by its record.
    async fn crash_and_reopen(self, model: Arc<ContractModel>) -> Self {
        let image = self.process.crashed().await;
        let transcript = tickloop_agent_test_support::transcript_file(&image, self.process.chat);
        let mut lines: Vec<String> = std::fs::read_to_string(&transcript)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        if let Some(advanced) = lines
            .iter()
            .rposition(|line| line.contains("\"type\":\"scheduled-tasks\""))
        {
            lines.remove(advanced);
        }
        std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();
        let process = self.resumed(image, model).await;
        let fired = self.fired.into_inner().unwrap();
        let hand_overs = HandOvers(Mutex::new(self.hand_overs.0.lock().unwrap().clone()));
        Self {
            process,
            fired: Mutex::new(fired),
            hand_overs,
        }
    }

    /// Another process holding the same Chat at the same time; only the lease holder hands the
    /// run over.
    async fn another_host(&self, model: Arc<ContractModel>) -> Self {
        Self {
            process: self.resumed(self.process.root.clone(), model).await,
            fired: Mutex::new(HashMap::new()),
            hand_overs: HandOvers::default(),
        }
    }
}

/// One task's later run merges into its run waiting behind a busy turn: the ledger records the
/// merge under the task, the task counts the passed-over time as skipped, and cancelling the
/// task withdraws the waiting run with its reason.
#[tokio::test(flavor = "multi_thread")]
async fn one_tasks_later_run_merges_counts_as_skipped_and_cancelling_withdraws() {
    let model = ContractModel::new();
    let source = OneTaskSource::new(model.clone()).await;
    hold_a_turn(&source.process.agent, source.process.chat, &model).await;
    source.deliver(1, "first-run").await;
    source.deliver(2, "second-run").await;
    let task = source.task().await.unwrap();
    assert_eq!(task.skipped, 1);
    source.process.agent.flush_transcripts().await;
    let deliveries = stored_trigger_deliveries(&source.process.root, source.process.chat);
    let states: Vec<_> = deliveries
        .iter()
        .filter(|delivery| {
            matches!(
                &delivery.identity,
                tickloop_agent_protocol::TriggerIdentity::ScheduledTask { task_id, .. }
                    if *task_id == task.id
            )
        })
        .map(|delivery| delivery.state.clone())
        .collect();
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(
        matches!(
            states[1],
            InputState::Settled {
                outcome: SettledOutcome::Merged { .. }
            }
        ),
        "{states:?}"
    );

    source.end().await;
    model.release_held_turn();
    wait_for_state(&source.process.agent, source.process.chat, LoopState::Idle).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("first-run") + model.seen("second-run"), 0);
    source.process.agent.flush_transcripts().await;
    let withdrawn = stored_trigger_deliveries(&source.process.root, source.process.chat)
        .into_iter()
        .find(|delivery| {
            delivery.state
                == InputState::Settled {
                    outcome: SettledOutcome::Discarded {
                        reason: DiscardReason::SourceEnded,
                    },
                }
        });
    assert!(withdrawn.is_some());
    source.process.agent.shutdown().await;
}

/// A Continue whose caller stops waiting after the switch turned here still watches the
/// artifact links the stop stopped watching, and their requests reach the Loop.
#[tokio::test(flavor = "multi_thread")]
async fn a_continue_dropped_midway_still_watches_the_links_again() {
    let model = ContractModel::new();
    let source = LocalFeedbackSource::new(model.clone()).await;
    let (agent, chat) = (source.agent().clone(), source.chat());
    assert!(agent.feedback_watcher_running_for_tests(chat).await);
    agent.stop(chat).await.unwrap();
    assert!(!agent.feedback_watcher_running_for_tests(chat).await);
    let changed = agent.pause_next_for_tests(
        chat,
        tickloop_agent_runtime::trigger_pauses::PausePoint::ReceivingChanged,
    );
    let continued = tokio::spawn({
        let agent = agent.clone();
        async move { agent.continue_loop(chat).await }
    });
    tokio::time::timeout(WAIT, changed.entered)
        .await
        .expect("the switch turned here")
        .unwrap();
    continued.abort();
    assert!(continued.await.unwrap_err().is_cancelled());
    drop(changed.release);
    tokio::time::timeout(WAIT, async {
        while !agent.feedback_watcher_running_for_tests(chat).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the links are watched again");
    source.deliver(1, "after-the-continue").await;
    model.wait_seen("after-the-continue", 1).await;
    agent.shutdown().await;
}
