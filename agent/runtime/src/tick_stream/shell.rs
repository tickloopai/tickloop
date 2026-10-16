//! Tick Updates of Shell tasks, which run in the Tickhost. A Shell Tick updates once, when its
//! task ends, and that update is always the end this Agent recorded for the task, however the
//! Tickhost reported it. A watch that joins a task that already ended takes that end as its
//! baseline. Watches stay in the Chat whose Shell tasks list the task, and need no host authority
//! of their own.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use serde_json::json;
use tickhost::shell::OUTPUT_TAIL_BYTES;
use tokio::sync::watch;
use uuid::Uuid;

use super::{
    ConnectionBudget, EventPublisher, InstanceObserver, InstanceRef, PreparedTickWatch,
    RECONNECT_PAUSE, TickConnectionState, TickMetadata, TickObservation, TickStreamHandle,
    TickStreamPort, WatchAdmission, WatchId, follow_instances,
};
use crate::{Agent, ShellTaskCenter, WeakAgent};
use tickloop_agent_protocol::{HostToolAuthority, ObservationKind, ShellTask};

const RECOVER_WITH: &str =
    "Read the output_file returned by Bash with Read; a user command's output shows in its view";
/// The one update of a Shell Tick: its end.
const EXIT_SEQUENCE: u64 = 1;

pub struct ShellTickStream {
    agent: WeakAgent,
    chats: watch::Sender<BTreeMap<WatchId, Uuid>>,
    authorities: watch::Sender<BTreeMap<WatchId, HostToolAuthority>>,
    budget: ConnectionBudget,
}

impl ShellTickStream {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent: agent.downgrade(),
            chats: watch::channel(BTreeMap::new()).0,
            authorities: watch::channel(BTreeMap::new()).0,
            budget: ConnectionBudget::default(),
        }
    }

    fn scope(&self) -> ShellScope {
        ShellScope {
            agent: self.agent.clone(),
            chats: self.chats.subscribe(),
            authorities: self.authorities.subscribe(),
            budget: self.budget.clone(),
        }
    }
}

fn task_of(instance: &InstanceRef) -> Option<Uuid> {
    (instance.access() == tick_protocol::SHELL_ACCESS)
        .then(|| Uuid::parse_str(instance.id()).ok())
        .flatten()
}

#[derive(Clone)]
struct ShellScope {
    agent: WeakAgent,
    chats: watch::Receiver<BTreeMap<WatchId, Uuid>>,
    authorities: watch::Receiver<BTreeMap<WatchId, HostToolAuthority>>,
    budget: ConnectionBudget,
}

impl ShellScope {
    fn center(&self) -> Result<ShellTaskCenter, String> {
        let agent = self.agent.upgrade().ok_or("Shell runtime is unavailable")?;
        Ok(ShellTaskCenter::new(agent))
    }

    /// The task as the Shell tasks of `chat` list it.
    async fn task(&self, chat: Uuid, task: Uuid) -> Result<Option<ShellTask>, String> {
        let snapshot = self
            .center()?
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        Ok(snapshot.tasks.into_iter().find(|listed| listed.id == task))
    }

    /// A Shell watch holds only in the Chat whose task it watches, and without a host authority.
    async fn check(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        if authority.is_some() {
            return Err("Shell watch requires its local task/Chat scope".into());
        }
        let task = task_of(instance).ok_or("Shell watch requires a Shell task")?;
        match self.task(chat, task).await? {
            Some(listed) if listed.chat_id == chat => Ok(()),
            _ => Err("Shell Tick is unavailable in this Chat".into()),
        }
    }

    /// The end of the task of `instance` as its record tells it, with the end of its output when
    /// that can be read.
    async fn recorded_end(&self, instance: &InstanceRef, listed: &ShellTask) -> TickObservation {
        let output = match (listed.capabilities.can_read_output, self.center()) {
            (true, Ok(center)) => center
                .read_output(
                    listed.chat_id,
                    listed.id,
                    listed.output_bytes.saturating_sub(OUTPUT_TAIL_BYTES as u64),
                    OUTPUT_TAIL_BYTES,
                )
                .await
                .ok()
                .map(|chunk| chunk.output),
            _ => None,
        };
        TickObservation {
            instance: instance.clone(),
            sequence: EXIT_SEQUENCE,
            observed_at: ended_at_ms(listed).map_or_else(
                tickloop_agent_base::timestamp::now_timestamp,
                tickloop_agent_base::timestamp::format_timestamp,
            ),
            dropped: 0,
            data: json!({
                "task_id": listed.id,
                "status": listed.status_label(),
                "state": listed.state,
                "exit_code": listed.exit_code,
                "signal": listed.signal,
                "reason": listed.reason,
                "output_bytes": listed.output_bytes,
                "output": output,
                "truncated": listed.output_bytes > OUTPUT_TAIL_BYTES as u64,
            }),
            kind: ObservationKind::Protocol,
            recover_with: Some(RECOVER_WITH.into()),
            gap: false,
        }
    }

