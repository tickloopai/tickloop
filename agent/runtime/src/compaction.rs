//! 压缩边界与压缩摘要，照 Claude Code `compact_boundary` 与 `isCompactSummary` 的表示。
//!
//! 全量压缩在对话末尾追加一个边界 system 消息和一条摘要 user 消息，边界之前的消息仍留在本进程
//! 内存中，模型只看到最后一个边界之后的消息。定向摘要（partial compact）把对话替换为
//! 边界、摘要与保留的消息；保留的消息换上新的消息 id，在 Transcript 中直线重写到新边界之后
//! （偏离 Claude Code 的保留区段重接，见 `CompactionLayout::apply`）。

use std::collections::{BTreeMap, HashMap};

use uuid::Uuid;

use super::ChatBookkeeping;
use tickloop_agent_protocol::{
    COMPACT_BOUNDARY_TEXT, ChatSnapshot, CompactBoundary, CompactionMarker, Content, Message, Role,
    SummarizeMetadata, Usage,
};

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
    pub(super) fn apply(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
    ) -> (ChatSnapshot, ChatBookkeeping) {
        let rewrites_existing = !self.keeps_existing_prefix(snapshot.messages.len());
        let mut compacted = snapshot.clone();
        let mut compacted_bookkeeping = bookkeeping.clone();
        compacted.messages = Vec::with_capacity(self.sources.len());
        compacted.message_runs = BTreeMap::new();
        compacted.message_contexts = BTreeMap::new();
        compacted_bookkeeping.denial_references = BTreeMap::new();
        compacted.message_ids = BTreeMap::new();
        compacted.original_message_ids = BTreeMap::new();
        compacted.prompt_execution_modes = BTreeMap::new();
        compacted.inherited_prompts.clear();
        compacted.host_notes = BTreeMap::new();
        compacted.message_compactions = BTreeMap::new();
        compacted.message_usage = BTreeMap::new();
        compacted.message_sources = BTreeMap::new();
        compacted.message_models = BTreeMap::new();
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
                    if let Some(reference) = bookkeeping.denial_references.get(&original) {
                        compacted_bookkeeping
                            .denial_references
                            .insert(index, reference.clone());
                    }
                    if let Some(original_id) = snapshot.message_ids.get(&original) {
                        let id = if rewrites_existing {
                            Uuid::new_v4()
                        } else {
                            *original_id
                        };
                        compacted.message_ids.insert(index, id);
                        if let Some(origin) = snapshot.agent_message_origins.get(original_id) {
                            compacted.agent_message_origins.insert(id, origin.clone());
                        }
                        if let Some(origin) = snapshot.human_family_origins.get(original_id) {
                            compacted.human_family_origins.insert(id, origin.clone());
                        }
                    }
                    if let Some(source_id) = snapshot
                        .original_message_ids
                        .get(&original)
                        .or_else(|| snapshot.message_ids.get(&original))
                    {
                        compacted.original_message_ids.insert(index, *source_id);
                    }
                    if let Some(mode) = snapshot.prompt_execution_modes.get(&original) {
                        compacted.prompt_execution_modes.insert(index, *mode);
                    }
                    if snapshot.inherited_prompts.contains(&original) {
                        compacted.inherited_prompts.insert(index);
                    }
                    if let Some(announced) = snapshot.host_notes.get(&original) {
                        compacted.host_notes.insert(index, announced.clone());
                    }
                    if let Some(source) = snapshot.message_sources.get(&original) {
                        compacted.message_sources.insert(index, source.clone());
                    }
                    if let Some(model) = snapshot.message_models.get(&original) {
                        compacted.message_models.insert(index, model.clone());
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
        compacted.announced_host_tools = compacted.visible_announced_host_tools();
        if rewrites_existing {
            for start in compacted_bookkeeping.loop_starts.values_mut() {
                *start = self.moved_loop_start(*start, snapshot.messages.len());
            }
        }
        (compacted, compacted_bookkeeping)
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
