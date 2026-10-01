use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use uuid::Uuid;

use super::entry::{
    ChainEntry, ContentReplacementEntry, FileHistorySnapshotEntry, Line, parse_timestamp_ms,
};
use super::writer::CachedMetadata;
use crate::model::{Content, Role, Usage};
use crate::runtime::approval_context::OriginalUserInput;

pub(crate) struct LoadedTranscript {
    /// 从根到叶子的当前对话链。
    pub chain: Vec<ChainEntry>,
    pub metadata: CachedMetadata,
    /// 照 Claude Code：按条目 `messageId` 保存的文件回滚快照，同一键以最后一条为准。
    pub file_history_snapshots: HashMap<Uuid, FileHistorySnapshotEntry>,
    /// 文件顺序中全部内容替换条目；调用方按 chatId 取用（照 Claude Code 按 sessionId 取用）。
    pub content_replacements: Vec<ContentReplacementEntry>,
    /// 加载时解析到的全部链条目 uuid，用作写入去重的种子。
    pub uuids: HashSet<Uuid>,
    /// 加载时需要让用户知道的问题，例如保留区段校验失败后加载了完整历史。
    pub warnings: Vec<String>,
}

struct IndexedEntry {
    file_index: usize,
    role: Role,
    entry: ChainEntry,
}

/// 照 Claude Code `SKIP_PRECOMPACT_THRESHOLD`：超过 5MB 的 Transcript 跳过压缩边界之前的内容。
pub(crate) const SKIP_PRECOMPACT_THRESHOLD: u64 = 5 * 1024 * 1024;
const COMPACT_BOUNDARY_MARKER: &[u8] = b"\"compact_boundary\"";
const SYSTEM_LINE_PREFIX: &[u8] = b"{\"type\":\"system\"";
/// 照 Claude Code `METADATA_TYPE_MARKERS`：边界之前仍要读取的 Chat 级元数据。Tick Loop 的 Chat 级
/// 元数据只有标题，另外保留 Branch 来源。
const PRE_BOUNDARY_METADATA_MARKERS: [&[u8]; 3] = [
    b"{\"type\":\"custom-title\"",
    b"{\"type\":\"branch-origin\"",
    b"{\"type\":\"delegation\"",
];

/// 照 Claude Code `loadTranscriptFile` 与 `buildConversationChain` 加载 Transcript：
/// 跳过无法解析的行（含截断的末行），重接部分压缩的保留区段，取时间最新的叶子沿父指针
/// 重建对话链。没有任何 user／assistant 条目时返回 `None`。
///
/// 文件超过 5MB 时，照 Claude Code `readTranscriptForLoad` 丢弃最后一个不带保留区段的压缩边界
/// 之前的内容，只从中读取 Chat 级元数据。与 Claude Code 不同，选出的叶子的祖先链走进被跳过的
/// 区域时（rewind 越过该边界后继续的分支）改为加载完整文件，不截断对话。
pub(crate) fn load(path: &Path) -> io::Result<Option<LoadedTranscript>> {
    let bytes = std::fs::read(path)?;
    let lines = split_lines(&bytes);
    if bytes.len() as u64 > SKIP_PRECOMPACT_THRESHOLD {
        let skip_before = last_unpreserved_boundary_offset(&lines);
        if skip_before > 0 {
            let (loaded, reaches_root) = load_lines(path, &lines, skip_before);
            if reaches_root && loaded.is_some() {
                return Ok(loaded);
            }
        }
    }
    Ok(load_lines(path, &lines, 0).0)
}

