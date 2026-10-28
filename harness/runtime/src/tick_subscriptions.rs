//! Tick Subscriptions: the path from a Tick Update to a Loop.
//!
//! A subscription belongs to the Chat's Loop, so every mutation goes through
//! the subscription configuration path and is recorded with the Loop; user pauses use the source
//! change transaction. A paused subscription has no delivery watch, so resuming it establishes a
//! fresh baseline. The Agent keeps the union of active subscriptions' Tick Instances as the desired
//! set of one [`TickStreamPort`], whether or
//! not its Loop receives now; updates arriving from that stream accumulate per subscription. A Terminal
//! Tick pushes no screen: the stream only says when its screen first changed since a subscription
//! last got it, which marks that change pending; when the subscription's period elapses, the
//! stream reads the screen once and reports it as an update if it differs from what that
//! subscription last got. Its program's exit is read at once. When a subscription's period
//! elapses with updates pending, or as soon as an immediate one receives an update, the latest
//! update per Tick Instance merges into the owning Loop's pending delivery and is handed to
//! [`Agent::deliver_trigger`] as soon as the Loop can take it: it receives and its turn ended.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use tickloop_harness_protocol::{SubscriptionId, SubscriptionKind};
use tickloop_ids::{IdContext, ModelRef};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, Instant};

use super::loop_lease::TriggerPermit;
use super::tick_content::{
    DeliveredWatermark, TickContentCache, WATERMARK_DIRECTORY, read_watermark, remove_watermark,
    write_watermark,
};
use super::wakeup::{Eligibility, Hold, TriggerDelivery};
use super::*;
use crate::tick_stream::{
    TickConnectionState, TickObservation, TickStreamEvent, TickStreamPort, WatchId,
};
use crate::tick_trim::{Trimmed, encoded_len, omits_values, omitted_value, trim};
use tick_protocol::{InstanceRef, StateSeries};
use tickloop_harness_protocol::{
    HostToolAuthority, LoopId, LoopSnapshot, LoopState, MAX_DELIVERY_ITEM_BYTES,
    MAX_DELIVERY_PAYLOAD_BYTES, ObservationKind, Receipt, TickDeliveryItem, TickLinkState,
    TickSubscription, TickSubscriptionChange, TickSubscriptionFrequency, TickSubscriptionStatus,
    ToolDefinition, ToolResult, TriggerSource,
};

const SUBSCRIPTION_READY_TIMEOUT: Duration = Duration::from_secs(10);
const ALREADY_SUBSCRIBED: &str =
    "this Chat already subscribes to this Tick; s cycles its subscription";
const NO_SUBSCRIBING_LOOP: &str = "this Chat cannot take its Tick Updates now";

use crate::tick_tools::TICK_MANAGE;
const DEFAULT_FREQUENCY: TickSubscriptionFrequency = TickSubscriptionFrequency::High;
/// How soon a delivery its Loop could not take is looked at again, whatever the frequency of the
/// subscriptions it carries.
const HELD_DELIVERY_RETRY: Duration = Duration::from_secs(5);
/// How long one sample of a Terminal Tick's screen may take before its period passes without it.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(5);
/// An item the stream or a later delivery replaces is itself one superseded Tick Update.
const SUPERSEDED_UPDATE: u64 = 1;
/// How many times an item's data allowance is narrowed to make room for what its trimming adds;
/// past that its data gives way whole.
const ITEM_FIT_ATTEMPTS: usize = 4;
/// The longest text a Tick reports for an item's header fields, its observation times and
/// recovery guidance, that a delivery carries; a longer one is replaced by its size.
const MAX_HEADER_TEXT_BYTES: usize = 128;
const TRIMMED_HINT: &str = "Where an item lists `trimmed`, whole segments of its data were left out in place, each cut saying how many elements or lines it replaced; for the full content use that instance's `recover_with` (an extension Tick's query tool through TickCall, a Shell's output file through Read) or TickRead.";
const TRIMMED_SCREEN_HINT: &str = "A Terminal Tick's trimmed screen text keeps its first and last lines; TickRead returns its current screen, and output that scrolled past between periods cannot be recovered.";
const UNREADABLE_HINT: &str = "Where `data` or a Terminal Tick's `text` is null, the update could not be read again; read that instance with the native tools for its type.";
const ARTIFACT_SUBSCRIPTION_REFUSAL: &str = "an artifact takes no Tick Subscription: its association with the Chat is its subscription and brings each @tickloop feedback request to the Chat. An artifact the Chat creates is associated with it; the user associates others by selecting them in /artifacts or with f on their Monitor row.";

/// The source condition of a Tick the user sends with Send Update: it is still one of the
/// Chat's Monitor members, and no plan waits for review.
pub(super) fn manual_eligibility(snapshot: &ChatSnapshot, instance: &InstanceRef) -> Eligibility {
    let monitor = &snapshot.monitor;
    if monitor.is_removed(instance)
        || !monitor
            .members
            .iter()
            .any(|member| member.instance == *instance)
    {
        return Eligibility::Ended(tickloop_harness_transcript::DiscardReason::SourceEnded);
    }
    match snapshot.chat_loop.state {
        LoopState::WaitingPlan => Eligibility::Hold(Hold::PlanReview),
        LoopState::Idle
        | LoopState::Queued
        | LoopState::Running
        | LoopState::WaitingPermission
        | LoopState::WaitingQuestion
        | LoopState::Failed
        | LoopState::Interrupted => Eligibility::Eligible,
    }
}

/// What the user hears when a Tick they send cannot reach the Loop.
fn manual_admission(eligibility: Eligibility) -> Result<(), AgentError> {
    match eligibility {
        Eligibility::Eligible => Ok(()),
        Eligibility::Ended(_) => Err(AgentError::Invalid(
            "Send Update requires a Tick owned by this Chat".into(),
        )),
        Eligibility::Hold(_) => Err(AgentError::Invalid(NO_SUBSCRIBING_LOOP.into())),
    }
}

