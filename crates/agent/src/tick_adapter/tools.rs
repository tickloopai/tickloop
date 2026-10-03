use super::*;
use crate::{
    host_tools::{
        BuiltinToolCall, HostCallContext, HostCallResult, HostEndpointIdentity, HostToolPermission,
        PreparedHostCall, WorkspaceHostContext,
    },
    model::ToolDefinition,
    runtime::{Agent, ChatId, ShellTask, ShellTaskCenter},
    tools::ToolResult,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

pub(super) struct AdapterTools {
    adapters: TickAdapters,
    native: Option<Arc<dyn HostToolProvider>>,
}
struct BoundCall {
    provider: Arc<dyn HostToolProvider>,
    call: PreparedHostCall,
}
impl AdapterTools {
    pub(super) fn new(adapters: TickAdapters, native: Option<Arc<dyn HostToolProvider>>) -> Self {
        Self { adapters, native }
    }
    fn provider(&self, name: &str, input: &Value) -> Result<Arc<dyn HostToolProvider>, String> {
        if matches!(name, "ReadTick" | "CallTickTool") {
            let instance: InstanceRef = serde_json::from_value(input["tick_instance_id"].clone())
                .map_err(|error| error.to_string())?;
            self.adapters.provider(&instance)
        } else {
            self.native
                .clone()
                .ok_or_else(|| "Native host unavailable".into())
        }
    }
    async fn prepare_bound(
        &self,
        chat: ChatId,
        context: Option<&HostCallContext>,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let provider = self.provider(name, input)?;
        let call = match context {
            Some(context) => {
                provider
                    .prepare_call_in_context(chat, context, name, input, cancel)
                    .await?
            }
            None => provider.prepare_call(chat, name, input, cancel).await?,
        };
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            call.input().clone(),
            call.permission(),
            call.approval_summary().into(),
        )
        .with_permission_input(call.permission_input().clone())
        .with_policy_identity(call.policy_name().to_owned(), call.policy_input().clone())
        .with_payload(BoundCall { provider, call }))
    }
}
pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition { name: "ReadTick".into(), description: "Read a Tick Instance's current content and declared query/action catalog. Reading does not subscribe.".into(), input_schema: json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":3},"workspace_id":{"type":"string","minLength":1}},"required":["tick_instance_id"],"additionalProperties":false}) },
        ToolDefinition { name: "CallTickTool".into(), description: "Call a query or action declared by a Tick Adapter. Read its catalog first; arguments and permissions are checked before dispatch.".into(), input_schema: json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":3},"workspace_id":{"type":"string","minLength":1},"tool":{"type":"string","minLength":1},"args":{"type":"object"}},"required":["tick_instance_id","tool","args"],"additionalProperties":false}) },
    ]
}
#[async_trait]
impl HostToolProvider for AdapterTools {
    async fn builtin_alias(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
    ) -> Result<Option<BuiltinToolCall>, String> {
        self.provider(name, input)?
            .builtin_alias(chat, name, input)
            .await
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .native
            .as_ref()
            .map(|provider| provider.definitions())
            .unwrap_or_default();
        definitions.retain(|tool| !matches!(tool.name.as_str(), "ReadTick" | "CallTickTool"));
        definitions.extend(self::definitions());
        definitions
    }
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        self.native
            .as_ref()
            .map(|native| native.host_endpoint())
            .unwrap_or(Ok(None))
    }
    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        self.native
            .as_ref()
            .and_then(|native| native.workspace_host_context())
    }
    fn permission(&self, chat: ChatId, name: &str, input: &Value) -> HostToolPermission {
        self.provider(name, input)
            .map(|provider| provider.permission(chat, name, input))
            .unwrap_or(HostToolPermission::Reviewable)
    }
    async fn prepare_call(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.prepare_bound(chat, None, name, input, cancel).await
    }
    async fn prepare_call_in_context(
        &self,
        chat: ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.prepare_bound(chat, Some(context), name, input, cancel)
            .await
    }
    async fn revalidate_call(
        &self,
        chat: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let bound = call
            .payload::<BoundCall>()
            .ok_or("Tick Adapter call lost its binding")?;
        bound
            .provider
            .revalidate_call(chat, &bound.call, cancel)
            .await
    }
    async fn execute_prepared(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        match call.into_payload::<BoundCall>() {
            Ok(bound) => {
                bound
                    .provider
                    .execute_prepared(chat, bound.call, cancel)
                    .await
            }
            Err(_) => HostCallResult::NotDispatched("Tick Adapter call lost its binding".into()),
        }
    }
    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        error("Tick Adapter calls require preparation")
    }
}

const DEFAULT_OUTPUT_LIMIT: usize = 4096;

