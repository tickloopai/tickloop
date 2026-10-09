//! The native Tick tools a worker installs. Each call is routed by its target alone, before any
//! host resolves it: Workspace discovery, `herdr:` instances and explicit placements go to the
//! Herdr backend, while hidden opens and `hosted:` instances go to the Tickhost backend. Every
//! call runs under the authority of the backend it was routed to; artifact and Shell references
//! go to their native tools instead.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::{HERDR_INSTANCE_TYPE, HOSTED_INSTANCE_TYPE};
use tickhost_connect::page::MAX_PAGE_SIZE;
use tokio_util::sync::CancellationToken;

use crate::{
    host_tools::{
        HostCallContext, HostCallResult, HostToolAuthority, HostToolPermission, HostToolProvider,
        PreparedHostCall, WorkspaceHostContext,
    },
    hosted::tools::HostedTickTools,
    native_host::NativeHostToolProvider,
};
use tickloop_agent_protocol::{ChatId, ToolDefinition, ToolResult};

pub const READ_WORKSPACE: &str = "ReadWorkspace";
pub const READ_TICK: &str = "ReadTick";
pub const CALL_TICK_TOOL: &str = "CallTickTool";
pub const MODIFY_TICK: &str = "ModifyTick";
/// The open placement that runs a Tick hidden in the Tickhost. An open without a placement
/// means the same.
pub const HIDDEN_PLACEMENT: &str = "hidden";
const PLACEMENT_DIRECTIONS: [&str; 4] = ["left", "right", "up", "down"];

#[derive(Debug, thiserror::Error)]
pub enum TickToolTargetError {
    #[error(
        "NativeToolRequired: {operation} cannot operate on an Artifact Tick; use mcp__artifacts__artifact_read and the installed Artifact MCP edit/publish tools with artifact_id"
    )]
    Artifact { operation: String },
    #[error(
        "NativeToolRequired: {operation} cannot operate on a Shell Tick; use Bash, Read on its output_file, and TaskStop with task_id"
    )]
    Shell { operation: String },
    #[error(
        "InvalidTickTarget: {operation} requires the complete {supported}; use Artifact MCP tools for artifacts and Bash/Read/TaskStop for shells"
    )]
    Invalid {
        operation: String,
        supported: &'static str,
    },
}

/// The backend that serves one Tick tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TickBackend {
    Herdr,
    Hosted,
}

/// The backends a provider has. They decide which references and placements it accepts and
/// what its tool definitions offer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TickBackends {
    pub herdr: bool,
    pub hosted: bool,
}

impl TickBackends {
    pub(crate) const HERDR: Self = Self {
        herdr: true,
        hosted: false,
    };
    pub(crate) const HOSTED: Self = Self {
        herdr: false,
        hosted: true,
    };
    pub(crate) const ALL: Self = Self {
        herdr: true,
        hosted: true,
    };

