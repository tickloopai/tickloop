//! Shared domain types and AgentAccess wire definitions that depend on no other Agent crate.

mod access;
mod approvals;
mod artifact;
mod auto_rules;
mod auto_settings;
mod auto_setup;
mod chat;
mod chat_status;
mod context_usage;
pub mod daemon_management;
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
mod scheduled_tasks;
mod shell_tasks;
mod skill;
mod tick_subscriptions;
mod tool;
mod tools;
mod upgrade;
mod usage;

pub use upgrade::{
    RUNTIME_HANDOFF_VERSION, RuntimeBinaryInfo, RuntimeVersion, WorkerRuntimeVersion,
};

pub use access::{
    AccessCapability, AccessError, AccessErrorCode, AfterTurnAction, AgentAccess, AgentCommand,
    AgentDetailQuery, AgentFilter, AgentSummary, CLIENT_HANDOFF_VERSION, ChunkedResponseKind,
    ClientHandoffHello, ClientMessage, CommandEnvelope, CommandReceipt, CommandResult,
    ConfigurationChange, Cursor, DEFAULT_PAGE_LIMIT, DisplayReceipt, DisplayUpdate, DisplayedView,
    EnvironmentValue, ExecutionAvailability, ExecutionRef, HistoryAction, HostLocation,
    HostWorkContext, InteractionSource, LaunchSpec, LiveChatSnapshot, LiveChatStatus, LoopAction,
    LoopRef, MAX_PAGE_LIMIT, Observation, ObservationChange, ObservationFailure,
    ObservationRequest, ObservationUpdate, ObservationValue, OtherTickhost, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, Page, PendingRequestKind, PersistenceState, ProtocolLimits,
    ProviderCredentialScope, QueuedCommandFailure, RECEIPT_CAPACITY, RECEIPT_RETENTION_SECONDS,
    REPLAY_CAPACITY, ReceiptState, RecordedMessage, RecordedTranscript, RequestRef, ResetReason,
    ServerHandoffHello, ServerHello, ServerMessage, SessionResume, TaskAction, TaskOrigin, TaskRef,
    TickhostOccupancy, TranscriptRef, UserInput, VIEW_LEASE_SECONDS, ViewRef, bounded_update,
    build_identity, set_build_identity,
};
pub use approvals::{
    ApprovalAssessmentDetails, ApprovalAttemptDetails, ApprovalAttemptOutcome, ApprovalDiagnostic,
    ApprovalFailureKind, ApprovalFallbackReason, ApprovalOutcome, ApprovalReviewer,
    ApprovalReviewerKind, ApprovalStage, AutoReviewerHealth, OriginalUserInput,
    PreparedApprovalFacts,
};
pub use artifact::{ArtifactAudience, ArtifactFeedbackHistory, ArtifactPublication, ArtifactTick};
pub use auto_rules::{
    AutoBlockKind, AutoEnvironment, AutoEnvironmentSource, AutoPolicy, AutoRule, AutoRuleSource,
    RuleApplicability, RuleFinding, SoftDenyClearance, TaskScopeFinding, VerdictEvidence,
};
pub use auto_settings::{
    AutoEnvironmentChange, AutoEnvironmentEntry, AutoEnvironmentEntrySource,
    AutoReviewerPreference, AutoRuleChange, AutoRuleGroupSettings, AutoRuleKind, AutoRuleSettings,
    AutoSettingsRevision, CustomAutoRule,
};
pub use auto_setup::{AutoEnvironmentDraft, AutoSetupOptions, AutoSetupPurpose};
pub use chat::{
    AgentHostOptions, ChatSnapshot, DirectoryTrustPolicy, Event, EventKind, GuidanceNote, HostNote,
    InputId, LoopId, OperationRecord, OperationState, PendingCommand, PendingInput,
    PermissionApprovalSource, PermissionExecution, PermissionRequest, PromptPolicy,
    ProposedToolCall, QueuedInput, QueuedInputKind, Receipt, ResponseId, RunId, SavedChat,
    ToolCallRecovery, UserShellReceipt, UserShellRecord, UserShellState, chat_display_title,
    chat_has_work, chat_preview, short_chat_title,
};
pub use chat_status::ChatStatus;
pub use context_usage::{
    ContextMeasurement, ESTIMATED_CONTEXT_BYTES_PER_TOKEN, serialized_context_bytes,
};
pub use denials::{
    AUTO_REJECTION_PREFIX, ApprovalHistoryView, ApprovalPath, ApprovalRecord, ApprovalResult,
    DenialAutoFinding, DenialAutoRuleSummary, DenialCategory, DenialPersistence, DenialRecord,
    DenialReference, DenialReviewFacts, DenialRuleSource, DenialRuleSummary, DenialsView,
    DisplaySummary, MAX_RECENT_DENIALS, render_reference,
};
pub use family::{
    AgentColor, AgentMessageSource, ChildRunStatus, DelegationCeiling, DelegationMetadata,
    DelegationParent, DirectedHumanSource, FamilyCompletion, FamilyInputRecord,
    FamilyMessageRejection, FamilyMessageRejectionCode, MAX_SUBAGENT_DEPTH, MessageCause,
    SEND_AGENT_MESSAGE_TOOL_NAME, SUBAGENT_HANDBACK_TOOL_NAME, SUBAGENT_MISSING_REPORT,
    SUBAGENT_REPORT_RECEIPT, SUBAGENT_TOOL_NAME, SubagentNotificationDelivery,
    SubagentRegistration, SubagentReport, VerifiedUserRelay,
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
    SummaryDirection, TakenBackPrompt,
};
pub use host::{
    HerdrInteraction, HerdrPane, HostCallContext, HostEndpointIdentity, HostToolAuthority,
    HostedResources,
};
pub use hosted::{
    ATTACH_ARGUMENTS, ATTACH_ENVIRONMENT, HostedMember, HostedMemberState, HostedProgramExit,
    HostedViewGrant, VIEW_PANE_ENVIRONMENT,
};
pub use ids::ChatId;
pub use image::{Image, ImageMediaType};
pub use input::{ImageContext, ImageOrigin, InputContext, TextContext, render_input_context};
pub use interaction::{Question, QuestionAnswer, QuestionOption, QuestionRequest};
pub use limits::*;
pub use local_messages::{
    FOLDED_TICK_REPLY_CHARACTERS, INTERRUPT_MESSAGE, INTERRUPT_MESSAGE_FOR_TOOL_USE, LocalMessage,
    SubagentReportMessage, TaskNotification, TaskNotificationStatus, TickDeliveryLine,
};
pub use loops::{
    EventSourceBinding, LoopSnapshot, LoopState, LoopTriggerNotice, LoopTriggerNoticeKind, Trigger,
    TriggerPromptSource, TriggerSource,
};
pub use model::{
    ApiKeyStatus, Content, ImageSource, InputModality, Message, ModelAuthSource, ModelConfig,
    ModelIdentity, ModelProvider, PrivateHistorySource, Protocol, ReasoningEffort, Role,
    ToolResultBlock, ToolResultContent, Usage,
};
pub use model_availability::{AccountModelDirectory, DirectorySemantics, ModelAvailability};
pub use model_card::{
    Adapter, ModelCard, ModelPatch, ProviderPatch, TemperatureControl, ThinkingBudgets,
    ThinkingControl, ThinkingOffEncoding,
};
pub use monitor::{MonitorMember, MonitorMembership};
pub use permissions::{Decision, ExecutionMode, PermissionPolicy, PermissionRule, PermissionScope};
pub use permissions_view::{
    PermissionChange, PermissionRuleScope, PermissionRuleSource, PermissionRuleView,
    PermissionsRevision, PermissionsView, SessionPermissionDenial, WorkspaceScope,
};
pub use plan::{Plan, Progress, TodoItem, TodoStatus};
pub use projection::{HistoryCall, InvalidConversation};
pub use provider_connection::{ConnectionRegion, ModelDiscovery, ProductKind, ProviderConnection};
pub use provider_failure::{ProviderFailureClass, ProviderHttpFailure};
pub use scheduled_tasks::{
    MAX_SCHEDULED_TASKS_PER_CHAT, ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskDecision,
    ScheduledTaskKind, ScheduledTaskPromptSource,
};
pub use shell_tasks::{
    FINISHED_SHELL_DISPLAY_MS, SHELL_TASK_OUTPUT_MAX_BYTES, SHELL_TASK_PROMPT_QUIET_PERIOD,
    ShellTask, ShellTaskAction, ShellTaskCapabilities, ShellTaskCenterError, ShellTaskDelta,
    ShellTaskNotice, ShellTaskOrigin, ShellTaskOutputChunk, ShellTaskOwner,
    ShellTaskPromptWatchdog, ShellTaskSnapshot, ShellTaskState, ShellTaskSubscriptionEvent,
    recover_shell_update_time,
};
pub use skill::{InvokedSkill, RemoteSkillDefinition, RemoteSkillResource, SkillSource};
pub use tick_subscriptions::{
    MAX_DELIVERY_ITEM_BYTES, MAX_DELIVERY_PAYLOAD_BYTES, ObservationKind, TICK_MANUAL_SOURCE,
    TICK_SUBSCRIPTION_SOURCE, TickLinkState, TickSubscription, TickSubscriptionChange,
    TickSubscriptionFrequency, TickSubscriptionStatus,
};
pub use tool::ToolDefinition;
pub use tools::{
    CitationSource, CommandSnapshot, CommandState, FileReadRecord, SearchConfig,
    SearchProviderKind, SearchTestResult, SearchTestStatus, ToolResult, WebSearchFailure,
    WebSearchSuggestion,
};
pub use usage::{
    CounterSummary, CounterTotal, LedgerError, ModelRequestKind, ModelUsageSummary,
    ObservationEffect, ProtocolFailure, RequestFailure, RequestOutcome, RequestOwner,
    RequestSequence, RequestUsageLedger, RequestUsageRecord, ResponseRejection, UsageCounter,
    UsageCoverage, UsageSummary,
};

mod tasks;
pub use tasks::*;
