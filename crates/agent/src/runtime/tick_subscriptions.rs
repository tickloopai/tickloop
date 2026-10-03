//! Tick Subscriptions: the path from a Tick Update to a Loop.
//!
//! A subscription is Trigger configuration, so every mutation goes through
//! [`Agent::configure_trigger`] and inherits its chain entries, resume rules and leases. The
//! Agent keeps the union of every armed subscription's Tick Instance as the desired set of one
//! [`TickStreamPort`]; updates arriving from that stream accumulate per subscription. When a
//! subscription's cadence elapses with updates pending, the latest update per Tick Instance
//! merges into the owning Loop's pending delivery and is handed to [`Agent::trigger`] as soon as
//! the Loop can take it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Duration, Instant};

use super::*;
use crate::model::ToolDefinition;
use crate::tick_stream::{
    ObservationKind, TickConnectionState, TickObservation, TickStreamEvent, TickStreamPort, WatchId,
};
use crate::tools::ToolResult;
use tick_connect::instance::InstanceRef;

pub const TICK_SUBSCRIPTION_SOURCE: &str = "tick_subscription";
pub const DEFAULT_CADENCE_SECONDS: u64 = 60;
pub const MIN_CADENCE_SECONDS: u64 = 5;
pub const MAX_CADENCE_SECONDS: u64 = 86_400;
/// The most one delivery may add to the model's context. A larger one loses its biggest Tick
/// Update data first and tells the model which tool brings it back.
pub const MAX_DELIVERY_PAYLOAD_BYTES: usize = 16 * 1024;
const SUBSCRIPTION_READY_TIMEOUT: Duration = Duration::from_secs(10);

const SUBSCRIBE_TICK_TOOL: &str = "SubscribeTick";
const UNSUBSCRIBE_TICK_TOOL: &str = "UnsubscribeTick";
/// An item the stream or a later delivery replaces is itself one dropped Tick Update.
const SUPERSEDED_UPDATE: u64 = 1;
const TRUNCATION_HINT: &str = "Tick Update data was too large for this delivery and was left out where `data` is null; read it from the Tick Instance with CallTickTool.";

/// One Loop's association with a Tick Instance: it receives that instance's Tick Updates, and the
/// latest one is handed to the Agent every `every_seconds`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TickSubscription {
    pub id: Uuid,
    pub instance: InstanceRef,
    pub every_seconds: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TickSubscriptionChange {
    Cadence(u64),
    Cancel,
}

/// Whether a subscription currently reaches its Tick Instance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TickLinkState {
    #[default]
    Initializing,
    Live,
    Disconnected,
    InstanceGone,
    IdentityUncertain,
}

/// One row of the Chat's subscription view.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TickSubscriptionStatus {
    pub id: Uuid,
    pub loop_id: LoopId,
    pub loop_state: LoopState,
    pub instance: InstanceRef,
    pub every_seconds: u64,
    /// Tick Updates accumulated since the last delivery.
    pub accumulated: u64,
    pub last_delivery_ms: Option<u64>,
    pub last_delivery: Option<String>,
    #[serde(default)]
    pub kind: Option<ObservationKind>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    pub link: TickLinkState,
}

