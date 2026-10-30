//! Files on this machine without any tool's rules: paths resolved inside a root directory, files
//! read whole or by byte range without following symbolic links, changes published through a
//! temporary file checked against the file it replaces, deletion of the expected file only, and
//! ripgrep's `Glob` and `Grep` walks. The file tools and the local Execution Environments share it;
//! read-before-write, approval and sensitive-path rules stay with the tools.

use std::{
    ffi::{CString, OsStr},
    fs::{File, FileType, Metadata},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

use ignore::WalkBuilder;
use tickloop_harness_base::private_fs::{
    OpenTarget, PRIVATE_FILE_MODE, open_without_following_links,
};
pub use tickloop_harness_protocol::FileIdentity;
use tokio_util::sync::CancellationToken;

use crate::{
    file_search::{
        FileSearchError, FoundFile, GLOB_SKIPPED_NAMES, GREP_SKIPPED_NAMES, GlobMatches, GlobQuery,
        GrepMatches, GrepQuery, GrepSearch, RIPGREP_IGNORE_FILE, file_types, oldest_first,
        override_matcher,
    },
    is_sensitive_path,
};

/// Permission bits kept when overwriting an existing file; new files get private file permissions.
const PRESERVED_PERMISSION_BITS: u32 = 0o777;
/// Parent directories created for a new file get the conventional mode, narrowed by the umask.
const NEW_DIRECTORY_MODE: u32 = 0o777;
const TEMPORARY_FILE_PREFIX: &str = ".tickloop-write-";

#[derive(Debug, thiserror::Error)]
pub enum LocalFileError {
    #[error("{} does not exist", .0.display())]
    NotFound(PathBuf),
    /// The path, once symbolic links are resolved, leaves its root, or a dangling link makes that
    /// impossible to rule out.
    #[error("{} leads outside its directory", .0.display())]
    OutsideRoot(PathBuf),
    #[error("{} is a directory", .0.display())]
    IsDirectory(PathBuf),
    #[error("{} is not a directory", .0.display())]
    NotADirectory(PathBuf),
    #[error("{} is not a regular file", .0.display())]
    NotRegular(PathBuf),
    /// The file is no longer the one the change expected, and nothing changed.
    #[error("{} changed since it was read", .0.display())]
    Conflict(PathBuf),
    #[error("{} is larger than 8 MiB", .0.display())]
    TooLarge(PathBuf),
    #[error("{0}")]
    InvalidInput(String),
    #[error("the file operation was cancelled")]
    Cancelled,
    #[error("file access failed for {}: {error}", path.display())]
    Io { path: PathBuf, error: io::Error },
    /// The change took effect, but completing it failed.
    #[error("the change to {} took effect, but completing it failed: {error}", path.display())]
    Unsettled { path: PathBuf, error: io::Error },
}

impl LocalFileError {
    fn io(path: &Path, error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::NotFound(path.into())
        } else {
            Self::Io {
                path: path.into(),
                error,
            }
        }
    }
}

impl From<FileSearchError> for LocalFileError {
    fn from(error: FileSearchError) -> Self {
        match error {
            FileSearchError::InvalidInput(message) => Self::InvalidInput(message),
            FileSearchError::NotFound(path) => Self::NotFound(path.into()),
            FileSearchError::NotADirectory(path) => Self::NotADirectory(path.into()),
            FileSearchError::TooLarge(path) => Self::TooLarge(path.into()),
        }
    }
}

/// The real path of the existing `below` in `root`; `below` is `/`-separated and relative (empty
/// for the root itself). Symbolic links are resolved, and the result must stay inside the real
/// path of `root`.
pub fn resolve_existing(root: &Path, below: &str) -> Result<PathBuf, LocalFileError> {
    let real_root = real_path(root)?;
    let requested = join_below(&real_root, below)?;
    let resolved = requested
        .canonicalize()
        .map_err(|error| LocalFileError::io(&requested, error))?;
    inside(&real_root, resolved, &requested)
}

