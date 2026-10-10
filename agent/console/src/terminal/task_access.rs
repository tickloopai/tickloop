use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use uuid::Uuid;

use super::execution::ConsoleExecution;
use tickloop_agent_protocol::{
    AccessError, AgentCommand, ChatStatus, ExecutionAvailability, ShellTask, ShellTaskAction,
    ShellTaskCenterError, ShellTaskOrigin, ShellTaskOutputChunk, ShellTaskSnapshot, ShellTaskState,
    TaskAction, TaskOrigin, TaskRef, UserShellState,
};

#[derive(Clone)]
pub(super) struct ShellTaskCenter {
    agent: ConsoleExecution,
    revisions: Arc<Mutex<HashMap<Uuid, ShellTaskSnapshot>>>,
}

/// When Shell `id` of the Chat started: its output file's creation, else its first recorded
/// update, else when a running user Shell entered its state.
pub(super) fn shell_started_at_ms(status: &ChatStatus, id: Uuid) -> Option<u64> {
    let command = status
        .commands
        .iter()
        .find(|command| command.operation_id == id);
    let shell = status.user_shells.iter().find(|shell| shell.id == id);
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
        .or_else(|| status.command_started_at_ms.get(&id).copied())
        .or_else(|| {
            shell
                .filter(|shell| matches!(shell.state, UserShellState::Running))
                .and_then(|shell| shell.state_since)
        })
}

/// When each running background Shell in `tasks` started, for the elapsed time on its Monitor
/// row.
pub(super) fn shell_starts(status: &ChatStatus, tasks: &ShellTaskSnapshot) -> HashMap<Uuid, u64> {
    tasks
        .tasks
        .iter()
        .filter(|task| {
            !task.state.is_terminal()
                && (task.was_background || task.state == ShellTaskState::Background)
        })
        .filter_map(|task| Some((task.id, shell_started_at_ms(status, task.id)?)))
        .collect()
}

