use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::*;
use tickloop_agent_protocol::{
    ApprovalOutcome, ApprovalPath, ApprovalReviewerKind, AutoBlockKind, DenialCategory,
    DenialRecord, EventKind, HostToolAuthority, OperationRecord, OperationState,
    PermissionApprovalSource, PromptPolicy, Receipt, SEND_AGENT_MESSAGE_TOOL_NAME, ToolDefinition,
    ToolResult,
};
use tickloop_agent_tools::{
    ASK_USER_QUESTION_TOOL_NAME, ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME,
    permissions::PermissionPolicyExt,
};

struct AutoPermissionDecision {
    decision: Decision,
    approval_reason: Option<String>,
    denial_message: Option<String>,
}

#[derive(Clone)]
enum ToolRoute {
    BuiltIn,
    Plan,
    Interaction,
    Skill,
    TickSubscription,
    Delegation,
    AgentMessage,
    Handback,
    Host(Arc<dyn crate::host_tools::HostToolProvider>),
}

enum PreparedLocalCall {
    Family(agent_messages::PreparedFamilyCall),
    Delegation(delegation::PreparedDelegation),
    Handback(crate::handback::PreparedHandback),
}

impl PreparedLocalCall {
    fn policy_input(&self) -> &Value {
        match self {
            Self::Family(call) => call.policy_input(),
            Self::Delegation(call) => &call.input,
            Self::Handback(call) => &call.input,
        }
    }
}

#[derive(Clone)]
pub(super) struct RegisteredTool {
    definition: ToolDefinition,
    route: ToolRoute,
}

impl RegisteredTool {
    fn host_provider(&self) -> Option<Arc<dyn crate::host_tools::HostToolProvider>> {
        match &self.route {
            ToolRoute::Host(provider) => Some(provider.clone()),
            _ => None,
        }
    }

    fn is_host_or_local(&self) -> bool {
        !matches!(self.route, ToolRoute::BuiltIn)
    }
}

pub(super) struct ToolRegistry {
    tools: Vec<RegisteredTool>,
}

impl ToolRegistry {
    pub(super) fn for_chat(
        chat: &Chat,
        host_provider: Option<Arc<dyn crate::host_tools::HostToolProvider>>,
        tick_stream_installed: bool,
        prompt_policy: PromptPolicy,
    ) -> Self {
        let is_delegated = chat.snapshot.delegation.is_some();
        let mut tools = tickloop_agent_tools::Tools::definitions()
            .into_iter()
            .map(|definition| RegisteredTool {
                definition,
                route: ToolRoute::BuiltIn,
            })
            .collect::<Vec<_>>();
        tools.extend(
            plan::definitions(is_delegated)
                .into_iter()
                .map(|definition| RegisteredTool {
                    definition,
                    route: ToolRoute::Plan,
                }),
        );
        tools.extend(
            interaction::definitions(is_delegated)
                .into_iter()
                .map(|definition| RegisteredTool {
                    definition,
                    route: ToolRoute::Interaction,
                }),
        );
        if !is_delegated {
            tools.extend(
                delegation::definitions()
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::Delegation,
                    }),
            );
        }
        tools.extend(
            agent_messages::definitions(is_delegated)
                .into_iter()
                .map(|definition| RegisteredTool {
                    definition,
                    route: ToolRoute::AgentMessage,
                }),
        );
        if crate::handback::available(chat, prompt_policy) {
            tools.push(RegisteredTool {
                definition: crate::handback::definition(),
                route: ToolRoute::Handback,
            });
        }
        if tick_stream_installed {
            tools.extend(
                tick_subscriptions::definitions()
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::TickSubscription,
                    }),
            );
        }
        tools.extend(
            chat.skills
                .tool_definition()
                .into_iter()
                .map(|definition| RegisteredTool {
                    definition,
                    route: ToolRoute::Skill,
                }),
        );
        if let Some(provider) = host_provider {
            tools.extend(
                provider
                    .definitions()
                    .into_iter()
                    .filter(|definition| {
                        chat.snapshot.delegation.as_ref().is_none_or(|delegation| {
                            delegation.ceiling.host_tools.contains_key(&definition.name)
                        })
                    })
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::Host(provider.clone()),
                    }),
            );
        }
        Self { tools }
    }

    pub(super) fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub(super) fn host_tool_names(&self) -> std::collections::BTreeSet<String> {
        self.tools
            .iter()
            .filter(|tool| matches!(tool.route, ToolRoute::Host(_)))
            .map(|tool| tool.definition.name.clone())
            .collect()
    }

    pub(super) fn resolve(&self, name: &str) -> Option<RegisteredTool> {
        self.tools
            .iter()
            .find(|tool| tool.definition.name == name)
            .cloned()
    }
}

fn validate_tool_input(definition: &ToolDefinition, input: &Value) -> Result<(), String> {
    validate_schema(&definition.input_schema, input, "input")
        .map_err(|reason| format!("invalid {} input: {reason}", definition.name))
}

pub(crate) fn validate_schema(schema: &Value, value: &Value, location: &str) -> Result<(), String> {
    let fields = schema
        .as_object()
        .ok_or_else(|| format!("{location} uses an unsupported tool schema"))?;
    for field in fields.keys() {
        if !matches!(
            field.as_str(),
            "const"
                | "enum"
                | "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "minItems"
                | "maxItems"
                | "items"
                | "minLength"
                | "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
                | "format"
                | "default"
                | "description"
        ) {
            return Err(format!(
                "{location} uses unsupported tool schema field {field}"
            ));
        }
    }
    if let Some(expected) = schema.get("const")
        && value != expected
    {
        return Err(format!("{location} has an invalid value"));
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array)
        && !choices.contains(value)
    {
        return Err(format!("{location} has an invalid value"));
    }
    if let Some(types) = schema.get("type") {
        let matches = |name: &str| match name {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        };
        let valid = types.as_str().is_some_and(matches)
            || types
                .as_array()
                .is_some_and(|types| types.iter().filter_map(Value::as_str).any(matches));
        if !valid {
            return Err(format!("{location} has an invalid type"));
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(field) {
                    return Err(format!("{location}.{field} is required"));
                }
            }
        }
        for (field, child) in object {
            let child_schema = properties.and_then(|properties| properties.get(field));
            match (child_schema, schema.get("additionalProperties")) {
                (Some(child_schema), _) => {
                    validate_schema(child_schema, child, &format!("{location}.{field}"))?;
                }
                (None, Some(Value::Bool(false))) => {
                    return Err(format!("{location}.{field} is not allowed"));
                }
                (None, Some(additional_schema)) if additional_schema.is_object() => {
                    validate_schema(additional_schema, child, &format!("{location}.{field}"))?;
                }
                (None, _) => {}
            }
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
            && items.len() < minimum as usize
        {
            return Err(format!("{location} has too few items"));
        }
        if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64)
            && items.len() > maximum as usize
        {
            return Err(format!("{location} has too many items"));
        }
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in items.iter().enumerate() {
                validate_schema(item_schema, item, &format!("{location}[{index}]"))?;
            }
        }
    }
    if let Some(text) = value.as_str()
        && let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
        && text.chars().count() < minimum as usize
    {
        return Err(format!("{location} is too short"));
    }
    if let Some(format) = schema.get("format").and_then(Value::as_str)
        && (format != "uuid"
            || value
                .as_str()
                .is_none_or(|text| Uuid::parse_str(text).is_err()))
    {
        return Err(format!("{location} has an invalid format"));
    }
    if let Some(number) = value.as_f64() {
        for (key, valid) in [
            (
                "minimum",
                schema
                    .get("minimum")
                    .and_then(Value::as_f64)
                    .is_none_or(|minimum| number >= minimum),
            ),
            (
                "maximum",
                schema
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .is_none_or(|maximum| number <= maximum),
            ),
            (
                "exclusiveMinimum",
                schema
                    .get("exclusiveMinimum")
                    .and_then(Value::as_f64)
                    .is_none_or(|minimum| number > minimum),
            ),
            (
                "exclusiveMaximum",
                schema
                    .get("exclusiveMaximum")
                    .and_then(Value::as_f64)
                    .is_none_or(|maximum| number < maximum),
            ),
        ] {
            if !valid {
                return Err(format!("{location} violates {key}"));
            }
        }
    }
    Ok(())
}

