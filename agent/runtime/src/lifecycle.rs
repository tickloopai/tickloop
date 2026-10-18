//! A Chat's answer lifecycle records and input ledger.
//!
//! Both live in the Chat's bookkeeping and change in the same snapshot change as the work they
//! describe; the recorder writes every record whose revision it has not written yet, after the
//! chain entries of the same batch.

use std::collections::{BTreeMap, HashMap};

use sha2::{Digest, Sha256};
use tickloop_agent_base::private_fs::unix_time_milliseconds;
use tickloop_agent_base::timestamp::format_timestamp;
use tickloop_agent_protocol::{
    ChatId, ChatSnapshot, EventKind, InputContext, InputId, LoopId, PendingInput, Receipt,
    ResponseId, RunId, Usage,
};
use uuid::Uuid;

use crate::transcript::{
    InputEntry, InputOp, InputState, InterruptedBy, Line, ResponseEnding, ResponseEntry,
    ResponseState, SettledOutcome,
};
use crate::{Agent, AgentError, Chat};

/// A ledger input a submission carries: its id, operation and payload digest.
#[derive(Clone, Debug)]
pub(crate) struct LedgerInput {
    pub id: InputId,
    pub op: InputOp,
    pub digest: String,
}

impl LedgerInput {
    pub(crate) fn new(
        id: InputId,
        op: InputOp,
        goal: Option<LoopId>,
        text: &str,
        contexts: &[InputContext],
        paths: &[std::path::PathBuf],
    ) -> Self {
        let payload = serde_json::json!({
            "op": op,
            "loop": goal,
            "text": text,
            "contexts": contexts,
            "paths": paths,
        });
        let digest = Sha256::digest(payload.to_string().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self { id, op, digest }
    }
}

/// What the ledger knows of one input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputLedgerState {
    /// The input's latest ledger record.
    pub entry: InputEntry,
    /// Its answers, in the order they started.
    pub responses: Vec<ResponseEntry>,
    /// It is queued, running or parked in this instance.
    pub is_live: bool,
    /// Its prompt entered the conversation.
    pub prompt_in_conversation: bool,
}

#[derive(Clone, Debug)]
struct Revised<T> {
    record: T,
    revision: u64,
}

#[derive(Clone, Copy, Debug)]
struct ActiveResponse {
    response_id: ResponseId,
    /// The first message index not yet claimed for this response.
    next_message: usize,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Lifecycle {
    inputs: BTreeMap<InputId, Revised<InputEntry>>,
    responses: BTreeMap<ResponseId, Revised<ResponseEntry>>,
    /// Accepted inputs that have not settled, by the Run of their receipt.
    open_inputs: HashMap<RunId, InputId>,
    /// Started responses by Run.
    active: HashMap<RunId, ActiveResponse>,
    revision: u64,
}

fn now() -> String {
    format_timestamp(unix_time_milliseconds())
}

impl Lifecycle {
    fn next_revision(&mut self) -> u64 {
        self.revision += 1;
        self.revision
    }

    pub(crate) fn input(&self, id: InputId) -> Option<&InputEntry> {
        self.inputs.get(&id).map(|input| &input.record)
    }

    /// The original receipt of an input the ledger already holds under `input`'s id, or the
    /// refusal of another payload under it; `None` for a new input. A replay depends only on the
    /// ledger, not on the Loops, files or policy it was first checked against.
    pub(crate) fn replay(
        &self,
        chat_id: ChatId,
        input: &LedgerInput,
    ) -> Option<Result<Receipt, AgentError>> {
        let accepted = self.input(input.id)?;
        if accepted.digest != input.digest || accepted.op != input.op {
            return Some(Err(AgentError::Invalid(
                "input id reused with a different payload".into(),
            )));
        }
        Some(Ok(Receipt {
            chat_id,
            loop_id: accepted.loop_id,
            run_id: accepted.run_id,
        }))
    }

    pub(crate) fn is_input(&self, id: Uuid) -> bool {
        self.inputs.contains_key(&id)
    }

