//! The host's completion check: an isolated model call, without tools, that judges a goal's
//! completion report against the evidence the conversation already shows.
//!
//! The objective and the report are always sent whole. The conversation is fitted to the remaining
//! bytes the way approval evidence is: oversized entries first lose their middle, then the oldest
//! entries are omitted. Every cut is marked in place and counted in `conversation_truncation`, so
//! the check never mistakes the remainder for the complete conversation.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tickloop_harness_model::{ModelOutput, ModelRequest, StructuredOutput};
use tickloop_harness_protocol::{ChatSnapshot, Content, GoalVerdict, LocalMessage, Message, Role};

pub(crate) const CHECK_RESULT_NAME: &str = "goal_check_result";
const CHECK_SYSTEM_PROMPT: &str = include_str!("templates/completion_check.md");
const TRUNCATION_GUIDANCE: &str = "The host shortened the conversation to fit this check: conversation_truncation counts the oldest entries it omitted and the entries whose middle it cut, each cut marked in place. Omitted text is evidence of nothing; do not assume it held the missing proof.";
/// Any single entry uses at most this share of the conversation's bytes.
const ENTRY_SHARE_DIVISOR: usize = 4;
const MIN_ENTRY_BYTES: usize = 2 * 1024;
/// Strings this short are never cut, and a cut string keeps at least this much text.
const MIN_CUT_STRING_BYTES: usize = 512;

/// One piece of conversation evidence, in the order the Chat recorded it. A tool result keeps
/// its call's id, because concurrent calls' results are recorded in completion order and the
/// call itself may be omitted to fit the check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum EvidenceEntry {
    /// What the user wrote.
    User {
        text: String,
    },
    /// Input the host or another Agent added: a goal continuation, a Tick or Artifact delivery,
    /// a scheduled instruction, a notification or an Agent message. It is not the user's word.
    HostInput {
        text: String,
    },
    Agent {
        text: String,
    },
    ToolCall {
        call_id: String,
        tool: String,
        input: String,
    },
    ToolResult {
        call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        is_error: bool,
        output: String,
    },
}

impl EvidenceEntry {
    fn text_mut(&mut self) -> &mut String {
        match self {
            Self::User { text } | Self::HostInput { text } | Self::Agent { text } => text,
            Self::ToolCall { input, .. } => input,
            Self::ToolResult { output, .. } => output,
        }
    }
}

/// Oldest entries omitted, entries whose middle was cut, and the bytes both removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct EvidenceTruncation {
    pub omitted_oldest: usize,
    pub shortened: usize,
    pub omitted_bytes: usize,
}

#[derive(Serialize)]
struct CheckPayload<'a> {
    objective: &'a str,
    completion_report: &'a str,
    conversation: Vec<EvidenceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_truncation: Option<EvidenceTruncation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckReply {
    verdict: ReplyVerdict,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReplyVerdict {
    Met,
    NotMet,
    Impossible,
}

/// The Chat's conversation as check evidence: user and Agent text, tool calls and their text
/// results. Only a prompt the user submitted counts as the user's, recognized by its recorded
/// execution mode and never by its wording; whatever else entered as a user message is host
/// input. Reasoning, images and provider data are not evidence the check
/// can weigh.
pub(crate) fn conversation_evidence(snapshot: &ChatSnapshot) -> Vec<EvidenceEntry> {
    let mut tool_names = HashMap::<&str, &str>::new();
    let mut entries = Vec::new();
    for (index, message) in snapshot.messages.iter().enumerate() {
        // A prompt the user submitted carries its execution mode, which the Transcript keeps
        // with it; Trigger input, notifications, deliveries and steering never do.
        let is_users_prompt = snapshot.prompt_execution_modes.contains_key(&index)
            && !snapshot
                .message_ids
                .get(&index)
                .is_some_and(|id| snapshot.agent_message_origins.contains_key(id));
        for content in &message.content {
            match content {
                Content::Text { text } if text.trim().is_empty() => {}
                Content::Text { text } => match message.role {
                    Role::User if is_users_prompt && LocalMessage::parse(text).is_none() => {
                        entries.push(EvidenceEntry::User { text: text.clone() })
                    }
                    Role::User => entries.push(EvidenceEntry::HostInput { text: text.clone() }),
                    Role::Assistant => entries.push(EvidenceEntry::Agent { text: text.clone() }),
                    Role::System => {}
                },
                Content::ToolUse { id, name, input } => {
                    tool_names.insert(id, name);
                    entries.push(EvidenceEntry::ToolCall {
                        call_id: id.clone(),
                        tool: name.clone(),
                        input: input.to_string(),
                    });
                }
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => entries.push(EvidenceEntry::ToolResult {
                    call_id: tool_use_id.clone(),
                    tool: tool_names
                        .get(tool_use_id.as_str())
                        .map(|name| (*name).to_owned()),
                    is_error: *is_error,
                    output: content.text().into_owned(),
                }),
                Content::Thinking { .. } | Content::ProtocolData { .. } | Content::Image { .. } => {
                }
            }
        }
    }
    entries
}

/// The check's request; its user payload stays within `budget_bytes` unless the objective and
/// the report alone exceed it.
pub(crate) fn check_request(
    objective: &str,
    completion_report: &str,
    conversation: Vec<EvidenceEntry>,
    budget_bytes: usize,
) -> ModelRequest {
    let fixed_bytes = json_len(&CheckPayload {
        objective,
        completion_report,
        conversation: Vec::new(),
        conversation_truncation: Some(EvidenceTruncation {
            omitted_oldest: usize::MAX,
            shortened: usize::MAX,
            omitted_bytes: usize::MAX,
        }),
    });
    let (conversation, conversation_truncation) =
        fit(conversation, budget_bytes.saturating_sub(fixed_bytes));
    let mut system_prompt = CHECK_SYSTEM_PROMPT.trim_end().to_owned();
    if conversation_truncation.is_some() {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(TRUNCATION_GUIDANCE);
    }
    let payload = CheckPayload {
        objective,
        completion_report,
        conversation,
        conversation_truncation,
    };
    ModelRequest {
        history_sources: Vec::new(),
        messages: vec![
            Message {
                role: Role::System,
                content: vec![Content::Text {
                    text: system_prompt,
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: serde_json::to_string(&payload).expect("goal check payload serializes"),
                }],
            },
        ],
        tools: Vec::new(),
        structured_output: Some(StructuredOutput {
            name: CHECK_RESULT_NAME.into(),
            schema: json!({"type":"object","properties":{
                "verdict":{"type":"string","enum":["met","not_met","impossible"]},
                "reason":{"type":"string"}
            },"required":["verdict","reason"],"additionalProperties":false}),
        }),
        temperature: Some(0.0),
        reasoning_effort: None,
    }
}

pub(crate) fn parse_check_reply(output: &ModelOutput) -> Result<GoalVerdict, &'static str> {
    if !matches!(output.stop_reason.as_str(), "end_turn" | "stop") {
        return Err("completion check response did not finish");
    }
    if output.content.iter().any(|content| {
        matches!(
            content,
            Content::ToolUse { .. } | Content::ToolResult { .. }
        )
    }) {
        return Err("completion check returned a tool operation");
    }
    let text = output
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let reply = serde_json::from_str::<CheckReply>(unfenced(&text)?)
        .map_err(|_| "completion check returned an invalid verdict")?;
    let reason = reply.reason.trim().to_owned();
    match reply.verdict {
        ReplyVerdict::Met => Ok(GoalVerdict::Met),
        ReplyVerdict::NotMet | ReplyVerdict::Impossible if reason.is_empty() => {
            Err("completion check gave no reason")
        }
        ReplyVerdict::NotMet => Ok(GoalVerdict::NotMet { reason }),
        ReplyVerdict::Impossible => Ok(GoalVerdict::Impossible { reason }),
    }
}

