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

/// A feedback source of a Loop. Saved through `event_sources::EventSourceRecord`, which reads
/// publication-only records written by earlier versions unchanged.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    try_from = "event_sources::EventSourceRecord",
    into = "event_sources::EventSourceRecord"
)]
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
