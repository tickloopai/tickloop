use super::{AgentError, ChatSnapshot, LoopId, OperationState};
use tickloop_agent_model::validate_model_for_resume_in_root;
use tickloop_agent_protocol::{Content, Message, Role, ToolResult};
use tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME;

pub(super) fn before_resume(
    snapshot: &mut ChatSnapshot,
    loop_id: LoopId,
    state_root: Option<&std::path::Path>,
) -> Result<(), AgentError> {
    for directory in std::iter::once(&snapshot.workdir).chain(&snapshot.additional_directories) {
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

pub(super) fn close_dangling_tool_calls(
    snapshot: &mut ChatSnapshot,
    only_run: Option<super::RunId>,
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
                content: "User clarification was interrupted before an answer was received. Ask again if needed after the user manually resumes this goal.".into(),
                is_error: true,
                output_ref: None,
            });
        }
    }
    let calls = super::context::history_calls(snapshot)?;
    let mut groups: std::collections::BTreeMap<Option<super::RunId>, Vec<Content>> =
        std::collections::BTreeMap::new();
    for call in calls
        .into_iter()
        .filter(|call| call.result.is_none() && only_run.is_none_or(|run| call.run_id == Some(run)))
    {
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
        if recorded.as_ref().is_none_or(|operation| {
            !matches!(
                operation.state,
                OperationState::Completed | OperationState::Failed
            )
        }) {
            snapshot.tool_call_recoveries.push(super::ToolCallRecovery {
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
        let result = match recorded {
            Some(operation) if matches!(operation.state, OperationState::Completed | OperationState::Failed) => operation.result.clone().ok_or_else(|| AgentError::Invalid(format!("completed operation {} has no durable result", operation.id)))?,
            Some(operation) if matches!(operation.state, OperationState::Admitted | OperationState::Unknown) => {
                operation.state = OperationState::Unknown;
                ToolResult { content: format!("Operation {} has an unknown outcome after interruption. Do not automatically repeat it; inspect current state before proposing it again.", operation.id), is_error: true, output_ref: None }
            }
            Some(operation) => {
                let result = interrupted_before_admission();
                operation.state = OperationState::Failed;
                operation.result = Some(result.clone());
                result
            }
            None => ToolResult { content: "Tool call was interrupted and its outcome is unknown; inspect current state before proposing it again.".into(), is_error: true, output_ref: None },
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

fn interrupted_before_admission() -> ToolResult {
    ToolResult {
        content: "Interrupted before tool admission; the operation was not executed.".into(),
        is_error: true,
        output_ref: None,
    }
}

impl ChatSnapshot {
    /// 本进程内结果未知的操作，作为宿主状态告知模型。
    pub(crate) fn current_operation_uncertainties(&self) -> Vec<serde_json::Value> {
        self.operations
            .iter()
            .filter(|operation| operation.state == OperationState::Unknown)
            .map(|operation| {
                serde_json::json!({
                    "id": operation.id,
                    "loop_id": operation.loop_id,
                    "tool": operation.name,
                })
            })
            .collect()
    }
}