    fn references(self) -> &'static str {
        match (self.herdr, self.hosted) {
            (true, true) => {
                "herdr:<instance id> from ReadWorkspace or hosted:<instance id> from a hidden open"
            }
            (false, true) => "hosted:<instance id> from a hidden open",
            _ => "herdr:<instance id> from ReadWorkspace",
        }
    }

    /// The backend serving `name` with `input`, decided without resolving its target.
    pub(crate) fn route(self, name: &str, input: &Value) -> Result<TickBackend, String> {
        let backend = match name {
            READ_WORKSPACE => TickBackend::Herdr,
            READ_TICK | CALL_TICK_TOOL => self.target(name, input)?,
            MODIFY_TICK => match input["action"].as_str() {
                Some("open") => match input["placement"]["type"].as_str() {
                    None | Some(HIDDEN_PLACEMENT) => TickBackend::Hosted,
                    Some(_) => TickBackend::Herdr,
                },
                Some(action @ ("move" | "send_input")) => match self.target(name, input)? {
                    TickBackend::Herdr => TickBackend::Herdr,
                    TickBackend::Hosted => {
                        return Err(format!(
                            "UnsupportedCapability: {action} applies to a Herdr Pane and a hosted Tick has none; use CallTickTool for the tools it declares"
                        ));
                    }
                },
                Some("close") => self.target(name, input)?,
                Some("reopen") => match self.target(name, input)? {
                    TickBackend::Hosted => TickBackend::Hosted,
                    TickBackend::Herdr => {
                        return Err("UnsupportedCapability: reopen starts a new hosted instance in place of one lost with its Tickhost; a Herdr Pane is never reopened".into());
                    }
                },
                _ => {
                    return Err(
                        "ModifyTick action must be open, move, send_input, close or reopen".into(),
                    );
                }
            },
            _ => return Err(format!("unknown Tick tool: {name}")),
        };
        match backend {
            TickBackend::Herdr if !self.herdr => Err(format!(
                "UnsupportedCapability: this Agent does not run in Herdr, so Workspaces, herdr: instances and visible Panes are unavailable{}",
                if self.hosted {
                    "; omit placement to open the Tick hidden in the Tickhost"
                } else {
                    ""
                }
            )),
            TickBackend::Hosted if !self.hosted => Err(format!(
                "UnsupportedCapability: this Agent has no Tickhost, so hidden opens and hosted: instances are unavailable{}",
                if self.herdr {
                    "; pass an explicit placement to open a visible Herdr Pane"
                } else {
                    ""
                }
            )),
            _ => Ok(backend),
        }
    }

    fn target(self, name: &str, input: &Value) -> Result<TickBackend, String> {
        let operation = name.to_owned();
        let target = input["tick_instance_id"].as_str().unwrap_or_default();
        let error = match target.split_once(':').map(|(kind, _)| kind) {
            Some(HERDR_INSTANCE_TYPE) => return Ok(TickBackend::Herdr),
            Some(HOSTED_INSTANCE_TYPE) => return Ok(TickBackend::Hosted),
            Some("artifact") => TickToolTargetError::Artifact { operation },
            Some("shell") => TickToolTargetError::Shell { operation },
            _ => TickToolTargetError::Invalid {
                operation,
                supported: self.references(),
            },
        };
        Err(error.to_string())
    }

    /// The model-visible Tick tools these backends offer.
    pub(crate) fn definitions(self) -> Vec<ToolDefinition> {
        let mut definitions = Vec::new();
        if self.herdr {
            definitions.push(ToolDefinition {
                name: READ_WORKSPACE.into(),
                description: format!("List running programs in the current Herdr Workspace, or in an explicitly named Workspace. Uses the Agent's Work Context by default; without one, discovers available Workspaces. The optional caller describes the View carrying this input, and may be null while the Agent works in the background. Each program has a stable Tick Instance reference. Results come in pages of at most {MAX_PAGE_SIZE} programs with the complete total; pass next_cursor back as cursor until it is null. CursorExpired means the Workspace changed: list again from the first page. Listing never subscribes."),
                input_schema: json!({"type":"object","properties":{"workspace_id":{"type":"string","minLength":1},"cursor":{"type":"object","properties":{"generation":{"type":"string"},"after":{"type":"string"}},"required":["generation","after"],"additionalProperties":false},"limit":{"type":"integer","minimum":1,"maximum":MAX_PAGE_SIZE}},"additionalProperties":false}),
            });
        }
        let identity = match (self.herdr, self.hosted) {
            (true, true) => {
                "its complete instance identity: hosted:<id> for a Tick this Chat opened hidden in the Tickhost, or herdr:<id> for a Herdr program from ReadWorkspace"
            }
            (false, true) => {
                "its complete hosted: instance identity, for a Tick this Chat opened hidden in the Tickhost"
            }
            _ => "the complete herdr: instance identity of a Herdr program from ReadWorkspace",
        };
        let target_properties = |extra: Value| {
            let mut properties = json!({"tick_instance_id":{"type":"string","minLength":1}});
            if self.herdr {
                properties["workspace_id"] = json!({"type":"string","minLength":1});
            }
            if let (Value::Object(properties), Value::Object(extra)) = (&mut properties, extra) {
                properties.extend(extra);
            }
            properties
        };
        definitions.push(ToolDefinition {
            name: READ_TICK.into(),
            description: format!(
                "Read a Tick using {identity}. Protocol Ticks return structured content and their declared tool catalog.{} Reading never subscribes. For artifacts use the installed Artifact MCP tools; for Shell tasks use Read on Bash's output_file.",
                if self.herdr {
                    " Ordinary Herdr Pane programs return at most 200 recent plain-text lines and 64 KiB; workspace_id applies to herdr: instances."
                } else {
                    ""
                }
            ),
            input_schema: json!({"type":"object","properties":target_properties(json!({})),"required":["tick_instance_id"],"additionalProperties":false}),
        });
        definitions.push(ToolDefinition {
            name: CALL_TICK_TOOL.into(),
            description: format!("Call one tool declared by a protocol Tick using {identity}. Read that Tick's catalog first; query tools read content, while modifying tools require permission. Use native Artifact MCP tools for artifacts and Bash/Read/TaskStop for Shell tasks."),
            input_schema: json!({"type":"object","properties":target_properties(json!({"tool":{"type":"string","minLength":1},"args":{"type":"object"}})),"required":["tick_instance_id","tool","args"],"additionalProperties":false}),
        });
        definitions.push(ToolDefinition {
            name: MODIFY_TICK.into(),
            description: self.modify_description(),
            input_schema: self.modify_schema(),
        });
        definitions
    }

    fn modify_description(self) -> String {
        let mut parts = Vec::new();
        if self.hosted {
            parts.push("open runs a protocol Tick hidden in the Tickhost when placement is omitted or {\"type\":\"hidden\"}: it keeps running without any View or layout, joins this Chat's Monitor, and is not subscribed.");
        }
        if self.herdr {
            parts.push(if self.hosted {
                "An explicit placement opens a visible Herdr Pane instead: current_tab places relative to the View captured for this input, which must still be valid; pane places relative to the named Pane in the selected Workspace; new_tab opens a new Tab."
            } else {
                "open needs an explicit placement and opens a visible Herdr Pane, since this Agent has no Tickhost for hidden Ticks: current_tab places relative to the View captured for this input, which must still be valid; pane places relative to the named Pane in the selected Workspace; new_tab opens a new Tab."
            });
            parts.push("Explicit Pane targets and new_tab work without an Agent View. Direction accepts left, right, up, or down and defaults to right. Placement is frozen before approval and never follows UI focus. move keeps a herdr: instance and its subscriptions and defaults to current_tab; for an explicit relative direction, invoke move even when the Tick is already in the target Tab. send_input types into a herdr: Pane and is checked before sending, without an atomic identity/send guarantee.");
        }
        parts.push(match (self.herdr, self.hosted) {
            (true, true) => "close ends the Tick's process: a hosted: Tick in the Tickhost, or the herdr: Pane in Herdr.",
            (false, true) => "close ends the hosted Tick's process.",
            _ => "close ends the herdr: Pane's program.",
        });
        if self.hosted {
            parts.push("reopen starts a new hosted instance from the recorded program of one lost with its Tickhost; it needs approval again, gets a new identity, and no subscription follows it.");
        }
        parts.push("Omitted cwd uses the Agent file work directory; for command[0]='tick', prefer the bundled sibling executable, then PATH. Results report only verified facts: the instance, whether it runs hidden in the Tickhost or visible in a Herdr Pane, and its host and location. Use the tickloop Skill for guidance.");
        parts.join(" ")
    }

    fn modify_schema(self) -> Value {
        let mut placements = Vec::new();
        if self.hosted {
            placements.push(json!({"type":"object","properties":{"type":{"const":HIDDEN_PLACEMENT}},"required":["type"],"additionalProperties":false}));
        }
        if self.herdr {
            placements.extend(herdr_placements(true));
        }
        let mut open = json!({"type":"object","properties":{"action":{"const":"open"},"command":{"type":"array","minItems":1,"items":{"type":"string"}},"cwd":{"type":"string","minLength":1},"placement":{"type":"object","oneOf":placements}},"required":["action","command"],"additionalProperties":false});
        if self.herdr {
            open["properties"]["workspace_id"] = json!({"type":"string","minLength":1});
        }
        if !self.hosted {
            open["required"] = json!(["action", "command", "placement"]);
        }
        let mut variants = vec![open];
        if self.herdr {
            variants.push(json!({"type":"object","properties":{"action":{"const":"move"},"tick_instance_id":{"type":"string","minLength":1},"workspace_id":{"type":"string","minLength":1},"placement":{"type":"object","oneOf":herdr_placements(false)}},"required":["action","tick_instance_id"],"additionalProperties":false}));
            variants.push(json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":1},"workspace_id":{"type":"string","minLength":1},"action":{"type":"string","enum":["send_input","close"]},"text":{"type":"string"},"keys":{"type":"array","minItems":1,"items":{"type":"string"}}},"required":["tick_instance_id","action"],"additionalProperties":false}));
        } else {
            variants.push(json!({"type":"object","properties":{"action":{"const":"close"},"tick_instance_id":{"type":"string","minLength":1}},"required":["action","tick_instance_id"],"additionalProperties":false}));
        }
        if self.hosted {
            variants.push(json!({"type":"object","properties":{"action":{"const":"reopen"},"tick_instance_id":{"type":"string","minLength":1}},"required":["action","tick_instance_id"],"additionalProperties":false}));
        }
        json!({"type":"object","oneOf":variants})
    }
}

