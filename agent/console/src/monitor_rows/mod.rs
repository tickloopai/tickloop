//! The Console's Monitor rows. Each registered Tick type describes its rows, member rows, row
//! intents and refresh from what the View already holds; the runtime's `TickAdapter` of the same
//! `<type>` supplies the stream.
mod builtin;
mod probe;

#[cfg(test)]
pub(crate) use builtin::artifact_actions;
pub use builtin::{
    ArtifactRows, FINISHED_SHELL_DISPLAY_MS, HerdrRows, HostedRows, ShellRows, finished_shell_until,
};
pub(crate) use builtin::{
    HOSTED_CLOSE_KEY, HOSTED_REOPEN_KEY, feedback_binding, key_command, shell_tone, signal_name,
};
pub use probe::{HerdrProbe, MemberFindings, ProbeError, TickProbe};

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use tick_protocol::InstanceRef;
use tickloop_agent_artifacts::ArtifactMetadata;
use tickloop_agent_protocol::{
    ChatSnapshot, LoopState, MonitorMember, MonitorMembership, ShellTaskSnapshot,
    TickSubscriptionStatus,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TickGroup {
    pub order: u16,
    pub name: &'static str,
}
#[allow(non_upper_case_globals)]
impl TickGroup {
    pub const Artifacts: Self = Self {
        order: 0,
        name: "Artifacts",
    };
    pub const Shells: Self = Self {
        order: 1,
        name: "Shells",
    };
    pub const Ticks: Self = Self {
        order: 2,
        name: "Ticks",
    };
    pub fn title(self) -> &'static str {
        self.name
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TickGlyph {
    Artifact,
    Shell,
    Protocol,
    Terminal,
}

/// What a Monitor row action means, as plain data. Rows only describe it; the Console carries it
/// out with its own state, such as the selected Chat, the row and any panel it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TickIntent {
    /// Shows a notice instead of acting.
    Notify(String),
    ShowDetails(InstanceRef),
    /// Hides the row in this Chat only and keeps every other fact. A row its Loops subscribe to
    /// is hidden only after the user confirms cancelling those subscriptions.
    RemoveFromMonitor(InstanceRef),
    OpenHosted(InstanceRef),
    ShowHostedSnapshot(InstanceRef),
    /// Ends a hosted Tick; the Console asks for confirmation first.
    CloseHosted(InstanceRef),
    /// Starts a new instance in place of a lost hosted Tick; the Console asks for confirmation
    /// first.
    ReopenHosted(InstanceRef),
    OpenShell {
        task: Uuid,
    },
    /// An Artifact row action on instance `id`: open, remove, copy or feedback by its key.
    Artifact {
        id: String,
        key: char,
    },
    RefreshArtifacts,
    /// Shows the Tick Subscriptions on the instance inside Monitor, starting with `operation`.
    ManageSubscriptions {
        instance: InstanceRef,
        operation: SubscriptionOperation,
    },
}

/// Where the subscription detail of a row starts. Editing and cancelling go straight to the one
/// subscription when the row has only one; with several the user picks one first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionOperation {
    View,
    EditFrequency,
    Cancel,
}

/// Who controls an instance's lifecycle, as far as this Chat can verify it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TickOwner {
    ThisChat,
    /// A program the user started in Herdr; Herdr owns its Pane and process.
    Herdr,
    Service(&'static str),
    Unknown,
}
impl TickOwner {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ThisChat => "this Chat",
            Self::Herdr => "Herdr",
            Self::Service(name) => name,
            Self::Unknown => "unknown",
        }
    }
}

/// How a row's status reads at a glance. The Console paints the row's leading type glyph in the
/// state colour shared with the Subagents panel, blinking while it runs; the status words stay
/// for the row's detail.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StatusTone {
    #[default]
    Neutral,
    /// Still running; `since_ms` is when it started, when the View knows it.
    Running {
        since_ms: Option<u64>,
    },
    Live,
    /// Reachable only partly or uncertainly, such as a disconnected link.
    Degraded,
    Gone,
    Succeeded,
    Failed,
    Stopped,
}

