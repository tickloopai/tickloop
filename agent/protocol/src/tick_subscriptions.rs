use serde::{Deserialize, Serialize};
use tick_protocol::InstanceRef;
use uuid::Uuid;

use crate::{HostToolAuthority, LoopId, LoopState};

pub const TICK_SUBSCRIPTION_SOURCE: &str = "tick_subscription";
pub const DEFAULT_CADENCE_SECONDS: u64 = 60;
pub const MIN_CADENCE_SECONDS: u64 = 5;
pub const MAX_CADENCE_SECONDS: u64 = 86_400;
/// The most one delivery may add to the model's context. A larger one loses its biggest Tick
/// Update data first and tells the model which tool brings it back.
pub const MAX_DELIVERY_PAYLOAD_BYTES: usize = 16 * 1024;

/// One Loop's association with a Tick Instance: it receives that instance's Tick Updates, and the
/// latest one is handed to the Agent every `every_seconds`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TickSubscription {
    pub id: Uuid,
    pub instance: InstanceRef,
    pub every_seconds: u64,
    pub authority: Option<HostToolAuthority>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TickSubscriptionChange {
    Cadence(u64),
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
    pub every_seconds: u64,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ObservationKind {
    Protocol,
    Terminal { truncated: bool },
}