fn herdr_placements(allow_new_tab: bool) -> Vec<Value> {
    let mut variants = vec![
        json!({"type":"object","properties":{"type":{"const":"current_tab"},"direction":{"enum":PLACEMENT_DIRECTIONS}},"required":["type"],"additionalProperties":false}),
        json!({"type":"object","properties":{"type":{"const":"pane"},"pane_id":{"type":"string","minLength":1},"direction":{"enum":PLACEMENT_DIRECTIONS}},"required":["type","pane_id"],"additionalProperties":false}),
    ];
    if allow_new_tab {
        variants.push(json!({"type":"object","properties":{"type":{"const":"new_tab"}},"required":["type"],"additionalProperties":false}));
    }
    variants
}

/// The native Tick tools of a worker: its Herdr backend when it runs in a Herdr View, and its
/// Tickhost backend when its daemon granted one.
pub struct TickToolRouter {
    herdr: Option<Arc<NativeHostToolProvider>>,
    hosted: Option<Arc<HostedTickTools>>,
}

enum Backend<'a> {
    Herdr(&'a NativeHostToolProvider),
    Hosted(&'a HostedTickTools),
}

impl Backend<'_> {
    fn provider(&self) -> &dyn HostToolProvider {
        match self {
            Self::Herdr(provider) => *provider,
            Self::Hosted(provider) => *provider,
        }
    }
}

