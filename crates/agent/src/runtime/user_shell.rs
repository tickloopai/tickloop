use super::{
    Agent, AgentError, Chat, ChatId, MessageBoundary, TaskNotification, UserShellReceipt,
    UserShellRecord, UserShellState, commands::notification_status, local_messages,
};
use crate::model::Message;
use crate::tools::{
    CANCELLED_COMMAND_REASON, CommandState, ToolResult, Tools, UserCommandSnapshot,
};
use tokio::sync::broadcast;
use uuid::Uuid;

const CONTEXT_OUTPUT_BYTES: usize = 16 * 1024;

struct UserShellOutput {
    text: String,
    is_truncated: bool,
}

impl Agent {
    pub fn subscribe_user_shells(&self) -> broadcast::Receiver<UserShellRecord> {
        self.inner.user_shell_events.subscribe()
    }

    pub async fn submit_user_shell_for_goal(
        &self,
        chat_id: ChatId,
        loop_id: Option<super::LoopId>,
        command: String,
        background: bool,
    ) -> Result<UserShellReceipt, AgentError> {
        if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AgentError::Invalid("agent is shut down".into()));
        }
        if command.trim().is_empty() || command.contains('\0') {
            return Err(AgentError::Invalid(
                "shell command cannot be empty or contain NUL".into(),
            ));
        }
        let command_id = Uuid::new_v4();
        let (tools, should_watch, record) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            if loop_id
                .is_some_and(|loop_id| !chat.snapshot.loops.iter().any(|goal| goal.id == loop_id))
            {
                return Err(AgentError::NotFound);
            }
            let mut snapshot = chat.snapshot.clone();
            snapshot.last_queue_sequence += 1;
            let record = UserShellRecord {
                id: command_id,
                chat_id,
                loop_id,
                command: command.clone(),
                cwd: snapshot.workdir.clone(),
                starts_in_background: background,
                queued_sequence: snapshot.last_queue_sequence,
                armed: true,
                preceding_model_runs: snapshot
                    .pending_inputs
                    .iter()
                    .map(|pending| pending.receipt.run_id)
                    .collect(),
                state: UserShellState::Queued,
                pid: None,
                exit_code: None,
                signal: None,
                reason: None,
                output_ref: None,
                output_bytes: 0,
                submitted_at_message: snapshot.messages.len(),
            };
            snapshot.user_shells.push(record.clone());
            self.save(&snapshot);
            chat.snapshot = snapshot;
            let should_watch = !chat.watching_user_shells;
            chat.watching_user_shells = true;
            (chat.tools.clone(), should_watch, record)
        };
        let _ = self.inner.user_shell_events.send(record.clone());
        if should_watch {
            self.watch_user_shells(chat_id, tools.clone()).await;
        }
        self.start_user_shell_queue(chat_id).await?;
        Ok(UserShellReceipt {
            chat_id,
            command_id,
        })
    }

    pub async fn background_user_shell(
        &self,
        chat_id: ChatId,
        command_id: Uuid,
    ) -> Result<UserShellRecord, AgentError> {
        if let Some(record) = self
            .update_queued_user_shell(chat_id, command_id, |record| {
                record.starts_in_background = true;
            })
            .await?
        {
            self.start_user_shell_queue(chat_id).await?;
            return Ok(record);
        }
        let tools = self.user_shell_tools(chat_id, command_id).await?;
        let command = tools
            .background_user_shell(command_id, chat_id)
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let record = self.record_user_shell(chat_id, command).await?;
        self.start_user_shell_queue(chat_id).await?;
        Ok(record)
    }

    pub async fn stop_user_shell(
        &self,
        chat_id: ChatId,
        command_id: Uuid,
    ) -> Result<UserShellRecord, AgentError> {
        if let Some(record) = self
            .update_queued_user_shell(chat_id, command_id, |record| {
                record.state = UserShellState::Failed;
                record.reason = Some("cancelled before execution".into());
            })
            .await?
        {
            self.start_user_shell_queue(chat_id).await?;
            return Ok(record);
        }
        let tools = self.user_shell_tools(chat_id, command_id).await?;
        let command = tools
            .stop_user_shell(command_id, chat_id)
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let record = self.record_user_shell(chat_id, command).await?;
        self.start_user_shell_queue(chat_id).await?;
        Ok(record)
    }

    /// 按操作查看 `!` 命令输出；命令状态只在本进程内保留，重启后不可用。
    pub async fn read_user_shell_output(
        &self,
        chat_id: ChatId,
        command_id: Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, AgentError> {
        let (tools, record) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            let record = chat
                .snapshot
                .user_shells
                .iter()
                .find(|record| record.id == command_id)
                .cloned()
                .ok_or(AgentError::NotFound)?;
            (chat.tools.clone(), record)
        };
        if tools
            .user_commands()
            .await
            .iter()
            .any(|command| command.operation_id == command_id)
        {
            return tools
                .read_user_shell(command_id, chat_id, offset, limit)
                .await
                .map_err(|error| AgentError::Invalid(error.to_string()));
        }
        let snapshot = record_to_command_snapshot(&record).ok_or(AgentError::NotFound)?;
        Tools::read_archived_user_shell(&snapshot, offset, limit)
            .map_err(|error| AgentError::Invalid(error.to_string()))
    }

    async fn user_shell_tools(
        &self,
        chat_id: ChatId,
        command_id: Uuid,
    ) -> Result<std::sync::Arc<Tools>, AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        if !chat
            .snapshot
            .user_shells
            .iter()
            .any(|record| record.id == command_id && record.chat_id == chat_id)
        {
            return Err(AgentError::NotFound);
        }
        Ok(chat.tools.clone())
    }

    async fn watch_user_shells(&self, chat_id: ChatId, tools: std::sync::Arc<Tools>) {
        let queue_ready = {
            let chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get(&chat_id) else {
                return;
            };
            chat.user_shell_queue.clone()
        };
        let mut updates = tools.subscribe_user_shells();
        let agent = self.clone();
        self.inner.tasks.lock().await.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = agent.inner.shutdown.cancelled() => break,
                    _ = queue_ready.notified() => {
                        let _ = agent.start_user_shell_queue(chat_id).await;
                    }
                    update = updates.recv() => match update {
                        Ok(command) => {
                            let releases_slot = matches!(command.state, CommandState::Background | CommandState::Completed | CommandState::Failed | CommandState::Unknown);
                            if agent.record_watched_user_shell(chat_id, command).await && releases_slot {
                                let _ = agent.start_user_shell_queue(chat_id).await;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            for command in tools.user_commands().await {
                                agent.record_watched_user_shell(chat_id, command).await;
                            }
                            let _ = agent.start_user_shell_queue(chat_id).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }));
    }

    async fn record_watched_user_shell(
        &self,
        chat_id: ChatId,
        command: UserCommandSnapshot,
    ) -> bool {
        loop {
            match self.record_user_shell(chat_id, command.clone()).await {
                Ok(_) => return true,
                Err(AgentError::Storage(_)) => {
                    tokio::select! {
                        _ = self.inner.shutdown.cancelled() => return false,
                        _ = tokio::time::sleep(super::SHELL_STATE_RETRY_DELAY) => {}
                    }
                }
                Err(_) => return false,
            }
        }
    }

    pub(super) async fn start_user_shell_queue(&self, chat_id: ChatId) -> Result<(), AgentError> {
        loop {
            let next = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst)
                    || chat.active.is_some()
                    || !chat.parked.is_empty()
                    || chat.has_foreground_user_shell()
                {
                    return Ok(());
                }
                let command_barrier = chat
                    .snapshot
                    .pending_commands
                    .iter()
                    .map(|command| command.queued_sequence)
                    .min();
                if let Some(index) = chat.snapshot.user_shells.iter().position(|record| {
                    record.state == UserShellState::Queued
                        && record.armed
                        && command_barrier.is_none_or(|sequence| record.queued_sequence < sequence)
                }) {
                    let has_preceding_model_run =
                        chat.snapshot.pending_inputs.iter().any(|pending| {
                            chat.snapshot.user_shells[index]
                                .preceding_model_runs
                                .contains(&pending.receipt.run_id)
                        });
                    if has_preceding_model_run {
                        None
                    } else {
                        // 命令输入在结束或转入后台时与结果一起写入；此前暂存的消息先写出，保持先后顺序。
                        self.flush_deferred_messages(chat, MessageBoundary::Idle)?;
                        let record = &mut chat.snapshot.user_shells[index];
                        record.state = UserShellState::Running;
                        Some((chat.tools.clone(), record.clone()))
                    }
                } else {
                    None
                }
            };
            let Some((tools, record)) = next else {
                return self.start_next_agent_run(chat_id).await;
            };
            let _ = self.inner.user_shell_events.send(record.clone());
            let result = tools
                .execute_user_shell(
                    &record.cwd,
                    record.command.clone(),
                    record.id,
                    chat_id,
                    record.starts_in_background,
                    self.inner.shutdown.child_token(),
                )
                .await;
            if let Err(error) = result {
                self.fail_user_shell(chat_id, record.id, error.to_string())
                    .await?;
                continue;
            }
            return Ok(());
        }
    }

    pub(super) async fn start_next_agent_run(&self, chat_id: ChatId) -> Result<(), AgentError> {
        let next = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            self.next_run(chat)?
        };
        if let Some(next) = next {
            self.inner.tasks.lock().await.push(self.run_task(next));
        }
        Ok(())
    }

    async fn update_queued_user_shell(
        &self,
        chat_id: ChatId,
        command_id: Uuid,
        update: impl FnOnce(&mut UserShellRecord),
    ) -> Result<Option<UserShellRecord>, AgentError> {
        let record =
            {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                let Some(record) = chat.snapshot.user_shells.iter_mut().find(|record| {
                    record.id == command_id && record.state == UserShellState::Queued
                }) else {
                    return Ok(None);
                };
                update(record);
                record.clone()
            };
        let _ = self.inner.user_shell_events.send(record.clone());
        Ok(Some(record))
    }

    pub(super) async fn record_user_shell(
        &self,
        chat_id: ChatId,
        command: UserCommandSnapshot,
    ) -> Result<UserShellRecord, AgentError> {
        let output = if matches!(
            command.state,
            CommandState::Completed | CommandState::Failed | CommandState::Unknown
        ) {
            self.read_live_user_shell_output(chat_id, &command).await
        } else {
            None
        };
        let (record, continues_goal) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let record = chat
                .snapshot
                .user_shells
                .iter_mut()
                .find(|record| record.id == command.operation_id)
                .ok_or(AgentError::NotFound)?;
            let previous = record.clone();
            record.starts_in_background |= command.was_background;
            if user_shell_state_is_terminal(&previous.state) {
                record.output_bytes = record.output_bytes.max(command.output_bytes);
                (record.clone(), false)
            } else {
                record.state = map_state(&command.state);
                record.pid = command.pid;
                record.exit_code = command.exit_code;
                record.signal = command.signal;
                record.reason = command.reason.clone();
                record.output_ref = Some(command.output_ref.clone());
                record.output_bytes = command.output_bytes;
                let record = record.clone();
                let continues_goal = if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst)
                {
                    false
                } else {
                    self.record_user_shell_messages(chat, &previous, &record, &command, output)?
                };
                (record, continues_goal)
            }
        };
        let _ = self.inner.user_shell_events.send(record.clone());
        if continues_goal {
            self.start_next_agent_run(chat_id).await?;
        }
        Ok(record)
    }

    /// Records `!` command transitions as conversation messages: foreground results or
    /// interruption, background start, then completion notice. Returns whether to continue its Loop.
    fn record_user_shell_messages(
        &self,
        chat: &mut Chat,
        previous: &UserShellRecord,
        current: &UserShellRecord,
        command: &UserCommandSnapshot,
        output: Option<UserShellOutput>,
    ) -> Result<bool, AgentError> {
        let was_announced_as_background = has_announced_background(previous);
        let input = local_messages::bash_input(&current.command);
        let announcement = local_messages::background_started(
            current.id,
            &command.output_ref,
            !previous.starts_in_background && !command.backgrounded_on_timeout,
        );
        if current.state == UserShellState::Background {
            if !was_announced_as_background {
                self.append_foreground_shell_messages(chat, [input, announcement])?;
            }
            return Ok(false);
        }
        if !user_shell_state_is_terminal(&current.state) {
            return Ok(false);
        }
        if was_announced_as_background || command.was_background {
            if !was_announced_as_background {
                self.append_foreground_shell_messages(chat, [input, announcement])?;
            }
            let notification = TaskNotification::for_background_command(
                current.chat_id,
                current.id,
                None,
                command.output_ref.clone(),
                &current.command,
                notification_status(&command.state, command.reason.as_deref()),
                current.exit_code,
            );
            return self.notify_task_completion(chat, current.loop_id, notification);
        }
        self.append_foreground_shell_messages(chat, [input, foreground_result(current, output)])?;
        Ok(false)
    }

    async fn read_live_user_shell_output(
        &self,
        chat_id: ChatId,
        command: &UserCommandSnapshot,
    ) -> Option<UserShellOutput> {
        let tools = {
            let chats = self.inner.chats.lock().await;
            chats.get(&chat_id)?.tools.clone()
        };
        let result = tools
            .read_user_shell(command.operation_id, chat_id, 0, CONTEXT_OUTPUT_BYTES)
            .await
            .ok()?;
        let value = serde_json::from_str::<serde_json::Value>(&result.content).ok()?;
        Some(UserShellOutput {
            text: value.get("output")?.as_str()?.to_owned(),
            is_truncated: value.get("truncated")?.as_bool()?,
        })
    }

    async fn fail_user_shell(
        &self,
        chat_id: ChatId,
        command_id: Uuid,
        reason: String,
    ) -> Result<(), AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let record = chat
            .snapshot
            .user_shells
            .iter_mut()
            .find(|record| record.id == command_id)
            .ok_or(AgentError::NotFound)?;
        record.state = UserShellState::Failed;
        record.reason = Some(reason.clone());
        let record = record.clone();
        self.append_foreground_shell_messages(
            chat,
            [
                local_messages::bash_input(&record.command),
                local_messages::bash_output("", &format!("Command failed: {reason}")),
            ],
        )?;
        let _ = self.inner.user_shell_events.send(record);
        Ok(())
    }
}

