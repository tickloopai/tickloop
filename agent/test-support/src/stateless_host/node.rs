//! A reference Harness Node: one holding of a family's lease claims the family's tasks for as
//! long as the family runs, and acknowledges each task on its own condition.
//!
//! 1. A turn task (a user's message, an answer, a delivered request) is acknowledged as soon as
//!    its input settled or waits for a person, or for an answer or a delivery once it was
//!    accepted; `sync` succeeded first, and every event through the commit it reflects is
//!    published with no marker owed.
//! 2. `takeover` and `handover` tasks load the family with `resume_chat(root)` and are
//!    acknowledged when the holding ends. Every heartbeat renews the lease and extends the claim
//!    of every task the holding has not acknowledged.
//! 3. The holding ends once the family is quiet and no task is queued: `shutdown → sync →
//!    drain`, then the remaining acknowledgements, what the family waits on, and the release.
//! 4. At the holding-time cap or on SIGTERM the holding hands the family over:
//!    `hand_over(ceiling)`, then `handover:{root}:{epoch}`; a turn task whose input neither
//!    settled nor waits is given back without counting the delivery, and the handover task goes
//!    on with it.
//! 5. A holder that crashes, panics included, writes, acknowledges and releases nothing more;
//!    the scheduler's sweep takes the family over once the lease expired.

use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use tickloop_agent_runtime::{HandOver, Turn, TurnOp};
use tickloop_agent_transcript::InputState;

use super::*;

/// How often a holding looks for acknowledgements it can make when nothing else happens.
const ACKNOWLEDGEMENT_RECHECK: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug)]
pub struct NodeConfig {
    /// The longest a holding lasts, on the host's clock, before it hands the family over.
    pub holding_cap: Duration,
    /// How long, in real time, a handover lets the family's Runs reach a safe point.
    pub handover_ceiling: Duration,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            holding_cap: Duration::from_secs(30 * 60),
            handover_ceiling: Duration::from_secs(240),
        }
    }
}

/// How a holding of the family ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldingEnd {
    /// The family was quiet with no task queued; the lease was released.
    Quiet,
    /// The holding handed the family over and released the lease.
    HandedOver(HandOver),
    /// The holder stopped before it could finish, or failed to: see `Halt`.
    Halted(Halt),
}

/// What one holding of a family did.
#[derive(Clone, Debug)]
pub struct HoldingReport {
    pub epoch: u64,
    pub end: HoldingEnd,
    /// The tasks it claimed, in order.
    pub claimed: Vec<Uuid>,
    pub acknowledged: Vec<Uuid>,
    /// The tasks it gave back without counting their delivery.
    pub returned: Vec<Uuid>,
    /// The turn tasks the Harness refused for good, with why; they are acknowledged.
    pub refused: Vec<(Uuid, String)>,
    /// What the family waited on when the holding ended quietly.
    pub waiting: Option<Waits>,
}

/// A node of reference hosts: each holding of a family runs there on a fresh Harness.
pub struct ReferenceNode {
    host: ReferenceHost,
    config: NodeConfig,
    sigterm: CancellationToken,
    holdings: Mutex<HashMap<Uuid, Arc<Shared>>>,
}

impl ReferenceNode {
    pub fn new(host: ReferenceHost, config: NodeConfig) -> Self {
        Self {
            host,
            config,
            sigterm: CancellationToken::new(),
            holdings: Mutex::default(),
        }
    }

    /// One holding of the family of the root Chat `root`, until it ends.
    pub async fn hold(&self, root: Uuid) -> Result<HoldingReport, LeaseError> {
        let holding = self.host.acquire(root)?;
        self.holdings
            .lock()
            .unwrap()
            .insert(root, holding.shared.clone());
        let report = holding.hold_family(self.config, &self.sigterm).await;
        self.holdings.lock().unwrap().remove(&root);
        Ok(report)
    }

    /// The node is asked to stop: every holding hands its family over.
    pub fn sigterm(&self) {
        self.sigterm.cancel();
    }

    /// Whether the node's holding of the family began handing it over.
    pub fn is_handing_over(&self, root: Uuid) -> bool {
        self.holdings
            .lock()
            .unwrap()
            .get(&root)
            .is_some_and(|shared| shared.state.lock().unwrap().handing_over)
    }

