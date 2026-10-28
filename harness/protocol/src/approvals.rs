use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AutoBlockKind, AutoReviewerPreference, AutoRule, Usage, VerdictEvidence};

#[derive(Clone, Debug, Default, Serialize)]
pub struct PreparedApprovalFacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation: Option<crate::ExecutionIsolation>,
    pub artifact_publication: Option<crate::ArtifactPublication>,
    pub normalized_input: serde_json::Value,
    pub canonical_path: Option<std::path::PathBuf>,
    pub in_workspace: Option<bool>,
    pub read_only: Option<bool>,
    pub tick_instance: Option<tick_protocol::InstanceRef>,
    pub closes_tick: bool,
    /// Protocol declarations are external data, never evidence of authority or verified effects.
    pub external_declaration: Option<serde_json::Value>,
}

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDiagnostic {
    MissingAuthorization,
    TaskScopeUncertain,
    ApplicabilityUncertain { rule_id: String },
    ClearanceUncertain { rule_id: String },
    JevLowConfidence { question_id: String },
    ReviewerFailure { failure: ApprovalFailureKind },
    DenialLimit,
}

impl std::fmt::Display for ApprovalDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingAuthorization => {
                formatter.write_str("Original user authorization is unavailable")
            }
            Self::TaskScopeUncertain => formatter.write_str("Task scope is uncertain"),
            Self::ApplicabilityUncertain { rule_id } => {
                write!(formatter, "Rule applicability is uncertain: {rule_id}")
            }
            Self::ClearanceUncertain { rule_id } => write!(
                formatter,
                "Authorization or exception is uncertain: {rule_id}"
            ),
            Self::JevLowConfidence { question_id } => write!(
                formatter,
                "Jev confidence or selected probability is below threshold: {question_id}"
            ),
            Self::ReviewerFailure { failure } => write!(formatter, "Reviewer failed: {failure:?}"),
            Self::DenialLimit => {
                formatter.write_str("Consecutive or total automatic denial limit reached")
            }
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<ApprovalStage>,
    pub reviewer: ApprovalReviewer,
    pub elapsed_ms: u64,
    pub outcome: ApprovalAttemptOutcome,
    pub failure: Option<ApprovalFailureKind>,
    pub usage: Option<Usage>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStage {
    RiskScreen,
    Authorization,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalAssessmentDetails {
    #[serde(default)]
    pub diagnostics: Option<Vec<ApprovalDiagnostic>>,
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
