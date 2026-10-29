use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cache_prefix::ListingGroup;
use tickloop_harness_protocol::{
    ApprovalOutcome, ApprovalPath, ApprovalReviewerKind, AutoBlockKind, DenialCategory,
    DenialRecord, EventKind, HostToolAuthority, OperationRecord, OperationState,
    PermissionApprovalSource, PromptPolicy, Receipt, SEND_AGENT_MESSAGE_TOOL_NAME, ToolDefinition,
    ToolResult,
};
use tickloop_harness_tools::{
    ASK_USER_QUESTION_TOOL_NAME, ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME,
    permissions::PermissionPolicyExt,
};

/// Admits the `Read` of each local file a host call would send, by the calling Chat's Read rules
/// and delegated scope, before the file is opened.
struct ChatLocalReads {
    agent: Agent,
    receipt: Receipt,
}

#[async_trait::async_trait]
impl tickloop_harness_tools::LocalReadAdmission for ChatLocalReads {
    async fn admit(&self, read: &tickloop_harness_tools::PreparedTool) -> Result<bool, String> {
        self.agent.check_local_read(&self.receipt, read).await
    }
}

struct AutoPermissionDecision {
    decision: Decision,
    approval_reason: Option<String>,
    denial_message: Option<String>,
}

#[derive(Clone)]
enum ToolRoute {
    Fold(Arc<crate::tool_exposure::ExposureDirectory>),
    BuiltIn,
    Plan,
    Interaction,
    /// The Skills whose listing the request sent, which its Skill calls expand against.
    Skill(Arc<tickloop_harness_policy::skills::SkillRegistry>),
    McpResource(Arc<dyn crate::host_tools::HostToolProvider>),
    TickSubscription,
    TickManage(Option<Arc<dyn crate::host_tools::HostToolProvider>>),
    Delegation,
    AgentMessage,
    SharedTask,
    Goal,
    ScheduledTask,
    Handback,
    Host(Arc<dyn crate::host_tools::HostToolProvider>),
    /// An MCP tool the Run's first request offered that is no longer offered: the Run keeps its
    /// declaration where it was listed, and its calls are not executed.
    Unavailable(ListingGroup),
}

/// What a call's admission hands its dispatch besides the call itself.
struct DispatchGrant {
    /// The host authority a Tick watch was admitted under.
    watch_authority: Option<HostToolAuthority>,
}

enum PreparedLocalCall {
    McpResource(crate::mcp_resources::PreparedResourceCall),
    Family(agent_messages::PreparedFamilyCall),
    Delegation(delegation::PreparedDelegation),
    Handback(crate::handback::PreparedHandback),
}

