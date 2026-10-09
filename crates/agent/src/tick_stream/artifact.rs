use super::{
    EventPublisher, InstanceRef, ObservationKind, TickConnectionState, TickObservation,
    TickStreamHandle, TickStreamPort, WatchId,
};
use crate::host_tools::HostToolAuthority;
use crate::runtime::{DEFAULT_CADENCE_SECONDS, MAX_CADENCE_SECONDS, MIN_CADENCE_SECONDS};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tickloop_agent_artifacts::{ArtifactClient, ArtifactError, ArtifactMetadata};
use tokio::{sync::watch, time::Instant};
use uuid::Uuid;

pub struct ArtifactTickStream {
    root: PathBuf,
    cadences: watch::Sender<BTreeMap<WatchId, u64>>,
    authorities: watch::Sender<BTreeMap<WatchId, HostToolAuthority>>,
}
impl ArtifactTickStream {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cadences: watch::channel(BTreeMap::new()).0,
            authorities: watch::channel(BTreeMap::new()).0,
        }
    }
}
#[async_trait::async_trait]
impl TickStreamPort for ArtifactTickStream {
    async fn prepare_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        _input: &serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<super::PreparedTickWatch>, String> {
        use crate::host_tools::HostToolProvider;
        let (origin, id) = instance.artifact_parts().ok_or("Invalid Artifact subscription identity; use artifact:<origin>/<artifact_id> from artifact_ticks")?;
        let provider: std::sync::Arc<dyn HostToolProvider> =
            std::sync::Arc::new(crate::mcp_tools::McpHostToolProvider::new(
                ArtifactClient::for_origin(&self.root, origin)
                    .map_err(|error| error.to_string())?,
                None,
            ));
        provider.refresh(cancel.clone()).await?;
        let query = provider
            .prepare_call(
                chat,
                "mcp__artifacts__artifact_read",
                &serde_json::json!({"artifact_id":id}),
                cancel,
            )
            .await?;
        Ok(Some(super::PreparedTickWatch::new(provider, query)?))
    }

    fn set_watch_authorities(&self, authorities: BTreeMap<WatchId, HostToolAuthority>) {
        self.authorities.send_replace(authorities);
    }
    async fn revalidate_watch(
        &self,
        _chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        authorized_client(&self.root, instance, authority).map(|_| ())
    }
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
            self.authorities.subscribe(),
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
    mut authorities: watch::Receiver<BTreeMap<WatchId, HostToolAuthority>>,
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
        let mut due: BTreeMap<(InstanceRef, Option<HostToolAuthority>), Vec<WatchId>> =
            BTreeMap::new();
        for (watch, cursor) in &cursors {
            if cursor.due <= now {
                due.entry((
                    cursor.instance.clone(),
                    authorities.borrow().get(watch).cloned(),
                ))
                .or_default()
                .push(*watch);
            }
        }
        for ((instance, authority), audience) in due {
            let result = match authorized_client(&root, &instance, authority.as_ref()) {
                Ok((client, credential)) => {
                    client
                        .read_with_credential(instance.artifact_parts().unwrap().1, &credential)
                        .await
                }
                Err(_) => Err(ArtifactError::AccountChanged),
            };
            for watch in audience {
                let still_desired = desired.borrow().clone();
                let Some(cursor) = cursors
                    .get_mut(&watch)
                    .filter(|cursor| still_desired.get(&watch) == Some(&cursor.instance))
                else {
                    continue;
                };
                cursor.due = Instant::now() + Duration::from_secs(cursor.cadence);
                if authorities.borrow().get(&watch) != authority.as_ref()
                    || authorized_client(&root, &instance, authority.as_ref()).is_err()
                {
                    publisher.state_to(
                        instance.clone(),
                        TickConnectionState::IdentityUncertain,
                        [watch],
                    );
                    continue;
                }
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
            result = authorities.changed() => if result.is_err() { return; },
            _ = async { if let Some(next) = next { tokio::time::sleep_until(next).await; } else { std::future::pending::<()>().await; } } => {}
        }
    }
}
fn authorized_client(
    root: &std::path::Path,
    instance: &InstanceRef,
    authority: Option<&HostToolAuthority>,
) -> Result<(ArtifactClient, tickloop_agent_artifacts::CredentialSnapshot), String> {
    let (origin, _) = instance
        .artifact_parts()
        .ok_or("Artifact watch identity is invalid")?;
    let Some(HostToolAuthority::Mcp {
        server_id,
        account_id,
    }) = authority
    else {
        return Err(
            "Artifact watch has no admitted account authority; subscribe explicitly".into(),
        );
    };
    if server_id != origin {
        return Err("Artifact watch endpoint is outside its admitted authority".into());
    }
    let client = ArtifactClient::for_origin(root, origin).map_err(|error| error.to_string())?;
    let credential = client
        .credential_snapshot()
        .map_err(|error| error.to_string())?
        .ok_or("Artifact watch account is unavailable")?;
    if credential.origin != *server_id || credential.account_id != *account_id {
        return Err("Artifact watch account changed outside its admitted authority".into());
    }
    Ok((client, credential))
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
