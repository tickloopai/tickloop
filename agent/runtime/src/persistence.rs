//! 运行时与 Transcript 的适配层。
//!
//! `Agent::save` 只把 Chat 标脏；后台冲刷任务最多每 100ms 取一次已提交的快照，
//! 与已写出的内容做差分，把新的链上条目追加到 `TranscriptStore`。Chat 快照不整体保存：恢复时按
//! id 加载 Transcript，由对话链与元数据重建 Chat，并执行 Claude Code 的恢复后处理。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::WaitHosting;
use super::denials::DenialWindow;
use super::{
    Agent, AgentError, Chat, ChatBookkeeping,
    compaction::{CompactionLayout, MessageSource},
};
use crate::file_history::{FileHistory, FileHistorySnapshot};
use crate::transcript::{
    CachedMetadata, ChatPlacement, FLUSH_INTERVAL, JsonlTranscriptStore, LoadRange,
    LoadedTranscript, Settled, StorePosition, StoredTranscript, TranscriptError, TranscriptOutbox,
    TranscriptStore, assemble,
};
use tickloop_agent_base::private_fs::unix_time_milliseconds;
use tickloop_agent_base::timestamp::format_timestamp;
use tickloop_agent_protocol::projection;
use tickloop_agent_protocol::{
    AgentMessageSource, ApprovalRecord, BranchOrigin, ChatId, ChatSnapshot, CompactBoundary,
    CompactTrigger, CompactionMarker, Content, DelegationMetadata, DenialRecord,
    DirectedHumanSource, Event, EventKind, ExecutionMode, FamilyCompletion, FamilyInputRecord,
    HostNote, InputContext, LocalMessage, LoopId, LoopSnapshot, LoopState, MAX_RECENT_DENIALS,
    Message, ModelConfig, MonitorMembership, PrivateHistorySource, Role, RunId, SavedChat,
    SubagentRegistration, SummarizeMetadata, SummaryDirection, TodoItem, ToolCallRecovery, Trigger,
    Usage,
};
use tickloop_agent_transcript::{
    AiTitleEntry, ArtifactTicksEntry, BranchOriginEntry, COMPACT_BOUNDARY_SUBTYPE, ChainEntry,
    CompactMetadata, CompactTriggerEntry, ContentReplacementEntry, ContentReplacementRecord,
    CustomTitleEntry, DelegationEntry, FamilyCompletionsEntry, FamilyInputsEntry,
    FileHistorySnapshotEntry, ForkedFrom, LOOP_CREATED_SUBTYPE, LOOP_ENDED_SUBTYPE,
    LOOP_STATE_SUBTYPE, LOOP_TRIGGER_SUBTYPE, LastPromptEntry, Line, LoopRecord,
    MonitorMembersEntry, PartialCompactDirection, ProvenanceEntry, SentAgentMessagesEntry,
    SubagentsEntry, SummarizeMetadataEntry,
};

/// 照 Claude Code：恢复时末尾是工具结果，追加这条 meta user 消息表示从中断处继续。
const CONTINUE_FROM_INTERRUPTION: &str = "Continue from where you left off.";
/// 照 Claude Code `NO_RESPONSE_REQUESTED`：末尾是 user 消息时追加的 assistant 哨兵。
const NO_RESPONSE_REQUESTED: &str = "No response requested.";
/// 从 Transcript 恢复的 Branch 没有标题时使用的名称。
const UNTITLED_BRANCH_NAME: &str = "Branch";

pub(super) struct ChatRecorder {
    store: Arc<dyn TranscriptStore>,
    /// The local JSONL store, whose root and Chat files also place denials, plans, File History
    /// and tool-result sidecars. Another store keeps none of those.
    local: Option<JsonlTranscriptStore>,
    outbox: TranscriptOutbox,
    /// Held while appending, so batches reach the store in the order they were formed.
    appends: tokio::sync::Mutex<()>,
    logs: StdMutex<HashMap<ChatId, ChatLog>>,
    dirty: StdMutex<HashSet<ChatId>>,
    notify: Arc<Notify>,
    flush_loop_started: AtomicBool,
    /// Cuts the flush loop's wait short.
    flush_now: Arc<Notify>,
    closed: CancellationToken,
    pub(super) resume_lock: tokio::sync::Mutex<()>,
    denial_flush_gate: Arc<StdMutex<()>>,
    denial_saved_revisions: Arc<StdMutex<HashMap<ChatId, u64>>>,
}

struct ChatLog {
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
    /// The lifecycle and ledger revision written last.
    recorded_lifecycle: u64,
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
                tickloop_agent_base::private_fs::create_private_directory(directory)
                    .map_err(|error| error.to_string())?;
                tickloop_agent_base::private_fs::replace_file_atomically(
                    &path,
                    &bytes,
                    tickloop_agent_base::private_fs::PRIVATE_FILE_MODE,
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

/// Which chain a load restores.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ChatLoad {
    /// The physical entry the chain ends at; `None` takes the newest leaf.
    pub leaf: Option<Uuid>,
}

/// Loop 条目记录的内容：完整状态与 run id。
type LoopEntryContent = (LoopRecord, Option<RunId>);

/// 之后的条目接在链上最后一个条目之后，包括叶子之后接回的尾随 Loop 条目（例如一轮回复
/// 结束后写出的停止）；接在最后一条消息之后会让这些 Loop 条目脱离新链，已停止的 Loop 复活。
/// 恢复时过滤掉的消息不作为父条目。
fn adopted_parent_cursor(links: &[MessageLink], loaded: &LoadedTranscript) -> Option<Uuid> {
    let kept_uuids = links
        .iter()
        .flat_map(|link| link.uuids.iter())
        .collect::<HashSet<_>>();
    loaded
        .chain
        .iter()
        .rev()
        .find(|entry| entry.is_loop_record() || kept_uuids.contains(&entry.uuid))
        .map(|entry| entry.uuid)
}

/// The Loop records a restored chain already holds, with the plan state actually recorded, and
/// the Loops it created.
fn recorded_loop_entries(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    checkpoints: &[super::plan::PlanCheckpoint],
) -> (HashMap<LoopId, LoopEntryContent>, HashSet<LoopId>) {
    let recorded_plans = checkpoints
        .iter()
        .filter_map(|checkpoint| {
            checkpoint
                .plan_version
                .map(|version| (checkpoint.loop_id, (version, checkpoint.approval_pending)))
        })
        .collect::<HashMap<_, _>>();
    let records = snapshot
        .loops
        .iter()
        .map(|goal| {
            let (mut record, run) = loop_entry_content(snapshot, bookkeeping, goal);
            record.plan_version = recorded_plans.get(&goal.id).map(|(version, _)| *version);
            record.plan_approval_pending = recorded_plans
                .get(&goal.id)
                .is_some_and(|(_, pending)| *pending);
            (goal.id, (record, run))
        })
        .collect();
    let created = snapshot.loops.iter().map(|goal| goal.id).collect();
    (records, created)
}

#[derive(Clone)]
struct MessageLink {
    uuids: Vec<Uuid>,
    blocks: usize,
}

impl ChatRecorder {
    pub(super) async fn read_subagents(
        &self,
        parent: ChatId,
    ) -> Result<Vec<SubagentRegistration>, AgentError> {
        let loaded = self.load_chat(parent, ChatLoad::default()).await?;
        Ok(loaded
            .metadata
            .subagents
            .filter(|entry| entry.chat_id == parent)
            .map(|entry| entry.subagents)
            .unwrap_or_default())
    }

    /// A saved Chat's Transcript: from the latest boundary, and in full when the chain does not
    /// reach the root there.
    pub(super) async fn load_chat(
        &self,
        chat: ChatId,
        load: ChatLoad,
    ) -> Result<LoadedTranscript, AgentError> {
        let tail = self
            .load_stored(chat, LoadRange::FromLatestBoundary)
            .await?
            .ok_or(AgentError::NotFound)?;
        if let (Some(loaded), true) = assemble(&tail, load.leaf) {
            return Ok(loaded);
        }
        let full = self
            .load_stored(chat, LoadRange::Full)
            .await?
            .ok_or(AgentError::NotFound)?;
        assemble(&full, load.leaf).0.ok_or(AgentError::NotFound)
    }

    /// Every Line the store keeps of a recorded Chat.
    pub(super) async fn stored_lines(&self, chat: ChatId) -> Option<Vec<Line>> {
        if !self.logs.lock().unwrap().contains_key(&chat) {
            return None;
        }
        let stored = self.load_stored(chat, LoadRange::Full).await.ok()??;
        Some(stored.retained.into_iter().chain(stored.lines).collect())
    }

    async fn load_stored(
        &self,
        chat: ChatId,
        range: LoadRange,
    ) -> Result<Option<StoredTranscript>, TranscriptError> {
        self.unless_fenced()?;
        let loaded = self.store.load(chat, range).await;
        self.observe(loaded)
    }

    /// Whichever store call answers Fenced, a newer holder owns this Harness's Chats: every
    /// outbox stops, and nothing more reaches the store.
    fn observe<T>(&self, result: Result<T, TranscriptError>) -> Result<T, TranscriptError> {
        if matches!(result, Err(TranscriptError::Fenced)) {
            self.outbox.fence();
        }
        result
    }

    fn unless_fenced(&self) -> Result<(), TranscriptError> {
        match self.outbox.is_fenced() {
            true => Err(TranscriptError::Fenced),
            false => Ok(()),
        }
    }

