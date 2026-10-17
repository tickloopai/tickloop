use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_agent_protocol::{
    Receipt, RemoteSkillDefinition, SkillSource, ToolDefinition, ToolResult,
};
use tokio_util::sync::CancellationToken;

use crate::{
    Agent,
    host_tools::{HostToolProvider, McpResourceServer},
};

pub(crate) const READ_MCP_RESOURCE_TOOL: &str = "ReadMcpResourceTool";
pub(crate) const LIST_MCP_RESOURCES_TOOL: &str = "ListMcpResourcesTool";

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: READ_MCP_RESOURCE_TOOL.into(),
        description: "Read a resource from a connected MCP server. Use its exact server name and full resource URI. Resolve relative Skill references against the resource root in the loaded guidance. Reading content, including SKILL.md, does not activate a Skill.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "server": {"type": "string", "description": "The MCP server name"},
                "uri": {"type": "string", "minLength": 1, "description": "The full resource URI to read"}
            },
            "required": ["server", "uri"],
            "additionalProperties": false
        }),
    }, ToolDefinition {
        name: LIST_MCP_RESOURCES_TOOL.into(),
        description: "List resources published by connected MCP servers. Optionally select a server. Follow nextCursor by passing it as cursor with the same server. This discovers resource metadata without downloading content or activating Skills; it does not discover callable tools.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "server": {"type":"string", "description":"The MCP server name; omit to list resources from connected servers"},
                "cursor": {"type":"string", "description":"Opaque nextCursor from an earlier resource-list result"}
            },
            "additionalProperties": false
        }),
    }]
}

pub(crate) struct PreparedResourceCall {
    pub input: Value,
    name: String,
    source: McpResourceServer,
    skill: Option<(RemoteSkillDefinition, Option<String>)>,
}

impl PreparedResourceCall {
    pub(crate) fn prepare(
        provider: &dyn HostToolProvider,
        name: &str,
        input: &Value,
    ) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ListInput {
            server: Option<String>,
            cursor: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ReadInput {
            server: String,
            uri: String,
        }
        let source = provider
            .mcp_resource_server()
            .ok_or("MCP resource source unavailable")?;
        let (server, uri) = if name == LIST_MCP_RESOURCES_TOOL {
            let parsed: ListInput =
                serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
            if parsed.cursor.is_some() && parsed.server.is_none() {
                return Err("server is required when continuing a resource-list cursor".into());
            }
            (parsed.server, None)
        } else {
            let parsed: ReadInput =
                serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
            (Some(parsed.server), Some(parsed.uri))
        };
        if server.as_ref().is_some_and(|server| server != &source.name) {
            return Err("unknown or unavailable MCP resource server".into());
        }
        let skill = if let Some(target) = &uri {
            let definition = provider.remote_skills().into_iter().find(|definition| {
                matches!(&definition.source, SkillSource::Remote {resources, ..} if resources.iter().any(|resource| &resource.uri == target))
            });
            if let Some(definition) = definition {
                let SkillSource::Remote { uri, .. } = &definition.source else {
                    return Err("resource source is not remote".into());
                };
                let relative = if uri == target {
                    None
                } else {
                    Some(
                        target
                            .strip_prefix(uri.strip_suffix("SKILL.md").ok_or("invalid Skill URI")?)
                            .ok_or("resource is outside the Skill directory")?
                            .to_owned(),
                    )
                };
                crate::remote_skills::resource_entry(&definition, relative.as_deref())?;
                Some((definition, relative))
            } else {
                if let Some(path) = target.strip_prefix("skill://tickloop-artifacts/")
                    && !crate::remote_skills::valid_resource(path)
                {
                    return Err("invalid Skill resource path".into());
                }
                None
            }
        } else {
            None
        };
        Ok(Self {
            input: input.clone(),
            name: name.into(),
            source,
            skill,
        })
    }
}

impl Agent {
    pub(crate) async fn mcp_resource_tool(
        &self,
        receipt: &Receipt,
        provider: &Arc<dyn HostToolProvider>,
        prepared: &PreparedResourceCall,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        let (revision, ancestor_revisions) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or("chat unavailable")?;
            if cancel.is_cancelled()
                || chat.active_run != Some(receipt.run_id)
                || !crate::mcp_resource_authority_allowed(chat, &prepared.source.authority)
                || provider.mcp_resource_server().as_ref() != Some(&prepared.source)
            {
                return Err("MCP resource is outside this Run's current service authority".into());
            }
            (
                chat.snapshot.policy.revision,
                crate::delegation::ancestor_revisions(&chats, receipt.chat_id)?,
            )
        };
        let mut result = if prepared.name == LIST_MCP_RESOURCES_TOOL {
            provider
                .list_mcp_resources(prepared.input["cursor"].as_str(), cancel.clone())
                .await?
        } else if let Some((definition, relative)) = &prepared.skill {
            let text = provider
                .read_remote_skill(definition, relative.as_deref(), cancel.clone())
                .await?;
            json!({"contents":[{"uri":prepared.input["uri"],"mimeType":"text/markdown","text":text}]})
        } else {
            provider
                .read_mcp_resource(
                    prepared.input["uri"]
                        .as_str()
                        .ok_or("resource URI missing")?,
                    cancel.clone(),
                )
                .await?
        };
        self.check_delegated_call(receipt, &prepared.name, &prepared.input, None)
            .await?;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).ok_or("chat unavailable")?;
        if cancel.is_cancelled()
            || chat.active_run != Some(receipt.run_id)
            || chat.snapshot.policy.revision != revision
            || crate::delegation::ancestor_revisions(&chats, receipt.chat_id)? != ancestor_revisions
            || !crate::mcp_resource_authority_allowed(chat, &prepared.source.authority)
            || provider.mcp_resource_server().as_ref() != Some(&prepared.source)
            || prepared.skill.as_ref().is_some_and(|(definition, _)| {
                !provider.remote_skill_is_current(&definition.source)
            })
        {
            return Err("MCP resource source, policy or Run changed during reading".into());
        }
        if prepared.name == LIST_MCP_RESOURCES_TOOL {
            if let Some(resources) = result["resources"].as_array_mut() {
                for resource in resources {
                    resource["server"] = json!(prepared.source.name);
                }
            }
            result["server"] = json!(prepared.source.name);
        }
        Ok(ToolResult {
            content: result.to_string(),
            is_error: false,
            output_ref: None,
        })
    }
}
