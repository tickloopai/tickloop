//! Low-level primitives shared by files Tickloop writes itself: private permission bits, creating
//! and checking private directories, atomic replacement via a temporary file in the same directory,
//! opening paths component by component from the root without following symlinks, and millisecond
//! timestamps for written records.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Directory permissions that only the current user can access.
pub const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
/// File permissions that only the current user can read and write.
pub const PRIVATE_FILE_MODE: u32 = 0o600;
const MODE_BITS: u32 = 0o7777;
const GROUP_OTHER_PERMISSION_BITS: u32 = 0o077;
const TEMPORARY_FILE_SUFFIX: &str = "tmp";
const TEMPORARY_DIRECTORY_DESCRIPTION: &str = "temporary directory";

/// Recursively creates directories with private directory permissions (existing ones are reused;
/// permissions are subject to umask).
pub fn create_private_directory(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIRECTORY_MODE)
        .create(path)
}

pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Ensures `path` is a real directory (not a symlink) owned by the current user with no group/other
/// permission bits. `description` states what the directory is for and appears in the rejection
/// error.
pub fn verify_private_directory(path: &Path, description: &str) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    let uid = current_uid();
    let mode = metadata.mode() & MODE_BITS;
    let problem = if metadata.file_type().is_symlink() {
        "it is a symbolic link".to_owned()
    } else if !metadata.is_dir() {
        "it is not a directory".to_owned()
    } else if metadata.uid() != uid {
        format!("it is owned by uid {} instead of {uid}", metadata.uid())
    } else if mode & GROUP_OTHER_PERMISSION_BITS != 0 {
        format!("its mode {mode:04o} grants group or other access")
    } else {
        return Ok(());
    };
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "refusing to use {description} {}: {problem}",
            path.display()
        ),
    ))
}

/// A directory Tickloop writes itself. Under the user config directory it is created normally;
/// under the system temp directory, other users can pre-plant directories or symlinks in the shared
/// `/tmp`, so every level below the anchor must be a real directory owned by the current user with
/// no group/other permission bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateDirectory {
    /// The trusted base directory; `None` means the whole path is trusted.
    anchor: Option<PathBuf>,
    path: PathBuf,
}

impl PrivateDirectory {
    pub fn trusted(path: PathBuf) -> Self {
        Self { anchor: None, path }
    }

    /// A directory at `relative` below the trusted `anchor`; every layer below it is verified.
    pub fn anchored(anchor: PathBuf, relative: impl AsRef<Path>) -> Self {
        Self {
            path: anchor.join(relative),
            anchor: Some(anchor),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: impl AsRef<Path>) -> Self {
        Self {
            anchor: self.anchor.clone(),
            path: self.path.join(name),
        }
    }

    /// Creates the directory (reusing an existing one); on failure or a failed check, returns an
    /// error explaining why.
    pub fn create(&self) -> io::Result<()> {
        let Some(anchor) = &self.anchor else {
            return create_private_directory(&self.path);
        };
        let relative = self.path.strip_prefix(anchor).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is outside the temporary directory {}",
                    self.path.display(),
                    anchor.display()
                ),
            )
        })?;
        create_private_directory(anchor)?;
        let mut current = anchor.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a normalized path", self.path.display()),
                ));
            };
            current.push(name);
            match std::fs::DirBuilder::new()
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(&current)
            {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            verify_private_directory(&current, TEMPORARY_DIRECTORY_DESCRIPTION)?;
        }
        Ok(())
    }
}

/// Atomically replaces `path`: exclusively creates a temporary file with `mode` in the same
/// directory (without following symlinks), writes and syncs it to disk, then renames it over. At
/// any moment `path` holds either the complete old content or the complete new content; on failure
/// the temporary file is removed.
pub fn replace_file_atomically(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", path.display()),
        )
    })?;
    let temporary = path.with_file_name(format!(
        ".{}.{}.{}.{TEMPORARY_FILE_SUFFIX}",
        file_name.to_string_lossy(),
        std::process::id(),
        tickloop_ids::random_token()
    ));
    let replaced = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(contents)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, path));
    if replaced.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    replaced
}

/// What [`open_without_following_links`] requires of the last path component.
#[derive(Clone, Copy, Debug)]
pub enum OpenTarget {
    /// The last component may be any kind of directory entry (intermediate components must still be
    /// directories).
    Entry,
    /// Every component must be a directory. With `create_mode`, missing levels are created with
    /// that mode (subject to umask); otherwise a missing one returns `NotFound`.
    Directory { create_mode: Option<u32> },
}

