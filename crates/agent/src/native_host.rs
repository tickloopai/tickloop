use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_connect::access::{AccessPermission, PreparedOperation, TickAccess, TickLocation};
use tokio_util::sync::CancellationToken;

use crate::{
    host_tools::{
        HostCallResult, HostToolPermission, HostToolProvider, PreparedHostCall,
        WorkspaceHostContext,
    },
    model::ToolDefinition,
    runtime::ChatId,
    tools::ToolResult,
};

pub struct NativeHostToolProvider {
    access: Arc<TickAccess>,
}

const PLACEMENT_DIRECTIONS: [&str; 4] = ["left", "right", "up", "down"];

impl NativeHostToolProvider {
    pub fn new(access: Arc<TickAccess>) -> Self {
        Self { access }
    }

    pub fn access(&self) -> &Arc<TickAccess> {
        &self.access
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

fn prepared_binding(call: &PreparedHostCall) -> Result<&PreparedOperation, String> {
    call.payload::<PreparedOperation>()
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
    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        Some(WorkspaceHostContext::attached_herdr(
            tick_connect::access::bundled_tick_executable(),
        ))
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "ReadWorkspace".into(),
                description: "List running programs in the current Herdr Workspace, or in an explicitly named Workspace. Includes the calling Agent's current Pane and Tab under caller. Each program has a stable Tick Instance reference.".into(),
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
                description: "Open a program, move an existing Tick beside another Pane while preserving its instance and subscriptions, send terminal input, or close a Pane. For open, omitted placement creates a new Tab; placement.type=current_tab places relative to this Agent, and type=pane places relative to the named Pane in the selected Workspace. Move defaults to current_tab. Direction accepts left, right, up, or down and defaults to right. For an explicit relative direction, invoke move even when the Tick is already in the target Tab. Placement is frozen before approval and never follows UI focus. Right/down do not request focus; moving a focused Pane lets Herdr choose a remaining Pane. Left/up finish focused on the anchor Pane (this Agent for current_tab, the named Pane for pane). For command[0]='tick', prefer the bundled sibling executable, then PATH. Use the tickloop Skill for guidance. Terminal input is checked before sending, without an atomic identity/send guarantee.".into(),
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
        if cancel.is_cancelled() {
            return Err("Native host operation was cancelled before preparation".into());
        }
        let operation_name = match name {
            "ReadWorkspace" => "read_workspace",
            "ReadTick" => "read_tick",
            "CallTickTool" => "call_tick_tool",
            "ModifyTick" => "modify_tick",
            _ => return Err(format!("unknown Native host tool: {name}")),
        };
        let prepared = self
            .access
            .prepare(operation_name, input)
            .await
            .map_err(|error| error.to_string())?;
        let permission = match prepared.permission {
            AccessPermission::ReadOnly => HostToolPermission::ReadOnly,
            AccessPermission::Confirmation => HostToolPermission::Reviewable,
        };
        let call = PreparedHostCall::new(
            name.to_owned(),
            input.clone(),
            prepared.input.clone(),
            permission,
            prepared.summary.clone(),
        );
        Ok(call.with_payload(prepared))
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
        let prepared = prepared_binding(call)?;
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
        let Ok(prepared) = call.into_payload::<PreparedOperation>() else {
            return HostCallResult::NotDispatched(
                "Native host operation has no prepared binding".into(),
            );
        };
        match self.access.execute(prepared, cancel).await {
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
