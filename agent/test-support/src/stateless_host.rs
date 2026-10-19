//! A reference stateless host. Each holding of a Chat's lease runs one turn of a fresh Harness
//! and publishes it to an Event Stream under the publisher contract of the Harness split:
//!
//! 1. The lease raises the epoch; the local deadline is the request's send time plus the TTL
//!    minus a margin, on the holder's own clock.
//! 2. Activating the stream fences every older epoch before this holder publishes anything.
//! 3. Seqs start above both the committed `last_seq` and the stream's top.
//! 4. A takeover writes a resync marker unless the previous holder, if any, released the lease
//!    and published everything it committed. Every marker first raises `last_seq` to its seq.
//! 5. `with_seams → advance → outcome → quiesce → shutdown → sync`, or for a scheduler's wake
//!    `with_seams → wake(due_at) → quiesce → shutdown → sync`.
//! 6. Events go out as an ordered prefix: a Harness event that is not transient waits for the
//!    commit that reflects it, and everything after it waits too. Nothing is published or
//!    appended once the local deadline has passed.
//! 7. A publish that fails, or events the Harness broadcast dropped, owe a resync marker.
//! 8. The task is acknowledged only after `sync` succeeded, every event is published and no
//!    marker is owed; a fence or a stale epoch stops the holder without acknowledging. What the
//!    Chat waits on once quiet is written with the acknowledgement, so the host knows when to
//!    come back: its scheduler wakes the family at `wake_at`, giving that time as `due_at`.
//!
//! A Harness whose waits are durable leaves a Run that waits for a person suspended in the
//! Transcript; the holding that delivers the reply rebuilds it from there.
//!
//! The lease is the root Chat's and covers its whole family: the members its Runs delegate write
//! through the same holding, and a task may target any of them. The stream carries the root
//! Chat's commits and events.
//!
//! Lines and `last_seq` commit together, so a client that loads the history at `last_seq` and
//! follows the stream after it sees every commit exactly once.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tickloop_agent_protocol::{ChatId, Event, LoopId};
use tickloop_agent_runtime::transcript::{
    AppendAck, AppendBatch, BatchId, ChatPlacement, LoadRange, StorePosition, StoredTranscript,
    TranscriptError, TranscriptStore,
};
use tickloop_agent_runtime::{
    Agent, AgentError, Quiescence, ScheduleDecision, TurnOutcome, TurnRequest, Waits,
};
use tickloop_agent_transcript::Line;
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::event_stream::{MemoryEventStream, StreamEntry, StreamError, StreamItem};
use crate::transcripts::{FaultyTranscriptStore, InMemoryTranscriptStore};

/// A client that reloads more often than this while catching up once is looping.
const FOLLOWER_ROUNDS: usize = 64;

/// A monotonic clock the test moves by hand.
pub struct ManualClock {
    now: watch::Sender<Duration>,
}

impl ManualClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            now: watch::channel(Duration::ZERO).0,
        })
    }

    pub fn now(&self) -> Duration {
        *self.now.borrow()
    }

    pub fn advance(&self, by: Duration) {
        self.now.send_modify(|now| *now += by);
    }

    fn changes(&self) -> watch::Receiver<Duration> {
        self.now.subscribe()
    }
}

/// A Chat Lease as its holder knows it.
#[derive(Clone, Debug)]
pub struct Lease {
    pub chat: Uuid,
    pub epoch: u64,
    pub holder: String,
    pub previous: Option<PreviousLease>,
}