/// Like [`resolve_existing`] for a file to write, which may not exist yet: its deepest existing
/// ancestor is resolved and the missing names are appended. A symbolic link whose target is
/// missing can lead anywhere, so it counts as leaving the root.
pub fn resolve_target(root: &Path, below: &str) -> Result<PathBuf, LocalFileError> {
    let real_root = real_path(root)?;
    let requested = join_below(&real_root, below)?;
    inside(&real_root, real_target(&requested)?, &requested)
}

/// The real path `below` in `root` leads to, inside the root or not: resolved like
/// [`resolve_target`], with a dangling symbolic link refused the same way.
pub fn resolve_anywhere(root: &Path, below: &str) -> Result<PathBuf, LocalFileError> {
    real_target(&join_below(&real_path(root)?, below)?)
}

fn real_target(requested: &Path) -> Result<PathBuf, LocalFileError> {
    let mut missing_names = Vec::new();
    let mut candidate = requested;
    let existing = loop {
        match candidate.canonicalize() {
            Ok(existing) => break existing,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(LocalFileError::io(candidate, error)),
        }
        if std::fs::symlink_metadata(candidate).is_ok() {
            return Err(LocalFileError::OutsideRoot(requested.into()));
        }
        // The real root exists, so the walk up stops at it at the latest.
        let (Some(parent), Some(name)) = (candidate.parent(), candidate.file_name()) else {
            return Err(LocalFileError::NotFound(requested.into()));
        };
        missing_names.push(name);
        candidate = parent;
    };
    Ok(missing_names
        .iter()
        .rev()
        .fold(existing, |resolved, name| resolved.join(name)))
}

fn real_path(root: &Path) -> Result<PathBuf, LocalFileError> {
    root.canonicalize()
        .map_err(|error| LocalFileError::io(root, error))
}

fn join_below(root: &Path, below: &str) -> Result<PathBuf, LocalFileError> {
    let mut joined = root.to_path_buf();
    for component in Path::new(below).components() {
        match component {
            Component::Normal(name) => joined.push(name),
            Component::CurDir => {}
            _ => {
                return Err(LocalFileError::InvalidInput(format!(
                    "{below} must be a relative path without `..`"
                )));
            }
        }
    }
    Ok(joined)
}

fn inside(root: &Path, resolved: PathBuf, requested: &Path) -> Result<PathBuf, LocalFileError> {
    if resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(LocalFileError::OutsideRoot(requested.into()))
    }
}

/// `path` below `root`, `/`-separated; `path` itself when it is not below `root`.
fn below_root(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// The metadata of whatever is at the absolute, normalized `path`, which must have no symbolic
/// link in any component.
pub fn stat(path: &Path) -> Result<Metadata, LocalFileError> {
    open_without_following_links(path, OpenTarget::Entry)
        .and_then(|entry| entry.metadata())
        .map_err(|error| LocalFileError::io(path, error))
}

/// Opens the regular file at the absolute, normalized `path` without following symbolic links.
pub fn open_file(path: &Path) -> Result<(File, Metadata), LocalFileError> {
    let file = open_without_following_links(path, OpenTarget::Entry)
        .map_err(|error| LocalFileError::io(path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| LocalFileError::io(path, error))?;
    if metadata.is_dir() {
        return Err(LocalFileError::IsDirectory(path.into()));
    }
    if !metadata.is_file() {
        return Err(LocalFileError::NotRegular(path.into()));
    }
    Ok((file, metadata))
}

/// `limit` bytes of the regular file at `path` from `offset`, fewer at its end, with the metadata
/// the file had when it was opened.
pub fn read_range(
    path: &Path,
    offset: u64,
    limit: usize,
) -> Result<(Metadata, Vec<u8>), LocalFileError> {
    let (mut file, metadata) = open_file(path)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| LocalFileError::io(path, error))?;
    let available = usize::try_from(metadata.len().saturating_sub(offset)).unwrap_or(usize::MAX);
    let mut data = Vec::with_capacity(limit.min(available));
    file.take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut data)
        .map_err(|error| LocalFileError::io(path, error))?;
    Ok((metadata, data))
}

