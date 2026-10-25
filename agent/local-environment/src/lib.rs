//! Tickloop Code's local Execution Environment and Tickhost lifecycle.
pub mod environment;
pub mod hosted;
mod paths;
use tickloop_harness::runtime::{host_tools, tick_tools};

/// Local composition helpers for embedded Agents and isolated integration hosts.
pub trait AgentLocalExt {
    fn install_shell_grants(
        &self,
        grants: std::sync::Arc<dyn hosted::shells::ShellGrants>,
    ) -> std::sync::Arc<hosted::shells::ShellTasks>;
    fn install_own_tickhost(&self) -> std::sync::Arc<hosted::shells::ShellTasks>;
}
impl AgentLocalExt for tickloop_harness::runtime::Agent {
    fn install_shell_grants(
        &self,
        grants: std::sync::Arc<dyn hosted::shells::ShellGrants>,
    ) -> std::sync::Arc<hosted::shells::ShellTasks> {
        let tasks = hosted::shells::ShellTasks::new(grants);
        self.install_shell_runtime(tasks.clone());
        tasks
    }
    fn install_own_tickhost(&self) -> std::sync::Arc<hosted::shells::ShellTasks> {
        self.install_shell_grants(std::sync::Arc::new(hosted::shells::OwnTickhost::new()))
    }
}