    /// The holder of the family dies where it is. `false` when the node holds no such family.
    pub fn crash(&self, root: Uuid) -> bool {
        let holding = self.holdings.lock().unwrap().get(&root).cloned();
        holding.is_some_and(|shared| {
            shared.halt_or(Halt::Crashed);
            true
        })
    }
}

impl DurableChats {
    /// Whether a task of the family waits to be claimed.
    fn has_queued_tasks(&self, root: Uuid) -> bool {
        self.rows.lock().unwrap().get(&root).is_some_and(|row| {
            row.tasks
                .iter()
                .any(|task| task.view.state == TaskState::Queued)
        })
    }
}

/// A task the holding claimed and has not acknowledged or given back.
struct ClaimedTask {
    id: Uuid,
    work: ClaimedWork,
}

enum ClaimedWork {
    /// A turn of the family member `target`; an answer or a delivery is done once accepted.
    Turn {
        target: ChatId,
        input: Uuid,
        is_done_once_accepted: bool,
        turn: Box<Turn>,
    },
    /// The Harness refused the task for good, as for an answer to a question no longer asked.
    Refused(String),
    /// Acknowledged when the holding ends.
    AtHoldingEnd,
}

/// How the family's part of a holding ended, before the Harness shut down.
enum FamilyEnd {
    Quiet(Option<Waits>),
    HandedOver(HandOver),
}

impl ClaimedTask {
    /// Whether the task may be acknowledged now, its events published aside.
    async fn is_done(&self, agent: &Agent) -> bool {
        match &self.work {
            ClaimedWork::Refused(_) | ClaimedWork::AtHoldingEnd => true,
            ClaimedWork::Turn {
                target,
                input,
                is_done_once_accepted,
                turn,
            } => {
                let Ok(Some(state)) = agent.input_state(*target, *input).await else {
                    return false;
                };
                if *is_done_once_accepted || matches!(state.entry.state, InputState::Settled { .. })
                {
                    return true;
                }
                // The outcome resolves at once when the turn waits for a person.
                matches!(
                    tokio::time::timeout(Duration::ZERO, turn.outcome()).await,
                    Ok(Ok(TurnOutcome::Waiting(_)))
                )
            }
        }
    }

    fn is_turn(&self) -> bool {
        !matches!(self.work, ClaimedWork::AtHoldingEnd)
    }
}

impl Holding {
    /// One holding of the family: its tasks until it is quiet, the cap passes or the node is
    /// asked to stop.
    async fn hold_family(&self, config: NodeConfig, sigterm: &CancellationToken) -> HoldingReport {
        let lease = self.shared.lease.clone();
        let mut report = HoldingReport {
            epoch: lease.epoch,
            end: HoldingEnd::Quiet,
            claimed: Vec::new(),
            acknowledged: Vec::new(),
            returned: Vec::new(),
            refused: Vec::new(),
            waiting: None,
        };
        if let Err(halt) = self.take_over() {
            report.end = HoldingEnd::Halted(halt);
            return report;
        }
        let agent = (self.build)(self.store.clone());
        let stop = CancellationToken::new();
        let beats = tokio::spawn(heartbeat(self.shared.clone(), self.config, stop.clone()));
        let mut pump = None;
        let mut tasks = Vec::new();
        let ran = match self.family_root(&agent, &stop, &mut pump).await {
            Ok(root) => {
                self.run_family(&agent, root, config, sigterm, &mut tasks, &mut report)
                    .await
            }
            Err(halt) => Err(halt),
        };
        // Decided while the Harness still holds the family: a turn whose input neither settled
        // nor waits goes back to the queue.
        let mut finished = Vec::new();
        let mut unfinished = Vec::new();
        if ran.is_ok() {
            for task in tasks {
                if task.is_done(&agent).await {
                    finished.push(task.id);
                } else {
                    unfinished.push(task.id);
                }
            }
        }
        agent.shutdown().await;
        let synced = match ran {
            Ok(end) => match self.until_halted(agent.sync()).await {
                Ok(Ok(_)) => Ok(end),
                Ok(Err(error)) => Err(self.agent_failure(error)),
                Err(halt) => Err(halt),
            },
            Err(halt) => Err(halt),
        };
        drop(agent);
        let drained = match synced {
            Ok(end) => {
                self.shared.seal_all();
                self.drain().await.map(|()| end)
            }
            Err(halt) => Err(halt),
        };
        let finished = drained.and_then(|end| {
            self.shared.check_deadline()?;
            for task in &finished {
                self.acknowledge_claim(*task, &mut report)?;
            }
            for task in &unfinished {
                self.shared.chats.return_task(&lease, *task);
                self.shared.claims.lock().unwrap().remove(task);
                report.returned.push(*task);
            }
            Ok(end)
        });
        stop.cancel();
        beats.await.unwrap();
        if let Some(pump) = pump {
            pump.await.unwrap();
        }
        let owes_marker = self.shared.state.lock().unwrap().needs_resync;
        match finished {
            Ok(FamilyEnd::Quiet(waiting)) => {
                report.waiting = waiting.clone();
                self.shared.chats.finish_holding(&lease, Some(waiting));
            }
            Ok(FamilyEnd::HandedOver(handover)) => {
                report.end = HoldingEnd::HandedOver(handover);
                // The handover task's holding writes what the family waits on next.
                self.shared.chats.finish_holding(&lease, Some(None));
            }
            Err(halt) => {
                if matches!(halt, Halt::DrainTimedOut | Halt::Failed(_)) && !owes_marker {
                    self.shared.chats.release_lease(&lease);
                }
                report.end = HoldingEnd::Halted(halt);
            }
        }
        report
    }

