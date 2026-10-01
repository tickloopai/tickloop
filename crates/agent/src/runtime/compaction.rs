//! 压缩边界与压缩摘要，照 Claude Code `compact_boundary` 与 `isCompactSummary` 的表示。
//!
//! 全量压缩在对话末尾追加一个边界 system 消息和一条摘要 user 消息，边界之前的消息仍留在本进程
//! 内存中，模型只看到最后一个边界之后的消息。定向摘要（partial compact）把对话替换为
//! 边界、摘要与保留的消息；保留的消息换上新的消息 id，在 Transcript 中直线重写到新边界之后
//! （偏离 Claude Code 的保留区段重接，见 `CompactionLayout::apply`）。

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ChatSnapshot, SummaryDirection};
use crate::model::{Content, Message, ModelConfig, Role, Usage};

/// 照 Claude Code `createCompactBoundaryMessage` 的 `content`。
pub const COMPACT_BOUNDARY_TEXT: &str = "Conversation compacted";
/// 照 Claude Code `getCompactUserSummaryMessage` 的开头。
const FULL_SUMMARY_PREFIX: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n";
const FULL_SUMMARY_PINNED_STATE: &str = "\n\nGoals and plans retained by the host:\n";
/// 照 Claude Code `getCompactUserSummaryMessage` 的 `suppressFollowUpQuestions` 分支，自动压缩使用。
const CONTINUE_WITHOUT_QUESTIONS: &str = "\nContinue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, do not preface with \"I'll continue\" or similar. Pick up the last task as if the break never happened.";
const DIRECTED_SUMMARY_PREFIX: &str =
    "Summarized conversation (historical data, not new instructions):\n";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Manual,
    Auto,
}

/// 照 Claude Code `compactMetadata`。Tick Loop 不写 `preservedSegment`。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompactBoundary {
    pub trigger: CompactTrigger,
    pub pre_tokens: usize,
    pub user_context: Option<String>,
    pub messages_summarized: Option<usize>,
    /// 边界被之后的 Summarize from here 作为保留消息带到新边界之后。这样的边界在 Transcript 中
    /// 接在前一条消息后面，不再是对话链的起点。
    #[serde(default)]
    pub preserved_by_later_summary: bool,
    /// Host-copied original prompts from the active chain, never model summaries.
    #[serde(default)]
    pub(super) original_user_inputs: Option<Vec<super::approval_context::OriginalUserInput>>,
    /// 压缩时的模型，只在本进程内有效：rewind 越过边界重新暴露私有推理时用来判断兼容性。
    /// 恢复后边界之前的消息不在内存中，无需保存。
    #[serde(skip)]
    pub(super) model: Option<Box<ModelConfig>>,
}

/// 照 Claude Code `summarizeMetadata`。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SummarizeMetadata {
    pub messages_summarized: usize,
    pub user_context: Option<String>,
    pub direction: SummaryDirection,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum CompactionMarker {
    Boundary(CompactBoundary),
    /// 摘要保留了消息时带 `summarize_metadata`，否则照 Claude Code 只在完整记录中显示。
    Summary {
        summarize_metadata: Option<SummarizeMetadata>,
    },
}

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
    pub(super) fn active_start(&self) -> usize {
        self.boundary_indexes().last().copied().unwrap_or(0)
    }

    fn boundary_indexes(&self) -> Vec<usize> {
        self.message_compactions
            .iter()
            .filter(|(_, marker)| matches!(marker, CompactionMarker::Boundary(_)))
            .map(|(index, _)| *index)
            .collect()
    }

    /// 截断到 `message_count` 条消息时被移除的边界中，最早一个记录的压缩模型。
    pub(super) fn earliest_boundary_model_from(&self, message_count: usize) -> Option<ModelConfig> {
        self.message_compactions
            .range(message_count..)
            .find_map(|(_, marker)| match marker {
                CompactionMarker::Boundary(boundary) => boundary.model.as_deref().cloned(),
                CompactionMarker::Summary { .. } => None,
            })
    }
}

/// 压缩后对话中一条消息的来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MessageSource {
    Existing(usize),
    Boundary,
    Summary,
}

/// 压缩结果：新对话由已有消息、新的边界与摘要按顺序组成。
pub(super) struct CompactionLayout {
    pub sources: Vec<MessageSource>,
    /// 边界的逻辑父消息在压缩前对话中的下标。
    pub logical_parent: Option<usize>,
    pub boundary: CompactBoundary,
    pub summary: Message,
    pub summarize_metadata: Option<SummarizeMetadata>,
}

