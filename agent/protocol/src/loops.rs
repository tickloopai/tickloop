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
    /// Records written before the source was kept read as explicit, the way they were delivered.
    #[serde(default)]
    pub prompt_source: TriggerPromptSource,
    pub interval_seconds: Option<u64>,
    pub expires_at_ms: Option<u64>,
    pub accepts_events: bool,
    pub armed: bool,
    /// The Tick Instances this Loop receives Tick Updates from, each at its own frequency.
    #[serde(default)]
    pub subscriptions: Vec<TickSubscription>,
}

impl Trigger {
    /// The Trigger a Tick Subscription adds to a Loop that has none: it takes events and carries
    /// the Loop's goal, which deliveries present as background rather than as an instruction.
    pub fn for_loop_goal(goal: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            prompt: goal.to_owned(),
            prompt_source: TriggerPromptSource::LoopGoal,
            interval_seconds: None,
            expires_at_ms: None,
            accepts_events: true,
            armed: true,
            subscriptions: Vec::new(),
        }
    }

    /// Gives the Trigger a prompt the user or the Agent wrote for it, the instruction each firing
    /// carries.
    pub fn set_explicit_prompt(&mut self, prompt: String) {
        self.prompt = prompt;
        self.prompt_source = TriggerPromptSource::Explicit;
    }
}

/// Where a Trigger's prompt came from, which decides how a firing presents it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum TriggerPromptSource {
    /// Written for this Trigger: the instruction for the turn each firing starts.
    #[default]
    Explicit,
    /// Copied from the Loop's goal because a Tick Subscription needed a Trigger: what the user
    /// first asked of the Loop, given as background and never as a new instruction.
    LoopGoal,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TriggerSource {
    pub source: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LoopTriggerNoticeKind {
    /// 另一个 Tickloop 进程持有该 Loop 的 Trigger，本进程等待接手。
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
