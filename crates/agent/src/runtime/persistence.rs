//! 运行时与 Transcript 的适配层。
//!
//! `Agent::save` 只把 Chat 标脏；后台冲刷任务最多每 100ms 取一次已提交的快照，
//! 与已写出的内容做差分，追加新的链上条目。Chat 快照不整体写入磁盘：恢复时按 id 加载
//! Transcript，由对话链与元数据重建 Chat，并执行 Claude Code 的恢复后处理。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::denials::{ApprovalRecord, DenialRecord, DenialWindow, MAX_RECENT_DENIALS};
use super::{
    Agent, AgentError, BranchOrigin, Chat, ChatId, ChatSnapshot, InputContext, LocalMessage,
    LoopId, LoopSnapshot, LoopState, RunId, SummaryDirection, TodoItem, Trigger,
    compaction::{
        CompactBoundary, CompactTrigger, CompactionLayout, CompactionMarker, MessageSource,
        SummarizeMetadata,
    },
};
use crate::file_history::{FileHistory, FileHistorySnapshot};
use crate::model::{Content, Message, PrivateHistorySource, Role, Usage};
use crate::permissions::ExecutionMode;
use crate::private_fs::unix_time_milliseconds;
use crate::transcript::{
    BranchOriginEntry, COMPACT_BOUNDARY_SUBTYPE, CachedMetadata, ChainEntry, CompactMetadata,
    CompactTriggerEntry, ContentReplacementEntry, ContentReplacementRecord, CustomTitleEntry,
    FLUSH_INTERVAL, FileHistorySnapshotEntry, ForkedFrom, LOOP_CREATED_SUBTYPE, LOOP_ENDED_SUBTYPE,
    LOOP_STATE_SUBTYPE, LOOP_TRIGGER_SUBTYPE, LastPromptEntry, Line, LoadedTranscript, LoopRecord,
    PartialCompactDirection, SummarizeMetadataEntry, TranscriptWriter, format_timestamp,
    list_project, load, locate, project_directory, transcript_path,
};

/// 照 Claude Code：恢复时末尾是工具结果，追加这条 meta user 消息表示从中断处继续。
const CONTINUE_FROM_INTERRUPTION: &str = "Continue from where you left off.";
/// 照 Claude Code `NO_RESPONSE_REQUESTED`：末尾是 user 消息时追加的 assistant 哨兵。
const NO_RESPONSE_REQUESTED: &str = "No response requested.";
/// 从 Transcript 恢复的 Branch 没有标题时使用的名称。
const UNTITLED_BRANCH_NAME: &str = "Branch";

/// 一个 Transcript 在项目目录中的摘要，供 Chat 选择器使用。
#[derive(Clone, Debug)]
pub struct SavedChat {
    pub id: ChatId,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub modified: SystemTime,
    pub cwd: Option<PathBuf>,
    pub branch_source: Option<ChatId>,
    pub git_branch: Option<String>,
}

pub(super) struct ChatRecorder {
    root: PathBuf,
    writer: Arc<TranscriptWriter>,
    logs: StdMutex<HashMap<ChatId, ChatLog>>,
    dirty: StdMutex<HashSet<ChatId>>,
    notify: Arc<Notify>,
    flush_loop_started: AtomicBool,
    closed: CancellationToken,
    resume_lock: tokio::sync::Mutex<()>,
    denial_flush_gate: Arc<StdMutex<()>>,
    denial_saved_revisions: Arc<StdMutex<HashMap<ChatId, u64>>>,
}

struct ChatLog {
    /// 创建或恢复时固定；之后改变工作目录也不移动文件。
    path: PathBuf,
    /// 与 `snapshot.messages` 一一对应的已写出条目。
    links: Vec<MessageLink>,
    meta_messages: HashSet<usize>,
    /// 下一条新消息的父条目。通常是最后写出的条目；压缩后照 Claude Code 是摘要消息，
    /// 保留的消息不重写，不移动这个游标。
    parent_cursor: Option<Uuid>,
    /// 加载时发现、需要让用户知道的问题。
    load_warnings: Vec<String>,
    /// 本进程为该文件写出的最后一批链上条目的时间。之后的批次时间严格递增，rewind 后接续的
    /// 条目即使与被放弃分叉写于同一毫秒，也会成为时间更新的叶子。
    last_entry_milliseconds: u64,
    /// 当前对话链上各 Loop 最后记录的完整状态；快照中的 Loop 与之不同时追加 Loop 条目。
    recorded_loops: HashMap<LoopId, LoopEntryContent>,
    /// 在本文件中写过创建条目或从文件加载的 Loop，用于区分创建与之后的状态变化。
    created_loops: HashSet<LoopId>,
}

const DENIAL_FILE_VERSION: u32 = 1;
const APPROVAL_FILE_VERSION: u32 = 2;
const MAX_DENIAL_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(serde::Deserialize, serde::Serialize)]
struct SavedDenials {
    version: u32,
    chat_id: ChatId,
    records: Vec<DenialRecord>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct SavedApprovals {
    version: u32,
    chat_id: ChatId,
    records: Vec<ApprovalRecord>,
}

type DenialWrite = (ChatId, PathBuf, u64, Vec<ApprovalRecord>);
type DenialWriteResult = (ChatId, u64, Result<(), String>);

fn write_denial_windows(
    gate: &StdMutex<()>,
    saved_revisions: &StdMutex<HashMap<ChatId, u64>>,
    writes: Vec<DenialWrite>,
) -> Vec<DenialWriteResult> {
    let _gate = gate.lock().unwrap();
    writes
        .into_iter()
        .map(|(chat_id, path, revision, records)| {
            if saved_revisions
                .lock()
                .unwrap()
                .get(&chat_id)
                .is_some_and(|saved| *saved >= revision)
            {
                return (chat_id, revision, Ok(()));
            }
            let result = serde_json::to_vec(&SavedApprovals {
                version: APPROVAL_FILE_VERSION,
                chat_id,
                records,
            })
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                let directory = path
                    .parent()
                    .ok_or_else(|| "denial path has no parent".to_owned())?;
                crate::private_fs::create_private_directory(directory)
                    .map_err(|error| error.to_string())?;
                crate::private_fs::replace_file_atomically(
                    &path,
                    &bytes,
                    crate::private_fs::PRIVATE_FILE_MODE,
                )
                .map_err(|error| error.to_string())
            });
            if result.is_ok() {
                saved_revisions.lock().unwrap().insert(chat_id, revision);
            }
            (chat_id, revision, result)
        })
        .collect()
}

/// Loop 条目记录的内容：完整状态与 run id。
type LoopEntryContent = (LoopRecord, Option<RunId>);

#[derive(Clone)]
struct MessageLink {
    uuids: Vec<Uuid>,
    blocks: usize,
}

impl ChatRecorder {
    pub(super) fn path_for_chat(&self, chat_id: ChatId) -> Option<PathBuf> {
        Some(self.logs.lock().unwrap().get(&chat_id)?.path.clone())
    }

    fn denial_path_for_chat(&self, chat_id: ChatId) -> Option<PathBuf> {
        Some(self.path_for_chat(chat_id)?.with_extension("denials.json"))
    }

