use std::{collections::BTreeSet, future::Future, pin::Pin};

use serde_json::{Value, json};

use super::*;
use crate::{
    host_tools::HostToolProvider,
    tick_tools::{CALL_TICK_TOOL, MODIFY_TICK, READ_TICK, READ_WORKSPACE, uses_herdr_layout},
};
use tickloop_agent_protocol::local_messages;
use tickloop_agent_protocol::{
    AgentColor, AgentMessageSource, DelegationCeiling, DelegationMetadata, FamilyMessageRejection,
    FamilyMessageRejectionCode, HostToolAuthority, Receipt, SUBAGENT_TOOL_NAME,
    SubagentRegistration, ToolDefinition, ToolResult,
};
use tickloop_agent_tools::{PreparedTool, Workspace, permissions::PermissionPolicyExt};

/// Records the Artifact account the host holds for every Artifact tool a ceiling lacks, so a child
/// keeps to that account even while the tool directory is unavailable.
fn record_artifact_account(
    host_tools: &mut BTreeMap<String, BTreeSet<HostToolAuthority>>,
    provider: &dyn HostToolProvider,
) -> Result<bool, String> {
    let names = crate::mcp_tools::ARTIFACT_TOOL_NAMES;
    if names.iter().any(|name| host_tools.contains_key(*name)) {
        return Ok(false);
    }
    let authorities = provider.tool_authorities(names[0])?;
    if authorities.is_empty() {
        return Ok(false);
    }
    for name in names {
        host_tools.insert(name.to_owned(), authorities.clone());
    }
    Ok(true)
}

#[derive(Clone)]
pub(super) struct SubagentInvocation {
    pub receipt: Receipt,
    pub run_in_background: bool,
}

pub const MAX_SUBAGENT_HANDLE_CHARACTERS: usize = 32;
const MAX_LABEL_INPUT_CHARACTERS: usize = 160;

pub(super) struct PreparedDelegation {
    pub input: Value,
    task: String,
    run_in_background: bool,
    requested_name: Option<String>,
    description: String,
}

impl PreparedDelegation {
    pub(super) fn parse(input: &Value) -> Result<Self, String> {
        let task = input["task"]
            .as_str()
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .ok_or("delegation task cannot be empty")?
            .to_owned();
        let run_in_background = input
            .get("run_in_background")
            .map(|value| value.as_bool().ok_or("run_in_background must be a boolean"))
            .transpose()?
            .unwrap_or(false);
        let requested_name = optional_label(input, "name")?;
        let description =
            optional_label(input, "description")?.unwrap_or_else(|| compact_description(&task));
        let mut input = input.clone();
        input["task"] = json!(task);
        input["run_in_background"] = json!(run_in_background);
        input["description"] = json!(description);
        if let Some(name) = &requested_name {
            input["name"] = json!(name);
        }
        Ok(Self {
            input,
            task,
            run_in_background,
            requested_name,
            description,
        })
    }
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: SUBAGENT_TOOL_NAME.into(),
        description: "Create one bounded independent child under your frozen resource and permission ceiling. Foreground waits for its final report. Set run_in_background=true to return immediately; completion arrives automatically as a task notification. Messages arrive automatically, so finish a turn after asking for clarification. Name and description identify the child separately from its task. The child has its own identity, Loop and approval/question requests. Interrupting foreground observation does not stop the child. Bash follows the parent's frozen permission authority and normal approval checks; the working directory is not an OS sandbox. Nested delegation and history rewrites are unavailable.".into(),
        input_schema: json!({"type":"object","properties":{
            "task":{"type":"string","minLength":1},
            "run_in_background":{"type":"boolean","default":false,"description":"Return immediately and receive the final report as an automatic task notification."},
            "name":{"type":"string","minLength":1,"description":"Short lowercase agent name, such as file-check; unique within this parent's children."},
            "description":{"type":"string","minLength":1,"description":"Short 3–5 word task description for the Subagents list; defaults to task."}
        },"required":["task"],"additionalProperties":false}),
    }]
}