fn foreground_result(record: &UserShellRecord, output: Option<UserShellOutput>) -> Message {
    if record.reason.as_deref() == Some(CANCELLED_COMMAND_REASON) {
        return local_messages::interruption(false);
    }
    let mut text = output
        .as_ref()
        .map_or("", |output| output.text.trim_end())
        .to_owned();
    if let (Some(output), Some(output_ref)) = (&output, &record.output_ref)
        && output.is_truncated
    {
        text.push_str(&format!(
            "\n[output truncated; full output: {}]",
            output_ref.display()
        ));
    }
    if record.state == UserShellState::Completed {
        return local_messages::bash_output(&text, "");
    }
    let failure = match (record.exit_code, &record.reason) {
        (Some(code), _) => Some(format!("Exit code {code}")),
        (None, Some(reason)) => Some(reason.clone()),
        (None, None) => record.signal.map(|signal| format!("Signal {signal}")),
    };
    let stderr = [Some(text), failure]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    local_messages::bash_output("", &stderr)
}

/// 转入后台时已写过后台提示：记录处于后台，或从后台进入停止中。
fn has_announced_background(record: &UserShellRecord) -> bool {
    record.state == UserShellState::Background
        || (record.state == UserShellState::Cancelling && record.starts_in_background)
}

fn map_state(state: &CommandState) -> UserShellState {
    match state {
        CommandState::Running => UserShellState::Running,
        CommandState::Background => UserShellState::Background,
        CommandState::Cancelling => UserShellState::Cancelling,
        CommandState::Completed => UserShellState::Completed,
        CommandState::Failed => UserShellState::Failed,
        CommandState::Unknown => UserShellState::Unknown,
    }
}

fn user_shell_state_is_terminal(state: &UserShellState) -> bool {
    matches!(
        state,
        UserShellState::Completed | UserShellState::Failed | UserShellState::Unknown
    )
}

fn record_to_command_snapshot(record: &UserShellRecord) -> Option<UserCommandSnapshot> {
    Some(UserCommandSnapshot {
        operation_id: record.id,
        chat_id: record.chat_id,
        command: record.command.clone(),
        cwd: record.cwd.clone(),
        pid: record.pid,
        state: match record.state {
            UserShellState::Queued | UserShellState::Running => CommandState::Running,
            UserShellState::Background => CommandState::Background,
            UserShellState::Cancelling => CommandState::Cancelling,
            UserShellState::Completed => CommandState::Completed,
            UserShellState::Failed => CommandState::Failed,
            UserShellState::Unknown => CommandState::Unknown,
        },
        was_background: record.starts_in_background,
        backgrounded_on_timeout: false,
        exit_code: record.exit_code,
        signal: record.signal,
        reason: record.reason.clone(),
        output_ref: record.output_ref.clone()?,
        output_bytes: record.output_bytes,
    })
}
