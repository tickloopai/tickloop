//! The Console's Monitor rows. Each registered Tick type describes its rows, member rows, row
//! intents and refresh from what the View already holds; the runtime's `TickAdapter` of the same
//! `<type>` supplies the stream.
mod builtin;
mod discovery;

#[cfg(test)]
pub(crate) use builtin::artifact_actions;
pub use builtin::{ArtifactRows, HerdrRows, HostedRows, ShellRows};
pub(crate) use builtin::{HOSTED_CLOSE_KEY, HOSTED_REOPEN_KEY};
pub use discovery::{
    DiscoveredTick, DiscoveryError, HerdrDiscovery, MemberFindings, TickDiscovery, TickProbe,
};

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use tick_protocol::InstanceRef;
use tickloop_agent_artifacts::ArtifactMetadata;
use tickloop_agent_protocol::{
    ChatSnapshot, LoopState, MonitorMember, MonitorMemberSource, ShellTaskSnapshot,
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
    /// Hides the row in this Chat only and keeps every other fact.
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
    OpenSubscription {
        id: Uuid,
    },
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
#[derive(Clone, Debug)]
pub struct TickDescriptor {
    pub instance: InstanceRef,
    pub group: TickGroup,
    pub glyph: TickGlyph,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub owner: TickOwner,
    /// The latest business update; resource and screen refreshes never set it.
    pub updated_at_ms: Option<u64>,
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
/// Adds a program to Monitor from discovery.
pub const MONITOR_ADD_KEY: char = 'a';
pub const MONITOR_REFRESH_KEY: char = 'r';
/// Keys the Monitor list itself owns. Row actions never use them, so the list's hints always
/// match what a key does.
pub const MONITOR_KEYS: [char; 2] = [MONITOR_ADD_KEY, MONITOR_REFRESH_KEY];

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
        owner: member_owner(member),
        updated_at_ms: None,
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

fn member_owner(member: &MonitorMember) -> TickOwner {
    match member.source {
        MonitorMemberSource::Opened => TickOwner::ThisChat,
        MonitorMemberSource::Added => TickOwner::Unknown,
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
    /// The Chat's complete deduplicated Monitor set: each type's rows plus opened and added
    /// members, minus the instances the user removed. Listing never subscribes or calls the model.
    pub fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        let mut merged = merge_descriptors(
            self.rows
                .values()
                .flat_map(|type_rows| type_rows.list(context)),
        );
        if let Some(chat) = &context.chat {
            for member in &chat.monitor.members {
                match merged.get_mut(&member.instance) {
                    Some(entry) => {
                        if entry.title == entry.instance.as_str()
                            && let Some(title) = &member.title
                        {
                            entry.title = title.clone();
                        }
                        if member.source == MonitorMemberSource::Opened {
                            entry.owner = TickOwner::ThisChat;
                        }
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
            merged.retain(|instance, _| !chat.monitor.is_removed(instance));
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
            let frequencies = context
                .subscriptions
                .iter()
                .filter(|row| {
                    row.instance == entry.instance
                        && !matches!(row.loop_state, LoopState::Stopped | LoopState::Expired)
                })
                .map(|row| format!("{}s", row.every_seconds))
                .collect::<Vec<_>>();
            if !frequencies.is_empty() {
                if !entry.status.is_empty() {
                    entry.status.push_str(" · ");
                }
                entry.status.push_str(&frequencies.join(" / "));
            }
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
                lead.running |= other.running;
                if lead.title == lead.instance.as_str() {
                    lead.title = other.title;
                }
                if lead.status.is_empty() {
                    lead.status = other.status;
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

pub fn artifact_target(instance: &InstanceRef) -> Option<(String, String)> {
    instance
        .artifact_parts()
        .map(|(origin, id)| (origin.to_owned(), id.to_owned()))
}
