//! Records the Agent wire carries from its native hosts. Each one has the fields and serde shape
//! of the host type it mirrors, so a change inside Herdr or the Tickhost reaches the Agent wire
//! only through an explicit conversion in the runtime.

use std::{collections::BTreeMap, path::PathBuf};

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
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

/// A hosted Tick's resource sample as `tickhost::host::Resources` reports it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct HostedResources {
    pub business_pid: u32,
    pub cpu_time_ms: Option<u64>,
    pub rss_bytes: Option<u64>,
    pub sampled_at_ms: u64,
}

/// A hosted Tick's latest visible text as `tickhost::screen::ScreenSnapshot` takes it, newest
/// lines kept when the snapshot limits cut it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostedScreenSnapshot {
    pub lines: Vec<String>,
    pub truncated: bool,
    pub rows: u16,
    pub columns: u16,
    pub alternate_screen: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HerdrInteraction {
    pub socket: PathBuf,
    pub pane: HerdrPane,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostCallContext {
    pub workdir: PathBuf,
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
