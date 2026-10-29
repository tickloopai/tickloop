use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use tickloop_harness_protocol::{
    AgentMessageSource, DelegationMetadata, DirectedHumanSource, FamilyCompletion,
    FamilyInputRecord, Message, ModelIdentity, MonitorMembership, OriginalUserInput,
    PrivateHistorySource, QuestionRequest, Role, SubagentRegistration, ToolCallRecovery,
    ToolResult, ToolResultContent, Usage,
};

/// One line in the Transcript. Unknown types are kept and written back unchanged, so older versions
/// can read the file without dropping records written by newer versions.
/// A store keeps each Line as its JSON; the local store writes one per line of a JSONL file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Line {
    User(ChainEntry),
    Assistant(ChainEntry),
    System(ChainEntry),
    ArtifactTicks(ArtifactTicksEntry),
    MonitorMembers(MonitorMembersEntry),
    CustomTitle(CustomTitleEntry),
    AiTitle(AiTitleEntry),
    Subagents(SubagentsEntry),
    TaskBoards(TaskBoardsEntry),
    /// The Chat's goal as it is now; the latest record wins.
    Goal(GoalEntry),
    /// A goal change the conversation shows as one line; every one is kept.
    GoalEvent(GoalEventEntry),
    /// The root Chat's scheduled tasks; the latest record wins.
    ScheduledTasks(ScheduledTasksEntry),
    FamilyCompletions(FamilyCompletionsEntry),
    FamilyInputs(FamilyInputsEntry),
    FamilyMessageHold(FamilyMessageHoldEntry),
    SentAgentMessages(SentAgentMessagesEntry),
    /// Where the Chat's WebFetch URLs and Artifact writes may lead; the latest record wins.
    Provenance(ProvenanceEntry),
    CitationSources(CitationSourcesEntry),
    LastPrompt(LastPromptEntry),
    BranchOrigin(BranchOriginEntry),
    Delegation(DelegationEntry),
    /// Durable ACP origin and session-only configuration; never client credentials.
    AcpSession(AcpSessionEntry),
    /// Bounded, validated proposed edit hunks; never a complete file snapshot or proof of success.
    RecordedEdit(RecordedEditEntry),
    /// Following Claude Code: a file-rewind snapshot, associated with a message on the chain by
    /// message id.
    FileHistorySnapshot(FileHistorySnapshotEntry),
    /// Following Claude Code: a replacement decision for a large tool result, rebuilt on resume;
    /// entries of the same Chat accumulate one by one.
    ContentReplacement(ContentReplacementEntry),
    /// An answer's lifecycle; the latest record of a response id wins.
    Response(ResponseEntry),
    /// The input ledger; the latest record of an input id wins.
    Input(InputEntry),
    /// A Run's durable wait; the latest record of a wait id wins.
    Wait(WaitEntry),
    NestedCall(NestedCallEntry),
    /// The retained state of the Chat as of this Line: loading it stands for every retained Line
    /// before it.
    RetainedSnapshot(RetainedSnapshotEntry),
    /// A Line of a kind this version does not know, kept as it was written.
    #[serde(untagged)]
    Unknown(UnknownLine),
}

/// A Line of a kind this version does not know: its `type` and its whole JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct UnknownLine {
    pub kind: String,
    pub value: serde_json::Value,
}

impl Serialize for UnknownLine {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UnknownLine {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let kind = value["type"]
            .as_str()
            .ok_or_else(|| serde::de::Error::custom("a Line has a type"))?
            .to_owned();
        // A known kind that does not parse is a broken Line, not an unknown one.
        if serde_json::from_value::<MetadataKind>(serde_json::Value::String(kind.clone()))
            .is_ok_and(|known| known != MetadataKind::Other)
        {
            return Err(serde::de::Error::custom(format!("invalid {kind} Line")));
        }
        Ok(Self { kind, value })
    }
}

/// The retained Lines that stand for the retained state of a Chat: the latest of each kind
/// whose latest Line wins, the records by id still needed, the live accumulating Lines, and the
/// newest Lines of each unknown kind.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetainedSnapshotEntry {
    pub chat_id: Uuid,
    pub lines: Vec<Line>,
}

/// How a load keeps a kind of Line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retention {
    /// The conversation chain, read from the latest compact boundary.
    Chain,
    /// The latest Line of the kind stands for the Chat.
    ChatLatest,
    /// The latest Line of each id wins.
    LatestById,
    /// Every Line still adds to the Chat's state.
    Accumulating,
    /// Kept as written, the newest few of each kind.
    Unknown,
    /// Stands for every retained Line before it.
    Snapshot,
    /// A Trigger source's own state, which only the Chat's lease holder writes, again after a
    /// compaction: no retained snapshot stands for it, so none written by another process
    /// carries a stale copy. The kinds of the Goal and the scheduled tasks are such.
    SourceState,
}

/// The kind of a Line read from its top-level `type` alone, without parsing the rest of it.
/// [`Line::kind`] maps every Line variant to the kind of the same name.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum MetadataKind {
    User,
    Assistant,
    System,
    CustomTitle,
    AiTitle,
    LastPrompt,
    BranchOrigin,
    Delegation,
    AcpSession,
    RecordedEdit,
    ArtifactTicks,
    MonitorMembers,
    Subagents,
    FamilyInputs,
    FamilyMessageHold,
    SentAgentMessages,
    Provenance,
    CitationSources,
    TaskBoards,
    Goal,
    GoalEvent,
    ScheduledTasks,
    FamilyCompletions,
    FileHistorySnapshot,
    ContentReplacement,
    Response,
    Input,
    Wait,
    NestedCall,
    RetainedSnapshot,
    #[serde(other)]
    Other,
}
impl MetadataKind {
    /// Every kind decides here how a load keeps it; a new kind must be classified.
    pub fn retention(&self) -> Retention {
        match self {
            Self::User | Self::Assistant | Self::System => Retention::Chain,
            Self::CustomTitle
            | Self::AiTitle
            | Self::LastPrompt
            | Self::BranchOrigin
            | Self::Delegation
            | Self::AcpSession
            | Self::ArtifactTicks
            | Self::MonitorMembers
            | Self::Subagents
            | Self::FamilyMessageHold
            | Self::FamilyInputs
            | Self::SentAgentMessages
            | Self::Provenance
            | Self::CitationSources
            | Self::TaskBoards
            | Self::FamilyCompletions => Retention::ChatLatest,
            Self::Goal | Self::ScheduledTasks => Retention::SourceState,
            Self::Response | Self::Input | Self::Wait | Self::NestedCall | Self::RecordedEdit => {
                Retention::LatestById
            }
            Self::FileHistorySnapshot | Self::ContentReplacement | Self::GoalEvent => {
                Retention::Accumulating
            }
            Self::RetainedSnapshot => Retention::Snapshot,
            Self::Other => Retention::Unknown,
        }
    }
    pub fn is_chain(&self) -> bool {
        self.retention() == Retention::Chain
    }
    pub fn is_retained(&self) -> bool {
        !self.is_chain()
    }
}
/// The fields of a Line a store reads before, or instead of, parsing the whole Line.
#[derive(Debug, Deserialize)]
pub struct MetadataHeader {
    #[serde(rename = "type")]
    pub kind: MetadataKind,
    #[serde(rename = "agentMessageOrigin")]
    pub agent_origin: Option<serde_json::Value>,
}