/// Recovers original prompts from the logical ancestry of an active compact boundary.
/// The ordinary large-file loader deliberately skips this ancestry for model context.
pub(crate) fn load_original_user_inputs(
    path: &Path,
    boundary_id: Uuid,
    chat_id: Uuid,
) -> io::Result<Option<Vec<OriginalUserInput>>> {
    let bytes = std::fs::read(path)?;
    let mut entries = HashMap::new();
    for (_, raw) in split_lines(&bytes) {
        let Ok(line) = serde_json::from_slice::<Line>(raw) else {
            continue;
        };
        if let Line::User(entry) | Line::Assistant(entry) | Line::System(entry) = line {
            entries.insert(entry.uuid, entry);
        }
    }
    let Some(boundary) = entries
        .get(&boundary_id)
        .filter(|entry| entry.chat_id == chat_id && entry.is_compact_boundary())
    else {
        return Ok(None);
    };
    let Some(logical_parent) = boundary.logical_parent_uuid else {
        return Ok(None);
    };
    let mut cursor = Some(logical_parent);
    let mut visited = HashSet::new();
    let mut originals = Vec::new();
    while let Some(uuid) = cursor {
        if !visited.insert(uuid) {
            return Ok(None);
        }
        let Some(entry) = entries.get(&uuid).filter(|entry| entry.chat_id == chat_id) else {
            return Ok(None);
        };
        if entry.message.role == Role::User
            && entry.execution_mode.is_some()
            && !entry.is_compact_summary
            && !entry.is_inherited_prompt
            && entry.agent_message_origin.is_none()
        {
            let (Some(loop_id), Some(run_id)) = (entry.loop_id, entry.run_id) else {
                return Ok(None);
            };
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
            if text.is_empty() {
                return Ok(None);
            }
            originals.push(OriginalUserInput {
                chat_id,
                loop_id,
                run_id,
                message_id: entry.original_user_message_id.unwrap_or(entry.uuid),
                text,
            });
        }
        cursor = if entry.is_compact_boundary() {
            let Some(logical_parent) = entry.logical_parent_uuid else {
                return Ok(None);
            };
            Some(logical_parent)
        } else {
            entry.parent_uuid
        };
    }
    originals.reverse();
    Ok(Some(originals))
}

/// 从 `skip_before` 偏移之后的行加载；之前的行只读取 Chat 级元数据。第二个返回值表示对话链
/// 沿父指针一直走到了根，没有停在缺失的父条目上。
fn load_lines(
    path: &Path,
    lines: &[(u64, &[u8])],
    skip_before: u64,
) -> (Option<LoadedTranscript>, bool) {
    let mut entries: HashMap<Uuid, IndexedEntry> = HashMap::new();
    let mut metadata = CachedMetadata::default();
    let mut file_history_snapshots = HashMap::new();
    let mut content_replacements = Vec::new();
    for (file_index, &(line_offset, raw)) in lines.iter().enumerate() {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if line_offset < skip_before
            && !PRE_BOUNDARY_METADATA_MARKERS
                .iter()
                .any(|marker| raw.starts_with(marker))
        {
            continue;
        }
        let Ok(line) = serde_json::from_slice::<Line>(raw) else {
            continue;
        };
        let role = match &line {
            Line::User(_) => Role::User,
            Line::Assistant(_) => Role::Assistant,
            Line::System(_) => Role::System,
            Line::CustomTitle(entry) => {
                metadata.custom_title = Some(entry.clone());
                continue;
            }
            Line::LastPrompt(entry) => {
                metadata.last_prompt = Some(entry.clone());
                continue;
            }
            Line::BranchOrigin(entry) => {
                metadata.branch_origin = Some(entry.clone());
                continue;
            }
            Line::Delegation(entry) => {
                metadata.delegation = Some(entry.clone());
                continue;
            }
            Line::FileHistorySnapshot(entry) => {
                file_history_snapshots.insert(entry.message_id, entry.clone());
                continue;
            }
            Line::ContentReplacement(entry) => {
                content_replacements.push(entry.clone());
                continue;
            }
            Line::Unknown => continue,
        };
        let (Line::User(entry) | Line::Assistant(entry) | Line::System(entry)) = line else {
            unreachable!();
        };
        entries.insert(
            entry.uuid,
            IndexedEntry {
                file_index,
                role,
                entry,
            },
        );
    }
    let uuids = entries.keys().copied().collect::<HashSet<_>>();
    let mut warnings = Vec::new();
    let prune = apply_preserved_segment_relinks(&mut entries).unwrap_or_else(|warning| {
        warnings.push(format!("{} in {}", warning, path.display()));
        None
    });
    // 有后续对话条目的条目。Loop 条目不算后续：它们挂在消息之后，不改变哪条消息是叶子。
    let mut parents = HashSet::new();
    for indexed in entries
        .values()
        .filter(|indexed| !indexed.entry.is_loop_record())
    {
        let mut cursor = indexed.entry.parent_uuid;
        while let Some(parent) = cursor {
            if !parents.insert(parent) {
                break;
            }
            cursor = entries
                .get(&parent)
                .filter(|parent| parent.entry.is_loop_record())
                .and_then(|parent| parent.entry.parent_uuid);
        }
    }
    let leaf = entries
        .values()
        .filter(|indexed| indexed.role != Role::System && !parents.contains(&indexed.entry.uuid))
        .max_by(|left, right| {
            let left_time = parse_timestamp_ms(&left.entry.timestamp).unwrap_or(i64::MIN);
            let right_time = parse_timestamp_ms(&right.entry.timestamp).unwrap_or(i64::MIN);
            left_time
                .cmp(&right_time)
                .then_with(|| right.file_index.cmp(&left.file_index))
        });
    let Some(leaf) = leaf else {
        return (None, true);
    };
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut cursor = Some(leaf.entry.uuid);
    let mut reaches_root = true;
    while let Some(uuid) = cursor {
        if !visited.insert(uuid) {
            break;
        }
        let Some(indexed) = entries.get(&uuid) else {
            reaches_root = false;
            break;
        };
        chain.push(indexed.entry.clone());
        cursor = indexed.entry.parent_uuid;
    }
    chain.reverse();
    append_trailing_loop_records(&mut chain, &entries, &parents, &mut visited);
    // 只有叶子的对话链经过最后一个边界时才删除它之前的内容；rewind 越过该边界后继续的分支
    // 仍需要边界之前的祖先。
    if let Some(prune) = prune
        && chain.iter().any(|entry| entry.uuid == prune.last_boundary)
    {
        entries.retain(|uuid, indexed| {
            indexed.file_index >= prune.last_boundary_index || prune.preserved.contains(uuid)
        });
    }
    recover_orphaned_parallel_tool_results(&mut chain, &entries);
    let loaded = LoadedTranscript {
        uuids,
        chain,
        metadata,
        file_history_snapshots,
        content_replacements,
        warnings,
    };
    (Some(loaded), reaches_root)
}

