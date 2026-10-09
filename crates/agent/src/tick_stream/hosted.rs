//! Tick Updates of hosted Ticks, which run in the dedicated Tick host. A watch follows the
//! instance through the data binding its Chat owns, so a Tick whose host was replaced or whose
//! owner ended is never silently rebound. The approved watches of each instance reach the owner
//! as Watch intents, which keep the Tick from idling out.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use serde_json::{Value, json};
use tick_connect::{
    hosted::{CONTROL_TIMEOUT, OBSERVATION_FRAME_BYTES},
    protocol::TickRead,
    session::{SessionLinkState, SessionObservation},
};
use tokio::sync::watch;
use uuid::Uuid;

use super::{
    EventPublisher, InstanceObserver, InstanceRef, PreparedTickWatch, ProtocolFeed,
    TickConnectionState, TickObservation, TickStreamHandle, TickStreamPort, WatchAdmission,
    WatchId, connection_state, current_observation, follow_instances, observe_protocol,
};
use crate::{
    host_tools::{HostToolAuthority, HostToolProvider},
    hosted::{registry::TARGET_GONE, tools::HostedTickTools},
    tick_tool_router::READ_TICK,
};

/// A watch connects to its Tick once and reconnects at most this often in a process; then it
/// stays Disconnected until the Chat subscribes again.
const RECONNECTS_PER_WATCH: u32 = 3;
const RECONNECT_PAUSE: Duration = Duration::from_secs(5);
/// How soon a Watch intent change the owner did not take is offered again.
const INTENT_RETRY_PAUSE: Duration = Duration::from_secs(5);

pub struct HostedTickStream {
    tools: Arc<HostedTickTools>,
    authorities: watch::Sender<BTreeMap<WatchId, HostToolAuthority>>,
    chats: watch::Sender<BTreeMap<WatchId, Uuid>>,
    /// The (Chat, instance) pairs with a live watch registration, whose binding is therefore
    /// current.
    live: Arc<Mutex<HashSet<(Uuid, InstanceRef)>>>,
    /// Connection attempts per watch in this process.
    connects: Arc<Mutex<BTreeMap<WatchId, u32>>>,
}

impl HostedTickStream {
    pub fn new(tools: Arc<HostedTickTools>) -> Self {
        Self {
            tools,
            authorities: watch::channel(BTreeMap::new()).0,
            chats: watch::channel(BTreeMap::new()).0,
            live: Arc::default(),
            connects: Arc::default(),
        }
    }

    fn scope(&self) -> HostedScope {
        HostedScope {
            tools: self.tools.clone(),
            authorities: self.authorities.subscribe(),
            chats: self.chats.subscribe(),
            live: self.live.clone(),
            connects: self.connects.clone(),
        }
    }
}

#[derive(Clone)]
struct HostedScope {
    tools: Arc<HostedTickTools>,
    authorities: watch::Receiver<BTreeMap<WatchId, HostToolAuthority>>,
    chats: watch::Receiver<BTreeMap<WatchId, Uuid>>,
    live: Arc<Mutex<HashSet<(Uuid, InstanceRef)>>>,
    connects: Arc<Mutex<BTreeMap<WatchId, u32>>>,
}

impl HostedScope {
    /// A hosted watch is admitted only under the authority of this worker's own Tick host.
    fn authorizes(&self, authority: Option<&HostToolAuthority>) -> Result<(), String> {
        let Some(HostToolAuthority::Native { .. }) = authority else {
            return Err("Hosted watch has no admitted host authority; subscribe explicitly".into());
        };
        if authority != Some(&self.tools.authority()) {
            return Err("Hosted watch is outside its admitted Tick host".into());
        }
        Ok(())
    }

    fn chat_of(&self, watch: &WatchId) -> Option<Uuid> {
        self.chats.borrow().get(watch).copied()
    }

    fn mark_live(&self, chat: Uuid, instance: &InstanceRef, is_live: bool) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if is_live {
            live.insert((chat, instance.clone()));
        } else {
            live.remove(&(chat, instance.clone()));
        }
    }

    /// Forgets the connection counts of watches no longer wanted.
    fn forget_connects_except(&self, desired: &BTreeMap<WatchId, InstanceRef>) {
        self.connects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|watch, _| desired.contains_key(watch));
    }

    fn count_connect(&self, watches: &BTreeSet<WatchId>) {
        let mut connects = self.connects.lock().unwrap_or_else(PoisonError::into_inner);
        for watch in watches {
            *connects.entry(*watch).or_default() += 1;
        }
    }

    fn may_connect(&self, watch: &WatchId) -> bool {
        self.connects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(watch)
            .is_none_or(|connects| *connects <= RECONNECTS_PER_WATCH)
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
            .collect()
    }
}