    /// Claims and starts the family's tasks, acknowledges each once it may, and ends when the
    /// family is quiet with nothing queued, or hands it over at the cap or on SIGTERM.
    async fn run_family(
        &self,
        agent: &Agent,
        root: ChatId,
        config: NodeConfig,
        sigterm: &CancellationToken,
        tasks: &mut Vec<ClaimedTask>,
        report: &mut HoldingReport,
    ) -> Result<FamilyEnd, Halt> {
        let lease = self.shared.lease.clone();
        let started = self.shared.clock.now();
        loop {
            while let Some((id, kind)) = self.shared.chats.claim_next(&lease) {
                self.shared.claims.lock().unwrap().insert(id);
                report.claimed.push(id);
                let work = self.start_task(agent, root, kind).await?;
                if let ClaimedWork::Refused(error) = &work {
                    report.refused.push((id, error.clone()));
                }
                tasks.push(ClaimedTask { id, work });
            }
            self.acknowledge_done(agent, tasks, report).await?;
            let is_capped = self.shared.clock.now().saturating_sub(started) >= config.holding_cap;
            if is_capped || sigterm.is_cancelled() {
                self.shared.state.lock().unwrap().handing_over = true;
                let handover = self
                    .until_halted(agent.hand_over(config.handover_ceiling))
                    .await?;
                self.shared.chats.enqueue(
                    lease.chat,
                    &format!("handover:{}:{}", lease.chat, lease.epoch),
                    TaskKind::Handover,
                );
                return Ok(FamilyEnd::HandedOver(handover));
            }
            let queued = self.shared.chats.task_queued.notified();
            tokio::pin!(queued);
            queued.as_mut().enable();
            if self.shared.chats.has_queued_tasks(lease.chat) {
                continue;
            }
            let mut clock = self.shared.clock.changes();
            tokio::select! {
                biased;
                _ = self.shared.halted.cancelled() => return Err(self.shared.stored_halt()),
                _ = sigterm.cancelled() => {}
                _ = queued => {}
                _ = clock.changed() => self.shared.check_deadline()?,
                quiescence = agent.quiesce(root) => {
                    if !self.shared.chats.has_queued_tasks(lease.chat)
                        && self.acknowledge_done(agent, tasks, report).await?
                    {
                        return Ok(FamilyEnd::Quiet(match quiescence {
                            Quiescence::Waiting(waits) => Some(waits),
                            Quiescence::Idle => None,
                        }));
                    }
                    tokio::time::sleep(ACKNOWLEDGEMENT_RECHECK).await;
                }
                _ = tokio::time::sleep(ACKNOWLEDGEMENT_RECHECK) => {}
            }
        }
    }

    /// The family's root Chat, created on the first holding, with its publisher started.
    async fn family_root(
        &self,
        agent: &Agent,
        stop: &CancellationToken,
        pump: &mut Option<JoinHandle<()>>,
    ) -> Result<ChatId, Halt> {
        let root = match self.shared.chats.harness_chat(self.shared.lease.chat) {
            Some(chat) => chat,
            None => self
                .guarded(agent.create_chat_without_folder())
                .await?
                .map_err(|error| self.agent_failure(error))?,
        };
        self.shared.state.lock().unwrap().harness_chat = Some(root);
        self.shared
            .chats
            .bind_root(&self.shared.lease, root)
            .map_err(|_| self.shared.halt_or(Halt::Fenced))?;
        *pump = Some(tokio::spawn(publish_events(
            self.shared.clone(),
            agent.subscribe(),
            stop.clone(),
            self.config.retry_delay,
        )));
        Ok(root)
    }

