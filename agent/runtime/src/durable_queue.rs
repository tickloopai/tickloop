//! The queue a Chat's input ledger carries from one host to the next. Every queued input is
//! accepted with its content, so with durable wait hosting a load queues again what was waiting,
//! after another attempt at each input whose answer lost its host.

use tickloop_agent_protocol::{ChatId, LoopState, Receipt};
use tickloop_agent_transcript::{
    InputEntry, InputOp, InputState, InterruptedBy, ResponseEnding, ResponseState, SettledOutcome,
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
                WaitHosting::Process => self.forget_internal_inputs(chat),
                WaitHosting::Durable => {
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

    fn forget_internal_inputs(&self, chat: &mut Chat) {
        for entry in chat.bookkeeping.lifecycle.open_entries() {
            if entry.state == InputState::Accepted && !entry.op.is_external() {
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
            if losses >= LOST_HOST_LIMIT {
                self.settle_input(
                    chat,
                    entry.run_id,
                    SettledOutcome::Failed {
                        error: LOST_HOST_ERROR.into(),
                    },
                );
                continue;
            }
            self.queue_continuation_of(chat, &entry, answer_parent);
        }
    }

    /// Queues another attempt at `entry` when the conversation still ends where its answer
    /// would go; otherwise the host's next delivery of it finds the conversation moved on.
    fn queue_continuation_of(
        &self,
        chat: &mut Chat,
        entry: &InputEntry,
        answer_parent: Option<uuid::Uuid>,
    ) {
        if turn::admit_continuation(chat, entry, answer_parent).is_err() {
            return;
        }
        let receipt = Receipt {
            chat_id: chat.snapshot.id,
            loop_id: entry.loop_id,
            run_id: entry.run_id,
        };
        let _ = self.queue_answer_run(chat, &receipt, answer_parent);
    }

    /// Queues again the inputs accepted and not started, in their order, behind the
    /// continuations.
    fn requeue_inputs(&self, chat: &mut Chat) {
        for mut pending in chat.bookkeeping.lifecycle.queued_inputs() {
            let is_queued = chat
                .snapshot
                .pending_inputs
                .iter()
                .any(|queued| queued.receipt.run_id == pending.receipt.run_id);
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
            chat.snapshot.last_queue_sequence += 1;
            pending.queued_sequence = chat.snapshot.last_queue_sequence;
            chat.snapshot.pending_inputs.push(pending);
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
