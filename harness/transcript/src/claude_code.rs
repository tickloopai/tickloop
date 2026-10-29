//! The Claude Code projection: a Chat's conversation as the JSONL session file Claude Code
//! writes, which hook scripts read through `transcript_path`.
//!
//! Like Claude Code, every content block is its own line, carrying `type`, `message.role` and a
//! one-block `message.content` of `text`, `tool_use` or `tool_result`. Only what the
//! conversation shows the model is projected: thinking, protocol data, images, host notes and
//! every record outside the conversation stay out. The projection is append-only like the file
//! it feeds: a Line already projected, or a compaction's copy of one, adds nothing again.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tickloop_harness_protocol::Content;
use uuid::Uuid;

use crate::entry::{ChainEntry, Line};

/// One line of the projected session file.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeLine {
    pub parent_uuid: Option<Uuid>,
    pub is_sidechain: bool,
    #[serde(rename = "type")]
    pub kind: ClaudeCodeRole,
    pub message: ClaudeCodeMessage,
    pub uuid: Uuid,
    pub timestamp: String,
    pub session_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeCodeRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClaudeCodeMessage {
    pub role: ClaudeCodeRole,
    /// Exactly one block.
    pub content: Vec<ClaudeCodeBlock>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeCodeBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result's text; images it carried are left out.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// Applies Lines in store order and gives the session lines each one adds.
#[derive(Clone, Debug, Default)]
pub struct ClaudeCodeProjector {
    /// For every chain entry applied, and every compaction source among them, the last session
    /// line projected from it or, when it added none, before it.
    positions: HashMap<Uuid, Option<Uuid>>,
}

impl ClaudeCodeProjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, line: &Line) -> Vec<ClaudeCodeLine> {
        let Some(entry) = line.chain_entry() else {
            return Vec::new();
        };
        if self.positions.contains_key(&entry.uuid) {
            return Vec::new();
        }
        // A compaction copies whole entries; a fragment appended later to a copied message
        // carries the message's source id but holds new content.
        let copied_source = entry
            .original_message_id
            .filter(|_| !entry.continues_message);
        if let Some(position) = copied_source.and_then(|source| self.positions.get(&source)) {
            self.positions.insert(entry.uuid, *position);
            return Vec::new();
        }
        let parent = entry
            .parent_uuid
            .or(entry.logical_parent_uuid)
            .and_then(|parent| self.positions.get(&parent).copied().flatten());
        let role = match line {
            Line::User(_) => Some(ClaudeCodeRole::User),
            Line::Assistant(_) => Some(ClaudeCodeRole::Assistant),
            _ => None,
        };
        let (Some(role), true) = (role, shows_the_model(entry)) else {
            self.place(entry.uuid, copied_source, parent);
            return Vec::new();
        };
        let mut previous = parent;
        let projected = conversation_blocks(entry)
            .into_iter()
            .enumerate()
            .map(|(index, block)| {
                let uuid = block_uuid(entry.uuid, index);
                let line = ClaudeCodeLine {
                    parent_uuid: previous,
                    is_sidechain: entry.is_sidechain,
                    kind: role,
                    message: ClaudeCodeMessage {
                        role,
                        content: vec![block],
                    },
                    uuid,
                    timestamp: entry.timestamp.clone(),
                    session_id: entry.chat_id,
                    cwd: entry.cwd.clone(),
                    version: entry.version.clone(),
                };
                previous = Some(uuid);
                line
            })
            .collect();
        self.place(entry.uuid, copied_source, previous);
        projected
    }

    fn place(&mut self, entry: Uuid, copied_source: Option<Uuid>, position: Option<Uuid>) {
        self.positions.insert(entry, position);
        if let Some(source) = copied_source {
            self.positions.insert(source, position);
        }
    }
}

/// The session lines of `lines`, in store order.
pub fn project(lines: &[Line]) -> Vec<ClaudeCodeLine> {
    let mut projector = ClaudeCodeProjector::new();
    lines
        .iter()
        .flat_map(|line| projector.apply(line))
        .collect()
}

/// Host notes are not conversation; a compaction summary is, though shown in the transcript
/// view alone.
fn shows_the_model(entry: &ChainEntry) -> bool {
    !entry.is_meta && entry.loop_record.is_none()
}

fn conversation_blocks(entry: &ChainEntry) -> Vec<ClaudeCodeBlock> {
    entry
        .message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } if !text.trim().is_empty() => {
                Some(ClaudeCodeBlock::Text { text: text.clone() })
            }
            Content::ToolUse { id, name, input } => Some(ClaudeCodeBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(ClaudeCodeBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.text().into_owned(),
                is_error: *is_error,
            }),
            _ => None,
        })
        .collect()
}

/// The first block keeps its entry's id; each later one gets an id derived from the entry's id
/// and its place, so projecting the same Line again gives the same lines.
fn block_uuid(entry: Uuid, index: usize) -> Uuid {
    if index == 0 {
        return entry;
    }
    let digest = Sha256::new()
        .chain_update(entry.as_bytes())
        .chain_update((index as u64).to_be_bytes())
        .finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

#[cfg(test)]
mod tests;