    fn chat_of(&self, watch: &WatchId) -> Option<Uuid> {
        self.chats.borrow().get(watch).copied()
    }

    /// The admitted watches of the Chat that owns the task; any other is told its identity is
    /// uncertain.
    fn owned(
        &self,
        requested: &BTreeSet<WatchId>,
        owner: Uuid,
        instance: &InstanceRef,
        publisher: &EventPublisher,
    ) -> BTreeSet<WatchId> {
        let mut owned = self.admitted(requested, instance, publisher);
        owned.retain(|watch| {
            let is_owned = self.chat_of(watch) == Some(owner);
            if !is_owned {
                publisher.state_to(
                    instance.clone(),
                    TickConnectionState::IdentityUncertain,
                    [*watch],
                );
            }
            is_owned
        });
        owned
    }
}

impl WatchAdmission for ShellScope {
    fn admitted(
        &self,
        requested: &BTreeSet<WatchId>,
        instance: &InstanceRef,
        publisher: &EventPublisher,
    ) -> BTreeSet<WatchId> {
        let authorities = self.authorities.borrow();
        requested
            .iter()
            .copied()
            .filter(|watch| {
                let is_local = self.chat_of(watch).is_some() && !authorities.contains_key(watch);
                if !is_local {
                    publisher.state_to(
                        instance.clone(),
                        TickConnectionState::IdentityUncertain,
                        [*watch],
                    );
                }
                is_local
            })
            .filter(|watch| self.budget.admit_or_disconnect(*watch, instance, publisher))
            .collect()
    }
}

#[async_trait::async_trait]
impl InstanceObserver for ShellScope {
    fn budget(&self) -> &ConnectionBudget {
        &self.budget
    }

    async fn authorities_changed(&mut self) -> bool {
        tokio::select! {
            changed = self.authorities.changed() => changed.is_ok(),
            changed = self.chats.changed() => changed.is_ok(),
        }
    }

    async fn observe(
        self,
        instance: InstanceRef,
        wanted: watch::Receiver<BTreeSet<WatchId>>,
        publisher: EventPublisher,
    ) {
        let (Some(task), Some(agent)) = (task_of(&instance), self.agent.upgrade()) else {
            return;
        };
        let events = TaskEvents {
            agent: agent.subscribe(),
            user_shells: agent.subscribe_user_shells(),
        };
        drop(agent);
        ShellObserver {
            scope: self,
            task,
            instance,
            wanted,
            publisher,
            ready: BTreeSet::new(),
            awaiting_exit: BTreeSet::new(),
        }
        .follow(events)
        .await;
    }
}

/// What changes the record of a task: the Agent records its commands and its user's `!` commands.
struct TaskEvents {
    agent: tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    user_shells: tokio::sync::broadcast::Receiver<tickloop_agent_protocol::UserShellRecord>,
}

impl TaskEvents {
    /// Waits for the next change; false once none can come.
    async fn changed(&mut self) -> bool {
        use tokio::sync::broadcast::error::RecvError;
        tokio::select! {
            event = self.agent.recv() => !matches!(event, Err(RecvError::Closed)),
            event = self.user_shells.recv() => !matches!(event, Err(RecvError::Closed)),
        }
    }
}

/// Follows one task for the watches that want it.
struct ShellObserver {
    scope: ShellScope,
    task: Uuid,
    instance: InstanceRef,
    wanted: watch::Receiver<BTreeSet<WatchId>>,
    publisher: EventPublisher,
    /// Watches given their baseline.
    ready: BTreeSet<WatchId>,
    /// Watches whose baseline was a task that had not ended: they get its end once.
    awaiting_exit: BTreeSet<WatchId>,
}

