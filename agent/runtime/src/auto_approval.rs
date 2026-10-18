use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use super::approval_context::{ApprovalSnapshot, ReviewEvidence};
use super::auto_rules::RuleVerdict;
use super::jev_review::{review_request, verdict};
use super::{Agent, AgentError, AutoReview, auto_mode, model_request};
use tickloop_agent_model::{
    ModelClient, ModelError, ModelFailureKind, ModelResponseRejection,
    judgment::{JevClient, JevConfig, JudgmentClient},
};
use tickloop_agent_protocol::{
    ApprovalAttemptDetails, ApprovalAttemptOutcome, ApprovalFailureKind, ApprovalReviewer,
    ApprovalReviewerKind, ApprovalStage, AutoReviewerHealth, Content, EventKind, ModelIdentity,
    ModelProvider, ModelRequestKind, Protocol, Receipt, RequestOutcome, ResponseRejection,
};

const MAX_ATTEMPTS: usize = 2;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);
pub(super) const JEV_REVIEW_BUDGET: Duration = Duration::from_secs(20);
const LLM_REVIEW_BUDGET: Duration = Duration::from_secs(45);

#[derive(Clone, Copy)]
struct StageRequest {
    stage: ApprovalStage,
    max_attempts: usize,
    deadline: tokio::time::Instant,
}

/// Attempts left for the authorization stage after screening. The one retry with smaller
/// evidence after a context-limit rejection does not use the shared allowance.
fn remaining_attempts(allowance: usize, screen: &[ApprovalAttemptDetails]) -> usize {
    let used = screen
        .iter()
        .filter(|attempt| attempt.failure != Some(ApprovalFailureKind::ContextLimit))
        .count();
    allowance.saturating_sub(used).max(1)
}

pub(super) fn review_from_rule_verdict(verdict: RuleVerdict) -> AutoReview {
    match verdict {
        RuleVerdict::Allow => AutoReview::Allow,
        RuleVerdict::Block {
            reason,
            kind,
            rule_id,
            evidence,
        } => AutoReview::Block {
            reason,
            kind,
            rule_id,
            evidence,
        },
    }
}

impl Agent {
    pub async fn set_judgment_client(&self, client: Arc<dyn JudgmentClient>) {
        *self.inner.judgment.lock().await = Some(client);
        *self.inner.auto_reviewer.lock().await = None;
        self.inner.auto_health.lock().await.reset();
    }

    pub async fn auto_reviewer_health(&self) -> AutoReviewerHealth {
        self.inner
            .auto_health
            .lock()
            .await
            .status(tokio::time::Instant::now())
    }

    pub(super) async fn judgment_client(
        &self,
    ) -> Result<
        (
            Arc<dyn JudgmentClient>,
            Option<tickloop_agent_model::judgment::JevConnectionVersion>,
        ),
        ModelError,
    > {
        if let Some(client) = self.inner.judgment.lock().await.as_ref() {
            return Ok((client.clone(), None));
        }
        let settings = self.inner.settings.clone();
        let client = tokio::task::spawn_blocking(move || {
            let config = settings
                .as_ref()
                .map(|settings| settings.load().jev_config())
                .transpose()
                .map_err(ModelError::Configuration)?
                .unwrap_or_else(JevConfig::default);
            let root = settings
                .as_ref()
                .and_then(tickloop_agent_config::UserSettings::root)
                .map(ToOwned::to_owned);
            JevClient::bind(config, root)
        })
        .await
        .map_err(|error| ModelError::Configuration(error.to_string()))??;
        let version = client.connection_version();
        Ok((Arc::new(client), version))
    }

    /// Reviews `context` fitted to `budget_bytes`, Jev's estimated snapshot capacity.
    pub(super) async fn review_with_jev(
        &self,
        receipt: &Receipt,
        context: &ApprovalSnapshot,
        budget_bytes: usize,
        client: Arc<dyn JudgmentClient>,
        deadline: tokio::time::Instant,
        cancel: CancellationToken,
    ) -> Result<ReviewerRun, AgentError> {
        let mut evidence = ReviewEvidence::new(context, budget_bytes);
        let screen = self
            .review_jev_stage(
                receipt,
                &mut evidence,
                client.clone(),
                cancel.clone(),
                StageRequest {
                    stage: ApprovalStage::RiskScreen,
                    max_attempts: MAX_ATTEMPTS,
                    deadline,
                },
            )
            .await?;
        if !matches!(screen.review, AutoReview::Block { .. })
            && screen.failure != Some(ApprovalFailureKind::InvalidResponse)
        {
            return Ok(screen);
        }
        let final_review = self
            .review_jev_stage(
                receipt,
                &mut evidence,
                client,
                cancel,
                StageRequest {
                    stage: ApprovalStage::Authorization,
                    max_attempts: remaining_attempts(MAX_ATTEMPTS + 1, &screen.attempts),
                    deadline,
                },
            )
            .await?;
        Ok(final_review.with_prior_attempts(screen.attempts))
    }