/// One sample of the business process. Absent values stay absent; nothing is shown as zero.
#[derive(Clone, Debug, PartialEq)]
pub struct TickResources {
    pub pid: u32,
    pub cpu_time_ms: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub sampled_at_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorAction {
    pub key: char,
    pub label: &'static str,
    pub intent: TickIntent,
}
/// A row's latest content update as the current Chat sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowUpdate {
    pub at_ms: u64,
    /// `None` once the Chat handled the update.
    pub unread: Option<Unread>,
    /// When one of the Chat's Agent turns took the update in, once one did; the user opening the
    /// row handles it without one.
    pub taken_in_at_ms: Option<u64>,
}

/// An update the Chat has not handled yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unread {
    /// How many updates wait, when the source counts them, such as a subscription's updates
    /// waiting for delivery.
    pub count: Option<u64>,
}

impl RowUpdate {
    /// An update the Chat has not been checked against yet.
    pub fn new(at_ms: u64) -> Self {
        Self {
            at_ms,
            unread: Some(Unread::default()),
            taken_in_at_ms: None,
        }
    }
}

/// How long a gone instance keeps its row before it leaves Monitor.
pub const GONE_DISPLAY_MS: u64 = 30_000;

#[derive(Clone, Debug)]
pub struct TickDescriptor {
    pub instance: InstanceRef,
    pub group: TickGroup,
    pub glyph: TickGlyph,
    pub title: String,
    pub kind: String,
    /// The state in words, such as `exit 0`, `live` or `gone`, for the row's detail. The row
    /// itself tells the state only by its glyph's colour.
    pub status: String,
    /// What the row shows at its right before its latest update: facts its glyph's colour
    /// cannot tell, such as a non-zero exit code, a signal or the subscription marker.
    pub notes: Vec<String>,
    /// Whether a Loop of this Chat subscribes to the instance through its type rather than a Tick
    /// Subscription, as an artifact's association does with its @tickloop requests. It shows as
    /// the subscription marker without a frequency, and the row's own `x` ends it. Only a type
    /// that takes no Tick Subscriptions sets it.
    pub inherent_subscription: bool,
    pub owner: TickOwner,
    pub tone: StatusTone,
    /// The latest business update; resource and screen refreshes never set it.
    pub updated_at_ms: Option<u64>,
    /// The latest update that counts for unread: a Tick Update or observation, an artifact's
    /// human edit or feedback request, or a background Shell's end.
    pub update: Option<RowUpdate>,
    pub running: bool,
    pub resources: Option<TickResources>,
    pub actions: Vec<MonitorAction>,
}
#[derive(Default)]
pub struct MonitorContext {
    pub chat: Option<ChatSnapshot>,
    pub shells: Option<ShellTaskSnapshot>,
    pub subscriptions: Vec<TickSubscriptionStatus>,
    pub metadata: HashMap<String, Option<ArtifactMetadata>>,
    pub metadata_failures: HashMap<String, String>,
    /// What the latest host probe found for members no subscription reports on.
    pub member_states: HashMap<InstanceRef, MemberState>,
    pub member_resources: HashMap<InstanceRef, TickResources>,
    /// What the latest host probe calls members named by what they run.
    pub member_titles: HashMap<InstanceRef, String>,
    /// When protocol members last sent a Tick Update, as their hosts saw it.
    pub member_updates: HashMap<InstanceRef, u64>,
    /// Since when each instance has shown as gone; it leaves Monitor [`GONE_DISPLAY_MS`] later.
    pub gone_since: HashMap<InstanceRef, u64>,
    /// When each running Shell started, as far as the View knows.
    pub shell_starts: HashMap<Uuid, u64>,
    /// The row whose detail is open; it stays listed while the detail is open.
    pub retained: Option<InstanceRef>,
    pub now_ms: u64,
}

/// A host probe's finding for one member. Probing reads host state only: it never reads Tick
/// content, subscribes or calls the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberState {
    Live {
        protocol: bool,
    },
    /// Its host died with it; a hosted Tick can be reopened as a new instance.
    Lost,
    Gone,
    IdentityUncertain,
}

