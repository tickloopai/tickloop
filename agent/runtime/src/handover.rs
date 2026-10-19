//! Voluntary handover. A host that gives up a family it holds, at its holding-time cap or on
//! SIGTERM, drains it: no Run starts, and each running Run stops at its next safe point, which
//! is before its next tool call or model request (a wait for a request gate and a retry backoff
//! count). It records where it stopped as a `Handover` wait of its suspended answer, so the
//! next holder goes on with the same attempt. A model request that is streaming and a tool call
//! that is running finish first. Whatever still runs at the ceiling ends as a host would lose
//! it, as `Interrupted{Handover}`: its input stays open and its Loop as it was, no completion is
//! recorded, and the next holder continues it without counting it as a lost host.

use std::time::Duration;

use tickloop_agent_protocol::{Receipt, RunId, Usage};
use tickloop_agent_transcript::{InterruptedBy, ResponseEnding, ResumePoint, WaitKind};

use super::{Agent, Chat, suspension};

/// How often a handover looks at the Runs again when no event arrives.
const HANDOVER_RECHECK: Duration = Duration::from_millis(20);

/// What a voluntary handover left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandOver {
    /// Every Run stopped at a safe point or ended before the ceiling.
    pub drained: bool,
    /// The Runs still going at the ceiling: their answers ended as `Interrupted{Handover}` and
    /// the next holder continues their inputs.
    pub interrupted: Vec<RunId>,
}

impl Agent {
    /// Drains every family this instance holds for another host: from now on no Run starts and
    /// each running Run stops at its next safe point. A Run that waits durably, for a person or
    /// for a member, is left as it is for `shutdown` to release. After `ceiling` the Runs still
    /// going end as `Interrupted{Handover}`. The host then shuts the instance down and syncs.
    pub async fn hand_over(&self, ceiling: Duration) -> HandOver {
        self.inner.handover.cancel();
        let mut events = self.subscribe();
        let give_up = tokio::time::Instant::now() + ceiling;
        loop {
            if !self.has_running_run().await {
                return HandOver {
                    drained: true,
                    interrupted: Vec::new(),
                };
            }
            tokio::select! {
                _ = events.recv() => {}
                _ = tokio::time::sleep(HANDOVER_RECHECK) => {}
                _ = tokio::time::sleep_until(give_up) => break,
            }
        }
        HandOver {
            drained: false,
            interrupted: self.end_runs_at_ceiling().await,
        }
    }

    /// Whether a Run holds an execution slot and neither waits durably nor left, or a Run that
    /// ended still records its end with its parent and delivers notifications.
    async fn has_running_run(&self) -> bool {
        let chats = self.inner.chats.lock().await;
        chats
            .values()
            .any(|chat| running_run(chat).is_some() || !chat.family_finishing.is_empty())
    }

    /// Ends each Run still going as its host would lose it: it is released, so it leaves this
    /// instance writing nothing more, its answer ends as `Interrupted{Handover}` and the work
    /// in flight is cancelled.
    async fn end_runs_at_ceiling(&self) -> Vec<RunId> {
        let mut interrupted = Vec::new();
        let mut chats = self.inner.chats.lock().await;
        for chat in chats.values_mut() {
            let Some(run) = running_run(chat) else {
                continue;
            };
            self.end_for_handover(chat, run);
            if let Some(cancel) = &chat.active {
                cancel.cancel();
            }
            interrupted.push(run);
        }
        interrupted
    }

    /// Ends the Run as its host would lose it: released first, so it writes nothing more, then
    /// its answer ends as `Interrupted{Handover}`, leaving its input open for a continuation.
    fn end_for_handover(&self, chat: &mut Chat, run: RunId) {
        chat.releases.entry(run).or_default().cancel();
        self.end_response(
            chat,
            run,
            ResponseEnding::Interrupted {
                by: InterruptedBy::Handover,
            },
            Usage::default(),
        );
        self.save_now(&chat.snapshot);
    }

    /// A handover stopped the Run's model request before it reached the vendor: the Run stops
    /// before its model request, or, when its answer cannot be suspended there, ends as at the
    /// ceiling. Either way it leaves with the release error.
    pub(crate) async fn leave_before_model_request(&self, receipt: &Receipt) -> String {
        if let Err(released) = self.stop_at_safe_point(receipt, None).await {
            return released;
        }
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id) {
            self.end_for_handover(chat, receipt.run_id);
        }
        suspension::RUN_RELEASED.into()
    }

    /// A safe point of `receipt`'s Run: before its call `at_call`, or before its next model
    /// request without one. During a handover the Run stops here: its answer is suspended on a
    /// `Handover` wait that says where it goes on, and it leaves this instance with the release
    /// error. A Run released already leaves as well.
    pub(crate) async fn stop_at_safe_point(
        &self,
        receipt: &Receipt,
        at_call: Option<&str>,
    ) -> Result<(), String> {
        if !self.inner.handover.is_cancelled() {
            return Ok(());
        }
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&receipt.chat_id) else {
            return Ok(());
        };
        if Agent::release_of(chat, receipt.run_id).is_cancelled() {
            return Err(suspension::RUN_RELEASED.into());
        }
        let resume = match at_call {
            Some(call_id) => {
                let Some(assistant_message) =
                    suspension::batch_message_of(&chat.snapshot, receipt.run_id, call_id)
                else {
                    return Ok(());
                };
                ResumePoint::InBatch {
                    assistant_message,
                    call_id: call_id.into(),
                }
            }
            None => ResumePoint::BeforeModelRequest,
        };
        if chat
            .bookkeeping
            .lifecycle
            .suspend(receipt, WaitKind::Handover, resume)
            .is_none()
        {
            return Ok(());
        }
        chat.releases.entry(receipt.run_id).or_default().cancel();
        self.save_now(&chat.snapshot);
        Err(suspension::RUN_RELEASED.into())
    }

    /// Whether a model request of `receipt`'s Run that was cancelled stopped for a handover, and
    /// not for the Run's own cancellation.
    pub(crate) fn stopped_for_handover(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> bool {
        self.inner.handover.is_cancelled() && !cancel.is_cancelled()
    }
}

/// The Chat's Run that holds the execution slot and runs: one that waits for a member or was
/// released does not.
fn running_run(chat: &Chat) -> Option<RunId> {
    let run = chat.active_run?;
    let waits_for_member = chat
        .bookkeeping
        .lifecycle
        .open_wait_of(run, |kind| {
            matches!(kind, WaitKind::Subagent { .. } | WaitKind::Children { .. })
        })
        .is_some();
    let is_released = Agent::release_of(chat, run).is_cancelled();
    (!waits_for_member && !is_released).then_some(run)
}
