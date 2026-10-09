//! The client side of AgentAccess over a private Unix socket: the length-prefixed frames both
//! ends share, the client a Console or daemon uses to reach an Agent, and finding or starting the
//! local daemon.

use std::{
    fs::OpenOptions,
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Instant,
};

use tickloop_agent_protocol::{AccessError, AccessErrorCode, PROTOCOL_MAJOR, PROTOCOL_MINOR};

pub mod daemon_endpoint;
pub mod daemon_management;
pub mod daemon_stop;
pub mod frame;
mod incoming;
mod local;
mod recovering;

use daemon_endpoint::{
    CONNECT_INTERVAL, LOCK_NAME, SOCKET_NAME, STARTUP_DEADLINE, check_socket_length,
    remove_stale_socket, runtime_directory, validate_directory, validate_file, validate_socket,
};
pub use local::{LocalAgentClient, negotiate};
pub use recovering::{ReconnectFactory, RecoveringAgentClient, worker_link};

pub const DAEMON_COMMAND: &str = "daemon";
pub const RUN_SUBCOMMAND: &str = "run";
/// The hidden process entry of the daemon: it does not appear in `--help`, and names its role in
/// `ps`.
#[doc(hidden)]
pub const DAEMON_RUN_ARGUMENTS: [&str; 2] = [DAEMON_COMMAND, RUN_SUBCOMMAND];

#[derive(Debug)]
pub struct DaemonStartupError {
    message: String,
    pub mismatch: Option<DaemonMismatch>,
}

#[derive(Debug)]
pub struct DaemonMismatch {
    pub directory: PathBuf,
    pub description: String,
    socket_identity: (u64, u64),
}

impl DaemonMismatch {
    pub async fn prepare_stop(&self) -> Result<Option<daemon_stop::PreparedDaemonStop>, String> {
        daemon_stop::prepare(&self.directory, Some(self.socket_identity)).await
    }
}

impl std::fmt::Display for DaemonStartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for DaemonStartupError {}
impl From<String> for DaemonStartupError {
    fn from(message: String) -> Self {
        Self {
            message,
            mismatch: None,
        }
    }
}
impl From<&str> for DaemonStartupError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

/// Connects to the daemon of this user's runtime directory, starting it from `executable` when
/// none serves it.
pub async fn connect_or_start(executable: PathBuf) -> Result<LocalAgentClient, DaemonStartupError> {
    connect_or_start_at(runtime_directory(), executable).await
}

/// Reconnects to an already-running daemon without undoing an explicit daemon stop.
pub async fn connect_to_running_daemon(executable: PathBuf) -> Result<LocalAgentClient, String> {
    let directory = runtime_directory();
    validate_directory(&directory)?;
    let directory = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let endpoint = directory.join(SOCKET_NAME);
    check_socket_length(&endpoint)?;
    connect_existing(&endpoint, &executable, false)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Agent daemon is not running; launch tickloop to start it.".into())
}

#[doc(hidden)]
pub async fn connect_or_start_at(
    directory: PathBuf,
    executable: PathBuf,
) -> Result<LocalAgentClient, DaemonStartupError> {
    validate_directory(&directory)?;
    let directory = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let executable = executable
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let endpoint = directory.join(SOCKET_NAME);
    check_socket_length(&endpoint)?;
    if let Some(client) = connect_existing(&endpoint, &executable, false).await? {
        return Ok(client);
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join(LOCK_NAME))
        .map_err(|error| format!("cannot open Agent daemon lock: {error}"))?;
    validate_file(&lock, "Agent daemon lock")?;
    let deadline = Instant::now() + STARTUP_DEADLINE;
    let mut spawned_daemon = None;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        if let Some(client) = connect_existing(&endpoint, &executable, false).await? {
            return Ok(client);
        }
        remove_stale_socket(&endpoint)?;
        let lock_fd = lock.as_raw_fd();
        let mut command = std::process::Command::new(&executable);
        command
            .args(DAEMON_RUN_ARGUMENTS)
            .arg(&directory)
            .arg(lock_fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Only the winner's already-validated lock is inherited by the detached daemon.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(lock_fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        spawned_daemon = Some(
            command
                .spawn()
                .map_err(|error| format!("cannot start local Agent daemon: {error}"))?,
        );
    } else {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(format!("cannot arbitrate Agent daemon startup: {error}").into());
        }
    }
    drop(lock);
    loop {
        let own_startup = spawned_daemon
            .as_mut()
            .is_some_and(|process| matches!(process.try_wait(), Ok(None)));
        if let Some(client) = connect_existing(&endpoint, &executable, own_startup).await? {
            return Ok(client);
        }
        if Instant::now() >= deadline {
            return Err("local Agent daemon did not become ready within 5 seconds".into());
        }
        tokio::time::sleep(CONNECT_INTERVAL).await;
    }
}

