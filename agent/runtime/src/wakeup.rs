//! The wake-up base: how every Trigger reaches the Chat's Loop.
//!
//! A source only decides when it has a delivery, how its deliveries wait ([`Coalesce`]) and what
//! their content is to the Agent ([`tickloop_agent_protocol::Authority`]). Everything else is
//! here, once: the Loop's receiving switch, the eligibility check every delivery passes when it
//! is queued and again when it is taken, the one entry deliveries come through, under the
//! Chat's lease and a ledger id the source names, and withdrawing queued input its source no
//! longer stands behind. Every rule matches on [`TriggerSource`] without a catch-all, so a new
//! source answers to each of them.

use super::loop_lease::{ReceivingChange, TriggerPermit};
use super::*;
use tickloop_agent_protocol::{
    ChatSnapshot, Coalesce, LoopSnapshot, LoopState, OperationRecord, PauseReason, Receipt,
    Receiving, TriggerSource,
};

const STOPPED_BEFORE_ADMISSION: &str = "the Chat was stopped before Shell admission";

/// One delivery a source hands to the base.
pub(super) struct TriggerDelivery {
    pub(super) source: TriggerSource,
    /// What the model reads.
    pub(super) input: String,
    /// The ledger input a host named for it; otherwise the source names one.
    pub(super) ledger: Option<lifecycle::LedgerInput>,
}

/// Why a delivery waits at its source instead of entering the Loop's queue, or why a queued one
/// is not taken yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hold {
    /// The receiving switch is paused.
    Paused,
    /// The Loop's turn has not ended; a source that keeps only its latest delivery merges into
    /// it meanwhile.
    Busy,
    /// A plan waits for the user's review.
    PlanReview,
    /// Plan mode holds back what would only continue the work.
    PlanMode,
    /// The artifact link's account signed out or lost access.
    LinkUnavailable,
    /// The delivery carries an update the Chat already took, as another process holding its
    /// lease delivered it; the source sorts it out and delivers the rest.
    Delivered,
}

impl Hold {
    fn reason(self) -> &'static str {
        match self {
            Self::Paused => {
                "this Chat's Loop has paused receiving; it resumes when the Chat is continued"
            }
            Self::Busy => "this Chat's Loop takes it once its current turn ends",
            Self::PlanReview => "this Chat's Loop waits for its plan to be reviewed",
            Self::PlanMode => "Plan mode holds it back",
            Self::LinkUnavailable => "the artifact link's account is unavailable",
            Self::Delivered => "this Chat already took an update the delivery carries",
        }
    }
}

/// Whether a source's delivery may reach the Loop now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Eligibility {
    Eligible,
    Hold(Hold),
    /// The source no longer stands behind it: it is refused, and withdrawn if queued.
    Ended(DiscardReason),
}

/// What the entry does with a delivery.
enum Admission {
    Queue,
    /// Another delivery of the same key waits; this one merges into it.
    MergeInto(Receipt),
    Hold(Hold),
    Refuse(DiscardReason),
}

fn refusal(reason: DiscardReason) -> &'static str {
    match reason {
        DiscardReason::SourceEnded => "its Trigger source ended or could not be proved",
        DiscardReason::Stopped => "its Agent was stopped",
        DiscardReason::Unsubscribed => "its Tick Subscription was cancelled",
        DiscardReason::Unlinked => "its artifact is no longer linked to this Chat",
    }
}

/// The ledger input a delivery is accepted under, named by what it delivers: a delivery repeated
/// after a crash, by another process or by a host waking the Chat again finds its record and runs
/// nothing again.
pub(super) fn trigger_ledger(
    chat_id: ChatId,
    source: &TriggerSource,
    input: &str,
) -> lifecycle::LedgerInput {
    match source {
        TriggerSource::ArtifactFeedback {
            origin,
            account_id,
            artifact_id,
            feedback_id,
            delivery_id,
        } => turn::delivery_ledger(event_sources::ExternalDelivery::named(
            origin,
            account_id,
            artifact_id,
            feedback_id,
            *delivery_id,
            input.to_owned(),
        )),
        TriggerSource::TickUpdate { .. }
        | TriggerSource::TickManual { .. }
        | TriggerSource::GoalContinuation { .. }
        | TriggerSource::ScheduledTask { .. } => {
            // The id is named by the delivery's whole identity, so the payload is the id itself.
            let id = tickloop_agent_transcript::input_ids::trigger(chat_id, &source.identity());
            lifecycle::LedgerInput::new(id, InputOp::Trigger, &id)
        }
    }
}

