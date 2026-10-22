//! The turn entry point: a host's input under its own id, the display node it follows, and the
//! outcome the host waits for.
//!
//! The input ledger makes `advance` safe to repeat: an input it already holds is attached to,
//! answered with its recorded outcome, or continued when its answer lost its host. An explicitly
//! selected display node is checked when the input is accepted, and the Chat must be idle for it;
//! a node off the current chain is reached by rebuilding the Chat on that branch from its store.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use tickloop_agent_protocol::{
    ChatId, ExecutionMode, HostCallContext, InputContext, InputId, LoopId, LoopState,
    OperationRecord, OperationState, PermissionRequest, QuestionAnswer, QuestionRequest, Receipt,
    RunId, Usage,
};
use uuid::Uuid;

use crate::event_sources::ExternalDelivery;
use crate::lifecycle::{LedgerInput, ledger_state};
use crate::{
    Agent, AgentError, Chat, LoopSnapshotExt, TurnAdmission, WeakAgent, chat_is_busy, rewind,
};
use tickloop_agent_transcript::display::{DisplayKind, DisplayNode, DisplayStatus, project};
use tickloop_agent_transcript::{
    DiscardReason, InputEntry, InputOp, InputState, InterruptedBy, Line, ResponseEnding,
    ResponseState, SettledOutcome,
};

/// A display node's id: a prompt's earliest source id, or an answer's response id.
pub type MessageId = Uuid;

