//! The authenticated worker-to-owner channel for hosted Tick lifecycles. A worker proves its
//! private grant and names the Chat it acts for; the owner verifies that the Chat runs in that
//! worker, derives the config root from the worker's launch itself, and dispatches through its
//! own host controller. Workers never hold host control or reconcile rights.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tick_connect::{
    host_error::HostError,
    hosted::{
        BUSINESS_FRAME_BYTES, CONTROL_TIMEOUT, peer_uid, read_frame, same_secret, write_frame,
    },
    instance::InstanceRef,
    page::PageCursor,
};
use tick_host::host::{OpenRequest, TickOwner};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::Mutex,
};
use uuid::Uuid;

use super::{
    controller::{HostController, LIFECYCLE_TIMEOUT},
    tools::MODIFY_TICK,
};
use crate::{
    agent_access::{
        AgentAccess, AgentDetailQuery, ExecutionRef, ObservationRequest, ObservationValue,
        OwnerServer,
    },
    host_tools::{HostEndpointIdentity, HostToolAuthority},
};

pub const ENDPOINT_NAME: &str = "worker-control.sock";
/// A worker request is small: an open carries argv, cwd and the launch environment.
const REQUEST_FRAME_BYTES: usize = tick_connect::protocol::MAX_REQUEST_BYTES;
/// How long a worker waits for the owner: the longest lifecycle request plus transport.
const OWNER_ANSWER_TIMEOUT: Duration =
    Duration::from_secs(LIFECYCLE_TIMEOUT.as_secs() + CONTROL_TIMEOUT.as_secs());
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// What a worker receives privately at startup. The capability never reaches logs or models.
#[derive(Clone, Deserialize, Serialize)]
pub struct WorkerControlGrant {
    pub endpoint: PathBuf,
    pub owner_epoch: Uuid,
    pub binding: Uuid,
    pub capability: String,
    /// The Tick host endpoint this owner controls: the authority of the worker's hosted tools.
    pub host_endpoint: PathBuf,
}

impl std::fmt::Debug for WorkerControlGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerControlGrant")
            .field("endpoint", &self.endpoint)
            .field("owner_epoch", &self.owner_epoch)
            .field("binding", &self.binding)
            .field("host_endpoint", &self.host_endpoint)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerRequest {
    OpenHostedTick {
        chat: Uuid,
        operation: Uuid,
        command: Vec<String>,
        cwd: PathBuf,
        environment: Vec<(String, String)>,
        title: Option<String>,
        admission: HostedAdmission,
    },
    CloseHostedTick {
        chat: Uuid,
        operation: Uuid,
        instance: InstanceRef,
        admission: HostedAdmission,
    },
    ListHostedTicks {
        chat: Uuid,
        cursor: Option<PageCursor>,
    },
    GetHostedOperationReceipt {
        chat: Uuid,
        operation: Uuid,
    },
}

/// What a lifecycle request was admitted under in the worker: the tool it prepared and the
/// host authority it froze. The owner checks both against its own host and the Chat's ceiling.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostedAdmission {
    pub tool: String,
    pub authority: HostToolAuthority,
}

impl WorkerRequest {
    fn admission(&self) -> Option<&HostedAdmission> {
        match self {
            Self::OpenHostedTick { admission, .. } | Self::CloseHostedTick { admission, .. } => {
                Some(admission)
            }
            Self::ListHostedTicks { .. } | Self::GetHostedOperationReceipt { .. } => None,
        }
    }