/// How the lease before this one ended, as of the acquisition.
#[derive(Clone, Copy, Debug)]
pub struct PreviousLease {
    pub epoch: u64,
    /// Its holder released it; otherwise it expired.
    pub released: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseError {
    Held { holder: String, epoch: u64 },
}

/// A Chat's history as one snapshot: its Lines and the `last_seq` they were committed at.
pub struct History {
    pub lines: Vec<Line>,
    pub last_seq: u64,
}

/// The durable side of the host: per Chat its lease, the Transcript, the committed `last_seq`
/// and the acknowledged tasks. Lines and `last_seq` commit in one step.
pub struct DurableChats {
    clock: Arc<ManualClock>,
    transcripts: Arc<InMemoryTranscriptStore>,
    rows: Mutex<HashMap<Uuid, ChatRow>>,
}

#[derive(Default)]
struct ChatRow {
    epoch: u64,
    lease: Option<LeaseRow>,
    last_seq: u64,
    /// The Harness's id of the root Chat, from its creation on.
    harness_chat: Option<ChatId>,
    /// The other members of the root Chat's family that committed Lines.
    members: HashSet<ChatId>,
    committed: HashSet<BatchId>,
    acknowledged: HashMap<Uuid, usize>,
    /// What the Chat waited on when its last task was acknowledged; `None` when it was idle.
    waiting: Option<Waits>,
}

impl ChatRow {
    /// A holding writes the root Chat its lease is for, the first it writes, and the members of
    /// its family.
    fn serves(&mut self, chat: ChatId) {
        self.harness_chat.get_or_insert(chat);
    }
}

struct LeaseRow {
    holder: String,
    expires_at: Duration,
    released: bool,
}

/// An append as the durable side took it.
struct Commit {
    ack: AppendAck,
    /// False when the batch had committed before and this is its retry.
    is_new: bool,
}

impl DurableChats {
    /// Leases expire by `clock`.
    pub fn new(clock: Arc<ManualClock>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            transcripts: Arc::new(InMemoryTranscriptStore::new()),
            rows: Mutex::default(),
        })
    }

    pub fn transcripts(&self) -> &Arc<InMemoryTranscriptStore> {
        &self.transcripts
    }

    /// The next epoch, unless an unreleased lease has not expired yet.
    pub fn acquire_lease(
        &self,
        chat: Uuid,
        holder: &str,
        ttl: Duration,
    ) -> Result<Lease, LeaseError> {
        let now = self.clock.now();
        let mut rows = self.rows.lock().unwrap();
        let row = rows.entry(chat).or_default();
        if let Some(lease) = &row.lease
            && !lease.released
            && lease.expires_at > now
        {
            return Err(LeaseError::Held {
                holder: lease.holder.clone(),
                epoch: row.epoch,
            });
        }
        let previous = row.lease.as_ref().map(|lease| PreviousLease {
            epoch: row.epoch,
            released: lease.released,
        });
        row.epoch += 1;
        row.lease = Some(LeaseRow {
            holder: holder.into(),
            expires_at: now + ttl,
            released: false,
        });
        Ok(Lease {
            chat,
            epoch: row.epoch,
            holder: holder.into(),
            previous,
        })
    }

    /// Extends a lease nobody took over.
    pub fn renew_lease(&self, lease: &Lease, ttl: Duration) -> bool {
        let now = self.clock.now();
        self.holding(lease, |row| {
            let held = row.lease.as_mut().filter(|held| !held.released);
            match held {
                Some(held) => {
                    held.expires_at = now + ttl;
                    Ok(())
                }
                None => Err(TranscriptError::Fenced),
            }
        })
        .is_ok()
    }

    pub fn release_lease(&self, lease: &Lease) {
        let _ = self.holding(lease, |row| {
            if let Some(held) = &mut row.lease {
                held.released = true;
            }
            Ok(())
        });
    }

    pub fn history(&self, chat: Uuid) -> History {
        let rows = self.rows.lock().unwrap();
        let Some(row) = rows.get(&chat) else {
            return History {
                lines: Vec::new(),
                last_seq: 0,
            };
        };
        History {
            lines: row
                .harness_chat
                .map(|harness_chat| self.transcripts.lines(harness_chat))
                .unwrap_or_default(),
            last_seq: row.last_seq,
        }
    }

    pub fn last_seq(&self, chat: Uuid) -> u64 {
        self.rows
            .lock()
            .unwrap()
            .get(&chat)
            .map_or(0, |row| row.last_seq)
    }

    /// The members of the Chat's family other than its root that committed Lines.
    pub fn members(&self, chat: Uuid) -> HashSet<ChatId> {
        self.rows
            .lock()
            .unwrap()
            .get(&chat)
            .map(|row| row.members.clone())
            .unwrap_or_default()
    }

    /// Every Line committed of the family member `member` of the Chat.
    pub fn member_lines(&self, member: ChatId) -> Vec<Line> {
        self.transcripts.lines(member)
    }

    pub fn harness_chat(&self, chat: Uuid) -> Option<ChatId> {
        self.rows
            .lock()
            .unwrap()
            .get(&chat)
            .and_then(|row| row.harness_chat)
    }

    /// What the Chat waited on when its last task was acknowledged.
    pub fn waiting(&self, chat: Uuid) -> Option<Waits> {
        self.rows
            .lock()
            .unwrap()
            .get(&chat)
            .and_then(|row| row.waiting.clone())
    }

    /// How many times the task of `input` was acknowledged.
    pub fn acknowledgements(&self, chat: Uuid, input: Uuid) -> usize {
        self.rows
            .lock()
            .unwrap()
            .get(&chat)
            .and_then(|row| row.acknowledged.get(&input).copied())
            .unwrap_or(0)
    }

    /// Runs `change` on the Chat's row while `lease` is its current lease.
    fn holding<T>(
        &self,
        lease: &Lease,
        change: impl FnOnce(&mut ChatRow) -> Result<T, TranscriptError>,
    ) -> Result<T, TranscriptError> {
        let mut rows = self.rows.lock().unwrap();
        match rows.get_mut(&lease.chat) {
            Some(row) if row.epoch == lease.epoch => change(row),
            _ => Err(TranscriptError::Fenced),
        }
    }

    fn check_chat(&self, lease: &Lease, chat: ChatId) -> Result<(), TranscriptError> {
        self.holding(lease, |row| {
            row.serves(chat);
            Ok(())
        })
    }

    /// The Harness created the root Chat of the lease's row.
    fn bind_root(&self, lease: &Lease, chat: ChatId) -> Result<(), TranscriptError> {
        self.holding(lease, |row| {
            row.harness_chat.get_or_insert(chat);
            Ok(())
        })
    }

    /// Commits the batch and, when it adds Lines, makes `seq` the committed `last_seq`.
    fn commit(
        &self,
        lease: &Lease,
        chat: ChatId,
        batch: &AppendBatch,
        seq: u64,
    ) -> Result<Commit, TranscriptError> {
        self.holding(lease, |row| {
            row.serves(chat);
            // The durable side keys retries by batch, as a database would by the batch id.
            let is_new = !row.committed.contains(&batch.id);
            let ack = self.transcripts.append_blocking(chat, batch)?;
            if is_new {
                row.committed.insert(batch.id);
                // The stream carries the root Chat's commits.
                if !batch.lines.is_empty() && row.harness_chat == Some(chat) {
                    assert!(seq > row.last_seq, "seqs only grow");
                    row.last_seq = seq;
                } else if !batch.lines.is_empty() {
                    row.members.insert(chat);
                }
            }
            Ok(Commit { ack, is_new })
        })
    }

    /// An empty append that raises `last_seq` to `seq`.
    fn raise_last_seq(&self, lease: &Lease, seq: u64) -> Result<(), TranscriptError> {
        self.holding(lease, |row| {
            row.last_seq = row.last_seq.max(seq);
            Ok(())
        })
    }

    fn complete_task(
        &self,
        lease: &Lease,
        input: Uuid,
        waiting: Option<Waits>,
    ) -> Result<(), TranscriptError> {
        self.holding(lease, |row| {
            *row.acknowledged.entry(input).or_default() += 1;
            row.waiting = waiting;
            Ok(())
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HostConfig {
    pub lease_ttl: Duration,
    /// How long before the lease's expiry the holder stops; more than one publish and one
    /// append take.
    pub lease_margin: Duration,
    /// How long, in real time, a holder waits for everything to be published.
    pub drain_timeout: Duration,
    /// The pause, in real time, before publishing again after the stream failed.
    pub retry_delay: Duration,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(30),
            lease_margin: Duration::from_secs(5),
            drain_timeout: Duration::from_secs(2),
            retry_delay: Duration::from_millis(5),
        }
    }
}

/// Why a holder stopped before acknowledging its task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Halt {
    /// A newer epoch activated the stream.
    StaleEpoch,
    /// The stream was not activated at this epoch.
    NotActivated,
    /// The durable side has a newer lease.
    Fenced,
    /// The local deadline passed.
    DeadlinePassed,
    /// The holder died at its crash point.
    Crashed,
    /// Not everything was published in time.
    DrainTimedOut,
    Failed(String),
}

