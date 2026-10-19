//! A Chat's answer lifecycle records, input ledger and durable waits.
//!
//! They live in the Chat's bookkeeping and change in the same snapshot change as the work they
//! describe; the recorder writes every record whose revision it has not written yet, after the
//! chain entries of the same batch.

use std::collections::{BTreeMap, HashMap, HashSet};

use sha2::{Digest, Sha256};
use tickloop_agent_base::private_fs::unix_time_milliseconds;
use tickloop_agent_base::timestamp::format_timestamp;
use tickloop_agent_protocol::{
    ChatId, ChatSnapshot, EventKind, InputId, PendingInput, Receipt, ResponseId, RunId, Usage,
};
use uuid::Uuid;

use crate::{Agent, AgentError, Chat};
use tickloop_agent_transcript::{
    AbandonReason, InputEntry, InputOp, InputState, InterruptedBy, Line, ModelVisibleResult,
    ResponseEnding, ResponseEntry, ResponseState, ResumePoint, SettledOutcome, WaitEntry, WaitKind,
    WaitState,
};

/// A ledger input a submission carries: its id, operation and payload digest.
#[derive(Clone, Debug)]
pub(crate) struct LedgerInput {
    pub id: InputId,
    pub op: InputOp,
    pub digest: String,
    /// The display node the input was accepted under.
    pub display_parent: Option<Uuid>,
    /// The Run of an earlier acceptance of this input that was lost before it started: this
    /// acceptance replaces it.
    pub replaces: Option<RunId>,
}

