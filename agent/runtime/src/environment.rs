//! Where a Chat's Folder, commands and Ticks live: its Execution Environment.

use std::sync::Arc;

use crate::hosted::{shells::ShellGrants, tools::HostedTickTools};

/// Where the Agent runs commands and Ticks for Chats with a Folder. A Harness without one has
/// Chats without a Folder: they touch no local file and read no local process state.
pub trait ExecutionEnvironment: Send + Sync {
    /// Who grants Bash its spawns; `None` refuses every spawn.
    fn shell_grants(&self) -> Option<Arc<dyn ShellGrants>>;
    /// The local Tickhost's Ticks; `None` leaves the Tick tools out.
    fn hosted_ticks(&self) -> Option<Arc<HostedTickTools>>;
}

/// This computer: the file tools and project Skills work on the Chat's local Folder, and the
/// daemon's Tickhost, when it granted one, runs commands and Ticks.
pub struct LocalEnvironment {
    shell_grants: Option<Arc<dyn ShellGrants>>,
    hosted_ticks: Option<Arc<HostedTickTools>>,
}

impl LocalEnvironment {
    /// Local files without a Tickhost: no command runs and no Tick opens.
    pub fn bare() -> Self {
        Self {
            shell_grants: None,
            hosted_ticks: None,
        }
    }

    /// Local files with the commands and Ticks of the Tickhost a daemon granted.
    pub fn with(shell_grants: Arc<dyn ShellGrants>, hosted_ticks: Arc<HostedTickTools>) -> Self {
        Self {
            shell_grants: Some(shell_grants),
            hosted_ticks: Some(hosted_ticks),
        }
    }
}

impl ExecutionEnvironment for LocalEnvironment {
    fn shell_grants(&self) -> Option<Arc<dyn ShellGrants>> {
        self.shell_grants.clone()
    }

    fn hosted_ticks(&self) -> Option<Arc<HostedTickTools>> {
        self.hosted_ticks.clone()
    }
}
