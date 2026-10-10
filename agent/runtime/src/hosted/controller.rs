//! The Agent daemon's single controller of the Tickhost. The daemon starts the host on its first
//! open, keeps the controller key in its private runtime directory so a restarted daemon fences
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
use tickhost::{
    host::{OpenRequest, OpenedTick, OwnerSnapshotChunk, Receipt, TickAdapter, TickOwner},
    paths::HostPaths,
};
use tickhost_connect::{
    host_error::HostError,
    hosted::{BUILD_VERSION, CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT, HostConnection, Role},
};
use tickloop_agent_protocol::HostedViewGrant;
use tokio::sync::Mutex;
use uuid::Uuid;

/// The executable the daemon starts the Tickhost from: the one installed next to `tickloop`.
pub const TICKHOST_EXECUTABLE: &str = "tickhost";
const KEY_FILE: &str = "tickhost.key";
pub const START_ATTEMPTS: u32 = 3;
pub const START_WAIT: Duration = Duration::from_secs(3);
/// An open: the host waits up to the control timeout for the Tick to become ready, plus the
/// launch itself and its reply.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(2 * CONTROL_TIMEOUT.as_secs());
/// The longest the daemon may take for one worker lifecycle request: starting a host, then
/// opening.
pub const LIFECYCLE_TIMEOUT: Duration =
    Duration::from_secs(START_ATTEMPTS as u64 * START_WAIT.as_secs() + OPEN_TIMEOUT.as_secs());
const DISCONNECTED: &str = "Disconnected";
/// Chat ids per owner snapshot chunk, keeping each chunk well under one registry record
/// ([`SHARD_BYTES`](super::registry::SHARD_BYTES)).
const SNAPSHOT_CHUNK_OWNERS: usize = 16 * 1024;
const CONNECT_INTERVAL: Duration = Duration::from_millis(25);
/// Only an open starts a host; other requests find that no hosted Tick exists.
const HOST_NOT_RUNNING: &str = "HostNotRunning";

/// One Tick's approved Watch intents as the request that hands them to the host, never larger
/// than one registry record.
pub struct WatchIntents(Value);

impl WatchIntents {
    pub fn new(
        owner: &TickOwner,
        instance: &tick_protocol::InstanceRef,
        watches: &std::collections::BTreeSet<Uuid>,
    ) -> Result<Self, HostError> {
        let request = json!({"owner":owner,"instance":instance,"watches":watches});
        super::registry::within_shard(
            format_args!("the Watch intents of {instance}"),
            request.to_string().len(),
        )?;
        Ok(Self(request))
    }
}

/// What the host ran for an owner when a listing began.
pub struct OwnerListing {
    pub started_ms: u64,
    /// `None` when no host ran.
    pub running: Option<HostListing>,
}

/// The instances one host epoch runs for an owner.
pub struct HostListing {
    pub epoch: Uuid,
    pub live: std::collections::HashSet<tick_protocol::InstanceRef>,
}

struct ListedPage {
    listing: HostListing,
    next: Option<tickhost_connect::page::PageCursor>,
}

impl HostListing {
    /// One page of a host `list` reply, which names the host epoch that produced it.
    fn from_page(page: &Value) -> Option<ListedPage> {
        let epoch = serde_json::from_value(page.get("epoch")?.clone()).ok()?;
        let page: tickhost_connect::page::Page<tickhost::host::TickRow> =
            serde_json::from_value(page.clone()).ok()?;
        Some(ListedPage {
            listing: HostListing {
                epoch,
                live: page
                    .items
                    .into_iter()
                    .filter_map(|row| row.instance)
                    .collect(),
            },
            next: page.next,
        })
    }
}

#[derive(Clone, Copy)]
enum Start {
    OnDemand,
    Never,
}