/// How often a wait on a Chat looks at it again when no event arrives.
const RECHECK: Duration = Duration::from_millis(100);
/// How often a wait for a turn's outcome looks again when no event or ledger change arrives.
const OUTCOME_RECHECK: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct TurnRequest {
    /// The host's id for this input; repeating it repeats nothing.
    pub input_id: InputId,
    pub op: TurnOp,
    pub delivery: Delivery,
    pub host_context: Option<HostCallContext>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum TurnOp {
    /// A prompt after `after`.
    Continue {
        after: ParentSelection,
        target_loop: Option<LoopId>,
        message: UserMessage,
    },
    /// A prompt in place of the top-level prompt `prompt`, as its sibling.
    ReplacePrompt {
        prompt: MessageId,
        message: UserMessage,
    },
    /// Another answer to the prompt of the ended answer `answer`, as its sibling.
    RegenerateAnswer { answer: MessageId },
    /// A user Skill invocation after `after`.
    Skill {
        after: ParentSelection,
        target_loop: Option<LoopId>,
        skill: SkillInvocation,
    },
    /// The user's reply to the question `request` a Run waits on; it joins that Run.
    Answer { request: Uuid, reply: QuestionReply },
    /// An @tickloop request delivered to the Loop `target_loop`, under the input id its delivery
    /// names. It queues behind the Loop's work, whatever the delivery mode, and a Loop that has
    /// ended discards it.
    Deliver {
        target_loop: LoopId,
        delivery: ExternalDelivery,
    },
}

/// How the user settles a question.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum QuestionReply {
    /// One answer per question, in the order the questions were asked.
    Answered(Vec<QuestionAnswer>),
    /// The question is declined, as Esc does in its dialog; the waiting Run ends there.
    Declined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ParentSelection {
    /// Wherever the conversation ends when the input starts or joins a Run.
    CurrentLeaf,
    /// A new root: the first message of an empty Chat, or a sibling of the existing roots.
    Root,
    /// This display node, checked when the input is accepted.
    Message(MessageId),
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UserMessage {
    pub text: String,
    pub contexts: Vec<InputContext>,
    /// Files and directories attached by reference; a Chat without a Folder refuses them.
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SkillInvocation {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    /// Wait behind the work ahead; an explicitly selected node then needs an idle Chat.
    #[default]
    Queue,
    /// Interrupt the work ahead first.
    SendNow,
}

/// An input `advance` accepted, or already held.
#[derive(Clone)]
pub struct Turn {
    input_id: InputId,
    receipt: Receipt,
    agent: WeakAgent,
}

#[derive(Clone, Debug)]
pub enum TurnOutcome {
    Completed {
        usage: Usage,
    },
    /// The answer waits for a person.
    Waiting(TurnWait),
    Interrupted {
        by: InterruptedBy,
        /// Operations dispatched without a known result: never repeat them automatically.
        uncertain_operations: Vec<OperationRecord>,
    },
    Failed {
        error: String,
    },
    /// Answered by the Run `into`.
    Merged {
        into: RunId,
    },
    Withdrawn,
    Discarded {
        reason: DiscardReason,
    },
    /// A newer holder owns the Chat's store: nothing of this turn may be acknowledged.
    Fenced,
}

#[derive(Clone, Debug)]
pub enum TurnWait {
    Permission(PermissionRequest),
    Question(QuestionRequest),
    /// The answer waits for the member `chat` of its family, as a foreground `Subagent` call or
    /// a member waiting for its children does, and that member waits for a person.
    Member {
        chat: ChatId,
        wait: Box<TurnWait>,
    },
}

impl Turn {
    pub fn input_id(&self) -> InputId {
        self.input_id
    }

    pub fn receipt(&self) -> &Receipt {
        &self.receipt
    }

    /// The outcome the input has reached: its settled outcome, a wait for a person, or the
    /// interruption of a host that lost it. Waits while it is queued or running.
    pub async fn outcome(&self) -> Result<TurnOutcome, AgentError> {
        match self.agent.upgrade() {
            Some(agent) => {
                agent
                    .turn_outcome(self.receipt.chat_id, self.input_id)
                    .await
            }
            None => Ok(TurnOutcome::Interrupted {
                by: InterruptedBy::Host,
                uncertain_operations: Vec::new(),
            }),
        }
    }
}

/// What the ledger makes of a request for an input.
enum Admission {
    /// It is settled, or queued, running or waiting in this instance.
    Known(Receipt),
    /// Its answer lost its host: another attempt continues it, under `answer_parent`.
    Continuation {
        entry: Box<InputEntry>,
        answer_parent: Option<Uuid>,
    },
    /// It runs, replacing an acceptance lost before it started.
    Run { replaces: Option<RunId> },
}

/// A reservation of a Chat for replacing its conversation with another stored branch. Dropped
/// without being used or released, it ends in a task of the instance.
pub(crate) struct BranchReservation {
    agent: WeakAgent,
    chat_id: ChatId,
    id: Uuid,
    held: bool,
}

impl BranchReservation {
    async fn release(mut self) {
        self.held = false;
        if let Some(agent) = self.agent.upgrade() {
            agent.release_branch(self.chat_id, self.id).await;
        }
    }
}

impl Drop for BranchReservation {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        let (Some(agent), Ok(runtime)) =
            (self.agent.upgrade(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        let (chat_id, id) = (self.chat_id, self.id);
        let releasing = agent.clone();
        agent.inner.tasks.spawn_on(
            async move { releasing.release_branch(chat_id, id).await },
            &runtime,
        );
    }
}

/// A stored branch staged under a reservation of its Chat, to install with an acceptance.
pub(crate) struct BranchSwitch {
    reservation: BranchReservation,
    staged: Box<crate::persistence::StagedBranch>,
}

/// A staged branch made into the Chat it would install, with what installing it takes.
pub(crate) struct StagedInstall {
    pub(crate) chat: Chat,
    log: crate::persistence::StagedLog,
    leaf: Uuid,
    reservation: BranchReservation,
}

impl BranchSwitch {
    pub(crate) fn reservation(&self) -> Uuid {
        self.reservation.id
    }

    /// The Chat on the staged branch, carrying `current`'s instance state.
    pub(crate) fn into_chat(self, current: &Chat) -> Box<StagedInstall> {
        let (chat, log, leaf) = (*self.staged).into_chat(current);
        Box::new(StagedInstall {
            chat,
            log,
            leaf,
            reservation: self.reservation,
        })
    }
}

/// What admitting an explicitly placed input found.
enum Placed {
    /// The ledger meanwhile holds the input.
    Known(Receipt),
    Admit(TurnAdmission),
}

/// What a selected display node must be.
#[derive(Clone, Copy)]
enum NodeRole {
    /// An ended answer, or a prompt without an answer.
    ContinuationPoint,
    Prompt,
}

impl NodeRole {
    fn admits(self, node: &DisplayNode, nodes: &[DisplayNode]) -> Result<(), AgentError> {
        match (self, &node.kind) {
            (Self::ContinuationPoint, DisplayKind::Answer { .. }) => {
                match node.status == DisplayStatus::Streaming {
                    true => Err(answer_not_ended()),
                    false => Ok(()),
                }
            }
            (Self::ContinuationPoint, DisplayKind::Prompt { .. }) => {
                match nodes.iter().any(|other| {
                    other.parent == Some(node.id)
                        && matches!(other.kind, DisplayKind::Answer { .. })
                }) {
                    true => Err(prompt_has_answer()),
                    false => Ok(()),
                }
            }
            (Self::Prompt, DisplayKind::Prompt { .. }) => Ok(()),
            (Self::Prompt, DisplayKind::Answer { .. }) => Err(only_prompts_are_replaced()),
        }
    }
}

/// Where an explicitly selected input goes.
#[derive(Clone, Copy)]
pub(crate) enum Placement {
    Root,
    After(MessageId),
    Replacing(MessageId),
}

/// A display node on the Chat's current chain, by message index.
#[derive(Clone, Copy, Debug)]
struct ChainNode {
    id: Uuid,
    is_prompt: bool,
    first: usize,
    last: usize,
}

impl TurnRequest {
    /// A new prompt queued after the current leaf: a local host's submission.
    pub fn after_current_leaf(
        target_loop: Option<LoopId>,
        text: String,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
        host_context: Option<HostCallContext>,
    ) -> Self {
        Self {
            input_id: Uuid::new_v4(),
            op: TurnOp::Continue {
                after: ParentSelection::CurrentLeaf,
                target_loop,
                message: UserMessage {
                    text,
                    contexts,
                    paths,
                },
            },
            delivery: Delivery::Queue,
            host_context,
        }
    }
}

impl TurnRequest {
    /// The delivery of an @tickloop request to the Loop `target_loop`, under the input id the
    /// delivery names.
    pub fn deliver(target_loop: LoopId, delivery: ExternalDelivery) -> Self {
        Self {
            input_id: delivery.input_id(),
            op: TurnOp::Deliver {
                target_loop,
                delivery,
            },
            delivery: Delivery::Queue,
            host_context: None,
        }
    }
}

/// The ledger input of a delivery to the Loop `target_loop`, as `advance` records it.
pub(crate) fn delivery_ledger(target_loop: LoopId, delivery: ExternalDelivery) -> LedgerInput {
    let input_id = delivery.input_id();
    let op = TurnOp::Deliver {
        target_loop,
        delivery,
    };
    LedgerInput::new(input_id, op.kind(), &op)
}

impl TurnOp {
    fn kind(&self) -> InputOp {
        match self {
            Self::Continue { .. } => InputOp::Continue,
            Self::ReplacePrompt { .. } => InputOp::ReplacePrompt,
            Self::RegenerateAnswer { .. } => InputOp::RegenerateAnswer,
            Self::Skill { .. } => InputOp::Skill,
            Self::Answer { .. } => InputOp::Answer,
            Self::Deliver { .. } => InputOp::Feedback,
        }
    }
}

impl Agent {
    /// Accepts the input `request` names, or finds it in the ledger: a settled input answers
    /// with its outcome, a live one is attached to, and one whose answer lost its host is
    /// continued by another attempt. The same id with another payload is refused.
    pub async fn advance(&self, chat_id: ChatId, request: TurnRequest) -> Result<Turn, AgentError> {
        self.advance_with_delivery_binding(chat_id, request, true)
            .await
    }

    /// The local inbox already received this input; unlinking does not revoke its admission.
    pub(super) async fn advance_received_delivery(
        &self,
        chat_id: ChatId,
        target: LoopId,
        delivery: ExternalDelivery,
    ) -> Result<Turn, AgentError> {
        self.advance_with_delivery_binding(chat_id, TurnRequest::deliver(target, delivery), false)
            .await
    }

    async fn advance_with_delivery_binding(
        &self,
        chat_id: ChatId,
        request: TurnRequest,
        requires_binding: bool,
    ) -> Result<Turn, AgentError> {
        // Held through restoring the Chat and every task the input starts: `shutdown` either
        // refuses this or waits for all of it.
        let _admission = self.inner.tasks.token();
        self.refuse_when_closed()?;
        self.make_resident(chat_id).await?;
        let TurnRequest {
            input_id,
            op,
            delivery,
            host_context,
        } = request;
        if let TurnOp::Deliver { delivery, .. } = &op
            && delivery.input_id() != input_id
        {
            return Err(AgentError::Invalid(
                "a delivery's input id is the one its delivery names".into(),
            ));
        }
        // A delivery's prompt is no part of its payload: see `ExternalDelivery`.
        let mut ledger = LedgerInput::new(input_id, op.kind(), &op);
        let admission = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            classify(chat, &ledger)?
        };
        let receipt = match admission {
            Admission::Known(receipt) => receipt,
            Admission::Continuation {
                entry,
                answer_parent,
            } => {
                // Boxed: a submission is a large future, and hosts await `advance` on small
                // stacks.
                Box::pin(self.continue_input(chat_id, &ledger, *entry, answer_parent, delivery))
                    .await?
            }
            Admission::Run { replaces } => {
                ledger.replaces = replaces;
                Box::pin(self.run_op(
                    chat_id,
                    op,
                    ledger,
                    delivery,
                    host_context,
                    requires_binding,
                ))
                .await?
            }
        };
        Ok(Turn {
            input_id,
            receipt,
            agent: WeakAgent {
                inner: std::sync::Arc::downgrade(&self.inner),
            },
        })
    }

    async fn run_op(
        &self,
        chat_id: ChatId,
        op: TurnOp,
        mut ledger: LedgerInput,
        delivery: Delivery,
        host_context: Option<HostCallContext>,
        requires_binding: bool,
    ) -> Result<Receipt, AgentError> {
        let placement = match op {
            TurnOp::Continue { after, .. } | TurnOp::Skill { after, .. } => placement(after),
            TurnOp::ReplacePrompt { prompt, .. } => Some(Placement::Replacing(prompt)),
            TurnOp::RegenerateAnswer { answer } => {
                return self.regenerate(chat_id, answer, ledger, delivery).await;
            }
            TurnOp::Answer { request, reply } => {
                return self
                    .reply_to_question(chat_id, request, reply, Some(ledger))
                    .await;
            }
            TurnOp::Deliver {
                target_loop,
                delivery,
            } => {
                if !requires_binding {
                    return self
                        .take_received_delivery(chat_id, target_loop, delivery, ledger, false)
                        .await;
                }
                return self
                    .take_delivery(chat_id, target_loop, delivery, ledger)
                    .await;
            }
        };
        let turn = match self
            .admit_placement(chat_id, placement, delivery, &mut ledger)
            .await?
        {
            Placed::Known(receipt) => return Ok(receipt),
            Placed::Admit(turn) => turn,
        };
        let reservation = turn.branch.as_ref().map(BranchSwitch::reservation);
        let accepted = match op {
            TurnOp::Continue {
                target_loop,
                message,
                ..
            } => {
                self.submit_contexts(message_submission(
                    chat_id,
                    target_loop,
                    message,
                    host_context,
                    ledger,
                    turn,
                ))
                .await
            }
            TurnOp::ReplacePrompt { message, .. } => {
                self.submit_contexts(message_submission(
                    chat_id,
                    None,
                    message,
                    host_context,
                    ledger,
                    turn,
                ))
                .await
            }
            TurnOp::Skill {
                target_loop, skill, ..
            } => {
                self.submit_skill(chat_id, target_loop, skill, host_context, ledger, turn)
                    .await
            }
            TurnOp::RegenerateAnswer { .. } | TurnOp::Answer { .. } | TurnOp::Deliver { .. } => {
                unreachable!("regenerated, answered or delivered above")
            }
        };
        // A refused input leaves the Chat as it was: the branch it staged is not installed.
        if accepted.is_err()
            && let Some(id) = reservation
        {
            self.release_branch(chat_id, id).await;
        }
        accepted
    }

    /// How the input is admitted: after the current leaf, interrupting the work ahead when sent
    /// now; or at an explicitly selected place, once the Chat is idle on that place's branch.
    async fn admit_placement(
        &self,
        chat_id: ChatId,
        placement: Option<Placement>,
        delivery: Delivery,
        ledger: &mut LedgerInput,
    ) -> Result<Placed, AgentError> {
        let Some(placement) = placement else {
            return Ok(Placed::Admit(TurnAdmission {
                send_now: delivery == Delivery::SendNow,
                ..TurnAdmission::default()
            }));
        };
        if let Some(receipt) = self.await_idle(chat_id, delivery, ledger).await? {
            return Ok(Placed::Known(receipt));
        }
        let branch = match placement {
            Placement::Root => None,
            Placement::After(id) => {
                ledger.display_parent = Some(id);
                self.select_branch(chat_id, id, NodeRole::ContinuationPoint)
                    .await?
            }
            Placement::Replacing(id) => self.select_branch(chat_id, id, NodeRole::Prompt).await?,
        };
        Ok(Placed::Admit(TurnAdmission {
            send_now: false,
            placement: Some(placement),
            branch,
        }))
    }

    pub(crate) fn refuse_when_closed(&self) -> Result<(), AgentError> {
        match self.inner.closed.load(std::sync::atomic::Ordering::SeqCst) {
            true => Err(AgentError::Invalid("agent host has shut down".into())),
            false => Ok(()),
        }
    }

    /// Brings the Chat into this instance when it does not hold it yet; a resident Chat is not
    /// loaded again. With durable wait hosting the load rebuilds its suspended Runs, continues
    /// the inputs a lost host was answering, queues again what was queued and arms its Triggers
    /// again. `due_at` is the time the host's scheduler woke the Chat for, such as the
    /// [`crate::Waits::wake_at`] it was given: across the family, each Trigger fires once for its
    /// latest time due by then, and each Loop whose deadline came by then ends. A load also
    /// takes what is due by this instance's clock, should it be later; a family held already
    /// leaves that to its timers. A time already dealt with does not fire again, so a host
    /// may wake a Chat as often as it likes.
    pub async fn wake(
        &self,
        chat_id: ChatId,
        due_at: Option<std::time::SystemTime>,
    ) -> Result<(), AgentError> {
        let _admission = self.inner.tasks.token();
        self.refuse_when_closed()?;
        let was_resident = self.inner.chats.lock().await.contains_key(&chat_id);
        self.make_resident(chat_id).await?;
        let due_at = due_at
            .and_then(|due_at| due_at.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|due_at| u64::try_from(due_at.as_millis()).unwrap_or(u64::MAX));
        // A family this instance holds already has its timers on this clock; a wake for a
        // time from before only adds what the scheduler found due by then.
        let until = match (due_at, was_resident) {
            (Some(due_at), true) => due_at,
            (due_at, _) => {
                let now = self.inner.schedule_clock.now_ms();
                due_at.map_or(now, |due_at| due_at.max(now))
            }
        };
        self.fire_due_triggers(chat_id, until).await
    }

    /// Loads a saved Chat this instance does not hold yet.
    pub(crate) async fn make_resident(&self, chat_id: ChatId) -> Result<(), AgentError> {
        if self.inner.chats.lock().await.contains_key(&chat_id) {
            return Ok(());
        }
        if self.inner.recorder.is_none() {
            return Err(AgentError::NotFound);
        }
        Box::pin(self.resume_chat(chat_id)).await.map(|_| ())
    }

    /// Waits until the Chat is idle. Under `Queue` a busy Chat refuses the input; under
    /// `SendNow` its running or waiting Run is interrupted first, while work that no
    /// interruption ends still refuses it. Gives the input's receipt instead when the ledger
    /// meanwhile holds it live or settled, such as a concurrent first request's acceptance.
    async fn await_idle(
        &self,
        chat_id: ChatId,
        delivery: Delivery,
        ledger: &LedgerInput,
    ) -> Result<Option<Receipt>, AgentError> {
        let mut events = self.subscribe();
        loop {
            {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                if let Admission::Known(receipt) = classify(chat, ledger)? {
                    return Ok(Some(receipt));
                }
                if rewind::ensure_idle(chat).is_ok() {
                    return Ok(None);
                }
                let has_queued_work = !chat.snapshot.pending_inputs.is_empty()
                    || !chat.snapshot.pending_commands.is_empty()
                    || chat.has_foreground_user_shell();
                let is_interruptible = chat.branch_reservation.is_none()
                    && (chat.active.is_some() || !chat.parked.is_empty());
                if delivery == Delivery::Queue || has_queued_work || !is_interruptible {
                    return Err(chat_is_busy());
                }
                chat.interrupt();
            }
            tokio::select! {
                _ = events.recv() => {}
                _ = tokio::time::sleep(RECHECK) => {}
                _ = self.inner.shutdown.cancelled() => {}
            }
            self.refuse_when_closed()?;
        }
    }

    /// The stored branch that holds the display node `id`, staged when the current chain does
    /// not hold it: the idle Chat is reserved first, everything produced so far is committed,
    /// and the node is checked against `role` before its branch is restored beside the Chat.
    /// An answer without entries of its own stands where its prompt does.
    async fn select_branch(
        &self,
        chat_id: ChatId,
        id: MessageId,
        role: NodeRole,
    ) -> Result<Option<BranchSwitch>, AgentError> {
        if self.on_current_chain(chat_id, id).await? {
            return Ok(None);
        }
        let reservation = self.reserve_branch(chat_id).await?;
        match self.stage_selected_branch(chat_id, id, role).await {
            Ok(Some(staged)) => Ok(Some(BranchSwitch {
                reservation,
                staged,
            })),
            Ok(None) => {
                reservation.release().await;
                Ok(None)
            }
            Err(error) => {
                reservation.release().await;
                Err(error)
            }
        }
    }

    /// Under a reservation: commits everything produced so far, checks the node `id` against
    /// `role` in the store, and restores the branch that ends at it. `None` when it turns out to
    /// stand on the current chain (an answer without entries whose prompt is there).
    async fn stage_selected_branch(
        &self,
        chat_id: ChatId,
        id: MessageId,
        role: NodeRole,
    ) -> Result<Option<Box<crate::persistence::StagedBranch>>, AgentError> {
        self.sync().await?;
        let lines = self.read_stored_lines(chat_id).await?;
        let nodes = project(&lines);
        let mut target = id;
        let mut role = Some(role);
        let leaf = loop {
            let node = nodes
                .iter()
                .find(|node| node.id == target)
                .ok_or_else(unknown_node)?;
            if let Some(role) = role.take() {
                role.admits(node, &nodes)?;
            }
            if let Some(leaf) = stored_endpoint(&lines, target) {
                break leaf;
            }
            match (&node.kind, node.parent) {
                (DisplayKind::Answer { .. }, Some(prompt)) => {
                    if self.on_current_chain(chat_id, prompt).await? {
                        return Ok(None);
                    }
                    target = prompt;
                }
                _ => return Err(unknown_node()),
            }
        };
        Ok(Some(Box::new(self.stage_branch(chat_id, leaf).await?)))
    }

    async fn on_current_chain(&self, chat_id: ChatId, id: MessageId) -> Result<bool, AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        Ok(chain_nodes(chat).iter().any(|node| node.id == id))
    }

    /// Reserves the idle Chat for replacing its conversation with another stored branch: it
    /// takes no input and starts nothing until the reservation is used or released.
    async fn reserve_branch(&self, chat_id: ChatId) -> Result<BranchReservation, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        rewind::ensure_idle(chat).map_err(|_| chat_is_busy())?;
        if chat.snapshot.delegation.is_some() {
            return Err(AgentError::Invalid(
                "a Subagent's conversation cannot switch branches".into(),
            ));
        }
        if !chat.trigger_cancellations.is_empty() || !chat.event_cancellations.is_empty() {
            return Err(AgentError::Invalid(
                "a Loop of this Chat waits for its Trigger; stop it before switching branches"
                    .into(),
            ));
        }
        let id = Uuid::new_v4();
        chat.branch_reservation = Some(id);
        chat.active = Some(tokio_util::sync::CancellationToken::new());
        Ok(BranchReservation {
            agent: WeakAgent {
                inner: std::sync::Arc::downgrade(&self.inner),
            },
            chat_id,
            id,
            held: true,
        })
    }

    /// Ends the reservation `id` of the Chat, if it still holds it.
    async fn release_branch(&self, chat_id: ChatId, id: Uuid) {
        let next = {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&chat_id) else {
                return;
            };
            if chat.branch_reservation != Some(id) {
                return;
            }
            chat.branch_reservation = None;
            chat.active = None;
            drop(chats);
            self.next_run(chat_id).await
        };
        if let Ok(Some(next)) = next {
            self.run_task(next);
        }
    }

    /// Puts a staged branch in place of the Chat's conversation, under the reservation its
    /// switch held: the recorder follows the branch from now on. Gives the branch's leaf.
    pub(crate) fn install_branch(&self, chat: &mut Chat, staged: Box<StagedInstall>) -> Uuid {
        let StagedInstall {
            chat: branch,
            log,
            leaf,
            mut reservation,
        } = *staged;
        if let Some(recorder) = &self.inner.recorder {
            recorder.adopt_branch(chat.snapshot.id, log);
        }
        *chat = branch;
        reservation.held = false;
        leaf
    }

    /// Builds `candidate` on the conversation cut at `cut`, a copy of `original`'s; gives the
    /// Loops that existed before, to tell the ones the cut removed.
    pub(crate) fn prepare_cut(
        original: &Chat,
        candidate: &mut Chat,
        cut: Option<(usize, Option<ExecutionMode>)>,
    ) -> Result<HashSet<LoopId>, AgentError> {
        let loops_at_start = original
            .snapshot
            .loops
            .iter()
            .map(|goal| goal.id)
            .collect::<HashSet<_>>();
        if let Some((index, mode)) = cut {
            (candidate.snapshot, candidate.bookkeeping) = rewind::conversation_before(
                &original.snapshot,
                &original.bookkeeping,
                index,
                mode,
                &loops_at_start,
            )?;
        }
        Ok(loops_at_start)
    }

    /// Commits the cut at message `index` that `candidate` was built on: the stored chain forks
    /// there, and the views redraw.
    pub(crate) fn commit_cut(&self, original: &mut Chat, candidate: &mut Chat, index: usize) {
        let first_removed = first_removed_message(original, index);
        self.replace_conversation(
            original,
            index,
            first_removed,
            (candidate.snapshot.clone(), candidate.bookkeeping.clone()),
            Uuid::nil(),
        );
        candidate.snapshot = original.snapshot.clone();
        candidate.bookkeeping = original.bookkeeping.clone();
        candidate.needs_plan_mode_exit_notice = original.needs_plan_mode_exit_notice;
    }

    /// Cuts the conversation before message `index`; gives the Loops the cut removed.
    fn cut(
        &self,
        chat: &mut Chat,
        index: usize,
        mode: Option<ExecutionMode>,
    ) -> Result<Vec<LoopId>, AgentError> {
        let loops_at_start = chat
            .snapshot
            .loops
            .iter()
            .map(|goal| goal.id)
            .collect::<HashSet<_>>();
        let candidate = rewind::conversation_before(
            &chat.snapshot,
            &chat.bookkeeping,
            index,
            mode,
            &loops_at_start,
        )?;
        let first_removed = first_removed_message(chat, index);
        self.replace_conversation(chat, index, first_removed, candidate, Uuid::nil());
        Ok(rewind::stop_listening_to_removed_loops(
            chat,
            loops_at_start,
        ))
    }

    pub(crate) async fn retire_cut_loops(&self, chat_id: ChatId, removed_loops: Vec<LoopId>) {
        if removed_loops.is_empty() {
            return;
        }
        let Some(tools) = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .map(|chat| chat.tools.clone())
        else {
            return;
        };
        self.retire_removed_loops(chat_id, removed_loops, &tools)
            .await;
    }

    /// Another answer to the prompt of `answer`: the conversation is cut after that prompt and
    /// a Run answers it again in the answer's Loop. A prompt off the current chain is reached by
    /// its staged branch, installed together with the acceptance.
    async fn regenerate(
        &self,
        chat_id: ChatId,
        answer: MessageId,
        mut ledger: LedgerInput,
        delivery: Delivery,
    ) -> Result<Receipt, AgentError> {
        if let Some(receipt) = self.await_idle(chat_id, delivery, &ledger).await? {
            return Ok(receipt);
        }
        let prompt = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            answered_prompt(chat, answer)?
        };
        let switch = self
            .select_branch(chat_id, prompt, NodeRole::Prompt)
            .await?;
        let reservation = switch.as_ref().map(BranchSwitch::reservation);
        ledger.display_parent = Some(prompt);
        let accepted = self
            .accept_regeneration(chat_id, answer, prompt, ledger, switch)
            .await;
        if accepted.is_err()
            && let Some(id) = reservation
        {
            self.release_branch(chat_id, id).await;
        }
        accepted
    }

    async fn accept_regeneration(
        &self,
        chat_id: ChatId,
        answer: MessageId,
        prompt: MessageId,
        ledger: LedgerInput,
        switch: Option<BranchSwitch>,
    ) -> Result<Receipt, AgentError> {
        let _admission = self.inner.tasks.token();
        let (receipt, removed_loops) = {
            let mut chats = self.inner.chats.lock().await;
            self.refuse_when_closed()?;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            if let Some(replay) = crate::lifecycle::replay(chat, &ledger) {
                return replay;
            }
            let staged = match (switch, chat.branch_reservation) {
                (Some(switch), Some(reserved)) if switch.reservation() == reserved => {
                    Some(switch.into_chat(chat))
                }
                (None, None) => None,
                _ => return Err(chat_is_busy()),
            };
            let base = staged.as_ref().map_or(&*chat, |staged| &staged.chat);
            rewind::ensure_idle(base).map_err(|_| chat_is_busy())?;
            let loop_id = answered_prompt(base, answer).and_then(|_| {
                base.bookkeeping
                    .lifecycle
                    .response(answer)
                    .map(|response| response.loop_id)
                    .ok_or_else(unknown_node)
            })?;
            let node = chain_node(base, &chain_nodes(base), prompt)
                .filter(|node| node.is_prompt)
                .ok_or_else(unknown_node)?;
            let index = node.last + 1;
            let loop_survives_cut = base
                .bookkeeping
                .loop_starts
                .get(&loop_id)
                .is_none_or(|start| *start < index);
            if !loop_survives_cut {
                return Err(AgentError::Invalid(
                    "the answer's Loop is no longer part of the conversation".into(),
                ));
            }
            admit_answer_loop(base, loop_id)?;
            let mut candidate = base.clone();
            let cut = (index < base.snapshot.messages.len()).then_some((index, None));
            let loops_at_start = Self::prepare_cut(base, &mut candidate, cut)?;
            let selected_leaf = staged.map(|staged| self.install_branch(chat, staged));
            if cut.is_some() {
                self.commit_cut(chat, &mut candidate, index);
            }
            *chat = candidate;
            let removed_loops = match cut {
                Some(_) => rewind::stop_listening_to_removed_loops(chat, loops_at_start),
                None => Vec::new(),
            };
            if cut.is_none()
                && let Some(leaf) = selected_leaf
            {
                self.emit_conversation_rewound(chat, Uuid::nil(), leaf);
            }
            let receipt = Receipt {
                chat_id,
                loop_id,
                run_id: Uuid::new_v4(),
            };
            self.queue_answer_run(chat, &receipt, Some(prompt))?;
            let queued = chat
                .snapshot
                .pending_inputs
                .iter()
                .find(|pending| pending.receipt.run_id == receipt.run_id)
                .expect("the answer's Run is queued")
                .clone();
            chat.bookkeeping.lifecycle.accept(&ledger, &queued);
            (receipt, removed_loops)
        };
        self.retire_cut_loops(chat_id, removed_loops).await;
        self.start_next_run(chat_id).await?;
        Ok(receipt)
    }

    /// Another attempt at an input whose answer lost its host, in its own Run: it continues
    /// the conversation where the lost attempt left it, and shows as that attempt's sibling.
    async fn continue_input(
        &self,
        chat_id: ChatId,
        ledger: &LedgerInput,
        entry: InputEntry,
        answer_parent: Option<Uuid>,
        delivery: Delivery,
    ) -> Result<Receipt, AgentError> {
        if let Some(receipt) = self.await_idle(chat_id, delivery, ledger).await? {
            return Ok(receipt);
        }
        let _admission = self.inner.tasks.token();
        let receipt = {
            let mut chats = self.inner.chats.lock().await;
            self.refuse_when_closed()?;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            match classify(chat, ledger)? {
                Admission::Known(receipt) => return Ok(receipt),
                Admission::Continuation { .. } => {}
                Admission::Run { .. } => return Err(moved_on()),
            }
            rewind::ensure_idle(chat).map_err(|_| chat_is_busy())?;
            admit_continuation(chat, &entry, answer_parent)?;
            let receipt = Receipt {
                chat_id,
                loop_id: entry.loop_id,
                run_id: entry.run_id,
            };
            self.queue_answer_run(chat, &receipt, answer_parent)?;
            receipt
        };
        self.start_next_run(chat_id).await?;
        Ok(receipt)
    }

    /// Queues a Run that answers the conversation as it stands, without a prompt of its own. A
    /// sentinel recovery appended after a trailing user message goes first, so the Run answers
    /// that message.
    pub(crate) fn queue_answer_run(
        &self,
        chat: &mut Chat,
        receipt: &Receipt,
        answer_parent: Option<Uuid>,
    ) -> Result<(), AgentError> {
        let last = chat.snapshot.messages.len().checked_sub(1);
        if let Some(last) = last
            && crate::persistence::is_recovery_sentinel(&chat.snapshot.messages[last])
            && chat
                .bookkeeping
                .lifecycle
                .response_of(&chat.snapshot, &chat.bookkeeping.message_responses, last)
                .is_none()
        {
            self.cut(chat, last, None)?;
        }
        let goal = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == receipt.loop_id)
            .ok_or_else(|| {
                AgentError::Invalid(
                    "the answer's Loop is no longer part of the conversation".into(),
                )
            })?;
        goal.set_state(LoopState::Queued);
        if let Some(parent) = answer_parent {
            chat.bookkeeping
                .lifecycle
                .answer_under(receipt.run_id, parent);
        }
        crate::queue_continuation(chat, receipt.clone());
        self.save(&chat.snapshot);
        Ok(())
    }

    async fn start_next_run(&self, chat_id: ChatId) -> Result<(), AgentError> {
        let next = self.next_run(chat_id).await?;
        #[cfg(test)]
        admission_pause::wait_if_paused(chat_id).await;
        if let Some(next) = next {
            self.run_task(next);
        }
        self.inner.ledger_changes.send_replace(());
        Ok(())
    }

    async fn turn_outcome(
        &self,
        chat_id: ChatId,
        input: InputId,
    ) -> Result<TurnOutcome, AgentError> {
        let mut changes = self.inner.ledger_changes.subscribe();
        let mut events = self.subscribe();
        loop {
            changes.mark_unchanged();
            if self.transcripts_fenced() {
                return Ok(TurnOutcome::Fenced);
            }
            let reached = {
                let chats = self.inner.chats.lock().await;
                let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
                let state = ledger_state(chat, input).ok_or(AgentError::NotFound)?;
                let run = state.entry.run_id;
                let uncertain = || uncertain_operations(chat, run);
                // A host that shut down runs nothing more: what it still queued is lost with it.
                let is_live = state.is_live
                    && (!self.inner.closed.load(std::sync::atomic::Ordering::SeqCst)
                        || chat.active_run == Some(run)
                        || chat.parked.contains_key(&run)
                        || chat.bookkeeping.lifecycle.active_response(run).is_some());
                match state.entry.state {
                    InputState::Settled { outcome } => Some(match outcome {
                        SettledOutcome::Completed { usage } => TurnOutcome::Completed { usage },
                        SettledOutcome::Interrupted { by } => TurnOutcome::Interrupted {
                            by,
                            uncertain_operations: uncertain(),
                        },
                        SettledOutcome::Failed { error } => TurnOutcome::Failed { error },
                        SettledOutcome::Merged { into } => TurnOutcome::Merged { into },
                        SettledOutcome::Withdrawn => TurnOutcome::Withdrawn,
                        SettledOutcome::Discarded { reason } => TurnOutcome::Discarded { reason },
                    }),
                    InputState::Accepted | InputState::Started if is_live => {
                        waiting_for_person(&chats, chat, run).map(TurnOutcome::Waiting)
                    }
                    InputState::Accepted | InputState::Started => Some(TurnOutcome::Interrupted {
                        by: InterruptedBy::Host,
                        uncertain_operations: uncertain(),
                    }),
                }
            };
            if let Some(outcome) = reached {
                return Ok(outcome);
            }
            tokio::select! {
                _ = changes.changed() => {}
                _ = events.recv() => {}
                _ = tokio::time::sleep(OUTCOME_RECHECK) => {}
            }
        }
    }
}