/// Where a holder dies, as a process would: nothing of it is written or published afterwards,
/// and it neither acknowledges the task nor releases the lease.
#[derive(Clone, Copy, Debug)]
pub enum CrashPoint {
    /// As an append whose Lines match is about to commit; none of it lands.
    BeforeCommit(fn(&[Line]) -> bool),
    /// Right after an append whose Lines match commits, before it is published.
    AfterCommit(fn(&[Line]) -> bool),
    /// Once everything is committed and published, before the acknowledgement.
    BeforeAck,
}

/// What a holding was given to do.
enum Task {
    /// A turn of the family member `member`, or of the root Chat.
    Turn {
        member: Option<ChatId>,
        request: Box<TurnRequest>,
    },
    /// The scheduler's wake of the family for the time it was asked to come back at.
    Wake { due_at: Option<SystemTime> },
    /// The user's answer to a schedule the model created in a Loop of the family member
    /// `member`, or of the root Chat.
    DecideSchedule {
        member: Option<ChatId>,
        loop_id: LoopId,
        decision: ScheduleDecision,
    },
}

/// What one holding of the lease did with its task.
#[derive(Clone, Debug)]
pub struct TurnReport {
    pub epoch: u64,
    /// The outcome `advance` reached, if the holder got that far; a wake has none.
    pub outcome: Option<TurnOutcome>,
    pub halt: Option<Halt>,
    pub acknowledged: bool,
    pub harness_chat: Option<ChatId>,
    /// What the Chat waited on once quiet, if the holder got that far.
    pub waiting: Option<Waits>,
}

type HarnessBuilder = Arc<dyn Fn(Arc<dyn TranscriptStore>) -> Agent + Send + Sync>;

/// One host process: it builds a Harness around its holding's store for every turn.
pub struct ReferenceHost {
    holder: String,
    chats: Arc<DurableChats>,
    stream: Arc<MemoryEventStream>,
    clock: Arc<ManualClock>,
    config: HostConfig,
    build: HarnessBuilder,
}

impl ReferenceHost {
    /// `clock` is the host's own: its local deadlines run on it.
    pub fn new(
        holder: &str,
        chats: Arc<DurableChats>,
        stream: Arc<MemoryEventStream>,
        clock: Arc<ManualClock>,
        config: HostConfig,
        build: impl Fn(Arc<dyn TranscriptStore>) -> Agent + Send + Sync + 'static,
    ) -> Self {
        Self {
            holder: holder.into(),
            chats,
            stream,
            clock,
            config,
            build: Arc::new(build),
        }
    }

