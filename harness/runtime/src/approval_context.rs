use std::{
    collections::{HashMap, HashSet},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tickloop_harness_protocol::{ScheduledTaskId, ScheduledTaskIdKind};
use tickloop_ids::{IdContext, ModelRef};
use uuid::Uuid;

use super::ChatBookkeeping;
use tickloop_harness_protocol::{
    AutoPolicy, ChatSnapshot, CompactionMarker, Content, DelegatedGoal, GoalSnapshot,
    OperationRecord, OriginalUserInput, Role,
};
use tickloop_harness_transcript::{ChainEntry, Line};

mod budget;
pub(super) mod input_projection;
mod reply_history;
pub(super) use budget::{ContextTruncation, ReviewEvidence};
use input_projection::{InputPurpose, project_tool_input};
pub(super) use reply_history::ReplyHistory;
pub(super) use reply_history::capture as capture_reply_history;

pub(super) const APPROVAL_CONTEXT_VERSION: &str = "trusted-approval-v10";
pub(super) const REVIEW_TEMPLATE_VERSION: &str = "auto-review-v19";
/// Images carry no review evidence in serialized text; their base64 never enters the snapshot.
const IMAGE_PLACEHOLDER: &str = "[image omitted from the approval context]";

const DEPENDENCY_FILES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.toml",
    "Cargo.lock",
    "pyproject.toml",
    "uv.lock",
    "requirements.txt",
    ".npmrc",
    ".yarnrc.yml",
    ".cargo/config.toml",
];
const MAX_DEPENDENCY_BYTES: u64 = 4 * 1024 * 1024;

