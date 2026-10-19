//! A Run's tool batch: the calls of one assistant message run in order, each result joins the
//! conversation as soon as its call ends, and the end of the batch prepares the next model
//! request. A Run can re-enter its batch at a given call, which is how a Run waiting on a call
//! continues where the wait left it.

use std::{collections::HashSet, sync::atomic::Ordering};

use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{
    ChatSnapshot, Content, EventKind, Message, OperationState, ProposedToolCall, Receipt, Role,
    RunId, SubagentReport, ToolResultBlock, ToolResultContent, Usage,
};
use tickloop_agent_transcript::{ModelVisibleResult, ResumePoint};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    Agent, Chat, MessageBoundary, ToolCallOutcome, ToolInvocation, ToolRegistry, approval_context,
    host_tools::{HostCallStatus, HostEffect},
    monitor_members, shared_tasks,
};

/// How a Run starts executing.
pub(crate) enum RunStart {
    /// With a model request, as a new Run or a continuation does.
    Request(ModelRequest),
    /// Inside the conversation it already has, at `at`. A given `result` is appended as the
    /// result of the call `at` names; without one that call runs.
    Resume {
        at: ResumePoint,
        result: Option<ModelVisibleResult>,
    },
}

impl From<ModelRequest> for RunStart {
    fn from(request: ModelRequest) -> Self {
        Self::Request(request)
    }
}

/// The calls of one assistant message, which a Run makes in order.
pub(crate) struct ToolBatch {
    /// The assistant message's current id, which file changes made by its calls belong to.
    pub message_id: Uuid,
    pub calls: Vec<ProposedToolCall>,
    /// The batch is entered again: a call whose result the chain already holds is done.
    pub is_reentered: bool,
}

/// How a tool batch ends.
pub(crate) enum BatchExit {
    /// The Run continues with this model request.
    Continue(ModelRequest),
    /// The Run ended at the end of the batch, as a Subagent whose report was delivered does.
    Ended(Usage),
}

impl Agent {
    /// The ids of the calls the Run already has on the chain. A model reply reusing one is
    /// invalid, so a continued or re-entered Run cannot repeat a call within the Run.
    pub(super) async fn calls_on_chain(&self, receipt: &Receipt) -> HashSet<String> {
        let chats = self.inner.chats.lock().await;
        chats
            .get(&receipt.chat_id)
            .map(|chat| run_call_ids(&chat.snapshot, receipt.run_id))
            .unwrap_or_default()
    }

    /// The tools a model request of the Run offers, after refreshing what they depend on.
    pub(super) async fn prepare_tool_registry(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<ToolRegistry, String> {
        let host_provider = self.inner.host_tools.read().unwrap().clone();
        if let Some(provider) = host_provider {
            let _ = provider.refresh(cancel.clone()).await;
        }
        self.refresh_task_tools_setting()
            .await
            .map_err(|error| error.to_string())?;
        self.ensure_handback_requirement(receipt).await?;
        self.pin_delegated_artifact_account(receipt.chat_id).await?;
        self.rediscover_skills(receipt.chat_id).await;
        self.synchronize_remote_skills(receipt).await;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).unwrap();
        Ok(ToolRegistry::for_chat(
            chat,
            self.inner.host_tools.read().unwrap().clone(),
            self.inner.tick_stream_installed.load(Ordering::SeqCst),
            self.inner.host_options.prompt_policy,
            shared_tasks::tool_access(&chats, receipt),
            &self.inner.tool_policy,
        ))
    }

