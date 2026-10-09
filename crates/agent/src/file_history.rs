//! 文件回滚备份（file-history），照 Claude Code `utils/fileHistory.ts` 实现。
//!
//! Agent 编辑文件之前为未跟踪的文件建首版备份；每条用户提示为全部跟踪中的文件建快照
//! （未变化复用上一版，变化则版本加一，已删除记为空），最多保留 100 个。rewind 取目标
//! 消息最后一个快照恢复文件。备份放在 `<root>/file-history/<chatId>/`，文件名为路径的
//! SHA-256 前 16 位十六进制加版本号。与 Claude Code 一样不加文件锁，也不协调多个进程
//! 同时修改同一文件。
//!
//! 与 Claude Code 不同，读写被跟踪的文件时不跟随符号链接：从根目录逐段以 `O_NOFOLLOW` 打开父目录，
//! 在该目录内读取、`unlinkat` 删除，或写临时文件后 `renameat` 替换。文件或其父目录被替换为符号链接、
//! 或不再是普通文件时，快照不备份它，rewind 把它记为失败并跳过，不读入也不改动工作区外的文件。
//! 跟踪键是文件工具解析后的 canonical 路径，逐段检查不会误拒工作目录本身所在的路径。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::private_fs::{
    OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, PrivateDirectory,
    open_without_following_links,
};
use crate::transcript::now_timestamp;

/// 状态根目录（或不持久化时的项目临时目录）下存放各 Chat 备份的目录。
pub(crate) const FILE_HISTORY_DIRECTORY: &str = "file-history";
/// 照 Claude Code `MAX_SNAPSHOTS`。
pub(crate) const MAX_SNAPSHOTS: usize = 100;
const BACKUP_NAME_HASH_HEX_LENGTH: usize = 16;
const FIRST_VERSION: u32 = 1;
/// 恢复时补建的工作区目录照 `create_dir_all` 以 0777 创建，实际权限由 umask 决定。
const WORKSPACE_DIRECTORY_MODE: u32 = 0o777;
const STAGED_FILE_PREFIX: &str = ".tickloop-file-history-";
const PERMISSION_BITS: u32 = 0o7777;
const COMPARE_CHUNK_BYTES: usize = 64 * 1024;

/// 某个文件在一个快照中的版本；`backup_file_name` 为空表示该版本中文件不存在。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileHistoryBackup {
    pub backup_file_name: Option<String>,
    pub version: u32,
    pub backup_time: String,
}

/// 一条用户提示对应的快照。路径是绝对路径（与 Claude Code 缩短为相对工作目录的路径不同，
/// Tick Loop 的工作目录可以用 `/cwd` 改变，绝对路径才能稳定地找回文件）。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileHistorySnapshot {
    pub message_id: Uuid,
    pub tracked_file_backups: BTreeMap<String, FileHistoryBackup>,
    pub timestamp: String,
}

/// 按目标快照恢复文件的结果。单个文件失败时照 Claude Code 跳过该文件继续处理其他文件。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FileRestoreReport {
    pub changed_files: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FileHistoryError {
    #[error("the selected snapshot was not found")]
    SnapshotNotFound,
}

/// 一个 Chat 的文件回滚状态。
#[derive(Clone, Debug)]
pub(crate) struct FileHistory {
    directory: PrivateDirectory,
    snapshots: Vec<FileHistorySnapshot>,
    tracked_files: BTreeSet<String>,
    /// 已随提示追加、备份尚未算出的快照，按提示顺序排列。
    pending_snapshots: Vec<PendingSnapshot>,
}

#[derive(Clone, Debug)]
struct PendingSnapshot {
    message_id: Uuid,
    tracked_files: Vec<String>,
}

/// 在 Chat 锁外为一个待定快照计算备份所需的输入，由 [`FileHistory::next_snapshot_plan`] 给出。
#[derive(Debug)]
pub(crate) struct SnapshotPlan {
    message_id: Uuid,
    directory: PrivateDirectory,
    files: Vec<PlannedFile>,
}

#[derive(Debug)]
struct PlannedFile {
    key: String,
    /// 上一个快照中的版本，用来判断文件是否变化。
    previous: Option<FileHistoryBackup>,
    /// 生成计划时待定快照中的版本；提交时不一致说明期间已被编辑跟踪改写，保留改写后的版本。
    expected: Option<FileHistoryBackup>,
}

/// [`SnapshotPlan::compute`] 的结果，交给 [`FileHistory::commit_snapshot`] 提交。
#[derive(Debug)]
pub(crate) struct ComputedSnapshot {
    message_id: Uuid,
    files: Vec<ComputedFile>,
}

#[derive(Debug)]
struct ComputedFile {
    key: String,
    expected: Option<FileHistoryBackup>,
    resolved: Option<FileHistoryBackup>,
}

impl FileHistory {
    pub(crate) fn new(directory: PrivateDirectory) -> Self {
        Self::restore(directory, Vec::new())
    }