    pub(crate) fn accept(&mut self, input: &LedgerInput, receipt: &Receipt) {
        let revision = self.next_revision();
        self.open_inputs.insert(receipt.run_id, input.id);
        self.inputs.insert(
            input.id,
            Revised {
                record: InputEntry {
                    input_id: input.id,
                    op: input.op,
                    digest: input.digest.clone(),
                    display_parent: None,
                    loop_id: receipt.loop_id,
                    run_id: receipt.run_id,
                    timestamp: now(),
                    state: InputState::Accepted,
                },
                revision,
            },
        );
    }

    /// Settles the open input of `run`; `false` when it has none.
    pub(crate) fn settle(&mut self, run: RunId, outcome: SettledOutcome) -> bool {
        let Some(id) = self.open_inputs.remove(&run) else {
            return false;
        };
        let revision = self.next_revision();
        if let Some(input) = self.inputs.get_mut(&id) {
            input.record.state = InputState::Settled { outcome };
            input.record.timestamp = now();
            input.revision = revision;
        }
        true
    }

    /// The input of `run` the ledger still has open.
    pub(crate) fn open_input(&self, run: RunId) -> Option<InputId> {
        self.open_inputs.get(&run).copied()
    }

    pub(crate) fn responses_of(&self, input: InputId) -> Vec<ResponseEntry> {
        let mut responses = self
            .responses
            .values()
            .filter(|response| response.record.input_id == Some(input))
            .map(|response| response.record.clone())
            .collect::<Vec<_>>();
        responses.sort_by_key(|response| response.attempt);
        responses
    }

    /// Starts the answer of `receipt`'s Run; the messages its Run appends from now on belong to
    /// it.
    pub(crate) fn start(
        &mut self,
        snapshot: &ChatSnapshot,
        message_responses: &BTreeMap<usize, ResponseId>,
        receipt: &Receipt,
    ) -> ResponseEntry {
        let input_id = self.open_inputs.get(&receipt.run_id).copied();
        let attempt = self
            .responses
            .values()
            .filter(|response| match input_id {
                Some(input) => response.record.input_id == Some(input),
                None => response.record.run_id == receipt.run_id,
            })
            .count() as u32
            + 1;
        let record = ResponseEntry {
            response_id: Uuid::new_v4(),
            input_id,
            attempt,
            run_id: receipt.run_id,
            loop_id: receipt.loop_id,
            display_parent: self.latest_display_node(snapshot, message_responses),
            timestamp: now(),
            state: ResponseState::Started,
        };
        let revision = self.next_revision();
        self.active.insert(
            receipt.run_id,
            ActiveResponse {
                response_id: record.response_id,
                next_message: snapshot.messages.len(),
            },
        );
        self.responses.insert(
            record.response_id,
            Revised {
                record: record.clone(),
                revision,
            },
        );
        record
    }

    pub(crate) fn active_response(&self, run: RunId) -> Option<ResponseId> {
        self.active.get(&run).map(|active| active.response_id)
    }

    /// Ties the messages each started answer's Run appended since the last claim to it.
    pub(crate) fn claim(
        &mut self,
        snapshot: &ChatSnapshot,
        message_responses: &mut BTreeMap<usize, ResponseId>,
    ) {
        for (run, active) in &mut self.active {
            for index in active.next_message..snapshot.messages.len() {
                if snapshot.message_runs.get(&index) == Some(run) {
                    message_responses.entry(index).or_insert(active.response_id);
                }
            }
            active.next_message = active.next_message.max(snapshot.messages.len());
        }
    }

    /// Ends the started answer of `run`. `None` when it has none.
    pub(crate) fn end(
        &mut self,
        snapshot: &ChatSnapshot,
        message_responses: &mut BTreeMap<usize, ResponseId>,
        run: RunId,
        ending: ResponseEnding,
    ) -> Option<ResponseEntry> {
        self.claim(snapshot, message_responses);
        let active = self.active.remove(&run)?;
        let revision = self.next_revision();
        let response = self.responses.get_mut(&active.response_id)?;
        response.record.state = ResponseState::Ended { ending };
        response.record.timestamp = now();
        response.revision = revision;
        Some(response.record.clone())
    }

    /// Message indexes moved: every message before `messages` is already claimed.
    pub(crate) fn restart_claims_at(&mut self, messages: usize) {
        for active in self.active.values_mut() {
            active.next_message = messages;
        }
    }

