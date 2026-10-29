//! The Agent daemon's single controller of the Tickhost. The daemon starts the host on its first
//! open or Shell spawn and keeps the controller key in its private runtime directory so a
//! restarted daemon fences the previous one. A running host that refuses that key can never be
//! controlled again: the next request stops it, and an open or spawn starts one with the key.
//! Compatible hosts keep their instances across product upgrades; changed host content is
//! replaced once empty.
//! An Agent without a daemon that is given a host of its
//! own, as in tests, controls one served inside its process.

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
    shell::{SpawnBinding, SpawnGrant},
};
use tickhost_connect::{
    host_error::HostError,
    hosted::{
        BUSINESS_MAJOR, BUSINESS_MINOR, CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT, HelloReply,
        HostConnection, ManagementConnection, Role,
    },
};
use tickloop_harness::protocol::{HostedViewGrant, OtherTickhost, TickhostOccupancy};
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
/// The longest the daemon may take for one worker lifecycle request: stopping a host no daemon
/// can control, starting one, then opening.
pub const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(
    tickhost::stop::STOP_TIMEOUT.as_secs()
        + START_ATTEMPTS as u64 * START_WAIT.as_secs()
        + OPEN_TIMEOUT.as_secs(),
);
const DISCONNECTED: &str = "Disconnected";
/// Chat ids per owner snapshot chunk, keeping each chunk well under one registry record
/// ([`SHARD_BYTES`](super::registry::SHARD_BYTES)).
const SNAPSHOT_CHUNK_OWNERS: usize = 16 * 1024;
const CONNECT_INTERVAL: Duration = Duration::from_millis(25);
/// Only an open or a spawn starts a host; other requests find that no hosted Tick exists.
const HOST_NOT_RUNNING: &str = "HostNotRunning";
const INCOMPATIBLE_HOST: &str = "IncompatibleHost";
/// How a host refuses a hello that does not carry its controller key.
const FORBIDDEN: &str = "Forbidden";
/// A host of another build that ran nothing was stopped, so one of this build may start.
const HOST_REPLACED: &str = "HostReplaced";
/// What a request meets while a Tickhost of another build runs Ticks, Shell tasks or connections.
pub const OTHER_TICKHOST_BUILD: &str = "The running Tickhost uses an incompatible protocol; its existing work is preserved and activation is deferred. `tickhost stop` explicitly ends its Ticks and Shells";
/// How a host older than the build field appears in a mismatch.
const UNNAMED_TICKHOST_BUILD: &str = "unknown (an older Tickhost)";
/// Stopping a host ends each of its Ticks after their terminate grace, side by side.
const HOST_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a spawn is asked for of a host that is exiting, which refuses it or closes the
/// connection unanswered; each retry finds it gone and starts a new one. A grant whose answer
/// was lost is never claimed and expires, so asking again is safe.
const SPAWN_ATTEMPTS: u32 = 3;
const SPAWN_RETRY_PAUSE: Duration = Duration::from_millis(100);

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

/// How a controller starts its host.
enum HostProgram {
    /// The `tickhost` installed next to `tickloop`, started as its own session.
    Executable(PathBuf),
    /// The host library, served inside this process; it runs Shell tasks only, since a protocol
    /// Tick's launcher is the Tickhost executable.
    InProcess(std::sync::Mutex<Option<tickhost::in_process::InProcessHost>>),
}

pub struct HostController {
    paths: HostPaths,
    key_file: PathBuf,
    daemon_epoch: Uuid,
    program: HostProgram,
    /// How long a host this daemon starts lets an unused Tick idle.
    idle_timeout: Duration,
    /// Connections between requests. Each request holds its own connection, so a slow open of
    /// one Tick never delays another worker's request.
    idle: Mutex<Vec<HostConnection>>,
    /// Serializes starts so concurrent first opens start one host.
    starting: Mutex<()>,
    /// A connection held for as long as its host runs: the host stops every Shell task once the
    /// daemon's last connection closed, and pooled connections come and go.
    lifeline: Mutex<Option<HostConnection>>,
    /// The host epoch whose management hello named this daemon's build.
    verified_epoch: std::sync::Mutex<Option<Uuid>>,
}

