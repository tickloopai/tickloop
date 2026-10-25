//! File rollback backups (file-history), implemented following Claude Code's
//! `utils/fileHistory.ts`.
//!
//! Before the Agent edits a file, an initial backup is made for files not yet tracked; each user
//! prompt takes a snapshot of every tracked file (unchanged files reuse the previous version,
//! changed files bump the version, deleted files are recorded as empty), keeping at most 100.
//! Rewind restores files from the last snapshot of the target message. Backups live in
//! `<root>/file-history/<chatId>/`, named by the first 16 hex digits of the path's SHA-256 plus the
//! version number. Like Claude Code, there is no file locking and no coordination of multiple
//! processes modifying the same file at once.
//!
//! Unlike Claude Code, reading and writing tracked files does not follow symlinks: parent
//! directories are opened segment by segment from the root with `O_NOFOLLOW`, and the file is read,
//! removed with `unlinkat`, or replaced by writing a temporary file and `renameat` within that
//! directory. When a file or its parent directory has been replaced by a symlink, or is no longer a
//! regular file, the snapshot does not back it up and rewind records it as a failure and skips it,
//! neither reading nor modifying files outside the workspace. Tracking keys are the canonical paths
//! resolved by the file tools, so the per-segment check does not wrongly reject the path containing
//! the working directory itself.

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

use tickloop_agent_base::private_fs::{
    OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, PrivateDirectory,
    open_without_following_links,
};
use tickloop_agent_base::timestamp::now_timestamp;

/// Directory under the state root (or the project temp directory when not persisting) holding each
/// Chat's backups.
pub(crate) const FILE_HISTORY_DIRECTORY: &str = "file-history";
/// Following Claude Code's `MAX_SNAPSHOTS`.
pub(crate) const MAX_SNAPSHOTS: usize = 100;
const BACKUP_NAME_HASH_HEX_LENGTH: usize = 16;
const FIRST_VERSION: u32 = 1;
/// Workspace directories recreated during restore are created as 0777 like `create_dir_all`; the
/// actual permissions are determined by the umask.
const WORKSPACE_DIRECTORY_MODE: u32 = 0o777;
const STAGED_FILE_PREFIX: &str = ".tickloop-file-history-";
const PERMISSION_BITS: u32 = 0o7777;
const COMPARE_CHUNK_BYTES: usize = 64 * 1024;

/// A file's version in one snapshot; an empty `backup_file_name` means the file does not exist in
/// that version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileHistoryBackup {
    pub backup_file_name: Option<String>,
    pub version: u32,
    pub backup_time: String,
}

/// The snapshot for one user prompt. Paths are absolute (unlike Claude Code, which shortens them
/// relative to the working directory: Tickloop's working directory can change via `/cwd`, so only
/// absolute paths reliably find the file again).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileHistorySnapshot {
    pub message_id: Uuid,
    pub tracked_file_backups: BTreeMap<String, FileHistoryBackup>,
    pub timestamp: String,
}

/// Result of restoring files to a target snapshot. When a single file fails, it is skipped and the
/// others are still processed, following Claude Code.
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

/// File rollback state of one Chat.
#[derive(Clone, Debug)]
pub(crate) struct FileHistory {
    directory: PrivateDirectory,
    snapshots: Vec<FileHistorySnapshot>,
    tracked_files: BTreeSet<String>,
    /// Snapshots already appended with their prompts whose backups are not yet computed, in prompt
    /// order.
    pending_snapshots: Vec<PendingSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingSnapshot {
    message_id: Uuid,
    tracked_files: Vec<String>,
}

/// Input for computing a pending snapshot's backups outside the Chat lock, given by
/// [`FileHistory::next_snapshot_plan`].
#[derive(Debug)]
pub(crate) struct SnapshotPlan {
    message_id: Uuid,
    directory: PrivateDirectory,
    files: Vec<PlannedFile>,
}

#[derive(Debug)]
struct PlannedFile {
    key: String,
    /// Version in the previous snapshot, used to tell whether the file changed.
    previous: Option<FileHistoryBackup>,
    /// Version in the pending snapshot when the plan was made; a mismatch at commit means edit
    /// tracking rewrote it in the meantime, and the rewritten version is kept.
    expected: Option<FileHistoryBackup>,
}

/// Result of [`SnapshotPlan::compute`], committed via [`FileHistory::commit_snapshot`].
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
    pub(super) fn upgrade_checkpoint(&self) -> serde_json::Value {
        serde_json::json!({
            "snapshots": self.snapshots,
            "tracked_files": self.tracked_files,
            "pending_snapshots": self.pending_snapshots,
        })
    }