fn unfenced(text: &str) -> Result<&str, &'static str> {
    let text = text.trim();
    let Some(fenced) = text.strip_prefix("```") else {
        return Ok(text);
    };
    let (language, body) = fenced
        .split_once('\n')
        .ok_or("completion check returned an unfinished JSON fence")?;
    if !language.trim().is_empty() && !language.trim().eq_ignore_ascii_case("json") {
        return Err("completion check returned an unsupported fence");
    }
    body.trim_end()
        .strip_suffix("```")
        .map(str::trim)
        .ok_or("completion check returned an unfinished JSON fence")
}

fn fit(
    mut entries: Vec<EvidenceEntry>,
    budget: usize,
) -> (Vec<EvidenceEntry>, Option<EvidenceTruncation>) {
    if json_len(&entries) <= budget {
        return (entries, None);
    }
    let mut truncation = EvidenceTruncation::default();
    let entry_cap = (budget / ENTRY_SHARE_DIVISOR).max(MIN_ENTRY_BYTES);
    for entry in &mut entries {
        let excess = json_len(entry).saturating_sub(entry_cap);
        let removed = cut_middle(entry.text_mut(), excess);
        if removed > 0 {
            truncation.shortened += 1;
            truncation.omitted_bytes += removed;
        }
    }
    let sizes = entries.iter().map(json_len).collect::<Vec<_>>();
    let array_len = |kept: &[usize]| 2 + kept.iter().sum::<usize>() + kept.len().saturating_sub(1);
    let mut omitted = 0;
    while omitted < sizes.len() && array_len(&sizes[omitted..]) > budget {
        truncation.omitted_oldest += 1;
        truncation.omitted_bytes += sizes[omitted];
        omitted += 1;
    }
    entries.drain(..omitted);
    (entries, Some(truncation))
}

/// Replaces the middle of `text` with an omission marker so it loses at least `excess` bytes,
/// keeping its head and tail and never fewer than `MIN_CUT_STRING_BYTES`. Returns the bytes
/// removed.
fn cut_middle(text: &mut String, excess: usize) -> usize {
    let marker_bytes = omission_marker(usize::MAX).len();
    if excess == 0 || text.len() <= MIN_CUT_STRING_BYTES {
        return 0;
    }
    let keep = text
        .len()
        .saturating_sub(excess + marker_bytes)
        .max(MIN_CUT_STRING_BYTES);
    let mut head = keep / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (keep - keep / 2);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let removed = tail.saturating_sub(head);
    if removed <= marker_bytes {
        return 0;
    }
    text.replace_range(head..tail, &omission_marker(removed));
    removed
}

/// The in-place mark of a cut. It needs no JSON escaping, so its serialized size is its length.
fn omission_marker(bytes: usize) -> String {
    format!(" [... {bytes} bytes omitted by the host to fit the completion check ...] ")
}

fn json_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value)
        .expect("goal check evidence serializes")
        .len()
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;