/// 叶子之后写入的 Loop 条目（例如一轮回复结束后的状态变化）不在父指针链上，从叶子向下接回：
/// 每一步取没有后续对话条目、时间最新的 Loop 子条目。
fn append_trailing_loop_records(
    chain: &mut Vec<ChainEntry>,
    entries: &HashMap<Uuid, IndexedEntry>,
    parents: &HashSet<Uuid>,
    visited: &mut HashSet<Uuid>,
) {
    let mut trailing_children: HashMap<Uuid, Vec<&IndexedEntry>> = HashMap::new();
    for indexed in entries
        .values()
        .filter(|indexed| indexed.entry.is_loop_record() && !parents.contains(&indexed.entry.uuid))
    {
        if let Some(parent) = indexed.entry.parent_uuid {
            trailing_children.entry(parent).or_default().push(indexed);
        }
    }
    let mut tip = chain.last().map(|entry| entry.uuid);
    while let Some(next) = tip
        .and_then(|uuid| trailing_children.get(&uuid))
        .and_then(|children| {
            children.iter().max_by_key(|child| {
                (
                    parse_timestamp_ms(&child.entry.timestamp).unwrap_or(i64::MIN),
                    child.file_index,
                )
            })
        })
        .filter(|next| visited.insert(next.entry.uuid))
    {
        chain.push(next.entry.clone());
        tip = Some(next.entry.uuid);
    }
}

/// 每一行的文件偏移与内容（不含换行符）。
fn split_lines(bytes: &[u8]) -> Vec<(u64, &[u8])> {
    let mut offset = 0u64;
    bytes
        .split(|byte| *byte == b'\n')
        .map(|raw| {
            let line = (offset, raw);
            offset += raw.len() as u64 + 1;
            line
        })
        .collect()
}

/// 照 Claude Code `scanChunkLines`：最后一个不带保留区段的压缩边界所在行的偏移；带保留区段的
/// 边界不截断（保留的消息在它之前）。被定向摘要重写到新边界之后的旧边界有父条目，不是对话链
/// 的起点，同样不截断（Claude Code 写出的边界父指针总为空）。没有这样的边界时返回 0。
fn last_unpreserved_boundary_offset(lines: &[(u64, &[u8])]) -> u64 {
    lines
        .iter()
        .rev()
        .filter(|(_, raw)| {
            raw.starts_with(SYSTEM_LINE_PREFIX)
                && raw
                    .windows(COMPACT_BOUNDARY_MARKER.len())
                    .any(|window| window == COMPACT_BOUNDARY_MARKER)
        })
        .find_map(|(offset, raw)| match serde_json::from_slice::<Line>(raw) {
            Ok(Line::System(entry))
                if entry.is_compact_boundary()
                    && entry.parent_uuid.is_none()
                    && entry
                        .compact_metadata
                        .as_ref()
                        .is_none_or(|metadata| metadata.preserved_segment.is_none()) =>
            {
                Some(*offset)
            }
            _ => None,
        })
        .unwrap_or(0)
}

