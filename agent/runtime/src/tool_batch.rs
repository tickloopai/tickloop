use std::{collections::VecDeque, future::Future, panic::AssertUnwindSafe, sync::Arc};

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use tickloop_agent_protocol::{
    Content, EventKind, Message, OperationState, ProposedToolCall, Receipt, Role, ToolResultBlock,
    ToolResultContent,
};
use tokio::sync::{Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    Agent, approval_context, monitor_members,
    tool_execution::{ToolCallOutcome, ToolInvocation, ToolRegistry},
};

const MAX_CONCURRENT_TOOL_CALLS: usize = 10;

enum ToolPermit {
    Shared(OwnedRwLockReadGuard<()>),
    Exclusive(OwnedRwLockWriteGuard<()>),
}

struct ToolScope {
    gate: Arc<RwLock<()>>,
    permit: Mutex<Option<ToolPermit>>,
}

tokio::task_local! {
    static TOOL_SCOPE: Arc<ToolScope>;
}

async fn coordinate_call(
    gate: Arc<RwLock<()>>,
    cancel: CancellationToken,
    call: impl Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("run interrupted before tool preparation".into()),
        permit = gate.clone().read_owned() => permit,
    };
    TOOL_SCOPE
        .scope(
            Arc::new(ToolScope {
                gate,
                permit: Mutex::new(Some(ToolPermit::Shared(permit))),
            }),
            call,
        )
        .await
}

/// Parking a Run for a human must wait until its other tool calls have settled.
pub(super) async fn serialize_permission_prompt(cancel: &CancellationToken) -> Result<(), String> {
    let Ok(scope) = TOOL_SCOPE.try_with(Arc::clone) else {
        return Ok(());
    };
    let mut permit = scope.permit.lock().await;
    match permit.take() {
        Some(ToolPermit::Shared(shared)) => drop(shared),
        Some(ToolPermit::Exclusive(exclusive)) => {
            *permit = Some(ToolPermit::Exclusive(exclusive));
            return Ok(());
        }
        None => return Err("tool call is no longer active".into()),
    }
    let exclusive = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("permission request cancelled".into()),
        permit = scope.gate.clone().write_owned() => permit,
    };
    *permit = Some(ToolPermit::Exclusive(exclusive));
    Ok(())
}

