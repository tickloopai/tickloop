use std::{cmp::Reverse, collections::HashMap, sync::Arc};

use serde::Deserialize;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

use super::{Agent, AgentError};
use tickloop_agent_protocol::{
    ChatId, ChatSnapshot, CommandSnapshot, CommandState, Event, EventKind,
    SHELL_TASK_OUTPUT_MAX_BYTES, ShellTask, ShellTaskAction, ShellTaskCapabilities,
    ShellTaskCenterError, ShellTaskDelta, ShellTaskOrigin, ShellTaskOutputChunk, ShellTaskOwner,
    ShellTaskSnapshot, ShellTaskState, ShellTaskSubscriptionEvent, ToolResult, UserShellRecord,
    UserShellState, recover_shell_update_time,
};

#[derive(Clone)]
pub struct ShellTaskCenter {
    agent: Agent,
    revisions: Arc<Mutex<HashMap<ChatId, RevisionState>>>,
}

#[derive(Clone)]
struct RevisionState {
    live_revision: u64,
    tasks: Vec<ShellTask>,
}

pub struct ShellTaskSubscription {
    center: ShellTaskCenter,
    chat_id: ChatId,
    agent_events: broadcast::Receiver<Event>,
    user_events: broadcast::Receiver<UserShellRecord>,
    description_events: broadcast::Receiver<Uuid>,
    pending_task_id: Option<Uuid>,
    known_tasks: HashMap<Uuid, ShellTask>,
    last_live_revision: u64,
}

#[derive(Clone)]
struct OrderedTask {
    task: ShellTask,
    conversation_position: usize,
    stable_sequence: u64,
}

impl ShellTaskCenter {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            revisions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn snapshot(
        &self,
        chat_id: ChatId,
    ) -> Result<ShellTaskSnapshot, ShellTaskCenterError> {
        // Keep source reads in revision order, including across concurrent subscribers.
        let mut revisions = self.revisions.lock().await;
        let mut chat_ids = self
            .agent
            .inner
            .chats
            .lock()
            .await
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if !chat_ids.contains(&chat_id) {
            return Err(ShellTaskCenterError::ChatNotFound { chat_id });
        }
        chat_ids.sort_by_key(|candidate| (*candidate != chat_id, *candidate));
        let mut tasks = Vec::new();
        for candidate in chat_ids {
            // The projection reads the Chat in place: copying its history for a few task rows
            // would hold the Chat lock as long as the conversation is.
            let projected = match self.agent.commands(candidate).await {
                Ok(live_agent_commands) => self
                    .agent
                    .inner
                    .chats
                    .lock()
                    .await
                    .get(&candidate)
                    .map(|chat| project_tasks(&chat.snapshot, live_agent_commands))
                    .ok_or(AgentError::NotFound),
                Err(error) => Err(error),
            };
            match projected {
                Ok(projected) => tasks.extend(projected),
                Err(AgentError::NotFound) if candidate != chat_id => continue,
                Err(error) => return Err(map_chat_error(candidate, error)),
            }
        }
        let generated = self.agent.inner.shell_descriptions.lock().await;
        for task in tasks.iter_mut().filter(|task| task.description.is_none()) {
            task.description = generated.get(&task.id).cloned().flatten();
        }
        drop(generated);
        // Order within each Chat is unchanged; active tasks as a whole come before finished ones,
        // and within each group the foreground Chat comes first.
        tasks.sort_by_key(|task| (task.state.is_terminal(), task.chat_id != chat_id));
        let live_revision = match revisions.get_mut(&chat_id) {
            Some(state) if state.tasks == tasks => state.live_revision,
            Some(state) => {
                state.live_revision = state.live_revision.saturating_add(1);
                state.tasks = tasks.clone();
                state.live_revision
            }
            None => {
                let live_revision = u64::from(!tasks.is_empty());
                revisions.insert(
                    chat_id,
                    RevisionState {
                        live_revision,
                        tasks: tasks.clone(),
                    },
                );
                live_revision
            }
        };
        Ok(ShellTaskSnapshot {
            chat_id,
            live_revision,
            tasks,
        })
    }