fn classify(chat: &Chat, ledger: &LedgerInput) -> Result<Admission, AgentError> {
    let Some(state) = ledger_state(chat, ledger.id) else {
        return Ok(Admission::Run { replaces: None });
    };
    let entry = state.entry;
    if entry.digest != ledger.digest || entry.op != ledger.op {
        return Err(AgentError::Invalid(
            "input id reused with a different payload".into(),
        ));
    }
    let receipt = Receipt {
        chat_id: chat.snapshot.id,
        loop_id: entry.loop_id,
        run_id: entry.run_id,
    };
    if matches!(entry.state, InputState::Settled { .. }) || state.is_live {
        return Ok(Admission::Known(receipt));
    }
    Ok(match state.responses.last() {
        Some(last)
            if last.state
                == (ResponseState::Ended {
                    ending: ResponseEnding::Interrupted {
                        by: InterruptedBy::Host,
                    },
                }) =>
        {
            Admission::Continuation {
                answer_parent: last.display_parent,
                entry: Box::new(entry),
            }
        }
        Some(_) => Admission::Known(receipt),
        None if entry.op != InputOp::RegenerateAnswer && state.prompt_in_conversation => {
            Admission::Continuation {
                answer_parent: Some(entry.input_id),
                entry: Box::new(entry),
            }
        }
        None => Admission::Run {
            replaces: Some(entry.run_id),
        },
    })
}

