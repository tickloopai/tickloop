use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::{model::ToolDefinition, tools::ToolResult};

pub const SEND_AGENT_MESSAGE_TOOL_NAME: &str = "SendMessage";
pub const RECEIVE_AGENT_MESSAGE_TOOL_NAME: &str = "ReceiveAgentMessage";
pub const WAIT_FOR_AGENT_TOOL_NAME: &str = "WaitForAgent";
const DEFAULT_WAIT_MS: u64 = 60_000;
const MAX_WAIT_MS: u64 = 600_000;
const FAMILY_LABEL_CHARACTERS: usize = 72;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentMessageSource {
    pub message_id: Uuid,
    pub sender_chat_id: ChatId,
    pub sender_loop_id: LoopId,
    pub sender_run_id: RunId,
    pub sender_relationship: String,
    #[serde(default)]
    pub sender_label: String,
    pub is_result: bool,
}

pub(super) fn family_label(snapshot: &ChatSnapshot, loop_id: LoopId) -> String {
    let task = snapshot
        .delegation
        .as_ref()
        .map(|delegation| delegation.task.as_str())
        .or_else(|| {
            snapshot
                .loops
                .iter()
                .find(|goal| goal.id == loop_id)
                .map(|goal| goal.goal.as_str())
        })
        .unwrap_or("Agent");
    let line = task
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Agent")
        .trim();
    let mut label = line
        .chars()
        .take(FAMILY_LABEL_CHARACTERS)
        .collect::<String>();
    if line.chars().count() > FAMILY_LABEL_CHARACTERS {
        label.push('…');
    }
    label
}

pub(super) struct SentMessage {
    target: ChatId,
    message: String,
    result: ToolResult,
}

