use std::{sync::Arc, time::Duration};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    Agent, AgentError, ChatId, ChatSnapshot, Event, EventKind, LedgerError, ProtocolFailure,
    Receipt, RequestFailure, RequestKind, RequestOutcome, RequestOwner, RequestSequence,
    RequestUsageLedger, ResponseRejection, UsageCounter,
};
use crate::model::{
    Content, Message, ModelClient, ModelDelta, ModelError, ModelFailureKind, ModelIdentity,
    ModelOutput, ModelRequest, ModelRequestContext, ModelResponseRejection, Role,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RequestContext {
    owner: RequestOwner,
    kind: RequestKind,
}

impl RequestContext {
    pub(super) fn for_run(receipt: &Receipt, kind: RequestKind) -> Self {
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
            kind: RequestKind::Title,
        }
    }

    pub(super) fn manual_compaction(chat_id: ChatId) -> Self {
        Self {
            owner: RequestOwner {
                chat_id,
                loop_id: None,
                run_id: None,
            },
            kind: RequestKind::CompactionManual,
        }
    }

    pub(super) fn directed_compaction(chat_id: ChatId) -> Self {
        Self {
            owner: RequestOwner {
                chat_id,
                loop_id: None,
                run_id: None,
            },
            kind: RequestKind::CompactionDirected,
        }
    }

    fn conversation_receipt(&self) -> Option<Receipt> {
        (self.kind == RequestKind::Conversation).then(|| Receipt {
            chat_id: self.owner.chat_id,
            loop_id: self.owner.loop_id.expect("conversation has a Loop"),
            run_id: self.owner.run_id.expect("conversation has a Run"),
        })
    }

    fn retry_receipt(&self) -> Option<Receipt> {
        matches!(
            self.kind,
            RequestKind::Conversation | RequestKind::PermissionCheck | RequestKind::WebFetch
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
    displayed_text: String,
    output_started: bool,
    regression: Option<UsageCounter>,
}

impl Agent {
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
        match delta {
            ModelDelta::OutputStarted => progress.output_started = true,
            ModelDelta::Usage(usage) => {
                let effect = self
                    .change_request_usage(context.owner.chat_id, |ledger| {
                        ledger.observe(sequence, usage)
                    })
                    .await?;
                progress.regression = progress.regression.or(effect.regression);
            }
            ModelDelta::Text(text) if display_text => {
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
            ModelDelta::Text(_) | ModelDelta::Thinking(_) => progress.output_started = true,
        }
        Ok(())
    }

    /// 照 Claude Code：用户中断或停止时，已经流出的文字保留为一条 assistant 消息；
    /// Loop 到期、宿主关闭等其他原因取消的请求不保留。
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
        self.save(snapshot);
        Ok(())
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
        let mut retries = 0;
        let max_attempts = if context.kind == RequestKind::PermissionCheck {
            MAX_PERMISSION_CHECK_RETRIES + 1
        } else {
            MAX_MODEL_ERROR_RETRIES + 1
        };
        let model_context = ModelRequestContext::new(max_attempts);
        loop {
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
                        timeout: tracking.timeout,
                        captured_text: Some(&mut attempt_text),
                        output_started: Some(&mut output_started),
                        model_context: Some(model_context.clone()),
                    },
                )
                .await;
            if let Some(text) = tracking.captured_text.as_mut() {
                text.push_str(&attempt_text);
            }
            let retry = match &result {
                Err(AgentError::Model(error))
                    if !output_started && model_context.has_remaining_attempts() =>
                {
                    model_error_retry_delay(error, model_context.used_attempts() - 1, context.kind)
                        .map(|(delay, max_retries)| (delay, max_retries, retry_error_label(error)))
                }
                _ => None,
            };
            if let Some((delay, max_retries, error)) = retry {
                if let Some(receipt) = context.retry_receipt() {
                    let mut chats = self.inner.chats.lock().await;
                    let chat = chats
                        .get_mut(&receipt.chat_id)
                        .ok_or(AgentError::NotFound)?;
                    if chat.active_run != Some(receipt.run_id) || cancel.is_cancelled() {
                        return Err(ModelError::Cancelled.into());
                    }
                    if context.kind == RequestKind::Conversation {
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
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled.into()),
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
        request: ModelRequest,
        cancel: CancellationToken,
        tracking: RequestTracking<'_>,
    ) -> Result<PendingModelResponse, AgentError> {
        let request_bytes = (context.kind == RequestKind::Conversation).then(|| {
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
                chat.snapshot
                    .request_usage
                    .record_serialized_request_bytes(sequence, bytes)?;
            }
            was_cancelled
        };
        if was_cancelled {
            self.finish_model_request(context.owner.chat_id, sequence, RequestOutcome::Cancelled)
                .await?;
            return Err(ModelError::Cancelled.into());
        }
        let request_cancel = cancel.child_token();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut progress = RequestProgress::default();
        // The scope destroys the actual request future before the receiver is closed and drained.
        let mut result = {
            let response = async {
                if context.kind == RequestKind::Title {
                    return client
                        .generate_title(request, sender, request_cancel.clone())
                        .await;
                }
                match tracking.model_context {
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
                tokio::select! {
                    biased;
                    output = &mut response => break output.map_err(AgentError::from),
                    _ = cancel.cancelled() => break Err(ModelError::Cancelled.into()),
                    _ = &mut deadline => break Err(ModelError::TimedOut.into()),
                    Some(delta) = receiver.recv() => {
                        if let Err(error) = self.record_model_delta(&context, sequence, delta,
                            !cancel.is_cancelled(), &mut progress).await {
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
                .record_model_delta(
                    &context,
                    sequence,
                    delta,
                    !cancel.is_cancelled(),
                    &mut progress,
                )
                .await
            {
                result = Err(error);
            }
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
    kind: RequestKind,
) -> Option<(Duration, usize)> {
    let (is_retryable, retry_after) = error.retry_hint();
    let is_overload = error.status() == Some(529);
    let max_retries = if kind == RequestKind::PermissionCheck && is_overload {
        0
    } else if is_overload {
        MAX_OVERLOAD_ATTEMPTS - 1
    } else if kind == RequestKind::PermissionCheck {
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
    match error {
        ModelError::Cancelled => RequestOutcome::Cancelled,
        ModelError::TimedOut => RequestOutcome::TimedOut,
        ModelError::ResponseRejected(reason) => RequestOutcome::HostRejected(match reason {
            ModelResponseRejection::InvalidToolCall
            | ModelResponseRejection::UnexpectedToolOperation => ResponseRejection::InvalidToolCall,
            ModelResponseRejection::MalformedStructuredOutput => ResponseRejection::InvalidVerdict,
            ModelResponseRejection::OutputLimit => ResponseRejection::OutputLimit,
            ModelResponseRejection::ContextLimit => ResponseRejection::ContextLimit,
            ModelResponseRejection::ContentFilter => ResponseRejection::ContentFilter,
            ModelResponseRejection::Refusal => ResponseRejection::Refusal,
            ModelResponseRejection::UnsupportedContinuation => {
                ResponseRejection::UnsupportedContinuation
            }
        }),
        _ => RequestOutcome::Failed(match error.kind() {
            ModelFailureKind::Configuration => RequestFailure::LocalConfiguration,
            ModelFailureKind::AuthenticationRequired | ModelFailureKind::AuthenticationFailed => {
                RequestFailure::Authentication
            }
            ModelFailureKind::QuotaExhausted => RequestFailure::Http {
                status: error.status().unwrap_or(402),
            },
            ModelFailureKind::EntitlementDenied => RequestFailure::Http { status: 403 },
            ModelFailureKind::RateLimited => RequestFailure::RateLimited,
            ModelFailureKind::Connection => RequestFailure::Connection,
            ModelFailureKind::StreamInterrupted => RequestFailure::StreamInterrupted,
            ModelFailureKind::InvalidResponse => {
                RequestFailure::Protocol(ProtocolFailure::InvalidResponse)
            }
            ModelFailureKind::ProviderFailure => error
                .status()
                .map_or(RequestFailure::ProviderFailure, |status| {
                    RequestFailure::Http { status }
                }),
            ModelFailureKind::Cancelled
            | ModelFailureKind::Timeout
            | ModelFailureKind::ResponseRejected => unreachable!(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Content, InputModality, ModelConfig, ModelIdentity, Protocol, Usage};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn retry_policy_distinguishes_transient_and_terminal_errors() {
        let (first_delay, retries) = model_error_retry_delay(
            &ModelError::Http { status: 503 },
            0,
            RequestKind::Conversation,
        )
        .unwrap();
        assert!((500..=625).contains(&first_delay.as_millis()));
        assert_eq!(retries, MAX_MODEL_ERROR_RETRIES);
        assert!(
            model_error_retry_delay(
                &ModelError::Http { status: 402 },
                0,
                RequestKind::Conversation
            )
            .is_none()
        );
        assert!(
            model_error_retry_delay(&ModelError::Authentication, 0, RequestKind::Conversation)
                .is_none()
        );
        assert!(
            model_error_retry_delay(&ModelError::ProviderFailure, 0, RequestKind::Conversation)
                .is_none()
        );
        assert!(
            model_error_retry_delay(
                &ModelError::Http { status: 529 },
                2,
                RequestKind::Conversation
            )
            .is_none()
        );
        assert!(
            model_error_retry_delay(
                &ModelError::Http { status: 529 },
                0,
                RequestKind::PermissionCheck
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
            model_error_retry_delay(&error, 0, RequestKind::Conversation),
            Some((Duration::from_secs(7), MAX_MODEL_ERROR_RETRIES))
        );
        let error = ModelError::HttpResponse {
            status: 503,
            description: String::new(),
            retry_after: None,
            should_retry: Some(false),
        };
        assert!(model_error_retry_delay(&error, 0, RequestKind::Conversation).is_none());
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
            request: crate::model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<crate::model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<crate::model::ModelOutput, crate::model::ModelError> {
            Ok(crate::model::fixture_title(request))
        }

        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: crate::model::ModelProvider::Api,
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
            provider: crate::model::ModelProvider::Api,
            auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
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
        assert_eq!(record.kind, RequestKind::CompactionDirected);
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
            RequestContext::directed_compaction(uuid::Uuid::new_v4()),
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