    /// The latest revision; every record changed since a recorder wrote up to it has a higher one.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Records changed after revision `written`, in revision order.
    pub(crate) fn changed_after(&self, written: u64) -> Vec<Line> {
        let mut lines = self
            .inputs
            .values()
            .filter(|input| input.revision > written)
            .map(|input| (input.revision, Line::Input(input.record.clone())))
            .chain(
                self.responses
                    .values()
                    .filter(|response| response.revision > written)
                    .map(|response| (response.revision, Line::Response(response.record.clone()))),
            )
            .collect::<Vec<_>>();
        lines.sort_by_key(|(revision, _)| *revision);
        lines.into_iter().map(|(_, line)| line).collect()
    }

    /// The answer a message is part of: the one it was claimed for, or else the started answer of
    /// its Run when it came after that answer's last claim.
    pub(crate) fn response_of(
        &self,
        snapshot: &ChatSnapshot,
        message_responses: &BTreeMap<usize, ResponseId>,
        index: usize,
    ) -> Option<ResponseId> {
        message_responses.get(&index).copied().or_else(|| {
            let active = self.active.get(snapshot.message_runs.get(&index)?)?;
            (active.next_message <= index).then_some(active.response_id)
        })
    }

    /// The display node the conversation ends at: the latest prompt that is not part of an
    /// answer, or the answer of a later message. A Run without a prompt of its own answers after
    /// it.
    fn latest_display_node(
        &self,
        snapshot: &ChatSnapshot,
        message_responses: &BTreeMap<usize, ResponseId>,
    ) -> Option<Uuid> {
        (0..snapshot.messages.len()).rev().find_map(|index| {
            if let Some(response) = self.response_of(snapshot, message_responses, index) {
                return Some(response);
            }
            snapshot
                .prompt_execution_modes
                .contains_key(&index)
                .then(|| {
                    snapshot
                        .original_message_ids
                        .get(&index)
                        .or_else(|| snapshot.message_ids.get(&index))
                        .copied()
                })
                .flatten()
        })
    }

    /// The ledger and answers stored records describe, the latest record of each id winning.
    /// Inputs restore as they were: an input still open stays open for a continuation. A copy
    /// under a new Chat id keeps the answers but not the source Chat's ledger.
    pub(crate) fn restore<'a>(
        lines: impl IntoIterator<Item = &'a Line>,
        keeps_inputs: bool,
    ) -> Self {
        let mut lifecycle = Self::default();
        for line in lines {
            let revision = lifecycle.next_revision();
            match line {
                Line::Input(entry) if keeps_inputs => {
                    if entry.state == InputState::Accepted {
                        lifecycle.open_inputs.insert(entry.run_id, entry.input_id);
                    } else {
                        lifecycle.open_inputs.remove(&entry.run_id);
                    }
                    lifecycle.inputs.insert(
                        entry.input_id,
                        Revised {
                            record: entry.clone(),
                            revision,
                        },
                    );
                }
                Line::Response(entry) => {
                    lifecycle.responses.insert(
                        entry.response_id,
                        Revised {
                            record: entry.clone(),
                            revision,
                        },
                    );
                }
                _ => {}
            }
        }
        lifecycle
    }

    /// After a load: an answer that started and never ended lost its host. Gives the ids it
    /// ended.
    pub(crate) fn end_orphaned_responses(&mut self) -> Vec<ResponseId> {
        let orphaned = self
            .responses
            .values()
            .filter(|response| response.record.state == ResponseState::Started)
            .map(|response| response.record.response_id)
            .collect::<Vec<_>>();
        for id in &orphaned {
            let revision = self.next_revision();
            let response = self.responses.get_mut(id).unwrap();
            response.record.state = ResponseState::Ended {
                ending: ResponseEnding::Interrupted {
                    by: InterruptedBy::Host,
                },
            };
            response.record.timestamp = now();
            response.revision = revision;
        }
        orphaned
    }

    pub(crate) fn state(
        &self,
        id: InputId,
        snapshot: &ChatSnapshot,
        is_live: impl Fn(RunId) -> bool,
    ) -> Option<InputLedgerState> {
        let entry = self.input(id)?.clone();
        let prompt_in_conversation = snapshot
            .message_ids
            .values()
            .chain(snapshot.original_message_ids.values())
            .any(|message| *message == id);
        Some(InputLedgerState {
            is_live: is_live(entry.run_id),
            responses: self.responses_of(id),
            prompt_in_conversation,
            entry,
        })
    }
}

