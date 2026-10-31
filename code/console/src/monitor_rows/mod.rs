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
use tickloop_harness::artifacts::ArtifactMetadata;
use tickloop_harness::protocol::{
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
    /// Shows the live screen of an extension Tick as a Monitor card under its row, which Enter
    /// enters.
    OpenView(InstanceRef),
    /// Shows the live screen of a Terminal Tick or a Shell full screen in this terminal.
    OpenFullScreen(InstanceRef),
    /// Reopens an ended Terminal Tick this Chat opened in its row's place and shows the new
    /// instance full screen.
    ReopenFullScreen(InstanceRef),
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
    /// When one of the Chat's Agent turns took the update in, once one did; an Agent read
    /// handles it without one.
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
    pub paused: bool,
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
        let paused = periodic.clone().all(|row| row.paused);
        let waiting = periodic
            .filter(|row| !row.paused)
            .filter(|row| row.accumulated > 0 || row.has_pending_change);
        Some(Self {
            paused,
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
    /// How a hosted Tick ended once its process no longer runs, as its card's rule says it:
    /// `exited 3`, `ended` or `lost`. A Shell's own screen tells how it ended.
    pub ended: Option<String>,
    pub resources: Option<TickResources>,
    pub actions: Vec<MonitorAction>,
    /// When the row joined the Monitor, as far as its source tells: when an Artifact was linked
    /// to the Chat, or the row's place a reopened Tick took over. Listing fills it in from a
    /// time-ordered identity otherwise.
    pub joined_at_ms: Option<u64>,
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

/// A hosted Tick's card says `ended` once the Tick is gone with no exit status to tell.
pub(crate) const TICK_ENDED: &str = "ended";

pub(crate) fn terminal_exit_status(exit: tickloop_harness::protocol::HostedProgramExit) -> String {
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
pub const MONITOR_PAUSE_KEY: char = 'p';
/// Hides every shown card, without hiding it from the Agent's use.
pub const MONITOR_HIDE_CARDS_KEY: char = 'z';
/// Shows the card of every row that has one, as the user's.
pub const MONITOR_SHOW_CARDS_KEY: char = 'Z';
pub const MONITOR_KEYS: [char; 4] = [
    MONITOR_REFRESH_KEY,
    MONITOR_PAUSE_KEY,
    MONITOR_HIDE_CARDS_KEY,
    MONITOR_SHOW_CARDS_KEY,
];

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
        Some(tickloop_harness::protocol::ObservationKind::Terminal { .. })
    );
    TickDescriptor {
        joined_at_ms: None,
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
        running: row.link == tickloop_harness::protocol::TickLinkState::Live,
        ended: None,
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
/// took it in or the Agent read the instance at or after it, and taken in only by the turn; while
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
        joined_at_ms: member.joined_at_ms,
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
        ended: None,
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
            // Beside periodic subscriptions, Send Update hands over what they hold at once.
            if rows.iter().all(|row| row.frequency.period().is_some())
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
        for entry in &mut entries {
            entry.joined_at_ms = entry
                .joined_at_ms
                .or_else(|| identity_time_ms(&entry.instance));
        }
        entries.sort_by(descriptor_order);
        entries
    }
    /// One page of [`Self::list`]. Pages partition the set in the display order, which updates
    /// never change, so no update moves a row to another page or within one.
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
    // The key sorts as the display order does: no join time sorts after every join time.
    let mut page = tickhost_connect::page::paginate(
        entries,
        |entry| {
            format!(
                "{:05}:{:020}:{}",
                entry.group.order,
                entry.joined_at_ms.unwrap_or(u64::MAX),
                entry.instance
            )
        },
        cursor,
        limit,
    )?;
    page.items.sort_by(descriptor_order);
    Ok(page)
}

fn descriptor_order(left: &TickDescriptor, right: &TickDescriptor) -> std::cmp::Ordering {
    display_order(
        (left.group, left.joined_at_ms, &left.instance),
        (right.group, right.joined_at_ms, &right.instance),
    )
}

/// Groups in their declared order; within a group rows in the order they joined, oldest first,
/// rows without a join time after them by identity. Updates never move a row.
pub(crate) fn display_order(
    left: (TickGroup, Option<u64>, &InstanceRef),
    right: (TickGroup, Option<u64>, &InstanceRef),
) -> std::cmp::Ordering {
    let joined = |time: Option<u64>| time.unwrap_or(u64::MAX);
    left.0
        .cmp(&right.0)
        .then_with(|| joined(left.1).cmp(&joined(right.1)))
        .then_with(|| left.2.cmp(right.2))
}

/// When a hosted Tick or a Shell task was created, from the UUID v7 of its identity, which is
/// when it joined the Monitor of the Chat that started it.
pub(crate) fn identity_time_ms(instance: &InstanceRef) -> Option<u64> {
    let uuid = instance
        .hosted_run()
        .map(|run| run.uuid())
        .or_else(|| instance.shell_task().map(|task| task.uuid()))?;
    (uuid.get_version_num() == 7).then(|| {
        let bytes = uuid.as_bytes();
        bytes[..6]
            .iter()
            .fold(0_u64, |time, byte| time << 8 | u64::from(*byte))
    })
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
                lead.joined_at_ms = match (lead.joined_at_ms, other.joined_at_ms) {
                    (Some(lead), Some(other)) => Some(lead.min(other)),
                    (lead, other) => lead.or(other),
                };
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

#[cfg(test)]
mod order_tests {
    use super::*;

    /// A hosted instance whose UUID v7 says it was created at `created_ms`.
    fn hosted_created_at(created_ms: u64) -> InstanceRef {
        let mut bytes = [0_u8; 16];
        bytes[..6].copy_from_slice(&created_ms.to_be_bytes()[2..]);
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = 1;
        format!("hosted:{}", Uuid::from_bytes(bytes))
            .parse()
            .unwrap()
    }

    fn row(instance: InstanceRef, group: TickGroup, updated_at_ms: u64) -> TickDescriptor {
        let mut row = member_descriptor(&MonitorMember {
            instance,
            title: None,
            is_terminal: false,
            min_rows: None,
            max_rows: None,
            joined_at_ms: None,
        });
        row.group = group;
        row.updated_at_ms = Some(updated_at_ms);
        row.update = Some(RowUpdate::new(updated_at_ms));
        row
    }

    struct FixedRows(Vec<TickDescriptor>);
    impl MonitorRows for FixedRows {
        fn access(&self) -> &'static str {
            "hosted"
        }
        fn list(&self, _: &MonitorContext) -> Vec<TickDescriptor> {
            self.0.clone()
        }
    }

    fn listed(rows: Vec<TickDescriptor>) -> Vec<InstanceRef> {
        MonitorRowsRegistry::new([Arc::new(FixedRows(rows)) as Arc<dyn MonitorRows>])
            .unwrap()
            .list(&MonitorContext::default())
            .into_iter()
            .map(|row| row.instance)
            .collect()
    }

    #[test]
    fn rows_keep_the_order_they_joined_in_whatever_updates_later() {
        let (oldest, middle, newest) = (
            hosted_created_at(1_000),
            hosted_created_at(2_000),
            hosted_created_at(3_000),
        );
        let mut linked_late = row(hosted_created_at(500), TickGroup::Artifacts, 1);
        linked_late.joined_at_ms = Some(9_000);
        let mut linked_early = row(hosted_created_at(600), TickGroup::Artifacts, 99_999);
        linked_early.joined_at_ms = Some(8_000);
        let untimed: InstanceRef = "hosted:not-a-uuid".parse().unwrap();
        let rows = |updates: [u64; 3]| {
            vec![
                row(newest.clone(), TickGroup::Ticks, updates[0]),
                row(untimed.clone(), TickGroup::Ticks, 50_000),
                row(oldest.clone(), TickGroup::Ticks, updates[1]),
                row(middle.clone(), TickGroup::Ticks, updates[2]),
                linked_late.clone(),
                linked_early.clone(),
            ]
        };
        let expected = vec![
            linked_early.instance.clone(),
            linked_late.instance.clone(),
            oldest.clone(),
            middle.clone(),
            newest.clone(),
            untimed.clone(),
        ];
        assert_eq!(listed(rows([10, 20, 30])), expected);
        assert_eq!(
            listed(rows([90_000, 10, 70_000])),
            expected,
            "an update never moves a row"
        );

        let all =
            MonitorRowsRegistry::new(
                [Arc::new(FixedRows(rows([5, 6, 7]))) as Arc<dyn MonitorRows>],
            )
            .unwrap()
            .list(&MonitorContext::default());
        let mut paged = Vec::new();
        let mut cursor = None;
        loop {
            let page = page_of(all.clone(), cursor.as_ref(), Some(2)).unwrap();
            paged.extend(page.items.into_iter().map(|row| row.instance));
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(paged, expected, "pages follow the display order");
    }

    #[test]
    fn a_reopened_tick_keeps_the_place_of_the_row_it_took_over() {
        let (oldest, middle) = (hosted_created_at(1_000), hosted_created_at(2_000));
        let reopened = hosted_created_at(9_000);
        let member = member_descriptor(&MonitorMember {
            instance: reopened.clone(),
            title: None,
            is_terminal: false,
            min_rows: None,
            max_rows: None,
            joined_at_ms: Some(1_500),
        });
        let subscribed = row(reopened.clone(), TickGroup::Ticks, 10);
        assert_eq!(
            listed(vec![
                row(middle.clone(), TickGroup::Ticks, 1),
                subscribed,
                member,
                row(oldest.clone(), TickGroup::Ticks, 2),
            ]),
            [oldest, reopened, middle],
            "a subscription's row of it moves it nowhere either"
        );
    }
}