    fn load_denials(&self, path: &Path, chat_id: ChatId) -> DenialWindow {
        let path = path.with_extension("denials.json");
        let contents = match crate::private_fs::open_without_following_links(
            &path,
            crate::private_fs::OpenTarget::Entry,
        )
        .and_then(|file| {
            let mut contents = Vec::new();
            file.take(MAX_DENIAL_FILE_BYTES + 1)
                .read_to_end(&mut contents)?;
            if contents.len() as u64 > MAX_DENIAL_FILE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "recent denial history is too large",
                ));
            }
            Ok(contents)
        }) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return DenialWindow::default();
            }
            Err(error) => {
                return DenialWindow {
                    load_warning: Some(format!("Could not load recent denials: {error}")),
                    ..Default::default()
                };
            }
        };
        let parsed = serde_json::from_slice::<SavedApprovals>(&contents);
        let records = match parsed {
            Ok(saved)
                if saved.version == APPROVAL_FILE_VERSION
                    && saved.chat_id == chat_id
                    && saved.records.len() <= MAX_RECENT_DENIALS
                    && saved.records.iter().all(|record| record.chat_id == chat_id) =>
            {
                Some(saved.records)
            }
            _ => serde_json::from_slice::<SavedDenials>(&contents)
                .ok()
                .filter(|saved| {
                    saved.version == DENIAL_FILE_VERSION
                        && saved.chat_id == chat_id
                        && saved.records.len() <= MAX_RECENT_DENIALS
                        && saved.records.iter().all(|record| record.chat_id == chat_id)
                })
                .map(|saved| {
                    saved
                        .records
                        .into_iter()
                        .map(ApprovalRecord::from_denial)
                        .collect()
                }),
        };
        match records {
            Some(records) => DenialWindow {
                records: records.into(),
                ..Default::default()
            },
            None => DenialWindow {
                load_warning: Some(
                    "Recent denial history is damaged; this Chat was restored without it".into(),
                ),
                ..Default::default()
            },
        }
    }

    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            root,
            writer: Arc::default(),
            logs: StdMutex::default(),
            dirty: StdMutex::default(),
            notify: Arc::new(Notify::new()),
            flush_loop_started: AtomicBool::new(false),
            closed: CancellationToken::new(),
            resume_lock: tokio::sync::Mutex::new(()),
            denial_flush_gate: Arc::new(StdMutex::new(())),
            denial_saved_revisions: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub(super) fn register_chat(&self, chat_id: ChatId, workdir: &Path) {
        let path = transcript_path(&project_directory(&self.root, workdir), chat_id);
        self.register_path(chat_id, path);
    }

    fn register_path(&self, chat_id: ChatId, path: PathBuf) {
        self.logs.lock().unwrap().insert(
            chat_id,
            ChatLog {
                path,
                links: Vec::new(),
                meta_messages: HashSet::new(),
                parent_cursor: None,
                load_warnings: Vec::new(),
                last_entry_milliseconds: 0,
                recorded_loops: HashMap::new(),
                created_loops: HashSet::new(),
            },
        );
    }

    /// 以新 chatId 恢复的 Chat（`--fork-session`）写入工作目录所属项目中的新文件：已加载的消息作为新条目重写，
    /// 标题照 Claude Code 随元数据带到新文件，Branch 来源以新 chatId 重新记录。
    fn register_resumed_copy(
        &self,
        chat_id: ChatId,
        workdir: &Path,
        meta_messages: HashSet<usize>,
        metadata: CachedMetadata,
        load_warnings: Vec<String>,
    ) {
        let path = transcript_path(&project_directory(&self.root, workdir), chat_id);
        self.logs.lock().unwrap().insert(
            chat_id,
            ChatLog {
                path: path.clone(),
                links: Vec::new(),
                meta_messages,
                parent_cursor: None,
                load_warnings,
                last_entry_milliseconds: 0,
                recorded_loops: HashMap::new(),
                created_loops: HashSet::new(),
            },
        );
        let custom_title = metadata.custom_title.map(|entry| {
            Line::CustomTitle(CustomTitleEntry {
                chat_id,
                custom_title: entry.custom_title,
            })
        });
        let branch_origin = metadata.branch_origin.map(|entry| {
            Line::BranchOrigin(BranchOriginEntry {
                chat_id,
                forked_from: entry.forked_from,
            })
        });
        self.writer.enqueue(
            &path,
            custom_title.into_iter().chain(branch_origin).collect(),
        );
    }

    /// 把 Branch 的来源与标题排入子 Chat 的 Transcript；子 Chat 与来源放在同一项目目录。
    pub(super) fn register_branch(
        &self,
        source: &mut ChatSnapshot,
        child_chat_id: ChatId,
        title: &str,
    ) {
        self.enqueue_sync(source);
        let (path, leaf) = {
            let logs = self.logs.lock().unwrap();
            let Some(log) = logs.get(&source.id) else {
                return;
            };
            let directory = log.path.parent().unwrap_or(&self.root).to_path_buf();
            (
                transcript_path(&directory, child_chat_id),
                log.links.last().and_then(|link| link.uuids.last().copied()),
            )
        };
        self.register_path(child_chat_id, path.clone());
        self.writer.enqueue(
            &path,
            vec![
                Line::BranchOrigin(BranchOriginEntry {
                    chat_id: child_chat_id,
                    forked_from: ForkedFrom {
                        chat_id: source.id,
                        message_uuid: leaf,
                    },
                }),
                Line::CustomTitle(CustomTitleEntry {
                    chat_id: child_chat_id,
                    custom_title: title.to_owned(),
                }),
            ],
        );
    }

    /// 照 Claude Code `recordContentReplacement`：替换决定作为元数据条目追加到 Chat 的 Transcript。
    fn enqueue_content_replacements(
        &self,
        chat_id: ChatId,
        replacements: Vec<ContentReplacementRecord>,
    ) {
        let Some(path) = self
            .logs
            .lock()
            .unwrap()
            .get(&chat_id)
            .map(|log| log.path.clone())
        else {
            return;
        };
        self.writer.enqueue(
            &path,
            vec![Line::ContentReplacement(ContentReplacementEntry {
                chat_id,
                replacements,
            })],
        );
    }

    /// rewind 截断对话前调用：先写出截断前尚未写出的消息，被放弃的消息照 Claude Code 留在
    /// 文件中；之后的条目接在最后保留的消息后面，在同一文件中形成分叉。
    pub(super) fn truncate(&self, snapshot: &mut ChatSnapshot, message_count: usize) {
        self.enqueue_sync(snapshot);
        let chat_id = snapshot.id;
        if let Some(log) = self.logs.lock().unwrap().get_mut(&chat_id) {
            log.links.truncate(message_count);
            log.meta_messages.retain(|index| *index < message_count);
            log.parent_cursor = log.links.last().and_then(|link| link.uuids.last().copied());
            // 截断点之后的 Loop 条目不在新的对话链上；下一次同步为截断后仍存在的 Loop 重记完整状态。
            log.recorded_loops.clear();
        }
        self.mark_dirty(chat_id);
    }

    /// 压缩与定向摘要提交时、替换内存快照之前调用。先写出压缩前尚未写出的消息，再追加新的
    /// 边界（父指针为空，逻辑父指针指向压缩前最后一条）与摘要，并为未结束的 Loop 重记状态；
    /// 这些条目作为同一批入队，保证大文件跳过边界之前的内容时 Loop、计划与进度不丢失。
    ///
    /// 全量压缩照 Claude Code `recordTranscript`：保留的消息沿用原条目，下一条消息接在摘要之后。
    /// 定向摘要偏离 Claude Code，不写保留区段：边界之后按内存顺序把对话中的消息作为新条目直线
    /// 重写（uuid 取 `compacted` 中已换新的消息 id），`renamed_snapshots` 中随提示换了 id 的文件
    /// 回滚快照以新 id 重新写入；被摘要的旧条目留在文件中成为被放弃的分支，下一条消息接在内存
    /// 对话的最后一条之后。
    ///
    /// 边界之后对话中仍存在的工具结果的内容替换决定同批重新记录：超过 5MB 的 Transcript 加载时
    /// 跳过边界之前的记录，否则保留消息中被替换的结果会以原文发送。
    pub(super) fn compact(
        &self,
        snapshot: &mut ChatSnapshot,
        compacted: &mut ChatSnapshot,
        layout: &CompactionLayout,
        renamed_snapshots: &[FileHistorySnapshot],
        content_replacements: &super::tool_results::ContentReplacementState,
    ) {
        let Some((path, mut lines)) = self.sync_chat(snapshot) else {
            return;
        };
        let compaction_lines = self.compaction_lines(snapshot, compacted, layout);
        let Some(compaction_lines) = compaction_lines else {
            self.writer.enqueue(&path, lines);
            return;
        };
        lines.extend(compaction_lines);
        lines.extend(renamed_snapshots.iter().filter_map(|renamed| {
            Some(Line::FileHistorySnapshot(FileHistorySnapshotEntry {
                message_id: renamed.message_id,
                snapshot: serde_json::to_value(renamed).ok()?,
                is_snapshot_update: false,
            }))
        }));
        let replacements = content_replacements.records_for(compacted);
        if !replacements.is_empty() {
            lines.push(Line::ContentReplacement(ContentReplacementEntry {
                chat_id: compacted.id,
                replacements,
            }));
        }
        self.writer.enqueue(&path, lines);
    }

    /// 压缩后对话中需要新写的链上条目，同时把已写出条目的对应表换成压缩后的对话。已写出的条目
    /// 与压缩前的对话对不上时返回 `None`，不做任何修改。
    fn compaction_lines(
        &self,
        snapshot: &ChatSnapshot,
        compacted: &ChatSnapshot,
        layout: &CompactionLayout,
    ) -> Option<Vec<Line>> {
        let mut logs = self.logs.lock().unwrap();
        let log = logs
            .get_mut(&snapshot.id)
            .filter(|log| log.links.len() == snapshot.messages.len())?;
        let keeps_existing = layout.keeps_existing_prefix(snapshot.messages.len());
        let old_links = std::mem::take(&mut log.links);
        let milliseconds = unix_time_milliseconds().max(log.last_entry_milliseconds + 1);
        let timestamp = format_timestamp(milliseconds);
        let git_branch = current_git_branch(&compacted.workdir);
        let mut lines = Vec::new();
        let mut parent = None;
        // 压缩边界让对话链断开，边界之前的 Loop 条目不再在链上：为每个未结束的 Loop 重记完整状态。
        // 全量压缩写在摘要之后；定向摘要按 Loop 在压缩后对话中的创建位置写在对应消息之前。
        let first_loop_position = if keeps_existing {
            layout.sources.len()
        } else {
            layout
                .position(MessageSource::Boundary)
                .map_or(0, |index| index + 1)
        };
        let mut loop_updates = compacted
            .loops
            .iter()
            .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
            .map(|goal| {
                let start = compacted.loop_starts.get(&goal.id).copied().unwrap_or(0);
                (
                    start.max(first_loop_position),
                    loop_entry_content(compacted, goal),
                )
            })
            .collect::<Vec<_>>();
        loop_updates.sort_by_key(|(position, _)| *position);
        let mut loop_updates = VecDeque::from(loop_updates);
        log.recorded_loops.clear();
        for (index, source) in layout.sources.iter().enumerate() {
            if let MessageSource::Existing(original) = source
                && keeps_existing
            {
                let link = old_links[*original].clone();
                parent = link.uuids.last().copied();
                log.links.push(link);
                continue;
            }
            while loop_updates
                .front()
                .is_some_and(|(position, _)| *position <= index)
            {
                let (_, content) = loop_updates.pop_front().unwrap();
                lines.push(record_loop(
                    log,
                    compacted,
                    content,
                    &mut parent,
                    &timestamp,
                ));
            }
            let uuid = compacted
                .message_ids
                .get(&index)
                .copied()
                .unwrap_or_else(Uuid::new_v4);
            let message = compacted.messages[index].clone();
            let role = message.role;
            let blocks = message.content.len();
            let mut entry = chain_entry(compacted, index, uuid, message, parent, &timestamp);
            if *source == MessageSource::Boundary {
                entry.logical_parent_uuid = layout
                    .logical_parent
                    .and_then(|original| old_links.get(original))
                    .and_then(|link| link.uuids.last().copied());
            }
            entry.git_branch = git_branch.clone();
            entry.is_meta = matches!(source, MessageSource::Existing(original)
                if log.meta_messages.contains(original));
            parent = Some(uuid);
            log.links.push(MessageLink {
                uuids: vec![uuid],
                blocks,
            });
            lines.push(Line::chain(role, entry));
        }
        for (_, content) in loop_updates {
            lines.push(record_loop(
                log,
                compacted,
                content,
                &mut parent,
                &timestamp,
            ));
        }
        // 边界之前的计划与进度工具调用不再在链上：在全部消息之后为这些 Loop 记下当时的计划版本与进度。
        for goal in compacted
            .loops
            .iter()
            .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
        {
            if let Some(content) = plan_entry_content(compacted, goal) {
                lines.push(record_loop(
                    log,
                    compacted,
                    content,
                    &mut parent,
                    &timestamp,
                ));
            }
        }
        for goal in &compacted.loops {
            log.recorded_loops
                .entry(goal.id)
                .or_insert_with(|| loop_entry_content(compacted, goal));
        }
        log.meta_messages = layout
            .sources
            .iter()
            .enumerate()
            .filter_map(|(index, source)| match source {
                MessageSource::Existing(original) if log.meta_messages.contains(original) => {
                    Some(index)
                }
                _ => None,
            })
            .collect();
        log.parent_cursor = parent;
        log.last_entry_milliseconds = milliseconds;
        Some(lines)
    }

    pub(super) fn file_history_root(&self) -> PathBuf {
        self.root.join(crate::file_history::FILE_HISTORY_DIRECTORY)
    }

    pub(super) fn plans_directory(&self) -> PathBuf {
        super::plan::plans_directory(&self.root)
    }

    /// 照 Claude Code `recordFileHistorySnapshot`：快照作为元数据条目追加到 Chat 的 Transcript。
    pub(super) fn record_file_history_snapshot(
        &self,
        chat_id: ChatId,
        message_id: Uuid,
        snapshot: &FileHistorySnapshot,
        is_snapshot_update: bool,
    ) {
        let Some(path) = self
            .logs
            .lock()
            .unwrap()
            .get(&chat_id)
            .map(|log| log.path.clone())
        else {
            return;
        };
        let Ok(snapshot) = serde_json::to_value(snapshot) else {
            return;
        };
        self.writer.enqueue(
            &path,
            vec![Line::FileHistorySnapshot(FileHistorySnapshotEntry {
                message_id,
                snapshot,
                is_snapshot_update,
            })],
        );
    }

    pub(super) fn project_directory_of(&self, chat_id: ChatId) -> Option<PathBuf> {
        self.logs
            .lock()
            .unwrap()
            .get(&chat_id)
            .and_then(|log| log.path.parent().map(Path::to_path_buf))
    }

    fn mark_dirty(&self, chat_id: ChatId) {
        self.dirty.lock().unwrap().insert(chat_id);
        self.notify.notify_one();
    }

    fn take_dirty(&self) -> HashSet<ChatId> {
        std::mem::take(&mut *self.dirty.lock().unwrap())
    }

    fn enqueue_sync(&self, snapshot: &ChatSnapshot) {
        if let Some((path, lines)) = self.sync_chat(snapshot) {
            self.writer.enqueue(&path, lines);
        }
    }

    /// 差分依据：运行时对消息只有三种修改：追加新消息；向最后一条消息追加内容块（并行工具
    /// 结果），写成标记接续该消息的新条目；rewind 截断对话，由 `truncate` 事先同步到已写出的条目。
    fn sync_chat(&self, snapshot: &ChatSnapshot) -> Option<(PathBuf, Vec<Line>)> {
        let mut logs = self.logs.lock().unwrap();
        let log = logs.get_mut(&snapshot.id)?;
        if snapshot.messages.len() < log.links.len() {
            return None;
        }
        let milliseconds = unix_time_milliseconds().max(log.last_entry_milliseconds + 1);
        let timestamp = format_timestamp(milliseconds);
        let mut git_branch = None;
        let mut lines = Vec::new();
        let mut parent = log.parent_cursor;
        let first_new_message = log.links.len();
        // 与链上记录不同的 Loop。尚未记录的 Loop 写在它创建时所在的消息之前（已写出的消息之前
        // 无法插入时写在本批开头）；已记录的 Loop 的变化写在本批末尾。条目位置只会晚于实际发生
        // 的位置：rewind 截断后会为仍存在的 Loop 重记状态，所以不会让 Loop 在新链上错误地消失。
        let mut loop_updates = snapshot
            .loops
            .iter()
            .filter_map(|goal| {
                let content = loop_entry_content(snapshot, goal);
                if log.recorded_loops.get(&goal.id) == Some(&content) {
                    return None;
                }
                let position = if log.recorded_loops.contains_key(&goal.id) {
                    snapshot.messages.len()
                } else {
                    snapshot
                        .loop_starts
                        .get(&goal.id)
                        .copied()
                        .unwrap_or(0)
                        .clamp(first_new_message, snapshot.messages.len())
                };
                Some((position, content))
            })
            .collect::<Vec<_>>();
        // 首次写入当前链段的 Loop：链上缺少它之前的计划与进度工具调用（Branch、`--fork-session`、rewind 截断），
        // 在本批末尾记下当时的计划版本与进度。
        let first_recorded_loops = loop_updates
            .iter()
            .map(|(_, (record, _))| record.loop_id)
            .filter(|loop_id| !log.recorded_loops.contains_key(loop_id))
            .collect::<Vec<_>>();
        loop_updates.sort_by_key(|(position, _)| *position);
        let mut loop_updates = VecDeque::from(loop_updates);
        if let Some(index) = log.links.len().checked_sub(1) {
            let message = &snapshot.messages[index];
            let blocks = log.links[index].blocks;
            if message.content.len() > blocks {
                let appended = Message {
                    role: message.role,
                    content: message.content[blocks..].to_vec(),
                };
                // 接在最后写出的条目之后：消息之后可能已经有 Loop 条目。
                let mut entry = chain_entry(
                    snapshot,
                    index,
                    Uuid::new_v4(),
                    appended,
                    parent.or(log.links[index].uuids.last().copied()),
                    &timestamp,
                );
                entry.subtype = None;
                entry.compact_metadata = None;
                entry.is_compact_summary = false;
                entry.summarize_metadata = None;
                entry.is_visible_in_transcript_only = false;
                entry.usage = None;
                entry.git_branch = git_branch
                    .get_or_insert_with(|| current_git_branch(&snapshot.workdir))
                    .clone();
                entry.contexts = None;
                entry.execution_mode = None;
                entry.continues_message = true;
                entry.is_meta = log.meta_messages.contains(&index);
                parent = Some(entry.uuid);
                let link = &mut log.links[index];
                link.uuids.push(entry.uuid);
                link.blocks = message.content.len();
                lines.push(Line::chain(message.role, entry));
            }
        }
        for index in first_new_message..snapshot.messages.len() {
            while loop_updates
                .front()
                .is_some_and(|(position, _)| *position <= index)
            {
                let (_, content) = loop_updates.pop_front().unwrap();
                lines.push(record_loop(log, snapshot, content, &mut parent, &timestamp));
            }
            let message = snapshot.messages[index].clone();
            let role = message.role;
            let is_meta = log.meta_messages.contains(&index);
            let prompt = (role == Role::User && !is_meta)
                .then(|| typed_prompt(&message))
                .flatten();
            let blocks = message.content.len();
            let uuid = snapshot
                .message_ids
                .get(&index)
                .copied()
                .unwrap_or_else(Uuid::new_v4);
            let mut entry = chain_entry(snapshot, index, uuid, message, parent, &timestamp);
            if entry.is_compact_boundary() && entry.parent_uuid.is_none() {
                entry.logical_parent_uuid = parent;
            }
            entry.git_branch = git_branch
                .get_or_insert_with(|| current_git_branch(&snapshot.workdir))
                .clone();
            entry.is_meta = is_meta;
            parent = Some(entry.uuid);
            log.links.push(MessageLink {
                uuids: vec![entry.uuid],
                blocks,
            });
            lines.push(Line::chain(role, entry));
            if let Some(prompt) = prompt {
                lines.push(Line::LastPrompt(LastPromptEntry {
                    chat_id: snapshot.id,
                    last_prompt: prompt,
                }));
            }
        }
        for (_, content) in loop_updates {
            lines.push(record_loop(log, snapshot, content, &mut parent, &timestamp));
        }
        for goal in snapshot
            .loops
            .iter()
            .filter(|goal| first_recorded_loops.contains(&goal.id))
        {
            if let Some(content) = plan_entry_content(snapshot, goal) {
                lines.push(record_loop(log, snapshot, content, &mut parent, &timestamp));
            }
        }
        if !lines.is_empty() {
            log.last_entry_milliseconds = milliseconds;
            log.parent_cursor = parent;
        }
        Some((log.path.clone(), lines))
    }

    /// 恢复后接管已有文件：已加载的消息沿用文件中的条目，之后的条目接在叶子后面。
    fn adopt(
        &self,
        chat_id: ChatId,
        path: PathBuf,
        links: Vec<MessageLink>,
        meta_messages: HashSet<usize>,
        loaded: LoadedTranscript,
    ) {
        // 之后的条目接在链上最后一个条目之后，包括叶子之后接回的尾随 Loop 条目（例如一轮回复
        // 结束后写出的停止）；接在最后一条消息之后会让这些 Loop 条目脱离新链，已停止的 Loop 复活。
        // 恢复时过滤掉的消息不作为父条目。
        let kept_uuids = links
            .iter()
            .flat_map(|link| link.uuids.iter())
            .collect::<HashSet<_>>();
        let parent_cursor = loaded
            .chain
            .iter()
            .rev()
            .find(|entry| entry.is_loop_record() || kept_uuids.contains(&entry.uuid))
            .map(|entry| entry.uuid);
        self.writer
            .adopt_existing(&path, loaded.uuids, loaded.metadata);
        self.logs.lock().unwrap().insert(
            chat_id,
            ChatLog {
                path,
                links,
                meta_messages,
                parent_cursor,
                load_warnings: loaded.warnings,
                last_entry_milliseconds: 0,
                recorded_loops: HashMap::new(),
                created_loops: HashSet::new(),
            },
        );
    }

    /// 恢复后的 Loop 视为已在链上：恢复规则带来的变化（转为已中断、解除武装）不写入文件，
    /// 之后再次恢复时按同样的规则得到同样的结果，等待手动恢复的 Trigger 也不会因此丢失。
    fn adopt_restored_loops(&self, snapshot: &ChatSnapshot) {
        if let Some(log) = self.logs.lock().unwrap().get_mut(&snapshot.id) {
            log.recorded_loops = snapshot
                .loops
                .iter()
                .map(|goal| (goal.id, loop_entry_content(snapshot, goal)))
                .collect();
            log.created_loops = snapshot.loops.iter().map(|goal| goal.id).collect();
        }
    }

    fn materialized_paths(&self) -> Vec<PathBuf> {
        self.logs
            .lock()
            .unwrap()
            .values()
            .map(|log| log.path.clone())
            .filter(|path| self.writer.is_materialized(path))
            .collect()
    }

    pub(super) fn flush_on_drop(&self, chats: &mut HashMap<ChatId, Chat>) {
        self.closed.cancel();
        for chat in chats.values() {
            self.enqueue_sync(&chat.snapshot);
        }
        self.writer.flush_blocking();
        let writes = chats
            .iter()
            .filter_map(|(chat_id, chat)| {
                let window = &chat.denial_window;
                if window.revision <= window.saved_revision {
                    return None;
                }
                Some((
                    *chat_id,
                    self.denial_path_for_chat(*chat_id)?,
                    window.revision,
                    window.records.iter().cloned().collect(),
                ))
            })
            .collect();
        write_denial_windows(
            &self.denial_flush_gate,
            &self.denial_saved_revisions,
            writes,
        );
    }
}

