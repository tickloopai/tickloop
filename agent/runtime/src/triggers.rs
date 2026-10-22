use super::loop_lease::{RunStateChange, TriggerPermit};
use super::*;
use tickloop_agent_protocol::local_messages;
use tickloop_agent_protocol::{
    ChatSnapshot, EventKind, LoopId, LoopSnapshot, LoopState, OperationRecord, OperationState,
    Receipt, TICK_SUBSCRIPTION_SOURCE, Trigger, TriggerSource, UserShellState,
};
use tickloop_agent_transcript::{DiscardReason, SettledOutcome};

pub(super) fn can_trigger(goal: &LoopSnapshot) -> bool {
    (goal.trigger.as_ref().is_some_and(|trigger| trigger.armed)
        || goal
            .event_sources
            .iter()
            .any(|source| !source.resume_required))
        && matches!(
            goal.state,
            LoopState::Running
                | LoopState::Queued
                | LoopState::WaitingTrigger
                | LoopState::Completed
        )
}

/// Loop 进入一个新进程（重启恢复、`--fork-session` 或 Branch 复制）时的恢复规则：运行或等待中的 Loop 转为
/// 已中断；武装中的 Trigger 解除武装，记入手动恢复时重新武装。
pub(super) fn recover_loops(snapshot: &mut ChatSnapshot) {
    recover_loops_keeping(snapshot, &HashSet::new(), WaitHosting::Process);
}

/// [`recover_loops`], except that the Loops in `kept` keep their state: their Runs are rebuilt.
/// A durable host keeps every armed Trigger armed and its Loop waiting for it. It keeps the
/// Loop's artifact links receiving too, since the host delivers their requests rather than a
/// watcher of this process.
pub(super) fn recover_loops_keeping(
    snapshot: &mut ChatSnapshot,
    kept: &HashSet<LoopId>,
    waits: WaitHosting,
) {
    recover_triggers(snapshot, waits);
    for goal in snapshot
        .loops
        .iter_mut()
        .filter(|goal| !kept.contains(&goal.id))
    {
        let keeps_its_trigger = waits == WaitHosting::Durable
            && goal.trigger.as_ref().is_some_and(|trigger| trigger.armed);
        if !goal.event_sources.is_empty() && waits == WaitHosting::Process {
            for source in &mut goal.event_sources {
                source.resume_required = true;
            }
            if goal.state == LoopState::WaitingTrigger && !keeps_its_trigger {
                goal.recover_state(LoopState::Interrupted);
            }
        }
        if matches!(
            goal.state,
            LoopState::Running
                | LoopState::Queued
                | LoopState::WaitingPermission
                | LoopState::WaitingQuestion
        ) {
            goal.recover_state(LoopState::Interrupted);
        }
    }
}

fn recover_triggers(snapshot: &mut ChatSnapshot, waits: WaitHosting) {
    for goal in &mut snapshot.loops {
        let Some(trigger) = &goal.trigger else {
            continue;
        };
        let armed = trigger.armed;
        let has_ended = matches!(goal.state, LoopState::Stopped | LoopState::Expired);
        if has_ended {
            snapshot.resume_triggers.retain(|id| *id != goal.id);
        } else if waits == WaitHosting::Durable && armed {
            continue;
        } else if armed && !snapshot.resume_triggers.contains(&goal.id) {
            snapshot.resume_triggers.push(goal.id);
        }
        if let Some(trigger) = &mut goal.trigger {
            trigger.armed = false;
        }
        if goal.state == LoopState::WaitingTrigger {
            goal.recover_state(LoopState::Interrupted);
        }
    }
    snapshot.pending_inputs.retain(|pending| {
        pending.trigger_source.is_none()
            && snapshot.loops.iter().any(|goal| {
                goal.id == pending.receipt.loop_id
                    && !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
            })
    });
}

