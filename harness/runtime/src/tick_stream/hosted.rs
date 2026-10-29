//! Tick Updates of hosted Ticks, which run in the dedicated Tickhost. A watch follows the
//! instance through the data binding its Chat owns, so a Tick whose host was replaced or whose
//! owner ended is never silently rebound. An extension Tick pushes its updates. A terminal Tick's
//! screen is read through its host watch when a subscription's period elapses or its program
//! exits, and compared with what that watch last got; between reads the host only says when the
//! screen first changed, which marks the change pending for each watch that has not got it.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::{Arc, Mutex, PoisonError},
};

use serde_json::{Value, json};
use tick_protocol::TickState;
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use super::{
    ConnectionBudget, EventPublisher, InstanceObserver, InstanceRef, PreparedTickWatch,
    ProtocolFeed, RECONNECT_PAUSE, StreamTasks, TickConnectionState, TickMetadata, TickObservation,
    TickStreamHandle, TickStreamPort, WatchAdmission, WatchId, connection_state,
    current_observation, follow_instances, observe_protocol,
};
use crate::environment::{
    ExecutionAuthority, ExecutionEnvironment, ExecutionError, ExecutionInspection, ExecutionKind,
    ExecutionLinkState, ExecutionPort, ExecutionQuery, ExecutionScope, ExecutionUpdate,
    ObservationCursor, ObserveRequest, TerminalDescription, TerminalRead,
};
use crate::tick_tools::TICK_READ;
use tickloop_harness_protocol::{HostToolAuthority, ObservationKind};

type Screens = Arc<Mutex<TerminalScreens>>;

/// Samples waiting for one observer of a terminal Tick.
const SAMPLE_QUEUE: usize = 16;

/// What each watch of a terminal Tick last got of its screen, where its next sample is compared,
/// and the observers that read the screen for those samples.
#[derive(Default)]
struct TerminalScreens {
    cursors: BTreeMap<WatchId, ScreenCursor>,
    /// Each observer's way to its live host watch: a sample reads through it, so the host's
    /// change signal is armed against what was read.
    samplers: BTreeMap<Uuid, mpsc::Sender<SampleRequest>>,
}

struct ScreenCursor {
    /// The observer that follows the watch now; only it forgets the record when it ends.
    observer: Uuid,
    chat: Uuid,
    instance: InstanceRef,
    sent: Value,
    /// The screen changed since `sent`, as the watch was last told.
    has_pending_change: bool,
}

/// A sample of one watch, answered with the screen when it differs from what the watch last got.
struct SampleRequest {
    watch: WatchId,
    reply: oneshot::Sender<Option<TickObservation>>,
}

pub struct HostedTickStream {
    environment: Arc<dyn ExecutionEnvironment>,
    authority: HostToolAuthority,
    authorities: watch::Sender<BTreeMap<WatchId, HostToolAuthority>>,
    chats: watch::Sender<BTreeMap<WatchId, Uuid>>,
    /// The (Chat, instance) pairs with a live watch registration, whose binding is therefore
    /// current.
    live: Arc<Mutex<HashSet<(Uuid, InstanceRef)>>>,
    screens: Screens,
    budget: ConnectionBudget,
}

impl HostedTickStream {
    pub fn with_environment(
        environment: Arc<dyn ExecutionEnvironment>,
        authority: HostToolAuthority,
    ) -> Self {
        Self {
            environment,
            authority,
            authorities: watch::channel(BTreeMap::new()).0,
            chats: watch::channel(BTreeMap::new()).0,
            live: Arc::default(),
            screens: Arc::default(),
            budget: ConnectionBudget::default(),
        }
    }

    fn scope(&self) -> HostedScope {
        HostedScope {
            environment: self.environment.clone(),
            authority: self.authority.clone(),
            authorities: self.authorities.subscribe(),
            chats: self.chats.subscribe(),
            live: self.live.clone(),
            screens: self.screens.clone(),
            budget: self.budget.clone(),
        }
    }
}

#[derive(Clone)]
struct HostedScope {
    environment: Arc<dyn ExecutionEnvironment>,
    authority: HostToolAuthority,
    authorities: watch::Receiver<BTreeMap<WatchId, HostToolAuthority>>,
    chats: watch::Receiver<BTreeMap<WatchId, Uuid>>,
    live: Arc<Mutex<HashSet<(Uuid, InstanceRef)>>>,
    screens: Screens,
    budget: ConnectionBudget,
}

