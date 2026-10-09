//! The worker's side of the Chat end that ends hosted Ticks: StopSubagent stopping a direct child.
//! It reaches the daemon over the worker-control channel; the daemon checks the claim itself
//! before it ends anything.

use uuid::Uuid;

use super::worker_control::{DaemonControl, WorkerControlGrant, WorkerRequest};
use crate::StoppedChildHook;

pub struct DaemonLifecycle {
    control: DaemonControl,
}

impl DaemonLifecycle {
    pub fn new(grant: WorkerControlGrant) -> Self {
        Self {
            control: DaemonControl::new(grant),
        }
    }
}

#[async_trait::async_trait]
impl StoppedChildHook for DaemonLifecycle {
    async fn child_stopped(&self, root: Uuid, child: Uuid) -> Result<(), String> {
        self.control
            .request(WorkerRequest::EndStoppedChild {
                chat: root,
                child,
                operation: Uuid::new_v4(),
            })
            .await
            .map(|_| ())
            .map_err(|error| error.message)
    }
}
