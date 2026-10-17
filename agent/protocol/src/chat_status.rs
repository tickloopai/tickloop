use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ArtifactTick, BranchOrigin, ChatId, ChatSnapshot, CommandSnapshot, DelegationMetadata, Event,
    EventKind, LoopSnapshot, ModelConfig, ModelRequestKind, MonitorMembership, PendingCommand,
    PendingInput, PermissionPolicy, PermissionRequest, Plan, Progress, ProviderHttpFailure,
    QuestionRequest, QueuedInput, ReasoningEffort, RequestOutcome, RequestUsageLedger, RunId,
    UserShellRecord,
    chat::{fallback_title, highest_pasted_image_id, queued_inputs},
};

/// What a Chat is doing now, without its history: the state a View's status row, Monitor,
/// request dialogs and run tracking read while turns stream. It leaves out the messages, events,
/// operations, tool results and pending Thinking a `ChatSnapshot` carries, so reading it costs
/// the same however long the conversation grows; the few facts derived from history are
/// computed where the history is.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatStatus {
    #[serde(
        default = "crate::SearchConfig::enabled_default",
        skip_serializing_if = "crate::SearchConfig::is_enabled_default"
    )]
    pub search_config: Option<crate::SearchConfig>,
    #[serde(default)]
    pub task_tools_enabled: bool,
    #[serde(default)]
    pub task_board: Option<crate::TaskBoard>,
    #[serde(default)]
    pub task_boards: BTreeMap<crate::LoopId, crate::TaskBoard>,
    pub id: ChatId,
    pub workdir: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub branch_origin: Option<BranchOrigin>,
    pub generated_title: Option<String>,
    pub delegation: Option<DelegationMetadata>,
    pub model: ModelConfig,
    pub pending_model: Option<ModelConfig>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub thinking_enabled: bool,
    pub policy: PermissionPolicy,
    pub loops: Vec<LoopSnapshot>,
    pub plans: Vec<Plan>,
    pub progress: Vec<Progress>,
    pub permissions: Vec<PermissionRequest>,
    pub questions: Vec<QuestionRequest>,
    pub pending_inputs: Vec<PendingInput>,
    pub pending_commands: Vec<PendingCommand>,
    pub user_shells: Vec<UserShellRecord>,
    pub commands: Vec<CommandSnapshot>,
    pub monitor: MonitorMembership,
    pub artifact_ticks: Vec<ArtifactTick>,
    pub provider_failures: HashMap<String, ProviderHttpFailure>,
    pub context_window_tokens: Option<usize>,
    /// The input tokens of the current model's latest conversation request; `None` when that
    /// request finished without reporting them.
    pub context_tokens: Option<u64>,
    /// The highest `[Image #N]` pasted into the conversation, sent or queued.
    pub highest_pasted_image_id: Option<u32>,
    /// The pending permission and question requests whose request event is still in the replay
    /// window, in the order they were asked.
    pub request_order: Vec<Uuid>,
    /// When each Agent Shell command first reported an update within the replay window.
    pub command_started_at_ms: BTreeMap<Uuid, u64>,
    /// The runs that took in a message from another Agent of the family or from a View directing
    /// a family member.
    pub family_input_runs: BTreeSet<RunId>,
    pub last_sequence: u64,
}

/// The state of a Chat that lives only in the process running it, as a `ChatSnapshot` or a
/// `LiveChatSnapshot` holds it.
pub(crate) struct ProcessState<'a> {
    pub model: &'a ModelConfig,
    pub pending_model: &'a Option<ModelConfig>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub thinking_enabled: bool,
    pub policy: &'a PermissionPolicy,
    pub additional_directories: &'a [PathBuf],
    pub permissions: &'a [PermissionRequest],
    pub questions: &'a [QuestionRequest],
    pub pending_inputs: &'a [PendingInput],
    pub pending_commands: &'a [PendingCommand],
    pub user_shells: &'a [UserShellRecord],
    pub commands: &'a [CommandSnapshot],
    pub request_usage: &'a RequestUsageLedger,
    pub context_window_tokens: Option<usize>,
    pub events: &'a [Event],
    pub last_sequence: u64,
}

impl ChatStatus {
    pub fn of(snapshot: &ChatSnapshot) -> Self {
        Self::assemble(
            snapshot,
            ProcessState {
                model: &snapshot.model,
                pending_model: &snapshot.pending_model,
                reasoning_effort: snapshot.reasoning_effort,
                thinking_enabled: snapshot.thinking_enabled,
                policy: &snapshot.policy,
                additional_directories: &snapshot.additional_directories,
                permissions: &snapshot.permissions,
                questions: &snapshot.questions,
                pending_inputs: &snapshot.pending_inputs,
                pending_commands: &snapshot.pending_commands,
                user_shells: &snapshot.user_shells,
                commands: &snapshot.commands,
                request_usage: &snapshot.request_usage,
                context_window_tokens: snapshot.context_window_tokens,
                events: &snapshot.events,
                last_sequence: snapshot.last_sequence,
            },
        )
    }

