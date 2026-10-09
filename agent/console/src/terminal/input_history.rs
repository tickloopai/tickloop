//! 输入历史：照 Claude Code `history.ts`，保存在全局 `<root>/history.jsonl`。
//!
//! 条目先进入内存缓冲，由后台写入线程加锁追加到文件；读取时先看缓冲，
//! 再倒序读文件并跳过坏行。已解析的文件内容按文件身份与长度缓存，文件变长时
//! 只解析新增字节。大段粘贴按内容哈希写入 `<root>/paste-cache/`，只在填回草稿时读取。

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    ops::{ControlFlow, Range},
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime},
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use tickloop_agent_base::private_fs::{PRIVATE_FILE_MODE, unix_time_milliseconds};
use tickloop_agent_config::{PASTE_CACHE_DIRECTORY, PASTE_FILE_EXTENSION};

const HISTORY_FILE_NAME: &str = "history.jsonl";
const MAX_HISTORY_ITEMS: usize = 100;
const MAX_SHELL_HISTORY_COMMANDS: usize = 50;
const SHELL_PROMPT_PREFIX: char = '!';
const MAX_INLINE_PASTED_CHARACTERS: usize = 1024;
const PASTE_HASH_HEX_LENGTH: usize = 16;
// proper-lockfile 语义：锁目录超过 10 秒视为过期；拿不到锁时再试 3 次，每次间隔 50ms。
const LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
const LOCK_RETRIES: usize = 3;
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
// 追加失败后每 500ms 重试，最多 5 次；之后暂停，直到下一条输入到来。
const MAX_FLUSH_RETRIES: usize = 5;
const FLUSH_RETRY_DELAY: Duration = Duration::from_millis(500);
const REVERSE_READ_CHUNK_BYTES: u64 = 64 * 1024;
const PASTED_TEXT_KIND: &str = "text";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum HistoryScope {
    #[default]
    ThisSession,
    ThisProject,
    AllProjects,
}

impl HistoryScope {
    pub(super) fn next(self) -> Self {
        match self {
            Self::ThisSession => Self::ThisProject,
            Self::ThisProject => Self::AllProjects,
            Self::AllProjects => Self::ThisSession,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ThisSession => "This session",
            Self::ThisProject => "This project",
            Self::AllProjects => "All projects",
        }
    }
}

/// 草稿中一段折叠粘贴的位置，`range` 是原始草稿中的字节范围。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PastedText {
    pub(super) id: u64,
    pub(super) range: Range<usize>,
}

/// 搜索候选只携带显示文本与已存储的粘贴引用；完整输入由
/// [`InputHistory::resolve_match`] 在填回草稿时展开。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HistoryMatch {
    project: String,
    display: String,
    pasted_contents: BTreeMap<u64, StoredPastedContent>,
}

impl HistoryMatch {
    pub(super) fn project(&self) -> &str {
        &self.project
    }

