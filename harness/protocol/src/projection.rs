//! How a Chat's archive reads as a conversation: what the model sees after the last compact
//! boundary, the public items a View shows, and the user prompts a rewind can return to.

use std::collections::{BTreeMap, HashMap, HashSet};

use uuid::Uuid;

use crate::{
    ActiveConversationItem, BranchPublicItem, BranchPublicSource, ChatSnapshot, CompactTrigger,
    CompactionMarker, Content, DenialReference, Event, GuidanceNote, HostNote, LocalMessage,
    Message, Protocol, RewindPoint, Role, RunId, render_input_context, render_reference,
};

/// The Skill, MCP and execution environment guidance the model can still see.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VisibleGuidance {
    pub skills: BTreeMap<String, (String, bool)>,
    pub mcp_servers: BTreeMap<String, String>,
    /// The digest of the MCP configuration warnings the model was last told.
    pub mcp_warnings: Option<String>,
    pub environment: Option<String>,
}

/// A conversation whose tool calls and results cannot be paired into one model history.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidConversation(pub String);

/// Follows the opening of Claude Code's `getCompactUserSummaryMessage`.
const FULL_SUMMARY_PREFIX: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n";

const FULL_SUMMARY_PINNED_STATE: &str = "\n\nGoals and plans retained by the host:\n";

