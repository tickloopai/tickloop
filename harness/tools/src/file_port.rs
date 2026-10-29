//! The files of the Execution Environment Port: each directory's root name and file capability,
//! paths below a root, the file queries and their answers, and [`FileHost`], through which the file
//! tools reach them. The runtime's `ExecutionPort` states their semantics and re-exports them; they
//! live here so that the tools need no runtime.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::file_search::{FileSearchError, GlobMatches, GlobQuery, GrepMatches, GrepQuery};

/// Why an Execution Environment refused or failed an operation; callers act on `code`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ExecutionError {
    pub code: String,
    pub message: String,
}

impl ExecutionError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// The codes of [`ExecutionError`] that callers act on.
pub mod error_code {
    pub const NOT_FOUND: &str = "NotFound";
    pub const CONFLICT: &str = "Conflict";
    pub const OUTSIDE_ROOT: &str = "OutsideRoot";
    pub const ACCESS_DENIED: &str = "AccessDenied";
    pub const IS_DIRECTORY: &str = "IsDirectory";
    pub const NOT_A_DIRECTORY: &str = "NotADirectory";
    pub const TOO_LARGE: &str = "TooLarge";
    pub const UNSUPPORTED: &str = "Unsupported";
    pub const INVALID_INPUT: &str = "InvalidInput";
    pub const AUTHORITY_REVOKED: &str = "AuthorityRevoked";
    pub const IO: &str = "Io";
    pub const CANCELLED: &str = "Cancelled";
}

/// The longest root name, in bytes: a directory name with room for a suffix.
pub const ROOT_NAME_MAX_BYTES: usize = 255;
const PARENT_DIRECTORY: &str = "..";
const CURRENT_DIRECTORY: &str = ".";

/// The name of a Folder or Additional Directory root, unique among the roots of one Port.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootName(String);

impl RootName {
    pub fn new(name: impl Into<String>) -> Result<Self, ExecutionError> {
        let name = name.into();
        if name.is_empty()
            || name.len() > ROOT_NAME_MAX_BYTES
            || name.contains(['/', '\0'])
            || name == CURRENT_DIRECTORY
            || name == PARENT_DIRECTORY
        {
            return Err(ExecutionError::new(
                error_code::INVALID_INPUT,
                format!(
                    "root name {name:?} must be 1 to {ROOT_NAME_MAX_BYTES} bytes without `/`, NUL, `.` or `..`"
                ),
            ));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RootName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccess {
    #[default]
    None,
    Read,
    ReadWrite,
}

impl FileAccess {
    pub fn can_read(self) -> bool {
        self != Self::None
    }

    pub fn can_write(self) -> bool {
        self == Self::ReadWrite
    }
}

/// A Folder or Additional Directory as the environment offers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRoot {
    pub name: RootName,
    /// The absolute path the model sees for the root: locally its real path, elsewhere what the
    /// host chooses, such as `/<name>`.
    pub display_path: String,
    pub access: FileAccess,
    pub is_folder: bool,
    /// Where the root is on the Harness's own machine, given only when it is there, for what
    /// only that machine can do (Git environment information, linked worktrees).
    pub local_path: Option<PathBuf>,
}

/// The file roots of a bound Port and what it allows outside them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileCapabilities {
    roots: Vec<FileRoot>,
    outside_roots: FileAccess,
}

impl FileCapabilities {
    /// Root names and display paths must be unique, display paths absolute and normalized, and at
    /// most one root is the Folder. `outside_roots` applies to absolute paths below no root.
    pub fn new(roots: Vec<FileRoot>, outside_roots: FileAccess) -> Result<Self, ExecutionError> {
        for (index, root) in roots.iter().enumerate() {
            let normalized = absolute_components(&root.display_path)
                .ok()
                .map(|components| absolute_path(&components));
            if normalized.as_deref() != Some(root.display_path.as_str()) {
                return Err(ExecutionError::new(
                    error_code::INVALID_INPUT,
                    format!(
                        "root {} has a display path that is not absolute and normalized: {}",
                        root.name, root.display_path
                    ),
                ));
            }
            if roots[..index].iter().any(|earlier| {
                earlier.name == root.name || earlier.display_path == root.display_path
            }) {
                return Err(ExecutionError::new(
                    error_code::INVALID_INPUT,
                    format!("root {} repeats a name or display path", root.name),
                ));
            }
        }
        if roots.iter().filter(|root| root.is_folder).count() > 1 {
            return Err(ExecutionError::new(
                error_code::INVALID_INPUT,
                "only one root can be the Folder",
            ));
        }
        Ok(Self {
            roots,
            outside_roots,
        })
    }

    pub fn roots(&self) -> &[FileRoot] {
        &self.roots
    }

    pub fn outside_roots(&self) -> FileAccess {
        self.outside_roots
    }