impl Agent {
    pub(super) async fn execute_tool_batch(
        &self,
        receipt: &Receipt,
        assistant_message_id: Uuid,
        calls: Vec<ProposedToolCall>,
        registry: &ToolRegistry,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let workspace = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
            tickloop_agent_tools::Workspace {
                workdir: chat.snapshot.workdir.clone(),
                additional_directories: chat.snapshot.additional_directories.clone(),
            }
        };
        let mut concurrent = Vec::new();
        for call in calls {
            if registry.is_concurrency_safe(&call.name, &call.input, &workspace) {
                concurrent.push(call);
            } else {
                self.execute_concurrent_tools(
                    receipt,
                    assistant_message_id,
                    std::mem::take(&mut concurrent),
                    registry,
                    cancel.clone(),
                )
                .await?;
                self.execute_and_commit_tool_call(
                    receipt,
                    assistant_message_id,
                    call,
                    registry,
                    cancel.clone(),
                    &cancel,
                )
                .await?;
            }
        }
        self.execute_concurrent_tools(receipt, assistant_message_id, concurrent, registry, cancel)
            .await
    }

    async fn execute_concurrent_tools(
        &self,
        receipt: &Receipt,
        assistant_message_id: Uuid,
        calls: Vec<ProposedToolCall>,
        registry: &ToolRegistry,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let batch_cancel = cancel.child_token();
        let gate = Arc::new(RwLock::new(()));
        let mut pending = VecDeque::from(calls);
        let mut in_flight = FuturesUnordered::new();
        let mut first_error = None;
        let mut panic_payload = None;
        loop {
            while first_error.is_none()
                && panic_payload.is_none()
                && in_flight.len() < MAX_CONCURRENT_TOOL_CALLS
            {
                let Some(call) = pending.pop_front() else {
                    break;
                };
                in_flight.push(
                    AssertUnwindSafe(coordinate_call(
                        gate.clone(),
                        batch_cancel.clone(),
                        self.execute_and_commit_tool_call(
                            receipt,
                            assistant_message_id,
                            call,
                            registry,
                            batch_cancel.clone(),
                            &cancel,
                        ),
                    ))
                    .catch_unwind(),
                );
            }
            let Some(result) = in_flight.next().await else {
                break;
            };
            match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(payload) => {
                    panic_payload.get_or_insert(payload);
                }
            }
            pending.clear();
            batch_cancel.cancel();
        }
        if let Some(payload) = panic_payload {
            std::panic::resume_unwind(payload);
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn execute_and_commit_tool_call(
        &self,
        receipt: &Receipt,
        assistant_message_id: Uuid,
        call: ProposedToolCall,
        registry: &ToolRegistry,
        cancel: CancellationToken,
        run_cancel: &CancellationToken,
    ) -> Result<(), String> {
        let ProposedToolCall {
            call_id: id,
            name,
            input,
        } = call;
        let tool_use = monitor_members::ToolUse::of(&name, &input);
        let invocation =
            ToolInvocation::new(id.clone(), name.clone(), input, registry.resolve(&name));
        let outcome = self
            .execute_registered_call(receipt, assistant_message_id, invocation, cancel.clone())
            .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(record) = error.denial {
                    self.record_final_denial(record).await;
                }
                return Err(error.message);
            }
        };
        let ToolCallOutcome {
            result,
            host_call_status,
            host_effect,
            images: result_images,
            permission_comment,
            continue_after_denial,
            is_rejected_by_user,
            denial,
        } = outcome;
        if let Some(record) = denial {
            self.record_final_denial(record).await;
        }
        let mut result = result;
        if let Some(crate::host_tools::HostEffect::CreatedArtifact(tick)) = &host_effect
            && !result.is_error
            && host_call_status == Some(crate::host_tools::HostCallStatus::Completed)
            && let Err(error) = self.link_created_artifact(receipt, tick).await
        {
            result.content.push_str(&format!("\n\nThe artifact was created, but linking it for @tickloop requests failed: {error}. The user can press f on it in Monitor to link it."));
        }
        let result = self
            .persist_large_tool_result(receipt, &name, &id, result)
            .await
            .map_err(|error| error.to_string())?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id).unwrap();
        let model_result = if !continue_after_denial {
            permission_comment.as_deref().map_or_else(
                || result.content.clone(),
                |comment| {
                    format!(
                        "{}\n\nThe user approved this action with this comment: {comment}",
                        result.content
                    )
                },
            )
        } else {
            result.content.clone()
        };
        let content = if result_images.is_empty() {
            model_result.into()
        } else {
            ToolResultContent::Blocks(
                std::iter::once(ToolResultBlock::Text { text: model_result })
                    .chain(result_images.into_iter().map(ToolResultBlock::from))
                    .collect(),
            )
        };
        let block = Content::ToolResult {
            tool_use_id: id.clone(),
            content,
            is_error: result.is_error,
        };
        let last_index = chat.snapshot.messages.len().checked_sub(1);
        let same_run = !chat.start_new_tool_result_message
            && last_index.is_some_and(|index| {
                chat.snapshot.message_runs.get(&index) == Some(&receipt.run_id)
            });
        if let Some(message) = chat.snapshot.messages.last_mut().filter(|message| {
            same_run
                && message.role == Role::User
                && message
                    .content
                    .iter()
                    .all(|content| matches!(content, Content::ToolResult { .. }))
        }) {
            message.content.push(block);
        } else {
            chat.snapshot
                .message_runs
                .insert(chat.snapshot.messages.len(), receipt.run_id);
            chat.snapshot.messages.push(Message {
                role: Role::User,
                content: vec![block],
            });
        }
        chat.start_new_tool_result_message = false;
        if let Some(operation) = chat
            .snapshot
            .operations
            .iter_mut()
            .find(|operation| operation.run_id == receipt.run_id && operation.call_id == id)
        {
            operation.result = Some(result.clone());
        }
        if !result.is_error
            && let Some(tool_use) = tool_use
        {
            self.mark_tool_use(chat, tool_use);
        }
        if !result.is_error
            && host_call_status == Some(crate::host_tools::HostCallStatus::Completed)
        {
            match &host_effect {
                Some(crate::host_tools::HostEffect::CreatedArtifact(tick)) => {
                    if let Some(instance) = self.auto_attach_artifact_tick(chat, tick.clone()) {
                        self.mark_joined(chat, [instance]);
                    }
                }
                Some(crate::host_tools::HostEffect::ReadArtifact(tick)) => {
                    if let Some(instance) = self.auto_attach_artifact_tick(chat, tick.clone()) {
                        self.mark_taken_in(chat, [instance]);
                    }
                }
                Some(crate::host_tools::HostEffect::RenamedArtifact { instance, title }) => {
                    self.retitle_artifact_tick(chat, instance, title)
                }
                Some(crate::host_tools::HostEffect::OpenedTick {
                    instance,
                    title,
                    is_terminal,
                }) => {
                    if let Some(operation) = chat.snapshot.operations.iter().find(|operation| {
                        operation.run_id == receipt.run_id
                            && operation.call_id == id
                            && operation.state == OperationState::Completed
                    }) {
                        chat.bookkeeping.tick_creation_receipts.push(
                            approval_context::TickCreationReceipt {
                                loop_id: receipt.loop_id,
                                operation_id: operation.id,
                                instance: instance.clone(),
                            },
                        );
                    }
                    self.record_opened_tick(chat, instance.clone(), title.clone(), *is_terminal);
                }
                None => {}
            }
        }
        self.save(&chat.snapshot);
        self.emit(
            chat,
            receipt,
            EventKind::ToolCompleted {
                call_id: id.clone(),
                result: result.clone(),
            },
        );
        // Tool errors go back to the model; a bare user denial or stopped foreground command ends the run.
        if is_rejected_by_user && !continue_after_denial {
            chat.user_cancelled_runs.insert(receipt.run_id);
        }
        if chat.user_cancelled_runs.contains(&receipt.run_id) {
            run_cancel.cancel();
            return Err(result.content);
        }
        Ok(())
    }
}
