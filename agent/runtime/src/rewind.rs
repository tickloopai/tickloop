//! Rewind，照 Claude Code `rewindConversationTo` 与 `fileHistoryRewind` 实现。
//!
//! 对话只截断内存：rewind 到某条提示时丢弃它及之后的消息，下一条消息接在最后保留的消息之后，
//! 在同一 Transcript 中形成分叉，被放弃的消息仍留在文件中。代码按目标提示的文件回滚快照恢复。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::loop_lease::RunStateChange;
use super::{Agent, AgentError, Chat, ChatBookkeeping};
use crate::file_history::{
    FileHistoryError, FileHistorySnapshot, FileRestoreReport, create_first_backup,
};
use crate::transcript::private_project_temp_directory;
use tickloop_agent_base::private_fs::PrivateDirectory;
use tickloop_agent_protocol::local_messages::LocalMessage;
use tickloop_agent_protocol::projection;
use tickloop_agent_protocol::{
    ChatId, ChatSnapshot, CodeRewind, Content, Event, EventKind, ExecutionMode, HostNote, LoopId,
    LoopSnapshot, LoopState, Message, RewindDraft, RewindMode, RewindOutcome, RewindPoint, Role,
    RunId, TakenBackPrompt, UserShellRecord, UserShellState,
};
use tickloop_agent_tools::Tools;
use tickloop_agent_transcript::{DiscardReason, SettledOutcome};

