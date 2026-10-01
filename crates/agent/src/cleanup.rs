//! 过期文件清理，照 Claude Code `utils/cleanup.ts`。
//!
//! 按修改时间删除清理天数之前的文件：各项目目录中的 Transcript、Chat 目录中的大工具结果、
//! 计划文件、文件回滚备份目录、图片缓存目录与粘贴缓存，之后尝试删除空目录。系统临时目录中的后台
//! 任务输出、输入历史、进程登记与设置文件不清理（与 Claude Code 一致）。
//!
//! 与 Claude Code 一样按修改时间判断，不检查文件是否正被使用：正在对话的 Chat 每次冲刷都会
//! 更新 Transcript 的修改时间，因此当前 Chat（无论属于本进程还是其他进程）的记录不会过期。
//! 在此之上，文件回滚备份目录、图片缓存目录与计划文件不随对话更新修改时间（复用备份不改目录、
//! 图片只在粘贴时写入、计划只在写计划时保存），长期运行的 Loop 会让它们先于 Transcript 过期；因此
//! Transcript 仍未过期的 Chat 保留这些文件（Claude Code 没有这一层）。
//!
//! 只认 Tick Loop 自己写出的名称：`<chatId>.jsonl` 的 Transcript、`<chatId>` 命名的 Chat 目录、文件回滚
//! 备份目录与图片缓存目录（连字符分隔的小写 UUID），防止配置目录被误设为主目录这类情况下删除用户
//! 同名目录中的文件。
//! 目录项的类型取自不跟随符号链接的 `lstat`：符号链接既不是文件也不是目录，一律跳过，链接与其
//! 指向的内容都不删除；删除目录树用的 `remove_dir_all` 同样只删除链接本身。

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use uuid::Uuid;

use crate::file_history::FILE_HISTORY_DIRECTORY;
use crate::runtime::{
    MAX_PLAN_SLUG_BYTES, PLAN_FILE_EXTENSION, PLANS_DIRECTORY, TOOL_RESULTS_SUBDIRECTORY,
    is_valid_plan_slug, plan_file_slug,
};
use crate::settings::{CleanupPeriod, UserSettings};
use crate::terminal::{IMAGE_CACHE_DIRECTORY, PASTE_CACHE_DIRECTORY, PASTE_FILE_EXTENSION};
use crate::transcript::{LITE_READ_BUF_SIZE, PROJECTS_DIRECTORY, TRANSCRIPT_FILE_SUFFIX};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
/// 链上条目记录计划文件名前缀的字段（`transcript::entry` 的 `slug`）。
const SLUG_FIELD_PATTERN: &[u8] = b"\"slug\":\"";
/// 照 Transcript 列表的 `LITE_READ_BUF_SIZE`，先读头尾各 64KB 查找 slug。
const TRANSCRIPT_EDGE_BYTES: u64 = LITE_READ_BUF_SIZE as u64;
/// 头尾都没有 slug 时流式扫描整个文件的上限；超过时无法确定该 Chat 的计划文件，本次不删除计划文件。
const MAX_SLUG_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const SLUG_SCAN_CHUNK_BYTES: usize = 1024 * 1024;
const DENIAL_FILE_SUFFIX: &str = ".denials.json";

/// 照 Claude Code `CleanupResult`：`messages` 为删除的文件或目录数，`errors` 为删除失败数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupResult {
    pub messages: usize,
    pub errors: usize,
}

impl CleanupResult {
    fn add(&mut self, other: CleanupResult) {
        self.messages += other.messages;
        self.errors += other.errors;
    }

