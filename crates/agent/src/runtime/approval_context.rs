use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::auto_rules::AutoPolicy;
use super::{ChatSnapshot, OperationRecord};
use crate::model::{Content, Role};

pub(super) const APPROVAL_CONTEXT_VERSION: &str = "trusted-approval-v2";
pub(super) const REVIEW_TEMPLATE_VERSION: &str = "auto-review-v2";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct OriginalUserInput {
    pub chat_id: Uuid,
    pub loop_id: Uuid,
    pub run_id: Uuid,
    pub message_id: Uuid,
    pub text: String,
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

#[derive(Clone, Debug, Serialize)]
pub(super) struct ApprovalSnapshot {
    pub context_version: &'static str,
    pub review_template_version: &'static str,
    pub operation: ApprovalOperation,
    pub original_user_inputs: Vec<OriginalUserInput>,
    pub working_directory: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub permission_policy_revision: u64,
    pub auto_policy: AutoPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApprovalContextError {
    MissingOriginalUserInput,
}

pub(super) fn originals_for_active_chain(
    snapshot: &ChatSnapshot,
) -> Result<Vec<OriginalUserInput>, ApprovalContextError> {
    originals_for_active_chain_with_legacy(snapshot, None, None)
}

fn originals_for_active_chain_with_legacy(
    snapshot: &ChatSnapshot,
    legacy: Option<Vec<OriginalUserInput>>,
    proposed_call_id: Option<&str>,
) -> Result<Vec<OriginalUserInput>, ApprovalContextError> {
    let boundary = snapshot
        .message_compactions
        .iter()
        .rev()
        .find_map(|(index, marker)| {
            matches!(marker, super::CompactionMarker::Boundary(_)).then_some(*index)
        });
    let mut originals = match boundary {
        Some(index) => match snapshot.message_compactions.get(&index) {
            Some(super::CompactionMarker::Boundary(boundary)) => boundary
                .original_user_inputs
                .clone()
                .or(legacy)
                .ok_or(ApprovalContextError::MissingOriginalUserInput)?,
            _ => unreachable!(),
        },
        None => Vec::new(),
    };
    let start = boundary.map_or(0, |index| index + 1);
    let mut pending_questions = HashMap::new();
    for (index, message) in
        snapshot
            .messages
            .iter()
            .enumerate()
            .skip(start)
            .take_while(|(_, message)| {
                !message.content.iter().any(|content| {
            matches!(content, Content::ToolUse { id, .. } if proposed_call_id == Some(id.as_str()))
        })
            })
    {
        if message.role == Role::Assistant {
            if let Some(run_id) = snapshot.message_runs.get(&index).copied() {
                for content in &message.content {
                    if let Content::ToolUse { id, name, .. } = content
                        && name == crate::runtime::ASK_USER_QUESTION_TOOL_NAME
                    {
                        let message_id = snapshot
                            .message_ids
                            .get(&index)
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
        {
            let Some(run_id) = snapshot.message_runs.get(&index).copied() else {
                return Err(ApprovalContextError::MissingOriginalUserInput);
            };
            let Some(loop_id) = snapshot.run_loops.get(&run_id).copied() else {
                return Err(ApprovalContextError::MissingOriginalUserInput);
            };
            let Some(message_id) = snapshot
                .original_user_message_ids
                .get(&index)
                .or_else(|| snapshot.message_ids.get(&index))
                .copied()
            else {
                return Err(ApprovalContextError::MissingOriginalUserInput);
            };
            let text = message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            if text.is_empty() {
                return Err(ApprovalContextError::MissingOriginalUserInput);
            }
            originals.push(OriginalUserInput {
                chat_id: snapshot.id,
                loop_id,
                run_id,
                message_id,
                text,
            });
        }
        let Some(run_id) = snapshot.message_runs.get(&index).copied() else {
            continue;
        };
        let Some(loop_id) = snapshot.run_loops.get(&run_id).copied() else {
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
                        chat_id: snapshot.id,
                        loop_id,
                        run_id,
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

pub(super) fn capture(
    snapshot: &ChatSnapshot,
    operation: &OperationRecord,
    legacy: Option<Vec<OriginalUserInput>>,
    auto_policy: AutoPolicy,
) -> Result<ApprovalSnapshot, ApprovalContextError> {
    let originals =
        originals_for_active_chain_with_legacy(snapshot, legacy, Some(&operation.call_id))?;
    let original_user_inputs = originals
        .into_iter()
        .filter(|input| input.chat_id == snapshot.id && input.loop_id == operation.loop_id)
        .collect::<Vec<_>>();
    if original_user_inputs.is_empty() {
        return Err(ApprovalContextError::MissingOriginalUserInput);
    }
    Ok(ApprovalSnapshot {
        context_version: APPROVAL_CONTEXT_VERSION,
        review_template_version: REVIEW_TEMPLATE_VERSION,
        operation: ApprovalOperation {
            id: operation.id,
            call_id: operation.call_id.clone(),
            loop_id: operation.loop_id,
            run_id: operation.run_id,
            tool: operation.name.clone(),
            input: operation.input.clone(),
        },
        original_user_inputs,
        working_directory: snapshot.workdir.clone(),
        additional_directories: snapshot.additional_directories.clone(),
        permission_policy_revision: snapshot.policy.revision,
        auto_policy,
    })
}