impl Agent {
    /// 照 Claude Code 的 Rewind 选择器：按模式恢复代码、对话或两者。先恢复代码，再截断对话；
    /// 代码恢复失败不妨碍对话恢复。
    ///
    /// 分三段执行：持锁校验空闲、预先算出截断后的对话（出错不改动任何东西）并占住该 Chat；
    /// 释放锁后在阻塞线程中恢复文件，其他 Chat 照常运行；重新加锁后截断对话并释放占用。
    /// 占用期间该 Chat 不能开始 Run、压缩、定向摘要、Branch 或另一次 rewind，期间排队的输入
    /// 在释放占用后照常开始。
    pub async fn rewind(
        &self,
        chat_id: ChatId,
        message_id: Uuid,
        mode: RewindMode,
    ) -> Result<RewindOutcome, AgentError> {
        self.require_root_chat(chat_id, "rewind history").await?;
        // Held until the task is spawned: `shutdown` either refuses this or waits for the task.
        let admission = self.inner.tasks.token();
        let (point, history, loops_at_start) = {
            let mut chats = self.inner.chats.lock().await;
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err(AgentError::Invalid("agent host has shut down".into()));
            }
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            ensure_idle(chat)?;
            let point = chat.snapshot.rewind_point(message_id).ok_or_else(|| {
                AgentError::Invalid("this message is no longer part of the conversation".into())
            })?;
            let loops_at_start = chat
                .snapshot
                .loops
                .iter()
                .map(|goal| goal.id)
                .collect::<HashSet<_>>();
            let history = match (mode.restores_code(), &chat.file_history) {
                (false, _) => None,
                (true, Some(history)) => Some(history.clone()),
                (true, None) => return Err(AgentError::Invalid(super::FOLDERLESS_CHAT.into())),
            };
            if mode.restores_conversation() {
                conversation_before(
                    &chat.snapshot,
                    &chat.bookkeeping,
                    point.message_index,
                    Some(point.execution_mode),
                    &loops_at_start,
                )?;
            }
            chat.active = Some(CancellationToken::new());
            (point, history, loops_at_start)
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let agent = self.clone();
        // 放进宿主任务，调用方放弃等待时也会释放占用。
        self.inner.tasks.spawn(async move {
            let restored = match history {
                Some(history) => {
                    Some(tokio::task::spawn_blocking(move || history.rewind(message_id)).await)
                }
                None => None,
            };
            let (result, removed_loops) = {
                let mut chats = agent.inner.chats.lock().await;
                let Some(chat) = chats.get_mut(&chat_id) else {
                    let _ = sender.send(Err(AgentError::NotFound));
                    return;
                };
                chat.active = None;
                let result = agent.finish_rewind(chat, point, mode, restored, &loops_at_start);
                let removed_loops = stop_listening_to_removed_loops(chat, loops_at_start);
                (result, (removed_loops, chat.tools.clone()))
            };
            let next = agent.next_run(chat_id).await;
            let (result, mut next) = match (result, next) {
                (Ok(mut outcome), Ok(next)) => {
                    match agent.snapshot(chat_id).await {
                        Ok(snapshot) => outcome.snapshot = snapshot,
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            return;
                        }
                    }
                    (Ok(outcome), next)
                }
                (Ok(_), Err(error)) => (Err(error), None),
                (Err(error), next) => (Err(error), next.ok().flatten()),
            };
            let (removed_loops, tools) = removed_loops;
            agent
                .retire_removed_loops(chat_id, removed_loops, &tools)
                .await;
            let _ = sender.send(result);
            while let Some((receipt, request, cancel)) = next {
                next = agent.run(receipt, request, cancel).await;
            }
        });
        drop(admission);
        receiver
            .await
            .map_err(|_| AgentError::Invalid("rewind task ended unexpectedly".into()))?
    }

    /// 照 Claude Code 中断后的自动恢复（`REPL.tsx` 在 `onQuery` 收尾处以
    /// `messagesAfterAreOnlySynthetic` 判断后调用 `restoreMessageSync`）：用户中断的 `run_id` 还没有
    /// 助手文字或工具调用时，对话截断到它的提示之前，模型此后看不到这条提示，Transcript 中留作
    /// 被放弃的分支。与 rewind 不同，不建新的草稿 Loop：提示建立的 Loop 随提示移除，再次发送时
    /// 新建；提示发给的已有 Loop 保留，再次发送仍发给它。条件不满足时不改动任何东西并返回
    /// `None`，中断照常留在对话中。
    pub async fn take_back_interrupted_prompt(
        &self,
        chat_id: ChatId,
        run_id: RunId,
    ) -> Result<Option<TakenBackPrompt>, AgentError> {
        let (taken_back, removed_loops, tools) = {
            let mut chats = self.inner.chats.lock().await;
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err(AgentError::Invalid("agent host has shut down".into()));
            }
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let Some((point, prompt_loop)) = interrupted_prompt(chat, run_id) else {
                return Ok(None);
            };
            let loops_at_start = chat
                .snapshot
                .loops
                .iter()
                .map(|goal| goal.id)
                .collect::<HashSet<_>>();
            let (candidate, candidate_bookkeeping) = conversation_before(
                &chat.snapshot,
                &chat.bookkeeping,
                point.message_index,
                Some(point.execution_mode),
                &loops_at_start,
            )?;
            let loop_id = candidate
                .loops
                .iter()
                .any(|goal| goal.id == prompt_loop)
                .then_some(prompt_loop);
            self.replace_conversation(
                chat,
                point.message_index,
                point.message_id,
                (candidate, candidate_bookkeeping),
                prompt_loop,
            );
            self.save(&chat.snapshot);
            let removed_loops = stop_listening_to_removed_loops(chat, loops_at_start);
            let taken_back = TakenBackPrompt {
                loop_id,
                prompt: point.prompt,
                contexts: point.contexts,
            };
            (taken_back, removed_loops, chat.tools.clone())
        };
        self.retire_removed_loops(chat_id, removed_loops, &tools)
            .await;
        Ok(Some(taken_back))
    }

    /// 被截断移除的 Loop 若仍有后台命令在运行，随之停止。移除视为结束：先写结束状态再归还租约，
    /// 同一 Loop 在其他进程中的 Trigger 下一次到点时随之停止。
    pub(super) async fn retire_removed_loops(
        &self,
        chat_id: ChatId,
        removed_loops: Vec<LoopId>,
        tools: &Tools,
    ) {
        for loop_id in removed_loops {
            let recorded = self
                .record_loop_run_state(chat_id, loop_id, RunStateChange::Ended)
                .await;
            self.inner.loop_leases.release(chat_id, loop_id);
            self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
            for command in tools.stop_loop(loop_id).await {
                let _ = self.record_command(chat_id, command).await;
            }
        }
    }

    /// rewind 的第三段：按文件恢复结果与模式截断对话。对话以占用结束时的快照重新计算，
    /// 保留占用期间排队的输入等变化。
    fn finish_rewind(
        &self,
        chat: &mut Chat,
        point: RewindPoint,
        mode: RewindMode,
        restored: Option<Result<Result<FileRestoreReport, FileHistoryError>, JoinError>>,
        loops_at_start: &HashSet<LoopId>,
    ) -> Result<RewindOutcome, AgentError> {
        let code = match restored {
            None => None,
            Some(Err(error)) => {
                return Err(AgentError::Invalid(format!("file restore ended: {error}")));
            }
            Some(Ok(Ok(report))) => Some(CodeRewind {
                changed_files: report.changed_files,
                failed_files: report.failures,
                error: None,
            }),
            Some(Ok(Err(error))) if mode == RewindMode::Code => {
                return Err(AgentError::Invalid(error.to_string()));
            }
            Some(Ok(Err(error))) => Some(CodeRewind {
                error: Some(error.to_string()),
                ..CodeRewind::default()
            }),
        };
        let draft = if mode.restores_conversation() {
            let (mut candidate, mut candidate_bookkeeping) = conversation_before(
                &chat.snapshot,
                &chat.bookkeeping,
                point.message_index,
                Some(point.execution_mode),
                loops_at_start,
            )?;
            let goal = prompt_loop_goal(&chat.snapshot, point.message_index)
                .unwrap_or_else(|| point.prompt.clone());
            let loop_id = Uuid::new_v4();
            candidate_bookkeeping.add_loop(
                &mut candidate,
                LoopSnapshot {
                    id: loop_id,
                    goal,
                    state: LoopState::Idle,
                    run_id: None,
                    run_started_at: None,
                    trigger: None,
                    event_sources: Vec::new(),
                    state_since: None,
                },
            );
            self.replace_conversation(
                chat,
                point.message_index,
                point.message_id,
                (candidate, candidate_bookkeeping),
                loop_id,
            );
            Some(RewindDraft {
                loop_id,
                prompt: point.prompt,
                contexts: point.contexts,
            })
        } else {
            None
        };
        self.save(&chat.snapshot);
        Ok(RewindOutcome {
            snapshot: chat.snapshot.clone(),
            mode,
            code,
            conversation_restored: draft.is_some(),
            draft,
        })
    }

    /// 恢复到目标提示时会被改变的文件，不改动磁盘。没有该提示的快照时返回 `None`。
    pub async fn rewind_file_changes(
        &self,
        chat_id: ChatId,
        message_id: Uuid,
    ) -> Result<Option<Vec<PathBuf>>, AgentError> {
        self.require_root_chat(chat_id, "rewind files").await?;
        let history = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .file_history
            .clone()
            .ok_or_else(|| AgentError::Invalid(super::FOLDERLESS_CHAT.into()))?;
        tokio::task::spawn_blocking(move || history.files_changed_by_rewind(message_id))
            .await
            .map_err(|error| AgentError::Invalid(format!("file comparison ended: {error}")))
    }

    /// 换上截断到第 `index` 条消息之前的对话（`message_id` 是被截掉的第一条）：Transcript 中
    /// 下一条消息接在保留的最后一条之后，各 View 收到 `ConversationRewound` 后重画对话。
    pub(super) fn replace_conversation(
        &self,
        chat: &mut Chat,
        index: usize,
        message_id: Uuid,
        (candidate, candidate_bookkeeping): (ChatSnapshot, ChatBookkeeping),
        loop_id: LoopId,
    ) {
        if let Some(recorder) = &self.inner.recorder {
            recorder.truncate(&chat.snapshot, &chat.bookkeeping, index);
        }
        // The kept history ends where the prompt was submitted, in the mode it restores. Only
        // an exited-plan-mode notice the prompt's first request carried is owed again.
        let first_reply = (index..chat.snapshot.messages.len())
            .find(|index| chat.snapshot.messages[*index].role == Role::Assistant)
            .unwrap_or(chat.snapshot.messages.len());
        chat.needs_plan_mode_exit_notice = chat
            .snapshot
            .host_notes
            .range(index..first_reply)
            .any(|(_, note)| *note == HostNote::PlanModeExit);
        chat.snapshot = candidate;
        chat.bookkeeping = candidate_bookkeeping;
        self.inner.ledger_changes.send_replace(());
        self.emit_conversation_rewound(chat, loop_id, message_id);
    }

    pub(super) fn emit_conversation_rewound(
        &self,
        chat: &mut Chat,
        loop_id: LoopId,
        message_id: Uuid,
    ) {
        chat.snapshot.last_sequence += 1;
        let event = Event {
            sequence: chat.snapshot.last_sequence,
            chat_id: chat.snapshot.id,
            loop_id,
            run_id: Uuid::nil(),
            kind: EventKind::ConversationRewound { message_id },
        };
        chat.snapshot.events.push(event.clone());
        if chat.snapshot.events.len() > super::EVENT_REPLAY_LIMIT {
            chat.snapshot.events.remove(0);
        }
        let _ = self.inner.events.send(event);
    }

    /// 文件回滚备份目录：持久化时照 Claude Code 为 `<root>/file-history/<chatId>`。
    pub(super) fn file_history_directory(
        &self,
        chat_id: ChatId,
        folder: &Path,
    ) -> PrivateDirectory {
        match self
            .inner
            .recorder
            .as_ref()
            .and_then(|recorder| recorder.file_history_root())
        {
            Some(directory) => PrivateDirectory::trusted(directory),
            // 不持久化时不在工作目录写文件，备份放在系统临时目录的项目目录下（工单 54），
            // 创建时逐层校验属主、权限位与符号链接。
            None => private_project_temp_directory(folder)
                .join(crate::file_history::FILE_HISTORY_DIRECTORY),
        }
        .join(chat_id.to_string())
    }

    /// 照 Claude Code `fileHistoryMakeSnapshot`：提示写入对话时只追加待定快照
    /// （`append_input_message`），这里按提示顺序逐个在锁外算出备份、持锁提交，
    /// 再以提示 id 写入 Transcript。Run 开始时与合并输入之后调用，先于本轮任何工具执行，
    /// 快照因此记录的是本轮编辑之前的文件，编辑跟踪的快照更新也总写在它之后。
    pub(super) async fn complete_file_history_snapshots(&self, chat_id: ChatId) {
        loop {
            let plan = {
                let chats = self.inner.chats.lock().await;
                let Some(plan) = chats
                    .get(&chat_id)
                    .and_then(|chat| chat.file_history.as_ref()?.next_snapshot_plan())
                else {
                    return;
                };
                plan
            };
            let Ok(computed) = tokio::task::spawn_blocking(move || plan.compute()).await else {
                return;
            };
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return;
            };
            if let Some(snapshot) = chat
                .file_history
                .as_mut()
                .and_then(|history| history.commit_snapshot(computed))
            {
                self.record_file_history_snapshot(chat_id, snapshot.message_id, &snapshot, false);
            }
        }
    }

    pub(super) fn record_file_history_snapshot(
        &self,
        chat_id: ChatId,
        message_id: Uuid,
        snapshot: &FileHistorySnapshot,
        is_snapshot_update: bool,
    ) {
        if let Some(recorder) = &self.inner.recorder {
            recorder.record_file_history_snapshot(
                chat_id,
                message_id,
                snapshot,
                is_snapshot_update,
            );
        }
    }

    /// 照 Claude Code `fileHistoryTrackEdit`：编辑工具执行前为未跟踪的文件建首版备份，
    /// 以发起编辑的 assistant 消息为键写入快照更新。备份失败（包括文件已被替换为符号链接或
    /// 不是普通文件）只放弃跟踪，不阻止编辑。
    pub(super) async fn track_file_edit(&self, chat_id: ChatId, path: &Path, message_id: Uuid) {
        let directory = {
            let chats = self.inner.chats.lock().await;
            let Some(history) = chats
                .get(&chat_id)
                .and_then(|chat| chat.file_history.as_ref())
            else {
                return;
            };
            if !history.needs_backup(path) {
                return;
            }
            history.backup_directory().clone()
        };
        let source = path.to_path_buf();
        let backup =
            tokio::task::spawn_blocking(move || create_first_backup(&directory, &source)).await;
        let Ok(Ok(backup)) = backup else {
            return;
        };
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        if let Some(snapshot) = chat
            .file_history
            .as_mut()
            .and_then(|history| history.commit_tracked_edit(path, backup))
        {
            self.record_file_history_snapshot(chat_id, message_id, &snapshot, true);
        }
    }
}