    pub(super) fn record_subagents(&self, chat_id: ChatId, subagents: Vec<SubagentRegistration>) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::Subagents(SubagentsEntry { chat_id, subagents })],
            None,
        );
    }

    pub(super) fn record_provenance(&self, chat_id: ChatId, entry: ProvenanceEntry) {
        self.outbox
            .enqueue(chat_id, vec![Line::Provenance(entry)], None);
    }

    pub(super) fn record_sent_agent_messages(
        &self,
        chat_id: ChatId,
        messages: BTreeMap<Uuid, AgentMessageSource>,
    ) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::SentAgentMessages(SentAgentMessagesEntry {
                chat_id,
                messages,
            })],
            None,
        );
    }

    pub(super) fn record_family_inputs(
        &self,
        chat_id: ChatId,
        inputs: BTreeMap<Uuid, FamilyInputRecord>,
    ) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::FamilyInputs(FamilyInputsEntry { chat_id, inputs })],
            None,
        );
    }

    pub(super) fn record_family_completions(
        &self,
        chat_id: ChatId,
        completions: Vec<FamilyCompletion>,
    ) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::FamilyCompletions(FamilyCompletionsEntry {
                chat_id,
                completions,
            })],
            None,
        );
    }

    pub(super) fn record_ai_title(&self, chat_id: ChatId, title: String) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::AiTitle(AiTitleEntry {
                chat_id,
                ai_title: title,
            })],
            None,
        );
    }

    pub(super) fn local_store(&self) -> Option<JsonlTranscriptStore> {
        self.local.clone()
    }

    /// The JSONL file of a recorded Chat in the local store.
    pub(super) fn local_chat_path(&self, chat_id: ChatId) -> Option<PathBuf> {
        if !self.logs.lock().unwrap().contains_key(&chat_id) {
            return None;
        }
        self.local.as_ref()?.chat_path(chat_id)
    }

    fn denial_path_for_chat(&self, chat_id: ChatId) -> Option<PathBuf> {
        Some(
            self.local_chat_path(chat_id)?
                .with_extension("denials.json"),
        )
    }

    fn load_denials(&self, chat_id: ChatId) -> DenialWindow {
        let Some(path) = self.denial_path_for_chat(chat_id) else {
            return DenialWindow::default();
        };
        let contents = match tickloop_agent_base::private_fs::open_without_following_links(
            &path,
            tickloop_agent_base::private_fs::OpenTarget::Entry,
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

    pub(super) fn new(store: Arc<dyn TranscriptStore>) -> Self {
        let local = (store.clone() as Arc<dyn std::any::Any + Send + Sync>)
            .downcast::<JsonlTranscriptStore>()
            .ok()
            .map(|local| (*local).clone());
        Self {
            store,
            local,
            outbox: TranscriptOutbox::default(),
            appends: tokio::sync::Mutex::new(()),
            logs: StdMutex::default(),
            dirty: StdMutex::default(),
            notify: Arc::new(Notify::new()),
            flush_loop_started: AtomicBool::new(false),
            flush_now: Arc::new(Notify::new()),
            closed: CancellationToken::new(),
            resume_lock: tokio::sync::Mutex::new(()),
            denial_flush_gate: Arc::new(StdMutex::new(())),
            denial_saved_revisions: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub(super) async fn register_chat(
        &self,
        chat_id: ChatId,
        folder: Option<&Path>,
    ) -> Result<(), AgentError> {
        self.register(chat_id, &placement(folder), HashSet::new(), Vec::new())
            .await
            .map(|_| ())
    }

    /// Registers the Chat with the store and starts its log. `false` when the store keeps no
    /// Transcript for it.
    async fn register(
        &self,
        chat_id: ChatId,
        placement: &ChatPlacement,
        meta_messages: HashSet<usize>,
        load_warnings: Vec<String>,
    ) -> Result<bool, AgentError> {
        self.unless_fenced()?;
        let registered = self.store.register(chat_id, placement).await;
        let position = match self.observe(registered) {
            Ok(position) => position,
            Err(TranscriptError::Unsupported) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        self.logs.lock().unwrap().insert(
            chat_id,
            ChatLog {
                links: Vec::new(),
                meta_messages,
                parent_cursor: None,
                load_warnings,
                last_entry_milliseconds: 0,
                recorded_loops: HashMap::new(),
                created_loops: HashSet::new(),
                recorded_lifecycle: 0,
            },
        );
        self.outbox.register(chat_id, position);
        Ok(true)
    }

    /// 以新 chatId 恢复的 Chat（`--fork-session`）写入工作目录所属项目中的新 Transcript：已加载的消息作为新条目重写，
    /// 标题照 Claude Code 随元数据带到新 Transcript，Branch 来源以新 chatId 重新记录。
    async fn register_resumed_copy(
        &self,
        chat_id: ChatId,
        folder: Option<&Path>,
        meta_messages: HashSet<usize>,
        metadata: CachedMetadata,
        load_warnings: Vec<String>,
    ) -> Result<(), AgentError> {
        if !self
            .register(chat_id, &placement(folder), meta_messages, load_warnings)
            .await?
        {
            return Ok(());
        }
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
        self.outbox.enqueue(
            chat_id,
            custom_title
                .into_iter()
                .chain(branch_origin)
                .chain(metadata.artifact_ticks.map(|entry| {
                    Line::ArtifactTicks(ArtifactTicksEntry {
                        chat_id,
                        cwd: folder.map(Path::to_path_buf),
                        artifact_ticks: entry.artifact_ticks,
                        removed_artifact_ticks: entry.removed_artifact_ticks,
                    })
                }))
                .chain(
                    metadata
                        .monitor_members
                        .map(|entry| entry.monitor.for_new_chat())
                        .filter(|monitor| !monitor.is_empty())
                        .map(|monitor| {
                            Line::MonitorMembers(MonitorMembersEntry {
                                chat_id,
                                cwd: folder.map(Path::to_path_buf),
                                monitor,
                            })
                        }),
                )
                .collect(),
            None,
        );
        Ok(())
    }

    /// 把 Branch 的来源与标题排入子 Chat 的 Transcript；子 Chat 与来源放在一起。
    pub(super) async fn register_branch(
        &self,
        source: &ChatSnapshot,
        source_bookkeeping: &ChatBookkeeping,
        child_chat_id: ChatId,
        title: &str,
    ) -> Result<(), AgentError> {
        self.enqueue_sync(source, source_bookkeeping);
        let leaf = {
            let logs = self.logs.lock().unwrap();
            let Some(log) = logs.get(&source.id) else {
                return Ok(());
            };
            log.links.last().and_then(|link| link.uuids.last().copied())
        };
        if !self
            .register(
                child_chat_id,
                &ChatPlacement::BesideChat(source.id),
                HashSet::new(),
                Vec::new(),
            )
            .await?
        {
            return Ok(());
        }
        let mut lines = vec![
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
        ];
        let monitor = source.monitor.for_new_chat();
        if !monitor.is_empty() {
            lines.push(Line::MonitorMembers(MonitorMembersEntry {
                chat_id: child_chat_id,
                cwd: source.folder.clone(),
                monitor,
            }));
        }
        if !source.artifact_ticks.is_empty() || !source.removed_artifact_ticks.is_empty() {
            lines.push(Line::ArtifactTicks(ArtifactTicksEntry {
                chat_id: child_chat_id,
                cwd: source.folder.clone(),
                artifact_ticks: source.artifact_ticks.clone(),
                removed_artifact_ticks: source.removed_artifact_ticks.clone(),
            }));
        }
        self.outbox.enqueue(child_chat_id, lines, None);
        Ok(())
    }

    /// 照 Claude Code `recordContentReplacement`：替换决定作为元数据条目追加到 Chat 的 Transcript。
    fn enqueue_content_replacements(
        &self,
        chat_id: ChatId,
        replacements: Vec<ContentReplacementRecord>,
    ) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::ContentReplacement(ContentReplacementEntry {
                chat_id,
                replacements,
            })],
            None,
        );
    }

    /// rewind 截断对话前调用：先写出截断前尚未写出的消息，被放弃的消息照 Claude Code 留在
    /// 文件中；之后的条目接在最后保留的消息后面，在同一文件中形成分叉。
    pub(super) fn truncate(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
        message_count: usize,
    ) {
        self.enqueue_sync(snapshot, bookkeeping);
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
        chat: &Chat,
        compacted: &ChatSnapshot,
        compacted_bookkeeping: &ChatBookkeeping,
        layout: &CompactionLayout,
        renamed_snapshots: &[FileHistorySnapshot],
    ) {
        let Some(mut lines) = self.sync_chat(&chat.snapshot, &chat.bookkeeping) else {
            return;
        };
        let events_through = Some(compacted.last_sequence);
        let compaction_lines =
            self.compaction_lines(&chat.snapshot, compacted, compacted_bookkeeping, layout);
        let Some(compaction_lines) = compaction_lines else {
            self.outbox.enqueue(compacted.id, lines, events_through);
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
        let replacements = chat.content_replacements.records_for(compacted);
        if !replacements.is_empty() {
            lines.push(Line::ContentReplacement(ContentReplacementEntry {
                chat_id: compacted.id,
                replacements,
            }));
        }
        self.outbox.enqueue(compacted.id, lines, events_through);
    }

    /// 压缩后对话中需要新写的链上条目，同时把已写出条目的对应表换成压缩后的对话。已写出的条目
    /// 与压缩前的对话对不上时返回 `None`，不做任何修改。
    fn compaction_lines(
        &self,
        snapshot: &ChatSnapshot,
        compacted: &ChatSnapshot,
        compacted_bookkeeping: &ChatBookkeeping,
        layout: &CompactionLayout,
    ) -> Option<Vec<Line>> {
        let folder = compacted.folder.as_deref();
        let mut logs = self.logs.lock().unwrap();
        let log = logs
            .get_mut(&snapshot.id)
            .filter(|log| log.links.len() == snapshot.messages.len())?;
        let keeps_existing = layout.keeps_existing_prefix(snapshot.messages.len());
        let old_links = std::mem::take(&mut log.links);
        let milliseconds = unix_time_milliseconds().max(log.last_entry_milliseconds + 1);
        let timestamp = format_timestamp(milliseconds);
        let git_branch = folder.and_then(current_git_branch);
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
                let start = compacted_bookkeeping
                    .loop_starts
                    .get(&goal.id)
                    .copied()
                    .unwrap_or(0);
                (
                    start.max(first_loop_position),
                    loop_entry_content(compacted, compacted_bookkeeping, goal),
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
                    compacted_bookkeeping,
                    content,
                    &mut parent,
                    EntryStamp {
                        timestamp: &timestamp,
                        folder,
                    },
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
            let mut entry = chain_entry(
                compacted,
                compacted_bookkeeping,
                index,
                uuid,
                message,
                parent,
                EntryStamp {
                    timestamp: &timestamp,
                    folder,
                },
            );
            if *source == MessageSource::Boundary {
                entry.logical_parent_uuid = layout
                    .logical_parent
                    .and_then(|original| old_links.get(original))
                    .and_then(|link| link.uuids.last().copied());
            }
            entry.git_branch = git_branch.clone();
            entry.is_meta |= matches!(source, MessageSource::Existing(original)
                if log.meta_messages.contains(original));
            // A rewritten part of an answer names the entry it copies, so a display projection
            // that already holds that content only aliases it.
            if let MessageSource::Existing(original) = source
                && entry.response_id.is_some()
                && entry.original_message_id.is_none()
            {
                entry.original_message_id = old_links[*original].uuids.first().copied();
            }
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
                compacted_bookkeeping,
                content,
                &mut parent,
                EntryStamp {
                    timestamp: &timestamp,
                    folder,
                },
            ));
        }
        // 边界之前的计划与进度工具调用不再在链上：在全部消息之后为这些 Loop 记下当时的计划版本与进度。
        for goal in compacted
            .loops
            .iter()
            .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
        {
            if let Some(content) = plan_entry_content(compacted, compacted_bookkeeping, goal) {
                lines.push(record_loop(
                    log,
                    compacted,
                    compacted_bookkeeping,
                    content,
                    &mut parent,
                    EntryStamp {
                        timestamp: &timestamp,
                        folder,
                    },
                ));
            }
        }
        for goal in &compacted.loops {
            log.recorded_loops
                .entry(goal.id)
                .or_insert_with(|| loop_entry_content(compacted, compacted_bookkeeping, goal));
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

    pub(super) fn file_history_root(&self) -> Option<PathBuf> {
        Some(
            self.local
                .as_ref()?
                .root()
                .join(crate::file_history::FILE_HISTORY_DIRECTORY),
        )
    }

    pub(super) fn plans_directory(&self) -> Option<PathBuf> {
        Some(super::plan::plans_directory(self.local.as_ref()?.root()))
    }

    /// 照 Claude Code `recordFileHistorySnapshot`：快照作为元数据条目追加到 Chat 的 Transcript。
    pub(super) fn record_file_history_snapshot(
        &self,
        chat_id: ChatId,
        message_id: Uuid,
        snapshot: &FileHistorySnapshot,
        is_snapshot_update: bool,
    ) {
        let Ok(snapshot) = serde_json::to_value(snapshot) else {
            return;
        };
        self.outbox.enqueue(
            chat_id,
            vec![Line::FileHistorySnapshot(FileHistorySnapshotEntry {
                message_id,
                snapshot,
                is_snapshot_update,
            })],
            None,
        );
    }

    /// The project directory of a recorded Chat in the local store.
    pub(super) fn local_project_directory(&self, chat_id: ChatId) -> Option<PathBuf> {
        Some(self.local_chat_path(chat_id)?.parent()?.to_path_buf())
    }

    fn mark_dirty(&self, chat_id: ChatId) {
        self.dirty.lock().unwrap().insert(chat_id);
        self.notify.notify_one();
    }

    fn take_dirty(&self) -> HashSet<ChatId> {
        std::mem::take(&mut *self.dirty.lock().unwrap())
    }

    fn enqueue_sync(&self, snapshot: &ChatSnapshot, bookkeeping: &ChatBookkeeping) {
        if let Some(lines) = self.sync_chat(snapshot, bookkeeping) {
            self.outbox.set_display_leaf(
                snapshot.id,
                bookkeeping
                    .lifecycle
                    .display_leaf(snapshot, &bookkeeping.message_responses),
            );
            self.outbox
                .enqueue(snapshot.id, lines, Some(snapshot.last_sequence));
        }
    }

    /// 差分依据：运行时对消息只有三种修改：追加新消息；向最后一条消息追加内容块（并行工具
    /// 结果），写成标记接续该消息的新条目；rewind 截断对话，由 `truncate` 事先同步到已写出的条目。
    fn sync_chat(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
    ) -> Option<Vec<Line>> {
        let folder = snapshot.folder.as_deref();
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
                let content = loop_entry_content(snapshot, bookkeeping, goal);
                if log.recorded_loops.get(&goal.id) == Some(&content) {
                    return None;
                }
                let position = if log.recorded_loops.contains_key(&goal.id) {
                    snapshot.messages.len()
                } else {
                    bookkeeping
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
                    bookkeeping,
                    index,
                    Uuid::new_v4(),
                    appended,
                    parent.or(log.links[index].uuids.last().copied()),
                    EntryStamp {
                        timestamp: &timestamp,
                        folder,
                    },
                );
                entry.subtype = None;
                entry.compact_metadata = None;
                entry.is_compact_summary = false;
                entry.summarize_metadata = None;
                entry.is_visible_in_transcript_only = false;
                entry.usage = None;
                entry.git_branch = git_branch
                    .get_or_insert_with(|| folder.and_then(current_git_branch))
                    .clone();
                entry.contexts = None;
                entry.execution_mode = None;
                entry.continues_message = true;
                entry.is_meta |= log.meta_messages.contains(&index);
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
                lines.push(record_loop(
                    log,
                    snapshot,
                    bookkeeping,
                    content,
                    &mut parent,
                    EntryStamp {
                        timestamp: &timestamp,
                        folder,
                    },
                ));
            }
            let message = snapshot.messages[index].clone();
            let role = message.role;
            let is_meta =
                log.meta_messages.contains(&index) || snapshot.host_notes.contains_key(&index);
            let uuid = snapshot
                .message_ids
                .get(&index)
                .copied()
                .unwrap_or_else(Uuid::new_v4);
            let prompt = (role == Role::User
                && !is_meta
                && !snapshot.agent_message_origins.contains_key(&uuid))
            .then(|| typed_prompt(&message))
            .flatten();
            let blocks = message.content.len();
            let mut entry = chain_entry(
                snapshot,
                bookkeeping,
                index,
                uuid,
                message,
                parent,
                EntryStamp {
                    timestamp: &timestamp,
                    folder,
                },
            );
            if entry.is_compact_boundary() && entry.parent_uuid.is_none() {
                entry.logical_parent_uuid = parent;
            }
            entry.git_branch = git_branch
                .get_or_insert_with(|| folder.and_then(current_git_branch))
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
            lines.push(record_loop(
                log,
                snapshot,
                bookkeeping,
                content,
                &mut parent,
                EntryStamp {
                    timestamp: &timestamp,
                    folder,
                },
            ));
        }
        for goal in snapshot
            .loops
            .iter()
            .filter(|goal| first_recorded_loops.contains(&goal.id))
        {
            if let Some(content) = plan_entry_content(snapshot, bookkeeping, goal) {
                lines.push(record_loop(
                    log,
                    snapshot,
                    bookkeeping,
                    content,
                    &mut parent,
                    EntryStamp {
                        timestamp: &timestamp,
                        folder,
                    },
                ));
            }
        }
        if !lines.is_empty() {
            log.last_entry_milliseconds = milliseconds;
            log.parent_cursor = parent;
        }
        lines.extend(bookkeeping.lifecycle.changed_after(log.recorded_lifecycle));
        log.recorded_lifecycle = bookkeeping.lifecycle.revision();
        Some(lines)
    }

    /// 恢复后接管已有文件：已加载的消息沿用文件中的条目，之后的条目接在叶子后面。
    fn adopt(
        &self,
        chat_id: ChatId,
        links: Vec<MessageLink>,
        meta_messages: HashSet<usize>,
        loaded: LoadedTranscript,
    ) {
        let parent_cursor = adopted_parent_cursor(&links, &loaded);
        self.outbox.adopt(
            chat_id,
            loaded.position,
            loaded.uuids,
            loaded.metadata,
            loaded.retained,
        );
        self.logs.lock().unwrap().insert(
            chat_id,
            ChatLog {
                links,
                meta_messages,
                parent_cursor,
                load_warnings: loaded.warnings,
                last_entry_milliseconds: 0,
                recorded_loops: HashMap::new(),
                created_loops: HashSet::new(),
                recorded_lifecycle: 0,
            },
        );
    }

    /// The physical entry a display node ends at on the Chat's current chain: a prompt's latest
    /// copy, or an answer's last entry. Lines produced so far are queued first, so every message
    /// has its entries.
    fn physical_endpoint(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
        display_id: Uuid,
    ) -> Option<Uuid> {
        self.enqueue_sync(snapshot, bookkeeping);
        let response_of = |index: usize| {
            bookkeeping
                .lifecycle
                .response_of(snapshot, &bookkeeping.message_responses, index)
        };
        let index = (0..snapshot.messages.len()).rev().find(|index| {
            response_of(*index) == Some(display_id)
                || (snapshot.prompt_execution_modes.contains_key(index)
                    && response_of(*index).is_none()
                    && snapshot
                        .original_message_ids
                        .get(index)
                        .or_else(|| snapshot.message_ids.get(index))
                        == Some(&display_id))
        })?;
        let logs = self.logs.lock().unwrap();
        logs.get(&snapshot.id)?
            .links
            .get(index)?
            .uuids
            .last()
            .copied()
    }

    /// A staged branch replaces the Chat's conversation: later entries follow the branch's leaf,
    /// what the store already holds of it is not written again, and Lines queued meanwhile stay
    /// queued.
    pub(super) fn adopt_branch(&self, chat_id: ChatId, log: StagedLog) {
        self.outbox
            .remember_written(chat_id, log.uuids, log.retained);
        let mut logs = self.logs.lock().unwrap();
        let last_entry_milliseconds = logs
            .get(&chat_id)
            .map_or(0, |log| log.last_entry_milliseconds);
        logs.insert(
            chat_id,
            ChatLog {
                links: log.links,
                meta_messages: log.meta_messages,
                parent_cursor: log.parent_cursor,
                load_warnings: log.load_warnings,
                last_entry_milliseconds,
                recorded_loops: log.recorded_loops,
                created_loops: log.created_loops,
                recorded_lifecycle: log.recorded_lifecycle,
            },
        );
    }

    /// The restored lifecycle and ledger records are already stored.
    fn adopt_lifecycle(&self, chat_id: ChatId, lifecycle: &super::lifecycle::Lifecycle) {
        if let Some(log) = self.logs.lock().unwrap().get_mut(&chat_id) {
            log.recorded_lifecycle = lifecycle.revision();
        }
    }

    /// Adopt ordinary recovery state without rewriting it. Keep the actual recorded plan state
    /// as the comparison baseline so plan normalization is checkpointed before the View loads.
    fn adopt_restored_loops(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
        checkpoints: &[super::plan::PlanCheckpoint],
    ) {
        if let Some(log) = self.logs.lock().unwrap().get_mut(&snapshot.id) {
            (log.recorded_loops, log.created_loops) =
                recorded_loop_entries(snapshot, bookkeeping, checkpoints);
        }
    }

    /// Appends what every Chat has queued; failures stay with the outbox for the next flush.
    async fn flush(&self) {
        let _appends = self.appends.lock().await;
        for chat in self.outbox.chats() {
            let _ = self.append_queued(chat, false).await;
        }
    }

    /// Appends the Chat's pending batch and then its queued Lines. With `durable`, the last
    /// batch also makes everything before it durable and the position it returns is committed.
    /// The caller holds `appends`.
    async fn append_queued(
        &self,
        chat: ChatId,
        durable: bool,
    ) -> Result<StorePosition, TranscriptError> {
        loop {
            let Some(batch) = self.outbox.next_batch(chat, durable) else {
                if let Some(refusal) = self.outbox.refusal(chat) {
                    return Err(refusal);
                }
                return Ok(self.outbox.position(chat).unwrap_or_default());
            };
            self.unless_fenced()?;
            let result = self.store.append(chat, &batch).await;
            match self.outbox.settle(chat, batch.id, result) {
                // A durable batch makes everything before it durable; Lines queued after a
                // retried one still need a batch of their own.
                Settled::Committed(ack) if batch.durable && !self.outbox.has_queued(chat) => {
                    return Ok(ack.position);
                }
                Settled::Committed(_) => {}
                Settled::Failed(message) => return Err(TranscriptError::Storage(message)),
                Settled::Refused(error) => return Err(error),
            }
        }
    }

    fn flush_blocking(&self) {
        // Nobody else can be appending once the Harness is dropped; if somebody is, their
        // batches must stay ahead of these.
        let Ok(_appends) = self.appends.try_lock() else {
            return;
        };
        for chat in self.outbox.chats() {
            while let Some(batch) = self.outbox.next_batch(chat, false) {
                if self.outbox.is_fenced() {
                    return;
                }
                let result = self.store.append_blocking(chat, &batch);
                if !matches!(
                    self.outbox.settle(chat, batch.id, result),
                    Settled::Committed(_)
                ) {
                    break;
                }
            }
        }
    }

    /// Queues the Chat's cached metadata again at the end of its Transcript, taking a custom
    /// title another process wrote to the local file. `false` when nothing was queued.
    async fn queue_metadata_again(&self, chat_id: ChatId) -> bool {
        let title = match self.local.clone() {
            Some(local) if self.outbox.is_stored(chat_id) => {
                tokio::task::spawn_blocking(move || local.latest_custom_title(chat_id))
                    .await
                    .ok()
                    .flatten()
            }
            _ => None,
        };
        self.outbox.reappend_metadata(chat_id, title)
    }

    pub(super) fn flush_on_drop(&self, chats: &mut HashMap<ChatId, Chat>) {
        self.closed.cancel();
        for chat in chats.values() {
            self.enqueue_sync(&chat.snapshot, &chat.bookkeeping);
        }
        self.flush_blocking();
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

/// When a recorded Chat's chain entry is written, and the Folder it names as its `cwd`: a Chat
/// without a Folder names none.
#[derive(Clone, Copy)]
struct EntryStamp<'a> {
    timestamp: &'a str,
    folder: Option<&'a Path>,
}

fn placement(folder: Option<&Path>) -> ChatPlacement {
    folder.map_or(ChatPlacement::Folderless, |folder| {
        ChatPlacement::Folder(folder.to_path_buf())
    })
}

fn chain_entry(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    index: usize,
    uuid: Uuid,
    message: Message,
    parent: Option<Uuid>,
    stamp: EntryStamp<'_>,
) -> ChainEntry {
    let is_assistant = message.role == Role::Assistant;
    let is_user = message.role == Role::User;
    let run_id = snapshot.message_runs.get(&index).copied();
    let tool_call_recoveries = snapshot
        .tool_call_recoveries
        .iter()
        .filter(|fact| {
            fact.run_id == run_id
                && message.content.iter().any(|content| {
                    matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == &fact.call_id)
                })
        })
        .cloned()
        .collect();
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
        timestamp: stamp.timestamp.to_owned(),
        cwd: stamp.folder.map(Path::to_path_buf),
        version: env!("CARGO_PKG_VERSION").into(),
        git_branch: None,
        message,
        invoked_skills: snapshot.invoked_skills.clone(),
        is_meta: snapshot.host_notes.contains_key(&index),
        host_tools: snapshot
            .host_notes
            .get(&index)
            .and_then(HostNote::announced_host_tools)
            .cloned(),
        plan_mode_exit: snapshot.host_notes.get(&index) == Some(&HostNote::PlanModeExit),
        execution_mode: snapshot
            .prompt_execution_modes
            .get(&index)
            .and_then(|mode| serde_json::to_value(mode).ok()),
        is_inherited_prompt: snapshot.inherited_prompts.contains(&index),
        original_message_id: snapshot.original_message_ids.get(&index).copied(),
        human_family_origin: snapshot.human_family_origins.get(&uuid).cloned(),
        agent_message_origin: snapshot
            .message_ids
            .get(&index)
            .and_then(|id| snapshot.agent_message_origins.get(id))
            .cloned(),
        run_id,
        tool_call_recoveries,
        contexts: snapshot
            .message_contexts
            .get(&index)
            .and_then(|contexts| serde_json::to_value(contexts).ok()),
        continues_message: false,
        subtype: boundary.map(|_| COMPACT_BOUNDARY_SUBTYPE.to_owned()),
        compact_metadata: boundary.map(|boundary| {
            compact_metadata_entry(boundary, bookkeeping.lifecycle.compacted_leaf())
        }),
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
        model: is_assistant
            .then(|| snapshot.message_models.get(&index).cloned())
            .flatten(),
        private_source: snapshot.message_sources.get(&index).cloned(),
        loop_id: snapshot
            .message_runs
            .get(&index)
            .and_then(|run| bookkeeping.run_loops.get(run))
            .copied(),
        slug: bookkeeping.plan_slug.clone(),
        response_id: bookkeeping.lifecycle.response_of(
            snapshot,
            &bookkeeping.message_responses,
            index,
        ),
        input_id: is_user
            .then(|| {
                snapshot
                    .original_message_ids
                    .get(&index)
                    .or_else(|| snapshot.message_ids.get(&index))
            })
            .flatten()
            .filter(|id| bookkeeping.lifecycle.is_input(**id))
            .copied(),
        loop_record: None,
    }
}