/// 带保留区段的文件在重接之后，按最后一个边界删除之前内容的依据。
struct PrecompactPrune {
    last_boundary: Uuid,
    last_boundary_index: usize,
    preserved: HashSet<Uuid>,
}

/// 照 Claude Code `applyPreservedSegmentRelinks` 读取带保留区段的文件（Tick Loop 自己不再写区段）：
/// 只采用文件中最后一个带保留区段的边界。先从尾到头校验区段链，再把头接到锚点、把锚点的其他
/// 子节点接到尾，清零保留 assistant 消息的用量。返回删除最后一个边界之前、不在保留区段中的
/// 条目所需的信息，由调用方在选出叶子后决定是否删除。区段校验失败时不做任何修改，返回说明，
/// 调用方加载完整历史。
fn apply_preserved_segment_relinks(
    entries: &mut HashMap<Uuid, IndexedEntry>,
) -> Result<Option<PrecompactPrune>, String> {
    let boundaries = entries
        .values()
        .filter(|indexed| indexed.entry.is_compact_boundary())
        .map(|indexed| {
            (
                indexed.file_index,
                indexed.entry.uuid,
                indexed
                    .entry
                    .compact_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.preserved_segment),
            )
        })
        .collect::<Vec<_>>();
    let Some((segment_index, segment)) = boundaries
        .iter()
        .filter_map(|(index, _, segment)| segment.map(|segment| (*index, segment)))
        .max_by_key(|(index, _)| *index)
    else {
        return Ok(None);
    };
    let (last_boundary_index, last_boundary) = boundaries
        .iter()
        .map(|(index, uuid, _)| (*index, *uuid))
        .max_by_key(|(index, _)| *index)
        .unwrap_or((segment_index, segment.head_uuid));
    // 区段之后又有不带区段的边界时，区段已经过时：不重接，仍按最后一个边界删除之前的内容。
    let segment_is_live = segment_index == last_boundary_index;
    let mut preserved = HashSet::new();
    if segment_is_live {
        let mut cursor = entries.get(&segment.tail_uuid);
        let mut reached_head = false;
        while let Some(indexed) = cursor.filter(|indexed| preserved.insert(indexed.entry.uuid)) {
            if indexed.entry.uuid == segment.head_uuid {
                reached_head = true;
                break;
            }
            cursor = indexed
                .entry
                .parent_uuid
                .and_then(|parent| entries.get(&parent));
        }
        if !reached_head {
            return Err(
                "compact boundary preserved segment is incomplete; loaded the full history without relinking".into(),
            );
        }
        if let Some(head) = entries.get_mut(&segment.head_uuid) {
            head.entry.parent_uuid = Some(segment.anchor_uuid);
        }
        for indexed in entries.values_mut() {
            if indexed.entry.parent_uuid == Some(segment.anchor_uuid)
                && indexed.entry.uuid != segment.head_uuid
            {
                indexed.entry.parent_uuid = Some(segment.tail_uuid);
            }
        }
        for uuid in &preserved {
            if let Some(indexed) = entries
                .get_mut(uuid)
                .filter(|indexed| indexed.role == Role::Assistant)
            {
                let usage = indexed.entry.usage.get_or_insert_with(Usage::default);
                usage.input_tokens = Some(0);
                usage.output_tokens = Some(0);
                usage.cache_read_input_tokens = Some(0);
                usage.cache_write_input_tokens = Some(0);
            }
        }
    }
    Ok(Some(PrecompactPrune {
        last_boundary,
        last_boundary_index,
        preserved,
    }))
}

