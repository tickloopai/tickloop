//! Durable waits. A Run suspends where only an outside event ends its wait, such as a question
//! for the user: the wait and where the Run goes on are recorded in the Transcript. With durable
//! wait hosting, a load rebuilds each suspended Run as a parked one, and a shutdown releases a
//! suspended Run without writing anything, so whichever instance loads the Chat next goes on
//! with it.

use std::collections::HashSet;
use std::sync::Arc;

use tickloop_harness_protocol::{
    ChatSnapshot, Content, LoopState, OperationRecord, OperationState, QuestionRequest, Receipt,
    RunId, ToolResult,
};
use tickloop_harness_tools::ASK_USER_QUESTION_TOOL_NAME;
use tickloop_harness_transcript::{
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
const HANDED_OVER_RUN_CANCELLED: &str = "the run was cancelled before it went on";
const HANDOVER_LOST: &str = "the run's handover is no longer recorded";

/// The canonical id of the Run's assistant message that made the call `call_id`.
pub(crate) fn batch_message_of(snapshot: &ChatSnapshot, run: RunId, call_id: &str) -> Option<Uuid> {
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
        .filter(|wait| match &wait.resume {
            ResumePoint::InBatch {
                assistant_message,
                call_id,
            } => tool_batch::locate_batch_call(
                snapshot,
                wait.run_id.into(),
                *assistant_message,
                call_id,
            )
            .is_ok(),
            ResumePoint::BeforeModelRequest => true,
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
            .filter_map(|wait| wait.resume.call_id().map(str::to_owned))
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

    /// The Runs of `chat` whose answer is suspended, when this host keeps no waits of its own.
    fn suspended_runs(&self, chat: &Chat) -> HashSet<RunId> {
        match self.inner.waits {
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
                            response.state == tickloop_harness_transcript::ResponseState::Suspended
                        })
                })
                .collect(),
        }
    }

    /// At shutdown, first: a Run whose answer is suspended is released when this host keeps no
    /// waits of its own, before anything else of the shutdown can end its wait.
    pub(crate) fn release_runs_at_shutdown(&self, chat: &Chat) {
        for run in self.suspended_runs(chat) {
            chat.releases[&run].cancel();
        }
    }

    /// At shutdown: every Run that was not released is cancelled, as before.
    pub(crate) fn stop_runs_at_shutdown(&self, chat: &Chat) {
        let suspended = self.suspended_runs(chat);
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

    /// The foreground `Subagent` call `call_id` of `parent`'s Run waits for the child run `child`
    /// to end: the wait is recorded with the answer's `Suspended`, and the Run gets its release.
    /// A child run whose end is already recorded ends the wait at once.
    pub(crate) async fn suspend_for_subagent(
        &self,
        parent: &Receipt,
        call_id: &str,
        child: &Receipt,
    ) -> Result<CancellationToken, String> {
        let registration = self
            .subagent_registration(child.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut chats = self.inner.chats.lock().await;
        let delegation = chats
            .get(&child.chat_id)
            .and_then(|chat| chat.snapshot.delegation.clone());
        let chat = chats
            .get_mut(&parent.chat_id)
            .ok_or("parent is unavailable")?;
        let lifecycle = &chat.bookkeeping.lifecycle;
        let is_waiting = lifecycle
            .open_wait_of(parent.run_id, |kind| {
                matches!(kind, WaitKind::Subagent { child_run, .. } if *child_run == child.run_id)
            })
            .is_some();
        let operation_id = chat
            .snapshot
            .operations
            .iter()
            .find(|operation| operation.run_id == parent.run_id && operation.call_id == call_id)
            .map(|operation| operation.id);
        let assistant_message = batch_message_of(&chat.snapshot, parent.run_id, call_id);
        if !is_waiting
            && let (Some(operation_id), Some(assistant_message)) = (operation_id, assistant_message)
            && chat
                .bookkeeping
                .lifecycle
                .suspend(
                    parent,
                    WaitKind::Subagent {
                        child_chat: child.chat_id.into(),
                        child_loop: child.loop_id.into(),
                        child_run: child.run_id.into(),
                        operation_id,
                    },
                    ResumePoint::InBatch {
                        assistant_message,
                        call_id: call_id.into(),
                    },
                )
                .is_some()
            && let (Some(delegation), Some(registration)) = (&delegation, &registration)
            && let Some(completion) = chat
                .snapshot
                .family_completions
                .iter()
                .find(|completion| {
                    completion.child_chat_id == child.chat_id
                        && completion.child_run_id == child.run_id
                })
                .cloned()
        {
            crate::family::resolve_subagent_wait(chat, &completion, delegation, registration);
        }
        let release = chat.releases.entry(parent.run_id).or_default().clone();
        self.save_now(&chat.snapshot);
        Ok(release)
    }

    /// The delegated member's Run waits for the children `children` to report, at its call
    /// `at_call` or else before its next model request: the wait is recorded with the answer's
    /// `Suspended`, unless the Run already waits on them, and the Run gets its release.
    pub(crate) async fn suspend_for_children(
        &self,
        receipt: &Receipt,
        children: Vec<tickloop_harness_protocol::ChatId>,
        at_call: Option<&str>,
    ) -> CancellationToken {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&receipt.chat_id) else {
            return CancellationToken::new();
        };
        let is_waiting = chat
            .bookkeeping
            .lifecycle
            .open_wait_of(receipt.run_id, |kind| {
                matches!(kind, WaitKind::Children { .. })
            })
            .is_some();
        let resume =
            match at_call {
                Some(call_id) => batch_message_of(&chat.snapshot, receipt.run_id, call_id).map(
                    |assistant_message| ResumePoint::InBatch {
                        assistant_message,
                        call_id: call_id.into(),
                    },
                ),
                None => Some(ResumePoint::BeforeModelRequest),
            };
        if !is_waiting && let Some(resume) = resume {
            chat.bookkeeping.lifecycle.suspend(
                receipt,
                WaitKind::Children {
                    children: children.into_iter().map(Into::into).collect(),
                },
                resume,
            );
        }
        let release = chat.releases.entry(receipt.run_id).or_default().clone();
        self.save_now(&chat.snapshot);
        release
    }

    /// The member's Run waited for its children and goes on: before its next model request
    /// its wait is over, in one change with its answer's `Resumed`; at its report call the wait
    /// ends with the result that call reports, which reaches the chain as any call's does.
    /// Nothing when it did not wait.
    pub(crate) async fn end_children_wait(&self, receipt: &Receipt) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&receipt.chat_id) else {
            return;
        };
        let Some(wait) = chat
            .bookkeeping
            .lifecycle
            .open_wait_of(receipt.run_id, |kind| {
                matches!(kind, WaitKind::Children { .. })
            })
            .cloned()
        else {
            return;
        };
        match wait.resume {
            ResumePoint::BeforeModelRequest => {
                chat.bookkeeping.lifecycle.end_wait(wait.wait_id);
            }
            ResumePoint::InBatch { .. } => {
                chat.bookkeeping.lifecycle.resolve_wait(
                    wait.wait_id,
                    children_finished_result(crate::handback::CHILDREN_FINISHED.into()),
                    None,
                );
            }
        }
        self.save_now(&chat.snapshot);
    }

    /// Registers every suspended Run of a Chat a durable load restored as the Run it was: an
    /// open question waits for its reply again as a parked Run, a foreground call or a member
    /// waiting for its children holds the execution slot again, and a wait that already ended
    /// with a result not yet on the chain is ready to go on. Nothing runs until
    /// [`Agent::start_rebuilt_runs`].
    pub(crate) async fn register_suspended_runs(
        &self,
        chat_id: tickloop_harness_protocol::ChatId,
    ) -> Vec<RebuiltRun> {
        let mut rebuilt = Vec::new();
        let mut invocations = Vec::new();
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return rebuilt;
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
            let receipt = Receipt {
                chat_id,
                loop_id: chat.snapshot.chat_loop.id,
                run_id: wait.run_id.into(),
            };
            let is_resolved = matches!(wait.state, WaitState::Resolved { .. });
            let holds_slot = !is_resolved
                && matches!(
                    wait.kind,
                    WaitKind::Subagent { .. } | WaitKind::Children { .. }
                )
                && chat.active.is_none();
            let chat_loop = &mut chat.snapshot.chat_loop;
            chat_loop.run_id = Some(wait.run_id.into());
            chat_loop.recover_state(match wait.kind {
                WaitKind::Question { .. } => LoopState::WaitingQuestion,
                _ => LoopState::Running,
            });
            let cancel = CancellationToken::new();
            let release = CancellationToken::new();
            let resume = Arc::new(Notify::new());
            if holds_slot {
                chat.active = Some(cancel.clone());
                chat.active_run = Some(wait.run_id.into());
            } else {
                chat.parked.insert(
                    wait.run_id.into(),
                    ParkedRun {
                        receipt: receipt.clone(),
                        cancel: cancel.clone(),
                        resume: resume.clone(),
                        sequence: first_sequence + order as u64,
                    },
                );
            }
            chat.releases.insert(wait.run_id.into(), release.clone());
            chat.bookkeeping
                .run_loops
                .insert(wait.run_id.into(), receipt.loop_id);
            chat.bookkeeping.lifecycle.reactivate(
                wait.run_id.into(),
                wait.response_id.into(),
                chat.snapshot.messages.len(),
            );
            let call_input = request_input(&chat.snapshot, &wait).unwrap_or_default();
            let call_id = wait.resume.call_id().map(str::to_owned);
            let mut tail = RebuiltTail::Ready;
            match &wait.kind {
                WaitKind::Question {
                    request,
                    operation_id,
                } => {
                    let (state, result) = match &wait.state {
                        WaitState::Resolved { result, .. } if !result.tool.is_error => {
                            (OperationState::Completed, Some(result.tool.clone()))
                        }
                        _ => (OperationState::Admitted, None),
                    };
                    if !is_resolved {
                        chat.snapshot.questions.push(request.clone());
                        let (sender, receiver) = oneshot::channel();
                        self.inner
                            .question_answers
                            .lock()
                            .await
                            .insert(request.id, sender);
                        tail = RebuiltTail::Question(receiver);
                    }
                    chat.snapshot.operations.push(OperationRecord {
                        parent_call_id: None,
                        parent_call_index: None,
                        task_binding: None,
                        id: *operation_id,
                        loop_id: wait.loop_id.into(),
                        run_id: wait.run_id.into(),
                        call_id: request.call_id.clone(),
                        name: ASK_USER_QUESTION_TOOL_NAME.into(),
                        input: call_input,
                        file_change: None,
                        state,
                        result,
                    });
                }
                WaitKind::Subagent {
                    child_chat,
                    child_loop,
                    child_run,
                    operation_id,
                } => {
                    let child = Receipt {
                        chat_id: (*child_chat).into(),
                        loop_id: (*child_loop).into(),
                        run_id: (*child_run).into(),
                    };
                    let call_id = call_id.clone().unwrap_or_default();
                    // The call goes on returning the child's run, so its completion is no
                    // notification, unless send now resolved the wait before the run ended.
                    let is_moved = is_resolved
                        && !chat.snapshot.family_completions.iter().any(|completion| {
                            completion.child_chat_id == child.chat_id
                                && completion.child_run_id == child.run_id
                        });
                    invocations.push((
                        (wait.run_id, call_id.clone()),
                        crate::delegation::SubagentInvocation {
                            receipt: child.clone(),
                            run_in_background: is_moved,
                        },
                    ));
                    chat.snapshot.operations.push(OperationRecord {
                        parent_call_id: None,
                        parent_call_index: None,
                        task_binding: None,
                        id: *operation_id,
                        loop_id: wait.loop_id.into(),
                        run_id: wait.run_id.into(),
                        call_id: call_id.clone(),
                        name: tickloop_harness_protocol::SUBAGENT_TOOL_NAME.into(),
                        input: call_input,
                        file_change: None,
                        state: OperationState::Admitted,
                        result: None,
                    });
                    if !is_resolved {
                        tail = RebuiltTail::Subagent { child, call_id };
                    }
                }
                WaitKind::Children { .. } => {
                    tail = RebuiltTail::Children {
                        at_call: call_id.clone(),
                    };
                }
                WaitKind::Handover => tail = RebuiltTail::Handover,
            }
            // A wait that ended, or a slot-holding wait whose slot another Run took, goes on once
            // the Run gets the slot.
            if is_resolved || (!holds_slot && !matches!(wait.kind, WaitKind::Question { .. })) {
                chat.ready_runs.push_back(wait.run_id.into());
            }
            rebuilt.push(RebuiltRun {
                receipt,
                wait_id: wait.wait_id,
                tail,
                holds_slot,
                cancel,
                release,
                resume,
            });
        }
        if !rebuilt.is_empty() {
            self.save(&chat.snapshot);
        }
        drop(chats);
        self.inner.subagent_invocations.lock().await.extend(
            invocations
                .into_iter()
                .map(|((run, call), invocation)| ((run.into(), call), invocation)),
        );
        rebuilt
    }

    /// Starts the wait tail of every Run [`Agent::register_suspended_runs`] rebuilt.
    pub(crate) fn start_rebuilt_runs(&self, rebuilt: Vec<RebuiltRun>) {
        for run in rebuilt {
            let agent = self.clone();
            self.inner
                .tasks
                .spawn(Box::pin(async move { agent.continue_rebuilt(run).await }));
        }
    }

    /// A rebuilt Run's wait tail, as the waiting Run's own: the end of its wait, then the
    /// execution slot, then the Run again where it waited.
    async fn continue_rebuilt(self, run: RebuiltRun) {
        let RebuiltRun {
            receipt,
            wait_id,
            tail,
            holds_slot,
            cancel,
            release,
            resume,
        } = run;
        // A Run that does not hold the slot takes it first, as a parked Run does.
        let takes_slot = |waited: Result<(), String>| {
            let agent = self.clone();
            let receipt = receipt.clone();
            let cancel = cancel.clone();
            async move {
                match waited {
                    Ok(()) if !holds_slot => agent.resume_parked_run(&receipt, cancel).await,
                    waited => waited,
                }
            }
        };
        let start = match tail {
            RebuiltTail::Question(receiver) => {
                let reply = tokio::select! {
                    biased;
                    _ = release.cancelled() => Err(RUN_RELEASED.to_string()),
                    _ = cancel.cancelled() => Err(QUESTION_CANCELLED.to_string()),
                    reply = receiver => reply.map(|_: PendingReply| ()).map_err(|_| QUESTION_EXPIRED.to_string()),
                };
                let waited = takes_slot(reply).await;
                if self.leave_if_released(&receipt).await {
                    return;
                }
                self.resume_start(&receipt, wait_id, waited).await
            }
            RebuiltTail::Ready => {
                let waited = tokio::select! {
                    biased;
                    _ = release.cancelled() => Err(RUN_RELEASED.to_string()),
                    _ = cancel.cancelled() => Err(QUESTION_CANCELLED.to_string()),
                    _ = resume.notified() => Ok(()),
                };
                if self.leave_if_released(&receipt).await {
                    return;
                }
                self.resume_start(&receipt, wait_id, waited).await
            }
            RebuiltTail::Subagent { child, call_id } => {
                let waited = match holds_slot {
                    true => Ok(()),
                    false => {
                        tokio::select! {
                            biased;
                            _ = release.cancelled() => Err(RUN_RELEASED.to_string()),
                            _ = cancel.cancelled() => Err(QUESTION_CANCELLED.to_string()),
                            _ = resume.notified() => Ok(()),
                        }
                    }
                };
                let observed = match waited {
                    Ok(()) => {
                        self.observe_rebuilt_child(
                            &receipt, wait_id, &call_id, &child, &cancel, &release,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                if self.leave_if_released(&receipt).await {
                    return;
                }
                self.subagent_resume_start(&receipt, wait_id, observed)
                    .await
            }
            RebuiltTail::Handover => {
                let waited = tokio::select! {
                    biased;
                    _ = release.cancelled() => Err(RUN_RELEASED.to_string()),
                    _ = cancel.cancelled() => Err(HANDED_OVER_RUN_CANCELLED.to_string()),
                    _ = resume.notified() => Ok(()),
                };
                if self.leave_if_released(&receipt).await {
                    return;
                }
                self.handover_resume_start(&receipt, wait_id, waited).await
            }
            RebuiltTail::Children { at_call } => {
                let waited = match holds_slot {
                    true => Ok(()),
                    false => {
                        tokio::select! {
                            biased;
                            _ = release.cancelled() => Err(RUN_RELEASED.to_string()),
                            _ = cancel.cancelled() => Err(QUESTION_CANCELLED.to_string()),
                            _ = resume.notified() => Ok(()),
                        }
                    }
                };
                let waited = match waited {
                    Ok(()) => self
                        .wait_for_subagents(&receipt, &cancel, at_call.as_deref())
                        .await
                        .map(|_| ()),
                    Err(error) => Err(error),
                };
                if self.leave_if_released(&receipt).await {
                    return;
                }
                self.children_resume_start(&receipt, wait_id, at_call, waited)
                    .await
            }
        };
        let mut next = match start {
            Ok(start) => self.run(receipt, start, cancel).await,
            // The Run cannot go on where it waited, so it ends with why.
            Err(error) => {
                self.finish_run(receipt, cancel, Err(error), false, false)
                    .await
            }
        };
        while let Some((receipt, request, cancel)) = next {
            next = self.run(receipt, request, cancel).await;
        }
    }

    /// Observes a rebuilt foreground call's child run until the family records its end, as the
    /// call itself observes it, and gives what the call returns: the result its wait ended with,
    /// such as one a load decided for a child it found lost. An interruption ends the
    /// observation as it does there.
    async fn observe_rebuilt_child(
        &self,
        receipt: &Receipt,
        wait_id: Uuid,
        call_id: &str,
        child: &Receipt,
        cancel: &CancellationToken,
        release: &CancellationToken,
    ) -> Result<ModelVisibleResult, String> {
        let mut events = self.subscribe();
        loop {
            if let Some(result) = self.resolved_result(receipt, wait_id).await {
                return Ok(result);
            }
            if let Some(result) = self.recorded_child_result(receipt.chat_id, child).await? {
                return self.waited_result(receipt, call_id, result).await;
            }
            tokio::select! {
                biased;
                _ = release.cancelled() => return Err(RUN_RELEASED.into()),
                _ = cancel.cancelled() => {
                    let result = self.end_interrupted_observation(receipt, call_id, child).await?;
                    return self.waited_result(receipt, call_id, result).await;
                }
                event = events.recv() => match event {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err("Agent event stream closed while observing child".into());
                    }
                },
            }
        }
    }

    /// The result the wait `wait_id` of `receipt`'s Run ended with, if it ended.
    async fn resolved_result(
        &self,
        receipt: &Receipt,
        wait_id: Uuid,
    ) -> Option<ModelVisibleResult> {
        let chats = self.inner.chats.lock().await;
        match &chats
            .get(&receipt.chat_id)?
            .bookkeeping
            .lifecycle
            .wait(wait_id)?
            .state
        {
            WaitState::Resolved { result, .. } => Some(result.clone()),
            _ => None,
        }
    }

    /// The result a foreground call's own text records for the model, stored as the call's own
    /// result is when it is large.
    async fn waited_result(
        &self,
        receipt: &Receipt,
        call_id: &str,
        result: ToolResult,
    ) -> Result<ModelVisibleResult, String> {
        let tool = self
            .persist_large_tool_result(
                receipt,
                tickloop_harness_protocol::SUBAGENT_TOOL_NAME,
                call_id,
                result,
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(ModelVisibleResult {
            content: tool.content.clone().into(),
            tool,
            ends_run: false,
        })
    }

    /// Where a rebuilt foreground call goes on: at its call, with what it returns.
    async fn subagent_resume_start(
        &self,
        receipt: &Receipt,
        wait_id: Uuid,
        observed: Result<ModelVisibleResult, String>,
    ) -> Result<RunStart, String> {
        let result = observed?;
        let chats = self.inner.chats.lock().await;
        let wait = chats
            .get(&receipt.chat_id)
            .and_then(|chat| chat.bookkeeping.lifecycle.wait(wait_id))
            .ok_or(QUESTION_EXPIRED)?
            .clone();
        Ok(RunStart::Resume {
            at: wait.resume,
            result: Some(result),
        })
    }

    /// Where a rebuilt member goes on once its children reported: before its next model request,
    /// or at the report call it was making, which then reports nothing, as the member's own call
    /// does after it waited.
    async fn children_resume_start(
        &self,
        receipt: &Receipt,
        wait_id: Uuid,
        at_call: Option<String>,
        waited: Result<(), String>,
    ) -> Result<RunStart, String> {
        let chats = self.inner.chats.lock().await;
        let wait = chats
            .get(&receipt.chat_id)
            .and_then(|chat| chat.bookkeeping.lifecycle.wait(wait_id))
            .ok_or(QUESTION_EXPIRED)?
            .clone();
        match at_call {
            None => waited.map(|()| RunStart::Resume {
                at: wait.resume,
                result: None,
            }),
            Some(_) => {
                // The report call reports what its wait ended with, or why it did not end.
                let result = match (waited, &wait.state) {
                    (Ok(()), WaitState::Resolved { result, .. }) => result.clone(),
                    (waited, _) => children_finished_result(
                        waited
                            .err()
                            .unwrap_or_else(|| crate::handback::CHILDREN_FINISHED.into()),
                    ),
                };
                Ok(RunStart::Resume {
                    at: wait.resume,
                    result: Some(result),
                })
            }
        }
    }

    /// Where a Run a handover stopped goes on once it has the execution slot: its wait is over
    /// and its answer goes on in one change, then it takes up the call or the model request it
    /// stopped before.
    async fn handover_resume_start(
        &self,
        receipt: &Receipt,
        wait_id: Uuid,
        waited: Result<(), String>,
    ) -> Result<RunStart, String> {
        waited?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id).ok_or(HANDOVER_LOST)?;
        let at = chat
            .bookkeeping
            .lifecycle
            .wait(wait_id)
            .ok_or(HANDOVER_LOST)?
            .resume
            .clone();
        chat.bookkeeping.lifecycle.end_wait(wait_id);
        self.save_now(&chat.snapshot);
        Ok(RunStart::Resume { at, result: None })
    }

    /// Where the rebuilt Run goes on: at its waiting call, with the result the reply decided, or
    /// with the error its wait ended in, as the asking Run's own call reports a cancelled or
    /// expired question. An error when the Chat no longer holds the wait.
    async fn resume_start(
        &self,
        receipt: &Receipt,
        wait_id: Uuid,
        waited: Result<(), String>,
    ) -> Result<RunStart, String> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id).ok_or(QUESTION_EXPIRED)?;
        let wait = chat
            .bookkeeping
            .lifecycle
            .wait(wait_id)
            .ok_or(QUESTION_EXPIRED)?
            .clone();
        let result = match (waited, wait.state) {
            // As the asking Run's own: an answer the user gave reaches the chain even when the
            // Run is cancelled before it takes the slot again, and so does what a child
            // reported or the report call was told, whatever it says.
            (waited, WaitState::Resolved { result, .. })
                if waited.is_ok()
                    || !result.tool.is_error
                    || !matches!(wait.kind, WaitKind::Question { .. }) =>
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
        Ok(RunStart::Resume {
            at: wait.resume,
            result: Some(result),
        })
    }
}

