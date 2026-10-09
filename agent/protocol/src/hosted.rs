use std::path::PathBuf;

use tick_protocol::InstanceRef;
use uuid::Uuid;

use crate::{HostedResources, HostedScreenSnapshot};

/// One hosted Tick as its Chat's Monitor shows it.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct HostedMember {
    pub instance: InstanceRef,
    pub title: Option<String>,
    pub state: HostedMemberState,
    pub resources: Option<HostedResources>,
    /// When the host last received a Tick Update from it, in Unix milliseconds. The host tracks
    /// this without anyone subscribing and without reading the update's content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostedMemberState {
    Running,
    /// Its host died with it; only an explicit reopen starts a new instance in its place.
    Lost,
}

/// A hosted Tick's screen as text when it was taken: the newest lines within the snapshot
/// limits, saying whether older ones were cut.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct HostedSnapshot {
    pub screen: HostedScreenSnapshot,
    pub taken_at_ms: u64,
}

/// One view's permission to attach to a hosted Tick's terminal: it reaches `endpoint` as a
/// viewer of `instance` once, and only the host process `host_epoch` honours it. It carries no
/// data token.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostedViewGrant {
    pub endpoint: PathBuf,
    pub host_epoch: Uuid,
    pub instance: InstanceRef,
    pub view: Uuid,
}