/// The first `limit` entries of the directory at `path` by name, with their own types (a symbolic
/// link is not followed), and whether it has more.
pub fn list_directory(
    path: &Path,
    limit: usize,
) -> Result<(Vec<(String, FileType)>, bool), LocalFileError> {
    list_directory_excluding(path, limit, &[])
}

pub(crate) fn list_directory_excluding(
    path: &Path,
    limit: usize,
    excluded: &[PathBuf],
) -> Result<(Vec<(String, FileType)>, bool), LocalFileError> {
    if !stat(path)?.is_dir() {
        return Err(LocalFileError::NotADirectory(path.into()));
    }
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| LocalFileError::io(path, error))?
        .filter_map(|entry| match entry {
            Ok(entry) if excluded.iter().any(|path| entry.path().starts_with(path)) => None,
            Ok(entry) => Some(
                entry
                    .file_type()
                    .map(|kind| (entry.file_name().to_string_lossy().into_owned(), kind)),
            ),
            Err(error) => Some(Err(error)),
        })
        .collect::<io::Result<Vec<_>>>()
        .map_err(|error| LocalFileError::io(path, error))?;
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    Ok((entries, truncated))
}

/// Replaces the file at the absolute, normalized `path`, whose components must not be symbolic
/// links. The content goes to a temporary file in the target directory first and is then
/// published atomically: a hard link for a new file, a rename over an existing one. The file must
/// still have the `expected` identity, or not exist when it is `None`, both before staging and
/// right before publishing; otherwise nothing changes and the write is a conflict. An existing
/// file keeps its permission bits, and a new one gets private permissions and its missing parent
/// directories. Returns the metadata of the published file.
pub fn write_file(
    path: &Path,
    content: &[u8],
    expected: Option<&FileIdentity>,
    operation: uuid::Uuid,
    cancel: &CancellationToken,
) -> Result<Metadata, LocalFileError> {
    let (parent, name) = split_path(path)?;
    let create_mode = expected.is_none().then_some(NEW_DIRECTORY_MODE);
    let directory = open_without_following_links(parent, OpenTarget::Directory { create_mode })
        .map_err(|error| LocalFileError::io(parent, error))?;
    let current = entry_at(&directory, &name, path)?;
    if current.as_ref().map(FileIdentity::from_metadata).as_ref() != expected {
        return Err(LocalFileError::Conflict(path.into()));
    }
    let mode = current
        .as_ref()
        .map(|metadata| metadata.mode() & PRESERVED_PERMISSION_BITS)
        .unwrap_or(PRIVATE_FILE_MODE);
    let temporary = CString::new(format!("{TEMPORARY_FILE_PREFIX}{operation}"))
        .expect("an operation id has no NUL");
    // SAFETY: The directory descriptor is valid and the temporary file name is NUL-terminated; with
    // O_CREAT the mode argument is passed as required.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            PRIVATE_FILE_MODE as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(LocalFileError::io(path, io::Error::last_os_error()));
    }
    // SAFETY: The descriptor returned by a successful openat is not held by anything else;
    // ownership passes to `File`.
    let mut staged = unsafe { File::from_raw_fd(descriptor) };
    let published = (|| {
        let failed = |error| LocalFileError::io(path, error);
        staged.write_all(content).map_err(failed)?;
        // SAFETY: The descriptor is valid while `staged` is alive.
        if unsafe { libc::fchmod(staged.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(failed(io::Error::last_os_error()));
        }
        staged.sync_all().map_err(failed)?;
        if cancel.is_cancelled() {
            return Err(LocalFileError::Cancelled);
        }
        let current = entry_at(&directory, &name, path)?;
        if current.as_ref().map(FileIdentity::from_metadata).as_ref() != expected {
            return Err(LocalFileError::Conflict(path.into()));
        }
        #[cfg(test)]
        fail_before_publish(path)?;
        let published = if expected.is_none() {
            // A new target must not replace a file that appeared only after approval: the hard link
            // fails when the target already exists.
            // SAFETY: Both names are NUL-terminated and under the same valid directory descriptor;
            // with flags 0 symlinks are not followed.
            unsafe {
                libc::linkat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            }
        } else {
            // SAFETY: Both names are NUL-terminated and under the same valid directory descriptor.
            unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                )
            }
        };
        if published != 0 {
            return Err(failed(io::Error::last_os_error()));
        }
        directory
            .sync_all()
            .map_err(|error| LocalFileError::Unsettled {
                path: parent.into(),
                error,
            })
    })();
    // SAFETY: The directory descriptor is valid and the temporary file name is NUL-terminated;
    // after a successful publish, what is removed is the leftover temporary name.
    unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0) };
    published?;
    // The staged descriptor is the published file, so its identity is ours even if the name is
    // replaced again right away; it is read once the temporary name is gone, which changes it.
    staged
        .metadata()
        .map_err(|error| LocalFileError::Unsettled {
            path: path.into(),
            error,
        })
}