/// Loop 在对话链上的完整状态。等待手动恢复的 Trigger 一并记录，重启后仍能由用户重新武装。
fn loop_entry_content(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    goal: &LoopSnapshot,
) -> LoopEntryContent {
    (
        LoopRecord {
            subagent_report: goal
                .run_id
                .and_then(|run| snapshot.subagent_reports.get(&run).cloned()),
            failure_events: snapshot
                .failure_events
                .iter()
                .filter(|event| event.loop_id == goal.id)
                .filter_map(|event| serde_json::to_value(event).ok())
                .collect(),
            loop_id: goal.id,
            goal: goal.goal.clone(),
            state: serde_json::to_value(&goal.state).unwrap_or(serde_json::Value::Null),
            trigger: goal
                .trigger
                .as_ref()
                .and_then(|trigger| serde_json::to_value(trigger).ok()),
            trigger_awaits_resume: snapshot.resume_triggers.contains(&goal.id),
            event_sources: goal
                .event_sources
                .iter()
                .filter_map(|source| serde_json::to_value(source).ok())
                .collect(),
            plan_version: super::plan::highest_plan_version(snapshot, bookkeeping, goal.id),
            plan_approval_pending: snapshot
                .plans
                .iter()
                .any(|plan| plan.loop_id == goal.id && plan.approval_pending),
            progress: None,
            state_since: goal.state_since.map(serde_json::Value::from),
            run_started_at: goal.run_started_at,
        },
        goal.run_id,
    )
}