    /// Step 1: the lease and the local deadline.
    pub fn acquire(&self, chat: Uuid) -> Result<Holding, LeaseError> {
        let sent = self.clock.now();
        let lease = self
            .chats
            .acquire_lease(chat, &self.holder, self.config.lease_ttl)?;
        let shared = Arc::new(Shared {
            lease,
            chats: self.chats.clone(),
            stream: self.stream.clone(),
            clock: self.clock.clone(),
            state: Mutex::new(PublishState {
                deadline: deadline(sent, self.config),
                ..PublishState::default()
            }),
            wake: Notify::new(),
            progress: Notify::new(),
            halted: CancellationToken::new(),
        });
        let store = Arc::new(FaultyTranscriptStore::new(Arc::new(LeasedTranscripts {
            shared: shared.clone(),
        })));
        Ok(Holding {
            shared,
            store,
            config: self.config,
            build: self.build.clone(),
            top: Mutex::new(None),
        })
    }

    /// One delivery of a task: a holding that runs it.
    pub async fn deliver(
        &self,
        chat: Uuid,
        request: TurnRequest,
    ) -> Result<TurnReport, LeaseError> {
        Ok(self.acquire(chat)?.run(request).await)
    }

    /// One delivery of a task for the family member `member` of the Chat, such as the reply to
    /// a question a child asked: a holding of the Chat's lease that runs it.
    pub async fn deliver_to(
        &self,
        chat: Uuid,
        member: ChatId,
        request: TurnRequest,
    ) -> Result<TurnReport, LeaseError> {
        Ok(self.acquire(chat)?.run_on(Some(member), request).await)
    }
}

fn deadline(sent: Duration, config: HostConfig) -> Duration {
    (sent + config.lease_ttl).saturating_sub(config.lease_margin)
}

/// One holding of a Chat's lease.
pub struct Holding {
    shared: Arc<Shared>,
    store: Arc<FaultyTranscriptStore>,
    config: HostConfig,
    build: HarnessBuilder,
    /// The stream's top as activation returned it.
    top: Mutex<Option<u64>>,
}

impl Holding {
    pub fn lease(&self) -> &Lease {
        &self.shared.lease
    }

    /// The store the Harness writes through, to inject storage faults.
    pub fn store(&self) -> &Arc<FaultyTranscriptStore> {
        &self.store
    }

    /// The Harness events waiting for a commit that reflects them.
    pub fn held_events(&self) -> Vec<Event> {
        self.shared
            .state
            .lock()
            .unwrap()
            .holding
            .iter()
            .cloned()
            .collect()
    }

    pub fn crash_at(&self, point: CrashPoint) {
        self.shared.state.lock().unwrap().crash = Some(point);
    }

    /// Renews the lease; the local deadline then counts from this request.
    pub fn renew(&self) -> bool {
        let sent = self.shared.clock.now();
        let renewed = self
            .shared
            .chats
            .renew_lease(&self.shared.lease, self.config.lease_ttl);
        if renewed {
            self.shared.state.lock().unwrap().deadline = deadline(sent, self.config);
        }
        renewed
    }

    /// Step 2: from here on no older epoch publishes.
    pub fn activate(&self) -> Result<u64, StreamError> {
        let lease = &self.shared.lease;
        let top = self.shared.stream.activate(lease.chat, lease.epoch)?;
        *self.top.lock().unwrap() = Some(top);
        Ok(top)
    }

    /// Steps 2 to 8 for one task; a holding runs one.
    pub async fn run(&self, request: TurnRequest) -> TurnReport {
        self.run_on(None, request).await
    }

    /// Steps 2 to 8 for one task of the family member `member`, or of the root Chat.
    pub async fn run_on(&self, member: Option<ChatId>, request: TurnRequest) -> TurnReport {
        let input = request.input_id;
        self.hold(
            input,
            Task::Turn {
                member,
                request: Box::new(request),
            },
        )
        .await
    }

    /// Steps 2 to 8 for the scheduler's task `task`, which wakes the Chat's family for the time
    /// `due_at` its last acknowledgement asked to be woken at, or for a takeover without one.
    pub async fn wake(&self, task: Uuid, due_at: Option<SystemTime>) -> TurnReport {
        self.hold(task, Task::Wake { due_at }).await
    }

    /// Steps 2 to 8 for the user's task `task` that confirms or rejects the schedule the model
    /// created for the Loop `loop_id` of the family member `member`, or of the root Chat.
    pub async fn decide_schedule(
        &self,
        task: Uuid,
        member: Option<ChatId>,
        loop_id: LoopId,
        decision: ScheduleDecision,
    ) -> TurnReport {
        self.hold(
            task,
            Task::DecideSchedule {
                member,
                loop_id,
                decision,
            },
        )
        .await
    }