impl TickToolRouter {
    pub fn new(
        herdr: Option<Arc<NativeHostToolProvider>>,
        hosted: Option<Arc<HostedTickTools>>,
    ) -> Self {
        Self { herdr, hosted }
    }

    fn backends(&self) -> TickBackends {
        TickBackends {
            herdr: self.herdr.is_some(),
            hosted: self.hosted.is_some(),
        }
    }

    fn backend(&self, name: &str, input: &Value) -> Result<Backend<'_>, String> {
        match (
            self.backends().route(name, input)?,
            &self.herdr,
            &self.hosted,
        ) {
            (TickBackend::Herdr, Some(herdr), _) => Ok(Backend::Herdr(herdr)),
            (TickBackend::Hosted, _, Some(hosted)) => Ok(Backend::Hosted(hosted)),
            _ => unreachable!("routing only selects a present backend"),
        }
    }
}

#[async_trait]
impl HostToolProvider for TickToolRouter {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.backends().definitions()
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        let context = self
            .herdr
            .as_ref()
            .and_then(|herdr| herdr.workspace_host_context())
            .unwrap_or_else(WorkspaceHostContext::from_environment);
        Some(if self.hosted.is_some() {
            context.with_tickhost()
        } else {
            context
        })
    }

    fn tool_authorities(&self, name: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        let mut authorities = BTreeSet::new();
        if let Some(herdr) = &self.herdr
            && herdr.definitions().iter().any(|tool| tool.name == name)
        {
            authorities.extend(herdr.tool_authorities(name)?);
        }
        if let Some(hosted) = &self.hosted
            && hosted.definitions().iter().any(|tool| tool.name == name)
        {
            authorities.extend(hosted.tool_authorities(name)?);
        }
        Ok(authorities)
    }

    fn call_authority(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<Option<HostToolAuthority>, String> {
        self.backend(name, input)?
            .provider()
            .call_authority(name, input)
    }

    fn permission(&self, chat: ChatId, name: &str, input: &Value) -> HostToolPermission {
        self.backend(name, input)
            .map_or(HostToolPermission::Reviewable, |backend| {
                backend.provider().permission(chat, name, input)
            })
    }

    async fn prepare_call(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.backend(name, input)?
            .provider()
            .prepare_call(chat, name, input, cancel)
            .await
    }

    async fn prepare_call_in_context(
        &self,
        chat: ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        self.backend(name, input)?
            .provider()
            .prepare_call_in_context(chat, context, name, input, cancel)
            .await
    }

    async fn revalidate_call(
        &self,
        chat: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        self.backend(call.name(), call.source_input())?
            .provider()
            .revalidate_call(chat, call, cancel)
            .await
    }

    async fn execute_prepared(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        match self.backend(call.name(), call.source_input()) {
            Ok(backend) => {
                backend
                    .provider()
                    .execute_prepared(chat, call, cancel)
                    .await
            }
            Err(error) => HostCallResult::NotDispatched(error),
        }
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        ToolResult {
            content: "Tick tool calls require preparation".into(),
            is_error: true,
            output_ref: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_placements_expose_four_directions_for_both_anchor_types() {
        for allow_new_tab in [false, true] {
            let variants = herdr_placements(allow_new_tab);
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