pub(super) struct ToolCallOutcome {
    pub(super) result: ToolResult,
    /// Images for the model only, sent after the result text.
    pub(super) images: Vec<tickloop_agent_protocol::Image>,
    pub(super) host_call_status: Option<crate::host_tools::HostCallStatus>,
    pub(super) host_effect: Option<crate::host_tools::HostEffect>,
    pub(super) permission_comment: Option<String>,
    pub(super) continue_after_denial: bool,
    pub(super) is_rejected_by_user: bool,
    pub(super) denial: Option<DenialRecord>,
}

impl ToolCallOutcome {
    fn not_admitted(message: String) -> Self {
        Self {
            result: ToolResult {
                content: message,
                is_error: true,
                output_ref: None,
            },
            images: Vec::new(),
            host_call_status: None,
            host_effect: None,
            permission_comment: None,
            continue_after_denial: false,
            is_rejected_by_user: false,
            denial: None,
        }
    }
}

pub(super) struct ToolCallError {
    pub(super) message: String,
    pub(super) denial: Option<DenialRecord>,
}

impl From<String> for ToolCallError {
    fn from(message: String) -> Self {
        Self {
            message,
            denial: None,
        }
    }
}

impl From<&str> for ToolCallError {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}

impl ToolCallError {
    fn denied(message: String, denial: DenialRecord) -> Self {
        Self {
            message,
            denial: Some(denial),
        }
    }
}

pub(super) struct ToolInvocation {
    id: String,
    name: String,
    input: Value,
    registration: Option<RegisteredTool>,
}

impl ToolInvocation {
    pub(super) fn new(
        id: String,
        name: String,
        input: Value,
        registration: Option<RegisteredTool>,
    ) -> Self {
        Self {
            id,
            name,
            input,
            registration,
        }
    }
}

impl Agent {
    async fn dispatch_registered_tool(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        registered: &RegisteredTool,
        prepared_local: Option<&PreparedLocalCall>,
        authority: Option<HostToolAuthority>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        match &registered.route {
            ToolRoute::AgentMessage => {
                self.agent_message_tool(
                    receipt,
                    &operation.call_id,
                    &operation.name,
                    &operation.input,
                    prepared_local.and_then(|call| match call {
                        PreparedLocalCall::Family(call) => Some(call),
                        _ => None,
                    }),
                    cancel,
                )
                .await
            }
            ToolRoute::Handback => {
                let Some(PreparedLocalCall::Handback(prepared)) = prepared_local else {
                    return Err("SubagentHandback was not prepared".into());
                };
                self.execute_handback(receipt, operation, prepared, cancel)
                    .await
            }
            ToolRoute::Delegation => {
                let Some(PreparedLocalCall::Delegation(prepared)) = prepared_local else {
                    return Err("Subagent was not prepared".into());
                };
                self.subagent_tool(receipt, &operation.call_id, prepared, cancel)
                    .await
            }
            ToolRoute::Skill => self.skill_tool(receipt, &operation.input).await,
            ToolRoute::TickSubscription => {
                self.tick_subscription_tool(receipt, &operation.name, &operation.input, authority)
                    .await
            }
            ToolRoute::Host(provider) => Ok(provider
                .execute(
                    receipt.chat_id,
                    &operation.name,
                    operation.input.clone(),
                    cancel,
                )
                .await),
            ToolRoute::Interaction => self
                .interaction_tool(
                    receipt,
                    &operation.call_id,
                    &operation.name,
                    &operation.input,
                    cancel,
                )
                .await
                .expect("registered interaction tool has a handler"),
            ToolRoute::Plan => self
                .control_tool(receipt, operation, cancel)
                .await
                .expect("registered plan tool has a handler"),
            ToolRoute::BuiltIn => unreachable!("built-in tools execute typed operations"),
        }
    }

