use super::*;
use tickloop_agent_protocol::{
    CommandSnapshot, CommandState, EventKind, OperationState, Receipt, RunId, TaskNotification,
    TaskNotificationStatus, ToolResult,
};
use tickloop_agent_tools::{CANCELLED_COMMAND_REASON, Tools};

impl Agent {
    pub async fn commands(&self, chat_id: ChatId) -> Result<Vec<CommandSnapshot>, AgentError> {
        let (tools, mut recorded) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            (chat.tools.clone(), chat.snapshot.commands.clone())
        };
        for command in tools.commands().await {
            recorded.retain(|existing| existing.operation_id != command.operation_id);
            recorded.push(command);
        }
        Ok(recorded)
    }

    pub async fn background_command(
        &self,
        chat_id: ChatId,
        operation_id: Uuid,
    ) -> Result<CommandSnapshot, AgentError> {
        let tools = self.chat_tools(chat_id).await?;
        let command = tools
            .background(operation_id)
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        self.record_command(chat_id, command).await
    }

    pub async fn stop_command(
        &self,
        chat_id: ChatId,
        operation_id: Uuid,
    ) -> Result<CommandSnapshot, AgentError> {
        let tools = self.chat_tools(chat_id).await?;
        let is_foreground = tools.commands().await.iter().any(|command| {
            command.operation_id == operation_id && command.state == CommandState::Running
        });
        // Stopping a foreground command awaited by a tool interrupts its run; the result does not return to the model.
        let interrupted_run = if is_foreground {
            self.mark_run_awaiting_command_cancelled(chat_id, operation_id)
                .await?
        } else {
            None
        };
        let command = match tools.stop_command(operation_id).await {
            Ok(command) => command,
            Err(error) => {
                if let Some(run_id) = interrupted_run
                    && let Some(chat) = self.inner.chats.lock().await.get_mut(&chat_id)
                {
                    chat.user_cancelled_runs.remove(&run_id);
                }
                return Err(AgentError::Invalid(error.to_string()));
            }
        };
        self.record_command(chat_id, command).await
    }

    async fn mark_run_awaiting_command_cancelled(
        &self,
        chat_id: ChatId,
        operation_id: Uuid,
    ) -> Result<Option<RunId>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let Some(run_id) = chat
            .snapshot
            .operations
            .iter()
            .find(|operation| operation.id == operation_id)
            .map(|operation| operation.run_id)
            .filter(|run_id| chat.active_run == Some(*run_id))
        else {
            return Ok(None);
        };
        Ok(chat.user_cancelled_runs.insert(run_id).then_some(run_id))
    }

    pub async fn read_command(
        &self,
        chat_id: ChatId,
        operation_id: Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, AgentError> {
        let (tools, archived) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            (
                chat.tools.clone(),
                chat.snapshot
                    .commands
                    .iter()
                    .find(|command| command.operation_id == operation_id)
                    .cloned(),
            )
        };
        let result = if tools
            .commands()
            .await
            .iter()
            .any(|command| command.operation_id == operation_id)
        {
            tools.read_command(operation_id, offset, limit).await
        } else {
            Tools::read_archived_command(&archived.ok_or(AgentError::NotFound)?, offset, limit)
        };
        result.map_err(|error| AgentError::Invalid(error.to_string()))
    }

    /// The live state of the command a Bash call started, found through the call's operation.
    pub(super) async fn command_state_for_call(
        &self,
        receipt: &Receipt,
        call_id: &str,
    ) -> Option<CommandState> {
        let (tools, operation_id) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id)?;
            let operation_id = chat
                .snapshot
                .operations
                .iter()
                .find(|operation| {
                    operation.run_id == receipt.run_id && operation.call_id == call_id
                })?
                .id;
            (chat.tools.clone(), operation_id)
        };
        tools.command_state(operation_id).await
    }

    async fn chat_tools(&self, chat_id: ChatId) -> Result<Arc<Tools>, AgentError> {
        Ok(self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .tools
            .clone())
    }

    pub(super) async fn watch_commands(&self, chat_id: ChatId, tools: Arc<Tools>) {
        let mut updates = tools.subscribe_commands();
        let agent = self.clone();
        self.inner.tasks.lock().await.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=agent.inner.shutdown.cancelled()=>break,
                    update=updates.recv()=>match update {
                        Ok(command)=>agent.record_watched_command(chat_id,command).await,
                        Err(broadcast::error::RecvError::Lagged(_))=>{for command in tools.commands().await {agent.record_watched_command(chat_id,command).await;}},
                        Err(broadcast::error::RecvError::Closed)=>break,
                    }
                }
            }
        }));
    }

    async fn record_watched_command(&self, chat_id: ChatId, command: CommandSnapshot) {
        let _upgrade = self.inner.upgrade_gate.read().await;
        loop {
            match self.record_command(chat_id, command.clone()).await {
                Ok(_) => return,
                Err(AgentError::Storage(_)) => {
                    tokio::select! {
                        _ = self.inner.shutdown.cancelled() => return,
                        _ = tokio::time::sleep(super::SHELL_STATE_RETRY_DELAY) => {}
                    }
                }
                Err(_) => return,
            }
        }
    }

    pub(super) async fn record_command(
        &self,
        chat_id: ChatId,
        incoming: CommandSnapshot,
    ) -> Result<CommandSnapshot, AgentError> {
        let tools = self.chat_tools(chat_id).await?;
        let result = if matches!(
            incoming.state,
            CommandState::Completed | CommandState::Failed
        ) {
            tools.finished_command_result(incoming.operation_id).await
        } else {
            None
        };
        let is_stopped_by_model = tools
            .is_command_stopped_by_model(incoming.operation_id)
            .await;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let mut snapshot = chat.snapshot.clone();
        let previous = snapshot
            .commands
            .iter()
            .find(|command| command.operation_id == incoming.operation_id)
            .cloned();
        let command = merge_command_snapshot(previous.as_ref(), incoming);
        let needs_task_notification = command.was_background
            && !is_stopped_by_model
            && command_state_is_terminal(&command.state)
            && previous
                .as_ref()
                .is_none_or(|previous| !command_state_is_terminal(&previous.state));
        snapshot
            .commands
            .retain(|existing| existing.operation_id != command.operation_id);
        snapshot.commands.push(command.clone());
        let operation = snapshot
            .operations
            .iter_mut()
            .find(|operation| operation.id == command.operation_id)
            .ok_or(AgentError::NotFound)?;
        operation.state = match command.state {
            CommandState::Completed => OperationState::Completed,
            CommandState::Failed => OperationState::Failed,
            CommandState::Unknown => OperationState::Unknown,
            _ => OperationState::Admitted,
        };
        if let Some(result) = result {
            operation.result = Some(result);
        }
        let receipt = Receipt {
            chat_id,
            loop_id: operation.loop_id,
            run_id: operation.run_id,
        };
        let tool_use_id = operation.call_id.clone();
        self.save(&snapshot);
        chat.snapshot = snapshot;
        let mut continues_goal = false;
        if needs_task_notification && !self.inner.closed.load(Ordering::SeqCst) {
            let notification = TaskNotification::for_background_command(
                chat_id,
                command.operation_id,
                Some(tool_use_id),
                command.output_ref.clone(),
                command
                    .description
                    .as_deref()
                    .filter(|description| !description.is_empty())
                    .unwrap_or(&command.command),
                notification_status(&command.state, command.reason.as_deref()),
                command.exit_code,
            );
            continues_goal =
                self.notify_task_completion(chat, Some(receipt.loop_id), notification)?;
        }
        self.emit(
            chat,
            &receipt,
            EventKind::CommandUpdated {
                command: command.clone(),
            },
        );
        drop(chats);
        if command.was_background
            && !command_state_is_terminal(&command.state)
            && !super::shell_description::has_description(command.description.as_deref())
        {
            self.describe_background_shell(chat_id, command.operation_id, &command.command)
                .await;
        }
        if continues_goal {
            self.start_next_agent_run(chat_id).await?;
        }
        Ok(command)
    }
}

