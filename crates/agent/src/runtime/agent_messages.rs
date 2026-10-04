use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::{model::ToolDefinition, tools::ToolResult};

pub const SEND_AGENT_MESSAGE_TOOL_NAME: &str = "SendMessage";
pub const RECEIVE_AGENT_MESSAGE_TOOL_NAME: &str = "ReceiveAgentMessage";
pub const WAIT_FOR_AGENT_TOOL_NAME: &str = "WaitForAgent";
pub const LIST_FAMILY_AGENTS_TOOL_NAME: &str = "ListFamilyAgents";
pub const STOP_AGENT_TOOL_NAME: &str = "StopAgent";
const DEFAULT_WAIT_MS: u64 = 60_000;
const MAX_WAIT_MS: u64 = 600_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct VerifiedUserRelay {
    pub original_chat_id: ChatId,
    pub original_message_id: Uuid,
    pub original_text: String,
    pub forwarded_verbatim: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilyMessageRejectionCode {
    InvalidTarget,
    InvalidUserRelay,
    StaleTarget,
    OriginInactive,
    DepthLimit,
    ChainLimit,
    SenderRateLimit,
    InboxFull,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FamilyMessageRejection {
    pub code: FamilyMessageRejectionCode,
    pub message: String,
}

impl FamilyMessageRejection {
    pub(super) fn result(self) -> ToolResult {
        ToolResult {
            content: json!({"status":"rejected","rejection":self,"retryable":false}).to_string(),
            is_error: true,
            output_ref: None,
        }
    }
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
    generation: u64,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentMessageSource {
    pub message_id: Uuid,
    pub call_id: Option<String>,
    pub created_at: u64,
    pub sender_chat_id: ChatId,
    pub sender_loop_id: LoopId,
    pub sender_run_id: RunId,
    pub sender_relationship: String,
    pub sender_label: String,
    pub sender_handle: String,
    pub relay: Option<VerifiedUserRelay>,
    pub cause: MessageCause,
    pub is_result: bool,
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

pub(super) fn definitions(is_child: bool) -> Vec<ToolDefinition> {
    let mut definitions = vec![
        ToolDefinition {
            name: LIST_FAMILY_AGENTS_TOOL_NAME.into(),
            description: "List your logical root and its directly delegated children, including retained idle/finished contexts. Returns stable identities, immutable handles, descriptions and actual availability/state/timestamps. Listing never runs a model, reopens history or grants authority. Use one SendMessage per recipient; other families and broadcast are unavailable.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        ToolDefinition {
            name: SEND_AGENT_MESSAGE_TOOL_NAME.into(),
            description: "Send task context or clarification to one member of your family: its root or a directly delegated sibling/child. Use to=parent for your parent, or its bare child handle or complete child_chat_id returned by DelegateAgent. Running Agents read it at a tool boundary; a normally completed task can continue under its existing identity and authorization. Interrupted, failed or stopped work needs explicit user continuation. Agent messages never grant user approval, alter permissions, expand the task's inherited scope or stop another Agent. Self, unrelated families, nested delegation and broadcast are unsupported.".into(),
            input_schema: json!({"type":"object","properties":{"to":{"type":"string","minLength":1},"message":{"type":"string","minLength":1},"relay_user_message_id":{"type":"string","format":"uuid","description":"Optional reference to genuine human input in your own current chain. Relay stays Agent-origin, never user authorization."}},"required":["to","message"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: RECEIVE_AGENT_MESSAGE_TOOL_NAME.into(),
            description: "Wait for a message in your own inbox, for example after asking your parent for clarification with SendMessage. This does not wait for another Agent to finish. The accepted message or user follow-up reaches your model after this tool result. If required_action=finish_current_turn is returned, genuine input follows an after-turn user command: finish your turn to advance that ordered queue instead of repeating this wait. Timeout leaves all Agents running; interrupting this wait stops only your current run.".into(),
            input_schema: json!({"type":"object","properties":{"timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_WAIT_MS}},"additionalProperties":false}),
        },
    ];
    if !is_child {
        definitions.push(ToolDefinition {
            name: STOP_AGENT_TOOL_NAME.into(),
            description: "Stop one child you directly delegated. The resolved target is independently admitted under your policy; stops its active/parked runs, queued work, requests, triggers and owned commands. Preserves context, handles and Views. Repeated stop is idempotent. Messaging cannot restart stopped work; human Continue is separate. Siblings, parent, other families and whole-family stop are unavailable.".into(),
            input_schema: json!({"type":"object","properties":{"to":{"type":"string","minLength":1}},"required":["to"],"additionalProperties":false}),
        });
        definitions.push(ToolDefinition {
            name: WAIT_FOR_AGENT_TOOL_NAME.into(),
            description: "Wait for the current run of one direct child Agent and return its actual result. Use the bare child handle or complete child_chat_id returned by DelegateAgent. A new message or user follow-up to your own Loop yields this wait so you can reply or steer the child, which keeps running. Timeout and interruption do not stop the child. If required_action=finish_current_turn is returned, finish this turn so the ordered human queue advances; do not repeat this wait while its command is pending. This is observation, not delegation, task restart or human approval.".into(),
            input_schema: json!({"type":"object","properties":{"to":{"type":"string","minLength":1},"timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_WAIT_MS}},"required":["to"],"additionalProperties":false}),
        });
    }
    definitions
}

fn result(value: Value) -> ToolResult {
    ToolResult {
        content: value.to_string(),
        is_error: false,
        output_ref: None,
    }
}

fn family_root(chats: &HashMap<ChatId, Chat>, member: ChatId) -> Result<ChatId, String> {
    let member = chats.get(&member).ok_or("family member is unavailable")?;
    let Some(delegation) = &member.snapshot.delegation else {
        return Ok(member.snapshot.id);
    };
    let root = chats
        .get(&delegation.parent_chat_id)
        .ok_or("family root is unavailable")?;
    if root.snapshot.delegation.is_some() {
        return Err("nested families are unsupported".into());
    }
    Ok(root.snapshot.id)
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
            .get(&root)
            .and_then(|entries| entries.iter().find(|entry| entry.handle == target))
            .map(|entry| entry.child_chat_id)
            .or_else(|| Uuid::parse_str(target).ok())
            .ok_or("use a family handle or complete family Chat identity")?
    };
    if target_id == sender.chat_id {
        return Err("an Agent cannot send a message to itself".into());
    }
    if family_root(chats, target_id)? != root {
        return Err("Agent messages are limited to one actual root and its direct children".into());
    }
    let receiver = &chats[&target_id];
    if target_id == root {
        let delegation = source
            .snapshot
            .delegation
            .as_ref()
            .ok_or("sender has no parent")?;
        return Ok((target_id, delegation.parent_loop_id, "child"));
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
        if sender.chat_id == root {
            "parent"
        } else {
            "sibling"
        },
    ))
}

fn pending_input(snapshot: &ChatSnapshot, receipt: &Receipt) -> Option<&'static str> {
    let barrier = snapshot
        .pending_commands
        .iter()
        .map(|command| command.queued_sequence)
        .chain(
            snapshot
                .user_shells
                .iter()
                .filter(|record| record.state == UserShellState::Queued && record.armed)
                .map(|record| record.queued_sequence),
        )
        .min();
    let mut blocked = false;
    for pending in &snapshot.pending_inputs {
        if !pending.armed
            || pending.trigger_source.is_some()
            || pending.task_continuation
            || pending.receipt.loop_id != receipt.loop_id
        {
            continue;
        }
        if pending.agent_message.is_some()
            || barrier.is_none_or(|sequence| pending.queued_sequence < sequence)
        {
            return Some("input_pending");
        }
        blocked = true;
    }
    blocked.then_some("after_turn_queue")
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
    let text = snapshot.run_response_text(child.run_id).unwrap_or_default();
    let metadata = snapshot.delegation.as_ref()?;
    Some(ToolResult {
        content: json!({"delegation_id":metadata.delegation_id,"child_chat_id":child.chat_id,
            "handle":registration?.handle,"description":registration?.description,"created_at":registration?.created_at,
            "child_loop_id":child.loop_id,"child_run_id":child.run_id,"parent_chat_id":parent.chat_id,
            "status":status,"child_state":format!("{:?}", goal.state).to_lowercase(),"result":text,"error":error}).to_string(),
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
        if name != STOP_AGENT_TOOL_NAME {
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
        if snapshot.delegation.is_some() {
            return Err(FamilyMessageRejection {
                code: FamilyMessageRejectionCode::InvalidTarget,
                message: "only the family root may stop its direct child".into(),
            });
        }
        self.subagents(sender.chat_id)
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
        if relationship != "parent" {
            return Err(FamilyMessageRejection {
                code: FamilyMessageRejectionCode::InvalidTarget,
                message: "StopAgent targets only your direct child".into(),
            });
        }
        let mut policy_input = input.clone();
        policy_input["to"] = target.to_string().into();
        Ok(PreparedFamilyCall::Stop(PreparedFamilyStop {
            policy_input,
            target,
            generation: chats[&target].family_generation,
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
            .map_or(sender.chat_id, |delegation| delegation.parent_chat_id);
        self.subagents(root).await.map_err(|error| {
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
            .is_some_and(|goal| {
                matches!(
                    goal.state,
                    LoopState::Stopped
                        | LoopState::Interrupted
                        | LoopState::Expired
                        | LoopState::Failed
                )
            })
        {
            return Err(reject(
                FamilyMessageRejectionCode::StaleTarget,
                "stopped or interrupted work requires explicit human continuation".into(),
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
                let original =
                    approval_context::originals_for_active_chain(&chats[&sender.chat_id].snapshot)
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
        let cause = family_mailbox::current_cause(&chats[&sender.chat_id].snapshot, sender)?;
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

    pub(super) async fn agent_wait_input(&self, receipt: &Receipt) -> Option<&'static str> {
        self.inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .and_then(|chat| pending_input(&chat.snapshot, receipt))
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
            STOP_AGENT_TOOL_NAME => {
                let Some(PreparedFamilyCall::Stop(stop)) = prepared else {
                    return Err("StopAgent was not prepared".into());
                };
                self.stop_family_child(receipt, stop, &cancel).await
            }
            LIST_FAMILY_AGENTS_TOOL_NAME => self
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
            }
            RECEIVE_AGENT_MESSAGE_TOOL_NAME => {
                let mut events = self.subscribe();
                let mut input_wakeup = self.inner.input_wakeup.subscribe();
                let deadline = tokio::time::Instant::now()
                    + Duration::from_millis(
                        input["timeout_ms"].as_u64().unwrap_or(DEFAULT_WAIT_MS),
                    );
                loop {
                    if let Some(reason) = self.agent_wait_input(receipt).await {
                        return Ok(result(
                            json!({"status":"input_pending","delivery":if reason=="after_turn_queue" {"after_turn_queue"} else {"after_tool_result"},"required_action":(reason=="after_turn_queue").then_some("finish_current_turn")}),
                        ));
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return Err("message receive interrupted".into()),
                        _ = input_wakeup.changed()=>{},
                        _ = tokio::time::sleep_until(deadline) => return Ok(result(json!({"status":"timeout"}))),
                        event = events.recv() => if matches!(event, Err(broadcast::error::RecvError::Closed)) { return Err("Agent event stream closed".into()); },
                    }
                }
            }
            WAIT_FOR_AGENT_TOOL_NAME => {
                let mut events = self.subscribe();
                let mut input_wakeup = self.inner.input_wakeup.subscribe();
                let child = {
                    let chats = self.inner.chats.lock().await;
                    let (target, loop_id, relationship) = peer_target(
                        &chats,
                        &*self.inner.subagent_registry.lock().await,
                        receipt,
                        input["to"].as_str().ok_or("child target is missing")?,
                    )?;
                    if relationship != "parent" {
                        return Err("WaitForAgent observes only your direct child".into());
                    }
                    let chat = &chats[&target];
                    let snapshot = &chat.snapshot;
                    let run_id = chat
                        .active_run
                        .filter(|run| snapshot.run_loops.get(run) == Some(&loop_id))
                        .or_else(|| {
                            chat.parked
                                .values()
                                .find(|run| run.receipt.loop_id == loop_id)
                                .map(|run| run.receipt.run_id)
                        })
                        .or_else(|| {
                            snapshot
                                .pending_inputs
                                .iter()
                                .rev()
                                .find(|pending| pending.armed && pending.receipt.loop_id == loop_id)
                                .map(|pending| pending.receipt.run_id)
                        })
                        .or_else(|| {
                            snapshot
                                .loops
                                .iter()
                                .find(|goal| goal.id == loop_id)
                                .and_then(|goal| goal.run_id)
                        })
                        .ok_or("child has no run to observe")?;
                    Receipt {
                        chat_id: target,
                        loop_id,
                        run_id,
                    }
                };
                let deadline = tokio::time::Instant::now()
                    + Duration::from_millis(
                        input["timeout_ms"].as_u64().unwrap_or(DEFAULT_WAIT_MS),
                    );
                loop {
                    let snapshot = self
                        .snapshot(child.chat_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    let registration = self
                        .subagent_registration(child.chat_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    if let Some(result) =
                        child_run_result(&snapshot, &child, receipt, registration.as_ref())
                    {
                        return Ok(result);
                    }
                    if let Some(reason) = self.agent_wait_input(receipt).await {
                        self.register_parent_result_delivery(receipt.clone(), child.clone())
                            .await;
                        return Ok(result(
                            json!({"status":"running","child_chat_id":child.chat_id,"child_run_id":child.run_id,"wait_reason":reason,"required_action":(reason=="after_turn_queue").then_some("finish_current_turn")}),
                        ));
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return Err("child wait interrupted; child execution continues independently".into()),
                        _ = input_wakeup.changed()=>{},
                        _ = tokio::time::sleep_until(deadline) => {
                            self.register_parent_result_delivery(receipt.clone(), child.clone()).await;
                            return Ok(result(json!({"status":"running","child_chat_id":child.chat_id,"child_run_id":child.run_id,"wait_reason":"timeout"})));
                        },
                        event = events.recv() => if matches!(event, Err(broadcast::error::RecvError::Closed)) { return Err("Agent event stream closed".into()); },
                    }
                }
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
    ) -> Result<ToolResult, String> {
        let (target, message, is_result, prepared) = match delivery {
            FamilyDelivery::Message { prepared, body } => {
                (prepared.target.to_string(), body, false, Some(prepared))
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
            family_mailbox::current_cause(&chats[&sender.chat_id].snapshot, sender)
                .map_err(|rejection| rejection.message)?
        };
        if !is_result {
            cause.depth = cause.depth.saturating_add(1);
        }
        let original = chats.get_mut(&target_id).unwrap();
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
        if receiver
            .active
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
            || receiver
                .parked
                .values()
                .any(|run| run.receipt.loop_id == loop_id && run.cancel.is_cancelled())
        {
            return Err("message not dispatched: target run was interrupted".into());
        }
        let goal = receiver
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == loop_id)
            .ok_or("target Loop is unavailable")?;
        if matches!(
            goal.state,
            LoopState::Interrupted | LoopState::Stopped | LoopState::Failed | LoopState::Expired
        ) {
            return Err("message not dispatched: interrupted or ended work requires explicit user continuation".into());
        }
        if !matches!(
            goal.state,
            LoopState::Running | LoopState::WaitingPermission | LoopState::WaitingQuestion
        ) {
            goal.set_state(LoopState::Queued);
        }
        let now = crate::private_fs::unix_time_milliseconds();
        if !is_result
            && let Err(rejection) = mailbox.check(
                sender.chat_id,
                &cause,
                receiver
                    .snapshot
                    .pending_inputs
                    .iter()
                    .filter(|pending| pending.agent_message.is_some())
                    .count(),
                now,
            )
        {
            return Ok(rejection.result());
        }
        let source = AgentMessageSource {
            cause: cause.clone(),
            message_id: Uuid::new_v4(),
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
        let input = local_messages::agent_message(&source, message);
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
        self.save(&receiver.snapshot);
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
        self.inner.input_wakeup.send_replace(());
        if let Some(next) = next {
            self.inner.tasks.lock().await.push(self.run_task(next));
        }
        Ok(delivered)
    }

    pub(super) async fn deliver_child_result(
        &self,
        child: &Receipt,
        parent: &Receipt,
        result: &str,
    ) -> Result<ToolResult, String> {
        self.deliver_agent_message(
            child,
            "delegation-result",
            FamilyDelivery::Completion {
                parent,
                body: result,
            },
            &self.inner.shutdown,
        )
        .await
    }

    pub(super) async fn register_parent_result_delivery(&self, parent: Receipt, child: Receipt) {
        self.inner
            .parent_result_waits
            .lock()
            .await
            .entry(child.run_id)
            .or_insert(parent.clone());
        if let Ok(snapshot) = self.snapshot(child.chat_id).await
            && let Ok(registration) = self.subagent_registration(child.chat_id).await
            && child_run_result(&snapshot, &child, &parent, registration.as_ref()).is_some()
        {
            self.record_family_completion(&child, &snapshot).await;
        }
    }
}

impl Agent {
    async fn stop_family_child(
        &self,
        sender: &Receipt,
        prepared: &PreparedFamilyStop,
        cancel: &CancellationToken,
    ) -> Result<ToolResult, String> {
        let (loops, commands, shells) = {
            let mut chats = self.inner.chats.lock().await;
            let root = chats.get(&sender.chat_id).ok_or("root is unavailable")?;
            if root.snapshot.delegation.is_some()
                || root.active_run != Some(sender.run_id)
                || cancel.is_cancelled()
            {
                return Err("StopAgent origin is no longer an active root".into());
            }
            let child = chats
                .get_mut(&prepared.target)
                .ok_or("child is unavailable")?;
            if child.family_generation != prepared.generation
                || child
                    .snapshot
                    .delegation
                    .as_ref()
                    .is_none_or(|delegation| delegation.parent_chat_id != sender.chat_id)
            {
                return Err("prepared child binding changed".into());
            }
            if !child
                .snapshot
                .loops
                .iter()
                .all(|goal| matches!(goal.state, LoopState::Stopped | LoopState::Expired))
            {
                child.family_generation = child.family_generation.saturating_add(1);
            }
            child.snapshot.pending_commands.clear();
            child.snapshot.pending_inputs.clear();
            child.ready_runs.clear();
            for record in &mut child.snapshot.user_shells {
                if record.state == UserShellState::Queued {
                    record.armed = false;
                }
            }
            let loops = child
                .snapshot
                .loops
                .iter()
                .map(|goal| goal.id)
                .collect::<Vec<_>>();
            let commands = child
                .snapshot
                .commands
                .iter()
                .filter(|command| {
                    matches!(
                        command.state,
                        crate::tools::CommandState::Running
                            | crate::tools::CommandState::Background
                    )
                })
                .map(|command| command.operation_id)
                .collect::<Vec<_>>();
            let shells = child
                .snapshot
                .user_shells
                .iter()
                .filter(|record| {
                    matches!(
                        record.state,
                        UserShellState::Running
                            | UserShellState::Background
                            | UserShellState::Queued
                            | UserShellState::Cancelling
                    )
                })
                .map(|record| record.id)
                .collect::<Vec<_>>();
            self.save(&child.snapshot);
            (loops, commands, shells)
        };
        let mut unknown = Vec::new();
        for loop_id in loops {
            unknown.extend(
                self.stop(prepared.target, loop_id)
                    .await
                    .map_err(|error| error.to_string())?,
            );
        }
        for command in commands {
            self.stop_command(prepared.target, command)
                .await
                .map_err(|error| error.to_string())?;
        }
        for shell in shells {
            self.stop_user_shell(prepared.target, shell)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(result(
            json!({"status":"stopped","child_chat_id":prepared.target,"unresolved_operations":unknown}),
        ))
    }
}
