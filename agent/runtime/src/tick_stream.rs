//! Observation stream between a Tickhost and the Agent's Loop scheduler.

mod hosted;
mod shell;
pub use hosted::HostedTickStream;
pub use shell::ShellTickStream;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{HostToolAuthority, ObservationKind};
use tokio::sync::{Notify, mpsc, watch};
use uuid::Uuid;

pub type WatchId = Uuid;
const STREAM_CAPACITY: usize = 128;
/// A watch connects to its Tick once and reconnects at most this often in an Agent process;
/// then it stays Disconnected until it is subscribed again, which makes a new watch.
pub const RECONNECTS_PER_WATCH: usize = 3;
/// The connections a watch makes at most: its first connect and every reconnect.
pub const CONNECTS_PER_WATCH: usize = 1 + RECONNECTS_PER_WATCH;
/// How long a watch waits after a failed or lost connection before it connects again.
pub const RECONNECT_PAUSE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TickObservation {
    pub instance: InstanceRef,
    pub sequence: u64,
    pub observed_at: String,
    /// Count of accepted observations superseded before this consumer received them.
    pub dropped: u64,
    pub data: serde_json::Value,
    pub kind: ObservationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recover_with: Option<String>,
    /// The source could not prove continuity since the preceding observation.
    pub gap: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TickConnectionState {
    Initializing,
    Live,
    Disconnected,
    Gone,
    IdentityUncertain,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TickMetadata {
    pub title: String,
    pub kind: ObservationKind,
    pub updated_at_ms: Option<u64>,
    /// The series an extension Tick declares in its state.
    pub series: Vec<tick_protocol::StateSeries>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TickStreamEvent {
    Metadata {
        instance: InstanceRef,
        audience: Vec<WatchId>,
        metadata: TickMetadata,
    },
    Ready {
        watch: WatchId,
    },
    Observation {
        audience: Vec<WatchId>,
        value: TickObservation,
    },
    State {
        instance: InstanceRef,
        state: TickConnectionState,
        audience: Vec<WatchId>,
    },
    Gap {
        instance: InstanceRef,
        audience: Vec<WatchId>,
    },
    /// A Terminal Tick's screen text changed since these watches last got it, so their next
    /// samples will read it; or, once not pending, a sample settled it.
    ScreenChange {
        instance: InstanceRef,
        audience: Vec<WatchId>,
        is_pending: bool,
    },
    /// A Terminal Tick's program exited: these watches sample its last screen now, without
    /// waiting for their period.
    ProgramExited {
        instance: InstanceRef,
        audience: Vec<WatchId>,
    },
}

pub struct TickStreamHandle {
    pub desired: watch::Sender<BTreeMap<WatchId, InstanceRef>>,
    pub events: mpsc::Receiver<TickStreamEvent>,
}

/// Revalidation of one prepared subscription admission, without executing its query body.
pub struct WatchHostAuthority {
    pub tool_name: String,
    pub authority: Option<HostToolAuthority>,
}

#[async_trait::async_trait]
pub trait TickWatchValidator: Send + Sync {
    async fn revalidate(
        &self,
        chat: Uuid,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String>;
    fn host_authority(&self) -> Result<Option<WatchHostAuthority>, String> {
        Ok(None)
    }
}

struct HostWatchValidator {
    provider: Arc<dyn crate::host_tools::HostToolProvider>,
    query: crate::host_tools::PreparedHostCall,
    authority: Option<HostToolAuthority>,
}
#[async_trait::async_trait]
impl TickWatchValidator for HostWatchValidator {
    async fn revalidate(
        &self,
        chat: Uuid,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        self.provider
            .revalidate_call(chat, &self.query, cancel)
            .await
    }
    fn host_authority(&self) -> Result<Option<WatchHostAuthority>, String> {
        Ok(Some(WatchHostAuthority {
            tool_name: self.query.policy_name().to_owned(),
            authority: self.authority.clone(),
        }))
    }
}

pub struct PreparedTickWatch {
    admission: crate::host_tools::PreparedHostCall,
    validator: Arc<dyn TickWatchValidator>,
}
impl PreparedTickWatch {
    pub fn new(
        provider: Arc<dyn crate::host_tools::HostToolProvider>,
        query: crate::host_tools::PreparedHostCall,
    ) -> Result<Self, String> {
        let authority = provider.call_authority(query.policy_name(), query.source_input())?;
        let admission = crate::host_tools::PreparedHostCall::new(
            query.policy_name().into(),
            query.policy_input().clone(),
            query.policy_input().clone(),
            query.permission(),
            query.approval_summary().into(),
        )
        .with_permission_input(query.permission_input().clone());
        Ok(Self {
            admission,
            validator: Arc::new(HostWatchValidator {
                provider,
                query,
                authority,
            }),
        })
    }
    pub fn with_validator(
        admission: crate::host_tools::PreparedHostCall,
        validator: Arc<dyn TickWatchValidator>,
    ) -> Self {
        Self {
            admission,
            validator,
        }
    }
    pub(crate) fn host_authority(&self) -> Result<Option<WatchHostAuthority>, String> {
        self.validator.host_authority()
    }
    pub(crate) fn into_admission(
        self,
        input: serde_json::Value,
    ) -> crate::host_tools::PreparedHostCall {
        crate::host_tools::PreparedHostCall::new(
            "SubscribeTick".into(),
            input.clone(),
            input,
            self.admission.permission(),
            self.admission.approval_summary().into(),
        )
        .with_policy_identity(
            self.admission.policy_name().into(),
            self.admission.policy_input().clone(),
        )
        .with_permission_input(self.admission.permission_input().clone())
        .with_payload(self)
    }
    pub(crate) async fn revalidate(
        chat: Uuid,
        call: &crate::host_tools::PreparedHostCall,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        call.payload::<Self>()
            .ok_or("Tick watch lost its prepared admission")?
            .validator
            .revalidate(chat, cancel)
            .await
    }
}

#[async_trait::async_trait]
pub trait TickStreamPort: Send + Sync + 'static {
    fn open(&self) -> TickStreamHandle;
    /// Caller identities for active watches, including restored subscriptions.
    fn set_watch_chats(&self, _chats: BTreeMap<WatchId, Uuid>) {}
    /// Prepare subscription admission with the native query policy and stable target ownership.
    fn set_watch_authorities(&self, _authorities: BTreeMap<WatchId, HostToolAuthority>);
    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String>;
    async fn prepare_watch(
        &self,
        _chat: Uuid,
        _instance: &InstanceRef,
        _input: &serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<PreparedTickWatch>, String> {
        Ok(None)
    }
    /// The instance's current content, read again for a delivery whose update data the
    /// content cache evicted, after its watch was revalidated. `None` when this kind of Tick
    /// cannot be read again; such a delivery goes out without the data.
    async fn read_current(
        &self,
        _chat: Uuid,
        _instance: &InstanceRef,
    ) -> Result<Option<TickObservation>, String> {
        Ok(None)
    }
    /// A Terminal Tick's screen, read for `watch` of `instance` because its subscription's
    /// period elapsed or its program exited: the observation when its text, or whether its
    /// program exited, differs from what this watch last got. `None` when nothing changed, and
    /// for the Ticks whose updates the stream pushes. A sample settles the watch's
    /// [`TickStreamEvent::ScreenChange`]: the stream reports it no longer pending unless the
    /// screen changed again after it was read.
    async fn sample(&self, _watch: WatchId, _instance: &InstanceRef) -> Option<TickObservation> {
        None
    }
}

/// What an extension Tick that declares no recovery query tells the Agent instead.
const NO_RECOVERY: &str =
    "none: dropped Tick Updates cannot be recovered; ReadTick returns only the current state";

/// An extension Tick's current content as an observation, for a delivery read again.
fn current_observation(instance: &InstanceRef, read: tick_protocol::TickState) -> TickObservation {
    TickObservation {
        instance: instance.clone(),
        sequence: read.seq,
        observed_at: read.at.unwrap_or_default(),
        dropped: 0,
        data: read.data,
        kind: ObservationKind::Protocol,
        recover_with: None,
        gap: false,
    }
}

/// A finite latest-only mailbox. Producers never wait for a slow Agent: each watch keeps one
/// accepted observation, while state and baseline notifications have independent slots.
#[derive(Clone)]
pub struct EventPublisher {
    state: Arc<Mutex<EventMailbox>>,
    wake: Arc<Notify>,
}

#[derive(Default)]
struct EventMailbox {
    ready: BTreeSet<WatchId>,
    gaps: BTreeMap<WatchId, InstanceRef>,
    states: BTreeMap<WatchId, (InstanceRef, TickConnectionState)>,
    metadata: BTreeMap<WatchId, (InstanceRef, TickMetadata)>,
    latest: BTreeMap<WatchId, TickObservation>,
    /// Whether each watch's Terminal Tick screen changed since it last got it, latest only.
    screen_changes: BTreeMap<WatchId, (InstanceRef, bool)>,
    exits: BTreeMap<WatchId, InstanceRef>,
    terminal: BTreeSet<WatchId>,
}

impl EventPublisher {
    pub fn publish(&self, event: TickStreamEvent) {
        match event {
            TickStreamEvent::Metadata {
                instance,
                audience,
                metadata,
            } => self.metadata(instance, audience, metadata),
            TickStreamEvent::Ready { watch } => self.ready(watch),
            TickStreamEvent::Observation { audience, value } => self.observation(&audience, value),
            TickStreamEvent::State {
                instance,
                state,
                audience,
            } => self.state_to(instance, state, audience),
            TickStreamEvent::Gap { instance, audience } => self.gap_to(instance, audience),
            TickStreamEvent::ScreenChange {
                instance,
                audience,
                is_pending,
            } => self.screen_change(instance, audience, is_pending),
            TickStreamEvent::ProgramExited { instance, audience } => {
                self.program_exited(instance, audience)
            }
        }
    }
    pub fn channel() -> (Self, mpsc::Receiver<TickStreamEvent>) {
        let publisher = Self {
            state: Arc::new(Mutex::new(EventMailbox::default())),
            wake: Arc::new(Notify::new()),
        };
        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let draining = publisher.clone();
        tokio::spawn(async move {
            loop {
                let event = {
                    let mut mailbox = draining
                        .state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if let Some(watch) = mailbox.ready.pop_first() {
                        Some(TickStreamEvent::Ready { watch })
                    } else if let Some((watch, instance)) = mailbox.gaps.pop_first() {
                        Some(TickStreamEvent::Gap {
                            instance,
                            audience: vec![watch],
                        })
                    } else if let Some((watch, (instance, state))) = mailbox.states.pop_first() {
                        Some(TickStreamEvent::State {
                            instance,
                            state,
                            audience: vec![watch],
                        })
                    } else if let Some((watch, (instance, metadata))) = mailbox.metadata.pop_first()
                    {
                        Some(TickStreamEvent::Metadata {
                            instance,
                            metadata,
                            audience: vec![watch],
                        })
                    } else if let Some((watch, instance)) = mailbox.exits.pop_first() {
                        Some(TickStreamEvent::ProgramExited {
                            instance,
                            audience: vec![watch],
                        })
                    } else if let Some((watch, value)) = mailbox.latest.pop_first() {
                        Some(TickStreamEvent::Observation {
                            audience: vec![watch],
                            value,
                        })
                    } else {
                        mailbox
                            .screen_changes
                            .pop_first()
                            .map(
                                |(watch, (instance, is_pending))| TickStreamEvent::ScreenChange {
                                    instance,
                                    audience: vec![watch],
                                    is_pending,
                                },
                            )
                    }
                };
                match event {
                    Some(event) => {
                        if sender.send(event).await.is_err() {
                            return;
                        }
                    }
                    None => {
                        tokio::select! {
                            _ = draining.wake.notified() => {}
                            _ = sender.closed() => return,
                        }
                    }
                }
            }
        });
        (publisher, receiver)
    }

    pub fn metadata(
        &self,
        instance: InstanceRef,
        audience: impl IntoIterator<Item = WatchId>,
        metadata: TickMetadata,
    ) {
        let mut mailbox = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for watch in audience {
            mailbox
                .metadata
                .insert(watch, (instance.clone(), metadata.clone()));
        }
        self.wake.notify_one();
    }

    pub fn ready(&self, watch: WatchId) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ready
            .insert(watch);
        self.wake.notify_one();
    }

    pub fn state_to(
        &self,
        instance: InstanceRef,
        state: TickConnectionState,
        audience: impl IntoIterator<Item = WatchId>,
    ) {
        let mut mailbox = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for watch in audience {
            mailbox.states.insert(watch, (instance.clone(), state));
            if matches!(
                state,
                TickConnectionState::Gone | TickConnectionState::IdentityUncertain
            ) {
                mailbox.terminal.insert(watch);
                mailbox.ready.remove(&watch);
            }
        }
        self.wake.notify_one();
    }

    pub fn gap_to(&self, instance: InstanceRef, audience: impl IntoIterator<Item = WatchId>) {
        let mut mailbox = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for watch in audience {
            mailbox.gaps.insert(watch, instance.clone());
        }
        self.wake.notify_one();
    }

    pub fn screen_change(
        &self,
        instance: InstanceRef,
        audience: impl IntoIterator<Item = WatchId>,
        is_pending: bool,
    ) {
        let mut mailbox = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for watch in audience {
            mailbox
                .screen_changes
                .insert(watch, (instance.clone(), is_pending));
        }
        self.wake.notify_one();
    }

    pub fn program_exited(
        &self,
        instance: InstanceRef,
        audience: impl IntoIterator<Item = WatchId>,
    ) {
        let mut mailbox = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for watch in audience {
            mailbox.exits.insert(watch, instance.clone());
        }
        self.wake.notify_one();
    }

    pub fn observation(&self, audience: &[WatchId], value: TickObservation) {
        let mut mailbox = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for watch in audience {
            let mut next = value.clone();
            if let Some(previous) = mailbox.latest.remove(watch) {
                next.dropped = next
                    .dropped
                    .saturating_add(previous.dropped)
                    .saturating_add(1);
                next.gap |= previous.gap;
            }
            mailbox.latest.insert(*watch, next);
        }
        drop(mailbox);
        self.wake.notify_one();
    }

    pub fn retain(&self, desired: &BTreeMap<WatchId, InstanceRef>) {
        let mut mailbox = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        mailbox
            .metadata
            .retain(|watch, _| desired.contains_key(watch));
        mailbox.ready.retain(|watch| desired.contains_key(watch));
        mailbox.gaps.retain(|watch, _| desired.contains_key(watch));
        mailbox
            .latest
            .retain(|watch, _| desired.contains_key(watch));
        mailbox
            .states
            .retain(|watch, _| desired.contains_key(watch));
        mailbox
            .screen_changes
            .retain(|watch, _| desired.contains_key(watch));
        mailbox.exits.retain(|watch, _| desired.contains_key(watch));
        mailbox.terminal.retain(|watch| desired.contains_key(watch));
    }

    fn terminal_watches(&self, watches: &BTreeSet<WatchId>) -> BTreeSet<WatchId> {
        let mailbox = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        watches.intersection(&mailbox.terminal).copied().collect()
    }
}

#[derive(Clone, Default)]
pub struct InMemoryTickStream {
    shared: Arc<Mutex<InMemoryState>>,
}

#[derive(Default)]
struct InMemoryState {
    desired: Option<watch::Receiver<BTreeMap<WatchId, InstanceRef>>>,
    events: Option<EventPublisher>,
    auto_ready: bool,
    /// What the next sample of a watch of each instance finds changed.
    samples: BTreeMap<InstanceRef, TickObservation>,
}

impl InMemoryTickStream {
    pub fn new() -> Self {
        let stream = Self::default();
        stream.state().auto_ready = true;
        stream
    }

    pub fn manual_ready() -> Self {
        Self::default()
    }

    pub fn watched(&self) -> BTreeSet<InstanceRef> {
        self.state()
            .desired
            .as_ref()
            .map(|desired| desired.borrow().values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn watches(&self) -> BTreeMap<WatchId, InstanceRef> {
        self.state()
            .desired
            .as_ref()
            .map(|desired| desired.borrow().clone())
            .unwrap_or_default()
    }

    pub fn is_open(&self) -> bool {
        self.state().events.is_some()
    }

    pub fn push(&self, value: TickObservation) {
        let audience: Vec<_> = self
            .watches()
            .into_iter()
            .filter_map(|(watch, instance)| (instance == value.instance).then_some(watch))
            .collect();
        self.emit(TickStreamEvent::Observation { audience, value });
    }

    /// Makes the next sample of a watch of `value`'s instance find `value` changed.
    pub fn change_on_next_sample(&self, value: TickObservation) {
        self.state().samples.insert(value.instance.clone(), value);
    }

    pub fn emit(&self, event: TickStreamEvent) {
        if let Some(events) = &self.state().events {
            events.publish(event);
        }
    }

    fn state(&self) -> MutexGuard<'_, InMemoryState> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl TickStreamPort for InMemoryTickStream {
    fn set_watch_authorities(&self, _authorities: BTreeMap<WatchId, HostToolAuthority>) {}
    /// Like a hosted Terminal Tick, a sample settles the watch's pending screen change.
    async fn sample(&self, watch: WatchId, instance: &InstanceRef) -> Option<TickObservation> {
        if self.watches().get(&watch) != Some(instance) {
            return None;
        }
        let mut state = self.state();
        if let Some(events) = &state.events {
            events.screen_change(instance.clone(), [watch], false);
        }
        state.samples.remove(instance)
    }
    async fn revalidate_watch(
        &self,
        _chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        if authority.is_some() || !self.watched().contains(instance) {
            return Err("In-memory watch is outside its configured local scope".into());
        }
        Ok(())
    }
    fn open(&self) -> TickStreamHandle {
        let (desired, watched) = watch::channel(BTreeMap::new());
        let (sender, events) = EventPublisher::channel();
        let mut state = self.state();
        if state.auto_ready {
            let mut ready_watches = watched.clone();
            let ready_sender = sender.clone();
            tokio::spawn(async move {
                let mut seen = BTreeSet::new();
                loop {
                    let current = ready_watches.borrow_and_update().clone();
                    for watch in current.keys().filter(|watch| !seen.contains(*watch)) {
                        ready_sender.ready(*watch);
                    }
                    seen = current.keys().copied().collect();
                    if ready_watches.changed().await.is_err() {
                        return;
                    }
                }
            });
        }
        state.desired = Some(watched);
        state.events = Some(sender);
        TickStreamHandle { desired, events }
    }
}

/// Decides which requested watches may still observe an instance; each refused one is told
/// its identity is uncertain.
trait WatchAdmission: Send + Sync {
    fn admitted(
        &self,
        requested: &BTreeSet<WatchId>,
        instance: &InstanceRef,
        publisher: &EventPublisher,
    ) -> BTreeSet<WatchId>;
}

/// Where a stream's watched instances run: one observer per instance follows it for the
/// watches that want it.
#[async_trait::async_trait]
trait InstanceObserver: WatchAdmission + Clone + 'static {
    /// The connection attempts of this stream's watches.
    fn budget(&self) -> &ConnectionBudget;

    /// Waits until the admitted watch authorities change; false once they never will again.
    async fn authorities_changed(&mut self) -> bool;

    async fn observe(
        self,
        instance: InstanceRef,
        wanted: watch::Receiver<BTreeSet<WatchId>>,
        publisher: EventPublisher,
    );
}

/// The connection attempts each watch of a stream made in this Agent process.
#[derive(Clone, Default)]
struct ConnectionBudget {
    attempts: Arc<Mutex<BTreeMap<WatchId, Attempts>>>,
}

#[derive(Clone, Copy)]
enum Attempts {
    /// Attempts so far: the first connects, the others reconnect.
    Made(usize),
    /// The watch was refused another attempt.
    Spent,
}

impl ConnectionBudget {
    /// Takes one connection attempt for each of `watches` that has one left; a watch without
    /// one is told it is Disconnected and stays so.
    fn take(
        &self,
        watches: &BTreeSet<WatchId>,
        instance: &InstanceRef,
        publisher: &EventPublisher,
    ) -> BTreeSet<WatchId> {
        let mut attempts = self.attempts.lock().unwrap_or_else(PoisonError::into_inner);
        watches
            .iter()
            .copied()
            .filter(|watch| {
                let attempt = attempts.entry(*watch).or_insert(Attempts::Made(0));
                match attempt {
                    Attempts::Made(made) if *made < CONNECTS_PER_WATCH => {
                        *made += 1;
                        true
                    }
                    _ => {
                        *attempt = Attempts::Spent;
                        publisher.state_to(
                            instance.clone(),
                            TickConnectionState::Disconnected,
                            [*watch],
                        );
                        false
                    }
                }
            })
            .collect()
    }

    /// Admits `watch` while it may still follow its Tick; a watch refused another attempt is
    /// told instead that it stays Disconnected.
    fn admit_or_disconnect(
        &self,
        watch: WatchId,
        instance: &InstanceRef,
        publisher: &EventPublisher,
    ) -> bool {
        let attempts = self.attempts.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(attempts.get(&watch), Some(Attempts::Spent)) {
            publisher.state_to(instance.clone(), TickConnectionState::Disconnected, [watch]);
            return false;
        }
        true
    }

    /// Forgets the attempts of watches no longer wanted.
    fn retain(&self, desired: &BTreeMap<WatchId, InstanceRef>) {
        self.attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|watch, _| desired.contains_key(watch));
    }
}

/// An extension Tick as a watch sees it: its declaration, which names it and its recovery tool,
/// and a registration that yields the current content and then latest-only updates. A failure
/// keeps the host's code, which tells an ended Tick from a lost connection.
#[async_trait::async_trait]
trait ProtocolFeed: Send + Sync {
    async fn descriptor(
        &self,
    ) -> Result<tick_protocol::TickDescriptor, tickhost_connect::host_error::HostError>;

    async fn watch(
        &self,
    ) -> Result<
        (
            tick_protocol::TickState,
            u64,
            watch::Receiver<tickhost_connect::session::SessionObservation>,
        ),
        tickhost_connect::host_error::HostError,
    >;
}

/// Keeps one observer per watched instance, retiring watches that reached a terminal state.
async fn follow_instances<O: InstanceObserver>(
    mut scope: O,
    mut desired: watch::Receiver<BTreeMap<WatchId, InstanceRef>>,
    publisher: EventPublisher,
) {
    struct Worker {
        sender: watch::Sender<BTreeSet<WatchId>>,
        watches: BTreeSet<WatchId>,
        generation: Uuid,
    }

    let mut workers: BTreeMap<InstanceRef, Worker> = BTreeMap::new();
    let mut retired: BTreeMap<InstanceRef, BTreeSet<WatchId>> = BTreeMap::new();
    let (finished_sender, mut finished) = mpsc::unbounded_channel::<(InstanceRef, Uuid)>();
    loop {
        let wanted = desired.borrow_and_update().clone();
        for (instance, watches) in &mut retired {
            watches.retain(|watch| wanted.get(watch) == Some(instance));
        }
        retired.retain(|_, watches| !watches.is_empty());
        let mut completed = Vec::new();
        for (instance, worker) in &workers {
            if worker.sender.is_closed() {
                retired
                    .entry(instance.clone())
                    .or_default()
                    .extend(publisher.terminal_watches(&worker.watches));
                completed.push(instance.clone());
            }
        }
        for instance in completed {
            workers.remove(&instance);
        }
        let eligible: BTreeMap<_, _> = wanted
            .iter()
            .filter(|(watch, instance)| {
                !retired
                    .get(instance)
                    .is_some_and(|watches| watches.contains(*watch))
            })
            .map(|(watch, instance)| (*watch, instance.clone()))
            .collect();
        publisher.retain(&wanted);
        scope.budget().retain(&wanted);
        let mut groups: BTreeMap<InstanceRef, BTreeSet<WatchId>> = BTreeMap::new();
        for (watch, instance) in eligible {
            let admitted = scope.admitted(&BTreeSet::from([watch]), &instance, &publisher);
            if !admitted.is_empty() {
                groups.entry(instance).or_default().extend(admitted);
            }
        }
        workers.retain(|instance, worker| {
            if let Some(watches) = groups.get(instance) {
                if watches.is_disjoint(&worker.watches) {
                    worker.sender.send_replace(BTreeSet::new());
                    return false;
                }
                worker.watches.extend(watches);
                worker.sender.send_replace(watches.clone());
                true
            } else {
                worker.sender.send_replace(BTreeSet::new());
                false
            }
        });
        for (instance, watches) in groups {
            if workers.contains_key(&instance) {
                continue;
            }
            let (sender, receiver) = watch::channel(watches.clone());
            let generation = Uuid::new_v4();
            workers.insert(
                instance.clone(),
                Worker {
                    sender,
                    watches,
                    generation,
                },
            );
            let scope = scope.clone();
            let publisher = publisher.clone();
            let finished_sender = finished_sender.clone();
            tokio::spawn(async move {
                scope.observe(instance.clone(), receiver, publisher).await;
                let _ = finished_sender.send((instance, generation));
            });
        }
        tokio::select! {
            changed = desired.changed() => if changed.is_err() { return; },
            changed = scope.authorities_changed() => if !changed { return; },
            Some((instance, generation)) = finished.recv() => {
                if workers.get(&instance).is_some_and(|worker| worker.generation == generation) {
                    let worker = workers.remove(&instance).expect("the worker was present");
                    retired
                        .entry(instance)
                        .or_default()
                        .extend(publisher.terminal_watches(&worker.watches));
                }
            }
        }
    }
}

/// The code of a failure that only lost the connection to a Tick.
const DISCONNECTED_CODE: &str = "Disconnected";

fn connection_state(code: &str) -> TickConnectionState {
    match code {
        "TargetGone" => TickConnectionState::Gone,
        "ProtocolViolation" | "IncompatibleHost" => TickConnectionState::IdentityUncertain,
        _ => TickConnectionState::Disconnected,
    }
}

/// Follows an extension Tick for the wanted watches: a baseline for each, then latest-only
/// updates. Returns whether following ended for good rather than for a reconnect.
async fn observe_protocol(
    feed: &dyn ProtocolFeed,
    instance: &InstanceRef,
    wanted: &mut watch::Receiver<BTreeSet<WatchId>>,
    publisher: &EventPublisher,
    ready_watches: &mut BTreeSet<WatchId>,
    scope: &dyn WatchAdmission,
) -> bool {
    use tickhost_connect::session::SessionLinkState;

    let attempted = scope.admitted(&wanted.borrow_and_update().clone(), instance, publisher);
    if attempted.is_empty() {
        return true;
    }
    let descriptor = match feed.descriptor().await {
        Ok(descriptor) => descriptor,
        Err(error) => return publish_failure(&error, instance, publisher, &attempted),
    };
    if scope.admitted(&attempted, instance, publisher).is_empty() {
        return true;
    }
    let (baseline, registration_received, mut updates) = match feed.watch().await {
        Ok(watch) => watch,
        Err(error) => return publish_failure(&error, instance, publisher, &attempted),
    };
    if scope.admitted(&attempted, instance, publisher).is_empty() {
        return true;
    }
    let mut cursors: BTreeMap<WatchId, ProtocolCursor> = BTreeMap::new();
    let initial = updates.borrow_and_update().clone();
    publisher.metadata(
        instance.clone(),
        scope.admitted(&wanted.borrow().clone(), instance, publisher),
        TickMetadata {
            title: descriptor.title,
            kind: ObservationKind::Protocol,
            updated_at_ms: None,
            series: descriptor
                .state
                .map(|state| state.series)
                .unwrap_or_default(),
        },
    );
    let recovery = descriptor
        .recover_with
        .unwrap_or_else(|| NO_RECOVERY.to_owned());
    let mut latest_sequence = baseline.seq;
    let baseline_targets = scope.admitted(&wanted.borrow_and_update().clone(), instance, publisher);
    for watch in baseline_targets.iter().copied() {
        cursors.insert(
            watch,
            ProtocolCursor {
                received: registration_received,
                sequence: baseline.seq,
            },
        );
        publisher.ready(watch);
        ready_watches.insert(watch);
    }
    publisher.state_to(
        instance.clone(),
        TickConnectionState::Live,
        baseline_targets.iter().copied(),
    );
    let initial_targets = scope.admitted(&wanted.borrow().clone(), instance, publisher);
    if let Some(sequence) = publish_protocol_update(
        instance,
        &initial,
        &initial_targets,
        &mut cursors,
        &recovery,
        publisher,
    ) {
        latest_sequence = sequence;
    }
    match initial.state {
        SessionLinkState::Live => {}
        SessionLinkState::Gone => {
            publisher.state_to(
                instance.clone(),
                TickConnectionState::Gone,
                cursors.keys().copied(),
            );
            return true;
        }
        SessionLinkState::Disconnected => {
            let watches = cursors.keys().copied().collect();
            return settle_disconnect(feed, instance, publisher, &watches).await;
        }
        SessionLinkState::ProtocolError(_) => {
            publisher.state_to(
                instance.clone(),
                TickConnectionState::IdentityUncertain,
                cursors.keys().copied(),
            );
            return true;
        }
    }
    loop {
        tokio::select! {
            changed = updates.changed() => if changed.is_err() {
                let watches = cursors.keys().copied().collect();
                return settle_disconnect(feed, instance, publisher, &watches).await;
            },
            changed = wanted.changed() => if changed.is_err() { return true; },
        }
        let observation = updates.borrow_and_update().clone();
        let targets = scope.admitted(&wanted.borrow_and_update().clone(), instance, publisher);
        if targets.is_empty() {
            return true;
        }
        match observation.state {
            SessionLinkState::Live => {}
            SessionLinkState::Gone => {
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::Gone,
                    cursors.keys().copied(),
                );
                return true;
            }
            SessionLinkState::Disconnected => {
                let watches = cursors.keys().copied().collect();
                return settle_disconnect(feed, instance, publisher, &watches).await;
            }
            SessionLinkState::ProtocolError(_) => {
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::IdentityUncertain,
                    cursors.keys().copied(),
                );
                return true;
            }
        }
        if let Some(sequence) = publish_protocol_update(
            instance,
            &observation,
            &targets,
            &mut cursors,
            &recovery,
            publisher,
        ) {
            latest_sequence = sequence;
        }
        cursors.retain(|watch, _| targets.contains(watch));
        for watch in targets {
            if let std::collections::btree_map::Entry::Vacant(entry) = cursors.entry(watch) {
                entry.insert(ProtocolCursor {
                    received: observation.received,
                    sequence: latest_sequence,
                });
                publisher.ready(watch);
                ready_watches.insert(watch);
            }
        }
    }
}

/// Publishes what a failed `describe` or `watch` says of the Tick to `watches`; whether that
/// ends observing it, as a Tick that ended or whose identity is uncertain does.
fn publish_failure(
    error: &tickhost_connect::host_error::HostError,
    instance: &InstanceRef,
    publisher: &EventPublisher,
    watches: &BTreeSet<WatchId>,
) -> bool {
    let state = connection_state(&error.code);
    publisher.state_to(instance.clone(), state, watches.iter().copied());
    state != TickConnectionState::Disconnected
}

/// Settles a watch whose stream ended without telling how its Tick ended, as when its host
/// closed it right as the Tick went: the host is asked at once, and still says so for an ended
/// Tick, rather than the watch reconnecting after [`RECONNECT_PAUSE`] only to learn it then.
/// Whether that ends observing the Tick.
async fn settle_disconnect(
    feed: &dyn ProtocolFeed,
    instance: &InstanceRef,
    publisher: &EventPublisher,
    watches: &BTreeSet<WatchId>,
) -> bool {
    match feed.descriptor().await {
        Ok(_) => publish_failure(
            &tickhost_connect::host_error::HostError::after(
                DISCONNECTED_CODE,
                "the watch ended while the Tick runs",
            ),
            instance,
            publisher,
            watches,
        ),
        Err(error) => publish_failure(&error, instance, publisher, watches),
    }
}

#[derive(Clone, Copy)]
struct ProtocolCursor {
    received: u64,
    sequence: u64,
}

fn publish_protocol_update(
    instance: &InstanceRef,
    observation: &tickhost_connect::session::SessionObservation,
    targets: &BTreeSet<WatchId>,
    cursors: &mut BTreeMap<WatchId, ProtocolCursor>,
    recovery: &str,
    publisher: &EventPublisher,
) -> Option<u64> {
    let update = observation.update.as_ref()?;
    let mut groups: BTreeMap<(u64, bool), Vec<WatchId>> = BTreeMap::new();
    for (watch, cursor) in cursors {
        if !targets.contains(watch) || observation.received <= cursor.received {
            continue;
        }
        let accepted = observation.received - cursor.received;
        let dropped = accepted - 1;
        let source_gap = update.seq.saturating_sub(cursor.sequence) > accepted;
        groups
            .entry((dropped, source_gap))
            .or_default()
            .push(*watch);
        cursor.received = observation.received;
        cursor.sequence = update.seq;
    }
    for ((dropped, gap), audience) in groups {
        publisher.observation(
            &audience,
            TickObservation {
                instance: instance.clone(),
                sequence: update.seq,
                observed_at: update.at.clone().unwrap_or_default(),
                dropped,
                data: update.data.clone(),
                kind: ObservationKind::Protocol,
                recover_with: Some(recovery.to_owned()),
                gap,
            },
        );
    }
    Some(update.seq)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tickhost_connect::host_error::HostError;
    use tickhost_connect::session::{SessionLinkState, SessionObservation};

    use super::*;

    /// An extension Tick whose watch stream ends at once without telling how, and whose host
    /// answers `describe` until it has answered `answers` times, then says the Tick ended.
    struct EndingFeed {
        answers: usize,
        described: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ProtocolFeed for EndingFeed {
        async fn descriptor(&self) -> Result<tick_protocol::TickDescriptor, HostError> {
            if self.described.fetch_add(1, Ordering::SeqCst) >= self.answers {
                return Err(HostError::before("TargetGone", "the Tick has ended"));
            }
            Ok(tick_protocol::TickDescriptor {
                protocol: tick_protocol::NATIVE_PROTOCOL_VERSION,
                kind: "example.ending".into(),
                title: "Ending".into(),
                description: "Ends".into(),
                revision: "1".into(),
                tools: Vec::new(),
                recover_with: None,
                state: None,
            })
        }

        async fn watch(
            &self,
        ) -> Result<
            (
                tick_protocol::TickState,
                u64,
                watch::Receiver<SessionObservation>,
            ),
            HostError,
        > {
            let (_ended, observations) = watch::channel(SessionObservation {
                update: None,
                received: 0,
                state: SessionLinkState::Live,
            });
            Ok((tick_protocol::TickState::empty(), 0, observations))
        }
    }

    struct AdmitsAll;

    impl WatchAdmission for AdmitsAll {
        fn admitted(
            &self,
            requested: &BTreeSet<WatchId>,
            _instance: &InstanceRef,
            _publisher: &EventPublisher,
        ) -> BTreeSet<WatchId> {
            requested.clone()
        }
    }

    /// Whether observing an [`EndingFeed`] that answers `answers` times ended the watch, and the
    /// last connection state it published.
    async fn observe_ending(answers: usize) -> (bool, TickConnectionState) {
        let (publisher, mut events) = EventPublisher::channel();
        let instance = InstanceRef::hosted(Uuid::new_v4());
        let watch = Uuid::new_v4();
        let (_wanted_sender, mut wanted) = watch::channel(BTreeSet::from([watch]));
        let feed = EndingFeed {
            answers,
            described: AtomicUsize::new(0),
        };
        let ended = observe_protocol(
            &feed,
            &instance,
            &mut wanted,
            &publisher,
            &mut BTreeSet::new(),
            &AdmitsAll,
        )
        .await;
        let mut last = None;
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(200), events.recv()).await
        {
            if let TickStreamEvent::State { state, .. } = event {
                last = Some(state);
            }
        }
        (ended, last.expect("a connection state was published"))
    }

    #[tokio::test]
    async fn a_watch_whose_stream_ends_with_its_tick_reports_it_gone_at_once() {
        assert_eq!(observe_ending(1).await, (true, TickConnectionState::Gone));
    }

    #[tokio::test]
    async fn a_reconnect_that_finds_its_tick_ended_reports_it_gone() {
        assert_eq!(observe_ending(0).await, (true, TickConnectionState::Gone));
    }

    #[tokio::test]
    async fn a_watch_whose_tick_still_runs_stays_disconnected_until_it_reconnects() {
        assert_eq!(
            observe_ending(usize::MAX).await,
            (false, TickConnectionState::Disconnected)
        );
    }

    #[tokio::test]
    async fn a_new_watch_is_not_retired_with_a_worker_that_ended_before_it_joined() {
        let (publisher, _events) = EventPublisher::channel();
        let instance = InstanceRef::hosted(Uuid::new_v4());
        let old = Uuid::new_v4();
        let fresh = Uuid::new_v4();
        let old_desired = BTreeMap::from([(old, instance.clone())]);
        publisher.retain(&old_desired);
        publisher.state_to(
            instance.clone(),
            TickConnectionState::IdentityUncertain,
            [old],
        );

        let all_desired = BTreeMap::from([(old, instance.clone()), (fresh, instance)]);
        publisher.retain(&all_desired);
        assert_eq!(
            publisher.terminal_watches(&BTreeSet::from([old, fresh])),
            BTreeSet::from([old]),
            "a fresh explicit Watch must be eligible for a new worker"
        );
    }
}