/// The conversation before message `index`, in `execution_mode` when given (the mode a prompt
/// cut at its own index was sent in).
///
/// `loops_at_start` 是 rewind 占用开始时存在的 Loop：只有它们会因创建位置在截断点之后而移除，
/// 连同其排队输入、权限请求与提问；占用期间新建的 Loop 保留，创建位置移到截断点。
pub(super) fn conversation_before(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    index: usize,
    execution_mode: Option<ExecutionMode>,
    loops_at_start: &HashSet<LoopId>,
) -> Result<(ChatSnapshot, ChatBookkeeping), AgentError> {
    let mut candidate = snapshot.clone();
    let mut candidate_bookkeeping = bookkeeping.clone();
    let kept_runs = candidate
        .message_runs
        .range(..index)
        .map(|(_, run)| *run)
        .collect::<HashSet<RunId>>();
    let abandoned_runs = candidate
        .message_runs
        .range(index..)
        .map(|(_, run)| *run)
        .filter(|run| !kept_runs.contains(run))
        .collect::<HashSet<RunId>>();
    candidate.messages.truncate(index);
    candidate.message_runs.split_off(&index);
    candidate.message_contexts.split_off(&index);
    candidate_bookkeeping.denial_references.split_off(&index);
    candidate_bookkeeping.message_responses.split_off(&index);
    candidate_bookkeeping.lifecycle.restart_claims_at(index);
    candidate_bookkeeping.lifecycle.forget_tail_answer();
    candidate.message_ids.split_off(&index);
    candidate.original_message_ids.split_off(&index);
    candidate.prompt_execution_modes.split_off(&index);
    candidate
        .inherited_prompts
        .retain(|message_index| *message_index < index);
    candidate.host_notes.split_off(&index);
    candidate.announced_host_tools = candidate.visible_announced_host_tools();
    candidate.message_usage.split_off(&index);
    candidate.message_sources.split_off(&index);
    candidate.message_models.split_off(&index);
    // 照 Claude Code：rewind 越过压缩边界时，边界随截断的消息一起丢弃，模型重新看到边界之前的
    // 原始对话；下一条消息在 Transcript 中从边界之前分叉。
    candidate.message_compactions.split_off(&index);
    if let Some(creation) = candidate
        .branch_origin
        .as_mut()
        .and_then(|origin| origin.creation.as_mut())
        && creation.model_seed_start + creation.model_seed_message_count > index
    {
        creation.clear_seed();
    }
    candidate
        .events
        .retain(|event| !abandoned_runs.contains(&event.run_id));
    // Loop 随对话链生效：截断位置或之后创建的 Loop 在新的对话链上不存在。
    let removed_loops = candidate_bookkeeping
        .loop_starts
        .iter()
        .filter(|(loop_id, start)| **start >= index && loops_at_start.contains(loop_id))
        .map(|(loop_id, _)| *loop_id)
        .collect::<HashSet<_>>();
    for start in candidate_bookkeeping.loop_starts.values_mut() {
        *start = (*start).min(index);
    }
    for pending in &candidate.pending_inputs {
        if removed_loops.contains(&pending.receipt.loop_id) {
            candidate_bookkeeping.lifecycle.settle(
                pending.receipt.run_id,
                SettledOutcome::Discarded {
                    reason: DiscardReason::LoopEnded,
                },
            );
        }
    }
    candidate
        .pending_inputs
        .retain(|pending| !removed_loops.contains(&pending.receipt.loop_id));
    candidate
        .permissions
        .retain(|permission| !removed_loops.contains(&permission.loop_id));
    candidate
        .questions
        .retain(|question| !removed_loops.contains(&question.loop_id));
    candidate
        .loops
        .retain(|goal| !removed_loops.contains(&goal.id));
    candidate_bookkeeping
        .loop_starts
        .retain(|loop_id, _| !removed_loops.contains(loop_id));
    candidate
        .resume_triggers
        .retain(|loop_id| !removed_loops.contains(loop_id));
    candidate
        .plans
        .retain(|plan| !removed_loops.contains(&plan.loop_id));
    candidate_bookkeeping
        .unavailable_plan_versions
        .retain(|loop_id, _| !removed_loops.contains(loop_id));
    candidate
        .progress
        .retain(|progress| !removed_loops.contains(&progress.loop_id));
    if let Some(mode) = execution_mode
        && candidate.policy.mode != mode
    {
        candidate.policy.mode = mode;
        candidate.policy.revision += 1;
    }
    projection::projected_messages(&candidate, &candidate_bookkeeping.denial_references)?;
    Ok((candidate, candidate_bookkeeping))
}

