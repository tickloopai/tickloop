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
    /// How an exited terminal Tick's program ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<HostedProgramExit>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostedMemberState {
    Running,
    /// A terminal Tick whose program ended; its last screen stays until it is closed.
    Exited,
    /// Its host died with it; only an explicit reopen starts a new instance in its place.
    Lost,
}

/// How a terminal Tick's program ended: its exit code, or the signal that ended it.
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct HostedProgramExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// A hosted Tick's screen as text when it was taken: the newest lines within the snapshot
/// limits, saying whether older ones were cut.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct HostedSnapshot {
    pub screen: HostedScreenSnapshot,
    pub taken_at_ms: u64,
}

/// The `tickloop` arguments that run the attach client of one hosted Tick view: a Monitor popup,
/// or a Herdr Pane that shows the Tick.
pub const ATTACH_ARGUMENTS: [&str; 2] = ["monitor", "attach"];
/// The environment variable through which an attach client receives its view grant as JSON.
pub const ATTACH_ENVIRONMENT: &str = "TICKLOOP_MONITOR_ATTACH";

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
