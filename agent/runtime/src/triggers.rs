use super::loop_lease::{RunStateChange, TriggerPermit};
use super::*;
use tickloop_agent_protocol::local_messages;
use tickloop_agent_protocol::{
    ChatSnapshot, LoopSnapshot, LoopState, OperationRecord, Receipt, TICK_SUBSCRIPTION_SOURCE,
    Trigger, TriggerPromptSource, TriggerSource,
};

const STOPPED_BEFORE_ADMISSION: &str = "the Chat was stopped before Shell admission";

/// Whether a Trigger may reach the Loop now: it receives from its Trigger or a feedback link,
/// and no turn waits for a person. An interrupted or failed turn does not stop it.
pub(super) fn can_trigger(chat_loop: &LoopSnapshot) -> bool {
    (chat_loop
        .trigger
        .as_ref()
        .is_some_and(|trigger| trigger.armed)
        || chat_loop
            .event_sources
            .iter()
            .any(|source| !source.resume_required))
        && match chat_loop.state {
            LoopState::Idle
            | LoopState::Queued
            | LoopState::Running
            | LoopState::Interrupted
            | LoopState::Failed => true,
            LoopState::WaitingPermission | LoopState::WaitingQuestion | LoopState::WaitingPlan => {
                false
            }
        }
}

/// Whether a queued Trigger input may start a turn: the Loop still receives from its source. A
/// user's one-off Tick update is no Trigger the Loop receives, so pausing does not hold it; a
/// received @tickloop request stays valid after its artifact is unlinked.
pub(super) fn takes_trigger_input(chat_loop: &LoopSnapshot, source: &TriggerSource) -> bool {
    match source.source.as_str() {
        TICK_MANUAL_SOURCE => true,
        super::event_sources::ARTIFACT_FEEDBACK_SOURCE => {
            chat_loop.event_sources.is_empty()
                || chat_loop
                    .event_sources
                    .iter()
                    .any(|binding| !binding.resume_required)
        }
        _ => chat_loop
            .trigger
            .as_ref()
            .is_some_and(|trigger| trigger.armed),
    }
}

/// Loop 进入一个新进程（重启恢复、`--fork-session` 或 Branch 复制）时的恢复规则：运行或等待中的
/// 一轮转为已中断；接收暂停，武装中的 Trigger 解除武装，Chat 继续时重新武装。
pub(super) fn recover_loop(snapshot: &mut ChatSnapshot) {
    recover_loop_keeping(snapshot, false, WaitHosting::Process);
}