    async fn hold(&self, input: Uuid, task: Task) -> TurnReport {
        let lease = self.shared.lease.clone();
        let mut report = TurnReport {
            epoch: lease.epoch,
            outcome: None,
            halt: None,
            acknowledged: false,
            harness_chat: None,
            waiting: None,
        };
        if let Err(halt) = self.take_over() {
            report.halt = Some(halt);
            return report;
        }
        let agent = (self.build)(self.store.clone());
        let stop = CancellationToken::new();
        let mut pump = None;
        let reached = self
            .reach_outcome(&agent, task, &stop, &mut pump, &mut report)
            .await;
        agent.shutdown().await;
        let synced = match reached {
            Ok(outcome) => {
                report.outcome = outcome;
                match self.until_halted(agent.sync()).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(error)) => Err(self.agent_failure(error)),
                    Err(halt) => Err(halt),
                }
            }
            Err(halt) => Err(halt),
        };
        drop(agent);
        let drained = match synced {
            Ok(()) => {
                // `sync` is the durability barrier: every event before it is reflected.
                self.shared.seal_all();
                self.drain().await
            }
            Err(halt) => Err(halt),
        };
        let acknowledged = drained.and_then(|()| self.acknowledge(input, report.waiting.clone()));
        stop.cancel();
        if let Some(pump) = pump {
            pump.await.unwrap();
        }
        // A release reads as a clean handoff (N4), so only a holder that owes clients no
        // marker releases; the others leave the lease to expire. A crashed, fenced or late
        // holder cannot release at all.
        let owes_marker = self.shared.state.lock().unwrap().needs_resync;
        match acknowledged {
            Ok(()) => {
                report.acknowledged = true;
                self.shared.chats.release_lease(&lease);
            }
            Err(halt) => {
                if matches!(halt, Halt::DrainTimedOut | Halt::Failed(_)) && !owes_marker {
                    self.shared.chats.release_lease(&lease);
                }
                report.halt = Some(halt);
            }
        }
        report
    }

    /// Steps 2 to 4. A holder that fails here keeps the lease until it expires, so the next
    /// holder writes the marker this one may have owed.
    fn take_over(&self) -> Result<(), Halt> {
        let activated = *self.top.lock().unwrap();
        let top = match activated {
            Some(top) => top,
            None => self.activate().map_err(stream_halt)?,
        };
        let lease = &self.shared.lease;
        let last_seq = self.shared.chats.last_seq(lease.chat);
        let is_clean_handoff = lease.previous.is_none_or(|previous| previous.released);
        {
            let mut state = self.shared.state.lock().unwrap();
            state.next_seq = last_seq.max(top) + 1;
            state.needs_resync = !(is_clean_handoff && top >= last_seq);
        }
        // A marker that fails to publish stays owed; the publisher writes it again.
        match self.shared.publish_ready() {
            Publishing::Halted => Err(self.shared.stored_halt()),
            _ => Ok(()),
        }
    }

    async fn reach_outcome(
        &self,
        agent: &Agent,
        task: Task,
        stop: &CancellationToken,
        pump: &mut Option<JoinHandle<()>>,
        report: &mut TurnReport,
    ) -> Result<Option<TurnOutcome>, Halt> {
        let chat = match self.shared.chats.harness_chat(self.shared.lease.chat) {
            Some(chat) => chat,
            None => self
                .until_halted(agent.create_chat_without_folder())
                .await?
                .map_err(|error| self.agent_failure(error))?,
        };
        report.harness_chat = Some(chat);
        self.shared.state.lock().unwrap().harness_chat = Some(chat);
        self.shared
            .chats
            .bind_root(&self.shared.lease, chat)
            .map_err(|_| self.shared.halt_or(Halt::Fenced))?;
        *pump = Some(tokio::spawn(publish_events(
            self.shared.clone(),
            agent.subscribe(),
            stop.clone(),
            self.config.retry_delay,
        )));
        let (target, outcome) = match task {
            Task::Turn { member, request } => {
                let target = member.unwrap_or(chat);
                let turn = self
                    .until_halted(agent.advance(target, *request))
                    .await?
                    .map_err(|error| self.agent_failure(error))?;
                let outcome = self
                    .until_halted(turn.outcome())
                    .await?
                    .map_err(|error| self.agent_failure(error))?;
                if matches!(outcome, TurnOutcome::Fenced) {
                    return Err(self.shared.halt_or(Halt::Fenced));
                }
                (target, Some(outcome))
            }
            Task::Wake { due_at } => {
                self.until_halted(agent.wake(chat, due_at))
                    .await?
                    .map_err(|error| self.agent_failure(error))?;
                (chat, None)
            }
            Task::DecideSchedule {
                member,
                loop_id,
                decision,
            } => {
                let target = member.unwrap_or(chat);
                self.until_halted(agent.decide_schedule(target, loop_id, decision))
                    .await?
                    .map_err(|error| self.agent_failure(error))?;
                (target, None)
            }
        };
        if let Quiescence::Waiting(waits) = self.until_halted(agent.quiesce(target)).await? {
            report.waiting = Some(waits);
        }
        Ok(outcome)
    }

    /// Runs `work` until the holder halts or its local deadline passes.
    async fn until_halted<T>(&self, work: impl Future<Output = T>) -> Result<T, Halt> {
        let mut clock = self.shared.clock.changes();
        tokio::pin!(work);
        loop {
            self.shared.check_deadline()?;
            tokio::select! {
                biased;
                _ = self.shared.halted.cancelled() => return Err(self.shared.stored_halt()),
                output = &mut work => return Ok(output),
                _ = clock.changed() => {}
            }
        }
    }

    fn agent_failure(&self, error: AgentError) -> Halt {
        match error {
            AgentError::Fenced => self.shared.halt_or(Halt::Fenced),
            error => self.shared.halt_or(Halt::Failed(error.to_string())),
        }
    }

    /// Step 8: waits until every event is published and no marker is owed.
    async fn drain(&self) -> Result<(), Halt> {
        let give_up = tokio::time::Instant::now() + self.config.drain_timeout;
        self.shared.state.lock().unwrap().flush_requested = true;
        self.shared.wake.notify_one();
        loop {
            let progressed = self.shared.progress.notified();
            tokio::pin!(progressed);
            progressed.as_mut().enable();
            {
                let state = self.shared.state.lock().unwrap();
                if let Some(halt) = &state.halt {
                    return Err(halt.clone());
                }
                if state.is_drained() {
                    return Ok(());
                }
            }
            tokio::select! {
                _ = progressed => {}
                _ = self.shared.halted.cancelled() => {}
                _ = tokio::time::sleep_until(give_up) => {
                    return Err(self.shared.halt_or(Halt::DrainTimedOut));
                }
            }
        }
    }

    fn acknowledge(&self, input: Uuid, waiting: Option<Waits>) -> Result<(), Halt> {
        self.shared.check_deadline()?;
        if matches!(
            self.shared.state.lock().unwrap().crash,
            Some(CrashPoint::BeforeAck)
        ) {
            return Err(self.shared.halt_or(Halt::Crashed));
        }
        self.shared
            .chats
            .complete_task(&self.shared.lease, input, waiting)
            .map_err(|_| self.shared.halt_or(Halt::Fenced))
    }
}

