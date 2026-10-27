//! Rewind, implemented following Claude Code's `rewindConversationTo` and `fileHistoryRewind`.
//!
//! The conversation is truncated only in memory: rewinding to a prompt drops it and every later
//! message, and the next message follows the last kept message, forming a fork in the same
//! Transcript while the abandoned messages stay in the file. Code is restored from the target
//! prompt's file rollback snapshot.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Agent, AgentError, Chat, ChatBookkeeping};
use crate::file_history::{
    FileHistoryError, FileHistorySnapshot, FileRestoreReport, create_first_backup,
};
use crate::transcript::private_project_temp_directory;
use tickloop_harness_base::private_fs::PrivateDirectory;
use tickloop_harness_protocol::local_messages::LocalMessage;
use tickloop_harness_protocol::projection;
use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, CodeRewind, Content, Event, EventKind, ExecutionMode, HostNote,
    LoopState, Message, RewindDraft, RewindMode, RewindOutcome, RewindPoint, Role, RunId,
    TakenBackPrompt, UserShellRecord, UserShellState,
};

impl Agent {
    /// Following Claude Code's Rewind selector: restores code, conversation or both depending on
    /// the mode. Code is restored first, then the conversation is truncated; a failed code restore
    /// does not prevent the conversation restore.
    ///
    /// Runs in three stages: under the lock, check the Chat is idle, precompute the truncated
    /// conversation (an error changes nothing), and occupy the Chat; release the lock and restore
    /// files in a blocking thread while other Chats keep running; relock, truncate the conversation
    /// and release the occupation. While occupied, the Chat cannot start a Run, compaction,
    /// directed summary, Branch or another rewind; input queued meanwhile starts normally after the
    /// occupation is released.
    pub async fn rewind(
        &self,
        chat_id: ChatId,
        message_id: Uuid,
        mode: RewindMode,
    ) -> Result<RewindOutcome, AgentError> {
        self.require_root_chat(chat_id, "rewind history").await?;
        // Held until the task is spawned: `shutdown` either refuses this or waits for the task.
        let admission = self.inner.tasks.token();
        let (point, history) = {
            let mut chats = self.inner.chats.lock().await;
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err(AgentError::Invalid("agent host has shut down".into()));
            }
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            ensure_idle(chat)?;
            let point = chat.snapshot.rewind_point(message_id).ok_or_else(|| {
                AgentError::Invalid("this message is no longer part of the conversation".into())
            })?;
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
                )?;
            }
            chat.active = Some(CancellationToken::new());
            (point, history)
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let agent = self.clone();
        // Run inside a host task so the occupation is released even if the caller stops waiting.
        self.inner.tasks.spawn(async move {
            let restored = match history {
                Some(history) => {
                    Some(tokio::task::spawn_blocking(move || history.rewind(message_id)).await)
                }
                None => None,
            };
            let result = {
                let mut chats = agent.inner.chats.lock().await;
                let Some(chat) = chats.get_mut(&chat_id) else {
                    let _ = sender.send(Err(AgentError::NotFound));
                    return;
                };
                chat.active = None;
                agent.finish_rewind(chat, point, mode, restored)
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

    /// Following Claude Code's automatic restore after an interrupt (`REPL.tsx` calls
    /// `restoreMessageSync` at the end of `onQuery` after checking
    /// `messagesAfterAreOnlySynthetic`): when the user-interrupted `run_id` has no assistant text
    /// or tool calls yet, the conversation is truncated to before its prompt, so the model no
    /// longer sees that prompt, and the Transcript keeps it as an abandoned branch. When the
    /// conditions are not met, nothing changes and `None` is returned, leaving the interrupt in the
    /// conversation as usual.
    pub async fn take_back_interrupted_prompt(
        &self,
        chat_id: ChatId,
        run_id: RunId,
    ) -> Result<Option<TakenBackPrompt>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(AgentError::Invalid("agent host has shut down".into()));
        }
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let Some(point) = interrupted_prompt(chat, run_id) else {
            return Ok(None);
        };
        let candidate = conversation_before(
            &chat.snapshot,
            &chat.bookkeeping,
            point.message_index,
            Some(point.execution_mode),
        )?;
        self.replace_conversation(chat, point.message_index, point.message_id, candidate);
        self.save(&chat.snapshot);
        Ok(Some(TakenBackPrompt {
            prompt: point.prompt,
            contexts: point.contexts,
        }))
    }

    /// Third stage of rewind: truncates the conversation according to the file restore result and
    /// the mode. The conversation is recomputed from the snapshot at the end of the occupation,
    /// keeping changes such as input queued during it.
    fn finish_rewind(
        &self,
        chat: &mut Chat,
        point: RewindPoint,
        mode: RewindMode,
        restored: Option<Result<Result<FileRestoreReport, FileHistoryError>, JoinError>>,
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
            let candidate = conversation_before(
                &chat.snapshot,
                &chat.bookkeeping,
                point.message_index,
                Some(point.execution_mode),
            )?;
            self.replace_conversation(chat, point.message_index, point.message_id, candidate);
            Some(RewindDraft {
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

    /// Files that restoring to the target prompt would change, without touching disk. Returns
    /// `None` when the prompt has no snapshot.
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

    /// Switches to the conversation truncated before message `index` (`message_id` is the first one
    /// cut off): in the Transcript the next message follows the last kept one, and each View
    /// redraws the conversation on receiving `ConversationRewound`.
    pub(super) fn replace_conversation(
        &self,
        chat: &mut Chat,
        index: usize,
        message_id: Uuid,
        (candidate, mut candidate_bookkeeping): (ChatSnapshot, ChatBookkeeping),
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
        candidate_bookkeeping.citation_sources = chat.bookkeeping.citation_sources.clone();
        chat.replace_snapshot(candidate);
        chat.bookkeeping = candidate_bookkeeping;
        self.inner.ledger_changes.send_replace(());
        self.emit_conversation_rewound(chat, message_id);
    }

    pub(super) fn emit_conversation_rewound(&self, chat: &mut Chat, message_id: Uuid) {
        chat.snapshot.last_sequence += 1;
        let event = Event {
            sequence: chat.snapshot.last_sequence,
            chat_id: chat.snapshot.id,
            loop_id: chat.snapshot.chat_loop.id,
            run_id: Uuid::nil().into(),
            kind: EventKind::ConversationRewound { message_id },
        };
        chat.snapshot.events.push(event.clone());
        if chat.snapshot.events.len() > super::EVENT_REPLAY_LIMIT {
            chat.snapshot.events.remove(0);
        }
        let _ = self.inner.events.send(event);
    }

    /// File rollback backup directory: `<root>/file-history/<chatId>` when persisting, following
    /// Claude Code.
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
            // When not persisting, nothing is written in the working directory: backups go under
            // the project directory in the system temp directory (ticket 54), with ownership,
            // permission bits and symlinks checked at each level on creation.
            None => private_project_temp_directory(folder)
                .join(crate::file_history::FILE_HISTORY_DIRECTORY),
        }
        .join(chat_id.uuid().to_string())
    }

    /// Following Claude Code's `fileHistoryMakeSnapshot`: writing a prompt to the conversation only
    /// appends a pending snapshot (`append_input_message`); here, in prompt order, each snapshot's
    /// backups are computed outside the lock, committed under the lock, and written to the
    /// Transcript under the prompt id. Called when a Run starts and after merging input, before any
    /// tool runs in this turn, so the snapshot records the files before this turn's edits, and
    /// edit-tracking snapshot updates are always written after it.
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

    /// Following Claude Code's `fileHistoryTrackEdit`: before an edit tool runs, makes the initial
    /// backup for an untracked file and writes the snapshot update keyed by the assistant message
    /// that issued the edit. A failed backup (including a file replaced by a symlink or not a
    /// regular file) only gives up tracking and does not block the edit.
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
/// cut at its own index was sent in). The Loop stays: only the conversation is cut.
pub(super) fn conversation_before(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    index: usize,
    execution_mode: Option<ExecutionMode>,
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
    candidate.operations = std::mem::take(&mut candidate.operations)
        .into_iter()
        .filter(|operation| {
            operation.parent_call_id.is_none()
                || super::nested_calls::parent_on_chain(&candidate, operation)
        })
        .collect();
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
    // Following Claude Code: when rewind crosses a compaction boundary, the boundary is dropped
    // along with the truncated messages, and the model sees the original conversation before the
    // boundary again; the next message forks in the Transcript from before the boundary.
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
    if let Some(mode) = execution_mode
        && candidate.policy.mode != mode
    {
        candidate.policy.mode = mode;
        candidate.policy.revision += 1;
    }
    projection::projected_messages(&candidate, &candidate_bookkeeping.denial_references)?;
    Ok((candidate, candidate_bookkeeping))
}

/// The prompt the user-interrupted `run_id` can retract. Following Claude Code's guard: the Chat is
/// idle with no queued input (queued input having started after the interrupt means the user has
/// already moved on), and the viewed Agent is not a delegated Subagent; the last prompt in the
/// conversation belongs to this turn, the turn was interrupted, and only messages that do not count
/// as a response follow the prompt.
fn interrupted_prompt(chat: &Chat, run_id: RunId) -> Option<RewindPoint> {
    let snapshot = &chat.snapshot;
    if snapshot.delegation.is_some() || ensure_idle(chat).is_err() {
        return None;
    }
    let (&index, _) = snapshot.prompt_execution_modes.last_key_value()?;
    if snapshot.chat_loop.state != LoopState::Interrupted
        || snapshot.message_runs.get(&index) != Some(&run_id)
    {
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
    snapshot.rewind_point_at(index)
}

/// Following Claude Code's `messagesAfterAreOnlySynthetic`: interrupt markers and assistant
/// messages with no text or tool calls (for example, thinking only) do not count as a response, and
/// all other messages do; host notes written to the model, like Claude Code's meta attachments, do
/// not count either and are excluded by index by the caller.
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
    use tickloop_harness_protocol::Content;

    use async_trait::async_trait;
    use tokio::sync::{broadcast, mpsc::UnboundedSender};

    use super::*;
    use crate::file_history::test_pause;
    use tickloop_harness_model::{
        LocalModelClients, ModelClient, ModelClients, ModelDelta, ModelError, ModelOutput,
        ModelRequest,
    };
    use tickloop_harness_protocol::{Event, Receipt, UserShellRecord};
    use tickloop_harness_protocol::{InputModality, ModelConfig, ModelIdentity, Protocol, Usage};

    const WRITE_PREFIX: &str = "write ";
    const STEP_TIMEOUT: Duration = Duration::from_secs(10);

    /// When the prompt starts with `write `, Reads first and then Writes the remaining text into
    /// tracked.txt; other requests get a text-only reply.
    struct EditingModel;

    #[async_trait]
    impl ModelClient for EditingModel {
        async fn generate_title(
            &self,
            request: tickloop_harness_model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError>
        {
            Ok(tickloop_harness_model::fixture_title(request))
        }

        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: tickloop_harness_protocol::ModelProvider::Api,
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
        let receipt = agent.submit(chat_id, prompt.into()).await.unwrap();
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

    /// Two Chats have each edited tracked.txt in their own working directory, so later prompts
    /// snapshot it.
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
    async fn rewind_keeps_the_loop_and_runs_the_input_queued_meanwhile() {
        let TwoChats {
            _directory,
            agent,
            mut events,
            first,
            first_tracked,
            ..
        } = two_chats_with_tracked_files().await;
        run_prompt(&agent, &mut events, first, "write watching agent").await;
        let snapshot = agent.snapshot(first).await.unwrap();
        let loop_id = snapshot.chat_loop.id;
        let point = snapshot
            .rewind_points()
            .into_iter()
            .find(|point| point.prompt == "write watching agent")
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
        let queued = agent
            .submit(first, "talk during rewind".into())
            .await
            .unwrap();
        let triggered = agent
            .deliver_trigger_for_tests(
                first,
                tickloop_harness_protocol::TriggerSource::ScheduledTask {
                    task_id: Uuid::from_u128(0x5e1d),
                    scheduled_for_ms: 1,
                    wrappers: Vec::new(),
                },
                "event check",
            )
            .await
            .unwrap();
        assert!(
            agent
                .snapshot(first)
                .await
                .unwrap()
                .pending_inputs
                .iter()
                .any(|pending| pending.trigger_source.is_some()),
            "the event did not queue during the rewind"
        );
        drop(pause);
        let outcome = tokio::time::timeout(STEP_TIMEOUT, rewinding)
            .await
            .expect("rewind finishes after the restore is released")
            .unwrap()
            .unwrap();
        assert_eq!(outcome.snapshot.chat_loop.id, loop_id);
        assert!(outcome.snapshot.chat_loop.receiving.is_receiving());
        for receipt in [&queued, &triggered] {
            tokio::time::timeout(
                STEP_TIMEOUT,
                wait_for_completion(&agent, &mut events, receipt),
            )
            .await
            .expect("input queued during the rewind runs afterwards");
        }
        let after = agent.snapshot(first).await.unwrap();
        assert!(after.pending_inputs.is_empty());
        assert_eq!(after.chat_loop.id, loop_id);
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
        let snapshotting = agent.submit(first, "talk".into()).await.unwrap();
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
            provider: tickloop_harness_protocol::ModelProvider::Api,
            auth_source: tickloop_harness_protocol::ModelAuthSource::ApiKeyEnv,
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
            chat_id: Uuid::new_v4().into(),
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
