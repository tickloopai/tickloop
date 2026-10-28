//! A Run's tool batch: the calls of one assistant message run in order, except that consecutive
//! concurrency-safe calls run together, up to [`MAX_CONCURRENT_TOOL_CALLS`] at once. Writes and
//! other shared-state calls are ordered barriers between such groups. Each result joins the
//! conversation as soon as its call ends, and the end of the batch prepares the next model
//! request. A Run can re-enter its batch at a given call, which is how a Run waiting on a call
//! continues where the wait left it.

use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, atomic::Ordering},
};

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};

use tickloop_harness_model::ModelRequest;
use tickloop_harness_protocol::{
    ChatSnapshot, Content, EventKind, Message, OperationState, ProposedToolCall, Receipt, Role,
    RunId, SubagentReport, ToolResultBlock, ToolResultContent, Usage,
};
use tickloop_harness_transcript::{ModelVisibleResult, ResumePoint};
use tokio::sync::{Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    Agent, Chat, MessageBoundary, ToolCallOutcome, ToolInvocation, ToolRegistry, approval_context,
    host_tools::{HostCallStatus, HostEffect},
    monitor_members, shared_tasks,
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

/// What an ended call leaves for the conversation and the Chat.
struct CallCompletion {
    result: ModelVisibleResult,
    sources: Option<Vec<tickloop_harness_protocol::CitationSource>>,
    host_effect: Option<HostEffect>,
    tool_use: Option<monitor_members::ToolUse>,
}

/// How a call's result joins the conversation.
#[derive(Clone, Copy)]
struct CommitMode {
    /// The result came from the call's wait, not from running it here.
    is_resumed: bool,
    /// The batch is entered again, so the chain may already hold the result.
    is_reentered: bool,
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

    /// Refreshes the host tools while telling the Chat which MCP servers the request waits for.
    async fn refresh_showing_awaited_servers(
        &self,
        receipt: &Receipt,
        provider: &dyn crate::host_tools::HostToolProvider,
        cancel: &CancellationToken,
    ) {
        let mut awaited = provider.awaited_mcp_servers();
        if let Some(awaited) = &mut awaited {
            awaited.mark_changed();
        }
        let refresh = provider.refresh(cancel.clone());
        tokio::pin!(refresh);
        let mut shown = false;
        loop {
            let changed = async {
                match &mut awaited {
                    Some(awaited) => awaited
                        .changed()
                        .await
                        .map(|()| awaited.borrow_and_update().clone()),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = &mut refresh => break,
                servers = changed => match servers {
                    Ok(servers) if servers.is_empty() && !shown => {}
                    Ok(servers) => {
                        shown = !servers.is_empty();
                        self.show_awaited_mcp_servers(receipt, servers).await;
                    }
                    Err(_) => awaited = None,
                },
            }
        }
        if shown {
            self.show_awaited_mcp_servers(receipt, Vec::new()).await;
        }
    }

    async fn show_awaited_mcp_servers(&self, receipt: &Receipt, servers: Vec<String>) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats
            .get_mut(&receipt.chat_id)
            .filter(|chat| chat.active_run == Some(receipt.run_id))
        else {
            return;
        };
        self.publish_event(
            chat,
            receipt,
            tickloop_harness_protocol::EventKind::WaitingForMcpServers { servers },
        );
    }

    /// The tools a model request of the Run offers, after refreshing what they depend on.
    #[tracing::instrument(target = "tickloop::request_timing", skip_all, fields(chat = ?receipt.chat_id, run = ?receipt.run_id))]
    pub(super) async fn prepare_tool_registry(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<ToolRegistry, String> {
        let host_provider = self.inner.host_tools.read().unwrap().clone();
        if let Some(provider) = host_provider {
            let _timing = crate::timing::Phase::new("refresh");
            Box::pin(self.refresh_showing_awaited_servers(receipt, provider.as_ref(), cancel))
                .await;
        }
        self.refresh_task_tools_setting()
            .await
            .map_err(|error| error.to_string())?;
        self.ensure_handback_requirement(receipt).await?;
        self.pin_delegated_artifact_account(receipt.chat_id).await?;
        self.rediscover_skills(receipt.chat_id).await;
        self.synchronize_remote_skills(receipt).await;
        let mut chats = self.inner.chats.lock().await;
        let task_access = shared_tasks::tool_access(&chats, receipt);
        let chat = chats.get_mut(&receipt.chat_id).unwrap();
        let _timing = crate::timing::Phase::new("ToolRegistry::for_chat");
        let mut registry = ToolRegistry::for_chat(
            chat,
            self.inner.host_tools.read().unwrap().clone(),
            self.inner.tick_stream_installed.load(Ordering::SeqCst),
            self.inner.host_options.prompt_policy,
            task_access,
            &self.inner.tool_policy,
            self.scheduled_task_terms(),
        );
        registry.hold_host_tools(&mut chat.cache_prefix, receipt.run_id);
        Ok(registry)
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

    /// Runs the batch's calls from `start`, appending each result as its call ends, then ends
    /// the batch. `first_result`, when given, is call `start`'s result: it is appended instead
    /// of running the call. A safe point comes before each call that runs alone and before each
    /// group of concurrency-safe calls, so a Run never stops there with a call in flight.
    pub(super) async fn run_batch_from(
        &self,
        receipt: &Receipt,
        registry: &ToolRegistry,
        mut batch: ToolBatch,
        start: usize,
        first_result: Option<ModelVisibleResult>,
        cancel: &CancellationToken,
    ) -> Result<BatchExit, String> {
        let mut calls = std::mem::take(&mut batch.calls).into_iter().skip(start);
        if let Some(result) = first_result
            && let Some(call) = calls.next()
        {
            let tool_use = monitor_members::ToolUse::of(&call.name, &call.input);
            self.commit_call_result(
                receipt,
                &call.call_id,
                CallCompletion {
                    result,
                    sources: None,
                    host_effect: None,
                    tool_use,
                },
                CommitMode {
                    is_resumed: true,
                    is_reentered: batch.is_reentered,
                },
                cancel,
            )
            .await?;
        }
        let workspace = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
            super::configured_workspace_for_chat(chat)
        };
        let mut concurrent = Vec::new();
        for call in calls {
            if batch.is_reentered
                && self
                    .inner
                    .chats
                    .lock()
                    .await
                    .get(&receipt.chat_id)
                    .is_some_and(|chat| {
                        has_result_on_chain(&chat.snapshot, receipt.run_id, &call.call_id)
                    })
            {
                continue;
            }
            if registry.is_concurrency_safe(
                &call.name,
                &call.input,
                &workspace,
                self.inner.waits,
                self.inner.tool_policy.subagent_launch,
            ) {
                concurrent.push(call);
                continue;
            }
            self.execute_concurrent_tools(
                receipt,
                &batch,
                std::mem::take(&mut concurrent),
                registry,
                cancel,
            )
            .await?;
            Box::pin(self.stop_at_safe_point(receipt, Some(&call.call_id))).await?;
            Box::pin(self.execute_and_commit_tool_call(
                receipt,
                &batch,
                call,
                registry,
                cancel.clone(),
                cancel,
            ))
            .await?;
        }
        self.execute_concurrent_tools(receipt, &batch, concurrent, registry, cancel)
            .await?;
        self.finish_batch(receipt).await
    }

    /// Runs a group of concurrency-safe calls together. The first error or panic cancels the
    /// calls not yet started; calls already started settle so their state is recorded.
    async fn execute_concurrent_tools(
        &self,
        receipt: &Receipt,
        batch: &ToolBatch,
        calls: Vec<ProposedToolCall>,
        registry: &ToolRegistry,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let Some(first) = calls.first() else {
            return Ok(());
        };
        Box::pin(self.stop_at_safe_point(receipt, Some(&first.call_id))).await?;
        let group_cancel = cancel.child_token();
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
                        group_cancel.clone(),
                        Box::pin(self.execute_and_commit_tool_call(
                            receipt,
                            batch,
                            call,
                            registry,
                            group_cancel.clone(),
                            cancel,
                        )),
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
            group_cancel.cancel();
        }
        if let Some(payload) = panic_payload {
            std::panic::resume_unwind(payload);
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Runs one call under `call_cancel` and appends its result; a result that ends the Run
    /// cancels `run_cancel`.
    async fn execute_and_commit_tool_call(
        &self,
        receipt: &Receipt,
        batch: &ToolBatch,
        call: ProposedToolCall,
        registry: &ToolRegistry,
        call_cancel: CancellationToken,
        run_cancel: &CancellationToken,
    ) -> Result<(), String> {
        let ProposedToolCall {
            call_id,
            name,
            input,
        } = call;
        let tool_use = monitor_members::ToolUse::of(&name, &input);
        let invocation = ToolInvocation::new(
            call_id.clone(),
            name.clone(),
            input,
            registry.resolve(&name),
        );
        let outcome = self
            .execute_registered_call(receipt, batch.message_id, invocation, call_cancel)
            .await;
        let mut completion = self
            .complete_call(receipt, &call_id, &name, outcome)
            .await?;
        completion.tool_use = tool_use;
        self.commit_call_result(
            receipt,
            &call_id,
            completion,
            CommitMode {
                is_resumed: false,
                is_reentered: batch.is_reentered,
            },
            run_cancel,
        )
        .await
    }

    /// Appends a call's result to the conversation and applies its host effect. A Run released
    /// while it waited leaves without its result: from the release on it writes nothing.
    async fn commit_call_result(
        &self,
        receipt: &Receipt,
        call_id: &str,
        completion: CallCompletion,
        mode: CommitMode,
        run_cancel: &CancellationToken,
    ) -> Result<(), String> {
        let CallCompletion {
            result,
            sources,
            host_effect,
            tool_use,
        } = completion;
        let mut chats = self.inner.chats.lock().await;
        let mut candidate = sources.as_ref().map(|_| chats[&receipt.chat_id].clone());
        let chat = match candidate.as_mut() {
            Some(candidate) => candidate,
            None => chats.get_mut(&receipt.chat_id).unwrap(),
        };
        if Agent::release_of(chat, receipt.run_id).is_cancelled() {
            return Err(super::suspension::RUN_RELEASED.into());
        }
        let is_appended =
            append_call_result(chat, receipt.run_id, call_id, &result, mode.is_reentered);
        if (mode.is_resumed || sources.is_some())
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
        self.apply_completed_host_effect(chat, receipt, call_id, host_effect);
        if is_appended && let Some(sources) = &sources {
            let index = chat.snapshot.messages.len() - 1;
            let message_id = *chat
                .snapshot
                .message_ids
                .entry(index)
                .or_insert_with(tickloop_ids::generate);
            let table = &mut chat.bookkeeping.citation_sources;
            table.chat_id = receipt.chat_id.into();
            table.calls.push(tickloop_harness_transcript::CitationCall {
                call_id: call_id.into(),
                run_id: receipt.run_id,
                message_id,
                numbers: sources.iter().map(|source| source.number).collect(),
                aliases: Vec::new(),
            });
            if let Some(recorder) = &self.inner.recorder {
                recorder.record_citation_sources(table.clone());
            }
        }
        self.save(&chat.snapshot);
        if let Some(candidate) = candidate {
            if is_appended && let Some(recorder) = &self.inner.recorder {
                let current = &chats[&receipt.chat_id];
                if let Err(failure) = recorder.commit_citation_call(current, &candidate).await {
                    let error = match failure {
                        crate::persistence::CitationCommitError::Association(error) => error,
                        crate::persistence::CitationCommitError::ResultNode(error) => {
                            if matches!(error, crate::transcript::TranscriptError::Storage(_)) {
                                // The outbox retries this exact result. Keep its execution fact
                                // in memory too, after the association has become durable.
                                *chats.get_mut(&receipt.chat_id).unwrap() = candidate;
                            }
                            error
                        }
                    };
                    return Err(error.to_string());
                }
            }
            *chats.get_mut(&receipt.chat_id).unwrap() = candidate;
        }
        let chat = chats.get_mut(&receipt.chat_id).unwrap();
        if Agent::release_of(chat, receipt.run_id).is_cancelled() {
            return Err(super::suspension::RUN_RELEASED.into());
        }
        if is_appended {
            self.emit(
                chat,
                receipt,
                EventKind::ToolCompleted {
                    sources: sources.unwrap_or_default(),
                    call_id: call_id.into(),
                    result: result.tool.clone(),
                },
            );
        }
        // Tool errors go back to the model; a bare user denial or stopped foreground command ends the run.
        if result.ends_run {
            chat.user_cancelled_runs.insert(receipt.run_id);
        }
        if chat.user_cancelled_runs.contains(&receipt.run_id) {
            run_cancel.cancel();
            return Err(result.tool.content);
        }
        Ok(())
    }

    pub(super) async fn complete_nested_call(
        &self,
        receipt: &Receipt,
        call_id: &str,
        name: &str,
        input: &serde_json::Value,
        outcome: ToolCallOutcome,
    ) -> Result<tickloop_harness_protocol::ToolResult, String> {
        let completion = self
            .complete_call(receipt, call_id, name, Ok(outcome))
            .await?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or("chat is unavailable")?;
        if !completion.result.tool.is_error
            && let Some(tool_use) = monitor_members::ToolUse::of(name, input)
        {
            self.mark_tool_use(chat, tool_use);
        }
        self.apply_completed_host_effect(chat, receipt, call_id, completion.host_effect);
        self.save(&chat.snapshot);
        let mut result = completion.result.tool;
        result.content = completion.result.content.text().into_owned();
        Ok(result)
    }

    fn apply_completed_host_effect(
        &self,
        chat: &mut Chat,
        receipt: &Receipt,
        call_id: &str,
        host_effect: Option<HostEffect>,
    ) {
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
                min_rows,
                max_rows,
            }) => {
                if let Some(operation) = chat.snapshot.operations.iter().find(|operation| {
                    operation.run_id == receipt.run_id
                        && operation.call_id == call_id
                        && operation.state == OperationState::Completed
                }) {
                    chat.bookkeeping.tick_creation_receipts.push(
                        approval_context::TickCreationReceipt {
                            loop_id: receipt.loop_id.into(),
                            operation_id: operation.id,
                            instance: instance.clone(),
                        },
                    );
                }
                self.record_opened_tick(chat, instance, title, is_terminal, min_rows, max_rows);
            }
            None => {}
        }
    }

    /// Turns an executed call's outcome into the result the conversation records, and the host
    /// effect its result has on the Chat.
    async fn complete_call(
        &self,
        receipt: &Receipt,
        call_id: &str,
        name: &str,
        outcome: Result<ToolCallOutcome, super::tool_execution::ToolCallError>,
    ) -> Result<CallCompletion, String> {
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(record) = error.denial {
                    self.record_final_denial(*record).await;
                }
                return Err(error.message);
            }
        };
        let ToolCallOutcome {
            mut result,
            sources,
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
        Ok(CallCompletion {
            result: ModelVisibleResult {
                tool: result,
                content,
                ends_run: is_rejected_by_user && !continue_after_denial,
            },
            sources,
            host_effect,
            tool_use: None,
        })
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
                .message_ids
                .insert(snapshot.messages.len(), tickloop_ids::generate());
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