    /// 照 Claude Code `fileHistoryRestoreStateFromLog`：从对话链上的快照重建跟踪集合。
    pub(crate) fn restore(
        directory: PrivateDirectory,
        snapshots: Vec<FileHistorySnapshot>,
    ) -> Self {
        let tracked_files = snapshots
            .iter()
            .flat_map(|snapshot| snapshot.tracked_file_backups.keys().cloned())
            .collect();
        Self {
            directory,
            snapshots,
            tracked_files,
            pending_snapshots: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn directory(&self) -> &Path {
        self.directory.path()
    }

    pub(crate) fn backup_directory(&self) -> &PrivateDirectory {
        &self.directory
    }

    #[cfg(test)]
    pub(crate) fn snapshots(&self) -> &[FileHistorySnapshot] {
        &self.snapshots
    }

    #[cfg(test)]
    pub(crate) fn can_restore(&self, message_id: Uuid) -> bool {
        self.snapshots
            .iter()
            .any(|snapshot| snapshot.message_id == message_id)
    }

    /// 只保留属于 `message_ids` 的快照，用于 Branch 只带走分出点之前的对话。
    pub(crate) fn retain_messages(&mut self, message_ids: &HashSet<Uuid>) {
        self.snapshots
            .retain(|snapshot| message_ids.contains(&snapshot.message_id));
        self.pending_snapshots
            .retain(|pending| message_ids.contains(&pending.message_id));
        self.tracked_files = self
            .snapshots
            .iter()
            .flat_map(|snapshot| snapshot.tracked_file_backups.keys().cloned())
            .collect();
    }

    /// 定向摘要把保留的提示重写成新条目后，快照随提示换用新 id。返回换了 id 且已算出备份的
    /// 快照，调用方以新 id 重新写入 Transcript；待定快照提交时再以新 id 写入。
    pub(crate) fn rename_messages(
        &mut self,
        renamed: &HashMap<Uuid, Uuid>,
    ) -> Vec<FileHistorySnapshot> {
        for pending in &mut self.pending_snapshots {
            if let Some(new_id) = renamed.get(&pending.message_id) {
                pending.message_id = *new_id;
            }
        }
        let pending = self
            .pending_snapshots
            .iter()
            .map(|pending| pending.message_id)
            .collect::<HashSet<_>>();
        self.snapshots
            .iter_mut()
            .filter_map(|snapshot| {
                snapshot.message_id = *renamed.get(&snapshot.message_id)?;
                (!pending.contains(&snapshot.message_id)).then(|| snapshot.clone())
            })
            .collect()
    }

    /// 编辑前是否需要为文件建首版备份。照 Claude Code：还没有任何快照时不跟踪；
    /// 最近一个快照已跟踪该文件时由下一次快照检查变化，不改写首版备份。
    pub(crate) fn needs_backup(&self, path: &Path) -> bool {
        let Some(key) = tracking_key(path) else {
            return false;
        };
        self.snapshots
            .last()
            .is_some_and(|latest| !latest.tracked_file_backups.contains_key(key))
    }

    /// 照 Claude Code `fileHistoryTrackEdit` 的提交阶段：把首版备份补记到最近一个快照。
    /// 返回更新后的快照，调用方以 `isSnapshotUpdate` 写入 Transcript；期间已被其他
    /// 编辑跟踪时返回 `None`。
    pub(crate) fn commit_tracked_edit(
        &mut self,
        path: &Path,
        backup: FileHistoryBackup,
    ) -> Option<FileHistorySnapshot> {
        let key = tracking_key(path)?;
        let latest = self.snapshots.last_mut()?;
        if latest.tracked_file_backups.contains_key(key) {
            return None;
        }
        latest.tracked_file_backups.insert(key.to_owned(), backup);
        self.tracked_files.insert(key.to_owned());
        Some(latest.clone())
    }

    /// 同步完成 `fileHistoryTrackEdit`：需要时建首版备份并补记到最近快照。
    #[cfg(test)]
    pub(crate) fn track_edit(&mut self, path: &Path) -> io::Result<Option<FileHistorySnapshot>> {
        if !self.needs_backup(path) {
            return Ok(None);
        }
        let backup = create_first_backup(&self.directory, path)?;
        Ok(self.commit_tracked_edit(path, backup))
    }

    /// 照 Claude Code `fileHistoryMakeSnapshot` 的开头：随提示在对话中的位置立即追加快照，
    /// 先沿用上一个快照中的版本，由 [`Self::next_snapshot_plan`]、[`SnapshotPlan::compute`]
    /// 与 [`Self::commit_snapshot`] 在锁外算出备份后替换。超过 `MAX_SNAPSHOTS` 时移除最早的快照。
    pub(crate) fn begin_snapshot(&mut self, message_id: Uuid) {
        let latest = self.snapshots.last();
        let tracked_files = latest
            .map(|_| self.tracked_files.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let tracked_file_backups = latest
            .map(|latest| {
                tracked_files
                    .iter()
                    .filter_map(|key| {
                        latest
                            .tracked_file_backups
                            .get(key)
                            .map(|backup| (key.clone(), backup.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.snapshots.push(FileHistorySnapshot {
            message_id,
            tracked_file_backups,
            timestamp: now_timestamp(),
        });
        self.pending_snapshots.push(PendingSnapshot {
            message_id,
            tracked_files,
        });
        if self.snapshots.len() > MAX_SNAPSHOTS {
            let excess = self.snapshots.len() - MAX_SNAPSHOTS;
            self.snapshots.drain(..excess);
            let retained = self
                .snapshots
                .iter()
                .map(|snapshot| snapshot.message_id)
                .collect::<HashSet<_>>();
            self.pending_snapshots
                .retain(|pending| retained.contains(&pending.message_id));
        }
    }

    /// 最早的待定快照的计算计划；快照必须按提示顺序逐个提交，后一个才能以前一个为基准。
    pub(crate) fn next_snapshot_plan(&self) -> Option<SnapshotPlan> {
        let pending = self.pending_snapshots.first()?;
        let index = self
            .snapshots
            .iter()
            .rposition(|snapshot| snapshot.message_id == pending.message_id)?;
        let previous = index
            .checked_sub(1)
            .map(|previous| &self.snapshots[previous]);
        let current = &self.snapshots[index];
        let files = pending
            .tracked_files
            .iter()
            .map(|key| PlannedFile {
                key: key.clone(),
                previous: previous
                    .and_then(|snapshot| snapshot.tracked_file_backups.get(key).cloned()),
                expected: current.tracked_file_backups.get(key).cloned(),
            })
            .collect();
        Some(SnapshotPlan {
            message_id: pending.message_id,
            directory: self.directory.clone(),
            files,
        })
    }

    /// 提交锁外算出的快照。单个文件备份失败时该文件沿用上一个快照中的版本（上一个快照也没有时
    /// 不记入）；计划生成后被编辑跟踪改写的条目保持不变。快照已被淘汰或不再待定时返回 `None`。
    pub(crate) fn commit_snapshot(
        &mut self,
        computed: ComputedSnapshot,
    ) -> Option<FileHistorySnapshot> {
        let position = self
            .pending_snapshots
            .iter()
            .position(|pending| pending.message_id == computed.message_id)?;
        self.pending_snapshots.remove(position);
        let snapshot = self
            .snapshots
            .iter_mut()
            .rev()
            .find(|snapshot| snapshot.message_id == computed.message_id)?;
        for file in computed.files {
            if snapshot.tracked_file_backups.get(&file.key) != file.expected.as_ref() {
                continue;
            }
            match file.resolved {
                Some(backup) => snapshot.tracked_file_backups.insert(file.key, backup),
                None => snapshot.tracked_file_backups.remove(&file.key),
            };
        }
        Some(snapshot.clone())
    }

    /// 同步完成一次 `fileHistoryMakeSnapshot`。
    #[cfg(test)]
    pub(crate) fn make_snapshot(&mut self, message_id: Uuid) -> FileHistorySnapshot {
        self.begin_snapshot(message_id);
        while let Some(plan) = self.next_snapshot_plan() {
            self.commit_snapshot(plan.compute());
        }
        self.snapshots.last().cloned().unwrap()
    }

    /// 照 Claude Code `applySnapshot`：目标为空时删除文件；快照中没有的文件用首版备份；
    /// 内容不同才复制回去。找不到目标消息的快照时报错，不改动任何文件。
    pub(crate) fn rewind(&self, message_id: Uuid) -> Result<FileRestoreReport, FileHistoryError> {
        let target = self.target_snapshot(message_id)?;
        let mut report = FileRestoreReport::default();
        for key in &self.tracked_files {
            let path = PathBuf::from(key);
            let Some(backup_file_name) = self.target_backup_name(target, key) else {
                report
                    .failures
                    .push((path, "no backup of this file can be found".into()));
                continue;
            };
            match self.apply_backup(&path, backup_file_name.as_deref()) {
                Ok(true) => report.changed_files.push(path),
                Ok(false) => {}
                Err(error) => report.failures.push((path, error.to_string())),
            }
        }
        Ok(report)
    }

    /// 照 Claude Code `fileHistoryHasAnyChanges`／`fileHistoryGetDiffStats`：
    /// 恢复到目标消息时会改变的文件，不改动磁盘。没有该消息的快照时返回 `None`。
    /// 无法安全比较的文件（例如已被替换为符号链接）也列出，rewind 时会报告失败。
    pub(crate) fn files_changed_by_rewind(&self, message_id: Uuid) -> Option<Vec<PathBuf>> {
        let target = self.target_snapshot(message_id).ok()?;
        Some(
            self.tracked_files
                .iter()
                .filter(|key| {
                    let path = Path::new(key.as_str());
                    match self.target_backup_name(target, key) {
                        None => false,
                        Some(None) => tracked_entry_may_exist(path),
                        Some(Some(name)) => {
                            match (
                                open_tracked_file(path),
                                open_backup_file(&self.directory, &name),
                            ) {
                                (Ok(mut current), Ok(mut backup)) => {
                                    origin_changed(current.as_mut(), backup.as_mut())
                                }
                                _ => true,
                            }
                        }
                    }
                })
                .map(PathBuf::from)
                .collect(),
        )
    }

    /// 同一组快照换到另一个备份目录，不复制备份文件；与 [`Self::copy_backups_to`] 配合，
    /// 让 Branch 在 Chat 锁内建立状态、在锁外复制备份。
    pub(crate) fn relocated(&self, directory: PrivateDirectory) -> Self {
        Self {
            directory,
            snapshots: self.snapshots.clone(),
            tracked_files: self.tracked_files.clone(),
            pending_snapshots: Vec::new(),
        }
    }

    /// 照 Claude Code `copyFileHistoryForResume`：把备份硬链接到 `directory`，失败时改为复制。
    /// 返回备份全部复制成功、应当写入新 Transcript 的快照。
    pub(crate) fn copy_backups_to(&self, directory: &PrivateDirectory) -> Vec<FileHistorySnapshot> {
        let Ok(target) = open_backup_directory(directory, true) else {
            return Vec::new();
        };
        let source = open_backup_directory(&self.directory, false).ok();
        self.snapshots
            .iter()
            .filter(|snapshot| {
                snapshot
                    .tracked_file_backups
                    .values()
                    .filter_map(|backup| backup.backup_file_name.as_deref())
                    .all(|name| {
                        source
                            .as_ref()
                            .is_some_and(|source| link_or_copy(source, &target, name))
                    })
            })
            .cloned()
            .collect()
    }

    /// [`Self::relocated`] 与 [`Self::copy_backups_to`] 一起完成，用于已在 Chat 锁外、以新 chatId 恢复的 Chat（`--fork-session`）。
    pub(crate) fn copy_to(&self, directory: PrivateDirectory) -> (Self, Vec<FileHistorySnapshot>) {
        let recorded = self.copy_backups_to(&directory);
        (self.relocated(directory), recorded)
    }

    fn target_snapshot(&self, message_id: Uuid) -> Result<&FileHistorySnapshot, FileHistoryError> {
        self.snapshots
            .iter()
            .rev()
            .find(|snapshot| snapshot.message_id == message_id)
            .ok_or(FileHistoryError::SnapshotNotFound)
    }

    /// 目标快照中的备份名；快照未跟踪该文件时照 Claude Code 退回首版备份。
    /// 外层 `None` 表示找不到可用的备份。
    fn target_backup_name(
        &self,
        target: &FileHistorySnapshot,
        key: &str,
    ) -> Option<Option<String>> {
        if let Some(backup) = target.tracked_file_backups.get(key) {
            return Some(backup.backup_file_name.clone());
        }
        self.snapshots.iter().find_map(|snapshot| {
            snapshot
                .tracked_file_backups
                .get(key)
                .filter(|backup| backup.version == FIRST_VERSION)
                .map(|backup| backup.backup_file_name.clone())
        })
    }

    /// 返回 `true` 表示文件被改动。
    fn apply_backup(&self, path: &Path, backup_file_name: Option<&str>) -> io::Result<bool> {
        #[cfg(test)]
        test_pause::wait_if_paused(path);
        let Some(name) = backup_file_name else {
            return remove_tracked_file(path);
        };
        let mut backup = open_backup_file(&self.directory, name)?;
        let mut current = open_tracked_file(path)?;
        if !origin_changed(current.as_mut(), backup.as_mut()) {
            return Ok(false);
        }
        let mut backup = backup
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the backup file is missing"))?;
        backup.file.rewind()?;
        let (parent, file_name) = split_path(path)?;
        let parent_directory = open_directory(parent, Some(WORKSPACE_DIRECTORY_MODE))?;
        // 比较之后文件可能被替换；再确认一次，符号链接或非普通文件不写。
        ensure_regular_or_missing(&parent_directory, &file_name)?;
        publish_copy(
            &parent_directory,
            &file_name,
            &mut backup.file,
            backup.metadata.mode(),
        )?;
        Ok(true)
    }
}

impl SnapshotPlan {
    /// 照 Claude Code `fileHistoryMakeSnapshot` 的逐文件处理：未变化复用上一版，变化则版本加一，
    /// 已删除记为空。读写文件，应在 `spawn_blocking` 中调用。
    pub(crate) fn compute(self) -> ComputedSnapshot {
        let files = self
            .files
            .into_iter()
            .map(|file| {
                let resolved = match next_backup(
                    &self.directory,
                    Path::new(&file.key),
                    file.previous.as_ref(),
                ) {
                    Ok(backup) => Some(backup),
                    Err(_) => file.previous,
                };
                ComputedFile {
                    key: file.key,
                    expected: file.expected,
                    resolved,
                }
            })
            .collect();
        ComputedSnapshot {
            message_id: self.message_id,
            files,
        }
    }
}

/// 只跟踪 UTF-8 路径：快照以 JSON 对象的键保存路径。
fn tracking_key(path: &Path) -> Option<&str> {
    path.to_str()
}

fn backup_file_name(path: &str, version: u32) -> String {
    let digest = format!("{:x}", Sha256::digest(path.as_bytes()));
    format!("{}@v{version}", &digest[..BACKUP_NAME_HASH_HEX_LENGTH])
}

/// 照 Claude Code `fileHistoryTrackEdit` 的备份阶段：为编辑前的文件建首版备份。
pub(crate) fn create_first_backup(
    directory: &PrivateDirectory,
    path: &Path,
) -> io::Result<FileHistoryBackup> {
    create_backup(directory, path, FIRST_VERSION)
}

/// 照 Claude Code `createBackup`：源文件不存在时记为空；否则复制到备份目录并保留权限位。
/// 源文件或其父目录是符号链接、或源文件不是普通文件时返回说明原因的错误，不建备份。
fn create_backup(
    directory: &PrivateDirectory,
    path: &Path,
    version: u32,
) -> io::Result<FileHistoryBackup> {
    let backup_time = now_timestamp();
    match open_tracked_file(path)? {
        Some(mut source) => write_backup(directory, path, version, &mut source, backup_time),
        None => Ok(FileHistoryBackup {
            backup_file_name: None,
            version,
            backup_time,
        }),
    }
}

fn next_backup(
    directory: &PrivateDirectory,
    path: &Path,
    previous: Option<&FileHistoryBackup>,
) -> io::Result<FileHistoryBackup> {
    #[cfg(test)]
    test_pause::wait_if_paused(path);
    let backup_time = now_timestamp();
    let version = previous.map_or(FIRST_VERSION, |backup| backup.version + 1);
    let Some(mut source) = open_tracked_file(path)? else {
        return Ok(FileHistoryBackup {
            backup_file_name: None,
            version,
            backup_time,
        });
    };
    if let Some(previous) = previous
        && let Some(name) = &previous.backup_file_name
        && let Ok(mut backup) = open_backup_file(directory, name)
        && !origin_changed(Some(&mut source), backup.as_mut())
    {
        return Ok(previous.clone());
    }
    source.file.rewind()?;
    write_backup(directory, path, version, &mut source, backup_time)
}

fn write_backup(
    directory: &PrivateDirectory,
    path: &Path,
    version: u32,
    source: &mut OpenedFile,
    backup_time: String,
) -> io::Result<FileHistoryBackup> {
    let key = tracking_key(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file history only tracks UTF-8 paths",
        )
    })?;
    let name = backup_file_name(key, version);
    let backup_directory = open_backup_directory(directory, true)?;
    publish_copy(
        &backup_directory,
        &c_string(name.as_bytes())?,
        &mut source.file,
        source.metadata.mode(),
    )?;
    Ok(FileHistoryBackup {
        backup_file_name: Some(name),
        version,
        backup_time,
    })
}

struct OpenedFile {
    file: File,
    metadata: Metadata,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EntryKind {
    Regular,
    SymbolicLink,
    Other,
}

/// 打开备份目录：从根目录逐段以 `O_NOFOLLOW` 打开，确认每一层都是真实目录，`create` 时以 0700
/// 补建缺失的层级；之后按 [`PrivateDirectory`] 校验（系统临时目录下逐层校验属主与权限位）。
/// `create` 为假且目录不存在时返回 `NotFound`。
fn open_backup_directory(directory: &PrivateDirectory, create: bool) -> io::Result<File> {
    let opened = open_directory(directory.path(), create.then_some(PRIVATE_DIRECTORY_MODE))?;
    directory.create()?;
    Ok(opened)
}

fn open_backup_file(directory: &PrivateDirectory, name: &str) -> io::Result<Option<OpenedFile>> {
    let backup_directory = match open_backup_directory(directory, false) {
        Ok(backup_directory) => backup_directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    open_regular_file_at(&backup_directory, &c_string(name.as_bytes())?)
}

/// 不跟随符号链接地打开被跟踪的文件；文件或父目录不存在时返回 `None`。
fn open_tracked_file(path: &Path) -> io::Result<Option<OpenedFile>> {
    let (parent, name) = split_path(path)?;
    match open_directory(parent, None) {
        Ok(directory) => open_regular_file_at(&directory, &name),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 目标版本为空时 rewind 是否可能删除文件；无法安全判断时按存在处理，由 rewind 报告失败。
fn tracked_entry_may_exist(path: &Path) -> bool {
    let Ok((parent, name)) = split_path(path) else {
        return true;
    };
    match open_directory(parent, None) {
        Ok(directory) => !matches!(entry_kind_at(&directory, &name), Ok(None)),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

/// 照 Claude Code 删除目标版本为空的文件；只删除普通文件，父目录不存在时视为已删除。
fn remove_tracked_file(path: &Path) -> io::Result<bool> {
    let (parent, name) = split_path(path)?;
    let directory = match open_directory(parent, None) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if entry_kind_at(&directory, &name)?.is_none() {
        return Ok(false);
    }
    ensure_regular_or_missing(&directory, &name)?;
    // SAFETY: 目录描述符有效，名称是以 NUL 结尾的单个路径段。
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::NotFound => Ok(false),
            _ => Err(error),
        };
    }
    Ok(true)
}

fn split_path(path: &Path) -> io::Result<(&Path, CString)> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, c_string(name.as_bytes())?)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file history only tracks files with a parent directory",
        )),
    }
}

fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// 从根目录逐段以 `O_NOFOLLOW` 打开目录。`create_mode` 给出时以该模式补建缺失的层级
/// （受 umask 约束），否则缺失时返回 `NotFound`。任何一层是符号链接或不是目录时报错。
fn open_directory(path: &Path, create_mode: Option<u32>) -> io::Result<File> {
    open_without_following_links(path, OpenTarget::Directory { create_mode }).map_err(|error| {
        match error.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => io::Error::other(
                "a parent directory is a symbolic link or not a directory; file history does not follow links",
            ),
            _ => error,
        }
    })
}

fn entry_kind_at(directory: &File, name: &CStr) -> io::Result<Option<EntryKind>> {
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: 目录描述符有效，名称以 NUL 结尾，`status` 足够容纳 `stat` 结构。
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::NotFound => Ok(None),
            _ => Err(error),
        };
    }
    // SAFETY: fstatat 成功时已写满 `status`。
    let file_type = unsafe { status.assume_init() }.st_mode & libc::S_IFMT;
    Ok(Some(match file_type {
        libc::S_IFREG => EntryKind::Regular,
        libc::S_IFLNK => EntryKind::SymbolicLink,
        _ => EntryKind::Other,
    }))
}

fn ensure_regular_or_missing(directory: &File, name: &CStr) -> io::Result<()> {
    match entry_kind_at(directory, name)? {
        None | Some(EntryKind::Regular) => Ok(()),
        Some(EntryKind::SymbolicLink) => Err(symbolic_link_error()),
        Some(EntryKind::Other) => Err(not_regular_error()),
    }
}

fn symbolic_link_error() -> io::Error {
    io::Error::other("the file is a symbolic link; file history does not follow links")
}

fn not_regular_error() -> io::Error {
    io::Error::other("the file is not a regular file")
}

/// 不跟随符号链接地打开目录中的普通文件；不存在时返回 `None`。
fn open_regular_file_at(directory: &File, name: &CStr) -> io::Result<Option<OpenedFile>> {
    match entry_kind_at(directory, name)? {
        None => return Ok(None),
        Some(EntryKind::Regular) => {}
        Some(EntryKind::SymbolicLink) => return Err(symbolic_link_error()),
        Some(EntryKind::Other) => return Err(not_regular_error()),
    }
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: 目录描述符有效，名称是以 NUL 结尾的单个路径段；成功时返回的描述符归 `File` 所有。
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return match (error.kind(), error.raw_os_error()) {
            (io::ErrorKind::NotFound, _) => Ok(None),
            (_, Some(libc::ELOOP)) => Err(symbolic_link_error()),
            _ => Err(error),
        };
    }
    // SAFETY: openat 成功返回的描述符未被其他对象持有，所有权交给 `File`。
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(not_regular_error());
    }
    Ok(Some(OpenedFile { file, metadata }))
}

/// 在目录内写临时文件、设置权限位后 `renameat` 替换 `name`；失败时删除临时文件。
/// 替换的是目录项本身，即使 `name` 此时是符号链接也不会写到链接指向的位置。
fn publish_copy(directory: &File, name: &CStr, source: &mut File, mode: u32) -> io::Result<()> {
    let staged = c_string(format!("{STAGED_FILE_PREFIX}{}", Uuid::new_v4()).as_bytes())?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: 目录描述符有效，名称以 NUL 结尾；成功时返回的描述符归 `File` 所有。
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            staged.as_ptr(),
            flags,
            PRIVATE_FILE_MODE as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat 成功返回的描述符未被其他对象持有，所有权交给 `File`。
    let mut staged_file = unsafe { File::from_raw_fd(descriptor) };
    let published = (|| {
        io::copy(source, &mut staged_file)?;
        // SAFETY: 描述符有效。
        if unsafe {
            libc::fchmod(
                staged_file.as_raw_fd(),
                (mode & PERMISSION_BITS) as libc::mode_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 两个名称都在同一个有效的目录描述符下，且以 NUL 结尾。
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                staged.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if published.is_err() {
        // SAFETY: 目录描述符有效，名称以 NUL 结尾。
        unsafe { libc::unlinkat(directory.as_raw_fd(), staged.as_ptr(), 0) };
    }
    published
}

fn link_or_copy(source: &File, target: &File, name: &str) -> bool {
    let Ok(name) = c_string(name.as_bytes()) else {
        return false;
    };
    // SAFETY: 两个目录描述符有效，名称以 NUL 结尾；flags 为 0 时不跟随符号链接。
    let linked = unsafe {
        libc::linkat(
            source.as_raw_fd(),
            name.as_ptr(),
            target.as_raw_fd(),
            name.as_ptr(),
            0,
        )
    };
    if linked == 0 {
        return true;
    }
    match io::Error::last_os_error().kind() {
        io::ErrorKind::AlreadyExists => true,
        io::ErrorKind::NotFound => false,
        _ => open_regular_file_at(source, &name)
            .ok()
            .flatten()
            .is_some_and(|mut backup| {
                publish_copy(target, &name, &mut backup.file, backup.metadata.mode()).is_ok()
            }),
    }
}

/// 照 Claude Code `checkOriginFileChanged`：一方存在一方缺失算变化；权限或大小不同算变化；
/// 原文件修改时间早于备份时不再比较内容；否则比较内容。
fn origin_changed(original: Option<&mut OpenedFile>, backup: Option<&mut OpenedFile>) -> bool {
    let (original, backup) = match (original, backup) {
        (None, None) => return false,
        (Some(original), Some(backup)) => (original, backup),
        _ => return true,
    };
    if original.metadata.mode() != backup.metadata.mode()
        || original.metadata.len() != backup.metadata.len()
    {
        return true;
    }
    if modified_nanos(&original.metadata) < modified_nanos(&backup.metadata) {
        return false;
    }
    !same_content(&mut original.file, &mut backup.file).unwrap_or(false)
}

fn same_content(left: &mut File, right: &mut File) -> io::Result<bool> {
    let mut left_chunk = vec![0; COMPARE_CHUNK_BYTES];
    let mut right_chunk = vec![0; COMPARE_CHUNK_BYTES];
    loop {
        let left_length = read_chunk(left, &mut left_chunk)?;
        let right_length = read_chunk(right, &mut right_chunk)?;
        if left_chunk[..left_length] != right_chunk[..right_length] {
            return Ok(false);
        }
        if left_length == 0 {
            return Ok(true);
        }
    }
}

/// 读满缓冲区或读到文件末尾。
fn read_chunk(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn modified_nanos(metadata: &Metadata) -> i128 {
    i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

/// 测试接缝：让读写指定路径的 file-history 操作在阻塞线程中停住，直到测试放行。
#[cfg(test)]
pub(crate) mod test_pause {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    /// 测试没有放行时自动放行的期限，出错的测试不会一直挂起。
    const PAUSE_LIMIT: Duration = Duration::from_secs(5);

    type Gate = (mpsc::Sender<()>, mpsc::Receiver<()>);

    static GATES: Mutex<Option<HashMap<PathBuf, Gate>>> = Mutex::new(None);

    /// 一次性暂停；丢弃时放行。
    pub(crate) struct Pause {
        pub(crate) entered: mpsc::Receiver<()>,
        _release: mpsc::Sender<()>,
    }

    pub(crate) fn pause_next(path: &Path) -> Pause {
        let (entered_sender, entered) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        GATES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(path.to_path_buf(), (entered_sender, release_receiver));
        Pause {
            entered,
            _release: release,
        }
    }

    pub(super) fn wait_if_paused(path: &Path) {
        let gate = GATES
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|gates| gates.remove(path));
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.recv_timeout(PAUSE_LIMIT);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn history(root: &Path) -> FileHistory {
        FileHistory::new(PrivateDirectory::trusted(
            root.join(FILE_HISTORY_DIRECTORY).join("chat"),
        ))
    }

    /// 逐段 `O_NOFOLLOW` 打开要求 canonical 路径（macOS 的临时目录位于符号链接 `/var` 之下）。
    fn canonical_temp_directory() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        (directory, root)
    }

    #[test]
    fn backups_follow_claude_code_names_and_keep_permission_bits() {
        let (_directory, root) = canonical_temp_directory();
        let path = root.join("script.sh");
        std::fs::write(&path, "echo one\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let mut history = history(&root);
        history.make_snapshot(Uuid::from_u128(1));

        let updated = history.track_edit(&path).unwrap().unwrap();
        let backup = &updated.tracked_file_backups[path.to_str().unwrap()];
        let digest = format!("{:x}", Sha256::digest(path.to_str().unwrap().as_bytes()));
        assert_eq!(
            backup.backup_file_name.as_deref(),
            Some(format!("{}@v1", &digest[..16]).as_str())
        );
        let backup_path = history
            .directory()
            .join(backup.backup_file_name.as_ref().unwrap());
        assert_eq!(std::fs::read(&backup_path).unwrap(), b"echo one\n");
        assert_eq!(
            std::fs::metadata(&backup_path).unwrap().mode() & 0o777,
            0o751
        );
        assert_eq!(
            std::fs::metadata(history.directory()).unwrap().mode() & 0o777,
            0o700
        );
        assert!(history.track_edit(&path).unwrap().is_none());
    }

    #[test]
    fn edits_without_a_snapshot_are_not_tracked() {
        let (_directory, root) = canonical_temp_directory();
        let path = root.join("untracked.txt");
        let mut history = history(&root);
        assert!(history.track_edit(&path).unwrap().is_none());
        assert!(!history.directory().exists());
    }

    #[test]
    fn snapshots_reuse_unchanged_versions_bump_changed_ones_and_mark_deletions() {
        let (_directory, root) = canonical_temp_directory();
        let [kept, changed, deleted] =
            ["kept.txt", "changed.txt", "deleted.txt"].map(|name| root.join(name));
        for path in [&kept, &changed, &deleted] {
            std::fs::write(path, "original").unwrap();
        }
        let mut history = history(&root);
        history.make_snapshot(Uuid::from_u128(1));
        for path in [&kept, &changed, &deleted] {
            history.track_edit(path).unwrap();
        }
        std::fs::write(&changed, "changed content").unwrap();
        std::fs::remove_file(&deleted).unwrap();

        let snapshot = history.make_snapshot(Uuid::from_u128(2));
        let version =
            |path: &PathBuf| snapshot.tracked_file_backups[path.to_str().unwrap()].clone();
        assert_eq!(version(&kept).version, 1);
        assert_eq!(version(&changed).version, 2);
        assert!(version(&changed).backup_file_name.unwrap().ends_with("@v2"));
        assert_eq!(version(&deleted).version, 2);
        assert!(version(&deleted).backup_file_name.is_none());
    }

    #[test]
    fn pending_snapshots_keep_prompt_order_and_edits_committed_while_computing() {
        let (_directory, root) = canonical_temp_directory();
        let [first, second] = ["first.txt", "second.txt"].map(|name| root.join(name));
        std::fs::write(&first, "one").unwrap();
        let mut history = history(&root);
        history.make_snapshot(Uuid::from_u128(1));
        history.track_edit(&first).unwrap();
        std::fs::write(&first, "two").unwrap();

        history.begin_snapshot(Uuid::from_u128(2));
        history.begin_snapshot(Uuid::from_u128(3));
        let plan = history.next_snapshot_plan().unwrap();
        std::fs::write(&second, "second").unwrap();
        history.track_edit(&second).unwrap();
        let committed = history.commit_snapshot(plan.compute()).unwrap();
        assert_eq!(committed.message_id, Uuid::from_u128(2));
        assert_eq!(
            committed.tracked_file_backups[first.to_str().unwrap()].version,
            2
        );
        let latest = history.commit_snapshot(history.next_snapshot_plan().unwrap().compute());
        let latest = latest.unwrap();
        assert_eq!(latest.message_id, Uuid::from_u128(3));
        assert_eq!(
            latest.tracked_file_backups[first.to_str().unwrap()].version,
            2
        );
        assert_eq!(
            latest.tracked_file_backups[second.to_str().unwrap()].version,
            1
        );
        assert!(history.next_snapshot_plan().is_none());
    }

    #[test]
    fn rewind_restores_modified_created_and_deleted_files() {
        let (_directory, root) = canonical_temp_directory();
        let modified = root.join("modified.txt");
        let created = root.join("created.txt");
        let deleted = root.join("deleted.txt");
        std::fs::write(&modified, "before").unwrap();
        std::fs::write(&deleted, "keep me").unwrap();
        let mut history = history(&root);
        history.make_snapshot(Uuid::from_u128(1));
        for path in [&modified, &created, &deleted] {
            history.track_edit(path).unwrap();
        }
        std::fs::write(&modified, "after").unwrap();
        std::fs::write(&created, "new").unwrap();
        std::fs::remove_file(&deleted).unwrap();
        history.make_snapshot(Uuid::from_u128(2));

        let mut changed = history.files_changed_by_rewind(Uuid::from_u128(1)).unwrap();
        changed.sort();
        let mut expected = vec![created.clone(), deleted.clone(), modified.clone()];
        expected.sort();
        assert_eq!(changed, expected);
        let report = history.rewind(Uuid::from_u128(1)).unwrap();
        assert!(report.failures.is_empty());
        assert_eq!(std::fs::read_to_string(&modified).unwrap(), "before");
        assert!(!created.exists());
        assert_eq!(std::fs::read_to_string(&deleted).unwrap(), "keep me");
        assert_eq!(
            history.files_changed_by_rewind(Uuid::from_u128(1)),
            Some(Vec::new())
        );

        history.rewind(Uuid::from_u128(2)).unwrap();
        assert_eq!(std::fs::read_to_string(&modified).unwrap(), "after");
        assert_eq!(std::fs::read_to_string(&created).unwrap(), "new");
        assert!(!deleted.exists());
        let staged = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
            .filter(|name| name.starts_with(STAGED_FILE_PREFIX))
            .count();
        assert_eq!(staged, 0);
        assert!(matches!(
            history.rewind(Uuid::from_u128(3)),
            Err(FileHistoryError::SnapshotNotFound)
        ));
    }

    #[test]
    fn symbolic_links_and_special_files_are_never_backed_up() {
        let (_directory, root) = canonical_temp_directory();
        let outside = root.join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let history = history(&root);

        let error = create_first_backup(history.backup_directory(), &link).unwrap_err();
        assert!(error.to_string().contains("symbolic link"), "{error}");
        let linked_parent = root.join("linked");
        std::os::unix::fs::symlink(&root, &linked_parent).unwrap();
        let error = create_first_backup(
            history.backup_directory(),
            &linked_parent.join("outside.txt"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("parent directory"), "{error}");
        let error = create_first_backup(history.backup_directory(), &root).unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");
        assert!(!history.directory().exists());
    }

    #[test]
    fn backup_directories_that_are_symbolic_links_are_rejected() {
        let (_directory, root) = canonical_temp_directory();
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(FILE_HISTORY_DIRECTORY)).unwrap();
        let path = root.join("file.txt");
        std::fs::write(&path, "content").unwrap();
        let history = history(&root);

        assert!(create_first_backup(history.backup_directory(), &path).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn only_the_latest_hundred_snapshots_are_kept_and_old_files_use_their_first_backup() {
        let (_directory, root) = canonical_temp_directory();
        let path = root.join("file.txt");
        std::fs::write(&path, "first").unwrap();
        let mut history = history(&root);
        history.make_snapshot(Uuid::from_u128(0));
        history.track_edit(&path).unwrap();
        std::fs::write(&path, "second").unwrap();
        for index in 1..=MAX_SNAPSHOTS as u128 {
            history.make_snapshot(Uuid::from_u128(index));
        }
        assert_eq!(history.snapshots().len(), MAX_SNAPSHOTS);
        assert!(!history.can_restore(Uuid::from_u128(0)));
        assert!(matches!(
            history.rewind(Uuid::from_u128(0)),
            Err(FileHistoryError::SnapshotNotFound)
        ));
        std::fs::write(&path, "third").unwrap();
        history.rewind(Uuid::from_u128(1)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn copies_share_backups_through_hard_links_and_restore_independently() {
        let (_directory, root) = canonical_temp_directory();
        let path = root.join("file.txt");
        std::fs::write(&path, "before the copy").unwrap();
        let mut source = history(&root);
        source.make_snapshot(Uuid::from_u128(1));
        source.track_edit(&path).unwrap();
        std::fs::write(&path, "after the copy").unwrap();
        source.make_snapshot(Uuid::from_u128(2));

        let target = root.join(FILE_HISTORY_DIRECTORY).join("copy");
        let (copy, recorded) = source.copy_to(PrivateDirectory::trusted(target.clone()));
        assert_eq!(recorded.len(), 2);
        let name = recorded[0].tracked_file_backups[path.to_str().unwrap()]
            .backup_file_name
            .clone()
            .unwrap();
        assert_eq!(
            std::fs::metadata(source.directory().join(&name))
                .unwrap()
                .ino(),
            std::fs::metadata(target.join(&name)).unwrap().ino()
        );
        std::fs::remove_dir_all(source.directory()).unwrap();
        copy.rewind(Uuid::from_u128(1)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before the copy");
    }
}