    pub async fn subscribe(
        &self,
        chat_id: ChatId,
    ) -> Result<ShellTaskSubscription, ShellTaskCenterError> {
        let agent_events = self.agent.subscribe();
        let user_events = self.agent.subscribe_user_shells();
        let description_events = self.agent.inner.shell_description_events.subscribe();
        let initial = self.snapshot(chat_id).await?;
        Ok(ShellTaskSubscription {
            center: self.clone(),
            chat_id,
            agent_events,
            user_events,
            description_events,
            pending_task_id: None,
            known_tasks: task_map(&initial.tasks),
            last_live_revision: initial.live_revision,
        })
    }

    pub async fn read_output(
        &self,
        chat_id: ChatId,
        task_id: Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ShellTaskOutputChunk, ShellTaskCenterError> {
        if limit == 0 || limit > SHELL_TASK_OUTPUT_MAX_BYTES {
            return Err(ShellTaskCenterError::InvalidOutputRead {
                task_id,
                reason: format!("limit must be between 1 and {SHELL_TASK_OUTPUT_MAX_BYTES} bytes"),
            });
        }
        let snapshot = self.snapshot(chat_id).await?;
        let task = find_task(&snapshot, task_id)?;
        if !task.capabilities.can_read_output {
            return Err(ShellTaskCenterError::OutputUnavailable { task_id });
        }
        let owner_chat_id = task.chat_id;
        let result = match task.origin {
            ShellTaskOrigin::Agent => {
                self.agent
                    .read_command(owner_chat_id, task_id, offset, limit)
                    .await
            }
            ShellTaskOrigin::User => {
                self.agent
                    .read_user_shell_output(owner_chat_id, task_id, offset, limit)
                    .await
            }
        }
        .map_err(|error| map_output_error(task_id, error))?;
        parse_output_chunk(task_id, offset, limit, result)
    }

    pub async fn dispatch(
        &self,
        chat_id: ChatId,
        task_id: Uuid,
        action: ShellTaskAction,
    ) -> Result<ShellTask, ShellTaskCenterError> {
        let snapshot = self.snapshot(chat_id).await?;
        let task = find_task(&snapshot, task_id)?;
        if task.state.is_terminal() {
            return Ok(task.clone());
        }
        let is_available = match action {
            ShellTaskAction::Background => task.capabilities.can_background,
            ShellTaskAction::Stop => task.capabilities.can_stop,
        };
        if !is_available {
            return Err(ShellTaskCenterError::ActionUnavailable { task_id, action });
        }
        let chat_id = task.chat_id;
        match (task.origin, action) {
            (ShellTaskOrigin::Agent, ShellTaskAction::Background) => self
                .agent
                .background_command(chat_id, task_id)
                .await
                .map(|_| ()),
            (ShellTaskOrigin::Agent, ShellTaskAction::Stop) => {
                self.agent.stop_command(chat_id, task_id).await.map(|_| ())
            }
            (ShellTaskOrigin::User, ShellTaskAction::Background) => self
                .agent
                .background_user_shell(chat_id, task_id)
                .await
                .map(|_| ()),
            (ShellTaskOrigin::User, ShellTaskAction::Stop) => self
                .agent
                .stop_user_shell(chat_id, task_id)
                .await
                .map(|_| ()),
        }
        .map_err(|error| ShellTaskCenterError::Runtime(error.to_string()))?;
        let refreshed = self.snapshot(snapshot.chat_id).await?;
        Ok(find_task(&refreshed, task_id)?.clone())
    }
}

impl ShellTaskSubscription {
    pub fn chat_id(&self) -> ChatId {
        self.chat_id
    }