/// Whether the Loop has anything that may wake it: it receives, from a Tick Subscription or an
/// available artifact link.
pub(super) fn listens(chat_loop: &LoopSnapshot) -> bool {
    chat_loop.receiving.is_receiving()
        && (!chat_loop.subscriptions.is_empty()
            || chat_loop
                .event_sources
                .iter()
                .any(|source| !source.resume_required))
}

/// The receiving switch's history-load rule: a Loop that enters a new process from its history
/// (a restart, `--fork-session` or a Branch copy) pauses receiving, and its running or waiting
/// turn becomes interrupted. A durable host's load is how it runs the Chat, not a history load:
/// it keeps receiving, and a Loop whose Run is rebuilt (`keeps_run`) keeps its state.
pub(super) fn recover_loop(snapshot: &mut ChatSnapshot, keeps_run: bool, waits: WaitHosting) {
    match waits {
        WaitHosting::Process => {
            if snapshot.chat_loop.receiving.is_receiving() {
                snapshot.chat_loop.receiving = Receiving::Paused {
                    reason: PauseReason::HistoryLoaded,
                };
            }
        }
        WaitHosting::Durable => {}
    }
    if keeps_run {
        return;
    }
    let chat_loop = &mut snapshot.chat_loop;
    if matches!(
        chat_loop.state,
        LoopState::Running
            | LoopState::Queued
            | LoopState::WaitingPermission
            | LoopState::WaitingQuestion
    ) {
        chat_loop.recover_state(LoopState::Interrupted);
    }
}

/// Pauses receiving. The sources keep what they have and the queued Trigger input waits; only
/// the artifact watcher, whose requests the service keeps, stops.
fn pause_receiving(chat: &mut Chat, reason: PauseReason) {
    chat.snapshot.chat_loop.receiving = Receiving::Paused { reason };
    if let Some(cancel) = chat.feedback_watcher.take() {
        cancel.cancel();
    }
}

/// Resumes receiving; whether it was paused.
fn resume_receiving(chat: &mut Chat) -> bool {
    let was_paused = !chat.snapshot.chat_loop.receiving.is_receiving();
    chat.snapshot.chat_loop.receiving = Receiving::Receiving;
    was_paused
}

impl Agent {
    /// The one eligibility check, at the entry and again when queued input is taken: the source
    /// still stands behind the delivery, then the receiving switch, then what the source holds
    /// it back for.
    pub(super) fn eligibility(&self, chat: &Chat, source: &TriggerSource) -> Eligibility {
        let snapshot = &chat.snapshot;
        let source_condition = match source {
            TriggerSource::TickUpdate { items } => {
                self.tick_deliveries()
                    .items_eligibility(snapshot.id, &snapshot.chat_loop, items)
            }
            TriggerSource::TickManual { instance, .. } => {
                tick_subscriptions::manual_eligibility(snapshot, instance)
            }
            TriggerSource::ArtifactFeedback {
                origin,
                account_id,
                artifact_id,
                ..
            } => event_sources::feedback_eligibility(
                &snapshot.chat_loop,
                origin,
                account_id,
                artifact_id,
            ),
            TriggerSource::GoalContinuation { .. } => {
                if snapshot.policy.mode == ExecutionMode::Plan {
                    Eligibility::Hold(Hold::PlanMode)
                } else {
                    Eligibility::Eligible
                }
            }
            TriggerSource::ScheduledTask { .. } => Eligibility::Eligible,
        };
        match (source_condition, snapshot.chat_loop.receiving) {
            (Eligibility::Ended(reason), _) => Eligibility::Ended(reason),
            (Eligibility::Eligible | Eligibility::Hold(_), Receiving::Paused { .. }) => {
                Eligibility::Hold(Hold::Paused)
            }
            (condition, Receiving::Receiving) => condition,
        }
    }

