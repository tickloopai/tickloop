//! The worker's side of the Chat ends that end hosted Ticks: retention deleting an expired
//! Transcript, and StopSubagent stopping a direct child. Both reach the daemon over the worker-control
//! channel; the daemon checks each claim itself before it ends anything.

use tokio::runtime::Handle;
use uuid::Uuid;

use super::worker_control::{DaemonControl, WorkerControlGrant, WorkerRequest};
use crate::{cleanup::ChatDeletionNotice, runtime::StoppedChildHook};

pub struct DaemonLifecycle {
    control: DaemonControl,
    /// The worker's runtime: cleanup runs on its own thread and blocks on daemon answers.
    runtime: Handle,
}

impl DaemonLifecycle {
    /// Must be created inside the worker's Tokio runtime.
    pub fn new(grant: WorkerControlGrant) -> Self {
        Self {
            control: DaemonControl::new(grant),
            runtime: Handle::current(),
        }
    }
}

impl ChatDeletionNotice for DaemonLifecycle {
    fn begin(&self, chat: Uuid) -> Result<Uuid, String> {
        let operation = Uuid::new_v4();
        self.runtime
            .block_on(
                self.control
                    .request(WorkerRequest::BeginChatDeletion { chat, operation }),
            )
            .map(|_| operation)
            .map_err(|error| error.message)
    }

    fn finish(&self, chat: Uuid, operation: Uuid) {
        // A lost answer changes nothing: the daemon settles the deletion itself once this
        // cleanup releases the Chat's retention lock.
        let _ = self.runtime.block_on(
            self.control
                .request(WorkerRequest::FinishChatDeletion { chat, operation }),
        );
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