pub(super) fn validate(trigger: &Trigger) -> Result<(), String> {
    if trigger.subscriptions.is_empty() {
        return Ok(());
    }
    if !trigger.accepts_events {
        return Err("Tick Subscriptions require a trigger that accepts events".into());
    }
    let mut instances = BTreeSet::new();
    for subscription in &trigger.subscriptions {
        if !(MIN_CADENCE_SECONDS..=MAX_CADENCE_SECONDS).contains(&subscription.every_seconds) {
            return Err(format!(
                "a Tick Subscription cadence must be {MIN_CADENCE_SECONDS} to {MAX_CADENCE_SECONDS} seconds"
            ));
        }
        if !instances.insert(subscription.instance.clone()) {
            return Err("a Loop holds at most one Tick Subscription per Tick Instance".into());
        }
    }
    Ok(())
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: SUBSCRIBE_TICK_TOOL.into(),
            description: format!(
                "Subscribe the current goal to a Tick Instance. Every {DEFAULT_CADENCE_SECONDS} seconds by default the goal is woken with that instance's latest Tick Update and how many updates were dropped in between; a quiet cadence wakes nothing. Intermediate updates are not kept, so read older data with the Tick's own query tools."
            ),
            input_schema: json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":3},"every_seconds":{"type":"integer","minimum":MIN_CADENCE_SECONDS,"maximum":MAX_CADENCE_SECONDS}},"required":["tick_instance_id"],"additionalProperties":false}),
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
    every_seconds: Option<u64>,
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
    pending: HashMap<(ChatId, LoopId), PendingDelivery>,
    ready_changed: Arc<tokio::sync::Notify>,
}

struct SubscriptionRuntime {
    chat_id: ChatId,
    loop_id: LoopId,
    instance: InstanceRef,
    watch: WatchId,
    ready: bool,
    link: TickLinkState,
    blocked: Option<TickLinkState>,
    every_seconds: u64,
    due_at: Instant,
    /// The newest update since the last delivery; earlier ones are counted, not kept.
    latest: Option<TickObservation>,
    dropped: u64,
    accumulated: u64,
    gap: bool,
    last_delivery_ms: Option<u64>,
    last_delivery: Option<String>,
    kind: Option<ObservationKind>,
    title: Option<String>,
    updated_at_ms: Option<u64>,
}

#[derive(Default)]
struct PendingDelivery {
    items: BTreeMap<InstanceRef, DeliveryItem>,
    /// A cadence elapsed while the Loop was executing, so the delivery flushes the moment it can.
    due: bool,
}

/// One flush: what the model receives, and what the Loop keeps hold of if the trigger fails.
struct Delivery {
    payload: Value,
    items: Vec<DeliveryItem>,
    truncated: bool,
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
    fn next_due(&self) -> Option<Instant> {
        self.subscriptions
            .values()
            .map(|subscription| subscription.due_at)
            .min()
    }