impl HostController {
    pub fn new(
        agent_runtime: &Path,
        paths: HostPaths,
        daemon_epoch: Uuid,
        executable: PathBuf,
        idle_timeout: Duration,
    ) -> Self {
        Self::with_program(
            agent_runtime,
            paths,
            daemon_epoch,
            HostProgram::Executable(executable),
            idle_timeout,
        )
    }

    /// The controller of a host served inside this process, for an Agent without a daemon that is
    /// given a host of its own.
    pub fn in_process(
        agent_runtime: &Path,
        paths: HostPaths,
        daemon_epoch: Uuid,
        idle_timeout: Duration,
    ) -> Self {
        Self::with_program(
            agent_runtime,
            paths,
            daemon_epoch,
            HostProgram::InProcess(std::sync::Mutex::new(None)),
            idle_timeout,
        )
    }

    fn with_program(
        agent_runtime: &Path,
        paths: HostPaths,
        daemon_epoch: Uuid,
        program: HostProgram,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            paths,
            key_file: agent_runtime.join(KEY_FILE),
            daemon_epoch,
            program,
            idle_timeout,
            idle: Mutex::new(Vec::new()),
            starting: Mutex::new(()),
            lifeline: Mutex::new(None),
            verified_epoch: std::sync::Mutex::new(None),
        }
    }

    pub(super) fn stop_in_process(&self) {
        if let HostProgram::InProcess(server) = &self.program {
            let server = server.lock().unwrap().take();
            drop(server);
        }
    }

    pub fn daemon_epoch(&self) -> Uuid {
        self.daemon_epoch
    }

    /// Keeps the existing daemon generation connected while its successor takes ownership.
    /// The successor constructs this controller with the preserved epoch before calling here.
    pub async fn prepare_daemon_handoff(&self) -> Result<(), HostError> {
        let connection = match self.connect_running().await {
            Ok(connection) => connection,
            Err(error) if error.code == HOST_NOT_RUNNING => return Ok(()),
            Err(error) => return Err(error),
        };
        *self.lifeline.lock().await = Some(connection);
        Ok(())
    }

    /// Releases this daemon's connections after its successor connects and old callers quiesce.
    pub async fn relinquish_daemon(&self) {
        let _starting = self.starting.lock().await;
        self.idle.lock().await.clear();
        self.lifeline.lock().await.take();
    }

    /// Retires changed, empty content without starting a host or adding pooled connections.
    pub async fn maintain_upgrade(&self) -> Result<(), HostError> {
        if self.verified_epoch.lock().unwrap().is_some() {
            return Ok(());
        }
        let Ok(_starting) = self.starting.try_lock() else {
            return Ok(());
        };
        let Some(management) = self.management().await? else {
            return Ok(());
        };
        let epoch = management.hello().epoch;
        if is_this_build(management.hello()) {
            *self.verified_epoch.lock().unwrap() = Some(epoch);
            return Ok(());
        }
        let own_connections = self
            .idle
            .lock()
            .await
            .iter()
            .filter(|connection| connection.epoch() == epoch)
            .count()
            + self.lifeline.try_lock().map_or(0, |lifeline| {
                usize::from(
                    lifeline
                        .as_ref()
                        .is_some_and(|connection| connection.epoch() == epoch),
                )
            });
        if runs_nothing(management.hello(), own_connections)
            && management
                .shutdown_idle_host(own_connections, HOST_SHUTDOWN_TIMEOUT)
                .await?
        {
            self.wait_stopped().await?;
            self.idle
                .lock()
                .await
                .retain(|connection| connection.epoch() != epoch);
            let mut lifeline = self.lifeline.lock().await;
            if lifeline
                .as_ref()
                .is_some_and(|connection| connection.epoch() == epoch)
            {
                lifeline.take();
            }
        }
        Ok(())
    }

    pub fn endpoint(&self) -> PathBuf {
        self.paths.socket()
    }

    /// How long a host this daemon starts keeps a released Shell task nobody views.
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Opens `request` in the host. A protocol program starts behind the host's own launcher,
    /// `tickhost launch`, which connects it through the hosted launch contract; a terminal program
    /// starts as it is.
    pub async fn open(&self, mut request: OpenRequest) -> Result<OpenedTick, HostError> {
        let HostProgram::Executable(executable) = &self.program else {
            return Err(HostError::before(
                "Unsupported",
                "hosted Ticks run in the Tickhost the Agent daemon starts",
            ));
        };
        if request.adapter == TickAdapter::Protocol {
            request.command = [
                executable.to_string_lossy().into_owned(),
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

    /// Grants one spawn of what `binding` names, starting a host when none runs, and holds a
    /// connection to that host for as long as it runs.
    pub async fn allow_spawn(&self, binding: &SpawnBinding) -> Result<SpawnGrant, HostError> {
        let mut attempt = 1;
        let granted = loop {
            match self
                .request(
                    "allow_spawn",
                    json!(binding),
                    CONTROL_TIMEOUT,
                    Start::OnDemand,
                )
                .await
            {
                Err(error)
                    if matches!(error.code.as_str(), "ShuttingDown" | "OutcomeUnknown")
                        && attempt < SPAWN_ATTEMPTS =>
                {
                    attempt += 1;
                    tokio::time::sleep(SPAWN_RETRY_PAUSE).await;
                }
                result => break result?,
            }
        };
        let grant: SpawnGrant = serde_json::from_value(granted)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))?;
        self.hold_shell_host(grant.epoch).await?;
        Ok(grant)
    }

    pub async fn sandbox_proxy_port(&self) -> Result<u16, HostError> {
        #[derive(serde::Deserialize)]
        struct Endpoint {
            port: u16,
            epoch: Uuid,
        }
        let endpoint: Endpoint = serde_json::from_value(
            self.request(
                "sandbox_proxy_port",
                json!({}),
                CONTROL_TIMEOUT,
                Start::OnDemand,
            )
            .await?,
        )
        .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))?;
        if endpoint.port == 0 {
            return Err(HostError::after(
                "ProtocolViolation",
                "invalid sandbox proxy port",
            ));
        }
        self.hold_shell_host(endpoint.epoch).await?;
        Ok(endpoint.port)
    }

    async fn hold_shell_host(&self, epoch: Uuid) -> Result<(), HostError> {
        let mut lifeline = self.lifeline.lock().await;
        if lifeline
            .as_ref()
            .is_none_or(|connection| connection.epoch() != epoch)
        {
            let key = std::fs::read_to_string(&self.key_file)
                .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
            let connection = self.connect(key.trim()).await?;
            if connection.epoch() != epoch {
                return Err(HostError::before(
                    "EpochChanged",
                    "another Tickhost started while preparing a Shell",
                ));
            }
            *lifeline = Some(connection);
        }
        Ok(())
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
        self.view_grant(instance, &granted)
    }

    /// Lets one view attach to the Shell task `instance` that `identity` submitted, running or
    /// ended. The grant works once and names the host process it was made by.
    pub async fn allow_shell_view(
        &self,
        identity: &tickhost::shell::ShellIdentity,
        instance: &tick_protocol::InstanceRef,
    ) -> Result<HostedViewGrant, HostError> {
        let granted = self
            .request(
                "allow_shell_view",
                json!({"identity":identity,"instance":instance}),
                CONTROL_TIMEOUT,
                Start::Never,
            )
            .await?;
        self.view_grant(instance, &granted)
    }

    fn view_grant(
        &self,
        instance: &tick_protocol::InstanceRef,
        granted: &Value,
    ) -> Result<HostedViewGrant, HostError> {
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
        let started_ms = tickloop_harness::base::private_fs::unix_time_milliseconds();
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
            let idle = self
                .idle
                .lock()
                .await
                .pop()
                .filter(|connection| self.is_verified(connection.epoch()));
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

    /// Connects with this daemon's key, or starts a host. A new host only binds after taking the
    /// host lock, so a running host no daemon can control is stopped first.
    async fn connect_or_start(&self) -> Result<HostConnection, HostError> {
        let _starting = self.starting.lock().await;
        let known = self.controller_key()?;
        if self.host_running()? {
            match self.connect_to_running(known.as_deref()).await {
                Ok(connection) => return Ok(connection),
                Err(error) if error.code == HOST_REPLACED => {}
                Err(error) if error.code == FORBIDDEN => self.stop_uncontrollable_host().await?,
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
                    Err(error) if error.code == HOST_REPLACED => {
                        return Err(HostError::before(
                            INCOMPATIBLE_HOST,
                            "the Tickhost installed next to tickloop is another build; install tickloop and tickhost from the same build",
                        ));
                    }
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
        if !self.host_running()? {
            return Err(HostError::before(
                HOST_NOT_RUNNING,
                "no Tickhost is running",
            ));
        }
        let key = self.controller_key()?;
        match self.connect_to_running(key.as_deref()).await {
            Err(error) if error.code == HOST_REPLACED => Err(HostError::before(
                HOST_NOT_RUNNING,
                "no Tickhost of this build is running",
            )),
            Err(error) if error.code == FORBIDDEN => {
                self.stop_uncontrollable_host().await?;
                Err(uncontrollable_host_stopped())
            }
            result => result,
        }
    }

    /// This daemon's controller key; `None` when the key file is missing. Only a missing key
    /// means no host of this daemon can exist; an unreadable key is never replaced, or a live
    /// host's Ticks would lose their controller.
    fn controller_key(&self) -> Result<Option<String>, HostError> {
        match std::fs::read_to_string(&self.key_file) {
            Ok(key) => Ok(Some(key.trim().to_owned())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(HostError::before(
                "HostUnavailable",
                format!("cannot read the Tickhost controller key: {error}"),
            )),
        }
    }

    /// Connects to the running host with this daemon's `key`. `Forbidden` means no Agent daemon
    /// can control that host again: it refused the key, the key is missing, or its endpoint is
    /// gone.
    async fn connect_to_running(&self, key: Option<&str>) -> Result<HostConnection, HostError> {
        let Some(key) = key else {
            return Err(uncontrollable("the Tickhost controller key is missing"));
        };
        if !self.endpoint_present().await {
            return Err(uncontrollable("the running Tickhost has no endpoint"));
        }
        self.connect(key).await
    }

    /// Whether the running host's endpoint exists. A host takes its lock just before it binds,
    /// so only an endpoint missing for longer than a start may take is gone for good.
    async fn endpoint_present(&self) -> bool {
        let deadline = tokio::time::Instant::now() + START_WAIT;
        loop {
            if std::fs::symlink_metadata(self.paths.socket()).is_ok() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(CONNECT_INTERVAL).await;
        }
    }

    fn host_running(&self) -> Result<bool, HostError> {
        self.paths
            .host_running()
            .map_err(|error| HostError::before("HostUnavailable", error.to_string()))
    }

    /// Only one daemon serves a config root and only it keeps the controller key, so a running
    /// host that refuses this daemon's key, runs while the key is missing or lost its endpoint can
    /// never be controlled by any daemon again, and its Ticks are already unreachable. It is
    /// stopped as `tickhost stop` stops a host, which signals only a verified `tickhost run`
    /// process; a host this process serves itself is ended directly. Only a request that may
    /// start a host starts the next one.
    async fn stop_uncontrollable_host(&self) -> Result<(), HostError> {
        self.stop_in_process();
        tickhost::stop::stop_host(&self.paths)
            .await
            .map_err(|error| {
                HostError::before(
                    "HostUnavailable",
                    format!("cannot stop a Tickhost no Agent daemon controls: {error}"),
                )
            })?;
        self.idle.lock().await.clear();
        Ok(())
    }

    /// Negotiates protocol compatibility before choosing whether changed content can be replaced.
    async fn connect(&self, key: &str) -> Result<HostConnection, HostError> {
        let connection = HostConnection::connect(
            &self.paths.socket(),
            Role::Controller {
                controller_key: key.to_owned(),
                daemon_epoch: self.daemon_epoch,
            },
            &[CAPABILITY_LIFECYCLE],
        )
        .await;
        match &connection {
            Ok(opened) if self.is_verified(opened.epoch()) => return connection,
            Ok(_) => {}
            Err(error) if error.code == INCOMPATIBLE_HOST => {}
            Err(_) => return connection,
        }
        let management = match ManagementConnection::connect(&self.paths.socket(), key).await {
            Ok(management) => management,
            // A host that negotiated the business protocol but cannot be asked its build is used
            // as it is, without being remembered as verified.
            Err(_) if connection.is_ok() => return connection,
            // A host of another business protocol checks the key only on its frozen management
            // channel.
            Err(error) if error.code == FORBIDDEN => return Err(error),
            Err(_) => return Err(other_build()),
        };
        if is_this_build(management.hello()) {
            if connection.is_ok() {
                *self.verified_epoch.lock().unwrap() = Some(management.hello().epoch);
            }
            return connection;
        }
        // The host counts the business connection this daemon just made among its own.
        let own_connections = usize::from(connection.is_ok())
            + self.idle.lock().await.len()
            + self
                .lifeline
                .try_lock()
                .map_or(0, |lifeline| usize::from(lifeline.is_some()));
        if !runs_nothing(management.hello(), own_connections) {
            return connection.map_err(|_| other_build());
        }
        if !management
            .shutdown_idle_host(own_connections, HOST_SHUTDOWN_TIMEOUT)
            .await?
        {
            return connection.map_err(|_| other_build());
        }
        drop(connection);
        self.wait_stopped().await?;
        self.idle.lock().await.clear();
        Err(HostError::before(
            HOST_REPLACED,
            "stopped a Tickhost of another build that ran nothing",
        ))
    }

    fn is_verified(&self, epoch: Uuid) -> bool {
        *self.verified_epoch.lock().unwrap() == Some(epoch)
    }

    /// The management channel of the running host, from this daemon's key; `None` when no host
    /// runs, after stopping one no daemon can control. Called with the start lock held.
    async fn management(&self) -> Result<Option<ManagementConnection>, HostError> {
        let key = self.controller_key()?;
        if !self.host_running()? {
            return Ok(None);
        }
        let management = match key {
            Some(key) if self.endpoint_present().await => {
                ManagementConnection::connect(&self.paths.socket(), &key).await
            }
            _ => Err(uncontrollable(
                "no Agent daemon can reach the running Tickhost",
            )),
        };
        match management {
            Ok(management) => Ok(Some(management)),
            Err(error) if error.code == FORBIDDEN => {
                self.stop_uncontrollable_host().await?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// The running Tickhost when it is another build than this daemon's and replacing it would
    /// end Ticks, Shell tasks or connections, or it cannot say; one that runs nothing is replaced
    /// at once.
    pub async fn other_tickhost(&self) -> Result<Option<OtherTickhost>, HostError> {
        match self.connect_running().await {
            Ok(connection) => {
                self.idle.lock().await.push(connection);
                return Ok(None);
            }
            Err(error) if error.code == HOST_NOT_RUNNING => return Ok(None),
            Err(error) if error.code == INCOMPATIBLE_HOST => {}
            Err(error) => return Err(error),
        }
        let _starting = self.starting.lock().await;
        let Some(management) = self.management().await? else {
            return Ok(None);
        };
        let hello = management.hello().clone();
        if is_this_build(&hello) {
            *self.verified_epoch.lock().unwrap() = Some(hello.epoch);
            return Ok(None);
        }
        Ok(Some(OtherTickhost {
            description: format!(
                "Running Tickhost build {}; this build {}",
                hello
                    .host
                    .as_ref()
                    .map_or(UNNAMED_TICKHOST_BUILD, |host| host.build.as_str()),
                tickhost::host::BUILD_IDENTITY
            ),
            // A host that cannot say what it is starting cannot say all it runs.
            occupancy: hello.host.and_then(|host| {
                Some(TickhostOccupancy {
                    pending: host.occupancy.pending?,
                    ticks: host.occupancy.ticks,
                    shells: host.occupancy.shells,
                    views: host.occupancy.views,
                })
            }),
        }))
    }

    /// Stops the running Tickhost of another build on the user's word, ending what it runs; the
    /// next open or Shell spawn starts one of this build.
    pub async fn replace_other_tickhost(&self) -> Result<(), HostError> {
        let _starting = self.starting.lock().await;
        let Some(management) = self.management().await? else {
            return Ok(());
        };
        if is_this_build(management.hello()) {
            return Ok(());
        }
        self.stop(management).await?;
        self.idle.lock().await.clear();
        Ok(())
    }

    /// Stops the host `management` reached and waits until it gave up its lock, so a host of
    /// this build can take it at once.
    async fn stop(&self, management: ManagementConnection) -> Result<(), HostError> {
        management.shutdown_host(HOST_SHUTDOWN_TIMEOUT).await?;
        self.wait_stopped().await
    }

    async fn wait_stopped(&self) -> Result<(), HostError> {
        let deadline = tokio::time::Instant::now() + START_WAIT;
        while self.host_running()? {
            if tokio::time::Instant::now() >= deadline {
                return Err(HostError::after(
                    "OutcomeUnknown",
                    "the stopped Tickhost still holds its lock",
                ));
            }
            tokio::time::sleep(CONNECT_INTERVAL).await;
        }
        Ok(())
    }

    /// Only the selected executable's component content may start a new host.
    fn check_executable(&self) -> Result<(), HostError> {
        let HostProgram::Executable(executable) = &self.program else {
            return Ok(());
        };
        let output = std::process::Command::new(executable)
            .arg("--content-identity")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| {
                HostError::before(
                    "HostUnavailable",
                    format!(
                        "cannot run the Tickhost {}: {error}; install tickhost next to tickloop from the same build",
                        executable.display()
                    ),
                )
            })?;
        let reported = String::from_utf8_lossy(&output.stdout);
        let expected = tickhost::host::BUILD_IDENTITY;
        if !output.status.success() || reported.trim() != expected {
            return Err(HostError::before(
                "IncompatibleHost",
                format!(
                    "the Tickhost {} reports {:?}, but this Agent needs {expected}; install tickloop and tickhost from the same build",
                    executable.display(),
                    reported.trim()
                ),
            ));
        }
        Ok(())
    }

    /// Starts the host as its own session: it outlives this daemon and is reaped here only
    /// while the daemon still runs. A host served in this process ends with it.
    fn spawn(&self, key: &str) -> std::io::Result<()> {
        let config = tickhost::host::HostConfig {
            controller_key: key.to_owned(),
            idle_timeout: self.idle_timeout,
        };
        let executable = match &self.program {
            HostProgram::Executable(executable) => executable,
            HostProgram::InProcess(server) => {
                // What would keep the host from serving fails this start at once.
                self.paths.prepare()?;
                *server.lock().unwrap() = Some(tickhost::in_process::InProcessHost::start(
                    self.paths.clone(),
                    config,
                )?);
                return Ok(());
            }
        };
        let mut command = std::process::Command::new(executable);
        command
            .arg(tickhost::cli::RUN_COMMAND)
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
        let config = serde_json::to_string(&config)?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(format!("{config}\n").as_bytes())?;
        drop(stdin);
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// A host that answered and refused this daemon is not retried.
fn is_definite_refusal(error: &HostError) -> bool {
    matches!(error.code.as_str(), INCOMPATIBLE_HOST | FORBIDDEN)
}

/// Product versions do not change the identity of unchanged Tickhost content.
fn is_this_build(hello: &HelloReply) -> bool {
    hello.host.as_ref().is_some_and(|host| {
        host.build == tickhost::host::BUILD_IDENTITY
            && host.business_major == BUSINESS_MAJOR
            && host.business_minor == BUSINESS_MINOR
    })
}

/// Whether stopping the host ends nothing beyond `own_connections` of this daemon's; a host too
/// old to say is taken to run something.
fn runs_nothing(hello: &HelloReply, own_connections: usize) -> bool {
    hello.host.as_ref().is_some_and(|host| {
        let occupancy = &host.occupancy;
        occupancy.ticks.is_empty()
            && occupancy.shells == 0
            && occupancy.pending == Some(0)
            && occupancy.views == Some(0)
            && occupancy.connections <= own_connections
    })
}

/// A host no Agent daemon can control, as a host refuses a hello without its controller key.
fn uncontrollable(reason: &str) -> HostError {
    HostError::before(FORBIDDEN, reason)
}

fn uncontrollable_host_stopped() -> HostError {
    HostError::before(
        HOST_NOT_RUNNING,
        "stopped a Tickhost no Agent daemon could control; none runs now",
    )
}

fn other_build() -> HostError {
    HostError::before(INCOMPATIBLE_HOST, OTHER_TICKHOST_BUILD)
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