    pub fn root(&self, name: &RootName) -> Option<&FileRoot> {
        self.roots.iter().find(|root| &root.name == name)
    }

    pub fn folder(&self) -> Option<&FileRoot> {
        self.roots.iter().find(|root| root.is_folder)
    }

    /// What the Port allows on `path`; a root it does not have allows nothing.
    pub fn access(&self, path: &FilePath) -> FileAccess {
        match path.root() {
            Some(name) => self.root(name).map_or(FileAccess::None, |root| root.access),
            None => self.outside_roots,
        }
    }

    /// The path the model wrote, as a path below a root: relative paths start at the Folder,
    /// `.` and `..` are resolved first, and an absolute path belongs to the root with the longest
    /// display path containing it. A path that leaves one root therefore lands in another or
    /// outside every root, where [`FileCapabilities::outside_roots`] decides; `..` above `/` is
    /// refused.
    pub fn resolve(&self, requested: &str) -> Result<FilePath, ExecutionError> {
        let absolute = if requested.starts_with('/') {
            requested.to_owned()
        } else {
            let folder = self.folder().ok_or_else(|| {
                ExecutionError::new(
                    error_code::INVALID_INPUT,
                    format!("relative path {requested} needs a Folder"),
                )
            })?;
            format!("{}/{requested}", folder.display_path)
        };
        let components = absolute_components(&absolute)?;
        let owner = self
            .roots
            .iter()
            .filter_map(|root| {
                let prefix = absolute_components(&root.display_path).ok()?;
                components
                    .starts_with(&prefix)
                    .then_some((prefix.len(), root))
            })
            .max_by_key(|(depth, _)| *depth);
        Ok(match owner {
            Some((depth, root)) => FilePath {
                root: Some(root.name.clone()),
                path: components[depth..].join("/"),
            },
            None => FilePath {
                root: None,
                path: absolute_path(&components),
            },
        })
    }

    /// The absolute path the model sees for `path`.
    pub fn display(&self, path: &FilePath) -> String {
        let Some(root) = path.root().and_then(|name| self.root(name)) else {
            return path.path().to_owned();
        };
        match (root.display_path.as_str(), path.path()) {
            (display, "") => display.to_owned(),
            ("/", below) => format!("/{below}"),
            (display, below) => format!("{display}/{below}"),
        }
    }
}

/// A normalized path: below a root (`/`-separated, without a leading `/`, empty for the root
/// itself), or an absolute path outside every root.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FilePath {
    root: Option<RootName>,
    path: String,
}

impl FilePath {
    /// `path` below `root`; `.` is dropped and `..` that leaves the root is refused.
    pub fn in_root(root: RootName, path: &str) -> Result<Self, ExecutionError> {
        if path.starts_with('/') {
            return Err(ExecutionError::new(
                error_code::INVALID_INPUT,
                format!("{path} is absolute; a path in a root is relative to it"),
            ));
        }
        Ok(Self {
            root: Some(root),
            path: normalized_components(path)?.join("/"),
        })
    }

    /// An absolute path outside every root, with `.` and `..` resolved.
    pub fn outside_roots(path: &str) -> Result<Self, ExecutionError> {
        Ok(Self {
            root: None,
            path: absolute_path(&absolute_components(path)?),
        })
    }

    pub fn root(&self) -> Option<&RootName> {
        self.root.as_ref()
    }

    /// Below the root, or absolute when the path is outside every root.
    pub fn path(&self) -> &str {
        &self.path
    }
}

fn normalized_components(path: &str) -> Result<Vec<&str>, ExecutionError> {
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | CURRENT_DIRECTORY => {}
            PARENT_DIRECTORY => {
                if components.pop().is_none() {
                    return Err(ExecutionError::new(
                        error_code::OUTSIDE_ROOT,
                        format!("{path} leaves its root"),
                    ));
                }
            }
            name => components.push(name),
        }
    }
    Ok(components)
}

fn absolute_components(path: &str) -> Result<Vec<&str>, ExecutionError> {
    if !path.starts_with('/') {
        return Err(ExecutionError::new(
            error_code::INVALID_INPUT,
            format!("{path} is not absolute"),
        ));
    }
    normalized_components(path)
}

fn absolute_path(components: &[&str]) -> String {
    format!("/{}", components.join("/"))
}

/// An opaque version of a file's content; versions are only compared for equality. The local
/// environment derives it from the file's identity; others choose their own.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileVersion(String);