    pub async fn recv(&mut self) -> Result<ShellTaskSubscriptionEvent, ShellTaskCenterError> {
        loop {
            if self.pending_task_id.is_none() {
                let task_id = tokio::select! {
                    event = self.agent_events.recv() => match event {
                        Ok(event) => match event.kind {
                            EventKind::CommandUpdated { command } => Some(command.operation_id),
                            _ => None,
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            return Ok(ShellTaskSubscriptionEvent::ResyncRequired {
                                last_live_revision: self.last_live_revision,
                            });
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(ShellTaskCenterError::EventStreamClosed);
                        }
                    },
                    event = self.user_events.recv() => match event {
                        Ok(record) => Some(record.id),
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            return Ok(ShellTaskSubscriptionEvent::ResyncRequired {
                                last_live_revision: self.last_live_revision,
                            });
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(ShellTaskCenterError::EventStreamClosed);
                        }
                    },
                    event = self.description_events.recv() => match event {
                        Ok(task_id) => Some(task_id),
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            return Ok(ShellTaskSubscriptionEvent::ResyncRequired {
                                last_live_revision: self.last_live_revision,
                            });
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(ShellTaskCenterError::EventStreamClosed);
                        }
                    },
                };
                self.pending_task_id = task_id;
            }
            let Some(task_id) = self.pending_task_id else {
                continue;
            };
            // A parent select can cancel this read after the broadcast event was consumed.
            let snapshot = self.center.snapshot(self.chat_id).await?;
            self.pending_task_id = None;
            let Some(task) = snapshot
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .cloned()
            else {
                return Ok(ShellTaskSubscriptionEvent::ResyncRequired {
                    last_live_revision: self.last_live_revision,
                });
            };
            let previous = self.known_tasks.insert(task_id, task.clone());
            self.last_live_revision = snapshot.live_revision;
            if previous.as_ref() == Some(&task) {
                continue;
            }
            return Ok(ShellTaskSubscriptionEvent::Changed(Box::new(
                ShellTaskDelta {
                    live_revision: snapshot.live_revision,
                    task,
                    previous_state: previous.as_ref().map(|task| task.state),
                    previous_output_bytes: previous.map(|task| task.output_bytes),
                },
            )));
        }
    }

    pub async fn resync(&mut self) -> Result<ShellTaskSnapshot, ShellTaskCenterError> {
        let snapshot = self.center.snapshot(self.chat_id).await?;
        self.known_tasks = task_map(&snapshot.tasks);
        self.last_live_revision = snapshot.live_revision;
        self.pending_task_id = None;
        Ok(snapshot)
    }
}

pub(crate) fn project_tasks(
    chat: &ChatSnapshot,
    live_agent_commands: Vec<CommandSnapshot>,
) -> Vec<ShellTask> {
    let mut agent_commands = chat
        .commands
        .iter()
        .cloned()
        .map(|command| (command.operation_id, command))
        .collect::<HashMap<_, _>>();
    for mut command in live_agent_commands {
        if let Some(recorded) = agent_commands.get(&command.operation_id) {
            command.updated_at_ms = command
                .updated_at_ms
                .into_iter()
                .chain(recorded.updated_at_ms)
                .max();
        }
        agent_commands.insert(command.operation_id, command);
    }

    let mut tasks = agent_commands
        .into_values()
        .map(|command| {
            let operation_position = chat
                .operations
                .iter()
                .position(|operation| operation.id == command.operation_id);
            let call_id =
                operation_position.map(|position| chat.operations[position].call_id.clone());
            let operation_position = operation_position.unwrap_or(usize::MAX);
            OrderedTask {
                task: agent_task(chat.id, command, call_id),
                conversation_position: operation_position,
                stable_sequence: operation_position as u64,
            }
        })
        .chain(chat.user_shells.iter().map(|record| OrderedTask {
            task: user_task(record),
            conversation_position: record.submitted_at_message,
            stable_sequence: record.queued_sequence,
        }))
        .collect::<Vec<_>>();
    tasks.sort_by_key(|ordered| {
        (
            ordered.task.state.is_terminal(),
            Reverse(ordered.conversation_position),
            Reverse(ordered.stable_sequence),
            match ordered.task.origin {
                ShellTaskOrigin::Agent => 0_u8,
                ShellTaskOrigin::User => 1_u8,
            },
        )
    });
    tasks.into_iter().map(|ordered| ordered.task).collect()
}