    /// Re-enters the Run's tool batch at `at` with freshly prepared tools.
    pub(super) async fn reenter_batch(
        &self,
        receipt: &Receipt,
        at: ResumePoint,
        result: Option<ModelVisibleResult>,
        cancel: &CancellationToken,
    ) -> Result<BatchExit, String> {
        let (assistant_message, call_id) = match at {
            ResumePoint::InBatch {
                assistant_message,
                call_id,
            } => (assistant_message, call_id),
            // As after a wait at the end of a turn: what was queued to the Loop meanwhile joins
            // the conversation before the next model request.
            ResumePoint::BeforeModelRequest => {
                self.merge_queued_messages(receipt)
                    .await
                    .map_err(|error| error.to_string())?;
                let chats = self.inner.chats.lock().await;
                let chat = chats
                    .get(&receipt.chat_id)
                    .ok_or_else(|| "chat is unavailable".to_string())?;
                return self.continuation_request(chat).map(BatchExit::Continue);
            }
        };
        let (batch, start) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats
                .get(&receipt.chat_id)
                .ok_or_else(|| "chat is unavailable".to_string())?;
            locate_batch_call(&chat.snapshot, receipt.run_id, assistant_message, &call_id)?
        };
        let registry = self.prepare_tool_registry(receipt, cancel).await?;
        self.run_batch_from(receipt, &registry, batch, start, result, cancel)
            .await
    }

    /// Runs the batch's calls from `start` in order, appending each result as its call ends,
    /// then ends the batch. `first_result`, when given, is call `start`'s result: it is appended
    /// instead of running the call.
    pub(super) async fn run_batch_from(
        &self,
        receipt: &Receipt,
        registry: &ToolRegistry,
        batch: ToolBatch,
        start: usize,
        mut first_result: Option<ModelVisibleResult>,
        cancel: &CancellationToken,
    ) -> Result<BatchExit, String> {
        for ProposedToolCall {
            call_id,
            name,
            input,
        } in batch.calls.into_iter().skip(start)
        {
            let tool_use = monitor_members::ToolUse::of(&name, &input);
            let (result, host_effect, is_resumed) = match first_result.take() {
                Some(result) => (result, None, true),
                None if batch.is_reentered
                    && self
                        .inner
                        .chats
                        .lock()
                        .await
                        .get(&receipt.chat_id)
                        .is_some_and(|chat| {
                            has_result_on_chain(&chat.snapshot, receipt.run_id, &call_id)
                        }) =>
                {
                    continue;
                }
                None => {
                    self.stop_at_safe_point(receipt, Some(&call_id)).await?;
                    let invocation = ToolInvocation::new(
                        call_id.clone(),
                        name.clone(),
                        input,
                        registry.resolve(&name),
                    );
                    let outcome = self
                        .execute_registered_call(
                            receipt,
                            batch.message_id,
                            invocation,
                            cancel.clone(),
                        )
                        .await;
                    let (result, host_effect) = self
                        .complete_call(receipt, &call_id, &name, outcome)
                        .await?;
                    (result, host_effect, false)
                }
            };
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).unwrap();
            // A Run released while it waited leaves without its result: from the release on it
            // writes nothing.
            if Agent::release_of(chat, receipt.run_id).is_cancelled() {
                return Err(super::suspension::RUN_RELEASED.into());
            }
            let is_appended =
                append_call_result(chat, receipt.run_id, &call_id, &result, batch.is_reentered);
            if is_resumed
                && is_appended
                && let Some(operation) = chat.snapshot.operations.iter_mut().find(|operation| {
                    operation.run_id == receipt.run_id && operation.call_id == call_id
                })
                && operation.state == OperationState::Admitted
            {
                operation.state = if result.tool.is_error {
                    OperationState::Failed
                } else {
                    OperationState::Completed
                };
            }
            if is_appended
                && !result.tool.is_error
                && let Some(tool_use) = tool_use
            {
                self.mark_tool_use(chat, tool_use);
            }
            match host_effect {
                Some(HostEffect::CreatedArtifact(tick)) => {
                    if let Some(instance) = self.auto_attach_artifact_tick(chat, tick) {
                        self.mark_joined(chat, [instance]);
                    }
                }
                Some(HostEffect::ReadArtifact(tick)) => {
                    if let Some(instance) = self.auto_attach_artifact_tick(chat, tick) {
                        self.mark_taken_in(chat, [instance]);
                    }
                }
                Some(HostEffect::RenamedArtifact { instance, title }) => {
                    self.retitle_artifact_tick(chat, &instance, &title)
                }
                Some(HostEffect::OpenedTick {
                    instance,
                    title,
                    is_terminal,
                }) => {
                    if let Some(operation) = chat.snapshot.operations.iter().find(|operation| {
                        operation.run_id == receipt.run_id
                            && operation.call_id == call_id
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
                    self.record_opened_tick(chat, instance, title, is_terminal);
                }
                None => {}
            }
            self.save(&chat.snapshot);
            if is_appended {
                self.emit(
                    chat,
                    receipt,
                    EventKind::ToolCompleted {
                        call_id: call_id.clone(),
                        result: result.tool.clone(),
                    },
                );
            }
            // Tool errors go back to the model; a bare user denial or stopped foreground command ends the run.
            if result.ends_run {
                chat.user_cancelled_runs.insert(receipt.run_id);
            }
            if chat.user_cancelled_runs.contains(&receipt.run_id) {
                cancel.cancel();
                return Err(result.tool.content);
            }
        }
        self.finish_batch(receipt).await
    }

    /// Turns an executed call's outcome into the result the conversation records, and the host
    /// effect its result has on the Chat.
    async fn complete_call(
        &self,
        receipt: &Receipt,
        call_id: &str,
        name: &str,
        outcome: Result<ToolCallOutcome, super::tool_execution::ToolCallError>,
    ) -> Result<(ModelVisibleResult, Option<HostEffect>), String> {
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
            mut result,
            host_call_status,
            host_effect,
            images,
            permission_comment,
            continue_after_denial,
            is_rejected_by_user,
            denial,
        } = outcome;
        if let Some(record) = denial {
            self.record_final_denial(record).await;
        }
        if let Some(HostEffect::CreatedArtifact(tick)) = &host_effect
            && !result.is_error
            && host_call_status == Some(HostCallStatus::Completed)
            && let Err(error) = self.link_created_artifact(receipt, tick).await
        {
            result.content.push_str(&format!("\n\nThe artifact was created, but linking it for @tickloop requests failed: {error}. The user can press f on it in Monitor to link it."));
        }
        let result = self
            .persist_large_tool_result(receipt, name, call_id, result)
            .await
            .map_err(|error| error.to_string())?;
        let model_text = match permission_comment {
            Some(comment) if !continue_after_denial => format!(
                "{}\n\nThe user approved this action with this comment: {comment}",
                result.content
            ),
            _ => result.content.clone(),
        };
        let content = if images.is_empty() {
            model_text.into()
        } else {
            ToolResultContent::Blocks(
                std::iter::once(ToolResultBlock::Text { text: model_text })
                    .chain(images.into_iter().map(ToolResultBlock::from))
                    .collect(),
            )
        };
        let host_effect = host_effect
            .filter(|_| !result.is_error && host_call_status == Some(HostCallStatus::Completed));
        Ok((
            ModelVisibleResult {
                tool: result,
                content,
                ends_run: is_rejected_by_user && !continue_after_denial,
            },
            host_effect,
        ))
    }

    /// The end of a batch: deferred local messages and messages queued to the Run's Loop join
    /// the conversation, and the next model request is prepared, unless the Run's Subagent
    /// report was delivered.
    async fn finish_batch(&self, receipt: &Receipt) -> Result<BatchExit, String> {
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).unwrap();
            self.flush_deferred_messages(chat, MessageBoundary::ToolResults)
                .map_err(|error| error.to_string())?;
            if matches!(
                chat.snapshot.subagent_reports.get(&receipt.run_id),
                Some(SubagentReport::Delivered { .. })
            ) {
                return chat
                    .snapshot
                    .request_usage
                    .run_conversation_usage(receipt.run_id)
                    .map(BatchExit::Ended)
                    .map_err(|error| error.to_string());
            }
        }
        self.merge_queued_messages(receipt)
            .await
            .map_err(|error| error.to_string())?;
        self.complete_file_history_snapshots(receipt.chat_id).await;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).unwrap();
        self.continuation_request(chat).map(BatchExit::Continue)
    }
}

