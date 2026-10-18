use serde_json::{Value, json};

use super::*;
use tickloop_agent_protocol::local_messages;
use tickloop_agent_protocol::{
    AgentMessageSource, ChatSnapshot, EventKind, FamilyCompletion, FamilyMessageRejection,
    FamilyMessageRejectionCode, LoopId, LoopState, MessageCause, PendingInput, Receipt,
    SEND_AGENT_MESSAGE_TOOL_NAME, SubagentRegistration, ToolDefinition, ToolResult,
    VerifiedUserRelay, chat_display_title,
};

pub const LIST_SUBAGENTS_TOOL_NAME: &str = "ListSubagents";
pub const STOP_SUBAGENT_TOOL_NAME: &str = "StopSubagent";

/// Told after StopSubagent stopped a direct child, so what the child owns outside this runtime (its
/// hosted Ticks) ends with it. Embedded runtimes have none.
#[async_trait::async_trait]
pub trait StoppedChildHook: Send + Sync {
    async fn child_stopped(&self, root: ChatId, child: ChatId) -> Result<(), String>;
}

pub(super) struct PreparedFamilyMessage {
    pub policy_input: Value,
    target: ChatId,
    loop_id: LoopId,
    target_generation: u64,
    relationship: &'static str,
    relay: Option<VerifiedUserRelay>,
    cause: MessageCause,
}

pub(super) struct PreparedFamilyStop {
    policy_input: Value,
    target: ChatId,
    generations: Vec<(ChatId, u64)>,
}

pub(super) enum PreparedFamilyCall {
    Message(PreparedFamilyMessage),
    Stop(PreparedFamilyStop),
}
impl PreparedFamilyCall {
    pub(super) fn policy_input(&self) -> &Value {
        match self {
            Self::Message(message) => &message.policy_input,
            Self::Stop(stop) => &stop.policy_input,
        }
    }
}

pub(super) fn family_label(
    snapshot: &ChatSnapshot,
    registration: Option<&SubagentRegistration>,
) -> String {
    if let Some(registration) = registration {
        return registration.handle.clone();
    }
    chat_display_title(snapshot, &snapshot.user_shells)
}

enum FamilyDelivery<'a> {
    Message {
        prepared: &'a PreparedFamilyMessage,
        body: &'a str,
    },
    Handback {
        prepared: &'a PreparedFamilyMessage,
        body: &'a str,
    },
    Completion {
        parent: &'a Receipt,
        body: &'a str,
    },
}

pub(super) struct SentMessage {
    target: ChatId,
    message: String,
    relay: Option<VerifiedUserRelay>,
    result: ToolResult,
}

const ENDED_TARGET_REFUSAL: &str =
    "message not dispatched: interrupted or ended work requires explicit user continuation";

/// Why a family delivery was not dispatched.
pub(super) enum DispatchRefusal {
    /// The receiving root run is being cancelled, or its Loop failed or was interrupted: a
    /// completion notification stays pending until that run ends or the Loop is explicitly
    /// continued, rather than starting the Loop.
    Deferred,
    Refused(String),
}

impl From<String> for DispatchRefusal {
    fn from(message: String) -> Self {
        Self::Refused(message)
    }
}

impl From<&str> for DispatchRefusal {
    fn from(message: &str) -> Self {
        Self::Refused(message.into())
    }
}