    pub(super) fn from_upgrade(
        directory: PrivateDirectory,
        value: serde_json::Value,
    ) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Checkpoint {
            snapshots: Vec<FileHistorySnapshot>,
            tracked_files: BTreeSet<String>,
            pending_snapshots: Vec<PendingSnapshot>,
        }
        let checkpoint: Checkpoint =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        Ok(Self {
            directory,
            snapshots: checkpoint.snapshots,
            tracked_files: checkpoint.tracked_files,
            pending_snapshots: checkpoint.pending_snapshots,
        })
    }
    pub(crate) fn new(directory: PrivateDirectory) -> Self {
        Self::restore(directory, Vec::new())
    }

    /// Following Claude Code's `fileHistoryRestoreStateFromLog`: rebuilds the tracked set from the
    /// snapshots on the conversation chain.
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

    /// Keeps only the snapshots belonging to `message_ids`, so a Branch carries only the
    /// conversation before the branch point.
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

    /// After a directed summary rewrites the kept prompts as new entries, the snapshots follow the
    /// prompts to their new ids. Returns the snapshots whose id changed and whose backups are
    /// already computed, which the caller rewrites to the Transcript under the new id; pending
    /// snapshots are written under the new id when committed.
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

    /// Whether an initial backup is needed before editing the file. Following Claude Code: nothing
    /// is tracked while there are no snapshots yet; when the latest snapshot already tracks the
    /// file, the next snapshot checks for changes and the initial backup is not rewritten.
    pub(crate) fn needs_backup(&self, path: &Path) -> bool {
        let Some(key) = tracking_key(path) else {
            return false;
        };
        self.snapshots
            .last()
            .is_some_and(|latest| !latest.tracked_file_backups.contains_key(key))
    }

    /// Following the commit phase of Claude Code's `fileHistoryTrackEdit`: records the initial
    /// backup into the latest snapshot. Returns the updated snapshot, which the caller writes to
    /// the Transcript as `isSnapshotUpdate`; returns `None` when another edit started tracking the
    /// file in the meantime.
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

    /// Completes `fileHistoryTrackEdit` synchronously: makes the initial backup when needed and
    /// records it into the latest snapshot.
    #[cfg(test)]
    pub(crate) fn track_edit(&mut self, path: &Path) -> io::Result<Option<FileHistorySnapshot>> {
        if !self.needs_backup(path) {
            return Ok(None);
        }
        let backup = create_first_backup(&self.directory, path)?;
        Ok(self.commit_tracked_edit(path, backup))
    }

    /// Following the start of Claude Code's `fileHistoryMakeSnapshot`: appends the snapshot
    /// immediately at the prompt's position in the conversation, initially reusing the versions
    /// from the previous snapshot, and replaces them once [`Self::next_snapshot_plan`],
    /// [`SnapshotPlan::compute`] and [`Self::commit_snapshot`] have computed the backups outside
    /// the lock. Removes the oldest snapshot beyond `MAX_SNAPSHOTS`.
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

    /// Computation plan for the oldest pending snapshot; snapshots must be committed one by one in
    /// prompt order so that each can use the previous one as its baseline.
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

    /// Commits a snapshot computed outside the lock. When backing up a single file fails, that file
    /// keeps its version from the previous snapshot (and is not recorded if the previous snapshot
    /// lacks it too); entries rewritten by edit tracking after the plan was made stay unchanged.
    /// Returns `None` when the snapshot has been evicted or is no longer pending.
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

    /// Completes one `fileHistoryMakeSnapshot` synchronously.
    #[cfg(test)]
    pub(crate) fn make_snapshot(&mut self, message_id: Uuid) -> FileHistorySnapshot {
        self.begin_snapshot(message_id);
        while let Some(plan) = self.next_snapshot_plan() {
            self.commit_snapshot(plan.compute());
        }
        self.snapshots.last().cloned().unwrap()
    }

    /// Following Claude Code's `applySnapshot`: deletes the file when the target is empty; uses the
    /// initial backup for files absent from the snapshot; copies back only when the content
    /// differs. Errors without touching any file when no snapshot of the target message is found.
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

    /// Following Claude Code's `fileHistoryHasAnyChanges`/`fileHistoryGetDiffStats`: the files that
    /// restoring to the target message would change, without touching disk. Returns `None` when the
    /// message has no snapshot. Files that cannot be compared safely (for example, replaced by a
    /// symlink) are also listed; rewind will report them as failures.
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

    /// The same snapshots moved to another backup directory without copying backup files; together
    /// with [`Self::copy_backups_to`], lets a Branch build its state inside the Chat lock and copy
    /// backups outside it.
    pub(crate) fn relocated(&self, directory: PrivateDirectory) -> Self {
        Self {
            directory,
            snapshots: self.snapshots.clone(),
            tracked_files: self.tracked_files.clone(),
            pending_snapshots: Vec::new(),
        }
    }

    /// Following Claude Code's `copyFileHistoryForResume`: hard-links the backups into `directory`,
    /// falling back to copying. Returns the snapshots whose backups were all copied successfully
    /// and should be written to the new Transcript.
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

    /// [`Self::relocated`] and [`Self::copy_backups_to`] in one step, for a Chat already outside
    /// the Chat lock and resumed under a new chatId (`--fork-session`).
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

    /// Backup name in the target snapshot; when the snapshot does not track the file, falls back to
    /// the initial backup following Claude Code. An outer `None` means no usable backup was found.
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

    /// Returns `true` when the file was changed.
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
        // The file may have been replaced after the comparison; check again and do not write
        // symlinks or non-regular files.
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
    /// Following the per-file handling of Claude Code's `fileHistoryMakeSnapshot`: unchanged files
    /// reuse the previous version, changed files bump the version, deleted files are recorded as
    /// empty. Reads and writes files, so it should be called in `spawn_blocking`.
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

