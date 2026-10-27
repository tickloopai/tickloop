use super::{
    Agent, AgentError, Chat, MessageBoundary, UserShellRecordExt, commands::notification_status,
};
use tickloop_harness_protocol::local_messages;
use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, CommandState, Message, PermissionExecution, Receipt, TaskNotification,
    ToolResult, UserShellReceipt, UserShellRecord, UserShellState,
};
use tickloop_harness_tools::{CANCELLED_COMMAND_REASON, Tools, UserCommandSnapshot};
use tokio::sync::broadcast;
use uuid::Uuid;

const CONTEXT_OUTPUT_BYTES: usize = 16 * 1024;

struct UserShellOutput {
    text: String,
    is_truncated: bool,
}

struct UserShellAuthorization {
    receipt: Receipt,
    owner: super::session_permissions::PermissionOwner,
    operation: super::session_permissions::SessionPermissionOperation,
    evaluation: super::session_permissions::SessionPermissionEvaluation,
    ancestor_revisions: Vec<(ChatId, u64)>,
    policy_revision: u64,
}

impl Agent {
    pub(super) fn take_user_shell_cancellation(
        &self,
        chat_id: ChatId,
        command_id: Uuid,
    ) -> Option<tokio_util::sync::CancellationToken> {
        self.inner
            .user_shell_cancellations
            .lock()
            .unwrap()
            .remove(&(chat_id, command_id))
    }

    /// Withdraws the permission requests of the user Shells `is_withdrawn` selects. The caller
    /// holds the Chat.
    pub(super) async fn withdraw_user_shell_requests(
        &self,
        snapshot: &mut ChatSnapshot,
        is_withdrawn: impl Fn(Uuid) -> bool,
    ) {
        let requests = snapshot
            .permissions
            .iter()
            .filter(|request| {
                matches!(
                    request.execution,
                    PermissionExecution::UserShell { command_id } if is_withdrawn(command_id)
                )
            })
            .map(|request| request.id)
            .collect::<Vec<_>>();
        snapshot
            .permissions
            .retain(|request| !requests.contains(&request.id));
        let mut approvals = self.inner.approvals.lock().await;
        for request in requests {
            approvals.remove(&request);
        }
    }

    pub fn subscribe_user_shells(&self) -> broadcast::Receiver<UserShellRecord> {
        self.inner.user_shell_events.subscribe()
    }