/// The row action Enter and a click run.
pub const MONITOR_ENTER_KEY: char = 'o';
pub const MONITOR_REFRESH_KEY: char = 'r';
/// Changes the frequency of a subscribed row's subscription.
pub const SUBSCRIPTION_FREQUENCY_KEY: char = 'e';
/// Cancels a subscribed row's subscription after confirmation.
pub const UNSUBSCRIBE_KEY: char = 'u';
/// Shows a subscribed row's Tick Subscriptions; Enter stays with opening the Tick.
pub const SUBSCRIPTIONS_KEY: char = 'w';
/// Keys the Monitor list itself owns. Row actions never use them, so the list's hints always
/// match what a key does.
pub const MONITOR_KEYS: [char; 1] = [MONITOR_REFRESH_KEY];

/// The Monitor half of a Tick type. Rows are computed from the View's own Chat snapshot,
/// subscriptions, Shell tasks, artifact metadata and host probes; they never read Tick content,
/// subscribe or call the model.
pub trait MonitorRows: Send + Sync + 'static {
    fn tick_type(&self) -> &'static str;
    /// Rows this type knows from its own sources (subscriptions, services, tasks).
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor>;
    /// The row for a Chat member the type did not list itself. Types whose host can be probed
    /// add the probe's state; the default shows only what the Chat recorded.
    fn member(&self, member: &MonitorMember, _context: &MonitorContext) -> TickDescriptor {
        member_descriptor(member)
    }
    /// What the Monitor refresh key also reloads for this type's rows.
    fn refresh_intent(&self) -> Option<TickIntent> {
        None
    }
    /// Whether Loops follow this type's instances with Tick Subscriptions. A type that subscribes
    /// through its own relation, as an artifact does through its association, never shows a Tick
    /// Subscription on its rows.
    fn takes_tick_subscriptions(&self) -> bool {
        true
    }
}

/// Whether the subscription's Loop can still deliver; a stopped or expired one cannot.
fn can_deliver(row: &TickSubscriptionStatus) -> bool {
    !matches!(row.loop_state, LoopState::Stopped | LoopState::Expired)
}

/// The subscriptions on `instance` whose Loops can still deliver.
pub fn subscriptions_on(
    subscriptions: &[TickSubscriptionStatus],
    instance: &InstanceRef,
) -> Vec<TickSubscriptionStatus> {
    subscriptions
        .iter()
        .filter(|row| &row.instance == instance && can_deliver(row))
        .cloned()
        .collect()
}

/// The subscriptions of Loops that can still deliver, by the instance they watch.
fn active_subscriptions(
    context: &MonitorContext,
) -> BTreeMap<InstanceRef, Vec<&TickSubscriptionStatus>> {
    let mut subscribed: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for row in context.subscriptions.iter().filter(|row| can_deliver(row)) {
        subscribed
            .entry(row.instance.clone())
            .or_default()
            .push(row);
    }
    subscribed
}

/// The row of a subscribed instance no Tick type lists: what the subscription knows of it.
fn subscription_descriptor(row: &TickSubscriptionStatus) -> TickDescriptor {
    let terminal = matches!(
        row.kind,
        Some(tickloop_agent_protocol::ObservationKind::Terminal { .. })
    );
    TickDescriptor {
        instance: row.instance.clone(),
        group: TickGroup::Ticks,
        glyph: if terminal {
            TickGlyph::Terminal
        } else {
            TickGlyph::Protocol
        },
        title: row
            .title
            .clone()
            .unwrap_or_else(|| row.instance.to_string()),
        kind: if terminal { "Terminal" } else { "Tick" }.into(),
        status: builtin::link_status(row.link).into(),
        notes: Vec::new(),
        inherent_subscription: false,
        tone: builtin::link_tone(row.link),
        owner: TickOwner::Unknown,
        updated_at_ms: row.updated_at_ms,
        update: builtin::subscription_update(row),
        running: row.link == tickloop_agent_protocol::TickLinkState::Live,
        resources: None,
        actions: vec![remove_from_monitor(&row.instance)],
    }
}