fn stream_halt(error: StreamError) -> Halt {
    match error {
        StreamError::StaleEpoch { .. } => Halt::StaleEpoch,
        StreamError::NotActivated { .. } => Halt::NotActivated,
        error => Halt::Failed(format!("{error:?}")),
    }
}

/// What a holding's store adapter and its publisher share.
struct Shared {
    lease: Lease,
    chats: Arc<DurableChats>,
    stream: Arc<MemoryEventStream>,
    clock: Arc<ManualClock>,
    state: Mutex<PublishState>,
    /// Wakes the publisher.
    wake: Notify,
    /// The publisher received events or finished a publish.
    progress: Notify,
    halted: CancellationToken,
}

#[derive(Default)]
struct PublishState {
    deadline: Duration,
    harness_chat: Option<ChatId>,
    next_seq: u64,
    /// Every Harness event up to this sequence is reflected by a commit.
    covered_through: u64,
    /// Events waiting behind the first one no commit reflects yet.
    holding: VecDeque<Event>,
    /// Entries with seqs, in seq order, not yet published.
    slots: VecDeque<StreamEntry>,
    needs_resync: bool,
    /// The seq of the marker owed or being published.
    marker: Option<u64>,
    /// Entries left the slots and their publish has not returned.
    in_flight: bool,
    /// The broadcast dropped events; the next one received sets `barrier`.
    lagged: bool,
    /// Events up to this sequence may have been dropped unseen: nothing after them goes out
    /// before a commit reflects them.
    barrier: u64,
    flush_requested: bool,
    /// Every event the Harness broadcast has been received.
    events_drained: bool,
    crash: Option<CrashPoint>,
    halt: Option<Halt>,
}

enum Publishing {
    Published,
    Idle,
    Retry,
    Halted,
}

impl PublishState {
    fn allocate(&mut self, item: StreamItem) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.slots.push_back(StreamEntry { seq, item });
        seq
    }

    fn cover(&mut self, through: u64) {
        self.covered_through = self.covered_through.max(through);
        while let Some(front) = self.holding.front() {
            if !self.may_release(front) {
                break;
            }
            let event = self.holding.pop_front().unwrap();
            self.allocate(StreamItem::Event(Box::new(event)));
        }
    }

    /// Whether `event` may take a seq once everything before it has.
    fn may_release(&self, event: &Event) -> bool {
        self.covered_through >= self.barrier
            && (event.kind.is_transient() || event.sequence <= self.covered_through)
    }

    fn is_drained(&self) -> bool {
        self.events_drained
            && self.holding.is_empty()
            && self.slots.is_empty()
            && !self.in_flight
            && !self.needs_resync
    }
}