fn merge_command_snapshot(
    existing: Option<&CommandSnapshot>,
    mut incoming: CommandSnapshot,
) -> CommandSnapshot {
    let Some(existing) = existing else {
        incoming
            .updated_at_ms
            .get_or_insert_with(tickloop_agent_base::private_fs::unix_time_milliseconds);
        return incoming;
    };
    if command_state_is_terminal(&existing.state) {
        let mut authoritative = existing.clone();
        authoritative.was_background |= incoming.was_background;
        authoritative.backgrounded_on_timeout |= incoming.backgrounded_on_timeout;
        if incoming.output_bytes > authoritative.output_bytes {
            authoritative.updated_at_ms = incoming
                .updated_at_ms
                .or_else(|| Some(tickloop_agent_base::private_fs::unix_time_milliseconds()));
        }
        authoritative.output_bytes = authoritative.output_bytes.max(incoming.output_bytes);
        return authoritative;
    }
    if incoming.updated_at_ms.is_none() {
        incoming.updated_at_ms =
            if incoming.state != existing.state || incoming.output_bytes > existing.output_bytes {
                Some(tickloop_agent_base::private_fs::unix_time_milliseconds())
            } else {
                existing.updated_at_ms
            };
    }
    incoming.was_background |= existing.was_background;
    incoming.backgrounded_on_timeout |= existing.backgrounded_on_timeout;
    incoming.output_bytes = incoming.output_bytes.max(existing.output_bytes);
    incoming
}

