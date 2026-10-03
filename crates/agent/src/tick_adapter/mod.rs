//! Registered Tick adapters own type knowledge; lists and Loops consume their common contract.
mod builtin;
mod shell;
mod tools;

pub use builtin::{ArtifactAdapter, HerdrAdapter, ShellAdapter, shell_status};
pub use shell::ShellTickStream;

use crate::{
    artifacts::ArtifactMetadata,
    host_tools::HostToolProvider,
    runtime::{ChatSnapshot, ShellTaskSnapshot, TickSubscriptionStatus},
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

pub use crate::terminal::tick_actions::{
    PreparedTickAction, TickActionBackend, TickActionContext, TickActionExecutor,
    TickActionOutcome, TickActionUi, TickConfirmationView, TickDetailKey, TickDetailLine,
    TickDetailView,
};
#[derive(Clone)]
pub enum TickListOperation {
    Execute(Arc<dyn TickActionExecutor>),
    RemoveLocal,
    Notify(String),
}
impl std::fmt::Debug for TickListOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Execute(_) => f.write_str("AdapterAction"),
            Self::RemoveLocal => f.write_str("RemoveLocal"),
            Self::Notify(message) => f.debug_tuple("Notify").field(message).finish(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct TickListAction {
    pub key: char,
    pub label: &'static str,
    pub operation: TickListOperation,
}
#[derive(Clone, Debug)]
pub struct TickDescriptor {
    pub instance: InstanceRef,
    pub group: TickGroup,
    pub glyph: TickGlyph,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub updated_at_ms: Option<u64>,
    pub running: bool,
    pub actions: Vec<TickListAction>,
}
#[derive(Default)]
pub struct TickListContext {
    pub chat: Option<ChatSnapshot>,
    pub shells: Option<ShellTaskSnapshot>,
    pub subscriptions: Vec<TickSubscriptionStatus>,
    pub metadata: HashMap<String, Option<ArtifactMetadata>>,
    pub metadata_failures: HashMap<String, String>,
}

pub trait TickAdapter: Send + Sync + 'static {
    fn tick_type(&self) -> &'static str;
    fn list(&self, context: &TickListContext) -> Vec<TickDescriptor>;
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>>;
    /// ReadTick supplies the instance's catalog; CallTickTool prepares its declared query/action.
    fn tools(&self) -> Option<Arc<dyn HostToolProvider>>;
    fn refresh_action(&self) -> Option<Arc<dyn TickActionExecutor>> {
        None
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
    ) -> Self {
        Self::new([
            Arc::new(ArtifactAdapter::new(root)) as Arc<dyn TickAdapter>,
            Arc::new(ShellAdapter::new(agent)),
            Arc::new(HerdrAdapter::new(access)),
        ])
        .expect("unique built-in Tick Adapters")
    }
    pub fn presentation() -> Self {
        Self::builtin(None, None, None)
    }
    pub fn list(&self, context: &TickListContext) -> Vec<TickDescriptor> {
        let mut entries = merge_descriptors(
            self.adapters
                .values()
                .flat_map(|adapter| adapter.list(context)),
        )
        .into_values()
        .collect::<Vec<_>>();
        for entry in &mut entries {
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
        entries.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| a.instance.cmp(&b.instance))
        });
        entries
    }
    fn provider(&self, instance: &InstanceRef) -> Result<Arc<dyn HostToolProvider>, String> {
        self.adapters
            .get(instance.tick_type())
            .and_then(|adapter| adapter.tools())
            .ok_or_else(|| format!("Tick Adapter {} is unavailable", instance.tick_type()))
    }
    pub(crate) fn refresh_actions(&self) -> Vec<Arc<dyn TickActionExecutor>> {
        self.adapters
            .values()
            .filter_map(|adapter| adapter.refresh_action())
            .collect()
    }
    pub fn host_tools(
        &self,
        native: Option<Arc<dyn HostToolProvider>>,
    ) -> Arc<dyn HostToolProvider> {
        Arc::new(tools::AdapterTools::new(self.clone(), native))
    }
}

fn merge_descriptors(
    entries: impl IntoIterator<Item = TickDescriptor>,
) -> BTreeMap<InstanceRef, TickDescriptor> {
    let mut merged: BTreeMap<InstanceRef, TickDescriptor> = BTreeMap::new();
    for entry in entries {
        let rank = |entry: &TickDescriptor| {
            (
                entry.updated_at_ms,
                entry.running,
                entry.title != entry.instance.as_str(),
            )
        };
        match merged.entry(entry.instance.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if rank(&entry) > rank(slot.get()) {
                    slot.insert(entry);
                }
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
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<crate::tick_stream::PreparedTickWatch>, String> {
        let provider = self.provider(instance)?;
        let call = provider
            .prepare_call(
                chat,
                "ReadTick",
                &serde_json::json!({"tick_instance_id":instance}),
                cancel,
            )
            .await?;
        Ok(Some(crate::tick_stream::PreparedTickWatch::new(
            provider, call,
        )))
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
