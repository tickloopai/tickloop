use std::collections::HashSet;

use super::AgentError;
use tickloop_harness_model::validate_model_for_resume_in_root;
use tickloop_harness_protocol::OperationId;
use tickloop_harness_protocol::projection;
use tickloop_harness_protocol::{
    ChatSnapshot, Content, LoopId, Message, OperationState, Role, RunId, ToolCallRecovery,
    ToolResult,
};
use tickloop_harness_tools::ASK_USER_QUESTION_TOOL_NAME;
use tickloop_ids::IdContext;

pub(super) fn before_resume(
    snapshot: &mut ChatSnapshot,
    loop_id: LoopId,
    state_root: Option<&std::path::Path>,
) -> Result<(), AgentError> {
    // A Chat without a Folder has no local directory or credential to check.
    if snapshot.folder.is_some() {
        for directory in snapshot
            .folder
            .iter()
            .chain(&snapshot.additional_directories)
        {
            let resolved = directory.canonicalize().map_err(|_| {
                AgentError::Invalid(format!(
                    "working or authorized directory is unavailable: {}",
                    directory.display()
                ))
            })?;
            if resolved != *directory || !resolved.is_dir() {
                return Err(AgentError::Invalid(format!(
                    "working or authorized directory changed: {}",
                    directory.display()
                )));
            }
        }
        validate_model_for_resume_in_root(&snapshot.model, state_root)
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
    }
    for pending in &mut snapshot.pending_inputs {
        if pending.receipt.loop_id == loop_id
            && pending.trigger_source.is_none()
            && pending.agent_message.is_none()
        {
            pending.armed = true;
        }
    }
    Ok(())
}

/// The result a load finds recorded for a dangling call whose effect stayed inside the family,
/// such as the child a `Subagent` call delegated; `None` when its outcome is unknown.
pub(super) type CallReconciler<'a> =
    dyn Fn(&ChatSnapshot, &projection::HistoryCall) -> Option<ToolResult> + 'a;

pub(super) fn close_dangling_tool_calls(
    ids: &IdContext,
    snapshot: &mut ChatSnapshot,
    only_run: Option<RunId>,
) -> Result<(), AgentError> {
    close_calls(ids, snapshot, only_run, &HashSet::new(), &|_, _| None)
}

/// Closes the dangling calls of every Run except those in `kept`, whose Runs are rebuilt; a call
/// `reconcile` finds the result of gets that result instead of an unknown outcome.
pub(super) fn close_dangling_tool_calls_except(
    ids: &IdContext,
    snapshot: &mut ChatSnapshot,
    kept: &HashSet<RunId>,
    reconcile: &CallReconciler<'_>,
) -> Result<(), AgentError> {
    close_calls(ids, snapshot, None, kept, reconcile)
}

