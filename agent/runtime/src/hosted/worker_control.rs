//! The worker side and wire types of the authenticated worker-to-daemon channel for hosted Tick
//! lifecycles and Shell spawns. A worker proves its private grant and names the Chat it acts for;
//! the daemon (`agent_daemon::worker_control`) verifies that the Chat runs in that worker and
//! dispatches through its own host controller. Workers never hold host control or reconcile
//! rights.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost_connect::{
    host_error::HostError,
    hosted::{BUSINESS_FRAME_BYTES, CONTROL_TIMEOUT, read_frame, write_frame},
    page::PageCursor,
};
use tokio::{io::BufReader, net::UnixStream};
use uuid::Uuid;

use super::controller::LIFECYCLE_TIMEOUT;
use tickloop_agent_protocol::{HostEndpointIdentity, HostToolAuthority};

pub const ENDPOINT_NAME: &str = "worker-control.sock";
/// How long a worker waits for the daemon: the longest lifecycle request plus transport.
const DAEMON_ANSWER_TIMEOUT: Duration =
    Duration::from_secs(LIFECYCLE_TIMEOUT.as_secs() + CONTROL_TIMEOUT.as_secs());

/// What a worker receives privately at startup. The capability never reaches logs or models.
#[derive(Clone, Deserialize, Serialize)]
pub struct WorkerControlGrant {
    pub endpoint: PathBuf,
    pub daemon_epoch: Uuid,
    pub binding: Uuid,
    pub capability: String,
    /// The Tickhost endpoint this daemon controls: the authority of the worker's hosted tools.
    pub host_endpoint: PathBuf,
}

impl std::fmt::Debug for WorkerControlGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerControlGrant")
            .field("endpoint", &self.endpoint)
            .field("daemon_epoch", &self.daemon_epoch)
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
        adapter: tickhost::host::TickAdapter,
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
    /// Starts a new instance in place of `instance`, whose host was lost, from its recorded
    /// launch.
    ReopenHostedTick {
        chat: Uuid,
        operation: Uuid,
        instance: InstanceRef,
        environment: Vec<(String, String)>,
        admission: HostedAdmission,
    },
    /// Lets one View of `chat` attach to its hosted `instance` once.
    AllowHostedView {
        chat: Uuid,
        instance: InstanceRef,
    },
    /// Root Chat `chat` stopped its direct child `child` with StopSubagent.
    EndStoppedChild {
        chat: Uuid,
        child: Uuid,
        operation: Uuid,
    },
    /// A one-time grant for this worker to start the command `chat` approved and checked last.
    AllowShellSpawn {
        chat: Uuid,
        operation: Uuid,
        approval: tickhost::shell::ShellApproval,
    },
    /// Lets one View of `chat` show the Shell task `instance` this worker submitted for it once.
    AllowShellView {
        chat: Uuid,
        instance: InstanceRef,
    },
}

/// A spawn grant as the daemon hands it to its worker: what it binds, and the host to claim it at.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IssuedSpawn {
    pub grant: tickhost::shell::SpawnGrant,
    pub binding: tickhost::shell::SpawnBinding,
    pub endpoint: PathBuf,
}

/// One page of a Chat's hosted Ticks, as the daemon answers its worker.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostedTickPage {
    #[serde(flatten)]
    pub page: tickhost_connect::page::Page<tickhost::host::TickRow>,
    /// The host endpoint the listed Ticks' data bindings reach.
    pub endpoint: PathBuf,
    /// On a listing from the start: the Chat's instances lost with their host.
    pub lost: Vec<LostTick>,
}

/// An instance lost with its host, which its user can reopen as a new one.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LostTick {
    #[serde(default)]
    pub is_terminal: bool,
    pub instance: InstanceRef,
    pub title: Option<String>,
}

/// What a lifecycle request was admitted under in the worker: the tool it prepared and the
/// host authority it froze. The daemon checks both against its own host and the Chat's ceiling.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostedAdmission {
    pub tool: String,
    pub authority: HostToolAuthority,
}

impl WorkerRequest {
    pub fn admission(&self) -> Option<&HostedAdmission> {
        match self {
            Self::OpenHostedTick { admission, .. }
            | Self::CloseHostedTick { admission, .. }
            | Self::ReopenHostedTick { admission, .. } => Some(admission),
            Self::ListHostedTicks { .. }
            | Self::GetHostedOperationReceipt { .. }
            | Self::AllowHostedView { .. }
            | Self::EndStoppedChild { .. }
            | Self::AllowShellSpawn { .. }
            | Self::AllowShellView { .. } => None,
        }
    }

    pub fn chat(&self) -> Uuid {
        match self {
            Self::OpenHostedTick { chat, .. }
            | Self::CloseHostedTick { chat, .. }
            | Self::ReopenHostedTick { chat, .. }
            | Self::ListHostedTicks { chat, .. }
            | Self::GetHostedOperationReceipt { chat, .. }
            | Self::AllowHostedView { chat, .. }
            | Self::EndStoppedChild { chat, .. }
            | Self::AllowShellSpawn { chat, .. }
            | Self::AllowShellView { chat, .. } => *chat,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerEnvelope {
    pub daemon_epoch: Uuid,
    pub binding: Uuid,
    pub capability: String,
    pub request: WorkerRequest,
}

/// The worker side: one request per connection, so a lost connection only affects its own
/// request, whose outcome the worker then reads from the operation receipt.
pub struct DaemonControl {
    grant: WorkerControlGrant,
}

impl DaemonControl {
    pub fn new(grant: WorkerControlGrant) -> Self {
        Self { grant }
    }

    /// The Tickhost this worker's daemon controls.
    pub fn host_endpoint(&self) -> HostEndpointIdentity {
        HostEndpointIdentity::Tickhost {
            endpoint: self.grant.host_endpoint.clone(),
            uid: tickhost::paths::current_uid(),
        }
    }

    pub async fn request(&self, request: WorkerRequest) -> Result<Value, HostError> {
        let stream =
            tokio::time::timeout(CONTROL_TIMEOUT, UnixStream::connect(&self.grant.endpoint))
                .await
                .map_err(|_| {
                    HostError::before("DaemonUnavailable", "the Agent daemon did not answer")
                })?
                .map_err(|error| HostError::before("DaemonUnavailable", error.to_string()))?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let envelope = WorkerEnvelope {
            daemon_epoch: self.grant.daemon_epoch,
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
            DAEMON_ANSWER_TIMEOUT,
            read_frame(&mut reader, BUSINESS_FRAME_BYTES),
        )
        .await
        .map_err(|_| {
            HostError::after("OutcomeUnknown", "the Agent daemon did not answer in time")
        })??
        .ok_or_else(|| {
            HostError::after("OutcomeUnknown", "the Agent daemon closed the connection")
        })?;
        tickhost_connect::hosted::response_result(response, Some(1))
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
