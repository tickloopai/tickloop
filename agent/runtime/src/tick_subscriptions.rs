//! Tick Subscriptions: the path from a Tick Update to a Loop.
//!
//! A subscription is Trigger configuration, so every mutation goes through
//! [`Agent::configure_trigger`] and inherits its chain entries, resume rules and leases. The
//! Agent keeps the union of every armed subscription's Tick Instance as the desired set of one
//! [`TickStreamPort`]; updates arriving from that stream accumulate per subscription. A Terminal
//! Tick pushes nothing: when its subscription's period elapses, the stream reads its screen once
//! and reports it as an update if it differs from what that subscription last got. When a
//! subscription's period elapses with updates pending, or as soon as an immediate one receives an
//! update, the latest update per Tick Instance merges into the owning Loop's pending delivery and
//! is handed to [`Agent::trigger`] as soon as the Loop can take it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Duration, Instant};

use super::tick_content::{
    DeliveredWatermark, TickContentCache, WATERMARK_DIRECTORY, read_watermark, remove_watermark,
    write_watermark,
};
use super::*;
use crate::tick_stream::{
    TickConnectionState, TickObservation, TickStreamEvent, TickStreamPort, WatchId,
};
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{
    HostToolAuthority, LoopId, LoopSnapshot, LoopState, MAX_DELIVERY_PAYLOAD_BYTES,
    ObservationKind, Receipt, TICK_SUBSCRIPTION_SOURCE, TickLinkState, TickSubscription,
    TickSubscriptionChange, TickSubscriptionFrequency, TickSubscriptionStatus, ToolDefinition,
    ToolResult, Trigger, TriggerSource,
};

const SUBSCRIPTION_READY_TIMEOUT: Duration = Duration::from_secs(10);
const ALREADY_SUBSCRIBED: &str =
    "a Loop of this Chat already subscribes to this Tick; e changes the frequency";
const NO_SUBSCRIBING_LOOP: &str = "no Loop of this Chat can take its Tick Updates";

const SUBSCRIBE_TICK_TOOL: &str = "SubscribeTick";
const UNSUBSCRIBE_TICK_TOOL: &str = "UnsubscribeTick";
const DEFAULT_FREQUENCY: TickSubscriptionFrequency = TickSubscriptionFrequency::High;
/// How soon a delivery its Loop could not take is looked at again, whatever the frequency of the
/// subscriptions it carries.
const HELD_DELIVERY_RETRY: Duration = Duration::from_secs(5);
/// How long one sample of a Terminal Tick's screen may take before its period passes without it.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(5);
/// An item the stream or a later delivery replaces is itself one dropped Tick Update.
const SUPERSEDED_UPDATE: u64 = 1;
const TRUNCATION_HINT: &str = "Tick Update data was left out where `data` is null, because it was too large for this delivery or could not be read again; use the native tools for that instance type to recover it.";
const ARTIFACT_SUBSCRIPTION_REFUSAL: &str = "an artifact takes no Tick Subscription: its association with a goal is its subscription and brings each @tickloop feedback request to that goal. An artifact a goal creates is associated with it; the user associates others by selecting them in /artifacts or with f on their Monitor row.";

pub(super) fn validate(trigger: &Trigger) -> Result<(), String> {
    if trigger.subscriptions.is_empty() {
        return Ok(());
    }
    if !trigger.accepts_events {
        return Err("Tick Subscriptions require a trigger that accepts events".into());
    }
    let mut instances = BTreeSet::new();
    for subscription in &trigger.subscriptions {
        check_frequency(&subscription.instance, subscription.frequency)?;
        if !instances.insert(subscription.instance.clone()) {
            return Err("a Loop holds at most one Tick Subscription per Tick Instance".into());
        }
    }
    Ok(())
}

/// The frequency a Tick type decides for every subscription to it, which no subscriber chooses:
/// a Shell Tick is delivered as soon as it updates. Other types deliver periodically, except an
/// artifact, whose association is its only subscription.
fn fixed_frequency(instance: &InstanceRef) -> Result<Option<TickSubscriptionFrequency>, String> {
    match instance.access() {
        tick_protocol::ARTIFACT_ACCESS => Err(ARTIFACT_SUBSCRIPTION_REFUSAL.into()),
        tick_protocol::SHELL_ACCESS => Ok(Some(TickSubscriptionFrequency::Immediate)),
        _ => Ok(None),
    }
}

fn check_frequency(
    instance: &InstanceRef,
    frequency: TickSubscriptionFrequency,
) -> Result<(), String> {
    match fixed_frequency(instance)? {
        Some(fixed) if fixed != frequency => Err(format!(
            "a Shell Tick Subscription is always {}",
            fixed.name()
        )),
        None if frequency.period().is_none() => Err(format!(
            "only a Shell Tick Subscription is {}; choose {}",
            frequency.name(),
            periodic_choices()
        )),
        _ => Ok(()),
    }
}

fn periodic_choices() -> String {
    TickSubscriptionFrequency::PERIODIC
        .map(TickSubscriptionFrequency::name)
        .join(", ")
}

