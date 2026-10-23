use serde::{Deserialize, Serialize};
use tick_protocol::InstanceRef;
use uuid::Uuid;

use crate::{ChatId, HostToolAuthority, LoopId, RunId, TickSubscription};

/// What the Chat's Loop is doing in its current turn. A Loop has no ending state: a failed or
/// interrupted turn is only how that turn ended, and whether Triggers reach the Loop is its
/// [`Receiving`] switch.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum LoopState {
    Idle,
    Queued,
    WaitingPermission,
    WaitingQuestion,
    WaitingPlan,
    Running,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoopSnapshot {
    pub id: LoopId,
    pub state: LoopState,
    pub run_id: Option<RunId>,
    pub run_started_at: Option<u64>,
    /// Unix milliseconds when the runtime moved this Loop into its current state. A state set by
    /// restoring, and records written before this field, leave it unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<u64>,
    #[serde(default)]
    pub receiving: Receiving,
    /// The Tick Instances this Loop receives Tick Updates from, each at its own frequency.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<TickSubscription>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_sources: Vec<EventSourceBinding>,
}

impl LoopSnapshot {
    /// The Loop a new Chat starts with: idle and receiving, with nothing to receive yet.
    pub fn new(id: LoopId) -> Self {
        Self {
            id,
            state: LoopState::Idle,
            run_id: None,
            run_started_at: None,
            state_since: None,
            receiving: Receiving::Receiving,
            subscriptions: Vec::new(),
            event_sources: Vec::new(),
        }
    }

    /// A state the Loop is found in when it is restored, not one it entered now: when the state
    /// changes, nobody knows when that happened, so its time becomes unknown.
    pub fn recover_state(&mut self, state: LoopState) {
        if self.state != state {
            self.state = state;
            self.state_since = None;
        }
    }
}

/// Whether Triggers reach the Loop: the one switch every Trigger source answers to. While paused
/// the sources keep what they have and deliver nothing, and queued Trigger input is not taken.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Receiving {
    #[default]
    Receiving,
    /// Until the Chat is continued: the user chooses to continue, or submits a new input.
    Paused { reason: PauseReason },
}

impl Receiving {
    pub fn is_receiving(self) -> bool {
        match self {
            Self::Receiving => true,
            Self::Paused { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// `/stop`, here or in another process holding the same Chat.
    Stopped,
    /// The Chat was loaded from its history; a planned upgrade's live handoff is no such load.
    HistoryLoaded,
    /// A Trigger input's start could not be recorded in the Transcript for too long; the
    /// process keeps the Chat's lease and tries on.
    TranscriptUnavailable,
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
    /// The link itself is unavailable: its account signed out or lost access to the artifact.
    /// Whether the Loop receives at all is its [`Receiving`] switch.
    pub resume_required: bool,
    /// Unix milliseconds of the newest feedback request delivered through this link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_at_ms: Option<u64>,
}

/// Where one Trigger delivery comes from. Every rule of the wake-up base matches on it, so a new
/// source is a new variant every rule must answer for.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSource {
    /// The latest Tick Updates of the Loop's Tick Subscriptions that came due together.
    TickUpdate { items: Vec<TickDeliveryItem> },
    /// A Tick's current content the user sent once with Send Update, read at `read_at_ms`.
    TickManual {
        instance: InstanceRef,
        sequence: u64,
        read_at_ms: u64,
        authority: Option<HostToolAuthority>,
    },
    /// One @tickloop feedback request on an artifact linked to the Loop.
    ArtifactFeedback {
        origin: String,
        account_id: String,
        artifact_id: String,
        feedback_id: String,
        delivery_id: Uuid,
    },
    /// The Goal's continuation after the turn `after_run` ended, written for the objective at
    /// `objective_revision`; a continuation taken after the objective changed is written again.
    GoalContinuation {
        goal_id: Uuid,
        after_run: RunId,
        #[serde(default)]
        objective_revision: u64,
    },
    /// A scheduled task's run for one time its schedule names, in Unix milliseconds.
    ScheduledTask {
        #[serde(deserialize_with = "crate::scheduled_tasks::scheduled_task_reference")]
        task_id: Uuid,
        scheduled_for_ms: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        wrappers: Vec<crate::ScheduledInputWrapper>,
    },
}

/// One subscription's update a Tick delivery carries: the newest of the `updates` it merges.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TickDeliveryItem {
    pub subscription: Uuid,
    pub watch: Uuid,
    pub instance: InstanceRef,
    pub sequence: u64,
    pub updates: u64,
}

/// What one Trigger delivery is: the part of its [`TriggerSource`] that every delivery of it
/// carries alike. What another delivery of it may carry differently, such as the watch a Tick
/// Update came through, how many updates it merged or when a Tick was read, is no part of it.
/// It names the delivery's ledger id, and its records keep it to say where it came from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerIdentity {
    /// Each subscription's update, ordered by subscription and sequence.
    TickUpdate {
        updates: Vec<SubscriptionSequence>,
    },
    TickManual {
        instance: InstanceRef,
        sequence: u64,
    },
    ArtifactFeedback {
        origin: String,
        account_id: String,
        delivery_id: Uuid,
    },
    GoalContinuation {
        goal_id: Uuid,
        after_run: RunId,
    },
    ScheduledTask {
        #[serde(deserialize_with = "crate::scheduled_tasks::scheduled_task_reference")]
        task_id: Uuid,
        scheduled_for_ms: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        wrappers: Vec<crate::ScheduledInputWrapper>,
    },
}