fn optional_label(input: &Value, field: &str) -> Result<Option<String>, String> {
    input.get(field).map(|value| {
        let original = value.as_str().ok_or_else(|| format!("delegation {field} must be text"))?;
        let label=original.trim();
        if label.is_empty() || original.chars().any(char::is_control) || label.chars().count() > MAX_LABEL_INPUT_CHARACTERS {
            return Err(format!("delegation {field} must contain 1–{MAX_LABEL_INPUT_CHARACTERS} printable characters"));
        }
        Ok(label.to_owned())
    }).transpose()
}

fn compact_description(task: &str) -> String {
    task.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_LABEL_INPUT_CHARACTERS)
        .collect()
}

fn agent_name_slug(label: &str) -> String {
    let mut slug = String::new();
    for character in label.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= MAX_SUBAGENT_HANDLE_CHARACTERS {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "agent".into()
    } else {
        slug.into()
    }
}

fn unique_agent_name(label: &str, used: &[String]) -> String {
    let base = agent_name_slug(label);
    let available = |name: &str| {
        !matches!(name, "main" | "parent")
            && !used.iter().any(|used| used.eq_ignore_ascii_case(name))
    };
    if available(&base) {
        return base;
    }
    for ordinal in 2.. {
        let suffix = format!("-{ordinal}");
        let prefix = &base[..base
            .len()
            .min(MAX_SUBAGENT_HANDLE_CHARACTERS.saturating_sub(suffix.len()))];
        let candidate = format!("{}{suffix}", prefix.trim_end_matches('-'));
        if available(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

pub(super) fn check_call(
    chats: &HashMap<ChatId, Chat>,
    sessions: &session_permissions::SessionPermissionContext,
    receipt: &Receipt,
    name: &str,
    input: &Value,
    prepared: Option<&PreparedTool>,
    plan_file: Option<&std::path::Path>,
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
        super::session_permissions::capture_workspace_scope(&Workspace {
            workdir: ceiling.workdir.clone(),
            additional_directories: ceiling.additional_directories.clone(),
        })
        .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if denials
        .iter()
        .any(|denial| super::session_permissions::denial_matches(denial, &session_operation))
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
        if name != tickloop_agent_tools::BASH_TOOL_NAME
            && current_parent_decision == Some(Decision::Ask)
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
    if name == SUBAGENT_TOOL_NAME {
        return Err("Unsupported: nested delegation is unavailable for this child".into());
    }
    if ceiling
        .policy
        .explicit_decision_with_session(name, input, delegation.parent_loop_id, None)
        == Some(Decision::Deny)
    {
        return Err("delegated authorization ceiling denies this operation".into());
    }
    if let Some(prepared) = prepared {
        let workspace = Workspace {
            workdir: ceiling.workdir.clone(),
            additional_directories: ceiling.additional_directories.clone(),
        };
        let is_plan_file_access = plan_file.is_some_and(|path| prepared.accesses_file(path))
            && (prepared.is_read_only || chat.snapshot.policy.mode == ExecutionMode::Plan);
        if !is_plan_file_access && !prepared.is_supported_in_delegated_scope() {
            return Err(
                "Unsupported: the operation is outside the delegated resource scope".into(),
            );
        }
        if !is_plan_file_access
            && ceiling
                .policy
                .decide(prepared, &workspace, delegation.parent_loop_id)
                == Decision::Deny
        {
            return Err("delegated authorization ceiling denies this operation".into());
        }
    }
    if matches!(
        name,
        READ_WORKSPACE | READ_TICK | CALL_TICK_TOOL | MODIFY_TICK
    ) {
        if name == MODIFY_TICK
            && matches!(
                input["action"].as_str(),
                Some("open" | "reopen" | "send_input")
            )
        {
            return Err(
                "Unsupported: delegated Agents cannot launch a program or submit terminal input"
                    .into(),
            );
        }
        // A Tick belongs to the Chat that opened it, which the daemon enforces on every call;
        // Herdr Panes stay scoped by the inherited Workspace.
        if uses_herdr_layout(name, input) {
            let workspace = ceiling
                .host_context
                .as_ref()
                .and_then(|context| context.workspace_id.as_deref());
            if workspace.is_none() || input.get("workspace_id").and_then(Value::as_str) != workspace
            {
                return Err(
                    "delegated host operation is outside the inherited Workspace scope".into(),
                );
            }
        }
        if ceiling.policy.mode == ExecutionMode::Plan
            && matches!(name, CALL_TICK_TOOL | MODIFY_TICK)
        {
            return Err("delegated Plan authorization does not admit host mutations".into());
        }
    }
    Ok(())
}

/// A call's permission decision and the policy revisions it was made under.
pub(super) struct PermissionAuthority {
    pub decision: Decision,
    pub explicit_ask: bool,
    /// The Chat's own policy revision.
    pub policy_revision: u64,
    /// The parent policy revision a delegated child's Bash was decided under; `None` for other
    /// calls, and when the parent is gone, which decides Deny.
    pub parent_revision: Option<u64>,
}

impl PermissionAuthority {
    /// Whether this re-evaluated authority still admits a call approved under the given
    /// revisions: nothing denies it, and once either policy changed it must allow outright.
    pub(super) fn still_admits(
        &self,
        approved_policy_revision: u64,
        approved_parent_revision: Option<u64>,
    ) -> bool {
        self.decision != Decision::Deny
            && (self.decision == Decision::Allow
                || self.policy_revision == approved_policy_revision
                    && self.parent_revision == approved_parent_revision)
    }
}

/// The permission decision for `prepared` in the receipt's Chat and Loop, or `None` when the
/// Chat is gone. A delegated child's Bash is further narrowed by its frozen ceiling and by its
/// parent's current policy.
pub(super) fn permission_authority(
    chats: &HashMap<ChatId, Chat>,
    sessions: &session_permissions::SessionPermissionContext,
    receipt: &Receipt,
    prepared: &PreparedTool,
    session: Option<tickloop_agent_tools::permissions::SessionPermissionDecision>,
    plan_file: Option<&std::path::Path>,
) -> Option<PermissionAuthority> {
    let chat = chats.get(&receipt.chat_id)?;
    let policy = &chat.snapshot.policy;
    let own = PermissionAuthority {
        decision: policy.decide_with_plan_file(
            prepared,
            &configured_workspace_for_chat(chat),
            receipt.loop_id,
            session,
            plan_file,
        ),
        explicit_ask: policy.explicit_match_with_session(
            &prepared.name,
            &prepared.input,
            receipt.loop_id,
            session,
        ) == Some(tickloop_agent_tools::permissions::ExplicitPermissionDecision::Ask),
        policy_revision: policy.revision,
        parent_revision: None,
    };
    let Some(delegation) = chat
        .snapshot
        .delegation
        .as_ref()
        .filter(|_| prepared.name == tickloop_agent_tools::BASH_TOOL_NAME)
    else {
        return Some(own);
    };
    let workspace = Workspace {
        workdir: delegation.ceiling.workdir.clone(),
        additional_directories: delegation.ceiling.additional_directories.clone(),
    };
    let frozen = delegation.ceiling.policy.decide_with_plan_file(
        prepared,
        &workspace,
        delegation.parent_loop_id,
        session,
        plan_file,
    );
    let frozen_ask = delegation.ceiling.policy.explicit_decision_with_session(
        &prepared.name,
        &prepared.input,
        delegation.parent_loop_id,
        session,
    ) == Some(Decision::Ask);
    let parent = chats.get(&delegation.parent_chat_id);
    let (current, parent_ask, revision) = parent.map_or((Decision::Deny, false, None), |parent| {
        let Ok(owner) = PermissionOwner::new(delegation.parent_chat_id, delegation.parent_loop_id)
        else {
            return (Decision::Deny, false, Some(parent.snapshot.policy.revision));
        };
        let parent_workspace = configured_workspace_for_chat(parent);
        let Ok(operation) = SessionPermissionOperation::from_prepared(prepared, &parent_workspace)
        else {
            return (Decision::Deny, false, Some(parent.snapshot.policy.revision));
        };
        let parent_session = sessions.evaluate(owner, &operation).decision;
        let effective_session = if parent_session
            == Some(tickloop_agent_tools::permissions::SessionPermissionDecision::Deny)
        {
            parent_session
        } else {
            session
        };
        (
            parent.snapshot.policy.decide_with_plan_file(
                prepared,
                &parent_workspace,
                delegation.parent_loop_id,
                effective_session,
                plan_file,
            ),
            parent.snapshot.policy.explicit_decision_with_session(
                &prepared.name,
                &prepared.input,
                delegation.parent_loop_id,
                effective_session,
            ) == Some(Decision::Ask),
            Some(parent.snapshot.policy.revision),
        )
    });
    Some(PermissionAuthority {
        decision: own.decision.stricter(frozen).stricter(current),
        explicit_ask: own.explicit_ask || frozen_ask || parent_ask,
        parent_revision: revision,
        ..own
    })
}

impl Agent {
    pub async fn subagents(&self, parent: ChatId) -> Result<Vec<SubagentRegistration>, AgentError> {
        let mut registry = self.inner.subagent_registry.lock().await;
        if let Some(entries) = registry.get(&parent) {
            return Ok(entries.clone());
        }
        let entries = match &self.inner.recorder {
            Some(recorder) => recorder.read_subagents(parent)?,
            None => return Err(AgentError::NotFound),
        };
        registry.insert(parent, entries.clone());
        Ok(entries)
    }

    pub async fn subagent_registration(
        &self,
        child: ChatId,
    ) -> Result<Option<SubagentRegistration>, AgentError> {
        let parent = self
            .inner
            .chats
            .lock()
            .await
            .get(&child)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .delegation
            .as_ref()
            .map(|metadata| metadata.parent_chat_id);
        let Some(parent) = parent else {
            return Ok(None);
        };
        self.subagents(parent)
            .await?
            .into_iter()
            .find(|registration| registration.child_chat_id == child)
            .map(Some)
            .ok_or_else(|| {
                AgentError::Invalid("Subagent is missing its immutable family registration".into())
            })
    }

    /// Checks that a delegated call runs under an authority its child inherited for `name`: that
    /// of the backend this very input routes to, as that backend currently is.
    pub(super) async fn check_delegated_host_call(
        &self,
        receipt: &Receipt,
        provider: &dyn HostToolProvider,
        name: &str,
        input: &Value,
    ) -> Result<(), String> {
        if self
            .inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .ok_or("chat is unavailable")?
            .snapshot
            .delegation
            .is_none()
        {
            return Ok(());
        }
        self.check_delegated_host_authority(receipt, name, provider.call_authority(name, input)?)
            .await
    }

    /// A child delegated while no Artifact account was signed in takes up the first account the
    /// host signs in to afterwards; the ceiling records it, so a later account switch stays out.
    pub(super) async fn pin_delegated_artifact_account(
        &self,
        chat_id: ChatId,
    ) -> Result<(), String> {
        let Some(provider) = self.inner.host_tools.read().unwrap().clone() else {
            return Ok(());
        };
        let delegation = {
            let mut chats = self.inner.chats.lock().await;
            let Some(delegation) = chats
                .get_mut(&chat_id)
                .and_then(|chat| chat.snapshot.delegation.as_mut())
            else {
                return Ok(());
            };
            if !record_artifact_account(&mut delegation.ceiling.host_tools, provider.as_ref())? {
                return Ok(());
            }
            delegation.clone()
        };
        self.record_delegation(chat_id, delegation);
        Ok(())
    }

    pub(super) async fn check_delegated_host_authority(
        &self,
        receipt: &Receipt,
        name: &str,
        authority: Option<HostToolAuthority>,
    ) -> Result<(), String> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
        let Some(delegation) = &chat.snapshot.delegation else {
            return Ok(());
        };
        let allowed = delegation
            .ceiling
            .host_tools
            .get(name)
            .ok_or("Unsupported: this tool is outside the child's inherited authorization")?;
        if !authority.is_some_and(|authority| allowed.contains(&authority)) {
            return Err("Unsupported: this tool endpoint or account is outside the child's inherited authorization".into());
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
        let plan_file = chats.get(&receipt.chat_id).and_then(|chat| {
            super::plan::current_plan_file(
                &chat.bookkeeping,
                self.plans_directory(&chat.snapshot.workdir).path(),
                receipt.loop_id,
            )
        });
        check_call(
            &chats,
            &sessions,
            receipt,
            name,
            input,
            prepared,
            plan_file.as_deref(),
        )
    }

    pub(super) fn subagent_tool<'a>(
        &'a self,
        parent: &'a Receipt,
        call_id: &'a str,
        prepared: &'a PreparedDelegation,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, String>> + Send + 'a>> {
        Box::pin(async move {
            let task = prepared.task.clone();
            let run_in_background = prepared.run_in_background;
            let requested_name = &prepared.requested_name;
            let description = prepared.description.clone();
            let fallback_name = description
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
            let mut events = self.subscribe();
            let invocation = {
                let mut invocations = self.inner.subagent_invocations.lock().await;
                let key = (parent.run_id, call_id.to_owned());
                if let Some(invocation) = invocations.get(&key) {
                    invocation.clone()
                } else {
                    if cancel.is_cancelled() {
                        return Err("delegation interrupted before child creation".into());
                    }
                    let registrations = self
                        .subagents(parent.chat_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    let (snapshot, bookkeeping, client, output_style, session_denials, name) = {
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
                            chat.bookkeeping.clone(),
                            chat.client.clone(),
                            chat.output_style,
                            sessions.freeze_denials(parent_owner),
                            unique_agent_name(
                                requested_name.as_deref().unwrap_or(&fallback_name),
                                &registrations
                                    .iter()
                                    .map(|registration| registration.handle.clone())
                                    .collect::<Vec<_>>(),
                            ),
                        )
                    };
                    let mut cause = family_mailbox::current_cause(&snapshot, &bookkeeping, parent)
                        .map_err(|rejection| rejection.message)?;
                    cause.depth = cause.depth.saturating_add(1);
                    if cause.depth > family_mailbox::MAX_MESSAGE_DEPTH {
                        return Ok(FamilyMessageRejection {
                            code: FamilyMessageRejectionCode::DepthLimit,
                            message: "Subagent causal depth exhausted; do not retry automatically"
                                .into(),
                        }
                        .result());
                    }
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
                    let mut host_tools: BTreeMap<_, _> = self
                        .inner
                        .host_tools
                        .read()
                        .unwrap()
                        .as_ref()
                        .map(|provider| {
                            provider
                                .definitions()
                                .into_iter()
                                .map(|tool| {
                                    let authorities = provider.tool_authorities(&tool.name)?;
                                    Ok((!authorities.is_empty())
                                        .then_some((tool.name, authorities)))
                                })
                                .collect::<Result<Vec<_>, String>>()
                        })
                        .transpose()?
                        .unwrap_or_default()
                        .into_iter()
                        .flatten()
                        .collect();
                    if let Some(provider) = self.inner.host_tools.read().unwrap().as_ref() {
                        record_artifact_account(&mut host_tools, provider.as_ref())?;
                    }
                    if let Some(context) = &mut host_context {
                        context.interaction = None;
                    }
                    let mut registration = SubagentRegistration {
                        child_chat_id: Uuid::new_v4(),
                        handle: name,
                        description: description.clone(),
                        created_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
                        color: None,
                    };
                    let trusted_user_inputs =
                        approval_context::originals_for_active_chain(&snapshot, &bookkeeping)
                            .ok()
                            .map(|inputs| {
                                inputs
                                    .into_iter()
                                    .filter(|input| {
                                        input.chat_id == parent.chat_id
                                            && input.loop_id == parent.loop_id
                                    })
                                    .collect::<Vec<_>>()
                            });
                    let ancestors = self.approval_history_ancestors(&snapshot).await;
                    let trusted_reply_history = trusted_user_inputs.as_ref().and_then(|inputs| {
                        serde_json::to_value(approval_context::capture_reply_history(
                            &snapshot,
                            &bookkeeping,
                            Some((parent.run_id, call_id)),
                            inputs,
                            ancestors.as_deref(),
                        ))
                        .ok()
                    });
                    let metadata = DelegationMetadata {
                        delegation_id: Uuid::new_v4(),
                        parent_chat_id: parent.chat_id,
                        parent_loop_id: parent.loop_id,
                        parent_run_id: parent.run_id,
                        parent_call_id: call_id.to_owned(),
                        task: task.clone(),
                        trusted_user_inputs: trusted_user_inputs
                            .and_then(|inputs| serde_json::to_value(inputs).ok()),
                        trusted_reply_history,
                        ceiling: DelegationCeiling {
                            workdir: snapshot.workdir.clone(),
                            additional_directories: snapshot.additional_directories.clone(),
                            policy,
                            session_denials: Some(session_denials),
                            host_context: host_context.clone(),
                            host_tools,
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
                    let sender_label = agent_messages::family_label(&snapshot, None);
                    let child_id = self
                        .create_chat_with_delegation(
                            registration.child_chat_id,
                            snapshot.workdir.clone(),
                            Some((metadata.clone(), snapshot, client, output_style)),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    {
                        let mut registry = self.inner.subagent_registry.lock().await;
                        let entries = registry.entry(parent.chat_id).or_default();
                        let used = entries.iter().map(|entry| entry.color).collect::<Vec<_>>();
                        registration.color = Some(AgentColor::next_for(&used));
                        entries.push(registration);
                        if let Some(recorder) = &self.inner.recorder {
                            recorder.record_subagents(parent.chat_id, entries.clone());
                        }
                    }
                    if let Some(context) = host_context {
                        self.set_host_work_context(child_id, context)
                            .await
                            .map_err(|error| error.to_string())?;
                    }

                    let source = AgentMessageSource {
                        cause,
                        message_id: Uuid::new_v4(),
                        call_id: None,
                        created_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
                        sender_chat_id: parent.chat_id,
                        sender_loop_id: parent.loop_id,
                        sender_run_id: parent.run_id,
                        sender_relationship: "parent".into(),
                        is_result: false,
                        sender_label,
                        sender_handle: "main".into(),
                        relay: None,
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
                    let invocation = SubagentInvocation {
                        receipt,
                        run_in_background,
                    };
                    invocations.insert(key, invocation.clone());
                    invocation
                }
            };
            let child = invocation.receipt;
            if invocation.run_in_background {
                let registration = self
                    .subagent_registration(child.chat_id)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or("child handle is unavailable")?;
                return Ok(ToolResult {
                    content: json!({"status":"async_launched", "child_chat_id":child.chat_id,
                        "child_loop_id":child.loop_id,"child_run_id":child.run_id,
                        "handle":registration.handle,"description":registration.description,
                        "created_at":registration.created_at})
                    .to_string(),
                    is_error: false,
                    output_ref: None,
                });
            }
            loop {
                let snapshot = self
                    .snapshot(child.chat_id)
                    .await
                    .map_err(|error| error.to_string())?;
                let registration = self
                    .subagent_registration(child.chat_id)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(result) = agent_messages::child_run_result(
                    &snapshot,
                    &child,
                    parent,
                    registration.as_ref(),
                ) {
                    return Ok(result);
                }
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(ToolResult {
                        content:json!({"status":"interrupted","child_chat_id":child.chat_id,"handle":registration.as_ref().map(|registration|registration.handle.as_str()),"description":registration.as_ref().map(|registration|registration.description.as_str()),"created_at":registration.as_ref().map(|registration|registration.created_at),"child_loop_id":child.loop_id,"child_run_id":child.run_id,"error":"Foreground observation interrupted; the child Agent remains independently active."}).to_string(),
                        is_error:true,output_ref:None,
                    }),
                    event = events.recv() => match event {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {},
                        Err(broadcast::error::RecvError::Closed) => return Err("Agent event stream closed while observing child".into()),
                    }
                }
            }
        })
    }
}
