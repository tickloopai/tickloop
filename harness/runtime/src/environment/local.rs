//! The file operations of the Port for directories on this machine, on the local file backend of
//! the tools; [`FolderEnvironment`](super::FolderEnvironment) and Code's Local Environment share
//! them.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tickloop_harness_protocol::{FileIdentity, ToolResult};
use tickloop_harness_tools::local_files::{self, LocalFileError};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    DirectoryEntry, DirectoryListing, ExecutionAuthority, ExecutionError, ExecutionFacts,
    ExecutionIntent, ExecutionOutcome, ExecutionResult, ExecutionScope, FileAccess,
    FileCapabilities, FileContent, FileInspection, FileKind, FileMetadata, FilePath, FileQuery,
    FileRoot, FileStat, FileVersion, PreparedExecution, ROOT_NAME_MAX_BYTES, RootName, error_code,
};

const FILESYSTEM_ROOT: &str = "/";
/// The name of a root whose directory has no name of its own: the filesystem root.
const UNNAMED_ROOT: &str = "root";
/// Room kept below [`ROOT_NAME_MAX_BYTES`] for the suffix that tells clashing names apart.
const ROOT_NAME_SUFFIX_BYTES: usize = 8;
const IO_FAILED: &str = "Io";
const CANCELLED: &str = "Cancelled";

/// The Folder and Additional Directories of a scope as directories on this machine. Each root's
/// display path and local path are its real absolute path, its name is its directory name with a
/// suffix when another root took it, and every root, like every absolute path outside them, can be
/// read and written; the Harness decides what needs approval. Every operation resolves symbolic
/// links again and refuses a real path that leaves the root it was asked for.
#[derive(Clone, Debug, Default)]
pub struct LocalFiles {
    capabilities: FileCapabilities,
}

impl LocalFiles {
    /// No roots and nothing outside them for a scope without a Folder, whose Chat touches no file.
    pub fn for_scope(scope: &ExecutionScope) -> Self {
        let Some(folder) = &scope.folder else {
            return Self::default();
        };
        let mut roots: Vec<FileRoot> = Vec::new();
        let directories = std::iter::once((folder, true)).chain(
            scope
                .additional_directories
                .iter()
                .map(|directory| (directory, false)),
        );
        for (directory, is_folder) in directories {
            let Some(display_path) = real_absolute_path(directory) else {
                continue;
            };
            if roots.iter().any(|root| root.display_path == display_path) {
                continue;
            }
            roots.push(FileRoot {
                name: unique_root_name(&display_path, &roots),
                local_path: Some(PathBuf::from(&display_path)),
                display_path,
                access: FileAccess::ReadWrite,
                is_folder,
            });
        }
        Self {
            capabilities: FileCapabilities::new(roots, FileAccess::ReadWrite)
                .expect("local roots have distinct names and normalized real paths"),
        }
    }

    pub fn capabilities(&self) -> &FileCapabilities {
        &self.capabilities
    }

    pub async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ExecutionError> {
        let (root, below) = self.location(query.path(), FileAccess::Read)?;
        // Dropping this future, as a cancelled read does, stops a search still walking.
        let cancel = CancellationToken::new();
        let _stop_on_drop = cancel.clone().drop_guard();
        tokio::task::spawn_blocking(move || inspect(&root, &below, query, &cancel))
            .await
            .map_err(|error| ExecutionError::new(IO_FAILED, error.to_string()))?
            .map_err(|error| execution_error(error, false))
    }

