use super::AutoReview;
use super::approval_context::{
    APPROVAL_CONTEXT_VERSION, ApprovalSnapshot, REVIEW_TEMPLATE_VERSION,
};
use tickloop_agent_protocol::{
    ApprovalAssessmentDetails, ApprovalAttemptDetails, ApprovalDiagnostic, ApprovalFailureKind,
    ApprovalFallbackReason, ApprovalOutcome, ApprovalReviewer, ApprovalReviewerKind, AutoBlockKind,
    AutoReviewerPreference, AutoRule, OperationRecord, VerdictEvidence,
};

pub(super) struct ApprovalAssessment {
    pub verdict: AutoReview,
    pub details: ApprovalAssessmentDetails,
    pub context_metrics: Option<serde_json::Value>,
}

impl ApprovalAssessment {
    pub fn new(
        context: &ApprovalSnapshot,
        verdict: AutoReview,
        reviewer: Option<ApprovalReviewer>,
    ) -> Self {
        let facts = verdict_facts(context, &verdict);
        let details = ApprovalAssessmentDetails {
            diagnostics: Some(match &verdict {
                AutoReview::Ask { diagnostics, .. } => diagnostics.clone(),
                AutoReview::ContextLimit => vec![ApprovalDiagnostic::ReviewerFailure {
                    failure: ApprovalFailureKind::ContextLimit,
                }],
                _ => Vec::new(),
            }),
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
        let context_metrics = Some(serde_json::json!({
            "snapshot_json_bytes": serde_json::to_vec(context).expect("context serializes").len(),
            "tool_history_json_bytes": serde_json::to_vec(&context.tool_history).expect("history serializes").len(),
            "tool_history_calls": context.tool_history.calls.len(),
            "tool_history_complete": context.tool_history.complete,
            "tool_history_compaction_gap": context.tool_history.compaction_gap,
            "operation_input_json_bytes": serde_json::to_vec(&context.operation.input).expect("input serializes").len(),
            "operation_input_complete": !super::approval_context::input_projection::is_incomplete(&context.operation.input),
        }));
        Self {
            verdict,
            details,
            context_metrics,
        }
    }

    pub fn missing_context(
        operation: &OperationRecord,
        permission_policy_revision: u64,
        missing_reason: String,
        diagnostic: ApprovalDiagnostic,
    ) -> Self {
        let verdict = AutoReview::Ask {
            diagnostics: vec![diagnostic.clone()],
            reason: missing_reason.clone(),
        };
        let details = ApprovalAssessmentDetails {
            diagnostics: Some(vec![diagnostic]),
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
        Self {
            verdict,
            details,
            context_metrics: None,
        }
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
        if matches!(self.verdict, AutoReview::Unavailable { .. })
            && let Some(failure) = attempts.iter().rev().find_map(|attempt| attempt.failure)
        {
            self.details.diagnostics = Some(vec![ApprovalDiagnostic::ReviewerFailure { failure }]);
        }
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
            block_evidence: evidence.clone(),
        },
        AutoReview::Ask { reason, .. } => VerdictFacts {
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