/// 目标提示所属 Loop 的目标文字。提示的 run 仍是 Loop 当前 run、有用量或操作记录时直接对应；
/// 合并进其他 run 的提示没有自己的记录，沿对话向前找最近一条能对应到 Loop 的消息。
fn prompt_loop_goal(snapshot: &ChatSnapshot, message_index: usize) -> Option<String> {
    let loop_of_run = |run: RunId| {
        snapshot
            .loops
            .iter()
            .find(|goal| goal.run_id == Some(run))
            .map(|goal| goal.id)
            .or_else(|| {
                snapshot
                    .request_usage
                    .entries()
                    .iter()
                    .find(|usage| usage.run_id == Some(run))
                    .and_then(|usage| usage.loop_id)
            })
            .or_else(|| {
                snapshot
                    .operations
                    .iter()
                    .find(|operation| operation.run_id == run)
                    .map(|operation| operation.loop_id)
            })
    };
    let loop_id = snapshot
        .message_runs
        .range(..=message_index)
        .rev()
        .find_map(|(_, run)| loop_of_run(*run))?;
    snapshot
        .loops
        .iter()
        .find(|goal| goal.id == loop_id)
        .map(|goal| goal.goal.clone())
}

/// 截断后不在对话中的 Loop：停止它们的 Trigger 与事件来源，返回这些 Loop 供
/// [`Agent::retire_removed_loops`] 结束。
pub(super) fn stop_listening_to_removed_loops(
    chat: &mut Chat,
    loops_at_start: HashSet<LoopId>,
) -> Vec<LoopId> {
    let removed_loops = loops_at_start
        .into_iter()
        .filter(|loop_id| !chat.snapshot.loops.iter().any(|goal| goal.id == *loop_id))
        .collect::<Vec<_>>();
    for loop_id in &removed_loops {
        if let Some(cancel) = chat.trigger_cancellations.remove(loop_id) {
            cancel.cancel();
        }
        if let Some(cancel) = chat.event_cancellations.remove(loop_id) {
            cancel.cancel();
        }
    }
    removed_loops
}