pub(super) fn dependency_fingerprints(
    directory: &std::path::Path,
) -> std::collections::BTreeMap<PathBuf, String> {
    DEPENDENCY_FILES
        .iter()
        .filter_map(|name| {
            let path = directory.join(name);
            let canonical = path.canonicalize().ok()?;
            if !canonical.starts_with(directory) {
                return None;
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&canonical)
                .ok()?;
            let metadata = file.metadata().ok()?;
            if !metadata.is_file() || metadata.len() > MAX_DEPENDENCY_BYTES {
                return None;
            }
            let mut bytes = Vec::new();
            file.take(MAX_DEPENDENCY_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > MAX_DEPENDENCY_BYTES {
                return None;
            }
            Some((path, format!("{:x}", Sha256::digest(bytes))))
        })
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct DependencyEvidence {
    pub path: PathBuf,
    pub unchanged_since_loop_start: bool,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub(super) struct TickCreationReceipt {
    pub loop_id: Uuid,
    pub operation_id: Uuid,
    pub instance: tick_protocol::InstanceRef,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ApprovalOperation {
    pub id: Uuid,
    pub call_id: String,
    pub loop_id: Uuid,
    pub run_id: Uuid,
    pub tool: String,
    pub input: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum HistoricalToolOutcome {
    ReturnedSuccess,
    ReturnedError,
    OutcomeUnknown,
    NotObserved,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct HistoricalToolCall {
    pub run_id: Option<Uuid>,
    pub call_id: String,
    pub tool: String,
    pub input: Value,
    pub resolved_path: Option<PathBuf>,
    pub outcome: HistoricalToolOutcome,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ToolHistory {
    pub complete: bool,
    pub compaction_gap: bool,
    pub calls: Vec<HistoricalToolCall>,
}

impl Default for ToolHistory {
    fn default() -> Self {
        Self {
            complete: true,
            compaction_gap: false,
            calls: Vec::new(),
        }
    }
}

fn proposed_call_position(
    snapshot: &ChatSnapshot,
    proposed: Option<(Uuid, &str)>,
) -> Option<(usize, usize)> {
    let (run, call) = proposed?;
    snapshot
        .messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            if message.role != Role::Assistant
                || snapshot.message_runs.get(&index) != Some(&tickloop_ids::Id::from(run))
            {
                return None;
            }
            message
                .content
                .iter()
                .enumerate()
                .rev()
                .find_map(|(block, content)| {
                    matches!(content, Content::ToolUse { id, .. } if id == call)
                        .then_some((index, block))
                })
        })
}

fn tool_history(snapshot: &ChatSnapshot, operation: &OperationRecord) -> ToolHistory {
    recovered_tool_history(snapshot, operation, None)
}

pub(super) fn recovered_tool_history(
    snapshot: &ChatSnapshot,
    operation: &OperationRecord,
    ancestors: Option<&[ChainEntry]>,
) -> ToolHistory {
    let compact_boundary = snapshot
        .message_compactions
        .iter()
        .rev()
        .find_map(|(index, marker)| match marker {
            CompactionMarker::Boundary(boundary) if !boundary.preserved_by_later_summary => {
                Some(*index)
            }
            _ => None,
        });
    let compaction_gap = compact_boundary.is_some() && ancestors.is_none();
    let mut history = ToolHistory {
        complete: !compaction_gap,
        compaction_gap,
        ..Default::default()
    };
    let boundary = proposed_call_position(
        snapshot,
        Some((operation.run_id.into(), &operation.call_id)),
    );
    let mut pending: HashMap<(Uuid, &str), usize> = HashMap::new();
    let mut seen_calls = HashSet::new();
    let mut ambiguous = HashSet::new();
    let mut prepared = HashMap::new();
    for prior in &snapshot.operations {
        let key = (prior.run_id, prior.call_id.as_str());
        if let std::collections::hash_map::Entry::Vacant(entry) = prepared.entry(key) {
            entry.insert(Some(prior));
        } else {
            prepared.insert(key, None);
        }
    }
    let mut observed_messages = HashSet::new();
    let recovered = ancestors
        .into_iter()
        .flatten()
        .filter(|entry| !entry.is_compact_summary)
        .map(|entry| {
            (
                None,
                entry.run_id,
                Some(entry.original_message_id.unwrap_or(entry.uuid)),
                &entry.message,
            )
        });
    let start = if ancestors.is_some() {
        compact_boundary.map_or(0, |index| index + 1)
    } else {
        0
    };
    let visible = snapshot
        .messages
        .iter()
        .enumerate()
        .skip(start)
        .map(|(index, message)| {
            (
                Some(index),
                snapshot.message_runs.get(&index).map(|run| run.uuid()),
                snapshot
                    .original_message_ids
                    .get(&index)
                    .or_else(|| snapshot.message_ids.get(&index))
                    .copied(),
                message,
            )
        });
    for (index, run, source_id, message) in recovered.chain(visible) {
        if source_id.is_some_and(|id| !observed_messages.insert(id)) {
            continue;
        }
        for (block, content) in message.content.iter().enumerate() {
            match content {
                Content::ToolUse { id, name, input }
                    if message.role == Role::Assistant
                        && index.is_none_or(|index| {
                            boundary.is_none_or(|boundary| (index, block) < boundary)
                        }) =>
                {
                    let Some(projected_input) =
                        project_tool_input(name, input, InputPurpose::Historical)
                    else {
                        if let Some(run) = run {
                            let key = (run, id.as_str());
                            if !seen_calls.insert(key) {
                                history.complete = false;
                                ambiguous.insert(key);
                                pending.remove(&key);
                            }
                        }
                        continue;
                    };
                    if input_projection::is_incomplete(&projected_input) {
                        history.complete = false;
                    }
                    let previous = run
                        .and_then(|run| prepared.get(&(run.into(), id.as_str())))
                        .copied()
                        .flatten();
                    let path_field = match name.as_str() {
                        "Read" | "Write" | "Edit" => Some("file_path"),
                        "Glob" | "Grep" => Some("path"),
                        "Bash" => Some("cwd"),
                        _ => None,
                    };
                    let call = HistoricalToolCall {
                        run_id: run,
                        call_id: id.clone(),
                        tool: name.clone(),
                        input: projected_input,
                        resolved_path: previous
                            .and_then(|prior| prior.input.get(path_field?))
                            .and_then(Value::as_str)
                            .map(PathBuf::from)
                            .filter(|path| path.is_absolute()),
                        outcome: HistoricalToolOutcome::NotObserved,
                    };
                    let position = history.calls.len();
                    history.calls.push(call);
                    if let Some(run) = run {
                        let key = (run, id.as_str());
                        if !seen_calls.insert(key) || ambiguous.contains(&key) {
                            history.complete = false;
                            ambiguous.insert(key);
                            pending.remove(&key);
                        } else {
                            pending.insert(key, position);
                        }
                    } else {
                        history.complete = false;
                    }
                }
                Content::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } if message.role == Role::User => {
                    if let Some(position) =
                        run.and_then(|run| pending.remove(&(run, tool_use_id.as_str())))
                    {
                        history.calls[position].outcome = if *is_error {
                            HistoricalToolOutcome::ReturnedError
                        } else {
                            HistoricalToolOutcome::ReturnedSuccess
                        };
                    }
                }
                _ => {}
            }
        }
    }
    for call in &mut history.calls {
        if let Some(prior) = call
            .run_id
            .and_then(|run| prepared.get(&(run.into(), call.call_id.as_str())))
            .copied()
            .flatten()
            && prior.state == tickloop_harness_protocol::OperationState::Unknown
        {
            call.outcome = HistoricalToolOutcome::OutcomeUnknown;
        }
    }
    history
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct AgentMessageEvidence {
    pub message_id: Uuid,
    pub source: Value,
    pub content: Vec<Content>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ScheduledTaskEvidence {
    pub id: uuid::Uuid,
    pub creator: tickloop_harness_protocol::ScheduledTaskCreator,
    pub creation_call_id: Option<String>,
    pub details: Value,
}

fn scheduled_task_evidence(
    ids: &IdContext,
    snapshot: &ChatSnapshot,
    operation: &OperationRecord,
    history: &ToolHistory,
) -> Option<ScheduledTaskEvidence> {
    if !matches!(
        operation.name.as_str(),
        crate::scheduled_tasks::tools::CRON_UPDATE_TOOL_NAME
            | crate::scheduled_tasks::tools::CRON_DELETE_TOOL_NAME
    ) {
        return None;
    }
    let reference = ModelRef::<ScheduledTaskIdKind>::from(operation.input.get("id")?.as_str()?);
    let task_id = ids.resolve(&reference).ok()?;
    let task = snapshot
        .scheduled_tasks
        .iter()
        .find(|task| task.id == task_id.uuid())?;
    let creation_call_id = history.calls.iter().find_map(|call| {
        if task.creator != tickloop_harness_protocol::ScheduledTaskCreator::Agent
            || call.tool != crate::scheduled_tasks::tools::CRON_CREATE_TOOL_NAME
            || !matches!(call.outcome, HistoricalToolOutcome::ReturnedSuccess)
            || !snapshot.operations.iter().any(|prior| {
                prior.loop_id == operation.loop_id
                    && Some(prior.run_id) == call.run_id.map(Into::into)
                    && prior.call_id == call.call_id
                    && prior.name == call.tool
                    && prior.state == tickloop_harness_protocol::OperationState::Completed
            })
        {
            return None;
        }
        snapshot
            .messages
            .iter()
            .enumerate()
            .find_map(|(index, message)| {
                if message.role != Role::User
                    || snapshot.message_runs.get(&index).copied() != call.run_id.map(Into::into)
                {
                    return None;
                }
                message.content.iter().find_map(|content| match content {
                    Content::ToolResult {
                        tool_use_id,
                        content,
                        is_error: false,
                        ..
                    } if tool_use_id == &call.call_id
                        && serde_json::from_str::<Value>(&content.text())
                            .ok()
                            .and_then(|result| ScheduledTaskId::parse(result["id"].as_str()?))
                            .is_some_and(|created| created == task_id) =>
                    {
                        Some(call.call_id.clone())
                    }
                    _ => None,
                })
            })
    });
    Some(ScheduledTaskEvidence {
        id: task.id,
        creator: task.creator,
        creation_call_id,
        details: project_tool_input(
            "CronList",
            &serde_json::json!({"kind": task.kind, "prompt": task.prompt}),
            InputPurpose::Proposed,
        )?,
    })
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ApprovalSnapshot {
    pub auto_policy: AutoPolicy,
    /// Present only when the evidence was cut to fit the reviewer's input capacity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_truncation: Option<ContextTruncation>,
    pub tool_history: ToolHistory,
    pub reply_history: ReplyHistory,
    pub agent_messages: Vec<AgentMessageEvidence>,
    pub dependency_files: Option<Vec<DependencyEvidence>>,
    pub prepared_facts: Option<tickloop_harness_protocol::PreparedApprovalFacts>,
    pub creation_receipt: Option<TickCreationReceipt>,
    pub scheduled_task: Option<ScheduledTaskEvidence>,
    pub context_version: &'static str,
    pub review_template_version: &'static str,
    pub operation: ApprovalOperation,
    pub original_user_inputs: Vec<OriginalUserInput>,
    pub working_directory: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub permission_policy_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApprovalContextError {
    MissingOriginalUserInput,
}

pub(super) fn originals_for_active_chain(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
) -> Result<Vec<OriginalUserInput>, ApprovalContextError> {
    originals_for_active_chain_with_legacy(snapshot, bookkeeping, None, None)
}

fn originals_for_active_chain_with_legacy(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    legacy: Option<Vec<OriginalUserInput>>,
    proposed_call: Option<(Uuid, &str)>,
) -> Result<Vec<OriginalUserInput>, ApprovalContextError> {
    let boundary = snapshot
        .message_compactions
        .iter()
        .rev()
        .find_map(|(index, marker)| {
            matches!(marker, CompactionMarker::Boundary(_)).then_some(*index)
        });
    let mut originals = match boundary {
        Some(index) => match snapshot.message_compactions.get(&index) {
            Some(CompactionMarker::Boundary(boundary)) => boundary
                .original_user_inputs
                .clone()
                .or(legacy)
                .ok_or(ApprovalContextError::MissingOriginalUserInput)?,
            _ => unreachable!(),
        },
        None => Vec::new(),
    };
    let start = boundary.map_or(0, |index| index + 1);
    let captured_inputs = originals
        .iter()
        .map(|input| (input.chat_id, input.message_id))
        .collect::<HashSet<_>>();
    let mut pending_questions = HashMap::new();
    let proposed_position = proposed_call_position(snapshot, proposed_call);
    let mut call_counts = HashMap::new();
    for (index, message) in snapshot.messages.iter().enumerate().skip(start) {
        if message.role != Role::Assistant {
            continue;
        }
        let Some(run) = snapshot.message_runs.get(&index).copied() else {
            continue;
        };
        for content in &message.content {
            if let Content::ToolUse { id, .. } = content {
                *call_counts.entry((run, id.clone())).or_insert(0usize) += 1;
            }
        }
    }
    for (index, message) in snapshot.messages.iter().enumerate().skip(start) {
        if message.role == Role::Assistant {
            if let Some(run_id) = snapshot.message_runs.get(&index).copied() {
                for (block, content) in message.content.iter().enumerate() {
                    if let Content::ToolUse { id, name, .. } = content
                        && name == tickloop_harness_tools::ASK_USER_QUESTION_TOOL_NAME
                        && proposed_position.is_none_or(|position| (index, block) < position)
                    {
                        let message_id = snapshot
                            .original_message_ids
                            .get(&index)
                            .or_else(|| snapshot.message_ids.get(&index))
                            .copied()
                            .ok_or(ApprovalContextError::MissingOriginalUserInput)?;
                        pending_questions.insert((run_id, id.clone()), message_id);
                    }
                }
            }
            continue;
        }
        if message.role != Role::User {
            continue;
        }
        if snapshot.prompt_execution_modes.contains_key(&index)
            && !snapshot.inherited_prompts.contains(&index)
            && !snapshot
                .message_ids
                .get(&index)
                .is_some_and(|id| snapshot.agent_message_origins.contains_key(id))
        {
            let Some(run_id) = snapshot.message_runs.get(&index).copied() else {
                return Err(ApprovalContextError::MissingOriginalUserInput);
            };
            let Some(loop_id) = bookkeeping.run_loops.get(&run_id).copied() else {
                return Err(ApprovalContextError::MissingOriginalUserInput);
            };
            let Some(message_id) = snapshot
                .original_message_ids
                .get(&index)
                .or_else(|| snapshot.message_ids.get(&index))
                .copied()
            else {
                return Err(ApprovalContextError::MissingOriginalUserInput);
            };
            let text = snapshot
                .message_ids
                .get(&index)
                .and_then(|id| snapshot.human_family_origins.get(id))
                .map(|source| source.original_submission.clone())
                .unwrap_or_else(|| {
                    message
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            Content::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n")
                });
            if text.is_empty() {
                return Err(ApprovalContextError::MissingOriginalUserInput);
            }
            originals.push(OriginalUserInput {
                chat_id: snapshot.id.into(),
                loop_id: loop_id.into(),
                run_id: run_id.into(),
                message_id,
                text,
            });
        }
        let Some(run_id) = snapshot.message_runs.get(&index).copied() else {
            continue;
        };
        let Some(loop_id) = bookkeeping.run_loops.get(&run_id).copied() else {
            continue;
        };
        for content in &message.content {
            if let Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = content
                && let Some(message_id) = pending_questions.remove(&(run_id, tool_use_id.clone()))
                && !*is_error
            {
                if call_counts.get(&(run_id, tool_use_id.clone())) != Some(&1) {
                    return Err(ApprovalContextError::MissingOriginalUserInput);
                }
                if captured_inputs.contains(&(snapshot.id.into(), message_id)) {
                    continue;
                }
                let answer = content.text();
                if answer.trim().is_empty() {
                    continue;
                }
                // The result is the host-rendered, user-confirmed question/answer text. Its
                // originating assistant message gives it a stable identity across restoration.
                if let Some(previous) = originals
                    .iter_mut()
                    .find(|input| input.message_id == message_id)
                {
                    previous.text.push_str("\n\n");
                    previous.text.push_str(&answer);
                } else {
                    originals.push(OriginalUserInput {
                        chat_id: snapshot.id.into(),
                        loop_id: loop_id.into(),
                        run_id: run_id.into(),
                        message_id,
                        text: answer.into_owned(),
                    });
                }
            }
        }
    }
    let mut seen = HashSet::new();
    originals.retain(|input| seen.insert((input.chat_id, input.message_id)));
    Ok(originals)
}

/// The goal a child delegated from `snapshot` may weigh as the user's: the root's goal the user
/// set with /goal, unfinished, as a reference the child checks against the root at each review.
pub(super) fn delegated_goal(snapshot: &ChatSnapshot) -> Option<DelegatedGoal> {
    match &snapshot.delegation {
        Some(delegation) => delegation.trusted_goal,
        None => snapshot
            .goal
            .as_ref()
            .filter(|goal| is_users_unfinished_goal(goal))
            .map(|goal| DelegatedGoal {
                goal_id: goal.id,
                objective_revision: goal.objective_revision,
            }),
    }
}

fn is_users_unfinished_goal(goal: &GoalSnapshot) -> bool {
    goal.set_by == tickloop_harness_protocol::GoalAuthor::User && !goal.status.is_finished()
}

/// The root Chat's goal as it is now, `root_goal`, as one of the user's inputs, when the user set
/// or edited it with /goal and it is unfinished: the user's own request for the work it continues.
/// A goal the Agent set with CreateGoal is only the Agent's account of the request. A child
/// weighs it only while it is still the goal, at the revision, it was delegated under.
fn user_goal_input(
    snapshot: &ChatSnapshot,
    root_goal: Option<&GoalSnapshot>,
    operation: &OperationRecord,
) -> Option<OriginalUserInput> {
    let goal = root_goal.filter(|goal| is_users_unfinished_goal(goal))?;
    let (chat_id, loop_id) = match &snapshot.delegation {
        None => (snapshot.id, operation.loop_id),
        Some(delegation) => {
            let delegated_under = DelegatedGoal {
                goal_id: goal.id,
                objective_revision: goal.objective_revision,
            };
            if delegation.trusted_goal != Some(delegated_under) {
                return None;
            }
            (delegation.root_chat_id(), delegation.root_loop_id())
        }
    };
    Some(OriginalUserInput {
        chat_id: chat_id.into(),
        loop_id: loop_id.into(),
        run_id: operation.run_id.into(),
        message_id: goal.id,
        text: format!(
            "The user set this Chat's goal with /goal, for the Agent to keep working toward: {}",
            goal.objective
        ),
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn capture(
    ids: &IdContext,
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    operation: &OperationRecord,
    root_goal: Option<&GoalSnapshot>,
    legacy: Option<Vec<OriginalUserInput>>,
    folder: &std::path::Path,
    auto_policy: AutoPolicy,
) -> Result<ApprovalSnapshot, ApprovalContextError> {
    let originals = originals_for_active_chain_with_legacy(
        snapshot,
        bookkeeping,
        legacy,
        Some((operation.run_id.into(), &operation.call_id)),
    )?;
    let mut original_user_inputs = originals
        .into_iter()
        .filter(|input| input.chat_id == snapshot.id && input.loop_id == operation.loop_id)
        .collect::<Vec<_>>();
    let goal_input = user_goal_input(snapshot, root_goal, operation);
    if let Some(delegation) = &snapshot.delegation {
        let frozen = delegation
            .trusted_user_inputs
            .as_ref()
            .and_then(|value| serde_json::from_value::<Vec<OriginalUserInput>>(value.clone()).ok())
            .unwrap_or_default();
        let mut inherited = frozen
            .iter()
            .filter(|input| {
                delegation.lineage().iter().any(|parent| {
                    input.chat_id == parent.chat_id && input.loop_id == parent.loop_id
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if inherited.is_empty() && goal_input.is_none() {
            return Err(ApprovalContextError::MissingOriginalUserInput);
        }
        inherited.append(&mut original_user_inputs);
        original_user_inputs = inherited;
    }
    original_user_inputs.extend(goal_input);
    if original_user_inputs.is_empty() {
        return Err(ApprovalContextError::MissingOriginalUserInput);
    }
    let tool_history = tool_history(snapshot, operation);
    let scheduled_task = scheduled_task_evidence(ids, snapshot, operation, &tool_history);
    Ok(ApprovalSnapshot {
        context_truncation: None,
        tool_history,
        reply_history: capture_reply_history(
            snapshot,
            bookkeeping,
            Some((operation.run_id.into(), &operation.call_id)),
            &original_user_inputs,
            None,
        ),
        agent_messages: snapshot
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                let message_id = *snapshot.message_ids.get(&index)?;
                let source = snapshot.agent_message_origins.get(&message_id)?;
                Some(AgentMessageEvidence {
                    message_id,
                    source: source.clone(),
                    content: message
                        .content
                        .iter()
                        .map(|content| match content {
                            Content::Image { .. } => Content::Text {
                                text: IMAGE_PLACEHOLDER.into(),
                            },
                            content => content.clone(),
                        })
                        .collect(),
                })
            })
            .collect(),
        dependency_files: bookkeeping.dependency_baseline.as_ref().map(|baseline| {
            let current = dependency_fingerprints(folder);
            baseline
                .keys()
                .chain(current.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|path| DependencyEvidence {
                    path: path.clone(),
                    unchanged_since_loop_start: baseline.get(path).is_some()
                        && baseline.get(path) == current.get(path),
                })
                .collect()
        }),
        prepared_facts: None,
        creation_receipt: None,
        scheduled_task,
        context_version: APPROVAL_CONTEXT_VERSION,
        review_template_version: REVIEW_TEMPLATE_VERSION,
        operation: ApprovalOperation {
            id: operation.id,
            call_id: operation.call_id.clone(),
            loop_id: operation.loop_id.into(),
            run_id: operation.run_id.into(),
            tool: operation.name.clone(),
            input: project_tool_input(&operation.name, &operation.input, InputPurpose::Proposed)
                .unwrap_or(Value::Null),
        },
        original_user_inputs,
        working_directory: folder.to_path_buf(),
        additional_directories: snapshot.additional_directories.clone(),
        permission_policy_revision: snapshot.policy.revision,
        auto_policy,
    })
}

impl crate::Agent {
    /// `lines` are the Chat's stored Lines when the caller already loaded them.
    pub(super) async fn approval_history_ancestors(
        &self,
        snapshot: &ChatSnapshot,
        lines: Option<&[Line]>,
    ) -> Option<Vec<ChainEntry>> {
        let boundary = snapshot
            .message_compactions
            .iter()
            .rev()
            .find_map(|(index, marker)| match marker {
                CompactionMarker::Boundary(boundary) if !boundary.preserved_by_later_summary => {
                    snapshot.message_ids.get(index).copied()
                }
                _ => None,
            })?;
        match lines {
            Some(lines) => {
                crate::transcript::load_compaction_ancestors(lines, boundary, snapshot.id.into())
            }
            None => {
                let lines = self.stored_lines(snapshot.id).await?;
                crate::transcript::load_compaction_ancestors(&lines, boundary, snapshot.id.into())
            }
        }
    }
}

pub(super) fn append_nested_evidence(context: &mut ApprovalSnapshot, text: String) {
    use reply_history::{ReplyContext, ReplySource, ReplyTurn};
    let Some(input) = context.original_user_inputs.last() else {
        return;
    };
    context.reply_history.contexts.push(ReplyContext {
        chat_id: input.chat_id,
        user_message_id: input.message_id,
        preceding: vec![ReplyTurn {
            message_id: tickloop_ids::generate(),
            source: ReplySource::Assistant,
            text,
        }],
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness_protocol::{Message, ModelConfig, OperationState};

    /// A child weighs its root's goal as the user's only while the root's goal is still the one,
    /// at the revision, it was delegated under, the user's and unfinished.
    #[test]
    fn a_child_weighs_only_the_roots_current_goal_it_was_delegated_under() {
        use crate::goal::state::{GoalEvent, apply};
        use tickloop_harness_protocol::{GoalAuthor, GoalStatus};

        let set = |set_by| {
            apply(
                None,
                GoalEvent::Set {
                    id: Uuid::new_v4(),
                    objective: "Ship the export command".into(),
                    token_budget: None,
                    replace: false,
                    set_by,
                },
                1_000,
            )
            .unwrap()
            .goal
            .unwrap()
        };
        let mut root = crate::empty_snapshot(
            Uuid::new_v4().into(),
            Some("/project".into()),
            ModelConfig::unselected(),
        );
        root.goal = Some(set(GoalAuthor::User));
        let delegated = delegated_goal(&root).expect("the user's goal is delegated");
        let mut child = crate::empty_snapshot(
            Uuid::new_v4().into(),
            Some("/project".into()),
            ModelConfig::unselected(),
        );
        child.delegation = Some(tickloop_harness_protocol::DelegationMetadata {
            delegation_id: Uuid::new_v4(),
            parent_chat_id: root.id,
            parent_loop_id: root.chat_loop.id,
            parent_run_id: Uuid::new_v4().into(),
            parent_call_id: "s1".into(),
            ancestors: Vec::new(),
            task: "write the export".into(),
            ceiling: tickloop_harness_protocol::DelegationCeiling {
                folder: child.folder.clone(),
                additional_directories: Vec::new(),
                policy: child.policy.clone(),
                session_denials: None,
                host_context: None,
                host_tools: Default::default(),
                tick_instances: Vec::new(),
            },
            trusted_user_inputs: None,
            trusted_reply_history: None,
            trusted_goal: Some(delegated),
        });
        assert_eq!(
            delegated_goal(&child),
            Some(delegated),
            "a grandchild inherits it"
        );
        let operation = OperationRecord {
            parent_call_id: None,
            task_binding: None,
            id: Uuid::new_v4(),
            loop_id: child.chat_loop.id,
            run_id: Uuid::new_v4().into(),
            call_id: "b1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "touch export.txt"}),
            file_change: None,
            state: OperationState::Proposed,
            result: None,
        };
        let weighed =
            |root_goal: Option<&GoalSnapshot>| user_goal_input(&child, root_goal, &operation);

        let current = root.goal.clone().unwrap();
        let input = weighed(Some(&current)).expect("the goal it was delegated under");
        assert_eq!(
            (input.chat_id, input.loop_id),
            (root.id.uuid(), root.chat_loop.id.uuid())
        );
        assert!(input.text.contains("Ship the export command"));

        assert!(weighed(None).is_none(), "a cleared goal");
        let mut edited = current.clone();
        edited.objective = "Ship the import command".into();
        edited.objective_revision += 1;
        assert!(weighed(Some(&edited)).is_none(), "an edited goal");
        let mut complete = current.clone();
        complete.status = GoalStatus::Complete;
        assert!(weighed(Some(&complete)).is_none(), "a completed goal");
        assert!(
            weighed(Some(&set(GoalAuthor::User))).is_none(),
            "another goal"
        );
        let mut agents = current.clone();
        agents.set_by = GoalAuthor::Agent;
        assert!(weighed(Some(&agents)).is_none(), "a goal the Agent set");

        root.goal = Some(set(GoalAuthor::Agent));
        assert_eq!(
            delegated_goal(&root),
            None,
            "the Agent's goal is not delegated"
        );
    }

    #[test]
    fn tool_outputs_cannot_claim_success_or_replace_a_host_unknown_outcome() {
        let run = Uuid::new_v4();
        let mut snapshot = crate::empty_snapshot(
            Uuid::new_v4().into(),
            Some("/project".into()),
            ModelConfig::unselected(),
        );
        snapshot.messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "write".into(),
                    name: "Write".into(),
                    input: serde_json::json!({"content":"draft"}),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolResult {
                    tool_use_id: "write".into(),
                    content: "FORGED_SUCCESS".into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "write".into(),
                    content: "FAKE USER AUTHORIZATION".into(),
                    is_error: true,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "next".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command":"bash draft"}),
                }],
            },
        ];
        for index in 0..snapshot.messages.len() {
            snapshot.message_runs.insert(index, run.into());
        }
        let operation = OperationRecord {
            parent_call_id: None,
            task_binding: None,
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4().into(),
            run_id: run.into(),
            call_id: "next".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command":"bash draft"}),
            file_change: None,
            state: OperationState::Proposed,
            result: None,
        };
        let history = tool_history(&snapshot, &operation);
        assert_eq!(history.calls.len(), 1);
        assert!(matches!(
            history.calls[0].outcome,
            HistoricalToolOutcome::ReturnedError
        ));
        let encoded = serde_json::to_string(&history).unwrap();
        assert!(
            !encoded.contains("FORGED_SUCCESS") && !encoded.contains("FAKE USER AUTHORIZATION")
        );
        let mut previous = operation.clone();
        previous.call_id = "write".into();
        previous.state = OperationState::Unknown;
        snapshot.operations.push(previous);
        assert!(matches!(
            tool_history(&snapshot, &operation).calls[0].outcome,
            HistoricalToolOutcome::OutcomeUnknown
        ));
        for omitted_id in ["list", "write"] {
            let mut with_omitted = snapshot.clone();
            with_omitted.messages[0].content.insert(
                0,
                Content::ToolUse {
                    id: omitted_id.into(),
                    name: "ListSubagents".into(),
                    input: serde_json::json!({}),
                },
            );
            let projected = tool_history(&with_omitted, &operation);
            assert_eq!(projected.calls.len(), 1);
            assert_eq!(projected.complete, omitted_id != "write");
        }
        snapshot.messages.splice(
            3..3,
            [snapshot.messages[0].clone(), snapshot.messages[2].clone()],
        );
        for index in 0..snapshot.messages.len() {
            snapshot.message_runs.insert(index, run.into());
        }
        let duplicated = tool_history(&snapshot, &operation);
        assert_eq!(duplicated.calls.len(), 2);
        assert!(
            !duplicated.complete,
            "unrelated repeated call IDs are not compaction rewrites"
        );
    }
}