impl Shared {
    /// The halt of a holder whose `halted` token is cancelled: it is stored first.
    fn stored_halt(&self) -> Halt {
        self.state
            .lock()
            .unwrap()
            .halt
            .clone()
            .expect("a holder halts with its reason")
    }

    /// The holder's halt; `halt` when it had none yet.
    fn halt_or(&self, halt: Halt) -> Halt {
        let mut state = self.state.lock().unwrap();
        self.halt_locked(&mut state, halt)
    }

    fn halt_locked(&self, state: &mut PublishState, halt: Halt) -> Halt {
        let halt = state.halt.get_or_insert(halt).clone();
        self.halted.cancel();
        halt
    }

    fn check_deadline(&self) -> Result<(), Halt> {
        let mut state = self.state.lock().unwrap();
        self.admit(&mut state)
    }

    /// Whether the holder may still write or publish.
    fn admit(&self, state: &mut PublishState) -> Result<(), Halt> {
        if let Some(halt) = &state.halt {
            return Err(halt.clone());
        }
        if self.clock.now() >= state.deadline {
            return Err(self.halt_locked(state, Halt::DeadlinePassed));
        }
        Ok(())
    }

    fn receive(&self, event: Event) {
        let mut state = self.state.lock().unwrap();
        if state.halt.is_some() || state.harness_chat != Some(event.chat_id) {
            return;
        }
        if std::mem::take(&mut state.lagged) {
            state.barrier = state.barrier.max(event.sequence.saturating_sub(1));
        }
        if state.holding.is_empty() && state.may_release(&event) {
            state.allocate(StreamItem::Event(Box::new(event)));
        } else {
            state.holding.push_back(event);
        }
    }

    fn lagged(&self) {
        let mut state = self.state.lock().unwrap();
        state.needs_resync = true;
        state.lagged = true;
    }

    fn events_caught_up(&self, closed: bool) {
        let mut state = self.state.lock().unwrap();
        if closed || state.flush_requested {
            state.events_drained = true;
            self.progress.notify_waiters();
        }
    }

    fn seal_all(&self) {
        self.state.lock().unwrap().cover(u64::MAX);
        self.wake.notify_one();
    }

    fn check_chat(&self, chat: ChatId) -> Result<(), TranscriptError> {
        let mut state = self.state.lock().unwrap();
        self.admit(&mut state)
            .map_err(|_| TranscriptError::Fenced)?;
        self.chats
            .check_chat(&self.lease, chat)
            .inspect_err(|error| self.fenced_by(&mut state, error))
    }

    fn fenced_by(&self, state: &mut PublishState, error: &TranscriptError) {
        if matches!(error, TranscriptError::Fenced) {
            self.halt_locked(state, Halt::Fenced);
        }
    }

    /// Commits a batch with its seq, then releases what the commit reflects.
    fn commit(&self, chat: ChatId, batch: &AppendBatch) -> Result<AppendAck, TranscriptError> {
        let mut state = self.state.lock().unwrap();
        self.admit(&mut state)
            .map_err(|_| TranscriptError::Fenced)?;
        if let Some(CrashPoint::BeforeCommit(matches)) = state.crash
            && matches(&batch.lines)
        {
            self.halt_locked(&mut state, Halt::Crashed);
            return Err(TranscriptError::Fenced);
        }
        let seq = state.next_seq;
        let commit = self
            .chats
            .commit(&self.lease, chat, batch, seq)
            .inspect_err(|error| self.fenced_by(&mut state, error))?;
        // A member's Lines and events are its own: only the root Chat's go out on the stream.
        let is_root = state.harness_chat == Some(chat);
        if commit.is_new && !batch.lines.is_empty() && is_root {
            state.allocate(StreamItem::Committed {
                lines: batch.lines.clone(),
            });
        }
        if is_root {
            state.cover(batch.events_through);
        }
        if commit.is_new
            && let Some(CrashPoint::AfterCommit(matches)) = state.crash
            && matches(&batch.lines)
        {
            self.halt_locked(&mut state, Halt::Crashed);
            return Err(TranscriptError::Fenced);
        }
        drop(state);
        self.wake.notify_one();
        Ok(commit.ack)
    }

