//! The local `TranscriptStore`: one append-only JSONL file per Chat under
//! `<root>/projects/<project>/`, the layout Claude Code uses.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use tickloop_harness_base::private_fs::{PRIVATE_FILE_MODE, create_private_directory};
use tickloop_harness_protocol::ChatId;
use tickloop_harness_transcript::{Line, MetadataHeader, MetadataKind};

use super::listing::{LITE_READ_BUF_SIZE, TranscriptSummary, last_json_string_field, list_project};
use super::path::{locate, project_directory, transcript_path};
use super::reader::{LoadedTranscript, assemble};
use super::store::{
    AppendAck, AppendBatch, BatchId, ChatPlacement, LoadRange, StorePosition, StoredTranscript,
    TranscriptError, TranscriptStore,
};

/// Following Claude Code's `SKIP_PRECOMPACT_THRESHOLD`: Transcripts over 5MB skip content before
/// the compaction boundary.
pub(crate) const SKIP_PRECOMPACT_THRESHOLD: u64 = 5 * 1024 * 1024;
const COMPACT_BOUNDARY_MARKER: &[u8] = b"\"compact_boundary\"";
const SYSTEM_LINE_PREFIX: &[u8] = b"{\"type\":\"system\"";
const DELEGATION_LINE_PREFIX: &[u8] = b"{\"type\":\"delegation\"";
/// How many acknowledged batches per Chat a retry can still find: each recorder retries only
/// its one pending batch, so this covers many recorders sharing a Chat.
const REMEMBERED_BATCHES: usize = 64;

/// Positions are byte lengths of the Chat's file, so appends from other processes keep them
/// monotonic. Expected positions are not enforced: two processes may continue the same Chat.
/// Every store opened on one root in a process shares what it knows of the files, so a batch
/// retried through any of them is written once.
#[derive(Clone)]
pub struct JsonlTranscriptStore {
    files: Arc<ChatFiles>,
}

/// The stores open in this process, by canonical root.
fn open_roots() -> &'static Mutex<HashMap<PathBuf, Weak<ChatFiles>>> {
    static OPEN_ROOTS: OnceLock<Mutex<HashMap<PathBuf, Weak<ChatFiles>>>> = OnceLock::new();
    OPEN_ROOTS.get_or_init(Mutex::default)
}

struct ChatFiles {
    root: PathBuf,
    chats: Mutex<HashMap<ChatId, ChatFile>>,
    io: Mutex<()>,
}

struct ChatFile {
    /// Fixed when created or found; later working directory changes do not move the file.
    path: PathBuf,
    /// The file is already on disk: this process appended to it successfully, or it was read while
    /// loading. Later appends no longer create the file, so a missing file means it was deleted
    /// externally.
    exists_on_disk: bool,
    /// The file as an acknowledged append or a load last saw it. A shorter or different file
    /// has lost committed Lines.
    known: Option<KnownFile>,
    /// Recently acknowledged batches, which a retry finds even after other batches.
    committed: VecDeque<(BatchId, AppendAck)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct KnownFile {
    identity: FileIdentity,
    length: u64,
}

impl KnownFile {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            identity: FileIdentity::of(metadata),
            length: metadata.len(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity(Option<(u64, u64)>);

impl FileIdentity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self(Some((metadata.dev(), metadata.ino())))
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Self(None)
        }
    }
}

impl ChatFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            exists_on_disk: false,
            known: None,
            committed: VecDeque::new(),
        }
    }

    fn remember(&mut self, batch: BatchId, ack: AppendAck) {
        if self.committed.len() == REMEMBERED_BATCHES {
            self.committed.pop_front();
        }
        self.committed.push_back((batch, ack));
    }

    fn acknowledged(&self, batch: BatchId) -> Option<AppendAck> {
        self.committed
            .iter()
            .find_map(|(id, ack)| (*id == batch).then_some(*ack))
    }
}