    /// Resolves the file a `WriteFile` or `DeleteFile` changes and keeps its real path in the
    /// approval facts, which [`LocalFiles::execute`] then changes without following links.
    pub async fn prepare(
        &self,
        intent: ExecutionIntent,
        port: Uuid,
        authority: &ExecutionAuthority,
    ) -> Result<PreparedExecution, ExecutionError> {
        let path = intent
            .file_path()
            .ok_or_else(|| ExecutionError::new(error_code::INVALID_INPUT, "not a file change"))?;
        let (root, below) = self.location(path, FileAccess::ReadWrite)?;
        let target =
            tokio::task::spawn_blocking(move || local_files::resolve_target(&root, &below))
                .await
                .map_err(|error| ExecutionError::new(IO_FAILED, error.to_string()))?
                .map_err(|error| execution_error(error, false))?;
        let mut facts = ExecutionFacts::for_file(&intent, &self.capabilities.display(path))
            .expect("a file change has file facts");
        facts.approval.canonical_path = Some(target);
        Ok(PreparedExecution::new(port, authority, facts))
    }

    /// Carries out a change [`LocalFiles::prepare`] prepared and the Harness admitted. Whatever
    /// fails before the file is published changes nothing.
    pub async fn execute(
        &self,
        prepared: &PreparedExecution,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        let facts = prepared.facts();
        let (Some(path), Some(target)) = (
            facts.intent.file_path(),
            facts.approval.canonical_path.clone(),
        ) else {
            return ExecutionOutcome::NotDispatched(ExecutionError::new(
                error_code::INVALID_INPUT,
                "this file change was not prepared on this machine",
            ));
        };
        if cancel.is_cancelled() {
            return ExecutionOutcome::NotDispatched(ExecutionError::new(
                CANCELLED,
                "execution cancelled before dispatch",
            ));
        }
        let shown = self.capabilities.display(path);
        let operation = prepared.operation();
        let change = match facts.intent.clone() {
            ExecutionIntent::WriteFile {
                content, expected, ..
            } => {
                let expected = match expected.as_ref().map(version_identity) {
                    Some(None) => return conflict(&shown),
                    expected => expected.flatten(),
                };
                tokio::task::spawn_blocking(move || -> Result<_, LocalFileError> {
                    let published = local_files::write_file(
                        &target,
                        &content,
                        expected.as_ref(),
                        operation,
                        &cancel,
                    )?;
                    Ok((
                        json!({"status":"written", "path":shown, "bytes":content.len()}),
                        Some(file_metadata(&published)),
                    ))
                })
            }
            ExecutionIntent::DeleteFile { expected, .. } => {
                let Some(expected) = version_identity(&expected) else {
                    return conflict(&shown);
                };
                tokio::task::spawn_blocking(move || -> Result<_, LocalFileError> {
                    local_files::delete_file(&target, &expected)?;
                    Ok((json!({"status":"deleted", "path":shown}), None))
                })
            }
            _ => {
                return ExecutionOutcome::NotDispatched(ExecutionError::new(
                    error_code::INVALID_INPUT,
                    "not a file change",
                ));
            }
        };
        let finished: Result<(Value, Option<FileMetadata>), LocalFileError> = match change.await {
            Ok(finished) => finished,
            Err(error) => {
                return ExecutionOutcome::OutcomeUnknown {
                    operation,
                    reason: error.to_string(),
                };
            }
        };
        match finished {
            Ok((content, file)) => ExecutionOutcome::Completed(ExecutionResult {
                result: ToolResult {
                    content: content.to_string(),
                    is_error: false,
                    output_ref: None,
                },
                opened: None,
                shell: None,
                command: None,
                handoff: None,
                file: file.map(Box::new),
            }),
            Err(error @ LocalFileError::Unsettled { .. }) => ExecutionOutcome::OutcomeUnknown {
                operation,
                reason: error.to_string(),
            },
            Err(error) => ExecutionOutcome::NotDispatched(execution_error(error, true)),
        }
    }