/// Appends a call's result to the conversation: into the Run's trailing message of tool results,
/// or as a new message when the conversation moved on since, and onto the call's operation. A
/// wait on the call is done and its answer goes on, in the same change. A result the chain
/// already holds is not appended again: `false` then.
fn append_call_result(
    chat: &mut Chat,
    run_id: RunId,
    call_id: &str,
    result: &ModelVisibleResult,
    is_reentered: bool,
) -> bool {
    // Only a Run that suspended has waits.
    if chat.releases.contains_key(&run_id) {
        chat.bookkeeping.lifecycle.resume_after(run_id, call_id);
    }
    if is_reentered && has_result_on_chain(&chat.snapshot, run_id, call_id) {
        return false;
    }
    let block = Content::ToolResult {
        tool_use_id: call_id.into(),
        content: result.content.clone(),
        is_error: result.tool.is_error,
    };
    let snapshot = &mut chat.snapshot;
    let last_index = snapshot.messages.len().checked_sub(1);
    let same_run = !chat.start_new_tool_result_message
        && last_index.is_some_and(|index| snapshot.message_runs.get(&index) == Some(&run_id));
    match snapshot.messages.last_mut().filter(|message| {
        same_run
            && message.role == Role::User
            && message
                .content
                .iter()
                .all(|content| matches!(content, Content::ToolResult { .. }))
    }) {
        Some(message) => message.content.push(block),
        None => {
            snapshot
                .message_runs
                .insert(snapshot.messages.len(), run_id);
            snapshot.messages.push(Message {
                role: Role::User,
                content: vec![block],
            });
        }
    }
    chat.start_new_tool_result_message = false;
    if let Some(operation) = snapshot
        .operations
        .iter_mut()
        .find(|operation| operation.run_id == run_id && operation.call_id == call_id)
    {
        operation.result = Some(result.tool.clone());
    }
    true
}

