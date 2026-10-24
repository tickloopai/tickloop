use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tickloop_agent_protocol::{
    ChatSnapshot, CompactionMarker, Content, HostNote, Message, OriginalUserInput, Role,
};
use tickloop_agent_transcript::ChainEntry;
use uuid::Uuid;

use crate::ChatBookkeeping;

const REPLY_HISTORY_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyHistory {
    pub version: u32,
    pub complete: bool,
    pub contexts: Vec<ReplyContext>,
}

impl Default for ReplyHistory {
    fn default() -> Self {
        Self {
            version: REPLY_HISTORY_VERSION,
            complete: true,
            contexts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyContext {
    pub chat_id: Uuid,
    pub user_message_id: Uuid,
    pub preceding: Vec<ReplyTurn>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyTurn {
    pub message_id: Uuid,
    pub source: ReplySource,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReplySource {
    Assistant,
    Agent,
    Host,
}

struct ObservedMessage<'a> {
    id: Uuid,
    user_id: Uuid,
    loop_id: Option<Uuid>,
    run_id: Option<Uuid>,
    is_human: bool,
    is_agent: bool,
    is_summary: bool,
    /// Skill or MCP server guidance the host wrote for the model, which no one said in the turn.
    is_guidance: bool,
    is_proposed_batch: bool,
    message: &'a Message,
}

pub(crate) fn capture(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    proposed: Option<(Uuid, &str)>,
    originals: &[OriginalUserInput],
    ancestors: Option<&[ChainEntry]>,
) -> ReplyHistory {
    let mut history = ReplyHistory::default();
    if let Some(delegation) = &snapshot.delegation {
        let inherited = delegation
            .trusted_reply_history
            .as_ref()
            .and_then(|value| serde_json::from_value::<ReplyHistory>(value.clone()).ok())
            .filter(|history| history.version == REPLY_HISTORY_VERSION);
        match inherited {
            Some(inherited) => {
                history.complete = inherited.complete;
                history
                    .contexts
                    .extend(inherited.contexts.into_iter().filter(|context| {
                        delegation
                            .lineage()
                            .iter()
                            .any(|parent| context.chat_id == parent.chat_id)
                            && originals.iter().any(|input| {
                                input.chat_id == context.chat_id
                                    && delegation.lineage().iter().any(|parent| {
                                        input.chat_id == parent.chat_id
                                            && input.loop_id == parent.loop_id
                                    })
                                    && input.message_id == context.user_message_id
                            })
                    }));
            }
            None => history.complete = false,
        }
    }
    let boundary = snapshot.message_compactions.iter().rev().find_map(|(index, marker)| {
        matches!(marker, CompactionMarker::Boundary(boundary) if !boundary.preserved_by_later_summary).then_some(*index)
    });
    if !snapshot.message_compactions.is_empty() && ancestors.is_none() {
        history.complete = false;
    }
    let proposed_position = super::proposed_call_position(snapshot, proposed);
    let recovered = ancestors
        .into_iter()
        .flatten()
        .map(|entry| ObservedMessage {
            id: entry.original_message_id.unwrap_or(entry.uuid),
            user_id: entry.original_message_id.unwrap_or(entry.uuid),
            loop_id: entry.loop_id,
            run_id: entry.run_id,
            is_human: entry.execution_mode.is_some()
                && !entry.is_inherited_prompt
                && entry.agent_message_origin.is_none(),
            is_agent: entry.agent_message_origin.is_some(),
            is_summary: entry.is_compact_summary,
            is_guidance: entry.guidance.is_some(),
            is_proposed_batch: false,
            message: &entry.message,
        });
    let visible = snapshot
        .messages
        .iter()
        .enumerate()
        .skip(if ancestors.is_some() {
            boundary.map_or(0, |index| index + 1)
        } else {
            0
        })
        .filter_map(|(index, message)| {
            let recorded_id = *snapshot.message_ids.get(&index)?;
            let id = snapshot
                .original_message_ids
                .get(&index)
                .copied()
                .unwrap_or(recorded_id);
            let is_agent = snapshot.agent_message_origins.contains_key(&id)
                || snapshot.agent_message_origins.contains_key(&recorded_id);
            let run_id = snapshot.message_runs.get(&index).copied();
            Some(ObservedMessage {
                id,
                user_id: snapshot
                    .original_message_ids
                    .get(&index)
                    .copied()
                    .unwrap_or(id),
                loop_id: (run_id.and_then(|run| bookkeeping.run_loops.get(&run).copied()))
                    .map(Into::into),
                run_id: run_id.map(Into::into),
                is_human: snapshot.prompt_execution_modes.contains_key(&index)
                    && !snapshot.inherited_prompts.contains(&index)
                    && !is_agent,
                is_agent,
                is_summary: matches!(
                    snapshot.message_compactions.get(&index),
                    Some(CompactionMarker::Summary { .. })
                ),
                is_guidance: matches!(snapshot.host_notes.get(&index), Some(HostNote::Guidance(_))),
                is_proposed_batch: proposed_position.is_some_and(|(proposed, _)| proposed == index),
                message,
            })
        });
    let mut pending: HashMap<Uuid, Vec<ReplyTurn>> = HashMap::new();
    let mut questions = HashMap::new();
    let mut observed = HashSet::new();
    let mut anchored = HashSet::new();
    for entry in recovered.chain(visible) {
        if entry.is_summary
            || entry.is_guidance
            || !matches!(entry.message.role, Role::Assistant | Role::User)
            || !observed.insert(entry.id)
        {
            continue;
        }
        let Some(loop_id) = entry.loop_id else {
            history.complete = false;
            continue;
        };
        let before = pending.entry(loop_id).or_default();
        if entry.message.role == Role::Assistant {
            for block in &entry.message.content {
                if let Content::ToolUse { id, name, .. } = block
                    && name == tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME
                {
                    questions.insert((entry.run_id, id.clone()), entry.id);
                }
            }
        }
        if entry.message.role == Role::User && entry.is_human {
            if originals.iter().any(|input| {
                input.chat_id == snapshot.id
                    && input.loop_id == loop_id
                    && input.message_id == entry.user_id
            }) {
                history.contexts.push(ReplyContext {
                    chat_id: snapshot.id.into(),
                    user_message_id: entry.user_id,
                    preceding: std::mem::take(before),
                });
                anchored.insert(entry.user_id);
            } else {
                before.clear();
            }
            continue;
        }
        if entry.message.role == Role::User {
            for block in &entry.message.content {
                if let Content::ToolResult {
                    tool_use_id,
                    is_error: false,
                    ..
                } = block
                    && let Some(user_id) = questions.remove(&(entry.run_id, tool_use_id.clone()))
                    && originals
                        .iter()
                        .any(|input| input.chat_id == snapshot.id && input.message_id == user_id)
                {
                    before.clear();
                    if anchored.insert(user_id) {
                        history.contexts.push(ReplyContext {
                            chat_id: snapshot.id.into(),
                            user_message_id: user_id,
                            preceding: Vec::new(),
                        });
                    }
                }
            }
        }
        if entry.is_proposed_batch {
            continue;
        }
        let text = entry
            .message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            before.push(ReplyTurn {
                message_id: entry.id,
                source: if entry.message.role == Role::Assistant {
                    ReplySource::Assistant
                } else if entry.is_agent {
                    ReplySource::Agent
                } else {
                    ReplySource::Host
                },
                text,
            });
        }
    }
    history.complete &= originals.iter().all(|input| {
        history.contexts.iter().any(|context| {
            context.chat_id == input.chat_id && context.user_message_id == input.message_id
        })
    });
    history
}