    pub(super) async fn execute_registered_call(
        &self,
        receipt: &Receipt,
        assistant_message_id: Uuid,
        invocation: ToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolCallOutcome, ToolCallError> {
        let ToolInvocation {
            id,
            name,
            input,
            registration,
        } = invocation;
        if cancel.is_cancelled() {
            return Err("run interrupted before tool preparation".into());
        }
        if let Some(registered) = &registration
            && registered.is_host_or_local()
            && let Err(error) = match &registered.route {
                ToolRoute::Host(provider) => provider.validate_input(
                    &registered.definition.name,
                    &registered.definition.input_schema,
                    &input,
                ),
                _ => validate_tool_input(&registered.definition, &input),
            }
        {
            return Ok(ToolCallOutcome {
                result: ToolResult {
                    content: error,
                    is_error: true,
                    output_ref: None,
                },
                images: Vec::new(),
                host_call_status: None,
                host_effect: None,
                permission_comment: None,
                continue_after_denial: false,
                is_rejected_by_user: false,
                denial: None,
            });
        }
        self.refresh_project_rules(receipt.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut permission_comment = None;
        let mut continue_after_denial = false;
        let mut is_rejected_by_user = false;
        let mut was_approved_by_human = false;
        let mut auto_denial_message = None;
        let mut deterministic_denial_reason = None;
        let mut denial: Option<DenialRecord> = None;
        let mut images = Vec::new();
        let (configured_workspace, workspace) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).unwrap();
            let configured_workspace = configured_workspace_for_chat(chat);
            let workspace = configured_workspace.clone();
            (configured_workspace, workspace)
        };
        let permission_owner = PermissionOwner::new(receipt.chat_id, receipt.loop_id)
            .map_err(|error| error.to_string())?;
        let host_tools = registration
            .as_ref()
            .and_then(|registered| registered.host_provider());
        let event_context = self
            .inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .and_then(|chat| chat.active_event_claims.get(&receipt.run_id))
            .map(|active| crate::host_tools::EventToolContext {
                origin: active.source.origin.clone(),
                account_id: active.source.account_id.clone(),
                artifact_id: active.source.artifact_id.clone(),
                binding_id: active.source.binding_id,
                epoch: active.source.epoch,
                request_id: active.claim.request_id.clone(),
                claim_id: active.claim.claim_id.clone(),
                fencing_token: active.claim.fencing_token.clone(),
            });
        let prepared_host_call = if let Some(provider) = host_tools.as_ref() {
            if let Err(error) = self
                .check_delegated_host_call(receipt, provider.as_ref(), &name, &input)
                .await
            {
                return Ok(ToolCallOutcome::not_admitted(error));
            }
            let context = self.host_call_context(receipt, &workspace.workdir);
            let prepared = if let Some(context) = context {
                provider
                    .prepare_call_in_context(
                        receipt.chat_id,
                        &context,
                        &name,
                        &input,
                        cancel.clone(),
                    )
                    .await
            } else {
                provider
                    .prepare_call(receipt.chat_id, &name, &input, cancel.clone())
                    .await
            };
            match prepared {
                Ok(prepared) => {
                    if prepared.name() != name || prepared.source_input() != &input {
                        return Err("host provider prepared a different tool invocation".into());
                    }
                    if let Err(error) = self
                        .check_delegated_host_call(
                            receipt,
                            provider.as_ref(),
                            prepared.name(),
                            prepared.source_input(),
                        )
                        .await
                    {
                        return Ok(ToolCallOutcome::not_admitted(error));
                    }
                    Some(match event_context {
                        Some(context) => prepared.with_event_context(context),
                        None => prepared,
                    })
                }
                Err(error) => {
                    return Ok(ToolCallOutcome {
                        result: ToolResult {
                            content: error,
                            is_error: true,
                            output_ref: None,
                        },
                        images: Vec::new(),
                        host_call_status: None,
                        host_effect: None,
                        permission_comment: None,
                        continue_after_denial: false,
                        is_rejected_by_user: false,
                        denial: None,
                    });
                }
            }
        } else if matches!(
            registration.as_ref().map(|registered| &registered.route),
            Some(ToolRoute::TickSubscription)
        ) && name == "SubscribeTick"
        {
            let instance = match tick_subscriptions::subscription_request(&input) {
                Ok((instance, _)) => instance,
                Err(error) => return Ok(ToolCallOutcome::not_admitted(error)),
            };
            let port = self.tick_deliveries().port.clone();
            match port {
                Some(port) => match port
                    .prepare_watch(receipt.chat_id, &instance, &input, cancel.clone())
                    .await
                {
                    Ok(Some(watch)) => {
                        if let Some(binding) = watch.host_authority()?
                            && let Err(error) = self
                                .check_delegated_host_authority(
                                    receipt,
                                    &binding.tool_name,
                                    binding.authority,
                                )
                                .await
                        {
                            return Ok(ToolCallOutcome::not_admitted(error));
                        }
                        Some(watch.into_admission(input.clone()))
                    }
                    Ok(None) => None,
                    Err(error) => return Ok(ToolCallOutcome::not_admitted(error)),
                },
                None => None,
            }
        } else {
            None
        };
        let prepared_local = if matches!(
            name.as_str(),
            SEND_AGENT_MESSAGE_TOOL_NAME | agent_messages::STOP_SUBAGENT_TOOL_NAME
        ) && matches!(
            registration.as_ref().map(|registered| &registered.route),
            Some(ToolRoute::AgentMessage)
        ) {
            match self.prepare_family_call(receipt, &name, &input).await {
                Ok(prepared) => Some(PreparedLocalCall::Family(prepared)),
                Err(rejection) => {
                    return Ok(ToolCallOutcome {
                        result: rejection.result(),
                        ..ToolCallOutcome::not_admitted(String::new())
                    });
                }
            }
        } else if matches!(
            registration.as_ref().map(|registered| &registered.route),
            Some(ToolRoute::Delegation)
        ) {
            match delegation::PreparedDelegation::parse(&input) {
                Ok(prepared) => Some(PreparedLocalCall::Delegation(prepared)),
                Err(error) => return Ok(ToolCallOutcome::not_admitted(error)),
            }
        } else if matches!(
            registration.as_ref().map(|registered| &registered.route),
            Some(ToolRoute::Handback)
        ) {
            match self.prepare_handback(receipt, &input).await {
                Ok(prepared) => Some(PreparedLocalCall::Handback(prepared)),
                Err(error) => return Ok(ToolCallOutcome::not_admitted(error)),
            }
        } else {
            None
        };
        let host_permission = prepared_host_call
            .as_ref()
            .map(|call| call.permission())
            .or_else(|| {
                let route = &registration.as_ref()?.route;
                (matches!(route, ToolRoute::Delegation)
                    || matches!(route, ToolRoute::AgentMessage)
                        && name == SEND_AGENT_MESSAGE_TOOL_NAME)
                    .then_some(crate::host_tools::HostToolPermission::Classified {
                        allow_rule_bypasses_review: name == SEND_AGENT_MESSAGE_TOOL_NAME,
                    })
            });
        let normalized_name = prepared_host_call
            .as_ref()
            .map_or(name.as_str(), |call| call.policy_name());
        let normalized_input = prepared_local
            .as_ref()
            .map(|prepared| prepared.policy_input())
            .unwrap_or_else(|| {
                prepared_host_call
                    .as_ref()
                    .map_or(&input, |call| call.policy_input())
            });
        let permission_input = prepared_local
            .as_ref()
            .map(|prepared| prepared.policy_input())
            .unwrap_or_else(|| {
                prepared_host_call
                    .as_ref()
                    .map_or(&input, |call| call.permission_input())
            });
        if let Err(error) = self
            .check_delegated_call(receipt, normalized_name, normalized_input, None)
            .await
        {
            return Ok(ToolCallOutcome::not_admitted(error));
        }
        let mut host_call_status = None;
        let mut host_effect = None;
        let result = if registration
            .as_ref()
            .is_some_and(|registered| registered.is_host_or_local())
        {
            let session_operation = SessionPermissionOperation::from_normalized(
                normalized_name.to_owned(),
                permission_input.clone(),
                super::session_permissions::capture_workspace_scope(&configured_workspace)
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            let operation = OperationRecord {
                id: Uuid::new_v4(),
                loop_id: receipt.loop_id,
                run_id: receipt.run_id,
                call_id: id.clone(),
                name: normalized_name.to_owned(),
                input: normalized_input.clone(),
                file_change: None,
                state: OperationState::Proposed,
                result: None,
            };
            let (
                decision,
                explicit_ask,
                mut revision,
                mut session_evaluation,
                execution_mode,
                policy,
            ) = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&receipt.chat_id).unwrap();
                if cancel.is_cancelled() {
                    return Err("run interrupted before host operation".into());
                }
                chat.snapshot.operations.push(operation.clone());
                self.save(&chat.snapshot);
                let session_evaluation = self
                    .inner
                    .session_permissions
                    .lock()
                    .await
                    .evaluate(permission_owner, &session_operation);
                let explicit_decision = chat.snapshot.policy.explicit_match_with_session(
                    normalized_name,
                    permission_input,
                    receipt.loop_id,
                    session_evaluation.decision,
                );
                (
                    host_tool_decision(
                        explicit_decision.map(Decision::from),
                        host_permission,
                        chat.snapshot.policy.mode,
                        host_permission
                            == Some(crate::host_tools::HostToolPermission::PrivateArtifactWrite)
                            && chat.has_human_input_for_run(receipt.run_id),
                    ),
                    explicit_decision
                        == Some(tickloop_agent_tools::permissions::ExplicitPermissionDecision::Ask),
                    chat.snapshot.policy.revision,
                    session_evaluation,
                    chat.snapshot.policy.mode,
                    chat.snapshot.policy.clone(),
                )
            };
            // The user confirms every request to enter Plan mode, whatever the execution mode;
            // only a Deny rule or a host without prompts refuses it without asking.
            let requires_plan_mode_consent =
                name == ENTER_PLAN_MODE_TOOL_NAME && execution_mode != ExecutionMode::Plan;
            let decision = match decision {
                Decision::Deny => Decision::Deny,
                _ if name == EXIT_PLAN_MODE_TOOL_NAME => Decision::Allow,
                _ if requires_plan_mode_consent => Decision::Ask,
                Decision::Ask
                    if execution_mode == ExecutionMode::Auto
                        && !explicit_ask
                        && name != ASK_USER_QUESTION_TOOL_NAME
                        && host_permission
                            != Some(crate::host_tools::HostToolPermission::Confirmation) =>
                {
                    Decision::Allow
                }
                other => other,
            };
            let prompts_unavailable = decision == Decision::Ask
                && self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt;
            let mut decision = self.apply_prompt_policy_with_explicit_ask(
                decision,
                explicit_ask
                    || host_permission == Some(crate::host_tools::HostToolPermission::Confirmation),
            );
            let mut assessment_details = None;
            let mut approval_source = if explicit_ask {
                PermissionApprovalSource::ExplicitAsk
            } else {
                PermissionApprovalSource::PermissionPolicy
            };
            let mut approval_reason = Some(format!(
                "{}: {}",
                if explicit_ask {
                    "an explicit Ask rule requires human approval"
                } else if requires_plan_mode_consent {
                    "entering plan mode requires the user's confirmation"
                } else {
                    "the permission policy requires human approval"
                },
                prepared_host_call
                    .as_ref()
                    .map_or("", |call| call.approval_summary()),
            ));
            let mut auto_denial_message = None;
            if decision == Decision::Check {
                let mut assessment = self
                    .review_auto_operation(
                        receipt,
                        &operation,
                        prepared_host_call
                            .as_ref()
                            .map(|call| call.approval_facts().clone()),
                        cancel.clone(),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                {
                    let mut chats = self.inner.chats.lock().await;
                    let chat = chats.get_mut(&receipt.chat_id).unwrap();
                    self.emit(
                        chat,
                        receipt,
                        EventKind::ApprovalAssessed {
                            assessment: Box::new(assessment.details.clone()),
                        },
                    );
                }
                let reviewed = self
                    .resolve_auto_assessment(receipt, &operation, &policy, &mut assessment)
                    .await?;
                assessment_details = Some(assessment.details.clone());
                decision = reviewed.decision;
                approval_source = PermissionApprovalSource::AutoReviewer;
                approval_reason = reviewed.approval_reason;
                auto_denial_message = reviewed.denial_message;
            }
            let allowed = match decision {
                Decision::Allow => true,
                Decision::Deny => false,
                _ => {
                    let answer = self
                        .request_permission(
                            receipt,
                            &operation,
                            session_operation.clone(),
                            approval_source,
                            approval_reason,
                            cancel.clone(),
                        )
                        .await?;
                    revision = answer.revision;
                    permission_comment = answer.comment;
                    is_rejected_by_user = !answer.allow;
                    was_approved_by_human = answer.allow;
                    continue_after_denial = is_rejected_by_user && permission_comment.is_some();
                    session_evaluation = self
                        .inner
                        .session_permissions
                        .lock()
                        .await
                        .evaluate(permission_owner, &session_operation);
                    answer.allow
                }
            };
            if allowed && let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id) {
                chat.auto_denials.record_success();
            }
            if !allowed {
                let reason = if let Some(reason) = auto_denial_message {
                    reason
                } else if prompts_unavailable {
                    auto_mode::prompts_unavailable_message(&name)
                } else if is_rejected_by_user && name == ENTER_PLAN_MODE_TOOL_NAME {
                    plan::enter_plan_mode_declined(permission_comment.as_deref())
                } else {
                    permission_denial_message(permission_comment.as_deref())
                };
                let category = if is_rejected_by_user {
                    DenialCategory::Human
                } else if let Some(details) = &assessment_details {
                    if details.outcome == ApprovalOutcome::Block {
                        DenialCategory::Automatic
                    } else {
                        DenialCategory::NoHuman
                    }
                } else if prompts_unavailable {
                    DenialCategory::NoHuman
                } else if execution_mode == ExecutionMode::Plan {
                    DenialCategory::Plan
                } else {
                    DenialCategory::Rule
                };
                denial = Some(
                    super::denials::DenialCandidate::new(
                        &operation,
                        permission_input,
                        category,
                        (explicit_ask || is_rejected_by_user || assessment_details.is_some())
                            .then_some(approval_source),
                        &policy,
                        assessment_details.as_ref(),
                    )
                    .finish(receipt.chat_id, &operation),
                );
                Err(reason)
            } else {
                let final_approval_source = (was_approved_by_human || assessment_details.is_some())
                    .then_some(approval_source);
                let validation = if let (Some(provider), Some(prepared)) =
                    (host_tools.as_ref(), prepared_host_call.as_ref())
                {
                    match self
                        .check_delegated_host_call(
                            receipt,
                            provider.as_ref(),
                            prepared.name(),
                            prepared.source_input(),
                        )
                        .await
                    {
                        Ok(()) => {
                            provider
                                .revalidate_call(receipt.chat_id, prepared, cancel.clone())
                                .await
                        }
                        Err(error) => Err(error),
                    }
                } else if let Some((prepared, watch)) =
                    prepared_host_call.as_ref().and_then(|prepared| {
                        prepared
                            .payload::<crate::tick_stream::PreparedTickWatch>()
                            .map(|watch| (prepared, watch))
                    })
                {
                    let authority_check = match watch.host_authority()? {
                        Some(binding) => {
                            self.check_delegated_host_authority(
                                receipt,
                                &binding.tool_name,
                                binding.authority,
                            )
                            .await
                        }
                        None => Ok(()),
                    };
                    match authority_check {
                        Ok(()) => {
                            crate::tick_stream::PreparedTickWatch::revalidate(
                                receipt.chat_id,
                                prepared,
                                cancel.clone(),
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    Ok(())
                };
                match validation {
                    Err(error) => {
                        if !cancel.is_cancelled() {
                            denial = Some(
                                super::denials::DenialCandidate::new(
                                    &operation,
                                    permission_input,
                                    DenialCategory::FinalAdmission,
                                    final_approval_source,
                                    &policy,
                                    assessment_details.as_ref(),
                                )
                                .finish(receipt.chat_id, &operation),
                            );
                        }
                        Err(error)
                    }
                    Ok(()) => {
                        if self.refresh_project_rules(receipt.chat_id).await.is_err() {
                            if cancel.is_cancelled() {
                                return Err("run interrupted before host admission".into());
                            }
                            let reason =
                                "project permission rules are unavailable before final admission"
                                    .to_owned();
                            let record = super::denials::DenialCandidate::new(
                                &operation,
                                permission_input,
                                DenialCategory::FinalAdmission,
                                final_approval_source,
                                &policy,
                                assessment_details.as_ref(),
                            )
                            .project_rules_unavailable()
                            .finish(receipt.chat_id, &operation);
                            return Err(ToolCallError::denied(reason, record));
                        }
                        if let Err(error) = self
                            .admit_host_operation(
                                receipt,
                                &operation,
                                permission_input,
                                HostAdmission {
                                    session_evaluation: &session_evaluation,
                                    policy_revision: revision,
                                    host_permission,
                                    approval_path: if was_approved_by_human {
                                        ApprovalPath::Human
                                    } else {
                                        match assessment_details
                                            .as_ref()
                                            .and_then(|details| details.reviewer.as_ref())
                                        {
                                            Some(reviewer)
                                                if reviewer.kind == ApprovalReviewerKind::Llm =>
                                            {
                                                ApprovalPath::AutoLlm
                                            }
                                            Some(_) => ApprovalPath::AutoJev,
                                            None => ApprovalPath::Direct,
                                        }
                                    },
                                    direct_basis: if host_permission
                                        == Some(crate::host_tools::HostToolPermission::ReadOnly)
                                    {
                                        super::denials::DirectApprovalBasis::ReadOnly
                                    } else if execution_mode == ExecutionMode::FullAccess {
                                        super::denials::DirectApprovalBasis::FullAccess
                                    } else {
                                        super::denials::DirectApprovalBasis::HostTool
                                    },
                                    approval_source: final_approval_source,
                                    assessment: assessment_details.as_ref(),
                                },
                                &cancel,
                            )
                            .await
                        {
                            if cancel.is_cancelled() {
                                return Err(error.into());
                            }
                            let current_policy = self
                                .inner
                                .chats
                                .lock()
                                .await
                                .get(&receipt.chat_id)
                                .ok_or_else(|| ToolCallError::from("chat is unavailable"))?
                                .snapshot
                                .policy
                                .clone();
                            let record = super::denials::DenialCandidate::new(
                                &operation,
                                permission_input,
                                DenialCategory::FinalAdmission,
                                final_approval_source,
                                &current_policy,
                                assessment_details.as_ref(),
                            )
                            .finish(receipt.chat_id, &operation);
                            return Err(ToolCallError::denied(error, record));
                        }
                        let watch_authority = match prepared_host_call.as_ref().and_then(|call| {
                            call.payload::<crate::tick_stream::PreparedTickWatch>()
                        }) {
                            Some(watch) => watch
                                .host_authority()?
                                .and_then(|binding| binding.authority),
                            None => None,
                        };
                        if let (Some(provider), Some(prepared)) =
                            (host_tools.as_ref(), prepared_host_call)
                        {
                            let host_result = provider
                                .execute_prepared(receipt.chat_id, prepared, cancel.clone())
                                .await;
                            host_call_status = Some(host_result.status());
                            let (result, effect) = host_result.into_parts();
                            host_effect = effect;
                            Ok(result)
                        } else {
                            let dispatch_operation = OperationRecord {
                                name: name.clone(),
                                input: input.clone(),
                                ..operation.clone()
                            };
                            self.dispatch_registered_tool(
                                receipt,
                                &dispatch_operation,
                                registration
                                    .as_ref()
                                    .expect("registered local or Host tool"),
                                prepared_local.as_ref(),
                                watch_authority,
                                cancel.clone(),
                            )
                            .await
                        }
                    }
                }
            }
        } else {
            let tools = self
                .inner
                .chats
                .lock()
                .await
                .get(&receipt.chat_id)
                .unwrap()
                .tools
                .clone();
            let prepared = if registration.is_some() {
                tools
                    .prepare(&workspace, &name, &input)
                    .await
                    .map_err(|error| error.to_string())
            } else {
                Err(format!("unknown tool: {name}"))
            };
            match prepared {
                Ok(prepared) => {
                    if let Err(error) = self
                        .check_delegated_call(
                            receipt,
                            &prepared.name,
                            &prepared.input,
                            Some(&prepared),
                        )
                        .await
                    {
                        return Ok(ToolCallOutcome::not_admitted(error));
                    }
                    let session_operation =
                        SessionPermissionOperation::from_prepared(&prepared, &configured_workspace)
                            .map_err(|error| error.to_string())?;
                    let operation = OperationRecord {
                        id: Uuid::new_v4(),
                        loop_id: receipt.loop_id,
                        run_id: receipt.run_id,
                        call_id: id.clone(),
                        name: prepared.name.clone(),
                        input: prepared.input.clone(),
                        file_change: prepared.file_change.clone(),
                        state: OperationState::Proposed,
                        result: None,
                    };
                    {
                        let mut chats = self.inner.chats.lock().await;
                        let chat = chats.get_mut(&receipt.chat_id).unwrap();
                        chat.snapshot.operations.push(operation.clone());
                        self.save(&chat.snapshot);
                    }
                    let (
                        mut decision,
                        explicit_ask,
                        mut policy_revision,
                        mut delegated_revision,
                        mut session_evaluation,
                        execution_mode,
                        mut policy,
                        plan_file,
                    ) = {
                        let chats = self.inner.chats.lock().await;
                        let chat = chats.get(&receipt.chat_id).unwrap();
                        let sessions = self.inner.session_permissions.lock().await;
                        let session_evaluation =
                            sessions.evaluate(permission_owner, &session_operation);
                        let plan_file = plan::current_plan_file(
                            &chat.bookkeeping,
                            self.plans_directory(&chat.snapshot.workdir).path(),
                            receipt.loop_id,
                        );
                        let authority = delegation::permission_authority(
                            &chats,
                            &sessions,
                            receipt,
                            &prepared,
                            session_evaluation.decision,
                            plan_file.as_deref(),
                        )
                        .unwrap();
                        (
                            authority.decision,
                            authority.explicit_ask,
                            authority.policy_revision,
                            authority.parent_revision,
                            session_evaluation,
                            chat.snapshot.policy.mode,
                            chat.snapshot.policy.clone(),
                            plan_file,
                        )
                    };
                    let is_plan_mode_side_effect = execution_mode == ExecutionMode::Plan
                        && prepared.is_plan_mode_side_effect()
                        && !plan_file
                            .as_deref()
                            .is_some_and(|path| prepared.accesses_file(path));
                    let mut assessment_details = None;
                    if decision == Decision::Deny {
                        deterministic_denial_reason = Some(if is_plan_mode_side_effect {
                            "Plan execution mode denied this side effect".to_owned()
                        } else if let Some(message) = policy.content_denial_message(
                            &prepared.name,
                            &prepared.input,
                            receipt.loop_id,
                            session_evaluation.decision,
                        ) {
                            message
                        } else {
                            "an authorization rule denied this operation".to_owned()
                        });
                    }
                    let safety_check = (decision == Decision::Ask)
                        .then(|| prepared.safety_check())
                        .flatten();
                    let mut approval_source = if explicit_ask {
                        PermissionApprovalSource::ExplicitAsk
                    } else if safety_check.is_some() {
                        PermissionApprovalSource::SafetyCheck
                    } else {
                        PermissionApprovalSource::PermissionPolicy
                    };
                    let mut approval_reason = (decision == Decision::Ask).then(|| {
                        if explicit_ask {
                            "an explicit Ask rule requires human approval".to_owned()
                        } else {
                            safety_check.map_or_else(
                                || "the permission policy requires human approval".to_owned(),
                                |check| check.reason,
                            )
                        }
                    });
                    if decision == Decision::Ask
                        && self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt
                    {
                        deterministic_denial_reason =
                            Some(auto_mode::prompts_unavailable_message(&prepared.name));
                    }
                    let bypassed_by_unrestricted_host = !explicit_ask
                        && matches!(decision, Decision::Ask | Decision::Check)
                        && self.inner.host_options.prompt_policy == PromptPolicy::Unrestricted;
                    decision = self.apply_prompt_policy_with_explicit_ask(decision, explicit_ask);
                    if decision == Decision::Check {
                        let mut review_attempts = 0;
                        loop {
                            review_attempts += 1;
                            let mut assessment = self
                                .review_auto_operation(
                                    receipt,
                                    &operation,
                                    Some(prepared.approval_facts()),
                                    cancel.clone(),
                                )
                                .await
                                .map_err(|error| error.to_string())?;
                            {
                                let mut chats = self.inner.chats.lock().await;
                                let chat = chats.get_mut(&receipt.chat_id).unwrap();
                                self.emit(
                                    chat,
                                    receipt,
                                    EventKind::ApprovalAssessed {
                                        assessment: Box::new(assessment.details.clone()),
                                    },
                                );
                            }
                            assessment_details = Some(assessment.details.clone());
                            if matches!(assessment.verdict, AutoReview::Allow) {
                                let revision_matches = self
                                    .auto_settings_snapshot()
                                    .await
                                    .map(|(_, revision)| {
                                        assessment.details.auto_settings_revision.as_deref()
                                            == Some(revision.as_str())
                                    })
                                    .unwrap_or(false);
                                if !revision_matches {
                                    let (current, current_session, current_policy) = {
                                        let chats = self.inner.chats.lock().await;
                                        let chat = chats.get(&receipt.chat_id).unwrap();
                                        let sessions = self.inner.session_permissions.lock().await;
                                        let current_session =
                                            sessions.evaluate(permission_owner, &session_operation);
                                        let current = delegation::permission_authority(
                                            &chats,
                                            &sessions,
                                            receipt,
                                            &prepared,
                                            current_session.decision,
                                            plan_file.as_deref(),
                                        )
                                        .unwrap();
                                        (current, current_session, chat.snapshot.policy.clone())
                                    };
                                    let current_decision = current.decision;
                                    policy = current_policy;
                                    policy_revision = current.policy_revision;
                                    delegated_revision = current.parent_revision;
                                    session_evaluation = current_session;
                                    if review_attempts < 2
                                        && current_decision == Decision::Check
                                        && !cancel.is_cancelled()
                                    {
                                        continue;
                                    }
                                    let reason = "Auto approval settings or local policy changed before admission; retry the operation".to_owned();
                                    let record = super::denials::DenialCandidate::new(
                                        &operation,
                                        &operation.input,
                                        DenialCategory::FinalAdmission,
                                        Some(PermissionApprovalSource::AutoReviewer),
                                        &policy,
                                        assessment_details.as_ref(),
                                    )
                                    .finish(receipt.chat_id, &operation);
                                    return Err(ToolCallError::denied(reason, record));
                                }
                            }
                            let reviewed = self
                                .resolve_auto_assessment(
                                    receipt,
                                    &operation,
                                    &policy,
                                    &mut assessment,
                                )
                                .await?;
                            assessment_details = Some(assessment.details.clone());
                            decision = reviewed.decision;
                            approval_source = PermissionApprovalSource::AutoReviewer;
                            approval_reason = reviewed.approval_reason;
                            auto_denial_message = reviewed.denial_message;
                            break;
                        }
                    }
                    let permitted = match decision {
                        Decision::Allow => true,
                        Decision::Deny => false,
                        _ => {
                            let answer = self
                                .request_permission(
                                    receipt,
                                    &operation,
                                    session_operation.clone(),
                                    approval_source,
                                    approval_reason,
                                    cancel.clone(),
                                )
                                .await?;
                            policy_revision = answer.revision;
                            permission_comment = answer.comment;
                            is_rejected_by_user = !answer.allow;
                            was_approved_by_human = answer.allow;
                            continue_after_denial =
                                is_rejected_by_user && permission_comment.is_some();
                            session_evaluation = self
                                .inner
                                .session_permissions
                                .lock()
                                .await
                                .evaluate(permission_owner, &session_operation);
                            answer.allow
                        }
                    };
                    if permitted
                        && let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id)
                    {
                        chat.auto_denials.record_success();
                    }
                    if !permitted {
                        let reason = auto_denial_message.clone().unwrap_or_else(|| {
                            deterministic_denial_reason.clone().unwrap_or_else(|| {
                                permission_denial_message(permission_comment.as_deref())
                            })
                        });
                        let category = if is_rejected_by_user {
                            DenialCategory::Human
                        } else if is_plan_mode_side_effect {
                            DenialCategory::Plan
                        } else if let Some(details) = &assessment_details {
                            match details.outcome {
                                ApprovalOutcome::Block => DenialCategory::Automatic,
                                _ => DenialCategory::NoHuman,
                            }
                        } else if approval_source == PermissionApprovalSource::ExplicitAsk
                            || self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt
                        {
                            DenialCategory::NoHuman
                        } else {
                            DenialCategory::Rule
                        };
                        denial = Some(
                            super::denials::DenialCandidate::new(
                                &operation,
                                &operation.input,
                                category,
                                Some(approval_source),
                                &policy,
                                assessment_details.as_ref(),
                            )
                            .finish(receipt.chat_id, &operation),
                        );
                        Err(reason)
                    } else {
                        let final_approval_source = if was_approved_by_human {
                            Some(approval_source)
                        } else if assessment_details.is_some() {
                            Some(PermissionApprovalSource::AutoReviewer)
                        } else {
                            None
                        };
                        if let Some(details) = assessment_details
                            .as_ref()
                            .filter(|details| details.outcome == ApprovalOutcome::Allow)
                        {
                            let revision_matches = self
                                .auto_settings_snapshot()
                                .await
                                .map(|(_, revision)| {
                                    details.auto_settings_revision.as_deref()
                                        == Some(revision.as_str())
                                })
                                .unwrap_or(false);
                            if !revision_matches {
                                let current_policy = self
                                    .inner
                                    .chats
                                    .lock()
                                    .await
                                    .get(&receipt.chat_id)
                                    .ok_or_else(|| ToolCallError::from("chat is unavailable"))?
                                    .snapshot
                                    .policy
                                    .clone();
                                let reason = "Auto approval settings changed before final admission; retry the operation".to_owned();
                                let record = super::denials::DenialCandidate::new(
                                    &operation,
                                    &operation.input,
                                    DenialCategory::FinalAdmission,
                                    Some(PermissionApprovalSource::AutoReviewer),
                                    &current_policy,
                                    assessment_details.as_ref(),
                                )
                                .finish(receipt.chat_id, &operation);
                                return Err(ToolCallError::denied(reason, record));
                            }
                        }
                        if self.refresh_project_rules(receipt.chat_id).await.is_err() {
                            if cancel.is_cancelled() {
                                return Err("run interrupted before tool admission".into());
                            }
                            let reason =
                                "project permission rules are unavailable before final admission"
                                    .to_owned();
                            let record = super::denials::DenialCandidate::new(
                                &operation,
                                &operation.input,
                                DenialCategory::FinalAdmission,
                                final_approval_source,
                                &policy,
                                assessment_details.as_ref(),
                            )
                            .project_rules_unavailable()
                            .finish(receipt.chat_id, &operation);
                            return Err(ToolCallError::denied(reason, record));
                        }
                        let mut chats = self.inner.chats.lock().await;
                        let session_context = self.inner.session_permissions.lock().await;
                        delegation::check_call(
                            &chats,
                            &session_context,
                            receipt,
                            &prepared.name,
                            &prepared.input,
                            Some(&prepared),
                            plan_file.as_deref(),
                        )
                        .map_err(ToolCallError::from)?;
                        let chat = chats.get(&receipt.chat_id).unwrap();
                        if let Err(error) =
                            self.require_working_directory_trust(&chat.snapshot.workdir)
                        {
                            let reason = error.to_string();
                            let record = super::denials::DenialCandidate::new(
                                &operation,
                                &operation.input,
                                DenialCategory::FinalAdmission,
                                final_approval_source,
                                &chat.snapshot.policy,
                                assessment_details.as_ref(),
                            )
                            .finish(receipt.chat_id, &operation);
                            return Err(ToolCallError::denied(reason, record));
                        }
                        if cancel.is_cancelled() || chat.active_run != Some(receipt.run_id) {
                            return Err("run interrupted before tool admission".into());
                        }
                        let current_session_operation = SessionPermissionOperation::from_prepared(
                            &prepared,
                            &configured_workspace_for_chat(chat),
                        )
                        .map_err(|error| {
                            let reason = error.to_string();
                            let record = super::denials::DenialCandidate::new(
                                &operation,
                                &operation.input,
                                DenialCategory::FinalAdmission,
                                final_approval_source,
                                &chat.snapshot.policy,
                                assessment_details.as_ref(),
                            )
                            .finish(receipt.chat_id, &operation);
                            ToolCallError::denied(reason, record)
                        })?;
                        let current = delegation::permission_authority(
                            &chats,
                            &session_context,
                            receipt,
                            &prepared,
                            session_evaluation.decision,
                            plan_file.as_deref(),
                        )
                        .unwrap();
                        if !session_context.revalidate(
                            &session_evaluation.token,
                            permission_owner,
                            &current_session_operation,
                        ) || !current.still_admits(policy_revision, delegated_revision)
                        {
                            let reason =
                                "permission policy changed before admission; retry the operation"
                                    .to_owned();
                            let record = super::denials::DenialCandidate::new(
                                &operation,
                                &operation.input,
                                DenialCategory::FinalAdmission,
                                final_approval_source,
                                &chat.snapshot.policy,
                                assessment_details.as_ref(),
                            )
                            .finish(receipt.chat_id, &operation);
                            return Err(ToolCallError::denied(reason, record));
                        }
                        drop(session_context);
                        let chat = chats.get_mut(&receipt.chat_id).unwrap();
                        self.admit_operation(chat, operation.id)
                            .map_err(|error| error.to_string())?;
                        let approval_path = if was_approved_by_human {
                            ApprovalPath::Human
                        } else {
                            match assessment_details
                                .as_ref()
                                .and_then(|details| details.reviewer.as_ref())
                            {
                                Some(reviewer) if reviewer.kind == ApprovalReviewerKind::Llm => {
                                    ApprovalPath::AutoLlm
                                }
                                Some(_) => ApprovalPath::AutoJev,
                                None => ApprovalPath::Direct,
                            }
                        };
                        let approval = super::denials::allowed_approval_record(
                            receipt.chat_id,
                            &operation,
                            &operation.input,
                            &chat.snapshot.policy,
                            super::denials::AllowedDecision {
                                path: approval_path,
                                direct_basis: if prepared.is_read_only {
                                    super::denials::DirectApprovalBasis::ReadOnly
                                } else if chat.snapshot.policy.mode == ExecutionMode::FullAccess {
                                    super::denials::DirectApprovalBasis::FullAccess
                                } else if bypassed_by_unrestricted_host {
                                    super::denials::DirectApprovalBasis::UnrestrictedHost
                                } else {
                                    super::denials::DirectApprovalBasis::Policy
                                },
                                approval_source: final_approval_source,
                                assessment: assessment_details.as_ref(),
                            },
                        );
                        chat.denial_window.add_approval(approval);
                        self.save(&chat.snapshot);
                        self.emit(
                            chat,
                            receipt,
                            EventKind::ToolStarted {
                                call_id: id.clone(),
                                name: prepared.name.clone(),
                                input: prepared.input.clone(),
                            },
                        );
                        let tools = chat.tools.clone();
                        let context = tickloop_agent_tools::ExecutionContext {
                            operation_id: operation.id,
                            loop_id: receipt.loop_id,
                            accepts_images: chat.snapshot.model.accepts_images(),
                        };
                        let secondary_model = super::web_fetch::ChatSecondaryModel::new(
                            self,
                            receipt,
                            chat.client.clone(),
                        );
                        let edited_file = prepared
                            .edited_file_path()
                            .map(std::path::Path::to_path_buf);
                        drop(chats);
                        if let Some(path) = edited_file {
                            self.track_file_edit(receipt.chat_id, &path, assistant_message_id)
                                .await;
                        }
                        let execution = tools
                            .execute_with_context(
                                prepared,
                                cancel.clone(),
                                context,
                                &secondary_model,
                            )
                            .await;
                        images = execution.images;
                        execution.result.map_err(|error| error.to_string())
                    }
                }
                Err(error) => Err(error),
            }
        };
        let result = result.unwrap_or_else(|error| ToolResult {
            content: error,
            is_error: true,
            output_ref: None,
        });
        Ok(ToolCallOutcome {
            result,
            images,
            host_call_status,
            host_effect,
            permission_comment,
            continue_after_denial,
            is_rejected_by_user,
            denial,
        })
    }

    async fn resolve_auto_assessment(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        policy: &PermissionPolicy,
        assessment: &mut approval_assessment::ApprovalAssessment,
    ) -> Result<AutoPermissionDecision, ToolCallError> {
        let decision;
        let mut approval_reason = None;
        let mut auto_denial_message = None;
        match assessment.verdict.clone() {
            AutoReview::Allow => decision = Decision::Allow,
            AutoReview::Ask { reason, .. } => {
                decision = self.apply_prompt_policy(Decision::Ask);
                approval_reason = Some(reason.clone());
                if decision == Decision::Deny {
                    auto_denial_message = Some(reason);
                }
            }
            AutoReview::ContextLimit => {
                if self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt {
                    self.emit_permission_check(
                        receipt,
                        Decision::Deny,
                        auto_mode::REVIEW_CONTEXT_LIMIT_MESSAGE.into(),
                    )
                    .await;
                    let mut reason = auto_mode::HEADLESS_CONTEXT_LIMIT_ERROR.to_owned();
                    if let Some(warning) = self
                        .log_auto_review(
                            receipt,
                            operation,
                            policy,
                            assessment,
                            Decision::Deny,
                            Some(&reason),
                        )
                        .await
                    {
                        reason.push_str(&format!("\n{warning}"));
                    }
                    let record = super::denials::DenialCandidate::new(
                        operation,
                        &operation.input,
                        DenialCategory::NoHuman,
                        Some(PermissionApprovalSource::AutoReviewer),
                        policy,
                        Some(&assessment.details),
                    )
                    .finish(receipt.chat_id, operation);
                    return Err(ToolCallError::denied(reason, record));
                }
                self.emit_permission_check(
                    receipt,
                    Decision::Ask,
                    auto_mode::REVIEW_CONTEXT_LIMIT_MESSAGE.into(),
                )
                .await;
                decision = Decision::Ask;
                approval_reason = Some(auto_mode::REVIEW_CONTEXT_LIMIT_MESSAGE.into());
            }
            AutoReview::Unavailable { message } => {
                decision = self.apply_prompt_policy(Decision::Ask);
                approval_reason = Some(message.clone());
                if decision == Decision::Deny {
                    auto_denial_message = Some(message);
                }
            }
            AutoReview::Block { reason, kind, .. } => {
                let limit_warning = self.record_auto_denial(receipt.chat_id).await;
                if limit_warning.is_some() {
                    assessment
                        .details
                        .diagnostics
                        .get_or_insert_with(Vec::new)
                        .push(tickloop_agent_protocol::ApprovalDiagnostic::DenialLimit);
                }
                match limit_warning {
                    Some(_)
                        if self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt =>
                    {
                        let mut reason = auto_mode::HEADLESS_DENIAL_LIMIT_ERROR.to_owned();
                        if let Some(warning) = self
                            .log_auto_review(
                                receipt,
                                operation,
                                policy,
                                assessment,
                                Decision::Deny,
                                Some(&reason),
                            )
                            .await
                        {
                            reason.push_str(&format!("\n{warning}"));
                        }
                        let record = super::denials::DenialCandidate::new(
                            operation,
                            &operation.input,
                            DenialCategory::Automatic,
                            Some(PermissionApprovalSource::AutoReviewer),
                            policy,
                            Some(&assessment.details),
                        )
                        .finish(receipt.chat_id, operation);
                        return Err(ToolCallError::denied(reason, record));
                    }
                    Some(warning) if kind == AutoBlockKind::SoftDeny => {
                        decision = Decision::Ask;
                        approval_reason =
                            Some(format!("{warning}\n\nLatest blocked action: {reason}"));
                    }
                    _ => {
                        decision = Decision::Deny;
                        auto_denial_message = Some(auto_mode::rejection_message(&reason));
                    }
                }
            }
        }
        if let Some(warning) = self
            .log_auto_review(
                receipt,
                operation,
                policy,
                assessment,
                decision,
                approval_reason
                    .as_deref()
                    .or(auto_denial_message.as_deref()),
            )
            .await
        {
            let reason = if decision == Decision::Ask {
                &mut approval_reason
            } else {
                &mut auto_denial_message
            };
            reason
                .get_or_insert_with(String::new)
                .push_str(&format!("\n{warning}"));
        }
        Ok(AutoPermissionDecision {
            decision,
            approval_reason,
            denial_message: auto_denial_message,
        })
    }

    async fn record_auto_denial(&self, chat_id: ChatId) -> Option<String> {
        self.inner
            .chats
            .lock()
            .await
            .get_mut(&chat_id)
            .and_then(|chat| chat.auto_denials.record_denial())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA_COMBINATORS: [&str; 3] = ["oneOf", "anyOf", "allOf"];

    fn combinators(schema: &Value) -> Vec<String> {
        match schema {
            Value::Object(fields) => fields
                .iter()
                .flat_map(|(key, child)| {
                    let mut found = combinators(child);
                    if SCHEMA_COMBINATORS.contains(&key.as_str()) {
                        found.push(key.clone());
                    }
                    found
                })
                .collect(),
            Value::Array(items) => items.iter().flat_map(combinators).collect(),
            _ => Vec::new(),
        }
    }

    /// Every tool this Agent defines itself, in each of its forms.
    fn built_in_definitions() -> Vec<ToolDefinition> {
        let directory = tempfile::tempdir().unwrap();
        let skills = tickloop_agent_config::skills::SkillRegistry::discover(directory.path(), None);
        tickloop_agent_tools::Tools::definitions()
            .into_iter()
            .chain([false, true].into_iter().flat_map(plan::definitions))
            .chain([false, true].into_iter().flat_map(interaction::definitions))
            .chain(delegation::definitions())
            .chain(
                [false, true]
                    .into_iter()
                    .flat_map(agent_messages::definitions),
            )
            .chain([crate::handback::definition()])
            .chain(tick_subscriptions::definitions())
            .chain(skills.tool_definition())
            .chain(
                [false, true]
                    .into_iter()
                    .flat_map(crate::tick_tools::definitions),
            )
            .chain([crate::mcp_tools::login_definition()])
            .collect()
    }

    /// OpenAI-compatible models such as GLM fill nothing for a schema whose root is only a
    /// `oneOf`, so every tool this Agent defines itself is one flat object; per-form rules
    /// belong to the tool's own input check.
    #[test]
    fn every_built_in_tool_schema_is_one_flat_object() {
        let definitions = built_in_definitions();
        for name in [
            "Read",
            "Skill",
            "Subagent",
            "SubscribeTick",
            "ModifyTick",
            "ArtifactLogin",
        ] {
            assert!(
                definitions.iter().any(|definition| definition.name == name),
                "{name} is not covered"
            );
        }
        for definition in definitions {
            let schema = &definition.input_schema;
            assert_eq!(schema["type"], "object", "{}", definition.name);
            assert!(
                schema["properties"].is_object(),
                "{} has no properties",
                definition.name
            );
            assert_eq!(
                combinators(schema),
                Vec::<String>::new(),
                "{} has schema alternatives",
                definition.name
            );
        }
    }

    /// The built-in Skills and those of the Tick packages in this repository name a tool by its
    /// compound name, such as `ModifyTick`; each such name must be a tool the model can be given.
    #[test]
    fn shipped_skills_name_only_tools_the_agent_offers() {
        const PACKAGE_SKILLS: [&str; 2] = [
            include_str!("../../../ticks/longbridge/skills/longbridge/SKILL.md"),
            include_str!("../../../ticks/futu/skills/futu/SKILL.md"),
        ];
        const NOT_TOOLS: [&str; 1] = ["OutcomeUnknown"];
        let tools = built_in_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .chain(crate::mcp_tools::ARTIFACT_TOOL_NAMES.map(str::to_owned))
            .collect::<HashSet<_>>();
        let is_compound_name = |word: &&str| {
            let mut segments = 0;
            let mut characters = word.chars().peekable();
            while let Some(first) = characters.next() {
                let mut lowercase = 0;
                while characters.next_if(char::is_ascii_lowercase).is_some() {
                    lowercase += 1;
                }
                if !first.is_ascii_uppercase() || lowercase == 0 {
                    return false;
                }
                segments += 1;
            }
            segments > 1
        };
        let named = tickloop_agent_config::skills::BUILTIN_SKILLS
            .into_iter()
            .chain(PACKAGE_SKILLS)
            .flat_map(|skill| skill.split(|character: char| !character.is_ascii_alphanumeric()))
            .filter(is_compound_name)
            .collect::<HashSet<_>>();
        assert!(named.is_superset(&HashSet::from([
            "ModifyTick",
            "SubscribeTick",
            "ArtifactRead"
        ])));
        for word in named {
            assert!(
                tools.contains(word) || NOT_TOOLS.contains(&word),
                "a shipped Skill names {word}, which is no tool"
            );
        }
    }
}
