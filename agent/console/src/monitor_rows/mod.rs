//! The Console's Monitor rows. Each registered access (the instance prefix, such as `shell:`)
//! describes its rows, member rows, row intents and refresh from what the View already holds; the
//! runtime's `TickAdapter` of the same access supplies the stream.
mod builtin;
mod probe;

#[cfg(test)]
pub(crate) use builtin::artifact_actions;
pub use builtin::{ArtifactRows, HostedRows, ShellRows};
pub(crate) use builtin::{REMOVE_KEY, SUBSCRIBE_KEY};
pub use probe::{MemberFindings, ProbeError, TickProbe};

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use tick_protocol::InstanceRef;
use tickloop_agent_artifacts::ArtifactMetadata;
use tickloop_agent_protocol::{
    ChatStatus, MonitorMember, MonitorMembership, ShellTaskSnapshot, TickSubscriptionFrequency,
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
    pub const Ticks: Self = Self {
        order: 1,
        name: "Ticks",
    };
    pub const Shells: Self = Self {
        order: 2,
        name: "Shells",
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
    /// Hides the row in this Chat and cancels its Tick Subscriptions.
    RemoveFromMonitor(InstanceRef),
    /// Opens the attached view of a local Tick the Tickhost runs, a hosted Tick or a Shell: a
    /// Herdr popup inside Herdr, otherwise full screen in this terminal.
    OpenView(InstanceRef),
    /// Ends a hosted Tick this Chat opened, or forgets the record of one that already ended, and
    /// takes its row out with the Tick Subscriptions on it.
    CloseHosted {
        instance: InstanceRef,
        ends_process: bool,
    },
    /// Starts a new instance in place of a hosted Tick that no longer runs, in its row's place;
    /// restores the old instance's Tick Subscriptions.
    ReopenHosted(InstanceRef),
    /// Subscribes the Chat to an extension or terminal Tick at the default frequency, or, once one
    /// of its Loops subscribes to it, cycles that subscription through high, medium and low.
    Subscribe(InstanceRef),
    SendUpdate(InstanceRef),
    /// Stops a running Shell.
    StopShell {
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

/// Where the subscription detail of a row starts. Cancelling goes straight to the one
/// subscription when the row has only one; with several the user picks one first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionOperation {
    View,
    Cancel,
}

/// Who controls an instance's lifecycle, as far as this Chat can verify it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TickOwner {
    ThisChat,
    Service(&'static str),
    Unknown,
}
impl TickOwner {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ThisChat => "this Chat",
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

/// How a row marks the periodic Tick Subscriptions on its instance: `↻` in the colour of the most
/// frequent, and whether updates, or a Terminal Tick's changed screen, wait on them for a check to
/// hand them to the Agent. Immediate subscriptions, which Shells and artifacts have by default,
/// are never marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionMark {
    pub frequency: TickSubscriptionFrequency,
    /// Whether one of them holds what its next check hands to the Agent.
    pub is_waiting: bool,
    /// The nearest next check among those holding something, once the Agent reports one.
    pub next_check: Option<NextCheck>,
}

impl SubscriptionMark {
    /// The mark of the periodic subscriptions among `rows`, `None` without any.
    pub fn of(rows: &[&TickSubscriptionStatus]) -> Option<Self> {
        let periodic = rows.iter().filter(|row| row.frequency.period().is_some());
        let frequency = periodic
            .clone()
            .map(|row| row.frequency)
            .min_by_key(|frequency| frequency.period())?;
        let waiting = periodic.filter(|row| row.accumulated > 0 || row.has_pending_change);
        Some(Self {
            frequency,
            is_waiting: waiting.clone().next().is_some(),
            next_check: waiting
                .filter_map(|row| NextCheck::of(row))
                .min_by_key(|check| check.at_ms),
        })
    }
}

/// The next periodic check and the wait from its first pending change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NextCheck {
    pub at_ms: u64,
    pub wait_ms: u64,
}

impl NextCheck {
    fn of(row: &TickSubscriptionStatus) -> Option<Self> {
        let (at_ms, period_ms) = row.next_check_at_ms.zip(row.period_ms)?;
        let wait_ms = row
            .pending_since_ms
            .map_or(period_ms, |since| at_ms.saturating_sub(since).max(1));
        (wait_ms > 0).then_some(Self { at_ms, wait_ms })
    }

    fn wait_started_at(self) -> u64 {
        self.at_ms.saturating_sub(self.wait_ms)
    }

    /// How many of `columns` the wait since the first pending change fills; all of them once
    /// the check is due.
    pub fn elapsed_columns(self, columns: usize, now_ms: u64) -> usize {
        let elapsed = now_ms
            .saturating_sub(self.wait_started_at())
            .min(self.wait_ms);
        (u128::from(elapsed) * columns as u128 / u128::from(self.wait_ms)) as usize
    }

    /// When the elapsed part next fills one more of `columns`; `None` once it fills them all.
    pub fn next_column_at(self, columns: usize, now_ms: u64) -> Option<u64> {
        let filled = self.elapsed_columns(columns, now_ms);
        (filled < columns).then(|| {
            let elapsed =
                (u128::from(self.wait_ms) * (filled as u128 + 1)).div_ceil(columns as u128);
            self.wait_started_at() + elapsed as u64
        })
    }
}

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
    /// What the row shows at its right before its marks: facts its glyph's colour cannot tell,
    /// such as a non-zero exit code or a signal.
    pub notes: Vec<String>,
    /// The row's subscription mark, which the Monitor list sets from the Tick Subscriptions on
    /// the instance.
    pub subscription: Option<SubscriptionMark>,
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
    pub terminal_members: std::collections::HashSet<InstanceRef>,
    /// Shared with the View that read it; listing rows never copies the conversation.
    pub chat: Option<Arc<ChatStatus>>,
    pub shells: Option<ShellTaskSnapshot>,
    pub subscriptions: Vec<TickSubscriptionStatus>,
    pub metadata: HashMap<String, Option<ArtifactMetadata>>,
    pub metadata_failures: HashMap<String, String>,
    /// What the latest host probe found for members no subscription reports on.
    pub member_states: HashMap<InstanceRef, MemberState>,
    pub member_resources: HashMap<InstanceRef, TickResources>,
    /// When protocol members last sent a Tick Update, as their hosts saw it.
    pub member_updates: HashMap<InstanceRef, u64>,
    /// When each running Shell started, as far as the View knows.
    pub shell_starts: HashMap<Uuid, u64>,
    /// The row whose detail is open; it stays listed while the detail is open.
    pub retained: Option<InstanceRef>,
    pub now_ms: u64,
}

