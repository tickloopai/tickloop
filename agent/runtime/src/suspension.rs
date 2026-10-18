//! Durable waits. A Run suspends where only an outside event ends its wait, such as a question
//! for the user: the wait and where the Run goes on are recorded in the Transcript. With durable
//! wait hosting, a load rebuilds each suspended Run as a parked one, and a shutdown releases a
//! suspended Run without writing anything, so whichever instance loads the Chat next goes on
//! with it.

use std::collections::HashSet;
use std::sync::Arc;

use tickloop_agent_protocol::{
    ChatSnapshot, Content, LoopState, OperationRecord, OperationState, QuestionRequest, Receipt,
    RunId, ToolResult,
};
use tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME;
use tickloop_agent_transcript::{
    Line, ModelVisibleResult, ResumePoint, WaitEntry, WaitKind, WaitState,
};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    Agent, Chat, ParkedRun, RunStart, WaitHosting, interaction::PendingReply, lifecycle::Lifecycle,
    run_supervision, tool_batch,
};

/// Why a released Run's wait ended: it leaves this instance and nothing of it is written.
pub(crate) const RUN_RELEASED: &str = "the waiting run was released to another host";
const QUESTION_CANCELLED: &str = "question was cancelled";
const QUESTION_EXPIRED: &str = "question expired";

/// The canonical id of the Run's assistant message that made the call `call_id`.
fn batch_message_of(snapshot: &ChatSnapshot, run: RunId, call_id: &str) -> Option<Uuid> {
    let index = (0..snapshot.messages.len()).rev().find(|index| {
        snapshot.message_runs.get(index) == Some(&run)
            && snapshot.messages[*index]
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolUse { id, .. } if id == call_id))
    })?;
    snapshot
        .original_message_ids
        .get(&index)
        .or_else(|| snapshot.message_ids.get(&index))
        .copied()
}

/// The suspended waits a load rebuilds under `waits`: under durable wait hosting, those whose
/// Loop goes on and whose batch the loaded chain holds. Every other unended answer lost its host.
pub(crate) fn rebuilt_waits(
    waits: WaitHosting,
    lifecycle: &Lifecycle,
    snapshot: &ChatSnapshot,
) -> Vec<WaitEntry> {
    if waits == WaitHosting::Process {
        return Vec::new();
    }
    lifecycle
        .suspended_waits()
        .into_iter()
        .filter(|wait| {
            let ResumePoint::InBatch {
                assistant_message,
                call_id,
            } = &wait.resume;
            let loop_goes_on = snapshot.loops.iter().any(|goal| {
                goal.id == wait.loop_id
                    && !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
            });
            loop_goes_on
                && tool_batch::locate_batch_call(snapshot, wait.run_id, *assistant_message, call_id)
                    .is_ok()
        })
        .collect()
}

/// The calls the waits of suspended answers in `lines` are on. A load keeps their batch
/// messages although no call of them has a result yet.
pub(crate) fn waited_calls(waits: WaitHosting, lines: &[Line]) -> HashSet<String> {
    match waits {
        WaitHosting::Process => HashSet::new(),
        WaitHosting::Durable => Lifecycle::restore(lines, true)
            .suspended_waits()
            .into_iter()
            .map(|wait| {
                let ResumePoint::InBatch { call_id, .. } = wait.resume;
                call_id
            })
            .collect(),
    }
}

impl Agent {
    /// The question `request` suspends its Run: the wait is recorded with the answer's
    /// `Suspended`, in the change that makes the Loop wait for it, and the Run gets its release.
    pub(crate) fn suspend_for_question(
        &self,
        chat: &mut Chat,
        receipt: &Receipt,
        request: &QuestionRequest,
    ) -> CancellationToken {
        let operation_id = chat
            .snapshot
            .operations
            .iter()
            .find(|operation| {
                operation.run_id == receipt.run_id && operation.call_id == request.call_id
            })
            .map(|operation| operation.id);
        let assistant_message = batch_message_of(&chat.snapshot, receipt.run_id, &request.call_id);
        if let (Some(operation_id), Some(assistant_message)) = (operation_id, assistant_message) {
            chat.bookkeeping.lifecycle.suspend(
                receipt,
                WaitKind::Question {
                    request: request.clone(),
                    operation_id,
                },
                ResumePoint::InBatch {
                    assistant_message,
                    call_id: request.call_id.clone(),
                },
            );
        }
        chat.releases.entry(receipt.run_id).or_default().clone()
    }

    /// The release of `receipt`'s Run, which never fires for a Run that does not wait durably.
    pub(crate) fn release_of(chat: &Chat, run: RunId) -> CancellationToken {
        chat.releases.get(&run).cloned().unwrap_or_default()
    }

