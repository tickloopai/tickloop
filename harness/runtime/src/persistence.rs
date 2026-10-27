//! Adapter layer between the runtime and the Transcript.
//!
//! `Agent::save` only marks the Chat dirty; a background flush task takes the committed snapshot at
//! most every 100ms, diffs it against what was already written, and appends the new chain entries
//! to `TranscriptStore`. Chat snapshots are not saved whole: on resume the Transcript is loaded by
//! id, the Chat is rebuilt from the conversation chain and metadata, and Claude Code's post-resume
//! processing runs.

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
use tickloop_harness_base::private_fs::unix_time_milliseconds;
use tickloop_harness_base::timestamp::format_timestamp;
use tickloop_harness_protocol::projection;
use tickloop_harness_protocol::{
    AgentMessageSource, ApprovalRecord, BranchOrigin, ChatId, ChatSnapshot, CompactBoundary,
    CompactTrigger, CompactionMarker, Content, DelegationMetadata, DenialRecord,
    DirectedHumanSource, Event, EventKind, ExecutionMode, FamilyCompletion, FamilyInputRecord,
    HostNote, InputContext, LocalMessage, LoopId, LoopSnapshot, LoopState, MAX_RECENT_DENIALS,
    Message, ModelConfig, MonitorMembership, PrivateHistorySource, Receiving, ResponseId, Role,
    RunId, SavedChat, SubagentRegistration, SummarizeMetadata, SummaryDirection, TickSubscription,
    TodoItem, ToolCallRecovery, Usage,
};
use tickloop_harness_transcript::{
    AiTitleEntry, ArtifactTicksEntry, BranchOriginEntry, COMPACT_BOUNDARY_SUBTYPE, ChainEntry,
    CompactMetadata, CompactTriggerEntry, ContentReplacementEntry, ContentReplacementRecord,
    CustomTitleEntry, DelegationEntry, FamilyCompletionsEntry, FamilyInputsEntry,
    FileHistorySnapshotEntry, ForkedFrom, LOOP_SUBTYPE, LastPromptEntry, Line, LoopRecord,
    MonitorMembersEntry, PartialCompactDirection, ProvenanceEntry, SentAgentMessagesEntry,
    SubagentsEntry, SummarizeMetadataEntry,
};

/// Following Claude Code: when the conversation ends with a tool result on resume, this meta user
/// message is appended to continue from where it was interrupted.
const CONTINUE_FROM_INTERRUPTION: &str = "Continue from where you left off.";
/// Following Claude Code's `NO_RESPONSE_REQUESTED`: the assistant sentinel appended when the
/// conversation ends with a user message.
const NO_RESPONSE_REQUESTED: &str = "No response requested.";
/// Name used when a Branch restored from a Transcript has no title.
const UNTITLED_BRANCH_NAME: &str = "Branch";