    /// The local root of `path` and the path below it, once the Port is known to allow `needed`
    /// there.
    fn location(
        &self,
        path: &FilePath,
        needed: FileAccess,
    ) -> Result<(PathBuf, String), ExecutionError> {
        let (root, below) = match path.root() {
            Some(name) => {
                let root = self
                    .capabilities
                    .root(name)
                    .ok_or_else(|| outside_root(path))?;
                let local_path = root.local_path.clone().ok_or_else(|| outside_root(path))?;
                (local_path, path.path().to_owned())
            }
            None if self.capabilities.outside_roots() == FileAccess::None => {
                return Err(outside_root(path));
            }
            None => (
                PathBuf::from(FILESYSTEM_ROOT),
                path.path().trim_start_matches('/').to_owned(),
            ),
        };
        let access = self.capabilities.access(path);
        let allowed = if needed.can_write() {
            access.can_write()
        } else {
            access.can_read()
        };
        if !allowed {
            return Err(ExecutionError::new(
                error_code::ACCESS_DENIED,
                format!("{} does not allow this", self.capabilities.display(path)),
            ));
        }
        Ok((root, below))
    }
}

fn inspect(
    root: &Path,
    below: &str,
    query: FileQuery,
    cancel: &CancellationToken,
) -> Result<FileInspection, LocalFileError> {
    let target = local_files::resolve_existing(root, below)?;
    Ok(match query {
        FileQuery::Stat(_) => FileInspection::Stat(file_stat(&local_files::stat(&target)?)),
        FileQuery::Read { offset, limit, .. } => {
            let (metadata, data) = local_files::read_range(&target, offset, limit)?;
            FileInspection::Read(FileContent {
                metadata: file_metadata(&metadata),
                data,
            })
        }
        FileQuery::List { limit, .. } => {
            let (entries, truncated) = local_files::list_directory(&target, limit)?;
            FileInspection::List(DirectoryListing {
                entries: entries
                    .into_iter()
                    .map(|(name, file_type)| DirectoryEntry {
                        name,
                        kind: if file_type.is_file() {
                            FileKind::File
                        } else if file_type.is_dir() {
                            FileKind::Directory
                        } else {
                            FileKind::Other
                        },
                    })
                    .collect(),
                truncated,
            })
        }
        FileQuery::Glob { query, .. } => FileInspection::Glob(local_files::glob(
            &real_root(root)?,
            &target,
            &query,
            cancel,
        )?),
        FileQuery::Grep { query, .. } => FileInspection::Grep(local_files::grep(
            &real_root(root)?,
            &target,
            &query,
            cancel,
        )?),
    })
}

/// The real path the search results are shown below.
fn real_root(root: &Path) -> Result<PathBuf, LocalFileError> {
    root.canonicalize().map_err(|error| LocalFileError::Io {
        path: root.into(),
        error,
    })
}

/// The real absolute path of a directory; for one that does not exist yet, the real path of its
/// deepest existing ancestor with the missing names appended. `None` when it is not UTF-8 text.
fn real_absolute_path(directory: &str) -> Option<String> {
    let real = match Path::new(directory).canonicalize() {
        Ok(real) => real,
        Err(_) => {
            let absolute = std::path::absolute(directory).ok()?;
            let normalized =
                PathBuf::from(FilePath::outside_roots(absolute.to_str()?).ok()?.path());
            let mut missing_names = Vec::new();
            let mut candidate = normalized.as_path();
            let existing = loop {
                if let Ok(existing) = candidate.canonicalize() {
                    break existing;
                }
                missing_names.push(candidate.file_name()?);
                candidate = candidate.parent()?;
            };
            missing_names
                .iter()
                .rev()
                .fold(existing, |real, name| real.join(name))
        }
    };
    real.to_str().map(str::to_owned)
}