impl ShellTaskCenter {
    pub(super) async fn started_at_ms(&self, chat: Uuid, id: Uuid) -> Option<u64> {
        let status = self.agent.status(chat).await.ok()?;
        shell_started_at_ms(&status, id)
    }
    pub(super) fn new(agent: ConsoleExecution) -> Self {
        Self {
            agent,
            revisions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    /// The Chat's Shell tasks, read from its execution directly. Only a failed read consults
    /// the execution catalog: a Chat it no longer lists, or whose execution it confirms lost,
    /// has no tasks; a Chat it moved to a live replacing execution is read once more; any other
    /// failure keeps the previous tasks without their controls.
    pub(super) async fn snapshot(
        &self,
        chat: Uuid,
    ) -> Result<ShellTaskSnapshot, ShellTaskCenterError> {
        let queried = self.agent.reference(chat).await.ok();
        let mut tasks = match self.agent.task_rows(chat).await {
            Ok(rows) => rows,
            Err(error) => {
                let catalog = match self.agent.catalog().await {
                    Ok(catalog) => catalog,
                    Err(error) => return self.stale_snapshot(chat, runtime_error(error)).await,
                };
                match catalog.into_iter().find(|entry| entry.chat_id == chat) {
                    None => Vec::new(),
                    Some(entry)
                        if Some(entry.execution) == queried
                            && entry.availability == ExecutionAvailability::Lost =>
                    {
                        Vec::new()
                    }
                    Some(entry)
                        if Some(entry.execution) != queried
                            && entry.availability == ExecutionAvailability::Live =>
                    {
                        match self.agent.task_rows(chat).await {
                            Ok(rows) => rows,
                            Err(error) => {
                                return self.stale_snapshot(chat, runtime_error(error)).await;
                            }
                        }
                    }
                    Some(_) => return self.stale_snapshot(chat, runtime_error(error)).await,
                }
            }
        };
        tasks.retain(|task| task.chat_id == chat);
        tasks.sort_by_key(|task| (task.state.is_terminal(), task.id));
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
            .command(AgentCommand::ManageTask { execution, action }, None)
            .await
            .map_err(runtime_error)?;
        self.find(chat, id).await
    }
}

fn runtime_error(error: AccessError) -> ShellTaskCenterError {
    ShellTaskCenterError::Runtime(error.to_string())
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;
    use tickloop_agent_daemon::EmbeddedAgentAccess;
    use tickloop_agent_protocol::{
        AccessError, AgentAccess, CommandEnvelope, CommandReceipt, DisplayReceipt, DisplayUpdate,
        LaunchSpec, Observation, ObservationRequest, ObservationValue, ViewRef,
    };

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
    async fn completed_shell_time_is_recovered_when_an_older_daemon_snapshot_is_resynced() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tickloop_agent_runtime::Agent::new_embedded(
            tickloop_agent_test_support::config(),
            Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let access = Arc::new(OlderTaskRecords(EmbeddedAgentAccess::new(runtime.clone())));
        let agent = ConsoleExecution::from_access_for_test(
            access.clone(),
            access.0.daemon_epoch(),
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
                    && record.state == tickloop_agent_protocol::UserShellState::Failed
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

    #[tokio::test]
    async fn a_snapshot_holds_only_its_own_chats_tasks() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tickloop_agent_runtime::Agent::new_embedded(
            tickloop_agent_test_support::config(),
            Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
        let agent = ConsoleExecution::from_access_for_test(
            access.clone(),
            access.daemon_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            LaunchSpec::embedded(directory.path().into()),
            None,
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let peer = agent.create_chat(directory.path().into()).await.unwrap();
        agent.display_chat(Some(peer)).await.unwrap();
        let peer_task = agent
            .submit_user_shell_for_goal(peer, None, "printf peer".into(), true)
            .await
            .unwrap()
            .command_id;
        agent.display_chat(Some(chat)).await.unwrap();
        let center = ShellTaskCenter::new(agent.clone());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !center
                .snapshot(peer)
                .await
                .unwrap()
                .tasks
                .iter()
                .any(|task| task.id == peer_task)
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = center.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.chat_id, chat);
        assert!(snapshot.tasks.is_empty(), "{:?}", snapshot.tasks);
        runtime.shutdown().await;
    }
}

#[cfg(test)]
mod availability_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tickloop_agent_daemon::EmbeddedAgentAccess;
    use tickloop_agent_protocol::{
        AccessError, AgentAccess, AgentDetailQuery, CommandEnvelope, CommandReceipt,
        DisplayReceipt, DisplayUpdate, LaunchSpec, Observation, ObservationRequest,
        ObservationValue, ViewRef,
    };

    #[derive(Clone, Copy)]
    enum FailureProjection {
        Lost,
        Live,
        Replaced,
        Unavailable,
    }

    struct FailedTaskLookup {
        actual: EmbeddedAgentAccess,
        failed: AtomicBool,
        projection: FailureProjection,
    }
    #[async_trait::async_trait]
    impl AgentAccess for FailedTaskLookup {
        async fn observe(&self, request: ObservationRequest) -> Result<Observation, AccessError> {
            if matches!(
                &request,
                ObservationRequest::Agent {
                    detail: AgentDetailQuery::Tasks,
                    ..
                }
            ) {
                self.failed.store(true, Ordering::SeqCst);
                return Err(AccessError::unavailable("fixture worker no longer running"));
            }
            if self.failed.load(Ordering::SeqCst)
                && matches!(&request, ObservationRequest::Overview { .. })
                && matches!(self.projection, FailureProjection::Unavailable)
            {
                return Err(AccessError::unavailable("fixture catalog unavailable"));
            }
            let mut observation = self.actual.observe(request).await?;
            if self.failed.load(Ordering::SeqCst)
                && let ObservationValue::Overview(rows) = &mut observation.initial
            {
                for row in rows {
                    match self.projection {
                        FailureProjection::Live | FailureProjection::Unavailable => {}
                        FailureProjection::Lost => row.availability = ExecutionAvailability::Lost,
                        FailureProjection::Replaced => {
                            row.availability = ExecutionAvailability::Lost;
                            row.execution.generation += 1;
                        }
                    }
                }
            }
            Ok(observation)
        }
        async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
            self.actual.execute(command).await
        }
        async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
            self.actual.show_view(update).await
        }
    }

    #[tokio::test]
    async fn a_worker_lost_between_catalog_and_tasks_does_not_abort_passive_observation() {
        for projection in [
            FailureProjection::Lost,
            FailureProjection::Live,
            FailureProjection::Replaced,
            FailureProjection::Unavailable,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let runtime = tickloop_agent_runtime::Agent::new_embedded(
                tickloop_agent_test_support::config(),
                Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
            );
            let actual = EmbeddedAgentAccess::new(runtime.clone());
            let epoch = actual.daemon_epoch();
            let access = Arc::new(FailedTaskLookup {
                actual,
                failed: AtomicBool::new(false),
                projection,
            });
            let agent = ConsoleExecution::from_access_for_test(
                access,
                epoch,
                ViewRef {
                    session_id: Uuid::new_v4(),
                    view_id: Uuid::new_v4(),
                },
                LaunchSpec::embedded(directory.path().into()),
                None,
            );
            let chat = agent.create_chat(directory.path().into()).await.unwrap();
            let result = ShellTaskCenter::new(agent).snapshot(chat).await;
            runtime.shutdown().await;
            match projection {
                FailureProjection::Lost => assert!(
                    result.unwrap().tasks.is_empty(),
                    "confirmed lost task owner must leave loss reporting to the execution catalog"
                ),
                FailureProjection::Live
                | FailureProjection::Replaced
                | FailureProjection::Unavailable => assert!(
                    result.is_err(),
                    "unconfirmed loss or another generation must not hide the task lookup failure"
                ),
            }
        }
    }
}