fn message_submission(
    chat_id: ChatId,
    goal: Option<LoopId>,
    message: UserMessage,
    host_context: Option<HostCallContext>,
    ledger: LedgerInput,
    turn: TurnAdmission,
) -> crate::family::ContextSubmission {
    crate::family::ContextSubmission {
        chat_id,
        goal,
        input: message.text,
        contexts: message.contexts,
        paths: message.paths,
        host_context,
        directed_source: None,
        target_generation: None,
        ledger: Some(ledger),
        turn,
    }
}

fn placement(after: ParentSelection) -> Option<Placement> {
    match after {
        ParentSelection::CurrentLeaf => None,
        ParentSelection::Root => Some(Placement::Root),
        ParentSelection::Message(id) => Some(Placement::After(id)),
    }
}

fn first_removed_message(chat: &Chat, index: usize) -> Uuid {
    chat.snapshot
        .original_message_ids
        .get(&index)
        .or_else(|| chat.snapshot.message_ids.get(&index))
        .copied()
        .unwrap_or_else(Uuid::nil)
}

fn moved_on() -> AgentError {
    AgentError::Invalid("the conversation has moved on since this input lost its answer".into())
}

fn answer_not_ended() -> AgentError {
    AgentError::Invalid("the answer has not ended".into())
}

