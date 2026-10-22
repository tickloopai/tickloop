use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex, OnceLock},
};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tickloop_agent_protocol::{
    ChatId, Content, Decision, HostToolAuthority, Receipt, RemoteSkillDefinition, SkillSource,
    ToolDefinition, ToolResult,
};
use tickloop_agent_tools::permissions::PermissionPolicyExt;
use tokio_util::sync::CancellationToken;

use crate::{
    Agent,
    host_tools::{HostToolProvider, McpResourceServer},
};

pub(crate) const READ_MCP_RESOURCE_TOOL: &str = "ReadMcpResourceTool";
pub(crate) const LIST_MCP_RESOURCES_TOOL: &str = "ListMcpResourcesTool";
const MAX_CURSOR_BINDINGS: usize = 1024;
const MAX_CURSOR_BYTES: usize = 64 * 1024;
const MAX_RESOURCE_SERVERS: usize = 64;

type CursorBindings = VecDeque<(String, McpResourceServer, String)>;

// A random handle wraps a server's cursor without exposing account identity or allowing the
// caller to edit the binding. The bounded registry deliberately expires old/process-lost cursors;
// callers can always restart listing. A cursor is not an authorization grant.
fn cursor_bindings() -> &'static Mutex<CursorBindings> {
    static BINDINGS: OnceLock<Mutex<CursorBindings>> = OnceLock::new();
    BINDINGS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn wrap_cursor(source: &McpResourceServer, cursor: &str) -> Result<String, String> {
    if cursor.len() > MAX_CURSOR_BYTES {
        return Err("MCP resource-list cursor exceeds the supported size".into());
    }
    let token = format!("mcp-resource:v1:{}", uuid::Uuid::new_v4());
    let mut bindings = cursor_bindings()
        .lock()
        .map_err(|_| "MCP resource cursor registry unavailable")?;
    if bindings.len() == MAX_CURSOR_BINDINGS {
        bindings.pop_front();
    }
    bindings.push_back((token.clone(), source.clone(), cursor.into()));
    Ok(token)
}

fn unwrap_cursor(source: &McpResourceServer, token: &str) -> Result<String, String> {
    let bindings = cursor_bindings()
        .lock()
        .map_err(|_| "MCP resource cursor registry unavailable")?;
    let (_, bound, cursor) = bindings
        .iter()
        .find(|(candidate, _, _)| candidate == token)
        .ok_or("unknown or expired MCP resource-list cursor; restart listing")?;
    if bound != source {
        return Err(
            "MCP resource-list cursor belongs to a different server or source identity".into(),
        );
    }
    Ok(cursor.clone())
}

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: READ_MCP_RESOURCE_TOOL.into(),
        description: "Read a resource from a connected MCP server. Use its exact server name and full resource URI. Resolve relative Skill references against the resource root in the loaded guidance. Reading content, including SKILL.md, does not activate a Skill.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "server": {"type": "string", "minLength": 1, "description": "The MCP server name"},
                "uri": {"type": "string", "minLength": 1, "description": "The full resource URI to read"}
            },
            "required": ["server", "uri"],
            "additionalProperties": false
        }),
    }, ToolDefinition {
        name: LIST_MCP_RESOURCES_TOOL.into(),
        description: "List resources published by connected MCP servers. Omit server for per-server results from all authorized current servers, or select an exact server. Follow each server's opaque nextCursor by passing it as cursor with that same server; old cursors can expire, requiring a new list. This discovers resource metadata without downloading content or activating Skills; it does not discover callable tools.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "server": {"type":"string", "minLength":1, "description":"The MCP server name; omit to list resources from connected servers"},
                "cursor": {"type":"string", "description":"Opaque nextCursor from an earlier resource-list result; requires the same server"}
            },
            "additionalProperties": false
        }),
    }]
}

pub(crate) struct PreparedResourceCall {
    pub input: Value,
    name: String,
    sources: Vec<McpResourceServer>,
    all_servers: bool,
    cursor: Option<String>,
    skill: Option<(RemoteSkillDefinition, Option<String>)>,
}

fn skill_matches_source(definition: &RemoteSkillDefinition, source: &McpResourceServer) -> bool {
    matches!(
        (&definition.source, &source.authority),
        (
            SkillSource::Remote { server_id, account_id, .. },
            HostToolAuthority::Mcp { server_id: selected_server, account_id: selected_account }
        ) if server_id == selected_server && account_id == selected_account
    )
}

fn source_is_current(source: &McpResourceServer, current: &[McpResourceServer]) -> bool {
    let mut matching = current
        .iter()
        .filter(|candidate| candidate.name == source.name);
    matching.next() == Some(source) && matching.next().is_none()
}