impl JsonlTranscriptStore {
    /// The store under `root`, created privately and in canonical form.
    pub fn open(root: PathBuf) -> io::Result<Self> {
        create_private_directory(&root)?;
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::other("session root is not a directory"));
        }
        let mut open = open_roots().lock().unwrap();
        open.retain(|_, files| files.strong_count() > 0);
        if let Some(files) = open.get(&root).and_then(Weak::upgrade) {
            return Ok(Self { files });
        }
        let files = Arc::new(ChatFiles {
            root: root.clone(),
            chats: Mutex::new(HashMap::new()),
            io: Mutex::new(()),
        });
        open.insert(root, Arc::downgrade(&files));
        Ok(Self { files })
    }

    pub fn root(&self) -> &Path {
        &self.files.root
    }

    /// The file of a Chat this store registered or loaded.
    pub fn chat_path(&self, chat: ChatId) -> Option<PathBuf> {
        self.files
            .chats
            .lock()
            .unwrap()
            .get(&chat)
            .map(|file| file.path.clone())
    }

    /// The saved Chats of `folder`'s project, newest first.
    pub fn project_chats(&self, folder: &Path) -> io::Result<Vec<TranscriptSummary>> {
        list_project(&project_directory(&self.files.root, folder))
    }

    /// ACP listing cannot treat a missed head/tail metadata window as an ordinary Chat. A file
    /// that cannot be scanned is listed as no ACP session rather than failing the listing.
    pub fn project_acp_chats(&self, folder: &Path) -> io::Result<Vec<TranscriptSummary>> {
        let mut summaries = self.project_chats(folder)?;
        let directory = project_directory(&self.files.root, folder);
        for summary in &mut summaries {
            if summary.acp_session.is_none() {
                summary.acp_session =
                    super::listing::scan_acp_session(&transcript_path(&directory, summary.chat_id));
            }
        }
        Ok(summaries)
    }

    /// The saved titles in the project of a Chat this store registered or loaded.
    pub fn project_titles(&self, chat: ChatId) -> Vec<String> {
        self.chat_path(chat)
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .and_then(|directory| list_project(&directory).ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|summary| summary.title)
            .collect()
    }

    /// The custom title last written to the end of the Chat's file, possibly by another process.
    pub fn latest_custom_title(&self, chat: ChatId) -> Option<String> {
        let tail = read_tail(&self.chat_path(chat)?).ok()?;
        last_json_string_field(&tail, "customTitle")
    }
}

#[async_trait]
impl TranscriptStore for JsonlTranscriptStore {
    async fn register(
        &self,
        chat: ChatId,
        placement: &ChatPlacement,
    ) -> Result<StorePosition, TranscriptError> {
        let mut chats = self.files.chats.lock().unwrap();
        let directory = match placement {
            ChatPlacement::Folder(folder) => project_directory(&self.files.root, folder),
            ChatPlacement::BesideChat(source) => chats
                .get(source)
                .and_then(|file| file.path.parent())
                .unwrap_or(&self.files.root)
                .to_path_buf(),
            ChatPlacement::Folderless => return Err(TranscriptError::Unsupported),
        };
        let path = transcript_path(&directory, chat.into());
        let position = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
        match chats.get_mut(&chat) {
            // Registering again keeps what the store knows of the file and its batches.
            Some(file) if file.path == path => {}
            Some(file) => {
                let committed = std::mem::take(&mut file.committed);
                *file = ChatFile::new(path);
                file.committed = committed;
            }
            None => {
                chats.insert(chat, ChatFile::new(path));
            }
        }
        Ok(StorePosition(position))
    }

    async fn load(
        &self,
        chat: ChatId,
        range: LoadRange,
    ) -> Result<Option<StoredTranscript>, TranscriptError> {
        let files = self.files.clone();
        tokio::task::spawn_blocking(move || files.load(chat, range))
            .await
            .map_err(|error| TranscriptError::Storage(error.to_string()))?
    }

    async fn append(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        let files = self.files.clone();
        let batch = batch.clone();
        tokio::task::spawn_blocking(move || files.append(chat, &batch))
            .await
            .map_err(|error| TranscriptError::Storage(error.to_string()))?
    }

    fn append_blocking(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.files.append(chat, batch)
    }
}