/// Shows the row's subscriptions as a marker, `⟳` for the type's inherent one, which has no
/// frequency, `⟳ high` for one Tick Subscription at its frequency or `⟳ 2` for several, and adds
/// the keys that manage Tick Subscriptions; the frequency key only while one of them can change,
/// which an immediate one cannot. Enter always opens the Tick itself; a row without its own opening
/// shows the Tick's details.
fn mark_subscribed(entry: &mut TickDescriptor, rows: &[&TickSubscriptionStatus]) {
    entry.notes.push(match rows {
        [] => "⟳".into(),
        [row] => format!("⟳ {}", row.frequency.name()),
        rows => format!("⟳ {}", rows.len()),
    });
    if rows.is_empty() {
        return;
    }
    let instance = entry.instance.clone();
    let manage = |operation| TickIntent::ManageSubscriptions {
        instance: instance.clone(),
        operation,
    };
    if !entry
        .actions
        .iter()
        .any(|action| action.key == MONITOR_ENTER_KEY)
    {
        entry.actions.insert(
            0,
            MonitorAction {
                key: MONITOR_ENTER_KEY,
                label: "open",
                intent: TickIntent::ShowDetails(instance.clone()),
            },
        );
    }
    let can_change_frequency = rows.iter().any(|row| row.frequency.period().is_some());
    for (key, label, operation) in [
        (
            SUBSCRIPTIONS_KEY,
            "subscriptions",
            SubscriptionOperation::View,
        ),
        (
            SUBSCRIPTION_FREQUENCY_KEY,
            "frequency",
            SubscriptionOperation::EditFrequency,
        ),
        (
            UNSUBSCRIBE_KEY,
            "unsubscribe",
            SubscriptionOperation::Cancel,
        ),
    ] {
        if key == SUBSCRIPTION_FREQUENCY_KEY && !can_change_frequency {
            continue;
        }
        if !entry.actions.iter().any(|action| action.key == key) {
            entry.actions.push(MonitorAction {
                key,
                label,
                intent: manage(operation),
            });
        }
    }
}

/// Settles whether the Chat still has the row's latest update to handle: handled once a turn
/// took it in or the user opened the row at or after it, and taken in only by the turn; while
/// unread, a subscription's count of updates waiting for delivery counts it.
fn resolve_unread(
    entry: &mut TickDescriptor,
    membership: Option<&MonitorMembership>,
    accumulated: u64,
) {
    let handled_at = membership.and_then(|membership| membership.handled_at(&entry.instance));
    let taken_in_at = membership.and_then(|membership| membership.taken_in_at(&entry.instance));
    let Some(update) = &mut entry.update else {
        return;
    };
    if handled_at.is_some_and(|handled| handled >= update.at_ms) {
        update.unread = None;
        update.taken_in_at_ms = taken_in_at.filter(|taken_in| *taken_in >= update.at_ms);
    } else if accumulated > 0 {
        update.unread = Some(Unread {
            count: Some(accumulated),
        });
    }
}

/// Whether a gone Tick has had its time on screen: it leaves [`GONE_DISPLAY_MS`] after it went,
/// unless a Loop still subscribes to it, it has an unread update, or it was lost with its host
/// and can be reopened. Artifacts and Shells follow their own rules.
fn has_left_as_gone(entry: &TickDescriptor, context: &MonitorContext, subscribed: bool) -> bool {
    entry.group == TickGroup::Ticks
        && entry.tone == StatusTone::Gone
        && !subscribed
        && entry
            .update
            .as_ref()
            .is_none_or(|update| update.unread.is_none())
        && !matches!(
            context.member_states.get(&entry.instance),
            Some(MemberState::Lost)
        )
        && context
            .gone_since
            .get(&entry.instance)
            .is_some_and(|since| since.saturating_add(GONE_DISPLAY_MS) <= context.now_ms)
}

/// A member row with only the facts the Chat recorded: no invented status, time or resources.
pub fn member_descriptor(member: &MonitorMember) -> TickDescriptor {
    TickDescriptor {
        title: member
            .title
            .clone()
            .unwrap_or_else(|| member.instance.to_string()),
        instance: member.instance.clone(),
        group: TickGroup::Ticks,
        glyph: TickGlyph::Protocol,
        kind: "Tick".into(),
        status: String::new(),
        notes: Vec::new(),
        inherent_subscription: false,
        owner: TickOwner::ThisChat,
        tone: StatusTone::Neutral,
        updated_at_ms: None,
        update: None,
        running: false,
        resources: None,
        actions: vec![
            MonitorAction {
                key: MONITOR_ENTER_KEY,
                label: "details",
                intent: TickIntent::ShowDetails(member.instance.clone()),
            },
            remove_from_monitor(&member.instance),
        ],
    }
}