pub(crate) fn terminal_exit_status(exit: tickloop_agent_protocol::HostedProgramExit) -> String {
    match (exit.code, exit.signal) {
        (Some(code), _) => format!("exited {code}"),
        (_, Some(signal)) => format!("exited by signal {signal}"),
        _ => "exited".into(),
    }
}

/// A host probe's finding for one member. Probing reads host state only: it never reads Tick
/// content, subscribes or calls the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberState {
    Live,
    /// A hosted terminal Tick whose program ended, with how it ended for its status.
    Exited(String),
    /// Its host died with it; a hosted Tick can be reopened as a new instance.
    Lost,
    Gone,
    /// Its host runs but serves no request of this Agent, such as a Tickhost of another build
    /// the user kept; whether it still runs is unknown, so it is neither lost nor reopened.
    Unavailable,
}

/// The row action Enter and a click run.
pub const MONITOR_ENTER_KEY: char = 'o';
pub const MONITOR_REFRESH_KEY: char = 'r';
/// Cancels the selected subscription directly in its detail.
pub const UNSUBSCRIBE_KEY: char = 'u';
/// What the subscribe key does on a row with a periodic subscription.
const SUBSCRIPTION_ACTION_LABEL: &str = "subscriptions";
/// Shows a subscribed row's Tick Subscriptions; Enter stays with opening the Tick.
pub const SUBSCRIPTIONS_KEY: char = 'w';
/// Keys the Monitor list itself owns. Row actions never use them, so the list's hints always
/// match what a key does.
pub const MONITOR_KEYS: [char; 1] = [MONITOR_REFRESH_KEY];

