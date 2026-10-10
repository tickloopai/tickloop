use super::*;
use crate::{
    Agent, ShellTaskCenter,
    tick_stream::{TickConnectionState, TickMetadata, TickObservation},
};
use std::time::Duration;
use tickloop_agent_protocol::{HostToolAuthority, ObservationKind, ShellTask};

const OUTPUT_SUMMARY_BYTES: usize = 4096;
const SHELL_OBSERVATION_INTERVAL: Duration = Duration::from_millis(250);
/// A Shell Tick updates once, when it exits.
const EXIT_SEQUENCE: u64 = 1;

pub struct ShellTickStream {
    agent: crate::WeakAgent,
    chats: watch::Sender<BTreeMap<WatchId, Uuid>>,
    authorities: watch::Sender<BTreeMap<WatchId, HostToolAuthority>>,
}
impl ShellTickStream {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent: agent.downgrade(),
            chats: watch::channel(BTreeMap::new()).0,
            authorities: watch::channel(BTreeMap::new()).0,
        }
    }
}
struct ShellWatchValidator {
    agent: crate::WeakAgent,
    task: Uuid,
}
#[async_trait::async_trait]
impl crate::tick_stream::TickWatchValidator for ShellWatchValidator {
    async fn revalidate(
        &self,
        chat: Uuid,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("Shell subscription cancelled before admission".into());
        }
        let agent = self.agent.upgrade().ok_or("Shell runtime is unavailable")?;
        let snapshot = ShellTaskCenter::new(agent)
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        if !snapshot
            .tasks
            .iter()
            .any(|task| task.id == self.task && task.chat_id == chat)
        {
            return Err("Shell Tick is unavailable in this Chat".into());
        }
        Ok(())
    }
}
/// A watch's baseline: the task it follows and whether that task had already ended or has ended
/// since, after which the watch has nothing more to publish.
struct Cursor {
    task: Uuid,
    has_ended: bool,
}

/// When the task ended, which is its only update; `None` while it runs.
fn ended_at_ms(task: &ShellTask) -> Option<u64> {
    task.state
        .is_terminal()
        .then(|| task.ended_at_ms.or(task.updated_at_ms))
        .flatten()
}
#[async_trait::async_trait]
impl TickStreamPort for ShellTickStream {
    fn set_watch_authorities(&self, authorities: BTreeMap<WatchId, HostToolAuthority>) {
        self.authorities.send_replace(authorities);
    }
    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        use crate::tick_stream::TickWatchValidator;
        if authority.is_some() || instance.tick_type() != "shell" {
            return Err("Shell watch requires its local task/Chat scope".into());
        }
        ShellWatchValidator {
            agent: self.agent.clone(),
            task: Uuid::parse_str(instance.id()).map_err(|error| error.to_string())?,
        }
        .revalidate(chat, tokio_util::sync::CancellationToken::new())
        .await
    }
    async fn prepare_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        input: &serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<crate::tick_stream::PreparedTickWatch>, String> {
        use crate::tick_stream::TickWatchValidator;
        let validator = Arc::new(ShellWatchValidator {
            agent: self.agent.clone(),
            task: Uuid::parse_str(instance.id()).map_err(|error| error.to_string())?,
        });
        validator.revalidate(chat, cancel).await?;
        let admission = crate::host_tools::PreparedHostCall::new(
            "SubscribeTick".into(),
            input.clone(),
            input.clone(),
            crate::host_tools::HostToolPermission::ReadOnly,
            format!("Observe Shell task {} in the current Chat", validator.task),
        );
        Ok(Some(crate::tick_stream::PreparedTickWatch::with_validator(
            admission, validator,
        )))
    }

    fn set_watch_chats(&self, chats: BTreeMap<WatchId, Uuid>) {
        self.chats.send_replace(chats);
    }
    fn open(&self) -> TickStreamHandle {
        let (desired, mut receiver) = watch::channel(BTreeMap::<WatchId, InstanceRef>::new());
        let (publisher, events) = EventPublisher::channel();
        let weak = self.agent.clone();
        let mut owners = self.chats.subscribe();
        let mut authorities = self.authorities.subscribe();
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
                    targets.get(watch) == Some(&InstanceRef::shell(cursor.task))
                });
                let active = targets
                    .iter()
                    .filter(|(watch, _)| !cursors.get(watch).is_some_and(|cursor| cursor.has_ended))
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
                                    if owners.borrow().get(watch) != Some(&task.chat_id)
                                        || authorities.borrow().contains_key(watch)
                                    {
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
                                            updated_at_ms: ended_at_ms(task),
                                        },
                                    );
                                    publisher.state_to(
                                        instance.clone(),
                                        TickConnectionState::Live,
                                        [*watch],
                                    );
                                    if let Some(cursor) = cursors.get_mut(watch) {
                                        if task.state.is_terminal() {
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
                                            if owners.borrow().get(watch) != Some(&task.chat_id)
                                                || authorities.borrow().contains_key(watch)
                                            {
                                                publisher.state_to(
                                                    instance.clone(),
                                                    TickConnectionState::IdentityUncertain,
                                                    [*watch],
                                                );
                                                continue;
                                            }
                                            // Read the authoritative tail before ending the watch; a failed read remains retryable.
                                            if task.capabilities.can_read_output && output.is_none()
                                            {
                                                continue;
                                            }
                                            cursor.has_ended = true;
                                            publisher.observation(&[*watch], TickObservation {
                                                instance: instance.clone(), sequence: EXIT_SEQUENCE,
                                                observed_at: ended_at_ms(task).map(tickloop_agent_base::timestamp::format_timestamp).unwrap_or_else(tickloop_agent_base::timestamp::now_timestamp), dropped: 0,
                                                data: serde_json::json!({"task_id":task.id,"status":task.status_label(),"state":task.state,"exit_code":task.exit_code,"signal":task.signal,"output_bytes":task.output_bytes,"output":output,"truncated":task.output_bytes > OUTPUT_SUMMARY_BYTES as u64}),
                                                kind: ObservationKind::Protocol, recover_with: Some("Read the output_file returned by Bash with Read; use the Shell detail for user commands".into()), gap: false,
                                            });
                                        }
                                    } else {
                                        cursors.insert(
                                            *watch,
                                            Cursor {
                                                task: task.id,
                                                has_ended: task.state.is_terminal(),
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
                    changed = authorities.changed() => if changed.is_err() { return; },
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
