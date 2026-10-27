//! Shared preparation, admission and execution for calls made inside a model tool.
use crate::{
    Agent, LoopSnapshotExt, ToolInvocation, ToolRegistry,
    tool_execution::{ToolCallError, ToolCallOutcome},
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tickloop_harness_protocol::{EventKind, OperationRecord, OperationState, Receipt, ToolResult};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_BATCH_CALLS: usize = 10;

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NestedStatus {
    Completed,
    Error,
    Denied,
    NotExecuted,
    Unknown,
}

#[derive(Clone, Serialize)]
pub(crate) struct NestedOutcome {
    pub call_id: String,
    pub name: String,
    pub status: NestedStatus,
    pub result: ToolResult,
    #[serde(skip)]
    pub images: Vec<tickloop_harness_protocol::Image>,
    #[serde(skip)]
    pub sources: Vec<tickloop_harness_protocol::CitationSource>,
}

pub(crate) struct NestedCalls {
    agent: Agent,
    receipt: Receipt,
    message_id: Uuid,
    parent: String,
    registry: ToolRegistry,
    stop: CancellationToken,
    stop_reason: std::sync::Mutex<Option<String>>,
    pending: std::sync::Mutex<std::collections::BTreeMap<String, (usize, String, Value)>>,
    permissions: std::sync::Mutex<std::collections::BTreeSet<Uuid>>,
    completed: Mutex<Vec<(Value, Value)>>,
}

tokio::task_local! {
    static CURRENT: Arc<NestedCalls>;
    static CALL_INDEX: usize;
}

pub(crate) fn parent_call_id() -> Option<String> {
    CURRENT.try_with(|scope| scope.parent.clone()).ok()
}

pub(crate) fn parent_call_index() -> Option<usize> {
    CALL_INDEX.try_with(|index| *index).ok()
}

pub(crate) async fn parent_call() -> Option<tickloop_harness_protocol::ProposedToolCall> {
    let scope = CURRENT.try_with(Arc::clone).ok()?;
    let chats = scope.agent.inner.chats.lock().await;
    let operation = chats
        .get(&scope.receipt.chat_id)?
        .snapshot
        .operations
        .iter()
        .find(|operation| {
            operation.call_id == scope.parent && operation.run_id == scope.receipt.run_id
        })?;
    Some(tickloop_harness_protocol::ProposedToolCall {
        call_id: operation.call_id.clone(),
        name: operation.name.clone(),
        input: operation.input.clone(),
    })
}

pub(crate) fn register_permission(id: Uuid) {
    let _ = CURRENT.try_with(|scope| scope.permissions.lock().unwrap().insert(id));
}

pub(crate) fn is_nested() -> bool {
    CURRENT.try_with(|_| true).unwrap_or(false)
}
pub(crate) fn stop_after_denial(reason: &str) {
    let _ = CURRENT.try_with(|scope| {
        *scope.stop_reason.lock().unwrap() = Some(reason.to_owned());
        scope.stop.cancel();
    });
}

pub(crate) fn check_dispatch() -> Result<(), String> {
    if CURRENT
        .try_with(|scope| scope.stop_token().is_cancelled())
        .unwrap_or(false)
    {
        Err("Nested execution stopped after Auto rejection; call not dispatched".into())
    } else {
        Ok(())
    }
}

pub(crate) async fn stopped() {
    match CURRENT.try_with(|scope| scope.stop.clone()) {
        Ok(stop) => stop.cancelled().await,
        Err(_) => std::future::pending::<()>().await,
    }
}

impl NestedCalls {
    pub(crate) fn stop_token(&self) -> CancellationToken {
        self.stop.clone()
    }

    pub(crate) async fn execute(
        self: &Arc<Self>,
        call_index: usize,
        name: String,
        input: Value,
        cancel: CancellationToken,
    ) -> NestedOutcome {
        let call_id = tickloop_ids::generate().to_string();
        self.pending
            .lock()
            .unwrap()
            .insert(call_id.clone(), (call_index, name.clone(), input.clone()));
        let forbidden = matches!(
            name.as_str(),
            "ExecuteTools"
                | "AskUserQuestion"
                | "EnterPlanMode"
                | "ExitPlanMode"
                | "Subagent"
                | "Skill"
                | "SendMessage"
                | "StopSubagent"
                | "SubagentHandback"
        ) || crate::scheduled_tasks::tools::is_scheduled_task_tool(&name)
            || matches!(name.as_str(), "CreateGoal" | "GetGoal" | "UpdateGoal");
        let registration = self.registry.resolve_nested(&name);
        let unavailable = forbidden
            || registration.is_none()
            || self.stop_token().is_cancelled()
            || cancel.is_cancelled();
        let outcome = if unavailable {
            NestedOutcome { images:vec![],sources:vec![],call_id:call_id.clone(), name:name.clone(), status:NestedStatus::NotExecuted,
                result:ToolResult {content:"Tool unavailable, prohibited inside a nested call, or batch stopped before dispatch".into(),is_error:true,output_ref:None} }
        } else {
            let invocation =
                ToolInvocation::new(call_id.clone(), name.clone(), input.clone(), registration);
            let result = CALL_INDEX.scope(call_index, CURRENT
                .scope(self.clone(), async {
                    tokio::select! {
                        biased;
                        result = Box::pin(self.agent.execute_registered_call(&self.receipt, self.message_id, invocation, cancel.clone())) => result,
                        _ = cancel.cancelled() => Err("Nested call cancelled; outcome is unknown if already dispatched".into()),
                    }
                }))
                .await;
            match result {
                Ok(outcome) => {
                    let status = if outcome.denial.is_some() || outcome.is_rejected_by_user {
                        NestedStatus::Denied
                    } else if outcome.host_call_status
                        == Some(crate::host_tools::HostCallStatus::OutcomeUnknown)
                    {
                        NestedStatus::Unknown
                    } else if outcome.host_call_status
                        == Some(crate::host_tools::HostCallStatus::NotDispatched)
                    {
                        NestedStatus::NotExecuted
                    } else if outcome.result.is_error {
                        NestedStatus::Error
                    } else {
                        NestedStatus::Completed
                    };
                    let images = outcome.images.clone();
                    let sources = outcome.sources.clone().unwrap_or_default();
                    let result = self
                        .agent
                        .complete_nested_call(&self.receipt, &call_id, &name, &input, outcome)
                        .await
                        .unwrap_or_else(|error| ToolResult {
                            content: error,
                            is_error: true,
                            output_ref: None,
                        });
                    NestedOutcome {
                        images,
                        sources,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        status,
                        result,
                    }
                }
                Err(error) => {
                    if let Some(denial) = error.denial {
                        self.agent.record_final_denial(*denial).await;
                    }
                    let admitted = self.agent.inner.chats.lock().await[&self.receipt.chat_id]
                        .snapshot
                        .operations
                        .iter()
                        .any(|operation| {
                            operation.call_id == call_id
                                && matches!(
                                    operation.state,
                                    OperationState::Admitted | OperationState::Unknown
                                )
                        });
                    NestedOutcome {
                        images: vec![],
                        sources: vec![],
                        call_id: call_id.clone(),
                        name: name.clone(),
                        status: if admitted {
                            NestedStatus::Unknown
                        } else {
                            NestedStatus::NotExecuted
                        },
                        result: ToolResult {
                            content: error.message,
                            is_error: true,
                            output_ref: None,
                        },
                    }
                }
            }
        };
        self.completed
            .lock()
            .await
            .push((input.clone(), serde_json::to_value(&outcome).unwrap()));
        let mut chats = self.agent.inner.chats.lock().await;
        let chat = chats.get_mut(&self.receipt.chat_id).unwrap();
        if !chat.snapshot.operations.iter().any(|operation| {
            operation.run_id == self.receipt.run_id && operation.call_id == call_id
        }) {
            let operation = OperationRecord {
                parent_call_id: Some(self.parent.clone()),
                parent_call_index: Some(call_index),
                task_binding: None,
                id: tickloop_ids::generate(),
                loop_id: self.receipt.loop_id,
                run_id: self.receipt.run_id,
                call_id: call_id.clone(),
                name: name.clone(),
                input,
                file_change: None,
                state: OperationState::Failed,
                result: Some(outcome.result.clone()),
            };
            self.agent
                .record_nested_operation(self.receipt.chat_id, &operation);
            chat.snapshot.operations.push(operation);
        }
        if let Some(operation) = chat.snapshot.operations.iter_mut().find(|operation| {
            operation.run_id == self.receipt.run_id && operation.call_id == call_id
        }) {
            if outcome.status == NestedStatus::Unknown {
                operation.state = OperationState::Unknown;
            } else if outcome.status == NestedStatus::NotExecuted {
                operation.state = OperationState::Failed;
            }
            operation.result = Some(outcome.result.clone());
            self.agent
                .record_nested_operation(self.receipt.chat_id, operation);
        }
        self.agent.emit(
            chat,
            &self.receipt,
            EventKind::ToolCompleted {
                call_id,
                result: outcome.result.clone(),
                sources: vec![],
            },
        );
        self.agent.save(&chat.snapshot);
        self.pending.lock().unwrap().remove(&outcome.call_id);
        outcome
    }

    /// Settles calls and confirmations even when the engine dropped their futures while they
    /// were waiting for runtime locks. The parent awaits this after the bounded callback wait.
    pub(crate) async fn finish(&self) {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        let permissions = std::mem::take(&mut *self.permissions.lock().unwrap());
        let mut chats = self.agent.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&self.receipt.chat_id) else {
            return;
        };
        let mut approvals = self.agent.inner.approvals.lock().await;
        chat.snapshot
            .permissions
            .retain(|request| !permissions.contains(&request.id));
        for id in permissions {
            approvals.remove(&id);
        }
        drop(approvals);
        if chat.snapshot.permissions.is_empty()
            && chat.active_run == Some(self.receipt.run_id)
            && chat.snapshot.chat_loop.state
                == tickloop_harness_protocol::LoopState::WaitingPermission
        {
            chat.snapshot
                .chat_loop
                .set_state(tickloop_harness_protocol::LoopState::Running);
        }
        for (call_id, (call_index, name, input)) in pending {
            let index = chat.snapshot.operations.iter().position(|operation| {
                operation.run_id == self.receipt.run_id && operation.call_id == call_id
            });
            let index = index.unwrap_or_else(|| {
                chat.snapshot.operations.push(OperationRecord {
                    parent_call_id: Some(self.parent.clone()),
                    parent_call_index: Some(call_index),
                    task_binding: None,
                    id: tickloop_ids::generate(),
                    loop_id: self.receipt.loop_id,
                    run_id: self.receipt.run_id,
                    call_id: call_id.clone(),
                    name,
                    input,
                    file_change: None,
                    state: OperationState::Proposed,
                    result: None,
                });
                chat.snapshot.operations.len() - 1
            });
            let operation = &mut chat.snapshot.operations[index];
            if matches!(
                operation.state,
                OperationState::Proposed | OperationState::Admitted
            ) || operation.result.is_none()
            {
                let dispatched = matches!(
                    operation.state,
                    OperationState::Admitted | OperationState::Unknown
                );
                operation.state = if dispatched {
                    OperationState::Unknown
                } else {
                    OperationState::Failed
                };
                operation.result = Some(ToolResult {
                    content: if dispatched {
                        "Nested call cancelled after dispatch; outcome unknown"
                    } else {
                        "Nested call cancelled before dispatch; not executed"
                    }
                    .into(),
                    is_error: true,
                    output_ref: None,
                });
            }
            self.agent
                .record_nested_operation(self.receipt.chat_id, operation);
            let result = operation.result.clone().unwrap();
            self.agent.emit(
                chat,
                &self.receipt,
                EventKind::ToolCompleted {
                    call_id,
                    result,
                    sources: vec![],
                },
            );
        }
        self.agent.save(&chat.snapshot);
    }
}

