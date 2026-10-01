use std::{
    os::unix::fs::{FileTypeExt, MetadataExt},
    sync::Arc,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_connect::access::{
    AccessContext, AccessPermission, PreparedOperation, TickAccess, TickLocation,
};
use tokio_util::sync::CancellationToken;

use crate::{
    host_tools::{
        HostCallContext, HostCallResult, HostEndpointIdentity, HostToolPermission,
        HostToolProvider, PreparedHostCall, WorkspaceHostContext,
    },
    model::ToolDefinition,
    runtime::ChatId,
    tools::ToolResult,
};

pub struct NativeHostToolProvider {
    access: Arc<TickAccess>,
    trusted_adapter_scope: Option<uuid::Uuid>,
}

struct NativePreparedCall {
    operation: PreparedOperation,
    endpoint: Option<HostEndpointIdentity>,
}

const PLACEMENT_DIRECTIONS: [&str; 4] = ["left", "right", "up", "down"];

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
                            interaction: interaction.map(|source| source.pane.clone()),
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

fn prepared_binding(call: &PreparedHostCall) -> Result<&NativePreparedCall, String> {
    call.payload::<NativePreparedCall>()
        .ok_or_else(|| "Native host operation has no prepared binding".into())
}

fn placement_schema(allow_new_tab: bool) -> Value {
    let mut variants = vec![
        json!({"type":"object","properties":{"type":{"const":"current_tab"},"direction":{"enum":PLACEMENT_DIRECTIONS}},"required":["type"],"additionalProperties":false}),
        json!({"type":"object","properties":{"type":{"const":"pane"},"pane_id":{"type":"string","minLength":1},"direction":{"enum":PLACEMENT_DIRECTIONS}},"required":["type","pane_id"],"additionalProperties":false}),
    ];
    if allow_new_tab {
        variants.push(json!({"type":"object","properties":{"type":{"const":"new_tab"}},"required":["type"],"additionalProperties":false}));
    }
    json!({"type":"object","oneOf":variants})
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
            tick_connect::access::bundled_tick_executable(),
        ))
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "ReadWorkspace".into(),
                description: "List running programs in the current Herdr Workspace, or in an explicitly named Workspace. Uses the Agent's Work Context by default; without one, discovers available Workspaces. The optional caller describes the View carrying this input, and may be null while the Agent works in the background. Each program has a stable Tick Instance reference.".into(),
                input_schema: json!({"type":"object","properties":{"workspace_id":{"type":"string","minLength":1}},"additionalProperties":false}),
            },
            ToolDefinition {
                name: "ReadTick".into(),
                description: "Read the current content of a selected Tick Instance. Protocol Ticks return structured content and their declared tool catalog. Ordinary programs return at most 200 recent plain-text lines and 64 KiB; older output is not recoverable from that view.".into(),
                input_schema: json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":1},"workspace_id":{"type":"string","minLength":1}},"required":["tick_instance_id"],"additionalProperties":false}),
            },
            ToolDefinition {
                name: "CallTickTool".into(),
                description: "Call one tool declared by a protocol Tick. Read that Tick's catalog first; query tools read content, while modifying tools require permission.".into(),
                input_schema: json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":1},"workspace_id":{"type":"string","minLength":1},"tool":{"type":"string","minLength":1},"args":{"type":"object"}},"required":["tick_instance_id","tool","args"],"additionalProperties":false}),
            },
            ToolDefinition {
                name: "ModifyTick".into(),
                description: "Open a program, move an existing Tick beside another Pane while preserving its instance and subscriptions, send terminal input, or close a Pane. For open, omitted placement creates a new Tab; placement.type=current_tab places relative to the View captured for this input, which must still be valid, and type=pane places relative to the named Pane in the selected Workspace. Explicit targets and new_tab work without an Agent View; omitted cwd uses the Agent file work directory. Results report the verified host, Workspace, Tab, Pane, Tick Instance and state, with unknown fields left unresolved. Move defaults to current_tab. Direction accepts left, right, up, or down and defaults to right. For an explicit relative direction, invoke move even when the Tick is already in the target Tab. Placement is frozen before approval and never follows UI focus. Right/down do not request focus; moving a focused Pane lets Herdr choose a remaining Pane. Left/up finish focused on the anchor Pane (this Agent for current_tab, the named Pane for pane). For command[0]='tick', prefer the bundled sibling executable, then PATH. Use the tickloop Skill for guidance. Terminal input is checked before sending, without an atomic identity/send guarantee.".into(),
                input_schema: json!({"type":"object","oneOf":[
                    {"type":"object","properties":{"action":{"const":"open"},"command":{"type":"array","minItems":1,"items":{"type":"string"}},"cwd":{"type":"string","minLength":1},"workspace_id":{"type":"string","minLength":1},"placement":placement_schema(true)},"required":["action","command"],"additionalProperties":false},
                    {"type":"object","properties":{"action":{"const":"move"},"tick_instance_id":{"type":"string","minLength":1},"workspace_id":{"type":"string","minLength":1},"placement":placement_schema(false)},"required":["action","tick_instance_id"],"additionalProperties":false},
                    {"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":1},"workspace_id":{"type":"string","minLength":1},"action":{"type":"string","enum":["send_input","close"]},"text":{"type":"string"},"keys":{"type":"array","minItems":1,"items":{"type":"string"}}},"required":["tick_instance_id","action"],"additionalProperties":false}
                ]}),
            },
        ]
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
        match self.access.execute(binding.operation, cancel).await {
            Ok(value)
                if value["status"] == "created_not_placed"
                    && value["error"]["dispatched"] == true =>
            {
                HostCallResult::OutcomeUnknown(value.to_string())
            }
            Ok(value) => HostCallResult::Completed(ToolResult {
                is_error: value["status"] == "created_not_placed",
                content: value.to_string(),
                output_ref: None,
            }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_schema_exposes_four_directions_for_both_anchor_types() {
        for allow_new_tab in [false, true] {
            let schema = placement_schema(allow_new_tab);
            let variants = schema["oneOf"].as_array().unwrap();
            for anchor_type in ["current_tab", "pane"] {
                let variant = variants
                    .iter()
                    .find(|variant| variant["properties"]["type"]["const"] == anchor_type)
                    .unwrap();
                assert_eq!(
                    variant["properties"]["direction"]["enum"],
                    json!(["left", "right", "up", "down"])
                );
                assert!(
                    !variant["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("direction"))
                );
            }
            assert_eq!(
                variants
                    .iter()
                    .any(|variant| variant["properties"]["type"]["const"] == "new_tab"),
                allow_new_tab
            );
        }
    }
}
