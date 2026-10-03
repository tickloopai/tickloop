//! Observation stream between a Tick host and the Agent's Loop scheduler.

mod artifact;
pub use artifact::ArtifactTickStream;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use serde::{Deserialize, Serialize};
pub use tick_connect::instance::InstanceRef;
use tokio::sync::{Notify, mpsc, watch};
use uuid::Uuid;

pub type WatchId = Uuid;
const STREAM_CAPACITY: usize = 128;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ObservationKind {
    Protocol,
    Terminal { truncated: bool },
}

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

/// A subscription's prepared query binding; admission revalidates it without reading its body.
pub struct PreparedTickWatch {
    provider: Arc<dyn crate::host_tools::HostToolProvider>,
    query: crate::host_tools::PreparedHostCall,
}
impl PreparedTickWatch {
    pub fn new(
        provider: Arc<dyn crate::host_tools::HostToolProvider>,
        query: crate::host_tools::PreparedHostCall,
    ) -> Self {
        Self { provider, query }
    }
    pub(crate) fn provider(&self) -> &dyn crate::host_tools::HostToolProvider {
        self.provider.as_ref()
    }
    pub(crate) fn into_admission(
        self,
        input: serde_json::Value,
    ) -> crate::host_tools::PreparedHostCall {
        crate::host_tools::PreparedHostCall::new(
            "SubscribeTick".into(),
            input.clone(),
            input,
            self.query.permission(),
            self.query.approval_summary().into(),
        )
        .with_policy_identity(
            self.query.policy_name().into(),
            self.query.policy_input().clone(),
        )
        .with_permission_input(self.query.permission_input().clone())
        .with_payload(self)
    }
    pub(crate) async fn revalidate(
        chat: Uuid,
        call: &crate::host_tools::PreparedHostCall,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        let watch = call
            .payload::<Self>()
            .ok_or("Tick watch lost its prepared query")?;
        watch
            .provider
            .revalidate_call(chat, &watch.query, cancel)
            .await
    }
}