    /// Reviews `context` fitted to `budget_bytes`, the LLM's estimated snapshot capacity.
    pub(super) async fn review_with_llm(
        &self,
        receipt: &Receipt,
        context: &ApprovalSnapshot,
        budget_bytes: usize,
        client: Arc<dyn ModelClient>,
        cancel: CancellationToken,
    ) -> Result<ReviewerRun, AgentError> {
        let deadline = tokio::time::Instant::now() + LLM_REVIEW_BUDGET;
        let mut evidence = ReviewEvidence::new(context, budget_bytes);
        let screen = self
            .review_llm_stage(
                receipt,
                &mut evidence,
                client.clone(),
                cancel.clone(),
                StageRequest {
                    stage: ApprovalStage::RiskScreen,
                    max_attempts: auto_mode::MAX_CLASSIFIER_ATTEMPTS,
                    deadline,
                },
            )
            .await?;
        if !matches!(screen.review, AutoReview::Block { .. }) && !screen.exhausted_format_retries {
            return Ok(screen);
        }
        let final_review = self
            .review_llm_stage(
                receipt,
                &mut evidence,
                client,
                cancel,
                StageRequest {
                    stage: ApprovalStage::Authorization,
                    max_attempts: remaining_attempts(
                        auto_mode::MAX_CLASSIFIER_ATTEMPTS + 1,
                        &screen.attempts,
                    ),
                    deadline,
                },
            )
            .await?;
        Ok(final_review.with_prior_attempts(screen.attempts))
    }

    /// One Jev stage; a context-limit rejection retries it once with smaller evidence.
    async fn review_jev_stage(
        &self,
        receipt: &Receipt,
        evidence: &mut ReviewEvidence<'_>,
        client: Arc<dyn JudgmentClient>,
        cancel: CancellationToken,
        stage_request: StageRequest,
    ) -> Result<ReviewerRun, AgentError> {
        let run = self
            .review_jev_attempts(
                receipt,
                evidence.snapshot(),
                client.clone(),
                cancel.clone(),
                stage_request,
            )
            .await?;
        if !matches!(run.review, AutoReview::ContextLimit) || !evidence.shrink() {
            return Ok(run);
        }
        let retry = self
            .review_jev_attempts(receipt, evidence.snapshot(), client, cancel, stage_request)
            .await?;
        Ok(retry.with_prior_attempts(run.attempts))
    }

    /// One LLM stage; a context-limit rejection retries it once with smaller evidence.
    async fn review_llm_stage(
        &self,
        receipt: &Receipt,
        evidence: &mut ReviewEvidence<'_>,
        client: Arc<dyn ModelClient>,
        cancel: CancellationToken,
        stage_request: StageRequest,
    ) -> Result<ReviewerRun, AgentError> {
        let run = self
            .review_llm_attempts(
                receipt,
                evidence.snapshot(),
                client.clone(),
                cancel.clone(),
                stage_request,
            )
            .await?;
        if !matches!(run.review, AutoReview::ContextLimit) || !evidence.shrink() {
            return Ok(run);
        }
        let retry = self
            .review_llm_attempts(receipt, evidence.snapshot(), client, cancel, stage_request)
            .await?;
        Ok(retry.with_prior_attempts(run.attempts))
    }