impl PreparedLocalCall {
    fn policy_input(&self) -> &Value {
        match self {
            Self::McpResource(call) => &call.input,
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

fn merge_tick_management(mut tools: Vec<RegisteredTool>) -> Vec<RegisteredTool> {
    let variants = tools
        .iter()
        .filter(|tool| tool.definition.name == crate::tick_tools::TICK_MANAGE)
        .cloned()
        .collect::<Vec<_>>();
    if variants.len() < 2 {
        return tools;
    }
    let mut definition = variants[0].definition.clone();
    let mut provider = None;
    let mut actions = Vec::new();
    let mut properties = serde_json::Map::new();
    let mut descriptions = Vec::new();
    for variant in variants {
        if let ToolRoute::Host(host) = variant.route {
            provider = Some(host);
        }
        descriptions.push(variant.definition.description);
        if let Some(fields) = variant.definition.input_schema["properties"].as_object() {
            for (key, value) in fields {
                if key == "action" {
                    for action in value["enum"].as_array().into_iter().flatten() {
                        if !actions.contains(action) {
                            actions.push(action.clone());
                        }
                    }
                } else {
                    merge_property(&mut properties, key, value);
                }
            }
        }
    }
    let names = actions
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    properties.insert(
        "action".into(),
        serde_json::json!({"type":"string","enum":actions,"description":format!("One of {names}; each takes the fields whose description names it.")}),
    );
    definition.input_schema = serde_json::json!({"type":"object","properties":properties,"required":["action"],"additionalProperties":false});
    definition.description = descriptions.join(" ");
    tools.retain(|tool| tool.definition.name != crate::tick_tools::TICK_MANAGE);
    tools.push(RegisteredTool {
        definition,
        route: ToolRoute::TickManage(provider),
    });
    tools
}

/// Adds one variant's field to the merged TickManage fields. A field both variants take keeps the
/// later variant's schema, as before, and both descriptions, so it names every action it serves.
fn merge_property(
    properties: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: &serde_json::Value,
) {
    let mut merged = value.clone();
    if let Some(earlier) = properties.get(key)
        && let (Some(first), Some(second)) = (
            earlier["description"].as_str(),
            value["description"].as_str(),
        )
        && first != second
    {
        merged["description"] = format!("{first} {second}").into();
    }
    properties.insert(key.to_owned(), merged);
}

fn listing_group(route: &ToolRoute) -> ListingGroup {
    match route {
        ToolRoute::BuiltIn => ListingGroup::BuiltIn,
        ToolRoute::McpResource(_) => ListingGroup::McpResources,
        ToolRoute::Fold(_) => ListingGroup::Fold,
        ToolRoute::Host(_) | ToolRoute::TickManage(Some(_)) => ListingGroup::HostTools,
        ToolRoute::Unavailable(group) => *group,
        _ => ListingGroup::Runtime,
    }
}

fn sort_for_listing(tools: &mut [RegisteredTool]) {
    tools.sort_by(|first, second| {
        listing_group(&first.route)
            .cmp(&listing_group(&second.route))
            .then_with(|| first.definition.name.cmp(&second.definition.name))
    });
}

/// Whether a Run holds the tool to its first request: the tools MCP servers bring, which come and
/// go as servers connect, disconnect and list again.
fn is_held(tool: &RegisteredTool) -> bool {
    match listing_group(&tool.route) {
        ListingGroup::BuiltIn | ListingGroup::Runtime => false,
        ListingGroup::McpResources | ListingGroup::Fold => true,
        ListingGroup::HostTools => tool
            .definition
            .name
            .starts_with(crate::tool_exposure::MCP_TOOL_PREFIX),
    }
}

/// The result of a call to a host tool the Run still declares although its source no longer
/// offers it.
const UNAVAILABLE_HOST_TOOL_MESSAGE: &str = "This tool is unavailable now: its source stopped offering it during this turn. This call was not executed.";

/// The result of a WebSearch call in a Chat whose host does not offer it.
const UNOFFERED_WEB_SEARCH_MESSAGE: &str =
    "WebSearch is not available in this Chat. This call was not executed.";
/// The result of a Task tool call in a Chat whose host does not offer Task tools.
const UNOFFERED_TASK_TOOLS_MESSAGE: &str = "Task tools are not available in this Chat; this call was not executed. Continue with the available tools.";

/// The result of a scheduled task tool call in a Chat that does not offer them, such as a
/// Subagent's.
const UNOFFERED_SCHEDULED_TASK_TOOLS_MESSAGE: &str = "Scheduled task tools are not available in this Chat; only the root Agent schedules tasks. This call was not executed.";

/// Runtime tools offered only through tool search, as Claude Code's `shouldDefer` tools: the
/// conversation names them, and a search loads their definitions. Never listed, whatever the
/// execution mode, so a mode switch leaves the tools a request offers unchanged.
const DEFERRED_RUNTIME_TOOLS: [&str; 1] = [ENTER_PLAN_MODE_TOOL_NAME];

/// Host tools that need the Chat's Folder: they show its Ticks or send its local files.
const FOLDER_HOST_TOOLS: [&str; 5] = [
    crate::tick_tools::TICK_DESCRIBE,
    crate::tick_tools::TICK_READ,
    crate::tick_tools::TICK_CALL,
    crate::tick_tools::TICK_MANAGE,
    crate::mcp_tools::ARTIFACT_UPLOAD_TOOL,
];

/// The MCP tools discovery offers `chat`; a delegated Chat only those its ceiling grants.
pub(super) fn chat_discovery_tools(
    chat: &Chat,
    provider: &dyn crate::host_tools::HostToolProvider,
) -> Vec<crate::tool_exposure::ExposureTool> {
    provider
        .discovery_tools()
        .into_iter()
        .filter(|tool| {
            chat.snapshot.delegation.as_ref().is_none_or(|delegation| {
                delegation
                    .ceiling
                    .host_tools
                    .contains_key(&tool.definition.name)
            })
        })
        .collect()
}

/// Vendor tool search when the Chat's model offers it, SearchTools otherwise.
pub(super) fn search_support(chat: &Chat) -> crate::tool_exposure::SearchSupport {
    if tickloop_harness_model::native_tool_search::offered(&chat.snapshot.model).is_some() {
        crate::tool_exposure::SearchSupport::Native
    } else {
        crate::tool_exposure::SearchSupport::Fold
    }
}

pub(super) struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    folded: std::collections::BTreeSet<String>,
    /// Runtime tools offered only through tool search, which a call may still name directly.
    searched: std::collections::BTreeSet<String>,
    /// Callable directly, but offered only through the model's native tool search.
    deferred: Vec<ToolDefinition>,
    /// Whether the Chat's model searches deferred tools natively.
    native_search: bool,
    /// The user's exposure changes, which reach the Run's held MCP tools at once.
    user_exposure: crate::cache_prefix::UserExposure,
}

impl ToolRegistry {
    /// The tools a request of `chat` offers: those `policy` admits, and the file tools, Bash and
    /// TaskStop only when the Chat has a Folder.
    pub(super) fn for_chat(
        chat: &Chat,
        host_provider: Option<Arc<dyn crate::host_tools::HostToolProvider>>,
        tick_stream_installed: bool,
        prompt_policy: PromptPolicy,
        task_access: shared_tasks::TaskToolAccess,
        policy: &crate::ToolPolicy,
        scheduled_task_terms: crate::scheduled_tasks::tools::ToolTerms,
    ) -> Self {
        let is_delegated = chat.snapshot.delegation.is_some();
        let has_folder = chat.snapshot.folder.is_some();
        let mut tools = tickloop_harness_tools::Tools::definitions()
            .into_iter()
            .filter(|definition| match definition.name.as_str() {
                tickloop_harness_tools::WEB_FETCH_TOOL_NAME => policy.web_fetch,
                tickloop_harness_tools::WEB_SEARCH_TOOL_NAME => policy.web_search,
                _ => has_folder,
            })
            .map(|definition| RegisteredTool {
                definition,
                route: ToolRoute::BuiltIn,
            })
            .collect::<Vec<_>>();
        if policy.plan {
            tools.extend(
                plan::definitions(is_delegated, policy.delegation)
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::Plan,
                    }),
            );
        }
        if policy.questions {
            tools.push(RegisteredTool {
                definition: interaction::definition(is_delegated),
                route: ToolRoute::Interaction,
            });
        }
        if policy.delegation
            && chat.snapshot.delegation.as_ref().is_none_or(|delegation| {
                delegation.depth() < tickloop_harness_protocol::MAX_SUBAGENT_DEPTH
            })
        {
            tools.extend(
                delegation::definitions(policy.subagent_launch)
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::Delegation,
                    }),
            );
        }
        if policy.family_messages {
            tools.extend(agent_messages::definitions().into_iter().map(|definition| {
                RegisteredTool {
                    definition,
                    route: ToolRoute::AgentMessage,
                }
            }));
        }
        if policy.tasks && task_access != shared_tasks::TaskToolAccess::Disabled {
            tools.extend(
                shared_tasks::definitions(task_access)
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::SharedTask,
                    }),
            );
        }
        if policy.goals && !is_delegated {
            tools.extend(
                crate::goal::tools::definitions()
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::Goal,
                    }),
            );
        }
        if policy.scheduled_tasks && !is_delegated {
            tools.extend(
                crate::scheduled_tasks::tools::definitions(
                    scheduled_task_terms,
                    policy.schedule_wakeup,
                )
                .into_iter()
                .map(|definition| RegisteredTool {
                    definition,
                    route: ToolRoute::ScheduledTask,
                }),
            );
        }
        if policy.delegation && crate::handback::available(chat, prompt_policy) {
            tools.push(RegisteredTool {
                definition: crate::handback::definition(),
                route: ToolRoute::Handback,
            });
        }
        if policy.tick_subscriptions && tick_stream_installed {
            tools.extend(
                tick_subscriptions::definitions()
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::TickSubscription,
                    }),
            );
        }
        if policy.skills {
            tools.extend(
                chat.skills
                    .tool_definition_for_context(context::context_window(&chat.snapshot))
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::Skill(chat.skills.clone()),
                    }),
            );
        }
        let native_search =
            tickloop_harness_model::native_tool_search::offered(&chat.snapshot.model).is_some();
        let searched: std::collections::BTreeSet<String> = tools
            .iter()
            .map(|tool| tool.definition.name.clone())
            .filter(|name| DEFERRED_RUNTIME_TOOLS.contains(&name.as_str()))
            .collect();
        let mut discovery_tools: Vec<_> = tools
            .iter()
            .filter(|tool| searched.contains(&tool.definition.name))
            .map(|tool| {
                crate::tool_exposure::ExposureTool::deferred_runtime_tool(tool.definition.clone())
            })
            .collect();
        let mut unavailable_servers = Vec::new();
        let mut offers_search = !discovery_tools.is_empty();
        let mut folded = std::collections::BTreeSet::new();
        let mut deferred = Vec::new();
        let mut user_exposure = crate::cache_prefix::UserExposure::default();
        if let Some(provider) = host_provider.filter(|_| policy.artifacts) {
            if provider.has_tool_search()
                || provider
                    .mcp_resource_servers()
                    .iter()
                    .any(|server| crate::mcp_resource_authority_allowed(chat, &server.authority))
            {
                tools.extend(
                    crate::mcp_resources::definitions()
                        .into_iter()
                        .map(|definition| RegisteredTool {
                            definition,
                            route: ToolRoute::McpResource(provider.clone()),
                        }),
                );
            }
            if provider.has_tool_search() {
                let mcp_tools = chat_discovery_tools(chat, provider.as_ref());
                user_exposure = crate::cache_prefix::UserExposure {
                    revisions: provider.exposure_revisions(),
                    tool_servers: mcp_tools
                        .iter()
                        .filter_map(|tool| {
                            Some((tool.definition.name.clone(), tool.server.clone()?))
                        })
                        .collect(),
                };
                discovery_tools.extend(mcp_tools);
                unavailable_servers = provider.unavailable_tool_servers();
                offers_search = true;
            }
            tools.extend(
                provider
                    .definitions()
                    .into_iter()
                    .filter(|definition| {
                        (has_folder || !FOLDER_HOST_TOOLS.contains(&definition.name.as_str()))
                            && chat.snapshot.delegation.as_ref().is_none_or(|delegation| {
                                delegation.ceiling.host_tools.contains_key(&definition.name)
                            })
                    })
                    .map(|definition| RegisteredTool {
                        definition: crate::mcp_tools::chat_definition(definition, has_folder),
                        route: ToolRoute::Host(provider.clone()),
                    }),
            );
        }
        // Tool search is offered whenever a tool waits behind it, with or without MCP servers.
        if offers_search {
            let directory = Arc::new(
                crate::tool_exposure::ExposureDirectory::new(discovery_tools, search_support(chat))
                    .with_unavailable_servers(unavailable_servers),
            );
            deferred = directory.deferred_tools();
            deferred.sort_by(|first, second| first.name.cmp(&second.name));
            let deferred_names: HashSet<_> =
                deferred.iter().map(|tool| tool.name.as_str()).collect();
            folded.extend(
                directory
                    .tools()
                    .filter(|tool| {
                        tool.server.is_some()
                            && tool.exposure != tickloop_harness_protocol::mcp::ToolExposure::Direct
                            && !deferred_names.contains(tool.definition.name.as_str())
                    })
                    .map(|tool| tool.definition.name.clone()),
            );
            tools.extend(
                directory
                    .discovery_definitions()
                    .into_iter()
                    .map(|definition| RegisteredTool {
                        definition,
                        route: ToolRoute::Fold(directory.clone()),
                    }),
            );
        }
        let mut tools = merge_tick_management(tools);
        sort_for_listing(&mut tools);
        Self {
            tools,
            folded,
            searched,
            deferred,
            native_search,
            user_exposure,
        }
    }

    /// Holds the MCP part of the tools to what the Run's first request offered, as `state`
    /// decides: MCP tools that appeared since stay unlisted until the next Run, and those that
    /// vanished keep their declaration and are not executed, unless the user changed their
    /// server's exposure. Other host tools, such as the Artifacts tools a sign-in brings, follow
    /// the host as before.
    pub(super) fn hold_host_tools(
        &mut self,
        state: &mut crate::cache_prefix::CachePrefixState,
        run_id: RunId,
    ) {
        let listed = self
            .tools
            .iter()
            .filter(|tool| is_held(tool) && self.is_listed(&tool.definition.name))
            .map(|tool| (listing_group(&tool.route), tool.definition.clone()))
            .collect();
        state.order_deferred(&mut self.deferred);
        let Some(offer) = state.hold(
            run_id,
            self.native_search,
            listed,
            self.deferred.clone(),
            self.user_exposure.clone(),
        ) else {
            return;
        };
        self.folded.extend(offer.withheld);
        for (group, definition) in offer.listed {
            self.folded.remove(&definition.name);
            match self
                .tools
                .iter_mut()
                .find(|tool| tool.definition.name == definition.name)
            {
                Some(tool) => tool.definition = definition,
                None => self.tools.push(RegisteredTool {
                    definition,
                    route: ToolRoute::Unavailable(group),
                }),
            }
        }
        let registered: HashSet<_> = self
            .tools
            .iter()
            .map(|tool| tool.definition.name.clone())
            .collect();
        for definition in &offer.deferred {
            if !registered.contains(&definition.name) {
                self.tools.push(RegisteredTool {
                    definition: definition.clone(),
                    route: ToolRoute::Unavailable(ListingGroup::HostTools),
                });
            }
        }
        sort_for_listing(&mut self.tools);
        self.deferred = offer.deferred;
    }

    fn is_listed(&self, name: &str) -> bool {
        !self.folded.contains(name)
            && !self.searched.contains(name)
            && !self.deferred.iter().any(|tool| tool.name == name)
    }

    /// The names of the runtime tools offered only through tool search.
    pub(super) fn searched_names(&self) -> Vec<String> {
        self.searched.iter().cloned().collect()
    }

    /// The runtime tools offered only through tool search, as tool search shows them.
    pub(super) fn searched_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| self.searched.contains(&tool.definition.name))
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub(super) fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| self.is_listed(&tool.definition.name))
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub(super) fn deferred_definitions(&self) -> Vec<ToolDefinition> {
        self.deferred.clone()
    }

    pub(super) fn refresh(&mut self, chat: &Chat) {
        if let Some(tool) = self
            .tools
            .iter_mut()
            .find(|tool| matches!(tool.route, ToolRoute::Skill(_)))
            && let Some(definition) = chat
                .skills
                .tool_definition_for_context(context::context_window(&chat.snapshot))
        {
            tool.definition = definition;
            tool.route = ToolRoute::Skill(chat.skills.clone());
        }
    }

    pub(super) fn host_tool_names(&self) -> std::collections::BTreeSet<String> {
        self.tools
            .iter()
            .filter(|tool| self.is_listed(&tool.definition.name))
            .filter(|tool| {
                matches!(
                    tool.route,
                    ToolRoute::Host(_) | ToolRoute::TickManage(Some(_)) | ToolRoute::Unavailable(_)
                )
            })
            .map(|tool| tool.definition.name.clone())
            .collect()
    }

    pub(super) fn resolve(&self, name: &str) -> Option<RegisteredTool> {
        if self.folded.contains(name) {
            return None;
        }
        self.resolve_nested(name)
    }

    pub(super) fn resolve_nested(&self, name: &str) -> Option<RegisteredTool> {
        self.tools
            .iter()
            .find(|tool| tool.definition.name == name)
            .cloned()
    }

    pub(super) fn is_concurrency_safe(
        &self,
        name: &str,
        input: &Value,
        workspace: &tickloop_harness_tools::Workspace,
        waits: crate::WaitHosting,
        subagent_launch: crate::SubagentLaunch,
    ) -> bool {
        let Some(tool) = self.tools.iter().find(|tool| tool.definition.name == name) else {
            return false;
        };
        match &tool.route {
            ToolRoute::BuiltIn => {
                tickloop_harness_tools::is_concurrency_safe(workspace, name, input)
            }
            ToolRoute::Host(provider) => provider.is_concurrency_safe(name, input),
            // A foreground Subagent call suspends its Run on a durable wait, and a durably
            // hosted Run goes on from exactly one waiting call.
            ToolRoute::Delegation => {
                waits == crate::WaitHosting::Process
                    || delegation::runs_in_background(subagent_launch, input)
            }
            ToolRoute::McpResource(_) => true,
            ToolRoute::SharedTask => matches!(name, "TaskGet" | "TaskList"),
            ToolRoute::ScheduledTask => crate::scheduled_tasks::tools::is_concurrency_safe(name),
            _ => false,
        }
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

/// A call its `PreToolUse` hooks let through, as it is prepared.
struct RegisteredCall {
    id: String,
    name: String,
    input: Value,
    registration: Option<RegisteredTool>,
    ids: tickloop_ids::IdContext,
    task_binding: Option<Option<tickloop_harness_protocol::TaskExecutionBinding>>,
}

/// What the call's `PreToolUse` hooks decided about its admission.
#[derive(Default)]
struct HookGate {
    permission: Option<crate::hooks::HookPermission>,
    reason: Option<String>,
}

impl HookGate {
    fn asks(&self) -> bool {
        self.permission == Some(crate::hooks::HookPermission::Ask)
    }

    fn allows(&self) -> bool {
        self.permission == Some(crate::hooks::HookPermission::Allow)
    }

    fn ask_reason(&self, tool: &str) -> String {
        self.reason
            .clone()
            .unwrap_or_else(|| format!("Hook PreToolUse:{tool} asked for confirmation"))
    }
}

/// Checks a host or local tool's input against its schema; built-in tools check theirs as
/// they are prepared.
fn validate_registered_input(registered: &RegisteredTool, input: &Value) -> Result<(), String> {
    if !registered.is_host_or_local() {
        return Ok(());
    }
    match &registered.route {
        ToolRoute::Host(provider) => provider.validate_input(
            &registered.definition.name,
            &registered.definition.input_schema,
            input,
        ),
        _ => validate_tool_input(&registered.definition, input),
    }
}

pub(super) struct ToolCallOutcome {
    pub(super) result: ToolResult,
    pub(super) sources: Option<Vec<tickloop_harness_protocol::CitationSource>>,
    /// Images for the model only, sent after the result text.
    pub(super) images: Vec<tickloop_harness_protocol::Image>,
    pub(super) host_call_status: Option<crate::host_tools::HostCallStatus>,
    pub(super) host_effect: Option<crate::host_tools::HostEffect>,
    pub(super) permission_comment: Option<String>,
    pub(super) continue_after_denial: bool,
    pub(super) is_rejected_by_user: bool,
    pub(super) denial: Option<DenialRecord>,
    /// What the call's hooks tell the model after its result.
    pub(super) hook_feedback: crate::hooks::ToolHookFeedback,
}

impl ToolCallOutcome {
    pub(super) fn not_admitted(message: String) -> Self {
        Self {
            result: ToolResult {
                content: message,
                is_error: true,
                output_ref: None,
            },
            sources: None,
            images: Vec::new(),
            host_call_status: None,
            host_effect: None,
            permission_comment: None,
            continue_after_denial: false,
            is_rejected_by_user: false,
            denial: None,
            hook_feedback: Default::default(),
        }
    }
}

pub(super) struct ToolCallError {
    pub(super) message: String,
    pub(super) denial: Option<Box<DenialRecord>>,
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
            denial: Some(Box::new(denial)),
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
        grant: DispatchGrant,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        match &registered.route {
            ToolRoute::Fold(directory) => {
                let value = match operation.name.as_str() {
                    crate::tool_exposure::SEARCH_TOOLS => directory.search(&operation.input),
                    crate::tool_exposure::DESCRIBE_TOOLS => directory.describe(&operation.input),
                    _ => unreachable!("ExecuteTools runs through nested calls"),
                }?;
                Ok(ToolResult {
                    content: value.to_string(),
                    is_error: false,
                    output_ref: None,
                })
            }
            ToolRoute::SharedTask => self.shared_task_tool(receipt, operation, &cancel).await,
            ToolRoute::Goal => self.goal_tool(receipt, operation, cancel).await,
            ToolRoute::ScheduledTask => self.scheduled_task_tool(receipt, operation).await,
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
                let feedback_account = self.feedback_account_context(receipt).await?;
                let Some(PreparedLocalCall::Delegation(prepared)) = prepared_local else {
                    return Err("Subagent was not prepared".into());
                };
                self.subagent_tool(
                    receipt,
                    &operation.call_id,
                    prepared,
                    feedback_account,
                    cancel,
                )
                .await
            }
            ToolRoute::McpResource(provider) => {
                let Some(PreparedLocalCall::McpResource(prepared)) = prepared_local else {
                    return Err("MCP resource call was not prepared".into());
                };
                self.mcp_resource_tool(receipt, &operation.call_id, provider, prepared, cancel)
                    .await
            }
            ToolRoute::Skill(skills) => self.skill_tool(receipt, skills, &operation.input).await,
            ToolRoute::TickManage(_) => Err("TickManage route was not prepared".into()),
            ToolRoute::Unavailable(_) => Err(UNAVAILABLE_HOST_TOOL_MESSAGE.into()),
            ToolRoute::TickSubscription => {
                self.tick_subscription_tool(
                    receipt,
                    &operation.name,
                    &operation.input,
                    grant.watch_authority,
                )
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
        if invocation.name == crate::tool_exposure::EXECUTE_TOOLS
            && matches!(
                invocation.registration.as_ref().map(|tool| &tool.route),
                Some(ToolRoute::Fold(_))
            )
        {
            return Box::pin(self.execute_fold(
                receipt,
                assistant_message_id,
                invocation.id,
                invocation.input,
                cancel,
            ))
            .await;
        }
        let call_id = invocation.id.clone();
        let is_shell = invocation.name == tickloop_harness_tools::BASH_TOOL_NAME;
        // Retries reuse call IDs; settle only the operation created by this invocation.
        let operation_start = self
            .inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .ok_or_else(|| ToolCallError::from("chat is unavailable"))?
            .snapshot
            .operations
            .len();
        let outcome = Box::pin(self.execute_registered_call_inner(
            receipt,
            assistant_message_id,
            invocation,
            cancel.clone(),
        ))
        .await;
        let command_state = if is_shell {
            self.command_state_for_call(receipt, &call_id).await
        } else {
            None
        };
        let mut chats = self.inner.chats.lock().await;
        if let Some(chat) = chats.get_mut(&receipt.chat_id)
            && let Some(operation) = chat
                .snapshot
                .operations
                .iter_mut()
                .skip(operation_start)
                .rev()
                .find(|operation| {
                    operation.run_id == receipt.run_id && operation.call_id == call_id
                })
        {
            use crate::host_tools::HostCallStatus;
            use tickloop_harness_protocol::CommandState;

            if outcome.is_err()
                && cancel.is_cancelled()
                && operation.state == OperationState::Proposed
            {
                chat.snapshot.tool_call_recoveries.push(
                    tickloop_harness_protocol::ToolCallRecovery {
                        run_id: Some(receipt.run_id),
                        call_id: call_id.clone(),
                        was_admitted: Some(false),
                    },
                );
            }
            // Numbered web results become observable only after their source association and
            // conversation node are durable. Recovery must not publish the raw operation early.
            if outcome
                .as_ref()
                .is_ok_and(|outcome| outcome.sources.is_some())
            {
                return outcome;
            }
            let (result, state) = match &outcome {
                Ok(outcome) => {
                    let state = match (outcome.host_call_status, command_state) {
                        (Some(HostCallStatus::OutcomeUnknown), _) => OperationState::Unknown,
                        (Some(HostCallStatus::NotDispatched), _) => OperationState::Failed,
                        (
                            _,
                            Some(
                                CommandState::Running
                                | CommandState::Background
                                | CommandState::Cancelling,
                            ),
                        ) => OperationState::Admitted,
                        (_, Some(CommandState::Unknown)) => OperationState::Unknown,
                        (_, Some(CommandState::Completed)) => OperationState::Completed,
                        (_, Some(CommandState::Failed)) => OperationState::Failed,
                        _ if outcome.result.is_error => OperationState::Failed,
                        _ => OperationState::Completed,
                    };
                    (outcome.result.clone(), state)
                }
                Err(error) => (
                    ToolResult {
                        content: error.message.clone(),
                        is_error: true,
                        output_ref: None,
                    },
                    if operation.state == OperationState::Proposed {
                        OperationState::Failed
                    } else {
                        OperationState::Unknown
                    },
                ),
            };
            if !matches!(
                operation.state,
                OperationState::Completed | OperationState::Failed
            ) {
                operation.state = state;
            }
            operation.result = Some(result);
            self.record_nested_operation(receipt.chat_id, operation);
            self.save(&chat.snapshot);
        }
        outcome
    }

    async fn execute_registered_call_inner(
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
        let ids = self.chat_ids(receipt.chat_id).await;
        let input = if crate::tick_tools::is_tick_tool(&name) {
            crate::tick_tools::stored_instance_input(&ids, &name, input)
        } else {
            input
        };
        let registration = registration.map(|mut registered| {
            if let ToolRoute::TickManage(provider) = &registered.route {
                registered.route =
                    if matches!(input["action"].as_str(), Some("subscribe" | "unsubscribe")) {
                        ToolRoute::TickSubscription
                    } else {
                        provider
                            .as_ref()
                            .map_or(ToolRoute::TickSubscription, |provider| {
                                ToolRoute::Host(provider.clone())
                            })
                    };
            }
            registered
        });
        let input = if registration
            .as_ref()
            .is_some_and(|tool| matches!(tool.route, ToolRoute::Delegation))
        {
            delegation::offered_input(self.inner.tool_policy.subagent_launch, input)
        } else {
            input
        };
        if cancel.is_cancelled() {
            return Err("run interrupted before tool preparation".into());
        }
        if registration
            .as_ref()
            .is_some_and(|tool| matches!(tool.route, ToolRoute::Unavailable(_)))
        {
            return Ok(ToolCallOutcome::not_admitted(
                UNAVAILABLE_HOST_TOOL_MESSAGE.into(),
            ));
        }
        if name == tickloop_harness_tools::WEB_SEARCH_TOOL_NAME
            && (registration.is_none()
                || self
                    .inner
                    .chats
                    .lock()
                    .await
                    .get(&receipt.chat_id)
                    .is_some_and(|chat| chat.snapshot.search_config.is_none()))
        {
            return Ok(ToolCallOutcome::not_admitted(
                if self.inner.tool_policy.web_search {
                    tickloop_harness_tools::web_search::DISABLED_MESSAGE
                } else {
                    UNOFFERED_WEB_SEARCH_MESSAGE
                }
                .into(),
            ));
        }
        if registration.is_none() && shared_tasks::is_task_tool(&name) {
            return Ok(ToolCallOutcome::not_admitted(
                if self.inner.tool_policy.tasks {
                    "Task tools are disabled or limited for this request; this call was not executed. Continue with the available tools. Task tools can be enabled in Settings."
                } else {
                    UNOFFERED_TASK_TOOLS_MESSAGE
                }
                .into(),
            ));
        }
        if registration.is_none() && crate::scheduled_tasks::tools::is_scheduled_task_tool(&name) {
            return Ok(ToolCallOutcome::not_admitted(
                UNOFFERED_SCHEDULED_TASK_TOOLS_MESSAGE.into(),
            ));
        }
        self.refresh_task_binding_setting(receipt)
            .await
            .map_err(|error| ToolCallError::from(error.to_string()))?;
        let task_binding = if matches!(
            registration.as_ref().map(|tool| &tool.route),
            Some(ToolRoute::SharedTask)
        ) {
            None
        } else {
            Some(
                shared_tasks::binding_for_tool(&*self.inner.chats.lock().await, receipt, &name)
                    .map_err(ToolCallError::from)?,
            )
        };

        if let Some(registered) = &registration
            && let Err(error) = validate_registered_input(registered, &input)
        {
            return Ok(ToolCallOutcome::not_admitted(error));
        }
        let (input, hook_gate, pre_tool_feedback) = if registration.is_some() {
            match self
                .pre_tool_use(receipt, &id, &name, input.clone(), &cancel)
                .await
            {
                crate::hooks::PreToolGate::Cancelled => {
                    return Ok(ToolCallOutcome {
                        host_call_status: Some(crate::host_tools::HostCallStatus::NotDispatched),
                        ..ToolCallOutcome::not_admitted(
                            crate::recovery::NOT_ADMITTED_AFTER_INTERRUPTION.into(),
                        )
                    });
                }
                crate::hooks::PreToolGate::Refused { message, feedback } => {
                    return Ok(ToolCallOutcome {
                        hook_feedback: feedback,
                        ..ToolCallOutcome::not_admitted(message)
                    });
                }
                crate::hooks::PreToolGate::Proceed {
                    input: rewritten,
                    permission,
                    reason,
                    feedback,
                } => {
                    let route = registration.as_ref().map(|tool| &tool.route);
                    let rewritten = if rewritten == input {
                        rewritten
                    } else if matches!(route, Some(ToolRoute::Delegation)) {
                        delegation::offered_input(self.inner.tool_policy.subagent_launch, rewritten)
                    } else if crate::tick_tools::is_tick_tool(&name) {
                        crate::tick_tools::stored_instance_input(&ids, &name, rewritten)
                    } else {
                        rewritten
                    };
                    // A Tick call's route was chosen by its action; a rewrite may not choose
                    // another.
                    if crate::tick_tools::is_tick_tool(&name)
                        && rewritten.get("action") != input.get("action")
                    {
                        return Ok(ToolCallOutcome {
                            hook_feedback: feedback,
                            ..ToolCallOutcome::not_admitted(format!(
                                "A PreToolUse hook's updatedInput changed the action of {name}; the call was not run."
                            ))
                        });
                    }
                    if let Some(registered) = &registration
                        && let Err(error) = validate_registered_input(registered, &rewritten)
                    {
                        return Ok(ToolCallOutcome {
                            hook_feedback: feedback,
                            ..ToolCallOutcome::not_admitted(error)
                        });
                    }
                    (rewritten, HookGate { permission, reason }, feedback)
                }
            }
        } else {
            (input, HookGate::default(), Default::default())
        };
        let mut outcome = self
            .admit_and_run_call(
                receipt,
                assistant_message_id,
                RegisteredCall {
                    id,
                    name,
                    input,
                    registration,
                    ids,
                    task_binding,
                },
                hook_gate,
                cancel,
            )
            .await?;
        let mut hook_feedback = pre_tool_feedback;
        hook_feedback.extend(std::mem::take(&mut outcome.hook_feedback));
        outcome.hook_feedback = hook_feedback;
        Ok(outcome)
    }

    async fn failed_tool_preparation(
        &self,
        receipt: &Receipt,
        call_id: &str,
        name: &str,
        input: &Value,
        error: String,
        cancel: &CancellationToken,
    ) -> ToolCallOutcome {
        let mut outcome = ToolCallOutcome::not_admitted(error);
        if !cancel.is_cancelled() {
            outcome.hook_feedback = self
                .post_tool_use(receipt, call_id, name, input, &outcome.result, cancel)
                .await;
        }
        outcome
    }

    /// Prepares, admits and runs a call its `PreToolUse` hooks let through, then runs its
    /// `PostToolUse` after success or `PostToolUseFailure` after a preparation or execution error.
    async fn admit_and_run_call(
        &self,
        receipt: &Receipt,
        assistant_message_id: Uuid,
        call: RegisteredCall,
        hook_gate: HookGate,
        cancel: CancellationToken,
    ) -> Result<ToolCallOutcome, ToolCallError> {
        let RegisteredCall {
            id,
            name,
            input,
            registration,
            ids,
            task_binding,
        } = call;
        let mut dispatched = false;
        let mut preparation_failed = false;
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
        let mut sources = None;
        let (configured_workspace, workspace) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).unwrap();
            let configured_workspace = configured_workspace_for_chat(chat);
            let workspace = configured_workspace.clone();
            (configured_workspace, workspace)
        };
        let permission_owner = PermissionOwner::new(receipt.chat_id.into(), receipt.loop_id.into())
            .map_err(|error| error.to_string())?;
        let host_tools = registration
            .as_ref()
            .and_then(|registered| registered.host_provider());
        let artifact_account = match self.feedback_account_context(receipt).await {
            Ok(context) => context,
            Err(error) => return Ok(ToolCallOutcome::not_admitted(error)),
        };
        let prepared_host_call = if let Some(provider) = host_tools.as_ref() {
            if let Err(error) = self
                .check_delegated_host_call(receipt, provider.as_ref(), &name, &input)
                .await
            {
                return Ok(ToolCallOutcome::not_admitted(error));
            }
            let context = self.host_call_context(receipt, workspace.folder.as_deref());
            let local_files = self
                .inner
                .chats
                .lock()
                .await
                .get(&receipt.chat_id)
                .ok_or("chat is unavailable")?
                .tools
                .local_file_reader(
                    &workspace,
                    Arc::new(ChatLocalReads {
                        agent: self.clone(),
                        receipt: receipt.clone(),
                    }),
                );
            let scope = crate::host_tools::HostCallScope {
                local_files: local_files.as_ref(),
                context: context.as_ref(),
            };
            let prepared = provider
                .prepare_call_in_scope(receipt.chat_id, &scope, &name, &input, cancel.clone())
                .await;
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
                    Some(match artifact_account {
                        Some(context) => prepared.with_artifact_account(context),
                        None => prepared,
                    })
                }
                Err(error) => {
                    let error = if crate::tick_tools::is_tick_tool(&name) {
                        crate::tick_tools::model_tick_text(&ids, &error)
                    } else {
                        error
                    };
                    return Ok(self
                        .failed_tool_preparation(receipt, &id, &name, &input, error, &cancel)
                        .await);
                }
            }
        } else if matches!(
            registration.as_ref().map(|registered| &registered.route),
            Some(ToolRoute::TickSubscription)
        ) && name == "TickManage"
            && input["action"] == "subscribe"
        {
            let instance = match tick_subscriptions::subscription_request(&input) {
                Ok((instance, _)) => instance,
                Err(error) => {
                    return Ok(self
                        .failed_tool_preparation(
                            receipt,
                            &id,
                            &name,
                            &input,
                            crate::tick_tools::model_tick_text(&ids, &error),
                            &cancel,
                        )
                        .await);
                }
            };
            let port = self.tick_deliveries().port.clone();
            match port {
                Some(port) => match port
                    .prepare_watch(receipt.chat_id.into(), &instance, &input, cancel.clone())
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
                    Err(error) => {
                        return Ok(self
                            .failed_tool_preparation(receipt, &id, &name, &input, error, &cancel)
                            .await);
                    }
                },
                None => None,
            }
        } else {
            None
        };
        let local_read_asks = prepared_host_call
            .as_ref()
            .is_some_and(|call| call.local_read_asks());
        let prepared_local = if let Some(ToolRoute::McpResource(provider)) =
            registration.as_ref().map(|tool| &tool.route)
        {
            match crate::mcp_resources::PreparedResourceCall::prepare(
                provider.as_ref(),
                &name,
                &input,
            ) {
                Ok(prepared) => Some(PreparedLocalCall::McpResource(prepared)),
                Err(error) => {
                    return Ok(self
                        .failed_tool_preparation(receipt, &id, &name, &input, error, &cancel)
                        .await);
                }
            }
        } else if matches!(
            name.as_str(),
            SEND_AGENT_MESSAGE_TOOL_NAME | agent_messages::STOP_SUBAGENT_TOOL_NAME
        ) && matches!(
            registration.as_ref().map(|registered| &registered.route),
            Some(ToolRoute::AgentMessage)
        ) {
            match self.prepare_family_call(receipt, &name, &input).await {
                Ok(prepared) => Some(PreparedLocalCall::Family(prepared)),
                Err(rejection) => {
                    if matches!(
                        rejection.code,
                        tickloop_harness_protocol::FamilyMessageRejectionCode::InvalidTarget
                    ) {
                        return Ok(self
                            .failed_tool_preparation(
                                receipt,
                                &id,
                                &name,
                                &input,
                                rejection.result().content,
                                &cancel,
                            )
                            .await);
                    }
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
            match delegation::PreparedDelegation::parse(
                &input,
                self.inner.tool_policy.subagent_launch,
            ) {
                Ok(prepared) => Some(PreparedLocalCall::Delegation(prepared)),
                Err(error) => {
                    return Ok(self
                        .failed_tool_preparation(receipt, &id, &name, &input, error, &cancel)
                        .await);
                }
            }
        } else if matches!(
            registration.as_ref().map(|registered| &registered.route),
            Some(ToolRoute::Handback)
        ) {
            if let Some(feedback) = self.subagent_stop_before_handback(receipt, &cancel).await? {
                return Ok(ToolCallOutcome::not_admitted(feedback));
            }
            match self.prepare_handback(receipt, &id, &input).await {
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
                // As in Codex, the goal tools never wait on an approval: reading and reporting on
                // the goal change nothing outside it, and setting one runs directly too.
                if matches!(
                    route,
                    ToolRoute::SharedTask | ToolRoute::McpResource(_) | ToolRoute::Fold(_)
                ) || matches!(route, ToolRoute::Goal)
                    && name != crate::goal::tools::CREATE_GOAL_TOOL_NAME
                {
                    return Some(crate::host_tools::HostToolPermission::ReadOnly);
                }
                if matches!(route, ToolRoute::Goal) {
                    return Some(crate::host_tools::HostToolPermission::Automatic);
                }
                if matches!(route, ToolRoute::ScheduledTask) {
                    return Some(crate::scheduled_tasks::tools::permission(&name));
                }
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
        let artifact_admission = match self
            .check_artifact_provenance(receipt, normalized_name, normalized_input)
            .await
        {
            Ok(admission) => admission,
            Err(error) => return Ok(ToolCallOutcome::not_admitted(error)),
        };
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
                parent_call_id: crate::nested_calls::parent_call_id(),
                parent_call_index: crate::nested_calls::parent_call_index(),
                task_binding: task_binding.flatten(),
                id: tickloop_ids::generate(),
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
                ancestor_revisions,
                mut session_evaluation,
                execution_mode,
                policy,
            ) = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&receipt.chat_id).unwrap();
                if cancel.is_cancelled() {
                    return Err("run interrupted before host operation".into());
                }
                self.record_nested_operation(receipt.chat_id, &operation);
                chat.snapshot.operations.push(operation.clone());
                self.save(&chat.snapshot);
                let sessions = self.inner.session_permissions.lock().await;
                let session_evaluation = sessions.evaluate(permission_owner, &session_operation);
                let (authority, policy) = delegation::host_authority(
                    &chats,
                    &sessions,
                    receipt,
                    &operation,
                    permission_input,
                    delegation::HostCallPermission {
                        mcp_server: prepared_host_call
                            .as_ref()
                            .and_then(|call| call.mcp_server()),
                        permission: host_permission,
                        local_read: prepared_host_call
                            .as_ref()
                            .and_then(|call| call.local_read()),
                    },
                    session_evaluation.decision,
                )?;
                (
                    if local_read_asks {
                        authority.decision.stricter(Decision::Ask)
                    } else {
                        authority.decision
                    },
                    authority.explicit_ask || local_read_asks,
                    authority.policy_revision,
                    authority.ancestor_revisions,
                    session_evaluation,
                    policy.mode,
                    policy,
                )
            };
            // Plan refuses these host tools before any rule is consulted.
            let is_plan_mode_host_denial = decision == Decision::Deny
                && execution_mode == ExecutionMode::Plan
                && matches!(
                    host_permission,
                    Some(
                        crate::host_tools::HostToolPermission::Automatic
                            | crate::host_tools::HostToolPermission::Reviewable
                            | crate::host_tools::HostToolPermission::Confirmation
                    )
                );
            let decision = match decision {
                Decision::Deny => Decision::Deny,
                _ if name == EXIT_PLAN_MODE_TOOL_NAME => Decision::Allow,
                Decision::Ask
                    if execution_mode == ExecutionMode::Auto
                        && !explicit_ask
                        && name != ASK_USER_QUESTION_TOOL_NAME
                        && !matches!(
                            host_permission,
                            Some(
                                crate::host_tools::HostToolPermission::Confirmation
                                    | crate::host_tools::HostToolPermission::ThirdParty
                            )
                        ) =>
                {
                    Decision::Allow
                }
                other => other,
            };
            let hook_asks = hook_gate.asks() && decision != Decision::Deny;
            let hook_admits = hook_gate.allows()
                && !matches!(decision, Decision::Deny | Decision::Allow)
                && !explicit_ask
                && !local_read_asks
                && !matches!(
                    host_permission,
                    Some(
                        crate::host_tools::HostToolPermission::Confirmation
                            | crate::host_tools::HostToolPermission::Classified {
                                allow_rule_bypasses_review: false
                            }
                    )
                );
            let decision = if hook_asks {
                Decision::Ask
            } else if hook_admits {
                Decision::Allow
            } else {
                decision
            };
            let prompts_unavailable = decision == Decision::Ask
                && self.inner.host_options.prompt_policy == PromptPolicy::DenyOnPrompt;
            let mut decision = self.apply_prompt_policy_keeping_required_approval(
                decision,
                explicit_ask
                    || hook_asks
                    || matches!(
                        host_permission,
                        Some(
                            crate::host_tools::HostToolPermission::Confirmation
                                | crate::host_tools::HostToolPermission::ThirdParty
                        )
                    ),
            );
            // A write to an Artifact with provenance needs no review; rules still decide.
            if decision == Decision::Check
                && artifact_admission == super::web_provenance::ArtifactAdmission::Vouched
            {
                decision = Decision::Allow;
            }
            let mut assessment_details = None;
            let mut approval_source = if explicit_ask {
                PermissionApprovalSource::ExplicitAsk
            } else if hook_asks {
                PermissionApprovalSource::Hook
            } else {
                PermissionApprovalSource::PermissionPolicy
            };
            let mut approval_reason = if hook_asks && !explicit_ask {
                Some(hook_gate.ask_reason(&name))
            } else {
                Some(format!(
                    "{}: {}",
                    if explicit_ask {
                        "an explicit Ask rule requires human approval"
                    } else {
                        "the permission policy requires human approval"
                    },
                    prepared_host_call
                        .as_ref()
                        .map_or("", |call| call.approval_summary()),
                ))
            };
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
                let allow_rule_skips_review = !matches!(
                    host_permission,
                    Some(crate::host_tools::HostToolPermission::Classified {
                        allow_rule_bypasses_review: false
                    })
                );
                let reviewed = self
                    .resolve_auto_assessment(
                        receipt,
                        &operation,
                        &policy,
                        &mut assessment,
                        allow_rule_skips_review,
                    )
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
                            super::PermissionPrompt {
                                source: approval_source,
                                reason: approval_reason,
                                mcp_server: prepared_host_call
                                    .as_ref()
                                    .and_then(|call| call.mcp_server())
                                    .map(str::to_owned),
                            },
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
                } else if !is_rejected_by_user && is_plan_mode_host_denial {
                    self.plan_mode_refusal(receipt).await
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
                        prepared_host_call
                            .as_ref()
                            .and_then(|call| call.mcp_server()),
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
                    let admitted = match self
                        .check_delegated_host_call(
                            receipt,
                            provider.as_ref(),
                            prepared.name(),
                            prepared.source_input(),
                        )
                        .await
                    {
                        Ok(()) => match prepared.local_read() {
                            Some(read) => match self.check_local_read(receipt, read).await {
                                Ok(true) if !local_read_asks => Err(format!(
                                    "{} of {} now needs approval, so the file was not sent; call {name} again to ask",
                                    read.name,
                                    read.input["file_path"].as_str().unwrap_or("the file")
                                )),
                                Ok(_) => Ok(()),
                                Err(error) => Err(error),
                            },
                            None => Ok(()),
                        },
                        Err(error) => Err(error),
                    };
                    match admitted {
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
                                receipt.chat_id.into(),
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
                                    prepared_host_call
                                        .as_ref()
                                        .and_then(|call| call.mcp_server()),
                                )
                                .finish(receipt.chat_id, &operation),
                            );
                        }
                        Err(error)
                    }
                    Ok(()) => {
                        self.refresh_task_binding_setting(receipt)
                            .await
                            .map_err(|error| ToolCallError::from(error.to_string()))?;
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
                                prepared_host_call
                                    .as_ref()
                                    .and_then(|call| call.mcp_server()),
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
                                    mcp_server: prepared_host_call
                                        .as_ref()
                                        .and_then(|call| call.mcp_server()),
                                    tool_name: &name,
                                    expected_task_binding: task_binding,
                                    session_evaluation: &session_evaluation,
                                    policy_revision: revision,
                                    ancestor_revisions: &ancestor_revisions,
                                    host_permission,
                                    local_read: prepared_host_call
                                        .as_ref()
                                        .and_then(|call| call.local_read()),
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
                                    } else if hook_admits {
                                        super::denials::DirectApprovalBasis::Hook
                                    } else if policy.admission_mode() == ExecutionMode::FullAccess {
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
                                prepared_host_call
                                    .as_ref()
                                    .and_then(|call| call.mcp_server()),
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
                            dispatched = true;
                            let host_result = provider
                                .execute_prepared(receipt.chat_id, prepared, cancel.clone())
                                .await;
                            host_call_status = Some(host_result.status());
                            let (mut result, effect) = match host_result {
                                crate::host_tools::HostCallResult::CompletedWithImages(
                                    mut result,
                                    host_images,
                                ) => {
                                    let accepts_images = self
                                        .inner
                                        .chats
                                        .lock()
                                        .await
                                        .get(&receipt.chat_id)
                                        .is_some_and(|chat| chat.snapshot.model.accepts_images());
                                    if accepts_images {
                                        images = host_images;
                                    } else if !host_images.is_empty() {
                                        result.content.push_str("\n[MCP images omitted: this model does not accept images]");
                                    }
                                    (result, None)
                                }
                                other => other.into_parts(),
                            };
                            if crate::tick_tools::is_tick_tool(&name) {
                                result.content =
                                    match serde_json::from_str::<Value>(&result.content) {
                                        Ok(value) => {
                                            crate::tick_tools::model_tick_result(&ids, value)
                                                .to_string()
                                        }
                                        Err(_) => crate::tick_tools::model_tick_text(
                                            &ids,
                                            &result.content,
                                        ),
                                    };
                            }
                            if let Some(unchanged) = self
                                .inner
                                .chats
                                .lock()
                                .await
                                .get(&receipt.chat_id)
                                .and_then(|chat| {
                                    crate::artifact_reads::unchanged_own_page(
                                        &chat.snapshot,
                                        &name,
                                        &input,
                                        &result,
                                    )
                                })
                            {
                                result = unchanged;
                            }
                            if let Some(crate::host_tools::HostEffect::CreatedArtifact(created)) =
                                &effect
                            {
                                self.remember_created_artifact(
                                    receipt.chat_id,
                                    &created.artifact_id,
                                )
                                .await;
                            }
                            host_effect = effect;
                            Ok(result)
                        } else {
                            let dispatch_operation = OperationRecord {
                                name: name.clone(),
                                input: input.clone(),
                                ..operation.clone()
                            };
                            dispatched = true;
                            self.dispatch_registered_tool(
                                receipt,
                                &dispatch_operation,
                                registration
                                    .as_ref()
                                    .expect("registered local or Host tool"),
                                prepared_local.as_ref(),
                                DispatchGrant { watch_authority },
                                cancel.clone(),
                            )
                            .await
                        }
                    }
                }
            }
        } else {
            let (tools, ids) = {
                let chats = self.inner.chats.lock().await;
                let chat = chats.get(&receipt.chat_id).unwrap();
                (chat.tools.clone(), chat.ids)
            };
            let operation_id = tickloop_ids::generate();
            let prepared = if registration.is_some() {
                tools
                    .prepare_for_operation(&ids, &workspace, &name, &input, operation_id)
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
                    if let Err(error) = self.check_web_fetch_provenance(receipt, &prepared).await {
                        return Ok(ToolCallOutcome::not_admitted(error));
                    }
                    let session_operation =
                        SessionPermissionOperation::from_prepared(&prepared, &configured_workspace)
                            .map_err(|error| error.to_string())?;
                    let operation = OperationRecord {
                        parent_call_id: crate::nested_calls::parent_call_id(),
                        parent_call_index: crate::nested_calls::parent_call_index(),
                        task_binding: task_binding.flatten(),
                        id: operation_id,
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
                        self.record_nested_operation(receipt.chat_id, &operation);
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
                        let plan_file = chat.snapshot.folder.as_deref().and_then(|folder| {
                            plan::current_plan_file(
                                &chat.bookkeeping,
                                self.plans_directory(folder).path(),
                                receipt.loop_id,
                            )
                        });
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
                            authority.ancestor_revisions,
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
                            self.plan_mode_refusal(receipt).await
                        } else if let Some(message) = policy.content_denial_message(
                            &prepared.name,
                            &prepared.input,
                            receipt.loop_id.into(),
                            session_evaluation.decision,
                        ) {
                            message
                        } else {
                            "an authorization rule denied this operation".to_owned()
                        });
                    }
                    let hook_asks = hook_gate.asks() && decision != Decision::Deny;
                    let hook_admits = hook_gate.allows()
                        && !matches!(decision, Decision::Deny | Decision::Allow)
                        && !explicit_ask
                        && prepared.safety_check().is_none()
                        && !prepared.requires_environment_approval();
                    if hook_asks {
                        decision = Decision::Ask;
                    } else if hook_admits {
                        decision = Decision::Allow;
                    }
                    let safety_check = (decision == Decision::Ask)
                        .then(|| prepared.safety_check())
                        .flatten();
                    let mut approval_source = if explicit_ask {
                        PermissionApprovalSource::ExplicitAsk
                    } else if hook_asks {
                        PermissionApprovalSource::Hook
                    } else if safety_check.is_some() {
                        PermissionApprovalSource::SafetyCheck
                    } else {
                        PermissionApprovalSource::PermissionPolicy
                    };
                    let mut approval_reason = (decision == Decision::Ask).then(|| {
                        if hook_asks && !explicit_ask {
                            hook_gate.ask_reason(&prepared.name)
                        } else if explicit_ask {
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
                    let approval_required =
                        explicit_ask || hook_asks || prepared.requires_environment_approval();
                    let bypassed_by_unrestricted_host = !approval_required
                        && matches!(decision, Decision::Ask | Decision::Check)
                        && self.inner.host_options.prompt_policy == PromptPolicy::Unrestricted;
                    decision = self
                        .apply_prompt_policy_keeping_required_approval(decision, approval_required);
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
                                        if let Some(binding) = task_binding
                                            && shared_tasks::binding_for_tool(
                                                &chats, receipt, &name,
                                            )
                                            .map_err(ToolCallError::from)?
                                                != binding
                                        {
                                            return Err(
                                                "task execution binding changed before dispatch"
                                                    .into(),
                                            );
                                        }
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
                                    delegated_revision = current.ancestor_revisions;
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
                                        None,
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
                                    true,
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
                                    super::PermissionPrompt {
                                        source: approval_source,
                                        reason: approval_reason,
                                        mcp_server: None,
                                    },
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
                                None,
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
                                    None,
                                )
                                .finish(receipt.chat_id, &operation);
                                return Err(ToolCallError::denied(reason, record));
                            }
                        }
                        self.refresh_task_binding_setting(receipt)
                            .await
                            .map_err(|error| ToolCallError::from(error.to_string()))?;
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
                                None,
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
                            chat.snapshot.folder.as_deref().map_or(Ok(()), |folder| {
                                self.require_working_directory_trust(folder)
                            })
                        {
                            let reason = error.to_string();
                            let record = super::denials::DenialCandidate::new(
                                &operation,
                                &operation.input,
                                DenialCategory::FinalAdmission,
                                final_approval_source,
                                &chat.snapshot.policy,
                                assessment_details.as_ref(),
                                None,
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
                                None,
                            )
                            .finish(receipt.chat_id, &operation);
                            ToolCallError::denied(reason, record)
                        })?;
                        if let Some(binding) = task_binding
                            && shared_tasks::binding_for_tool(&chats, receipt, &name)
                                .map_err(ToolCallError::from)?
                                != binding
                        {
                            return Err("task execution binding changed before dispatch".into());
                        }
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
                        ) || !current.still_admits(policy_revision, &delegated_revision)
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
                                None,
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
                                mcp_server: None,
                                path: approval_path,
                                direct_basis: if prepared.is_read_only {
                                    super::denials::DirectApprovalBasis::ReadOnly
                                } else if hook_admits {
                                    super::denials::DirectApprovalBasis::Hook
                                } else if chat.snapshot.policy.admission_mode()
                                    == ExecutionMode::FullAccess
                                {
                                    super::denials::DirectApprovalBasis::FullAccess
                                } else if bypassed_by_unrestricted_host {
                                    super::denials::DirectApprovalBasis::UnrestrictedHost
                                } else if chat.snapshot.policy.admits_by_isolation(&prepared) {
                                    super::denials::DirectApprovalBasis::Sandbox
                                } else {
                                    super::denials::DirectApprovalBasis::Policy
                                },
                                approval_source: final_approval_source,
                                assessment: assessment_details.as_ref(),
                            },
                        );
                        self.record_approval(chat, receipt, approval);
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
                        let context = tickloop_harness_tools::ExecutionContext {
                            ids: chat.ids,
                            operation_id: operation.id,
                            loop_id: receipt.loop_id.into(),
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
                        dispatched = true;
                        let execution = tools
                            .execute_with_context(
                                prepared,
                                cancel.clone(),
                                context,
                                &secondary_model,
                            )
                            .await;
                        self.remember_fetch_observations(receipt.chat_id, &tools)
                            .await;
                        images = execution.images;
                        match execution.result {
                            Ok(mut result) => {
                                sources = self
                                    .cite_web_result(
                                        receipt.chat_id,
                                        &name,
                                        &mut result,
                                        execution.web_sources,
                                    )
                                    .await?;
                                Ok(result)
                            }
                            Err(error) => Err(error.to_string()),
                        }
                    }
                }
                Err(error) => {
                    preparation_failed = registration.is_some();
                    Err(error)
                }
            }
        };
        let result = result.unwrap_or_else(|error| ToolResult {
            content: error,
            is_error: true,
            output_ref: None,
        });
        if sources.is_none()
            && self.inner.citation_style == crate::CitationStyle::Numbered
            && matches!(
                name.as_str(),
                tickloop_harness_tools::WEB_SEARCH_TOOL_NAME
                    | tickloop_harness_tools::WEB_FETCH_TOOL_NAME
            )
        {
            sources = Some(Vec::new());
        }
        let hook_feedback = if (dispatched || preparation_failed) && !cancel.is_cancelled() {
            self.post_tool_use(receipt, &id, &name, &input, &result, &cancel)
                .await
        } else {
            Default::default()
        };
        Ok(ToolCallOutcome {
            result,
            sources,
            images,
            host_call_status,
            host_effect,
            permission_comment,
            continue_after_denial,
            is_rejected_by_user,
            denial,
            hook_feedback,
        })
    }

    async fn resolve_auto_assessment(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        policy: &PermissionPolicy,
        assessment: &mut approval_assessment::ApprovalAssessment,
        allow_rule_skips_review: bool,
    ) -> Result<AutoPermissionDecision, ToolCallError> {
        let decision;
        let mut approval_reason = None;
        let mut auto_denial_message = None;
        match assessment.verdict.clone() {
            AutoReview::Allow => decision = Decision::Allow,
            AutoReview::Ask { reason, .. } => {
                decision = self.review_escalation();
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
                        None,
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
                decision = self.review_escalation();
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
                        .push(tickloop_harness_protocol::ApprovalDiagnostic::DenialLimit);
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
                            None,
                        )
                        .finish(receipt.chat_id, operation);
                        crate::nested_calls::stop_after_denial(&reason);
                        return Err(ToolCallError::denied(reason, record));
                    }
                    Some(warning) if kind == AutoBlockKind::SoftDeny => {
                        decision = Decision::Ask;
                        approval_reason =
                            Some(format!("{warning}\n\nLatest blocked action: {reason}"));
                    }
                    _ => {
                        decision = Decision::Deny;
                        auto_denial_message = Some(auto_mode::rejection_message(
                            &reason,
                            allow_rule_skips_review.then_some(operation.name.as_str()),
                        ));
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
        if decision == Decision::Deny && matches!(assessment.verdict, AutoReview::Block { .. }) {
            crate::nested_calls::stop_after_denial(
                auto_denial_message
                    .as_deref()
                    .unwrap_or("Auto review rejected the call"),
            );
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

    #[tokio::test]
    async fn captured_registry_keeps_search_listed_whether_search_is_on_or_off() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            tickloop_harness_test_support::config(),
            Arc::new(tickloop_harness_test_support::ScriptedModel::new(vec![])),
        );
        let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
        {
            let mut chats = agent.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).unwrap();
            let mut registry = ToolRegistry::for_chat(
                chat,
                None,
                false,
                PromptPolicy::Interactive,
                shared_tasks::TaskToolAccess::Disabled,
                &crate::ToolPolicy::local(),
                crate::scheduled_tasks::tools::ToolTerms::DEFAULT,
            );
            let listed = registry.definitions();
            for enabled in [false, true, false] {
                chat.snapshot.search_config =
                    enabled.then(tickloop_harness_protocol::SearchConfig::default);
                registry.refresh(chat);
                assert!(registry.resolve("WebSearch").is_some());
                assert!(registry.resolve("WebFetch").is_some());
                assert_eq!(
                    serde_json::to_vec(&registry.definitions()).unwrap(),
                    serde_json::to_vec(&listed).unwrap()
                );
            }
        }
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn skill_catalog_tracks_the_active_model_context_and_learned_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let description = "Long workflow description "
            .repeat(30)
            .trim_end()
            .to_owned();
        let agent = Agent::new_embedded(
            tickloop_harness_test_support::config_with_window(8_192),
            Arc::new(tickloop_harness_test_support::ScriptedModel::new(vec![])),
        );
        let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
        let mut chats = agent.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).unwrap();
        let mut skills = tickloop_harness_policy::skills::SkillRegistry::builtin();
        skills.register_markdown(
            &format!("---\nname: catalog-workflow\ndescription: {description}\n---\nPrivate body"),
            tickloop_harness_protocol::SkillSource::Project {
                path: directory
                    .path()
                    .join(".tickloop/skills/catalog-workflow/SKILL.md"),
            },
            &[],
        );
        chat.skills = Arc::new(skills);
        let catalog = |chat: &Chat| {
            ToolRegistry::for_chat(
                chat,
                None,
                false,
                PromptPolicy::Interactive,
                shared_tasks::TaskToolAccess::Disabled,
                &crate::ToolPolicy::local(),
                crate::scheduled_tasks::tools::ToolTerms::DEFAULT,
            )
            .resolve(tickloop_harness_policy::skills::SKILL_TOOL_NAME)
            .unwrap()
            .definition
            .description
        };
        let small = catalog(chat);
        assert!(small.contains("catalog-workflow"));
        assert!(!small.contains(&description));

        chat.snapshot.model = tickloop_harness_test_support::config_with_window(1_000_000);
        chat.snapshot.context_window_tokens = Some(8_192);
        assert!(catalog(chat).contains(&description));

        chat.snapshot.model = tickloop_harness_test_support::config();
        assert!(!catalog(chat).contains(&description));
        chat.snapshot.context_window_tokens = Some(1_000_000);
        assert!(catalog(chat).contains(&description));
    }

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
        let skills = tickloop_harness_policy::skills::SkillRegistry::builtin();
        tickloop_harness_tools::Tools::definitions()
            .into_iter()
            .chain(
                [(false, false), (false, true), (true, false)]
                    .into_iter()
                    .flat_map(|(is_delegated, offers_subagent)| {
                        plan::definitions(is_delegated, offers_subagent)
                    }),
            )
            .chain([false, true].into_iter().map(interaction::definition))
            .chain(
                [
                    crate::SubagentLaunch::AlwaysBackground,
                    crate::SubagentLaunch::BackgroundByDefault,
                ]
                .into_iter()
                .flat_map(delegation::definitions),
            )
            .chain(agent_messages::definitions())
            .chain([crate::handback::definition()])
            .chain(crate::scheduled_tasks::tools::definitions(
                crate::scheduled_tasks::tools::ToolTerms::DEFAULT,
                true,
            ))
            .chain(tick_subscriptions::definitions())
            .chain(skills.tool_definition())
            .chain(crate::mcp_resources::definitions())
            .chain(crate::tick_tools::definitions())
            .chain([
                crate::mcp_tools::login_definition(),
                crate::mcp_tools::upload_definition(),
            ])
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
            "ReadMcpResourceTool",
            "ListMcpResourcesTool",
            "Subagent",
            "TickDescribe",
            "TickRead",
            "TickCall",
            "TickManage",
            "ArtifactLogin",
            "ArtifactUpload",
            "CronCreate",
            "ScheduleWakeup",
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

    /// The built-in Skills name a tool by its compound name, such as `TickManage`; each such name
    /// must be a tool the model can be given.
    #[test]
    fn shipped_skills_name_only_tools_the_agent_offers() {
        const NOT_TOOLS: [&str; 3] = ["OutcomeUnknown", "TickUpdate", "CallToolResult"];
        let tools = built_in_definitions()
            .into_iter()
            .chain(crate::tool_exposure::fold_definitions())
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
        let named = tickloop_harness_policy::skills::BUILTIN_SKILLS
            .into_iter()
            .flat_map(|skill| skill.split(|character: char| !character.is_ascii_alphanumeric()))
            .filter(is_compound_name)
            .collect::<HashSet<_>>();
        assert!(named.is_superset(&HashSet::from(
            ["TickManage", "TaskStop", "ScheduleWakeup",]
        )));
        for word in named {
            assert!(
                tools.contains(word) || NOT_TOOLS.contains(&word),
                "a shipped Skill names {word}, which is no tool"
            );
        }
    }
}