fn prompt_has_answer() -> AgentError {
    AgentError::Invalid("this prompt has an answer; continue after the answer".into())
}

fn only_prompts_are_replaced() -> AgentError {
    AgentError::Invalid("only a top-level prompt can be replaced".into())
}

fn unknown_node() -> AgentError {
    AgentError::Invalid("this message is not part of the Chat".into())
}

/// The display nodes of the Chat's current chain, in order.
fn chain_nodes(chat: &Chat) -> Vec<ChainNode> {
    let snapshot = &chat.snapshot;
    let bookkeeping = &chat.bookkeeping;
    let mut nodes: Vec<ChainNode> = Vec::new();
    for index in 0..snapshot.messages.len() {
        let (id, is_prompt) =
            match bookkeeping
                .lifecycle
                .response_of(snapshot, &bookkeeping.message_responses, index)
            {
                Some(response) => (response, false),
                None if snapshot.prompt_execution_modes.contains_key(&index) => {
                    match snapshot
                        .original_message_ids
                        .get(&index)
                        .or_else(|| snapshot.message_ids.get(&index))
                    {
                        Some(id) => (*id, true),
                        None => continue,
                    }
                }
                None => continue,
            };
        match nodes.last_mut() {
            Some(node) if node.id == id => node.last = index,
            _ => nodes.push(ChainNode {
                id,
                is_prompt,
                first: index,
                last: index,
            }),
        }
    }
    nodes
}

