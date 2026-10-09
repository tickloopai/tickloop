use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ChatId, LoopId, RunId, TickSubscription};

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

/// An artifact linked to a Loop for one account's @tickloop feedback requests. Records saved in
/// earlier formats do not parse and are dropped when the Chat loads.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventSourceBinding {
    pub origin: String,
    pub account_id: String,
    pub artifact_id: String,
    pub binding_id: Uuid,
    pub client_instance_id: Uuid,
    pub epoch: u64,
    /// Listening waits for an explicit Continue, which links the artifact again.
    pub resume_required: bool,
    /// Unix milliseconds of the newest feedback request delivered through this link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_at_ms: Option<u64>,
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
