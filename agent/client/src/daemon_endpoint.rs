//! The daemon's private runtime directory: where its socket and lock live, and the checks every
//! process makes before it trusts them.

use std::{
    fs::File,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

/// How long a daemon or worker has to become reachable, and a peer to answer.
pub const STARTUP_DEADLINE: Duration = Duration::from_secs(5);
pub const CONNECT_INTERVAL: Duration = Duration::from_millis(25);
pub const SOCKET_NAME: &str = "daemon.sock";
pub const LOCK_NAME: &str = "daemon.lock";

pub fn runtime_directory() -> PathBuf {
    std::env::var_os("TICKLOOP_DAEMON_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/tickloop-daemon-{}", unsafe {
                libc::geteuid()
            }))
        })
}

pub fn validate_directory(directory: &Path) -> Result<(), String> {
    if !directory.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .map_err(|error| format!("cannot create private Agent runtime directory: {error}"))?;
    }
    let metadata = std::fs::symlink_metadata(directory).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err("Agent runtime directory must be a non-symlink directory owned by this user with mode 0700".into());
    }
    Ok(())
}

pub fn validate_file(file: &File, label: &str) -> Result<(), String> {
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(format!(
            "{label} must be a regular file owned by this user with mode 0600"
        ));
    }
    Ok(())
}

pub fn validate_socket(endpoint: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    let metadata = std::fs::symlink_metadata(endpoint).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err("Agent endpoint must be a socket owned by this user with mode 0600".into());
    }
    Ok(())
}

pub fn remove_stale_socket(endpoint: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(endpoint).is_ok() {
        validate_socket(endpoint)?;
        std::fs::remove_file(endpoint).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn check_socket_length(endpoint: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    if endpoint.as_os_str().as_bytes().len() >= 104 {
        return Err("Agent runtime directory is too long for a Unix socket path".into());
    }
    Ok(())
}