/// The Monitor half of an access. Rows are computed from the View's own Chat snapshot,
/// subscriptions, Shell tasks, artifact metadata and host probes; they never read Tick content,
/// subscribe or call the model.
pub trait MonitorRows: Send + Sync + 'static {
    fn access(&self) -> &'static str;
    /// Rows this access knows from its own sources (subscriptions, services, tasks).
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor>;
    /// The row for a Chat member the access did not list itself. Accesses whose host can be
    /// probed add the probe's state; the default shows only what the Chat recorded.
    fn member(&self, member: &MonitorMember, _context: &MonitorContext) -> TickDescriptor {
        member_descriptor(member)
    }
    /// What the Monitor refresh key also reloads for this access's rows.
    fn refresh_intent(&self) -> Option<TickIntent> {
        None
    }
    /// Whether Loops follow this access's instances with Tick Subscriptions. An access whose
    /// instances are followed through their own relation, as an artifact is through its
    /// association, never shows a Tick Subscription on its rows.
    fn takes_tick_subscriptions(&self) -> bool {
        true
    }
}

/// The subscriptions on `instance`.
pub fn subscriptions_on(
    subscriptions: &[TickSubscriptionStatus],
    instance: &InstanceRef,
) -> Vec<TickSubscriptionStatus> {
    subscriptions
        .iter()
        .filter(|row| &row.instance == instance)
        .cloned()
        .collect()
}

/// The subscriptions, by the instance they watch.
fn active_subscriptions(
    context: &MonitorContext,
) -> BTreeMap<InstanceRef, Vec<&TickSubscriptionStatus>> {
    let mut subscribed: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for row in &context.subscriptions {
        subscribed
            .entry(row.instance.clone())
            .or_default()
            .push(row);
    }
    subscribed
}

/// The row of a subscribed instance no access lists: what the subscription knows of it.
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
        subscription: None,
        tone: builtin::link_tone(row.link),
        owner: TickOwner::Unknown,
        updated_at_ms: row.updated_at_ms,
        update: builtin::subscription_update(row),
        running: row.link == tickloop_agent_protocol::TickLinkState::Live,
        resources: None,
        actions: vec![remove_from_monitor(&row.instance)],
    }
}

/// Marks the row with its periodic Tick Subscriptions and adds the keys that manage all of them.
/// While one of them can change frequency, which an immediate one cannot, the subscribe key
/// cycles it instead. Enter always opens the Tick itself; a row without its own opening shows the
/// Tick's details.
fn mark_subscribed(entry: &mut TickDescriptor, rows: &[&TickSubscriptionStatus]) {
    entry.subscription = SubscriptionMark::of(rows);
    let instance = entry.instance.clone();
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
    if rows.iter().any(|row| row.frequency.period().is_some()) {
        let is_key_free = !entry
            .actions
            .iter()
            .any(|action| action.key == SUBSCRIBE_KEY);
        match entry
            .actions
            .iter_mut()
            .find(|action| matches!(action.intent, TickIntent::Subscribe(_)))
        {
            Some(subscribe) => subscribe.label = SUBSCRIPTION_ACTION_LABEL,
            None if is_key_free => entry.actions.push(MonitorAction {
                key: SUBSCRIBE_KEY,
                label: SUBSCRIPTION_ACTION_LABEL,
                intent: TickIntent::Subscribe(instance.clone()),
            }),
            None => {}
        }
    }
}

