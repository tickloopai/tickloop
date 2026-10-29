//! Directories on this machine as the file roots of a Port, on the local file backend. The local
//! Execution Environments carry out their file operations with it, and it is the [`FileHost`] of
//! the Harness's own directories of a Chat and of tools used on their own.

use std::path::{Path, PathBuf};

use tickloop_harness_protocol::FileIdentity;
use tokio_util::sync::CancellationToken;

use crate::{
    file_port::{
        DirectoryEntry, DirectoryListing, ExecutionError, FileAccess, FileCapabilities,
        FileContent, FileHost, FileInspection, FileKind, FileMetadata, FilePath, FileQuery,
        FileRoot, FileStat, FileVersion, FileWriteError, FileWritePreparation, PreparedFileWrite,
        ROOT_NAME_MAX_BYTES, ResolvedPath, RootName, error_code,
    },
    local_files::{self, LocalFileError},
};

const FILESYSTEM_ROOT: &str = "/";
/// The name of a root whose directory has no name of its own: the filesystem root.
const UNNAMED_ROOT: &str = "root";
/// Room kept below [`ROOT_NAME_MAX_BYTES`] for the suffix that tells clashing names apart.
const ROOT_NAME_SUFFIX_BYTES: usize = 8;

/// Directories on this machine as roots. Each root's display path and local path are its real
/// absolute path, and its name is its directory name with a suffix when another root took it.
/// Every operation resolves symbolic links again and refuses a real path that leaves the root it
/// was asked for.
#[derive(Clone, Debug, Default)]
pub struct LocalRoots {
    capabilities: FileCapabilities,
}

impl LocalRoots {
    /// A Folder and its Additional Directories: every root, like every absolute path outside
    /// them, can be read and written, and the Harness decides what needs approval. Without a
    /// Folder there are no roots and nothing outside them.
    pub fn new(folder: Option<&str>, additional_directories: &[String]) -> Self {
        let Some(folder) = folder else {
            return Self::default();
        };
        let directories = std::iter::once((folder, true)).chain(
            additional_directories
                .iter()
                .map(|directory| (directory.as_str(), false)),
        );
        Self::with_roots(directories, FileAccess::ReadWrite)
    }

    /// The Folder and Additional Directories of `workspace`.
    pub fn for_workspace(workspace: &crate::Workspace) -> Self {
        let directories = workspace
            .additional_directories
            .iter()
            .filter_map(|directory| directory.to_str().map(str::to_owned))
            .collect::<Vec<_>>();
        Self::new(
            workspace.folder.as_deref().and_then(Path::to_str),
            &directories,
        )
    }

    /// The Harness's own directories of a Chat as roots that can be read and written, with nothing
    /// outside them.
    pub fn harness_directories(directories: &[&Path]) -> Self {
        Self::with_roots(
            directories
                .iter()
                .filter_map(|directory| Some((directory.to_str()?, false))),
            FileAccess::None,
        )
    }