/// Removes the regular file at the absolute, normalized `path`, whose components must not be
/// symbolic links, while it still has the `expected` identity.
pub fn delete_file(path: &Path, expected: &FileIdentity) -> Result<(), LocalFileError> {
    let (parent, name) = split_path(path)?;
    let directory =
        open_without_following_links(parent, OpenTarget::Directory { create_mode: None })
            .map_err(|error| LocalFileError::io(parent, error))?;
    let current =
        entry_at(&directory, &name, path)?.ok_or_else(|| LocalFileError::NotFound(path.into()))?;
    if &FileIdentity::from_metadata(&current) != expected {
        return Err(LocalFileError::Conflict(path.into()));
    }
    // SAFETY: The directory descriptor is valid and the name is a single NUL-terminated path
    // segment.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(LocalFileError::io(path, io::Error::last_os_error()));
    }
    Ok(())
}

fn split_path(path: &Path) -> Result<(&Path, CString), LocalFileError> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(LocalFileError::InvalidInput(format!(
            "{} has no parent directory",
            path.display()
        )));
    };
    let name = CString::new(name.as_bytes())
        .map_err(|_| LocalFileError::InvalidInput("path contains NUL".into()))?;
    Ok((parent, name))
}

/// The metadata of the regular file `name` in `directory` without following a symbolic link, or
/// `None` when there is none.
fn entry_at(
    directory: &File,
    name: &CString,
    path: &Path,
) -> Result<Option<Metadata>, LocalFileError> {
    // SAFETY: The directory descriptor is valid and the name is NUL-terminated.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(LocalFileError::io(path, error));
    }
    // SAFETY: The descriptor returned by a successful openat is not held by anything else;
    // ownership passes to `File`.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file
        .metadata()
        .map_err(|error| LocalFileError::io(path, error))?;
    if metadata.is_dir() {
        return Err(LocalFileError::IsDirectory(path.into()));
    }
    if !metadata.is_file() {
        return Err(LocalFileError::NotRegular(path.into()));
    }
    Ok(Some(metadata))
}

