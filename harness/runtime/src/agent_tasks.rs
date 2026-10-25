//! The Agent's background tasks. `shutdown` waits for every one of them; a Worker that handed
//! its family to an upgraded successor aborts them instead, so none acts for the old owner.

use std::{
    future::Future,
    sync::{Arc, Mutex, PoisonError},
};

use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::task::{TaskTracker, task_tracker::TaskTrackerToken};

#[derive(Clone, Default)]
pub(crate) struct AgentTasks {
    tracker: TaskTracker,
    aborts: Arc<Mutex<Vec<AbortHandle>>>,
}

impl AgentTasks {
    pub(crate) fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.remember(self.tracker.spawn(task))
    }

    pub(crate) fn spawn_on<F>(
        &self,
        task: F,
        runtime: &tokio::runtime::Handle,
    ) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.remember(self.tracker.spawn_on(task, runtime))
    }

    /// Held while a caller is about to spawn: `wait` does not end before it is dropped.
    pub(crate) fn token(&self) -> TaskTrackerToken {
        self.tracker.token()
    }

    /// The tracker a Tick stream spawns its own tasks on; they end when the Agent shuts down.
    pub(crate) fn tracker(&self) -> TaskTracker {
        self.tracker.clone()
    }

    pub(crate) fn close(&self) -> bool {
        self.tracker.close()
    }

    pub(crate) async fn wait(&self) {
        self.tracker.wait().await;
    }

    /// Aborts every task spawned here that has not ended.
    pub(crate) fn abort_all(&self) {
        for task in std::mem::take(&mut *self.lock()) {
            task.abort();
        }
    }

    fn remember<T>(&self, task: JoinHandle<T>) -> JoinHandle<T> {
        let mut aborts = self.lock();
        aborts.retain(|task| !task.is_finished());
        aborts.push(task.abort_handle());
        task
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<AbortHandle>> {
        self.aborts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
