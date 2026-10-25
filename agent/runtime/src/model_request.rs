use std::{sync::Arc, time::Duration};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Agent, AgentError};
use tickloop_agent_model::{
    ModelClient, ModelDelta, ModelError, ModelFailureKind, ModelOutput, ModelRequest,
    ModelRequestContext, ModelResponseRejection,
};
use tickloop_agent_protocol::{
    ChatId, ChatSnapshot, Content, Event, EventKind, LedgerError, Message, ModelIdentity,
    ModelRequestKind, ProtocolFailure, Receipt, RequestFailure, RequestOutcome, RequestOwner,
    RequestSequence, RequestUsageLedger, ResponseRejection, Role, UsageCounter,
};

const MODEL_DELTA_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
const MODEL_DELTA_BATCH_COUNT: usize = 256;
const MODEL_DELTA_ENVELOPE_BYTES: usize = 1024;
const MAX_JSON_ESCAPE_EXPANSION: usize = 6;
const MODEL_DELTA_BATCH_BYTES: usize = (tickloop_agent_protocol::FEED_CHANGE_BYTES
    - MODEL_DELTA_ENVELOPE_BYTES)
    / MAX_JSON_ESCAPE_EXPANSION;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RequestContext {
    owner: RequestOwner,
    kind: ModelRequestKind,
}

impl RequestContext {
    pub(super) fn for_run(receipt: &Receipt, kind: ModelRequestKind) -> Self {
        Self {
            owner: RequestOwner {
                chat_id: receipt.chat_id,
                loop_id: Some(receipt.loop_id),
                run_id: Some(receipt.run_id),
            },
            kind,
        }
    }

    pub(super) fn title(chat_id: ChatId) -> Self {
        Self {
            owner: RequestOwner {
                chat_id,
                loop_id: None,
                run_id: None,
            },
            kind: ModelRequestKind::Title,
        }
    }

    pub(super) fn shell_description(chat_id: ChatId) -> Self {
        Self {
            owner: RequestOwner {
                chat_id,
                loop_id: None,
                run_id: None,
            },
            kind: ModelRequestKind::ShellDescription,
        }
    }

    pub(super) fn auto_setup(chat_id: ChatId) -> Self {
        Self {
            owner: RequestOwner {
                chat_id,
                loop_id: None,
                run_id: None,
            },
            kind: ModelRequestKind::AutoSetup,
        }
    }

    pub(super) fn manual_compaction(chat_id: ChatId) -> Self {
        Self {
            owner: RequestOwner {
                chat_id,
                loop_id: None,
                run_id: None,
            },
            kind: ModelRequestKind::CompactionManual,
        }
    }

    pub(super) fn directed_compaction(chat_id: ChatId) -> Self {
        Self {
            owner: RequestOwner {
                chat_id,
                loop_id: None,
                run_id: None,
            },
            kind: ModelRequestKind::CompactionDirected,
        }
    }

    fn conversation_receipt(&self) -> Option<Receipt> {
        (self.kind == ModelRequestKind::Conversation).then(|| Receipt {
            chat_id: self.owner.chat_id,
            loop_id: self.owner.loop_id.expect("conversation has a Loop"),
            run_id: self.owner.run_id.expect("conversation has a Run"),
        })
    }

    fn retry_receipt(&self) -> Option<Receipt> {
        matches!(
            self.kind,
            ModelRequestKind::Conversation
                | ModelRequestKind::PermissionCheck
                | ModelRequestKind::WebFetch
                | ModelRequestKind::GoalCheck
        )
        .then(|| Receipt {
            chat_id: self.owner.chat_id,
            loop_id: self.owner.loop_id.expect("run request has a Loop"),
            run_id: self.owner.run_id.expect("run request has a Run"),
        })
    }
}

pub(super) struct PendingModelResponse {
    pub sequence: RequestSequence,
    pub output: ModelOutput,
}

struct RequestTracking<'a> {
    timeout: Option<Duration>,
    captured_text: Option<&'a mut String>,
    output_started: Option<&'a mut bool>,
    model_context: Option<ModelRequestContext>,
}

struct RetryTracking<'a> {
    timeout: Option<Duration>,
    captured_text: Option<&'a mut String>,
    first_sequence: Option<RequestSequence>,
}

pub(super) const MAX_MODEL_ERROR_RETRIES: usize = 10;
const MAX_PERMISSION_CHECK_RETRIES: usize = 2;
const MAX_OVERLOAD_ATTEMPTS: usize = 3;
const BASE_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(32);

pub(super) fn start_tracked_model_request(
    snapshot: &mut ChatSnapshot,
    context: RequestContext,
    identity: ModelIdentity,
) -> Result<RequestSequence, AgentError> {
    if context.owner.chat_id != snapshot.id {
        return Err(LedgerError::ChatMismatch {
            expected: snapshot.id,
            actual: context.owner.chat_id,
        }
        .into());
    }
    snapshot
        .request_usage
        .start(context.owner, context.kind, identity)
        .map_err(AgentError::from)
}

#[derive(Default)]
struct RequestProgress {
    output_bytes: usize,
    displayed_text: String,
    output_started: bool,
    regression: Option<UsageCounter>,
    pending_delta: Option<ModelDelta>,
    pending_delta_count: usize,
    delta_deadline: Option<tokio::time::Instant>,
}

impl Agent {
    async fn buffer_model_delta(
        &self,
        context: &RequestContext,
        sequence: RequestSequence,
        delta: ModelDelta,
        cancel: &CancellationToken,
        progress: &mut RequestProgress,
    ) -> Result<(), AgentError> {
        let (text, thinking) = match delta {
            ModelDelta::Text(text) => (text, false),
            ModelDelta::Thinking(text) => (text, true),
            other => {
                return self
                    .buffer_model_delta_part(context, sequence, other, cancel, progress)
                    .await;
            }
        };
        let mut remaining = text.as_str();
        loop {
            let mut end = remaining.len().min(MODEL_DELTA_BATCH_BYTES);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            let (part, rest) = remaining.split_at(end);
            let delta = if thinking {
                ModelDelta::Thinking(part.into())
            } else {
                ModelDelta::Text(part.into())
            };
            self.buffer_model_delta_part(context, sequence, delta, cancel, progress)
                .await?;
            if rest.is_empty() {
                return Ok(());
            }
            remaining = rest;
        }
    }

