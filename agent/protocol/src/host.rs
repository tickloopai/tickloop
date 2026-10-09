//! Records the Agent wire carries from its native hosts. Each one has the fields and serde shape
//! of the host type it mirrors, so a change inside Herdr or the Tickhost reaches the Agent wire
//! only through an explicit conversion in the runtime.

use std::collections::BTreeMap;

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
