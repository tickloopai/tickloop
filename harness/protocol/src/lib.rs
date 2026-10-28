//! Shared Harness domain types and wire definitions.

mod approvals;
mod artifact;
mod auto_compact;
mod auto_rules;
mod auto_settings;
mod auto_setup;
mod chat;
mod chat_status;
mod context_breakdown;
mod context_usage;
mod denials;
mod family;
mod file_change;
mod goal;
mod history;
mod host;
mod hosted;
mod ids;
mod image;
mod input;
mod interaction;
pub mod local_messages;
mod loops;
pub mod mcp;
mod model;
mod model_availability;
mod model_card;
pub mod model_ids;
mod monitor;
mod permissions;
mod permissions_view;
mod plan;
pub mod projection;
mod provider_connection;
mod provider_credential_scope;
mod provider_failure;
mod scheduled_tasks;
mod shell_tasks;
mod skill;
mod tick_subscriptions;
mod tool;
mod tools;
mod trigger_display;
mod usage;

pub use approvals::{
    ApprovalAssessmentDetails, ApprovalAttemptDetails, ApprovalAttemptOutcome, ApprovalDiagnostic,
    ApprovalFailureKind, ApprovalFallbackReason, ApprovalOutcome, ApprovalReviewer,
    ApprovalReviewerKind, ApprovalStage, AutoReviewerHealth, OriginalUserInput,
    PreparedApprovalFacts,
};
pub use artifact::{ArtifactAudience, ArtifactFeedbackHistory, ArtifactPublication, ArtifactTick};
pub use auto_compact::{
    auto_compact_line, auto_compact_tokens, auto_compact_warning_tokens, context_window_tokens,
};
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
pub use chat_status::{ChatStatus, ChatStatusInput};
pub use context_breakdown::{
    ContextBreakdown, ContextCategory, ContextCategoryUsage, ContextDraft, ContextItem,
    ContextToolServer, ToolSearchMode, ToolSearchPool, UnavailableToolServer,
};
pub use context_usage::{
    ContextMeasurement, ContextTokenCounter, ESTIMATED_CONTEXT_BYTES_PER_TOKEN,
    context_token_counter, current_context_tokens, current_context_tokens_counted,
    estimated_image_tokens, message_content_tokens, serialized_context_bytes,
    set_context_token_counter,
};
pub use denials::{
    AUTO_REJECTION_PREFIX, ApprovalHistoryView, ApprovalPath, ApprovalRecord, ApprovalResult,
    DenialAutoFinding, DenialAutoRuleSummary, DenialCategory, DenialPersistence, DenialRecord,
    DenialReference, DenialReviewFacts, DenialRuleSource, DenialRuleSummary, DenialsView,
    DisplaySummary, MAX_RECENT_DENIALS, render_reference,
};
pub use family::{
    AgentColor, AgentMessageSource, ChildRunStatus, DelegatedGoal, DelegationCeiling,
    DelegationMetadata, DelegationParent, DirectedHumanSource, FamilyCompletion, FamilyInputRecord,
    FamilyMessageRejection, FamilyMessageRejectionCode, MAX_SUBAGENT_DEPTH, MessageCause,
    SEND_AGENT_MESSAGE_TOOL_NAME, SUBAGENT_HANDBACK_TOOL_NAME, SUBAGENT_MISSING_REPORT,
    SUBAGENT_REPORT_RECEIPT, SUBAGENT_TOOL_NAME, SubagentNotificationDelivery,
    SubagentRegistration, SubagentReport, VerifiedUserRelay,
};
pub use file_change::{
    FileChangeAction, FileChangeArtifactRef, FileChangeReviewSummary, FileIdentity, FileVersion,
    PreparedFileChange,
};
pub use goal::{
    GoalAction, GoalAuthor, GoalBlock, GoalCheck, GoalHistoryEntry, GoalNotice, GoalPause,
    GoalSnapshot, GoalStatus, GoalVerdict,
};
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
    HostedResources, OtherTickhost, TickhostOccupancy,
};
pub use hosted::{
    ATTACH_ARGUMENTS, ATTACH_ENVIRONMENT, HostedMember, HostedMemberState, HostedProgramExit,
    HostedViewGrant, VIEW_PANE_ENVIRONMENT,
};
pub use ids::{
    AgentMessageId, AgentMessageKind, ChatId, ChatKind, DelegationId, DelegationKind, HostedRunId,
    HostedRunKind, InputKind, LoopKind, MessageChainId, MessageChainKind, OperationId,
    OperationKind, ResponseKind, RunKind, ScheduledTaskId, ScheduledTaskIdKind, SharedTaskId,
    SharedTaskKind, ShellTaskId, ShellTaskKind, SubscriptionId, SubscriptionKind,
};
pub use image::{Image, ImageMediaType};
pub use input::{ImageContext, ImageOrigin, InputContext, TextContext, render_input_context};
pub use interaction::{Question, QuestionAnswer, QuestionOption, QuestionRequest};
pub use local_messages::{
    FOLDED_TICK_REPLY_CHARACTERS, INTERRUPT_MESSAGE, INTERRUPT_MESSAGE_FOR_TOOL_USE, LocalMessage,
    SubagentReportMessage, TaskNotification, TaskNotificationStatus, TickDeliveryLine,
};
pub use loops::{
    Authority, Coalesce, EventSourceBinding, LoopSnapshot, LoopState, LoopTriggerNotice,
    LoopTriggerNoticeKind, PauseReason, Receiving, SubscriptionSequence, TickDeliveryItem,
    TriggerIdentity, TriggerKind, TriggerSource,
};
pub use model::{
    ApiKeyStatus, Content, ImageSource, InputModality, Message, ModelAuthSource, ModelConfig,
    ModelIdentity, ModelProvider, PrivateHistorySource, Protocol, ReasoningEffort, Role,
    ToolResultBlock, ToolResultContent, Usage,
};
pub use model_availability::{AccountModelDirectory, DirectorySemantics, ModelAvailability};
pub use model_card::{
    Adapter, ModelCard, ModelPatch, NativeToolSearch, ProviderPatch, TemperatureControl,
    ThinkingBudgets, ThinkingControl, ThinkingOffEncoding,
};
pub use monitor::{MonitorMember, MonitorMembership};
pub use permissions::{Decision, ExecutionMode, PermissionPolicy, PermissionRule, PermissionScope};
pub use permissions_view::{
    AdditionalDirectorySource, AdditionalDirectorySources, PermissionChange, PermissionRuleScope,
    PermissionRuleSource, PermissionRuleView, PermissionsRevision, PermissionsView,
    SessionPermissionDenial, WorkspaceScope,
};
pub use plan::{Plan, Progress, TodoItem, TodoStatus};
pub use projection::{HistoryCall, InvalidConversation};
pub use provider_connection::{ConnectionRegion, ModelDiscovery, ProductKind, ProviderConnection};
pub use provider_credential_scope::ProviderCredentialScope;
pub use provider_failure::{ProviderFailureClass, ProviderHttpFailure};
pub use scheduled_tasks::{
    MAX_SCHEDULED_TASK_TITLE_CHARS, MAX_SCHEDULED_TASKS_PER_CHAT, ScheduledTask,
    ScheduledTaskConfirmation, ScheduledTaskCreator, ScheduledTaskDecision, ScheduledTaskKind,
    ScheduledTaskNotice, ScheduledTaskRemovalReason, ScheduledTaskRevocation,
    legacy_scheduled_task_id,
};
pub use shell_tasks::{
    FINISHED_SHELL_DISPLAY_MS, SHELL_TASK_OUTPUT_MAX_BYTES, SHELL_TASK_PROMPT_QUIET_PERIOD,
    ShellTask, ShellTaskAction, ShellTaskCapabilities, ShellTaskCenterError, ShellTaskDelta,
    ShellTaskNotice, ShellTaskOrigin, ShellTaskOutputChunk, ShellTaskOwner,
    ShellTaskPromptWatchdog, ShellTaskSnapshot, ShellTaskState, ShellTaskSubscriptionEvent,
    recover_shell_update_time,
};
pub use skill::{
    InvokedSkill, RemoteSkillDefinition, RemoteSkillResource, SkillSource, UserSkillInvocation,
};
pub use tick_subscriptions::{
    MAX_DELIVERY_ITEM_BYTES, MAX_DELIVERY_PAYLOAD_BYTES, ObservationKind, TickLinkState,
    TickSubscription, TickSubscriptionChange, TickSubscriptionFrequency, TickSubscriptionStatus,
};
pub use tool::ToolDefinition;
pub use tools::{
    CitationSource, CommandSnapshot, CommandState, FileReadRecord, SearchConfig,
    SearchProviderKind, SearchTestResult, SearchTestStatus, ToolResult, WebSearchFailure,
    WebSearchSuggestion,
};
pub use trigger_display::{
    GOAL_CONTINUATION_EVENT, ScheduledInputWrapper, ending_scheduled_run_prompt,
    final_scheduled_run_prompt, loop_instructions_error_prompt, loop_instructions_prompt,
    scheduled_skill_prompt, strip_final_scheduled_run_prompt, strip_loop_instructions_error_prompt,
};
pub use usage::{
    CounterSummary, CounterTotal, LedgerError, ModelRequestKind, ModelUsageSummary,
    ObservationEffect, ProtocolFailure, RequestFailure, RequestOutcome, RequestOwner,
    RequestSequence, RequestUsageLedger, RequestUsageRecord, ResponseRejection, UsageCounter,
    UsageCoverage, UsageSummary,
};

mod tasks;
pub use tasks::*;

mod isolation;
pub use isolation::{ExecutionIsolation, NetworkIsolation, WriteIsolation};