/// [`recover_loop`], except that a Loop whose Run is rebuilt (`keeps_run`) keeps its state. A
/// durable host keeps an armed Trigger armed. It keeps the Loop's artifact links receiving too,
/// since the host delivers their requests rather than a watcher of this process.
pub(super) fn recover_loop_keeping(
    snapshot: &mut ChatSnapshot,
    keeps_run: bool,
    waits: WaitHosting,
) {
    recover_trigger(snapshot, waits);
    if keeps_run {
        return;
    }
    let chat_loop = &mut snapshot.chat_loop;
    if waits == WaitHosting::Process {
        for source in &mut chat_loop.event_sources {
            source.resume_required = true;
        }
    }
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

fn recover_trigger(snapshot: &mut ChatSnapshot, waits: WaitHosting) {
    if let Some(trigger) = &mut snapshot.chat_loop.trigger
        && !(waits == WaitHosting::Durable && trigger.armed)
    {
        if trigger.armed {
            snapshot.trigger_awaits_resume = true;
        }
        trigger.armed = false;
    }
    snapshot
        .pending_inputs
        .retain(|pending| pending.trigger_source.is_none());
}

/// Re-arms a Trigger that waits for the Chat to be continued; whether it did.
pub(super) fn rearm_trigger_awaiting_resume(chat: &mut Chat) -> Result<bool, AgentError> {
    if !chat.snapshot.trigger_awaits_resume {
        return Ok(false);
    }
    chat.snapshot.trigger_awaits_resume = false;
    let chat_loop = &mut chat.snapshot.chat_loop;
    let Some(trigger) = chat_loop.trigger.as_mut() else {
        return Ok(false);
    };
    trigger.armed = true;
    chat.trigger_cancellations
        .insert(chat_loop.id, CancellationToken::new());
    Ok(true)
}

impl Agent {
    pub async fn configure_trigger(
        &self,
        chat_id: ChatId,
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
            let lacks_prompt = match trigger.prompt_source {
                TriggerPromptSource::Explicit => trigger.prompt.trim().is_empty(),
                TriggerPromptSource::Subscriptions => false,
            };
            if lacks_prompt || !trigger.accepts_events {
                return Err(AgentError::Invalid(
                    "trigger requires a prompt and explicit events".into(),
                ));
            }
            tick_subscriptions::validate(trigger).map_err(AgentError::Invalid)?;
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let loop_id = chat.snapshot.chat_loop.id;
        chat.snapshot.chat_loop.trigger = trigger.clone();
        // A Trigger disarmed until the Chat is continued stays so; arming it or removing it
        // settles that.
        chat.snapshot.trigger_awaits_resume &=
            trigger.as_ref().is_some_and(|trigger| !trigger.armed);
        self.discard_replaced_trigger_inputs(chat);
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

    fn discard_replaced_trigger_inputs(&self, chat: &mut Chat) {
        let mut discarded = Vec::new();
        chat.snapshot.pending_inputs.retain(|pending| {
            let keep = pending.trigger_source.as_ref().is_none_or(|source| {
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

    /// 停止状态显示另一个进程暂停了接收：本地同样解除 Trigger 武装、取消事件派发令牌并归还租约，
    /// 不再写停止状态。Chat 继续时重新武装。
    pub(super) async fn disarm_trigger_disarmed_elsewhere(&self, chat_id: ChatId, loop_id: LoopId) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        if let Some(trigger) = &mut chat.snapshot.chat_loop.trigger
            && trigger.armed
        {
            trigger.armed = false;
            chat.snapshot.trigger_awaits_resume = true;
        }
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

    /// With durable wait hosting, the loaded Chat's armed Trigger goes on taking events in this
    /// instance.
    pub(crate) async fn register_armed_triggers_after_load(&self, chat_id: ChatId) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        let chat_loop = &chat.snapshot.chat_loop;
        if can_trigger(chat_loop)
            && chat_loop
                .trigger
                .as_ref()
                .is_some_and(|trigger| trigger.armed)
            && let Some(previous) = chat
                .trigger_cancellations
                .insert(chat_loop.id, CancellationToken::new())
        {
            previous.cancel();
        }
    }

    /// `/stop`: interrupts the current turn, with every turn that waits for a person, discards
    /// the queued messages and Shell commands that have not started, and pauses receiving until
    /// the Chat is continued. Queued Trigger input waits for that; background Shells and Ticks go
    /// on.
    pub(super) async fn stop_loop(
        &self,
        chat_id: ChatId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        let (operations, loop_id, disarmed) = {
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
            let chat_loop = &mut chat.snapshot.chat_loop;
            let loop_id = chat_loop.id;
            let mut disarmed = false;
            if let Some(trigger) = &mut chat_loop.trigger
                && trigger.armed
            {
                trigger.armed = false;
                disarmed = true;
            }
            chat.snapshot.trigger_awaits_resume = true;
            for source in &mut chat.snapshot.chat_loop.event_sources {
                disarmed |= !source.resume_required;
                source.resume_required = true;
            }
            if let Some(cancel) = chat.trigger_cancellations.remove(&loop_id) {
                cancel.cancel();
            }
            if let Some(cancel) = chat.event_cancellations.remove(&loop_id) {
                cancel.cancel();
            }
            self.save(&chat.snapshot);
            (operations, loop_id, disarmed)
        };
        if disarmed {
            // Written before the lease is returned: a process taking it over sees the pause.
            let recorded = self
                .record_loop_run_state(chat_id, loop_id, RunStateChange::Disarmed)
                .await;
            self.inner.loop_leases.release(chat_id, loop_id);
            self.warn_loop_run_state_failure(chat_id, loop_id, recorded);
        }
        self.sync_tick_subscriptions().await;
        Ok(operations)
    }

    /// Continues the Chat without a new input: its Loop receives Triggers again, and Trigger
    /// input queued before it paused may start.
    pub async fn continue_loop(&self, chat_id: ChatId) -> Result<(), AgentError> {
        let rearmed = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            let rearmed = rearm_trigger_awaiting_resume(chat)?;
            if rearmed {
                self.save(&chat.snapshot);
            }
            rearmed.then_some(chat.snapshot.chat_loop.id)
        };
        if let Some(loop_id) = rearmed {
            self.record_armed_trigger(chat_id, loop_id).await;
            self.sync_tick_subscriptions().await;
            self.flush_due_tick_delivery(chat_id, loop_id);
        }
        Box::pin(self.resume_feedback_sources(chat_id)).await;
        if let Some(next) = self.next_run(chat_id).await? {
            self.run_task(next);
        }
        Ok(())
    }

    pub async fn trigger(
        &self,
        chat_id: ChatId,
        source: TriggerSource,
    ) -> Result<Receipt, AgentError> {
        if source.source.trim().is_empty() {
            return Err(AgentError::Invalid("event source is required".into()));
        }
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        let chat_loop = &chat.snapshot.chat_loop;
        let loop_id = chat_loop.id;
        let trigger = chat_loop
            .trigger
            .as_ref()
            .filter(|trigger| trigger.accepts_events)
            .ok_or_else(|| {
                AgentError::Invalid("the Loop does not accept explicit events".into())
            })?;
        let input = if source.source == TICK_SUBSCRIPTION_SOURCE {
            local_messages::tick_delivery(&source.payload, trigger)
        } else {
            local_messages::external_trigger(&source, trigger)
        };
        let is_eligible = can_trigger(chat_loop);
        let cancel = chat.trigger_cancellations.get(&loop_id).cloned();
        drop(chats);
        if is_eligible {
            match self.acquire_trigger_permit(chat_id, loop_id).await {
                TriggerPermit::Fire => {}
                TriggerPermit::Wait(reason) => return Err(AgentError::Invalid(reason)),
                TriggerPermit::Withdrawn => {
                    return Err(AgentError::Invalid(
                        "receiving was paused in another Tickloop process; the event was not \
                         delivered"
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