/// The current chain's node `id` among its `nodes`. An answer without messages of its own on
/// the chain stands where its prompt does.
fn chain_node(chat: &Chat, nodes: &[ChainNode], id: MessageId) -> Option<ChainNode> {
    nodes
        .iter()
        .rev()
        .find(|node| node.id == id)
        .copied()
        .or_else(|| {
            let parent = chat.bookkeeping.lifecycle.response(id)?.display_parent?;
            nodes.iter().rev().find(|node| node.id == parent).copied()
        })
}

/// Where the conversation is cut for an explicitly selected input, with the execution mode it
/// returns to; `None` when it already ends at the selected node.
pub(crate) fn placement_cut(
    chat: &Chat,
    placement: Placement,
) -> Result<Option<(usize, Option<ExecutionMode>)>, AgentError> {
    let messages = chat.snapshot.messages.len();
    match placement {
        Placement::Root => Ok((messages > 0).then_some((0, None))),
        Placement::After(id) => {
            let lifecycle = &chat.bookkeeping.lifecycle;
            match lifecycle.response(id) {
                Some(response) if !response.state.is_ended() => {
                    return Err(answer_not_ended());
                }
                Some(_) => {}
                None if lifecycle.has_answer_under(id) => return Err(prompt_has_answer()),
                None => {}
            }
            let nodes = chain_nodes(chat);
            let node = chain_node(chat, &nodes, id)
                .filter(|node| node.is_prompt || lifecycle.response(id).is_some())
                .ok_or_else(unknown_node)?;
            let is_tail = nodes.last().is_some_and(|tail| tail.first == node.first);
            Ok((!is_tail).then_some((node.last + 1, None)))
        }
        Placement::Replacing(id) => {
            let node = chain_node(chat, &chain_nodes(chat), id)
                .filter(|node| node.is_prompt && node.id == id)
                .ok_or_else(only_prompts_are_replaced)?;
            let mode = chat
                .snapshot
                .prompt_execution_modes
                .get(&node.first)
                .copied();
            Ok(Some((node.first, mode)))
        }
    }
}