#[async_trait::async_trait]
pub trait TickStreamPort: Send + Sync + 'static {
    fn open(&self) -> TickStreamHandle;
    fn set_cadences(&self, _cadences: BTreeMap<WatchId, u64>) {}
    /// Caller identities for active watches, including restored subscriptions.
    fn set_watch_chats(&self, _chats: BTreeMap<WatchId, Uuid>) {}
    /// Adapters prepare their read operation so watching inherits its policy and ownership.
    async fn prepare_watch(
        &self,
        _chat: Uuid,
        _instance: &InstanceRef,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<PreparedTickWatch>, String> {
        Ok(None)
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

impl TickStreamPort for InMemoryTickStream {
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
    access: Arc<tick_connect::access::TickAccess>,
}

impl NativeTickStream {
    pub fn new(access: Arc<tick_connect::access::TickAccess>) -> Self {
        Self { access }
    }
}

impl TickStreamPort for NativeTickStream {
    fn open(&self) -> TickStreamHandle {
        let (desired, wanted) = watch::channel(BTreeMap::new());
        let (publisher, events) = EventPublisher::channel();
        let access = self.access.clone();
        tokio::spawn(async move { follow_native(access, wanted, publisher).await });
        TickStreamHandle { desired, events }
    }
}

async fn follow_native(
    access: Arc<tick_connect::access::TickAccess>,
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
        let mut groups: BTreeMap<InstanceRef, BTreeSet<WatchId>> = BTreeMap::new();
        for (watch, instance) in eligible {
            groups.entry(instance).or_default().insert(watch);
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
            let access = access.clone();
            let publisher = publisher.clone();
            let finished_sender = finished_sender.clone();
            tokio::spawn(async move {
                observe_native_instance(access, instance.clone(), receiver, publisher).await;
                let _ = finished_sender.send((instance, generation));
            });
        }
        tokio::select! {
            changed = desired.changed() => if changed.is_err() { return; },
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
    access: Arc<tick_connect::access::TickAccess>,
    instance: InstanceRef,
    mut wanted: watch::Receiver<BTreeSet<WatchId>>,
    publisher: EventPublisher,
) {
    let mut ready_watches = BTreeSet::new();
    loop {
        if wanted.borrow().is_empty() {
            return;
        }
        let attempted = wanted.borrow_and_update().clone();
        match access.resolve(&instance).await {
            Ok(location) if location.protocol => {
                let ended = observe_protocol(
                    &access,
                    &instance,
                    &mut wanted,
                    &publisher,
                    &mut ready_watches,
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
                    &access,
                    &instance,
                    &mut wanted,
                    &publisher,
                    &mut ready_watches,
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
        let pause = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(pause);
        tokio::select! {
            _ = &mut pause => {}
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
    access: &tick_connect::access::TickAccess,
    instance: &InstanceRef,
    wanted: &mut watch::Receiver<BTreeSet<WatchId>>,
    publisher: &EventPublisher,
    ready_watches: &mut BTreeSet<WatchId>,
) -> bool {
    let mut cursors: BTreeMap<WatchId, u64> = BTreeMap::new();
    let mut last_text: Option<String> = None;
    let mut sequence = 0_u64;
    let mut lost_host_events = false;
    let mut cadence = tokio::time::interval(Duration::from_secs(5));
    cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if wanted.borrow().is_empty() {
            return true;
        }
        tokio::select! {
            _ = cadence.tick() => {}
            changed = wanted.changed() => if changed.is_err() { return true; },
        }
        let targets = wanted.borrow_and_update().clone();
        if targets.is_empty() {
            return true;
        }
        match access.read(instance).await {
            Ok(snapshot) if snapshot.terminal => {
                publisher.metadata(
                    instance.clone(),
                    targets.iter().copied(),
                    TickMetadata {
                        title: instance.to_string(),
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
                            observed_at: snapshot.observed_at,
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

async fn observe_protocol(
    access: &tick_connect::access::TickAccess,
    instance: &InstanceRef,
    wanted: &mut watch::Receiver<BTreeSet<WatchId>>,
    publisher: &EventPublisher,
    ready_watches: &mut BTreeSet<WatchId>,
) -> bool {
    use tick_connect::session::SessionLinkState;

    let attempted = wanted.borrow_and_update().clone();
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
    let recovery_tool = match session.describe().await {
        Ok(descriptor) => descriptor.recover_with,
        Err(_) => {
            publisher.state_to(
                instance.clone(),
                TickConnectionState::Disconnected,
                attempted.iter().copied(),
            );
            return false;
        }
    };
    let (baseline, registration_received, mut updates) = match session.watch().await {
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
    let mut cursors: BTreeMap<WatchId, ProtocolCursor> = BTreeMap::new();
    let initial = updates.borrow_and_update().clone();
    publisher.metadata(
        instance.clone(),
        wanted.borrow().iter().copied(),
        TickMetadata {
            title: instance.to_string(),
            kind: ObservationKind::Protocol,
            updated_at_ms: None,
        },
    );
    let mut latest_sequence = baseline.sequence;
    let baseline_targets = wanted.borrow_and_update().clone();
    for watch in baseline_targets.iter().copied() {
        cursors.insert(
            watch,
            ProtocolCursor {
                received: registration_received,
                sequence: baseline.sequence,
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
    let initial_targets = wanted.borrow().clone();
    if let Some(sequence) = publish_protocol_update(
        instance,
        &initial,
        &initial_targets,
        &mut cursors,
        recovery_tool.as_deref(),
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
        let targets = wanted.borrow_and_update().clone();
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
            recovery_tool.as_deref(),
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
    observation: &tick_connect::session::SessionObservation,
    targets: &BTreeSet<WatchId>,
    cursors: &mut BTreeMap<WatchId, ProtocolCursor>,
    recovery_tool: Option<&str>,
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
        let source_gap = update.sequence.saturating_sub(cursor.sequence) > accepted;
        groups
            .entry((dropped, source_gap))
            .or_default()
            .push(*watch);
        cursor.received = observation.received;
        cursor.sequence = update.sequence;
    }
    for ((dropped, gap), audience) in groups {
        publisher.observation(
            &audience,
            TickObservation {
                instance: instance.clone(),
                sequence: update.sequence,
                observed_at: update.observed_at.clone(),
                dropped,
                data: update.data.clone(),
                kind: ObservationKind::Protocol,
                recover_with: recovery_tool.map(str::to_owned),
                gap,
            },
        );
    }
    Some(update.sequence)
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