#[cfg(test)]
thread_local! {
    /// Makes this thread's next publication fail right before the file is published.
    pub(crate) static FAIL_BEFORE_PUBLISH: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn fail_before_publish(path: &Path) -> Result<(), LocalFileError> {
    if FAIL_BEFORE_PUBLISH.with(|failure| failure.get()) {
        Err(LocalFileError::io(
            path,
            io::Error::other("injected publication failure"),
        ))
    } else {
        Ok(())
    }
}

/// A walk that never follows symbolic links and prunes skipped names and sensitive paths below
/// the root; `respect_ignore_files` chooses between `rg` and `rg --no-ignore`, both `--hidden`.
fn walker(
    root: &Path,
    skipped_names: &'static [&'static str],
    respect_ignore_files: bool,
    excluded: &[PathBuf],
) -> WalkBuilder {
    let excluded = excluded.to_vec();
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(respect_ignore_files)
        .hidden(false)
        .follow_links(false)
        .sort_by_file_name(OsStr::cmp)
        .filter_entry(move |entry| {
            let name = entry.file_name();
            !excluded.iter().any(|path| entry.path().starts_with(path))
                && !skipped_names.iter().any(|skipped| name == *skipped)
                && !is_sensitive_path(Path::new(name))
        });
    if respect_ignore_files {
        builder.add_custom_ignore_filename(RIPGREP_IGNORE_FILE);
    }
    builder
}

/// `rg --files --glob <pattern> --sort=modified --no-ignore --hidden` in the directory `base`,
/// with the found files shown below `root`.
pub fn glob(
    root: &Path,
    base: &Path,
    query: &GlobQuery,
    cancel: &CancellationToken,
) -> Result<GlobMatches, LocalFileError> {
    glob_excluding(root, base, query, cancel, &[])
}

pub(crate) fn glob_excluding(
    root: &Path,
    base: &Path,
    query: &GlobQuery,
    cancel: &CancellationToken,
    excluded: &[PathBuf],
) -> Result<GlobMatches, LocalFileError> {
    let mut walk = walker(base, GLOB_SKIPPED_NAMES, false, excluded);
    walk.overrides(override_matcher(base, [&query.pattern])?);
    if !std::fs::metadata(base)
        .map_err(|error| LocalFileError::io(base, error))?
        .is_dir()
    {
        return Err(LocalFileError::NotADirectory(base.into()));
    }
    let mut found = Vec::new();
    for entry in walk.build() {
        if cancel.is_cancelled() {
            return Err(LocalFileError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            found.push(FoundFile {
                path: below_root(root, entry.path()),
                modified,
            });
        }
    }
    Ok(oldest_first(found, query.limit))
}

/// `rg` with the query's flags on the file or directory `base`, with the files it shows below
/// `root`; the page the query's `offset` and `head_limit` select.
pub fn grep(
    root: &Path,
    base: &Path,
    query: &GrepQuery,
    cancel: &CancellationToken,
) -> Result<GrepMatches, LocalFileError> {
    grep_excluding(root, base, query, cancel, &[])
}

pub(crate) fn grep_excluding(
    root: &Path,
    base: &Path,
    query: &GrepQuery,
    cancel: &CancellationToken,
    excluded: &[PathBuf],
) -> Result<GrepMatches, LocalFileError> {
    let mut search = GrepSearch::new(query)?;
    let mut walk = walker(base, GREP_SKIPPED_NAMES, true, excluded);
    walk.overrides(override_matcher(base, &query.globs)?);
    if let Some(file_type) = &query.file_type {
        walk.types(file_types(file_type)?);
    }
    let shows_paths = std::fs::metadata(base)
        .map_err(|error| LocalFileError::io(base, error))?
        .is_dir();
    for entry in walk.build() {
        if cancel.is_cancelled() {
            return Err(LocalFileError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let Ok((file, metadata)) = open_file(entry.path()) else {
            continue;
        };
        let found = search
            .search(
                &below_root(root, entry.path()),
                file,
                metadata.len(),
                metadata.modified().ok(),
                entry.depth() == 0,
                shows_paths,
            )
            .map_err(|error| match error {
                FileSearchError::TooLarge(_) => LocalFileError::TooLarge(entry.path().into()),
                other => other.into(),
            })?;
        if found.is_break() {
            break;
        }
    }
    Ok(search.finish())
}
