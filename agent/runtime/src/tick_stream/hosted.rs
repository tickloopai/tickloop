//! Tick Updates of hosted Ticks, which run in the dedicated Tickhost. A watch follows the
//! instance through the data binding its Chat owns, so a Tick whose host was replaced or whose
//! owner ended is never silently rebound. A protocol Tick pushes its updates; a terminal Tick's
//! screen is read when a subscription's period elapses and compared with what that watch last
//! got. The approved watches of each instance reach the daemon as Watch intents, which keep the
//! Tick from idling out.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use serde_json::{Value, json};
use tick_protocol::TickState;
use tickhost::host::{TerminalDescription, TerminalRead, TickAdapter};
use tickhost_connect::{
    hosted::{CONTROL_TIMEOUT, OBSERVATION_FRAME_BYTES},
    session::{SessionLinkState, SessionObservation},
};
use tokio::sync::watch;
use uuid::Uuid;

use super::{
    ConnectionBudget, EventPublisher, InstanceObserver, InstanceRef, PreparedTickWatch,
    ProtocolFeed, RECONNECT_PAUSE, TickConnectionState, TickMetadata, TickObservation,
    TickStreamHandle, TickStreamPort, WatchAdmission, WatchId, connection_state,
    current_observation, follow_instances, observe_protocol,
};
use crate::{
    host_tools::HostToolProvider,
    hosted::{registry::TARGET_GONE, tools::HostedTickTools},
    tick_tool_router::READ_TICK,
};
use tickloop_agent_protocol::{HostToolAuthority, ObservationKind};

/// How soon a Watch intent change the daemon did not take is offered again.
const INTENT_RETRY_PAUSE: Duration = Duration::from_secs(5);

/// What each watch of a terminal Tick last got of its screen: where its next sample is compared.
type Screens = Arc<Mutex<BTreeMap<WatchId, ScreenCursor>>>;

struct ScreenCursor {
    /// The observer that follows the watch now; only it forgets the record when it ends.
    observer: Uuid,
    chat: Uuid,
    instance: InstanceRef,
    sent: Value,
}

pub struct HostedTickStream {
    tools: Arc<HostedTickTools>,
    authorities: watch::Sender<BTreeMap<WatchId, HostToolAuthority>>,
    chats: watch::Sender<BTreeMap<WatchId, Uuid>>,
    /// The (Chat, instance) pairs with a live watch registration, whose binding is therefore
    /// current.
    live: Arc<Mutex<HashSet<(Uuid, InstanceRef)>>>,
    screens: Screens,
    budget: ConnectionBudget,
}

impl HostedTickStream {
    pub fn new(tools: Arc<HostedTickTools>) -> Self {
        Self {
            tools,
            authorities: watch::channel(BTreeMap::new()).0,
            chats: watch::channel(BTreeMap::new()).0,
            live: Arc::default(),
            screens: Arc::default(),
            budget: ConnectionBudget::default(),
        }
    }