/// 带计划版本与进度的 Loop 状态；该 Loop 既没有计划也没有进度时返回 `None`。
fn plan_entry_content(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    goal: &LoopSnapshot,
) -> Option<LoopEntryContent> {
    let plan_version = super::plan::highest_plan_version(snapshot, bookkeeping, goal.id);
    let progress = snapshot
        .progress
        .iter()
        .find(|progress| progress.loop_id == goal.id)
        .and_then(|progress| serde_json::to_value(&progress.todos).ok());
    if plan_version.is_none() && progress.is_none() {
        return None;
    }
    let (mut record, run_id) = loop_entry_content(snapshot, bookkeeping, goal);
    record.plan_version = plan_version;
    record.progress = progress;
    Some((record, run_id))
}

/// A recorded Trigger as configured: without the latest scheduled time its Loop dealt with,
/// which each fire moves on without reconfiguring anything.
fn configured_trigger(trigger: &Option<serde_json::Value>) -> Option<serde_json::Value> {
    trigger.clone().map(|mut trigger| {
        if let Some(fields) = trigger.as_object_mut() {
            fields.remove("last_fired_at_ms");
        }
        trigger
    })
}

/// 把 Loop 状态写成接在 `parent` 之后的 system 条目，并记为链上的最新状态。
fn record_loop(
    log: &mut ChatLog,
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    (record, run_id): LoopEntryContent,
    parent: &mut Option<Uuid>,
    stamp: EntryStamp<'_>,
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
            configured_trigger(&previous.trigger) != configured_trigger(&record.trigger)
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
        timestamp: stamp.timestamp.to_owned(),
        cwd: stamp.folder.map(Path::to_path_buf),
        version: env!("CARGO_PKG_VERSION").into(),
        git_branch: None,
        message: Message {
            role: Role::System,
            content: Vec::new(),
        },
        invoked_skills: snapshot.invoked_skills.clone(),
        is_meta: false,
        host_tools: None,
        plan_mode_exit: false,
        execution_mode: None,
        is_inherited_prompt: false,
        original_message_id: None,
        agent_message_origin: None,
        human_family_origin: None,
        run_id,
        tool_call_recoveries: Vec::new(),
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
        slug: bookkeeping.plan_slug.clone(),
        response_id: None,
        input_id: None,
        loop_record: Some(record.clone()),
    };
    *parent = Some(entry.uuid);
    log.created_loops.insert(loop_id);
    log.recorded_loops
        .insert(loop_id, (record.without_progress(), run_id));
    Line::System(entry)
}