/// The shared x action: hides the row in this Chat only and keeps every other fact.
pub fn remove_from_monitor(instance: &InstanceRef) -> MonitorAction {
    MonitorAction {
        key: 'x',
        label: "remove",
        intent: TickIntent::RemoveFromMonitor(instance.clone()),
    }
}

/// The Monitor rows of every registered Tick type, at most one per `<type>`.
#[derive(Clone)]
pub struct MonitorRowsRegistry {
    rows: BTreeMap<&'static str, Arc<dyn MonitorRows>>,
}
impl MonitorRowsRegistry {
    pub fn new(rows: impl IntoIterator<Item = Arc<dyn MonitorRows>>) -> Result<Self, String> {
        let mut registered = BTreeMap::new();
        for type_rows in rows {
            InstanceRef::validate_type(type_rows.tick_type())?;
            if registered
                .insert(type_rows.tick_type(), type_rows)
                .is_some()
            {
                return Err("duplicate Monitor rows type".into());
            }
        }
        Ok(Self { rows: registered })
    }
    pub fn builtin() -> Self {
        Self::new([
            Arc::new(ArtifactRows) as Arc<dyn MonitorRows>,
            Arc::new(ShellRows),
            Arc::new(HerdrRows),
            Arc::new(HostedRows),
        ])
        .expect("unique built-in Monitor rows")
    }
    /// The Chat's complete deduplicated Monitor set: each type's rows plus the members the Chat
    /// opened, minus the instances the user removed. An instance a Loop holds a Tick Subscription
    /// to always has a row, so no subscription runs out of sight; a type that takes none never
    /// shows one. Listing never subscribes or calls the model.
    pub fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        let mut merged = merge_descriptors(
            self.rows
                .values()
                .flat_map(|type_rows| type_rows.list(context)),
        );
        let mut subscribed = active_subscriptions(context);
        subscribed.retain(|instance, _| {
            self.rows
                .get(instance.tick_type())
                .is_none_or(|type_rows| type_rows.takes_tick_subscriptions())
        });
        for (instance, rows) in &subscribed {
            merged
                .entry(instance.clone())
                .or_insert_with(|| subscription_descriptor(rows[0]));
        }
        if let Some(chat) = &context.chat {
            for member in &chat.monitor.members {
                match merged.get_mut(&member.instance) {
                    Some(entry) => {
                        // A subscription knows a protocol Tick only by its declared title; the
                        // member's name adds the arguments that tell its instances apart.
                        if let Some(title) = &member.title
                            && (entry.title == entry.instance.as_str()
                                || entry.glyph == TickGlyph::Protocol)
                        {
                            entry.title = title.clone();
                        }
                        entry.owner = TickOwner::ThisChat;
                    }
                    None => {
                        let row = match self.rows.get(member.instance.tick_type()) {
                            Some(type_rows) => type_rows.member(member, context),
                            None => member_descriptor(member),
                        };
                        merged.insert(member.instance.clone(), row);
                    }
                }
            }
            merged.retain(|instance, _| {
                !chat.monitor.is_removed(instance) || subscribed.contains_key(instance)
            });
        }
        let mut entries = merged.into_values().collect::<Vec<_>>();
        for entry in &mut entries {
            if let Some(title) = context.member_titles.get(&entry.instance) {
                entry.title = title.clone();
            }
            entry.actions.retain(|action| {
                let reserved = MONITOR_KEYS.contains(&action.key);
                debug_assert!(
                    !reserved,
                    "Monitor row action {:?} uses a key the Monitor list owns",
                    action.key
                );
                !reserved
            });
            let rows = subscribed
                .get(&entry.instance)
                .map_or(&[][..], Vec::as_slice);
            if entry.inherent_subscription || !rows.is_empty() {
                mark_subscribed(entry, rows);
            }
            let accumulated = subscribed
                .get(&entry.instance)
                .map_or(0, |rows| rows.iter().map(|row| row.accumulated).sum());
            resolve_unread(
                entry,
                context.chat.as_ref().map(|chat| &chat.monitor),
                accumulated,
            );
        }
        entries.retain(|entry| {
            !has_left_as_gone(entry, context, subscribed.contains_key(&entry.instance))
        });
        entries.sort_by(descriptor_order);
        entries
    }
    /// One page of [`Self::list`]. Pages partition the set in a stable group-and-identity order,
    /// so updates never move a row to another page; within a page rows keep the display order.
    /// A page never stands for the complete set, and a membership change expires older cursors.
    pub fn page(
        &self,
        context: &MonitorContext,
        cursor: Option<&tickhost_connect::page::PageCursor>,
        limit: Option<usize>,
    ) -> Result<tickhost_connect::page::Page<TickDescriptor>, tickhost_connect::page::PageError>
    {
        page_of(self.list(context), cursor, limit)
    }
    pub fn refresh_intents(&self) -> Vec<TickIntent> {
        self.rows
            .values()
            .filter_map(|type_rows| type_rows.refresh_intent())
            .collect()
    }
}