    async fn start_task(
        &self,
        agent: &Agent,
        root: ChatId,
        kind: TaskKind,
    ) -> Result<ClaimedWork, Halt> {
        match kind {
            TaskKind::Turn { member, request } => {
                let request = *request;
                let target = member.unwrap_or(root);
                let input = request.input_id;
                let is_done_once_accepted =
                    matches!(request.op, TurnOp::Answer { .. } | TurnOp::Deliver { .. });
                match self.guarded(agent.advance(target, request)).await? {
                    Ok(turn) => Ok(ClaimedWork::Turn {
                        target,
                        input,
                        is_done_once_accepted,
                        turn: Box::new(turn),
                    }),
                    Err(AgentError::Fenced) => Err(self.shared.halt_or(Halt::Fenced)),
                    // A request the Harness refuses for what it asks, such as an answer to a
                    // question no longer asked, never runs; any other failure leaves the task
                    // to be delivered again, counted against its attempts.
                    Err(error @ AgentError::Invalid(_)) => {
                        Ok(ClaimedWork::Refused(error.to_string()))
                    }
                    Err(error) => Err(self.agent_failure(error)),
                }
            }
            TaskKind::Takeover | TaskKind::Handover => {
                self.guarded(agent.resume_chat(root))
                    .await?
                    .map_err(|error| self.agent_failure(error))?;
                Ok(ClaimedWork::AtHoldingEnd)
            }
        }
    }

    /// Acknowledges every turn task that is done, after `sync` and once everything through the
    /// commit it reflects is published. Whether every claimed turn task is acknowledged now.
    async fn acknowledge_done(
        &self,
        agent: &Agent,
        tasks: &mut Vec<ClaimedTask>,
        report: &mut HoldingReport,
    ) -> Result<bool, Halt> {
        let mut done = Vec::new();
        for task in tasks.iter().filter(|task| task.is_turn()) {
            if task.is_done(agent).await {
                done.push(task.id);
            }
        }
        if !done.is_empty() {
            self.until_halted(agent.sync())
                .await?
                .map_err(|error| self.agent_failure(error))?;
            let through = self.shared.state.lock().unwrap().covered_through;
            if self
                .shared
                .publish_through(through, self.config.drain_timeout)
                .await?
            {
                for task in &done {
                    self.acknowledge_claim(*task, report)?;
                }
                tasks.retain(|task| !done.contains(&task.id));
            }
        }
        Ok(!tasks.iter().any(ClaimedTask::is_turn))
    }

    fn acknowledge_claim(&self, task: Uuid, report: &mut HoldingReport) -> Result<(), Halt> {
        self.shared
            .chats
            .acknowledge_task(&self.shared.lease, task)
            .map_err(|_| self.shared.halt_or(Halt::Fenced))?;
        self.shared.claims.lock().unwrap().remove(&task);
        report.acknowledged.push(task);
        Ok(())
    }

    /// Runs `work` as `until_halted` does; a panic in it is the holder's crash.
    async fn guarded<T>(&self, work: impl Future<Output = T>) -> Result<T, Halt> {
        match self
            .until_halted(AssertUnwindSafe(work).catch_unwind())
            .await?
        {
            Ok(output) => Ok(output),
            Err(_) => Err(self.shared.halt_or(Halt::Crashed)),
        }
    }
}

/// Every tick of the host's clock renews the lease, moves the local deadline and extends the
/// claim of every task the holding has not acknowledged yet.
async fn heartbeat(shared: Arc<Shared>, config: HostConfig, stop: CancellationToken) {
    let mut clock = shared.clock.changes();
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = shared.halted.cancelled() => return,
            changed = clock.changed() => if changed.is_err() {
                return;
            },
        }
        let sent = shared.clock.now();
        if shared.check_deadline().is_err()
            || !shared.chats.renew_lease(&shared.lease, config.lease_ttl)
        {
            return;
        }
        shared.state.lock().unwrap().deadline = deadline(sent, config);
        let claims = shared.claims.lock().unwrap().clone();
        shared.chats.extend_claims(&shared.lease, &claims);
    }
}
