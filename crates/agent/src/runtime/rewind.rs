//! Rewind，照 Claude Code `rewindConversationTo` 与 `fileHistoryRewind` 实现。
//!
//! 对话只截断内存：rewind 到某条提示时丢弃它及之后的消息，下一条消息接在最后保留的消息之后，
//! 在同一 Transcript 中形成分叉，被放弃的消息仍留在文件中。代码按目标提示的文件回滚快照恢复。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::loop_lease::RunStateChange;
use super::{
    Agent, AgentError, Chat, ChatId, ChatSnapshot, Event, EventKind, InputContext, LoopId,
    LoopSnapshot, LoopState, RunId, UserShellState,
};
use crate::file_history::{
    FileHistoryError, FileHistorySnapshot, FileRestoreReport, create_first_backup,
};
use crate::model::{Content, Message};
use crate::permissions::ExecutionMode;
use crate::transcript::{PrivateDirectory, private_project_temp_directory};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RewindMode {
    Code,
    Conversation,
    CodeAndConversation,
}

impl RewindMode {
    fn restores_code(self) -> bool {
        self != Self::Conversation
    }

    fn restores_conversation(self) -> bool {
        self != Self::Code
    }
}

/// rewind 与定向摘要可以选择的位置：当前对话中的一条用户提示。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RewindPoint {
    /// 提示在 Transcript 中的 uuid，也是文件回滚快照关联的消息 id。
    pub message_id: Uuid,
    pub message_index: usize,
    pub prompt: String,
    pub contexts: Vec<InputContext>,
    /// The execution mode captured when this prompt was submitted.
    pub execution_mode: ExecutionMode,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RewindDraft {
    pub loop_id: LoopId,
    pub prompt: String,
    pub contexts: Vec<InputContext>,
}

/// 恢复代码的结果。照 Claude Code，单个文件失败时跳过该文件继续；找不到目标提示的快照时
/// 不改动任何文件，`error` 说明原因。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CodeRewind {
    pub changed_files: Vec<PathBuf>,
    pub failed_files: Vec<(PathBuf, String)>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RewindOutcome {
    pub snapshot: ChatSnapshot,
    pub mode: RewindMode,
    pub code: Option<CodeRewind>,
    pub conversation_restored: bool,
    pub draft: Option<RewindDraft>,
}

impl ChatSnapshot {
    pub fn conversation_messages(&self) -> Result<Vec<Message>, AgentError> {
        super::context::projected_messages(self)
    }

    pub fn conversation_events(&self) -> Vec<Event> {
        self.events.clone()
    }

    /// 当前对话中可以 rewind 的用户提示，按对话顺序排列。
    pub fn rewind_points(&self) -> Vec<RewindPoint> {
        self.prompt_execution_modes
            .keys()
            .filter_map(|index| self.rewind_point_at(*index))
            .collect()
    }

    pub(super) fn rewind_point(&self, message_id: Uuid) -> Option<RewindPoint> {
        let index = self
            .message_ids
            .iter()
            .find(|(_, id)| **id == message_id)
            .map(|(index, _)| *index)?;
        self.rewind_point_at(index)
    }