impl Line {
    pub fn chain(role: Role, entry: ChainEntry) -> Self {
        match role {
            Role::User => Self::User(entry),
            Role::Assistant => Self::Assistant(entry),
            Role::System => Self::System(entry),
        }
    }

    pub fn chain_entry(&self) -> Option<&ChainEntry> {
        match self {
            Self::User(entry) | Self::Assistant(entry) | Self::System(entry) => Some(entry),
            _ => None,
        }
    }

    pub fn is_conversation_message(&self) -> bool {
        matches!(self, Self::User(_) | Self::Assistant(_))
    }

    /// Chat-level records outside the conversation chain. A load from the latest compact
    /// boundary keeps the ones written before it.
    pub fn is_retained(&self) -> bool {
        self.kind().is_retained()
    }

    pub fn kind(&self) -> MetadataKind {
        match self {
            Self::User(_) => MetadataKind::User,
            Self::Assistant(_) => MetadataKind::Assistant,
            Self::System(_) => MetadataKind::System,
            Self::ArtifactTicks(_) => MetadataKind::ArtifactTicks,
            Self::MonitorMembers(_) => MetadataKind::MonitorMembers,
            Self::CustomTitle(_) => MetadataKind::CustomTitle,
            Self::AiTitle(_) => MetadataKind::AiTitle,
            Self::Subagents(_) => MetadataKind::Subagents,
            Self::TaskBoards(_) => MetadataKind::TaskBoards,
            Self::Goal(_) => MetadataKind::Goal,
            Self::GoalEvent(_) => MetadataKind::GoalEvent,
            Self::ScheduledTasks(_) => MetadataKind::ScheduledTasks,
            Self::FamilyCompletions(_) => MetadataKind::FamilyCompletions,
            Self::FamilyInputs(_) => MetadataKind::FamilyInputs,
            Self::FamilyMessageHold(_) => MetadataKind::FamilyMessageHold,
            Self::SentAgentMessages(_) => MetadataKind::SentAgentMessages,
            Self::Provenance(_) => MetadataKind::Provenance,
            Self::CitationSources(_) => MetadataKind::CitationSources,
            Self::LastPrompt(_) => MetadataKind::LastPrompt,
            Self::BranchOrigin(_) => MetadataKind::BranchOrigin,
            Self::Delegation(_) => MetadataKind::Delegation,
            Self::AcpSession(_) => MetadataKind::AcpSession,
            Self::RecordedEdit(_) => MetadataKind::RecordedEdit,
            Self::FileHistorySnapshot(_) => MetadataKind::FileHistorySnapshot,
            Self::ContentReplacement(_) => MetadataKind::ContentReplacement,
            Self::Response(_) => MetadataKind::Response,
            Self::Input(_) => MetadataKind::Input,
            Self::Wait(_) => MetadataKind::Wait,
            Self::NestedCall(_) => MetadataKind::NestedCall,
            Self::RetainedSnapshot(_) => MetadataKind::RetainedSnapshot,
            Self::Unknown(_) => MetadataKind::Other,
        }
    }
}

/// Public review captured from a validated sidecar at preparation time. The terminal tool
/// result determines whether the operation succeeded; these hunks alone grant no authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedEditEntry {
    pub operation_id: Uuid,
    pub run_id: Uuid,
    pub call_id: String,
    pub review: tickloop_harness_protocol::RecordedFileChangeReview,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainEntry {
    pub uuid: Uuid,
    pub parent_uuid: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_parent_uuid: Option<Uuid>,
    #[serde(default)]
    pub is_sidechain: bool,
    pub chat_id: Uuid,
    pub timestamp: String,
    /// The Chat's Folder. A Chat without a Folder writes none, and a chain whose last entry has
    /// none restores as a Chat without a Folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invoked_skills: Vec<tickloop_harness_protocol::InvokedSkill>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_skill_invocation: Option<tickloop_harness_protocol::UserSkillInvocation>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_meta: bool,
    /// Tickloop extension: marks a host note, the host tool names announced as available once
    /// it was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_tools: Option<std::collections::BTreeSet<String>>,
    /// Tickloop extension: marks the host note telling the model that Plan mode ended, Claude
    /// Code's `plan_mode_exit` attachment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_mode_exit: bool,
    /// Tickloop extension: marks the host note telling the model the execution mode it is now
    /// in, Claude Code's `plan_mode`, `auto_mode` and their exit attachments. Raw like
    /// `execution_mode`, so an unknown mode cannot invalidate the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_mode_note: Option<serde_json::Value>,
    /// Tickloop extension: marks the host note telling the model the state of its Loop and
    /// Subagents.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub loop_state_note: bool,
    /// Tickloop extension: on a Loop state note, the state of the Loop the model knows once it
    /// reads the note, so a load tells only what changed since. Raw, so a later format cannot
    /// invalidate the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_state_view: Option<serde_json::Value>,
    /// Tickloop extension: marks the host note reminding the model of the task tools, Claude
    /// Code's `task_reminder` attachment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub task_reminder: bool,
    /// Tickloop extension: marks the host note telling the model the date it now is, Claude
    /// Code's `date_change` attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_change: Option<String>,
    /// Tickloop extension: marks a host note carrying guidance, like Claude Code's Skill messages
    /// and `mcp_instructions_delta` attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<tickloop_harness_protocol::GuidanceNote>,
    /// The execution mode at a user checkpoint stays raw so an unknown legacy value cannot
    /// invalidate the entire Transcript entry.
    #[serde(
        default,
        alias = "permissionMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub execution_mode: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_inherited_prompt: bool,
    #[serde(
        default,
        alias = "originalUserMessageId",
        skip_serializing_if = "Option::is_none"
    )]
    pub original_message_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message_origin: Option<serde_json::Value>,
    pub human_family_origin: Option<DirectedHumanSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_recoveries: Vec<ToolCallRecovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<serde_json::Value>,
    /// Tickloop extension: when one in-memory message is split into several entries (parallel tool
    /// results appended to a result message), later entries are marked as continuing the message of
    /// their parent entry and are merged back into one message on load. In Claude Code each tool
    /// result is already a separate user message; Tickloop combines parallel results into one
    /// in-memory message, hence this field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub continues_message: bool,
    /// Follows the `subtype` of Claude Code's system entries; a compact boundary is
    /// [`COMPACT_BOUNDARY_SUBTYPE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_metadata: Option<CompactMetadata>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_compact_summary: bool,
    /// Following Claude Code: when a directional summary keeps messages, the summary message
    /// records the number of summarized messages, the user's extra instructions and the direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarize_metadata: Option<SummarizeMetadataEntry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_visible_in_transcript_only: bool,
    /// Usage of an assistant entry (Claude Code records it in `message.usage`; Tickloop's message
    /// structure has no such field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// The model that generated an assistant entry's message (Claude Code records it in
    /// `message.model`; Tickloop's message structure has no such field). On resume it decides
    /// whether private reasoning in the history came from the current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Recorded<ModelIdentity>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_source: Option<Recorded<PrivateHistorySource>>,
    /// The Loop of the run the entry belongs to (recorded when there is a `runId`). On resume it
    /// assigns plan and progress tool calls to their own Loops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_id: Option<Uuid>,
    /// Follows the `slug` of Claude Code messages: the Chat's plan file name prefix, written on
    /// every entry once generated; on resume the plan file is associated through the first slug on
    /// the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// The answer this entry is part of: its assistant messages, tool results, interruption
    /// marker and the prompts merged into it at a tool boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<Uuid>,
    /// The ledger input this user prompt entered with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<Uuid>,
    /// The full Loop state recorded by a Loop entry (system, with subtype `loop_*`); the run id is
    /// in `runId`. Such entries hang on the conversation chain but are not conversation messages
    /// and never enter the messages sent to the model.
    #[serde(rename = "loop", default, skip_serializing_if = "Option::is_none")]
    pub loop_record: Option<LoopRecord>,
}