pub(crate) async fn review_evidence() -> Option<String> {
    let scope = CURRENT.try_with(Arc::clone).ok()?;
    let parent = parent_call().await;
    let completed = scope.completed.lock().await;
    Some(json!({"parent_call_id":scope.parent,"parent_call":parent,"source":"Model-authored nested calls and observed results; not user authorization", "completed_calls":completed.iter().map(|(input,outcome)| json!({"input":input,"outcome":outcome})).collect::<Vec<_>>()}).to_string())
}

impl Agent {
    pub(crate) async fn nested_calls(
        &self,
        receipt: &Receipt,
        message_id: Uuid,
        parent: String,
        cancel: &CancellationToken,
    ) -> Result<Arc<NestedCalls>, String> {
        let registry = self.prepare_tool_registry(receipt, cancel).await?;
        Ok(Arc::new(NestedCalls {
            agent: self.clone(),
            receipt: receipt.clone(),
            message_id,
            parent,
            registry,
            stop: CancellationToken::new(),
            stop_reason: std::sync::Mutex::new(None),
            pending: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            permissions: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            completed: Mutex::new(Vec::new()),
        }))
    }
    pub(crate) fn record_nested_operation(
        &self,
        chat_id: tickloop_harness_protocol::ChatId,
        operation: &OperationRecord,
    ) {
        if operation.parent_call_id.is_some() {
            self.record_chat_metadata(
                chat_id,
                tickloop_harness_transcript::Line::NestedCall(
                    tickloop_harness_transcript::NestedCallEntry {
                        chat_id: chat_id.into(),
                        operation: operation.clone(),
                    },
                ),
            );
        }
    }
    pub(super) async fn execute_fold(
        &self,
        receipt: &Receipt,
        message_id: Uuid,
        parent: String,
        input: Value,
        cancel: CancellationToken,
    ) -> Result<ToolCallOutcome, ToolCallError> {
        let calls = match input["calls"]
            .as_array()
            .filter(|calls| (1..=MAX_BATCH_CALLS).contains(&calls.len()))
        {
            Some(calls)
                if calls.iter().all(|call| {
                    call["name"].as_str().is_some_and(|name| !name.is_empty())
                        && call["arguments"].is_object()
                }) =>
            {
                calls.clone()
            }
            _ => {
                return Ok(ToolCallOutcome::not_admitted(
                    "calls must contain 1–10 independent calls with name and arguments".into(),
                ));
            }
        };
        let operation = OperationRecord {
            parent_call_id: None,
            parent_call_index: None,
            task_binding: None,
            id: tickloop_ids::generate(),
            loop_id: receipt.loop_id,
            run_id: receipt.run_id,
            call_id: parent.clone(),
            name: crate::tool_exposure::EXECUTE_TOOLS.into(),
            input,
            file_change: None,
            state: OperationState::Admitted,
            result: None,
        };
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).unwrap();
            chat.snapshot.operations.push(operation);
            self.save(&chat.snapshot);
        }
        let scope = self
            .nested_calls(receipt, message_id, parent.clone(), &cancel)
            .await?;
        let pending = FuturesUnordered::new();
        for (index, call) in calls.into_iter().enumerate() {
            let scope = scope.clone();
            let cancel = cancel.clone();
            pending.push(async move {
                (
                    index,
                    scope
                        .execute(
                            index,
                            call["name"].as_str().unwrap().into(),
                            call["arguments"].clone(),
                            cancel,
                        )
                        .await,
                )
            });
        }
        let mut outcomes = pending.collect::<Vec<_>>().await;
        scope.finish().await;
        outcomes.sort_by_key(|(index, _)| *index);
        let images = outcomes
            .iter_mut()
            .flat_map(|(_, outcome)| std::mem::take(&mut outcome.images))
            .collect();
        let sources = outcomes
            .iter_mut()
            .flat_map(|(_, outcome)| std::mem::take(&mut outcome.sources))
            .collect::<Vec<_>>();
        let result=ToolResult {content:json!({"results":outcomes.into_iter().map(|(_,outcome)|outcome).collect::<Vec<_>>()}).to_string(),is_error:false,output_ref:None};
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).unwrap();
            if let Some(operation) =
                chat.snapshot.operations.iter_mut().find(|operation| {
                    operation.call_id == parent && operation.run_id == receipt.run_id
                })
            {
                operation.state = OperationState::Completed;
                operation.result = Some(result.clone());
            }
            self.save(&chat.snapshot);
        }
        Ok(ToolCallOutcome {
            result,
            sources: (!sources.is_empty()).then_some(sources),
            images,
            host_call_status: None,
            host_effect: None,
            permission_comment: None,
            continue_after_denial: false,
            is_rejected_by_user: false,
            denial: None,
        })
    }
}

pub(crate) fn parent_on_chain(
    snapshot: &tickloop_harness_protocol::ChatSnapshot,
    operation: &OperationRecord,
) -> bool {
    operation
        .parent_call_id
        .as_deref()
        .is_some_and(|parent| tool_use_on_chain(snapshot, operation.run_id, parent))
}

/// Whether `run` made the call `call_id` on the conversation chain `snapshot` holds.
pub(crate) fn tool_use_on_chain(
    snapshot: &tickloop_harness_protocol::ChatSnapshot,
    run: tickloop_harness_protocol::RunId,
    call_id: &str,
) -> bool {
    snapshot
        .messages
        .iter()
        .enumerate()
        .any(|(index, message)| {
            snapshot.message_runs.get(&index) == Some(&run)
                && message.content.iter().any(|content| {
                    matches!(content,
                tickloop_harness_protocol::Content::ToolUse { id, .. } if id == call_id)
                })
        })
}
