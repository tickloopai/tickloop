//! Private runtime and state locations of the Tick host. Directories are 0700 and sockets or
//! state files 0600, owned by this user and never reached through a symlink.

use std::{
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const RUNTIME_DIR_ENV: &str = "TICKLOOP_TICK_HOST_RUNTIME_DIR";
pub const SOCKET_NAME: &str = "host.sock";
pub const LOCK_NAME: &str = "host.lock";
pub const SESSION_DIRECTORY: &str = "s";
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
                    PathBuf::from(format!("/tmp/tickloop-tick-host-{}", current_uid()))
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
                "Tick host runtime directory is too long for a Unix socket path",
            ));
        }
        if self.sessions().as_os_str().as_bytes().len() > MAX_SESSION_DIRECTORY_BYTES {
            return Err(io::Error::other(
                "Tick host session directory is too long for Tick Session sockets",
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