fn annotate_list(mut result: Value, source: &McpResourceServer) -> Result<Value, String> {
    if !result.is_object() || !result["resources"].is_array() {
        return Err("invalid MCP resource-list response".into());
    }
    if let Some(cursor) = result.get("nextCursor").filter(|cursor| !cursor.is_null()) {
        let cursor = cursor.as_str().ok_or("invalid MCP resource-list cursor")?;
        result["nextCursor"] = json!(wrap_cursor(source, cursor)?);
    }
    for resource in result["resources"].as_array_mut().expect("checked array") {
        let entry = resource
            .as_object_mut()
            .ok_or("invalid MCP resource metadata")?;
        entry.insert("server".into(), json!(source.name));
    }
    result["server"] = json!(source.name);
    Ok(result)
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
        let (server, uri, token) = match name {
            LIST_MCP_RESOURCES_TOOL => {
                let parsed: ListInput =
                    serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
                if parsed.cursor.is_some() && parsed.server.is_none() {
                    return Err("server is required when continuing a resource-list cursor".into());
                }
                (parsed.server, None, parsed.cursor)
            }
            READ_MCP_RESOURCE_TOOL => {
                let parsed: ReadInput =
                    serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
                if parsed.uri.is_empty() {
                    return Err("resource URI must not be empty".into());
                }
                (Some(parsed.server), Some(parsed.uri), None)
            }
            _ => return Err("unknown MCP resource tool".into()),
        };
        let current = provider.mcp_resource_servers();
        if current.len() > MAX_RESOURCE_SERVERS {
            return Err("too many MCP resource servers".into());
        }
        let mut names = HashSet::new();
        if current
            .iter()
            .any(|source| source.name.is_empty() || !names.insert(&source.name))
        {
            return Err("empty or ambiguous MCP resource server name".into());
        }
        let all_servers = server.is_none();
        let sources: Vec<_> = current
            .into_iter()
            .filter(|source| server.as_ref().is_none_or(|server| server == &source.name))
            .collect();
        if sources.is_empty() {
            return Err("unknown or unavailable MCP resource server".into());
        }
        let cursor = token
            .as_deref()
            .map(|token| unwrap_cursor(&sources[0], token))
            .transpose()?;
        let skill = if let Some(target) = &uri {
            let source = &sources[0];
            let definition = provider.remote_skills().into_iter().find(|definition| {
                skill_matches_source(definition, source)
                    && matches!(&definition.source, SkillSource::Remote {resources, ..} if resources.iter().any(|resource| &resource.uri == target))
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
                if source.name == "tickloop-artifacts"
                    && let Some(path) = target.strip_prefix("skill://tickloop-artifacts/")
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
            all_servers,
            cursor,
            skill,
        })
    }
}

