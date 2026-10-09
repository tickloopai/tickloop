//! Private runtime and state locations of the Tickhost. Directories are 0700 and sockets or
//! state files 0600, owned by this user and never reached through a symlink.

use std::{
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const RUNTIME_DIR_ENV: &str = "TICKLOOP_TICKHOST_RUNTIME_DIR";
pub const SOCKET_NAME: &str = "host.sock";
pub const LOCK_NAME: &str = "host.lock";
/// Holds one private directory per Tick, where its launcher binds the Tick Session socket.
pub const SESSION_DIRECTORY: &str = "s";
/// One empty file per host epoch that exited because every Tick had ended.
pub const EMPTIED_DIRECTORY: &str = "emptied";
/// macOS `sun_path` holds 104 bytes including the terminator.
pub const MAX_SOCKET_PATH_BYTES: usize = 103;
/// TickSession keeps paths at most this long so `<uuid>.sock` still fits.
pub const MAX_SESSION_DIRECTORY_BYTES: usize = 55;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPaths {
    pub runtime: PathBuf,
}

impl HostPaths {
    /// The runtime directory from the environment or the per-user default under /tmp.
    pub fn from_environment() -> Self {
        Self {
            runtime: std::env::var_os(RUNTIME_DIR_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(format!("/tmp/tickloop-tickhost-{}", current_uid()))
                }),
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join(SOCKET_NAME)
    }

    pub fn lock(&self) -> PathBuf {
        self.runtime.join(LOCK_NAME)
    }

    pub fn sessions(&self) -> PathBuf {
        self.runtime.join(SESSION_DIRECTORY)
    }

    /// The session directory of the host's `serial`th Tick, removed with that Tick, so even a
    /// launcher killed before it could clean up leaves no socket behind.
    pub fn tick_session_directory(&self, serial: u64) -> PathBuf {
        self.sessions().join(format!("{serial:x}"))
    }

    /// Removes whatever the Ticks of an earlier host left in the session directory; the host
    /// lock proves no host serves them any more.
    pub fn clear_sessions(&self) {
        let Ok(entries) = std::fs::read_dir(self.sessions()) else {
            return;
        };
        for entry in entries.flatten() {
            let _ = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                std::fs::remove_dir_all(entry.path())
            } else {
                std::fs::remove_file(entry.path())
            };
        }
    }

    fn emptied_marker(&self, epoch: uuid::Uuid) -> PathBuf {
        self.runtime.join(EMPTIED_DIRECTORY).join(epoch.to_string())
    }

    /// Records that host `epoch` exits because every Tick it ran had ended. A host that crashed
    /// or was shut down with Ticks leaves no mark, so its Ticks count as lost.
    pub fn record_emptied(&self, epoch: uuid::Uuid) -> io::Result<()> {
        private_directory(&self.runtime.join(EMPTIED_DIRECTORY))?;
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(self.emptied_marker(epoch))
            .map(drop)
    }

    /// Forgets marks older than `retention`; records of those hosts then count as lost, which
    /// only offers a reopen the user can decline.
    pub fn prune_emptied(&self, retention: std::time::Duration) {
        let Ok(entries) = std::fs::read_dir(self.runtime.join(EMPTIED_DIRECTORY)) else {
            return;
        };
        for entry in entries.flatten() {
            let is_old = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > retention));
            if is_old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Whether host `epoch` exited only after all its Ticks had ended.
    pub fn was_emptied(&self, epoch: uuid::Uuid) -> bool {
        std::fs::symlink_metadata(self.emptied_marker(epoch))
            .is_ok_and(|metadata| metadata.is_file())
    }

    /// Whether a host serves this runtime directory: a live host holds the host lock for its
    /// whole life, so an obtainable lock means none runs, whatever socket file remains.
    pub fn host_running(&self) -> io::Result<bool> {
        let lock = match std::fs::File::open(self.lock()) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(true)
        } else {
            Err(error)
        }
    }

    /// Creates or verifies the private runtime and session directories and checks that every
    /// socket path fits.
    pub fn prepare(&self) -> io::Result<()> {
        private_directory(&self.runtime)?;
        private_directory(&self.sessions())?;
        use std::os::unix::ffi::OsStrExt;
        if self.socket().as_os_str().as_bytes().len() > MAX_SOCKET_PATH_BYTES {
            return Err(io::Error::other(
                "Tickhost runtime directory is too long for a Unix socket path",
            ));
        }
        let longest_tick_session_directory = self.tick_session_directory(u64::MAX);
        if longest_tick_session_directory.as_os_str().as_bytes().len() > MAX_SESSION_DIRECTORY_BYTES
        {
            return Err(io::Error::other(
                "Tickhost session directory is too long for Tick Session sockets",
            ));
        }
        Ok(())
    }
}

pub fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

/// Creates the directory 0700 when missing; otherwise requires a real directory owned by this
/// user and tightens its mode.
pub fn private_directory(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a private directory of this user", path.display()),
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// The device and inode of an endpoint this process created, so cleanup never removes a
/// replacement that another process bound at the same path.
pub fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

pub fn remove_if_identity(path: &Path, identity: (u64, u64)) {
    if file_identity(path).is_ok_and(|current| current == identity) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_fits_only_while_its_longest_tick_session_directory_fits() {
        let directory = tempfile::Builder::new()
            .prefix("thp")
            .tempdir_in("/private/tmp")
            .unwrap();
        let longest_tick_suffix = format!("/{SESSION_DIRECTORY}/{:x}", u64::MAX);
        let fitting_bytes = MAX_SESSION_DIRECTORY_BYTES - longest_tick_suffix.len();
        let runtime_of = |bytes: usize| {
            let parent_bytes = directory.path().as_os_str().len() + "/".len();
            HostPaths {
                runtime: directory.path().join("r".repeat(bytes - parent_bytes)),
            }
        };
        assert!(runtime_of(fitting_bytes).prepare().is_ok());
        assert!(runtime_of(fitting_bytes + 1).prepare().is_err());
    }
}
