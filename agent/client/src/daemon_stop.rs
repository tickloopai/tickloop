use std::{
    fs::{File, Metadata, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tokio::net::UnixStream;
use uuid::Uuid;

use crate::daemon_endpoint::{
    CONNECT_INTERVAL, LOCK_NAME, SOCKET_NAME, STARTUP_DEADLINE, check_socket_length, validate_file,
};
use crate::daemon_management as control;
use tickloop_agent_access::daemon_management::{CONTROL_MAJOR, Reply as DaemonReply, Request};

const STOP_DEADLINE: Duration = Duration::from_secs(10);
struct DaemonConnection {
    stream: UnixStream,
    epoch: Uuid,
}

struct DaemonFiles {
    endpoint: PathBuf,
    socket_identity: Option<(u64, u64)>,
    lock: Option<File>,
    lock_path: PathBuf,
}

pub struct PreparedDaemonStop {
    files: DaemonFiles,
    connection: DaemonConnection,
}

/// Binds a shutdown to one verified endpoint and daemon epoch, without stopping anything.
pub async fn prepare(
    directory: &Path,
    expected_socket: Option<(u64, u64)>,
) -> Result<Option<PreparedDaemonStop>, String> {
    let Some(files) = DaemonFiles::read(directory)? else {
        return Ok(None);
    };
    if expected_socket.is_some() && expected_socket != files.socket_identity {
        return Err("Agent daemon changed before confirmation; launch again to review it.".into());
    }
    let connection = tokio::time::timeout(STARTUP_DEADLINE, files.connect())
        .await
        .map_err(|_| "Agent daemon is not responding; it was left running.")??;
    let Some(connection) = connection else {
        if files.lock_is_held()? {
            return Err("Agent daemon is starting or stopping; retry daemon stop shortly.".into());
        }
        return Ok(None);
    };
    files.validate_original_socket()?;
    Ok(Some(PreparedDaemonStop { files, connection }))
}

impl PreparedDaemonStop {
    /// Consumes the confirmation target once; an unknown shutdown outcome is never retried.
    pub async fn stop(self) -> Result<(), String> {
        let Self {
            files,
            mut connection,
        } = self;
        files.validate_original_socket()?;
        tokio::time::timeout(STARTUP_DEADLINE, connection.shutdown())
            .await
            .map_err(|_| "Agent daemon stop was not confirmed; no stop request was resent.")??;
        drop(connection);
        let deadline = Instant::now() + STOP_DEADLINE;
        loop {
            if files.stopped()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("Agent daemon is still stopping; wait, then retry daemon stop.".into());
            }
            tokio::time::sleep(CONNECT_INTERVAL).await;
        }
    }
}

impl DaemonFiles {
    fn read(directory: &Path) -> Result<Option<Self>, String> {
        let metadata = match std::fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Cannot inspect Agent runtime directory: {error}")),
        };
        if !metadata.file_type().is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o700
        {
            return Err("Agent runtime directory must be a non-symlink directory owned by this user with mode 0700".into());
        }
        let endpoint = directory.join(SOCKET_NAME);
        check_socket_length(&endpoint)?;
        let socket_identity = match std::fs::symlink_metadata(&endpoint) {
            Ok(metadata) => {
                validate_socket_metadata(&metadata)?;
                Some((metadata.dev(), metadata.ino()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Cannot inspect Agent daemon socket: {error}")),
        };
        let lock = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join(LOCK_NAME))
        {
            Ok(lock) => {
                validate_file(&lock, "Agent daemon lock")?;
                Some(lock)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Cannot inspect Agent daemon lock: {error}")),
        };
        Ok(Some(Self {
            endpoint,
            socket_identity,
            lock,
            lock_path: directory.join(LOCK_NAME),
        }))
    }

    async fn connect(&self) -> Result<Option<DaemonConnection>, String> {
        if self.socket_identity.is_none() {
            return Ok(None);
        }
        self.validate_original_socket()?;
        let mut stream = match UnixStream::connect(&self.endpoint).await {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(format!("Cannot connect to Agent daemon: {error}")),
        };
        let credentials = stream
            .peer_cred()
            .map_err(|error| format!("Cannot verify Agent daemon identity: {error}"))?;
        if credentials.uid() != unsafe { libc::geteuid() } {
            return Err("Agent daemon must belong to the current user.".into());
        }
        control::write(
            &mut stream,
            &Request::Hello {
                major: CONTROL_MAJOR,
                minor: 0,
            },
        )
        .await?;
        match control::read(&mut stream).await? {
            Some(DaemonReply::Hello(hello)) if hello.major == CONTROL_MAJOR && hello.minor == 0 => {
                Ok(Some(DaemonConnection {
                    stream,
                    epoch: hello.daemon_epoch,
                }))
            }
            Some(DaemonReply::Error { error, .. }) if error.code != "VersionMismatch" => {
                Err(error.message)
            }
            _ => Err("This Agent daemon version cannot be stopped by this build. Use the build that started it.".into()),
        }
    }

    fn validate_original_socket(&self) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(&self.endpoint)
            .map_err(|error| format!("Cannot inspect Agent daemon socket: {error}"))?;
        self.validate_original_socket_metadata(&metadata)
    }

    fn validate_original_socket_metadata(&self, metadata: &Metadata) -> Result<(), String> {
        self.validate_original_lock()?;
        validate_socket_metadata(metadata)?;
        if Some((metadata.dev(), metadata.ino())) != self.socket_identity {
            return Err("Agent daemon changed while stopping; retry daemon stop.".into());
        }
        Ok(())
    }

    fn validate_original_lock(&self) -> Result<(), String> {
        let current = match std::fs::symlink_metadata(&self.lock_path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Cannot inspect Agent daemon lock: {error}")),
        };
        let original = self
            .lock
            .as_ref()
            .map(File::metadata)
            .transpose()
            .map_err(|error| format!("Cannot inspect Agent daemon lock: {error}"))?;
        match (original, current) {
            (None, None) => Ok(()),
            (Some(original), Some(current))
                if current.file_type().is_file()
                    && original.dev() == current.dev()
                    && original.ino() == current.ino()
                    && current.uid() == unsafe { libc::geteuid() }
                    && current.mode() & 0o777 == 0o600 =>
            {
                Ok(())
            }
            _ => Err("Agent daemon changed while stopping; daemon lock was replaced.".into()),
        }
    }

    fn lock_is_held(&self) -> Result<bool, String> {
        self.validate_original_lock()?;
        let Some(lock) = &self.lock else {
            return Ok(false);
        };
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) } != 0 {
                return Err(format!(
                    "Cannot release Agent daemon lock probe: {}",
                    io::Error::last_os_error()
                ));
            }
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(true)
        } else {
            Err(format!("Cannot inspect Agent daemon lock: {error}"))
        }
    }

    fn stopped(&self) -> Result<bool, String> {
        match std::fs::symlink_metadata(&self.endpoint) {
            Ok(metadata) => {
                self.validate_original_socket_metadata(&metadata)?;
                Ok(false)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(!self.lock_is_held()?),
            Err(error) => Err(format!("Cannot inspect Agent daemon socket: {error}")),
        }
    }
}

fn validate_socket_metadata(metadata: &Metadata) -> Result<(), String> {
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err("Agent endpoint must be a socket owned by this user with mode 0600".into());
    }
    Ok(())
}

impl DaemonConnection {
    async fn shutdown(&mut self) -> Result<(), String> {
        let request_id = tickloop_ids::generate();
        control::write(
            &mut self.stream,
            &Request::ShutdownDaemon {
                request_id,
                daemon_epoch: self.epoch,
            },
        )
        .await
        .map_err(|_| "Agent daemon stop was not confirmed; no stop request was resent.")?;
        match control::read(&mut self.stream).await {
            Ok(Some(DaemonReply::DaemonStopped {
                request_id: response_id,
            })) if response_id == request_id => Ok(()),
            Ok(Some(DaemonReply::Error {
                request_id: Some(response_id),
                error,
            })) if response_id == request_id => Err(error.message),
            _ => Err("Agent daemon stop was not confirmed; no stop request was resent.".into()),
        }
    }
}