    async fn review_jev_attempts(
        &self,
        receipt: &Receipt,
        context: &ApprovalSnapshot,
        client: Arc<dyn JudgmentClient>,
        cancel: CancellationToken,
        stage_request: StageRequest,
    ) -> Result<ReviewerRun, AgentError> {
        let StageRequest {
            stage,
            max_attempts,
            deadline,
        } = stage_request;
        let reviewer = ApprovalReviewer {
            kind: ApprovalReviewerKind::Jev,
            requested_model: client.model().into(),
            reported_model: None,
        };
        let request = match review_request(context, stage) {
            Ok(request) => request,
            Err(AutoReview::ContextLimit) => {
                return Ok(ReviewerRun::technical(
                    AutoReview::ContextLimit,
                    reviewer,
                    ApprovalFailureKind::ContextLimit,
                    Vec::new(),
                ));
            }
            Err(_) => unreachable!(),
        };
        let mut attempts = Vec::new();
        for attempt_index in 0..max_attempts {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled.into());
            }
            let sequence = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats
                    .get_mut(&receipt.chat_id)
                    .ok_or(AgentError::NotFound)?;
                if chat.active_run != Some(receipt.run_id) {
                    return Err(ModelError::Cancelled.into());
                }
                model_request::start_tracked_model_request(
                    &mut chat.snapshot,
                    model_request::RequestContext::for_run(
                        receipt,
                        ModelRequestKind::PermissionCheck,
                    ),
                    ModelIdentity {
                        profile: "Jev".into(),
                        protocol: Protocol::SystemOne,
                        requested_model: client.model().into(),
                        provider: ModelProvider::Api,
                    },
                )?
            };
            let started = tokio::time::Instant::now();
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(ModelError::Cancelled.into()),
                result = tokio::time::timeout_at(deadline, client.evaluate(&request, cancel.child_token())) => {
                    result.unwrap_or_else(|_| Err(ModelError::TimedOut.into()))
                },
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let error = match result {
                Ok(evaluation) => {
                    self.change_request_usage(receipt.chat_id, |ledger| {
                        ledger.observe(sequence, evaluation.usage.clone())
                    })
                    .await?;
                    let semantic = evaluation
                        .validate(&request)
                        .map_err(|error| error.to_string())
                        .and_then(|()| verdict(&evaluation, &context.auto_policy));
                    match semantic {
                        Ok(verdict) => {
                            self.finish_model_request(
                                receipt.chat_id,
                                sequence,
                                RequestOutcome::Accepted,
                            )
                            .await?;
                            let review = review_from_rule_verdict(verdict);
                            let result = outcome_for_review(&review);
                            attempts.push(ApprovalAttemptDetails {
                                stage: Some(stage),
                                reviewer: ApprovalReviewer {
                                    reported_model: Some(evaluation.model.clone()),
                                    ..reviewer.clone()
                                },
                                elapsed_ms,
                                outcome: result,
                                failure: None,
                                usage: Some(evaluation.usage),
                            });
                            return Ok(ReviewerRun {
                                exhausted_format_retries: false,
                                review,
                                reviewer: Some(ApprovalReviewer {
                                    reported_model: Some(evaluation.model),
                                    ..reviewer
                                }),
                                attempts,
                                failure: None,
                            });
                        }
                        Err(_) => {
                            self.finish_model_request(
                                receipt.chat_id,
                                sequence,
                                RequestOutcome::HostRejected(ResponseRejection::InvalidVerdict),
                            )
                            .await?;
                            let failure = ApprovalFailureKind::InvalidResponse;
                            attempts.push(ApprovalAttemptDetails {
                                stage: Some(stage),
                                reviewer: reviewer.clone(),
                                elapsed_ms,
                                outcome: ApprovalAttemptOutcome::TechnicalFailure,
                                failure: Some(failure),
                                usage: Some(evaluation.usage),
                            });
                            return Ok(ReviewerRun::technical(
                                AutoReview::Unavailable {
                                    message: "Jev returned an invalid overall Auto decision".into(),
                                },
                                reviewer,
                                failure,
                                attempts,
                            ));
                        }
                    }
                }
                Err(error) => {
                    if let Some(usage) = error.usage.clone() {
                        self.change_request_usage(receipt.chat_id, |ledger| {
                            ledger.observe(sequence, usage)
                        })
                        .await?;
                    }
                    let model_error = error.error;
                    self.finish_model_request(
                        receipt.chat_id,
                        sequence,
                        model_request::model_failure_outcome(&model_error),
                    )
                    .await?;
                    if matches!(model_error, ModelError::Cancelled) {
                        return Err(model_error.into());
                    }
                    let failure = failure_kind(&model_error);
                    attempts.push(ApprovalAttemptDetails {
                        stage: Some(stage),
                        reviewer: reviewer.clone(),
                        elapsed_ms,
                        outcome: ApprovalAttemptOutcome::TechnicalFailure,
                        failure: Some(failure),
                        usage: error.usage,
                    });
                    model_error
                }
            };
            let (retryable, retry_after) = error.retry_hint();
            if retryable && attempt_index + 1 < max_attempts {
                let delay = retry_after.unwrap_or(Duration::from_millis(500));
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if delay < remaining && delay <= MAX_RETRY_DELAY {
                    {
                        let mut chats = self.inner.chats.lock().await;
                        let chat = chats
                            .get_mut(&receipt.chat_id)
                            .ok_or(AgentError::NotFound)?;
                        self.emit(
                            chat,
                            receipt,
                            EventKind::ModelRetry {
                                kind: ModelRequestKind::PermissionCheck,
                                error: format!("Jev: {error}"),
                                attempt: attempt_index + 1,
                                max_retries: max_attempts - 1,
                                delay_ms: delay.as_millis() as u64,
                            },
                        );
                    }
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(ModelError::Cancelled.into()),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
            }
            let failure = failure_kind(&error);
            let review = if failure == ApprovalFailureKind::ContextLimit {
                AutoReview::ContextLimit
            } else {
                AutoReview::Unavailable {
                    message: auto_mode::unavailable_message(
                        &context.operation.tool,
                        client.model(),
                        Some(&error),
                    ),
                }
            };
            return Ok(ReviewerRun::technical(review, reviewer, failure, attempts));
        }
        unreachable!()
    }

    async fn review_llm_attempts(
        &self,
        receipt: &Receipt,
        context: &ApprovalSnapshot,
        client: Arc<dyn ModelClient>,
        cancel: CancellationToken,
        stage_request: StageRequest,
    ) -> Result<ReviewerRun, AgentError> {
        let StageRequest {
            stage,
            max_attempts,
            deadline,
        } = stage_request;
        let reviewer = ApprovalReviewer {
            kind: ApprovalReviewerKind::Llm,
            requested_model: client.identity().requested_model,
            reported_model: None,
        };
        let mut request = auto_mode::classifier_request(context, stage);
        let mut attempts = Vec::new();
        let mut invalid_reason = "invalid classification";
        for _ in 0..max_attempts {
            let started = tokio::time::Instant::now();
            let response = self
                .tracked_model_request(
                    model_request::RequestContext::for_run(
                        receipt,
                        ModelRequestKind::PermissionCheck,
                    ),
                    client.clone(),
                    request.clone(),
                    cancel.clone(),
                    Some(deadline.saturating_duration_since(tokio::time::Instant::now())),
                )
                .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let response = match response {
                Ok(response) => response,
                Err(AgentError::Model(ModelError::Cancelled)) => {
                    return Err(ModelError::Cancelled.into());
                }
                Err(AgentError::Model(error)) => {
                    if error.kind() == ModelFailureKind::Cancelled {
                        return Err(error.into());
                    }
                    let response_failure = auto_mode::response_failure(&error);
                    let failure = if response_failure.is_some_and(|(_, retryable)| retryable) {
                        ApprovalFailureKind::InvalidResponse
                    } else {
                        failure_kind(&error)
                    };
                    attempts.push(ApprovalAttemptDetails {
                        stage: Some(stage),
                        reviewer: reviewer.clone(),
                        elapsed_ms,
                        outcome: ApprovalAttemptOutcome::TechnicalFailure,
                        failure: Some(failure),
                        usage: None,
                    });
                    if let Some((reason, retryable)) = response_failure {
                        if retryable {
                            invalid_reason = reason;
                            request = auto_mode::classifier_retry_request(&request, reason);
                            continue;
                        }
                        return Ok(ReviewerRun::technical(
                            AutoReview::Unavailable {
                                message: format!(
                                    "Auto reviewer {reason}; review this action manually"
                                ),
                            },
                            reviewer,
                            failure,
                            attempts,
                        ));
                    }
                    let review = if failure == ApprovalFailureKind::ContextLimit {
                        AutoReview::ContextLimit
                    } else {
                        AutoReview::Unavailable {
                            message: auto_mode::unavailable_message(
                                &context.operation.tool,
                                &reviewer.requested_model,
                                Some(&error),
                            ),
                        }
                    };
                    return Ok(ReviewerRun::technical(review, reviewer, failure, attempts));
                }
                Err(error) => return Err(error),
            };
            if response.output.content.iter().any(|content| {
                matches!(
                    content,
                    Content::ToolUse { .. } | Content::ToolResult { .. }
                )
            }) {
                self.finish_model_request(
                    receipt.chat_id,
                    response.sequence,
                    RequestOutcome::HostRejected(ResponseRejection::InvalidToolCall),
                )
                .await?;
                attempts.push(ApprovalAttemptDetails {
                    stage: Some(stage),
                    reviewer: reviewer.clone(),
                    elapsed_ms,
                    outcome: ApprovalAttemptOutcome::TechnicalFailure,
                    failure: Some(ApprovalFailureKind::InvalidResponse),
                    usage: Some(response.output.usage),
                });
                return Ok(ReviewerRun::technical(
                    AutoReview::Unavailable { message:"Auto reviewer returned unexpected tool operations; review this action manually".into() },
                    reviewer, ApprovalFailureKind::InvalidResponse, attempts,
                ));
            }
            match auto_mode::parse_verdict(&response.output, &context.auto_policy) {
                Ok(verdict) => {
                    self.finish_model_request(
                        receipt.chat_id,
                        response.sequence,
                        RequestOutcome::Accepted,
                    )
                    .await?;
                    let review = review_from_rule_verdict(verdict);
                    attempts.push(ApprovalAttemptDetails {
                        stage: Some(stage),
                        reviewer: reviewer.clone(),
                        elapsed_ms,
                        outcome: outcome_for_review(&review),
                        failure: None,
                        usage: Some(response.output.usage),
                    });
                    return Ok(ReviewerRun {
                        exhausted_format_retries: false,
                        review,
                        reviewer: Some(reviewer),
                        attempts,
                        failure: None,
                    });
                }
                Err(reason) => {
                    invalid_reason = reason;
                    request = auto_mode::classifier_retry_request(&request, reason);
                    self.finish_model_request(
                        receipt.chat_id,
                        response.sequence,
                        RequestOutcome::HostRejected(ResponseRejection::InvalidVerdict),
                    )
                    .await?;
                    attempts.push(ApprovalAttemptDetails {
                        stage: Some(stage),
                        reviewer: reviewer.clone(),
                        elapsed_ms,
                        outcome: ApprovalAttemptOutcome::TechnicalFailure,
                        failure: Some(ApprovalFailureKind::InvalidResponse),
                        usage: Some(response.output.usage),
                    });
                }
            }
        }
        let mut result = ReviewerRun::technical(
            AutoReview::Unavailable {
                message: format!(
                    "Invalid Auto reviewer response after {} attempts ({invalid_reason}); review this action manually",
                    attempts.len()
                ),
            },
            reviewer,
            ApprovalFailureKind::InvalidResponse,
            attempts,
        );
        result.exhausted_format_retries = true;
        Ok(result)
    }
}

