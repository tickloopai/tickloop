use std::time::Duration;

use serde::{Deserialize, Serialize};
use tick_protocol::InstanceRef;
use uuid::Uuid;

use crate::{HostToolAuthority, LoopId, LoopState};

pub const TICK_SUBSCRIPTION_SOURCE: &str = "tick_subscription";
/// The most one delivery may add to the model's context; an item it carries alone may take all of
/// it. Tick Update data over its item's share is trimmed by whole segments, and items that no
/// longer fit wait for the Loop's next turn.
pub const MAX_DELIVERY_PAYLOAD_BYTES: usize = 16 * 1024;
/// The most one item of a delivery carrying several may take.
pub const MAX_DELIVERY_ITEM_BYTES: usize = 4 * 1024;

/// How often a Tick Subscription hands the latest Tick Update to the Agent. `Immediate` is decided
/// by the Tick type, never chosen: a Shell Tick is delivered as soon as it updates. Protocol and
/// Terminal Ticks choose one of [`Self::PERIODIC`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickSubscriptionFrequency {
    Immediate,
    High,
    Medium,
    Low,
}

impl TickSubscriptionFrequency {
    /// The frequencies a subscriber chooses among, from the most to the least frequent.
    pub const PERIODIC: [Self; 3] = [Self::High, Self::Medium, Self::Low];

    /// How long a periodic frequency waits between deliveries; `None` for an immediate one.
    pub fn period(self) -> Option<Duration> {
        match self {
            Self::Immediate => None,
            Self::High => Some(Duration::from_secs(60)),
            Self::Medium => Some(Duration::from_secs(600)),
            Self::Low => Some(Duration::from_secs(1800)),
        }
    }

    /// The name the model tool and the View use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// One Loop's association with a Tick Instance: it receives that instance's Tick Updates, and the
/// latest one is handed to the Agent at its `frequency`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TickSubscription {
    pub id: Uuid,
    pub instance: InstanceRef,
    pub frequency: TickSubscriptionFrequency,
    pub authority: Option<HostToolAuthority>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TickSubscriptionChange {
    Frequency(TickSubscriptionFrequency),
    Cancel,
}

/// Whether a subscription currently reaches its Tick Instance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TickLinkState {
    #[default]
    Initializing,
    Live,
    Disconnected,
    InstanceGone,
    IdentityUncertain,
}

/// One subscription of the Chat's Loops, as Monitor shows it on its Tick Instance's row.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TickSubscriptionStatus {
    pub id: Uuid,
    pub loop_id: LoopId,
    pub loop_state: LoopState,
    pub instance: InstanceRef,
    pub frequency: TickSubscriptionFrequency,
    /// Tick Updates accumulated since the last delivery.
    pub accumulated: u64,
    pub last_delivery_ms: Option<u64>,
    pub last_delivery: Option<String>,
    #[serde(default)]
    pub kind: Option<ObservationKind>,
    #[serde(default)]
    pub title: Option<String>,
    /// When the instance last updated, in Unix milliseconds: the latest Tick Update or
    /// observation this subscription received, or the update time the instance's metadata
    /// reports, whichever is later.
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    pub link: TickLinkState,
    /// When a periodic subscription next checks its instance, in Unix milliseconds; `None` for an
    /// immediate one, and before the Agent schedules it.
    #[serde(default)]
    pub next_check_at_ms: Option<u64>,
    /// How long a periodic subscription waits between checks, in milliseconds; `None` for an
    /// immediate one.
    #[serde(default)]
    pub period_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ObservationKind {
    Protocol,
    Terminal { truncated: bool },
}