    fn publish_desired(&self) {
        if let Some(port) = &self.port {
            port.set_watch_chats(
                self.subscriptions
                    .values()
                    .filter(|subscription| subscription.blocked.is_none())
                    .map(|subscription| (subscription.watch, subscription.chat_id))
                    .collect(),
            );
            port.set_cadences(
                self.subscriptions
                    .values()
                    .filter(|subscription| subscription.blocked.is_none())
                    .map(|subscription| (subscription.watch, subscription.every_seconds))
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
        for subscription in self.subscriptions.values_mut().filter(|subscription| {
            subscription.ready
                && subscription.blocked.is_none()
                && audience.contains(&subscription.watch)
                && subscription.instance == observation.instance
        }) {
            subscription.kind = Some(observation.kind.clone());
            let superseded = subscription.latest.replace(observation.clone());
            let replaced = superseded.map_or(0, |_| SUPERSEDED_UPDATE);
            subscription.dropped = subscription
                .dropped
                .saturating_add(observation.dropped)
                .saturating_add(replaced);
            subscription.accumulated = subscription
                .accumulated
                .saturating_add(1 + observation.dropped);
            subscription.gap |= observation.gap;
        }
    }

    fn mark_gap(&mut self, instance: &InstanceRef, audience: &[WatchId]) {
        for subscription in self.subscriptions.values_mut().filter(|subscription| {
            &subscription.instance == instance
                && audience.contains(&subscription.watch)
                && subscription.blocked.is_none()
        }) {
            subscription.gap = true;
            if subscription.latest.take().is_some() {
                subscription.dropped = subscription.dropped.saturating_add(1);
            }
        }
    }

    /// Moves every elapsed subscription's newest update into its Loop's pending delivery and
    /// returns the Loops that now have something to flush. A cadence that elapses without a new
    /// update delivers nothing of its own, but still retries a delivery the Loop is holding.
    fn collect_elapsed(&mut self, now: Instant) -> Vec<(ChatId, LoopId)> {
        let mut elapsed = Vec::new();
        for (id, subscription) in &mut self.subscriptions {
            if subscription.due_at > now {
                continue;
            }
            subscription.due_at = now + Duration::from_secs(subscription.every_seconds);
            let item = subscription.latest.take().map(|update| DeliveryItem {
                subscription: *id,
                watch: subscription.watch,
                instance: update.instance,
                sequence: update.sequence,
                observed_at: update.observed_at,
                dropped: std::mem::take(&mut subscription.dropped),
                gap: std::mem::take(&mut subscription.gap),
                kind: update.kind,
                recover_with: update.recover_with,
                data: update.data,
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
        ready
    }

    fn take_delivery(&mut self, owner: (ChatId, LoopId)) -> Option<Delivery> {
        let pending = self.pending.remove(&owner)?;
        if pending.items.is_empty() {
            return None;
        }
        let items: Vec<_> = pending.items.into_values().collect();
        let (payload, truncated) = fit_delivery_payload(&items);
        Some(Delivery {
            payload,
            items,
            truncated,
        })
    }

    fn restore(&mut self, owner: (ChatId, LoopId), items: Vec<DeliveryItem>) {
        let pending = self.pending.entry(owner).or_default();
        pending.due = true;
        for item in items {
            pending.items.entry(item.instance.clone()).or_insert(item);
        }
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
        Ok(configured
            .into_iter()
            .map(|(loop_id, loop_state, subscription)| {
                let runtime = deliveries.subscriptions.get(&subscription.id);
                TickSubscriptionStatus {
                    id: subscription.id,
                    loop_id,
                    loop_state,
                    instance: subscription.instance.clone(),
                    every_seconds: subscription.every_seconds,
                    accumulated: runtime.map_or(0, |runtime| runtime.accumulated),
                    last_delivery_ms: runtime.and_then(|runtime| runtime.last_delivery_ms),
                    last_delivery: runtime.and_then(|runtime| runtime.last_delivery.clone()),
                    kind: runtime.and_then(|runtime| runtime.kind.clone()),
                    title: runtime.and_then(|runtime| runtime.title.clone()),
                    updated_at_ms: runtime.and_then(|runtime| runtime.updated_at_ms),
                    link: runtime.map_or(TickLinkState::Initializing, |runtime| runtime.link),
                }
            })
            .collect())
    }

    /// Changes one subscription's cadence or cancels it, from the Chat's subscription view or a
    /// model tool. The change is written through the owning Loop's Trigger.
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
            TickSubscriptionChange::Cadence(every_seconds) => {
                for subscription in &mut trigger.subscriptions {
                    if subscription.id == subscription_id {
                        subscription.every_seconds = every_seconds;
                    }
                }
            }
        }
        self.configure_trigger(chat_id, loop_id, Some(trigger))
            .await
    }

    pub(super) async fn tick_subscription_tool(
        &self,
        receipt: &Receipt,
        name: &str,
        input: &Value,
    ) -> Result<ToolResult, String> {
        let outcome = match name {
            SUBSCRIBE_TICK_TOOL => self.subscribe_tick(receipt, input).await,
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

    async fn subscribe_tick(&self, receipt: &Receipt, input: &Value) -> Result<String, String> {
        let input: SubscribeTickInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid {SUBSCRIBE_TICK_TOOL} input: {error}"))?;
        let every_seconds = input.every_seconds.unwrap_or(DEFAULT_CADENCE_SECONDS);
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
            .find(|subscription| subscription.instance == input.tick_instance_id)
        {
            return Err(format!(
                "this goal already subscribes to Tick Instance {} as {}; change its cadence instead",
                existing.instance, existing.id
            ));
        }
        // A Trigger the user disarmed stays disarmed: subscribing is not consent to resume.
        let armed = trigger.armed;
        trigger.accepts_events = true;
        let subscription = TickSubscription {
            id: Uuid::new_v4(),
            instance: input.tick_instance_id,
            every_seconds,
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
            "every_seconds": subscription.every_seconds,
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
                    "the Tick host is disconnected; observation has not started",
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
        let configured: Vec<_> = self
            .inner
            .chats
            .lock()
            .await
            .iter()
            .flat_map(|(chat_id, chat)| {
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
                            .map(|subscription| (*chat_id, goal.id, subscription.clone()))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut deliveries = self.tick_deliveries();
        let now = Instant::now();
        let kept: BTreeSet<_> = configured
            .iter()
            .map(|(_, _, subscription)| subscription.id)
            .collect();
        deliveries.subscriptions.retain(|id, runtime| {
            kept.contains(id)
                && configured.iter().any(|(chat_id, loop_id, subscription)| {
                    subscription.id == *id
                        && subscription.instance == runtime.instance
                        && *chat_id == runtime.chat_id
                        && *loop_id == runtime.loop_id
                })
        });
        let owners: BTreeSet<_> = configured
            .iter()
            .map(|(chat_id, loop_id, _)| (*chat_id, *loop_id))
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
        for (chat_id, loop_id, subscription) in configured {
            match deliveries.subscriptions.get_mut(&subscription.id) {
                Some(runtime) if runtime.every_seconds != subscription.every_seconds => {
                    runtime.every_seconds = subscription.every_seconds;
                    runtime.due_at = now + Duration::from_secs(subscription.every_seconds);
                }
                Some(_) => {}
                None => {
                    deliveries.subscriptions.insert(
                        subscription.id,
                        SubscriptionRuntime {
                            chat_id,
                            loop_id,
                            instance: subscription.instance.clone(),
                            watch: Uuid::new_v4(),
                            ready: false,
                            link: TickLinkState::Initializing,
                            blocked: None,
                            every_seconds: subscription.every_seconds,
                            due_at: now + Duration::from_secs(subscription.every_seconds),
                            latest: None,
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
        drop(deliveries);
        self.inner.tick_wake.notify_one();
    }

    /// Flushes a Loop's pending delivery when a cadence elapsed while it was executing.
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

    /// Hands one Loop's pending delivery to [`Agent::trigger`], or marks it due when the Loop is
    /// executing. A Loop waiting for a permission or a question only accumulates.
    async fn deliver(&self, chat_id: ChatId, loop_id: LoopId) {
        let state = {
            let chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get(&chat_id) else {
                return;
            };
            let Some(goal) = chat.snapshot.loops.iter().find(|goal| goal.id == loop_id) else {
                return;
            };
            goal.state.clone()
        };
        let owner = (chat_id, loop_id);
        let delivery = {
            let mut deliveries = self.tick_deliveries();
            match state {
                LoopState::WaitingTrigger | LoopState::Completed => deliveries.take_delivery(owner),
                LoopState::Running | LoopState::Queued => {
                    if let Some(pending) = deliveries.pending.get_mut(&owner) {
                        pending.due = true;
                    }
                    None
                }
                _ => None,
            }
        };
        let Some(Delivery {
            payload,
            items,
            truncated,
        }) = delivery
        else {
            return;
        };
        let source = TriggerSource {
            source: TICK_SUBSCRIPTION_SOURCE.into(),
            payload,
        };
        let outcome = self.trigger(chat_id, loop_id, source).await;
        let mut deliveries = self.tick_deliveries();
        match outcome {
            Ok(_) => {
                let delivered_at = crate::private_fs::unix_time_milliseconds();
                let summary = delivery_summary(&items, truncated);
                for item in &items {
                    if let Some(runtime) = deliveries.subscriptions.get_mut(&item.subscription) {
                        runtime.accumulated = 0;
                        runtime.last_delivery_ms = Some(delivered_at);
                        runtime.last_delivery = Some(summary.clone());
                    }
                }
            }
            Err(_) => deliveries.restore(owner, items),
        }
    }

    pub(super) fn tick_deliveries(&self) -> std::sync::MutexGuard<'_, TickDeliveries> {
        self.inner
            .tick_deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Owns the stream events and every subscription's cadence.
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
                        Some(event) => agent.apply_stream_event(event),
                        None => return,
                    },
                    _ = async {
                        match next_due {
                            Some(due_at) => tokio::time::sleep_until(due_at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        let ready = agent.tick_deliveries().collect_elapsed(Instant::now());
                        for (chat_id, loop_id) in ready {
                            agent.deliver(chat_id, loop_id).await;
                        }
                    }
                }
            }
        })
    }

    fn apply_stream_event(&self, event: TickStreamEvent) {
        let mut deliveries = self.tick_deliveries();
        match event {
            TickStreamEvent::Metadata {
                instance,
                audience,
                metadata,
            } => {
                for subscription in deliveries
                    .subscriptions
                    .values_mut()
                    .filter(|subscription| {
                        audience.contains(&subscription.watch) && subscription.instance == instance
                    })
                {
                    subscription.kind = Some(metadata.kind.clone());
                    subscription.title = Some(metadata.title.clone());
                    subscription.updated_at_ms = metadata.updated_at_ms;
                }
            }
            TickStreamEvent::Ready { watch } => {
                if let Some(subscription) = deliveries
                    .subscriptions
                    .values_mut()
                    .find(|subscription| subscription.watch == watch)
                    && subscription.blocked.is_none()
                {
                    subscription.ready = true;
                    subscription.link = TickLinkState::Live;
                }
            }
            TickStreamEvent::Observation { audience, value } => {
                deliveries.record(&audience, &value)
            }
            TickStreamEvent::Gap { instance, audience } => {
                deliveries.mark_gap(&instance, &audience)
            }
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
                for subscription in deliveries
                    .subscriptions
                    .values_mut()
                    .filter(|subscription| {
                        subscription.instance == instance && audience.contains(&subscription.watch)
                    })
                {
                    subscription.link = link;
                    if matches!(
                        link,
                        TickLinkState::InstanceGone | TickLinkState::IdentityUncertain
                    ) {
                        subscription.blocked = Some(link);
                    }
                }
                deliveries.publish_desired();
            }
        }
        deliveries.ready_changed.notify_waiters();
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
    let mut payload = json!({"items": items, "truncated": truncated});
    if truncated {
        payload["recovery"] = json!(if items
            .iter()
            .any(|item| matches!(item.kind, ObservationKind::Terminal { .. }))
        {
            "Terminal observation was too large; use ReadTick for the current text. Unobserved terminal history cannot be recovered.".to_owned()
        } else if let Some(tool) = items.iter().find_map(|item| item.recover_with.as_deref()) {
            if tool.len() <= 128 {
                format!(
                    "Tick Update data was too large; use CallTickTool with the declared {tool} query to recover it."
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

/// Keeps a delivery within [`MAX_DELIVERY_PAYLOAD_BYTES`] by blanking the largest Tick Update
/// `data` first, so the model still learns which Tick Instances moved and can fetch the rest.
fn fit_delivery_payload(items: &[DeliveryItem]) -> (Value, bool) {
    let mut payload = delivery_payload(items, false);
    if payload.to_string().len() <= MAX_DELIVERY_PAYLOAD_BYTES {
        return (payload, false);
    }
    let mut delivered = items.to_vec();
    let mut largest_first: Vec<_> = (0..delivered.len()).collect();
    largest_first.sort_by_key(|index| std::cmp::Reverse(delivered[*index].data.to_string().len()));
    for index in largest_first {
        delivered[index].data = Value::Null;
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