/// 用户中断的 `run_id` 可以撤回的提示及其 Loop。照 Claude Code 的守卫：Chat 已空闲且没有排队的
/// 输入（中断后排队的输入已开始就是用户已经往下走了），查看的不是委派的子 Agent；对话中最后
/// 一条提示属于这一轮且其 Loop 已中断，提示之后只有不算回应的消息。
fn interrupted_prompt(chat: &Chat, run_id: RunId) -> Option<(RewindPoint, LoopId)> {
    let snapshot = &chat.snapshot;
    if snapshot.delegation.is_some() || ensure_idle(chat).is_err() {
        return None;
    }
    let (&index, _) = snapshot.prompt_execution_modes.last_key_value()?;
    let loop_id = *chat.bookkeeping.run_loops.get(&run_id)?;
    let is_interrupted = snapshot
        .loops
        .iter()
        .any(|goal| goal.id == loop_id && goal.state == LoopState::Interrupted);
    if !is_interrupted || snapshot.message_runs.get(&index) != Some(&run_id) {
        return None;
    }
    if !snapshot
        .messages
        .get(index + 1..)?
        .iter()
        .zip(index + 1..)
        .all(|(message, index)| {
            snapshot.host_notes.contains_key(&index) || is_not_a_response(message)
        })
    {
        return None;
    }
    Some((snapshot.rewind_point_at(index)?, loop_id))
}