/// What a member's report call reports when it waited for its children: an error, so the
/// member reads their reports first.
fn children_finished_result(error: String) -> ModelVisibleResult {
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

/// What a rebuilt Run waits on before it goes on where it waited.
enum RebuiltTail {
    /// The reply to an open question.
    Question(oneshot::Receiver<PendingReply>),
    /// The child run of an open foreground call.
    Subagent { child: Receipt, call_id: String },
    /// The children an open member's wait is on, at its report call `at_call` or else before
    /// its next model request.
    Children { at_call: Option<String> },
    /// Nothing: the wait already ended, and the Run waits for the execution slot.
    Ready,
    /// Nothing but the execution slot: a handover stopped the Run at a safe point.
    Handover,
}

/// A Run rebuilt around its wait, and what its wait tail waits on.
pub(crate) struct RebuiltRun {
    receipt: Receipt,
    wait_id: Uuid,
    tail: RebuiltTail,
    /// The Run holds the execution slot, as a foreground call or a member waiting for its
    /// children does; any other rebuilt Run is parked.
    holds_slot: bool,
    cancel: CancellationToken,
    release: CancellationToken,
    resume: Arc<Notify>,
}

/// The input of the call a wait is on, from its batch message.
fn request_input(snapshot: &ChatSnapshot, wait: &WaitEntry) -> Option<serde_json::Value> {
    let ResumePoint::InBatch {
        assistant_message,
        call_id,
    } = &wait.resume
    else {
        return None;
    };
    let (batch, start) =
        tool_batch::locate_batch_call(snapshot, wait.run_id.into(), *assistant_message, call_id)
            .ok()?;
    batch.calls.into_iter().nth(start).map(|call| call.input)
}
