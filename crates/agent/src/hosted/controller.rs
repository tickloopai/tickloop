//! The Agent owner's single controller of the Tick host. The owner starts the host on its first
//! open, keeps the controller key in its private runtime directory so a restarted owner fences
//! the previous one, and never replaces a host it cannot verify.

use std::{
    io::Write,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::{Value, json};
use tick_connect::{
    host_error::HostError,
    hosted::{CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT, HostConnection, Role},
};
use tick_host::{
    host::{OpenRequest, OpenedTick, Receipt, TickOwner},
    paths::HostPaths,
};
use tokio::sync::Mutex;
use uuid::Uuid;

/// The owner's internal mode that runs the host library in its own process.
pub const HOST_MODE: &str = "--tick-host";
const KEY_FILE: &str = "tick-host.key";
pub const START_ATTEMPTS: u32 = 3;
pub const START_WAIT: Duration = Duration::from_secs(3);
/// An open: the host waits up to the control timeout for the Tick to become ready, plus the
/// launch itself and its reply.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(2 * CONTROL_TIMEOUT.as_secs());
/// The longest the owner may take for one worker lifecycle request: starting a host, then
/// opening.
pub const LIFECYCLE_TIMEOUT: Duration =
    Duration::from_secs(START_ATTEMPTS as u64 * START_WAIT.as_secs() + OPEN_TIMEOUT.as_secs());
const DISCONNECTED: &str = "Disconnected";
const CONNECT_INTERVAL: Duration = Duration::from_millis(25);
/// Only an open starts a host; other requests find that no hosted Tick exists.
const HOST_NOT_RUNNING: &str = "HostNotRunning";

#[derive(Clone, Copy)]
enum Start {
    OnDemand,
    Never,
}

pub struct HostController {
    paths: HostPaths,
    key_file: PathBuf,
    owner_epoch: Uuid,
    executable: PathBuf,
    /// Connections between requests. Each request holds its own connection, so a slow open of
    /// one Tick never delays another worker's request.
    idle: Mutex<Vec<HostConnection>>,
    /// Serializes starts so concurrent first opens start one host.
    starting: Mutex<()>,
}

impl HostController {
    pub fn new(
        agent_runtime: &Path,
        paths: HostPaths,
        owner_epoch: Uuid,
        executable: PathBuf,
    ) -> Self {
        Self {
            paths,
            key_file: agent_runtime.join(KEY_FILE),
            owner_epoch,
            executable,
            idle: Mutex::new(Vec::new()),
            starting: Mutex::new(()),
        }
    }

    pub fn endpoint(&self) -> PathBuf {
        self.paths.socket()
    }

    pub async fn open(&self, request: OpenRequest) -> Result<OpenedTick, HostError> {
        let value = self
            .request("open", json!(request), OPEN_TIMEOUT, Start::OnDemand)
            .await?;
        serde_json::from_value(value)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }

    pub async fn close(
        &self,
        operation: Uuid,
        owner: &TickOwner,
        instance: &tick_connect::instance::InstanceRef,
    ) -> Result<Receipt, HostError> {
        let value = match self
            .request(
                "close",
                json!({"operation":operation,"owner":owner,"instance":instance}),
                CONTROL_TIMEOUT,
                Start::Never,
            )
            .await
        {
            Err(error) if error.code == HOST_NOT_RUNNING => {
                return Ok(Receipt::Closed {
                    instance: instance.clone(),
                });
            }
            result => result?,
        };
        serde_json::from_value(value)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }

    pub async fn receipt(
        &self,
        operation: Uuid,
        owner: &TickOwner,
    ) -> Result<Option<Receipt>, HostError> {
        // Receipts live in the host that made them; without a host the outcome is unknown.
        let value = match self
            .request(
                "receipt",
                json!({"operation":operation,"owner":owner}),
                CONTROL_TIMEOUT,
                Start::Never,
            )
            .await
        {
            Err(error) if error.code == HOST_NOT_RUNNING => return Ok(None),
            result => result?,
        };
        serde_json::from_value(value["receipt"].clone())
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }

    pub async fn list(
        &self,
        owner: &TickOwner,
        cursor: Option<tick_connect::page::PageCursor>,
    ) -> Result<Value, HostError> {
        match self
            .request(
                "list",
                json!({"owner":owner,"cursor":cursor}),
                CONTROL_TIMEOUT,
                Start::Never,
            )
            .await
        {
            Err(error) if error.code == HOST_NOT_RUNNING => {
                let rows = Vec::<tick_host::host::TickRow>::new();
                tick_connect::page::paginate(
                    rows,
                    |row| row.terminal.to_string(),
                    cursor.as_ref(),
                    None,
                )
                .map(|page| json!(page))
                .map_err(|error| HostError::before(error.code(), error.to_string()))
            }
            result => result,
        }
    }

    /// Sends one controller request. Only `Start::OnDemand` starts a host when none serves the
    /// endpoint. A request that fails after it was written is never retried here, and its
    /// connection, whose state is then unknown, is not reused.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        start: Start,
    ) -> Result<Value, HostError> {
        // A connection to a host that has since exited fails before anything is sent; that one
        // request is safely retried on a fresh connection, starting a new host if needed.
        for attempt in 0..2 {
            let idle = self.idle.lock().await.pop();
            let mut connection = match (idle, start) {
                (Some(connection), _) => connection,
                (None, Start::OnDemand) => self.connect_or_start().await?,
                (None, Start::Never) => self.connect_running().await?,
            };
            let result = connection.request(method, params.clone(), timeout).await;
            match &result {
                Err(error) if error.code == DISCONNECTED && !error.dispatched => {
                    // Nothing was sent, and every idle connection reached the same lost host.
                    self.idle.lock().await.clear();
                    if attempt == 1 {
                        return result;
                    }
                }
                Err(error) if error.dispatched => return result,
                _ => {
                    self.idle.lock().await.push(connection);
                    return result;
                }
            }
        }
        unreachable!("the second attempt always returns")
    }

    /// Connects with this owner's key, or starts a host. A live host this owner cannot verify
    /// is refused, never replaced: a new host only binds after taking the host lock.
    async fn connect_or_start(&self) -> Result<HostConnection, HostError> {
        let _starting = self.starting.lock().await;
        // Only a missing key means no host of this owner can exist; an unreadable key is never
        // replaced, or a live host's Ticks would lose their controller.
        let known = match std::fs::read_to_string(&self.key_file) {
            Ok(key) => Some(key.trim().to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(HostError::before(
                    "HostUnavailable",
                    format!("cannot read the Tick host controller key: {error}"),
                ));
            }
        };
        if let Some(key) = &known
            && self.paths.socket().exists()
        {
            match self.connect(key).await {
                Ok(connection) => return Ok(connection),
                Err(error) if is_definite_refusal(&error) => return Err(error),
                Err(_) => {}
            }
        }
        let key = match known {
            Some(key) => key,
            None => {
                let key = tick_host::host::random_token();
                write_private(&self.key_file, &key)
                    .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
                key
            }
        };
        let mut last = HostError::before("HostUnavailable", "the Tick host did not start");
        for _ in 0..START_ATTEMPTS {
            self.spawn(&key)
                .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
            let deadline = tokio::time::Instant::now() + START_WAIT;
            while tokio::time::Instant::now() < deadline {
                match self.connect(&key).await {
                    Ok(connection) => return Ok(connection),
                    Err(error) if is_definite_refusal(&error) => return Err(error),
                    Err(error) => last = error,
                }
                tokio::time::sleep(CONNECT_INTERVAL).await;
            }
        }
        Err(last)
    }

    async fn connect_running(&self) -> Result<HostConnection, HostError> {
        // Holding the start lock keeps this probe from racing a host that is taking its lock.
        let _starting = self.starting.lock().await;
        let running = self
            .paths
            .host_running()
            .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
        if !running {
            return Err(HostError::before(
                HOST_NOT_RUNNING,
                "no Tick host is running",
            ));
        }
        let key = std::fs::read_to_string(&self.key_file).map_err(|_| {
            HostError::before(
                "Forbidden",
                "the running Tick host was not started by this Agent owner",
            )
        })?;
        self.connect(key.trim()).await
    }

    async fn connect(&self, key: &str) -> Result<HostConnection, HostError> {
        HostConnection::connect(
            &self.paths.socket(),
            Role::Controller {
                controller_key: key.to_owned(),
                owner_epoch: self.owner_epoch,
            },
            &[CAPABILITY_LIFECYCLE],
        )
        .await
    }

    /// Starts the host as its own session: it outlives this owner and is reaped here only
    /// while the owner still runs.
    fn spawn(&self, key: &str) -> std::io::Result<()> {
        let mut command = std::process::Command::new(&self.executable);
        command
            .arg(HOST_MODE)
            .env(tick_host::paths::RUNTIME_DIR_ENV, &self.paths.runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(format!("{key}\n").as_bytes())?;
        drop(stdin);
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// A host that answered and refused this owner is never retried or replaced.
fn is_definite_refusal(error: &HostError) -> bool {
    matches!(error.code.as_str(), "IncompatibleHost" | "Forbidden")
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents.as_bytes())
}