/// One page of an already listed Monitor set; see [`MonitorRowsRegistry::page`].
pub fn page_of(
    entries: Vec<TickDescriptor>,
    cursor: Option<&tickhost_connect::page::PageCursor>,
    limit: Option<usize>,
) -> Result<tickhost_connect::page::Page<TickDescriptor>, tickhost_connect::page::PageError> {
    let mut page = tickhost_connect::page::paginate(
        entries,
        |entry| format!("{:05}:{}", entry.group.order, entry.instance),
        cursor,
        limit,
    )?;
    page.items.sort_by(descriptor_order);
    Ok(page)
}

fn descriptor_order(left: &TickDescriptor, right: &TickDescriptor) -> std::cmp::Ordering {
    display_order(
        (left.group, left.updated_at_ms, &left.instance),
        (right.group, right.updated_at_ms, &right.instance),
    )
}

/// Groups in their declared order; within a group the most recently updated first.
pub(crate) fn display_order(
    left: (TickGroup, Option<u64>, &InstanceRef),
    right: (TickGroup, Option<u64>, &InstanceRef),
) -> std::cmp::Ordering {
    left.0
        .cmp(&right.0)
        .then_with(|| right.1.cmp(&left.1))
        .then_with(|| left.2.cmp(right.2))
}

/// Folds every projection of one instance into one row without losing facts: the newest
/// projection leads, while the latest update time, running state, title, owner, resources and
/// actions from the others are kept.
fn merge_descriptors(
    entries: impl IntoIterator<Item = TickDescriptor>,
) -> BTreeMap<InstanceRef, TickDescriptor> {
    let mut merged: BTreeMap<InstanceRef, TickDescriptor> = BTreeMap::new();
    for entry in entries {
        match merged.entry(entry.instance.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let current = slot.get_mut();
                let rank = |entry: &TickDescriptor| {
                    (
                        entry.updated_at_ms,
                        entry.running,
                        entry.title != entry.instance.as_str(),
                    )
                };
                let (mut lead, other) = if rank(&entry) > rank(current) {
                    (entry, current.clone())
                } else {
                    (current.clone(), entry)
                };
                lead.updated_at_ms = lead.updated_at_ms.max(other.updated_at_ms);
                if other.update.as_ref().map(|update| update.at_ms)
                    > lead.update.as_ref().map(|update| update.at_ms)
                {
                    lead.update = other.update.clone();
                }
                lead.running |= other.running;
                lead.inherent_subscription |= other.inherent_subscription;
                if lead.title == lead.instance.as_str() {
                    lead.title = other.title;
                }
                if lead.status.is_empty() && lead.tone == StatusTone::Neutral {
                    lead.status = other.status;
                    lead.notes = other.notes;
                    lead.tone = other.tone;
                }
                if lead.owner == TickOwner::Unknown {
                    lead.owner = other.owner;
                }
                if other.resources.as_ref().map(|sample| sample.sampled_at_ms)
                    > lead.resources.as_ref().map(|sample| sample.sampled_at_ms)
                {
                    lead.resources = other.resources;
                }
                for action in other.actions {
                    if !lead.actions.iter().any(|known| known.key == action.key) {
                        lead.actions.push(action);
                    }
                }
                *current = lead;
            }
        }
    }
    merged
}