pub(super) struct ReviewerRun {
    exhausted_format_retries: bool,
    pub review: AutoReview,
    pub reviewer: Option<ApprovalReviewer>,
    pub attempts: Vec<ApprovalAttemptDetails>,
    pub failure: Option<ApprovalFailureKind>,
}

impl ReviewerRun {
    fn with_prior_attempts(mut self, mut attempts: Vec<ApprovalAttemptDetails>) -> Self {
        attempts.append(&mut self.attempts);
        self.attempts = attempts;
        self
    }

    pub fn binding_failure(failure: ApprovalFailureKind) -> Self {
        Self {
            exhausted_format_retries: false,
            review: AutoReview::Unavailable {
                message: "Jev Auto review is unavailable".into(),
            },
            reviewer: None,
            attempts: Vec::new(),
            failure: Some(failure),
        }
    }

    fn technical(
        review: AutoReview,
        reviewer: ApprovalReviewer,
        failure: ApprovalFailureKind,
        attempts: Vec<ApprovalAttemptDetails>,
    ) -> Self {
        Self {
            exhausted_format_retries: false,
            review,
            reviewer: Some(reviewer),
            attempts,
            failure: Some(failure),
        }
    }
}

fn outcome_for_review(review: &AutoReview) -> ApprovalAttemptOutcome {
    match review {
        AutoReview::Allow => ApprovalAttemptOutcome::Allow,
        AutoReview::Block { .. } => ApprovalAttemptOutcome::Block,
        AutoReview::Ask { .. } => ApprovalAttemptOutcome::NeedsHuman,
        AutoReview::ContextLimit | AutoReview::Unavailable { .. } => {
            ApprovalAttemptOutcome::TechnicalFailure
        }
    }
}

pub(super) fn failure_kind(error: &ModelError) -> ApprovalFailureKind {
    match error.kind() {
        ModelFailureKind::Configuration => ApprovalFailureKind::Configuration,
        ModelFailureKind::AuthenticationRequired | ModelFailureKind::AuthenticationFailed => {
            ApprovalFailureKind::Authentication
        }
        ModelFailureKind::Connection | ModelFailureKind::StreamInterrupted => {
            ApprovalFailureKind::Connection
        }
        ModelFailureKind::Timeout => ApprovalFailureKind::Timeout,
        ModelFailureKind::InvalidResponse => ApprovalFailureKind::InvalidResponse,
        ModelFailureKind::ResponseRejected if is_context_limit(error) => {
            ApprovalFailureKind::ContextLimit
        }
        _ => ApprovalFailureKind::Service,
    }
}

/// HTTP clients report a context-limit response through the stage that read it.
fn is_context_limit(error: &ModelError) -> bool {
    match error {
        ModelError::AtStage { error, .. } => is_context_limit(error),
        error => matches!(
            error,
            ModelError::ResponseRejected(ModelResponseRejection::ContextLimit)
        ),
    }
}