impl ChatFiles {
    fn load(
        &self,
        chat: ChatId,
        range: LoadRange,
    ) -> Result<Option<StoredTranscript>, TranscriptError> {
        let storage = |error: io::Error| TranscriptError::Storage(error.to_string());
        let known = self
            .chats
            .lock()
            .unwrap()
            .get(&chat)
            .map(|file| file.path.clone());
        let path = match known {
            Some(path) => path,
            None => match locate(&self.root, chat.into()).map_err(storage)? {
                Some(path) => path,
                None => return Ok(None),
            },
        };
        let (stored, metadata) = match read_file(&path, range)
            .and_then(|stored| Ok((stored, std::fs::metadata(&path)?)))
        {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage(error)),
        };
        let mut chats = self.chats.lock().unwrap();
        let file = chats.entry(chat).or_insert_with(|| ChatFile::new(path));
        file.exists_on_disk = true;
        file.known = Some(KnownFile {
            identity: FileIdentity::of(&metadata),
            length: stored.position.0,
        });
        Ok(Some(stored))
    }

    fn append(&self, chat: ChatId, batch: &AppendBatch) -> Result<AppendAck, TranscriptError> {
        // Checking the file and the batch, writing and recording happen as one step, so two
        // sends of one batch cannot both write it.
        let _io = self.io.lock().unwrap();
        let (path, exists_on_disk, known, acknowledged) = {
            let chats = self.chats.lock().unwrap();
            let file = chats.get(&chat).ok_or_else(|| {
                TranscriptError::Storage(format!(
                    "Chat {} has no Transcript in this store",
                    chat.public()
                ))
            })?;
            (
                file.path.clone(),
                file.exists_on_disk,
                file.known,
                file.acknowledged(batch.id),
            )
        };
        let lost = |what: &str| {
            TranscriptError::Conflict(format!(
                "Transcript {} was {what}, so the rest of this conversation is not saved",
                path.display()
            ))
        };
        let failure = |error: io::Error, action: &str| {
            if is_deleted(&error, exists_on_disk) {
                lost("deleted")
            } else {
                TranscriptError::Storage(format!(
                    "cannot {action} Transcript {}: {error}",
                    path.display()
                ))
            }
        };
        // Growth from other writers is fine; a shorter or replaced file lost committed Lines,
        // even ones whose acknowledgement is being retried.
        if let Some(known) = known {
            let current = std::fs::metadata(&path).map_err(|error| failure(error, "read"))?;
            if FileIdentity::of(&current) != known.identity {
                return Err(lost("replaced"));
            }
            if current.len() < known.length {
                return Err(lost("truncated"));
            }
        }
        let ack = match acknowledged {
            Some(ack) => ack,
            None => {
                let mut bytes = Vec::new();
                for line in &batch.lines {
                    append_line(&mut bytes, line);
                }
                let written = if bytes.is_empty() {
                    std::fs::metadata(&path).map_err(|error| failure(error, "read"))?
                } else {
                    append_to_transcript(&path, &bytes, exists_on_disk)
                        .map_err(|error| failure(error, "append"))?
                };
                let ack = AppendAck {
                    position: StorePosition(written.len()),
                };
                if let Some(file) = self.chats.lock().unwrap().get_mut(&chat) {
                    file.exists_on_disk |= !bytes.is_empty();
                    file.known = Some(KnownFile::of(&written));
                    file.remember(batch.id, ack);
                }
                ack
            }
        };
        if batch.durable {
            sync_file(&path).map_err(|error| failure(error, "sync"))?;
        }
        Ok(ack)
    }
}

/// Loads a Transcript file the way the Harness loads a Chat: from the latest boundary, and in
/// full when that misses the root. It only reads, so observing a saved Chat registers nothing
/// and creates no directory.
pub(crate) fn load_file(path: &Path) -> io::Result<Option<LoadedTranscript>> {
    let (loaded, reaches_root) = assemble(&read_file(path, LoadRange::FromLatestBoundary)?, None);
    if reaches_root && loaded.is_some() {
        return Ok(loaded);
    }
    Ok(assemble(&read_file(path, LoadRange::Full)?, None).0)
}

/// Reads a Transcript file. Above 5MB, a load from the latest boundary returns the lines from
/// the last compact boundary that starts the chain, with the retained lines before it, the way
/// Claude Code's `readTranscriptForLoad` does.
pub(crate) fn read_file(path: &Path, range: LoadRange) -> io::Result<StoredTranscript> {
    let bytes = std::fs::read(path)?;
    let lines = split_lines(&bytes);
    validate_delegation_records(&lines)?;
    let skip_before = match range {
        LoadRange::FromLatestBoundary if bytes.len() as u64 > SKIP_PRECOMPACT_THRESHOLD => {
            last_unpreserved_boundary_offset(&lines)
        }
        _ => 0,
    };
    // The retained Lines before the boundary are read from the latest snapshot on; a snapshot
    // after the boundary stands for all of them.
    let retained_from = match skip_before {
        0 => 0,
        _ => lines
            .iter()
            .rev()
            .find(|(_, raw)| {
                serde_json::from_slice::<MetadataHeader>(raw)
                    .is_ok_and(|header| header.kind == MetadataKind::RetainedSnapshot)
            })
            .map_or(0, |(offset, _)| (*offset).min(skip_before)),
    };
    let mut stored = StoredTranscript {
        position: StorePosition(bytes.len() as u64),
        ..StoredTranscript::default()
    };
    for &(offset, raw) in &lines {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let before_boundary = offset < skip_before;
        if before_boundary
            && (offset < retained_from
                || !serde_json::from_slice::<MetadataHeader>(raw)
                    .is_ok_and(|header| header.kind.is_retained()))
        {
            continue;
        }
        let Ok(line) = serde_json::from_slice::<Line>(raw) else {
            continue;
        };
        if before_boundary {
            stored.retained.push(line);
        } else {
            stored.lines.push(line);
        }
    }
    Ok(stored)
}

