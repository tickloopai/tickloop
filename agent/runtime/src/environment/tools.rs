use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::{InstanceRef, TickDescriptor};
use tickloop_agent_protocol::{
    ChatId, HostEndpointIdentity, HostToolAuthority, ToolDefinition, ToolResult,
};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
    host_tools::{HostCallScope, HostToolPermission, HostToolProvider},
    tick_tools::{TICK_CALL, TICK_DESCRIBE, TICK_MANAGE, TICK_READ},
};

/// Generic Tick tools, available for any execution environment with Tick support.
pub struct ExecutionTools {
    environment: Arc<dyn ExecutionEnvironment>,
    authority: HostToolAuthority,
}

impl ExecutionTools {
    pub fn new(environment: Arc<dyn ExecutionEnvironment>, authority: HostToolAuthority) -> Self {
        Self {
            environment,
            authority,
        }
    }
}

#[async_trait]
impl HostToolProvider for ExecutionTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        crate::tick_tools::definitions(false)
    }
    fn is_concurrency_safe(&self, name: &str, _: &Value) -> bool {
        matches!(name, TICK_READ | TICK_DESCRIBE)
    }
    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        crate::tick_tools::validate_input(name, schema, input, false)
    }
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        Ok(match &self.authority {
            HostToolAuthority::Native { endpoint } => Some(endpoint.clone()),
            _ => None,
        })
    }
    fn tool_authorities(&self, _: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        Ok(BTreeSet::from([self.authority.clone()]))
    }
    fn call_authority(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<Option<HostToolAuthority>, String> {
        crate::tick_tools::check_call(name, input, false)?;
        Ok(Some(self.authority.clone()))
    }
    fn permission(&self, _: ChatId, name: &str, _: &Value) -> HostToolPermission {
        if matches!(name, TICK_READ | TICK_DESCRIBE) {
            HostToolPermission::ReadOnly
        } else {
            HostToolPermission::Reviewable
        }
    }
    async fn prepare_call(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.prepare_call_in_scope(
            chat,
            &HostCallScope {
                local_files: None,
                context: None,
            },
            name,
            input,
            cancel,
        )
        .await
    }
    async fn prepare_call_in_scope(
        &self,
        chat: ChatId,
        scope: &HostCallScope<'_>,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let authority = ExecutionAuthority::with_cancellation(chat, cancel);
        let port = self
            .environment
            .bind(
                ExecutionScope {
                    chat,
                    folder: scope
                        .context
                        .map(|context| context.folder.to_string_lossy().into_owned()),
                },
                authority.clone(),
            )
            .map_err(|error| error.to_string())?;
        prepare_on_port(port, authority, chat, name, input, false).await
    }
    async fn revalidate_call(
        &self,
        chat: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("hosted Tick operation was cancelled before dispatch".into());
        }
        revalidate_call(chat, call).await
    }
    async fn execute_prepared(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        execute_call(chat, call, cancel).await
    }
    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        ToolResult {
            content: "hosted Tick calls require preparation".into(),
            is_error: true,
            output_ref: None,
        }
    }
}

pub(super) async fn prepare_on_port(
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    chat: ChatId,
    name: &str,
    input: &Value,
    views: bool,
) -> Result<PreparedHostCall, String> {
    crate::tick_tools::check_call(name, input, views)?;
    let instance = || {
        serde_json::from_value::<InstanceRef>(input["tick_instance_id"].clone())
            .map_err(|error| error.to_string())
    };
    if matches!(name, TICK_READ | TICK_DESCRIBE) {
        let instance = instance()?;
        let description = name == TICK_DESCRIBE;
        return Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            input.clone(),
            HostToolPermission::ReadOnly,
            format!(
                "{} hosted Tick {}",
                if description { "Describe" } else { "Read" },
                instance.public()
            ),
        )
        .with_payload(EnvironmentReadCall {
            port,
            chat,
            instance,
            description,
        }));
    }
    let intent = match (name, input["action"].as_str()) {
        (TICK_MANAGE, Some("close")) => ExecutionIntent::Close {
            instance: instance()?,
        },
        (TICK_MANAGE, Some("reopen")) => ExecutionIntent::Reopen {
            instance: instance()?,
        },
        (TICK_MANAGE, Some("send_input")) => {
            let instance = instance()?;
            let keys = input
                .get("keys")
                .map(|keys| {
                    serde_json::from_value::<Vec<String>>(keys.clone())
                        .map_err(|error| error.to_string())
                })
                .transpose()?
                .unwrap_or_default();
            let data = crate::hosted::terminal_input::encode(
                input.get("text").and_then(Value::as_str),
                &keys,
            )?;
            ExecutionIntent::SendInput { instance, data }
        }
        (TICK_MANAGE, Some("open")) => {
            let command = serde_json::from_value(input["command"].clone())
                .map_err(|error| format!("invalid command: {error}"))?;
            let view = match input["placement"]["type"].as_str() {
                None | Some("hidden") => None,
                Some(_) => Some(ViewPlacement {
                    workspace_id: input["workspace_id"].as_str().map(str::to_owned),
                    placement: serde_json::from_value(input["placement"].clone())
                        .map_err(|error| error.to_string())?,
                }),
            };
            ExecutionIntent::Open(OpenSpec {
                command,
                cwd: input["cwd"].as_str().map(str::to_owned),
                protocol: input["protocol"].as_bool().unwrap_or(false),
                view,
            })
        }
        (TICK_CALL, _) => {
            let instance = instance()?;
            if matches!(
                port.inspect(ExecutionQuery::Kind(instance.clone()))
                    .await
                    .map_err(|error| error.to_string())?,
                ExecutionInspection::Kind(ExecutionKind::Terminal)
            ) {
                return Err(format!(
                    "UnsupportedCapability: {instance} is a terminal Tick and declares no tools; read it with TickRead and type into it with TickManage send_input"
                ));
            }
            let ExecutionInspection::Description(catalog) = port
                .inspect(ExecutionQuery::Describe(instance.clone()))
                .await
                .map_err(|error| error.to_string())?
            else {
                return Err("missing Tick catalog".into());
            };
            let catalog: TickDescriptor =
                serde_json::from_value(catalog).map_err(|error| error.to_string())?;
            let tool = input["tool"].as_str().ok_or("tool is required")?.to_owned();
            let args = input["args"].clone();
            let declaration = catalog
                .tools
                .iter()
                .find(|declared| declared.name == tool)
                .ok_or_else(|| format!("{instance} declares no tool {tool:?}"))?;
            jsonschema::validator_for(&declaration.input_schema)
                .map_err(|error| error.to_string())?
                .validate(&args)
                .map_err(|error| format!("invalid args for {tool}: {error}"))?;
            ExecutionIntent::Call {
                instance,
                tool,
                args,
                revision: catalog.revision,
            }
        }
        _ => return Err("unsupported execution operation".into()),
    };
    let prepared = port.prepare(intent).await.map_err(|error| error.message)?;
    let facts = prepared.facts();
    Ok(PreparedHostCall::new(
        name.into(),
        input.clone(),
        facts.normalized_input.clone(),
        facts.permission,
        facts.summary.clone(),
    )
    .with_approval_facts(facts.approval.clone())
    .with_payload(EnvironmentPreparedCall {
        port,
        prepared,
        authority,
        chat,
    }))
}