    fn chat(&self) -> Uuid {
        match self {
            Self::OpenHostedTick { chat, .. }
            | Self::CloseHostedTick { chat, .. }
            | Self::ListHostedTicks { chat, .. }
            | Self::GetHostedOperationReceipt { chat, .. } => *chat,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkerEnvelope {
    owner_epoch: Uuid,
    binding: Uuid,
    capability: String,
    request: WorkerRequest,
}

struct Grant {
    capability: String,
    worker: Arc<dyn AgentAccess>,
    root: Option<String>,
}

/// The owner side: one private endpoint, one grant per running worker.
pub struct WorkerControl {
    endpoint: PathBuf,
    epoch: Uuid,
    grants: Mutex<HashMap<Uuid, Grant>>,
    controller: Arc<HostController>,
    owner: OnceLock<OwnerServer>,
}

impl WorkerControl {
    pub fn new(directory: &Path, epoch: Uuid, controller: Arc<HostController>) -> Self {
        Self {
            endpoint: directory.join(ENDPOINT_NAME),
            epoch,
            grants: Mutex::new(HashMap::new()),
            controller,
            owner: OnceLock::new(),
        }
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    /// Completes the owner wiring once the owner server exists.
    pub fn attach_owner(&self, owner: OwnerServer) {
        let _ = self.owner.set(owner);
    }

    /// A fresh grant for a worker about to start.
    pub fn grant(&self) -> WorkerControlGrant {
        WorkerControlGrant {
            endpoint: self.endpoint.clone(),
            owner_epoch: self.epoch,
            binding: Uuid::new_v4(),
            capability: tick_host::host::random_token(),
            host_endpoint: self.controller.endpoint(),
        }
    }

    /// Records the grant once its worker is running, with the config root it was launched for.
    pub async fn register(
        &self,
        grant: &WorkerControlGrant,
        worker: Arc<dyn AgentAccess>,
        config_root: Option<&Path>,
    ) {
        self.grants.lock().await.insert(
            grant.binding,
            Grant {
                capability: grant.capability.clone(),
                worker,
                root: config_root.and_then(root_identity),
            },
        );
    }

    pub async fn revoke(&self, worker: &Arc<dyn AgentAccess>) {
        self.grants
            .lock()
            .await
            .retain(|_, grant| !Arc::ptr_eq(&grant.worker, worker));
    }

    pub async fn serve(self: Arc<Self>, listener: UnixListener) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let control = self.clone();
                    tokio::spawn(async move {
                        let _ = control.connection(stream).await;
                    });
                }
                Err(_) => tokio::time::sleep(ACCEPT_RETRY_DELAY).await,
            }
        }
    }

    /// Serves exactly one bounded request per connection, read within the control timeout.
    async fn connection(&self, stream: UnixStream) -> Result<(), HostError> {
        if peer_uid(&stream)? != unsafe { libc::geteuid() } {
            return Err(HostError::before("Forbidden", "peer is another user"));
        }
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let frame = tokio::time::timeout(
            CONTROL_TIMEOUT,
            read_frame(&mut reader, REQUEST_FRAME_BYTES),
        )
        .await
        .map_err(|_| HostError::before("Timeout", "no worker request arrived"))??
        .ok_or_else(|| HostError::before("Disconnected", "no worker request"))?;
        let id = frame["id"].clone();
        let reply = match serde_json::from_value::<WorkerEnvelope>(frame["envelope"].clone()) {
            Ok(envelope) => self.handle(envelope).await,
            Err(error) => Err(HostError::before("InvalidArguments", error.to_string())),
        };
        let frame = match reply {
            Ok(result) => json!({"id":id,"result":result}),
            Err(error) => json!({"id":id,"error":error}),
        };
        write_frame(&mut writer, &frame, BUSINESS_FRAME_BYTES).await
    }

    async fn handle(&self, envelope: WorkerEnvelope) -> Result<Value, HostError> {
        if envelope.owner_epoch != self.epoch {
            return Err(HostError::before(
                "EpochChanged",
                "this worker belongs to an earlier Agent owner",
            ));
        }
        let (worker, root) = {
            let grants = self.grants.lock().await;
            let grant = grants
                .get(&envelope.binding)
                .filter(|grant| same_secret(&grant.capability, &envelope.capability))
                .ok_or_else(|| HostError::before("Forbidden", "unknown worker grant"))?;
            (grant.worker.clone(), grant.root.clone())
        };
        let owner = self
            .owner
            .get()
            .ok_or_else(|| HostError::before("HostUnavailable", "the owner is starting"))?;
        let chat = envelope.request.chat();
        let execution = owner
            .verify_worker_chat(chat, &worker)
            .await
            .map_err(|error| HostError::before("Forbidden", error.message))?;
        if let Some(admission) = envelope.request.admission() {
            self.admit(&worker, execution, admission).await?;
        }
        let root = root.ok_or_else(|| {
            HostError::before(
                "Unsupported",
                "hosted Ticks need a config root for durable ownership",
            )
        })?;
        let tick_owner = TickOwner { root, chat };
        match envelope.request {
            WorkerRequest::OpenHostedTick {
                operation,
                command,
                cwd,
                environment,
                title,
                ..
            } => self
                .controller
                .open(OpenRequest {
                    operation,
                    owner: tick_owner,
                    command,
                    cwd,
                    environment,
                    title,
                })
                .await
                .map(|opened| json!(opened)),
            WorkerRequest::CloseHostedTick {
                operation,
                instance,
                ..
            } => self
                .controller
                .close(operation, &tick_owner, &instance)
                .await
                .map(|receipt| json!(receipt)),
            WorkerRequest::ListHostedTicks { cursor, .. } => {
                let mut page = self.controller.list(&tick_owner, cursor).await?;
                page["endpoint"] = json!(self.controller.endpoint());
                Ok(page)
            }
            WorkerRequest::GetHostedOperationReceipt { operation, .. } => self
                .controller
                .receipt(operation, &tick_owner)
                .await
                .map(|receipt| json!({"operation":operation,"receipt":receipt})),
        }
    }
}