    pub async fn submit_user_shell(
        &self,
        chat_id: ChatId,
        command: String,
        background: bool,
    ) -> Result<UserShellReceipt, AgentError> {
        if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst)
            || self
                .inner
                .upgrade_paused
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(AgentError::Invalid("agent is shut down".into()));
        }
        if command.trim().is_empty() || command.contains('\0') {
            return Err(AgentError::Invalid(
                "shell command cannot be empty or contain NUL".into(),
            ));
        }
        let command_id = tickloop_ids::generate();
        let (tools, should_watch, record) = {
            let mut chats = self.inner.chats.lock().await;
            if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst)
                || self
                    .inner
                    .upgrade_paused
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(AgentError::Invalid(
                    "Agent is unavailable during a planned upgrade".into(),
                ));
            }
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let folder = super::require_folder(&chat.snapshot)?.to_path_buf();
            // A `!` command belongs to the Loop once it has worked, or always in a delegated
            // Chat, so its background completion goes on with that work; before the first turn
            // it only records its completion.
            let chat_loop = &chat.snapshot.chat_loop;
            let loop_id = (chat.snapshot.delegation.is_some() || chat_loop.run_id.is_some())
                .then_some(chat_loop.id);
            let mut snapshot = chat.snapshot.clone();
            snapshot.last_queue_sequence += 1;
            let record = UserShellRecord {
                updated_at_ms: Some(tickloop_harness_base::private_fs::unix_time_milliseconds()),
                id: command_id,
                chat_id,
                loop_id,
                command: command.clone(),
                cwd: folder,
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
                state_since: Some(tickloop_harness_base::private_fs::unix_time_milliseconds()),
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
            .background_user_shell(command_id, chat_id.into())
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
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .and_then(|chat| {
                chat.snapshot.user_shells.iter().find(|record| {
                    record.id == command_id && user_shell_state_is_terminal(&record.state)
                })
            })
            .cloned()
        {
            return Ok(record);
        }
        if let Some(cancel) = self.take_user_shell_cancellation(chat_id, command_id) {
            cancel.cancel();
        }
        let waiting = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            if let Some(record) = chat
                .snapshot
                .user_shells
                .iter_mut()
                .find(|record| record.id == command_id && record.awaits_admission())
            {
                record.set_state(UserShellState::Failed);
                record.reason = Some("cancelled before Shell permission admission".into());
                let record = record.clone();
                self.withdraw_user_shell_requests(&mut chat.snapshot, |id| id == command_id)
                    .await;
                self.save(&chat.snapshot);
                Some(record)
            } else {
                None
            }
        };
        if let Some(record) = waiting {
            let _ = self.inner.user_shell_events.send(record.clone());
            self.start_user_shell_queue(chat_id).await?;
            return Ok(record);
        }
        if let Some(record) = self
            .update_queued_user_shell(chat_id, command_id, |record| {
                record.set_state(UserShellState::Failed);
                record.reason = Some("cancelled before execution".into());
            })
            .await?
        {
            self.start_user_shell_queue(chat_id).await?;
            return Ok(record);
        }
        let tools = self.user_shell_tools(chat_id, command_id).await?;
        let command = tools
            .stop_user_shell(command_id, chat_id.into())
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let record = self.record_user_shell(chat_id, command).await?;
        self.start_user_shell_queue(chat_id).await?;
        Ok(record)
    }

    /// Views `!` command output by operation; command state is kept only within this process and is
    /// unavailable after a restart.
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
                .read_user_shell(command_id, chat_id.into(), offset, limit)
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

    pub(super) async fn watch_user_shells(&self, chat_id: ChatId, tools: std::sync::Arc<Tools>) {
        let queue_ready = {
            let chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get(&chat_id) else {
                return;
            };
            chat.user_shell_queue.clone()
        };
        let mut updates = tools.subscribe_user_shells();
        let agent = self.clone();
        self.inner.tasks.spawn(async move {
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
        });
    }

    async fn record_watched_user_shell(
        &self,
        chat_id: ChatId,
        command: UserCommandSnapshot,
    ) -> bool {
        let _upgrade = self.inner.upgrade_gate.read().await;
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

    pub(super) fn start_user_shell_queue(
        &self,
        chat_id: ChatId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AgentError>> + Send + '_>>
    {
        Box::pin(async move {
            let next = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst)
                    || self
                        .inner
                        .upgrade_paused
                        .load(std::sync::atomic::Ordering::SeqCst)
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
                        // The command input is written together with the result when it finishes or
                        // moves to the background; messages held before that are written first,
                        // preserving order.
                        self.flush_deferred_messages(chat, MessageBoundary::Idle)?;
                        let record = &mut chat.snapshot.user_shells[index];
                        record.set_state(UserShellState::WaitingPermission);
                        let cancel = self.inner.shutdown.child_token();
                        self.inner
                            .user_shell_cancellations
                            .lock()
                            .unwrap()
                            .insert((chat_id, record.id), cancel.clone());
                        Some((chat.tools.clone(), record.clone(), cancel))
                    }
                } else {
                    None
                }
            };
            let Some((tools, record, cancel)) = next else {
                return self.start_next_agent_run(chat_id).await;
            };
            let _ = self.inner.user_shell_events.send(record.clone());
            let agent = self.clone();
            self.inner.tasks.spawn(async move {
                if let Err(error) = agent
                    .admit_user_shell(chat_id, tools, &record, cancel)
                    .await
                {
                    agent.take_user_shell_cancellation(chat_id, record.id);
                    let _ = agent
                        .fail_user_shell(chat_id, record.id, error.to_string())
                        .await;
                    let _ = agent.start_user_shell_queue(chat_id).await;
                }
            });
            Ok(())
        })
    }

    async fn admit_user_shell(
        &self,
        chat_id: ChatId,
        tools: std::sync::Arc<Tools>,
        record: &UserShellRecord,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), AgentError> {
        use super::session_permissions::{PermissionOwner, SessionPermissionOperation};
        use tickloop_harness_protocol::Decision;
        use tickloop_harness_protocol::{
            PermissionApprovalSource, PermissionExecution, PermissionRequest, Receipt,
        };
        let snapshot = self.snapshot(chat_id).await?;
        let workspace = tickloop_harness_tools::Workspace {
            folder: snapshot.folder.clone(),
            additional_directories: snapshot.additional_directories.clone(),
        };
        let input = serde_json::json!({
            "command": record.command,
            "cwd": record.cwd,
            "run_in_background": record.starts_in_background,
        });
        let ids = self.chat_ids(chat_id).await;
        let prepared = tools
            .prepare_for_operation(
                &ids,
                &workspace,
                tickloop_harness_tools::BASH_TOOL_NAME,
                &input,
                record.id,
            )
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let mut authorization = None;
        if snapshot.delegation.is_some() {
            self.refresh_project_rules(chat_id).await?;
            let loop_id = record
                .loop_id
                .ok_or_else(|| AgentError::Invalid("delegated Shell requires its Loop".into()))?;
            let receipt = Receipt {
                chat_id,
                loop_id,
                run_id: Uuid::nil().into(),
            };
            self.check_delegated_call(&receipt, &prepared.name, &prepared.input, Some(&prepared))
                .await
                .map_err(AgentError::Invalid)?;
            let owner = PermissionOwner::new(chat_id.into(), loop_id.into())
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
            let operation = SessionPermissionOperation::from_prepared(&prepared, &workspace)
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
            let (authority, mut evaluation) = {
                let chats = self.inner.chats.lock().await;
                let sessions = self.inner.session_permissions.lock().await;
                let evaluation = sessions.evaluate(owner, &operation);
                let authority = super::delegation::permission_authority(
                    &chats,
                    &sessions,
                    &receipt,
                    &prepared,
                    evaluation.decision,
                    None,
                )
                .ok_or(AgentError::NotFound)?;
                (authority, evaluation)
            };
            let mut policy_revision = authority.policy_revision;
            let decision = self.apply_prompt_policy_keeping_required_approval(
                authority.decision,
                authority.explicit_ask,
            );
            if decision == Decision::Deny {
                return Err(AgentError::Invalid(
                    "delegated permission authority denies this Shell".into(),
                ));
            }
            if decision != Decision::Allow {
                let request = PermissionRequest {
                    parent_call: None,
                    mcp_server: None,
                    id: tickloop_ids::generate(),
                    loop_id,
                    execution: PermissionExecution::UserShell {
                        command_id: record.id,
                    },
                    name: prepared.name.clone(),
                    input: prepared.input.clone(),
                    file_change: None,
                    approval_source: if authority.explicit_ask {
                        PermissionApprovalSource::ExplicitAsk
                    } else {
                        PermissionApprovalSource::PermissionPolicy
                    },
                    approval_reason: Some(
                        "the inherited Shell permission authority requires approval".into(),
                    ),
                };
                let answer = self
                    .wait_for_permission(chat_id, request, operation.clone(), cancel.clone())
                    .await
                    .map_err(AgentError::Invalid)?;
                if !answer.allow {
                    return Err(AgentError::Invalid(
                        "permission denied; user Shell was not executed".into(),
                    ));
                }
                policy_revision = answer.revision;
                evaluation = self
                    .inner
                    .session_permissions
                    .lock()
                    .await
                    .evaluate(owner, &operation);
            }
            authorization = Some(UserShellAuthorization {
                receipt,
                owner,
                operation,
                evaluation,
                ancestor_revisions: authority.ancestor_revisions,
                policy_revision,
            });
            self.refresh_project_rules(chat_id).await?;
        }
        {
            let mut chats = self.inner.chats.lock().await;
            let sessions = self.inner.session_permissions.lock().await;
            if let Some(authorization) = &authorization {
                super::delegation::check_call(
                    &chats,
                    &sessions,
                    &authorization.receipt,
                    &prepared.name,
                    &prepared.input,
                    Some(&prepared),
                    None,
                )
                .map_err(AgentError::Invalid)?;
                let current = super::delegation::permission_authority(
                    &chats,
                    &sessions,
                    &authorization.receipt,
                    &prepared,
                    authorization.evaluation.decision,
                    None,
                )
                .ok_or(AgentError::NotFound)?;
                if !current.still_admits(
                    authorization.policy_revision,
                    &authorization.ancestor_revisions,
                ) || !sessions.revalidate(
                    &authorization.evaluation.token,
                    authorization.owner,
                    &authorization.operation,
                ) {
                    return Err(AgentError::Invalid(
                        "Shell permission authority changed before dispatch".into(),
                    ));
                }
            }
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let shell = chat
                .snapshot
                .user_shells
                .iter_mut()
                .find(|shell| shell.id == record.id)
                .ok_or(AgentError::NotFound)?;
            if cancel.is_cancelled() || shell.state != UserShellState::WaitingPermission {
                return Err(AgentError::Invalid(
                    "user Shell was cancelled before dispatch".into(),
                ));
            }
            shell.set_state(UserShellState::Running);
            let update = shell.clone();
            self.save(&chat.snapshot);
            let _ = self.inner.user_shell_events.send(update);
        }
        tools
            .execute_user_shell(prepared, record.id, chat_id.into(), cancel)
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        Ok(())
    }

    /// Starts what waits in the Chat; with nothing waiting, its goal is considered again, as
    /// what held its continuation back may have ended.
    pub(super) async fn start_next_agent_run(&self, chat_id: ChatId) -> Result<(), AgentError> {
        match self.next_run(chat_id).await? {
            Some(next) => self.run_task(next),
            None => self.reconsider_goal(chat_id).await,
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
        let (record, continues_goal, ended) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let record = chat
                .snapshot
                .user_shells
                .iter_mut()
                .find(|record| record.id == command.operation_id)
                .ok_or(AgentError::NotFound)?;
            let previous = record.clone();
            if command.output_bytes > record.output_bytes {
                record.updated_at_ms = command
                    .updated_at_ms
                    .or_else(|| Some(tickloop_harness_base::private_fs::unix_time_milliseconds()));
            }
            record.starts_in_background |= command.was_background;
            if user_shell_state_is_terminal(&previous.state) {
                record.output_bytes = record.output_bytes.max(command.output_bytes);
                (record.clone(), false, false)
            } else {
                record.set_state(map_state(&command.state));
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
                let ended = user_shell_state_is_terminal(&record.state);
                (record, continues_goal, ended)
            }
        };
        let _ = self.inner.user_shell_events.send(record.clone());
        // A user's `!` command never has a description of its own.
        if record.starts_in_background && !user_shell_state_is_terminal(&record.state) {
            self.describe_background_shell(chat_id, record.id, &record.command)
                .await;
        }
        // A Shell that ended may have been all a waiting goal continuation waited for.
        if continues_goal || ended {
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
            &chat.ids,
            current.id.into(),
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
        self.take_user_shell_cancellation(current.chat_id, current.id);
        if was_announced_as_background || command.was_background {
            if !was_announced_as_background {
                self.append_foreground_shell_messages(chat, [input, announcement])?;
            }
            let notification = TaskNotification::for_background_command(
                current.chat_id,
                current.id.into(),
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
            .read_user_shell(
                command.operation_id,
                chat_id.into(),
                0,
                CONTEXT_OUTPUT_BYTES,
            )
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
        if user_shell_state_is_terminal(&record.state) {
            return Ok(());
        }
        record.set_state(UserShellState::Failed);
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

/// A background notice was already written when moving to the background: the record is in the
/// background, or went from the background to stopping.
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
        updated_at_ms: record.updated_at_ms,
        operation_id: record.id,
        chat_id: record.chat_id.into(),
        command: record.command.clone(),
        cwd: record.cwd.clone(),
        pid: record.pid,
        state: match record.state {
            UserShellState::Queued
            | UserShellState::WaitingPermission
            | UserShellState::Running => CommandState::Running,
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