fn close_calls(
    ids: &IdContext,
    snapshot: &mut ChatSnapshot,
    only_run: Option<RunId>,
    kept: &HashSet<RunId>,
    reconcile: &CallReconciler<'_>,
) -> Result<(), AgentError> {
    for operation in &mut snapshot.operations {
        if only_run.is_none_or(|run| operation.run_id == run)
            && operation.name == ASK_USER_QUESTION_TOOL_NAME
            && matches!(
                operation.state,
                OperationState::Admitted | OperationState::Unknown
            )
            && operation.result.is_none()
        {
            operation.state = OperationState::Failed;
            operation.result = Some(ToolResult {
                content: "User clarification was interrupted before an answer was received. Ask again if needed after the user continues the Chat.".into(),
                is_error: true,
                output_ref: None,
            });
        }
    }
    for operation in &mut snapshot.operations {
        if only_run.is_none_or(|run| operation.run_id == run)
            && operation.state == OperationState::Admitted
        {
            operation.state = OperationState::Unknown;
        }
    }
    let calls = projection::history_calls(snapshot)?;
    let mut groups: std::collections::BTreeMap<Option<RunId>, Vec<Content>> =
        std::collections::BTreeMap::new();
    for call in calls.into_iter().filter(|call| {
        call.result.is_none()
            && only_run.is_none_or(|run| call.run_id == Some(run))
            && call.run_id.is_none_or(|run| !kept.contains(&run))
    }) {
        let reconciled = reconcile(snapshot, &call);
        let nested_summary = interrupted_nested_calls(snapshot, &call);
        let matching = snapshot
            .operations
            .iter()
            .enumerate()
            .filter(|(_, operation)| {
                operation.call_id == call.call_id
                    && call.run_id.is_none_or(|run| operation.run_id == run)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            return Err(AgentError::Invalid("archive lacks run identity for repeated tool operations; restore an unambiguous checkpoint".into()));
        }
        let recorded = matching
            .first()
            .map(|index| &mut snapshot.operations[*index]);
        let result_run = recorded
            .as_ref()
            .map(|operation| operation.run_id)
            .or(call.run_id);
        if reconciled.is_none()
            && recorded.as_ref().is_none_or(|operation| {
                !matches!(
                    operation.state,
                    OperationState::Completed | OperationState::Failed
                )
            })
        {
            snapshot.tool_call_recoveries.push(ToolCallRecovery {
                run_id: result_run,
                call_id: call.call_id.clone(),
                was_admitted: recorded.as_ref().map(|operation| {
                    matches!(
                        operation.state,
                        OperationState::Admitted | OperationState::Unknown
                    )
                }),
            });
        }
        let result = if let Some(result) = reconciled {
            if let Some(operation) = recorded {
                operation.state = if result.is_error {
                    OperationState::Failed
                } else {
                    OperationState::Completed
                };
                operation.result = Some(result.clone());
            }
            result
        } else if let Some(summary) = nested_summary.filter(|_| {
            recorded.as_ref().is_none_or(|operation| {
                !matches!(
                    operation.state,
                    OperationState::Completed | OperationState::Failed
                )
            })
        }) {
            if let Some(operation) = recorded {
                operation.state = OperationState::Unknown;
            }
            ToolResult {
                content: summary,
                is_error: true,
                output_ref: None,
            }
        } else {
            match recorded {
                Some(operation) if matches!(operation.state, OperationState::Completed | OperationState::Failed) => operation.result.clone().ok_or_else(|| AgentError::Invalid(format!("completed operation {} has no durable result", operation.id)))?,
                Some(operation) if matches!(operation.state, OperationState::Admitted | OperationState::Unknown) => {
                    operation.state = OperationState::Unknown;
                    ToolResult { content: format!("Operation {} has an unknown outcome after interruption. Do not automatically repeat it; inspect current state before proposing it again.", ids.model_text(OperationId::from(operation.id))), is_error: true, output_ref: None }
            }
            Some(operation) => {
                let result = interrupted_before_admission();
                operation.state = OperationState::Failed;
                operation.result = Some(result.clone());
                result
            }
            None => ToolResult { content: "Tool call was interrupted and its outcome is unknown; inspect current state before proposing it again.".into(), is_error: true, output_ref: None },
            }
        };
        groups
            .entry(result_run)
            .or_default()
            .push(Content::ToolResult {
                tool_use_id: call.call_id,
                content: result.content.into(),
                is_error: result.is_error,
            });
    }
    for (origin, results) in groups {
        if let Some(run_id) = origin {
            snapshot
                .message_runs
                .insert(snapshot.messages.len(), run_id);
        }
        snapshot.messages.push(Message {
            role: Role::User,
            content: results,
        });
    }
    Ok(())
}

const MAX_REASON_CHARACTERS: usize = 200;

/// What an interrupted batch did, call by call, when it made any calls: their effects stay, so
/// the model decides how to go on instead of the load running anything again.
fn interrupted_nested_calls(
    snapshot: &ChatSnapshot,
    parent: &projection::HistoryCall,
) -> Option<String> {
    let lines = snapshot
        .operations
        .iter()
        .filter(|operation| {
            operation.parent_call_id.as_deref() == Some(parent.call_id.as_str())
                && parent.run_id.is_none_or(|run| operation.run_id == run)
        })
        .map(|operation| {
            let outcome = match operation.state {
                OperationState::Completed => "completed".to_owned(),
                OperationState::Unknown | OperationState::Admitted => "outcome unknown; it may have taken effect, so inspect current state before repeating it".to_owned(),
                OperationState::Proposed => "not executed".to_owned(),
                OperationState::Failed => format!(
                    "did not complete: {}",
                    operation
                        .result
                        .as_ref()
                        .and_then(|result| result.content.lines().next())
                        .unwrap_or("no result")
                        .chars()
                        .take(MAX_REASON_CHARACTERS)
                        .collect::<String>()
                ),
            };
            format!("- {}: {outcome}", operation.name)
        })
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| {
        format!(
            "This call was interrupted before it returned, for example because the agent process exited, and it will not be rerun. The calls it made:\n{}",
            lines.join("\n")
        )
    })
}

fn interrupted_before_admission() -> ToolResult {
    ToolResult {
        content: "Interrupted before tool admission; the operation was not executed.".into(),
        is_error: true,
        output_ref: None,
    }
}