    async fn buffer_model_delta_part(
        &self,
        context: &RequestContext,
        sequence: RequestSequence,
        delta: ModelDelta,
        cancel: &CancellationToken,
        progress: &mut RequestProgress,
    ) -> Result<(), AgentError> {
        if !matches!(delta, ModelDelta::Text(_) | ModelDelta::Thinking(_)) {
            self.flush_model_delta(context, sequence, cancel, progress)
                .await?;
            return self
                .record_model_delta(context, sequence, delta, !cancel.is_cancelled(), progress)
                .await;
        }
        let merged = match (&mut progress.pending_delta, &delta) {
            (Some(ModelDelta::Text(current)), ModelDelta::Text(next))
            | (Some(ModelDelta::Thinking(current)), ModelDelta::Thinking(next))
                if current.len().saturating_add(next.len()) <= MODEL_DELTA_BATCH_BYTES =>
            {
                current.push_str(next);
                true
            }
            _ => false,
        };
        if !merged {
            self.flush_model_delta(context, sequence, cancel, progress)
                .await?;
            progress.pending_delta = Some(delta);
            progress.delta_deadline =
                Some(tokio::time::Instant::now() + MODEL_DELTA_FLUSH_INTERVAL);
        }
        progress.pending_delta_count += 1;
        let bytes = match &progress.pending_delta {
            Some(ModelDelta::Text(text) | ModelDelta::Thinking(text)) => text.len(),
            _ => 0,
        };
        if progress.pending_delta_count >= MODEL_DELTA_BATCH_COUNT
            || bytes >= MODEL_DELTA_BATCH_BYTES
        {
            self.flush_model_delta(context, sequence, cancel, progress)
                .await?;
        }
        Ok(())
    }

    async fn flush_model_delta(
        &self,
        context: &RequestContext,
        sequence: RequestSequence,
        cancel: &CancellationToken,
        progress: &mut RequestProgress,
    ) -> Result<(), AgentError> {
        progress.delta_deadline = None;
        progress.pending_delta_count = 0;
        if let Some(delta) = progress.pending_delta.take() {
            self.record_model_delta(context, sequence, delta, !cancel.is_cancelled(), progress)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn change_request_usage<T>(
        &self,
        chat_id: ChatId,
        change: impl FnOnce(&mut RequestUsageLedger) -> Result<T, LedgerError>,
    ) -> Result<T, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        Ok(change(&mut chat.snapshot.request_usage)?)
    }

    pub(super) async fn finish_model_request(
        &self,
        chat_id: ChatId,
        sequence: RequestSequence,
        outcome: RequestOutcome,
    ) -> Result<(), AgentError> {
        self.change_request_usage(chat_id, |ledger| ledger.finish(sequence, outcome))
            .await
    }

    async fn record_model_delta(
        &self,
        context: &RequestContext,
        sequence: RequestSequence,
        delta: ModelDelta,
        display_text: bool,
        progress: &mut RequestProgress,
    ) -> Result<(), AgentError> {
        let output_exhausted = if let Some(task) =
            tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(context.kind)
        {
            let limit = task.budget().output_tokens();
            match &delta {
                ModelDelta::Text(text) | ModelDelta::Thinking(text) => {
                    progress.output_bytes = progress.output_bytes.saturating_add(text.len());
                    progress.output_bytes >= limit as usize
                }
                ModelDelta::Usage(usage) => usage
                    .output_tokens
                    .is_some_and(|tokens| tokens >= u64::from(limit)),
                ModelDelta::ToolUseStarted { .. } | ModelDelta::OutputStarted => false,
            }
        } else {
            false
        };
        match delta {
            ModelDelta::OutputStarted => progress.output_started = true,
            ModelDelta::Usage(usage) => {
                let (effect, charged) = self
                    .change_request_usage(context.owner.chat_id, |ledger| {
                        let previous = ledger
                            .entries()
                            .iter()
                            .find(|record| record.sequence == sequence)
                            .map_or(0, |record| crate::goal::charged_tokens(&record.usage));
                        let effect = ledger.observe(sequence, usage)?;
                        let current = ledger
                            .entries()
                            .iter()
                            .find(|record| record.sequence == sequence)
                            .map_or(0, |record| crate::goal::charged_tokens(&record.usage));
                        Ok((effect, current.saturating_sub(previous)))
                    })
                    .await?;
                progress.regression = progress.regression.or(effect.regression);
                self.charge_goal_tokens(context.owner.chat_id, charged)
                    .await;
            }
            ModelDelta::Text(text) if display_text => {
                progress.output_started = true;
                if let Some(receipt) = context.conversation_receipt() {
                    let mut chats = self.inner.chats.lock().await;
                    let chat = chats
                        .get_mut(&receipt.chat_id)
                        .ok_or(AgentError::NotFound)?;
                    chat.snapshot
                        .request_usage
                        .add_context_output_bytes(sequence, text.len())?;
                    chat.snapshot.last_sequence += 1;
                    let event = Event {
                        sequence: chat.snapshot.last_sequence,
                        chat_id: receipt.chat_id,
                        loop_id: receipt.loop_id,
                        run_id: receipt.run_id,
                        kind: EventKind::Text(text.clone()),
                    };
                    chat.snapshot.events.push(event.clone());
                    if chat.snapshot.events.len() > super::EVENT_REPLAY_LIMIT {
                        chat.snapshot.events.remove(0);
                    }
                    progress.displayed_text.push_str(&text);
                    let _ = self.inner.events.send(event);
                }
            }
            ModelDelta::Thinking(thinking) if display_text => {
                progress.output_started = true;
                if let Some(receipt) = context.conversation_receipt() {
                    let mut chats = self.inner.chats.lock().await;
                    let chat = chats
                        .get_mut(&receipt.chat_id)
                        .ok_or(AgentError::NotFound)?;
                    chat.snapshot
                        .request_usage
                        .add_context_output_bytes(sequence, thinking.len())?;
                    chat.snapshot
                        .pending_thinking
                        .entry(receipt.run_id)
                        .or_default()
                        .push_str(&thinking);
                    chat.snapshot.last_sequence += 1;
                    let event = Event {
                        sequence: chat.snapshot.last_sequence,
                        chat_id: receipt.chat_id,
                        loop_id: receipt.loop_id,
                        run_id: receipt.run_id,
                        kind: EventKind::Thinking(thinking),
                    };
                    chat.snapshot.events.push(event.clone());
                    if chat.snapshot.events.len() > super::EVENT_REPLAY_LIMIT {
                        chat.snapshot.events.remove(0);
                    }
                    let _ = self.inner.events.send(event);
                }
            }
            ModelDelta::ToolUseStarted { id, name } if display_text => {
                progress.output_started = true;
                if let Some(receipt) = context.conversation_receipt() {
                    let mut chats = self.inner.chats.lock().await;
                    let chat = chats
                        .get_mut(&receipt.chat_id)
                        .ok_or(AgentError::NotFound)?;
                    chat.snapshot.last_sequence += 1;
                    let event = Event {
                        sequence: chat.snapshot.last_sequence,
                        chat_id: receipt.chat_id,
                        loop_id: receipt.loop_id,
                        run_id: receipt.run_id,
                        kind: EventKind::ToolUseStarted { call_id: id, name },
                    };
                    chat.snapshot.events.push(event.clone());
                    if chat.snapshot.events.len() > super::EVENT_REPLAY_LIMIT {
                        chat.snapshot.events.remove(0);
                    }
                    let _ = self.inner.events.send(event);
                }
            }
            ModelDelta::Text(_) | ModelDelta::Thinking(_) | ModelDelta::ToolUseStarted { .. } => {
                progress.output_started = true
            }
        }
        if output_exhausted {
            return Err(ModelError::ResponseRejected(ModelResponseRejection::OutputLimit).into());
        }
        Ok(())
    }

    /// Following Claude Code: when the user interrupts or stops, text already streamed is kept as
    /// an assistant message; requests cancelled for other reasons, such as the Loop expiring or the
    /// host shutting down, are not kept.
    pub(super) async fn commit_interrupted_reply(
        &self,
        receipt: &Receipt,
        text: String,
    ) -> Result<(), AgentError> {
        if text.trim().is_empty() || self.inner.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or(AgentError::NotFound)?;
        if !chat.user_cancelled_runs.contains(&receipt.run_id) {
            return Ok(());
        }
        let snapshot = &mut chat.snapshot;
        snapshot
            .message_runs
            .insert(snapshot.messages.len(), receipt.run_id);
        snapshot
            .message_models
            .insert(snapshot.messages.len(), snapshot.model.identity());
        snapshot.messages.push(Message {
            role: Role::Assistant,
            content: vec![Content::Text { text }],
        });
        snapshot.last_model_run = Some(receipt.run_id);
        crate::lifecycle::claim_response_messages(chat);
        self.save(&chat.snapshot);
        Ok(())
    }

    /// One permit of each of the host's request gates, taken in order, for a single request to
    /// a model's vendor and its stream: whoever waits here holds no other permit, so requests
    /// that wait on each other, such as a compaction's within a Run's, cannot deadlock.
    pub(super) async fn vendor_request_permits(
        &self,
    ) -> Result<Vec<tokio::sync::OwnedSemaphorePermit>, ModelError> {
        let mut permits = Vec::with_capacity(self.inner.request_gates.len());
        for gate in &self.inner.request_gates {
            // A gate the host closed admits no further request.
            permits.push(
                gate.clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| ModelError::Cancelled)?,
            );
        }
        Ok(permits)
    }