fn chain_entry(
    snapshot: &ChatSnapshot,
    index: usize,
    uuid: Uuid,
    message: Message,
    parent: Option<Uuid>,
    timestamp: &str,
) -> ChainEntry {
    let is_assistant = message.role == Role::Assistant;
    let marker = snapshot.message_compactions.get(&index);
    let boundary = match marker {
        Some(CompactionMarker::Boundary(boundary)) => Some(boundary),
        _ => None,
    };
    let summarize_metadata = match marker {
        Some(CompactionMarker::Summary { summarize_metadata }) => Some(summarize_metadata),
        _ => None,
    };
    ChainEntry {
        uuid,
        // 照 Claude Code `insertMessageChain`：压缩边界的父指针为空，对话链在边界处断开。被之后的
        // 定向摘要保留的旧边界接在前一条消息之后，保持直线链。
        parent_uuid: if boundary.is_some_and(|boundary| !boundary.preserved_by_later_summary) {
            None
        } else {
            parent
        },
        logical_parent_uuid: None,
        is_sidechain: false,
        chat_id: snapshot.id,
        timestamp: timestamp.to_owned(),
        cwd: snapshot.workdir.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        git_branch: None,
        message,
        invoked_skills: snapshot.invoked_skills.clone(),
        is_meta: false,
        execution_mode: snapshot
            .prompt_execution_modes
            .get(&index)
            .and_then(|mode| serde_json::to_value(mode).ok()),
        is_inherited_prompt: snapshot.inherited_prompts.contains(&index),
        original_user_message_id: snapshot.original_user_message_ids.get(&index).copied(),
        run_id: snapshot.message_runs.get(&index).copied(),
        contexts: snapshot
            .message_contexts
            .get(&index)
            .and_then(|contexts| serde_json::to_value(contexts).ok()),
        continues_message: false,
        subtype: boundary.map(|_| COMPACT_BOUNDARY_SUBTYPE.to_owned()),
        compact_metadata: boundary.map(compact_metadata_entry),
        is_compact_summary: summarize_metadata.is_some(),
        summarize_metadata: summarize_metadata.and_then(Option::as_ref).map(|metadata| {
            SummarizeMetadataEntry {
                messages_summarized: metadata.messages_summarized,
                user_context: metadata.user_context.clone(),
                direction: match metadata.direction {
                    SummaryDirection::FromHere => PartialCompactDirection::From,
                    SummaryDirection::UpToHere => PartialCompactDirection::UpTo,
                },
            }
        }),
        is_visible_in_transcript_only: summarize_metadata.is_some_and(Option::is_none),
        usage: snapshot.message_usage.get(&index).cloned(),
        // 进程内切换模型要求对话中没有私有推理，所以写入时 Chat 的模型就是生成带私有推理消息的模型。
        model: is_assistant.then(|| snapshot.model.identity()),
        private_source: (is_assistant
            && snapshot.model.auth_source == crate::model::ModelAuthSource::CodexSubscription)
            .then(|| snapshot.model.private_history_source())
            .flatten(),
        loop_id: snapshot
            .message_runs
            .get(&index)
            .and_then(|run| snapshot.run_loops.get(run))
            .copied(),
        slug: snapshot.plan_slug.clone(),
        loop_record: None,
    }
}