    /// 带粘贴占位符的显示文本，用于列表展示与搜索匹配。
    pub(super) fn display(&self) -> &str {
        &self.display
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HistoryDraft {
    pub(super) text: String,
    pub(super) cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HistorySearchInput {
    Insert(String),
    Backspace,
    Previous,
    Next,
    CycleScope,
    AcceptEnter,
    AcceptTab,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HistorySearchOutcome {
    Open,
    Accepted(HistoryDraft),
    Cancelled(HistoryDraft),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredEntry {
    display: String,
    #[serde(default)]
    pasted_contents: BTreeMap<u64, StoredPastedContent>,
    timestamp: u64,
    project: String,
    #[serde(default)]
    chat_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPastedContent {
    id: u64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
}

#[derive(Clone, Debug)]
struct HistoryPaths {
    root: PathBuf,
    history_file: PathBuf,
    lock_directory: PathBuf,
    paste_cache: PathBuf,
}

impl HistoryPaths {
    fn new(root: &Path) -> Self {
        let history_file = root.join(HISTORY_FILE_NAME);
        Self {
            root: root.to_path_buf(),
            lock_directory: root.join(format!("{HISTORY_FILE_NAME}.lock")),
            history_file,
            paste_cache: root.join(PASTE_CACHE_DIRECTORY),
        }
    }

    fn paste_file(&self, hash: &str) -> PathBuf {
        self.paste_cache
            .join(format!("{hash}{PASTE_FILE_EXTENSION}"))
    }
}

#[derive(Debug, Default)]
struct PendingWrites {
    entries: Vec<StoredEntry>,
    is_closing: bool,
    is_paused: bool,
}

#[derive(Debug, Default)]
struct WriterShared {
    pending: Mutex<PendingWrites>,
    wake: Condvar,
}

impl WriterShared {
    fn lock(&self) -> MutexGuard<'_, PendingWrites> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug)]
pub(super) struct InputHistory {
    paths: Option<HistoryPaths>,
    shared: Arc<WriterShared>,
    writer: Option<JoinHandle<()>>,
    last_recorded: Option<StoredEntry>,
    /// The entry the last [`Self::record`] queued, until [`Self::withdraw_last`] takes it.
    last_added: Option<StoredEntry>,
    /// Written entries this process withdrew; reads skip them.
    withdrawn: Vec<StoredEntry>,
    parsed_file: Mutex<ParsedHistoryFile>,
}

impl InputHistory {
    pub(super) fn open(root: &Path) -> Self {
        let paths = HistoryPaths::new(root);
        let shared = Arc::new(WriterShared::default());
        let writer = {
            let shared = Arc::clone(&shared);
            let paths = paths.clone();
            thread::Builder::new()
                .name("agent-input-history".into())
                .spawn(move || run_writer(&shared, &paths))
                .ok()
        };
        Self {
            paths: Some(paths),
            shared,
            writer,
            last_recorded: None,
            last_added: None,
            withdrawn: Vec::new(),
            parsed_file: Mutex::default(),
        }
    }

    /// 不写盘的历史，只在本进程内可见。
    pub(super) fn in_memory() -> Self {
        Self {
            paths: None,
            shared: Arc::new(WriterShared::default()),
            writer: None,
            last_recorded: None,
            last_added: None,
            withdrawn: Vec::new(),
            parsed_file: Mutex::default(),
        }
    }

    pub(super) fn record(
        &mut self,
        chat_id: &str,
        cwd: &Path,
        prompt: &str,
        pasted_texts: &[PastedText],
    ) -> Result<bool, InputHistoryError> {
        let chat_id = safe_single_line(chat_id);
        let project = project_key(cwd);
        if chat_id.is_empty() {
            return Err(InputHistoryError::message("history chat ID is empty"));
        }
        if project.is_empty() {
            return Err(InputHistoryError::message("history project path is empty"));
        }
        let (display, pasted) = split_pasted_texts(prompt, pasted_texts);
        let display = safe_prompt(&display);
        if display.is_empty() {
            return Ok(false);
        }
        let pasted_contents = pasted
            .into_iter()
            .map(|(id, content)| (id, self.store_pasted_text(id, &safe_prompt(&content))))
            .collect();
        let mut entry = StoredEntry {
            display,
            pasted_contents,
            timestamp: 0,
            project,
            chat_id,
        };
        if self.last_recorded.as_ref() == Some(&entry) {
            return Ok(false);
        }
        self.last_recorded = Some(entry.clone());
        entry.timestamp = unix_time_milliseconds();
        self.last_added = Some(entry.clone());

        let mut pending = self.shared.lock();
        pending.entries.push(entry);
        pending.is_paused = false;
        drop(pending);
        self.shared.wake.notify_all();
        Ok(true)
    }

    /// 照 Claude Code `removeLastFromHistory`：提交后又被撤回、放回输入框的提示不再留在历史里，
    /// 上箭头不会把它和输入框中的文字重复列出。只撤回最近一次记录且内容就是 `prompt` 的条目，
    /// 最多一次；仍在缓冲中的直接移除，已写入文件的在本进程读取时跳过。
    pub(super) fn withdraw_last(&mut self, prompt: &str) {
        let Some(entry) = self.last_added.take() else {
            return;
        };
        if self.resolve_pasted_text(&entry.display, &entry.pasted_contents) != safe_prompt(prompt) {
            return;
        }
        self.last_recorded = None;
        let mut pending = self.shared.lock();
        if let Some(index) = pending
            .entries
            .iter()
            .rposition(|candidate| *candidate == entry)
        {
            pending.entries.remove(index);
        } else {
            drop(pending);
            self.withdrawn.push(entry);
        }
    }

    /// 上箭头历史：只看当前项目，当前 Chat 的条目在前，各组内由新到旧。
    pub(super) fn recent_prompts(
        &self,
        chat_id: &str,
        cwd: &Path,
    ) -> Result<Vec<String>, InputHistoryError> {
        Ok(self
            .project_window(chat_id, cwd)?
            .iter()
            .map(|entry| self.resolve_pasted_text(&entry.display, &entry.pasted_contents))
            .filter(|prompt| !prompt.starts_with(SHELL_PROMPT_PREFIX))
            .collect())
    }

    /// Shell 补全历史：照 Claude Code `getShellHistoryCommands`，在上箭头的同一窗口内
    /// 取以 `!` 开头的显示文本，去掉前缀并修剪空白后去重，最多 50 条；不展开粘贴。
    pub(super) fn recent_shell_commands(
        &self,
        chat_id: &str,
        cwd: &Path,
    ) -> Result<Vec<String>, InputHistoryError> {
        let mut seen = HashSet::new();
        Ok(self
            .project_window(chat_id, cwd)?
            .into_iter()
            .filter_map(|entry| {
                let command = entry.display.strip_prefix(SHELL_PROMPT_PREFIX)?.trim();
                (!command.is_empty()).then(|| command.to_owned())
            })
            .filter(|command| seen.insert(command.clone()))
            .take(MAX_SHELL_HISTORY_COMMANDS)
            .collect())
    }

    /// 展开候选中的粘贴引用，得到填回草稿的完整输入；大段粘贴此时才读取 paste-cache。
    pub(super) fn resolve_match(&self, candidate: &HistoryMatch) -> String {
        self.resolve_pasted_text(&candidate.display, &candidate.pasted_contents)
    }

    /// 当前项目最近 100 条，当前 Chat 的条目排在前面，各组内由新到旧。
    fn project_window(
        &self,
        chat_id: &str,
        cwd: &Path,
    ) -> Result<Vec<StoredEntry>, InputHistoryError> {
        let chat_id = safe_single_line(chat_id);
        let project = project_key(cwd);
        let mut current_chat = Vec::new();
        let mut other_chats = Vec::new();
        self.visit_entries_newest_first(|entry| {
            if entry.project == project {
                if entry.chat_id == chat_id {
                    current_chat.push(entry.clone());
                } else {
                    other_chats.push(entry.clone());
                }
            }
            if current_chat.len() + other_chats.len() >= MAX_HISTORY_ITEMS {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        current_chat.append(&mut other_chats);
        Ok(current_chat)
    }

    /// 一次性搜索，供测试直接断言候选；界面通过 [`HistorySearch`] 使用同一候选集。
    #[cfg(test)]
    pub(super) fn search(
        &self,
        chat_id: &str,
        cwd: &Path,
        scope: HistoryScope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<HistoryMatch>, InputHistoryError> {
        let candidates = self.search_candidates(chat_id, cwd, scope)?;
        Ok(filter_matches(&candidates, query, limit))
    }

    /// 停止后台写入线程并冲刷缓冲；退出时调用，重复调用无副作用。
    pub(super) fn flush(&mut self) {
        let Some(writer) = self.writer.take() else {
            return;
        };
        self.shared.lock().is_closing = true;
        self.shared.wake.notify_all();
        let _ = writer.join();
    }

    /// 搜索候选：按范围过滤、按显示文本去重，由新到旧最多 100 条。
    fn search_candidates(
        &self,
        chat_id: &str,
        cwd: &Path,
        scope: HistoryScope,
    ) -> Result<Vec<HistoryMatch>, InputHistoryError> {
        let chat_id = safe_single_line(chat_id);
        let project = project_key(cwd);
        let mut seen_displays = HashSet::new();
        let mut entries = Vec::new();
        self.visit_entries_newest_first(|entry| {
            let is_in_scope = match scope {
                HistoryScope::ThisSession => entry.chat_id == chat_id,
                HistoryScope::ThisProject => entry.project == project,
                HistoryScope::AllProjects => true,
            };
            if is_in_scope && seen_displays.insert(entry.display.clone()) {
                entries.push(HistoryMatch {
                    project: entry.project.clone(),
                    display: entry.display.clone(),
                    pasted_contents: entry.pasted_contents.clone(),
                });
            }
            if entries.len() >= MAX_HISTORY_ITEMS {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        Ok(entries)
    }

    fn visit_entries_newest_first(
        &self,
        mut visit: impl FnMut(&StoredEntry) -> ControlFlow<()>,
    ) -> Result<(), InputHistoryError> {
        let mut parsed_file = self
            .parsed_file
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // 持有缓冲锁完成快照并同步文件新增部分，避免与写入线程交错时重复或遗漏条目。
        let pending = {
            let pending = self.shared.lock();
            if let Some(paths) = &self.paths {
                parsed_file
                    .sync(&paths.history_file)
                    .map_err(InputHistoryError::new)?;
            }
            pending.entries.clone()
        };
        let mut visit = |entry: &StoredEntry| {
            if self.withdrawn.contains(entry) {
                ControlFlow::Continue(())
            } else {
                visit(entry)
            }
        };
        for entry in pending.iter().rev() {
            if visit(entry).is_break() {
                return Ok(());
            }
        }
        let mut index = 0;
        loop {
            if index == parsed_file.entries.len() {
                if parsed_file
                    .load_older_lines()
                    .map_err(InputHistoryError::new)?
                {
                    continue;
                }
                return Ok(());
            }
            if visit(&parsed_file.entries[index]).is_break() {
                return Ok(());
            }
            index += 1;
        }
    }

    fn store_pasted_text(&self, id: u64, content: &str) -> StoredPastedContent {
        let mut stored = StoredPastedContent {
            id,
            kind: PASTED_TEXT_KIND.into(),
            content: Some(content.to_owned()),
            content_hash: None,
        };
        let Some(paths) = &self.paths else {
            return stored;
        };
        if content.chars().count() <= MAX_INLINE_PASTED_CHARACTERS {
            return stored;
        }
        let hash = hash_pasted_text(content);
        // 与 Claude Code 一致：写入失败不阻止条目记录，只是恢复时无法展开该粘贴。
        let _ = write_private_file(&paths.paste_cache, &paths.paste_file(&hash), content);
        stored.content = None;
        stored.content_hash = Some(hash);
        stored
    }

    fn resolve_pasted_text(
        &self,
        display: &str,
        pasted_contents: &BTreeMap<u64, StoredPastedContent>,
    ) -> String {
        let contents = pasted_contents
            .iter()
            .filter(|(_, stored)| stored.kind == PASTED_TEXT_KIND)
            .filter_map(|(id, stored)| {
                let content = match (&stored.content, &stored.content_hash, &self.paths) {
                    (Some(content), _, _) => content.clone(),
                    (None, Some(hash), Some(paths)) if is_valid_paste_hash(hash) => {
                        safe_prompt(&fs::read_to_string(paths.paste_file(hash)).ok()?)
                    }
                    _ => return None,
                };
                Some((*id, content))
            })
            .collect::<BTreeMap<_, _>>();
        expand_pasted_text_references(display, &contents)
    }
}

impl Drop for InputHistory {
    fn drop(&mut self) {
        self.flush();
    }
}

fn run_writer(shared: &WriterShared, paths: &HistoryPaths) {
    let mut failed_attempts = 0;
    let mut pending = shared.lock();
    loop {
        while !pending.is_closing && (pending.entries.is_empty() || pending.is_paused) {
            pending = shared
                .wake
                .wait(pending)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if pending.is_closing {
            drop(pending);
            let _ = append_pending_entries(shared, paths);
            return;
        }
        drop(pending);
        match append_pending_entries(shared, paths) {
            Ok(()) => failed_attempts = 0,
            Err(_) => failed_attempts += 1,
        }
        pending = shared.lock();
        if failed_attempts > MAX_FLUSH_RETRIES {
            failed_attempts = 0;
            pending.is_paused = true;
        } else if failed_attempts > 0 && !pending.is_closing {
            pending = shared
                .wake
                .wait_timeout(pending, FLUSH_RETRY_DELAY)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

fn append_pending_entries(shared: &WriterShared, paths: &HistoryPaths) -> io::Result<()> {
    if shared.lock().entries.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(&paths.root)?;
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(PRIVATE_FILE_MODE)
        .open(&paths.history_file)?;
    let _lock = HistoryFileLock::acquire(&paths.lock_directory)?;
    let mut pending = shared.lock();
    append_batch(&mut file, &mut pending.entries)
}

/// 照 Claude Code `immediateFlushHistory`：拿到锁后先取出整批并清空缓冲再追加，
/// 追加失败则丢弃该批，不会在重试时重写已写入的部分。已写入的半截字节在仍持锁时
/// 截回原长度，避免留下坏行并与下一批的首行粘连。
fn append_batch(
    file: &mut impl HistoryAppendTarget,
    entries: &mut Vec<StoredEntry>,
) -> io::Result<()> {
    let original_length = file.length()?;
    let batch = std::mem::take(entries);
    let mut payload = String::new();
    for entry in &batch {
        let line = serde_json::to_string(entry).map_err(io::Error::other)?;
        payload.push_str(&line);
        payload.push('\n');
    }
    if let Err(error) = file.write_all(payload.as_bytes()) {
        let _ = file.truncate(original_length);
        return Err(error);
    }
    Ok(())
}

trait HistoryAppendTarget: Write {
    fn length(&self) -> io::Result<u64>;
    fn truncate(&mut self, length: u64) -> io::Result<()>;
}

impl HistoryAppendTarget for File {
    fn length(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn truncate(&mut self, length: u64) -> io::Result<()> {
        self.set_len(length)
    }
}

/// proper-lockfile 风格的目录锁：`mkdir` 成功即持有，释放时删除目录。
struct HistoryFileLock {
    directory: PathBuf,
}

impl HistoryFileLock {
    fn acquire(directory: &Path) -> io::Result<Self> {
        let mut retries_left = LOCK_RETRIES;
        loop {
            match fs::create_dir(directory) {
                Ok(()) => {
                    return Ok(Self {
                        directory: directory.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(directory) && fs::remove_dir(directory).is_ok() {
                        continue;
                    }
                    if retries_left == 0 {
                        return Err(error);
                    }
                    retries_left -= 1;
                    thread::sleep(LOCK_RETRY_INTERVAL);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for HistoryFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.directory);
    }
}

fn lock_is_stale(directory: &Path) -> bool {
    fs::metadata(directory)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > LOCK_STALE_AFTER)
}

/// 已解析的 `history.jsonl` 片段：缓存 `[oldest_line_start, newest_line_end)` 内的完整行，
/// 条目由新到旧排列。按需向前倒序补读更早的块；文件变长时只解析新增的完整行，
/// 末尾未写完的半行留到补全后再读。文件被替换、截短或同长度改写时整体重建。
#[derive(Debug, Default)]
struct ParsedHistoryFile {
    file: Option<File>,
    identity: Option<(u64, u64)>,
    length: u64,
    modified: Option<SystemTime>,
    oldest_line_start: u64,
    newest_line_end: u64,
    entries: VecDeque<StoredEntry>,
}

impl ParsedHistoryFile {
    fn sync(&mut self, path: &Path) -> io::Result<()> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                *self = Self::default();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        let identity = Some((metadata.dev(), metadata.ino()));
        let length = metadata.len();
        let modified = metadata.modified().ok();
        let is_unchanged_prefix = self.identity == identity
            && length >= self.length
            && (length > self.length || modified == self.modified);
        if !is_unchanged_prefix {
            let boundary = line_boundary_before(&file, length)?;
            *self = Self {
                identity,
                oldest_line_start: boundary,
                newest_line_end: boundary,
                ..Self::default()
            };
        }
        self.file = Some(file);
        self.length = length;
        self.modified = modified;
        self.load_newer_lines()
    }

    fn load_newer_lines(&mut self) -> io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        if self.length <= self.newest_line_end {
            return Ok(());
        }
        let appended = read_byte_range(file, self.newest_line_end, self.length)?;
        let Some(last_newline) = appended.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(());
        };
        for line in appended[..last_newline].split(|byte| *byte == b'\n') {
            if let Some(entry) = parse_entry(line) {
                self.entries.push_front(entry);
            }
        }
        self.newest_line_end += last_newline as u64 + 1;
        Ok(())
    }

    /// 向前补读一块完整行；已读到文件开头时返回 `false`。
    fn load_older_lines(&mut self) -> io::Result<bool> {
        let Some(file) = &self.file else {
            return Ok(false);
        };
        let end = self.oldest_line_start;
        if end == 0 {
            return Ok(false);
        }
        let mut chunk_bytes = REVERSE_READ_CHUNK_BYTES;
        loop {
            let start = end.saturating_sub(chunk_bytes);
            let chunk = read_byte_range(file, start, end)?;
            // `end` 前一个字节是换行；块首的残缺行不算，单行超过块大小时加倍重读。
            let first_line_offset = if start == 0 {
                0
            } else {
                match chunk[..chunk.len() - 1]
                    .iter()
                    .position(|byte| *byte == b'\n')
                {
                    Some(index) => index + 1,
                    None => {
                        chunk_bytes = chunk_bytes.saturating_mul(2);
                        continue;
                    }
                }
            };
            for line in chunk[first_line_offset..]
                .split(|byte| *byte == b'\n')
                .rev()
            {
                if let Some(entry) = parse_entry(line) {
                    self.entries.push_back(entry);
                }
            }
            self.oldest_line_start = start + first_line_offset as u64;
            return Ok(true);
        }
    }
}

fn line_boundary_before(file: &File, end: u64) -> io::Result<u64> {
    let mut chunk_end = end;
    while chunk_end > 0 {
        let chunk_start = chunk_end.saturating_sub(REVERSE_READ_CHUNK_BYTES);
        let chunk = read_byte_range(file, chunk_start, chunk_end)?;
        if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
            return Ok(chunk_start + index as u64 + 1);
        }
        chunk_end = chunk_start;
    }
    Ok(0)
}

fn read_byte_range(file: &File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; (end - start) as usize];
    file.read_exact_at(&mut bytes, start)?;
    Ok(bytes)
}

fn parse_entry(line: &[u8]) -> Option<StoredEntry> {
    let mut entry = serde_json::from_slice::<StoredEntry>(line).ok()?;
    entry.display = safe_prompt(&entry.display);
    entry.project = safe_single_line(&entry.project);
    entry.chat_id = safe_single_line(&entry.chat_id);
    for stored in entry.pasted_contents.values_mut() {
        stored.content = stored.content.as_deref().map(safe_prompt);
    }
    (!entry.display.is_empty() && !entry.project.is_empty()).then_some(entry)
}

/// 把草稿中的折叠粘贴换成 `[Pasted text #N +M lines]` 占位符，返回显示文本与粘贴内容。
fn split_pasted_texts(prompt: &str, pasted_texts: &[PastedText]) -> (String, Vec<(u64, String)>) {
    let mut ordered = pasted_texts.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|pasted| pasted.range.start);
    let mut display = String::with_capacity(prompt.len());
    let mut contents: Vec<(u64, String)> = Vec::new();
    let mut offset = 0;
    for pasted in ordered {
        let range = &pasted.range;
        let is_usable = pasted.id > 0
            && offset <= range.start
            && range.start < range.end
            && prompt.get(range.clone()).is_some()
            && contents.iter().all(|(id, _)| *id != pasted.id);
        if !is_usable {
            continue;
        }
        let content = &prompt[range.clone()];
        display.push_str(&prompt[offset..range.start]);
        display.push_str(&format_pasted_text_reference(
            pasted.id,
            line_break_count(content),
        ));
        contents.push((pasted.id, content.to_owned()));
        offset = range.end;
    }
    display.push_str(&prompt[offset..]);
    (display, contents)
}

fn format_pasted_text_reference(id: u64, line_breaks: usize) -> String {
    if line_breaks == 0 {
        format!("[Pasted text #{id}]")
    } else {
        format!("[Pasted text #{id} +{line_breaks} lines]")
    }
}

/// 与 Claude Code 相同，按 `\r\n`、`\r`、`\n` 计数换行。
fn line_break_count(text: &str) -> usize {
    let mut characters = text.chars().peekable();
    let mut count = 0;
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                characters.next_if_eq(&'\n');
                count += 1;
            }
            '\n' => count += 1,
            _ => {}
        }
    }
    count
}

fn expand_pasted_text_references(display: &str, contents: &BTreeMap<u64, String>) -> String {
    static REFERENCE_PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = REFERENCE_PATTERN.get_or_init(|| {
        Regex::new(r"\[(?:Pasted text|Image|\.\.\.Truncated text) #(\d+)(?: \+\d+ lines)?\.*\]")
            .expect("pasted text reference pattern is valid")
    });
    let mut expanded = String::with_capacity(display.len());
    let mut offset = 0;
    for captures in pattern.captures_iter(display) {
        let (Some(whole), Some(id)) = (captures.get(0), captures.get(1)) else {
            continue;
        };
        let Some(content) = id
            .as_str()
            .parse::<u64>()
            .ok()
            .and_then(|id| contents.get(&id))
        else {
            continue;
        };
        expanded.push_str(&display[offset..whole.start()]);
        expanded.push_str(content);
        offset = whole.end();
    }
    expanded.push_str(&display[offset..]);
    expanded
}

fn hash_pasted_text(content: &str) -> String {
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..PASTE_HASH_HEX_LENGTH]
        .to_owned()
}

fn is_valid_paste_hash(hash: &str) -> bool {
    hash.len() == PASTE_HASH_HEX_LENGTH && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn write_private_file(directory: &Path, path: &Path, content: &str) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)?
        .write_all(content.as_bytes())
}

fn filter_matches(candidates: &[HistoryMatch], query: &str, limit: usize) -> Vec<HistoryMatch> {
    let query = safe_single_line(query).to_lowercase();
    let mut matches = candidates
        .iter()
        .enumerate()
        .filter_map(|(recency, candidate)| {
            match_score(&candidate.display, &query).map(|score| (score, recency, candidate))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(score, recency, _)| (*score, *recency));
    matches
        .into_iter()
        .take(limit)
        .map(|(_, _, candidate)| candidate.clone())
        .collect()
}

#[derive(Clone, Debug)]
pub(super) struct HistorySearch {
    chat_id: String,
    cwd: PathBuf,
    original: HistoryDraft,
    query: String,
    candidates: Vec<HistoryMatch>,
    matches: Vec<HistoryMatch>,
    selected: Option<usize>,
    scope: HistoryScope,
    limit: usize,
}

impl HistorySearch {
    pub(super) fn open(
        history: &InputHistory,
        chat_id: &str,
        cwd: &Path,
        original_draft: &str,
        original_cursor: usize,
        initial_query: &str,
        limit: usize,
    ) -> Result<Self, InputHistoryError> {
        let original = HistoryDraft {
            text: original_draft.to_owned(),
            cursor: floor_char_boundary(original_draft, original_cursor.min(original_draft.len())),
        };
        let mut search = Self {
            chat_id: safe_single_line(chat_id),
            cwd: cwd.to_path_buf(),
            original,
            query: safe_single_line(initial_query),
            candidates: Vec::new(),
            matches: Vec::new(),
            selected: None,
            scope: HistoryScope::ThisSession,
            limit,
        };
        search.reload(history)?;
        Ok(search)
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn matches(&self) -> &[HistoryMatch] {
        &self.matches
    }

    pub(super) fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub(super) fn selected_match(&self) -> Option<&HistoryMatch> {
        self.selected.and_then(|index| self.matches.get(index))
    }

    pub(super) fn scope(&self) -> HistoryScope {
        self.scope
    }

    pub(super) fn apply(
        &mut self,
        history: &InputHistory,
        input: HistorySearchInput,
    ) -> Result<HistorySearchOutcome, InputHistoryError> {
        match input {
            HistorySearchInput::Insert(text) => {
                self.query.push_str(&safe_single_line(&text));
                self.refilter();
            }
            HistorySearchInput::Backspace => {
                self.query.pop();
                self.refilter();
            }
            HistorySearchInput::Previous => {
                if !self.matches.is_empty() {
                    self.selected = Some(self.selected.unwrap_or(0).saturating_sub(1));
                }
            }
            HistorySearchInput::Next => {
                if !self.matches.is_empty() {
                    self.selected = Some(
                        self.selected
                            .map_or(0, |index| (index + 1).min(self.matches.len() - 1)),
                    );
                }
            }
            HistorySearchInput::CycleScope => {
                self.scope = self.scope.next();
                self.reload(history)?;
            }
            HistorySearchInput::AcceptEnter | HistorySearchInput::AcceptTab => {
                if let Some(selected) = self.selected_match() {
                    let text = history.resolve_match(selected);
                    return Ok(HistorySearchOutcome::Accepted(HistoryDraft {
                        cursor: text.len(),
                        text,
                    }));
                }
            }
            HistorySearchInput::Cancel => {
                return Ok(HistorySearchOutcome::Cancelled(self.original.clone()));
            }
        }
        Ok(HistorySearchOutcome::Open)
    }

    fn reload(&mut self, history: &InputHistory) -> Result<(), InputHistoryError> {
        self.candidates = history.search_candidates(&self.chat_id, &self.cwd, self.scope)?;
        self.refilter();
        Ok(())
    }

    fn refilter(&mut self) {
        self.matches = filter_matches(&self.candidates, &self.query, self.limit);
        self.selected = (!self.matches.is_empty()).then_some(0);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InputHistoryError {
    message: String,
}

impl InputHistoryError {
    fn new(error: impl fmt::Display) -> Self {
        Self::message(error.to_string())
    }

    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for InputHistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InputHistoryError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MatchScore {
    class: u8,
    start: usize,
    gaps: usize,
}

fn match_score(prompt: &str, query: &str) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(MatchScore {
            class: 4,
            start: 0,
            gaps: 0,
        });
    }

    let prompt = prompt.to_lowercase();
    if prompt == query {
        return Some(MatchScore {
            class: 0,
            start: 0,
            gaps: 0,
        });
    }
    if prompt.starts_with(query) {
        return Some(MatchScore {
            class: 1,
            start: 0,
            gaps: prompt.chars().count().saturating_sub(query.chars().count()),
        });
    }
    if let Some(start) = prompt.find(query) {
        return Some(MatchScore {
            class: 2,
            start: prompt[..start].chars().count(),
            gaps: 0,
        });
    }

    let mut prompt_characters = prompt.chars().enumerate();
    let mut first = None;
    let mut last = 0;
    for query_character in query.chars() {
        let (index, _) =
            prompt_characters.find(|(_, prompt_character)| *prompt_character == query_character)?;
        first.get_or_insert(index);
        last = index;
    }
    let first = first.unwrap_or(0);
    Some(MatchScore {
        class: 3,
        start: first,
        gaps: last.saturating_sub(first) + 1 - query.chars().count(),
    })
}

fn project_key(cwd: &Path) -> String {
    let path = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    safe_single_line(&path.to_string_lossy())
}

fn safe_prompt(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || *character == '\n' || *character == '\t')
        .collect()
}

fn safe_single_line(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_root(directory: &tempfile::TempDir) -> PathBuf {
        directory.path().join("nested/root")
    }

    fn history_lines(root: &Path) -> Vec<String> {
        fs::read_to_string(root.join(HISTORY_FILE_NAME))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn record(history: &mut InputHistory, chat_id: &str, cwd: &Path, prompt: &str) -> bool {
        history.record(chat_id, cwd, prompt, &[]).unwrap()
    }

    fn displays(matches: &[HistoryMatch]) -> Vec<&str> {
        matches.iter().map(HistoryMatch::display).collect()
    }

    #[test]
    fn history_persists_across_restart_and_skips_consecutive_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        {
            let mut history = InputHistory::open(&root);
            assert!(record(&mut history, "chat-a", project.path(), "first"));
            assert!(!record(&mut history, "chat-a", project.path(), "first"));
            assert!(record(&mut history, "chat-a", project.path(), "second"));
            assert!(record(&mut history, "chat-a", project.path(), "first"));
        }
        assert_eq!(history_lines(&root).len(), 3);

        let history = InputHistory::open(&root);
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["first", "second", "first"]
        );
        let matches = history
            .search("chat-a", project.path(), HistoryScope::ThisProject, "", 10)
            .unwrap();
        assert_eq!(displays(&matches), ["first", "second"]);
    }

    #[test]
    fn withdrawing_the_last_prompt_hides_it_whether_or_not_it_was_written() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut history = InputHistory::in_memory();
        assert!(record(&mut history, "chat-a", project.path(), "kept"));
        assert!(record(&mut history, "chat-a", project.path(), "sent"));
        history.withdraw_last("another prompt");
        history.withdraw_last("sent");
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["sent", "kept"],
            "only a matching last prompt is withdrawn, once"
        );
        assert!(record(&mut history, "chat-a", project.path(), "taken back"));
        history.withdraw_last("taken back");
        history.withdraw_last("sent");
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["sent", "kept"],
            "earlier entries stay"
        );

        let root = history_root(&directory);
        let mut written = InputHistory::open(&root);
        assert!(record(&mut written, "chat-a", project.path(), "kept"));
        assert!(record(&mut written, "chat-a", project.path(), "taken back"));
        written.flush();
        assert_eq!(history_lines(&root).len(), 2);
        written.withdraw_last("taken back");
        assert_eq!(
            written.recent_prompts("chat-a", project.path()).unwrap(),
            ["kept"]
        );
        // A new entry differs from the withdrawn one by its millisecond timestamp.
        thread::sleep(Duration::from_millis(2));
        assert!(record(&mut written, "chat-a", project.path(), "taken back"));
        assert_eq!(
            written.recent_prompts("chat-a", project.path()).unwrap(),
            ["taken back", "kept"],
            "sending the prompt again records it again"
        );
    }

    #[test]
    fn two_writers_each_append_200_entries_without_losing_or_corrupting_lines() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        let writers = ["writer-a", "writer-b"].map(|chat_id| {
            let root = root.clone();
            let project = project.path().to_path_buf();
            thread::spawn(move || {
                let mut history = InputHistory::open(&root);
                for index in 0..200 {
                    record(
                        &mut history,
                        chat_id,
                        &project,
                        &format!("{chat_id} 第 {index} 条\n多行"),
                    );
                }
            })
        });
        for writer in writers {
            writer.join().unwrap();
        }

        let lines = history_lines(&root);
        assert_eq!(lines.len(), 400);
        let mut displays = lines
            .iter()
            .map(|line| serde_json::from_str::<StoredEntry>(line).unwrap().display)
            .collect::<Vec<_>>();
        displays.sort();
        displays.dedup();
        assert_eq!(displays.len(), 400);
        assert!(!root.join(format!("{HISTORY_FILE_NAME}.lock")).exists());
    }

    #[test]
    fn a_stale_lock_is_replaced_and_a_held_lock_delays_the_append_until_release() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        let lock_directory = root.join(format!("{HISTORY_FILE_NAME}.lock"));
        fs::create_dir_all(&lock_directory).unwrap();
        File::open(&lock_directory)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        {
            let mut history = InputHistory::open(&root);
            record(&mut history, "chat-a", project.path(), "after stale lock");
        }
        assert_eq!(history_lines(&root).len(), 1);

        fs::create_dir(&lock_directory).unwrap();
        let mut history = InputHistory::open(&root);
        record(&mut history, "chat-a", project.path(), "while locked");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(history_lines(&root).len(), 1);
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["while locked", "after stale lock"]
        );
        fs::remove_dir(&lock_directory).unwrap();
        history.flush();
        assert_eq!(history_lines(&root).len(), 2);
    }

    #[test]
    fn up_arrow_prompts_stay_in_the_current_project_and_list_the_current_chat_first() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        let mut history = InputHistory::open(&root);
        record(&mut history, "chat-a", project.path(), "mine older");
        record(&mut history, "chat-b", project.path(), "other chat newer");
        record(&mut history, "chat-a", project.path(), "mine newer");
        record(
            &mut history,
            "chat-c",
            elsewhere.path(),
            "other project newest",
        );
        record(&mut history, "chat-a", project.path(), "!ls");
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["mine newer", "mine older", "other chat newer"]
        );

        for index in 0..150 {
            record(
                &mut history,
                "chat-b",
                project.path(),
                &format!("bulk {index}"),
            );
        }
        let recent = history.recent_prompts("chat-a", project.path()).unwrap();
        assert_eq!(recent.len(), MAX_HISTORY_ITEMS);
        assert_eq!(recent[0], "bulk 149");
    }

    #[test]
    fn large_pastes_round_trip_through_the_paste_cache_and_small_ones_stay_inline() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        let large = format!(
            "大段粘贴\n{}\n结束",
            "x".repeat(MAX_INLINE_PASTED_CHARACTERS)
        );
        let small = "small\npaste\nwith\nlines";
        let prompt = format!("Review {large} and {small} please");
        let large_start = "Review ".len();
        let small_start = prompt.find(small).unwrap();
        let pasted_texts = [
            PastedText {
                id: 2,
                range: small_start..small_start + small.len(),
            },
            PastedText {
                id: 1,
                range: large_start..large_start + large.len(),
            },
        ];
        {
            let mut history = InputHistory::open(&root);
            assert!(
                history
                    .record("chat-a", project.path(), &prompt, &pasted_texts)
                    .unwrap()
            );
        }
        assert_eq!(
            fs::read_dir(root.join(PASTE_CACHE_DIRECTORY))
                .unwrap()
                .count(),
            1
        );

        let history = InputHistory::open(&root);
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            [prompt.as_str()]
        );
        let matches = history
            .search("chat-a", project.path(), HistoryScope::ThisSession, "", 10)
            .unwrap();
        assert_eq!(
            matches[0].display(),
            "Review [Pasted text #1 +2 lines] and [Pasted text #2 +3 lines] please"
        );
        assert_eq!(history.resolve_match(&matches[0]), prompt);
    }

    #[test]
    fn scopes_isolate_two_chats_and_two_working_directories() {
        let directory = tempfile::tempdir().unwrap();
        let first_project = tempfile::tempdir().unwrap();
        let second_project = tempfile::tempdir().unwrap();
        let mut history = InputHistory::open(&history_root(&directory));
        record(
            &mut history,
            "chat-a",
            first_project.path(),
            "alpha current",
        );
        record(
            &mut history,
            "chat-b",
            first_project.path(),
            "alpha earlier",
        );
        record(
            &mut history,
            "chat-c",
            second_project.path(),
            "beta elsewhere",
        );

        let chat = history
            .search(
                "chat-a",
                first_project.path(),
                HistoryScope::ThisSession,
                "",
                10,
            )
            .unwrap();
        assert_eq!(displays(&chat), ["alpha current"]);

        let project = history
            .search(
                "chat-a",
                first_project.path(),
                HistoryScope::ThisProject,
                "",
                10,
            )
            .unwrap();
        assert_eq!(displays(&project), ["alpha earlier", "alpha current"]);

        let all = history
            .search(
                "chat-a",
                first_project.path(),
                HistoryScope::AllProjects,
                "",
                10,
            )
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].display(), "beta elsewhere");
    }