fn validate_delegation_records(lines: &[(u64, &[u8])]) -> io::Result<()> {
    let mut has_delegation_authorization = false;
    let mut has_parent_origin = false;
    for (_, raw) in lines {
        match serde_json::from_slice::<MetadataHeader>(raw) {
            Ok(header) => {
                if matches!(header.kind, MetadataKind::System)
                    && serde_json::from_slice::<serde_json::Value>(raw)
                        .ok()
                        .is_some_and(|entry| entry["loop"].get("subagentReport").is_some())
                {
                    serde_json::from_slice::<Line>(raw).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Subagent report checkpoint is invalid or incompatible",
                        )
                    })?;
                }
                if matches!(header.kind, MetadataKind::Delegation) {
                    serde_json::from_slice::<Line>(raw).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "delegated Transcript authorization is invalid or incompatible",
                        )
                    })?;
                    has_delegation_authorization = true;
                }
                has_parent_origin |= header
                    .agent_origin
                    .is_some_and(|origin| origin["sender_relationship"] == "parent");
            }
            Err(_) => {
                if raw.trim_ascii_start().starts_with(DELEGATION_LINE_PREFIX) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "delegated Transcript authorization is invalid or incompatible",
                    ));
                }
            }
        }
    }
    if has_parent_origin && !has_delegation_authorization {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "delegated Transcript authorization is missing",
        ));
    }
    Ok(())
}

/// File offset and content (without the newline) of each line.
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

/// Following Claude Code's `scanChunkLines`: offset of the line holding the last compaction
/// boundary without a preserved segment; boundaries with a preserved segment do not truncate (the
/// preserved messages come before them). Old boundaries rewritten after a new boundary by a
/// directed summary have a parent entry and are not the start of the conversation chain, so they do
/// not truncate either (boundaries written by Claude Code always have a null parent pointer).
/// Returns 0 when there is no such boundary.
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
            Ok(Line::System(entry)) if entry.restarts_chain() => Some(*offset),
            _ => None,
        })
        .unwrap_or(0)
}

fn is_deleted(error: &io::Error, exists_on_disk: bool) -> bool {
    exists_on_disk && error.kind() == io::ErrorKind::NotFound
}

fn append_line(buffer: &mut Vec<u8>, line: &Line) {
    if serde_json::to_writer(&mut *buffer, line).is_ok() {
        buffer.push(b'\n');
    }
}

/// When the file is already on disk, only appends and never creates: a missing file means it was
/// deleted externally, and the caller stops recording. The first write creates the file, creating
/// the directory first if needed.
fn append_to_transcript(
    path: &Path,
    bytes: &[u8],
    exists_on_disk: bool,
) -> io::Result<std::fs::Metadata> {
    if exists_on_disk {
        return append(path, bytes, false);
    }
    match append(path, bytes, true) {
        Ok(written) => Ok(written),
        Err(_) => {
            if let Some(parent) = path.parent() {
                create_private_directory(parent)?;
            }
            append(path, bytes, true)
        }
    }
}

/// On a mid-write failure, truncates the partially written bytes back to the original length,
/// avoiding a broken line and duplicate writes on retry.
/// Returns the file as it is after the write, including what other writers appended.
fn append(path: &Path, bytes: &[u8], create: bool) -> io::Result<std::fs::Metadata> {
    let mut options = std::fs::OpenOptions::new();
    options.create(create).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, PRIVATE_FILE_MODE);
    let mut file = options.open(path)?;
    let original_length = file.metadata()?.len();
    if let Err(error) = file.write_all(bytes) {
        let _ = file.set_len(original_length);
        return Err(error);
    }
    file.metadata()
}

fn sync_file(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn read_tail(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(LITE_READ_BUF_SIZE as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