    fn record(&mut self, outcome: io::Result<bool>) {
        match outcome {
            Ok(true) => self.messages += 1,
            Ok(false) => {}
            Err(_) => self.errors += 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupOutcome {
    Completed(CleanupResult),
    /// 设置文件有问题且显式配置了清理天数，没有清理任何文件。
    Skipped,
}

/// 照 Claude Code `cleanupOldMessageFilesInBackground`：读取 `<root>/settings.json` 的清理天数后依次
/// 清理各类文件。设置有问题且显式配置了清理天数时跳过，不回退到默认天数。阻塞调用。
pub fn cleanup_old_message_files_in_background(root: &Path) -> CleanupOutcome {
    let days = match UserSettings::in_root(root).cleanup_period_for_cleanup() {
        CleanupPeriod::Days(days) => days,
        CleanupPeriod::SkipInvalidSettings => return CleanupOutcome::Skipped,
    };
    let cutoff = cutoff_date(days);
    let (mut result, active_chats) = cleanup_old_transcript_files(root, cutoff);
    result.add(cleanup_old_plan_files(root, cutoff, &active_chats));
    result.add(cleanup_old_file_history_backups(
        root,
        cutoff,
        &active_chats,
    ));
    result.add(cleanup_old_image_caches(root, cutoff, &active_chats));
    result.add(cleanup_old_pastes(root, cutoff));
    CleanupOutcome::Completed(result)
}

/// 照 Claude Code `getCutoffDate`；天数过大时截断到 Unix 纪元，不删除任何文件。
fn cutoff_date(days: u64) -> SystemTime {
    Duration::from_secs(days)
        .checked_mul(SECONDS_PER_DAY as u32)
        .and_then(|period| SystemTime::now().checked_sub(period))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Transcript 仍未过期（或删除失败）的 Chat，以及这些 Transcript 中记录的计划文件名前缀。
#[derive(Default)]
struct ActiveChats {
    chat_ids: HashSet<Uuid>,
    /// `None`：某个 Transcript 读取失败或超过扫描上限，无法确定全部仍在使用的前缀。
    plan_slugs: Option<HashSet<String>>,
}

impl ActiveChats {
    fn new() -> Self {
        Self {
            chat_ids: HashSet::new(),
            plan_slugs: Some(HashSet::new()),
        }
    }

    fn record(&mut self, chat_id: Uuid, transcript: &Path) {
        self.chat_ids.insert(chat_id);
        let Some(known_slugs) = &mut self.plan_slugs else {
            return;
        };
        match plan_slugs_in_transcript(transcript) {
            Ok(Some(slugs)) => known_slugs.extend(slugs),
            Ok(None) | Err(_) => self.plan_slugs = None,
        }
    }

    fn keeps_plan(&self, slug: &str) -> bool {
        self.plan_slugs
            .as_ref()
            .is_none_or(|slugs| slugs.contains(slug))
    }
}

/// 照 Claude Code `cleanupOldSessionFiles`：每个项目目录中删除过期的 `<chatId>.jsonl`；`<chatId>` Chat 目录中
/// 只清理 `tool-results/` 下的文件及其下一层目录中的文件，再依次尝试删除空的工具目录、`tool-results/`、
/// Chat 目录与项目目录。同时收集 Transcript 仍保留的 Chat。
fn cleanup_old_transcript_files(root: &Path, cutoff: SystemTime) -> (CleanupResult, ActiveChats) {
    let mut result = CleanupResult::default();
    let mut active_chats = ActiveChats::new();
    let Some(project_entries) = read_real_directory(&root.join(PROJECTS_DIRECTORY)) else {
        return (result, active_chats);
    };
    for project_entry in project_entries {
        if !project_entry.is_directory {
            continue;
        }
        let project_directory = project_entry.path;
        let Some(entries) = read_real_directory(&project_directory) else {
            result.errors += 1;
            continue;
        };
        let mut denial_files = Vec::new();
        for entry in entries {
            if entry.is_file {
                if chat_id_with_suffix(&entry.path, DENIAL_FILE_SUFFIX).is_some() {
                    denial_files.push(entry.path);
                    continue;
                }
                let Some(chat_id) = chat_id_with_suffix(&entry.path, TRANSCRIPT_FILE_SUFFIX) else {
                    continue;
                };
                let outcome = unlink_if_old(&entry.path, cutoff);
                if !matches!(outcome, Ok(true)) {
                    active_chats.record(chat_id, &entry.path);
                }
                result.record(outcome);
            } else if entry.is_directory && chat_id_with_suffix(&entry.path, "").is_some() {
                let chat_directory = entry.path;
                let tool_results_directory = chat_directory.join(TOOL_RESULTS_SUBDIRECTORY);
                let Some(tool_entries) = read_real_directory(&tool_results_directory) else {
                    try_rmdir(&chat_directory);
                    continue;
                };
                for tool_entry in tool_entries {
                    if tool_entry.is_file {
                        result.record(unlink_if_old(&tool_entry.path, cutoff));
                    } else if tool_entry.is_directory {
                        let Some(tool_files) = read_real_directory(&tool_entry.path) else {
                            continue;
                        };
                        for tool_file in tool_files.into_iter().filter(|file| file.is_file) {
                            result.record(unlink_if_old(&tool_file.path, cutoff));
                        }
                        try_rmdir(&tool_entry.path);
                    }
                }
                try_rmdir(&tool_results_directory);
                try_rmdir(&chat_directory);
            }
        }
        for path in denial_files {
            let chat_id = chat_id_with_suffix(&path, DENIAL_FILE_SUFFIX).unwrap();
            let transcript = project_directory.join(format!("{chat_id}{TRANSCRIPT_FILE_SUFFIX}"));
            if !fs::symlink_metadata(&transcript)
                .is_ok_and(|metadata| metadata.file_type().is_file())
            {
                result.record(fs::remove_file(&path).map(|()| true));
            }
        }
        try_rmdir(&project_directory);
    }
    (result, active_chats)
}

/// 照 Claude Code `cleanupOldPlanFiles`：`<root>/plans/*.md`，之后尝试删除空的计划目录。
/// `<slug>-loop-<loopId>.md` 的 slug 仍记录在未过期 Transcript 中时保留。
fn cleanup_old_plan_files(
    root: &Path,
    cutoff: SystemTime,
    active_chats: &ActiveChats,
) -> CleanupResult {
    let plans_directory = root.join(PLANS_DIRECTORY);
    let mut result = CleanupResult::default();
    if let Some(entries) = read_real_directory(&plans_directory) {
        for entry in entries {
            if !entry.is_file || !has_extension(&entry.path, PLAN_FILE_EXTENSION) {
                continue;
            }
            if entry
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(plan_file_slug)
                .is_some_and(|slug| active_chats.keeps_plan(slug))
            {
                continue;
            }
            result.record(unlink_if_old(&entry.path, cutoff));
        }
    }
    try_rmdir(&plans_directory);
    result
}

/// 照 Claude Code `cleanupOldFileHistoryBackups`：按目录本身的修改时间删除整个
/// `<root>/file-history/<chatId>/`，之后尝试删除空的 `file-history/`。Transcript 未过期的 Chat 保留。
fn cleanup_old_file_history_backups(
    root: &Path,
    cutoff: SystemTime,
    active_chats: &ActiveChats,
) -> CleanupResult {
    cleanup_old_chat_directories(&root.join(FILE_HISTORY_DIRECTORY), cutoff, active_chats)
}

/// `<root>/image-cache/<chatId>/` 照文件回滚备份清理。Claude Code `cleanupOldImageCaches` 启动时删除
/// 当前进程以外的全部目录；Tick Loop 一个进程承载多个 Chat 且允许多进程并存，照搬会删除其他运行中
/// 进程的缓存，因此改为随该 Chat 的 Transcript 过期。
fn cleanup_old_image_caches(
    root: &Path,
    cutoff: SystemTime,
    active_chats: &ActiveChats,
) -> CleanupResult {
    cleanup_old_chat_directories(&root.join(IMAGE_CACHE_DIRECTORY), cutoff, active_chats)
}

/// 按目录本身的修改时间删除 `directory` 下过期的 `<chatId>/` 目录树，Transcript 未过期的 Chat 保留，
/// 之后尝试删除空的 `directory`。
fn cleanup_old_chat_directories(
    directory: &Path,
    cutoff: SystemTime,
    active_chats: &ActiveChats,
) -> CleanupResult {
    let mut result = CleanupResult::default();
    let Some(entries) = read_real_directory(directory) else {
        return result;
    };
    let expirable = entries.into_iter().filter(|entry| {
        entry.is_directory
            && chat_id_with_suffix(&entry.path, "")
                .is_some_and(|chat_id| !active_chats.chat_ids.contains(&chat_id))
    });
    for entry in expirable {
        result.record(is_old(&entry.path, cutoff).and_then(|old| {
            if old {
                fs::remove_dir_all(&entry.path)?;
            }
            Ok(old)
        }));
    }
    try_rmdir(directory);
    result
}

/// 照 Claude Code `cleanupOldPastes`：`<root>/paste-cache/*.txt`；粘贴缓存目录保留。
fn cleanup_old_pastes(root: &Path, cutoff: SystemTime) -> CleanupResult {
    cleanup_single_directory(
        &root.join(PASTE_CACHE_DIRECTORY),
        PASTE_FILE_EXTENSION,
        cutoff,
    )
}

/// 文件名为 `<chatId><suffix>` 时返回 Chat ID；只接受 Tick Loop 写出的连字符分隔小写 UUID。
fn chat_id_with_suffix(path: &Path, suffix: &str) -> Option<Uuid> {
    let name = path.file_name()?.to_str()?.strip_suffix(suffix)?;
    Uuid::try_parse(name)
        .ok()
        .filter(|chat_id| chat_id.hyphenated().to_string() == name)
}

/// 收集 Transcript 中记录的计划文件名前缀。slug 生成后写在之后的每个链上条目上（同一 Chat 中不变），
/// 通常出现在尾部，因此大文件先读头尾各 64KB；都没有时（例如 Chat 从未写过计划，或尾部被大段非链上
/// 条目占满）再按块流式扫描整个文件，块间保留可能截断一个值的重叠部分。小文件直接整个扫描。文件超过扫描上限时返回 `None`：宁可本次保留全部计划
/// 文件，也不删除仍在使用的计划；这种情况只出现在超大且头尾没有 slug 的 Transcript 上。
fn plan_slugs_in_transcript(path: &Path) -> io::Result<Option<HashSet<String>>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut slugs = HashSet::new();
    if length > 2 * TRANSCRIPT_EDGE_BYTES {
        let mut edge = Vec::new();
        (&mut file)
            .take(TRANSCRIPT_EDGE_BYTES)
            .read_to_end(&mut edge)?;
        collect_plan_slugs(&edge, &mut slugs);
        file.seek(SeekFrom::Start(length - TRANSCRIPT_EDGE_BYTES))?;
        edge.clear();
        (&mut file)
            .take(TRANSCRIPT_EDGE_BYTES)
            .read_to_end(&mut edge)?;
        collect_plan_slugs(&edge, &mut slugs);
        if !slugs.is_empty() {
            return Ok(Some(slugs));
        }
        if length > MAX_SLUG_SCAN_BYTES {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(0))?;
    }
    let boundary_bytes = SLUG_FIELD_PATTERN.len() + MAX_PLAN_SLUG_BYTES + 1;
    let mut window = Vec::with_capacity(SLUG_SCAN_CHUNK_BYTES + boundary_bytes);
    let mut chunk = vec![0; SLUG_SCAN_CHUNK_BYTES];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            return Ok(Some(slugs));
        }
        window.extend_from_slice(&chunk[..read]);
        collect_plan_slugs(&window, &mut slugs);
        window.drain(..window.len().saturating_sub(boundary_bytes));
    }
}

/// 按字节查找 `"slug":"<value>"`，只收集形如 `runtime::plan` 所生成的值（小写字母单词以连字符连接）。
/// 匹配不到结束引号的截断值忽略，由下一块的重叠部分补上。
fn collect_plan_slugs(bytes: &[u8], slugs: &mut HashSet<String>) {
    let mut offset = 0;
    while let Some(found) = find_bytes(&bytes[offset..], SLUG_FIELD_PATTERN) {
        let value_start = offset + found + SLUG_FIELD_PATTERN.len();
        offset = value_start;
        let Some(value_length) = bytes[value_start..].iter().position(|&byte| byte == b'"') else {
            continue;
        };
        if let Ok(value) = std::str::from_utf8(&bytes[value_start..value_start + value_length])
            && is_valid_plan_slug(value)
        {
            slugs.insert(value.to_owned());
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 照 Claude Code `cleanupSingleDirectory`，是否删除空目录由调用方决定。
fn cleanup_single_directory(
    directory: &Path,
    extension: &str,
    cutoff: SystemTime,
) -> CleanupResult {
    let mut result = CleanupResult::default();
    let Some(entries) = read_real_directory(directory) else {
        return result;
    };
    for entry in entries {
        if entry.is_file && has_extension(&entry.path, extension) {
            result.record(unlink_if_old(&entry.path, cutoff));
        }
    }
    result
}

struct DirectoryEntry {
    path: PathBuf,
    is_file: bool,
    is_directory: bool,
}

/// 列出目录项，类型取自 `lstat`。`directory` 本身不存在、不可读或不是真实目录（包括指向目录的
/// 符号链接）时返回 `None`。
fn read_real_directory(directory: &Path) -> Option<Vec<DirectoryEntry>> {
    if !fs::symlink_metadata(directory).ok()?.is_dir() {
        return None;
    }
    let entries = fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            Some(DirectoryEntry {
                path: entry.path(),
                is_file: file_type.is_file(),
                is_directory: file_type.is_dir(),
            })
        })
        .collect();
    Some(entries)
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(extension))
}

fn is_old(path: &Path, cutoff: SystemTime) -> io::Result<bool> {
    Ok(fs::symlink_metadata(path)?.modified()? < cutoff)
}

/// 照 Claude Code `unlinkIfOld`。删除前再次确认仍是普通文件：之后被换成符号链接时 `unlink`
/// 也只会删除链接本身。
fn unlink_if_old(path: &Path, cutoff: SystemTime) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.modified()? >= cutoff {
        return Ok(false);
    }
    fs::remove_file(path)?;
    Ok(true)
}