/// One Tick Subscription's update by its sequence.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SubscriptionSequence {
    pub subscription: Uuid,
    pub sequence: u64,
}

impl TriggerIdentity {
    pub fn kind(&self) -> TriggerKind {
        match self {
            Self::TickUpdate { .. } => TriggerKind::TickUpdate,
            Self::TickManual { .. } => TriggerKind::TickManual,
            Self::ArtifactFeedback { .. } => TriggerKind::ArtifactFeedback,
            Self::GoalContinuation { .. } => TriggerKind::GoalContinuation,
            Self::ScheduledTask { .. } => TriggerKind::ScheduledTask,
        }
    }
}

/// The kinds of [`TriggerSource`], without their content.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    TickUpdate,
    TickManual,
    ArtifactFeedback,
    GoalContinuation,
    ScheduledTask,
}

impl TriggerKind {
    /// What a delivery of this kind is to the Agent.
    pub fn authority(self) -> Authority {
        match self {
            Self::TickUpdate | Self::TickManual | Self::ArtifactFeedback => Authority::Data,
            Self::GoalContinuation | Self::ScheduledTask => Authority::Instruction,
        }
    }
}

/// How deliveries of one source wait while the Loop cannot take them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Coalesce {
    /// Only the latest waits: the source keeps merging into it until the Loop's turn ends.
    LatestOnly,
    /// Each delivery waits on its own.
    EachItem,
    /// At most one delivery per key waits; another for the same key merges into it.
    OnePerKey(String),
}

/// What a delivery's content is to the Agent.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// Untrusted data the turn looks at, never an instruction.
    Data,
    /// An instruction the user authorized ahead of time.
    Instruction,
}

impl TriggerSource {
    pub fn kind(&self) -> TriggerKind {
        match self {
            Self::TickUpdate { .. } => TriggerKind::TickUpdate,
            Self::TickManual { .. } => TriggerKind::TickManual,
            Self::ArtifactFeedback { .. } => TriggerKind::ArtifactFeedback,
            Self::GoalContinuation { .. } => TriggerKind::GoalContinuation,
            Self::ScheduledTask { .. } => TriggerKind::ScheduledTask,
        }
    }

    pub fn identity(&self) -> TriggerIdentity {
        match self {
            Self::TickUpdate { items } => {
                let mut updates: Vec<_> = items
                    .iter()
                    .map(|item| SubscriptionSequence {
                        subscription: item.subscription,
                        sequence: item.sequence,
                    })
                    .collect();
                updates.sort_unstable();
                TriggerIdentity::TickUpdate { updates }
            }
            Self::TickManual {
                instance, sequence, ..
            } => TriggerIdentity::TickManual {
                instance: instance.clone(),
                sequence: *sequence,
            },
            Self::ArtifactFeedback {
                origin,
                account_id,
                delivery_id,
                ..
            } => TriggerIdentity::ArtifactFeedback {
                origin: origin.clone(),
                account_id: account_id.clone(),
                delivery_id: *delivery_id,
            },
            Self::GoalContinuation {
                goal_id, after_run, ..
            } => TriggerIdentity::GoalContinuation {
                goal_id: *goal_id,
                after_run: *after_run,
            },
            Self::ScheduledTask {
                task_id,
                scheduled_for_ms,
                wrappers,
            } => TriggerIdentity::ScheduledTask {
                task_id: *task_id,
                scheduled_for_ms: *scheduled_for_ms,
                wrappers: wrappers.clone(),
            },
        }
    }

    pub fn coalesce(&self) -> Coalesce {
        match self {
            Self::TickUpdate { .. } => Coalesce::LatestOnly,
            Self::TickManual { .. } | Self::ArtifactFeedback { .. } => Coalesce::EachItem,
            Self::GoalContinuation { goal_id, .. } => Coalesce::OnePerKey(goal_id.to_string()),
            Self::ScheduledTask { task_id, .. } => Coalesce::OnePerKey(task_id.to_string()),
        }
    }

    pub fn authority(&self) -> Authority {
        self.kind().authority()
    }

    /// How many source events the delivery merges: a Tick Update counts every update its items
    /// replaced.
    pub fn merged(&self) -> u64 {
        match self {
            Self::TickUpdate { items } => items.iter().map(|item| item.updates).sum(),
            Self::TickManual { .. }
            | Self::ArtifactFeedback { .. }
            | Self::GoalContinuation { .. }
            | Self::ScheduledTask { .. } => 1,
        }
    }

    /// Whether `other` waits in the same place as this delivery: a later delivery of the same
    /// source, or of the same key, merges with it.
    pub fn shares_wait_with(&self, other: &Self) -> bool {
        match self.coalesce() {
            Coalesce::LatestOnly => self.kind() == other.kind(),
            Coalesce::EachItem => false,
            Coalesce::OnePerKey(key) => {
                self.kind() == other.kind() && other.coalesce() == Coalesce::OnePerKey(key)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LoopTriggerNoticeKind {
    /// 另一个 Tickloop 进程持有该 Chat 的 Trigger 租约，本进程等待接手。
    HeldByAnotherProcess,
    /// 租约或接收状态无法读写，或锁由另一台主机持有；需要用户处理。
    Warning,
}

/// 宿主状态中需要让用户看到的 Trigger 租约情况。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LoopTriggerNotice {
    pub chat_id: ChatId,
    pub kind: LoopTriggerNoticeKind,
    pub message: String,
}