fn agent_task(chat_id: ChatId, command: CommandSnapshot, call_id: Option<String>) -> ShellTask {
    let state = map_agent_state(command.state);
    ShellTask {
        updated_at_ms: recover_shell_update_time(
            command.updated_at_ms,
            None,
            Some(&command.output_ref),
        ),
        // A command stamps its record when it finishes; output written later does not count.
        ended_at_ms: command.updated_at_ms.filter(|_| state.is_terminal()),
        id: command.operation_id,
        chat_id,
        origin: ShellTaskOrigin::Agent,
        owner: ShellTaskOwner::Agent {
            loop_id: command.loop_id.into(),
        },
        call_id,
        command: command.command,
        description: command
            .description
            .filter(|description| !description.trim().is_empty()),
        cwd: command.cwd,
        state,
        was_background: command.was_background,
        pid: command.pid,
        exit_code: command.exit_code,
        signal: command.signal,
        reason: command.reason,
        output_bytes: command.output_bytes,
        capabilities: capabilities(ShellTaskOrigin::Agent, state, true),
    }
}

fn user_task(record: &UserShellRecord) -> ShellTask {
    let state = map_user_state(&record.state);
    ShellTask {
        updated_at_ms: recover_shell_update_time(
            record.updated_at_ms,
            record.state_since,
            record.output_ref.as_deref(),
        ),
        ended_at_ms: record
            .state_since
            .or(record.updated_at_ms)
            .filter(|_| state.is_terminal()),
        id: record.id,
        chat_id: record.chat_id,
        origin: ShellTaskOrigin::User,
        owner: ShellTaskOwner::User {
            chat_id: record.chat_id,
        },
        call_id: None,
        command: record.command.clone(),
        description: None,
        cwd: record.cwd.clone(),
        state,
        was_background: record.starts_in_background,
        pid: record.pid,
        exit_code: record.exit_code,
        signal: record.signal,
        reason: record.reason.clone(),
        output_bytes: record.output_bytes,
        capabilities: capabilities(ShellTaskOrigin::User, state, record.output_ref.is_some()),
    }
}

fn capabilities(
    origin: ShellTaskOrigin,
    state: ShellTaskState,
    has_output: bool,
) -> ShellTaskCapabilities {
    ShellTaskCapabilities {
        can_background: match origin {
            ShellTaskOrigin::Agent => state == ShellTaskState::Running,
            ShellTaskOrigin::User => {
                matches!(state, ShellTaskState::Queued | ShellTaskState::Running)
            }
        },
        can_stop: match origin {
            ShellTaskOrigin::Agent => {
                matches!(state, ShellTaskState::Running | ShellTaskState::Background)
            }
            ShellTaskOrigin::User => matches!(
                state,
                ShellTaskState::Queued
                    | ShellTaskState::WaitingPermission
                    | ShellTaskState::Running
                    | ShellTaskState::Background
            ),
        },
        can_read_output: has_output,
    }
}

fn map_agent_state(state: CommandState) -> ShellTaskState {
    match state {
        CommandState::Running => ShellTaskState::Running,
        CommandState::Background => ShellTaskState::Background,
        CommandState::Cancelling => ShellTaskState::Cancelling,
        CommandState::Completed => ShellTaskState::Completed,
        CommandState::Failed => ShellTaskState::Failed,
        CommandState::Unknown => ShellTaskState::Unknown,
    }
}