/// Tickloop 不写 `preservedSegment`（见 `ChatRecorder::compact`）；读取时仍兼容带区段的文件。
fn compact_metadata_entry(
    boundary: &CompactBoundary,
    display_leaf: Option<Uuid>,
) -> CompactMetadata {
    CompactMetadata {
        display_leaf,
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
            // 链上有父条目的边界不是对话链的起点：Tickloop 重写的保留边界，或 Claude Code
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
    (!text.trim().is_empty()
        && matches!(
            LocalMessage::parse(&text),
            None | Some(LocalMessage::AgentCommunication { .. })
        ))
    .then_some(text)
}

/// 从工作目录向上查找 `.git/HEAD`；工作树中的 `.git` 文件按 `gitdir:` 跳转。不启动 git 进程。
fn current_git_branch(folder: &Path) -> Option<String> {
    for directory in folder.ancestors() {
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
    invoked_skills: Vec<tickloop_agent_protocol::InvokedSkill>,
    message_runs: BTreeMap<usize, RunId>,
    message_responses: BTreeMap<usize, Uuid>,
    tool_call_recoveries: BTreeMap<usize, Vec<ToolCallRecovery>>,
    message_contexts: BTreeMap<usize, Vec<InputContext>>,
    message_ids: BTreeMap<usize, Uuid>,
    agent_message_origins: BTreeMap<Uuid, serde_json::Value>,
    human_family_origins: BTreeMap<Uuid, DirectedHumanSource>,
    original_message_ids: BTreeMap<usize, Uuid>,
    prompt_execution_modes: BTreeMap<usize, ExecutionMode>,
    inherited_prompts: HashSet<usize>,
    host_notes: BTreeMap<usize, HostNote>,
    message_compactions: BTreeMap<usize, CompactionMarker>,
    message_usage: BTreeMap<usize, Usage>,
    message_sources: BTreeMap<usize, PrivateHistorySource>,
    message_models: BTreeMap<usize, tickloop_agent_protocol::ModelIdentity>,
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
        message_responses: BTreeMap::new(),
        tool_call_recoveries: BTreeMap::new(),
        message_contexts: BTreeMap::new(),
        message_ids: BTreeMap::new(),
        agent_message_origins: BTreeMap::new(),
        human_family_origins: BTreeMap::new(),
        original_message_ids: BTreeMap::new(),
        prompt_execution_modes: BTreeMap::new(),
        inherited_prompts: HashSet::new(),
        host_notes: BTreeMap::new(),
        message_compactions: BTreeMap::new(),
        message_usage: BTreeMap::new(),
        message_sources: BTreeMap::new(),
        message_models: BTreeMap::new(),
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
        if let Some(mut record) = entry.loop_record.clone().filter(|_| entry.is_loop_record()) {
            if let Some(trigger) = record.trigger.as_mut()
                && trigger
                    .get("interval_seconds")
                    .is_some_and(serde_json::Value::is_u64)
                && trigger
                    .get("schedule")
                    .is_none_or(serde_json::Value::is_null)
                && trigger.get("armed").and_then(serde_json::Value::as_bool) == Some(true)
                && trigger
                    .get("armed_at_ms")
                    .is_none_or(serde_json::Value::is_null)
                && let Some(armed_at) =
                    tickloop_agent_base::timestamp::parse_timestamp_ms(&entry.timestamp)
                        .and_then(|value| u64::try_from(value).ok())
            {
                trigger["armed_at_ms"] = serde_json::json!(armed_at);
            }
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
        let index = restored.messages.len() - usize::from(continues_message);
        if !entry.tool_call_recoveries.is_empty() {
            restored
                .tool_call_recoveries
                .entry(index)
                .or_default()
                .extend(entry.tool_call_recoveries.iter().cloned());
        }
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
        restored.message_ids.insert(index, entry.uuid);
        if let Some(model) = entry
            .model
            .clone()
            .filter(|_| entry.message.role == Role::Assistant)
        {
            restored.message_models.insert(index, model);
        }
        if let Some(run_id) = entry.run_id {
            restored.message_runs.insert(index, run_id);
        }
        if let Some(response) = entry.response_id {
            restored.message_responses.insert(index, response);
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
            restored.message_compactions.insert(index, marker);
        }
        if entry.is_inherited_prompt {
            restored.inherited_prompts.insert(index);
        }
        if let Some(announced) = &entry.host_tools {
            restored
                .host_notes
                .insert(index, HostNote::HostTools(announced.clone()));
        } else if entry.plan_mode_exit {
            restored.host_notes.insert(index, HostNote::PlanModeExit);
        }
        if let Some(origin) = &entry.human_family_origin {
            restored
                .human_family_origins
                .insert(entry.uuid, origin.clone());
        }
        if let Some(origin) = &entry.agent_message_origin {
            restored
                .agent_message_origins
                .insert(entry.uuid, origin.clone());
        }
        if let Some(source_id) = entry.original_message_id {
            restored.original_message_ids.insert(index, source_id);
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
            && entry.agent_message_origin.is_none()
            && typed_prompt(&entry.message).is_some()
        {
            restored
                .prompt_execution_modes
                .insert(index, execution_mode);
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
            message_responses: BTreeMap::new(),
            tool_call_recoveries: BTreeMap::new(),
            message_contexts: BTreeMap::new(),
            message_ids: BTreeMap::new(),
            agent_message_origins: self.agent_message_origins.clone(),
            human_family_origins: self.human_family_origins.clone(),
            original_message_ids: BTreeMap::new(),
            prompt_execution_modes: BTreeMap::new(),
            inherited_prompts: HashSet::new(),
            host_notes: BTreeMap::new(),
            message_compactions: BTreeMap::new(),
            message_usage: BTreeMap::new(),
            message_sources: BTreeMap::new(),
            message_models: BTreeMap::new(),
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
            if let Some(response) = self.message_responses.get(&original) {
                kept.message_responses.insert(index, *response);
            }
            if let Some(facts) = self.tool_call_recoveries.get(&original) {
                kept.tool_call_recoveries.insert(index, facts.clone());
            }
            if let Some(contexts) = self.message_contexts.get(&original) {
                kept.message_contexts.insert(index, contexts.clone());
            }
            if let Some(id) = self.message_ids.get(&original) {
                kept.message_ids.insert(index, *id);
            }
            if let Some(id) = self.original_message_ids.get(&original) {
                kept.original_message_ids.insert(index, *id);
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
            if let Some(announced) = self.host_notes.get(&original) {
                kept.host_notes.insert(index, announced.clone());
            }
            if let Some(marker) = self.message_compactions.get(&original) {
                kept.message_compactions.insert(index, marker.clone());
            }
            if let Some(usage) = self.message_usage.get(&original) {
                kept.message_usage.insert(index, usage.clone());
            }
            if let Some(model) = self.message_models.get(&original) {
                kept.message_models.insert(index, model.clone());
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
    bookkeeping: ChatBookkeeping,
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
    waits: WaitHosting,
    family: Option<&super::family_load::FamilyRecords>,
) -> Result<RecoveredSnapshot, AgentError> {
    let kept = restored.without(removed);
    let folder = match loaded.chain.last() {
        Some(entry) => entry.cwd.clone(),
        None => loaded
            .metadata
            .artifact_ticks
            .as_ref()
            .and_then(|entry| entry.cwd.clone())
            .or_else(|| {
                loaded
                    .metadata
                    .monitor_members
                    .as_ref()
                    .and_then(|entry| entry.cwd.clone())
            }),
    };
    let mut snapshot = super::empty_snapshot(chat_id, folder, ModelConfig::unselected());
    snapshot.generated_title = loaded
        .metadata
        .ai_title
        .as_ref()
        .filter(|entry| entry.chat_id == source_chat_id)
        .map(|entry| entry.ai_title.clone());
    snapshot.sent_agent_messages = loaded
        .metadata
        .sent_agent_messages
        .as_ref()
        .filter(|entry| entry.chat_id == source_chat_id)
        .map(|entry| entry.messages.clone())
        .unwrap_or_default();
    snapshot.family_inputs = loaded
        .metadata
        .family_inputs
        .as_ref()
        .filter(|entry| entry.chat_id == source_chat_id)
        .map(|entry| entry.inputs.clone())
        .unwrap_or_default();
    snapshot.family_completions = loaded
        .metadata
        .family_completions
        .as_ref()
        .filter(|entry| entry.chat_id == source_chat_id)
        .map(|entry| entry.completions.clone())
        .unwrap_or_default();
    if chat_id == source_chat_id {
        snapshot.task_boards = loaded
            .metadata
            .task_boards
            .as_ref()
            .filter(|entry| entry.chat_id == chat_id)
            .map(|entry| entry.boards.clone())
            .unwrap_or_default();
        for board in snapshot
            .task_boards
            .values_mut()
            .filter(|_| waits == WaitHosting::Process)
        {
            for task in &mut board.tasks {
                if task.status == tickloop_agent_protocol::SharedTaskStatus::InProgress {
                    task.status = tickloop_agent_protocol::SharedTaskStatus::NeedsAttention;
                    task.result = Some(
                        "Worker restored; reconcile the prior execution before explicit release."
                            .into(),
                    );
                    task.revision += 1;
                    board.revision += 1;
                }
            }
        }
    }
    snapshot.branch_origin = restored_branch_origin(loaded, source_chat_id);
    snapshot.delegation = loaded
        .metadata
        .delegation
        .as_ref()
        .filter(|entry| entry.chat_id == source_chat_id)
        .map(|entry| entry.delegation.clone());
    if let Some(entry) = loaded
        .metadata
        .delegation
        .as_ref()
        .filter(|entry| entry.chat_id == source_chat_id)
    {
        snapshot.search_config = entry.search_config.clone();
    }
    snapshot.artifact_ticks = loaded
        .metadata
        .artifact_ticks
        .as_ref()
        .map(|entry| entry.artifact_ticks.clone())
        .unwrap_or_default();
    snapshot.removed_artifact_ticks = loaded
        .metadata
        .artifact_ticks
        .as_ref()
        .map(|entry| entry.removed_artifact_ticks.clone())
        .unwrap_or_default();
    // Monitor membership belongs to the Chat that recorded it; a copy under a new Chat id is a
    // new owner and keeps only the removals.
    snapshot.monitor = loaded
        .metadata
        .monitor_members
        .as_ref()
        .filter(|entry| entry.chat_id == source_chat_id)
        .map(|entry| {
            if chat_id == source_chat_id {
                entry.monitor.clone()
            } else {
                entry.monitor.for_new_chat()
            }
        })
        .unwrap_or_default();
    let mut bookkeeping = ChatBookkeeping {
        lifecycle: super::lifecycle::Lifecycle::restore(
            &loaded.lifecycle,
            chat_id == source_chat_id,
        ),
        provenance: loaded
            .metadata
            .provenance
            .clone()
            .filter(|entry| entry.chat_id == source_chat_id)
            .unwrap_or_default(),
        ..ChatBookkeeping::default()
    };
    bookkeeping.lifecycle.remember_compacted_leaf(
        loaded
            .chain
            .iter()
            .rev()
            .find_map(|entry| entry.compact_metadata.as_ref()?.display_leaf),
    );
    kept.fill_conversation(&mut snapshot, &mut bookkeeping);
    recover_snapshot_for_resume(
        snapshot,
        bookkeeping,
        kept.meta_messages.clone(),
        waits,
        family,
    )
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
    fn fill_conversation(&self, snapshot: &mut ChatSnapshot, bookkeeping: &mut ChatBookkeeping) {
        snapshot.messages = self.messages.clone();
        snapshot.invoked_skills = self.invoked_skills.clone();
        snapshot.message_runs = self.message_runs.clone();
        bookkeeping.message_responses = self.message_responses.clone();
        snapshot.tool_call_recoveries = self
            .tool_call_recoveries
            .values()
            .flatten()
            .cloned()
            .collect();
        snapshot.message_contexts = self.message_contexts.clone();
        snapshot.message_ids = self.message_ids.clone();
        snapshot.agent_message_origins = self.agent_message_origins.clone();
        snapshot.human_family_origins = self.human_family_origins.clone();
        snapshot.original_message_ids = self.original_message_ids.clone();
        snapshot.prompt_execution_modes = self.prompt_execution_modes.clone();
        snapshot.inherited_prompts = self.inherited_prompts.clone();
        snapshot.message_usage = self.message_usage.clone();
        snapshot.message_sources = self.message_sources.clone();
        snapshot.message_models = self.message_models.clone();
        snapshot.message_compactions = self.message_compactions.clone();
        snapshot.host_notes = self.host_notes.clone();
        snapshot.announced_host_tools = snapshot.visible_announced_host_tools();
        bookkeeping.run_loops = self.run_loops.clone();
        bookkeeping.plan_slug = self.plan_slug.clone();
        self.replay_loops(snapshot, bookkeeping);
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
                approval_pending: record.plan_approval_pending,
                progress: record
                    .progress
                    .clone()
                    .and_then(|todos| serde_json::from_value::<Vec<TodoItem>>(todos).ok()),
            })
            .collect()
    }

    /// 沿对话链按顺序重放 Loop 条目：每条以完整状态覆盖同一 Loop，创建位置取第一条的位置。
    /// 无法解析的条目跳过。
    fn replay_loops(&self, snapshot: &mut ChatSnapshot, bookkeeping: &mut ChatBookkeeping) {
        snapshot.loops.clear();
        bookkeeping.loop_starts.clear();
        snapshot.resume_triggers.clear();
        snapshot.failure_events.clear();
        for (position, (record, run_id)) in &self.loop_records {
            if let (Some(run), Some(report)) = (run_id, &record.subagent_report) {
                snapshot.subagent_reports.insert(*run, report.clone());
            }
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
            for value in &record.failure_events {
                if let Ok(mut event) = serde_json::from_value::<Event>(value.clone())
                    && event.loop_id == record.loop_id
                    && matches!(event.kind, EventKind::Failed(_))
                    && !snapshot.failure_events.iter().any(|previous| {
                        previous.run_id == event.run_id && previous.sequence == event.sequence
                    })
                {
                    event.chat_id = snapshot.id;
                    snapshot.last_sequence = snapshot.last_sequence.max(event.sequence);
                    snapshot.events.push(event.clone());
                    snapshot.failure_events.push(event);
                }
            }
            let goal = LoopSnapshot {
                id: record.loop_id,
                goal: record.goal.clone(),
                state,
                run_id: *run_id,
                run_started_at: record.run_started_at,
                trigger,
                event_sources: record
                    .event_sources
                    .iter()
                    .filter_map(|source| serde_json::from_value(source.clone()).ok())
                    .collect(),
                state_since: record
                    .state_since
                    .as_ref()
                    .and_then(serde_json::Value::as_u64),
            };
            match snapshot
                .loops
                .iter_mut()
                .find(|existing| existing.id == goal.id)
            {
                Some(existing) => *existing = goal,
                None => {
                    bookkeeping.loop_starts.insert(goal.id, *position);
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
/// Under durable wait hosting, a suspended Run that is rebuilt keeps its Loop, its batch and the
/// end of the conversation as they were.
fn recover_snapshot_for_resume(
    mut snapshot: ChatSnapshot,
    bookkeeping: ChatBookkeeping,
    mut meta_messages: HashSet<usize>,
    waits: WaitHosting,
    family: Option<&super::family_load::FamilyRecords>,
) -> Result<RecoveredSnapshot, AgentError> {
    let rebuilt_runs = |snapshot: &ChatSnapshot| {
        super::suspension::rebuilt_waits(waits, &bookkeeping.lifecycle, snapshot)
            .into_iter()
            .map(|wait| wait.run_id)
            .collect::<HashSet<_>>()
    };
    let rebuilt = rebuilt_runs(&snapshot);
    let rebuilt_loops = snapshot
        .loops
        .iter()
        .filter(|goal| goal.run_id.is_some_and(|run| rebuilt.contains(&run)))
        .map(|goal| goal.id)
        .collect::<HashSet<_>>();
    super::triggers::recover_loops_keeping(&mut snapshot, &rebuilt_loops, waits);
    // A kept Loop whose deadline passed meanwhile expired: its Run is not rebuilt after all.
    let rebuilt = rebuilt_runs(&snapshot);
    // A family call's effect is in the family's records: its result is what they say.
    let message_responses = bookkeeping.message_responses.clone();
    let reconcile = |snapshot: &ChatSnapshot, call: &projection::HistoryCall| {
        family.and_then(|family| family.reconcile(snapshot, &message_responses, call))
    };
    super::recovery::close_dangling_tool_calls_except(&mut snapshot, &rebuilt, &reconcile)?;
    let ends_with_rebuilt_run = (0..snapshot.messages.len())
        .rev()
        .find(|index| snapshot.messages[*index].role != Role::System)
        .and_then(|index| snapshot.message_runs.get(&index))
        .is_some_and(|run| rebuilt.contains(run));
    if !ends_with_rebuilt_run {
        append_interruption_messages(&mut snapshot, &mut meta_messages);
    }
    super::branch::validate_snapshot(&snapshot)?;
    super::shared_tasks::validate_restored_boards(&snapshot).map_err(AgentError::Invalid)?;
    projection::projected_messages(&snapshot, &bookkeeping.denial_references)?;
    Ok(RecoveredSnapshot {
        snapshot,
        bookkeeping,
        meta_messages,
    })
}

/// Remove unanswered tool-only turns and blank text; keep private blocks in the archive.
/// Target projection decides whether those blocks can be sent to the selected model. A call a
/// durable wait is on counts as answered: its Run goes on there.
fn unrecoverable_assistant_messages(
    messages: &[Message],
    waited_calls: &HashSet<String>,
) -> BTreeSet<usize> {
    let resolved = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .chain(waited_calls.iter().map(String::as_str))
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
        && message
            .content
            .iter()
            .all(|content| matches!(content, Content::Text { text } if text.trim().is_empty()))
}

/// The assistant sentinel recovery appends after a conversation that ends with a user message.
pub(super) fn is_recovery_sentinel(message: &Message) -> bool {
    message.role == Role::Assistant
        && matches!(message.content.as_slice(), [Content::Text { text }] if text == NO_RESPONSE_REQUESTED)
}

fn no_store_to_switch() -> AgentError {
    AgentError::Invalid("this Chat keeps no Transcript to select another branch from".into())
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
    pub(super) async fn adopt_upgrade_transcript(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
    ) -> Result<(), AgentError> {
        let Some(recorder) = &self.inner.recorder else {
            return Ok(());
        };
        let loaded = self.load_saved_transcript(snapshot.id).await?;
        let (restored, _) = restore_chain(&loaded.chain);
        let checkpoints = restored.plan_checkpoints();
        recorder.adopt(snapshot.id, restored.links, restored.meta_messages, loaded);
        recorder.adopt_restored_loops(snapshot, bookkeeping, &checkpoints);
        recorder.adopt_lifecycle(snapshot.id, &bookkeeping.lifecycle);
        Ok(())
    }

    pub(super) async fn checkpoint_transcript(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
    ) -> Result<(), String> {
        let Some(recorder) = &self.inner.recorder else {
            return Ok(());
        };
        if !recorder.logs.lock().unwrap().contains_key(&snapshot.id) {
            return Ok(());
        }
        recorder.enqueue_sync(snapshot, bookkeeping);
        if !recorder.outbox.is_materialized(snapshot.id)
            || recorder.outbox.refusal(snapshot.id).is_some()
        {
            return Err("Transcript is unavailable for a connection checkpoint".into());
        }
        let _appends = recorder.appends.lock().await;
        recorder
            .append_queued(snapshot.id, true)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Under durable wait hosting, every Line of the Chats so far is committed now, their first
    /// ones included, before the caller records what depends on them; another instance that
    /// loads the family then finds them. A process host records nothing ahead of its
    /// conversation.
    pub(super) async fn make_durable(&self, chat_ids: &[ChatId]) -> Result<(), String> {
        if self.inner.waits != WaitHosting::Durable {
            return Ok(());
        }
        let Some(recorder) = &self.inner.recorder else {
            return Ok(());
        };
        {
            let chats = self.inner.chats.lock().await;
            for chat_id in chat_ids {
                let Some(chat) = chats.get(chat_id) else {
                    continue;
                };
                recorder.outbox.materialize(*chat_id);
                recorder.enqueue_sync(&chat.snapshot, &chat.bookkeeping);
            }
        }
        let _appends = recorder.appends.lock().await;
        for chat_id in chat_ids {
            recorder
                .append_queued(*chat_id, true)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// The durability barrier: every Line produced so far is committed, or the error that
    /// stopped it. Gives each recorded Chat's committed position. A local store is best effort
    /// otherwise; this is where its failures surface.
    pub async fn sync(&self) -> Result<BTreeMap<ChatId, StorePosition>, AgentError> {
        let Some(recorder) = &self.inner.recorder else {
            return Ok(BTreeMap::new());
        };
        {
            let chats = self.inner.chats.lock().await;
            recorder.take_dirty();
            for chat in chats.values() {
                recorder.enqueue_sync(&chat.snapshot, &chat.bookkeeping);
            }
        }
        let _appends = recorder.appends.lock().await;
        let mut positions = BTreeMap::new();
        let mut first_error = None;
        for chat in recorder.outbox.chats() {
            let durable = recorder.outbox.is_materialized(chat);
            match recorder.append_queued(chat, durable).await {
                Ok(position) => {
                    positions.insert(chat, position);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error.into()),
            None => Ok(positions),
        }
    }

    /// Every Line the store keeps of a recorded Chat.
    pub(super) async fn stored_lines(&self, chat_id: ChatId) -> Option<Vec<Line>> {
        self.inner.recorder.as_ref()?.stored_lines(chat_id).await
    }

    /// Every Line the store keeps of the Chat, read in full.
    pub(super) async fn read_stored_lines(&self, chat_id: ChatId) -> Result<Vec<Line>, AgentError> {
        let recorder = self
            .inner
            .recorder
            .as_ref()
            .ok_or_else(no_store_to_switch)?;
        let stored = recorder
            .load_stored(chat_id, LoadRange::Full)
            .await?
            .ok_or(AgentError::NotFound)?;
        Ok(stored.retained.into_iter().chain(stored.lines).collect())
    }

    /// A newer holder owns this Harness's Chats: nothing more reaches their store.
    pub(super) fn transcripts_fenced(&self) -> bool {
        self.inner
            .recorder
            .as_ref()
            .is_some_and(|recorder| recorder.outbox.is_fenced())
    }

    pub(super) fn record_artifact_ticks(
        &self,
        chat_id: ChatId,
        folder: Option<&Path>,
        artifact_ticks: Vec<tickloop_agent_protocol::ArtifactTick>,
        removed_artifact_ticks: std::collections::BTreeSet<String>,
    ) {
        self.record_chat_metadata(
            chat_id,
            Line::ArtifactTicks(ArtifactTicksEntry {
                chat_id,
                cwd: folder.map(Path::to_path_buf),
                artifact_ticks,
                removed_artifact_ticks,
            }),
        );
    }

    pub(super) fn record_monitor_membership(
        &self,
        chat_id: ChatId,
        folder: Option<&Path>,
        monitor: MonitorMembership,
    ) {
        self.record_chat_metadata(
            chat_id,
            Line::MonitorMembers(MonitorMembersEntry {
                chat_id,
                cwd: folder.map(Path::to_path_buf),
                monitor,
            }),
        );
    }

    /// Queues one Chat-level metadata line and wakes the background flush, so the change reaches
    /// disk while a long-lived service keeps the Chat open.
    pub(super) fn record_chat_metadata(&self, chat_id: ChatId, line: Line) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        if !recorder.logs.lock().unwrap().contains_key(&chat_id) {
            return;
        }
        recorder.outbox.enqueue(chat_id, vec![line], None);
        recorder.notify.notify_one();
        self.ensure_flush_loop(recorder);
    }

    pub(super) fn record_delegation(
        &self,
        child_id: ChatId,
        mut delegation: DelegationMetadata,
        search_config: Option<tickloop_agent_protocol::SearchConfig>,
    ) {
        // A record never carries the host's rules: every load fills in the ones it then has.
        delegation.ceiling.policy.host_rules.clear();
        if let Some(recorder) = &self.inner.recorder {
            recorder.outbox.enqueue(
                child_id,
                vec![Line::Delegation(DelegationEntry {
                    chat_id: child_id,
                    delegation,
                    search_config,
                })],
                None,
            );
        }
    }

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
        let flush_now = recorder.flush_now.clone();
        let closed = recorder.closed.clone();
        self.inner.tasks.spawn_on(
            async move {
                loop {
                    tokio::select! {
                        _ = closed.cancelled() => break,
                        _ = notify.notified() => {}
                    }
                    tokio::select! {
                        _ = closed.cancelled() => break,
                        _ = flush_now.notified() => {}
                        _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
                    }
                    let Some(inner) = host.upgrade() else {
                        break;
                    };
                    Agent { inner }.flush_dirty_transcripts().await;
                }
            },
            &runtime,
        );
    }

    /// Ends the background flush loop; whatever is saved later is written when the last handle
    /// drops.
    pub(super) fn stop_transcript_flushing(&self) {
        if let Some(recorder) = &self.inner.recorder {
            recorder.closed.cancel();
        }
    }

    async fn flush_dirty_transcripts(&self) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        {
            let chats = self.inner.chats.lock().await;
            for chat_id in recorder.take_dirty() {
                if let Some(chat) = chats.get(&chat_id) {
                    recorder.enqueue_sync(&chat.snapshot, &chat.bookkeeping);
                }
            }
        }
        recorder.flush().await;
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
                recorder.enqueue_sync(&chat.snapshot, &chat.bookkeeping);
            }
        }
        recorder.flush().await;
        self.flush_denial_histories().await;
    }

    /// Where the display node `display_id` ends on the Chat's current physical chain; `None` when
    /// the chain holds no such node or the Chat is not recorded.
    pub async fn physical_endpoint(
        &self,
        chat_id: ChatId,
        display_id: Uuid,
    ) -> Result<Option<Uuid>, AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        Ok(self.inner.recorder.as_ref().and_then(|recorder| {
            recorder.physical_endpoint(&chat.snapshot, &chat.bookkeeping, display_id)
        }))
    }

    /// `save` without waiting for the flush interval: a started answer's record reaches the
    /// store promptly, and a publisher holds the answer's deltas until it is committed.
    pub(super) fn save_now(&self, snapshot: &ChatSnapshot) {
        self.save(snapshot);
        if let Some(recorder) = &self.inner.recorder {
            recorder.flush_now.notify_one();
        }
    }

    /// 退出时照 Claude Code：先冲刷，再把元数据重新追加到每个 Transcript 末尾。
    pub(super) async fn close_transcripts(&self) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        self.flush_transcripts().await;
        for chat in recorder.outbox.chats() {
            if recorder.outbox.is_materialized(chat) {
                recorder.queue_metadata_again(chat).await;
            }
        }
        recorder.flush().await;
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
            .and_then(|recorder| recorder.outbox.unresolved_error())
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
    /// 文件重新读取；Tickloop 的去重集合由写入器随写入增量维护，与文件内容一致，无需重建。
    pub(super) async fn reappend_transcript_metadata(&self, chat_id: ChatId) {
        let Some(recorder) = &self.inner.recorder else {
            return;
        };
        if let Some(chat) = self.inner.chats.lock().await.get(&chat_id) {
            recorder.enqueue_sync(&chat.snapshot, &chat.bookkeeping);
        }
        recorder.flush().await;
        if recorder.queue_metadata_again(chat_id).await {
            recorder.flush().await;
        }
    }

    /// 列出工作目录所属项目中已保存的 Chat，按最近活动时间从新到旧排列。
    pub async fn saved_chats(&self, folder: &Path) -> Result<Vec<SavedChat>, AgentError> {
        let Some(local) = self
            .inner
            .recorder
            .as_ref()
            .and_then(ChatRecorder::local_store)
        else {
            return Ok(Vec::new());
        };
        self.flush_transcripts().await;
        let folder = folder.to_path_buf();
        let summaries = tokio::task::spawn_blocking(move || local.project_chats(&folder))
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
        // A durable host holds a family whole: every member comes in with the Chat.
        if self.inner.waits == WaitHosting::Durable {
            Box::pin(self.resume_family(chat_id)).await?;
            super::family_tree::lineage(&*self.inner.chats.lock().await, chat_id)
                .map_err(AgentError::Invalid)?;
            return self.snapshot(chat_id).await;
        }
        {
            let chats = self.inner.chats.lock().await;
            if let Some(chat) = chats.get(&chat_id) {
                super::family_tree::lineage(&chats, chat_id).map_err(AgentError::Invalid)?;
                return Ok(chat.snapshot.clone());
            }
        }
        let Some(recorder) = &self.inner.recorder else {
            return Err(AgentError::NotFound);
        };
        let _resume = recorder.resume_lock.lock().await;
        {
            let chats = self.inner.chats.lock().await;
            if let Some(chat) = chats.get(&chat_id) {
                super::family_tree::lineage(&chats, chat_id).map_err(AgentError::Invalid)?;
                return Ok(chat.snapshot.clone());
            }
        }
        let mut pending = Vec::new();
        let mut visited = HashSet::new();
        let mut current = chat_id;
        loop {
            if !visited.insert(current)
                || visited.len() > tickloop_agent_protocol::MAX_SUBAGENT_DEPTH + 1
            {
                return Err(AgentError::Invalid(
                    "invalid or excessive delegation ancestry".into(),
                ));
            }
            if self.inner.chats.lock().await.contains_key(&current) {
                break;
            }
            let loaded = self.load_saved_transcript(current).await?;
            let parent = loaded
                .metadata
                .delegation
                .as_ref()
                .map(|entry| entry.delegation.parent_chat_id);
            pending.push((current, loaded));
            let Some(parent) = parent else { break };
            current = parent;
        }
        let mut lineages = HashMap::new();
        {
            let chats = self.inner.chats.lock().await;
            if chats.contains_key(&current) {
                lineages.insert(
                    current,
                    super::family_tree::lineage(&chats, current).map_err(AgentError::Invalid)?,
                );
            }
        }
        for (id, loaded) in pending.iter().rev() {
            let lineage = match &loaded.metadata.delegation {
                Some(entry) => {
                    let delegation = &entry.delegation;
                    let mut lineage = lineages
                        .get(&delegation.parent_chat_id)
                        .cloned()
                        .ok_or_else(|| {
                            AgentError::Invalid("delegated parent is unavailable".into())
                        })?;
                    lineage.push(tickloop_agent_protocol::DelegationParent {
                        chat_id: delegation.parent_chat_id,
                        loop_id: delegation.parent_loop_id,
                    });
                    if lineage.len() > tickloop_agent_protocol::MAX_SUBAGENT_DEPTH
                        || lineage != delegation.lineage()
                    {
                        return Err(AgentError::Invalid(
                            "persisted delegation ancestry is inconsistent".into(),
                        ));
                    }
                    lineage
                }
                None => Vec::new(),
            };
            lineages.insert(*id, lineage);
        }
        for (id, loaded) in pending.into_iter().rev() {
            // Boxed: restoring a Chat is a large future, and resuming awaits it in a loop.
            Box::pin(self.insert_restored_chat(id, loaded, RestoreTarget::Resume, None)).await?;
        }
        super::family_tree::lineage(&*self.inner.chats.lock().await, chat_id)
            .map_err(AgentError::Invalid)?;
        self.snapshot(chat_id).await
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
        let loaded = self.load_saved_transcript(source_chat_id).await?;
        if loaded.metadata.delegation.is_some() {
            return Err(AgentError::Invalid("Unsupported: a delegated Transcript cannot be copied into an unrestricted new Agent".into()));
        }
        let new_chat_id = loop {
            let candidate = Uuid::new_v4();
            if !self.inner.chats.lock().await.contains_key(&candidate) {
                break candidate;
            }
        };
        Box::pin(self.insert_restored_chat(
            new_chat_id,
            loaded,
            RestoreTarget::NewChat {
                source: source_chat_id,
            },
            None,
        ))
        .await
    }

    pub(super) async fn load_saved_transcript(
        &self,
        chat_id: ChatId,
    ) -> Result<LoadedTranscript, AgentError> {
        let recorder = self.inner.recorder.as_ref().ok_or(AgentError::NotFound)?;
        recorder.load_chat(chat_id, ChatLoad::default()).await
    }

    /// The conversation a loaded chain restores, before it joins this instance: what resuming a
    /// Chat and selecting another of its branches share.
    fn restore_conversation(
        &self,
        chat_id: ChatId,
        loaded: &mut LoadedTranscript,
        source_chat_id: ChatId,
        waits: WaitHosting,
        family: Option<&super::family_load::FamilyRecords>,
    ) -> Result<RestoredConversation, AgentError> {
        let (restored, execution_mode_warnings) = restore_chain(&loaded.chain);
        loaded.warnings.extend(execution_mode_warnings);
        let mut waited_calls = super::suspension::waited_calls(waits, &loaded.lifecycle);
        // A family call's effect may be in the family's records: its batch stays, and the load
        // settles the call by them.
        if family.is_some() {
            waited_calls.extend(super::family_load::family_call_ids(&restored.messages));
        }
        let removed = unrecoverable_assistant_messages(&restored.messages, &waited_calls);
        let mut recovered = recover_transcript(
            chat_id,
            loaded,
            &restored,
            &removed,
            source_chat_id,
            waits,
            family,
        )?;
        let folder = recovered.snapshot.folder.clone();
        match (&folder, &self.inner.environment) {
            (Some(folder), Some(_)) => self.require_working_directory_trust(folder)?,
            (None, None) => {}
            (Some(_), None) => return Err(AgentError::Invalid(super::NO_ENVIRONMENT.into())),
            (None, Some(_)) => {
                return Err(AgentError::Invalid(
                    "a restored Chat without a Folder needs a Harness without an environment"
                        .into(),
                ));
            }
        }
        let plans_directory = folder
            .as_deref()
            .map(|folder| self.plans_directory(folder).path().to_path_buf());
        let plan_checkpoints = restored.without(&removed).plan_checkpoints();
        let plan_warnings = super::plan::rebuild_plans_and_progress(
            &mut recovered.snapshot,
            &mut recovered.bookkeeping,
            &plan_checkpoints,
            plans_directory.as_deref(),
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
        let mut bookkeeping = recovered.bookkeeping;
        bookkeeping.restored_skill_names = recovered
            .snapshot
            .invoked_skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
        Ok(RestoredConversation {
            snapshot: recovered.snapshot,
            bookkeeping,
            links: restored.without(&removed).links,
            meta_messages: recovered.meta_messages,
            plan_checkpoints,
            plans_directory,
            folder,
            content_replacements,
            replacement_records,
        })
    }

    /// Restores the stored chain that ends at the physical entry `leaf` beside the Chat in
    /// memory, which stays in place until [`StagedBranch::install`]. Nothing is written.
    pub(super) async fn stage_branch(
        &self,
        chat_id: ChatId,
        leaf: Uuid,
    ) -> Result<StagedBranch, AgentError> {
        let recorder = self
            .inner
            .recorder
            .as_ref()
            .ok_or_else(no_store_to_switch)?;
        let mut loaded = recorder
            .load_chat(chat_id, ChatLoad { leaf: Some(leaf) })
            .await?;
        let RestoredConversation {
            snapshot,
            mut bookkeeping,
            links,
            meta_messages,
            plan_checkpoints,
            folder,
            content_replacements,
            ..
        } = self.restore_conversation(chat_id, &mut loaded, chat_id, WaitHosting::Process, None)?;
        let file_history = folder.as_deref().map(|folder| {
            FileHistory::restore(
                self.file_history_directory(chat_id, folder),
                file_history_snapshots(&loaded),
            )
        });
        let (recorded_loops, created_loops) =
            recorded_loop_entries(&snapshot, &bookkeeping, &plan_checkpoints);
        let recorded_lifecycle = bookkeeping.lifecycle.revision();
        bookkeeping.lifecycle.settle_after_load(
            &snapshot,
            &bookkeeping.message_responses.clone(),
            &HashSet::new(),
        );
        let parent_cursor = adopted_parent_cursor(&links, &loaded);
        Ok(StagedBranch {
            snapshot,
            bookkeeping,
            content_replacements,
            file_history,
            leaf,
            log: StagedLog {
                links,
                meta_messages,
                parent_cursor,
                load_warnings: loaded.warnings,
                uuids: loaded.uuids,
                recorded_loops,
                created_loops,
                recorded_lifecycle,
                retained: loaded.retained,
            },
        })
    }

    /// Restores a loaded Chat into this instance. A family member restored with the records of
    /// its whole family settles its family calls by them, and leaves going on with its work to
    /// the family's restore, once every member is in.
    pub(super) async fn insert_restored_chat(
        &self,
        chat_id: ChatId,
        loaded: LoadedTranscript,
        target: RestoreTarget,
        family: Option<&super::family_load::FamilyRecords>,
    ) -> Result<ChatSnapshot, AgentError> {
        let recorder = self.inner.recorder.as_ref().ok_or(AgentError::NotFound)?;
        // 替换决定、文件回滚备份与元数据都属于写入这份 Transcript 的 Chat：恢复时是自己，以新 chatId 恢复时是源 Chat。
        let source_chat_id = match &target {
            RestoreTarget::Resume => chat_id,
            RestoreTarget::NewChat { source } => *source,
        };
        let mut loaded = loaded;
        // A copy under a new Chat id goes on with none of the source's Runs.
        let waits = match &target {
            RestoreTarget::Resume => self.inner.waits,
            RestoreTarget::NewChat { .. } => WaitHosting::Process,
        };
        let registrations = if chat_id == source_chat_id {
            loaded
                .metadata
                .subagents
                .as_ref()
                .filter(|entry| entry.chat_id == chat_id)
                .map(|entry| entry.subagents.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let RestoredConversation {
            mut snapshot,
            mut bookkeeping,
            links,
            meta_messages,
            plan_checkpoints,
            plans_directory,
            mut folder,
            content_replacements,
            replacement_records,
        } = self.restore_conversation(chat_id, &mut loaded, source_chat_id, waits, family)?;
        // 与设置写入互斥到 Chat 加入本进程为止，期间写入的持久规则改动不会漏掉这个 Chat。
        let _settings_gate = self.inner.settings_writes.lock().await;
        let (client, output_style) = self.apply_chat_defaults(&mut snapshot, &mut bookkeeping);
        if let Some(delegation) = &mut snapshot.delegation {
            self.apply_host_rules(&mut delegation.ceiling.policy);
            folder = delegation.ceiling.folder.clone();
            let ceiling = delegation.ceiling.clone();
            snapshot.folder = folder.clone();
            snapshot.additional_directories = ceiling.additional_directories;
            snapshot.policy = ceiling.policy;
        }
        if snapshot.plans.iter().any(|plan| plan.approval_pending) {
            snapshot.policy.mode = ExecutionMode::Plan;
            for goal in &mut snapshot.loops {
                if !matches!(goal.state, LoopState::Stopped | LoopState::Expired)
                    && snapshot
                        .plans
                        .iter()
                        .any(|plan| plan.loop_id == goal.id && plan.approval_pending)
                {
                    goal.recover_state(LoopState::WaitingPlan);
                }
            }
        }
        let mut file_history = folder.as_deref().map(|folder| {
            FileHistory::restore(
                self.file_history_directory(source_chat_id, folder),
                file_history_snapshots(&loaded),
            )
        });
        let mut denial_window = DenialWindow::default();
        match target {
            RestoreTarget::Resume => {
                recorder.adopt(chat_id, links, meta_messages, loaded);
                recorder.adopt_restored_loops(&snapshot, &bookkeeping, &plan_checkpoints);
                recorder.adopt_lifecycle(chat_id, &bookkeeping.lifecycle);
                denial_window = recorder.load_denials(chat_id);
            }
            RestoreTarget::NewChat { .. } => {
                for goal in &mut snapshot.loops {
                    goal.event_sources.clear();
                }
                // 照 Claude Code `copyPlanForFork`：以新 chatId 恢复时使用新的 slug 并复制计划文件。
                super::plan::copy_plans_to_new_slug(
                    &mut snapshot,
                    &mut bookkeeping,
                    plans_directory.as_deref(),
                );
                recorder
                    .register_resumed_copy(
                        chat_id,
                        folder.as_deref(),
                        meta_messages,
                        loaded.metadata,
                        loaded.warnings,
                    )
                    .await?;
                // 重写的对话以压缩边界开头；替换决定与文件回滚快照写在对话之后，新文件超过 5MB
                // 时加载不会随边界之前的内容跳过它们。
                recorder.enqueue_sync(&snapshot, &bookkeeping);
                // 照 Claude Code `processResumedConversation`：以新 chatId 重新记录源 Chat
                // 的全部替换决定；替换文本仍指向源 Chat 目录中的外存文件。
                if !replacement_records.is_empty() {
                    recorder.enqueue_content_replacements(chat_id, replacement_records);
                }
                // 照 Claude Code `copyFileHistoryForResume`：备份硬链接到新 Chat 的目录，
                // 复制成功的快照以新 chatId 重新记录。
                if let (Some(history), Some(folder)) = (file_history.take(), folder.as_deref()) {
                    let (copied, recorded) =
                        history.copy_to(self.file_history_directory(chat_id, folder));
                    file_history = Some(copied);
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
        }
        let rebuilt = super::suspension::rebuilt_waits(waits, &bookkeeping.lifecycle, &snapshot)
            .into_iter()
            .map(|wait| wait.response_id)
            .collect();
        bookkeeping.lifecycle.settle_after_load(
            &snapshot,
            &bookkeeping.message_responses.clone(),
            &rebuilt,
        );
        self.checkpoint_transcript(&snapshot, &bookkeeping)
            .await
            .map_err(AgentError::Storage)?;
        let tools = self.create_chat_tools(&snapshot);
        let skills = self.discover_skills(folder.as_deref());
        self.inner
            .subagent_registry
            .lock()
            .await
            .insert(chat_id, registrations);
        self.inner.chats.lock().await.insert(
            chat_id,
            Chat {
                family_generation: 0,
                family_stopping: false,
                task_checkpoint_pending: false,
                family_finishing: HashSet::new(),
                snapshot: snapshot.clone(),
                bookkeeping,
                start_new_tool_result_message: false,
                needs_plan_mode_exit_notice: false,
                active: None,
                active_run: None,
                upgrade_wait: None,
                parked: HashMap::new(),
                releases: HashMap::new(),
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
                event_cancellations: HashMap::new(),
                feedback_inputs: HashMap::new(),
                feedback_cleanup_started: false,
                deferred_messages: Vec::new(),
                user_cancelled_runs: HashSet::new(),
                output_style,
                auto_denials: super::auto_mode::DenialTracking::default(),
                denial_window,
                branch_reservation: None,
            },
        );
        if let Some(context) = snapshot
            .delegation
            .as_ref()
            .and_then(|delegation| delegation.ceiling.host_context.clone())
        {
            self.set_host_work_context(chat_id, context).await?;
        }
        self.save(&snapshot);
        self.start_feedback_cleanup(chat_id).await;
        if family.is_none() {
            self.restore_after_load(chat_id, waits).await;
        }
        Ok(snapshot)
    }
}

struct RestoredConversation {
    snapshot: ChatSnapshot,
    bookkeeping: ChatBookkeeping,
    links: Vec<MessageLink>,
    meta_messages: HashSet<usize>,
    plan_checkpoints: Vec<super::plan::PlanCheckpoint>,
    plans_directory: Option<PathBuf>,
    folder: Option<PathBuf>,
    content_replacements: super::tool_results::ContentReplacementState,
    replacement_records: Vec<ContentReplacementRecord>,
}

/// Another stored branch of a Chat in memory, restored and waiting to replace its conversation.
pub(super) struct StagedBranch {
    snapshot: ChatSnapshot,
    bookkeeping: ChatBookkeeping,
    content_replacements: super::tool_results::ContentReplacementState,
    file_history: Option<FileHistory>,
    /// The physical entry the branch ends at.
    leaf: Uuid,
    log: StagedLog,
}

/// The recorder's view of a staged branch: what is already stored of it.
pub(super) struct StagedLog {
    links: Vec<MessageLink>,
    meta_messages: HashSet<usize>,
    parent_cursor: Option<Uuid>,
    load_warnings: Vec<String>,
    uuids: HashSet<Uuid>,
    recorded_loops: HashMap<LoopId, LoopEntryContent>,
    created_loops: HashSet<LoopId>,
    recorded_lifecycle: u64,
    retained: super::transcript::RetainedFold,
}

impl StagedBranch {
    /// The Chat on the staged branch: its conversation and what derives from it come from the
    /// store; the instance's own state (tools, model, permissions, settings, events, operations
    /// and shells) stays the Chat's in memory. Nothing is installed yet.
    pub(super) fn into_chat(self, current: &Chat) -> (Chat, StagedLog, Uuid) {
        let mut chat = current.clone();
        let mut snapshot = self.snapshot;
        let previous = &current.snapshot;
        snapshot.model = previous.model.clone();
        snapshot.reasoning_effort = previous.reasoning_effort;
        snapshot.thinking_enabled = previous.thinking_enabled;
        snapshot.policy = previous.policy.clone();
        snapshot.search_config = previous.search_config.clone();
        snapshot.task_tools_enabled = previous.task_tools_enabled;
        if previous.generated_title.is_some() {
            snapshot.generated_title = previous.generated_title.clone();
        }
        snapshot.events = previous.events.clone();
        snapshot.failure_events = previous.failure_events.clone();
        snapshot.last_sequence = snapshot.last_sequence.max(previous.last_sequence);
        snapshot.last_queue_sequence = snapshot
            .last_queue_sequence
            .max(previous.last_queue_sequence);
        snapshot.operations = previous.operations.clone();
        snapshot.request_usage = previous.request_usage.clone();
        snapshot.provider_failures = previous.provider_failures.clone();
        snapshot.user_shells = previous.user_shells.clone();
        snapshot.pending_commands = previous.pending_commands.clone();
        if snapshot.plans.iter().any(|plan| plan.approval_pending) {
            snapshot.policy.mode = ExecutionMode::Plan;
        }
        chat.snapshot = snapshot;
        chat.bookkeeping = self.bookkeeping;
        chat.content_replacements = self.content_replacements;
        chat.file_history = self.file_history;
        chat.needs_plan_mode_exit_notice = false;
        chat.start_new_tool_result_message = false;
        chat.branch_reservation = None;
        chat.active = None;
        (chat, self.log, self.leaf)
    }
}

pub(super) enum RestoreTarget {
    /// 继续写入原 Transcript。
    Resume,
    /// 写入新 chatId 的 Transcript。
    NewChat { source: ChatId },
}

#[cfg(test)]
#[path = "transcript_replay_tests.rs"]
mod transcript_replay_tests;

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn recovery_keeps_private_only_history_for_source_projection() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![Content::Thinking {
                    thinking: "readable".into(),
                    signature: Some("signed".into()),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::ProtocolData {
                    protocol: tickloop_agent_protocol::Protocol::Responses,
                    value: serde_json::json!({"type":"reasoning","encrypted_content":"opaque"}),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::Text { text: " ".into() }],
            },
        ];
        assert_eq!(
            unrecoverable_assistant_messages(&messages, &HashSet::new()),
            BTreeSet::from([2])
        );
    }
}

#[cfg(test)]
mod golden_tests;