impl ShellObserver {
    async fn follow(mut self, mut events: TaskEvents) {
        loop {
            let requested = self.wanted.borrow_and_update().clone();
            let admitted = self
                .scope
                .admitted(&requested, &self.instance, &self.publisher);
            let Some(chat) = admitted.iter().find_map(|watch| self.scope.chat_of(watch)) else {
                return;
            };
            let listed = match self.scope.task(chat, self.task).await {
                Ok(Some(listed)) => listed,
                Ok(None) => {
                    self.publisher.state_to(
                        self.instance.clone(),
                        TickConnectionState::Gone,
                        admitted.iter().copied(),
                    );
                    return;
                }
                Err(_) => {
                    self.publisher.state_to(
                        self.instance.clone(),
                        TickConnectionState::Disconnected,
                        admitted.iter().copied(),
                    );
                    if !self.pause().await {
                        return;
                    }
                    continue;
                }
            };
            let owned =
                self.scope
                    .owned(&admitted, listed.chat_id, &self.instance, &self.publisher);
            self.ready.retain(|watch| owned.contains(watch));
            self.awaiting_exit.retain(|watch| owned.contains(watch));
            if owned.is_empty() {
                if self.wanted.changed().await.is_err() {
                    return;
                }
                continue;
            }
            let is_ended = listed.state.is_terminal();
            if is_ended && !self.awaiting_exit.is_empty() {
                let ended = self.scope.recorded_end(&self.instance, &listed).await;
                let awaiting = std::mem::take(&mut self.awaiting_exit);
                self.publisher
                    .observation(&awaiting.into_iter().collect::<Vec<_>>(), ended);
            }
            self.baseline(&owned, &listed, is_ended);
            let changed = if is_ended {
                self.wanted.changed().await.is_ok()
            } else {
                tokio::select! {
                    changed = self.wanted.changed() => changed.is_ok(),
                    changed = events.changed() => changed,
                }
            };
            if !changed {
                return;
            }
        }
    }

    /// Waits before connecting again; false once nobody wants the task.
    async fn pause(&mut self) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_PAUSE) => true,
            changed = self.wanted.changed() => changed.is_ok(),
        }
    }

    fn metadata(listed: &ShellTask, ended_at_ms: Option<u64>) -> TickMetadata {
        TickMetadata {
            title: listed.command.lines().next().unwrap_or_default().into(),
            kind: ObservationKind::Protocol,
            updated_at_ms: ended_at_ms,
            series: Vec::new(),
        }
    }

    /// Gives the watches that have none yet the task as it is now as their baseline.
    fn baseline(&mut self, owned: &BTreeSet<WatchId>, listed: &ShellTask, is_ended: bool) {
        self.publisher.metadata(
            self.instance.clone(),
            owned.iter().copied(),
            Self::metadata(listed, ended_at_ms(listed)),
        );
        self.publisher.state_to(
            self.instance.clone(),
            TickConnectionState::Live,
            owned.iter().copied(),
        );
        for watch in owned.difference(&self.ready).copied().collect::<Vec<_>>() {
            self.publisher.ready(watch);
            self.ready.insert(watch);
            if !is_ended {
                self.awaiting_exit.insert(watch);
            }
        }
    }
}

/// When the task ended, which is its only update; `None` while it has not.
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

    fn set_watch_chats(&self, chats: BTreeMap<WatchId, Uuid>) {
        self.chats.send_replace(chats);
    }

    async fn revalidate_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        authority: Option<&HostToolAuthority>,
    ) -> Result<(), String> {
        self.scope().check(chat, instance, authority).await
    }

    /// The task's one update, its end, read again for a delivery; `None` while it runs.
    async fn read_current(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<Option<TickObservation>, String> {
        let scope = self.scope();
        let task = task_of(instance).ok_or("Shell read requires a Shell task")?;
        match scope.task(chat, task).await? {
            Some(listed) if listed.chat_id == chat && listed.state.is_terminal() => {
                Ok(Some(scope.recorded_end(instance, &listed).await))
            }
            _ => Ok(None),
        }
    }

    async fn prepare_watch(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
        input: &serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<PreparedTickWatch>, String> {
        use super::TickWatchValidator;
        let validator = Arc::new(ShellWatchValidator {
            scope: self.scope(),
            instance: instance.clone(),
        });
        validator.revalidate(chat, cancel).await?;
        let task = task_of(instance).ok_or("Shell watch requires a Shell task")?;
        let admission = crate::host_tools::PreparedHostCall::new(
            "TickManage".into(),
            input.clone(),
            input.clone(),
            crate::host_tools::HostToolPermission::ReadOnly,
            format!("Observe Shell task {task} in the current Chat"),
        );
        Ok(Some(PreparedTickWatch::with_validator(
            admission, validator,
        )))
    }

    fn open(&self) -> TickStreamHandle {
        let (desired, wanted) = watch::channel(BTreeMap::new());
        let (publisher, events) = EventPublisher::channel();
        let scope = self.scope();
        tokio::spawn(async move { follow_instances(scope, wanted, publisher).await });
        TickStreamHandle { desired, events }
    }
}

struct ShellWatchValidator {
    scope: ShellScope,
    instance: InstanceRef,
}

#[async_trait::async_trait]
impl super::TickWatchValidator for ShellWatchValidator {
    async fn revalidate(
        &self,
        chat: Uuid,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("Shell subscription cancelled before admission".into());
        }
        self.scope.check(chat, &self.instance, None).await
    }
}