pub(super) struct ShellTools {
    center: ShellTaskCenter,
}
enum ShellOperation {
    Read,
    Output { offset: u64, limit: usize },
    Stop,
}
struct ShellCall {
    task: Uuid,
    operation: ShellOperation,
}
impl ShellTools {
    pub(super) fn new(agent: Agent) -> Self {
        Self {
            center: ShellTaskCenter::new(agent),
        }
    }
    async fn task(&self, chat: ChatId, id: Uuid) -> Result<ShellTask, String> {
        self.center
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?
            .tasks
            .into_iter()
            .find(|task| task.id == id && task.chat_id == chat)
            .ok_or_else(|| "Shell Tick is unavailable in this Chat".into())
    }
    async fn resolve(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
    ) -> Result<(ShellTask, ShellOperation), String> {
        let instance: InstanceRef = serde_json::from_value(input["tick_instance_id"].clone())
            .map_err(|error| error.to_string())?;
        let id = Uuid::parse_str(instance.id()).map_err(|error| error.to_string())?;
        let task = self.task(chat, id).await?;
        if name == "ReadTick" {
            return Ok((task, ShellOperation::Read));
        }
        if name != "CallTickTool" {
            return Err("unknown Shell Tick tool".into());
        }
        let tool = input["tool"].as_str().ok_or("Tick Tool name is required")?;
        let catalog = shell_catalog(&task);
        let definition = catalog
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == tool)
            .ok_or("Shell Tick Tool is unavailable; read its catalog")?;
        self.validate_input(tool, &definition["input_schema"], &input["args"])?;
        let operation = match tool {
            "output" => ShellOperation::Output {
                offset: input["args"]["offset"].as_u64().unwrap_or(0),
                limit: input["args"]["limit"]
                    .as_u64()
                    .unwrap_or(DEFAULT_OUTPUT_LIMIT as u64) as usize,
            },
            "stop" => ShellOperation::Stop,
            _ => return Err("unknown Shell Tick catalog entry".into()),
        };
        Ok((task, operation))
    }
}
fn shell_catalog(task: &ShellTask) -> Value {
    let mut catalog = vec![
        json!({"name":"output","mutates":false,"description":"Read this task's output with byte offset and limit","input_schema":{"type":"object","properties":{"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":crate::runtime::SHELL_TASK_OUTPUT_MAX_BYTES}},"additionalProperties":false}}),
    ];
    if task.capabilities.can_stop {
        catalog.push(json!({"name":"stop","mutates":true,"description":"Stop this task using the existing TaskStop behavior","input_schema":{"type":"object","properties":{},"additionalProperties":false}}));
    }
    json!(catalog)
}
#[async_trait]
impl HostToolProvider for ShellTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self::definitions()
    }
    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }
    async fn builtin_alias(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
    ) -> Result<Option<BuiltinToolCall>, String> {
        if name != "CallTickTool" || input["tool"] != "stop" {
            return Ok(None);
        }
        let (task, operation) = self.resolve(chat, name, input).await?;
        if !matches!(operation, ShellOperation::Stop) {
            return Err("invalid Shell stop alias".into());
        }
        Ok(Some(BuiltinToolCall {
            name: "TaskStop".into(),
            input: json!({"task_id":task.id}),
        }))
    }
    async fn prepare_call(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if cancel.is_cancelled() {
            return Err("Tick call cancelled before preparation".into());
        }
        let (task, operation) = self.resolve(chat, name, input).await?;
        if matches!(operation, ShellOperation::Stop) {
            return Err("Shell stop must execute through the built-in TaskStop policy and ownership boundary".into());
        }
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            input.clone(),
            HostToolPermission::ReadOnly,
            format!("Read Shell task {}: {}", task.id, task.command),
        )
        .with_payload(ShellCall {
            task: task.id,
            operation,
        }))
    }
    async fn revalidate_call(
        &self,
        chat: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("Tick call cancelled before dispatch".into());
        }
        let bound = call
            .payload::<ShellCall>()
            .ok_or("Shell Tick call lost its binding")?;
        self.task(chat, bound.task).await?;
        Ok(())
    }
    async fn execute_prepared(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if let Err(error) = self.revalidate_call(chat, &call, cancel.clone()).await {
            return HostCallResult::NotDispatched(error);
        }
        let bound = call
            .into_payload::<ShellCall>()
            .expect("validated Shell Tick binding");
        let task = match self.task(chat, bound.task).await {
            Ok(task) => task,
            Err(error) => return HostCallResult::NotDispatched(error),
        };
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched(
                "Shell Tick call cancelled before dispatch".into(),
            );
        }
        let result = match bound.operation {
            ShellOperation::Read => Ok(
                json!({"tick_instance_id":InstanceRef::shell(task.id),"task":task,"tools":shell_catalog(&task)}),
            ),
            ShellOperation::Output { offset, limit } => self
                .center
                .read_output(chat, task.id, offset, limit)
                .await
                .map(|chunk| json!(chunk))
                .map_err(|error| error.to_string()),
            ShellOperation::Stop => {
                return HostCallResult::NotDispatched(
                    "Shell stops require built-in TaskStop dispatch".into(),
                );
            }
        };
        HostCallResult::Completed(result.map(success).unwrap_or_else(error))
    }
    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        error("Shell Tick calls require preparation")
    }
}

