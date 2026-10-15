use super::*;
use crate::{
    Agent,
    tick_stream::{HostedTickStream, ShellTickStream},
};

/// Ticks a Chat opened in the dedicated Tickhost; only a worker with a Tickhost grant follows
/// them.
pub struct HostedAdapter {
    stream: Option<Arc<dyn TickStreamPort>>,
}
impl HostedAdapter {
    pub fn new(tools: Option<Arc<crate::hosted::tools::HostedTickTools>>) -> Self {
        Self {
            stream: tools
                .map(|tools| Arc::new(HostedTickStream::new(tools)) as Arc<dyn TickStreamPort>),
        }
    }
}
impl TickAdapter for HostedAdapter {
    fn access(&self) -> &'static str {
        tick_protocol::HOSTED_ACCESS
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        self.stream.clone()
    }
}

/// Shell tasks, which run in the Tickhost; every Agent follows its own.
pub struct ShellAdapter {
    stream: Arc<dyn TickStreamPort>,
}
impl ShellAdapter {
    pub fn new(agent: Agent) -> Self {
        Self {
            stream: Arc::new(ShellTickStream::new(agent)),
        }
    }
}
impl TickAdapter for ShellAdapter {
    fn access(&self) -> &'static str {
        tick_protocol::SHELL_ACCESS
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        Some(self.stream.clone())
    }
}
