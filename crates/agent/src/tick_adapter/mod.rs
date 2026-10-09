//! Registered Tick adapters own type knowledge; Monitor and Loops consume their common contract.
mod builtin;
mod discovery;
mod shell;

#[cfg(test)]
pub(crate) use builtin::artifact_actions;
pub use builtin::{ArtifactAdapter, HerdrAdapter, HostedAdapter, ShellAdapter, shell_status};
pub(crate) use builtin::{HOSTED_CLOSE_KEY, HOSTED_REOPEN_KEY};
pub use discovery::{
    DiscoveredTick, DiscoveryError, HerdrDiscovery, MemberFindings, TickDiscovery, TickProbe,
};
pub use shell::ShellTickStream;

use crate::{
    artifacts::ArtifactMetadata,
    runtime::{
        ChatSnapshot, MonitorMember, MonitorMemberSource, ShellTaskSnapshot, TickSubscriptionStatus,
    },
    tick_stream::{EventPublisher, InstanceRef, TickStreamHandle, TickStreamPort, WatchId},
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use tokio::sync::watch;
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

/// What a Monitor row action means, as plain data. Adapters only describe it; the Console carries
/// it out with its own state, such as the selected Chat, the row and any panel it opens.
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

pub trait TickAdapter: Send + Sync + 'static {
    fn tick_type(&self) -> &'static str;
    /// Rows this adapter knows from its own sources (subscriptions, services, tasks).
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor>;
    /// The row for a Chat member the adapter did not list itself. Adapters whose host can be
    /// probed add the probe's state; the default shows only what the Chat recorded.
    fn member(&self, member: &MonitorMember, _context: &MonitorContext) -> TickDescriptor {
        member_descriptor(member)
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>>;
    /// What the Monitor refresh key also reloads for this adapter's rows.
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

#[derive(Clone, Default)]
pub struct TickAdapters {
    adapters: BTreeMap<&'static str, Arc<dyn TickAdapter>>,
}
impl TickAdapters {
    pub fn new(adapters: impl IntoIterator<Item = Arc<dyn TickAdapter>>) -> Result<Self, String> {
        let mut registered = BTreeMap::new();
        for adapter in adapters {
            InstanceRef::validate_type(adapter.tick_type())?;
            if registered.insert(adapter.tick_type(), adapter).is_some() {
                return Err("duplicate Tick Adapter type".into());
            }
        }
        Ok(Self {
            adapters: registered,
        })
    }
    pub fn builtin(
        agent: Option<crate::runtime::Agent>,
        root: Option<std::path::PathBuf>,
        access: Option<Arc<tick_connect::access::TickAccess>>,
        hosted: Option<Arc<crate::hosted::tools::HostedTickTools>>,
    ) -> Self {
        Self::new([
            Arc::new(ArtifactAdapter::new(root)) as Arc<dyn TickAdapter>,
            Arc::new(ShellAdapter::new(agent)),
            Arc::new(HerdrAdapter::new(access.map(|access| {
                Arc::new(crate::native_host::NativeHostToolProvider::new(access))
            }))),
            Arc::new(HostedAdapter::new(hosted)),
        ])
        .expect("unique built-in Tick Adapters")
    }
    pub fn presentation() -> Self {
        Self::builtin(None, None, None, None)
    }
    /// The Chat's complete deduplicated Monitor set: adapter rows plus opened and added members,
    /// minus the instances the user removed. Listing never subscribes or calls the model.
    pub fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        let mut merged = merge_descriptors(
            self.adapters
                .values()
                .flat_map(|adapter| adapter.list(context)),
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
                        let row = match self.adapters.get(member.instance.tick_type()) {
                            Some(adapter) => adapter.member(member, context),
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
                    "Tick Adapter row action {:?} uses a key the Monitor list owns",
                    action.key
                );
                !reserved
            });
            let frequencies = context
                .subscriptions
                .iter()
                .filter(|row| {
                    row.instance == entry.instance
                        && !matches!(
                            row.loop_state,
                            crate::runtime::LoopState::Stopped | crate::runtime::LoopState::Expired
                        )
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
        cursor: Option<&tick_connect::page::PageCursor>,
        limit: Option<usize>,
    ) -> Result<tick_connect::page::Page<TickDescriptor>, tick_connect::page::PageError> {
        page_of(self.list(context), cursor, limit)
    }
    pub fn refresh_intents(&self) -> Vec<TickIntent> {
        self.adapters
            .values()
            .filter_map(|adapter| adapter.refresh_intent())
            .collect()
    }
}

/// One page of an already listed Monitor set; see [`TickAdapters::page`].
pub fn page_of(
    entries: Vec<TickDescriptor>,
    cursor: Option<&tick_connect::page::PageCursor>,
    limit: Option<usize>,
) -> Result<tick_connect::page::Page<TickDescriptor>, tick_connect::page::PageError> {
    let mut page = tick_connect::page::paginate(
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

/// Groups in adapter order; within a group the most recently updated first.
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
#[async_trait::async_trait]
impl TickStreamPort for TickAdapters {
    async fn prepare_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        input: &serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<crate::tick_stream::PreparedTickWatch>, String> {
        let stream = self
            .adapters
            .get(instance.tick_type())
            .and_then(|adapter| adapter.stream())
            .ok_or_else(|| format!("Tick Adapter {} is unavailable", instance.tick_type()))?;
        stream.prepare_watch(chat, instance, input, cancel).await
    }
    fn set_watch_authorities(
        &self,
        authorities: BTreeMap<WatchId, crate::host_tools::HostToolAuthority>,
    ) {
        for stream in self
            .adapters
            .values()
            .filter_map(|adapter| adapter.stream())
        {
            stream.set_watch_authorities(authorities.clone());
        }
    }
    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&crate::host_tools::HostToolAuthority>,
    ) -> Result<(), String> {
        let stream = self
            .adapters
            .get(instance.tick_type())
            .and_then(|adapter| adapter.stream())
            .ok_or("Tick stream is unavailable")?;
        stream.revalidate_watch(chat, instance, authority).await
    }
    async fn read_current(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<Option<crate::tick_stream::TickObservation>, String> {
        match self
            .adapters
            .get(instance.tick_type())
            .and_then(|adapter| adapter.stream())
        {
            Some(stream) => stream.read_current(chat, instance).await,
            None => Ok(None),
        }
    }
    fn set_watch_chats(&self, chats: BTreeMap<WatchId, Uuid>) {
        for stream in self
            .adapters
            .values()
            .filter_map(|adapter| adapter.stream())
        {
            stream.set_watch_chats(chats.clone());
        }
    }
    fn set_cadences(&self, cadences: BTreeMap<WatchId, u64>) {
        for stream in self
            .adapters
            .values()
            .filter_map(|adapter| adapter.stream())
        {
            stream.set_cadences(cadences.clone());
        }
    }
    fn open(&self) -> TickStreamHandle {
        let (desired, receiver) = watch::channel(BTreeMap::<WatchId, InstanceRef>::new());
        let (publisher, events) = EventPublisher::channel();
        for adapter in self.adapters.values() {
            let Some(stream) = adapter.stream() else {
                continue;
            };
            let stream = stream.open();
            let tick_type = adapter.tick_type();
            let mut receiver = receiver.clone();
            let publisher = publisher.clone();
            tokio::spawn(async move {
                let mut events = stream.events;
                loop {
                    let targets = receiver
                        .borrow_and_update()
                        .iter()
                        .filter(|(_, instance)| instance.tick_type() == tick_type)
                        .map(|(watch, instance)| (*watch, instance.clone()))
                        .collect();
                    stream.desired.send_if_modified(|current| {
                        if *current == targets {
                            false
                        } else {
                            *current = targets;
                            true
                        }
                    });
                    tokio::select! {
                        changed = receiver.changed() => if changed.is_err() { return },
                        event = events.recv() => match event { Some(event) => publisher.publish(event), None => return }
                    }
                }
            });
        }
        let types = self
            .adapters
            .iter()
            .filter(|(_, adapter)| adapter.stream().is_some())
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            let mut receiver = receiver;
            loop {
                let targets = receiver.borrow_and_update().clone();
                publisher.retain(&targets);
                for (watch, instance) in targets
                    .iter()
                    .filter(|(_, instance)| !types.contains(&instance.tick_type()))
                {
                    publisher.state_to(
                        instance.clone(),
                        crate::tick_stream::TickConnectionState::Gone,
                        [*watch],
                    );
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        });
        TickStreamHandle { desired, events }
    }
}

pub fn artifact_target(instance: &InstanceRef) -> Option<(String, String)> {
    instance
        .artifact_parts()
        .map(|(origin, id)| (origin.to_owned(), id.to_owned()))
}