    /// Whether queued Trigger input may start a turn now: only once the Loop's turn has ended,
    /// and only while it is still eligible.
    pub(super) fn takes_trigger_input(&self, chat: &Chat, source: &TriggerSource) -> Eligibility {
        match chat.snapshot.chat_loop.state {
            LoopState::Idle | LoopState::Queued | LoopState::Interrupted | LoopState::Failed => {
                self.eligibility(chat, source)
            }
            LoopState::WaitingPlan => match self.eligibility(chat, source) {
                Eligibility::Ended(reason) => Eligibility::Ended(reason),
                Eligibility::Eligible | Eligibility::Hold(_) => Eligibility::Hold(Hold::PlanReview),
            },
            LoopState::Running | LoopState::WaitingPermission | LoopState::WaitingQuestion => {
                match self.eligibility(chat, source) {
                    Eligibility::Ended(reason) => Eligibility::Ended(reason),
                    Eligibility::Eligible | Eligibility::Hold(_) => Eligibility::Hold(Hold::Busy),
                }
            }
        }
    }

    /// What the entry does with a delivery, under the Chats lock: an eligible delivery queues
    /// unless its source's coalescing keeps it at the source or merges it into one waiting.
    fn admission(&self, chat: &Chat, source: &TriggerSource) -> Admission {
        match self.eligibility(chat, source) {
            Eligibility::Eligible => {}
            Eligibility::Hold(hold) => return Admission::Hold(hold),
            Eligibility::Ended(reason) => return Admission::Refuse(reason),
        }
        let carries_delivered = match source {
            TriggerSource::TickUpdate { items } => {
                self.tick_deliveries().holds_delivered_items(items)
            }
            TriggerSource::TickManual { .. }
            | TriggerSource::ArtifactFeedback { .. }
            | TriggerSource::GoalContinuation { .. }
            | TriggerSource::ScheduledTask { .. } => false,
        };
        if carries_delivered {
            return Admission::Hold(Hold::Delivered);
        }
        let waiting = chat.snapshot.pending_inputs.iter().find(|pending| {
            pending
                .trigger_source
                .as_ref()
                .is_some_and(|queued| source.shares_wait_with(queued))
        });
        match source.coalesce() {
            Coalesce::LatestOnly if waiting.is_some() => Admission::Hold(Hold::Busy),
            Coalesce::LatestOnly => match chat.snapshot.chat_loop.state {
                LoopState::Idle | LoopState::Interrupted | LoopState::Failed => Admission::Queue,
                LoopState::WaitingPlan => Admission::Hold(Hold::PlanReview),
                LoopState::Queued
                | LoopState::Running
                | LoopState::WaitingPermission
                | LoopState::WaitingQuestion => Admission::Hold(Hold::Busy),
            },
            Coalesce::EachItem => Admission::Queue,
            Coalesce::OnePerKey(_) => match waiting {
                Some(pending) => Admission::MergeInto(pending.receipt.clone()),
                None => Admission::Queue,
            },
        }
    }

    /// Whether a delivery of `source` would be queued now; a source that keeps only its latest
    /// delivery asks before it prepares one.
    pub(super) fn would_admit(&self, chat: &Chat, source: &TriggerSource) -> bool {
        match self.admission(chat, source) {
            Admission::Queue | Admission::MergeInto(_) => true,
            Admission::Hold(_) | Admission::Refuse(_) => false,
        }
    }

    /// The entry under the Chats lock, from the submission: `Ok(None)` queues the delivery,
    /// `Ok(Some(receipt))` merged it into the waiting input `receipt`.
    pub(super) fn admit_trigger(
        &self,
        chat: &Chat,
        source: &TriggerSource,
    ) -> Result<Option<Receipt>, AgentError> {
        match self.admission(chat, source) {
            Admission::Queue => Ok(None),
            Admission::MergeInto(receipt) => Ok(Some(receipt)),
            Admission::Hold(hold) => Err(AgentError::Invalid(hold.reason().into())),
            Admission::Refuse(reason) => Err(AgentError::Invalid(format!(
                "the delivery was refused: {}",
                refusal(reason)
            ))),
        }
    }

    /// A delivery that merged into the waiting input `into` keeps its own ledger record, which
    /// names its source and what it merged into. Its receipt is `into`'s, the run that answers
    /// it, on its first acceptance and on every replay.
    pub(super) fn record_merged_trigger(
        &self,
        chat: &mut Chat,
        ledger: &lifecycle::LedgerInput,
        source: &TriggerSource,
        into: &Receipt,
    ) {
        chat.bookkeeping.lifecycle.record_settled(
            ledger,
            into,
            SettledOutcome::Merged { into: into.run_id },
            Some(tickloop_agent_transcript::TriggerRecord::of(source)),
        );
    }

