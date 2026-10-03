use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

use super::execution::ConsoleExecution;
use crate::{
    agent_access::{TaskAction, TaskOrigin, TaskRef},
    runtime::{
        Event, ShellTask, ShellTaskAction, ShellTaskCenterError, ShellTaskDelta, ShellTaskOrigin,
        ShellTaskOutputChunk, ShellTaskSnapshot, ShellTaskSubscriptionEvent, UserShellRecord,
    },
};

#[derive(Clone)]
pub(super) struct ShellTaskCenter {
    agent: ConsoleExecution,
    revisions: Arc<Mutex<HashMap<Uuid, ShellTaskSnapshot>>>,
}

pub(super) struct ShellTaskSubscription {
    center: ShellTaskCenter,
    chat: Uuid,
    events: broadcast::Receiver<Event>,
    shells: broadcast::Receiver<UserShellRecord>,
    facts: broadcast::Receiver<()>,
    refresh_pending: bool,
    previous: ShellTaskSnapshot,
    pending: VecDeque<ShellTaskSubscriptionEvent>,
}

impl ShellTaskCenter {
    pub(super) async fn started_at_ms(&self, chat: Uuid, id: Uuid) -> Option<u64> {
        let snapshot = self.agent.snapshot(chat).await.ok()?;
        let command = snapshot
            .commands
            .iter()
            .find(|command| command.operation_id == id);
        let shell = snapshot.user_shells.iter().find(|shell| shell.id == id);
        let path = command
            .map(|command| command.output_ref.as_path())
            .or_else(|| shell.and_then(|shell| shell.output_ref.as_deref()));
        let creation = path.and_then(|path| {
            std::fs::metadata(path)
                .ok()?
                .created()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis()
                .try_into()
                .ok()
        });
        creation
            .or_else(|| {
                snapshot
                    .events
                    .iter()
                    .filter_map(|event| match &event.kind {
                        crate::runtime::EventKind::CommandUpdated { command }
                            if command.operation_id == id =>
                        {
                            command.updated_at_ms
                        }
                        _ => None,
                    })
                    .min()
            })
            .or_else(|| {
                shell
                    .filter(|shell| matches!(shell.state, crate::runtime::UserShellState::Running))
                    .and_then(|shell| shell.state_since)
            })
    }
    pub(super) fn new(agent: ConsoleExecution) -> Self {
        Self {
            agent,
            revisions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub(super) async fn snapshot(
        &self,
        chat: Uuid,
    ) -> Result<ShellTaskSnapshot, ShellTaskCenterError> {
        let mut tasks = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let catalog = match self.agent.catalog().await {
            Ok(catalog) => catalog,
            Err(error) => return self.stale_snapshot(chat, runtime_error(error)).await,
        };
        for entry in catalog {
            if entry.availability == crate::agent_access::ExecutionAvailability::Lost {
                continue;
            }
            let rows = match self.agent.task_rows(entry.chat_id).await {
                Ok(rows) => rows,
                Err(error) => return self.stale_snapshot(chat, runtime_error(error)).await,
            };
            for task in rows {
                if seen.insert((task.chat_id, task.id)) {
                    tasks.push(task);
                }
            }
        }
        tasks.sort_by_key(|task| (task.chat_id != chat, task.state.is_terminal(), task.id));
        let mut revisions = self.revisions.lock().await;
        let live_revision = revisions
            .get(&chat)
            .map(|old| old.live_revision + u64::from(old.tasks != tasks))
            .unwrap_or(1);
        let snapshot = ShellTaskSnapshot {
            chat_id: chat,
            live_revision,
            tasks,
        };
        revisions.insert(chat, snapshot.clone());
        Ok(snapshot)
    }
    async fn stale_snapshot(
        &self,
        chat: Uuid,
        error: ShellTaskCenterError,
    ) -> Result<ShellTaskSnapshot, ShellTaskCenterError> {
        let mut revisions = self.revisions.lock().await;
        let Some(snapshot) = revisions.get_mut(&chat) else {
            return Err(error);
        };
        let mut changed = false;
        for task in &mut snapshot.tasks {
            changed |= task.capabilities.can_background
                || task.capabilities.can_stop
                || task.capabilities.can_read_output;
            task.capabilities.can_background = false;
            task.capabilities.can_stop = false;
            task.capabilities.can_read_output = false;
        }
        snapshot.live_revision += u64::from(changed);
        Ok(snapshot.clone())
    }
    pub(super) async fn subscribe(
        &self,
        chat: Uuid,
    ) -> Result<ShellTaskSubscription, ShellTaskCenterError> {
        let events = self.agent.subscribe();
        let shells = self.agent.subscribe_user_shells();
        let facts = self.agent.subscribe_facts();
        let previous = self.snapshot(chat).await?;
        Ok(ShellTaskSubscription {
            center: self.clone(),
            chat,
            events,
            shells,
            facts,
            refresh_pending: false,
            previous,
            pending: VecDeque::new(),
        })
    }
    async fn find(&self, chat: Uuid, id: Uuid) -> Result<ShellTask, ShellTaskCenterError> {
        self.snapshot(chat)
            .await?
            .tasks
            .into_iter()
            .find(|task| task.id == id)
            .ok_or(ShellTaskCenterError::TaskNotFound {
                chat_id: chat,
                task_id: id,
            })
    }
    pub(super) async fn read_output(
        &self,
        chat: Uuid,
        id: Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ShellTaskOutputChunk, ShellTaskCenterError> {
        let task = self.find(chat, id).await?;
        let origin = match task.origin {
            ShellTaskOrigin::Agent => TaskOrigin::Agent,
            ShellTaskOrigin::User => TaskOrigin::User,
        };
        self.agent
            .task_output(task.chat_id, id, origin, offset, limit)
            .await
            .map_err(runtime_error)
    }
    pub(super) async fn dispatch(
        &self,
        chat: Uuid,
        id: Uuid,
        action: ShellTaskAction,
    ) -> Result<ShellTask, ShellTaskCenterError> {
        let task = self.find(chat, id).await?;
        if task.state.is_terminal() {
            return Ok(task);
        }
        let available = match action {
            ShellTaskAction::Background => task.capabilities.can_background,
            ShellTaskAction::Stop => task.capabilities.can_stop,
        };
        if !available {
            return Err(ShellTaskCenterError::ActionUnavailable {
                task_id: id,
                action,
            });
        }
        let execution = self
            .agent
            .reference(task.chat_id)
            .await
            .map_err(runtime_error)?;
        let task_ref = TaskRef {
            execution,
            task_id: id,
            origin: match task.origin {
                ShellTaskOrigin::Agent => TaskOrigin::Agent,
                ShellTaskOrigin::User => TaskOrigin::User,
            },
        };
        let action = match action {
            ShellTaskAction::Background => TaskAction::Background { task: task_ref },
            ShellTaskAction::Stop => TaskAction::Stop { task: task_ref },
        };
        self.agent
            .command(
                crate::agent_access::AgentCommand::ManageTask { execution, action },
                None,
            )
            .await
            .map_err(runtime_error)?;
        self.find(chat, id).await
    }
}

impl ShellTaskSubscription {
    pub(super) fn chat_id(&self) -> Uuid {
        self.chat
    }
    pub(super) async fn recv(
        &mut self,
    ) -> Result<ShellTaskSubscriptionEvent, ShellTaskCenterError> {
        loop {
            if let Some(update) = self.pending.pop_front() {
                return Ok(update);
            }
            if !self.refresh_pending {
                let lagged = tokio::select! {
                event=self.events.recv()=>match event{Ok(_)=>false,Err(broadcast::error::RecvError::Lagged(_))=>true,Err(_)=>return Err(ShellTaskCenterError::EventStreamClosed)},
                event=self.shells.recv()=>match event{Ok(_)=>false,Err(broadcast::error::RecvError::Lagged(_))=>true,Err(_)=>return Err(ShellTaskCenterError::EventStreamClosed)},
                event=self.facts.recv()=>match event{Ok(_)=>false,Err(broadcast::error::RecvError::Lagged(_))=>true,Err(_)=>return Err(ShellTaskCenterError::EventStreamClosed)},
                };
                self.refresh_pending = true;
                if lagged {
                    return Ok(ShellTaskSubscriptionEvent::ResyncRequired {
                        last_live_revision: self.previous.live_revision,
                    });
                }
            }
            let snapshot = self.center.snapshot(self.chat).await?;
            for task in &snapshot.tasks {
                let previous = self
                    .previous
                    .tasks
                    .iter()
                    .find(|old| old.id == task.id && old.chat_id == task.chat_id);
                if previous != Some(task) {
                    self.pending
                        .push_back(ShellTaskSubscriptionEvent::Changed(ShellTaskDelta {
                            live_revision: snapshot.live_revision,
                            task: task.clone(),
                            previous_state: previous.map(|old| old.state),
                            previous_output_bytes: previous.map(|old| old.output_bytes),
                        }));
                }
            }
            self.previous = snapshot;
            self.refresh_pending = false;
        }
    }
    pub(super) async fn resync(&mut self) -> Result<ShellTaskSnapshot, ShellTaskCenterError> {
        let snapshot = self.center.snapshot(self.chat).await?;
        self.previous = snapshot.clone();
        self.pending.clear();
        self.refresh_pending = false;
        Ok(snapshot)
    }
}

fn runtime_error(error: crate::runtime::AgentError) -> ShellTaskCenterError {
    ShellTaskCenterError::Runtime(error.to_string())
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;
    use crate::agent_access::*;
    use crate::terminal::test_support;

    struct OlderTaskRecords(EmbeddedAgentAccess);
    #[async_trait::async_trait]
    impl AgentAccess for OlderTaskRecords {
        async fn observe(&self, request: ObservationRequest) -> Result<Observation, AccessError> {
            let mut observation = self.0.observe(request).await?;
            match &mut observation.initial {
                ObservationValue::Tasks(tasks) => {
                    for task in tasks {
                        task.updated_at_ms = None;
                    }
                }
                ObservationValue::Agent(snapshot) => {
                    for shell in &mut snapshot.user_shells {
                        shell.updated_at_ms = None;
                    }
                    for command in &mut snapshot.commands {
                        command.updated_at_ms = None;
                    }
                }
                _ => {}
            }
            Ok(observation)
        }
        async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
            self.0.execute(command).await
        }
        async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
            self.0.show_view(update).await
        }
    }

    #[tokio::test]
    async fn completed_shell_time_is_recovered_when_an_older_owner_snapshot_is_resynced() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = crate::runtime::Agent::new_embedded(
            test_support::config(),
            Arc::new(test_support::ScriptedModel::new(Vec::new())),
        );
        let access = Arc::new(OlderTaskRecords(EmbeddedAgentAccess::new(runtime.clone())));
        let agent = ConsoleExecution::from_access_for_test(
            access.clone(),
            access.0.owner_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            LaunchSpec::embedded(directory.path().into()),
            None,
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = runtime.subscribe_user_shells();
        let receipt = agent
            .submit_user_shell_for_goal(chat, None, "printf finished; exit 1".into(), false)
            .await
            .unwrap();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let record = events.recv().await.unwrap();
                if record.id == receipt.command_id
                    && record.state == crate::runtime::UserShellState::Failed
                {
                    break record;
                }
            }
        })
        .await
        .unwrap();
        let center = ShellTaskCenter::new(agent.clone());
        for _ in 0..2 {
            let snapshot = center.snapshot(chat).await.unwrap();
            let task = snapshot
                .tasks
                .iter()
                .find(|task| task.id == receipt.command_id)
                .unwrap();
            assert_eq!(task.exit_code, Some(1));
            assert!(task.updated_at_ms.is_some());
            assert!(task.updated_at_ms >= terminal.state_since);
        }
        runtime.shutdown().await;
    }
}
