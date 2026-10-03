use super::*;
use crate::{
    runtime::{Agent, ShellTask, ShellTaskCenter},
    tick_stream::{ObservationKind, TickConnectionState, TickMetadata, TickObservation},
};
use std::time::Duration;

const OUTPUT_SUMMARY_BYTES: usize = 4096;
const SHELL_OBSERVATION_INTERVAL: Duration = Duration::from_millis(250);

pub struct ShellTickStream {
    agent: crate::runtime::WeakAgent,
    chats: watch::Sender<BTreeMap<WatchId, Uuid>>,
}
impl ShellTickStream {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent: agent.downgrade(),
            chats: watch::channel(BTreeMap::new()).0,
        }
    }
}
struct Cursor {
    task: ShellTask,
    sequence: u64,
}
impl TickStreamPort for ShellTickStream {
    fn set_watch_chats(&self, chats: BTreeMap<WatchId, Uuid>) {
        self.chats.send_replace(chats);
    }
    fn open(&self) -> TickStreamHandle {
        let (desired, mut receiver) = watch::channel(BTreeMap::<WatchId, InstanceRef>::new());
        let (publisher, events) = EventPublisher::channel();
        let weak = self.agent.clone();
        let mut owners = self.chats.subscribe();
        tokio::spawn(async move {
            let Some(agent) = weak.upgrade() else { return };
            let shutdown = agent.shutdown_token();
            let mut agent_events = agent.subscribe();
            let mut user_events = agent.subscribe_user_shells();
            drop(agent);
            let mut cursors: BTreeMap<WatchId, Cursor> = BTreeMap::new();
            loop {
                let targets = receiver.borrow_and_update().clone();
                publisher.retain(&targets);
                cursors.retain(|watch, cursor| {
                    targets.get(watch) == Some(&InstanceRef::shell(cursor.task.id))
                });
                let active = targets
                    .iter()
                    .filter(|(watch, _)| {
                        !cursors
                            .get(watch)
                            .is_some_and(|cursor| cursor.task.state.is_terminal())
                    })
                    .collect::<Vec<_>>();
                if !active.is_empty() {
                    let Some(agent) = weak.upgrade() else { return };
                    let center = ShellTaskCenter::new(agent.clone());
                    let chats = agent.list_chats().await;
                    if let Some(chat) = chats.first() {
                        match center.snapshot(chat.id).await {
                            Ok(snapshot) => {
                                for (watch, instance) in active {
                                    let Some(task) = snapshot
                                        .tasks
                                        .iter()
                                        .find(|task| InstanceRef::shell(task.id) == *instance)
                                    else {
                                        publisher.state_to(
                                            instance.clone(),
                                            TickConnectionState::Gone,
                                            [*watch],
                                        );
                                        continue;
                                    };
                                    if owners.borrow().get(watch) != Some(&task.chat_id) {
                                        publisher.state_to(
                                            instance.clone(),
                                            TickConnectionState::IdentityUncertain,
                                            [*watch],
                                        );
                                        continue;
                                    }
                                    publisher.metadata(
                                        instance.clone(),
                                        [*watch],
                                        TickMetadata {
                                            title: task
                                                .command
                                                .lines()
                                                .next()
                                                .unwrap_or_default()
                                                .into(),
                                            kind: ObservationKind::Protocol,
                                            updated_at_ms: task.updated_at_ms,
                                        },
                                    );
                                    publisher.state_to(
                                        instance.clone(),
                                        TickConnectionState::Live,
                                        [*watch],
                                    );
                                    if let Some(cursor) = cursors.get_mut(watch) {
                                        if (
                                            cursor.task.state,
                                            cursor.task.output_bytes,
                                            cursor.task.exit_code,
                                            cursor.task.signal,
                                            &cursor.task.reason,
                                        ) != (
                                            task.state,
                                            task.output_bytes,
                                            task.exit_code,
                                            task.signal,
                                            &task.reason,
                                        ) {
                                            let output = if task.capabilities.can_read_output {
                                                center
                                                    .read_output(
                                                        task.chat_id,
                                                        task.id,
                                                        task.output_bytes.saturating_sub(
                                                            OUTPUT_SUMMARY_BYTES as u64,
                                                        ),
                                                        OUTPUT_SUMMARY_BYTES,
                                                    )
                                                    .await
                                                    .ok()
                                                    .map(|chunk| chunk.output)
                                            } else {
                                                None
                                            };
                                            // Read the authoritative tail before advancing; a failed read remains retryable.
                                            if task.capabilities.can_read_output && output.is_none()
                                            {
                                                continue;
                                            }
                                            cursor.sequence = cursor.sequence.saturating_add(1);
                                            cursor.task = task.clone();
                                            publisher.observation(&[*watch], TickObservation {
                                                instance: instance.clone(), sequence: cursor.sequence,
                                                observed_at: task.updated_at_ms.map(crate::transcript::format_timestamp).unwrap_or_else(crate::transcript::now_timestamp), dropped: 0,
                                                data: serde_json::json!({"task_id":task.id,"status":shell_status(task),"state":task.state,"exit_code":task.exit_code,"signal":task.signal,"output_bytes":task.output_bytes,"output":output,"truncated":task.output_bytes > OUTPUT_SUMMARY_BYTES as u64}),
                                                kind: ObservationKind::Protocol, recover_with: Some("output".into()), gap: false,
                                            });
                                        }
                                    } else {
                                        cursors.insert(
                                            *watch,
                                            Cursor {
                                                task: task.clone(),
                                                sequence: 0,
                                            },
                                        );
                                        publisher.ready(*watch);
                                    }
                                }
                            }
                            Err(_) => {
                                for (watch, instance) in active {
                                    publisher.state_to(
                                        instance.clone(),
                                        TickConnectionState::Disconnected,
                                        [*watch],
                                    );
                                }
                            }
                        }
                    } else {
                        for (watch, instance) in active {
                            publisher.state_to(
                                instance.clone(),
                                TickConnectionState::Gone,
                                [*watch],
                            );
                        }
                    }
                }
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => if changed.is_err() { return },
                    changed = owners.changed() => if changed.is_err() { return },
                    _ = agent_events.recv(), if !targets.is_empty() => {},
                    _ = user_events.recv(), if !targets.is_empty() => {},
                    _ = tokio::time::sleep(SHELL_OBSERVATION_INTERVAL), if !targets.is_empty() => {},
                }
            }
        });
        TickStreamHandle { desired, events }
    }
}