    /// The handover that stops a request of `context` before it reaches the vendor: a Run's
    /// conversation request stops at a handover, any other request never does.
    fn handover_of(&self, context: &RequestContext) -> CancellationToken {
        if context.kind == ModelRequestKind::Conversation && context.owner.run_id.is_some() {
            self.inner.handover.clone()
        } else {
            CancellationToken::new()
        }
    }

    pub(super) async fn tracked_model_request(
        &self,
        context: RequestContext,
        client: Arc<dyn ModelClient>,
        request: ModelRequest,
        cancel: CancellationToken,
        timeout: Option<Duration>,
    ) -> Result<PendingModelResponse, AgentError> {
        self.tracked_model_request_capturing(context, client, request, cancel, timeout, None)
            .await
    }

    pub(super) async fn tracked_model_request_capturing(
        &self,
        context: RequestContext,
        client: Arc<dyn ModelClient>,
        request: ModelRequest,
        cancel: CancellationToken,
        timeout: Option<Duration>,
        captured_text: Option<&mut String>,
    ) -> Result<PendingModelResponse, AgentError> {
        let timeout = tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(context.kind)
            .map(|task| {
                timeout.map_or(task.budget().timeout(), |timeout| {
                    timeout.min(task.budget().timeout())
                })
            })
            .or(timeout);
        let client = if context.kind != ModelRequestKind::PermissionCheck
            && let Some(task) =
                tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(context.kind)
        {
            client.auxiliary_client(task)?.unwrap_or(client)
        } else {
            client
        };
        self.tracked_model_request_retrying(
            context,
            client,
            request,
            cancel,
            RetryTracking {
                timeout,
                captured_text,
                first_sequence: None,
            },
        )
        .await
    }

    pub(super) async fn tracked_model_request_started_retrying(
        &self,
        context: RequestContext,
        sequence: RequestSequence,
        client: Arc<dyn ModelClient>,
        request: ModelRequest,
        cancel: CancellationToken,
        timeout: Option<Duration>,
    ) -> Result<PendingModelResponse, AgentError> {
        self.tracked_model_request_retrying(
            context,
            client,
            request,
            cancel,
            RetryTracking {
                timeout,
                captured_text: None,
                first_sequence: Some(sequence),
            },
        )
        .await
    }

