//! The stateless-host contract: real Harness instances, one per holding of a Chat's lease,
//! driven through `advance` by the reference host under takeovers, local deadlines, stream
//! failures and crashes. Every publish is checked as it happens: a commit or resync marker
//! never names a seq above the committed `last_seq`, and a delta never goes out before the
//! start of its answer is committed.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, Content, EventKind, ModelIdentity, PromptPolicy, Protocol, Usage,
};
use tickloop_agent_runtime::transcript::TranscriptStore;
use tickloop_agent_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, ToolPolicy, TurnOp,
    TurnOutcome, TurnRequest, UserMessage,
};
use tickloop_agent_test_support::event_stream::{
    MemoryEventStream, StreamEntry, StreamError, StreamItem,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::stateless_host::{
    CrashPoint, DurableChats, Halt, HostConfig, LeaseError, ManualClock, ReferenceHost,
    StreamFollower, TurnReport,
};
use tickloop_agent_transcript::{InputState, Line, ResponseState, SettledOutcome};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);
/// Past the default lease TTL.
const LEASE_EXPIRED: Duration = Duration::from_secs(31);
const LEAK_CYCLES: usize = 50;

enum Act {
    /// Announces that it holds the request and waits to be released.
    Gate,
    Delta(&'static str),
    Answer(&'static str),
}

/// Answers each request by its script, or with "answer" once the scripts run out.
struct Model {
    scripts: Mutex<VecDeque<Vec<Act>>>,
    requests: Mutex<Vec<ModelRequest>>,
    holding: Notify,
    released: Notify,
}

impl Model {
    fn new(scripts: Vec<Vec<Act>>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::default(),
            holding: Notify::new(),
            released: Notify::new(),
        })
    }

    fn requests(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn last_request(&self) -> String {
        format!(
            "{:?}",
            self.requests.lock().unwrap().last().unwrap().messages
        )
    }

    async fn until_holding(&self) {
        tokio::time::timeout(WAIT, self.holding.notified())
            .await
            .expect("the model holds a request");
    }

    fn release(&self) {
        self.released.notify_one();
    }
}

#[async_trait]
impl ModelClient for Model {
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
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![Act::Answer("answer")]);
        for act in script {
            match act {
                Act::Gate => {
                    self.holding.notify_one();
                    tokio::select! {
                        _ = self.released.notified() => {}
                        _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                    }
                }
                Act::Delta(text) => {
                    let _ = events.send(ModelDelta::Text(text.into()));
                }
                Act::Answer(text) => {
                    return Ok(ModelOutput {
                        private_source: None,
                        content: vec![Content::Text { text: text.into() }],
                        usage: Usage::default(),
                        stop_reason: "end_turn".into(),
                    });
                }
            }
        }
        Err(ModelError::InvalidResponse(
            "the script has no answer".into(),
        ))
    }
}

/// The stage-one cloud composition: no Folder, WebFetch alone, no prompt that waits for a
/// person.
fn cloud_harness(
    model: &Arc<Model>,
) -> impl Fn(Arc<dyn TranscriptStore>) -> Agent + Send + Sync + 'static {
    let model: Arc<dyn ModelClient> = model.clone();
    move |store| {
        Agent::with_seams(
            HarnessSeams {
                transcripts: Some(store),
                environment: None,
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions {
                model: tickloop_agent_test_support::config(),
                settings: None,
                host: AgentHostOptions {
                    prompt_policy: PromptPolicy::DenyOnPrompt,
                    ..AgentHostOptions::product_defaults()
                },
                tools: ToolPolicy::web_fetch_only(),
                host_rules: Vec::new(),
            },
        )
        .unwrap()
    }
}

fn prompt(after: ParentSelection, text: &str) -> TurnRequest {
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
        delivery: Delivery::SendNow,
        host_context: None,
    }
}

/// One Chat with its durable side and Event Stream; every publish is checked as it happens.
struct World {
    clock: Arc<ManualClock>,
    chats: Arc<DurableChats>,
    stream: Arc<MemoryEventStream>,
    chat: Uuid,
}

fn world() -> World {
    let clock = ManualClock::new();
    let chats = DurableChats::new(clock.clone());
    let stream = Arc::new(MemoryEventStream::new());
    let observed = chats.clone();
    stream.observe(move |chat, entries| check_publish(&observed, chat, entries));
    World {
        clock,
        chats,
        stream,
        chat: Uuid::new_v4(),
    }
}