    /// Publishes every entry with a seq, after a resync marker when one is owed.
    fn publish_ready(&self) -> Publishing {
        let entries = {
            let mut state = self.state.lock().unwrap();
            let is_owed = state.needs_resync && state.marker.is_none();
            if !is_owed && state.slots.is_empty() {
                return if state.halt.is_some() {
                    Publishing::Halted
                } else {
                    Publishing::Idle
                };
            }
            if self.admit(&mut state).is_err() {
                return Publishing::Halted;
            }
            if is_owed {
                // What has not gone out cannot go out in order any more: clients reload it.
                // Committed entries are at or below `last_seq`, so the reload has them.
                state.slots.clear();
                let seq = state.next_seq;
                if self.chats.raise_last_seq(&self.lease, seq).is_err() {
                    self.halt_locked(&mut state, Halt::Fenced);
                    return Publishing::Halted;
                }
                state.marker = Some(state.allocate(StreamItem::Resync));
            }
            state.in_flight = true;
            state.slots.drain(..).collect::<Vec<_>>()
        };
        let published = self
            .stream
            .publish(self.lease.chat, self.lease.epoch, &entries);
        let mut state = self.state.lock().unwrap();
        state.in_flight = false;
        self.progress.notify_waiters();
        match published {
            Ok(()) => {
                if state
                    .marker
                    .is_some_and(|marker| entries.iter().any(|entry| entry.seq == marker))
                {
                    state.marker = None;
                    state.needs_resync = false;
                }
                Publishing::Published
            }
            Err(StreamError::Unavailable) => {
                state.needs_resync = true;
                // An owed marker keeps its seq, already within `last_seq`, and goes first
                // again; what followed it in the lost publish is in the reload or transient.
                let marker = state
                    .marker
                    .and_then(|marker| entries.iter().find(|entry| entry.seq == marker));
                match marker {
                    Some(marker) => state.slots.push_front(marker.clone()),
                    None => state.marker = None,
                }
                Publishing::Retry
            }
            Err(error) => {
                self.halt_locked(&mut state, stream_halt(error));
                Publishing::Halted
            }
        }
    }
}

/// The publisher of one holding: receives the Harness's events and publishes in seq order.
async fn publish_events(
    shared: Arc<Shared>,
    mut events: broadcast::Receiver<Event>,
    stop: CancellationToken,
    retry_delay: Duration,
) {
    let mut open = true;
    loop {
        while open {
            match events.try_recv() {
                Ok(event) => shared.receive(event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => shared.lagged(),
                Err(broadcast::error::TryRecvError::Empty) => {
                    shared.events_caught_up(false);
                    break;
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    shared.events_caught_up(true);
                    open = false;
                }
            }
        }
        let retry = loop {
            match shared.publish_ready() {
                Publishing::Published => {}
                Publishing::Idle => break false,
                Publishing::Retry => break true,
                Publishing::Halted => return,
            }
        };
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = shared.halted.cancelled() => return,
            _ = shared.wake.notified() => {}
            received = events.recv(), if open => match received {
                Ok(event) => shared.receive(event),
                Err(broadcast::error::RecvError::Lagged(_)) => shared.lagged(),
                Err(broadcast::error::RecvError::Closed) => {
                    shared.events_caught_up(true);
                    open = false;
                }
            },
            _ = tokio::time::sleep(retry_delay), if retry => {}
        }
    }
}

/// The Transcript store a holding gives its Harness: every write is fenced by the lease and
/// the local deadline, and commits with its seq.
struct LeasedTranscripts {
    shared: Arc<Shared>,
}

#[async_trait]
impl TranscriptStore for LeasedTranscripts {
    async fn register(
        &self,
        chat: ChatId,
        placement: &ChatPlacement,
    ) -> Result<StorePosition, TranscriptError> {
        self.shared.check_chat(chat)?;
        self.shared
            .chats
            .transcripts
            .register(chat, placement)
            .await
    }

    async fn load(
        &self,
        chat: ChatId,
        range: LoadRange,
    ) -> Result<Option<StoredTranscript>, TranscriptError> {
        self.shared.check_chat(chat)?;
        self.shared.chats.transcripts.load(chat, range).await
    }

    async fn append(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.shared.commit(chat, batch)
    }

    fn append_blocking(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.shared.commit(chat, batch)
    }
}

/// A client of the Chat: it loads the history, then follows the stream after its `last_seq`,
/// and reloads at every resync marker.
pub struct StreamFollower {
    chat: Uuid,
    cursor: u64,
    lines: Vec<Line>,
    events: Vec<Event>,
    reloads: usize,
}

impl StreamFollower {
    pub fn connect(chats: &DurableChats, chat: Uuid) -> Self {
        let history = chats.history(chat);
        Self {
            chat,
            cursor: history.last_seq,
            lines: history.lines,
            events: Vec::new(),
            reloads: 0,
        }
    }

    /// Reads everything after the cursor. Panics when a marker would make it reload forever.
    pub fn catch_up(&mut self, chats: &DurableChats, stream: &MemoryEventStream) {
        for _ in 0..FOLLOWER_ROUNDS {
            let entries = stream.read_after(self.chat, self.cursor);
            if entries.is_empty() {
                return;
            }
            for entry in entries {
                match entry.item {
                    StreamItem::Resync => {
                        let history = chats.history(self.chat);
                        assert!(
                            history.last_seq >= entry.seq,
                            "the resync marker at {} is above the history's last_seq {}",
                            entry.seq,
                            history.last_seq
                        );
                        self.cursor = history.last_seq;
                        self.lines = history.lines;
                        self.reloads += 1;
                        break;
                    }
                    StreamItem::Committed { lines } => self.lines.extend(lines),
                    StreamItem::Event(event) => self.events.push(*event),
                }
                self.cursor = entry.seq;
            }
        }
        panic!("the client keeps reloading the history");
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn reloads(&self) -> usize {
        self.reloads
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}