    async fn tracked_model_request_retrying(
        &self,
        context: RequestContext,
        client: Arc<dyn ModelClient>,
        request: ModelRequest,
        cancel: CancellationToken,
        mut tracking: RetryTracking<'_>,
    ) -> Result<PendingModelResponse, AgentError> {
        let _upgrade = self.inner.upgrade_gate.read().await;
        let mut retries = 0;
        let max_attempts = if context.kind == ModelRequestKind::PermissionCheck {
            MAX_PERMISSION_CHECK_RETRIES + 1
        } else {
            MAX_MODEL_ERROR_RETRIES + 1
        };
        let model_context = ModelRequestContext::new(max_attempts);
        let deadline = tracking
            .timeout
            .filter(|_| {
                tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(context.kind).is_some()
            })
            .map(|timeout| tokio::time::Instant::now() + timeout);
        loop {
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                let (error, outcome) = if cancel.is_cancelled() {
                    (ModelError::Cancelled, RequestOutcome::Cancelled)
                } else {
                    (ModelError::TimedOut, RequestOutcome::TimedOut)
                };
                if let Some(sequence) = tracking.first_sequence.take() {
                    self.finish_model_request(context.owner.chat_id, sequence, outcome)
                        .await?;
                }
                return Err(error.into());
            }
            let sequence = if let Some(sequence) = tracking.first_sequence.take() {
                sequence
            } else {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats
                    .get_mut(&context.owner.chat_id)
                    .ok_or(AgentError::NotFound)?;
                if cancel.is_cancelled()
                    || context
                        .owner
                        .run_id
                        .is_some_and(|run| chat.active_run != Some(run))
                {
                    return Err(ModelError::Cancelled.into());
                }
                start_tracked_model_request(&mut chat.snapshot, context, client.identity())?
            };
            let mut attempt_text = String::new();
            let mut output_started = false;
            if !model_context.try_begin_attempt() {
                return Err(ModelError::ProviderFailure.into());
            }
            let result = self
                .tracked_model_request_started_capturing(
                    context,
                    sequence,
                    client.clone(),
                    request.clone(),
                    cancel.clone(),
                    RequestTracking {
                        timeout: deadline
                            .map(|deadline| {
                                deadline.saturating_duration_since(tokio::time::Instant::now())
                            })
                            .or(tracking.timeout),
                        captured_text: Some(&mut attempt_text),
                        output_started: Some(&mut output_started),
                        model_context: Some(model_context.clone()),
                    },
                )
                .await;
            if let Some(text) = tracking.captured_text.as_mut() {
                text.push_str(&attempt_text);
            }
            // Once reply text was shown, the Run continues from that text instead of replaying.
            let retry = match &result {
                Err(AgentError::Model(error))
                    if (!output_started
                        || (error.kind() == ModelFailureKind::StreamInterrupted
                            && attempt_text.is_empty()))
                        && model_context.has_remaining_attempts() =>
                {
                    model_error_retry_delay(error, model_context.used_attempts() - 1, context.kind)
                        .map(|(delay, max_retries)| (delay, max_retries, retry_error_label(error)))
                }
                _ => None,
            };
            if let Some((delay, max_retries, error)) = retry {
                let delay = deadline.map_or(delay, |deadline| {
                    delay.min(deadline.saturating_duration_since(tokio::time::Instant::now()))
                });
                if let Some(receipt) = context.retry_receipt() {
                    let mut chats = self.inner.chats.lock().await;
                    let chat = chats
                        .get_mut(&receipt.chat_id)
                        .ok_or(AgentError::NotFound)?;
                    if chat.active_run != Some(receipt.run_id) || cancel.is_cancelled() {
                        return Err(ModelError::Cancelled.into());
                    }
                    if context.kind == ModelRequestKind::Conversation {
                        chat.snapshot.pending_thinking.remove(&receipt.run_id);
                    }
                    self.emit(
                        chat,
                        &receipt,
                        EventKind::ModelRetry {
                            kind: context.kind,
                            error,
                            attempt: retries + 1,
                            max_retries,
                            delay_ms: delay.as_millis() as u64,
                        },
                    );
                }
                // A Run's retry backoff is a safe point for a handover.
                let handover = self.handover_of(&context);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled.into()),
                    _ = handover.cancelled() => return Err(ModelError::Cancelled.into()),
                    _ = tokio::time::sleep(delay) => {}
                }
                retries += 1;
                continue;
            }
            if tracking.captured_text.is_none()
                && matches!(&result, Err(AgentError::Model(ModelError::Cancelled)))
                && let Some(receipt) = context.conversation_receipt()
            {
                self.commit_interrupted_reply(&receipt, attempt_text)
                    .await?;
            }
            return result;
        }
    }

    async fn tracked_model_request_started_capturing(
        &self,
        context: RequestContext,
        sequence: RequestSequence,
        client: Arc<dyn ModelClient>,
        mut request: ModelRequest,
        cancel: CancellationToken,
        tracking: RequestTracking<'_>,
    ) -> Result<PendingModelResponse, AgentError> {
        if tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(context.kind).is_some() {
            request.reasoning_effort = client.minimum_reasoning_effort();
            request.temperature = None;
        }
        let request_bytes = (context.kind == ModelRequestKind::Conversation).then(|| {
            let messages = if request
                .messages
                .first()
                .is_some_and(|message| message.role == Role::System)
            {
                &request.messages[1..]
            } else {
                &request.messages
            };
            super::context::serialized_request_bytes(messages, &request.tools)
        });
        let was_cancelled = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&context.owner.chat_id)
                .ok_or(AgentError::NotFound)?;
            chat.snapshot.request_usage.validate_in_flight(
                sequence,
                context.owner,
                context.kind,
                &client.identity(),
            )?;
            let was_cancelled = cancel.is_cancelled()
                || context
                    .owner
                    .run_id
                    .is_some_and(|run| chat.active_run != Some(run));
            if !was_cancelled && let Some(bytes) = request_bytes {
                let measurement = tickloop_agent_protocol::ContextMeasurement::capture(
                    &chat.snapshot,
                    super::context::serialized_request_bytes(&request.messages, &request.tools),
                    super::context::serialized_request_bytes(
                        &request.messages[..usize::from(
                            request
                                .messages
                                .first()
                                .is_some_and(|message| message.role == Role::System),
                        )],
                        &request.tools,
                    ),
                );
                chat.snapshot
                    .request_usage
                    .record_serialized_request_bytes(sequence, bytes)?;
                chat.snapshot
                    .request_usage
                    .record_context_measurement(sequence, measurement)?;
            }
            was_cancelled
        };
        if was_cancelled {
            self.finish_model_request(context.owner.chat_id, sequence, RequestOutcome::Cancelled)
                .await?;
            return Err(ModelError::Cancelled.into());
        }
        // Waiting at the host's gates spends none of the request's time. A Run's conversation
        // request sends nothing once a handover began: waiting here is a safe point.
        let handover = self.handover_of(&context);
        let permits = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ModelError::Cancelled),
            _ = handover.cancelled() => Err(ModelError::Cancelled),
            permits = self.vendor_request_permits() => permits,
        };
        let _permits = match permits {
            Ok(permits) => permits,
            Err(error) => {
                self.finish_model_request(
                    context.owner.chat_id,
                    sequence,
                    RequestOutcome::Cancelled,
                )
                .await?;
                return Err(error.into());
            }
        };
        let request_cancel = cancel.child_token();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut progress = RequestProgress::default();
        // The scope destroys the actual request future before the receiver is closed and drained.
        let mut result = {
            let response = async {
                // A Shell description is a short side label like a title, on the same channel.
                if matches!(
                    context.kind,
                    ModelRequestKind::Title | ModelRequestKind::ShellDescription
                ) {
                    return client
                        .generate_title(request, sender, request_cancel.clone())
                        .await;
                }
                let mut model_context = tracking.model_context;
                if let Some(task) =
                    tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(context.kind)
                {
                    model_context = Some(
                        model_context
                            .unwrap_or_else(|| ModelRequestContext::new(1))
                            .with_output_token_limit(task.budget().output_tokens()),
                    );
                }
                if matches!(
                    context.kind,
                    ModelRequestKind::Conversation
                        | ModelRequestKind::CompactionAutomatic
                        | ModelRequestKind::CompactionManual
                        | ModelRequestKind::CompactionDirected
                ) {
                    let chats = self.inner.chats.lock().await;
                    if let Some(chat) = chats.get(&context.owner.chat_id)
                        && let Some(limit) = super::context::request_output_token_limit(
                            &chat.snapshot,
                            &request,
                            context.kind,
                        )?
                    {
                        model_context = Some(
                            model_context
                                .unwrap_or_else(|| ModelRequestContext::new(1))
                                .with_output_token_limit(limit),
                        );
                    }
                }
                match model_context {
                    Some(model_context) => {
                        client
                            .complete_for_chat_with_context(
                                context.owner.chat_id,
                                request,
                                sender,
                                request_cancel.clone(),
                                model_context,
                            )
                            .await
                    }
                    None => {
                        client
                            .complete_for_chat(
                                context.owner.chat_id,
                                request,
                                sender,
                                request_cancel.clone(),
                            )
                            .await
                    }
                }
            };
            let deadline = async {
                match tracking.timeout {
                    Some(duration) => tokio::time::sleep(duration).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(response, deadline);
            loop {
                let flush_deadline = progress.delta_deadline;
                let flush = async {
                    match flush_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    biased;
                    output = &mut response => break output.map_err(AgentError::from),
                    _ = cancel.cancelled() => break Err(ModelError::Cancelled.into()),
                    _ = &mut deadline => break Err(ModelError::TimedOut.into()),
                    _ = flush => {
                        if let Err(error) = self.flush_model_delta(&context, sequence, &cancel, &mut progress).await {
                            break Err(error);
                        }
                    }
                    Some(delta) = receiver.recv() => {
                        if let Err(error) = self.buffer_model_delta(&context, sequence, delta,
                            &cancel, &mut progress).await {
                            break Err(error);
                        }
                    }
                }
            }
        };
        request_cancel.cancel();
        receiver.close();
        while let Ok(delta) = receiver.try_recv() {
            if let Err(error) = self
                .buffer_model_delta(&context, sequence, delta, &cancel, &mut progress)
                .await
            {
                result = Err(error);
            }
        }
        if let Err(error) = self
            .flush_model_delta(&context, sequence, &cancel, &mut progress)
            .await
        {
            result = Err(error);
        }
        if let Ok(output) = &result
            && let Err(error) = self
                .record_model_delta(
                    &context,
                    sequence,
                    ModelDelta::Usage(output.usage.clone()),
                    false,
                    &mut progress,
                )
                .await
        {
            result = Err(error);
        }
        if let Ok(output) = &result
            && let Some(task) =
                tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(context.kind)
        {
            let bytes: usize = output
                .content
                .iter()
                .map(|content| match content {
                    Content::Text { text } => text.len(),
                    Content::Thinking { thinking, .. } => thinking.len(),
                    _ => 0,
                })
                .sum();
            if bytes >= task.budget().output_tokens() as usize {
                result =
                    Err(ModelError::ResponseRejected(ModelResponseRejection::OutputLimit).into());
            }
        }
        let persistence_failed = matches!(
            &result,
            Err(AgentError::Storage(_) | AgentError::Usage(_) | AgentError::NotFound)
        );
        if !persistence_failed {
            let outcome = if let Some(counter) = progress.regression {
                result = Err(ModelError::InvalidResponse(format!(
                    "service-reported usage decreased for {counter:?}"
                ))
                .into());
                Some(RequestOutcome::Failed(RequestFailure::Protocol(
                    ProtocolFailure::UsageRegression(counter),
                )))
            } else if let Err(AgentError::Model(error)) = &result {
                Some(model_failure_outcome(error))
            } else {
                None
            };
            if let Some(outcome) = outcome
                && let Err(error) = self
                    .finish_model_request(context.owner.chat_id, sequence, outcome)
                    .await
            {
                result = Err(error);
            }
        }

        if let Some(connection) = client.connection_id() {
            let mut failures = self.inner.provider_failures.lock().unwrap();
            match &result {
                Err(AgentError::Model(error)) => {
                    if let Some(failure) = error.provider_failure() {
                        failures.insert(connection, failure.clone());
                    }
                }
                Ok(_) => {
                    failures.remove(&connection);
                }
                _ => {}
            }
        }
        let caller_captures_text = tracking.captured_text.is_some();
        if let Some(output_started) = tracking.output_started {
            *output_started = progress.output_started;
        }
        if let Some(captured_text) = tracking.captured_text {
            captured_text.push_str(&progress.displayed_text);
        }
        match result {
            Ok(output) => Ok(PendingModelResponse { sequence, output }),
            Err(error) => {
                if !caller_captures_text
                    && matches!(error, AgentError::Model(ModelError::Cancelled))
                    && let Some(receipt) = context.conversation_receipt()
                {
                    self.commit_interrupted_reply(&receipt, progress.displayed_text)
                        .await?;
                }
                Err(error)
            }
        }
    }
}

