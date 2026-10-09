use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::approval_context::{
    APPROVAL_CONTEXT_VERSION, ApprovalSnapshot, REVIEW_TEMPLATE_VERSION,
};
use super::auto_rules::{AutoBlockKind, AutoRule, VerdictEvidence};
use super::{AutoReview, OperationRecord};
use tickloop_agent_protocol::{AutoReviewerPreference, Usage};

pub(super) struct ApprovalAssessment {
    pub verdict: AutoReview,
    pub details: ApprovalAssessmentDetails,
}

impl ApprovalAssessment {
    pub fn new(
        context: &ApprovalSnapshot,
        verdict: AutoReview,
        reviewer: Option<ApprovalReviewer>,
    ) -> Self {
        let facts = verdict_facts(context, &verdict);
        let details = ApprovalAssessmentDetails {
            operation_id: context.operation.id,
            call_id: context.operation.call_id.clone(),
            loop_id: context.operation.loop_id,
            run_id: context.operation.run_id,
            permission_policy_revision: context.permission_policy_revision,
            context_version: context.context_version.into(),
            review_template_version: context.review_template_version.into(),
            reviewer,
            preferred_reviewer: None,
            auto_settings_revision: None,
            attempts: Vec::new(),
            fallback_from: None,
            fallback_failure: None,
            fallback_reason: None,
            block_kind: facts.block_kind,
            block_rule: facts.block_rule,
            block_evidence: facts.block_evidence,
            outcome: facts.outcome,
            reason: facts.reason,
        };
        Self { verdict, details }
    }

    pub fn missing_context(
        operation: &OperationRecord,
        permission_policy_revision: u64,
        missing_reason: String,
    ) -> Self {
        let verdict = AutoReview::Ask {
            reason: missing_reason.clone(),
        };
        let details = ApprovalAssessmentDetails {
            operation_id: operation.id,
            call_id: operation.call_id.clone(),
            loop_id: operation.loop_id,
            run_id: operation.run_id,
            permission_policy_revision,
            context_version: APPROVAL_CONTEXT_VERSION.into(),
            review_template_version: REVIEW_TEMPLATE_VERSION.into(),
            reviewer: None,
            preferred_reviewer: None,
            auto_settings_revision: None,
            attempts: Vec::new(),
            fallback_from: None,
            fallback_failure: None,
            fallback_reason: None,
            block_kind: None,
            block_rule: None,
            block_evidence: None,
            outcome: ApprovalOutcome::NeedsHuman,
            reason: Some(missing_reason),
        };
        Self { verdict, details }
    }

    pub fn with_auto_settings(
        mut self,
        preferred_reviewer: AutoReviewerPreference,
        revision: &tickloop_agent_protocol::AutoSettingsRevision,
    ) -> Self {
        self.details.preferred_reviewer = Some(preferred_reviewer);
        self.details.auto_settings_revision = Some(revision.as_str().into());
        self
    }

    pub fn with_attempts(
        mut self,
        attempts: Vec<ApprovalAttemptDetails>,
        fallback_from: Option<ApprovalReviewerKind>,
        fallback_failure: Option<ApprovalFailureKind>,
    ) -> Self {
        self.details.attempts = attempts;
        self.details.fallback_from = fallback_from;
        self.details.fallback_failure = fallback_failure;
        self
    }

    pub fn with_fallback_reason(mut self, reason: Option<ApprovalFallbackReason>) -> Self {
        self.details.fallback_reason = reason;
        self
    }
}

struct VerdictFacts {
    outcome: ApprovalOutcome,
    reason: Option<String>,
    block_kind: Option<AutoBlockKind>,
    block_rule: Option<AutoRule>,
    block_evidence: Option<VerdictEvidence>,
}

fn verdict_facts(context: &ApprovalSnapshot, verdict: &AutoReview) -> VerdictFacts {
    match verdict {
        AutoReview::Allow => VerdictFacts {
            outcome: ApprovalOutcome::Allow,
            reason: None,
            block_kind: None,
            block_rule: None,
            block_evidence: None,
        },
        AutoReview::Block {
            reason,
            kind,
            rule_id,
            evidence,
        } => VerdictFacts {
            outcome: ApprovalOutcome::Block,
            reason: Some(reason.clone()),
            block_kind: Some(*kind),
            block_rule: rule_id.as_ref().and_then(|id| {
                context
                    .auto_policy
                    .rules
                    .iter()
                    .find(|rule| rule.id == *id)
                    .cloned()
            }),
            block_evidence: Some(evidence.clone()),
        },
        AutoReview::Ask { reason } => VerdictFacts {
            outcome: ApprovalOutcome::NeedsHuman,
            reason: Some(reason.clone()),
            block_kind: None,
            block_rule: None,
            block_evidence: None,
        },
        AutoReview::ContextLimit => VerdictFacts {
            outcome: ApprovalOutcome::ContextLimit,
            reason: None,
            block_kind: None,
            block_rule: None,
            block_evidence: None,
        },
        AutoReview::Unavailable { message } => VerdictFacts {
            outcome: ApprovalOutcome::Unavailable,
            reason: Some(message.clone()),
            block_kind: None,
            block_rule: None,
            block_evidence: None,
        },
    }
}
