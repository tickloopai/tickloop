use super::{
    EventPublisher, InstanceRef, ObservationKind, TickConnectionState, TickObservation,
    TickStreamHandle, TickStreamPort, WatchId,
};
use crate::artifacts::{ArtifactClient, ArtifactError, ArtifactMetadata};
use crate::runtime::{DEFAULT_CADENCE_SECONDS, MAX_CADENCE_SECONDS, MIN_CADENCE_SECONDS};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::{sync::watch, time::Instant};

pub struct ArtifactTickStream {
    root: PathBuf,
    cadences: watch::Sender<BTreeMap<WatchId, u64>>,
}
impl ArtifactTickStream {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cadences: watch::channel(BTreeMap::new()).0,
        }
    }
}
impl TickStreamPort for ArtifactTickStream {
    fn set_cadences(&self, cadences: BTreeMap<WatchId, u64>) {
        self.cadences.send_replace(cadences);
    }
    fn open(&self) -> TickStreamHandle {
        let (desired, receiver) = watch::channel(BTreeMap::<WatchId, InstanceRef>::new());
        let (publisher, events) = EventPublisher::channel();
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
    body_sequence: Option<u64>,
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
            .filter(|(_, instance)| instance.tick_type() == "artifact")
        {
            let cadence = frequencies
                .get(watch)
                .copied()
                .unwrap_or(DEFAULT_CADENCE_SECONDS)
                .clamp(MIN_CADENCE_SECONDS, MAX_CADENCE_SECONDS);
            let cursor = cursors.entry(*watch).or_insert_with(|| Cursor {
                instance: instance.clone(),
                body_sequence: None,
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
                            cursor.body_sequence = body_update(metadata)
                                .map(|update| update.sequence)
                                .or(metadata.human_sequence);
                            cursor.ready = true;
                            publisher.ready(watch);
                            continue;
                        }
                        if let Some(value) = observation(&instance, metadata)
                            && value.sequence > cursor.body_sequence.unwrap_or(0)
                        {
                            cursor.body_sequence = Some(value.sequence);
                            publisher.observation(&[watch], value);
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
    let (origin, id) = instance
        .artifact_parts()
        .ok_or(ArtifactError::InvalidResponse)?;
    ArtifactClient::for_origin(root, origin)?.read(id).await
}
fn observation(instance: &InstanceRef, metadata: &ArtifactMetadata) -> Option<TickObservation> {
    let update = body_update(metadata)?;
    let sequence = update.sequence;
    let updated_at = update.updated_at.to_owned();
    let change = update.change;
    let mut data = serde_json::json!({"artifact_id":metadata.id,"title":metadata.title,"change":change,"human_updated_at":updated_at,"human_sequence":sequence});
    if let Some(version) = update.version_id {
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

struct BodyUpdate<'a> {
    sequence: u64,
    updated_at: &'a str,
    change: &'a str,
    version_id: Option<&'a str>,
}

fn body_update(metadata: &ArtifactMetadata) -> Option<BodyUpdate<'_>> {
    let update = if let Some(sequence) = metadata.human_body_sequence {
        BodyUpdate {
            sequence,
            updated_at: metadata.human_body_updated_at.as_deref()?,
            change: metadata.human_body_change.as_deref()?,
            version_id: metadata.human_body_version_id.as_deref(),
        }
    } else {
        BodyUpdate {
            sequence: metadata.human_sequence?,
            updated_at: metadata.human_updated_at.as_deref()?,
            change: metadata.human_change.as_deref()?,
            version_id: metadata.human_version_id.as_deref(),
        }
    };
    if !["edit", "publish"].contains(&update.change) {
        return None;
    }
    Some(update)
}