    fn with_roots<'d>(
        directories: impl Iterator<Item = (&'d str, bool)>,
        outside_roots: FileAccess,
    ) -> Self {
        let mut roots: Vec<FileRoot> = Vec::new();
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
            capabilities: FileCapabilities::new(roots, outside_roots)
                .expect("local roots have distinct names and normalized real paths"),
        }
    }

    pub fn capabilities(&self) -> &FileCapabilities {
        &self.capabilities
    }

    /// The local root of `path` and the path below it, once the roots are known to allow
    /// `needed` there.
    pub fn location(
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

    /// Answers `query` on this thread, which it blocks; `cancel` stops a search still walking.
    pub fn inspect_blocking(
        &self,
        query: FileQuery,
        cancel: &CancellationToken,
    ) -> Result<FileInspection, ExecutionError> {
        let (root, below) = self.location(query.path(), FileAccess::Read)?;
        self.inspect_at(&root, &below, query, cancel)
            .map_err(|error| execution_error(error, false))
    }

    fn inspect_at(
        &self,
        root: &Path,
        below: &str,
        query: FileQuery,
        cancel: &CancellationToken,
    ) -> Result<FileInspection, LocalFileError> {
        if let FileQuery::Resolve(_) = query {
            let real = local_files::resolve_anywhere(root, below)?;
            let kind = std::fs::symlink_metadata(&real)
                .ok()
                .map(|metadata| file_kind(&metadata.file_type()));
            let path = real
                .to_str()
                .and_then(|real| self.capabilities.resolve(real).ok())
                .ok_or_else(|| {
                    LocalFileError::InvalidInput(format!(
                        "{} is not a path the file tools can name",
                        real.display()
                    ))
                })?;
            return Ok(FileInspection::Resolved(ResolvedPath { path, kind }));
        }
        let target = local_files::resolve_existing(root, below)?;
        Ok(match query {
            FileQuery::Resolve(_) => unreachable!("answered above"),
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
                            kind: file_kind(&file_type),
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

    /// The real path a `WriteFile` or `DeleteFile` of `path` changes, resolved on this thread,
    /// which it blocks.
    pub fn resolve_change(&self, path: &FilePath) -> Result<PathBuf, ExecutionError> {
        let (root, below) = self.location(path, FileAccess::ReadWrite)?;
        local_files::resolve_target(&root, &below).map_err(|error| execution_error(error, false))
    }
}

/// Writes `content` to the real path a change was resolved to, while the file still has the
/// `expected` version (or does not exist when it is `None`), without following links.
pub fn write_resolved(
    target: &Path,
    content: &[u8],
    expected: Option<&FileVersion>,
    operation: uuid::Uuid,
    cancel: &CancellationToken,
) -> Result<FileMetadata, LocalFileError> {
    let expected = match expected.map(file_identity) {
        Some(None) => return Err(LocalFileError::Conflict(target.into())),
        expected => expected.flatten(),
    };
    let published = local_files::write_file(target, content, expected.as_ref(), operation, cancel)?;
    Ok(file_metadata(&published))
}

/// Deletes the file at the real path a change was resolved to while it still has `expected`.
pub fn delete_resolved(target: &Path, expected: &FileVersion) -> Result<(), LocalFileError> {
    let expected =
        file_identity(expected).ok_or_else(|| LocalFileError::Conflict(target.into()))?;
    local_files::delete_file(target, &expected)
}

/// The tools on their own and the Harness's own directories reach these roots on the calling
/// thread, which every operation blocks.
#[async_trait::async_trait]
impl FileHost for LocalRoots {
    fn capabilities(&self) -> FileCapabilities {
        self.capabilities.clone()
    }

    async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ExecutionError> {
        self.inspect_blocking(query, &CancellationToken::new())
    }

    async fn prepare_write(
        &self,
        path: FilePath,
        content: Vec<u8>,
        expected: Option<FileVersion>,
    ) -> Result<FileWritePreparation, ExecutionError> {
        Ok(FileWritePreparation::new(Box::new(LocalWrite {
            target: self.resolve_change(&path)?,
            content,
            expected,
            operation: tickloop_ids::generate(),
        })))
    }
}

struct LocalWrite {
    target: PathBuf,
    content: Vec<u8>,
    expected: Option<FileVersion>,
    operation: uuid::Uuid,
}

#[async_trait::async_trait]
impl PreparedFileWrite for LocalWrite {
    async fn write(
        self: Box<Self>,
        cancel: CancellationToken,
    ) -> Result<FileMetadata, FileWriteError> {
        write_resolved(
            &self.target,
            &self.content,
            self.expected.as_ref(),
            self.operation,
            &cancel,
        )
        .map_err(|error| match error {
            LocalFileError::Unsettled { .. } => FileWriteError::OutcomeUnknown(error.to_string()),
            error => FileWriteError::NotWritten(execution_error(error, true)),
        })
    }
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

fn file_kind(file_type: &std::fs::FileType) -> FileKind {
    if file_type.is_file() {
        FileKind::File
    } else if file_type.is_dir() {
        FileKind::Directory
    } else {
        FileKind::Other
    }
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

/// The identity a local file's version names; `None` for a version no local file can have.
pub fn file_identity(version: &FileVersion) -> Option<FileIdentity> {
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
pub fn execution_error(error: LocalFileError, prepared: bool) -> ExecutionError {
    let code = match &error {
        LocalFileError::NotFound(_) => error_code::NOT_FOUND,
        LocalFileError::OutsideRoot(_) => error_code::OUTSIDE_ROOT,
        LocalFileError::IsDirectory(_) => error_code::IS_DIRECTORY,
        LocalFileError::NotADirectory(_) => error_code::NOT_A_DIRECTORY,
        LocalFileError::NotRegular(_) => error_code::UNSUPPORTED,
        LocalFileError::Conflict(_) => error_code::CONFLICT,
        LocalFileError::TooLarge(_) => error_code::TOO_LARGE,
        LocalFileError::InvalidInput(_) => error_code::INVALID_INPUT,
        LocalFileError::Cancelled => error_code::CANCELLED,
        LocalFileError::Io { error, .. } | LocalFileError::Unsettled { error, .. } => {
            match (error.raw_os_error(), prepared) {
                (Some(libc::ELOOP | libc::ENOTDIR | libc::EEXIST), true) => error_code::CONFLICT,
                (Some(libc::ELOOP), false) => error_code::OUTSIDE_ROOT,
                (Some(libc::ENOTDIR), false) => error_code::NOT_A_DIRECTORY,
                (Some(libc::EISDIR), _) => error_code::IS_DIRECTORY,
                (Some(libc::EACCES | libc::EPERM), _) => error_code::ACCESS_DENIED,
                _ => error_code::IO,
            }
        }
    };
    ExecutionError::new(code, error.to_string())
}