struct EnvironmentReadCall {
    port: Arc<dyn ExecutionPort>,
    chat: ChatId,
    instance: InstanceRef,
    description: bool,
}

pub(crate) fn is_environment_call(call: &PreparedHostCall) -> bool {
    call.payload::<EnvironmentPreparedCall>().is_some()
        || call.payload::<EnvironmentReadCall>().is_some()
}

pub(crate) async fn revalidate_call(chat: ChatId, call: &PreparedHostCall) -> Result<(), String> {
    if let Some(call) = call.payload::<EnvironmentPreparedCall>() {
        return call.revalidate(chat).await;
    }
    let call = call
        .payload::<EnvironmentReadCall>()
        .ok_or("hosted Tick operation has no prepared binding")?;
    if chat != call.chat {
        return Err("preparation belongs to another Chat".into());
    }
    match call
        .port
        .inspect(ExecutionQuery::Kind(call.instance.clone()))
        .await
        .map_err(|error| error.to_string())?
    {
        ExecutionInspection::Kind(_) => Ok(()),
        _ => Err("Tick kind changed after preparation".into()),
    }
}

pub(crate) async fn execute_call(
    chat: ChatId,
    call: PreparedHostCall,
    cancel: CancellationToken,
) -> HostCallResult {
    if cancel.is_cancelled() {
        return HostCallResult::NotDispatched(
            "hosted Tick operation was cancelled before dispatch".into(),
        );
    }
    if call.payload::<EnvironmentPreparedCall>().is_some() {
        return call
            .into_payload::<EnvironmentPreparedCall>()
            .expect("checked prepared type")
            .execute(chat, cancel)
            .await;
    }
    let Ok(call) = call.into_payload::<EnvironmentReadCall>() else {
        return HostCallResult::NotDispatched(
            "hosted Tick operation has no prepared binding".into(),
        );
    };
    if chat != call.chat {
        return HostCallResult::NotDispatched("preparation belongs to another Chat".into());
    }
    let kind = match call
        .port
        .inspect(ExecutionQuery::Kind(call.instance.clone()))
        .await
    {
        Ok(ExecutionInspection::Kind(kind)) => kind,
        Ok(_) => return HostCallResult::NotDispatched("missing Tick kind".into()),
        Err(error) => return HostCallResult::NotDispatched(error.to_string()),
    };
    let query = if call.description {
        ExecutionQuery::Describe(call.instance.clone())
    } else {
        ExecutionQuery::Read(call.instance.clone())
    };
    let inspected = match call.port.inspect(query).await {
        Ok(inspected) => inspected,
        Err(error) => return HostCallResult::NotDispatched(error.to_string()),
    };
    let value = match inspected {
        ExecutionInspection::Description(descriptor) => {
            json!({"tick_instance_id":call.instance, "source":if kind == ExecutionKind::Protocol { "protocol" } else { "terminal" }, "descriptor":descriptor})
        }
        ExecutionInspection::Read(read) => {
            let mut value = json!({"tick_instance_id":call.instance, "source":if read.kind == ExecutionKind::Protocol { "protocol" } else { "terminal" }, "seq":read.state.seq, "at":read.state.at, "data":read.state.data});
            if read.kind == ExecutionKind::Terminal {
                value["truncated"] = json!(read.truncated);
            }
            value
        }
        _ => return HostCallResult::NotDispatched("unexpected Tick inspection result".into()),
    };
    HostCallResult::Completed(ToolResult {
        content: value.to_string(),
        is_error: false,
        output_ref: None,
    })
}