/// A record a later version may have written with a value of one of its enums this version does
/// not know: such a record is kept as written, and nothing acts on it.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded<T> {
    Known(T),
    Later(serde_json::Value),
}

impl<T> Recorded<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Later(_) => None,
        }
    }
}

/// How a record shows that a later version wrote it.
pub trait LaterValues {
    /// Whether `value`, which does not parse, names a value only a later version knows.
    fn holds_later_value(value: &serde_json::Value) -> bool;
}

impl LaterValues for ModelIdentity {
    fn holds_later_value(value: &serde_json::Value) -> bool {
        use tickloop_harness_protocol::{ModelProvider, Protocol, names_later_variant};
        names_later_variant::<Protocol>(&value["protocol"])
            || names_later_variant::<ModelProvider>(&value["provider"])
    }
}

impl LaterValues for PrivateHistorySource {
    fn holds_later_value(value: &serde_json::Value) -> bool {
        use tickloop_harness_protocol::{Adapter, ModelAuthSource, names_later_variant};
        names_later_variant::<Adapter>(&value["adapter"])
            || names_later_variant::<ModelAuthSource>(&value["auth_source"])
    }
}

impl<T: Serialize> Serialize for Recorded<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Known(value) => value.serialize(serializer),
            Self::Later(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: serde::de::DeserializeOwned + LaterValues> Deserialize<'de> for Recorded<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match serde_json::from_value(value.clone()) {
            Ok(known) => Ok(Self::Known(known)),
            Err(_) if T::holds_later_value(&value) => Ok(Self::Later(value)),
            Err(error) => Err(serde::de::Error::custom(error)),
        }
    }
}

impl ChainEntry {
    pub fn is_compact_boundary(&self) -> bool {
        self.message.role == Role::System
            && self.subtype.as_deref() == Some(COMPACT_BOUNDARY_SUBTYPE)
    }

    /// A compact boundary the conversation chain starts again at, which a load from the latest
    /// boundary may begin with: no parent, and no preserved segment before it.
    pub fn restarts_chain(&self) -> bool {
        self.is_compact_boundary()
            && self.parent_uuid.is_none()
            && self
                .compact_metadata
                .as_ref()
                .is_none_or(|metadata| metadata.preserved_segment.is_none())
    }

    pub fn is_loop_record(&self) -> bool {
        self.message.role == Role::System && self.loop_record.is_some()
    }

    /// A Loop entry in the current format, as opposed to one of the records a Chat with several
    /// Loops wrote.
    pub fn is_current_loop_record(&self) -> bool {
        self.is_loop_record() && self.subtype.as_deref() == Some(LOOP_SUBTYPE)
    }
}

pub const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";
/// Subtype of Loop entries. On resume the full state in the last such entry on the chain wins; Loop
/// entries with other subtypes are the old format from when one Chat had several Loops and are
/// discarded on resume.
pub const LOOP_SUBTYPE: &str = "chat_loop";

/// The full state of a Loop on the conversation chain. The state, receive switch and subscriptions
/// are stored as raw JSON; this module does not parse their structure.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LoopRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_report: Option<tickloop_harness_protocol::SubagentReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_events: Vec<serde_json::Value>,
    pub loop_id: Uuid,
    pub state: serde_json::Value,
    /// Receive switch; carries a reason when paused (`/stop` or history load).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiving: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_sources: Vec<serde_json::Value>,
    /// The submitted version is durable before the approval tool receives its result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_version: Option<u64>,
    #[serde(default)]
    pub plan_approval_pending: bool,
    /// Progress steps (raw JSON) written together with `plan_version`; later progress tool calls on
    /// the chain override them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<serde_json::Value>,
    /// Unix milliseconds when the runtime moved the Loop into `state`, kept as raw JSON so an
    /// unreadable value leaves the time unknown instead of dropping the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<serde_json::Value>,
    pub run_started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_started: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions: Option<u64>,
}

impl LoopRecord {
    /// Progress remains a separate checkpoint; plan changes are part of the Loop state.
    pub fn without_progress(&self) -> Self {
        Self {
            progress: None,
            ..self.clone()
        }
    }
}

/// Follows Claude Code's `compactMetadata`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompactMetadata {
    pub trigger: CompactTriggerEntry,
    pub pre_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_summarized: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_segment: Option<PreservedSegmentEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_user_inputs: Option<Vec<OriginalUserInput>>,
    /// The display node the Chat showed as current when it was compacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_leaf: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CompactTriggerEntry {
    Manual,
    Auto,
    /// A trigger a later version knows, kept as written.
    #[serde(untagged)]
    Later(String),
}

/// Follows Claude Code's `preservedSegment`: kept messages are not rewritten, and on load their
/// parent pointers are reattached from this.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreservedSegmentEntry {
    pub head_uuid: Uuid,
    pub anchor_uuid: Uuid,
    pub tail_uuid: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SummarizeMetadataEntry {
    pub messages_summarized: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
    pub direction: PartialCompactDirection,
}