/// 照 Claude Code `tryRmdir`：目录非空、不存在或不是目录时什么也不做。
fn try_rmdir(directory: &Path) {
    let _ = fs::remove_dir(directory);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_split_across_scan_chunks_is_found() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        let line = br#"{"type":"assistant","slug":"calm-river"}"#;
        let field_offset = line
            .windows(SLUG_FIELD_PATTERN.len())
            .position(|window| window == SLUG_FIELD_PATTERN)
            .unwrap();
        let line_start = SLUG_SCAN_CHUNK_BYTES - field_offset - 4;
        let mut content = vec![b' '; line_start];
        content.extend_from_slice(line);
        content.resize(3 * SLUG_SCAN_CHUNK_BYTES, b' ');
        std::fs::write(&path, content).unwrap();

        let slugs = plan_slugs_in_transcript(&path).unwrap().unwrap();

        assert_eq!(slugs, HashSet::from(["calm-river".to_owned()]));
    }

    #[test]
    fn a_transcript_beyond_the_scan_limit_without_slugs_at_its_edges_is_inconclusive() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        File::create(&path)
            .unwrap()
            .set_len(MAX_SLUG_SCAN_BYTES + 1)
            .unwrap();

        assert_eq!(plan_slugs_in_transcript(&path).unwrap(), None);
    }
}