/// The prompt an ended answer answered.
fn answered_prompt(chat: &Chat, answer: MessageId) -> Result<MessageId, AgentError> {
    let lifecycle = &chat.bookkeeping.lifecycle;
    let response = lifecycle.response(answer).ok_or_else(unknown_node)?;
    if !response.state.is_ended() {
        return Err(answer_not_ended());
    }
    response
        .display_parent
        .filter(|parent| lifecycle.response(*parent).is_none())
        .ok_or_else(|| AgentError::Invalid("only an answer to a prompt can be regenerated".into()))
}

/// The answer's Loop may take another Run: it has ended its last one without ending itself.
/// Another attempt may continue `entry`: the conversation still ends at it, its prompt or one of
/// its answers, and its Loop may take another Run.
pub(crate) fn admit_continuation(
    chat: &Chat,
    entry: &InputEntry,
    answer_parent: Option<Uuid>,
) -> Result<(), AgentError> {
    let bookkeeping = &chat.bookkeeping;
    let tail = bookkeeping
        .lifecycle
        .latest_display_node(&chat.snapshot, &bookkeeping.message_responses);
    let attempts = bookkeeping.lifecycle.responses_of(entry.input_id);
    let is_at_tail = tail.is_some_and(|tail| {
        tail == entry.input_id
            || Some(tail) == answer_parent
            || attempts.iter().any(|attempt| attempt.response_id == tail)
    });
    if !is_at_tail && !ends_the_conversation(chat, entry) {
        return Err(moved_on());
    }
    admit_answer_loop(chat, entry.loop_id)
}