/// 照 Claude Code `messagesAfterAreOnlySynthetic`：中断标记和没有文字、工具调用的助手消息
/// （例如只有思考）不算回应，其他消息都算；宿主写给模型的 host note 像 Claude Code 的 meta
/// 附件，也不算，由调用方按下标排除。
fn is_not_a_response(message: &Message) -> bool {
    match message.role {
        Role::Assistant => message.content.iter().all(|content| match content {
            Content::Text { text } => text.trim().is_empty(),
            Content::ToolUse { .. } => false,
            _ => true,
        }),
        Role::User => matches!(
            message.content.as_slice(),
            [Content::Text { text }]
                if matches!(LocalMessage::parse(text), Some(LocalMessage::Interrupted { .. }))
        ),
        Role::System => false,
    }
}

pub(super) fn ensure_idle(chat: &Chat) -> Result<(), AgentError> {
    if chat.branch_reservation.is_some()
        || chat.active.is_some()
        || !chat.parked.is_empty()
        || !chat.snapshot.pending_inputs.is_empty()
        || !chat.snapshot.pending_commands.is_empty()
        || !chat.snapshot.permissions.is_empty()
        || !chat.snapshot.questions.is_empty()
        || !chat.ready_runs.is_empty()
        || chat
            .snapshot
            .user_shells
            .iter()
            .any(user_shell_blocks_rewind)
    {
        return Err(AgentError::Invalid("finish or interrupt current work and resolve queued inputs before restoring a checkpoint".into()));
    }
    Ok(())
}