fn check_publish(chats: &DurableChats, chat: Uuid, entries: &[StreamEntry]) {
    let history = chats.history(chat);
    for entry in entries {
        match &entry.item {
            StreamItem::Resync | StreamItem::Committed { .. } => assert!(
                entry.seq <= history.last_seq,
                "seq {} went out above the committed last_seq {}",
                entry.seq,
                history.last_seq
            ),
            StreamItem::Event(event) if event.kind.is_transient() => assert!(
                history.lines.iter().any(|line| matches!(
                    line,
                    Line::Response(response) if response.run_id == event.run_id
                )),
                "a delta of Run {} went out before its answer's start was committed",
                event.run_id
            ),
            StreamItem::Event(_) => {}
        }
    }
}

impl World {
    fn host(&self, holder: &str, model: &Arc<Model>) -> ReferenceHost {
        self.host_with(holder, model, self.clock.clone(), HostConfig::default())
    }

    fn host_with(
        &self,
        holder: &str,
        model: &Arc<Model>,
        clock: Arc<ManualClock>,
        config: HostConfig,
    ) -> ReferenceHost {
        ReferenceHost::new(
            holder,
            self.chats.clone(),
            self.stream.clone(),
            clock,
            config,
            cloud_harness(model),
        )
    }

    fn follower(&self) -> StreamFollower {
        StreamFollower::connect(&self.chats, self.chat)
    }

    fn catch_up(&self, follower: &mut StreamFollower) {
        follower.catch_up(&self.chats, &self.stream);
    }

    fn lines(&self) -> Vec<Line> {
        self.chats.history(self.chat).lines
    }

    fn entries(&self) -> Vec<StreamEntry> {
        self.stream.entries(self.chat)
    }

    fn input_state(&self, input: Uuid) -> Option<InputState> {
        self.lines().into_iter().rev().find_map(|line| match line {
            Line::Input(entry) if entry.input_id == input => Some(entry.state),
            _ => None,
        })
    }

    /// The latest answer to `input`.
    fn answer_to(&self, input: Uuid) -> Uuid {
        self.lines()
            .into_iter()
            .rev()
            .find_map(|line| match line {
                Line::Response(entry) if entry.input_id == Some(input) => Some(entry.response_id),
                _ => None,
            })
            .expect("the input has an answer")
    }

    /// How many stored user entries carry `text`.
    fn prompts(&self, text: &str) -> usize {
        self.lines()
            .iter()
            .filter(|line| {
                matches!(line, Line::User(_)) && serde_json::to_string(line).unwrap().contains(text)
            })
            .count()
    }

    fn assert_follower_current(&self, follower: &StreamFollower) {
        assert_eq!(
            as_json(follower.lines()),
            as_json(&self.lines()),
            "the client's history differs from the stored one"
        );
    }

    fn assert_acknowledged_once(&self, input: Uuid) {
        assert_eq!(self.chats.acknowledgements(self.chat, input), 1);
    }
}

fn as_json(lines: &[Line]) -> Vec<serde_json::Value> {
    lines
        .iter()
        .map(|line| serde_json::to_value(line).unwrap())
        .collect()
}

fn is_completed(state: Option<InputState>) -> bool {
    matches!(
        state,
        Some(InputState::Settled {
            outcome: SettledOutcome::Completed { .. }
        })
    )
}

fn assert_completed_and_acknowledged(report: &TurnReport) {
    assert!(
        matches!(report.outcome, Some(TurnOutcome::Completed { .. })),
        "{report:?}"
    );
    assert!(report.acknowledged, "{report:?}");
    assert_eq!(report.halt, None);
}

fn accepts_an_input(lines: &[Line]) -> bool {
    lines.iter().any(
        |line| matches!(line, Line::Input(entry) if matches!(entry.state, InputState::Accepted)),
    )
}

fn starts_an_answer(lines: &[Line]) -> bool {
    lines.iter().any(|line| {
        matches!(line, Line::Response(entry) if matches!(entry.state, ResponseState::Started))
    })
}

fn settles_an_input(lines: &[Line]) -> bool {
    lines.iter().any(|line| {
        matches!(line, Line::Input(entry) if matches!(entry.state, InputState::Settled { .. }))
    })
}