fn validate(subscriptions: &[TickSubscription]) -> Result<(), String> {
    let mut instances = BTreeSet::new();
    for subscription in subscriptions {
        check_frequency(&subscription.instance, subscription.frequency)?;
        if !instances.insert(subscription.instance.clone()) {
            return Err("a Chat holds at most one Tick Subscription per Tick Instance".into());
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
    vec![ToolDefinition {
        name: TICK_MANAGE.into(),
        description: format!(
            "Manage this Chat's Tick subscriptions. action=subscribe requires tick_instance_id and optionally frequency ({periods}; default {}). A Shell Tick is immediate and takes no frequency. An artifact uses its association rather than a subscription. action=unsubscribe requires subscription, the subscription id. A quiet period delivers nothing; state updates merge to the latest value. Opening or reading a Tick never subscribes automatically. Report that observation started only when the result says ready: true; for ready: false, inspect the returned link instead of subscribing again.",
            DEFAULT_FREQUENCY.name()
        ),
        input_schema: json!({"type":"object","properties":{
            "action":{"type":"string","enum":["subscribe","unsubscribe"]},
            "tick_instance_id":{"type":"string","minLength":3,"description":"Required by subscribe: a hosted: identity or a shell: task."},
            "frequency":{"type":"string","enum":frequencies,"description":"Optional for subscribe."},
            "subscription":{"type":"string","minLength":1,"description":"Required by unsubscribe: the subscription id a subscribe returned."}
        },"required":["action"],"additionalProperties":false}),
    }]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TickSubscribeInput {
    action: String,
    tick_instance_id: InstanceRef,
    frequency: Option<TickSubscriptionFrequency>,
}

/// The Tick Instance a `TickManage` call names and the frequency it gets: a Shell Tick is
/// always immediate and takes no frequency, an artifact takes no subscription, any other type
/// gets the chosen periodic one or the default.
pub(super) fn subscription_request(
    input: &Value,
) -> Result<(InstanceRef, TickSubscriptionFrequency), String> {
    let input: TickSubscribeInput = serde_json::from_value(input.clone())
        .map_err(|error| format!("invalid {TICK_MANAGE} input: {error}"))?;
    if input.action != "subscribe" {
        return Err("TickManage requires action=subscribe here".into());
    }
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
#[serde(deny_unknown_fields)]
struct TickUnsubscribeInput {
    action: String,
    subscription: ModelRef<SubscriptionKind>,
}

#[derive(Default)]
pub(super) struct TickDeliveries {
    pub(super) port: Option<Arc<dyn TickStreamPort>>,
    desired: Option<tokio::sync::watch::Sender<BTreeMap<WatchId, InstanceRef>>>,
    subscriptions: BTreeMap<Uuid, SubscriptionRuntime>,
    monitor_watches: BTreeMap<(ChatId, InstanceRef), MonitorWatch>,
    /// The newest update data of every subscription, within the Agent's content budget.
    content: TickContentCache,
    pending: HashMap<(ChatId, LoopId), PendingDelivery>,
    ready_changed: Arc<tokio::sync::Notify>,
    resuming: BTreeSet<WatchId>,
    baselines: BTreeMap<WatchId, u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DeliveryCheckpoint {
    subscriptions: BTreeMap<Uuid, SubscriptionRuntime>,
    #[serde(with = "crate::upgrade::map_entries")]
    monitor_watches: BTreeMap<(ChatId, InstanceRef), MonitorWatch>,
    content: TickContentCache,
    #[serde(with = "crate::upgrade::map_entries")]
    pending: HashMap<(ChatId, LoopId), PendingDelivery>,
    baselines: BTreeMap<WatchId, u64>,
}

impl TickDeliveries {
    pub(super) fn upgrade_checkpoint(&self) -> Value {
        serde_json::to_value(DeliveryCheckpoint {
            subscriptions: self.subscriptions.clone(),
            monitor_watches: self.monitor_watches.clone(),
            content: self.content.clone(),
            pending: self.pending.clone(),
            baselines: self.baselines.clone(),
        })
        .expect("Tick delivery checkpoint serializes")
    }

    pub(super) fn restore_upgrade(&mut self, value: Value) -> Result<(), String> {
        let checkpoint: DeliveryCheckpoint =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        self.subscriptions = checkpoint.subscriptions;
        self.monitor_watches = checkpoint.monitor_watches;
        self.content = checkpoint.content;
        self.pending = checkpoint.pending;
        self.baselines = checkpoint.baselines;
        self.resuming = self
            .subscriptions
            .values()
            .map(|runtime| runtime.watch)
            .chain(self.monitor_watches.values().map(|watch| watch.watch))
            .collect();
        Ok(())
    }

    pub(super) fn activate_upgrade(&self) {
        self.publish_desired();
    }
}

struct WatchBinding {
    watch: WatchId,
    instance: InstanceRef,
    authority: Option<HostToolAuthority>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct MonitorWatch {
    watch: WatchId,
    authority: Option<HostToolAuthority>,
    ready: bool,
    series: Vec<StateSeries>,
    last_sequence: Option<u64>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    #[serde(with = "crate::upgrade::optional_instant")]
    due_at: Option<Instant>,
    /// The newest update since the last delivery, without its data, which waits in the content
    /// cache; earlier ones are counted, not kept.
    latest: Option<LatestUpdate>,
    /// The last delivered update; one at or before it is never delivered again.
    delivered: Option<DeliveredWatermark>,
    /// Where the delivered watermark is kept for a saved Chat.
    watermarks: Option<std::path::PathBuf>,
    /// The first update since the last delivery; later ones only move [`Self::latest`].
    first: Option<FirstUpdate>,
    /// Updates since the last delivery that a newer one replaced.
    superseded: u64,
    accumulated: u64,
    /// A Terminal Tick's screen changed since this subscription last got it; its next sample
    /// settles it.
    has_pending_change: bool,
    #[serde(with = "crate::upgrade::optional_instant")]
    pending_since: Option<Instant>,
    program_exit: Option<tickloop_harness_protocol::HostedProgramExit>,
    gap: bool,
    last_delivery_ms: Option<u64>,
    last_delivery: Option<String>,
    kind: Option<ObservationKind>,
    title: Option<String>,
    updated_at_ms: Option<u64>,
    /// The series the instance declares in its state, whose newest end a trimmed delivery keeps.
    series: Vec<StateSeries>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct FirstUpdate {
    #[serde(with = "crate::upgrade::instant")]
    received_at: Instant,
    sequence: u64,
    observed_at: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct LatestUpdate {
    sequence: u64,
    observed_at: String,
    kind: ObservationKind,
    recover_with: Option<String>,
    /// Its data is larger than the content budget, so it is read again for the delivery; what
    /// [`left_out`] keeps of it waits in the cache meanwhile.
    is_oversized: bool,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct PendingDelivery {
    items: BTreeMap<InstanceRef, DeliveryItem>,
    /// The delivery came due while the Loop could not take it, so it flushes when the Loop's run
    /// ends.
    due: bool,
    /// When a held delivery is looked at again, in case the run ended before it was held or the
    /// delivery failed.
    #[serde(with = "crate::upgrade::optional_instant")]
    retry_at: Option<Instant>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DeliveryItem {
    title: Option<String>,
    subscription: Uuid,
    watch: WatchId,
    instance: InstanceRef,
    /// The sequence and time of the first update this item merges, and of the newest, whose data
    /// it carries.
    first_sequence: u64,
    first_observed_at: String,
    sequence: u64,
    observed_at: String,
    /// How many updates it merges: the newest and every one that newest replaced.
    updates: u64,
    /// When the earliest update it merges came due; the longest waiting go first.
    #[serde(with = "crate::upgrade::instant")]
    waiting_since: Instant,
    gap: bool,
    kind: ObservationKind,
    recover_with: Option<String>,
    series: Vec<StateSeries>,
    data: Value,
    /// The data is not in the content cache, because it was evicted or never fit, and is read
    /// again before delivery.
    needs_refill: bool,
}

impl DeliveryItem {
    /// This item takes the place of one the Loop has not received yet, whose updates it now
    /// merges too.
    fn superseding(self, superseded: &Self) -> Self {
        Self {
            first_sequence: superseded.first_sequence,
            first_observed_at: superseded.first_observed_at.clone(),
            updates: self.updates + superseded.updates,
            waiting_since: self.waiting_since.min(superseded.waiting_since),
            gap: self.gap || superseded.gap,
            ..self
        }
    }

    /// The item's fields beside its data, the text its Tick reports in them bounded.
    fn header(&self, ids: &IdContext) -> Value {
        let mut header = json!({
            "subscription": ids.model_text(SubscriptionId::from(self.subscription)),
            "watch": ids.model_text(WatchId::from(self.watch)),
            "instance": self.instance.model_text(ids),
            "updates": self.updates,
            "seq": span(&self.first_sequence.to_string(), &self.sequence.to_string()),
            "between": bounded_text(&span(&self.first_observed_at, &self.observed_at)),
            "gap": self.gap,
            "kind": self.kind,
            "data": Value::Null,
        });
        if let Some(title) = &self.title {
            header["title"] = json!(bounded_text(title));
        }
        if let Some(recover_with) = &self.recover_with {
            header["recover_with"] = json!(bounded_text(recover_with));
        }
        header
    }

    /// The item as a delivery carries it with its `header`, its data shortened so the whole
    /// item takes at most `allowance` encoded bytes, with what was shortened. Data that does not
    /// fit beside what its trimming reports gives way whole.
    fn delivered(&self, header: &Value, allowance: usize) -> (Value, Vec<Trimmed>) {
        fit_tick_item(header, &self.data, &self.series, allowance)
    }
}

fn fit_tick_item(
    header: &Value,
    data: &Value,
    series: &[StateSeries],
    allowance: usize,
) -> (Value, Vec<Trimmed>) {
    let mut data_allowance = allowance.saturating_sub(encoded_len(header));
    for _ in 0..ITEM_FIT_ATTEMPTS {
        let mut item = header.clone();
        let mut data = data.clone();
        let trimmed = trim(&mut data, data_allowance, series);
        if !trimmed.is_empty() {
            item["trimmed"] = json!(trimmed);
        }
        item["data"] = data;
        let size = encoded_len(&item);
        if size <= allowance {
            return (item, trimmed);
        }
        data_allowance = data_allowance.saturating_sub(size - allowance);
    }
    let (marker, record) = Trimmed::whole_omission(encoded_len(data));
    let mut item = header.clone();
    item["data"] = marker;
    item["trimmed"] = json!([&record]);
    (item, vec![record])
}

/// `text` as a delivery's header carries it: as it is, or replaced by its size when longer than
/// [`MAX_HEADER_TEXT_BYTES`].
fn bounded_text(text: &str) -> String {
    match text.len() <= MAX_HEADER_TEXT_BYTES {
        true => text.to_owned(),
        false => omitted_value(text.len()),
    }
}

/// `first→last`, or one value when they are the same.
fn span(first: &str, last: &str) -> String {
    match first == last {
        true => last.to_owned(),
        false => format!("{first}→{last}"),
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
            TickStreamEvent::Baseline { watch, sequence } => {
                if !self.resuming.contains(&watch) {
                    self.baselines.insert(watch, sequence);
                }
            }
            TickStreamEvent::Metadata {
                instance,
                audience,
                metadata,
            } => {
                for ((_, target), watch) in &mut self.monitor_watches {
                    if *target == instance && audience.contains(&watch.watch) {
                        watch.series = metadata.series.clone();
                    }
                }
                for subscription in self.subscriptions.values_mut().filter(|subscription| {
                    audience.contains(&subscription.watch) && subscription.instance == instance
                }) {
                    subscription.kind = Some(metadata.kind.clone());
                    subscription.title = Some(metadata.title.clone());
                    subscription.series = metadata.series.clone();
                    subscription.updated_at_ms =
                        subscription.updated_at_ms.max(metadata.updated_at_ms);
                }
            }
            TickStreamEvent::Ready { watch } => {
                for monitor in self
                    .monitor_watches
                    .values_mut()
                    .filter(|monitor| monitor.watch == watch)
                {
                    monitor.ready = true;
                }
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
            TickStreamEvent::ScreenChange {
                instance,
                audience,
                is_pending,
            } => self.mark_screen_change(&instance, &audience, is_pending),
            // Its watches are sampled at once by `Agent::apply_stream_event`.
            TickStreamEvent::ProgramExited { .. } => {}
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
                        subscription.has_pending_change = false;
                        subscription.pending_since = None;
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

    /// The watches the delivered items came through, while each is still the subscription the
    /// Loop configures, on the watch it was admitted for; otherwise why the delivery ended.
    fn item_bindings(
        &self,
        chat_id: ChatId,
        chat_loop: &LoopSnapshot,
        items: &[TickDeliveryItem],
    ) -> Result<Vec<WatchBinding>, DiscardReason> {
        items
            .iter()
            .map(|item| {
                let configured = chat_loop
                    .subscriptions
                    .iter()
                    .find(|configured| {
                        configured.id == item.subscription
                            && configured.instance == item.instance
                            && !configured.paused
                    })
                    .ok_or(DiscardReason::Unsubscribed)?;
                let runtime = self
                    .subscriptions
                    .get(&item.subscription)
                    .filter(|runtime| {
                        runtime.watch == item.watch
                            && runtime.instance == item.instance
                            && runtime.chat_id == chat_id
                            && runtime.loop_id == chat_loop.id
                            && runtime.blocked.is_none()
                            && runtime.authority == configured.authority
                    })
                    .ok_or(DiscardReason::SourceEnded)?;
                Ok(WatchBinding {
                    watch: runtime.watch,
                    instance: runtime.instance.clone(),
                    authority: runtime.authority.clone(),
                })
            })
            .collect()
    }

    /// The restored delivery's items bound to the watches their subscriptions run now, where the
    /// subscription, its instance and its authority are still the ones the delivery was
    /// accepted under: a load or another process watches through a new watch, and the delivery
    /// stays the same.
    pub(super) fn rebind_restored_items(
        &self,
        chat_id: ChatId,
        chat_loop: &LoopSnapshot,
        items: &mut [TickDeliveryItem],
    ) {
        for item in items {
            let Some(configured) = chat_loop.subscriptions.iter().find(|configured| {
                configured.id == item.subscription
                    && configured.instance == item.instance
                    && !configured.paused
            }) else {
                continue;
            };
            if let Some(runtime) = self
                .subscriptions
                .get(&item.subscription)
                .filter(|runtime| {
                    runtime.instance == item.instance
                        && runtime.chat_id == chat_id
                        && runtime.loop_id == chat_loop.id
                        && runtime.authority == configured.authority
                })
            {
                item.watch = runtime.watch.into();
            }
        }
    }

    /// Whether an item of a new delivery was delivered already: a delivery prepared before the
    /// lease told what another process delivered does not reach the Loop.
    pub(super) fn holds_delivered_items(&self, items: &[TickDeliveryItem]) -> bool {
        items.iter().any(|item| {
            self.subscriptions
                .get(&item.subscription)
                .and_then(|runtime| runtime.delivered)
                .is_some_and(|delivered| item.sequence <= delivered.sequence)
        })
    }

    /// The Tick Update source's own condition.
    pub(super) fn items_eligibility(
        &self,
        chat_id: ChatId,
        chat_loop: &LoopSnapshot,
        items: &[TickDeliveryItem],
    ) -> Eligibility {
        match self.item_bindings(chat_id, chat_loop, items) {
            Ok(_) => Eligibility::Eligible,
            Err(reason) => Eligibility::Ended(reason),
        }
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
                    .chain(self.monitor_watches.values().filter_map(|watch| {
                        watch
                            .authority
                            .clone()
                            .map(|authority| (watch.watch, authority))
                    }))
                    .collect(),
            );
            port.set_watch_chats(
                self.subscriptions
                    .values()
                    .filter(|subscription| subscription.blocked.is_none())
                    .map(|subscription| (subscription.watch, subscription.chat_id.uuid()))
                    .chain(
                        self.monitor_watches
                            .iter()
                            .map(|((chat, _), watch)| (watch.watch, chat.uuid())),
                    )
                    .collect(),
            );
        }
        let desired: BTreeMap<WatchId, InstanceRef> = self
            .subscriptions
            .values()
            .filter(|subscription| subscription.blocked.is_none())
            .map(|subscription| (subscription.watch, subscription.instance.clone()))
            .chain(
                self.monitor_watches
                    .iter()
                    .map(|((_, instance), watch)| (watch.watch, instance.clone())),
            )
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
            if matches!(observation.kind, ObservationKind::Terminal { .. }) {
                subscription.program_exit =
                    serde_json::from_value(observation.data["exit"].clone()).ok();
            }
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
            // The updates the stream merged into this one precede it in its source's numbering,
            // unless a gap restarted that numbering with this one.
            if subscription.first.is_none() || observation.gap {
                let merged = if observation.gap {
                    0
                } else {
                    observation.dropped
                };
                subscription.first = Some(FirstUpdate {
                    received_at: Instant::now(),
                    sequence: observation.sequence.saturating_sub(merged),
                    observed_at: observation.observed_at.clone(),
                });
            }
            let superseded = subscription.latest.replace(LatestUpdate {
                sequence: observation.sequence,
                observed_at: observation.observed_at.clone(),
                kind: observation.kind.clone(),
                recover_with: observation.recover_with.clone(),
                is_oversized: !is_kept,
            });
            let replaced = superseded.map_or(0, |_| SUPERSEDED_UPDATE);
            subscription.superseded = subscription
                .superseded
                .saturating_add(observation.dropped)
                .saturating_add(replaced);
            subscription.pending_since.get_or_insert_with(Instant::now);
            subscription.accumulated = subscription
                .accumulated
                .saturating_add(1 + observation.dropped);
            subscription.gap |= observation.gap;
            subscription.updated_at_ms = subscription.updated_at_ms.max(Some(
                tickloop_harness_base::private_fs::unix_time_milliseconds(),
            ));
            if subscription.frequency == TickSubscriptionFrequency::Immediate {
                subscription.due_at = Some(Instant::now());
            }
        }
    }

    /// Records a screen sampled for `watch` as an update due now, since its period elapsed or its
    /// program exited; whether it was taken.
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

    /// Marks whether a Terminal Tick's screen changed since `audience` last got it. The first
    /// change is when the instance last updated, though it is no Tick Update until a sample finds
    /// it still there.
    fn mark_screen_change(
        &mut self,
        instance: &InstanceRef,
        audience: &[WatchId],
        is_pending: bool,
    ) {
        for subscription in self.subscriptions.values_mut().filter(|subscription| {
            subscription.ready
                && subscription.blocked.is_none()
                && &subscription.instance == instance
                && audience.contains(&subscription.watch)
        }) {
            if is_pending && !subscription.has_pending_change {
                subscription.updated_at_ms = subscription.updated_at_ms.max(Some(
                    tickloop_harness_base::private_fs::unix_time_milliseconds(),
                ));
            }
            subscription.has_pending_change = is_pending;
            if is_pending {
                subscription.pending_since.get_or_insert_with(Instant::now);
            } else if subscription.accumulated == 0 {
                subscription.pending_since = None;
            }
        }
    }

    /// The watches of `instance` among `audience` that can sample its screen now.
    fn samplable_watches(
        &self,
        instance: &InstanceRef,
        audience: &[WatchId],
    ) -> Vec<(WatchId, InstanceRef)> {
        self.subscriptions
            .values()
            .filter(|subscription| {
                subscription.ready
                    && subscription.blocked.is_none()
                    && &subscription.instance == instance
                    && audience.contains(&subscription.watch)
            })
            .map(|subscription| (subscription.watch, subscription.instance.clone()))
            .collect()
    }

    fn mark_gap(&mut self, instance: &InstanceRef, audience: &[WatchId]) {
        for (id, subscription) in self.subscriptions.iter_mut().filter(|(_, subscription)| {
            &subscription.instance == instance
                && audience.contains(&subscription.watch)
                && subscription.blocked.is_none()
        }) {
            subscription.gap = true;
            subscription.first = None;
            if subscription.latest.take().is_some() {
                self.content.remove(*id);
                subscription.superseded = subscription.superseded.saturating_add(1);
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
                let first = subscription.first.take().unwrap_or(FirstUpdate {
                    received_at: now,
                    sequence: update.sequence,
                    observed_at: update.observed_at.clone(),
                });
                DeliveryItem {
                    title: subscription.title.clone(),
                    subscription: *id,
                    watch: subscription.watch,
                    instance: subscription.instance.clone(),
                    first_sequence: first.sequence,
                    first_observed_at: first.observed_at,
                    sequence: update.sequence,
                    observed_at: update.observed_at,
                    updates: std::mem::take(&mut subscription.superseded) + 1,
                    waiting_since: now,
                    gap: std::mem::take(&mut subscription.gap),
                    kind: update.kind,
                    recover_with: update.recover_with,
                    series: subscription.series.clone(),
                    needs_refill: data.is_none() || update.is_oversized,
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

    /// `owner`'s pending items, the longest waiting first, so an item a full delivery put off is
    /// not put off again by newer ones.
    fn take_delivery(&mut self, owner: (ChatId, LoopId)) -> Option<Vec<DeliveryItem>> {
        let pending = self.pending.remove(&owner)?;
        if pending.items.is_empty() {
            return None;
        }
        let mut items: Vec<_> = pending.items.into_values().collect();
        items.sort_by_key(|item| item.waiting_since);
        Some(items)
    }

    /// Puts items back for `owner`'s next delivery; a newer item that came due meanwhile for the
    /// same instance takes their place and merges their updates. While they wait they hold no
    /// data of their own: each keeps only what [`left_out`] keeps and is read again before
    /// delivery, and its data waits in the content cache, within its budget, for a Tick that
    /// cannot be read again then.
    fn restore(&mut self, owner: (ChatId, LoopId), items: Vec<DeliveryItem>) {
        let pending = self.pending.entry(owner).or_default();
        for mut item in items {
            let data = (!item.needs_refill).then(|| item.data.clone());
            item.data = left_out(&item.kind, &item.data);
            item.needs_refill = true;
            let (subscription, sequence) = (item.subscription, item.sequence);
            // Deliveries put back in any order: the newest update of the instance stays.
            let kept = match pending.items.remove(&item.instance) {
                Some(waiting) if waiting.sequence >= item.sequence => waiting.superseding(&item),
                Some(waiting) => item.superseding(&waiting),
                None => item,
            };
            if kept.sequence == sequence
                && let Some(data) = data
            {
                self.content.put_back(subscription, sequence, data);
            }
            pending.items.insert(kept.instance.clone(), kept);
        }
        self.hold(owner, Instant::now());
    }

    /// The updates `items` carry no longer wait in their subscriptions: they were delivered.
    fn let_go(&mut self, items: &[DeliveryItem]) {
        for item in items {
            if let Some(runtime) = self.subscriptions.get_mut(&item.subscription) {
                runtime.accumulated = runtime.accumulated.saturating_sub(item.updates);
                if let Some(first) = &runtime.first {
                    runtime.pending_since = Some(first.received_at);
                } else if runtime.accumulated == 0 && !runtime.has_pending_change {
                    runtime.pending_since = None;
                }
            }
        }
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
    /// Installs the Tick observation stream and enables the subscription actions of `TickManage`.
    /// The stream's tasks are this Agent's: `shutdown` cancels them and waits for them.
    pub async fn install_tick_stream(&self, port: Arc<dyn TickStreamPort>) {
        let handle = port.open(&crate::tick_stream::StreamTasks::new(
            self.inner.tasks.tracker(),
            self.inner.shutdown.clone(),
        ));
        {
            let mut deliveries = self.tick_deliveries();
            deliveries.desired = Some(handle.desired);
            deliveries.port = Some(port);
        }
        self.inner
            .tick_stream_installed
            .store(true, Ordering::SeqCst);
        self.tick_delivery_task(handle.events);
        self.sync_tick_subscriptions().await;
    }

    /// Bytes of Tick Update data waiting in this Agent's content cache.
    #[doc(hidden)]
    pub fn tick_content_bytes(&self) -> usize {
        self.tick_deliveries().content.used_bytes()
    }

    /// The sequence of the newest update of `subscription` this process knows was delivered.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn tick_delivered_sequence_for_tests(&self, subscription: Uuid) -> Option<u64> {
        self.tick_deliveries()
            .subscriptions
            .get(&subscription)?
            .delivered
            .as_ref()
            .map(|delivered| delivered.sequence)
    }

    pub async fn tick_subscriptions(
        &self,
        chat_id: ChatId,
    ) -> Result<Vec<TickSubscriptionStatus>, AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        let chat_loop = &chat.snapshot.chat_loop;
        let configured: Vec<_> = chat_loop
            .subscriptions
            .iter()
            .map(|subscription| (chat_loop.state.clone(), subscription.clone()))
            .collect();
        drop(chats);
        let deliveries = self.tick_deliveries();
        let now = Instant::now();
        let now_ms = tickloop_harness_base::private_fs::unix_time_milliseconds();
        Ok(configured
            .into_iter()
            .map(|(loop_state, subscription)| {
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
                    paused: subscription.paused,
                    id: subscription.id,
                    loop_state,
                    instance: subscription.instance.clone(),
                    frequency: subscription.frequency,
                    accumulated: runtime.map_or(0, |runtime| runtime.accumulated),
                    has_pending_change: runtime.is_some_and(|runtime| runtime.has_pending_change),
                    pending_since_ms: runtime.and_then(|runtime| runtime.pending_since).map(
                        |since| {
                            now_ms.saturating_sub(
                                now.saturating_duration_since(since).as_millis() as u64
                            )
                        },
                    ),
                    program_exit: runtime.and_then(|runtime| runtime.program_exit),
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

    /// Changes a subscription from Monitor, or cancels it from a model tool. Only the user-facing
    /// AgentAccess command exposes pauses; a frequency the Tick type does not allow is refused.
    pub async fn update_tick_subscription(
        &self,
        chat_id: ChatId,
        subscription_id: Uuid,
        change: TickSubscriptionChange,
    ) -> Result<(), AgentError> {
        let mut subscriptions = self.loop_subscriptions(chat_id).await?;
        if !subscriptions
            .iter()
            .any(|subscription| subscription.id == subscription_id)
        {
            return Err(AgentError::NotFound);
        }
        match change {
            TickSubscriptionChange::Paused(paused) => {
                self.run_source_change(chat_id, move |_change, chat| {
                    let subscription = chat
                        .snapshot
                        .chat_loop
                        .subscriptions
                        .iter_mut()
                        .find(|subscription| subscription.id == subscription_id)
                        .ok_or(AgentError::NotFound)?;
                    if subscription.frequency.period().is_none() {
                        return Err(AgentError::Invalid(
                            "Pausing this Tick is not supported".into(),
                        ));
                    }
                    subscription.paused = paused;
                    Ok(())
                })
                .await??;
                self.sync_tick_subscriptions().await;
                return Ok(());
            }
            TickSubscriptionChange::Cancel => {
                subscriptions.retain(|subscription| subscription.id != subscription_id)
            }
            TickSubscriptionChange::Frequency(frequency) => {
                for subscription in &mut subscriptions {
                    if subscription.id == subscription_id {
                        subscription.frequency = frequency;
                    }
                }
            }
        }
        self.configure_subscriptions(chat_id, subscriptions).await
    }

    async fn loop_subscriptions(
        &self,
        chat_id: ChatId,
    ) -> Result<Vec<TickSubscription>, AgentError> {
        Ok(self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .chat_loop
            .subscriptions
            .clone())
    }

    /// Replaces the Loop's Tick Subscriptions: the one place they change. The receiving switch
    /// stays as it is; the watches follow the new set, and queued Tick Updates of a cancelled
    /// subscription are withdrawn.
    pub(super) async fn configure_subscriptions(
        &self,
        chat_id: ChatId,
        subscriptions: Vec<TickSubscription>,
    ) -> Result<(), AgentError> {
        validate(&subscriptions).map_err(AgentError::Invalid)?;
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            if let Some(delegation) = &chat.snapshot.delegation {
                if subscriptions.iter().any(|subscription| {
                    !delegation
                        .ceiling
                        .tick_instances
                        .contains(&subscription.instance)
                }) {
                    return Err(AgentError::Invalid("Unsupported: Tick Subscription is outside the inherited delegated instance scope".into()));
                }
                if subscriptions
                    .iter()
                    .filter_map(|subscription| subscription.authority.as_ref())
                    .any(|authority| {
                        !delegation
                            .ceiling
                            .host_tools
                            .values()
                            .any(|allowed| allowed.contains(authority))
                    })
                {
                    return Err(AgentError::Invalid(
                        "Tick watch authority is outside the frozen delegated account/endpoint scope"
                            .into(),
                    ));
                }
            }
            chat.snapshot.chat_loop.subscriptions = subscriptions;
            self.save(&chat.snapshot);
        }
        self.sync_tick_subscriptions().await;
        Ok(())
    }

    /// Subscribes the Chat's Loop to `instance` because its user asked to on the instance's
    /// Monitor row, at the frequency its type fixes or else the default; an instance the Loop
    /// already subscribes to is refused with what to do instead.
    pub async fn tick_subscribe_for_user(
        &self,
        chat_id: ChatId,
        instance: InstanceRef,
    ) -> Result<(), AgentError> {
        {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            let is_subscribed = chat
                .snapshot
                .chat_loop
                .subscriptions
                .iter()
                .any(|subscription| subscription.instance == instance);
            if is_subscribed {
                return Err(AgentError::Invalid(ALREADY_SUBSCRIBED.into()));
            }
        }
        let frequency = fixed_frequency(&instance)
            .map_err(AgentError::Invalid)?
            .unwrap_or(DEFAULT_FREQUENCY);
        self.subscribe_loop_for_user(chat_id, instance, frequency)
            .await
    }

    /// Ends this Chat's subscriptions to `lost` once `reopened` took its place, and with
    /// `restore` subscribes the Loop to `reopened` again at the frequency it had, admitted as the
    /// user's own subscription. The error tells which could not be restored.
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
                .chat_loop
                .subscriptions
                .iter()
                .filter(|subscription| subscription.instance == *lost)
                .map(|subscription| (subscription.id, subscription.frequency))
                .collect::<Vec<_>>()
        };
        let mut refusals = Vec::new();
        for (subscription, frequency) in subscriptions {
            self.update_tick_subscription(chat_id, subscription, TickSubscriptionChange::Cancel)
                .await?;
            if restore
                && let Err(error) = self
                    .subscribe_loop_for_user(chat_id, reopened.clone(), frequency)
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

    async fn admit_manual_tick(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        instance: &InstanceRef,
    ) -> Result<Option<HostToolAuthority>, AgentError> {
        if instance.access() != "hosted" {
            return Err(AgentError::Invalid(
                "Send Update requires an extension or Terminal Tick".into(),
            ));
        }
        let port = self
            .tick_deliveries()
            .port
            .clone()
            .ok_or_else(|| AgentError::Invalid("Tick stream is unavailable".into()))?;
        let input = json!({"tick_instance_id": instance});
        let watch = port
            .prepare_watch(
                chat_id.into(),
                instance,
                &input,
                self.inner.shutdown.child_token(),
            )
            .await
            .map_err(AgentError::Invalid)?;
        let authority = watch
            .as_ref()
            .map(|watch| watch.host_authority())
            .transpose()
            .map_err(AgentError::Invalid)?
            .flatten()
            .and_then(|binding| binding.authority);
        let admission = watch.map(|watch| watch.into_admission(input.clone()));
        let (policy_name, permission_input) = admission
            .as_ref()
            .map_or((crate::tick_tools::TICK_READ, &input), |call| {
                (call.policy_name(), call.permission_input())
            });
        self.refresh_project_rules(chat_id).await?;
        {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            manual_admission(manual_eligibility(&chat.snapshot, instance))?;
            let operation = SessionPermissionOperation::from_normalized(
                policy_name.to_owned(),
                permission_input.clone(),
                super::session_permissions::capture_workspace_scope(
                    &configured_workspace_for_chat(chat),
                )
                .map_err(|error| AgentError::Invalid(error.to_string()))?,
            )
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
            let session = self.inner.session_permissions.lock().await.evaluate(
                PermissionOwner::new(chat_id.into(), loop_id.into())
                    .map_err(|error| AgentError::Invalid(error.to_string()))?,
                &operation,
            );
            if chat.snapshot.policy.explicit_match_with_session(
                policy_name,
                permission_input,
                loop_id.into(),
                session.decision,
            ) == Some(tickloop_harness_tools::permissions::ExplicitPermissionDecision::Deny)
            {
                return Err(AgentError::Invalid(format!(
                    "a Deny rule for {policy_name} refuses sending {instance}"
                )));
            }
        }
        if let Some(admission) = &admission {
            crate::tick_stream::PreparedTickWatch::revalidate(
                chat_id.into(),
                admission,
                self.inner.shutdown.child_token(),
            )
            .await
            .map_err(AgentError::Invalid)?;
        }
        port.revalidate_watch(chat_id.into(), instance, authority.as_ref())
            .await
            .map_err(AgentError::Invalid)?;
        Ok(authority)
    }

    pub async fn tick_send_update_for_user(
        &self,
        chat_id: ChatId,
        instance: InstanceRef,
    ) -> Result<Receipt, AgentError> {
        let (loop_id, title) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            let chat_loop = &chat.snapshot.chat_loop;
            if chat_loop
                .subscriptions
                .iter()
                .any(|subscription| subscription.instance == instance)
            {
                return Err(AgentError::Invalid(
                    "Send Update is available for unsubscribed Ticks".into(),
                ));
            }
            // Asked before the Tick is read; the entry asks again with what was read.
            let unread = TriggerSource::TickManual {
                instance: instance.clone(),
                sequence: 0,
                read_at_ms: 0,
                authority: None,
            };
            manual_admission(self.eligibility(chat, &unread))?;
            let title = chat
                .snapshot
                .monitor
                .members
                .iter()
                .find(|member| member.instance == instance)
                .and_then(|member| member.title.clone());
            (chat_loop.id, title)
        };
        let authority = self.admit_manual_tick(chat_id, loop_id, &instance).await?;
        let port = self
            .tick_deliveries()
            .port
            .clone()
            .ok_or_else(|| AgentError::Invalid("Tick stream is unavailable".into()))?;
        {
            let mut deliveries = self.tick_deliveries();
            let key = (chat_id, instance.clone());
            if deliveries
                .monitor_watches
                .get(&key)
                .is_none_or(|watch| watch.authority != authority)
            {
                deliveries.monitor_watches.insert(
                    key,
                    MonitorWatch {
                        watch: tickloop_ids::generate().into(),
                        authority: authority.clone(),
                        ready: false,
                        series: Vec::new(),
                        last_sequence: None,
                    },
                );
            }
            deliveries.publish_desired();
        }
        let series = tokio::time::timeout(SUBSCRIPTION_READY_TIMEOUT, async {
            loop {
                let notify = self.tick_deliveries().ready_changed.clone();
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(series) = self
                    .tick_deliveries()
                    .monitor_watches
                    .get(&(chat_id, instance.clone()))
                    .filter(|watch| watch.ready)
                    .map(|watch| watch.series.clone())
                {
                    break series;
                }
                tokio::select! {
                    _ = notified => {},
                    _ = self.inner.shutdown.cancelled() => return Vec::new(),
                }
            }
        })
        .await
        .map_err(|_| AgentError::Invalid("The Tick did not become ready for Send Update".into()))?;
        let through_ms = crate::monitor_members::now_ms();
        let observation =
            tokio::time::timeout(SAMPLE_TIMEOUT, port.read_current(chat_id.into(), &instance))
                .await
                .map_err(|_| AgentError::Invalid("Reading the Tick timed out".into()))?
                .map_err(AgentError::Invalid)?
                .ok_or_else(|| AgentError::Invalid("The Tick has no readable content".into()))?;
        if observation.instance != instance
            || (matches!(observation.kind, ObservationKind::Terminal { .. })
                && !observation.data["exit"].is_null())
        {
            return Err(AgentError::Invalid(
                "The Tick has ended or changed identity".into(),
            ));
        }
        let current_authority = self.admit_manual_tick(chat_id, loop_id, &instance).await?;
        if current_authority != authority {
            return Err(AgentError::Invalid(
                "Tick authority changed while reading".into(),
            ));
        }
        let ids = self.chat_ids(chat_id).await;
        let mut header = json!({"instance":instance.model_text(&ids),"seq":observation.sequence.to_string(),"kind":observation.kind,"updates":1,"through_ms":through_ms});
        if let Some(title) = title {
            header["title"] = json!(bounded_text(&title));
        }
        let (item, trimmed) =
            fit_tick_item(&header, &observation.data, &series, MAX_DELIVERY_ITEM_BYTES);
        let payload = delivery_payload(
            vec![item],
            &DeliveryNotes {
                is_trimmed: !trimmed.is_empty(),
                ..Default::default()
            },
        );
        let input = match self.goal_background(chat_id).await {
            Some(goal) => {
                tickloop_harness_protocol::local_messages::tick_delivery_with_goal(&payload, &goal)
            }
            None => tickloop_harness_protocol::local_messages::tick_delivery(&payload),
        };
        let source = TriggerSource::TickManual {
            instance,
            sequence: observation.sequence,
            read_at_ms: through_ms,
            authority,
        };
        self.deliver_trigger(
            chat_id,
            TriggerDelivery {
                source,
                input,
                ledger: None,
                on_accept: None,
            },
        )
        .await
    }

    pub(super) async fn revalidate_manual_tick_source(
        &self,
        chat_id: ChatId,
        instance: &InstanceRef,
        expected: &Option<HostToolAuthority>,
    ) -> Result<(), AgentError> {
        let loop_id = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .chat_loop
            .id;
        let authority = self.admit_manual_tick(chat_id, loop_id, instance).await?;
        if authority != *expected {
            return Err(AgentError::Invalid(
                "Tick authority changed before delivery".into(),
            ));
        }
        Ok(())
    }

    /// Subscribes the Loop to `instance` at `frequency` on its user's word, as an approved
    /// TickManage would without asking: the watch is admitted under the Chat's current
    /// authority, and a Deny rule for it still refuses it.
    async fn subscribe_loop_for_user(
        &self,
        chat_id: ChatId,
        instance: InstanceRef,
        frequency: TickSubscriptionFrequency,
    ) -> Result<(), AgentError> {
        check_frequency(&instance, frequency).map_err(AgentError::Invalid)?;
        let port = self
            .tick_deliveries()
            .port
            .clone()
            .ok_or_else(|| AgentError::Invalid("Tick Subscriptions are unavailable".into()))?;
        let input =
            json!({"action":"subscribe","tick_instance_id": instance, "frequency": frequency});
        let watch = port
            .prepare_watch(chat_id.into(), &instance, &input, CancellationToken::new())
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
            .map_or((TICK_MANAGE, &input), |admission| {
                (admission.policy_name(), admission.permission_input())
            });
        self.refresh_project_rules(chat_id).await?;
        let mut subscriptions = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            let loop_id = chat.snapshot.chat_loop.id;
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
                PermissionOwner::new(chat_id.into(), loop_id.into())
                    .map_err(|error| AgentError::Invalid(error.to_string()))?,
                &session_operation,
            );
            let decision = chat.snapshot.policy.explicit_match_with_session(
                policy_name,
                permission_input,
                loop_id.into(),
                session.decision,
            );
            if decision
                == Some(tickloop_harness_tools::permissions::ExplicitPermissionDecision::Deny)
            {
                return Err(AgentError::Invalid(format!(
                    "a Deny rule for {policy_name} refuses subscribing to {instance}"
                )));
            }
            chat.snapshot.chat_loop.subscriptions.clone()
        };
        if subscriptions
            .iter()
            .any(|subscription| subscription.instance == instance)
        {
            return Err(AgentError::Invalid(ALREADY_SUBSCRIBED.into()));
        }
        subscriptions.push(TickSubscription {
            paused: false,
            id: tickloop_ids::generate(),
            instance,
            frequency,
            authority,
        });
        self.configure_subscriptions(chat_id, subscriptions).await
    }

    pub(super) async fn tick_subscription_tool(
        &self,
        receipt: &Receipt,
        name: &str,
        input: &Value,
        authority: Option<HostToolAuthority>,
    ) -> Result<ToolResult, String> {
        let outcome = match (name, input["action"].as_str()) {
            (TICK_MANAGE, Some("subscribe")) => {
                self.tick_subscribe(receipt, input, authority).await
            }
            (TICK_MANAGE, Some("unsubscribe")) => self.tick_unsubscribe(receipt, input).await,
            _ => Err("unknown TickManage subscription action".into()),
        };
        if outcome.is_ok()
            && let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id)
        {
            crate::loop_state_notes::acknowledge_subscriptions(chat);
        }
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

    async fn tick_subscribe(
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
        let run_cancel = chat
            .run_cancel(receipt.run_id)
            .cloned()
            .ok_or_else(|| "run is unavailable".to_string())?;
        let mut subscriptions = chat.snapshot.chat_loop.subscriptions.clone();
        let is_receiving = chat.snapshot.chat_loop.receiving.is_receiving();
        drop(chats);
        if let Some(existing) = subscriptions
            .iter()
            .find(|subscription| subscription.instance == instance)
        {
            let ids = self.chat_ids(receipt.chat_id).await;
            return Err(format!(
                "this Chat already subscribes to Tick Instance {} as {}; cancel it with {TICK_MANAGE} before subscribing at another frequency",
                existing.instance.model_text(&ids),
                ids.model_text(SubscriptionId::from(existing.id))
            ));
        }
        let subscription = TickSubscription {
            paused: false,
            id: tickloop_ids::generate(),
            instance,
            frequency,
            authority,
        };
        subscriptions.push(subscription.clone());
        self.configure_subscriptions(receipt.chat_id, subscriptions)
            .await
            .map_err(|error| error.to_string())?;
        let (ready, link) = self
            .wait_for_subscription_ready(subscription.id, &run_cancel)
            .await;
        let ids = self.chat_ids(receipt.chat_id).await;
        let mut answer = json!({
            "subscription": ids.model_text(SubscriptionId::from(subscription.id)),
            "tick_instance_id": subscription.instance.model_text(&ids),
            "frequency": subscription.frequency,
            "paused": subscription.paused,
            "receiving": is_receiving,
            "ready": ready,
        });
        // Subscribing is no consent to continue a paused Loop.
        if !is_receiving {
            answer["note"] = json!(
                "this Chat has paused receiving: the subscription watches its Tick and delivers the latest update once the Chat is continued"
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

    async fn tick_unsubscribe(&self, receipt: &Receipt, input: &Value) -> Result<String, String> {
        let input: TickUnsubscribeInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid {TICK_MANAGE} input: {error}"))?;
        if input.action != "unsubscribe" {
            return Err("TickManage requires action=unsubscribe here".into());
        }
        let ids = self.chat_ids(receipt.chat_id).await;
        let not_found = || tickloop_ids::not_found::<SubscriptionKind>(input.subscription.as_str());
        let subscription = ids.resolve(&input.subscription).map_err(|_| not_found())?;
        self.update_tick_subscription(
            receipt.chat_id,
            subscription.uuid(),
            TickSubscriptionChange::Cancel,
        )
        .await
        .map_err(|error| match error {
            AgentError::NotFound => not_found(),
            error => error.to_string(),
        })?;
        Ok(json!({"subscription": ids.model_text(subscription), "cancelled": true}).to_string())
    }

    /// Reconciles the runtime with the subscriptions of every loaded Chat's Loop and republishes
    /// the desired set. A paused Loop keeps watching, so its subscriptions keep their watches and
    /// pending updates and only wait to be taken.
    pub(super) async fn sync_tick_subscriptions(&self) {
        if !self.inner.tick_stream_installed.load(Ordering::SeqCst) {
            return;
        }
        let saves_chats = self.inner.recorder.is_some();
        let (configured, members) = {
            let chats = self.inner.chats.lock().await;
            let configured: Vec<_> = chats
                .iter()
                .flat_map(|(chat_id, chat)| {
                    let watermarks = chat
                        .tools
                        .chat_directory()
                        .filter(|_| saves_chats)
                        .map(|directory| directory.join(WATERMARK_DIRECTORY).path().to_path_buf());
                    let chat_loop = &chat.snapshot.chat_loop;
                    chat_loop
                        .subscriptions
                        .iter()
                        .filter(|subscription| !subscription.paused)
                        .map(|subscription| {
                            (
                                *chat_id,
                                chat_loop.id,
                                subscription.clone(),
                                watermarks.clone(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            let members: BTreeSet<_> = chats
                .iter()
                .flat_map(|(chat_id, chat)| {
                    chat.snapshot
                        .monitor
                        .members
                        .iter()
                        .filter(|member| !chat.snapshot.monitor.is_removed(&member.instance))
                        .map(|member| (*chat_id, member.instance.clone()))
                })
                .collect();
            (configured, members)
        };
        // Watermark files are read and removed outside the delivery lock.
        let mut cancelled = Vec::new();
        let mut monitor_samples = Vec::new();
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
                if let Some(runtime) = deliveries.subscriptions.remove(&id)
                    && members.contains(&(runtime.chat_id, runtime.instance.clone()))
                    && runtime.instance.access() == "hosted"
                    && runtime.blocked.is_none()
                {
                    let key = (runtime.chat_id, runtime.instance.clone());
                    if deliveries
                        .monitor_watches
                        .get(&key)
                        .is_none_or(|watch| watch.authority != runtime.authority)
                    {
                        monitor_samples.push((runtime.watch, runtime.instance));
                        deliveries.monitor_watches.insert(
                            key,
                            MonitorWatch {
                                watch: runtime.watch,
                                authority: runtime.authority,
                                ready: runtime.ready,
                                series: runtime.series,
                                last_sequence: runtime
                                    .latest
                                    .as_ref()
                                    .map(|latest| latest.sequence)
                                    .max(runtime.delivered.map(|delivered| delivered.sequence)),
                            },
                        );
                    }
                }
                deliveries.content.remove(id);
                // Only a subscription no Loop configures any more loses its watermark.
                if !kept.contains(&id)
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
                                watch: tickloop_ids::generate().into(),
                                ready: false,
                                link: TickLinkState::Initializing,
                                blocked: None,
                                frequency: subscription.frequency,
                                due_at: next_delivery(subscription.frequency, now),
                                latest: None,
                                delivered: None,
                                watermarks,
                                first: None,
                                superseded: 0,
                                accumulated: 0,
                                has_pending_change: false,
                                pending_since: None,
                                program_exit: None,
                                gap: false,
                                last_delivery_ms: None,
                                last_delivery: None,
                                kind: None,
                                title: None,
                                updated_at_ms: None,
                                series: Vec::new(),
                            },
                        );
                    }
                }
            }
            deliveries
                .monitor_watches
                .retain(|member, _| members.contains(member));
            let live_watches: BTreeSet<_> = deliveries
                .subscriptions
                .values()
                .map(|runtime| runtime.watch)
                .chain(deliveries.monitor_watches.values().map(|watch| watch.watch))
                .collect();
            deliveries
                .baselines
                .retain(|watch, _| live_watches.contains(watch));
            deliveries
                .resuming
                .retain(|watch| live_watches.contains(watch));
            deliveries.publish_desired();
            deliveries.ready_changed.notify_waiters();
        }
        if let Some(port) = self.tick_deliveries().port.clone() {
            for (watch, instance) in monitor_samples {
                let port = port.clone();
                let shutdown = self.inner.shutdown.clone();
                self.inner.tasks.spawn(async move {
                    tokio::select! {
                        _ = shutdown.cancelled() => {},
                        _ = tokio::time::timeout(SAMPLE_TIMEOUT, port.sample(watch, &instance)) => {},
                    }
                });
            }
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
        self.withdraw_every_ended_input().await;
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
        let shutdown = self.inner.shutdown.clone();
        self.inner.tasks.spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {}
                _ = agent.deliver(chat_id, loop_id) => {}
            }
        });
    }

    /// Hands one Loop's pending delivery to [`Agent::deliver_trigger`], or marks it due while the
    /// Loop cannot take it, as when it pauses receiving, executes or waits for the user, so it
    /// flushes once the Loop's run ends or it is continued.
    async fn deliver(&self, chat_id: ChatId, loop_id: LoopId) {
        let _upgrade = self.inner.upgrade_gate.read().await;
        let owner = (chat_id, loop_id);
        // The end of a run sets the Loop's state and flushes a held delivery under the Chats
        // lock, so the state is read and the delivery taken or held under it too: a run that ends
        // meanwhile either comes first and leaves the Loop free to take it, or comes after and
        // finds it held.
        let delivery = {
            let chats = self.inner.chats.lock().await;
            let Some(chat) = chats
                .get(&chat_id)
                .filter(|chat| chat.snapshot.chat_loop.id == loop_id)
            else {
                return;
            };
            // Asked of the delivery before its items are known; the entry asks again with them.
            let unprepared = TriggerSource::TickUpdate { items: Vec::new() };
            let takes = self.would_admit(chat, &unprepared);
            let mut deliveries = self.tick_deliveries();
            if takes {
                deliveries.take_delivery(owner)
            } else {
                deliveries.hold(owner, Instant::now());
                None
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
                    .revalidate_watch(chat_id.into(), &item.instance, authority.as_ref())
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
                    Some(port) => port.read_current(chat_id.into(), &item.instance).await,
                    None => Ok(None),
                };
                let put_back = self
                    .tick_deliveries()
                    .content
                    .take_put_back(item.subscription, item.sequence);
                match (current, put_back) {
                    (Ok(Some(current)), _) if current.sequence >= item.sequence => {
                        item.updates += current.sequence - item.sequence;
                        item.sequence = current.sequence;
                        item.observed_at = current.observed_at;
                        item.data = current.data;
                        item.needs_refill = false;
                    }
                    // A Tick that cannot be read again is delivered with what it gave before.
                    (_, Some(data)) => {
                        item.data = data;
                        item.needs_refill = false;
                    }
                    (_, None) => {}
                }
                let still_admitted = match &port {
                    Some(port) => port
                        .revalidate_watch(chat_id.into(), &item.instance, authority.as_ref())
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
        // What was delivered is known only under the lease, once what another process holding it
        // delivered is taken over: the items are sorted out and batched after that. The entry
        // holds back a batch that a later takeover finds delivered.
        let boundary = self.enter_trigger_boundary(chat_id).await;
        match self.acquire_trigger_permit(chat_id, &boundary).await {
            TriggerPermit::Fire(_) => {}
            TriggerPermit::Wait { .. } | TriggerPermit::Withdrawn | TriggerPermit::Failed(_) => {
                self.tick_deliveries().restore(owner, items);
                self.inner.tick_wake.notify_one();
                return;
            }
        }
        drop(boundary);
        {
            let mut deliveries = self.tick_deliveries();
            let (fresh, delivered): (Vec<_>, Vec<_>) = items.into_iter().partition(|item| {
                deliveries
                    .subscriptions
                    .get(&item.subscription)
                    .and_then(|runtime| runtime.delivered)
                    .is_none_or(|delivered| item.sequence > delivered.sequence)
            });
            deliveries.let_go(&delivered);
            items = fresh;
        }
        if items.is_empty() {
            return;
        }
        let ids = {
            let chats = self.inner.chats.lock().await;
            if let Some(chat) = chats.get(&chat_id) {
                for item in &mut items {
                    if let Some(title) = chat
                        .snapshot
                        .monitor
                        .members
                        .iter()
                        .find(|member| member.instance == item.instance)
                        .and_then(|member| member.title.clone())
                    {
                        item.title = Some(title);
                    }
                }
            }
            chats
                .get(&chat_id)
                .map_or_else(IdContext::new, |chat| chat.ids)
        };
        let FittedDelivery {
            payload,
            delivered: items,
            deferred,
            summary,
        } = fit_delivery(&ids, items);
        if !deferred.is_empty() {
            // Held, they flush as soon as the run this delivery starts ends.
            self.tick_deliveries().restore(owner, deferred);
        }
        let marks = {
            let deliveries = self.tick_deliveries();
            let delivery = tickloop_ids::generate();
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
        let source = TriggerSource::TickUpdate {
            items: items
                .iter()
                .map(|item| TickDeliveryItem {
                    subscription: item.subscription,
                    watch: item.watch.into(),
                    instance: item.instance.clone(),
                    sequence: item.sequence,
                    updates: item.updates,
                })
                .collect(),
        };
        let input = match self.goal_background(chat_id).await {
            Some(goal) => {
                tickloop_harness_protocol::local_messages::tick_delivery_with_goal(&payload, &goal)
            }
            None => tickloop_harness_protocol::local_messages::tick_delivery(&payload),
        };
        let outcome = self
            .deliver_trigger(
                chat_id,
                TriggerDelivery {
                    source,
                    input,
                    ledger: None,
                    on_accept: None,
                },
            )
            .await;
        match outcome {
            Ok(_) => {
                self.record_delivery(&marks, &items, &summary);
                self.record_delivered_watermarks(chat_id, marks).await;
            }
            Err(_) => {
                self.tick_deliveries().restore(owner, items);
                self.inner.tick_wake.notify_one();
            }
        }
    }

    /// What an accepted delivery leaves in its subscriptions: the updates it carried are
    /// delivered and no longer wait.
    fn record_delivery(
        &self,
        marks: &[(Uuid, Option<std::path::PathBuf>, DeliveredWatermark)],
        items: &[DeliveryItem],
        summary: &str,
    ) {
        let mut deliveries = self.tick_deliveries();
        let delivered_at = tickloop_harness_base::private_fs::unix_time_milliseconds();
        for (subscription, _, mark) in marks {
            if let Some(runtime) = deliveries.subscriptions.get_mut(subscription)
                && runtime
                    .delivered
                    .is_none_or(|delivered| delivered.sequence < mark.sequence)
            {
                runtime.delivered = Some(*mark);
            }
        }
        deliveries.let_go(items);
        for item in items {
            if let Some(runtime) = deliveries.subscriptions.get_mut(&item.subscription) {
                runtime.last_delivery_ms = Some(delivered_at);
                runtime.last_delivery = Some(summary.to_owned());
            }
        }
    }

    /// Writes the watermarks of an accepted delivery once its acceptance is durable, so no
    /// later process skips an update the ledger does not hold: a crash before this leaves the
    /// update to be delivered again, where the ledger finds it; a crash after it finds the
    /// accepted input queued again when the Chat loads.
    async fn record_delivered_watermarks(
        &self,
        chat_id: ChatId,
        marks: Vec<(Uuid, Option<std::path::PathBuf>, DeliveredWatermark)>,
    ) {
        if marks.iter().all(|(_, directory, _)| directory.is_none()) {
            return;
        }
        let accepted = {
            let chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get(&chat_id) else {
                return;
            };
            self.checkpoint_transcript(&chat.snapshot, &chat.bookkeeping)
                .await
        };
        if accepted.is_err() {
            return;
        }
        // A watermark that cannot be written only lets the update be delivered again, which
        // the ledger holds already.
        let written = tokio::task::spawn_blocking(move || {
            marks
                .into_iter()
                .filter_map(|(subscription, directory, mark)| {
                    let directory = directory?;
                    let _ = write_watermark(&directory, subscription, mark);
                    Some((directory, subscription))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        // A subscription cancelled meanwhile removed its watermark before or after this wrote it;
        // after, its watermark goes with it here.
        let cancelled: Vec<_> = {
            let deliveries = self.tick_deliveries();
            written
                .into_iter()
                .filter(|(_, subscription)| !deliveries.subscriptions.contains_key(subscription))
                .collect()
        };
        if !cancelled.is_empty() {
            let _ = tokio::task::spawn_blocking(move || {
                for (directory, subscription) in cancelled {
                    let _ = remove_watermark(&directory, subscription);
                }
            })
            .await;
        }
    }

    /// The Chat's subscriptions delivered up to the newest of what this process knows, their
    /// watermarks, which another process holding the Chat's lease may have advanced, and the
    /// sequences `in_ledger` the ledger holds: an update at or before them is not delivered
    /// again.
    pub(super) async fn refresh_tick_watermarks(
        &self,
        chat_id: ChatId,
        in_ledger: HashMap<Uuid, (u64, Uuid)>,
    ) {
        let stored: Vec<_> = self
            .tick_deliveries()
            .subscriptions
            .iter()
            .filter(|(_, runtime)| runtime.chat_id == chat_id)
            .filter_map(|(id, runtime)| Some((*id, runtime.watermarks.clone()?)))
            .collect();
        let mut read = tokio::task::spawn_blocking(move || {
            stored
                .into_iter()
                .filter_map(|(subscription, directory)| {
                    read_watermark(&directory, subscription)
                        .ok()
                        .flatten()
                        .map(|mark| (subscription, mark))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        read.extend(
            in_ledger
                .into_iter()
                .map(|(subscription, (sequence, delivery))| {
                    (subscription, DeliveredWatermark { sequence, delivery })
                }),
        );
        let mut deliveries = self.tick_deliveries();
        for (subscription, mark) in read {
            if let Some(runtime) = deliveries.subscriptions.get_mut(&subscription)
                && runtime
                    .delivered
                    .is_none_or(|delivered| delivered.sequence < mark.sequence)
            {
                runtime.delivered = Some(mark);
            }
        }
    }

    /// Proves the delivered items' watches again with their Tickhost; one that fails is no longer
    /// certain to be the Tick its subscription was admitted for.
    pub(super) async fn revalidate_tick_source(
        &self,
        chat_id: ChatId,
        items: &[TickDeliveryItem],
    ) -> Result<(), AgentError> {
        let bindings = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            self.tick_deliveries()
                .item_bindings(chat_id, &chat.snapshot.chat_loop, items)
                .map_err(|_| {
                    AgentError::Invalid("Tick watch is no longer admitted for this Chat".into())
                })?
        };
        let port = self
            .tick_deliveries()
            .port
            .clone()
            .ok_or_else(|| AgentError::Invalid("Tick stream is unavailable".into()))?;
        for binding in bindings {
            if let Err(error) = port
                .revalidate_watch(
                    chat_id.into(),
                    &binding.instance,
                    binding.authority.as_ref(),
                )
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
        Ok(())
    }

    pub(super) fn tick_deliveries(&self) -> std::sync::MutexGuard<'_, TickDeliveries> {
        self.inner
            .tick_deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Owns the stream events and every subscription's delivery schedule.
    fn tick_delivery_task(&self, mut events: tokio::sync::mpsc::Receiver<TickStreamEvent>) {
        let agent = self.clone();
        self.inner.tasks.spawn(async move {
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
                        let _upgrade = agent.inner.upgrade_gate.read().await;
                        let now = Instant::now();
                        agent.sample_due_screens(now);
                        let ready = agent.tick_deliveries().collect_elapsed(now);
                        for (chat_id, loop_id) in ready {
                            agent.deliver(chat_id, loop_id).await;
                        }
                    }
                }
            }
        });
    }

    /// Samples the screen of every Terminal Tick whose subscription's period elapsed by `now`.
    fn sample_due_screens(&self, now: Instant) {
        let due = self
            .tick_deliveries()
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
        self.sample_screens(due);
    }

    /// Reads, side by side and away from the delivery task, the screens of the `due` watches: a
    /// screen that differs from what the watch last got is its update, delivered as soon as it is
    /// read.
    fn sample_screens(&self, due: Vec<(WatchId, InstanceRef)>) {
        let port = self.tick_deliveries().port.clone();
        let Some(port) = port.filter(|_| !due.is_empty()) else {
            return;
        };
        let agent = self.clone();
        self.inner.tasks.spawn(async move {
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
                let _upgrade = agent.inner.upgrade_gate.read().await;
                if let Ok(Some((watch, observation))) = sampled
                    && agent.tick_deliveries().record_sample(watch, &observation)
                {
                    agent.inner.tick_wake.notify_one();
                }
            }
        });
    }

    async fn apply_stream_event(&self, event: TickStreamEvent) {
        let _upgrade = self.inner.upgrade_gate.read().await;
        let resume = if let TickStreamEvent::Ready { watch } = &event {
            let mut deliveries = self.tick_deliveries();
            if deliveries.resuming.remove(watch) {
                deliveries.apply_event(TickStreamEvent::Ready { watch: *watch });
                let target = deliveries
                    .subscriptions
                    .values()
                    .find(|runtime| runtime.watch == *watch)
                    .map(|runtime| {
                        (
                            runtime.chat_id,
                            runtime.instance.clone(),
                            runtime
                                .latest
                                .as_ref()
                                .map(|latest| latest.sequence)
                                .max(runtime.delivered.map(|watermark| watermark.sequence))
                                .max(deliveries.baselines.get(watch).copied()),
                        )
                    })
                    .or_else(|| {
                        deliveries
                            .monitor_watches
                            .iter()
                            .find(|(_, monitor)| monitor.watch == *watch)
                            .map(|((chat, instance), monitor)| {
                                (
                                    *chat,
                                    instance.clone(),
                                    monitor
                                        .last_sequence
                                        .max(deliveries.baselines.get(watch).copied()),
                                )
                            })
                    });
                target
                    .zip(deliveries.port.clone())
                    .map(|(target, port)| (*watch, target, port))
            } else {
                None
            }
        } else {
            None
        };
        let event = if let Some((watch, (chat, instance, previous), port)) = resume {
            match port.read_current(chat.into(), &instance).await {
                Ok(Some(mut observation))
                    if previous.is_some_and(|sequence| observation.sequence > sequence) =>
                {
                    observation.dropped = observation
                        .sequence
                        .saturating_sub(previous.unwrap())
                        .saturating_sub(1);
                    TickStreamEvent::Observation {
                        audience: vec![watch],
                        value: observation,
                    }
                }
                _ => event,
            }
        } else {
            event
        };
        let changed = match &event {
            TickStreamEvent::Observation { audience, value } => Some((&value.instance, audience)),
            TickStreamEvent::ScreenChange {
                instance,
                audience,
                is_pending: true,
            }
            | TickStreamEvent::ProgramExited { instance, audience } => Some((instance, audience)),
            _ => None,
        };
        let is_new_sequence = |previous: Option<u64>| match &event {
            TickStreamEvent::Observation { value, .. } => {
                value.gap || previous.is_none_or(|previous| value.sequence > previous)
            }
            _ => true,
        };
        if let Some((instance, audience)) = changed {
            let (owners, passive, port) = {
                let mut deliveries = self.tick_deliveries();
                let owners: BTreeSet<_> = deliveries
                    .subscriptions
                    .values()
                    .filter(|runtime| {
                        runtime.instance == *instance
                            && runtime.ready
                            && runtime.blocked.is_none()
                            && audience.contains(&runtime.watch)
                            && is_new_sequence(
                                runtime
                                    .latest
                                    .as_ref()
                                    .map(|latest| latest.sequence)
                                    .max(runtime.delivered.map(|delivered| delivered.sequence)),
                            )
                    })
                    .map(|runtime| runtime.chat_id)
                    .chain(
                        deliveries
                            .monitor_watches
                            .iter()
                            .filter(|((_, target), watch)| {
                                target == instance
                                    && watch.ready
                                    && audience.contains(&watch.watch)
                                    && is_new_sequence(watch.last_sequence)
                            })
                            .map(|((chat, _), _)| *chat),
                    )
                    .collect();
                let passive: Vec<_> = deliveries
                    .monitor_watches
                    .iter()
                    .filter(|((_, target), watch)| {
                        target == instance && audience.contains(&watch.watch)
                    })
                    .map(|(_, watch)| watch.watch)
                    .collect();
                if let TickStreamEvent::Observation { value, .. } = &event {
                    for ((_, target), watch) in &mut deliveries.monitor_watches {
                        if target == instance
                            && audience.contains(&watch.watch)
                            && is_new_sequence(watch.last_sequence)
                        {
                            watch.last_sequence = Some(value.sequence);
                        }
                    }
                }
                (owners, passive, deliveries.port.clone())
            };
            let mut chats = self.inner.chats.lock().await;
            for owner in owners {
                if let Some(chat) = chats.get_mut(&owner) {
                    chat.snapshot
                        .monitor
                        .mark_observed(instance.clone(), crate::monitor_members::now_ms());
                    self.record_monitor_membership(
                        owner,
                        chat.snapshot.folder.as_deref(),
                        chat.snapshot.monitor.clone(),
                    );
                }
            }
            drop(chats);
            if matches!(
                event,
                TickStreamEvent::ScreenChange { .. } | TickStreamEvent::ProgramExited { .. }
            ) && let Some(port) = port
            {
                for watch in passive {
                    let instance = instance.clone();
                    let port = port.clone();
                    let shutdown = self.inner.shutdown.clone();
                    self.inner.tasks.spawn(async move {
                        tokio::select! {
                            _ = shutdown.cancelled() => {},
                            _ = tokio::time::timeout(SAMPLE_TIMEOUT, port.sample(watch, &instance)) => {},
                        }
                    });
                }
            }
        }
        if let TickStreamEvent::ProgramExited { instance, audience } = &event {
            let exited = self.tick_deliveries().samplable_watches(instance, audience);
            self.sample_screens(exited);
            return;
        }
        let invalidates_watch = matches!(
            &event,
            TickStreamEvent::State {
                state: TickConnectionState::Gone | TickConnectionState::IdentityUncertain,
                ..
            }
        );
        self.tick_deliveries().apply_event(event);
        if invalidates_watch {
            self.withdraw_every_ended_input().await;
        }
    }
}

/// One delivery as the Loop receives it, and the items that wait for its next turn.
struct FittedDelivery {
    payload: Value,
    delivered: Vec<DeliveryItem>,
    deferred: Vec<DeliveryItem>,
    /// What the delivery handed over, as the subscription view shows it.
    summary: String,
}

/// What a delivery tells the model beside its items.
#[derive(Default)]
struct DeliveryNotes {
    is_trimmed: bool,
    is_screen_trimmed: bool,
    unreadable: usize,
    more_next_turn: usize,
}

/// Fits a delivery within [`MAX_DELIVERY_PAYLOAD_BYTES`]: each item takes at most
/// [`MAX_DELIVERY_ITEM_BYTES`], or the whole delivery when it is the only one, with its data
/// trimmed by segments to fit, and an item that no longer fits waits for the next turn. The
/// first item always goes, which still keeps the cap: its allowance is no more than what the
/// delivery has left, and [`DeliveryItem::delivered`] keeps within it whenever its header does,
/// which bounds every field a Tick reports.
fn fit_delivery(ids: &IdContext, items: Vec<DeliveryItem>) -> FittedDelivery {
    let worst_notes = DeliveryNotes {
        is_trimmed: true,
        is_screen_trimmed: true,
        unreadable: items.len(),
        more_next_turn: items.len(),
    };
    let mut remaining = MAX_DELIVERY_PAYLOAD_BYTES
        .saturating_sub(encoded_len(&delivery_payload(Vec::new(), &worst_notes)));
    let allowance = match items.len() {
        1 => remaining,
        _ => MAX_DELIVERY_ITEM_BYTES.min(remaining),
    };
    let mut notes = DeliveryNotes::default();
    let mut payload_items = Vec::new();
    let mut delivered = Vec::new();
    let mut deferred = Vec::new();
    let mut lines = Vec::new();
    for item in items {
        let header = item.header(ids);
        // Each item after the first is preceded by a comma.
        let separator = usize::from(!delivered.is_empty());
        // One that would not fit even without its data waits untrimmed.
        if !delivered.is_empty() && encoded_len(&header) + separator > remaining {
            deferred.push(item);
            continue;
        }
        let (mut value, mut trimmed) = item.delivered(&header, allowance);
        let mut size = encoded_len(&value) + separator;
        if size > remaining && !delivered.is_empty() {
            // What is left may still hold it trimmed further, unless that would replace values
            // its full share keeps; then it waits for that share in the next turn.
            let (smaller, smaller_trimmed) = item.delivered(&header, remaining - separator);
            let smaller_size = encoded_len(&smaller) + separator;
            let loses_values = omits_values(&smaller_trimmed) && !omits_values(&trimmed);
            if smaller_size > remaining || loses_values {
                deferred.push(item);
                continue;
            }
            (value, trimmed, size) = (smaller, smaller_trimmed, smaller_size);
        }
        let is_trimmed = !trimmed.is_empty();
        remaining = remaining.saturating_sub(size);
        let is_screen = matches!(item.kind, ObservationKind::Terminal { .. });
        notes.is_trimmed |= is_trimmed && !is_screen;
        notes.is_screen_trimmed |= is_trimmed && is_screen;
        notes.unreadable += usize::from(item.needs_refill);
        lines.push(delivery_line(&item, is_trimmed));
        payload_items.push(value);
        delivered.push(item);
    }
    notes.more_next_turn = deferred.len();
    let payload = delivery_payload(payload_items, &notes);
    debug_assert!(encoded_len(&payload) <= MAX_DELIVERY_PAYLOAD_BYTES);
    FittedDelivery {
        payload,
        delivered,
        deferred,
        summary: lines.join(", "),
    }
}

/// One delivered item as the subscription view shows it: its instance and sequence, how many
/// updates it merges and whether its data was trimmed.
fn delivery_line(item: &DeliveryItem, is_trimmed: bool) -> String {
    let mut line = format!(
        "{} #{}",
        item.instance,
        span(&item.first_sequence.to_string(), &item.sequence.to_string())
    );
    if item.updates > 1 {
        line.push_str(&format!(" · {} updates", item.updates));
    }
    if is_trimmed {
        line.push_str(" · trimmed");
    }
    line
}

fn delivery_payload(items: Vec<Value>, notes: &DeliveryNotes) -> Value {
    let mut payload = json!({"items": items});
    let hints: Vec<&str> = [
        (notes.is_trimmed, TRIMMED_HINT),
        (notes.is_screen_trimmed, TRIMMED_SCREEN_HINT),
        (notes.unreadable > 0, UNREADABLE_HINT),
    ]
    .into_iter()
    .filter_map(|(applies, hint)| applies.then_some(hint))
    .collect();
    if !hints.is_empty() {
        payload["recovery"] = json!(hints.join(" "));
    }
    if notes.unreadable > 0 {
        payload["unreadable_instances"] = json!(notes.unreadable);
    }
    if notes.more_next_turn > 0 {
        payload["more_next_turn"] = json!(notes.more_next_turn);
    }
    payload
}

/// What the content cache keeps of Tick Update `data` too large for it until the delivery reads
/// it again: nothing, except how a Terminal Tick's program ended, which a failed read must not
/// lose.
fn left_out(kind: &ObservationKind, data: &Value) -> Value {
    match kind {
        ObservationKind::Terminal { .. } if !data["exit"].is_null() => {
            json!({"text": null, "exit": data["exit"]})
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod manual_admission_tests {
    use super::*;

    /// A subscription no admission let in, as one restored from another Chat's history, to a
    /// Shell another Chat owns never watches it.
    #[tokio::test]
    async fn a_configured_watch_of_another_chats_shell_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            tickloop_harness_test_support::config(),
            Arc::new(tickloop_harness_test_support::ScriptedModel::new(Vec::<
                Vec<tickloop_harness_protocol::Content>,
            >::new(
            ))),
        );
        let owner = agent.create_chat(directory.path().into()).await.unwrap();
        let observer = agent.create_chat(directory.path().into()).await.unwrap();
        let task = agent
            .submit_user_shell(owner, "sleep 30".into(), true)
            .await
            .unwrap();
        agent
            .install_tick_stream(Arc::new(crate::tick_stream::ShellTickStream::new(
                agent.clone(),
            )))
            .await;
        agent
            .configure_subscriptions(
                observer,
                vec![TickSubscription {
                    paused: false,
                    id: Uuid::new_v4(),
                    instance: InstanceRef::shell(task.command_id),
                    frequency: TickSubscriptionFrequency::Immediate,
                    authority: None,
                }],
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let rows = agent.tick_subscriptions(observer).await.unwrap();
                if rows[0].link == TickLinkState::IdentityUncertain {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a foreign Shell watch fails closed before its baseline");
        agent.shutdown().await;
    }

    #[test]
    fn a_loop_refuses_subscriptions_it_cannot_honour() {
        let subscription = |instance: InstanceRef, frequency| TickSubscription {
            paused: false,
            id: Uuid::new_v4(),
            instance,
            frequency,
            authority: None,
        };
        let fixture = InstanceRef::new("calendar", "fixture").unwrap();
        let shell = InstanceRef::shell(Uuid::new_v4());
        let artifact = InstanceRef::artifact("https://docs.example", "doc").unwrap();
        use TickSubscriptionFrequency::{High, Immediate, Low};
        for rejected in [
            vec![subscription(fixture.clone(), Immediate)],
            vec![subscription(shell.clone(), High)],
            vec![subscription(artifact.clone(), High)],
            vec![subscription(artifact, Immediate)],
            vec![
                subscription(fixture.clone(), High),
                subscription(fixture.clone(), Low),
            ],
        ] {
            assert!(validate(&rejected).is_err());
        }
        assert!(validate(&[subscription(fixture, High), subscription(shell, Immediate),]).is_ok());
    }

    #[test]
    fn manual_updates_wait_only_for_plan_approval() {
        let instance = InstanceRef::new("calendar", "fixture").unwrap();
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "folder": null, "messages": [],
            "chat_loop": {"id": Uuid::new_v4(), "state": "Idle"},
        }))
        .unwrap();
        assert_eq!(
            manual_eligibility(&snapshot, &instance),
            Eligibility::Ended(DiscardReason::SourceEnded),
            "a Tick the Chat does not own is no source"
        );
        snapshot
            .monitor
            .members
            .push(tickloop_harness_protocol::MonitorMember {
                is_terminal: false,
                instance: instance.clone(),
                title: None,
            });
        assert_eq!(
            manual_eligibility(&snapshot, &instance),
            Eligibility::Eligible
        );
        for state in [LoopState::Interrupted, LoopState::Failed] {
            snapshot.chat_loop.state = state;
            assert_eq!(
                manual_eligibility(&snapshot, &instance),
                Eligibility::Eligible,
                "how the last turn ended never holds data back"
            );
        }
        snapshot.chat_loop.state = LoopState::WaitingPermission;
        assert_eq!(
            manual_eligibility(&snapshot, &instance),
            Eligibility::Eligible,
            "permission waits may queue data"
        );
        snapshot.chat_loop.state = LoopState::WaitingPlan;
        assert_eq!(
            manual_eligibility(&snapshot, &instance),
            Eligibility::Hold(Hold::PlanReview),
            "data must not approve a plan"
        );
    }
}

#[cfg(test)]
mod put_back_tests {
    use super::*;

    fn item(subscription: Uuid, instance: &InstanceRef, sequence: u64) -> DeliveryItem {
        DeliveryItem {
            title: None,
            subscription,
            watch: Uuid::nil().into(),
            instance: instance.clone(),
            first_sequence: sequence,
            first_observed_at: String::new(),
            sequence,
            observed_at: String::new(),
            updates: 1,
            waiting_since: Instant::now(),
            gap: false,
            kind: ObservationKind::Protocol,
            recover_with: None,
            series: Vec::new(),
            data: json!({ "seq": sequence }),
            needs_refill: false,
        }
    }

    /// Two deliveries of one subscription put back newest first, then the older, within the
    /// budget: the newer update waits, with its data for a Tick that cannot be read again.
    #[tokio::test]
    async fn deliveries_put_back_out_of_order_keep_the_newest_update_and_its_data() {
        let mut deliveries = TickDeliveries::default();
        let owner = (ChatId::new(), LoopId::new());
        let subscription = Uuid::new_v4();
        let instance: InstanceRef = "calendar:fixture".parse().unwrap();
        deliveries.restore(owner, vec![item(subscription, &instance, 2)]);
        deliveries.restore(owner, vec![item(subscription, &instance, 1)]);
        let waiting = deliveries.take_delivery(owner).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].sequence, 2);
        assert_eq!(waiting[0].updates, 2);
        assert_eq!(
            deliveries.content.take_put_back(subscription, 2),
            Some(json!({ "seq": 2 }))
        );

        deliveries.restore(owner, vec![item(subscription, &instance, 3)]);
        deliveries.restore(owner, vec![item(subscription, &instance, 4)]);
        let waiting = deliveries.take_delivery(owner).unwrap();
        assert_eq!(waiting[0].sequence, 4);
        assert_eq!(
            deliveries.content.take_put_back(subscription, 4),
            Some(json!({ "seq": 4 }))
        );
    }
}