/// The directory's name, or that name with the first free `-2`, `-3`, ... suffix.
fn unique_root_name(display_path: &str, roots: &[FileRoot]) -> RootName {
    let mut base = Path::new(display_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(UNNAMED_ROOT)
        .to_owned();
    if base.len() > ROOT_NAME_MAX_BYTES - ROOT_NAME_SUFFIX_BYTES {
        let mut end = ROOT_NAME_MAX_BYTES - ROOT_NAME_SUFFIX_BYTES;
        while !base.is_char_boundary(end) {
            end -= 1;
        }
        base.truncate(end);
    }
    (1..)
        .map(|attempt| {
            if attempt == 1 {
                base.clone()
            } else {
                format!("{base}-{attempt}")
            }
        })
        .filter_map(|candidate| RootName::new(candidate).ok())
        .find(|candidate| roots.iter().all(|root| &root.name != candidate))
        .expect("some suffix is free")
}

fn file_stat(metadata: &std::fs::Metadata) -> FileStat {
    if metadata.is_file() {
        FileStat::File(file_metadata(metadata))
    } else if metadata.is_dir() {
        FileStat::Directory {
            modified: metadata.modified().ok(),
        }
    } else {
        FileStat::Other
    }
}

fn file_metadata(metadata: &std::fs::Metadata) -> FileMetadata {
    FileMetadata {
        version: local_version(&FileIdentity::from_metadata(metadata)),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

/// A local file's version is its identity: device, inode, length, modification and change time.
fn local_version(identity: &FileIdentity) -> FileVersion {
    FileVersion::new(format!(
        "{}:{}:{}:{}:{}:{}:{}",
        identity.device,
        identity.inode,
        identity.byte_len,
        identity.modified.0,
        identity.modified.1,
        identity.changed.0,
        identity.changed.1
    ))
}

/// The identity a local version names; `None` for a version no local file can have.
fn version_identity(version: &FileVersion) -> Option<FileIdentity> {
    let mut fields = version.as_str().split(':');
    let mut next = || fields.next()?.parse::<i64>().ok();
    let identity = FileIdentity {
        device: u64::try_from(next()?).ok()?,
        inode: u64::try_from(next()?).ok()?,
        byte_len: u64::try_from(next()?).ok()?,
        modified: (next()?, next()?),
        changed: (next()?, next()?),
    };
    (fields.next().is_none() && local_version(&identity) == *version).then_some(identity)
}

fn conflict(shown: &str) -> ExecutionOutcome {
    ExecutionOutcome::NotDispatched(ExecutionError::new(
        error_code::CONFLICT,
        format!("{shown} changed since it was read"),
    ))
}

fn outside_root(path: &FilePath) -> ExecutionError {
    ExecutionError::new(
        error_code::OUTSIDE_ROOT,
        format!("{} is outside this Chat's directories", path.path()),
    )
}

/// The code a caller acts on for a local failure. `prepared` is a change working on the real path
/// resolved when it was prepared: a symbolic link or a file where that path had a directory, or a
/// file where it had none, means the path changed since, which is a conflict. Elsewhere a
/// component that turned into a symbolic link after resolution leaves the root.
fn execution_error(error: LocalFileError, prepared: bool) -> ExecutionError {
    let code = match &error {
        LocalFileError::NotFound(_) => error_code::NOT_FOUND,
        LocalFileError::OutsideRoot(_) => error_code::OUTSIDE_ROOT,
        LocalFileError::IsDirectory(_) => error_code::IS_DIRECTORY,
        LocalFileError::NotADirectory(_) => error_code::NOT_A_DIRECTORY,
        LocalFileError::NotRegular(_) => error_code::UNSUPPORTED,
        LocalFileError::Conflict(_) => error_code::CONFLICT,
        LocalFileError::TooLarge(_) => error_code::TOO_LARGE,
        LocalFileError::InvalidInput(_) => error_code::INVALID_INPUT,
        LocalFileError::Cancelled => CANCELLED,
        LocalFileError::Io { error, .. } | LocalFileError::Unsettled { error, .. } => {
            match (error.raw_os_error(), prepared) {
                (Some(libc::ELOOP | libc::ENOTDIR | libc::EEXIST), true) => error_code::CONFLICT,
                (Some(libc::ELOOP), false) => error_code::OUTSIDE_ROOT,
                (Some(libc::ENOTDIR), false) => error_code::NOT_A_DIRECTORY,
                (Some(libc::EISDIR), _) => error_code::IS_DIRECTORY,
                (Some(libc::EACCES | libc::EPERM), _) => error_code::ACCESS_DENIED,
                _ => IO_FAILED,
            }
        }
    };
    ExecutionError::new(code, error.to_string())
}