fn retry_error_label(error: &ModelError) -> String {
    if let Some(failure) = error.provider_failure() {
        return failure.to_string();
    }
    if let Some(status) = error.status() {
        return format!("HTTP {status}");
    }
    match error.kind() {
        ModelFailureKind::RateLimited => "rate limited".into(),
        ModelFailureKind::Connection => "connection lost".into(),
        ModelFailureKind::StreamInterrupted => "stream interrupted".into(),
        ModelFailureKind::Timeout => "timed out".into(),
        _ => "model unavailable".into(),
    }
}

fn model_error_retry_delay(
    error: &ModelError,
    retries: usize,
    kind: ModelRequestKind,
) -> Option<(Duration, usize)> {
    // A stream cut after its output began ran no tool and may be replayed, except an Auto review,
    // which fails closed.
    let (is_retryable, retry_after) = if error.kind() == ModelFailureKind::StreamInterrupted
        && kind != ModelRequestKind::PermissionCheck
    {
        (true, None)
    } else {
        error.retry_hint()
    };
    let is_overload = error.status() == Some(529);
    let max_retries = if kind == ModelRequestKind::PermissionCheck && is_overload {
        0
    } else if is_overload {
        MAX_OVERLOAD_ATTEMPTS - 1
    } else if kind == ModelRequestKind::PermissionCheck {
        MAX_PERMISSION_CHECK_RETRIES
    } else {
        MAX_MODEL_ERROR_RETRIES
    };
    if !is_retryable || retries >= max_retries {
        return None;
    }
    let multiplier = 1u32 << retries.min(6);
    let base = BASE_RETRY_DELAY
        .saturating_mul(multiplier)
        .min(MAX_RETRY_DELAY);
    let jitter = Duration::from_millis(fastrand::u64(0..=base.as_millis() as u64 / 4));
    Some((retry_after.unwrap_or(base + jitter), max_retries))
}