    /// The one entry every Trigger delivery comes through: within the Chat's Trigger boundary,
    /// under its lease, after its source proves it again, it is queued or merged as its source
    /// coalesces, or refused. An error leaves the delivery with its source.
    pub(super) async fn deliver_trigger(
        &self,
        chat_id: ChatId,
        delivery: TriggerDelivery,
    ) -> Result<Receipt, AgentError> {
        let boundary = self.enter_trigger_boundary(chat_id).await;
        let holding = match self.acquire_trigger_permit(chat_id, &boundary).await {
            TriggerPermit::Fire(holding) => holding,
            TriggerPermit::Wait { reason, .. } => return Err(AgentError::Invalid(reason)),
            TriggerPermit::Withdrawn => {
                return Err(AgentError::Invalid(
                    "receiving was paused in another Tickloop process; the delivery waits".into(),
                ));
            }
            TriggerPermit::Failed(error) => return Err(AgentError::Storage(error)),
        };
        // Boxed: proving a source reads its Tick, which must not enlarge every caller's stack.
        Box::pin(self.revalidate_trigger_source(chat_id, &delivery.source)).await?;
        let TriggerDelivery {
            source,
            input,
            ledger,
        } = delivery;
        Box::pin(self.submit_internal(
            chat_id,
            input,
            SubmissionOptions {
                trigger_source: Some(source),
                trigger_lease: Some((holding, boundary)),
                ledger,
                ..Default::default()
            },
        ))
        .await
    }

    /// What the source proves again outside the Chats lock before its delivery is queued or
    /// taken: that the Tick it read is still the one its watch was admitted for.
    pub(super) async fn revalidate_trigger_source(
        &self,
        chat_id: ChatId,
        source: &TriggerSource,
    ) -> Result<(), AgentError> {
        match source {
            TriggerSource::TickUpdate { items } => {
                self.revalidate_tick_source(chat_id, items).await
            }
            TriggerSource::TickManual {
                instance,
                authority,
                ..
            } => {
                self.revalidate_manual_tick_source(chat_id, instance, authority)
                    .await
            }
            TriggerSource::ArtifactFeedback { .. }
            | TriggerSource::GoalContinuation { .. }
            | TriggerSource::ScheduledTask { .. } => Ok(()),
        }
    }

    /// Withdraws the queued Trigger input its source no longer stands behind; whether it did.
    pub(super) fn withdraw_ended_inputs(&self, chat: &mut Chat) -> bool {
        let ended: Vec<_> = chat
            .snapshot
            .pending_inputs
            .iter()
            .filter_map(|pending| {
                let source = pending.trigger_source.as_ref()?;
                match self.eligibility(chat, source) {
                    Eligibility::Ended(reason) => Some((pending.receipt.clone(), reason)),
                    Eligibility::Eligible | Eligibility::Hold(_) => None,
                }
            })
            .collect();
        if ended.is_empty() {
            return false;
        }
        for (receipt, reason) in ended {
            self.withdraw_trigger_input(chat, &receipt, reason);
        }
        true
    }

    /// Takes one queued Trigger input back with the reason its source gave.
    pub(super) fn withdraw_trigger_input(
        &self,
        chat: &mut Chat,
        receipt: &Receipt,
        reason: DiscardReason,
    ) {
        chat.snapshot
            .pending_inputs
            .retain(|pending| pending.receipt.run_id != receipt.run_id);
        self.settle_input(chat, receipt.run_id, SettledOutcome::Discarded { reason });
        self.inner
            .host_contexts
            .lock()
            .unwrap()
            .runs
            .remove(&receipt.run_id);
        if chat.snapshot.pending_inputs.is_empty()
            && chat.snapshot.chat_loop.state == LoopState::Queued
        {
            chat.snapshot.chat_loop.set_state(LoopState::Idle);
        }
    }

    pub(super) async fn withdraw_every_ended_input(&self) {
        let mut chats = self.inner.chats.lock().await;
        for chat in chats.values_mut() {
            if self.withdraw_ended_inputs(chat) {
                self.save(&chat.snapshot);
            }
        }
    }

