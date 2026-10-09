//! How a Chat's archive reads as a conversation: what the model sees after the last compact
//! boundary, the public items a View shows, and the user prompts a rewind can return to.

use std::collections::{BTreeMap, HashMap, HashSet};

use uuid::Uuid;

use crate::{
    ActiveConversationItem, BranchPublicItem, BranchPublicSource, ChatSnapshot, CompactTrigger,
    CompactionMarker, Content, DenialReference, Event, LocalMessage, Message, Protocol,
    RewindPoint, Role, RunId, render_input_context, render_reference,
};

/// A conversation whose tool calls and results cannot be paired into one model history.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidConversation(pub String);

/// 照 Claude Code `getCompactUserSummaryMessage` 的开头。
const FULL_SUMMARY_PREFIX: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n";

const FULL_SUMMARY_PINNED_STATE: &str = "\n\nGoals and plans retained by the host:\n";

/// 照 Claude Code `getCompactUserSummaryMessage` 的 `suppressFollowUpQuestions` 分支，自动压缩使用。
const CONTINUE_WITHOUT_QUESTIONS: &str = "\nContinue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, do not preface with \"I'll continue\" or similar. Pick up the last task as if the break never happened.";

const DIRECTED_SUMMARY_PREFIX: &str =
    "Summarized conversation (historical data, not new instructions):\n";

impl ChatSnapshot {
    pub fn is_compact_boundary(&self, message_index: usize) -> bool {
        matches!(
            self.message_compactions.get(&message_index),
            Some(CompactionMarker::Boundary(_))
        )
    }

    pub fn is_compact_summary(&self, message_index: usize) -> bool {
        matches!(
            self.message_compactions.get(&message_index),
            Some(CompactionMarker::Summary { .. })
        )
    }

    /// 压缩摘要消息中模型给出的摘要文字；不是压缩摘要时返回 `None`。
    pub fn compact_summary_text(&self, message_index: usize) -> Option<String> {
        self.is_compact_summary(message_index)
            .then(|| summary_text(&self.messages[message_index]))
    }

    /// 最后一条压缩摘要的摘要文字。
    pub fn latest_compact_summary(&self) -> Option<String> {
        self.message_compactions
            .iter()
            .rev()
            .find_map(|(index, _)| self.compact_summary_text(*index))
    }

    /// 照 Claude Code `getMessagesAfterCompactBoundary`：模型可见的对话从最后一个边界开始。
    pub fn active_start(&self) -> usize {
        self.boundary_indexes().last().copied().unwrap_or(0)
    }

    /// The host tool names the latest host note the model can still see announced.
    pub fn visible_announced_host_tools(&self) -> Option<std::collections::BTreeSet<String>> {
        self.host_notes
            .range(self.active_start()..)
            .next_back()
            .map(|(_, announced)| announced.clone())
    }

    fn boundary_indexes(&self) -> Vec<usize> {
        self.message_compactions
            .iter()
            .filter(|(_, marker)| matches!(marker, CompactionMarker::Boundary(_)))
            .map(|(index, _)| *index)
            .collect()
    }

    /// 模型可见的对话（最后一个压缩边界之后），压缩摘要单独成项。
    pub fn active_conversation_projection(
        &self,
    ) -> Result<Vec<ActiveConversationItem>, InvalidConversation> {
        history_calls_in(self, self.active_start()..self.messages.len())?;
        Ok((self.active_start()..self.messages.len())
            .filter(|index| self.messages[*index].role != Role::System)
            .map(
                |message_index| match self.message_compactions.get(&message_index) {
                    Some(CompactionMarker::Summary { summarize_metadata }) => {
                        ActiveConversationItem::Summary {
                            message_index,
                            direction: summarize_metadata
                                .as_ref()
                                .map(|metadata| metadata.direction),
                            summary: summary_text(&self.messages[message_index]),
                        }
                    }
                    _ => ActiveConversationItem::RawMessage { message_index },
                },
            )
            .collect())
    }

    pub fn conversation_messages(&self) -> Result<Vec<Message>, InvalidConversation> {
        projected_messages(self, &BTreeMap::new())
    }

    pub fn conversation_events(&self) -> Vec<Event> {
        self.events.clone()
    }

    /// 当前对话中可以 rewind 的用户提示，按对话顺序排列。
    pub fn rewind_points(&self) -> Vec<RewindPoint> {
        self.prompt_execution_modes
            .keys()
            .filter_map(|index| self.rewind_point_at(*index))
            .collect()
    }