pub struct HostController {
    paths: HostPaths,
    key_file: PathBuf,
    daemon_epoch: Uuid,
    executable: PathBuf,
    /// How long a host this daemon starts lets an unused Tick idle.
    idle_timeout: Duration,
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
        daemon_epoch: Uuid,
        executable: PathBuf,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            paths,
            key_file: agent_runtime.join(KEY_FILE),
            daemon_epoch,
            executable,
            idle_timeout,
            idle: Mutex::new(Vec::new()),
            starting: Mutex::new(()),
        }
    }

    pub fn endpoint(&self) -> PathBuf {
        self.paths.socket()
    }

    /// How long a host this daemon starts lets a Tick idle, and lets Watch intents go unrenewed.
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Opens `request` in the host. A protocol program starts behind the host's own launcher,
    /// `tickhost launch`, which connects it through the hosted launch contract; a terminal program
    /// starts as it is.
    pub async fn open(&self, mut request: OpenRequest) -> Result<OpenedTick, HostError> {
        if request.adapter == TickAdapter::Protocol {
            request.command = [
                self.executable.to_string_lossy().into_owned(),
                tickhost::cli::LAUNCH_COMMAND.into(),
                "--".into(),
            ]
            .into_iter()
            .chain(request.command)
            .collect();
        }
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
        instance: &tick_protocol::InstanceRef,
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

    /// The epoch of the running host, or none when no host runs.
    pub async fn host_epoch(&self) -> Result<Option<Uuid>, HostError> {
        match self
            .request("diagnostics", json!({}), CONTROL_TIMEOUT, Start::Never)
            .await
        {
            Ok(diagnostics) => serde_json::from_value(diagnostics["epoch"].clone())
                .map(Some)
                .map_err(|error| HostError::after("ProtocolViolation", error.to_string())),
            Err(error) if error.code == HOST_NOT_RUNNING => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Sends a complete owner snapshot of `root` in chunks. Without a running host there are no
    /// Ticks to reconcile.
    pub async fn reconcile_owners(
        &self,
        root: &str,
        revision: u64,
        owners: &[Uuid],
    ) -> Result<(), HostError> {
        let chunks = owners.chunks(SNAPSHOT_CHUNK_OWNERS).collect::<Vec<_>>();
        let count = chunks.len().max(1) as u32;
        for chunk in 0..count {
            let snapshot = OwnerSnapshotChunk {
                root: root.to_owned(),
                revision,
                chunk,
                chunks: count,
                owners: chunks
                    .get(chunk as usize)
                    .map_or_else(Vec::new, |owners| owners.to_vec()),
            };
            match self
                .request(
                    "reconcile_owners",
                    json!(snapshot),
                    CONTROL_TIMEOUT,
                    Start::Never,
                )
                .await
            {
                Ok(_) => {}
                Err(error) if error.code == HOST_NOT_RUNNING => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Replaces and renews the approved Watch intents of one of an owner's Ticks. Without a
    /// running host there is nothing to protect.
    pub async fn set_intents(&self, intents: &WatchIntents) -> Result<(), HostError> {
        match self
            .request(
                "set_intents",
                intents.0.clone(),
                CONTROL_TIMEOUT,
                Start::Never,
            )
            .await
        {
            Err(error) if error.code == HOST_NOT_RUNNING => Ok(()),
            result => result.map(|_| ()),
        }
    }

    /// Lets one view attach to `owner`'s Tick `instance` on the running host. The grant works
    /// once and names the host process it was made by.
    pub async fn allow_view(
        &self,
        owner: &TickOwner,
        instance: &tick_protocol::InstanceRef,
    ) -> Result<HostedViewGrant, HostError> {
        let granted = self
            .request(
                "allow_view",
                json!({"owner":owner,"instance":instance}),
                CONTROL_TIMEOUT,
                Start::Never,
            )
            .await?;
        let field = |name: &str| {
            serde_json::from_value(granted[name].clone())
                .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
        };
        Ok(HostedViewGrant {
            endpoint: self.endpoint(),
            host_epoch: field("epoch")?,
            instance: instance.clone(),
            view: field("view")?,
        })
    }

    /// What the host runs for `owner`, read across all pages of one host, and the moment the
    /// reading began: launch records written after it are newer than the listing.
    pub async fn owner_listing(&self, owner: &TickOwner) -> Result<OwnerListing, HostError> {
        let started_ms = tickloop_agent_base::private_fs::unix_time_milliseconds();
        Ok(OwnerListing {
            started_ms,
            running: self.live_instances(owner).await?,
        })
    }

    /// The running host's epoch and every instance it runs for `owner`, read across all pages
    /// of that one host; `None` when no host runs.
    async fn live_instances(&self, owner: &TickOwner) -> Result<Option<HostListing>, HostError> {
        let mut listing: Option<HostListing> = None;
        let mut cursor = None;
        loop {
            let page = match self
                .request(
                    "list",
                    json!({"owner":owner,"cursor":cursor}),
                    CONTROL_TIMEOUT,
                    Start::Never,
                )
                .await
            {
                Err(error) if error.code == HOST_NOT_RUNNING => return Ok(None),
                result => result?,
            };
            let page = HostListing::from_page(&page)
                .ok_or_else(|| HostError::before("ProtocolViolation", "malformed host listing"))?;
            let next = page.next.clone();
            match &mut listing {
                None => listing = Some(page.listing),
                Some(listing) if listing.epoch == page.listing.epoch => {
                    listing.live.extend(page.listing.live);
                }
                Some(_) => {
                    return Err(HostError::before(
                        "EpochChanged",
                        "another Tickhost started while its Ticks were listed",
                    ));
                }
            }
            match next {
                Some(next) => cursor = Some(next),
                None => return Ok(listing),
            }
        }
    }

    /// Whether host `epoch` exited only after every Tick it ran had ended.
    pub fn was_emptied(&self, epoch: Uuid) -> bool {
        self.paths.was_emptied(epoch)
    }

    /// Ends every Tick of `owner` and fences its later opens. Without a running host none runs.
    pub async fn end_owner(
        &self,
        owner: &TickOwner,
        revision: u64,
        operation: Uuid,
    ) -> Result<(), HostError> {
        match self
            .request(
                "end_owner",
                json!({"owner":owner,"revision":revision,"operation":operation}),
                CONTROL_TIMEOUT,
                Start::Never,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if error.code == HOST_NOT_RUNNING => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub async fn list(
        &self,
        owner: &TickOwner,
        cursor: Option<tickhost_connect::page::PageCursor>,
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
                let rows = Vec::<tickhost::host::TickRow>::new();
                tickhost_connect::page::paginate(
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

    /// Connects with this daemon's key, or starts a host. A live host this daemon cannot verify
    /// is refused, never replaced: a new host only binds after taking the host lock.
    async fn connect_or_start(&self) -> Result<HostConnection, HostError> {
        let _starting = self.starting.lock().await;
        // Only a missing key means no host of this daemon can exist; an unreadable key is never
        // replaced, or a live host's Ticks would lose their controller.
        let known = match std::fs::read_to_string(&self.key_file) {
            Ok(key) => Some(key.trim().to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(HostError::before(
                    "HostUnavailable",
                    format!("cannot read the Tickhost controller key: {error}"),
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
                let key = tickhost::host::random_token();
                write_private(&self.key_file, &key)
                    .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
                key
            }
        };
        self.check_executable()?;
        let mut last_failure = String::new();
        for _ in 0..START_ATTEMPTS {
            // A host that exits before it takes its configuration is one more failed start.
            if let Err(error) = self.spawn(&key) {
                last_failure = error.to_string();
                continue;
            }
            let deadline = tokio::time::Instant::now() + START_WAIT;
            while tokio::time::Instant::now() < deadline {
                match self.connect(&key).await {
                    Ok(connection) => return Ok(connection),
                    Err(error) if is_definite_refusal(&error) => return Err(error),
                    Err(error) => last_failure = error.to_string(),
                }
                tokio::time::sleep(CONNECT_INTERVAL).await;
            }
        }
        Err(HostError::before(
            "HostUnavailable",
            format!(
                "the Tickhost did not start in {START_ATTEMPTS} attempts of {}s each (last: {last_failure})",
                START_WAIT.as_secs()
            ),
        ))
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
                "no Tickhost is running",
            ));
        }
        let key = std::fs::read_to_string(&self.key_file).map_err(|_| {
            HostError::before(
                "Forbidden",
                "the running Tickhost was not started by this Agent daemon",
            )
        })?;
        self.connect(key.trim()).await
    }

    async fn connect(&self, key: &str) -> Result<HostConnection, HostError> {
        HostConnection::connect(
            &self.paths.socket(),
            Role::Controller {
                controller_key: key.to_owned(),
                daemon_epoch: self.daemon_epoch,
            },
            &[CAPABILITY_LIFECYCLE],
        )
        .await
    }

    /// Refuses to start a Tickhost of another build: its version must be this daemon's.
    fn check_executable(&self) -> Result<(), HostError> {
        let output = std::process::Command::new(&self.executable)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| {
                HostError::before(
                    "HostUnavailable",
                    format!(
                        "cannot run the Tickhost {}: {error}; install tickhost next to tickloop from the same build",
                        self.executable.display()
                    ),
                )
            })?;
        let reported = String::from_utf8_lossy(&output.stdout);
        let expected = format!("{TICKHOST_EXECUTABLE} {BUILD_VERSION}");
        if !output.status.success() || reported.trim() != expected {
            return Err(HostError::before(
                "IncompatibleHost",
                format!(
                    "the Tickhost {} reports {:?}, but this Agent needs {expected}; install tickloop and tickhost from the same build",
                    self.executable.display(),
                    reported.trim()
                ),
            ));
        }
        Ok(())
    }

    /// Starts the host as its own session: it outlives this daemon and is reaped here only
    /// while the daemon still runs.
    fn spawn(&self, key: &str) -> std::io::Result<()> {
        let mut command = std::process::Command::new(&self.executable);
        command
            .arg(tickhost::cli::RUN_COMMAND)
            .env(tickhost::paths::RUNTIME_DIR_ENV, &self.paths.runtime)
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
        let config = serde_json::to_string(&tickhost::host::HostConfig {
            controller_key: key.to_owned(),
            idle_timeout: self.idle_timeout,
        })?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(format!("{config}\n").as_bytes())?;
        drop(stdin);
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// A host that answered and refused this daemon is never retried or replaced.
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