impl From<DispatchRefusal> for String {
    fn from(refusal: DispatchRefusal) -> Self {
        match refusal {
            DispatchRefusal::Deferred => ENDED_TARGET_REFUSAL.into(),
            DispatchRefusal::Refused(message) => message,
        }
    }
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    let mut definitions = vec![
        ToolDefinition {
            name: LIST_SUBAGENTS_TOOL_NAME.into(),
            description: "List the root and all descendants in your own family, including retained idle/finished contexts. Returns stable identities, immutable handles, descriptions and actual availability/state/timestamps. Listing never runs a model, reopens history or grants authority. Messages and completion notifications arrive automatically; finish your turn when awaiting new input instead of repeatedly listing.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        ToolDefinition {
            name: SEND_AGENT_MESSAGE_TOOL_NAME.into(),
            description: "Send task context or clarification to one member of your family, including siblings. Use to=parent for your parent, or a bare handle or complete child_chat_id returned by Subagent. Messages arrive automatically at the next tool round; an idle recipient starts a new turn. A finished child (completed, failed or stopped) resumes its existing context and frozen authorization; an ended root or expired task requires human continuation. Agent messages never grant user approval, alter permissions or expand inherited scope. Self, unrelated families and broadcast are unavailable.".into(),
            input_schema: json!({"type":"object","properties":{"to":{"type":"string","minLength":1},"message":{"type":"string","minLength":1},"relay_user_message_id":{"type":"string","format":"uuid","description":"Optional reference to genuine human input in your own current chain. Relay stays Agent-origin, never user authorization."}},"required":["to","message"],"additionalProperties":false}),
        },
    ];
    definitions.push(ToolDefinition {
            name: STOP_SUBAGENT_TOOL_NAME.into(),
            description: "Stop one direct child and its descendants under your normal admission policy. The root may target any descendant. Cancels active/parked runs, queued work, requests, triggers and owned commands; preserves context, handles and Views. Repeated stop is idempotent. View close or panel hiding does not stop execution. Parent, siblings, other families and whole-family stop are unavailable.".into(),
            input_schema: json!({"type":"object","properties":{"to":{"type":"string","minLength":1}},"required":["to"],"additionalProperties":false}),
        });
    definitions
}

fn requires_human_continuation(snapshot: &ChatSnapshot, state: &LoopState) -> bool {
    *state == LoopState::Expired
        || (snapshot.delegation.is_none()
            && matches!(
                state,
                LoopState::Stopped | LoopState::Interrupted | LoopState::Failed
            ))
}

fn result(value: Value) -> ToolResult {
    ToolResult {
        content: value.to_string(),
        is_error: false,
        output_ref: None,
    }
}

fn family_root(chats: &HashMap<ChatId, Chat>, member: ChatId) -> Result<ChatId, String> {
    super::family_tree::root(chats, member)
}

fn peer_target(
    chats: &HashMap<ChatId, Chat>,
    registry: &HashMap<ChatId, Vec<SubagentRegistration>>,
    sender: &Receipt,
    target: &str,
) -> Result<(ChatId, LoopId, &'static str), String> {
    let source = chats
        .get(&sender.chat_id)
        .ok_or("sender Agent is unavailable")?;
    let root = family_root(chats, sender.chat_id)?;
    let target_id = if target == "parent" {
        source
            .snapshot
            .delegation
            .as_ref()
            .ok_or("this Agent has no parent")?
            .parent_chat_id
    } else if target == "main" {
        root
    } else {
        registry
            .values()
            .flatten()
            .find(|entry| {
                entry.handle == target
                    && chats
                        .get(&entry.child_chat_id)
                        .and_then(|chat| chat.snapshot.delegation.as_ref())
                        .is_some_and(|delegation| delegation.root_chat_id() == root)
            })
            .map(|entry| entry.child_chat_id)
            .or_else(|| Uuid::parse_str(target).ok())
            .ok_or("use a family handle or complete family Chat identity")?
    };
    if target_id == sender.chat_id {
        return Err("an Agent cannot send a message to itself".into());
    }
    if family_root(chats, target_id)? != root {
        return Err("Agent messages are limited to one actual family".into());
    }
    let receiver = &chats[&target_id];
    if target_id == root {
        let delegation = source
            .snapshot
            .delegation
            .as_ref()
            .ok_or("sender has no parent")?;
        return Ok((target_id, delegation.root_loop_id(), "child"));
    }
    let loop_id = receiver
        .snapshot
        .loops
        .first()
        .ok_or("child Loop is unavailable")?
        .id;
    Ok((
        target_id,
        loop_id,
        if receiver
            .snapshot
            .delegation
            .as_ref()
            .is_some_and(|delegation| delegation.parent_chat_id == sender.chat_id)
        {
            "parent"
        } else if source
            .snapshot
            .delegation
            .as_ref()
            .is_some_and(|delegation| delegation.parent_chat_id == target_id)
        {
            "child"
        } else {
            "sibling"
        },
    ))
}