fn map_user_state(state: &UserShellState) -> ShellTaskState {
    match state {
        UserShellState::Queued => ShellTaskState::Queued,
        UserShellState::WaitingPermission => ShellTaskState::WaitingPermission,
        UserShellState::Running => ShellTaskState::Running,
        UserShellState::Background => ShellTaskState::Background,
        UserShellState::Cancelling => ShellTaskState::Cancelling,
        UserShellState::Completed => ShellTaskState::Completed,
        UserShellState::Failed => ShellTaskState::Failed,
        UserShellState::Unknown => ShellTaskState::Unknown,
    }
}

fn find_task(
    snapshot: &ShellTaskSnapshot,
    task_id: Uuid,
) -> Result<&ShellTask, ShellTaskCenterError> {
    snapshot.tasks.iter().find(|task| task.id == task_id).ok_or(
        ShellTaskCenterError::TaskNotFound {
            chat_id: snapshot.chat_id,
            task_id,
        },
    )
}

fn task_map(tasks: &[ShellTask]) -> HashMap<Uuid, ShellTask> {
    tasks.iter().cloned().map(|task| (task.id, task)).collect()
}

fn map_chat_error(chat_id: ChatId, error: AgentError) -> ShellTaskCenterError {
    match error {
        AgentError::NotFound => ShellTaskCenterError::ChatNotFound { chat_id },
        error => ShellTaskCenterError::Runtime(error.to_string()),
    }
}

fn map_output_error(task_id: Uuid, error: AgentError) -> ShellTaskCenterError {
    match error {
        AgentError::NotFound => ShellTaskCenterError::OutputUnavailable { task_id },
        error => ShellTaskCenterError::OutputReadFailed {
            task_id,
            reason: error.to_string(),
        },
    }
}

#[derive(Deserialize)]
struct OutputPayload {
    output: String,
    encoding: String,
    total_bytes: u64,
    offset: u64,
    truncated: bool,
    next_offset: Option<u64>,
}