/// Loop 在对话链上的完整状态。等待手动恢复的 Trigger 一并记录，重启后仍能由用户重新武装。
fn loop_entry_content(snapshot: &ChatSnapshot, goal: &LoopSnapshot) -> LoopEntryContent {
    (
        LoopRecord {
            loop_id: goal.id,
            goal: goal.goal.clone(),
            state: serde_json::to_value(&goal.state).unwrap_or(serde_json::Value::Null),
            trigger: goal
                .trigger
                .as_ref()
                .and_then(|trigger| serde_json::to_value(trigger).ok()),
            trigger_awaits_resume: snapshot.resume_triggers.contains(&goal.id),
            plan_version: None,
            progress: None,
        },
        goal.run_id,
    )
}

/// 带计划版本与进度的 Loop 状态；该 Loop 既没有计划也没有进度时返回 `None`。
fn plan_entry_content(snapshot: &ChatSnapshot, goal: &LoopSnapshot) -> Option<LoopEntryContent> {
    let plan_version = super::plan::highest_plan_version(snapshot, goal.id);
    let progress = snapshot
        .progress
        .iter()
        .find(|progress| progress.loop_id == goal.id)
        .and_then(|progress| serde_json::to_value(&progress.todos).ok());
    if plan_version.is_none() && progress.is_none() {
        return None;
    }
    let (mut record, run_id) = loop_entry_content(snapshot, goal);
    record.plan_version = plan_version;
    record.progress = progress;
    Some((record, run_id))
}

/// 把 Loop 状态写成接在 `parent` 之后的 system 条目，并记为链上的最新状态。
fn record_loop(
    log: &mut ChatLog,
    snapshot: &ChatSnapshot,
    (record, run_id): LoopEntryContent,
    parent: &mut Option<Uuid>,
    timestamp: &str,
) -> Line {
    let loop_id = record.loop_id;
    let is_ended = matches!(
        serde_json::from_value::<LoopState>(record.state.clone()),
        Ok(LoopState::Stopped | LoopState::Expired)
    );
    let subtype = if !log.created_loops.contains(&loop_id) {
        LOOP_CREATED_SUBTYPE
    } else if is_ended {
        LOOP_ENDED_SUBTYPE
    } else if log
        .recorded_loops
        .get(&loop_id)
        .is_some_and(|(previous, _)| {
            previous.trigger != record.trigger
                || previous.trigger_awaits_resume != record.trigger_awaits_resume
        })
    {
        LOOP_TRIGGER_SUBTYPE
    } else {
        LOOP_STATE_SUBTYPE
    };
    let entry = ChainEntry {
        uuid: Uuid::new_v4(),
        parent_uuid: *parent,
        logical_parent_uuid: None,
        is_sidechain: false,
        chat_id: snapshot.id,
        timestamp: timestamp.to_owned(),
        cwd: snapshot.workdir.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        git_branch: None,
        message: Message {
            role: Role::System,
            content: Vec::new(),
        },
        invoked_skills: snapshot.invoked_skills.clone(),
        is_meta: false,
        execution_mode: None,
        is_inherited_prompt: false,
        original_user_message_id: None,
        run_id,
        contexts: None,
        continues_message: false,
        subtype: Some(subtype.to_owned()),
        compact_metadata: None,
        is_compact_summary: false,
        summarize_metadata: None,
        is_visible_in_transcript_only: false,
        usage: None,
        model: None,
        private_source: None,
        loop_id: None,
        slug: snapshot.plan_slug.clone(),
        loop_record: Some(record.clone()),
    };
    *parent = Some(entry.uuid);
    log.created_loops.insert(loop_id);
    log.recorded_loops
        .insert(loop_id, (record.without_plan(), run_id));
    Line::System(entry)
}