pub(super) fn child_run_result(
    snapshot: &ChatSnapshot,
    child: &Receipt,
    parent: &Receipt,
    registration: Option<&SubagentRegistration>,
) -> Option<ToolResult> {
    let goal = snapshot
        .loops
        .iter()
        .find(|goal| goal.id == child.loop_id)?;
    let (status, error) = snapshot
        .events
        .iter()
        .rev()
        .find_map(|event| {
            if event.run_id != child.run_id {
                return None;
            }
            match &event.kind {
                EventKind::Completed { .. } => Some(("completed", None)),
                EventKind::Failed(error) => Some(("failed", Some(error.as_str()))),
                EventKind::Interrupted => Some(("interrupted", None)),
                _ => None,
            }
        })
        .or_else(|| {
            (goal.run_id == Some(child.run_id))
                .then_some(match goal.state {
                    LoopState::Interrupted | LoopState::Stopped | LoopState::Expired => {
                        Some(("interrupted", None))
                    }
                    LoopState::Failed => Some(("failed", None)),
                    LoopState::Completed => Some(("completed", None)),
                    _ => None,
                })
                .flatten()
        })?;
    let text = crate::handback::completion_text(snapshot, child.run_id);
    let metadata = snapshot.delegation.as_ref()?;
    Some(ToolResult {
        content: json!({"delegation_id":metadata.delegation_id,"child_chat_id":child.chat_id,
            "handle":registration?.handle,"description":registration?.description,"created_at":registration?.created_at,
            "child_loop_id":child.loop_id,"child_run_id":child.run_id,"parent_chat_id":parent.chat_id,
            "status":status,"child_state":format!("{:?}", goal.state).to_lowercase(),"result":text,"error":error,
            "usage":snapshot.request_usage.run_conversation_usage(child.run_id).unwrap_or_default()}).to_string(),
        is_error:status != "completed", output_ref:None,
    })
}

impl Agent {
    pub(super) async fn prepare_family_call(
        &self,
        sender: &Receipt,
        name: &str,
        input: &Value,
    ) -> Result<PreparedFamilyCall, FamilyMessageRejection> {
        if name != STOP_SUBAGENT_TOOL_NAME {
            return self
                .prepare_family_message(sender, input)
                .await
                .map(PreparedFamilyCall::Message);
        }
        let snapshot =
            self.snapshot(sender.chat_id)
                .await
                .map_err(|error| FamilyMessageRejection {
                    code: FamilyMessageRejectionCode::OriginInactive,
                    message: error.to_string(),
                })?;
        self.family_registrations(sender.chat_id)
            .await
            .map_err(|error| FamilyMessageRejection {
                code: FamilyMessageRejectionCode::InvalidTarget,
                message: error.to_string(),
            })?;
        let chats = self.inner.chats.lock().await;
        let registry = self.inner.subagent_registry.lock().await;
        let (target, _, relationship) = peer_target(
            &chats,
            &registry,
            sender,
            input["to"].as_str().unwrap_or_default(),
        )
        .map_err(|message| FamilyMessageRejection {
            code: FamilyMessageRejectionCode::InvalidTarget,
            message,
        })?;
        if relationship != "parent" && snapshot.delegation.is_some() {
            return Err(FamilyMessageRejection {
                code: FamilyMessageRejectionCode::InvalidTarget,
                message: "StopSubagent targets only your direct child".into(),
            });
        }
        let mut targets = super::family_tree::descendants(&chats, target).map_err(|message| {
            FamilyMessageRejection {
                code: FamilyMessageRejectionCode::InvalidTarget,
                message,
            }
        })?;
        targets.push(target);
        targets.sort();
        let generations = targets
            .iter()
            .map(|id| (*id, chats[id].family_generation))
            .collect();
        let mut policy_input = input.clone();
        policy_input["to"] = target.to_string().into();
        policy_input["affected_members"] = json!(targets);
        Ok(PreparedFamilyCall::Stop(PreparedFamilyStop {
            policy_input,
            target,
            generations,
        }))
    }

