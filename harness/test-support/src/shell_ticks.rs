//! A Shell Tick on an in-memory Tick stream: a Trigger source a test updates at will. A Shell
//! Tick Subscription is immediate, so each update reaches the Loop without waiting for a period.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tick_protocol::InstanceRef;
use tickloop_harness_protocol::{ChatId, ObservationKind, TickLinkState};
use tickloop_harness_runtime::Agent;
use tickloop_harness_runtime::tick_stream::{InMemoryTickStream, TickObservation};
use uuid::Uuid;

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// One Shell Tick as one Agent watches it; each Agent gets its own stream, and a Tick of the same
/// `task` on each.
pub struct ShellTick {
    pub stream: InMemoryTickStream,
    pub instance: InstanceRef,
    task: Uuid,
}

impl ShellTick {
    /// The Shell Tick of `task` on a stream of its own.
    pub fn new(task: Uuid) -> Self {
        Self::on(InMemoryTickStream::new(), task)
    }

    /// The Shell Tick of `task` on `stream`, which other Ticks of the same Agent share.
    pub fn on(stream: InMemoryTickStream, task: Uuid) -> Self {
        Self {
            stream,
            instance: InstanceRef::shell(task),
            task,
        }
    }

    /// Installs this Tick's stream as `agent`'s Tick stream, once per Agent.
    pub async fn install(&self, agent: &Agent) {
        agent
            .install_tick_stream(Arc::new(self.stream.clone()))
            .await;
    }

    /// Subscribes the Chat's Loop as its user would, then waits until the watch is live.
    pub async fn subscribe(&self, agent: &Agent, chat: ChatId) {
        agent
            .tick_subscribe_for_user(chat, self.instance.clone())
            .await
            .unwrap();
        self.wait_until_live(agent, chat).await;
    }

    /// Waits until the Chat's subscription to this Tick watches it.
    pub async fn wait_until_live(&self, agent: &Agent, chat: ChatId) {
        tokio::time::timeout(READY_TIMEOUT, async {
            loop {
                let live = agent
                    .tick_subscriptions(chat)
                    .await
                    .unwrap()
                    .iter()
                    .any(|status| {
                        status.instance == self.instance && status.link == TickLinkState::Live
                    });
                if live {
                    return;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .expect("the Shell Tick's watch becomes live");
    }

    /// Pushes the update `sequence`, whose data carries `note`.
    pub fn update(&self, sequence: u64, note: &str) {
        self.stream.push(TickObservation {
            instance: self.instance.clone(),
            sequence,
            observed_at: "2026-10-22T00:00:00Z".into(),
            dropped: 0,
            data: json!({ "task_id": self.task, "note": note }),
            kind: ObservationKind::Protocol,
            recover_with: None,
            gap: false,
        });
    }
}