/// Tick Loop 不写 `preservedSegment`（见 `ChatRecorder::compact`）；读取时仍兼容带区段的文件。
fn compact_metadata_entry(boundary: &CompactBoundary) -> CompactMetadata {
    CompactMetadata {
        trigger: match boundary.trigger {
            CompactTrigger::Manual => CompactTriggerEntry::Manual,
            CompactTrigger::Auto => CompactTriggerEntry::Auto,
        },
        pre_tokens: boundary.pre_tokens,
        user_context: boundary.user_context.clone(),
        messages_summarized: boundary.messages_summarized,
        preserved_segment: None,
        original_user_inputs: boundary.original_user_inputs.clone(),
    }
}

fn compaction_marker(entry: &ChainEntry) -> Option<CompactionMarker> {
    if let Some(metadata) = entry
        .compact_metadata
        .as_ref()
        .filter(|_| entry.is_compact_boundary())
    {
        return Some(CompactionMarker::Boundary(CompactBoundary {
            trigger: match metadata.trigger {
                CompactTriggerEntry::Manual => CompactTrigger::Manual,
                CompactTriggerEntry::Auto => CompactTrigger::Auto,
            },
            pre_tokens: metadata.pre_tokens,
            user_context: metadata.user_context.clone(),
            messages_summarized: metadata.messages_summarized,
            // 链上有父条目的边界不是对话链的起点：Tick Loop 重写的保留边界，或 Claude Code
            // 保留区段重接后的边界。
            preserved_by_later_summary: entry.parent_uuid.is_some(),
            original_user_inputs: metadata.original_user_inputs.clone(),
            model: None,
        }));
    }
    entry.is_compact_summary.then(|| CompactionMarker::Summary {
        summarize_metadata: entry
            .summarize_metadata
            .as_ref()
            .map(|metadata| SummarizeMetadata {
                messages_summarized: metadata.messages_summarized,
                user_context: metadata.user_context.clone(),
                direction: match metadata.direction {
                    PartialCompactDirection::From => SummaryDirection::FromHere,
                    PartialCompactDirection::UpTo => SummaryDirection::UpToHere,
                },
            }),
    })
}

/// 用户输入的纯文本提示；本地消息（中断标记、`!` 命令、任务通知）不算。
fn typed_prompt(message: &Message) -> Option<String> {
    let text = message
        .content
        .iter()
        .map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?
        .join("\n");
    (!text.trim().is_empty() && LocalMessage::parse(&text).is_none()).then_some(text)
}

/// 从工作目录向上查找 `.git/HEAD`；工作树中的 `.git` 文件按 `gitdir:` 跳转。不启动 git 进程。
fn current_git_branch(workdir: &Path) -> Option<String> {
    for directory in workdir.ancestors() {
        let dot_git = directory.join(".git");
        let git_directory = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let pointer = std::fs::read_to_string(&dot_git).ok()?;
            let target = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
            directory.join(target)
        } else {
            continue;
        };
        let head = std::fs::read_to_string(git_directory.join("HEAD")).ok()?;
        return Some(
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map_or_else(|| "HEAD".into(), str::to_owned),
        );
    }
    None
}

struct RestoredChain {
    messages: Vec<Message>,
    invoked_skills: Vec<crate::skills::InvokedSkill>,
    message_runs: BTreeMap<usize, RunId>,
    message_contexts: BTreeMap<usize, Vec<InputContext>>,
    message_ids: BTreeMap<usize, Uuid>,
    original_user_message_ids: BTreeMap<usize, Uuid>,
    prompt_execution_modes: BTreeMap<usize, ExecutionMode>,
    inherited_prompts: HashSet<usize>,
    message_compactions: BTreeMap<usize, CompactionMarker>,
    message_usage: BTreeMap<usize, Usage>,
    /// Assistant private history sources are used only while restoring a transcript.
    message_sources: BTreeMap<usize, PrivateHistorySource>,
    links: Vec<MessageLink>,
    meta_messages: HashSet<usize>,
    message_of_entry: HashMap<Uuid, usize>,
    /// 链上的 Loop 条目，按链上顺序，连同条目之前的消息数。
    loop_records: Vec<(usize, LoopEntryContent)>,
    /// 条目记录的 run 所属 Loop。
    run_loops: BTreeMap<RunId, LoopId>,
    /// 照 Claude Code `getSlugFromLog`：链上第一个带 slug 的条目。链上第一个 slug 无效（可能被
    /// 篡改）时取之后第一个有效的 slug（恢复时换用的新 slug 写在之后的条目上）；都无效时保留
    /// 第一个，由重建计划时警告并换用新的 slug。
    plan_slug: Option<String>,
}

/// Restores chain entries into messages and preserves user prompts with execution modes as rewind
/// points.
fn restore_chain(chain: &[ChainEntry]) -> (RestoredChain, Vec<String>) {
    let mut restored = RestoredChain {
        messages: Vec::new(),
        invoked_skills: Vec::new(),
        message_runs: BTreeMap::new(),
        message_contexts: BTreeMap::new(),
        message_ids: BTreeMap::new(),
        original_user_message_ids: BTreeMap::new(),
        prompt_execution_modes: BTreeMap::new(),
        inherited_prompts: HashSet::new(),
        message_compactions: BTreeMap::new(),
        message_usage: BTreeMap::new(),
        message_sources: BTreeMap::new(),
        links: Vec::new(),
        meta_messages: HashSet::new(),
        message_of_entry: HashMap::new(),
        loop_records: Vec::new(),
        run_loops: BTreeMap::new(),
        plan_slug: None,
    };
    let mut execution_mode_warnings = Vec::new();
    for entry in chain {
        if !entry.invoked_skills.is_empty() {
            restored.invoked_skills = entry.invoked_skills.clone();
        }
        if let Some(slug) = &entry.slug
            && !restored
                .plan_slug
                .as_deref()
                .is_some_and(super::plan::is_valid_plan_slug)
            && (restored.plan_slug.is_none() || super::plan::is_valid_plan_slug(slug))
        {
            restored.plan_slug = Some(slug.clone());
        }
        let entry_loop = entry
            .loop_record
            .as_ref()
            .map(|record| record.loop_id)
            .or(entry.loop_id);
        if let (Some(run_id), Some(loop_id)) = (entry.run_id, entry_loop) {
            restored.run_loops.insert(run_id, loop_id);
        }
        if let Some(record) = entry.loop_record.clone().filter(|_| entry.is_loop_record()) {
            restored
                .loop_records
                .push((restored.messages.len(), (record, entry.run_id)));
            continue;
        }
        let continues_message = entry.continues_message
            && restored
                .messages
                .last()
                .is_some_and(|message| message.role == entry.message.role);
        if continues_message {
            let index = restored.messages.len() - 1;
            restored.messages[index]
                .content
                .extend(entry.message.content.iter().cloned());
            let link = &mut restored.links[index];
            link.uuids.push(entry.uuid);
            link.blocks = restored.messages[index].content.len();
            restored.message_of_entry.insert(entry.uuid, index);
            continue;
        }
        let index = restored.messages.len();
        if let Some(run_id) = entry.run_id {
            restored.message_runs.insert(index, run_id);
        }
        if let Some(contexts) = entry
            .contexts
            .clone()
            .and_then(|value| serde_json::from_value::<Vec<InputContext>>(value).ok())
        {
            restored.message_contexts.insert(index, contexts);
        }
        if entry.is_meta {
            restored.meta_messages.insert(index);
        }
        if let Some(marker) = compaction_marker(entry) {
            if matches!(marker, CompactionMarker::Boundary(_)) {
                restored.message_ids.insert(index, entry.uuid);
            }
            restored.message_compactions.insert(index, marker);
        }
        if entry.is_inherited_prompt {
            restored.inherited_prompts.insert(index);
        }
        if let Some(source_id) = entry.original_user_message_id {
            restored.original_user_message_ids.insert(index, source_id);
        }
        if let Some(usage) = entry
            .usage
            .clone()
            .filter(|_| entry.message.role == Role::Assistant)
        {
            restored.message_usage.insert(index, usage);
        }
        if let Some(source) = entry
            .private_source
            .clone()
            .filter(|_| entry.message.role == Role::Assistant)
        {
            restored.message_sources.insert(index, source);
        }
        let execution_mode = entry.execution_mode.as_ref().map(|value| {
            let (mode, warning) = restored_execution_mode(value);
            if let Some(warning) = warning
                && !execution_mode_warnings.contains(&warning)
            {
                execution_mode_warnings.push(warning);
            }
            mode
        });
        if let Some(execution_mode) = execution_mode
            && entry.message.role == Role::User
            && !entry.is_meta
            && typed_prompt(&entry.message).is_some()
        {
            restored
                .prompt_execution_modes
                .insert(index, execution_mode);
            restored.message_ids.insert(index, entry.uuid);
        }
        restored.links.push(MessageLink {
            uuids: vec![entry.uuid],
            blocks: entry.message.content.len(),
        });
        restored.messages.push(entry.message.clone());
        restored.message_of_entry.insert(entry.uuid, index);
    }
    (restored, execution_mode_warnings)
}