pub(super) fn model_failure_outcome(error: &ModelError) -> RequestOutcome {
    use tickloop_agent_protocol::ProviderFailureClass;

    let failure = match error {
        ModelError::AtStage { error, .. } => return model_failure_outcome(error),
        ModelError::Cancelled => return RequestOutcome::Cancelled,
        ModelError::TimedOut | ModelError::TimedOutAt { .. } => return RequestOutcome::TimedOut,
        ModelError::ResponseRejected(reason) => {
            return RequestOutcome::HostRejected(match reason {
                ModelResponseRejection::InvalidToolCall
                | ModelResponseRejection::UnexpectedToolOperation => {
                    ResponseRejection::InvalidToolCall
                }
                ModelResponseRejection::MalformedStructuredOutput => {
                    ResponseRejection::InvalidVerdict
                }
                ModelResponseRejection::OutputLimit => ResponseRejection::OutputLimit,
                ModelResponseRejection::ContextLimit => ResponseRejection::ContextLimit,
                ModelResponseRejection::ContentFilter => ResponseRejection::ContentFilter,
                ModelResponseRejection::Refusal => ResponseRejection::Refusal,
                ModelResponseRejection::UnsupportedContinuation => {
                    ResponseRejection::UnsupportedContinuation
                }
            });
        }
        ModelError::Configuration(_) | ModelError::UnsupportedProtocol(_) => {
            RequestFailure::LocalConfiguration
        }
        ModelError::Authentication
        | ModelError::AuthenticationAt { .. }
        | ModelError::CodexNotLoggedIn
        | ModelError::MiniMaxNotLoggedIn
        | ModelError::CopilotNotLoggedIn
        | ModelError::CodexAuthentication
        | ModelError::MiniMaxAuthentication
        | ModelError::CopilotAuthentication => RequestFailure::Authentication,
        ModelError::InsufficientBalance => RequestFailure::Http { status: 402 },
        ModelError::QuotaExhausted { status, .. }
        | ModelError::EntitlementDenied { status, .. } => RequestFailure::Http { status: *status },
        ModelError::RateLimited => RequestFailure::RateLimited,
        ModelError::Connection | ModelError::ConnectionAt { .. } => RequestFailure::Connection,
        ModelError::StreamInterrupted | ModelError::StreamInterruptedAfterOutput => {
            RequestFailure::StreamInterrupted
        }
        ModelError::InvalidResponse(_) => {
            RequestFailure::Protocol(ProtocolFailure::InvalidResponse)
        }
        ModelError::ProviderFailure | ModelError::RequestBudgetExhausted { .. } => {
            RequestFailure::ProviderFailure
        }
        ModelError::Http { status } | ModelError::HttpResponse { status, .. } => {
            if *status == 429 {
                RequestFailure::RateLimited
            } else {
                RequestFailure::Http { status: *status }
            }
        }
        ModelError::ProviderHttp(failure) => match failure.class {
            ProviderFailureClass::Authentication => RequestFailure::Authentication,
            ProviderFailureClass::RateLimit => RequestFailure::RateLimited,
            ProviderFailureClass::UsageLimit
            | ProviderFailureClass::Billing
            | ProviderFailureClass::UsageOrPlan
            | ProviderFailureClass::ModelUnavailable
            | ProviderFailureClass::ServerError
            | ProviderFailureClass::Refused => RequestFailure::Http {
                status: failure.status,
            },
        },
    };
    RequestOutcome::Failed(failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tickloop_agent_protocol::{
        Content, InputModality, ModelConfig, ModelIdentity, Protocol, Usage,
    };

    #[test]
    fn staged_response_rejections_preserve_their_outcome_without_panicking() {
        for reason in [
            ModelResponseRejection::ContextLimit,
            ModelResponseRejection::ContentFilter,
        ] {
            let plain = ModelError::ResponseRejected(reason);
            let expected = model_failure_outcome(&plain);
            let staged = plain.at_stage(tickloop_agent_model::ModelRequestStage::ResponseHeaders);
            assert_eq!(model_failure_outcome(&staged), expected);
        }
    }

    #[test]
    fn transport_timeouts_preserve_their_outcome_without_panicking() {
        use tickloop_agent_model::ModelRequestStage;

        for stage in [
            ModelRequestStage::Connect,
            ModelRequestStage::ResponseHeaders,
            ModelRequestStage::ResponseStream,
        ] {
            for error in [
                ModelError::TimedOutAt { stage },
                ModelError::TimedOutAt { stage }.at_stage(stage),
            ] {
                assert_eq!(model_failure_outcome(&error), RequestOutcome::TimedOut);
            }
        }
    }

    #[test]
    fn retry_policy_distinguishes_transient_and_terminal_errors() {
        let (first_delay, retries) = model_error_retry_delay(
            &ModelError::Http { status: 503 },
            0,
            ModelRequestKind::Conversation,
        )
        .unwrap();
        assert!((500..=625).contains(&first_delay.as_millis()));
        assert_eq!(retries, MAX_MODEL_ERROR_RETRIES);
        assert!(
            model_error_retry_delay(
                &ModelError::StreamInterruptedAfterOutput,
                0,
                ModelRequestKind::Conversation
            )
            .is_some()
        );
        assert!(
            model_error_retry_delay(
                &ModelError::StreamInterruptedAfterOutput,
                0,
                ModelRequestKind::PermissionCheck
            )
            .is_none()
        );
        assert!(
            model_error_retry_delay(
                &ModelError::Http { status: 402 },
                0,
                ModelRequestKind::Conversation
            )
            .is_none()
        );
        assert!(
            model_error_retry_delay(
                &ModelError::Authentication,
                0,
                ModelRequestKind::Conversation
            )
            .is_none()
        );
        assert!(
            model_error_retry_delay(
                &ModelError::ProviderFailure,
                0,
                ModelRequestKind::Conversation
            )
            .is_none()
        );
        assert!(
            model_error_retry_delay(
                &ModelError::Http { status: 529 },
                2,
                ModelRequestKind::Conversation
            )
            .is_none()
        );
        assert!(
            model_error_retry_delay(
                &ModelError::Http { status: 529 },
                0,
                ModelRequestKind::PermissionCheck
            )
            .is_none()
        );
    }

    #[test]
    fn retry_policy_honors_server_headers() {
        let error = ModelError::HttpResponse {
            status: 429,
            description: String::new(),
            retry_after: Some(Duration::from_secs(7)),
            should_retry: Some(true),
        };
        assert_eq!(
            model_error_retry_delay(&error, 0, ModelRequestKind::Conversation),
            Some((Duration::from_secs(7), MAX_MODEL_ERROR_RETRIES))
        );
        let error = ModelError::HttpResponse {
            status: 503,
            description: String::new(),
            retry_after: None,
            should_retry: Some(false),
        };
        assert!(model_error_retry_delay(&error, 0, ModelRequestKind::Conversation).is_none());
    }

    enum Behavior {
        Complete,
        Wait,
        CompleteAndCancel(CancellationToken),
    }

    struct ControlledModel {
        calls: AtomicUsize,
        dropped: AtomicBool,
        behavior: Behavior,
    }

    impl ControlledModel {
        fn new(behavior: Behavior) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                dropped: AtomicBool::new(false),
                behavior,
            })
        }
    }

    struct RequestDrop<'a>(&'a AtomicBool);
    impl Drop for RequestDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl ModelClient for ControlledModel {
        async fn generate_title(
            &self,
            request: tickloop_agent_model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
            Ok(tickloop_agent_model::fixture_title(request))
        }

        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: tickloop_agent_protocol::ModelProvider::Api,
                profile: "usage-test".into(),
                protocol: Protocol::Messages,
                requested_model: "usage-test".into(),
            }
        }

        async fn complete(
            &self,
            _: ModelRequest,
            events: mpsc::UnboundedSender<ModelDelta>,
            _: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            let _guard = RequestDrop(&self.dropped);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let usage = Usage {
                input_tokens: Some(17),
                output_tokens: Some(3),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            };
            events.send(ModelDelta::Usage(usage.clone())).unwrap();
            match &self.behavior {
                Behavior::Wait => std::future::pending::<()>().await,
                Behavior::CompleteAndCancel(cancel) => cancel.cancel(),
                Behavior::Complete => {}
            }
            Ok(ModelOutput {
                private_source: None,
                content: vec![Content::Text {
                    text: "response".into(),
                }],
                usage,
                stop_reason: "end_turn".into(),
            })
        }
    }

    async fn fixture(model: Arc<ControlledModel>) -> (tempfile::TempDir, Agent, ChatId) {
        let directory = tempfile::tempdir().unwrap();
        let config = ModelConfig {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
            name: "usage-test".into(),
            protocol: Protocol::Messages,
            model: "usage-test".into(),
            base_url: "http://127.0.0.1".into(),
            api_key_env: String::new(),
            max_output_tokens: 256,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: String::new(),
            connection: None,
            card: None,
            input_modalities: vec![InputModality::Text],
        };
        let agent = Agent::open_embedded(directory.path().join("state"), config, model).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        (directory, agent, chat)
    }

    #[tokio::test]
    async fn auxiliary_requests_use_the_lowest_supported_effort_and_leave_conversation_unchanged() {
        struct EffortModel {
            supports_off: bool,
            requests: Arc<std::sync::Mutex<Vec<ModelRequest>>>,
            auxiliary: bool,
        }

        #[async_trait::async_trait]
        impl ModelClient for EffortModel {
            fn auxiliary_client(
                &self,
                _: tickloop_agent_model::auxiliary::AuxiliaryTask,
            ) -> Result<Option<Arc<dyn ModelClient>>, ModelError> {
                Ok(Some(Arc::new(Self {
                    supports_off: self.supports_off,
                    requests: self.requests.clone(),
                    auxiliary: true,
                })))
            }

            fn identity(&self) -> ModelIdentity {
                let mut identity = tickloop_agent_test_support::config().identity();
                if self.auxiliary {
                    identity.requested_model = "auxiliary-fixture".into();
                }
                identity
            }

            fn supports_reasoning_effort(
                &self,
                effort: tickloop_agent_protocol::ReasoningEffort,
            ) -> bool {
                use tickloop_agent_protocol::ReasoningEffort;
                matches!(effort, ReasoningEffort::Low | ReasoningEffort::High)
                    || self.supports_off && effort == ReasoningEffort::Off
            }

            async fn complete(
                &self,
                request: ModelRequest,
                _: mpsc::UnboundedSender<ModelDelta>,
                _: CancellationToken,
            ) -> Result<ModelOutput, ModelError> {
                self.requests.lock().unwrap().push(request);
                Ok(ModelOutput {
                    private_source: None,
                    content: vec![Content::Text {
                        text: "response".into(),
                    }],
                    usage: Default::default(),
                    stop_reason: "end_turn".into(),
                })
            }
        }

        for supports_off in [true, false] {
            let model = Arc::new(EffortModel {
                supports_off,
                requests: Default::default(),
                auxiliary: false,
            });
            let directory = tempfile::tempdir().unwrap();
            let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
            let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
            let loop_id = uuid::Uuid::new_v4();
            let run_id = uuid::Uuid::new_v4();
            agent
                .inner
                .chats
                .lock()
                .await
                .get_mut(&chat_id)
                .unwrap()
                .active_run = Some(run_id.into());
            for kind in [
                ModelRequestKind::Conversation,
                ModelRequestKind::Title,
                ModelRequestKind::AutoSetup,
                ModelRequestKind::CompactionAutomatic,
                ModelRequestKind::CompactionManual,
                ModelRequestKind::CompactionDirected,
                ModelRequestKind::PermissionCheck,
                ModelRequestKind::WebFetch,
                ModelRequestKind::ShellDescription,
            ] {
                let run_owned = matches!(
                    kind,
                    ModelRequestKind::Conversation
                        | ModelRequestKind::CompactionAutomatic
                        | ModelRequestKind::PermissionCheck
                        | ModelRequestKind::WebFetch
                );
                let context = RequestContext {
                    owner: RequestOwner {
                        chat_id,
                        loop_id: run_owned.then_some(loop_id.into()),
                        run_id: run_owned.then_some(run_id.into()),
                    },
                    kind,
                };
                let mut request = empty_request();
                request.reasoning_effort = Some(tickloop_agent_protocol::ReasoningEffort::High);
                let response = agent
                    .tracked_model_request(
                        context,
                        model.clone(),
                        request,
                        CancellationToken::new(),
                        None,
                    )
                    .await
                    .unwrap();
                let routed = tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(kind)
                    .is_some()
                    && kind != ModelRequestKind::PermissionCheck;
                let snapshot = agent.snapshot(chat_id).await.unwrap();
                let entry = snapshot
                    .request_usage
                    .entries()
                    .iter()
                    .find(|entry| entry.sequence == response.sequence)
                    .unwrap();
                assert_eq!(
                    entry.model.requested_model,
                    if routed {
                        "auxiliary-fixture"
                    } else {
                        "fixture"
                    }
                );
                let actual = model
                    .requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .reasoning_effort;
                let expected = if tickloop_agent_model::auxiliary::AuxiliaryTask::for_request(kind)
                    .is_none()
                {
                    tickloop_agent_protocol::ReasoningEffort::High
                } else if supports_off {
                    tickloop_agent_protocol::ReasoningEffort::Off
                } else {
                    tickloop_agent_protocol::ReasoningEffort::Low
                };
                assert_eq!(
                    actual,
                    Some(expected),
                    "{kind:?}, supports_off={supports_off}"
                );
                agent
                    .finish_model_request(chat_id, response.sequence, RequestOutcome::Accepted)
                    .await
                    .unwrap();
            }
            agent.shutdown().await;
        }
    }

    fn empty_request() -> ModelRequest {
        ModelRequest {
            history_sources: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            structured_output: None,
            temperature: None,
            reasoning_effort: None,
        }
    }

    async fn persist_prestarted_request(
        agent: &Agent,
        chat_id: ChatId,
        context: RequestContext,
        identity: ModelIdentity,
    ) -> RequestSequence {
        let mut chats = agent.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).unwrap();
        start_tracked_model_request(&mut chat.snapshot, context, identity).unwrap()
    }

    #[tokio::test]
    async fn directed_prestarted_request_reuses_the_atomic_ledger_entry() {
        let model = ControlledModel::new(Behavior::Complete);
        let (_directory, agent, chat_id) = fixture(model.clone()).await;
        let context = RequestContext::directed_compaction(chat_id);
        let sequence = persist_prestarted_request(&agent, chat_id, context, model.identity()).await;

        let response = agent
            .tracked_model_request_started_capturing(
                context,
                sequence,
                model.clone(),
                empty_request(),
                CancellationToken::new(),
                RequestTracking {
                    timeout: None,
                    captured_text: None,
                    output_started: None,
                    model_context: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(response.sequence, sequence);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        agent
            .finish_model_request(chat_id, sequence, RequestOutcome::Accepted)
            .await
            .unwrap();
        let snapshot = agent.snapshot(chat_id).await.unwrap();
        assert_eq!(snapshot.request_usage.entries().len(), 1);
        let record = &snapshot.request_usage.entries()[0];
        assert_eq!(record.kind, ModelRequestKind::CompactionDirected);
        assert_eq!(record.loop_id, None);
        assert_eq!(record.run_id, None);
        assert_eq!(record.outcome, RequestOutcome::Accepted);
        assert_eq!(record.usage.input_tokens, Some(17));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn prestarted_request_rejects_mismatched_context_without_calling_the_model() {
        let model = ControlledModel::new(Behavior::Complete);
        let (_directory, agent, chat_id) = fixture(model.clone()).await;
        let directed = RequestContext::directed_compaction(chat_id);
        let sequence =
            persist_prestarted_request(&agent, chat_id, directed, model.identity()).await;

        let error = agent
            .tracked_model_request_started_capturing(
                RequestContext::manual_compaction(chat_id),
                sequence,
                model.clone(),
                empty_request(),
                CancellationToken::new(),
                RequestTracking {
                    timeout: None,
                    captured_text: None,
                    output_started: None,
                    model_context: None,
                },
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            AgentError::Usage(LedgerError::RequestMismatch(actual)) if actual == sequence
        ));
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            agent
                .snapshot(chat_id)
                .await
                .unwrap()
                .request_usage
                .entries()[0]
                .outcome,
            RequestOutcome::InFlight
        );
        agent
            .finish_model_request(chat_id, sequence, RequestOutcome::Cancelled)
            .await
            .unwrap();
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_prestarted_request_is_terminal_without_calling_the_model() {
        let model = ControlledModel::new(Behavior::Complete);
        let (_directory, agent, chat_id) = fixture(model.clone()).await;
        let context = RequestContext::directed_compaction(chat_id);
        let sequence = persist_prestarted_request(&agent, chat_id, context, model.identity()).await;
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = agent
            .tracked_model_request_started_capturing(
                context,
                sequence,
                model.clone(),
                empty_request(),
                cancel,
                RequestTracking {
                    timeout: None,
                    captured_text: None,
                    output_started: None,
                    model_context: None,
                },
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(error, AgentError::Model(ModelError::Cancelled)));
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            agent
                .snapshot(chat_id)
                .await
                .unwrap()
                .request_usage
                .entries()[0]
                .outcome,
            RequestOutcome::Cancelled
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn timed_out_prestarted_request_retains_usage_and_terminalizes_the_same_entry() {
        let model = ControlledModel::new(Behavior::Wait);
        let (_directory, agent, chat_id) = fixture(model.clone()).await;
        let context = RequestContext::directed_compaction(chat_id);
        let sequence = persist_prestarted_request(&agent, chat_id, context, model.identity()).await;

        let error = agent
            .tracked_model_request_started_capturing(
                context,
                sequence,
                model.clone(),
                empty_request(),
                CancellationToken::new(),
                RequestTracking {
                    timeout: Some(Duration::from_millis(10)),
                    captured_text: None,
                    output_started: None,
                    model_context: None,
                },
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(error, AgentError::Model(ModelError::TimedOut)));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let snapshot = agent.snapshot(chat_id).await.unwrap();
        assert_eq!(snapshot.request_usage.entries().len(), 1);
        assert_eq!(
            snapshot.request_usage.entries()[0].outcome,
            RequestOutcome::TimedOut
        );
        assert_eq!(
            snapshot.request_usage.entries()[0].usage.input_tokens,
            Some(17)
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn start_helper_only_mutates_the_candidate_and_rejects_a_foreign_chat() {
        let model = ControlledModel::new(Behavior::Complete);
        let (_directory, agent, chat_id) = fixture(model.clone()).await;
        let original = agent.snapshot(chat_id).await.unwrap();
        let mut candidate = original.clone();
        let sequence = start_tracked_model_request(
            &mut candidate,
            RequestContext::directed_compaction(chat_id),
            model.identity(),
        )
        .unwrap();
        assert_eq!(sequence, RequestSequence(1));
        assert!(original.request_usage.entries().is_empty());
        assert_eq!(candidate.request_usage.entries().len(), 1);
        assert!(
            agent
                .snapshot(chat_id)
                .await
                .unwrap()
                .request_usage
                .entries()
                .is_empty()
        );

        let mut foreign_candidate = original;
        let error = start_tracked_model_request(
            &mut foreign_candidate,
            RequestContext::directed_compaction(uuid::Uuid::new_v4().into()),
            model.identity(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            AgentError::Usage(LedgerError::ChatMismatch { .. })
        ));
        assert!(foreign_candidate.request_usage.entries().is_empty());
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn local_timeout_drops_the_future_and_retains_reported_usage() {
        let model = ControlledModel::new(Behavior::Wait);
        let (_directory, agent, chat) = fixture(model.clone()).await;
        let context = RequestContext::manual_compaction(chat);
        let sequence = persist_prestarted_request(&agent, chat, context, model.identity()).await;
        let error = agent
            .tracked_model_request_started_capturing(
                context,
                sequence,
                model.clone(),
                empty_request(),
                CancellationToken::new(),
                RequestTracking {
                    timeout: Some(Duration::from_millis(10)),
                    captured_text: None,
                    output_started: None,
                    model_context: None,
                },
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(error, AgentError::Model(ModelError::TimedOut)));
        assert!(model.dropped.load(Ordering::SeqCst));
        let snapshot = agent.snapshot(chat).await.unwrap();
        let entry = &snapshot.request_usage.entries()[0];
        assert_eq!(entry.usage.input_tokens, Some(17));
        assert_eq!(entry.outcome, RequestOutcome::TimedOut);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn completed_response_wins_simultaneous_cancel_and_drains_queued_usage() {
        let cancel = CancellationToken::new();
        let model = ControlledModel::new(Behavior::CompleteAndCancel(cancel.clone()));
        let (_directory, agent, chat) = fixture(model.clone()).await;
        let response = agent
            .tracked_model_request(
                RequestContext::manual_compaction(chat),
                model,
                empty_request(),
                cancel,
                None,
            )
            .await
            .unwrap();
        agent
            .finish_model_request(chat, response.sequence, RequestOutcome::Accepted)
            .await
            .unwrap();
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.request_usage.entries().len(), 1);
        let entry = &snapshot.request_usage.entries()[0];
        assert_eq!(entry.usage.input_tokens, Some(17));
        assert_eq!(entry.outcome, RequestOutcome::Accepted);
        agent.shutdown().await;
    }
}