fn user_shell_blocks_rewind(shell: &UserShellRecord) -> bool {
    shell.state == UserShellState::Queued
        || (matches!(
            shell.state,
            UserShellState::Running | UserShellState::Cancelling
        ) && !shell.starts_in_background)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use tickloop_agent_protocol::Content;

    use async_trait::async_trait;
    use tokio::sync::{broadcast, mpsc::UnboundedSender};

    use super::*;
    use crate::file_history::test_pause;
    use tickloop_agent_model::{
        LocalModelClients, ModelClient, ModelClients, ModelDelta, ModelError, ModelOutput,
        ModelRequest,
    };
    use tickloop_agent_protocol::{Event, Receipt, UserShellRecord};
    use tickloop_agent_protocol::{InputModality, ModelConfig, ModelIdentity, Protocol, Usage};

    const WRITE_PREFIX: &str = "write ";
    const STEP_TIMEOUT: Duration = Duration::from_secs(10);

    /// 提示以 `write ` 开头时先 Read 再以 Write 把其余文字写进 tracked.txt，其他请求只回复文字。
    struct EditingModel;

    #[async_trait]
    impl ModelClient for EditingModel {
        async fn generate_title(
            &self,
            request: tickloop_agent_model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
            Ok(tickloop_agent_model::fixture_title(request))
        }

        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: tickloop_agent_protocol::ModelProvider::Api,
                profile: "fixture".into(),
                protocol: Protocol::Messages,
                requested_model: "fixture".into(),
            }
        }

        async fn complete(
            &self,
            request: ModelRequest,
            _: UnboundedSender<ModelDelta>,
            _: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            let written = request.messages.last().and_then(|message| {
                message.content.iter().find_map(|content| match content {
                    Content::Text { text } => text.strip_prefix(WRITE_PREFIX),
                    _ => None,
                })
            });
            let content = match written {
                Some(text) => vec![
                    Content::ToolUse {
                        id: Uuid::new_v4().to_string(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": "tracked.txt"}),
                    },
                    Content::ToolUse {
                        id: Uuid::new_v4().to_string(),
                        name: "Write".into(),
                        input: serde_json::json!({"file_path": "tracked.txt", "content": text}),
                    },
                ],
                None => vec![Content::Text {
                    text: "done".into(),
                }],
            };
            Ok(ModelOutput {
                private_source: None,
                content,
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            })
        }
    }

    async fn wait_for_completion(
        agent: &Agent,
        events: &mut broadcast::Receiver<Event>,
        receipt: &Receipt,
    ) {
        loop {
            let event = events.recv().await.expect("event stream open");
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => agent
                    .answer_permission(receipt.chat_id, request.id, true)
                    .await
                    .expect("permission accepted"),
                EventKind::Completed { .. } => return,
                EventKind::Failed(error) => panic!("prompt failed: {error}"),
                _ => {}
            }
        }
    }

    async fn run_prompt(
        agent: &Agent,
        events: &mut broadcast::Receiver<Event>,
        chat_id: ChatId,
        prompt: &str,
    ) {
        let receipt = agent.submit(chat_id, None, prompt.into()).await.unwrap();
        tokio::time::timeout(STEP_TIMEOUT, wait_for_completion(agent, events, &receipt))
            .await
            .unwrap_or_else(|_| panic!("prompt {prompt:?} did not finish in time"));
    }

    struct TwoChats {
        _directory: tempfile::TempDir,
        agent: Agent,
        events: broadcast::Receiver<Event>,
        first: ChatId,
        second: ChatId,
        first_tracked: PathBuf,
    }

    /// 两个 Chat 各自在自己的工作目录中编辑过 tracked.txt，之后的提示会为它建快照。
    async fn two_chats_with_tracked_files() -> TwoChats {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let [first_workdir, second_workdir] = ["first", "second"].map(|name| root.join(name));
        for workdir in [&first_workdir, &second_workdir] {
            std::fs::create_dir(workdir).unwrap();
            std::fs::write(workdir.join("tracked.txt"), "original").unwrap();
        }
        let agent =
            Agent::open_embedded(root.join("state"), config(), Arc::new(EditingModel)).unwrap();
        let first = agent.create_chat(first_workdir.clone()).await.unwrap();
        let second = agent.create_chat(second_workdir).await.unwrap();
        let mut events = agent.subscribe();
        run_prompt(&agent, &mut events, first, "write first agent").await;
        run_prompt(&agent, &mut events, second, "write second agent").await;
        TwoChats {
            _directory: directory,
            agent,
            events,
            first,
            second,
            first_tracked: first_workdir.join("tracked.txt"),
        }
    }

    fn wait_until_paused(pause: &test_pause::Pause) {
        tokio::task::block_in_place(|| pause.entered.recv_timeout(STEP_TIMEOUT))
            .expect("file history reached the paused file");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn another_chat_completes_a_turn_while_rewind_restores_files() {
        let TwoChats {
            _directory,
            agent,
            mut events,
            first,
            second,
            first_tracked,
        } = two_chats_with_tracked_files().await;
        run_prompt(&agent, &mut events, first, "talk").await;
        let target = agent.snapshot(first).await.unwrap().rewind_points()[0].message_id;

        let pause = test_pause::pause_next(&first_tracked);
        let rewinding = tokio::spawn({
            let agent = agent.clone();
            async move { agent.rewind(first, target, RewindMode::Code).await }
        });
        wait_until_paused(&pause);
        run_prompt(&agent, &mut events, second, "write second again").await;
        let busy = agent
            .rewind(first, target, RewindMode::Conversation)
            .await
            .unwrap_err();
        assert!(
            busy.to_string()
                .contains("finish or interrupt current work")
        );
        assert!(!rewinding.is_finished());

        drop(pause);
        let outcome = tokio::time::timeout(STEP_TIMEOUT, rewinding)
            .await
            .expect("rewind finishes after the restore is released")
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome.code.unwrap().changed_files,
            std::slice::from_ref(&first_tracked)
        );
        assert_eq!(std::fs::read_to_string(&first_tracked).unwrap(), "original");
        run_prompt(&agent, &mut events, first, "talk after rewind").await;
        agent.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rewind_drops_queued_work_of_removed_loops_and_keeps_loops_created_meanwhile() {
        let TwoChats {
            _directory,
            agent,
            mut events,
            first,
            first_tracked,
            ..
        } = two_chats_with_tracked_files().await;
        run_prompt(&agent, &mut events, first, "write scheduled agent").await;
        let snapshot = agent.snapshot(first).await.unwrap();
        let scheduled = snapshot.loops.last().unwrap().id;
        let point = snapshot
            .rewind_points()
            .into_iter()
            .find(|point| point.prompt == "write scheduled agent")
            .unwrap();
        agent
            .configure_trigger(
                first,
                scheduled,
                Some(tickloop_agent_protocol::Trigger {
                    id: Uuid::new_v4(),
                    prompt: "scheduled check".into(),
                    prompt_source: tickloop_agent_protocol::TriggerPromptSource::Explicit,
                    schedule: Some(tickloop_agent_protocol::Recurrence::parse("every 1s").unwrap()),
                    armed_at_ms: None,
                    last_fired_at_ms: None,
                    confirmation: None,
                    expires_at_ms: None,
                    accepts_events: false,
                    armed: true,
                    subscriptions: Vec::new(),
                }),
            )
            .await
            .unwrap();

        let pause = test_pause::pause_next(&first_tracked);
        let rewinding = tokio::spawn({
            let agent = agent.clone();
            async move {
                agent
                    .rewind(first, point.message_id, RewindMode::CodeAndConversation)
                    .await
            }
        });
        wait_until_paused(&pause);
        // 占用期间：新建 Loop 的输入排队，被移除 Loop 的定时输入也排队。
        let created = agent
            .submit(first, None, "talk during rewind".into())
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !agent
            .snapshot(first)
            .await
            .unwrap()
            .pending_inputs
            .iter()
            .any(|pending| pending.receipt.loop_id == scheduled && pending.trigger_source.is_some())
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the scheduled trigger did not queue during the rewind"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(pause);
        let outcome = tokio::time::timeout(STEP_TIMEOUT, rewinding)
            .await
            .expect("rewind finishes after the restore is released")
            .unwrap()
            .unwrap();
        assert!(
            !outcome
                .snapshot
                .loops
                .iter()
                .any(|goal| goal.id == scheduled)
        );
        assert!(
            outcome
                .snapshot
                .loops
                .iter()
                .any(|goal| goal.id == created.loop_id)
        );
        assert!(
            outcome
                .snapshot
                .pending_inputs
                .iter()
                .all(|pending| pending.receipt.loop_id != scheduled)
        );
        tokio::time::timeout(
            STEP_TIMEOUT,
            wait_for_completion(&agent, &mut events, &created),
        )
        .await
        .expect("input queued for a Loop created during the rewind runs afterwards");
        let after = agent.snapshot(first).await.unwrap();
        assert!(after.pending_inputs.is_empty());
        let latest = after.rewind_points().last().unwrap().message_id;
        agent
            .rewind(first, latest, RewindMode::Conversation)
            .await
            .expect("no leftover input blocks the next rewind");
        agent.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn another_chat_completes_a_turn_while_a_prompt_snapshot_backs_up_files() {
        let TwoChats {
            _directory,
            agent,
            mut events,
            first,
            second,
            first_tracked,
        } = two_chats_with_tracked_files().await;
        let messages_before = agent.snapshot(first).await.unwrap().messages.len();
        let pause = test_pause::pause_next(&first_tracked);
        let snapshotting = agent.submit(first, None, "talk".into()).await.unwrap();
        wait_until_paused(&pause);
        run_prompt(&agent, &mut events, second, "write second again").await;
        assert_eq!(
            agent.snapshot(first).await.unwrap().messages.len(),
            messages_before + 1,
            "the paused prompt must not reach the model before its snapshot is released"
        );

        drop(pause);
        tokio::time::timeout(
            STEP_TIMEOUT,
            wait_for_completion(&agent, &mut events, &snapshotting),
        )
        .await
        .expect("the paused prompt finishes after its snapshot is released");
        let points = agent.snapshot(first).await.unwrap().rewind_points();
        assert_eq!(points.len(), 2);
        std::fs::write(&first_tracked, "changed by hand").unwrap();
        let latest = agent
            .rewind(first, points[1].message_id, RewindMode::Code)
            .await
            .unwrap();
        assert!(latest.code.unwrap().failed_files.is_empty());
        assert_eq!(
            std::fs::read_to_string(&first_tracked).unwrap(),
            "first agent"
        );
        agent
            .rewind(first, points[0].message_id, RewindMode::Code)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&first_tracked).unwrap(), "original");
        agent.shutdown().await;
    }

    fn config() -> ModelConfig {
        ModelConfig {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
            name: "fixture".into(),
            protocol: Protocol::Messages,
            base_url: "http://127.0.0.1".into(),
            model: "fixture".into(),
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

    fn shell(state: UserShellState, starts_in_background: bool) -> UserShellRecord {
        UserShellRecord {
            updated_at_ms: None,
            id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            loop_id: None,
            command: "sleep 1".into(),
            cwd: PathBuf::from("/tmp"),
            starts_in_background,
            queued_sequence: 1,
            armed: true,
            preceding_model_runs: Vec::new(),
            state,
            pid: None,
            exit_code: None,
            signal: None,
            reason: None,
            output_ref: None,
            output_bytes: 0,
            submitted_at_message: 0,
            state_since: None,
        }
    }

    #[test]
    fn foreground_shells_block_rewind_but_background_shells_do_not() {
        assert!(user_shell_blocks_rewind(&shell(
            UserShellState::Cancelling,
            false
        )));
        assert!(user_shell_blocks_rewind(&shell(
            UserShellState::Queued,
            true
        )));
        assert!(!user_shell_blocks_rewind(&shell(
            UserShellState::Running,
            true
        )));
        assert!(!user_shell_blocks_rewind(&shell(
            UserShellState::Completed,
            false
        )));
    }

    #[tokio::test]
    async fn busy_chats_reject_rewind_without_touching_the_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            config(),
            LocalModelClients::new(None).client(&config()).unwrap(),
        );
        let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
        {
            let mut chats = agent.inner.chats.lock().await;
            chats.get_mut(&chat_id).unwrap().active = Some(CancellationToken::new());
        }
        let error = agent
            .rewind(chat_id, Uuid::new_v4(), RewindMode::Conversation)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("finish or interrupt current work")
        );
        agent
            .inner
            .chats
            .lock()
            .await
            .get_mut(&chat_id)
            .unwrap()
            .active = None;
        agent.shutdown().await;
    }
}