/// Only UTF-8 paths are tracked: snapshots store paths as JSON object keys.
fn tracking_key(path: &Path) -> Option<&str> {
    path.to_str()
}

fn backup_file_name(path: &str, version: u32) -> String {
    let digest = format!("{:x}", Sha256::digest(path.as_bytes()));
    format!("{}@v{version}", &digest[..BACKUP_NAME_HASH_HEX_LENGTH])
}

/// Following the backup phase of Claude Code's `fileHistoryTrackEdit`: makes the initial backup of
/// the file before it is edited.
pub(crate) fn create_first_backup(
    directory: &PrivateDirectory,
    path: &Path,
) -> io::Result<FileHistoryBackup> {
    create_backup(directory, path, FIRST_VERSION)
}

/// Following Claude Code's `createBackup`: records empty when the source file does not exist;
/// otherwise copies it into the backup directory, preserving permission bits. Returns an error
/// explaining why, without making a backup, when the source file or its parent directory is a
/// symlink or the source file is not a regular file.
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

/// Opens the backup directory: opens it segment by segment from the root with `O_NOFOLLOW`,
/// confirming each level is a real directory, and with `create` recreates missing levels as 0700;
/// then validates it as a [`PrivateDirectory`] (under the system temp directory, ownership and
/// permission bits are checked at each level). Returns `NotFound` when `create` is false and the
/// directory does not exist.
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

