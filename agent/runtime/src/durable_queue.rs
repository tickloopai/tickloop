//! The queue a Chat's input ledger carries from one host to the next. Every queued input is
//! accepted with its content, so with durable wait hosting a load queues again what was waiting,
//! after another attempt at each input whose answer lost its host.

use tickloop_agent_protocol::{ChatId, EventKind, LoopSnapshot, LoopState, Receipt};
use tickloop_agent_transcript::{
    InputOp, InterruptedBy, ResponseEnding, ResponseState, SettledOutcome,
};

use super::{Agent, Chat, LoopSnapshotExt, WaitHosting, turn};

/// An input whose answer lost its host this many times in a row fails instead of continuing.
const LOST_HOST_LIMIT: usize = 3;
const LOST_HOST_ERROR: &str = "lost its host 3 times";

impl Agent {
    /// After a load. With durable wait hosting: another attempt continues each input whose
    /// answer lost its host, unless it lost its host three times in a row; the inputs that were
    /// queued are queued again after them; suspended Runs are rebuilt. With process wait
    /// hosting, the inputs the Agent queued for itself are lost with the process, as they always
    /// were.
    pub(crate) async fn restore_after_load(&self, chat_id: ChatId, waits: WaitHosting) {
        {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return;
            };
            match waits {
                WaitHosting::Process => self.forget_inputs(chat, |_| true),
                WaitHosting::Durable => {
                    // A queued @tickloop request is delivered again by its source (H09g).
                    self.forget_inputs(chat, |op| op == InputOp::Feedback);
                    self.continue_lost_inputs(chat);
                    self.requeue_inputs(chat);
                }
            }
            self.save(&chat.snapshot);
        }
        if waits == WaitHosting::Durable {
            self.rebuild_suspended_runs(chat_id).await;
        }
    }

    /// The open inputs of `ops` that the user did not send are lost with their host: nothing
    /// sends them again.
    fn forget_inputs(&self, chat: &mut Chat, ops: impl Fn(InputOp) -> bool) {
        for entry in chat.bookkeeping.lifecycle.open_entries() {
            if !entry.op.is_from_user() && ops(entry.op) {
                self.settle_input(
                    chat,
                    entry.run_id,
                    SettledOutcome::Interrupted {
                        by: InterruptedBy::Host,
                    },
                );
            }
        }
    }

    fn continue_lost_inputs(&self, chat: &mut Chat) {
        for entry in chat.bookkeeping.lifecycle.open_entries() {
            let attempts = chat.bookkeeping.lifecycle.responses_of(entry.input_id);
            let answer_parent = match attempts.last() {
                Some(last) if lost_its_host(&last.state) => last.display_parent,
                Some(_) => continue,
                // A prompt in the conversation whose Run never started an answer.
                None if entry.op != InputOp::RegenerateAnswer
                    && chat
                        .bookkeeping
                        .lifecycle
                        .state(entry.input_id, &chat.snapshot, |_| false)
                        .is_some_and(|state| state.prompt_in_conversation) =>
                {
                    Some(entry.input_id)
                }
                None => continue,
            };
            let losses = attempts
                .iter()
                .rev()
                .filter(|attempt| !is_handover(&attempt.state))
                .take_while(|attempt| lost_its_host(&attempt.state))
                .count();
            let receipt = Receipt {
                chat_id: chat.snapshot.id,
                loop_id: entry.loop_id,
                run_id: entry.run_id,
            };
            if losses >= LOST_HOST_LIMIT {
                self.fail_lost_input(chat, &receipt);
                continue;
            }
            // Another attempt goes on only where the conversation still ends at the input's
            // answer; otherwise a user's input finds the conversation moved on when the host
            // delivers it again, and one the Agent queued for itself is lost with its host.
            let continued = turn::admit_continuation(chat, &entry, answer_parent)
                .and_then(|()| self.queue_answer_run(chat, &receipt, answer_parent));
            if continued.is_err() && !entry.op.is_from_user() {
                self.settle_input(
                    chat,
                    entry.run_id,
                    SettledOutcome::Interrupted {
                        by: InterruptedBy::Host,
                    },
                );
            }
        }
    }

    /// The input lost its host too many times in a row: it fails, and so does its Loop.
    fn fail_lost_input(&self, chat: &mut Chat, receipt: &Receipt) {
        self.settle_input(
            chat,
            receipt.run_id,
            SettledOutcome::Failed {
                error: LOST_HOST_ERROR.into(),
            },
        );
        if let Some(goal) = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == receipt.loop_id)
            .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
        {
            goal.set_state(LoopState::Failed);
        }
        self.emit(chat, receipt, EventKind::Failed(LOST_HOST_ERROR.into()));
    }

    /// Queues again the inputs accepted and not started, behind the continuations, in the order
    /// they were queued: each goes where it went when it was queued.
    fn requeue_inputs(&self, chat: &mut Chat) {
        let feedback = chat
            .bookkeeping
            .lifecycle
            .open_entries()
            .into_iter()
            .filter(|entry| entry.op == InputOp::Feedback)
            .map(|entry| entry.run_id)
            .collect::<std::collections::HashSet<_>>();
        for pending in chat.bookkeeping.lifecycle.queued_inputs() {
            if feedback.contains(&pending.receipt.run_id) {
                continue;
            }
            let is_queued = chat
                .snapshot
                .pending_inputs
                .iter()
                .any(|queued| queued.receipt.run_id == pending.receipt.run_id);
            // A Loop is recorded once its first Run starts: an input that was to start one
            // brings it back.
            if !chat
                .snapshot
                .loops
                .iter()
                .any(|goal| goal.id == pending.receipt.loop_id)
            {
                chat.bookkeeping.add_loop(
                    &mut chat.snapshot,
                    LoopSnapshot {
                        id: pending.receipt.loop_id,
                        goal: pending.input.clone(),
                        state: LoopState::Idle,
                        run_id: None,
                        run_started_at: None,
                        trigger: None,
                        event_sources: Vec::new(),
                        state_since: None,
                    },
                );
            }
            let Some(goal) = chat
                .snapshot
                .loops
                .iter_mut()
                .find(|goal| goal.id == pending.receipt.loop_id)
            else {
                continue;
            };
            if is_queued || matches!(goal.state, LoopState::Stopped | LoopState::Expired) {
                continue;
            }
            if !matches!(
                goal.state,
                LoopState::Running | LoopState::WaitingQuestion | LoopState::WaitingPermission
            ) {
                goal.set_state(LoopState::Queued);
            }
            chat.snapshot.last_queue_sequence = chat
                .snapshot
                .last_queue_sequence
                .max(pending.queued_sequence);
            // As submissions place them: an input that is not a Trigger's goes before the first
            // Trigger's, and a user's also before the first Agent message.
            let index = match pending.trigger_source {
                Some(_) => None,
                None => chat.snapshot.pending_inputs.iter().position(|queued| {
                    queued.trigger_source.is_some()
                        || (pending.agent_message.is_none() && queued.agent_message.is_some())
                }),
            }
            .unwrap_or(chat.snapshot.pending_inputs.len())
            // Continuations stay first.
            .max(
                chat.snapshot
                    .pending_inputs
                    .iter()
                    .take_while(|queued| queued.task_continuation)
                    .count(),
            );
            chat.snapshot.pending_inputs.insert(index, pending);
        }
    }
}

fn lost_its_host(state: &ResponseState) -> bool {
    matches!(
        state,
        ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::Host | InterruptedBy::Handover,
            },
        }
    )
}

fn is_handover(state: &ResponseState) -> bool {
    matches!(
        state,
        ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::Handover,
            },
        }
    )
}
