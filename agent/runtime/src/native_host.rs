use std::{
    os::unix::fs::{FileTypeExt, MetadataExt},
    sync::Arc,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickhost_connect::access::{
    AccessContext, AccessPermission, PreparedOperation, TickAccess, TickLocation,
};
use tokio_util::sync::CancellationToken;

use crate::{
    host_tools::{
        HostCallResult, HostEffect, HostToolPermission, HostToolProvider, PreparedHostCall,
        WorkspaceHostContext, herdr_adapter_pane,
    },
    tick_tool_router::TickBackends,
};
use tickloop_agent_protocol::{
    ChatId, HostCallContext, HostEndpointIdentity, ToolDefinition, ToolResult,
};

/// The Herdr backend of the native Tick tools: Workspace discovery and the Panes it lists or
/// opens visibly. A worker installs it behind [`crate::tick_tool_router::TickToolRouter`].
pub struct NativeHostToolProvider {
    access: Arc<TickAccess>,
    trusted_adapter_scope: Option<uuid::Uuid>,
}

struct NativePreparedCall {
    operation: PreparedOperation,
    endpoint: Option<HostEndpointIdentity>,
}

impl NativeHostToolProvider {
    pub fn new(access: Arc<TickAccess>) -> Self {
        Self {
            access,
            trusted_adapter_scope: None,
        }
    }

    /// An embedding caller owns this adapter's trust boundary and must reuse its scope only for the same endpoint.
    pub fn with_trusted_adapter_identity(access: Arc<TickAccess>, scope: uuid::Uuid) -> Self {
        Self {
            access,
            trusted_adapter_scope: Some(scope),
        }
    }

    pub fn access(&self) -> &Arc<TickAccess> {
        &self.access
    }

    async fn prepare_native_call(
        &self,
        name: &str,
        input: &Value,
        context: Option<&HostCallContext>,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if cancel.is_cancelled() {
            return Err("Native host operation was cancelled before preparation".into());
        }
        TickBackends::HERDR.route(name, input)?;
        let endpoint = self.host_endpoint()?;
        let operation_name = match name {
            "ReadWorkspace" => "read_workspace",
            "ReadTick" => "read_tick",
            "CallTickTool" => "call_tick_tool",
            "ModifyTick" => "modify_tick",
            _ => return Err(format!("unknown Native host tool: {name}")),
        };
        let prepared = match context {
            Some(context) => {
                let is_relative = input["placement"]["type"] == "current_tab"
                    || (name == "ModifyTick"
                        && input["action"] == "move"
                        && input.get("placement").is_none());
                let interaction = context
                    .interaction
                    .as_ref()
                    .filter(|source| source.socket == self.access.context().socket);
                if is_relative && context.interaction.is_some() && interaction.is_none() {
                    return Err("interaction View belongs to a different host instance".into());
                }
                self.access
                    .prepare_in_context(
                        operation_name,
                        input,
                        &AccessContext {
                            workdir: context.workdir.clone(),
                            workspace_id: context.workspace_id.clone(),
                            interaction: interaction
                                .map(|source| herdr_adapter_pane(source.pane.clone())),
                        },
                    )
                    .await
            }
            None => self.access.prepare(operation_name, input).await,
        }
        .map_err(|error| error.to_string())?;
        if self.host_endpoint()? != endpoint {
            return Err("Native host endpoint changed during preparation".into());
        }
        let permission = match prepared.permission {
            AccessPermission::ReadOnly => HostToolPermission::ReadOnly,
            AccessPermission::Confirmation => HostToolPermission::Reviewable,
        };
        Ok(PreparedHostCall::new(
            name.to_owned(),
            input.clone(),
            prepared.input.clone(),
            permission,
            prepared.summary.clone(),
        )
        .with_payload(NativePreparedCall {
            operation: prepared,
            endpoint,
        }))
    }
}

fn target_directory(target: &TickLocation) -> &str {
    target
        .pane
        .foreground_cwd
        .as_deref()
        .or(target.pane.cwd.as_deref())
        .unwrap_or("")
}

/// The command line of a prepared open, used as the Monitor title until the Tick reports one.
fn opened_program_title(operation: &PreparedOperation) -> Option<String> {
    (operation.name == "modify_tick" && operation.input["action"] == "open")
        .then(|| {
            operation.input["command"]
                .as_array()?
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()
                .map(|words| words.join(" "))
        })
        .flatten()
}

fn prepared_binding(call: &PreparedHostCall) -> Result<&NativePreparedCall, String> {
    call.payload::<NativePreparedCall>()
        .ok_or_else(|| "Native host operation has no prepared binding".into())
}

#[async_trait]
impl HostToolProvider for NativeHostToolProvider {
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        if let Some(scope) = self.trusted_adapter_scope {
            return Ok(Some(HostEndpointIdentity::TrustedAdapter { scope }));
        }
        let socket = &self.access.context().socket;
        let metadata = match std::fs::symlink_metadata(socket) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot verify Native host endpoint: {error}")),
        };
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("Native host endpoint must be a socket owned by this user".into());
        }
        Ok(Some(HostEndpointIdentity::HerdrSocket {
            path: socket
                .canonicalize()
                .map_err(|error| format!("cannot resolve Native host endpoint: {error}"))?,
            owner: metadata.uid(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        Some(WorkspaceHostContext::attached_herdr(
            tickhost_connect::access::bundled_tick_executable(),
            tickhost_connect::access::bundled_tickhost_executable(),
        ))
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        TickBackends::HERDR.definitions()
    }

    fn permission(&self, _: ChatId, name: &str, _: &Value) -> HostToolPermission {
        if matches!(name, "ReadWorkspace" | "ReadTick") {
            HostToolPermission::ReadOnly
        } else {
            HostToolPermission::Reviewable
        }
    }

    async fn prepare_call(
        &self,
        _: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.prepare_native_call(name, input, None, cancel).await
    }

    async fn prepare_call_in_context(
        &self,
        _: ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.prepare_native_call(name, input, Some(context), cancel)
            .await
    }

    async fn revalidate_call(
        &self,
        _: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("Native host operation was cancelled before dispatch".into());
        }
        let binding = prepared_binding(call)?;
        if self.host_endpoint()? != binding.endpoint {
            return Err("Native host endpoint changed before dispatch".into());
        }
        let prepared = &binding.operation;
        self.access
            .revalidate(prepared)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(target) = &prepared.target {
            let current = self
                .access
                .resolve(&target.instance)
                .await
                .map_err(|error| error.to_string())?;
            if target_directory(&current) != target_directory(target) {
                return Err(
                    "target working directory changed before dispatch; prepare and approve again"
                        .into(),
                );
            }
        }
        Ok(())
    }

    async fn execute_prepared(
        &self,
        chat_id: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if let Err(error) = self.revalidate_call(chat_id, &call, cancel.clone()).await {
            return HostCallResult::NotDispatched(error);
        }
        let Ok(binding) = call.into_payload::<NativePreparedCall>() else {
            return HostCallResult::NotDispatched(
                "Native host operation has no prepared binding".into(),
            );
        };
        let opened_title = opened_program_title(&binding.operation);
        match self.access.execute(binding.operation, cancel).await {
            Ok(value)
                if value["status"] == "created_not_placed"
                    && value["error"]["dispatched"] == true =>
            {
                HostCallResult::OutcomeUnknown(value.to_string())
            }
            Ok(mut value) => {
                let is_opened = value["status"] == "opened";
                if is_opened {
                    value["visibility"] = json!("visible");
                }
                let result = ToolResult {
                    is_error: value["status"] == "created_not_placed",
                    content: value.to_string(),
                    output_ref: None,
                };
                if !is_opened {
                    return HostCallResult::Completed(result);
                }
                match serde_json::from_value(value["tick_instance_id"].clone()) {
                    Ok(instance) => HostCallResult::CompletedWithEffect(
                        result,
                        Box::new(HostEffect::OpenedTick {
                            instance,
                            title: opened_title,
                        }),
                    ),
                    // The program runs, but without a verifiable identity it cannot join Monitor.
                    Err(error) => HostCallResult::Completed(ToolResult {
                        is_error: true,
                        content: format!(
                            "{}\nThe host reported an invalid Tick Instance ({error}); the program was opened but is not in Monitor. Use ReadWorkspace to find it before acting on it.",
                            value
                        ),
                        output_ref: None,
                    }),
                }
            }
            Err(error) if error.dispatched => HostCallResult::OutcomeUnknown(error.to_string()),
            Err(error) => HostCallResult::NotDispatched(error.to_string()),
        }
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        ToolResult {
            content: "Native host calls require preparation".into(),
            is_error: true,
            output_ref: None,
        }
    }
}