/// Follows Claude Code's `PartialCompactDirection`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PartialCompactDirection {
    From,
    UpTo,
    /// A direction a later version knows, kept as written.
    #[serde(untagged)]
    Later(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomTitleEntry {
    pub chat_id: Uuid,
    pub custom_title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTitleEntry {
    pub chat_id: Uuid,
    pub ai_title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentsEntry {
    pub chat_id: Uuid,
    pub subagents: Vec<SubagentRegistration>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LastPromptEntry {
    pub chat_id: Uuid,
    pub last_prompt: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BranchOriginEntry {
    pub chat_id: Uuid,
    pub forked_from: ForkedFrom,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationEntry {
    pub chat_id: Uuid,
    pub delegation: DelegationMetadata,
    #[serde(default = "tickloop_harness_protocol::SearchConfig::enabled_default")]
    pub search_config: Option<tickloop_harness_protocol::SearchConfig>,
}

/// The local authority boundary that created an ACP session. Paths are canonical and the
/// directory identity detects a replacement at the same spelling after a restart.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionOrigin {
    pub canonical_cwd: PathBuf,
    pub cwd_device: u64,
    pub cwd_inode: u64,
    pub config_root: PathBuf,
}

impl AcpSessionOrigin {
    pub fn capture(cwd: &std::path::Path, config_root: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;

        let canonical_cwd = std::fs::canonicalize(cwd)?;
        let metadata = std::fs::metadata(&canonical_cwd)?;
        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ACP working directory is not a directory",
            ));
        }
        Ok(Self {
            canonical_cwd,
            cwd_device: metadata.dev(),
            cwd_inode: metadata.ino(),
            config_root: std::fs::canonicalize(config_root)?,
        })
    }

    pub fn matches_current(
        &self,
        cwd: &std::path::Path,
        config_root: &std::path::Path,
    ) -> std::io::Result<bool> {
        Ok(*self == Self::capture(cwd, config_root)?)
    }
}

/// The latest ACP session configuration. Only model identity is recorded, not its provider
/// connection, token, environment or any MCP server configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionEntry {
    pub chat_id: Uuid,
    pub origin: AcpSessionOrigin,
    pub startup_permission_ceiling: tickloop_harness_protocol::PermissionPolicy,
    pub execution_mode: tickloop_harness_protocol::ExecutionMode,
    pub model: ModelIdentity,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ForkedFrom {
    pub chat_id: Uuid,
    #[serde(default)]
    pub message_uuid: Option<Uuid>,
}

/// Follows Claude Code's `file-history-snapshot`: `message_id` is the associated message on the
/// chain; an update entry (`is_snapshot_update`) is keyed by the message that started the edit and
/// replaces the snapshot for `snapshot.messageId`. The snapshot body is stored as raw JSON; this
/// module does not parse its structure.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistorySnapshotEntry {
    pub message_id: Uuid,
    pub snapshot: serde_json::Value,
    pub is_snapshot_update: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContentReplacementEntry {
    pub chat_id: Uuid,
    pub replacements: Vec<ContentReplacementRecord>,
}

/// Claude Code's `ContentReplacementRecord`: `replacement` is the original text the model saw; on
/// resume it is applied as is rather than regenerated by the current code.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ContentReplacementRecord {
    #[serde(rename_all = "camelCase")]
    ToolResult {
        tool_use_id: String,
        replacement: String,
    },
    #[serde(other)]
    Unknown,
}

impl ContentReplacementRecord {
    pub fn tool_use_id(&self) -> &str {
        match self {
            Self::ToolResult { tool_use_id, .. } => tool_use_id,
            Self::Unknown => "",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactTicksEntry {
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    pub artifact_ticks: Vec<tickloop_harness_protocol::ArtifactTick>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub removed_artifact_ticks: std::collections::BTreeSet<String>,
}

/// The latest Monitor membership of one Chat. An empty Chat with a working directory and members
/// materializes its Transcript so the membership survives a restart.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorMembersEntry {
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    pub monitor: MonitorMembership,
}

impl MonitorMembersEntry {
    pub fn materializes_chat(&self) -> bool {
        self.cwd.is_some() && !self.monitor.is_empty()
    }
}

/// The latest scheduled tasks of one Chat: every creation, run and deletion rewrites the whole
/// list.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", try_from = "ScheduledTasksRecord")]
pub struct ScheduledTasksEntry {
    pub chat_id: Uuid,
    /// The Chat's folder, which lets tasks scheduled before any message keep the Chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub tasks: Vec<tickloop_harness_protocol::ScheduledTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revoked: Vec<tickloop_harness_protocol::ScheduledTaskRevocation>,
}

impl ScheduledTasksEntry {
    /// Whether the record has the store keep a Chat without messages; once kept, the Chat stays
    /// after its last task is gone.
    pub fn materializes_chat(&self) -> bool {
        !self.tasks.is_empty()
    }
}

/// A [`ScheduledTasksEntry`] as stored, whose tasks may predate their UUIDs.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduledTasksRecord {
    chat_id: Uuid,
    #[serde(default)]
    cwd: Option<PathBuf>,
    tasks: Vec<serde_json::Value>,
    #[serde(default)]
    revoked: Vec<tickloop_harness_protocol::ScheduledTaskRevocation>,
}

impl TryFrom<ScheduledTasksRecord> for ScheduledTasksEntry {
    type Error = serde_json::Error;

    fn try_from(record: ScheduledTasksRecord) -> Result<Self, Self::Error> {
        let tasks = record
            .tasks
            .into_iter()
            .map(|task| tickloop_harness_protocol::ScheduledTask::from_record(record.chat_id, task))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            chat_id: record.chat_id,
            cwd: record.cwd,
            tasks,
            revoked: record.revoked,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyCompletionsEntry {
    pub chat_id: Uuid,
    pub completions: Vec<FamilyCompletion>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyInputsEntry {
    pub chat_id: Uuid,
    pub inputs: std::collections::BTreeMap<Uuid, FamilyInputRecord>,
}

/// Sender runs stopped in the same batch as this receiver. Their later messages stay queued
/// until the receiver is explicitly continued or accepts a new deliverable family message.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyMessageHoldEntry {
    pub chat_id: Uuid,
    pub stopped_runs: std::collections::HashSet<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SentAgentMessagesEntry {
    pub chat_id: Uuid,
    pub messages: std::collections::BTreeMap<Uuid, AgentMessageSource>,
}

/// The complete append-only source table, outside the conversation chain and prompt prefix.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CitationSourcesEntry {
    pub chat_id: Uuid,
    pub sources: Vec<tickloop_harness_protocol::CitationSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<CitationCall>,
}

/// A call's sources and the conversation nodes that retain its evidence.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CitationCall {
    pub call_id: String,
    pub run_id: tickloop_harness_protocol::RunId,
    pub message_id: Uuid,
    pub numbers: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<Uuid>,
}

/// What the Chat's own work vouches for: the links its fetched pages gave on their own origin,
/// the targets its fetches were redirected to within the same site, and the Artifacts it
/// created or the user shared with it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceEntry {
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub page_links: std::collections::BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub redirect_targets: std::collections::BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub artifacts: std::collections::BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskBoardsEntry {
    pub chat_id: Uuid,
    pub boards: std::collections::BTreeMap<Uuid, tickloop_harness_protocol::TaskBoard>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GoalEntry {
    pub chat_id: Uuid,
    /// The Chat's folder, which lets a goal set before any message keep the Chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// `None` once the goal was cleared.
    pub goal: Option<tickloop_harness_protocol::GoalSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GoalEventEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_message_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub goal_id: Uuid,
    /// The objective when it happened.
    pub objective: String,
    pub notice: tickloop_harness_protocol::GoalNotice,
    pub timestamp: String,
}

/// One answer: one attempt of one Run. Written when it starts and again when it ends.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResponseEntry {
    pub response_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<Uuid>,
    pub attempt: u32,
    pub run_id: Uuid,
    pub loop_id: Uuid,
    /// The display id of the prompt the answer belongs under: the latest prompt when it started.
    pub display_parent: Option<Uuid>,
    pub timestamp: String,
    pub state: ResponseState,
    /// It ended without a message of its own, so no chain entry stands for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub without_messages: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResponseState {
    Started,
    /// Its Run waits durably: a [`WaitEntry`] of the answer says on what and where it continues.
    Suspended,
    /// Its Run continued after a wait, in the change that put the wait's result on the chain.
    Resumed,
    Ended {
        ending: ResponseEnding,
    },
}

impl ResponseState {
    pub fn is_ended(&self) -> bool {
        matches!(self, Self::Ended { .. })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResponseEnding {
    Completed,
    Interrupted { by: InterruptedBy },
    Failed { error: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InterruptedBy {
    /// The user interrupted or stopped it.
    User,
    /// The host went away, as on shutdown or a crash; the input may be continued.
    Host,
    /// A voluntary handover's host ended it at its ceiling; the input may be continued.
    Handover,
}

/// One ledger record of an input. Every record repeats what was accepted, so the latest record
/// of an input id is enough on its own.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InputEntry {
    pub input_id: Uuid,
    pub op: InputOp,
    /// SHA-256 of the input's payload, hex encoded; reusing the id with another payload is
    /// refused.
    pub digest: String,
    /// The display node the input was accepted under; `None` when it joins the conversation
    /// wherever it ends when the input starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_parent: Option<Uuid>,
    pub loop_id: Uuid,
    pub run_id: Uuid,
    pub timestamp: String,
    pub state: InputState,
    /// While `Accepted`: the queued input as it waits, enough to queue it again after a load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<serde_json::Value>,
    /// What a Trigger delivery was, in every record of the input: its acceptance, the turn it
    /// started or merged into, and its withdrawal with the reason. The acceptance's `queued`
    /// holds the whole source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerRecord>,
}

/// A Trigger delivery as its ledger records name it: the identity its ledger id is named by,
/// which says which subscription, request, Goal or task it came from, and how many source
/// events it merges.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TriggerRecord {
    pub identity: tickloop_harness_protocol::TriggerIdentity,
    pub merged: u64,
}

impl TriggerRecord {
    pub fn of(source: &tickloop_harness_protocol::TriggerSource) -> Self {
        Self {
            identity: source.identity(),
            merged: source.merged(),
        }
    }

    pub fn kind(&self) -> tickloop_harness_protocol::TriggerKind {
        self.identity.kind()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InputOp {
    /// A prompt continuing the conversation.
    Continue,
    /// A user Skill invocation.
    Skill,
    /// A prompt in place of an earlier one, as its sibling.
    ReplacePrompt,
    /// Another answer to the prompt of an earlier answer, as its sibling.
    RegenerateAnswer,
    /// The user's reply to a question a Run waits on; it joins that Run.
    Answer,
    /// A Trigger, Tick update or other source that continues a Loop.
    Trigger,
    /// An @tickloop request on an Artifact.
    Feedback,
    /// A message from another Agent of the family, such as a completion notification.
    AgentMessage,
}

impl InputOp {
    /// The user sent it, as a prompt, a Skill, an edit or a reply.
    pub fn is_from_user(&self) -> bool {
        matches!(
            self,
            Self::Continue
                | Self::Skill
                | Self::ReplacePrompt
                | Self::RegenerateAnswer
                | Self::Answer
        )
    }

    /// The host or the user sends it under its own id, which repeating it must find again; the
    /// others come from the Agent itself.
    pub fn is_external(&self) -> bool {
        matches!(
            self,
            Self::Continue
                | Self::Skill
                | Self::ReplacePrompt
                | Self::RegenerateAnswer
                | Self::Answer
                | Self::Feedback
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InputState {
    Accepted,
    /// Its Run started. Only an input whose prompt does not carry its id records this.
    Started,
    Settled {
        outcome: SettledOutcome,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SettledOutcome {
    Completed {
        usage: Usage,
    },
    Interrupted {
        by: InterruptedBy,
    },
    Failed {
        error: String,
    },
    /// Answered by the Run `into`, which it joined before or while that Run went on.
    Merged {
        into: Uuid,
    },
    /// Taken back from the queue before it started.
    Withdrawn,
    Discarded {
        reason: DiscardReason,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiscardReason {
    /// Its Trigger source ended or could no longer be proved before the input started.
    SourceEnded,
    /// Its Agent was stopped while it waited.
    Stopped,
    /// The Tick Subscription it came from was cancelled before it started.
    Unsubscribed,
    /// The artifact it came from was unlinked from the Chat before it started.
    Unlinked,
}

/// A Run's durable wait: a point in the Run that only an outside event ends, and where the Run
/// continues once it ends.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitEntry {
    /// A question's wait id is its request id.
    pub wait_id: Uuid,
    pub chat_id: Uuid,
    pub loop_id: Uuid,
    pub run_id: Uuid,
    pub response_id: Uuid,
    pub kind: WaitKind,
    pub resume: ResumePoint,
    pub timestamp: String,
    pub state: WaitState,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WaitKind {
    /// An `AskUserQuestion` call waits for the user's reply.
    #[serde(rename_all = "camelCase")]
    Question {
        request: QuestionRequest,
        /// The call's operation, which a rebuilt Run restores so a cancellation reads as it would
        /// have in the process that asked.
        operation_id: Uuid,
    },
    /// A foreground `Subagent` call waits for its child's run to end and report.
    #[serde(rename_all = "camelCase")]
    Subagent {
        child_chat: Uuid,
        child_loop: Uuid,
        child_run: Uuid,
        /// The call's operation, which a rebuilt Run restores.
        operation_id: Uuid,
    },
    /// A delegated member waits until every child it delegated has reported.
    #[serde(rename_all = "camelCase")]
    Children { children: Vec<Uuid> },
    /// A voluntary handover stopped the Run at a safe point: whoever holds the Chat next goes on
    /// with the same attempt where the wait says, once the Run takes the execution slot.
    Handover,
}

/// Where a Run continues.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResumePoint {
    /// In the tool batch of the assistant message whose canonical (earliest source) id is
    /// `assistant_message`, at its call `call_id`; the calls after it run next, then the end of
    /// the batch. Re-entry fails when the loaded chain does not hold that message.
    #[serde(rename_all = "camelCase")]
    InBatch {
        assistant_message: Uuid,
        call_id: String,
    },
    /// Before the Run's next model request: the messages queued to its Loop meanwhile join the
    /// conversation first.
    BeforeModelRequest,
}

impl ResumePoint {
    /// The call the Run waits on, when it waits inside a tool batch.
    pub fn call_id(&self) -> Option<&str> {
        match self {
            Self::InBatch { call_id, .. } => Some(call_id),
            Self::BeforeModelRequest => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WaitState {
    Open,
    /// The wait ended with `result`, which is not on the chain yet; `by_input` is the ledger input
    /// that ended it.
    #[serde(rename_all = "camelCase")]
    Resolved {
        result: ModelVisibleResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by_input: Option<Uuid>,
    },
    /// The result is on the chain, or a wait without a call's result is over: written in the same
    /// change as the answer's `Resumed`.
    Appended,
    Abandoned {
        reason: AbandonReason,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AbandonReason {
    /// The Run was interrupted, stopped or failed while it waited.
    Interrupted,
    /// The host that held the Run went away and its successor does not rebuild waits.
    HostLost,
}

/// A call's result as the conversation records it, decided when the wait on the call ends.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelVisibleResult {
    /// The tool's own result, which its operation record and `ToolCompleted` carry.
    pub tool: ToolResult,
    /// What the model sees: the tool's text with any approval comment, then its images.
    pub content: ToolResultContent,
    /// The Run ends after this result, as it does after a bare user denial, a stopped foreground
    /// command or a declined question.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ends_run: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness_protocol::Content;

    #[test]
    fn unknown_line_types_are_kept_as_written() {
        let raw = r#"{"type":"future-kind","value":1}"#;
        let line: Line = serde_json::from_str(raw).unwrap();
        assert!(matches!(&line, Line::Unknown(unknown) if unknown.kind == "future-kind"));
        assert_eq!(line.kind(), MetadataKind::Other);
        assert!(line.is_retained());
        assert_eq!(
            serde_json::to_value(&line).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }

    #[test]
    fn acp_metadata_is_chat_latest_and_a_broken_known_record_is_not_unknown() {
        assert_eq!(MetadataKind::AcpSession.retention(), Retention::ChatLatest);
        assert!(serde_json::from_str::<Line>(r#"{"type":"acp-session","chatId":7}"#).is_err());
    }

    #[test]
    fn a_broken_line_of_a_known_kind_is_not_unknown() {
        assert!(serde_json::from_str::<Line>(r#"{"type":"user","uuid":7}"#).is_err());
    }

    fn guidance_line(guidance: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "user",
            "uuid": Uuid::from_u128(1),
            "parentUuid": null,
            "isSidechain": false,
            "chatId": Uuid::nil(),
            "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "later",
            "message": {"role": "user", "content": [{"type": "text", "text": "Host notice"}]},
            "isMeta": true,
            "guidance": guidance,
            "humanFamilyOrigin": null,
        })
    }

    /// A guidance kind a later version adds leaves the host note's Line readable here, and
    /// writing it back keeps what that version wrote.
    #[test]
    fn a_host_note_with_a_later_guidance_kind_is_kept_as_written() {
        let raw = guidance_line(serde_json::json!({
            "kind": "later_kind",
            "servers": {"docs": "sha256:01"},
            "detail": [1, 2],
        }));
        let line: Line = serde_json::from_value(raw.clone()).unwrap();
        let Line::User(entry) = &line else {
            panic!("a user Line: {line:?}");
        };
        assert!(matches!(
            &entry.guidance,
            Some(tickloop_harness_protocol::GuidanceNote::Later(later)) if later.tag() == "later_kind"
        ));
        assert_eq!(serde_json::to_value(&line).unwrap(), raw);
    }

    /// A chain Line of `role` with `fields` added, as a later version may write it.
    fn chain_line(role: &str, fields: serde_json::Value) -> serde_json::Value {
        let mut line = serde_json::json!({
            "type": role,
            "uuid": Uuid::from_u128(1),
            "parentUuid": null,
            "chatId": Uuid::nil(),
            "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "later",
            "message": {"role": role, "content": [{"type": "text", "text": "Hello"}]},
            "humanFamilyOrigin": null,
        });
        line.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        line
    }

    /// The Line reads, and writing it back keeps what the later version wrote at each pointer;
    /// reading that again changes nothing.
    fn assert_kept_as_written(raw: &serde_json::Value, pointers: &[&str]) -> Line {
        let line: Line = serde_json::from_value(raw.clone()).unwrap();
        let written = serde_json::to_value(&line).unwrap();
        for pointer in pointers {
            assert_eq!(written.pointer(pointer), raw.pointer(pointer), "{pointer}");
        }
        let again: Line = serde_json::from_value(written.clone()).unwrap();
        assert_eq!(serde_json::to_value(&again).unwrap(), written);
        line
    }

    #[test]
    fn a_later_skill_source_is_kept_as_written() {
        let later = serde_json::json!({"kind": "later_source", "registry": "skills.example"});
        let raw = chain_line(
            "user",
            serde_json::json!({
                "invokedSkills": [{"name": "deploy", "content": "Steps", "source": later}],
                "userSkillInvocation": {"name": "deploy", "source": later},
            }),
        );
        let line = assert_kept_as_written(
            &raw,
            &["/invokedSkills/0/source", "/userSkillInvocation/source"],
        );
        let entry = line.chain_entry().unwrap();
        assert!(matches!(
            &entry.invoked_skills[0].source,
            tickloop_harness_protocol::SkillSource::Later(later) if later.tag() == "later_source"
        ));
        let broken = chain_line(
            "user",
            serde_json::json!({"userSkillInvocation": {"name": "deploy", "source": {"kind": "user"}}}),
        );
        assert!(serde_json::from_value::<Line>(broken).is_err());
    }

    #[test]
    fn a_later_model_or_private_history_source_is_kept_as_written() {
        let raw = chain_line(
            "assistant",
            serde_json::json!({
                "model": {
                    "profile": "later",
                    "protocol": "later_protocol",
                    "requested_model": "m",
                    "provider": "api",
                },
                "privateSource": {
                    "provider_id": "p",
                    "endpoint_fingerprint": "f",
                    "adapter": "messages",
                    "service_model_id": "m",
                    "auth_source": "later_sign_in",
                    "credential_reference": "",
                },
            }),
        );
        let line = assert_kept_as_written(&raw, &["/model", "/privateSource"]);
        let entry = line.chain_entry().unwrap();
        assert!(matches!(entry.model, Some(Recorded::Later(_))));
        assert!(matches!(entry.private_source, Some(Recorded::Later(_))));
        let known = chain_line(
            "assistant",
            serde_json::json!({"model": {"profile": "p", "protocol": "messages", "requested_model": "m"}}),
        );
        let known: Line = serde_json::from_value(known).unwrap();
        assert!(matches!(
            known.chain_entry().unwrap().model,
            Some(Recorded::Known(_))
        ));
        let broken = chain_line(
            "assistant",
            serde_json::json!({"model": {"profile": "p", "protocol": "messages"}}),
        );
        assert!(serde_json::from_value::<Line>(broken).is_err());
    }

    #[test]
    fn a_later_input_context_or_image_origin_is_kept_as_written() {
        let origin = |contexts: serde_json::Value| {
            chain_line(
                "user",
                serde_json::json!({"humanFamilyOrigin": {
                    "message_id": Uuid::from_u128(2),
                    "source_chat_id": Uuid::from_u128(3),
                    "source_handle": "root",
                    "original_submission": "Look",
                    "original_contexts": contexts,
                    "original_paths": [],
                    "source_session_id": Uuid::from_u128(4),
                    "source_view_id": Uuid::from_u128(5),
                    "source_display_version": 1,
                }}),
            )
        };
        let raw = origin(serde_json::json!([
            {"kind": "later_context", "uri": "later://a"},
            {
                "kind": "image",
                "origin": {"type": "later_origin", "from": "camera"},
                "image": {"media_type": "image/png", "data": "AA==", "width": 1, "height": 1},
            },
        ]));
        let line = assert_kept_as_written(&raw, &["/humanFamilyOrigin/original_contexts"]);
        let contexts = &line
            .chain_entry()
            .unwrap()
            .human_family_origin
            .as_ref()
            .unwrap()
            .original_contexts;
        assert!(matches!(
            &contexts[0],
            tickloop_harness_protocol::InputContext::Later(_)
        ));
        assert!(matches!(
            &contexts[1],
            tickloop_harness_protocol::InputContext::Image(image)
                if matches!(image.origin, tickloop_harness_protocol::ImageOrigin::Later(_))
        ));
        let broken = origin(serde_json::json!([{"kind": "file"}]));
        assert!(serde_json::from_value::<Line>(broken).is_err());
    }

    #[test]
    fn a_later_subagent_report_status_is_kept_as_written() {
        let report = |report: serde_json::Value| {
            chain_line(
                "system",
                serde_json::json!({
                    "subtype": LOOP_SUBTYPE,
                    "loop": {
                        "loopId": Uuid::from_u128(2),
                        "state": "Idle",
                        "subagentReport": report,
                        "runStartedAt": null,
                    },
                }),
            )
        };
        let raw = report(serde_json::json!({"status": "later_status", "note": "x"}));
        let line = assert_kept_as_written(&raw, &["/loop/subagentReport"]);
        assert!(matches!(
            line.chain_entry()
                .unwrap()
                .loop_record
                .as_ref()
                .unwrap()
                .subagent_report,
            Some(tickloop_harness_protocol::SubagentReport::Later(_))
        ));
        let broken = report(serde_json::json!({"status": "pending"}));
        assert!(serde_json::from_value::<Line>(broken).is_err());
    }

    #[test]
    fn a_later_compaction_trigger_or_summary_direction_is_kept_as_written() {
        let raw = chain_line(
            "system",
            serde_json::json!({
                "subtype": COMPACT_BOUNDARY_SUBTYPE,
                "compactMetadata": {"trigger": "later_trigger", "preTokens": 10},
            }),
        );
        let line = assert_kept_as_written(&raw, &["/compactMetadata/trigger"]);
        assert_eq!(
            line.chain_entry()
                .unwrap()
                .compact_metadata
                .as_ref()
                .unwrap()
                .trigger,
            CompactTriggerEntry::Later("later_trigger".into())
        );
        let raw = chain_line(
            "user",
            serde_json::json!({
                "isCompactSummary": true,
                "summarizeMetadata": {"messagesSummarized": 2, "direction": "later_direction"},
            }),
        );
        assert_kept_as_written(&raw, &["/summarizeMetadata/direction"]);
        let broken = chain_line(
            "system",
            serde_json::json!({"compactMetadata": {"trigger": 7, "preTokens": 10}}),
        );
        assert!(serde_json::from_value::<Line>(broken).is_err());
    }

    #[test]
    fn later_content_blocks_are_kept_as_written() {
        let content = serde_json::json!([
            {"type": "later_block", "id": "call-1", "detail": {"x": 1}},
            {"type": "protocol_data", "protocol": "later_protocol", "value": {"type": "reasoning"}},
            {
                "type": "tool_result",
                "tool_use_id": "call-0",
                "content": [
                    {"type": "later_result_block", "detail": 2},
                    {"type": "image", "source": {"type": "later_source", "url": "later://i"}, "width": 1, "height": 1},
                ],
                "is_error": false,
            },
        ]);
        let raw = chain_line(
            "user",
            serde_json::json!({"message": {"role": "user", "content": content}}),
        );
        let line = assert_kept_as_written(&raw, &["/message/content"]);
        let blocks = &line.chain_entry().unwrap().message.content;
        assert!(matches!(&blocks[0], Content::Later(later) if later.tag() == "later_block"));
        assert_eq!(blocks[0].later_call_id(), Some("call-1"));
        assert!(matches!(&blocks[1], Content::Later(later) if later.tag() == "protocol_data"));
        assert!(matches!(
            &blocks[2],
            Content::ToolResult { content: tickloop_harness_protocol::ToolResultContent::Blocks(results), .. }
                if matches!(results[0], tickloop_harness_protocol::ToolResultBlock::Later(_))
                    && matches!(
                        results[1],
                        tickloop_harness_protocol::ToolResultBlock::Image {
                            source: tickloop_harness_protocol::ImageSource::Later(_),
                            ..
                        }
                    )
        ));
        for broken in [
            serde_json::json!({"type": "text"}),
            serde_json::json!({"type": "protocol_data", "protocol": "messages"}),
            serde_json::json!({"type": "image", "source": {"type": "base64"}, "width": 1, "height": 1}),
        ] {
            let raw = chain_line(
                "user",
                serde_json::json!({"message": {"role": "user", "content": [broken]}}),
            );
            assert!(serde_json::from_value::<Line>(raw).is_err(), "{broken}");
        }
    }

    /// Only an unknown kind is kept as unknown: a known kind still reads as itself, and one that
    /// does not parse breaks its Line as before.
    #[test]
    fn a_known_guidance_kind_is_never_read_as_unknown() {
        let known: Line = serde_json::from_value(guidance_line(serde_json::json!({
            "kind": "mcp_servers",
            "servers": {"docs": "sha256:01"},
        })))
        .unwrap();
        assert!(matches!(
            known.chain_entry().and_then(|entry| entry.guidance.as_ref()),
            Some(tickloop_harness_protocol::GuidanceNote::McpServers { servers }) if servers.len() == 1
        ));
        for broken in [
            serde_json::json!({"kind": "mcp_servers"}),
            serde_json::json!({"kind": "skill", "name": 7}),
            serde_json::json!({"servers": {}}),
        ] {
            assert!(
                serde_json::from_value::<Line>(guidance_line(broken.clone())).is_err(),
                "{broken}"
            );
        }
    }

    /// A store classifying a written Line by its header alone reads the kind the Line has.
    #[test]
    fn the_header_of_a_written_line_reads_as_its_kind() {
        let chat_id = Uuid::nil();
        let user: ChainEntry = serde_json::from_value(serde_json::json!({
            "uuid": Uuid::nil(),
            "parentUuid": null,
            "chatId": chat_id,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "test",
            "message": {"role": "user", "content": []},
            "humanFamilyOrigin": null,
        }))
        .unwrap();
        let lines = [
            Line::User(user.clone()),
            Line::Assistant(user.clone()),
            Line::System(user),
            Line::ArtifactTicks(ArtifactTicksEntry {
                chat_id,
                cwd: None,
                artifact_ticks: Vec::new(),
                removed_artifact_ticks: Default::default(),
            }),
            Line::MonitorMembers(MonitorMembersEntry {
                chat_id,
                cwd: None,
                monitor: Default::default(),
            }),
            Line::CustomTitle(CustomTitleEntry {
                chat_id,
                custom_title: "title".into(),
            }),
            Line::AiTitle(AiTitleEntry {
                chat_id,
                ai_title: "title".into(),
            }),
            Line::Subagents(SubagentsEntry {
                chat_id,
                subagents: Vec::new(),
            }),
            Line::TaskBoards(TaskBoardsEntry {
                chat_id,
                boards: Default::default(),
            }),
            Line::FamilyCompletions(FamilyCompletionsEntry {
                chat_id,
                completions: Vec::new(),
            }),
            Line::FamilyInputs(FamilyInputsEntry {
                chat_id,
                inputs: Default::default(),
            }),
            Line::SentAgentMessages(SentAgentMessagesEntry {
                chat_id,
                messages: Default::default(),
            }),
            Line::CitationSources(CitationSourcesEntry {
                chat_id,
                sources: Vec::new(),
                calls: Vec::new(),
            }),
            Line::Provenance(ProvenanceEntry {
                chat_id,
                ..Default::default()
            }),
            Line::AcpSession(AcpSessionEntry {
                chat_id,
                origin: AcpSessionOrigin {
                    canonical_cwd: PathBuf::from("/project"),
                    cwd_device: 1,
                    cwd_inode: 2,
                    config_root: PathBuf::from("/config"),
                },
                startup_permission_ceiling: Default::default(),
                execution_mode: tickloop_harness_protocol::ExecutionMode::Auto,
                model: ModelIdentity {
                    profile: "test".into(),
                    protocol: tickloop_harness_protocol::Protocol::Responses,
                    requested_model: "test".into(),
                    provider: Default::default(),
                },
            }),
            Line::LastPrompt(LastPromptEntry {
                chat_id,
                last_prompt: "prompt".into(),
            }),
            Line::BranchOrigin(BranchOriginEntry {
                chat_id,
                forked_from: ForkedFrom {
                    chat_id,
                    message_uuid: None,
                },
            }),
            Line::FileHistorySnapshot(FileHistorySnapshotEntry {
                message_id: Uuid::nil(),
                snapshot: serde_json::json!({}),
                is_snapshot_update: false,
            }),
            Line::ContentReplacement(ContentReplacementEntry {
                chat_id,
                replacements: Vec::new(),
            }),
            Line::Response(ResponseEntry {
                response_id: Uuid::nil(),
                input_id: None,
                attempt: 1,
                run_id: Uuid::nil(),
                loop_id: Uuid::nil(),
                display_parent: None,
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                state: ResponseState::Started,
                without_messages: false,
            }),
            Line::Input(InputEntry {
                input_id: Uuid::nil(),
                op: InputOp::Continue,
                digest: String::new(),
                display_parent: None,
                loop_id: Uuid::nil(),
                run_id: Uuid::nil(),
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                state: InputState::Accepted,
                queued: None,
                trigger: None,
            }),
            Line::Wait(WaitEntry {
                wait_id: Uuid::nil(),
                chat_id,
                loop_id: Uuid::nil(),
                run_id: Uuid::nil(),
                response_id: Uuid::nil(),
                kind: WaitKind::Question {
                    request: QuestionRequest {
                        id: Uuid::nil(),
                        chat_id: tickloop_ids::Id::from(chat_id),
                        loop_id: tickloop_ids::Id::from(Uuid::nil()),
                        run_id: tickloop_ids::Id::from(Uuid::nil()),
                        call_id: "call".into(),
                        questions: Vec::new(),
                    },
                    operation_id: Uuid::nil(),
                },
                resume: ResumePoint::InBatch {
                    assistant_message: Uuid::nil(),
                    call_id: "call".into(),
                },
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                state: WaitState::Open,
            }),
        ];
        for line in lines {
            let raw = serde_json::to_vec(&line).unwrap();
            let header = serde_json::from_slice::<MetadataHeader>(&raw).unwrap();
            assert_eq!(header.kind, line.kind());
        }
    }

    fn is_chain(raw: &[u8]) -> bool {
        serde_json::from_slice::<MetadataHeader>(raw)
            .unwrap()
            .kind
            .is_chain()
    }

    #[test]
    fn chain_classification_uses_the_top_level_type_independently_of_field_order() {
        assert!(is_chain(br#"{"message":{},"type":"user"}"#));
        assert!(is_chain(br#"{"type":"assistant","message":{}}"#));
        assert!(is_chain(br#"{"message":{},"type":"\u0075ser"}"#));
        assert!(!is_chain(
            br#"{"inputs":{"example":{"type":"user"}},"type":"family-inputs"}"#
        ));
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedCallEntry {
    pub chat_id: Uuid,
    pub operation: tickloop_harness_protocol::OperationRecord,
}