    pub(super) async fn prepare_family_message(
        &self,
        sender: &Receipt,
        input: &Value,
    ) -> Result<PreparedFamilyMessage, FamilyMessageRejection> {
        let reject = |code, message: String| FamilyMessageRejection { code, message };
        let snapshot = self.snapshot(sender.chat_id).await.map_err(|error| {
            reject(
                FamilyMessageRejectionCode::OriginInactive,
                error.to_string(),
            )
        })?;
        let root = snapshot
            .delegation
            .as_ref()
            .map_or(sender.chat_id, |delegation| delegation.root_chat_id());
        self.family_registrations(root).await.map_err(|error| {
            reject(FamilyMessageRejectionCode::InvalidTarget, error.to_string())
        })?;
        let chats = self.inner.chats.lock().await;
        let registry = self.inner.subagent_registry.lock().await;
        let (target, loop_id, relationship) = peer_target(
            &chats,
            &registry,
            sender,
            input["to"].as_str().unwrap_or_default(),
        )
        .map_err(|message| reject(FamilyMessageRejectionCode::InvalidTarget, message))?;
        if chats[&target]
            .snapshot
            .loops
            .iter()
            .find(|goal| goal.id == loop_id)
            .is_some_and(|goal| requires_human_continuation(&chats[&target].snapshot, &goal.state))
        {
            return Err(reject(
                FamilyMessageRejectionCode::StaleTarget,
                "an ended root or expired task requires explicit human continuation".into(),
            ));
        }
        let relay = input
            .get("relay_user_message_id")
            .map(|reference| {
                let id = reference
                    .as_str()
                    .and_then(|value| Uuid::parse_str(value).ok())
                    .ok_or_else(|| {
                        reject(
                            FamilyMessageRejectionCode::InvalidUserRelay,
                            "relay reference must be a genuine human message ID".into(),
                        )
                    })?;
                let original = approval_context::originals_for_active_chain(
                    &chats[&sender.chat_id].snapshot,
                    &chats[&sender.chat_id].bookkeeping,
                )
                .ok()
                .and_then(|inputs| {
                    inputs.into_iter().find(|original| {
                        original.chat_id == sender.chat_id && original.message_id == id
                    })
                })
                .ok_or_else(|| {
                    reject(
                        FamilyMessageRejectionCode::InvalidUserRelay,
                        "relay reference is not genuine human input in the sender's active chain"
                            .into(),
                    )
                })?;
                Ok(VerifiedUserRelay {
                    original_chat_id: original.chat_id,
                    original_message_id: original.message_id,
                    forwarded_verbatim: input["message"].as_str() == Some(original.text.as_str()),
                    original_text: original.text,
                })
            })
            .transpose()?;
        let mut policy_input = input.clone();
        policy_input["to"] = target.to_string().into();
        let cause = family_mailbox::current_cause(
            &chats[&sender.chat_id].snapshot,
            &chats[&sender.chat_id].bookkeeping,
            sender,
        )?;
        Ok(PreparedFamilyMessage {
            cause,
            policy_input,
            target,
            loop_id,
            target_generation: chats[&target].family_generation,
            relationship,
            relay,
        })
    }

    pub(super) async fn agent_message_tool(
        &self,
        receipt: &Receipt,
        call_id: &str,
        name: &str,
        input: &Value,
        prepared: Option<&PreparedFamilyCall>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        match name {
            STOP_SUBAGENT_TOOL_NAME => {
                let Some(PreparedFamilyCall::Stop(stop)) = prepared else {
                    return Err("StopSubagent was not prepared".into());
                };
                self.stop_family_child(receipt, stop, &cancel).await
            }
            LIST_SUBAGENTS_TOOL_NAME => self
                .family_agents(receipt.chat_id)
                .await
                .map(|members| result(json!({"status":"ok","members":members})))
                .map_err(|error| error.to_string()),
            SEND_AGENT_MESSAGE_TOOL_NAME => {
                let message = input["message"]
                    .as_str()
                    .filter(|message| !message.trim().is_empty())
                    .ok_or("message cannot be empty")?;
                let Some(PreparedFamilyCall::Message(prepared)) = prepared else {
                    return Err("SendMessage was not prepared".into());
                };
                self.deliver_agent_message(
                    receipt,
                    call_id,
                    FamilyDelivery::Message {
                        prepared,
                        body: message,
                    },
                    &cancel,
                )
                .await
                .map_err(String::from)
            }
            _ => Err("Agent message tool is unavailable".into()),
        }
    }

