//! The queue a Chat's input ledger carries from one host to the next. Every queued input is
//! accepted with its content, so with durable wait hosting a load queues again what was waiting,
//! after another attempt at each input whose answer lost its host.

use tickloop_agent_protocol::{ChatId, EventKind, LoopState, PendingInput, Receipt, TriggerSource};
use tickloop_agent_transcript::{
    InputEntry, InputOp, InterruptedBy, ResponseEnding, ResponseState, SettledOutcome,
};

use super::{Agent, Chat, LoopSnapshotExt, WaitHosting, event_sources, turn};

pub(super) fn is_trigger_input(pending: &PendingInput) -> bool {
    pending.trigger_source.is_some()
}

/// An input whose answer lost its host this many times in a row fails instead of continuing.
const LOST_HOST_LIMIT: usize = 3;
const LOST_HOST_ERROR: &str = "lost its host 3 times";

impl Agent {
    /// After a load. With durable wait hosting: another attempt continues each input whose
    /// answer lost its host, unless it lost its host three times in a row; the inputs that were
    /// queued are queued again after them; suspended Runs are rebuilt. With process wait
    /// hosting, Trigger input that had not started is queued again, to wait for the Chat to be
    /// continued, since its source already moved past it; the rest the Agent queued for itself
    /// is lost with the process, as it always was.
    pub(crate) async fn restore_after_load(&self, chat_id: ChatId, waits: WaitHosting) {
        match waits {
            WaitHosting::Process => {
                let mut chats = self.inner.chats.lock().await;
                let Some(chat) = chats.get_mut(&chat_id) else {
                    return;
                };
                chat.bookkeeping.lifecycle.reopen_started_ahead();
                let waiting = chat
                    .bookkeeping
                    .lifecycle
                    .queued_inputs()
                    .into_iter()
                    .filter(is_trigger_input)
                    .map(|pending| pending.receipt.run_id)
                    .collect::<std::collections::HashSet<_>>();
                self.forget_inputs(chat, |entry| !waiting.contains(&entry.run_id));
                self.requeue_inputs(chat, is_trigger_input);
                self.save(&chat.snapshot);
            }
            WaitHosting::Durable => self.restore_family_after_load(&[chat_id], Vec::new()).await,
        }
    }

    /// With durable wait hosting, the work a loaded Chat had comes back, none of it running yet:
    /// another attempt continues each input whose answer lost its host, unless it lost its host
    /// three times in a row, and the inputs that were queued are queued again after them.
    pub(crate) async fn queue_after_load(&self, chat_id: ChatId) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        chat.bookkeeping.lifecycle.reopen_started_ahead();
        self.continue_lost_inputs(chat);
        self.requeue_inputs(chat, |_| true);
        self.save(&chat.snapshot);
    }

    /// The open inputs among `lost` that the user did not send are lost with their host: nothing
    /// sends them again.
    fn forget_inputs(&self, chat: &mut Chat, lost: impl Fn(&InputEntry) -> bool) {
        for entry in chat.bookkeeping.lifecycle.open_entries() {
            if !entry.op.is_from_user() && lost(&entry) {
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
        chat.snapshot.chat_loop.set_state(LoopState::Failed);
        self.emit(chat, receipt, EventKind::Failed(LOST_HOST_ERROR.into()));
    }

    /// Queues again the inputs accepted and not started that `requeued` selects, behind the
    /// continuations, in the order they were queued, each to the Chat's Loop.
    pub(super) fn requeue_inputs(&self, chat: &mut Chat, requeued: impl Fn(&PendingInput) -> bool) {
        for mut pending in chat
            .bookkeeping
            .lifecycle
            .queued_inputs()
            .into_iter()
            .filter(|pending| requeued(pending))
        {
            let is_queued = chat
                .snapshot
                .pending_inputs
                .iter()
                .any(|queued| queued.receipt.run_id == pending.receipt.run_id);
            if is_queued {
                continue;
            }
            pending.receipt.loop_id = chat.snapshot.chat_loop.id;
            if let Some(TriggerSource::TickUpdate { items }) = &mut pending.trigger_source {
                self.tick_deliveries().rebind_restored_items(
                    chat.snapshot.id,
                    &chat.snapshot.chat_loop,
                    items,
                );
            }
            let chat_loop = &mut chat.snapshot.chat_loop;
            if !matches!(
                chat_loop.state,
                LoopState::Running | LoopState::WaitingQuestion | LoopState::WaitingPermission
            ) {
                chat_loop.set_state(LoopState::Queued);
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
            if let Some(identity) = pending.trigger_source.as_ref().and_then(|source| {
                event_sources::feedback_identity(chat.snapshot.id, pending.receipt.loop_id, source)
            }) {
                chat.feedback_inputs
                    .insert(pending.receipt.run_id, identity);
            }
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