/// Lets the user subscribe an extension or terminal Tick that still exists from its row; Shells and
/// artifacts are subscribed by default.
fn offer_subscribing(entry: &mut TickDescriptor) {
    let is_tick = entry.group == TickGroup::Ticks
        && matches!(entry.glyph, TickGlyph::Protocol | TickGlyph::Terminal);
    if is_tick
        && entry.tone != StatusTone::Gone
        && !entry
            .actions
            .iter()
            .any(|action| action.key == SUBSCRIBE_KEY)
    {
        entry.actions.push(MonitorAction {
            key: SUBSCRIBE_KEY,
            label: "subscriptions",
            intent: TickIntent::Subscribe(entry.instance.clone()),
        });
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
    if let Some(observed) =
        membership.and_then(|membership| membership.observed_at(&entry.instance))
        && entry.updated_at_ms.is_none_or(|updated| updated < observed)
    {
        entry.updated_at_ms = Some(observed);
        entry.update = Some(RowUpdate::new(observed));
    }
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

/// A member row with only the facts the Chat recorded: no invented status, time or resources.
pub fn member_descriptor(member: &MonitorMember) -> TickDescriptor {
    TickDescriptor {
        title: member
            .title
            .clone()
            .unwrap_or_else(|| member.instance.to_string()),
        instance: member.instance.clone(),
        group: TickGroup::Ticks,
        glyph: if member.is_terminal {
            TickGlyph::Terminal
        } else {
            TickGlyph::Protocol
        },
        kind: if member.is_terminal {
            "Terminal"
        } else {
            "Tick"
        }
        .into(),
        status: String::new(),
        notes: Vec::new(),
        subscription: None,
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
        key: REMOVE_KEY,
        label: "remove",
        intent: TickIntent::RemoveFromMonitor(instance.clone()),
    }
}

/// The Monitor rows of every registered access, at most one per access.
#[derive(Clone)]
pub struct MonitorRowsRegistry {
    rows: BTreeMap<&'static str, Arc<dyn MonitorRows>>,
}
impl MonitorRowsRegistry {
    pub fn new(rows: impl IntoIterator<Item = Arc<dyn MonitorRows>>) -> Result<Self, String> {
        let mut registered = BTreeMap::new();
        for access_rows in rows {
            InstanceRef::validate_access(access_rows.access())?;
            if registered
                .insert(access_rows.access(), access_rows)
                .is_some()
            {
                return Err("duplicate Monitor rows access".into());
            }
        }
        Ok(Self { rows: registered })
    }
    pub fn builtin() -> Self {
        Self::new([
            Arc::new(ArtifactRows) as Arc<dyn MonitorRows>,
            Arc::new(ShellRows),
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
                .flat_map(|access_rows| access_rows.list(context)),
        );
        let mut subscribed = active_subscriptions(context);
        subscribed.retain(|instance, _| {
            self.rows
                .get(instance.access())
                .is_none_or(|access_rows| access_rows.takes_tick_subscriptions())
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
                        // A subscription knows an extension Tick only by its declared title; the
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
                        let row = match self.rows.get(member.instance.access()) {
                            Some(access_rows) => access_rows.member(member, context),
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
            entry.actions.retain(|action| {
                let reserved = MONITOR_KEYS.contains(&action.key);
                debug_assert!(
                    !reserved,
                    "Monitor row action {:?} uses a key the Monitor list owns",
                    action.key
                );
                !reserved
            });
            offer_subscribing(entry);
            let rows = subscribed
                .get(&entry.instance)
                .map_or(&[][..], Vec::as_slice);
            if !rows.is_empty() {
                mark_subscribed(entry, rows);
            }
            if rows.is_empty()
                && entry.instance.access() == "hosted"
                && entry.running
                && entry.owner == TickOwner::ThisChat
            {
                entry.actions.push(MonitorAction {
                    key: 'u',
                    label: "send update",
                    intent: TickIntent::SendUpdate(entry.instance.clone()),
                });
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
            .filter_map(|access_rows| access_rows.refresh_intent())
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