fn is_resync(entry: &StreamEntry) -> bool {
    matches!(entry.item, StreamItem::Resync)
}

async fn eventually(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

#[test]
fn activation_fences_older_epochs_before_the_newer_one_publishes() {
    let stream = MemoryEventStream::new();
    let chat = Uuid::new_v4();
    let entry = |seq| StreamEntry {
        seq,
        item: StreamItem::Resync,
    };

    assert_eq!(
        stream.publish(chat, 1, &[entry(1)]),
        Err(StreamError::NotActivated { fence: 0 })
    );
    assert_eq!(stream.activate(chat, 1), Ok(0));
    stream.publish(chat, 1, &[entry(1), entry(2)]).unwrap();

    assert_eq!(stream.activate(chat, 2), Ok(2));
    assert_eq!(
        stream.publish(chat, 1, &[entry(3)]),
        Err(StreamError::StaleEpoch { fence: 2 }),
        "refused although epoch 2 has published nothing yet"
    );
    assert_eq!(
        stream.activate(chat, 1),
        Err(StreamError::StaleEpoch { fence: 2 })
    );
    assert_eq!(
        stream.publish(chat, 3, &[entry(3)]),
        Err(StreamError::NotActivated { fence: 2 })
    );
    assert_eq!(
        stream.publish(chat, 2, &[entry(2)]),
        Err(StreamError::SeqConflict { top: 2 })
    );
    stream.publish(chat, 2, &[entry(3)]).unwrap();
    assert_eq!(stream.top(chat), 3);
    assert_eq!(stream.rejections().len(), 5);
}

/// The old holder still believes it is in time (its clock lags the lease's), yet once the
/// newer holder activated the stream its delta is refused, before the newer holder published.
#[tokio::test]
async fn an_older_holder_cannot_publish_once_a_newer_one_activated() {
    let world = world();
    let model = Model::new(vec![vec![
        Act::Gate,
        Act::Delta("late"),
        Act::Gate,
        Act::Answer("never"),
    ]]);
    let lagging_clock = ManualClock::new();
    let old = world.host_with("old", &model, lagging_clock, HostConfig::default());
    let new = world.host("new", &model);
    let request = prompt(ParentSelection::Root, "hello");

    let holding = old.acquire(world.chat).unwrap();
    let running = {
        let request = request.clone();
        tokio::spawn(async move { holding.run(request).await })
    };
    model.until_holding().await;
    // Once the title is stored and the answer's start is published, the old holder has
    // nothing left to commit before its next delta: that delta is its next write.
    eventually(
        "the title is stored and the answer's start published",
        || {
            let entries = world.entries();
            let published = |kind: fn(&EventKind) -> bool| {
                entries.iter().any(
                    |entry| matches!(&entry.item, StreamItem::Event(event) if kind(&event.kind)),
                )
            };
            published(|kind| matches!(kind, EventKind::ResponseStarted { .. }))
                && published(|kind| matches!(kind, EventKind::Started))
                && world
                    .lines()
                    .iter()
                    .any(|line| matches!(line, Line::AiTitle(_)))
        },
    )
    .await;

    world.clock.advance(LEASE_EXPIRED);
    let taking_over = new.acquire(world.chat).unwrap();
    assert_eq!(taking_over.lease().epoch, 2);
    taking_over.activate().unwrap();
    let published = world.entries().len();
    model.release();

    let report = tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    assert_eq!(report.halt, Some(Halt::StaleEpoch), "{report:?}");
    assert!(!report.acknowledged);
    assert_eq!(world.entries().len(), published);
    assert!(
        world
            .stream
            .rejections()
            .iter()
            .any(|rejection| rejection.epoch == 1
                && rejection.error == StreamError::StaleEpoch { fence: 2 })
    );

    let mut follower = world.follower();
    let finished = taking_over.run(request.clone()).await;
    assert_completed_and_acknowledged(&finished);
    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
    assert!(is_completed(world.input_state(request.input_id)));
    world.assert_acknowledged_once(request.input_id);
}

#[tokio::test]
async fn a_holder_stops_at_its_local_deadline_and_the_next_holder_finishes_the_answer() {
    let world = world();
    let model = Model::new(vec![vec![Act::Gate, Act::Answer("too late")]]);
    let first = world.host("first", &model);
    let second = world.host("second", &model);
    let request = prompt(ParentSelection::Root, "hello");
    let mut follower = world.follower();

    let holding = first.acquire(world.chat).unwrap();
    let running = {
        let request = request.clone();
        tokio::spawn(async move { holding.run(request).await })
    };
    model.until_holding().await;
    let lines = world.lines().len();
    let entries = world.entries().len();

    // Past the 25s local deadline, before the 30s lease expires.
    world.clock.advance(Duration::from_secs(26));
    let report = tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    assert_eq!(report.halt, Some(Halt::DeadlinePassed), "{report:?}");
    assert!(!report.acknowledged);
    assert_eq!(
        world.lines().len(),
        lines,
        "nothing appended after the deadline"
    );
    assert_eq!(world.entries().len(), entries, "nothing published after it");
    assert!(matches!(
        world.input_state(request.input_id),
        Some(InputState::Accepted)
    ));
    assert!(matches!(
        second.acquire(world.chat),
        Err(LeaseError::Held { .. })
    ));

    world.clock.advance(Duration::from_secs(5));
    let finished = second.deliver(world.chat, request.clone()).await.unwrap();
    assert_completed_and_acknowledged(&finished);
    assert_eq!(model.requests(), 2, "the next holder answers again");
    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
    assert_eq!(follower.reloads(), 1, "the takeover after expiry resyncs");
    world.assert_acknowledged_once(request.input_id);
}

#[tokio::test]
async fn renewing_the_lease_moves_the_local_deadline() {
    let world = world();
    let model = Model::new(vec![vec![Act::Gate, Act::Answer("in time")]]);
    let host = world.host("host", &model);
    let other = world.host("other", &model);
    let request = prompt(ParentSelection::Root, "hello");

    let holding = Arc::new(host.acquire(world.chat).unwrap());
    let running = {
        let (holding, request) = (holding.clone(), request.clone());
        tokio::spawn(async move { holding.run(request).await })
    };
    model.until_holding().await;
    world.clock.advance(Duration::from_secs(20));
    assert!(holding.renew());
    world.clock.advance(Duration::from_secs(11));
    assert!(matches!(
        other.acquire(world.chat),
        Err(LeaseError::Held { .. })
    ));
    model.release();

    let report = tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    assert_completed_and_acknowledged(&report);
}

#[tokio::test]
async fn deltas_never_overtake_the_committed_start_of_their_answer() {
    let world = world();
    let model = Model::new(vec![vec![
        Act::Delta("Hel"),
        Act::Delta("lo"),
        Act::Gate,
        Act::Answer("Hello"),
    ]]);
    let host = world.host("host", &model);
    let mut follower = world.follower();
    let request = prompt(ParentSelection::Root, "greet me");

    // Nothing commits until the deltas were streamed: they wait for the answer's start.
    let holding = Arc::new(host.acquire(world.chat).unwrap());
    holding.store().fail_appends();
    let running = {
        let (holding, request) = (holding.clone(), request.clone());
        tokio::spawn(async move { holding.run(request).await })
    };
    model.until_holding().await;
    eventually("the deltas wait behind the uncommitted start", || {
        holding
            .held_events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::Text(_)))
    })
    .await;
    assert!(world.lines().is_empty());
    assert!(
        !world.entries().iter().any(|entry| matches!(
            &entry.item,
            StreamItem::Event(event) if event.kind.is_transient()
        )),
        "a delta went out before anything was committed"
    );
    holding.store().heal();
    model.release();
    let report = tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    assert_completed_and_acknowledged(&report);

    let entries = world.entries();
    let deltas = entries
        .iter()
        .filter_map(|entry| match &entry.item {
            StreamItem::Event(event) => match &event.kind {
                EventKind::Text(text) => Some((entry.seq, event.run_id, text.as_str())),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    // The Harness may coalesce deltas; all of the streamed text goes out.
    assert_eq!(
        deltas.iter().map(|(_, _, text)| *text).collect::<String>(),
        "Hello"
    );
    for (seq, run, _) in deltas {
        assert!(
            entries.iter().any(|entry| entry.seq < seq
                && matches!(&entry.item, StreamItem::Committed { lines } if lines.iter().any(|line| matches!(
                    line,
                    Line::Response(response) if response.run_id == run && response.state == ResponseState::Started
                )))),
            "the delta at {seq} went out before the commit that starts its answer"
        );
    }
    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
    assert_eq!(follower.reloads(), 0);
}

#[tokio::test]
async fn a_failed_publish_owes_a_marker_at_or_below_last_seq_and_clients_reload_once() {
    let world = world();
    let model = Model::new(Vec::new());
    let host = world.host("host", &model);
    let mut follower = world.follower();
    let first = prompt(ParentSelection::Root, "first prompt");
    assert_completed_and_acknowledged(&host.deliver(world.chat, first.clone()).await.unwrap());
    world.catch_up(&mut follower);
    assert_eq!(follower.reloads(), 0);

    world.stream.fail_next_publishes(1);
    let second = prompt(
        ParentSelection::Message(world.answer_to(first.input_id)),
        "second prompt",
    );
    let report = host.deliver(world.chat, second.clone()).await.unwrap();
    assert_completed_and_acknowledged(&report);
    let markers = world
        .entries()
        .into_iter()
        .filter(is_resync)
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 1);
    assert!(markers[0].seq <= world.chats.last_seq(world.chat));

    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
    assert_eq!(follower.reloads(), 1);
    // Following on after the reload skips the marker: the client does not reload again.
    world.catch_up(&mut follower);
    assert_eq!(follower.reloads(), 1);
    let mut late_client = world.follower();
    world.catch_up(&mut late_client);
    assert_eq!(late_client.reloads(), 0);
    world.assert_follower_current(&late_client);
}

#[tokio::test]
async fn a_publish_lost_after_its_commit_withholds_the_ack_until_redelivery_repairs_it() {
    let world = world();
    let model = Model::new(Vec::new());
    let config = HostConfig {
        drain_timeout: Duration::from_millis(200),
        ..HostConfig::default()
    };
    let first_host = world.host_with("first", &model, world.clock.clone(), config);
    let second_host = world.host("second", &model);
    let mut follower = world.follower();
    let first = prompt(ParentSelection::Root, "first prompt");
    assert_completed_and_acknowledged(
        &first_host.deliver(world.chat, first.clone()).await.unwrap(),
    );
    world.catch_up(&mut follower);

    let second = prompt(
        ParentSelection::Message(world.answer_to(first.input_id)),
        "second prompt",
    );
    let holding = first_host.acquire(world.chat).unwrap();
    holding.activate().unwrap();
    let top = world.stream.top(world.chat);
    world.stream.set_unavailable(true);
    let lost = holding.run(second.clone()).await;
    assert!(matches!(lost.outcome, Some(TurnOutcome::Completed { .. })));
    assert_eq!(lost.halt, Some(Halt::DrainTimedOut));
    assert!(!lost.acknowledged);
    assert!(
        is_completed(world.input_state(second.input_id)),
        "the answer is committed"
    );
    let last_seq = world.chats.last_seq(world.chat);
    assert!(last_seq > world.stream.top(world.chat));
    assert!(
        last_seq < top + 10,
        "the owed marker is retried at its seq instead of raising last_seq again: {top} → {last_seq}"
    );
    assert!(
        matches!(
            second_host.acquire(world.chat),
            Err(LeaseError::Held { .. })
        ),
        "a holder that owes a marker does not release the lease as a clean handoff"
    );
    world.catch_up(&mut follower);
    assert_ne!(
        as_json(follower.lines()),
        as_json(&world.lines()),
        "the client missed the commit"
    );

    world.stream.set_unavailable(false);
    world.clock.advance(LEASE_EXPIRED);
    let requests = model.requests();
    let repaired = second_host
        .deliver(world.chat, second.clone())
        .await
        .unwrap();
    assert_completed_and_acknowledged(&repaired);
    assert_eq!(
        model.requests(),
        requests,
        "the settled input is not answered again"
    );
    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
    assert_eq!(follower.reloads(), 1);
    world.assert_acknowledged_once(second.input_id);
}

#[tokio::test]
async fn a_holder_that_cannot_activate_the_stream_keeps_the_lease_and_runs_nothing() {
    let world = world();
    let model = Model::new(Vec::new());
    let host = world.host("host", &model);
    let other = world.host("other", &model);
    let request = prompt(ParentSelection::Root, "hello");

    world.stream.set_unavailable(true);
    let report = host.deliver(world.chat, request.clone()).await.unwrap();
    assert!(matches!(report.halt, Some(Halt::Failed(_))), "{report:?}");
    assert!(!report.acknowledged);
    assert_eq!(model.requests(), 0);
    assert!(world.lines().is_empty());
    assert!(matches!(
        other.acquire(world.chat),
        Err(LeaseError::Held { .. })
    ));

    world.stream.set_unavailable(false);
    world.clock.advance(LEASE_EXPIRED);
    let mut follower = world.follower();
    let finished = other.deliver(world.chat, request.clone()).await.unwrap();
    assert_completed_and_acknowledged(&finished);
    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
    world.assert_acknowledged_once(request.input_id);
}

/// What a crash on the second turn and its redelivery to another host left behind.
struct Crash {
    world: World,
    second: TurnRequest,
    requests_after_crash: usize,
    requests_after_redelivery: usize,
}

/// Completes a first turn, crashes the holder of the second at `point`, checks what the dead
/// holder left with `after_crash`, then lets its lease expire and delivers the second turn
/// to another host.
async fn crash_on_second_turn(point: CrashPoint, after_crash: impl FnOnce(&World, Uuid)) -> Crash {
    let world = world();
    let model = Model::new(Vec::new());
    let crashing = world.host("crashing", &model);
    let surviving = world.host("surviving", &model);
    let mut follower = world.follower();
    let first = prompt(ParentSelection::Root, "first prompt");
    assert_completed_and_acknowledged(&crashing.deliver(world.chat, first.clone()).await.unwrap());
    world.catch_up(&mut follower);

    let second = prompt(
        ParentSelection::Message(world.answer_to(first.input_id)),
        "second prompt",
    );
    let holding = crashing.acquire(world.chat).unwrap();
    holding.crash_at(point);
    let crashed = holding.run(second.clone()).await;
    assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
    assert!(!crashed.acknowledged);
    after_crash(&world, second.input_id);
    let requests_after_crash = model.requests();
    assert!(
        matches!(surviving.acquire(world.chat), Err(LeaseError::Held { .. })),
        "a dead holder never releases its lease"
    );

    world.clock.advance(LEASE_EXPIRED);
    let redelivered = surviving.deliver(world.chat, second.clone()).await.unwrap();
    assert_completed_and_acknowledged(&redelivered);
    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
    assert_eq!(
        follower.reloads(),
        1,
        "the takeover after the crash resyncs once"
    );
    assert!(is_completed(world.input_state(second.input_id)));
    assert_eq!(world.prompts("second prompt"), 1);
    world.assert_acknowledged_once(second.input_id);
    Crash {
        requests_after_redelivery: model.requests(),
        world,
        second,
        requests_after_crash,
    }
}

#[tokio::test]
async fn a_crash_before_the_first_commit_leaves_the_input_to_run_again() {
    let crash = crash_on_second_turn(
        CrashPoint::BeforeCommit(accepts_an_input),
        |world, input| {
            assert_eq!(world.input_state(input), None, "nothing of the turn landed");
        },
    )
    .await;
    assert!(crash.requests_after_redelivery > crash.requests_after_crash);
}

#[tokio::test]
async fn a_crash_after_a_commit_before_its_publish_continues_the_answer() {
    let crash = crash_on_second_turn(CrashPoint::AfterCommit(starts_an_answer), |world, input| {
        assert_eq!(world.input_state(input), Some(InputState::Accepted));
    })
    .await;
    assert!(crash.requests_after_redelivery > crash.requests_after_crash);
    let attempts = crash
        .world
        .lines()
        .into_iter()
        .filter_map(|line| match line {
            Line::Response(entry) if entry.input_id == Some(crash.second.input_id) => {
                Some((entry.attempt, entry.state))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        attempts
            .iter()
            .any(|(attempt, state)| *attempt == 1 && matches!(state, ResponseState::Ended { .. })),
        "the lost attempt is ended by its next holder: {attempts:?}"
    );
    assert!(
        attempts.iter().any(|(attempt, _)| *attempt == 2),
        "{attempts:?}"
    );
}

#[tokio::test]
async fn a_crash_after_publishing_before_the_ack_returns_the_settled_outcome() {
    let crash = crash_on_second_turn(CrashPoint::BeforeAck, |world, input| {
        assert!(is_completed(world.input_state(input)));
    })
    .await;
    assert_eq!(
        crash.requests_after_redelivery, crash.requests_after_crash,
        "the settled input is not answered again"
    );
}

#[tokio::test]
async fn a_crash_after_the_terminal_commit_before_the_ack_returns_the_settled_outcome() {
    let crash = crash_on_second_turn(CrashPoint::AfterCommit(settles_an_input), |world, input| {
        assert!(is_completed(world.input_state(input)));
        assert!(
            world.chats.last_seq(world.chat) > world.stream.top(world.chat),
            "the terminal commit was never published"
        );
    })
    .await;
    assert_eq!(crash.requests_after_redelivery, crash.requests_after_crash);
}

/// A newer lease alone, without activating the stream, fences the old holder's final append.
#[tokio::test]
async fn the_final_append_under_a_newer_lease_is_fenced() {
    let world = world();
    let model = Model::new(vec![vec![Act::Gate, Act::Answer("late answer")]]);
    let old = world.host_with("old", &model, ManualClock::new(), HostConfig::default());
    let new = world.host("new", &model);
    let request = prompt(ParentSelection::Root, "hello");
    let mut follower = world.follower();

    let holding = old.acquire(world.chat).unwrap();
    let running = {
        let request = request.clone();
        tokio::spawn(async move {
            let report = holding.run(request).await;
            (report, holding.store().appends())
        })
    };
    model.until_holding().await;
    world.clock.advance(LEASE_EXPIRED);
    let taking_over = new.acquire(world.chat).unwrap();
    model.release();

    let (report, attempted) = tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    assert_eq!(report.halt, Some(Halt::Fenced), "{report:?}");
    assert!(!report.acknowledged);
    assert!(attempted > 0, "the old holder tried to append");
    assert!(!settles_an_input(&world.lines()));
    assert!(
        !serde_json::to_string(&world.lines())
            .unwrap()
            .contains("late answer")
    );

    let finished = taking_over.run(request.clone()).await;
    assert_completed_and_acknowledged(&finished);
    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
    world.assert_acknowledged_once(request.input_id);
}

#[tokio::test]
async fn two_hosts_alternate_as_holders_without_resyncing_clients() {
    let world = world();
    let model = Model::new(Vec::new());
    let hosts = [world.host("left", &model), world.host("right", &model)];
    let mut follower = world.follower();
    let mut after = ParentSelection::Root;
    for (turn, host) in hosts.iter().cycle().take(6).enumerate() {
        let request = prompt(after, &format!("prompt {turn}"));
        let report = host.deliver(world.chat, request.clone()).await.unwrap();
        assert_completed_and_acknowledged(&report);
        assert_eq!(report.epoch, turn as u64 + 1);
        world.catch_up(&mut follower);
        world.assert_follower_current(&follower);
        world.assert_acknowledged_once(request.input_id);
        after = ParentSelection::Message(world.answer_to(request.input_id));
    }
    assert_eq!(follower.reloads(), 0, "clean handoffs write no marker");
    assert!(!world.entries().iter().any(is_resync));
    let last_request = model.last_request();
    for turn in 0..6 {
        assert!(last_request.contains(&format!("prompt {turn}")));
    }
}

async fn wait_for_alive_tasks(expected: usize, cycle: usize) {
    let metrics = tokio::runtime::Handle::current().metrics();
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics.num_alive_tasks() > expected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "cycle {cycle}: {} tasks alive, {expected} before",
            metrics.num_alive_tasks()
        )
    });
}

/// Each cycle is one holding with a fresh Harness, alternating between two hosts; after it,
/// neither the Harness nor the publisher leaves a task behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_holdings_leave_no_instance_or_task_behind() {
    let world = world();
    let model = Model::new(Vec::new());
    let hosts = [world.host("left", &model), world.host("right", &model)];
    let baseline = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    let mut after = ParentSelection::Root;
    for (cycle, host) in hosts.iter().cycle().take(LEAK_CYCLES).enumerate() {
        let request = prompt(after, "prompt");
        let report = host.deliver(world.chat, request.clone()).await.unwrap();
        assert_completed_and_acknowledged(&report);
        after = ParentSelection::Message(world.answer_to(request.input_id));
        wait_for_alive_tasks(baseline, cycle).await;
    }
    let mut follower = world.follower();
    world.catch_up(&mut follower);
    world.assert_follower_current(&follower);
}