/// 照 Claude Code `recoverOrphanedParallelToolResults`：并行工具结果若写成了兄弟分支，
/// 沿父链重建时只会带上其中一支，这里把其余结果补回同一条结果消息。
/// Tick Loop 目前的写入拓扑是线性的，正常不会触发。
fn recover_orphaned_parallel_tool_results(
    chain: &mut Vec<ChainEntry>,
    entries: &HashMap<Uuid, IndexedEntry>,
) {
    let on_chain = chain.iter().map(|entry| entry.uuid).collect::<HashSet<_>>();
    let mut resolved = chain
        .iter()
        .flat_map(|entry| entry.message.content.iter())
        .filter_map(|content| match content {
            Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut position = 0;
    while position < chain.len() {
        let assistant = &chain[position];
        let unresolved = assistant
            .message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolUse { id, .. } if !resolved.contains(id) => Some(id.clone()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        if assistant.message.role != Role::Assistant || unresolved.is_empty() {
            position += 1;
            continue;
        }
        let mut group_end = position;
        while chain.get(group_end + 1).is_some_and(|entry| {
            is_tool_results(entry) && (group_end == position || entry.continues_message)
        }) {
            group_end += 1;
        }
        if group_end == position {
            position += 1;
            continue;
        }
        let group = chain[position..=group_end]
            .iter()
            .map(|entry| entry.uuid)
            .collect::<HashSet<_>>();
        let mut orphans = entries
            .values()
            .filter(|indexed| {
                indexed.role == Role::User
                    && !on_chain.contains(&indexed.entry.uuid)
                    && indexed
                        .entry
                        .parent_uuid
                        .is_some_and(|parent| group.contains(&parent))
                    && is_tool_results(&indexed.entry)
                    && indexed.entry.message.content.iter().any(|content| {
                        matches!(content, Content::ToolResult { tool_use_id, .. } if unresolved.contains(tool_use_id))
                    })
            })
            .map(|indexed| {
                let mut entry = indexed.entry.clone();
                entry.continues_message = true;
                (parse_timestamp_ms(&entry.timestamp), indexed.file_index, entry)
            })
            .collect::<Vec<_>>();
        orphans.sort_by_key(|(timestamp, file_index, _)| (*timestamp, *file_index));
        for (_, _, entry) in &orphans {
            resolved.extend(
                entry
                    .message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    }),
            );
        }
        let inserted = orphans.len();
        chain.splice(
            group_end + 1..group_end + 1,
            orphans.into_iter().map(|(_, _, entry)| entry),
        );
        position = group_end + inserted + 1;
    }
}

fn is_tool_results(entry: &ChainEntry) -> bool {
    entry.message.role == Role::User
        && !entry.message.content.is_empty()
        && entry
            .message
            .content
            .iter()
            .all(|content| matches!(content, Content::ToolResult { .. }))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;
    use crate::model::Message;
    use crate::transcript::entry::{
        COMPACT_BOUNDARY_SUBTYPE, CompactMetadata, CompactTriggerEntry, PreservedSegmentEntry,
        format_timestamp,
    };

    fn entry(
        uuid: Uuid,
        parent: Option<Uuid>,
        milliseconds: u64,
        continues_message: bool,
        message: Message,
    ) -> ChainEntry {
        ChainEntry {
            uuid,
            parent_uuid: parent,
            logical_parent_uuid: None,
            is_sidechain: false,
            chat_id: Uuid::nil(),
            timestamp: format_timestamp(milliseconds),
            cwd: PathBuf::from("/tmp"),
            version: "test".into(),
            git_branch: None,
            message,
            invoked_skills: Vec::new(),
            is_meta: false,
            execution_mode: None,
            is_inherited_prompt: false,
            original_user_message_id: None,
            agent_message_origin: None,
            run_id: None,
            contexts: None,
            continues_message,
            subtype: None,
            compact_metadata: None,
            is_compact_summary: false,
            summarize_metadata: None,
            is_visible_in_transcript_only: false,
            usage: None,
            model: None,
            private_source: None,
            loop_id: None,
            slug: None,
            loop_record: None,
        }
    }

    fn text(role: Role, value: &str) -> Message {
        Message {
            role,
            content: vec![Content::Text { text: value.into() }],
        }
    }

    fn write(path: &Path, lines: &[Line]) {
        let mut file = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{}", serde_json::to_string(line).unwrap()).unwrap();
        }
    }

    #[test]
    fn the_latest_leaf_wins_and_other_branches_stay_out_of_the_chain() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        let [root, older, newer] = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        write(
            &path,
            &[
                Line::User(entry(root, None, 1_000, false, text(Role::User, "root"))),
                Line::Assistant(entry(
                    newer,
                    Some(root),
                    3_000,
                    false,
                    text(Role::Assistant, "new"),
                )),
                Line::Assistant(entry(
                    older,
                    Some(root),
                    2_000,
                    false,
                    text(Role::Assistant, "old"),
                )),
            ],
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\":\"user\",\"uuid\":").unwrap();

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(
            loaded
                .chain
                .iter()
                .map(|entry| entry.uuid)
                .collect::<Vec<_>>(),
            vec![root, newer]
        );
        assert_eq!(loaded.uuids.len(), 3);
    }

    #[test]
    fn file_history_snapshots_keep_the_last_entry_for_each_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        let [prompt, assistant] = [Uuid::new_v4(), Uuid::new_v4()];
        let snapshot = |message_id: Uuid, marker: &str, is_snapshot_update: bool| {
            Line::FileHistorySnapshot(FileHistorySnapshotEntry {
                message_id,
                snapshot: serde_json::json!({ "marker": marker }),
                is_snapshot_update,
            })
        };
        write(
            &path,
            &[
                snapshot(prompt, "made", false),
                Line::User(entry(prompt, None, 1_000, false, text(Role::User, "go"))),
                snapshot(assistant, "first edit", true),
                snapshot(assistant, "second edit", true),
                Line::Assistant(entry(
                    assistant,
                    Some(prompt),
                    2_000,
                    false,
                    text(Role::Assistant, "done"),
                )),
            ],
        );

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.file_history_snapshots.len(), 2);
        assert_eq!(
            loaded.file_history_snapshots[&prompt].snapshot["marker"],
            "made"
        );
        let update = &loaded.file_history_snapshots[&assistant];
        assert!(update.is_snapshot_update);
        assert_eq!(update.snapshot["marker"], "second edit");
    }

    fn boundary(uuid: Uuid, milliseconds: u64, segment: Option<PreservedSegmentEntry>) -> Line {
        let mut entry = entry(
            uuid,
            None,
            milliseconds,
            false,
            text(Role::System, "Conversation compacted"),
        );
        entry.subtype = Some(COMPACT_BOUNDARY_SUBTYPE.into());
        entry.compact_metadata = Some(CompactMetadata {
            trigger: CompactTriggerEntry::Manual,
            pre_tokens: 0,
            user_context: None,
            messages_summarized: Some(2),
            preserved_segment: segment,
            original_user_inputs: None,
        });
        Line::System(entry)
    }

    #[test]
    fn preserved_segments_relink_after_the_last_boundary_or_load_the_full_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        let [
            prompt,
            answer,
            kept_prompt,
            kept_answer,
            compacted,
            summary,
            next,
        ] = std::array::from_fn(|_| Uuid::new_v4());
        let mut lines = vec![
            Line::User(entry(prompt, None, 1_000, false, text(Role::User, "p"))),
            Line::Assistant(entry(
                answer,
                Some(prompt),
                2_000,
                false,
                text(Role::Assistant, "a"),
            )),
            Line::User(entry(
                kept_prompt,
                Some(answer),
                3_000,
                false,
                text(Role::User, "kp"),
            )),
            Line::Assistant(entry(
                kept_answer,
                Some(kept_prompt),
                4_000,
                false,
                text(Role::Assistant, "ka"),
            )),
            boundary(
                compacted,
                5_000,
                Some(PreservedSegmentEntry {
                    head_uuid: kept_prompt,
                    anchor_uuid: Uuid::nil(),
                    tail_uuid: kept_answer,
                }),
            ),
            Line::User(entry(
                summary,
                Some(compacted),
                5_000,
                false,
                text(Role::User, "s"),
            )),
            Line::User(entry(
                next,
                Some(summary),
                6_000,
                false,
                text(Role::User, "n"),
            )),
        ];
        if let Line::System(entry) = &mut lines[4] {
            entry
                .compact_metadata
                .as_mut()
                .unwrap()
                .preserved_segment
                .as_mut()
                .unwrap()
                .anchor_uuid = summary;
        }
        write(&path, &lines);
        let loaded = load(&path).unwrap().unwrap();
        let chain = loaded
            .chain
            .iter()
            .map(|entry| entry.uuid)
            .collect::<Vec<_>>();
        assert_eq!(chain, [compacted, summary, kept_prompt, kept_answer, next]);
        assert_eq!(
            loaded.chain[3]
                .usage
                .as_ref()
                .and_then(|usage| usage.input_tokens),
            Some(0)
        );
        assert!(loaded.warnings.is_empty());

        // 区段的尾不在文件中：不重接、不删除，并给出可见的说明。
        if let Line::System(entry) = &mut lines[4] {
            entry
                .compact_metadata
                .as_mut()
                .unwrap()
                .preserved_segment
                .as_mut()
                .unwrap()
                .tail_uuid = Uuid::new_v4();
        }
        write(&path, &lines);
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(
            loaded
                .chain
                .iter()
                .map(|entry| entry.uuid)
                .collect::<Vec<_>>(),
            [compacted, summary, next]
        );
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("preserved segment"));
    }

    #[test]
    fn a_leaf_that_branches_before_the_last_boundary_keeps_its_ancestors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        let [prompt, answer, kept, compacted, summary, rewound] =
            std::array::from_fn(|_| Uuid::new_v4());
        let lines = vec![
            Line::User(entry(prompt, None, 1_000, false, text(Role::User, "p"))),
            Line::Assistant(entry(
                answer,
                Some(prompt),
                2_000,
                false,
                text(Role::Assistant, "a"),
            )),
            Line::User(entry(
                kept,
                Some(answer),
                3_000,
                false,
                text(Role::User, "k"),
            )),
            boundary(
                compacted,
                4_000,
                Some(PreservedSegmentEntry {
                    head_uuid: kept,
                    anchor_uuid: summary,
                    tail_uuid: kept,
                }),
            ),
            Line::User(entry(
                summary,
                Some(compacted),
                4_000,
                false,
                text(Role::User, "s"),
            )),
            // rewind 越过边界后接在边界之前的消息上继续。
            Line::User(entry(
                rewound,
                Some(answer),
                5_000,
                false,
                text(Role::User, "r"),
            )),
        ];
        write(&path, &lines);
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(
            loaded
                .chain
                .iter()
                .map(|entry| entry.uuid)
                .collect::<Vec<_>>(),
            [prompt, answer, rewound]
        );
    }

    #[test]
    fn a_large_transcript_loads_in_full_when_the_leaf_reaches_before_the_skipped_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        let [prompt, compacted, summary, rewound] = std::array::from_fn(|_| Uuid::new_v4());
        let large = "x".repeat(SKIP_PRECOMPACT_THRESHOLD as usize);
        write(
            &path,
            &[
                Line::User(entry(prompt, None, 1_000, false, text(Role::User, &large))),
                boundary(compacted, 2_000, None),
                Line::User(entry(
                    summary,
                    Some(compacted),
                    2_000,
                    false,
                    text(Role::User, "s"),
                )),
                Line::User(entry(
                    rewound,
                    Some(prompt),
                    3_000,
                    false,
                    text(Role::User, "r"),
                )),
            ],
        );
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(
            loaded
                .chain
                .iter()
                .map(|entry| entry.uuid)
                .collect::<Vec<_>>(),
            [prompt, rewound]
        );
    }

    #[test]
    fn orphaned_parallel_tool_results_rejoin_their_result_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        let [prompt, assistant, first, second] = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        let calls = Message {
            role: Role::Assistant,
            content: ["a", "b"]
                .map(|id| Content::ToolUse {
                    id: id.into(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                })
                .to_vec(),
        };
        let result = |id: &str| Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: id.into(),
                content: id.into(),
                is_error: false,
            }],
        };
        write(
            &path,
            &[
                Line::User(entry(prompt, None, 1_000, false, text(Role::User, "go"))),
                Line::Assistant(entry(assistant, Some(prompt), 2_000, false, calls)),
                Line::User(entry(first, Some(assistant), 3_000, false, result("a"))),
                Line::User(entry(second, Some(assistant), 2_500, false, result("b"))),
            ],
        );

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(
            loaded
                .chain
                .iter()
                .map(|entry| entry.uuid)
                .collect::<Vec<_>>(),
            vec![prompt, assistant, first, second]
        );
        assert!(loaded.chain[3].continues_message);
    }
}