impl LedgerInput {
    pub(crate) fn new(id: InputId, op: InputOp, payload: &impl serde::Serialize) -> Self {
        let payload = serde_json::to_string(payload).expect("an input payload is serializable");
        let digest = Sha256::digest(payload.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            id,
            op,
            digest,
            display_parent: None,
            replaces: None,
        }
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
    waits: BTreeMap<Uuid, Revised<WaitEntry>>,
    /// Accepted inputs that have not settled, by the Run of their receipt.
    open_inputs: HashMap<RunId, InputId>,
    /// Started responses by Run.
    active: HashMap<RunId, ActiveResponse>,
    /// The display node a queued Run's answer hangs under instead of where the conversation ends:
    /// another attempt at an answer is its sibling.
    answer_parents: HashMap<RunId, Uuid>,
    /// The answer started last on the current chain; it is the display leaf while no message of
    /// its own stands for it.
    tail_answer: Option<ResponseId>,
    /// The display leaf when the conversation was last compacted, for a chain whose remaining
    /// messages show no display node.
    compacted_leaf: Option<Uuid>,
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
        if accepted.state == InputState::Accepted && input.replaces == Some(accepted.run_id) {
            return None;
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

    /// Accepts `input` as the queued input `queued`, whose content the record keeps until it
    /// starts, so a load can queue it again.
    pub(crate) fn accept(&mut self, input: &LedgerInput, queued: &PendingInput) {
        let receipt = &queued.receipt;
        let revision = self.next_revision();
        self.open_inputs.retain(|_, open| *open != input.id);
        self.open_inputs.insert(receipt.run_id, input.id);
        self.inputs.insert(
            input.id,
            Revised {
                record: InputEntry {
                    input_id: input.id,
                    op: input.op,
                    digest: input.digest.clone(),
                    display_parent: input.display_parent,
                    loop_id: receipt.loop_id,
                    run_id: receipt.run_id,
                    timestamp: now(),
                    state: InputState::Accepted,
                    queued: Some(
                        serde_json::to_value(queued).expect("a queued input is serializable"),
                    ),
                },
                revision,
            },
        );
    }

    /// The Run of `run`'s open input started. An input whose prompt does not carry its id
    /// records it; for the others their prompt in the conversation shows it.
    pub(crate) fn start_input(&mut self, run: RunId) {
        let Some(id) = self.open_inputs.get(&run).copied() else {
            return;
        };
        let revision = self.next_revision();
        if let Some(input) = self.inputs.get_mut(&id) {
            input.record.queued = None;
            if !matches!(
                input.record.op,
                InputOp::Continue
                    | InputOp::Skill
                    | InputOp::ReplacePrompt
                    | InputOp::RegenerateAnswer
            ) {
                input.record.state = InputState::Started;
            }
            input.record.timestamp = now();
            input.revision = revision;
        }
    }

    /// The inputs accepted and not yet started, in the order they were queued: what a load
    /// queues again.
    pub(crate) fn queued_inputs(&self) -> Vec<PendingInput> {
        let mut queued = self
            .inputs
            .values()
            .filter(|input| input.record.state == InputState::Accepted)
            .filter_map(|input| {
                serde_json::from_value::<PendingInput>(input.record.queued.clone()?).ok()
            })
            // Another attempt at an answer is the continuation of its input, not a queued input.
            .filter(|pending| !pending.task_continuation)
            .collect::<Vec<_>>();
        queued.sort_by_key(|pending| pending.queued_sequence);
        queued
    }

    /// Settles the open input of `run`; `false` when it has none.
    pub(crate) fn settle(&mut self, run: RunId, outcome: SettledOutcome) -> bool {
        let Some(id) = self.open_inputs.remove(&run) else {
            return false;
        };
        let revision = self.next_revision();
        if let Some(input) = self.inputs.get_mut(&id) {
            input.record.state = InputState::Settled { outcome };
            input.record.queued = None;
            input.record.timestamp = now();
            input.revision = revision;
        }
        true
    }

    /// Records the ledger input `input` settled as `outcome` from the start, without a Run of its
    /// own: a reply that ended a wait merged into the waiting Run, or a delivery its ended Loop
    /// discarded. `receipt`'s Run is only its identity.
    pub(crate) fn record_settled(
        &mut self,
        input: &LedgerInput,
        receipt: &Receipt,
        outcome: SettledOutcome,
    ) {
        let revision = self.next_revision();
        // An earlier acceptance of the input that was lost before it started is settled too.
        self.open_inputs.retain(|_, open| *open != input.id);
        self.inputs.insert(
            input.id,
            Revised {
                record: InputEntry {
                    input_id: input.id,
                    op: input.op,
                    digest: input.digest.clone(),
                    display_parent: input.display_parent,
                    loop_id: receipt.loop_id,
                    run_id: receipt.run_id,
                    timestamp: now(),
                    state: InputState::Settled { outcome },
                    queued: None,
                },
                revision,
            },
        );
    }

    /// The ledger holds an input of the Loop `loop_id`.
    pub(crate) fn knows_loop(&self, loop_id: Uuid) -> bool {
        self.inputs
            .values()
            .any(|input| input.record.loop_id == loop_id)
    }

    /// Every input the ledger has open: accepted or started, and not settled.
    pub(crate) fn open_entries(&self) -> Vec<InputEntry> {
        let mut open = self
            .inputs
            .values()
            .filter(|input| !matches!(input.record.state, InputState::Settled { .. }))
            .collect::<Vec<_>>();
        open.sort_by_key(|input| input.revision);
        open.into_iter().map(|input| input.record.clone()).collect()
    }

    /// The input of `run` the ledger still has open.
    pub(crate) fn open_input(&self, run: RunId) -> Option<InputId> {
        self.open_inputs.get(&run).copied()
    }

    pub(crate) fn response(&self, id: ResponseId) -> Option<&ResponseEntry> {
        self.responses.get(&id).map(|response| &response.record)
    }

    /// Some answer, on any branch, hangs under the display node `node`.
    pub(crate) fn has_answer_under(&self, node: Uuid) -> bool {
        self.responses
            .values()
            .any(|response| response.record.display_parent == Some(node))
    }

    /// The answer `run` starts next hangs under `parent`.
    pub(crate) fn answer_under(&mut self, run: RunId, parent: Uuid) {
        self.answer_parents.insert(run, parent);
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
            display_parent: self
                .answer_parents
                .remove(&receipt.run_id)
                .or_else(|| self.display_leaf(snapshot, message_responses)),
            timestamp: now(),
            state: ResponseState::Started,
            without_messages: false,
        };
        self.tail_answer = Some(record.response_id);
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
        let without_messages = !message_responses
            .values()
            .any(|response| *response == active.response_id);
        let abandoned = match &ending {
            ResponseEnding::Interrupted {
                by: InterruptedBy::Handover,
            } => None,
            ResponseEnding::Interrupted {
                by: InterruptedBy::Host,
            } => Some(AbandonReason::HostLost),
            _ => Some(AbandonReason::Interrupted),
        };
        let response = self.responses.get_mut(&active.response_id)?;
        response.record.state = ResponseState::Ended { ending };
        response.record.without_messages = without_messages;
        response.record.timestamp = now();
        response.revision = revision;
        let ended = response.record.clone();
        if let Some(reason) = abandoned {
            self.abandon_waits(active.response_id, reason);
        }
        Some(ended)
    }

    /// Starts a durable wait of `receipt`'s Run: its answer is suspended until the wait ends.
    /// `None` when the Run has no started answer.
    pub(crate) fn suspend(
        &mut self,
        receipt: &Receipt,
        kind: WaitKind,
        resume: ResumePoint,
    ) -> Option<Uuid> {
        let response_id = self.active.get(&receipt.run_id)?.response_id;
        let wait_id = match &kind {
            WaitKind::Question { request, .. } => request.id,
            WaitKind::Subagent { .. } | WaitKind::Children { .. } => Uuid::new_v4(),
        };
        let revision = self.next_revision();
        let response = self.responses.get_mut(&response_id)?;
        response.record.state = ResponseState::Suspended;
        response.record.timestamp = now();
        response.revision = revision;
        let revision = self.next_revision();
        self.waits.insert(
            wait_id,
            Revised {
                record: WaitEntry {
                    wait_id,
                    chat_id: receipt.chat_id,
                    loop_id: receipt.loop_id,
                    run_id: receipt.run_id,
                    response_id,
                    kind,
                    resume,
                    timestamp: now(),
                    state: WaitState::Open,
                },
                revision,
            },
        );
        Some(wait_id)
    }

    pub(crate) fn wait(&self, id: Uuid) -> Option<&WaitEntry> {
        self.waits.get(&id).map(|wait| &wait.record)
    }

    /// The open wait of `run` that `matches` picks, if the Run waits on one.
    pub(crate) fn open_wait_of(
        &self,
        run: RunId,
        matches: impl Fn(&WaitKind) -> bool,
    ) -> Option<&WaitEntry> {
        self.waits.values().map(|wait| &wait.record).find(|wait| {
            wait.run_id == run && matches!(wait.state, WaitState::Open) && matches(&wait.kind)
        })
    }

    /// The open wait of a foreground `Subagent` call on the child run `child_run`.
    pub(crate) fn open_subagent_wait(&self, child_run: RunId) -> Option<&WaitEntry> {
        self.waits.values().map(|wait| &wait.record).find(|wait| {
            matches!(wait.state, WaitState::Open)
                && matches!(wait.kind, WaitKind::Subagent { child_run: waited, .. } if waited == child_run)
        })
    }

    /// The wait `id`, which no call's result ends, is over: its answer goes on, in the same
    /// change. `false` when the wait is not open.
    pub(crate) fn end_wait(&mut self, id: Uuid) -> bool {
        let Some(response_id) = self
            .waits
            .get(&id)
            .filter(|wait| matches!(wait.record.state, WaitState::Open))
            .map(|wait| wait.record.response_id)
        else {
            return false;
        };
        let revision = self.next_revision();
        let wait = self.waits.get_mut(&id).unwrap();
        wait.record.state = WaitState::Appended;
        wait.record.timestamp = now();
        wait.revision = revision;
        self.mark_resumed(response_id);
        true
    }

    fn mark_resumed(&mut self, response_id: ResponseId) {
        let revision = self.next_revision();
        if let Some(response) = self
            .responses
            .get_mut(&response_id)
            .filter(|response| response.record.state == ResponseState::Suspended)
        {
            response.record.state = ResponseState::Resumed;
            response.record.timestamp = now();
            response.revision = revision;
        }
    }

    /// Ends the open wait `id` with `result`, decided now; the answer stays suspended until the
    /// result is on the chain. `false` when the wait is not open.
    pub(crate) fn resolve_wait(
        &mut self,
        id: Uuid,
        result: ModelVisibleResult,
        by_input: Option<Uuid>,
    ) -> bool {
        let revision = self.next_revision();
        let Some(wait) = self
            .waits
            .get_mut(&id)
            .filter(|wait| matches!(wait.record.state, WaitState::Open))
        else {
            return false;
        };
        wait.record.state = WaitState::Resolved { result, by_input };
        wait.record.timestamp = now();
        wait.revision = revision;
        true
    }

    /// The result of the call `call_id` of `run` is on the chain: a wait on that call is done and
    /// its answer goes on, in the same change. A wait no reply ended was interrupted: its call
    /// reports the error it ended in. `false` when no wait of the Run is on that call.
    pub(crate) fn resume_after(&mut self, run: RunId, call_id: &str) -> bool {
        let Some(wait_id) = self
            .waits
            .values()
            .find(|wait| {
                wait.record.run_id == run
                    && matches!(
                        wait.record.state,
                        WaitState::Open | WaitState::Resolved { .. }
                    )
                    && wait.record.resume.call_id() == Some(call_id)
            })
            .map(|wait| wait.record.wait_id)
        else {
            return false;
        };
        let revision = self.next_revision();
        let wait = self.waits.get_mut(&wait_id).unwrap();
        wait.record.state = match wait.record.state {
            WaitState::Open => WaitState::Abandoned {
                reason: AbandonReason::Interrupted,
            },
            _ => WaitState::Appended,
        };
        wait.record.timestamp = now();
        wait.revision = revision;
        let response_id = wait.record.response_id;
        self.mark_resumed(response_id);
        true
    }

    fn abandon_waits(&mut self, response_id: ResponseId, reason: AbandonReason) {
        let open = self
            .waits
            .values()
            .filter(|wait| {
                wait.record.response_id == response_id
                    && matches!(
                        wait.record.state,
                        WaitState::Open | WaitState::Resolved { .. }
                    )
            })
            .map(|wait| wait.record.wait_id)
            .collect::<Vec<_>>();
        for id in open {
            let revision = self.next_revision();
            let wait = self.waits.get_mut(&id).unwrap();
            wait.record.state = WaitState::Abandoned { reason };
            wait.record.timestamp = now();
            wait.revision = revision;
        }
    }

    /// The waits of suspended answers that have not ended, in the order they began.
    pub(crate) fn suspended_waits(&self) -> Vec<WaitEntry> {
        let mut waits = self
            .waits
            .values()
            .filter(|wait| {
                matches!(
                    wait.record.state,
                    WaitState::Open | WaitState::Resolved { .. }
                ) && self
                    .responses
                    .get(&wait.record.response_id)
                    .is_some_and(|response| response.record.state == ResponseState::Suspended)
            })
            .collect::<Vec<_>>();
        waits.sort_by_key(|wait| wait.revision);
        waits.into_iter().map(|wait| wait.record.clone()).collect()
    }

    /// A suspended answer restored from its records goes on in this instance: the messages its
    /// Run appends from `next_message` on belong to it.
    pub(crate) fn reactivate(&mut self, run: RunId, response_id: ResponseId, next_message: usize) {
        self.active.insert(
            run,
            ActiveResponse {
                response_id,
                next_message,
            },
        );
    }

    /// The conversation was cut: the answer started last is no longer on it.
    pub(crate) fn forget_tail_answer(&mut self) {
        self.tail_answer = None;
    }

    /// The conversation is being compacted while `leaf` is its display leaf.
    pub(crate) fn remember_compacted_leaf(&mut self, leaf: Option<Uuid>) {
        if leaf.is_some() {
            self.compacted_leaf = leaf;
        }
    }

    pub(crate) fn compacted_leaf(&self) -> Option<Uuid> {
        self.compacted_leaf
    }

    /// The display node the Chat shows as current: where the conversation's messages end, or the
    /// answer started after it that no message stands for yet (awaiting its first delta, or
    /// ended without content).
    pub(crate) fn display_leaf(
        &self,
        snapshot: &ChatSnapshot,
        message_responses: &BTreeMap<usize, ResponseId>,
    ) -> Option<Uuid> {
        let anchor = self
            .latest_display_node(snapshot, message_responses)
            .or(self.compacted_leaf);
        self.tail_answer
            .filter(|answer| {
                self.responses
                    .get(answer)
                    .is_some_and(|response| response.record.display_parent == anchor)
                    && !message_responses.values().any(|claimed| claimed == answer)
            })
            .or(anchor)
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
            .chain(
                self.waits
                    .values()
                    .filter(|wait| wait.revision > written)
                    .map(|wait| (wait.revision, Line::Wait(wait.record.clone()))),
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
    pub(crate) fn latest_display_node(
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
                    if matches!(entry.state, InputState::Accepted | InputState::Started) {
                        lifecycle.open_inputs.insert(entry.run_id, entry.input_id);
                    } else if lifecycle.open_inputs.get(&entry.run_id) == Some(&entry.input_id) {
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
                Line::Wait(entry) => {
                    lifecycle.waits.insert(
                        entry.wait_id,
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

    /// After a load: an answer that never ended lost its host, unless it is a suspended answer
    /// in `rebuilt`, which goes on in this instance; the waits of the lost ones are abandoned.
    /// Then the answer without messages that hangs where the loaded chain ends, if any, is its
    /// display leaf.
    pub(crate) fn settle_after_load(
        &mut self,
        snapshot: &ChatSnapshot,
        message_responses: &BTreeMap<usize, ResponseId>,
        rebuilt: &HashSet<ResponseId>,
    ) {
        let orphaned = self
            .responses
            .values()
            .filter(|response| {
                !response.record.state.is_ended() && !rebuilt.contains(&response.record.response_id)
            })
            .map(|response| response.record.response_id)
            .collect::<Vec<_>>();
        for id in &orphaned {
            self.abandon_waits(*id, AbandonReason::HostLost);
            let revision = self.next_revision();
            let response = self.responses.get_mut(id).unwrap();
            response.record.state = ResponseState::Ended {
                ending: ResponseEnding::Interrupted {
                    by: InterruptedBy::Host,
                },
            };
            response.record.without_messages =
                !message_responses.values().any(|claimed| claimed == id);
            response.record.timestamp = now();
            response.revision = revision;
        }
        let anchor = self
            .latest_display_node(snapshot, message_responses)
            .or(self.compacted_leaf);
        self.tail_answer = self
            .responses
            .values()
            .filter(|response| {
                response.record.without_messages && response.record.display_parent == anchor
            })
            .max_by_key(|response| response.revision)
            .map(|response| response.record.response_id);
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

/// [`Lifecycle::replay`] in the Chat: an acceptance that replaces one lost before it started
/// finds that acceptance live once another delivery of the input replaced it, and attaches to it.
pub(crate) fn replay(chat: &Chat, input: &LedgerInput) -> Option<Result<Receipt, AgentError>> {
    let chat_id = chat.snapshot.id;
    match chat.bookkeeping.lifecycle.replay(chat_id, input) {
        None if input.replaces.is_some() => ledger_state(chat, input.id)
            .filter(|state| state.is_live)
            .map(|state| {
                Ok(Receipt {
                    chat_id,
                    loop_id: state.entry.loop_id,
                    run_id: state.entry.run_id,
                })
            }),
        replay => replay,
    }
}

pub(crate) fn ledger_state(chat: &Chat, input: InputId) -> Option<InputLedgerState> {
    chat.bookkeeping
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
        })
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
        Ok(ledger_state(chat, input))
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
