//! Records the Agent wire carries from its native hosts. Each one has the fields and serde shape
//! of the host type it mirrors, so a change inside Herdr or the Tickhost reaches the Agent wire
//! only through an explicit conversion in the runtime.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A Herdr Pane as `tickhost_connect::herdr::Pane` reads it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HerdrPane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
}

/// A hosted Tick's resource sample as `tickhost::host::Resources` reports it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct HostedResources {
    pub business_pid: u32,
    pub cpu_time_ms: Option<u64>,
    pub rss_bytes: Option<u64>,
    pub sampled_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HerdrInteraction {
    pub socket: PathBuf,
    pub pane: HerdrPane,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostCallContext {
    pub folder: PathBuf,
    pub workspace_id: Option<String>,
    pub interaction: Option<HerdrInteraction>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostEndpointIdentity {
    HerdrSocket {
        path: PathBuf,
        owner: u32,
        device: u64,
        inode: u64,
    },
    TrustedAdapter {
        scope: uuid::Uuid,
    },
    /// The user's Tickhost endpoint. Its process epoch changes across restarts and is checked
    /// per call through each instance's binding, not here.
    Tickhost {
        endpoint: PathBuf,
        uid: u32,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostToolAuthority {
    Native {
        endpoint: HostEndpointIdentity,
    },
    Mcp {
        server_id: String,
        account_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OtherTickhost {
    /// Which build runs and which the daemon is.
    pub description: String,
    /// What replacing it ends; `None` from a Tickhost too old to say.
    pub occupancy: Option<TickhostOccupancy>,
}

/// What stopping a Tickhost ends.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct TickhostOccupancy {
    /// Each Tick it holds, by its title, or its instance when it has none.
    pub ticks: Vec<String>,
    pub shells: usize,
    /// Ticks and Shells it is starting, which stopping it ends before they run.
    pub pending: usize,
    /// The views attached to its Ticks and Shells, which stopping it closes; `None` from a
    /// Tickhost too old to say.
    pub views: Option<usize>,
}