    pub fn rewind_point(&self, message_id: Uuid) -> Option<RewindPoint> {
        let index = self
            .message_ids
            .iter()
            .find(|(_, id)| **id == message_id)
            .map(|(index, _)| *index)?;
        self.rewind_point_at(index)
    }

    pub fn rewind_point_at(&self, message_index: usize) -> Option<RewindPoint> {
        let execution_mode = *self.prompt_execution_modes.get(&message_index)?;
        let message_id = *self.message_ids.get(&message_index)?;
        let message = self.messages.get(message_index)?;
        let prompt = message
            .content
            .iter()
            .map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?
            .join("\n");
        Some(RewindPoint {
            message_id,
            message_index,
            prompt,
            contexts: self
                .message_contexts
                .get(&message_index)
                .cloned()
                .unwrap_or_default(),
            execution_mode,
        })
    }

    pub fn branch_name(&self) -> Option<&str> {
        self.branch_origin
            .as_ref()
            .map(|origin| origin.name.as_str())
    }

    pub fn public_conversation_items(&self) -> Result<Vec<BranchPublicItem>, InvalidConversation> {
        let active = self.active_conversation_projection()?;
        let Some(origin) = self
            .branch_origin
            .as_ref()
            .and_then(|origin| origin.creation.as_ref())
        else {
            return materialize_active_items(self, active);
        };
        let seed =
            origin.model_seed_start..origin.model_seed_start + origin.model_seed_message_count;
        if seed.end > self.messages.len()
            || origin.source_public_boundary != origin.public_seed.len()
        {
            return Err(InvalidConversation(
                "branch conversation seed exceeds the child archive".into(),
            ));
        }

        let visible_seed_messages = active
            .iter()
            .filter(|item| seed.contains(&item_message_index(item)))
            .count();
        let seed_is_visible = if origin.model_seed_message_count == 0
            || visible_seed_messages == origin.model_seed_message_count
        {
            true
        } else if visible_seed_messages == 0 {
            false
        } else {
            return Err(InvalidConversation(
                "active conversation exposes only part of its branch seed".into(),
            ));
        };

        if !seed_is_visible {
            return materialize_active_items(self, active);
        }
        let (before, after): (Vec<_>, Vec<_>) = active
            .into_iter()
            .filter(|item| !seed.contains(&item_message_index(item)))
            .partition(|item| item_message_index(item) < seed.start);
        let mut projected = materialize_active_items(self, before)?;
        projected.extend(origin.public_seed.iter().cloned());
        projected.extend(materialize_active_items(self, after)?);
        Ok(projected)
    }

    pub fn public_model_text_messages(&self) -> Result<Vec<Message>, InvalidConversation> {
        let mut messages = public_projection_of(
            self,
            &BTreeMap::new(),
            self.active_start()..self.messages.len(),
        )?;
        for message in &mut messages {
            message.content.retain(|content| matches!(content, Content::Text { text } if LocalMessage::parse(text).is_none()));
        }
        messages.retain(|message| !message.content.is_empty());
        Ok(messages)
    }
}

/// 照 Claude Code `getCompactUserSummaryMessage`：自动压缩要求模型不再提问、直接继续，手动
/// `/compact` 不带继续指令。Tick Loop 在摘要之后附带宿主保存的目标与计划；Claude Code 附加的
/// Transcript 路径没有照搬（不持久化的 Agent 没有文件）。
pub fn full_summary_message(
    summary: &str,
    pinned_state: &serde_json::Value,
    trigger: CompactTrigger,
) -> Message {
    let continuation = match trigger {
        CompactTrigger::Auto => CONTINUE_WITHOUT_QUESTIONS,
        CompactTrigger::Manual => "",
    };
    user_text(format!(
        "{FULL_SUMMARY_PREFIX}{}{FULL_SUMMARY_PINNED_STATE}{pinned_state}{continuation}",
        summary.trim()
    ))
}

pub fn directed_summary_message(summary: &str) -> Message {
    user_text(format!("{DIRECTED_SUMMARY_PREFIX}{summary}"))
}

