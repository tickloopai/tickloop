//! File rollback backups (file-history), implemented following Claude Code's
//! `utils/fileHistory.ts`.
//!
//! Before the Agent edits a file, an initial backup is made for files not yet tracked; each user
//! prompt takes a snapshot of every tracked file (unchanged files reuse the previous version,
//! changed files bump the version, deleted files are recorded as empty), keeping at most 100.
//! Rewind restores files from the last snapshot of the target message. Backups live on this
//! machine in `<root>/file-history/<chatId>/`, named by the first 16 hex digits of the path's
//! SHA-256 plus the version number. Like Claude Code, there is no file locking and no coordination
//! of multiple processes modifying the same file at once.
//!
//! Tracked files are reached where the Chat's file tools reach them: the Folder and Additional
//! Directories through the Chat's Execution Environment, the Chat's own directories on this
//! machine. Tracking keys are the absolute paths the model sees for the files, with links resolved
//! by the file tools (locally the canonical paths), and each use maps a key to its root again.
//! Restoring writes or deletes a file only while it still has the version read just before, so a
//! change made meanwhile is kept and reported. File history only changes files in directories the
//! Chat can write and does not follow symbolic links: a tracked path that now leads elsewhere, or
//! is no longer a regular file, is neither backed up nor restored, and rewind records it as a
//! failure and skips it. Backups do not keep permission bits, which the Port does not report; a
//! restored file keeps its own, and a recreated one gets what the environment gives new files.
//!
//! Backups exist only for files on this machine. A file whose root the Port reports without a
//! local path, and every file of a Chat whose Folder is only in the environment
//! ([`FileLocation::is_only_in_environment`]), is never read into or restored from a backup and
//! never makes the backup directory exist; such a Chat therefore writes nothing here. Its edits
//! are only named in the snapshot they happened in, so rewind reports each such file as not
//! restored instead of leaving it out silently, while files on this machine in the same Chat are
//! restored as usual.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_harness_base::private_fs::{
    OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, PrivateDirectory,
    open_without_following_links,
};
use tickloop_harness_base::timestamp::now_timestamp;
use tickloop_harness_tools::file_port::{
    FileContent, FileInspection, FileKind, FileMetadata, FileQuery, FileWriteError, error_code,
};
use tickloop_harness_tools::{FileLocation, FileLocator};

