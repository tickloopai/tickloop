use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    ChatId, HostCallContext, HostToolAuthority, InputContext, LoopId, LoopState, PermissionPolicy,
    RunId, SessionPermissionDenial, ToolResult, Usage,
};

pub const SEND_AGENT_MESSAGE_TOOL_NAME: &str = "SendMessage";
pub const SUBAGENT_HANDBACK_TOOL_NAME: &str = "SubagentHandback";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubagentReport {
    Pending {
        reminders: u8,
    },
    Delivered {
        message_id: Uuid,
        report: String,
        review: Box<crate::ApprovalAssessmentDetails>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct VerifiedUserRelay {
    pub original_chat_id: ChatId,
    pub original_message_id: Uuid,
    pub original_text: String,
    pub forwarded_verbatim: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilyMessageRejectionCode {
    InvalidTarget,
    InvalidUserRelay,
    StaleTarget,
    OriginInactive,
    DepthLimit,
    ChainLimit,
    SenderRateLimit,
    InboxFull,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FamilyMessageRejection {
    pub code: FamilyMessageRejectionCode,
    pub message: String,
}

impl FamilyMessageRejection {
    pub fn result(self) -> ToolResult {
        ToolResult {
            content: serde_json::json!({"status":"rejected","rejection":self,"retryable":false})
                .to_string(),
            is_error: true,
            output_ref: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentMessageSource {
    pub message_id: Uuid,
    pub call_id: Option<String>,
    pub created_at: u64,
    pub sender_chat_id: ChatId,
    pub sender_loop_id: LoopId,
    pub sender_run_id: RunId,
    pub sender_relationship: String,
    pub sender_label: String,
    pub sender_handle: String,
    pub relay: Option<VerifiedUserRelay>,
    pub cause: MessageCause,
    pub is_result: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MessageCause {
    pub chain_id: Uuid,
    pub depth: u8,
    pub reply_to: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DirectedHumanSource {
    pub message_id: Uuid,
    pub source_chat_id: ChatId,
    pub source_handle: String,
    pub original_submission: String,
    pub original_contexts: Vec<InputContext>,
    pub original_paths: Vec<PathBuf>,
    pub source_session_id: Uuid,
    pub source_view_id: Uuid,
    pub source_display_version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FamilyInputRecord {
    pub message_id: Uuid,
    pub target_chat_id: ChatId,
    pub target_handle: String,
    pub target_run_id: RunId,
    pub submitted_at: u64,
    pub response: Option<String>,
    pub state: Option<LoopState>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentNotificationDelivery {
    Foreground,
    Pending,
    Queued,
    Rejected(FamilyMessageRejection),
}

impl SubagentNotificationDelivery {
    pub fn is_background(&self) -> bool {
        !matches!(self, Self::Foreground)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FamilyCompletion {
    pub child_chat_id: ChatId,
    pub child_loop_id: LoopId,
    pub child_run_id: RunId,
    pub handle: String,
    pub state: LoopState,
    pub result: String,
    pub error: Option<String>,
    pub started_at: Option<u64>,
    pub finished_at: u64,
    pub triggering_message_ids: Vec<Uuid>,
    pub notification: SubagentNotificationDelivery,
    pub usage: Usage,
}

pub const SUBAGENT_TOOL_NAME: &str = "Subagent";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubagentRegistration {
    pub child_chat_id: ChatId,
    pub handle: String,
    pub description: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DelegationCeiling {
    pub workdir: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub policy: PermissionPolicy,
    #[serde(default)]
    pub session_denials: Option<Vec<SessionPermissionDenial>>,
    pub host_context: Option<HostCallContext>,
    /// Per host tool, the authorities of every backend that could serve it when the child was
    /// delegated. Each call must run under one of them; a backend found later never joins. The
    /// Artifact tools record the signed-in Artifact account even while its tools are unavailable;
    /// a child delegated with no account signed in records the first account signed in afterwards.
    pub host_tools: BTreeMap<String, BTreeSet<HostToolAuthority>>,
    #[serde(default)]
    pub tick_instances: Vec<tick_protocol::InstanceRef>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DelegationMetadata {
    pub delegation_id: Uuid,
    pub parent_chat_id: ChatId,
    pub parent_loop_id: LoopId,
    pub parent_run_id: RunId,
    pub parent_call_id: String,
    pub task: String,
    pub ceiling: DelegationCeiling,
    #[serde(default)]
    pub trusted_user_inputs: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_reply_history: Option<Value>,
}
