use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AutoBlockKind, AutoReviewerPreference, AutoRule, Usage, VerdictEvidence};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReviewerKind {
    Llm,
    Jev,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalReviewer {
    pub kind: ApprovalReviewerKind,
    pub requested_model: String,
    pub reported_model: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    Allow,
    Block,
    NeedsHuman,
    ContextLimit,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalFailureKind {
    Configuration,
    Authentication,
    Connection,
    Service,
    Timeout,
    InvalidResponse,
    ContextLimit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalFallbackReason {
    TechnicalFailure(ApprovalFailureKind),
    CoolingDown,
    ProbeInProgress,
    ConnectionChanged,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAttemptOutcome {
    Allow,
    Block,
    NeedsHuman,
    TechnicalFailure,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalAttemptDetails {
    pub reviewer: ApprovalReviewer,
    pub elapsed_ms: u64,
    pub outcome: ApprovalAttemptOutcome,
    pub failure: Option<ApprovalFailureKind>,
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalAssessmentDetails {
    pub operation_id: Uuid,
    pub call_id: String,
    pub loop_id: Uuid,
    pub run_id: Uuid,
    pub permission_policy_revision: u64,
    pub context_version: String,
    pub review_template_version: String,
    pub reviewer: Option<ApprovalReviewer>,
    #[serde(default)]
    pub preferred_reviewer: Option<AutoReviewerPreference>,
    #[serde(default)]
    pub auto_settings_revision: Option<String>,
    #[serde(default)]
    pub attempts: Vec<ApprovalAttemptDetails>,
    #[serde(default)]
    pub fallback_from: Option<ApprovalReviewerKind>,
    #[serde(default)]
    pub fallback_failure: Option<ApprovalFailureKind>,
    #[serde(default)]
    pub fallback_reason: Option<ApprovalFallbackReason>,
    #[serde(default)]
    pub block_kind: Option<AutoBlockKind>,
    #[serde(default)]
    pub block_rule: Option<AutoRule>,
    #[serde(default)]
    pub block_evidence: Option<VerdictEvidence>,
    pub outcome: ApprovalOutcome,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OriginalUserInput {
    pub chat_id: Uuid,
    pub loop_id: Uuid,
    pub run_id: Uuid,
    pub message_id: Uuid,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReviewerHealth {
    Ready,
    CoolingDown { remaining_seconds: u64 },
    Probing,
}