impl Agent {
    pub async fn configure_trigger(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        trigger: Option<Trigger>,
    ) -> Result<(), AgentError> {
        if let Some(trigger) = &trigger {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if chat.snapshot.delegation.as_ref().is_some_and(|delegation| {
                trigger.subscriptions.iter().any(|subscription| {
                    !delegation
                        .ceiling
                        .tick_instances
                        .contains(&subscription.instance)
                })
            }) {
                return Err(AgentError::Invalid("Unsupported: Tick Subscription is outside the inherited delegated instance scope".into()));
            }
        }
        if let Some(trigger) = &trigger {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if let Some(delegation) = &chat.snapshot.delegation
                && trigger
                    .subscriptions
                    .iter()
                    .filter_map(|subscription| subscription.authority.as_ref())
                    .any(|authority| {
                        !delegation
                            .ceiling
                            .host_tools
                            .values()
                            .any(|allowed| allowed.contains(authority))
                    })
            {
                return Err(AgentError::Invalid(
                    "Tick watch authority is outside the frozen delegated account/endpoint scope"
                        .into(),
                ));
            }
        }
        if let Some(trigger) = &trigger {
            if trigger.prompt.trim().is_empty() || !trigger.accepts_events {
                return Err(AgentError::Invalid(
                    "trigger requires a prompt and explicit events".into(),
                ));
            }
            tick_subscriptions::validate(trigger).map_err(AgentError::Invalid)?;
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let mut snapshot = chat.snapshot.clone();
        let goal = snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == loop_id)
            .ok_or(AgentError::NotFound)?;
        if matches!(goal.state, LoopState::Stopped | LoopState::Expired) {
            return Err(AgentError::Invalid(
                "ended goals cannot take a trigger".into(),
            ));
        }
        goal.trigger = trigger.clone();
        if goal.state == LoopState::Completed
            && trigger.as_ref().is_some_and(|trigger| trigger.armed)
        {
            goal.set_state(LoopState::WaitingTrigger);
        } else if goal.state == LoopState::WaitingTrigger
            && trigger.as_ref().is_none_or(|trigger| !trigger.armed)
            && !goal
                .event_sources
                .iter()
                .any(|source| !source.resume_required)
        {
            goal.set_state(LoopState::Completed);
        }
        snapshot.resume_triggers.retain(|id| *id != loop_id);
        chat.snapshot = snapshot;
        self.discard_replaced_trigger_inputs(chat, loop_id);
        self.save(&chat.snapshot);
        if let Some(previous) = chat.trigger_cancellations.remove(&loop_id) {
            previous.cancel();
        }
        let is_armed = trigger.is_some_and(|trigger| trigger.armed);
        if is_armed {
            chat.trigger_cancellations
                .insert(loop_id, CancellationToken::new());
        }
        drop(chats);
        self.sync_tick_subscriptions().await;
        if !is_armed {
            let recorded = self
                .record_loop_run_state(chat_id, loop_id, RunStateChange::Disarmed)
                .await;
            self.inner.loop_leases.release(chat_id, loop_id);
            self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
            return Ok(());
        }
        self.record_armed_trigger(chat_id, loop_id).await;
        Ok(())
    }

    fn discard_replaced_trigger_inputs(&self, chat: &mut Chat, loop_id: LoopId) {
        let mut discarded = Vec::new();
        chat.snapshot.pending_inputs.retain(|pending| {
            let keep = pending.receipt.loop_id != loop_id
                || pending.trigger_source.as_ref().is_none_or(|source| {
                    matches!(
                        source.source.as_str(),
                        super::event_sources::ARTIFACT_FEEDBACK_SOURCE
                            | TICK_SUBSCRIPTION_SOURCE
                            | TICK_MANUAL_SOURCE
                    )
                });
            if !keep {
                discarded.push(pending.receipt.clone());
            }
            keep
        });
        for receipt in discarded {
            self.finish_discarded_trigger_input(chat, &receipt);
        }
        self.discard_invalid_tick_inputs(chat);
    }