fn restored_execution_mode(value: &serde_json::Value) -> (ExecutionMode, Option<String>) {
    let (mode, replacement) = match value.as_str() {
        Some("acceptEdits") => return (ExecutionMode::AcceptEdits, None),
        Some("plan") => return (ExecutionMode::Plan, None),
        Some("auto") => return (ExecutionMode::Auto, None),
        Some("fullAccess") => return (ExecutionMode::FullAccess, None),
        Some("ask") => return (ExecutionMode::AcceptEdits, None),
        Some("default" | "manual") => (ExecutionMode::AcceptEdits, "Ask"),
        Some("dontAsk" | "bypass") => (ExecutionMode::Plan, "Plan"),
        Some(other) => {
            return (
                ExecutionMode::Plan,
                Some(format!(
                    "unknown historical execution mode `{other}` was recovered as Plan"
                )),
            );
        }
        None => {
            return (
                ExecutionMode::Plan,
                Some("invalid historical execution mode was recovered as Plan".into()),
            );
        }
    };
    (
        mode,
        Some(format!(
            "historical execution mode `{}` was recovered as {replacement}",
            value.as_str().expect("matched string execution mode")
        )),
    )
}

impl RestoredChain {
    /// 去掉给定下标的消息，其余消息依次前移。
    fn without(&self, removed: &BTreeSet<usize>) -> Self {
        let mut kept = Self {
            messages: Vec::new(),
            invoked_skills: self.invoked_skills.clone(),
            message_runs: BTreeMap::new(),
            message_contexts: BTreeMap::new(),
            message_ids: BTreeMap::new(),
            original_user_message_ids: BTreeMap::new(),
            prompt_execution_modes: BTreeMap::new(),
            inherited_prompts: HashSet::new(),
            message_compactions: BTreeMap::new(),
            message_usage: BTreeMap::new(),
            message_sources: BTreeMap::new(),
            links: Vec::new(),
            meta_messages: HashSet::new(),
            message_of_entry: HashMap::new(),
            run_loops: self.run_loops.clone(),
            plan_slug: self.plan_slug.clone(),
            loop_records: self
                .loop_records
                .iter()
                .map(|(position, content)| {
                    (
                        position - removed.range(..*position).count(),
                        content.clone(),
                    )
                })
                .collect(),
        };
        for (original, message) in self.messages.iter().enumerate() {
            if removed.contains(&original) {
                continue;
            }
            let index = kept.messages.len();
            if let Some(run_id) = self.message_runs.get(&original) {
                kept.message_runs.insert(index, *run_id);
            }
            if let Some(contexts) = self.message_contexts.get(&original) {
                kept.message_contexts.insert(index, contexts.clone());
            }
            if let Some(id) = self.message_ids.get(&original) {
                kept.message_ids.insert(index, *id);
            }
            if let Some(id) = self.original_user_message_ids.get(&original) {
                kept.original_user_message_ids.insert(index, *id);
            }
            if let Some(mode) = self.prompt_execution_modes.get(&original) {
                kept.prompt_execution_modes.insert(index, *mode);
            }
            if self.inherited_prompts.contains(&original) {
                kept.inherited_prompts.insert(index);
            }
            if self.meta_messages.contains(&original) {
                kept.meta_messages.insert(index);
            }
            if let Some(marker) = self.message_compactions.get(&original) {
                kept.message_compactions.insert(index, marker.clone());
            }
            if let Some(usage) = self.message_usage.get(&original) {
                kept.message_usage.insert(index, usage.clone());
            }
            if let Some(source) = self.message_sources.get(&original) {
                kept.message_sources.insert(index, source.clone());
            }
            for uuid in &self.links[original].uuids {
                kept.message_of_entry.insert(*uuid, index);
            }
            kept.links.push(self.links[original].clone());
            kept.messages.push(message.clone());
        }
        kept
    }
}

struct RecoveredSnapshot {
    snapshot: ChatSnapshot,
    meta_messages: HashSet<usize>,
}

/// 由对话链与元数据重建 Chat：工作目录取链上最后一个条目的 `cwd`，Branch 来源取 `branch-origin`
/// 与标题元数据。`removed` 是本次恢复照 Claude Code 删除的消息。
fn recover_transcript(
    chat_id: ChatId,
    loaded: &LoadedTranscript,
    restored: &RestoredChain,
    removed: &BTreeSet<usize>,
    source_chat_id: ChatId,
) -> Result<RecoveredSnapshot, AgentError> {
    let kept = restored.without(removed);
    let workdir = loaded
        .chain
        .last()
        .map(|entry| entry.cwd.clone())
        .unwrap_or_default();
    let mut snapshot = super::empty_snapshot(chat_id, workdir, super::unselected_model());
    snapshot.branch_origin = restored_branch_origin(loaded, source_chat_id);
    kept.fill_conversation(&mut snapshot);
    recover_snapshot_for_resume(snapshot, kept.meta_messages.clone())
}

/// Branch 来源元数据属于写入这份 Transcript 的 Chat；名称取标题，没有有效标题时使用默认名称。
fn restored_branch_origin(
    loaded: &LoadedTranscript,
    source_chat_id: ChatId,
) -> Option<BranchOrigin> {
    let entry = loaded
        .metadata
        .branch_origin
        .as_ref()
        .filter(|entry| entry.chat_id == source_chat_id)?;
    let name = loaded
        .metadata
        .custom_title
        .as_ref()
        .filter(|title| title.chat_id == source_chat_id)
        .map(|title| title.custom_title.trim().to_owned())
        .filter(|name| super::branch::is_valid_branch_name(name))
        .unwrap_or_else(|| UNTITLED_BRANCH_NAME.into());
    Some(BranchOrigin {
        source_chat_id: entry.forked_from.chat_id,
        name,
        creation: None,
    })
}

impl RestoredChain {
    fn fill_conversation(&self, snapshot: &mut ChatSnapshot) {
        snapshot.messages = self.messages.clone();
        snapshot.invoked_skills = self.invoked_skills.clone();
        snapshot.message_runs = self.message_runs.clone();
        snapshot.message_contexts = self.message_contexts.clone();
        snapshot.message_ids = self.message_ids.clone();
        snapshot.original_user_message_ids = self.original_user_message_ids.clone();
        snapshot.prompt_execution_modes = self.prompt_execution_modes.clone();
        snapshot.inherited_prompts = self.inherited_prompts.clone();
        snapshot.message_usage = self.message_usage.clone();
        snapshot.message_compactions = self.message_compactions.clone();
        snapshot.run_loops = self.run_loops.clone();
        snapshot.plan_slug = self.plan_slug.clone();
        self.replay_loops(snapshot);
    }

    /// 链上 Loop 条目携带的计划版本与进度。
    fn plan_checkpoints(&self) -> Vec<super::plan::PlanCheckpoint> {
        self.loop_records
            .iter()
            .filter(|(_, (record, _))| record.plan_version.is_some() || record.progress.is_some())
            .map(|(position, (record, _))| super::plan::PlanCheckpoint {
                position: *position,
                loop_id: record.loop_id,
                plan_version: record.plan_version,
                progress: record
                    .progress
                    .clone()
                    .and_then(|todos| serde_json::from_value::<Vec<TodoItem>>(todos).ok()),
            })
            .collect()
    }

    /// 沿对话链按顺序重放 Loop 条目：每条以完整状态覆盖同一 Loop，创建位置取第一条的位置。
    /// 无法解析的条目跳过。
    fn replay_loops(&self, snapshot: &mut ChatSnapshot) {
        snapshot.loops.clear();
        snapshot.loop_starts.clear();
        snapshot.resume_triggers.clear();
        for (position, (record, run_id)) in &self.loop_records {
            let Ok(state) = serde_json::from_value::<LoopState>(record.state.clone()) else {
                continue;
            };
            let Ok(trigger) = record
                .trigger
                .clone()
                .map(serde_json::from_value::<Trigger>)
                .transpose()
            else {
                continue;
            };
            let goal = LoopSnapshot {
                id: record.loop_id,
                goal: record.goal.clone(),
                state,
                run_id: *run_id,
                trigger,
            };
            match snapshot
                .loops
                .iter_mut()
                .find(|existing| existing.id == goal.id)
            {
                Some(existing) => *existing = goal,
                None => {
                    snapshot.loop_starts.insert(goal.id, *position);
                    snapshot.loops.push(goal);
                }
            }
            snapshot
                .resume_triggers
                .retain(|loop_id| *loop_id != record.loop_id);
            if record.trigger_awaits_resume {
                snapshot.resume_triggers.push(record.loop_id);
            }
        }
    }
}

/// Restores conversation and chain-backed state. Process-local state, including execution mode,
/// pending interactions, queued input, operations, and usage, starts from defaults.
/// Removed assistant messages take their tool calls with them, so no operation state is migrated.
fn recover_snapshot_for_resume(
    mut snapshot: ChatSnapshot,
    mut meta_messages: HashSet<usize>,
) -> Result<RecoveredSnapshot, AgentError> {
    super::triggers::recover_loops(&mut snapshot);
    super::recovery::close_dangling_tool_calls(&mut snapshot, None)?;
    append_interruption_messages(&mut snapshot, &mut meta_messages);
    super::branch::validate_snapshot(&snapshot)?;
    super::context::projected_messages(&snapshot)?;
    Ok(RecoveredSnapshot {
        snapshot,
        meta_messages,
    })
}