fn has_result_on_chain(snapshot: &ChatSnapshot, run_id: RunId, call_id: &str) -> bool {
    snapshot
        .messages
        .iter()
        .enumerate()
        .filter(|(index, _)| snapshot.message_runs.get(index) == Some(&run_id))
        .flat_map(|(_, message)| &message.content)
        .any(|content| {
            matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == call_id)
        })
}

/// The calls an assistant message makes, in order.
pub(crate) fn batch_calls(content: &[Content]) -> Vec<ProposedToolCall> {
    content
        .iter()
        .filter_map(|content| match content {
            Content::ToolUse { id, name, input } => Some(ProposedToolCall {
                call_id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn run_call_ids(snapshot: &ChatSnapshot, run_id: RunId) -> HashSet<String> {
    snapshot
        .messages
        .iter()
        .enumerate()
        .filter(|(index, _)| snapshot.message_runs.get(index) == Some(&run_id))
        .flat_map(|(_, message)| &message.content)
        .filter_map(|content| match content {
            Content::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// The batch of the Run's assistant message with canonical id `assistant_message` on the current
/// chain, and the position of `call_id` in it. Compaction may give kept
/// messages new ids; the canonical id is the earliest source id, which it keeps.
pub(crate) fn locate_batch_call(
    snapshot: &ChatSnapshot,
    run_id: RunId,
    assistant_message: Uuid,
    call_id: &str,
) -> Result<(ToolBatch, usize), String> {
    let index = (0..snapshot.messages.len())
        .rev()
        .find(|index| {
            snapshot
                .original_message_ids
                .get(index)
                .or_else(|| snapshot.message_ids.get(index))
                == Some(&assistant_message)
        })
        .filter(|index| {
            snapshot.message_runs.get(index) == Some(&run_id)
                && snapshot.messages[*index].role == Role::Assistant
        })
        .ok_or_else(|| "the Run's batch is not on the conversation".to_string())?;
    let calls = batch_calls(&snapshot.messages[index].content);
    let start = calls
        .iter()
        .position(|call| call.call_id == call_id)
        .ok_or_else(|| "the Run's batch has no such call".to_string())?;
    let message_id = snapshot
        .message_ids
        .get(&index)
        .copied()
        .unwrap_or(assistant_message);
    Ok((
        ToolBatch {
            message_id,
            calls,
            is_reentered: true,
        },
        start,
    ))
}

#[cfg(test)]
#[path = "tool_batch_tests.rs"]
mod tests;