async fn connect_existing(
    endpoint: &Path,
    executable: &Path,
    own_startup: bool,
) -> Result<Option<LocalAgentClient>, DaemonStartupError> {
    if !endpoint.exists() {
        return Ok(None);
    }
    validate_socket(endpoint)?;
    let metadata = std::fs::symlink_metadata(endpoint).map_err(|error| error.to_string())?;
    let socket_identity = (metadata.dev(), metadata.ino());
    match tokio::time::timeout(STARTUP_DEADLINE, LocalAgentClient::connect(endpoint)).await {
        Ok(Ok(client)) => Ok(Some(client)),
        Ok(Err(error)) if error.code == AccessErrorCode::VersionMismatch => {
            Err(DaemonStartupError {
                message: incompatible_daemon_guidance(endpoint, executable, &error, own_startup),
                mismatch: (!own_startup).then(|| DaemonMismatch {
                    directory: endpoint
                        .parent()
                        .expect("daemon runtime directory")
                        .to_owned(),
                    description: error.message.clone(),
                    socket_identity,
                }),
            })
        }
        Ok(Err(error)) => {
            if matches!(error.code, AccessErrorCode::Unavailable)
                && matches!(
                    std::os::unix::net::UnixStream::connect(endpoint)
                        .err()
                        .map(|e| e.kind()),
                    Some(io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound)
                )
            {
                Ok(None)
            } else {
                Err(error.to_string().into())
            }
        }
        Err(_) => Err("existing Agent daemon is not responding; it was left running".into()),
    }
}

fn incompatible_daemon_guidance(
    endpoint: &Path,
    executable: &Path,
    error: &AccessError,
    own_startup: bool,
) -> String {
    let runtime = endpoint
        .parent()
        .expect("daemon endpoint has a runtime directory");
    let title = if own_startup {
        "Agent executable changed during startup"
    } else {
        "Agent daemon version mismatch"
    };
    let details = format!(
        "{title} (client {}.{}).\nProtocol: {}\nRuntime: {}\nExecutable: {}",
        PROTOCOL_MAJOR,
        PROTOCOL_MINOR,
        error.message,
        runtime.display(),
        executable.display(),
    );
    let (Some(runtime), Some(executable)) = (runtime.to_str(), executable.to_str()) else {
        return format!(
            "{details}\n\nRun this build with daemon stop, then relaunch.\nThis stops running Agents. Saved history is kept."
        );
    };
    let default_directory = PathBuf::from(format!("/tmp/tickloop-daemon-{}", unsafe {
        libc::geteuid()
    }));
    let default_directory = default_directory
        .canonicalize()
        .unwrap_or(default_directory);
    let prefix = if Path::new(runtime) == default_directory {
        String::new()
    } else {
        format!(
            "TICKLOOP_DAEMON_RUNTIME_DIR={} ",
            shell_words::quote(runtime)
        )
    };
    let stop = format!("{prefix}{}", shell_words::quote(executable));
    let mut launch = prefix;
    if let Some(config) = std::env::var_os("TICKLOOP_CONFIG_DIR")
        && let Some(config) = config.to_str()
    {
        launch.push_str(&format!(
            "TICKLOOP_CONFIG_DIR={} ",
            shell_words::quote(config)
        ));
    }
    launch.push_str(&shell_words::quote(executable));
    let startup_note = if own_startup {
        "\nUse a pinned executable for CLI, daemon, and workers before relaunching."
    } else {
        ""
    };
    format!(
        "{details}{startup_note}\n\nStop the daemon and relaunch:\n  {stop} daemon stop\n  {launch}\n\nThis stops running Agents. Saved history is kept."
    )
}