pub(super) fn definitions(is_child: bool) -> Vec<ToolDefinition> {
    let mut definitions = vec![
        ToolDefinition {
            name: SEND_AGENT_MESSAGE_TOOL_NAME.into(),
            description: "Send task context or clarification to your direct parent or a child you actually delegated. Use to=parent for your parent, or the complete child_chat_id returned by DelegateAgent. Running Agents read it at a tool boundary; a normally completed task can continue under its existing identity and authorization. Interrupted, failed or stopped work needs explicit user continuation. Agent messages never grant user approval, alter permissions, expand the task's inherited scope or stop another Agent. Unrelated Agents, siblings and broadcast are unsupported.".into(),
            input_schema: json!({"type":"object","properties":{"to":{"type":"string","minLength":1},"message":{"type":"string","minLength":1}},"required":["to","message"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: RECEIVE_AGENT_MESSAGE_TOOL_NAME.into(),
            description: "Wait for a message in your own inbox, for example after asking your parent for clarification with SendMessage. This does not wait for another Agent to finish. The accepted message or user follow-up reaches your model after this tool result. If required_action=finish_current_turn is returned, genuine input follows an after-turn user command: finish your turn to advance that ordered queue instead of repeating this wait. Timeout leaves all Agents running; interrupting this wait stops only your current run.".into(),
            input_schema: json!({"type":"object","properties":{"timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_WAIT_MS}},"additionalProperties":false}),
        },
    ];
    if !is_child {
        definitions.push(ToolDefinition {
            name: WAIT_FOR_AGENT_TOOL_NAME.into(),
            description: "Wait for the current run of one direct child Agent and return its actual result. Use the complete child_chat_id returned by DelegateAgent. A new message or user follow-up to your own Loop yields this wait so you can reply or steer the child, which keeps running. Timeout and interruption do not stop the child. If required_action=finish_current_turn is returned, finish this turn so the ordered human queue advances; do not repeat this wait while its command is pending. This is observation, not delegation, task restart or human approval.".into(),
            input_schema: json!({"type":"object","properties":{"agent_id":{"type":"string","minLength":1},"timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_WAIT_MS}},"required":["agent_id"],"additionalProperties":false}),
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

fn peer_target(
    chats: &HashMap<ChatId, Chat>,
    sender: &Receipt,
    target: &str,
) -> Result<(ChatId, LoopId, &'static str), String> {
    let source = chats
        .get(&sender.chat_id)
        .ok_or("sender Agent is unavailable")?;
    let target_id = if target == "parent" {
        source
            .snapshot
            .delegation
            .as_ref()
            .ok_or("this Agent has no parent")?
            .parent_chat_id
    } else {
        Uuid::parse_str(target).map_err(|_| "use parent or a complete direct child Agent ID")?
    };
    let receiver = chats
        .get(&target_id)
        .ok_or("target Agent is unavailable; messages cannot load or restart history")?;
    if let Some(delegation) = &source.snapshot.delegation
        && delegation.parent_chat_id == target_id
    {
        return Ok((target_id, delegation.parent_loop_id, "child"));
    }
    if let Some(delegation) = &receiver.snapshot.delegation
        && delegation.parent_chat_id == sender.chat_id
    {
        let loop_id = receiver
            .snapshot
            .loops
            .first()
            .ok_or("child Loop is unavailable")?
            .id;
        return Ok((target_id, loop_id, "parent"));
    }
    Err("Unsupported: Agent messages are limited to a real direct parent and child".into())
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
    let text = snapshot
        .messages
        .iter()
        .enumerate()
        .rev()
        .find(|(index, message)| {
            message.role == Role::Assistant
                && snapshot.message_runs.get(index) == Some(&child.run_id)
        })
        .map(|(_, message)| {
            message
                .content
                .iter()
                .filter_map(|block| {
                    if let Content::Text { text } = block {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let metadata = snapshot.delegation.as_ref()?;
    Some(ToolResult {
        content: json!({"delegation_id":metadata.delegation_id,"child_chat_id":child.chat_id,
            "child_loop_id":child.loop_id,"child_run_id":child.run_id,"parent_chat_id":parent.chat_id,
            "status":status,"child_state":format!("{:?}", goal.state).to_lowercase(),"result":text,"error":error}).to_string(),
        is_error:status != "completed", output_ref:None,
    })
}

impl Agent {
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
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        match name {
            SEND_AGENT_MESSAGE_TOOL_NAME => {
                let message = input["message"]
                    .as_str()
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                    .ok_or("message cannot be empty")?;
                self.deliver_agent_message(
                    receipt,
                    call_id,
                    input["to"].as_str().ok_or("message target is missing")?,
                    message,
                    false,
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
                        receipt,
                        input["agent_id"]
                            .as_str()
                            .ok_or("child target is missing")?,
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
                    if let Some(result) = child_run_result(&snapshot, &child, receipt) {
                        return Ok(result);
                    }
                    if let Some(reason) = self.agent_wait_input(receipt).await {
                        self.observe_delegated_result(receipt.clone(), child.clone())
                            .await;
                        return Ok(result(
                            json!({"status":"running","child_chat_id":child.chat_id,"child_run_id":child.run_id,"wait_reason":reason,"required_action":(reason=="after_turn_queue").then_some("finish_current_turn")}),
                        ));
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return Err("child wait interrupted; child execution continues independently".into()),
                        _ = input_wakeup.changed()=>{},
                        _ = tokio::time::sleep_until(deadline) => {
                            self.observe_delegated_result(receipt.clone(), child.clone()).await;
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
        target: &str,
        message: &str,
        is_result: bool,
        cancel: &CancellationToken,
    ) -> Result<ToolResult, String> {
        let mut sent = self.inner.agent_messages.lock().await;
        let mut chats = self.inner.chats.lock().await;
        let (target_id, loop_id, relationship) = peer_target(&chats, sender, target)?;
        let key = (sender.run_id, call_id.to_owned());
        if let Some(existing) = sent.get(&key) {
            return if existing.target == target_id && existing.message == message {
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
        let sender_label = family_label(&chats[&sender.chat_id].snapshot, sender.loop_id);
        let original = chats.get_mut(&target_id).unwrap();
        let mut candidate = original.clone();
        let receiver = &mut candidate;
        let target_label = family_label(&receiver.snapshot, loop_id);
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
        let source = AgentMessageSource {
            message_id: Uuid::new_v4(),
            sender_chat_id: sender.chat_id,
            sender_loop_id: sender.loop_id,
            sender_run_id: sender.run_id,
            sender_relationship: relationship.into(),
            sender_label,
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
        let delivered = result(
            json!({"status":"accepted","message_id":source.message_id,"target_agent_id":target_id,
            "target_loop_id":loop_id,"target_run_id":receipt.run_id,"target_label":target_label,"target_relationship":if relationship=="parent" {"child"} else {"parent"},"delivery":"queued_at_tool_boundary"}),
        );
        sent.insert(
            key,
            SentMessage {
                target: target_id,
                message: message.into(),
                result: delivered.clone(),
            },
        );
        drop(chats);
        drop(sent);
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

    pub(super) async fn observe_delegated_result(&self, parent: Receipt, child: Receipt) {
        if !self
            .inner
            .delegation_notifications
            .lock()
            .await
            .insert(child.run_id)
        {
            return;
        }
        let agent = self.clone();
        let mut events = self.subscribe();
        let task = tokio::spawn(async move {
            loop {
                let Ok(snapshot) = agent.snapshot(child.chat_id).await else {
                    return;
                };
                if let Some(outcome) = child_run_result(&snapshot, &child, &parent) {
                    let _ = agent
                        .deliver_agent_message(
                            &child,
                            "delegation-result",
                            &parent.chat_id.to_string(),
                            &outcome.content,
                            true,
                            &agent.inner.shutdown,
                        )
                        .await;
                    return;
                }
                tokio::select! {
                    _ = agent.inner.shutdown.cancelled() => return,
                    event = events.recv() => if matches!(event, Err(broadcast::error::RecvError::Closed)) { return; },
                }
            }
        });
        self.inner.tasks.lock().await.push(task);
    }
}