/// Directory under the state root (or the project temp directory when not persisting) holding each
/// Chat's backups.
pub(crate) const FILE_HISTORY_DIRECTORY: &str = "file-history";
/// Following Claude Code's `MAX_SNAPSHOTS`.
pub(crate) const MAX_SNAPSHOTS: usize = 100;
const BACKUP_NAME_HASH_HEX_LENGTH: usize = 16;
const FIRST_VERSION: u32 = 1;
const STAGED_FILE_PREFIX: &str = ".tickloop-file-history-";
const PERMISSION_BITS: u32 = 0o7777;
/// How much of a tracked file one read through its host asks for.
const READ_CHUNK_BYTES: usize = 1024 * 1024;
/// Why a file only in the environment is neither backed up nor restored.
pub(crate) const ONLY_IN_ENVIRONMENT: &str = "the file is only in the execution environment, and file history keeps no copy of it on this machine";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backup_directory: Option<PathBuf>,
    pub message_id: Uuid,
    pub tracked_file_backups: BTreeMap<String, FileHistoryBackup>,
    /// Files only in the environment edited after this snapshot's prompt, which have no backup.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    environment_files: BTreeSet<String>,
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
            "backup_directory": self.directory.path(),
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
            #[serde(default)]
            backup_directory: Option<PathBuf>,
            snapshots: Vec<FileHistorySnapshot>,
            tracked_files: BTreeSet<String>,
            pending_snapshots: Vec<PendingSnapshot>,
        }
        let checkpoint: Checkpoint =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        let directory = restored_directory(directory, checkpoint.backup_directory.as_deref())?;
        Ok(Self {
            directory,
            snapshots: checkpoint.snapshots,
            tracked_files: checkpoint.tracked_files,
            pending_snapshots: checkpoint.pending_snapshots,
        })
    }
    pub(crate) fn protect_from(&self, tools: &tickloop_harness_tools::Tools) {
        if let Some(parent) = self.directory().parent() {
            tools.protect_directory(parent.to_path_buf());
        }
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
        let directory = restored_directory(
            directory.clone(),
            snapshots
                .iter()
                .rev()
                .find_map(|snapshot| snapshot.backup_directory.as_deref()),
        )
        .unwrap_or(directory);
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

    /// Whether an initial backup is needed before editing the file the model sees at `key`.
    /// Following Claude Code: nothing is tracked while there are no snapshots yet; when the latest
    /// snapshot already tracks the file, the next snapshot checks for changes and the initial
    /// backup is not rewritten.
    pub(crate) fn needs_backup(&self, key: &str) -> bool {
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
        key: &str,
        backup: FileHistoryBackup,
    ) -> Option<FileHistorySnapshot> {
        let latest = self.snapshots.last_mut()?;
        if latest.tracked_file_backups.contains_key(key) {
            return None;
        }
        latest.tracked_file_backups.insert(key.to_owned(), backup);
        self.tracked_files.insert(key.to_owned());
        Some(latest.clone())
    }

    /// Names a file only in the environment, which gets no backup, as edited after the latest
    /// snapshot's prompt. Returns the updated snapshot, which the caller writes to the Transcript as
    /// `isSnapshotUpdate`; returns `None` when there is no snapshot yet or it already names the file.
    pub(crate) fn note_environment_edit(&mut self, key: &str) -> Option<FileHistorySnapshot> {
        let latest = self.snapshots.last_mut()?;
        latest
            .environment_files
            .insert(key.to_owned())
            .then(|| latest.clone())
    }

    /// Completes `fileHistoryTrackEdit` in one step: makes the initial backup when needed and
    /// records it into the latest snapshot.
    #[cfg(test)]
    pub(crate) async fn track_edit(
        &mut self,
        files: &FileLocator,
        path: &Path,
    ) -> io::Result<Option<FileHistorySnapshot>> {
        let location = files
            .locate(path)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let key = location.display();
        if !self.needs_backup(&key) {
            return Ok(None);
        }
        let backup = create_first_backup(&self.directory, &location).await?;
        Ok(self.commit_tracked_edit(&key, backup))
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
            backup_directory: Some(self.directory.path().to_path_buf()),
            message_id,
            tracked_file_backups,
            environment_files: BTreeSet::new(),
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

    /// Completes one `fileHistoryMakeSnapshot` in one step.
    #[cfg(test)]
    pub(crate) async fn make_snapshot(
        &mut self,
        files: &FileLocator,
        message_id: Uuid,
    ) -> FileHistorySnapshot {
        self.begin_snapshot(message_id);
        while let Some(plan) = self.next_snapshot_plan() {
            self.commit_snapshot(plan.compute(files).await);
        }
        self.snapshots.last().cloned().unwrap()
    }

    /// Following Claude Code's `applySnapshot`: deletes the file when the target is empty; uses the
    /// initial backup for files absent from the snapshot; copies back only when the content
    /// differs. Errors without touching any file when no snapshot of the target message is found.
    /// Files only in the environment edited since the target prompt are reported as failures.
    pub(crate) async fn rewind(
        &self,
        files: &FileLocator,
        message_id: Uuid,
    ) -> Result<FileRestoreReport, FileHistoryError> {
        let target_index = self
            .snapshots
            .iter()
            .rposition(|snapshot| snapshot.message_id == message_id)
            .ok_or(FileHistoryError::SnapshotNotFound)?;
        let target = &self.snapshots[target_index];
        let mut report = FileRestoreReport::default();
        for key in &self.tracked_files {
            let path = PathBuf::from(key);
            let Some(backup_file_name) = self.target_backup_name(target, key) else {
                report
                    .failures
                    .push((path, "no backup of this file can be found".into()));
                continue;
            };
            match self
                .apply_backup(files, key, backup_file_name.as_deref())
                .await
            {
                Ok(true) => report.changed_files.push(path),
                Ok(false) => {}
                Err(error) => report.failures.push((path, error.to_string())),
            }
        }
        let environment_files = self.snapshots[target_index..]
            .iter()
            .flat_map(|snapshot| &snapshot.environment_files)
            .collect::<BTreeSet<_>>();
        for key in environment_files {
            let path = PathBuf::from(key);
            if !report.failures.iter().any(|(failed, _)| *failed == path) {
                report.failures.push((path, ONLY_IN_ENVIRONMENT.into()));
            }
        }
        Ok(report)
    }

    /// Following Claude Code's `fileHistoryHasAnyChanges`/`fileHistoryGetDiffStats`: the files that
    /// restoring to the target message would change, without changing any. Returns `None` when the
    /// message has no snapshot. Files that cannot be compared safely (for example, replaced by a
    /// symlink) are also listed; rewind will report them as failures.
    pub(crate) async fn files_changed_by_rewind(
        &self,
        files: &FileLocator,
        message_id: Uuid,
    ) -> Option<Vec<PathBuf>> {
        let target = self.target_snapshot(message_id).ok()?;
        let mut changed = Vec::new();
        for key in &self.tracked_files {
            let changes = match self.target_backup_name(target, key) {
                None => false,
                Some(None) => tracked_file(files, key)
                    .await
                    .is_none_or(|tracked| tracked.exists),
                Some(Some(name)) => match tracked_file(files, key).await {
                    Some(tracked) => match (
                        tracked.read().await,
                        read_backup(&self.directory, &name).await,
                    ) {
                        (Ok(current), Ok(backup)) => {
                            content_changed(current.as_ref(), backup.as_deref())
                        }
                        _ => true,
                    },
                    None => true,
                },
            };
            if changes {
                changed.push(PathBuf::from(key));
            }
        }
        Some(changed)
    }

    /// The same snapshots moved to another backup directory without copying backup files; together
    /// with [`Self::copy_backups_to`], lets a Branch build its state inside the Chat lock and copy
    /// backups outside it.
    pub(crate) fn relocated(&self, directory: PrivateDirectory) -> Self {
        Self {
            directory: directory.clone(),
            snapshots: self
                .snapshots
                .iter()
                .cloned()
                .map(|mut snapshot| {
                    snapshot.backup_directory = Some(directory.path().to_path_buf());
                    snapshot
                })
                .collect(),
            tracked_files: self.tracked_files.clone(),
            pending_snapshots: Vec::new(),
        }
    }

    /// Following Claude Code's `copyFileHistoryForResume`: hard-links the backups into `directory`,
    /// falling back to copying. Returns the snapshots whose backups were all copied successfully
    /// and should be written to the new Transcript.
    pub(crate) fn copy_backups_to(&self, directory: &PrivateDirectory) -> Vec<FileHistorySnapshot> {
        let relocate = |mut snapshot: FileHistorySnapshot| {
            snapshot.backup_directory = Some(directory.path().to_path_buf());
            snapshot
        };
        // Without a backup to copy, the directory is not created: a Chat whose files are only in
        // the environment keeps nothing on this machine.
        if self.snapshots.iter().all(|snapshot| {
            snapshot
                .tracked_file_backups
                .values()
                .all(|backup| backup.backup_file_name.is_none())
        }) {
            return self.snapshots.iter().cloned().map(relocate).collect();
        }
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
            .map(relocate)
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

    /// Returns `true` when the file was changed. The change carries the version just read, so a
    /// file changed after that read is left as it is and reported.
    async fn apply_backup(
        &self,
        files: &FileLocator,
        key: &str,
        backup_file_name: Option<&str>,
    ) -> io::Result<bool> {
        #[cfg(test)]
        test_pause::wait_if_paused(key).await;
        let tracked = locate_tracked(files, key)?.inspect().await?;
        let current = tracked.read().await?;
        let host = &tracked.location.host;
        let path = tracked.location.path.clone();
        let Some(name) = backup_file_name else {
            let Some(current) = current else {
                return Ok(false);
            };
            host.delete(path, current.metadata.version)
                .await
                .map_err(change_error)?;
            return Ok(true);
        };
        let backup = read_backup(&self.directory, name).await?;
        if !content_changed(current.as_ref(), backup.as_deref()) {
            return Ok(false);
        }
        let backup = backup
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the backup file is missing"))?;
        let expected = current.map(|current| current.metadata.version);
        host.prepare_write(path, backup, expected)
            .await
            .map_err(|error| io::Error::other(error.message))?
            .write(CancellationToken::new())
            .await
            .map_err(change_error)?;
        Ok(true)
    }
}

impl SnapshotPlan {
    /// Following the per-file handling of Claude Code's `fileHistoryMakeSnapshot`: unchanged files
    /// reuse the previous version, changed files bump the version, deleted files are recorded as
    /// empty.
    pub(crate) async fn compute(self, files: &FileLocator) -> ComputedSnapshot {
        let mut computed = Vec::with_capacity(self.files.len());
        for file in self.files {
            let resolved = match next_backup(
                &self.directory,
                files,
                &file.key,
                file.previous.as_ref(),
            )
            .await
            {
                Ok(backup) => Some(backup),
                Err(_) => file.previous,
            };
            computed.push(ComputedFile {
                key: file.key,
                expected: file.expected,
                resolved,
            });
        }
        ComputedSnapshot {
            message_id: self.message_id,
            files: computed,
        }
    }
}

fn restored_directory(
    fallback: PrivateDirectory,
    recorded: Option<&Path>,
) -> Result<PrivateDirectory, String> {
    let Some(recorded) = recorded.filter(|path| *path != fallback.path()) else {
        return Ok(fallback);
    };
    let root = crate::transcript::private_temp_root();
    let relative = recorded
        .strip_prefix(root.path())
        .map_err(|_| "file history namespace is outside private storage")?;
    let components = relative.components().collect::<Vec<_>>();
    if !(2..=3).contains(&components.len())
        || components
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || recorded.file_name() != fallback.path().file_name()
        || recorded.parent().and_then(Path::file_name)
            != Some(std::ffi::OsStr::new(FILE_HISTORY_DIRECTORY))
        || components
            .first()
            .is_some_and(|component| component.as_os_str() == "sessions")
    {
        return Err("invalid file history namespace".into());
    }
    Ok(root.join(relative))
}

fn backup_file_name(key: &str, version: u32) -> String {
    let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
    format!("{}@v{version}", &digest[..BACKUP_NAME_HASH_HEX_LENGTH])
}

/// Following the backup phase of Claude Code's `fileHistoryTrackEdit`: makes the initial backup of
/// the file at `location` before it is edited.
pub(crate) async fn create_first_backup(
    directory: &PrivateDirectory,
    location: &FileLocation,
) -> io::Result<FileHistoryBackup> {
    let backup_time = now_timestamp();
    let tracked = TrackedLocation::new(location.clone())?.inspect().await?;
    match tracked.read().await? {
        Some(source) => {
            write_backup(
                directory,
                &location.display(),
                FIRST_VERSION,
                source.data,
                backup_time,
            )
            .await
        }
        None => Ok(FileHistoryBackup {
            backup_file_name: None,
            version: FIRST_VERSION,
            backup_time,
        }),
    }
}

async fn next_backup(
    directory: &PrivateDirectory,
    files: &FileLocator,
    key: &str,
    previous: Option<&FileHistoryBackup>,
) -> io::Result<FileHistoryBackup> {
    #[cfg(test)]
    test_pause::wait_if_paused(key).await;
    let backup_time = now_timestamp();
    let version = previous.map_or(FIRST_VERSION, |backup| backup.version + 1);
    let Some(source) = locate_tracked(files, key)?.inspect().await?.read().await? else {
        return Ok(FileHistoryBackup {
            backup_file_name: None,
            version,
            backup_time,
        });
    };
    if let Some(previous) = previous
        && let Some(name) = &previous.backup_file_name
        && let Ok(backup) = read_backup(directory, name).await
        && !content_changed(Some(&source), backup.as_deref())
    {
        return Ok(previous.clone());
    }
    write_backup(directory, key, version, source.data, backup_time).await
}

async fn write_backup(
    directory: &PrivateDirectory,
    key: &str,
    version: u32,
    content: Vec<u8>,
    backup_time: String,
) -> io::Result<FileHistoryBackup> {
    let directory = directory.clone();
    let key = key.to_owned();
    let (name, version) = tokio::task::spawn_blocking(move || {
        let backup_directory = open_backup_directory(&directory, true)?;
        let mut version = version;
        loop {
            let name = backup_file_name(&key, version);
            match publish_copy(
                &backup_directory,
                &c_string(name.as_bytes())?,
                &mut content.as_slice(),
                PRIVATE_FILE_MODE,
            ) {
                Ok(()) => return Ok::<_, io::Error>((name, version)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    version = version
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("file history version exhausted"))?;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .map_err(io::Error::other)??;
    Ok(FileHistoryBackup {
        backup_file_name: Some(name),
        version,
        backup_time,
    })
}

/// The content of a backup, or `None` when it is missing.
async fn read_backup(directory: &PrivateDirectory, name: &str) -> io::Result<Option<Vec<u8>>> {
    let directory = directory.clone();
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || {
        let Some(mut backup) = open_backup_file(&directory, &name)? else {
            return Ok(None);
        };
        let mut content = Vec::new();
        backup.file.read_to_end(&mut content)?;
        Ok(Some(content))
    })
    .await
    .map_err(io::Error::other)?
}

/// Following Claude Code's `checkOriginFileChanged`: one side existing and the other missing
/// counts as changed, and otherwise the content is compared. The modification times Claude Code
/// compares first come from two machines' clocks here, so they are not trusted.
fn content_changed(current: Option<&FileContent>, backup: Option<&[u8]>) -> bool {
    match (current, backup) {
        (None, None) => false,
        (Some(current), Some(backup)) => current.data != backup,
        _ => true,
    }
}

/// The failure of a restore the host refused or whose outcome is unknown; neither is retried.
fn change_error(error: FileWriteError) -> io::Error {
    match error {
        FileWriteError::NotWritten(error) => io::Error::other(error.message),
        FileWriteError::OutcomeUnknown(reason) => io::Error::other(format!(
            "the restore was sent but its answer was lost ({reason}), so it may or may not have taken effect; it was not sent again"
        )),
    }
}

/// A tracked file located where the Chat can change it.
struct TrackedLocation {
    location: FileLocation,
}

/// A tracked file whose path was found to lead to itself.
struct TrackedFile {
    location: FileLocation,
    exists: bool,
}

/// Locates the tracked file the model sees at `key`.
fn locate_tracked(files: &FileLocator, key: &str) -> io::Result<TrackedLocation> {
    let location = files
        .locate(Path::new(key))
        .map_err(|error| io::Error::other(error.to_string()))?;
    TrackedLocation::new(location)
}

/// The tracked file at `key` when it can be located and its path leads to itself.
async fn tracked_file(files: &FileLocator, key: &str) -> Option<TrackedFile> {
    locate_tracked(files, key).ok()?.inspect().await.ok()
}

impl TrackedLocation {
    /// File history changes only files on this machine in directories the Chat can write.
    fn new(location: FileLocation) -> io::Result<Self> {
        if location.is_only_in_environment() {
            return Err(io::Error::other(ONLY_IN_ENVIRONMENT));
        }
        if !location
            .host
            .capabilities()
            .access(&location.path)
            .can_write()
        {
            return Err(io::Error::other(
                "the file is in a directory this Chat can only read",
            ));
        }
        Ok(Self { location })
    }

    /// What is at the path now: an error when the path leads elsewhere through a symbolic link or
    /// to something other than a regular file.
    async fn inspect(self) -> io::Result<TrackedFile> {
        let location = self.location;
        let resolved = match location
            .host
            .inspect(FileQuery::Resolve(location.path.clone()))
            .await
        {
            Ok(FileInspection::Resolved(resolved)) => resolved,
            Ok(_) => return Err(unexpected_answer()),
            Err(error) => return Err(io::Error::other(error.message)),
        };
        if resolved.path != location.path {
            return Err(io::Error::other(format!(
                "the path now leads to {} through a symbolic link; file history does not follow links",
                location.host.capabilities().display(&resolved.path)
            )));
        }
        let exists = match resolved.kind {
            None => false,
            Some(FileKind::File) => true,
            Some(FileKind::Directory | FileKind::Other) => return Err(not_regular_error()),
        };
        Ok(TrackedFile { location, exists })
    }
}

impl TrackedFile {
    /// The whole file, read in chunks that must all come from one version of it; `None` when
    /// nothing is there.
    async fn read(&self) -> io::Result<Option<FileContent>> {
        if !self.exists {
            return Ok(None);
        }
        let mut data = Vec::new();
        let mut metadata: Option<FileMetadata> = None;
        loop {
            let query = FileQuery::Read {
                path: self.location.path.clone(),
                offset: data.len() as u64,
                limit: READ_CHUNK_BYTES,
            };
            let chunk = match self.location.host.inspect(query).await {
                Ok(FileInspection::Read(chunk)) => chunk,
                Ok(_) => return Err(unexpected_answer()),
                Err(error) if error.code == error_code::NOT_FOUND && metadata.is_none() => {
                    return Ok(None);
                }
                Err(error) if error.code == error_code::IS_DIRECTORY => {
                    return Err(not_regular_error());
                }
                Err(error) => return Err(io::Error::other(error.message)),
            };
            if metadata
                .as_ref()
                .is_some_and(|metadata| metadata.version != chunk.metadata.version)
            {
                return Err(io::Error::other(
                    "the file changed while file history read it",
                ));
            }
            let is_last = chunk.data.len() < READ_CHUNK_BYTES;
            data.extend(chunk.data);
            metadata = Some(chunk.metadata);
            if is_last {
                break;
            }
        }
        Ok(metadata.map(|metadata| FileContent { metadata, data }))
    }
}

fn unexpected_answer() -> io::Error {
    io::Error::other("the environment answered a file query with something else")
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

/// Writes a temporary file in the backup directory, sets its permission bits, then replaces
/// `name` with `renameat`; the temporary file is removed on failure. The directory entry itself is
/// replaced, so even if `name` is a symlink at that moment, nothing is written to the link's
/// target.
fn publish_copy(
    directory: &File,
    name: &CStr,
    source: &mut impl Read,
    mode: u32,
) -> io::Result<()> {
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
            libc::linkat(
                directory.as_raw_fd(),
                staged.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    {
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

/// Test seam: makes file-history operations reading or writing a given path wait until the test
/// releases them.
#[cfg(test)]
pub(crate) mod test_pause {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    use tokio::sync::oneshot;

    /// Deadline after which the pause releases itself if the test has not, so a failing test does
    /// not hang forever.
    const PAUSE_LIMIT: Duration = Duration::from_secs(5);

    type Gate = (mpsc::Sender<()>, oneshot::Receiver<()>);

    static GATES: Mutex<Option<HashMap<PathBuf, Gate>>> = Mutex::new(None);

    /// A one-shot pause; released when dropped.
    pub(crate) struct Pause {
        pub(crate) entered: mpsc::Receiver<()>,
        _release: oneshot::Sender<()>,
    }

    pub(crate) fn pause_next(path: &Path) -> Pause {
        let (entered_sender, entered) = mpsc::channel();
        let (release, release_receiver) = oneshot::channel();
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

    pub(super) async fn wait_if_paused(key: &str) {
        let gate = GATES
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|gates| gates.remove(Path::new(key)));
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = tokio::time::timeout(PAUSE_LIMIT, release).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use tickloop_harness_tools::local_roots::LocalRoots;

    use super::*;

    fn history(root: &Path) -> FileHistory {
        FileHistory::new(PrivateDirectory::trusted(
            root.join(FILE_HISTORY_DIRECTORY).join("chat"),
        ))
    }

    /// The files of a Chat whose Folder is `root` on this machine.
    fn local_files(root: &Path) -> FileLocator {
        FileLocator::new(Arc::new(LocalRoots::new(root.to_str(), &[])))
    }

    /// Local roots are real paths (the macOS temp directory sits under the symlink `/var`).
    fn canonical_temp_directory() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        (directory, root)
    }

    #[tokio::test]
    async fn copying_a_history_without_backups_creates_no_directory() {
        let (_directory, root) = canonical_temp_directory();
        let files = local_files(&root);
        let mut history = history(&root);
        history.make_snapshot(&files, Uuid::from_u128(1)).await;
        assert!(
            history
                .note_environment_edit("/design/index.html")
                .is_some()
        );
        assert!(
            history
                .note_environment_edit("/design/index.html")
                .is_none()
        );
        let target = PrivateDirectory::trusted(root.join("copy").join("chat"));

        let (copied, recorded) = history.copy_to(target.clone());

        assert!(!root.join("copy").exists());
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].backup_directory.as_deref(), Some(target.path()));
        assert_eq!(
            copied.snapshots()[0].environment_files,
            recorded[0].environment_files
        );
        let report = copied.rewind(&files, Uuid::from_u128(1)).await.unwrap();
        assert!(report.changed_files.is_empty());
        assert_eq!(
            report.failures,
            vec![(
                PathBuf::from("/design/index.html"),
                ONLY_IN_ENVIRONMENT.to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn backups_follow_claude_code_names_in_a_private_directory() {
        let (_directory, root) = canonical_temp_directory();
        let files = local_files(&root);
        let path = root.join("script.sh");
        std::fs::write(&path, "echo one\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let mut history = history(&root);
        history.make_snapshot(&files, Uuid::from_u128(1)).await;

        let updated = history.track_edit(&files, &path).await.unwrap().unwrap();
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
            PRIVATE_FILE_MODE
        );
        assert_eq!(
            std::fs::metadata(history.directory()).unwrap().mode() & 0o777,
            0o700
        );
        assert!(history.track_edit(&files, &path).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn edits_without_a_snapshot_are_not_tracked() {
        let (_directory, root) = canonical_temp_directory();
        let path = root.join("untracked.txt");
        let mut history = history(&root);
        assert!(
            history
                .track_edit(&local_files(&root), &path)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!history.directory().exists());
    }

    #[tokio::test]
    async fn snapshots_reuse_unchanged_versions_bump_changed_ones_and_mark_deletions() {
        let (_directory, root) = canonical_temp_directory();
        let files = local_files(&root);
        let [kept, changed, deleted] =
            ["kept.txt", "changed.txt", "deleted.txt"].map(|name| root.join(name));
        for path in [&kept, &changed, &deleted] {
            std::fs::write(path, "original").unwrap();
        }
        let mut history = history(&root);
        history.make_snapshot(&files, Uuid::from_u128(1)).await;
        for path in [&kept, &changed, &deleted] {
            history.track_edit(&files, path).await.unwrap();
        }
        std::fs::write(&changed, "changed content").unwrap();
        std::fs::remove_file(&deleted).unwrap();

        let snapshot = history.make_snapshot(&files, Uuid::from_u128(2)).await;
        let version =
            |path: &PathBuf| snapshot.tracked_file_backups[path.to_str().unwrap()].clone();
        assert_eq!(version(&kept).version, 1);
        assert_eq!(version(&changed).version, 2);
        assert!(version(&changed).backup_file_name.unwrap().ends_with("@v2"));
        assert_eq!(version(&deleted).version, 2);
        assert!(version(&deleted).backup_file_name.is_none());
    }

    #[tokio::test]
    async fn pending_snapshots_keep_prompt_order_and_edits_committed_while_computing() {
        let (_directory, root) = canonical_temp_directory();
        let files = local_files(&root);
        let [first, second] = ["first.txt", "second.txt"].map(|name| root.join(name));
        std::fs::write(&first, "one").unwrap();
        let mut history = history(&root);
        history.make_snapshot(&files, Uuid::from_u128(1)).await;
        history.track_edit(&files, &first).await.unwrap();
        std::fs::write(&first, "two").unwrap();

        history.begin_snapshot(Uuid::from_u128(2));
        history.begin_snapshot(Uuid::from_u128(3));
        let plan = history.next_snapshot_plan().unwrap();
        std::fs::write(&second, "second").unwrap();
        history.track_edit(&files, &second).await.unwrap();
        let committed = history.commit_snapshot(plan.compute(&files).await).unwrap();
        assert_eq!(committed.message_id, Uuid::from_u128(2));
        assert_eq!(
            committed.tracked_file_backups[first.to_str().unwrap()].version,
            2
        );
        let plan = history.next_snapshot_plan().unwrap();
        let latest = history.commit_snapshot(plan.compute(&files).await);
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

    #[tokio::test]
    async fn rewind_restores_modified_created_and_deleted_files() {
        let (_directory, root) = canonical_temp_directory();
        let files = local_files(&root);
        let modified = root.join("modified.txt");
        let created = root.join("created.txt");
        let deleted = root.join("deleted.txt");
        std::fs::write(&modified, "before").unwrap();
        std::fs::write(&deleted, "keep me").unwrap();
        let mut history = history(&root);
        history.make_snapshot(&files, Uuid::from_u128(1)).await;
        for path in [&modified, &created, &deleted] {
            history.track_edit(&files, path).await.unwrap();
        }
        std::fs::write(&modified, "after").unwrap();
        std::fs::write(&created, "new").unwrap();
        std::fs::remove_file(&deleted).unwrap();
        history.make_snapshot(&files, Uuid::from_u128(2)).await;

        let mut changed = history
            .files_changed_by_rewind(&files, Uuid::from_u128(1))
            .await
            .unwrap();
        changed.sort();
        let mut expected = vec![created.clone(), deleted.clone(), modified.clone()];
        expected.sort();
        assert_eq!(changed, expected);
        let report = history.rewind(&files, Uuid::from_u128(1)).await.unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(std::fs::read_to_string(&modified).unwrap(), "before");
        assert!(!created.exists());
        assert_eq!(std::fs::read_to_string(&deleted).unwrap(), "keep me");
        assert_eq!(
            history
                .files_changed_by_rewind(&files, Uuid::from_u128(1))
                .await,
            Some(Vec::new())
        );

        history.rewind(&files, Uuid::from_u128(2)).await.unwrap();
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
            history.rewind(&files, Uuid::from_u128(3)).await,
            Err(FileHistoryError::SnapshotNotFound)
        ));
    }

    #[tokio::test]
    async fn symbolic_links_and_special_files_are_never_backed_up() {
        let (_directory, root) = canonical_temp_directory();
        let files = local_files(&root);
        let outside = root.join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let history = history(&root);
        let first_backup = |path: PathBuf| {
            let files = files.clone();
            let directory = history.backup_directory().clone();
            async move { create_first_backup(&directory, &files.locate(&path).unwrap()).await }
        };

        let error = first_backup(link).await.unwrap_err();
        assert!(error.to_string().contains("symbolic link"), "{error}");
        let linked_parent = root.join("linked");
        std::os::unix::fs::symlink(&root, &linked_parent).unwrap();
        let error = first_backup(linked_parent.join("outside.txt"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("symbolic link"), "{error}");
        let error = first_backup(root.clone()).await.unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");
        assert!(!history.directory().exists());
    }

    #[tokio::test]
    async fn backup_directories_that_are_symbolic_links_are_rejected() {
        let (_directory, root) = canonical_temp_directory();
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(FILE_HISTORY_DIRECTORY)).unwrap();
        let path = root.join("file.txt");
        std::fs::write(&path, "content").unwrap();
        let history = history(&root);

        let location = local_files(&root).locate(&path).unwrap();
        assert!(
            create_first_backup(history.backup_directory(), &location)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn only_the_latest_hundred_snapshots_are_kept_and_old_files_use_their_first_backup() {
        let (_directory, root) = canonical_temp_directory();
        let files = local_files(&root);
        let path = root.join("file.txt");
        std::fs::write(&path, "first").unwrap();
        let mut history = history(&root);
        history.make_snapshot(&files, Uuid::from_u128(0)).await;
        history.track_edit(&files, &path).await.unwrap();
        std::fs::write(&path, "second").unwrap();
        for index in 1..=MAX_SNAPSHOTS as u128 {
            history.make_snapshot(&files, Uuid::from_u128(index)).await;
        }
        assert_eq!(history.snapshots().len(), MAX_SNAPSHOTS);
        assert!(!history.can_restore(Uuid::from_u128(0)));
        assert!(matches!(
            history.rewind(&files, Uuid::from_u128(0)).await,
            Err(FileHistoryError::SnapshotNotFound)
        ));
        std::fs::write(&path, "third").unwrap();
        history.rewind(&files, Uuid::from_u128(1)).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[tokio::test]
    async fn copies_share_backups_through_hard_links_and_restore_independently() {
        let (_directory, root) = canonical_temp_directory();
        let files = local_files(&root);
        let path = root.join("file.txt");
        std::fs::write(&path, "before the copy").unwrap();
        let mut source = history(&root);
        source.make_snapshot(&files, Uuid::from_u128(1)).await;
        source.track_edit(&files, &path).await.unwrap();
        std::fs::write(&path, "after the copy").unwrap();
        source.make_snapshot(&files, Uuid::from_u128(2)).await;

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
        copy.rewind(&files, Uuid::from_u128(1)).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before the copy");
    }
}
