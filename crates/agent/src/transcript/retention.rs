//! Shared retention locks protect live Chat files from cleanup. They carry no execution identity.

use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use uuid::Uuid;

use crate::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, current_uid, verify_private_directory,
};

pub(crate) const RETENTION_DIRECTORY: &str = "chat-retention";

pub(crate) struct RetentionGuard {
    _file: File,
}

pub(crate) fn prepare_directory(root: &Path) -> io::Result<PathBuf> {
    let directory = root.join(RETENTION_DIRECTORY);
    create_private_directory(&directory)?;
    verify_private_directory(&directory, "Chat retention directory")?;
    Ok(directory)
}

impl RetentionGuard {
    pub(crate) fn shared(root: &Path, chat_id: Uuid) -> io::Result<Self> {
        Self::acquire(root, chat_id, libc::LOCK_SH)
    }

    pub(crate) fn exclusive(root: &Path, chat_id: Uuid) -> io::Result<Self> {
        Self::acquire(root, chat_id, libc::LOCK_EX)
    }

    fn acquire(root: &Path, chat_id: Uuid, operation: libc::c_int) -> io::Result<Self> {
        let path = prepare_directory(root)?.join(format!("{chat_id}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Chat retention lock is not a private regular file",
            ));
        }
        // SAFETY: the descriptor belongs to this open File for the duration of flock.
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let current = std::fs::symlink_metadata(&path)?;
        if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
            return Err(io::Error::other("Chat retention lock path changed"));
        }
        // Keep the lock file in place so independent open descriptions always share one inode.
        Ok(Self { _file: file })
    }
}