fn parse_output_chunk(
    task_id: Uuid,
    requested_offset: u64,
    requested_limit: usize,
    result: ToolResult,
) -> Result<ShellTaskOutputChunk, ShellTaskCenterError> {
    let payload: OutputPayload = serde_json::from_str(&result.content)
        .map_err(|error| ShellTaskCenterError::InvalidOutput(error.to_string()))?;
    if payload.offset != requested_offset
        || payload.total_bytes < payload.offset
        || payload
            .next_offset
            .is_some_and(|next| next < payload.offset || next > payload.total_bytes)
        || (payload.truncated && payload.next_offset.is_none())
        || (payload.truncated
            && payload
                .next_offset
                .is_some_and(|next| next <= payload.offset))
        || (!payload.truncated && payload.next_offset.is_some())
        || payload.output.len() > requested_limit.saturating_mul(3)
    {
        return Err(ShellTaskCenterError::InvalidOutput(
            "offset metadata is inconsistent".into(),
        ));
    }
    let confirmed_offset = payload.next_offset.unwrap_or(payload.total_bytes);
    Ok(ShellTaskOutputChunk {
        task_id,
        offset: payload.offset,
        confirmed_offset,
        total_bytes: payload.total_bytes,
        next_offset: payload.next_offset,
        truncated: payload.truncated,
        encoding: payload.encoding,
        output: payload.output,
    })
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
    use tickloop_agent_protocol::{InputModality, ModelConfig, Protocol};

    struct UnusedModel;

    #[async_trait]
    impl ModelClient for UnusedModel {
        async fn generate_title(
            &self,
            request: tickloop_agent_model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
            Ok(tickloop_agent_model::fixture_title(request))
        }

        fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
            tickloop_agent_protocol::ModelIdentity {
                provider: tickloop_agent_protocol::ModelProvider::Api,
                profile: "fixture".into(),
                protocol: tickloop_agent_protocol::Protocol::Messages,
                requested_model: "fixture".into(),
            }
        }

        async fn complete(
            &self,
            _: ModelRequest,
            _: UnboundedSender<ModelDelta>,
            _: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            panic!("model should not run")
        }
    }

    fn agent() -> Agent {
        let agent = Agent::new_embedded(
            ModelConfig {
                provider: tickloop_agent_protocol::ModelProvider::Api,
                auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
                name: "test".into(),
                protocol: Protocol::Messages,
                base_url: "http://127.0.0.1".into(),
                model: "test".into(),
                api_key_env: String::new(),
                max_output_tokens: 1024,
                provider_id: String::new(),
                provider_name: String::new(),
                model_id: String::new(),
                connection: None,
                card: None,
                input_modalities: vec![InputModality::Text],
            },
            Arc::new(UnusedModel),
        );
        agent.install_own_tickhost();
        agent
    }

    async fn create_chat(agent: &Agent, workdir: &Path) -> ChatId {
        agent.create_chat(workdir.into()).await.unwrap()
    }

    #[test]
    fn owner_specific_capabilities_preserve_lifecycle_boundaries() {
        assert_eq!(
            capabilities(ShellTaskOrigin::Agent, ShellTaskState::Running, true),
            ShellTaskCapabilities {
                can_background: true,
                can_stop: true,
                can_read_output: true,
            }
        );
        assert_eq!(
            capabilities(ShellTaskOrigin::User, ShellTaskState::Queued, false),
            ShellTaskCapabilities {
                can_background: true,
                can_stop: true,
                can_read_output: false,
            }
        );
        assert_eq!(
            capabilities(ShellTaskOrigin::Agent, ShellTaskState::Completed, true),
            ShellTaskCapabilities {
                can_background: false,
                can_stop: false,
                can_read_output: true,
            }
        );
    }

    #[tokio::test]
    async fn cancelling_a_task_subscription_read_preserves_the_consumed_update() {
        let directory = tempfile::tempdir().unwrap();
        let agent = agent();
        let chat_id = create_chat(&agent, directory.path()).await;
        let center = ShellTaskCenter::new(agent.clone());
        let mut subscription = center.subscribe(chat_id).await.unwrap();
        let mut completion_events = agent.subscribe_user_shells();
        let receipt = agent
            .submit_user_shell(chat_id, "printf finished".into(), false)
            .await
            .unwrap();
        let terminal = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let record = completion_events.recv().await.unwrap();
                if record.id == receipt.command_id
                    && matches!(record.state, UserShellState::Completed)
                {
                    break record;
                }
            }
        })
        .await
        .unwrap();

        // Deliver only the final notification so another update cannot mask a lost wakeup.
        let (updates, receiver) = broadcast::channel(1);
        subscription.user_events = receiver;
        subscription.agent_events = agent.subscribe();
        updates.send(terminal).unwrap();
        let revision_guard = center.revisions.lock().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscription.recv())
                .await
                .is_err()
        );
        drop(revision_guard);
        let event = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, ShellTaskSubscriptionEvent::Changed(delta)
            if delta.task.id == receipt.command_id && delta.task.state == ShellTaskState::Completed));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn task_subscription_lag_resync_preserves_latest_output() {
        let directory = tempfile::tempdir().unwrap();
        let agent = agent();
        let chat_id = create_chat(&agent, directory.path()).await;
        let center = ShellTaskCenter::new(agent.clone());
        let mut subscription = center.subscribe(chat_id).await.unwrap();
        let mut completion_events = agent.subscribe_user_shells();
        let receipt = agent
            .submit_user_shell(chat_id, "printf lag-output".into(), true)
            .await
            .unwrap();
        let terminal = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let record = completion_events.recv().await.unwrap();
                if record.id == receipt.command_id
                    && matches!(record.state, UserShellState::Completed)
                {
                    break record;
                }
            }
        })
        .await
        .unwrap();
        let (updates, receiver) = broadcast::channel(1);
        subscription.user_events = receiver;
        subscription.agent_events = agent.subscribe();
        // A fresh receiver keeps a background description arriving meanwhile from racing the lag.
        subscription.description_events = agent.inner.shell_description_events.subscribe();
        updates.send(terminal.clone()).unwrap();
        updates.send(terminal).unwrap();
        assert!(matches!(
            subscription.recv().await.unwrap(),
            ShellTaskSubscriptionEvent::ResyncRequired { .. }
        ));
        let snapshot = subscription.resync().await.unwrap();
        assert_eq!(snapshot.tasks[0].state, ShellTaskState::Completed);
        let chunk = center
            .read_output(chat_id, receipt.command_id, 0, 65536)
            .await
            .unwrap();
        assert_eq!(chunk.output, "lag-output");
        assert_eq!(chunk.confirmed_offset, 10);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn user_shell_snapshot_and_output_use_the_shared_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let agent = agent();
        let chat_id = create_chat(&agent, directory.path()).await;
        let mut subscription = ShellTaskCenter::new(agent.clone())
            .subscribe(chat_id)
            .await
            .unwrap();
        let receipt = agent
            .submit_user_shell(chat_id, "printf 'abcdef'".into(), false)
            .await
            .unwrap();

        let task = loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(3), subscription.recv())
                    .await
                    .unwrap()
                    .unwrap();
            if let ShellTaskSubscriptionEvent::Changed(delta) = event
                && delta.task.id == receipt.command_id
                && delta.task.state.is_terminal()
            {
                break delta.task;
            }
        };
        assert_eq!(task.origin, ShellTaskOrigin::User);
        assert_eq!(task.owner, ShellTaskOwner::User { chat_id });
        assert_eq!(task.state, ShellTaskState::Completed);
        assert!(!task.capabilities.can_stop);

        let output = subscription
            .center
            .read_output(chat_id, receipt.command_id, 0, 4)
            .await
            .unwrap();
        assert_eq!(output.output, "abcd");
        assert_eq!(output.offset, 0);
        assert_eq!(output.confirmed_offset, 4);
        assert_eq!(output.next_offset, Some(4));
        assert_eq!(output.total_bytes, 6);
        assert!(output.truncated);
        for action in [ShellTaskAction::Background, ShellTaskAction::Stop] {
            let terminal = subscription
                .center
                .dispatch(chat_id, receipt.command_id, action)
                .await
                .unwrap();
            assert_eq!(terminal.state, ShellTaskState::Completed);
            assert_eq!(terminal.id, receipt.command_id);
            assert!(!terminal.was_background);
        }
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn snapshots_keep_active_tasks_before_terminal_tasks_with_stable_order() {
        let directory = tempfile::tempdir().unwrap();
        let agent = agent();
        let chat_id = create_chat(&agent, directory.path()).await;
        let completed = agent
            .submit_user_shell(chat_id, "printf first".into(), false)
            .await
            .unwrap();
        loop {
            let snapshot = agent.snapshot(chat_id).await.unwrap();
            if matches!(
                snapshot.user_shells[0].state,
                UserShellState::Completed | UserShellState::Failed | UserShellState::Unknown
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let older_active = agent
            .submit_user_shell(chat_id, "sleep 5".into(), true)
            .await
            .unwrap();
        let newer_active = agent
            .submit_user_shell(chat_id, "sleep 5".into(), true)
            .await
            .unwrap();
        let center = ShellTaskCenter::new(agent.clone());
        let first = center.snapshot(chat_id).await.unwrap();
        let second = center.snapshot(chat_id).await.unwrap();
        assert_eq!(first.live_revision, second.live_revision);
        assert_eq!(first.tasks[0].id, newer_active.command_id);
        assert_eq!(first.tasks[1].id, older_active.command_id);
        assert_eq!(first.tasks[2].id, completed.command_id);
        assert!(first.tasks[2].state.is_terminal());
        let stopped = center
            .dispatch(chat_id, newer_active.command_id, ShellTaskAction::Stop)
            .await
            .unwrap();
        assert_eq!(stopped.id, newer_active.command_id);
        assert_eq!(stopped.owner, ShellTaskOwner::User { chat_id });
        assert!(stopped.state.is_terminal());
        center
            .dispatch(chat_id, older_active.command_id, ShellTaskAction::Stop)
            .await
            .unwrap();
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn agent_commands_retain_their_goal_owner() {
        let directory = tempfile::tempdir().unwrap();
        let agent = agent();
        let chat_id = create_chat(&agent, directory.path()).await;
        let loop_id = Uuid::new_v4();
        let command_id = Uuid::new_v4();
        let chat = agent.snapshot(chat_id).await.unwrap();
        let tasks = project_tasks(
            &chat,
            vec![CommandSnapshot {
                updated_at_ms: None,
                operation_id: command_id,
                loop_id,
                command: "pwd".into(),
                description: None,
                cwd: directory.path().into(),
                pid: Some(123),
                state: CommandState::Running,
                was_background: false,
                backgrounded_on_timeout: false,
                exit_code: None,
                signal: None,
                reason: None,
                output_ref: directory.path().join("output.log"),
                output_bytes: 7,
            }],
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].origin, ShellTaskOrigin::Agent);
        assert_eq!(
            tasks[0].owner,
            ShellTaskOwner::Agent {
                loop_id: loop_id.into()
            }
        );
        assert!(tasks[0].capabilities.can_background);
        assert!(tasks[0].capabilities.can_stop);
        assert!(tasks[0].capabilities.can_read_output);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn output_limit_is_validated_before_runtime_access() {
        let directory = tempfile::tempdir().unwrap();
        let agent = agent();
        let chat_id = create_chat(&agent, directory.path()).await;
        let task_id = Uuid::new_v4();
        let center = ShellTaskCenter::new(agent.clone());
        assert_eq!(
            center
                .read_output(chat_id, task_id, 0, SHELL_TASK_OUTPUT_MAX_BYTES + 1)
                .await,
            Err(ShellTaskCenterError::InvalidOutputRead {
                task_id,
                reason: format!("limit must be between 1 and {SHELL_TASK_OUTPUT_MAX_BYTES} bytes"),
            })
        );
        agent.shutdown().await;
    }

    #[test]
    fn malformed_output_metadata_is_rejected() {
        let task_id = Uuid::new_v4();
        let result = ToolResult {
            content: serde_json::json!({
                "output": "abc",
                "encoding": "utf-8-lossy",
                "total_bytes": 3,
                "offset": 2,
                "truncated": true,
                "next_offset": 4,
            })
            .to_string(),
            is_error: false,
            output_ref: None,
        };
        assert_eq!(
            parse_output_chunk(task_id, 2, 10, result),
            Err(ShellTaskCenterError::InvalidOutput(
                "offset metadata is inconsistent".into()
            ))
        );
    }

    #[test]
    fn nonprogressing_and_oversized_output_pages_are_rejected() {
        let task_id = Uuid::new_v4();
        for (requested_offset, content) in [
            (
                2,
                serde_json::json!({
                    "output": "abc",
                    "encoding": "utf-8-lossy",
                    "total_bytes": 5,
                    "offset": 2,
                    "truncated": true,
                    "next_offset": 2,
                })
                .to_string(),
            ),
            (
                0,
                serde_json::json!({
                    "output": "x".repeat(31),
                    "encoding": "utf-8-lossy",
                    "total_bytes": 10,
                    "offset": 0,
                    "truncated": false,
                    "next_offset": null,
                })
                .to_string(),
            ),
        ] {
            let result = ToolResult {
                content,
                is_error: false,
                output_ref: None,
            };
            assert_eq!(
                parse_output_chunk(task_id, requested_offset, 10, result),
                Err(ShellTaskCenterError::InvalidOutput(
                    "offset metadata is inconsistent".into()
                ))
            );
        }
    }
}