    /// 停止状态显示另一个进程解除了该 Trigger 的武装：本地同样解除武装、取消 Trigger 任务并
    /// 归还租约，不再写停止状态。
    pub(super) async fn disarm_trigger_disarmed_elsewhere(&self, chat_id: ChatId, loop_id: LoopId) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        let mut snapshot = chat.snapshot.clone();
        if let Some(goal) = snapshot.loops.iter_mut().find(|goal| goal.id == loop_id) {
            if let Some(trigger) = &mut goal.trigger {
                trigger.armed = false;
            }
            if goal.state == LoopState::WaitingTrigger
                && !goal
                    .event_sources
                    .iter()
                    .any(|source| !source.resume_required)
            {
                goal.set_state(LoopState::Completed);
            }
        }
        snapshot.resume_triggers.retain(|id| *id != loop_id);
        chat.snapshot = snapshot;
        self.discard_replaced_trigger_inputs(chat, loop_id);
        self.save(&chat.snapshot);
        if let Some(cancel) = chat.trigger_cancellations.remove(&loop_id) {
            cancel.cancel();
        }
        drop(chats);
        self.inner.loop_leases.release(chat_id, loop_id);
        self.sync_tick_subscriptions().await;
    }

    /// Records the arming of the Trigger just armed in the Loop's run state, which other
    /// processes read before they take the Loop's lease.
    pub(super) async fn record_armed_trigger(&self, chat_id: ChatId, loop_id: LoopId) {
        let recorded = self
            .record_loop_run_state(chat_id, loop_id, RunStateChange::Armed)
            .await;
        self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
    }

    /// With durable wait hosting, the loaded Chat's armed Triggers go on taking events in this
    /// instance.
    pub(crate) async fn register_armed_triggers_after_load(&self, chat_id: ChatId) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        let armed = chat
            .snapshot
            .loops
            .iter()
            .filter(|goal| {
                can_trigger(goal) && goal.trigger.as_ref().is_some_and(|trigger| trigger.armed)
            })
            .map(|goal| goal.id)
            .collect::<Vec<_>>();
        for loop_id in armed {
            if let Some(previous) = chat
                .trigger_cancellations
                .insert(loop_id, CancellationToken::new())
            {
                previous.cancel();
            }
        }
    }

    pub(super) async fn end_goal(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        self.unlink_feedback_receivers(chat_id, Some(loop_id), None)
            .await?;
        self.finish_goal(chat_id, loop_id, "stopped by user", true)
            .await
    }

    /// 停止状态显示 Loop 已在另一个进程结束：本地同样停止，不再写停止状态。
    pub(super) async fn end_goal_ended_elsewhere(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        self.finish_goal(
            chat_id,
            loop_id,
            "stopped in another Tickloop process",
            false,
        )
        .await
    }

    async fn finish_goal(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        reason: &str,
        records_run_state: bool,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let original = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let mut chat = original.clone();
        let goal = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == loop_id)
            .ok_or(AgentError::NotFound)?;
        let operations: Vec<_> = chat
            .snapshot
            .operations
            .iter()
            .filter(|operation| {
                operation.loop_id == loop_id
                    && matches!(
                        operation.state,
                        OperationState::Admitted | OperationState::Unknown
                    )
            })
            .cloned()
            .collect();
        if matches!(goal.state, LoopState::Stopped | LoopState::Expired) {
            return Ok(operations);
        }
        let receipt = Receipt {
            chat_id,
            loop_id,
            run_id: goal.run_id.unwrap_or_else(Uuid::new_v4),
        };
        let is_active = chat.active_run == Some(receipt.run_id);
        goal.set_state(LoopState::Stopped);
        for source in &mut goal.event_sources {
            source.resume_required = true;
        }
        if let Some(trigger) = &mut goal.trigger {
            trigger.armed = false;
        }
        let mut cancelled_shells = HashSet::new();
        for shell in &mut chat.snapshot.user_shells {
            if shell.loop_id == Some(loop_id)
                && (shell.state == UserShellState::Queued || shell.awaits_admission())
            {
                cancelled_shells.insert(shell.id);
                shell.armed = false;
                shell.set_state(UserShellState::Failed);
                shell.reason = Some("the owning Loop ended before Shell admission".into());
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
        let (discarded, kept) = std::mem::take(&mut chat.snapshot.pending_inputs)
            .into_iter()
            .partition::<Vec<_>, _>(|pending| pending.receipt.loop_id == loop_id);
        chat.snapshot.pending_inputs = kept;
        self.settle_removed_inputs(
            &mut chat,
            &discarded,
            SettledOutcome::Discarded {
                reason: DiscardReason::Stopped,
            },
        );
        chat.snapshot
            .permissions
            .retain(|permission| permission.loop_id != loop_id);
        chat.snapshot
            .questions
            .retain(|question| question.loop_id != loop_id);
        chat.snapshot.resume_triggers.retain(|id| *id != loop_id);
        self.save(&chat.snapshot);
        let cancelled_runs = chat
            .parked
            .values()
            .filter(|run| run.receipt.loop_id == loop_id)
            .map(|waiting| {
                waiting.cancel.cancel();
                waiting.receipt.run_id
            })
            .chain(is_active.then_some(receipt.run_id))
            .collect::<Vec<_>>();
        if is_active && let Some(cancel) = &chat.active {
            cancel.cancel();
        }
        chat.user_cancelled_runs.extend(cancelled_runs);
        if let Some(cancel) = chat.trigger_cancellations.remove(&loop_id) {
            cancel.cancel();
        }
        if let Some(cancel) = chat.event_cancellations.remove(&loop_id) {
            cancel.cancel();
        }
        let tools = chat.tools.clone();
        self.emit(
            &mut chat,
            &receipt,
            EventKind::GoalEnded {
                reason: reason.to_owned(),
                operations: operations.clone(),
            },
        );
        *original = chat;
        drop(chats);
        // 先写结束状态再归还租约：接手的进程读取停止状态时已看到结束。
        let recorded = if records_run_state {
            self.record_loop_run_state(chat_id, loop_id, RunStateChange::Ended)
                .await
        } else {
            Ok(())
        };
        self.inner.loop_leases.release(chat_id, loop_id);
        self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
        self.sync_tick_subscriptions().await;
        for command in tools.stop_loop(loop_id).await {
            self.record_command(chat_id, command).await?;
        }
        // Child completions held for this Loop while it had failed or was interrupted are refused
        // now that it has ended.
        self.flush_subagent_notifications(chat_id).await;
        Ok(operations)
    }

    pub async fn trigger(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        source: TriggerSource,
    ) -> Result<Receipt, AgentError> {
        if source.source.trim().is_empty() {
            return Err(AgentError::Invalid("event source is required".into()));
        }
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        let goal = chat
            .snapshot
            .loops
            .iter()
            .find(|goal| goal.id == loop_id)
            .ok_or(AgentError::NotFound)?;
        let trigger = goal
            .trigger
            .as_ref()
            .filter(|trigger| trigger.accepts_events)
            .ok_or_else(|| AgentError::Invalid("goal does not accept explicit events".into()))?;
        let input = if source.source == TICK_SUBSCRIPTION_SOURCE {
            local_messages::tick_delivery(&source.payload, trigger)
        } else {
            local_messages::external_trigger(&source, trigger)
        };
        let is_eligible = can_trigger(goal);
        let cancel = chat.trigger_cancellations.get(&loop_id).cloned();
        drop(chats);
        if is_eligible {
            match self.acquire_trigger_permit(chat_id, loop_id).await {
                TriggerPermit::Fire => {}
                TriggerPermit::Wait(reason) => return Err(AgentError::Invalid(reason)),
                TriggerPermit::Withdrawn => {
                    return Err(AgentError::Invalid(
                        "this goal was stopped or its trigger disarmed in another Tickloop \
                         process; the event was not delivered"
                            .into(),
                    ));
                }
                TriggerPermit::Failed(error) => return Err(AgentError::Storage(error)),
            }
        }
        if source.source == TICK_SUBSCRIPTION_SOURCE
            && let Err(error) = self
                .revalidate_tick_source(chat_id, loop_id, &source.payload)
                .await
        {
            self.discard_invalid_pending_tick_inputs().await;
            return Err(error);
        }
        self.submit_internal(
            chat_id,
            Some(loop_id),
            input,
            SubmissionOptions {
                trigger_source: Some(source),
                trigger_cancel: cancel,
                ..Default::default()
            },
        )
        .await
    }
}