impl WorkerControl {
    /// A lifecycle request must have been prepared for this owner's Tick host with the tool
    /// that changes Ticks, and a delegated Chat must have inherited that tool for this host. The
    /// ceiling is read through the owner's own route to the Chat, not from the request.
    async fn admit(
        &self,
        worker: &Arc<dyn AgentAccess>,
        execution: ExecutionRef,
        admission: &HostedAdmission,
    ) -> Result<(), HostError> {
        let expected = HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::TickHost {
                endpoint: self.controller.endpoint(),
                uid: tick_host::paths::current_uid(),
            },
        };
        if admission.tool != MODIFY_TICK || admission.authority != expected {
            return Err(HostError::before(
                "Forbidden",
                "the request was not prepared for this owner's Tick host",
            ));
        }
        let observed = worker
            .observe(ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::Snapshot,
                after: None,
            })
            .await
            .map_err(|error| HostError::before("Forbidden", error.message))?;
        let ObservationValue::Agent(snapshot) = observed.initial else {
            return Err(HostError::before(
                "Forbidden",
                "the Chat's authorization could not be read",
            ));
        };
        if let Some(delegation) = &snapshot.conversation.delegation
            && delegation.ceiling.host_tools.get(&admission.tool) != Some(&admission.authority)
        {
            return Err(HostError::before(
                "Forbidden",
                "this delegated Chat did not inherit hosted Tick lifecycles",
            ));
        }
        Ok(())
    }
}

/// The worker side: one request per connection, so a lost connection only affects its own
/// request, whose outcome the worker then reads from the operation receipt.
pub struct OwnerControl {
    grant: WorkerControlGrant,
}

impl OwnerControl {
    pub fn new(grant: WorkerControlGrant) -> Self {
        Self { grant }
    }

    /// The Tick host this worker's owner controls.
    pub fn host_endpoint(&self) -> HostEndpointIdentity {
        HostEndpointIdentity::TickHost {
            endpoint: self.grant.host_endpoint.clone(),
            uid: tick_host::paths::current_uid(),
        }
    }

    pub async fn request(&self, request: WorkerRequest) -> Result<Value, HostError> {
        let stream =
            tokio::time::timeout(CONTROL_TIMEOUT, UnixStream::connect(&self.grant.endpoint))
                .await
                .map_err(|_| {
                    HostError::before("OwnerUnavailable", "the Agent owner did not answer")
                })?
                .map_err(|error| HostError::before("OwnerUnavailable", error.to_string()))?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let envelope = WorkerEnvelope {
            owner_epoch: self.grant.owner_epoch,
            binding: self.grant.binding,
            capability: self.grant.capability.clone(),
            request,
        };
        write_frame(
            &mut writer,
            &json!({"id":1,"envelope":envelope}),
            BUSINESS_FRAME_BYTES,
        )
        .await
        .map_err(|error| HostError::before(&error.code, error.message))?;
        let response = tokio::time::timeout(
            OWNER_ANSWER_TIMEOUT,
            read_frame(&mut reader, BUSINESS_FRAME_BYTES),
        )
        .await
        .map_err(|_| HostError::after("OutcomeUnknown", "the Agent owner did not answer in time"))??
        .ok_or_else(|| {
            HostError::after("OutcomeUnknown", "the Agent owner closed the connection")
        })?;
        tick_connect::hosted::response_result(response, Some(1))
    }
}

/// The config root's identity for ownership scope: user, device, inode and canonical path, so a
/// path that later names another directory never inherits this root's Ticks.
pub fn root_identity(root: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let canonical = root.canonicalize().ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    Some(format!(
        "{}:{}:{}:{}",
        metadata.uid(),
        metadata.dev(),
        metadata.ino(),
        canonical.display()
    ))
}