impl FileVersion {
    pub fn new(version: impl Into<String>) -> Self {
        Self(version.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMetadata {
    pub version: FileVersion,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileStat {
    File(FileMetadata),
    Directory {
        modified: Option<SystemTime>,
    },
    /// Something that is neither, such as a device; file operations refuse it.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Other,
}

/// The bytes of a file in the requested range, with the whole file's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContent {
    pub metadata: FileMetadata,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: FileKind,
}

/// A directory's entries sorted by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryListing {
    pub entries: Vec<DirectoryEntry>,
    /// More entries exist than the query's limit.
    pub truncated: bool,
}

/// A read of the environment's files. `Glob` and `Grep` search below `path` and answer with paths
/// below its root (below `/` outside every root).
pub enum FileQuery {
    /// The path the environment acts on for this one once symbolic links are resolved, below the
    /// root that holds it or outside every root, and what is there. The names of a missing path
    /// below its deepest existing directory are kept; a symbolic link whose target is missing
    /// leaves every root ([`error_code::OUTSIDE_ROOT`]). An environment without links answers the
    /// path itself.
    Resolve(FilePath),
    Stat(FilePath),
    /// `limit` bytes from `offset`, fewer only at the end of the file.
    Read {
        path: FilePath,
        offset: u64,
        limit: usize,
    },
    List {
        path: FilePath,
        limit: usize,
    },
    Glob {
        path: FilePath,
        query: GlobQuery,
    },
    Grep {
        path: FilePath,
        query: GrepQuery,
    },
}

impl FileQuery {
    pub fn path(&self) -> &FilePath {
        match self {
            Self::Resolve(path)
            | Self::Stat(path)
            | Self::Read { path, .. }
            | Self::List { path, .. }
            | Self::Glob { path, .. }
            | Self::Grep { path, .. } => path,
        }
    }
}

#[derive(Debug)]
pub enum FileInspection {
    Resolved(ResolvedPath),
    Stat(FileStat),
    Read(FileContent),
    List(DirectoryListing),
    Glob(GlobMatches),
    Grep(GrepMatches),
}

/// What [`FileQuery::Resolve`] answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPath {
    pub path: FilePath,
    /// `None` when nothing is there yet.
    pub kind: Option<FileKind>,
}

impl From<FileSearchError> for ExecutionError {
    fn from(error: FileSearchError) -> Self {
        let code = match &error {
            FileSearchError::InvalidInput(_) => error_code::INVALID_INPUT,
            FileSearchError::NotFound(_) => error_code::NOT_FOUND,
            FileSearchError::NotADirectory(_) => error_code::NOT_A_DIRECTORY,
            FileSearchError::TooLarge(_) => error_code::TOO_LARGE,
        };
        Self::new(code, error.to_string())
    }
}

/// The files of a Chat's Folder and Additional Directories as its file tools reach them: through
/// the Chat's Execution Environment Port, or on this machine for tools used on their own.
#[async_trait::async_trait]
pub trait FileHost: Send + Sync {
    /// The roots, fixed when the Port was bound.
    fn capabilities(&self) -> FileCapabilities;
    async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ExecutionError>;
    /// Prepares a `WriteFile` of `content` that expects the file at `expected`, or no file when it
    /// is `None`. Nothing is written until the preparation is carried out.
    async fn prepare_write(
        &self,
        path: FilePath,
        content: Vec<u8>,
        expected: Option<FileVersion>,
    ) -> Result<FileWritePreparation, ExecutionError>;
}

/// Why a prepared write reports no file.
#[derive(Debug)]
pub enum FileWriteError {
    /// Nothing was dispatched, or the environment refused the write and changed nothing.
    NotWritten(ExecutionError),
    /// The write was dispatched and its answer lost: it took effect whole or not at all, and
    /// sending it again could repeat it.
    OutcomeUnknown(String),
}

#[async_trait::async_trait]
pub trait PreparedFileWrite: Send + Sync {
    async fn write(
        self: Box<Self>,
        cancel: CancellationToken,
    ) -> Result<FileMetadata, FileWriteError>;
}

/// A write prepared before approval and carried out at most once after it, however often the
/// call that holds it is cloned.
#[derive(Clone)]
pub struct FileWritePreparation {
    action: Arc<Mutex<Option<Box<dyn PreparedFileWrite>>>>,
}

impl std::fmt::Debug for FileWritePreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileWritePreparation")
            .finish_non_exhaustive()
    }
}

impl FileWritePreparation {
    pub fn new(action: Box<dyn PreparedFileWrite>) -> Self {
        Self {
            action: Arc::new(Mutex::new(Some(action))),
        }
    }

    pub async fn write(&self, cancel: CancellationToken) -> Result<FileMetadata, FileWriteError> {
        let action = self
            .action
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                FileWriteError::NotWritten(ExecutionError::new(
                    error_code::INVALID_INPUT,
                    "this write was already carried out",
                ))
            })?;
        action.write(cancel).await
    }
}

#[cfg(test)]
#[path = "file_port_tests.rs"]
mod tests;