    #[test]
    fn same_prompt_is_available_in_each_chat() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut history = InputHistory::open(&history_root(&directory));
        assert!(record(&mut history, "chat-a", project.path(), "same"));
        assert!(record(&mut history, "chat-b", project.path(), "same"));
        assert_eq!(
            history
                .search("chat-b", project.path(), HistoryScope::ThisSession, "", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn search_candidates_are_limited_to_the_newest_100_distinct_displays() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut history = InputHistory::open(&history_root(&directory));
        for index in 0..150 {
            record(
                &mut history,
                "chat-a",
                project.path(),
                &format!("entry {index}"),
            );
            record(
                &mut history,
                "chat-b",
                project.path(),
                &format!("entry {index}"),
            );
        }
        let matches = history
            .search("chat-a", project.path(), HistoryScope::ThisProject, "", 500)
            .unwrap();
        assert_eq!(matches.len(), MAX_HISTORY_ITEMS);
        assert_eq!(matches[0].display(), "entry 149");
        assert_eq!(matches[99].display(), "entry 50");
    }

    #[test]
    fn substring_and_fuzzy_results_have_stable_relevance_and_recency_order() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut history = InputHistory::open(&history_root(&directory));
        for prompt in [
            "deploy production",
            "please deploy now",
            "debug local process",
            "deploy preview",
            "deploy",
        ] {
            record(&mut history, "chat-a", project.path(), prompt);
        }

        let matches = history
            .search(
                "chat-a",
                project.path(),
                HistoryScope::ThisSession,
                "deploy",
                10,
            )
            .unwrap();
        assert_eq!(
            displays(&matches),
            [
                "deploy",
                "deploy preview",
                "deploy production",
                "please deploy now"
            ]
        );

        let fuzzy = history
            .search(
                "chat-a",
                project.path(),
                HistoryScope::ThisSession,
                "dlp",
                10,
            )
            .unwrap();
        assert_eq!(fuzzy[0].display(), "deploy preview");
        assert!(
            fuzzy
                .iter()
                .any(|entry| entry.display() == "debug local process")
        );
    }