    async fn deliver_agent_message(
        &self,
        sender: &Receipt,
        call_id: &str,
        delivery: FamilyDelivery<'_>,
        cancel: &CancellationToken,
    ) -> Result<ToolResult, DispatchRefusal> {
        if call_id.starts_with("task-assignment-") {
            self.refresh_task_tools_setting()
                .await
                .map_err(|error| error.to_string())?;
        }
        let is_handback = matches!(&delivery, FamilyDelivery::Handback { .. });
        let is_completion = matches!(&delivery, FamilyDelivery::Completion { .. });
        let (target, message, is_result, prepared) = match delivery {
            FamilyDelivery::Message { prepared, body } => {
                (prepared.target.to_string(), body, false, Some(prepared))
            }
            FamilyDelivery::Handback { prepared, body } => {
                (prepared.target.to_string(), body, true, Some(prepared))
            }
            FamilyDelivery::Completion { parent, body } => {
                (parent.chat_id.to_string(), body, true, None)
            }
        };
        let source_registration = self
            .subagent_registration(sender.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        self.subagents(sender.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut mailbox = self.inner.family_mailbox.lock().await;
        let mut chats = self.inner.chats.lock().await;
        for chat in chats.values() {
            mailbox.synchronize(&chat.snapshot);
        }
        let (target_id, loop_id, relationship) = peer_target(
            &chats,
            &*self.inner.subagent_registry.lock().await,
            sender,
            &target,
        )?;
        if prepared.is_some_and(|prepared| {
            prepared.target != target_id
                || prepared.loop_id != loop_id
                || prepared.relationship != relationship
                || prepared.target_generation != chats[&target_id].family_generation
        }) {
            return Ok(FamilyMessageRejection {
                code: FamilyMessageRejectionCode::StaleTarget,
                message: "prepared family target changed before dispatch".into(),
            }
            .result());
        }
        let task_message_id =
            shared_tasks::assignment_message_id(&chats, sender, target_id, message, call_id);
        if call_id.starts_with("task-assignment-") && task_message_id.is_none() {
            return Err("task assignment was cleared or superseded before dispatch".into());
        }
        if let Some(message_id) = task_message_id {
            if !chats[&sender.chat_id].snapshot.task_tools_enabled {
                return Err(
                    "Task tools are disabled; assignment notification was not dispatched".into(),
                );
            }
            let target = &chats[&target_id].snapshot;
            if target
                .agent_message_origins
                .values()
                .any(|source| source["message_id"] == message_id.to_string())
                || target.pending_inputs.iter().any(|input| {
                    input
                        .agent_message
                        .as_ref()
                        .is_some_and(|source| source.message_id == message_id)
                })
            {
                return Ok(result(
                    json!({"status":"accepted","message_id":message_id,"delivery":"already_accepted"}),
                ));
            }
        }
        let key = (sender.run_id, call_id.to_owned());
        if let Some(existing) = mailbox.sent.get(&key) {
            return if existing.target == target_id
                && existing.message == message
                && existing.relay == prepared.and_then(|prepared| prepared.relay.clone())
            {
                Ok(existing.result.clone())
            } else {
                Err("message call already delivered different content or target".into())
            };
        }
        if self.inner.closed.load(Ordering::SeqCst) || cancel.is_cancelled() {
            return Err("message not dispatched: run interrupted or owner shut down".into());
        }
        if is_result
            && (chats[&target_id]
                .snapshot
                .agent_message_origins
                .contains_key(&sender.run_id)
                || chats[&target_id]
                    .snapshot
                    .agent_message_origins
                    .values()
                    .any(|source| source["message_id"] == sender.run_id.to_string())
                || chats[&target_id]
                    .snapshot
                    .pending_inputs
                    .iter()
                    .any(|pending| {
                        pending
                            .agent_message
                            .as_ref()
                            .is_some_and(|source| source.message_id == sender.run_id)
                    }))
        {
            return Ok(result(
                json!({"status":"accepted","message_id":sender.run_id,"target_chat_id":target_id,"delivery":"already_accepted"}),
            ));
        }
        if !is_result && chats[&sender.chat_id].active_run != Some(sender.run_id) {
            return Err("message not dispatched: originating run is no longer active".into());
        }
        let sender_label = family_label(
            &chats[&sender.chat_id].snapshot,
            source_registration.as_ref(),
        );
        let mut cause = if let Some(prepared) = prepared {
            prepared.cause.clone()
        } else {
            family_mailbox::current_cause(
                &chats[&sender.chat_id].snapshot,
                &chats[&sender.chat_id].bookkeeping,
                sender,
            )
            .map_err(|rejection| rejection.message)?
        };
        cause.depth = cause.depth.saturating_add(1);
        let original = chats.get_mut(&target_id).unwrap();
        if original.family_stopping {
            return Err("Subagent subtree is stopping".into());
        }
        let is_target_run_ending = original
            .active
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
            || original
                .parked
                .values()
                .any(|run| run.receipt.loop_id == loop_id && run.cancel.is_cancelled());
        let is_child = original.snapshot.delegation.is_some();
        let goal_index = original
            .snapshot
            .loops
            .iter()
            .position(|goal| goal.id == loop_id)
            .ok_or("target Loop is unavailable")?;
        let target_state = &original.snapshot.loops[goal_index].state;
        let awaits_continuation =
            !is_child && matches!(target_state, LoopState::Interrupted | LoopState::Failed);
        if is_completion && (awaits_continuation || is_target_run_ending) {
            return Err(DispatchRefusal::Deferred);
        }
        if is_target_run_ending {
            return Err("message not dispatched: target run was interrupted".into());
        }
        if *target_state == LoopState::Expired
            || awaits_continuation
            || (!is_child && *target_state == LoopState::Stopped)
        {
            return Err(ENDED_TARGET_REFUSAL.into());
        }
        let mut candidate = original.clone();
        let receiver = &mut candidate;
        let registry = self.inner.subagent_registry.lock().await;
        let target_registration = receiver.snapshot.delegation.as_ref().and_then(|metadata| {
            registry
                .get(&metadata.parent_chat_id)?
                .iter()
                .find(|entry| entry.child_chat_id == target_id)
        });
        let target_label = family_label(&receiver.snapshot, target_registration);
        drop(registry);
        let goal = &mut receiver.snapshot.loops[goal_index];
        if !matches!(
            goal.state,
            LoopState::Running
                | LoopState::WaitingPermission
                | LoopState::WaitingQuestion
                | LoopState::WaitingPlan
        ) {
            goal.set_state(LoopState::Queued);
        }
        let now = tickloop_agent_base::private_fs::unix_time_milliseconds();
        let pending_count = receiver
            .snapshot
            .pending_inputs
            .iter()
            .filter(|pending| pending.agent_message.is_some())
            .count();
        let guard = if is_result {
            mailbox.check_notification(&cause, pending_count)
        } else {
            mailbox.check(sender.chat_id, &cause, pending_count, now)
        };
        if let Err(rejection) = guard {
            return Ok(rejection.result());
        }
        let source = AgentMessageSource {
            cause: cause.clone(),
            message_id: if is_result {
                sender.run_id
            } else {
                task_message_id.unwrap_or_else(Uuid::new_v4)
            },
            call_id: Some(call_id.into()),
            created_at: now,
            sender_chat_id: sender.chat_id,
            sender_loop_id: sender.loop_id,
            sender_run_id: sender.run_id,
            sender_relationship: relationship.into(),
            sender_label,
            sender_handle: source_registration
                .as_ref()
                .map_or("main", |registration| registration.handle.as_str())
                .into(),
            relay: prepared.and_then(|prepared| prepared.relay.clone()),
            is_result,
        };
        let receipt = Receipt {
            chat_id: target_id,
            loop_id,
            run_id: Uuid::new_v4(),
        };
        receiver.snapshot.last_queue_sequence += 1;
        let queued_sequence = receiver.snapshot.last_queue_sequence;
        let input = if is_result && !is_handback {
            message.to_owned()
        } else {
            local_messages::agent_message(&source, message)
        };
        let index = receiver
            .snapshot
            .pending_inputs
            .iter()
            .position(|pending| pending.trigger_source.is_some())
            .unwrap_or(receiver.snapshot.pending_inputs.len());
        receiver.snapshot.pending_inputs.insert(
            index,
            PendingInput {
                receipt: receipt.clone(),
                input,
                contexts: Vec::new(),
                agent_message: Some(source.clone()),
                directed_human: None,
                denial_reference: None,
                trigger_source: None,
                armed: true,
                queued_sequence,
                task_continuation: false,
                is_skill_invocation: false,
                attached_file_reads: Vec::new(),
            },
        );
        self.remember_host_context(&receipt, None);
        self.emit(
            receiver,
            &receipt,
            EventKind::AgentMessageQueued {
                source: source.clone(),
            },
        );
        if is_handback {
            self.checkpoint_transcript(&receiver.snapshot, &receiver.bookkeeping)
                .await?;
        } else {
            self.save(&receiver.snapshot);
        }
        *original = candidate;
        if !is_result {
            mailbox.record_source(&source);
            let sender_chat = chats
                .get_mut(&sender.chat_id)
                .expect("admitted sender remains loaded");
            sender_chat
                .snapshot
                .sent_agent_messages
                .insert(source.message_id, source.clone());
            if let Some(recorder) = &self.inner.recorder {
                recorder.record_sent_agent_messages(
                    sender.chat_id,
                    sender_chat.snapshot.sent_agent_messages.clone(),
                );
            }
        }
        let delivered = result(
            json!({"status":"accepted","message_id":source.message_id,"target_chat_id":target_id,
            "target_loop_id":loop_id,"target_run_id":receipt.run_id,"target_label":target_label,"target_relationship":match relationship {"parent"=>"child","child"=>"parent",_=>"sibling"},"delivery":"queued_at_tool_boundary"}),
        );
        mailbox.sent.insert(
            key,
            SentMessage {
                target: target_id,
                message: message.into(),
                relay: prepared.and_then(|prepared| prepared.relay.clone()),
                result: delivered.clone(),
            },
        );
        drop(chats);
        drop(mailbox);
        let next = self
            .next_run(target_id)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(next) = next {
            self.run_task(next);
        }
        Ok(delivered)
    }

    pub(super) async fn deliver_handback_message(
        &self,
        child: &Receipt,
        call_id: &str,
        target: &PreparedFamilyCall,
        report: &str,
        cancel: &CancellationToken,
    ) -> Result<ToolResult, String> {
        let PreparedFamilyCall::Message(prepared) = target else {
            return Err("Handback target was not prepared".into());
        };
        self.deliver_agent_message(
            child,
            call_id,
            FamilyDelivery::Handback {
                prepared,
                body: report,
            },
            cancel,
        )
        .await
        .map_err(String::from)
    }

    pub(super) async fn deliver_subagent_notification(
        &self,
        child: &Receipt,
        parent: &Receipt,
        completion: &FamilyCompletion,
    ) -> Result<ToolResult, DispatchRefusal> {
        let body = local_messages::subagent_notification(completion);
        self.deliver_agent_message(
            child,
            "subagent-completion",
            FamilyDelivery::Completion {
                parent,
                body: &body,
            },
            &self.inner.shutdown,
        )
        .await
    }
}

impl Agent {
    async fn stop_family_child(
        &self,
        sender: &Receipt,
        prepared: &PreparedFamilyStop,
        cancel: &CancellationToken,
    ) -> Result<ToolResult, String> {
        let stopped = self
            .stop_subagent_tree(
                prepared.target,
                Some((&prepared.generations, sender, cancel)),
            )
            .await
            .map_err(|error| error.to_string())?;
        let failed = !stopped.failures.is_empty();
        Ok(ToolResult {
            content: json!({"status":if failed {"partial_failure"} else {"stopped"},"child_chat_id":prepared.target,
                "unresolved_operations":stopped.operations,"failures":stopped.failures}).to_string(),
            is_error: failed, output_ref: None,
        })
    }

    /// Installs the hook StopSubagent calls once a child is stopped.
    pub fn install_stopped_child_hook(&self, hook: Arc<dyn StoppedChildHook>) {
        *self.inner.stopped_child_hook.write().unwrap() = Some(hook);
    }
}