#[async_trait::async_trait]
impl InstanceObserver for HostedScope {
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
        observe_hosted_instance(self, instance, wanted, publisher).await;
    }
}

async fn observe_hosted_instance(
    scope: HostedScope,
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
        let owned = OwnedWatches {
            scope: &scope,
            chat,
        };
        let attempted = owned.admitted(&requested, &instance, &publisher);
        if attempted.is_empty() {
            if wanted.changed().await.is_err() {
                return;
            }
            continue;
        }
        scope.count_connect(&attempted);
        let feed = HostedFeed {
            tools: scope.tools.clone(),
            chat,
            instance: instance.clone(),
        };
        if let Err(error) = feed.check_binding().await {
            let state = connection_state(&error.code);
            publisher.state_to(instance.clone(), state, attempted.iter().copied());
            if matches!(
                state,
                TickConnectionState::Gone | TickConnectionState::IdentityUncertain
            ) {
                return;
            }
        } else {
            scope.mark_live(chat, &instance, true);
            let ended = observe_protocol(
                &feed,
                &instance,
                &mut wanted,
                &publisher,
                &mut ready_watches,
                &owned,
            )
            .await;
            scope.mark_live(chat, &instance, false);
            if ended {
                return;
            }
            if !wanted.borrow().is_empty() {
                publisher.gap_to(instance.clone(), ready_watches.iter().copied());
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

/// The admitted watches of the one Chat that owns the observed instance and that may still
/// connect: only that Chat holds the binding, and a watch that used its reconnects stays
/// Disconnected.
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
            if !self.scope.may_connect(watch) {
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::Disconnected,
                    [*watch],
                );
                return false;
            }
            true
        });
        admitted
    }
}

/// One hosted instance as its owning Chat reaches it through the Tick host.
struct HostedFeed {
    tools: Arc<HostedTickTools>,
    chat: Uuid,
    instance: InstanceRef,
}

impl HostedFeed {
    /// Proves the Chat still holds a current binding for the instance before anything is read.
    async fn check_binding(&self) -> Result<(), tick_connect::host_error::HostError> {
        self.tools.data(self.chat, &self.instance).await.map(drop)
    }
}

#[async_trait::async_trait]
impl ProtocolFeed for HostedFeed {
    async fn recovery_tool(&self) -> Result<Option<String>, String> {
        let mut connection = self
            .tools
            .data(self.chat, &self.instance)
            .await
            .map_err(|error| error.to_string())?;
        let descriptor = connection
            .request("describe", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        Ok(descriptor["recover_with"].as_str().map(str::to_owned))
    }

    /// Registers a host watch: the reply carries the current content, then the host pushes
    /// latest-only observations until the Tick ends or the connection does.
    async fn watch(&self) -> Result<(TickRead, u64, watch::Receiver<SessionObservation>), String> {
        let mut connection = self
            .tools
            .data(self.chat, &self.instance)
            .await
            .map_err(|error| error.to_string())?;
        let registered = connection
            .request("watch", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        let baseline: TickRead = serde_json::from_value(registered["read"].clone())
            .map_err(|error| error.to_string())?;
        let received = registered["received"]
            .as_u64()
            .ok_or("the Tick host registered no observation count")?;
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
                                    "the Tick host sent a malformed watch event".into(),
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

/// Keeps the owner's Watch intents equal to the hosted watches each Chat wants, one change at a
/// time and in order, and renews them before they lapse. A change the owner did not take is
/// offered again until it is; one whose answer was lost counts as possibly applied, so an ended
/// watch is always withdrawn and never keeps protecting its Tick.
async fn sync_watch_intents(
    scope: HostedScope,
    mut desired: watch::Receiver<BTreeMap<WatchId, InstanceRef>>,
) {
    let mut chats = scope.chats.clone();
    // What the owner holds per (Chat, instance); `None` when the last report's outcome is unknown.
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
            scope.forget_connects_except(&desired);
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
        self.chats.send_replace(chats);
    }

    fn set_watch_authorities(&self, authorities: BTreeMap<WatchId, HostToolAuthority>) {
        self.authorities.send_replace(authorities);
    }

    /// A hosted watch holds only while its authority is this worker's Tick host and its Chat
    /// still owns a current binding of the instance: a live registration proves it, otherwise
    /// the owner is asked again.
    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        if instance.tick_type() != tick_connect::instance::HOSTED_TYPE {
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
        let mut connection = self
            .tools
            .data(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        let read = connection
            .request("read", json!({"activity":"automatic"}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        let read: TickRead = serde_json::from_value(read).map_err(|error| error.to_string())?;
        Ok(Some(current_observation(instance, read)))
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
