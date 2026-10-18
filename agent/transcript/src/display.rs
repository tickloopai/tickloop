//! The display projection: what a user interface shows of a Chat, derived from its Lines alone.
//!
//! Display nodes are prompts and answers. A prompt's display id is its earliest source id
//! (`originalMessageId`, else its uuid; a prompt submitted under a ledger input has that input's
//! id). An answer's display id is its response id. Entries a compaction rewrote only alias the
//! node they copy. Internal entries (Loop records, compaction markers, host notes, protocol
//! data) are folded away: a prompt hangs under the display node its physical parent belongs to,
//! and an answer under the prompt its lifecycle record names.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tickloop_agent_protocol::{Content, ImageContext, InputContext, Protocol, Role};
use uuid::Uuid;

use crate::entry::{ChainEntry, InterruptedBy, Line, ResponseEnding, ResponseEntry, ResponseState};

/// The longest tool result text a tool card carries.
pub const DISPLAY_TOOL_RESULT_CHARS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayNode {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub kind: DisplayKind,
    pub blocks: Vec<DisplayBlock>,
    pub status: DisplayStatus,
    pub timestamp: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisplayKind {
    Prompt {
        input_id: Option<Uuid>,
    },
    /// One attempt of one Run; `attempt` is 0 until its lifecycle record is known.
    Answer {
        input_id: Option<Uuid>,
        attempt: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisplayBlock {
    Text {
        text: String,
    },
    /// The model's visible reasoning before or between its answer text and tool calls. The
    /// Transcript does not record when it started or ended; a host that wants its duration
    /// measures the streamed `Thinking` events.
    Thinking {
        text: String,
    },
    Image(ImageContext),
    ToolCall {
        call_id: String,
        name: String,
        input: serde_json::Value,
        result: Option<DisplayToolResult>,
    },
    /// A prompt that joined the answer at a tool boundary.
    FollowUp {
        prompt_id: Uuid,
        input_id: Option<Uuid>,
        text: String,
        images: Vec<ImageContext>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayToolResult {
    pub text: String,
    pub is_error: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisplayStatus {
    Complete,
    Streaming,
    Interrupted { by: InterruptedBy },
    Failed { error: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum DisplayChange {
    /// The node's whole current state, new or changed.
    Upsert(DisplayNode),
}

/// Applies Lines in store order and reports the display nodes each one changed.
#[derive(Clone, Debug, Default)]
pub struct DisplayProjector {
    nodes: HashMap<Uuid, DisplayNode>,
    order: Vec<Uuid>,
    /// The display node each applied chain entry is part of, or follows when it is internal.
    positions: HashMap<Uuid, Option<Uuid>>,
}

/// The display nodes of `lines`, in the order they first appeared.
pub fn project(lines: &[Line]) -> Vec<DisplayNode> {
    let mut projector = DisplayProjector::default();
    for line in lines {
        projector.apply(line);
    }
    projector.nodes().cloned().collect()
}

impl DisplayProjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node(&self, id: Uuid) -> Option<&DisplayNode> {
        self.nodes.get(&id)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &DisplayNode> {
        self.order.iter().filter_map(|id| self.nodes.get(id))
    }

    /// The display node a physical chain entry is part of or, for an internal entry, follows.
    pub fn canonical_id(&self, physical: Uuid) -> Option<Uuid> {
        self.positions.get(&physical).copied().flatten()
    }

    pub fn apply(&mut self, line: &Line) -> Vec<DisplayChange> {
        let changed = match line {
            Line::Response(response) => Some(self.apply_response(response)),
            _ => match line.chain_entry() {
                Some(entry) => self.apply_entry(entry),
                None => None,
            },
        };
        changed
            .and_then(|id| self.nodes.get(&id))
            .map(|node| vec![DisplayChange::Upsert(node.clone())])
            .unwrap_or_default()
    }

    fn apply_response(&mut self, response: &ResponseEntry) -> Uuid {
        let node = self.answer(response.response_id, None, &response.timestamp);
        node.parent = response.display_parent;
        node.kind = DisplayKind::Answer {
            input_id: response.input_id,
            attempt: response.attempt,
        };
        node.status = match &response.state {
            ResponseState::Started => DisplayStatus::Streaming,
            ResponseState::Ended { ending } => match ending {
                ResponseEnding::Completed => DisplayStatus::Complete,
                ResponseEnding::Interrupted { by } => DisplayStatus::Interrupted { by: *by },
                ResponseEnding::Failed { error } => DisplayStatus::Failed {
                    error: error.clone(),
                },
            },
        };
        response.response_id
    }

    fn apply_entry(&mut self, entry: &ChainEntry) -> Option<Uuid> {
        if self.positions.contains_key(&entry.uuid) {
            return None;
        }
        let follows = entry
            .parent_uuid
            .or(entry.logical_parent_uuid)
            .and_then(|parent| self.positions.get(&parent).copied().flatten());
        // A compaction copies whole entries; a fragment appended later to a copied message
        // carries the message's source id but holds new content.
        if !entry.continues_message
            && let Some(source) = entry.original_message_id
            && let Some(copied) = self.positions.get(&source).copied()
        {
            self.positions.insert(entry.uuid, copied);
            return None;
        }
        let is_prompt = entry.message.role == Role::User
            && entry.execution_mode.is_some()
            && !entry.is_compact_summary
            && !entry.continues_message;
        let (position, changed) = match entry.response_id {
            Some(response) => {
                let follow_up = is_prompt.then(|| DisplayBlock::FollowUp {
                    prompt_id: entry.original_message_id.unwrap_or(entry.uuid),
                    input_id: entry.input_id,
                    text: message_text(&entry.message.content),
                    images: prompt_images(entry),
                });
                let node = self.answer(response, follows, &entry.timestamp);
                match follow_up {
                    Some(block) => node.blocks.push(block),
                    None => add_answer_content(node, entry),
                }
                (Some(response), Some(response))
            }
            None if is_prompt => {
                let id = entry.original_message_id.unwrap_or(entry.uuid);
                let mut blocks = Vec::new();
                let text = message_text(&entry.message.content);
                if !text.is_empty() {
                    blocks.push(DisplayBlock::Text { text });
                }
                blocks.extend(prompt_images(entry).into_iter().map(DisplayBlock::Image));
                self.insert(DisplayNode {
                    id,
                    parent: follows,
                    kind: DisplayKind::Prompt {
                        input_id: entry.input_id,
                    },
                    blocks,
                    status: DisplayStatus::Complete,
                    timestamp: entry.timestamp.clone(),
                });
                (Some(id), Some(id))
            }
            None => (follows, None),
        };
        self.positions.insert(entry.uuid, position);
        // A copy seen before its source (a load from a later boundary) stands for the source, so
        // a later copy of the same source only aliases it.
        if !entry.continues_message
            && let Some(source) = entry.original_message_id
        {
            self.positions.entry(source).or_insert(position);
        }
        changed
    }

    fn insert(&mut self, node: DisplayNode) {
        if !self.nodes.contains_key(&node.id) {
            self.order.push(node.id);
        }
        self.nodes.insert(node.id, node);
    }

    /// The answer node, created streaming under `parent` when it is new.
    fn answer(&mut self, id: Uuid, parent: Option<Uuid>, timestamp: &str) -> &mut DisplayNode {
        if !self.nodes.contains_key(&id) {
            self.insert(DisplayNode {
                id,
                parent,
                kind: DisplayKind::Answer {
                    input_id: None,
                    attempt: 0,
                },
                blocks: Vec::new(),
                status: DisplayStatus::Streaming,
                timestamp: timestamp.to_owned(),
            });
        }
        self.nodes.get_mut(&id).unwrap()
    }
}

fn message_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn prompt_images(entry: &ChainEntry) -> Vec<ImageContext> {
    entry
        .contexts
        .clone()
        .and_then(|contexts| serde_json::from_value::<Vec<InputContext>>(contexts).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|context| match context {
            InputContext::Image(image) => Some(image),
            _ => None,
        })
        .collect()
}

/// Thinking continues the block right before it (Gemini stores each streamed thought part as
/// its own content), so one stretch of thinking is one block; a block never starts with only
/// whitespace.
fn add_thinking(node: &mut DisplayNode, text: &str) {
    match node.blocks.last_mut() {
        Some(DisplayBlock::Thinking { text: shown }) => shown.push_str(text),
        _ if !text.trim().is_empty() => node.blocks.push(DisplayBlock::Thinking {
            text: text.to_owned(),
        }),
        _ => {}
    }
}

/// The readable text of a Responses reasoning item: its reasoning text, else its summary.
fn responses_reasoning_text(item: &serde_json::Value) -> String {
    let texts = |field: &str| -> Vec<&str> {
        item[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| part["text"].as_str())
            .filter(|text| !text.is_empty())
            .collect()
    };
    let content = texts("content");
    let parts = if content.is_empty() {
        texts("summary")
    } else {
        content
    };
    parts.join("\n\n")
}

/// An answer shows its model text, its thinking (including Responses reasoning summaries) and
/// its tool cards; other protocol data and host-written user text stay out.
fn add_answer_content(node: &mut DisplayNode, entry: &ChainEntry) {
    let is_model_output = entry.message.role == Role::Assistant;
    for content in &entry.message.content {
        match content {
            Content::Text { text } if is_model_output => {
                node.blocks.push(DisplayBlock::Text { text: text.clone() });
            }
            Content::Thinking { thinking, .. } if is_model_output => add_thinking(node, thinking),
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value,
            } if is_model_output && value["type"] == "reasoning" => {
                add_thinking(node, &responses_reasoning_text(value));
            }
            Content::ToolUse { id, name, input } => node.blocks.push(DisplayBlock::ToolCall {
                call_id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                result: None,
            }),
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let card = node.blocks.iter_mut().find_map(|block| match block {
                    DisplayBlock::ToolCall {
                        call_id, result, ..
                    } if call_id == tool_use_id => Some(result),
                    _ => None,
                });
                if let Some(result) = card {
                    let text = content.text();
                    let truncated = text.chars().count() > DISPLAY_TOOL_RESULT_CHARS;
                    *result = Some(DisplayToolResult {
                        text: text.chars().take(DISPLAY_TOOL_RESULT_CHARS).collect(),
                        is_error: *is_error,
                        truncated,
                    });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::Message;

    fn entry(uuid: Uuid, parent: Option<Uuid>, role: Role, content: Vec<Content>) -> ChainEntry {
        let role_name = match role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };
        serde_json::from_value(serde_json::json!({
            "type": role_name,
            "uuid": uuid,
            "parentUuid": parent,
            "chatId": Uuid::nil(),
            "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "test",
            "message": Message { role, content },
            "humanFamilyOrigin": null,
        }))
        .unwrap()
    }

    fn tool_use(id: &str) -> Content {
        Content::ToolUse {
            id: id.into(),
            name: "Read".into(),
            input: serde_json::json!({}),
        }
    }

    fn tool_result(id: &str) -> Content {
        Content::ToolResult {
            tool_use_id: id.into(),
            content: format!("result {id}").into(),
            is_error: false,
        }
    }

    fn thinking(text: &str) -> Content {
        Content::Thinking {
            thinking: text.into(),
            signature: Some("signed".into()),
        }
    }

    /// Thinking shows where the model wrote it, once per model call, and empty thinking (a
    /// signature or redacted reasoning without text) shows nothing.
    #[test]
    fn an_answer_shows_its_thinking_in_order_and_skips_empty_thinking() {
        let prompt = Uuid::new_v4();
        let response = Uuid::new_v4();
        let mut prompt_entry = entry(
            prompt,
            None,
            Role::User,
            vec![Content::Text {
                text: "question".into(),
            }],
        );
        prompt_entry.execution_mode = Some(serde_json::json!("default"));
        let first_call = Uuid::new_v4();
        let mut call = entry(
            first_call,
            Some(prompt),
            Role::Assistant,
            vec![thinking("Look it up first."), tool_use("a")],
        );
        call.response_id = Some(response);
        let results = Uuid::new_v4();
        let mut result = entry(
            results,
            Some(first_call),
            Role::User,
            vec![tool_result("a")],
        );
        result.response_id = Some(response);
        let mut reply = entry(
            Uuid::new_v4(),
            Some(results),
            Role::Assistant,
            vec![
                thinking(" \n"),
                thinking("The result "),
                Content::ProtocolData {
                    protocol: Protocol::Google,
                    value: serde_json::json!({ "paired": true }),
                },
                thinking("answers it."),
                Content::Text {
                    text: "Answer.".into(),
                },
            ],
        );
        reply.response_id = Some(response);

        let nodes = project(
            &[prompt_entry, call, result, reply]
                .into_iter()
                .map(|entry| Line::chain(entry.message.role, entry))
                .collect::<Vec<_>>(),
        );
        let answer = &nodes[1];
        assert_eq!(answer.id, response);
        let shown = answer
            .blocks
            .iter()
            .map(|block| match block {
                DisplayBlock::Thinking { text } => format!("thinking: {text}"),
                DisplayBlock::Text { text } => format!("text: {text}"),
                DisplayBlock::ToolCall { call_id, .. } => format!("tool: {call_id}"),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [
                "thinking: Look it up first.",
                "tool: a",
                "thinking: The result answers it.",
                "text: Answer.",
            ]
        );
        let serialized = serde_json::to_string(&nodes).unwrap();
        assert!(!serialized.contains("signed"), "signatures stay out");
    }

    #[test]
    fn a_responses_reasoning_item_shows_its_text_or_summary() {
        let reasoning = |value: serde_json::Value| Content::ProtocolData {
            protocol: Protocol::Responses,
            value,
        };
        let mut node = DisplayNode {
            id: Uuid::new_v4(),
            parent: None,
            kind: DisplayKind::Answer {
                input_id: None,
                attempt: 1,
            },
            blocks: Vec::new(),
            status: DisplayStatus::Streaming,
            timestamp: String::new(),
        };
        let mut reply = entry(
            Uuid::new_v4(),
            None,
            Role::Assistant,
            vec![
                reasoning(serde_json::json!({
                    "type": "reasoning", "encrypted_content": "opaque",
                    "summary": [{ "type": "summary_text", "text": "Plan." },
                                { "type": "summary_text", "text": "Check." }],
                })),
                reasoning(serde_json::json!({ "type": "reasoning", "summary": [] })),
                Content::Text {
                    text: "Done.".into(),
                },
                reasoning(serde_json::json!({ "type": "message", "content": [{ "text": "no" }] })),
            ],
        );
        reply.response_id = Some(node.id);
        add_answer_content(&mut node, &reply);
        assert_eq!(
            node.blocks,
            [
                DisplayBlock::Thinking {
                    text: "Plan.\n\nCheck.".into()
                },
                DisplayBlock::Text {
                    text: "Done.".into()
                },
            ]
        );
        assert!(!serde_json::to_string(&node).unwrap().contains("opaque"));
    }

    /// A compaction's copy of an answer entry only aliases it, while a fragment appended to the
    /// copied message later still adds its content.
    #[test]
    fn copies_alias_and_later_fragments_of_a_copied_message_still_show() {
        let prompt = Uuid::new_v4();
        let response = Uuid::new_v4();
        let mut prompt_entry = entry(
            prompt,
            None,
            Role::User,
            vec![Content::Text {
                text: "question".into(),
            }],
        );
        prompt_entry.execution_mode = Some(serde_json::json!("default"));
        let assistant = Uuid::new_v4();
        let mut call = entry(
            assistant,
            Some(prompt),
            Role::Assistant,
            vec![tool_use("a"), tool_use("b")],
        );
        call.response_id = Some(response);
        let results = Uuid::new_v4();
        let mut first_result = entry(results, Some(assistant), Role::User, vec![tool_result("a")]);
        first_result.response_id = Some(response);
        let mut copy = call.clone();
        copy.uuid = Uuid::new_v4();
        copy.original_message_id = Some(assistant);
        let mut copied_results = first_result.clone();
        copied_results.uuid = Uuid::new_v4();
        copied_results.parent_uuid = Some(copy.uuid);
        copied_results.original_message_id = Some(results);
        let mut fragment = entry(
            Uuid::new_v4(),
            Some(copied_results.uuid),
            Role::User,
            vec![tool_result("b")],
        );
        fragment.response_id = Some(response);
        fragment.continues_message = true;
        fragment.original_message_id = Some(results);

        let nodes = project(
            &[
                prompt_entry,
                call,
                first_result,
                copy,
                copied_results,
                fragment,
            ]
            .into_iter()
            .map(|entry| Line::chain(entry.message.role, entry))
            .collect::<Vec<_>>(),
        );
        assert_eq!(nodes.len(), 2);
        let answer = &nodes[1];
        assert_eq!((answer.id, answer.parent), (response, Some(prompt)));
        let results = answer
            .blocks
            .iter()
            .map(|block| match block {
                DisplayBlock::ToolCall { result, .. } => result.as_ref().map(|r| r.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results, [Some("result a".into()), Some("result b".into())]);
    }
}