impl HostedScope {
    /// A hosted watch is admitted only under the authority of this worker's own Tickhost.
    fn authorizes(&self, authority: Option<&HostToolAuthority>) -> Result<(), String> {
        let Some(HostToolAuthority::Native { .. }) = authority else {
            return Err("Hosted watch has no admitted host authority; subscribe explicitly".into());
        };
        if authority != Some(&self.authority) {
            return Err("Hosted watch is outside its admitted Tickhost".into());
        }
        Ok(())
    }

    fn chat_of(&self, watch: &WatchId) -> Option<Uuid> {
        self.chats.borrow().get(watch).copied()
    }

    /// Forgets what the watches `observer` followed last got of a terminal Tick's screen.
    fn forget_screens(&self, observer: Uuid) {
        self.screens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cursors
            .retain(|_, cursor| cursor.observer != observer);
    }

    fn mark_live(&self, chat: Uuid, instance: &InstanceRef, is_live: bool) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if is_live {
            live.insert((chat, instance.clone()));
        } else {
            live.remove(&(chat, instance.clone()));
        }
    }
}

impl WatchAdmission for HostedScope {
    fn admitted(
        &self,
        requested: &BTreeSet<WatchId>,
        instance: &InstanceRef,
        publisher: &EventPublisher,
    ) -> BTreeSet<WatchId> {
        let authorities = self.authorities.borrow();
        requested
            .iter()
            .filter_map(|watch| {
                if self.authorizes(authorities.get(watch)).is_ok() && self.chat_of(watch).is_some()
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
impl InstanceObserver for HostedScope {
    fn budget(&self) -> &ConnectionBudget {
        &self.budget
    }

    async fn authorities_changed(&mut self) -> bool {
        tokio::select! {
            changed = self.authorities.changed() => changed.is_ok(),
            changed = self.chats.changed() => changed.is_ok(),
        }
    }

    async fn observe(
        self,
        instance: InstanceRef,
        wanted: watch::Receiver<BTreeSet<WatchId>>,
        publisher: EventPublisher,
        tasks: StreamTasks,
    ) {
        let observer = tickloop_ids::generate();
        observe_hosted_instance(&self, observer, instance, wanted, publisher, tasks).await;
        // A later watch of this Tick starts from the screen as it is then.
        self.forget_screens(observer);
    }
}

async fn observe_hosted_instance(
    scope: &HostedScope,
    observer: Uuid,
    instance: InstanceRef,
    mut wanted: watch::Receiver<BTreeSet<WatchId>>,
    publisher: EventPublisher,
    _tasks: StreamTasks,
) {
    let mut ready_watches = BTreeSet::new();
    loop {
        let requested = wanted.borrow_and_update().clone();
        let Some(chat) = requested.iter().find_map(|watch| scope.chat_of(watch)) else {
            return;
        };
        let owned = OwnedWatches { scope, chat };
        let owned_watches = owned.admitted(&requested, &instance, &publisher);
        let attempted = scope.budget.take(&owned_watches, &instance, &publisher);
        if attempted.is_empty() {
            if wanted.changed().await.is_err() {
                return;
            }
            continue;
        }
        let feed = HostedFeed {
            environment: scope.environment.clone(),
            chat,
            instance: instance.clone(),
        };
        match feed.check_binding().await {
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
            Ok(adapter) => {
                scope.mark_live(chat, &instance, true);
                let ended = match adapter {
                    ExecutionKind::Protocol => {
                        observe_protocol(
                            &feed,
                            &instance,
                            &mut wanted,
                            &publisher,
                            &mut ready_watches,
                            &owned,
                        )
                        .await
                    }
                    ExecutionKind::Shell => return,
                    ExecutionKind::Terminal => {
                        observe_terminal_screen(
                            &feed,
                            observer,
                            &mut wanted,
                            &publisher,
                            &owned,
                            &scope.screens,
                        )
                        .await
                    }
                };
                scope.mark_live(chat, &instance, false);
                if ended {
                    return;
                }
                // A terminal Tick loses nothing while disconnected: its next sample compares with
                // what each watch last got.
                if adapter == ExecutionKind::Protocol && !wanted.borrow().is_empty() {
                    publisher.gap_to(instance.clone(), ready_watches.iter().copied());
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

/// The admitted watches of the one Chat that owns the observed instance: only that Chat holds
/// the binding.
struct OwnedWatches<'a> {
    scope: &'a HostedScope,
    chat: Uuid,
}

impl WatchAdmission for OwnedWatches<'_> {
    fn admitted(
        &self,
        requested: &BTreeSet<WatchId>,
        instance: &InstanceRef,
        publisher: &EventPublisher,
    ) -> BTreeSet<WatchId> {
        let mut admitted = self.scope.admitted(requested, instance, publisher);
        admitted.retain(|watch| {
            if self.scope.chat_of(watch) != Some(self.chat) {
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::IdentityUncertain,
                    [*watch],
                );
                return false;
            }
            true
        });
        admitted
    }
}

/// One hosted instance as its owning Chat reaches it through the Tickhost.
struct HostedFeed {
    environment: Arc<dyn ExecutionEnvironment>,
    chat: Uuid,
    instance: InstanceRef,
}

impl HostedFeed {
    /// Proves the Chat still holds a current binding for the instance before anything is read,
    /// and tells which adapter serves it.
    fn port(&self) -> Result<Arc<dyn ExecutionPort>, ExecutionError> {
        self.environment.bind(
            ExecutionScope {
                chat: self.chat.into(),
                folder: None,
                additional_directories: Vec::new(),
            },
            ExecutionAuthority::new(self.chat.into()),
        )
    }

    async fn check_binding(&self) -> Result<ExecutionKind, ExecutionError> {
        match self
            .port()?
            .inspect(ExecutionQuery::Kind(self.instance.clone()))
            .await?
        {
            ExecutionInspection::Kind(ExecutionKind::Protocol) => Ok(ExecutionKind::Protocol),
            ExecutionInspection::Kind(ExecutionKind::Terminal) => Ok(ExecutionKind::Terminal),
            _ => Err(ExecutionError::new(
                "Unsupported",
                "this is not a hosted Tick",
            )),
        }
    }

    /// Registers a host watch of a terminal Tick: what it runs, its screen now, which the host's
    /// change signal is armed against, and the watch itself.
    async fn watch_terminal(
        &self,
    ) -> Result<(TerminalDescription, TerminalRead, TerminalLink), String> {
        let port = self.port().map_err(|error| error.to_string())?;
        let ExecutionInspection::Description(description) = port
            .inspect(ExecutionQuery::Describe(self.instance.clone()))
            .await
            .map_err(|error| error.to_string())?
        else {
            return Err("missing terminal description".into());
        };
        let description = serde_json::from_value(description).map_err(|error| error.to_string())?;
        let observed = port
            .observe(ObserveRequest {
                instance: self.instance.clone(),
                after: None,
            })
            .await
            .map_err(|error| error.to_string())?;
        let baseline =
            TerminalRead::from_read(observed.baseline).map_err(|error| error.to_string())?;
        Ok((
            description,
            baseline,
            TerminalLink {
                port,
                cursor: observed.cursor,
                updates: observed.updates,
                saw_exit: false,
            },
        ))
    }
}

struct TerminalLink {
    port: Arc<dyn ExecutionPort>,
    cursor: ObservationCursor,
    updates: watch::Receiver<ExecutionUpdate>,
    saw_exit: bool,
}

enum LinkEvent {
    Changed,
    Exited,
    Ended(TickConnectionState),
}

impl TerminalLink {
    async fn next_event(&mut self) -> LinkEvent {
        loop {
            if self.updates.changed().await.is_err() {
                return LinkEvent::Ended(TickConnectionState::Disconnected);
            }
            let update = self.updates.borrow_and_update().clone();
            match update.state {
                ExecutionLinkState::Gone => return LinkEvent::Ended(TickConnectionState::Gone),
                ExecutionLinkState::Disconnected => {
                    return LinkEvent::Ended(TickConnectionState::Disconnected);
                }
                ExecutionLinkState::ProtocolError(_) => {
                    return LinkEvent::Ended(TickConnectionState::IdentityUncertain);
                }
                ExecutionLinkState::Live => {}
            }
            if let Some(change) = update.terminal {
                if change.exited && !self.saw_exit {
                    self.saw_exit = true;
                    return LinkEvent::Exited;
                }
                if change.dirty {
                    return LinkEvent::Changed;
                }
            }
        }
    }

    async fn read(&mut self) -> Result<TerminalRead, TickConnectionState> {
        match self
            .port
            .inspect(ExecutionQuery::Sample(self.cursor.clone()))
            .await
        {
            Ok(ExecutionInspection::Read(read)) => {
                TerminalRead::from_read(read).map_err(|error| connection_state(&error.code))
            }
            Ok(_) => Err(TickConnectionState::IdentityUncertain),
            Err(error) => Err(connection_state(&error.code)),
        }
    }
}

/// An observer's entry in [`TerminalScreens::samplers`], removed when it stops following.
struct SamplerRegistration<'a> {
    screens: &'a Screens,
    observer: Uuid,
}

impl<'a> SamplerRegistration<'a> {
    fn register(
        screens: &'a Screens,
        observer: Uuid,
        sampler: mpsc::Sender<SampleRequest>,
    ) -> Self {
        screens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .samplers
            .insert(observer, sampler);
        Self { screens, observer }
    }
}

impl Drop for SamplerRegistration<'_> {
    fn drop(&mut self) {
        self.screens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .samplers
            .remove(&self.observer);
    }
}

/// The screen of a terminal Tick as one observer's watches got it, against the screen last read
/// through the observer's host watch.
struct ObservedScreen<'a> {
    observer: Uuid,
    instance: &'a InstanceRef,
    screens: &'a Screens,
    publisher: &'a EventPublisher,
    last_read: TerminalRead,
    has_announced_exit: bool,
}

impl ObservedScreen<'_> {
    fn lock(&self) -> std::sync::MutexGuard<'_, TerminalScreens> {
        self.screens.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts `watches` from the screen last read.
    fn join(&self, chat: Uuid, watches: &[WatchId]) {
        let mut screens = self.lock();
        for watch in watches {
            screens.cursors.insert(
                *watch,
                ScreenCursor {
                    observer: self.observer,
                    chat,
                    instance: self.instance.clone(),
                    sent: self.last_read.data(),
                    has_pending_change: false,
                },
            );
        }
    }

    /// Takes over `watches` an earlier observer followed: each keeps what it last got, holds a
    /// change when the screen last read differs from it, and reads the exit it missed at once.
    fn adopt(&self, watches: &[WatchId]) {
        let data = self.last_read.data();
        let (mut changed, mut exited) = (Vec::new(), Vec::new());
        {
            let mut screens = self.lock();
            for watch in watches {
                let Some(cursor) = screens.cursors.get_mut(watch) else {
                    continue;
                };
                cursor.observer = self.observer;
                if cursor.sent == data || !cursor.sent["exit"].is_null() {
                    continue;
                }
                if !cursor.has_pending_change {
                    cursor.has_pending_change = true;
                    changed.push(*watch);
                }
                if self.last_read.exit.is_some() {
                    exited.push(*watch);
                }
            }
        }
        self.publisher
            .screen_change(self.instance.clone(), changed, true);
        if !exited.is_empty() {
            self.publisher.program_exited(self.instance.clone(), exited);
        }
    }

    /// The host said the screen changed since it was last read: every watch that had got that
    /// screen now holds a change.
    fn changed(&self) {
        let changed = self
            .lock()
            .cursors
            .iter_mut()
            .filter(|(_, cursor)| {
                cursor.observer == self.observer
                    && !cursor.has_pending_change
                    && cursor.sent["exit"].is_null()
            })
            .map(|(watch, cursor)| {
                cursor.has_pending_change = true;
                *watch
            })
            .collect::<Vec<_>>();
        self.publisher
            .screen_change(self.instance.clone(), changed, true);
    }

    /// Settles a screen read through the host watch, for `sampled` when a sample asked for it:
    /// that watch gets the screen when it differs from what it last got, and its change is
    /// settled; every other watch holds a change when the screen differs from what it got.
    fn settle(&mut self, read: TerminalRead, sampled: Option<WatchId>) -> Option<TickObservation> {
        let data = read.data();
        let mut observation = None;
        let (mut changed, mut settled) = (Vec::new(), Vec::new());
        {
            let mut screens = self.lock();
            for (watch, cursor) in screens
                .cursors
                .iter_mut()
                .filter(|(_, cursor)| cursor.observer == self.observer)
            {
                if Some(*watch) == sampled {
                    if cursor.sent != data {
                        cursor.sent = data.clone();
                        observation = Some(terminal_observation(self.instance, read.clone()));
                    }
                    if std::mem::take(&mut cursor.has_pending_change) {
                        settled.push(*watch);
                    }
                } else if !cursor.has_pending_change
                    && cursor.sent != data
                    && cursor.sent["exit"].is_null()
                {
                    cursor.has_pending_change = true;
                    changed.push(*watch);
                }
            }
        }
        self.publisher
            .screen_change(self.instance.clone(), changed, true);
        self.publisher
            .screen_change(self.instance.clone(), settled, false);
        self.last_read = read;
        if self.last_read.exit.is_some() {
            self.announce_exit();
        }
        observation
    }

    /// Tells every watch that has not got the program's exit to read it now, once.
    fn announce_exit(&mut self) {
        if std::mem::replace(&mut self.has_announced_exit, true) {
            return;
        }
        let exited = self
            .lock()
            .cursors
            .iter()
            .filter(|(_, cursor)| cursor.observer == self.observer && cursor.sent["exit"].is_null())
            .map(|(watch, _)| *watch)
            .collect::<Vec<_>>();
        if !exited.is_empty() {
            self.publisher.program_exited(self.instance.clone(), exited);
        }
    }
}

/// A terminal Tick's screen, read without counting as using the Tick.
fn terminal_observation(instance: &InstanceRef, read: TerminalRead) -> TickObservation {
    TickObservation {
        instance: instance.clone(),
        sequence: read.seq,
        data: read.data(),
        observed_at: read.at,
        dropped: 0,
        kind: ObservationKind::Terminal {
            truncated: read.truncated,
        },
        recover_with: None,
        gap: false,
    }
}

/// Follows a terminal Tick for the wanted watches. Its screen is never pushed: each watch starts
/// from the screen as it is when it joins, and its subscription samples the screen through the
/// host watch at its own frequency, or at once when the program exits. In between the host only
/// says when the screen first changed since it was last read, which marks that change pending for
/// the watches that have not got it. Returns whether following ended for good rather than for a
/// reconnect.
async fn observe_terminal_screen(
    feed: &HostedFeed,
    observer: Uuid,
    wanted: &mut watch::Receiver<BTreeSet<WatchId>>,
    publisher: &EventPublisher,
    scope: &dyn WatchAdmission,
    screens: &Screens,
) -> bool {
    let instance = &feed.instance;
    let attempted = scope.admitted(&wanted.borrow_and_update().clone(), instance, publisher);
    if attempted.is_empty() {
        return true;
    }
    let (description, registered, mut link) = match feed.watch_terminal().await {
        Ok(watched) => watched,
        Err(_) => {
            publisher.state_to(
                instance.clone(),
                TickConnectionState::Disconnected,
                attempted.iter().copied(),
            );
            return false;
        }
    };
    let title = description
        .title
        .unwrap_or_else(|| description.command.join(" "));
    let (sampler, mut samples) = mpsc::channel(SAMPLE_QUEUE);
    let _registration = SamplerRegistration::register(screens, observer, sampler);
    let mut screen = ObservedScreen {
        observer,
        instance,
        screens,
        publisher,
        last_read: registered.clone(),
        has_announced_exit: false,
    };
    // The watches this observer followed before it reconnected hold the change, and read the
    // exit, they missed meanwhile.
    screen.settle(registered, None);
    let mut is_current = true;
    loop {
        let targets = scope.admitted(&wanted.borrow_and_update().clone(), instance, publisher);
        if targets.is_empty() {
            return true;
        }
        // A watch an earlier observer of this Tick followed keeps what it last got.
        let (joining, adopted) = {
            let mut screens = screens.lock().unwrap_or_else(PoisonError::into_inner);
            screens
                .cursors
                .retain(|watch, cursor| cursor.observer != observer || targets.contains(watch));
            let adopted = targets
                .iter()
                .filter(|watch| {
                    screens
                        .cursors
                        .get(watch)
                        .is_some_and(|cursor| cursor.observer != observer)
                })
                .copied()
                .collect::<Vec<_>>();
            let joining = targets
                .iter()
                .filter(|watch| !screens.cursors.contains_key(watch))
                .copied()
                .collect::<Vec<_>>();
            (joining, adopted)
        };
        if !joining.is_empty() && !is_current {
            match link.read().await {
                Ok(read) => {
                    screen.settle(read, None);
                }
                Err(state) => {
                    publisher.state_to(instance.clone(), state, targets.iter().copied());
                    return state != TickConnectionState::Disconnected;
                }
            }
        }
        screen.join(feed.chat, &joining);
        for watch in adopted.iter().chain(&joining) {
            publisher.ready_at(*watch, screen.last_read.seq);
        }
        screen.adopt(&adopted);
        publisher.metadata(
            instance.clone(),
            targets.iter().copied(),
            TickMetadata {
                title: title.clone(),
                kind: ObservationKind::Terminal {
                    truncated: screen.last_read.truncated,
                },
                updated_at_ms: None,
                series: Vec::new(),
            },
        );
        publisher.state_to(
            instance.clone(),
            TickConnectionState::Live,
            targets.iter().copied(),
        );
        // Ends with the state the watch ended in, or with none when the wanted watches changed.
        let ended = loop {
            tokio::select! {
                event = link.next_event() => match event {
                    LinkEvent::Changed => screen.changed(),
                    LinkEvent::Exited => screen.announce_exit(),
                    LinkEvent::Ended(state) => break Some(state),
                },
                Some(request) = samples.recv() => match link.read().await {
                    Ok(read) => {
                        let observation = screen.settle(read, Some(request.watch));
                        let _ = request.reply.send(observation);
                    }
                    Err(state) => break Some(state),
                },
                changed = wanted.changed() => {
                    if changed.is_err() {
                        return true;
                    }
                    break None;
                }
            }
        };
        if let Some(state) = ended {
            publisher.state_to(instance.clone(), state, targets.iter().copied());
            return state != TickConnectionState::Disconnected;
        }
        is_current = false;
    }
}

#[async_trait::async_trait]
impl ProtocolFeed for HostedFeed {
    async fn descriptor(&self) -> Result<tick_protocol::TickDescriptor, ExecutionError> {
        match self
            .port()?
            .inspect(ExecutionQuery::Describe(self.instance.clone()))
            .await?
        {
            ExecutionInspection::Description(value) => serde_json::from_value(value)
                .map_err(|error| ExecutionError::new("ProtocolViolation", error.to_string())),
            _ => Err(ExecutionError::new(
                "ProtocolViolation",
                "description query returned no descriptor",
            )),
        }
    }

    async fn watch(
        &self,
    ) -> Result<(TickState, u64, watch::Receiver<ExecutionUpdate>), ExecutionError> {
        let observation = self
            .port()?
            .observe(ObserveRequest {
                instance: self.instance.clone(),
                after: None,
            })
            .await?;
        Ok((
            observation.baseline.state,
            observation.cursor.position,
            observation.updates,
        ))
    }
}

#[async_trait::async_trait]
impl TickStreamPort for HostedTickStream {
    async fn prepare_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        _input: &Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<PreparedTickWatch>, String> {
        self.revalidate_watch(chat, instance, Some(&self.authority))
            .await?;
        if cancel.is_cancelled() {
            return Err("watch preparation was cancelled".into());
        }
        let input = json!({"tick_instance_id":instance});
        Ok(Some(PreparedTickWatch::with_validator(
            crate::host_tools::PreparedHostCall::new(
                TICK_READ.into(),
                input.clone(),
                input,
                crate::host_tools::HostToolPermission::ReadOnly,
                format!("Read hosted Tick {}", instance.public()),
            ),
            Arc::new(EnvironmentWatchValidator {
                environment: self.environment.clone(),
                authority: self.authority.clone(),
                chat,
                instance: instance.clone(),
            }),
        )))
    }

    fn set_watch_chats(&self, chats: BTreeMap<WatchId, Uuid>) {
        self.screens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cursors
            .retain(|watch, _| chats.contains_key(watch));
        self.chats.send_replace(chats);
    }

    fn set_watch_authorities(&self, authorities: BTreeMap<WatchId, HostToolAuthority>) {
        self.authorities.send_replace(authorities);
    }

    /// A hosted watch holds only while its authority is this worker's Tickhost and its Chat
    /// still owns a current binding of the instance: a live registration proves it, otherwise
    /// the daemon is asked again.
    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        if instance.access() != tick_protocol::HOSTED_ACCESS {
            return Err("Hosted watch requires a hosted instance".into());
        }
        let scope = self.scope();
        scope.authorizes(authority)?;
        let is_live = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&(chat, instance.clone()));
        if !is_live {
            let port = self
                .environment
                .bind(
                    ExecutionScope {
                        chat: chat.into(),
                        folder: None,
                        additional_directories: Vec::new(),
                    },
                    ExecutionAuthority::new(chat.into()),
                )
                .map_err(|error| error.to_string())?;
            port.inspect(ExecutionQuery::Kind(instance.clone()))
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// An automatic read: it never counts as using the Tick, so it does not keep it from idling.
    async fn read_current(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<Option<TickObservation>, String> {
        let port = self
            .environment
            .bind(
                ExecutionScope {
                    chat: chat.into(),
                    folder: None,
                    additional_directories: Vec::new(),
                },
                ExecutionAuthority::new(chat.into()),
            )
            .map_err(|error| error.to_string())?;
        let ExecutionInspection::Read(read) = port
            .inspect(ExecutionQuery::Read(instance.clone()))
            .await
            .map_err(|error| error.to_string())?
        else {
            return Err("read query returned no state".into());
        };
        let mut observation = current_observation(instance, read.state);
        if read.kind == ExecutionKind::Terminal {
            observation.kind = ObservationKind::Terminal {
                truncated: read.truncated,
            };
        }
        Ok(Some(observation))
    }

    /// Reads a terminal Tick's screen for `watch` while it is still admitted, through the host
    /// watch its observer holds, and reports it when it differs from what the watch last got,
    /// which it then becomes. A watch whose host watch is lost samples nothing until it
    /// reconnects.
    async fn sample(&self, watch: WatchId, instance: &InstanceRef) -> Option<TickObservation> {
        // Once a watch got the program's exit its screen never changes again.
        let (chat, sampler) = {
            let screens = self.screens.lock().unwrap_or_else(PoisonError::into_inner);
            let cursor = screens
                .cursors
                .get(&watch)
                .filter(|cursor| cursor.instance == *instance && cursor.sent["exit"].is_null())?;
            (cursor.chat, screens.samplers.get(&cursor.observer)?.clone())
        };
        let scope = self.scope();
        let is_admitted = scope.chat_of(&watch) == Some(chat)
            && scope
                .authorizes(scope.authorities.borrow().get(&watch))
                .is_ok();
        if !is_admitted {
            return None;
        }
        let (reply, answer) = oneshot::channel();
        sampler.send(SampleRequest { watch, reply }).await.ok()?;
        answer.await.ok().flatten()
    }

    fn open(&self, tasks: &StreamTasks) -> TickStreamHandle {
        let (desired, wanted) = watch::channel(BTreeMap::new());
        let (publisher, events) = EventPublisher::channel(tasks);
        let scope = self.scope();
        let followers = tasks.clone();
        tasks.spawn(async move { follow_instances(scope, wanted, publisher, followers).await });
        TickStreamHandle { desired, events }
    }
}

struct EnvironmentWatchValidator {
    environment: Arc<dyn ExecutionEnvironment>,
    authority: HostToolAuthority,
    chat: Uuid,
    instance: InstanceRef,
}

#[async_trait::async_trait]
impl super::TickWatchValidator for EnvironmentWatchValidator {
    async fn revalidate(
        &self,
        chat: Uuid,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        if chat != self.chat || cancel.is_cancelled() {
            return Err("watch admission is no longer valid".into());
        }
        let port = self
            .environment
            .bind(
                ExecutionScope {
                    chat: chat.into(),
                    folder: None,
                    additional_directories: Vec::new(),
                },
                ExecutionAuthority::with_cancellation(chat.into(), cancel),
            )
            .map_err(|error| error.to_string())?;
        port.inspect(ExecutionQuery::Kind(self.instance.clone()))
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    fn host_authority(&self) -> Result<Option<super::WatchHostAuthority>, String> {
        Ok(Some(super::WatchHostAuthority {
            tool_name: TICK_READ.into(),
            authority: Some(self.authority.clone()),
        }))
    }
}
