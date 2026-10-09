use super::*;
use crate::{
    Agent,
    tick_stream::{ArtifactTickStream, HostedTickStream, NativeTickStream},
};
use std::path::PathBuf;

pub struct HerdrAdapter {
    stream: Option<Arc<dyn TickStreamPort>>,
}
impl HerdrAdapter {
    pub fn new(provider: Option<Arc<crate::native_host::NativeHostToolProvider>>) -> Self {
        Self {
            stream: provider.map(|provider| {
                Arc::new(NativeTickStream::new(provider)) as Arc<dyn TickStreamPort>
            }),
        }
    }
}
impl TickAdapter for HerdrAdapter {
    fn tick_type(&self) -> &'static str {
        tick_protocol::HERDR_INSTANCE_TYPE
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        self.stream.clone()
    }
}

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
    fn tick_type(&self) -> &'static str {
        tick_protocol::HOSTED_INSTANCE_TYPE
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        self.stream.clone()
    }
}

pub struct ArtifactAdapter {
    stream: Option<Arc<dyn TickStreamPort>>,
}
impl ArtifactAdapter {
    pub fn new(root: Option<PathBuf>) -> Self {
        let stream =
            root.map(|root| Arc::new(ArtifactTickStream::new(root)) as Arc<dyn TickStreamPort>);
        Self { stream }
    }
}
impl TickAdapter for ArtifactAdapter {
    fn tick_type(&self) -> &'static str {
        "artifact"
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        self.stream.clone()
    }
}

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
    fn tick_type(&self) -> &'static str {
        "shell"
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        Some(self.stream.clone())
    }
}