pub(crate) fn claim_response_messages(chat: &mut Chat) {
    let bookkeeping = &mut chat.bookkeeping;
    bookkeeping
        .lifecycle
        .claim(&chat.snapshot, &mut bookkeeping.message_responses);
}

impl Agent {
    /// Starts the answer of a Run that just began, with its `ResponseStarted` event.
    pub(crate) fn start_response(&self, chat: &mut Chat, receipt: &Receipt) {
        let bookkeeping = &mut chat.bookkeeping;
        let response =
            bookkeeping
                .lifecycle
                .start(&chat.snapshot, &bookkeeping.message_responses, receipt);
        self.emit(
            chat,
            receipt,
            EventKind::ResponseStarted {
                response_id: response.response_id,
                attempt: response.attempt,
                display_parent: response.display_parent,
            },
        );
        self.save_now(&chat.snapshot);
    }

    /// Ends the Run's answer and settles the input it answered. An answer the host lost leaves
    /// its input open for a continuation.
    pub(crate) fn end_response(
        &self,
        chat: &mut Chat,
        run: RunId,
        ending: ResponseEnding,
        usage: Usage,
    ) {
        let bookkeeping = &mut chat.bookkeeping;
        bookkeeping.lifecycle.end(
            &chat.snapshot,
            &mut bookkeeping.message_responses,
            run,
            ending.clone(),
        );
        let outcome = match ending {
            ResponseEnding::Interrupted {
                by: InterruptedBy::Host,
            } => return,
            ResponseEnding::Interrupted { by } => SettledOutcome::Interrupted { by },
            ResponseEnding::Failed { error } => SettledOutcome::Failed { error },
            ResponseEnding::Completed => SettledOutcome::Completed { usage },
        };
        self.settle_input(chat, run, outcome);
    }

    pub(crate) fn settle_input(&self, chat: &mut Chat, run: RunId, outcome: SettledOutcome) {
        if chat.bookkeeping.lifecycle.settle(run, outcome) {
            self.inner.ledger_changes.send_replace(());
        }
    }

    /// Settles the ledger inputs of queued inputs removed before they started.
    pub(crate) fn settle_removed_inputs<'a>(
        &self,
        chat: &mut Chat,
        removed: impl IntoIterator<Item = &'a PendingInput>,
        outcome: SettledOutcome,
    ) {
        for pending in removed {
            self.settle_input(chat, pending.receipt.run_id, outcome.clone());
        }
    }

    /// What the ledger of `chat` knows of `input`; `None` when it has no record of it.
    pub async fn input_state(
        &self,
        chat: ChatId,
        input: InputId,
    ) -> Result<Option<InputLedgerState>, AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat).ok_or(AgentError::NotFound)?;
        Ok(chat
            .bookkeeping
            .lifecycle
            .state(input, &chat.snapshot, |run| {
                chat.active_run == Some(run)
                    || chat.parked.contains_key(&run)
                    || chat.bookkeeping.lifecycle.active_response(run).is_some()
                    || chat
                        .snapshot
                        .pending_inputs
                        .iter()
                        .any(|pending| pending.receipt.run_id == run)
            }))
    }

    /// Waits until the ledger settles `input`. Fails when the ledger has no record of it or the
    /// host shuts down first; an input whose answer the host lost stays open, so a shutdown
    /// ends the wait.
    pub async fn settled_input(
        &self,
        chat: ChatId,
        input: InputId,
    ) -> Result<SettledOutcome, AgentError> {
        let mut changes = self.inner.ledger_changes.subscribe();
        loop {
            changes.mark_unchanged();
            let state = self
                .input_state(chat, input)
                .await?
                .ok_or(AgentError::NotFound)?;
            if let InputState::Settled { outcome } = state.entry.state {
                return Ok(outcome);
            }
            if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(AgentError::Invalid("agent host has shut down".into()));
            }
            if changes.changed().await.is_err() {
                return Err(AgentError::Invalid("agent host has shut down".into()));
            }
        }
    }
}