/// Opens an absolute, normalized path component by component from the root with `O_NOFOLLOW`: each
/// component is opened relative to the previous directory's descriptor, so replacing a level with a
/// symlink mid-way cannot redirect the access elsewhere. Returns `ELOOP` when any component is a
/// symlink, and `ENOTDIR` when a component that should be a directory is not one. Opens with
/// `O_NONBLOCK`, so a FIFO as the last component does not block.
pub fn open_without_following_links(path: &Path, target: OpenTarget) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an absolute path", path.display()),
        ));
    }
    let names = path
        .components()
        .filter(|component| *component != Component::RootDir)
        .map(|component| match component {
            Component::Normal(name) => CString::new(name.as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL")),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a normalized path", path.display()),
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut handle = File::open("/")?;
    for (index, name) in names.iter().enumerate() {
        let is_last = index + 1 == names.len();
        let (must_be_directory, create_mode) = match target {
            OpenTarget::Entry => (!is_last, None),
            OpenTarget::Directory { create_mode } => (true, create_mode),
        };
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if must_be_directory {
                libc::O_DIRECTORY
            } else {
                0
            };
        let mut opened = open_at(&handle, name, flags);
        if let (Err(error), Some(mode)) = (&opened, create_mode)
            && error.kind() == io::ErrorKind::NotFound
        {
            // SAFETY: The directory descriptor is valid and the name is a single NUL-terminated
            // path component.
            if unsafe { libc::mkdirat(handle.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) }
                != 0
            {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            opened = open_at(&handle, name, flags);
        }
        handle = opened?;
    }
    Ok(handle)
}

fn open_at(directory: &File, name: &CString, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: The directory descriptor is valid and the name is a single NUL-terminated path
    // component.
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The descriptor returned by a successful openat is not held by anything else;
    // ownership passes to `File`.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

/// Milliseconds since the Unix epoch; 0 when the system clock is before the epoch.
pub fn unix_time_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symbolic_link_in_any_component_is_not_followed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/file.txt"), "content").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

        assert!(
            open_without_following_links(&root.join("real/file.txt"), OpenTarget::Entry).is_ok()
        );
        let through_link =
            open_without_following_links(&root.join("link/file.txt"), OpenTarget::Entry)
                .unwrap_err();
        assert!(
            matches!(
                through_link.raw_os_error(),
                Some(libc::ELOOP | libc::ENOTDIR)
            ),
            "{through_link}"
        );
        assert!(
            open_without_following_links(
                &root.join("real/file.txt"),
                OpenTarget::Directory { create_mode: None }
            )
            .is_err()
        );
        assert!(open_without_following_links(Path::new("relative"), OpenTarget::Entry).is_err());
        assert!(
            open_without_following_links(&root.join("real/../real"), OpenTarget::Entry).is_err()
        );
    }

    #[test]
    fn missing_directories_are_created_only_when_asked() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().canonicalize().unwrap().join("a/b");
        let missing =
            open_without_following_links(&nested, OpenTarget::Directory { create_mode: None })
                .unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        open_without_following_links(
            &nested,
            OpenTarget::Directory {
                create_mode: Some(PRIVATE_DIRECTORY_MODE),
            },
        )
        .unwrap();
        assert!(nested.is_dir());
    }

    #[test]
    fn private_temp_directories_are_created_owner_only_and_reused() {
        let anchor = tempfile::tempdir().unwrap();
        let directory = PrivateDirectory::anchored(
            anchor.path().to_path_buf(),
            "tickloop-1/project/chat/tasks",
        );
        directory.create().unwrap();
        directory.create().unwrap();
        for layer in [
            "tickloop-1",
            "tickloop-1/project",
            "tickloop-1/project/chat/tasks",
        ] {
            let metadata = std::fs::symlink_metadata(anchor.path().join(layer)).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.mode() & 0o777, 0o700, "{layer}");
        }
    }

    #[test]
    fn private_temp_directories_reject_symbolic_links_and_shared_layers() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(anchor.path().join("tickloop-1")).unwrap();
        std::fs::set_permissions(
            anchor.path().join("tickloop-1"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), anchor.path().join("tickloop-1/linked"))
            .unwrap();
        let error =
            PrivateDirectory::anchored(anchor.path().to_path_buf(), "tickloop-1/linked/chat")
                .create()
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("symbolic link"), "{error}");
        assert!(!elsewhere.path().join("chat").exists());

        let shared = anchor.path().join("tickloop-1/shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o750)).unwrap();
        let error =
            PrivateDirectory::anchored(anchor.path().to_path_buf(), "tickloop-1/shared/chat")
                .create()
                .unwrap_err();
        assert!(error.to_string().contains("mode 0750"), "{error}");
        assert!(!shared.join("chat").exists());

        std::fs::write(anchor.path().join("tickloop-1/file"), "").unwrap();
        let error = PrivateDirectory::anchored(anchor.path().to_path_buf(), "tickloop-1/file/chat")
            .create()
            .unwrap_err();
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[test]
    fn an_atomic_replacement_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        replace_file_atomically(&path, b"first", PRIVATE_FILE_MODE).unwrap();
        replace_file_atomically(&path, b"second", PRIVATE_FILE_MODE).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(
            std::fs::metadata(&path).unwrap().mode() & 0o777,
            PRIVATE_FILE_MODE
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
