use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ChatId, LoopId, Receipt, RunId, TickSubscription};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum LoopState {
    Idle,
    Queued,
    WaitingPermission,
    WaitingQuestion,
    WaitingPlan,
    WaitingTrigger,
    Expired,
    Running,
    Completed,
    Failed,
    Interrupted,
    Stopped,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoopSnapshot {
    pub id: LoopId,
    pub goal: String,
    pub state: LoopState,
    pub run_id: Option<RunId>,
    pub run_started_at: Option<u64>,
    #[serde(default)]
    pub trigger: Option<Trigger>,
    /// Unix milliseconds when the runtime moved this Loop into its current state. A state set by
    /// restoring, and records written before this field, leave it unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_sources: Vec<EventSourceBinding>,
}

impl LoopSnapshot {
    /// A state the Loop is found in when it is restored, not one it entered now: when the state
    /// changes, nobody knows when that happened, so its time becomes unknown.
    pub fn recover_state(&mut self, state: LoopState) {
        if self.state != state {
            self.state = state;
            self.state_since = None;
        }
    }
}

/// A feedback source of a Loop. Saved through [`EventSourceRecord`], which reads
/// publication-only records written by earlier versions unchanged.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "EventSourceRecord", into = "EventSourceRecord")]
pub struct EventSourceBinding {
    pub server_id: String,
    pub owner_account_id: String,
    pub source_key: String,
    /// Proof of a confirmed publication by this Chat; absent for a source that was only connected.
    pub publication: Option<SourcePublication>,
    /// Earlier confirmed publications for replay detection after a later publication.
    pub publication_history: Vec<SourcePublication>,
    /// The explicit connection that created this source and its complete frozen request.
    pub connection: Option<SourceConnection>,
    pub association_operation_id: String,
    pub binding_id: Uuid,
    pub client_instance_id: Uuid,
    pub credential_generation: [u8; 32],
    pub directory_fingerprint: [u8; 32],
    pub bridge_fingerprint: [u8; 32],
    pub association_epoch: Option<u64>,
    pub resume_required: bool,
    /// The association may have been dispatched without a confirmed outcome.
    pub association_unknown: bool,
    /// Public error code of an authenticated definite association rejection.
    pub association_rejected: Option<String>,
    pub pending_cursor: Option<String>,
    pub claim_operations: Vec<EventClaimOperation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourcePublication {
    pub version_id: String,
    pub operation_id: String,
    pub receipt: Receipt,
}

#[derive(Clone, Debug)]
pub struct SourceConnection {
    pub operation_id: String,
    /// The Receipt of the run whose approved connect created this request.
    pub receipt: Receipt,
    pub request: FrozenAssociationRequest,
}

/// The complete association request saved before its only dispatch; an exact same-ID retry
/// reuses it unchanged.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FrozenAssociationRequest {
    pub server_id: String,
    pub owner_account_id: String,
    pub source_key: String,
    pub operation_id: String,
    pub binding_id: Uuid,
    pub client_instance_id: Uuid,
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub expected_epoch: Option<u64>,
    pub replace_existing: bool,
    pub credential_generation: [u8; 32],
    pub directory_fingerprint: [u8; 32],
    pub bridge_fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventClaimOperation {
    pub event_id: String,
    pub operation_id: String,
    pub status: EventClaimStatus,
    #[serde(default)]
    pub claim_id: Option<String>,
    #[serde(default)]
    pub lease_expires_at: Option<String>,
    #[serde(default)]
    pub renewal_operation_id: Option<String>,
    #[serde(default)]
    pub release_operation_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum EventClaimStatus {
    Pending,
    Claimed,
    Completed,
    NeedsAttention,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Trigger {
    pub id: Uuid,
    pub prompt: String,
    pub interval_seconds: Option<u64>,
    pub expires_at_ms: Option<u64>,
    pub accepts_events: bool,
    pub armed: bool,
    /// The Tick Instances this Loop receives Tick Updates from, each with its own cadence.
    #[serde(default)]
    pub subscriptions: Vec<TickSubscription>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TriggerSource {
    pub source: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LoopTriggerNoticeKind {
    /// 另一个 Tick Loop 进程持有该 Loop 的 Trigger，本进程等待接手。
    HeldByAnotherProcess,
    /// 租约或停止状态无法读写，或锁由另一台主机持有；需要用户处理。
    Warning,
}

/// 宿主状态中需要让用户看到的 Trigger 租约情况。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LoopTriggerNotice {
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub kind: LoopTriggerNoticeKind,
    pub message: String,
}

const MAX_SAFE_EPOCH: u64 = 9_007_199_254_740_991;

/// Saved shape of [`EventSourceBinding`]. Publication-only records keep the original three
/// publication fields; a connected source adds a tagged origin and its frozen request. Records
/// containing a connection are not readable by binaries older than this format.
#[derive(Deserialize, Serialize)]
pub struct EventSourceRecord {
    server_id: String,
    owner_account_id: String,
    source_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_receipt: Option<Receipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    publication_history: Vec<SourcePublication>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<SourceOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    association_request: Option<FrozenAssociationRequest>,
    association_operation_id: String,
    binding_id: Uuid,
    client_instance_id: Uuid,
    credential_generation: [u8; 32],
    directory_fingerprint: [u8; 32],
    bridge_fingerprint: [u8; 32],
    #[serde(default)]
    association_epoch: Option<u64>,
    #[serde(default)]
    resume_required: bool,
    #[serde(default)]
    association_unknown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    association_rejected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    claim_operations: Vec<EventClaimOperation>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SourceOrigin {
    Connection {
        operation_id: String,
        receipt: Receipt,
    },
}

impl TryFrom<EventSourceRecord> for EventSourceBinding {
    type Error = String;

    fn try_from(record: EventSourceRecord) -> Result<Self, Self::Error> {
        let publication = match (
            record.version_id,
            record.publication_operation_id,
            record.publication_receipt,
        ) {
            (Some(version_id), Some(operation_id), Some(receipt)) => Some(SourcePublication {
                version_id,
                operation_id,
                receipt,
            }),
            (None, None, None) => None,
            _ => return Err("feedback source publication provenance is incomplete".into()),
        };
        let mut operation_ids = std::collections::HashSet::new();
        if let Some(latest) = &publication {
            operation_ids.insert(latest.operation_id.as_str());
        }
        if record.publication_history.iter().any(|prior| {
            prior.version_id.is_empty()
                || prior.operation_id.is_empty()
                || !operation_ids.insert(prior.operation_id.as_str())
        }) {
            return Err("feedback source publication history is invalid".into());
        }
        if publication.is_none() && !record.publication_history.is_empty() {
            return Err("feedback source publication history has no latest publication".into());
        }
        let connection = match (record.origin, record.association_request) {
            (
                Some(SourceOrigin::Connection {
                    operation_id,
                    receipt,
                }),
                Some(request),
            ) if request.operation_id == operation_id
                && request.server_id == record.server_id
                && request.owner_account_id == record.owner_account_id
                && request.source_key == record.source_key
                && request.chat_id == receipt.chat_id
                && request.loop_id == receipt.loop_id
                && request
                    .expected_epoch
                    .is_none_or(|epoch| (1..=MAX_SAFE_EPOCH).contains(&epoch)) =>
            {
                Some(SourceConnection {
                    operation_id,
                    receipt,
                    request,
                })
            }
            (None, None) => None,
            _ => return Err("feedback source connection provenance is invalid".into()),
        };
        if publication.is_none() && connection.is_none() {
            return Err("feedback source has no publication or connection provenance".into());
        }
        Ok(Self {
            server_id: record.server_id,
            owner_account_id: record.owner_account_id,
            source_key: record.source_key,
            publication,
            publication_history: record.publication_history,
            connection,
            association_operation_id: record.association_operation_id,
            binding_id: record.binding_id,
            client_instance_id: record.client_instance_id,
            credential_generation: record.credential_generation,
            directory_fingerprint: record.directory_fingerprint,
            bridge_fingerprint: record.bridge_fingerprint,
            association_epoch: record.association_epoch,
            resume_required: record.resume_required,
            association_unknown: record.association_unknown,
            association_rejected: record.association_rejected,
            pending_cursor: record.pending_cursor,
            claim_operations: record.claim_operations,
        })
    }
}

impl From<EventSourceBinding> for EventSourceRecord {
    fn from(binding: EventSourceBinding) -> Self {
        let (version_id, publication_operation_id, publication_receipt) = match binding.publication
        {
            Some(publication) => (
                Some(publication.version_id),
                Some(publication.operation_id),
                Some(publication.receipt),
            ),
            None => (None, None, None),
        };
        let (origin, association_request) = match binding.connection {
            Some(connection) => (
                Some(SourceOrigin::Connection {
                    operation_id: connection.operation_id,
                    receipt: connection.receipt,
                }),
                Some(connection.request),
            ),
            None => (None, None),
        };
        Self {
            server_id: binding.server_id,
            owner_account_id: binding.owner_account_id,
            source_key: binding.source_key,
            version_id,
            publication_operation_id,
            publication_receipt,
            publication_history: binding.publication_history,
            origin,
            association_request,
            association_operation_id: binding.association_operation_id,
            binding_id: binding.binding_id,
            client_instance_id: binding.client_instance_id,
            credential_generation: binding.credential_generation,
            directory_fingerprint: binding.directory_fingerprint,
            bridge_fingerprint: binding.bridge_fingerprint,
            association_epoch: binding.association_epoch,
            resume_required: binding.resume_required,
            association_unknown: binding.association_unknown,
            association_rejected: binding.association_rejected,
            pending_cursor: binding.pending_cursor,
            claim_operations: binding.claim_operations,
        }
    }
}
