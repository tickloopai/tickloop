use std::{
    collections::{HashMap, HashSet},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::ChatBookkeeping;
use tickloop_agent_protocol::{
    AutoPolicy, ChatSnapshot, CompactionMarker, Content, OperationRecord, OriginalUserInput, Role,
};

pub(super) const APPROVAL_CONTEXT_VERSION: &str = "trusted-approval-v3";
pub(super) const REVIEW_TEMPLATE_VERSION: &str = "auto-review-v4";

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

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
pub(super) struct ApprovalSnapshot {
    pub dependency_files: Option<Vec<DependencyEvidence>>,
    pub prepared_facts: Option<tickloop_agent_protocol::PreparedApprovalFacts>,
    pub creation_receipt: Option<TickCreationReceipt>,
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
    bookkeeping: &ChatBookkeeping,
) -> Result<Vec<OriginalUserInput>, ApprovalContextError> {
    originals_for_active_chain_with_legacy(snapshot, bookkeeping, None, None)
}

fn originals_for_active_chain_with_legacy(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    legacy: Option<Vec<OriginalUserInput>>,
    proposed_call_id: Option<&str>,
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
                        && name == tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME
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
                .original_user_message_ids
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
    bookkeeping: &ChatBookkeeping,
    operation: &OperationRecord,
    legacy: Option<Vec<OriginalUserInput>>,
    auto_policy: AutoPolicy,
) -> Result<ApprovalSnapshot, ApprovalContextError> {
    let originals = originals_for_active_chain_with_legacy(
        snapshot,
        bookkeeping,
        legacy,
        Some(&operation.call_id),
    )?;
    let mut original_user_inputs = originals
        .into_iter()
        .filter(|input| input.chat_id == snapshot.id && input.loop_id == operation.loop_id)
        .collect::<Vec<_>>();
    if let Some(delegation) = &snapshot.delegation {
        let frozen = delegation
            .trusted_user_inputs
            .as_ref()
            .and_then(|value| serde_json::from_value::<Vec<OriginalUserInput>>(value.clone()).ok())
            .filter(|inputs| !inputs.is_empty())
            .ok_or(ApprovalContextError::MissingOriginalUserInput)?;
        let mut inherited = frozen
            .iter()
            .filter(|input| {
                input.chat_id == delegation.parent_chat_id
                    && input.loop_id == delegation.parent_loop_id
            })
            .cloned()
            .collect::<Vec<_>>();
        if inherited.is_empty() {
            return Err(ApprovalContextError::MissingOriginalUserInput);
        }
        inherited.append(&mut original_user_inputs);
        original_user_inputs = inherited;
    }
    if original_user_inputs.is_empty() {
        return Err(ApprovalContextError::MissingOriginalUserInput);
    }
    Ok(ApprovalSnapshot {
        dependency_files: bookkeeping
            .dependency_baselines
            .get(&operation.loop_id)
            .map(|baseline| {
                let current = dependency_fingerprints(&snapshot.workdir);
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