pub(super) enum CitationCommitError {
    Association(TranscriptError),
    ResultNode(TranscriptError),
}

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
    /// Written entries corresponding one-to-one with `snapshot.messages`.
    links: Vec<MessageLink>,
    meta_messages: HashSet<usize>,
    /// Parent entry of the next new message. Usually the last written entry; after compaction it is
    /// the summary message, following Claude Code, and preserved messages are not rewritten and do
    /// not move this cursor.
    parent_cursor: Option<Uuid>,
    /// Problems found while loading that the user needs to know about.
    load_warnings: Vec<String>,
    /// Time of the last batch of chain entries this process wrote to the file. Later batch times
    /// strictly increase, so entries continued after a rewind become the newer leaf even when
    /// written in the same millisecond as the abandoned fork.
    last_entry_milliseconds: u64,
    /// Full Loop state last recorded on the current conversation chain; a Loop entry is appended
    /// when the Loop in the snapshot differs from it.
    recorded_loop: Option<LoopEntryContent>,
    /// The lifecycle and ledger revision written last.
    recorded_lifecycle: u64,
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
                tickloop_harness_base::private_fs::create_private_directory(directory)
                    .map_err(|error| error.to_string())?;
                tickloop_harness_base::private_fs::replace_file_atomically(
                    &path,
                    &bytes,
                    tickloop_harness_base::private_fs::PRIVATE_FILE_MODE,
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

/// Contents recorded by a Loop entry: the full state and the run id.
type LoopEntryContent = (LoopRecord, Option<RunId>);

/// Subsequent entries attach after the last entry on the chain, including trailing Loop entries
/// reattached after the leaf (such as a stop written after a reply turn ends); attaching after the
/// last message would detach those Loop entries from the new chain and revive a stopped Loop.
/// Messages filtered out on resume are not used as parents.
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

/// The Loop record a restored chain already holds, with the plan state actually recorded; `None`
/// when the chain holds none, so the next sync records the Loop.
fn recorded_loop_entry(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    checkpoints: &[super::plan::PlanCheckpoint],
    records_loop: bool,
) -> Option<LoopEntryContent> {
    if !records_loop {
        return None;
    }
    let recorded_plan = checkpoints.iter().rev().find_map(|checkpoint| {
        checkpoint
            .plan_version
            .map(|version| (version, checkpoint.approval_pending))
    });
    let (mut record, run) = loop_entry_content(snapshot, bookkeeping);
    record.plan_version = recorded_plan.map(|(version, _)| version);
    record.plan_approval_pending = recorded_plan.is_some_and(|(_, pending)| pending);
    Some((record, run))
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
            vec![Line::Subagents(SubagentsEntry {
                chat_id: chat_id.into(),
                subagents,
            })],
            None,
        );
    }

    pub(super) fn record_provenance(&self, chat_id: ChatId, entry: ProvenanceEntry) {
        self.outbox
            .enqueue(chat_id, vec![Line::Provenance(entry)], None);
    }

    pub(super) async fn commit_citation_call(
        &self,
        current: &Chat,
        candidate: &Chat,
    ) -> Result<(), CitationCommitError> {
        self.enqueue_sync(&current.snapshot, &current.bookkeeping);
        let _appends = self.appends.lock().await;
        self.append_queued(current.snapshot.id, true)
            .await
            .map_err(CitationCommitError::Association)?;
        self.enqueue_sync(&candidate.snapshot, &candidate.bookkeeping);
        self.append_queued(candidate.snapshot.id, true)
            .await
            .map_err(CitationCommitError::ResultNode)?;
        Ok(())
    }

    pub(super) fn citation_links(&self, chat: ChatId) -> HashMap<Uuid, Vec<Uuid>> {
        self.logs
            .lock()
            .unwrap()
            .get(&chat)
            .into_iter()
            .flat_map(|log| &log.links)
            .filter_map(|link| Some((*link.uuids.first()?, link.uuids.clone())))
            .collect()
    }

    pub(super) fn record_citation_sources(
        &self,
        entry: tickloop_harness_transcript::CitationSourcesEntry,
    ) {
        self.outbox.enqueue(
            entry.chat_id.into(),
            vec![Line::CitationSources(entry)],
            None,
        );
    }

    pub(super) fn record_sent_agent_messages(
        &self,
        chat_id: ChatId,
        messages: BTreeMap<Uuid, AgentMessageSource>,
    ) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::SentAgentMessages(SentAgentMessagesEntry {
                chat_id: chat_id.into(),
                messages,
            })],
            None,
        );
    }

    pub(super) fn record_family_message_hold(&self, chat: &Chat, stopped_runs: HashSet<RunId>) {
        // Sync advances the lifecycle revision baseline with the records queued alongside the
        // hold. An append already in progress may drain this group before the next dirty flush.
        let hold = Line::FamilyMessageHold(tickloop_harness_transcript::FamilyMessageHoldEntry {
            chat_id: chat.snapshot.id.into(),
            stopped_runs: stopped_runs.into_iter().map(Into::into).collect(),
        });
        self.enqueue_with_sync(&chat.snapshot, &chat.bookkeeping, vec![hold], Vec::new());
    }

    pub(super) fn record_family_inputs(
        &self,
        chat_id: ChatId,
        inputs: BTreeMap<Uuid, FamilyInputRecord>,
    ) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::FamilyInputs(FamilyInputsEntry {
                chat_id: chat_id.into(),
                inputs,
            })],
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
                chat_id: chat_id.into(),
                completions,
            })],
            None,
        );
    }

    pub(super) fn record_ai_title(&self, chat_id: ChatId, title: String) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::AiTitle(AiTitleEntry {
                chat_id: chat_id.into(),
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
        let contents = match tickloop_harness_base::private_fs::open_without_following_links(
            &path,
            tickloop_harness_base::private_fs::OpenTarget::Entry,
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
                recorded_loop: None,
                recorded_lifecycle: 0,
            },
        );
        self.outbox.register(chat_id, position);
        Ok(true)
    }

    /// A Chat resumed under a new chatId (`--fork-session`) is written to a new Transcript in the
    /// project of its working directory: loaded messages are rewritten as new entries, the title is
    /// carried to the new Transcript with the metadata following Claude Code, and the Branch origin
    /// is re-recorded under the new chatId.
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
                chat_id: chat_id.into(),
                custom_title: entry.custom_title,
            })
        });
        let branch_origin = metadata.branch_origin.map(|entry| {
            Line::BranchOrigin(BranchOriginEntry {
                chat_id: chat_id.into(),
                forked_from: entry.forked_from,
            })
        });
        self.outbox.enqueue(
            chat_id,
            custom_title
                .into_iter()
                .chain(branch_origin)
                .chain(metadata.citation_sources.map(|entry| {
                    Line::CitationSources(tickloop_harness_transcript::CitationSourcesEntry {
                        chat_id: chat_id.into(),
                        ..entry
                    })
                }))
                .chain(metadata.artifact_ticks.map(|entry| {
                    Line::ArtifactTicks(ArtifactTicksEntry {
                        chat_id: chat_id.into(),
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
                                chat_id: chat_id.into(),
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

    /// Queues the Branch origin and title into the child Chat's Transcript; the child Chat is
    /// placed alongside its origin.
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
                chat_id: child_chat_id.into(),
                forked_from: ForkedFrom {
                    chat_id: source.id.into(),
                    message_uuid: leaf,
                },
            }),
            Line::CustomTitle(CustomTitleEntry {
                chat_id: child_chat_id.into(),
                custom_title: title.to_owned(),
            }),
        ];
        if !source_bookkeeping.citation_sources.sources.is_empty() {
            lines.push(Line::CitationSources(
                tickloop_harness_transcript::CitationSourcesEntry {
                    chat_id: child_chat_id.into(),
                    ..source_bookkeeping.citation_sources.clone()
                },
            ));
        }
        let monitor = source.monitor.for_new_chat();
        if !monitor.is_empty() {
            lines.push(Line::MonitorMembers(MonitorMembersEntry {
                chat_id: child_chat_id.into(),
                cwd: source.folder.clone(),
                monitor,
            }));
        }
        if !source.artifact_ticks.is_empty() || !source.removed_artifact_ticks.is_empty() {
            lines.push(Line::ArtifactTicks(ArtifactTicksEntry {
                chat_id: child_chat_id.into(),
                cwd: source.folder.clone(),
                artifact_ticks: source.artifact_ticks.clone(),
                removed_artifact_ticks: source.removed_artifact_ticks.clone(),
            }));
        }
        self.outbox.enqueue(child_chat_id, lines, None);
        Ok(())
    }

    /// Following Claude Code's `recordContentReplacement`: replacement decisions are appended to
    /// the Chat's Transcript as metadata entries.
    fn enqueue_content_replacements(
        &self,
        chat_id: ChatId,
        replacements: Vec<ContentReplacementRecord>,
    ) {
        self.outbox.enqueue(
            chat_id,
            vec![Line::ContentReplacement(ContentReplacementEntry {
                chat_id: chat_id.into(),
                replacements,
            })],
            None,
        );
    }

    /// Called before a rewind truncates the conversation: first writes out messages not yet written
    /// before truncation; abandoned messages stay in the file, following Claude Code. Subsequent
    /// entries attach after the last kept message, forming a fork in the same file.
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
            // Loop entries after the truncation point are not on the new conversation chain; the
            // next sync re-records the full Loop state.
            log.recorded_loop = None;
        }
        self.mark_dirty(chat_id);
    }

    /// Called when compaction or a directed summary commits, before replacing the in-memory
    /// snapshot. First writes out messages not yet written before compaction, then appends the new
    /// boundary (null parent pointer, logical parent pointing at the last pre-compaction message)
    /// and the summary, and re-records the Loop state; these entries are queued as one batch so the
    /// Loop, plan and progress are not lost when a large file skips content before the boundary.
    ///
    /// Full compaction follows Claude Code's `recordTranscript`: preserved messages keep their
    /// original entries and the next message attaches after the summary. Directed summaries deviate
    /// from Claude Code and do not write preserved segments: after the boundary, the conversation's
    /// messages are rewritten as new entries in a straight line in memory order (uuids taken from
    /// the renewed message ids in `compacted`), and file-history snapshots in `renamed_snapshots`
    /// whose ids changed with their prompts are rewritten under the new ids; the summarized old
    /// entries stay in the file as an abandoned branch, and the next message attaches after the
    /// last message of the in-memory conversation.
    ///
    /// Content replacement decisions for tool results still in the conversation after the boundary
    /// are re-recorded in the same batch: loading a Transcript over 5MB skips records before the
    /// boundary, otherwise replaced results in preserved messages would be sent as original text.
    pub(super) fn compact(
        &self,
        chat: &Chat,
        compacted: &ChatSnapshot,
        compacted_bookkeeping: &ChatBookkeeping,
        layout: &CompactionLayout,
        renamed_snapshots: &[FileHistorySnapshot],
        source_states: Vec<Line>,
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
        if compacted_bookkeeping.citation_sources != chat.bookkeeping.citation_sources {
            lines.push(Line::CitationSources(
                compacted_bookkeeping.citation_sources.clone(),
            ));
        }
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
                chat_id: compacted.id.into(),
                replacements,
            }));
        }
        self.outbox
            .enqueue_group(compacted.id, lines, source_states, events_through);
    }

    /// Chain entries that must be newly written for the compacted conversation, while switching the
    /// written-entry mapping to the compacted conversation. Returns `None` without changing
    /// anything when the written entries do not match the pre-compaction conversation.
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
        // The compaction boundary breaks the conversation chain, so Loop entries before the
        // boundary are no longer on the chain: re-record the full Loop state. Full compaction
        // writes it after the summary; a directed summary writes it after the preserved messages.
        let loop_position = if keeps_existing {
            layout.sources.len()
        } else {
            layout
                .position(MessageSource::Boundary)
                .map_or(0, |index| index + 1)
        };
        let mut loop_update = Some(loop_entry_content(compacted, compacted_bookkeeping));
        log.recorded_loop = None;
        for (index, source) in layout.sources.iter().enumerate() {
            if let MessageSource::Existing(original) = source
                && keeps_existing
            {
                let link = old_links[*original].clone();
                parent = link.uuids.last().copied();
                log.links.push(link);
                continue;
            }
            if index >= loop_position
                && let Some(content) = loop_update.take()
            {
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
                .unwrap_or_else(tickloop_ids::generate);
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
        if let Some(content) = loop_update {
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
        // Plan and progress tool calls before the boundary are no longer on the chain: record the
        // plan version and progress at that point after all messages.
        if let Some(content) = plan_entry_content(compacted, compacted_bookkeeping) {
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

    /// Following Claude Code's `recordFileHistorySnapshot`: snapshots are appended to the Chat's
    /// Transcript as metadata entries.
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
        self.enqueue_with_sync(snapshot, bookkeeping, Vec::new(), Vec::new());
    }

    /// Queues what the Chat changed since its last sync, then `lines` and the Trigger source
    /// records `source_states`, as one group the store takes whole.
    fn enqueue_with_sync(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
        lines: Vec<Line>,
        source_states: Vec<Line>,
    ) {
        let Some(mut synced) = self.sync_chat(snapshot, bookkeeping) else {
            return;
        };
        synced.extend(lines);
        self.outbox.set_display_leaf(
            snapshot.id,
            bookkeeping
                .lifecycle
                .display_leaf(snapshot, &bookkeeping.message_responses),
        );
        self.outbox.enqueue_group(
            snapshot.id,
            synced,
            source_states,
            Some(snapshot.last_sequence),
        );
    }

    /// Basis for the diff: the runtime modifies messages in only three ways: appending a new
    /// message; appending content blocks to the last message (parallel tool results), written as a
    /// new entry marked as continuing that message; and rewind truncating the conversation, synced
    /// to the written entries beforehand by `truncate`.
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
        // Write the Loop when it differs from the chain record. A Loop not yet recorded in the
        // current chain segment is written before the first new message of this batch; changes to
        // an already recorded Loop are written at the end of the batch. A rewind truncation
        // re-records the Loop state.
        let loop_content = loop_entry_content(snapshot, bookkeeping);
        let first_records_loop = log.recorded_loop.is_none();
        let mut loop_update = (log.recorded_loop.as_ref() != Some(&loop_content)).then(|| {
            let position = if first_records_loop {
                first_new_message
            } else {
                snapshot.messages.len()
            };
            (position, loop_content)
        });
        if let Some(index) = log.links.len().checked_sub(1) {
            let message = &snapshot.messages[index];
            let blocks = log.links[index].blocks;
            if message.content.len() > blocks {
                let appended = Message {
                    role: message.role,
                    content: message.content[blocks..].to_vec(),
                };
                // Attach after the last written entry: there may already be Loop entries after the
                // message.
                let mut entry = chain_entry(
                    snapshot,
                    bookkeeping,
                    index,
                    tickloop_ids::generate(),
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
            if let Some((_, content)) = loop_update.take_if(|(position, _)| *position <= index) {
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
                .unwrap_or_else(tickloop_ids::generate);
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
                    chat_id: snapshot.id.into(),
                    last_prompt: prompt,
                }));
            }
        }
        if let Some((_, content)) = loop_update {
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
        // First write of the Loop in the current chain segment: the chain lacks the plan and
        // progress tool calls before it (Branch, `--fork-session`, rewind truncation), so record
        // the plan version and progress at that point at the end of this batch.
        if first_records_loop && let Some(content) = plan_entry_content(snapshot, bookkeeping) {
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
        if !lines.is_empty() {
            log.last_entry_milliseconds = milliseconds;
            log.parent_cursor = parent;
        }
        lines.extend(bookkeeping.lifecycle.changed_after(log.recorded_lifecycle));
        log.recorded_lifecycle = bookkeeping.lifecycle.revision();
        Some(lines)
    }

    /// Takes over the existing file after resume: loaded messages keep their entries in the file,
    /// and subsequent entries attach after the leaf.
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
                recorded_loop: None,
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
            response_of(*index) == Some(display_id.into())
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
                recorded_loop: log.recorded_loop,
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
    fn adopt_restored_loop(
        &self,
        snapshot: &ChatSnapshot,
        bookkeeping: &ChatBookkeeping,
        checkpoints: &[super::plan::PlanCheckpoint],
        records_loop: bool,
    ) {
        if let Some(log) = self.logs.lock().unwrap().get_mut(&snapshot.id) {
            log.recorded_loop =
                recorded_loop_entry(snapshot, bookkeeping, checkpoints, records_loop);
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
        // Following Claude Code's `insertMessageChain`: a compaction boundary has a null parent
        // pointer, so the conversation chain breaks at the boundary. Old boundaries preserved by a
        // later directed summary attach after the previous message, keeping the chain straight.
        parent_uuid: if boundary.is_some_and(|boundary| !boundary.preserved_by_later_summary) {
            None
        } else {
            parent
        },
        logical_parent_uuid: None,
        is_sidechain: false,
        chat_id: snapshot.id.into(),
        timestamp: stamp.timestamp.to_owned(),
        cwd: stamp.folder.map(Path::to_path_buf),
        version: env!("CARGO_PKG_VERSION").into(),
        git_branch: None,
        message,
        invoked_skills: snapshot.invoked_skills.clone(),
        user_skill_invocation: run_id
            .and_then(|run| snapshot.user_skill_invocations.get(&run))
            .cloned(),
        is_meta: snapshot.host_notes.contains_key(&index),
        host_tools: snapshot
            .host_notes
            .get(&index)
            .and_then(HostNote::announced_host_tools)
            .cloned(),
        plan_mode_exit: snapshot.host_notes.get(&index) == Some(&HostNote::PlanModeExit),
        date_change: match snapshot.host_notes.get(&index) {
            Some(HostNote::DateChange(date)) => Some(date.clone()),
            _ => None,
        },
        guidance: match snapshot.host_notes.get(&index) {
            Some(HostNote::Guidance(note)) => Some(note.clone()),
            _ => None,
        },
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
        run_id: run_id.map(Into::into),
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
            .contains_key(&index)
            .then_some(snapshot.chat_loop.id.into()),
        slug: bookkeeping.plan_slug.clone(),
        response_id: bookkeeping
            .lifecycle
            .response_of(snapshot, &bookkeeping.message_responses, index)
            .map(Into::into),
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

/// Full Loop state on the conversation chain. Triggers waiting for the Chat to continue are
/// recorded too, so the user can still re-arm them after a restart.
fn loop_entry_content(snapshot: &ChatSnapshot, bookkeeping: &ChatBookkeeping) -> LoopEntryContent {
    let chat_loop = &snapshot.chat_loop;
    (
        LoopRecord {
            subagent_report: chat_loop
                .run_id
                .and_then(|run| snapshot.subagent_reports.get(&run).cloned()),
            failure_events: snapshot
                .failure_events
                .iter()
                .filter_map(|event| serde_json::to_value(event).ok())
                .collect(),
            loop_id: chat_loop.id.into(),
            state: serde_json::to_value(&chat_loop.state).unwrap_or(serde_json::Value::Null),
            // Receiving is the default a record without the field reads as.
            receiving: (!chat_loop.receiving.is_receiving())
                .then(|| serde_json::to_value(chat_loop.receiving).ok())
                .flatten(),
            subscriptions: chat_loop
                .subscriptions
                .iter()
                .filter_map(|subscription| serde_json::to_value(subscription).ok())
                .collect(),
            event_sources: chat_loop
                .event_sources
                .iter()
                .filter_map(|source| serde_json::to_value(source).ok())
                .collect(),
            plan_version: super::plan::highest_plan_version(snapshot, bookkeeping),
            plan_approval_pending: snapshot.plans.iter().any(|plan| plan.approval_pending),
            progress: None,
            state_since: chat_loop.state_since.map(serde_json::Value::from),
            run_started_at: chat_loop.run_started_at,
        },
        chat_loop.run_id,
    )
}

/// Loop state with plan version and progress; returns `None` when the Loop has neither a plan nor
/// progress.
fn plan_entry_content(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
) -> Option<LoopEntryContent> {
    let plan_version = super::plan::highest_plan_version(snapshot, bookkeeping);
    let progress = snapshot
        .progress
        .first()
        .and_then(|progress| serde_json::to_value(&progress.todos).ok());
    if plan_version.is_none() && progress.is_none() {
        return None;
    }
    let (mut record, run_id) = loop_entry_content(snapshot, bookkeeping);
    record.plan_version = plan_version;
    record.progress = progress;
    Some((record, run_id))
}

/// Writes the Loop state as a system entry attached after `parent` and records it as the latest
/// state on the chain.
fn record_loop(
    log: &mut ChatLog,
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    (record, run_id): LoopEntryContent,
    parent: &mut Option<Uuid>,
    stamp: EntryStamp<'_>,
) -> Line {
    let entry = ChainEntry {
        uuid: tickloop_ids::generate(),
        parent_uuid: *parent,
        logical_parent_uuid: None,
        is_sidechain: false,
        chat_id: snapshot.id.into(),
        timestamp: stamp.timestamp.to_owned(),
        cwd: stamp.folder.map(Path::to_path_buf),
        version: env!("CARGO_PKG_VERSION").into(),
        git_branch: None,
        message: Message {
            role: Role::System,
            content: Vec::new(),
        },
        invoked_skills: snapshot.invoked_skills.clone(),
        user_skill_invocation: run_id
            .and_then(|run| snapshot.user_skill_invocations.get(&run))
            .cloned(),
        is_meta: false,
        host_tools: None,
        plan_mode_exit: false,
        date_change: None,
        guidance: None,
        execution_mode: None,
        is_inherited_prompt: false,
        original_message_id: None,
        agent_message_origin: None,
        human_family_origin: None,
        run_id: run_id.map(Into::into),
        tool_call_recoveries: Vec::new(),
        contexts: None,
        continues_message: false,
        subtype: Some(LOOP_SUBTYPE.to_owned()),
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
    log.recorded_loop = Some((record.without_progress(), run_id));
    Line::System(entry)
}

/// Tickloop does not write `preservedSegment` (see `ChatRecorder::compact`); reading still supports
/// files with segments.
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
            // A boundary with a parent entry on the chain is not the start of the conversation
            // chain: either a preserved boundary rewritten by Tickloop, or a boundary after Claude
            // Code's preserved-segment relink.
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

/// A plain-text prompt typed by the user; local messages (interrupt markers, `!` commands, task
/// notifications) do not count.
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

/// Searches upward from the working directory for `.git/HEAD`; a `.git` file in a worktree is
/// followed via `gitdir:`. Does not spawn a git process.
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
    invoked_skills: Vec<tickloop_harness_protocol::InvokedSkill>,
    user_skill_invocations: BTreeMap<RunId, tickloop_harness_protocol::UserSkillInvocation>,
    message_runs: BTreeMap<usize, RunId>,
    message_responses: BTreeMap<usize, ResponseId>,
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
    message_models: BTreeMap<usize, tickloop_harness_protocol::ModelIdentity>,
    links: Vec<MessageLink>,
    meta_messages: HashSet<usize>,
    message_of_entry: HashMap<Uuid, usize>,
    /// Current-format Loop entries on the chain, in chain order, with the number of messages before
    /// each entry.
    loop_records: Vec<(usize, LoopEntryContent)>,
    /// The Loop owning the run recorded by the entry.
    run_loops: BTreeMap<RunId, LoopId>,
    /// Following Claude Code's `getSlugFromLog`: the first entry on the chain with a slug. When the
    /// first slug on the chain is invalid (possibly tampered with), take the first valid slug after
    /// it (a new slug switched to on resume is written on later entries); when none is valid, keep
    /// the first one, and rebuilding the plan warns and switches to a new slug.
    plan_slug: Option<String>,
}

/// Restores chain entries into messages and preserves user prompts with execution modes as rewind
/// points.
fn restore_chain(chain: &[ChainEntry]) -> (RestoredChain, Vec<String>) {
    let mut restored = RestoredChain {
        messages: Vec::new(),
        invoked_skills: Vec::new(),
        user_skill_invocations: BTreeMap::new(),
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
        if let (Some(run), Some(skill)) = (entry.run_id, &entry.user_skill_invocation) {
            restored
                .user_skill_invocations
                .insert(run.into(), skill.clone());
        }
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
            restored.run_loops.insert(run_id.into(), loop_id.into());
        }
        if entry.is_loop_record() {
            // Records of a Chat with several Loops are an earlier format, dropped on load.
            if let Some(record) = entry
                .loop_record
                .clone()
                .filter(|_| entry.is_current_loop_record())
            {
                restored.loop_records.push((
                    restored.messages.len(),
                    (record, entry.run_id.map(Into::into)),
                ));
            }
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
            restored.message_runs.insert(index, run_id.into());
        }
        if let Some(response) = entry.response_id {
            restored.message_responses.insert(index, response.into());
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
        } else if let Some(date) = &entry.date_change {
            restored
                .host_notes
                .insert(index, HostNote::DateChange(date.clone()));
        } else if let Some(note) = &entry.guidance {
            restored
                .host_notes
                .insert(index, HostNote::Guidance(note.clone()));
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

pub(super) fn citations_in_chain(
    chain: &[ChainEntry],
    calls: &[tickloop_harness_transcript::CitationCall],
    sources: &[tickloop_harness_protocol::CitationSource],
) -> crate::VisibleCitations {
    let (restored, _) = restore_chain(chain);
    let messages = restored
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let ids = restored.links[index]
                .uuids
                .iter()
                .copied()
                .chain(restored.original_message_ids.get(&index).copied())
                .collect();
            crate::citations::CitationMessage {
                ids,
                content: &message.content,
            }
        })
        .collect::<Vec<_>>();
    crate::citations::visible_in_messages(calls, sources, &messages)
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
    /// Removes the messages at the given indices, shifting the remaining messages forward.
    fn without(&self, removed: &BTreeSet<usize>) -> Self {
        let mut kept = Self {
            messages: Vec::new(),
            invoked_skills: self.invoked_skills.clone(),
            user_skill_invocations: self.user_skill_invocations.clone(),
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

/// Rebuilds the Chat from the conversation chain and metadata: the working directory is the `cwd`
/// of the last entry on the chain, and the Branch origin comes from `branch-origin` and the title
/// metadata. `removed` lists the messages this resume deleted following Claude Code;
/// `scheduled_now` is the clock used by Scheduled Tasks.
#[allow(clippy::too_many_arguments)]
fn recover_transcript(
    chat_id: ChatId,
    loaded: &LoadedTranscript,
    restored: &RestoredChain,
    removed: &BTreeSet<usize>,
    source_chat_id: ChatId,
    waits: WaitHosting,
    family: Option<&super::family_load::FamilyRecords>,
    scheduled_now: chrono::DateTime<chrono::Utc>,
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
            })
            .or_else(|| loaded.goal.as_ref().and_then(|entry| entry.cwd.clone()))
            .or_else(|| {
                loaded
                    .scheduled_tasks
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
        snapshot.goal = loaded
            .goal
            .as_ref()
            .filter(|entry| entry.chat_id == chat_id)
            .and_then(|entry| entry.goal.clone());
        let scheduled = loaded
            .scheduled_tasks
            .as_ref()
            .filter(|entry| entry.chat_id == chat_id);
        snapshot.scheduled_tasks = scheduled
            .map(|entry| entry.tasks.clone())
            .unwrap_or_default();
        snapshot.scheduled_task_revocations = scheduled
            .map(|entry| entry.revoked.clone())
            .unwrap_or_default();
        // A Worker loading the Chat from its history keeps only the recurring tasks still
        // running and the one-shot tasks still ahead; a durable host's load is how it runs the
        // Chat, so it keeps them all.
        if waits == WaitHosting::Process {
            super::scheduled_tasks::after_history_load(&mut snapshot, scheduled_now);
        }
        snapshot.task_boards = loaded
            .metadata
            .task_boards
            .as_ref()
            .filter(|entry| entry.chat_id == chat_id)
            .map(|entry| {
                entry
                    .boards
                    .iter()
                    .map(|(loop_id, board)| (LoopId::from(*loop_id), board.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for board in snapshot
            .task_boards
            .values_mut()
            .filter(|_| waits == WaitHosting::Process)
        {
            for task in &mut board.tasks {
                if task.status == tickloop_harness_protocol::SharedTaskStatus::InProgress {
                    task.status = tickloop_harness_protocol::SharedTaskStatus::NeedsAttention;
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
        continued_message_ids: restored
            .links
            .iter()
            .filter(|link| link.uuids.len() > 1)
            .filter_map(|link| Some((*link.uuids.first()?, link.uuids.clone())))
            .collect(),
        citation_sources: loaded
            .metadata
            .citation_sources
            .clone()
            .filter(|entry| entry.chat_id == source_chat_id)
            .map(|entry| tickloop_harness_transcript::CitationSourcesEntry {
                chat_id: chat_id.into(),
                ..entry
            })
            .unwrap_or_default(),
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
    let mut nested = std::collections::BTreeMap::new();
    for line in &loaded.lifecycle {
        if let Line::NestedCall(entry) = line
            && entry.chat_id == source_chat_id
            && super::nested_calls::parent_on_chain(&snapshot, &entry.operation)
        {
            let mut operation = entry.operation.clone();
            if matches!(
                operation.state,
                tickloop_harness_protocol::OperationState::Proposed
                    | tickloop_harness_protocol::OperationState::Admitted
            ) {
                let admitted =
                    operation.state == tickloop_harness_protocol::OperationState::Admitted;
                operation.state = if admitted {
                    tickloop_harness_protocol::OperationState::Unknown
                } else {
                    tickloop_harness_protocol::OperationState::Failed
                };
                operation.result = Some(tickloop_harness_protocol::ToolResult {content:if admitted {"Nested call interrupted after admission; outcome unknown. Do not retry automatically."} else {"Nested call interrupted before admission; not executed."}.into(),is_error:true,output_ref:None});
            }
            nested.insert(operation.id, operation);
        }
    }
    snapshot.operations.extend(nested.into_values());

    recover_snapshot_for_resume(
        snapshot,
        bookkeeping,
        kept.meta_messages.clone(),
        waits,
        family,
    )
}

/// Branch origin metadata belongs to the Chat that wrote this Transcript; the name is the title, or
/// the default name when there is no valid title.
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
        source_chat_id: entry.forked_from.chat_id.into(),
        name,
        creation: None,
    })
}

impl RestoredChain {
    fn fill_conversation(&self, snapshot: &mut ChatSnapshot, bookkeeping: &mut ChatBookkeeping) {
        snapshot.messages = self.messages.clone();
        snapshot.invoked_skills = self.invoked_skills.clone();
        snapshot.user_skill_invocations = self.user_skill_invocations.clone();
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
        bookkeeping.plan_slug = self.plan_slug.clone();
        self.replay_loop(snapshot);
        bookkeeping.run_loops = self
            .run_loops
            .keys()
            .map(|run| (*run, snapshot.chat_loop.id))
            .collect();
    }

    /// Whether the chain holds a record of the Loop in the current format.
    fn records_loop(&self) -> bool {
        !self.loop_records.is_empty()
    }

    /// Plan version and progress carried by Loop entries on the chain.
    fn plan_checkpoints(&self) -> Vec<super::plan::PlanCheckpoint> {
        self.loop_records
            .iter()
            .filter(|(_, (record, _))| record.plan_version.is_some() || record.progress.is_some())
            .map(|(position, (record, _))| super::plan::PlanCheckpoint {
                position: *position,
                plan_version: record.plan_version,
                approval_pending: record.plan_approval_pending,
                progress: record
                    .progress
                    .clone()
                    .and_then(|todos| serde_json::from_value::<Vec<TodoItem>>(todos).ok()),
            })
            .collect()
    }

    /// Replays Loop entries in order along the conversation chain: each entry overwrites the Loop
    /// with its full state. Unparseable entries are skipped; with no entries the new Chat's Loop is
    /// kept.
    fn replay_loop(&self, snapshot: &mut ChatSnapshot) {
        snapshot.failure_events.clear();
        for (_, (record, run_id)) in &self.loop_records {
            if let (Some(run), Some(report)) = (run_id, &record.subagent_report) {
                snapshot.subagent_reports.insert(*run, report.clone());
            }
            let Ok(state) = serde_json::from_value::<LoopState>(record.state.clone()) else {
                continue;
            };
            let Ok(receiving) = record
                .receiving
                .clone()
                .map(serde_json::from_value::<Receiving>)
                .transpose()
            else {
                continue;
            };
            let Ok(subscriptions) = record
                .subscriptions
                .iter()
                .cloned()
                .map(serde_json::from_value::<TickSubscription>)
                .collect::<Result<Vec<_>, _>>()
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
            snapshot.chat_loop = LoopSnapshot {
                id: record.loop_id.into(),
                state,
                run_id: *run_id,
                run_started_at: record.run_started_at,
                receiving: receiving.unwrap_or_default(),
                subscriptions,
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
            .map(|wait| RunId::from(wait.run_id))
            .collect::<HashSet<_>>()
    };
    let rebuilt = rebuilt_runs(&snapshot);
    let keeps_run = snapshot
        .chat_loop
        .run_id
        .is_some_and(|run| rebuilt.contains(&run));
    super::wakeup::recover_loop(&mut snapshot, keeps_run, waits);
    let rebuilt = rebuilt_runs(&snapshot);
    // A family call's effect is in the family's records: its result is what they say.
    let message_responses = bookkeeping.message_responses.clone();
    // The loading Chat's Loop writes ids as a new Loop does.
    let ids = tickloop_ids::IdContext::new();
    let reconcile = |snapshot: &ChatSnapshot, call: &projection::HistoryCall| {
        family.and_then(|family| family.reconcile(&ids, snapshot, &message_responses, call))
    };
    super::recovery::close_dangling_tool_calls_except(&ids, &mut snapshot, &rebuilt, &reconcile)?;
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

/// Following Claude Code's `deserializeMessagesWithInterruptDetection`: when the conversation ends
/// with a tool result, first append the "continue from where it was interrupted" meta message;
/// then, whenever it ends with a user message, append the no-response-requested sentinel. Meta
/// messages do not count toward interrupt detection.
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

/// Following Claude Code's `buildFileHistorySnapshotChain`: collects file-history snapshots along
/// the conversation chain, with update entries replacing older snapshots of the same message;
/// snapshots not on the current chain (abandoned forks) have no effect.
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
        let records_loop = restored.records_loop();
        recorder.adopt(snapshot.id, restored.links, restored.meta_messages, loaded);
        recorder.adopt_restored_loop(snapshot, bookkeeping, &checkpoints, records_loop);
        recorder.adopt_lifecycle(snapshot.id, &bookkeeping.lifecycle);
        Ok(())
    }

    /// Queues for the Chat's Transcript what it changed so far, then the metadata `lines` and
    /// the records `source_states` a Trigger source made of its own state with that change: one
    /// group the store takes whole, so no crash keeps the change without the source's part, or
    /// the source's part without the change.
    pub(super) fn queue_with_source_states(
        &self,
        chat: &Chat,
        lines: Vec<Line>,
        source_states: Vec<Line>,
    ) {
        if let Some(recorder) = &self.inner.recorder
            && recorder
                .logs
                .lock()
                .unwrap()
                .contains_key(&chat.snapshot.id)
        {
            recorder.enqueue_with_sync(&chat.snapshot, &chat.bookkeeping, lines, source_states);
        }
    }

    /// Whether a batch of the Chat's Transcript the store failed to take waits to be appended
    /// again.
    pub(super) fn transcript_holds_failed_batch(&self, chat_id: ChatId) -> bool {
        self.inner
            .recorder
            .as_ref()
            .is_some_and(|recorder| recorder.outbox.holds_failed_batch(chat_id))
    }

    /// Whether the Chat's Transcript keeps what it queued buffered until something keeps the
    /// Chat, such as its first message: there is nothing to commit for it yet.
    pub(super) fn transcript_awaits_materializing(&self, chat_id: ChatId) -> bool {
        self.inner.recorder.as_ref().is_some_and(|recorder| {
            recorder.logs.lock().unwrap().contains_key(&chat_id)
                && !recorder.outbox.is_materialized(chat_id)
        })
    }

    pub(super) fn transcript_materialized(&self, chat_id: ChatId) -> bool {
        self.inner
            .recorder
            .as_ref()
            .is_some_and(|recorder| recorder.outbox.is_materialized(chat_id))
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
        self.commit_queued(snapshot.id).await
    }

    /// Commits what the Chat has queued for its Transcript, without holding the Chats while it
    /// waits for the store: what the caller queued under them stands queued already.
    pub(super) async fn commit_queued(&self, chat_id: ChatId) -> Result<(), String> {
        let Some(recorder) = &self.inner.recorder else {
            return Ok(());
        };
        if !recorder.logs.lock().unwrap().contains_key(&chat_id) {
            return Ok(());
        }
        if !recorder.outbox.is_materialized(chat_id) || recorder.outbox.refusal(chat_id).is_some() {
            return Err("Transcript is unavailable for a connection checkpoint".into());
        }
        let _appends = recorder.appends.lock().await;
        recorder
            .append_queued(chat_id, true)
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
        self.commit_transcripts(chat_ids).await
    }

    pub(super) async fn commit_transcripts(&self, chat_ids: &[ChatId]) -> Result<(), String> {
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
        artifact_ticks: Vec<tickloop_harness_protocol::ArtifactTick>,
        removed_artifact_ticks: std::collections::BTreeSet<String>,
    ) {
        self.record_chat_metadata(
            chat_id,
            Line::ArtifactTicks(ArtifactTicksEntry {
                chat_id: chat_id.into(),
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
                chat_id: chat_id.into(),
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
        search_config: Option<tickloop_harness_protocol::SearchConfig>,
    ) {
        // A record never carries the host's rules: every load fills in the ones it then has.
        delegation.ceiling.policy.host_rules.clear();
        delegation.ceiling.policy.launch_rules.clear();
        if let Some(recorder) = &self.inner.recorder {
            recorder.outbox.enqueue(
                child_id,
                vec![Line::Delegation(DelegationEntry {
                    chat_id: child_id.into(),
                    delegation,
                    search_config,
                })],
                None,
            );
        }
    }

    /// Persistence entry point: only marks the Chat dirty; the background flush task writes the
    /// Transcript.
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

    /// Immediately writes all Chats loaded by this process to their Transcripts. Used when the
    /// process is about to exit directly.
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

    /// On exit, following Claude Code: flush first, then re-append metadata to the end of each
    /// Transcript.
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

    /// Records the content replacement decisions newly made this time. An Agent without persistence
    /// keeps only runtime state.
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

    /// Unresolved Transcript write failure. Write failures do not affect running; failed entries
    /// stay in the queue and every later flush retries them; returns `None` once a retry succeeds.
    pub fn transcript_write_error(&self) -> Option<String> {
        self.inner
            .recorder
            .as_ref()
            .and_then(|recorder| recorder.outbox.unresolved_error())
    }

    /// Problems found while loading this Chat's Transcript (for example, loading the full history
    /// after preserved-segment validation failed).
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

    /// Following Claude Code's post-compaction `reAppendSessionMetadata`: after flushing,
    /// re-appends metadata such as the title to the end of the file so listings that read only the
    /// tail can still find it.
    ///
    /// Claude Code also clears the uuid set cached per sessionId (`clearSessionMessagesCache`) and
    /// rereads it from the file on the next write; Tickloop's dedup set is maintained incrementally
    /// by the writer as it writes and matches the file contents, so no rebuild is needed.
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

    /// Lists saved Chats in the project of the working directory, most recent activity first.
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
                id: summary.chat_id.into(),
                title: summary.title,
                first_prompt: summary.first_prompt,
                last_prompt: summary.last_prompt,
                modified: summary.modified,
                cwd: summary.cwd,
                branch_source: summary.forked_from.map(|origin| origin.chat_id.into()),
                git_branch: summary.git_branch,
            })
            .collect())
    }

    /// Resumes a Chat by id. Returns the instance directly when it is already in this process's
    /// memory; otherwise loads it from the Transcript and runs post-resume processing, without
    /// automatically continuing any work.
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
            if chats.contains_key(&chat_id) {
                super::family_tree::lineage(&chats, chat_id).map_err(AgentError::Invalid)?;
                drop(chats);
                return self.snapshot(chat_id).await;
            }
        }
        let Some(recorder) = &self.inner.recorder else {
            return Err(AgentError::NotFound);
        };
        let _resume = recorder.resume_lock.lock().await;
        {
            let chats = self.inner.chats.lock().await;
            if chats.contains_key(&chat_id) {
                super::family_tree::lineage(&chats, chat_id).map_err(AgentError::Invalid)?;
                drop(chats);
                return self.snapshot(chat_id).await;
            }
        }
        let mut pending = Vec::new();
        let mut visited = HashSet::new();
        let mut current = chat_id;
        loop {
            if !visited.insert(current)
                || visited.len() > tickloop_harness_protocol::MAX_SUBAGENT_DEPTH + 1
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
                    lineage.push(tickloop_harness_protocol::DelegationParent {
                        chat_id: delegation.parent_chat_id,
                        loop_id: delegation.parent_loop_id,
                    });
                    if lineage.len() > tickloop_harness_protocol::MAX_SUBAGENT_DEPTH
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
            self.record_scheduled_tasks_after_history_load(id).await;
        }
        super::family_tree::lineage(&*self.inner.chats.lock().await, chat_id)
            .map_err(AgentError::Invalid)?;
        self.snapshot(chat_id).await
    }

    /// Following Claude Code's `--fork-session`: loads the conversation from the Transcript,
    /// switches to a new chatId, and writes a new Transcript; the original file stays unchanged and
    /// the original Chat does not enter this process.
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
            let candidate = ChatId::new();
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
        // Calls a batch made may have taken effect: their parent stays, and the load reports it
        // as interrupted with each call's outcome.
        waited_calls.extend(loaded.lifecycle.iter().filter_map(|line| match line {
            Line::NestedCall(entry) if entry.chat_id == source_chat_id => {
                entry.operation.parent_call_id.clone()
            }
            _ => None,
        }));
        let removed = unrecoverable_assistant_messages(&restored.messages, &waited_calls);
        let mut recovered = recover_transcript(
            chat_id,
            loaded,
            &restored,
            &removed,
            source_chat_id,
            waits,
            family,
            self.scheduled_now(),
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
            records_loop: restored.records_loop(),
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
            records_loop,
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
        let recorded_loop =
            recorded_loop_entry(&snapshot, &bookkeeping, &plan_checkpoints, records_loop);
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
                recorded_loop,
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
        // Replacement decisions, file-history backups and metadata all belong to the Chat that
        // wrote this Transcript: itself on resume, the source Chat when resuming under a new
        // chatId.
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
        let stopped_runs = (chat_id == source_chat_id).then(|| {
            loaded
                .metadata
                .family_message_hold
                .as_ref()
                .filter(|entry| entry.chat_id == chat_id)
                .map(|entry| entry.stopped_runs.clone())
                .unwrap_or_default()
        });
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
            records_loop,
            links,
            meta_messages,
            plan_checkpoints,
            plans_directory,
            mut folder,
            content_replacements,
            replacement_records,
        } = self.restore_conversation(chat_id, &mut loaded, source_chat_id, waits, family)?;
        // Mutually exclusive with settings writes until the Chat joins this process, so persistent
        // rule changes written meanwhile do not miss this Chat.
        let _settings_gate = self.inner.settings_writes.lock().await;
        let (client, output_style) = self.apply_chat_defaults(&mut snapshot, &mut bookkeeping);
        if let Some(delegation) = &mut snapshot.delegation {
            self.apply_host_rules(&mut delegation.ceiling.policy);
            delegation.ceiling.policy.launch_rules.clear();
            folder = delegation.ceiling.folder.clone();
            let ceiling = delegation.ceiling.clone();
            snapshot.folder = folder.clone();
            snapshot.additional_directories = ceiling.additional_directories;
            snapshot.policy = ceiling.policy;
        }
        if snapshot.plans.iter().any(|plan| plan.approval_pending) {
            snapshot.policy.mode = ExecutionMode::Plan;
            snapshot.chat_loop.recover_state(LoopState::WaitingPlan);
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
                recorder.adopt_restored_loop(
                    &snapshot,
                    &bookkeeping,
                    &plan_checkpoints,
                    records_loop,
                );
                recorder.adopt_lifecycle(chat_id, &bookkeeping.lifecycle);
                denial_window = recorder.load_denials(chat_id);
            }
            RestoreTarget::NewChat { .. } => {
                snapshot.chat_loop.event_sources.clear();
                // Following Claude Code's `copyPlanForFork`: when resuming under a new chatId, use
                // a new slug and copy the plan file.
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
                // The rewritten conversation starts with a compaction boundary; replacement
                // decisions and file-history snapshots are written after the conversation, so when
                // the new file exceeds 5MB, loading does not skip them along with the content
                // before the boundary.
                recorder.enqueue_sync(&snapshot, &bookkeeping);
                // Following Claude Code's `processResumedConversation`: re-record all of the source
                // Chat's replacement decisions under the new chatId; the replacement text still
                // points to the externalized files in the source Chat's directory.
                if !replacement_records.is_empty() {
                    recorder.enqueue_content_replacements(chat_id, replacement_records);
                }
                // Following Claude Code's `copyFileHistoryForResume`: backups are hard-linked into
                // the new Chat's directory, and successfully copied snapshots are re-recorded under
                // the new chatId.
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
            .map(|wait| wait.response_id.into())
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
        if let Some(stopped_runs) = stopped_runs {
            self.inner
                .family_mailbox
                .lock()
                .await
                .stopped_runs
                .insert(chat_id, stopped_runs.into_iter().map(Into::into).collect());
        }
        self.inner.chats.lock().await.insert(
            chat_id,
            Chat {
                ids: tickloop_ids::IdContext::new(),
                family_generation: 0,
                family_stopping: false,
                task_checkpoint_pending: false,
                family_finishing: HashSet::new(),
                snapshot: snapshot.clone(),
                bookkeeping,
                start_new_tool_result_message: false,
                needs_plan_mode_exit_notice: false,
                mode_before_plan: None,
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
                feedback_watcher: None,
                feedback_inputs: HashMap::new(),
                feedback_cleanup_started: false,
                user_loops: HashMap::new(),
                deferred_messages: Vec::new(),
                user_cancelled_runs: HashSet::new(),
                output_style,
                user_context: Default::default(),
                environment: Default::default(),
                announced_date: None,
                auto_denials: super::auto_mode::DenialTracking::default(),
                denial_window,
                branch_reservation: None,
                goal_recorded_tokens: snapshot.goal.as_ref().map_or(0, |goal| goal.tokens_used),
                loaded_from_history: waits == WaitHosting::Process,
                goal_clock_ms: 0,
                cache_prefix: Default::default(),
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
        // A loaded Loop watches its subscriptions even while it pauses receiving.
        self.sync_tick_subscriptions().await;
        if family.is_none() {
            self.restore_after_load(chat_id, waits).await;
        }
        Ok(snapshot)
    }
}

struct RestoredConversation {
    snapshot: ChatSnapshot,
    bookkeeping: ChatBookkeeping,
    /// Whether the chain holds a record of the Loop in the current format.
    records_loop: bool,
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
    recorded_loop: Option<LoopEntryContent>,
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
        // Choosing another stored branch changes the conversation alone, as a rewind does.
        snapshot.scheduled_tasks = previous.scheduled_tasks.clone();
        snapshot.scheduled_task_revocations = previous.scheduled_task_revocations.clone();
        if snapshot.plans.iter().any(|plan| plan.approval_pending) {
            snapshot.policy.mode = ExecutionMode::Plan;
        }
        chat.replace_snapshot(snapshot);
        chat.bookkeeping = self.bookkeeping;
        chat.bookkeeping.citation_sources = current.bookkeeping.citation_sources.clone();
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
    /// Continue writing to the original Transcript.
    Resume,
    /// Write to the Transcript of the new chatId.
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
                    protocol: tickloop_harness_protocol::Protocol::Responses,
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
