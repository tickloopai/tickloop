use std::{future::Future, pin::Pin};

use serde_json::{Value, json};

use super::*;
use crate::{
    host_tools::{HostCallContext, HostEndpointIdentity, HostToolProvider},
    model::ToolDefinition,
    tools::{PreparedTool, ToolResult, Workspace},
};

pub const DELEGATE_AGENT_TOOL_NAME: &str = "DelegateAgent";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DelegationCeiling {
    pub workdir: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub policy: PermissionPolicy,
    #[serde(default)]
    pub session_denials: Option<Vec<SessionPermissionDenial>>,
    pub host_context: Option<HostCallContext>,
    #[serde(default)]
    pub host_endpoint: Option<HostEndpointIdentity>,
    #[serde(default)]
    pub tick_instances: Vec<tick_connect::instance::InstanceRef>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DelegationMetadata {
    pub delegation_id: Uuid,
    pub parent_chat_id: ChatId,
    pub parent_loop_id: LoopId,
    pub parent_run_id: RunId,
    pub parent_call_id: String,
    pub task: String,
    pub ceiling: DelegationCeiling,
    #[serde(default)]
    pub(crate) trusted_user_inputs: Option<Value>,
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: DELEGATE_AGENT_TOOL_NAME.into(),
        description: "Delegate one bounded task to an independent child Agent and wait for its actual result. Incoming user context or child messages yield this wait while the child keeps working; use SendMessage to reply or steer it and WaitForAgent to observe its result. A yielded wait also delivers the eventual child result to your inbox. If required_action=finish_current_turn is returned, finish this turn to advance a pending after-turn human command instead of waiting again. The child inherits this task's working directories, host context and permission ceiling; follow-up input cannot expand them. The child has its own identity, Loop and approval/question requests. Interrupting this wait does not stop the child. Nested delegation, history rewrites and unrestricted Shell commands are unsupported. This creates a new task, not a Branch or a copy of your conversation.".into(),
        input_schema: json!({"type":"object","properties":{"task":{"type":"string","minLength":1}},"required":["task"],"additionalProperties":false}),
    }]
}