pub(super) struct ArtifactTools {
    root: PathBuf,
}
struct ArtifactCall {
    provider: Arc<dyn HostToolProvider>,
    call: PreparedHostCall,
    catalog: Option<Value>,
}
impl ArtifactTools {
    pub(super) fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
#[async_trait]
impl HostToolProvider for ArtifactTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self::definitions()
    }
    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }
    async fn prepare_call(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let instance: InstanceRef = serde_json::from_value(input["tick_instance_id"].clone())
            .map_err(|error| error.to_string())?;
        let (origin, id) = instance
            .artifact_parts()
            .ok_or("invalid Artifact Tick identity")?;
        let provider: Arc<dyn HostToolProvider> =
            Arc::new(crate::mcp_tools::McpHostToolProvider::new(
                crate::artifacts::ArtifactClient::for_origin(&self.root, origin)
                    .map_err(|error| error.to_string())?,
                None,
            ));
        provider.refresh(cancel.clone()).await?;
        let tools = provider
            .definitions()
            .into_iter()
            .filter(|tool| {
                tool.input_schema["properties"].get("artifact_id").is_some()
                    && provider.permission(chat, &tool.name, &json!({"artifact_id":id}))
                        == HostToolPermission::ReadOnly
            })
            .collect::<Vec<_>>();
        let tool = if name == "ReadTick" {
            "artifact_read"
        } else if name == "CallTickTool" {
            input["tool"].as_str().ok_or("Tick Tool name is required")?
        } else {
            return Err("unknown Artifact Tick tool".into());
        };
        let remote_name = format!("mcp__artifacts__{tool}");
        let definition = tools
            .iter()
            .find(|tool| tool.name == remote_name)
            .ok_or("Artifact query unavailable; read its catalog")?;
        let mut args = if name == "ReadTick" {
            json!({})
        } else {
            input["args"].clone()
        };
        if !args.is_object() {
            return Err("Tick Tool arguments must be an object".into());
        }
        if args.get("artifact_id").is_some_and(|value| value != id) {
            return Err("Artifact Tick Tool cannot target another instance".into());
        }
        args["artifact_id"] = json!(id);
        provider.validate_input(&remote_name, &definition.input_schema, &args)?;
        let call = provider
            .prepare_call(chat, &remote_name, &args, cancel)
            .await?;
        let catalog = (name == "ReadTick").then(|| json!(tools.into_iter().map(|tool| {
            let mut schema = tool.input_schema;
            schema["properties"].as_object_mut().unwrap().remove("artifact_id");
            if let Some(required) = schema["required"].as_array_mut() { required.retain(|field| field != "artifact_id"); }
            json!({"name":tool.name.trim_start_matches("mcp__artifacts__"),"description":tool.description,"mutates":false,"input_schema":schema})
        }).collect::<Vec<_>>()));
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            input.clone(),
            call.permission(),
            call.approval_summary().into(),
        )
        .with_permission_input(call.permission_input().clone())
        .with_policy_identity(call.policy_name().to_owned(), call.policy_input().clone())
        .with_payload(ArtifactCall {
            provider,
            call,
            catalog,
        }))
    }
    async fn revalidate_call(
        &self,
        chat: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let bound = call
            .payload::<ArtifactCall>()
            .ok_or("Artifact Tick call lost its binding")?;
        bound
            .provider
            .revalidate_call(chat, &bound.call, cancel)
            .await
    }
    async fn execute_prepared(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        let Ok(bound) = call.into_payload::<ArtifactCall>() else {
            return HostCallResult::NotDispatched("Artifact Tick call lost its binding".into());
        };
        let result = bound
            .provider
            .execute_prepared(chat, bound.call, cancel)
            .await;
        if let Some(catalog) = bound.catalog {
            let result = result.into_tool_result();
            HostCallResult::Completed(ToolResult {
                content: json!({"content":result.content,"tools":catalog}).to_string(),
                ..result
            })
        } else {
            result
        }
    }
    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        error("Artifact Tick calls require preparation")
    }
}
fn success(value: Value) -> ToolResult {
    ToolResult {
        content: value.to_string(),
        is_error: false,
        output_ref: None,
    }
}
fn error(message: impl ToString) -> ToolResult {
    ToolResult {
        content: message.to_string(),
        is_error: true,
        output_ref: None,
    }
}