impl Agent {
    // Every argument is part of the immutable prepared-call authority recheck.
    #[allow(clippy::too_many_arguments)]
    async fn revalidate_mcp_resource_call(
        &self,
        receipt: &Receipt,
        provider: &dyn HostToolProvider,
        prepared: &PreparedResourceCall,
        sources: &[McpResourceServer],
        revision: u64,
        ancestor_revisions: &[(ChatId, u64)],
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).ok_or("chat unavailable")?;
        let current = provider.mcp_resource_servers();
        if cancel.is_cancelled()
            || chat.active_run != Some(receipt.run_id)
            || chat.snapshot.policy.revision != revision
            || crate::delegation::ancestor_revisions(&chats, receipt.chat_id)? != ancestor_revisions
            || sources.iter().any(|source| {
                !crate::mcp_resource_authority_allowed(chat, &source.authority)
                    || !source_is_current(source, &current)
            })
            || prepared.skill.as_ref().is_some_and(|(definition, _)| {
                !provider.remote_skill_is_current(&definition.source)
            })
        {
            return Err("MCP resource source, policy or Run changed during reading".into());
        }
        Ok(())
    }

    pub(crate) async fn mcp_resource_tool(
        &self,
        receipt: &Receipt,
        provider: &Arc<dyn HostToolProvider>,
        prepared: &PreparedResourceCall,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        let (revision, ancestor_revisions, sources) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or("chat unavailable")?;
            let sources: Vec<_> = prepared
                .sources
                .iter()
                .filter(|source| crate::mcp_resource_authority_allowed(chat, &source.authority))
                .cloned()
                .collect();
            if sources.is_empty()
                || (!prepared.all_servers && sources.len() != prepared.sources.len())
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
                sources,
            )
        };
        self.revalidate_mcp_resource_call(
            receipt,
            provider.as_ref(),
            prepared,
            &sources,
            revision,
            &ancestor_revisions,
            &cancel,
        )
        .await?;
        let result = if prepared.name == LIST_MCP_RESOURCES_TOOL {
            let mut results = Vec::new();
            for source in &sources {
                let mut scoped_input = prepared.input.clone();
                scoped_input["server"] = json!(source.name);
                // An omitted server must not bypass a server-specific Deny or silently satisfy
                // a server-specific Ask with an approval of different, unscoped arguments.
                let scoped_policy = if prepared.all_servers {
                    let chats = self.inner.chats.lock().await;
                    crate::delegation::effective_policy(&chats, receipt)?
                        .explicit_decision_with_session(
                            &prepared.name,
                            &scoped_input,
                            receipt.loop_id,
                            None,
                        )
                } else {
                    None
                };
                if matches!(scoped_policy, Some(Decision::Deny | Decision::Ask)) {
                    results.push(json!({"server":source.name,"error":"resource listing requires an explicit server call under the current policy"}));
                    continue;
                }
                self.check_delegated_call(receipt, &prepared.name, &scoped_input, None)
                    .await?;
                self.revalidate_mcp_resource_call(
                    receipt,
                    provider.as_ref(),
                    prepared,
                    &sources,
                    revision,
                    &ancestor_revisions,
                    &cancel,
                )
                .await?;
                let result = provider
                    .list_mcp_resources_from(
                        &source.name,
                        prepared.cursor.as_deref(),
                        cancel.clone(),
                    )
                    .await;
                self.check_delegated_call(receipt, &prepared.name, &scoped_input, None)
                    .await?;
                self.revalidate_mcp_resource_call(
                    receipt,
                    provider.as_ref(),
                    prepared,
                    &sources,
                    revision,
                    &ancestor_revisions,
                    &cancel,
                )
                .await?;
                let result = result.and_then(|result| annotate_list(result, source));
                if !prepared.all_servers {
                    return result.map(|result| ToolResult {
                        content: result.to_string(),
                        is_error: false,
                        output_ref: None,
                    });
                }
                results.push(match result {
                    Ok(result) => result,
                    Err(error) => json!({"server":source.name,"error":error}),
                });
            }
            json!({"servers":results})
        } else {
            let source = &sources[0];
            self.check_delegated_call(receipt, &prepared.name, &prepared.input, None)
                .await?;
            self.revalidate_mcp_resource_call(
                receipt,
                provider.as_ref(),
                prepared,
                &sources,
                revision,
                &ancestor_revisions,
                &cancel,
            )
            .await?;
            let result = if let Some((definition, relative)) = &prepared.skill {
                provider.read_remote_skill(definition, relative.as_deref(), cancel.clone()).await
                    .map(|text| json!({"contents":[{"uri":prepared.input["uri"],"mimeType":"text/markdown","text":text}]}))
            } else {
                provider
                    .read_mcp_resource_from(
                        &source.name,
                        prepared.input["uri"]
                            .as_str()
                            .ok_or("resource URI missing")?,
                        cancel.clone(),
                    )
                    .await
            };
            self.check_delegated_call(receipt, &prepared.name, &prepared.input, None)
                .await?;
            self.revalidate_mcp_resource_call(
                receipt,
                provider.as_ref(),
                prepared,
                &sources,
                revision,
                &ancestor_revisions,
                &cancel,
            )
            .await?;
            let result = result?;
            if let Some(uri) = prepared.input["uri"].as_str()
                && let Some(text) = sole_text(&result)
            {
                let chats = self.inner.chats.lock().await;
                let chat = chats.get(&receipt.chat_id).ok_or("chat unavailable")?;
                if read_unchanged(chat, uri, &digest(text)) {
                    return Ok(unchanged(uri));
                }
            }
            result
        };
        self.revalidate_mcp_resource_call(
            receipt,
            provider.as_ref(),
            prepared,
            &sources,
            revision,
            &ancestor_revisions,
            &cancel,
        )
        .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_tools::HostToolPermission;
    use tickloop_agent_protocol::{ChatId, RemoteSkillResource};

    struct Provider {
        sources: Vec<McpResourceServer>,
        skills: Vec<RemoteSkillDefinition>,
    }

    #[async_trait::async_trait]
    impl HostToolProvider for Provider {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        fn mcp_resource_servers(&self) -> Vec<McpResourceServer> {
            self.sources.clone()
        }
        fn remote_skills(&self) -> Vec<RemoteSkillDefinition> {
            self.skills.clone()
        }
        fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }
        async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
            unreachable!("resource preparation must not dispatch a call")
        }
    }

    fn source(name: &str, account: &str) -> McpResourceServer {
        McpResourceServer {
            name: name.into(),
            authority: HostToolAuthority::Mcp {
                server_id: format!("https://{name}.example"),
                account_id: account.into(),
            },
        }
    }

    #[test]
    fn cursor_is_opaque_and_bound_to_server_and_account() {
        let first = source("first", "account");
        let token = wrap_cursor(&first, "private-backend-cursor").unwrap();
        assert!(!token.contains("private-backend-cursor"));
        assert_eq!(
            unwrap_cursor(&first, &token).unwrap(),
            "private-backend-cursor"
        );
        assert!(unwrap_cursor(&source("second", "account"), &token).is_err());
        assert!(unwrap_cursor(&source("first", "different-account"), &token).is_err());
        assert!(unwrap_cursor(&first, "private-backend-cursor").is_err());
        assert!(wrap_cursor(&first, &"x".repeat(MAX_CURSOR_BYTES + 1)).is_err());
    }

    #[test]
    fn current_source_requires_unique_name_and_unchanged_identity() {
        let first = source("first", "a");
        assert!(source_is_current(
            &first,
            &[first.clone(), source("second", "b")]
        ));
        assert!(!source_is_current(&first, &[source("first", "changed")]));
        assert!(!source_is_current(&first, &[first.clone(), first.clone()]));
        assert!(!source_is_current(&first, &[]));
    }

    #[test]
    fn preparation_selects_exact_server_and_rejects_ambiguous_names() {
        let mut provider = Provider {
            sources: vec![source("first", "a"), source("second", "b")],
            skills: vec![],
        };
        let all =
            PreparedResourceCall::prepare(&provider, LIST_MCP_RESOURCES_TOOL, &json!({})).unwrap();
        assert_eq!(all.sources.len(), 2);
        let selected = PreparedResourceCall::prepare(
            &provider,
            READ_MCP_RESOURCE_TOOL,
            &json!({"server":"second","uri":"fixture://resource"}),
        )
        .unwrap();
        assert_eq!(selected.sources, vec![source("second", "b")]);
        assert!(
            PreparedResourceCall::prepare(
                &provider,
                LIST_MCP_RESOURCES_TOOL,
                &json!({"cursor":"raw"})
            )
            .is_err()
        );
        let token = wrap_cursor(&provider.sources[0], "raw").unwrap();
        let continued = PreparedResourceCall::prepare(
            &provider,
            LIST_MCP_RESOURCES_TOOL,
            &json!({"server":"first","cursor":token}),
        )
        .unwrap();
        assert_eq!(continued.cursor.as_deref(), Some("raw"));
        assert!(
            PreparedResourceCall::prepare(
                &provider,
                LIST_MCP_RESOURCES_TOOL,
                &json!({"server":"second","cursor":token})
            )
            .is_err()
        );
        provider.sources.push(source("first", "different"));
        assert!(
            PreparedResourceCall::prepare(
                &provider,
                LIST_MCP_RESOURCES_TOOL,
                &json!({"server":"first"})
            )
            .is_err()
        );
    }

    #[test]
    fn skill_uri_collision_does_not_redirect_to_another_server() {
        let uri = "skill://tickloop-artifacts/artifact-design/SKILL.md";
        let provider = Provider {
            sources: vec![source("tickloop-artifacts", "a"), source("other", "a")],
            skills: vec![RemoteSkillDefinition {
                name: "tickloop-artifacts:artifact-design".into(),
                description: "fixture".into(),
                source: SkillSource::Remote {
                    server_id: "https://tickloop-artifacts.example".into(),
                    account_id: "a".into(),
                    uri: uri.into(),
                    resources: vec![RemoteSkillResource {
                        uri: uri.into(),
                        digest: "fixture".into(),
                        size: 0,
                    }],
                },
            }],
        };
        let other = PreparedResourceCall::prepare(
            &provider,
            READ_MCP_RESOURCE_TOOL,
            &json!({"server":"other","uri":uri}),
        )
        .unwrap();
        assert!(other.skill.is_none());
        let artifact = PreparedResourceCall::prepare(
            &provider,
            READ_MCP_RESOURCE_TOOL,
            &json!({"server":"tickloop-artifacts","uri":uri}),
        )
        .unwrap();
        assert!(artifact.skill.is_some());
    }

    #[test]
    fn list_metadata_contains_bound_cursors_and_rejects_malformed_results() {
        let first = source("first", "a");
        let listed = annotate_list(
            json!({"resources":[{"uri":"fixture://one"}],"nextCursor":"raw"}),
            &first,
        )
        .unwrap();
        assert_eq!(listed["resources"][0]["server"], "first");
        assert_eq!(listed["server"], "first");
        assert_eq!(
            unwrap_cursor(&first, listed["nextCursor"].as_str().unwrap()).unwrap(),
            "raw"
        );
        assert!(annotate_list(json!({"resources":[null]}), &first).is_err());
        assert!(annotate_list(json!({"resources":[],"nextCursor":42}), &first).is_err());
        assert!(annotate_list(json!(null), &first).is_err());
    }
}