    /// The Chat's Trigger state as a load would build it from the store, taken over under a
    /// newly held lease: the ledger's Trigger records, the Trigger input that waits, and the
    /// Tick sequences delivered. What another process accepted, ran or settled while it held the
    /// lease is known here as it is there. The user's input and Agent messages are this
    /// process's own and stay as they are.
    pub(super) async fn reload_trigger_state(&self, chat_id: ChatId) -> Result<(), String> {
        // Nothing of the Chat is stored yet, so no other process took anything of it.
        if !self.transcript_materialized(chat_id) {
            return Ok(());
        }
        {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or("the Chat is no longer loaded")?;
            self.checkpoint_transcript(&chat.snapshot, &chat.bookkeeping)
                .await?;
        }
        let lines = match self.read_stored_lines(chat_id).await {
            Ok(lines) => lines,
            Err(AgentError::NotFound) => Vec::new(),
            Err(error) => return Err(error.to_string()),
        };
        #[cfg(any(test, feature = "test-support"))]
        self.pause_point(chat_id, trigger_pauses::PausePoint::TakeoverRead)
            .await;
        let (delivered, takes_waiting_input) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&chat_id)
                .ok_or("the Chat is no longer loaded")?;
            chat.bookkeeping.lifecycle.adopt_trigger_records(&lines);
            let lifecycle = &chat.bookkeeping.lifecycle;
            let (kept, taken_elsewhere): (Vec<_>, Vec<_>) =
                std::mem::take(&mut chat.snapshot.pending_inputs)
                    .into_iter()
                    .partition(|pending| {
                        pending.trigger_source.is_none()
                            || lifecycle.waits_to_start(pending.receipt.run_id)
                    });
            chat.snapshot.pending_inputs = kept;
            for pending in &taken_elsewhere {
                chat.feedback_inputs.remove(&pending.receipt.run_id);
                self.inner
                    .host_contexts
                    .lock()
                    .unwrap()
                    .runs
                    .remove(&pending.receipt.run_id);
            }
            self.requeue_inputs(chat, durable_queue::is_trigger_input);
            if chat.snapshot.pending_inputs.is_empty()
                && chat.snapshot.chat_loop.state == LoopState::Queued
            {
                chat.snapshot.chat_loop.set_state(LoopState::Idle);
            }
            self.save(&chat.snapshot);
            let takes_waiting_input = chat.active.is_none()
                && chat
                    .snapshot
                    .pending_inputs
                    .iter()
                    .any(|pending| pending.trigger_source.is_some());
            (
                chat.bookkeeping.lifecycle.delivered_tick_sequences(),
                takes_waiting_input,
            )
        };
        self.refresh_tick_watermarks(chat_id, delivered).await;
        if takes_waiting_input {
            self.inner
                .tasks
                .spawn(self.clone().start_waiting_input(chat_id));
        }
        Ok(())
    }

    /// Starts what waits in an idle Loop. Its future is named, not inferred: starting a run asks
    /// for the lease, whose taking over may start this again.
    fn start_waiting_input(self, chat_id: ChatId) -> futures_util::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            let _ = self.start_next_agent_run(chat_id).await;
        })
    }

    /// The pause another process recorded: this process pauses too, without recording it again.
    pub(super) async fn pause_receiving_paused_elsewhere(&self, chat_id: ChatId) {
        {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return;
            };
            pause_receiving(chat, PauseReason::Stopped);
            self.save(&chat.snapshot);
        }
        self.inner.chat_leases.release(chat_id);
    }

    /// The Chat is continued: its Loop receives again, its sources deliver what they kept, and
    /// Trigger input queued while it was paused may start. A Loop already receiving only looks
    /// again at its artifact links, whose account may be back.
    pub(super) async fn continue_receiving(&self, chat_id: ChatId) {
        if let Some(loop_id) = self
            .change_receiving(chat_id, ReceivingChange::Resumed)
            .await
        {
            self.flush_due_tick_delivery(chat_id, loop_id);
        }
        Box::pin(self.resume_feedback_sources(chat_id)).await;
    }

    /// Turns the receiving switch here and records it for other processes in one operation of
    /// the Chat's Trigger boundary, so the switch here and the recorded one never part: a pause
    /// also releases the lease, after whatever the boundary was doing. The Loop's id when the
    /// switch turned; a resume of a Loop already receiving changes and records nothing.
    async fn change_receiving(&self, chat_id: ChatId, change: ReceivingChange) -> Option<LoopId> {
        let boundary = self.enter_trigger_boundary(chat_id).await;
        let changed = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id)?;
            let changed = match change {
                ReceivingChange::Paused => {
                    pause_receiving(chat, PauseReason::Stopped);
                    true
                }
                ReceivingChange::Resumed => resume_receiving(chat),
            };
            if changed {
                self.save(&chat.snapshot);
            }
            changed.then_some(chat.snapshot.chat_loop.id)
        };
        changed?;
        #[cfg(any(test, feature = "test-support"))]
        self.pause_point(chat_id, trigger_pauses::PausePoint::ReceivingChanged)
            .await;
        self.record_receiving(chat_id, change, boundary).await;
        changed
    }

    /// `/stop`: interrupts the current turn, with every turn that waits for a person, discards
    /// the queued messages and Shell commands that have not started, and pauses receiving until
    /// the Chat is continued. Queued Trigger input waits for that; background Shells and Ticks go
    /// on.
    pub(super) async fn stop_loop(
        &self,
        chat_id: ChatId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        let operations = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let operations = chat.interrupt_every_run();
            let (discarded, kept) = std::mem::take(&mut chat.snapshot.pending_inputs)
                .into_iter()
                .partition::<Vec<_>, _>(|pending| pending.trigger_source.is_none());
            chat.snapshot.pending_inputs = kept;
            self.settle_removed_inputs(
                chat,
                &discarded,
                SettledOutcome::Discarded {
                    reason: DiscardReason::Stopped,
                },
            );
            let mut cancelled_shells = HashSet::new();
            for shell in &mut chat.snapshot.user_shells {
                if shell.state == UserShellState::Queued || shell.awaits_admission() {
                    cancelled_shells.insert(shell.id);
                    shell.armed = false;
                    shell.set_state(UserShellState::Failed);
                    shell.reason = Some(STOPPED_BEFORE_ADMISSION.into());
                    if let Some(cancel) = self.take_user_shell_cancellation(chat_id, shell.id) {
                        cancel.cancel();
                    }
                    let _ = self.inner.user_shell_events.send(shell.clone());
                }
            }
            self.withdraw_user_shell_requests(&mut chat.snapshot, |command_id| {
                cancelled_shells.contains(&command_id)
            })
            .await;
            pause_receiving(chat, PauseReason::Stopped);
            self.save(&chat.snapshot);
            operations
        };
        // Paused here at once, so no Trigger input starts meanwhile, and again with the record,
        // before the lease is returned: a process taking it over sees the pause.
        self.change_receiving(chat_id, ReceivingChange::Paused)
            .await;
        Ok(operations)
    }

    /// Continues the Chat without a new input.
    pub async fn continue_loop(&self, chat_id: ChatId) -> Result<(), AgentError> {
        if !self.inner.chats.lock().await.contains_key(&chat_id) {
            return Err(AgentError::NotFound);
        }
        self.continue_receiving(chat_id).await;
        if let Some(next) = self.next_run(chat_id).await? {
            self.run_task(next);
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Agent {
    /// Hands a delivery of `source` to the one entry, for tests standing in for a source that
    /// has no producer of its own yet.
    #[doc(hidden)]
    pub async fn deliver_trigger_for_tests(
        &self,
        chat_id: ChatId,
        source: TriggerSource,
        input: &str,
    ) -> Result<Receipt, AgentError> {
        self.deliver_trigger(
            chat_id,
            TriggerDelivery {
                source,
                input: input.into(),
                ledger: None,
            },
        )
        .await
    }

    /// Takes back the queued deliveries `ended` names, as a source that stopped standing behind
    /// them does.
    #[doc(hidden)]
    pub async fn withdraw_trigger_for_tests(
        &self,
        chat_id: ChatId,
        ended: impl Fn(&TriggerSource) -> bool,
    ) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        let withdrawn: Vec<_> = chat
            .snapshot
            .pending_inputs
            .iter()
            .filter(|pending| pending.trigger_source.as_ref().is_some_and(&ended))
            .map(|pending| pending.receipt.clone())
            .collect();
        for receipt in withdrawn {
            self.withdraw_trigger_input(chat, &receipt, DiscardReason::SourceEnded);
        }
        self.save(&chat.snapshot);
    }

    /// Pauses the Chat the next time it reaches `point`; dropping `release` lets it go on.
    #[doc(hidden)]
    pub fn pause_next_for_tests(
        &self,
        chat_id: ChatId,
        point: trigger_pauses::PausePoint,
    ) -> trigger_pauses::Pause {
        trigger_pauses::pause_next(self.pause_key(), chat_id, point)
    }

    /// Holds the Chat at `point` if a test asked to.
    pub(super) async fn pause_point(&self, chat_id: ChatId, point: trigger_pauses::PausePoint) {
        trigger_pauses::wait_if_paused(self.pause_key(), chat_id, point).await;
    }

    /// Holds the thread at `point` if a test asked to, for a point within work that does not
    /// yield.
    pub(super) fn pause_point_blocking(&self, chat_id: ChatId, point: trigger_pauses::PausePoint) {
        trigger_pauses::wait_if_paused_blocking(self.pause_key(), chat_id, point);
    }

    /// Pauses are this Agent's own: another Agent in the same test process holding the same Chat
    /// passes the same points.
    fn pause_key(&self) -> usize {
        std::sync::Arc::as_ptr(&self.inner) as usize
    }
}

/// Points within the Chat's Trigger boundary a test can hold a Chat at.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod trigger_pauses {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use tickloop_agent_protocol::ChatId;
    use tokio::sync::oneshot;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum PausePoint {
        /// A takeover read the store and has not applied it.
        TakeoverRead,
        /// A permit is about to check the lease, on the thread that runs it.
        LeaseChecking,
        /// The receiving switch turned here and is not recorded yet.
        ReceivingChanged,
        /// The receiving switch was recorded, and a pause has not released its holding.
        ReceivingRecorded,
        /// Queued Trigger input passed its permit and the check of its holding, and has not
        /// started.
        TriggerChecked,
    }

    type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);
    /// A point of one Chat in one Agent.
    type At = (usize, ChatId, PausePoint);

    static GATES: Mutex<Option<HashMap<At, Gate>>> = Mutex::new(None);

    pub struct Pause {
        pub entered: oneshot::Receiver<()>,
        pub release: oneshot::Sender<()>,
    }

    pub(crate) fn pause_next(agent: usize, chat: ChatId, point: PausePoint) -> Pause {
        let (entered_sender, entered) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        GATES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert((agent, chat, point), (entered_sender, release_receiver));
        Pause { entered, release }
    }

    pub(crate) fn wait_if_paused_blocking(agent: usize, chat: ChatId, point: PausePoint) {
        let gate = GATES
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|gates| gates.remove(&(agent, chat, point)));
        if let Some((entered, mut release)) = gate {
            let _ = entered.send(());
            // Waiting within the runtime's own thread: polled, as a blocking receive would refuse.
            while matches!(release.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    pub(crate) async fn wait_if_paused(agent: usize, chat: ChatId, point: PausePoint) {
        let gate = GATES
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|gates| gates.remove(&(agent, chat, point)));
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::TickDeliveryItem;

    fn tick_update(watch: u128, updates: u64, items: &[(u128, u64)]) -> TriggerSource {
        TriggerSource::TickUpdate {
            items: items
                .iter()
                .map(|(subscription, sequence)| TickDeliveryItem {
                    subscription: Uuid::from_u128(*subscription),
                    watch: Uuid::from_u128(watch),
                    instance: "calendar:fixture".parse().unwrap(),
                    sequence: *sequence,
                    updates,
                })
                .collect(),
        }
    }

    /// A delivery repeated through another watch, read again later or merging more updates is
    /// the same delivery: its ledger input matches, so the ledger replays it instead of refusing
    /// it as an id reused with another payload.
    #[test]
    fn a_repeated_delivery_names_the_same_ledger_input() {
        let chat = Uuid::new_v4();
        let same = |left: &TriggerSource, right: &TriggerSource| {
            let (left, right) = (
                trigger_ledger(chat, left, "first rendering"),
                trigger_ledger(chat, right, "second rendering"),
            );
            left.id == right.id && left.digest == right.digest
        };
        assert!(same(
            &tick_update(1, 1, &[(10, 4), (11, 7)]),
            &tick_update(2, 3, &[(11, 7), (10, 4)]),
        ));
        assert!(!same(
            &tick_update(1, 1, &[(10, 4)]),
            &tick_update(1, 1, &[(10, 5)]),
        ));
        let manual = |read_at_ms| TriggerSource::TickManual {
            instance: "calendar:fixture".parse().unwrap(),
            sequence: 9,
            read_at_ms,
            authority: None,
        };
        assert!(same(&manual(1_000), &manual(2_000)));
    }
}
