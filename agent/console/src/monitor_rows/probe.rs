//! Probes of Monitor members' host state. Probing reads host state only: it never joins,
//! subscribes, reads Tick content or calls the model.

use super::*;

/// A host whose members' state cannot be read now.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProbeError(pub String);

/// Reads the host state of a Chat's members of one Tick type.
#[async_trait::async_trait]
pub trait TickProbe: Send + Sync {
    /// The Tick type whose instances this probe reports on.
    fn tick_type(&self) -> &'static str;
    /// The current host state of `chat`'s member `instances`. A host that cannot be read reports
    /// an error, so earlier findings stay instead of being wiped.
    async fn probe(
        &self,
        chat: Uuid,
        instances: &[InstanceRef],
    ) -> Result<MemberFindings, ProbeError>;
}

/// What a host probe found for members no subscription reports on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemberFindings {
    pub states: HashMap<InstanceRef, MemberState>,
    /// Process samples of the members whose host takes them.
    pub resources: HashMap<InstanceRef, TickResources>,
    /// When each protocol member last sent a Tick Update, as its host saw it without anyone
    /// subscribing or reading the update.
    pub updates: HashMap<InstanceRef, u64>,
}