    pub(super) fn rewind_point_at(&self, message_index: usize) -> Option<RewindPoint> {
        let execution_mode = *self.prompt_execution_modes.get(&message_index)?;
        let message_id = *self.message_ids.get(&message_index)?;
        let message = self.messages.get(message_index)?;
        let prompt = message
            .content
            .iter()
            .map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?
            .join("\n");
        Some(RewindPoint {
            message_id,
            message_index,
            prompt,
            contexts: self
                .message_contexts
                .get(&message_index)
                .cloned()
                .unwrap_or_default(),
            execution_mode,
        })
    }
}

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
        let mut tasks = self.inner.tasks.lock().await;
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
            if mode.restores_conversation() {
                conversation_before(&chat.snapshot, &point, &loops_at_start)?;
            }
            chat.active = Some(CancellationToken::new());
            (point, chat.file_history.clone(), loops_at_start)
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let agent = self.clone();
        // 放进宿主任务，调用方放弃等待时也会释放占用。
        tasks.push(tokio::spawn(async move {
            let restored = if mode.restores_code() {
                Some(tokio::task::spawn_blocking(move || history.rewind(message_id)).await)
            } else {
                None
            };
            let (result, removed_loops) = {
                let mut chats = agent.inner.chats.lock().await;
                let Some(chat) = chats.get_mut(&chat_id) else {
                    let _ = sender.send(Err(AgentError::NotFound));
                    return;
                };
                chat.active = None;
                let result = agent.finish_rewind(chat, point, mode, restored, &loops_at_start);
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
            // 被截断移除的 Loop 若仍有 Trigger 或后台命令在运行，随之停止。移除视为结束：先写结束
            // 状态再归还租约，同一 Loop 在其他进程中的 Trigger 下一次到点时随之停止。
            let (removed_loops, tools) = removed_loops;
            for loop_id in removed_loops {
                let recorded = agent
                    .record_loop_run_state(chat_id, loop_id, RunStateChange::Ended)
                    .await;
                agent.inner.loop_leases.release(chat_id, loop_id);
                agent.warn_loop_run_state_failure(chat_id, loop_id, recorded);
                for command in tools.stop_loop(loop_id).await {
                    let _ = agent.record_command(chat_id, command).await;
                }
            }
            let _ = sender.send(result);
            while let Some((receipt, request, cancel)) = next {
                next = agent.run(receipt, request, cancel).await;
            }
        }));
        drop(tasks);
        receiver
            .await
            .map_err(|_| AgentError::Invalid("rewind task ended unexpectedly".into()))?
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
            let (candidate, loop_id) = conversation_before(&chat.snapshot, &point, loops_at_start)?;
            if let Some(recorder) = &self.inner.recorder {
                recorder.truncate(&mut chat.snapshot, point.message_index);
            }
            chat.snapshot = candidate;
            self.reset_restored_todo_graces(chat);
            self.emit_conversation_rewound(chat, loop_id, point.message_id);
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
            .clone();
        tokio::task::spawn_blocking(move || history.files_changed_by_rewind(message_id))
            .await
            .map_err(|error| AgentError::Invalid(format!("file comparison ended: {error}")))
    }

    fn emit_conversation_rewound(&self, chat: &mut Chat, loop_id: LoopId, message_id: Uuid) {
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
        workdir: &Path,
    ) -> PrivateDirectory {
        match &self.inner.recorder {
            Some(recorder) => PrivateDirectory::trusted(recorder.file_history_root()),
            // 不持久化时不在工作目录写文件，备份放在系统临时目录的项目目录下（工单 54），
            // 创建时逐层校验属主、权限位与符号链接。
            None => private_project_temp_directory(workdir)
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
                    .and_then(|chat| chat.file_history.next_snapshot_plan())
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
            if let Some(snapshot) = chat.file_history.commit_snapshot(computed) {
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
            let Some(chat) = chats.get(&chat_id) else {
                return;
            };
            if !chat.file_history.needs_backup(path) {
                return;
            }
            chat.file_history.backup_directory().clone()
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
        if let Some(snapshot) = chat.file_history.commit_tracked_edit(path, backup) {
            self.record_file_history_snapshot(chat_id, message_id, &snapshot, true);
        }
    }
}

/// Rewinds before the target prompt, restores its execution mode, and creates an idle draft Loop.
///
/// `loops_at_start` 是 rewind 占用开始时存在的 Loop：只有它们会因创建位置在截断点之后而移除，
/// 连同其排队输入、权限请求与提问；占用期间新建的 Loop 保留，创建位置移到截断点。
fn conversation_before(
    snapshot: &ChatSnapshot,
    point: &RewindPoint,
    loops_at_start: &HashSet<LoopId>,
) -> Result<(ChatSnapshot, LoopId), AgentError> {
    let index = point.message_index;
    let mut candidate = snapshot.clone();
    let goal = prompt_loop_goal(snapshot, index).unwrap_or_else(|| point.prompt.clone());
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
    candidate.denial_references.split_off(&index);
    candidate.message_ids.split_off(&index);
    candidate.original_user_message_ids.split_off(&index);
    candidate.prompt_execution_modes.split_off(&index);
    candidate
        .inherited_prompts
        .retain(|message_index| *message_index < index);
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
    let removed_loops = candidate
        .loop_starts
        .iter()
        .filter(|(loop_id, start)| **start >= index && loops_at_start.contains(loop_id))
        .map(|(loop_id, _)| *loop_id)
        .collect::<HashSet<_>>();
    for start in candidate.loop_starts.values_mut() {
        *start = (*start).min(index);
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
    candidate
        .loop_starts
        .retain(|loop_id, _| !removed_loops.contains(loop_id));
    candidate
        .resume_triggers
        .retain(|loop_id| !removed_loops.contains(loop_id));
    candidate
        .plans
        .retain(|plan| !removed_loops.contains(&plan.loop_id));
    candidate
        .unavailable_plan_versions
        .retain(|loop_id, _| !removed_loops.contains(loop_id));
    candidate
        .progress
        .retain(|progress| !removed_loops.contains(&progress.loop_id));
    if candidate.policy.mode != point.execution_mode {
        candidate.policy.mode = point.execution_mode;
        candidate.policy.revision += 1;
    }
    super::context::projected_messages(&candidate)?;
    let loop_id = Uuid::new_v4();
    candidate.add_loop(LoopSnapshot {
        id: loop_id,
        goal,
        state: LoopState::Idle,
        run_id: None,
        run_started_at: None,
        trigger: None,
        event_sources: Vec::new(),
        state_since: None,
    });
    Ok((candidate, loop_id))
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

fn ensure_idle(chat: &Chat) -> Result<(), AgentError> {
    if chat.active.is_some()
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

fn user_shell_blocks_rewind(shell: &super::UserShellRecord) -> bool {
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

    use async_trait::async_trait;
    use tokio::sync::{broadcast, mpsc::UnboundedSender};

    use super::*;
    use crate::file_history::test_pause;
    use crate::model::{
        InputModality, ModelClient, ModelConfig, ModelDelta, ModelError, ModelIdentity,
        ModelOutput, ModelRequest, Protocol, Usage,
    };
    use crate::runtime::{Event, Receipt, UserShellRecord};

    const WRITE_PREFIX: &str = "write ";
    const STEP_TIMEOUT: Duration = Duration::from_secs(10);

    /// 提示以 `write ` 开头时先 Read 再以 Write 把其余文字写进 tracked.txt，其他请求只回复文字。
    struct EditingModel;

    #[async_trait]
    impl ModelClient for EditingModel {
        async fn generate_title(
            &self,
            request: crate::model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<crate::model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<crate::model::ModelOutput, crate::model::ModelError> {
            Ok(crate::model::fixture_title(request))
        }

        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: crate::model::ModelProvider::Api,
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
                Some(crate::runtime::Trigger {
                    id: Uuid::new_v4(),
                    prompt: "scheduled check".into(),
                    interval_seconds: Some(1),
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
            provider: crate::model::ModelProvider::Api,
            auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
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
            Arc::new(crate::model::HttpModelClient::new(config()).unwrap()),
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