    /// Whether the Run was released; a released Run is taken out of this instance without
    /// ending it, its answer, its input or its Loop.
    pub(crate) async fn leave_if_released(&self, receipt: &Receipt) -> bool {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&receipt.chat_id) else {
            return false;
        };
        if !chat
            .releases
            .get(&receipt.run_id)
            .is_some_and(CancellationToken::is_cancelled)
        {
            return false;
        }
        run_supervision::clear_run_state(chat, receipt.run_id);
        drop(chats);
        self.inner
            .host_contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .runs
            .remove(&receipt.run_id);
        true
    }

    /// At shutdown: a Run whose answer is suspended is released when this host keeps no waits
    /// of its own; every other Run is cancelled, as before.
    pub(crate) fn stop_runs_at_shutdown(&self, chat: &Chat) {
        let suspended = match self.inner.waits {
            WaitHosting::Process => HashSet::new(),
            WaitHosting::Durable => chat
                .releases
                .keys()
                .copied()
                .filter(|run| {
                    let lifecycle = &chat.bookkeeping.lifecycle;
                    lifecycle
                        .active_response(*run)
                        .and_then(|response| lifecycle.response(response))
                        .is_some_and(|response| {
                            response.state == tickloop_agent_transcript::ResponseState::Suspended
                        })
                })
                .collect::<HashSet<_>>(),
        };
        for run in &suspended {
            chat.releases[run].cancel();
        }
        if let Some(cancel) = &chat.active
            && chat.active_run.is_none_or(|run| !suspended.contains(&run))
        {
            cancel.cancel();
        }
        for (run, waiting) in &chat.parked {
            if !suspended.contains(run) {
                waiting.cancel.cancel();
            }
        }
    }

    /// Rebuilds every suspended Run of a Chat a durable load restored, as the parked Run it was:
    /// an open question waits for its reply again, and a wait that already ended with a result
    /// not yet on the chain is ready to go on.
    pub(crate) async fn rebuild_suspended_runs(&self, chat_id: tickloop_agent_protocol::ChatId) {
        let mut rebuilt = Vec::new();
        {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return;
            };
            let waits = rebuilt_waits(
                WaitHosting::Durable,
                &chat.bookkeeping.lifecycle,
                &chat.snapshot,
            );
            let first_sequence = chat
                .snapshot
                .last_sequence
                .saturating_sub(waits.len() as u64);
            // Rebuilt Runs suspended before anything this instance parks, in the order they did.
            for (order, wait) in waits.into_iter().enumerate() {
                let WaitKind::Question {
                    request,
                    operation_id,
                } = &wait.kind;
                let receipt = Receipt {
                    chat_id,
                    loop_id: wait.loop_id,
                    run_id: wait.run_id,
                };
                let goal = chat
                    .snapshot
                    .loops
                    .iter_mut()
                    .find(|goal| goal.id == wait.loop_id)
                    .expect("a rebuilt wait's Loop goes on");
                goal.run_id = Some(wait.run_id);
                goal.recover_state(LoopState::WaitingQuestion);
                let cancel = CancellationToken::new();
                let release = CancellationToken::new();
                let resume = Arc::new(Notify::new());
                chat.parked.insert(
                    wait.run_id,
                    ParkedRun {
                        receipt: receipt.clone(),
                        cancel: cancel.clone(),
                        resume: resume.clone(),
                        sequence: first_sequence + order as u64,
                    },
                );
                chat.releases.insert(wait.run_id, release.clone());
                chat.bookkeeping.run_loops.insert(wait.run_id, wait.loop_id);
                chat.bookkeeping.lifecycle.reactivate(
                    wait.run_id,
                    wait.response_id,
                    chat.snapshot.messages.len(),
                );
                let input = request_input(&chat.snapshot, &wait).unwrap_or_default();
                let (state, result, receiver) = match &wait.state {
                    WaitState::Resolved { result, .. } => {
                        chat.ready_runs.push_back(wait.run_id);
                        match result.tool.is_error {
                            true => (OperationState::Admitted, None, None),
                            false => (OperationState::Completed, Some(result.tool.clone()), None),
                        }
                    }
                    _ => {
                        chat.snapshot.questions.push(request.clone());
                        let (sender, receiver) = oneshot::channel();
                        self.inner
                            .question_answers
                            .lock()
                            .await
                            .insert(request.id, sender);
                        (OperationState::Admitted, None, Some(receiver))
                    }
                };
                chat.snapshot.operations.push(OperationRecord {
                    task_binding: None,
                    id: *operation_id,
                    loop_id: wait.loop_id,
                    run_id: wait.run_id,
                    call_id: request.call_id.clone(),
                    name: ASK_USER_QUESTION_TOOL_NAME.into(),
                    input,
                    file_change: None,
                    state,
                    result,
                });
                rebuilt.push(RebuiltQuestion {
                    receipt,
                    wait_id: wait.wait_id,
                    receiver,
                    cancel,
                    release,
                    resume,
                });
            }
            if !rebuilt.is_empty() {
                self.save(&chat.snapshot);
            }
        }
        for question in rebuilt {
            let agent = self.clone();
            self.inner.tasks.spawn(Box::pin(
                async move { agent.continue_rebuilt(question).await },
            ));
        }
        if let Ok(Some(next)) = self.next_run(chat_id).await {
            self.run_task(next);
        }
    }

    /// A rebuilt Run's wait tail, as the asking Run's own: the reply, then the execution slot,
    /// then the batch again at the waiting call with the result the reply decided.
    async fn continue_rebuilt(self, question: RebuiltQuestion) {
        let RebuiltQuestion {
            receipt,
            wait_id,
            receiver,
            cancel,
            release,
            resume,
        } = question;
        let waited = match receiver {
            Some(receiver) => {
                let reply = tokio::select! {
                    biased;
                    _ = release.cancelled() => Err(RUN_RELEASED.to_string()),
                    _ = cancel.cancelled() => Err(QUESTION_CANCELLED.to_string()),
                    reply = receiver => reply.map(|_: PendingReply| ()).map_err(|_| QUESTION_EXPIRED.to_string()),
                };
                match reply {
                    Ok(()) => self.resume_parked_run(&receipt, cancel.clone()).await,
                    Err(error) => Err(error),
                }
            }
            None => tokio::select! {
                biased;
                _ = release.cancelled() => Err(RUN_RELEASED.to_string()),
                _ = cancel.cancelled() => Err(QUESTION_CANCELLED.to_string()),
                _ = resume.notified() => Ok(()),
            },
        };
        if self.leave_if_released(&receipt).await {
            return;
        }
        let mut next = match self.resume_start(&receipt, wait_id, waited).await {
            Some(start) => self.run(receipt, start, cancel).await,
            // The Chat lost the wait: the Run cannot go on, so it ends.
            None => {
                self.finish_run(receipt, cancel, Err(QUESTION_EXPIRED.into()), false, false)
                    .await
            }
        };
        while let Some((receipt, request, cancel)) = next {
            next = self.run(receipt, request, cancel).await;
        }
    }

    /// Where the rebuilt Run goes on: at its waiting call, with the result the reply decided, or
    /// with the error its wait ended in, as the asking Run's own call reports a cancelled or
    /// expired question. `None` when the Chat no longer holds the wait.
    async fn resume_start(
        &self,
        receipt: &Receipt,
        wait_id: Uuid,
        waited: Result<(), String>,
    ) -> Option<RunStart> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id)?;
        let wait = chat.bookkeeping.lifecycle.wait(wait_id)?.clone();
        let result = match (waited, wait.state) {
            // As the asking Run's own: an answer the user gave reaches the chain even when the
            // Run is cancelled before it takes the slot again.
            (waited, WaitState::Resolved { result, .. })
                if waited.is_ok() || !result.tool.is_error =>
            {
                result
            }
            (waited, _) => {
                chat.snapshot
                    .questions
                    .retain(|request| request.id != wait_id);
                self.inner.question_answers.lock().await.remove(&wait_id);
                self.save(&chat.snapshot);
                let error = waited.err().unwrap_or_else(|| QUESTION_EXPIRED.into());
                ModelVisibleResult {
                    tool: ToolResult {
                        content: error.clone(),
                        is_error: true,
                        output_ref: None,
                    },
                    content: error.into(),
                    ends_run: false,
                }
            }
        };
        Some(RunStart::Resume {
            at: wait.resume,
            result: Some(result),
        })
    }
}

/// A Run rebuilt around its question, and what its wait tail waits on.
struct RebuiltQuestion {
    receipt: Receipt,
    wait_id: Uuid,
    /// The reply to an open question; `None` when the wait already ended.
    receiver: Option<oneshot::Receiver<PendingReply>>,
    cancel: CancellationToken,
    release: CancellationToken,
    resume: Arc<Notify>,
}

/// The input of the call a wait is on, from its batch message.
fn request_input(snapshot: &ChatSnapshot, wait: &WaitEntry) -> Option<serde_json::Value> {
    let ResumePoint::InBatch {
        assistant_message,
        call_id,
    } = &wait.resume;
    let (batch, start) =
        tool_batch::locate_batch_call(snapshot, wait.run_id, *assistant_message, call_id).ok()?;
    batch.calls.into_iter().nth(start).map(|call| call.input)
}