/// An Agent message, a Trigger's prompt or an @tickloop request is no display node of its own:
/// its answer is where the conversation ends when nothing but that answer's Run, and what a load
/// added, follows the message. A Trigger's prompt and a request are found as their Run's first
/// message.
fn ends_the_conversation(chat: &Chat, entry: &InputEntry) -> bool {
    let snapshot = &chat.snapshot;
    let start = match entry.op {
        InputOp::AgentMessage => snapshot
            .message_ids
            .iter()
            .filter(|(_, id)| **id == entry.input_id)
            .map(|(index, _)| *index)
            .max(),
        InputOp::Trigger | InputOp::Feedback => (0..snapshot.messages.len())
            .find(|index| snapshot.message_runs.get(index) == Some(&entry.run_id)),
        _ => None,
    };
    let Some(start) = start else {
        return false;
    };
    (start + 1..snapshot.messages.len()).all(|index| {
        snapshot
            .message_runs
            .get(&index)
            .is_none_or(|run| *run == entry.run_id)
    })
}

fn admit_answer_loop(chat: &Chat, loop_id: LoopId) -> Result<(), AgentError> {
    let goal = chat
        .snapshot
        .loops
        .iter()
        .find(|goal| goal.id == loop_id)
        .ok_or_else(|| {
            AgentError::Invalid("the answer's Loop is no longer part of the conversation".into())
        })?;
    match goal.state {
        LoopState::Idle | LoopState::Completed | LoopState::Interrupted | LoopState::Failed => {
            Ok(())
        }
        LoopState::Stopped | LoopState::Expired => Err(AgentError::Invalid(
            "goal has ended; create a new goal".into(),
        )),
        _ => Err(AgentError::Invalid(
            "the answer's Loop is waiting; finish or stop it first".into(),
        )),
    }
}

/// The physical entry the display node `id` ends at among every stored Line: a prompt's latest
/// copy, or an answer's last entry.
fn stored_endpoint(lines: &[Line], id: MessageId) -> Option<Uuid> {
    let entries = || lines.iter().filter_map(Line::chain_entry);
    let answer_end = entries()
        .rev()
        .find(|entry| entry.response_id == Some(id))
        .map(|entry| entry.uuid);
    let prompt_end = || {
        entries()
            .rev()
            .find(|entry| {
                entry.response_id.is_none()
                    && entry.execution_mode.is_some()
                    && !entry.continues_message
                    && (entry.uuid == id || entry.original_message_id == Some(id))
            })
            .map(|entry| entry.uuid)
    };
    answer_end.or_else(prompt_end)
}

fn uncertain_operations(chat: &Chat, run: RunId) -> Vec<OperationRecord> {
    chat.snapshot
        .operations
        .iter()
        .filter(|operation| {
            operation.run_id == run
                && matches!(
                    operation.state,
                    OperationState::Admitted | OperationState::Unknown
                )
        })
        .cloned()
        .collect()
}

/// What the Run waits for when it is parked on a person.
fn waiting_for_person(chats: &HashMap<ChatId, Chat>, chat: &Chat, run: RunId) -> Option<TurnWait> {
    if chat.active_run == Some(run) {
        return waiting_member(chats, chat, run);
    }
    if !chat.parked.contains_key(&run) {
        return None;
    }
    let snapshot = &chat.snapshot;
    snapshot
        .permissions
        .iter()
        .find(|request| request.execution.run_id() == Some(run))
        .cloned()
        .map(TurnWait::Permission)
        .or_else(|| {
            snapshot
                .questions
                .iter()
                .find(|question| question.run_id == run)
                .cloned()
                .map(TurnWait::Question)
        })
}

/// The person a Run holding its slot waits for through the members it waits on: the child of
/// its foreground call, or each child it waits to report.
fn waiting_member(chats: &HashMap<ChatId, Chat>, chat: &Chat, run: RunId) -> Option<TurnWait> {
    let wait = chat.bookkeeping.lifecycle.open_wait_of(run, |kind| {
        matches!(
            kind,
            tickloop_agent_transcript::WaitKind::Subagent { .. }
                | tickloop_agent_transcript::WaitKind::Children { .. }
        )
    })?;
    let runs = match &wait.kind {
        tickloop_agent_transcript::WaitKind::Subagent {
            child_chat,
            child_run,
            ..
        } => vec![(*child_chat, *child_run)],
        tickloop_agent_transcript::WaitKind::Children { children } => children
            .iter()
            .filter_map(|child| chats.get(child))
            .flat_map(|child| {
                child
                    .snapshot
                    .loops
                    .iter()
                    .filter_map(|goal| Some((child.snapshot.id, goal.run_id?)))
            })
            .collect(),
        tickloop_agent_transcript::WaitKind::Question { .. }
        | tickloop_agent_transcript::WaitKind::Handover => Vec::new(),
    };
    runs.into_iter().find_map(|(member, run)| {
        let wait = waiting_for_person(chats, chats.get(&member)?, run)?;
        Some(TurnWait::Member {
            chat: member,
            wait: Box::new(wait),
        })
    })
}

/// Test seam: holds an admitted submission of a Chat after its Run was selected and before its
/// task is registered, until the test releases it.
#[cfg(test)]
pub(crate) mod admission_pause {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use tickloop_agent_protocol::ChatId;
    use tokio::sync::oneshot;

    type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);

    static GATES: Mutex<Option<HashMap<ChatId, Gate>>> = Mutex::new(None);

    /// Pauses the Chat's next admitted submission; dropping `release` lets it go on.
    pub(crate) struct Pause {
        pub(crate) entered: oneshot::Receiver<()>,
        pub(crate) release: oneshot::Sender<()>,
    }

    pub(crate) fn pause_next(chat: ChatId) -> Pause {
        let (entered_sender, entered) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        GATES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(chat, (entered_sender, release_receiver));
        Pause { entered, release }
    }

    pub(crate) async fn wait_if_paused(chat: ChatId) {
        let gate = GATES
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|gates| gates.remove(&chat));
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
}
