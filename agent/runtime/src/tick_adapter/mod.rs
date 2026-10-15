//! Registered Tick adapters supply the TickStreamPort of each access (the instance prefix, such as
//! `shell:`); the Loop runtime consumes their common contract. The Console registers the same
//! access for its Monitor rows.
mod builtin;

pub use crate::tick_stream::ShellTickStream;
pub use builtin::{HostedAdapter, ShellAdapter};

use crate::tick_stream::{EventPublisher, TickStreamHandle, TickStreamPort, WatchId};
use std::{collections::BTreeMap, sync::Arc};
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::HostToolAuthority;
use tokio::sync::watch;
use uuid::Uuid;

pub trait TickAdapter: Send + Sync + 'static {
    fn access(&self) -> &'static str;
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>>;
}

#[derive(Clone)]
pub struct TickAdapters {
    adapters: BTreeMap<&'static str, Arc<dyn TickAdapter>>,
}
impl TickAdapters {
    pub fn new(adapters: impl IntoIterator<Item = Arc<dyn TickAdapter>>) -> Result<Self, String> {
        let mut registered = BTreeMap::new();
        for adapter in adapters {
            InstanceRef::validate_access(adapter.access())?;
            if registered.insert(adapter.access(), adapter).is_some() {
                return Err("duplicate Tick Adapter access".into());
            }
        }
        Ok(Self {
            adapters: registered,
        })
    }
    pub fn builtin(
        agent: crate::Agent,
        hosted: Option<Arc<crate::hosted::tools::HostedTickTools>>,
    ) -> Self {
        Self::new([
            Arc::new(ShellAdapter::new(agent)) as Arc<dyn TickAdapter>,
            Arc::new(HostedAdapter::new(hosted)),
        ])
        .expect("unique built-in Tick Adapters")
    }
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
            .get(instance.access())
            .and_then(|adapter| adapter.stream())
            .ok_or_else(|| format!("Tick Adapter {} is unavailable", instance.access()))?;
        stream.prepare_watch(chat, instance, input, cancel).await
    }
    fn set_watch_authorities(&self, authorities: BTreeMap<WatchId, HostToolAuthority>) {
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
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        let stream = self
            .adapters
            .get(instance.access())
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
            .get(instance.access())
            .and_then(|adapter| adapter.stream())
        {
            Some(stream) => stream.read_current(chat, instance).await,
            None => Ok(None),
        }
    }
    async fn sample(
        &self,
        watch: WatchId,
        instance: &InstanceRef,
    ) -> Option<crate::tick_stream::TickObservation> {
        self.adapters
            .get(instance.access())
            .and_then(|adapter| adapter.stream())?
            .sample(watch, instance)
            .await
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
    fn open(&self) -> TickStreamHandle {
        let (desired, receiver) = watch::channel(BTreeMap::<WatchId, InstanceRef>::new());
        let (publisher, events) = EventPublisher::channel();
        for adapter in self.adapters.values() {
            let Some(stream) = adapter.stream() else {
                continue;
            };
            let stream = stream.open();
            let access = adapter.access();
            let mut receiver = receiver.clone();
            let publisher = publisher.clone();
            tokio::spawn(async move {
                let mut events = stream.events;
                loop {
                    let targets = receiver
                        .borrow_and_update()
                        .iter()
                        .filter(|(_, instance)| instance.access() == access)
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
        let accesses = self
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
                    .filter(|(_, instance)| !accesses.contains(&instance.access()))
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