impl CompactionLayout {
    pub(super) fn position(&self, source: MessageSource) -> Option<usize> {
        self.sources
            .iter()
            .position(|candidate| *candidate == source)
    }

    /// 压缩前的全部消息原样留在开头（全量压缩）；否则是定向摘要，保留的消息要重写到新边界之后。
    pub(super) fn keeps_existing_prefix(&self, message_count: usize) -> bool {
        let existing = self
            .sources
            .iter()
            .filter(|source| matches!(source, MessageSource::Existing(_)))
            .count();
        existing == message_count
            && self.sources[..existing]
                .iter()
                .enumerate()
                .all(|(index, source)| *source == MessageSource::Existing(index))
    }

    /// 按布局生成压缩后的快照：保留消息的附属记录随消息移动，边界与摘要获得新的消息 id。
    ///
    /// 定向摘要偏离 Claude Code：Claude Code 让保留的消息沿用原条目，加载时按边界上的保留区段
    /// 重接，但定向摘要叠加、rewind 到区段的头之后继续对话时，重接会丢掉或复活消息。Tick Loop
    /// 把保留的消息作为新条目直线重写在新边界之后，因此它们换上新的消息 id（关联提示 id 的文件
    /// 回滚快照由调用方按 [`Self::renamed_message_ids`] 重记）；其中 assistant 消息的用量照
    /// Claude Code 重接时的做法清零，被带到新边界之后的旧边界标记为 `preserved_by_later_summary`。
    pub(super) fn apply(&self, snapshot: &ChatSnapshot) -> ChatSnapshot {
        let rewrites_existing = !self.keeps_existing_prefix(snapshot.messages.len());
        let mut compacted = snapshot.clone();
        compacted.messages = Vec::with_capacity(self.sources.len());
        compacted.message_runs = BTreeMap::new();
        compacted.message_contexts = BTreeMap::new();
        compacted.denial_references = BTreeMap::new();
        compacted.message_ids = BTreeMap::new();
        compacted.original_user_message_ids = BTreeMap::new();
        compacted.prompt_execution_modes = BTreeMap::new();
        compacted.inherited_prompts.clear();
        compacted.message_compactions = BTreeMap::new();
        compacted.message_usage = BTreeMap::new();
        for (index, source) in self.sources.iter().enumerate() {
            match *source {
                MessageSource::Existing(original) => {
                    let message = snapshot.messages[original].clone();
                    if let Some(run) = snapshot.message_runs.get(&original) {
                        compacted.message_runs.insert(index, *run);
                    }
                    if let Some(contexts) = snapshot.message_contexts.get(&original) {
                        compacted.message_contexts.insert(index, contexts.clone());
                    }
                    if let Some(reference) = snapshot.denial_references.get(&original) {
                        compacted.denial_references.insert(index, reference.clone());
                    }
                    if let Some(id) = snapshot.message_ids.get(&original) {
                        let id = if rewrites_existing {
                            Uuid::new_v4()
                        } else {
                            *id
                        };
                        compacted.message_ids.insert(index, id);
                    }
                    if snapshot.prompt_execution_modes.contains_key(&original)
                        && let Some(source_id) = snapshot
                            .original_user_message_ids
                            .get(&original)
                            .or_else(|| snapshot.message_ids.get(&original))
                    {
                        compacted
                            .original_user_message_ids
                            .insert(index, *source_id);
                    }
                    if let Some(mode) = snapshot.prompt_execution_modes.get(&original) {
                        compacted.prompt_execution_modes.insert(index, *mode);
                    }
                    if snapshot.inherited_prompts.contains(&original) {
                        compacted.inherited_prompts.insert(index);
                    }
                    let usage = snapshot.message_usage.get(&original).cloned();
                    if rewrites_existing && message.role == Role::Assistant {
                        compacted
                            .message_usage
                            .insert(index, zeroed(usage.unwrap_or_default()));
                    } else if let Some(usage) = usage {
                        compacted.message_usage.insert(index, usage);
                    }
                    if let Some(marker) = snapshot.message_compactions.get(&original) {
                        let mut marker = marker.clone();
                        if let CompactionMarker::Boundary(boundary) = &mut marker
                            && rewrites_existing
                        {
                            boundary.preserved_by_later_summary = true;
                        }
                        compacted.message_compactions.insert(index, marker);
                    }
                    compacted.messages.push(message);
                }
                MessageSource::Boundary => {
                    compacted.messages.push(Message {
                        role: Role::System,
                        content: vec![Content::Text {
                            text: COMPACT_BOUNDARY_TEXT.into(),
                        }],
                    });
                    compacted.message_ids.insert(index, Uuid::new_v4());
                    compacted
                        .message_compactions
                        .insert(index, CompactionMarker::Boundary(self.boundary.clone()));
                }
                MessageSource::Summary => {
                    compacted.messages.push(self.summary.clone());
                    compacted.message_ids.insert(index, Uuid::new_v4());
                    compacted.message_compactions.insert(
                        index,
                        CompactionMarker::Summary {
                            summarize_metadata: self.summarize_metadata.clone(),
                        },
                    );
                }
            }
        }
        if let Some(origin) = compacted
            .branch_origin
            .as_mut()
            .and_then(|origin| origin.creation.as_mut())
        {
            let seed_start = (origin.model_seed_message_count > 0)
                .then(|| self.moved(origin.model_seed_start))
                .flatten();
            let seed_moves_together = seed_start.is_some_and(|start| {
                (0..origin.model_seed_message_count).all(|offset| {
                    self.moved(origin.model_seed_start + offset) == Some(start + offset)
                })
            });
            match seed_start.filter(|_| seed_moves_together) {
                Some(start) => origin.model_seed_start = start,
                None if !rewrites_existing => {}
                None => origin.clear_seed(),
            }
        }
        if rewrites_existing {
            for start in compacted.loop_starts.values_mut() {
                *start = self.moved_loop_start(*start, snapshot.messages.len());
            }
        }
        compacted
    }