/// When a subscription next hands over what it holds: a period from `now`, or for an immediate
/// one not until an update arrives.
fn next_delivery(frequency: TickSubscriptionFrequency, now: Instant) -> Option<Instant> {
    frequency.period().map(|period| now + period)
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    let periods = TickSubscriptionFrequency::PERIODIC
        .iter()
        .filter_map(|frequency| {
            frequency
                .period()
                .map(|period| format!("`{}` every {} min", frequency.name(), period.as_secs() / 60))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let frequencies = TickSubscriptionFrequency::PERIODIC.map(TickSubscriptionFrequency::name);
    vec![
        ToolDefinition {
            name: SUBSCRIBE_TICK_TOOL.into(),
            description: format!(
                "Subscribe the current goal to a Tick Instance. A protocol or terminal Tick wakes the goal with that instance's latest Tick Update and how many updates were dropped in between at the chosen frequency: {periods}; `{}` is the default, and a period without updates wakes nothing. A Shell Tick takes no frequency: it updates once, when it exits, and that update is delivered as soon as it arrives; read its output_file for progress before then. An artifact takes no Tick Subscription: its @tickloop requests reach the goal it is associated with. Intermediate updates are not kept, so read older data with the Tick's own query tools.",
                DEFAULT_FREQUENCY.name()
            ),
            input_schema: json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":3},"frequency":{"type":"string","enum":frequencies}},"required":["tick_instance_id"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: UNSUBSCRIBE_TICK_TOOL.into(),
            description: "Cancel a Tick Subscription of the current goal by its subscription id."
                .into(),
            input_schema: json!({"type":"object","properties":{"subscription":{"type":"string","format":"uuid"}},"required":["subscription"],"additionalProperties":false}),
        },
    ]
}

#[derive(Deserialize)]
struct SubscribeTickInput {
    tick_instance_id: InstanceRef,
    frequency: Option<TickSubscriptionFrequency>,
}

/// The Tick Instance a `SubscribeTick` call names and the frequency it gets: a Shell Tick is
/// always immediate and takes no frequency, an artifact takes no subscription, any other type
/// gets the chosen periodic one or the default.
pub(super) fn subscription_request(
    input: &Value,
) -> Result<(InstanceRef, TickSubscriptionFrequency), String> {
    let input: SubscribeTickInput = serde_json::from_value(input.clone())
        .map_err(|error| format!("invalid {SUBSCRIBE_TICK_TOOL} input: {error}"))?;
    let frequency = match (fixed_frequency(&input.tick_instance_id)?, input.frequency) {
        (Some(_), Some(_)) => {
            return Err(
                "a Shell Tick is delivered as soon as it updates and takes no frequency; omit frequency"
                    .into(),
            );
        }
        (Some(fixed), None) => fixed,
        (None, requested) => requested.unwrap_or(DEFAULT_FREQUENCY),
    };
    check_frequency(&input.tick_instance_id, frequency)?;
    Ok((input.tick_instance_id, frequency))
}

#[derive(Deserialize)]
struct UnsubscribeTickInput {
    subscription: Uuid,
}

#[derive(Default)]
pub(super) struct TickDeliveries {
    pub(super) port: Option<Arc<dyn TickStreamPort>>,
    desired: Option<tokio::sync::watch::Sender<BTreeMap<WatchId, InstanceRef>>>,
    subscriptions: BTreeMap<Uuid, SubscriptionRuntime>,
    /// The newest update data of every subscription, within the Agent's content budget.
    content: TickContentCache,
    pending: HashMap<(ChatId, LoopId), PendingDelivery>,
    ready_changed: Arc<tokio::sync::Notify>,
}

struct WatchBinding {
    watch: WatchId,
    instance: InstanceRef,
    authority: Option<HostToolAuthority>,
}

struct SubscriptionRuntime {
    chat_id: ChatId,
    loop_id: LoopId,
    instance: InstanceRef,
    authority: Option<HostToolAuthority>,
    watch: WatchId,
    ready: bool,
    link: TickLinkState,
    blocked: Option<TickLinkState>,
    frequency: TickSubscriptionFrequency,
    /// When it next hands its newest update to the Loop; `None` while an immediate subscription
    /// has nothing new.
    due_at: Option<Instant>,
    /// The newest update since the last delivery, without its data, which waits in the content
    /// cache; earlier ones are counted, not kept.
    latest: Option<LatestUpdate>,
    /// The last delivered update; one at or before it is never delivered again.
    delivered: Option<DeliveredWatermark>,
    /// Where the delivered watermark is kept for a saved Chat.
    watermarks: Option<std::path::PathBuf>,
    dropped: u64,
    accumulated: u64,
    gap: bool,
    last_delivery_ms: Option<u64>,
    last_delivery: Option<String>,
    kind: Option<ObservationKind>,
    title: Option<String>,
    updated_at_ms: Option<u64>,
}

struct LatestUpdate {
    sequence: u64,
    observed_at: String,
    kind: ObservationKind,
    recover_with: Option<String>,
    /// Its data is larger than the content budget and could never fit a delivery either; what
    /// [`left_out`] keeps of it waits in the cache instead.
    is_oversized: bool,
}

#[derive(Default)]
struct PendingDelivery {
    items: BTreeMap<InstanceRef, DeliveryItem>,
    /// The delivery came due while the Loop could not take it, so it flushes when the Loop's run
    /// ends.
    due: bool,
    /// When a held delivery is looked at again, in case the run ended before it was held or the
    /// delivery failed.
    retry_at: Option<Instant>,
}

#[derive(Clone, Serialize)]
struct DeliveryItem {
    subscription: Uuid,
    watch: WatchId,
    instance: InstanceRef,
    sequence: u64,
    observed_at: String,
    dropped: u64,
    gap: bool,
    kind: ObservationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    recover_with: Option<String>,
    data: Value,
    /// The data was evicted from the content cache and is read again before delivery.
    #[serde(skip)]
    needs_refill: bool,
    /// The data was too large to keep or deliver; the delivery says so.
    #[serde(skip)]
    is_oversized: bool,
}

impl DeliveryItem {
    /// This item takes the place of one the Loop has not received yet. That earlier item and
    /// everything it already counted are dropped Tick Updates.
    fn superseding(self, superseded: &Self) -> Self {
        Self {
            dropped: self.dropped + superseded.dropped + SUPERSEDED_UPDATE,
            gap: self.gap || superseded.gap,
            ..self
        }
    }
}

impl TickDeliveries {
    /// A confirmed rename names the instance's subscriptions before its metadata is read again.
    pub(super) fn retitle(&mut self, instance: &InstanceRef, title: &str) {
        for subscription in self
            .subscriptions
            .values_mut()
            .filter(|subscription| subscription.instance == *instance)
        {
            subscription.title = Some(title.to_owned());
        }
    }

    fn apply_event(&mut self, event: TickStreamEvent) {
        match event {
            TickStreamEvent::Metadata {
                instance,
                audience,
                metadata,
            } => {
                for subscription in self.subscriptions.values_mut().filter(|subscription| {
                    audience.contains(&subscription.watch) && subscription.instance == instance
                }) {
                    subscription.kind = Some(metadata.kind.clone());
                    subscription.title = Some(metadata.title.clone());
                    subscription.updated_at_ms =
                        subscription.updated_at_ms.max(metadata.updated_at_ms);
                }
            }
            TickStreamEvent::Ready { watch } => {
                if let Some(subscription) = self
                    .subscriptions
                    .values_mut()
                    .find(|subscription| subscription.watch == watch)
                    && subscription.blocked.is_none()
                {
                    subscription.ready = true;
                    subscription.link = TickLinkState::Live;
                }
            }
            TickStreamEvent::Observation { audience, value } => self.record(&audience, &value),
            TickStreamEvent::Gap { instance, audience } => self.mark_gap(&instance, &audience),
            TickStreamEvent::State {
                instance,
                state,
                audience,
            } => {
                let link = match state {
                    TickConnectionState::Initializing => TickLinkState::Initializing,
                    TickConnectionState::Live => TickLinkState::Live,
                    TickConnectionState::Disconnected => TickLinkState::Disconnected,
                    TickConnectionState::Gone => TickLinkState::InstanceGone,
                    TickConnectionState::IdentityUncertain => TickLinkState::IdentityUncertain,
                };
                for (id, subscription) in
                    self.subscriptions.iter_mut().filter(|(_, subscription)| {
                        subscription.instance == instance && audience.contains(&subscription.watch)
                    })
                {
                    subscription.link = link;
                    if matches!(
                        link,
                        TickLinkState::InstanceGone | TickLinkState::IdentityUncertain
                    ) {
                        subscription.blocked = Some(link);
                        subscription.latest = None;
                        self.content.remove(*id);
                    }
                }
                let blocked: BTreeSet<_> = self
                    .subscriptions
                    .iter()
                    .filter(|(_, runtime)| runtime.blocked.is_some())
                    .map(|(id, _)| *id)
                    .collect();
                for pending in self.pending.values_mut() {
                    pending
                        .items
                        .retain(|_, item| !blocked.contains(&item.subscription));
                }
                self.publish_desired();
            }
        }
        self.ready_changed.notify_waiters();
    }

    fn source_bindings(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        payload: &Value,
        loops: &[LoopSnapshot],
    ) -> Result<Vec<WatchBinding>, AgentError> {
        let trigger = loops
            .iter()
            .find(|goal| {
                goal.id == loop_id && !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
            })
            .and_then(|goal| goal.trigger.as_ref())
            .filter(|trigger| {
                trigger.armed
                    && trigger.accepts_events
                    && trigger.expires_at_ms.is_none_or(|deadline| {
                        deadline > tickloop_agent_base::private_fs::unix_time_milliseconds()
                    })
            })
            .ok_or_else(|| {
                AgentError::Invalid("Tick delivery has no active subscription configuration".into())
            })?;
        let items = payload["items"]
            .as_array()
            .filter(|items| !items.is_empty())
            .ok_or_else(|| AgentError::Invalid("Tick delivery has no watch bindings".into()))?;
        items
            .iter()
            .map(|item| {
                let subscription = serde_json::from_value::<Uuid>(item["subscription"].clone())
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                let watch = serde_json::from_value::<Uuid>(item["watch"].clone())
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                let instance = serde_json::from_value::<InstanceRef>(item["instance"].clone())
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                let runtime = self
                    .subscriptions
                    .get(&subscription)
                    .filter(|runtime| {
                        runtime.watch == watch
                            && runtime.instance == instance
                            && runtime.chat_id == chat_id
                            && runtime.loop_id == loop_id
                            && runtime.blocked.is_none()
                            && trigger.subscriptions.iter().any(|configured| {
                                configured.id == subscription
                                    && configured.instance == runtime.instance
                                    && configured.authority == runtime.authority
                            })
                    })
                    .ok_or_else(|| {
                        AgentError::Invalid("Tick watch is no longer admitted for this Loop".into())
                    })?;
                Ok(WatchBinding {
                    watch: runtime.watch,
                    instance: runtime.instance.clone(),
                    authority: runtime.authority.clone(),
                })
            })
            .collect()
    }

    fn next_due(&self) -> Option<Instant> {
        let retries = self.pending.values().filter_map(|pending| pending.retry_at);
        self.subscriptions
            .values()
            .filter_map(|subscription| subscription.due_at)
            .chain(retries)
            .min()
    }

    fn publish_desired(&self) {
        if let Some(port) = &self.port {
            port.set_watch_authorities(
                self.subscriptions
                    .values()
                    .filter(|subscription| subscription.blocked.is_none())
                    .filter_map(|subscription| {
                        subscription
                            .authority
                            .clone()
                            .map(|authority| (subscription.watch, authority))
                    })
                    .collect(),
            );
            port.set_watch_chats(
                self.subscriptions
                    .values()
                    .filter(|subscription| subscription.blocked.is_none())
                    .map(|subscription| (subscription.watch, subscription.chat_id))
                    .collect(),
            );
        }
        let desired: BTreeMap<WatchId, InstanceRef> = self
            .subscriptions
            .values()
            .filter(|subscription| subscription.blocked.is_none())
            .map(|subscription| (subscription.watch, subscription.instance.clone()))
            .collect();
        if let Some(sender) = &self.desired {
            sender.send_if_modified(|current| {
                if *current == desired {
                    return false;
                }
                *current = desired;
                true
            });
        }
    }

    fn record(&mut self, audience: &[WatchId], observation: &TickObservation) {
        for (id, subscription) in self.subscriptions.iter_mut().filter(|(_, subscription)| {
            subscription.ready
                && subscription.blocked.is_none()
                && audience.contains(&subscription.watch)
                && subscription.instance == observation.instance
        }) {
            subscription.kind = Some(observation.kind.clone());
            // A reconnect that repeats a delivered update delivers nothing again. After a gap
            // the source numbers its updates afresh, so the old watermark no longer applies.
            if observation.gap {
                subscription.delivered = None;
            } else if subscription
                .delivered
                .is_some_and(|delivered| observation.sequence <= delivered.sequence)
            {
                continue;
            }
            let is_kept = self.content.store(*id, observation.data.clone());
            if !is_kept {
                let remainder = left_out(&observation.kind, &observation.data);
                if !remainder.is_null() {
                    self.content.store(*id, remainder);
                }
            }
            let superseded = subscription.latest.replace(LatestUpdate {
                sequence: observation.sequence,
                observed_at: observation.observed_at.clone(),
                kind: observation.kind.clone(),
                recover_with: observation.recover_with.clone(),
                is_oversized: !is_kept,
            });
            let replaced = superseded.map_or(0, |_| SUPERSEDED_UPDATE);
            subscription.dropped = subscription
                .dropped
                .saturating_add(observation.dropped)
                .saturating_add(replaced);
            subscription.accumulated = subscription
                .accumulated
                .saturating_add(1 + observation.dropped);
            subscription.gap |= observation.gap;
            subscription.updated_at_ms = subscription.updated_at_ms.max(Some(
                tickloop_agent_base::private_fs::unix_time_milliseconds(),
            ));
            if subscription.frequency == TickSubscriptionFrequency::Immediate {
                subscription.due_at = Some(Instant::now());
            }
        }
    }

    /// Records a screen sampled for `watch` as its period's update, due now since its period
    /// already elapsed; whether it was taken.
    fn record_sample(&mut self, watch: WatchId, observation: &TickObservation) -> bool {
        self.record(&[watch], observation);
        let mut is_due = false;
        for subscription in self
            .subscriptions
            .values_mut()
            .filter(|subscription| subscription.watch == watch && subscription.latest.is_some())
        {
            subscription.due_at = Some(Instant::now());
            is_due = true;
        }
        is_due
    }

    fn mark_gap(&mut self, instance: &InstanceRef, audience: &[WatchId]) {
        for (id, subscription) in self.subscriptions.iter_mut().filter(|(_, subscription)| {
            &subscription.instance == instance
                && audience.contains(&subscription.watch)
                && subscription.blocked.is_none()
        }) {
            subscription.gap = true;
            if subscription.latest.take().is_some() {
                self.content.remove(*id);
                subscription.dropped = subscription.dropped.saturating_add(1);
            }
        }
    }

    /// Moves every due subscription's newest update into its Loop's pending delivery and returns
    /// the Loops that now have something to flush. A period that elapses without a new update
    /// delivers nothing of its own, but still retries a delivery the Loop is holding.
    fn collect_elapsed(&mut self, now: Instant) -> Vec<(ChatId, LoopId)> {
        let mut elapsed = Vec::new();
        for (id, subscription) in &mut self.subscriptions {
            if subscription.due_at.is_none_or(|due_at| due_at > now) {
                continue;
            }
            subscription.due_at = next_delivery(subscription.frequency, now);
            let item = subscription.latest.take().map(|update| {
                let data = self.content.take(*id);
                DeliveryItem {
                    subscription: *id,
                    watch: subscription.watch,
                    instance: subscription.instance.clone(),
                    sequence: update.sequence,
                    observed_at: update.observed_at,
                    dropped: std::mem::take(&mut subscription.dropped),
                    gap: std::mem::take(&mut subscription.gap),
                    kind: update.kind,
                    recover_with: update.recover_with,
                    needs_refill: data.is_none() && !update.is_oversized,
                    is_oversized: update.is_oversized,
                    data: data.unwrap_or(Value::Null),
                }
            });
            elapsed.push(((subscription.chat_id, subscription.loop_id), item));
        }
        let mut ready = Vec::new();
        for (owner, item) in elapsed {
            if let Some(item) = item {
                let pending = self.pending.entry(owner).or_default();
                match pending.items.get_mut(&item.instance) {
                    Some(pending_item) => *pending_item = item.superseding(pending_item),
                    None => {
                        pending.items.insert(item.instance.clone(), item);
                    }
                }
            }
            let has_items = self
                .pending
                .get(&owner)
                .is_some_and(|pending| !pending.items.is_empty());
            if has_items && !ready.contains(&owner) {
                ready.push(owner);
            }
        }
        for (owner, pending) in &mut self.pending {
            if pending.retry_at.is_some_and(|retry_at| retry_at <= now) {
                pending.retry_at = None;
                if !pending.items.is_empty() && !ready.contains(owner) {
                    ready.push(*owner);
                }
            }
        }
        ready
    }

    fn take_delivery(&mut self, owner: (ChatId, LoopId)) -> Option<Vec<DeliveryItem>> {
        let pending = self.pending.remove(&owner)?;
        if pending.items.is_empty() {
            return None;
        }
        let items: Vec<_> = pending.items.into_values().collect();
        Some(items)
    }

    fn restore(&mut self, owner: (ChatId, LoopId), items: Vec<DeliveryItem>) {
        let pending = self.pending.entry(owner).or_default();
        for item in items {
            pending.items.entry(item.instance.clone()).or_insert(item);
        }
        self.hold(owner, Instant::now());
    }

    /// Keeps `owner`'s pending delivery until its Loop can take it. It flushes when the Loop's
    /// run ends, and is looked at again shortly in case that end came before it was held or the
    /// delivery failed; the schedules of its subscriptions stay as they are.
    fn hold(&mut self, owner: (ChatId, LoopId), now: Instant) {
        let Some(pending) = self.pending.get_mut(&owner) else {
            return;
        };
        pending.due = true;
        let retry_at = now + HELD_DELIVERY_RETRY;
        pending.retry_at = Some(pending.retry_at.map_or(retry_at, |at| at.min(retry_at)));
    }
}

impl Agent {
    /// Installs the Tick observation stream. `SubscribeTick` and `UnsubscribeTick` reach
    /// the model only after a port is installed.
    pub async fn install_tick_stream(&self, port: Arc<dyn TickStreamPort>) {
        let handle = port.open();
        {
            let mut deliveries = self.tick_deliveries();
            deliveries.desired = Some(handle.desired);
            deliveries.port = Some(port);
        }
        self.inner
            .tick_stream_installed
            .store(true, Ordering::SeqCst);
        let task = self.tick_delivery_task(handle.events);
        self.inner.tasks.lock().await.push(task);
        self.sync_tick_subscriptions().await;
    }

    /// Bytes of Tick Update data waiting in this Agent's content cache.
    #[doc(hidden)]
    pub fn tick_content_bytes(&self) -> usize {
        self.tick_deliveries().content.used_bytes()
    }

    pub async fn tick_subscriptions(
        &self,
        chat_id: ChatId,
    ) -> Result<Vec<TickSubscriptionStatus>, AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        let configured: Vec<_> = chat
            .snapshot
            .loops
            .iter()
            .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
            .flat_map(|goal| {
                goal.trigger
                    .iter()
                    .flat_map(|trigger| &trigger.subscriptions)
                    .map(|subscription| (goal.id, goal.state.clone(), subscription.clone()))
            })
            .collect();
        drop(chats);
        let deliveries = self.tick_deliveries();
        let now = Instant::now();
        let now_ms = tickloop_agent_base::private_fs::unix_time_milliseconds();
        Ok(configured
            .into_iter()
            .map(|(loop_id, loop_state, subscription)| {
                let runtime = deliveries.subscriptions.get(&subscription.id);
                let period = subscription.frequency.period();
                // A check already due reads as now until the delivery task reschedules it.
                let next_check_at_ms = runtime
                    .and_then(|runtime| runtime.due_at)
                    .filter(|_| period.is_some())
                    .map(|due_at| {
                        now_ms.saturating_add(
                            due_at.saturating_duration_since(now).as_millis() as u64,
                        )
                    });
                TickSubscriptionStatus {
                    id: subscription.id,
                    loop_id,
                    loop_state,
                    instance: subscription.instance.clone(),
                    frequency: subscription.frequency,
                    accumulated: runtime.map_or(0, |runtime| runtime.accumulated),
                    last_delivery_ms: runtime.and_then(|runtime| runtime.last_delivery_ms),
                    last_delivery: runtime.and_then(|runtime| runtime.last_delivery.clone()),
                    kind: runtime.and_then(|runtime| runtime.kind.clone()),
                    title: runtime.and_then(|runtime| runtime.title.clone()),
                    updated_at_ms: runtime.and_then(|runtime| runtime.updated_at_ms),
                    link: runtime.map_or(TickLinkState::Initializing, |runtime| runtime.link),
                    next_check_at_ms,
                    period_ms: period.map(|period| period.as_millis() as u64),
                }
            })
            .collect())
    }

    /// Changes one subscription's frequency or cancels it, from Monitor or a model tool. The
    /// change is written through the owning Loop's Trigger, which refuses a frequency the Tick
    /// type does not allow.
    pub async fn update_tick_subscription(
        &self,
        chat_id: ChatId,
        subscription_id: Uuid,
        change: TickSubscriptionChange,
    ) -> Result<(), AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        let (loop_id, mut trigger) = chat
            .snapshot
            .loops
            .iter()
            .find_map(|goal| {
                let trigger = goal.trigger.as_ref()?;
                trigger
                    .subscriptions
                    .iter()
                    .any(|subscription| subscription.id == subscription_id)
                    .then(|| (goal.id, trigger.clone()))
            })
            .ok_or(AgentError::NotFound)?;
        drop(chats);
        match change {
            TickSubscriptionChange::Cancel => trigger
                .subscriptions
                .retain(|subscription| subscription.id != subscription_id),
            TickSubscriptionChange::Frequency(frequency) => {
                for subscription in &mut trigger.subscriptions {
                    if subscription.id == subscription_id {
                        subscription.frequency = frequency;
                    }
                }
            }
        }
        self.configure_trigger(chat_id, loop_id, Some(trigger))
            .await
    }

    /// Subscribes the Chat to `instance` at the default frequency because its user asked to on
    /// the instance's Monitor row. The Chat's newest Loop that has not ended receives it; an
    /// instance one of its Loops already subscribes to, or a Chat without such a Loop, is refused
    /// with what to do instead.
    pub async fn subscribe_tick_for_user(
        &self,
        chat_id: ChatId,
        instance: InstanceRef,
    ) -> Result<(), AgentError> {
        let loop_id = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            let open_loops = chat
                .snapshot
                .loops
                .iter()
                .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
                .collect::<Vec<_>>();
            let is_subscribed = open_loops.iter().any(|goal| {
                goal.trigger
                    .iter()
                    .flat_map(|trigger| &trigger.subscriptions)
                    .any(|subscription| subscription.instance == instance)
            });
            if is_subscribed {
                return Err(AgentError::Invalid(ALREADY_SUBSCRIBED.into()));
            }
            open_loops
                .last()
                .map(|goal| goal.id)
                .ok_or_else(|| AgentError::Invalid(NO_SUBSCRIBING_LOOP.into()))?
        };
        self.subscribe_loop_for_user(chat_id, loop_id, instance, DEFAULT_FREQUENCY)
            .await
    }

    /// Ends this Chat's subscriptions to `lost` once `reopened` took its place, and with
    /// `restore` subscribes each of their Loops to `reopened` again at the frequency it had,
    /// admitted as the user's own subscription. The error tells which could not be restored.
    pub async fn replace_tick_subscriptions(
        &self,
        chat_id: ChatId,
        lost: &InstanceRef,
        reopened: &InstanceRef,
        restore: bool,
    ) -> Result<(), AgentError> {
        let subscriptions = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat.snapshot
                .loops
                .iter()
                .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
                .flat_map(|goal| {
                    goal.trigger
                        .iter()
                        .flat_map(|trigger| &trigger.subscriptions)
                        .filter(|subscription| subscription.instance == *lost)
                        .map(|subscription| (goal.id, subscription.id, subscription.frequency))
                })
                .collect::<Vec<_>>()
        };
        let mut refusals = Vec::new();
        for (loop_id, subscription, frequency) in subscriptions {
            self.update_tick_subscription(chat_id, subscription, TickSubscriptionChange::Cancel)
                .await?;
            if restore
                && let Err(error) = self
                    .subscribe_loop_for_user(chat_id, loop_id, reopened.clone(), frequency)
                    .await
            {
                refusals.push(error.to_string());
            }
        }
        if refusals.is_empty() {
            return Ok(());
        }
        Err(AgentError::Invalid(format!(
            "{} of its Tick Subscriptions could not follow: {}",
            refusals.len(),
            refusals.join("; ")
        )))
    }

    /// Subscribes `loop_id` to `instance` at `frequency` on its user's word, as an approved
    /// SubscribeTick would without asking: the watch is admitted under the Chat's current
    /// authority, and a Deny rule for it still refuses it.
    async fn subscribe_loop_for_user(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        instance: InstanceRef,
        frequency: TickSubscriptionFrequency,
    ) -> Result<(), AgentError> {
        check_frequency(&instance, frequency).map_err(AgentError::Invalid)?;
        let port = self
            .tick_deliveries()
            .port
            .clone()
            .ok_or_else(|| AgentError::Invalid("Tick Subscriptions are unavailable".into()))?;
        let input = json!({"tick_instance_id": instance, "frequency": frequency});
        let watch = port
            .prepare_watch(chat_id, &instance, &input, CancellationToken::new())
            .await
            .map_err(AgentError::Invalid)?;
        let (authority, admission) = match watch {
            Some(watch) => (
                watch
                    .host_authority()
                    .map_err(AgentError::Invalid)?
                    .and_then(|binding| binding.authority),
                Some(watch.into_admission(input.clone())),
            ),
            None => (None, None),
        };
        let (policy_name, permission_input) = admission
            .as_ref()
            .map_or((SUBSCRIBE_TICK_TOOL, &input), |admission| {
                (admission.policy_name(), admission.permission_input())
            });
        self.refresh_project_rules(chat_id).await?;
        let mut trigger = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            let session_operation = SessionPermissionOperation::from_normalized(
                policy_name.to_owned(),
                permission_input.clone(),
                super::session_permissions::capture_workspace_scope(
                    &configured_workspace_for_chat(chat),
                )
                .map_err(|error| AgentError::Invalid(error.to_string()))?,
            )
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
            let session = self.inner.session_permissions.lock().await.evaluate(
                PermissionOwner::new(chat_id, loop_id)
                    .map_err(|error| AgentError::Invalid(error.to_string()))?,
                &session_operation,
            );
            let decision = chat.snapshot.policy.explicit_match_with_session(
                policy_name,
                permission_input,
                loop_id,
                session.decision,
            );
            if decision == Some(tickloop_agent_tools::permissions::ExplicitPermissionDecision::Deny)
            {
                return Err(AgentError::Invalid(format!(
                    "a Deny rule for {policy_name} refuses subscribing to {instance}"
                )));
            }
            let goal = chat
                .snapshot
                .loops
                .iter()
                .find(|goal| goal.id == loop_id)
                .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
                .ok_or_else(|| AgentError::Invalid(NO_SUBSCRIBING_LOOP.into()))?;
            goal.trigger.clone().unwrap_or_else(|| Trigger {
                id: Uuid::new_v4(),
                prompt: goal.goal.clone(),
                interval_seconds: None,
                expires_at_ms: None,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            })
        };
        if trigger
            .subscriptions
            .iter()
            .any(|subscription| subscription.instance == instance)
        {
            return Err(AgentError::Invalid(ALREADY_SUBSCRIBED.into()));
        }
        trigger.accepts_events = true;
        trigger.subscriptions.push(TickSubscription {
            id: Uuid::new_v4(),
            instance,
            frequency,
            authority,
        });
        self.configure_trigger(chat_id, loop_id, Some(trigger))
            .await
    }

    pub(super) async fn tick_subscription_tool(
        &self,
        receipt: &Receipt,
        name: &str,
        input: &Value,
        authority: Option<HostToolAuthority>,
    ) -> Result<ToolResult, String> {
        let outcome = match name {
            SUBSCRIBE_TICK_TOOL => self.subscribe_tick(receipt, input, authority).await,
            UNSUBSCRIBE_TICK_TOOL => self.unsubscribe_tick(receipt, input).await,
            _ => Err("unknown Tick Subscription tool".into()),
        };
        Ok(match outcome {
            Ok(content) => ToolResult {
                content,
                is_error: false,
                output_ref: None,
            },
            Err(error) => ToolResult {
                content: error,
                is_error: true,
                output_ref: None,
            },
        })
    }

    async fn subscribe_tick(
        &self,
        receipt: &Receipt,
        input: &Value,
        authority: Option<HostToolAuthority>,
    ) -> Result<String, String> {
        let (instance, frequency) = subscription_request(input)?;
        let chats = self.inner.chats.lock().await;
        let chat = chats
            .get(&receipt.chat_id)
            .ok_or_else(|| "chat is unavailable".to_string())?;
        let goal = chat
            .snapshot
            .loops
            .iter()
            .find(|goal| goal.id == receipt.loop_id)
            .ok_or_else(|| "goal is unavailable".to_string())?;
        let run_cancel = chat
            .run_cancel(receipt.run_id)
            .cloned()
            .ok_or_else(|| "run is unavailable".to_string())?;
        let mut trigger = goal.trigger.clone().unwrap_or_else(|| Trigger {
            id: Uuid::new_v4(),
            prompt: goal.goal.clone(),
            interval_seconds: None,
            expires_at_ms: None,
            accepts_events: true,
            armed: true,
            subscriptions: Vec::new(),
        });
        drop(chats);
        if let Some(existing) = trigger
            .subscriptions
            .iter()
            .find(|subscription| subscription.instance == instance)
        {
            return Err(format!(
                "this goal already subscribes to Tick Instance {} as {}; cancel it with {UNSUBSCRIBE_TICK_TOOL} before subscribing at another frequency",
                existing.instance, existing.id
            ));
        }
        // A Trigger the user disarmed stays disarmed: subscribing is not consent to resume.
        let armed = trigger.armed;
        trigger.accepts_events = true;
        let subscription = TickSubscription {
            id: Uuid::new_v4(),
            instance,
            frequency,
            authority,
        };
        trigger.subscriptions.push(subscription.clone());
        self.configure_trigger(receipt.chat_id, receipt.loop_id, Some(trigger))
            .await
            .map_err(|error| error.to_string())?;
        let (ready, link) = if armed {
            self.wait_for_subscription_ready(subscription.id, &run_cancel)
                .await
        } else {
            (false, TickLinkState::Initializing)
        };
        let mut answer = json!({
            "subscription": subscription.id,
            "tick_instance_id": subscription.instance,
            "frequency": subscription.frequency,
            "armed": armed,
            "ready": ready,
        });
        if !armed {
            answer["note"] = json!(
                "this goal's Trigger is disarmed, so the subscription is configured but delivers nothing until it is armed again"
            );
        } else if !ready {
            answer["link"] = json!(format!("{link:?}"));
            answer["note"] = json!(match link {
                TickLinkState::InstanceGone =>
                    "the Tick Instance is gone; select a new instance explicitly",
                TickLinkState::IdentityUncertain =>
                    "the Tick Instance identity could not be proved; select the target explicitly",
                TickLinkState::Disconnected =>
                    "the Tickhost is disconnected; observation has not started",
                _ =>
                    "the Tick watch did not reach its baseline before the readiness deadline; check subscription status before relying on updates",
            });
        }
        Ok(answer.to_string())
    }

    async fn wait_for_subscription_ready(
        &self,
        subscription_id: Uuid,
        run_cancel: &CancellationToken,
    ) -> (bool, TickLinkState) {
        let deadline = Instant::now() + SUBSCRIPTION_READY_TIMEOUT;
        loop {
            let ready_changed = self.tick_deliveries().ready_changed.clone();
            let notified = ready_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (ready, link, blocked) = {
                let deliveries = self.tick_deliveries();
                match deliveries.subscriptions.get(&subscription_id) {
                    Some(runtime) => (runtime.ready, runtime.link, runtime.blocked.is_some()),
                    None => return (false, TickLinkState::Disconnected),
                }
            };
            if blocked || ready {
                return (ready && !blocked && link == TickLinkState::Live, link);
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => return (false, link),
                _ = run_cancel.cancelled() => return (false, TickLinkState::Disconnected),
                _ = self.inner.shutdown.cancelled() => return (false, TickLinkState::Disconnected),
            }
        }
    }

    async fn unsubscribe_tick(&self, receipt: &Receipt, input: &Value) -> Result<String, String> {
        let input: UnsubscribeTickInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid {UNSUBSCRIBE_TICK_TOOL} input: {error}"))?;
        self.update_tick_subscription(
            receipt.chat_id,
            input.subscription,
            TickSubscriptionChange::Cancel,
        )
        .await
        .map_err(|error| match error {
            AgentError::NotFound => format!("subscription {} does not exist", input.subscription),
            error => error.to_string(),
        })?;
        Ok(json!({"subscription": input.subscription, "cancelled": true}).to_string())
    }

    /// Reconciles the runtime with the Trigger configuration of every loaded Chat and republishes
    /// the desired set. Called after any change that can arm, disarm or end a Loop.
    pub(super) async fn sync_tick_subscriptions(&self) {
        if !self.inner.tick_stream_installed.load(Ordering::SeqCst) {
            return;
        }
        let saves_chats = self.inner.recorder.is_some();
        let (configured, existing) = {
            let chats = self.inner.chats.lock().await;
            let configured: Vec<_> = chats
                .iter()
                .flat_map(|(chat_id, chat)| {
                    let watermarks = saves_chats.then(|| {
                        chat.tools
                            .chat_directory()
                            .join(WATERMARK_DIRECTORY)
                            .path()
                            .to_path_buf()
                    });
                    chat.snapshot
                        .loops
                        .iter()
                        .filter(|goal| {
                            goal.trigger.as_ref().is_some_and(|trigger| trigger.armed)
                                && !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
                        })
                        .flat_map(|goal| {
                            goal.trigger
                                .iter()
                                .flat_map(|trigger| &trigger.subscriptions)
                                .map(|subscription| {
                                    (*chat_id, goal.id, subscription.clone(), watermarks.clone())
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            // A disarmed or stopped Loop keeps its subscriptions' delivered watermarks; only a
            // subscription no Loop configures any more is cancelled.
            let existing: BTreeSet<Uuid> = chats
                .values()
                .flat_map(|chat| &chat.snapshot.loops)
                .filter_map(|goal| goal.trigger.as_ref())
                .flat_map(|trigger| &trigger.subscriptions)
                .map(|subscription| subscription.id)
                .collect();
            (configured, existing)
        };
        // Watermark files are read and removed outside the delivery lock.
        let mut cancelled = Vec::new();
        let mut restored = Vec::new();
        {
            let mut deliveries = self.tick_deliveries();
            let now = Instant::now();
            let kept: BTreeSet<_> = configured
                .iter()
                .map(|(_, _, subscription, _)| subscription.id)
                .collect();
            let removed: Vec<_> = deliveries
                .subscriptions
                .iter()
                .filter(|(id, runtime)| {
                    !(kept.contains(*id)
                        && configured
                            .iter()
                            .any(|(chat_id, loop_id, subscription, _)| {
                                subscription.id == **id
                                    && subscription.instance == runtime.instance
                                    && subscription.authority == runtime.authority
                                    && *chat_id == runtime.chat_id
                                    && *loop_id == runtime.loop_id
                            }))
                })
                .map(|(id, runtime)| (*id, runtime.watermarks.clone()))
                .collect();
            for (id, watermarks) in removed {
                deliveries.subscriptions.remove(&id);
                deliveries.content.remove(id);
                if !existing.contains(&id)
                    && let Some(directory) = watermarks
                {
                    cancelled.push((directory, id));
                }
            }
            let owners: BTreeSet<_> = configured
                .iter()
                .map(|(chat_id, loop_id, _, _)| (*chat_id, *loop_id))
                .collect();
            let active_watches: BTreeMap<_, _> = deliveries
                .subscriptions
                .iter()
                .map(|(id, runtime)| (*id, runtime.watch))
                .collect();
            deliveries.pending.retain(|owner, pending| {
                pending
                    .items
                    .retain(|_, item| active_watches.get(&item.subscription) == Some(&item.watch));
                owners.contains(owner) && !pending.items.is_empty()
            });
            for (chat_id, loop_id, subscription, watermarks) in configured {
                match deliveries.subscriptions.get_mut(&subscription.id) {
                    Some(runtime) if runtime.frequency != subscription.frequency => {
                        runtime.frequency = subscription.frequency;
                        runtime.due_at = next_delivery(subscription.frequency, now);
                    }
                    Some(_) => {}
                    None => {
                        if let Some(directory) = &watermarks {
                            restored.push((subscription.id, directory.clone()));
                        }
                        deliveries.subscriptions.insert(
                            subscription.id,
                            SubscriptionRuntime {
                                chat_id,
                                loop_id,
                                instance: subscription.instance.clone(),
                                authority: subscription.authority.clone(),
                                watch: Uuid::new_v4(),
                                ready: false,
                                link: TickLinkState::Initializing,
                                blocked: None,
                                frequency: subscription.frequency,
                                due_at: next_delivery(subscription.frequency, now),
                                latest: None,
                                delivered: None,
                                watermarks,
                                dropped: 0,
                                accumulated: 0,
                                gap: false,
                                last_delivery_ms: None,
                                last_delivery: None,
                                kind: None,
                                title: None,
                                updated_at_ms: None,
                            },
                        );
                    }
                }
            }
            deliveries.publish_desired();
            deliveries.ready_changed.notify_waiters();
        }
        if !cancelled.is_empty() || !restored.is_empty() {
            // A watermark that cannot be read is treated as none: a corrupt record must not
            // block deliveries, at the cost of possibly repeating the last one. A cancelled
            // watermark that cannot be removed only leaves a stale file behind.
            let loaded = tokio::task::spawn_blocking(move || {
                for (directory, subscription) in cancelled {
                    let _ = remove_watermark(&directory, subscription);
                }
                restored
                    .into_iter()
                    .filter_map(|(subscription, directory)| {
                        read_watermark(&directory, subscription)
                            .ok()
                            .flatten()
                            .map(|watermark| (subscription, watermark))
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let mut deliveries = self.tick_deliveries();
            for (subscription, watermark) in loaded {
                if let Some(runtime) = deliveries.subscriptions.get_mut(&subscription)
                    && runtime.delivered.is_none()
                {
                    runtime.delivered = Some(watermark);
                }
            }
        }
        self.inner.tick_wake.notify_one();
        self.discard_invalid_pending_tick_inputs().await;
    }

    /// Flushes a Loop's pending delivery that came due while the Loop could not take it.
    pub(super) fn flush_due_tick_delivery(&self, chat_id: ChatId, loop_id: LoopId) {
        if !self.inner.tick_stream_installed.load(Ordering::SeqCst)
            || !self
                .tick_deliveries()
                .pending
                .get(&(chat_id, loop_id))
                .is_some_and(|pending| pending.due)
        {
            return;
        }
        let agent = self.clone();
        tokio::spawn(async move { agent.deliver(chat_id, loop_id).await });
    }

    /// Hands one Loop's pending delivery to [`Agent::trigger`], or marks it due while the Loop
    /// cannot take it, as when it executes or waits for the user, so it flushes once the Loop's
    /// run ends instead of bypassing the user's answer.
    async fn deliver(&self, chat_id: ChatId, loop_id: LoopId) {
        let owner = (chat_id, loop_id);
        // The end of a run sets the Loop's state and flushes a held delivery under the Chats
        // lock, so the state is read and the delivery taken or held under it too: a run that ends
        // meanwhile either comes first and leaves the Loop free to take it, or comes after and
        // finds it held.
        let delivery = {
            let chats = self.inner.chats.lock().await;
            let Some(goal) = chats
                .get(&chat_id)
                .and_then(|chat| chat.snapshot.loops.iter().find(|goal| goal.id == loop_id))
            else {
                return;
            };
            let mut deliveries = self.tick_deliveries();
            match goal.state {
                LoopState::WaitingTrigger | LoopState::Completed => deliveries.take_delivery(owner),
                _ => {
                    deliveries.hold(owner, Instant::now());
                    None
                }
            }
        };
        let Some(items) = delivery else {
            // A held delivery may have set a retry earlier than the delivery task's timer.
            self.inner.tick_wake.notify_one();
            return;
        };
        let mut authorized_items = Vec::new();
        let port = self.tick_deliveries().port.clone();
        for item in items {
            let authority = self
                .tick_deliveries()
                .subscriptions
                .get(&item.subscription)
                .and_then(|runtime| runtime.authority.clone());
            let admitted = match &port {
                Some(port) => port
                    .revalidate_watch(chat_id, &item.instance, authority.as_ref())
                    .await
                    .is_ok(),
                None => false,
            };
            if admitted {
                authorized_items.push(item);
            } else {
                self.apply_stream_event(TickStreamEvent::State {
                    instance: item.instance,
                    state: TickConnectionState::IdentityUncertain,
                    audience: vec![item.watch],
                })
                .await;
            }
        }
        let mut items = Vec::new();
        // Data the content cache evicted is read again now, between two revalidations of its
        // watch; a Tick that cannot be read again is delivered without it.
        for mut item in authorized_items {
            if item.needs_refill {
                let authority = self
                    .tick_deliveries()
                    .subscriptions
                    .get(&item.subscription)
                    .and_then(|runtime| runtime.authority.clone());
                let current = match &port {
                    Some(port) => port.read_current(chat_id, &item.instance).await,
                    None => Ok(None),
                };
                if let Ok(Some(current)) = current
                    && current.sequence >= item.sequence
                {
                    item.dropped += current.sequence - item.sequence;
                    item.sequence = current.sequence;
                    item.observed_at = current.observed_at;
                    item.data = current.data;
                    item.needs_refill = false;
                }
                let still_admitted = match &port {
                    Some(port) => port
                        .revalidate_watch(chat_id, &item.instance, authority.as_ref())
                        .await
                        .is_ok(),
                    None => false,
                };
                if !still_admitted {
                    self.apply_stream_event(TickStreamEvent::State {
                        instance: item.instance,
                        state: TickConnectionState::IdentityUncertain,
                        audience: vec![item.watch],
                    })
                    .await;
                    continue;
                }
            }
            items.push(item);
        }
        let marks = {
            let deliveries = self.tick_deliveries();
            items.retain(|item| {
                deliveries
                    .subscriptions
                    .get(&item.subscription)
                    .and_then(|runtime| runtime.delivered)
                    .is_none_or(|delivered| item.sequence > delivered.sequence)
            });
            let delivery = Uuid::new_v4();
            items
                .iter()
                .map(|item| {
                    let watermarks = deliveries
                        .subscriptions
                        .get(&item.subscription)
                        .and_then(|runtime| runtime.watermarks.clone());
                    (
                        item.subscription,
                        watermarks,
                        DeliveredWatermark {
                            sequence: item.sequence,
                            delivery,
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        if items.is_empty() {
            return;
        }
        // The watermark is durable before the delivery is handed on, so a repeat of this update
        // is never delivered again, even by a later process.
        let durable = marks.clone();
        let written = tokio::task::spawn_blocking(move || {
            durable
                .iter()
                .filter_map(|(subscription, directory, mark)| {
                    directory
                        .as_ref()
                        .map(|directory| (directory, subscription, mark))
                })
                .try_for_each(|(directory, subscription, mark)| {
                    write_watermark(directory, *subscription, *mark)
                })
        })
        .await;
        if !matches!(written, Ok(Ok(()))) {
            self.tick_deliveries().restore(owner, items);
            self.inner.tick_wake.notify_one();
            return;
        }
        {
            let mut deliveries = self.tick_deliveries();
            for (subscription, _, mark) in marks {
                if let Some(runtime) = deliveries.subscriptions.get_mut(&subscription) {
                    runtime.delivered = Some(mark);
                }
            }
        }
        let (payload, truncated) = fit_delivery_payload(&items);
        let source = TriggerSource {
            source: TICK_SUBSCRIPTION_SOURCE.into(),
            payload,
        };
        let outcome = self.trigger(chat_id, loop_id, source).await;
        let mut deliveries = self.tick_deliveries();
        match outcome {
            Ok(_) => {
                let delivered_at = tickloop_agent_base::private_fs::unix_time_milliseconds();
                let summary = delivery_summary(&items, truncated);
                for item in &items {
                    if let Some(runtime) = deliveries.subscriptions.get_mut(&item.subscription) {
                        runtime.accumulated = 0;
                        runtime.last_delivery_ms = Some(delivered_at);
                        runtime.last_delivery = Some(summary.clone());
                    }
                }
            }
            Err(_) => {
                deliveries.restore(owner, items);
                self.inner.tick_wake.notify_one();
            }
        }
    }

    pub(super) fn validate_tick_source_bindings(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        payload: &Value,
        loops: &[LoopSnapshot],
    ) -> Result<(), AgentError> {
        self.tick_deliveries()
            .source_bindings(chat_id, loop_id, payload, loops)
            .map(|_| ())
    }

    pub(super) async fn revalidate_tick_source(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        payload: &Value,
    ) -> Result<(), AgentError> {
        let (bindings, loops) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            let loops = chat.snapshot.loops.clone();
            let bindings = self
                .tick_deliveries()
                .source_bindings(chat_id, loop_id, payload, &loops)?;
            (bindings, loops)
        };
        let port = self
            .tick_deliveries()
            .port
            .clone()
            .ok_or_else(|| AgentError::Invalid("Tick stream is unavailable".into()))?;
        for binding in bindings {
            if let Err(error) = port
                .revalidate_watch(chat_id, &binding.instance, binding.authority.as_ref())
                .await
            {
                self.tick_deliveries().apply_event(TickStreamEvent::State {
                    instance: binding.instance,
                    state: TickConnectionState::IdentityUncertain,
                    audience: vec![binding.watch],
                });
                return Err(AgentError::Invalid(error));
            }
        }
        self.tick_deliveries()
            .source_bindings(chat_id, loop_id, payload, &loops)?;
        Ok(())
    }

    pub(super) fn discard_invalid_tick_inputs(&self, chat: &mut Chat) -> bool {
        let deliveries = self.tick_deliveries();
        let mut discarded = Vec::new();
        let loops = &chat.snapshot.loops;
        chat.snapshot.pending_inputs.retain(|pending| {
            let valid = pending.trigger_source.as_ref().is_none_or(|source| {
                source.source != TICK_SUBSCRIPTION_SOURCE
                    || deliveries
                        .source_bindings(
                            pending.receipt.chat_id,
                            pending.receipt.loop_id,
                            &source.payload,
                            loops,
                        )
                        .is_ok()
            });
            if !valid {
                discarded.push(pending.receipt.clone());
            }
            valid
        });
        if discarded.is_empty() {
            return false;
        }
        for receipt in discarded {
            self.finish_discarded_trigger_input(chat, &receipt);
        }
        true
    }

    pub(super) fn finish_discarded_trigger_input(&self, chat: &mut Chat, receipt: &Receipt) {
        self.inner
            .host_contexts
            .lock()
            .unwrap()
            .runs
            .remove(&receipt.run_id);
        if !chat
            .snapshot
            .pending_inputs
            .iter()
            .any(|pending| pending.receipt.loop_id == receipt.loop_id)
            && let Some(goal) = chat
                .snapshot
                .loops
                .iter_mut()
                .find(|goal| goal.id == receipt.loop_id && goal.state == LoopState::Queued)
        {
            goal.set_state(
                if goal.trigger.as_ref().is_some_and(|trigger| trigger.armed)
                    || goal
                        .event_sources
                        .iter()
                        .any(|source| !source.resume_required)
                {
                    LoopState::WaitingTrigger
                } else {
                    LoopState::Completed
                },
            );
        }
    }

    pub(super) async fn discard_invalid_pending_tick_inputs(&self) {
        let mut chats = self.inner.chats.lock().await;
        for chat in chats.values_mut() {
            if self.discard_invalid_tick_inputs(chat) {
                self.save(&chat.snapshot);
            }
        }
    }

    pub(super) fn tick_deliveries(&self) -> std::sync::MutexGuard<'_, TickDeliveries> {
        self.inner
            .tick_deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Owns the stream events and every subscription's delivery schedule.
    fn tick_delivery_task(
        &self,
        mut events: tokio::sync::mpsc::Receiver<TickStreamEvent>,
    ) -> tokio::task::JoinHandle<()> {
        let agent = self.clone();
        tokio::spawn(async move {
            loop {
                let next_due = agent.tick_deliveries().next_due();
                tokio::select! {
                    biased;
                    _ = agent.inner.shutdown.cancelled() => return,
                    _ = agent.inner.tick_wake.notified() => {}
                    event = events.recv() => match event {
                        Some(event) => agent.apply_stream_event(event).await,
                        None => return,
                    },
                    _ = async {
                        match next_due {
                            Some(due_at) => tokio::time::sleep_until(due_at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        let now = Instant::now();
                        agent.sample_due_screens(now);
                        let ready = agent.tick_deliveries().collect_elapsed(now);
                        for (chat_id, loop_id) in ready {
                            agent.deliver(chat_id, loop_id).await;
                        }
                    }
                }
            }
        })
    }

    /// Reads, side by side and away from the delivery task, the screen of every Terminal Tick
    /// whose subscription's period elapsed by `now`: a screen that differs from what the watch last
    /// got is that period's update, delivered as soon as it is read.
    fn sample_due_screens(&self, now: Instant) {
        let (port, due) = {
            let deliveries = self.tick_deliveries();
            let due = deliveries
                .subscriptions
                .values()
                .filter(|subscription| {
                    subscription.ready
                        && subscription.blocked.is_none()
                        && subscription.frequency.period().is_some()
                        && subscription.due_at.is_some_and(|due_at| due_at <= now)
                })
                .map(|subscription| (subscription.watch, subscription.instance.clone()))
                .collect::<Vec<_>>();
            (deliveries.port.clone(), due)
        };
        let Some(port) = port.filter(|_| !due.is_empty()) else {
            return;
        };
        let agent = self.clone();
        tokio::spawn(async move {
            let mut samples = tokio::task::JoinSet::new();
            for (watch, instance) in due {
                let port = port.clone();
                samples.spawn(async move {
                    tokio::time::timeout(SAMPLE_TIMEOUT, port.sample(watch, &instance))
                        .await
                        .ok()
                        .flatten()
                        .map(|observation| (watch, observation))
                });
            }
            loop {
                let sampled = tokio::select! {
                    _ = agent.inner.shutdown.cancelled() => return,
                    sampled = samples.join_next() => match sampled {
                        Some(sampled) => sampled,
                        None => return,
                    },
                };
                if let Ok(Some((watch, observation))) = sampled
                    && agent.tick_deliveries().record_sample(watch, &observation)
                {
                    agent.inner.tick_wake.notify_one();
                }
            }
        });
    }

    async fn apply_stream_event(&self, event: TickStreamEvent) {
        let invalidates_watch = matches!(
            &event,
            TickStreamEvent::State {
                state: TickConnectionState::Gone | TickConnectionState::IdentityUncertain,
                ..
            }
        );
        self.tick_deliveries().apply_event(event);
        if invalidates_watch {
            self.discard_invalid_pending_tick_inputs().await;
        }
    }
}

fn delivery_summary(items: &[DeliveryItem], truncated: bool) -> String {
    let lines = items
        .iter()
        .map(|item| match item.dropped {
            0 => format!("{} #{}", item.instance, item.sequence),
            dropped => format!("{} #{} · dropped {dropped}", item.instance, item.sequence),
        })
        .collect::<Vec<_>>()
        .join(", ");
    match truncated {
        true => format!("{lines} · truncated"),
        false => lines,
    }
}

fn delivery_payload(items: &[DeliveryItem], truncated: bool) -> Value {
    let unreadable = items.iter().filter(|item| item.needs_refill).count();
    let mut payload = json!({"items": items, "truncated": truncated});
    if unreadable > 0 {
        payload["unreadable_instances"] = json!(unreadable);
    }
    if truncated || unreadable > 0 {
        payload["recovery"] = json!(if items
            .iter()
            .any(|item| matches!(item.kind, ObservationKind::Terminal { .. }))
        {
            "A Terminal Tick's screen was too large for this delivery; use ReadTick for its current screen. Output that scrolled past between periods cannot be recovered.".to_owned()
        } else if let Some(tool) = items.iter().find_map(|item| item.recover_with.as_deref()) {
            if tool.len() <= 128 {
                format!(
                    "Tick Update data was left out where `data` is null; recovery guidance from the instance: {tool}. Use Read on Bash output_file for shells, or CallTickTool for a protocol Tick query."
                )
            } else {
                TRUNCATION_HINT.to_owned()
            }
        } else {
            TRUNCATION_HINT.to_owned()
        });
    }
    payload
}

/// What a delivery says of Tick Update `data` it has no room for: nothing, except how a Terminal
/// Tick's program ended, which leaving out its screen text must not lose.
fn left_out(kind: &ObservationKind, data: &Value) -> Value {
    match kind {
        ObservationKind::Terminal { .. } if !data["exit"].is_null() => {
            json!({"text": null, "exit": data["exit"]})
        }
        _ => Value::Null,
    }
}

/// Keeps a delivery within [`MAX_DELIVERY_PAYLOAD_BYTES`] by blanking the largest Tick Update
/// `data` first, so the model still learns which Tick Instances moved and can fetch the rest.
fn fit_delivery_payload(items: &[DeliveryItem]) -> (Value, bool) {
    let has_oversized = items.iter().any(|item| item.is_oversized);
    let mut payload = delivery_payload(items, has_oversized);
    if payload.to_string().len() <= MAX_DELIVERY_PAYLOAD_BYTES {
        return (payload, has_oversized);
    }
    let mut delivered = items.to_vec();
    let mut largest_first: Vec<_> = (0..delivered.len()).collect();
    largest_first.sort_by_key(|index| std::cmp::Reverse(delivered[*index].data.to_string().len()));
    for index in largest_first {
        delivered[index].data = left_out(&delivered[index].kind, &delivered[index].data);
        payload = delivery_payload(&delivered, true);
        if payload.to_string().len() <= MAX_DELIVERY_PAYLOAD_BYTES {
            return (payload, true);
        }
    }
    let recovery = payload["recovery"].clone();
    let mut omitted = 0;
    while payload.to_string().len() > MAX_DELIVERY_PAYLOAD_BYTES && !delivered.is_empty() {
        delivered.pop();
        omitted += 1;
        payload = delivery_payload(&delivered, true);
        payload["recovery"] = recovery.clone();
        payload["omitted_instances"] = json!(omitted);
    }
    (payload, true)
}
