use std::{
    ffi::OsString,
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

use crate::service_management::{self as control, CONTROL_MAJORS, Reply as ServiceReply, Request};

use super::{
    CONNECT_INTERVAL, LOCK_NAME, SOCKET_NAME, STARTUP_DEADLINE, check_socket_length,
    runtime_directory, validate_file,
};

const STOP_DEADLINE: Duration = Duration::from_secs(10);
const SERVICE_USAGE: &str = "Usage: tickloop service stop";
const SERVICE_HELP: &str = "Usage: tickloop service stop\n\n\
Stop the local Agent service and its running Agents. Saved history is kept.\n\
Run tickloop again to start this build.\n\
  -h, --help  Show this help.\n\n\
TICKLOOP_AGENT_RUNTIME_DIR selects the local service.";

struct ServiceConnection {
    stream: UnixStream,
    epoch: Uuid,
}

struct ServiceFiles {
    endpoint: PathBuf,
    socket_identity: Option<(u64, u64)>,
    lock: Option<File>,
    lock_path: PathBuf,
}

pub(super) async fn run(arguments: &[OsString]) -> Result<(), String> {
    let Some(command) = arguments.first().and_then(|value| value.to_str()) else {
        return Err(SERVICE_USAGE.into());
    };
    if matches!(command, "--help" | "-h") && arguments.len() == 1 {
        println!("{SERVICE_HELP}");
        return Ok(());
    }
    if command != "stop" {
        return Err(format!("Unknown service command. {SERVICE_USAGE}"));
    }
    let mut help = false;
    for argument in &arguments[1..] {
        match argument.to_str() {
            Some("--help" | "-h") => help = true,
            _ => return Err(format!("Invalid service stop option. {SERVICE_USAGE}")),
        }
    }
    if help {
        println!("{SERVICE_HELP}");
        return Ok(());
    }
    stop(&runtime_directory()).await
}

async fn stop(directory: &Path) -> Result<(), String> {
    let Some(files) = ServiceFiles::read(directory)? else {
        println!("Agent service is not running.");
        return Ok(());
    };
    let connection = tokio::time::timeout(STARTUP_DEADLINE, files.connect())
        .await
        .map_err(|_| "Agent service is not responding; it was left running.")??;
    let Some(mut connection) = connection else {
        if files.lock_is_held()? {
            return Err(
                "Agent service is starting or stopping; retry service stop shortly.".into(),
            );
        }
        println!("Agent service is not running.");
        return Ok(());
    };
    files.validate_original_socket()?;
    println!("Stopping the Agent service and its running Agents. Saved history is kept.");
    tokio::time::timeout(STARTUP_DEADLINE, connection.shutdown())
        .await
        .map_err(|_| "Agent service stop was not confirmed; no stop request was resent.")??;
    drop(connection);
    let deadline = Instant::now() + STOP_DEADLINE;
    loop {
        if files.stopped()? {
            println!("Agent service stopped.");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("Agent service is still stopping; wait, then retry service stop.".into());
        }
        tokio::time::sleep(CONNECT_INTERVAL).await;
    }
}

impl ServiceFiles {
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
            Err(error) => return Err(format!("Cannot inspect Agent service socket: {error}")),
        };
        let lock = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join(LOCK_NAME))
        {
            Ok(lock) => {
                validate_file(&lock, "Agent owner lock")?;
                Some(lock)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Cannot inspect Agent owner lock: {error}")),
        };
        Ok(Some(Self {
            endpoint,
            socket_identity,
            lock,
            lock_path: directory.join(LOCK_NAME),
        }))
    }

    async fn connect(&self) -> Result<Option<ServiceConnection>, String> {
        if self.socket_identity.is_none() {
            return Ok(None);
        }
        for major in CONTROL_MAJORS {
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
                Err(error) => return Err(format!("Cannot connect to Agent service: {error}")),
            };
            let credentials = stream
                .peer_cred()
                .map_err(|error| format!("Cannot verify Agent service identity: {error}"))?;
            if credentials.uid() != unsafe { libc::geteuid() } {
                return Err("Agent service must belong to the current user.".into());
            }
            control::write(&mut stream, &Request::Hello { major, minor: 0 }).await?;
            match control::read(&mut stream).await? {
                Some(ServiceReply::Hello(hello)) if hello.major == major && hello.minor == 0 => {
                    return Ok(Some(ServiceConnection {
                        stream,
                        epoch: hello.owner_epoch,
                    }));
                }
                Some(ServiceReply::Error {
                    request_id: None,
                    error,
                }) if error.code == "VersionMismatch" => {}
                Some(ServiceReply::Error { error, .. }) => return Err(error.message),
                _ => return Err("This Agent service version cannot be stopped by this build. Use the build that started it.".into()),
            }
        }
        Err("This Agent service version cannot be stopped by this build. Use the build that started it.".into())
    }

    fn validate_original_socket(&self) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(&self.endpoint)
            .map_err(|error| format!("Cannot inspect Agent service socket: {error}"))?;
        self.validate_original_socket_metadata(&metadata)
    }

    fn validate_original_socket_metadata(&self, metadata: &Metadata) -> Result<(), String> {
        self.validate_original_lock()?;
        validate_socket_metadata(metadata)?;
        if Some((metadata.dev(), metadata.ino())) != self.socket_identity {
            return Err("Agent service changed while stopping; retry service stop.".into());
        }
        Ok(())
    }

    fn validate_original_lock(&self) -> Result<(), String> {
        let current = match std::fs::symlink_metadata(&self.lock_path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Cannot inspect Agent owner lock: {error}")),
        };
        let original = self
            .lock
            .as_ref()
            .map(File::metadata)
            .transpose()
            .map_err(|error| format!("Cannot inspect Agent owner lock: {error}"))?;
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
            _ => Err("Agent service changed while stopping; owner lock was replaced.".into()),
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
                    "Cannot release Agent owner lock probe: {}",
                    io::Error::last_os_error()
                ));
            }
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(true)
        } else {
            Err(format!("Cannot inspect Agent owner lock: {error}"))
        }
    }

    fn stopped(&self) -> Result<bool, String> {
        match std::fs::symlink_metadata(&self.endpoint) {
            Ok(metadata) => {
                self.validate_original_socket_metadata(&metadata)?;
                Ok(false)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(!self.lock_is_held()?),
            Err(error) => Err(format!("Cannot inspect Agent service socket: {error}")),
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

impl ServiceConnection {
    async fn shutdown(&mut self) -> Result<(), String> {
        let request_id = Uuid::new_v4();
        control::write(
            &mut self.stream,
            &Request::ShutdownOwner {
                request_id,
                owner_epoch: self.epoch,
            },
        )
        .await
        .map_err(|_| "Agent service stop was not confirmed; no stop request was resent.")?;
        match control::read(&mut self.stream).await {
            Ok(Some(ServiceReply::OwnerStopped {
                request_id: response_id,
            })) if response_id == request_id => Ok(()),
            Ok(Some(ServiceReply::Error {
                request_id: Some(response_id),
                error,
            })) if response_id == request_id => Err(error.message),
            _ => Err("Agent service stop was not confirmed; no stop request was resent.".into()),
        }
    }
}
