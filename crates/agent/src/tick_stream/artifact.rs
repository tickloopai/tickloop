use super::{
    EventPublisher, InstanceRef, ObservationKind, TickConnectionState, TickObservation,
    TickStreamEvent, TickStreamHandle, TickStreamPort, WatchId,
};
use crate::artifacts::{ArtifactClient, ArtifactError, ArtifactMetadata};
use crate::runtime::{DEFAULT_CADENCE_SECONDS, MAX_CADENCE_SECONDS, MIN_CADENCE_SECONDS};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::watch, time::Instant};

pub struct ArtifactTickStream {
    root: PathBuf,
    native: Option<Arc<dyn TickStreamPort>>,
    cadences: watch::Sender<BTreeMap<WatchId, u64>>,
}
impl ArtifactTickStream {
    pub fn new(root: PathBuf, native: Option<Arc<dyn TickStreamPort>>) -> Self {
        Self {
            root,
            native,
            cadences: watch::channel(BTreeMap::new()).0,
        }
    }
}
impl TickStreamPort for ArtifactTickStream {
    fn set_cadences(&self, cadences: BTreeMap<WatchId, u64>) {
        if let Some(native) = &self.native {
            native.set_cadences(cadences.clone());
        }
        self.cadences.send_replace(cadences);
    }
    fn open(&self) -> TickStreamHandle {
        let (desired, receiver) = watch::channel(BTreeMap::<WatchId, InstanceRef>::new());
        let (publisher, events) = EventPublisher::channel();
        if let Some(native) = &self.native {
            let native = native.open();
            let mut desired_rx = receiver.clone();
            let publisher = publisher.clone();
            tokio::spawn(async move {
                let mut native_events = native.events;
                loop {
                    let targets = desired_rx
                        .borrow_and_update()
                        .iter()
                        .filter(|(_, instance)| !instance.as_str().starts_with("artifact:"))
                        .map(|(watch, instance)| (*watch, instance.clone()))
                        .collect();
                    native.desired.send_replace(targets);
                    tokio::select! {
                        result = desired_rx.changed() => if result.is_err() { return; },
                        event = native_events.recv() => match event {
                            Some(TickStreamEvent::Metadata { instance, audience, metadata }) => publisher.metadata(instance, audience, metadata),
                            Some(TickStreamEvent::Ready { watch }) => publisher.ready(watch),
                            Some(TickStreamEvent::Observation { audience, value }) => publisher.observation(&audience, value),
                            Some(TickStreamEvent::State { instance, state, audience }) => publisher.state_to(instance, state, audience),
                            Some(TickStreamEvent::Gap { instance, audience }) => publisher.gap_to(instance, audience),
                            None => return,
                        }
                    }
                }
            });
        }
        tokio::spawn(observe(
            self.root.clone(),
            receiver,
            self.cadences.subscribe(),
            publisher,
        ));
        TickStreamHandle { desired, events }
    }
}
struct Cursor {
    instance: InstanceRef,
    sequence: Option<u64>,
    ready: bool,
    due: Instant,
    cadence: u64,
}
async fn observe(
    root: PathBuf,
    mut desired: watch::Receiver<BTreeMap<WatchId, InstanceRef>>,
    mut cadences: watch::Receiver<BTreeMap<WatchId, u64>>,
    publisher: EventPublisher,
) {
    let mut cursors: BTreeMap<WatchId, Cursor> = BTreeMap::new();
    loop {
        let targets = desired.borrow_and_update().clone();
        let frequencies = cadences.borrow_and_update().clone();
        publisher.retain(&targets);
        cursors.retain(|watch, cursor| targets.get(watch) == Some(&cursor.instance));
        for (watch, instance) in targets
            .iter()
            .filter(|(_, instance)| instance.as_str().starts_with("artifact:"))
        {
            let cadence = frequencies
                .get(watch)
                .copied()
                .unwrap_or(DEFAULT_CADENCE_SECONDS)
                .clamp(MIN_CADENCE_SECONDS, MAX_CADENCE_SECONDS);
            let cursor = cursors.entry(*watch).or_insert_with(|| Cursor {
                instance: instance.clone(),
                sequence: None,
                ready: false,
                due: Instant::now(),
                cadence,
            });
            if cursor.cadence != cadence {
                cursor.cadence = cadence;
                cursor.due = Instant::now() + Duration::from_secs(cadence);
            }
        }
        let now = Instant::now();
        let mut due: BTreeMap<InstanceRef, Vec<WatchId>> = BTreeMap::new();
        for (watch, cursor) in &cursors {
            if cursor.due <= now {
                due.entry(cursor.instance.clone()).or_default().push(*watch);
            }
        }
        for (instance, audience) in due {
            let result = read(&root, &instance).await;
            let still_desired = desired.borrow().clone();
            for watch in audience {
                let Some(cursor) = cursors
                    .get_mut(&watch)
                    .filter(|cursor| still_desired.get(&watch) == Some(&cursor.instance))
                else {
                    continue;
                };
                cursor.due = Instant::now() + Duration::from_secs(cursor.cadence);
                match &result {
                    Ok(metadata) => {
                        publisher.metadata(
                            instance.clone(),
                            [watch],
                            super::TickMetadata {
                                title: metadata.title.clone(),
                                kind: ObservationKind::Protocol,
                                updated_at_ms: metadata
                                    .human_updated_at
                                    .as_deref()
                                    .and_then(crate::transcript::parse_timestamp_ms)
                                    .map(|value| value.max(0) as u64),
                            },
                        );
                        publisher.state_to(instance.clone(), TickConnectionState::Live, [watch]);
                        if !cursor.ready {
                            cursor.sequence = metadata.human_sequence;
                            cursor.ready = true;
                            publisher.ready(watch);
                            continue;
                        }
                        if metadata
                            .human_sequence
                            .is_some_and(|sequence| sequence > cursor.sequence.unwrap_or(0))
                        {
                            cursor.sequence = metadata.human_sequence;
                            if metadata.human_change.as_deref() == Some("feedback") {
                                continue;
                            }
                            if let Some(value) = observation(&instance, metadata) {
                                publisher.observation(&[watch], value);
                            }
                        }
                    }
                    Err(ArtifactError::Service { status: 404, .. }) => {
                        publisher.state_to(instance.clone(), TickConnectionState::Gone, [watch])
                    }
                    Err(_) => publisher.state_to(
                        instance.clone(),
                        TickConnectionState::Disconnected,
                        [watch],
                    ),
                }
            }
        }
        let next = cursors.values().map(|cursor| cursor.due).min();
        tokio::select! {
            result = desired.changed() => if result.is_err() { return; },
            result = cadences.changed() => if result.is_err() { return; },
            _ = async { if let Some(next) = next { tokio::time::sleep_until(next).await; } else { std::future::pending::<()>().await; } } => {}
        }
    }
}
async fn read(
    root: &std::path::Path,
    instance: &InstanceRef,
) -> Result<ArtifactMetadata, ArtifactError> {
    let identity = instance
        .as_str()
        .strip_prefix("artifact:")
        .ok_or(ArtifactError::InvalidResponse)?;
    let (origin, id) = identity
        .rsplit_once('/')
        .ok_or(ArtifactError::InvalidResponse)?;
    ArtifactClient::for_origin(root, origin)?.read(id).await
}
fn observation(instance: &InstanceRef, metadata: &ArtifactMetadata) -> Option<TickObservation> {
    let sequence = metadata.human_sequence?;
    let updated_at = metadata.human_updated_at.clone()?;
    let change = metadata.human_change.as_deref()?;
    if !["edit", "publish", "feedback"].contains(&change) {
        return None;
    }
    let mut data = serde_json::json!({"artifact_id":metadata.id,"title":metadata.title,"change":change,"human_updated_at":updated_at,"human_sequence":sequence});
    if let Some(version) = &metadata.human_version_id {
        data["version_id"] = serde_json::json!(version);
    }
    Some(TickObservation {
        instance: instance.clone(),
        sequence,
        observed_at: updated_at,
        dropped: 0,
        data,
        kind: ObservationKind::Protocol,
        recover_with: Some("Read the artifact with its MCP tools".into()),
        gap: false,
    })
}
