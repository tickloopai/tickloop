use std::{collections::HashSet, sync::Arc};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tickloop_harness_protocol::{
    Content, Receipt, RemoteSkillDefinition, SkillSource, ToolDefinition, ToolResult,
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
    sources: Vec<McpResourceServer>,
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
        let sources: Vec<_> = provider
            .mcp_resource_servers()
            .into_iter()
            .filter(|source| server.as_ref().is_none_or(|server| server == &source.name))
            .collect();
        if sources.is_empty() {
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
            sources,
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
                || !prepared
                    .sources
                    .iter()
                    .all(|source| crate::mcp_resource_authority_allowed(chat, &source.authority))
                || !prepared
                    .sources
                    .iter()
                    .all(|source| provider.mcp_resource_servers().contains(source))
            {
                return Err("MCP resource is outside this Run's current service authority".into());
            }
            if let Some((definition, relative)) = &prepared.skill {
                let resource =
                    crate::remote_skills::resource_entry(definition, relative.as_deref())?;
                if read_unchanged(chat, &resource.uri, &resource.digest) {
                    return Ok(unchanged(&resource.uri));
                }
            }
            (
                chat.snapshot.policy.revision,
                crate::delegation::ancestor_revisions(&chats, receipt.chat_id)?,
            )
        };
        let result = if prepared.name == LIST_MCP_RESOURCES_TOOL {
            let mut results = Vec::new();
            for source in &prepared.sources {
                let mut result = provider
                    .list_mcp_resources_for(
                        &source.name,
                        prepared.input["cursor"].as_str(),
                        cancel.clone(),
                    )
                    .await?;
                if let Some(resources) = result["resources"].as_array_mut() {
                    for resource in resources {
                        resource["server"] = json!(source.name);
                    }
                }
                result["server"] = json!(source.name);
                results.push(result);
            }
            if results.len() == 1 {
                results.remove(0)
            } else {
                json!({"servers":results})
            }
        } else if let Some((definition, relative)) = &prepared.skill {
            let text = provider
                .read_remote_skill(definition, relative.as_deref(), cancel.clone())
                .await?;
            json!({"contents":[{"uri":prepared.input["uri"],"mimeType":"text/markdown","text":text}]})
        } else {
            provider
                .read_mcp_resource_for(
                    &prepared.sources[0].name,
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
            || !prepared
                .sources
                .iter()
                .all(|source| crate::mcp_resource_authority_allowed(chat, &source.authority))
            || !prepared
                .sources
                .iter()
                .all(|source| provider.mcp_resource_servers().contains(source))
            || prepared.skill.as_ref().is_some_and(|(definition, _)| {
                !provider.remote_skill_is_current(&definition.source)
            })
        {
            return Err("MCP resource source, policy or Run changed during reading".into());
        }
        if prepared.name == READ_MCP_RESOURCE_TOOL
            && let Some(uri) = prepared.input["uri"].as_str()
            && let Some(text) = sole_text(&result)
            && read_unchanged(chat, uri, &digest(text))
        {
            return Ok(unchanged(uri));
        }
        Ok(ToolResult {
            content: result.to_string(),
            is_error: false,
            output_ref: None,
        })
    }
}

/// Like Claude Code's `file_unchanged` Read, whether the model can still see this same resource,
/// with the same text, in an earlier `ReadMcpResourceTool` result. Another resource with the same
/// text is read in full, as another path is; a compaction or a saved-file preview hides the
/// earlier text again.
fn read_unchanged(chat: &crate::Chat, uri: &str, digest_of_text: &str) -> bool {
    let messages = &chat.snapshot.messages[chat.snapshot.active_start()..];
    let calls = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::ToolUse { id, name, .. } if name == READ_MCP_RESOURCE_TOOL => Some(id),
            _ => None,
        })
        .collect::<HashSet<_>>();
    messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| match block {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            } if calls.contains(tool_use_id)
                && !chat.content_replacements.is_replaced(tool_use_id) =>
            {
                serde_json::from_str::<Value>(&content.text()).is_ok_and(|result| {
                    result["contents"][0]["uri"] == uri
                        && sole_text(&result).map(digest).as_deref() == Some(digest_of_text)
                })
            }
            _ => false,
        })
}

fn sole_text(result: &Value) -> Option<&str> {
    match result["contents"].as_array()?.as_slice() {
        [content] => content["text"].as_str(),
        _ => None,
    }
}

fn digest(text: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn unchanged(uri: &str) -> ToolResult {
    ToolResult {
        content: format!(
            "{uri} is unchanged since it was read earlier in this conversation; refer to that result instead of reading it again."
        ),
        is_error: false,
        output_ref: None,
    }
}