pub(super) fn check_call(
    chats: &HashMap<ChatId, Chat>,
    sessions: &session_permissions::SessionPermissionContext,
    receipt: &Receipt,
    name: &str,
    input: &Value,
    prepared: Option<&PreparedTool>,
) -> Result<(), String> {
    let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
    let Some(delegation) = &chat.snapshot.delegation else {
        return Ok(());
    };
    let ceiling = &delegation.ceiling;
    let denials = ceiling.session_denials.as_ref().ok_or("Unsupported: this delegated record has no frozen Session denial scope; its authorization cannot be safely continued")?;
    let session_operation = SessionPermissionOperation::from_normalized(
        name.to_owned(),
        input.clone(),
        WorkspaceScope::capture(&Workspace {
            workdir: ceiling.workdir.clone(),
            additional_directories: ceiling.additional_directories.clone(),
        })
        .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if denials
        .iter()
        .any(|denial| denial.matches(&session_operation))
    {
        return Err("the frozen parent Session denial applies to this delegated operation".into());
    }
    if let Some(parent) = chats.get(&delegation.parent_chat_id) {
        let parent_owner =
            PermissionOwner::new(delegation.parent_chat_id, delegation.parent_loop_id)
                .map_err(|error| error.to_string())?;
        let current_parent_decision = parent.snapshot.policy.explicit_decision_with_session(
            name,
            input,
            delegation.parent_loop_id,
            sessions.evaluate(parent_owner, &session_operation).decision,
        );
        if current_parent_decision == Some(Decision::Deny) {
            return Err(
                "the parent's current denial still applies to this delegated operation".into(),
            );
        }
        if current_parent_decision == Some(Decision::Ask)
            && chat.snapshot.policy.explicit_decision_with_session(
                name,
                input,
                receipt.loop_id,
                None,
            ) != Some(Decision::Ask)
        {
            return Err("Unsupported: the parent's current approval restriction is not represented in this child's frozen authorization; this operation cannot proceed".into());
        }
    }
    if name == DELEGATE_AGENT_TOOL_NAME {
        return Err("Unsupported: nested delegation is unavailable for this child".into());
    }
    if ceiling
        .policy
        .explicit_decision_with_session(name, input, receipt.loop_id, None)
        == Some(Decision::Deny)
    {
        return Err("delegated authorization ceiling denies this operation".into());
    }
    if let Some(prepared) = prepared {
        let workspace = Workspace {
            workdir: ceiling.workdir.clone(),
            additional_directories: ceiling.additional_directories.clone(),
        };
        if !prepared.is_supported_in_delegated_scope(&workspace) {
            return Err("Unsupported: the operation cannot be proven to stay within the delegated resource scope; unrestricted Shell is unavailable".into());
        }
        if ceiling.policy.decide(prepared, &workspace, receipt.loop_id) == Decision::Deny {
            return Err("delegated authorization ceiling denies this operation".into());
        }
    }
    if matches!(
        name,
        "ReadWorkspace" | "ReadTick" | "CallTickTool" | "ModifyTick" | "SendTickInput"
    ) {
        let workspace = ceiling
            .host_context
            .as_ref()
            .and_then(|context| context.workspace_id.as_deref());
        if workspace.is_none() || input.get("workspace_id").and_then(Value::as_str) != workspace {
            return Err("delegated host operation is outside the inherited Workspace scope".into());
        }
        if name == "SendTickInput"
            || name == "ModifyTick"
                && matches!(input["action"].as_str(), Some("open" | "send_input"))
        {
            return Err("Unsupported: delegated Agents cannot launch an unrestricted terminal program or submit terminal input".into());
        }
        if ceiling.policy.mode == ExecutionMode::Plan
            && matches!(name, "CallTickTool" | "ModifyTick" | "SendTickInput")
        {
            return Err("delegated Plan authorization does not admit host mutations".into());
        }
    }
    Ok(())
}

impl Agent {
    pub(super) async fn check_delegated_host_endpoint(
        &self,
        receipt: &Receipt,
        provider: &dyn HostToolProvider,
    ) -> Result<(), String> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
        let Some(delegation) = &chat.snapshot.delegation else {
            return Ok(());
        };
        let expected = delegation.ceiling.host_endpoint.as_ref().ok_or(
            "Unsupported: this delegated record has no verified host endpoint authorization",
        )?;
        if provider.host_endpoint()?.as_ref() != Some(expected) {
            return Err(
                "Unsupported: this host endpoint is outside the child's inherited authorization"
                    .into(),
            );
        }
        Ok(())
    }

    pub(super) async fn require_root_chat(
        &self,
        chat_id: ChatId,
        capability: &str,
    ) -> Result<(), AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        if chat.snapshot.delegation.is_some() {
            return Err(AgentError::Invalid(format!(
                "Unsupported: delegated Agents cannot {capability}; their authorization ceiling is fixed"
            )));
        }
        Ok(())
    }

    pub(super) async fn check_delegated_call(
        &self,
        receipt: &Receipt,
        name: &str,
        input: &Value,
        prepared: Option<&PreparedTool>,
    ) -> Result<(), String> {
        let chats = self.inner.chats.lock().await;
        let sessions = self.inner.session_permissions.lock().await;
        check_call(&chats, &sessions, receipt, name, input, prepared)
    }

    pub(super) async fn check_delegated_user_shell(
        &self,
        chat_id: ChatId,
        loop_id: Option<LoopId>,
        command: &str,
    ) -> Result<(), AgentError> {
        if self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .delegation
            .is_none()
        {
            return Ok(());
        }
        self.refresh_project_rules(chat_id).await?;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        let delegation = chat
            .snapshot
            .delegation
            .as_ref()
            .ok_or(AgentError::NotFound)?;
        let workspace = Workspace {
            workdir: delegation.ceiling.workdir.clone(),
            additional_directories: delegation.ceiling.additional_directories.clone(),
        };
        let prepared = crate::tools::prepare(
            &workspace,
            crate::tools::BASH_TOOL_NAME,
            &json!({"command":command,"cwd":workspace.workdir}),
        )
        .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let loop_id = loop_id
            .or_else(|| chat.snapshot.loops.first().map(|goal| goal.id))
            .ok_or(AgentError::NotFound)?;
        let receipt = Receipt {
            chat_id,
            loop_id,
            run_id: Uuid::nil(),
        };
        let sessions = self.inner.session_permissions.lock().await;
        check_call(
            &chats,
            &sessions,
            &receipt,
            &prepared.name,
            &prepared.input,
            Some(&prepared),
        )
        .map_err(AgentError::Invalid)?;
        if delegation
            .ceiling
            .policy
            .decide(&prepared, &workspace, loop_id)
            != Decision::Allow
            || chat.snapshot.policy.decide(&prepared, &workspace, loop_id) != Decision::Allow
        {
            return Err(AgentError::Invalid("Unsupported: direct Shell cannot bypass delegated action approval; use an admitted scoped operation".into()));
        }
        Ok(())
    }

    pub(super) fn delegation_tool<'a>(
        &'a self,
        parent: &'a Receipt,
        call_id: &'a str,
        input: &'a Value,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, String>> + Send + 'a>> {
        Box::pin(async move {
            let task = input["task"]
                .as_str()
                .map(str::trim)
                .filter(|task| !task.is_empty())
                .ok_or("delegation task cannot be empty")?
                .to_owned();
            let mut events = self.subscribe();
            let mut input_wakeup = self.inner.input_wakeup.subscribe();
            let child = {
                let mut delegations = self.inner.delegations.lock().await;
                let key = (parent.run_id, call_id.to_owned());
                if let Some(receipt) = delegations.get(&key) {
                    receipt.clone()
                } else {
                    if cancel.is_cancelled() {
                        return Err("delegation interrupted before child creation".into());
                    }
                    let (snapshot, client, output_style, session_denials) = {
                        let chats = self.inner.chats.lock().await;
                        let chat = chats.get(&parent.chat_id).ok_or("parent is unavailable")?;
                        if chat.snapshot.delegation.is_some() {
                            return Err("Unsupported: nested delegation is unavailable".into());
                        }
                        let parent_owner = PermissionOwner::new(parent.chat_id, parent.loop_id)
                            .map_err(|error| error.to_string())?;
                        let sessions = self.inner.session_permissions.lock().await;
                        (
                            chat.snapshot.clone(),
                            chat.client.clone(),
                            chat.output_style,
                            sessions.freeze_denials(parent_owner),
                        )
                    };
                    let mut policy = snapshot.policy.clone();
                    for rules in [
                        &mut policy.rules,
                        &mut policy.project_constraints,
                        &mut policy.managed_constraints,
                    ] {
                        rules.retain(|rule| {
                            rule.loop_id.is_none() || rule.loop_id == Some(parent.loop_id)
                        });
                        for rule in rules {
                            rule.loop_id = None;
                            rule.persistent = false;
                        }
                    }
                    let mut host_context = self.host_call_context(parent, &snapshot.workdir);
                    let host_endpoint = self
                        .inner
                        .host_tools
                        .read()
                        .unwrap()
                        .as_ref()
                        .map(|provider| provider.host_endpoint())
                        .transpose()?
                        .flatten();
                    if let Some(context) = &mut host_context {
                        context.interaction = None;
                    }
                    let metadata = DelegationMetadata {
                        delegation_id: Uuid::new_v4(),
                        parent_chat_id: parent.chat_id,
                        parent_loop_id: parent.loop_id,
                        parent_run_id: parent.run_id,
                        parent_call_id: call_id.to_owned(),
                        task: task.clone(),
                        trusted_user_inputs: approval_context::originals_for_active_chain(
                            &snapshot,
                        )
                        .ok()
                        .map(|inputs| {
                            inputs
                                .into_iter()
                                .filter(|input| {
                                    input.chat_id == parent.chat_id
                                        && input.loop_id == parent.loop_id
                                })
                                .collect::<Vec<_>>()
                        })
                        .and_then(|inputs| serde_json::to_value(inputs).ok()),
                        ceiling: DelegationCeiling {
                            workdir: snapshot.workdir.clone(),
                            additional_directories: snapshot.additional_directories.clone(),
                            policy,
                            session_denials: Some(session_denials),
                            host_context: host_context.clone(),
                            host_endpoint,
                            tick_instances: snapshot
                                .loops
                                .iter()
                                .filter_map(|goal| goal.trigger.as_ref())
                                .flat_map(|trigger| {
                                    trigger
                                        .subscriptions
                                        .iter()
                                        .map(|subscription| subscription.instance.clone())
                                })
                                .collect(),
                        },
                    };
                    let sender_label = agent_messages::family_label(&snapshot, parent.loop_id);
                    let child_id = self
                        .create_chat_with_delegation(
                            Uuid::new_v4(),
                            snapshot.workdir.clone(),
                            Some((metadata.clone(), snapshot, client, output_style)),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    if let Some(context) = host_context {
                        self.set_host_work_context(child_id, context)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    let source = AgentMessageSource {
                        message_id: Uuid::new_v4(),
                        sender_chat_id: parent.chat_id,
                        sender_loop_id: parent.loop_id,
                        sender_run_id: parent.run_id,
                        sender_relationship: "parent".into(),
                        is_result: false,
                        sender_label,
                    };
                    let receipt = self
                        .submit_internal(
                            child_id,
                            None,
                            local_messages::agent_message(&source, &task),
                            SubmissionOptions {
                                agent_message: Some(source),
                                goal_title: Some(task),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    delegations.insert(key, receipt.clone());
                    receipt
                }
            };
            loop {
                let snapshot = self
                    .snapshot(child.chat_id)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(result) = agent_messages::child_run_result(&snapshot, &child, parent) {
                    return Ok(result);
                }
                if let Some(reason) = self.agent_wait_input(parent).await {
                    self.observe_delegated_result(parent.clone(), child.clone())
                        .await;
                    return Ok(ToolResult {
                        content: json!({"status":"running","child_chat_id":child.chat_id,
                            "child_loop_id":child.loop_id,"child_run_id":child.run_id,
                            "wait_reason":reason,"required_action":(reason=="after_turn_queue").then_some("finish_current_turn")}).to_string(),
                        is_error:false, output_ref:None,
                    });
                }
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(ToolResult {
                        content:json!({"status":"wait_interrupted","child_chat_id":child.chat_id,"child_loop_id":child.loop_id,"child_run_id":child.run_id,"error":"Delegation wait interrupted; the child Agent remains independently active."}).to_string(),
                        is_error:true,output_ref:None,
                    }),
                    _ = input_wakeup.changed()=>{},
                    event = events.recv() => match event {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {},
                        Err(broadcast::error::RecvError::Closed) => return Err("Agent event stream closed while observing child".into()),
                    }
                }
            }
        })
    }
}