    pub(crate) fn assemble(conversation: &ChatSnapshot, process: ProcessState<'_>) -> Self {
        let pending = process
            .permissions
            .iter()
            .map(|request| request.id)
            .chain(process.questions.iter().map(|request| request.id))
            .collect::<BTreeSet<_>>();
        let mut command_started_at_ms = BTreeMap::<Uuid, u64>::new();
        let mut request_order = Vec::new();
        for event in process.events {
            match &event.kind {
                EventKind::CommandUpdated { command } => {
                    if let Some(at) = command.updated_at_ms {
                        command_started_at_ms
                            .entry(command.operation_id)
                            .and_modify(|first| *first = (*first).min(at))
                            .or_insert(at);
                    }
                }
                EventKind::PermissionRequested { request } if pending.contains(&request.id) => {
                    request_order.push(request.id)
                }
                EventKind::QuestionAsked { request } if pending.contains(&request.id) => {
                    request_order.push(request.id)
                }
                _ => {}
            }
        }
        let family_input_runs = conversation
            .message_ids
            .iter()
            .filter(|(_, id)| {
                conversation.agent_message_origins.contains_key(id)
                    || conversation.human_family_origins.contains_key(id)
            })
            .filter_map(|(index, _)| conversation.message_runs.get(index).copied())
            .collect();
        Self {
            task_tools_enabled: conversation.task_tools_enabled,
            search_config: conversation.search_config.clone(),
            task_board: conversation.task_board.clone(),
            task_boards: conversation.task_boards.clone(),
            id: conversation.id,
            workdir: conversation.workdir.clone(),
            additional_directories: process.additional_directories.to_vec(),
            branch_origin: conversation.branch_origin.clone(),
            generated_title: conversation.generated_title.clone(),
            delegation: conversation.delegation.clone(),
            model: process.model.clone(),
            pending_model: process.pending_model.clone(),
            reasoning_effort: process.reasoning_effort,
            thinking_enabled: process.thinking_enabled,
            policy: process.policy.clone(),
            loops: conversation.loops.clone(),
            plans: conversation.plans.clone(),
            progress: conversation.progress.clone(),
            permissions: process.permissions.to_vec(),
            questions: process.questions.to_vec(),
            pending_inputs: process.pending_inputs.to_vec(),
            pending_commands: process.pending_commands.to_vec(),
            user_shells: process.user_shells.to_vec(),
            commands: process.commands.to_vec(),
            monitor: conversation.monitor.clone(),
            artifact_ticks: conversation.artifact_ticks.clone(),
            provider_failures: conversation.provider_failures.clone(),
            context_window_tokens: process.context_window_tokens,
            context_tokens: context_tokens(process.request_usage, process.model),
            highest_pasted_image_id: highest_pasted_image_id(
                &conversation.message_contexts,
                process.pending_inputs,
            ),
            request_order,
            command_started_at_ms,
            family_input_runs,
            last_sequence: process.last_sequence,
        }
    }

    pub fn queued_inputs(&self) -> Vec<QueuedInput> {
        queued_inputs(
            &self.pending_inputs,
            &self.pending_commands,
            &self.user_shells,
        )
    }

    /// Whether the model that answers the next user input accepts images; a pending model
    /// switch takes effect before that input runs.
    pub fn accepts_images(&self) -> bool {
        self.pending_model
            .as_ref()
            .unwrap_or(&self.model)
            .accepts_images()
    }

    pub fn display_title(&self) -> String {
        self.branch_origin
            .as_ref()
            .map(|origin| origin.name.clone())
            .or_else(|| self.generated_title.clone())
            .unwrap_or_else(|| fallback_title(&self.loops, &self.user_shells))
    }
}

/// The input tokens of `model`'s latest conversation request: none sent yet counts as empty, and
/// one still in flight as empty until it reports.
fn context_tokens(ledger: &RequestUsageLedger, model: &ModelConfig) -> Option<u64> {
    let identity = model.identity();
    let mut matching = ledger
        .entries()
        .iter()
        .rev()
        .filter(|entry| entry.kind == ModelRequestKind::Conversation && entry.model == identity)
        .peekable();
    let Some(latest) = matching.peek() else {
        return Some(0);
    };
    let in_flight = latest.outcome == RequestOutcome::InFlight;
    matching
        .find_map(|entry| entry.context_input_tokens())
        .or(in_flight.then_some(0))
}