    fn scope(&self) -> HostedScope {
        HostedScope {
            tools: self.tools.clone(),
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
    tools: Arc<HostedTickTools>,
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
        if authority != Some(&self.tools.authority()) {
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
    ) {
        let observer = Uuid::new_v4();
        observe_hosted_instance(&self, observer, instance, wanted, publisher).await;
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
            tools: scope.tools.clone(),
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
                    TickAdapter::Protocol => {
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
                    TickAdapter::Terminal => {
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
                if adapter == TickAdapter::Protocol && !wanted.borrow().is_empty() {
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
    tools: Arc<HostedTickTools>,
    chat: Uuid,
    instance: InstanceRef,
}

impl HostedFeed {
    /// Proves the Chat still holds a current binding for the instance before anything is read,
    /// and tells which adapter serves it.
    async fn check_binding(&self) -> Result<TickAdapter, tickhost_connect::host_error::HostError> {
        self.tools.data(self.chat, &self.instance).await?;
        self.tools.adapter(self.chat, &self.instance).await
    }

    /// Registers a host watch of a terminal Tick: what it runs, its screen now, and the state it
    /// ends in, Gone once the Tick ended or Disconnected when the watch is lost.
    async fn watch_terminal(
        &self,
    ) -> Result<
        (
            TerminalDescription,
            TerminalRead,
            tokio::sync::oneshot::Receiver<TickConnectionState>,
        ),
        String,
    > {
        let mut connection = self
            .tools
            .data(self.chat, &self.instance)
            .await
            .map_err(|error| error.to_string())?;
        let description = connection
            .request("describe", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        let description: TerminalDescription =
            serde_json::from_value(description).map_err(|error| error.to_string())?;
        let registered = connection
            .request("watch", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        let read: TerminalRead = serde_json::from_value(registered["read"].clone())
            .map_err(|error| error.to_string())?;
        let (sender, ended) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut sender = sender;
            let state = tokio::select! {
                event = connection.next_event_within(OBSERVATION_FRAME_BYTES) => match event {
                    Ok(Some(event)) if event["event"] == "gone" => TickConnectionState::Gone,
                    Ok(Some(_)) => TickConnectionState::IdentityUncertain,
                    Ok(None) | Err(_) => TickConnectionState::Disconnected,
                },
                _ = sender.closed() => return,
            };
            let _ = sender.send(state);
        });
        Ok((description, read, ended))
    }
}

/// A terminal Tick's screen, read without counting as using the Tick.
async fn read_terminal(
    tools: &HostedTickTools,
    chat: Uuid,
    instance: &InstanceRef,
) -> Result<TerminalRead, String> {
    let read = tools
        .data(chat, instance)
        .await
        .map_err(|error| error.to_string())?
        .request("read", json!({"activity":"automatic"}), CONTROL_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_value(read).map_err(|error| error.to_string())
}

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

/// Follows a terminal Tick for the wanted watches. Nothing is pushed: each watch starts from the
/// screen as it is when it joins, and its subscription samples the screen again at its own
/// frequency; the host's watch only tells when the Tick ends. Returns whether following ended
/// for good rather than for a reconnect.
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
    let (description, mut current, mut ended) = match feed.watch_terminal().await {
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
        .unwrap_or_else(|| tickhost_connect::naming::command_title(&description.command));
    let mut is_current = true;
    loop {
        let targets = scope.admitted(&wanted.borrow_and_update().clone(), instance, publisher);
        if targets.is_empty() {
            return true;
        }
        // A watch an earlier observer of this Tick followed keeps what it last got.
        let (joining, adopted) = {
            let mut screens = screens.lock().unwrap_or_else(PoisonError::into_inner);
            screens.retain(|watch, cursor| cursor.observer != observer || targets.contains(watch));
            let mut adopted = Vec::new();
            for watch in &targets {
                if let Some(cursor) = screens.get_mut(watch)
                    && cursor.observer != observer
                {
                    cursor.observer = observer;
                    adopted.push(*watch);
                }
            }
            let joining = targets
                .iter()
                .filter(|watch| !screens.contains_key(watch))
                .copied()
                .collect::<Vec<_>>();
            (joining, adopted)
        };
        for watch in adopted {
            publisher.ready(watch);
        }
        if !joining.is_empty() {
            if !is_current {
                current = match read_terminal(&feed.tools, feed.chat, instance).await {
                    Ok(read) => read,
                    Err(_) => {
                        publisher.state_to(
                            instance.clone(),
                            TickConnectionState::Disconnected,
                            targets.iter().copied(),
                        );
                        return false;
                    }
                };
            }
            let mut screens = screens.lock().unwrap_or_else(PoisonError::into_inner);
            for watch in joining {
                screens.insert(
                    watch,
                    ScreenCursor {
                        observer,
                        chat: feed.chat,
                        instance: instance.clone(),
                        sent: current.data(),
                    },
                );
                publisher.ready(watch);
            }
        }
        publisher.metadata(
            instance.clone(),
            targets.iter().copied(),
            TickMetadata {
                title: title.clone(),
                kind: ObservationKind::Terminal {
                    truncated: current.truncated,
                },
                updated_at_ms: None,
            },
        );
        publisher.state_to(
            instance.clone(),
            TickConnectionState::Live,
            targets.iter().copied(),
        );
        tokio::select! {
            state = &mut ended => {
                let state = state.unwrap_or(TickConnectionState::Disconnected);
                publisher.state_to(instance.clone(), state, targets.iter().copied());
                return state != TickConnectionState::Disconnected;
            }
            changed = wanted.changed() => if changed.is_err() { return true; },
        }
        is_current = false;
    }
}

#[async_trait::async_trait]
impl ProtocolFeed for HostedFeed {
    async fn descriptor(&self) -> Result<tick_protocol::TickDescriptor, String> {
        let mut connection = self
            .tools
            .data(self.chat, &self.instance)
            .await
            .map_err(|error| error.to_string())?;
        let descriptor = connection
            .request("describe", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        serde_json::from_value(descriptor).map_err(|error| error.to_string())
    }

    /// Registers a host watch: the reply carries the current content, then the host pushes
    /// latest-only observations until the Tick ends or the connection does.
    async fn watch(&self) -> Result<(TickState, u64, watch::Receiver<SessionObservation>), String> {
        let mut connection = self
            .tools
            .data(self.chat, &self.instance)
            .await
            .map_err(|error| error.to_string())?;
        let registered = connection
            .request("watch", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        let baseline: TickState = serde_json::from_value(registered["read"].clone())
            .map_err(|error| error.to_string())?;
        let received = registered["received"]
            .as_u64()
            .ok_or("the Tickhost registered no observation count")?;
        let (sender, receiver) = watch::channel(SessionObservation {
            update: None,
            received,
            state: SessionLinkState::Live,
        });
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    event = connection.next_event_within(OBSERVATION_FRAME_BYTES) => event,
                    _ = sender.closed() => return,
                };
                match event {
                    Ok(Some(event)) => match observation(&event) {
                        Some(observation) => {
                            sender.send_replace(observation);
                        }
                        None if event["event"] == "gone" => {
                            sender.send_modify(|current| current.state = SessionLinkState::Gone);
                            return;
                        }
                        None => {
                            sender.send_modify(|current| {
                                current.state = SessionLinkState::ProtocolError(
                                    "the Tickhost sent a malformed watch event".into(),
                                );
                            });
                            return;
                        }
                    },
                    // Ending the stream without a gone event disconnects the watch.
                    Ok(None) | Err(_) => return,
                }
            }
        });
        Ok((baseline, received, receiver))
    }
}

fn observation(event: &Value) -> Option<SessionObservation> {
    if event["event"] != "observation" {
        return None;
    }
    serde_json::from_value(event["observation"].clone()).ok()
}

/// Keeps the daemon's Watch intents equal to the hosted watches each Chat wants, one change at a
/// time and in order, and renews them before they lapse. A change the daemon did not take is
/// offered again until it is; one whose answer was lost counts as possibly applied, so an ended
/// watch is always withdrawn and never keeps protecting its Tick.
async fn sync_watch_intents(
    scope: HostedScope,
    mut desired: watch::Receiver<BTreeMap<WatchId, InstanceRef>>,
) {
    let mut chats = scope.chats.clone();
    // What the daemon holds per (Chat, instance); `None` when the last report's outcome is unknown.
    let mut sent: BTreeMap<(Uuid, InstanceRef), Option<BTreeSet<WatchId>>> = BTreeMap::new();
    let mut renewal = tokio::time::interval(scope.tools.intent_renewal());
    renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    renewal.tick().await;
    let mut is_renewing = false;
    loop {
        let mut wanted: BTreeMap<(Uuid, InstanceRef), BTreeSet<WatchId>> = sent
            .keys()
            .map(|key| (key.clone(), BTreeSet::new()))
            .collect();
        {
            let desired = desired.borrow_and_update();
            let authorities = scope.authorities.borrow();
            for (watch, instance) in desired.iter() {
                if let Some(chat) = scope.chat_of(watch)
                    && scope.authorizes(authorities.get(watch)).is_ok()
                {
                    wanted
                        .entry((chat, instance.clone()))
                        .or_default()
                        .insert(*watch);
                }
            }
        }
        let mut is_pending = false;
        for (key, watches) in wanted {
            let is_settled = sent
                .get(&key)
                .is_some_and(|current| current.as_ref() == Some(&watches));
            let needs_renewal = is_renewing && !watches.is_empty();
            if is_settled && !needs_renewal {
                continue;
            }
            match scope
                .tools
                .set_watch_intents(key.0, &key.1, watches.clone())
                .await
            {
                // A Tick that is gone needs no protection either way.
                Err(error) if error.code == TARGET_GONE => {
                    sent.remove(&key);
                }
                Err(error) => {
                    if error.dispatched {
                        sent.insert(key, None);
                    }
                    is_pending = true;
                }
                Ok(()) if watches.is_empty() => {
                    sent.remove(&key);
                }
                Ok(()) => {
                    sent.insert(key, Some(watches));
                }
            }
        }
        let retry = async {
            if is_pending {
                tokio::time::sleep(INTENT_RETRY_PAUSE).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        is_renewing = false;
        tokio::select! {
            changed = desired.changed() => if changed.is_err() { return; },
            changed = chats.changed() => if changed.is_err() { return; },
            () = retry => {}
            _ = renewal.tick() => is_renewing = true,
        }
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
        let query = self
            .tools
            .prepare_call(
                chat,
                READ_TICK,
                &json!({"tick_instance_id":instance}),
                cancel,
            )
            .await?;
        let watch = PreparedTickWatch::new(self.tools.clone(), query)?;
        let authority = watch
            .host_authority()?
            .and_then(|binding| binding.authority);
        self.revalidate_watch(chat, instance, authority.as_ref())
            .await?;
        Ok(Some(watch))
    }

    fn set_watch_chats(&self, chats: BTreeMap<WatchId, Uuid>) {
        self.screens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
        if instance.tick_type() != tick_protocol::HOSTED_INSTANCE_TYPE {
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
            self.tools
                .data(chat, instance)
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
        let adapter = self
            .tools
            .adapter(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        if adapter == TickAdapter::Terminal {
            let read = read_terminal(&self.tools, chat, instance).await?;
            return Ok(Some(terminal_observation(instance, read)));
        }
        let mut connection = self
            .tools
            .data(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        let read = connection
            .request("read", json!({"activity":"automatic"}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        let read: TickState = serde_json::from_value(read).map_err(|error| error.to_string())?;
        Ok(Some(current_observation(instance, read)))
    }

    /// Reads a terminal Tick's screen for `watch` while it is still admitted, and reports it when
    /// it differs from what the watch last got, which it then becomes.
    async fn sample(&self, watch: WatchId, instance: &InstanceRef) -> Option<TickObservation> {
        // Once a watch got the program's exit its screen never changes again.
        let chat = self
            .screens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&watch)
            .filter(|cursor| cursor.instance == *instance && cursor.sent["exit"].is_null())?
            .chat;
        let scope = self.scope();
        let is_admitted = scope.chat_of(&watch) == Some(chat)
            && scope
                .authorizes(scope.authorities.borrow().get(&watch))
                .is_ok();
        if !is_admitted {
            return None;
        }
        let read = read_terminal(&self.tools, chat, instance).await.ok()?;
        let observation = terminal_observation(instance, read);
        let mut screens = self.screens.lock().unwrap_or_else(PoisonError::into_inner);
        let cursor = screens.get_mut(&watch)?;
        if cursor.sent == observation.data {
            return None;
        }
        cursor.sent = observation.data.clone();
        Some(observation)
    }

    fn open(&self) -> TickStreamHandle {
        let (desired, wanted) = watch::channel(BTreeMap::new());
        let (publisher, events) = EventPublisher::channel();
        let scope = self.scope();
        tokio::spawn(sync_watch_intents(scope.clone(), wanted.clone()));
        tokio::spawn(async move { follow_instances(scope, wanted, publisher).await });
        TickStreamHandle { desired, events }
    }
}