/// 照 Claude Code 恢复过滤：所有 tool_use 都没有结果的 assistant 消息、只含 thinking
/// 或空白文本的 assistant 消息不再进入对话。返回要删除的消息下标。
fn unrecoverable_assistant_messages(messages: &[Message]) -> BTreeSet<usize> {
    let resolved = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::Assistant && is_unrecoverable_assistant(message, &resolved)
        })
        .map(|(index, _)| index)
        .collect()
}

fn is_unrecoverable_assistant(message: &Message, resolved: &HashSet<&str>) -> bool {
    let tool_uses = message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !tool_uses.is_empty() {
        return tool_uses.iter().all(|id| !resolved.contains(id));
    }
    !message.content.is_empty()
        && (message.content.iter().all(|content| {
            matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            )
        }) || message
            .content
            .iter()
            .all(|content| matches!(content, Content::Text { text } if text.trim().is_empty())))
}

/// 照 Claude Code `deserializeMessagesWithInterruptDetection`：末尾是工具结果时先追加
/// "从中断处继续"的 meta 消息；之后只要末尾是 user 消息，就追加不请求回复的哨兵。
/// meta 消息不参与中断判定。
fn append_interruption_messages(snapshot: &mut ChatSnapshot, meta_messages: &mut HashSet<usize>) {
    let last_turn_message = |snapshot: &ChatSnapshot| {
        (0..snapshot.messages.len())
            .rev()
            .find(|index| snapshot.messages[*index].role != Role::System)
    };
    if let Some(index) = last_turn_message(snapshot) {
        let message = &snapshot.messages[index];
        if message.role == Role::User
            && !meta_messages.contains(&index)
            && message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolResult { .. }))
        {
            meta_messages.insert(snapshot.messages.len());
            snapshot.messages.push(Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: CONTINUE_FROM_INTERRUPTION.into(),
                }],
            });
        }
    }
    if last_turn_message(snapshot).is_some_and(|index| snapshot.messages[index].role == Role::User)
    {
        snapshot.messages.push(Message {
            role: Role::Assistant,
            content: vec![Content::Text {
                text: NO_RESPONSE_REQUESTED.into(),
            }],
        });
    }
}

/// 照 Claude Code `buildFileHistorySnapshotChain`：沿对话链收集文件回滚快照，更新条目替换
/// 同一消息的旧快照；不在当前链上的快照（被放弃的分叉）不生效。
fn file_history_snapshots(loaded: &LoadedTranscript) -> Vec<FileHistorySnapshot> {
    let mut snapshots: Vec<FileHistorySnapshot> = Vec::new();
    let mut index_by_message = HashMap::new();
    for entry in &loaded.chain {
        let Some(record) = loaded.file_history_snapshots.get(&entry.uuid) else {
            continue;
        };
        let Ok(snapshot) = serde_json::from_value::<FileHistorySnapshot>(record.snapshot.clone())
        else {
            continue;
        };
        let existing = record
            .is_snapshot_update
            .then(|| index_by_message.get(&snapshot.message_id).copied())
            .flatten();
        match existing {
            Some(index) => snapshots[index] = snapshot,
            None => {
                index_by_message.insert(snapshot.message_id, snapshots.len());
                snapshots.push(snapshot);
            }
        }
    }
    snapshots
}

impl Agent {
    /// 持久化入口：只把 Chat 标脏，由后台冲刷任务写入 Transcript。
    pub(super) fn save(&self, snapshot: &ChatSnapshot) {
        if let Some(recorder) = &self.inner.recorder {
            recorder.mark_dirty(snapshot.id);
            self.ensure_flush_loop(recorder);
        }
    }