    #[test]
    fn chinese_queries_and_multiline_prompts_round_trip_across_instances() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        {
            let mut history = InputHistory::open(&root);
            record(
                &mut history,
                "中文会话",
                project.path(),
                "查看这个目录\n并总结文件",
            );
            record(&mut history, "中文会话", project.path(), "查看持仓");
        }

        let history = InputHistory::open(&root);
        let matches = history
            .search(
                "中文会话",
                project.path(),
                HistoryScope::ThisSession,
                "目录总结",
                10,
            )
            .unwrap();
        assert_eq!(displays(&matches), ["查看这个目录\n并总结文件"]);
    }

    #[test]
    fn corrupt_lines_and_control_characters_cannot_reach_the_ui() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        {
            let mut history = InputHistory::open(&root);
            record(
                &mut history,
                "chat-a",
                project.path(),
                "safe\u{1b}[31m\0text\nnext\tcolumn",
            );
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(root.join(HISTORY_FILE_NAME))
            .unwrap();
        file.write_all(b"{\"display\":\"safe truncated").unwrap();
        file.write_all(b"\n\x80\x81 not json\n").unwrap();
        let project_key = project_key(project.path());
        let control = serde_json::json!({
            "display": "safe\u{7}bell",
            "pastedContents": {},
            "timestamp": 1,
            "project": project_key,
            "chatId": "chat-a",
        });
        writeln!(file, "{control}").unwrap();
        drop(file);

        let history = InputHistory::open(&root);
        let matches = history
            .search(
                "chat-a",
                project.path(),
                HistoryScope::ThisSession,
                "safe",
                10,
            )
            .unwrap();
        assert_eq!(
            displays(&matches),
            ["safebell", "safe[31mtext\nnext\tcolumn"]
        );
        assert!(matches.iter().all(|candidate| {
            candidate
                .display()
                .chars()
                .all(|character| !character.is_control() || character == '\n' || character == '\t')
        }));
    }

    #[test]
    fn dialog_cycles_scopes_selects_and_accepts_with_enter_or_tab() {
        let directory = tempfile::tempdir().unwrap();
        let first_project = tempfile::tempdir().unwrap();
        let second_project = tempfile::tempdir().unwrap();
        let mut history = InputHistory::open(&history_root(&directory));
        record(
            &mut history,
            "chat-a",
            first_project.path(),
            "session result",
        );
        record(
            &mut history,
            "chat-b",
            first_project.path(),
            "project result",
        );
        record(
            &mut history,
            "chat-c",
            second_project.path(),
            "global result",
        );

        let mut search = HistorySearch::open(
            &history,
            "chat-a",
            first_project.path(),
            "draft",
            3,
            "result",
            10,
        )
        .unwrap();
        assert_eq!(search.scope(), HistoryScope::ThisSession);
        assert_eq!(search.matches().len(), 1);
        assert_eq!(search.selected(), Some(0));

        search
            .apply(&history, HistorySearchInput::CycleScope)
            .unwrap();
        assert_eq!(search.scope(), HistoryScope::ThisProject);
        assert_eq!(search.matches().len(), 2);
        search.apply(&history, HistorySearchInput::Next).unwrap();
        assert_eq!(search.selected_match().unwrap().display(), "session result");
        search
            .apply(&history, HistorySearchInput::Previous)
            .unwrap();
        assert_eq!(search.selected_match().unwrap().display(), "project result");
        search.apply(&history, HistorySearchInput::Next).unwrap();
        assert_eq!(
            search
                .apply(&history, HistorySearchInput::AcceptEnter)
                .unwrap(),
            HistorySearchOutcome::Accepted(HistoryDraft {
                text: "session result".into(),
                cursor: "session result".len(),
            })
        );

        search
            .apply(&history, HistorySearchInput::CycleScope)
            .unwrap();
        assert_eq!(search.scope().label(), "All projects");
        assert_eq!(search.matches().len(), 3);
        assert!(matches!(
            search
                .apply(&history, HistorySearchInput::AcceptTab)
                .unwrap(),
            HistorySearchOutcome::Accepted(_)
        ));
    }

    #[test]
    fn query_editing_refreshes_matches_and_cancel_restores_draft_and_cursor() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut history = InputHistory::open(&history_root(&directory));
        record(&mut history, "chat-a", project.path(), "你好世界");
        record(&mut history, "chat-a", project.path(), "other");

        let mut search = HistorySearch::open(
            &history,
            "chat-a",
            project.path(),
            "草稿内容",
            "草稿".len(),
            "",
            10,
        )
        .unwrap();
        search
            .apply(&history, HistorySearchInput::Insert("你\u{1b}".into()))
            .unwrap();
        search
            .apply(&history, HistorySearchInput::Insert("好".into()))
            .unwrap();
        assert_eq!(search.query(), "你好");
        assert_eq!(search.matches().len(), 1);
        search
            .apply(&history, HistorySearchInput::Backspace)
            .unwrap();
        assert_eq!(search.query(), "你");

        assert_eq!(
            search.apply(&history, HistorySearchInput::Cancel).unwrap(),
            HistorySearchOutcome::Cancelled(HistoryDraft {
                text: "草稿内容".into(),
                cursor: "草稿".len(),
            })
        );
    }

    #[test]
    fn invalid_utf8_cursor_is_floored_when_dialog_opens() {
        let history = InputHistory::in_memory();
        let project = tempfile::tempdir().unwrap();
        let search =
            HistorySearch::open(&history, "chat-a", project.path(), "你好", 2, "", 10).unwrap();
        assert_eq!(search.original.cursor, 0);
    }

    fn stored_entry(display: &str, cwd: &Path, chat_id: &str) -> StoredEntry {
        StoredEntry {
            display: display.into(),
            pasted_contents: BTreeMap::new(),
            timestamp: 1,
            project: project_key(cwd),
            chat_id: chat_id.into(),
        }
    }

    fn append_bytes(path: &Path, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn parsed_history_is_reused_until_the_file_grows_and_then_only_new_lines_are_parsed() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        let history_file = root.join(HISTORY_FILE_NAME);
        {
            let mut writer = InputHistory::open(&root);
            record(&mut writer, "chat-a", project.path(), "older");
        }
        let history = InputHistory::open(&root);
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["older"]
        );

        // 同长度改写已解析的行并还原修改时间：未重读文件，仍返回缓存中的条目。
        let length = fs::metadata(&history_file).unwrap().len() as usize;
        let modified = fs::metadata(&history_file).unwrap().modified().unwrap();
        let mut unreadable = vec![b'x'; length - 1];
        unreadable.push(b'\n');
        fs::write(&history_file, unreadable).unwrap();
        File::options()
            .write(true)
            .open(&history_file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["older"]
        );

        {
            let mut writer = InputHistory::open(&root);
            record(&mut writer, "chat-a", project.path(), "newer");
        }
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["newer", "older"]
        );
        assert_eq!(
            InputHistory::open(&root)
                .recent_prompts("chat-a", project.path())
                .unwrap(),
            ["newer"]
        );

        fs::write(&history_file, b"").unwrap();
        assert!(
            history
                .recent_prompts("chat-a", project.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_unterminated_tail_is_read_exactly_once_after_its_line_is_completed() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        let history_file = root.join(HISTORY_FILE_NAME);
        {
            let mut writer = InputHistory::open(&root);
            record(&mut writer, "chat-a", project.path(), "first");
        }
        let history = InputHistory::open(&root);
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["first"]
        );

        let line =
            serde_json::to_string(&stored_entry("second", project.path(), "chat-a")).unwrap();
        let (head, tail) = line.split_at(line.len() / 2);
        append_bytes(&history_file, head.as_bytes());
        assert_eq!(
            history.recent_prompts("chat-a", project.path()).unwrap(),
            ["first"]
        );
        append_bytes(&history_file, format!("{tail}\n").as_bytes());
        for _ in 0..2 {
            assert_eq!(
                history.recent_prompts("chat-a", project.path()).unwrap(),
                ["second", "first"]
            );
        }
    }

    #[test]
    fn lines_longer_than_a_read_chunk_are_loaded_backwards_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        let long_prompts =
            ["a", "b", "c"].map(|letter| letter.repeat(REVERSE_READ_CHUNK_BYTES as usize * 3 / 2));
        {
            let mut writer = InputHistory::open(&root);
            for prompt in &long_prompts {
                record(&mut writer, "chat-a", project.path(), prompt);
            }
        }
        let recent = InputHistory::open(&root)
            .recent_prompts("chat-a", project.path())
            .unwrap();
        assert_eq!(
            recent,
            [
                long_prompts[2].clone(),
                long_prompts[1].clone(),
                long_prompts[0].clone()
            ]
        );
    }

    #[test]
    fn shell_commands_follow_the_project_window_and_candidates_resolve_pastes_on_demand() {
        let directory = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let root = history_root(&directory);
        let mut history = InputHistory::open(&root);
        record(&mut history, "chat-b", project.path(), "!make");
        record(&mut history, "chat-a", project.path(), "!cargo test");
        record(&mut history, "chat-a", project.path(), "plain prompt");
        record(&mut history, "chat-a", project.path(), "!git status");
        record(&mut history, "chat-a", project.path(), "!  cargo test  ");
        record(&mut history, "chat-a", project.path(), "!");
        record(&mut history, "chat-c", elsewhere.path(), "!elsewhere");
        assert_eq!(
            history
                .recent_shell_commands("chat-a", project.path())
                .unwrap(),
            ["cargo test", "git status", "make"]
        );

        for index in 0..60 {
            record(
                &mut history,
                "chat-a",
                project.path(),
                &format!("!echo {index}"),
            );
        }
        let commands = history
            .recent_shell_commands("chat-a", project.path())
            .unwrap();
        assert_eq!(commands.len(), MAX_SHELL_HISTORY_COMMANDS);
        assert_eq!(commands[0], "echo 59");

        let pasted = "p".repeat(MAX_INLINE_PASTED_CHARACTERS + 1);
        let prompt = format!("Review {pasted}");
        let pasted_texts = [PastedText {
            id: 1,
            range: "Review ".len()..prompt.len(),
        }];
        history
            .record("chat-a", project.path(), &prompt, &pasted_texts)
            .unwrap();
        history.flush();
        let paste_cache = root.join(PASTE_CACHE_DIRECTORY);
        let moved_cache = directory.path().join("moved-paste-cache");
        fs::rename(&paste_cache, &moved_cache).unwrap();
        let history = InputHistory::open(&root);
        let matches = history
            .search(
                "chat-a",
                project.path(),
                HistoryScope::ThisSession,
                "Review",
                10,
            )
            .unwrap();
        assert_eq!(matches[0].display(), "Review [Pasted text #1]");
        fs::rename(&moved_cache, &paste_cache).unwrap();
        assert_eq!(history.resolve_match(&matches[0]), prompt);
    }

    struct FailingAppendTarget {
        bytes: Vec<u8>,
        accepted_length: usize,
    }

    impl Write for FailingAppendTarget {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let room = self.accepted_length.saturating_sub(self.bytes.len());
            if room == 0 {
                return Err(io::Error::other("disk full"));
            }
            let written = room.min(buffer.len());
            self.bytes.extend_from_slice(&buffer[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl HistoryAppendTarget for FailingAppendTarget {
        fn length(&self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn truncate(&mut self, length: u64) -> io::Result<()> {
            self.bytes.truncate(length as usize);
            Ok(())
        }
    }

    #[test]
    fn a_partially_written_batch_is_dropped_without_leaving_broken_or_repeated_lines() {
        let project = tempfile::tempdir().unwrap();
        let existing = serde_json::to_string(&stored_entry("existing", project.path(), "chat-a"))
            .unwrap()
            + "\n";
        let mut target = FailingAppendTarget {
            bytes: existing.clone().into_bytes(),
            accepted_length: existing.len() + 10,
        };
        let mut pending = vec![
            stored_entry("lost one", project.path(), "chat-a"),
            stored_entry("lost two", project.path(), "chat-a"),
        ];
        assert!(append_batch(&mut target, &mut pending).is_err());
        assert!(pending.is_empty());
        assert_eq!(target.bytes, existing.as_bytes());

        target.accepted_length = usize::MAX;
        pending.push(stored_entry("kept", project.path(), "chat-a"));
        append_batch(&mut target, &mut pending).unwrap();
        let displays = target
            .bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| parse_entry(line).unwrap().display)
            .collect::<Vec<_>>();
        assert_eq!(displays, ["existing", "kept"]);
    }
}
