use std::{collections::BTreeSet, future::Future, pin::Pin};

use serde_json::{Value, json};
use tickloop_ids::IdContext;

use super::*;
use crate::{
    host_tools::HostToolProvider,
    tick_tools::{READ_WORKSPACE, TICK_CALL, TICK_MANAGE, TICK_READ, uses_herdr_layout},
};
use tickloop_harness_protocol::local_messages;
use tickloop_harness_protocol::{
    AgentColor, AgentMessageSource, DelegationCeiling, DelegationMetadata, FamilyMessageRejection,
    FamilyMessageRejectionCode, HostToolAuthority, Receipt, SUBAGENT_TOOL_NAME,
    SubagentRegistration, ToolDefinition, ToolResult,
};
use tickloop_harness_tools::{PreparedTool, Workspace, permissions::PermissionPolicyExt};

/// Records the Artifact account the host holds for every Artifact tool a ceiling lacks, so a child
/// keeps to that account even while the tool directory is unavailable.
fn record_artifact_account(
    host_tools: &mut BTreeMap<String, BTreeSet<HostToolAuthority>>,
    provider: &dyn HostToolProvider,
) -> Result<bool, String> {
    let names = crate::mcp_tools::ARTIFACT_ACCOUNT_TOOL_NAMES;
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

#[cfg(test)]
mod ancestry_authority_tests {
    use super::*;
    use tickloop_harness_test_support::{ScriptedModel, call, config, text};

    #[tokio::test]
    async fn parent_auto_review_is_not_bypassed_by_a_frozen_accept_edits_mode() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call("a", "Subagent", json!({"task":"report", "name":"a"})),
            text("child done"),
            text("root done"),
        ]));
        let core = Agent::new_embedded(config(), model);
        let root = core.create_chat(directory.path().into()).await.unwrap();
        core.set_execution_mode(root, ExecutionMode::AcceptEdits)
            .await
            .unwrap();
        let mut events = core.subscribe();
        let run = core.submit(root, "delegate".into()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.run_id == run.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let child = core.subagents(root).await.unwrap()[0].child_chat_id;
        core.set_execution_mode(root, ExecutionMode::Auto)
            .await
            .unwrap();
        {
            let chats = core.inner.chats.lock().await;
            let sessions = core.inner.session_permissions.lock().await;
            let receipt = Receipt {
                chat_id: child,
                loop_id: chats[&child].snapshot.chat_loop.id,
                run_id: Uuid::new_v4().into(),
            };
            let input = json!({"task":"delegate again"});
            let operation = OperationRecord {
                parent_call_id: None,
                parent_call_index: None,
                task_binding: None,
                id: Uuid::new_v4(),
                loop_id: receipt.loop_id,
                run_id: receipt.run_id,
                call_id: "nested".into(),
                name: SUBAGENT_TOOL_NAME.into(),
                input: input.clone(),
                file_change: None,
                state: OperationState::Proposed,
                result: None,
            };
            let (authority, _) = host_authority(
                &chats,
                &sessions,
                &receipt,
                &operation,
                &input,
                HostCallPermission {
                    mcp_server: None,
                    permission: Some(crate::host_tools::HostToolPermission::Classified {
                        allow_rule_bypasses_review: false,
                    }),
                    local_read: None,
                },
                None,
            )
            .unwrap();
            assert_eq!(authority.decision, Decision::Check);
        }
        core.shutdown().await;
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct SubagentInvocation {
    pub receipt: Receipt,
    pub run_in_background: bool,
}

pub const MAX_SUBAGENT_HANDLE_CHARACTERS: usize = 32;
const MAX_ACTIVE_SUBAGENTS: usize = 256;
const MAX_LABEL_INPUT_CHARACTERS: usize = 160;

pub(super) struct PreparedDelegation {
    pub input: Value,
    task: String,
    run_in_background: bool,
    requested_name: Option<String>,
    description: String,
}

impl PreparedDelegation {
    pub(super) fn parse(input: &Value, launch: SubagentLaunch) -> Result<Self, String> {
        let task = input["task"]
            .as_str()
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .ok_or("delegation task cannot be empty")?
            .to_owned();
        let run_in_background = match launch {
            SubagentLaunch::AlwaysBackground => true,
            SubagentLaunch::BackgroundByDefault => input
                .get("run_in_background")
                .map(|value| value.as_bool().ok_or("run_in_background must be a boolean"))
                .transpose()?
                .unwrap_or(true),
        };
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

/// The input of a `Subagent` call as `launch` offers the tool: a `run_in_background` the model
/// is not offered is dropped, so an older call or a habit of the model still runs.
pub(super) fn offered_input(launch: SubagentLaunch, mut input: Value) -> Value {
    if launch == SubagentLaunch::AlwaysBackground
        && let Some(fields) = input.as_object_mut()
    {
        fields.remove("run_in_background");
    }
    input
}

/// Whether a `Subagent` call with `input` runs in the background under `launch`; a value that
/// is no boolean fails the call when it is prepared.
pub(super) fn runs_in_background(launch: SubagentLaunch, input: &Value) -> bool {
    match launch {
        SubagentLaunch::AlwaysBackground => true,
        SubagentLaunch::BackgroundByDefault => input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    }
}

/// Claude Code's sentences on background Subagents under each launch.
fn background_guidance(launch: SubagentLaunch) -> &'static str {
    match launch {
        SubagentLaunch::AlwaysBackground => {
            "Subagents run in the background; you'll be notified when one completes. Never fabricate or predict a pending agent's results — the notification is never something you write yourself; if the user asks before it arrives, say it's still running."
        }
        SubagentLaunch::BackgroundByDefault => {
            "Subagents run in the background by default; you'll be notified when one completes. Pass `run_in_background: false` only when your very next action depends on the result and nothing else could usefully happen while it runs — otherwise background it so the user can interject. Never fabricate or predict a pending agent's results — the notification is never something you write yourself; if the user asks before it arrives, say it's still running."
        }
    }
}

pub(super) fn definitions(launch: SubagentLaunch) -> Vec<ToolDefinition> {
    let later_runs = match launch {
        SubagentLaunch::AlwaysBackground => {
            "Every later run of the child (a user follow-up or your SendMessage) also completes as a task notification."
        }
        SubagentLaunch::BackgroundByDefault => {
            "Interrupting a call that waits for the report does not stop the child; that run's completion then arrives as a task notification, as does every later run of the child (a user follow-up or your SendMessage)."
        }
    };
    let mut properties = json!({
        "task":{"type":"string","minLength":1},
        "name":{"type":"string","minLength":1,"description":"Short lowercase agent name, such as file-check; unique within this parent's children."},
        "description":{"type":"string","minLength":1,"description":"Short 3–5 word task description for the Subagents list; defaults to task."}
    });
    if launch == SubagentLaunch::BackgroundByDefault {
        properties["run_in_background"] = json!({"type":"boolean","default":true,"description":"false waits for the child's final report and returns it as this call's result."});
    }
    vec![ToolDefinition {
        name: SUBAGENT_TOOL_NAME.into(),
        description: format!(
            "Create one bounded independent child under your frozen resource and permission ceiling. {} Messages arrive automatically, so finish a turn after asking for clarification. Name and description identify the child separately from its task. The child has its own identity, Chat and approval/question requests. {later_runs} Bash follows the parent's frozen permission authority and normal approval checks; the working directory is not an OS sandbox. Delegation is limited to three levels below main; history rewrites are unavailable.",
            background_guidance(launch)
        ),
        input_schema: json!({"type":"object","properties":properties,"required":["task"],"additionalProperties":false}),
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
    let parents = super::family_tree::lineage(chats, receipt.chat_id)?;
    if std::iter::once(receipt.chat_id)
        .chain(parents.iter().map(|parent| parent.chat_id))
        .any(|id| chats[&id].family_stopping)
    {
        return Err("Subagent subtree is stopping".into());
    }
    if name == SUBAGENT_TOOL_NAME && parents.len() >= tickloop_harness_protocol::MAX_SUBAGENT_DEPTH
    {
        return Err("Subagent nesting limit reached (maximum depth 3)".into());
    }
    check_direct_call(chats, sessions, receipt, name, input, prepared, plan_file)?;
    for parent in parents.iter().rev() {
        check_direct_call(
            chats,
            sessions,
            &Receipt {
                chat_id: parent.chat_id,
                loop_id: parent.loop_id,
                run_id: receipt.run_id,
            },
            name,
            input,
            prepared,
            plan_file,
        )?;
    }
    Ok(())
}

fn check_direct_call(
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
            folder: ceiling.folder.clone(),
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
        let parent_owner = PermissionOwner::new(
            delegation.parent_chat_id.into(),
            delegation.parent_loop_id.into(),
        )
        .map_err(|error| error.to_string())?;
        let current_parent_decision = parent.snapshot.policy.explicit_decision_with_session(
            name,
            input,
            delegation.parent_loop_id.into(),
            sessions.evaluate(parent_owner, &session_operation).decision,
        );
        if current_parent_decision == Some(Decision::Deny) {
            return Err(
                "the parent's current denial still applies to this delegated operation".into(),
            );
        }
    }
    if ceiling.policy.explicit_decision_with_session(
        name,
        input,
        delegation.parent_loop_id.into(),
        None,
    ) == Some(Decision::Deny)
    {
        return Err("delegated authorization ceiling denies this operation".into());
    }
    if let Some(prepared) = prepared {
        let workspace = Workspace {
            folder: ceiling.folder.clone(),
            additional_directories: ceiling.additional_directories.clone(),
        };
        let is_plan_file_access = plan_file.is_some_and(|path| prepared.accesses_file(path));
        if !is_plan_file_access && !prepared.is_supported_in_delegated_scope() {
            return Err(
                "Unsupported: the operation is outside the delegated resource scope".into(),
            );
        }
        if !is_plan_file_access
            && ceiling
                .policy
                .decide(prepared, &workspace, delegation.parent_loop_id.into())
                == Decision::Deny
        {
            return Err("delegated authorization ceiling denies this operation".into());
        }
    }
    if matches!(name, READ_WORKSPACE | TICK_READ | TICK_CALL | TICK_MANAGE) {
        if name == TICK_MANAGE
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
        if ceiling.policy.mode == ExecutionMode::Plan && matches!(name, TICK_CALL | TICK_MANAGE) {
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
    /// Identity-bound policy revisions for every ancestor checked at admission.
    pub ancestor_revisions: Vec<(ChatId, u64)>,
}

pub(super) fn ancestor_revisions(
    chats: &HashMap<ChatId, Chat>,
    chat_id: ChatId,
) -> Result<Vec<(ChatId, u64)>, String> {
    Ok(super::family_tree::lineage(chats, chat_id)?
        .iter()
        .map(|parent| {
            (
                parent.chat_id,
                chats[&parent.chat_id].snapshot.policy.revision,
            )
        })
        .collect())
}

pub(super) fn effective_policy(
    chats: &HashMap<ChatId, Chat>,
    receipt: &Receipt,
) -> Result<tickloop_harness_protocol::PermissionPolicy, String> {
    fn stricter(left: ExecutionMode, right: ExecutionMode) -> ExecutionMode {
        match (left, right) {
            (ExecutionMode::Plan, _) | (_, ExecutionMode::Plan) => ExecutionMode::Plan,
            (ExecutionMode::AcceptEdits, _) | (_, ExecutionMode::AcceptEdits) => {
                ExecutionMode::AcceptEdits
            }
            (ExecutionMode::Auto, _) | (_, ExecutionMode::Auto) => ExecutionMode::Auto,
            _ => ExecutionMode::FullAccess,
        }
    }
    fn leaving_plan(policy: &tickloop_harness_protocol::PermissionPolicy) -> ExecutionMode {
        policy.mode.leaving_plan(policy.mode_before_plan)
    }
    let parents = super::family_tree::lineage(chats, receipt.chat_id)?;
    let mut policy = chats[&receipt.chat_id].snapshot.policy.clone();
    for id in std::iter::once(receipt.chat_id).chain(parents.iter().map(|parent| parent.chat_id)) {
        let Some(delegation) = &chats[&id].snapshot.delegation else {
            continue;
        };
        for ancestor_policy in [
            &delegation.ceiling.policy,
            &chats[&delegation.parent_chat_id].snapshot.policy,
        ] {
            // A combined Plan decides as the stricter of the modes each side's Plan would leave to.
            let mode = stricter(policy.mode, ancestor_policy.mode);
            policy.mode_before_plan = (mode == ExecutionMode::Plan)
                .then(|| stricter(leaving_plan(&policy), leaving_plan(ancestor_policy)));
            policy.mode = mode;
            policy.plan_without_auto |= ancestor_policy.plan_without_auto;
            for rules in [
                &ancestor_policy.rules,
                &ancestor_policy.launch_rules,
                &ancestor_policy.project_constraints,
                &ancestor_policy.managed_constraints,
            ] {
                policy.managed_constraints.extend(
                    rules
                        .iter()
                        .filter(|rule| {
                            matches!(rule.decision, Decision::Deny | Decision::Ask)
                                && rule
                                    .loop_id
                                    .is_none_or(|loop_id| loop_id == delegation.parent_loop_id)
                        })
                        .cloned()
                        .map(|mut rule| {
                            rule.loop_id = Some(receipt.loop_id.into());
                            rule
                        }),
                );
            }
        }
    }
    Ok(policy)
}

/// What a prepared host call contributes to its approval: the host's permission class, and the
/// local file it sends, whose `Read` approval the call also needs.
#[derive(Clone, Copy)]
pub(super) struct HostCallPermission<'a> {
    pub(super) mcp_server: Option<&'a str>,
    pub(super) permission: Option<crate::host_tools::HostToolPermission>,
    pub(super) local_read: Option<&'a PreparedTool>,
}

pub(super) fn host_authority(
    chats: &HashMap<ChatId, Chat>,
    sessions: &session_permissions::SessionPermissionContext,
    receipt: &Receipt,
    operation: &OperationRecord,
    permission_input: &Value,
    host_call: HostCallPermission<'_>,
    session: Option<tickloop_harness_tools::permissions::SessionPermissionDecision>,
) -> Result<
    (
        PermissionAuthority,
        tickloop_harness_protocol::PermissionPolicy,
    ),
    String,
> {
    let HostCallPermission {
        permission,
        local_read,
        mcp_server,
    } = host_call;
    let policy = effective_policy(chats, receipt)?;
    let decide = |policy: &tickloop_harness_protocol::PermissionPolicy, loop_id, session, human| {
        let explicit = match mcp_server {
            Some(server) => policy.explicit_mcp_rule_match(
                &operation.name,
                server,
                permission_input,
                loop_id,
                session,
            ),
            None => policy.explicit_rule_match(&operation.name, permission_input, loop_id, session),
        };
        (
            host_tool_decision(explicit, permission, policy, human),
            explicit.is_some_and(|matched| {
                matched.decision
                    == tickloop_harness_tools::permissions::ExplicitPermissionDecision::Ask
            }),
        )
    };
    let own = &chats[&receipt.chat_id];
    let (mut decision, mut explicit_ask) = decide(
        &own.snapshot.policy,
        receipt.loop_id.into(),
        session,
        permission == Some(crate::host_tools::HostToolPermission::PrivateArtifactWrite)
            && own.has_human_input_for_run(receipt.run_id),
    );
    let parents = super::family_tree::lineage(chats, receipt.chat_id)?;
    for id in std::iter::once(receipt.chat_id).chain(parents.iter().map(|parent| parent.chat_id)) {
        let Some(delegation) = &chats[&id].snapshot.delegation else {
            continue;
        };
        let parent = &chats[&delegation.parent_chat_id];
        let session_operation = SessionPermissionOperation::from_normalized(
            operation.name.clone(),
            permission_input.clone(),
            super::session_permissions::capture_workspace_scope(&configured_workspace_for_chat(
                parent,
            ))
            .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let owner = PermissionOwner::new(
            delegation.parent_chat_id.into(),
            delegation.parent_loop_id.into(),
        )
        .map_err(|error| error.to_string())?;
        let inherited_session = sessions.evaluate(owner, &session_operation).decision;
        for (ancestor_policy, session) in [
            (&delegation.ceiling.policy, None),
            (&parent.snapshot.policy, inherited_session),
        ] {
            let (ancestor_decision, ancestor_ask) = decide(
                ancestor_policy,
                delegation.parent_loop_id.into(),
                session,
                false,
            );
            decision = decision.stricter(ancestor_decision);
            explicit_ask |= ancestor_ask;
        }
    }
    // A call that sends a local file needs whatever approval a `Read` of that file would.
    if let Some(read) = local_read {
        let read_operation =
            SessionPermissionOperation::from_prepared(read, &configured_workspace_for_chat(own))
                .map_err(|error| error.to_string())?;
        let owner = PermissionOwner::new(receipt.chat_id.into(), receipt.loop_id.into())
            .map_err(|error| error.to_string())?;
        let read_session = sessions.evaluate(owner, &read_operation).decision;
        let read_authority =
            permission_authority(chats, sessions, receipt, read, read_session, None)
                .ok_or("chat is unavailable")?;
        decision = decision.stricter(read_authority.decision);
        explicit_ask |= read_authority.explicit_ask;
    }

    Ok((
        PermissionAuthority {
            decision,
            explicit_ask,
            policy_revision: chats[&receipt.chat_id].snapshot.policy.revision,
            ancestor_revisions: ancestor_revisions(chats, receipt.chat_id)?,
        },
        policy,
    ))
}

impl PermissionAuthority {
    /// Whether this re-evaluated authority still admits a call approved under the given
    /// revisions: nothing denies it, and once either policy changed it must allow outright.
    pub(super) fn still_admits(
        &self,
        approved_policy_revision: u64,
        approved_ancestor_revisions: &[(ChatId, u64)],
    ) -> bool {
        self.decision != Decision::Deny
            && (self.decision == Decision::Allow
                || self.policy_revision == approved_policy_revision
                    && self.ancestor_revisions == approved_ancestor_revisions)
    }
}

/// The permission decision for `prepared` in the receipt's Chat and Loop, or `None` when the
/// Chat is gone. A delegated child's Bash is further narrowed by its frozen ceiling and by its
/// ancestors' current policies.
pub(super) fn permission_authority(
    chats: &HashMap<ChatId, Chat>,
    sessions: &session_permissions::SessionPermissionContext,
    receipt: &Receipt,
    prepared: &PreparedTool,
    session: Option<tickloop_harness_tools::permissions::SessionPermissionDecision>,
    plan_file: Option<&std::path::Path>,
) -> Option<PermissionAuthority> {
    let parents = super::family_tree::lineage(chats, receipt.chat_id).ok()?;
    let mut authority =
        direct_permission_authority(chats, sessions, receipt, prepared, session, plan_file)?;
    authority.ancestor_revisions = parents
        .iter()
        .map(|parent| {
            (
                parent.chat_id,
                chats[&parent.chat_id].snapshot.policy.revision,
            )
        })
        .collect();
    for parent in parents.iter().rev() {
        let parent_receipt = Receipt {
            chat_id: parent.chat_id,
            loop_id: parent.loop_id,
            run_id: receipt.run_id,
        };
        let ancestor = direct_permission_authority(
            chats,
            sessions,
            &parent_receipt,
            prepared,
            session,
            plan_file,
        )?;
        authority.decision = authority.decision.stricter(ancestor.decision);
        authority.explicit_ask |= ancestor.explicit_ask;
    }
    Some(authority)
}

fn direct_permission_authority(
    chats: &HashMap<ChatId, Chat>,
    sessions: &session_permissions::SessionPermissionContext,
    receipt: &Receipt,
    prepared: &PreparedTool,
    session: Option<tickloop_harness_tools::permissions::SessionPermissionDecision>,
    plan_file: Option<&std::path::Path>,
) -> Option<PermissionAuthority> {
    let chat = chats.get(&receipt.chat_id)?;
    let policy = &chat.snapshot.policy;
    let own = PermissionAuthority {
        decision: policy.decide_with_plan_file(
            prepared,
            &configured_workspace_for_chat(chat),
            receipt.loop_id.into(),
            session,
            plan_file,
        ),
        explicit_ask: policy.explicit_match_with_session(
            &prepared.name,
            &prepared.input,
            receipt.loop_id.into(),
            session,
        ) == Some(
            tickloop_harness_tools::permissions::ExplicitPermissionDecision::Ask,
        ),
        policy_revision: policy.revision,
        ancestor_revisions: Vec::new(),
    };
    let Some(delegation) = chat
        .snapshot
        .delegation
        .as_ref()
        .filter(|_| prepared.name == tickloop_harness_tools::BASH_TOOL_NAME)
    else {
        return Some(own);
    };
    let workspace = Workspace {
        folder: delegation.ceiling.folder.clone(),
        additional_directories: delegation.ceiling.additional_directories.clone(),
    };
    let frozen = delegation.ceiling.policy.decide_with_plan_file(
        prepared,
        &workspace,
        delegation.parent_loop_id.into(),
        session,
        plan_file,
    );
    let frozen_ask = delegation.ceiling.policy.explicit_decision_with_session(
        &prepared.name,
        &prepared.input,
        delegation.parent_loop_id.into(),
        session,
    ) == Some(Decision::Ask);
    let parent = chats.get(&delegation.parent_chat_id);
    let (current, parent_ask, revision) = parent.map_or((Decision::Deny, false, None), |parent| {
        let Ok(owner) = PermissionOwner::new(
            delegation.parent_chat_id.into(),
            delegation.parent_loop_id.into(),
        ) else {
            return (Decision::Deny, false, Some(parent.snapshot.policy.revision));
        };
        let parent_workspace = configured_workspace_for_chat(parent);
        let Ok(operation) = SessionPermissionOperation::from_prepared(prepared, &parent_workspace)
        else {
            return (Decision::Deny, false, Some(parent.snapshot.policy.revision));
        };
        let parent_session = sessions.evaluate(owner, &operation).decision;
        let effective_session = if parent_session
            == Some(tickloop_harness_tools::permissions::SessionPermissionDecision::Deny)
        {
            parent_session
        } else {
            session
        };
        (
            parent.snapshot.policy.decide_with_plan_file(
                prepared,
                &parent_workspace,
                delegation.parent_loop_id.into(),
                effective_session,
                plan_file,
            ),
            parent.snapshot.policy.explicit_decision_with_session(
                &prepared.name,
                &prepared.input,
                delegation.parent_loop_id.into(),
                effective_session,
            ) == Some(Decision::Ask),
            Some(parent.snapshot.policy.revision),
        )
    });
    Some(PermissionAuthority {
        decision: own.decision.stricter(frozen).stricter(current),
        explicit_ask: own.explicit_ask || frozen_ask || parent_ask,
        ancestor_revisions: revision
            .map(|revision| vec![(delegation.parent_chat_id, revision)])
            .unwrap_or_default(),
        ..own
    })
}

impl Agent {
    pub async fn family_registrations(
        &self,
        member: ChatId,
    ) -> Result<Vec<SubagentRegistration>, AgentError> {
        let root = super::family_tree::root(&*self.inner.chats.lock().await, member)
            .map_err(AgentError::Invalid)?;
        let mut pending = vec![root];
        let mut visited = HashSet::new();
        let mut entries = Vec::new();
        while let Some(parent) = pending.pop() {
            if !visited.insert(parent) {
                return Err(AgentError::Invalid("cyclic family registry".into()));
            }
            let children = self.subagents(parent).await?;
            pending.extend(children.iter().map(|child| child.child_chat_id));
            entries.extend(children);
        }
        Ok(entries)
    }

    pub async fn subagents(&self, parent: ChatId) -> Result<Vec<SubagentRegistration>, AgentError> {
        let mut registry = self.inner.subagent_registry.lock().await;
        if let Some(entries) = registry.get(&parent) {
            return Ok(entries.clone());
        }
        let entries = match &self.inner.recorder {
            Some(recorder) => recorder.read_subagents(parent).await?,
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
        let mut chats = self.inner.chats.lock().await;
        let mut members = super::family_tree::lineage(&chats, chat_id)?
            .into_iter()
            .map(|parent| parent.chat_id)
            .collect::<Vec<_>>();
        members.push(chat_id);
        for member in members {
            if let Some(chat) = chats.get_mut(&member)
                && let Some(delegation) = chat.snapshot.delegation.as_mut()
                && record_artifact_account(&mut delegation.ceiling.host_tools, provider.as_ref())?
            {
                self.record_delegation(
                    member,
                    delegation.clone(),
                    chat.snapshot.search_config.clone(),
                );
            }
        }

        Ok(())
    }

    pub(super) async fn check_delegated_host_authority(
        &self,
        receipt: &Receipt,
        name: &str,
        authority: Option<HostToolAuthority>,
    ) -> Result<(), String> {
        let chats = self.inner.chats.lock().await;
        let parents = super::family_tree::lineage(&chats, receipt.chat_id)?;
        for id in std::iter::once(receipt.chat_id)
            .chain(parents.iter().rev().map(|parent| parent.chat_id))
        {
            let chat = chats
                .get(&id)
                .ok_or("ancestor authorization is unavailable")?;
            let Some(delegation) = &chat.snapshot.delegation else {
                continue;
            };
            let allowed = delegation
                .ceiling
                .host_tools
                .get(name)
                .ok_or("Unsupported: this tool is outside the inherited authorization")?;
            if !authority
                .as_ref()
                .is_some_and(|authority| allowed.contains(authority))
            {
                return Err("Unsupported: this tool endpoint or account is outside an ancestor's inherited authorization".into());
            }
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
                self.plans_directory(chat.snapshot.folder.as_deref()?)
                    .path(),
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

    /// A host call that sends a local file elsewhere answers to the rules of its `Read` of that
    /// file: the delegated scope and every ancestor's denials refuse it, and so does an explicit
    /// Deny the Chat is under. `Ok(true)` reports an explicit Ask, which the call's one approval
    /// then has to satisfy.
    pub(super) async fn check_local_read(
        &self,
        receipt: &Receipt,
        read: &PreparedTool,
    ) -> Result<bool, String> {
        self.check_delegated_call(receipt, &read.name, &read.input, Some(read))
            .await?;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
        let owner = PermissionOwner::new(receipt.chat_id.into(), receipt.loop_id.into())
            .map_err(|error| error.to_string())?;
        let operation =
            SessionPermissionOperation::from_prepared(read, &configured_workspace_for_chat(chat))
                .map_err(|error| error.to_string())?;
        let session = self
            .inner
            .session_permissions
            .lock()
            .await
            .evaluate(owner, &operation)
            .decision;
        match effective_policy(&chats, receipt)?.explicit_match_with_session(
            &read.name,
            &read.input,
            receipt.loop_id.into(),
            session,
        ) {
            Some(tickloop_harness_tools::permissions::ExplicitPermissionDecision::Deny) => {
                Err(format!(
                    "{} of {} is denied, so the file is not sent",
                    read.name,
                    read.input["file_path"].as_str().unwrap_or("this file")
                ))
            }
            Some(tickloop_harness_tools::permissions::ExplicitPermissionDecision::Ask) => Ok(true),
            _ => Ok(false),
        }
    }

    pub(super) fn subagent_tool<'a>(
        &'a self,
        parent: &'a Receipt,
        call_id: &'a str,
        prepared: &'a PreparedDelegation,
        feedback_account: Option<crate::host_tools::ArtifactAccountContext>,
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
                        .family_registrations(parent.chat_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    let (
                        snapshot,
                        bookkeeping,
                        client,
                        output_style,
                        user_context,
                        environment,
                        announced_date,
                        session_denials,
                        name,
                        ancestors,
                    ) = {
                        let chats = self.inner.chats.lock().await;
                        let chat = chats.get(&parent.chat_id).ok_or("parent is unavailable")?;
                        if registrations
                            .iter()
                            .filter(|member| {
                                chats.get(&member.child_chat_id).is_some_and(|chat| {
                                    chat.active_run.is_some() || !chat.parked.is_empty()
                                })
                            })
                            .count()
                            >= MAX_ACTIVE_SUBAGENTS
                        {
                            return Err("Concurrent Subagent limit reached (20 active family members); wait for a completion instead of retrying".into());
                        }
                        let mut ancestors = super::family_tree::lineage(&chats, parent.chat_id)?;
                        if chat.family_stopping
                            || ancestors
                                .iter()
                                .any(|ancestor| chats[&ancestor.chat_id].family_stopping)
                        {
                            return Err("Subagent subtree is stopping".into());
                        }
                        if ancestors.len() >= tickloop_harness_protocol::MAX_SUBAGENT_DEPTH {
                            return Err("Subagent nesting limit reached (maximum depth 3)".into());
                        }
                        if chat.active_run != Some(parent.run_id) {
                            return Err("delegating run is no longer active".into());
                        }
                        ancestors.push(tickloop_harness_protocol::DelegationParent {
                            chat_id: parent.chat_id,
                            loop_id: parent.loop_id,
                        });
                        let parent_owner =
                            PermissionOwner::new(parent.chat_id.into(), parent.loop_id.into())
                                .map_err(|error| error.to_string())?;
                        let sessions = self.inner.session_permissions.lock().await;
                        (
                            chat.snapshot.clone(),
                            chat.bookkeeping.clone(),
                            chat.client.clone(),
                            chat.output_style,
                            chat.user_context.clone(),
                            chat.environment.clone(),
                            chat.announced_date.clone(),
                            sessions.freeze_denials(parent_owner),
                            unique_agent_name(
                                requested_name.as_deref().unwrap_or(&fallback_name),
                                &registrations
                                    .iter()
                                    .map(|registration| registration.handle.clone())
                                    .collect::<Vec<_>>(),
                            ),
                            ancestors,
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
                        &mut policy.launch_rules,
                        &mut policy.project_constraints,
                        &mut policy.managed_constraints,
                    ] {
                        rules.retain(|rule| {
                            rule.loop_id.is_none() || rule.loop_id == Some(parent.loop_id.into())
                        });
                        for rule in rules {
                            rule.loop_id = None;
                            rule.persistent = false;
                        }
                    }
                    let folder = snapshot.folder.clone();
                    let mut host_context = self.host_call_context(parent, folder.as_deref());
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
                    if let Some(account) = &feedback_account {
                        let authority = HostToolAuthority::Mcp {
                            server_id: account.origin.clone(),
                            account_id: account.account_id.clone(),
                        };
                        for name in crate::mcp_tools::ARTIFACT_ACCOUNT_TOOL_NAMES {
                            host_tools.insert(name.to_owned(), BTreeSet::from([authority.clone()]));
                        }
                    }
                    if let Some(context) = &mut host_context {
                        context.interaction = None;
                    }
                    let mut registration = SubagentRegistration {
                        child_chat_id: ChatId::new(),
                        handle: name,
                        description: description.clone(),
                        created_at: tickloop_harness_base::private_fs::unix_time_milliseconds(),
                        color: None,
                    };
                    let mut trusted_user_inputs =
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
                    if let Some(inherited) = snapshot
                        .delegation
                        .as_ref()
                        .and_then(|delegation| delegation.trusted_user_inputs.as_ref())
                        .and_then(|value| {
                            serde_json::from_value::<
                                Vec<tickloop_harness_protocol::OriginalUserInput>,
                            >(value.clone())
                            .ok()
                        })
                    {
                        trusted_user_inputs
                            .get_or_insert_with(Vec::new)
                            .extend(inherited);
                    }
                    let history_ancestors = self.approval_history_ancestors(&snapshot, None).await;
                    let trusted_reply_history = trusted_user_inputs.as_ref().and_then(|inputs| {
                        serde_json::to_value(approval_context::capture_reply_history(
                            &snapshot,
                            &bookkeeping,
                            Some((parent.run_id.into(), call_id)),
                            inputs,
                            history_ancestors.as_deref(),
                        ))
                        .ok()
                    });
                    let metadata = DelegationMetadata {
                        delegation_id: tickloop_harness_protocol::DelegationId::new().uuid(),
                        parent_chat_id: parent.chat_id,
                        parent_loop_id: parent.loop_id,
                        parent_run_id: parent.run_id,
                        parent_call_id: call_id.to_owned(),
                        ancestors,
                        task: task.clone(),
                        trusted_user_inputs: trusted_user_inputs
                            .and_then(|inputs| serde_json::to_value(inputs).ok()),
                        trusted_reply_history,
                        trusted_goal: approval_context::delegated_goal(&snapshot),
                        ceiling: DelegationCeiling {
                            folder: folder.clone(),
                            additional_directories: snapshot.additional_directories.clone(),
                            policy,
                            session_denials: Some(session_denials),
                            host_context: host_context.clone(),
                            host_tools,
                            tick_instances: snapshot
                                .chat_loop
                                .subscriptions
                                .iter()
                                .map(|subscription| subscription.instance.clone())
                                .collect(),
                        },
                    };
                    let parent_registration = self
                        .subagent_registration(parent.chat_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    let sender_label =
                        agent_messages::family_label(&snapshot, parent_registration.as_ref());
                    let child_id = self
                        .create_chat_with_delegation(
                            registration.child_chat_id,
                            folder,
                            Some(super::InheritedChat {
                                additional_directory_sources: bookkeeping
                                    .additional_directory_sources
                                    .clone(),
                                metadata: metadata.clone(),
                                parent: snapshot,
                                client,
                                output_style,
                                user_context,
                                environment,
                                announced_date,
                            }),
                            Vec::new(),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    {
                        let used = self
                            .family_registrations(parent.chat_id)
                            .await
                            .map_err(|error| error.to_string())?
                            .iter()
                            .map(|entry| entry.color)
                            .collect::<Vec<_>>();
                        let mut registry = self.inner.subagent_registry.lock().await;
                        let entries = registry.entry(parent.chat_id).or_default();
                        registration.color = Some(AgentColor::next_for(&used));
                        entries.push(registration);
                        if let Some(recorder) = &self.inner.recorder {
                            recorder.record_subagents(parent.chat_id, entries.clone());
                        }
                    }
                    self.ensure_team_task_board(parent).await?;
                    if let Some(context) = host_context {
                        self.set_host_work_context(child_id, context)
                            .await
                            .map_err(|error| error.to_string())?;
                    }

                    let source = AgentMessageSource {
                        cause,
                        message_id: tickloop_harness_protocol::AgentMessageId::new().uuid(),
                        call_id: None,
                        created_at: tickloop_harness_base::private_fs::unix_time_milliseconds(),
                        sender_chat_id: parent.chat_id,
                        sender_loop_id: parent.loop_id,
                        sender_run_id: parent.run_id,
                        sender_relationship: "parent".into(),
                        is_result: false,
                        sender_label,
                        sender_handle: parent_registration
                            .as_ref()
                            .map_or("main", |registration| registration.handle.as_str())
                            .into(),
                        relay: None,
                    };
                    let child_ids = self.chat_ids(child_id).await;
                    let receipt = self
                        .submit_internal(
                            child_id,
                            local_messages::agent_message(&child_ids, &source, &task),
                            SubmissionOptions {
                                task_delegation: Some(parent.clone()),
                                agent_message: Some(source),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    // The child and its task are durable before anything of the parent tells
                    // of it, so a load finds every child its parent's records name.
                    self.make_durable(&[parent.chat_id, child_id]).await?;
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
                let ids = self.chat_ids(parent.chat_id).await;
                return Ok(launched_result(&ids, &child, &registration));
            }
            let release = self.suspend_for_subagent(parent, call_id, &child).await?;
            let movable = self.movable_call(parent.run_id, call_id);
            loop {
                if let Some(result) = self.recorded_child_result(parent.chat_id, &child).await? {
                    return Ok(result);
                }
                tokio::select! {
                    biased;
                    _ = release.cancelled() => return Err(super::suspension::RUN_RELEASED.into()),
                    _ = cancel.cancelled() => {
                        return self.end_interrupted_observation(parent, call_id, &child).await;
                    }
                    () = movable.moved() => {
                        return self.end_moved_observation(parent, call_id, &child).await;
                    }
                    event = events.recv() => match event {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {},
                        Err(broadcast::error::RecvError::Closed) => return Err("Agent event stream closed while observing child".into()),
                    }
                }
            }
        })
    }

    /// Ends an interrupted foreground observation. A child run that has already ended is still
    /// this call's result; otherwise the call stops returning that run, so its completion
    /// notifies the parent like background work. Deciding under the invocation lock that
    /// classifying the run's completion also holds leaves each run either returned or notified.
    pub(super) async fn end_interrupted_observation(
        &self,
        parent: &Receipt,
        call_id: &str,
        child: &Receipt,
    ) -> Result<ToolResult, String> {
        let ids = self.chat_ids(parent.chat_id).await;
        let mut invocations = self.inner.subagent_invocations.lock().await;
        if let Some(result) = self.recorded_child_result(parent.chat_id, child).await? {
            return Ok(result);
        }
        let registration = self
            .subagent_registration(child.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(invocation) = invocations.get_mut(&(parent.run_id, call_id.to_owned())) {
            invocation.run_in_background = true;
        }
        Ok(interrupted_observation_result(
            &ids,
            child,
            registration.as_ref(),
        ))
    }

    /// Ends a foreground observation that send now moved to the background: as an interrupted
    /// one does, except that the Run goes on, so the call's wait ends with what it returns.
    async fn end_moved_observation(
        &self,
        parent: &Receipt,
        call_id: &str,
        child: &Receipt,
    ) -> Result<ToolResult, String> {
        let ids = self.chat_ids(parent.chat_id).await;
        let mut invocations = self.inner.subagent_invocations.lock().await;
        if let Some(result) = self.recorded_child_result(parent.chat_id, child).await? {
            return Ok(result);
        }
        let registration = self
            .subagent_registration(child.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(invocation) = invocations.get_mut(&(parent.run_id, call_id.to_owned())) {
            invocation.run_in_background = true;
        }
        let result = moved_observation_result(&ids, child, registration.as_ref());
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&parent.chat_id)
            .ok_or("parent is unavailable")?;
        if let Some(wait_id) = chat
            .bookkeeping
            .lifecycle
            .open_subagent_wait(child.run_id)
            .map(|wait| wait.wait_id)
        {
            let tool =
                crate::tool_results::waited_call_result(chat, SUBAGENT_TOOL_NAME, result.clone());
            chat.bookkeeping.lifecycle.resolve_wait(
                wait_id,
                tickloop_harness_transcript::ModelVisibleResult {
                    content: tool.content.clone().into(),
                    tool,
                    ends_run: false,
                },
                None,
            );
            self.save(&chat.snapshot);
        }
        Ok(result)
    }
}

/// Claude Code's guidance in a launched background agent's result.
const LAUNCHED_MESSAGE: &str = "The agent is working in the background. You will be notified automatically when it completes. You know nothing about its results until that notification arrives — do not report, assume, or predict them; continue other work or respond to the user in the meantime. If the user asks for progress, say the agent is still running.";

/// What a background `Subagent` call returns: the child it launched.
pub(super) fn launched_result(
    ids: &IdContext,
    child: &Receipt,
    registration: &SubagentRegistration,
) -> ToolResult {
    ToolResult {
        content: json!({"status":"async_launched", "child_chat_id":ids.model_text(child.chat_id),
            "child_loop_id":ids.model_text(child.loop_id),"child_run_id":ids.model_text(child.run_id),
            "handle":registration.handle,"description":registration.description,
            "created_at":registration.created_at,"message":LAUNCHED_MESSAGE})
        .to_string(),
        is_error: false,
        output_ref: None,
    }
}

/// What a foreground `Subagent` call returns when send now moves its child to the background:
/// Claude Code's launched agent, with why it moved.
pub(super) fn moved_observation_result(
    ids: &IdContext,
    child: &Receipt,
    registration: Option<&SubagentRegistration>,
) -> ToolResult {
    ToolResult {
        content: json!({"status":"async_launched","child_chat_id":ids.model_text(child.chat_id),
            "child_loop_id":ids.model_text(child.loop_id),"child_run_id":ids.model_text(child.run_id),
            "handle":registration.map(|registration|registration.handle.as_str()),
            "description":registration.map(|registration|registration.description.as_str()),
            "created_at":registration.map(|registration|registration.created_at),
            "message":"Moved to the background so that a message that arrived while it was running can reach you; it was not interrupted and keeps running. Its completion arrives as a task notification; you know nothing of its results until then, so do not report or predict them."})
        .to_string(),
        is_error: false,
        output_ref: None,
    }
}

/// What a foreground `Subagent` call returns when it stops observing a child that goes on.
pub(super) fn interrupted_observation_result(
    ids: &IdContext,
    child: &Receipt,
    registration: Option<&SubagentRegistration>,
) -> ToolResult {
    ToolResult {
        content: json!({"status":"interrupted","child_chat_id":ids.model_text(child.chat_id),"handle":registration.map(|registration|registration.handle.as_str()),"description":registration.map(|registration|registration.description.as_str()),"created_at":registration.map(|registration|registration.created_at),"child_loop_id":ids.model_text(child.loop_id),"child_run_id":ids.model_text(child.run_id),"error":"Foreground observation interrupted; the child Agent remains independently active and its completion arrives as a task notification."}).to_string(),
        is_error: true,
        output_ref: None,
    }
}