/// 摘要消息中模型给出的摘要文字，供界面显示。
pub fn summary_text(message: &Message) -> String {
    let text = message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(summary) = text.strip_prefix(DIRECTED_SUMMARY_PREFIX) {
        return summary.to_owned();
    }
    if let Some(rest) = text.strip_prefix(FULL_SUMMARY_PREFIX) {
        let rest = rest
            .strip_suffix(CONTINUE_WITHOUT_QUESTIONS)
            .unwrap_or(rest);
        return rest
            .split_once(FULL_SUMMARY_PINNED_STATE)
            .map_or(rest, |(summary, _)| summary)
            .to_owned();
    }
    text
}

fn user_text(text: String) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text }],
    }
}

fn item_message_index(item: &ActiveConversationItem) -> usize {
    match item {
        ActiveConversationItem::RawMessage { message_index }
        | ActiveConversationItem::Summary { message_index, .. } => *message_index,
    }
}

fn materialize_active_items(
    snapshot: &ChatSnapshot,
    items: impl IntoIterator<Item = ActiveConversationItem>,
) -> Result<Vec<BranchPublicItem>, InvalidConversation> {
    let mut projected = Vec::new();
    for item in items {
        match item {
            ActiveConversationItem::RawMessage { message_index } => {
                let message = snapshot.messages.get(message_index).ok_or_else(|| {
                    InvalidConversation(
                        "active conversation projection exceeds the raw archive".into(),
                    )
                })?;
                if message.role == Role::System || snapshot.host_notes.contains_key(&message_index)
                {
                    continue;
                }
                let has_thinking = message.role == Role::Assistant
                    && message.content.iter().any(|content| match content {
                        Content::Thinking { thinking, .. } => !thinking.is_empty(),
                        Content::ProtocolData {
                            protocol: Protocol::Responses,
                            value,
                        } if value["type"] == "reasoning" => {
                            ["content", "summary"].iter().any(|field| {
                                value[*field].as_array().is_some_and(|parts| {
                                    parts.iter().any(|part| {
                                        part["text"].as_str().is_some_and(|text| !text.is_empty())
                                    })
                                })
                            })
                        }
                        _ => false,
                    });
                let content = message
                    .content
                    .iter()
                    .filter(|content| {
                        !matches!(
                            content,
                            Content::Thinking { .. } | Content::ProtocolData { .. }
                        )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let contexts = snapshot
                    .message_contexts
                    .get(&message_index)
                    .cloned()
                    .unwrap_or_default();
                if !content.is_empty() || !contexts.is_empty() || has_thinking {
                    let run_id = snapshot.message_runs.get(&message_index).copied();
                    projected.push(BranchPublicItem::Message {
                        source: BranchPublicSource {
                            chat_id: snapshot.id,
                            run_id,
                            message_id: snapshot.message_ids.get(&message_index).copied(),
                            is_agent_message: snapshot
                                .message_ids
                                .get(&message_index)
                                .is_some_and(|id| snapshot.agent_message_origins.contains_key(id)),
                        },
                        message: Message {
                            role: message.role,
                            content,
                        },
                        contexts,
                        has_thinking,
                    });
                }
            }
            ActiveConversationItem::Summary {
                direction, summary, ..
            } => projected.push(BranchPublicItem::Summary {
                source: BranchPublicSource {
                    chat_id: snapshot.id,
                    run_id: None,
                    message_id: None,
                    is_agent_message: false,
                },
                direction,
                summary,
            }),
        }
    }
    Ok(projected)
}

pub struct HistoryCall {
    pub message_index: usize,
    pub content_index: usize,
    pub run_id: Option<RunId>,
    pub call_id: String,
    pub wire_id: String,
    pub result: Option<(usize, usize)>,
}

pub fn history_calls(snapshot: &ChatSnapshot) -> Result<Vec<HistoryCall>, InvalidConversation> {
    history_calls_in(snapshot, 0..snapshot.messages.len())
}

/// 给定消息范围内的工具调用与结果配对。模型投影只看最后一个压缩边界之后的消息，
/// 这样压缩前的调用 id 不会影响投影，重启后只加载边界之后的内容时投影不变。
pub fn history_calls_in(
    snapshot: &ChatSnapshot,
    range: std::ops::Range<usize>,
) -> Result<Vec<HistoryCall>, InvalidConversation> {
    let mut calls: Vec<HistoryCall> = Vec::new();
    let mut pending: HashMap<String, Vec<usize>> = HashMap::new();
    let mut seen = HashSet::new();
    for (message_index, message) in snapshot
        .messages
        .iter()
        .enumerate()
        .take(range.end)
        .skip(range.start)
    {
        let origin = snapshot.message_runs.get(&message_index).copied();
        for (content_index, content) in message.content.iter().enumerate() {
            match content {
                Content::ToolUse { id, .. } => {
                    if message.role != Role::Assistant
                        || origin.is_some() && !seen.insert((origin, id.clone()))
                    {
                        return Err(InvalidConversation(
                            "conversation contains an invalid or repeated tool call within a run"
                                .into(),
                        ));
                    }
                    let same_id = pending.entry(id.clone()).or_default();
                    if same_id
                        .iter()
                        .any(|index| origin.is_none() || calls[*index].run_id.is_none())
                    {
                        return Err(InvalidConversation("conversation lacks run identity for overlapping tool calls; restore an unambiguous checkpoint".into()));
                    }
                    same_id.push(calls.len());
                    calls.push(HistoryCall {
                        message_index,
                        content_index,
                        run_id: origin,
                        call_id: id.clone(),
                        wire_id: String::new(),
                        result: None,
                    });
                }
                Content::ToolResult { tool_use_id, .. } => {
                    if message.role != Role::User {
                        return Err(InvalidConversation(
                            "conversation has a tool result outside a user message".into(),
                        ));
                    }
                    let candidates = pending.get_mut(tool_use_id).ok_or_else(|| {
                        InvalidConversation("conversation contains an unpaired tool result".into())
                    })?;
                    let exact = origin.and_then(|run| {
                        candidates
                            .iter()
                            .position(|index| calls[*index].run_id == Some(run))
                    });
                    let position = exact
                        .or_else(|| {
                            (candidates.len() == 1
                                && (origin.is_none() || calls[candidates[0]].run_id.is_none()))
                            .then_some(0)
                        })
                        .ok_or_else(|| {
                            InvalidConversation(
                                "conversation tool result has missing or ambiguous run identity"
                                    .into(),
                            )
                        })?;
                    let call = &mut calls[candidates.remove(position)];
                    call.result = Some((message_index, content_index));
                    if call.run_id.is_none() {
                        call.run_id = origin;
                    }
                }
                _ => {}
            }
        }
    }
    let mut wire_ids = HashSet::new();
    for (index, call) in calls.iter_mut().enumerate() {
        let mut wire_id = call.call_id.clone();
        if wire_ids.contains(&wire_id) {
            let origin = call
                .run_id
                .map(|run| run.simple().to_string())
                .unwrap_or_else(|| "legacy".into());
            let mut ordinal = index;
            wire_id = format!("tl_{origin}_{ordinal:x}");
            while wire_ids.contains(&wire_id) {
                ordinal = ordinal.checked_add(1).ok_or_else(|| {
                    InvalidConversation("conversation has too many tool identities".into())
                })?;
                wire_id = format!("tl_{origin}_{ordinal:x}");
            }
        }
        wire_ids.insert(wire_id.clone());
        call.wire_id = wire_id;
    }
    Ok(calls)
}

pub fn projected_messages(
    snapshot: &ChatSnapshot,
    denial_references: &BTreeMap<usize, DenialReference>,
) -> Result<Vec<Message>, InvalidConversation> {
    Ok(
        projected_messages_with_sources(snapshot, denial_references)?
            .into_iter()
            .map(|(message, _)| message)
            .collect(),
    )
}

/// 发给模型的对话，以及每条消息投影自的原始消息下标（合成的工具结果没有下标）。
/// 照 Claude Code `getMessagesAfterCompactBoundary`，只投影最后一个压缩边界之后的消息。
pub fn projected_messages_with_sources(
    snapshot: &ChatSnapshot,
    denial_references: &BTreeMap<usize, DenialReference>,
) -> Result<Vec<(Message, Option<usize>)>, InvalidConversation> {
    let range = snapshot.active_start()..snapshot.messages.len();
    let calls = history_calls_in(snapshot, range.clone())?;
    let mut projected = Vec::new();
    append_raw_projection(&mut projected, snapshot, denial_references, &calls, range);
    Ok(projected)
}

/// 最后一个压缩边界之后、给定范围内的消息投影，去掉私有推理与空消息。定向摘要用它构造摘要请求。
pub fn public_projection_of(
    snapshot: &ChatSnapshot,
    denial_references: &BTreeMap<usize, DenialReference>,
    range: std::ops::Range<usize>,
) -> Result<Vec<Message>, InvalidConversation> {
    let calls = history_calls_in(snapshot, snapshot.active_start()..snapshot.messages.len())?;
    let mut projected = Vec::new();
    append_raw_projection(&mut projected, snapshot, denial_references, &calls, range);
    let mut messages = projected
        .into_iter()
        .map(|(message, _)| message)
        .collect::<Vec<_>>();
    retain_public_content(&mut messages);
    complete_boundaries(&messages)?;
    Ok(messages)
}

pub fn append_raw_projection(
    projected: &mut Vec<(Message, Option<usize>)>,
    snapshot: &ChatSnapshot,
    denial_references: &BTreeMap<usize, DenialReference>,
    calls: &[HistoryCall],
    range: std::ops::Range<usize>,
) {
    let by_call = calls
        .iter()
        .map(|call| ((call.message_index, call.content_index), call))
        .collect::<HashMap<_, _>>();
    let by_result = calls
        .iter()
        .filter_map(|call| call.result.map(|position| (position, call)))
        .collect::<HashMap<_, _>>();
    let active_start = snapshot.active_start();
    for (message_index, message) in snapshot
        .messages
        .iter()
        .enumerate()
        .take(range.end)
        .skip(range.start)
    {
        if message.role == Role::System {
            continue;
        }
        let mut content = Vec::new();
        let mut results = Vec::new();
        for (content_index, block) in message.content.iter().enumerate() {
            match block {
                Content::ToolUse { name, input, .. } => {
                    let call = by_call[&(message_index, content_index)];
                    content.push(Content::ToolUse {
                        id: call.wire_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                    let (result, is_error) = if let Some((index, block_index)) = call.result {
                        let Content::ToolResult {
                            content, is_error, ..
                        } = &snapshot.messages[index].content[block_index]
                        else {
                            unreachable!()
                        };
                        (content.clone(), *is_error)
                    } else {
                        ("Tool result is pending. No final execution result is recorded. This placeholder does not authorize execution or repetition; wait for the host to resume the owning goal.".into(), false)
                    };
                    results.push(Content::ToolResult {
                        tool_use_id: call.wire_id.clone(),
                        content: result,
                        is_error,
                    });
                }
                Content::ToolResult { .. } => {
                    let call = by_result[&(message_index, content_index)];
                    if message_index != call.message_index + 1 {
                        content.push(Content::Text { text: format!("The result for tool call {} became available at archive message {}, after the intervening conversation. Earlier intervening turns occurred while this result was pending; its result is paired with the original call in this model context.", call.wire_id, message_index - active_start) });
                    }
                }
                _ => content.push(block.clone()),
            }
        }
        if let Some(contexts) = snapshot.message_contexts.get(&message_index) {
            content.extend(contexts.iter().flat_map(render_input_context));
        }
        if let Some(reference) = denial_references.get(&message_index) {
            content.push(Content::Text {
                text: render_reference(reference),
            });
        }
        if !content.is_empty() {
            projected.push((
                Message {
                    role: message.role,
                    content,
                },
                Some(message_index),
            ));
        }
        if !results.is_empty() {
            projected.push((
                Message {
                    role: Role::User,
                    content: results,
                },
                None,
            ));
        }
    }
}

pub fn retain_public_content(messages: &mut Vec<Message>) {
    for message in messages.iter_mut() {
        message.content.retain(|content| {
            !matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            )
        });
    }
    messages.retain(|message| message.role != Role::System && !message.content.is_empty());
}

pub fn complete_boundaries(messages: &[Message]) -> Result<Vec<usize>, InvalidConversation> {
    let mut pending = HashSet::new();
    let mut boundaries = vec![0];
    for (index, message) in messages.iter().enumerate() {
        for content in &message.content {
            match content {
                Content::ToolUse { id, .. } => {
                    if !pending.insert(id.clone()) {
                        return Err(InvalidConversation(
                            "context has duplicate unresolved tool calls".into(),
                        ));
                    }
                }
                Content::ToolResult { tool_use_id, .. } if !pending.remove(tool_use_id) => {
                    return Err(InvalidConversation(
                        "context has an unpaired tool result".into(),
                    ));
                }
                _ => {}
            }
        }
        if pending.is_empty() {
            boundaries.push(index + 1);
        }
    }
    if !pending.is_empty() {
        return Err(InvalidConversation(
            "finish or resolve pending tool calls before compacting".into(),
        ));
    }
    Ok(boundaries)
}
