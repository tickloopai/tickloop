//! Observation stream between a Tickhost and the Agent's Loop scheduler.

mod artifact;
mod hosted;
pub use artifact::ArtifactTickStream;
pub use hosted::HostedTickStream;

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
/// How often an ordinary terminal is sampled while it is watched.
const TERMINAL_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

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
}

/// What a protocol Tick that declares no recovery query tells the Agent instead.
const NO_RECOVERY: &str =
    "none: dropped Tick Updates cannot be recovered; ReadTick returns only the current state";

/// A protocol Tick's current content as an observation, for a delivery read again.
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
                    } else {
                        mailbox.latest.pop_first().map(|(watch, value)| {
                            TickStreamEvent::Observation {
                                audience: vec![watch],
                                value,
                            }
                        })
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

    pub fn emit(&self, event: TickStreamEvent) {
        if let Some(events) = &self.state().events {
            match event {
                TickStreamEvent::Metadata {
                    instance,
                    audience,
                    metadata,
                } => events.metadata(instance, audience, metadata),
                TickStreamEvent::Ready { watch } => events.ready(watch),
                TickStreamEvent::Observation { audience, value } => {
                    events.observation(&audience, value)
                }
                TickStreamEvent::State {
                    instance,
                    state,
                    audience,
                } => events.state_to(instance, state, audience),
                TickStreamEvent::Gap { instance, audience } => events.gap_to(instance, audience),
            }
        }
    }

    fn state(&self) -> MutexGuard<'_, InMemoryState> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl TickStreamPort for InMemoryTickStream {
    fn set_watch_authorities(&self, _authorities: BTreeMap<WatchId, HostToolAuthority>) {}
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

/// Native Herdr observation adapter shared with the Agent's host tools through one TickAccess.
pub struct NativeTickStream {
    provider: Arc<crate::native_host::NativeHostToolProvider>,
    authorities: watch::Sender<BTreeMap<WatchId, HostToolAuthority>>,
    budget: ConnectionBudget,
}

impl NativeTickStream {
    pub fn new(provider: Arc<crate::native_host::NativeHostToolProvider>) -> Self {
        Self {
            provider,
            authorities: watch::channel(BTreeMap::new()).0,
            budget: ConnectionBudget::default(),
        }
    }

    fn scope(&self) -> NativeWatchScope {
        NativeWatchScope {
            provider: self.provider.clone(),
            authorities: self.authorities.subscribe(),
            budget: self.budget.clone(),
        }
    }
}

#[derive(Clone)]
struct NativeWatchScope {
    provider: Arc<crate::native_host::NativeHostToolProvider>,
    authorities: watch::Receiver<BTreeMap<WatchId, HostToolAuthority>>,
    budget: ConnectionBudget,
}
impl NativeWatchScope {
    fn revalidate(&self, authority: Option<&HostToolAuthority>) -> Result<(), String> {
        use crate::host_tools::HostToolProvider;
        let Some(authority @ HostToolAuthority::Native { .. }) = authority else {
            return Err(
                "Native watch has no admitted endpoint authority; subscribe explicitly".into(),
            );
        };
        if !self
            .provider
            .tool_authorities(crate::tick_tool_router::READ_TICK)?
            .contains(authority)
        {
            return Err("Native watch endpoint is outside its admitted authority".into());
        }
        Ok(())
    }
}

impl WatchAdmission for NativeWatchScope {
    fn admitted(
        &self,
        requested: &BTreeSet<WatchId>,
        instance: &InstanceRef,
        publisher: &EventPublisher,
    ) -> BTreeSet<WatchId> {
        requested
            .iter()
            .filter_map(|watch| {
                if self
                    .revalidate(self.authorities.borrow().get(watch))
                    .is_ok()
                {
                    Some(*watch)
                } else {
                    publisher.state_to(
                        instance.clone(),
                        TickConnectionState::IdentityUncertain,
                        [*watch],
                    );
                    None
                }
            })
            .filter(|watch| self.budget.admit_or_disconnect(*watch, instance, publisher))
            .collect()
    }
}

#[async_trait::async_trait]
impl InstanceObserver for NativeWatchScope {
    fn budget(&self) -> &ConnectionBudget {
        &self.budget
    }

    async fn authorities_changed(&mut self) -> bool {
        self.authorities.changed().await.is_ok()
    }

    async fn observe(
        self,
        instance: InstanceRef,
        wanted: watch::Receiver<BTreeSet<WatchId>>,
        publisher: EventPublisher,
    ) {
        observe_native_instance(self, instance, wanted, publisher).await;
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

/// A protocol Tick as a watch sees it: its declaration, which names it and its recovery tool,
/// and a registration that yields the current content and then latest-only updates.
#[async_trait::async_trait]
trait ProtocolFeed: Send + Sync {
    async fn descriptor(&self) -> Result<tick_protocol::TickDescriptor, String>;

    async fn watch(
        &self,
    ) -> Result<
        (
            tick_protocol::TickState,
            u64,
            watch::Receiver<tickhost_connect::session::SessionObservation>,
        ),
        String,
    >;
}

#[async_trait::async_trait]
impl ProtocolFeed for tickhost_connect::session::SessionClient {
    async fn descriptor(&self) -> Result<tick_protocol::TickDescriptor, String> {
        self.describe().await.map_err(|error| error.to_string())
    }

    async fn watch(
        &self,
    ) -> Result<
        (
            tick_protocol::TickState,
            u64,
            watch::Receiver<tickhost_connect::session::SessionObservation>,
        ),
        String,
    > {
        tickhost_connect::session::SessionClient::watch(self)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl TickStreamPort for NativeTickStream {
    async fn prepare_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        _input: &serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<PreparedTickWatch>, String> {
        use crate::host_tools::HostToolProvider;
        let query = self
            .provider
            .prepare_call(
                chat,
                "ReadTick",
                &serde_json::json!({"tick_instance_id":instance}),
                cancel,
            )
            .await?;
        let watch = PreparedTickWatch::new(self.provider.clone(), query)?;
        let authority = watch
            .host_authority()?
            .and_then(|binding| binding.authority);
        self.revalidate_watch(chat, instance, authority.as_ref())
            .await?;
        Ok(Some(watch))
    }
    fn set_watch_authorities(&self, authorities: BTreeMap<WatchId, HostToolAuthority>) {
        self.authorities.send_replace(authorities);
    }
    async fn revalidate_watch(
        &self,
        _chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        if instance.tick_type() != tick_protocol::HERDR_INSTANCE_TYPE {
            return Err("Native watch requires a Herdr instance".into());
        }
        self.scope().revalidate(authority)
    }
    async fn read_current(
        &self,
        _chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<Option<TickObservation>, String> {
        let session = self
            .provider
            .access()
            .session(instance)
            .await
            .map_err(|error| error.to_string())?;
        let read = session.read().await.map_err(|error| error.to_string())?;
        Ok(Some(current_observation(instance, read)))
    }
    fn open(&self) -> TickStreamHandle {
        let (desired, wanted) = watch::channel(BTreeMap::new());
        let (publisher, events) = EventPublisher::channel();
        let scope = self.scope();
        tokio::spawn(async move { follow_instances(scope, wanted, publisher).await });
        TickStreamHandle { desired, events }
    }
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

async fn observe_native_instance(
    scope: NativeWatchScope,
    instance: InstanceRef,
    mut wanted: watch::Receiver<BTreeSet<WatchId>>,
    publisher: EventPublisher,
) {
    let mut ready_watches = BTreeSet::new();
    loop {
        if wanted.borrow().is_empty() {
            return;
        }
        let admitted = scope.admitted(&wanted.borrow_and_update().clone(), &instance, &publisher);
        let attempted = scope.budget.take(&admitted, &instance, &publisher);
        if attempted.is_empty() {
            return;
        }
        let access = scope.provider.access();
        let location = access.resolve(&instance).await;
        if scope.admitted(&attempted, &instance, &publisher).is_empty() {
            return;
        }
        match location {
            Ok(location) if location.protocol => {
                let ended = observe_native_protocol(
                    access,
                    &instance,
                    &mut wanted,
                    &publisher,
                    &mut ready_watches,
                    &scope,
                )
                .await;
                if ended {
                    return;
                }
                if !wanted.borrow().is_empty() {
                    publisher.gap_to(instance.clone(), ready_watches.iter().copied());
                }
            }
            Ok(_) => {
                if observe_terminal(
                    access,
                    &instance,
                    &mut wanted,
                    &publisher,
                    &mut ready_watches,
                    &scope,
                )
                .await
                {
                    return;
                }
            }
            Err(error) => {
                let state = connection_state(&error.code);
                publisher.state_to(instance.clone(), state, attempted.iter().copied());
                if matches!(
                    state,
                    TickConnectionState::Gone | TickConnectionState::IdentityUncertain
                ) {
                    return;
                }
            }
        }
        if wanted.borrow().is_empty() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_PAUSE) => {}
            changed = wanted.changed() => if changed.is_err() { return; },
        }
    }
}

fn connection_state(code: &str) -> TickConnectionState {
    match code {
        "TargetGone" => TickConnectionState::Gone,
        "IdentityUncertain" | "ProtocolViolation" | "IncompatibleHost" => {
            TickConnectionState::IdentityUncertain
        }
        _ => TickConnectionState::Disconnected,
    }
}

async fn observe_terminal(
    access: &tickhost_connect::access::TickAccess,
    instance: &InstanceRef,
    wanted: &mut watch::Receiver<BTreeSet<WatchId>>,
    publisher: &EventPublisher,
    ready_watches: &mut BTreeSet<WatchId>,
    scope: &NativeWatchScope,
) -> bool {
    let mut cursors: BTreeMap<WatchId, u64> = BTreeMap::new();
    let mut last_text: Option<String> = None;
    let mut sequence = 0_u64;
    let mut lost_host_events = false;
    let mut sampling = tokio::time::interval(TERMINAL_SAMPLE_INTERVAL);
    sampling.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if wanted.borrow().is_empty() {
            return true;
        }
        tokio::select! {
            _ = sampling.tick() => {}
            changed = wanted.changed() => if changed.is_err() { return true; },
        }
        let targets = scope.admitted(&wanted.borrow_and_update().clone(), instance, publisher);
        if targets.is_empty() {
            return true;
        }
        let snapshot = access.read(instance).await;
        let targets = scope.admitted(&targets, instance, publisher);
        if targets.is_empty() {
            return true;
        }
        match snapshot {
            Ok(snapshot) if snapshot.terminal => {
                publisher.metadata(
                    instance.clone(),
                    targets.iter().copied(),
                    TickMetadata {
                        title: access
                            .located_name(instance)
                            .await
                            .unwrap_or_else(|| instance.to_string()),
                        kind: ObservationKind::Terminal {
                            truncated: snapshot.truncated,
                        },
                        updated_at_ms: None,
                    },
                );
                if lost_host_events {
                    publisher.state_to(
                        instance.clone(),
                        TickConnectionState::IdentityUncertain,
                        targets.iter().copied(),
                    );
                    return true;
                }
                let text = snapshot.data["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let has_changed = last_text.as_ref().is_some_and(|previous| previous != &text);
                if has_changed {
                    sequence = sequence.saturating_add(1);
                    let audience: Vec<_> = cursors
                        .keys()
                        .filter(|watch| targets.contains(watch))
                        .copied()
                        .collect();
                    publisher.observation(
                        &audience,
                        TickObservation {
                            instance: instance.clone(),
                            sequence,
                            observed_at: snapshot.at,
                            dropped: 0,
                            data: serde_json::json!({"text": text}),
                            kind: ObservationKind::Terminal {
                                truncated: snapshot.truncated,
                            },
                            recover_with: None,
                            gap: false,
                        },
                    );
                }
                last_text = Some(text);
                cursors.retain(|watch, _| targets.contains(watch));
                for watch in &targets {
                    if cursors.insert(*watch, sequence).is_none() {
                        publisher.ready(*watch);
                        ready_watches.insert(*watch);
                    }
                }
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::Live,
                    targets.iter().copied(),
                );
            }
            Ok(_) => {
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::IdentityUncertain,
                    targets.iter().copied(),
                );
                return true;
            }
            Err(error) => {
                let state = connection_state(&error.code);
                publisher.state_to(instance.clone(), state, targets.iter().copied());
                if matches!(
                    state,
                    TickConnectionState::Gone | TickConnectionState::IdentityUncertain
                ) {
                    return true;
                }
                lost_host_events |= last_text.is_some();
            }
        }
    }
}

async fn observe_native_protocol(
    access: &tickhost_connect::access::TickAccess,
    instance: &InstanceRef,
    wanted: &mut watch::Receiver<BTreeSet<WatchId>>,
    publisher: &EventPublisher,
    ready_watches: &mut BTreeSet<WatchId>,
    scope: &NativeWatchScope,
) -> bool {
    let attempted = scope.admitted(&wanted.borrow_and_update().clone(), instance, publisher);
    if attempted.is_empty() {
        return true;
    }
    let session = match access.session(instance).await {
        Ok(session) => session,
        Err(error) => {
            let state = connection_state(&error.code);
            publisher.state_to(instance.clone(), state, attempted.iter().copied());
            return matches!(
                state,
                TickConnectionState::Gone | TickConnectionState::IdentityUncertain
            );
        }
    };
    observe_protocol(&session, instance, wanted, publisher, ready_watches, scope).await
}

/// Follows a protocol Tick for the wanted watches: a baseline for each, then latest-only
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
        Err(_) => {
            publisher.state_to(
                instance.clone(),
                TickConnectionState::Disconnected,
                attempted.iter().copied(),
            );
            return false;
        }
    };
    if scope.admitted(&attempted, instance, publisher).is_empty() {
        return true;
    }
    let (baseline, registration_received, mut updates) = match feed.watch().await {
        Ok(watch) => watch,
        Err(_) => {
            publisher.state_to(
                instance.clone(),
                TickConnectionState::Disconnected,
                attempted.iter().copied(),
            );
            return false;
        }
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
            publisher.state_to(
                instance.clone(),
                TickConnectionState::Disconnected,
                cursors.keys().copied(),
            );
            return false;
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
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::Disconnected,
                    cursors.keys().copied(),
                );
                return false;
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
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::Disconnected,
                    cursors.keys().copied(),
                );
                return false;
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
    use super::*;

    #[tokio::test]
    async fn a_new_watch_is_not_retired_with_a_worker_that_ended_before_it_joined() {
        let (publisher, _events) = EventPublisher::channel();
        let instance = InstanceRef::native("terminal:one:1:birth").unwrap();
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