/// Opens a tracked file without following symlinks; returns `None` when the file or its parent
/// directory does not exist.
fn open_tracked_file(path: &Path) -> io::Result<Option<OpenedFile>> {
    let (parent, name) = split_path(path)?;
    match open_directory(parent, None) {
        Ok(directory) => open_regular_file_at(&directory, &name),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether rewind might delete the file when the target version is empty; when this cannot be
/// determined safely, the file is treated as existing and rewind reports the failure.
fn tracked_entry_may_exist(path: &Path) -> bool {
    let Ok((parent, name)) = split_path(path) else {
        return true;
    };
    match open_directory(parent, None) {
        Ok(directory) => !matches!(entry_kind_at(&directory, &name), Ok(None)),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

/// Deletes a file whose target version is empty, following Claude Code; only regular files are
/// deleted, and a missing parent directory counts as already deleted.
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
    // SAFETY: The directory descriptor is valid and the name is a single NUL-terminated path
    // segment.
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

/// Opens a directory segment by segment from the root with `O_NOFOLLOW`. With `create_mode`,
/// missing levels are created with that mode (subject to the umask); otherwise a missing level
/// returns `NotFound`. Errors when any level is a symlink or not a directory.
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
    // SAFETY: The directory descriptor is valid, the name is NUL-terminated, and `status` is large
    // enough to hold a `stat` struct.
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
    // SAFETY: fstatat has fully written `status` on success.
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

/// Opens a regular file in the directory without following symlinks; returns `None` when it does
/// not exist.
fn open_regular_file_at(directory: &File, name: &CStr) -> io::Result<Option<OpenedFile>> {
    match entry_kind_at(directory, name)? {
        None => return Ok(None),
        Some(EntryKind::Regular) => {}
        Some(EntryKind::SymbolicLink) => return Err(symbolic_link_error()),
        Some(EntryKind::Other) => return Err(not_regular_error()),
    }
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: The directory descriptor is valid and the name is a single NUL-terminated path
    // segment; on success the returned descriptor is owned by the `File`.
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return match (error.kind(), error.raw_os_error()) {
            (io::ErrorKind::NotFound, _) => Ok(None),
            (_, Some(libc::ELOOP)) => Err(symbolic_link_error()),
            _ => Err(error),
        };
    }
    // SAFETY: The descriptor returned by a successful openat is held by nothing else; ownership
    // passes to the `File`.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(not_regular_error());
    }
    Ok(Some(OpenedFile { file, metadata }))
}

/// Writes a temporary file in the directory, sets its permission bits, then replaces `name` with
/// `renameat`; the temporary file is removed on failure. The directory entry itself is replaced, so
/// even if `name` is a symlink at that moment, nothing is written to the link's target.
fn publish_copy(directory: &File, name: &CStr, source: &mut File, mode: u32) -> io::Result<()> {
    let staged = c_string(format!("{STAGED_FILE_PREFIX}{}", tickloop_ids::generate()).as_bytes())?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: The directory descriptor is valid and the name is NUL-terminated; on success the
    // returned descriptor is owned by the `File`.
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
    // SAFETY: The descriptor returned by a successful openat is held by nothing else; ownership
    // passes to the `File`.
    let mut staged_file = unsafe { File::from_raw_fd(descriptor) };
    let published = (|| {
        io::copy(source, &mut staged_file)?;
        // SAFETY: The descriptor is valid.
        if unsafe {
            libc::fchmod(
                staged_file.as_raw_fd(),
                (mode & PERMISSION_BITS) as libc::mode_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Both names are under the same valid directory descriptor and are NUL-terminated.
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
        // SAFETY: The directory descriptor is valid and the name is NUL-terminated.
        unsafe { libc::unlinkat(directory.as_raw_fd(), staged.as_ptr(), 0) };
    }
    published
}

fn link_or_copy(source: &File, target: &File, name: &str) -> bool {
    let Ok(name) = c_string(name.as_bytes()) else {
        return false;
    };
    // SAFETY: Both directory descriptors are valid and the names are NUL-terminated; with flags 0,
    // symlinks are not followed.
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

/// Following Claude Code's `checkOriginFileChanged`: one side existing and the other missing counts
/// as changed; differing permissions or size count as changed; when the original file's
/// modification time is earlier than the backup's, the content is not compared; otherwise the
/// content is compared.
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

/// Reads until the buffer is full or the end of the file is reached.
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

/// Test seam: makes file-history operations reading or writing a given path stop in the blocking
/// thread until the test releases them.
#[cfg(test)]
pub(crate) mod test_pause {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    /// Deadline after which the pause releases itself if the test has not, so a failing test does
    /// not hang forever.
    const PAUSE_LIMIT: Duration = Duration::from_secs(5);

    type Gate = (mpsc::Sender<()>, mpsc::Receiver<()>);

    static GATES: Mutex<Option<HashMap<PathBuf, Gate>>> = Mutex::new(None);

    /// A one-shot pause; released when dropped.
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

    /// Per-segment `O_NOFOLLOW` opening requires canonical paths (the macOS temp directory sits
    /// under the symlink `/var`).
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