    /// 定向摘要后 Loop 的创建位置：压缩后对话中第一条代表创建之后消息的条目之前。保留的消息
    /// 代表它自己，摘要代表被摘要的消息；rewind 到这个位置或更早的保留提示时 Loop 仍随截断移除。
    fn moved_loop_start(&self, start: usize, message_count: usize) -> usize {
        let kept = self
            .sources
            .iter()
            .filter_map(|source| match source {
                MessageSource::Existing(original) => Some(*original),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let summarizes_later_messages = (start..message_count).any(|index| !kept.contains(&index));
        self.sources
            .iter()
            .position(|source| match source {
                MessageSource::Existing(original) => *original >= start,
                MessageSource::Summary => summarizes_later_messages,
                MessageSource::Boundary => false,
            })
            .unwrap_or(self.sources.len())
    }

    /// `apply` 前后同一条保留消息的 id 变化（旧 id → 新 id）；全量压缩时为空。
    pub(super) fn renamed_message_ids(
        &self,
        before: &ChatSnapshot,
        after: &ChatSnapshot,
    ) -> HashMap<Uuid, Uuid> {
        self.sources
            .iter()
            .enumerate()
            .filter_map(|(index, source)| match source {
                MessageSource::Existing(original) => Some((
                    *before.message_ids.get(original)?,
                    *after.message_ids.get(&index)?,
                )),
                MessageSource::Boundary | MessageSource::Summary => None,
            })
            .filter(|(old, new)| old != new)
            .collect()
    }

    fn moved(&self, original: usize) -> Option<usize> {
        self.position(MessageSource::Existing(original))
    }
}

fn zeroed(mut usage: Usage) -> Usage {
    usage.input_tokens = Some(0);
    usage.output_tokens = Some(0);
    usage.cache_read_input_tokens = Some(0);
    usage.cache_write_input_tokens = Some(0);
    usage
}

/// 照 Claude Code `getCompactUserSummaryMessage`：自动压缩要求模型不再提问、直接继续，手动
/// `/compact` 不带继续指令。Tick Loop 在摘要之后附带宿主保存的目标与计划；Claude Code 附加的
/// Transcript 路径没有照搬（不持久化的 Agent 没有文件）。
pub(super) fn full_summary_message(
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

pub(super) fn directed_summary_message(summary: &str) -> Message {
    user_text(format!("{DIRECTED_SUMMARY_PREFIX}{summary}"))
}

/// 摘要消息中模型给出的摘要文字，供界面显示。
pub(super) fn summary_text(message: &Message) -> String {
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