fn command_state_is_terminal(state: &CommandState) -> bool {
    matches!(
        state,
        CommandState::Completed | CommandState::Failed | CommandState::Unknown
    )
}

/// 照 Claude Code 的后台任务状态：用户停止记为 killed，其余非正常结束记为 failed。
pub(super) fn notification_status(
    state: &CommandState,
    reason: Option<&str>,
) -> TaskNotificationStatus {
    match state {
        CommandState::Completed => TaskNotificationStatus::Completed,
        _ if reason == Some(CANCELLED_COMMAND_REASON) => TaskNotificationStatus::Killed,
        _ => TaskNotificationStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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

    fn model_config() -> ModelConfig {
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
        }
    }

    #[test]
    fn legacy_agent_command_defaults_to_foreground_history() {
        let mut value = serde_json::json!({
            "operation_id": Uuid::new_v4(),
            "loop_id": Uuid::new_v4(),
            "command": "printf legacy",
            "cwd": "/tmp",
            "pid": null,
            "state": "Completed",
            "exit_code": 0,
            "signal": null,
            "reason": null,
            "output_ref": "/tmp/legacy.log",
            "output_bytes": 6
        });
        value.as_object_mut().unwrap().remove("was_background");
        let command: CommandSnapshot = serde_json::from_value(value).unwrap();
        assert!(!command.was_background);
    }

    #[tokio::test]
    async fn agent_terminal_updates_notify_once_and_keep_the_latest_fact() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(model_config(), Arc::new(UnusedModel));
        let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
        let operation_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        {
            let mut chats = agent.inner.chats.lock().await;
            chats
                .get_mut(&chat_id)
                .unwrap()
                .snapshot
                .operations
                .push(OperationRecord {
                    task_binding: None,
                    id: operation_id,
                    loop_id,
                    run_id: Uuid::new_v4(),
                    call_id: "shell-call".into(),
                    name: tickloop_agent_tools::BASH_TOOL_NAME.into(),
                    input: serde_json::json!({"command":"printf agent"}),
                    file_change: None,
                    state: OperationState::Admitted,
                    result: None,
                });
        }
        let mut command = CommandSnapshot {
            updated_at_ms: None,
            operation_id,
            loop_id,
            command: "printf agent".into(),
            description: Some("Print the agent marker".into()),
            cwd: directory.path().into(),
            pid: Some(123),
            state: CommandState::Completed,
            was_background: true,
            backgrounded_on_timeout: false,
            exit_code: Some(0),
            signal: None,
            reason: None,
            output_ref: directory.path().join("agent.log"),
            output_bytes: 5,
        };
        let notifications = |snapshot: &ChatSnapshot| {
            snapshot
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|content| match content {
                    Content::Text { text } if text.starts_with("<task-notification>") => {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        agent
            .record_command(chat_id, command.clone())
            .await
            .unwrap();
        let recorded = notifications(&agent.snapshot(chat_id).await.unwrap());
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].contains(
                "<summary>Background command \"Print the agent marker\" completed (exit code 0)</summary>"
            ),
            "{}",
            recorded[0]
        );
        command.output_bytes = 12;
        agent
            .record_command(chat_id, command.clone())
            .await
            .unwrap();
        command.state = CommandState::Background;
        command.exit_code = None;
        command.output_ref = directory.path().join("stale.log");
        command.output_bytes = 1;
        let authoritative = agent.record_command(chat_id, command).await.unwrap();
        assert_eq!(authoritative.state, CommandState::Completed);
        assert_eq!(authoritative.exit_code, Some(0));
        assert_eq!(authoritative.output_bytes, 12);
        assert_eq!(authoritative.output_ref, directory.path().join("agent.log"));
        assert!(authoritative.was_background);
        assert_eq!(
            notifications(&agent.snapshot(chat_id).await.unwrap()).len(),
            1
        );
        agent.shutdown().await;
    }
}
