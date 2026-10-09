//! Shared domain types and AgentAccess wire definitions that depend on no other Agent crate.

mod access;
mod approvals;
mod artifact;
mod auto_rules;
mod auto_settings;
mod chat;
mod denials;
mod family;
mod file_change;
mod frame;
mod history;
mod host;
mod hosted;
mod ids;
mod image;
mod input;
mod interaction;
mod limits;
pub mod local_messages;
mod loops;
mod model;
mod model_availability;
mod model_card;
mod monitor;
mod permissions;
mod permissions_view;
mod plan;
pub mod projection;
mod provider_connection;
mod provider_failure;
mod shell_tasks;
mod skill;
mod tick_subscriptions;
mod tool;
mod tools;
mod usage;

pub use access::{
    AccessCapability, AccessError, AccessErrorCode, AfterTurnAction, AgentAccess, AgentCommand,
    AgentDetailQuery, AgentFilter, AgentSummary, ChunkedResponseKind, ClientMessage,
    CommandEnvelope, CommandReceipt, CommandResult, ConfigurationChange, Cursor,
    DEFAULT_PAGE_LIMIT, DisplayReceipt, DisplayUpdate, DisplayedView, EnvironmentValue,
    ExecutionAvailability, ExecutionRef, HistoryAction, HostLocation, HostWorkContext,
    InteractionSource, LaunchSpec, LiveChatSnapshot, LoopAction, LoopRef, MAX_PAGE_LIMIT,
    Observation, ObservationChange, ObservationFailure, ObservationRequest, ObservationUpdate,
    ObservationValue, PROTOCOL_MAJOR, PROTOCOL_MINOR, Page, PendingRequestKind, PersistenceState,
    ProtocolLimits, ProviderCredentialScope, QueuedCommandFailure, RECEIPT_CAPACITY,
    RECEIPT_RETENTION_SECONDS, REPLAY_CAPACITY, ReceiptState, RecordedMessage, RecordedTranscript,
    RequestRef, ResetReason, ServerHello, ServerMessage, TaskAction, TaskOrigin, TaskRef,
    TranscriptRef, UserInput, VIEW_LEASE_SECONDS, ViewRef, bounded_update,
};
pub use approvals::{
    ApprovalAssessmentDetails, ApprovalAttemptDetails, ApprovalAttemptOutcome, ApprovalFailureKind,
    ApprovalFallbackReason, ApprovalOutcome, ApprovalReviewer, ApprovalReviewerKind,
    AutoReviewerHealth, OriginalUserInput,
};
pub use artifact::ArtifactTick;
pub use auto_rules::{
    AutoBlockKind, AutoEnvironment, AutoEnvironmentSource, AutoPolicy, AutoRule, AutoRuleSource,
    RuleApplicability, RuleFinding, SoftDenyClearance, TaskScopeFinding, VerdictEvidence,
};
pub use auto_settings::{
    AutoReviewerPreference, AutoRuleChange, AutoRuleGroupSettings, AutoRuleKind, AutoRuleSettings,
    AutoSettingsRevision, CustomAutoRule,
};
pub use chat::{
    AgentHostOptions, AutoReviewerNotice, ChatSnapshot, DirectoryTrustPolicy, Event, EventKind,
    LoopId, OperationRecord, OperationState, PendingCommand, PendingInput,
    PermissionApprovalSource, PermissionExecution, PermissionRequest, PromptPolicy,
    ProposedToolCall, QueuedInput, QueuedInputKind, Receipt, RunId, SavedChat, ToolCallRecovery,
    UserShellReceipt, UserShellRecord, UserShellState, chat_display_title, short_chat_title,
};
pub use denials::{
    ApprovalHistoryView, ApprovalPath, ApprovalRecord, ApprovalResult, DenialAutoFinding,
    DenialAutoRuleSummary, DenialCategory, DenialPersistence, DenialRecord, DenialReference,
    DenialReviewFacts, DenialRuleSource, DenialRuleSummary, DenialsView, DisplaySummary,
    MAX_RECENT_DENIALS, render_reference,
};
pub use family::{
    AgentMessageSource, DelegationCeiling, DelegationMetadata, DirectedHumanSource,
    FamilyCompletion, FamilyInputRecord, FamilyMessageRejection, FamilyMessageRejectionCode,
    MessageCause, SEND_AGENT_MESSAGE_TOOL_NAME, SUBAGENT_TOOL_NAME, SubagentNotificationDelivery,
    SubagentRegistration, VerifiedUserRelay,
};
pub use file_change::{
    FileChangeAction, FileChangeArtifactRef, FileChangeReviewSummary, FileIdentity, FileVersion,
    PreparedFileChange,
};
pub use frame::{check_frame_length, decode_frame, encode_frame};
pub use history::{
    ActiveConversationItem, BranchCreation, BranchOrigin, BranchOutcome, BranchPublicItem,
    BranchPublicSource, BranchRequest, BranchSessionPermissionRegistration, COMPACT_BOUNDARY_TEXT,
    CodeRewind, CompactBoundary, CompactTrigger, CompactionMarker, CompactionOutcome,
    CompactionStatus, DirectedSummaryDraft, DirectedSummaryOutcome, DirectedSummaryRequest,
    ModelChange, RewindDraft, RewindMode, RewindOutcome, RewindPoint, SummarizeMetadata,
    SummaryDirection,
};
pub use host::{
    HerdrInteraction, HerdrPane, HostCallContext, HostEndpointIdentity, HostToolAuthority,
    HostedResources, HostedScreenSnapshot,
};
pub use hosted::{HostedMember, HostedMemberState, HostedSnapshot, HostedViewGrant};
pub use ids::ChatId;
pub use image::{Image, ImageMediaType};
pub use input::{ImageContext, ImageOrigin, InputContext, TextContext, render_input_context};
pub use interaction::{Question, QuestionAnswer, QuestionOption, QuestionRequest};
pub use limits::*;
pub use local_messages::{
    FOLDED_TICK_REPLY_CHARACTERS, INTERRUPT_MESSAGE, INTERRUPT_MESSAGE_FOR_TOOL_USE, LocalMessage,
    TaskNotification, TaskNotificationStatus, TickDeliveryLine,
};
pub use loops::{
    EventClaimOperation, EventClaimStatus, EventSourceBinding, EventSourceRecord,
    FrozenAssociationRequest, LoopSnapshot, LoopState, LoopTriggerNotice, LoopTriggerNoticeKind,
    SourceConnection, SourcePublication, Trigger, TriggerSource,
};
pub use model::{
    ApiKeyStatus, Content, ImageSource, InputModality, Message, ModelAuthSource, ModelConfig,
    ModelIdentity, ModelProvider, PrivateHistorySource, Protocol, ReasoningEffort, Role,
    ToolResultBlock, ToolResultContent, Usage,
};
pub use model_availability::{AccountModelDirectory, DirectorySemantics, ModelAvailability};
pub use model_card::{
    Adapter, ModelCard, ModelPatch, ProviderPatch, ThinkingBudgets, ThinkingControl,
};
pub use monitor::{MonitorMember, MonitorMemberSource, MonitorMembership};
pub use permissions::{Decision, ExecutionMode, PermissionPolicy, PermissionRule, PermissionScope};
pub use permissions_view::{
    PermissionChange, PermissionRuleScope, PermissionRuleSource, PermissionRuleView,
    PermissionsRevision, PermissionsView, SessionPermissionDenial, WorkspaceScope,
};
pub use plan::{Plan, Progress, TodoItem, TodoStatus};
pub use projection::{HistoryCall, InvalidConversation};
pub use provider_connection::{ConnectionRegion, ModelDiscovery, ProductKind, ProviderConnection};
pub use provider_failure::{ProviderFailureClass, ProviderHttpFailure};
pub use shell_tasks::{
    SHELL_TASK_OUTPUT_MAX_BYTES, SHELL_TASK_PROMPT_QUIET_PERIOD, ShellTask, ShellTaskAction,
    ShellTaskCapabilities, ShellTaskCenterError, ShellTaskDelta, ShellTaskNotice, ShellTaskOrigin,
    ShellTaskOutputChunk, ShellTaskOwner, ShellTaskPromptWatchdog, ShellTaskSnapshot,
    ShellTaskState, ShellTaskSubscriptionEvent, recover_shell_update_time,
};
pub use skill::{InvokedSkill, SkillSource};
pub use tick_subscriptions::{
    DEFAULT_CADENCE_SECONDS, MAX_CADENCE_SECONDS, MAX_DELIVERY_PAYLOAD_BYTES, MIN_CADENCE_SECONDS,
    ObservationKind, TICK_SUBSCRIPTION_SOURCE, TickLinkState, TickSubscription,
    TickSubscriptionChange, TickSubscriptionStatus,
};
pub use tool::ToolDefinition;
pub use tools::{CommandSnapshot, CommandState, FileReadRecord, SearchConfig, ToolResult};
pub use usage::{
    CounterSummary, CounterTotal, LedgerError, ModelRequestKind, ModelUsageSummary,
    ObservationEffect, ProtocolFailure, RequestFailure, RequestOutcome, RequestOwner,
    RequestSequence, RequestUsageLedger, RequestUsageRecord, ResponseRejection, UsageCounter,
    UsageCoverage, UsageSummary,
};