    fn ensure_flush_loop(&self, recorder: &ChatRecorder) {
        if recorder.flush_loop_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            recorder.flush_loop_started.store(false, Ordering::SeqCst);
            return;
        };
        let host = Arc::downgrade(&self.inner);
        let notify = recorder.notify.clone();
        let closed = recorder.closed.clone();
        runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = closed.cancelled() => break,
                    _ = notify.notified() => {}
                }
                tokio::select! {
                    _ = closed.cancelled() => break,
                    _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
                }
                let Some(inner) = host.upgrade() else {
                    break;
                };
                Agent { inner }.flush_dirty_transcripts().await;
            }
        });
    }

    async fn flush_dirty_transcripts(&self) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        {
            let chats = self.inner.chats.lock().await;
            for chat_id in recorder.take_dirty() {
                if let Some(chat) = chats.get(&chat_id) {
                    recorder.enqueue_sync(&chat.snapshot);
                }
            }
        }
        let writer = recorder.writer.clone();
        let _ = tokio::task::spawn_blocking(move || writer.flush_blocking()).await;
        self.flush_denial_histories().await;
    }

    async fn flush_denial_histories(&self) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        let writes = {
            let chats = self.inner.chats.lock().await;
            chats
                .iter()
                .filter_map(|(chat_id, chat)| {
                    let window = &chat.denial_window;
                    if window.revision <= window.saved_revision {
                        return None;
                    }
                    Some((
                        *chat_id,
                        recorder.denial_path_for_chat(*chat_id)?,
                        window.revision,
                        window.records.iter().cloned().collect(),
                    ))
                })
                .collect::<Vec<DenialWrite>>()
        };
        if writes.is_empty() {
            return;
        }
        let gate = recorder.denial_flush_gate.clone();
        let saved_revisions = recorder.denial_saved_revisions.clone();
        let outcomes = match tokio::task::spawn_blocking(move || {
            write_denial_windows(&gate, &saved_revisions, writes)
        })
        .await
        {
            Ok(outcomes) => outcomes,
            Err(error) => {
                let mut chats = self.inner.chats.lock().await;
                for chat in chats
                    .values_mut()
                    .filter(|chat| chat.denial_window.revision > chat.denial_window.saved_revision)
                {
                    chat.denial_window.save_error = Some(error.to_string());
                }
                return;
            }
        };
        let mut retry = false;
        let mut chats = self.inner.chats.lock().await;
        for (chat_id, revision, result) in outcomes {
            let Some(chat) = chats.get_mut(&chat_id) else {
                continue;
            };
            match result {
                Ok(()) => {
                    chat.denial_window.saved_revision =
                        chat.denial_window.saved_revision.max(revision);
                    chat.denial_window.save_error = None;
                }
                Err(error) => {
                    chat.denial_window.save_error = Some(error);
                    retry = true;
                }
            }
        }
        drop(chats);
        if retry {
            recorder.notify.notify_one();
        }
    }

    /// 立即把本进程已加载的全部 Chat 写入 Transcript。进程随后直接退出时使用。
    pub async fn flush_transcripts(&self) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        {
            let chats = self.inner.chats.lock().await;
            recorder.take_dirty();
            for chat in chats.values() {
                recorder.enqueue_sync(&chat.snapshot);
            }
        }
        let writer = recorder.writer.clone();
        let _ = tokio::task::spawn_blocking(move || writer.flush_blocking()).await;
        self.flush_denial_histories().await;
    }

    /// 退出时照 Claude Code：先冲刷，再把元数据重新追加到每个 Transcript 末尾。
    pub(super) async fn close_transcripts(&self) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        self.flush_transcripts().await;
        let writer = recorder.writer.clone();
        let paths = recorder.materialized_paths();
        let _ = tokio::task::spawn_blocking(move || {
            for path in paths {
                writer.reappend_metadata_blocking(&path);
            }
        })
        .await;
    }

    /// 记录本次新做出的内容替换决定。不持久化的 Agent 只保留运行期状态。
    pub(super) fn record_content_replacements(
        &self,
        snapshot: &ChatSnapshot,
        replacements: Vec<ContentReplacementRecord>,
    ) {
        if let Some(recorder) = &self.inner.recorder {
            recorder.enqueue_content_replacements(snapshot.id, replacements);
        }
        self.save(snapshot);
    }

    /// 尚未解决的 Transcript 写入失败。写入失败不影响运行，失败的条目留在队列中，
    /// 之后每次冲刷都会重试；重试成功后返回 `None`。
    pub fn transcript_write_error(&self) -> Option<String> {
        self.inner
            .recorder
            .as_ref()
            .and_then(|recorder| recorder.writer.unresolved_error())
    }

    /// 加载该 Chat 的 Transcript 时发现的问题（例如保留区段校验失败后加载了完整历史）。
    pub fn transcript_load_warnings(&self, chat_id: ChatId) -> Vec<String> {
        self.inner
            .recorder
            .as_ref()
            .and_then(|recorder| {
                recorder
                    .logs
                    .lock()
                    .unwrap()
                    .get(&chat_id)
                    .map(|log| log.load_warnings.clone())
            })
            .unwrap_or_default()
    }

    /// 照 Claude Code 压缩后的 `reAppendSessionMetadata`：冲刷后把标题等元数据重新追加到文件
    /// 末尾，让只读尾部的列表仍能取到。
    ///
    /// Claude Code 同时清空按 sessionId 缓存的 uuid 集合（`clearSessionMessagesCache`），下次写入时从
    /// 文件重新读取；Tick Loop 的去重集合由写入器随写入增量维护，与文件内容一致，无需重建。
    pub(super) async fn reappend_transcript_metadata(&self, chat_id: ChatId) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        if let Some(chat) = self.inner.chats.lock().await.get(&chat_id) {
            recorder.enqueue_sync(&chat.snapshot);
        }
        let Some(path) = recorder
            .logs
            .lock()
            .unwrap()
            .get(&chat_id)
            .map(|log| log.path.clone())
        else {
            return;
        };
        let writer = recorder.writer.clone();
        let _ = tokio::task::spawn_blocking(move || writer.reappend_metadata_blocking(&path)).await;
    }

    /// 列出工作目录所属项目中已保存的 Chat，按最近活动时间从新到旧排列。
    pub async fn saved_chats(&self, workdir: &Path) -> Result<Vec<SavedChat>, AgentError> {
        let Some(recorder) = &self.inner.recorder else {
            return Ok(Vec::new());
        };
        self.flush_transcripts().await;
        let directory = project_directory(&recorder.root, workdir);
        let summaries = tokio::task::spawn_blocking(move || list_project(&directory))
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))?;
        Ok(summaries
            .into_iter()
            .map(|summary| SavedChat {
                id: summary.chat_id,
                title: summary.title,
                first_prompt: summary.first_prompt,
                last_prompt: summary.last_prompt,
                modified: summary.modified,
                cwd: summary.cwd,
                branch_source: summary.forked_from.map(|origin| origin.chat_id),
                git_branch: summary.git_branch,
            })
            .collect())
    }

    /// 按 id 恢复 Chat。已在本进程内存中时直接返回该实例；否则从 Transcript 加载，
    /// 执行恢复后处理，但不自动继续任何工作。
    pub async fn resume_chat(&self, chat_id: ChatId) -> Result<ChatSnapshot, AgentError> {
        if let Some(chat) = self.inner.chats.lock().await.get(&chat_id) {
            return Ok(chat.snapshot.clone());
        }
        let Some(recorder) = &self.inner.recorder else {
            return Err(AgentError::NotFound);
        };
        let _resume = recorder.resume_lock.lock().await;
        if let Some(chat) = self.inner.chats.lock().await.get(&chat_id) {
            return Ok(chat.snapshot.clone());
        }
        let (path, loaded) = self.load_saved_transcript(chat_id).await?;
        self.insert_restored_chat(chat_id, loaded, RestoreTarget::Resume(path))
            .await
    }

    /// 照 Claude Code `--fork-session`：从 Transcript 加载对话，换上新的 chatId，
    /// 写入新的 Transcript；原文件保持不变，原 Chat 不进入本进程。
    pub async fn resume_as_new_chat(
        &self,
        source_chat_id: ChatId,
    ) -> Result<ChatSnapshot, AgentError> {
        let Some(recorder) = &self.inner.recorder else {
            return Err(AgentError::NotFound);
        };
        let _resume = recorder.resume_lock.lock().await;
        self.flush_transcripts().await;
        let (_, loaded) = self.load_saved_transcript(source_chat_id).await?;
        let new_chat_id = loop {
            let candidate = Uuid::new_v4();
            if !self.inner.chats.lock().await.contains_key(&candidate) {
                break candidate;
            }
        };
        self.insert_restored_chat(
            new_chat_id,
            loaded,
            RestoreTarget::NewChat {
                source: source_chat_id,
            },
        )
        .await
    }

    async fn load_saved_transcript(
        &self,
        chat_id: ChatId,
    ) -> Result<(PathBuf, LoadedTranscript), AgentError> {
        let recorder = self.inner.recorder.as_ref().ok_or(AgentError::NotFound)?;
        let root = recorder.root.clone();
        tokio::task::spawn_blocking(move || {
            let Some(path) = locate(&root, chat_id)? else {
                return Ok(None);
            };
            Ok::<_, std::io::Error>(load(&path)?.map(|loaded| (path, loaded)))
        })
        .await
        .map_err(|error| AgentError::Storage(error.to_string()))?
        .map_err(|error| AgentError::Storage(error.to_string()))?
        .ok_or(AgentError::NotFound)
    }

    async fn insert_restored_chat(
        &self,
        chat_id: ChatId,
        loaded: LoadedTranscript,
        target: RestoreTarget,
    ) -> Result<ChatSnapshot, AgentError> {
        let recorder = self.inner.recorder.as_ref().ok_or(AgentError::NotFound)?;
        // 替换决定、文件回滚备份与元数据都属于写入这份 Transcript 的 Chat：恢复时是自己，以新 chatId 恢复时是源 Chat。
        let source_chat_id = match &target {
            RestoreTarget::Resume(_) => chat_id,
            RestoreTarget::NewChat { source } => *source,
        };
        let mut loaded = loaded;
        let (restored, execution_mode_warnings) = restore_chain(&loaded.chain);
        loaded.warnings.extend(execution_mode_warnings);
        let removed = unrecoverable_assistant_messages(&restored.messages);
        let mut recovered =
            recover_transcript(chat_id, &loaded, &restored, &removed, source_chat_id)?;
        self.require_working_directory_trust(&recovered.snapshot.workdir)?;
        let plans_directory = recorder.plans_directory();
        let plan_warnings = super::plan::rebuild_plans_and_progress(
            &mut recovered.snapshot,
            &restored.without(&removed).plan_checkpoints(),
            Some(&plans_directory),
        )?;
        loaded.warnings.extend(plan_warnings);
        let replacement_records = loaded
            .content_replacements
            .iter()
            .filter(|entry| entry.chat_id == source_chat_id)
            .flat_map(|entry| entry.replacements.iter().cloned())
            .collect::<Vec<_>>();
        let content_replacements = super::tool_results::ContentReplacementState::reconstruct(
            &recovered.snapshot,
            &replacement_records,
        );
        let mut snapshot = recovered.snapshot;
        // 与设置写入互斥到 Chat 加入本进程为止，期间写入的持久规则改动不会漏掉这个 Chat。
        let _settings_gate = self.inner.settings_writes.lock().await;
        let (client, output_style) = self.apply_chat_defaults(&mut snapshot);
        super::context::strip_private_reasoning_of_other_models(
            &mut snapshot,
            &restored.without(&removed).message_sources,
        );
        let mut file_history = FileHistory::restore(
            self.file_history_directory(source_chat_id, &snapshot.workdir),
            file_history_snapshots(&loaded),
        );
        let denial_window = match &target {
            RestoreTarget::Resume(path) => recorder.load_denials(path, chat_id),
            RestoreTarget::NewChat { .. } => DenialWindow::default(),
        };
        match target {
            RestoreTarget::Resume(path) => {
                recorder.adopt(
                    chat_id,
                    path,
                    restored.without(&removed).links,
                    recovered.meta_messages,
                    loaded,
                );
                recorder.adopt_restored_loops(&snapshot);
            }
            RestoreTarget::NewChat { .. } => {
                // 照 Claude Code `copyPlanForFork`：以新 chatId 恢复时使用新的 slug 并复制计划文件。
                super::plan::copy_plans_to_new_slug(&mut snapshot, Some(&plans_directory));
                recorder.register_resumed_copy(
                    chat_id,
                    &snapshot.workdir,
                    recovered.meta_messages,
                    loaded.metadata,
                    loaded.warnings,
                );
                // 重写的对话以压缩边界开头；替换决定与文件回滚快照写在对话之后，新文件超过 5MB
                // 时加载不会随边界之前的内容跳过它们。
                recorder.enqueue_sync(&snapshot);
                // 照 Claude Code `processResumedConversation`：以新 chatId 重新记录源 Chat
                // 的全部替换决定；替换文本仍指向源 Chat 目录中的外存文件。
                if !replacement_records.is_empty() {
                    recorder.enqueue_content_replacements(chat_id, replacement_records);
                }
                // 照 Claude Code `copyFileHistoryForResume`：备份硬链接到新 Chat 的目录，
                // 复制成功的快照以新 chatId 重新记录。
                let (copied, recorded) =
                    file_history.copy_to(self.file_history_directory(chat_id, &snapshot.workdir));
                file_history = copied;
                for snapshot in &recorded {
                    recorder.record_file_history_snapshot(
                        chat_id,
                        snapshot.message_id,
                        snapshot,
                        false,
                    );
                }
            }
        }
        let tools = self.create_chat_tools(chat_id, &snapshot.workdir);
        let skills = crate::skills::SkillRegistry::discover(
            &snapshot.workdir,
            self.inner.user_skills_directory.as_deref(),
        );
        self.inner.chats.lock().await.insert(
            chat_id,
            Chat {
                snapshot: snapshot.clone(),
                active: None,
                active_run: None,
                parked: HashMap::new(),
                ready_runs: VecDeque::new(),
                client,
                pending_client: None,
                pending_model_thinking: None,
                tools,
                skills,
                content_replacements,
                file_history,
                watching_commands: false,
                watching_user_shells: false,
                user_shell_queue: Arc::new(Notify::new()),
                trigger_cancellations: HashMap::new(),
                deferred_messages: Vec::new(),
                user_cancelled_runs: HashSet::new(),
                output_style,
                auto_denials: super::auto_mode::DenialTracking::default(),
                denial_window,
            },
        );
        self.save(&snapshot);
        Ok(snapshot)
    }
}

enum RestoreTarget {
    /// 继续写入原 Transcript。
    Resume(PathBuf),
    /// 写入新 chatId 的 Transcript。
    NewChat { source: ChatId },
}