/// Follows the `suppressFollowUpQuestions` branch of Claude Code's `getCompactUserSummaryMessage`;
/// used by automatic compaction.
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

    /// The summary text produced by the model in a compact summary message; `None` when this is not
    /// a compact summary.
    pub fn compact_summary_text(&self, message_index: usize) -> Option<String> {
        self.is_compact_summary(message_index)
            .then(|| summary_text(&self.messages[message_index]))
    }

    /// The summary text of the last compact summary.
    pub fn latest_compact_summary(&self) -> Option<String> {
        self.message_compactions
            .iter()
            .rev()
            .find_map(|(index, _)| self.compact_summary_text(*index))
    }

    /// Following Claude Code's `getMessagesAfterCompactBoundary`: the model-visible conversation
    /// starts at the last boundary.
    pub fn active_start(&self) -> usize {
        self.boundary_indexes().last().copied().unwrap_or(0)
    }

    /// The host tool names the latest host tool note the model can still see announced.
    pub fn visible_announced_host_tools(&self) -> Option<std::collections::BTreeSet<String>> {
        self.host_notes
            .range(self.active_start()..)
            .rev()
            .find_map(|(_, note)| note.announced_host_tools().cloned())
    }

    /// The guidance the model can still see: each Skill's latest note, as its digest and whether
    /// it was truncated, and each MCP server's latest instructions digest, unless a later note
    /// withdrew it.
    pub fn visible_guidance(&self) -> VisibleGuidance {
        let mut visible = VisibleGuidance::default();
        for (index, note) in self.host_notes.range(self.active_start()..) {
            match note {
                HostNote::Guidance(GuidanceNote::Skill {
                    name,
                    digest,
                    truncated,
                }) => {
                    visible
                        .skills
                        .insert(name.clone(), (digest.clone(), *truncated));
                }
                HostNote::Guidance(GuidanceNote::McpInstructions { server, digest }) => {
                    visible.mcp_servers.insert(server.clone(), digest.clone());
                }
                HostNote::Guidance(GuidanceNote::McpServers { servers }) => {
                    visible.mcp_servers.extend(servers.clone());
                }
                HostNote::Guidance(GuidanceNote::McpWarnings { digest }) => {
                    visible.mcp_warnings = Some(digest.clone());
                }
                HostNote::Guidance(GuidanceNote::Environment { digest }) => {
                    // A boundary retains the last delivery state, not its model-visible text.
                    if !self.is_compact_boundary(*index) {
                        visible.environment = digest.clone();
                    }
                }
                HostNote::Guidance(GuidanceNote::Withdrawn {
                    skills,
                    mcp_servers,
                }) => {
                    visible.skills.retain(|name, _| !skills.contains(name));
                    visible
                        .mcp_servers
                        .retain(|server, _| !mcp_servers.contains(server));
                }
                HostNote::Guidance(GuidanceNote::Later(_))
                | HostNote::HostTools(_)
                | HostNote::PlanModeExit
                | HostNote::ExecutionMode(_)
                | HostNote::LoopState
                | HostNote::TaskReminder
                | HostNote::DateChange(_)
                | HostNote::Hook(_) => {}
            }
        }
        visible
    }

    fn boundary_indexes(&self) -> Vec<usize> {
        self.message_compactions
            .iter()
            .filter(|(_, marker)| matches!(marker, CompactionMarker::Boundary(_)))
            .map(|(index, _)| *index)
            .collect()
    }

    /// The model-visible conversation (after the last compact boundary), with compact summaries as
    /// separate items.
    pub fn active_conversation_projection(
        &self,
    ) -> Result<Vec<ActiveConversationItem>, InvalidConversation> {
        self.conversation_projection_from(self.active_start())
    }

    /// The conversation from `start`, with compact summaries as separate items; tool call pairing
    /// is checked only for the model-visible part.
    fn conversation_projection_from(
        &self,
        start: usize,
    ) -> Result<Vec<ActiveConversationItem>, InvalidConversation> {
        history_calls_in(self, self.active_start()..self.messages.len())?;
        Ok((start..self.messages.len())
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

    /// User prompts in the current conversation that can be rewound to, in conversation order.
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
        self.public_items_from(self.active_start())
    }

    /// The whole conversation as a person sees it: compaction only trims the model context,
    /// messages before the boundary that are still in memory are shown as usual, and each
    /// compaction leaves a summary item in place. Follows Claude Code's fullscreen mode, which
    /// keeps pre-compaction messages.
    pub fn full_public_conversation_items(
        &self,
    ) -> Result<Vec<BranchPublicItem>, InvalidConversation> {
        self.public_items_from(0)
    }

    fn public_items_from(
        &self,
        start: usize,
    ) -> Result<Vec<BranchPublicItem>, InvalidConversation> {
        let active = self.conversation_projection_from(start)?;
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

/// Following Claude Code's `getCompactUserSummaryMessage`: automatic compaction tells the model not
/// to ask further questions and to continue directly, while a manual `/compact` carries no continue
/// instruction. Tickloop appends the Goal and plan saved by the host after the summary; the
/// Transcript path Claude Code appends is not copied (an Agent that does not persist has no file).
pub fn full_summary_message(
    summary: &str,
    pinned_state: &serde_json::Value,
    trigger: CompactTrigger,
) -> Message {
    let continuation = match trigger {
        CompactTrigger::Auto => CONTINUE_WITHOUT_QUESTIONS,
        CompactTrigger::Manual | CompactTrigger::Later(_) => "",
    };
    user_text(format!(
        "{FULL_SUMMARY_PREFIX}{}{FULL_SUMMARY_PINNED_STATE}{pinned_state}{continuation}",
        summary.trim()
    ))
}

pub fn directed_summary_message(summary: &str) -> Message {
    user_text(format!("{DIRECTED_SUMMARY_PREFIX}{summary}"))
}

/// The summary text produced by the model in a summary message, for display.
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

/// Pairs tool calls with results within the given message range. The model projection looks only at
/// messages after the last compact boundary, so call ids from before compaction do not affect the
/// projection, and the projection is unchanged after a restart that loads only the content after
/// the boundary.
pub fn history_calls_in(
    snapshot: &ChatSnapshot,
    range: std::ops::Range<usize>,
) -> Result<Vec<HistoryCall>, InvalidConversation> {
    let mut calls: Vec<HistoryCall> = Vec::new();
    let mut pending: HashMap<String, Vec<usize>> = HashMap::new();
    let mut seen = HashSet::new();
    // A model request shows the result of a later version's call as text, as it shows the call.
    let mut later_calls = HashSet::new();
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
                Content::Later(_) => {
                    later_calls.extend(content.later_call_id().map(str::to_owned));
                }
                Content::ToolResult { tool_use_id, .. } if later_calls.contains(tool_use_id) => {}
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
                .map(|run| run.uuid().simple().to_string())
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

/// The conversation sent to the model, plus the index of the original message each message was
/// projected from (synthesized tool results have no index). Following Claude Code's
/// `getMessagesAfterCompactBoundary`, only messages after the last compact boundary are projected.
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

/// Projects the messages within the given range after the last compact boundary, dropping private
/// reasoning and empty messages. Directional summaries use it to build the summary request.
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
                        ("Tool result is pending. No final execution result is recorded. This placeholder does not authorize execution or repetition; wait for the host to resume the owning Loop.".into(), false)
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
                // A prompt that opens with an attachment keeps no text of its own. Providers
                // such as Anthropic reject an empty text block, so its contexts stand alone.
                Content::Text { text }
                    if text.is_empty()
                        && snapshot
                            .message_contexts
                            .get(&message_index)
                            .is_some_and(|contexts| !contexts.is_empty()) => {}
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
                Content::ToolUse { id, .. } if !pending.insert(id.clone()) => {
                    return Err(InvalidConversation(
                        "context has duplicate unresolved tool calls".into(),
                    ));
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
