//! Reads of a Page this Chat wrote. A Page is stored exactly as sent, so its source is already in
//! the model's own `ArtifactEdit` call; like Claude Code's `file_unchanged` Read, a read of the
//! revision that call saved answers with a note instead of sending the source a second time.

use std::collections::HashMap;

use serde_json::Value;
use tickloop_harness_protocol::{ChatSnapshot, Content, ToolResult};

use crate::mcp_tools::{ARTIFACT_EDIT_TOOL, ARTIFACT_READ_TOOL};

/// The read result with the source replaced by a note, when `result` reads back the revision of
/// a Page whose whole source a visible `ArtifactEdit` create or replace in this Chat saved.
pub(crate) fn unchanged_own_page(
    snapshot: &ChatSnapshot,
    name: &str,
    input: &Value,
    result: &ToolResult,
) -> Option<ToolResult> {
    if name != ARTIFACT_READ_TOOL || input["action"] != "read" || result.is_error {
        return None;
    }
    let mut read = serde_json::from_str::<Value>(&result.content).ok()?;
    if read["format"] != "page" {
        return None;
    }
    let revision = read["revision"].as_u64()?;
    let artifact_id = read["artifact_id"].as_str()?.to_owned();
    let source = read["source"].as_str()?.to_owned();
    let messages = &snapshot.messages[snapshot.active_start()..];
    let writes = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::ToolUse {
                id, name, input, ..
            } if name == ARTIFACT_EDIT_TOOL
                && matches!(input["action"].as_str(), Some("create" | "replace"))
                && input["content"].as_str() == Some(source.as_str()) =>
            {
                Some((id.as_str(), input))
            }
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let saved_by_this_chat = messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| match block {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            } => writes.get(tool_use_id.as_str()).is_some_and(|input| {
                serde_json::from_str::<Value>(&content.text()).is_ok_and(|receipt| {
                    receipt["revision"].as_u64() == Some(revision)
                        && receipt["artifact_id"]
                            .as_str()
                            .or_else(|| input["artifact_id"].as_str())
                            .is_some_and(|saved| {
                                tickloop_harness_artifacts::artifact_ids_match(saved, &artifact_id)
                            })
                })
            }),
            _ => false,
        });
    if !saved_by_this_chat {
        return None;
    }
    let object = read.as_object_mut()?;
    object.remove("source");
    object.insert(
        "source_unchanged".into(),
        format!(
            "Revision {revision} is exactly the content of your ArtifactEdit call that saved it in this conversation; edit from it instead of reading it again."
        )
        .into(),
    );
    Some(ToolResult {
        content: read.to_string(),
        ..result.clone()
    })
}
